//! Simulacra shell crate.
//!
//! A virtual shell that executes parsed command lines against a [`simulacra_types::VirtualFs`].
//! All I/O goes through the VFS — no real file descriptors are touched.

mod awk;
mod builtins;
mod executor;
mod heredoc;
mod http_proxy;
mod parser;
mod redirects;
mod ripgrep;
mod search;
mod sleep;
mod text;

pub(crate) const DEV_NULL: &str = "/dev/null";

pub use executor::ShellExecutor;
pub use http_proxy::{ShellHttpError, ShellHttpProxy, ShellHttpResponse};
pub use parser::{
    Command, Pipeline, Redirect, RedirectKind, RedirectStream, RedirectTarget, ShellLine, parse,
};

/// Optional hook for commands that are not native shell builtins but should
/// still run inside the mediated sandbox instead of returning "not found".
pub trait ShellExternalCommand: Send + Sync {
    fn run_external(
        &self,
        program: &str,
        args: &[String],
        stdin: &str,
        cwd: &str,
    ) -> Option<CommandResult>;
}

/// Result of running a command or pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

impl CommandResult {
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: 0,
        }
    }

    pub fn error(exit_code: i32, stderr: impl Into<String>) -> Self {
        Self {
            stdout: String::new(),
            stderr: stderr.into(),
            exit_code,
        }
    }
}

#[cfg(test)]
mod tests;
