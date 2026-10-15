use agent_core::types::CancellationToken;
use agent_extensions::{
    Extension, ExtensionContext, LifecycleEvent, ModelIdentity, ModelSelectSource, SharedRegistry,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

fn identity(provider: &str, model: &str) -> ModelIdentity {
    ModelIdentity {
        provider: provider.into(),
        model: model.into(),
    }
}
fn event() -> LifecycleEvent {
    LifecycleEvent::ModelSelect {
        model: identity("fixture", "next"),
        previous_model: Some(identity("fixture", "previous")),
        source: ModelSelectSource::Set,
    }
}
#[derive(Clone, Copy)]
enum Behavior {
    Observe,
    Error,
    Panic,
    Cancel,
}
struct Observer {
    id: &'static str,
    behavior: Behavior,
    seen: Arc<Mutex<Vec<(String, LifecycleEvent, ExtensionContext)>>>,
}
impl Extension for Observer {
    fn id(&self) -> &str {
        self.id
    }
    fn on_event(
        &self,
        event: &LifecycleEvent,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        self.seen
            .lock()
            .unwrap()
            .push((self.id.into(), event.clone(), context.clone()));
        match self.behavior {
            Behavior::Observe => Ok(()),
            Behavior::Error => Err("ordinary model notification failed".into()),
            Behavior::Panic => panic!("offline model notification panic"),
            Behavior::Cancel => {
                signal.cancel();
                Ok(())
            }
        }
    }
}

#[test]
fn model_select_serialization_uses_the_native_camel_case_event_shape() {
    assert_eq!(
        serde_json::to_value(event()).unwrap(),
        json!({
            "type":"model_select","model":{"provider":"fixture","model":"next"},
            "previousModel":{"provider":"fixture","model":"previous"},"source":"set"
        })
    );
    let restore = LifecycleEvent::ModelSelect {
        model: identity("demo", ""),
        previous_model: None,
        source: ModelSelectSource::Restore,
    };
    assert_eq!(
        serde_json::to_value(&restore).unwrap(),
        json!({
            "type":"model_select","model":{"provider":"demo","model":""},
            "previousModel":null,"source":"restore"
        })
    );
    assert_eq!(
        serde_json::from_value::<ModelSelectSource>(json!("restore")).unwrap(),
        ModelSelectSource::Restore
    );
    assert!(serde_json::from_value::<ModelSelectSource>(json!("unknown")).is_err());
    assert!(!restore.is_terminal());
}

#[test]
fn ordinary_errors_and_panics_are_diagnostic_and_do_not_veto_later_model_observers() {
    let registry = SharedRegistry::default();
    let seen = Arc::new(Mutex::new(vec![]));
    for (id, behavior) in [
        ("error", Behavior::Error),
        ("panic", Behavior::Panic),
        ("last", Behavior::Observe),
    ] {
        registry
            .register(Arc::new(Observer {
                id,
                behavior,
                seen: seen.clone(),
            }))
            .unwrap();
    }
    let context = ExtensionContext {
        model: Some(identity("fixture", "next")),
        ..Default::default()
    };
    let before = context.clone();
    registry
        .emit_event(&event(), &context, &CancellationToken::new())
        .unwrap();
    assert_eq!(context, before);
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.iter()
            .map(|(id, _, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["error", "panic", "last"]
    );
    assert!(seen
        .iter()
        .all(|(_, observed, snapshot)| observed == &event() && snapshot.model == context.model));
    assert!(seen
        .iter()
        .all(|(_, _, snapshot)| snapshot.context.messages.is_empty()));
    let diagnostics = registry.drain_diagnostics();
    assert_eq!(diagnostics.len(), 2);
    assert!(diagnostics
        .iter()
        .all(|diagnostic| diagnostic.event == "model_select"));
}

#[test]
fn cancelled_model_notification_does_not_invoke_any_observer() {
    let registry = SharedRegistry::default();
    let seen = Arc::new(Mutex::new(vec![]));
    registry
        .register(Arc::new(Observer {
            id: "observe",
            behavior: Behavior::Observe,
            seen: seen.clone(),
        }))
        .unwrap();
    let signal = CancellationToken::new();
    signal.cancel();
    assert!(registry
        .emit_event(&event(), &ExtensionContext::default(), &signal)
        .unwrap_err()
        .contains("aborted"));
    assert!(seen.lock().unwrap().is_empty());
    assert!(registry.drain_diagnostics().is_empty());
}

#[test]
fn cancellation_during_model_notification_stops_the_remaining_chain() {
    let registry = SharedRegistry::default();
    let seen = Arc::new(Mutex::new(vec![]));
    for (id, behavior) in [("cancel", Behavior::Cancel), ("later", Behavior::Observe)] {
        registry
            .register(Arc::new(Observer {
                id,
                behavior,
                seen: seen.clone(),
            }))
            .unwrap();
    }
    let signal = CancellationToken::new();
    assert!(registry
        .emit_event(&event(), &ExtensionContext::default(), &signal)
        .is_err());
    assert_eq!(
        seen.lock()
            .unwrap()
            .iter()
            .map(|(id, _, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["cancel"]
    );
    assert!(registry.drain_diagnostics().is_empty());
}
