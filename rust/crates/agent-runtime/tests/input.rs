#[path = "common/native_extension.rs"]
mod fixture;
use agent_core::agent_loop::Provider;
use agent_core::types::{AssistantMessage, CancellationToken, Content, Context, Message};
use agent_runtime::extensions::{
    extension_context, load_native_extensions, prepare_input, prepare_input_with_behavior,
    Extension, ExtensionContext, ExtensionMode, InputEvent, InputResult, InputSource,
    InputStreamingBehavior, SharedRegistry,
};
use agent_runtime::runtime::{run_with_provider, MessageQueues, RunOptions};
use agent_runtime::session::Session;
use fixture::{assert_gone, wait_pid, Fixture};
use serde_json::{json, Value};
use std::fs;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

fn image_prompt() -> Message {
    let mut message = Message::user("raw 中文🙂 \"quoted\"\ninput");
    if let Message::User(user) = &mut message {
        user.timestamp = 123;
        user.content.push(Content::Image {
            data: "original image".into(),
            mime_type: "image/png".into(),
        });
    }
    message
}
fn load(fixture: &Fixture, session: &Session) -> SharedRegistry {
    let context = extension_context(
        session,
        &SharedRegistry::default(),
        ExtensionMode::Rpc,
        None,
    )
    .unwrap();
    load_native_extensions(
        std::slice::from_ref(&fixture.manifest),
        &context,
        &CancellationToken::new(),
    )
    .unwrap()
}
fn write_args(fixture: &Fixture, args: Vec<String>) {
    let mut manifest = fixture.manifest();
    manifest.args = args;
    fs::write(&fixture.manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
}
fn records(path: &std::path::Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn native_input_transforms_utf8_and_preserves_images_and_timestamp_without_session_io() {
    let fixture = Fixture::new(&["--input-prefix=prefix:"]);
    let session = fixture.session();
    let registry = load(&fixture, &session);
    let context = extension_context(&session, &registry, ExtensionMode::Rpc, None).unwrap();
    let before = serde_json::to_value(&session).unwrap();
    let original = image_prompt();
    let result = prepare_input(
        &session,
        &registry,
        original.clone(),
        InputSource::Rpc,
        &context,
        &CancellationToken::new(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(result.text(), format!("prefix:{}", original.text()));
    let Message::User(result) = result else {
        panic!("expected user")
    };
    let Message::User(original) = original else {
        panic!("expected user")
    };
    assert_eq!(result.timestamp, 123);
    assert_eq!(result.content[1], original.content[1]);
    assert_eq!(serde_json::to_value(session).unwrap(), before);
    assert!(registry.drain_diagnostics().is_empty());
}

#[test]
fn multiple_real_processes_chain_transforms_and_handled_skips_later_process_io() {
    let first = Fixture::new(&["--input-prefix=first:"]);
    let second = Fixture::new(&["--input-handled"]);
    let third = Fixture::new(&["--input-prefix=unexpected:"]);
    let record = third.path.join("record");
    write_args(
        &third,
        vec![
            "--input-prefix=unexpected:".into(),
            format!("--record={}", record.display()),
        ],
    );
    for (fixture, id) in [(&first, "first"), (&second, "second"), (&third, "third")] {
        let mut manifest = fixture.manifest();
        manifest.id = id.into();
        fs::write(&fixture.manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
    }
    let session = first.session();
    let context = extension_context(
        &session,
        &SharedRegistry::default(),
        ExtensionMode::Rpc,
        None,
    )
    .unwrap();
    let registry = load_native_extensions(
        &[
            first.manifest.clone(),
            second.manifest.clone(),
            third.manifest.clone(),
        ],
        &context,
        &CancellationToken::new(),
    )
    .unwrap();
    let before = serde_json::to_value(&session).unwrap();
    assert!(prepare_input(
        &session,
        &registry,
        image_prompt(),
        InputSource::Interactive,
        &context,
        &CancellationToken::new()
    )
    .unwrap()
    .is_none());
    assert_eq!(serde_json::to_value(session).unwrap(), before);
    assert_eq!(
        records(&record)
            .iter()
            .filter(|record| record["method"] == "input")
            .count(),
        0
    );
}

#[test]
fn native_clear_and_replace_images_are_distinct_from_omitted_images() {
    for flag in ["--input-clear-images", "--input-replace-images"] {
        let fixture = Fixture::new(&[flag]);
        let session = fixture.session();
        let registry = load(&fixture, &session);
        let context = extension_context(&session, &registry, ExtensionMode::Rpc, None).unwrap();
        let result = prepare_input(
            &session,
            &registry,
            image_prompt(),
            InputSource::Extension,
            &context,
            &CancellationToken::new(),
        )
        .unwrap()
        .unwrap();
        let Message::User(user) = result else {
            panic!("expected user")
        };
        if flag == "--input-clear-images" {
            assert_eq!(user.content.len(), 1);
        } else {
            assert!(
                matches!(&user.content[1],Content::Image{data,mime_type} if data=="native image" && mime_type=="image/jpeg")
            );
        }
        assert!(registry.drain_diagnostics().is_empty());
    }
}

#[test]
fn native_error_malformed_and_invalid_transform_are_diagnostic_and_keep_original_input() {
    for flag in [
        "--input-error",
        "--input-malformed",
        "--input-invalid-images",
    ] {
        let fixture = Fixture::new(&[flag]);
        let session = fixture.session();
        let registry = load(&fixture, &session);
        let context = extension_context(&session, &registry, ExtensionMode::Rpc, None).unwrap();
        let prompt = image_prompt();
        assert_eq!(
            prepare_input(
                &session,
                &registry,
                prompt.clone(),
                InputSource::Rpc,
                &context,
                &CancellationToken::new()
            )
            .unwrap(),
            Some(prompt)
        );
        assert!(registry
            .drain_diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.event == "input"));
    }
}

#[test]
fn cancelling_a_stalled_native_input_kills_its_job_and_never_commits_fake_messages() {
    let fixture = Fixture::new(&[]);
    let pid_file = fixture.path.join("pid");
    write_args(
        &fixture,
        vec![
            "--hang-input".into(),
            format!("--pid={}", pid_file.display()),
        ],
    );
    let session = fixture.session();
    let before = serde_json::to_value(&session).unwrap();
    let registry = load(&fixture, &session);
    let pid = wait_pid(&pid_file);
    let context = extension_context(&session, &registry, ExtensionMode::Rpc, None).unwrap();
    let signal = CancellationToken::new();
    let cancellation = signal.clone();
    let worker = thread::spawn(move || {
        thread::sleep(Duration::from_millis(75));
        cancellation.cancel();
    });
    let start = Instant::now();
    assert!(prepare_input(
        &session,
        &registry,
        image_prompt(),
        InputSource::Rpc,
        &context,
        &signal
    )
    .unwrap_err()
    .contains("aborted"));
    assert!(start.elapsed() < Duration::from_secs(2));
    worker.join().unwrap();
    assert_gone(pid);
    assert_eq!(serde_json::to_value(session).unwrap(), before);
}

#[test]
fn native_raw_context_source_streaming_and_latest_branch_state_are_correct() {
    let fixture = Fixture::new(&[]);
    let record = fixture.path.join("requests");
    write_args(&fixture, vec![format!("--record={}", record.display())]);
    let mut session = fixture.session();
    let registry = load(&fixture, &session);
    let stale = extension_context(&session, &registry, ExtensionMode::Rpc, None).unwrap();
    session
        .record_runtime_state("extension:fixture", json!({"active":2}))
        .unwrap();
    session
        .append_message(Message::user("new branch message"))
        .unwrap();
    let prompt = Message::user("/template raw");
    assert_eq!(
        prepare_input_with_behavior(
            &session,
            &registry,
            prompt.clone(),
            InputSource::Extension,
            &stale,
            &CancellationToken::new(),
            Some(InputStreamingBehavior::FollowUp)
        )
        .unwrap(),
        Some(prompt)
    );
    let input = records(&record)
        .into_iter()
        .find(|record| record["method"] == "input")
        .unwrap();
    assert_eq!(input["params"]["event"]["text"], "/template raw");
    assert_eq!(input["params"]["event"]["source"], "extension");
    assert_eq!(input["params"]["event"]["streamingBehavior"], "followUp");
    assert_eq!(
        input["params"]["context"]["states"]["fixture"],
        json!({"active":2})
    );
    assert!(input["params"]["context"]["context"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["content"][0]["text"] == "new branch message"));
}

#[test]
fn explicit_native_frame_limits_do_not_silently_remove_large_attachments() {
    let fixture = Fixture::new(&[]);
    let session = fixture.session();
    let registry = load(&fixture, &session);
    let context = extension_context(&session, &registry, ExtensionMode::Rpc, None).unwrap();
    let mut prompt = image_prompt();
    if let Message::User(user) = &mut prompt {
        user.content.push(Content::Image {
            data: "x".repeat(2 * 1024 * 1024),
            mime_type: "image/png".into(),
        });
    }
    assert_eq!(
        prepare_input(
            &session,
            &registry,
            prompt.clone(),
            InputSource::Rpc,
            &context,
            &CancellationToken::new()
        )
        .unwrap(),
        Some(prompt.clone())
    );
    assert!(registry
        .drain_diagnostics()
        .iter()
        .any(|diagnostic| diagnostic.error.contains("frame exceeds")));
    let empty = SharedRegistry::default();
    assert_eq!(
        prepare_input(
            &session,
            &empty,
            prompt.clone(),
            InputSource::Interactive,
            &context,
            &CancellationToken::new()
        )
        .unwrap(),
        Some(prompt)
    );
}

struct CountInputs(Arc<Mutex<Vec<InputEvent>>>);
impl Extension for CountInputs {
    fn id(&self) -> &str {
        "count"
    }
    fn input(
        &self,
        event: &InputEvent,
        _: &ExtensionContext,
        _: &CancellationToken,
    ) -> Result<InputResult, String> {
        self.0.lock().unwrap().push(event.clone());
        Ok(InputResult::Transform {
            text: format!("{}:prepared", event.text),
            images: None,
        })
    }
}
struct Fixed {
    requests: Vec<Context>,
}
impl Provider for Fixed {
    fn complete(
        &mut self,
        context: &Context,
        _: &CancellationToken,
        _: &mut dyn FnMut(AssistantMessage),
    ) -> Result<AssistantMessage, String> {
        self.requests.push(context.clone());
        Ok(AssistantMessage::text("done"))
    }
}

#[test]
fn already_prepared_queue_input_is_not_hooked_again_by_paid_runtime() {
    let seen = Arc::new(Mutex::new(vec![]));
    let registry = SharedRegistry::default();
    registry
        .register(Arc::new(CountInputs(seen.clone())))
        .unwrap();
    let mut session = Session::default();
    let context = extension_context(&session, &registry, ExtensionMode::Tui, None).unwrap();
    let queued = prepare_input_with_behavior(
        &session,
        &registry,
        Message::user("queued"),
        InputSource::Interactive,
        &context,
        &CancellationToken::new(),
        Some(InputStreamingBehavior::Steer),
    )
    .unwrap()
    .unwrap();
    let queues = MessageQueues::default();
    queues.steer(queued);
    let mut provider = Fixed { requests: vec![] };
    let options = RunOptions {
        extensions: registry,
        ..Default::default()
    };
    run_with_provider(
        &mut session,
        Message::user("initial already prepared"),
        &mut provider,
        &[],
        &options,
        &queues,
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(
        seen.lock().unwrap()[0].streaming_behavior,
        Some(InputStreamingBehavior::Steer)
    );
    assert!(provider.requests[0]
        .messages
        .iter()
        .any(|message| message.text() == "queued:prepared"));
    assert!(!session
        .context
        .messages
        .iter()
        .any(|message| message.text().ends_with(":prepared:prepared")));
}

#[test]
fn helper_rejects_non_user_and_unsupported_blocks_without_session_or_model_io() {
    let session = Session::default();
    let registry = SharedRegistry::default();
    let context = extension_context(&session, &registry, ExtensionMode::Print, None).unwrap();
    let mut prompt = Message::user("raw");
    if let Message::User(user) = &mut prompt {
        user.content.push(Content::Thinking {
            thinking: "unsupported".into(),
        });
    }
    assert!(prepare_input(
        &session,
        &registry,
        prompt,
        InputSource::Interactive,
        &context,
        &CancellationToken::new()
    )
    .is_err());
    assert!(prepare_input(
        &session,
        &registry,
        Message::Assistant(AssistantMessage::text("fake")),
        InputSource::Interactive,
        &context,
        &CancellationToken::new()
    )
    .is_err());
    assert!(session.context.messages.is_empty());
}
