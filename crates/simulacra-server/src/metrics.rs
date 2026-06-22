//! OTel meter instruments for simulacra-server.
//!
//! All instruments are lazily initialised via `OnceLock` so they are acquired
//! from the global meter provider that is in place at first use — exactly the
//! pattern used by `simulacra-hooks` (`HookMeters`).
//!
//! `active_tasks` and `active_connections` are registered as observable gauges
//! (not `UpDownCounter`) so the OTel SDK exports them with `Gauge` data type,
//! which is semantically correct for "current count" readings.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, ObservableGauge};

/// Lazily-initialised OTel meter instruments for the server layer.
pub struct ServerMeters {
    /// Per-tenant in-flight task count — exported as an observable gauge.
    active_tasks_map: Arc<Mutex<HashMap<String, i64>>>,
    /// Holds the observable gauge registration alive (callback refs `active_tasks_map`).
    _active_tasks_gauge: ObservableGauge<i64>,

    /// Per-transport in-flight connection count — exported as an observable gauge.
    active_connections_map: Arc<Mutex<HashMap<String, i64>>>,
    /// Holds the observable gauge registration alive (callback refs `active_connections_map`).
    _active_connections_gauge: ObservableGauge<i64>,

    /// Task duration in seconds, recorded on terminal transition.
    pub task_duration: Histogram<f64>,
    /// Events emitted via `emit_event`, broken down by `event_type` + `tenant`.
    pub events_emitted: Counter<u64>,
    /// Auth failures broken down by `provider` + `reason`.
    pub auth_failures: Counter<u64>,

    // ── Trigger metrics (S032) ─────────────────────────────────────────────
    /// Webhook requests broken down by `webhook_name`, `tenant`, `status`.
    pub webhook_requests: Counter<u64>,
    /// Schedule fires broken down by `schedule_name`, `tenant`.
    pub schedule_fires: Counter<u64>,
    /// Missed schedule runs detected on startup, broken down by `schedule_name`, `missed_policy`.
    pub missed_runs: Counter<u64>,
}

impl ServerMeters {
    /// Return the process-wide singleton, initialising it on first call.
    pub fn get() -> &'static Self {
        static METERS: OnceLock<ServerMeters> = OnceLock::new();
        METERS.get_or_init(|| {
            let meter = opentelemetry::global::meter("simulacra-server");

            // active_tasks — observable gauge per tenant
            let active_tasks_map = Arc::new(Mutex::new(HashMap::<String, i64>::new()));
            let tasks_map_cb = active_tasks_map.clone();
            let _active_tasks_gauge = meter
                .i64_observable_gauge("simulacra.server.active_tasks")
                .with_description("Concurrent active tasks by tenant")
                .with_callback(move |observer| {
                    let map = tasks_map_cb.lock().unwrap();
                    for (tenant, &count) in map.iter() {
                        observer.observe(count, &[KeyValue::new("tenant", tenant.clone())]);
                    }
                })
                .build();

            // active_connections — observable gauge per transport
            let active_connections_map = Arc::new(Mutex::new(HashMap::<String, i64>::new()));
            let conns_map_cb = active_connections_map.clone();
            let _active_connections_gauge = meter
                .i64_observable_gauge("simulacra.server.active_connections")
                .with_description("Concurrent open connections by transport")
                .with_callback(move |observer| {
                    let map = conns_map_cb.lock().unwrap();
                    for (transport, &count) in map.iter() {
                        observer.observe(count, &[KeyValue::new("transport", transport.clone())]);
                    }
                })
                .build();

            ServerMeters {
                active_tasks_map,
                _active_tasks_gauge,
                active_connections_map,
                _active_connections_gauge,
                task_duration: meter
                    .f64_histogram("simulacra.server.task_duration")
                    .with_description("Task duration in seconds at terminal state")
                    .with_unit("s")
                    .build(),
                events_emitted: meter
                    .u64_counter("simulacra.server.events_emitted")
                    .with_description("Events emitted per event_type and tenant")
                    .build(),
                auth_failures: meter
                    .u64_counter("simulacra.server.auth_failures")
                    .with_description("Auth failures per provider and reason")
                    .build(),
                webhook_requests: meter
                    .u64_counter("simulacra.trigger.webhook_requests")
                    .with_description("Webhook requests by webhook_name, tenant, and status")
                    .build(),
                schedule_fires: meter
                    .u64_counter("simulacra.trigger.schedule_fires")
                    .with_description("Schedule fires by schedule_name and tenant")
                    .build(),
                missed_runs: meter
                    .u64_counter("simulacra.trigger.missed_runs")
                    .with_description("Missed schedule runs detected on startup")
                    .build(),
            }
        })
    }

    /// Adjust the active task count for `tenant` by `delta` (+1 or -1).
    pub fn add_active_tasks(&self, tenant: &str, delta: i64) {
        let mut map = self.active_tasks_map.lock().unwrap();
        let entry = map.entry(tenant.to_string()).or_insert(0);
        *entry += delta;
    }

    /// Adjust the active connection count for `transport` by `delta` (+1 or -1).
    pub fn add_active_connections(&self, transport: &str, delta: i64) {
        let mut map = self.active_connections_map.lock().unwrap();
        let entry = map.entry(transport.to_string()).or_insert(0);
        *entry += delta;
    }
}
