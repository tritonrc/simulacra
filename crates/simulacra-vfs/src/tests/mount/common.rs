use std::collections::HashMap;

pub(super) fn test_config(
    auto_mount_skills: bool,
    mounts: Vec<simulacra_config::MountConfig>,
    agent_types: HashMap<String, simulacra_config::AgentTypeConfig>,
) -> simulacra_config::SimulacraConfig {
    simulacra_config::SimulacraConfig {
        project: simulacra_config::ProjectConfig {
            name: "test".to_string(),
            description: None,
        },
        agent_types,
        integrations: HashMap::new(),
        tenants: HashMap::new(),
        mcp: None,
        task: None,
        vfs: simulacra_config::VfsConfig {
            auto_mount_skills,
            max_files_per_mount: 10_000,
            max_bytes_per_mount: 104_857_600,
            mounts,
        },
        tiers: Default::default(),
        wasm: None,
        hooks: None,
        memory: None,
        catalog: simulacra_config::CatalogConfig::default(),
    }
}

pub(super) fn empty_agent_type() -> simulacra_config::AgentTypeConfig {
    simulacra_config::AgentTypeConfig {
        backend: Default::default(),
        model: "test-model".to_string(),
        acp_profile: None,
        system_prompt: None,
        skills: vec![],
        max_turns: None,
        max_tokens: None,
        max_sub_agents: None,
        can_spawn: vec![],
        restart_policy: None,
        capabilities: None,
    }
}
