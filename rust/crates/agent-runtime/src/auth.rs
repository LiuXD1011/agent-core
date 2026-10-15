//! OpenAI Codex browser OAuth. Wire constants mirror the original TypeScript flow.
//! No browser is launched and no credential is stored by this module.
use agent_core::types::{timestamp, CancellationToken};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

#[path = "auth_native.rs"]
mod native;
pub use native::{DeviceLogin, NativeOAuthClient, NativeOAuthConfig, PkceLogin};

pub const CODEX_PROVIDER: &str = "openai-codex";
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const CODEX_AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const CODEX_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

/// Intentionally does not implement Debug: access and refresh are secrets.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthCredential {
    #[serde(rename = "type")]
    pub credential_type: String,
    pub access: String,
    pub refresh: String,
    pub expires: u64,
    #[serde(default)]
    pub account_id: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl OAuthCredential {
    pub fn validate_for_provider(&self, provider: &str) -> Result<(), String> {
        if provider == CODEX_PROVIDER {
            return self.validate();
        }
        if !matches!(
            provider,
            "anthropic" | "github-copilot" | "kimi-coding" | "xai" | "openrouter"
        ) || self.credential_type != "oauth"
            || self.access.is_empty()
            || self.access.bytes().any(|byte| byte.is_ascii_control())
            || self.refresh.bytes().any(|byte| byte.is_ascii_control())
            || provider != "openrouter" && self.refresh.is_empty()
        {
            return Err("Invalid provider OAuth credential".into());
        }
        Ok(())
    }
    pub fn is_expired(&self) -> bool {
        self.expires <= timestamp()
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.credential_type != "oauth"
            || self.access.is_empty()
            || self.refresh.is_empty()
            || self.access.bytes().any(|byte| byte.is_ascii_control())
            || self.refresh.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err("Invalid OAuth credential".into());
        }
        let account = codex_account_id(&self.access)?;
        if !self.account_id.is_empty() && account != self.account_id {
            return Err("OAuth account identity does not match the token claim".into());
        }
        Ok(())
    }
}

/// The JWT is decoded only to select the account header, not to authenticate it.
pub fn codex_account_id(access: &str) -> Result<String, String> {
    let parts: Vec<_> = access.split('.').collect();
    if parts.len() != 3 {
        return Err("Failed to extract Codex account ID".into());
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(parts[1].trim_end_matches('='))
        .or_else(|_| STANDARD.decode(parts[1]))
        .map_err(|_| "Failed to extract Codex account ID")?;
    let claims: Value = serde_json::from_slice(&bytes).map_err(|_| "Invalid Codex token claims")?;
    let account = claims["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .filter(|account| {
            !account.is_empty() && account.bytes().all(|byte| byte.is_ascii_graphic())
        })
        .ok_or("Codex token has no valid account ID")?;
    Ok(account.to_owned())
}

#[derive(Clone, Debug)]
pub struct CodexOAuthConfig {
    pub authorize_url: String,
    pub token_url: String,
    pub redirect_uri: String,
    pub callback_bind: SocketAddr,
    pub client_id: String,
    pub originator: String,
    pub timeout: Duration,
}

impl Default for CodexOAuthConfig {
    fn default() -> Self {
        Self {
            authorize_url: CODEX_AUTH_URL.into(),
            token_url: CODEX_TOKEN_URL.into(),
            redirect_uri: CODEX_REDIRECT_URI.into(),
            callback_bind: "127.0.0.1:1455".parse().expect("Static callback address"),
            client_id: CODEX_CLIENT_ID.into(),
            originator: "pi".into(),
            timeout: Duration::from_secs(15 * 60),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CodexOAuthClient {
    pub config: CodexOAuthConfig,
}

pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub(crate) fn random_hex() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| "Operating system random source failed")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub(crate) fn check_cancelled(signal: &CancellationToken) -> Result<(), String> {
    if signal.is_cancelled() {
        Err("Operation aborted".into())
    } else {
        Ok(())
    }
}

fn endpoint(source: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(source).map_err(|_| "Invalid OAuth endpoint")?;
    let loopback = url
        .host_str()
        .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1"));
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback)
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("OAuth endpoint must use HTTPS or local loopback HTTP".into());
    }
    Ok(url)
}

impl CodexOAuthClient {
    pub fn new(config: CodexOAuthConfig) -> Self {
        Self { config }
    }

    pub fn begin_login(&self) -> Result<CodexLogin, String> {
        let mut authorize = endpoint(&self.config.authorize_url)?;
        endpoint(&self.config.token_url)?;
        if !self.config.callback_bind.ip().is_loopback() || self.config.timeout.is_zero() {
            return Err("Invalid OAuth callback binding or timeout".into());
        }
        let listener = TcpListener::bind(self.config.callback_bind)
            .map_err(|_| "OAuth callback port is unavailable")?;
        listener
            .set_nonblocking(true)
            .map_err(|_| "OAuth callback setup failed")?;
        let address = listener
            .local_addr()
            .map_err(|_| "OAuth callback address unavailable")?;
        let mut redirect = reqwest::Url::parse(&self.config.redirect_uri)
            .map_err(|_| "Invalid OAuth redirect URI")?;
        if redirect.scheme() != "http"
            || !redirect
                .host_str()
                .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1"))
            || redirect.path() != "/auth/callback"
            || redirect.query().is_some()
            || redirect.fragment().is_some()
            || !redirect.username().is_empty()
            || redirect.password().is_some()
        {
            return Err("OAuth redirect URI must be the local /auth/callback route".into());
        }
        if redirect.port() == Some(0) {
            redirect
                .set_port(Some(address.port()))
                .map_err(|_| "Invalid callback port")?;
        }
        if redirect.port_or_known_default() != Some(address.port()) {
            return Err("OAuth callback and redirect ports differ".into());
        }
        let mut verifier_bytes = [0u8; 32];
        getrandom::fill(&mut verifier_bytes)
            .map_err(|_| "Operating system random source failed")?;
        let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
        let state = random_hex()?;
        authorize.query_pairs_mut().extend_pairs([
            ("response_type", "code"),
            ("client_id", self.config.client_id.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("scope", "openid profile email offline_access"),
            ("code_challenge", pkce_challenge(&verifier).as_str()),
            ("code_challenge_method", "S256"),
            ("state", state.as_str()),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", self.config.originator.as_str()),
        ]);
        Ok(CodexLogin {
            authorization_url: authorize.into(),
            redirect_uri: redirect.into(),
            listener,
            state,
            verifier,
            client: self.clone(),
        })
    }

    pub fn refresh(
        &self,
        credential: &OAuthCredential,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential, String> {
        check_cancelled(signal)?;
        if credential.credential_type != "oauth" || credential.refresh.is_empty() {
            return Err("No refreshable Codex OAuth credential".into());
        }
        let mut refreshed = self.token_request(
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &credential.refresh),
                ("client_id", &self.config.client_id),
            ],
            signal,
        )?;
        refreshed.extra = credential.extra.clone();
        Ok(refreshed)
    }

    fn token_request(
        &self,
        fields: &[(&str, &str)],
        signal: &CancellationToken,
    ) -> Result<OAuthCredential, String> {
        check_cancelled(signal)?;
        let url = endpoint(&self.config.token_url)?;
        let mut serializer =
            reqwest::Url::parse("https://fixture.invalid").expect("Static form base");
        serializer
            .query_pairs_mut()
            .extend_pairs(fields.iter().copied());
        let body = serializer.query().unwrap_or("").to_owned();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "OAuth HTTP runtime failed")?;
        let token: Value = runtime.block_on(async {
            let builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(120));
            let client = if url.scheme() == "http" { builder.no_proxy() } else { builder }.build().map_err(|_| "OAuth HTTP client failed")?;
            let task = async {
                let mut response = client.post(url).header("content-type","application/x-www-form-urlencoded").body(body).send().await.map_err(|_| "OAuth token request failed")?;
                if !response.status().is_success() { return Err(format!("OAuth token endpoint returned HTTP {}",response.status().as_u16())); }
                let mut bytes = Vec::new();
                while let Some(chunk) = response.chunk().await.map_err(|_| "OAuth token response read failed")? {
                    if bytes.len().saturating_add(chunk.len()) > 65_536 { return Err("OAuth token response exceeds limit".into()); }
                    bytes.extend_from_slice(&chunk);
                }
                serde_json::from_slice(&bytes).map_err(|_| "OAuth token response is not valid JSON".to_owned())
            };
            let cancellation = async { while !signal.is_cancelled() { tokio::time::sleep(Duration::from_millis(25)).await; } };
            tokio::select! { biased; result = task => result, _ = cancellation => Err("Operation aborted".into()) }
        })?;
        let access = token["access_token"]
            .as_str()
            .filter(|token| !token.is_empty())
            .ok_or("OAuth token response has no access token")?;
        let refresh = token["refresh_token"]
            .as_str()
            .filter(|token| !token.is_empty())
            .ok_or("OAuth token response has no refresh token")?;
        let expires = token["expires_in"]
            .as_u64()
            .filter(|expires| *expires > 0)
            .and_then(|seconds| seconds.checked_mul(1000))
            .and_then(|millis| timestamp().checked_add(millis))
            .ok_or("OAuth token response has invalid expiry")?;
        let credential = OAuthCredential {
            credential_type: "oauth".into(),
            access: access.into(),
            refresh: refresh.into(),
            expires,
            account_id: codex_account_id(access)?,
            extra: BTreeMap::new(),
        };
        credential.validate()?;
        Ok(credential)
    }
}

pub struct CodexLogin {
    pub authorization_url: String,
    pub redirect_uri: String,
    listener: TcpListener,
    state: String,
    verifier: String,
    client: CodexOAuthClient,
}

impl CodexLogin {
    pub fn callback_addr(&self) -> Result<SocketAddr, String> {
        self.listener
            .local_addr()
            .map_err(|_| "OAuth callback address unavailable".into())
    }

    pub fn finish(self, signal: &CancellationToken) -> Result<OAuthCredential, String> {
        let started = Instant::now();
        loop {
            check_cancelled(signal)?;
            if started.elapsed() >= self.client.config.timeout {
                return Err("OAuth callback timed out".into());
            }
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    let code = callback_code(&mut stream, &self.state, signal);
                    if let Ok(Some(code)) = code {
                        drop(stream);
                        drop(self.listener);
                        return self.client.token_request(
                            &[
                                ("grant_type", "authorization_code"),
                                ("client_id", &self.client.config.client_id),
                                ("code", &code),
                                ("code_verifier", &self.verifier),
                                ("redirect_uri", &self.redirect_uri),
                            ],
                            signal,
                        );
                    }
                    check_cancelled(signal)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(_) => return Err("OAuth callback listener failed".into()),
            }
        }
    }
}

fn callback_code(
    stream: &mut TcpStream,
    state: &str,
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
        if bytes.len() >= 16_384 || start.elapsed() > Duration::from_secs(2) {
            return Err("Invalid callback HTTP request".into());
        }
        let mut byte = [0u8];
        match stream.read(&mut byte) {
            Ok(0) => return Ok(None),
            Ok(_) => bytes.push(byte[0]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return Err("Callback HTTP read failed".into()),
        }
    }
    let request = std::str::from_utf8(&bytes).map_err(|_| "Invalid callback encoding")?;
    let parts: Vec<_> = request
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect();
    let url = parts
        .get(1)
        .and_then(|path| reqwest::Url::parse(&format!("http://localhost{path}")).ok());
    let mut status = "400 Bad Request";
    let mut code = None;
    if let Some(url) = url.filter(|_| parts.first() == Some(&"GET")) {
        if url.path() != "/auth/callback" {
            status = "404 Not Found";
        } else {
            let params: Vec<_> = url.query_pairs().collect();
            let states: Vec<_> = params.iter().filter(|(key, _)| key == "state").collect();
            let codes: Vec<_> = params.iter().filter(|(key, _)| key == "code").collect();
            if states.len() == 1
                && states[0].1 == state
                && codes.len() == 1
                && !codes[0].1.is_empty()
                && !params.iter().any(|(key, _)| key == "error")
            {
                code = Some(codes[0].1.to_string());
                status = "200 OK";
            }
        }
    }
    let text = if code.is_some() {
        "Authorization callback received. Return to the application to finish token exchange."
    } else {
        "Invalid authorization callback."
    };
    write!(stream,"HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",text.len()).map_err(|_| "Callback HTTP write failed")?;
    Ok(code)
}
