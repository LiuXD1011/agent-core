//! Unified connection UI. All storage/discovery work runs on a cancellable worker.
use super::*;
use agent_runtime::connections::{self, Connection, ConnectionError, Service};

pub(super) struct ConnectionsState {
    pub open: bool,
    pub loaded: bool,
    service: Service,
    model: String,
    key: String,
    records: Vec<(Connection, bool)>,
    status: String,
    receiver: Option<Receiver<Result<ConnectionResult, ConnectionError>>>,
    worker: Option<JoinHandle<()>>,
    signal: CancellationToken,
}
impl Default for ConnectionsState {
    fn default() -> Self {
        Self {
            open: false,
            loaded: false,
            service: Service::DeepSeek,
            model: String::new(),
            key: String::new(),
            records: Service::ALL
                .into_iter()
                .map(|service| (Connection::bundled(service), false))
                .collect(),
            status: "选择服务，保存密钥或登录，然后选择模型。".into(),
            receiver: None,
            worker: None,
            signal: CancellationToken::new(),
        }
    }
}
pub(super) enum ConnectionJob {
    Load,
    SaveKey(String),
    Refresh,
    Select,
    Forget,
}
enum ConnectionResult {
    Loaded(Vec<(Connection, bool)>),
    Updated(Connection, bool, &'static str),
    Select(Connection),
    Forgotten(Service),
}

impl ConnectionsState {
    pub fn busy(&self) -> bool {
        self.receiver.is_some()
    }
    pub fn cancel(&self) {
        self.signal.cancel();
    }
    pub fn stop(&mut self) {
        self.cancel();
        self.receiver = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.key.clear();
    }
    fn current(&self) -> Connection {
        self.records
            .iter()
            .find(|(c, _)| c.service == self.service)
            .map(|(c, _)| c.clone())
            .unwrap_or_else(|| Connection::bundled(self.service))
    }
    fn choose(&mut self, service: Service) {
        self.service = service;
        self.key.clear();
        self.model = self.current().selected_model;
        self.status = "正在编辑此连接；当前会话尚未切换。".into();
    }
    fn update(&mut self, connection: Connection, present: bool) {
        if let Some(entry) = self
            .records
            .iter_mut()
            .find(|(c, _)| c.service == connection.service)
        {
            *entry = (connection, present);
        }
    }
}

impl DesktopApp {
    pub(super) fn start_connection_job(&mut self, ctx: &egui::Context, job: ConnectionJob) {
        if self.busy() || self.closing {
            return;
        }
        let service = self.connections.service;
        let mut connection = self.connections.current();
        connection.selected_model = self.connections.model.trim().into();
        let storage = AuthStorage::new(&self.auth_path);
        let (sender, receiver) = mpsc::channel();
        self.connections.signal = CancellationToken::new();
        let signal = self.connections.signal.clone();
        self.connections.receiver = Some(receiver);
        self.connections.status = "正在处理，可随时取消…".into();
        let repaint = ctx.clone();
        self.connections.worker = Some(std::thread::spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| match job {
                ConnectionJob::Load => {
                    let records = Service::ALL
                        .into_iter()
                        .map(|service| {
                            Ok((
                                connections::load_connection(&storage, service, &signal)?,
                                connections::has_credential(&storage, service, &signal)?,
                            ))
                        })
                        .collect::<Result<Vec<_>, ConnectionError>>()?;
                    Ok(ConnectionResult::Loaded(records))
                }
                ConnectionJob::SaveKey(key) => {
                    connections::save_key(&storage, service, &key, &signal)?;
                    Ok(ConnectionResult::Updated(
                        connection,
                        true,
                        "密钥已加密保存；尚未验证模型调用权限。",
                    ))
                }
                ConnectionJob::Refresh => {
                    let next = connections::refresh(&storage, &connection, &signal)?;
                    connections::save_connection(&storage, &next, &signal)?;
                    let present = connections::has_credential(&storage, service, &signal)?;
                    Ok(ConnectionResult::Updated(
                        next,
                        present,
                        if service.browser_login() {
                            "已重新载入内置 Codex 目录；这不是账号可用模型查询。"
                        } else {
                            "模型目录已刷新并缓存；当前模型保持不变。"
                        },
                    ))
                }
                ConnectionJob::Select => {
                    if !connections::has_credential(&storage, service, &signal)? {
                        return Err(ConnectionError::MissingKey);
                    }
                    if connection.selected_model.is_empty() {
                        return Err(ConnectionError::InvalidModels);
                    }
                    connections::save_connection(&storage, &connection, &signal)?;
                    Ok(ConnectionResult::Select(connection))
                }
                ConnectionJob::Forget => {
                    storage
                        .delete(service.id(), &signal)
                        .map_err(|_| ConnectionError::Storage)?;
                    Ok(ConnectionResult::Forgotten(service))
                }
            }))
            .unwrap_or(Err(ConnectionError::Storage));
            let _ = sender.send(result);
            repaint.request_repaint();
        }));
    }

    pub(super) fn poll_connections(&mut self, ctx: &egui::Context) {
        let received = self.connections.receiver.as_ref().map(|r| r.try_recv());
        let result = match received {
            Some(Ok(result)) => result,
            Some(Err(TryRecvError::Disconnected)) => Err(ConnectionError::Storage),
            _ => return,
        };
        self.connections.receiver = None;
        if let Some(worker) = self.connections.worker.take() {
            let _ = worker.join();
        }
        if self.connections.signal.is_cancelled() || self.closing {
            // A completed atomic key/cache write may already have committed. Reload before further actions.
            self.connections.loaded = false;
            self.connections.status = "操作已结束；未切换当前模型，请重新读取保存状态。".into();
            return;
        }
        match result {
            Ok(ConnectionResult::Loaded(records)) => {
                self.connections.records = records;
                self.connections.loaded = true;
                self.connections.model = self.connections.current().selected_model;
                self.connections.status = "已读取本地连接；未发起网络请求。".into();
            }
            Ok(ConnectionResult::Updated(connection, present, message)) => {
                self.connections.update(connection, present);
                self.connections.status = message.into();
            }
            Ok(ConnectionResult::Select(connection)) => {
                let mut configuration = ModelConfiguration {
                    settings: connection
                        .service
                        .settings(&connection.selected_model, &self.auth_path),
                    context_window: 0,
                    max_output_tokens: 0,
                };
                // Preserve injected/test Codex configuration, not any other provider's keys or headers.
                configuration.settings.options.codex_oauth =
                    self.settings.options.codex_oauth.clone();
                refresh_configuration_budget(&mut configuration);
                if configuration.context_window == 0 {
                    (
                        configuration.context_window,
                        configuration.max_output_tokens,
                    ) = connection.budget(&connection.selected_model);
                }
                self.connections.update(connection, true);
                self.connections.status = "连接已保存，正在应用；结果见下方当前会话状态。".into();
                self.start_model_selection(ctx, configuration, ModelSelectSource::Set);
            }
            Ok(ConnectionResult::Forgotten(service)) => {
                if let Some((_, present)) = self
                    .connections
                    .records
                    .iter_mut()
                    .find(|(c, _)| c.service == service)
                {
                    *present = false;
                }
                if self.provider_id() == service.id() {
                    self.settings.api_key.clear();
                    self.settings.options.oauth = None;
                    if let Some(draft) = &mut self.settings_draft {
                        draft.settings.api_key.clear();
                        draft.settings.options.oauth = None;
                    }
                }
                self.connections.status = "已移除保存的凭据；模型目录与会话保留。".into();
            }
            Err(error) => {
                self.connections.status = error.to_string();
            }
        }
    }

    pub(super) fn open_connections(&mut self, ctx: &egui::Context) {
        self.connections.open = true;
        if !self.connections.loaded {
            self.start_connection_job(ctx, ConnectionJob::Load);
        }
    }

    pub(super) fn connections_ui(&mut self, ctx: &egui::Context) {
        if !self.connections.open {
            return;
        }
        let mut open = self.connections.open;
        egui::Window::new("模型服务")
            .id(egui::Id::new("unified-connections"))
            .open(&mut open)
            .default_width(560.0)
            .max_height((ctx.screen_rect().height() - 96.0).max(240.0))
            .vscroll(true)
            .show(ctx, |ui| {
                ui.heading("连接你的模型账号");
                ui.label("选择厂商 → 保存 API Key 或账号登录 → 选择模型并切换");
                ui.add_space(8.0);
                ui.add_enabled_ui(!self.busy() && !self.closing, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        egui::ComboBox::from_id_salt("connection-service")
                            .selected_text(self.connections.service.label())
                            .show_ui(ui, |ui| {
                                for service in Service::ALL {
                                    if ui
                                        .selectable_label(
                                            self.connections.service == service,
                                            service.label(),
                                        )
                                        .clicked()
                                    {
                                        self.connections.choose(service);
                                    }
                                }
                            });
                        if ui.button("读取已保存连接").clicked() {
                            self.start_connection_job(ctx, ConnectionJob::Load);
                        }
                    });
                    let service = self.connections.service;
                    ui.small(format!("接口：{}", service.base_url()));
                    let present = self
                        .connections
                        .records
                        .iter()
                        .any(|(c, saved)| c.service == service && *saved);
                    ui.label(if present {
                        "已保存凭据（调用权限以服务端响应为准）"
                    } else {
                        "尚未读取或保存此连接的凭据"
                    });
                    if service.browser_login() {
                        ui.label("使用现有 ChatGPT / Codex 登录流程；与 OpenAI API Key 分开存储。");
                        if ui.button("浏览器登录 ChatGPT / Codex").clicked() {
                            self.auth_provider = CODEX_PROVIDER.into();
                            self.start_auth(ctx, AuthAction::Login);
                        }
                        if ui.button("读取账号状态").clicked() {
                            self.auth_provider = CODEX_PROVIDER.into();
                            self.start_auth(ctx, AuthAction::Status);
                        }
                    } else {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.connections.key)
                                .password(true)
                                .hint_text("粘贴 API Key；留空不替换已存密钥")
                                .desired_width(f32::INFINITY),
                        );
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(
                                    !self.connections.key.is_empty(),
                                    egui::Button::new("保存 API Key"),
                                )
                                .clicked()
                            {
                                let key = std::mem::take(&mut self.connections.key);
                                self.start_connection_job(
                                    ctx,
                                    ConnectionJob::SaveKey(key.trim().into()),
                                );
                            }
                            ui.hyperlink_to("前往官方获取密钥", service.key_url());
                        });
                        if service == Service::ZhipuCoding {
                            ui.small(
                                "Coding Plan 使用独立端点与密钥记录，额度适用工具以厂商说明为准。",
                            );
                        }
                        ui.small("此入口使用 API Key，不收集厂商账号密码。");
                    }
                    ui.separator();
                    let connection = self.connections.current();
                    ui.horizontal_wrapped(|ui| {
                        let label = if service.browser_login() {
                            "重新载入内置模型"
                        } else {
                            "刷新模型"
                        };
                        if ui.button(label).clicked() {
                            self.start_connection_job(ctx, ConnectionJob::Refresh);
                        }
                        egui::ComboBox::from_id_salt("connection-model")
                            .width(280.0)
                            .selected_text(if self.connections.model.is_empty() {
                                "选择模型"
                            } else {
                                &self.connections.model
                            })
                            .show_ui(ui, |ui| {
                                for model in &connection.models {
                                    ui.selectable_value(
                                        &mut self.connections.model,
                                        model.id.clone(),
                                        &model.id,
                                    );
                                }
                            });
                    });
                    ui.small(format!(
                        "{} · {} 个模型",
                        connection.source.label(),
                        connection.models.len()
                    ));
                    if let Some(at) = connection.refreshed_at {
                        ui.small(format!("上次刷新：Unix {} 秒", at / 1000));
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut self.connections.model)
                            .hint_text("也可填写厂商提供的新模型 ID")
                            .desired_width(f32::INFINITY),
                    );
                    if matches!(service, Service::Zhipu | Service::ZhipuCoding) {
                        ui.small(
                            "刷新公开元数据不发送密钥，也不验证账号额度；未知能力不自动推断。",
                        );
                    }
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                !self.connections.model.trim().is_empty() && present,
                                egui::Button::new("保存并切换到此模型"),
                            )
                            .clicked()
                        {
                            self.start_connection_job(ctx, ConnectionJob::Select);
                        }
                        if ui
                            .add_enabled(present, egui::Button::new("移除凭据 / 退出登录"))
                            .clicked()
                        {
                            self.start_connection_job(ctx, ConnectionJob::Forget);
                        }
                    });
                    ui.separator();
                    ui.label("快速切换 · 已保存连接");
                    let records = self.connections.records.clone();
                    for (record, saved) in records {
                        if saved && !record.selected_model.is_empty() {
                            let active = self.provider_id() == record.service.id()
                                && self.settings.model == record.selected_model;
                            if ui
                                .selectable_label(
                                    active,
                                    format!(
                                        "{}  /  {}",
                                        record.service.label(),
                                        record.selected_model
                                    ),
                                )
                                .clicked()
                            {
                                self.connections.choose(record.service);
                                self.start_connection_job(ctx, ConnectionJob::Select);
                            }
                        }
                    }
                });
                if self.connections.busy() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        if ui.button("取消连接操作").clicked() {
                            self.connections.cancel();
                        }
                    });
                }
                if self.auth_receiver.is_some() {
                    ui.label(&self.auth_status);
                    if let Some(url) = &self.login_url {
                        ui.hyperlink_to("在浏览器继续登录", url);
                    }
                    if ui.button("取消账号登录").clicked() {
                        self.auth_signal.cancel();
                    }
                } else if self.connections.service.browser_login() {
                    ui.label(&self.auth_status);
                }
                ui.separator();
                ui.label(&self.connections.status);
                ui.label(format!(
                    "当前会话：{} / {}",
                    self.provider_id(),
                    self.settings.model
                ));
                ui.label(&self.status);
                if ui.button("高级模型 / Thinking 设置").clicked() {
                    self.settings_open = true;
                }
            });
        if !open {
            self.connections.key.clear();
        }
        self.connections.open = open;
    }
}

#[cfg(test)]
#[path = "app_connections_tests.rs"]
mod tests;
