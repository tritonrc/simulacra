use std::sync::Arc;

use async_graphql::{EmptySubscription, Schema};
use serde_json::{Value, json};
use simulacra_catalog::repo::{
    AgentFileRepository, AgentRepository, MemoryPoolRepository, SkillRepository, TenantRepository,
};
use simulacra_catalog::{Agent, AgentFile, Catalog, NewAgent, NewAgentFile, Tenant};
use simulacra_graphql::context::{AuthenticatedPrincipal, GraphQLContext};
use simulacra_graphql::schema::{MutationRoot, QueryRoot, SimulacraSchema};

struct SeededAgentFileSchemas {
    schema_a: SimulacraSchema,
    schema_b: SimulacraSchema,
    agent_with_files_a: Agent,
    agent_without_files_a: Agent,
    agent_b: Agent,
    files_a: Vec<AgentFile>,
    file_b: AgentFile,
}

async fn create_agent(catalog: &Catalog, tenant: &Tenant, name: &str) -> Agent {
    let skill_ids: [simulacra_catalog::SkillId; 0] = [];
    let capabilities: Vec<String> = Vec::new();

    catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name,
                description: Some("agent under test"),
                system_prompt: "You are a helpful assistant.",
                model: "gpt-test",
                max_turns: Some(32),
                max_tokens: Some(2048),
                memory_pool_id: None,
                skill_ids: &skill_ids,
                capabilities: &capabilities,
                channel_ids: &[],
            },
        )
        .await
        .unwrap()
}

async fn create_file(
    catalog: &Catalog,
    tenant: &Tenant,
    agent: &Agent,
    name: &str,
    mime_type: &str,
    bytes: &[u8],
) -> AgentFile {
    catalog
        .agent_files()
        .create(
            &tenant.id,
            NewAgentFile {
                agent_id: &agent.id,
                name,
                mime_type,
                bytes,
            },
        )
        .await
        .unwrap()
}

fn build_schema(
    tenant: &Tenant,
    agents_repo: Arc<dyn AgentRepository>,
    skills_repo: Arc<dyn SkillRepository>,
    memory_pools_repo: Arc<dyn MemoryPoolRepository>,
    agent_files_repo: Arc<dyn AgentFileRepository>,
) -> SimulacraSchema {
    Schema::build(
        QueryRoot::default(),
        MutationRoot::default(),
        EmptySubscription,
    )
    .data(agents_repo)
    .data(skills_repo)
    .data(memory_pools_repo)
    .data(agent_files_repo)
    .data(GraphQLContext {
        tenant_id: tenant.id.clone(),
        principal: AuthenticatedPrincipal {
            tenant_namespace: tenant.namespace.clone(),
            subject: "test-user".to_owned(),
        },
    })
    .finish()
}

fn sort_files_by_created_at_and_id(files: &mut [AgentFile]) {
    files.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });
}

async fn schema_with_seeded_catalog_and_files() -> SeededAgentFileSchemas {
    let catalog = Catalog::open_in_memory().unwrap();

    let tenant_a = catalog
        .tenants()
        .create("tenant-a", Some("Tenant A"))
        .await
        .unwrap();
    let tenant_b = catalog
        .tenants()
        .create("tenant-b", Some("Tenant B"))
        .await
        .unwrap();

    let agent_with_files_a = create_agent(&catalog, &tenant_a, "agent-with-files").await;
    let agent_without_files_a = create_agent(&catalog, &tenant_a, "agent-without-files").await;
    let agent_b = create_agent(&catalog, &tenant_b, "agent-b").await;

    let mut files_a = vec![
        create_file(
            &catalog,
            &tenant_a,
            &agent_with_files_a,
            "brief.txt",
            "text/plain",
            b"brief for agent a",
        )
        .await,
        create_file(
            &catalog,
            &tenant_a,
            &agent_with_files_a,
            "handbook.pdf",
            "application/pdf",
            b"%PDF-1.7\nagent handbook\n%%EOF",
        )
        .await,
        create_file(
            &catalog,
            &tenant_a,
            &agent_with_files_a,
            "metrics.csv",
            "text/csv",
            b"day,value\n1,2\n",
        )
        .await,
    ];
    sort_files_by_created_at_and_id(&mut files_a);

    let file_b = create_file(
        &catalog,
        &tenant_b,
        &agent_b,
        "tenant-b-secret.pdf",
        "application/pdf",
        b"tenant b pdf bytes",
    )
    .await;

    let agents_repo: Arc<dyn AgentRepository> = Arc::new(catalog.agents());
    let skills_repo: Arc<dyn SkillRepository> = Arc::new(catalog.skills());
    let memory_pools_repo: Arc<dyn MemoryPoolRepository> = Arc::new(catalog.memory_pools());
    let agent_files_repo: Arc<dyn AgentFileRepository> = Arc::new(catalog.agent_files());

    let schema_a = build_schema(
        &tenant_a,
        Arc::clone(&agents_repo),
        Arc::clone(&skills_repo),
        Arc::clone(&memory_pools_repo),
        Arc::clone(&agent_files_repo),
    );
    let schema_b = build_schema(
        &tenant_b,
        agents_repo,
        skills_repo,
        memory_pools_repo,
        agent_files_repo,
    );

    SeededAgentFileSchemas {
        schema_a,
        schema_b,
        agent_with_files_a,
        agent_without_files_a,
        agent_b,
        files_a,
        file_b,
    }
}

async fn execute_json(schema: &SimulacraSchema, query: impl Into<String>) -> Value {
    let response = schema.execute(query.into()).await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    response.data.into_json().unwrap()
}

fn download_url(agent: &Agent, file: &AgentFile) -> String {
    format!(
        "/api/v1/agents/{}/files/{}/bytes",
        agent.id.as_str(),
        file.id.as_str()
    )
}

#[tokio::test]
async fn agent_files_returns_files_in_created_at_id_order() {
    let seeded = schema_with_seeded_catalog_and_files().await;

    let data = execute_json(
        &seeded.schema_a,
        format!(
            r#"{{
                agent(id: "{}") {{
                    files {{ id name mimeType sizeBytes downloadUrl }}
                }}
            }}"#,
            seeded.agent_with_files_a.id.as_str()
        ),
    )
    .await;

    let expected_files: Vec<Value> = seeded
        .files_a
        .iter()
        .map(|file| {
            json!({
                "id": file.id.as_str(),
                "name": file.name,
                "mimeType": file.mime_type,
                "sizeBytes": file.size_bytes,
                "downloadUrl": download_url(&seeded.agent_with_files_a, file),
            })
        })
        .collect();

    assert_eq!(data["agent"]["files"], json!(expected_files));
}

#[tokio::test]
async fn agent_files_returns_empty_list_for_agent_with_no_files() {
    let seeded = schema_with_seeded_catalog_and_files().await;

    let data = execute_json(
        &seeded.schema_a,
        format!(
            r#"{{
                agent(id: "{}") {{
                    files {{ id }}
                }}
            }}"#,
            seeded.agent_without_files_a.id.as_str()
        ),
    )
    .await;

    assert_eq!(data["agent"]["files"], json!([]));
}

#[tokio::test]
async fn agent_files_exposes_exact_download_url_shape() {
    let seeded = schema_with_seeded_catalog_and_files().await;

    let data = execute_json(
        &seeded.schema_a,
        format!(
            r#"{{
                agent(id: "{}") {{
                    files {{ id downloadUrl }}
                }}
            }}"#,
            seeded.agent_with_files_a.id.as_str()
        ),
    )
    .await;

    assert_eq!(
        data["agent"]["files"][0]["downloadUrl"],
        json!(download_url(&seeded.agent_with_files_a, &seeded.files_a[0]))
    );
}

#[tokio::test]
async fn agent_query_returns_null_for_agent_owned_by_another_tenant() {
    let seeded = schema_with_seeded_catalog_and_files().await;

    let data = execute_json(
        &seeded.schema_a,
        format!(
            r#"{{
                agent(id: "{}") {{
                    id
                    files {{ id }}
                }}
            }}"#,
            seeded.agent_b.id.as_str()
        ),
    )
    .await;

    assert!(data["agent"].is_null());
}

#[tokio::test]
async fn agent_file_query_returns_all_fields_for_known_id_under_correct_tenant() {
    let seeded = schema_with_seeded_catalog_and_files().await;
    let file = &seeded.files_a[1];

    let data = execute_json(
        &seeded.schema_a,
        format!(
            r#"{{
                agentFile(id: "{}") {{
                    id
                    agentId
                    name
                    mimeType
                    sizeBytes
                    downloadUrl
                    createdAt
                    updatedAt
                }}
            }}"#,
            file.id.as_str()
        ),
    )
    .await;

    assert_eq!(data["agentFile"]["id"], file.id.as_str());
    assert_eq!(
        data["agentFile"]["agentId"],
        seeded.agent_with_files_a.id.as_str()
    );
    assert_eq!(data["agentFile"]["name"], file.name);
    assert_eq!(data["agentFile"]["mimeType"], file.mime_type);
    assert_eq!(data["agentFile"]["sizeBytes"], json!(file.size_bytes));
    assert_eq!(
        data["agentFile"]["downloadUrl"],
        json!(download_url(&seeded.agent_with_files_a, file))
    );
    assert_eq!(
        data["agentFile"]["createdAt"],
        json!(file.created_at.to_rfc3339())
    );
    assert_eq!(
        data["agentFile"]["updatedAt"],
        json!(file.updated_at.to_rfc3339())
    );
}

#[tokio::test]
async fn agent_file_query_returns_null_for_unknown_id_without_errors() {
    let seeded = schema_with_seeded_catalog_and_files().await;

    let data = execute_json(
        &seeded.schema_a,
        r#"{
            agentFile(id: "missing-agent-file") {
                id
            }
        }"#,
    )
    .await;

    assert!(data["agentFile"].is_null());
}

#[tokio::test]
async fn agent_file_query_returns_null_for_cross_tenant_id_without_errors() {
    let seeded = schema_with_seeded_catalog_and_files().await;

    let data = execute_json(
        &seeded.schema_a,
        format!(
            r#"{{
                agentFile(id: "{}") {{
                    id
                }}
            }}"#,
            seeded.file_b.id.as_str()
        ),
    )
    .await;

    assert!(data["agentFile"].is_null());
}

#[tokio::test]
async fn detach_agent_file_returns_true_and_removes_file_from_queries() {
    let seeded = schema_with_seeded_catalog_and_files().await;
    let removed = &seeded.files_a[1];

    let mutation = execute_json(
        &seeded.schema_a,
        format!(
            r#"mutation {{
                detachAgentFile(id: "{}")
            }}"#,
            removed.id.as_str()
        ),
    )
    .await;
    assert_eq!(mutation["detachAgentFile"], json!(true));

    let agent_file_data = execute_json(
        &seeded.schema_a,
        format!(
            r#"{{
                agentFile(id: "{}") {{
                    id
                }}
            }}"#,
            removed.id.as_str()
        ),
    )
    .await;
    assert!(agent_file_data["agentFile"].is_null());

    let agent_data = execute_json(
        &seeded.schema_a,
        format!(
            r#"{{
                agent(id: "{}") {{
                    files {{ id }}
                }}
            }}"#,
            seeded.agent_with_files_a.id.as_str()
        ),
    )
    .await;
    let remaining = agent_data["agent"]["files"].as_array().unwrap();
    assert_eq!(remaining.len(), 2);
    assert!(
        !remaining
            .iter()
            .any(|file| file["id"] == removed.id.as_str())
    );
    assert!(
        remaining
            .iter()
            .any(|file| file["id"] == seeded.files_a[0].id.as_str())
    );
    assert!(
        remaining
            .iter()
            .any(|file| file["id"] == seeded.files_a[2].id.as_str())
    );
}

#[tokio::test]
async fn detach_agent_file_returns_false_for_unknown_id_without_errors() {
    let seeded = schema_with_seeded_catalog_and_files().await;

    let data = execute_json(
        &seeded.schema_a,
        r#"mutation {
            detachAgentFile(id: "missing-agent-file")
        }"#,
    )
    .await;

    assert_eq!(data["detachAgentFile"], json!(false));
}

#[tokio::test]
async fn detach_agent_file_returns_false_for_cross_tenant_id_and_preserves_tenant_b_file() {
    let seeded = schema_with_seeded_catalog_and_files().await;

    let mutation = execute_json(
        &seeded.schema_a,
        format!(
            r#"mutation {{
                detachAgentFile(id: "{}")
            }}"#,
            seeded.file_b.id.as_str()
        ),
    )
    .await;
    assert_eq!(mutation["detachAgentFile"], json!(false));

    let data = execute_json(
        &seeded.schema_b,
        format!(
            r#"{{
                agentFile(id: "{}") {{
                    id
                    agentId
                    name
                }}
            }}"#,
            seeded.file_b.id.as_str()
        ),
    )
    .await;

    assert_eq!(data["agentFile"]["id"], seeded.file_b.id.as_str());
    assert_eq!(data["agentFile"]["agentId"], seeded.agent_b.id.as_str());
    assert_eq!(data["agentFile"]["name"], seeded.file_b.name);
}

#[tokio::test]
async fn agent_file_size_bytes_is_an_int_and_matches_the_byte_length() {
    let seeded = schema_with_seeded_catalog_and_files().await;
    let file = &seeded.files_a[0];

    let data = execute_json(
        &seeded.schema_a,
        format!(
            r#"{{
                agentFile(id: "{}") {{
                    sizeBytes
                }}
            }}"#,
            file.id.as_str()
        ),
    )
    .await;

    assert!(data["agentFile"]["sizeBytes"].is_i64());
    assert_eq!(
        data["agentFile"]["sizeBytes"].as_i64(),
        Some(file.size_bytes as i64)
    );
}

#[tokio::test]
async fn agent_file_mime_type_round_trips_application_pdf() {
    let seeded = schema_with_seeded_catalog_and_files().await;
    let file = &seeded.files_a[1];

    let data = execute_json(
        &seeded.schema_a,
        format!(
            r#"{{
                agentFile(id: "{}") {{
                    mimeType
                }}
            }}"#,
            file.id.as_str()
        ),
    )
    .await;

    assert_eq!(data["agentFile"]["mimeType"], json!("application/pdf"));
}
