//! Python execution support: the `py_exec` admission helper, the mediated
//! `PythonShellDispatcher`, and the `python3` shell-command result builder.

use simulacra_types::{JOURNAL_SCHEMA_VERSION, JournalEntry, JournalEntryKind};

use crate::AgentCell;
use crate::SandboxError;
use crate::guards::reserve_turn;

impl AgentCell {
    /// Reserve one execution turn and journal a top-level Python code execution.
    ///
    /// `py_exec` lives in `simulacra-python`, but its admission control belongs to
    /// the same AgentCell budget/journal stream as shell and JavaScript.
    pub fn begin_python_execution(&self) -> Result<(), SandboxError> {
        reserve_turn(&self.budget, &self.journal, &self.agent_id)?;
        if let Err(err) = self.journal.append(JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: self.agent_id.clone(),
            timestamp_ms: 0,
            entry: JournalEntryKind::CodeExecution {
                language: "python".to_string(),
            },
        }) {
            tracing::error!(error = %err, "journal append failed for py_exec");
        }
        Ok(())
    }

    #[cfg(feature = "python")]
    pub(crate) fn python_shell_result(&self, code: &str) -> simulacra_shell::CommandResult {
        let runtime = simulacra_python_runtime::PythonRuntime::new(
            simulacra_python_runtime::PythonResourceLimits {
                max_duration: Some(std::time::Duration::from_secs(30)),
                max_recursion_depth: Some(1000),
                ..simulacra_python_runtime::PythonResourceLimits::default()
            },
        );
        let dispatcher = PythonShellDispatcher { cell: self };
        match runtime.execute(code, &dispatcher) {
            Ok(output) => simulacra_shell::CommandResult {
                stdout: output.stdout,
                stderr: String::new(),
                exit_code: 0,
            },
            Err(e) => simulacra_shell::CommandResult {
                stdout: String::new(),
                stderr: format!("{e}\n"),
                exit_code: 1,
            },
        }
    }
}

#[cfg(feature = "python")]
struct PythonShellDispatcher<'a> {
    cell: &'a AgentCell,
}

#[cfg(feature = "python")]
impl simulacra_python_runtime::ExternalDispatcher for PythonShellDispatcher<'_> {
    fn read_file(&self, path: &str) -> Result<String, String> {
        self.cell
            .read_file(path)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .map_err(|e| e.to_string())
    }

    fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
        self.cell
            .write_file(path, content.as_bytes())
            .map_err(|e| e.to_string())
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, String> {
        self.cell.list_dir(path).map_err(|e| e.to_string())
    }

    fn http_get(&self, url: &str) -> Result<String, String> {
        self.cell
            .fetch_http(url, "GET", &[], None, None)
            .map(|response| String::from_utf8_lossy(&response.body).into_owned())
            .map_err(|e| e.to_string())
    }

    fn http_post(&self, url: &str, body: &str) -> Result<String, String> {
        self.cell
            .fetch_http(url, "POST", &[], Some(body.as_bytes()), None)
            .map(|response| String::from_utf8_lossy(&response.body).into_owned())
            .map_err(|e| e.to_string())
    }

    fn env_get(&self, _name: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
}
