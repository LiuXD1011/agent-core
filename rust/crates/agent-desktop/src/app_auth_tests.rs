use super::*;
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Instant;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("desktop-native-auth-{}", Session::default().id));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn app(&self, ctx: &egui::Context) -> DesktopApp {
        let mut app = DesktopApp::with_store(ctx, SessionStore::new(self.0.join("sessions")));
        app.session = Session {
            workspace: self.0.clone(),
            ..Default::default()
        };
        app.workspace = self.0.to_string_lossy().into_owned();
        app.auth_path = self.0.join("auth").join("credentials.dat");
        app.settings.options.auth_storage_path = Some(app.auth_path.clone());
        app
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn poll(app: &mut DesktopApp, ctx: &egui::Context, done: impl Fn(&DesktopApp) -> bool) {
    let start = Instant::now();
    while !done(app) {
        app.poll_auth(ctx);
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "{}",
            app.auth_status
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn request(stream: &mut TcpStream) -> (String, String) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut bytes = vec![];
    let mut byte = [0];
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 32768);
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
    }
    let headers = String::from_utf8(bytes).unwrap();
    let size = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse::<usize>().unwrap())
        .unwrap_or(0);
    let mut body = vec![0; size];
    stream.read_exact(&mut body).unwrap();
    (headers, String::from_utf8(body).unwrap())
}
fn server(responses: Vec<Value>) -> (String, JoinHandle<Vec<(String, String)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let worker = std::thread::spawn(move || {
        let mut result = vec![];
        for body in responses {
            let start = Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(start.elapsed() < Duration::from_secs(8));
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            result.push(request(&mut stream));
            let body = body.to_string();
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
        result
    });
    (base, worker)
}
fn config(provider: &str, base: &str) -> NativeOAuthConfig {
    let mut config = NativeOAuthConfig::for_provider(provider).unwrap();
    config.authorize_url = format!("{base}/authorize");
    config.token_url = format!("{base}/token");
    config.device_url = format!("{base}/device");
    config.copilot_token_url = format!("{base}/copilot");
    config.models_url = format!("{base}/models");
    config.callback_bind = "127.0.0.1:0".parse().unwrap();
    config.redirect_uri = "http://localhost:0/callback".into();
    config.timeout = Duration::from_secs(5);
    config.request_timeout = Duration::from_secs(3);
    config
}
#[test]
fn five_provider_desktop_login_status_and_logout_use_the_selected_encrypted_store() {
    for provider in [
        "anthropic",
        "openrouter",
        "github-copilot",
        "kimi-coding",
        "xai",
    ] {
        let fixture = Fixture::new();
        let ctx = egui::Context::default();
        let mut app = fixture.app(&ctx);
        let before = serde_json::to_value(&app.session).unwrap();
        let storage = AuthStorage::new(&app.auth_path);
        storage
            .write(
                "unrelated",
                json!({"type":"api_key","key":"unrelated-private-key"}),
                &CancellationToken::new(),
            )
            .unwrap();
        let responses = match provider {
            "anthropic" => vec![
                json!({"access_token":"desktop-access","refresh_token":"desktop-refresh","expires_in":3600}),
            ],
            "openrouter" => vec![json!({"key":"desktop-permanent-key"})],
            "github-copilot" => vec![
                json!({"device_code":"private-device","user_code":"DESKTOP-CODE","verification_uri":"https://github.com/login/device","interval":1,"expires_in":60}),
                json!({"access_token":"private-github-token"}),
                json!({"token":"tid=desktop;proxy-ep=proxy.individual.githubcopilot.com","expires_at":timestamp()/1000+3600}),
                json!({"data":[]}),
            ],
            _ => vec![
                json!({"device_code":"private-device","user_code":"DESKTOP-CODE","verification_uri":"https://fixture.invalid/verify","verification_uri_complete":"https://fixture.invalid/verify?code=DESKTOP-CODE","interval":1,"expires_in":60}),
                json!({"access_token":"desktop-access","refresh_token":"desktop-refresh","expires_in":3600}),
            ],
        };
        let (base, worker) = server(responses);
        app.auth_provider = provider.into();
        app.settings.options.oauth_config = Some(config(provider, &base));
        app.start_auth(&ctx, AuthAction::Login);
        assert!(app.busy());
        poll(&mut app, &ctx, |app| app.login_url.is_some());
        if matches!(provider, "anthropic" | "openrouter") {
            assert!(app.auth_device_code.is_none());
            app.auth_manual_sender
                .as_ref()
                .unwrap()
                .send("manual-ui-code".into())
                .unwrap();
        } else {
            assert_eq!(app.auth_device_code.as_deref(), Some("DESKTOP-CODE"));
        }
        poll(&mut app, &ctx, |app| app.auth_receiver.is_none());
        assert!(app.auth_status.contains(provider));
        assert!(app.auth_status.contains("已登录"));
        assert!(app.auth_worker.is_none());
        assert!(app.auth_manual_sender.is_none());
        assert!(app.auth_manual.is_empty());
        assert!(app.auth_device_code.is_none());
        assert!(app.settings.options.oauth.is_none());
        for secret in [
            "desktop-access",
            "desktop-refresh",
            "desktop-permanent-key",
            "private-device",
            "private-github-token",
        ] {
            assert!(!app.auth_status.contains(secret));
            assert!(!serde_json::to_string(&app.session)
                .unwrap()
                .contains(secret));
        }
        assert_eq!(serde_json::to_value(&app.session).unwrap(), before);
        assert!(storage
            .read(provider, &CancellationToken::new())
            .unwrap()
            .is_some());
        assert!(app.store.list().unwrap().is_empty());
        #[cfg(windows)]
        {
            let bytes = fs::read(storage.path()).unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains("desktop-refresh"));
        }
        app.start_auth(&ctx, AuthAction::Status);
        poll(&mut app, &ctx, |app| app.auth_receiver.is_none());
        assert!(app.auth_status.contains("已登录"));
        app.start_auth(&ctx, AuthAction::Logout);
        poll(&mut app, &ctx, |app| app.auth_receiver.is_none());
        assert!(app.auth_status.contains("已退出"));
        assert!(storage
            .read(provider, &CancellationToken::new())
            .unwrap()
            .is_none());
        assert_eq!(
            storage
                .read("unrelated", &CancellationToken::new())
                .unwrap()
                .unwrap()["key"],
            "unrelated-private-key"
        );
        assert!(!worker.join().unwrap().is_empty());
    }
}
#[test]
fn native_pkce_cancel_releases_manual_sender_callback_and_preserves_prior_bytes() {
    let fixture = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = fixture.app(&ctx);
    app.auth_provider = "anthropic".into();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let mut configuration = config("anthropic", "http://127.0.0.1:1");
    configuration.callback_bind = address;
    configuration.redirect_uri = format!("http://localhost:{}/callback", address.port());
    app.settings.options.oauth_config = Some(configuration);
    let store = AuthStorage::new(&app.auth_path);
    store
        .write(
            "anthropic",
            json!({"type":"api_key","key":"prior-fixture"}),
            &CancellationToken::new(),
        )
        .unwrap();
    let before = fs::read(store.path()).unwrap();
    app.start_auth(&ctx, AuthAction::Login);
    poll(&mut app, &ctx, |app| app.login_url.is_some());
    app.auth_signal.cancel();
    poll(&mut app, &ctx, |app| app.auth_receiver.is_none());
    assert!(app.auth_status.contains("取消"));
    assert!(app.auth_worker.is_none());
    assert!(app.auth_manual_sender.is_none());
    assert!(app.auth_manual.is_empty());
    let rebound = TcpListener::bind(address).unwrap();
    drop(rebound);
    assert_eq!(fs::read(store.path()).unwrap(), before);
    assert!(app.session.context.messages.is_empty());
}
#[test]
fn api_key_status_is_provider_specific_and_does_not_misparse_as_codex_oauth() {
    let fixture = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = fixture.app(&ctx);
    app.auth_provider = "openrouter".into();
    let store = AuthStorage::new(&app.auth_path);
    store
        .write(
            "openrouter",
            json!({"type":"api_key","key":"status-private-key"}),
            &CancellationToken::new(),
        )
        .unwrap();
    app.start_auth(&ctx, AuthAction::Status);
    poll(&mut app, &ctx, |app| app.auth_receiver.is_none());
    assert!(app.auth_status.contains("API Key"));
    assert!(!app.auth_status.contains("status-private-key"));
    assert!(app.settings.options.oauth.is_none());
}
