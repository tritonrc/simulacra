//! S042 — GraphQL control plane for the agent catalog.

use std::sync::Arc;

use axum::{
    Extension, Router,
    body::{Body, Bytes},
    http::{HeaderValue, StatusCode, header},
    response::Response,
    routing::post,
};

pub mod auth;
pub mod context;
pub mod error;
pub mod schema;
pub mod tool_catalog;

pub fn graphql_router(
    schema: schema::SimulacraSchema,
    auth: Arc<dyn auth::GraphQLAuthProvider>,
    tenant_resolver: context::TenantResolver,
) -> Router {
    Router::new()
        .route("/graphql", post(handler))
        .layer(axum::middleware::from_fn(auth::auth_middleware))
        .layer(Extension(schema))
        .layer(Extension(auth))
        .layer(Extension(tenant_resolver))
}

async fn handler(
    Extension(schema): Extension<schema::SimulacraSchema>,
    ctx: Option<Extension<context::GraphQLContext>>,
    body: Bytes,
) -> Response {
    let req: async_graphql::Request = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            // GraphQL-over-HTTP shape: errors are an array of objects with a
            // `message` field, JSON Content-Type, status 400 for parse failures.
            let envelope = serde_json::json!({
                "errors": [
                    { "message": format!("invalid graphql request: {e}") }
                ]
            });
            let body = serde_json::to_vec(&envelope).unwrap_or_else(|_| {
                br#"{"errors":[{"message":"invalid graphql request"}]}"#.to_vec()
            });
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                )
                .body(Body::from(body))
                .unwrap();
        }
    };
    let req = if let Some(Extension(ctx)) = ctx {
        req.data(ctx)
    } else {
        req
    };
    let response = schema.execute(req).await;
    let body = serde_json::to_vec(&response).unwrap_or_else(|_| b"{}".to_vec());
    let mut resp = Response::new(Body::from(body));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}
