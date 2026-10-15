//! Bounded command execution with process-tree ownership and cancellable output draining.

use agent_core::types::{CancellationToken, ToolResult};
use serde_json::json;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};

#[cfg(unix)]
unsafe extern "C" {
    fn kill(process: i32, signal: i32) -> i32;
}

const OUTPUT_LIMIT: usize = 102_400;
const POLL_INTERVAL: Duration = Duration::from_millis(20);

struct Output {
    bytes: Vec<u8>,
    truncated: bool,
}
type OutputMessage = (bool, Result<Output, String>);

fn output_text(output: &Output) -> (String, bool) {
    let mut text = String::from_utf8_lossy(&output.bytes).into_owned();
    let clipped = text.len() > OUTPUT_LIMIT;
    if clipped {
        let mut end = OUTPUT_LIMIT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    (text, output.truncated || clipped)
}

fn collect_output(mut reader: impl Read) -> Result<Output, String> {
    let mut output = Output {
        bytes: Vec::new(),
        truncated: false,
    };
    let mut buffer = [0u8; 8192];
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.to_string()),
        };
        if count == 0 {
            return Ok(output);
        }
        let keep = count.min(OUTPUT_LIMIT.saturating_sub(output.bytes.len()));
        output.bytes.extend_from_slice(&buffer[..keep]);
        output.truncated |= keep < count;
        // Continue draining after the cap, so a full pipe never blocks its writer.
    }
}

/// Each command owns its process tree. Descendants are terminated when the root
/// exits, on cancellation/timeout, and on any setup or output-collection error.
pub fn run_command(
    command: &str,
    workspace: &Path,
    timeout: Duration,
    signal: &CancellationToken,
) -> Result<ToolResult, String> {
    if signal.is_cancelled() {
        return Err("Operation aborted".into());
    }
    let start = Instant::now();
    let (mut child, mut tree) = spawn_command(command, workspace)?;
    let stdout = child.stdout.take().ok_or("stdout pipe missing")?;
    let stderr = child.stderr.take().ok_or("stderr pipe missing")?;
    let (sender, receiver) = mpsc::channel();
    let stdout_sender = sender.clone();
    let out = thread::spawn(move || {
        let _ = stdout_sender.send((true, collect_output(stdout)));
    });
    let err = thread::spawn(move || {
        let _ = sender.send((false, collect_output(stderr)));
    });
    let mut status = None;
    let mut stdout = None;
    let mut stderr = None;
    let result = loop {
        // Check the deadline during the drain phase as well as root execution.
        if signal.is_cancelled() {
            break Err("Operation aborted".into());
        }
        if start.elapsed() >= timeout {
            break Err("Command timed out".into());
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    status = Some(exit);
                    // Background descendants cannot retain our stdout/stderr pipes.
                    tree.close();
                }
                Ok(None) => {}
                Err(error) => break Err(error.to_string()),
            }
        }
        if let Some((is_stdout, output)) = receive_output(
            &receiver,
            timeout.saturating_sub(start.elapsed()).min(POLL_INTERVAL),
        ) {
            let output = match output {
                Ok(output) => output,
                Err(error) => break Err(error),
            };
            if is_stdout {
                stdout = Some(output);
            } else {
                stderr = Some(output);
            }
        }
        if let (Some(status), Some(stdout), Some(stderr)) = (&status, &stdout, &stderr) {
            let (stdout_text, stdout_truncated) = output_text(stdout);
            let (stderr_text, stderr_truncated) = output_text(stderr);
            let content = format!(
                "{stdout_text}{stderr_text}\n[exit {}]",
                status.code().unwrap_or(-1)
            );
            if !status.success() {
                break Err(content);
            }
            break Ok(ToolResult {
                content: ToolResult::text(content).content,
                details: json!({
                    "exitCode":status.code(), "stdout":stdout_text, "stderr":stderr_text,
                    "stdoutTruncated":stdout_truncated, "stderrTruncated":stderr_truncated,
                    "stdoutBytes":stdout.bytes.len(), "stderrBytes":stderr.bytes.len()
                }),
                ..ToolResult::default()
            });
        }
    };
    tree.close();
    if status.is_none() {
        let _ = child.kill();
    }
    // A deadline must never turn into an unbounded join on inherited pipe handles.
    // A completed reader is safe to join; unfinished readers own their pipe and
    // terminate after job closure without delaying delivery of the interruption.
    if out.is_finished() {
        let _ = out.join();
    }
    if err.is_finished() {
        let _ = err.join();
    }
    result
}

fn receive_output(receiver: &Receiver<OutputMessage>, wait: Duration) -> Option<OutputMessage> {
    match receiver.recv_timeout(wait) {
        Ok(output) => Some(output),
        Err(RecvTimeoutError::Timeout) => None,
        // Once both readers finish, the caller may still need to observe root exit.
        Err(RecvTimeoutError::Disconnected) => {
            thread::sleep(wait);
            None
        }
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use std::io;
    use std::mem::size_of;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::process::Child;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    pub(crate) struct ProcessTree {
        job: Option<OwnedHandle>,
    }

    impl ProcessTree {
        pub(crate) fn new() -> Result<Self, String> {
            // The job handle is deliberately non-inheritable. Its sole owner is
            // this runner, so closing it terminates every assigned descendant.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error().to_string());
            }
            let job = unsafe { OwnedHandle::from_raw_handle(handle) };
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let configured = unsafe {
                SetInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const std::ffi::c_void,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if configured == 0 {
                return Err(io::Error::last_os_error().to_string());
            }
            Ok(Self { job: Some(job) })
        }

        pub(crate) fn attach_and_resume(&self, child: &Child) -> Result<(), String> {
            let job = self.job.as_ref().ok_or("Command job is closed")?;
            if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } == 0
            {
                return Err(format!(
                    "AssignProcessToJobObject: {}",
                    io::Error::last_os_error()
                ));
            }
            // Stable std::process::Child does not expose its initial thread handle.
            // The process was created suspended, so it has exactly one initial
            // thread and cannot spawn descendants before job membership is set.
            let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
            if snapshot == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error().to_string());
            }
            let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
            let mut entry = THREADENTRY32 {
                dwSize: size_of::<THREADENTRY32>() as u32,
                ..THREADENTRY32::default()
            };
            let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
            while found != 0 {
                if entry.th32OwnerProcessID == child.id() {
                    let handle =
                        unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                    if handle.is_null() {
                        return Err(io::Error::last_os_error().to_string());
                    }
                    let thread = unsafe { OwnedHandle::from_raw_handle(handle) };
                    if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                        return Err(io::Error::last_os_error().to_string());
                    }
                    return Ok(());
                }
                entry.dwSize = size_of::<THREADENTRY32>() as u32;
                found = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
            }
            Err("Suspended command thread was not found".into())
        }

        pub(crate) fn close(&mut self) {
            drop(self.job.take());
        }
    }
}

#[cfg(target_os = "windows")]
pub(crate) use windows::ProcessTree;

#[cfg(target_os = "windows")]
fn spawn_command(command: &str, workspace: &Path) -> Result<(Child, ProcessTree), String> {
    let tree = ProcessTree::new()?;
    let mut process = Command::new("powershell.exe");
    process.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command",
        &format!("[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); $OutputEncoding = [Console]::OutputEncoding; {command}")]);
    process.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    let mut child = process
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    if let Err(error) = tree.attach_and_resume(&child) {
        let _ = child.kill();
        return Err(error);
    }
    Ok((child, tree))
}

#[cfg(unix)]
pub(crate) struct ProcessTree {
    process_group: Option<i32>,
}

#[cfg(unix)]
impl ProcessTree {
    pub(crate) fn close(&mut self) {
        if let Some(group) = self.process_group.take() {
            unsafe {
                kill(-group, 9);
            }
        }
    }
}

/// Direct argv execution for explicit native extensions. The transport owns all
/// three pipes and the same process-tree guard as the built-in command runner.
pub(crate) fn spawn_native(
    executable: &Path,
    args: &[String],
    workspace: &Path,
) -> Result<(Child, ProcessTree), String> {
    #[cfg(target_os = "windows")]
    let tree = ProcessTree::new()?;
    let mut process = Command::new(executable);
    process
        .args(args)
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(target_os = "windows")]
    process.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    #[cfg(unix)]
    process.process_group(0);
    let child = process.spawn().map_err(|error| error.to_string())?;
    #[cfg(target_os = "windows")]
    let mut child = child;
    #[cfg(target_os = "windows")]
    if let Err(error) = tree.attach_and_resume(&child) {
        let _ = child.kill();
        return Err(error);
    }
    #[cfg(unix)]
    let tree = ProcessTree {
        process_group: Some(child.id() as i32),
    };
    Ok((child, tree))
}

#[cfg(unix)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(unix)]
fn spawn_command(command: &str, workspace: &Path) -> Result<(Child, ProcessTree), String> {
    let child = Command::new("sh")
        .args(["-c", command])
        .process_group(0)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let process_group = child.id() as i32;
    Ok((
        child,
        ProcessTree {
            process_group: Some(process_group),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_reader_discards_excess_while_draining_to_eof() {
        let bytes = vec![b'x'; OUTPUT_LIMIT * 3];
        let output = collect_output(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!(output.bytes.len(), OUTPUT_LIMIT);
        assert!(output.truncated);
    }

    #[test]
    fn disconnected_reader_does_not_block_deadline_polling() {
        let (sender, receiver) = mpsc::channel();
        drop(sender);
        let start = Instant::now();
        assert!(receive_output(&receiver, POLL_INTERVAL).is_none());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn lossy_utf8_conversion_does_not_expand_the_output_past_its_byte_cap() {
        let output = Output {
            bytes: vec![0xff; OUTPUT_LIMIT],
            truncated: false,
        };
        let (text, truncated) = output_text(&output);
        assert!(text.len() <= OUTPUT_LIMIT);
        assert!(truncated);
    }
}
