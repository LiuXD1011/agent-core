use agent_core::types::{CancellationToken, Message, ThinkingLevel};
use agent_runtime::extensions::{
    Extension, ExtensionContext, ExtensionMode, LifecycleEvent, SharedRegistry,
};
use agent_runtime::provider::{ProviderMode, ProviderSettings, ThinkingCapabilities};
use agent_runtime::session::{Session, SessionStore};
use agent_runtime::thinking::*;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[test]
fn all_levels_have_exact_wire_names_and_invalid_levels_do_not_parse() {
    for level in ThinkingLevel::ALL {
        let text = level.as_str();
        assert_eq!(text.parse::<ThinkingLevel>().unwrap(), level);
        assert_eq!(serde_json::to_value(level).unwrap(), json!(text));
    }
    for value in ["HIGH", "ultra", " high", ""] {
        assert!(value.parse::<ThinkingLevel>().is_err());
    }
}
#[test]
fn branch_level_is_append_only_deduplicated_and_restored_without_messages() {
    let mut session = Session::default();
    let first = session.append_message(Message::user("before")).unwrap();
    let high = session
        .append_thinking_level(ThinkingLevel::High)
        .unwrap()
        .unwrap();
    assert!(session
        .append_thinking_level(ThinkingLevel::High)
        .unwrap()
        .is_none());
    session.navigate(Some(&first)).unwrap();
    assert_eq!(session.get_thinking_level().unwrap(), None);
    session.append_thinking_level(ThinkingLevel::Low).unwrap();
    assert_eq!(
        session.get_thinking_level().unwrap(),
        Some(ThinkingLevel::Low)
    );
    session.navigate(Some(&high)).unwrap();
    assert_eq!(
        session.get_thinking_level().unwrap(),
        Some(ThinkingLevel::High)
    );
    assert_eq!(session.context.messages.len(), 1);
    let dir = std::env::temp_dir().join(format!("thinking-session-{}", session.id));
    let store = SessionStore::new(&dir);
    store.save(&session).unwrap();
    let restored = store.list().unwrap().remove(0);
    assert_eq!(
        restored.get_thinking_level().unwrap(),
        Some(ThinkingLevel::High)
    );
    assert_eq!(
        restored
            .clone_current_branch()
            .unwrap()
            .get_thinking_level()
            .unwrap(),
        Some(ThinkingLevel::High)
    );
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn clamp_uses_catalog_rules_and_explicit_custom_capabilities_never_credentials() {
    let session = Session::default();
    let mut settings = ProviderSettings {
        mode: ProviderMode::OpenAiCompatible,
        model: "unknown".into(),
        ..Default::default()
    };
    assert_eq!(
        prepare_thinking_selection(
            &session,
            &settings,
            ThinkingLevel::High,
            &CancellationToken::new()
        )
        .unwrap()
        .level,
        ThinkingLevel::Off
    );
    settings.options.thinking_capabilities = Some(ThinkingCapabilities {
        reasoning: true,
        max_tokens: 4096,
        context_window: 32768,
        name: "custom".into(),
        thinking_level_map: BTreeMap::from([
            ("off".into(), None),
            ("minimal".into(), None),
            ("low".into(), None),
            ("medium".into(), None),
            ("high".into(), Some("high".into())),
        ]),
        compat: json!({}),
    });
    let selected = prepare_thinking_selection(
        &session,
        &settings,
        ThinkingLevel::Off,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(selected.level, ThinkingLevel::High);
    assert_eq!(
        selected.session.get_thinking_level().unwrap(),
        Some(ThinkingLevel::High)
    );
    assert_eq!(
        selected.settings.options.thinking_level,
        Some(ThinkingLevel::High)
    );
    assert!(selected.session.context.messages.is_empty());
    let selected = prepare_thinking_selection(
        &session,
        &settings,
        ThinkingLevel::Max,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(selected.level, ThinkingLevel::High);
}
#[test]
fn restore_no_entry_preserves_original_default_and_cancelled_selection_is_atomic() {
    let session = Session::default();
    let mut settings = ProviderSettings::default();
    restore_thinking_settings(&session, &mut settings).unwrap();
    assert_eq!(settings.options.thinking_level, None);
    let token = CancellationToken::new();
    token.cancel();
    let before = serde_json::to_value(&session).unwrap();
    assert!(prepare_thinking_selection(&session, &settings, ThinkingLevel::High, &token).is_err());
    assert_eq!(serde_json::to_value(&session).unwrap(), before);
}
struct Observer(Arc<Mutex<Vec<(LifecycleEvent, ExtensionContext)>>>);
impl Extension for Observer {
    fn id(&self) -> &str {
        "thinking-observer"
    }
    fn on_event(
        &self,
        event: &LifecycleEvent,
        context: &ExtensionContext,
        _: &CancellationToken,
    ) -> Result<(), String> {
        if matches!(event, LifecycleEvent::ThinkingLevelSelect { .. }) {
            self.0
                .lock()
                .unwrap()
                .push((event.clone(), context.clone()));
            return Err("ordinary observation error".into());
        }
        Ok(())
    }
}
#[test]
fn thinking_post_commit_observation_has_actual_context_and_error_is_diagnostic() {
    let registry = SharedRegistry::default();
    let trace = Arc::new(Mutex::new(vec![]));
    registry
        .register(Arc::new(Observer(trace.clone())))
        .unwrap();
    let mut session = Session::default();
    session.append_thinking_level(ThinkingLevel::High).unwrap();
    notify_thinking_level_select(
        &session,
        &registry,
        ExtensionMode::Desktop,
        None,
        ThinkingLevel::High,
        ThinkingLevel::Off,
        &CancellationToken::new(),
    )
    .unwrap();
    notify_thinking_level_select(
        &session,
        &registry,
        ExtensionMode::Desktop,
        None,
        ThinkingLevel::High,
        ThinkingLevel::High,
        &CancellationToken::new(),
    )
    .unwrap();
    let trace = trace.lock().unwrap();
    assert_eq!(trace.len(), 1);
    assert_eq!(trace[0].1.thinking_level, ThinkingLevel::High);
    assert_eq!(
        serde_json::to_value(&trace[0].0).unwrap(),
        json!({"type":"thinking_level_select","level":"high","previousLevel":"off"})
    );
    assert_eq!(
        registry.drain_diagnostics()[0].event,
        "thinking_level_select"
    );
}
