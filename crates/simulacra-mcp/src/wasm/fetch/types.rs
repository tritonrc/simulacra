use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum FetchError {
    #[error("capability denied: {0}")]
    CapabilityDenied(String),
    #[error("hook denied: {0}")]
    HookDenied(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("timeout")]
    Timeout,
}
