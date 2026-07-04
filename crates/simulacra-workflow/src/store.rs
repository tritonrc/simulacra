use std::sync::Arc;

use simulacra_types::VirtualFs;

use crate::{WorkflowAgentResult, WorkflowError, WorkflowRun, WorkflowScript, WorkflowScriptMeta};

#[derive(Clone)]
pub struct WorkflowStore {
    vfs: Arc<dyn VirtualFs>,
}

impl WorkflowStore {
    pub fn new(vfs: Arc<dyn VirtualFs>) -> Self {
        Self { vfs }
    }

    pub fn vfs(&self) -> Arc<dyn VirtualFs> {
        Arc::clone(&self.vfs)
    }

    pub fn validate_script_path(path: &str) -> Result<(), WorkflowError> {
        if path.contains('\0') {
            return Err(WorkflowError::InvalidScriptPath {
                path: path.to_string(),
                reason: "path contains NUL byte".into(),
            });
        }
        if !path.ends_with(".mjs") {
            return Err(WorkflowError::InvalidScriptPath {
                path: path.to_string(),
                reason: "workflow scripts must use .mjs extension".into(),
            });
        }
        let allowed_root =
            path.starts_with("/workflows/") || path.starts_with("/var/workflows/runs/");
        if !allowed_root {
            return Err(WorkflowError::InvalidScriptPath {
                path: path.to_string(),
                reason: "workflow path must be under /workflows/ or /var/workflows/runs/".into(),
            });
        }
        if path.split('/').any(|segment| segment == "..") {
            return Err(WorkflowError::InvalidScriptPath {
                path: path.to_string(),
                reason: "path traversal is not allowed".into(),
            });
        }
        Ok(())
    }

    pub fn script_path_for_name(name: &str) -> Result<String, WorkflowError> {
        if name.is_empty()
            || name.contains('/')
            || name.contains('\\')
            || name.contains('\0')
            || name == "."
            || name == ".."
        {
            return Err(WorkflowError::InvalidScriptPath {
                path: name.to_string(),
                reason: "workflow name must be a single path segment".into(),
            });
        }
        Ok(format!("/workflows/{name}.mjs"))
    }

    pub fn inline_script_path(run_id: &str) -> String {
        format!("/var/workflows/runs/{run_id}/workflow.mjs")
    }

    pub fn transcript_dir(run_id: &str) -> String {
        format!("/var/workflows/runs/{run_id}/agents")
    }

    pub fn result_path(run_id: &str, key: &str) -> String {
        let label = sanitize_label(key);
        format!("{}/{}.json", Self::transcript_dir(run_id), label)
    }

    pub fn state_path(run_id: &str) -> String {
        format!("/var/workflows/runs/{run_id}/state.json")
    }

    pub fn persist_inline_script(
        &self,
        run_id: &str,
        source: &str,
    ) -> Result<String, WorkflowError> {
        let path = Self::inline_script_path(run_id);
        self.vfs.write(&path, source.as_bytes())?;
        Ok(path)
    }

    pub fn read_script(&self, path: &str) -> Result<String, WorkflowError> {
        Self::validate_script_path(path)?;
        let bytes = self.vfs.read(path)?;
        String::from_utf8(bytes)
            .map_err(|e| WorkflowError::InvalidScript(format!("workflow script is not UTF-8: {e}")))
    }

    pub fn load_script(
        &self,
        path: String,
        source: String,
        meta: WorkflowScriptMeta,
    ) -> WorkflowScript {
        WorkflowScript { path, source, meta }
    }

    pub fn save_run(&self, run: &WorkflowRun) -> Result<(), WorkflowError> {
        let data = serde_json::to_vec_pretty(run)?;
        self.vfs.write(&Self::state_path(&run.run_id), &data)?;
        Ok(())
    }

    pub fn read_run(&self, run_id: &str) -> Result<WorkflowRun, WorkflowError> {
        let bytes = self.vfs.read(&Self::state_path(run_id))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn save_agent_result(
        &self,
        run_id: &str,
        result: &WorkflowAgentResult,
    ) -> Result<(), WorkflowError> {
        let data = serde_json::to_vec_pretty(result)?;
        self.vfs
            .write(&Self::result_path(run_id, &result.key), &data)?;
        Ok(())
    }

    pub fn read_agent_result(
        &self,
        run_id: &str,
        key: &str,
    ) -> Result<WorkflowAgentResult, WorkflowError> {
        let bytes = self.vfs.read(&Self::result_path(run_id, key))?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

fn sanitize_label(label: &str) -> String {
    label
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}
