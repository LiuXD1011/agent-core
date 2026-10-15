//! Independent task organization state: ordinary session saves never overwrite it.
use agent_core::types::CancellationToken;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const LIMIT: u64 = 1024 * 1024;
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskState {
    pub revision: u64,
    pub pinned: bool,
    pub archived: bool,
    pub pinned_rank: Option<usize>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskChange {
    SetPinned(bool),
    SetArchived(bool),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskStoreError {
    InvalidId,
    MissingSession,
    InvalidStore,
    Storage,
    Busy,
    Conflict,
    Cancelled,
    CommitUncertain,
}
impl std::fmt::Display for TaskStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidId => "任务 ID 无效",
            Self::MissingSession => "任务会话尚未保存或已不存在",
            Self::InvalidStore => "任务状态记录无效，原文件保持不变",
            Self::Storage => "任务状态存储失败",
            Self::Busy => "任务状态正由另一个操作使用，请刷新后重试",
            Self::Conflict => "任务状态已变更，请刷新后重试",
            Self::Cancelled => "任务状态操作已取消",
            Self::CommitUncertain => "任务状态已发布，但持久保存未确认；请重新读取",
        })
    }
}
impl std::error::Error for TaskStoreError {}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskCatalog {
    pub revision: u64,
    pub pinned_ids: Vec<String>,
    pub states: std::collections::BTreeMap<String, TaskState>,
}
#[derive(Default, Serialize, Deserialize)]
struct Record {
    archived: bool,
    #[serde(flatten)]
    extra: Map<String, Value>,
}
#[derive(Serialize, Deserialize)]
struct Stored {
    schema: u32,
    revision: u64,
    pinned_ids: Vec<String>,
    records: std::collections::BTreeMap<String, Record>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}
impl Stored {
    fn state(&self, id: &str) -> TaskState {
        let pinned_rank = self.pinned_ids.iter().position(|item| item == id);
        TaskState {
            revision: self.revision,
            pinned: pinned_rank.is_some(),
            pinned_rank,
            archived: self.records.get(id).is_some_and(|record| record.archived),
        }
    }
    fn catalog(&self) -> TaskCatalog {
        TaskCatalog {
            revision: self.revision,
            pinned_ids: self.pinned_ids.clone(),
            states: self
                .records
                .keys()
                .map(|id| (id.clone(), self.state(id)))
                .collect(),
        }
    }
}
#[derive(Clone)]
pub struct TaskStore {
    directory: PathBuf,
}
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}
impl TaskStore {
    pub(crate) fn new(directory: PathBuf) -> Self {
        Self { directory }
    }
    fn path(&self) -> PathBuf {
        self.directory.join("task-catalog.state")
    }
    fn session_exists(&self, id: &str) -> Result<(), TaskStoreError> {
        if !valid_id(id) {
            return Err(TaskStoreError::InvalidId);
        }
        match fs::metadata(self.directory.join(format!("{id}.json"))) {
            Ok(meta) if meta.is_file() => Ok(()),
            Ok(_) => Err(TaskStoreError::MissingSession),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(TaskStoreError::MissingSession)
            }
            Err(_) => Err(TaskStoreError::Storage),
        }
    }
    fn read(&self) -> Result<Stored, TaskStoreError> {
        let file = match File::open(self.path()) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Stored {
                    schema: 1,
                    revision: 0,
                    pinned_ids: Vec::new(),
                    records: Default::default(),
                    extra: Map::new(),
                })
            }
            Err(_) => return Err(TaskStoreError::Storage),
        };
        if file.metadata().map_err(|_| TaskStoreError::Storage)?.len() > LIMIT {
            return Err(TaskStoreError::InvalidStore);
        }
        let mut bytes = Vec::new();
        file.take(LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| TaskStoreError::Storage)?;
        if bytes.len() as u64 > LIMIT {
            return Err(TaskStoreError::InvalidStore);
        }
        let stored: Stored =
            serde_json::from_slice(&bytes).map_err(|_| TaskStoreError::InvalidStore)?;
        let unique: std::collections::BTreeSet<_> = stored.pinned_ids.iter().collect();
        if stored.schema != 1
            || stored.revision == 0
            || unique.len() != stored.pinned_ids.len()
            || stored.records.keys().any(|id| !valid_id(id))
            || stored
                .pinned_ids
                .iter()
                .any(|id| !stored.records.contains_key(id))
        {
            return Err(TaskStoreError::InvalidStore);
        }
        Ok(stored)
    }
    pub fn load_catalog(&self) -> Result<TaskCatalog, TaskStoreError> {
        Ok(self.read()?.catalog())
    }
    pub fn load(&self, id: &str) -> Result<TaskState, TaskStoreError> {
        self.session_exists(id)?;
        Ok(self.read()?.state(id))
    }
    fn commit(
        &self,
        stored: &mut Stored,
        signal: &CancellationToken,
    ) -> Result<(), TaskStoreError> {
        stored.revision = stored
            .revision
            .checked_add(1)
            .ok_or(TaskStoreError::InvalidStore)?;
        let bytes = serde_json::to_vec(stored).map_err(|_| TaskStoreError::InvalidStore)?;
        if bytes.len() as u64 > LIMIT {
            return Err(TaskStoreError::InvalidStore);
        }
        publish(&self.path(), &bytes, signal)
    }
    pub fn update(
        &self,
        id: &str,
        expected_revision: u64,
        change: TaskChange,
        signal: &CancellationToken,
    ) -> Result<TaskState, TaskStoreError> {
        cancelled(signal)?;
        if !valid_id(id) {
            return Err(TaskStoreError::InvalidId);
        }
        let _lock = lock(&self.directory.join("task-catalog.lock"))?;
        cancelled(signal)?;
        let mut stored = self.read()?;
        if !matches!(change, TaskChange::SetPinned(false)) || !stored.records.contains_key(id) {
            self.session_exists(id)?;
        }
        if stored.revision != expected_revision {
            return Err(TaskStoreError::Conflict);
        }
        let record = stored.records.entry(id.into()).or_default();
        match change {
            TaskChange::SetPinned(true) => {
                if !stored.pinned_ids.iter().any(|pinned| pinned == id) {
                    stored.pinned_ids.insert(0, id.into());
                }
            }
            TaskChange::SetPinned(false) => stored.pinned_ids.retain(|pinned| pinned != id),
            TaskChange::SetArchived(value) => record.archived = value,
        }
        self.commit(&mut stored, signal)?;
        Ok(stored.state(id))
    }
    pub fn reorder_pinned(
        &self,
        expected_revision: u64,
        ids: Vec<String>,
        signal: &CancellationToken,
    ) -> Result<TaskCatalog, TaskStoreError> {
        cancelled(signal)?;
        let _lock = lock(&self.directory.join("task-catalog.lock"))?;
        let mut stored = self.read()?;
        if stored.revision != expected_revision {
            return Err(TaskStoreError::Conflict);
        }
        let expected: std::collections::BTreeSet<_> = stored.pinned_ids.iter().collect();
        let actual: std::collections::BTreeSet<_> = ids.iter().collect();
        if expected != actual || actual.len() != ids.len() {
            return Err(TaskStoreError::InvalidStore);
        }
        stored.pinned_ids = ids;
        self.commit(&mut stored, signal)?;
        Ok(stored.catalog())
    }
}
fn cancelled(signal: &CancellationToken) -> Result<(), TaskStoreError> {
    if signal.is_cancelled() {
        Err(TaskStoreError::Cancelled)
    } else {
        Ok(())
    }
}
fn lock(path: &Path) -> Result<File, TaskStoreError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path).map_err(|_| TaskStoreError::Busy)?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        unsafe extern "C" {
            fn flock(fd: i32, operation: i32) -> i32;
        }
        if unsafe { flock(file.as_raw_fd(), 2 | 4) } != 0 {
            return Err(TaskStoreError::Busy);
        }
    }
    Ok(file)
}
struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn publish(path: &Path, bytes: &[u8], signal: &CancellationToken) -> Result<(), TaskStoreError> {
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|_| TaskStoreError::Storage)?;
    let suffix: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
    let temporary = path.with_extension(format!("task-{suffix}.tmp"));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|_| TaskStoreError::Storage)?;
    let _cleanup = Temporary(temporary.clone());
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| TaskStoreError::Storage)?;
    drop(file);
    cancelled(signal)?;
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };
        let source: Vec<_> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
        let target: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        if unsafe {
            MoveFileExW(
                source.as_ptr(),
                target.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(TaskStoreError::Storage);
        }
    }
    #[cfg(not(windows))]
    fs::rename(&temporary, path).map_err(|_| TaskStoreError::Storage)?;
    #[cfg(unix)]
    File::open(path.parent().ok_or(TaskStoreError::Storage)?)
        .and_then(|file| file.sync_all())
        .map_err(|_| TaskStoreError::CommitUncertain)?;
    Ok(())
}
