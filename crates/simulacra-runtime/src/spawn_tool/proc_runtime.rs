use super::*;

#[derive(Clone, Default)]
pub(super) struct RuntimeSharedToolList(Arc<Mutex<Vec<ToolDefinition>>>);

impl RuntimeSharedToolList {
    pub(super) fn set(&self, definitions: Vec<ToolDefinition>) {
        *self
            .0
            .lock()
            .expect("tool definition list lock should not be poisoned") = definitions;
    }
}

impl ToolLister for RuntimeSharedToolList {
    fn tool_names(&self) -> Vec<String> {
        self.0
            .lock()
            .expect("tool definition list lock should not be poisoned")
            .iter()
            .map(|definition| definition.name.clone())
            .collect()
    }

    fn tool_json(&self, name: &str) -> Option<String> {
        self.0
            .lock()
            .expect("tool definition list lock should not be poisoned")
            .iter()
            .find(|definition| definition.name == name)
            .and_then(|definition| serde_json::to_string(definition).ok())
    }
}

struct RuntimePipelineHookLister(Option<Arc<simulacra_hooks::pipeline::HookPipeline>>);

impl HookLister for RuntimePipelineHookLister {
    fn hook_names(&self, operation: &str) -> Vec<String> {
        let Some(pipeline) = self.0.as_ref() else {
            return vec![];
        };
        use simulacra_hooks::verdict::Operation;
        let operation = match operation {
            "tool_call" => Operation::ToolCall,
            "llm" => Operation::Llm,
            "spawn" => Operation::Spawn,
            "http_request" => Operation::HttpRequest,
            "vfs_write" => Operation::VfsWrite,
            _ => return vec![],
        };
        pipeline.hook_names(operation)
    }
}

pub(super) struct ChildProcRuntime {
    pub(super) vfs: Arc<dyn VirtualFs>,
    pub(super) journal: Arc<dyn JournalStorage>,
    pub(super) budget: Arc<Mutex<ResourceBudget>>,
    pub(super) turn: Arc<AtomicU64>,
    pub(super) tools: RuntimeSharedToolList,
}

pub(super) struct ChildProcSpec {
    pub(super) agent_id: AgentId,
    pub(super) agent_name: String,
    pub(super) model: String,
    pub(super) parent_id: AgentId,
    pub(super) capability: CapabilityToken,
    pub(super) budget: ResourceBudget,
    pub(super) pipeline: Option<Arc<simulacra_hooks::pipeline::HookPipeline>>,
}

pub(super) fn child_proc_runtime(
    inherited_vfs: Arc<dyn VirtualFs>,
    inherited_journal: Arc<dyn JournalStorage>,
    spec: ChildProcSpec,
) -> ChildProcRuntime {
    let budget = Arc::new(Mutex::new(spec.budget));
    let turn = Arc::new(AtomicU64::new(0));
    let journal_entries = Arc::new(AtomicU64::new(0));
    let tools = RuntimeSharedToolList::default();
    let state = Arc::new(ProcState {
        agent_id: spec.agent_id.0.clone(),
        agent_name: spec.agent_name,
        model: spec.model,
        parent_id: Some(spec.parent_id.0),
        budget: Arc::clone(&budget),
        capabilities: spec.capability,
        tools: Arc::new(tools.clone()),
        session_id: spec.agent_id.0,
        session_start: Instant::now(),
        journal_entries: Arc::clone(&journal_entries),
        hooks: Arc::new(RuntimePipelineHookLister(spec.pipeline)),
        turn: Arc::clone(&turn),
    });
    let vfs: Arc<dyn VirtualFs> = Arc::new(ProcFs::new(inherited_vfs, state));
    let journal: Arc<dyn JournalStorage> = Arc::new(CountingJournalStorage::new(
        inherited_journal,
        Arc::clone(&journal_entries),
    ));

    ChildProcRuntime {
        vfs,
        journal,
        budget,
        turn,
        tools,
    }
}
