use super::*;
use crate::InMemoryJournalStorage;
use simulacra_types::{MemoryCapability, MemoryPath, PathPattern};
use simulacra_vfs::MemoryFs;

fn parent_with_memory() -> CapabilityToken {
    CapabilityToken {
        paths_read: vec![PathPattern("/**".into())],
        paths_write: vec![PathPattern("/workspace/**".into())],
        memory: MemoryCapability {
            enabled: true,
            search_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
            write_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
        },
        ..Default::default()
    }
}

#[test]
fn override_without_memory_inherits_parent_memory() {
    // W1 regression: when the spawn_agent capabilities override has no
    // memory field, intersecting parent ∩ override must NOT strip the
    // parent's memory grants. The helper inherits parent.memory into
    // the override before intersect.
    let parent = parent_with_memory();
    let override_no_memory = CapabilityToken {
        // Match parent exactly so the path intersection has something to keep —
        // the focus of this test is the memory dimension, not path intersection.
        paths_read: vec![PathPattern("/**".into())],
        ..Default::default()
    };
    let with_memory = inherit_memory_when_override_unset(&override_no_memory, &parent);
    let intersected = parent.intersect(&with_memory);

    assert!(
        intersected.memory.enabled,
        "child must inherit parent memory when override doesn't author memory"
    );
    assert_eq!(
        intersected
            .memory
            .search_scopes
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>(),
        vec!["/var/memory/self"]
    );
}

#[test]
fn override_authoring_memory_is_not_overwritten() {
    // If a future override does author memory (e.g. narrows scopes),
    // the helper must NOT clobber it with parent.memory.
    let parent = parent_with_memory();
    let override_narrower = CapabilityToken {
        memory: MemoryCapability {
            enabled: true,
            search_scopes: vec![MemoryPath::parse("/var/memory/self/notes").unwrap()],
            write_scopes: vec![],
        },
        ..Default::default()
    };
    let merged = inherit_memory_when_override_unset(&override_narrower, &parent);
    // Should be the override's value, not parent's.
    assert_eq!(
        merged.memory.search_scopes[0].as_str(),
        "/var/memory/self/notes",
        "helper must not overwrite an override that authored memory"
    );
    assert!(merged.memory.write_scopes.is_empty());
}

#[test]
fn override_with_disabled_default_memory_inherits_parent() {
    // The override carries MemoryCapability::default() (disabled, empty)
    // because parse_capability_override has no JSON path for memory.
    // The helper must inherit parent memory in this case.
    let parent = parent_with_memory();
    let override_default = CapabilityToken::default();
    let merged = inherit_memory_when_override_unset(&override_default, &parent);
    assert!(merged.memory.enabled);
    assert_eq!(merged.memory.search_scopes.len(), 1);
}

#[test]
fn parent_without_memory_means_child_inherits_disabled() {
    // If parent has no memory, the child must also have no memory.
    let parent = CapabilityToken::default();
    let override_default = CapabilityToken::default();
    let merged = inherit_memory_when_override_unset(&override_default, &parent);
    assert!(!merged.memory.enabled);
}

#[test]
fn child_proc_runtime_overlays_child_proc_state_and_delegates_mailbox() {
    let inherited = Arc::new(MemoryFs::new());
    inherited.mkdir("/proc").unwrap();
    inherited.mkdir("/proc/mailbox").unwrap();
    inherited
        .write("/proc/mailbox/report.md", b"report")
        .unwrap();
    let inherited_vfs: Arc<dyn VirtualFs> = inherited;
    let inherited_journal: Arc<dyn JournalStorage> = Arc::new(InMemoryJournalStorage::new());
    let mut capability = CapabilityToken {
        javascript: true,
        ..Default::default()
    };
    capability.paths_read = vec![PathPattern("/**".into())];
    let runtime = child_proc_runtime(
        inherited_vfs,
        inherited_journal,
        ChildProcSpec {
            agent_id: AgentId("child-1".into()),
            agent_name: "researcher".into(),
            model: "child-model".into(),
            parent_id: AgentId("parent-1".into()),
            capability,
            budget: ResourceBudget::new(100, 4, Decimal::ZERO, 0),
            pipeline: None,
        },
    );
    runtime.tools.set(vec![ToolDefinition {
        name: "file_read".into(),
        description: "read".into(),
        input_schema: serde_json::json!({"type": "object"}),
    }]);

    assert_eq!(runtime.vfs.read("/proc/agent/id").unwrap(), b"child-1");
    assert_eq!(runtime.vfs.read("/proc/agent/name").unwrap(), b"researcher");
    assert_eq!(
        runtime.vfs.read("/proc/agent/parent_id").unwrap(),
        b"parent-1"
    );
    assert_eq!(
        runtime.vfs.read("/proc/capabilities/javascript").unwrap(),
        b"true"
    );
    assert_eq!(
        runtime.vfs.read("/proc/mailbox/report.md").unwrap(),
        b"report",
        "child-specific ProcFs must still delegate mailbox paths to the inherited stack"
    );
    assert_eq!(
        runtime.vfs.list_dir("/proc/tools").unwrap(),
        vec!["file_read"]
    );
}
