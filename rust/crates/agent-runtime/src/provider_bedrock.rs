//! Native Bedrock ConverseStream; credentials are resolved only in request workers.
use super::aws::{aws_uri_encode, AwsCredentials};
use crate::auth::check_cancelled;
use agent_core::types::{CancellationToken, Content, Context, Message};
use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

#[path = "provider_aws_credentials.rs"]
mod credentials;
#[cfg(test)]
#[path = "provider_aws_metadata_tests.rs"]
mod metadata_tests;
#[cfg(test)]
#[path = "provider_aws_credentials_tests.rs"]
mod sts_tests;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BedrockCacheRetention {
    None,
    #[default]
    Short,
    Long,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct BedrockOptions {
    pub region: String,
    pub profile: String,
    pub credentials_file: Option<PathBuf>,
    pub config_file: Option<PathBuf>,
    /// Empty selects the effective region's AWS STS endpoint. Validation performs no I/O.
    pub sts_endpoint: String,
    pub sso_cache_dir: Option<PathBuf>,
    pub sso_endpoint: String,
    pub sso_oidc_endpoint: String,
    #[serde(skip_serializing)]
    pub access_key_id: String,
    #[serde(skip_serializing)]
    pub secret_access_key: String,
    #[serde(skip_serializing)]
    pub session_token: String,
    #[serde(skip_serializing)]
    pub bearer_token: String,
    pub endpoint_explicit: bool,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f64>,
    pub cache_retention: BedrockCacheRetention,
}
pub struct ResolvedBedrock {
    pub region: String,
    pub bearer_token: Option<String>,
    pub credentials: Option<AwsCredentials>,
    /// UTC epoch seconds for expiring STS, SSO or process credentials; static/Bearer have no expiry.
    pub credentials_expiration: Option<u64>,
}
fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default().trim().to_owned()
}
fn region_valid(region: &str) -> bool {
    !region.is_empty()
        && region
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}
fn arn_region(model: &str) -> Option<&str> {
    let fields: Vec<_> = model.split(':').collect();
    (fields.len() >= 6
        && fields[0] == "arn"
        && fields[1].starts_with("aws")
        && fields[2] == "bedrock"
        && region_valid(fields[3]))
    .then(|| fields[3])
}
fn endpoint_region(source: &str) -> Option<String> {
    let url = Url::parse(source).ok()?;
    let host = url.host_str()?;
    let rest = host
        .strip_prefix("bedrock-runtime.")
        .or_else(|| host.strip_prefix("bedrock-runtime-fips."))?;
    let region = rest
        .strip_suffix(".amazonaws.com")
        .or_else(|| rest.strip_suffix(".amazonaws.com.cn"))?;
    region_valid(region).then(|| region.to_owned())
}
impl BedrockOptions {
    /// Config strings only. Secrets, profile files and network are not accessed here.
    pub fn from_env() -> Self {
        Self {
            region: {
                let region = env("AWS_REGION");
                if region.is_empty() {
                    env("AWS_DEFAULT_REGION")
                } else {
                    region
                }
            },
            profile: env("AWS_PROFILE"),
            credentials_file: std::env::var_os("AWS_SHARED_CREDENTIALS_FILE").map(PathBuf::from),
            config_file: std::env::var_os("AWS_CONFIG_FILE").map(PathBuf::from),
            sts_endpoint: env("AWS_ENDPOINT_URL_STS"),
            sso_endpoint: env("AWS_ENDPOINT_URL_SSO"),
            sso_oidc_endpoint: env("AWS_ENDPOINT_URL_SSO_OIDC"),
            cache_retention: if env("AGENT_CORE_CACHE_RETENTION") == "long" {
                BedrockCacheRetention::Long
            } else {
                BedrockCacheRetention::Short
            },
            ..Default::default()
        }
    }
    pub fn validate(&self, base: &str, model: &str) -> Result<(), String> {
        self.preview_base_url(base, model)?;
        if !self.sts_endpoint.is_empty() {
            credentials::sts_endpoint(&self.sts_endpoint, "us-east-1")?;
        }
        // The same URL policy applies to all credential endpoints, without I/O.
        for endpoint in [&self.sso_endpoint, &self.sso_oidc_endpoint] {
            if !endpoint.is_empty() {
                credentials::sts_endpoint(endpoint, "us-east-1")?;
            }
        }
        if !self.region.is_empty() && !region_valid(&self.region) {
            return Err("Invalid AWS region".into());
        }
        if self.profile.len() > 256 || self.profile.bytes().any(|byte| byte.is_ascii_control()) {
            return Err("Invalid AWS profile name".into());
        }
        if model.is_empty()
            || model.len() > 2048
            || !model.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
            })
            || model
                .split('/')
                .any(|segment| matches!(segment, "." | ".."))
        {
            return Err("Invalid Bedrock model ID".into());
        }
        // Prompt management ARNs require a different request body, not regular messages/system/tools.
        if model.contains(":prompt/") {
            return Err("Bedrock Prompt Management requests have not been migrated".into());
        }
        for token in [
            &self.access_key_id,
            &self.secret_access_key,
            &self.session_token,
            &self.bearer_token,
        ] {
            super::validate_header(token)?;
        }
        if self.max_tokens == Some(0)
            || self.temperature.is_some_and(|temperature| {
                !temperature.is_finite() || !(0.0..=1.0).contains(&temperature)
            })
        {
            return Err("Invalid Bedrock inference configuration".into());
        }
        if self.access_key_id.is_empty() != self.secret_access_key.is_empty() {
            return Err("Explicit AWS credentials are incomplete".into());
        }
        Ok(())
    }
    pub fn preview_base_url(&self, base: &str, model: &str) -> Result<Url, String> {
        let region = arn_region(model)
            .or_else(|| (!self.region.is_empty()).then_some(self.region.as_str()))
            .map(str::to_owned)
            .or_else(|| endpoint_region(base))
            .unwrap_or_else(|| "us-east-1".into());
        self.resolved_base_url(base, &region)
    }
    pub fn resolved_base_url(&self, base: &str, region: &str) -> Result<Url, String> {
        if !region_valid(region) {
            return Err("Invalid AWS region".into());
        }
        if !base.trim().is_empty() && (self.endpoint_explicit || endpoint_region(base).is_none()) {
            return super::clean_url(base);
        }
        let fips = base.contains("bedrock-runtime-fips.");
        let suffix = if region.starts_with("cn-") {
            "amazonaws.com.cn"
        } else {
            "amazonaws.com"
        };
        super::clean_url(&format!(
            "https://bedrock-runtime{}.{region}.{suffix}",
            if fips { "-fips" } else { "" }
        ))
    }
    pub fn request_url(&self, base: &str, model: &str, region: &str) -> Result<Url, String> {
        let mut url = self.resolved_base_url(base, region)?;
        let path = url.path().trim_end_matches('/');
        url.set_path(&format!(
            "{path}/model/{}/converse-stream",
            aws_uri_encode(model.as_bytes(), true)
        ));
        Ok(url)
    }
    pub fn route_identity(&self, url: &Url, region: &str) -> String {
        let mut identity = url.clone();
        identity.set_fragment(Some(&format!("region={region}")));
        identity.into()
    }
    pub fn resolve(
        &self,
        base: &str,
        model: &str,
        api_key: &str,
        signal: &CancellationToken,
    ) -> Result<ResolvedBedrock, String> {
        self.resolve_using(base, model, api_key, signal, &|name| {
            std::env::var(name).ok()
        })
    }
    fn resolve_using(
        &self,
        base: &str,
        model: &str,
        api_key: &str,
        signal: &CancellationToken,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<ResolvedBedrock, String> {
        check_cancelled(signal)?;
        self.validate(base, model)?;
        let read_env = |name| lookup(name).unwrap_or_default().trim().to_owned();
        let bearer_token = if !self.bearer_token.is_empty() {
            self.bearer_token.clone()
        } else if !api_key.trim().is_empty() {
            api_key.trim().to_owned()
        } else if !self.access_key_id.is_empty() {
            // Explicit native static credentials select SigV4, not an unrelated ambient bearer.
            String::new()
        } else {
            read_env("AWS_BEARER_TOKEN_BEDROCK")
        };
        super::validate_header(&bearer_token)?;
        let profile = if !self.profile.is_empty() {
            self.profile.clone()
        } else {
            read_env("AWS_PROFILE")
        };
        let mut region = arn_region(model)
            .map(str::to_owned)
            .or_else(|| (!self.region.is_empty()).then(|| self.region.clone()))
            .unwrap_or_else(|| {
                let region = read_env("AWS_REGION");
                if region.is_empty() {
                    read_env("AWS_DEFAULT_REGION")
                } else {
                    region
                }
            });
        let explicit = (!self.access_key_id.is_empty()).then(|| AwsCredentials {
            access_key_id: self.access_key_id.clone(),
            secret_access_key: self.secret_access_key.clone(),
            session_token: self.session_token.clone(),
        });
        let use_ambient = explicit.is_none() && bearer_token.is_empty() && profile.is_empty();
        let ambient_access = if use_ambient {
            read_env("AWS_ACCESS_KEY_ID")
        } else {
            String::new()
        };
        let ambient_secret = if use_ambient {
            read_env("AWS_SECRET_ACCESS_KEY")
        } else {
            String::new()
        };
        let mut resolved_credentials = if !bearer_token.is_empty() {
            None
        } else if explicit.is_some() {
            explicit
        } else if profile.is_empty() && (!ambient_access.is_empty() || !ambient_secret.is_empty()) {
            Some(AwsCredentials {
                access_key_id: ambient_access,
                secret_access_key: ambient_secret,
                session_token: read_env("AWS_SESSION_TOKEN"),
            })
        } else {
            None
        };
        let need_profile = resolved_credentials.is_none() && bearer_token.is_empty();
        let need_region = region.is_empty() && !profile.is_empty();
        let mut plan = None;
        if need_profile || need_region {
            let home = lookup("HOME")
                .or_else(|| lookup("USERPROFILE"))
                .filter(|home| !home.is_empty())
                .map(PathBuf::from);
            let resolve_path = |configured: &Option<PathBuf>,
                                name: &str,
                                default: &str|
             -> Option<(PathBuf, bool)> {
                let explicit = configured.clone().or_else(|| {
                    lookup(name)
                        .filter(|path| !path.is_empty())
                        .map(PathBuf::from)
                });
                let (path, required) = if let Some(path) = explicit {
                    (path, true)
                } else {
                    (home.as_ref()?.join(".aws").join(default), false)
                };
                let path =
                    if let Some(tail) = path.to_str().and_then(|path| path.strip_prefix("~/")) {
                        home.as_ref().map_or(path.clone(), |home| home.join(tail))
                    } else {
                        path
                    };
                Some((path, required))
            };
            let selected = if profile.is_empty() {
                "default"
            } else {
                profile.as_str()
            };
            let config_path = resolve_path(&self.config_file, "AWS_CONFIG_FILE", "config");
            let credentials_path = resolve_path(
                &self.credentials_file,
                "AWS_SHARED_CREDENTIALS_FILE",
                "credentials",
            );
            let load = |selected: &str| -> Result<BTreeMap<String, String>, String> {
                let mut data = if let Some((path, required)) = &config_path {
                    read_profile(path, selected, true, *required, signal)?
                } else {
                    BTreeMap::new()
                };
                let config_region = data.get("region").cloned();
                if let Some((path, required)) = &credentials_path {
                    data.extend(read_profile(path, selected, false, *required, signal)?);
                }
                if let Some(region) = config_region {
                    data.insert("region".into(), region);
                }
                Ok(data)
            };
            let data = load(selected)?;
            if region.is_empty() {
                region = data.get("region").cloned().unwrap_or_default();
            }
            if need_profile {
                let read_token = |source: &str| {
                    let path = if let Some(tail) = source.strip_prefix("~/") {
                        home.as_ref()
                            .map_or_else(|| PathBuf::from(source), |home| home.join(tail))
                    } else {
                        PathBuf::from(source)
                    };
                    read_web_token(&path, signal)
                };
                // Match the SDK default chain: configured/default profile precedes env WebIdentity.
                let env_token_path = read_env("AWS_WEB_IDENTITY_TOKEN_FILE");
                let env_role = read_env("AWS_ROLE_ARN");
                if !credentials::has_credential_configuration(&data)
                    && profile.is_empty()
                    && (!env_token_path.is_empty() || !env_role.is_empty())
                {
                    if env_token_path.is_empty() || env_role.is_empty() {
                        return Err(
                            "AWS WebIdentity environment configuration is incomplete".into()
                        );
                    }
                    plan = Some(credentials::web_identity_plan(
                        env_role,
                        &env_token_path,
                        read_env("AWS_ROLE_SESSION_NAME"),
                        &read_token,
                    )?);
                } else {
                    let additional_leaf = |data: &BTreeMap<String, String>| {
                        if let Some(source) = data.get("credential_source") {
                            return Ok(Some(credentials::metadata::source(source, data, lookup)?));
                        }
                        if let Some(command) = data.get("credential_process") {
                            if command.trim().is_empty()
                                || command.len() > 32_768
                                || command
                                    .bytes()
                                    .any(|byte| byte == 0 || byte == b'\r' || byte == b'\n')
                            {
                                return Err("Invalid AWS credential_process command".into());
                            }
                            return Ok(Some(credentials::CredentialPlan::Process {
                                command: command.clone(),
                                workspace: std::env::current_dir().map_err(|_| {
                                    "AWS credential_process working directory unavailable"
                                })?,
                            }));
                        }
                        if [
                            "sso_session",
                            "sso_start_url",
                            "sso_region",
                            "sso_account_id",
                            "sso_role_name",
                        ]
                        .iter()
                        .any(|key| data.contains_key(*key))
                        {
                            let session_name = data.get("sso_session").cloned();
                            let session = if let Some(name) = &session_name {
                                if name.is_empty()
                                    || name.len() > 256
                                    || name.bytes().any(|byte| byte.is_ascii_control())
                                {
                                    return Err("Invalid AWS SSO session name".into());
                                }
                                let (path, required) = config_path
                                    .as_ref()
                                    .ok_or("AWS SSO session config file missing")?;
                                let session = read_profile_section(
                                    path,
                                    name,
                                    Some("sso-session"),
                                    *required,
                                    signal,
                                )?;
                                for key in ["sso_start_url", "sso_region"] {
                                    if data
                                        .get(key)
                                        .is_some_and(|value| Some(value) != session.get(key))
                                    {
                                        return Err(
                                            "AWS SSO profile and session configuration differ"
                                                .into(),
                                        );
                                    }
                                }
                                session
                            } else {
                                data.clone()
                            };
                            let cache_dir = if let Some(path) = &self.sso_cache_dir {
                                if let Some(tail) =
                                    path.to_str().and_then(|path| path.strip_prefix("~/"))
                                {
                                    home.as_ref()
                                        .ok_or("AWS SSO home directory missing")?
                                        .join(tail)
                                } else {
                                    path.clone()
                                }
                            } else {
                                home.as_ref()
                                    .ok_or("AWS SSO cache directory missing")?
                                    .join(".aws/sso/cache")
                            };
                            let required = |fields: &BTreeMap<String, String>, key: &str| {
                                fields
                                    .get(key)
                                    .filter(|value| !value.is_empty())
                                    .cloned()
                                    .ok_or_else(|| "AWS SSO configuration is incomplete".to_owned())
                            };
                            let sso_region = required(&session, "sso_region")?;
                            if !region_valid(&sso_region) {
                                return Err("Invalid AWS SSO region".into());
                            }
                            let account = required(data, "sso_account_id")?;
                            if account.len() != 12
                                || !account.bytes().all(|byte| byte.is_ascii_digit())
                            {
                                return Err("Invalid AWS SSO account ID".into());
                            }
                            let role = required(data, "sso_role_name")?;
                            if role.len() > 128 || role.bytes().any(|byte| byte.is_ascii_control())
                            {
                                return Err("Invalid AWS SSO role name".into());
                            }
                            return Ok(Some(credentials::CredentialPlan::Sso(
                                credentials::sso::SsoPlan {
                                    start_url: required(&session, "sso_start_url")?,
                                    region: sso_region,
                                    account_id: account,
                                    role_name: role,
                                    session_name,
                                    cache_dir,
                                    endpoint: self.sso_endpoint.clone(),
                                    oidc_endpoint: self.sso_oidc_endpoint.clone(),
                                },
                            )));
                        }
                        Ok(None)
                    };
                    plan = Some(
                        if profile.is_empty() && !credentials::has_credential_configuration(&data) {
                            credentials::metadata::remote(&data, lookup)?
                        } else {
                            credentials::profile_plan(
                                selected,
                                &load,
                                &read_token,
                                &additional_leaf,
                                signal,
                            )?
                        },
                    );
                }
            }
        }
        if region.is_empty() {
            region = endpoint_region(base).unwrap_or_else(|| "us-east-1".into());
        }
        if !region_valid(&region) {
            return Err("Invalid AWS region".into());
        }
        let mut credentials_expiration = None;
        if use_ambient && resolved_credentials.is_some() {
            if let credentials::CredentialPlan::Environment { expiration, .. } =
                credentials::metadata::environment(lookup)?
            {
                credentials_expiration = expiration;
            }
        }
        if let Some(plan) = plan {
            let result = credentials::resolve_plan(&plan, &self.sts_endpoint, &region, signal)?;
            credentials_expiration = result.expiration;
            resolved_credentials = Some(result.credentials);
        }
        if let Some(credentials) = &resolved_credentials {
            credentials.validate()?;
        }
        check_cancelled(signal)?;
        Ok(ResolvedBedrock {
            region,
            bearer_token: (!bearer_token.is_empty()).then_some(bearer_token),
            credentials: resolved_credentials,
            credentials_expiration,
        })
    }
}
fn read_web_token(path: &Path, signal: &CancellationToken) -> Result<String, String> {
    check_cancelled(signal)?;
    let mut file =
        fs::File::open(path).map_err(|_| "AWS WebIdentity token file could not be read")?;
    let metadata = file
        .metadata()
        .map_err(|_| "AWS WebIdentity token file metadata unavailable")?;
    if !metadata.is_file() || metadata.len() > 20_002 {
        return Err("AWS WebIdentity token file must be a regular file within size limit".into());
    }
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        check_cancelled(signal)?;
        let count = file
            .read(&mut chunk)
            .map_err(|_| "AWS WebIdentity token file could not be read")?;
        if count == 0 {
            break;
        }
        if bytes.len() + count > 20_002 {
            return Err("AWS WebIdentity token file exceeds limit".into());
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    let token = std::str::from_utf8(&bytes)
        .map_err(|_| "AWS WebIdentity token file is not UTF-8")?
        .trim()
        .to_owned();
    bytes.fill(0);
    check_cancelled(signal)?;
    Ok(token)
}
fn read_profile(
    path: &Path,
    profile: &str,
    config: bool,
    required: bool,
    signal: &CancellationToken,
) -> Result<BTreeMap<String, String>, String> {
    read_profile_section(path, profile, config.then_some("profile"), required, signal)
}
fn read_profile_section(
    path: &Path,
    profile: &str,
    prefix: Option<&str>,
    required: bool,
    signal: &CancellationToken,
) -> Result<BTreeMap<String, String>, String> {
    check_cancelled(signal)?;
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => {
            return Ok(BTreeMap::new())
        }
        Err(_) => return Err("AWS profile file could not be read".into()),
    };
    let metadata = file
        .metadata()
        .map_err(|_| "AWS profile file metadata unavailable")?;
    if !metadata.is_file() || metadata.len() > 1_048_576 {
        return Err("AWS profile file must be a regular file within size limit".into());
    }
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 16_384];
    loop {
        check_cancelled(signal)?;
        let count = file
            .read(&mut chunk)
            .map_err(|_| "AWS profile file could not be read")?;
        if count == 0 {
            break;
        }
        if bytes.len() + count > 1_048_576 {
            return Err("AWS profile file exceeds size limit".into());
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    let source = std::str::from_utf8(&bytes).map_err(|_| "AWS profile file is not UTF-8")?;
    let mut section = String::new();
    let mut data = BTreeMap::new();
    let mut nested = false;
    for raw in source.trim_start_matches('\u{feff}').lines() {
        check_cancelled(signal)?;
        let comment = raw
            .char_indices()
            .find(|(index, character)| {
                matches!(character, '#' | ';')
                    && (*index == 0
                        || raw[..*index]
                            .chars()
                            .last()
                            .is_some_and(char::is_whitespace))
            })
            .map(|(index, _)| index)
            .unwrap_or(raw.len());
        let line = raw[..comment].trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let label = &line[1..line.len() - 1];
            section = if let Some(prefix) = prefix {
                label
                    .strip_prefix(&format!("{prefix} "))
                    .unwrap_or(if prefix == "profile" && label == "default" {
                        "default"
                    } else {
                        ""
                    })
                    .trim_matches(['\'', '"'])
                    .to_owned()
            } else {
                label.to_owned()
            };
            nested = false;
            continue;
        }
        if section != profile {
            continue;
        }
        if let Some((name, value)) = line.split_once('=') {
            let value = value.trim();
            if value.is_empty() {
                nested = true;
                continue;
            }
            if raw.trim_start() == raw {
                nested = false;
            }
            if !nested {
                data.insert(name.trim().to_owned(), value.to_owned());
            }
        }
    }
    bytes.fill(0);
    Ok(data)
}

fn claude(model: &str) -> bool {
    model.to_ascii_lowercase().contains("claude")
}
fn caching(model: &str) -> bool {
    let name = model
        .to_ascii_lowercase()
        .replace(['.', '_', ':', ' '], "-");
    name.contains("claude")
        && [
            "-4-",
            "fable-5",
            "opus-5",
            "sonnet-5",
            "claude-3-7-sonnet",
            "claude-3-5-haiku",
        ]
        .iter()
        .any(|part| name.contains(part))
}
fn call_id(id: &str) -> String {
    id.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}
fn sanitize_document(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter(|(key, _)| !key.is_empty())
                .map(|(key, value)| (key.clone(), sanitize_document(value)))
                .collect(),
        ),
        Value::Array(array) => Value::Array(array.iter().map(sanitize_document).collect()),
        _ => value.clone(),
    }
}
fn image_block(data: &str, mime: &str) -> Result<Value, String> {
    let format = match mime {
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => return Err("Unsupported Bedrock image format".into()),
    };
    STANDARD
        .decode(data)
        .map_err(|_| "Invalid Bedrock image base64")?;
    Ok(json!({"image":{"format":format,"source":{"bytes":data}}}))
}
fn user_parts(content: &[Content]) -> Result<Vec<Value>, String> {
    let mut parts = vec![];
    for block in content {
        match block {
            Content::Text { text } if !text.trim().is_empty() => parts.push(json!({"text":text})),
            Content::Image { data, mime_type } => parts.push(image_block(data, mime_type)?),
            _ => {}
        }
    }
    if parts.is_empty() {
        parts.push(json!({"text":"<empty>"}));
    }
    Ok(parts)
}
pub fn bedrock_request_payload(
    context: &Context,
    model: &str,
    tools: &[Value],
    options: &BedrockOptions,
) -> Result<Value, String> {
    let cache = options.cache_retention != BedrockCacheRetention::None && caching(model);
    let cache_point = json!({"cachePoint":if options.cache_retention==BedrockCacheRetention::Long{json!({"type":"default","ttl":"1h"})}else{json!({"type":"default"})}});
    let mut messages: Vec<Value> = vec![];
    let mut previous_tool = false;
    for message in &context.messages {
        match message {
            Message::User(user) => {
                messages.push(json!({"role":"user","content":user_parts(&user.content)?}));
                previous_tool = false;
            }
            Message::Assistant(assistant) => {
                let metadata = assistant.protocol_metadata.as_ref().filter(|metadata| {
                    metadata["protocol"] == "bedrock-converse-stream" && metadata["model"] == model
                });
                let mut content = vec![];
                for (index, block) in assistant.content.iter().enumerate() {
                    match block {
                Content::Text{text} if !text.trim().is_empty()=>content.push(json!({"text":text})),
                Content::ToolCall(call)=>content.push(json!({"toolUse":{"toolUseId":call_id(&call.id),"name":call.name,"input":sanitize_document(&call.arguments)}})),
                Content::Thinking{thinking}=>{
                    let stored=metadata.and_then(|metadata|metadata["blocks"].get(index.to_string()));
                    if let Some(redacted)=stored.filter(|block|block["kind"]=="redacted").and_then(|block|block["data"].as_str()) {if STANDARD.decode(redacted).is_ok_and(|bytes|!bytes.is_empty()){content.push(json!({"reasoningContent":{"redactedContent":redacted}}));}continue;}
                    if thinking.trim().is_empty(){continue;}
                    if metadata.is_none(){content.push(json!({"text":thinking}));}else if claude(model){let signature=stored.and_then(|block|block["signature"].as_str()).filter(|signature|!signature.trim().is_empty());if let Some(signature)=signature{content.push(json!({"reasoningContent":{"reasoningText":{"text":thinking,"signature":signature}}}));}else{content.push(json!({"text":thinking}));}}else{content.push(json!({"reasoningContent":{"reasoningText":{"text":thinking}}}));}
                },
                // AWS permits input images only in user/tool-result messages.
                _=>{}
            }
                }
                if !content.is_empty() {
                    messages.push(json!({"role":"assistant","content":content}));
                }
                previous_tool = false;
            }
            Message::ToolResult(result) => {
                let result = json!({"toolResult":{"toolUseId":call_id(&result.tool_call_id),"content":user_parts(&result.content)?,"status":if result.is_error{"error"}else{"success"}}});
                if previous_tool {
                    messages.last_mut().expect("Tool message present")["content"]
                        .as_array_mut()
                        .expect("Owned array")
                        .push(result);
                } else {
                    messages.push(json!({"role":"user","content":[result]}));
                }
                previous_tool = true;
            }
            Message::Custom { .. } => {}
        }
    }
    if cache {
        if let Some(last) = messages
            .last_mut()
            .filter(|message| message["role"] == "user")
        {
            last["content"]
                .as_array_mut()
                .expect("Owned array")
                .push(cache_point.clone());
        }
    }
    let mut payload = json!({"messages":messages});
    if !context.system_prompt.is_empty() {
        let mut system = vec![json!({"text":context.system_prompt})];
        if cache {
            system.push(cache_point);
        }
        payload["system"] = json!(system);
    }
    // Agent streamSimple supplies the catalog cap for every known model.
    let record = crate::catalog::find("amazon-bedrock", model);
    let max_tokens = options
        .max_tokens
        .map(u64::from)
        .or_else(|| record.map(|model| model.max_tokens))
        .map(|cap| {
            record.map_or(cap, |model| {
                let estimate = context.system_prompt.len().div_ceil(4) as f64
                    + context
                        .messages
                        .iter()
                        .map(crate::automatic::estimate_message_tokens)
                        .sum::<f64>()
                    + if tools.is_empty() {
                        0.0
                    } else {
                        serde_json::to_string(tools)
                            .expect("JSON values serialize")
                            .len()
                            .div_ceil(4) as f64
                    };
                cap.min(model.max_tokens).min(
                    (model.context_window as f64 - estimate - 4096.0)
                        .floor()
                        .max(1.0) as u64,
                )
            })
        });
    let mut inference = json!({});
    if let Some(tokens) = max_tokens {
        inference["maxTokens"] = json!(tokens);
    }
    if let Some(temperature) = options.temperature {
        inference["temperature"] = json!(temperature);
    }
    if inference
        .as_object()
        .is_some_and(|object| !object.is_empty())
    {
        payload["inferenceConfig"] = inference;
    }
    if !tools.is_empty() {
        payload["toolConfig"] = json!({"tools":tools.iter().map(|tool|{let tool=tool.get("function").unwrap_or(tool);json!({"toolSpec":{"name":tool["name"],"description":tool["description"],"inputSchema":{"json":tool.get("parameters").cloned().unwrap_or_else(||json!({"type":"object"}))}}})}).collect::<Vec<_>>()});
    }
    Ok(payload)
}

// The binary stream implementation is separated from credential/payload preparation.
pub use super::bedrock_stream::parse_bedrock_stream;

#[cfg(test)]
mod credential_tests {
    use super::*;
    struct Files(PathBuf);
    impl Files {
        fn new(config: &str, credentials: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "bedrock-profile-fixture-{}",
                crate::session::Session::default().id
            ));
            fs::create_dir(&path).unwrap();
            fs::write(path.join("config"), config).unwrap();
            fs::write(path.join("credentials"), credentials).unwrap();
            Self(path)
        }
        fn options(&self) -> BedrockOptions {
            BedrockOptions {
                profile: "qa".into(),
                credentials_file: Some(self.0.join("credentials")),
                config_file: Some(self.0.join("config")),
                ..Default::default()
            }
        }
    }
    impl Drop for Files {
        fn drop(&mut self) {
            fs::remove_file(self.0.join("config")).unwrap();
            fs::remove_file(self.0.join("credentials")).unwrap();
            fs::remove_dir(&self.0).unwrap();
        }
    }
    fn resolve(
        options: &BedrockOptions,
        environment: &[(&str, &str)],
        model: &str,
    ) -> Result<ResolvedBedrock, String> {
        let environment: BTreeMap<_, _> = environment
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        options.resolve_using(
            "https://bedrock-runtime.us-east-1.amazonaws.com",
            model,
            "",
            &CancellationToken::new(),
            &|key| environment.get(key).cloned(),
        )
    }
    #[test]
    fn named_profile_wins_ambient_static_keys_and_credentials_file_wins_config_secrets() {
        let files=Files::new("[default]\nregion=us-east-1\n[profile qa]\nregion=eu-west-1\naws_access_key_id=CONFIGKEY\naws_secret_access_key=config-secret\n", "[qa]\naws_access_key_id=FILEKEY\naws_secret_access_key=file-secret\naws_session_token=file-session\nregion=us-west-2\n");
        let resolved = resolve(
            &files.options(),
            &[
                ("AWS_PROFILE", "ignored-profile"),
                ("AWS_ACCESS_KEY_ID", "ENVKEY"),
                ("AWS_SECRET_ACCESS_KEY", "env-secret"),
            ],
            "fixture-model",
        )
        .unwrap();
        assert_eq!(resolved.region, "eu-west-1");
        let credentials = resolved.credentials.unwrap();
        assert_eq!(credentials.access_key_id, "FILEKEY");
        assert_eq!(credentials.secret_access_key, "file-secret");
        assert_eq!(credentials.session_token, "file-session");
    }
    #[test]
    fn ambient_bearer_has_original_sdk_priority_over_profile_or_static_credentials() {
        let options = BedrockOptions {
            region: "us-east-2".into(),
            profile: "unread-profile".into(),
            credentials_file: Some(PathBuf::from("must-not-read-fixture")),
            ..Default::default()
        };
        let resolved = resolve(
            &options,
            &[
                ("AWS_BEARER_TOKEN_BEDROCK", "bearer-fixture"),
                ("AWS_ACCESS_KEY_ID", "ENVKEY"),
                ("AWS_SECRET_ACCESS_KEY", "env-secret"),
            ],
            "fixture-model",
        )
        .unwrap();
        assert_eq!(resolved.bearer_token.as_deref(), Some("bearer-fixture"));
        assert!(resolved.credentials.is_none());
        let options = BedrockOptions {
            bearer_token: "explicit-bearer".into(),
            ..options
        };
        assert_eq!(
            resolve(
                &options,
                &[("AWS_BEARER_TOKEN_BEDROCK", "ambient-bearer")],
                "fixture-model"
            )
            .unwrap()
            .bearer_token
            .as_deref(),
            Some("explicit-bearer")
        );
    }
    #[test]
    fn region_precedence_is_arn_then_explicit_then_env_then_profile() {
        let files = Files::new(
            "[profile qa]\nregion=eu-west-1\n",
            "[qa]\naws_access_key_id=FILEKEY\naws_secret_access_key=file-secret\n",
        );
        let options = BedrockOptions {
            region: "us-east-2".into(),
            ..files.options()
        };
        let arn = "arn:aws-us-gov:bedrock:us-gov-west-1:123456789012:inference-profile/fixture";
        assert_eq!(
            resolve(&options, &[("AWS_REGION", "us-west-1")], arn)
                .unwrap()
                .region,
            "us-gov-west-1"
        );
        assert_eq!(
            resolve(&options, &[("AWS_REGION", "us-west-1")], "fixture")
                .unwrap()
                .region,
            "us-east-2"
        );
        let options = files.options();
        assert_eq!(
            resolve(
                &options,
                &[
                    ("AWS_REGION", "us-west-1"),
                    ("AWS_DEFAULT_REGION", "ap-south-1")
                ],
                "fixture"
            )
            .unwrap()
            .region,
            "us-west-1"
        );
        assert_eq!(
            resolve(&options, &[("AWS_DEFAULT_REGION", "ap-south-1")], "fixture")
                .unwrap()
                .region,
            "ap-south-1"
        );
        assert_eq!(
            resolve(&options, &[], "fixture").unwrap().region,
            "eu-west-1"
        );
    }
    #[test]
    fn environment_keys_without_profile_and_explicit_credentials_need_no_file_discovery() {
        let options = BedrockOptions {
            region: "us-east-1".into(),
            ..Default::default()
        };
        let resolved = resolve(
            &options,
            &[
                ("AWS_ACCESS_KEY_ID", "ENVKEY"),
                ("AWS_SECRET_ACCESS_KEY", "env-secret"),
                ("AWS_SESSION_TOKEN", "env-session"),
            ],
            "fixture",
        )
        .unwrap();
        let credentials = resolved.credentials.unwrap();
        assert_eq!(credentials.access_key_id, "ENVKEY");
        assert_eq!(credentials.session_token, "env-session");
        let options = BedrockOptions {
            access_key_id: "EXPLICITKEY".into(),
            secret_access_key: "explicit-secret".into(),
            session_token: "explicit-session".into(),
            ..options
        };
        assert_eq!(
            resolve(
                &options,
                &[
                    ("AWS_ACCESS_KEY_ID", "ENVKEY"),
                    ("AWS_SECRET_ACCESS_KEY", "env-secret")
                ],
                "fixture"
            )
            .unwrap()
            .credentials
            .unwrap()
            .access_key_id,
            "EXPLICITKEY"
        );
    }
    #[test]
    fn static_profile_selection_never_executes_or_downgrades_unmigrated_credential_providers() {
        for field in ["login_session", "mfa_serial", "saml_assertion"] {
            let files = Files::new(
                &format!("[profile qa]\nregion=us-east-1\n{field}=secret-fixture\n"),
                "[qa]\naws_access_key_id=FILEKEY\naws_secret_access_key=file-secret\n",
            );
            let error = resolve(
                &files.options(),
                &[
                    ("AWS_ACCESS_KEY_ID", "ENVKEY"),
                    ("AWS_SECRET_ACCESS_KEY", "env-secret"),
                ],
                "fixture",
            )
            .err()
            .unwrap();
            assert!(error.contains("have not been migrated"));
            assert!(!error.contains("secret-fixture"));
            assert!(!error.contains("file-secret"));
        }
    }
    #[test]
    fn profile_files_are_bounded_cancelled_and_nonsecrets_validate_without_reading_them() {
        let files = Files::new(
            "[profile qa]\nregion=us-east-1\n",
            "[qa]\naws_access_key_id=FILEKEY\naws_secret_access_key=file-secret\n",
        );
        let options = files.options();
        options.validate("http://127.0.0.1:1", "fixture").unwrap();
        let signal = CancellationToken::new();
        signal.cancel();
        assert!(options
            .resolve_using("", "fixture", "", &signal, &|_| None)
            .err()
            .unwrap()
            .contains("aborted"));
        fs::write(files.0.join("credentials"), vec![b'x'; 1_048_577]).unwrap();
        assert!(resolve(&options, &[], "fixture")
            .err()
            .unwrap()
            .contains("size limit"));
    }
}
