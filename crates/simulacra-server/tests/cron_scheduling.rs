//! Tests for cron scheduler, missed policy, and schedule lifecycle (S032 assertions).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use serde_json::json;
use simulacra_server::{
    BudgetPoolConfig, MissedPolicy, ScheduleConfig, ScheduleEntry, Scheduler, TaskManager,
    TenantConfig, TenantResolver,
};

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn schedule_config(
    name: &str,
    cron: &str,
    missed_policy: MissedPolicy,
    enabled: bool,
) -> ScheduleConfig {
    ScheduleConfig {
        name: name.to_string(),
        cron: cron.to_string(),
        tenant: "accounting".to_string(),
        task: "Generate quarterly revenue variance report".to_string(),
        agent_type: "accounting-agent".to_string(),
        missed_policy,
        enabled,
    }
}

fn tenant_resolver() -> TenantResolver {
    let mut tenants = HashMap::new();
    tenants.insert(
        "accounting".to_string(),
        TenantConfig {
            namespace: "accounting".to_string(),
            agent_type: "accounting-agent".to_string(),
            vfs_root: PathBuf::from("/data/accounting"),
            budget_pool: BudgetPoolConfig {
                max_tokens: 100000,
                max_cost: String::new(),
            },
            hooks: vec![],
            integrations: vec![],
            mcp_servers: Default::default(),
        },
    );
    TenantResolver::new(tenants, None)
}

// ─── ScheduleEntry assertions ─────────────────────────────────────────────────

#[test]
fn schedule_entry_parses_valid_cron_expression_successfully() {
    let config = schedule_config("daily", "0 8 * * 1-5", MissedPolicy::Skip, true);
    let entry = ScheduleEntry::from_config(config);

    assert!(
        entry.schedule.is_some(),
        "valid cron expression must parse successfully"
    );
    assert!(
        entry.is_active(),
        "enabled schedule with valid cron must be active"
    );
}

#[test]
fn cron_expression_parsing_error_disables_the_schedule_and_logs_an_error() {
    let config = schedule_config("broken", "not a cron expression", MissedPolicy::Skip, true);
    let entry = ScheduleEntry::from_config(config);

    assert!(
        entry.schedule.is_none(),
        "invalid cron expression must produce None schedule"
    );
    assert!(
        !entry.is_active(),
        "schedule with invalid cron must not be active even if enabled = true"
    );
}

#[test]
fn schedule_with_enabled_false_does_not_fire() {
    let config = schedule_config("disabled", "0 8 * * 1-5", MissedPolicy::Skip, false);
    let entry = ScheduleEntry::from_config(config);

    assert!(
        !entry.is_active(),
        "disabled schedule must not be active regardless of cron validity"
    );
}

#[test]
fn schedule_entry_next_after_returns_correct_next_fire_time() {
    let config = schedule_config("daily", "0 8 * * *", MissedPolicy::Skip, true);
    let entry = ScheduleEntry::from_config(config);

    let now = Utc::now();
    let next = entry.next_after(now);
    assert!(next.is_some(), "active schedule must have a next fire time");
    let next = next.unwrap();
    assert!(next > now, "next fire time must be in the future");
}

// ─── Missed policy assertions ─────────────────────────────────────────────────

#[tokio::test]
async fn missed_policy_skip_creates_no_task_for_missed_runs_after_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();

    let manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(tenant_resolver());

    // Simulate a missed run by writing a last_fire time in the past.
    let past = Utc::now() - chrono::Duration::hours(25);
    write_last_fire_for_test(&data_dir, "daily-skip", past);

    let config = schedule_config("daily-skip", "0 8 * * *", MissedPolicy::Skip, true);
    let entry = ScheduleEntry::from_config(config);
    let scheduler = Scheduler::new(
        vec![entry],
        data_dir,
        Arc::clone(&manager),
        Arc::clone(&resolver),
    );

    scheduler.handle_missed_runs().await;

    // Skip policy: no tasks created.
    assert!(
        manager.active_task_ids().is_empty(),
        "skip policy must not create any catch-up tasks"
    );
}

#[tokio::test]
async fn missed_policy_run_once_creates_exactly_one_task_if_runs_were_missed() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();

    let manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(tenant_resolver());

    // 3 missed runs.
    let past = Utc::now() - chrono::Duration::hours(72);
    write_last_fire_for_test(&data_dir, "daily-run-once", past);

    let config = schedule_config("daily-run-once", "0 8 * * *", MissedPolicy::RunOnce, true);
    let entry = ScheduleEntry::from_config(config);
    let scheduler = Scheduler::new(
        vec![entry],
        data_dir,
        Arc::clone(&manager),
        Arc::clone(&resolver),
    );

    scheduler.handle_missed_runs().await;

    // RunOnce: exactly one task.
    let active = manager.active_task_ids();
    assert_eq!(
        active.len(),
        1,
        "run-once policy must create exactly one catch-up task, got {}",
        active.len()
    );
}

#[tokio::test]
async fn missed_policy_backfill_creates_one_task_per_missed_interval_capped_at_ten() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();

    let manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(tenant_resolver());

    // 15 missed hourly runs (more than the cap of 10).
    let past = Utc::now() - chrono::Duration::hours(15);
    write_last_fire_for_test(&data_dir, "hourly-backfill", past);

    let config = schedule_config("hourly-backfill", "0 * * * *", MissedPolicy::Backfill, true);
    let entry = ScheduleEntry::from_config(config);
    let scheduler = Scheduler::new(
        vec![entry],
        data_dir,
        Arc::clone(&manager),
        Arc::clone(&resolver),
    );

    scheduler.handle_missed_runs().await;

    // Backfill: capped at 10.
    let active = manager.active_task_ids();
    assert!(
        active.len() <= 10,
        "backfill must be capped at 10 tasks, got {}",
        active.len()
    );
    assert!(!active.is_empty(), "backfill must create at least one task");
}

#[tokio::test]
async fn backfill_creates_multiple_tasks_not_just_one() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();

    let manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(tenant_resolver());

    // 5 missed hourly runs.
    let past = Utc::now() - chrono::Duration::hours(5);
    write_last_fire_for_test(&data_dir, "hourly-multi", past);

    let config = schedule_config("hourly-multi", "0 * * * *", MissedPolicy::Backfill, true);
    let entry = ScheduleEntry::from_config(config);
    let scheduler = Scheduler::new(
        vec![entry],
        data_dir,
        Arc::clone(&manager),
        Arc::clone(&resolver),
    );

    scheduler.handle_missed_runs().await;

    let active = manager.active_task_ids();
    assert!(
        active.len() >= 2,
        "backfill must create one task per missed interval, got {}",
        active.len()
    );
}

#[tokio::test]
async fn no_missed_runs_when_last_fire_is_recent() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();

    let manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(tenant_resolver());

    // Recent last fire — no missed runs.
    let recent = Utc::now() - chrono::Duration::minutes(5);
    write_last_fire_for_test(&data_dir, "daily-recent", recent);

    let config = schedule_config("daily-recent", "0 8 * * *", MissedPolicy::RunOnce, true);
    let entry = ScheduleEntry::from_config(config);
    let scheduler = Scheduler::new(
        vec![entry],
        data_dir,
        Arc::clone(&manager),
        Arc::clone(&resolver),
    );

    scheduler.handle_missed_runs().await;

    assert!(
        manager.active_task_ids().is_empty(),
        "no missed runs should produce no catch-up tasks"
    );
}

#[tokio::test]
async fn no_missed_runs_when_no_last_fire_persisted() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();

    let manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(tenant_resolver());

    // No last_fire file — first ever run.
    let config = schedule_config("first-run", "0 8 * * *", MissedPolicy::RunOnce, true);
    let entry = ScheduleEntry::from_config(config);
    let scheduler = Scheduler::new(
        vec![entry],
        data_dir,
        Arc::clone(&manager),
        Arc::clone(&resolver),
    );

    scheduler.handle_missed_runs().await;

    // No tasks — first run, no missed interval to catch up on.
    assert!(
        manager.active_task_ids().is_empty(),
        "no persisted last_fire must produce no catch-up tasks"
    );
}

// ─── Scheduled task assertions ────────────────────────────────────────────────

#[tokio::test]
async fn scheduled_task_is_created_through_task_manager_with_schedule_tenant_config() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();

    let manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(tenant_resolver());

    // Trigger one missed run by using an every-minute schedule with last_fire 2 minutes ago.
    let past = Utc::now() - chrono::Duration::minutes(2);
    write_last_fire_for_test(&data_dir, "governed-schedule", past);

    let config = schedule_config(
        "governed-schedule",
        "* * * * *",
        MissedPolicy::RunOnce,
        true,
    );
    let entry = ScheduleEntry::from_config(config);
    let scheduler = Scheduler::new(
        vec![entry],
        data_dir,
        Arc::clone(&manager),
        Arc::clone(&resolver),
    );

    scheduler.handle_missed_runs().await;

    let task_ids = manager.active_task_ids();
    assert_eq!(task_ids.len(), 1);

    let handle = manager.get_task(&task_ids[0]).unwrap();
    assert_eq!(handle.tenant, "accounting");
    assert_eq!(handle.agent_type, "accounting-agent");
}

#[tokio::test]
async fn scheduled_task_metadata_contains_schedule_source_info() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();

    let manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(tenant_resolver());

    let past = Utc::now() - chrono::Duration::minutes(2);
    write_last_fire_for_test(&data_dir, "metadata-schedule", past);

    let config = schedule_config(
        "metadata-schedule",
        "* * * * *",
        MissedPolicy::RunOnce,
        true,
    );
    let entry = ScheduleEntry::from_config(config);
    let scheduler = Scheduler::new(
        vec![entry],
        data_dir,
        Arc::clone(&manager),
        Arc::clone(&resolver),
    );

    scheduler.handle_missed_runs().await;

    let task_ids = manager.active_task_ids();
    assert_eq!(task_ids.len(), 1);

    let handle = manager.get_task(&task_ids[0]).unwrap();
    assert_eq!(handle.metadata["source"], json!("schedule"));
    assert_eq!(handle.metadata["schedule_name"], json!("metadata-schedule"));
    assert!(
        handle.metadata.get("fire_time").is_some(),
        "metadata must include fire_time"
    );
}

// ─── BLOCKER 6: Skip policy must advance last_fire ────────────────────────────

#[tokio::test]
async fn missed_policy_skip_advances_last_fire_to_prevent_redetection_on_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();

    let manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(tenant_resolver());

    // Simulate missed runs by setting last_fire 25 hours ago.
    let past = Utc::now() - chrono::Duration::hours(25);
    write_last_fire_for_test(&data_dir, "skip-advance", past);

    let config = schedule_config("skip-advance", "0 8 * * *", MissedPolicy::Skip, true);
    let entry = ScheduleEntry::from_config(config);
    let scheduler = Scheduler::new(
        vec![entry],
        data_dir.clone(),
        Arc::clone(&manager),
        Arc::clone(&resolver),
    );

    scheduler.handle_missed_runs().await;

    // The last_fire file must exist and contain a time close to `now`,
    // confirming that Skip advanced it beyond the original `past` value.
    let last_fire_path = data_dir.join("schedules").join("skip-advance.last_fire");
    assert!(
        last_fire_path.exists(),
        "skip policy must write an updated last_fire file"
    );

    let contents = std::fs::read_to_string(&last_fire_path).unwrap();
    let record: serde_json::Value = serde_json::from_str(&contents).unwrap();
    let stored_time: chrono::DateTime<Utc> = record["last_fire"]
        .as_str()
        .unwrap()
        .parse()
        .expect("last_fire must be a valid RFC3339 timestamp");

    // The stored time must be significantly more recent than the original `past`.
    let gap = stored_time - past;
    assert!(
        gap.num_hours() >= 24,
        "last_fire must have been advanced by at least 24 hours (was advanced by {} hours)",
        gap.num_hours()
    );

    // No tasks created — skip means skip.
    assert!(
        manager.active_task_ids().is_empty(),
        "skip policy must not create any tasks"
    );
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn write_last_fire_for_test(data_dir: &Path, name: &str, time: chrono::DateTime<Utc>) {
    let dir = data_dir.join("schedules");
    std::fs::create_dir_all(&dir).unwrap();
    let record = serde_json::json!({"last_fire": time.to_rfc3339()});
    std::fs::write(dir.join(format!("{name}.last_fire")), record.to_string()).unwrap();
}
