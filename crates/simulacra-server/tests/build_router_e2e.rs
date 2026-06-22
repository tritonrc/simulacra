//! S048 Task 4 — failing mount-glue integration tests for `build_router`.

use std::sync::Arc;

use async_graphql::{EmptySubscription, Schema};
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::json;
use simulacra_catalog::Catalog;
use simulacra_catalog::repo::{
    AgentFileRepository, AgentRepository, ChannelRepository, MemoryPoolRepository, SkillRepository,
    TenantRepository,
};
use simulacra_graphql::auth::NoAuthGraphQLProvider;
use simulacra_graphql::context::{AuthenticatedPrincipal, GraphQLContext};
use simulacra_graphql::schema::{MutationRoot, QueryRoot, SimulacraSchema};
use simulacra_server::{AppState, NoAuthProvider, TaskManager, TenantResolver, build_router};
use tower::ServiceExt;

fn make_state() -> AppState {
    let task_manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(TenantResolver::default());
    let auth: Arc<dyn simulacra_server::AuthProvider> =
        Arc::new(NoAuthProvider::new("dev@local", "default"));

    AppState::new(task_manager, resolver, auth)
}

async fn make_graphql_mount() -> simulacra_server::server::GraphQLMount {
    let catalog = Catalog::open_in_memory().expect("in-memory catalog must open");
    let tenant = catalog
        .tenants()
        .create("default", Some("Default"))
        .await
        .expect("default tenant must be seeded");

    let agents_repo: Arc<dyn AgentRepository> = Arc::new(catalog.agents());
    let skills_repo: Arc<dyn SkillRepository> = Arc::new(catalog.skills());
    let pools_repo: Arc<dyn MemoryPoolRepository> = Arc::new(catalog.memory_pools());
    let channels_repo: Arc<dyn ChannelRepository> = Arc::new(catalog.channels());
    let files_repo: Arc<dyn AgentFileRepository> = Arc::new(catalog.agent_files());

    let schema: SimulacraSchema = Schema::build(
        QueryRoot::default(),
        MutationRoot::default(),
        EmptySubscription,
    )
    .data(agents_repo)
    .data(skills_repo)
    .data(pools_repo)
    .data(channels_repo)
    .data(files_repo)
    .data(GraphQLContext {
        tenant_id: tenant.id.clone(),
        principal: AuthenticatedPrincipal {
            tenant_namespace: "default".to_owned(),
            subject: "dev@local".to_owned(),
        },
    })
    .finish();

    simulacra_server::server::GraphQLMount {
        schema,
        auth: Arc::new(NoAuthGraphQLProvider::new("dev@local", "default")),
        tenant_resolver: simulacra_graphql::context::TenantResolver::new(Arc::new(
            catalog.tenants(),
        )),
    }
}

#[tokio::test]
async fn serves_frontend_index_at_root() {
    let router = build_router(make_state(), vec![], None);

    let response = router
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .expect("root response should include a valid content-type header");
    assert!(
        content_type.starts_with("text/html"),
        "expected text/html content-type, got {content_type}"
    );
}

#[tokio::test]
async fn graphql_endpoint_is_reachable() {
    let mount = make_graphql_mount().await;
    let router = build_router(make_state(), vec![], Some(mount));

    let response = router
        .oneshot(
            Request::post("/graphql")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "query": "{ __typename }" })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(
        response.status().is_success(),
        "expected GraphQL endpoint to be reachable, got {}",
        response.status()
    );
}

#[tokio::test]
async fn rest_schema_endpoint_is_not_shadowed_by_static() {
    let router = build_router(make_state(), vec![], None);

    let response = router
        .oneshot(Request::get("/api/v1/schema").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .expect("schema response should include a valid content-type header");
    assert!(
        content_type.starts_with("application/json"),
        "expected application/json content-type, got {content_type}"
    );
}
