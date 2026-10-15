//! Tool preparation, execution, cancellation, and ordered result delivery.

use crate::types::{
    timestamp, AssistantMessage, CancellationToken, Content, Context, Event, ExecutionMode,
    Message, ToolCall, ToolResult, ToolResultMessage, Usage, Value,
};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::thread;

/// Native tools opt into parallel execution explicitly when using `Auto`.
pub trait AgentTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str {
        ""
    }
    fn parameters(&self) -> Value {
        serde_json::json!({ "type": "object" })
    }
    fn execution_mode(&self) -> Option<ExecutionMode> {
        None
    }
    fn prepare_arguments(&self, args: &Value) -> Result<Value, String> {
        Ok(args.clone())
    }
    fn validate_arguments(&self, args: &Value) -> Result<(), String> {
        validate_schema(&self.parameters(), args, "root").map_err(|reason| {
            format!(
                "Validation failed for tool \"{}\":\n  - {reason}\n\nReceived arguments:\n{}",
                self.name(),
                serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string())
            )
        })
    }
    fn execute(
        &self,
        call_id: &str,
        args: &Value,
        signal: &CancellationToken,
        on_update: &mut dyn FnMut(ToolResult),
    ) -> Result<ToolResult, String>;
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BeforeToolCallResult {
    pub block: bool,
    pub reason: Option<String>,
    pub terminate: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AfterToolCallResult {
    pub content: Option<Vec<Content>>,
    pub details: Option<Value>,
    pub usage: Option<Usage>,
    pub terminate: Option<bool>,
    pub is_error: Option<bool>,
}

pub struct BeforeToolCallContext<'a> {
    pub assistant_message: &'a AssistantMessage,
    pub tool_call: &'a ToolCall,
    /// Hooks may mutate validated arguments; execution does not revalidate them.
    pub args: &'a mut Value,
    pub context: &'a Context,
}

pub struct AfterToolCallContext<'a> {
    pub assistant_message: &'a AssistantMessage,
    pub tool_call: &'a ToolCall,
    pub args: &'a Value,
    pub result: &'a ToolResult,
    pub is_error: bool,
    pub context: &'a Context,
}

pub trait ToolHooks: Sync {
    fn before_tool_call(
        &self,
        _context: BeforeToolCallContext<'_>,
        _signal: &CancellationToken,
    ) -> Result<BeforeToolCallResult, String> {
        Ok(BeforeToolCallResult::default())
    }

    fn after_tool_call(
        &self,
        _context: AfterToolCallContext<'_>,
        _signal: &CancellationToken,
    ) -> Result<Option<AfterToolCallResult>, String> {
        Ok(None)
    }
}

pub struct NoopToolHooks;
impl ToolHooks for NoopToolHooks {}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExecutedToolCallBatch {
    pub messages: Vec<ToolResultMessage>,
    pub terminate: bool,
}
pub type ExecutedToolBatch = ExecutedToolCallBatch;

struct Prepared<'a> {
    call: ToolCall,
    tool: &'a dyn AgentTool,
    args: Value,
}

struct Finalized {
    call: ToolCall,
    result: ToolResult,
    is_error: bool,
}

enum Preparation<'a> {
    Prepared(Prepared<'a>),
    Immediate(Finalized),
}

enum WorkerMessage {
    Event(Event),
    Finalized(usize, Finalized),
}

/// Parallel calls are preflighted sequentially and run on at most four workers.
/// Completion events follow worker completion; transcript artifacts follow call order.
pub fn execute_tool_calls(
    tools: &[Box<dyn AgentTool>],
    context: &Context,
    assistant: &AssistantMessage,
    mode: ExecutionMode,
    hooks: &dyn ToolHooks,
    signal: &CancellationToken,
    emit: &mut dyn FnMut(Event),
) -> ExecutedToolCallBatch {
    let calls = assistant.tool_calls();
    let sequential = mode == ExecutionMode::Sequential
        || calls.iter().any(|call| {
            let tool_mode = tools
                .iter()
                .find(|tool| tool.name() == call.name)
                .and_then(|tool| tool.execution_mode());
            tool_mode == Some(ExecutionMode::Sequential)
                || (mode == ExecutionMode::Auto && tool_mode != Some(ExecutionMode::Parallel))
        });
    if sequential {
        let mut finalized = Vec::new();
        let mut messages = Vec::new();
        for call in calls {
            emit_start(&call, emit);
            let outcome = match prepare(tools, context, assistant, call, hooks, signal) {
                Preparation::Immediate(outcome) => outcome,
                Preparation::Prepared(prepared) => {
                    execute_prepared(&prepared, context, assistant, hooks, signal, emit)
                }
            };
            emit_end(&outcome, emit);
            messages.push(emit_result(&outcome, emit));
            finalized.push(outcome);
            if signal.is_cancelled() {
                break;
            }
        }
        return ExecutedToolCallBatch {
            messages,
            terminate: should_terminate(&finalized),
        };
    }

    let mut outcomes: Vec<Option<Finalized>> = Vec::new();
    let mut prepared = Vec::new();
    for call in calls {
        emit_start(&call, emit);
        let index = outcomes.len();
        match prepare(tools, context, assistant, call, hooks, signal) {
            Preparation::Immediate(outcome) => {
                emit_end(&outcome, emit);
                outcomes.push(Some(outcome));
            }
            Preparation::Prepared(call) => {
                outcomes.push(None);
                prepared.push((index, call));
            }
        }
        if signal.is_cancelled() {
            break;
        }
    }
    if !prepared.is_empty() {
        let next = AtomicUsize::new(0);
        let (sender, receiver) = mpsc::channel();
        thread::scope(|scope| {
            for _ in 0..prepared.len().min(4) {
                let sender = sender.clone();
                let prepared = &prepared;
                let next = &next;
                scope.spawn(move || loop {
                    let work = next.fetch_add(1, Ordering::Relaxed);
                    let Some((index, call)) = prepared.get(work) else {
                        break;
                    };
                    let mut emit_worker = |event| {
                        let _ = sender.send(WorkerMessage::Event(event));
                    };
                    let outcome = if signal.is_cancelled() {
                        error_outcome(call.call.clone(), "Operation aborted")
                    } else {
                        execute_prepared(call, context, assistant, hooks, signal, &mut emit_worker)
                    };
                    emit_end(&outcome, &mut emit_worker);
                    if sender
                        .send(WorkerMessage::Finalized(*index, outcome))
                        .is_err()
                    {
                        break;
                    }
                });
            }
            drop(sender);
            for item in receiver {
                match item {
                    WorkerMessage::Event(event) => emit(event),
                    WorkerMessage::Finalized(index, outcome) => outcomes[index] = Some(outcome),
                }
            }
        });
    }
    let finalized: Vec<Finalized> = outcomes.into_iter().flatten().collect();
    let messages = finalized
        .iter()
        .map(|outcome| emit_result(outcome, emit))
        .collect();
    ExecutedToolCallBatch {
        messages,
        terminate: should_terminate(&finalized),
    }
}

/// Truncated assistant arguments must never execute, even if they validate.
pub fn fail_tool_calls_from_truncated_message(
    calls: &[ToolCall],
    emit: &mut dyn FnMut(Event),
) -> ExecutedToolCallBatch {
    let messages = calls.iter().map(|call| {
        emit_start(call, emit);
        let outcome = error_outcome(call.clone(), format!(
            "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.", call.name
        ));
        emit_end(&outcome, emit);
        emit_result(&outcome, emit)
    }).collect();
    ExecutedToolCallBatch {
        messages,
        terminate: false,
    }
}

fn prepare<'a>(
    tools: &'a [Box<dyn AgentTool>],
    context: &Context,
    assistant: &AssistantMessage,
    call: ToolCall,
    hooks: &dyn ToolHooks,
    signal: &CancellationToken,
) -> Preparation<'a> {
    let Some(tool) = tools.iter().find(|tool| tool.name() == call.name) else {
        let reason = format!("Tool {} not found", call.name);
        return Preparation::Immediate(error_outcome(call, reason));
    };
    let prepared = guarded(|| {
        let mut args = tool.prepare_arguments(&call.arguments)?;
        normalize_arguments(&tool.parameters(), &mut args);
        tool.validate_arguments(&args)?;
        let before = hooks.before_tool_call(
            BeforeToolCallContext {
                assistant_message: assistant,
                tool_call: &call,
                args: &mut args,
                context,
            },
            signal,
        )?;
        Ok((args, before))
    });
    match prepared {
        Err(reason) => Preparation::Immediate(error_outcome(call, reason)),
        Ok(_) if signal.is_cancelled() => {
            Preparation::Immediate(error_outcome(call, "Operation aborted"))
        }
        Ok((_, before)) if before.block => {
            let reason = before
                .reason
                .filter(|reason| !reason.is_empty())
                .unwrap_or_else(|| "Tool execution was blocked".into());
            let mut outcome = error_outcome(call, reason);
            outcome.result.terminate = before.terminate;
            Preparation::Immediate(outcome)
        }
        Ok((args, _)) => Preparation::Prepared(Prepared {
            call,
            tool: tool.as_ref(),
            args,
        }),
    }
}

fn execute_prepared(
    prepared: &Prepared<'_>,
    context: &Context,
    assistant: &AssistantMessage,
    hooks: &dyn ToolHooks,
    signal: &CancellationToken,
    emit: &mut dyn FnMut(Event),
) -> Finalized {
    let executed = guarded(|| {
        prepared.tool.execute(
            &prepared.call.id,
            &prepared.args,
            signal,
            &mut |partial_result| {
                emit(Event::ToolExecutionUpdate {
                    tool_call_id: prepared.call.id.clone(),
                    tool_name: prepared.call.name.clone(),
                    args: prepared.call.arguments.clone(),
                    partial_result,
                });
            },
        )
    });
    let mut outcome = match executed {
        Ok(result) => Finalized {
            call: prepared.call.clone(),
            result,
            is_error: false,
        },
        Err(reason) => error_outcome(prepared.call.clone(), reason),
    };
    let after = guarded(|| {
        hooks.after_tool_call(
            AfterToolCallContext {
                assistant_message: assistant,
                tool_call: &prepared.call,
                args: &prepared.args,
                result: &outcome.result,
                is_error: outcome.is_error,
                context,
            },
            signal,
        )
    });
    match after {
        Ok(Some(patch)) => {
            if let Some(content) = patch.content {
                outcome.result.content = content;
            }
            if let Some(details) = patch.details {
                outcome.result.details = details;
            }
            if let Some(usage) = patch.usage {
                outcome.result.usage = Some(usage);
            }
            if let Some(terminate) = patch.terminate {
                outcome.result.terminate = terminate;
            }
            if let Some(is_error) = patch.is_error {
                outcome.is_error = is_error;
            }
        }
        Err(reason) => outcome = error_outcome(prepared.call.clone(), reason),
        Ok(None) => {}
    }
    outcome
}

fn guarded<T>(action: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    match catch_unwind(AssertUnwindSafe(action)) {
        Ok(result) => result,
        Err(payload) => Err(payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| {
                payload
                    .downcast_ref::<&str>()
                    .map(|message| (*message).to_string())
            })
            .unwrap_or_else(|| "Tool execution panicked".into())),
    }
}

fn error_outcome(call: ToolCall, message: impl Into<String>) -> Finalized {
    Finalized {
        call,
        result: ToolResult {
            content: vec![Content::text(message)],
            details: serde_json::json!({}),
            ..ToolResult::default()
        },
        is_error: true,
    }
}

fn should_terminate(outcomes: &[Finalized]) -> bool {
    !outcomes.is_empty() && outcomes.iter().all(|outcome| outcome.result.terminate)
}

fn emit_start(call: &ToolCall, emit: &mut dyn FnMut(Event)) {
    emit(Event::ToolExecutionStart {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        args: call.arguments.clone(),
    });
}

fn emit_end(outcome: &Finalized, emit: &mut dyn FnMut(Event)) {
    emit(Event::ToolExecutionEnd {
        tool_call_id: outcome.call.id.clone(),
        tool_name: outcome.call.name.clone(),
        result: outcome.result.clone(),
        is_error: outcome.is_error,
    });
}

fn emit_result(outcome: &Finalized, emit: &mut dyn FnMut(Event)) -> ToolResultMessage {
    let message = ToolResultMessage {
        tool_call_id: outcome.call.id.clone(),
        tool_name: outcome.call.name.clone(),
        content: outcome.result.content.clone(),
        details: outcome.result.details.clone(),
        usage: outcome.result.usage.clone(),
        added_tool_names: outcome.result.added_tool_names.clone(),
        is_error: outcome.is_error,
        timestamp: timestamp(),
    };
    emit(Event::MessageStart {
        message: Message::ToolResult(message.clone()),
    });
    emit(Event::MessageEnd {
        message: Message::ToolResult(message.clone()),
    });
    message
}

fn matches_type(value: &Value, kind: &str) -> bool {
    match kind {
        "null" => value.is_null(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "number" => value.is_number(),
        "integer" => value.as_f64().is_some_and(|number| number.fract() == 0.0),
        _ => false,
    }
}

fn schema_types(schema: &Value) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(kind)) => vec![kind],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

// Normalize primitive arguments and optional nulls for supported JSON schemas.
fn normalize_arguments(schema: &Value, value: &mut Value) {
    if let Some(branches) = schema.get("allOf").and_then(Value::as_array) {
        for branch in branches {
            normalize_arguments(branch, value);
        }
    }
    for union in ["anyOf", "oneOf"] {
        if let Some(branches) = schema.get(union).and_then(Value::as_array) {
            if !branches
                .iter()
                .any(|branch| validate_schema(branch, value, "root").is_ok())
            {
                for branch in branches {
                    let mut candidate = value.clone();
                    normalize_arguments(branch, &mut candidate);
                    if validate_schema(branch, &candidate, "root").is_ok() {
                        *value = candidate;
                        break;
                    }
                }
            }
        }
    }
    let kinds = schema_types(schema);
    if !kinds.iter().any(|kind| matches_type(value, kind)) {
        for kind in &kinds {
            let candidate = match *kind {
                "number" | "integer" => {
                    let number = match value {
                        Value::Null => Some(0.0),
                        Value::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
                        Value::String(text) if !text.trim().is_empty() => {
                            text.trim().parse::<f64>().ok()
                        }
                        _ => None,
                    };
                    number
                        .filter(|number| {
                            number.is_finite() && (*kind != "integer" || number.fract() == 0.0)
                        })
                        .and_then(|number| {
                            if number.fract() == 0.0 && number >= 0.0 && number < u64::MAX as f64 {
                                Some(serde_json::Number::from(number as u64))
                            } else if number.fract() == 0.0
                                && number >= i64::MIN as f64
                                && number < i64::MAX as f64
                            {
                                Some(serde_json::Number::from(number as i64))
                            } else {
                                serde_json::Number::from_f64(number)
                            }
                        })
                        .map(Value::Number)
                }
                "boolean" => match value {
                    Value::Null => Some(Value::Bool(false)),
                    Value::String(text) if text == "true" || text == "false" => {
                        Some(Value::Bool(text == "true"))
                    }
                    Value::Number(number)
                        if number.as_f64() == Some(1.0) || number.as_f64() == Some(0.0) =>
                    {
                        Some(Value::Bool(number.as_f64() == Some(1.0)))
                    }
                    _ => None,
                },
                "string" => match value {
                    Value::Null => Some(Value::String(String::new())),
                    Value::Number(_) | Value::Bool(_) => Some(Value::String(value.to_string())),
                    _ => None,
                },
                "null"
                    if value.as_str() == Some("")
                        || value.as_f64() == Some(0.0)
                        || value.as_bool() == Some(false) =>
                {
                    Some(Value::Null)
                }
                _ => None,
            };
            if let Some(candidate) = candidate {
                *value = candidate;
                break;
            }
        }
    }
    if kinds.contains(&"object") {
        if let Some(object) = value.as_object_mut() {
            let required = schema.get("required").and_then(Value::as_array);
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                for (name, property) in properties {
                    let optional = !required
                        .is_some_and(|names| names.iter().any(|item| item.as_str() == Some(name)));
                    if optional
                        && property.get("$ref").is_none()
                        && object.get(name).is_some_and(Value::is_null)
                        && validate_schema(property, &Value::Null, "root").is_err()
                    {
                        object.remove(name);
                    } else if let Some(value) = object.get_mut(name) {
                        normalize_arguments(property, value);
                    }
                }
            }
            if let Some(additional) = schema
                .get("additionalProperties")
                .filter(|item| item.is_object())
            {
                for (name, value) in object {
                    if schema
                        .get("properties")
                        .and_then(|properties| properties.get(name))
                        .is_none()
                    {
                        normalize_arguments(additional, value);
                    }
                }
            }
        }
    }
    if kinds.contains(&"array") {
        if let (Some(items), Some(values)) = (schema.get("items"), value.as_array_mut()) {
            for (index, value) in values.iter_mut().enumerate() {
                if let Some(item) = items
                    .as_array()
                    .and_then(|items| items.get(index))
                    .or_else(|| if items.is_array() { None } else { Some(items) })
                {
                    normalize_arguments(item, value);
                }
            }
        }
    }
}

/// Validation supports the structural JSON-schema keywords used by native tools.
/// Tools with custom formats or references should override `validate_arguments`.
fn validate_schema(schema: &Value, value: &Value, path: &str) -> Result<(), String> {
    if schema == &Value::Bool(false) {
        return Err(format!("{path}: value is not allowed"));
    }
    let fail = |reason: &str| Err(format!("{path}: {reason}"));
    for keyword in [
        "$ref",
        "$dynamicRef",
        "pattern",
        "format",
        "patternProperties",
        "propertyNames",
        "dependentRequired",
        "dependentSchemas",
        "unevaluatedProperties",
        "unevaluatedItems",
        "contains",
        "minContains",
        "maxContains",
        "if",
        "then",
        "else",
    ] {
        if schema.get(keyword).is_some() {
            return Err(format!("{path}: schema keyword {keyword} requires a custom validate_arguments implementation"));
        }
    }
    let kinds = schema_types(schema);
    if !kinds.is_empty() && !kinds.iter().any(|kind| matches_type(value, kind)) {
        return fail("unexpected argument type");
    }
    if schema
        .get("const")
        .is_some_and(|expected| expected != value)
    {
        return fail("value differs from const");
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        if !values.contains(value) {
            return fail("value is not in enum");
        }
    }
    for union in ["allOf", "anyOf", "oneOf"] {
        if let Some(branches) = schema.get(union).and_then(Value::as_array) {
            let matches = branches
                .iter()
                .filter(|branch| validate_schema(branch, value, path).is_ok())
                .count();
            if (union == "allOf" && matches != branches.len())
                || (union == "anyOf" && matches == 0)
                || (union == "oneOf" && matches != 1)
            {
                return fail("schema branches do not match");
            }
        }
    }
    if let Some(not) = schema.get("not") {
        if validate_schema(not, value, path).is_ok() {
            return fail("value matches excluded schema");
        }
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for name in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(name) {
                    return Err(format!("{path}.{name}: required argument missing"));
                }
            }
        }
        for (name, value) in object {
            if let Some(property) = schema
                .get("properties")
                .and_then(|properties| properties.get(name))
            {
                validate_schema(property, value, &format!("{path}.{name}"))?;
            } else if let Some(additional) = schema.get("additionalProperties") {
                validate_schema(additional, value, &format!("{path}.{name}"))?;
            }
        }
        validate_length(schema, "Properties", object.len(), path)?;
    }
    if let Some(values) = value.as_array() {
        validate_length(schema, "Items", values.len(), path)?;
        if schema.get("uniqueItems").and_then(Value::as_bool) == Some(true) {
            for (index, item) in values.iter().enumerate() {
                if values[..index].contains(item) {
                    return fail("array items must be unique");
                }
            }
        }
        if let Some(items) = schema.get("items") {
            for (index, value) in values.iter().enumerate() {
                if let Some(item) = items
                    .as_array()
                    .and_then(|items| items.get(index))
                    .or_else(|| if items.is_array() { None } else { Some(items) })
                {
                    validate_schema(item, value, &format!("{path}.{index}"))?;
                }
            }
        }
    }
    if let Some(text) = value.as_str() {
        validate_length(schema, "Length", text.chars().count(), path)?;
    }
    if let Some(number) = value.as_f64() {
        if let Some(divisor) = schema.get("multipleOf").and_then(Value::as_f64) {
            if divisor <= 0.0
                || (number / divisor - (number / divisor).round()).abs() > f64::EPSILON * 8.0
            {
                return fail("number is not a multipleOf the schema divisor");
            }
        }
        if schema
            .get("minimum")
            .and_then(Value::as_f64)
            .is_some_and(|bound| number < bound)
            || schema
                .get("maximum")
                .and_then(Value::as_f64)
                .is_some_and(|bound| number > bound)
            || schema
                .get("exclusiveMinimum")
                .and_then(Value::as_f64)
                .is_some_and(|bound| number <= bound)
            || schema
                .get("exclusiveMaximum")
                .and_then(Value::as_f64)
                .is_some_and(|bound| number >= bound)
        {
            return fail("number is outside schema bounds");
        }
    }
    Ok(())
}

fn validate_length(schema: &Value, suffix: &str, length: usize, path: &str) -> Result<(), String> {
    let length = length as u64;
    if schema
        .get(format!("min{suffix}"))
        .and_then(Value::as_u64)
        .is_some_and(|bound| length < bound)
        || schema
            .get(format!("max{suffix}"))
            .and_then(Value::as_u64)
            .is_some_and(|bound| length > bound)
    {
        return Err(format!("{path}: length is outside schema bounds"));
    }
    Ok(())
}
