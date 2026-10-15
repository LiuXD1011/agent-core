//! Bounded, read-only disk previews selected by a successful historical result.
//! Call on a worker. A preview describes current bytes, not the old tool output.
use crate::{session::Session, workbench::TaskProjection};
use agent_core::types::CancellationToken;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
};

pub const MAX_PREVIEW_BYTES: u64 = 256 * 1024;

/// Deliberately not Debug: text and paths can contain private document data.
pub struct ArtifactPreview {
    pub text: String,
    pub byte_len: u64,
    pub sha256: String,
    pub entry_id: String,
    pub path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactError {
    Missing,
    Directory,
    OutsideWorkspace,
    Link,
    TooLarge,
    Unsupported,
    Cancelled,
    Changed,
    InvalidRecord,
    Io,
}
impl std::fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Missing => "文件已不存在或已移动。",
            Self::Directory => "产物路径指向目录。",
            Self::OutsideWorkspace => "产物路径超出工作区或使用了特殊路径。",
            Self::Link => "产物路径包含链接或重解析点。",
            Self::TooLarge => "文件超过 256 KiB 文本预览上限。",
            Self::Unsupported => "文件不是受支持的 UTF-8 文本。",
            Self::Cancelled => "产物读取已取消。",
            Self::Changed => "文件正在变化，请重新读取。",
            Self::InvalidRecord => "当前分支没有对应的成功文件操作记录。",
            Self::Io => "文件读取失败，请检查访问状态。",
        })
    }
}
impl std::error::Error for ArtifactError {}
fn check(cancel: &CancellationToken) -> Result<(), ArtifactError> {
    if cancel.is_cancelled() {
        Err(ArtifactError::Cancelled)
    } else {
        Ok(())
    }
}
fn io(error: std::io::Error) -> ArtifactError {
    match error.kind() {
        std::io::ErrorKind::NotFound => ArtifactError::Missing,
        _ if cfg!(windows) && error.raw_os_error() == Some(32) => ArtifactError::Changed,
        _ => ArtifactError::Io,
    }
}

/// Never accepts an arbitrary filesystem path from the caller. The selected
/// branch must contain a uniquely matched successful write/edit result.
pub fn read_recorded_artifact(
    session: &Session,
    entry_id: &str,
    cancel: &CancellationToken,
) -> Result<ArtifactPreview, ArtifactError> {
    check(cancel)?;
    let entries = session
        .get_branch(None)
        .map_err(|_| ArtifactError::InvalidRecord)?;
    let mut messages = Vec::new();
    let mut ids = Vec::new();
    for entry in entries.iter().filter(|e| e.entry_type == "message") {
        check(cancel)?;
        let message = crate::legacy_session::context_message(&entry.to_value(), &mut Vec::new())
            .map_err(|_| ArtifactError::InvalidRecord)?;
        if let Some(message) = message {
            messages.push(message);
            ids.push(entry.id.as_str());
        }
    }
    let projection = TaskProjection::from_messages(&messages, false);
    let artifact = projection
        .artifacts
        .iter()
        .find(|a| ids.get(a.result_message) == Some(&entry_id))
        .ok_or(ArtifactError::InvalidRecord)?;
    let workspace = workspace_path(&session.workspace)?;
    let target = target_path(&workspace, &artifact.path)?;
    check(cancel)?;
    let (mut file, _guards) = open_checked(&workspace, &target)?;
    let before = file.metadata().map_err(io)?;
    if before.is_dir() {
        return Err(ArtifactError::Directory);
    }
    if !before.is_file() {
        return Err(ArtifactError::Unsupported);
    }
    if before.len() > MAX_PREVIEW_BYTES {
        return Err(ArtifactError::TooLarge);
    }
    let mut bytes = Vec::new();
    let mut block = [0u8; 8192];
    loop {
        check(cancel)?;
        let n = file.read(&mut block).map_err(io)?;
        if n == 0 {
            break;
        }
        if bytes.len() as u64 + n as u64 > MAX_PREVIEW_BYTES {
            return Err(ArtifactError::TooLarge);
        }
        bytes.extend_from_slice(&block[..n]);
    }
    check(cancel)?;
    let after = file.metadata().map_err(io)?;
    if before.len() != after.len()
        || after.len() != bytes.len() as u64
        || before.modified().map_err(io)? != after.modified().map_err(io)?
    {
        return Err(ArtifactError::Changed);
    }
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let text = String::from_utf8(bytes).map_err(|_| ArtifactError::Unsupported)?;
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err(ArtifactError::Unsupported);
    }
    Ok(ArtifactPreview {
        byte_len: after.len(),
        text,
        sha256,
        entry_id: entry_id.into(),
        path: artifact.path.clone(),
    })
}

fn workspace_path(path: &Path) -> Result<PathBuf, ArtifactError> {
    #[cfg(windows)]
    if let Some(Component::Prefix(prefix)) = path.components().next() {
        if let std::path::Prefix::VerbatimDisk(drive) = prefix.kind() {
            let mut normalized = PathBuf::from(format!("{}:\\", char::from(drive)));
            for component in path.components().skip(2) {
                normalized.push(component);
            }
            validate_path(&normalized)?;
            return Ok(normalized);
        }
    }
    validate_path(path)?;
    Ok(path.to_owned())
}
fn target_path(workspace: &Path, recorded: &str) -> Result<PathBuf, ArtifactError> {
    if !workspace.is_absolute() {
        return Err(ArtifactError::OutsideWorkspace);
    }
    validate_path(workspace)?;
    if recorded.starts_with("\\\\") && !recorded.starts_with("\\\\?\\") {
        return Err(ArtifactError::OutsideWorkspace);
    }
    let path = workspace_path(Path::new(recorded))?;
    validate_path(&path)?;
    #[cfg(windows)]
    let path = if path.is_absolute() {
        let root_parts: Vec<_> = workspace.components().collect();
        let parts: Vec<_> = path.components().collect();
        if parts.len() < root_parts.len()
            || !parts.iter().zip(&root_parts).all(|(a, b)| {
                a.as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
            })
        {
            return Err(ArtifactError::OutsideWorkspace);
        }
        let mut normalized = workspace.to_owned();
        for component in &parts[root_parts.len()..] {
            normalized.push(component);
        }
        normalized
    } else {
        path
    };
    let target = if path.is_absolute() {
        path
    } else {
        workspace.join(path)
    };
    if !target.starts_with(workspace) {
        return Err(ArtifactError::OutsideWorkspace);
    }
    Ok(target)
}
fn validate_path(path: &Path) -> Result<(), ArtifactError> {
    for component in path.components() {
        match component {
            Component::ParentDir => return Err(ArtifactError::OutsideWorkspace),
            Component::Prefix(prefix) => {
                #[cfg(windows)]
                if !matches!(prefix.kind(), std::path::Prefix::Disk(_)) {
                    return Err(ArtifactError::OutsideWorkspace);
                }
                #[cfg(not(windows))]
                let _ = prefix;
            }
            Component::Normal(name) => {
                let name = name.to_str().ok_or(ArtifactError::OutsideWorkspace)?;
                if name.contains(':')
                    || name.ends_with(['.', ' '])
                    || name.chars().any(char::is_control)
                {
                    return Err(ArtifactError::OutsideWorkspace);
                }
                // Reject DOS devices even with an extension, on every platform.
                let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
                if matches!(
                    stem.as_str(),
                    "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
                ) || ((stem.starts_with("COM") || stem.starts_with("LPT"))
                    && stem.len() == 4
                    && stem.as_bytes()[3].is_ascii_digit())
                {
                    return Err(ArtifactError::OutsideWorkspace);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(windows)]
fn open_checked(workspace: &Path, target: &Path) -> Result<(File, Vec<File>), ArtifactError> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_READ,
    };
    let mut guards = Vec::new();
    let mut path = PathBuf::new();
    let mut root_final = None;
    for component in target.components() {
        path.push(component);
        if !path.has_root() {
            continue;
        }
        let final_component = path == target;
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
        if !final_component {
            options.access_mode(FILE_READ_ATTRIBUTES);
        }
        let file = options.open(&path).map_err(io)?;
        let metadata = file.metadata().map_err(io)?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(ArtifactError::Link);
        }
        let final_path = handle_path(&file)?;
        if path == workspace {
            root_final = Some(final_path.clone());
        }
        if final_component {
            let root = root_final.ok_or(ArtifactError::OutsideWorkspace)?;
            if !final_path.starts_with(root) {
                return Err(ArtifactError::OutsideWorkspace);
            }
            return Ok((file, guards));
        }
        if !metadata.is_dir() {
            return Err(ArtifactError::Missing);
        }
        // Deny write/delete sharing and retain all ancestors through the read.
        // This pins the checked directory chain against rename/reparse changes.
        guards.push(file);
    }
    Err(ArtifactError::OutsideWorkspace)
}
#[cfg(windows)]
fn handle_path(file: &File) -> Result<PathBuf, ArtifactError> {
    use std::os::windows::{ffi::OsStringExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;
    let mut buffer = vec![0u16; 32768];
    // SAFETY: the handle belongs to a live File and buffer has the stated size.
    let n = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            0,
        )
    };
    if n == 0 || n as usize >= buffer.len() {
        return Err(ArtifactError::Io);
    }
    Ok(std::ffi::OsString::from_wide(&buffer[..n as usize]).into())
}

#[cfg(target_os = "linux")]
fn open_checked(workspace: &Path, target: &Path) -> Result<(File, Vec<File>), ArtifactError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut path = PathBuf::new();
    for component in target.components() {
        path.push(component);
        let metadata = std::fs::symlink_metadata(&path).map_err(io)?;
        if metadata.file_type().is_symlink() {
            return Err(ArtifactError::Link);
        }
        if !metadata.is_file() && !metadata.is_dir() {
            return Err(ArtifactError::Unsupported);
        }
    }
    let root = workspace.canonicalize().map_err(io)?;
    // Linux O_NONBLOCK ABI values checked against the installed libc 0.2.190.
    // A FIFO replacement between lstat and open must not stall a worker.
    #[cfg(any(
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "mips32r6",
        target_arch = "mips64r6"
    ))]
    const NONBLOCK: i32 = 128;
    #[cfg(any(target_arch = "sparc", target_arch = "sparc64"))]
    const NONBLOCK: i32 = 0x4000;
    #[cfg(not(any(
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "mips32r6",
        target_arch = "mips64r6",
        target_arch = "sparc",
        target_arch = "sparc64"
    )))]
    const NONBLOCK: i32 = 2048;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(NONBLOCK)
        .open(target)
        .map_err(io)?;
    {
        use std::os::fd::AsRawFd;
        let opened =
            std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).map_err(io)?;
        if !opened.starts_with(root) {
            return Err(ArtifactError::OutsideWorkspace);
        }
    }
    Ok((file, Vec::new()))
}

#[cfg(not(any(windows, target_os = "linux")))]
fn open_checked(_workspace: &Path, _target: &Path) -> Result<(File, Vec<File>), ArtifactError> {
    // No portable handle-based pathname verification is available here.
    Err(ArtifactError::Unsupported)
}
