//! ADC federation wire contracts mirror google-auth-library 10.6.2.
//! File/URL identity sources, workforce refresh and service-account impersonation
//! run only in the request worker, with bounded I/O and cancellation.
use super::{
    credential_endpoint, exchange_token, required, service_account_assertion, CachedToken,
    VERTEX_SCOPE, VERTEX_TOKEN_URL,
};
use crate::auth::check_cancelled;
use agent_core::types::{timestamp, CancellationToken};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

fn request(
    method: reqwest::Method,
    url: &str,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
    signal: &CancellationToken,
) -> Result<Vec<u8>, String> {
    check_cancelled(signal)?;
    let url = if method == reqwest::Method::GET {
        let url = reqwest::Url::parse(url).map_err(|_| "Invalid Vertex subject token URL")?;
        let local = url.host_str().is_some_and(|host| {
            matches!(
                host,
                "127.0.0.1" | "localhost" | "::1" | "[::1]" | "169.254.169.254"
            )
        });
        if !(url.scheme() == "https" || url.scheme() == "http" && local)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err("Invalid Vertex subject token URL".into());
        }
        url
    } else {
        credential_endpoint(url)?
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "Vertex federation runtime failed")?;
    let result = runtime.block_on(async {
        let builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30));
        let builder = if url.scheme() == "http" {
            builder.no_proxy()
        } else {
            builder
        };
        let client = builder
            .build()
            .map_err(|_| "Vertex federation client failed")?;
        let task = async {
            let mut request = client.request(method, url);
            for (name, value) in headers {
                request = request.header(name, value);
            }
            if !body.is_empty() {
                request = request.body(body);
            }
            let mut response = request
                .send()
                .await
                .map_err(|_| "Vertex federation request failed")?;
            if !response.status().is_success() {
                return Err(format!(
                    "Vertex federation endpoint returned HTTP {}",
                    response.status().as_u16()
                ));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| "Vertex federation response failed")?
            {
                if bytes.len().saturating_add(chunk.len()) > 1_048_576 {
                    return Err("Vertex federation response exceeds limit".into());
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(bytes)
        };
        let cancelled = async {
            while !signal.is_cancelled() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        };
        tokio::select! {result=task=>result,_=cancelled=>Err("Operation aborted".into())}
    });
    check_cancelled(signal)?;
    result
}
fn form(fields: &[(String, String)]) -> Vec<u8> {
    let mut url = reqwest::Url::parse("https://fixture.invalid").expect("Static form");
    url.query_pairs_mut().extend_pairs(
        fields
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    );
    url.query().unwrap_or("").as_bytes().to_vec()
}
fn json_body(bytes: &[u8]) -> Result<Value, String> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| "Invalid Vertex federation JSON")?;
    if !value.is_object() {
        return Err("Vertex federation response must be an object".into());
    }
    Ok(value)
}
fn parse_token(value: &Value) -> Result<CachedToken, String> {
    let access = required(value, "access_token")?.to_owned();
    super::super::validate_header(&access)?;
    if value["token_type"]
        .as_str()
        .is_some_and(|kind| !kind.eq_ignore_ascii_case("Bearer"))
    {
        return Err("Unexpected Vertex federation token type".into());
    }
    let expires = value["expires_in"]
        .as_u64()
        .filter(|value| *value > 0)
        .and_then(|seconds| seconds.checked_mul(1000))
        .and_then(|millis| timestamp().checked_add(millis))
        .ok_or("Invalid Vertex federation token expiry")?;
    Ok(CachedToken { access, expires })
}
fn basic(value: &Value) -> Result<BTreeMap<String, String>, String> {
    let mut headers = BTreeMap::from([(
        "content-type".into(),
        "application/x-www-form-urlencoded".into(),
    )]);
    if let Some(client) = value["client_id"]
        .as_str()
        .filter(|value| !value.is_empty())
    {
        if value
            .get("client_secret")
            .is_some_and(|secret| !secret.is_string())
        {
            return Err("Invalid Vertex confidential client secret".into());
        }
        let secret = value["client_secret"].as_str().unwrap_or("");
        headers.insert(
            "authorization".into(),
            format!("Basic {}", STANDARD.encode(format!("{client}:{secret}"))),
        );
    }
    Ok(headers)
}
fn token_endpoint(value: &Value, override_url: &str, workforce: bool) -> Result<String, String> {
    if override_url != VERTEX_TOKEN_URL {
        return Ok(credential_endpoint(override_url)?.into());
    }
    if let Some(url) = value["token_url"].as_str() {
        return Ok(credential_endpoint(url)?.into());
    }
    let universe = value["universe_domain"]
        .as_str()
        .unwrap_or("googleapis.com");
    if universe.is_empty()
        || !universe
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    {
        return Err("Invalid Vertex universe domain".into());
    }
    Ok(format!(
        "https://sts.{universe}/v1/{}",
        if workforce { "oauthtoken" } else { "token" }
    ))
}
fn file(path: &str, signal: &CancellationToken) -> Result<Vec<u8>, String> {
    check_cancelled(signal)?;
    let path = fs::canonicalize(Path::new(path))
        .map_err(|_| "Vertex subject token file could not be read")?;
    let metadata =
        fs::metadata(&path).map_err(|_| "Vertex subject token file metadata unavailable")?;
    if !metadata.is_file() || metadata.len() > 1_048_576 {
        return Err("Vertex subject token must be a bounded regular file".into());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|_| "Vertex subject token file could not be read")?
        .take(1_048_577)
        .read_to_end(&mut bytes)
        .map_err(|_| "Vertex subject token file could not be read")?;
    if bytes.len() > 1_048_576 {
        return Err("Vertex subject token file exceeds limit".into());
    }
    check_cancelled(signal)?;
    Ok(bytes)
}
fn subject(value: &Value, signal: &CancellationToken) -> Result<String, String> {
    let source = &value["credential_source"];
    if source.get("environment_id").is_some()
        || source.get("executable").is_some()
        || source.get("certificate").is_some()
    {
        return Err("Vertex federation credential source has not been migrated".into());
    }
    let format = source["format"]["type"].as_str().unwrap_or("text");
    if !matches!(format, "text" | "json") {
        return Err("Invalid Vertex subject token format".into());
    }
    let mut bytes = match (source["file"].as_str(), source["url"].as_str()) {
        (Some(path), None) => file(path, signal)?,
        (None, Some(url)) => {
            let headers = source["headers"]
                .as_object()
                .map(|headers| {
                    headers
                        .iter()
                        .map(|(key, value)| {
                            value
                                .as_str()
                                .map(|value| (key.clone(), value.into()))
                                .ok_or("Invalid Vertex subject token header")
                        })
                        .collect::<Result<BTreeMap<_, _>, _>>()
                })
                .transpose()?
                .unwrap_or_default();
            request(reqwest::Method::GET, url, headers, Vec::new(), signal)?
        }
        _ => {
            return Err(
                "Vertex federation requires exactly one file or URL credential source".into(),
            )
        }
    };
    let result = if format == "text" {
        std::str::from_utf8(&bytes)
            .map(str::to_owned)
            .map_err(|_| "Invalid Vertex subject token text".into())
    } else {
        json_body(&bytes).and_then(|body| {
            let key = required(&source["format"], "subject_token_field_name")?;
            required(&body, key).map(str::to_owned)
        })
    };
    bytes.fill(0);
    let subject = result?;
    if subject.is_empty() || subject.len() > 1_048_576 {
        return Err("Vertex subject token is empty or exceeds limit".into());
    }
    Ok(subject)
}
fn expire_time(value: &str) -> Result<u64, String> {
    // RFC3339 UTC timestamps emitted by IAM Credentials, retaining fractional ms.
    let body = value
        .strip_suffix('Z')
        .ok_or("Invalid Vertex impersonation expiry")?;
    let (date, time) = body
        .split_once('T')
        .ok_or("Invalid Vertex impersonation expiry")?;
    let date: Vec<_> = date.split('-').collect();
    let time: Vec<_> = time.split(':').collect();
    if date.len() != 3 || time.len() != 3 {
        return Err("Invalid Vertex impersonation expiry".into());
    }
    let number = |value: &str| {
        value
            .parse::<u64>()
            .map_err(|_| "Invalid Vertex impersonation expiry".to_owned())
    };
    let (year, month, day) = (number(date[0])?, number(date[1])?, number(date[2])?);
    let (hour, minute) = (number(time[0])?, number(time[1])?);
    let (second, fraction) = time[2].split_once('.').unwrap_or((time[2], ""));
    let second = number(second)?;
    let leap = |year: u64| {
        year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
    };
    if !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || hour > 23
        || minute > 59
        || second > 59
        || fraction.len() > 9
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("Invalid Vertex impersonation expiry".into());
    }
    let months = [
        31,
        if leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if day == 0 || day > months[(month - 1) as usize] {
        return Err("Invalid Vertex impersonation expiry".into());
    }
    let mut days = 0;
    for y in 1970..year {
        days += if leap(y) { 366 } else { 365 };
    }
    days += months[..(month - 1) as usize].iter().sum::<u64>() + day - 1;
    let millis = if fraction.is_empty() {
        0
    } else {
        number(&format!("{fraction:0<3}")[..3])?
    };
    Ok((days * 86400 + hour * 3600 + minute * 60 + second) * 1000 + millis)
}
fn impersonate(
    value: &Value,
    source: CachedToken,
    include_delegates: bool,
    signal: &CancellationToken,
) -> Result<CachedToken, String> {
    let url = required(value, "service_account_impersonation_url")?;
    let parsed = credential_endpoint(url)?;
    if !parsed.path().ends_with(":generateAccessToken") {
        return Err("Invalid Vertex impersonation route".into());
    }
    let lifetime = if include_delegates {
        value["lifetime"].as_u64()
    } else {
        value["service_account_impersonation"]["token_lifetime_seconds"].as_u64()
    }
    .unwrap_or(3600);
    if lifetime == 0 || lifetime > 43200 {
        return Err("Invalid Vertex impersonation lifetime".into());
    }
    let mut body = json!({"scope":[VERTEX_SCOPE],"lifetime":format!("{lifetime}s")});
    if include_delegates {
        let delegates = value.get("delegates").cloned().unwrap_or_else(|| json!([]));
        if !delegates.as_array().is_some_and(|values| {
            values
                .iter()
                .all(|value| value.as_str().is_some_and(|value| !value.is_empty()))
        }) {
            return Err("Invalid Vertex impersonation delegates".into());
        }
        body["delegates"] = delegates;
    }
    let bytes = request(
        reqwest::Method::POST,
        url,
        BTreeMap::from([
            ("content-type".into(), "application/json".into()),
            ("authorization".into(), format!("Bearer {}", source.access)),
        ]),
        body.to_string().into_bytes(),
        signal,
    )?;
    let value = json_body(&bytes)?;
    let access = required(&value, "accessToken")?.to_owned();
    super::super::validate_header(&access)?;
    let expires = expire_time(required(&value, "expireTime")?)?;
    if expires <= timestamp() {
        return Err("Vertex impersonation response is expired".into());
    }
    Ok(CachedToken { access, expires })
}
pub(super) fn resolve(
    value: &Value,
    override_url: &str,
    signal: &CancellationToken,
) -> Result<CachedToken, String> {
    resolve_inner(value, override_url, signal, &mut BTreeSet::new(), 0)
}
fn resolve_inner(
    value: &Value,
    override_url: &str,
    signal: &CancellationToken,
    seen: &mut BTreeSet<String>,
    depth: usize,
) -> Result<CachedToken, String> {
    check_cancelled(signal)?;
    if depth > 8 || !value.is_object() {
        return Err("Invalid or excessive Vertex ADC credential nesting".into());
    }
    let key = format!("{:x}", Sha256::digest(value.to_string()));
    if !seen.insert(key.clone()) {
        return Err("Cyclic Vertex ADC source credentials".into());
    }
    let result = match required(value, "type")? {
        "authorized_user" => exchange_token(
            override_url,
            &[
                ("grant_type".into(), "refresh_token".into()),
                ("client_id".into(), required(value, "client_id")?.into()),
                (
                    "client_secret".into(),
                    required(value, "client_secret")?.into(),
                ),
                (
                    "refresh_token".into(),
                    required(value, "refresh_token")?.into(),
                ),
            ],
            signal,
        ),
        "service_account" => exchange_token(
            override_url,
            &[
                (
                    "grant_type".into(),
                    "urn:ietf:params:oauth:grant-type:jwt-bearer".into(),
                ),
                (
                    "assertion".into(),
                    service_account_assertion(value, override_url, timestamp() / 1000)?,
                ),
            ],
            signal,
        ),
        "impersonated_service_account" => {
            let source = resolve_inner(
                &value["source_credentials"],
                override_url,
                signal,
                seen,
                depth + 1,
            )?;
            impersonate(value, source, true, signal)
        }
        "external_account_authorized_user" => {
            static ROTATED: OnceLock<Mutex<BTreeMap<String, String>>> = OnceLock::new();
            let rotated = ROTATED.get_or_init(|| Mutex::new(BTreeMap::new()));
            let cache_key = format!("{key}|{override_url}");
            let refresh = rotated
                .lock()
                .map_err(|_| "Vertex workforce refresh cache unavailable")?
                .get(&cache_key)
                .cloned()
                .unwrap_or(required(value, "refresh_token")?.into());
            required(value, "client_id")?;
            let bytes = request(
                reqwest::Method::POST,
                &token_endpoint(value, override_url, true)?,
                basic(value)?,
                form(&[
                    ("grant_type".into(), "refresh_token".into()),
                    ("refresh_token".into(), refresh),
                ]),
                signal,
            )?;
            let body = json_body(&bytes)?;
            let token = parse_token(&body)?;
            if let Some(refresh) = body["refresh_token"]
                .as_str()
                .filter(|value| !value.is_empty())
            {
                check_cancelled(signal)?;
                let mut rotated = rotated
                    .lock()
                    .map_err(|_| "Vertex workforce refresh cache unavailable")?;
                if rotated.len() >= 64 {
                    rotated.pop_first();
                }
                rotated.insert(cache_key, refresh.into());
            }
            Ok(token)
        }
        "external_account" => {
            let audience = required(value, "audience")?;
            let token_type = required(value, "subject_token_type")?;
            let subject = subject(value, signal)?;
            let mut fields = vec![
                (
                    "grant_type".into(),
                    "urn:ietf:params:oauth:grant-type:token-exchange".into(),
                ),
                ("audience".into(), audience.into()),
                ("scope".into(), VERTEX_SCOPE.into()),
                (
                    "requested_token_type".into(),
                    "urn:ietf:params:oauth:token-type:access_token".into(),
                ),
                ("subject_token".into(), subject),
                ("subject_token_type".into(), token_type.into()),
            ];
            if let Some(project) = value["workforce_pool_user_project"].as_str() {
                if !audience.starts_with("//iam.googleapis.com/locations/")
                    || !audience.contains("/workforcePools/")
                {
                    return Err("Workforce user project requires a workforce audience".into());
                }
                if value["client_id"]
                    .as_str()
                    .is_none_or(|value| value.is_empty())
                {
                    fields.push(("options".into(), json!({"userProject":project}).to_string()));
                }
            }
            let bytes = request(
                reqwest::Method::POST,
                &token_endpoint(value, override_url, false)?,
                basic(value)?,
                form(&fields),
                signal,
            )?;
            let body = json_body(&bytes)?;
            let token = parse_token(&body)?;
            if value["service_account_impersonation_url"].is_string() {
                impersonate(value, token, false, signal)
            } else {
                Ok(token)
            }
        }
        _ => Err("Vertex ADC credential type has not been migrated".into()),
    };
    seen.remove(&key);
    check_cancelled(signal)?;
    result
}
