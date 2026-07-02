use simulacra_types::VirtualFs;

use super::super::common::{
    assert_span_with_path, capture_spans, capture_trace, event_field_matches, field_matches,
};
use super::common::make_procfs;

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
