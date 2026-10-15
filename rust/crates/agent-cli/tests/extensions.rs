//! Offline frontend fixtures. Command output is UI data, not invented model history.
use agent_cli::args::parse;
use agent_cli::client::Client;
use agent_cli::rpc::run_rpc;
use agent_cli::terminal_client::TerminalClient;
use agent_core::types::{AssistantMessage, CancellationToken, Message};
use agent_runtime::extensions::{
    CommandResult, Extension, ExtensionCommand, ExtensionContext, LifecycleEvent,
    NotificationLevel, UiRequest,
};
use agent_runtime::session::{Session, SessionStore};
use serde_json::json;
use std::io::{self, BufReader, Read};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

struct Fixture {
    path: std::path::PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "agent-frontend-extensions-{}",
            Session::default().id
        ));
        std::fs::create_dir(&path).unwrap();
        Self { path }
    }
    fn client(&self) -> Client {
        Client::from_args(
            &parse(vec![
                "--cwd".into(),
                self.path.to_string_lossy().into_owned(),
                "--session-dir".into(),
                self.path.join("sessions").to_string_lossy().into_owned(),
                "--no-context-files".into(),
                "--no-skills".into(),
                "--no-prompt-templates".into(),
            ])
            .unwrap(),
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        assert!(self.path.starts_with(std::env::temp_dir()));
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
struct CommandFixture {
    invalid: bool,
    slow: bool,
    entered: Option<mpsc::Sender<()>>,
    observed: Arc<Mutex<Vec<serde_json::Value>>>,
}
impl ExtensionCommand for CommandFixture {
    fn name(&self) -> &str {
        if self.invalid {
            "invalid"
        } else if self.slow {
            "slow"
        } else {
            "count"
        }
    }
    fn description(&self) -> &str {
        "Offline command"
    }
    fn execute(
        &self,
        args: &str,
        context: &ExtensionContext,
        signal: &CancellationToken,
    ) -> Result<CommandResult, String> {
        self.observed
            .lock()
            .unwrap()
            .push(json!({"args":args,"state":context.states.get("fixture")}));
        if self.slow {
            if let Some(sender) = &self.entered {
                sender.send(()).unwrap();
            }
            while !signal.is_cancelled() {
                thread::sleep(Duration::from_millis(2));
            }
            return Err("fixture cancelled".into());
        }
        if self.invalid {
            return Ok(CommandResult {
                messages: vec![Message::Assistant(AssistantMessage::text("forged"))],
                ..Default::default()
            });
        }
        let count = context
            .states
            .get("fixture")
            .and_then(|v| v["count"].as_u64())
            .unwrap_or(0)
            + 1;
        Ok(CommandResult {
            text: format!("count={count}; {args}"),
            state: json!({"count":count}),
            session_name: Some("Extension fixture".into()),
            ui: vec![UiRequest::Notification {
                level: NotificationLevel::Info,
                text: "UI only".into(),
            }],
            ..Default::default()
        })
    }
}
struct ExtensionFixture {
    commands: Vec<Arc<dyn ExtensionCommand>>,
    events: Arc<Mutex<Vec<String>>>,
}
impl Extension for ExtensionFixture {
    fn id(&self) -> &str {
        "fixture"
    }
    fn commands(&self) -> Vec<Arc<dyn ExtensionCommand>> {
        self.commands.clone()
    }
    fn on_event(
        &self,
        event: &LifecycleEvent,
        _: &ExtensionContext,
        _: &CancellationToken,
    ) -> Result<(), String> {
        self.events.lock().unwrap().push(format!("{event:?}"));
        Ok(())
    }
}
fn register(
    client: &Client,
    invalid: bool,
    slow: bool,
    entered: Option<mpsc::Sender<()>>,
) -> Arc<Mutex<Vec<serde_json::Value>>> {
    let observed = Arc::new(Mutex::new(vec![]));
    client
        .options
        .extensions
        .register(Arc::new(ExtensionFixture {
            commands: vec![Arc::new(CommandFixture {
                invalid,
                slow,
                entered,
                observed: observed.clone(),
            })],
            events: Arc::new(Mutex::new(vec![])),
        }))
        .unwrap();
    observed
}

#[test]
fn command_bypasses_model_preflight_and_restores_branch_state_without_assistant() {
    let fixture = Fixture::new();
    let mut client = fixture.client();
    let observed = register(&client, false, false, None);
    // Commands must work before credentials/model configuration, without a provider request.
    client.settings.mode = agent_runtime::provider::ProviderMode::OpenAiCompatible;
    client.settings.api_key.clear();
    client.settings.model.clear();
    let mut events = vec![];
    client
        .run(
            Message::user("/count $1 literal"),
            &CancellationToken::new(),
            &mut |e| events.push(e),
        )
        .unwrap();
    assert!(events.is_empty());
    assert!(client.session.context.messages.is_empty());
    assert_eq!(
        client.last_command.as_ref().unwrap().text,
        "count=1; $1 literal"
    );
    let leaf = client.session.get_leaf_id().unwrap().unwrap();
    client
        .run(
            Message::user("/count second"),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(
        client.session.runtime_state("extension:fixture").unwrap(),
        Some(json!({"count":2}))
    );
    client.session.navigate(Some(&leaf)).unwrap();
    client
        .run(
            Message::user("/count branch"),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(
        client.last_command.as_ref().unwrap().text,
        "count=2; branch"
    );
    assert_eq!(observed.lock().unwrap()[2]["state"], json!({"count":1}));
    assert_eq!(
        SessionStore::new(fixture.path.join("sessions"))
            .list()
            .unwrap()[0]
            .name,
        "Extension fixture"
    );
}
#[test]
fn invalid_command_is_transactional_and_does_not_save_forged_messages() {
    let fixture = Fixture::new();
    let mut client = fixture.client();
    register(&client, true, false, None);
    let before = client.session.clone();
    assert!(client
        .execute_command("invalid", "", &CancellationToken::new())
        .is_err());
    assert_eq!(
        serde_json::to_value(&client.session).unwrap(),
        serde_json::to_value(before).unwrap()
    );
    assert!(!fixture.path.join("sessions").exists());
}
#[test]
fn terminal_commands_complete_and_display_ui_only_text() {
    let fixture = Fixture::new();
    let client = fixture.client();
    register(&client, false, false, None);
    let mut host = TerminalClient::new(client);
    assert!(host.model.commands.contains(&"/count".into()));
    host.submit("/count native", CancellationToken::new())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while host.model.running {
        host.poll();
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(2));
    }
    assert!(host.model.messages.is_empty());
    assert!(host.model.status.contains("count=1; native"));
    assert!(host.model.status.contains("UI only"));
}
struct ChannelRead {
    receiver: mpsc::Receiver<Vec<u8>>,
    pending: std::io::Cursor<Vec<u8>>,
}
impl Read for ChannelRead {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let size = self.pending.read(buffer)?;
            if size > 0 {
                return Ok(size);
            }
            match self.receiver.recv() {
                Ok(bytes) => self.pending = std::io::Cursor::new(bytes),
                Err(_) => return Ok(0),
            }
        }
    }
}
#[test]
fn rpc_extension_command_is_abortable_and_does_not_report_compaction() {
    let fixture = Fixture::new();
    let client = fixture.client();
    let (entered_tx, entered) = mpsc::channel();
    register(&client, false, true, Some(entered_tx));
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut output = vec![];
        let result = run_rpc(
            BufReader::new(ChannelRead {
                receiver,
                pending: std::io::Cursor::new(vec![]),
            }),
            &mut output,
            client,
            &CancellationToken::new(),
        );
        (result, output)
    });
    sender
        .send(b"{\"type\":\"extension_command\",\"id\":\"slow\",\"name\":\"slow\"}\n".to_vec())
        .unwrap();
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    sender.send(b"{\"type\":\"get_state\",\"id\":\"state\"}\n{\"type\":\"abort\",\"id\":\"abort\"}\n{\"type\":\"shutdown\",\"id\":\"shutdown\"}\n".to_vec()).unwrap();
    drop(sender);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !worker.is_finished() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(2));
    }
    let (result, output) = worker.join().unwrap();
    result.unwrap();
    let records: Vec<serde_json::Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let state = records.iter().find(|v| v["id"] == "state").unwrap();
    assert_eq!(state["data"]["isExtensionRunning"], true);
    assert_eq!(state["data"]["isCompacting"], false);
    let result = records.iter().find(|v| v["id"] == "slow").unwrap();
    assert_eq!(result["success"], false);
    assert_eq!(
        result["error"],
        "Extension fixture command slow: fixture cancelled"
    );
    assert!(SessionStore::new(fixture.path.join("sessions"))
        .list()
        .unwrap()[0]
        .context
        .messages
        .is_empty());
}
#[test]
fn cloud_aliases_and_explicit_budget_use_canonical_identity() {
    let fixture = Fixture::new();
    let args = parse(vec![
        "--cwd".into(),
        fixture.path.to_string_lossy().into_owned(),
        "--provider".into(),
        "azure".into(),
        "--model".into(),
        "directory-model".into(),
        "--azure-resource".into(),
        "fixture".into(),
        "--azure-deployment".into(),
        "arbitrary-deployment".into(),
        "--context-window".into(),
        "65536".into(),
        "--max-output-tokens".into(),
        "2048".into(),
        "--no-context-files".into(),
        "--no-skills".into(),
        "--no-prompt-templates".into(),
    ])
    .unwrap();
    let client = Client::from_args(&args).unwrap();
    assert_eq!(client.provider_id, "azure-openai-responses");
    assert_eq!(
        client.settings.options.azure.deployment,
        "arbitrary-deployment"
    );
    let budget = client.options.automation.model.as_ref().unwrap();
    assert_eq!(budget.context_window, 65536);
    assert_eq!(budget.max_output_tokens, 2048);
    assert!(budget.cost.is_null());
}

#[test]
fn cloud_alias_model_switch_and_debug_never_expose_tokens() {
    let fixture = Fixture::new();
    let mut client = fixture.client();
    client.provider_id = "google-vertex".into();
    client.settings = agent_cli::client::settings_for("vertex", "gemini-2.5-flash").unwrap();
    client.settings.api_key = "offline-express-key".into();
    client.settings.options.vertex.project = "fixture-project".into();
    client
        .choose_model("vertex", "gemini-2.0-flash", &CancellationToken::new())
        .unwrap();
    assert_eq!(client.settings.options.vertex.project, "fixture-project");
    let args = parse(vec![
        "--azure-token".into(),
        "secret-azure-fixture".into(),
        "--vertex-token".into(),
        "secret-vertex-fixture".into(),
        "--api-key".into(),
        "secret-key-fixture".into(),
    ])
    .unwrap();
    let debug = format!("{args:?}");
    assert!(!debug.contains("secret"));
}
