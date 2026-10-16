#![cfg(feature = "sandbox")]

//! `file_read` hands bytes to a host-supplied presenter and offers a byte
//! window; the file tools can be built by a host instead of only registered.

use std::sync::{Arc, Mutex};

use rust_decimal::Decimal;
use serde_json::{Value, json};
use simulacra_sandbox::AgentCell;
use simulacra_tool::{
    FilePresenter, FileReadRequest, PresentFuture, ToolRegistry, file_tools, register_file_tools,
    register_file_tools_with_presenter,
};
use simulacra_types::{
    AgentId, CapabilityToken, CheckpointData, JOURNAL_SCHEMA_VERSION, JournalEntry,
    JournalEntryKind, JournalError, JournalStorage, PathPattern, ResourceBudget, TokenUsage,
    ToolError, ToolOutput, VirtualFs,
};
use simulacra_vfs::MemoryFs;

/// Records every request and the exact bytes it was handed.
struct Recording {
    seen: Mutex<Vec<(FileReadRequest, Vec<u8>)>>,
}

impl Recording {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
        })
    }
}

impl FilePresenter for Recording {
    fn present<'a>(&'a self, request: FileReadRequest, bytes: Vec<u8>) -> PresentFuture<'a> {
        Box::pin(async move {
            self.seen.lock().unwrap().push((request, bytes));
            Ok(ToolOutput::success("presented"))
        })
    }
}

#[derive(Default)]
struct NullJournal {
    entries: Mutex<Vec<JournalEntry>>,
}

impl JournalStorage for NullJournal {
    fn append(&self, entry: JournalEntry) -> Result<(), JournalError> {
        self.entries.lock().unwrap().push(entry);
        Ok(())
    }

    fn read_all(&self, agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| entry.agent_id == *agent_id)
            .cloned()
            .collect())
    }

    fn query_token_usage(&self, _agent_id: &AgentId) -> Result<TokenUsage, JournalError> {
        Ok(TokenUsage::default())
    }

    fn save_checkpoint(
        &self,
        agent_id: &AgentId,
        _after_entry: usize,
        data: CheckpointData,
    ) -> Result<(), JournalError> {
        let snapshot_data =
            serde_json::to_vec(&data).map_err(|error| JournalError::Storage(error.to_string()))?;
        self.append(JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: agent_id.clone(),
            timestamp_ms: 0,
            entry: JournalEntryKind::Checkpoint { snapshot_data },
        })
    }

    fn fork_from(
        &self,
        agent_id: &AgentId,
        checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self.read_all(agent_id)?;
        if checkpoint_idx >= entries.len() {
            return Err(JournalError::InvalidCheckpointIndex(checkpoint_idx));
        }
        Ok(entries[..=checkpoint_idx].to_vec())
    }

    fn read_from(
        &self,
        agent_id: &AgentId,
        start_index: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self.read_all(agent_id)?;
        if start_index > entries.len() {
            return Err(JournalError::InvalidCheckpointIndex(start_index));
        }
        Ok(entries[start_index..].to_vec())
    }
}

fn capability() -> CapabilityToken {
    CapabilityToken {
        paths_read: vec![PathPattern("/**".into())],
        paths_write: vec![PathPattern("/**".into())],
        ..Default::default()
    }
}

/// A cell over a fresh filesystem that already holds `files`.
fn cell_with(files: &[(&str, &[u8])]) -> Arc<AgentCell> {
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    for (path, bytes) in files {
        vfs.write(path, bytes).unwrap();
    }
    let http_client: Arc<dyn simulacra_http::HttpClient> =
        Arc::new(simulacra_http::UreqHttpClient::default());
    Arc::new(AgentCell::new(
        vfs,
        capability(),
        Arc::new(Mutex::new(ResourceBudget::new(0, 0, Decimal::ZERO, 0))),
        Arc::new(NullJournal::default()),
        http_client,
    ))
}

fn default_registry(files: &[(&str, &[u8])]) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_file_tools(&mut registry, cell_with(files)).unwrap();
    registry
}

fn presenting_registry(files: &[(&str, &[u8])], presenter: Arc<dyn FilePresenter>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_file_tools_with_presenter(&mut registry, cell_with(files), presenter).unwrap();
    registry
}

async fn read(registry: &ToolRegistry, arguments: Value) -> Result<ToolOutput, ToolError> {
    registry
        .call_output("file_read", arguments, &capability())
        .await
}

#[tokio::test]
async fn file_read_hands_the_whole_file_and_the_request_to_the_host_presenter() {
    let presenter = Recording::new();
    let registry = presenting_registry(&[("/notes/a.txt", b"hello")], presenter.clone());

    let output = read(
        &registry,
        json!({"path": "/notes/a.txt", "offset": 1, "limit": 3}),
    )
    .await
    .unwrap();

    assert_eq!(output.content, "presented");
    assert!(!output.is_error);
    // The window is the presenter's to apply: it gets all five bytes, not "ell".
    assert_eq!(
        *presenter.seen.lock().unwrap(),
        vec![(
            FileReadRequest {
                path: "/notes/a.txt".into(),
                offset: Some(1),
                limit: Some(3),
            },
            b"hello".to_vec(),
        )]
    );
}

#[tokio::test]
async fn a_request_without_a_window_reaches_the_presenter_with_none() {
    let presenter = Recording::new();
    let registry = presenting_registry(&[("/a.txt", b"xy")], presenter.clone());

    read(&registry, json!({"path": "/a.txt", "offset": null}))
        .await
        .unwrap();

    assert_eq!(
        *presenter.seen.lock().unwrap(),
        vec![(
            FileReadRequest {
                path: "/a.txt".into(),
                offset: None,
                limit: None,
            },
            b"xy".to_vec(),
        )]
    );
}

#[tokio::test]
async fn the_default_presenter_returns_the_requested_window_decoded_lossily() {
    let registry = default_registry(&[("/w.txt", b"hello world"), ("/bad.bin", &[0xff, b'a'])]);

    let window = read(
        &registry,
        json!({"path": "/w.txt", "offset": 6, "limit": 5}),
    )
    .await
    .unwrap();
    assert_eq!(window.content, "world");

    let whole = read(&registry, json!({"path": "/w.txt"})).await.unwrap();
    assert_eq!(whole.content, "hello world");

    let offset_only = read(&registry, json!({"path": "/w.txt", "offset": 6}))
        .await
        .unwrap();
    assert_eq!(offset_only.content, "world");

    let limit_only = read(&registry, json!({"path": "/w.txt", "limit": 5}))
        .await
        .unwrap();
    assert_eq!(limit_only.content, "hello");

    // A limit longer than what remains is clamped, not an error.
    let clamped = read(
        &registry,
        json!({"path": "/w.txt", "offset": 6, "limit": 500}),
    )
    .await
    .unwrap();
    assert_eq!(clamped.content, "world");

    // Past the end is an empty window, not an error.
    let past_end = read(&registry, json!({"path": "/w.txt", "offset": 50}))
        .await
        .unwrap();
    assert_eq!((past_end.content.as_str(), past_end.is_error), ("", false));

    let lossy = read(&registry, json!({"path": "/bad.bin"})).await.unwrap();
    assert_eq!(lossy.content, "\u{fffd}a");
}

#[tokio::test]
async fn offset_and_limit_must_be_integers_of_at_least_their_minimum() {
    let registry = default_registry(&[("/w.txt", b"hello world")]);

    for bad in [
        json!({"path": "/w.txt", "offset": -1}),
        json!({"path": "/w.txt", "offset": "3"}),
        json!({"path": "/w.txt", "limit": 1.5}),
        // A zero-byte window is a mistake, not a request for nothing.
        json!({"path": "/w.txt", "limit": 0}),
    ] {
        match read(&registry, bad.clone()).await {
            Err(ToolError::InvalidArguments(_)) => {}
            other => panic!("{bad} should be invalid arguments, got {other:?}"),
        }
    }

    // Offset zero is the start of the file, so it is allowed.
    let from_start = read(
        &registry,
        json!({"path": "/w.txt", "offset": 0, "limit": 5}),
    )
    .await
    .unwrap();
    assert_eq!(from_start.content, "hello");
}

#[tokio::test]
async fn a_bad_window_is_rejected_before_the_presenter_runs() {
    let presenter = Recording::new();
    let registry = presenting_registry(&[("/a.txt", b"xy")], presenter.clone());

    let result = read(&registry, json!({"path": "/a.txt", "limit": 0})).await;

    assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
    assert!(presenter.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_missing_file_is_reported_before_the_presenter_runs() {
    let presenter = Recording::new();
    let registry = presenting_registry(&[], presenter.clone());

    let output = read(&registry, json!({"path": "/nope"})).await.unwrap();

    assert!(output.is_error);
    assert_eq!(output.content, "not found: /nope");
    assert!(presenter.seen.lock().unwrap().is_empty());
}

#[test]
fn the_file_read_schema_offers_offset_and_limit() {
    let tools = file_tools(cell_with(&[]), Recording::new());

    let file_read = tools
        .iter()
        .map(|tool| tool.definition())
        .find(|definition| definition.name == "file_read")
        .expect("file_read is one of the file tools");

    let schema = file_read.input_schema;
    assert_eq!(schema["properties"]["offset"]["type"], json!("integer"));
    assert_eq!(schema["properties"]["limit"]["type"], json!("integer"));
    assert_eq!(schema["properties"]["path"]["type"], json!("string"));
    assert_eq!(schema["required"], json!(["path"]));
}

#[test]
fn file_tools_builds_the_four_file_tools_in_registration_order() {
    let names: Vec<String> = file_tools(cell_with(&[]), Recording::new())
        .iter()
        .map(|tool| tool.definition().name)
        .collect();

    assert_eq!(
        names,
        ["file_read", "file_write", "apply_patch", "list_dir"]
    );
}
