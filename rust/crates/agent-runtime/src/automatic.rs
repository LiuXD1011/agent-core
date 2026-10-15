// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: MIT

//! Opt-in request-boundary compaction. Horizons are measured from completed plan steps.

use crate::catalog;
use crate::compaction::{active_checkpoint, compact_session, prepare_compaction, CompactionResult};
use crate::retry::{RetryOptions, RetryingProvider};
use crate::session::Session;
use agent_core::agent_loop::Provider;
use agent_core::context::{
    decide_compaction, CompactionEconomics, CompactionInput, CompactionReason,
};
use agent_core::tools::AgentTool;
use agent_core::types::{
    AssistantMessage, CancellationToken, Content, Context, Message, StopReason, ToolResult, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelBudget {
    pub provider: String,
    pub id: String,
    pub context_window: u64,
    pub max_output_tokens: u64,
    /// The observed catalog cost object, including input-token tiers. Null means unknown pricing.
    pub cost: Value,
}

impl ModelBudget {
    pub fn from_catalog(provider: &str, id: &str) -> Option<Self> {
        catalog::find(provider, id).map(|model| Self {
            provider: model.provider.clone(),
            id: model.id.clone(),
            context_window: model.context_window,
            max_output_tokens: model.max_tokens,
            cost: model.cost.clone(),
        })
    }
    fn cache_ratio(&self, tokens: f64) -> Option<f64> {
        let mut rates = &self.cost;
        let mut threshold = 0.0;
        if let Some(tiers) = self.cost["tiers"].as_array() {
            for tier in tiers {
                if let Some(above) = tier["inputTokensAbove"].as_f64() {
                    if tokens > above && above >= threshold {
                        rates = tier;
                        threshold = above;
                    }
                }
            }
        }
        let cache_write = rates["cacheWrite"].as_f64()?;
        if !cache_write.is_finite() || cache_write < 0.0 {
            return None;
        }
        // APIs without separately billed writes charge cache misses as ordinary input.
        let write = if cache_write > 0.0 {
            cache_write
        } else {
            rates["input"].as_f64()?
        };
        let read = rates["cacheRead"].as_f64()?;
        (write.is_finite() && write >= 0.0 && read.is_finite() && read > 0.0)
            .then_some(write / read)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AutomationOptions {
    pub enabled: bool,
    pub model: Option<ModelBudget>,
    pub keep_recent_user_turns: usize,
    pub reserve_tokens: u64,
    pub summary_token_estimate: u64,
    pub retry: RetryOptions,
}

impl Default for AutomationOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            model: None,
            keep_recent_user_turns: 1,
            reserve_tokens: 16_384,
            summary_token_estimate: 1_000,
            retry: RetryOptions::default(),
        }
    }
}

impl AutomationOptions {
    pub fn validate(&self) -> Result<(), String> {
        self.retry.validate()?;
        if !self.enabled {
            return Ok(());
        }
        let model = self.model.as_ref().ok_or(
            "Automatic compaction requires catalog model metadata or an explicit ModelBudget",
        )?;
        if model.id.is_empty() || model.provider.is_empty() || model.context_window == 0 {
            return Err(
                "Automatic compaction requires exact provider/model and positive context window"
                    .into(),
            );
        }
        if self.reserve_tokens >= model.context_window || self.summary_token_estimate == 0 {
            return Err("Compaction reserve must be smaller than the context window and summary estimate must be positive".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    pub id: String,
    pub goal: String,
    pub status: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct AutomationState {
    pub model_identity: Option<String>,
    pub plan: Vec<PlanStep>,
    pub request_count: u64,
    pub last_boundary_request_count: u64,
    pub completed_boundary_request_counts: Vec<u64>,
    pub last_context_tokens: Option<f64>,
    pub positive_context_delta_total: f64,
    pub positive_context_delta_count: u64,
    pub native_compaction_count: usize,
    pub cache_debt_tokens: f64,
    pub cache_debt_repayment_tokens: f64,
    pub cache_debt_unpriced: bool,
    pub last_summary_usage: Option<Usage>,
    pub last_checkpoint_context_tokens: Option<f64>,
    pub pending_boundary: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetReport {
    pub reason: String,
    pub compact: bool,
    pub write_tokens: f64,
    pub archive_tokens: f64,
    pub memo_tokens: f64,
    pub expected_remaining_requests: Option<f64>,
    pub breakeven_requests: Option<f64>,
    pub carried_debt_tokens: f64,
    pub cache_write_read_ratio: Option<f64>,
    pub cache_debt_unpriced: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum RuntimeEvent {
    BudgetDecision {
        decision: BudgetReport,
    },
    CompactionStart {
        reason: String,
    },
    CompactionEnd {
        reason: String,
        result: Option<CompactionResult>,
        error: Option<String>,
        aborted: bool,
    },
    RetryScheduled {
        source: String,
        attempt: usize,
        max_attempts: usize,
        delay_ms: u64,
        error: String,
    },
    RetryAttemptStart {
        source: String,
        attempt: usize,
    },
    RetryFinished {
        source: String,
        success: bool,
        attempt: usize,
        error: Option<String>,
    },
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationReport {
    pub session_id: Option<String>,
    pub active_leaf_id: Option<String>,
    pub state: AutomationState,
    pub decisions: Vec<BudgetReport>,
    pub compactions: usize,
    pub retries: usize,
    pub events: Vec<RuntimeEvent>,
}

#[derive(Clone, Debug, Default)]
pub struct AutomationHandle(Arc<Mutex<AutomationReport>>);

impl AutomationHandle {
    pub fn snapshot(&self) -> AutomationReport {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn state(&self) -> AutomationState {
        self.snapshot().state
    }
    pub fn drain_events(&self) -> Vec<RuntimeEvent> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .events
            .drain(..)
            .collect()
    }
    pub fn restore_state(&self, state: AutomationState) -> Result<(), String> {
        validate_plan(&state.plan)?;
        const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
        if state.last_boundary_request_count > state.request_count
            || state.request_count > MAX_SAFE_INTEGER
            || state
                .completed_boundary_request_counts
                .iter()
                .any(|count| *count > MAX_SAFE_INTEGER)
            || [
                state.positive_context_delta_total,
                state.cache_debt_tokens,
                state.cache_debt_repayment_tokens,
            ]
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0)
            || state
                .last_context_tokens
                .is_some_and(|value| !value.is_finite() || value < 0.0)
            || state
                .last_checkpoint_context_tokens
                .is_some_and(|value| !value.is_finite() || value < 0.0)
        {
            return Err("Invalid automatic context state".into());
        }
        self.0.lock().unwrap_or_else(|e| e.into_inner()).state = state;
        Ok(())
    }
    pub fn bind_session(&self, session: &Session) {
        let branch = session.get_branch(None).unwrap_or_default();
        let mut report = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if report
            .session_id
            .as_ref()
            .is_some_and(|id| id != &session.id)
            || report
                .active_leaf_id
                .as_ref()
                .is_some_and(|leaf| !branch.iter().any(|entry| &entry.id == leaf))
        {
            *report = AutomationReport::default();
        }
        report.session_id = Some(session.id.clone());
        report.active_leaf_id = branch.last().map(|entry| entry.id.clone());
        let branch_compactions = branch
            .iter()
            .filter(|entry| {
                entry.entry_type == "compaction"
                    && entry
                        .field("archiveId")
                        .as_str()
                        .is_some_and(|id| session.compactions.iter().any(|record| record.id == id))
            })
            .count();
        if report.state.model_identity.is_some()
            && report.state.native_compaction_count < branch_compactions
        {
            // A checkpoint not recorded by this ledger has unknown incremental cache cost.
            report.state.cache_debt_unpriced = true;
        }
        report.state.native_compaction_count = branch_compactions;
    }
    pub fn bind_model(&self, model: &ModelBudget) {
        let identity = format!("{}/{}", model.provider, model.id);
        let mut report = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let state = &mut report.state;
        if state
            .model_identity
            .as_ref()
            .is_some_and(|old| old != &identity)
        {
            // Cache debt and request-size deltas from another route cannot price this model.
            state.plan.clear();
            state.completed_boundary_request_counts.clear();
            state.last_boundary_request_count = state.request_count;
            state.last_context_tokens = None;
            state.positive_context_delta_total = 0.0;
            state.positive_context_delta_count = 0;
            state.cache_debt_tokens = 0.0;
            state.cache_debt_repayment_tokens = 0.0;
            state.cache_debt_unpriced = false;
            state.pending_boundary = false;
        }
        state.model_identity = Some(identity);
    }
    pub(crate) fn emit(&self, event: RuntimeEvent) {
        let mut report = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(event, RuntimeEvent::RetryScheduled { .. }) {
            report.retries += 1;
        }
        report.events.push(event);
    }
    pub(crate) fn record_request(&self, tokens: f64) {
        let mut report = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let state = &mut report.state;
        let delta = state
            .last_context_tokens
            .map_or(0.0, |last| (tokens - last).max(0.0));
        state.request_count += 1;
        state.last_context_tokens = Some(tokens);
        state.positive_context_delta_total += delta;
        state.positive_context_delta_count += u64::from(delta > 0.0);
        state.cache_debt_tokens =
            (state.cache_debt_tokens - state.cache_debt_repayment_tokens).max(0.0);
        if state.cache_debt_tokens == 0.0 {
            state.cache_debt_repayment_tokens = 0.0;
        }
    }
    pub fn update_plan(&self, steps: Vec<PlanStep>) -> Result<Vec<String>, String> {
        validate_plan(&steps)?;
        if steps.is_empty() {
            return Err("Plan must contain at least one valid step".into());
        }
        let mut report = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let state = &mut report.state;
        let completed: Vec<String> = steps
            .iter()
            .filter(|step| {
                step.status == "completed"
                    && !state
                        .plan
                        .iter()
                        .any(|old| old.id == step.id && old.status == "completed")
            })
            .map(|step| step.id.clone())
            .collect();
        if !completed.is_empty() {
            let interval = state
                .request_count
                .saturating_sub(state.last_boundary_request_count);
            if interval > 0 {
                state.completed_boundary_request_counts.push(interval);
                state.last_boundary_request_count = state.request_count;
                state.pending_boundary = true;
            }
        }
        state.plan = steps;
        Ok(completed)
    }
    fn committed_compaction(
        &self,
        debt: f64,
        repayment: f64,
        unpriced: bool,
        usage: Option<Usage>,
        context_tokens: f64,
    ) {
        let mut report = self.0.lock().unwrap_or_else(|e| e.into_inner());
        report.compactions += 1;
        let state = &mut report.state;
        state.native_compaction_count += 1;
        state.plan.clear();
        state.pending_boundary = false;
        state.last_context_tokens = None;
        state.positive_context_delta_total = 0.0;
        state.positive_context_delta_count = 0;
        state.cache_debt_tokens = debt.max(0.0);
        state.cache_debt_repayment_tokens = repayment.max(0.0);
        state.cache_debt_unpriced = unpriced;
        state.last_summary_usage = usage;
        state.last_checkpoint_context_tokens = Some(context_tokens);
    }
}

fn validate_plan(steps: &[PlanStep]) -> Result<(), String> {
    let mut ids = BTreeSet::new();
    if steps.len() > 128
        || steps.iter().any(|step| {
            step.id.is_empty()
                || step.goal.is_empty()
                || step.id.len() > 16_384
                || step.goal.len() > 16_384
                || !matches!(
                    step.status.as_str(),
                    "pending" | "in_progress" | "completed"
                )
                || !ids.insert(&step.id)
        })
    {
        return Err("Plan requires unique IDs and bounded id/goal with pending, in_progress, or completed status".into());
    }
    Ok(())
}

pub struct UpdatePlanTool {
    pub handle: AutomationHandle,
}
impl AgentTool for UpdatePlanTool {
    fn name(&self) -> &str {
        "update_plan"
    }
    fn description(&self) -> &str {
        "Replace the complete working plan. A newly completed step becomes a safe point where the agent may compact context if doing so is economical."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
        "steps":{"type":"array","minItems":1,"maxItems":128,"items":{"type":"object","properties":{
            "id":{"type":"string","minLength":1,"maxLength":16384},"goal":{"type":"string","minLength":1,"maxLength":16384},
            "status":{"type":"string","enum":["pending","in_progress","completed"]}},"required":["id","goal","status"],"additionalProperties":false}},
        "progress":{"type":"object","properties":{"files_changed":{"type":"array","items":{"type":"string","maxLength":1000},"maxItems":128},
            "verification":{"type":"array","items":{"type":"string","maxLength":1000},"maxItems":64},"decisions":{"type":"array","items":{"type":"string","maxLength":1000},"maxItems":64}},
            "required":["files_changed","verification","decisions"],"additionalProperties":false}},"required":["steps"],"additionalProperties":false})
    }
    fn execution_mode(&self) -> Option<agent_core::types::ExecutionMode> {
        Some(agent_core::types::ExecutionMode::Sequential)
    }
    fn execute(
        &self,
        _id: &str,
        args: &Value,
        signal: &CancellationToken,
        _update: &mut dyn FnMut(ToolResult),
    ) -> Result<ToolResult, String> {
        if signal.is_cancelled() {
            return Err("Plan update was aborted".into());
        }
        let steps: Vec<PlanStep> =
            serde_json::from_value(args["steps"].clone()).map_err(|error| error.to_string())?;
        let completed = self.handle.update_plan(steps.clone())?;
        Ok(ToolResult {
            content: vec![Content::text(format!(
                "<context-plan task_status=\"active\">{}</context-plan>",
                json!({"steps":steps})
            ))],
            details: json!({"boundary":!completed.is_empty(),"completed_step_ids":completed,"progress_recorded":args.get("progress").is_some(),"task_status":"active","plan":steps}),
            ..Default::default()
        })
    }
}

pub fn estimate_message_tokens(message: &Message) -> f64 {
    let content = match message {
        Message::User(message) => &message.content,
        Message::Assistant(message) => &message.content,
        Message::ToolResult(message) => &message.content,
        Message::Custom { .. } => return 0.0,
    };
    let chars: usize = content
        .iter()
        .map(|block| match block {
            Content::Text { text } => text.encode_utf16().count(),
            Content::Thinking { thinking } if matches!(message, Message::Assistant(_)) => {
                thinking.encode_utf16().count()
            }
            Content::ToolCall(call) if matches!(message, Message::Assistant(_)) => {
                call.name.encode_utf16().count() + call.arguments.to_string().encode_utf16().count()
            }
            Content::Image { .. } => 4_800,
            _ => 0,
        })
        .sum();
    chars.div_ceil(4) as f64
}

pub fn estimate_context_tokens(session: &Session, request: &Context) -> f64 {
    sized_context_tokens(session, request, None)
}

pub fn estimate_context_tokens_for_model(
    session: &Session,
    request: &Context,
    model: &ModelBudget,
) -> f64 {
    sized_context_tokens(session, request, Some(model))
}

fn sized_context_tokens(session: &Session, request: &Context, model: Option<&ModelBudget>) -> f64 {
    let visible = request
        .messages
        .iter()
        .map(estimate_message_tokens)
        .sum::<f64>();
    let fixed = request.system_prompt.len().div_ceil(4) as f64;
    let compacted_at = active_checkpoint(session)
        .ok()
        .flatten()
        .map_or(0, |(_, time)| time);
    let usage = request
        .messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| {
            let Message::Assistant(assistant) = message else {
                return None;
            };
            if model.is_some_and(|model| {
                assistant.protocol_metadata.as_ref().is_none_or(|metadata| {
                    metadata["model_identity_verified"].as_bool() == Some(false)
                        || metadata["model"] != model.id
                        || metadata["provider"] != model.provider
                })
            }) {
                return None;
            }
            if matches!(
                assistant.stop_reason,
                StopReason::Error | StopReason::Aborted
            ) || assistant.timestamp <= compacted_at
            {
                return None;
            }
            let usage = assistant.usage.as_ref()?;
            let tokens = if usage.total_tokens > 0 {
                usage.total_tokens
            } else {
                usage.input + usage.output + usage.cache_read + usage.cache_write
            };
            (tokens > 0).then(|| {
                tokens as f64
                    + request.messages[index + 1..]
                        .iter()
                        .map(estimate_message_tokens)
                        .sum::<f64>()
            })
        });
    (visible + fixed).max(usage.unwrap_or(0.0))
}

pub fn validate_request_boundary(messages: &[Message]) -> Result<(), String> {
    let mut pending = BTreeMap::new();
    for message in messages {
        match message {
            Message::Assistant(assistant) => {
                for call in assistant.tool_calls() {
                    if call.id.is_empty() || pending.insert(call.id, ()).is_some() {
                        return Err("Automatic context boundary contains an empty or duplicate pending tool call ID".into());
                    }
                }
            }
            Message::ToolResult(result) => {
                if pending.remove(&result.tool_call_id).is_none() {
                    return Err(
                        "Automatic context boundary contains an unpaired tool result".into(),
                    );
                }
            }
            _ => {}
        }
    }
    if pending.is_empty() {
        Ok(())
    } else {
        Err("Automatic context boundary contains unfinished tool calls".into())
    }
}

/// Runs only at a complete request boundary; no completed plan data means no economic horizon.
pub fn check_and_compact(
    session: &mut Session,
    request: &Context,
    provider: &mut dyn Provider,
    options: &AutomationOptions,
    handle: &AutomationHandle,
    signal: &CancellationToken,
    force_overflow: bool,
) -> Result<bool, String> {
    if !options.enabled || signal.is_cancelled() {
        return Ok(false);
    }
    if let Err(error) = validate_request_boundary(&request.messages) {
        handle.emit(RuntimeEvent::CompactionEnd {
            reason: "invalid_boundary".into(),
            result: None,
            error: Some(error.clone()),
            aborted: false,
        });
        return Err(error);
    }
    let model = options
        .model
        .as_ref()
        .ok_or("Automatic model budget missing")?;
    let preparation = prepare_compaction(session, options.keep_recent_user_turns);
    let archive_tokens = preparation.as_ref().map_or(0.0, |prepared| {
        prepared
            .archived_messages
            .iter()
            .map(estimate_message_tokens)
            .sum::<f64>()
    });
    let state = handle.state();
    let counts: Vec<f64> = state
        .completed_boundary_request_counts
        .iter()
        .map(|count| *count as f64)
        .collect();
    let real_horizon = state.pending_boundary && !counts.is_empty() && !state.plan.is_empty();
    let tokens = estimate_context_tokens_for_model(session, request, model);
    let mut decision = decide_compaction(&CompactionInput {
        write_tokens: tokens,
        archive_tokens,
        memo_tokens: options.summary_token_estimate as f64,
        context_tokens: tokens,
        completed_boundary_request_counts: real_horizon.then_some(counts.as_slice()),
        remaining_boundaries: state
            .plan
            .iter()
            .filter(|step| step.status != "completed")
            .count() as f64,
        average_context_token_increment: (state.positive_context_delta_count > 0).then(|| {
            state.positive_context_delta_total / state.positive_context_delta_count as f64
        }),
        context_window_tokens: Some(model.context_window as f64),
        prior_compaction_count: state.native_compaction_count,
        carried_debt_tokens: state.cache_debt_tokens,
        cache_debt_repayment_tokens: state.cache_debt_repayment_tokens,
        cache_write_read_ratio: (!state.cache_debt_unpriced)
            .then(|| model.cache_ratio(tokens))
            .flatten(),
        economics: CompactionEconomics {
            window_reserve_tokens: options.reserve_tokens as f64,
            ..Default::default()
        },
    });
    // A structured overflow code authorizes recovery, not an invented token count.
    if force_overflow
        && preparation.is_ok()
        && archive_tokens > options.summary_token_estimate as f64
    {
        decision.compact = true;
        decision.reason = CompactionReason::WindowProtection;
    }
    if decision.compact && preparation.is_err() {
        decision.compact = false;
        decision.reason = CompactionReason::NativeNotCompactable;
    }
    let report = BudgetReport {
        reason: decision.reason.as_str().into(),
        compact: decision.compact,
        write_tokens: decision.write_tokens,
        archive_tokens: decision.archive_tokens,
        memo_tokens: decision.memo_tokens,
        expected_remaining_requests: decision.expected_remaining_requests,
        breakeven_requests: decision.breakeven_requests,
        carried_debt_tokens: state.cache_debt_tokens,
        cache_write_read_ratio: decision.cache_write_read_ratio,
        cache_debt_unpriced: state.cache_debt_unpriced,
    };
    {
        let mut state = handle.0.lock().unwrap_or_else(|error| error.into_inner());
        state.decisions.push(report.clone());
        state.state.pending_boundary = false;
    }
    handle.emit(RuntimeEvent::BudgetDecision { decision: report });
    if !decision.compact {
        if force_overflow {
            return Err(
                "Context overflow has no complete history eligible for a compact-and-retry attempt"
                    .into(),
            );
        }
        return Ok(false);
    }
    let reason = if force_overflow {
        "overflow"
    } else {
        decision.reason.as_str()
    };
    handle.emit(RuntimeEvent::CompactionStart {
        reason: reason.into(),
    });
    let mut retry = RetryingProvider::new(
        provider,
        options.retry.clone(),
        handle.clone(),
        "compaction",
        None,
    );
    let mut candidate = session.clone();
    let outcome = compact_session(
        &mut candidate,
        &mut retry,
        options.keep_recent_user_turns,
        signal,
    );
    match outcome {
        Ok(result) => {
            let actual_summary_tokens = candidate
                .context
                .messages
                .first()
                .map_or(0.0, estimate_message_tokens);
            if actual_summary_tokens >= archive_tokens {
                let error =
                    "Automatic checkpoint has no positive measured context saving".to_owned();
                handle.emit(RuntimeEvent::CompactionEnd {
                    reason: reason.into(),
                    result: None,
                    error: Some(error.clone()),
                    aborted: false,
                });
                return Err(error);
            }
            *session = candidate;
            let actual_ratio = model.cache_ratio(tokens);
            let debt = tokens * actual_ratio.map_or(0.0, |ratio| (ratio - 1.0).max(0.0));
            let mut after_request = session.context.clone();
            after_request.system_prompt = request.system_prompt.clone();
            handle.committed_compaction(
                debt,
                (archive_tokens - actual_summary_tokens).max(0.0),
                actual_ratio.is_none(),
                result.usage.clone(),
                estimate_context_tokens_for_model(session, &after_request, model),
            );
            handle.emit(RuntimeEvent::CompactionEnd {
                reason: reason.into(),
                result: Some(result),
                error: None,
                aborted: false,
            });
            Ok(true)
        }
        Err(error) => {
            handle.emit(RuntimeEvent::CompactionEnd {
                reason: reason.into(),
                result: None,
                error: Some(error.clone()),
                aborted: signal.is_cancelled(),
            });
            Err(error)
        }
    }
}

/// Synchronize a successful explicit checkpoint with the same native cost ledger.
/// No provider request occurs here. Failure leaves the supplied post-checkpoint session and handle unchanged.
/// Callers that need whole-operation atomicity restore `before` if this transaction fails.
pub fn record_manual_checkpoint(
    session: &mut Session,
    before: &Session,
    result: &CompactionResult,
    model: Option<&ModelBudget>,
    handle: &AutomationHandle,
) -> Result<(), String> {
    if session.id != before.id {
        return Err("Manual checkpoint ledger requires the same session identity".into());
    }
    if model.is_some_and(|model| {
        model.provider.is_empty() || model.id.is_empty() || model.context_window == 0
    }) {
        return Err(
            "Manual checkpoint model budget has an invalid identity or context window".into(),
        );
    }
    let record = session
        .compactions
        .iter()
        .find(|record| record.id == result.id)
        .ok_or("Manual checkpoint archive was not committed")?;
    let branch = session.get_branch(None)?;
    if branch
        .iter()
        .rev()
        .find(|entry| entry.entry_type == "compaction")
        .is_none_or(|entry| entry.field("archiveId").as_str() != Some(result.id.as_str()))
        || record.summary != result.summary
        || record.usage != result.usage
        || record.archived_messages.len() != result.archived_message_count
        || record.kept_message_count != result.kept_message_count
        || !before
            .context
            .messages
            .starts_with(&record.archived_messages)
        || session.context.messages.len() != result.kept_message_count + 1
        || session.context.messages[1..] != before.context.messages[result.archived_message_count..]
    {
        return Err(
            "Manual checkpoint result does not match the selected branch and original context"
                .into(),
        );
    }
    let ledger = AutomationHandle::default();
    ledger.bind_session(before);
    if let Some(saved) = before.runtime_state("automation")? {
        ledger.restore_state(
            serde_json::from_value(saved)
                .map_err(|error| format!("Automatic runtime state: {error}"))?,
        )?;
        ledger.bind_session(before);
    } else {
        let report = handle.snapshot();
        let before_branch = before.get_branch(None)?;
        if report.session_id.as_ref() == Some(&before.id)
            && report
                .active_leaf_id
                .as_ref()
                .is_some_and(|leaf| before_branch.iter().any(|entry| &entry.id == leaf))
        {
            ledger.restore_state(report.state)?;
            ledger.bind_session(before);
        }
    }
    if let Some(model) = model {
        ledger.bind_model(model);
    }
    let tokens = model.map_or_else(
        || estimate_context_tokens(before, &before.context),
        |model| estimate_context_tokens_for_model(before, &before.context, model),
    );
    let after_tokens = model.map_or_else(
        || estimate_context_tokens(session, &session.context),
        |model| estimate_context_tokens_for_model(session, &session.context, model),
    );
    let ratio = model.and_then(|model| model.cache_ratio(tokens));
    let archive_tokens = record
        .archived_messages
        .iter()
        .map(estimate_message_tokens)
        .sum::<f64>();
    let summary_tokens = session
        .context
        .messages
        .first()
        .map_or(0.0, estimate_message_tokens);
    ledger.committed_compaction(
        tokens * ratio.map_or(0.0, |ratio| (ratio - 1.0).max(0.0)),
        (archive_tokens - summary_tokens).max(0.0),
        ratio.is_none(),
        result.usage.clone(),
        after_tokens,
    );
    let state = ledger.state();
    let mut candidate = session.clone();
    candidate.record_runtime_state(
        "automation",
        serde_json::to_value(&state).map_err(|error| error.to_string())?,
    )?;
    // All fallible validation and serialization precede the two committed snapshots.
    *session = candidate;
    handle.bind_session(session);
    {
        let mut report = handle.0.lock().unwrap_or_else(|error| error.into_inner());
        report.state = state;
        report.compactions += 1;
    }
    handle.emit(RuntimeEvent::CompactionEnd {
        reason: "manual".into(),
        result: Some(result.clone()),
        error: None,
        aborted: false,
    });
    Ok(())
}

pub fn recoverable_response(message: &AssistantMessage, model: &ModelBudget) -> bool {
    if message.stop_reason == StopReason::Length {
        return message.usage.as_ref().is_some_and(|usage| {
            model.max_output_tokens > 0 && usage.output < model.max_output_tokens
        });
    }
    false
}
