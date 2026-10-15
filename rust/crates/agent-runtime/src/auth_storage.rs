//! Native credential storage: current-user DPAPI on Windows; mode 0600 on Unix.
//! Legacy auth.json imports are strictly read-only and retain unknown entries.
use crate::auth::{
    check_cancelled, random_hex, CodexOAuthClient, NativeOAuthClient, OAuthCredential,
    CODEX_PROVIDER,
};
use agent_core::types::CancellationToken;
use serde::Serialize;
use serde_json::{Map, Value};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const LIMIT: u64 = 4_194_304;
pub fn default_auth_directory() -> PathBuf {
    std::env::var_os("AGENT_CORE_AUTH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".agent-core-rust")
                .join("auth")
        })
}
#[cfg(windows)]
const HEADER: &[u8] = b"AGENT_CORE_DPAPI_V1\n";
#[cfg(not(windows))]
const HEADER: &[u8] = b"AGENT_CORE_PRIVATE_V1\n";

#[derive(Clone)]
pub struct AuthStorage {
    path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialInfo {
    pub provider_id: String,
    pub credential_type: String,
    pub expires: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ImportReport {
    pub imported: Vec<String>,
    pub skipped: Vec<String>,
}

struct FileLock(PathBuf);
impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn io_error(error: std::io::Error) -> String {
    format!("Credential storage I/O failed: {}", error.kind())
}

impl AuthStorage {
    /// A stored entry owns the provider. A failed refresh or unknown type never
    /// falls back to environment authentication. Refresh and save share the lock.
    pub fn resolve_provider(
        &self,
        provider: &str,
        client: Option<&NativeOAuthClient>,
        signal: &CancellationToken,
    ) -> Result<Option<(String, Option<OAuthCredential>)>, String> {
        let _lock = self.lock(signal)?;
        let mut data = self.load()?;
        let Some(value) = data.get(provider).cloned() else {
            return Ok(None);
        };
        match value["type"].as_str() {
            Some("api_key") => {
                let key = value["key"]
                    .as_str()
                    .filter(|key| {
                        !key.is_empty() && !key.bytes().any(|byte| byte.is_ascii_control())
                    })
                    .ok_or("Invalid stored API key")?;
                check_cancelled(signal)?;
                Ok(Some((key.into(), None)))
            }
            Some("oauth") => {
                let client =
                    client.ok_or("Stored OAuth credential has no native provider handler")?;
                if client.config.provider != provider {
                    return Err("OAuth configuration provider identity mismatch".into());
                }
                let mut credential: OAuthCredential = serde_json::from_value(value)
                    .map_err(|_| "Invalid stored provider OAuth credential")?;
                credential.validate_for_provider(provider)?;
                if credential.expires <= agent_core::types::timestamp().saturating_add(300000) {
                    credential = client.refresh(&credential, signal)?;
                    credential.validate_for_provider(provider)?;
                    data.insert(
                        provider.into(),
                        serde_json::to_value(&credential)
                            .map_err(|_| "Credential serialization failed")?,
                    );
                    // The server has rotated a validated refresh credential. Commit
                    // it before surfacing a late cancellation; pending/invalid token
                    // requests never reach this non-cancellable commit boundary.
                    self.save(data, &CancellationToken::new())?;
                }
                check_cancelled(signal)?;
                Ok(Some((credential.access.clone(), Some(credential))))
            }
            _ => Err("Stored credential has an unsupported type".into()),
        }
    }
    pub fn ensure_oauth(
        &self,
        provider: &str,
        client: &NativeOAuthClient,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential, String> {
        self.resolve_provider(provider, Some(client), signal)?
            .and_then(|(_, credential)| credential)
            .ok_or("Provider OAuth login is required".into())
    }
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock(&self, signal: &CancellationToken) -> Result<FileLock, String> {
        check_cancelled(signal)?;
        if let Some(parent) = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(io_error)?;
        }
        let mut lock_path = self.path.as_os_str().to_os_string();
        lock_path.push(".lock");
        let lock_path = PathBuf::from(lock_path);
        let start = Instant::now();
        loop {
            check_cancelled(signal)?;
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(_) => return Ok(FileLock(lock_path)),
                Err(error)
                    if error.kind() == std::io::ErrorKind::AlreadyExists
                        && start.elapsed() < Duration::from_secs(30) =>
                {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Err(error) => return Err(io_error(error)),
            }
        }
    }

    fn load(&self) -> Result<Map<String, Value>, String> {
        let file = match fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
            Err(error) => return Err(io_error(error)),
        };
        if file.metadata().map_err(io_error)?.len() > LIMIT {
            return Err("Credential file exceeds limit".into());
        }
        let mut bytes = Vec::new();
        file.take(LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        let encrypted = bytes
            .strip_prefix(HEADER)
            .ok_or("Unrecognized native credential file format")?;
        let mut plaintext = unprotect(encrypted)?;
        let result = serde_json::from_slice::<Value>(&plaintext)
            .map_err(|_| "Invalid native credential JSON".to_owned())
            .and_then(|value| {
                value
                    .as_object()
                    .cloned()
                    .ok_or_else(|| "Credential store must be an object".into())
            });
        plaintext.fill(0);
        result
    }

    fn save(&self, data: Map<String, Value>, signal: &CancellationToken) -> Result<(), String> {
        check_cancelled(signal)?;
        let mut plaintext =
            serde_json::to_vec(&data).map_err(|_| "Credential serialization failed")?;
        if plaintext.len() as u64 > LIMIT - 65_536 {
            return Err("Credential store exceeds limit".into());
        }
        let protection = protect(&plaintext);
        plaintext.fill(0);
        let encrypted = protection?;
        let mut temp_name = self.path.as_os_str().to_os_string();
        temp_name.push(format!(".{}.tmp", random_hex()?));
        let temp = PathBuf::from(temp_name);
        let result = (|| {
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp).map_err(io_error)?;
            file.write_all(HEADER).map_err(io_error)?;
            file.write_all(&encrypted).map_err(io_error)?;
            file.sync_all().map_err(io_error)?;
            drop(file);
            check_cancelled(signal)?;
            fs::rename(&temp, &self.path).map_err(io_error)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    pub fn read(
        &self,
        provider: &str,
        signal: &CancellationToken,
    ) -> Result<Option<Value>, String> {
        let _lock = self.lock(signal)?;
        let data = self.load()?;
        check_cancelled(signal)?;
        Ok(data.get(provider).cloned())
    }
    pub fn list(&self, signal: &CancellationToken) -> Result<Vec<CredentialInfo>, String> {
        let _lock = self.lock(signal)?;
        let data = self.load()?;
        check_cancelled(signal)?;
        Ok(data
            .into_iter()
            .map(|(provider_id, value)| CredentialInfo {
                provider_id,
                credential_type: value["type"].as_str().unwrap_or("unknown").to_owned(),
                expires: value["expires"].as_u64(),
            })
            .collect())
    }
    pub fn write(
        &self,
        provider: &str,
        credential: Value,
        signal: &CancellationToken,
    ) -> Result<(), String> {
        if provider.is_empty() || !credential.is_object() {
            return Err("Credential provider and object are required".into());
        }
        let _lock = self.lock(signal)?;
        let mut data = self.load()?;
        data.insert(provider.into(), credential);
        self.save(data, signal)
    }
    pub fn delete(&self, provider: &str, signal: &CancellationToken) -> Result<(), String> {
        let _lock = self.lock(signal)?;
        let mut data = self.load()?;
        if data.remove(provider).is_some() {
            self.save(data, signal)?;
        }
        Ok(())
    }

    pub fn import_legacy(
        &self,
        legacy_path: &Path,
        signal: &CancellationToken,
    ) -> Result<ImportReport, String> {
        check_cancelled(signal)?;
        if legacy_path == self.path {
            return Err("Legacy import source and native store must differ".into());
        }
        let file = fs::File::open(legacy_path).map_err(io_error)?;
        if file.metadata().map_err(io_error)?.len() > LIMIT {
            return Err("Legacy auth.json exceeds limit".into());
        }
        let mut source = String::new();
        file.take(LIMIT + 1)
            .read_to_string(&mut source)
            .map_err(io_error)?;
        let legacy: Value = serde_json::from_str(source.trim_start_matches('\u{feff}'))
            .map_err(|_| "Invalid legacy auth.json")?;
        let entries = legacy
            .as_object()
            .ok_or("Legacy auth.json must be an object")?;
        if entries
            .iter()
            .any(|(provider, credential)| provider.is_empty() || !credential.is_object())
        {
            return Err("Legacy credential entries must be objects".into());
        }
        let _lock = self.lock(signal)?;
        let mut data = self.load()?;
        let mut report = ImportReport::default();
        for (provider, value) in entries {
            if data.contains_key(provider) {
                report.skipped.push(provider.clone());
            } else {
                data.insert(provider.clone(), value.clone());
                report.imported.push(provider.clone());
            }
        }
        if !report.imported.is_empty() {
            self.save(data, signal)?;
        }
        check_cancelled(signal)?;
        Ok(report)
    }

    /// Refresh is serialized with reads, login, logout and other refreshes across
    /// processes. A rotated token is committed before returning request auth.
    pub fn ensure_codex(
        &self,
        client: &CodexOAuthClient,
        signal: &CancellationToken,
    ) -> Result<OAuthCredential, String> {
        let _lock = self.lock(signal)?;
        let mut data = self.load()?;
        let value = data
            .get(CODEX_PROVIDER)
            .ok_or("OpenAI Codex OAuth login is required")?;
        let mut credential: OAuthCredential =
            serde_json::from_value(value.clone()).map_err(|_| "Invalid stored Codex credential")?;
        credential.validate()?;
        if credential.is_expired() {
            credential = client.refresh(&credential, signal)?;
            data.insert(
                CODEX_PROVIDER.into(),
                serde_json::to_value(&credential).map_err(|_| "Credential serialization failed")?,
            );
            self.save(data, &CancellationToken::new())?;
        }
        check_cancelled(signal)?;
        Ok(credential)
    }
}

#[cfg(windows)]
fn dpapi(data: &[u8], encrypt: bool) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: data
            .len()
            .try_into()
            .map_err(|_| "DPAPI input exceeds limit")?,
        pbData: data.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // DPAPI owns output.pbData; copy it and release with LocalFree on every
    // successful call. The input slice stays alive and is read-only throughout.
    let success = unsafe {
        if encrypt {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    if success == 0 {
        return Err("Windows DPAPI credential protection failed".into());
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        LocalFree(output.pbData.cast());
    }
    Ok(bytes)
}
#[cfg(windows)]
fn protect(data: &[u8]) -> Result<Vec<u8>, String> {
    dpapi(data, true)
}
#[cfg(windows)]
fn unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    dpapi(data, false)
}
#[cfg(not(windows))]
fn protect(data: &[u8]) -> Result<Vec<u8>, String> {
    Ok(data.to_vec())
}
#[cfg(not(windows))]
fn unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    Ok(data.to_vec())
}
