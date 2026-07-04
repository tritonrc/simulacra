use super::*;

pub struct Harness {
    pub vfs: Arc<SpyFs>,
    pub cell: AgentCell,
}

impl Harness {
    pub fn new(
        capability: CapabilityToken,
        budget: Arc<Mutex<ResourceBudget>>,
        journal: Arc<FakeJournalStorage>,
    ) -> Self {
        let vfs = Arc::new(SpyFs::new());
        let vfs_dyn: Arc<dyn VirtualFs> = vfs.clone();
        let journal_dyn: Arc<dyn JournalStorage> = journal.clone();
        let http_client: Arc<dyn simulacra_http::HttpClient> =
            Arc::new(simulacra_http::UreqHttpClient::default());
        let cell = AgentCell::new(
            Arc::clone(&vfs_dyn),
            capability,
            Arc::clone(&budget),
            journal_dyn,
            http_client,
        );

        Self { vfs, cell }
    }

    pub fn read_file(&self, path: &str) -> Result<Vec<u8>, ExpectedSandboxError> {
        self.cell.read_file(path).map_err(sandbox_error_to_expected)
    }

    pub fn write_file(&self, path: &str, data: &[u8]) -> Result<(), ExpectedSandboxError> {
        self.cell
            .write_file(path, data)
            .map_err(sandbox_error_to_expected)
    }

    pub fn list_dir(&self, path: &str) -> Result<Vec<String>, ExpectedSandboxError> {
        self.cell.list_dir(path).map_err(sandbox_error_to_expected)
    }

    pub fn execute_shell(&self, command: &str) -> Result<CommandResult, ExpectedSandboxError> {
        self.cell
            .execute_shell(command)
            .map_err(sandbox_error_to_expected)
    }

    pub fn execute_js(&self, code: &str) -> Result<JsOutput, ExpectedSandboxError> {
        self.cell
            .execute_js(code)
            .map_err(sandbox_error_to_expected)
    }
}

pub fn capability(
    reads: &[&str],
    writes: &[&str],
    shell: bool,
    javascript: bool,
) -> CapabilityToken {
    capability_with_network(reads, writes, &[], shell, javascript)
}

pub fn capability_with_network(
    reads: &[&str],
    writes: &[&str],
    network: &[&str],
    shell: bool,
    javascript: bool,
) -> CapabilityToken {
    CapabilityToken {
        network: network
            .iter()
            .map(|permission| NetworkPermission((*permission).to_string()))
            .collect(),
        shell,
        javascript,
        paths_read: reads
            .iter()
            .map(|pattern| PathPattern((*pattern).to_string()))
            .collect(),
        paths_write: writes
            .iter()
            .map(|pattern| PathPattern((*pattern).to_string()))
            .collect(),
        ..Default::default()
    }
}

pub fn unlimited_budget() -> Arc<Mutex<ResourceBudget>> {
    Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0)))
}

pub fn budget_with_overrides(
    max_turns: u32,
    used_turns: u32,
    max_vfs_bytes: u64,
    used_vfs_bytes: u64,
) -> Arc<Mutex<ResourceBudget>> {
    let mut value = serde_json::to_value(ResourceBudget::new(0, max_turns, Decimal::ZERO, 0))
        .expect("budget should serialize");
    let map = value
        .as_object_mut()
        .expect("resource budget should serialize as an object");
    map.insert("used_turns".into(), Value::from(used_turns));
    map.insert("max_vfs_bytes".into(), Value::from(max_vfs_bytes));
    map.insert("used_vfs_bytes".into(), Value::from(used_vfs_bytes));
    Arc::new(Mutex::new(
        serde_json::from_value(value).expect("budget should deserialize"),
    ))
}

pub fn budget_counter(budget: &Arc<Mutex<ResourceBudget>>, field: &str) -> u64 {
    serde_json::to_value(&*budget.lock().unwrap())
        .expect("budget should serialize")
        .get(field)
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

pub fn journal_payload(entry: &JournalEntry) -> String {
    serde_json::to_string(&entry.entry).expect("journal entry should serialize")
}

pub fn assert_budget_exhausted(
    error: ExpectedSandboxError,
    expected_resources: &[&str],
    used: &str,
    limit: &str,
) {
    match error {
        ExpectedSandboxError::BudgetExhausted {
            resource,
            used: actual_used,
            limit: actual_limit,
        } => {
            assert!(
                expected_resources.contains(&resource.as_str()),
                "expected one of {expected_resources:?}, got {resource}"
            );
            assert_eq!(actual_used, used);
            assert_eq!(actual_limit, limit);
        }
        other => panic!("expected BudgetExhausted, got {other:?}"),
    }
}

/// A [`Harness`]-like fixture backed directly by a [`MemoryFs`] (no spy layer).
///
/// Used by tests that only need real VFS state, not read/write observation.
pub struct MemoryHarness {
    pub vfs: Arc<MemoryFs>,
    pub cell: AgentCell,
}

impl MemoryHarness {
    pub fn new(capability: CapabilityToken, journal: Arc<FakeJournalStorage>) -> Self {
        let vfs = Arc::new(MemoryFs::new());
        let vfs_dyn: Arc<dyn VirtualFs> = vfs.clone();
        let journal_dyn: Arc<dyn JournalStorage> = journal;
        let budget = Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0)));
        let http_client: Arc<dyn simulacra_http::HttpClient> =
            Arc::new(simulacra_http::UreqHttpClient::default());
        let cell = AgentCell::new(vfs_dyn, capability, budget, journal_dyn, http_client);
        Self { vfs, cell }
    }
}

/// Build a capability token granting shell and/or JavaScript and/or Python,
/// scoped to `/workspace/**` for read and write paths.
pub fn capability_token(shell: bool, javascript: bool, python: bool) -> CapabilityToken {
    CapabilityToken {
        shell,
        javascript,
        python,
        paths_read: vec![PathPattern("/workspace/**".into())],
        paths_write: vec![PathPattern("/workspace/**".into())],
        ..Default::default()
    }
}
