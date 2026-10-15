//! Native IAM Identity Center cached-token credentials. No browser/login or ambient
//! file reads occur here: the caller supplies the complete selected SSO plan.
use super::{parse_expiration, CredentialResult};
use crate::auth::check_cancelled;
use crate::provider::{aws_timestamp, clean_url, AwsCredentials};
use agent_core::types::{timestamp, CancellationToken};
use reqwest::{Method, Url};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(windows)]
use std::os::windows::{ffi::OsStrExt, fs::MetadataExt, io::FromRawHandle};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError};
use std::time::{Duration, Instant};
#[cfg(windows)]
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
#[cfg(windows)]
use windows_sys::Win32::Security::{
    GetFileSecurityW, DACL_SECURITY_INFORMATION, SECURITY_ATTRIBUTES,
};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, MoveFileW, ReplaceFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_WRITE,
};

const FILE_LIMIT: usize = 1_048_576;
const RESPONSE_LIMIT: usize = 65_536;
const REFRESH_BEFORE: u64 = 300;
const LOCK_LIMIT: usize = 64;
const LOCK_WAIT: Duration = Duration::from_secs(30);
const LOGIN_REQUIRED: &str = "AWS SSO login required: cached token is missing, invalid or expired";

/// Identity fields are configuration, not secrets. The enclosing Bedrock module
/// is private; visibility also permits its profile parser to construct this plan.
#[derive(Clone)]
pub(in super::super) struct SsoPlan {
    pub(in super::super) start_url: String,
    pub(in super::super) region: String,
    pub(in super::super) account_id: String,
    pub(in super::super) role_name: String,
    pub(in super::super) session_name: Option<String>,
    pub(in super::super) cache_dir: PathBuf,
    pub(in super::super) endpoint: String,
    pub(in super::super) oidc_endpoint: String,
}

fn validate_plan(plan: &SsoPlan) -> Result<(), String> {
    let start = clean_url(&plan.start_url).map_err(|_| "Invalid AWS SSO start URL")?;
    if start.scheme() != "https"
        || plan.region.is_empty()
        || plan.region.len() > 64
        || !plan
            .region
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        || plan.account_id.len() != 12
        || !plan.account_id.bytes().all(|b| b.is_ascii_digit())
        || plan.role_name.is_empty()
        || plan.role_name.len() > 256
        || plan.role_name.bytes().any(|b| b.is_ascii_control())
        || plan.session_name.as_ref().is_some_and(|name| {
            name.is_empty() || name.len() > 256 || name.bytes().any(|b| b.is_ascii_control())
        })
    {
        return Err("Invalid AWS SSO configuration".into());
    }
    endpoint(plan, false)?;
    endpoint(plan, true)?;
    Ok(())
}

fn endpoint(plan: &SsoPlan, oidc: bool) -> Result<Url, String> {
    let configured = if oidc {
        &plan.oidc_endpoint
    } else {
        &plan.endpoint
    };
    let source = if configured.is_empty() {
        let service = if oidc { "oidc" } else { "portal.sso" };
        let suffix = if plan.region.starts_with("cn-") {
            "amazonaws.com.cn"
        } else {
            "amazonaws.com"
        };
        format!("https://{service}.{}.{suffix}", plan.region)
    } else {
        configured.clone()
    };
    let mut url = clean_url(&source).map_err(|_| "Invalid AWS SSO endpoint")?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if url.scheme() != "https" && !loopback {
        return Err("AWS SSO endpoint must use HTTPS or loopback HTTP".into());
    }
    let path = if oidc {
        "token"
    } else {
        "federation/credentials"
    };
    url.set_path(&format!("{}/{path}", url.path().trim_end_matches('/')));
    Ok(url)
}

fn cache_path(plan: &SsoPlan) -> PathBuf {
    let id = plan.session_name.as_deref().unwrap_or(&plan.start_url);
    let digest = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, id.as_bytes());
    let hash: String = digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    plan.cache_dir.join(format!("{hash}.json"))
}

struct CachedToken {
    bytes: Vec<u8>,
    value: Value,
    expires: u64,
    #[cfg(not(windows))]
    permissions: std::fs::Permissions,
}
enum CacheReadError {
    Missing,
    Other(String),
}
impl From<String> for CacheReadError {
    fn from(error: String) -> Self {
        Self::Other(error)
    }
}
impl From<&str> for CacheReadError {
    fn from(error: &str) -> Self {
        Self::Other(error.into())
    }
}
impl CacheReadError {
    fn message(self) -> String {
        match self {
            Self::Missing => LOGIN_REQUIRED.into(),
            Self::Other(error) => error,
        }
    }
}
fn read_cache(
    path: &Path,
    plan: &SsoPlan,
    signal: &CancellationToken,
) -> Result<CachedToken, String> {
    read_cache_once(path, plan, signal).map_err(CacheReadError::message)
}
fn read_cache_once(
    path: &Path,
    plan: &SsoPlan,
    signal: &CancellationToken,
) -> Result<CachedToken, CacheReadError> {
    check_cancelled(signal)?;
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            CacheReadError::Missing
        } else {
            LOGIN_REQUIRED.into()
        }
    })?;
    if !metadata.is_file() || is_link(&metadata) || metadata.len() > FILE_LIMIT as u64 {
        return Err("Invalid AWS SSO token cache file".into());
    }
    let mut file = File::open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            CacheReadError::Missing
        } else {
            "AWS SSO token cache cannot be read".into()
        }
    })?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        check_cancelled(signal)?;
        let count = file
            .read(&mut chunk)
            .map_err(|_| "AWS SSO token cache cannot be read")?;
        if count == 0 {
            break;
        }
        if bytes.len().saturating_add(count) > FILE_LIMIT {
            return Err("AWS SSO token cache exceeds limit".into());
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid AWS SSO token cache JSON")?;
    if !value.is_object() {
        return Err("Invalid AWS SSO token cache JSON".into());
    }
    secret(&value, "accessToken")?;
    let expires = value
        .get("expiresAt")
        .and_then(Value::as_str)
        .ok_or(LOGIN_REQUIRED)?;
    let expires = parse_expiration(expires).map_err(|_| LOGIN_REQUIRED)?;
    for (name, expected) in [("startUrl", &plan.start_url), ("region", &plan.region)] {
        // A modern profile may configure the issuer URL while its session cache
        // records the portal start URL; its SHA1 session-name key binds identity.
        if name == "startUrl" && plan.session_name.is_some() {
            continue;
        }
        if value
            .get(name)
            .is_some_and(|value| value.as_str() != Some(expected))
        {
            return Err("AWS SSO token cache identity does not match configuration".into());
        }
    }
    check_cancelled(signal)?;
    Ok(CachedToken {
        bytes,
        value,
        expires,
        #[cfg(not(windows))]
        permissions: metadata.permissions(),
    })
}
fn refresh_in_progress(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(sidecar(path)) {
        Ok(metadata) if metadata.is_file() && !is_link(&metadata) && metadata.len() <= 256 => {
            Ok(true)
        }
        Ok(_) => Err("Invalid AWS SSO cache lock file".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("AWS SSO cache lock cannot be inspected".into()),
    }
}
fn read_published_cache(
    path: &Path,
    plan: &SsoPlan,
    signal: &CancellationToken,
) -> Result<CachedToken, String> {
    let started = Instant::now();
    let mut name_reappeared = false;
    loop {
        check_cancelled(signal)?;
        if refresh_in_progress(path)? {
            if started.elapsed() >= LOCK_WAIT {
                return Err("AWS SSO cache lock timed out".into());
            }
            std::thread::sleep(Duration::from_millis(25));
            continue;
        }
        match read_cache_once(path, plan, signal) {
            Ok(cached) => return Ok(cached),
            Err(CacheReadError::Missing) => {
                // ReplaceFileW can briefly remove the old name while moving it
                // to its backup. Observe the writer's existing sidecar; never
                // create a lock merely to read a fresh, possibly read-only cache.
                if refresh_in_progress(path)? {
                    continue;
                }
                // The writer may have committed and removed its sidecar between
                // the failed open and inspection. Retry this publication only
                // when the formerly missing regular name has actually reappeared.
                if !name_reappeared && std::fs::symlink_metadata(path).is_ok() {
                    name_reappeared = true;
                    continue;
                }
                return Err(LOGIN_REQUIRED.into());
            }
            Err(error) => return Err(error.message()),
        }
    }
}
#[cfg(windows)]
fn is_link(metadata: &std::fs::Metadata) -> bool {
    metadata.file_attributes() & 0x400 != 0
}
#[cfg(not(windows))]
fn is_link(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn secret<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 16_384
                && !value.bytes().any(|b| b.is_ascii_control())
        })
        .ok_or_else(|| LOGIN_REQUIRED.into())
}

/// Includes token bytes, rather than mtime, to invalidate credentials on external
/// login even when the cache replacement has identical timestamp and length.
pub(super) fn fingerprint(plan: &SsoPlan, signal: &CancellationToken) -> Result<String, String> {
    validate_plan(plan)?;
    let path = cache_path(plan);
    let token = read_published_cache(&path, plan, signal)?;
    check_cancelled(signal)?;
    Ok(token_fingerprint(plan, &token.bytes))
}
fn token_fingerprint(plan: &SsoPlan, token_bytes: &[u8]) -> String {
    let mut hash = Sha256::new();
    for part in [
        Some(plan.start_url.as_str()),
        Some(&plan.region),
        Some(&plan.account_id),
        Some(&plan.role_name),
        plan.session_name.as_deref(),
        Some(&plan.endpoint),
        Some(&plan.oidc_endpoint),
    ] {
        let bytes = part.unwrap_or("").as_bytes();
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    let path = cache_path(plan);
    let path_bytes = path.as_os_str().as_encoded_bytes();
    hash.update((path_bytes.len() as u64).to_be_bytes());
    hash.update(path_bytes);
    hash.update((token_bytes.len() as u64).to_be_bytes());
    hash.update(token_bytes);
    format!("{:x}", hash.finalize())
}

type Locks = BTreeMap<PathBuf, Arc<Mutex<()>>>;
static LOCKS: OnceLock<Mutex<Locks>> = OnceLock::new();
fn local_lock(path: &Path) -> Result<Arc<Mutex<()>>, String> {
    // The containing directory remains stable while ReplaceFileW publishes a
    // replacement; canonicalizing the disappearing file name would race it.
    let key = std::fs::canonicalize(path.parent().ok_or(LOGIN_REQUIRED)?)
        .map_err(|_| LOGIN_REQUIRED)?
        .join(path.file_name().ok_or(LOGIN_REQUIRED)?);
    let mut locks = LOCKS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .map_err(|_| "AWS SSO cache lock failed")?;
    if let Some(lock) = locks.get(&key) {
        return Ok(Arc::clone(lock));
    }
    if locks.len() >= LOCK_LIMIT {
        let unused = locks
            .iter()
            .find_map(|(key, value)| (Arc::strong_count(value) == 1).then(|| key.clone()));
        if let Some(key) = unused {
            locks.remove(&key);
        } else {
            return Err("AWS SSO cache lock capacity reached".into());
        }
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(key, Arc::clone(&lock));
    Ok(lock)
}
fn wait_lock<'a>(
    lock: &'a Mutex<()>,
    signal: &CancellationToken,
) -> Result<MutexGuard<'a, ()>, String> {
    let started = Instant::now();
    loop {
        check_cancelled(signal)?;
        match lock.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(_)) => return Err("AWS SSO cache lock failed".into()),
            Err(TryLockError::WouldBlock) => {}
        }
        if started.elapsed() >= LOCK_WAIT {
            return Err("AWS SSO cache lock timed out".into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
fn nonce() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| "AWS SSO cache lock random source failed")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn sidecar(path: &Path) -> PathBuf {
    path.with_extension("json.agent-core-sso.lock")
}
struct FileLock {
    path: PathBuf,
    owner: String,
}
impl FileLock {
    fn acquire(path: &Path, signal: &CancellationToken) -> Result<Self, String> {
        let path = sidecar(path);
        let owner = format!("{}:{}", std::process::id(), nonce()?);
        let started = Instant::now();
        loop {
            check_cancelled(signal)?;
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    let owned = Self { path, owner };
                    let written = file
                        .write_all(owned.owner.as_bytes())
                        .and_then(|_| file.sync_all());
                    drop(file);
                    if written.is_err() {
                        // A failed initial write may be short. Only remove the
                        // just-created file when its bytes remain our prefix.
                        if std::fs::read(&owned.path)
                            .is_ok_and(|bytes| owned.owner.as_bytes().starts_with(&bytes))
                        {
                            let _ = std::fs::remove_file(&owned.path);
                        }
                        return Err("AWS SSO cache lock cannot be written".into());
                    }
                    check_cancelled(signal)?;
                    return Ok(owned);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err("AWS SSO cache lock cannot be created".into()),
            }
            if started.elapsed() >= LOCK_WAIT {
                return Err("AWS SSO cache lock timed out".into());
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}
impl Drop for FileLock {
    fn drop(&mut self) {
        // Never remove an external/replaced lock. Abandoned locks time out, rather
        // than guessing process liveness and risking concurrent refresh rotation.
        if std::fs::read(&self.path).is_ok_and(|value| value == self.owner.as_bytes()) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
#[cfg(windows)]
fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}
#[cfg(windows)]
fn file_dacl(path: &Path) -> Result<Vec<u32>, String> {
    let path = wide(path);
    let mut needed = 0;
    // SECURITY_DESCRIPTOR storage is u32-aligned, not a byte-vector cast.
    unsafe {
        GetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    if needed == 0 || needed > 65_536 {
        return Err("AWS SSO cache permissions cannot be read".into());
    }
    let mut descriptor = vec![0u32; (needed as usize).div_ceil(4)];
    if unsafe {
        GetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION,
            descriptor.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err("AWS SSO cache permissions cannot be read".into());
    }
    Ok(descriptor)
}
#[cfg(windows)]
fn create_temporary(original: &Path, temporary: &Path) -> Result<File, String> {
    let descriptor = file_dacl(original)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.as_ptr().cast_mut().cast(),
        bInheritHandle: 0,
    };
    // The old DACL applies from the instant the file exists, not after opening.
    // There is no permissive empty-file window in which a read handle can persist.
    let handle = unsafe {
        CreateFileW(
            wide(temporary).as_ptr(),
            FILE_GENERIC_WRITE,
            0,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err("AWS SSO cache private temporary file cannot be created".into());
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}
#[cfg(not(windows))]
fn create_temporary(_original: &Path, temporary: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options
        .open(temporary)
        .map_err(|_| "AWS SSO cache private temporary file cannot be created".into())
}
#[cfg(windows)]
fn matches_original(path: &Path, before: &[u8]) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.is_file() || is_link(&metadata) || metadata.len() != before.len() as u64 {
        return false;
    }
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut bytes = Vec::new();
    file.take(before.len() as u64 + 1)
        .read_to_end(&mut bytes)
        .is_ok()
        && bytes == before
}
#[cfg(windows)]
fn recover_backup(original: &Path, backup: &Path, before: &[u8]) -> Result<(), String> {
    match std::fs::symlink_metadata(original) {
        Ok(_) if matches_original(original, before) => {
            if matches_original(backup, before) {
                std::fs::remove_file(backup)
                    .map_err(|_| "AWS SSO cache replacement failed; recovery backup retained")?;
            }
            Ok(())
        }
        Ok(_) => Err("AWS SSO cache replacement failed; recovery backup retained".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if !matches_original(backup, before)
                || unsafe { MoveFileW(wide(backup).as_ptr(), wide(original).as_ptr()) } == 0
            {
                return Err("AWS SSO cache replacement failed; recovery backup retained".into());
            }
            // MoveFileW does not overwrite an independently created target.
            Ok(())
        }
        Err(_) => Err("AWS SSO cache replacement failed; recovery backup retained".into()),
    }
}
#[cfg(windows)]
fn replace_file(original: &Path, temporary: &Path, before: &[u8]) -> Result<(), String> {
    let backup = original.with_extension(format!("json.agent-core-{}.backup", nonce()?));
    if std::fs::symlink_metadata(&backup).is_ok() {
        return Err("AWS SSO cache recovery backup already exists".into());
    }
    if unsafe {
        ReplaceFileW(
            wide(original).as_ptr(),
            wide(temporary).as_ptr(),
            wide(&backup).as_ptr(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    } == 0
    {
        // Rare Win32 partial failures can move the original name. Retain the
        // owned backup on any recovery failure, never overwrite external data.
        if backup.exists() {
            recover_backup(original, &backup, before)?;
        }
        return Err("AWS SSO token cache cannot be replaced".into());
    }
    if !matches_original(&backup, before) {
        return Err("AWS SSO cache committed; unexpected recovery backup retained".into());
    }
    std::fs::remove_file(&backup)
        .map_err(|_| "AWS SSO cache committed; recovery backup cleanup failed")?;
    Ok(())
}
#[cfg(not(windows))]
fn replace_file(original: &Path, temporary: &Path, _before: &[u8]) -> Result<(), String> {
    std::fs::rename(temporary, original)
        .map_err(|_| "AWS SSO token cache cannot be replaced".into())
}
fn commit_cache(
    path: &Path,
    plan: &SsoPlan,
    before: &CachedToken,
    refreshed: &Value,
    signal: &CancellationToken,
) -> Result<(), String> {
    check_cancelled(signal)?;
    let bytes =
        serde_json::to_vec_pretty(refreshed).map_err(|_| "AWS SSO cache JSON cannot be written")?;
    if bytes.len() > FILE_LIMIT {
        return Err("AWS SSO token cache exceeds limit".into());
    }
    let temporary = path.with_extension(format!("json.agent-core-{}.tmp", nonce()?));
    let mut file = create_temporary(path, &temporary)?;
    let _temporary = TempFile(temporary.clone());
    // ReplaceFileW keeps the target's ACL and fails if ACL preservation fails
    // (no IGNORE_ACL flag). Unix mode is restored on the private temporary file.
    #[cfg(not(windows))]
    let permissions = file
        .set_permissions(before.permissions.clone())
        .map_err(|_| "AWS SSO cache permissions cannot be preserved");
    #[cfg(windows)]
    let permissions: Result<(), &str> = Ok(());
    let written = permissions.and_then(|_| {
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| "AWS SSO token cache cannot be written")
    });
    // Close the no-sharing Windows handle before the temporary guard cleans up
    // a failed write; otherwise RemoveFile would leave a secret-bearing temp.
    drop(file);
    written?;
    check_cancelled(signal)?;
    if read_cache(path, plan, signal)?.bytes != before.bytes {
        return Err("AWS SSO token cache changed during refresh".into());
    }
    check_cancelled(signal)?;
    replace_file(path, &temporary, &before.bytes)?;
    // Atomic replacement is the credential commit point. A later cancelled role
    // request must not restore the now-invalid, rotated refresh token.
    Ok(())
}

fn refresh_ready(token: &CachedToken, now: u64) -> bool {
    ["clientId", "clientSecret", "refreshToken"]
        .iter()
        .all(|name| secret(&token.value, name).is_ok())
        && token
            .value
            .get("registrationExpiresAt")
            .and_then(Value::as_str)
            .and_then(|value| parse_expiration(value).ok())
            .is_some_and(|expires| expires > now)
}

pub(super) fn resolve(
    plan: &SsoPlan,
    signal: &CancellationToken,
) -> Result<CredentialResult, String> {
    check_cancelled(signal)?;
    validate_plan(plan)?;
    let path = cache_path(plan);
    let mut cached = read_published_cache(&path, plan, signal)?;
    let now = timestamp() / 1000;
    if plan.session_name.is_some() && cached.expires <= now.saturating_add(REFRESH_BEFORE) {
        let lock = local_lock(&path)?;
        let _local = wait_lock(&lock, signal)?;
        let _file = FileLock::acquire(&path, signal)?;
        cached = read_cache(&path, plan, signal)?;
        let now = timestamp() / 1000;
        if cached.expires <= now.saturating_add(REFRESH_BEFORE) && refresh_ready(&cached, now) {
            match refresh(plan, &cached.value, signal) {
                Ok(value) => {
                    commit_cache(&path, plan, &cached, &value, signal)?;
                    cached.bytes = serde_json::to_vec_pretty(&value)
                        .map_err(|_| "AWS SSO cache JSON cannot be written")?;
                    cached.value = value;
                    cached.expires =
                        parse_expiration(cached.value["expiresAt"].as_str().ok_or(LOGIN_REQUIRED)?)
                            .map_err(|_| LOGIN_REQUIRED)?;
                }
                Err(_) if !signal.is_cancelled() && cached.expires > timestamp() / 1000 => {}
                Err(error) => return Err(error),
            }
        }
    }
    check_cancelled(signal)?;
    if cached.expires <= timestamp() / 1000 {
        return Err(LOGIN_REQUIRED.into());
    }
    let mut result = get_role(plan, secret(&cached.value, "accessToken")?, signal)?;
    // Bind credentials to the token actually sent, not a later external login.
    result.source_fingerprint = Some(token_fingerprint(plan, &cached.bytes));
    Ok(result)
}

fn refresh(plan: &SsoPlan, cached: &Value, signal: &CancellationToken) -> Result<Value, String> {
    let body = json!({"clientId":secret(cached,"clientId")?,"clientSecret":secret(cached,"clientSecret")?,
        "refreshToken":secret(cached,"refreshToken")?,"grantType":"refresh_token"});
    let response = request(
        Method::POST,
        endpoint(plan, true)?,
        None,
        Some(&body),
        signal,
    )?;
    let token =
        secret(&response, "accessToken").map_err(|_| "Invalid AWS SSO OIDC token response")?;
    if response
        .get("tokenType")
        .is_some_and(|value| value.as_str() != Some("Bearer"))
    {
        return Err("Invalid AWS SSO OIDC token type".into());
    }
    let seconds = response
        .get("expiresIn")
        .and_then(Value::as_u64)
        .filter(|seconds| *seconds > 0)
        .ok_or("Invalid AWS SSO OIDC token expiration")?;
    let expires = (timestamp() / 1000)
        .checked_add(seconds)
        .ok_or("Invalid AWS SSO OIDC token expiration")?;
    let date = aws_timestamp(expires).map_err(|_| "Invalid AWS SSO OIDC token expiration")?;
    let date = format!(
        "{}-{}-{}T{}:{}:{}Z",
        &date[..4],
        &date[4..6],
        &date[6..8],
        &date[9..11],
        &date[11..13],
        &date[13..15]
    );
    let mut refreshed = cached.clone();
    refreshed["accessToken"] = json!(token);
    refreshed["expiresAt"] = json!(date);
    if response.get("refreshToken").is_some() {
        refreshed["refreshToken"] =
            json!(secret(&response, "refreshToken")
                .map_err(|_| "Invalid AWS SSO OIDC refresh token")?);
    }
    check_cancelled(signal)?;
    Ok(refreshed)
}
fn get_role(
    plan: &SsoPlan,
    token: &str,
    signal: &CancellationToken,
) -> Result<CredentialResult, String> {
    let mut url = endpoint(plan, false)?;
    url.query_pairs_mut()
        .append_pair("account_id", &plan.account_id)
        .append_pair("role_name", &plan.role_name);
    let value = request(Method::GET, url, Some(token), None, signal)?;
    let role = value
        .get("roleCredentials")
        .filter(|value| value.is_object())
        .ok_or("Invalid AWS SSO role credential response")?;
    let required = |name| {
        secret(role, name)
            .map(str::to_owned)
            .map_err(|_| "AWS SSO role credential field missing")
    };
    let credentials = AwsCredentials {
        access_key_id: required("accessKeyId")?,
        secret_access_key: required("secretAccessKey")?,
        session_token: required("sessionToken")?,
    };
    credentials
        .validate()
        .map_err(|_| "Invalid AWS SSO role credentials")?;
    let expiration = role
        .get("expiration")
        .and_then(Value::as_u64)
        .ok_or("Invalid AWS SSO role expiration")?
        / 1000;
    if expiration <= timestamp() / 1000 {
        return Err("AWS SSO role credentials are already expired".into());
    }
    check_cancelled(signal)?;
    Ok(CredentialResult {
        credentials,
        expiration: Some(expiration),
        source_fingerprint: None,
    })
}

fn request(
    method: Method,
    url: Url,
    token: Option<&str>,
    body: Option<&Value>,
    signal: &CancellationToken,
) -> Result<Value, String> {
    check_cancelled(signal)?;
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120));
    if url.scheme() == "http" {
        builder = builder.no_proxy();
    }
    let client = builder
        .build()
        .map_err(|_| "AWS SSO HTTP client initialization failed")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "AWS SSO HTTP runtime initialization failed")?;
    let bytes: Vec<u8> = runtime.block_on(async {
        let operation = async {
            let mut request = client.request(method, url);
            if let Some(token) = token {
                request = request.header("x-amz-sso_bearer_token", token);
            }
            if let Some(body) = body {
                request = request.json(body);
            }
            let mut response = request
                .send()
                .await
                .map_err(|_| "AWS SSO credential request failed")?;
            if !response.status().is_success() {
                return Err(format!(
                    "AWS SSO credential endpoint returned HTTP {}",
                    response.status().as_u16()
                ));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| "AWS SSO credential response failed")?
            {
                if bytes.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
                    return Err("AWS SSO credential response exceeds limit".into());
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(bytes)
        };
        let cancellation = async {
            while !signal.is_cancelled() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        };
        tokio::select! {result=operation=>result,_=cancellation=>Err("Operation aborted".into())}
    })?;
    check_cancelled(signal)?;
    let value = serde_json::from_slice(&bytes).map_err(|_| "Invalid AWS SSO credential JSON")?;
    Ok(value)
}

#[cfg(test)]
#[path = "provider_aws_sso_tests.rs"]
mod tests;
