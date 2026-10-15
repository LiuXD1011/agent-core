//! Bounded, opt-in provider-call retries. Prompts and tool executions are never rerun.

use crate::automatic::{AutomationHandle, RuntimeEvent};
use agent_core::agent_loop::Provider;
use agent_core::types::{AssistantMessage, CancellationToken, Content, Context, StopReason};
use serde::{Deserialize, Serialize};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RetryOptions {
    pub enabled: bool,
    pub max_retries: usize,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for RetryOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            max_retries: 3,
            base_delay_ms: 2_000,
            max_delay_ms: 60_000,
        }
    }
}

impl RetryOptions {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_retries > 16 || self.max_delay_ms > 86_400_000 {
            return Err(
                "Retry budget must be at most 16 retries and delay cap at most one day".into(),
            );
        }
        Ok(())
    }
    pub fn delay_ms(&self, attempt: usize) -> u64 {
        self.base_delay_ms
            .saturating_mul(
                1_u64
                    .checked_shl(attempt.saturating_sub(1) as u32)
                    .unwrap_or(u64::MAX),
            )
            .min(self.max_delay_ms)
    }
}

pub fn is_context_overflow(error: &str) -> bool {
    let text = error.to_lowercase();
    if text.contains("rate limit")
        || text.contains("too many requests")
        || text.starts_with("throttling error:")
        || text.starts_with("service unavailable:")
    {
        return false;
    }
    [
        "prompt is too long",
        "request_too_large",
        "input is too long for requested model",
        "exceeds the context window",
        "maximum context length",
        "input token count",
        "maximum prompt length",
        "reduce the length of the messages",
        "maximum allowed input length",
        "exceeds the available context size",
        "greater than the context length",
        "context window exceeds limit",
        "exceeded model token limit",
        "model_context_window_exceeded",
        "prompt too long; exceeded",
        "range of input length should be",
        "context_length_exceeded",
        "context length exceeded",
        "context_window_exceeded",
        "too_many_tokens",
        "too many tokens",
        "token limit exceeded",
        "configured context size",
    ]
    .iter()
    .any(|pattern| text.contains(pattern))
}

pub fn has_partial_output(message: &AssistantMessage) -> bool {
    message.content.iter().any(|part| match part {
        Content::Text { text } => !text.is_empty(),
        Content::Thinking { thinking } => !thinking.is_empty(),
        Content::Image { data, .. } => !data.is_empty(),
        Content::ToolCall(_) => true,
    }) || message.protocol_metadata.as_ref().is_some_and(|metadata| {
        metadata["items"]
            .as_object()
            .is_some_and(|items| !items.is_empty())
            || metadata["signatures"]
                .as_object()
                .is_some_and(|signatures| !signatures.is_empty())
            || metadata["blocks"].as_object().is_some_and(|blocks| {
                blocks.values().any(|block| {
                    block["signature"]
                        .as_str()
                        .is_some_and(|signature| !signature.is_empty())
                })
            })
    })
}

/// Known deterministic failures override transient-looking substrings, unlike broad numeric matching.
pub fn is_transient_error(error: &str) -> bool {
    let text = error.to_lowercase();
    if is_context_overflow(error)
        || [
            "gousagelimiterror",
            "freeusagelimiterror",
            "monthly usage limit reached",
            "available balance",
            "insufficient_quota",
            "out of budget",
            "quota exceeded",
            "billing",
            "unauthorized",
            "authentication",
            "invalid api key",
            "invalid tool arguments",
            "invalid json",
            "invalid anthropic",
            "invalid responses",
            "duplicate",
            "conflicting",
            "unsupported",
            "has not been migrated",
        ]
        .iter()
        .any(|pattern| text.contains(pattern))
    {
        return false;
    }
    if (text.contains("http") || text.contains("status"))
        && text
            .split(|character: char| !character.is_ascii_alphanumeric())
            .any(|part| matches!(part, "429" | "500" | "502" | "503" | "504" | "524" | "529"))
    {
        return true;
    }
    [
        "overloaded",
        "rate limit",
        "rate-limit",
        "rate_limit",
        "too many requests",
        "service unavailable",
        "server error",
        "internal error",
        "network error",
        "connection error",
        "connection refused",
        "connection lost",
        "connection reset",
        "other side closed",
        "fetch failed",
        "getaddrinfo",
        "enotfound",
        "eai_again",
        "upstream connect",
        "reset before headers",
        "socket hang up",
        "socket connection was closed",
        "timed out",
        "timeout",
        "websocket closed",
        "websocket error",
        "stream ended without",
        "stream ended before message_stop",
        "stream ended before a terminal response event",
        "http2 request did not get a response",
        "you can retry your request",
        "try your request again",
        "please retry your request",
        "resourceexhausted",
    ]
    .iter()
    .any(|pattern| text.contains(pattern))
}

pub fn wait_retry(delay_ms: u64, signal: &CancellationToken) -> Result<(), String> {
    let started = Instant::now();
    let duration = Duration::from_millis(delay_ms);
    loop {
        if signal.is_cancelled() {
            return Err("Retry cancelled".into());
        }
        let elapsed = started.elapsed();
        if elapsed >= duration {
            return Ok(());
        }
        thread::sleep((duration - elapsed).min(Duration::from_millis(10)));
    }
}

pub struct RetryingProvider<'a> {
    provider: &'a mut dyn Provider,
    options: RetryOptions,
    handle: AutomationHandle,
    source: String,
    request_tokens: Option<f64>,
}

impl<'a> RetryingProvider<'a> {
    pub fn new(
        provider: &'a mut dyn Provider,
        options: RetryOptions,
        handle: AutomationHandle,
        source: &str,
        request_tokens: Option<f64>,
    ) -> Self {
        Self {
            provider,
            options,
            handle,
            source: source.into(),
            request_tokens,
        }
    }
    fn call(
        &mut self,
        context: &Context,
        signal: &CancellationToken,
        on_update: &mut dyn FnMut(AssistantMessage),
        summary: bool,
    ) -> Result<AssistantMessage, String> {
        self.options.validate()?;
        let mut attempt = 0;
        loop {
            if signal.is_cancelled() {
                if attempt > 0 {
                    self.finished(false, attempt, Some("Retry cancelled".into()));
                }
                return Ok(AssistantMessage::error("Operation aborted", true));
            }
            if let Some(tokens) = self.request_tokens {
                self.handle.record_request(tokens);
            }
            let mut partial: Option<AssistantMessage> = None;
            let mut produced_output = false;
            let mut update = |message: AssistantMessage| {
                produced_output |= has_partial_output(&message);
                partial = Some(message.clone());
                on_update(message);
            };
            let result = if summary {
                self.provider.complete_summary(context, signal, &mut update)
            } else {
                self.provider.complete(context, signal, &mut update)
            };
            let response = match &result {
                Ok(response) => response.clone(),
                Err(error) => {
                    let mut response = partial
                        .unwrap_or_else(|| AssistantMessage::error(error, signal.is_cancelled()));
                    response.stop_reason = if signal.is_cancelled() {
                        StopReason::Aborted
                    } else {
                        StopReason::Error
                    };
                    response.error_message = Some(error.clone());
                    response
                }
            };
            let error = response.error_message.as_deref().unwrap_or("Unknown error");
            produced_output |= has_partial_output(&response);
            if signal.is_cancelled() || response.stop_reason == StopReason::Aborted {
                if attempt > 0 {
                    self.finished(false, attempt, response.error_message.clone());
                }
                let mut aborted = response;
                aborted.stop_reason = StopReason::Aborted;
                aborted.error_message = Some("Operation aborted".into());
                return Ok(aborted);
            }
            if response.stop_reason != StopReason::Error {
                if attempt > 0 {
                    self.finished(true, attempt, None);
                }
                return result;
            }
            if produced_output
                || !self.options.enabled
                || attempt >= self.options.max_retries
                || !is_transient_error(error)
            {
                if attempt > 0 {
                    self.finished(false, attempt, response.error_message.clone());
                }
                return result;
            }
            attempt += 1;
            let delay = self.options.delay_ms(attempt);
            self.handle.emit(RuntimeEvent::RetryScheduled {
                source: self.source.clone(),
                attempt,
                max_attempts: self.options.max_retries,
                delay_ms: delay,
                error: error.into(),
            });
            if wait_retry(delay, signal).is_err() {
                self.finished(false, attempt, Some("Retry cancelled".into()));
                let mut aborted = response;
                aborted.stop_reason = StopReason::Aborted;
                aborted.error_message = Some("Retry cancelled".into());
                return Ok(aborted);
            }
            // Start a new stream snapshot; a failed attempt's partial tools never enter execution.
            on_update(AssistantMessage::text(""));
            self.handle.emit(RuntimeEvent::RetryAttemptStart {
                source: self.source.clone(),
                attempt,
            });
        }
    }
    fn finished(&self, success: bool, attempt: usize, error: Option<String>) {
        self.handle.emit(RuntimeEvent::RetryFinished {
            source: self.source.clone(),
            success,
            attempt,
            error,
        });
    }
}

impl Provider for RetryingProvider<'_> {
    fn complete(
        &mut self,
        context: &Context,
        signal: &CancellationToken,
        update: &mut dyn FnMut(AssistantMessage),
    ) -> Result<AssistantMessage, String> {
        self.call(context, signal, update, false)
    }
    fn complete_summary(
        &mut self,
        context: &Context,
        signal: &CancellationToken,
        update: &mut dyn FnMut(AssistantMessage),
    ) -> Result<AssistantMessage, String> {
        self.call(context, signal, update, true)
    }
}
