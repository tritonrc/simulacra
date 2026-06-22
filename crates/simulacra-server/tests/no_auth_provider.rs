use simulacra_server::{AuthProvider, Credentials, NoAuthProvider};

#[tokio::test]
async fn returns_configured_identity_for_empty_bearer() {
    let provider = NoAuthProvider::new("dev@local", "default");
    let identity = provider
        .authenticate(&Credentials::Bearer(String::new()))
        .await
        .expect("NoAuthProvider should never fail");

    assert_eq!(identity.subject, "dev@local");
    assert_eq!(identity.tenant_namespace.as_deref(), Some("default"));
}

#[tokio::test]
async fn ignores_bearer_token() {
    let provider = NoAuthProvider::new("dev@local", "default");
    let identity = provider
        .authenticate(&Credentials::Bearer("anything".to_string()))
        .await
        .expect("NoAuthProvider should never fail");

    assert_eq!(identity.subject, "dev@local");
    assert_eq!(identity.tenant_namespace.as_deref(), Some("default"));
}

#[tokio::test]
async fn ignores_api_key() {
    let provider = NoAuthProvider::new("dev@local", "default");
    let identity = provider
        .authenticate(&Credentials::ApiKey("anything".to_string()))
        .await
        .expect("NoAuthProvider should never fail");

    assert_eq!(identity.subject, "dev@local");
    assert_eq!(identity.tenant_namespace.as_deref(), Some("default"));
}
