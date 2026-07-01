use std::collections::HashMap;
use std::sync::Arc;

use simulacra_types::VirtualFs;

use crate::MemoryFs;
use crate::mount::{MountError, process_host_mounts};

use super::common::{empty_agent_type, test_config};

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
