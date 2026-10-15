use crate::compaction::NativeCompaction;
use crate::legacy_session::LegacyImportMetadata;
use crate::session_tree::SessionEntry;
use agent_core::types::{timestamp, Context};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SESSION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "StoredSession")]
pub struct Session {
    pub id: String,
    pub name: String,
    pub updated_at: u64,
    pub context: Context,
    pub workspace: PathBuf,
    #[serde(default)]
    pub imported: Option<LegacyImportMetadata>,
    #[serde(default)]
    pub compactions: Vec<NativeCompaction>,
    #[serde(default)]
    pub entries: Vec<SessionEntry>,
    #[serde(default)]
    pub leaf_id: Option<String>,
    #[serde(default)]
    pub tree_version: u32,
    #[serde(default, flatten)]
    pub metadata: Map<String, Value>,
}

// Decode typed model messages while retaining fields unknown to this runtime.
// This matters for old snapshots carrying provider/extension payloads.
#[derive(Deserialize)]
struct StoredSession {
    id: String,
    name: String,
    updated_at: u64,
    context: Value,
    workspace: PathBuf,
    #[serde(default)]
    imported: Option<LegacyImportMetadata>,
    #[serde(default)]
    compactions: Vec<Value>,
    #[serde(default)]
    entries: Vec<SessionEntry>,
    #[serde(default)]
    leaf_id: Option<String>,
    #[serde(default)]
    tree_version: u32,
    #[serde(default, flatten)]
    metadata: Map<String, Value>,
}

impl TryFrom<StoredSession> for Session {
    type Error = String;
    fn try_from(stored: StoredSession) -> Result<Self, String> {
        let context: Context =
            serde_json::from_value(stored.context.clone()).map_err(|error| error.to_string())?;
        let compactions: Vec<NativeCompaction> =
            serde_json::from_value(Value::Array(stored.compactions.clone()))
                .map_err(|error| error.to_string())?;
        let mut metadata = stored.metadata;
        if serde_json::to_value(&context).map_err(|error| error.to_string())? != stored.context {
            metadata
                .entry("preserved_context".to_owned())
                .or_insert(stored.context);
        }
        if serde_json::to_value(&compactions).map_err(|error| error.to_string())?
            != Value::Array(stored.compactions.clone())
        {
            metadata
                .entry("preserved_compactions".to_owned())
                .or_insert(Value::Array(stored.compactions));
        }
        Ok(Self {
            id: stored.id,
            name: stored.name,
            updated_at: stored.updated_at,
            context,
            workspace: stored.workspace,
            imported: stored.imported,
            compactions,
            entries: stored.entries,
            leaf_id: stored.leaf_id,
            tree_version: stored.tree_version,
            metadata,
        })
    }
}

impl Default for Session {
    fn default() -> Self {
        Self {
            id: format!(
                "{}-{}-{}",
                timestamp(),
                std::process::id(),
                SESSION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ),
            name: "新会话".into(),
            updated_at: timestamp(),
            workspace: std::env::current_dir().unwrap_or_default(),
            imported: None,
            compactions: vec![],
            entries: vec![],
            leaf_id: None,
            tree_version: 0,
            metadata: Map::new(),
            context: Context {
                system_prompt: "你是 Agent Core 编程助手。先阅读项目，再做最小修改。工具调用使用提供的 JSON schema。".into(),
                messages: vec![],
            },
        }
    }
}

#[derive(Clone)]
pub struct SessionStore {
    directory: PathBuf,
}

#[derive(Debug, Default)]
pub struct SessionLoad {
    pub sessions: Vec<Session>,
    pub diagnostics: Vec<String>,
}

impl SessionStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }
    pub fn default_directory() -> PathBuf {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(std::env::temp_dir)
            .join("AgentCore")
            .join("sessions")
    }
    pub fn path(&self, id: &str) -> Result<PathBuf, String> {
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err("Invalid session id".into());
        }
        Ok(self.directory.join(format!("{id}.json")))
    }
    pub fn save(&self, session: &Session) -> Result<(), String> {
        let mut snapshot = session.clone();
        snapshot.ensure_tree()?;
        fs::create_dir_all(&self.directory).map_err(|e| e.to_string())?;
        let path = self.path(&session.id)?;
        let temporary = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(&snapshot).map_err(|e| e.to_string())?;
        fs::write(&temporary, bytes).map_err(|e| e.to_string())?;
        fs::rename(&temporary, &path).map_err(|e| e.to_string())
    }
    pub fn list(&self) -> Result<Vec<Session>, String> {
        self.load_with_diagnostics().map(|loaded| loaded.sessions)
    }
    pub fn load_with_diagnostics(&self) -> Result<SessionLoad, String> {
        if !self.directory.exists() {
            return Ok(SessionLoad::default());
        }
        let mut loaded = SessionLoad::default();
        for entry in fs::read_dir(&self.directory).map_err(|e| e.to_string())? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    loaded.diagnostics.push(format!(
                        "{}: Session entry: {error}",
                        self.directory.display()
                    ));
                    continue;
                }
            };
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                let decoded = fs::read_to_string(&path)
                    .map_err(|e| format!("Session read: {e}"))
                    .and_then(|text| {
                        serde_json::from_str::<Session>(&text)
                            .map_err(|e| format!("Session decode: {e}"))
                    })
                    .and_then(|mut session| {
                        self.path(&session.id)?;
                        session.ensure_tree()?;
                        Ok(session)
                    });
                match decoded {
                    Ok(session) => loaded.sessions.push(session),
                    Err(error) => loaded
                        .diagnostics
                        .push(format!("{}: {error}", path.display())),
                }
            }
        }
        loaded
            .sessions
            .sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        Ok(loaded)
    }
    pub fn export_jsonl(session: &Session, path: &Path) -> Result<(), String> {
        let mut text = String::new();
        for message in &session.context.messages {
            text.push_str(&serde_json::to_string(message).map_err(|e| e.to_string())?);
            text.push('\n');
        }
        fs::write(path, text).map_err(|e| e.to_string())
    }
    pub fn export_branch_jsonl(session: &Session, path: &Path) -> Result<(), String> {
        crate::session_tree::export_branch_jsonl(session, path)
    }
    pub fn export_html(session: &Session, path: &Path) -> Result<(), String> {
        let html = crate::html_export::generate_session_html(session)?;
        fs::write(path, html).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::types::Message;
    #[test]
    fn session_round_trip_update_and_export() {
        let dir =
            std::env::temp_dir().join(format!("agent-rust-session-{}", Session::default().id));
        let store = SessionStore::new(&dir);
        let mut session = Session::default();
        session.context.messages.push(Message::user("中文消息"));
        store.save(&session).unwrap();
        session.name = "renamed".into();
        store.save(&session).unwrap();
        let loaded = store.list().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "renamed");
        assert_eq!(loaded[0].context.messages[0].text(), "中文消息");
        let export = dir.join("export.jsonl");
        SessionStore::export_jsonl(&session, &export).unwrap();
        assert_eq!(fs::read_to_string(&export).unwrap().lines().count(), 1);
        assert!(store.path("../escape").is_err());
        fs::remove_file(export).unwrap();
        fs::remove_file(store.path(&session.id).unwrap()).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn session_ids_are_unique_across_concurrent_creation() {
        let workers: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| (0..256).map(|_| Session::default().id).collect::<Vec<_>>())
            })
            .collect();
        let ids: std::collections::HashSet<_> = workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(ids.len(), 2048);
    }

    #[test]
    fn restores_each_sessions_workspace_without_cross_project_reuse() {
        let dir =
            std::env::temp_dir().join(format!("agent-rust-workspaces-{}", Session::default().id));
        let project_a = dir.join("project-a");
        let project_b = dir.join("project-b");
        fs::create_dir_all(&project_a).unwrap();
        fs::create_dir(&project_b).unwrap();
        let store = SessionStore::new(dir.join("sessions"));
        let session_a = Session {
            name: "project A".into(),
            workspace: fs::canonicalize(&project_a).unwrap(),
            updated_at: 1,
            ..Session::default()
        };
        let session_b = Session {
            name: "project B".into(),
            workspace: fs::canonicalize(&project_b).unwrap(),
            updated_at: 2,
            ..Session::default()
        };
        store.save(&session_a).unwrap();
        store.save(&session_b).unwrap();
        let restored = SessionStore::new(dir.join("sessions"))
            .load_with_diagnostics()
            .unwrap();
        assert!(restored.diagnostics.is_empty());
        assert_eq!(restored.sessions.len(), 2);
        assert_eq!(restored.sessions[0].id, session_b.id);
        assert_eq!(restored.sessions[0].workspace, session_b.workspace);
        assert_eq!(restored.sessions[1].id, session_a.id);
        assert_eq!(restored.sessions[1].workspace, session_a.workspace);
        assert_ne!(
            restored.sessions[0].workspace,
            restored.sessions[1].workspace
        );
        fs::remove_file(store.path(&session_a.id).unwrap()).unwrap();
        fs::remove_file(store.path(&session_b.id).unwrap()).unwrap();
        fs::remove_dir(dir.join("sessions")).unwrap();
        fs::remove_dir(project_a).unwrap();
        fs::remove_dir(project_b).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn loads_valid_sessions_and_preserves_corrupt_files_with_diagnostics() {
        let dir =
            std::env::temp_dir().join(format!("agent-rust-corrupt-{}", Session::default().id));
        let store = SessionStore::new(&dir);
        let session = Session::default();
        store.save(&session).unwrap();
        let corrupt = dir.join("corrupt.json");
        let original_bytes = b"{ invalid JSON";
        fs::write(&corrupt, original_bytes).unwrap();
        let invalid_id = dir.join("invalid-id.json");
        let invalid_session = Session {
            id: "../escape".into(),
            ..Session::default()
        };
        let invalid_bytes = serde_json::to_vec(&invalid_session).unwrap();
        fs::write(&invalid_id, &invalid_bytes).unwrap();
        let temporary = dir.join("ignored.json.tmp");
        fs::write(&temporary, b"unfinished write").unwrap();
        let loaded = store.load_with_diagnostics().unwrap();
        assert_eq!(loaded.sessions.len(), 1);
        assert_eq!(loaded.sessions[0].id, session.id);
        assert_eq!(loaded.diagnostics.len(), 2);
        assert!(loaded.diagnostics.iter().any(|diagnostic| {
            diagnostic.contains("corrupt.json") && diagnostic.contains("Session decode")
        }));
        assert!(loaded.diagnostics.iter().any(|diagnostic| {
            diagnostic.contains("invalid-id.json") && diagnostic.contains("Invalid session id")
        }));
        assert_eq!(store.list().unwrap().len(), 1);
        assert_eq!(fs::read(&corrupt).unwrap(), original_bytes);
        assert_eq!(fs::read(&invalid_id).unwrap(), invalid_bytes);
        assert_eq!(fs::read(&temporary).unwrap(), b"unfinished write");
        fs::remove_file(store.path(&session.id).unwrap()).unwrap();
        fs::remove_file(corrupt).unwrap();
        fs::remove_file(invalid_id).unwrap();
        fs::remove_file(temporary).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn reports_no_sessions_for_missing_directory() {
        let dir =
            std::env::temp_dir().join(format!("agent-rust-missing-{}", Session::default().id));
        let store = SessionStore::new(dir);
        let loaded = store.load_with_diagnostics().unwrap();
        assert!(loaded.sessions.is_empty());
        assert!(loaded.diagnostics.is_empty());
    }

    #[test]
    fn original_native_sessions_load_with_empty_migration_and_archive_fields() {
        let original = Session::default();
        let mut stored = serde_json::to_value(&original).unwrap();
        stored.as_object_mut().unwrap().remove("imported");
        stored.as_object_mut().unwrap().remove("compactions");
        let loaded: Session = serde_json::from_value(stored).unwrap();
        assert_eq!(loaded.id, original.id);
        assert_eq!(loaded.workspace, original.workspace);
        assert_eq!(loaded.context, original.context);
        assert!(loaded.imported.is_none());
        assert!(loaded.compactions.is_empty());
    }
}
