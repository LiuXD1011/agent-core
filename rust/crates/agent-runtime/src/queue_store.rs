use crate::runtime::{QueueError, QueueItem, QueueItemId, QueueMode};
use agent_core::types::Message;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const LIMIT: u64 = 64 * 1024 * 1024;
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct StoredQueue {
    pub schema: u32,
    #[serde(default)]
    pub workspace: Option<PathBuf>,
    pub session_id: String,
    pub revision: u64,
    pub next_id: u64,
    pub items: VecDeque<QueueItem>,
    pub inflight: Vec<QueueItem>,
    pub raw: BTreeMap<QueueItemId, Message>,
    pub paused: bool,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub dispatched: u64,
}
impl StoredQueue {
    pub fn empty(session_id: String) -> Self {
        Self {
            schema: 1,
            workspace: None,
            session_id,
            revision: 0,
            next_id: 1,
            items: VecDeque::new(),
            inflight: vec![],
            raw: BTreeMap::new(),
            paused: false,
            steering_mode: QueueMode::default(),
            follow_up_mode: QueueMode::default(),
            dispatched: 0,
        }
    }
    fn validate(&self, session_id: &str) -> Result<(), QueueError> {
        if self.schema != 1
            || self.session_id != session_id
            || self.items.len() + self.inflight.len() > 4096
        {
            return Err(QueueError::InvalidStore);
        }
        let mut ids = BTreeSet::new();
        for item in self.items.iter().chain(&self.inflight) {
            if item.id.0 == 0 || item.id.0 >= self.next_id || !ids.insert(item.id) {
                return Err(QueueError::InvalidStore);
            }
        }
        if self.next_id == 0 || self.raw.keys().any(|id| !ids.contains(id)) {
            return Err(QueueError::InvalidStore);
        }
        Ok(())
    }
}
pub(crate) struct QueueStore {
    path: PathBuf,
    session_id: String,
    _lock: File,
}
impl QueueStore {
    pub fn open(directory: &Path, session_id: &str) -> Result<(Self, StoredQueue), QueueError> {
        if session_id.is_empty()
            || !session_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(QueueError::InvalidStore);
        }
        fs::create_dir_all(directory).map_err(|_| QueueError::Storage)?;
        let path = directory.join(format!("{session_id}.json"));
        let lock_path = directory.join(format!("{session_id}.lock"));
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
        let lock = options.open(lock_path).map_err(|_| QueueError::StoreBusy)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            unsafe extern "C" {
                fn flock(fd: i32, operation: i32) -> i32;
            }
            if unsafe { flock(lock.as_raw_fd(), 2 | 4) } != 0 {
                return Err(QueueError::StoreBusy);
            }
        }
        let store = Self {
            path,
            session_id: session_id.into(),
            _lock: lock,
        };
        let state = store
            .read()?
            .unwrap_or_else(|| StoredQueue::empty(session_id.into()));
        Ok((store, state))
    }
    fn read(&self) -> Result<Option<StoredQueue>, QueueError> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(QueueError::Storage),
        };
        if file.metadata().map_err(|_| QueueError::Storage)?.len() > LIMIT {
            return Err(QueueError::InvalidStore);
        }
        let mut bytes = Vec::new();
        file.take(LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| QueueError::Storage)?;
        if bytes.len() as u64 > LIMIT {
            return Err(QueueError::InvalidStore);
        }
        let state: StoredQueue =
            serde_json::from_slice(&bytes).map_err(|_| QueueError::InvalidStore)?;
        state.validate(&self.session_id)?;
        Ok(Some(state))
    }
    pub fn save(&self, state: &StoredQueue, expected_revision: u64) -> Result<(), QueueError> {
        state.validate(&self.session_id)?;
        if self.read()?.map_or(0, |state| state.revision) != expected_revision {
            return Err(QueueError::StoreConflict);
        }
        let bytes = serde_json::to_vec(state).map_err(|_| QueueError::InvalidStore)?;
        if bytes.len() as u64 > LIMIT {
            return Err(QueueError::InvalidStore);
        }
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|_| QueueError::Storage)?;
        let suffix: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
        let temporary = self.path.with_extension(format!("{suffix}.tmp"));
        let mut owns_temporary = false;
        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary).map_err(|_| QueueError::Storage)?;
            owns_temporary = true;
            file.write_all(&bytes)
                .and_then(|_| file.sync_all())
                .map_err(|_| QueueError::Storage)?;
            drop(file);
            #[cfg(windows)]
            {
                use std::os::windows::ffi::OsStrExt;
                use windows_sys::Win32::Storage::FileSystem::{
                    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
                };
                let src: Vec<_> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
                let dst: Vec<_> = self.path.as_os_str().encode_wide().chain(Some(0)).collect();
                if unsafe {
                    MoveFileExW(
                        src.as_ptr(),
                        dst.as_ptr(),
                        MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                    )
                } == 0
                {
                    return Err(QueueError::Storage);
                }
            }
            #[cfg(not(windows))]
            fs::rename(&temporary, &self.path).map_err(|_| QueueError::Storage)?;
            #[cfg(unix)]
            File::open(self.path.parent().ok_or(QueueError::Storage)?)
                .and_then(|file| file.sync_all())
                .map_err(|_| QueueError::CommitUncertain)?;
            Ok(())
        })();
        if result.is_err() && owns_temporary {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}
