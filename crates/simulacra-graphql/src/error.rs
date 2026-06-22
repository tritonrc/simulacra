use async_graphql::{ErrorExtensions, FieldError};
use simulacra_catalog::CatalogError;

pub fn to_field_error(err: CatalogError) -> FieldError {
    let (code, message) = match &err {
        CatalogError::NotFound(message) => ("NOT_FOUND", message.clone()),
        CatalogError::Conflict(message) => ("CONFLICT", message.clone()),
        CatalogError::Validation(message) => ("VALIDATION", message.clone()),
        CatalogError::ReadOnly(message) => ("READ_ONLY", message.clone()),
        _ => ("INTERNAL", err.to_string()),
    };

    FieldError::new(message).extend_with(|_, ext| ext.set("code", code))
}
