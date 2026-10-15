use agent_core::types::CancellationToken;
use agent_runtime::{auth_storage::AuthStorage, connections::*, provider::ProviderMode};
use serde_json::json;

struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "connections-{}",
            agent_runtime::session::Session::default().id
        )))
    }
    fn storage(&self) -> AuthStorage {
        AuthStorage::new(self.0.join("credentials.dat"))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn presets_keep_api_accounts_and_coding_credentials_separate() {
    assert_eq!(Service::ALL.iter().filter(|s| s.browser_login()).count(), 1);
    assert_ne!(Service::Zhipu.id(), Service::ZhipuCoding.id());
    assert_ne!(Service::OpenAi.id(), Service::Codex.id());
    for service in Service::ALL {
        let settings = service.settings("new-model", std::path::Path::new("isolated.dat"));
        assert_eq!(Service::from_id(settings.provider_id()), Some(service));
        assert!(settings.api_key.is_empty());
        assert!(settings.options.oauth.is_none());
        assert!(settings.options.headers.is_empty());
        assert!(!settings.options.api_key_explicit);
        assert_eq!(settings.base_url, service.base_url());
    }
    assert_eq!(Service::OpenAi.mode(), ProviderMode::OpenAiResponses);
    assert_eq!(Service::Codex.mode(), ProviderMode::OpenAiCodex);
}

#[test]
fn storage_roundtrip_retains_models_metadata_and_isolates_credentials() {
    let fixture = Fixture::new();
    let storage = fixture.storage();
    let signal = CancellationToken::new();
    for service in [
        Service::DeepSeek,
        Service::Zhipu,
        Service::ZhipuCoding,
        Service::OpenAi,
    ] {
        save_key(
            &storage,
            service,
            &format!("fixture-{}", service.id()),
            &signal,
        )
        .unwrap();
        let mut connection = Connection::bundled(service);
        connection.selected_model = "fresh-model".into();
        save_connection(&storage, &connection, &signal).unwrap();
        assert_eq!(
            load_connection(&storage, service, &signal).unwrap(),
            connection
        );
        assert!(has_credential(&storage, service, &signal).unwrap());
        assert_eq!(
            storage
                .resolve_provider(service.id(), None, &signal)
                .unwrap()
                .unwrap()
                .0,
            format!("fixture-{}", service.id())
        );
    }
    assert!(!has_credential(&storage, Service::Codex, &signal).unwrap());
    #[cfg(windows)]
    assert!(!String::from_utf8_lossy(&std::fs::read(storage.path()).unwrap()).contains("fixture-"));
}

#[test]
fn fresh_model_ids_preserve_selection_and_unknown_capabilities() {
    let mut previous = Connection::bundled(Service::DeepSeek);
    previous.selected_model = "old-model".into();
    let next = parse_models(&previous, &json!({"data":[{"id":"new-model","future":{"v":1}},{"id":"detailed","context_window":123456,"max_output_tokens":8192}]}), false).unwrap();
    assert_eq!(next.selected_model, "old-model");
    assert_eq!(next.budget("new-model"), (0, 0));
    assert_eq!(next.budget("detailed"), (123456, 8192));
    assert_eq!(next.models[1].metadata["future"]["v"], 1);
    assert_eq!(next.source, ModelSource::ProviderApi);
    assert!(next.refreshed_at.is_some());
    assert!(parse_models(&previous, &json!({"error":"secret"}), false).is_err());
    assert!(parse_models(
        &previous,
        &json!({"data":[{"id":"same"},{"id":"same"}]}),
        false
    )
    .is_err());
    assert_eq!(previous.selected_model, "old-model");
}

#[test]
fn public_catalog_is_route_scoped_and_never_changes_endpoint() {
    let catalog = json!({"zhipuai":{"api":"https://attacker.invalid","models":{"glm-new":{"limit":{"context":32768,"output":4096},"modalities":{"output":["text"]},"future":"keep"},"image-only":{"modalities":{"output":["image"]}}}},"zhipuai-coding-plan":{"models":{"coding-only":{}}}});
    let standard = parse_models(&Connection::bundled(Service::Zhipu), &catalog, true).unwrap();
    let coding = parse_models(&Connection::bundled(Service::ZhipuCoding), &catalog, true).unwrap();
    assert_eq!(standard.models.len(), 1);
    assert_eq!(standard.models[0].id, "glm-new");
    assert_eq!(standard.budget("glm-new"), (32768, 4096));
    assert_eq!(standard.models[0].metadata["future"], "keep");
    assert_eq!(coding.models[0].id, "coding-only");
    assert_eq!(
        standard.service.base_url(),
        "https://open.bigmodel.cn/api/paas/v4"
    );
}

#[test]
fn invalid_or_cancelled_keys_never_replace_saved_credentials() {
    let fixture = Fixture::new();
    let storage = fixture.storage();
    let signal = CancellationToken::new();
    save_key(&storage, Service::DeepSeek, "fixture-original", &signal).unwrap();
    for key in ["", "foo\nbar", "with space", "中文"] {
        assert!(save_key(&storage, Service::DeepSeek, key, &signal).is_err());
    }
    assert_eq!(
        save_key(&storage, Service::Codex, "fixture-key", &signal),
        Err(ConnectionError::AccountLogin)
    );
    signal.cancel();
    assert_eq!(
        save_key(&storage, Service::DeepSeek, "replacement", &signal),
        Err(ConnectionError::Cancelled)
    );
    assert_eq!(
        storage
            .resolve_provider("deepseek", None, &CancellationToken::new())
            .unwrap()
            .unwrap()
            .0,
        "fixture-original"
    );
}

#[test]
fn codex_reload_is_explicitly_local_and_requires_no_network_or_credentials() {
    let fixture = Fixture::new();
    let mut previous = Connection::bundled(Service::Codex);
    previous.selected_model = "manual-model".into();
    let refreshed = refresh(&fixture.storage(), &previous, &CancellationToken::new()).unwrap();
    assert_eq!(refreshed.source, ModelSource::Bundled);
    assert_eq!(refreshed.refreshed_at, None);
    assert_eq!(refreshed.selected_model, "manual-model");
    assert!(!fixture.0.exists());
}
