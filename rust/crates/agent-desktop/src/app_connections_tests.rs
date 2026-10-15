use super::*;
use std::time::Instant;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("desktop-connections-{}", Session::default().id)))
    }
    fn app(&self, ctx: &egui::Context) -> DesktopApp {
        let mut app = DesktopApp::with_store(ctx, SessionStore::new(self.0.join("sessions")));
        // Avoid ambient credentials; session restoration is explicitly tested below.
        app.auth_path = self.0.join("auth/credentials.dat");
        app.settings.options.auth_storage_path = Some(app.auth_path.clone());
        app.settings.api_key.clear();
        app
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn settle(app: &mut DesktopApp, ctx: &egui::Context) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.busy() {
        app.poll(ctx);
        assert!(
            Instant::now() < deadline,
            "{} / {}",
            app.status,
            app.connections.status
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn unified_ui_is_offline_until_action_and_masks_key_in_both_themes() {
    let fixture = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = fixture.app(&ctx);
    app.connections.open = true;
    app.connections.key = "UNIQUE-FAUX-KEY".into();
    app.input = "unsent draft".into();
    let session = serde_json::to_value(&app.session).unwrap();
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        ctx.set_theme(theme);
        for service in Service::ALL {
            app.connections.service = service;
            for _ in 0..2 {
                let output = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(900.0, 640.0),
                        )),
                        ..Default::default()
                    },
                    |ctx| app.show(ctx),
                );
                let texts = output
                    .shapes
                    .iter()
                    .filter_map(|shape| match &shape.shape {
                        egui::Shape::Text(text) => Some(text.galley.text()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(!texts.contains("UNIQUE-FAUX-KEY"));
            }
            assert!(!app.busy());
        }
    }
    assert_eq!(serde_json::to_value(&app.session).unwrap(), session);
    assert_eq!(app.input, "unsent draft");
    assert!(!app.auth_path.exists());
    assert!(!app.allow_changes && !app.allow_commands);
}

#[test]
fn save_refresh_switch_and_reload_keep_credentials_out_of_sessions() {
    let fixture = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = fixture.app(&ctx);
    app.input = "keep draft".into();
    for service in [
        Service::DeepSeek,
        Service::Zhipu,
        Service::OpenAi,
        Service::ZhipuCoding,
    ] {
        app.connections.choose(service);
        app.start_connection_job(
            &ctx,
            ConnectionJob::SaveKey(format!("fixture-{}-secret", service.id())),
        );
        settle(&mut app, &ctx);
        assert!(app.connections.status.contains("加密保存"));
        app.connections.model = "new-model-not-in-snapshot".into();
        app.start_connection_job(&ctx, ConnectionJob::Select);
        settle(&mut app, &ctx);
        assert_eq!(app.provider_id(), service.id(), "{}", app.status);
        assert_eq!(app.settings.base_url, service.base_url());
        assert_eq!(app.settings.mode, service.mode());
        assert!(app.settings.api_key.is_empty());
        assert!(app.settings.options.oauth.is_none());
        assert!(app.settings.options.headers.is_empty());
        assert_eq!(app.input, "keep draft");
        let session = serde_json::to_string(&app.session).unwrap();
        assert!(!session.contains("secret"));
        assert!(!session.contains("credentials.dat"));
    }
    app.start_connection_job(&ctx, ConnectionJob::Load);
    settle(&mut app, &ctx);
    assert_eq!(
        app.connections
            .records
            .iter()
            .filter(|(_, saved)| *saved)
            .count(),
        4
    );
    // A newly discovered model restores its route without depending on the embedded snapshot.
    let restored = configuration_for_identity(
        &ModelConfiguration {
            settings: ProviderSettings::default(),
            context_window: 0,
            max_output_tokens: 0,
        },
        &ModelIdentity {
            provider: "zhipuai".into(),
            model: "new-model-not-in-snapshot".into(),
        },
    )
    .unwrap();
    assert_eq!(restored.settings.base_url, Service::Zhipu.base_url());
    assert_eq!(restored.settings.mode, ProviderMode::OpenAiCompatible);
    let storage = AuthStorage::new(&app.auth_path);
    assert_eq!(
        connections::load_connection(&storage, Service::OpenAi, &CancellationToken::new())
            .unwrap()
            .selected_model,
        "new-model-not-in-snapshot"
    );
}

#[test]
fn failed_save_missing_login_and_cancelled_selection_preserve_current_configuration() {
    let fixture = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = fixture.app(&ctx);
    app.connections.model = "manual-model".into();
    app.start_connection_job(&ctx, ConnectionJob::SaveKey("bad\nkey".into()));
    settle(&mut app, &ctx);
    assert!(app.connections.status.contains("无效字符"));
    app.start_connection_job(&ctx, ConnectionJob::Select);
    settle(&mut app, &ctx);
    assert_eq!(app.settings.mode, ProviderMode::Demo);
    app.start_connection_job(&ctx, ConnectionJob::SaveKey("fixture-key".into()));
    settle(&mut app, &ctx);
    app.start_connection_job(&ctx, ConnectionJob::Select);
    app.connections.cancel();
    settle(&mut app, &ctx);
    assert_eq!(app.settings.mode, ProviderMode::Demo);
    app.connections.choose(Service::Codex);
    app.connections.model = "codex-model".into();
    app.start_connection_job(&ctx, ConnectionJob::Select);
    settle(&mut app, &ctx);
    assert_eq!(app.settings.mode, ProviderMode::Demo);
}

#[test]
fn worker_blocks_switching_and_forgetting_preserves_other_services() {
    let fixture = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = fixture.app(&ctx);
    for service in [Service::DeepSeek, Service::OpenAi] {
        app.connections.choose(service);
        app.start_connection_job(&ctx, ConnectionJob::SaveKey("fixture-key".into()));
        settle(&mut app, &ctx);
    }
    app.connections.choose(Service::DeepSeek);
    app.running = true;
    app.start_connection_job(&ctx, ConnectionJob::Forget);
    assert!(!app.connections.busy());
    app.running = false;
    app.start_connection_job(&ctx, ConnectionJob::Forget);
    settle(&mut app, &ctx);
    let storage = AuthStorage::new(&app.auth_path);
    assert!(
        !connections::has_credential(&storage, Service::DeepSeek, &CancellationToken::new())
            .unwrap()
    );
    assert!(
        connections::has_credential(&storage, Service::OpenAi, &CancellationToken::new()).unwrap()
    );
}

#[test]
fn account_status_updates_unified_panel_and_switches_without_api_key() {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let fixture = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = fixture.app(&ctx);
    let access = format!("e30.{}.fixture-signature", URL_SAFE_NO_PAD.encode(serde_json::json!({"https://api.openai.com/auth":{"chatgpt_account_id":"fixture-account"}}).to_string()));
    let credential = OAuthCredential {
        credential_type: "oauth".into(),
        access,
        refresh: "fixture-refresh-secret".into(),
        expires: timestamp() + 3_600_000,
        account_id: "fixture-account".into(),
        extra: BTreeMap::new(),
    };
    AuthStorage::new(&app.auth_path)
        .write(
            CODEX_PROVIDER,
            serde_json::to_value(credential).unwrap(),
            &CancellationToken::new(),
        )
        .unwrap();
    app.connections.open = true;
    app.connections.choose(Service::Codex);
    app.auth_provider = CODEX_PROVIDER.into();
    app.start_auth(&ctx, AuthAction::Status);
    settle(&mut app, &ctx);
    assert!(app.connections.loaded);
    assert!(app
        .connections
        .records
        .iter()
        .any(|(c, present)| c.service == Service::Codex && *present));
    app.connections.model = "custom-codex-model".into();
    app.start_connection_job(&ctx, ConnectionJob::Select);
    settle(&mut app, &ctx);
    assert_eq!(app.settings.mode, ProviderMode::OpenAiCodex);
    assert!(app.settings.api_key.is_empty());
    assert!(app.settings.options.oauth.is_some());
    assert!(!serde_json::to_string(&app.session)
        .unwrap()
        .contains("fixture-refresh-secret"));
    app.start_connection_job(&ctx, ConnectionJob::Forget);
    settle(&mut app, &ctx);
    assert!(app.settings.options.oauth.is_none());
}

#[test]
fn sidebar_click_opens_unified_panel_without_submitting_a_prompt() {
    let fixture = Fixture::new();
    let ctx = egui::Context::default();
    let mut app = fixture.app(&ctx);
    app.input = "do not submit".into();
    let input = |events| egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1280.0, 840.0),
        )),
        events,
        ..Default::default()
    };
    let mut position = None;
    for _ in 0..2 {
        let output = ctx.run(input(vec![]), |ctx| app.show(ctx));
        position = output.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == "模型服务 · 添加 / 快速切换" => {
                Some(text.pos + text.galley.size() * 0.5)
            }
            _ => None,
        });
    }
    let position = position.expect("sidebar service action");
    for pressed in [true, false] {
        let _ = ctx.run(
            input(vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ]),
            |ctx| app.show(ctx),
        );
    }
    assert!(app.connections.open);
    settle(&mut app, &ctx);
    assert!(app.connections.loaded);
    assert_eq!(app.input, "do not submit");
    assert!(app.session.context.messages.is_empty());
}
