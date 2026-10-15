use super::*;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
#[cfg(windows)]
use windows_sys::Win32::Foundation::CloseHandle;
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
};

fn fixture_executable() -> &'static Path {
    static EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();
    EXECUTABLE.get_or_init(|| {
        let directory = std::env::temp_dir().join(format!(
            "agent-aws-process-compiler-{}",
            crate::session::Session::default().id
        ));
        fs::create_dir_all(&directory).unwrap();
        let output = directory.join(if cfg!(windows) {
            "credential fixture.exe"
        } else {
            "credential fixture"
        });
        let compiler = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let result = Command::new(compiler)
            .args(["--edition=2021", "-O"])
            .arg(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("examples/credential_process_fixture.rs"),
            )
            .arg("-o")
            .arg(&output)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "Fixture compiler: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        output
    })
}

struct Fixture {
    path: PathBuf,
    executable: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "agent-aws-process-{}",
            crate::session::Session::default().id
        ));
        fs::create_dir_all(&path).unwrap();
        let executable = path.join(if cfg!(windows) {
            "credential fixture.exe"
        } else {
            "credential fixture"
        });
        fs::copy(fixture_executable(), &executable).unwrap();
        Self { path, executable }
    }
    fn command(&self, mode: &str, args: &[String]) -> String {
        std::iter::once(self.executable.to_string_lossy().into_owned())
            .chain(std::iter::once(mode.to_owned()))
            .chain(args.iter().cloned())
            .map(|arg| {
                if cfg!(windows) {
                    format!("\"{arg}\"")
                } else {
                    format!("'{}'", arg.replace('\'', "'\\''"))
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
    fn invoke(&self, mode: &str) -> Result<CredentialResult, String> {
        resolve(
            &self.command(mode, &[]),
            &self.path,
            &CancellationToken::new(),
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn error(result: Result<CredentialResult, String>) -> String {
    result.err().expect("Expected credentials failure")
}
fn assert_redacted(error: &str) {
    for secret in [
        "secret-output",
        "session-output",
        "stderr-secret-token",
        "command-secret",
        "credential fixture",
    ] {
        assert!(!error.contains(secret), "{error}");
    }
}
fn wait_pid(path: &Path) -> u32 {
    let start = Instant::now();
    loop {
        if let Ok(text) = fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse() {
                return pid;
            }
        }
        assert!(start.elapsed() < Duration::from_secs(3));
        thread::sleep(Duration::from_millis(5));
    }
}
fn assert_gone(pid: u32) {
    #[cfg(windows)]
    {
        let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if !process.is_null() {
            let status = unsafe { WaitForSingleObject(process, 1500) };
            unsafe { CloseHandle(process) };
            assert_eq!(status, 0, "Credential fixture process {pid} remains live");
        }
    }
    #[cfg(unix)]
    {
        let start = Instant::now();
        while Path::new(&format!("/proc/{pid}")).exists() {
            if fs::read_to_string(format!("/proc/{pid}/stat"))
                .is_ok_and(|stat| stat.split_whitespace().nth(2) == Some("Z"))
            {
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(2));
            thread::sleep(Duration::from_millis(5));
        }
    }
}

#[test]
fn real_process_returns_optional_token_expiration_and_ignores_unknown_fields() {
    let fixture = Fixture::new();
    let result = fixture.invoke("success").unwrap();
    assert_eq!(result.credentials.access_key_id, "AKIAFIXTURE");
    assert_eq!(result.credentials.secret_access_key, "secret-output");
    assert_eq!(result.credentials.session_token, "session-output");
    assert_eq!(result.expiration, Some(4_070_908_800));
    let result = fixture.invoke("static").unwrap();
    assert!(result.credentials.session_token.is_empty());
    assert_eq!(result.expiration, None);
    assert!(fixture.invoke("version-float").is_ok());
    assert!(fixture.invoke("stderr-secret").is_ok());
}

#[test]
fn real_process_rejects_json_version_types_and_invalid_or_expired_credentials() {
    let fixture = Fixture::new();
    for mode in [
        "invalid-json",
        "invalid-utf8",
        "version",
        "version-string",
        "field-type",
        "token-null",
        "expiration-null",
        "expiration-invalid",
        "expired",
        "header",
        "empty",
    ] {
        let error = error(fixture.invoke(mode));
        assert_redacted(&error);
        assert!(
            error.starts_with("AWS credential_process"),
            "{mode}: {error}"
        );
    }
}

#[test]
fn nonzero_exit_and_start_errors_do_not_echo_command_stderr_or_credentials() {
    let fixture = Fixture::new();
    let failure = error(fixture.invoke("error"));
    assert_eq!(failure, "AWS credential_process exited unsuccessfully");
    assert_redacted(&failure);
    let failure = error(resolve(
        "command-secret-missing-executable",
        &fixture.path,
        &CancellationToken::new(),
    ));
    assert_redacted(&failure);
    let failure = error(resolve(
        &fixture.command("success", &[]),
        &fixture.path.join("missing-secret-directory"),
        &CancellationToken::new(),
    ));
    assert_eq!(failure, "AWS credential_process could not start");
    assert_redacted(&failure);
}

#[test]
fn shell_quoting_working_directory_and_chain_match_exec_not_powershell_or_direct_argv() {
    let fixture = Fixture::new();
    let record = fixture.path.join("argument record.txt");
    let command = fixture.command(
        "success",
        &[
            format!("--record={}", record.display()),
            "literal space & dollar$ semicolon; 中文".into(),
        ],
    );
    let prefix = if cfg!(windows) { "ver >nul" } else { ":" };
    let result = resolve(
        &format!("{prefix} && {command}"),
        &fixture.path,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(result.credentials.access_key_id, "AKIAFIXTURE");
    let record = fs::read_to_string(record).unwrap();
    assert!(record
        .lines()
        .next()
        .unwrap()
        .eq_ignore_ascii_case(&fixture.path.to_string_lossy()));
    assert!(record.contains("literal space & dollar$ semicolon; 中文"));
}

#[test]
fn both_output_limits_terminate_real_writers_without_secret_diagnostics() {
    let fixture = Fixture::new();
    for mode in ["stdout-flood", "stderr-flood"] {
        let start = Instant::now();
        let failure = error(resolve_with_timeout(
            &fixture.command(mode, &[]),
            &fixture.path,
            &CancellationToken::new(),
            Duration::from_secs(3),
        ));
        assert_eq!(failure, "AWS credential_process output exceeds limit");
        assert_redacted(&failure);
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}

#[test]
fn precancel_and_invalid_command_do_not_spawn_a_credentials_program() {
    let fixture = Fixture::new();
    let pid_file = fixture.path.join("pid");
    let signal = CancellationToken::new();
    signal.cancel();
    let failure = error(resolve(
        &fixture.command("success", &[format!("--pid={}", pid_file.display())]),
        &fixture.path,
        &signal,
    ));
    assert_eq!(failure, "Operation aborted");
    assert!(!pid_file.exists());
    for command in ["", " ", "command-secret\0invalid"] {
        assert_eq!(
            error(resolve(command, &fixture.path, &CancellationToken::new())),
            "Invalid AWS credential_process command"
        );
    }
}

#[test]
fn timeout_terminates_root_and_background_tree_with_bounded_pipe_draining() {
    let fixture = Fixture::new();
    let pid_file = fixture.path.join("pid");
    let child_file = fixture.path.join("child.pid");
    let start = Instant::now();
    let failure = error(resolve_with_timeout(
        &fixture.command(
            "background-hang",
            &[
                format!("--pid={}", pid_file.display()),
                format!("--child-pid={}", child_file.display()),
            ],
        ),
        &fixture.path,
        &CancellationToken::new(),
        // The deadline includes shell/job setup. Allow fixture startup under
        // parallel Windows tests before asserting termination of both real PIDs.
        Duration::from_secs(1),
    ));
    assert_eq!(failure, "AWS credential_process timed out");
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_gone(wait_pid(&pid_file));
    assert_gone(wait_pid(&child_file));
}

#[test]
fn cancellation_terminates_root_and_background_tree_before_two_seconds() {
    let fixture = Fixture::new();
    let pid_file = fixture.path.join("pid");
    let child_file = fixture.path.join("child.pid");
    let signal = CancellationToken::new();
    let cancel = signal.clone();
    let child_ready = child_file.clone();
    let cancel_worker = thread::spawn(move || {
        wait_pid(&child_ready);
        cancel.cancel();
    });
    let start = Instant::now();
    let failure = error(resolve(
        &fixture.command(
            "background-hang",
            &[
                format!("--pid={}", pid_file.display()),
                format!("--child-pid={}", child_file.display()),
            ],
        ),
        &fixture.path,
        &signal,
    ));
    cancel_worker.join().unwrap();
    assert_eq!(failure, "Operation aborted");
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_gone(wait_pid(&pid_file));
    assert_gone(wait_pid(&child_file));
}

#[test]
fn successful_parent_exit_reaps_background_pipe_owners_without_waiting_for_timeout() {
    let fixture = Fixture::new();
    let child_file = fixture.path.join("child.pid");
    let start = Instant::now();
    let result = resolve_with_timeout(
        &fixture.command(
            "background-exit",
            &[format!("--child-pid={}", child_file.display())],
        ),
        &fixture.path,
        &CancellationToken::new(),
        Duration::from_secs(3),
    )
    .unwrap();
    assert_eq!(result.credentials.access_key_id, "AKIAFIXTURE");
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_gone(wait_pid(&child_file));
}

#[test]
fn bounded_readers_accept_exact_cap_discard_stderr_and_reject_cap_plus_one() {
    let output = collect_output(std::io::Cursor::new(vec![b'x'; OUTPUT_LIMIT]), true).unwrap();
    assert_eq!(output.0.len(), OUTPUT_LIMIT);
    let output = collect_output(std::io::Cursor::new(vec![b'x'; OUTPUT_LIMIT]), false).unwrap();
    assert!(output.0.is_empty());
    for retain in [true, false] {
        assert_eq!(
            collect_output(std::io::Cursor::new(vec![b'x'; OUTPUT_LIMIT + 1]), retain)
                .err()
                .unwrap(),
            "AWS credential_process output exceeds limit"
        );
    }
}
