//! Explicit native executable transport. JSONL is isolated from the host's stdio.
//! A failed request closes its process tree; a late reply cannot satisfy a later call.

use crate::process::{spawn_native, ProcessTree};
use agent_core::types::CancellationToken;
use agent_extensions::{UiDialog, UiHandle, UiReply};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex, TryLockError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const EXTENSION_PROTOCOL: &str = "agent-core-extension";
pub const EXTENSION_PROTOCOL_VERSION: u64 = 1;
const MAX_FRAME: usize = 8 * 1024 * 1024;
const STDERR_CAP: usize = 102_400;
const POLL: Duration = Duration::from_millis(10);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeExtensionManifest {
    pub schema: String,
    pub id: String,
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub request_timeout_ms: u64,
    pub max_frame_bytes: usize,
}
impl Default for NativeExtensionManifest {
    fn default() -> Self {
        Self {
            schema: "agent-core-native-extension/1".into(),
            id: String::new(),
            executable: PathBuf::new(),
            args: vec![],
            request_timeout_ms: 30_000,
            max_frame_bytes: 1_048_576,
        }
    }
}
impl NativeExtensionManifest {
    pub fn load(path: &Path) -> Result<Self, String> {
        let file = fs::File::open(path)
            .map_err(|error| format!("Extension manifest {}: {error}", path.display()))?;
        if file.metadata().map_err(|error| error.to_string())?.len() > 65_536 {
            return Err("Extension manifest exceeds 64KB".into());
        }
        let mut bytes = Vec::new();
        file.take(65_537)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() > 65_536 {
            return Err("Extension manifest exceeds 64KB".into());
        }
        let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
        let mut manifest: Self = serde_json::from_slice(bytes)
            .map_err(|error| format!("Extension manifest decode: {error}"))?;
        if manifest.executable.is_relative() {
            manifest.executable = path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(&manifest.executable);
        }
        manifest.executable = fs::canonicalize(&manifest.executable)
            .map_err(|error| format!("Extension executable: {error}"))?;
        manifest.validate()?;
        Ok(manifest)
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != "agent-core-native-extension/1" {
            return Err("Unsupported native extension manifest schema".into());
        }
        if self.id.is_empty()
            || self.id.len() > 128
            || !self
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err("Invalid native extension id".into());
        }
        if !self.executable.is_file() || !self.executable.is_absolute() {
            return Err("Native extension executable must resolve to an absolute file".into());
        }
        if self.request_timeout_ms == 0
            || self.request_timeout_ms > 300_000
            || !(256..=MAX_FRAME).contains(&self.max_frame_bytes)
        {
            return Err("Invalid native extension timeout/frame limit".into());
        }
        if self.args.len() > 128
            || self
                .args
                .iter()
                .any(|arg| arg.contains('\0') || arg.len() > 65_536)
        {
            return Err("Invalid native extension argv".into());
        }
        Ok(())
    }
}

struct WriteCommand {
    bytes: Vec<u8>,
    ack: SyncSender<Result<(), String>>,
}
enum OutputFrame {
    Frame(Value),
    Failure(String),
}

/// A dialog never owns the transport lock. Its cancellation guard also runs on
/// EOF, malformed frames and callback unwinding, so no abandoned modal survives.
struct PendingUi {
    request_id: String,
    signal: CancellationToken,
    result: Receiver<Result<UiReply, String>>,
    worker: Option<JoinHandle<()>>,
}
impl PendingUi {
    fn start(request_id: String, dialog: UiDialog, ui: UiHandle) -> Self {
        let signal = CancellationToken::default();
        let worker_signal = signal.clone();
        let (sender, result) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let response = catch_unwind(AssertUnwindSafe(|| ui.request(dialog, &worker_signal)))
                .unwrap_or_else(|_| Err("Extension UI request panicked".into()));
            let _ = sender.send(response);
        });
        Self {
            request_id,
            signal,
            result,
            worker: Some(worker),
        }
    }
}
impl Drop for PendingUi {
    fn drop(&mut self) {
        self.signal.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn send_frame(
    live: &mut LiveProcess,
    bytes: Vec<u8>,
    check: &dyn Fn() -> Result<(), String>,
) -> Result<(), String> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let mut command = WriteCommand { bytes, ack: sender };
    loop {
        check()?;
        live.exited()?;
        match live
            .writer
            .as_ref()
            .ok_or("Extension writer closed")?
            .try_send(command)
        {
            Ok(()) => break,
            Err(TrySendError::Full(returned)) => {
                command = returned;
                thread::sleep(POLL);
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err("Extension writer disconnected".into())
            }
        }
    }
    loop {
        check()?;
        match receiver.recv_timeout(POLL) {
            Ok(result) => return result,
            Err(RecvTimeoutError::Timeout) => live.exited()?,
            Err(RecvTimeoutError::Disconnected) => {
                return Err("Extension writer disconnected".into())
            }
        }
    }
}

fn ui_dialog(frame: &Value, remaining: Duration) -> Result<(String, UiDialog), String> {
    let request_id = frame["requestId"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control))
        .ok_or("Invalid extension host request ID")?
        .to_owned();
    let method = frame["method"]
        .as_str()
        .ok_or("Extension host request has no method")?;
    let mut params = frame["params"]
        .as_object()
        .ok_or("Extension host request params must be an object")?
        .clone();
    if params.contains_key("method") {
        return Err("Extension host request params must not override method".into());
    }
    // Round up: an omitted UI deadline must not turn the parent request's
    // deadline into an earlier successful "cancelled" response.
    let remaining_ms = u64::try_from(remaining.as_nanos().div_ceil(1_000_000)).unwrap_or(u64::MAX);
    let timeout = match params.get("timeout") {
        None | Some(Value::Null) => remaining_ms,
        Some(value) => value
            .as_u64()
            .ok_or("Extension UI timeout must be unsigned milliseconds")?
            .min(remaining_ms),
    };
    params.insert("method".into(), Value::String(method.into()));
    params.insert("timeout".into(), json!(timeout));
    let dialog = serde_json::from_value(Value::Object(params))
        .map_err(|error| format!("Invalid extension host request: {error}"))?;
    Ok((request_id, dialog))
}

struct LiveProcess {
    child: Child,
    tree: ProcessTree,
    writer: Option<SyncSender<WriteCommand>>,
    output: Option<Receiver<OutputFrame>>,
    readers: Vec<JoinHandle<()>>,
}
impl LiveProcess {
    fn close(&mut self) {
        self.tree.close();
        let _ = self.child.kill();
        self.writer = None;
        self.output = None;
        let deadline = Instant::now() + Duration::from_millis(500);
        while self.readers.iter().any(|reader| !reader.is_finished()) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        for reader in self.readers.drain(..) {
            if reader.is_finished() {
                let _ = reader.join();
            }
        }
        let _ = self.child.try_wait();
    }
    fn exited(&mut self) -> Result<(), String> {
        if let Some(status) = self.child.try_wait().map_err(|error| error.to_string())? {
            return Err(format!("Native extension process exited ({status})"));
        }
        Ok(())
    }
}
impl Drop for LiveProcess {
    fn drop(&mut self) {
        self.close();
    }
}

struct ProcessState {
    live: Option<LiveProcess>,
    sequence: u64,
    failure: Option<String>,
}
pub struct NativeProcess {
    manifest: NativeExtensionManifest,
    state: Mutex<ProcessState>,
    stderr: Arc<Mutex<Vec<u8>>>,
    closing: AtomicBool,
    ui: Mutex<UiHandle>,
}
impl NativeProcess {
    pub fn spawn(
        manifest: NativeExtensionManifest,
        workspace: &Path,
        signal: &CancellationToken,
    ) -> Result<Self, String> {
        manifest.validate()?;
        if signal.is_cancelled() {
            return Err("Extension operation aborted".into());
        }
        if !workspace.is_dir() {
            return Err("Extension workspace is not a directory".into());
        }
        let (mut child, tree) = spawn_native(&manifest.executable, &manifest.args, workspace)?;
        let stdout = child.stdout.take().ok_or("Extension stdout missing")?;
        let mut stderr = child.stderr.take().ok_or("Extension stderr missing")?;
        let mut stdin = child.stdin.take().ok_or("Extension stdin missing")?;
        let (write_sender, write_receiver) = mpsc::sync_channel::<WriteCommand>(1);
        let writer = thread::spawn(move || {
            while let Ok(command) = write_receiver.recv() {
                let result = stdin
                    .write_all(&command.bytes)
                    .and_then(|_| stdin.flush())
                    .map_err(|error| format!("Extension stdin write: {error}"));
                let failed = result.is_err();
                let _ = command.ack.send(result);
                if failed {
                    break;
                }
            }
        });
        let (output_sender, output_receiver) = mpsc::sync_channel(32);
        let limit = manifest.max_frame_bytes;
        let out = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut frame = Vec::new();
                let read = reader
                    .by_ref()
                    .take((limit + 1) as u64)
                    .read_until(b'\n', &mut frame);
                let result = match read {
                    Ok(0) => OutputFrame::Failure("Extension stdout closed".into()),
                    Ok(_) if frame.len() > limit || !frame.ends_with(b"\n") => {
                        OutputFrame::Failure(
                            "Extension JSONL frame exceeds limit or lacks LF".into(),
                        )
                    }
                    Ok(_) => match serde_json::from_slice::<Value>(&frame) {
                        Ok(value) if value.is_object() => OutputFrame::Frame(value),
                        Ok(_) => {
                            OutputFrame::Failure("Extension JSONL frame must be an object".into())
                        }
                        Err(error) => {
                            OutputFrame::Failure(format!("Malformed extension JSONL: {error}"))
                        }
                    },
                    Err(error) => OutputFrame::Failure(format!("Extension stdout read: {error}")),
                };
                let failed = matches!(result, OutputFrame::Failure(_));
                if output_sender.send(result).is_err() || failed {
                    break;
                }
            }
        });
        let stderr_bytes = Arc::new(Mutex::new(Vec::new()));
        let collected = stderr_bytes.clone();
        let err = thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match stderr.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        let mut bytes = collected.lock().unwrap_or_else(|error| error.into_inner());
                        let keep = count.min(STDERR_CAP.saturating_sub(bytes.len()));
                        bytes.extend_from_slice(&buffer[..keep]);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            manifest,
            state: Mutex::new(ProcessState {
                live: Some(LiveProcess {
                    child,
                    tree,
                    writer: Some(write_sender),
                    output: Some(output_receiver),
                    readers: vec![writer, out, err],
                }),
                sequence: 0,
                failure: None,
            }),
            stderr: stderr_bytes,
            closing: AtomicBool::new(false),
            ui: Mutex::new(UiHandle::default()),
        })
    }
    pub fn id(&self) -> &str {
        &self.manifest.id
    }
    pub fn bind_ui(&self, ui: UiHandle) {
        *self.ui.lock().unwrap_or_else(|error| error.into_inner()) = ui;
    }
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(
            &self
                .stderr
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        )
        .into_owned()
    }
    /// Serialized, bounded requests, including time spent waiting for another call.
    pub fn request(
        &self,
        method: &str,
        params: Value,
        signal: &CancellationToken,
        on_update: &mut dyn FnMut(Value),
    ) -> Result<Value, String> {
        self.request_with_timeout(
            method,
            params,
            signal,
            Duration::from_millis(self.manifest.request_timeout_ms),
            on_update,
        )
    }
    pub fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        signal: &CancellationToken,
        timeout: Duration,
        on_update: &mut dyn FnMut(Value),
    ) -> Result<Value, String> {
        let start = Instant::now();
        let timeout = timeout.min(Duration::from_millis(self.manifest.request_timeout_ms));
        let check = || {
            if self.closing.load(Ordering::Acquire) {
                Err("Extension process is shut down".to_owned())
            } else if signal.is_cancelled() {
                Err("Extension operation aborted".to_owned())
            } else if start.elapsed() >= timeout {
                Err("Extension request timed out".to_owned())
            } else {
                Ok(())
            }
        };
        let mut state = loop {
            check()?;
            match self.state.try_lock() {
                Ok(state) => break state,
                Err(TryLockError::WouldBlock) => thread::sleep(POLL),
                Err(TryLockError::Poisoned(error)) => {
                    let mut state = error.into_inner();
                    if let Some(mut live) = state.live.take() {
                        live.close();
                    }
                    state.failure = Some("Extension transport panicked".into());
                    return Err("Extension transport panicked".into());
                }
            }
        };
        if let Some(error) = &state.failure {
            return Err(error.clone());
        }
        state.sequence = state
            .sequence
            .checked_add(1)
            .ok_or("Extension request sequence exhausted")?;
        let id = state.sequence;
        let frame = json!({"protocol":EXTENSION_PROTOCOL,"version":EXTENSION_PROTOCOL_VERSION,"id":id,"method":method,"params":params});
        let mut bytes = serde_json::to_vec(&frame).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        if bytes.len() > self.manifest.max_frame_bytes {
            return Err("Extension request frame exceeds limit".into());
        }
        let mut fatal = true;
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let live = state
                .live
                .as_mut()
                .ok_or("Extension process is shut down")?;
            send_frame(live, bytes, &check)?;
            let mut updates = 0usize;
            let mut requests = HashSet::new();
            let mut pending_ui: Option<PendingUi> = None;
            loop {
                check()?;
                if let Some(pending) = &pending_ui {
                    match pending.result.try_recv() {
                        Ok(reply) => {
                            let frame = match reply {
                                Ok(reply) => json!({"protocol":EXTENSION_PROTOCOL,"version":EXTENSION_PROTOCOL_VERSION,"id":id,"type":"host_result","requestId":pending.request_id,"success":true,"result":reply}),
                                Err(error) => json!({"protocol":EXTENSION_PROTOCOL,"version":EXTENSION_PROTOCOL_VERSION,"id":id,"type":"host_result","requestId":pending.request_id,"success":false,"error":error}),
                            };
                            let mut bytes = serde_json::to_vec(&frame).map_err(|error| error.to_string())?;
                            bytes.push(b'\n');
                            if bytes.len() > self.manifest.max_frame_bytes {
                                return Err("Extension UI result frame exceeds limit".into());
                            }
                            send_frame(live, bytes, &check)?;
                            pending_ui = None;
                        }
                        Err(TryRecvError::Empty) => {}
                        Err(TryRecvError::Disconnected) => return Err("Extension UI worker disconnected".into()),
                    }
                }
                let received = live
                    .output
                    .as_ref()
                    .ok_or("Extension output closed")?
                    .recv_timeout(POLL);
                let frame = match received {
                    Ok(OutputFrame::Frame(frame)) => frame,
                    Ok(OutputFrame::Failure(error)) => return Err(error),
                    Err(RecvTimeoutError::Timeout) => {
                        live.exited()?;
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        return Err("Extension stdout reader disconnected".into())
                    }
                };
                if frame["protocol"] != EXTENSION_PROTOCOL
                    || frame["version"] != EXTENSION_PROTOCOL_VERSION
                    || frame["id"] != id
                {
                    return Err("Extension response protocol/version/id mismatch".into());
                }
                match frame["type"].as_str() {
                    Some("host_request") => {
                        if pending_ui.is_some() {
                            return Err("Overlapping extension host requests".into());
                        }
                        let (request_id, dialog) = ui_dialog(&frame, timeout.saturating_sub(start.elapsed()))?;
                        if !requests.insert(request_id.clone()) {
                            return Err("Duplicate extension host request ID".into());
                        }
                        if requests.len() > 32 {
                            return Err("Extension host request limit exceeded".into());
                        }
                        let ui = self.ui.lock().unwrap_or_else(|error| error.into_inner()).clone();
                        pending_ui = Some(PendingUi::start(request_id, dialog, ui));
                    }
                    Some("update") => {
                        updates += 1;
                        if updates > 4096 {
                            return Err("Extension streaming update limit exceeded".into());
                        }
                        on_update(frame["result"].clone());
                    }
                    Some("result") => {
                        if pending_ui.is_some() {
                            return Err("Extension result arrived before its UI request completed".into());
                        }
                        if frame["success"].as_bool() == Some(true) {
                            return Ok(frame["result"].clone());
                        }
                        if frame["success"].as_bool() == Some(false) {
                            let error = frame["error"]
                                .as_str()
                                .ok_or("Extension error result has no error text")?;
                            fatal = false;
                            return Err(format!("Extension request failed: {error}"));
                        }
                        return Err("Extension response has no success flag".into());
                    }
                    _ => return Err("Unknown extension response frame type".into()),
                }
            }
        }))
        .unwrap_or_else(|_| Err("Native extension request panicked".into()));
        if let Err(error) = &outcome {
            if fatal {
                if let Some(mut live) = state.live.take() {
                    live.close();
                }
                state.failure = Some(error.clone());
            }
        }
        outcome
    }
    pub fn shutdown(&self) {
        self.closing.store(true, Ordering::Release);
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(mut live) = state.live.take() {
            live.close();
        }
        state.failure = Some("Extension process is shut down".into());
    }
    pub fn abort(&self) {
        self.closing.store(true, Ordering::Release);
    }
    pub fn is_busy(&self) -> bool {
        self.state.try_lock().is_err()
    }
}
impl Drop for NativeProcess {
    fn drop(&mut self) {
        self.shutdown();
    }
}
