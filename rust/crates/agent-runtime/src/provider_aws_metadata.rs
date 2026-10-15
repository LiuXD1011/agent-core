//! AWS SDK native Environment, ECS/EKS HTTP credentials and EC2 IMDSv2/v1.
//! All env snapshots/file reads and HTTP calls happen only in the request worker.
use super::super::super::aws::AwsCredentials;
use super::{parse_expiration, CredentialPlan, CredentialResult};
use crate::auth::check_cancelled;
use agent_core::types::{timestamp, CancellationToken};
use reqwest::Url;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub(in super::super) enum MetadataPlan {
    Container {
        endpoint: Url,
        token: String,
        token_file: Option<PathBuf>,
    },
    Instance {
        endpoint: Url,
        v1_disabled: bool,
    },
}
fn value(lookup: &dyn Fn(&str) -> Option<String>, name: &str) -> String {
    lookup(name).unwrap_or_default().trim().into()
}
fn container_url(source: &str) -> Result<Url, String> {
    let url = Url::parse(source).map_err(|_| "Invalid AWS container credential URI")?;
    let host = url
        .host_str()
        .ok_or("AWS container credential URI has no host")?;
    let local = host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
        || matches!(
            host,
            "169.254.170.2" | "169.254.170.23" | "[fd00:ec2::23]" | "fd00:ec2::23"
        );
    if !(url.scheme() == "https" || url.scheme() == "http" && local)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "AWS container credential URI must use HTTPS or an accepted local metadata host".into(),
        );
    }
    Ok(url)
}
fn container(lookup: &dyn Fn(&str) -> Option<String>) -> Result<CredentialPlan, String> {
    let full = value(lookup, "AWS_CONTAINER_CREDENTIALS_FULL_URI");
    let relative = value(lookup, "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI");
    let source = if !full.is_empty() {
        full
    } else if !relative.is_empty() {
        if !relative.starts_with('/') || relative.starts_with("//") {
            return Err("Invalid AWS relative container credential URI".into());
        }
        format!("http://169.254.170.2{relative}")
    } else {
        return Err("AWS container credential endpoint is not configured".into());
    };
    let endpoint = container_url(&source)?;
    let token = lookup("AWS_CONTAINER_AUTHORIZATION_TOKEN").unwrap_or_default();
    super::super::super::validate_header(&token)?;
    let token_file = if token.is_empty() {
        lookup("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    } else {
        None
    };
    Ok(CredentialPlan::Metadata(MetadataPlan::Container {
        endpoint,
        token,
        token_file,
    }))
}
fn instance(
    profile: &BTreeMap<String, String>,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<CredentialPlan, String> {
    let configured = value(lookup, "AWS_EC2_METADATA_SERVICE_ENDPOINT");
    let configured = if configured.is_empty() {
        profile
            .get("ec2_metadata_service_endpoint")
            .cloned()
            .unwrap_or_default()
    } else {
        configured
    };
    let source = if configured.is_empty() {
        let mode = value(lookup, "AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE");
        let mode = if mode.is_empty() {
            profile
                .get("ec2_metadata_service_endpoint_mode")
                .map(String::as_str)
                .unwrap_or("IPv4")
        } else {
            &mode
        };
        match mode {
            "IPv4" => "http://169.254.169.254",
            "IPv6" => "http://[fd00:ec2::254]",
            _ => return Err("Invalid AWS IMDS endpoint mode".into()),
        }
        .to_owned()
    } else {
        configured
    };
    let endpoint = Url::parse(&source).map_err(|_| "Invalid AWS IMDS endpoint")?;
    if !matches!(endpoint.scheme(), "http" | "https")
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err("Invalid AWS IMDS endpoint".into());
    }
    let disabled = lookup("AWS_EC2_METADATA_V1_DISABLED")
        .map(|value| !value.is_empty() && value != "false")
        .unwrap_or_else(|| {
            profile
                .get("ec2_metadata_v1_disabled")
                .is_some_and(|value| !value.is_empty() && value != "false")
        });
    Ok(CredentialPlan::Metadata(MetadataPlan::Instance {
        endpoint,
        v1_disabled: disabled,
    }))
}
pub(in super::super) fn environment(
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<CredentialPlan, String> {
    let credentials = AwsCredentials {
        access_key_id: value(lookup, "AWS_ACCESS_KEY_ID"),
        secret_access_key: value(lookup, "AWS_SECRET_ACCESS_KEY"),
        session_token: lookup("AWS_SESSION_TOKEN").unwrap_or_default(),
    };
    credentials.validate()?;
    let expiration = lookup("AWS_CREDENTIAL_EXPIRATION")
        .filter(|value| !value.is_empty())
        .map(|value| parse_expiration(&value))
        .transpose()?;
    if expiration.is_some_and(|expiration| expiration <= timestamp() / 1000) {
        return Err("AWS environment credentials are expired".into());
    }
    Ok(CredentialPlan::Environment {
        credentials,
        expiration,
    })
}
pub(in super::super) fn source(
    source: &str,
    profile: &BTreeMap<String, String>,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<CredentialPlan, String> {
    match source {
        "Environment" => environment(lookup),
        "EcsContainer" => container(lookup),
        "Ec2InstanceMetadata" => instance(profile, lookup),
        _ => Err("Unsupported AWS credential_source provider".into()),
    }
}
pub(in super::super) fn remote(
    profile: &BTreeMap<String, String>,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<CredentialPlan, String> {
    if !value(lookup, "AWS_CONTAINER_CREDENTIALS_FULL_URI").is_empty()
        || !value(lookup, "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").is_empty()
    {
        return container(lookup);
    }
    if lookup("AWS_EC2_METADATA_DISABLED")
        .is_some_and(|value| !value.is_empty() && value != "false")
    {
        return Err("AWS EC2 instance metadata access is disabled".into());
    }
    instance(profile, lookup)
}
fn token(plan: &MetadataPlan, signal: &CancellationToken) -> Result<String, String> {
    if let MetadataPlan::Container {
        token, token_file, ..
    } = plan
    {
        if let Some(path) = token_file {
            check_cancelled(signal)?;
            let file = fs::File::open(path)
                .map_err(|_| "AWS container authorization token file could not be read")?;
            let metadata = file
                .metadata()
                .map_err(|_| "AWS container token file metadata unavailable")?;
            if !metadata.is_file() || metadata.len() > 65536 {
                return Err(
                    "AWS container authorization token must be a bounded regular file".into(),
                );
            }
            let mut bytes = Vec::new();
            file.take(65537)
                .read_to_end(&mut bytes)
                .map_err(|_| "AWS container authorization token file could not be read")?;
            let result = String::from_utf8(bytes)
                .map_err(|_| "AWS container authorization token file must be UTF-8")?;
            if result.len() > 65536 {
                return Err("AWS container authorization token exceeds limit".into());
            }
            super::super::super::validate_header(&result)?;
            check_cancelled(signal)?;
            Ok(result)
        } else {
            Ok(token.clone())
        }
    } else {
        Ok(String::new())
    }
}
fn bound_fingerprint(plan: &MetadataPlan, token: &str) -> String {
    let mut hash = Sha256::new();
    let mut part = |value: &[u8]| {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value);
    };
    match plan {
        MetadataPlan::Container {
            endpoint,
            token_file,
            ..
        } => {
            part(b"container");
            part(endpoint.as_str().as_bytes());
            part(token.as_bytes());
            if let Some(path) = token_file {
                part(path.as_os_str().as_encoded_bytes());
            }
        }
        MetadataPlan::Instance {
            endpoint,
            v1_disabled,
        } => {
            part(b"instance");
            part(endpoint.as_str().as_bytes());
            part(&[u8::from(*v1_disabled)]);
        }
    }
    format!("{:x}", hash.finalize())
}
pub(super) fn fingerprint(
    plan: &MetadataPlan,
    signal: &CancellationToken,
) -> Result<String, String> {
    Ok(bound_fingerprint(plan, &token(plan, signal)?))
}
struct Response {
    status: u16,
    bytes: Vec<u8>,
}
fn http(
    method: reqwest::Method,
    url: Url,
    headers: BTreeMap<String, String>,
    signal: &CancellationToken,
) -> Result<Response, String> {
    check_cancelled(signal)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "AWS metadata runtime failed")?;
    let result = runtime.block_on(async {
        // Metadata bypasses proxies and redirects; the SDK request timeout is 1s.
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(1))
            .timeout(Duration::from_secs(1))
            .build()
            .map_err(|_| "AWS metadata client failed")?;
        let request = async {
            let mut request = client.request(method, url);
            for (name, value) in headers {
                request = request.header(name, value);
            }
            let mut response = request
                .send()
                .await
                .map_err(|_| "AWS metadata request failed or timed out")?;
            let status = response.status().as_u16();
            let mut bytes = Vec::new();
            if (200..300).contains(&status) {
                while let Some(chunk) = response
                    .chunk()
                    .await
                    .map_err(|_| "AWS metadata response read failed")?
                {
                    if bytes.len().saturating_add(chunk.len()) > 65536 {
                        return Err("AWS metadata response exceeds limit".into());
                    }
                    bytes.extend_from_slice(&chunk);
                }
            }
            Ok(Response { status, bytes })
        };
        let cancelled = async {
            while !signal.is_cancelled() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        };
        tokio::select! {response=request=>response,_=cancelled=>Err("Operation aborted".into())}
    });
    check_cancelled(signal)?;
    result
}
fn parse(response: Response, fingerprint: String) -> Result<CredentialResult, String> {
    if response.status != 200 {
        return Err(format!(
            "AWS metadata endpoint returned HTTP {}",
            response.status
        ));
    }
    let value: Value = serde_json::from_slice(&response.bytes)
        .map_err(|_| "Invalid AWS metadata credential JSON")?;
    let field = |name: &str| {
        value[name]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or("AWS metadata credential field missing")
    };
    if value.get("Code").is_some_and(|code| code != "Success") {
        return Err("AWS metadata service did not return successful credentials".into());
    }
    let credentials = AwsCredentials {
        access_key_id: field("AccessKeyId")?,
        secret_access_key: field("SecretAccessKey")?,
        session_token: field("Token")?,
    };
    credentials.validate()?;
    let expiration = parse_expiration(&field("Expiration")?)?;
    if expiration <= timestamp() / 1000 {
        return Err("AWS metadata credentials are expired".into());
    }
    Ok(CredentialResult {
        credentials,
        expiration: Some(expiration),
        source_fingerprint: Some(fingerprint),
    })
}
pub(super) fn resolve(
    plan: &MetadataPlan,
    signal: &CancellationToken,
) -> Result<CredentialResult, String> {
    match plan {
        MetadataPlan::Container { endpoint, .. } => {
            let mut last = String::new();
            for attempt in 0..4 {
                if attempt > 0 {
                    let start = Instant::now();
                    while start.elapsed() < Duration::from_secs(1) {
                        check_cancelled(signal)?;
                        std::thread::sleep(Duration::from_millis(25));
                    }
                }
                let token = token(plan, signal)?;
                let headers = if token.is_empty() {
                    BTreeMap::new()
                } else {
                    BTreeMap::from([("authorization".into(), token.clone())])
                };
                let result = http(reqwest::Method::GET, endpoint.clone(), headers, signal)
                    .and_then(|response| parse(response, bound_fingerprint(plan, &token)));
                match result {
                    Ok(result) => return Ok(result),
                    Err(error) => {
                        check_cancelled(signal)?;
                        last = error;
                    }
                }
            }
            Err(last)
        }
        MetadataPlan::Instance {
            endpoint,
            v1_disabled,
        } => {
            let mut token_url = endpoint.clone();
            token_url.set_path("/latest/api/token");
            let response = http(
                reqwest::Method::PUT,
                token_url,
                BTreeMap::from([(
                    "x-aws-ec2-metadata-token-ttl-seconds".into(),
                    "21600".into(),
                )]),
                signal,
            );
            let token = match response {
                Ok(response) if response.status == 200 => {
                    let token = String::from_utf8(response.bytes)
                        .map_err(|_| "Invalid AWS IMDSv2 token encoding")?;
                    if token.is_empty() {
                        return Err("AWS IMDSv2 token is empty".into());
                    }
                    super::super::super::validate_header(&token)?;
                    Some(token)
                }
                Ok(response) if response.status == 400 => {
                    return Err("AWS IMDSv2 token endpoint returned HTTP 400".into())
                }
                Ok(_) | Err(_) => {
                    check_cancelled(signal)?;
                    if *v1_disabled {
                        return Err("AWS IMDSv1 fallback is disabled".into());
                    }
                    None
                }
            };
            let headers = token
                .map(|token| BTreeMap::from([("x-aws-ec2-metadata-token".into(), token)]))
                .unwrap_or_default();
            let mut profile_url = endpoint.clone();
            profile_url.set_path("/latest/meta-data/iam/security-credentials/");
            let response = http(reqwest::Method::GET, profile_url, headers.clone(), signal)?;
            if response.status != 200 {
                return Err(format!(
                    "AWS metadata role endpoint returned HTTP {}",
                    response.status
                ));
            }
            let role = std::str::from_utf8(&response.bytes)
                .map_err(|_| "Invalid AWS metadata role encoding")?
                .trim();
            if role.is_empty()
                || role.len() > 256
                || !role.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'+')
                })
            {
                return Err("Invalid AWS metadata role name".into());
            }
            let mut credentials_url = endpoint.clone();
            credentials_url.set_path(&format!(
                "/latest/meta-data/iam/security-credentials/{role}"
            ));
            parse(
                http(reqwest::Method::GET, credentials_url, headers, signal)?,
                bound_fingerprint(plan, ""),
            )
        }
    }
}
