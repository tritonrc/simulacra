# S032 — Event Triggers

**Status:** Active
**Crates:** `simulacra-server` (extends S031), `simulacra-config`

## Dependencies

- **S006** — Resource budgets (trigger-created tasks inherit tenant budget pools)
- **S010** — Observability conventions
- **S026** — Governance hooks (triggered tasks run through same hook pipeline)
- **S031** — API server (triggers create tasks through the same `TaskManager`)

## Scope

External event triggers that create agent tasks: webhook receivers, cron/schedule-based task creation, and a trait-based interface for future event source subscriptions. All triggers create tasks through the same internal `TaskManager` path as the native API.

**In scope:**
- Webhook handler: HTTP endpoint in `simulacra-server` that receives external POSTs and creates tasks
- HMAC signature validation for webhook security
- Payload templating: extract fields from webhook body into task description
- Cron/schedule scheduler: background task in `simulacra-server` that creates tasks on a time schedule
- Standard cron syntax (5-field)
- Missed schedule policy: skip, run-once, backfill
- `EventSource` trait: pluggable interface for external event sources (Kafka, SQS, pub/sub)
- `[[webhooks]]` and `[[schedules]]` config sections in `simulacra.toml`
- Triggers are tenant-scoped: a webhook belongs to a tenant, runs with that tenant's config

**Out of scope:**
- Specific `EventSource` implementations (Kafka, SQS, pub/sub — S032 defines the trait only)
- Webhook response bodies (webhooks return 200 OK with task_id, nothing more)
- Complex workflow orchestration (chaining triggers, conditional branching)
- Webhook retry / dead-letter queue (the sender retries, not Simulacra)
- Schedule timezone handling beyond UTC (future enhancement)
- Dynamic trigger creation via API (triggers are config-defined in S032)

## Context

Simulacra agents should be triggered by any event, not just human chat. A webhook from Salesforce when a new customer signs up. A cron job that runs a quarterly report at 9am on the first of the quarter. A message on a Kafka topic when an order ships.

The key principle: **the agent doesn't know if it was triggered by John in Slack, a cron job at 9am, or a webhook from Salesforce.** Same task, same governance, same audit trail. Triggers are just another way to create tasks through the `TaskManager`.

This is critical for enterprise adoption. Business processes don't start with a human typing in a chat box. They start with events — a CRM update, a calendar trigger, an incoming email, a monitoring alert. Simulacra must be event-driven, not just chat-driven.

S032 defines webhooks and schedules as first-class trigger types, plus an `EventSource` trait for future pluggable event sources. All triggers flow through the same path: authenticate → resolve tenant → create task → agent executes → journal records everything.

## Design

### Trigger → Task flow

```
External Event (webhook POST, cron tick, event source message)
  │
  ▼
Trigger Handler (webhook endpoint, scheduler, event source adapter)
  │ Validates: signature, schema, tenant
  │
  ▼
TaskManager.create_task(tenant, description, agent_type, metadata)
  │ Same path as native API task.create
  │
  ▼
Agent executes with tenant config (VFS root, budget, hooks)
  │ Agent has no knowledge of trigger source
  │
  ▼
Journal records trigger metadata (source, payload hash, schedule name)
```

### Webhook handler

Webhook endpoints are registered from `[[webhooks]]` config at server startup. Each webhook gets a unique path.

```
POST /hooks/{webhook_name}
Headers:
  X-Simulacra-Signature: sha256=<hmac_hex>
  Content-Type: application/json
Body:
  { ...arbitrary JSON payload... }
```

Validation flow:
1. Look up webhook config by path
2. Compute HMAC-SHA256 of raw request body using the webhook's secret
3. Compare with `X-Simulacra-Signature` header (constant-time comparison)
4. If signature invalid → 401 Unauthorized
5. Parse JSON body
6. Apply payload template: substitute `{{payload.field}}` references with values from the body
7. Create task through `TaskManager` with the resolved tenant config
8. Return `200 OK` with `{ "task_id": "..." }`

### Payload templating

Simple Mustache-style substitution with dot-path access:

```
Template: "New customer: {{payload.company_name}}. Contact: {{payload.contact.email}}."
Payload:  { "company_name": "Acme Corp", "contact": { "email": "j@acme.com" } }
Result:   "New customer: Acme Corp. Contact: j@acme.com."
```

- `{{payload.<path>}}` accesses nested fields via dot notation
- Missing fields are replaced with `<missing: payload.field_name>`
- The full payload is also available to the agent as task metadata (so the agent can access fields not in the template)

### Cron scheduler

A background tokio task that evaluates cron expressions and creates tasks when they fire.

```rust
pub struct ScheduleEntry {
    pub name: String,
    pub cron: CronExpression,
    pub tenant: String,
    pub task: String,
    pub agent_type: String,
    pub missed_policy: MissedPolicy,
    pub enabled: bool,
}

pub enum MissedPolicy {
    Skip,       // If the server was down, skip missed runs
    RunOnce,    // Run once on startup if any runs were missed
    Backfill,   // Run once for each missed interval (capped at 10)
}
```

The scheduler:
1. Loads `[[schedules]]` from config at startup
2. Calculates the next fire time for each schedule
3. Sleeps until the nearest fire time
4. On fire: creates task through `TaskManager` with the schedule's tenant config
5. Records the last fire time (persisted to disk for missed-policy evaluation on restart)
6. If server was down and missed runs occurred, applies the configured `MissedPolicy`

### EventSource trait (future-ready)

```rust
/// Pluggable event source for external message systems.
/// S032 defines the trait; specific implementations are follow-up specs.
#[async_trait]
pub trait EventSource: Send + Sync {
    /// Human-readable name of this event source type (e.g., "kafka", "sqs").
    fn source_type(&self) -> &str;

    /// Start consuming events. Calls the provided callback for each event.
    /// Runs until the cancellation token is triggered.
    async fn start(
        &self,
        config: serde_json::Value,
        callback: Box<dyn Fn(EventMessage) -> BoxFuture<'static, ()> + Send + Sync>,
        cancel: CancellationToken,
    ) -> Result<(), EventSourceError>;
}

pub struct EventMessage {
    pub source_type: String,
    pub source_id: String,
    pub payload: serde_json::Value,
    pub metadata: HashMap<String, String>,
    pub timestamp: DateTime<Utc>,
}
```

The `EventSource` trait is intentionally minimal. Implementations handle connection management, offset tracking, and acknowledgment internally. The callback receives an `EventMessage` which is mapped to a task creation through the same `TaskManager` path.

### Config

```toml
[[webhooks]]
name = "new-customer-onboarding"
path = "/hooks/new-customer"
tenant = "csm"
task_template = "New customer: {{payload.company_name}}. Draft welcome sequence."
agent_type = "csm-agent"
secret = "WEBHOOK_SECRET_CSM"

[[webhooks]]
name = "incident-alert"
path = "/hooks/incident"
tenant = "ops"
task_template = "Incident: {{payload.title}}. Severity: {{payload.severity}}. Investigate and draft response."
agent_type = "ops-agent"
secret = "WEBHOOK_SECRET_OPS"

[[schedules]]
name = "q1-variance-report"
cron = "0 9 1 1,4,7,10 *"
tenant = "accounting"
task = "Generate quarterly revenue variance report against forecast"
agent_type = "accounting-agent"
missed_policy = "run-once"

[[schedules]]
name = "daily-inbox-triage"
cron = "0 8 * * 1-5"
tenant = "csm"
task = "Triage new support tickets from overnight. Categorize, prioritize, draft responses for P1s."
agent_type = "csm-agent"
missed_policy = "skip"
```

- `secret` references an environment variable name (not the secret itself — secrets never in config files)
- `missed_policy` defaults to `skip` if omitted
- `cron` uses standard 5-field syntax (minute, hour, day-of-month, month, day-of-week)

### Config types

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    pub name: String,
    pub path: String,
    pub tenant: String,
    pub task_template: String,
    pub agent_type: String,
    pub secret: String,  // Env var name
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleConfig {
    pub name: String,
    pub cron: String,
    pub tenant: String,
    pub task: String,
    pub agent_type: String,
    #[serde(default)]
    pub missed_policy: MissedPolicy,
    #[serde(default = "default_true")]
    pub enabled: bool,
}
```

## Behavior

### Webhook handling

1. Webhook endpoints are registered from `[[webhooks]]` config at server startup.
2. Each webhook is mounted at the configured `path` (e.g., `/hooks/new-customer`).
3. POST requests to a webhook path trigger the validation flow.
4. HMAC-SHA256 signature is computed from the raw request body and the secret (read from environment variable).
5. Signature comparison uses constant-time equality to prevent timing attacks.
6. Invalid signature returns HTTP 401 with no task creation.
7. Missing `X-Simulacra-Signature` header returns HTTP 401.
8. Valid signature with unparseable JSON body returns HTTP 400.
9. Payload template substitution replaces `{{payload.*}}` references with values from the parsed body.
10. Missing template fields are replaced with `<missing: payload.field_name>` (not an error).
11. The task is created through `TaskManager.create_task()` with the webhook's tenant config.
12. The full webhook payload is attached as task metadata (available to the agent).
13. Webhook returns HTTP 200 with `{ "task_id": "..." }` on success.
14. Webhook returns HTTP 503 if the server cannot accept new tasks (e.g., overloaded).

### Cron scheduling

15. The cron scheduler starts as a background tokio task during server startup.
16. Schedules with `enabled = false` are loaded but not evaluated.
17. The scheduler evaluates cron expressions in UTC.
18. When a cron expression fires, a task is created through `TaskManager.create_task()` with the schedule's tenant config.
19. The scheduler records the last fire time for each schedule to persistent storage.
20. On server restart, the scheduler compares the current time with the last fire time to detect missed runs.
21. `missed_policy = "skip"`: missed runs are ignored. The next run happens at the next scheduled time.
22. `missed_policy = "run-once"`: if any runs were missed, exactly one task is created immediately.
23. `missed_policy = "backfill"`: one task is created for each missed interval, capped at 10.
24. Backfill tasks are created sequentially with a brief delay between them (not all at once).
25. Cron parsing errors at startup are logged as errors; the invalid schedule is disabled.

### Trigger-task equivalence

26. Tasks created by triggers are identical to tasks created by the native API — same state machine, same events, same journal, same governance hooks.
27. Trigger source metadata (webhook name + payload hash, or schedule name + fire time) is recorded in the task's journal entry.
28. The agent receives the task description; it has no API to query its trigger source.
29. Budget enforcement applies to triggered tasks identically: the tenant's budget pool governs spend.

### EventSource trait

30. `EventSource` trait is defined but no implementations ship in S032.
31. The trait's `start` method runs until the `CancellationToken` is triggered (graceful shutdown).
32. `EventMessage` carries source type, payload, and metadata — enough for the `TaskManager` to create a task.

## Assertions

### Webhook handling

- [x] Webhook endpoint is registered at the configured path during server startup.
- [x] POST to webhook path with valid HMAC signature creates a task and returns 200 with task_id.
- [x] POST with invalid HMAC signature returns 401 and does not create a task.
- [x] POST with missing `X-Simulacra-Signature` header returns 401.
- [x] POST with valid signature but unparseable body returns 400.
- [x] Payload template substitution replaces `{{payload.field}}` with values from body.
- [x] Nested payload access works: `{{payload.contact.email}}` resolves correctly.
- [x] Missing template field is replaced with `<missing: payload.field_name>`.
- [x] Webhook secret is read from environment variable, not from config value directly.
- [x] Full webhook payload is attached as task metadata.
- [x] Webhook-created task runs through the same governance hooks as API-created tasks.
- [x] HMAC comparison uses constant-time equality.

### Cron scheduling

- [x] Schedule is evaluated and fires at the correct time (within 1 second tolerance).
- [x] Schedule with `enabled = false` does not fire.
- [x] Cron expression parsing error disables the schedule and logs an error.
- [x] Scheduled task is created through `TaskManager` with the schedule's tenant config.
- [x] Last fire time is persisted and survives server restart.
- [x] `missed_policy = "skip"`: no task created for missed runs after restart.
- [x] `missed_policy = "run-once"`: exactly one task created after restart if runs were missed.
- [x] `missed_policy = "backfill"`: one task per missed interval, capped at 10.
- [x] Backfill tasks are created sequentially, not all at once.
- [x] Scheduled task runs through the same governance hooks as API-created tasks.

### Trigger-task equivalence

- [x] Webhook-created task has identical lifecycle states as API-created task.
- [x] Schedule-created task has identical lifecycle states as API-created task.
- [x] Trigger source metadata is recorded in the task's journal entry.
- [x] Agent cannot distinguish trigger source from task content.
- [x] Tenant budget pool applies to triggered tasks.

### EventSource trait

- [x] `EventSource` trait compiles and is object-safe (`Send + Sync`).
- [x] `EventMessage` struct contains source_type, payload, metadata, and timestamp.
- [x] Trait method signatures support async execution with cancellation.

## Observability (see S010)

- [x] `simulacra_webhook_received` span wraps each webhook invocation with `simulacra.trigger.webhook_name`, `simulacra.trigger.tenant`, `simulacra.trigger.valid` (bool).
- [x] `simulacra_schedule_fired` span wraps each schedule fire with `simulacra.trigger.schedule_name`, `simulacra.trigger.tenant`.
- [x] `simulacra.trigger.webhook_requests` counter tracks webhook requests with `webhook_name`, `tenant`, `status` (success/auth_failure/parse_error) labels.
- [x] `simulacra.trigger.schedule_fires` counter tracks schedule fires with `schedule_name`, `tenant` labels.
- [x] `simulacra.trigger.missed_runs` counter tracks missed schedule runs detected on startup with `schedule_name`, `missed_policy` labels.
- [x] `tracing::info!` on webhook task creation with webhook name, tenant, task_id.
- [x] `tracing::info!` on schedule task creation with schedule name, tenant, task_id.
- [x] `tracing::warn!` on webhook auth failure with webhook name and source IP.
- [x] `tracing::warn!` on missed schedule runs with schedule name, count, and policy applied.
- [x] `tracing::error!` on cron parse failure with schedule name and expression.
