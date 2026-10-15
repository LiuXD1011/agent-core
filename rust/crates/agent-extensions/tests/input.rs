use agent_core::types::{CancellationToken, Content};
use agent_extensions::{
    validate_input, Extension, ExtensionContext, InputEvent, InputResult, InputSource,
    InputStreamingBehavior, SharedRegistry,
};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

type Callback = Arc<
    dyn Fn(&InputEvent, &ExtensionContext, &CancellationToken) -> Result<InputResult, String>
        + Send
        + Sync,
>;
struct Handler {
    id: String,
    callback: Callback,
}
impl Extension for Handler {
    fn id(&self) -> &str {
        &self.id
    }
    fn input(
        &self,
        event: &InputEvent,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<InputResult, String> {
        (self.callback)(event, context, signal)
    }
}
fn register(
    registry: &SharedRegistry,
    id: &str,
    callback: impl Fn(&InputEvent, &ExtensionContext, &CancellationToken) -> Result<InputResult, String>
        + Send
        + Sync
        + 'static,
) {
    registry
        .register(Arc::new(Handler {
            id: id.into(),
            callback: Arc::new(callback),
        }))
        .unwrap();
}
fn event() -> InputEvent {
    InputEvent {
        text: "raw".into(),
        images: vec![image("original")],
        ..Default::default()
    }
}
fn image(data: &str) -> Content {
    Content::Image {
        data: data.into(),
        mime_type: "image/png".into(),
    }
}
fn emit(registry: &SharedRegistry, event: &InputEvent) -> InputResult {
    registry
        .emit_input(
            event,
            &ExtensionContext::default(),
            &CancellationToken::new(),
        )
        .unwrap()
}

// Mirrors the observed TS input-event/streaming regression cases without JS IO.
#[test]
fn no_handlers_default_trait_and_explicit_continue_leave_input_unchanged() {
    struct DefaultExtension;
    impl Extension for DefaultExtension {
        fn id(&self) -> &str {
            "default"
        }
    }
    let registry = SharedRegistry::default();
    assert_eq!(emit(&registry, &event()), InputResult::Continue);
    registry.register(Arc::new(DefaultExtension)).unwrap();
    register(&registry, "explicit", |_, _, _| Ok(InputResult::Continue));
    assert_eq!(emit(&registry, &event()), InputResult::Continue);
}

#[test]
fn transformations_chain_in_registration_order_and_preserve_omitted_images() {
    let registry = SharedRegistry::default();
    let seen = Arc::new(Mutex::new(vec![]));
    for (index, suffix) in ["[1]", "[2]"].into_iter().enumerate() {
        let observed = seen.clone();
        register(&registry, &format!("handler{index}"), move |event, _, _| {
            observed.lock().unwrap().push(event.clone());
            Ok(InputResult::Transform {
                text: format!("{}{suffix}", event.text),
                images: None,
            })
        });
    }
    assert_eq!(
        emit(&registry, &event()),
        InputResult::Transform {
            text: "raw[1][2]".into(),
            images: Some(vec![image("original")])
        }
    );
    let observed = seen.lock().unwrap();
    assert_eq!(observed[0].text, "raw");
    assert_eq!(observed[1].text, "raw[1]");
    assert_eq!(observed[1].images, observed[0].images);
}

#[test]
fn images_can_be_replaced_and_explicitly_cleared_but_null_preserves() {
    for patch in [None, Some(vec![image("replaced")]), Some(vec![])] {
        let registry = SharedRegistry::default();
        let expected = patch.clone().unwrap_or_else(|| event().images);
        register(&registry, "images", move |_, _, _| {
            Ok(InputResult::Transform {
                text: "changed".into(),
                images: patch.clone(),
            })
        });
        assert_eq!(
            emit(&registry, &event()),
            InputResult::Transform {
                text: "changed".into(),
                images: Some(expected)
            }
        );
    }
    let parsed: InputResult =
        serde_json::from_value(json!({"action":"transform","text":"changed","images":null}))
            .unwrap();
    assert!(matches!(
        parsed,
        InputResult::Transform { images: None, .. }
    ));
    let clear: InputResult =
        serde_json::from_value(json!({"action":"transform","text":"changed","images":[]})).unwrap();
    assert!(matches!(clear,InputResult::Transform{images:Some(images),..} if images.is_empty()));
}

#[test]
fn handled_after_a_transform_short_circuits_all_later_handlers() {
    let registry = SharedRegistry::default();
    let calls = Arc::new(AtomicUsize::new(0));
    register(&registry, "first", |_, _, _| {
        Ok(InputResult::Transform {
            text: "transformed".into(),
            images: None,
        })
    });
    register(&registry, "handled", |event, _, _| {
        assert_eq!(event.text, "transformed");
        Ok(InputResult::Handled)
    });
    let observed = calls.clone();
    register(&registry, "later", move |_, _, _| {
        observed.fetch_add(1, Ordering::SeqCst);
        Ok(InputResult::Continue)
    });
    assert_eq!(emit(&registry, &event()), InputResult::Handled);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn source_and_streaming_behavior_are_fixed_through_the_chain() {
    for source in [
        InputSource::Interactive,
        InputSource::Rpc,
        InputSource::Extension,
    ] {
        for behavior in [
            None,
            Some(InputStreamingBehavior::Steer),
            Some(InputStreamingBehavior::FollowUp),
        ] {
            let registry = SharedRegistry::default();
            register(&registry, "one", |event, _, _| {
                Ok(InputResult::Transform {
                    text: format!("{}!", event.text),
                    images: None,
                })
            });
            register(&registry, "observe", move |event, _, _| {
                assert_eq!(event.source, source);
                assert_eq!(event.streaming_behavior, behavior);
                Ok(InputResult::Continue)
            });
            let mut input = event();
            input.source = source;
            input.streaming_behavior = behavior;
            emit(&registry, &input);
            let value = serde_json::to_value(&input).unwrap();
            assert_eq!(
                value["source"],
                match source {
                    InputSource::Interactive => "interactive",
                    InputSource::Rpc => "rpc",
                    InputSource::Extension => "extension",
                }
            );
            if behavior == Some(InputStreamingBehavior::FollowUp) {
                assert_eq!(value["streamingBehavior"], "followUp");
            }
        }
    }
}

#[test]
fn handler_error_and_panic_are_diagnostic_and_preserve_previous_transform() {
    let registry = SharedRegistry::default();
    register(&registry, "first", |_, _, _| {
        Ok(InputResult::Transform {
            text: "first".into(),
            images: None,
        })
    });
    register(&registry, "error", |_, _, _| Err("input error".into()));
    register(&registry, "panic", |_, _, _| panic!("fixture panic"));
    register(&registry, "last", |event, _, _| {
        assert_eq!(event.text, "first");
        Ok(InputResult::Transform {
            text: "last".into(),
            images: None,
        })
    });
    assert!(matches!(emit(&registry,&event()),InputResult::Transform{text,..} if text=="last"));
    let diagnostics = registry.drain_diagnostics();
    assert_eq!(diagnostics.len(), 2);
    assert_eq!(diagnostics[0].event, "input");
    assert!(diagnostics[0].error.contains("input error"));
    assert!(diagnostics[1].error.contains("panicked"));
}

#[test]
fn invalid_transform_is_transactional_and_diagnostic_instead_of_destroying_input() {
    let registry = SharedRegistry::default();
    register(&registry, "first", |_, _, _| {
        Ok(InputResult::Transform {
            text: "first".into(),
            images: None,
        })
    });
    register(&registry, "not_image", |_, _, _| {
        Ok(InputResult::Transform {
            text: "discarded".into(),
            images: Some(vec![Content::text("invalid")]),
        })
    });
    register(&registry, "too_large", |_, _, _| {
        Ok(InputResult::Transform {
            text: "x".repeat(1_048_577),
            images: Some(vec![]),
        })
    });
    register(&registry, "observe", |event, _, _| {
        assert_eq!(event.text, "first");
        assert_eq!(event.images, vec![image("original")]);
        Ok(InputResult::Continue)
    });
    assert!(matches!(emit(&registry,&event()),InputResult::Transform{text,..} if text=="first"));
    assert_eq!(registry.drain_diagnostics().len(), 2);
}

#[test]
fn initial_invalid_images_and_utf8_byte_limits_are_checked_before_callbacks() {
    let registry = SharedRegistry::default();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    register(&registry, "observe", move |_, _, _| {
        observed.fetch_add(1, Ordering::SeqCst);
        Ok(InputResult::Continue)
    });
    for images in [
        vec![Content::Thinking {
            thinking: "invalid".into(),
        }],
        vec![image("x"); 17],
    ] {
        let mut input = event();
        input.images = images;
        assert!(registry
            .emit_input(
                &input,
                &ExtensionContext::default(),
                &CancellationToken::new()
            )
            .is_err());
    }
    let mut input = event();
    input.text = "🙂".repeat(262_144);
    assert!(validate_input(&input).is_ok());
    input.text.push('x');
    assert!(registry
        .emit_input(
            &input,
            &ExtensionContext::default(),
            &CancellationToken::new()
        )
        .is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn real_large_images_have_an_independent_limit_and_are_not_subject_to_text_cap() {
    let input = InputEvent {
        text: "raw".into(),
        images: vec![image(&"x".repeat(2 * 1024 * 1024))],
        ..Default::default()
    };
    assert!(validate_input(&input).is_ok());
    assert_eq!(
        emit(&SharedRegistry::default(), &input),
        InputResult::Continue
    );
    let input = InputEvent {
        text: "".into(),
        images: vec![image(&"x".repeat(64 * 1024 * 1024))],
        ..Default::default()
    };
    assert!(validate_input(&input).unwrap_err().contains("64MiB"));
}

#[test]
fn cancellation_before_or_during_input_prevents_later_handlers_and_handled_commit() {
    for result in [
        InputResult::Continue,
        InputResult::Handled,
        InputResult::Transform {
            text: "cancelled".into(),
            images: None,
        },
    ] {
        let registry = SharedRegistry::default();
        let calls = Arc::new(AtomicUsize::new(0));
        register(&registry, "cancel", move |_, _, signal| {
            signal.cancel();
            Ok(result.clone())
        });
        let observed = calls.clone();
        register(&registry, "later", move |_, _, _| {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(InputResult::Continue)
        });
        assert!(registry
            .emit_input(
                &event(),
                &ExtensionContext::default(),
                &CancellationToken::new()
            )
            .unwrap_err()
            .contains("aborted"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
    let signal = CancellationToken::new();
    signal.cancel();
    assert!(SharedRegistry::default()
        .emit_input(&event(), &ExtensionContext::default(), &signal)
        .is_err());
}

#[test]
fn unregistering_from_input_is_lock_free_and_skips_stale_snapshotted_handler() {
    let registry = SharedRegistry::default();
    let callback_registry = registry.clone();
    register(&registry, "first", move |_, _, _| {
        callback_registry.unregister("second").unwrap();
        Ok(InputResult::Continue)
    });
    register(&registry, "second", |_, _, _| {
        panic!("unregistered handler ran")
    });
    assert_eq!(emit(&registry, &event()), InputResult::Continue);
    assert!(registry.drain_diagnostics().is_empty());
}

#[test]
fn text_reversal_returns_continue_but_explicit_equal_image_replacement_still_transforms() {
    for replace in [false, true] {
        let registry = SharedRegistry::default();
        register(&registry, "first", |_, _, _| {
            Ok(InputResult::Transform {
                text: "temporary".into(),
                images: None,
            })
        });
        register(&registry, "last", move |_, _, _| {
            Ok(InputResult::Transform {
                text: "raw".into(),
                images: replace.then(|| vec![image("original")]),
            })
        });
        let result = emit(&registry, &event());
        if replace {
            assert_eq!(
                result,
                InputResult::Transform {
                    text: "raw".into(),
                    images: Some(vec![image("original")])
                }
            );
        } else {
            assert_eq!(result, InputResult::Continue);
        }
    }
    let result = serde_json::to_value(InputResult::Transform {
        text: "".into(),
        images: Some(vec![]),
    })
    .unwrap();
    assert_eq!(result["images"], json!([]));
    assert!(serde_json::from_value::<InputResult>(
        json!({"action":"transform","text":"x","unexpected":true})
    )
    .is_err());
}
