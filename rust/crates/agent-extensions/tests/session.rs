use agent_core::types::CancellationToken;
use agent_extensions::*;
use serde_json::json;
use std::sync::{Arc, Mutex};

struct Observer {
    id: &'static str,
    trace: Arc<Mutex<Vec<String>>>,
    behavior: &'static str,
}
impl Extension for Observer {
    fn id(&self) -> &str {
        self.id
    }
    fn before_session_change(
        &self,
        event: &SessionBeforeEvent,
        _: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<SessionBeforeResult, String> {
        self.trace
            .lock()
            .unwrap()
            .push(format!("{}:{}", self.id, event.name()));
        match self.behavior {
            "error" => Err("ordinary session callback error".into()),
            "panic" => panic!("ordinary session callback panic"),
            "cancel-token" => {
                signal.cancel();
                Ok(Default::default())
            }
            _ => Ok(SessionBeforeResult {
                cancel: self.behavior == "veto",
            }),
        }
    }
    fn on_event(
        &self,
        event: &LifecycleEvent,
        context: &ExtensionContext,
        _: &CancellationToken,
    ) -> Result<(), String> {
        if let LifecycleEvent::SessionInfoChanged { name } = event {
            assert_eq!(&context.session_name, name);
            self.trace
                .lock()
                .unwrap()
                .push(format!("{}:{name:?}", self.id));
            if self.behavior == "error" {
                return Err("ordinary name observation error".into());
            }
        }
        Ok(())
    }
}
fn registered(
    behaviors: &[(&'static str, &'static str)],
) -> (SharedRegistry, Arc<Mutex<Vec<String>>>) {
    let registry = SharedRegistry::default();
    let trace = Arc::new(Mutex::new(vec![]));
    for &(id, behavior) in behaviors {
        registry
            .register(Arc::new(Observer {
                id,
                behavior,
                trace: trace.clone(),
            }))
            .unwrap();
    }
    (registry, trace)
}
fn event() -> SessionBeforeEvent {
    SessionBeforeEvent::Switch {
        reason: SessionSwitchReason::New,
        target_session_file: None,
    }
}

#[test]
fn before_session_veto_is_ordered_short_circuit_and_errors_are_diagnostic() {
    let (registry, trace) = registered(&[
        ("error", "error"),
        ("panic", "panic"),
        ("veto", "veto"),
        ("later", "continue"),
    ]);
    assert!(
        registry
            .emit_session_before(
                &event(),
                &ExtensionContext::default(),
                &CancellationToken::new()
            )
            .unwrap()
            .cancel
    );
    assert_eq!(trace.lock().unwrap().len(), 3);
    assert_eq!(
        registry
            .drain_diagnostics()
            .iter()
            .map(|d| d.event.as_str())
            .collect::<Vec<_>>(),
        vec!["session_before_switch", "session_before_switch"]
    );
}
#[test]
fn before_session_cancelled_token_prevents_commit_and_later_observation() {
    let (registry, trace) = registered(&[("first", "cancel-token"), ("later", "continue")]);
    assert!(registry
        .emit_session_before(
            &event(),
            &ExtensionContext::default(),
            &CancellationToken::new()
        )
        .is_err());
    assert_eq!(trace.lock().unwrap().len(), 1);
    let signal = CancellationToken::new();
    signal.cancel();
    assert!(registry
        .emit_session_before(&event(), &ExtensionContext::default(), &signal)
        .is_err());
    assert_eq!(trace.lock().unwrap().len(), 1);
}
#[test]
fn session_info_observation_accepts_clear_and_ordinary_error_does_not_veto() {
    let (registry, trace) = registered(&[("error", "error"), ("later", "continue")]);
    for name in [Some("selected".to_owned()), None] {
        registry
            .emit_event(
                &LifecycleEvent::SessionInfoChanged { name: name.clone() },
                &ExtensionContext {
                    session_name: name,
                    ..Default::default()
                },
                &CancellationToken::new(),
            )
            .unwrap();
    }
    assert_eq!(trace.lock().unwrap().len(), 4);
    assert!(registry
        .drain_diagnostics()
        .iter()
        .all(|d| d.event == "session_info_changed"));
}
#[test]
fn native_session_event_shapes_preserve_observed_names_and_fork_position() {
    assert_eq!(
        serde_json::to_value(SessionBeforeEvent::Switch {
            reason: SessionSwitchReason::Resume,
            target_session_file: Some("old.json".into())
        })
        .unwrap(),
        json!({"type":"session_before_switch","reason":"resume","targetSessionFile":"old.json"})
    );
    assert_eq!(
        serde_json::to_value(SessionBeforeEvent::Fork {
            entry_id: "entry".into(),
            position: ForkPosition::At
        })
        .unwrap(),
        json!({"type":"session_before_fork","entryId":"entry","position":"at"})
    );
    assert_eq!(
        serde_json::to_value(LifecycleEvent::SessionInfoChanged { name: None }).unwrap(),
        json!({"type":"session_info_changed","name":null})
    );
    assert!(serde_json::from_value::<SessionBeforeResult>(json!({"cancel":"true"})).is_err());
    assert!(
        serde_json::from_value::<SessionBeforeResult>(json!({"cancel":true,"ignored":1})).is_err()
    );
}
#[test]
fn session_name_normalization_preserves_internal_space_and_empty_clear() {
    // Original session-info regressions #3686 and #5996.
    assert_eq!(
        normalize_session_name("\u{feff}  alpha\r\n\nbeta  gamma\n  "),
        "alpha beta  gamma"
    );
    assert_eq!(normalize_session_name(" \r\n\u{00a0}\u{feff}"), "");
    assert_eq!(normalize_session_name("a\u{0085}b"), "a\u{0085}b");
}
