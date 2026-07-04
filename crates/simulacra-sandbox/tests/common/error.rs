use super::*;

#[allow(dead_code)]
#[derive(Debug)]
pub enum ExpectedSandboxError {
    CapabilityDenied(CapabilityDenied),
    BudgetExhausted {
        resource: String,
        used: String,
        limit: String,
    },
    Shell(String),
    Http(String),
    Js(String),
    Vfs(VfsError),
    Internal(String),
}

impl std::fmt::Display for ExpectedSandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CapabilityDenied(denied) => write!(f, "{denied}"),
            Self::BudgetExhausted {
                resource,
                used,
                limit,
            } => write!(
                f,
                "budget exhausted: {resource} — used {used}, limit {limit}"
            ),
            Self::Shell(message)
            | Self::Http(message)
            | Self::Js(message)
            | Self::Internal(message) => {
                write!(f, "{message}")
            }
            Self::Vfs(error) => write!(f, "{error}"),
        }
    }
}

pub fn sandbox_error_to_expected(error: simulacra_sandbox::SandboxError) -> ExpectedSandboxError {
    match error {
        simulacra_sandbox::SandboxError::CapabilityDenied(denied) => {
            ExpectedSandboxError::CapabilityDenied(denied)
        }
        simulacra_sandbox::SandboxError::BudgetExhausted(exhausted) => {
            ExpectedSandboxError::BudgetExhausted {
                resource: exhausted.resource,
                used: exhausted.used,
                limit: exhausted.limit,
            }
        }
        simulacra_sandbox::SandboxError::Shell(message) => ExpectedSandboxError::Shell(message),
        simulacra_sandbox::SandboxError::Http(message) => ExpectedSandboxError::Http(message),
        simulacra_sandbox::SandboxError::Js(message) => ExpectedSandboxError::Js(message),
        simulacra_sandbox::SandboxError::Vfs(vfs_err) => ExpectedSandboxError::Vfs(vfs_err),
        simulacra_sandbox::SandboxError::Internal(message) => {
            ExpectedSandboxError::Internal(message)
        }
    }
}
