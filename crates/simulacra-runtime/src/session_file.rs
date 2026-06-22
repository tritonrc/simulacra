//! File-backed session storage implementation.
//!
//! Stores sessions as JSON files at `<base_dir>/<session_id>/checkpoint.json`.
//! Writes are atomic (write to `.tmp` then rename) to prevent corruption.

use crate::{RuntimeError, Session, SessionStorage};
use std::path::PathBuf;

/// File-backed session storage.
///
/// Each session is stored as a JSON file at
/// `<base_dir>/<session_id>/checkpoint.json`. Directories are created
/// on demand and writes use a temporary file + rename for atomicity.
#[derive(Debug)]
pub struct FileSessionStorage {
    base_dir: PathBuf,
}

impl FileSessionStorage {
    /// Create a new file-backed session store rooted at `base_dir`.
    ///
    /// The directory is created lazily on first write, not at construction time.
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// Return the path to the checkpoint file for a given session id.
    ///
    /// The resulting path is validated to stay within `base_dir` to prevent
    /// path traversal attacks (e.g. `../../outside` or absolute session ids).
    fn checkpoint_path(&self, session_id: &str) -> Result<PathBuf, RuntimeError> {
        let joined = self.base_dir.join(session_id).join("checkpoint.json");

        // Normalize the path by resolving `.` and `..` components lexically.
        // We cannot use `canonicalize()` because the path may not exist yet
        // (first write). Instead we normalize both paths and compare prefixes.
        let normalized = normalize_path(&joined);
        let base_normalized = normalize_path(&self.base_dir);

        if !normalized.starts_with(&base_normalized) {
            return Err(RuntimeError::Session(format!(
                "session id escapes base directory: {session_id:?}"
            )));
        }

        Ok(joined)
    }
}

/// Lexically normalize a path by resolving `.` and `..` components without
/// touching the filesystem. This is necessary because the path may not exist
/// yet (we create directories on demand).
fn normalize_path(path: &std::path::Path) -> PathBuf {
    use std::path::Component;
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            other => normalized.push(other),
        }
    }
    normalized
}

impl SessionStorage for FileSessionStorage {
    fn save(&self, session: &Session) -> Result<(), RuntimeError> {
        let checkpoint_path = self.checkpoint_path(&session.id)?;
        let dir = checkpoint_path
            .parent()
            .ok_or_else(|| RuntimeError::Session("invalid checkpoint path".into()))?;

        std::fs::create_dir_all(dir)
            .map_err(|e| RuntimeError::Session(format!("failed to create session dir: {e}")))?;

        let json = serde_json::to_string_pretty(session)
            .map_err(|e| RuntimeError::Session(format!("failed to serialize session: {e}")))?;

        // Atomic write: write to a temporary file in the same directory, then
        // rename. This ensures readers never see a partially-written file.
        let tmp_path = checkpoint_path.with_extension("tmp");
        std::fs::write(&tmp_path, json.as_bytes())
            .map_err(|e| RuntimeError::Session(format!("failed to write tmp file: {e}")))?;

        std::fs::rename(&tmp_path, &checkpoint_path)
            .map_err(|e| RuntimeError::Session(format!("failed to rename tmp file: {e}")))?;

        Ok(())
    }

    fn load(&self, id: &str) -> Result<Option<Session>, RuntimeError> {
        let checkpoint_path = self.checkpoint_path(id)?;

        if !checkpoint_path.exists() {
            return Ok(None);
        }

        let data = std::fs::read_to_string(&checkpoint_path)
            .map_err(|e| RuntimeError::Session(format!("failed to read checkpoint: {e}")))?;

        let session: Session = serde_json::from_str(&data)
            .map_err(|e| RuntimeError::Session(format!("failed to deserialize session: {e}")))?;

        Ok(Some(session))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use simulacra_types::{AgentId, Message, Role};

    fn make_session(id: &str) -> Session {
        Session {
            id: id.into(),
            agent_id: AgentId("test-agent".into()),
            messages: vec![Message {
                role: Role::User,
                content: "hello".into(),
                tool_calls: vec![],
                tool_call_id: None,
            }],
            vfs_snapshot: None,
            created_at: 1000,
            used_tokens: 0,
            used_turns: 0,
        }
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "simulacra-file-session-test-roundtrip-{}",
            std::process::id()
        ));
        let storage = FileSessionStorage::new(dir.clone());
        let session = make_session("sess-1");

        storage.save(&session).unwrap();
        let loaded = storage
            .load("sess-1")
            .unwrap()
            .expect("session should exist");

        assert_eq!(loaded.id, "sess-1");
        assert_eq!(loaded.agent_id.0, "test-agent");
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].content, "hello");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_nonexistent_returns_none() {
        let dir = std::env::temp_dir().join(format!(
            "simulacra-file-session-test-none-{}",
            std::process::id()
        ));
        let storage = FileSessionStorage::new(dir.clone());

        assert!(storage.load("nonexistent").unwrap().is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_overwrites_existing() {
        let dir = std::env::temp_dir().join(format!(
            "simulacra-file-session-test-overwrite-{}",
            std::process::id()
        ));
        let storage = FileSessionStorage::new(dir.clone());

        let mut session = make_session("sess-2");
        storage.save(&session).unwrap();

        session.messages.push(Message {
            role: Role::Assistant,
            content: "updated".into(),
            tool_calls: vec![],
            tool_call_id: None,
        });
        storage.save(&session).unwrap();

        let loaded = storage.load("sess-2").unwrap().unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[1].content, "updated");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_leaves_no_tmp_file() {
        let dir = std::env::temp_dir().join(format!(
            "simulacra-file-session-test-atomic-{}",
            std::process::id()
        ));
        let storage = FileSessionStorage::new(dir.clone());
        let session = make_session("sess-atomic");

        storage.save(&session).unwrap();

        let tmp_path = dir.join("sess-atomic").join("checkpoint.tmp");
        assert!(!tmp_path.exists(), "tmp file should not remain after save");

        let checkpoint_path = dir.join("sess-atomic").join("checkpoint.json");
        assert!(checkpoint_path.exists(), "checkpoint.json should exist");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sessions_are_isolated_by_id() {
        let dir = std::env::temp_dir().join(format!(
            "simulacra-file-session-test-isolation-{}",
            std::process::id()
        ));
        let storage = FileSessionStorage::new(dir.clone());

        storage.save(&make_session("a")).unwrap();
        storage.save(&make_session("b")).unwrap();

        let a = storage.load("a").unwrap().unwrap();
        let b = storage.load("b").unwrap().unwrap();
        assert_eq!(a.id, "a");
        assert_eq!(b.id, "b");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_path_traversal_via_dotdot() {
        let dir = std::env::temp_dir().join(format!(
            "simulacra-file-session-test-traversal-{}",
            std::process::id()
        ));
        let storage = FileSessionStorage::new(dir.clone());

        let session = make_session("../../outside");
        let err = storage.save(&session).unwrap_err();
        assert!(
            format!("{err}").contains("escapes base directory"),
            "expected path traversal error, got: {err}"
        );

        let err = storage.load("../../outside").unwrap_err();
        assert!(
            format!("{err}").contains("escapes base directory"),
            "expected path traversal error, got: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_absolute_session_id() {
        let dir = std::env::temp_dir().join(format!(
            "simulacra-file-session-test-absolute-{}",
            std::process::id()
        ));
        let storage = FileSessionStorage::new(dir.clone());

        let session = make_session("/tmp/evil");
        let err = storage.save(&session).unwrap_err();
        assert!(
            format!("{err}").contains("escapes base directory"),
            "expected path traversal error, got: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistence_across_instances() {
        let dir = std::env::temp_dir().join(format!(
            "simulacra-file-session-test-persist-{}",
            std::process::id()
        ));

        {
            let storage = FileSessionStorage::new(dir.clone());
            storage.save(&make_session("persist-1")).unwrap();
        }

        {
            let storage = FileSessionStorage::new(dir.clone());
            let loaded = storage.load("persist-1").unwrap().expect("should persist");
            assert_eq!(loaded.id, "persist-1");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
