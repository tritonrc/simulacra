# S032 — Event Triggers

**Status:** Active
**Crates involved:** `simulacra-server` (extends S031), `simulacra-config`

## Dependencies

- **S006** — Resource budgets (trigger-created tasks inherit tenant budget pools)
- **S010** — Observability conventions
- **S026** — Governance hooks (triggered tasks run through same hook pipeline)
- **S031** — API server (triggers create tasks through the same `TaskManager`)

## Scope

External event triggers — webhooks, cron schedules, and a trait for future event source subscriptions. All triggers create tasks through the same internal `TaskManager` path as the native API.

Full spec: `specs/S032-event-triggers.md`

## Design

### The Principle

**The agent doesn't know if it was triggered by John in Slack, a cron job at 9am, or a webhook from Salesforce.** Same task, same governance, same audit trail.

This is not a nice-to-have. It's the foundation of enterprise agent deployment. Business processes start with events — CRM updates, calendar triggers, monitoring alerts, incoming emails. If agents only respond to chat, they're toys. If agents respond to events, they're infrastructure.

### Trigger → Task Equivalence

```
┌─────────────────────┐     ┌─────────────────────┐     ┌─────────────────────┐
│ Webhook POST        │     │ Cron tick            │     │ Event source msg    │
│ (Salesforce, etc.)  │     │ (scheduler)          │     │ (Kafka, SQS, etc.)  │
└────────┬────────────┘     └────────┬────────────┘     └────────┬────────────┘
         │                           │                           │
         ▼                           ▼                           ▼
┌────────────────────────────────────────────────────────────────────────────┐
│  TaskManager.create_task(tenant, description, agent_type, metadata)       │
│  ← Same path as native API task.create                                   │
└────────────────────────────────────────────────────────────────────────────┘
         │
         ▼
┌────────────────────────────────────────────────────────────────────────────┐
│  Agent executes with tenant config                                        │
│  VFS root · Budget pool · Governance hooks · Journal · OTel               │
└────────────────────────────────────────────────────────────────────────────┘
```

All three trigger types converge on the same `TaskManager.create_task()` call. The only difference is metadata: the journal records *how* the task was triggered (webhook name + payload hash, schedule name + fire time, event source + message ID) for audit purposes. The agent itself never sees this.

### Webhooks

External systems POST to Simulacra, Simulacra creates a task:

```
POST /hooks/new-customer
X-Simulacra-Signature: sha256=abc123...
Content-Type: application/json

{"company_name": "Acme Corp", "contact": {"email": "j@acme.com"}}
```

Security model:
- **HMAC-SHA256** signature validation. The secret is stored in an env var (never in config files). Simulacra computes the HMAC of the raw body and compares with the header using constant-time equality.
- Unsigned requests are rejected (401). No "insecure mode."

Payload templating:
- `task_template = "New customer: {{payload.company_name}}. Draft welcome."` uses Mustache-style substitution
- Dot-path access into nested JSON: `{{payload.contact.email}}`
- Missing fields → `<missing: payload.field_name>` (not an error — the agent can work with partial info)
- Full payload is also available as task metadata, so the agent can access any field

### Cron Schedules

Time-based task creation using standard 5-field cron syntax:

```toml
[[schedules]]
name = "q1-variance-report"
cron = "0 9 1 1,4,7,10 *"
tenant = "accounting"
task = "Generate quarterly revenue variance report against forecast"
agent_type = "accounting-agent"
missed_policy = "run-once"
```

The scheduler is a background tokio task. It tracks last fire times persistently, so on restart it can detect missed runs and apply the configured policy:

| Policy | Behavior |
|---|---|
| `skip` | Ignore missed runs. Next run at next scheduled time. |
| `run-once` | If any runs were missed, create exactly one task now. |
| `backfill` | Create one task per missed interval, capped at 10. |

All times are UTC. Schedules with `enabled = false` are loaded but inactive.

### EventSource Trait (Future-Ready)

S032 defines the interface for pluggable event sources. No implementations ship yet.

```rust
#[async_trait]
pub trait EventSource: Send + Sync {
    fn source_type(&self) -> &str;
    async fn start(
        &self,
        config: serde_json::Value,
        callback: Box<dyn Fn(EventMessage) -> BoxFuture<'static, ()> + Send + Sync>,
        cancel: CancellationToken,
    ) -> Result<(), EventSourceError>;
}
```

Future specs will implement this for Kafka, SQS, Google Pub/Sub, etc. The trait is intentionally minimal — implementations own their own connection management, offset tracking, and acknowledgment.

### Config Topology

Triggers are tenant-scoped. A webhook config references a tenant. A schedule config references a tenant. When the trigger fires, the task runs with that tenant's full config: VFS root, budget pool, agent type, governance hooks.

```toml
# Webhook belongs to the CSM tenant
[[webhooks]]
name = "new-customer-onboarding"
tenant = "csm"
agent_type = "csm-agent"
...

# Schedule belongs to the accounting tenant
[[schedules]]
name = "q1-variance-report"
tenant = "accounting"
agent_type = "accounting-agent"
...
```

This means trigger governance is tenant governance. No separate permission model for triggers.
