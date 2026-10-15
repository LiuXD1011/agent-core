//! Provider-specific native OAuth. Constants and credential semantics come from
//! the original project's five OAuth handlers, not the Codex JWT flow.
use super::{check_cancelled, pkce_challenge, random_hex, OAuthCredential};
use agent_core::types::{timestamp, CancellationToken};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct NativeOAuthConfig {
    pub provider: String,
    pub device_url: String,
    pub token_url: String,
    pub authorize_url: String,
    pub copilot_token_url: String,
    pub models_url: String,
    pub enterprise_domain: String,
    pub callback_bind: SocketAddr,
    pub redirect_uri: String,
    pub timeout: Duration,
    pub request_timeout: Duration,
    pub retry_delay: Duration,
}
impl NativeOAuthConfig {
    pub fn for_provider(provider: &str) -> Result<Self, String> {
        let mut config = Self {
            provider: provider.into(),
            device_url: String::new(),
            token_url: String::new(),
            authorize_url: String::new(),
            copilot_token_url: String::new(),
            models_url: String::new(),
            enterprise_domain: String::new(),
            callback_bind: "127.0.0.1:0".parse().expect("Static callback"),
            redirect_uri: String::new(),
            timeout: Duration::from_secs(900),
            request_timeout: Duration::from_secs(30),
            retry_delay: Duration::from_secs(1),
        };
        match provider {
            "github-copilot" => {
                config.device_url = "https://github.com/login/device/code".into();
                config.token_url = "https://github.com/login/oauth/access_token".into();
                config.copilot_token_url =
                    "https://api.github.com/copilot_internal/v2/token".into();
            }
            "kimi-coding" => {
                config.device_url = "https://auth.kimi.com/api/oauth/device_authorization".into();
                config.token_url = "https://auth.kimi.com/api/oauth/token".into();
            }
            "xai" => {
                config.device_url = "https://auth.x.ai/oauth2/device/code".into();
                config.token_url = "https://auth.x.ai/oauth2/token".into();
            }
            "anthropic" => {
                config.authorize_url = "https://claude.ai/oauth/authorize".into();
                config.token_url = "https://platform.claude.com/v1/oauth/token".into();
                config.callback_bind = "127.0.0.1:53692".parse().expect("Static callback");
                config.redirect_uri = "http://localhost:53692/callback".into();
            }
            "openrouter" => {
                config.authorize_url = "https://openrouter.ai/auth".into();
                config.token_url = "https://openrouter.ai/api/v1/auth/keys".into();
                config.timeout = Duration::from_secs(300);
            }
            _ => return Err("Provider has no migrated native OAuth flow".into()),
        }
        Ok(config)
    }
    /// Explicit non-secret configuration only; no credential files are opened.
    pub fn with_enterprise_domain(mut self, domain: &str) -> Result<Self, String> {
        if self.provider != "github-copilot" {
            return Err("Enterprise domain is only supported for Copilot".into());
        }
        let parsed = reqwest::Url::parse(&if domain.contains("://") {
            domain.into()
        } else {
            format!("https://{domain}")
        })
        .map_err(|_| "Invalid Copilot enterprise domain")?;
        let domain = parsed
            .host_str()
            .ok_or("Invalid Copilot enterprise domain")?;
        if parsed.scheme() != "https"
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.port().is_some()
        {
            return Err("Invalid Copilot enterprise domain".into());
        }
        self.enterprise_domain = domain.into();
        self.device_url = format!("https://{domain}/login/device/code");
        self.token_url = format!("https://{domain}/login/oauth/access_token");
        self.copilot_token_url = format!("https://api.{domain}/copilot_internal/v2/token");
        Ok(self)
    }
}
#[derive(Clone, Debug)]
pub struct NativeOAuthClient {
    pub config: NativeOAuthConfig,
}
struct HttpResponse {
    status: u16,
    body: Value,
    retry_after: Option<String>,
}
fn endpoint(value: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(value).map_err(|_| "Invalid OAuth endpoint")?;
    let loopback = url
        .host_str()
        .is_some_and(|host| matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]"));
    if !(url.scheme() == "https" || url.scheme() == "http" && loopback)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("OAuth endpoint must use HTTPS or local loopback HTTP".into());
    }
    Ok(url)
}
fn text<'a>(body: &'a Value, key: &str) -> Result<&'a str, String> {
    body[key]
        .as_str()
        .filter(|value| !value.is_empty() && !value.bytes().any(|byte| byte.is_ascii_control()))
        .ok_or_else(|| format!("OAuth response field {key} is invalid"))
}
fn seconds(body: &Value, key: &str, fallback: Option<u64>) -> Result<u64, String> {
    body.get(key)
        .map(|value| {
            value
                .as_f64()
                .filter(|number| number.is_finite() && *number > 0.0 && *number <= 31_536_000.0)
                .map(|number| (number * 1000.0) as u64)
                .ok_or("OAuth expiry field is invalid".into())
        })
        .unwrap_or_else(|| {
            fallback
                .map(|seconds| seconds * 1000)
                .ok_or("OAuth expiry field is missing".into())
        })
}
fn wait(duration: Duration, signal: &CancellationToken) -> Result<(), String> {
    let start = Instant::now();
    while start.elapsed() < duration {
        check_cancelled(signal)?;
        std::thread::sleep((duration - start.elapsed()).min(Duration::from_millis(25)));
    }
    check_cancelled(signal)
}
fn form(fields: &[(&str, &str)]) -> String {
    let mut url = reqwest::Url::parse("https://fixture.invalid").expect("Static form");
    url.query_pairs_mut().extend_pairs(fields.iter().copied());
    url.query().unwrap_or("").into()
}
fn copilot_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("user-agent".into(), "GitHubCopilotChat/0.35.0".into()),
        ("editor-version".into(), "vscode/1.107.0".into()),
        ("editor-plugin-version".into(), "copilot-chat/0.35.0".into()),
        ("copilot-integration-id".into(), "vscode-chat".into()),
    ])
}
impl NativeOAuthClient {
    pub fn new(config: NativeOAuthConfig) -> Self {
        Self { config }
    }
    fn client_id(&self) -> &'static str {
        match self.config.provider.as_str() {
            "github-copilot" => "Iv1.b507a08c87ecfe98",
            "kimi-coding" => "17e5f671-d194-4dfb-9706-5516cb48c098",
            "xai" => "b1a00492-073a-47ea-816f-4c329264a828",
            "anthropic" => "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
            _ => "",
        }
    }
    fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        content_type: &str,
        body: String,
        headers: BTreeMap<String, String>,
        signal: &CancellationToken,
    ) -> Result<HttpResponse, String> {
        check_cancelled(signal)?;
        let url = endpoint(url)?;
        if self.config.request_timeout.is_zero() {
            return Err("Invalid OAuth request timeout".into());
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "OAuth runtime failed")?;
        let result = runtime.block_on(async {
            let builder = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .timeout(self.config.request_timeout);
            let builder = if url.scheme() == "http" {
                builder.no_proxy()
            } else {
                builder
            };
            let client = builder.build().map_err(|_| "OAuth HTTP client failed")?;
            let task = async {
                let mut request = client
                    .request(method, url)
                    .header("accept", "application/json");
                if !content_type.is_empty() {
                    request = request.header("content-type", content_type);
                }
                for (name, value) in headers {
                    request = request.header(name, value);
                }
                if !body.is_empty() {
                    request = request.body(body);
                }
                let mut response = request
                    .send()
                    .await
                    .map_err(|_| "OAuth transport request failed")?;
                let status = response.status().as_u16();
                let retry_after = response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let mut bytes = Vec::new();
                while let Some(chunk) = response
                    .chunk()
                    .await
                    .map_err(|_| "OAuth response read failed")?
                {
                    if bytes.len().saturating_add(chunk.len()) > 65536 {
                        return Err("OAuth response exceeds limit".into());
                    }
                    bytes.extend_from_slice(&chunk);
                }
                let body = if bytes.is_empty() {
                    json!({})
                } else {
                    match serde_json::from_slice::<Value>(&bytes) {
                        Ok(body) => body,
                        Err(_) if !(200..300).contains(&status) => json!({}),
                        Err(_) => {
                            return Err(format!(
                                "OAuth endpoint returned invalid JSON (HTTP {status})"
                            ))
                        }
                    }
                };
                if !body.is_object() {
                    return Err("OAuth response must be an object".into());
                }
                Ok(HttpResponse {
                    status,
                    body,
                    retry_after,
                })
            };
            let cancelled = async {
                while !signal.is_cancelled() {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            };
            tokio::select! {biased; result=task=>result,_=cancelled=>Err("Operation aborted".into())}
        });
        result
    }
    fn post_form(
        &self,
        url: &str,
        fields: &[(&str, &str)],
        signal: &CancellationToken,
    ) -> Result<HttpResponse, String> {
        let headers = if self.config.provider == "github-copilot" {
            copilot_headers()
        } else {
            BTreeMap::new()
        };
        self.request(
            reqwest::Method::POST,
            url,
            "application/x-www-form-urlencoded",
            form(fields),
            headers,
            signal,
        )
    }
    fn post_json(
        &self,
        url: &str,
        body: Value,
        signal: &CancellationToken,
    ) -> Result<HttpResponse, String> {
        self.request(
            reqwest::Method::POST,
            url,
            "application/json",
            body.to_string(),
            BTreeMap::new(),
            signal,
        )
    }
    fn success(response: HttpResponse) -> Result<Value, String> {
        if !(200..300).contains(&response.status) {
            return Err(format!("OAuth endpoint returned HTTP {}", response.status));
        }
        Ok(response.body)
    }
    fn credential(
        &self,
        body: &Value,
        prior: Option<&OAuthCredential>,
    ) -> Result<OAuthCredential, String> {
        let openrouter = self.config.provider == "openrouter";
        let access = text(body, if openrouter { "key" } else { "access_token" })?;
        let refresh = if openrouter {
            String::new()
        } else if self.config.provider == "xai" && body.get("refresh_token").is_none() {
            prior
                .map(|prior| prior.refresh.clone())
                .ok_or("OAuth response has no refresh token")?
        } else {
            text(body, "refresh_token")?.into()
        };
        let skew = if matches!(self.config.provider.as_str(), "xai" | "anthropic") {
            300000
        } else {
            0
        };
        let expires = if openrouter {
            9007199254740991
        } else {
            timestamp()
                .checked_add(seconds(
                    body,
                    "expires_in",
                    (self.config.provider == "xai").then_some(3600),
                )?)
                .ok_or("OAuth expiry overflow")?
                .saturating_sub(skew)
        };
        let credential = OAuthCredential {
            credential_type: "oauth".into(),
            access: access.into(),
            refresh,
            expires,
            account_id: String::new(),
            extra: prior.map(|prior| prior.extra.clone()).unwrap_or_default(),
        };
        credential.validate_for_provider(&self.config.provider)?;
        Ok(credential)
    }
    pub fn begin_device(&self, signal: &CancellationToken) -> Result<DeviceLogin, String> {
        if !matches!(
            self.config.provider.as_str(),
            "github-copilot" | "kimi-coding" | "xai"
        ) {
            return Err("Provider does not use device authorization".into());
        }
        let mut fields = vec![("client_id", self.client_id())];
        if self.config.provider == "github-copilot" {
            fields.push(("scope", "read:user"));
        }
        if self.config.provider == "xai" {
            fields.extend([
                (
                    "scope",
                    "openid profile email offline_access grok-cli:access api:access",
                ),
                ("referrer", "pi"),
            ]);
        }
        let body = Self::success(self.post_form(&self.config.device_url, &fields, signal)?)?;
        let uri = body["verification_uri_complete"]
            .as_str()
            .filter(|value| !value.is_empty())
            .unwrap_or(text(&body, "verification_uri")?);
        let url = reqwest::Url::parse(uri).map_err(|_| "Untrusted OAuth verification URI")?;
        if !matches!(url.scheme(), "http" | "https")
            || self.config.provider == "xai" && url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err("Untrusted OAuth verification URI".into());
        }
        if self.config.provider == "kimi-coding" {
            text(&body, "verification_uri_complete")?;
        }
        let interval = body["interval"]
            .as_f64()
            .filter(|number| number.is_finite() && *number > 0.0 && *number < 3600.0)
            .unwrap_or(5.0);
        let lifetime = seconds(
            &body,
            "expires_in",
            (self.config.provider == "kimi-coding").then_some(900),
        )?;
        Ok(DeviceLogin {
            user_code: text(&body, "user_code")?.into(),
            verification_uri: url.into(),
            interval_seconds: interval,
            expires_in_seconds: lifetime as f64 / 1000.0,
            device_code: text(&body, "device_code")?.into(),
            client: self.clone(),
            started: Instant::now(),
        })
    }
    pub fn refresh(
        &self,
        credential: &OAuthCredential,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential, String> {
        check_cancelled(signal)?;
        credential.validate_for_provider(&self.config.provider)?;
        if self.config.provider == "github-copilot" && self.config.enterprise_domain.is_empty() {
            if let Some(domain) = credential
                .extra
                .get("enterpriseUrl")
                .and_then(Value::as_str)
                .filter(|domain| !domain.is_empty())
            {
                let mut config = self.config.clone();
                if config.copilot_token_url == "https://api.github.com/copilot_internal/v2/token" {
                    config = config.with_enterprise_domain(domain)?;
                } else {
                    config.enterprise_domain = domain.into();
                }
                return Self::new(config).refresh(credential, signal);
            }
        }
        if self.config.provider == "openrouter" {
            return Ok(credential.clone());
        }
        if self.config.provider == "github-copilot" {
            let mut result = self.copilot_exchange(&credential.refresh, signal)?;
            result.extra.extend(credential.extra.clone());
            self.copilot_models(&mut result, false, signal)?;
            return Ok(result);
        }
        let attempts = if self.config.provider == "kimi-coding" {
            4
        } else {
            1
        };
        let mut last = String::new();
        for attempt in 0..attempts {
            if attempt > 0 {
                wait(
                    self.config.retry_delay.saturating_mul(1 << (attempt - 1)),
                    signal,
                )?;
            }
            let response = if self.config.provider == "anthropic" {
                self.post_json(&self.config.token_url,json!({"grant_type":"refresh_token","client_id":self.client_id(),"refresh_token":credential.refresh}),signal)
            } else {
                self.post_form(
                    &self.config.token_url,
                    &[
                        ("grant_type", "refresh_token"),
                        ("client_id", self.client_id()),
                        ("refresh_token", &credential.refresh),
                    ],
                    signal,
                )
            };
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    check_cancelled(signal)?;
                    last = error;
                    if attempt + 1 < attempts {
                        continue;
                    }
                    return Err(last);
                }
            };
            if (200..300).contains(&response.status) {
                return self.credential(&response.body, Some(credential));
            }
            if self.config.provider == "kimi-coding"
                && (response.status == 429 || response.status >= 500)
                && attempt + 1 < attempts
            {
                last = format!("OAuth endpoint returned HTTP {}", response.status);
                continue;
            }
            return Err(format!(
                "OAuth refresh endpoint returned HTTP {}",
                response.status
            ));
        }
        Err(last)
    }
    fn copilot_exchange(
        &self,
        github_token: &str,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential, String> {
        let mut headers = copilot_headers();
        headers.insert("authorization".into(), format!("Bearer {github_token}"));
        let body = Self::success(self.request(
            reqwest::Method::GET,
            &self.config.copilot_token_url,
            "",
            String::new(),
            headers,
            signal,
        )?)?;
        let expires = body["expires_at"]
            .as_u64()
            .filter(|value| *value > 0)
            .and_then(|value| value.checked_mul(1000))
            .ok_or("Invalid Copilot token expiry")?
            .saturating_sub(300000);
        let mut extra = BTreeMap::new();
        if !self.config.enterprise_domain.is_empty() {
            extra.insert("enterpriseUrl".into(), json!(self.config.enterprise_domain));
        }
        let credential = OAuthCredential {
            credential_type: "oauth".into(),
            access: text(&body, "token")?.into(),
            refresh: github_token.into(),
            expires,
            account_id: String::new(),
            extra,
        };
        credential.validate_for_provider("github-copilot")?;
        Ok(credential)
    }
    fn copilot_models(
        &self,
        credential: &mut OAuthCredential,
        enable: bool,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        let base = crate::provider::CopilotOptions {
            enterprise_domain: self.config.enterprise_domain.clone(),
            base_url: String::new(),
        }
        .resolve_base(
            "https://api.individual.githubcopilot.com",
            &credential.access,
        )?;
        let url = if self.config.models_url.is_empty() {
            format!("{}/models", base.as_str().trim_end_matches('/'))
        } else {
            self.config.models_url.clone()
        };
        let mut headers = copilot_headers();
        headers.insert(
            "authorization".into(),
            format!("Bearer {}", credential.access),
        );
        headers.insert("x-github-api-version".into(), "2026-06-01".into());
        let mut response = self.request(
            reqwest::Method::GET,
            &url,
            "",
            String::new(),
            headers.clone(),
            signal,
        )?;
        if enable {
            for retry in 0..2 {
                if response.status != 429 {
                    break;
                }
                let delay = response
                    .retry_after
                    .as_deref()
                    .and_then(|value| value.parse::<f64>().ok())
                    .filter(|value| value.is_finite() && *value >= 0.0)
                    .map(Duration::from_secs_f64)
                    .unwrap_or(Duration::from_millis(500 * (1 << retry)));
                if delay >= Duration::from_secs(5) {
                    break;
                }
                wait(delay, signal)?;
                response = self.request(
                    reqwest::Method::GET,
                    &url,
                    "",
                    String::new(),
                    headers.clone(),
                    signal,
                )?;
            }
        }
        let body = Self::success(response)?;
        let data = body["data"]
            .as_array()
            .ok_or("Invalid Copilot model catalog")?;
        let rows: Vec<_> = data
            .iter()
            .filter(|row| {
                row["id"].is_string() && row["capabilities"]["supports"]["tool_calls"] != false
            })
            .collect();
        let picker: Vec<String> = rows
            .iter()
            .filter(|row| {
                row["model_picker_enabled"] == true && row["policy"]["state"] != "disabled"
            })
            .map(|row| row["id"].as_str().expect("Filtered id").into())
            .collect();
        let fallback =
            picker.is_empty() && base.host_str() == Some("api.individual.githubcopilot.com");
        let mut available = if fallback {
            rows.iter()
                .filter(|row| row["policy"]["state"] == "enabled")
                .map(|row| row["id"].as_str().expect("Filtered id").to_owned())
                .collect()
        } else {
            picker
        };
        if enable {
            for row in &rows {
                let id = row["id"].as_str().expect("Filtered id");
                if row["policy"]["state"] != "unconfigured"
                    || !(row["model_picker_enabled"] == true || fallback)
                    || crate::catalog::find("github-copilot", id).is_none()
                {
                    continue;
                }
                let mut policy = endpoint(&url)?;
                policy
                    .path_segments_mut()
                    .map_err(|_| "Invalid Copilot model route")?
                    .push(id)
                    .push("policy");
                let mut headers = headers.clone();
                headers.remove("x-github-api-version");
                headers.insert("openai-intent".into(), "chat-policy".into());
                headers.insert("x-interaction-type".into(), "chat-policy".into());
                let started = Instant::now();
                let mut response = self.request(
                    reqwest::Method::POST,
                    policy.as_str(),
                    "application/json",
                    json!({"state":"enabled"}).to_string(),
                    headers.clone(),
                    signal,
                )?;
                for retry in 0..2 {
                    if response.status != 429 {
                        break;
                    }
                    let delay = response
                        .retry_after
                        .as_deref()
                        .and_then(|value| value.parse::<f64>().ok())
                        .filter(|value| value.is_finite() && *value >= 0.0)
                        .map(Duration::from_secs_f64)
                        .unwrap_or(Duration::from_millis(500 * (1 << retry)));
                    if started.elapsed() + delay >= Duration::from_secs(5) {
                        break;
                    }
                    wait(delay, signal)?;
                    response = self.request(
                        reqwest::Method::POST,
                        policy.as_str(),
                        "application/json",
                        json!({"state":"enabled"}).to_string(),
                        headers.clone(),
                        signal,
                    )?;
                }
                if response.status == 429 {
                    break;
                }
                if (200..300).contains(&response.status) {
                    available.push(id.into());
                }
            }
        }
        available.sort();
        available.dedup();
        credential
            .extra
            .insert("availableModelIds".into(), json!(available));
        Ok(())
    }
    pub fn begin_pkce(&self) -> Result<PkceLogin, String> {
        if !matches!(self.config.provider.as_str(), "anthropic" | "openrouter") {
            return Err("Provider does not use PKCE callback authorization".into());
        }
        if !self.config.callback_bind.ip().is_loopback() || self.config.timeout.is_zero() {
            return Err("Invalid OAuth callback binding".into());
        }
        let mut authorize = endpoint(&self.config.authorize_url)?;
        endpoint(&self.config.token_url)?;
        let listener = TcpListener::bind(self.config.callback_bind)
            .map_err(|_| "OAuth callback port is unavailable")?;
        listener
            .set_nonblocking(true)
            .map_err(|_| "OAuth callback setup failed")?;
        let address = listener
            .local_addr()
            .map_err(|_| "OAuth callback address unavailable")?;
        let mut bytes = [0; 32];
        getrandom::fill(&mut bytes).map_err(|_| "Operating system random source failed")?;
        let verifier = URL_SAFE_NO_PAD.encode(bytes);
        let redirect_uri = if self.config.provider == "openrouter" {
            format!("http://{address}/oauth/callback/{}", random_hex()?)
        } else {
            let mut redirect = reqwest::Url::parse(&self.config.redirect_uri)
                .map_err(|_| "Invalid OAuth redirect URI")?;
            if redirect.scheme() != "http"
                || !redirect
                    .host_str()
                    .is_some_and(|host| matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]"))
                || redirect.path() != "/callback"
                || redirect.query().is_some()
                || redirect.fragment().is_some()
                || !redirect.username().is_empty()
                || redirect.password().is_some()
            {
                return Err("Invalid Anthropic OAuth redirect URI".into());
            }
            if redirect.port() == Some(0) {
                redirect
                    .set_port(Some(address.port()))
                    .map_err(|_| "Invalid callback port")?;
            }
            if redirect.port_or_known_default() != Some(address.port()) {
                return Err("OAuth callback and redirect ports differ".into());
            }
            redirect.into()
        };
        if self.config.provider == "anthropic" {
            authorize.query_pairs_mut().extend_pairs([("code","true"),("client_id",self.client_id()),("response_type","code"),("redirect_uri",redirect_uri.as_str()),("scope","org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload"),("code_challenge",pkce_challenge(&verifier).as_str()),("code_challenge_method","S256"),("state",verifier.as_str())]);
        } else {
            authorize.query_pairs_mut().extend_pairs([
                ("callback_url", redirect_uri.as_str()),
                ("code_challenge", pkce_challenge(&verifier).as_str()),
                ("code_challenge_method", "S256"),
            ]);
        }
        Ok(PkceLogin {
            authorization_url: authorize.into(),
            redirect_uri,
            listener,
            verifier,
            client: self.clone(),
        })
    }
}
pub struct DeviceLogin {
    pub user_code: String,
    pub verification_uri: String,
    pub interval_seconds: f64,
    pub expires_in_seconds: f64,
    device_code: String,
    client: NativeOAuthClient,
    started: Instant,
}
impl DeviceLogin {
    pub fn finish(self, signal: &CancellationToken) -> Result<OAuthCredential, String> {
        let lifetime =
            Duration::from_secs_f64(self.expires_in_seconds).min(self.client.config.timeout);
        let mut interval =
            Duration::from_secs_f64(self.interval_seconds).max(Duration::from_secs(1));
        wait(
            interval.min(lifetime.saturating_sub(self.started.elapsed())),
            signal,
        )?;
        while self.started.elapsed() < lifetime {
            let response = self.client.post_form(
                &self.client.config.token_url,
                &[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("client_id", self.client.client_id()),
                    ("device_code", &self.device_code),
                ],
                signal,
            )?;
            if (200..300).contains(&response.status) && response.body["access_token"].is_string() {
                if self.client.config.provider == "github-copilot" {
                    let mut credential = self
                        .client
                        .copilot_exchange(text(&response.body, "access_token")?, signal)?;
                    self.client.copilot_models(&mut credential, true, signal)?;
                    return Ok(credential);
                }
                return self.client.credential(&response.body, None);
            }
            match response.body["error"].as_str() {
                Some("authorization_pending") => {}
                Some("slow_down") => {
                    interval = response.body["interval"]
                        .as_f64()
                        .filter(|value| value.is_finite() && *value > 0.0 && *value < 3600.0)
                        .map(Duration::from_secs_f64)
                        .unwrap_or(interval + Duration::from_secs(5))
                        .max(Duration::from_secs(1));
                }
                Some("access_denied" | "authorization_denied") => {
                    return Err("OAuth device authorization denied".into())
                }
                Some("expired_token") => return Err("OAuth device code expired".into()),
                _ => {
                    return Err(format!(
                        "OAuth device token endpoint returned HTTP {}",
                        response.status
                    ))
                }
            }
            wait(
                interval.min(lifetime.saturating_sub(self.started.elapsed())),
                signal,
            )?;
        }
        Err("OAuth device flow timed out".into())
    }
}
pub struct PkceLogin {
    pub authorization_url: String,
    pub redirect_uri: String,
    listener: TcpListener,
    verifier: String,
    client: NativeOAuthClient,
}
impl PkceLogin {
    pub fn callback_addr(&self) -> Result<SocketAddr, String> {
        self.listener
            .local_addr()
            .map_err(|_| "OAuth callback address unavailable".into())
    }
    fn exchange(
        &self,
        input: &str,
        require_state: bool,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential, String> {
        check_cancelled(signal)?;
        let input = input.trim();
        let parsed = reqwest::Url::parse(input).ok();
        let pairs = parsed
            .as_ref()
            .map(|url| {
                url.query_pairs()
                    .map(|(key, value)| (key.into_owned(), value.into_owned()))
                    .collect::<Vec<_>>()
            })
            .or_else(|| {
                input.contains("code=").then(|| {
                    reqwest::Url::parse(&format!("http://localhost/?{input}"))
                        .expect("URL parser encodes query")
                        .query_pairs()
                        .map(|(key, value)| (key.into_owned(), value.into_owned()))
                        .collect()
                })
            });
        let (code, state) = if let Some(pairs) = pairs {
            if pairs.iter().any(|(key, _)| key == "error") {
                return Err("OAuth authorization was denied".into());
            }
            let codes: Vec<_> = pairs.iter().filter(|(key, _)| key == "code").collect();
            let states: Vec<_> = pairs.iter().filter(|(key, _)| key == "state").collect();
            if codes.len() != 1 || states.len() > 1 {
                return Err("Invalid OAuth authorization response".into());
            }
            (
                codes[0].1.clone(),
                states.first().map(|pair| pair.1.clone()),
            )
        } else if let Some((code, state)) = input.split_once('#') {
            (code.into(), Some(state.into()))
        } else {
            (input.into(), None)
        };
        if code.is_empty() || code.len() > 16384 || code.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err("Missing or invalid authorization code".into());
        }
        if self.client.config.provider == "anthropic"
            && (state.as_ref().is_some_and(|state| state != &self.verifier)
                || require_state && state.is_none())
        {
            return Err("OAuth state mismatch".into());
        }
        let body = if self.client.config.provider == "anthropic" {
            json!({"grant_type":"authorization_code","client_id":self.client.client_id(),"code":code,"state":self.verifier,"redirect_uri":self.redirect_uri,"code_verifier":self.verifier})
        } else {
            json!({"code":code,"code_verifier":self.verifier,"code_challenge_method":"S256"})
        };
        let token = NativeOAuthClient::success(self.client.post_json(
            &self.client.config.token_url,
            body,
            signal,
        )?)?;
        self.client.credential(&token, None)
    }
    pub fn finish(
        self,
        manual: Option<&str>,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential, String> {
        if let Some(input) = manual {
            return self.exchange(input, false, signal);
        }
        self.finish_with_manual(None, signal)
    }
    /// One worker owns the exchange, so manual input and a browser callback never
    /// double-exchange a one-use code. A caller can abort either path.
    pub fn finish_with_manual(
        self,
        manual: Option<&Receiver<String>>,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential, String> {
        let started = Instant::now();
        let path = reqwest::Url::parse(&self.redirect_uri)
            .expect("Validated redirect")
            .path()
            .to_owned();
        loop {
            check_cancelled(signal)?;
            if started.elapsed() >= self.client.config.timeout {
                return Err("OAuth callback timed out".into());
            }
            if let Some(input) = manual.and_then(|manual| manual.try_recv().ok()) {
                return self.exchange(&input, false, signal);
            }
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    if let Some(target) = callback_target(&mut stream, signal)? {
                        let url = reqwest::Url::parse(&format!("http://localhost{target}"))
                            .map_err(|_| "Invalid OAuth callback request")?;
                        if url.path() != path {
                            write_callback(&mut stream, 404)?;
                            continue;
                        }
                        let params: Vec<_> = url.query_pairs().collect();
                        if self.client.config.provider == "openrouter"
                            && params.iter().any(|(key, _)| key == "error")
                        {
                            write_callback(&mut stream, 400)?;
                            return Err("OAuth authorization was denied".into());
                        }
                        let states: Vec<_> =
                            params.iter().filter(|(key, _)| key == "state").collect();
                        let codes: Vec<_> =
                            params.iter().filter(|(key, _)| key == "code").collect();
                        if codes.len() != 1
                            || self.client.config.provider == "anthropic"
                                && (states.len() != 1
                                    || states[0].1 != self.verifier
                                    || params.iter().any(|(key, _)| key == "error"))
                        {
                            write_callback(&mut stream, 400)?;
                            continue;
                        }
                        let result = self.exchange(
                            url.as_str(),
                            self.client.config.provider == "anthropic",
                            signal,
                        );
                        write_callback(&mut stream, if result.is_ok() { 200 } else { 400 })?;
                        return result;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(_) => return Err("OAuth callback listener failed".into()),
            }
        }
    }
}
fn callback_target(
    stream: &mut TcpStream,
    signal: &CancellationToken,
) -> Result<Option<String>, String> {
    stream
        .set_nonblocking(false)
        .map_err(|_| "Callback socket setup failed")?;
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .map_err(|_| "Callback socket setup failed")?;
    stream
        .set_write_timeout(Some(Duration::from_millis(100)))
        .map_err(|_| "Callback socket setup failed")?;
    let mut bytes = Vec::new();
    let start = Instant::now();
    while !bytes.ends_with(b"\r\n\r\n") {
        check_cancelled(signal)?;
        if bytes.len() >= 16384 || start.elapsed() > Duration::from_secs(2) {
            return Ok(None);
        }
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(0) => return Ok(None),
            Ok(_) => bytes.push(byte[0]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(_) => return Err("Callback HTTP read failed".into()),
        }
    }
    let request = std::str::from_utf8(&bytes).map_err(|_| "Invalid callback encoding")?;
    let mut parts = request.lines().next().unwrap_or("").split_whitespace();
    if parts.next() != Some("GET") {
        write_callback(stream, 400)?;
        return Ok(None);
    }
    Ok(parts
        .next()
        .filter(|path| path.starts_with('/'))
        .map(str::to_owned))
}
fn write_callback(stream: &mut TcpStream, status: u16) -> Result<(), String> {
    let (text, status) = if status == 200 {
        (
            "Authorization completed. Return to the application.",
            "200 OK",
        )
    } else if status == 404 {
        ("Callback route not found.", "404 Not Found")
    } else {
        ("Authorization failed.", "400 Bad Request")
    };
    write!(stream,"HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",text.len()).map_err(|_|"Callback HTTP write failed".into())
}
