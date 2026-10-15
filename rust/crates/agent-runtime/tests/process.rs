use agent_core::types::CancellationToken;
use agent_runtime::process::run_command;
use std::fs;
#[cfg(target_os = "windows")]
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};
#[cfg(target_os = "windows")]
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};

#[cfg(unix)]
unsafe extern "C" {
    fn kill(process: i32, signal: i32) -> i32;
}

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "agent-rust-process-{}-{}-{}",
            std::process::id(),
            agent_core::types::timestamp(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(entries) = fs::read_dir(&self.0) {
            for entry in entries.flatten() {
                let _ = fs::remove_file(entry.path());
            }
        }
        let _ = fs::remove_dir(&self.0);
    }
}

#[test]
fn utf8_stdout_stderr_and_nonzero_exit_are_preserved() {
    let directory = Fixture::new();
    #[cfg(target_os = "windows")]
    let command = "[Console]::WriteLine('中文输出'); [Console]::Error.WriteLine('中文错误')";
    #[cfg(unix)]
    let command = "printf '中文输出'; printf '中文错误' >&2";
    let result = run_command(
        command,
        &directory.0,
        Duration::from_secs(10),
        &CancellationToken::new(),
    )
    .unwrap();
    assert!(result.details["stdout"]
        .as_str()
        .unwrap()
        .contains("中文输出"));
    assert!(result.details["stderr"]
        .as_str()
        .unwrap()
        .contains("中文错误"));
    assert_eq!(result.details["exitCode"], 0);
    let error = run_command(
        "exit 7",
        &directory.0,
        Duration::from_secs(10),
        &CancellationToken::new(),
    )
    .unwrap_err();
    assert!(error.contains("[exit 7]"));
}

#[test]
fn both_streams_are_capped_independently_without_blocking_the_writer() {
    let directory = Fixture::new();
    #[cfg(target_os = "windows")]
    let command = "[Console]::Write(('o' * 300000)); [Console]::Error.Write(('e' * 300000))";
    #[cfg(unix)]
    let command =
        "head -c 300000 /dev/zero | tr '\\000' o; head -c 300000 /dev/zero | tr '\\000' e >&2";
    let result = run_command(
        command,
        &directory.0,
        Duration::from_secs(10),
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(result.details["stdout"].as_str().unwrap().len(), 102_400);
    assert_eq!(result.details["stderr"].as_str().unwrap().len(), 102_400);
    assert_eq!(result.details["stdoutBytes"], 102_400);
    assert_eq!(result.details["stderrBytes"], 102_400);
    assert_eq!(result.details["stdoutTruncated"], true);
    assert_eq!(result.details["stderrTruncated"], true);
}

#[test]
fn pre_cancelled_commands_do_not_spawn() {
    let directory = Fixture::new();
    let signal = CancellationToken::new();
    signal.cancel();
    assert_eq!(
        run_command("exit 0", &directory.0, Duration::from_secs(1), &signal).unwrap_err(),
        "Operation aborted"
    );
}

#[cfg(target_os = "windows")]
fn spawn_pipe_holder(marker: Option<&Path>, keep_parent: bool) -> String {
    let marker = marker
        .map(|path| {
            format!(
                "[IO.File]::WriteAllText('{}',[string]$p.Id);",
                path.to_string_lossy().replace('\'', "''")
            )
        })
        .unwrap_or_default();
    format!(
        "$p = [Diagnostics.Process]::new(); $p.StartInfo.FileName = 'powershell.exe'; \
        $p.StartInfo.Arguments = '-NoLogo -NoProfile -NonInteractive -Command \"[Threading.Thread]::Sleep(15000)\"'; \
        $p.StartInfo.UseShellExecute = $false; $p.StartInfo.CreateNoWindow = $true; \
        [void]$p.Start(); [Console]::WriteLine(('child-pid:' + $p.Id)); {marker} {}",
        if keep_parent { "[Threading.Thread]::Sleep(15000)" } else { "exit 0" }
    )
}

#[cfg(unix)]
fn spawn_pipe_holder(marker: Option<&Path>, keep_parent: bool) -> String {
    let marker = marker
        .map(|path| {
            format!(
                "printf '%s' \"$child\" > '{}';",
                path.to_string_lossy().replace('\'', "'\\''")
            )
        })
        .unwrap_or_default();
    format!(
        "sleep 15 & child=$!; printf 'child-pid:%s\\n' \"$child\"; {marker} {}",
        if keep_parent { "sleep 15" } else { "exit 0" }
    )
}

#[cfg(target_os = "windows")]
fn assert_child_terminated(id: u32) {
    let handle = unsafe { OpenProcess(0x0010_0000, 0, id) }; // SYNCHRONIZE
    if handle.is_null() {
        return;
    } // A reaped process has no process handle to open.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    assert_eq!(
        unsafe { WaitForSingleObject(handle.as_raw_handle(), 2_000) },
        WAIT_OBJECT_0
    );
}

#[cfg(unix)]
fn assert_child_terminated(id: u32) {
    let start = Instant::now();
    loop {
        if unsafe { kill(id as i32, 0) } != 0 {
            return;
        }
        if let Ok(status) = fs::read_to_string(format!("/proc/{id}/stat")) {
            if status
                .rsplit_once(") ")
                .is_some_and(|(_, fields)| fields.starts_with('Z'))
            {
                return;
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "Command descendant is still running"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn parent_exit_with_a_child_holding_inherited_pipes_does_not_hang() {
    let directory = Fixture::new();
    let start = Instant::now();
    let result = run_command(
        &spawn_pipe_holder(None, false),
        &directory.0,
        Duration::from_secs(8),
        &CancellationToken::new(),
    )
    .unwrap();
    assert!(start.elapsed() < Duration::from_secs(8));
    let id = result.details["stdout"]
        .as_str()
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("child-pid:"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_child_terminated(id);
}

#[test]
fn timeout_terminates_the_process_tree_and_returns_within_its_deadline() {
    let directory = Fixture::new();
    let marker = directory.0.join("timeout-child.pid");
    let start = Instant::now();
    let error = run_command(
        &spawn_pipe_holder(Some(&marker), true),
        &directory.0,
        Duration::from_secs(4),
        &CancellationToken::new(),
    )
    .unwrap_err();
    assert_eq!(error, "Command timed out");
    assert!(start.elapsed() < Duration::from_secs(6));
    let id = fs::read_to_string(marker).unwrap().parse().unwrap();
    assert_child_terminated(id);
}

#[test]
fn cancellation_terminates_parent_and_pipe_holding_descendant() {
    let directory = Fixture::new();
    let marker = directory.0.join("cancel-child.pid");
    let signal = CancellationToken::new();
    let cancel = signal.clone();
    let ready = marker.clone();
    let notifier = thread::spawn(move || {
        let start = Instant::now();
        while !ready.exists() && start.elapsed() < Duration::from_secs(6) {
            thread::sleep(Duration::from_millis(20));
        }
        cancel.cancel();
    });
    let start = Instant::now();
    let error = run_command(
        &spawn_pipe_holder(Some(&marker), true),
        &directory.0,
        Duration::from_secs(10),
        &signal,
    )
    .unwrap_err();
    notifier.join().unwrap();
    assert_eq!(error, "Operation aborted");
    assert!(start.elapsed() < Duration::from_secs(8));
    let id = fs::read_to_string(marker).unwrap().parse().unwrap();
    assert_child_terminated(id);
}
