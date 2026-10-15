//! Synchronous extension callbacks wait on a bounded host dialog broker. The
//! frontend continues polling and owns presentation; no dialog creates a message.
use agent_core::types::{timestamp, CancellationToken};
use serde::{de::Error, Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_PENDING: usize = 32;
const TEXT_LIMIT: usize = 1_048_576;
const POLL: Duration = Duration::from_millis(10);
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiDialog {
    Select {
        title: String,
        options: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<u64>,
    },
    Confirm {
        title: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<u64>,
    },
    Input {
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<u64>,
    },
    Editor {
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefill: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<u64>,
    },
}
impl UiDialog {
    pub fn timeout(&self) -> Option<u64> {
        match self {
            Self::Select { timeout, .. }
            | Self::Confirm { timeout, .. }
            | Self::Input { timeout, .. }
            | Self::Editor { timeout, .. } => *timeout,
        }
    }
    pub fn set_timeout(&mut self, value: Option<u64>) {
        match self {
            Self::Select { timeout, .. }
            | Self::Confirm { timeout, .. }
            | Self::Input { timeout, .. }
            | Self::Editor { timeout, .. } => *timeout = value,
        }
    }
    pub fn title(&self) -> &str {
        match self {
            Self::Select { title, .. }
            | Self::Confirm { title, .. }
            | Self::Input { title, .. }
            | Self::Editor { title, .. } => title,
        }
    }
    fn validate(&self) -> Result<(), String> {
        let mut total = self.title().len();
        let mut lengths = vec![total];
        match self {
            Self::Select { options, .. } => {
                if options.len() > 1024 {
                    return Err("UI select exceeds 1024 options".into());
                }
                lengths.extend(options.iter().map(String::len));
            }
            Self::Confirm { message, .. } => lengths.push(message.len()),
            Self::Input { placeholder, .. } => {
                lengths.extend(placeholder.as_ref().map(String::len))
            }
            Self::Editor { prefill, .. } => lengths.extend(prefill.as_ref().map(String::len)),
        }
        total = 0;
        for length in lengths {
            if length > TEXT_LIMIT {
                return Err("UI text exceeds 1MiB".into());
            }
            total = total.saturating_add(length);
        }
        if total > 8 * TEXT_LIMIT {
            return Err("UI request text exceeds 8MiB".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UiDialogRequest {
    pub id: String,
    #[serde(flatten)]
    pub dialog: UiDialog,
}
impl<'de> Deserialize<'de> for UiDialogRequest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let Value::Object(mut fields) = Value::deserialize(deserializer)? else {
            return Err(D::Error::custom("UI request must be an object"));
        };
        let id = fields
            .remove("id")
            .ok_or_else(|| D::Error::missing_field("id"))?;
        let id = serde_json::from_value::<String>(id).map_err(D::Error::custom)?;
        let dialog =
            serde_json::from_value::<UiDialog>(Value::Object(fields)).map_err(D::Error::custom)?;
        Ok(Self { id, dialog })
    }
}

/// The same response fields as the original RPC UI interface. A cancel must
/// contain true; false is not silently interpreted as a successful answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum UiReply {
    Value { value: String },
    Confirmed { confirmed: bool },
    Cancelled { cancelled: bool },
}
impl UiReply {
    pub fn cancelled() -> Self {
        Self::Cancelled { cancelled: true }
    }
    fn validate_for(&self, dialog: &UiDialog) -> Result<(), String> {
        match (self, dialog) {
            (Self::Cancelled { cancelled: true }, _) => Ok(()),
            (Self::Confirmed { .. }, UiDialog::Confirm { .. }) => Ok(()),
            (Self::Value { value }, UiDialog::Select { options, .. })
                if value.len() <= TEXT_LIMIT && options.contains(value) =>
            {
                Ok(())
            }
            (Self::Value { value }, UiDialog::Input { .. } | UiDialog::Editor { .. })
                if value.len() <= TEXT_LIMIT =>
            {
                Ok(())
            }
            _ => Err("UI response does not match the pending dialog".into()),
        }
    }
}

struct Pending {
    dialog: UiDialog,
    reply: mpsc::SyncSender<UiReply>,
    active: Option<Arc<AtomicBool>>,
}
#[derive(Default)]
struct State {
    enabled: bool,
    closed: bool,
    queued: VecDeque<UiDialogRequest>,
    pending: BTreeMap<String, Pending>,
}
#[derive(Clone, Default)]
pub struct UiBroker(Arc<Mutex<State>>);
impl std::fmt::Debug for UiBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        f.debug_struct("UiBroker")
            .field("enabled", &state.enabled)
            .field("closed", &state.closed)
            .field("pending", &state.pending.len())
            .finish()
    }
}
impl UiBroker {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(State {
            enabled: true,
            ..Default::default()
        })))
    }
    pub fn handle(&self) -> UiHandle {
        UiHandle {
            broker: self.clone(),
            active: None,
        }
    }
    pub fn is_enabled(&self) -> bool {
        let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.enabled && !state.closed
    }
    pub fn drain_requests(&self) -> Vec<UiDialogRequest> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .queued
            .drain(..)
            .collect()
    }
    pub fn pending_ids(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .keys()
            .cloned()
            .collect()
    }
    pub fn is_pending(&self, id: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .contains_key(id)
    }
    pub fn respond(&self, id: &str, reply: UiReply) -> Result<(), String> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let pending = state
            .pending
            .get(id)
            .ok_or("UI request is unknown or already closed")?;
        if pending
            .active
            .as_ref()
            .is_some_and(|active| !active.load(Ordering::Acquire))
        {
            state.pending.remove(id);
            state.queued.retain(|request| request.id != id);
            return Err("UI request belongs to a stale extension".into());
        }
        reply.validate_for(&pending.dialog)?;
        let pending = state.pending.remove(id).expect("validated pending request");
        state.queued.retain(|request| request.id != id);
        pending
            .reply
            .try_send(reply)
            .map_err(|_| "UI request has already closed".into())
    }
    /// Frontends call this before stopping/joining their workers. A late reply can
    /// never resolve another request, and no pending sender survives shutdown.
    pub fn close(&self) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        state.queued.clear();
        state.pending.clear();
    }
    fn remove(&self, id: &str) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.pending.remove(id);
        state.queued.retain(|request| request.id != id);
    }
}

#[derive(Clone, Debug, Default)]
pub struct UiHandle {
    broker: UiBroker,
    active: Option<Arc<AtomicBool>>,
}
impl PartialEq for UiHandle {
    fn eq(&self, other: &Self) -> bool {
        (Arc::ptr_eq(&self.broker.0, &other.broker.0)
            && match (&self.active, &other.active) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            })
            || (!self.is_enabled() && !other.is_enabled())
    }
}
impl UiHandle {
    pub fn is_enabled(&self) -> bool {
        self.broker.is_enabled()
            && self
                .active
                .as_ref()
                .is_none_or(|active| active.load(Ordering::Acquire))
    }
    pub(crate) fn scoped(&self, active: Arc<AtomicBool>) -> Self {
        Self {
            broker: self.broker.clone(),
            active: Some(active),
        }
    }
    pub fn request(&self, dialog: UiDialog, signal: &CancellationToken) -> Result<UiReply, String> {
        if self
            .active
            .as_ref()
            .is_some_and(|active| !active.load(Ordering::Acquire))
        {
            return Err("Extension UI context is stale".into());
        }
        if signal.is_cancelled() || !self.is_enabled() || dialog.timeout() == Some(0) {
            return Ok(UiReply::cancelled());
        }
        dialog.validate()?;
        let deadline = dialog
            .timeout()
            .map(|timeout| {
                Instant::now()
                    .checked_add(Duration::from_millis(timeout))
                    .ok_or("UI timeout exceeds clock range")
            })
            .transpose()?;
        let sequence = SEQUENCE
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| "UI request sequence exhausted")?;
        let id = format!("ui-{}-{}-{sequence}", timestamp(), std::process::id());
        let (sender, receiver) = mpsc::sync_channel(1);
        {
            let mut state = self.broker.0.lock().unwrap_or_else(|e| e.into_inner());
            if state.closed || !state.enabled || signal.is_cancelled() {
                return Ok(UiReply::cancelled());
            }
            if state.pending.len() >= MAX_PENDING {
                return Err("UI pending request limit is 32".into());
            }
            state.queued.push_back(UiDialogRequest {
                id: id.clone(),
                dialog: dialog.clone(),
            });
            state.pending.insert(
                id.clone(),
                Pending {
                    dialog,
                    reply: sender,
                    active: self.active.clone(),
                },
            );
        }
        struct PendingGuard {
            broker: UiBroker,
            id: String,
        }
        impl Drop for PendingGuard {
            fn drop(&mut self) {
                self.broker.remove(&self.id);
            }
        }
        let _pending = PendingGuard {
            broker: self.broker.clone(),
            id,
        };
        loop {
            if signal.is_cancelled()
                || self
                    .active
                    .as_ref()
                    .is_some_and(|active| !active.load(Ordering::Acquire))
                || deadline.is_some_and(|deadline| Instant::now() >= deadline)
            {
                return Ok(UiReply::cancelled());
            }
            let wait = deadline.map_or(POLL, |deadline| {
                POLL.min(deadline.saturating_duration_since(Instant::now()))
            });
            match receiver.recv_timeout(wait) {
                Ok(_) if signal.is_cancelled() => return Ok(UiReply::cancelled()),
                Ok(reply) => return Ok(reply),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(UiReply::cancelled()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }
    pub fn select(
        &self,
        title: &str,
        options: &[String],
        timeout: Option<u64>,
        signal: &CancellationToken,
    ) -> Result<Option<String>, String> {
        match self.request(
            UiDialog::Select {
                title: title.into(),
                options: options.to_vec(),
                timeout,
            },
            signal,
        )? {
            UiReply::Value { value } => Ok(Some(value)),
            _ => Ok(None),
        }
    }
    pub fn confirm(
        &self,
        title: &str,
        message: &str,
        timeout: Option<u64>,
        signal: &CancellationToken,
    ) -> Result<bool, String> {
        match self.request(
            UiDialog::Confirm {
                title: title.into(),
                message: message.into(),
                timeout,
            },
            signal,
        )? {
            UiReply::Confirmed { confirmed } => Ok(confirmed),
            _ => Ok(false),
        }
    }
    pub fn input(
        &self,
        title: &str,
        placeholder: Option<&str>,
        timeout: Option<u64>,
        signal: &CancellationToken,
    ) -> Result<Option<String>, String> {
        match self.request(
            UiDialog::Input {
                title: title.into(),
                placeholder: placeholder.map(str::to_owned),
                timeout,
            },
            signal,
        )? {
            UiReply::Value { value } => Ok(Some(value)),
            _ => Ok(None),
        }
    }
    pub fn editor(
        &self,
        title: &str,
        prefill: Option<&str>,
        timeout: Option<u64>,
        signal: &CancellationToken,
    ) -> Result<Option<String>, String> {
        match self.request(
            UiDialog::Editor {
                title: title.into(),
                prefill: prefill.map(str::to_owned),
                timeout,
            },
            signal,
        )? {
            UiReply::Value { value } => Ok(Some(value)),
            _ => Ok(None),
        }
    }
}
