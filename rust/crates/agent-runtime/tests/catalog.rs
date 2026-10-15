use agent_runtime::catalog::{
    find, find_canonical, models, providers, search, validate_snapshot, SNAPSHOT_JSON,
};
use agent_runtime::provider::ProviderMode;
use serde_json::{json, Value};

#[test]
fn embedded_catalog_preserves_all_oracle_models_and_provider_keys() {
    assert_eq!(models().len(), 1408);
    assert_eq!(providers().len(), 39);
    assert_eq!(validate_snapshot(SNAPSHOT_JSON).unwrap(), 1408);
    let snapshot: Value = serde_json::from_str(SNAPSHOT_JSON).unwrap();
    assert_eq!(
        snapshot["source"]["sha256"],
        "d37f49ca5a7dc0f2981e8c52473359ed7c11192c24cc54573bf83e3a0a17a149"
    );
    for model in models() {
        assert_eq!(
            model.metadata,
            snapshot["providers"][&model.provider][&model.id]
        );
        assert_eq!(model.cost, model.metadata["cost"]);
        assert_eq!(
            model.context_window,
            model.metadata["contextWindow"].as_u64().unwrap()
        );
        assert_eq!(
            model.max_tokens,
            model.metadata["maxTokens"].as_u64().unwrap()
        );
    }
}

#[test]
fn model_lookup_is_exact_and_canonical_ids_preserve_slashes() {
    let model = find("anthropic", "claude-sonnet-4-6").unwrap();
    assert_eq!(find_canonical(&model.canonical_id()), Some(model));
    assert!(find("Anthropic", "claude-sonnet-4-6").is_none());
    assert!(find("anthropic", "CLAUDE-SONNET-4-6").is_none());
    assert!(find_canonical("anthropic").is_none());
    let slash_model = models()
        .iter()
        .find(|model| model.id.contains('/'))
        .unwrap();
    assert_eq!(
        find_canonical(&slash_model.canonical_id()),
        Some(slash_model)
    );
}

#[test]
fn native_protocol_markers_only_enable_migrated_routes() {
    assert_eq!(
        find("anthropic", "claude-sonnet-4-6").unwrap().mode(),
        Some(ProviderMode::Anthropic)
    );
    assert_eq!(
        find("google", "gemini-2.5-flash").unwrap().mode(),
        Some(ProviderMode::Google)
    );
    for provider in [
        "github-copilot",
        "cloudflare-workers-ai",
        "cloudflare-ai-gateway",
    ] {
        let entries: Vec<_> = models()
            .iter()
            .filter(|model| model.provider == provider)
            .collect();
        assert!(!entries.is_empty());
        assert!(entries.iter().all(|model| model.is_supported()
            && model.mode().is_some()
            && model.unsupported_reason().is_none()));
    }
    for model in models()
        .iter()
        .filter(|model| matches!(model.api.as_str(), "mistral-conversations"))
    {
        assert_eq!(model.mode(), Some(ProviderMode::Mistral));
    }
    assert!(models()
        .iter()
        .filter(|model| model.provider == "openai" && model.api == "openai-responses")
        .all(|model| model.mode() == Some(ProviderMode::OpenAiResponses)));
    assert!(models()
        .iter()
        .filter(|model| model.provider == "openai-codex")
        .all(|model| model.mode() == Some(ProviderMode::OpenAiCodex)));
    for (provider, mode) in [
        ("google-vertex", ProviderMode::GoogleVertex),
        ("amazon-bedrock", ProviderMode::AmazonBedrock),
        ("azure-openai-responses", ProviderMode::AzureOpenAiResponses),
    ] {
        let entries: Vec<_> = models()
            .iter()
            .filter(|model| model.provider == provider)
            .collect();
        assert!(!entries.is_empty());
        assert!(entries
            .iter()
            .all(|model| model.mode() == Some(mode.clone())));
    }
}

#[test]
fn metadata_includes_compatibility_thinking_maps_and_costs_without_loss() {
    let model = find("anthropic", "claude-fable-5").unwrap();
    assert_eq!(model.metadata["thinkingLevelMap"]["off"], Value::Null);
    assert_eq!(model.metadata["thinkingLevelMap"]["xhigh"], "xhigh");
    assert_eq!(model.metadata["compat"]["supportsStrictTools"], true);
    assert_eq!(
        model.metadata["compat"]["allowedFallbackModels"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(model.cost["input"], 10);
    assert_eq!(model.cost["cacheWrite"], 12.5);
    assert_eq!(model.context_window, 1_000_000);
    assert!(model.input.iter().any(|input| input == "image"));
}

#[test]
fn search_is_case_insensitive_and_keeps_every_empty_query_result() {
    assert_eq!(search("  ").len(), models().len());
    let results = search("CLAUDE-SONNET-4-6");
    assert!(results.iter().any(|model| model.provider == "anthropic"));
    assert!(search("google-generative-ai")
        .iter()
        .all(|model| model.api == "google-generative-ai"));
    assert!(search("no-such-fixture-model-123456789").is_empty());
}

#[test]
fn snapshot_validation_rejects_identity_schema_and_field_mismatches() {
    let model = find("anthropic", "claude-sonnet-4-6").unwrap();
    let original =
        json!({"schemaVersion":1,"providers":{"anthropic":{"claude-sonnet-4-6":model.metadata}}});
    assert_eq!(validate_snapshot(&original.to_string()).unwrap(), 1);
    for (field, value) in [
        ("id", json!("different")),
        ("provider", json!("different")),
        ("contextWindow", json!(0)),
        ("maxTokens", json!(-1)),
        ("cost", json!([])),
        ("input", json!([1])),
        ("reasoning", json!("yes")),
    ] {
        let mut invalid = original.clone();
        invalid["providers"]["anthropic"]["claude-sonnet-4-6"][field] = value;
        assert!(validate_snapshot(&invalid.to_string()).is_err());
    }
    assert!(validate_snapshot("{}").is_err());
    assert!(validate_snapshot("invalid JSON").is_err());
    assert!(validate_snapshot("{\"schemaVersion\":1,\"providers\":{}}").is_err());
}

#[test]
fn index_storage_is_initialized_once_and_supports_concurrent_readers() {
    let address = models().as_ptr() as usize;
    let handles: Vec<_> = (0..4)
        .map(|_| std::thread::spawn(|| models().as_ptr() as usize))
        .collect();
    for handle in handles {
        assert_eq!(handle.join().unwrap(), address);
    }
}
