//! Offline TUI model/branch commands run outside the UI thread and never become messages.
use agent_cli::args::Args;
use agent_cli::client::Client;
use agent_cli::terminal_client::TerminalClient;
use agent_core::types::{AssistantMessage, CancellationToken, Message};
use agent_runtime::extensions::{
    record_model_selection, stored_model_selection, Extension, ExtensionContext, ExtensionMode,
    InputEvent, InputResult, LifecycleEvent, ModelIdentity, ModelSelectSource,
};
use agent_runtime::session::{Session, SessionStore};
use serde_json::{json, Value};
#[cfg(windows)]
use std::ffi::c_void;
use std::fs;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "OpenProcess"]
    fn open_process(access: u32, inherit: i32, pid: u32) -> *mut c_void;
    #[link_name = "WaitForSingleObject"]
    fn wait_for_single_object(process: *mut c_void, milliseconds: u32) -> u32;
    #[link_name = "CloseHandle"]
    fn close_handle(handle: *mut c_void) -> i32;
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("terminal-model-{}", Session::default().id));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn args(&self) -> Args {
        Args {
            cwd: Some(self.0.clone()),
            provider: Some("demo".into()),
            auth_dir: Some(self.0.join("auth")),
            session_dir: Some(self.0.join("sessions")),
            agent_dir: Some(self.0.join("agent")),
            no_context_files: true,
            no_skills: true,
            no_prompts: true,
            ..Default::default()
        }
    }
    fn client(&self) -> Client {
        Client::from_args(&self.args()).unwrap()
    }
    fn native_client(&self, flags: &[&str]) -> Client {
        let manifest = self.0.join("extension.json");
        let mut args: Vec<String> = flags.iter().map(|flag| (*flag).into()).collect();
        args.extend([
            format!("--record={}", self.0.join("requests").display()),
            format!("--pid={}", self.0.join("pid").display()),
            format!("--child={}", self.0.join("child-pid").display()),
        ]);
        fs::write(&manifest,json!({"id":"fixture","executable":executable(),"args":args,"requestTimeoutMs":30000,"maxFrameBytes":1048576}).to_string()).unwrap();
        Client::from_args(&Args {
            extensions: vec![manifest],
            ..self.args()
        })
        .unwrap()
    }
    fn sessions(&self) -> Vec<Session> {
        SessionStore::new(self.0.join("sessions")).list().unwrap()
    }
    fn records(&self) -> Vec<Value> {
        let text = fs::read_to_string(self.0.join("requests")).unwrap();
        let end = text.rfind('\n').map_or(0, |index| index + 1);
        text[..end]
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        assert!(self.0.starts_with(std::env::temp_dir()));
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn executable() -> &'static Path {
    static EXE: OnceLock<PathBuf> = OnceLock::new();
    EXE.get_or_init(|| {
        let path =
            std::env::temp_dir().join(format!("terminal-model-compiler-{}", Session::default().id));
        fs::create_dir(&path).unwrap();
        let output = path.join(if cfg!(windows) {
            "fixture.exe"
        } else {
            "fixture"
        });
        let mut command = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()));
        #[cfg(windows)]
        command.creation_flags(0x0800_0000);
        let result = command
            .args(["--edition=2021", "-O"])
            .arg(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../agent-runtime/examples/native_extension_fixture.rs"),
            )
            .arg("-o")
            .arg(&output)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        output
    })
}
#[derive(Default)]
struct State {
    observed: Mutex<Vec<(LifecycleEvent, ExtensionContext)>>,
    input_calls: AtomicUsize,
    agent_events: AtomicUsize,
    session_start_calls: AtomicUsize,
    hang_set: AtomicBool,
    hang_restore: AtomicBool,
    entered: AtomicBool,
    cancelled: AtomicBool,
    fail: AtomicBool,
}
struct Hook(Arc<State>);
impl Extension for Hook {
    fn id(&self) -> &str {
        "terminal-model"
    }
    fn input(
        &self,
        _: &InputEvent,
        _: &ExtensionContext,
        _: &CancellationToken,
    ) -> Result<InputResult, String> {
        self.0.input_calls.fetch_add(1, Ordering::SeqCst);
        Ok(InputResult::Continue)
    }
    fn on_event(
        &self,
        event: &LifecycleEvent,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        if let LifecycleEvent::ModelSelect { source, .. } = event {
            self.0
                .observed
                .lock()
                .unwrap()
                .push((event.clone(), context.clone()));
            if (*source == ModelSelectSource::Set && self.0.hang_set.load(Ordering::SeqCst))
                || (*source == ModelSelectSource::Restore
                    && self.0.hang_restore.load(Ordering::SeqCst))
            {
                self.0.entered.store(true, Ordering::SeqCst);
                let deadline = Instant::now() + Duration::from_secs(10);
                while !signal.is_cancelled() {
                    assert!(Instant::now() < deadline, "Offline model hook deadline");
                    thread::sleep(Duration::from_millis(2));
                }
                self.0.cancelled.store(true, Ordering::SeqCst);
                return Err("offline model notification cancelled".into());
            }
            if self.0.fail.load(Ordering::SeqCst) {
                return Err("offline model notification failed".into());
            }
        }
        if matches!(event, LifecycleEvent::Agent { .. }) {
            self.0.agent_events.fetch_add(1, Ordering::SeqCst);
        }
        if matches!(event, LifecycleEvent::SessionStart { .. }) {
            self.0.session_start_calls.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
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
fn wait(host: &mut TerminalClient, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        host.poll();
        assert!(
            Instant::now() < deadline,
            "Terminal fixture: {}",
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
            "Terminal worker: {}",
            host.model.status
        );
        thread::sleep(Duration::from_millis(2));
    }
}
fn identity(model: &str) -> ModelIdentity {
    ModelIdentity {
        provider: "demo".into(),
        model: model.into(),
    }
}
fn assert_no_prompt_events(host: &TerminalClient, state: &State) {
    assert!(host.model.messages.is_empty());
    assert_eq!(state.input_calls.load(Ordering::SeqCst), 0);
    assert_eq!(state.agent_events.load(Ordering::SeqCst), 0);
}
fn assert_gone(path: &Path) {
    let pid: u32 = fs::read_to_string(path).unwrap().trim().parse().unwrap();
    #[cfg(windows)]
    {
        // SYNCHRONIZE asks only for the process object's terminated/signaled state.
        let process = unsafe { open_process(0x0010_0000, 0, pid) };
        if !process.is_null() {
            let status = unsafe { wait_for_single_object(process, 2000) };
            unsafe { close_handle(process) };
            assert_eq!(status, 0, "Native model fixture remains alive");
        }
    }
    #[cfg(unix)]
    {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Path::new(&format!("/proc/{pid}")).exists() {
            if fs::read_to_string(format!("/proc/{pid}/stat"))
                .is_ok_and(|stat| stat.split_whitespace().nth(2) == Some("Z"))
            {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
    }
}

#[test]
fn model_command_runs_a_worker_and_notifies_once_without_prompt_or_provider_events() {
    let fixture = Fixture::new();
    let state = Arc::new(State::default());
    let mut host = host(&fixture, &state);
    assert!(host
        .model
        .commands
        .iter()
        .any(|command| command == "/model"));
    host.submit("/help", CancellationToken::new()).unwrap();
    assert!(host.model.status.contains("/model PROVIDER MODEL"));
    host.submit("/model\tdemo\tselected", CancellationToken::new())
        .unwrap();
    assert!(host.model.running);
    drain(&mut host);
    assert_eq!(host.model.model, "demo/selected");
    assert_no_prompt_events(&host, &state);
    let observed = state.observed.lock().unwrap();
    assert_eq!(observed.len(), 1);
    assert!(
        matches!(&observed[0].0,LifecycleEvent::ModelSelect{model,previous_model:Some(previous),source:ModelSelectSource::Set} if model==&identity("selected")&&previous==&identity(""))
    );
    assert_eq!(observed[0].1.mode, ExtensionMode::Tui);
    assert!(observed[0].1.has_ui);
    assert_eq!(observed[0].1.model, Some(identity("selected")));
    drop(observed);
    host.submit("/model demo selected", CancellationToken::new())
        .unwrap();
    drain(&mut host);
    assert_eq!(state.observed.lock().unwrap().len(), 1);
    assert_eq!(
        stored_model_selection(&fixture.sessions()[0]).unwrap(),
        Some(identity("selected"))
    );
}

#[test]
fn invalid_model_command_and_usage_do_not_emit_model_events_or_messages() {
    let fixture = Fixture::new();
    let state = Arc::new(State::default());
    let mut host = host(&fixture, &state);
    for command in [
        "/model",
        "/model unknown-provider invalid",
        "/fork",
        "/resume",
    ] {
        host.submit(command, CancellationToken::new()).unwrap();
        drain(&mut host);
        assert_no_prompt_events(&host, &state);
        assert!(state.observed.lock().unwrap().is_empty());
        assert_eq!(host.model.model, "demo/(default)");
    }
    assert!(fixture
        .sessions()
        .iter()
        .all(|session| stored_model_selection(session).unwrap().is_none()));
}

#[test]
fn model_notification_error_is_diagnostic_and_does_not_veto_committed_settings() {
    let fixture = Fixture::new();
    let state = Arc::new(State::default());
    state.fail.store(true, Ordering::SeqCst);
    let mut host = host(&fixture, &state);
    host.submit("/model demo selected", CancellationToken::new())
        .unwrap();
    drain(&mut host);
    assert_eq!(host.model.model, "demo/selected");
    assert!(host
        .model
        .status
        .contains("offline model notification failed"));
    assert_no_prompt_events(&host, &state);
    assert_eq!(
        stored_model_selection(&fixture.sessions()[0]).unwrap(),
        Some(identity("selected"))
    );
}

#[test]
fn hanging_in_process_set_notification_is_cancellable_and_preserves_commit_and_draft() {
    let fixture = Fixture::new();
    let state = Arc::new(State::default());
    state.hang_set.store(true, Ordering::SeqCst);
    let mut host = host(&fixture, &state);
    let start = Instant::now();
    host.submit("/model demo committed", CancellationToken::new())
        .unwrap();
    assert!(start.elapsed() < Duration::from_millis(150));
    wait(&mut host, || state.entered.load(Ordering::SeqCst));
    host.model.editor.set_text("original editor draft").unwrap();
    assert!(host
        .submit("not queued during model change", CancellationToken::new())
        .unwrap_err()
        .contains("model/session"));
    let start = Instant::now();
    host.cancel();
    drain(&mut host);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(state.cancelled.load(Ordering::SeqCst));
    assert_eq!(host.model.model, "demo/committed");
    assert!(host.model.status.contains("模型选择已保存"));
    assert!(host.model.editor.text().contains("original editor draft"));
    assert!(!host.has_queued());
    assert_no_prompt_events(&host, &state);
    assert_eq!(
        stored_model_selection(&fixture.sessions()[0]).unwrap(),
        Some(identity("committed"))
    );
}

#[test]
fn hanging_real_process_set_notification_cancels_its_job_and_saves_model_without_messages() {
    let fixture = Fixture::new();
    let client = fixture.native_client(&["--hang-model-select"]);
    let mut host = TerminalClient::new(client);
    host.submit("/model demo native-committed", CancellationToken::new())
        .unwrap();
    wait(&mut host, || {
        fixture.records().iter().any(|record| {
            record["params"]["event"]["type"] == "model_select"
                && record["params"]["event"]["source"] == "set"
        })
    });
    let start = Instant::now();
    host.cancel();
    drain(&mut host);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(host.model.model, "demo/native-committed");
    assert!(host.model.messages.is_empty());
    assert!(host.model.status.contains("模型选择已保存"));
    assert_gone(&fixture.0.join("pid"));
    assert_gone(&fixture.0.join("child-pid"));
    assert_eq!(
        stored_model_selection(&fixture.sessions()[0]).unwrap(),
        Some(identity("native-committed"))
    );
}

#[test]
fn native_model_error_keeps_selected_model_and_never_runs_input_or_agent_callbacks() {
    let fixture = Fixture::new();
    let client = fixture.native_client(&["--model-select-error"]);
    let mut host = TerminalClient::new(client);
    host.submit("/model demo native-selected", CancellationToken::new())
        .unwrap();
    drain(&mut host);
    assert_eq!(host.model.model, "demo/native-selected");
    assert!(host.model.status.contains("model_select fixture error"));
    assert!(host.model.messages.is_empty());
    assert!(!fixture
        .records()
        .iter()
        .any(|record| record["method"] == "input" || record["method"] == "before_agent_start"));
    assert_eq!(
        stored_model_selection(&fixture.sessions()[0]).unwrap(),
        Some(identity("native-selected"))
    );
}

#[test]
fn branch_navigation_and_fork_restore_models_off_ui_and_recover_the_real_fork_draft() {
    let fixture = Fixture::new();
    let state = Arc::new(State::default());
    let mut client = fixture.client();
    record_model_selection(&mut client.session, &identity("branch-a")).unwrap();
    client
        .session
        .append_message(Message::user("draft A"))
        .unwrap();
    client
        .session
        .append_message(Message::Assistant(AssistantMessage::text("answer A")))
        .unwrap();
    let leaf_a = client.session.get_leaf_id().unwrap().unwrap();
    record_model_selection(&mut client.session, &identity("branch-b")).unwrap();
    let user_b = client
        .session
        .append_message(Message::user("draft B"))
        .unwrap();
    client
        .session
        .append_message(Message::Assistant(AssistantMessage::text("answer B")))
        .unwrap();
    client.settings.model = "branch-b".into();
    client.options.extension_model = Some(identity("branch-b"));
    client
        .options
        .extensions
        .register(Arc::new(Hook(state.clone())))
        .unwrap();
    let mut host = TerminalClient::new(client);
    host.submit(&format!("/navigate {leaf_a}"), CancellationToken::new())
        .unwrap();
    assert!(host.model.running);
    drain(&mut host);
    assert_eq!(host.model.model, "demo/branch-a");
    assert_eq!(host.model.messages.len(), 2);
    host.submit(&format!("/fork {user_b}"), CancellationToken::new())
        .unwrap();
    assert!(host.model.running);
    drain(&mut host);
    assert_eq!(host.model.model, "demo/branch-b");
    assert_eq!(host.model.editor.text(), "draft B");
    assert_eq!(host.model.messages.len(), 2);
    let observed = state.observed.lock().unwrap();
    assert_eq!(observed.len(), 2);
    assert!(observed.iter().all(|(event, context)| matches!(
        event,
        LifecycleEvent::ModelSelect {
            source: ModelSelectSource::Restore,
            ..
        }
    ) && context.mode == ExtensionMode::Tui
        && context.has_ui));
    assert_eq!(state.input_calls.load(Ordering::SeqCst), 0);
    assert_eq!(state.agent_events.load(Ordering::SeqCst), 0);
}

#[test]
fn native_model_state_routes_branch_restore_to_worker_even_without_extensions() {
    let fixture = Fixture::new();
    let mut client = fixture.client();
    record_model_selection(&mut client.session, &identity("a")).unwrap();
    let leaf = client.session.get_leaf_id().unwrap().unwrap();
    record_model_selection(&mut client.session, &identity("b")).unwrap();
    client.settings.model = "b".into();
    client.options.extension_model = Some(identity("b"));
    let mut host = TerminalClient::new(client);
    host.submit(&format!("/navigate {leaf}"), CancellationToken::new())
        .unwrap();
    assert!(host.model.running);
    drain(&mut host);
    assert_eq!(host.model.model, "demo/a");
    assert!(host.model.messages.is_empty());
}

#[test]
fn clone_new_and_resume_use_workers_and_notify_only_real_model_changes() {
    let fixture = Fixture::new();
    let state = Arc::new(State::default());
    let mut client = fixture.client();
    record_model_selection(&mut client.session, &identity("current")).unwrap();
    client.settings.model = "current".into();
    client.options.extension_model = Some(identity("current"));
    client
        .options
        .extensions
        .register(Arc::new(Hook(state.clone())))
        .unwrap();
    let mut saved = Session {
        workspace: fixture.0.clone(),
        ..Default::default()
    };
    record_model_selection(&mut saved, &identity("saved")).unwrap();
    saved.append_message(Message::user("saved user")).unwrap();
    client.store.as_ref().unwrap().save(&saved).unwrap();
    let mut host = TerminalClient::new(client);
    for command in [
        "/clone".into(),
        "/new".into(),
        format!("/resume {}", saved.id),
    ] {
        host.submit(&command, CancellationToken::new()).unwrap();
        assert!(host.model.running);
        drain(&mut host);
    }
    assert_eq!(host.model.model, "demo/saved");
    assert_eq!(host.model.messages.len(), 1);
    assert_eq!(host.model.messages[0].text(), "saved user");
    assert_eq!(state.observed.lock().unwrap().len(), 1);
    assert_eq!(state.session_start_calls.load(Ordering::SeqCst), 3);
    assert_eq!(state.input_calls.load(Ordering::SeqCst), 0);
    assert_eq!(state.agent_events.load(Ordering::SeqCst), 0);
}

#[test]
fn dropping_during_set_notification_joins_and_preserves_the_committed_model() {
    let fixture = Fixture::new();
    let state = Arc::new(State::default());
    state.hang_set.store(true, Ordering::SeqCst);
    let mut host = host(&fixture, &state);
    host.submit("/model demo drop-committed", CancellationToken::new())
        .unwrap();
    wait(&mut host, || state.entered.load(Ordering::SeqCst));
    let start = Instant::now();
    drop(host);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(state.cancelled.load(Ordering::SeqCst));
    assert_eq!(
        stored_model_selection(&fixture.sessions()[0]).unwrap(),
        Some(identity("drop-committed"))
    );
    assert!(fixture.sessions()[0].context.messages.is_empty());
}
