use agent_cli::args::parse;
use agent_cli::client::Client;
use agent_cli::terminal_client::TerminalClient;
use agent_core::types::{CancellationToken, Message};
use agent_runtime::extensions::{
    CommandResult, Extension, ExtensionCommand, ExtensionContext, ExtensionMode, InputEvent,
    InputResult, InputStreamingBehavior,
};
use agent_runtime::provider::ProviderMode;
use agent_runtime::resources::{PromptTemplate, ResourceScope};
use agent_runtime::session::{Session, SessionStore};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("terminal-input-{}", Session::default().id));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn client(&self) -> Client {
        let args = parse([
            "--cwd".into(),
            self.0.to_string_lossy().into_owned(),
            "--provider".into(),
            "demo".into(),
            "--auth-dir".into(),
            self.0.join("auth").to_string_lossy().into_owned(),
            "--session-dir".into(),
            self.0.join("sessions").to_string_lossy().into_owned(),
            "--agent-dir".into(),
            self.0.join("agent").to_string_lossy().into_owned(),
            "--no-context-files".into(),
            "--no-skills".into(),
            "--no-prompt-templates".into(),
        ])
        .unwrap();
        let mut client = Client::from_args(&args).unwrap();
        client.options.extension_mode = ExtensionMode::Tui;
        client
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct State {
    seen: Mutex<Vec<(InputEvent, ExtensionContext)>>,
    model_ready: AtomicBool,
    release_model: AtomicBool,
    release_input: AtomicBool,
    block_model: AtomicBool,
    block_input: AtomicBool,
    block_initial: AtomicBool,
    handled: AtomicBool,
    slash: AtomicBool,
    command_calls: AtomicUsize,
}
struct Hook(Arc<State>);
fn wait_gate(gate: &AtomicBool, signal: &CancellationToken) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !gate.load(Ordering::SeqCst) {
        if signal.is_cancelled() {
            return Err("fixture input aborted".into());
        }
        assert!(Instant::now() < deadline, "fixture gate timed out");
        thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}
impl Extension for Hook {
    fn id(&self) -> &str {
        "terminal_input"
    }
    fn commands(&self) -> Vec<Arc<dyn ExtensionCommand>> {
        vec![Arc::new(Self(self.0.clone()))]
    }
    fn input(
        &self,
        event: &InputEvent,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<InputResult, String> {
        self.0
            .seen
            .lock()
            .unwrap()
            .push((event.clone(), context.clone()));
        if event.streaming_behavior.is_none() && self.0.block_initial.load(Ordering::SeqCst) {
            wait_gate(&self.0.release_input, signal)?;
        }
        if event.streaming_behavior.is_some() && self.0.block_input.load(Ordering::SeqCst) {
            wait_gate(&self.0.release_input, signal)?;
        }
        if self.0.handled.load(Ordering::SeqCst) {
            return Ok(InputResult::Handled);
        }
        Ok(InputResult::Transform {
            text: if event.streaming_behavior.is_some() && self.0.slash.load(Ordering::SeqCst) {
                "/fixture_command".into()
            } else {
                format!("{}#hook", event.text)
            },
            images: None,
        })
    }
    fn context(
        &self,
        messages: &[Message],
        _: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<Option<Vec<Message>>, String> {
        if self.0.block_model.load(Ordering::SeqCst)
            && messages
                .iter()
                .rev()
                .find(|message| matches!(message, Message::User(_)))
                .is_some_and(|message| message.text() == "first#hook")
        {
            self.0.model_ready.store(true, Ordering::SeqCst);
            wait_gate(&self.0.release_model, signal)?;
        }
        Ok(None)
    }
}
impl ExtensionCommand for Hook {
    fn name(&self) -> &str {
        "fixture_command"
    }
    fn execute(
        &self,
        _: &str,
        _: &ExtensionContext,
        _: &CancellationToken,
    ) -> Result<CommandResult, String> {
        self.0.command_calls.fetch_add(1, Ordering::SeqCst);
        Ok(CommandResult {
            text: "direct command".into(),
            ..Default::default()
        })
    }
}
fn host(fixture: &Fixture, state: &Arc<State>) -> TerminalClient {
    let client = fixture.client();
    client
        .options
        .extensions
        .register(Arc::new(Hook(state.clone())))
        .unwrap();
    TerminalClient::new(client)
}
fn wait_until(host: &mut TerminalClient, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        host.poll();
        assert!(
            Instant::now() < deadline,
            "Terminal fixture timed out: {}",
            host.model.status
        );
        thread::sleep(Duration::from_millis(2));
    }
}
fn drain(host: &mut TerminalClient) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while host.model.running {
        host.poll();
        assert!(
            Instant::now() < deadline,
            "Terminal worker did not settle: {}",
            host.model.status
        );
        thread::sleep(Duration::from_millis(2));
    }
}
fn users(host: &TerminalClient) -> Vec<String> {
    host.model
        .messages
        .iter()
        .filter(|message| matches!(message, Message::User(_)))
        .map(Message::text)
        .collect()
}

#[test]
fn initial_input_transforms_once_and_handled_skips_provider_and_fake_messages() {
    let fixture = Fixture::new();
    let state = Arc::new(State::default());
    let mut host = host(&fixture, &state);
    host.submit("original", CancellationToken::new()).unwrap();
    drain(&mut host);
    assert_eq!(users(&host), vec!["original#hook"]);
    assert_eq!(state.seen.lock().unwrap().len(), 1);
    let fixture = Fixture::new();
    let state = Arc::new(State::default());
    state.handled.store(true, Ordering::SeqCst);
    let mut client = fixture.client();
    client.settings.mode = ProviderMode::OpenAiCompatible;
    client.settings.model.clear();
    client.settings.api_key.clear();
    client
        .options
        .extensions
        .register(Arc::new(Hook(state.clone())))
        .unwrap();
    let mut host = TerminalClient::new(client);
    host.submit("handled", CancellationToken::new()).unwrap();
    drain(&mut host);
    assert!(host.model.messages.is_empty());
    assert_eq!(host.model.status, "输入已由扩展处理");
    assert_eq!(state.seen.lock().unwrap().len(), 1);
}

#[test]
fn registered_command_priority_bypasses_input_callback() {
    let fixture = Fixture::new();
    let state = Arc::new(State::default());
    state.handled.store(true, Ordering::SeqCst);
    let mut host = host(&fixture, &state);
    host.submit("/fixture_command", CancellationToken::new())
        .unwrap();
    drain(&mut host);
    assert_eq!(state.command_calls.load(Ordering::SeqCst), 1);
    assert!(state.seen.lock().unwrap().is_empty());
    assert!(host.model.messages.is_empty());
    assert_eq!(host.model.status, "direct command");
}

#[test]
fn busy_inputs_run_off_ui_in_fifo_order_using_observed_current_context() {
    let fixture = Fixture::new();
    let state = Arc::new(State::default());
    state.block_model.store(true, Ordering::SeqCst);
    state.block_input.store(true, Ordering::SeqCst);
    let mut host = host(&fixture, &state);
    host.submit("first", CancellationToken::new()).unwrap();
    wait_until(&mut host, || state.model_ready.load(Ordering::SeqCst));
    host.poll();
    let start = Instant::now();
    host.submit("second", CancellationToken::new()).unwrap();
    host.submit("third", CancellationToken::new()).unwrap();
    assert!(start.elapsed() < Duration::from_millis(100));
    wait_until(&mut host, || state.seen.lock().unwrap().len() == 2);
    let seen = state.seen.lock().unwrap();
    assert_eq!(
        seen[1].0.streaming_behavior,
        Some(InputStreamingBehavior::Steer)
    );
    assert!(seen[1]
        .1
        .context
        .messages
        .iter()
        .any(|message| message.text() == "first#hook"));
    drop(seen);
    state.release_input.store(true, Ordering::SeqCst);
    wait_until(&mut host, || state.seen.lock().unwrap().len() == 3);
    state.release_model.store(true, Ordering::SeqCst);
    drain(&mut host);
    assert_eq!(
        users(&host),
        vec!["first#hook", "second#hook", "third#hook"]
    );
    assert_eq!(state.seen.lock().unwrap().len(), 3);
}

#[test]
fn preparation_keeps_running_after_main_finishes_and_late_slash_never_routes_twice() {
    let fixture = Fixture::new();
    let state = Arc::new(State::default());
    state.block_model.store(true, Ordering::SeqCst);
    state.block_input.store(true, Ordering::SeqCst);
    state.slash.store(true, Ordering::SeqCst);
    let mut host = host(&fixture, &state);
    host.submit("first", CancellationToken::new()).unwrap();
    wait_until(&mut host, || state.model_ready.load(Ordering::SeqCst));
    host.submit("late", CancellationToken::new()).unwrap();
    wait_until(&mut host, || state.seen.lock().unwrap().len() == 2);
    state.release_model.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_millis(80);
    while Instant::now() < deadline {
        assert!(
            !host.poll(),
            "poll declared global completion during preparation"
        );
        assert!(host.model.running);
        thread::sleep(Duration::from_millis(2));
    }
    state.release_input.store(true, Ordering::SeqCst);
    drain(&mut host);
    assert_eq!(users(&host), vec!["first#hook", "/fixture_command"]);
    assert_eq!(state.command_calls.load(Ordering::SeqCst), 0);
    assert_eq!(state.seen.lock().unwrap().len(), 2);
}

#[test]
fn stop_cancels_slow_preparation_recovers_raw_drafts_and_never_restarts() {
    let fixture = Fixture::new();
    let state = Arc::new(State::default());
    state.block_model.store(true, Ordering::SeqCst);
    state.block_input.store(true, Ordering::SeqCst);
    let mut host = host(&fixture, &state);
    host.submit("first", CancellationToken::new()).unwrap();
    wait_until(&mut host, || state.model_ready.load(Ordering::SeqCst));
    host.submit("second", CancellationToken::new()).unwrap();
    host.submit("third", CancellationToken::new()).unwrap();
    wait_until(&mut host, || state.seen.lock().unwrap().len() == 2);
    host.model.editor.set_text("current draft").unwrap();
    let start = Instant::now();
    host.cancel();
    assert_eq!(host.model.editor.text(), "second\n\nthird\n\ncurrent draft");
    drain(&mut host);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(!host.has_queued());
    assert_eq!(users(&host), vec!["first#hook"]);
    assert_eq!(host.model.editor.text(), "second\n\nthird\n\ncurrent draft");
    let sessions = SessionStore::new(fixture.0.join("sessions"))
        .list()
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0]
        .context
        .messages
        .iter()
        .any(|message| message.text() == "first#hook"));
}

#[test]
fn shared_interrupt_token_cancels_preparation_even_after_main_worker_settles() {
    let fixture = Fixture::new();
    let state = Arc::new(State::default());
    state.block_model.store(true, Ordering::SeqCst);
    state.block_input.store(true, Ordering::SeqCst);
    let mut host = host(&fixture, &state);
    let signal = CancellationToken::new();
    host.submit("first", signal.clone()).unwrap();
    wait_until(&mut host, || state.model_ready.load(Ordering::SeqCst));
    host.submit("late", CancellationToken::new()).unwrap();
    wait_until(&mut host, || state.seen.lock().unwrap().len() == 2);
    state.release_model.store(true, Ordering::SeqCst);
    thread::sleep(Duration::from_millis(25));
    host.poll();
    assert!(host.model.running);
    signal.cancel();
    drain(&mut host);
    assert_eq!(host.model.editor.text(), "late");
    assert!(!host.has_queued());
    assert_eq!(users(&host), vec!["first#hook"]);
}

#[test]
fn queue_capacity_is_bounded_before_any_more_callbacks_execute() {
    let fixture = Fixture::new();
    let state = Arc::new(State::default());
    state.block_model.store(true, Ordering::SeqCst);
    state.block_input.store(true, Ordering::SeqCst);
    let mut host = host(&fixture, &state);
    host.submit("first", CancellationToken::new()).unwrap();
    wait_until(&mut host, || state.model_ready.load(Ordering::SeqCst));
    for index in 0..32 {
        host.submit(&format!("queued{index}"), CancellationToken::new())
            .unwrap();
    }
    assert!(host
        .submit("too many", CancellationToken::new())
        .unwrap_err()
        .contains("32"));
    host.cancel();
    drain(&mut host);
    assert!(!host.model.editor.text().contains("too many"));
    assert!(!host.has_queued());
}

#[test]
fn busy_prompt_templates_are_hooked_raw_and_expanded_only_once() {
    let fixture = Fixture::new();
    let state = Arc::new(State::default());
    state.block_model.store(true, Ordering::SeqCst);
    let mut client = fixture.client();
    client.resources.prompts.push(PromptTemplate {
        name: "tpl".into(),
        description: "fixture".into(),
        argument_hint: None,
        content: "expanded $@".into(),
        file_path: fixture.0.join("template.md"),
        source: ResourceScope::Path,
    });
    client
        .options
        .extensions
        .register(Arc::new(Hook(state.clone())))
        .unwrap();
    let mut host = TerminalClient::new(client);
    host.submit("first", CancellationToken::new()).unwrap();
    wait_until(&mut host, || state.model_ready.load(Ordering::SeqCst));
    host.submit("/tpl argument", CancellationToken::new())
        .unwrap();
    wait_until(&mut host, || state.seen.lock().unwrap().len() == 2);
    state.release_model.store(true, Ordering::SeqCst);
    drain(&mut host);
    assert_eq!(state.seen.lock().unwrap()[1].0.text, "/tpl argument");
    assert_eq!(users(&host), vec!["first#hook", "expanded argument#hook"]);
}

#[test]
fn dropping_host_cancels_joins_preparation_and_main_without_losing_authoritative_save() {
    let fixture = Fixture::new();
    let state = Arc::new(State::default());
    state.block_model.store(true, Ordering::SeqCst);
    state.block_input.store(true, Ordering::SeqCst);
    let mut host = host(&fixture, &state);
    host.submit("first", CancellationToken::new()).unwrap();
    wait_until(&mut host, || state.model_ready.load(Ordering::SeqCst));
    host.submit("unconsumed", CancellationToken::new()).unwrap();
    wait_until(&mut host, || state.seen.lock().unwrap().len() == 2);
    let start = Instant::now();
    drop(host);
    assert!(start.elapsed() < Duration::from_secs(2));
    let sessions = SessionStore::new(fixture.0.join("sessions"))
        .list()
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0]
        .context
        .messages
        .iter()
        .any(|message| message.text() == "first#hook"));
    assert!(!sessions[0]
        .context
        .messages
        .iter()
        .any(|message| message.text().contains("unconsumed")));
}

#[test]
fn tui_constructor_overrides_in_process_print_mode_and_supplies_ui_context() {
    let fixture = Fixture::new();
    let state = Arc::new(State::default());
    let mut client = fixture.client();
    client.options.extension_mode = ExtensionMode::Print;
    client
        .options
        .extensions
        .register(Arc::new(Hook(state.clone())))
        .unwrap();
    let mut host = TerminalClient::new(client);
    host.submit("mode", CancellationToken::new()).unwrap();
    drain(&mut host);
    let seen = state.seen.lock().unwrap();
    assert_eq!(seen[0].1.mode, ExtensionMode::Tui);
    assert!(seen[0].1.has_ui);
}

#[test]
fn cancellation_of_a_prepared_but_unconsumed_queue_restores_original_raw_text() {
    let fixture = Fixture::new();
    let state = Arc::new(State::default());
    state.block_model.store(true, Ordering::SeqCst);
    let client = fixture.client();
    let queues = client.queues.clone();
    client
        .options
        .extensions
        .register(Arc::new(Hook(state.clone())))
        .unwrap();
    let mut host = TerminalClient::new(client);
    host.submit("first", CancellationToken::new()).unwrap();
    wait_until(&mut host, || state.model_ready.load(Ordering::SeqCst));
    host.submit("original queued", CancellationToken::new())
        .unwrap();
    wait_until(&mut host, || !queues.is_empty());
    host.cancel();
    assert_eq!(host.model.editor.text(), "original queued");
    assert!(!host.model.editor.text().contains("#hook"));
    drain(&mut host);
    assert_eq!(users(&host), vec!["first#hook"]);
}

#[test]
fn slow_initial_input_can_cancel_before_provider_and_restores_unconsumed_raw_draft() {
    let fixture = Fixture::new();
    let state = Arc::new(State::default());
    state.block_initial.store(true, Ordering::SeqCst);
    let mut host = host(&fixture, &state);
    host.submit("raw initial", CancellationToken::new())
        .unwrap();
    wait_until(&mut host, || state.seen.lock().unwrap().len() == 1);
    assert!(host.model.running);
    assert!(host.model.messages.is_empty());
    let start = Instant::now();
    host.cancel();
    drain(&mut host);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(host.model.messages.is_empty());
    assert_eq!(host.model.editor.text(), "raw initial");
    let sessions = SessionStore::new(fixture.0.join("sessions"))
        .list()
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].context.messages.is_empty());
}
