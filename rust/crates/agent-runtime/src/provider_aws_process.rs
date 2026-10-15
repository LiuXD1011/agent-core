//! AWS credential_process with the installed JavaScript SDK's shell semantics.
//! The caller owns cache/refresh policy; this module runs one bounded invocation.

use super::{parse_expiration, AwsCredentials, CredentialResult};
use crate::process::ProcessTree;
use agent_core::types::{timestamp, CancellationToken};
use serde_json::Value;
use std::io::Read;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Child;
#[cfg(windows)]
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};

const TIMEOUT: Duration = Duration::from_secs(30);
// node:child_process.exec's default maxBuffer applies separately to both streams.
const OUTPUT_LIMIT: usize = 1_048_576;
const POLL_INTERVAL: Duration = Duration::from_millis(20);

struct Output(Vec<u8>);
impl Drop for Output {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

fn collect_output(mut reader: impl Read, retain: bool) -> Result<Output, &'static str> {
    let mut output = Output(Vec::new());
    let mut total = 0usize;
    let mut buffer = [0u8; 8192];
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                buffer.fill(0);
                return Err("AWS credential_process output read failed");
            }
        };
        if count == 0 {
            return Ok(output);
        }
        total = total.saturating_add(count);
        if total > OUTPUT_LIMIT {
            buffer.fill(0);
            return Err("AWS credential_process output exceeds limit");
        }
        if retain {
            output.0.extend_from_slice(&buffer[..count]);
        }
        // Stderr is counted but never retained, logged or returned to the caller.
        buffer.fill(0);
    }
}

pub(super) fn resolve(
    command: &str,
    workspace: &Path,
    signal: &CancellationToken,
) -> Result<CredentialResult, String> {
    resolve_with_timeout(command, workspace, signal, TIMEOUT)
}

fn resolve_with_timeout(
    command: &str,
    workspace: &Path,
    signal: &CancellationToken,
    timeout: Duration,
) -> Result<CredentialResult, String> {
    if signal.is_cancelled() {
        return Err("Operation aborted".into());
    }
    if command.trim().is_empty() || command.contains('\0') {
        return Err("Invalid AWS credential_process command".into());
    }
    let start = Instant::now();
    let (mut child, mut tree) = spawn(command, workspace)?;
    // No input is supplied to a credentials program. Unix spawn_native uses a pipe.
    drop(child.stdin.take());
    let pipes = child.stdout.take().zip(child.stderr.take());
    let Some((stdout, stderr)) = pipes else {
        tree.close();
        let _ = child.kill();
        return Err("AWS credential_process pipes unavailable".into());
    };
    let (sender, receiver) = mpsc::channel();
    let stdout_sender = sender.clone();
    let out = thread::spawn(move || {
        let _ = stdout_sender.send((true, collect_output(stdout, true)));
    });
    let err = thread::spawn(move || {
        let _ = sender.send((false, collect_output(stderr, false)));
    });
    let mut status = None;
    let mut stdout = None;
    let mut stderr_done = false;
    let result = loop {
        if signal.is_cancelled() {
            break Err("Operation aborted".into());
        }
        if start.elapsed() >= timeout {
            break Err("AWS credential_process timed out".into());
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    status = Some(exit);
                    // Descendants must not keep inherited pipes alive after shell exit.
                    tree.close();
                    if !exit.success() {
                        break Err("AWS credential_process exited unsuccessfully".into());
                    }
                }
                Ok(None) => {}
                Err(_) => break Err("AWS credential_process status unavailable".into()),
            }
        }
        let wait = timeout.saturating_sub(start.elapsed()).min(POLL_INTERVAL);
        match receiver.recv_timeout(wait) {
            Ok((is_stdout, output)) => {
                let output = match output {
                    Ok(output) => output,
                    Err(error) => break Err(error.into()),
                };
                if is_stdout {
                    stdout = Some(output);
                } else {
                    stderr_done = true;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => thread::sleep(wait),
        }
        if status.is_some() && stderr_done {
            if let Some(output) = &stdout {
                break parse_output(&output.0);
            }
        }
    };
    tree.close();
    if status.is_none() {
        let _ = child.kill();
        // Reap without turning cancellation into an unbounded wait.
        let cleanup = Instant::now();
        while cleanup.elapsed() < Duration::from_millis(200) {
            if child.try_wait().is_ok_and(|status| status.is_some()) {
                break;
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
    // Job/group closure releases inherited pipes. Never wait without a deadline
    // for an unfinished reader; each owns at most one capped output buffer.
    if out.is_finished() {
        let _ = out.join();
    }
    if err.is_finished() {
        let _ = err.join();
    }
    if signal.is_cancelled() {
        return Err("Operation aborted".into());
    }
    result
}

fn parse_output(bytes: &[u8]) -> Result<CredentialResult, String> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| "AWS credential_process returned invalid JSON")?;
    let data: Value = serde_json::from_str(text.trim())
        .map_err(|_| "AWS credential_process returned invalid JSON")?;
    let data = data
        .as_object()
        .ok_or("AWS credential_process returned invalid credentials")?;
    if data.get("Version").and_then(Value::as_f64) != Some(1.0) {
        return Err("AWS credential_process did not return Version 1".into());
    }
    let required = |key: &str| {
        data.get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or("AWS credential_process returned invalid credentials")
    };
    let optional = |key: &str| {
        data.get(key)
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or("AWS credential_process returned invalid credentials")
            })
            .transpose()
    };
    let credentials = AwsCredentials {
        access_key_id: required("AccessKeyId")?,
        secret_access_key: required("SecretAccessKey")?,
        session_token: optional("SessionToken")?.unwrap_or_default(),
    };
    credentials
        .validate()
        .map_err(|_| "AWS credential_process returned invalid credentials")?;
    let expiration = optional("Expiration")?
        .map(|source| {
            parse_expiration(&source)
                .map_err(|_| "AWS credential_process returned invalid expiration")
        })
        .transpose()?;
    if expiration.is_some_and(|expires| expires <= timestamp() / 1000) {
        return Err("AWS credential_process returned expired credentials".into());
    }
    Ok(CredentialResult {
        credentials,
        expiration,
        source_fingerprint: None,
    })
}

#[cfg(windows)]
fn spawn(command: &str, workspace: &Path) -> Result<(Child, ProcessTree), String> {
    // The installed @aws-sdk/credential-provider-process invokes exec, not execFile.
    // Node uses ComSpec, /d /s /c and verbatim quoted command for cmd.exe; a
    // non-cmd ComSpec uses -c with its ordinary argv quoting instead.
    let shell = std::env::var_os("ComSpec")
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "cmd.exe".into());
    let shell_text = shell.to_string_lossy();
    let name = shell_text.rsplit('\\').next().unwrap_or_default();
    let is_cmd = name.eq_ignore_ascii_case("cmd.exe") || name.eq_ignore_ascii_case("cmd");
    let mut process = Command::new(shell);
    if is_cmd {
        process.raw_arg(format!("/d /s /c \"{command}\""));
    } else {
        process.args(["-c", command]);
    }
    let tree = ProcessTree::new().map_err(|_| "AWS credential_process job setup failed")?;
    process
        .creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = process
        .spawn()
        .map_err(|_| "AWS credential_process could not start")?;
    if tree.attach_and_resume(&child).is_err() {
        let _ = child.kill();
        return Err("AWS credential_process job setup failed".into());
    }
    Ok((child, tree))
}

#[cfg(unix)]
fn spawn(command: &str, workspace: &Path) -> Result<(Child, ProcessTree), String> {
    let shell = if cfg!(target_os = "android") {
        "/system/bin/sh"
    } else {
        "/bin/sh"
    };
    crate::process::spawn_native(Path::new(shell), &["-c".into(), command.into()], workspace)
        .map_err(|_| "AWS credential_process could not start".into())
}

#[cfg(test)]
#[path = "provider_aws_process_tests.rs"]
mod tests;
