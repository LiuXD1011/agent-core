//! Independent task organization state: ordinary session saves never overwrite it.
use agent_core::types::CancellationToken;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const LIMIT: u64 = 1024 * 1024;
const GROUP_NAME_LIMIT: usize = 80;
const GROUP_ID_ATTEMPTS: usize = 4;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskState {
    pub revision: u64,
    pub pinned: bool,
    pub archived: bool,
    pub pinned_rank: Option<usize>,
    /// Project this task is filed under. Organizing state only: filing a task
    /// never touches its session, messages or durable queue.
    pub group: Option<String>,
}

/// A user-created project. Tasks reference it by id; the name is display only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskGroup {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskChange {
    SetPinned(bool),
    SetArchived(bool),
}

/// One change applied to a set of tasks: either every task changes, or none does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchAction {
    Archive,
    Restore,
    Pin,
    Unpin,
    MoveToGroup(String),
    RemoveFromGroup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskStoreError {
    InvalidId,
    MissingSession,
    InvalidStore,
    InvalidGroup,
    GroupExists,
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
            Self::InvalidGroup => "分组不存在或名称无效",
            Self::GroupExists => "同名分组已存在",
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
    pub groups: Vec<TaskGroup>,
    pub states: std::collections::BTreeMap<String, TaskState>,
}
#[derive(Default, Serialize, Deserialize)]
struct Record {
    archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    group: Option<String>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}
#[derive(Serialize, Deserialize)]
struct StoredGroup {
    id: String,
    name: String,
    #[serde(flatten)]
    extra: Map<String, Value>,
}
#[derive(Serialize, Deserialize)]
struct Stored {
    schema: u32,
    revision: u64,
    pinned_ids: Vec<String>,
    /// Added after the first release. Older builds keep these entries in their
    /// flattened unknown-field map, so a file written here stays readable there.
    #[serde(default)]
    groups: Vec<StoredGroup>,
    records: std::collections::BTreeMap<String, Record>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}
impl Stored {
    fn state(&self, id: &str) -> TaskState {
        let pinned_rank = self.pinned_ids.iter().position(|item| item == id);
        let record = self.records.get(id);
        TaskState {
            revision: self.revision,
            pinned: pinned_rank.is_some(),
            pinned_rank,
            archived: record.is_some_and(|record| record.archived),
            group: record.and_then(|record| record.group.clone()),
        }
    }
    fn catalog(&self) -> TaskCatalog {
        TaskCatalog {
            revision: self.revision,
            pinned_ids: self.pinned_ids.clone(),
            groups: self
                .groups
                .iter()
                .map(|group| TaskGroup {
                    id: group.id.clone(),
                    name: group.name.clone(),
                })
                .collect(),
            states: self
                .records
                .keys()
                .map(|id| (id.clone(), self.state(id)))
                .collect(),
        }
    }
    fn known_groups(&self) -> std::collections::BTreeSet<&str> {
        self.groups.iter().map(|group| group.id.as_str()).collect()
    }
    fn group_position(&self, id: &str) -> Option<usize> {
        self.groups.iter().position(|group| group.id == id)
    }
    fn name_taken(&self, name: &str, except: Option<&str>) -> bool {
        self.groups
            .iter()
            .any(|group| group.name == name && Some(group.id.as_str()) != except)
    }
    fn assign(&mut self, id: &str, group: Option<String>) {
        self.records.entry(id.into()).or_default().group = group;
    }
}
#[derive(Clone)]
pub struct TaskStore {
    directory: PathBuf,
}
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}
/// Surrounding whitespace is not part of a name; the stored name is what the UI shows.
fn clean_group_name(name: &str) -> Result<String, TaskStoreError> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.chars().count() > GROUP_NAME_LIMIT
        || trimmed.chars().any(char::is_control)
    {
        return Err(TaskStoreError::InvalidGroup);
    }
    Ok(trimmed.to_string())
}
fn new_group_id(stored: &Stored) -> Result<String, TaskStoreError> {
    for _ in 0..GROUP_ID_ATTEMPTS {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).map_err(|_| TaskStoreError::Storage)?;
        let id = format!(
            "g{}",
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        if stored.group_position(&id).is_none() {
            return Ok(id);
        }
    }
    Err(TaskStoreError::Storage)
}
/// Rejects an empty set, duplicates and malformed identifiers before any lock is taken.
fn checked_ids(ids: &[String]) -> Result<(), TaskStoreError> {
    let unique: std::collections::BTreeSet<&String> = ids.iter().collect();
    if ids.is_empty() || unique.len() != ids.len() || ids.iter().any(|id| !valid_id(id)) {
        return Err(TaskStoreError::InvalidId);
    }
    Ok(())
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
                    groups: Vec::new(),
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
        let group_ids = stored.known_groups();
        if group_ids.len() != stored.groups.len()
            || stored
                .groups
                .iter()
                .any(|group| !valid_id(&group.id) || clean_group_name(&group.name).is_err())
            || stored.records.values().any(|record| {
                record
                    .group
                    .as_deref()
                    .is_some_and(|id| !group_ids.contains(id))
            })
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
    /// Every id must be known: already recorded here, or an existing session.
    fn ensure_known(&self, stored: &Stored, ids: &[String]) -> Result<(), TaskStoreError> {
        for id in ids {
            if !stored.records.contains_key(id) {
                self.session_exists(id)?;
            }
        }
        Ok(())
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
    /// Applies one action to every id as a single transaction: a rejected id, a
    /// stale revision or a cancellation leaves the file byte-identical.
    pub fn apply_batch(
        &self,
        expected_revision: u64,
        ids: &[String],
        action: BatchAction,
        signal: &CancellationToken,
    ) -> Result<TaskCatalog, TaskStoreError> {
        cancelled(signal)?;
        checked_ids(ids)?;
        if let BatchAction::MoveToGroup(group) = &action {
            if !valid_id(group) {
                return Err(TaskStoreError::InvalidGroup);
            }
        }
        let _lock = lock(&self.directory.join("task-catalog.lock"))?;
        cancelled(signal)?;
        let mut stored = self.read()?;
        if stored.revision != expected_revision {
            return Err(TaskStoreError::Conflict);
        }
        if let BatchAction::MoveToGroup(group) = &action {
            if stored.group_position(group).is_none() {
                return Err(TaskStoreError::InvalidGroup);
            }
        }
        self.ensure_known(&stored, ids)?;
        // Archiving, pinning and filing all record the task, exactly like update():
        // a pinned id without a record would make the whole catalog unreadable.
        for id in ids {
            stored.records.entry(id.clone()).or_default();
        }
        match &action {
            // The newest pin takes the front slot, so insert in reverse selection
            // order to keep the order the user chose.
            BatchAction::Pin => {
                for id in ids.iter().rev() {
                    if !stored.pinned_ids.iter().any(|pinned| pinned == id) {
                        stored.pinned_ids.insert(0, id.clone());
                    }
                }
            }
            BatchAction::Unpin => {
                stored.pinned_ids.retain(|pinned| !ids.contains(pinned));
            }
            _ => {}
        }
        for id in ids {
            match &action {
                BatchAction::Archive => {
                    stored.records.entry(id.clone()).or_default().archived = true
                }
                BatchAction::Restore => {
                    stored.records.entry(id.clone()).or_default().archived = false
                }
                BatchAction::MoveToGroup(group) => stored.assign(id, Some(group.clone())),
                BatchAction::RemoveFromGroup => stored.assign(id, None),
                BatchAction::Pin | BatchAction::Unpin => {}
            }
        }
        self.commit(&mut stored, signal)?;
        Ok(stored.catalog())
    }
    pub fn create_group(
        &self,
        expected_revision: u64,
        name: &str,
        signal: &CancellationToken,
    ) -> Result<TaskCatalog, TaskStoreError> {
        self.create_group_with_tasks(expected_revision, name, &[], signal)
    }
    /// Creates the project and files the given tasks into it in one transaction,
    /// so a rejected task cannot leave an empty group behind.
    pub fn create_group_with_tasks(
        &self,
        expected_revision: u64,
        name: &str,
        ids: &[String],
        signal: &CancellationToken,
    ) -> Result<TaskCatalog, TaskStoreError> {
        cancelled(signal)?;
        let name = clean_group_name(name)?;
        if !ids.is_empty() {
            checked_ids(ids)?;
        }
        let _lock = lock(&self.directory.join("task-catalog.lock"))?;
        cancelled(signal)?;
        let mut stored = self.read()?;
        if stored.revision != expected_revision {
            return Err(TaskStoreError::Conflict);
        }
        if stored.name_taken(&name, None) {
            return Err(TaskStoreError::GroupExists);
        }
        self.ensure_known(&stored, ids)?;
        let id = new_group_id(&stored)?;
        stored.groups.push(StoredGroup {
            id: id.clone(),
            name,
            extra: Map::new(),
        });
        for task in ids {
            stored.assign(task, Some(id.clone()));
        }
        self.commit(&mut stored, signal)?;
        Ok(stored.catalog())
    }
    pub fn rename_group(
        &self,
        expected_revision: u64,
        id: &str,
        name: &str,
        signal: &CancellationToken,
    ) -> Result<TaskCatalog, TaskStoreError> {
        cancelled(signal)?;
        let name = clean_group_name(name)?;
        let _lock = lock(&self.directory.join("task-catalog.lock"))?;
        cancelled(signal)?;
        let mut stored = self.read()?;
        if stored.revision != expected_revision {
            return Err(TaskStoreError::Conflict);
        }
        let position = stored
            .group_position(id)
            .ok_or(TaskStoreError::InvalidGroup)?;
        if stored.name_taken(&name, Some(id)) {
            return Err(TaskStoreError::GroupExists);
        }
        stored.groups[position].name = name;
        self.commit(&mut stored, signal)?;
        Ok(stored.catalog())
    }
    /// Removes the project only. Its tasks, pins, archives and queues stay, and
    /// tasks that were filed under it become ungrouped.
    pub fn delete_group(
        &self,
        expected_revision: u64,
        id: &str,
        signal: &CancellationToken,
    ) -> Result<TaskCatalog, TaskStoreError> {
        cancelled(signal)?;
        if !valid_id(id) {
            return Err(TaskStoreError::InvalidGroup);
        }
        let _lock = lock(&self.directory.join("task-catalog.lock"))?;
        cancelled(signal)?;
        let mut stored = self.read()?;
        if stored.revision != expected_revision {
            return Err(TaskStoreError::Conflict);
        }
        let position = stored
            .group_position(id)
            .ok_or(TaskStoreError::InvalidGroup)?;
        stored.groups.remove(position);
        for record in stored.records.values_mut() {
            if record.group.as_deref() == Some(id) {
                record.group = None;
            }
        }
        self.commit(&mut stored, signal)?;
        Ok(stored.catalog())
    }
    pub fn reorder_groups(
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
        let expected: std::collections::BTreeSet<_> = stored
            .groups
            .iter()
            .map(|group| group.id.as_str())
            .collect();
        let actual: std::collections::BTreeSet<_> = ids.iter().map(String::as_str).collect();
        if expected != actual || actual.len() != ids.len() {
            return Err(TaskStoreError::InvalidGroup);
        }
        let mut reordered = Vec::with_capacity(stored.groups.len());
        for id in &ids {
            let position = stored
                .group_position(id)
                .ok_or(TaskStoreError::InvalidGroup)?;
            let id = stored.groups[position].id.clone();
            let name = stored.groups[position].name.clone();
            let extra = std::mem::take(&mut stored.groups[position].extra);
            reordered.push(StoredGroup { id, name, extra });
        }
        stored.groups = reordered;
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
    // A brand-new store has no directory yet; organizing a fresh install must not
    // depend on some session having been saved first.
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|_| TaskStoreError::Storage)?;
    }
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
    let file = match options.open(path) {
        Ok(file) => file,
        // Another writer holds the exclusive handle: retry after refreshing.
        // A genuinely missing path is a storage problem, not a busy store.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(TaskStoreError::Storage)
        }
        Err(_) => return Err(TaskStoreError::Busy),
    };
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
