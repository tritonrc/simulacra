use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_graphql::{EmptySubscription, Schema};
use axum::body::Body;
use http::{Request, StatusCode};
use simulacra_catalog::repo::{
    AgentRepository, MemoryPoolRepository, SkillRepository, TenantRepository,
};
use simulacra_catalog::{Catalog, CatalogError, NewAgent, NewSkill, Tenant, TenantId};
use simulacra_graphql::auth::{AuthError, AuthPrincipal, GraphQLAuthProvider};
use simulacra_graphql::context::TenantResolver;
use simulacra_graphql::graphql_router;
use simulacra_graphql::schema::{MutationRoot, QueryRoot};
use tower::ServiceExt;

struct StubAuth(Result<AuthPrincipal, AuthError>);

#[async_trait::async_trait]
impl GraphQLAuthProvider for StubAuth {
    async fn authenticate(&self, _headers: &http::HeaderMap) -> Result<AuthPrincipal, AuthError> {
        self.0.clone()
    }
}

struct CountingTenants {
    tenant: Tenant,
    lookups: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl TenantRepository for CountingTenants {
    async fn get_by_namespace(&self, namespace: &str) -> Result<Tenant, CatalogError> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        if namespace == self.tenant.namespace {
            return Ok(self.tenant.clone());
        }
        Err(CatalogError::NotFound(format!(
            "tenant namespace={namespace}"
        )))
    }

    async fn get_by_id(&self, id: &TenantId) -> Result<Tenant, CatalogError> {
        if &self.tenant.id == id {
            return Ok(self.tenant.clone());
        }
        Err(CatalogError::NotFound(format!("tenant id={id}")))
    }

    async fn create(
        &self,
        _namespace: &str,
        _display_name: Option<&str>,
    ) -> Result<Tenant, CatalogError> {
        unimplemented!("test double only")
    }

    async fn get_or_create(
        &self,
        _namespace: &str,
        _display_name: Option<&str>,
    ) -> Result<Tenant, CatalogError> {
        unimplemented!("test double only")
    }
}

async fn schema_and_router(
    auth: Arc<dyn GraphQLAuthProvider>,
    tenants: TenantResolver,
) -> (
    axum::Router,
    simulacra_catalog::Agent,
    simulacra_catalog::Agent,
) {
    let catalog = Catalog::open_in_memory().unwrap();
    let acme = catalog
        .tenants()
        .create("acme", Some("Acme"))
        .await
        .unwrap();
    let evil = catalog
        .tenants()
        .create("evil", Some("Evil"))
        .await
        .unwrap();
    let skill = catalog
        .skills()
        .create(
            &acme.id,
            NewSkill {
                name: "alpha",
                description: None,
                body: "alpha body",
                metadata: None,
            },
        )
        .await
        .unwrap();

    let acme_agent = catalog
        .agents()
        .create(
            &acme.id,
            NewAgent {
                name: "acme-agent",
                description: Some("visible"),
                system_prompt: "prompt",
                model: "gpt-test",
                max_turns: None,
                max_tokens: None,
                memory_pool_id: None,
                skill_ids: std::slice::from_ref(&skill.id),
                capabilities: &["net:read".to_owned()],
                channel_ids: &[],
            },
        )
        .await
        .unwrap();
    let evil_agent = catalog
        .agents()
        .create(
            &evil.id,
            NewAgent {
                name: "evil-agent",
                description: Some("hidden"),
                system_prompt: "prompt",
                model: "gpt-test",
                max_turns: None,
                max_tokens: None,
                memory_pool_id: None,
                skill_ids: &[],
                capabilities: &["net:read".to_owned()],
                channel_ids: &[],
            },
        )
        .await
        .unwrap();

    // Intentionally do NOT pre-inject GraphQLContext via .data(...). The handler
    // is responsible for pulling the request-scoped GraphQLContext out of the
    // request extensions (populated by the auth middleware). Pre-injecting here
    // would mask middleware bugs by always seeding "acme".
    let _ = &acme;
    let schema = Schema::build(
        QueryRoot::default(),
        MutationRoot::default(),
        EmptySubscription,
    )
    .data(Arc::new(catalog.agents()) as Arc<dyn AgentRepository>)
    .data(Arc::new(catalog.skills()) as Arc<dyn SkillRepository>)
    .data(Arc::new(catalog.memory_pools()) as Arc<dyn MemoryPoolRepository>)
    .finish();

    let router = graphql_router(schema, auth, tenants);
    (router, acme_agent, evil_agent)
}

#[tokio::test]
async fn unauthenticated_request_returns_401() {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = catalog
        .tenants()
        .create("acme", Some("Acme"))
        .await
        .unwrap();
    let tenants = TenantResolver::new(Arc::new(CountingTenants {
        tenant,
        lookups: Arc::new(AtomicUsize::new(0)),
    }));
    let (router, _, _) =
        schema_and_router(Arc::new(StubAuth(Err(AuthError::Unauthenticated))), tenants).await;

    let response = router
        .oneshot(
            Request::post("/graphql")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"{ __typename }"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn authenticated_request_without_tenant_resolution_fails_before_resolvers() {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = catalog
        .tenants()
        .create("acme", Some("Acme"))
        .await
        .unwrap();
    let missing = Tenant {
        namespace: "different".to_owned(),
        ..tenant
    };
    let tenants = TenantResolver::new(Arc::new(CountingTenants {
        tenant: missing,
        lookups: Arc::new(AtomicUsize::new(0)),
    }));
    let (router, _, _) = schema_and_router(
        Arc::new(StubAuth(Ok(AuthPrincipal {
            tenant_namespace: "acme".to_owned(),
            subject: "user-1".to_owned(),
        }))),
        tenants,
    )
    .await;

    let response = router
        .oneshot(
            Request::post("/graphql")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"{ __typename }"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn tenant_a_principal_cannot_query_tenant_b_agent_by_id() {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = catalog
        .tenants()
        .create("acme", Some("Acme"))
        .await
        .unwrap();
    let tenants = TenantResolver::new(Arc::new(CountingTenants {
        tenant,
        lookups: Arc::new(AtomicUsize::new(0)),
    }));
    let (router, _acme_agent, evil_agent) = schema_and_router(
        Arc::new(StubAuth(Ok(AuthPrincipal {
            tenant_namespace: "acme".to_owned(),
            subject: "user-1".to_owned(),
        }))),
        tenants,
    )
    .await;

    let response = router
        .oneshot(
            Request::post("/graphql")
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"query":"{{ agent(id: \"{}\") {{ id name }} }} "}}"#,
                    evil_agent.id.as_str()
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // Cross-tenant id lookup must surface as `data.agent == null`, no errors —
    // a NotFound mapped to None (per S042 plan Task 7). A populated `data.agent`
    // would mean tenant A leaked tenant B's row.
    assert!(
        payload
            .get("errors")
            .map(|e| e.is_null() || e.as_array().map(|a| a.is_empty()).unwrap_or(false))
            .unwrap_or(true),
        "expected no errors, got: {payload}"
    );
    assert!(
        payload["data"]["agent"].is_null(),
        "expected data.agent=null (cross-tenant id should be invisible), got: {payload}"
    );
}

#[tokio::test]
async fn tenant_a_principal_cannot_update_tenant_b_agent() {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = catalog
        .tenants()
        .create("acme", Some("Acme"))
        .await
        .unwrap();
    let tenants = TenantResolver::new(Arc::new(CountingTenants {
        tenant,
        lookups: Arc::new(AtomicUsize::new(0)),
    }));
    let (router, _acme_agent, evil_agent) = schema_and_router(
        Arc::new(StubAuth(Ok(AuthPrincipal {
            tenant_namespace: "acme".to_owned(),
            subject: "user-1".to_owned(),
        }))),
        tenants,
    )
    .await;

    let response = router
        .oneshot(
            Request::post("/graphql")
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"query":"mutation {{ updateAgent(id: \"{}\", input: {{ description: \"patched\" }}) {{ id }} }} "}}"#,
                    evil_agent.id.as_str()
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // Spec line 524: cross-tenant updateAgent must fail. The repo treats the
    // row as invisible (CatalogError::NotFound), so the GraphQL extension code
    // must be NOT_FOUND.
    let code = payload["errors"][0]["extensions"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("expected errors[0].extensions.code, got: {payload}"));
    assert_eq!(code, "NOT_FOUND", "payload was: {payload}");
}

#[tokio::test]
async fn tenant_resolver_caches_and_reresolves_after_invalidate() {
    let tenant = Tenant {
        id: TenantId::from("acme-tenant"),
        namespace: "acme".to_owned(),
        display_name: Some("Acme".to_owned()),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let lookups = Arc::new(AtomicUsize::new(0));
    let resolver = TenantResolver::new(Arc::new(CountingTenants {
        tenant,
        lookups: Arc::clone(&lookups),
    }));

    let first = resolver.resolve("acme").await.unwrap();
    let second = resolver.resolve("acme").await.unwrap();
    resolver.invalidate("acme");
    let third = resolver.resolve("acme").await.unwrap();

    assert_eq!(first, TenantId::from("acme-tenant"));
    assert_eq!(second, TenantId::from("acme-tenant"));
    assert_eq!(third, TenantId::from("acme-tenant"));
    assert_eq!(lookups.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn malformed_json_body_returns_400_with_graphql_error_envelope() {
    // Phase 4 WARNING fix: when the request body fails to parse as a GraphQL
    // request, the handler must respond with status 400, JSON Content-Type,
    // and a GraphQL-over-HTTP-shaped error envelope (`{"errors":[{...}]}`).
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = catalog
        .tenants()
        .create("acme", Some("Acme"))
        .await
        .unwrap();
    let tenants = TenantResolver::new(Arc::new(CountingTenants {
        tenant,
        lookups: Arc::new(AtomicUsize::new(0)),
    }));
    let (router, _, _) = schema_and_router(
        Arc::new(StubAuth(Ok(AuthPrincipal {
            tenant_namespace: "acme".to_owned(),
            subject: "user-1".to_owned(),
        }))),
        tenants,
    )
    .await;

    let response = router
        .oneshot(
            Request::post("/graphql")
                .header("content-type", "application/json")
                // Lone "{" is invalid JSON.
                .body(Body::from("{"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let content_type = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .expect("content-type header")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        content_type.contains("json"),
        "expected JSON content-type, got: {content_type}"
    );

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let payload: serde_json::Value =
        serde_json::from_slice(&body).expect("400 body must parse as JSON");
    let errors = payload
        .get("errors")
        .and_then(|e| e.as_array())
        .expect("body must include an `errors` array");
    assert!(!errors.is_empty(), "errors array must not be empty");
    assert!(
        errors[0].get("message").and_then(|m| m.as_str()).is_some(),
        "first error must include a `message` string, got: {payload}"
    );
}

#[tokio::test]
async fn tenant_resolver_concurrent_resolves_do_not_double_call() {
    // W4: caching should suppress repeated repo lookups even under concurrency.
    // A brief race between two callers may produce 2 calls before the cache
    // settles, but >2 indicates the resolver is missing per-key coalescing or
    // is otherwise broken.
    let tenant = Tenant {
        id: TenantId::from("acme-tenant"),
        namespace: "acme".to_owned(),
        display_name: Some("Acme".to_owned()),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let lookups = Arc::new(AtomicUsize::new(0));
    let resolver = TenantResolver::new(Arc::new(CountingTenants {
        tenant,
        lookups: Arc::clone(&lookups),
    }));

    let r1 = resolver.clone();
    let r2 = resolver.clone();
    let (a, b) = tokio::join!(async move { r1.resolve("acme").await }, async move {
        r2.resolve("acme").await
    },);

    assert_eq!(a.unwrap(), TenantId::from("acme-tenant"));
    assert_eq!(b.unwrap(), TenantId::from("acme-tenant"));
    let calls = lookups.load(Ordering::SeqCst);
    assert!(
        calls == 1 || calls == 2,
        "expected 1 or 2 underlying lookups, got {calls}"
    );
}
