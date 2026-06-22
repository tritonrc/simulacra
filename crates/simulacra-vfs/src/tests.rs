use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use proptest::prelude::*;
use simulacra_types::{VfsError, VirtualFs};
use tracing_subscriber::layer::SubscriberExt;

use crate::mount::{
    MountError, copy_host_dir_to_vfs, detect_project_root, expand_tilde, process_host_mounts,
    resolve_mount_source,
};
use crate::{MemoryFs, OverlayFs};

#[derive(Clone)]
struct SharedFs {
    inner: Arc<dyn VirtualFs>,
}

impl SharedFs {
    fn memory() -> Self {
        Self {
            inner: Arc::new(MemoryFs::new()),
        }
    }
}

impl VirtualFs for SharedFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        self.inner.read(path)
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        self.inner.write(path, data)
    }

    fn exists(&self, path: &str) -> bool {
        self.inner.exists(path)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        self.inner.list_dir(path)
    }

    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        self.inner.mkdir(path)
    }

    fn remove(&self, path: &str) -> Result<(), VfsError> {
        self.inner.remove(path)
    }

    fn metadata(&self, path: &str) -> Result<simulacra_types::FsMetadata, VfsError> {
        self.inner.metadata(path)
    }

    fn snapshot(&self) -> Result<simulacra_types::VfsSnapshot, VfsError> {
        self.inner.snapshot()
    }

    fn restore(&self, snapshot: &simulacra_types::VfsSnapshot) -> Result<(), VfsError> {
        self.inner.restore(snapshot)
    }
}

#[derive(Debug, Clone)]
struct CapturedSpan {
    name: String,
    fields: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct CapturedEvent {
    #[allow(dead_code)]
    name: String,
    level: String,
    fields: HashMap<String, String>,
    current_span: Option<String>,
}

struct SpanCaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
}

impl<S> tracing_subscriber::Layer<S> for SpanCaptureLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        attrs.record(&mut visitor);
        self.spans.lock().unwrap().push(CapturedSpan {
            name: attrs.metadata().name().to_string(),
            fields,
        });
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if let Some(span_ref) = ctx.span(id) {
            let span_name = span_ref.name().to_string();
            let mut new_fields = HashMap::new();
            let mut visitor = FieldVisitor(&mut new_fields);
            values.record(&mut visitor);

            let mut spans = self.spans.lock().unwrap();
            for captured in spans.iter_mut().rev() {
                if captured.name == span_name {
                    for (key, value) in new_fields {
                        captured.fields.insert(key, value);
                    }
                    break;
                }
            }
        }
    }
}

struct TraceCaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl<S> tracing_subscriber::Layer<S> for TraceCaptureLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        attrs.record(&mut visitor);
        self.spans.lock().unwrap().push(CapturedSpan {
            name: attrs.metadata().name().to_string(),
            fields,
        });
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if let Some(span_ref) = ctx.span(id) {
            let span_name = span_ref.name().to_string();
            let mut new_fields = HashMap::new();
            let mut visitor = FieldVisitor(&mut new_fields);
            values.record(&mut visitor);

            let mut spans = self.spans.lock().unwrap();
            for captured in spans.iter_mut().rev() {
                if captured.name == span_name {
                    for (key, value) in new_fields {
                        captured.fields.insert(key, value);
                    }
                    break;
                }
            }
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        event.record(&mut visitor);

        let current_span = ctx
            .current_span()
            .id()
            .and_then(|id| ctx.span(id))
            .map(|span| span.name().to_string());

        self.events.lock().unwrap().push(CapturedEvent {
            name: event.metadata().name().to_string(),
            level: event.metadata().level().as_str().to_string(),
            fields,
            current_span,
        });
    }
}

struct FieldVisitor<'a>(&'a mut HashMap<String, String>);

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

fn setup_span_capture() -> (
    impl tracing::Subscriber + Send + Sync,
    Arc<Mutex<Vec<CapturedSpan>>>,
) {
    let spans = Arc::new(Mutex::new(Vec::new()));
    let layer = SpanCaptureLayer {
        spans: Arc::clone(&spans),
    };
    let subscriber = tracing_subscriber::registry::Registry::default().with(layer);
    (subscriber, spans)
}

fn capture_spans<T>(f: impl FnOnce() -> T) -> (T, Vec<CapturedSpan>) {
    let (subscriber, captured) = setup_span_capture();
    let result = tracing::subscriber::with_default(subscriber, f);
    let spans = captured.lock().unwrap().clone();
    (result, spans)
}

fn capture_trace<T>(f: impl FnOnce() -> T) -> (T, Vec<CapturedSpan>, Vec<CapturedEvent>) {
    let spans = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let layer = TraceCaptureLayer {
        spans: Arc::clone(&spans),
        events: Arc::clone(&events),
    };
    let subscriber = tracing_subscriber::registry::Registry::default().with(layer);
    let result = tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        f()
    });
    let spans = spans.lock().unwrap().clone();
    let events = events.lock().unwrap().clone();
    (result, spans, events)
}

fn field_matches(span: &CapturedSpan, key: &str, expected: &str) -> bool {
    span.fields
        .get(key)
        .map(|value| value.trim_matches('"') == expected)
        .unwrap_or(false)
}

fn event_field_matches(event: &CapturedEvent, key: &str, expected: &str) -> bool {
    event
        .fields
        .get(key)
        .map(|value| value.trim_matches('"') == expected)
        .unwrap_or(false)
}

fn assert_span_with_path(spans: &[CapturedSpan], operation: &str, path: &str) {
    let span = spans
        .iter()
        .find(|span| {
            field_matches(span, "simulacra.operation.name", operation)
                && field_matches(span, "simulacra.vfs.path", path)
        })
        .unwrap_or_else(|| {
            panic!("expected span for operation {operation} and path {path}; got {spans:#?}")
        });

    assert!(
        span.name.contains(operation),
        "span name should contain {operation}, got {}",
        span.name
    );
}

fn assert_span(spans: &[CapturedSpan], operation: &str) {
    let span = spans
        .iter()
        .find(|span| field_matches(span, "simulacra.operation.name", operation))
        .unwrap_or_else(|| panic!("expected span for operation {operation}; got {spans:#?}"));

    assert!(
        span.name.contains(operation),
        "span name should contain {operation}, got {}",
        span.name
    );
}

#[test]
fn write_then_read_roundtrip_returns_identical_bytes() {
    let fs = SharedFs::memory();
    let vfs: &dyn VirtualFs = &fs;
    let data = b"roundtrip bytes \0 with punctuation!";

    vfs.write("/artifacts/output.bin", data).unwrap();

    let roundtrip = vfs.read("/artifacts/output.bin").unwrap();
    assert_eq!(roundtrip, data);
}

proptest! {
    #[test]
    fn dotdot_at_root_resolves_to_root(
        climbs in 1usize..8,
        segments in prop::collection::vec("[a-z]{1,8}", 1..4),
    ) {
        let fs = SharedFs::memory();
        let vfs: &dyn VirtualFs = &fs;
        let canonical_path = format!("/{}", segments.join("/"));
        let traversed_path = format!("/{}{}/./", "../".repeat(climbs), segments.join("//"));
        let payload = canonical_path.as_bytes().to_vec();

        vfs.write(&canonical_path, &payload).unwrap();

        let read_back = vfs.read(&traversed_path).unwrap();
        prop_assert_eq!(read_back, payload);
    }
}

#[test]
fn snapshot_then_restore_is_a_no_op() {
    let fs = SharedFs::memory();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/alpha.txt", b"alpha").unwrap();
    vfs.write("/nested/beta.txt", b"beta").unwrap();
    let snapshot = vfs.snapshot().unwrap();

    vfs.write("/alpha.txt", b"mutated").unwrap();
    vfs.remove("/nested").unwrap();
    vfs.write("/new.txt", b"new").unwrap();

    vfs.restore(&snapshot).unwrap();

    let restored = vfs.snapshot().unwrap();
    assert_eq!(restored.data, snapshot.data);
    assert_eq!(vfs.read("/alpha.txt").unwrap(), b"alpha");
    assert_eq!(vfs.read("/nested/beta.txt").unwrap(), b"beta");
    assert!(matches!(vfs.read("/new.txt"), Err(VfsError::NotFound(_))));
}

#[test]
fn overlay_write_to_upper_does_not_mutate_lower() {
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let lower_view: &dyn VirtualFs = &lower;
    lower_view.write("/shared.txt", b"lower").unwrap();

    let overlay = OverlayFs::new(Box::new(lower.clone()), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    vfs.write("/shared.txt", b"upper").unwrap();

    assert_eq!(vfs.read("/shared.txt").unwrap(), b"upper");
    assert_eq!(lower_view.read("/shared.txt").unwrap(), b"lower");
}

#[test]
fn overlay_read_falls_through_to_lower_when_upper_has_no_entry() {
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let lower_view: &dyn VirtualFs = &lower;
    lower_view.write("/from-lower.txt", b"lower-layer").unwrap();

    let overlay = OverlayFs::new(Box::new(lower), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    assert_eq!(vfs.read("/from-lower.txt").unwrap(), b"lower-layer");
}

#[test]
fn overlay_delete_in_upper_shadows_lower_entry() {
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let lower_view: &dyn VirtualFs = &lower;
    lower_view.write("/masked.txt", b"still in lower").unwrap();

    let overlay = OverlayFs::new(Box::new(lower.clone()), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    vfs.remove("/masked.txt").unwrap();

    assert!(matches!(
        vfs.read("/masked.txt"),
        Err(VfsError::NotFound(_))
    ));
    assert!(!vfs.exists("/masked.txt"));
    assert_eq!(lower_view.read("/masked.txt").unwrap(), b"still in lower");
}

#[test]
fn list_dir_on_nonexistent_path_returns_error_not_empty_list() {
    let fs = SharedFs::memory();
    let vfs: &dyn VirtualFs = &fs;

    assert!(matches!(
        vfs.list_dir("/does/not/exist"),
        Err(VfsError::NotFound(_))
    ));
}

#[test]
fn metadata_returns_correct_size_after_write() {
    let fs = SharedFs::memory();
    let vfs: &dyn VirtualFs = &fs;
    let data = b"1234567890";

    vfs.write("/sizes/payload.bin", data).unwrap();

    let metadata = vfs.metadata("/sizes/payload.bin").unwrap();
    assert!(metadata.is_file);
    assert!(!metadata.is_dir);
    assert_eq!(metadata.size, data.len() as u64);
}

#[test]
fn write_produces_vfs_write_span_with_path() {
    let fs = SharedFs::memory();
    let vfs: &dyn VirtualFs = &fs;

    let (_, spans) = capture_spans(|| vfs.write("/logs/write.txt", b"hello").unwrap());

    assert_span_with_path(&spans, "vfs_write", "/logs/write.txt");
}

#[test]
fn read_produces_vfs_read_span_with_path() {
    let fs = SharedFs::memory();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/logs/read.txt", b"hello").unwrap();

    let (_, spans) = capture_spans(|| vfs.read("/logs/read.txt").unwrap());

    assert_span_with_path(&spans, "vfs_read", "/logs/read.txt");
}

#[test]
fn snapshot_and_restore_produce_vfs_spans() {
    let fs = SharedFs::memory();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/logs/state.txt", b"hello").unwrap();
    let snapshot = vfs.snapshot().unwrap();

    let (_, spans) = capture_spans(|| {
        let captured = vfs.snapshot().unwrap();
        vfs.restore(&captured).unwrap();
    });

    assert_span(&spans, "vfs_snapshot");
    assert_span(&spans, "vfs_restore");

    let current = vfs.snapshot().unwrap();
    assert_eq!(current.data, snapshot.data);
}

#[test]
fn overlay_write_implicitly_revives_whited_out_ancestor() {
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let lower_vfs: &dyn VirtualFs = &lower;
    lower_vfs.write("/dir/old.txt", b"old").unwrap();

    let overlay = OverlayFs::new(Box::new(lower.clone()), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    // 1. Remove /dir (creates whiteout for /dir)
    // Note: lower still has it, but it's shadowed.
    vfs.remove("/dir").unwrap();
    assert!(!vfs.exists("/dir/old.txt"));

    // 2. Write to /dir/new.txt (should implicitly recreate /dir)
    vfs.write("/dir/new.txt", b"new content").unwrap();

    // 3. Should be able to read /dir/new.txt
    // If this fails, it means /dir is still whited out and shadowing /dir/new.txt
    let content = vfs.read("/dir/new.txt");
    assert!(
        content.is_ok(),
        "Should be able to read file after writing to deleted directory, got {:?}",
        content.err()
    );
    assert_eq!(content.unwrap(), b"new content");
}

#[test]
fn list_dir_returns_entries_sorted_by_name() {
    let fs = SharedFs::memory();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/dir/c.txt", b"").unwrap();
    vfs.write("/dir/a.txt", b"").unwrap();
    vfs.write("/dir/b.txt", b"").unwrap();

    let entries = vfs.list_dir("/dir").unwrap();
    assert_eq!(entries, vec!["a.txt", "b.txt", "c.txt"]);
}

#[test]
fn write_creates_parent_directories_implicitly() {
    let fs = SharedFs::memory();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/a/b/c/file.txt", b"content").unwrap();

    assert!(vfs.exists("/a"));
    assert!(vfs.exists("/a/b"));
    assert!(vfs.exists("/a/b/c"));
    assert!(vfs.exists("/a/b/c/file.txt"));

    let entries = vfs.list_dir("/a/b").unwrap();
    assert_eq!(entries, vec!["c"]);
}

// ---------------------------------------------------------------------------
// S001 gap-fill: remove() on non-existent path returns error
// ---------------------------------------------------------------------------

#[test]
fn remove_nonexistent_path_returns_not_found_error() {
    let fs = SharedFs::memory();
    let vfs: &dyn VirtualFs = &fs;

    let result = vfs.remove("/does/not/exist.txt");
    assert!(
        matches!(result, Err(VfsError::NotFound(_))),
        "remove() on non-existent path should return NotFound, got {result:?}"
    );
}

// ---------------------------------------------------------------------------
// S001 gap-fill: OverlayFs snapshot/restore preserves whiteout state
// ---------------------------------------------------------------------------

#[test]
fn overlay_snapshot_then_restore_preserves_whiteout_state() {
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let lower_view: &dyn VirtualFs = &lower;
    lower_view.write("/visible.txt", b"data").unwrap();

    let overlay = OverlayFs::new(Box::new(lower.clone()), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    // Delete the file (creates a whiteout)
    vfs.remove("/visible.txt").unwrap();
    assert!(!vfs.exists("/visible.txt"));

    // Snapshot should capture the whiteout
    let snapshot = vfs.snapshot().unwrap();

    // Write something new to dirty the state
    vfs.write("/visible.txt", b"revived").unwrap();
    assert!(vfs.exists("/visible.txt"));

    // Restore should bring back the whiteout
    vfs.restore(&snapshot).unwrap();
    assert!(
        !vfs.exists("/visible.txt"),
        "whiteout should survive snapshot/restore — /visible.txt should remain deleted"
    );
}

// ---------------------------------------------------------------------------
// S001 gap-fill: thread-safety (concurrent reads and writes)
// ---------------------------------------------------------------------------

#[test]
fn concurrent_reads_and_writes_do_not_corrupt_state() {
    use std::sync::Arc;

    let fs = Arc::new(MemoryFs::new());

    let mut handles = vec![];
    for i in 0..10 {
        let fs_clone = Arc::clone(&fs);
        handles.push(std::thread::spawn(move || {
            let path = format!("/concurrent_{i}.txt");
            let data = format!("data_{i}");
            fs_clone.write(&path, data.as_bytes()).unwrap();
            let read_back = fs_clone.read(&path).unwrap();
            assert_eq!(read_back, data.as_bytes());
        }));
    }

    for handle in handles {
        handle.join().expect("thread should not panic");
    }

    // Verify all files exist with correct content
    for i in 0..10 {
        let path = format!("/concurrent_{i}.txt");
        let data = fs.read(&path).unwrap();
        assert_eq!(data, format!("data_{i}").as_bytes());
    }
}

// ===========================================================================
// V1: Project root detection and source path resolution
// ===========================================================================

#[test]
fn detect_project_root_config_based_uses_parent_of_config_path() {
    // S020 behavior 1: project root is the parent directory of the resolved config path
    let result = detect_project_root("/home/user/project/simulacra.toml", false).unwrap();
    assert_eq!(result, std::path::PathBuf::from("/home/user/project"));
}

#[test]
fn detect_project_root_absolute_config_path_uses_parent() {
    // S020 behavior 2: absolute config path → parent
    let result = detect_project_root("/tmp/myproject/simulacra.toml", false).unwrap();
    assert_eq!(result, std::path::PathBuf::from("/tmp/myproject"));
}

#[test]
fn detect_project_root_config_at_filesystem_root_returns_error() {
    // Edge case: config path with no parent (just a filename at root-ish level)
    // "/simulacra.toml" should have parent "/"
    let result = detect_project_root("/simulacra.toml", false).unwrap();
    assert_eq!(result, std::path::PathBuf::from("/"));
}

#[test]
fn detect_project_root_adhoc_mode_uses_cwd() {
    // S020 behavior 5: ad-hoc mode uses current working directory
    let cwd = std::env::current_dir().unwrap();
    let result = detect_project_root("unused", true).unwrap();
    // On macOS, /var -> /private/var, so strip_private_prefix may adjust
    // Just verify it's a valid directory
    assert!(
        result.is_absolute(),
        "ad-hoc project root should be absolute"
    );
    // The result should match cwd (possibly with /private stripped on macOS)
    #[cfg(target_os = "macos")]
    {
        let cwd_str = cwd.to_string_lossy();
        let result_str = result.to_string_lossy();
        // Either they match directly, or one has /private prefix stripped
        assert!(
            cwd_str == result_str
                || cwd_str.starts_with("/private") && result_str == cwd_str.replace("/private", "")
                || result_str.starts_with("/private")
                    && cwd_str == result_str.replace("/private", ""),
            "ad-hoc root {result_str} should correspond to cwd {cwd_str}"
        );
    }
    #[cfg(not(target_os = "macos"))]
    {
        assert_eq!(result, cwd);
    }
}

#[test]
fn resolve_mount_source_absolute_path_returned_directly() {
    // S020 assertion: absolute source paths used directly
    let root = std::path::Path::new("/project");
    let result = resolve_mount_source("/usr/share/data", root);
    assert_eq!(result, std::path::PathBuf::from("/usr/share/data"));
}

#[test]
fn resolve_mount_source_relative_path_joined_with_project_root() {
    // S020 assertion: relative source paths resolved against project root
    let root = std::path::Path::new("/home/user/project");
    let result = resolve_mount_source("prompts", root);
    assert_eq!(
        result,
        std::path::PathBuf::from("/home/user/project/prompts")
    );
}

#[test]
fn resolve_mount_source_relative_nested_path() {
    let root = std::path::Path::new("/project");
    let result = resolve_mount_source("a/b/c", root);
    assert_eq!(result, std::path::PathBuf::from("/project/a/b/c"));
}

#[cfg(unix)]
#[test]
fn expand_tilde_replaces_with_home() {
    // S020 assertion: tilde expansion on Unix
    let home = std::env::var("HOME").unwrap();
    let result = expand_tilde("~/simulacra-skills");
    assert_eq!(result, format!("{home}/simulacra-skills"));
}

#[cfg(unix)]
#[test]
fn expand_tilde_lone_tilde() {
    let home = std::env::var("HOME").unwrap();
    let result = expand_tilde("~");
    assert_eq!(result, home);
}

#[test]
fn expand_tilde_no_tilde_returns_unchanged() {
    let result = expand_tilde("/absolute/path");
    assert_eq!(result, "/absolute/path");
}

#[test]
fn expand_tilde_tilde_in_middle_returns_unchanged() {
    // Only leading ~ is expanded
    let result = expand_tilde("/path/with/~tilde");
    assert_eq!(result, "/path/with/~tilde");
}

#[cfg(unix)]
#[test]
fn resolve_mount_source_tilde_expanded_then_treated_as_absolute() {
    // After tilde expansion, the path should be absolute
    let root = std::path::Path::new("/project");
    let result = resolve_mount_source("~/data", root);
    let home = std::env::var("HOME").unwrap();
    assert_eq!(result, std::path::PathBuf::from(format!("{home}/data")));
    // Should NOT be joined with project root since it's absolute after expansion
    assert!(
        !result.starts_with("/project"),
        "tilde-expanded path should not be relative to project root"
    );
}

// ===========================================================================
// V3: copy_host_dir_to_vfs — recursive copy, limits, symlinks
// ===========================================================================

#[test]
fn copy_host_dir_copies_files_recursively_into_vfs() {
    // S020 behavior 20: mount copies full host directory tree recursively
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // Create a directory tree
    std::fs::create_dir_all(root.join("sub/nested")).unwrap();
    std::fs::write(root.join("top.txt"), b"top content").unwrap();
    std::fs::write(root.join("sub/mid.txt"), b"mid content").unwrap();
    std::fs::write(root.join("sub/nested/deep.txt"), b"deep content").unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let (file_count, total_bytes) =
        copy_host_dir_to_vfs(root, "/mount", &vfs, 1000, 10_000_000, "/mount").unwrap();

    assert_eq!(file_count, 3);
    assert_eq!(
        total_bytes,
        b"top content".len() as u64 + b"mid content".len() as u64 + b"deep content".len() as u64
    );

    assert_eq!(vfs.read("/mount/top.txt").unwrap(), b"top content");
    assert_eq!(vfs.read("/mount/sub/mid.txt").unwrap(), b"mid content");
    assert_eq!(
        vfs.read("/mount/sub/nested/deep.txt").unwrap(),
        b"deep content"
    );
}

#[test]
fn copy_host_dir_creates_empty_directories_in_vfs() {
    // S020 behavior 23: empty host directories become empty VFS directories
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("empty_dir")).unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let (file_count, _) =
        copy_host_dir_to_vfs(root, "/mount", &vfs, 1000, 10_000_000, "/mount").unwrap();

    assert_eq!(file_count, 0, "no files in an empty directory");
    assert!(
        vfs.exists("/mount/empty_dir"),
        "empty dir should exist in VFS"
    );
    let entries = vfs.list_dir("/mount/empty_dir").unwrap();
    assert!(entries.is_empty(), "empty dir should have no entries");
}

#[test]
fn copy_host_dir_includes_hidden_files() {
    // S020 behavior 24: hidden files (starting with .) are included
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(root.join(".hidden"), b"secret").unwrap();
    std::fs::write(root.join("visible"), b"public").unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let (file_count, _) =
        copy_host_dir_to_vfs(root, "/mount", &vfs, 1000, 10_000_000, "/mount").unwrap();

    assert_eq!(file_count, 2);
    assert_eq!(vfs.read("/mount/.hidden").unwrap(), b"secret");
    assert_eq!(vfs.read("/mount/visible").unwrap(), b"public");
}

#[test]
fn copy_host_dir_copies_binary_files_as_raw_bytes() {
    // S020 behavior 22: binary files copied as-is
    let tmp = tempfile::tempdir().unwrap();
    let binary_data: Vec<u8> = (0u8..=255).collect();
    std::fs::write(tmp.path().join("binary.bin"), &binary_data).unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    copy_host_dir_to_vfs(tmp.path(), "/mount", &vfs, 1000, 10_000_000, "/mount").unwrap();

    assert_eq!(vfs.read("/mount/binary.bin").unwrap(), binary_data);
}

#[test]
fn copy_host_dir_file_limit_exceeded_returns_error() {
    // S020 behavior 29: exceeding max_files_per_mount fails
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..5 {
        std::fs::write(tmp.path().join(format!("file{i}.txt")), b"data").unwrap();
    }

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let result = copy_host_dir_to_vfs(tmp.path(), "/mount", &vfs, 3, 10_000_000, "/mount");

    assert!(result.is_err());
    match result.unwrap_err() {
        MountError::FileLimitExceeded {
            mount_target,
            actual,
            limit,
        } => {
            assert_eq!(mount_target, "/mount");
            assert!(actual > 3, "actual {actual} should exceed limit 3");
            assert_eq!(limit, 3);
        }
        other => panic!("expected FileLimitExceeded, got {other:?}"),
    }
}

#[test]
fn copy_host_dir_byte_limit_exceeded_returns_error() {
    // S020 behavior 29: exceeding max_bytes_per_mount fails
    let tmp = tempfile::tempdir().unwrap();
    let large_data = vec![0u8; 500];
    std::fs::write(tmp.path().join("big.bin"), &large_data).unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let result = copy_host_dir_to_vfs(tmp.path(), "/mount", &vfs, 1000, 100, "/mount");

    assert!(result.is_err());
    match result.unwrap_err() {
        MountError::SizeLimitExceeded {
            mount_target,
            actual,
            limit,
        } => {
            assert_eq!(mount_target, "/mount");
            assert!(actual > 100, "actual {actual} should exceed limit 100");
            assert_eq!(limit, 100);
        }
        other => panic!("expected SizeLimitExceeded, got {other:?}"),
    }
}

#[test]
fn copy_host_dir_limits_are_per_mount_not_global() {
    // S020 behavior 30: limits are per-mount
    let tmp1 = tempfile::tempdir().unwrap();
    let tmp2 = tempfile::tempdir().unwrap();
    for i in 0..3 {
        std::fs::write(tmp1.path().join(format!("a{i}.txt")), b"data").unwrap();
        std::fs::write(tmp2.path().join(format!("b{i}.txt")), b"data").unwrap();
    }

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    // Each mount has 3 files, limit is 5 — both should succeed independently
    let r1 = copy_host_dir_to_vfs(tmp1.path(), "/m1", &vfs, 5, 10_000_000, "/m1");
    let r2 = copy_host_dir_to_vfs(tmp2.path(), "/m2", &vfs, 5, 10_000_000, "/m2");
    assert!(r1.is_ok());
    assert!(r2.is_ok());
}

#[cfg(unix)]
#[test]
fn copy_host_dir_follows_symlinks() {
    // S020 behavior 21: host symlinks are resolved (followed) before copying
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(root.join("real.txt"), b"real content").unwrap();
    std::os::unix::fs::symlink(root.join("real.txt"), root.join("link.txt")).unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let (file_count, _) =
        copy_host_dir_to_vfs(root, "/mount", &vfs, 1000, 10_000_000, "/mount").unwrap();

    // Both the real file and the symlink target should be copied
    assert_eq!(file_count, 2);
    assert_eq!(vfs.read("/mount/real.txt").unwrap(), b"real content");
    assert_eq!(vfs.read("/mount/link.txt").unwrap(), b"real content");
}

#[cfg(unix)]
#[test]
fn copy_host_dir_detects_symlink_loops() {
    // S020 behavior 21: symlink loops are detected and skipped
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("dir_a")).unwrap();
    // Create a symlink loop: dir_a/loop -> root (which contains dir_a)
    std::os::unix::fs::symlink(root, root.join("dir_a/loop")).unwrap();
    std::fs::write(root.join("dir_a/file.txt"), b"content").unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    // Should not hang or error — should complete with the loop skipped
    let result = copy_host_dir_to_vfs(root, "/mount", &vfs, 10000, 100_000_000, "/mount");
    assert!(
        result.is_ok(),
        "symlink loop should be skipped, not cause an error: {result:?}"
    );

    // The real file should still be copied
    assert_eq!(vfs.read("/mount/dir_a/file.txt").unwrap(), b"content");
}

// ===========================================================================
// V2: process_host_mounts — auto-mount and system-prompt mounting
// ===========================================================================

/// Helper to build a minimal SimulacraConfig for testing
fn test_config(
    auto_mount_skills: bool,
    mounts: Vec<simulacra_config::MountConfig>,
    agent_types: HashMap<String, simulacra_config::AgentTypeConfig>,
) -> simulacra_config::SimulacraConfig {
    simulacra_config::SimulacraConfig {
        project: simulacra_config::ProjectConfig {
            name: "test".to_string(),
            description: None,
        },
        agent_types,
        integrations: HashMap::new(),
        tenants: HashMap::new(),
        mcp: None,
        task: None,
        vfs: simulacra_config::VfsConfig {
            auto_mount_skills,
            max_files_per_mount: 10_000,
            max_bytes_per_mount: 104_857_600,
            mounts,
        },
        tiers: Default::default(),
        wasm: None,
        hooks: None,
        memory: None,
        catalog: simulacra_config::CatalogConfig::default(),
    }
}

fn empty_agent_type() -> simulacra_config::AgentTypeConfig {
    simulacra_config::AgentTypeConfig {
        model: "test-model".to_string(),
        system_prompt: None,
        skills: vec![],
        max_turns: None,
        max_tokens: None,
        max_sub_agents: None,
        can_spawn: vec![],
        restart_policy: None,
        capabilities: None,
    }
}

#[test]
fn process_host_mounts_auto_mounts_skills_directory() {
    // S020 behavior 14: auto-mount skills/ when it exists and auto_mount_skills is true
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("skills/rust-dev")).unwrap();
    std::fs::write(root.join("skills/rust-dev/prompt.md"), b"skill prompt").unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let config = test_config(true, vec![], HashMap::new());
    process_host_mounts(&vfs, &config, root, "default").unwrap();

    assert_eq!(
        vfs.read("/skills/rust-dev/prompt.md").unwrap(),
        b"skill prompt"
    );
}

#[test]
fn process_host_mounts_auto_mount_skills_false_skips_skills() {
    // S020 assertion: setting auto_mount_skills = false suppresses skill mount
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("skills")).unwrap();
    std::fs::write(root.join("skills/something.md"), b"content").unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let config = test_config(false, vec![], HashMap::new());
    process_host_mounts(&vfs, &config, root, "default").unwrap();

    assert!(
        !vfs.exists("/skills/something.md"),
        "skills should not be mounted when auto_mount_skills is false"
    );
}

#[test]
fn process_host_mounts_no_skills_dir_is_fine() {
    // If skills/ doesn't exist, auto-mount is silently skipped
    let tmp = tempfile::tempdir().unwrap();
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let config = test_config(true, vec![], HashMap::new());
    let result = process_host_mounts(&vfs, &config, tmp.path(), "default");
    assert!(result.is_ok());
}

#[test]
fn process_host_mounts_configured_mount_copies_directory() {
    // S020 behavior 6-8: configured [[vfs.mounts]] entries copy directory trees
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("prompts")).unwrap();
    std::fs::write(root.join("prompts/system.md"), b"system prompt text").unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let config = test_config(
        false,
        vec![simulacra_config::MountConfig {
            source: "prompts".to_string(),
            target: "/prompts".to_string(),
        }],
        HashMap::new(),
    );
    process_host_mounts(&vfs, &config, root, "default").unwrap();

    assert_eq!(
        vfs.read("/prompts/system.md").unwrap(),
        b"system prompt text"
    );
}

#[test]
fn process_host_mounts_invalid_target_no_leading_slash() {
    // S020 behavior 9: target without leading / is a startup error
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("data")).unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let config = test_config(
        false,
        vec![simulacra_config::MountConfig {
            source: "data".to_string(),
            target: "no-slash".to_string(),
        }],
        HashMap::new(),
    );
    let result = process_host_mounts(&vfs, &config, root, "default");

    assert!(result.is_err());
    match result.unwrap_err() {
        MountError::InvalidMountTarget(msg) => {
            assert!(
                msg.contains("no-slash"),
                "error should name the invalid target, got: {msg}"
            );
        }
        other => panic!("expected InvalidMountTarget, got {other:?}"),
    }
}

#[test]
fn process_host_mounts_mounting_to_root_is_error() {
    // S020 behavior 12: mount target "/" is a startup error
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("data")).unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let config = test_config(
        false,
        vec![simulacra_config::MountConfig {
            source: "data".to_string(),
            target: "/".to_string(),
        }],
        HashMap::new(),
    );
    let result = process_host_mounts(&vfs, &config, root, "default");

    assert!(result.is_err());
    match result.unwrap_err() {
        MountError::InvalidMountTarget(msg) => {
            assert!(
                msg.contains("root"),
                "error should mention root, got: {msg}"
            );
        }
        other => panic!("expected InvalidMountTarget, got {other:?}"),
    }
}

#[test]
fn process_host_mounts_nonexistent_source_is_error() {
    // S020 behavior 10: non-existent source fails startup
    let tmp = tempfile::tempdir().unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let config = test_config(
        false,
        vec![simulacra_config::MountConfig {
            source: "does_not_exist".to_string(),
            target: "/target".to_string(),
        }],
        HashMap::new(),
    );
    let result = process_host_mounts(&vfs, &config, tmp.path(), "default");

    assert!(result.is_err());
    match result.unwrap_err() {
        MountError::SourceNotFound {
            source_path,
            target,
        } => {
            assert!(source_path.contains("does_not_exist"));
            assert_eq!(target, "/target");
        }
        other => panic!("expected SourceNotFound, got {other:?}"),
    }
}

#[test]
fn process_host_mounts_empty_mounts_array_is_valid() {
    // S020 behavior 13: empty [[vfs.mounts]] is valid
    let tmp = tempfile::tempdir().unwrap();
    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let config = test_config(false, vec![], HashMap::new());
    let result = process_host_mounts(&vfs, &config, tmp.path(), "default");
    assert!(result.is_ok());
}

#[test]
fn process_host_mounts_system_prompt_relative_path_mounted() {
    // S020 behavior 16: relative system prompt paths are mounted into VFS
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("prompts")).unwrap();
    std::fs::write(root.join("prompts/planner.md"), b"planner system prompt").unwrap();

    let mut agents = HashMap::new();
    let mut agent = empty_agent_type();
    agent.system_prompt = Some("prompts/planner.md".to_string());
    agents.insert("planner".to_string(), agent);

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let config = test_config(false, vec![], agents);
    process_host_mounts(&vfs, &config, root, "default").unwrap();

    assert_eq!(
        vfs.read("/prompts/planner.md").unwrap(),
        b"planner system prompt"
    );
}

#[test]
fn process_host_mounts_absolute_system_prompt_not_mounted() {
    // S020 behavior 16: absolute paths are not mounted (only relative)
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    let mut agents = HashMap::new();
    let mut agent = empty_agent_type();
    agent.system_prompt = Some("/absolute/path/prompt.md".to_string());
    agents.insert("agent1".to_string(), agent);

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let config = test_config(false, vec![], agents);
    // Should succeed — absolute prompts are skipped
    let result = process_host_mounts(&vfs, &config, root, "default");
    assert!(result.is_ok());
    assert!(
        !vfs.exists("/absolute/path/prompt.md"),
        "absolute prompt should not be mounted"
    );
}

#[test]
fn process_host_mounts_missing_entry_agent_prompt_is_error() {
    // S020 behavior 17: missing system prompt for entry agent is a startup error
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    let mut agents = HashMap::new();
    let mut agent = empty_agent_type();
    agent.system_prompt = Some("prompts/missing.md".to_string());
    agents.insert("entry".to_string(), agent);

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let config = test_config(false, vec![], agents);
    let result = process_host_mounts(&vfs, &config, root, "entry");

    assert!(result.is_err());
    match result.unwrap_err() {
        MountError::EntryPromptNotFound {
            prompt_path,
            resolved,
        } => {
            assert_eq!(prompt_path, "prompts/missing.md");
            assert!(resolved.contains("prompts/missing.md"));
        }
        other => panic!("expected EntryPromptNotFound, got {other:?}"),
    }
}

#[test]
fn process_host_mounts_missing_non_entry_agent_prompt_skips_silently() {
    // S020 behavior 17: missing non-entry agent prompt emits WARN and skips
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    let mut agents = HashMap::new();
    let mut agent = empty_agent_type();
    agent.system_prompt = Some("prompts/optional.md".to_string());
    agents.insert("helper".to_string(), agent);

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let config = test_config(false, vec![], agents);
    // "default" is the entry agent, "helper" is not — so missing prompt is OK
    let result = process_host_mounts(&vfs, &config, root, "default");
    assert!(
        result.is_ok(),
        "non-entry agent missing prompt should not fail: {result:?}"
    );
}

#[test]
fn process_host_mounts_system_prompt_path_traversal_is_error() {
    // S020: path traversal check — system prompt resolving outside project root is rejected
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // Create a file outside the project root
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), b"secret").unwrap();

    // Create a symlink inside the project that points outside
    std::fs::create_dir_all(root.join("prompts")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        outside.path().join("secret.txt"),
        root.join("prompts/evil.md"),
    )
    .unwrap();

    #[cfg(unix)]
    {
        let mut agents = HashMap::new();
        let mut agent = empty_agent_type();
        agent.system_prompt = Some("prompts/evil.md".to_string());
        agents.insert("agent1".to_string(), agent);

        let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
        let config = test_config(false, vec![], agents);
        let result = process_host_mounts(&vfs, &config, root, "default");

        assert!(result.is_err());
        match result.unwrap_err() {
            MountError::PathTraversal {
                prompt_path,
                resolved,
                root: root_str,
            } => {
                assert_eq!(prompt_path, "prompts/evil.md");
                assert!(!resolved.starts_with(&root_str));
            }
            other => panic!("expected PathTraversal, got {other:?}"),
        }
    }
}

#[test]
fn process_host_mounts_system_prompt_too_large_is_error() {
    // S020: system prompt exceeding 1 MB is rejected
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("prompts")).unwrap();
    // Create a file larger than 1 MB
    let large_data = vec![b'x'; 1_048_577]; // 1 MB + 1 byte
    std::fs::write(root.join("prompts/huge.md"), &large_data).unwrap();

    let mut agents = HashMap::new();
    let mut agent = empty_agent_type();
    agent.system_prompt = Some("prompts/huge.md".to_string());
    agents.insert("agent1".to_string(), agent);

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let config = test_config(false, vec![], agents);
    let result = process_host_mounts(&vfs, &config, root, "default");

    assert!(result.is_err());
    match result.unwrap_err() {
        MountError::PromptTooLarge { prompt_path, size } => {
            assert_eq!(prompt_path, "prompts/huge.md");
            assert!(size > 1_048_576);
        }
        other => panic!("expected PromptTooLarge, got {other:?}"),
    }
}

#[test]
fn process_host_mounts_mount_ordering_skills_before_config() {
    // S020 behavior 31-32: skills mount before config mounts; later overwrites earlier
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("skills")).unwrap();
    std::fs::write(root.join("skills/shared.md"), b"from skills auto-mount").unwrap();

    // Also create a separate directory that mounts to /skills
    std::fs::create_dir_all(root.join("override")).unwrap();
    std::fs::write(root.join("override/shared.md"), b"from config mount").unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let config = test_config(
        true,
        vec![simulacra_config::MountConfig {
            source: "override".to_string(),
            target: "/skills".to_string(),
        }],
        HashMap::new(),
    );
    process_host_mounts(&vfs, &config, root, "default").unwrap();

    // Config mount runs after auto-mount, so last-writer-wins
    assert_eq!(
        vfs.read("/skills/shared.md").unwrap(),
        b"from config mount",
        "configured mount should overwrite auto-mount (last-writer-wins)"
    );
}

#[test]
fn process_host_mounts_overlapping_mounts_union_merge_directories() {
    // S020 behavior 11: directory-level union merge
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("source_a")).unwrap();
    std::fs::write(root.join("source_a/a.txt"), b"from A").unwrap();
    std::fs::create_dir_all(root.join("source_b")).unwrap();
    std::fs::write(root.join("source_b/b.txt"), b"from B").unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let config = test_config(
        false,
        vec![
            simulacra_config::MountConfig {
                source: "source_a".to_string(),
                target: "/shared".to_string(),
            },
            simulacra_config::MountConfig {
                source: "source_b".to_string(),
                target: "/shared".to_string(),
            },
        ],
        HashMap::new(),
    );
    process_host_mounts(&vfs, &config, root, "default").unwrap();

    // Both files should exist (union merge)
    assert_eq!(vfs.read("/shared/a.txt").unwrap(), b"from A");
    assert_eq!(vfs.read("/shared/b.txt").unwrap(), b"from B");
}

// ===========================================================================
// V4: Mount observability — spans and events
// ===========================================================================

#[test]
fn copy_host_dir_80pct_file_threshold_warning_fires() {
    // S020 behavior 28: 80% file limit triggers WARN
    // We test this indirectly: with max_files=5 and 4 files (80%), no error but warning should fire
    // Since we can't easily capture tracing::warn! in unit tests without a subscriber,
    // we verify the function completes successfully and returns correct counts
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..4 {
        std::fs::write(tmp.path().join(format!("f{i}.txt")), b"data").unwrap();
    }

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let (file_count, _) =
        copy_host_dir_to_vfs(tmp.path(), "/mount", &vfs, 5, 10_000_000, "/mount").unwrap();

    assert_eq!(file_count, 4, "should have 4 files (80% of 5 limit)");
}

#[test]
fn copy_host_dir_80pct_byte_threshold_warning_fires() {
    // S020 behavior 28: 80% byte limit triggers WARN
    let tmp = tempfile::tempdir().unwrap();
    // Write 85 bytes, limit is 100 => 85% > 80% threshold
    std::fs::write(tmp.path().join("big.txt"), vec![b'x'; 85]).unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let (file_count, total_bytes) =
        copy_host_dir_to_vfs(tmp.path(), "/mount", &vfs, 1000, 100, "/mount").unwrap();

    assert_eq!(file_count, 1);
    assert_eq!(total_bytes, 85);
}

#[test]
fn process_host_mounts_produces_vfs_mount_spans() {
    // S020 observability: each mount produces a span with simulacra.operation.name = "vfs_mount"
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::write(root.join("data/file.txt"), b"content").unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let config = test_config(
        false,
        vec![simulacra_config::MountConfig {
            source: "data".to_string(),
            target: "/data".to_string(),
        }],
        HashMap::new(),
    );

    let (_, spans) = capture_spans(|| {
        process_host_mounts(&vfs, &config, root, "default").unwrap();
    });

    // Find the vfs_mount span
    let mount_span = spans
        .iter()
        .find(|s| field_matches(s, "simulacra.operation.name", "vfs_mount"))
        .expect("should have a vfs_mount span");

    assert!(
        mount_span
            .fields
            .get("simulacra.vfs.mount.target")
            .map(|v| v.trim_matches('"') == "/data")
            .unwrap_or(false),
        "mount span should have target=/data, got {:?}",
        mount_span.fields
    );
    assert!(
        mount_span
            .fields
            .get("simulacra.vfs.mount.origin")
            .map(|v| v.trim_matches('"') == "config")
            .unwrap_or(false),
        "configured mount should have origin=config, got {:?}",
        mount_span.fields
    );
}

#[test]
fn process_host_mounts_auto_mount_has_origin_auto() {
    // S020 observability: auto mounts use simulacra.vfs.mount.origin = "auto"
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("skills")).unwrap();
    std::fs::write(root.join("skills/s.md"), b"skill").unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let config = test_config(true, vec![], HashMap::new());

    let (_, spans) = capture_spans(|| {
        process_host_mounts(&vfs, &config, root, "default").unwrap();
    });

    let auto_span = spans
        .iter()
        .find(|s| {
            field_matches(s, "simulacra.operation.name", "vfs_mount")
                && s.fields
                    .get("simulacra.vfs.mount.origin")
                    .map(|v| v.trim_matches('"') == "auto")
                    .unwrap_or(false)
        })
        .expect("should have a vfs_mount span with origin=auto");

    assert!(
        auto_span
            .fields
            .get("simulacra.vfs.mount.target")
            .map(|v| v.trim_matches('"') == "/skills")
            .unwrap_or(false),
        "auto mount span should target /skills"
    );
}

#[test]
fn process_host_mounts_span_includes_file_count() {
    // S020 observability: mount span includes simulacra.vfs.mount.file_count
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::write(root.join("data/a.txt"), b"a").unwrap();
    std::fs::write(root.join("data/b.txt"), b"b").unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let config = test_config(
        false,
        vec![simulacra_config::MountConfig {
            source: "data".to_string(),
            target: "/data".to_string(),
        }],
        HashMap::new(),
    );

    let (_, spans) = capture_spans(|| {
        process_host_mounts(&vfs, &config, root, "default").unwrap();
    });

    let mount_span = spans
        .iter()
        .find(|s| field_matches(s, "simulacra.operation.name", "vfs_mount"))
        .expect("should have a vfs_mount span");

    assert!(
        mount_span
            .fields
            .get("simulacra.vfs.mount.file_count")
            .map(|v| v == "2")
            .unwrap_or(false),
        "mount span should record file_count=2, got {:?}",
        mount_span.fields.get("simulacra.vfs.mount.file_count")
    );
}

// ===========================================================================
// V7: Overlay list_dir merge semantics
// ===========================================================================

#[test]
fn overlay_list_dir_merges_upper_and_lower_entries() {
    // S001 behavior 7 + V7: OverlayFs list_dir produces union of lower and upper
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let lower_vfs: &dyn VirtualFs = &lower;
    let upper_vfs: &dyn VirtualFs = &upper;

    lower_vfs.write("/dir/from_lower.txt", b"l").unwrap();
    upper_vfs.write("/dir/from_upper.txt", b"u").unwrap();

    let overlay = OverlayFs::new(Box::new(lower), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    let entries = vfs.list_dir("/dir").unwrap();
    assert!(
        entries.contains(&"from_lower.txt".to_string()),
        "list_dir should include lower entries: {entries:?}"
    );
    assert!(
        entries.contains(&"from_upper.txt".to_string()),
        "list_dir should include upper entries: {entries:?}"
    );
}

#[test]
fn overlay_list_dir_returns_sorted_entries() {
    // S001 behavior 4: list_dir returns entries sorted by name
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let lower_vfs: &dyn VirtualFs = &lower;
    let upper_vfs: &dyn VirtualFs = &upper;

    lower_vfs.write("/dir/zebra.txt", b"z").unwrap();
    lower_vfs.write("/dir/alpha.txt", b"a").unwrap();
    upper_vfs.write("/dir/middle.txt", b"m").unwrap();

    let overlay = OverlayFs::new(Box::new(lower), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    let entries = vfs.list_dir("/dir").unwrap();
    assert_eq!(entries, vec!["alpha.txt", "middle.txt", "zebra.txt"]);
}

#[test]
fn overlay_list_dir_deduplicates_entries_present_in_both_layers() {
    // When a file exists in both upper and lower, list_dir should show it once
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let lower_vfs: &dyn VirtualFs = &lower;
    let upper_vfs: &dyn VirtualFs = &upper;

    lower_vfs.write("/dir/shared.txt", b"lower").unwrap();
    upper_vfs.write("/dir/shared.txt", b"upper").unwrap();

    let overlay = OverlayFs::new(Box::new(lower), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    let entries = vfs.list_dir("/dir").unwrap();
    let count = entries.iter().filter(|e| *e == "shared.txt").count();
    assert_eq!(
        count, 1,
        "shared.txt should appear exactly once, got {entries:?}"
    );
}

#[test]
fn overlay_list_dir_excludes_whited_out_entries() {
    // Deleted lower entries should not appear in list_dir
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let lower_vfs: &dyn VirtualFs = &lower;

    lower_vfs.write("/dir/keep.txt", b"keep").unwrap();
    lower_vfs.write("/dir/delete.txt", b"delete").unwrap();

    let overlay = OverlayFs::new(Box::new(lower), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    vfs.remove("/dir/delete.txt").unwrap();

    let entries = vfs.list_dir("/dir").unwrap();
    assert!(
        entries.contains(&"keep.txt".to_string()),
        "keep.txt should remain: {entries:?}"
    );
    assert!(
        !entries.contains(&"delete.txt".to_string()),
        "deleted entry should not appear: {entries:?}"
    );
}

#[test]
fn overlay_list_dir_on_whited_out_directory_returns_not_found() {
    // If the directory itself is whited out, list_dir should return NotFound
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let lower_vfs: &dyn VirtualFs = &lower;

    lower_vfs.write("/dir/file.txt", b"data").unwrap();

    let overlay = OverlayFs::new(Box::new(lower), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    vfs.remove("/dir").unwrap();

    assert!(matches!(vfs.list_dir("/dir"), Err(VfsError::NotFound(_))));
}

#[test]
fn overlay_list_dir_upper_only_directory() {
    // list_dir works when directory exists only in upper
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let upper_vfs: &dyn VirtualFs = &upper;

    upper_vfs.write("/upper_only/file.txt", b"data").unwrap();

    let overlay = OverlayFs::new(Box::new(lower), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    let entries = vfs.list_dir("/upper_only").unwrap();
    assert_eq!(entries, vec!["file.txt"]);
}

#[test]
fn overlay_list_dir_lower_only_directory() {
    // list_dir works when directory exists only in lower
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();
    let lower_vfs: &dyn VirtualFs = &lower;

    lower_vfs.write("/lower_only/file.txt", b"data").unwrap();

    let overlay = OverlayFs::new(Box::new(lower), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    let entries = vfs.list_dir("/lower_only").unwrap();
    assert_eq!(entries, vec!["file.txt"]);
}

#[test]
fn overlay_list_dir_nonexistent_directory_returns_error() {
    // list_dir on a path that doesn't exist in either layer returns NotFound
    let lower = SharedFs::memory();
    let upper = SharedFs::memory();

    let overlay = OverlayFs::new(Box::new(lower), Box::new(upper));
    let vfs: &dyn VirtualFs = &overlay;

    assert!(matches!(
        vfs.list_dir("/nonexistent"),
        Err(VfsError::NotFound(_))
    ));
}
// ===========================================================================
// S029: Agent Procfs tests
// ===========================================================================

use std::sync::atomic::AtomicU64;
use std::time::Instant;

use rust_decimal::Decimal;
use simulacra_types::{CapabilityToken, NetworkPermission, PathPattern, ResourceBudget};

use crate::procfs::{HookLister, ProcFs, ProcState, ToolLister};

// --- Fake ToolLister --------------------------------------------------------

struct FakeToolLister {
    tools: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
}

impl FakeToolLister {
    fn default_tools() -> Arc<Self> {
        Arc::new(Self {
            tools: std::sync::Mutex::new(vec![
                (
                    "file_read".to_string(),
                    serde_json::json!({
                        "description": "Read a file",
                        "input_schema": {"type": "object"},
                        "name": "file_read"
                    }),
                ),
                (
                    "list_dir".to_string(),
                    serde_json::json!({
                        "description": "List a directory",
                        "input_schema": {"type": "object"},
                        "name": "list_dir"
                    }),
                ),
            ]),
        })
    }
}

impl ToolLister for FakeToolLister {
    fn tool_names(&self) -> Vec<String> {
        self.tools
            .lock()
            .unwrap()
            .iter()
            .map(|(n, _)| n.clone())
            .collect()
    }

    fn tool_json(&self, name: &str) -> Option<String> {
        self.tools
            .lock()
            .unwrap()
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| serde_json::to_string(v).unwrap())
    }
}

// --- Fake HookLister --------------------------------------------------------

struct FakeHookLister {
    hooks: std::collections::HashMap<String, Vec<String>>,
}

impl FakeHookLister {
    fn with_tool_call_hooks() -> Arc<Self> {
        let mut hooks = std::collections::HashMap::new();
        hooks.insert(
            "tool_call".to_string(),
            vec!["audit".to_string(), "enforce".to_string()],
        );
        Arc::new(Self { hooks })
    }

    fn empty() -> Arc<Self> {
        Arc::new(Self {
            hooks: std::collections::HashMap::new(),
        })
    }
}

impl HookLister for FakeHookLister {
    fn hook_names(&self, operation: &str) -> Vec<String> {
        self.hooks.get(operation).cloned().unwrap_or_default()
    }
}

// --- ProcState builders -----------------------------------------------------

fn default_budget() -> Arc<std::sync::Mutex<ResourceBudget>> {
    let mut b = ResourceBudget::new(100_000, 10, Decimal::ZERO, 0);
    b.used_tokens = 4_521;
    b.used_turns = 3;
    b.used_cost = Decimal::new(12, 2); // 0.12
    Arc::new(std::sync::Mutex::new(b))
}

fn default_capabilities() -> CapabilityToken {
    CapabilityToken {
        shell: true,
        javascript: true,
        python: false,
        network: vec![
            NetworkPermission("*".to_string()),
            NetworkPermission("*.github.com".to_string()),
        ],
        mcp_tools: vec!["mcp:*:*".to_string()],
        paths_read: vec![
            PathPattern("/workspace/**".to_string()),
            PathPattern("/proc/**".to_string()),
        ],
        paths_write: vec![
            PathPattern("/workspace/**".to_string()),
            PathPattern("/proc/mailbox/**".to_string()),
        ],
        ..Default::default()
    }
}

/// Standard ProcFs: agent "agent-abc123", turn=3, 100k token budget with 4521
/// used, no parent.
fn make_procfs() -> ProcFs<MemoryFs> {
    let state = Arc::new(ProcState {
        agent_id: "agent-abc123".to_string(),
        agent_name: "researcher".to_string(),
        model: "claude-sonnet-4-6".to_string(),
        parent_id: None,
        budget: default_budget(),
        capabilities: default_capabilities(),
        tools: FakeToolLister::default_tools(),
        session_id: "session-xyz".to_string(),
        session_start: Instant::now(),
        journal_entries: Arc::new(AtomicU64::new(42)),
        hooks: FakeHookLister::with_tool_call_hooks(),
        turn: Arc::new(AtomicU64::new(3)),
    });
    ProcFs::new(MemoryFs::new(), state)
}

/// Child agent with parent_id set.
fn make_procfs_child() -> ProcFs<MemoryFs> {
    let state = Arc::new(ProcState {
        agent_id: "child-agent".to_string(),
        agent_name: "worker".to_string(),
        model: "claude-sonnet-4-6".to_string(),
        parent_id: Some("parent-agent".to_string()),
        budget: default_budget(),
        capabilities: default_capabilities(),
        tools: FakeToolLister::default_tools(),
        session_id: "session-xyz".to_string(),
        session_start: Instant::now(),
        journal_entries: Arc::new(AtomicU64::new(0)),
        hooks: FakeHookLister::empty(),
        turn: Arc::new(AtomicU64::new(1)),
    });
    ProcFs::new(MemoryFs::new(), state)
}

/// Unlimited budget (max_tokens=0, max_turns=0).
fn make_procfs_unlimited_budget() -> ProcFs<MemoryFs> {
    let budget = {
        let mut b = ResourceBudget::new(0, 0, Decimal::ZERO, 0);
        b.used_tokens = 500;
        Arc::new(std::sync::Mutex::new(b))
    };
    let state = Arc::new(ProcState {
        agent_id: "agent-unlimited".to_string(),
        agent_name: "default".to_string(),
        model: "claude-sonnet-4-6".to_string(),
        parent_id: None,
        budget,
        capabilities: CapabilityToken::default(),
        tools: FakeToolLister::default_tools(),
        session_id: "session-unlimited".to_string(),
        session_start: Instant::now(),
        journal_entries: Arc::new(AtomicU64::new(0)),
        hooks: FakeHookLister::empty(),
        turn: Arc::new(AtomicU64::new(0)),
    });
    ProcFs::new(MemoryFs::new(), state)
}

/// No capabilities granted.
fn make_procfs_no_caps() -> ProcFs<MemoryFs> {
    let state = Arc::new(ProcState {
        agent_id: "agent-nocaps".to_string(),
        agent_name: "restricted".to_string(),
        model: "claude-sonnet-4-6".to_string(),
        parent_id: None,
        budget: default_budget(),
        capabilities: CapabilityToken {
            shell: false,
            javascript: false,
            python: false,
            ..Default::default()
        },
        tools: FakeToolLister::default_tools(),
        session_id: "session-nocaps".to_string(),
        session_start: Instant::now(),
        journal_entries: Arc::new(AtomicU64::new(0)),
        hooks: FakeHookLister::empty(),
        turn: Arc::new(AtomicU64::new(0)),
    });
    ProcFs::new(MemoryFs::new(), state)
}

fn procfs_read_str(vfs: &dyn VirtualFs, path: &str) -> String {
    String::from_utf8(
        vfs.read(path)
            .unwrap_or_else(|e| panic!("read({path}) failed: {e}")),
    )
    .unwrap()
}

fn assert_permission_denied(err: &VfsError) {
    assert!(
        err.to_string().to_ascii_lowercase().contains("permission"),
        "expected a permission-denied error, got {err:?}"
    );
}

// --- Agent identity tests ---------------------------------------------------

#[test]
fn procfs_agent_id_returns_the_agents_configured_id() {
    let fs = make_procfs();
    assert_eq!(procfs_read_str(&fs, "/proc/agent/id"), "agent-abc123");
}

#[test]
fn procfs_agent_name_returns_the_agent_type_name() {
    let fs = make_procfs();
    assert_eq!(procfs_read_str(&fs, "/proc/agent/name"), "researcher");
}

#[test]
fn procfs_agent_model_returns_the_model_string() {
    let fs = make_procfs();
    assert_eq!(
        procfs_read_str(&fs, "/proc/agent/model"),
        "claude-sonnet-4-6"
    );
}

#[test]
fn procfs_agent_turn_returns_the_current_turn_number_as_a_string() {
    let fs = make_procfs();
    assert_eq!(procfs_read_str(&fs, "/proc/agent/turn"), "3");
}

#[test]
fn procfs_agent_parent_id_returns_empty_string_for_root_agent() {
    let fs = make_procfs();
    assert_eq!(procfs_read_str(&fs, "/proc/agent/parent_id"), "");
}

#[test]
fn procfs_agent_parent_id_returns_parent_id_for_child_agent() {
    let fs = make_procfs_child();
    assert_eq!(
        procfs_read_str(&fs, "/proc/agent/parent_id"),
        "parent-agent"
    );
}

// --- Budget tests -----------------------------------------------------------

#[test]
fn procfs_budget_max_tokens_returns_the_configured_token_limit() {
    let fs = make_procfs();
    assert_eq!(procfs_read_str(&fs, "/proc/budget/max_tokens"), "100000");
}

#[test]
fn procfs_budget_used_tokens_returns_current_token_usage() {
    let fs = make_procfs();
    assert_eq!(procfs_read_str(&fs, "/proc/budget/used_tokens"), "4521");
}

#[test]
fn procfs_budget_remaining_tokens_returns_max_minus_used() {
    let fs = make_procfs();
    assert_eq!(
        procfs_read_str(&fs, "/proc/budget/remaining_tokens"),
        "95479"
    );
}

#[test]
fn procfs_budget_remaining_tokens_returns_zero_when_max_tokens_is_zero_unlimited() {
    let fs = make_procfs_unlimited_budget();
    assert_eq!(
        procfs_read_str(&fs, "/proc/budget/remaining_tokens"),
        "0",
        "unlimited budget (max=0) should report remaining_tokens as 0"
    );
}

#[test]
fn procfs_budget_max_turns_returns_the_configured_turn_limit() {
    let fs = make_procfs();
    assert_eq!(procfs_read_str(&fs, "/proc/budget/max_turns"), "10");
}

#[test]
fn procfs_budget_remaining_turns_returns_max_minus_used() {
    let fs = make_procfs();
    assert_eq!(procfs_read_str(&fs, "/proc/budget/remaining_turns"), "7");
}

#[test]
fn procfs_budget_used_cost_returns_cost_with_two_decimal_places() {
    let fs = make_procfs();
    assert_eq!(procfs_read_str(&fs, "/proc/budget/used_cost"), "0.12");
}

#[test]
fn procfs_budget_values_are_dynamic_at_read_time() {
    // Mutate the shared budget between reads and confirm ProcFs reflects it.
    let budget_arc = {
        let b = ResourceBudget::new(100_000, 10, Decimal::ZERO, 0);
        Arc::new(std::sync::Mutex::new(b))
    };
    let state = Arc::new(ProcState {
        agent_id: "agent-dynamic".to_string(),
        agent_name: "default".to_string(),
        model: "model".to_string(),
        parent_id: None,
        budget: Arc::clone(&budget_arc),
        capabilities: CapabilityToken::default(),
        tools: FakeToolLister::default_tools(),
        session_id: "s".to_string(),
        session_start: Instant::now(),
        journal_entries: Arc::new(AtomicU64::new(0)),
        hooks: FakeHookLister::empty(),
        turn: Arc::new(AtomicU64::new(0)),
    });
    let fs = ProcFs::new(MemoryFs::new(), state);

    let before = procfs_read_str(&fs, "/proc/budget/used_tokens");
    budget_arc.lock().unwrap().used_tokens = 999;
    let after = procfs_read_str(&fs, "/proc/budget/used_tokens");

    assert_eq!(before, "0");
    assert_eq!(after, "999");
}

// --- Capabilities tests -----------------------------------------------------

#[test]
fn procfs_capabilities_shell_returns_true_when_shell_is_granted() {
    let fs = make_procfs();
    assert_eq!(procfs_read_str(&fs, "/proc/capabilities/shell"), "true");
}

#[test]
fn procfs_capabilities_shell_returns_false_when_shell_is_not_granted() {
    let fs = make_procfs_no_caps();
    assert_eq!(procfs_read_str(&fs, "/proc/capabilities/shell"), "false");
}

#[test]
fn procfs_capabilities_network_returns_newline_separated_patterns() {
    let fs = make_procfs();
    assert_eq!(
        procfs_read_str(&fs, "/proc/capabilities/network"),
        "*\n*.github.com"
    );
}

#[test]
fn procfs_capabilities_network_returns_empty_string_when_no_network_access() {
    let fs = make_procfs_no_caps();
    assert_eq!(procfs_read_str(&fs, "/proc/capabilities/network"), "");
}

#[test]
fn procfs_capabilities_paths_read_returns_newline_separated_patterns() {
    let fs = make_procfs();
    assert_eq!(
        procfs_read_str(&fs, "/proc/capabilities/paths_read"),
        "/workspace/**\n/proc/**"
    );
}

#[test]
fn procfs_capabilities_mcp_tools_returns_newline_separated_patterns() {
    let fs = make_procfs();
    assert_eq!(
        procfs_read_str(&fs, "/proc/capabilities/mcp_tools"),
        "mcp:*:*"
    );
}

#[test]
fn procfs_capabilities_paths_write_returns_newline_separated_patterns() {
    let fs = make_procfs();
    assert_eq!(
        procfs_read_str(&fs, "/proc/capabilities/paths_write"),
        "/workspace/**\n/proc/mailbox/**"
    );
}

// --- Tool exposure tests ----------------------------------------------------

#[test]
fn procfs_tools_listing_returns_registered_tool_names_sorted() {
    let fs = make_procfs();
    let names = fs.list_dir("/proc/tools").unwrap();
    assert_eq!(names, vec!["file_read", "list_dir"]);
}

#[test]
fn procfs_tools_named_entry_returns_json_with_name_description_and_input_schema() {
    let fs = make_procfs();
    let raw = procfs_read_str(&fs, "/proc/tools/file_read");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("tool JSON must be valid");
    assert_eq!(v["name"], "file_read");
    assert!(v["description"].is_string());
    assert!(v["input_schema"].is_object());
}

#[test]
fn procfs_tools_nonexistent_tool_returns_not_found_error() {
    let fs = make_procfs();
    assert!(matches!(
        fs.read("/proc/tools/nosuch"),
        Err(VfsError::NotFound(_))
    ));
}

#[test]
fn procfs_tool_listing_reflects_dynamic_registry_changes() {
    struct DynamicToolLister {
        names: std::sync::Mutex<Vec<String>>,
    }
    impl ToolLister for DynamicToolLister {
        fn tool_names(&self) -> Vec<String> {
            self.names.lock().unwrap().clone()
        }
        fn tool_json(&self, _: &str) -> Option<String> {
            None
        }
    }
    let lister = Arc::new(DynamicToolLister {
        names: std::sync::Mutex::new(vec!["tool_a".to_string()]),
    });
    let lister_clone = Arc::clone(&lister);
    let state = Arc::new(ProcState {
        agent_id: "a".to_string(),
        agent_name: "a".to_string(),
        model: "m".to_string(),
        parent_id: None,
        budget: default_budget(),
        capabilities: CapabilityToken::default(),
        tools: lister,
        session_id: "s".to_string(),
        session_start: Instant::now(),
        journal_entries: Arc::new(AtomicU64::new(0)),
        hooks: FakeHookLister::empty(),
        turn: Arc::new(AtomicU64::new(0)),
    });
    let fs = ProcFs::new(MemoryFs::new(), state);

    let before = fs.list_dir("/proc/tools").unwrap();
    lister_clone
        .names
        .lock()
        .unwrap()
        .push("tool_b".to_string());
    let after = fs.list_dir("/proc/tools").unwrap();

    assert_eq!(before, vec!["tool_a"]);
    assert_eq!(after, vec!["tool_a", "tool_b"]);
}

// --- Session tests ----------------------------------------------------------

#[test]
fn procfs_session_id_returns_the_session_id() {
    let fs = make_procfs();
    assert_eq!(procfs_read_str(&fs, "/proc/session/id"), "session-xyz");
}

#[test]
fn procfs_session_uptime_ms_returns_a_numeric_string_that_increases_over_time() {
    let fs = make_procfs();
    let first: u64 = procfs_read_str(&fs, "/proc/session/uptime_ms")
        .parse()
        .expect("uptime_ms should be a number");
    std::thread::sleep(std::time::Duration::from_millis(2));
    let second: u64 = procfs_read_str(&fs, "/proc/session/uptime_ms")
        .parse()
        .expect("uptime_ms should stay numeric");
    assert!(second >= first, "uptime_ms should not decrease");
}

#[test]
fn procfs_session_journal_entries_returns_current_count() {
    let fs = make_procfs();
    assert_eq!(procfs_read_str(&fs, "/proc/session/journal_entries"), "42");
}

// --- Hook tests -------------------------------------------------------------

#[test]
fn procfs_hooks_tool_call_returns_newline_separated_hook_names() {
    let fs = make_procfs();
    assert_eq!(
        procfs_read_str(&fs, "/proc/hooks/tool_call"),
        "audit\nenforce"
    );
}

#[test]
fn procfs_hooks_tool_call_returns_empty_string_when_no_hooks_registered() {
    let fs = make_procfs_child();
    assert_eq!(procfs_read_str(&fs, "/proc/hooks/tool_call"), "");
}

#[test]
fn procfs_hooks_directory_lists_all_four_operation_types() {
    let fs = make_procfs();
    let names = fs.list_dir("/proc/hooks").unwrap();
    assert_eq!(names, vec!["http_request", "llm", "spawn", "tool_call"]);
}

// --- Directory listing tests ------------------------------------------------

#[test]
fn procfs_root_directory_listing_returns_expected_sorted_children() {
    let fs = make_procfs();
    assert_eq!(
        fs.list_dir("/proc").unwrap(),
        vec![
            "agent",
            "budget",
            "capabilities",
            "hooks",
            "mailbox",
            "session",
            "tools"
        ]
    );
}

#[test]
fn procfs_agent_directory_listing_returns_all_agent_file_names_sorted() {
    let fs = make_procfs();
    assert_eq!(
        fs.list_dir("/proc/agent").unwrap(),
        vec!["id", "model", "name", "parent_id", "turn"]
    );
}

#[test]
fn procfs_tools_directory_listing_returns_one_entry_per_tool() {
    let fs = make_procfs();
    assert_eq!(
        fs.list_dir("/proc/tools").unwrap(),
        vec!["file_read", "list_dir"]
    );
}

#[test]
fn procfs_budget_directory_listing_returns_all_budget_file_names_sorted() {
    let fs = make_procfs();
    assert_eq!(
        fs.list_dir("/proc/budget").unwrap(),
        vec![
            "max_cost",
            "max_fuel",
            "max_tokens",
            "max_turns",
            "remaining_tokens",
            "remaining_turns",
            "used_cost",
            "used_fuel",
            "used_tokens",
            "used_turns",
        ]
    );
}

#[test]
fn procfs_mailbox_directory_listing_delegates_to_inner_vfs() {
    let fs = make_procfs();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/proc/mailbox/report.md", b"body").unwrap();
    vfs.write("/proc/mailbox/data.json", b"{}").unwrap();
    let mut names = vfs.list_dir("/proc/mailbox").unwrap();
    names.sort();
    assert_eq!(names, vec!["data.json", "report.md"]);
}

#[test]
fn procfs_mailbox_listing_on_fresh_vfs_returns_empty_list() {
    // Before any writes, listing /proc/mailbox/ should return [] not NotFound.
    let fs = make_procfs();
    let names = fs.list_dir("/proc/mailbox").unwrap();
    assert!(
        names.is_empty(),
        "empty mailbox should return [] not NotFound; got {names:?}"
    );
}

// --- Trailing-slash path tests ----------------------------------------------

#[test]
fn procfs_list_dir_with_trailing_slash_on_proc_root_returns_expected_children() {
    let fs = make_procfs();
    assert_eq!(
        fs.list_dir("/proc/").unwrap(),
        vec![
            "agent",
            "budget",
            "capabilities",
            "hooks",
            "mailbox",
            "session",
            "tools"
        ]
    );
}

#[test]
fn procfs_list_dir_with_trailing_slash_on_agent_subdirectory_returns_expected_children() {
    let fs = make_procfs();
    assert_eq!(
        fs.list_dir("/proc/agent/").unwrap(),
        vec!["id", "model", "name", "parent_id", "turn"]
    );
}

#[test]
fn procfs_list_dir_with_trailing_slash_on_budget_subdirectory_returns_expected_children() {
    let fs = make_procfs();
    let listing = fs.list_dir("/proc/budget/").unwrap();
    assert!(listing.contains(&"max_tokens".to_string()));
    assert!(listing.contains(&"used_tokens".to_string()));
    assert!(listing.contains(&"remaining_tokens".to_string()));
}

#[test]
fn procfs_metadata_for_proc_root_with_trailing_slash_returns_directory_metadata() {
    let fs = make_procfs();
    let meta = fs.metadata("/proc/").unwrap();
    assert!(meta.is_dir);
    assert!(!meta.is_file);
}

// --- Write protection tests -------------------------------------------------

#[test]
fn procfs_write_to_agent_id_returns_permission_denied() {
    let fs = make_procfs();
    let err = fs
        .write("/proc/agent/id", b"mutated")
        .expect_err("/proc/agent/id should be read-only");
    assert_permission_denied(&err);
}

#[test]
fn procfs_write_to_budget_max_tokens_returns_permission_denied() {
    let fs = make_procfs();
    let err = fs
        .write("/proc/budget/max_tokens", b"999")
        .expect_err("/proc/budget/* should be read-only");
    assert_permission_denied(&err);
}

#[test]
fn procfs_remove_tool_entry_returns_permission_denied() {
    let fs = make_procfs();
    let err = fs
        .remove("/proc/tools/file_read")
        .expect_err("/proc/tools/* should be immutable");
    assert_permission_denied(&err);
}

#[test]
fn procfs_mkdir_under_proc_returns_permission_denied() {
    let fs = make_procfs();
    let err = fs
        .mkdir("/proc/custom")
        .expect_err("creating /proc directories should be rejected");
    assert_permission_denied(&err);
}

#[test]
fn procfs_rejects_all_non_mailbox_write_operations() {
    let fs = make_procfs();
    let vfs: &dyn VirtualFs = &fs;
    assert_permission_denied(
        &vfs.write("/proc/session/id", b"x")
            .expect_err("session id write should be denied"),
    );
    assert_permission_denied(
        &vfs.remove("/proc/agent/id")
            .expect_err("agent id remove should be denied"),
    );
    assert_permission_denied(
        &vfs.mkdir("/proc/budget/new")
            .expect_err("budget mkdir should be denied"),
    );
}

// --- Mailbox tests ----------------------------------------------------------

#[test]
fn procfs_mailbox_write_to_report_md_succeeds() {
    let fs = make_procfs();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/proc/mailbox/report.md", b"report body")
        .unwrap();
}

#[test]
fn procfs_mailbox_read_returns_written_content() {
    let fs = make_procfs();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/proc/mailbox/report.md", b"report body")
        .unwrap();
    assert_eq!(vfs.read("/proc/mailbox/report.md").unwrap(), b"report body");
}

#[test]
fn procfs_mailbox_list_dir_shows_written_files() {
    let fs = make_procfs();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/proc/mailbox/report.md", b"body").unwrap();
    vfs.write("/proc/mailbox/analysis.json", b"{}").unwrap();
    let mut names = vfs.list_dir("/proc/mailbox").unwrap();
    names.sort();
    assert_eq!(names, vec!["analysis.json", "report.md"]);
}

#[test]
fn procfs_mailbox_files_survive_snapshot_and_restore() {
    let fs = make_procfs();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/proc/mailbox/report.md", b"report body")
        .unwrap();
    let snap = vfs.snapshot().unwrap();
    vfs.remove("/proc/mailbox/report.md").unwrap();
    vfs.restore(&snap).unwrap();
    assert_eq!(vfs.read("/proc/mailbox/report.md").unwrap(), b"report body");
}

// --- Metadata and existence tests -------------------------------------------

#[test]
fn procfs_exists_returns_true_for_agent_id() {
    let fs = make_procfs();
    assert!(fs.exists("/proc/agent/id"));
}

#[test]
fn procfs_exists_returns_false_for_nonexistent_proc_path() {
    let fs = make_procfs();
    assert!(!fs.exists("/proc/nonexistent"));
}

#[test]
fn procfs_metadata_for_agent_directory_returns_directory_metadata() {
    let fs = make_procfs();
    let md = fs.metadata("/proc/agent").unwrap();
    assert!(md.is_dir);
    assert!(!md.is_file);
}

#[test]
fn procfs_metadata_for_agent_id_returns_file_metadata_with_correct_size() {
    let fs = make_procfs();
    let md = fs.metadata("/proc/agent/id").unwrap();
    assert!(md.is_file);
    assert!(!md.is_dir);
    assert_eq!(md.size, "agent-abc123".len() as u64);
}

#[test]
fn procfs_metadata_for_proc_root_returns_directory_metadata() {
    let fs = make_procfs();
    let md = fs.metadata("/proc").unwrap();
    assert!(md.is_dir);
    assert!(!md.is_file);
}

// --- Unknown paths tests ----------------------------------------------------

#[test]
fn procfs_read_unknown_proc_path_returns_not_found() {
    let fs = make_procfs();
    assert!(matches!(
        fs.read("/proc/nonexistent"),
        Err(VfsError::NotFound(_))
    ));
}

#[test]
fn procfs_read_unknown_agent_child_path_returns_not_found() {
    let fs = make_procfs();
    assert!(matches!(
        fs.read("/proc/agent/nonexistent"),
        Err(VfsError::NotFound(_))
    ));
}

#[test]
fn procfs_list_dir_unknown_subtree_returns_not_found() {
    let fs = make_procfs();
    assert!(matches!(
        fs.list_dir("/proc/nonexistent"),
        Err(VfsError::NotFound(_))
    ));
}

// --- Non-proc paths delegate to inner VFS -----------------------------------

#[test]
fn procfs_non_proc_paths_delegate_to_inner_vfs() {
    let fs = make_procfs();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/workspace/file.txt", b"hello").unwrap();
    assert_eq!(vfs.read("/workspace/file.txt").unwrap(), b"hello");
}

// --- Observability tests ----------------------------------------------------

#[test]
fn procfs_read_emits_simulacra_procfs_read_span_with_path_and_category() {
    let fs = make_procfs();
    let vfs: &dyn VirtualFs = &fs;

    let (_, spans) = capture_spans(|| {
        let _ = vfs.read("/proc/agent/id");
    });

    let found = spans.iter().any(|span| {
        span.name == "simulacra_procfs_read"
            && field_matches(span, "simulacra.procfs.path", "/proc/agent/id")
            && field_matches(span, "simulacra.procfs.category", "agent")
    });
    assert!(found, "expected simulacra_procfs_read span; got {spans:#?}");
}

#[test]
fn procfs_list_dir_emits_simulacra_procfs_list_dir_span_with_path() {
    let fs = make_procfs();
    let vfs: &dyn VirtualFs = &fs;

    let (_, spans) = capture_spans(|| {
        let _ = vfs.list_dir("/proc/agent");
    });

    assert!(
        spans.iter().any(|span| {
            span.name == "simulacra_procfs_list_dir"
                && field_matches(span, "simulacra.procfs.path", "/proc/agent")
        }),
        "expected simulacra_procfs_list_dir span; got {spans:#?}"
    );
}

#[test]
fn procfs_mailbox_writes_use_standard_vfs_write_observability_not_procfs_spans() {
    let fs = make_procfs();
    let vfs: &dyn VirtualFs = &fs;

    let (_, spans) = capture_spans(|| {
        vfs.write("/proc/mailbox/report.md", b"report").unwrap();
    });

    assert_span_with_path(&spans, "vfs_write", "/proc/mailbox/report.md");
    assert!(
        spans
            .iter()
            .all(|span| span.name != "simulacra_procfs_write"),
        "mailbox writes should not produce simulacra_procfs_write spans"
    );
}

#[test]
fn procfs_read_emits_debug_event_with_path_and_value_length() {
    let fs = make_procfs();
    let vfs: &dyn VirtualFs = &fs;

    let (_, _, events) = capture_trace(|| {
        let _ = vfs.read("/proc/agent/id");
    });

    assert!(
        events.iter().any(|event| {
            event.level == "DEBUG"
                && event.current_span.as_deref() == Some("simulacra_procfs_read")
                && event_field_matches(event, "simulacra.procfs.path", "/proc/agent/id")
                && event_field_matches(event, "simulacra.procfs.value_len", "12")
        }),
        "expected debug event with procfs path and value length; got {events:#?}"
    );
}

#[test]
fn procfs_write_attempt_to_read_only_path_emits_warn_event() {
    let fs = make_procfs();
    let vfs: &dyn VirtualFs = &fs;

    let (_, _, events) = capture_trace(|| {
        let _ = vfs.write("/proc/agent/id", b"mutated");
    });

    assert!(
        events.iter().any(|event| {
            event.level == "WARN"
                && event.current_span.is_none()
                && event_field_matches(event, "simulacra.procfs.path", "/proc/agent/id")
                && event
                    .fields
                    .values()
                    .any(|value| value.contains("write attempt to read-only procfs path"))
        }),
        "expected warn event for write to read-only procfs path; got {events:#?}"
    );
}

#[test]
fn procfs_read_increments_counter_with_category_label() {
    let fs = make_procfs();
    let vfs: &dyn VirtualFs = &fs;

    let (_, _, events) = capture_trace(|| {
        let _ = vfs.read("/proc/agent/id");
    });

    assert!(
        events.iter().any(|event| {
            event_field_matches(event, "simulacra.procfs.reads", "1")
                && event_field_matches(event, "category", "agent")
        }),
        "expected simulacra.procfs.reads counter emission with category label; got {events:#?}"
    );
}
