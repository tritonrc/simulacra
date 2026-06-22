//! Tests for grace-period cleanup of terminal TaskRecords (papercut-7).
//!
//! Background: `TaskManager.tasks` is a `HashMap` that historically grew without
//! bound. After a task reached a terminal state (Completed / Failed / Killed /
//! Cancelled) the record — including the entire per-task event log used by SSE
//! replay — was retained forever. A long-running server would eventually OOM.
//!
//! These tests pin the grace-period cleanup contract:
//!   * Terminal tasks remain subscribable for the configured grace period.
//!   * After the grace period elapses, `subscribe_task` and `get_task` return
//!     `NotFound`.
//!   * Non-terminal tasks (Pending, Running, Streaming, WaitingApproval,
//!     WaitingInput, Paused) are NEVER cleaned up automatically, regardless of
//!     how long they have existed.
//!   * The grace period is configurable via constructor.
//!   * Concurrent emit during cleanup does not panic and is race-safe.
//!
//! Time-based testing uses an injectable clock so tests do not sleep real
//! wall-clock seconds. The cleanup loop is driven manually via
//! `cleanup_expired_now()` which makes the tests deterministic and independent
//! of tokio's timer paused/advance semantics.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::json;
use simulacra_server::task::TestClock;
use simulacra_server::{BudgetPoolConfig, TaskManager, TaskManagerError, TaskState, TenantConfig};

fn tenant(namespace: &str) -> TenantConfig {
    TenantConfig {
        namespace: namespace.to_string(),
        agent_type: "worker".to_string(),
        vfs_root: PathBuf::from(format!("/tmp/{namespace}")),
        budget_pool: BudgetPoolConfig {
            max_tokens: 10_000,
            max_cost: "25.00".to_string(),
        },
        hooks: vec![],
        integrations: vec![],
        mcp_servers: Default::default(),
    }
}

// ── Within-grace-period subscribability ──────────────────────────────────────

#[tokio::test]
async fn terminal_task_is_subscribable_within_grace_period() {
    let clock = TestClock::new();
    let manager =
        TaskManager::with_grace_period_and_clock(Duration::from_millis(100), clock.clone());
    let handle = manager
        .create_task(&tenant("acme"), "graceful task", None, json!({}), None)
        .expect("task should be created");

    manager
        .emit_event(
            &handle.task_id,
            json!({"event": "agent.message", "content": "hello"}),
        )
        .expect("emit_event should succeed");

    manager
        .complete_task(&handle.task_id, TaskState::Completed, None)
        .expect("complete_task should succeed");

    // Advance virtual time but stay inside the grace window.
    clock.advance(Duration::from_millis(50));
    manager.cleanup_expired_now();

    // Subscribe must still succeed and full history must be present.
    let (history, _rx) = manager
        .subscribe_task(&handle.task_id)
        .expect("subscriber should attach within grace period");

    let event_names: Vec<&str> = history
        .iter()
        .map(|e| e["event"].as_str().unwrap_or(""))
        .collect();
    // pending→running, agent.message, running→completed
    assert!(
        event_names.contains(&"agent.message"),
        "history should preserve emitted events within grace window: {event_names:?}"
    );
    assert!(
        event_names.contains(&"task.state_changed"),
        "history should preserve state transitions: {event_names:?}"
    );

    // get_task also still succeeds.
    let task = manager
        .get_task(&handle.task_id)
        .expect("get_task should succeed within grace period");
    assert_eq!(task.state, TaskState::Completed);
}

// ── Post-grace cleanup ───────────────────────────────────────────────────────

#[tokio::test]
async fn terminal_task_is_removed_after_grace_period_elapses() {
    let clock = TestClock::new();
    let manager =
        TaskManager::with_grace_period_and_clock(Duration::from_millis(100), clock.clone());
    let handle = manager
        .create_task(&tenant("acme"), "expiring task", None, json!({}), None)
        .expect("task should be created");

    manager
        .complete_task(&handle.task_id, TaskState::Completed, None)
        .expect("complete_task should succeed");

    // Advance well past the grace window.
    clock.advance(Duration::from_millis(150));
    manager.cleanup_expired_now();

    // subscribe_task must now return NotFound (or SubscribeFailed).
    let err = manager
        .subscribe_task(&handle.task_id)
        .expect_err("subscribe should fail after grace period");
    assert!(
        matches!(
            err,
            TaskManagerError::NotFound { .. } | TaskManagerError::SubscribeFailed { .. }
        ),
        "expected NotFound/SubscribeFailed after expiry, got: {err:?}"
    );

    // get_task must also return NotFound.
    let err = manager
        .get_task(&handle.task_id)
        .expect_err("get_task should fail after grace period");
    assert!(
        matches!(err, TaskManagerError::NotFound { .. }),
        "expected NotFound after expiry, got: {err:?}"
    );
}

#[tokio::test]
async fn cleanup_removes_records_for_every_terminal_state() {
    // Ensure Completed, Failed, Killed, Cancelled all qualify for cleanup.
    let clock = TestClock::new();
    let manager =
        TaskManager::with_grace_period_and_clock(Duration::from_millis(50), clock.clone());

    let mut ids = Vec::new();
    for state in [
        TaskState::Completed,
        TaskState::Failed,
        TaskState::Killed,
        TaskState::Cancelled,
    ] {
        let handle = manager
            .create_task(&tenant("acme"), "term", None, json!({}), None)
            .unwrap();
        manager
            .complete_task(&handle.task_id, state.clone(), None)
            .unwrap_or_else(|e| panic!("complete_task({state:?}) failed: {e}"));
        ids.push(handle.task_id);
    }

    clock.advance(Duration::from_millis(100));
    manager.cleanup_expired_now();

    for id in ids {
        assert!(
            manager.get_task(&id).is_err(),
            "expired terminal task {id} should be cleaned"
        );
    }
}

// ── Non-terminal tasks are never cleaned ─────────────────────────────────────

#[tokio::test]
async fn running_task_is_never_cleaned_up_however_old() {
    let clock = TestClock::new();
    let manager =
        TaskManager::with_grace_period_and_clock(Duration::from_millis(10), clock.clone());
    let handle = manager
        .create_task(&tenant("acme"), "long-running", None, json!({}), None)
        .expect("task should be created");
    // Task auto-transitions to Running in create_task.

    // Advance far past the grace period.
    clock.advance(Duration::from_secs(3600));
    manager.cleanup_expired_now();

    let task = manager
        .get_task(&handle.task_id)
        .expect("running task must NEVER be cleaned up automatically");
    assert_eq!(task.state, TaskState::Running);
    manager
        .subscribe_task(&handle.task_id)
        .expect("running task must remain subscribable");
}

#[tokio::test]
async fn waiting_approval_task_is_never_cleaned_up_however_old() {
    let clock = TestClock::new();
    let manager =
        TaskManager::with_grace_period_and_clock(Duration::from_millis(10), clock.clone());
    let handle = manager
        .create_task(&tenant("acme"), "approval needed", None, json!({}), None)
        .expect("task should be created");
    manager
        .request_approval(&handle.task_id)
        .expect("transition to waiting_approval");

    clock.advance(Duration::from_secs(3600));
    manager.cleanup_expired_now();

    let task = manager
        .get_task(&handle.task_id)
        .expect("waiting_approval must NEVER be cleaned up automatically");
    assert_eq!(task.state, TaskState::WaitingApproval);
}

#[tokio::test]
async fn waiting_input_task_is_never_cleaned_up_however_old() {
    let clock = TestClock::new();
    let manager =
        TaskManager::with_grace_period_and_clock(Duration::from_millis(10), clock.clone());
    let handle = manager
        .create_task(&tenant("acme"), "input needed", None, json!({}), None)
        .expect("task should be created");
    manager
        .request_input(&handle.task_id)
        .expect("transition to waiting_input");

    clock.advance(Duration::from_secs(3600));
    manager.cleanup_expired_now();

    manager
        .get_task(&handle.task_id)
        .expect("waiting_input must NEVER be cleaned up automatically");
}

#[tokio::test]
async fn paused_task_is_never_cleaned_up_however_old() {
    let clock = TestClock::new();
    let manager =
        TaskManager::with_grace_period_and_clock(Duration::from_millis(10), clock.clone());
    let handle = manager
        .create_task(&tenant("acme"), "paused", None, json!({}), None)
        .expect("task should be created");
    manager
        .pause_task(&handle.task_id)
        .expect("transition to paused");

    clock.advance(Duration::from_secs(3600));
    manager.cleanup_expired_now();

    let task = manager
        .get_task(&handle.task_id)
        .expect("paused must NEVER be cleaned up automatically");
    assert_eq!(task.state, TaskState::Paused);
}

#[tokio::test]
async fn pending_task_is_never_cleaned_up_however_old() {
    let clock = TestClock::new();
    let manager =
        TaskManager::with_grace_period_and_clock(Duration::from_millis(10), clock.clone());
    let handle = manager
        .create_pending_task(&tenant("acme"), "pending", None, json!({}), None)
        .expect("task should be created in pending");

    clock.advance(Duration::from_secs(3600));
    manager.cleanup_expired_now();

    let task = manager
        .get_task(&handle.task_id)
        .expect("pending must NEVER be cleaned up automatically");
    assert_eq!(task.state, TaskState::Pending);
}

// ── Configurable grace period ────────────────────────────────────────────────

#[tokio::test]
async fn configurable_grace_period_is_honoured() {
    // Two managers with different grace periods sharing nothing else.
    let clock_short = TestClock::new();
    let manager_short =
        TaskManager::with_grace_period_and_clock(Duration::from_millis(50), clock_short.clone());

    let clock_long = TestClock::new();
    let manager_long =
        TaskManager::with_grace_period_and_clock(Duration::from_millis(500), clock_long.clone());

    let h_short = manager_short
        .create_task(&tenant("acme"), "short", None, json!({}), None)
        .unwrap();
    let h_long = manager_long
        .create_task(&tenant("acme"), "long", None, json!({}), None)
        .unwrap();

    manager_short
        .complete_task(&h_short.task_id, TaskState::Completed, None)
        .unwrap();
    manager_long
        .complete_task(&h_long.task_id, TaskState::Completed, None)
        .unwrap();

    // Advance both clocks by 100ms — short expires, long does not.
    clock_short.advance(Duration::from_millis(100));
    clock_long.advance(Duration::from_millis(100));
    manager_short.cleanup_expired_now();
    manager_long.cleanup_expired_now();

    assert!(
        manager_short.get_task(&h_short.task_id).is_err(),
        "short-grace task should be expired at 100ms"
    );
    assert!(
        manager_long.get_task(&h_long.task_id).is_ok(),
        "long-grace task should NOT be expired at 100ms"
    );

    // Advance long-clock past its grace.
    clock_long.advance(Duration::from_millis(500));
    manager_long.cleanup_expired_now();
    assert!(
        manager_long.get_task(&h_long.task_id).is_err(),
        "long-grace task should expire after 600ms"
    );
}

#[tokio::test]
async fn default_grace_period_is_one_hour() {
    // Default constructor uses a 1-hour grace period.
    let clock = TestClock::new();
    let manager = TaskManager::with_clock(clock.clone());
    let handle = manager
        .create_task(&tenant("acme"), "default-grace", None, json!({}), None)
        .unwrap();
    manager
        .complete_task(&handle.task_id, TaskState::Completed, None)
        .unwrap();

    // Just under 1 hour: still present.
    clock.advance(Duration::from_secs(3500));
    manager.cleanup_expired_now();
    assert!(manager.get_task(&handle.task_id).is_ok());

    // Past 1 hour: gone.
    clock.advance(Duration::from_secs(200));
    manager.cleanup_expired_now();
    assert!(manager.get_task(&handle.task_id).is_err());
}

// ── Cleanup races + concurrency ──────────────────────────────────────────────

#[tokio::test]
async fn emit_after_cleanup_returns_not_found_does_not_panic() {
    let clock = TestClock::new();
    let manager =
        TaskManager::with_grace_period_and_clock(Duration::from_millis(50), clock.clone());
    let handle = manager
        .create_task(&tenant("acme"), "race", None, json!({}), None)
        .unwrap();
    manager
        .complete_task(&handle.task_id, TaskState::Completed, None)
        .unwrap();

    clock.advance(Duration::from_millis(100));
    manager.cleanup_expired_now();

    let err = manager
        .emit_event(&handle.task_id, json!({"event": "late"}))
        .expect_err("emit on cleaned task should fail cleanly");
    assert!(matches!(err, TaskManagerError::NotFound { .. }));
}

#[tokio::test]
async fn concurrent_emit_during_cleanup_does_not_panic() {
    // Spawn a flood of emits while running cleanup repeatedly.
    let clock = TestClock::new();
    let manager =
        TaskManager::with_grace_period_and_clock(Duration::from_millis(10), clock.clone());

    // Create 16 tasks; complete half so they're cleanup candidates.
    let mut active_ids = Vec::new();
    for i in 0..16 {
        let handle = manager
            .create_task(&tenant("acme"), format!("t{i}"), None, json!({}), None)
            .unwrap();
        if i % 2 == 0 {
            manager
                .complete_task(&handle.task_id, TaskState::Completed, None)
                .unwrap();
        } else {
            active_ids.push(handle.task_id);
        }
    }

    let panics = Arc::new(AtomicUsize::new(0));
    let manager = Arc::new(manager);
    let clock = clock.clone();

    let mut joins = Vec::new();
    // Workers emit on the still-active tasks.
    for tid in active_ids {
        let m = manager.clone();
        let p = panics.clone();
        joins.push(tokio::spawn(async move {
            for _ in 0..100 {
                let r = std::panic::AssertUnwindSafe(|| {
                    let _ = m.emit_event(&tid, json!({"event": "x"}));
                });
                if std::panic::catch_unwind(r).is_err() {
                    p.fetch_add(1, Ordering::SeqCst);
                }
                tokio::task::yield_now().await;
            }
        }));
    }
    // Clock-advancer + cleanup driver.
    {
        let m = manager.clone();
        let p = panics.clone();
        joins.push(tokio::spawn(async move {
            for _ in 0..50 {
                clock.advance(Duration::from_millis(5));
                let r = std::panic::AssertUnwindSafe(|| {
                    m.cleanup_expired_now();
                });
                if std::panic::catch_unwind(r).is_err() {
                    p.fetch_add(1, Ordering::SeqCst);
                }
                tokio::task::yield_now().await;
            }
        }));
    }

    for j in joins {
        j.await
            .expect("worker task should not panic at the join level");
    }
    assert_eq!(
        panics.load(Ordering::SeqCst),
        0,
        "emit/cleanup race must not panic"
    );
}

// ── Background loop drop semantics ───────────────────────────────────────────

#[tokio::test]
async fn dropping_taskmanager_does_not_leak_background_loop() {
    // The background cleanup task must terminate when the last TaskManager
    // strong reference is dropped. We can't directly observe the JoinHandle,
    // but we can construct → drop many managers in a single test process and
    // ensure no unbounded resource accumulates. A weak proxy: confirm we can
    // create and drop 100 managers without the test hanging or panicking.
    for _ in 0..100 {
        let clock = TestClock::new();
        let manager =
            TaskManager::with_grace_period_and_clock(Duration::from_millis(1), clock.clone());
        let handle = manager
            .create_task(&tenant("acme"), "ephemeral", None, json!({}), None)
            .unwrap();
        manager
            .complete_task(&handle.task_id, TaskState::Completed, None)
            .unwrap();
        drop(manager);
    }
    // Yield once so any straggling spawned task can run/exit.
    tokio::task::yield_now().await;
}
