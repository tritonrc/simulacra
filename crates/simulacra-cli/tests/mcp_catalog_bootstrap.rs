use std::fs;
use std::net::TcpListener;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread;
use std::time::Duration;

use simulacra_cli::{CliArgs, CliMode, OutputFormat, bootstrap};
use tempfile::TempDir;

struct PassiveMcpProbe {
    url: String,
    connections: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl PassiveMcpProbe {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("MCP probe should bind");
        listener
            .set_nonblocking(true)
            .expect("MCP probe should be nonblocking");
        let url = format!(
            "http://{}/mcp",
            listener
                .local_addr()
                .expect("MCP probe should expose a local address")
        );
        let connections = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let probe_connections = Arc::clone(&connections);
        let probe_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !probe_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((_stream, _peer)) => {
                        probe_connections.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            url,
            connections,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for PassiveMcpProbe {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct TempConfig {
    _dir: TempDir,
    path: std::path::PathBuf,
}

impl TempConfig {
    fn with_mcp(url: &str) -> Self {
        let dir = tempfile::tempdir().expect("temporary project should be created");
        let path = dir.path().join("simulacra.toml");
        fs::write(
            &path,
            format!(
                r#"[project]
name = "s057-catalog-bootstrap"

[agent_types.default]
model = "claude-sonnet-4-20250514"
max_turns = 4
max_tokens = 4096

[agent_types.default.capabilities]
mcp = ["mcp:github:*"]
paths_read = ["/workspace/**"]
paths_write = ["/workspace/**"]

[[mcp.servers]]
name = "github"
transport = "http"
url = "{url}"

[task]
entry_agent = "default"
task = "catalog bootstrap"
"#
            ),
        )
        .expect("temporary config should be written");
        Self { _dir: dir, path }
    }
}

#[test]
fn configured_mcp_bootstrap_exposes_only_stable_meta_tools_without_connecting() {
    let probe = PassiveMcpProbe::new();
    let config = TempConfig::with_mcp(&probe.url);

    let boot = bootstrap(&CliArgs {
        config_path: config.path.to_string_lossy().into_owned(),
        task: None,
        mode: Some(CliMode::Headless),
        verbose: false,
        otlp_endpoint: None,
        session: None,
        model: None,
        max_turns: None,
        max_tokens: None,
        max_cost: None,
        no_catalog: true,
        output_format: OutputFormat::Text,
    })
    .expect("configured MCP descriptors should not require a startup connection");

    thread::sleep(Duration::from_millis(25));
    assert_eq!(
        probe.connections.load(Ordering::SeqCst),
        0,
        "bootstrap must retain configured MCP descriptors without opening a network connection"
    );

    let mcp_definitions: Vec<_> = boot
        .tool_definitions
        .iter()
        .filter(|definition| definition.name.starts_with("mcp_"))
        .collect();
    assert_eq!(
        mcp_definitions.len(),
        2,
        "a configured MCP runtime must expose exactly the stable mcp_search and mcp_call tools"
    );
    assert!(
        mcp_definitions
            .iter()
            .any(|definition| definition.name == "mcp_search")
            && mcp_definitions
                .iter()
                .any(|definition| definition.name == "mcp_call"),
        "the initial provider toolset should contain the two fixed MCP meta-tools"
    );
    assert!(
        boot.tool_definitions
            .iter()
            .all(|definition| definition.name != "github_search"),
        "the initial provider toolset must not expose an inventory-derived MCP tool schema"
    );
}
