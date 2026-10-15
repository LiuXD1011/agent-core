/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: MIT
 */

//! Model-request assembly and context compaction economics.
//!
//! The arithmetic and reason precedence mirror `packages/agent/src/context/budget.ts`.
//! Hooks execute synchronously in Rust; provider I/O belongs to the runtime adapter.

use crate::types::{AbortSignal, Context, Message};

/// Per-request extension points, in transform → convert → credential order.
pub trait RequestHooks {
    fn transform_context(
        &self,
        messages: &[Message],
        _signal: Option<&AbortSignal>,
    ) -> Vec<Message> {
        messages.to_vec()
    }

    fn convert_to_llm(&self, messages: &[Message]) -> Vec<Message> {
        messages
            .iter()
            .filter(|message| !matches!(message, Message::Custom { .. }))
            .cloned()
            .collect()
    }

    fn get_api_key(&self, _provider: &str) -> Option<String> {
        None
    }
}

impl RequestHooks for () {}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelRequest {
    pub context: Context,
    pub api_key: Option<String>,
}

/// Rebuild a fresh provider request without changing the original transcript.
pub fn build_model_request(
    context: &Context,
    hooks: &dyn RequestHooks,
    signal: Option<&AbortSignal>,
    provider: &str,
    configured_api_key: Option<&str>,
) -> ModelRequest {
    let messages = hooks.transform_context(&context.messages, signal);
    let messages = hooks.convert_to_llm(&messages);
    let api_key = hooks
        .get_api_key(provider)
        .filter(|key| !key.is_empty())
        .or_else(|| configured_api_key.map(str::to_owned));
    ModelRequest {
        context: Context {
            system_prompt: context.system_prompt.clone(),
            messages,
        },
        api_key,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompactionEconomics {
    pub remaining_request_scale: f64,
    pub remaining_request_stddev_k: f64,
    pub window_reserve_tokens: f64,
    pub first_compaction_request_scale: f64,
    pub subsequent_compaction_margin: f64,
}

pub const DEFAULT_COMPACTION_ECONOMICS: CompactionEconomics = CompactionEconomics {
    remaining_request_scale: 1.0,
    remaining_request_stddev_k: 0.0,
    window_reserve_tokens: 16_384.0,
    first_compaction_request_scale: 2.0,
    subsequent_compaction_margin: 1.5,
};

impl Default for CompactionEconomics {
    fn default() -> Self {
        DEFAULT_COMPACTION_ECONOMICS
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactionReason {
    Economic,
    WindowProtection,
    DeferredEconomic,
    DeferredSubsequentMargin,
    DeferredCarriedDebt,
    HorizonUnavailable,
    CacheRatioUnavailable,
    NativeNotCompactable,
    NonPositiveSaving,
}

impl CompactionReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Economic => "economic",
            Self::WindowProtection => "window_protection",
            Self::DeferredEconomic => "deferred_economic",
            Self::DeferredSubsequentMargin => "deferred_subsequent_margin",
            Self::DeferredCarriedDebt => "deferred_carried_debt",
            Self::HorizonUnavailable => "horizon_unavailable",
            Self::CacheRatioUnavailable => "cache_ratio_unavailable",
            Self::NativeNotCompactable => "native_not_compactable",
            Self::NonPositiveSaving => "non_positive_saving",
        }
    }
}

#[derive(Clone, Debug)]
pub struct RequestHorizonInput<'a> {
    pub completed_boundary_request_counts: &'a [f64],
    pub remaining_boundaries: f64,
    pub scale: f64,
    pub standard_deviation_k: f64,
    pub context_tokens: f64,
    pub context_window_tokens: Option<f64>,
    pub average_context_token_increment: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RequestHorizonEstimate {
    pub completed_boundary_request_counts: Vec<f64>,
    pub requests_per_boundary_mean: f64,
    pub requests_per_boundary_lower_bound: f64,
    pub unbounded_expected_remaining_requests: f64,
    pub average_context_token_increment: Option<f64>,
    pub window_request_upper_bound: Option<f64>,
    pub expected_remaining_requests: f64,
}

#[derive(Clone, Debug)]
pub struct CompactionInput<'a> {
    pub write_tokens: f64,
    pub archive_tokens: f64,
    pub memo_tokens: f64,
    pub context_tokens: f64,
    pub completed_boundary_request_counts: Option<&'a [f64]>,
    pub remaining_boundaries: f64,
    pub average_context_token_increment: Option<f64>,
    pub context_window_tokens: Option<f64>,
    pub prior_compaction_count: usize,
    pub carried_debt_tokens: f64,
    pub cache_debt_repayment_tokens: f64,
    pub cache_write_read_ratio: Option<f64>,
    pub economics: CompactionEconomics,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompactionDecision {
    pub write_tokens: f64,
    pub archive_tokens: f64,
    pub memo_tokens: f64,
    pub context_tokens: f64,
    pub completed_boundary_request_counts: Option<Vec<f64>>,
    pub requests_per_boundary_mean: Option<f64>,
    pub requests_per_boundary_lower_bound: Option<f64>,
    pub unbounded_expected_remaining_requests: Option<f64>,
    pub average_context_token_increment: Option<f64>,
    pub window_request_upper_bound: Option<f64>,
    pub expected_remaining_requests: Option<f64>,
    pub breakeven_requests: Option<f64>,
    pub combined_breakeven_requests: Option<f64>,
    pub effective_horizon_requests: Option<f64>,
    pub cache_write_read_ratio: Option<f64>,
    pub incremental_cache_cost_ratio: Option<f64>,
    pub prior_compaction_count: usize,
    pub carried_debt_tokens: f64,
    pub cache_debt_repayment_tokens: f64,
    pub compact: bool,
    pub reason: CompactionReason,
}

// Rust's f64::min/max ignore NaN; JavaScript's Math.min/max propagate it.
fn number_min(left: f64, right: f64) -> f64 {
    if left.is_nan() || right.is_nan() {
        f64::NAN
    } else if left == 0.0 && right == 0.0 {
        if left.is_sign_negative() || right.is_sign_negative() {
            -0.0
        } else {
            0.0
        }
    } else {
        left.min(right)
    }
}

fn number_max(left: f64, right: f64) -> f64 {
    if left.is_nan() || right.is_nan() {
        f64::NAN
    } else if left == 0.0 && right == 0.0 {
        if left.is_sign_positive() || right.is_sign_positive() {
            0.0
        } else {
            -0.0
        }
    } else {
        left.max(right)
    }
}

pub fn estimate_remaining_requests(input: &RequestHorizonInput<'_>) -> RequestHorizonEstimate {
    let count = input.completed_boundary_request_counts.len();
    let mean = input
        .completed_boundary_request_counts
        .iter()
        .fold(0.0, |total, count| total + count)
        / count.max(1) as f64;
    let mut lower_bound = mean;
    if input.standard_deviation_k != 0.0 {
        if count < 3 {
            lower_bound *= 0.5;
        } else {
            let variance = input
                .completed_boundary_request_counts
                .iter()
                .map(|count| (count - mean).powi(2))
                .fold(0.0, |total, squared_difference| total + squared_difference);
            let deviation = (variance / (count - 1) as f64).sqrt();
            lower_bound = number_max(0.0, mean - input.standard_deviation_k * deviation);
        }
    }
    let unbounded_expected_remaining_requests =
        1.0 + (lower_bound * number_max(0.0, input.remaining_boundaries) * input.scale).floor();
    let window_request_upper_bound = match (
        input.context_window_tokens,
        input.average_context_token_increment,
    ) {
        (Some(window), Some(increment)) if increment > 0.0 || increment.is_nan() => Some(
            number_max(0.0, ((window - input.context_tokens) / increment).floor()),
        ),
        _ => None,
    };
    RequestHorizonEstimate {
        completed_boundary_request_counts: input.completed_boundary_request_counts.to_vec(),
        requests_per_boundary_mean: mean,
        requests_per_boundary_lower_bound: lower_bound,
        unbounded_expected_remaining_requests,
        average_context_token_increment: input.average_context_token_increment,
        window_request_upper_bound,
        expected_remaining_requests: window_request_upper_bound
            .map_or(unbounded_expected_remaining_requests, |upper| {
                number_min(unbounded_expected_remaining_requests, upper)
            }),
    }
}

pub fn decide_compaction(input: &CompactionInput<'_>) -> CompactionDecision {
    let horizon = input.completed_boundary_request_counts.map(|counts| {
        estimate_remaining_requests(&RequestHorizonInput {
            completed_boundary_request_counts: counts,
            remaining_boundaries: input.remaining_boundaries,
            scale: input.economics.remaining_request_scale,
            standard_deviation_k: input.economics.remaining_request_stddev_k,
            context_tokens: input.context_tokens,
            context_window_tokens: input.context_window_tokens,
            average_context_token_increment: input.average_context_token_increment,
        })
    });
    let saving_tokens = input.archive_tokens - input.memo_tokens;
    let incremental_cache_cost_ratio = input
        .cache_write_read_ratio
        .map(|ratio| number_max(0.0, ratio - 1.0));
    let breakeven_requests = incremental_cache_cost_ratio
        .filter(|_| saving_tokens > 0.0)
        .map(|ratio| input.write_tokens * ratio / saving_tokens);
    let combined_breakeven_requests = incremental_cache_cost_ratio
        .filter(|_| saving_tokens > 0.0)
        .map(|ratio| (input.carried_debt_tokens + input.write_tokens * ratio) / saving_tokens);
    let first_compaction = input.prior_compaction_count == 0;
    let effective_horizon_requests = horizon.as_ref().map(|horizon| {
        if first_compaction {
            number_min(
                horizon.expected_remaining_requests
                    * input.economics.first_compaction_request_scale,
                horizon.window_request_upper_bound.unwrap_or(f64::INFINITY),
            )
        } else {
            horizon.expected_remaining_requests
        }
    });
    let window_protection = input.context_window_tokens.is_some_and(|window| {
        input.context_tokens >= window - input.economics.window_reserve_tokens
    });
    let base_economic = horizon.as_ref().is_some_and(|horizon| {
        horizon.expected_remaining_requests > 0.0
            && breakeven_requests
                .is_some_and(|breakeven| breakeven <= horizon.expected_remaining_requests)
    });
    let first_economic = first_compaction
        && effective_horizon_requests.is_some_and(|effective| {
            effective > 0.0 && breakeven_requests.is_some_and(|breakeven| breakeven <= effective)
        });
    let subsequent_margin_open = !first_compaction
        && horizon.as_ref().is_some_and(|horizon| {
            breakeven_requests.is_some_and(|breakeven| {
                breakeven * input.economics.subsequent_compaction_margin
                    <= horizon.expected_remaining_requests
            })
        });
    let carried_debt_gate_open = !first_compaction
        && horizon.as_ref().is_some_and(|horizon| {
            combined_breakeven_requests
                .is_some_and(|combined| combined <= horizon.expected_remaining_requests)
        });
    let economic = if first_compaction {
        first_economic
    } else {
        base_economic && subsequent_margin_open && carried_debt_gate_open
    };
    let compressible = saving_tokens > 0.0;
    let reason = if !compressible {
        CompactionReason::NonPositiveSaving
    } else if window_protection {
        CompactionReason::WindowProtection
    } else if economic {
        CompactionReason::Economic
    } else if horizon.is_none() {
        CompactionReason::HorizonUnavailable
    } else if breakeven_requests.is_none() {
        CompactionReason::CacheRatioUnavailable
    } else if !first_compaction && base_economic && !subsequent_margin_open {
        CompactionReason::DeferredSubsequentMargin
    } else if !first_compaction && base_economic && !carried_debt_gate_open {
        CompactionReason::DeferredCarriedDebt
    } else {
        CompactionReason::DeferredEconomic
    };
    CompactionDecision {
        write_tokens: input.write_tokens,
        archive_tokens: input.archive_tokens,
        memo_tokens: input.memo_tokens,
        context_tokens: input.context_tokens,
        completed_boundary_request_counts: horizon
            .as_ref()
            .map(|horizon| horizon.completed_boundary_request_counts.clone()),
        requests_per_boundary_mean: horizon
            .as_ref()
            .map(|horizon| horizon.requests_per_boundary_mean),
        requests_per_boundary_lower_bound: horizon
            .as_ref()
            .map(|horizon| horizon.requests_per_boundary_lower_bound),
        unbounded_expected_remaining_requests: horizon
            .as_ref()
            .map(|horizon| horizon.unbounded_expected_remaining_requests),
        average_context_token_increment: input.average_context_token_increment,
        window_request_upper_bound: horizon
            .as_ref()
            .and_then(|horizon| horizon.window_request_upper_bound),
        expected_remaining_requests: horizon
            .as_ref()
            .map(|horizon| horizon.expected_remaining_requests),
        breakeven_requests,
        combined_breakeven_requests,
        effective_horizon_requests,
        cache_write_read_ratio: input.cache_write_read_ratio,
        incremental_cache_cost_ratio,
        prior_compaction_count: input.prior_compaction_count,
        carried_debt_tokens: input.carried_debt_tokens,
        cache_debt_repayment_tokens: input.cache_debt_repayment_tokens,
        compact: compressible && (window_protection || economic),
        reason,
    }
}
