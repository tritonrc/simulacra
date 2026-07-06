mod common;
#[allow(unused_imports)]
use common::*;

fn budget_with_exhausted_tokens_and_vfs(
    max_vfs_bytes: u64,
    used_vfs_bytes: u64,
) -> Arc<Mutex<ResourceBudget>> {
    let mut budget = ResourceBudget::new(8, 0, Decimal::ZERO, 0);
    budget.used_tokens = 8;
    budget.max_vfs_bytes = max_vfs_bytes;
    budget.used_vfs_bytes = used_vfs_bytes;
    Arc::new(Mutex::new(budget))
}

#[test]
fn read_file_with_exhausted_token_budget_still_reads_when_path_capability_allows() {
    let journal = Arc::new(FakeJournalStorage::default());
    let harness = Harness::new(
        capability(&["/workspace/**"], &[], false, false),
        budget_with_exhausted_tokens_and_vfs(0, 0),
        Arc::clone(&journal),
    );
    harness.vfs.seed_file("/workspace/input.txt", b"visible");
    harness.vfs.clear_observations();

    let data = harness
        .read_file("/workspace/input.txt")
        .expect("read_file should be governed by path capability, not token exhaustion");

    assert_eq!(data, b"visible");
    assert_eq!(
        harness.vfs.read_count(),
        1,
        "allowed read should reach the VFS even when token budget is exhausted"
    );
    assert!(
        journal.entries().iter().any(|entry| matches!(
            &entry.entry,
            JournalEntryKind::ToolResult {
                tool_name,
                is_error,
                ..
            } if tool_name == "read_file" && !*is_error
        )),
        "successful read should still be journaled"
    );
}

#[test]
fn write_file_with_exhausted_token_budget_still_writes_and_reserves_vfs_bytes() {
    let budget = budget_with_exhausted_tokens_and_vfs(16, 3);
    let harness = Harness::new(
        capability(&[], &["/workspace/**"], false, false),
        Arc::clone(&budget),
        Arc::new(FakeJournalStorage::default()),
    );

    harness
        .write_file("/workspace/output.txt", b"hello")
        .expect(
            "write_file should be governed by path capability and VFS bytes, not token exhaustion",
        );

    assert_eq!(
        harness.vfs.write_count(),
        1,
        "allowed write should reach the VFS even when token budget is exhausted"
    );
    assert_eq!(
        harness.vfs.inner.read("/workspace/output.txt").unwrap(),
        b"hello"
    );
    assert_eq!(
        budget_counter(&budget, "used_vfs_bytes"),
        8,
        "write_file must reserve the written byte count"
    );
}

#[test]
fn js_fs_write_with_exhausted_token_budget_still_uses_vfs_byte_budget() {
    let budget = budget_with_exhausted_tokens_and_vfs(64, 0);
    let harness = Harness::new(
        capability(&["/workspace/**"], &["/workspace/**"], false, true),
        Arc::clone(&budget),
        Arc::new(FakeJournalStorage::default()),
    );

    let output = harness
        .execute_js(
            "import { readFileSync, writeFileSync } from 'fs';\n\
             writeFileSync('/workspace/js.txt', 'js-ok');\n\
             console.log(readFileSync('/workspace/js.txt', 'utf8'));",
        )
        .expect("JS fs host functions should not be blocked by exhausted LLM token budget");

    assert_eq!(output.stdout, "js-ok\n");
    assert_eq!(budget_counter(&budget, "used_vfs_bytes"), 5);
}

#[test]
fn write_file_with_exhausted_token_budget_still_rejects_vfs_byte_exhaustion_before_write() {
    let budget = budget_with_exhausted_tokens_and_vfs(6, 4);
    let harness = Harness::new(
        capability(&[], &["/workspace/**"], false, false),
        Arc::clone(&budget),
        Arc::new(FakeJournalStorage::default()),
    );

    let error = harness
        .write_file("/workspace/output.txt", b"abc")
        .unwrap_err();

    assert_budget_exhausted(error, &["vfs_bytes"], "7", "6");
    assert_eq!(
        harness.vfs.write_count(),
        0,
        "VFS byte exhaustion must reject before touching the VFS"
    );
    assert!(!harness.vfs.exists("/workspace/output.txt"));
    assert_eq!(
        budget_counter(&budget, "used_vfs_bytes"),
        4,
        "rejected writes must not reserve bytes"
    );
}
