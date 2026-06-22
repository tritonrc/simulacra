use std::sync::Arc;

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::Response,
};
use simulacra_catalog::CatalogError;

use crate::context::{AuthenticatedPrincipal, GraphQLContext, TenantResolver};

#[async_trait::async_trait]
pub trait GraphQLAuthProvider: Send + Sync {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<AuthPrincipal, AuthError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthPrincipal {
    pub tenant_namespace: String,
    pub subject: String,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    #[error("unauthenticated")]
    Unauthenticated,
    #[error("forbidden")]
    Forbidden,
}

pub async fn auth_middleware(mut req: Request, next: Next) -> Result<Response, StatusCode> {
    let auth = req
        .extensions()
        .get::<Arc<dyn GraphQLAuthProvider>>()
        .cloned()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let tenants = req
        .extensions()
        .get::<TenantResolver>()
        .cloned()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let principal = match auth.authenticate(req.headers()).await {
        Ok(p) => p,
        Err(AuthError::Unauthenticated) => {
            return Ok(Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(Body::empty())
                .unwrap());
        }
        Err(AuthError::Forbidden) => {
            return Ok(Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::empty())
                .unwrap());
        }
    };

    let tenant_id = match tenants.resolve(&principal.tenant_namespace).await {
        Ok(id) => id,
        Err(CatalogError::NotFound(_)) => {
            return Ok(Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::empty())
                .unwrap());
        }
        Err(_) => {
            return Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::empty())
                .unwrap());
        }
    };

    let ctx = GraphQLContext {
        tenant_id,
        principal: AuthenticatedPrincipal {
            tenant_namespace: principal.tenant_namespace,
            subject: principal.subject,
        },
    };
    req.extensions_mut().insert(ctx);

    Ok(next.run(req).await)
}

/// Dev-only auth provider that returns a fixed principal regardless of headers.
///
/// This is the `dev_mode` counterpart to `simulacra-server`'s `NoAuthProvider`. It
/// MUST NOT be used in production: it ignores the `Authorization` header (and
/// every other header) and unconditionally returns the configured subject and
/// tenant namespace.
pub struct NoAuthGraphQLProvider {
    subject: String,
    tenant_namespace: String,
}

impl NoAuthGraphQLProvider {
    pub fn new(subject: impl Into<String>, tenant_namespace: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            tenant_namespace: tenant_namespace.into(),
        }
    }
}

#[async_trait::async_trait]
impl GraphQLAuthProvider for NoAuthGraphQLProvider {
    async fn authenticate(&self, _headers: &HeaderMap) -> Result<AuthPrincipal, AuthError> {
        Ok(AuthPrincipal {
            subject: self.subject.clone(),
            tenant_namespace: self.tenant_namespace.clone(),
        })
    }
}
