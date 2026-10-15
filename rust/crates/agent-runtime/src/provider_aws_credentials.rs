//! Bounded native STS credential plans, AWS Query XML and cancellable per-key refresh.
//! Roles start from configured static/source profiles or a configured web token file.
use super::super::aws::{aws_timestamp, sign_aws_v4, AwsCredentials, SigV4Parameters};
use crate::auth::check_cancelled;
use agent_core::types::{timestamp, CancellationToken};
use quick_xml::{events::Event, Reader};
use reqwest::Url;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError};
use std::time::Duration;

#[path = "provider_aws_metadata.rs"]
pub(super) mod metadata;
#[path = "provider_aws_process.rs"]
mod process;
#[path = "provider_aws_sso.rs"]
pub(super) mod sso;

type Profile = BTreeMap<String, String>;
const MAX_CHAIN: usize = 8;
const REFRESH_BEFORE: u64 = 300;
const RESPONSE_LIMIT: usize = 65_536;

pub(super) fn has_credential_configuration(profile: &Profile) -> bool {
    [
        "aws_access_key_id",
        "aws_secret_access_key",
        "aws_session_token",
        "role_arn",
        "source_profile",
        "web_identity_token_file",
        "credential_source",
        "credential_process",
        "sso_start_url",
        "sso_session",
        "sso_account_id",
        "sso_role_name",
        "sso_region",
        "login_session",
        "mfa_serial",
        "saml_assertion",
    ]
    .iter()
    .any(|key| profile.contains_key(*key))
}

pub(super) enum CredentialPlan {
    Static(AwsCredentials),
    Environment {
        credentials: AwsCredentials,
        expiration: Option<u64>,
    },
    Metadata(metadata::MetadataPlan),
    Process {
        command: String,
        workspace: PathBuf,
    },
    Sso(sso::SsoPlan),
    Role {
        fields: Vec<(String, String)>,
        source: Box<CredentialPlan>,
    },
    WebIdentity {
        fields: Vec<(String, String)>,
    },
}
pub(super) struct CredentialResult {
    pub credentials: AwsCredentials,
    pub expiration: Option<u64>,
    /// The exact SSO token snapshot used by this result, not later disk contents.
    pub source_fingerprint: Option<String>,
}
fn static_result(
    credentials: &AwsCredentials,
    expiration: Option<u64>,
    source_fingerprint: Option<String>,
) -> CredentialResult {
    CredentialResult {
        credentials: AwsCredentials {
            access_key_id: credentials.access_key_id.clone(),
            secret_access_key: credentials.secret_access_key.clone(),
            session_token: credentials.session_token.clone(),
        },
        expiration,
        source_fingerprint,
    }
}
fn profile_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 256 || name.bytes().any(|byte| byte.is_ascii_control()) {
        return Err("Invalid AWS source profile name".into());
    }
    Ok(())
}
fn role_fields(profile: &Profile, web: bool) -> Result<Vec<(String, String)>, String> {
    let arn = profile.get("role_arn").ok_or("AWS role ARN is missing")?;
    let parts: Vec<_> = arn.splitn(6, ':').collect();
    if !(20..=2048).contains(&arn.len())
        || parts.len() != 6
        || parts[0] != "arn"
        || !matches!(parts[1], "aws" | "aws-us-gov" | "aws-cn")
        || parts[2] != "iam"
        || !parts[3].is_empty()
        || parts[4].len() != 12
        || !parts[4].bytes().all(|byte| byte.is_ascii_digit())
        || !parts[5].starts_with("role/")
        || parts[5].len() <= 5
        || arn
            .bytes()
            .any(|byte| !byte.is_ascii() || byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err("Invalid AWS role ARN".into());
    }
    let session = profile
        .get("role_session_name")
        .cloned()
        .unwrap_or_else(|| "agent-core-rust".into());
    if !(2..=64).contains(&session.len())
        || !session.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'_' | b'+' | b'=' | b',' | b'.' | b'@' | b'-')
        })
    {
        return Err("Invalid AWS role session name".into());
    }
    let duration = profile
        .get("duration_seconds")
        .map(|duration| {
            duration
                .parse::<u32>()
                .map_err(|_| "Invalid AWS role duration")
        })
        .transpose()?
        .unwrap_or(3600);
    if !(900..=43_200).contains(&duration) {
        return Err("Invalid AWS role duration".into());
    }
    let mut fields = vec![
        (
            "Action".into(),
            if web {
                "AssumeRoleWithWebIdentity"
            } else {
                "AssumeRole"
            }
            .into(),
        ),
        ("Version".into(), "2011-06-15".into()),
        ("RoleArn".into(), arn.clone()),
        ("RoleSessionName".into(), session),
        ("DurationSeconds".into(), duration.to_string()),
    ];
    if let Some(external) = profile.get("external_id") {
        if web {
            return Err("External ID is not a WebIdentity request parameter".into());
        }
        if !(2..=1224).contains(&external.len())
            || !external.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(
                        byte,
                        b'_' | b'+' | b'=' | b',' | b'.' | b'@' | b':' | b'/' | b'-'
                    )
            })
        {
            return Err("Invalid AWS external ID".into());
        }
        fields.push(("ExternalId".into(), external.clone()));
    }
    Ok(fields)
}
/// Build and validate the whole graph before any STS call. No role is assumed on a cycle.
pub(super) fn profile_plan(
    selected: &str,
    load: &dyn Fn(&str) -> Result<Profile, String>,
    read_token: &dyn Fn(&str) -> Result<String, String>,
    additional_leaf: &dyn Fn(&Profile) -> Result<Option<CredentialPlan>, String>,
    signal: &CancellationToken,
) -> Result<CredentialPlan, String> {
    fn visit(
        name: &str,
        load: &dyn Fn(&str) -> Result<Profile, String>,
        read_token: &dyn Fn(&str) -> Result<String, String>,
        additional_leaf: &dyn Fn(&Profile) -> Result<Option<CredentialPlan>, String>,
        signal: &CancellationToken,
        visited: &mut BTreeSet<String>,
        depth: usize,
    ) -> Result<CredentialPlan, String> {
        check_cancelled(signal)?;
        profile_name(name)?;
        if depth >= MAX_CHAIN {
            return Err("AWS source profile chain exceeds limit".into());
        }
        if !visited.insert(name.into()) {
            return Err("AWS source profile cycle detected".into());
        }
        let data = load(name)?;
        let unsupported = ["login_session", "mfa_serial", "saml_assertion"];
        if unsupported.iter().any(|key| data.contains_key(*key)) {
            return Err(
                "AWS login-session, MFA and SAML credential providers have not been migrated"
                    .into(),
            );
        }
        let plan = if data.contains_key("role_arn") {
            if [
                "source_profile",
                "credential_source",
                "web_identity_token_file",
            ]
            .iter()
            .filter(|key| data.contains_key(**key))
            .count()
                != 1
            {
                return Err("AWS role requires exactly one credential source".into());
            }
            if let Some(source) = data.get("source_profile") {
                if data.contains_key("web_identity_token_file") {
                    return Err(
                        "AWS role source profile and WebIdentity token are mutually exclusive"
                            .into(),
                    );
                }
                let fields = role_fields(&data, false)?;
                let source = visit(
                    source,
                    load,
                    read_token,
                    additional_leaf,
                    signal,
                    visited,
                    depth + 1,
                )?;
                CredentialPlan::Role {
                    fields,
                    source: Box::new(source),
                }
            } else if let Some(path) = data.get("web_identity_token_file") {
                let mut fields = role_fields(&data, true)?;
                let token = read_token(path)?;
                validate_token(&token)?;
                fields.push(("WebIdentityToken".into(), token));
                CredentialPlan::WebIdentity { fields }
            } else if data.contains_key("credential_source") {
                let source =
                    additional_leaf(&data)?.ok_or("AWS credential_source is not configured")?;
                CredentialPlan::Role {
                    fields: role_fields(&data, false)?,
                    source: Box::new(source),
                }
            } else {
                return Err("AWS role requires source_profile or web_identity_token_file".into());
            }
        } else {
            if data.contains_key("source_profile") || data.contains_key("web_identity_token_file") {
                return Err("AWS role configuration is incomplete".into());
            }
            // The installed SDK selects complete static credentials before process/SSO leaves.
            if !data.contains_key("aws_access_key_id")
                || !data.contains_key("aws_secret_access_key")
            {
                if let Some(plan) = additional_leaf(&data)? {
                    visited.remove(name);
                    return Ok(plan);
                }
            }
            let credentials = AwsCredentials {
                access_key_id: data.get("aws_access_key_id").cloned().unwrap_or_default(),
                secret_access_key: data
                    .get("aws_secret_access_key")
                    .cloned()
                    .unwrap_or_default(),
                session_token: data.get("aws_session_token").cloned().unwrap_or_default(),
            };
            credentials.validate()?;
            CredentialPlan::Static(credentials)
        };
        visited.remove(name);
        Ok(plan)
    }
    visit(
        selected,
        load,
        read_token,
        additional_leaf,
        signal,
        &mut BTreeSet::new(),
        0,
    )
}
pub(super) fn web_identity_plan(
    arn: String,
    path: &str,
    session: String,
    read_token: &dyn Fn(&str) -> Result<String, String>,
) -> Result<CredentialPlan, String> {
    let mut data = BTreeMap::from([("role_arn".into(), arn)]);
    if !session.is_empty() {
        data.insert("role_session_name".into(), session);
    }
    let mut fields = role_fields(&data, true)?;
    let token = read_token(path)?;
    validate_token(&token)?;
    fields.push(("WebIdentityToken".into(), token));
    Ok(CredentialPlan::WebIdentity { fields })
}
fn validate_token(token: &str) -> Result<(), String> {
    if !(4..=20_000).contains(&token.len()) {
        return Err("Invalid AWS WebIdentity token length".into());
    }
    super::super::validate_header(token)
}
pub(super) fn sts_endpoint(configured: &str, region: &str) -> Result<Url, String> {
    if region.is_empty()
        || !region
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err("Invalid STS region".into());
    }
    let source = if configured.trim().is_empty() {
        format!(
            "https://sts.{region}.{}",
            if region.starts_with("cn-") {
                "amazonaws.com.cn"
            } else {
                "amazonaws.com"
            }
        )
    } else {
        configured.trim().to_owned()
    };
    let url = super::super::clean_url(&source)?;
    let local = url
        .host_str()
        .is_some_and(|host| matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]"));
    if url.scheme() != "https" && !local {
        return Err("STS endpoint must use HTTPS or local loopback HTTP".into());
    }
    Ok(url)
}
fn form(fields: &[(String, String)]) -> Vec<u8> {
    let mut url = Url::parse("https://fixture.invalid").expect("Static form URL");
    url.query_pairs_mut().extend_pairs(
        fields
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    );
    url.query().unwrap_or_default().as_bytes().to_vec()
}
fn fingerprint(
    plan: &CredentialPlan,
    url: &Url,
    region: &str,
    signal: &CancellationToken,
    source_fingerprint: Option<&str>,
) -> Result<String, String> {
    fn part(hash: &mut Sha256, value: &[u8]) {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value);
    }
    fn plan_parts(
        hash: &mut Sha256,
        plan: &CredentialPlan,
        signal: &CancellationToken,
        source_fingerprint: Option<&str>,
    ) -> Result<(), String> {
        check_cancelled(signal)?;
        match plan {
            CredentialPlan::Static(credentials) => {
                part(hash, b"static");
                part(hash, credentials.access_key_id.as_bytes());
                part(hash, credentials.secret_access_key.as_bytes());
                part(hash, credentials.session_token.as_bytes());
            }
            CredentialPlan::Environment {
                credentials,
                expiration,
            } => {
                part(hash, b"environment");
                part(hash, credentials.access_key_id.as_bytes());
                part(hash, credentials.secret_access_key.as_bytes());
                part(hash, credentials.session_token.as_bytes());
                part(hash, &expiration.unwrap_or(0).to_be_bytes());
            }
            CredentialPlan::Metadata(plan) => {
                part(hash, b"metadata");
                let binding = source_fingerprint
                    .map(str::to_owned)
                    .map(Ok)
                    .unwrap_or_else(|| metadata::fingerprint(plan, signal))?;
                part(hash, binding.as_bytes());
            }
            CredentialPlan::Process { command, workspace } => {
                part(hash, b"process");
                part(hash, command.as_bytes());
                part(hash, workspace.as_os_str().as_encoded_bytes());
            }
            CredentialPlan::Sso(plan) => {
                part(hash, b"sso");
                let identity = source_fingerprint
                    .map(str::to_owned)
                    .map(Ok)
                    .unwrap_or_else(|| sso::fingerprint(plan, signal))?;
                part(hash, identity.as_bytes());
            }
            CredentialPlan::Role { fields, source } => {
                part(hash, b"role");
                for (key, value) in fields {
                    part(hash, key.as_bytes());
                    part(hash, value.as_bytes());
                }
                plan_parts(hash, source, signal, source_fingerprint)?;
            }
            CredentialPlan::WebIdentity { fields } => {
                part(hash, b"web");
                for (key, value) in fields {
                    part(hash, key.as_bytes());
                    part(hash, value.as_bytes());
                }
            }
        }
        Ok(())
    }
    let mut hash = Sha256::new();
    part(&mut hash, url.as_str().as_bytes());
    part(&mut hash, region.as_bytes());
    plan_parts(&mut hash, plan, signal, source_fingerprint)?;
    Ok(format!("{:x}", hash.finalize()))
}
fn cancellable_lock<'a, T>(
    lock: &'a Mutex<T>,
    signal: &CancellationToken,
) -> Result<MutexGuard<'a, T>, String> {
    loop {
        check_cancelled(signal)?;
        match lock.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(25)),
            Err(TryLockError::Poisoned(_)) => return Err("AWS credential cache unavailable".into()),
        }
    }
}
#[derive(Default)]
struct CacheState {
    credential: Option<CredentialResult>,
    failure: Option<(u64, String)>,
}
#[derive(Default)]
struct CacheEntry {
    state: Mutex<CacheState>,
    generation: AtomicU64,
}
pub(super) fn resolve_plan(
    plan: &CredentialPlan,
    endpoint: &str,
    region: &str,
    signal: &CancellationToken,
) -> Result<CredentialResult, String> {
    check_cancelled(signal)?;
    if let CredentialPlan::Static(credentials) = plan {
        return Ok(static_result(credentials, None, None));
    }
    if let CredentialPlan::Environment {
        credentials,
        expiration,
    } = plan
    {
        if expiration.is_some_and(|expiration| expiration <= timestamp() / 1000) {
            return Err("AWS environment credentials are expired".into());
        }
        return Ok(static_result(credentials, *expiration, None));
    }
    let url = sts_endpoint(endpoint, region)?;
    let key = fingerprint(plan, &url, region, signal, None)?;
    static CACHE: OnceLock<Mutex<BTreeMap<String, Arc<CacheEntry>>>> = OnceLock::new();
    let entry = {
        let mut cache =
            cancellable_lock(CACHE.get_or_init(|| Mutex::new(BTreeMap::new())), signal)?;
        if !cache.contains_key(&key) && cache.len() >= 64 {
            let removable = cache
                .iter()
                .find(|(_, entry)| Arc::strong_count(entry) == 1)
                .map(|(key, _)| key.clone());
            if let Some(key) = removable {
                cache.remove(&key);
            } else {
                return Err("AWS credential cache capacity reached".into());
            }
        }
        cache
            .entry(key.clone())
            .or_insert_with(|| Arc::new(CacheEntry::default()))
            .clone()
    };
    let observed_generation = entry.generation.load(Ordering::Acquire);
    let mut cached = cancellable_lock(&entry.state, signal)?;
    let now = timestamp() / 1000;
    if let Some(result) = cached.credential.as_ref().filter(|result| {
        result
            .expiration
            .is_some_and(|expires| expires > now.saturating_add(REFRESH_BEFORE))
    }) {
        return Ok(static_result(
            &result.credentials,
            result.expiration,
            result.source_fingerprint.clone(),
        ));
    }
    if let Some((_, error)) = cached
        .failure
        .as_ref()
        .filter(|(generation, _)| *generation > observed_generation)
    {
        return Err(error.clone());
    }
    let result: Result<CredentialResult, String> = (|| {
        if let CredentialPlan::Process { command, workspace } = plan {
            return process::resolve(command, workspace, signal);
        }
        if let CredentialPlan::Sso(plan) = plan {
            return sso::resolve(plan, signal);
        }
        if let CredentialPlan::Metadata(plan) = plan {
            return metadata::resolve(plan, signal);
        }
        let (fields, source) = match plan {
            CredentialPlan::Role { fields, source } => (
                fields,
                Some(resolve_plan(source, endpoint, region, signal)?),
            ),
            CredentialPlan::WebIdentity { fields } => (fields, None),
            CredentialPlan::Static(_)
            | CredentialPlan::Environment { .. }
            | CredentialPlan::Metadata(_)
            | CredentialPlan::Process { .. }
            | CredentialPlan::Sso(_) => {
                unreachable!("Leaf returned before STS exchange")
            }
        };
        let mut result = exchange(
            &url,
            region,
            fields,
            source.as_ref().map(|source| &source.credentials),
            signal,
        )?;
        result.source_fingerprint = source.and_then(|source| source.source_fingerprint);
        check_cancelled(signal)?;
        Ok(result)
    })();
    let generation = entry.generation.load(Ordering::Acquire).saturating_add(1);
    let returned = match result {
        Ok(result) => {
            let returned = static_result(
                &result.credentials,
                result.expiration,
                result.source_fingerprint.clone(),
            );
            cached.credential = Some(result);
            cached.failure = None;
            Ok(returned)
        }
        Err(error) => {
            // A cancelled owner does not cancel independent waiting callers.
            cached.failure = (!signal.is_cancelled()).then(|| (generation, error.clone()));
            Err(error)
        }
    };
    entry.generation.store(generation, Ordering::Release);
    if let Ok(result) = &returned {
        // Bind only the snapshot actually used by SSO, including a rotation that
        // this call committed. Never relabel credentials with a later external
        // login observed on disk. In-flight waiters retain their original Arc.
        // Finalize this already-produced binding even if the request is cancelled
        // immediately after SSO returns. This branch hashes an owned snapshot and
        // never performs file/network I/O.
        if let Some(binding) = result.source_fingerprint.as_deref() {
            let housekeeping = CancellationToken::new();
            let fresh = fingerprint(plan, &url, region, &housekeeping, Some(binding))?;
            if fresh != key {
                if let Ok(mut cache) = cancellable_lock(
                    CACHE.get_or_init(|| Mutex::new(BTreeMap::new())),
                    &housekeeping,
                ) {
                    if cache.get(&key).is_some_and(|old| Arc::ptr_eq(old, &entry)) {
                        cache.remove(&key);
                    }
                    cache.entry(fresh).or_insert_with(|| entry.clone());
                } else {
                    // Never leave credentials labelled with the wrong old key.
                    cached.credential = None;
                }
            }
        }
    }
    returned
}
fn exchange(
    url: &Url,
    region: &str,
    fields: &[(String, String)],
    source: Option<&AwsCredentials>,
    signal: &CancellationToken,
) -> Result<CredentialResult, String> {
    let body = form(fields);
    let mut headers = BTreeMap::from([(
        "content-type".into(),
        "application/x-www-form-urlencoded".into(),
    )]);
    if let Some(credentials) = source {
        headers = sign_aws_v4(SigV4Parameters {
            method: "POST",
            url,
            body: &body,
            headers: &headers,
            credentials,
            region,
            service: "sts",
            timestamp: &aws_timestamp(timestamp() / 1000)?,
            checksum_header: true,
        })?
        .headers;
    }
    let action = fields
        .iter()
        .find(|(name, _)| name == "Action")
        .map(|(_, value)| value.as_str())
        .ok_or("STS action missing")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "STS credential runtime failed")?;
    let mut bytes = runtime.block_on(async {
        let builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120));
        let local = url
            .host_str()
            .is_some_and(|host| matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]"));
        let client = if local { builder.no_proxy() } else { builder }
            .build()
            .map_err(|_| "STS credential client failed")?;
        let request = async {
            let mut request = client.post(url.clone()).body(body);
            for (name, value) in headers {
                request = request.header(name, value);
            }
            let mut response = request
                .send()
                .await
                .map_err(|_| "STS credential request failed")?;
            if !response.status().is_success() {
                return Err(format!(
                    "STS credential endpoint returned HTTP {}",
                    response.status().as_u16()
                ));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| "STS credential response failed")?
            {
                if bytes.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
                    return Err("STS credential response exceeds limit".into());
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
        tokio::select! {response=request=>response,_=cancelled=>Err("Operation aborted".into())}
    })?;
    check_cancelled(signal)?;
    let result = parse_response(&bytes, action);
    bytes.fill(0);
    let result = result?;
    if result
        .expiration
        .is_none_or(|expires| expires <= timestamp() / 1000)
    {
        return Err("STS credential response is already expired".into());
    }
    check_cancelled(signal)?;
    Ok(result)
}
fn parse_response(bytes: &[u8], action: &str) -> Result<CredentialResult, String> {
    if bytes.len() > RESPONSE_LIMIT || std::str::from_utf8(bytes).is_err() {
        return Err("Invalid STS credential XML".into());
    }
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().expand_empty_elements = true;
    let expected_root = format!("{action}Response");
    let expected_result = format!("{action}Result");
    let mut path: Vec<String> = vec![];
    let mut values: BTreeMap<String, String> = BTreeMap::new();
    let mut root_seen = false;
    let mut result_seen = false;
    let mut credentials_seen = false;
    loop {
        match reader
            .read_event()
            .map_err(|_| "Invalid STS credential XML")?
        {
            Event::Start(start) => {
                let name = std::str::from_utf8(start.name().as_ref())
                    .map_err(|_| "Invalid STS XML tag")?
                    .to_owned();
                if path.len() >= 16 || name.contains(':') {
                    return Err("Invalid STS XML nesting or tag".into());
                }
                for attribute in start.attributes() {
                    let attribute = attribute.map_err(|_| "Invalid STS XML attribute")?;
                    let value = attribute
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .map_err(|_| "Invalid STS XML attribute")?;
                    if attribute.key.as_ref() == b"xmlns"
                        && value != "https://sts.amazonaws.com/doc/2011-06-15/"
                    {
                        return Err("Invalid STS XML namespace".into());
                    }
                }
                if path.is_empty() {
                    if root_seen || name != expected_root {
                        return Err("Unexpected STS XML root".into());
                    }
                    root_seen = true;
                }
                if path.len() == 1 && name == expected_result {
                    if result_seen {
                        return Err("Duplicate STS result".into());
                    }
                    result_seen = true;
                }
                if path.len() == 2 && path[1] == expected_result && name == "Credentials" {
                    if credentials_seen {
                        return Err("Duplicate STS credentials".into());
                    }
                    credentials_seen = true;
                }
                if path.len() == 3
                    && path[1] == expected_result
                    && path[2] == "Credentials"
                    && matches!(
                        name.as_str(),
                        "AccessKeyId" | "SecretAccessKey" | "SessionToken" | "Expiration"
                    )
                    && values.insert(name.clone(), String::new()).is_some()
                {
                    return Err("Duplicate STS credential field".into());
                }
                if path.len() >= 4 && path[2] == "Credentials" {
                    return Err("Nested STS credential field".into());
                }
                path.push(name);
            }
            Event::End(_) => {
                path.pop().ok_or("Invalid STS XML closing tag")?;
            }
            Event::Text(text) => {
                let text = text.decode().map_err(|_| "Invalid STS XML text")?;
                append_text(&path, &expected_result, &mut values, &text)?;
            }
            Event::CData(text) => {
                let text = text.decode().map_err(|_| "Invalid STS XML text")?;
                append_text(&path, &expected_result, &mut values, &text)?;
            }
            Event::GeneralRef(reference) => {
                let resolved = if let Some(character) = reference
                    .resolve_char_ref()
                    .map_err(|_| "Invalid STS XML reference")?
                {
                    character.to_string()
                } else {
                    match reference
                        .decode()
                        .map_err(|_| "Invalid STS XML reference")?
                        .as_ref()
                    {
                        "amp" => "&",
                        "lt" => "<",
                        "gt" => ">",
                        "quot" => "\"",
                        "apos" => "'",
                        _ => return Err("Unsupported STS XML entity".into()),
                    }
                    .into()
                };
                append_text(&path, &expected_result, &mut values, &resolved)?;
            }
            Event::DocType(_) | Event::PI(_) => {
                return Err("Unsupported STS XML declaration".into())
            }
            Event::Eof => break,
            Event::Decl(_) | Event::Comment(_) => {}
            Event::Empty(_) => unreachable!("Empty elements expanded"),
        }
    }
    if !path.is_empty() || !root_seen || !result_seen || !credentials_seen {
        return Err("Incomplete STS credential XML".into());
    }
    let mut required = |key: &str| {
        values
            .remove(key)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "STS credential field missing".to_owned())
    };
    let credentials = AwsCredentials {
        access_key_id: required("AccessKeyId")?,
        secret_access_key: required("SecretAccessKey")?,
        session_token: required("SessionToken")?,
    };
    credentials.validate()?;
    let expiration = parse_expiration(&required("Expiration")?)?;
    Ok(CredentialResult {
        credentials,
        expiration: Some(expiration),
        source_fingerprint: None,
    })
}
fn append_text(
    path: &[String],
    result: &str,
    values: &mut BTreeMap<String, String>,
    text: &str,
) -> Result<(), String> {
    if path.len() == 4 && path[1] == result && path[2] == "Credentials" {
        if let Some(value) = values.get_mut(&path[3]) {
            value.push_str(text);
        }
    } else if path.is_empty() && !text.trim().is_empty() {
        return Err("Text outside STS XML root".into());
    }
    Ok(())
}
pub(super) fn parse_expiration(source: &str) -> Result<u64, String> {
    let bad = || "Invalid STS credential expiration".to_owned();
    let bytes = source.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return Err(bad());
    }
    let number = |start: usize, end: usize| {
        source
            .get(start..end)
            .filter(|part| part.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|part| part.parse::<i64>().ok())
            .ok_or_else(bad)
    };
    let year = number(0, 4)?;
    let month = number(5, 7)?;
    let day = number(8, 10)?;
    let hour = number(11, 13)?;
    let minute = number(14, 16)?;
    let second = number(17, 19)?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => return Err(bad()),
    };
    if !(1970..=9999).contains(&year)
        || !(1..=max_day).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&second)
    {
        return Err(bad());
    }
    let mut zone = 19;
    if bytes.get(zone) == Some(&b'.') {
        zone += 1;
        let first = zone;
        while bytes.get(zone).is_some_and(u8::is_ascii_digit) {
            zone += 1;
        }
        if zone == first {
            return Err(bad());
        }
    }
    let offset = if source.get(zone..) == Some("Z") {
        0
    } else {
        let tail = source.get(zone..).ok_or_else(bad)?;
        let data = tail.as_bytes();
        if data.len() != 6 || !matches!(data[0], b'+' | b'-') || data[3] != b':' {
            return Err(bad());
        }
        let h = number(zone + 1, zone + 3)?;
        let m = number(zone + 4, zone + 6)?;
        if h > 23 || m > 59 {
            return Err(bad());
        }
        (h * 3600 + m * 60) * if data[0] == b'+' { 1 } else { -1 }
    };
    let adjusted = year - i64::from(month <= 2);
    let era = adjusted / 400;
    let yoe = adjusted - era * 400;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    u64::try_from(days * 86400 + hour * 3600 + minute * 60 + second - offset).map_err(|_| bad())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expiration_dates_validate_calendar_fraction_timezone_and_epoch() {
        assert_eq!(parse_expiration("1970-01-01T00:00:00Z").unwrap(), 0);
        assert_eq!(
            parse_expiration("2024-02-29T23:59:59.999Z").unwrap(),
            1_709_251_199
        );
        assert_eq!(
            parse_expiration("2024-03-01T00:59:59+01:00").unwrap(),
            1_709_251_199
        );
        for value in [
            "2023-02-29T00:00:00Z",
            "2024-02-30T00:00:00Z",
            "2024-01-01T24:00:00Z",
            "2024-01-01T00:00:60Z",
            "2024-01-01T00:00:00.Z",
            "2024-01-01T00:00:00",
            "2024-01-01T00:00:00+24:00",
        ] {
            assert!(parse_expiration(value).is_err());
        }
    }
    #[test]
    fn xml_credentials_reject_duplicates_dtd_root_errors_and_nonsecret_failures() {
        let xml=br#"<AssumeRoleResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/"><AssumeRoleResult><Credentials><AccessKeyId>ASIAFIXTURE</AccessKeyId><SecretAccessKey>secret&amp;fixture</SecretAccessKey><SessionToken><![CDATA[session-fixture]]></SessionToken><Expiration>2030-01-01T00:00:00Z</Expiration></Credentials></AssumeRoleResult></AssumeRoleResponse>"#;
        let parsed = parse_response(xml, "AssumeRole").unwrap();
        assert_eq!(parsed.credentials.secret_access_key, "secret&fixture");
        let source = std::str::from_utf8(xml).unwrap();
        for invalid in [
            source.replace(
                "</Credentials>",
                "<AccessKeyId>duplicate</AccessKeyId></Credentials>",
            ),
            source
                .replace("<SecretAccessKey>", "<SecretAccessKey><nested>")
                .replace("</SecretAccessKey>", "</nested></SecretAccessKey>"),
            source.replace("AssumeRoleResponse", "OtherResponse"),
            format!("{source}{source}"),
            format!("<!DOCTYPE foo [<!ENTITY x SYSTEM 'secret-file'>]>{source}"),
            source.replace("&amp;", "&unknown;"),
            source.replace("2030-01-01", "2023-02-29"),
            source.replace(
                "https://sts.amazonaws.com/doc/2011-06-15/",
                "secret-namespace",
            ),
            source[..source.len() - 1].into(),
        ] {
            let error = parse_response(invalid.as_bytes(), "AssumeRole")
                .err()
                .unwrap();
            for secret in ["secret&fixture", "secret-file", "secret-namespace"] {
                assert!(!error.contains(secret));
            }
        }
    }
}
