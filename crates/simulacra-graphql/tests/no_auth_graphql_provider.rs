use axum::http::HeaderMap;
use simulacra_graphql::auth::{AuthPrincipal, GraphQLAuthProvider, NoAuthGraphQLProvider};

#[tokio::test]
async fn returns_configured_principal_for_empty_headers() {
    let provider: &dyn GraphQLAuthProvider = &NoAuthGraphQLProvider::new("dev@local", "default");

    let principal: AuthPrincipal = provider.authenticate(&HeaderMap::new()).await.unwrap();

    assert_eq!(principal.subject, "dev@local");
    assert_eq!(principal.tenant_namespace, "default");
}

#[tokio::test]
async fn ignores_authorization_header() {
    let provider: &dyn GraphQLAuthProvider = &NoAuthGraphQLProvider::new("dev@local", "default");
    let mut headers = HeaderMap::new();
    headers.insert("authorization", "Bearer anything".parse().unwrap());

    let principal: AuthPrincipal = provider.authenticate(&headers).await.unwrap();

    assert_eq!(principal.subject, "dev@local");
    assert_eq!(principal.tenant_namespace, "default");
}
