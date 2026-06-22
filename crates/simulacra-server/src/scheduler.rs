//! Cron scheduler — background tokio task that fires schedules and creates tasks.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use cron::Schedule;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use opentelemetry::KeyValue;

use crate::engine::SimulacraEngine;
use crate::metrics::ServerMeters;
use crate::task::TaskManager;
use crate::tenant::TenantResolver;

// ──────────────────────────────────────────────────────────────────────────────
// Types
// ──────────────────────────────────────────────────────────────────────────────

/// Policy for handling missed schedule runs on server restart.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MissedPolicy {
    /// Missed runs are ignored. Next run at next scheduled time.
    #[default]
    Skip,
    /// Run once if any runs were missed.
    RunOnce,
    /// Run once per missed interval, capped at 10.
    Backfill,
}

/// Configuration for a single cron schedule (from `[[schedules]]` in simulacra.toml).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleConfig {
    /// Unique schedule name (used in logs, spans, and persisted state).
    pub name: String,
    /// Standard 5-field cron expression (evaluated in UTC).
    pub cron: String,
    /// Tenant namespace for tasks created by this schedule.
    pub tenant: String,
    /// Task description sent to the agent.
    pub task: String,
    /// Agent type for tasks created by this schedule.
    pub agent_type: String,
    /// Policy for handling missed runs after a server restart.
    #[serde(default)]
    pub missed_policy: MissedPolicy,
    /// Whether this schedule is active. Disabled schedules are loaded but not run.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Normalize a cron expression to 7-field format expected by the `cron` crate.
///
/// Standard 5-field: `min hour day month weekday`
/// Standard 6-field: `sec min hour day month weekday`
/// 7-field (cron crate): `sec min hour day month weekday year`
fn normalize_cron_expression(expr: &str) -> String {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    match parts.len() {
        5 => format!("0 {} *", parts.join(" ")), // prepend sec=0, append year=*
        6 => format!("{} *", parts.join(" ")),   // append year=*
        _ => expr.to_string(),                   // pass through as-is
    }
}

/// A parsed schedule entry ready to run.
pub struct ScheduleEntry {
    pub config: ScheduleConfig,
    pub schedule: Option<Schedule>, // None if cron expression was invalid
}

impl ScheduleEntry {
    /// Parse the cron expression. Returns entry with `schedule = None` on parse error.
    ///
    /// The `cron` crate uses 7-field syntax (sec min hour day month weekday year).
    /// Standard 5-field expressions (min hour day month weekday) are auto-prefixed with `0`
    /// for seconds and `*` for year to produce 7-field format.
    pub fn from_config(config: ScheduleConfig) -> Self {
        let normalized = normalize_cron_expression(&config.cron);
        let schedule = match Schedule::from_str(&normalized) {
            Ok(s) => {
                info!(schedule_name = %config.name, cron = %config.cron, "parsed cron schedule");
                Some(s)
            }
            Err(e) => {
                error!(
                    schedule_name = %config.name,
                    cron = %config.cron,
                    error = %e,
                    "cron parse error — schedule disabled"
                );
                None
            }
        };
        Self { config, schedule }
    }

    /// Returns true if this schedule is enabled and has a valid cron expression.
    pub fn is_active(&self) -> bool {
        self.config.enabled && self.schedule.is_some()
    }

    /// Get the next fire time strictly after `after`.
    ///
    /// Uses `Schedule::after(&after)` so the result is anchored to the
    /// supplied timestamp, not to the current wall-clock time (which is what
    /// `upcoming(Utc)` would use). This matters when the scheduler is
    /// iterating over past intervals (e.g. backfill), or when the loop
    /// oversleeps and `now` has advanced past the originally-scheduled fire
    /// time. Returning anything other than "next occurrence after `after`"
    /// lets schedules silently get skipped.
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.schedule.as_ref()?.after(&after).next()
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Persistence
// ──────────────────────────────────────────────────────────────────────────────

/// Persisted last-fire record for a schedule.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LastFireRecord {
    pub last_fire: DateTime<Utc>,
}

/// Read the persisted last fire time for a schedule.
fn read_last_fire(data_dir: &Path, schedule_name: &str) -> Option<DateTime<Utc>> {
    let path = last_fire_path(data_dir, schedule_name);
    let contents = std::fs::read_to_string(path).ok()?;
    let record: LastFireRecord = serde_json::from_str(&contents).ok()?;
    Some(record.last_fire)
}

/// Persist the last fire time for a schedule.
fn write_last_fire(
    data_dir: &Path,
    schedule_name: &str,
    fire_time: DateTime<Utc>,
) -> std::io::Result<()> {
    let path = last_fire_path(data_dir, schedule_name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let record = LastFireRecord {
        last_fire: fire_time,
    };
    let json = serde_json::to_string(&record)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, json)
}

fn last_fire_path(data_dir: &Path, schedule_name: &str) -> PathBuf {
    data_dir
        .join("schedules")
        .join(format!("{schedule_name}.last_fire"))
}

// ──────────────────────────────────────────────────────────────────────────────
// Scheduler
// ──────────────────────────────────────────────────────────────────────────────

/// Cron scheduler — runs in a background tokio task.
///
/// Construct with [`Scheduler::with_engine`] in production so that each
/// schedule fire actually runs an agent. The [`Scheduler::new`] constructor
/// creates a record-only scheduler (no agent worker) and is intended for
/// tests that verify scheduling semantics (missed policy, cron parsing, etc.)
/// without the weight of a full engine.
pub struct Scheduler {
    entries: Vec<ScheduleEntry>,
    data_dir: PathBuf,
    task_manager: Arc<TaskManager>,
    resolver: Arc<TenantResolver>,
    /// Optional engine. When present, each fire calls `engine.spawn_task`
    /// (agent actually runs). When `None`, falls back to
    /// `TaskManager::create_task` — a record-only path.
    engine: Option<Arc<SimulacraEngine>>,
}

impl Scheduler {
    /// Construct a record-only scheduler. Used by tests; production should
    /// use [`Scheduler::with_engine`].
    pub fn new(
        entries: Vec<ScheduleEntry>,
        data_dir: PathBuf,
        task_manager: Arc<TaskManager>,
        resolver: Arc<TenantResolver>,
    ) -> Self {
        Self {
            entries,
            data_dir,
            task_manager,
            resolver,
            engine: None,
        }
    }

    /// Construct an engine-backed scheduler. Each schedule fire spawns a
    /// real agent via `SimulacraEngine::spawn_task`.
    pub fn with_engine(
        entries: Vec<ScheduleEntry>,
        data_dir: PathBuf,
        task_manager: Arc<TaskManager>,
        resolver: Arc<TenantResolver>,
        engine: Arc<SimulacraEngine>,
    ) -> Self {
        Self {
            entries,
            data_dir,
            task_manager,
            resolver,
            engine: Some(engine),
        }
    }

    /// Handle missed runs for all schedules at startup.
    /// Call this once before starting the main scheduler loop.
    pub async fn handle_missed_runs(&self) {
        let now = Utc::now();
        for entry in &self.entries {
            if !entry.is_active() {
                continue;
            }
            let last_fire = read_last_fire(&self.data_dir, &entry.config.name);
            let Some(last) = last_fire else {
                // Never fired — no missed runs to handle.
                continue;
            };

            // Count missed intervals between last fire and now.
            let schedule = entry.schedule.as_ref().unwrap();
            let missed: Vec<DateTime<Utc>> = schedule
                .after(&last)
                .take(11) // cap at 10 + 1 to detect overflow
                .take_while(|t| *t < now)
                .collect();

            if missed.is_empty() {
                continue;
            }

            let missed_count = missed.len().min(10);
            warn!(
                schedule_name = %entry.config.name,
                missed_count = missed_count,
                missed_policy = ?entry.config.missed_policy,
                "missed schedule runs detected on startup"
            );

            // Emit simulacra.trigger.missed_runs counter.
            let policy_str = match entry.config.missed_policy {
                MissedPolicy::Skip => "skip",
                MissedPolicy::RunOnce => "run-once",
                MissedPolicy::Backfill => "backfill",
            };
            ServerMeters::get().missed_runs.add(
                missed_count as u64,
                &[
                    KeyValue::new("schedule_name", entry.config.name.clone()),
                    KeyValue::new("missed_policy", policy_str),
                ],
            );

            match entry.config.missed_policy {
                MissedPolicy::Skip => {
                    info!(
                        schedule_name = %entry.config.name,
                        "skipping {} missed run(s) per policy",
                        missed_count
                    );
                    // Advance last_fire so we don't re-detect the same missed runs
                    // on the next server restart.
                    if let Err(e) = write_last_fire(&self.data_dir, &entry.config.name, now) {
                        warn!(
                            schedule_name = %entry.config.name,
                            error = %e,
                            "failed to advance last_fire after skip"
                        );
                    }
                }
                MissedPolicy::RunOnce => {
                    // Create exactly one task.
                    self.fire_schedule(entry, now).await;
                }
                MissedPolicy::Backfill => {
                    // One task per missed interval, capped at 10, sequential.
                    let to_run = missed.into_iter().take(10).collect::<Vec<_>>();
                    for fire_time in to_run {
                        self.fire_schedule(entry, fire_time).await;
                        // Brief delay between backfill tasks.
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }
    }

    /// Main scheduler loop — runs until cancelled.
    pub async fn run(self: Arc<Self>, cancel: tokio_util::sync::CancellationToken) {
        info!("cron scheduler started");
        // Remember the last time we checked for fires so that, even if
        // tokio::time::sleep oversleeps, we still fire any schedules whose
        // target time falls in the gap between last_tick and the new `now`.
        let mut last_tick = Utc::now();
        loop {
            let now = Utc::now();

            // Find the next fire time across all active schedules, anchored
            // at `last_tick` (not `now`). This prevents a schedule whose fire
            // time is already in the past (e.g., because we oversept) from
            // being skipped: its next_after(last_tick) is still a past
            // timestamp, and the comparison below against `now` catches it.
            let next = self
                .entries
                .iter()
                .filter(|e| e.is_active())
                .filter_map(|e| e.next_after(last_tick))
                .min();

            let Some(next_fire) = next else {
                // No active schedules — sleep a bit and re-check.
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(60)) => {}
                    _ = cancel.cancelled() => break,
                }
                last_tick = Utc::now();
                continue;
            };

            // Sleep until the next fire time. If it is already in the past
            // (oversleep or long-running previous tick), this is zero.
            let delay = (next_fire - now).to_std().unwrap_or(Duration::ZERO);

            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = cancel.cancelled() => break,
            }

            // Fire all schedules whose next-after-last_tick time has elapsed
            // by the time we wake up. Always re-read `now` — never use the
            // stale value captured at the top of the loop.
            let fire_time = Utc::now();
            for entry in &self.entries {
                if !entry.is_active() {
                    continue;
                }
                if let Some(t) = entry.next_after(last_tick)
                    && t <= fire_time
                {
                    self.fire_schedule(entry, fire_time).await;
                }
            }
            last_tick = fire_time;
        }
        info!("cron scheduler stopped");
    }

    async fn fire_schedule(&self, entry: &ScheduleEntry, fire_time: DateTime<Utc>) {
        let _span = tracing::info_span!(
            "simulacra_schedule_fired",
            "simulacra.trigger.schedule_name" = entry.config.name.as_str(),
            "simulacra.trigger.tenant" = entry.config.tenant.as_str(),
        )
        .entered();

        let tenant = match self.resolver.get(&entry.config.tenant) {
            Some(t) => t,
            None => {
                error!(
                    schedule_name = %entry.config.name,
                    tenant = %entry.config.tenant,
                    "scheduler: tenant not found — skipping fire"
                );
                return;
            }
        };

        let metadata = serde_json::json!({
            "source": "schedule",
            "schedule_name": entry.config.name,
            "fire_time": fire_time.to_rfc3339(),
        });

        // Engine path (production): spawn a real agent via spawn_task so the
        // schedule actually runs. The no-engine path is record-only and is
        // used by tests that validate scheduling semantics without the cost
        // of a full engine.
        let creation_result: Result<String, String> = match self.engine.as_ref() {
            Some(engine) => engine
                .spawn_task(
                    &self.task_manager,
                    &entry.config.task,
                    tenant,
                    Some(&entry.config.agent_type),
                    metadata,
                    None,
                    None,
                )
                .await
                .map(|h| h.task_id)
                .map_err(|e| e.to_string()),
            None => self
                .task_manager
                .create_task(
                    tenant,
                    entry.config.task.clone(),
                    Some(entry.config.agent_type.clone()),
                    metadata,
                    None,
                )
                .map(|h| h.task_id)
                .map_err(|e| e.to_string()),
        };

        match creation_result {
            Ok(task_id) => {
                info!(
                    schedule_name = %entry.config.name,
                    tenant = %entry.config.tenant,
                    task_id = %task_id,
                    via_engine = self.engine.is_some(),
                    "scheduled task created"
                );
                ServerMeters::get().schedule_fires.add(
                    1,
                    &[
                        KeyValue::new("schedule_name", entry.config.name.clone()),
                        KeyValue::new("tenant", entry.config.tenant.clone()),
                    ],
                );
                // Persist the fire time.
                if let Err(e) = write_last_fire(&self.data_dir, &entry.config.name, fire_time) {
                    warn!(
                        schedule_name = %entry.config.name,
                        error = %e,
                        "failed to persist last fire time"
                    );
                }
            }
            Err(e) => {
                error!(
                    schedule_name = %entry.config.name,
                    error = %e,
                    "failed to create scheduled task"
                );
            }
        }
    }
}
