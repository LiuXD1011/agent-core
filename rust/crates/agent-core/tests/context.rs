use agent_core::context::{
    build_model_request, decide_compaction, estimate_remaining_requests, CompactionEconomics,
    CompactionInput, CompactionReason, RequestHooks, RequestHorizonInput,
    DEFAULT_COMPACTION_ECONOMICS,
};
use agent_core::types::{AbortSignal, Context, Message, Value};
use std::cell::{Cell, RefCell};

fn decision_input() -> CompactionInput<'static> {
    CompactionInput {
        write_tokens: 80_000.0,
        archive_tokens: 60_000.0,
        memo_tokens: 1_000.0,
        context_tokens: 80_000.0,
        completed_boundary_request_counts: Some(&[4.0, 6.0, 5.0]),
        remaining_boundaries: 4.0,
        average_context_token_increment: Some(2_000.0),
        context_window_tokens: Some(200_000.0),
        prior_compaction_count: 0,
        carried_debt_tokens: 0.0,
        cache_debt_repayment_tokens: 0.0,
        cache_write_read_ratio: Some(1.0),
        economics: DEFAULT_COMPACTION_ECONOMICS,
    }
}

fn horizon_input() -> RequestHorizonInput<'static> {
    RequestHorizonInput {
        completed_boundary_request_counts: &[4.0, 6.0, 5.0],
        remaining_boundaries: 3.0,
        scale: 1.0,
        standard_deviation_k: 0.0,
        context_tokens: 100_000.0,
        context_window_tokens: Some(200_000.0),
        average_context_token_increment: Some(5_000.0),
    }
}

#[test]
fn estimates_remaining_horizon_from_completed_boundaries() {
    let result = estimate_remaining_requests(&horizon_input());
    assert_eq!(result.requests_per_boundary_mean, 5.0);
    assert_eq!(result.expected_remaining_requests, 16.0);
    assert_eq!(result.window_request_upper_bound, Some(20.0));
    assert_eq!(result.completed_boundary_request_counts, [4.0, 6.0, 5.0]);
}

#[test]
fn rejects_non_positive_savings_before_window_protection() {
    let mut input = decision_input();
    input.archive_tokens = 500.0;
    input.context_tokens = 195_000.0;
    let result = decide_compaction(&input);
    assert!(!result.compact);
    assert_eq!(result.reason, CompactionReason::NonPositiveSaving);
    assert_eq!(result.breakeven_requests, None);
    input.archive_tokens = input.memo_tokens;
    assert!(!decide_compaction(&input).compact);
}

#[test]
fn economic_breakeven_fits_the_horizon() {
    let result = decide_compaction(&decision_input());
    assert!(result.compact);
    assert_eq!(result.reason, CompactionReason::Economic);
    assert_eq!(result.expected_remaining_requests, Some(21.0));
    assert_eq!(result.effective_horizon_requests, Some(42.0));
    assert_eq!(result.incremental_cache_cost_ratio, Some(0.0));
}

#[test]
fn window_protection_overrides_missing_horizon_and_ratio() {
    let mut input = decision_input();
    input.context_tokens = 195_000.0;
    input.cache_write_read_ratio = Some(100.0);
    input.economics.window_reserve_tokens = 10_000.0;
    assert_eq!(
        decide_compaction(&input).reason,
        CompactionReason::WindowProtection
    );
    input.completed_boundary_request_counts = None;
    input.cache_write_read_ratio = None;
    assert!(decide_compaction(&input).compact);
    assert_eq!(
        decide_compaction(&input).reason,
        CompactionReason::WindowProtection
    );
}

#[test]
fn defers_without_cache_ratio() {
    let mut input = decision_input();
    input.cache_write_read_ratio = None;
    let result = decide_compaction(&input);
    assert!(!result.compact);
    assert_eq!(result.reason, CompactionReason::CacheRatioUnavailable);
    assert_eq!(result.breakeven_requests, None);
}

#[test]
fn charges_carried_debt_only_after_first_compaction() {
    let mut input = decision_input();
    input.prior_compaction_count = 1;
    input.cache_write_read_ratio = Some(2.0);
    input.carried_debt_tokens = 2_000_000.0;
    let result = decide_compaction(&input);
    assert!(!result.compact);
    assert_eq!(result.reason, CompactionReason::DeferredCarriedDebt);
    assert!(result.combined_breakeven_requests > result.breakeven_requests);
    input.prior_compaction_count = 0;
    assert!(decide_compaction(&input).compact);
}

#[test]
fn defers_without_completed_boundary_data_and_preserves_telemetry() {
    let mut input = decision_input();
    input.completed_boundary_request_counts = None;
    input.cache_write_read_ratio = None;
    input.cache_debt_repayment_tokens = 123.0;
    let result = decide_compaction(&input);
    assert_eq!(result.reason, CompactionReason::HorizonUnavailable);
    assert_eq!(result.completed_boundary_request_counts, None);
    assert_eq!(result.requests_per_boundary_mean, None);
    assert_eq!(result.expected_remaining_requests, None);
    assert_eq!(result.window_request_upper_bound, None);
    assert_eq!(result.average_context_token_increment, Some(2_000.0));
    assert_eq!(result.cache_debt_repayment_tokens, 123.0);
}

#[test]
fn small_samples_halve_lower_bound_only_when_deviation_is_enabled() {
    let mut input = horizon_input();
    input.completed_boundary_request_counts = &[4.0, 6.0];
    input.standard_deviation_k = 1.0;
    let result = estimate_remaining_requests(&input);
    assert_eq!(result.requests_per_boundary_mean, 5.0);
    assert_eq!(result.requests_per_boundary_lower_bound, 2.5);
    assert_eq!(result.expected_remaining_requests, 8.0);
    input.standard_deviation_k = 0.0;
    assert_eq!(
        estimate_remaining_requests(&input).expected_remaining_requests,
        16.0
    );
}

#[test]
fn uses_sample_standard_deviation_and_clamps_lower_bound() {
    let mut input = horizon_input();
    input.standard_deviation_k = 1.0;
    let result = estimate_remaining_requests(&input);
    assert_eq!(result.requests_per_boundary_lower_bound, 4.0);
    assert_eq!(result.expected_remaining_requests, 13.0);
    input.standard_deviation_k = 10.0;
    assert_eq!(
        estimate_remaining_requests(&input).requests_per_boundary_lower_bound,
        0.0
    );
}

#[test]
fn keeps_current_request_for_empty_data_and_negative_remaining_boundaries() {
    let mut input = horizon_input();
    input.completed_boundary_request_counts = &[];
    assert_eq!(
        estimate_remaining_requests(&input).expected_remaining_requests,
        1.0
    );
    input.completed_boundary_request_counts = &[4.0, 6.0, 5.0];
    input.remaining_boundaries = -3.0;
    assert_eq!(
        estimate_remaining_requests(&input).expected_remaining_requests,
        1.0
    );
}

#[test]
fn clips_horizon_to_window_and_disables_bound_for_nonpositive_increment() {
    let mut input = horizon_input();
    input.context_tokens = 199_000.0;
    let result = estimate_remaining_requests(&input);
    assert_eq!(result.unbounded_expected_remaining_requests, 16.0);
    assert_eq!(result.expected_remaining_requests, 0.0);
    for increment in [None, Some(0.0), Some(-1.0)] {
        input.average_context_token_increment = increment;
        let result = estimate_remaining_requests(&input);
        assert_eq!(result.window_request_upper_bound, None);
        assert_eq!(result.expected_remaining_requests, 16.0);
    }
    input.context_window_tokens = None;
    input.average_context_token_increment = Some(5_000.0);
    assert_eq!(
        estimate_remaining_requests(&input).window_request_upper_bound,
        None
    );
}

#[test]
fn first_compaction_uses_scaled_horizon_without_exceeding_window() {
    let mut input = decision_input();
    input.write_tokens = 30.0;
    input.archive_tokens = 2.0;
    input.memo_tokens = 1.0;
    input.cache_write_read_ratio = Some(2.0);
    assert!(decide_compaction(&input).compact);
    input.context_window_tokens = Some(130_000.0);
    let result = decide_compaction(&input);
    assert_eq!(result.effective_horizon_requests, Some(25.0));
    assert!(!result.compact);
    assert_eq!(result.reason, CompactionReason::DeferredEconomic);
}

#[test]
fn subsequent_margin_has_precedence_over_carried_debt() {
    let mut input = decision_input();
    input.prior_compaction_count = 1;
    input.write_tokens = 20.0;
    input.archive_tokens = 2.0;
    input.memo_tokens = 1.0;
    input.cache_write_read_ratio = Some(2.0);
    input.carried_debt_tokens = 100.0;
    let result = decide_compaction(&input);
    assert!(!result.compact);
    assert_eq!(result.reason, CompactionReason::DeferredSubsequentMargin);
    input.economics.subsequent_compaction_margin = 1.0;
    assert_eq!(
        decide_compaction(&input).reason,
        CompactionReason::DeferredCarriedDebt
    );
    input.carried_debt_tokens = 0.0;
    assert!(decide_compaction(&input).compact);
}

#[test]
fn cache_ratios_below_one_have_no_incremental_cache_cost() {
    let mut input = decision_input();
    input.cache_write_read_ratio = Some(0.5);
    let result = decide_compaction(&input);
    assert_eq!(result.incremental_cache_cost_ratio, Some(0.0));
    assert_eq!(result.breakeven_requests, Some(0.0));
    assert!(result.compact);
}

#[test]
fn window_protection_opens_at_exact_reserve_threshold() {
    let mut input = decision_input();
    input.cache_write_read_ratio = None;
    input.context_tokens = 200_000.0 - input.economics.window_reserve_tokens;
    assert_eq!(
        decide_compaction(&input).reason,
        CompactionReason::WindowProtection
    );
    input.context_tokens -= 1.0;
    assert_eq!(
        decide_compaction(&input).reason,
        CompactionReason::CacheRatioUnavailable
    );
}

#[test]
fn preserves_javascript_nan_propagation_in_window_bound_and_cache_cost() {
    let mut input = horizon_input();
    input.average_context_token_increment = Some(f64::NAN);
    let result = estimate_remaining_requests(&input);
    assert!(result.window_request_upper_bound.unwrap().is_nan());
    assert!(result.expected_remaining_requests.is_nan());
    let mut input = decision_input();
    input.cache_write_read_ratio = Some(f64::NAN);
    let result = decide_compaction(&input);
    assert!(result.incremental_cache_cost_ratio.unwrap().is_nan());
    assert!(result.breakeven_requests.unwrap().is_nan());
    assert!(!result.compact);
    assert_eq!(result.reason, CompactionReason::DeferredEconomic);
}

#[test]
fn preserves_positive_zero_for_empty_mean_and_zero_window_capacity() {
    let mut input = horizon_input();
    input.completed_boundary_request_counts = &[];
    input.context_tokens = input.context_window_tokens.unwrap();
    let result = estimate_remaining_requests(&input);
    assert!(result.requests_per_boundary_mean.is_sign_positive());
    assert!(result
        .window_request_upper_bound
        .unwrap()
        .is_sign_positive());
}

#[test]
fn retains_reason_strings_and_default_economics() {
    assert_eq!(CompactionEconomics::default(), DEFAULT_COMPACTION_ECONOMICS);
    assert_eq!(
        CompactionReason::DeferredCarriedDebt.as_str(),
        "deferred_carried_debt"
    );
    assert_eq!(
        CompactionReason::NativeNotCompactable.as_str(),
        "native_not_compactable"
    );
}

#[test]
fn default_request_hooks_filter_custom_messages_without_mutating_transcript() {
    let context = Context {
        system_prompt: "system".into(),
        messages: vec![
            Message::Custom {
                kind: "notification".into(),
                data: Value::Null,
                timestamp: 0,
            },
            Message::user("hello"),
        ],
    };
    let request = build_model_request(&context, &(), None, "faux", Some("configured"));
    assert_eq!(request.context.system_prompt, "system");
    assert_eq!(request.context.messages.len(), 1);
    assert_eq!(request.context.messages[0].text(), "hello");
    assert_eq!(request.api_key.as_deref(), Some("configured"));
    assert_eq!(context.messages.len(), 2);
}

struct RecordingHooks {
    calls: RefCell<Vec<&'static str>>,
    key_calls: Cell<usize>,
}

impl RequestHooks for RecordingHooks {
    fn transform_context(
        &self,
        messages: &[Message],
        signal: Option<&AbortSignal>,
    ) -> Vec<Message> {
        self.calls.borrow_mut().push("transform");
        assert!(signal.unwrap().is_aborted());
        messages[messages.len() - 1..].to_vec()
    }

    fn convert_to_llm(&self, messages: &[Message]) -> Vec<Message> {
        self.calls.borrow_mut().push("convert");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text(), "latest");
        vec![Message::user("converted")]
    }

    fn get_api_key(&self, provider: &str) -> Option<String> {
        self.calls.borrow_mut().push("key");
        assert_eq!(provider, "faux");
        self.key_calls.set(self.key_calls.get() + 1);
        Some(format!("fresh-{}", self.key_calls.get()))
    }
}

#[test]
fn request_pipeline_transforms_then_converts_and_refreshes_key_every_request() {
    let context = Context {
        system_prompt: "system".into(),
        messages: vec![Message::user("old"), Message::user("latest")],
    };
    let hooks = RecordingHooks {
        calls: RefCell::new(vec![]),
        key_calls: Cell::new(0),
    };
    let signal = AbortSignal::new();
    signal.cancel();
    let request = build_model_request(&context, &hooks, Some(&signal), "faux", Some("configured"));
    assert_eq!(*hooks.calls.borrow(), ["transform", "convert", "key"]);
    assert_eq!(request.context.messages[0].text(), "converted");
    assert_eq!(request.api_key.as_deref(), Some("fresh-1"));
    let request = build_model_request(&context, &hooks, Some(&signal), "faux", Some("configured"));
    assert_eq!(request.api_key.as_deref(), Some("fresh-2"));
}

struct EmptyKeyHooks;

impl RequestHooks for EmptyKeyHooks {
    fn get_api_key(&self, _provider: &str) -> Option<String> {
        Some(String::new())
    }
}

#[test]
fn empty_resolved_key_falls_back_but_empty_configured_key_is_preserved() {
    let context = Context::default();
    assert_eq!(
        build_model_request(&context, &EmptyKeyHooks, None, "faux", Some("configured")).api_key,
        Some("configured".into())
    );
    assert_eq!(
        build_model_request(&context, &EmptyKeyHooks, None, "faux", Some("")).api_key,
        Some(String::new())
    );
    assert_eq!(
        build_model_request(&context, &EmptyKeyHooks, None, "faux", None).api_key,
        None
    );
}
