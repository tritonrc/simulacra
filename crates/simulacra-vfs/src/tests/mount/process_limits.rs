use std::collections::HashMap;
use std::sync::Arc;

use simulacra_types::VirtualFs;

use crate::MemoryFs;
use crate::mount::{MountError, copy_host_dir_to_vfs, process_host_mounts};

use super::super::common::{capture_spans, capture_trace, field_matches};
use super::common::{empty_agent_type, test_config};

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
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..4 {
        std::fs::write(tmp.path().join(format!("f{i}.txt")), b"data").unwrap();
    }

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let ((file_count, _), _, events) = capture_trace(|| {
        copy_host_dir_to_vfs(tmp.path(), "/mount", &vfs, 5, 10_000_000, "/mount").unwrap()
    });

    assert_eq!(file_count, 4, "should have 4 files (80% of 5 limit)");
    let warning_count = events
        .iter()
        .filter(|event| {
            event.level == "WARN"
                && event
                    .fields
                    .values()
                    .any(|value| value.contains("approaching file limit: 4/5"))
        })
        .count();
    assert_eq!(
        warning_count, 1,
        "expected exactly one file-threshold warning event; got {events:#?}"
    );
}

#[test]
fn copy_host_dir_80pct_byte_threshold_warning_fires() {
    // S020 behavior 28: 80% byte limit triggers WARN
    let tmp = tempfile::tempdir().unwrap();
    // Write 85 bytes, limit is 100 => 85% > 80% threshold
    std::fs::write(tmp.path().join("big.txt"), vec![b'x'; 85]).unwrap();

    let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let ((file_count, total_bytes), _, events) = capture_trace(|| {
        copy_host_dir_to_vfs(tmp.path(), "/mount", &vfs, 1000, 100, "/mount").unwrap()
    });

    assert_eq!(file_count, 1);
    assert_eq!(total_bytes, 85);
    let warning_count = events
        .iter()
        .filter(|event| {
            event.level == "WARN"
                && event
                    .fields
                    .values()
                    .any(|value| value.contains("approaching size limit: 85/100"))
        })
        .count();
    assert_eq!(
        warning_count, 1,
        "expected exactly one byte-threshold warning event; got {events:#?}"
    );
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
