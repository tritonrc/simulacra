use std::fs;
use std::path::{Path, PathBuf};

use simulacra_cli::{CliArgs, CliMode, OutputFormat, bootstrap};
use simulacra_types::VirtualFs;
use tempfile::TempDir;

struct TempConfig {
    _dir: TempDir,
    path: PathBuf,
}

impl TempConfig {
    fn write(contents: &str) -> Self {
        let dir = tempfile::tempdir().expect("temp config dir should be created");
        let path = dir.path().join("simulacra.toml");
        fs::write(&path, contents).expect("temp config should be written");
        Self { _dir: dir, path }
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("cli crate should live under <repo>/crates/simulacra-cli")
        .to_path_buf()
}

fn bootstrap_args(config_path: String) -> CliArgs {
    CliArgs {
        config_path,
        task: Some("verify dev config source mounts".to_string()),
        mode: Some(CliMode::Headless),
        verbose: false,
        otlp_endpoint: None,
        session: None,
        model: None,
        max_turns: None,
        max_tokens: None,
        max_cost: None,
        no_catalog: true,
        output_format: OutputFormat::Jsonl,
    }
}

#[test]
fn dev_style_config_mounts_source_files_under_workspace_without_build_outputs() {
    let root = repo_root();
    let config = TempConfig::write(&dev_workspace_config(&root));

    let boot = bootstrap(&bootstrap_args(config.path.to_string_lossy().into_owned()))
        .expect("repo dev config should bootstrap with source mounts");

    assert!(
        boot.vfs
            .exists("/workspace/crates/simulacra-shell/src/lib.rs"),
        "source files should be visible under /workspace for coding-agent searches"
    );
    assert!(
        boot.vfs.exists("/workspace/specs/S020-vfs-host-mounts.md"),
        "spec files should be visible under /workspace"
    );
    assert!(
        !boot
            .vfs
            .exists("/workspace/crates/simulacra-mcp/tests/fixtures/sources/echo-mcp/target"),
        "dev config should not mount nested build output into the VFS workspace"
    );
}

fn dev_workspace_config(root: &Path) -> String {
    format!(
        r#"
[project]
name = "simulacra-dev-test"

[agent_types.default]
model = "claude-sonnet-4-6"

[agent_types.default.capabilities]
paths_read = ["/**"]
paths_write = ["/**"]

[vfs]
auto_mount_skills = true

[[vfs.mounts]]
source = "{}"
target = "/workspace/crates/simulacra-shell"

[[vfs.mounts]]
source = "{}"
target = "/workspace/crates/simulacra-mcp/src"

[[vfs.mounts]]
source = "{}"
target = "/workspace/specs"
"#,
        root.join("crates/simulacra-shell").display(),
        root.join("crates/simulacra-mcp/src").display(),
        root.join("specs").display()
    )
}
