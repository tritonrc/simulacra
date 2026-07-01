use std::sync::{Arc, atomic::AtomicU64};
use std::time::Instant;

use rust_decimal::Decimal;
use simulacra_types::{
    CapabilityToken, NetworkPermission, PathPattern, ResourceBudget, VfsError, VirtualFs,
};

use crate::MemoryFs;
use crate::procfs::{HookLister, ProcFs, ProcState, ToolLister};

// --- Fake ToolLister --------------------------------------------------------

pub(super) struct FakeToolLister {
    tools: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
}

impl FakeToolLister {
    pub(super) fn default_tools() -> Arc<Self> {
        Arc::new(Self {
            tools: std::sync::Mutex::new(vec![
                (
                    "file_read".to_string(),
                    serde_json::json!({
                        "description": "Read a file",
                        "input_schema": {"type": "object"},
                        "name": "file_read"
                    }),
                ),
                (
                    "list_dir".to_string(),
                    serde_json::json!({
                        "description": "List a directory",
                        "input_schema": {"type": "object"},
                        "name": "list_dir"
                    }),
                ),
            ]),
        })
    }
}

impl ToolLister for FakeToolLister {
    fn tool_names(&self) -> Vec<String> {
        self.tools
            .lock()
            .unwrap()
            .iter()
            .map(|(n, _)| n.clone())
            .collect()
    }

    fn tool_json(&self, name: &str) -> Option<String> {
        self.tools
            .lock()
            .unwrap()
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| serde_json::to_string(v).unwrap())
    }
}

// --- Fake HookLister --------------------------------------------------------

pub(super) struct FakeHookLister {
    hooks: std::collections::HashMap<String, Vec<String>>,
}

impl FakeHookLister {
    pub(super) fn with_tool_call_hooks() -> Arc<Self> {
        let mut hooks = std::collections::HashMap::new();
        hooks.insert(
            "tool_call".to_string(),
            vec!["audit".to_string(), "enforce".to_string()],
        );
        Arc::new(Self { hooks })
    }

    pub(super) fn empty() -> Arc<Self> {
        Arc::new(Self {
            hooks: std::collections::HashMap::new(),
        })
    }
}

impl HookLister for FakeHookLister {
    fn hook_names(&self, operation: &str) -> Vec<String> {
        self.hooks.get(operation).cloned().unwrap_or_default()
    }
}

// --- ProcState builders -----------------------------------------------------

pub(super) fn default_budget() -> Arc<std::sync::Mutex<ResourceBudget>> {
    let mut b = ResourceBudget::new(100_000, 10, Decimal::ZERO, 0);
    b.used_tokens = 4_521;
    b.used_turns = 3;
    b.used_cost = Decimal::new(12, 2); // 0.12
    Arc::new(std::sync::Mutex::new(b))
}

fn default_capabilities() -> CapabilityToken {
    CapabilityToken {
        shell: true,
        javascript: true,
        python: false,
        network: vec![
            NetworkPermission("*".to_string()),
            NetworkPermission("*.github.com".to_string()),
        ],
        mcp_tools: vec!["mcp:*:*".to_string()],
        paths_read: vec![
            PathPattern("/workspace/**".to_string()),
            PathPattern("/proc/**".to_string()),
        ],
        paths_write: vec![
            PathPattern("/workspace/**".to_string()),
            PathPattern("/proc/mailbox/**".to_string()),
        ],
        ..Default::default()
    }
}

/// Standard ProcFs: agent "agent-abc123", turn=3, 100k token budget with 4521
/// used, no parent.
pub(super) fn make_procfs() -> ProcFs<MemoryFs> {
    let state = Arc::new(ProcState {
        agent_id: "agent-abc123".to_string(),
        agent_name: "researcher".to_string(),
        model: "claude-sonnet-4-6".to_string(),
        parent_id: None,
        budget: default_budget(),
        capabilities: default_capabilities(),
        tools: FakeToolLister::default_tools(),
        session_id: "session-xyz".to_string(),
        session_start: Instant::now(),
        journal_entries: Arc::new(AtomicU64::new(42)),
        hooks: FakeHookLister::with_tool_call_hooks(),
        turn: Arc::new(AtomicU64::new(3)),
    });
    ProcFs::new(MemoryFs::new(), state)
}

/// Child agent with parent_id set.
pub(super) fn make_procfs_child() -> ProcFs<MemoryFs> {
    let state = Arc::new(ProcState {
        agent_id: "child-agent".to_string(),
        agent_name: "worker".to_string(),
        model: "claude-sonnet-4-6".to_string(),
        parent_id: Some("parent-agent".to_string()),
        budget: default_budget(),
        capabilities: default_capabilities(),
        tools: FakeToolLister::default_tools(),
        session_id: "session-xyz".to_string(),
        session_start: Instant::now(),
        journal_entries: Arc::new(AtomicU64::new(0)),
        hooks: FakeHookLister::empty(),
        turn: Arc::new(AtomicU64::new(1)),
    });
    ProcFs::new(MemoryFs::new(), state)
}

/// Unlimited budget (max_tokens=0, max_turns=0).
pub(super) fn make_procfs_unlimited_budget() -> ProcFs<MemoryFs> {
    let budget = {
        let mut b = ResourceBudget::new(0, 0, Decimal::ZERO, 0);
        b.used_tokens = 500;
        Arc::new(std::sync::Mutex::new(b))
    };
    let state = Arc::new(ProcState {
        agent_id: "agent-unlimited".to_string(),
        agent_name: "default".to_string(),
        model: "claude-sonnet-4-6".to_string(),
        parent_id: None,
        budget,
        capabilities: CapabilityToken::default(),
        tools: FakeToolLister::default_tools(),
        session_id: "session-unlimited".to_string(),
        session_start: Instant::now(),
        journal_entries: Arc::new(AtomicU64::new(0)),
        hooks: FakeHookLister::empty(),
        turn: Arc::new(AtomicU64::new(0)),
    });
    ProcFs::new(MemoryFs::new(), state)
}

/// No capabilities granted.
pub(super) fn make_procfs_no_caps() -> ProcFs<MemoryFs> {
    let state = Arc::new(ProcState {
        agent_id: "agent-nocaps".to_string(),
        agent_name: "restricted".to_string(),
        model: "claude-sonnet-4-6".to_string(),
        parent_id: None,
        budget: default_budget(),
        capabilities: CapabilityToken {
            shell: false,
            javascript: false,
            python: false,
            ..Default::default()
        },
        tools: FakeToolLister::default_tools(),
        session_id: "session-nocaps".to_string(),
        session_start: Instant::now(),
        journal_entries: Arc::new(AtomicU64::new(0)),
        hooks: FakeHookLister::empty(),
        turn: Arc::new(AtomicU64::new(0)),
    });
    ProcFs::new(MemoryFs::new(), state)
}

pub(super) fn procfs_read_str(vfs: &dyn VirtualFs, path: &str) -> String {
    String::from_utf8(
        vfs.read(path)
            .unwrap_or_else(|e| panic!("read({path}) failed: {e}")),
    )
    .unwrap()
}

pub(super) fn assert_permission_denied(err: &VfsError) {
    assert!(
        err.to_string().to_ascii_lowercase().contains("permission"),
        "expected a permission-denied error, got {err:?}"
    );
}
