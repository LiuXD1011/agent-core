//! Google Vertex express API keys, explicit Bearer tokens and file-based ADC.
//! Native file/URL federation and impersonation are delegated to the ADC module.
use super::google::{google_request_payload_for_protocol, parse_google_sse};
use crate::auth::check_cancelled;
use agent_core::types::{timestamp, AssistantMessage, CancellationToken, Context};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use ring::{
    rand::SystemRandom,
    signature::{RsaKeyPair, RSA_PKCS1_SHA256},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, Read};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock, TryLockError};
use std::time::Duration;
#[path = "provider_vertex_federation.rs"]
mod federation;

pub const VERTEX_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const VERTEX_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct VertexOptions {
    pub project: String,
    pub location: String,
    #[serde(skip_serializing)]
    pub access_token: String,
    pub adc_path: Option<PathBuf>,
    pub token_url: String,
}
impl Default for VertexOptions {
    fn default() -> Self {
        Self {
            project: String::new(),
            location: String::new(),
            access_token: String::new(),
            adc_path: None,
            token_url: VERTEX_TOKEN_URL.into(),
        }
    }
}
impl VertexOptions {
    /// Environment config only: ADC files and token endpoints are used only by request workers.
    pub fn from_env() -> Self {
        Self {
            project: std::env::var("GOOGLE_CLOUD_PROJECT")
                .ok()
                .filter(|project| !project.trim().is_empty())
                .or_else(|| std::env::var("GCLOUD_PROJECT").ok())
                .unwrap_or_default(),
            location: std::env::var("GOOGLE_CLOUD_LOCATION").unwrap_or_default(),
            adc_path: std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS").map(PathBuf::from),
            ..Self::default()
        }
    }
    pub fn uses_api_key(&self, key: &str) -> bool {
        let key = key.trim();
        !key.is_empty()
            && key != "gcp-vertex-credentials"
            && !(key.starts_with('<') && key.ends_with('>'))
    }
    pub fn validate(&self, configured: &str, model: &str, key: &str) -> Result<(), String> {
        super::validate_header(&self.access_token)?;
        let express = self.uses_api_key(key);
        if express && !self.access_token.is_empty() {
            return Err("Vertex API key and access token are mutually exclusive".into());
        }
        if !express {
            segment(&self.project, "project")?;
            segment(&self.location, "location")?;
        }
        segment(
            model
                .strip_prefix("publishers/google/models/")
                .unwrap_or(model),
            "model",
        )?;
        self.resolved_base_url(configured, express)?;
        credential_endpoint(&self.token_url)?;
        Ok(())
    }
    pub fn resolved_base_url(
        &self,
        configured: &str,
        express: bool,
    ) -> Result<reqwest::Url, String> {
        let default = "https://aiplatform.googleapis.com";
        let base = if !self.custom_base(configured) {
            if express || self.location == "global" {
                default.to_owned()
            } else if matches!(self.location.as_str(), "us" | "eu") {
                format!("https://aiplatform.{}.rep.googleapis.com", self.location)
            } else {
                format!("https://{}-aiplatform.googleapis.com", self.location)
            }
        } else {
            configured.trim().to_owned()
        };
        super::clean_url(&base)
    }
    fn custom_base(&self, configured: &str) -> bool {
        !configured.trim().is_empty()
            && !configured.contains("{location}")
            && configured.trim().trim_end_matches('/') != "https://aiplatform.googleapis.com"
    }
    pub fn request_url(
        &self,
        configured: &str,
        model: &str,
        key: &str,
    ) -> Result<reqwest::Url, String> {
        let express = self.uses_api_key(key);
        let mut url = self.resolved_base_url(configured, express)?;
        let path = url.path().trim_end_matches('/');
        let version = if path.split('/').any(|part| {
            part.strip_prefix('v').is_some_and(|version| {
                let (major, beta) = version.split_once("beta").unwrap_or((version, ""));
                !major.is_empty()
                    && major.bytes().all(|byte| byte.is_ascii_digit())
                    && beta.bytes().all(|byte| byte.is_ascii_digit())
            })
        }) {
            path.to_owned()
        } else {
            format!("{path}/v1")
        };
        let model = model
            .strip_prefix("publishers/google/models/")
            .unwrap_or(model);
        // The TS SDK explicitly chooses ResourceScope.COLLECTION for custom bases.
        // A proxy base may already contain /projects/.../locations/...; never duplicate it.
        let resource = if express || self.custom_base(configured) {
            format!("publishers/google/models/{model}")
        } else {
            format!(
                "projects/{}/locations/{}/publishers/google/models/{model}",
                self.project, self.location
            )
        };
        url.set_path(&format!("{version}/{resource}:streamGenerateContent"));
        url.set_query(Some("alt=sse"));
        Ok(url)
    }
    pub fn route_identity(&self, url: &reqwest::Url, key: &str) -> String {
        let mut identity = url.clone();
        if !self.uses_api_key(key) {
            identity.set_fragment(Some(&format!(
                "projects/{}/locations/{}",
                self.project, self.location
            )));
        }
        identity.into()
    }
    pub fn authorize(&self, key: &str, signal: &CancellationToken) -> Result<VertexAuth, String> {
        check_cancelled(signal)?;
        if self.uses_api_key(key) {
            return Ok(VertexAuth {
                api_key: Some(key.trim().to_owned()),
                access_token: None,
                quota_project: None,
            });
        }
        if !self.access_token.is_empty() {
            return Ok(VertexAuth {
                api_key: None,
                access_token: Some(self.access_token.clone()),
                quota_project: None,
            });
        }
        let path = self
            .adc_path
            .clone()
            .or_else(|| std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS").map(PathBuf::from))
            .or_else(default_adc_path)
            .ok_or("Vertex ADC file is not configured")?;
        let file = fs::File::open(path).map_err(|_| "Vertex ADC file could not be read")?;
        if file
            .metadata()
            .map_err(|_| "Vertex ADC file metadata unavailable")?
            .len()
            > 1_048_576
        {
            return Err("Vertex ADC file exceeds limit".into());
        }
        let mut bytes = Vec::new();
        file.take(1_048_577)
            .read_to_end(&mut bytes)
            .map_err(|_| "Vertex ADC file could not be read")?;
        check_cancelled(signal)?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| "Invalid Vertex ADC JSON")?;
        let kind = value["type"]
            .as_str()
            .ok_or("Vertex ADC credential type missing")?;
        if !matches!(
            kind,
            "authorized_user"
                | "service_account"
                | "external_account"
                | "external_account_authorized_user"
                | "impersonated_service_account"
        ) {
            return Err("Vertex ADC credential type has not been migrated".into());
        }
        let quota_project = value["quota_project_id"]
            .as_str()
            .filter(|project| !project.is_empty())
            .map(str::to_owned);
        if let Some(project) = &quota_project {
            segment(project, "quota project")?;
        }
        let mut fingerprint = Sha256::new();
        fingerprint.update(&bytes);
        fingerprint.update(self.token_url.as_bytes());
        fingerprint.update(VERTEX_SCOPE.as_bytes());
        let fingerprint = format!("{:x}", fingerprint.finalize());
        bytes.fill(0);
        static CACHE: OnceLock<Mutex<BTreeMap<String, CachedToken>>> = OnceLock::new();
        let cache = CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
        let mut cache = loop {
            check_cancelled(signal)?;
            match cache.try_lock() {
                Ok(cache) => break cache,
                Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(25)),
                Err(TryLockError::Poisoned(_)) => {
                    return Err("Vertex credential cache unavailable".into())
                }
            }
        };
        if let Some(token) = cache
            .get(&fingerprint)
            .filter(|token| token.expires > timestamp().saturating_add(60_000))
        {
            return Ok(VertexAuth {
                api_key: None,
                access_token: Some(token.access.clone()),
                quota_project,
            });
        }
        let token = if matches!(
            kind,
            "external_account"
                | "external_account_authorized_user"
                | "impersonated_service_account"
        ) {
            federation::resolve(&value, &self.token_url, signal)?
        } else {
            let fields = if kind == "authorized_user" {
                vec![
                    ("grant_type".into(), "refresh_token".into()),
                    ("client_id".into(), required(&value, "client_id")?.into()),
                    (
                        "client_secret".into(),
                        required(&value, "client_secret")?.into(),
                    ),
                    (
                        "refresh_token".into(),
                        required(&value, "refresh_token")?.into(),
                    ),
                ]
            } else {
                vec![
                    (
                        "grant_type".into(),
                        "urn:ietf:params:oauth:grant-type:jwt-bearer".into(),
                    ),
                    (
                        "assertion".into(),
                        service_account_assertion(&value, &self.token_url, timestamp() / 1000)?,
                    ),
                ]
            };
            exchange_token(&self.token_url, &fields, signal)?
        };
        check_cancelled(signal)?;
        let access = token.access.clone();
        if cache.len() >= 64 {
            cache.pop_first();
        }
        cache.insert(fingerprint, token);
        check_cancelled(signal)?;
        Ok(VertexAuth {
            api_key: None,
            access_token: Some(access),
            quota_project,
        })
    }
}

/// No Debug/serde implementation: token values never enter application settings output.
pub struct VertexAuth {
    pub api_key: Option<String>,
    pub access_token: Option<String>,
    pub quota_project: Option<String>,
}
struct CachedToken {
    access: String,
    expires: u64,
}
fn segment(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(format!("Invalid Vertex {field}"));
    }
    Ok(())
}
fn required<'a>(value: &'a Value, field: &str) -> Result<&'a str, String> {
    value[field]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("Vertex ADC {field} missing"))
}
fn default_adc_path() -> Option<PathBuf> {
    #[cfg(windows)]
    if let Some(directory) = std::env::var_os("APPDATA") {
        return Some(PathBuf::from(directory).join("gcloud/application_default_credentials.json"));
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| PathBuf::from(home).join(".config/gcloud/application_default_credentials.json"))
}
fn credential_endpoint(source: &str) -> Result<reqwest::Url, String> {
    let url = super::clean_url(source)?;
    let local = url
        .host_str()
        .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]"));
    if url.scheme() != "https" && !local {
        return Err("Vertex token endpoint must use HTTPS or local loopback HTTP".into());
    }
    Ok(url)
}
pub fn service_account_assertion(
    value: &Value,
    audience: &str,
    issued_at: u64,
) -> Result<String, String> {
    credential_endpoint(audience)?;
    let pem = required(value, "private_key")?;
    let body = pem
        .strip_prefix("-----BEGIN PRIVATE KEY-----")
        .and_then(|body| body.trim().strip_suffix("-----END PRIVATE KEY-----"))
        .ok_or("Vertex service-account key must be PKCS8 PEM")?;
    let mut der = STANDARD
        .decode(body.split_whitespace().collect::<String>())
        .map_err(|_| "Invalid service-account PEM")?;
    let key = RsaKeyPair::from_pkcs8(&der).map_err(|_| "Invalid service-account RSA key");
    der.fill(0);
    let key = key?;
    let mut header = json!({"alg":"RS256","typ":"JWT"});
    if let Some(id) = value["private_key_id"].as_str().filter(|id| !id.is_empty()) {
        header["kid"] = json!(id);
    }
    let claims = json!({"iss":required(value,"client_email")?,"scope":VERTEX_SCOPE,"aud":audience,"iat":issued_at,"exp":issued_at.checked_add(3600).ok_or("Invalid service-account assertion time")?});
    let signing = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let mut signature = vec![0u8; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        signing.as_bytes(),
        &mut signature,
    )
    .map_err(|_| "Service-account JWT signing failed")?;
    Ok(format!("{signing}.{}", URL_SAFE_NO_PAD.encode(signature)))
}
fn exchange_token(
    endpoint: &str,
    fields: &[(String, String)],
    signal: &CancellationToken,
) -> Result<CachedToken, String> {
    let url = credential_endpoint(endpoint)?;
    let mut form = reqwest::Url::parse("https://fixture.invalid").expect("Static form base");
    form.query_pairs_mut().extend_pairs(
        fields
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    );
    let body = form.query().unwrap_or("").to_owned();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "Vertex credential runtime failed")?;
    let value: Value = runtime.block_on(async {
        let builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(10));
        let client = if url.scheme() == "http" {
            builder.no_proxy()
        } else {
            builder
        }
        .build()
        .map_err(|_| "Vertex credential client failed")?;
        let request = async {
            let mut response = client
                .post(url)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(body)
                .send()
                .await
                .map_err(|_| "Vertex credential request failed")?;
            if !response.status().is_success() {
                return Err(format!(
                    "Vertex credential endpoint returned HTTP {}",
                    response.status().as_u16()
                ));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| "Vertex credential response failed")?
            {
                if bytes.len().saturating_add(chunk.len()) > 65_536 {
                    return Err("Vertex credential response exceeds limit".into());
                }
                bytes.extend_from_slice(&chunk);
            }
            serde_json::from_slice(&bytes)
                .map_err(|_| "Invalid Vertex credential token JSON".to_owned())
        };
        let cancelled = async {
            while !signal.is_cancelled() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        };
        tokio::select! {response=request=>response,_=cancelled=>Err("Operation aborted".into())}
    })?;
    check_cancelled(signal)?;
    let access = required(&value, "access_token")?.to_owned();
    super::validate_header(&access)?;
    if value
        .get("token_type")
        .and_then(Value::as_str)
        .is_some_and(|kind| !kind.eq_ignore_ascii_case("Bearer"))
    {
        return Err("Unexpected Vertex credential token type".into());
    }
    let expires = value["expires_in"]
        .as_u64()
        .filter(|seconds| *seconds > 0)
        .and_then(|seconds| seconds.checked_mul(1000))
        .and_then(|millis| timestamp().checked_add(millis))
        .ok_or("Invalid Vertex credential expiry")?;
    Ok(CachedToken { access, expires })
}
pub fn vertex_request_payload(context: &Context, model: &str, tools: &[Value]) -> Value {
    google_request_payload_for_protocol(context, model, tools, "google-vertex")
}
pub fn parse_vertex_sse(
    reader: impl BufRead,
    signal: &CancellationToken,
    on_update: &mut dyn FnMut(AssistantMessage),
) -> Result<AssistantMessage, String> {
    let mut update = |mut message: AssistantMessage| {
        if let Some(metadata) = message.protocol_metadata.as_mut() {
            metadata["protocol"] = json!("google-vertex");
        }
        on_update(message);
    };
    let mut message = parse_google_sse(reader, signal, &mut update)?;
    if let Some(metadata) = message.protocol_metadata.as_mut() {
        metadata["protocol"] = json!("google-vertex");
    }
    Ok(message)
}
