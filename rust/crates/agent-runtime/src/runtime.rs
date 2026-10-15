//! Shared execution boundary. Clients own their UI and storage policy, not an agent loop.

use crate::automatic::{
    check_and_compact, estimate_context_tokens, estimate_context_tokens_for_model,
    recoverable_response, AutomationHandle, AutomationOptions, UpdatePlanTool,
};
use crate::builtins::{create_tools, tool_definitions};
use crate::extensions::{
    custom_message_content, extension_context, BeforeAgentStartEvent, ExtensionContext,
    ExtensionMode, LifecycleEvent, ModelIdentity, SharedRegistry,
};
use crate::provider::{ModelProvider, ProviderSettings};
use crate::retry::{has_partial_output, is_context_overflow, RetryingProvider};
use crate::session::Session;
use agent_core::agent_loop::{run_agent_loop, LoopConfig, LoopHooks, Provider};
use agent_core::tools::AgentTool;
use agent_core::types::{
    AgentEvent, AssistantMessage, CancellationToken, Content, Context, Message, StopReason,
    UserMessage,
};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}
impl QueueMode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "all" => Ok(Self::All),
            "one-at-a-time" => Ok(Self::OneAtATime),
            _ => Err("Queue mode must be all or one-at-a-time".into()),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::OneAtATime => "one-at-a-time",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct QueueItemId(pub u64);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueueLane {
    Immediate,
    Steering,
    FollowUp,
}
// Deliberately no Debug: messages and original attachments are private input.
#[derive(Clone, Serialize, Deserialize)]
pub struct QueueItem {
    pub id: QueueItemId,
    pub lane: QueueLane,
    pub message: Message,
}
#[derive(Clone)]
pub struct QueueSnapshot {
    pub workspace: Option<std::path::PathBuf>,
    pub items: Vec<QueueItem>,
    pub uncertain: Vec<QueueItem>,
    pub paused: bool,
    pub recovery_required: bool,
    pub halted: bool,
    pub revision: u64,
    /// Handoffs, never successful model answers or durable admission.
    pub dispatched: u64,
    pub error: Option<QueueError>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueError {
    MissingItem,
    DuplicateItem,
    Storage,
    CommitUncertain,
    InvalidStore,
    StoreBusy,
    StoreConflict,
    RecoveryRequired,
    IdExhausted,
}
impl std::fmt::Display for QueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::MissingItem => "Queue item is no longer pending",
            Self::DuplicateItem => "Queue item is already pending",
            Self::Storage => "Queue storage operation failed; dispatch halted",
            Self::CommitUncertain => {
                "Queue update may have committed; reload and review before retry"
            }
            Self::InvalidStore => "Queue journal is invalid or exceeds its limit",
            Self::StoreBusy => "Queue journal is open in another process",
            Self::StoreConflict => "Queue journal changed externally; reload required",
            Self::RecoveryRequired => "Recovered queue requires explicit review",
            Self::IdExhausted => "Queue identity limit reached",
        })
    }
}
impl std::error::Error for QueueError {}
#[derive(Clone)]
struct Pending {
    stored: crate::queue_store::StoredQueue,
    admitted_ids: Vec<QueueItemId>,
    accepted: BTreeMap<QueueItemId, (String, String)>,
    track_admissions: bool,
    recovery_required: bool,
    error: Option<QueueError>,
}
impl Default for Pending {
    fn default() -> Self {
        Self {
            stored: crate::queue_store::StoredQueue::empty(String::new()),
            admitted_ids: vec![],
            accepted: BTreeMap::new(),
            track_admissions: false,
            recovery_required: false,
            error: None,
        }
    }
}
#[derive(Default)]
struct QueueShared {
    state: Mutex<Pending>,
    // Serialize writers, never held by snapshots or UI receipt reads.
    operation: Mutex<Option<crate::queue_store::QueueStore>>,
    halted: std::sync::atomic::AtomicU64,
}
#[derive(Clone, Default)]
pub struct MessageQueues(Arc<QueueShared>);
impl MessageQueues {
    pub fn open_durable(directory: &Path, session_id: &str) -> Result<Self, QueueError> {
        let (store, stored) = crate::queue_store::QueueStore::open(directory, session_id)?;
        let recovery_required = !stored.items.is_empty() || !stored.inflight.is_empty();
        Ok(Self(Arc::new(QueueShared {
            state: Mutex::new(Pending {
                stored,
                recovery_required,
                ..Pending::default()
            }),
            operation: Mutex::new(Some(store)),
            halted: std::sync::atomic::AtomicU64::new(u64::from(recovery_required)),
        })))
    }
    /// Bind canonical execution context without silently migrating queued input.
    pub fn bind_workspace(&self, workspace: &Path) -> Result<(), QueueError> {
        {
            let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.stored.workspace.as_deref() == Some(workspace) {
                return Ok(());
            }
        }
        self.change(|state, _| {
            if state.stored.workspace.as_deref() != Some(workspace)
                && (!state.stored.items.is_empty() || !state.stored.inflight.is_empty())
            {
                return Err(QueueError::RecoveryRequired);
            }
            state.stored.workspace = Some(workspace.into());
            Ok(())
        })
    }
    fn change<T>(
        &self,
        operation: impl FnOnce(&mut Pending, bool) -> Result<T, QueueError>,
    ) -> Result<T, QueueError> {
        let store = self.0.operation.lock().unwrap_or_else(|e| e.into_inner());
        let mut candidate = self
            .0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(error) = candidate.error {
            return Err(error);
        }
        let before = candidate.stored.revision;
        let result = operation(&mut candidate, store.is_some())?;
        candidate.stored.revision = before.checked_add(1).ok_or(QueueError::IdExhausted)?;
        if let Some(store) = store.as_ref() {
            if let Err(error) = store.save(&candidate.stored, before) {
                self.halt_in_memory();
                let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
                state.error = Some(error);
                return Err(error);
            }
        }
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        // UI may drain receipts or change subscription while a write is in flight.
        candidate.admitted_ids = std::mem::take(&mut state.admitted_ids);
        candidate.track_admissions = state.track_admissions;
        *state = candidate;
        Ok(result)
    }
    fn is_halted(&self) -> bool {
        self.0.halted.load(std::sync::atomic::Ordering::Acquire) & 1 != 0
    }
    pub fn halt_in_memory(&self) {
        let _ = self.0.halted.fetch_update(
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
            |old| Some(old.wrapping_add(2) | 1),
        );
    }
    fn resume_epoch(&self, epoch: u64) {
        let _ = self.0.halted.compare_exchange(
            epoch,
            epoch.wrapping_add(2) & !1,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        );
    }
    pub fn storage_error(&self) -> Option<QueueError> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner()).error
    }
    pub fn try_enqueue(
        &self,
        message: Message,
        lane: QueueLane,
        raw_input: Option<Message>,
    ) -> Result<QueueItemId, QueueError> {
        self.change(|state, durable| {
            static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let id = QueueItemId(if durable {
                state.stored.next_id
            } else {
                NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            });
            state.stored.next_id = state.stored.next_id.max(id.0);
            state.stored.next_id = state
                .stored
                .next_id
                .checked_add(1)
                .ok_or(QueueError::IdExhausted)?;
            state
                .stored
                .items
                .push_back(QueueItem { id, lane, message });
            if let Some(raw) = raw_input {
                state.stored.raw.insert(id, raw);
            }
            Ok(id)
        })
    }
    /// Legacy memory clients use this convenience API. Durable clients use try_enqueue.
    pub fn enqueue(&self, message: Message, lane: QueueLane) -> QueueItemId {
        self.try_enqueue(message, lane, None)
            .unwrap_or(QueueItemId(0))
    }
    pub fn raw_input(&self, id: QueueItemId) -> Option<Message> {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stored
            .raw
            .get(&id)
            .cloned()
    }
    pub fn take_admitted_ids(&self) -> Vec<QueueItemId> {
        std::mem::take(
            &mut self
                .0
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .admitted_ids,
        )
    }
    pub fn set_track_admissions(&self, enabled: bool) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.track_admissions = enabled;
        if !enabled {
            state.admitted_ids.clear();
        }
    }
    fn record_admission(
        &self,
        id: QueueItemId,
        session: &mut Session,
        entry_id: &str,
    ) -> Result<(), String> {
        let guard = self.0.operation.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            let mut bytes = [0u8; 16];
            getrandom::fill(&mut bytes).map_err(|_| "Queue receipt creation failed")?;
            let receipt: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            let entry = session
                .entries
                .iter_mut()
                .find(|entry| entry.id == entry_id)
                .ok_or("Queue transcript entry missing")?;
            entry.fields.insert(
                "queueReceipt".into(),
                serde_json::Value::String(receipt.clone()),
            );
            state.accepted.insert(id, (entry_id.into(), receipt));
        }
        if state.track_admissions {
            state.admitted_ids.push(id);
        }
        Ok(())
    }
    /// Call only AFTER successfully saving this exact session. A nonce bound to
    /// the actual transcript entry prevents an older or replaced session from
    /// acknowledging another run's in-memory receipt.
    pub fn confirm_saved_session(&self, session: &Session) -> Result<(), QueueError> {
        self.change(|state, durable| {
            if durable && state.stored.session_id != session.id {
                return Err(QueueError::InvalidStore);
            }
            let confirmed: BTreeSet<_> = state
                .stored
                .inflight
                .iter()
                .filter_map(|item| {
                    let (entry_id, receipt) = state.accepted.get(&item.id)?;
                    session
                        .entries
                        .iter()
                        .any(|entry| {
                            entry.id == *entry_id
                                && entry.field("queueReceipt").as_str() == Some(receipt.as_str())
                                && serde_json::from_value::<Message>(entry.field("message").clone())
                                    .ok()
                                    .as_ref()
                                    == Some(&item.message)
                        })
                        .then_some(item.id)
                })
                .collect();
            state
                .stored
                .inflight
                .retain(|item| !confirmed.contains(&item.id));
            for id in confirmed {
                state.stored.raw.remove(&id);
                state.accepted.remove(&id);
            }
            Ok(())
        })
    }
    pub fn revision(&self) -> u64 {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stored
            .revision
            .saturating_add(self.0.halted.load(std::sync::atomic::Ordering::Acquire))
    }
    pub fn snapshot(&self) -> QueueSnapshot {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        let items = [
            QueueLane::Immediate,
            QueueLane::Steering,
            QueueLane::FollowUp,
        ]
        .into_iter()
        .flat_map(|lane| {
            state
                .stored
                .items
                .iter()
                .filter(move |item| item.lane == lane)
                .cloned()
        })
        .collect();
        QueueSnapshot {
            workspace: state.stored.workspace.clone(),
            items,
            uncertain: state.stored.inflight.clone(),
            paused: state.stored.paused,
            recovery_required: state.recovery_required,
            halted: self.is_halted(),
            revision: state
                .stored
                .revision
                .saturating_add(self.0.halted.load(std::sync::atomic::Ordering::Acquire)),
            dispatched: state.stored.dispatched,
            error: state.error,
        }
    }
    pub fn edit(&self, id: QueueItemId, message: Message) -> Result<(), QueueError> {
        self.change(|state, _| {
            let item = state
                .stored
                .items
                .iter_mut()
                .find(|item| item.id == id)
                .ok_or(QueueError::MissingItem)?;
            item.message = message;
            state.stored.raw.remove(&id);
            Ok(())
        })
    }
    pub fn remove(&self, id: QueueItemId) -> Result<Message, QueueError> {
        self.change(|state, _| {
            let index = state
                .stored
                .items
                .iter()
                .position(|item| item.id == id)
                .ok_or(QueueError::MissingItem)?;
            let item = state
                .stored
                .items
                .remove(index)
                .ok_or(QueueError::MissingItem)?;
            state.stored.raw.remove(&id);
            Ok(item.message)
        })
    }
    pub fn remove_uncertain(&self, id: QueueItemId) -> Result<Message, QueueError> {
        self.change(|state, _| {
            let index = state
                .stored
                .inflight
                .iter()
                .position(|item| item.id == id)
                .ok_or(QueueError::MissingItem)?;
            let item = state.stored.inflight.remove(index);
            state.stored.raw.remove(&id);
            state.accepted.remove(&id);
            Ok(item.message)
        })
    }
    pub fn retry_uncertain(&self, id: QueueItemId) -> Result<(), QueueError> {
        self.change(|state, _| {
            let index = state
                .stored
                .inflight
                .iter()
                .position(|item| item.id == id)
                .ok_or(QueueError::MissingItem)?;
            let item = state.stored.inflight.remove(index);
            state.accepted.remove(&id);
            state.stored.items.push_back(item);
            state.stored.paused = true;
            Ok(())
        })
    }
    pub fn reorder(&self, ids: &[QueueItemId]) -> Result<(), QueueError> {
        self.change(|state, _| {
            let mut ordered = VecDeque::new();
            for id in ids {
                if let Some(index) = state.stored.items.iter().position(|item| item.id == *id) {
                    if let Some(item) = state.stored.items.remove(index) {
                        ordered.push_back(item);
                    }
                }
            }
            ordered.append(&mut state.stored.items);
            state.stored.items = ordered;
            Ok(())
        })
    }
    pub fn promote(&self, id: QueueItemId) -> Result<(), QueueError> {
        self.change(|state, _| {
            let item = state
                .stored
                .items
                .iter_mut()
                .find(|item| item.id == id)
                .ok_or(QueueError::MissingItem)?;
            item.lane = QueueLane::Immediate;
            Ok(())
        })
    }
    /// Explicit send-now resumes only the promoted lane, without clearing pause.
    pub fn promote_for_dispatch(&self, id: QueueItemId) -> Result<(), QueueError> {
        let epoch = self.0.halted.load(std::sync::atomic::Ordering::Acquire);
        self.change(|state, _| {
            if state.recovery_required {
                return Err(QueueError::RecoveryRequired);
            }
            let item = state
                .stored
                .items
                .iter_mut()
                .find(|item| item.id == id)
                .ok_or(QueueError::MissingItem)?;
            item.lane = QueueLane::Immediate;
            Ok(())
        })?;
        self.resume_epoch(epoch);
        Ok(())
    }
    pub fn try_set_paused(&self, paused: bool) -> Result<(), QueueError> {
        let epoch = self.0.halted.load(std::sync::atomic::Ordering::Acquire);
        self.change(|state, _| {
            if !paused && state.recovery_required {
                return Err(QueueError::RecoveryRequired);
            }
            state.stored.paused = paused;
            Ok(())
        })?;
        if !paused {
            self.resume_epoch(epoch);
        }
        Ok(())
    }
    pub fn resume_recovered(&self) -> Result<(), QueueError> {
        let epoch = self.0.halted.load(std::sync::atomic::Ordering::Acquire);
        self.change(|state, _| {
            state.recovery_required = false;
            state.stored.paused = false;
            Ok(())
        })?;
        self.resume_epoch(epoch);
        Ok(())
    }
    pub fn set_paused(&self, paused: bool) {
        let _ = self.try_set_paused(paused);
    }
    pub fn steer(&self, message: Message) {
        self.enqueue(message, QueueLane::Steering);
    }
    pub fn follow_up(&self, message: Message) {
        self.enqueue(message, QueueLane::FollowUp);
    }
    pub fn len(&self) -> usize {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stored
            .items
            .len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn reserve(&self, lanes: &[QueueLane], single: bool) -> Result<Vec<QueueItem>, QueueError> {
        if self.is_halted() {
            return Ok(vec![]);
        }
        {
            let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.recovery_required
                || !state.stored.items.iter().any(|item| {
                    lanes.contains(&item.lane)
                        && (!state.stored.paused || item.lane == QueueLane::Immediate)
                })
            {
                return Ok(vec![]);
            }
        }
        let items = self.change(|state, durable| {
            if state.recovery_required || self.is_halted() {
                return Ok((vec![], durable));
            }
            let mut items = Vec::new();
            for lane in lanes {
                if state.stored.paused && *lane != QueueLane::Immediate {
                    continue;
                }
                let mode = if *lane == QueueLane::FollowUp {
                    state.stored.follow_up_mode
                } else {
                    state.stored.steering_mode
                };
                while let Some(index) = state
                    .stored
                    .items
                    .iter()
                    .position(|item| item.lane == *lane)
                {
                    if let Some(item) = state.stored.items.remove(index) {
                        items.push(item);
                    }
                    if single || mode == QueueMode::OneAtATime {
                        break;
                    }
                }
                if !items.is_empty() && (single || mode == QueueMode::OneAtATime) {
                    break;
                }
            }
            if durable {
                state.stored.inflight.extend(items.clone());
            }
            state.stored.dispatched = state.stored.dispatched.saturating_add(items.len() as u64);
            Ok((items, durable))
        })?;
        let (items, durable) = items;
        // A Stop arriving during flush must not be undone by an old dispatch.
        if self.is_halted() {
            if !durable {
                for item in items.into_iter().rev() {
                    self.restore(item)?;
                }
            }
            return Ok(vec![]);
        }
        Ok(items)
    }
    pub fn try_take_next(&self) -> Result<Option<QueueItem>, QueueError> {
        Ok(self
            .reserve(
                &[
                    QueueLane::Immediate,
                    QueueLane::Steering,
                    QueueLane::FollowUp,
                ],
                true,
            )?
            .into_iter()
            .next())
    }
    pub fn take_next(&self) -> Option<QueueItem> {
        self.try_take_next().ok().flatten()
    }
    pub fn restore(&self, item: QueueItem) -> Result<(), QueueError> {
        self.change(|state, _| {
            if state.stored.items.iter().any(|queued| queued.id == item.id) {
                return Err(QueueError::DuplicateItem);
            }
            if state.recovery_required || state.accepted.contains_key(&item.id) {
                return Err(QueueError::RecoveryRequired);
            }
            state.stored.inflight.retain(|old| old.id != item.id);
            state.stored.next_id = state
                .stored
                .next_id
                .max(item.id.0.checked_add(1).ok_or(QueueError::IdExhausted)?);
            state.stored.items.push_front(item);
            Ok(())
        })
    }
    pub fn pop_next(&self) -> Option<Message> {
        self.take_next().map(|item| item.message)
    }
    pub fn modes(&self) -> (QueueMode, QueueMode) {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.stored.steering_mode, state.stored.follow_up_mode)
    }
    pub fn set_steering_mode(&self, mode: QueueMode) {
        let _ = self.change(|state, _| {
            state.stored.steering_mode = mode;
            Ok(())
        });
    }
    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        let _ = self.change(|state, _| {
            state.stored.follow_up_mode = mode;
            Ok(())
        });
    }
    pub fn clear(&self) -> (Vec<Message>, Vec<Message>) {
        self.change(|state, _| {
            let mut steering = vec![];
            let mut follow = vec![];
            for item in state.stored.items.drain(..) {
                state.stored.raw.remove(&item.id);
                if item.lane == QueueLane::FollowUp {
                    follow.push(item.message);
                } else {
                    steering.push(item.message);
                }
            }
            Ok((steering, follow))
        })
        .unwrap_or_default()
    }
    fn take_steering(&self) -> Vec<QueueItem> {
        self.reserve(&[QueueLane::Immediate, QueueLane::Steering], false)
            .unwrap_or_default()
    }
    fn take_follow_up(&self) -> Vec<QueueItem> {
        self.reserve(&[QueueLane::FollowUp], false)
            .unwrap_or_default()
    }
}
impl LoopHooks for MessageQueues {
    fn has_pending_messages(&self) -> bool {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        !state.recovery_required
            && !self.is_halted()
            && state
                .stored
                .items
                .iter()
                .any(|item| !state.stored.paused || item.lane == QueueLane::Immediate)
    }
    fn get_steering_messages(&mut self) -> Vec<Message> {
        self.take_steering()
            .into_iter()
            .map(|item| item.message)
            .collect()
    }
    fn get_follow_up_messages(&mut self) -> Vec<Message> {
        self.take_follow_up()
            .into_iter()
            .map(|item| item.message)
            .collect()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RunOptions {
    #[serde(skip)]
    pub queued_prompt_id: Option<QueueItemId>,
    pub allow_changes: bool,
    pub allow_commands: bool,
    pub no_tools: bool,
    pub tools: Option<Vec<String>>,
    pub exclude_tools: Vec<String>,
    pub max_turns: usize,
    pub system_prompt_suffix: String,
    pub automation: AutomationOptions,
    #[serde(skip)]
    pub automation_state: AutomationHandle,
    #[serde(skip)]
    pub extensions: SharedRegistry,
    pub extension_mode: ExtensionMode,
    pub extension_model: Option<ModelIdentity>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            queued_prompt_id: None,
            allow_changes: false,
            allow_commands: false,
            no_tools: false,
            tools: None,
            exclude_tools: vec![],
            max_turns: 128,
            system_prompt_suffix: String::new(),
            automation: AutomationOptions::default(),
            automation_state: AutomationHandle::default(),
            extensions: SharedRegistry::default(),
            extension_mode: ExtensionMode::default(),
            extension_model: None,
        }
    }
}

pub fn run_prompt(
    session: &mut Session,
    prompt: Message,
    settings: &ProviderSettings,
    options: &RunOptions,
    queues: &MessageQueues,
    signal: &CancellationToken,
    emit: &mut dyn FnMut(AgentEvent),
) -> Result<(), String> {
    let tools = configured_tools(&session.workspace, options)?;
    let mut provider = ModelProvider {
        settings: settings.clone(),
        tool_definitions: tool_definitions(&tools),
    };
    run_with_provider(
        session,
        prompt,
        &mut provider,
        &tools,
        options,
        queues,
        signal,
        emit,
    )
}

pub fn configured_tools(
    workspace: &Path,
    options: &RunOptions,
) -> Result<Vec<Box<dyn AgentTool>>, String> {
    let mut tools = options.extensions.tools();
    tools.extend(create_tools(
        workspace,
        options.allow_changes,
        options.allow_commands,
    ));
    if options.automation.enabled {
        tools.push(Box::new(UpdatePlanTool {
            handle: options.automation_state.clone(),
        }));
    }
    let mut names = BTreeSet::new();
    tools.retain(|tool| {
        !options.no_tools
            && (options.allow_changes || !matches!(tool.name(), "write" | "edit"))
            && (options.allow_commands || tool.name() != "run_command")
            && names.insert(tool.name().to_owned())
            && options
                .tools
                .as_ref()
                .is_none_or(|names| names.iter().any(|name| name == tool.name()))
            && !options.exclude_tools.iter().any(|name| name == tool.name())
    });
    if let Some(requested) = &options.tools {
        for name in requested {
            if !options.exclude_tools.contains(name)
                && !options.no_tools
                && !tools.iter().any(|tool| tool.name() == name)
            {
                return Err(format!("Tool not enabled: {name}"));
            }
        }
    }
    Ok(tools)
}

#[allow(clippy::too_many_arguments)]
pub fn run_with_provider(
    session: &mut Session,
    prompt: Message,
    provider: &mut dyn Provider,
    tools: &[Box<dyn AgentTool>],
    options: &RunOptions,
    queues: &MessageQueues,
    signal: &CancellationToken,
    emit: &mut dyn FnMut(AgentEvent),
) -> Result<(), String> {
    if !session.workspace.is_dir() {
        return Err(format!(
            "Workspace is not a directory: {}",
            session.workspace.display()
        ));
    }
    {
        let state = queues.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.stored.session_id.is_empty()
            && (state.stored.session_id != session.id
                || state.stored.workspace.as_deref() != Some(session.workspace.as_path()))
        {
            return Err(
                "Queue session or workspace differs; review and reprepare queued input".into(),
            );
        }
    }
    options.automation.validate()?;
    options.automation_state.bind_session(session);
    let saved_state = session.runtime_state("automation")?;
    let persist_state =
        options.automation.enabled || options.automation.retry.enabled || saved_state.is_some();
    if let Some(state) = saved_state {
        options.automation_state.restore_state(
            serde_json::from_value(state)
                .map_err(|error| format!("Automatic runtime state: {error}"))?,
        )?;
        options.automation_state.bind_session(session);
    }
    if let Some(model) = &options.automation.model {
        options.automation_state.bind_model(model);
    }
    if session.context.messages.is_empty() && session.name == "新会话" {
        session.name = prompt.text().chars().take(24).collect();
    }
    let mut context = session.context.clone();
    if !options.system_prompt_suffix.is_empty() {
        context.system_prompt.push_str("\n\n");
        context
            .system_prompt
            .push_str(&options.system_prompt_suffix);
    }
    let mut extension_snapshot = extension_context(
        session,
        &options.extensions,
        options.extension_mode,
        options.extension_model.clone(),
    )?;
    extension_snapshot.context = context.clone();
    if let Some(id) = options.queued_prompt_id {
        let state = queues.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state
            .stored
            .inflight
            .iter()
            .any(|item| item.id == id && item.message == prompt)
        {
            return Err("Queued prompt reservation does not match".into());
        }
    }
    let mut prompts = vec![prompt.clone()];
    if !options.extensions.ids().is_empty() && !signal.is_cancelled() {
        let images = match &prompt {
            Message::User(user) => user
                .content
                .iter()
                .filter(|content| matches!(content, Content::Image { .. }))
                .cloned()
                .collect(),
            _ => vec![],
        };
        let before = options.extensions.before_agent_start(
            &BeforeAgentStartEvent {
                prompt: prompt.text(),
                images,
                system_prompt: context.system_prompt.clone(),
            },
            &extension_snapshot,
            signal,
        );
        let before = match before {
            Ok(before) => before,
            Err(error) => {
                let _ = options.extensions.emit_event(
                    &LifecycleEvent::AgentSettled,
                    &extension_snapshot,
                    signal,
                );
                return Err(error);
            }
        };
        prompts.extend(before.messages);
        if let Some(system_prompt) = before.system_prompt {
            context.system_prompt = system_prompt;
            extension_snapshot.context = context.clone();
        }
    }
    let tool_hooks = options.extensions.tool_hooks(extension_snapshot);
    let stored = Rc::new(RefCell::new(session.clone()));
    let failure = Rc::new(RefCell::new(None));
    let replacement = Rc::new(RefCell::new(None));
    let shared_provider = Rc::new(RefCell::new(provider));
    let reservations = Rc::new(RefCell::new(Vec::<QueueItem>::new()));
    let reservation_guard = ReservationGuard {
        queues: queues.clone(),
        items: reservations.clone(),
    };
    let mut hooks = RuntimeHooks {
        reservations: reservations.clone(),
        session: stored.clone(),
        provider: shared_provider.clone(),
        queues: queues.clone(),
        options: options.automation.clone(),
        handle: options.automation_state.clone(),
        failure: failure.clone(),
        replacement: replacement.clone(),
        extensions: options.extensions.clone(),
        extension_mode: options.extension_mode,
        extension_model: options.extension_model.clone(),
        system_prompt: context.system_prompt.clone(),
    };
    let mut provider = RuntimeProvider {
        session: stored.clone(),
        provider: shared_provider.clone(),
        options: options.automation.clone(),
        handle: options.automation_state.clone(),
        failure: failure.clone(),
        replacement,
        extensions: options.extensions.clone(),
        extension_mode: options.extension_mode,
        extension_model: options.extension_model.clone(),
    };
    let mut initial_admission = options.queued_prompt_id;
    let result = run_agent_loop(
        prompts,
        &context,
        LoopConfig {
            max_turns: options.max_turns,
            ..Default::default()
        },
        &mut provider,
        if options.no_tools { &[] } else { tools },
        &mut hooks,
        &tool_hooks,
        signal,
        &mut |event| {
            if let AgentEvent::MessageEnd { message } = &event {
                // A preparation failure is diagnostic, not a fabricated transcript entry.
                if failure.borrow().is_none() {
                    let result = append_queued_message(
                        &mut stored.borrow_mut(),
                        &mut reservations.borrow_mut(),
                        queues,
                        message,
                    );
                    let result = result.and_then(|entry_id| {
                        if let Some(id) = initial_admission.take() {
                            queues.record_admission(id, &mut stored.borrow_mut(), &entry_id)?;
                        }
                        Ok(())
                    });
                    if let Err(error) = result {
                        *failure.borrow_mut() = Some(error);
                        signal.cancel();
                    }
                }
            }
            // A failed transcript transaction is not an admission notification.
            if matches!(&event, AgentEvent::MessageEnd { .. }) && failure.borrow().is_some() {
                return;
            }
            let mut snapshot = lifecycle_context(&stored.borrow(), options);
            snapshot
                .context
                .system_prompt
                .clone_from(&context.system_prompt);
            let _ = options.extensions.emit_event(
                &LifecycleEvent::Agent {
                    event: event.clone(),
                },
                &snapshot,
                signal,
            );
            emit(event);
        },
    );
    drop(reservation_guard);
    *session = stored.borrow().clone();
    let mut outcome = (|| {
        if let Some(error) = failure.borrow().clone() {
            return Err(error);
        }
        result.map_err(|e| e.to_string())?;
        if let Some(message) =
            session
                .context
                .messages
                .iter()
                .rev()
                .find_map(|message| match message {
                    Message::Assistant(message) => Some(message),
                    _ => None,
                })
        {
            if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                return Err(message
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "Agent run failed".into()));
            }
        }
        // A completed answer may also report silent overflow. Preserve it; never rerun its tools.
        if options.automation.enabled && !signal.is_cancelled() {
            let mut request = session.context.clone();
            request.system_prompt.clone_from(&context.system_prompt);
            request.messages = llm_messages(&request.messages)?;
            let mut inner = shared_provider.borrow_mut();
            check_and_compact(
                session,
                &request,
                &mut **inner,
                &options.automation,
                &options.automation_state,
                signal,
                false,
            )?;
        }
        Ok(())
    })();
    if persist_state {
        let saved = serde_json::to_value(options.automation_state.state())
            .map_err(|error| error.to_string())
            .and_then(|state| session.record_runtime_state("automation", state));
        if let Err(error) = saved {
            outcome = Err(match outcome {
                Ok(()) => error,
                Err(previous) => format!("{previous}; runtime state save: {error}"),
            });
        }
    }
    options.automation_state.bind_session(session);
    let _ = options.extensions.emit_event(
        &LifecycleEvent::AgentSettled,
        &lifecycle_context(session, options),
        signal,
    );
    outcome
}

struct ReservationGuard {
    queues: MessageQueues,
    items: Rc<RefCell<Vec<QueueItem>>>,
}
impl Drop for ReservationGuard {
    fn drop(&mut self) {
        // References taken while appending have unwound before this guard drops.
        for item in self.items.borrow_mut().drain(..).rev() {
            let _ = self.queues.restore(item);
        }
    }
}

fn append_queued_message(
    session: &mut Session,
    reservations: &mut Vec<QueueItem>,
    queues: &MessageQueues,
    message: &Message,
) -> Result<String, String> {
    let entry_id = session.append_message(message.clone())?;
    if reservations
        .first()
        .is_some_and(|item| item.message == *message)
    {
        queues.record_admission(reservations.remove(0).id, session, &entry_id)?;
    }
    Ok(entry_id)
}

// Terminal cleanup must still observe a failed transcript/state transaction.
fn lifecycle_context(session: &Session, options: &RunOptions) -> ExtensionContext {
    extension_context(
        session,
        &options.extensions,
        options.extension_mode,
        options.extension_model.clone(),
    )
    .unwrap_or_else(|_| ExtensionContext {
        context: session.context.clone(),
        workspace: session.workspace.clone(),
        session_id: Some(session.id.clone()),
        mode: options.extension_mode,
        has_ui: matches!(
            options.extension_mode,
            ExtensionMode::Tui | ExtensionMode::Desktop
        ),
        model: options.extension_model.clone(),
        ..Default::default()
    })
}

fn llm_messages(messages: &[Message]) -> Result<Vec<Message>, String> {
    let mut converted = Vec::new();
    for message in messages {
        if let Message::Custom { timestamp, .. } = message {
            if let Some(content) = custom_message_content(message)? {
                converted.push(Message::User(UserMessage {
                    content,
                    timestamp: *timestamp,
                }));
            }
        } else {
            converted.push(message.clone());
        }
    }
    Ok(converted)
}

type SharedProvider<'a> = Rc<RefCell<&'a mut dyn Provider>>;
type SharedSession = Rc<RefCell<Session>>;
type Failure = Rc<RefCell<Option<String>>>;
type Replacement = Rc<RefCell<Option<Context>>>;

struct RuntimeHooks<'a> {
    reservations: Rc<RefCell<Vec<QueueItem>>>,
    session: SharedSession,
    provider: SharedProvider<'a>,
    queues: MessageQueues,
    options: AutomationOptions,
    handle: AutomationHandle,
    failure: Failure,
    replacement: Replacement,
    extensions: SharedRegistry,
    extension_mode: ExtensionMode,
    extension_model: Option<ModelIdentity>,
    system_prompt: String,
}

impl LoopHooks for RuntimeHooks<'_> {
    fn has_pending_messages(&self) -> bool {
        self.queues.has_pending_messages()
    }
    fn transform_context(
        &mut self,
        messages: &[Message],
        signal: &CancellationToken,
    ) -> Vec<Message> {
        let result = extension_context(
            &self.session.borrow(),
            &self.extensions,
            self.extension_mode,
            self.extension_model.clone(),
        )
        .and_then(|mut snapshot| {
            snapshot.context.messages = messages.to_vec();
            snapshot
                .context
                .system_prompt
                .clone_from(&self.system_prompt);
            self.extensions
                .transform_context(messages, &snapshot, signal)
        });
        match result {
            Ok(messages) => messages,
            Err(error) => {
                if !signal.is_cancelled() {
                    *self.failure.borrow_mut() = Some(error);
                    signal.cancel();
                }
                messages.to_vec()
            }
        }
    }
    fn convert_to_llm(&mut self, messages: &[Message]) -> Vec<Message> {
        match llm_messages(messages) {
            Ok(messages) => messages,
            Err(error) => {
                *self.failure.borrow_mut() = Some(error);
                vec![]
            }
        }
    }
    fn prepare_request(
        &mut self,
        context: &Context,
        signal: &CancellationToken,
    ) -> Result<Option<Context>, String> {
        if let Some(error) = self.failure.borrow().clone() {
            return Err(error);
        }
        if let Some(error) = self.queues.storage_error() {
            signal.cancel();
            return Err(error.to_string());
        }
        if !self.options.enabled || signal.is_cancelled() {
            return Ok(None);
        }
        let mut candidate = self.session.borrow().clone();
        let mut request = candidate.context.clone();
        request.system_prompt = context.system_prompt.clone();
        let mut budget_context = request.clone();
        budget_context.messages = llm_messages(&budget_context.messages)?;
        let mut inner = self.provider.borrow_mut();
        match check_and_compact(
            &mut candidate,
            &budget_context,
            &mut **inner,
            &self.options,
            &self.handle,
            signal,
            false,
        ) {
            Ok(changed) => {
                if changed {
                    *self.session.borrow_mut() = candidate.clone();
                }
                request.messages = candidate.context.messages;
                Ok((changed || request.messages != context.messages).then_some(request))
            }
            Err(error) => {
                *self.failure.borrow_mut() = Some(error.clone());
                Err(error)
            }
        }
    }
    fn get_steering_messages(&mut self) -> Vec<Message> {
        let items = self.queues.take_steering();
        let messages = items.iter().map(|item| item.message.clone()).collect();
        self.reservations.borrow_mut().extend(items);
        messages
    }
    fn get_follow_up_messages(&mut self) -> Vec<Message> {
        let items = self.queues.take_follow_up();
        let messages = items.iter().map(|item| item.message.clone()).collect();
        self.reservations.borrow_mut().extend(items);
        messages
    }
    fn take_request_context(&mut self) -> Option<Context> {
        self.replacement.borrow_mut().take()
    }
}

struct RuntimeProvider<'a> {
    session: SharedSession,
    provider: SharedProvider<'a>,
    options: AutomationOptions,
    handle: AutomationHandle,
    failure: Failure,
    replacement: Replacement,
    extensions: SharedRegistry,
    extension_mode: ExtensionMode,
    extension_model: Option<ModelIdentity>,
}

impl Provider for RuntimeProvider<'_> {
    fn complete(
        &mut self,
        context: &Context,
        signal: &CancellationToken,
        update: &mut dyn FnMut(AssistantMessage),
    ) -> Result<AssistantMessage, String> {
        if let Some(error) = self.failure.borrow().clone() {
            return Err(error);
        }
        let tokens = self.options.model.as_ref().map_or_else(
            || estimate_context_tokens(&self.session.borrow(), context),
            |model| estimate_context_tokens_for_model(&self.session.borrow(), context, model),
        );
        let mut inner = self.provider.borrow_mut();
        let mut retry = RetryingProvider::new(
            &mut **inner,
            self.options.retry.clone(),
            self.handle.clone(),
            "agent",
            Some(tokens),
        );
        let mut produced_output = false;
        let result = retry.complete(context, signal, &mut |partial| {
            produced_output |= has_partial_output(&partial);
            update(partial);
        });
        if produced_output || !self.options.enabled || signal.is_cancelled() {
            return result;
        }
        let recover = match &result {
            Err(error) => is_context_overflow(error),
            Ok(message) => {
                (message.stop_reason == StopReason::Error
                    && message
                        .error_message
                        .as_ref()
                        .is_some_and(|error| is_context_overflow(error)))
                    || self
                        .options
                        .model
                        .as_ref()
                        .is_some_and(|model| recoverable_response(message, model))
            }
        };
        if !recover {
            return result;
        }
        let mut candidate = self.session.borrow().clone();
        match check_and_compact(
            &mut candidate,
            context,
            &mut **inner,
            &self.options,
            &self.handle,
            signal,
            true,
        ) {
            Ok(true) => {
                let mut request = candidate.context.clone();
                request.system_prompt = context.system_prompt.clone();
                let mut snapshot = extension_context(
                    &candidate,
                    &self.extensions,
                    self.extension_mode,
                    self.extension_model.clone(),
                )?;
                snapshot.context = request.clone();
                *self.session.borrow_mut() = candidate;
                *self.replacement.borrow_mut() = Some(request.clone());
                request.messages = llm_messages(&self.extensions.transform_context(
                    &request.messages,
                    &snapshot,
                    signal,
                )?)?;
                update(AssistantMessage::text(""));
                let tokens = self.options.model.as_ref().map_or_else(
                    || estimate_context_tokens(&self.session.borrow(), &request),
                    |model| {
                        estimate_context_tokens_for_model(&self.session.borrow(), &request, model)
                    },
                );
                let mut retry = RetryingProvider::new(
                    &mut **inner,
                    self.options.retry.clone(),
                    self.handle.clone(),
                    "overflow_recovery",
                    Some(tokens),
                );
                // One compact-and-retry attempt. A second overflow is returned normally.
                retry.complete(&request, signal, update)
            }
            Ok(false) => result,
            Err(error) => {
                *self.failure.borrow_mut() = Some(error.clone());
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod managed_queue_admission_tests {
    use super::*;
    #[test]
    fn newer_stop_wins_over_old_resume_epoch() {
        let queues = MessageQueues::default();
        queues.halt_in_memory();
        let epoch = queues.0.halted.load(std::sync::atomic::Ordering::Acquire);
        queues.halt_in_memory();
        queues.resume_epoch(epoch);
        assert!(queues.is_halted());
        let current = queues.0.halted.load(std::sync::atomic::Ordering::Acquire);
        queues.resume_epoch(current);
        assert!(!queues.is_halted());
    }
    #[test]
    fn failed_transcript_append_keeps_reservation_for_same_id_restore() {
        let queues = MessageQueues::default();
        queues.set_track_admissions(true);
        let message = Message::user("retained");
        let id = queues.enqueue(message.clone(), QueueLane::Steering);
        let mut reservations = queues.take_steering();
        let mut session = Session::default();
        session.append_message(Message::user("existing")).unwrap();
        session.leaf_id = Some("missing-entry".into());
        assert!(append_queued_message(&mut session, &mut reservations, &queues, &message).is_err());
        assert!(queues.take_admitted_ids().is_empty());
        for item in reservations.drain(..).rev() {
            queues.restore(item).unwrap();
        }
        assert_eq!(queues.snapshot().items[0].id, id);
        assert_eq!(queues.snapshot().items[0].message, message);
    }
}
