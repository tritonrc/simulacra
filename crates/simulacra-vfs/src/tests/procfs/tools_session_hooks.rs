use std::sync::{Arc, atomic::AtomicU64};
use std::time::Instant;

use simulacra_types::{CapabilityToken, VfsError, VirtualFs};

use crate::MemoryFs;
use crate::procfs::{ProcFs, ProcState, ToolLister};

use super::common::{
    FakeHookLister, default_budget, make_procfs, make_procfs_child, make_procfs_no_caps,
    procfs_read_str,
};

#[test]
fn procfs_capabilities_shell_returns_true_when_shell_is_granted() {
    let fs = make_procfs();
    assert_eq!(procfs_read_str(&fs, "/proc/capabilities/shell"), "true");
}

#[test]
fn procfs_capabilities_shell_returns_false_when_shell_is_not_granted() {
    let fs = make_procfs_no_caps();
    assert_eq!(procfs_read_str(&fs, "/proc/capabilities/shell"), "false");
}

#[test]
fn procfs_capabilities_network_returns_newline_separated_patterns() {
    let fs = make_procfs();
    assert_eq!(
        procfs_read_str(&fs, "/proc/capabilities/network"),
        "*\n*.github.com"
    );
}

#[test]
fn procfs_capabilities_network_returns_empty_string_when_no_network_access() {
    let fs = make_procfs_no_caps();
    assert_eq!(procfs_read_str(&fs, "/proc/capabilities/network"), "");
}

#[test]
fn procfs_capabilities_paths_read_returns_newline_separated_patterns() {
    let fs = make_procfs();
    assert_eq!(
        procfs_read_str(&fs, "/proc/capabilities/paths_read"),
        "/workspace/**\n/proc/**"
    );
}

#[test]
fn procfs_capabilities_mcp_tools_returns_newline_separated_patterns() {
    let fs = make_procfs();
    assert_eq!(
        procfs_read_str(&fs, "/proc/capabilities/mcp_tools"),
        "mcp:*:*"
    );
}

#[test]
fn procfs_capabilities_paths_write_returns_newline_separated_patterns() {
    let fs = make_procfs();
    assert_eq!(
        procfs_read_str(&fs, "/proc/capabilities/paths_write"),
        "/workspace/**\n/proc/mailbox/**"
    );
}

// --- Tool exposure tests ----------------------------------------------------

#[test]
fn procfs_tools_listing_returns_registered_tool_names_sorted() {
    let fs = make_procfs();
    let names = fs.list_dir("/proc/tools").unwrap();
    assert_eq!(names, vec!["file_read", "list_dir"]);
}

#[test]
fn procfs_tools_named_entry_returns_json_with_name_description_and_input_schema() {
    let fs = make_procfs();
    let raw = procfs_read_str(&fs, "/proc/tools/file_read");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("tool JSON must be valid");
    assert_eq!(v["name"], "file_read");
    assert!(v["description"].is_string());
    assert!(v["input_schema"].is_object());
}

#[test]
fn procfs_tools_nonexistent_tool_returns_not_found_error() {
    let fs = make_procfs();
    assert!(matches!(
        fs.read("/proc/tools/nosuch"),
        Err(VfsError::NotFound(_))
    ));
}

#[test]
fn procfs_tool_listing_reflects_dynamic_registry_changes() {
    struct DynamicToolLister {
        names: std::sync::Mutex<Vec<String>>,
    }
    impl ToolLister for DynamicToolLister {
        fn tool_names(&self) -> Vec<String> {
            self.names.lock().unwrap().clone()
        }
        fn tool_json(&self, _: &str) -> Option<String> {
            None
        }
    }
    let lister = Arc::new(DynamicToolLister {
        names: std::sync::Mutex::new(vec!["tool_a".to_string()]),
    });
    let lister_clone = Arc::clone(&lister);
    let state = Arc::new(ProcState {
        agent_id: "a".to_string(),
        agent_name: "a".to_string(),
        model: "m".to_string(),
        parent_id: None,
        budget: default_budget(),
        capabilities: CapabilityToken::default(),
        tools: lister,
        session_id: "s".to_string(),
        session_start: Instant::now(),
        journal_entries: Arc::new(AtomicU64::new(0)),
        hooks: FakeHookLister::empty(),
        turn: Arc::new(AtomicU64::new(0)),
    });
    let fs = ProcFs::new(MemoryFs::new(), state);

    let before = fs.list_dir("/proc/tools").unwrap();
    lister_clone
        .names
        .lock()
        .unwrap()
        .push("tool_b".to_string());
    let after = fs.list_dir("/proc/tools").unwrap();

    assert_eq!(before, vec!["tool_a"]);
    assert_eq!(after, vec!["tool_a", "tool_b"]);
}

// --- Session tests ----------------------------------------------------------

#[test]
fn procfs_session_id_returns_the_session_id() {
    let fs = make_procfs();
    assert_eq!(procfs_read_str(&fs, "/proc/session/id"), "session-xyz");
}

#[test]
fn procfs_session_uptime_ms_returns_a_numeric_string_that_increases_over_time() {
    let fs = make_procfs();
    let first: u64 = procfs_read_str(&fs, "/proc/session/uptime_ms")
        .parse()
        .expect("uptime_ms should be a number");
    std::thread::sleep(std::time::Duration::from_millis(2));
    let second: u64 = procfs_read_str(&fs, "/proc/session/uptime_ms")
        .parse()
        .expect("uptime_ms should stay numeric");
    assert!(second >= first, "uptime_ms should not decrease");
}

#[test]
fn procfs_session_journal_entries_returns_current_count() {
    let fs = make_procfs();
    assert_eq!(procfs_read_str(&fs, "/proc/session/journal_entries"), "42");
}

// --- Hook tests -------------------------------------------------------------

#[test]
fn procfs_hooks_tool_call_returns_newline_separated_hook_names() {
    let fs = make_procfs();
    assert_eq!(
        procfs_read_str(&fs, "/proc/hooks/tool_call"),
        "audit\nenforce"
    );
}

#[test]
fn procfs_hooks_tool_call_returns_empty_string_when_no_hooks_registered() {
    let fs = make_procfs_child();
    assert_eq!(procfs_read_str(&fs, "/proc/hooks/tool_call"), "");
}

#[test]
fn procfs_hooks_directory_lists_all_four_operation_types() {
    let fs = make_procfs();
    let names = fs.list_dir("/proc/hooks").unwrap();
    assert_eq!(names, vec!["http_request", "llm", "spawn", "tool_call"]);
}

// --- Directory listing tests ------------------------------------------------
