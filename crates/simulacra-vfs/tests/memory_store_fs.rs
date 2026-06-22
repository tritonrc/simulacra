use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rust_decimal::Decimal;
use simulacra_memory::{MemoryError, MemoryStore, SqliteMemoryStore};
use simulacra_types::{
    ArtifactEntry, ArtifactError, ArtifactStore, CapabilityToken, MemoryCapability, MemoryPath,
    NetworkPermission, PathPattern, ResourceBudget, TenantId, VfsError, VirtualFs,
};
use simulacra_vfs::{
    HookLister, IntegrationLister, MailboxFs, MemoryFs, MemoryStoreFs, ProcFs, ProcState,
    ServiceFs, ToolLister,
};

fn tenant() -> TenantId {
    TenantId::parse("tenant-a").unwrap()
}

fn memory_path(path: &str) -> MemoryPath {
    MemoryPath::parse(path).unwrap()
}

fn memory_capability(search_scopes: &[&str], write_scopes: &[&str]) -> MemoryCapability {
    MemoryCapability {
        enabled: true,
        search_scopes: search_scopes
            .iter()
            .map(|scope| memory_path(scope))
            .collect(),
        write_scopes: write_scopes
            .iter()
            .map(|scope| memory_path(scope))
            .collect(),
    }
}

#[derive(Default)]
struct StubArtifactStore {
    entries: Mutex<HashMap<(String, String), Vec<u8>>>,
}

impl ArtifactStore for StubArtifactStore {
    fn put(
        &self,
        task_id: &str,
        _tenant: &str,
        path: &str,
        data: &[u8],
    ) -> Result<(), ArtifactError> {
        self.entries
            .lock()
            .unwrap()
            .insert((task_id.to_string(), path.to_string()), data.to_vec());
        Ok(())
    }

    fn get(&self, _tenant: &str, task_id: &str, path: &str) -> Result<Vec<u8>, ArtifactError> {
        self.entries
            .lock()
            .unwrap()
            .get(&(task_id.to_string(), path.to_string()))
            .cloned()
            .ok_or_else(|| ArtifactError::NotFound(path.to_string()))
    }

    fn list(&self, _tenant: &str, task_id: &str) -> Result<Vec<ArtifactEntry>, ArtifactError> {
        let mut entries = self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|((stored_task, _), _)| stored_task == task_id)
            .map(|((_, path), data)| ArtifactEntry {
                path: path.clone(),
                size: data.len() as u64,
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(entries)
    }

    fn delete_task(&self, _tenant: &str, task_id: &str) -> Result<(), ArtifactError> {
        self.entries
            .lock()
            .unwrap()
            .retain(|(stored_task, _), _| stored_task != task_id);
        Ok(())
    }
}

struct StubToolLister;

impl ToolLister for StubToolLister {
    fn tool_names(&self) -> Vec<String> {
        vec!["semantic_search".to_string()]
    }

    fn tool_json(&self, name: &str) -> Option<String> {
        Some(format!(r#"{{"name":"{name}"}}"#))
    }
}

struct StubHookLister;

impl HookLister for StubHookLister {
    fn hook_names(&self, operation: &str) -> Vec<String> {
        vec![format!("{operation}-hook")]
    }
}

struct StubIntegrationLister;

impl IntegrationLister for StubIntegrationLister {
    fn integration_names(&self) -> Vec<String> {
        vec!["policies".to_string()]
    }

    fn integration_metadata(&self, name: &str) -> Option<String> {
        (name == "policies").then(|| r#"{"name":"policies"}"#.to_string())
    }

    fn integration_readme(&self, name: &str) -> Option<String> {
        (name == "policies").then(|| "# policies".to_string())
    }

    fn integration_skill_names(&self, name: &str) -> Vec<String> {
        if name == "policies" {
            vec!["lookup".to_string()]
        } else {
            Vec::new()
        }
    }
}

fn proc_state(
    memory: MemoryCapability,
    paths_read: &[&str],
    paths_write: &[&str],
) -> Arc<ProcState> {
    Arc::new(ProcState {
        agent_id: "agent-1".to_string(),
        agent_name: "worker".to_string(),
        model: "gpt-test".to_string(),
        parent_id: None,
        budget: Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0))),
        capabilities: CapabilityToken {
            network: vec![NetworkPermission("https://example.com".to_string())],
            mcp_tools: vec!["memory_search".to_string()],
            shell: false,
            javascript: false,
            python: false,
            paths_write: paths_write
                .iter()
                .map(|pattern| PathPattern((*pattern).to_string()))
                .collect(),
            paths_read: paths_read
                .iter()
                .map(|pattern| PathPattern((*pattern).to_string()))
                .collect(),
            spawn_types: Vec::new(),
            skill_patterns: Vec::new(),
            memory,
        },
        tools: Arc::new(StubToolLister),
        session_id: "session-1".to_string(),
        session_start: Instant::now(),
        journal_entries: Arc::new(AtomicU64::new(0)),
        hooks: Arc::new(StubHookLister),
        turn: Arc::new(AtomicU64::new(1)),
    })
}

fn build_stack(
    store: Arc<dyn MemoryStore>,
    memory: Option<MemoryCapability>,
    paths_read: &[&str],
    paths_write: &[&str],
) -> Arc<dyn VirtualFs> {
    let inner: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let mailbox_store: Arc<dyn ArtifactStore> = Arc::new(StubArtifactStore::default());
    let mailbox: Arc<dyn VirtualFs> = Arc::new(MailboxFs::new(
        inner,
        "task-1".to_string(),
        tenant().to_string(),
        mailbox_store,
    ));
    let memory_layer: Arc<dyn VirtualFs> = match memory.clone() {
        Some(capability) => Arc::new(MemoryStoreFs::new(mailbox, tenant(), store, capability)),
        None => mailbox,
    };
    let service: Arc<dyn VirtualFs> = Arc::new(ServiceFs::new(
        memory_layer,
        Arc::new(StubIntegrationLister),
    ));
    Arc::new(ProcFs::new(
        service,
        proc_state(memory.unwrap_or_default(), paths_read, paths_write),
    ))
}

#[test]
fn conditional_install_gate_returns_not_found_from_the_inner_vfs_when_memory_is_disabled() {
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
    let vfs = build_stack(store, None, &["/**"], &["/**"]);

    let err = vfs.read("/var/memory/self/x.md").unwrap_err();

    assert!(matches!(err, VfsError::NotFound(path) if path == "/var/memory/self/x.md"));
}

#[test]
fn installed_layer_enforces_self_gating_and_mnt_read_rules() {
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
    let self_path = memory_path("/var/memory/self/note.md");
    let users_path = memory_path("/var/memory/users/brian.md");
    let mnt_path = memory_path("/mnt/policies/hr.md");
    store.put(&tenant(), &self_path, b"self note").unwrap();
    store.put(&tenant(), &users_path, b"user note").unwrap();
    store.put(&tenant(), &mnt_path, b"hr policy").unwrap();

    let vfs = build_stack(
        store,
        Some(memory_capability(
            &["/var/memory/self", "/mnt"],
            &["/var/memory/self"],
        )),
        &["/**"],
        &["/**"],
    );

    assert_eq!(vfs.read(self_path.as_str()).unwrap(), b"self note");
    assert!(matches!(
        vfs.read(users_path.as_str()),
        Err(VfsError::PermissionDenied(_))
    ));
    assert!(matches!(
        vfs.write(users_path.as_str(), b"blocked"),
        Err(VfsError::PermissionDenied(_))
    ));
    assert!(matches!(
        vfs.write(mnt_path.as_str(), b"blocked"),
        Err(VfsError::PermissionDenied(_))
    ));
    assert_eq!(vfs.read(mnt_path.as_str()).unwrap(), b"hr policy");
}

#[test]
fn memory_paths_outside_search_scopes_are_rejected_even_when_generic_paths_read_is_wildcard() {
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
    let outside_path = memory_path("/var/memory/users/brian.md");
    store.put(&tenant(), &outside_path, b"private").unwrap();
    let vfs = build_stack(
        store,
        Some(memory_capability(
            &["/var/memory/self"],
            &["/var/memory/self"],
        )),
        &["/**"],
        &["/workspace/**"],
    );

    let err = vfs.read(outside_path.as_str()).unwrap_err();

    assert!(matches!(err, VfsError::PermissionDenied(_)));
}

#[test]
fn list_dir_returns_only_immediate_children_for_memory_prefixes() {
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
    for path in [
        "/var/memory/self/a.md",
        "/var/memory/self/sub/b.md",
        "/var/memory/self/sub/c.md",
    ] {
        store
            .put(&tenant(), &memory_path(path), path.as_bytes())
            .unwrap();
    }
    let vfs = build_stack(
        store,
        Some(memory_capability(
            &["/var/memory/self"],
            &["/var/memory/self"],
        )),
        &[],
        &[],
    );

    let entries = vfs.list_dir("/var/memory/self").unwrap();

    assert_eq!(entries, vec!["a.md", "sub"]);
}

#[test]
fn exists_returns_false_for_missing_in_scope_paths_instead_of_permission_denied() {
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
    let vfs = build_stack(
        store,
        Some(memory_capability(
            &["/var/memory/self"],
            &["/var/memory/self"],
        )),
        &[],
        &[],
    );

    assert!(!vfs.exists("/var/memory/self/missing.md"));
}

#[test]
fn mkdir_is_a_no_op_success_inside_write_scopes_and_denied_outside_them() {
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
    let vfs = build_stack(
        store,
        Some(memory_capability(
            &["/var/memory/self"],
            &["/var/memory/self"],
        )),
        &[],
        &[],
    );

    assert!(vfs.mkdir("/var/memory/self/projects").is_ok());
    assert!(matches!(
        vfs.mkdir("/var/memory/users/brian"),
        Err(VfsError::PermissionDenied(_))
    ));
}

#[test]
fn remove_deletes_in_scope_paths_and_rejects_out_of_scope_paths() {
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
    let inside = memory_path("/var/memory/self/delete-me.md");
    let outside = memory_path("/var/memory/users/keep-me.md");
    store.put(&tenant(), &inside, b"bye").unwrap();
    store.put(&tenant(), &outside, b"stay").unwrap();
    let vfs = build_stack(
        store.clone(),
        Some(memory_capability(
            &["/var/memory/self"],
            &["/var/memory/self"],
        )),
        &[],
        &[],
    );

    vfs.remove(inside.as_str()).unwrap();

    assert!(matches!(
        store.get(&tenant(), &inside),
        Err(MemoryError::NotFound(path)) if path == inside.as_str()
    ));
    assert!(matches!(
        vfs.remove(outside.as_str()),
        Err(VfsError::PermissionDenied(_))
    ));
}

#[test]
fn metadata_reports_files_and_directories_from_the_store() {
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
    let file_path = memory_path("/var/memory/self/info.md");
    let dir_path = "/var/memory/self/subdir";
    store.put(&tenant(), &file_path, b"abcde").unwrap();
    store
        .put(
            &tenant(),
            &memory_path("/var/memory/self/subdir/child.md"),
            b"child",
        )
        .unwrap();
    let vfs = build_stack(
        store,
        Some(memory_capability(
            &["/var/memory/self"],
            &["/var/memory/self"],
        )),
        &[],
        &[],
    );

    let file_meta = vfs.metadata(file_path.as_str()).unwrap();
    let dir_meta = vfs.metadata(dir_path).unwrap();

    assert!(file_meta.is_file);
    assert!(!file_meta.is_dir);
    assert_eq!(file_meta.size, 5);
    assert!(!dir_meta.is_file);
    assert!(dir_meta.is_dir);
}

#[test]
fn stack_composition_keeps_proc_svc_workspace_and_memory_routes_working() {
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
    let memory_file = memory_path("/var/memory/self/note.md");
    store.put(&tenant(), &memory_file, b"memory data").unwrap();
    let vfs = build_stack(
        store,
        Some(memory_capability(
            &["/var/memory/self", "/mnt"],
            &["/var/memory/self"],
        )),
        &["/**"],
        &["/**"],
    );

    vfs.write("/workspace/local.txt", b"workspace data")
        .unwrap();

    assert_eq!(vfs.read("/proc/agent/id").unwrap(), b"agent-1");
    assert_eq!(vfs.read("/svc/policies/README.md").unwrap(), b"# policies");
    assert_eq!(vfs.read("/workspace/local.txt").unwrap(), b"workspace data");
    assert_eq!(vfs.read(memory_file.as_str()).unwrap(), b"memory data");
}

#[test]
fn snapshot_and_restore_delegate_to_the_inner_vfs_while_memory_persists_via_the_store() {
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
    let memory_file = "/var/memory/self/note.md";
    let vfs = build_stack(
        store,
        Some(memory_capability(
            &["/var/memory/self"],
            &["/var/memory/self"],
        )),
        &[],
        &[],
    );

    vfs.write("/workspace/state.txt", b"before").unwrap();
    vfs.write(memory_file, b"memory before").unwrap();
    let snapshot = vfs.snapshot().unwrap();

    vfs.write("/workspace/state.txt", b"after").unwrap();
    vfs.write(memory_file, b"memory after").unwrap();
    vfs.restore(&snapshot).unwrap();

    assert_eq!(vfs.read("/workspace/state.txt").unwrap(), b"before");
    assert_eq!(vfs.read(memory_file).unwrap(), b"memory after");
}

#[test]
fn traversal_like_memory_paths_are_rejected_with_permission_denied() {
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
    let vfs = build_stack(
        store,
        Some(memory_capability(
            &["/var/memory/self"],
            &["/var/memory/self"],
        )),
        &[],
        &[],
    );

    let err = vfs.read("/var/memory/self/../users/x.md").unwrap_err();

    assert!(matches!(err, VfsError::PermissionDenied(_)));
}
