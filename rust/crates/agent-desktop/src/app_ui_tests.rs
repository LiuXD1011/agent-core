use super::*;
use agent_runtime::extensions::{UiDialogRequest, UiReply};
use serde_json::{json, Value};
use std::fs;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::sync::OnceLock;
use std::thread;
use std::time::Instant;

fn executable() -> &'static Path {
    static EXE: OnceLock<PathBuf> = OnceLock::new();
    EXE.get_or_init(|| {
        let path =
            std::env::temp_dir().join(format!("desktop-dialog-compiler-{}", Session::default().id));
        fs::create_dir(&path).unwrap();
        let output = path.join(if cfg!(windows) {
            "fixture.exe"
        } else {
            "fixture"
        });
        let compiler = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let mut compiler = std::process::Command::new(compiler);
        #[cfg(windows)]
        compiler.creation_flags(0x0800_0000);
        let result = compiler
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
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("desktop-dialog-{}", Session::default().id));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn manifest(&self) -> PathBuf {
        let path = self.0.join("extension.json");
        fs::write(&path,json!({"id":"fixture","executable":executable(),"args":["--ui-command","--ui-startup",format!("--record={}",self.0.join("requests").display())],"requestTimeoutMs":30000,"maxFrameBytes":1048576}).to_string()).unwrap();
        path
    }
    fn app(&self, ctx: &egui::Context) -> DesktopApp {
        let mut app = DesktopApp::with_store(ctx, SessionStore::new(self.0.join("sessions")));
        app.session.workspace = self.0.clone();
        app.workspace = self.0.to_string_lossy().into_owned();
        app.auth_path = self.0.join("auth/credentials.dat");
        app.settings.options.auth_storage_path = Some(app.auth_path.clone());
        app
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn frame(app: &mut DesktopApp, ctx: &egui::Context) {
    let _ = ctx.run(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 840.0),
            )),
            ..Default::default()
        },
        |ctx| app.show(ctx),
    );
}
fn dialog(app: &mut DesktopApp, ctx: &egui::Context) -> UiDialogRequest {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        frame(app, ctx);
        if let Some(request) = app.ui_dialogs.active_request() {
            return request.clone();
        }
        assert!(Instant::now() < deadline, "{}", app.status);
        thread::sleep(Duration::from_millis(5));
    }
}
fn drain(app: &mut DesktopApp, ctx: &egui::Context) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.busy() {
        frame(app, ctx);
        assert!(Instant::now() < deadline, "{}", app.status);
        thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn startup_and_command_show_real_desktop_modals_and_preserve_zero_message_session() {
    let fixture = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = fixture.app(&ctx);
    app.start_extension_action(&ctx, ExtensionAction::Load(vec![fixture.manifest()]));
    let request = dialog(&mut app, &ctx);
    assert_eq!(request.dialog.title(), "Extension startup input");
    assert!(app.busy());
    app.ui_dialogs
        .respond(
            &request.id,
            UiReply::Value {
                value: "desktop startup".into(),
            },
        )
        .unwrap();
    drain(&mut app, &ctx);
    assert_eq!(app.extensions.ids(), vec!["fixture"]);
    assert!(app.session.context.messages.is_empty());
    app.input = "/fixture_echo".into();
    app.start(&ctx);
    for reply in [
        UiReply::Value {
            value: "蓝🙂".into(),
        },
        UiReply::Confirmed { confirmed: true },
        UiReply::Value {
            value: "desktop typed🙂".into(),
        },
        UiReply::Value {
            value: "desktop\nmultiline🙂".into(),
        },
    ] {
        let request = dialog(&mut app, &ctx);
        assert!(app.running);
        assert!(app.session.context.messages.is_empty());
        app.ui_dialogs.respond(&request.id, reply).unwrap();
    }
    drain(&mut app, &ctx);
    assert!(app.session.context.messages.is_empty());
    let text = app
        .extension_display
        .iter()
        .find(|text| text.starts_with('['))
        .expect("command dialog result displayed");
    assert_eq!(
        serde_json::from_str::<Value>(text).unwrap(),
        json!([{"value":"蓝🙂"},{"confirmed":true},{"value":"desktop typed🙂"},{"value":"desktop\nmultiline🙂"}])
    );
    let records: Vec<Value> = fs::read_to_string(fixture.0.join("requests"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        records
            .iter()
            .filter(|frame| frame["type"] == "host_result")
            .count(),
        5
    );
    assert!(!records
        .iter()
        .any(|frame| frame["method"] == "input" || frame["method"] == "before_agent_start"));
}
#[test]
fn closing_desktop_with_pending_startup_dialog_cancels_and_joins_without_commit() {
    let fixture = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = fixture.app(&ctx);
    let before = serde_json::to_value(&app.session).unwrap();
    app.start_extension_action(&ctx, ExtensionAction::Load(vec![fixture.manifest()]));
    let request = dialog(&mut app, &ctx);
    let broker = app.ui_dialogs.broker();
    let start = Instant::now();
    app.stop_workers();
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(broker.pending_ids().is_empty());
    assert!(broker
        .respond(
            &request.id,
            UiReply::Value {
                value: "late".into()
            }
        )
        .is_err());
    assert_eq!(serde_json::to_value(&app.session).unwrap(), before);
    assert!(app.extensions.ids().is_empty());
    assert!(app.store.list().unwrap().is_empty());
}
