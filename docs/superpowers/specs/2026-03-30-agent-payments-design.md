# S030 — Agent Spend Management

**Status:** Active
**Crates involved:** `simulacra-payments` (new), `simulacra-http`, `simulacra-hooks`, `simulacra-types`, `simulacra-runtime`, `simulacra-cli`

## Dependencies

- **S006** — Resource budgets (`max_cost` / `used_cost`)
- **S010** — Observability conventions
- **S024** — HTTP client (outbound request pipeline)
- **S026** — Governance hooks (operation types, verdict model)

## Scope

Pluggable payment provider trait, transparent HTTP 402 handling, payment governance hooks, and full audit trail. This spec defines the framework; specific provider adapters (Stripe, Lithic) are separate crates.

Full spec: `specs/S030-agent-payments.md`

## Design

### Why 402

HTTP 402 Payment Required has been "reserved for future use" since 1997. In a world where agents call paid APIs programmatically, 402 is the natural machine-to-machine payment signal. The server says "pay me," the agent pays, the server delivers. No human in the loop unless governance policy requires one.

The key insight: 402 handling should be transparent, like redirect handling. The agent calls `fetch("https://api.vendor.com/data")` and gets data back. It doesn't know or care that a payment happened in the middle. The runtime handles it.

### Two-Layer Control Model

```
┌─────────────────────────────────────┐
│ POLICY (governance hooks)           │  "Should we pay?"
│ - Spend policy JS hooks             │  - Per-vendor rules
│ - Auto-approve thresholds           │  - Time-of-day restrictions
│ - Human escalation rules            │  - Team budget policies
├─────────────────────────────────────┤
│ PHYSICS (resource budget)           │  "Can we pay?"
│ - max_cost hard cap                 │  - Immutable at runtime
│ - used_cost monotonic counter       │  - Zero means unlimited
│ - BudgetExhausted = full stop       │  - No override possible
└─────────────────────────────────────┘
```

Policy is flexible (hooks can approve, deny, escalate). Physics is absolute (budget exhausted = done). This separation means enterprise admins configure policy while the runtime enforces hard limits.

### 402 Flow

```
Agent: fetch("https://api.vendor.com/data")
  │
  ▼
simulacra-http: GET https://api.vendor.com/data
  │
  ▼
Server: 402 Payment Required
  {"payment_required": {"amount": "0.50", "currency": "USD", ...}}
  │
  ▼
simulacra-http: Parse payment_required from body
  │ (unparseable? return 402 to agent as-is)
  │
  ▼
Governance: Operation::Payment hook chain
  │ Deny? → PaymentError::PolicyDenied → agent sees error
  │ Escalate? → PaymentError::Escalated → agent sees "pending approval"
  │ Kill? → agent terminated
  │ Continue ↓
  │
  ▼
Budget: used_cost + 0.50 <= max_cost?
  │ No? → PaymentError::BudgetExhausted → agent sees "budget exhausted"
  │ Yes ↓
  │
  ▼
Provider: PaymentProvider::authorize(card, 0.50, vendor, reason)
  │ Error? → propagate to agent
  │ Ok(receipt) ↓
  │
  ▼
Budget: used_cost += 0.50
  │
  ▼
Journal: JournalEntryKind::Payment (before retry)
  │
  ▼
simulacra-http: Retry original GET (once)
  │
  ▼
Server: 200 OK {"data": ...}
  │
  ▼
Agent: receives 200 response (never saw the 402)
```

### PaymentProvider Trait

Abstract over payment rails. The trait is deliberately minimal:

- `create_card` — session start. One card per agent session.
- `authorize` — charge against the card. Returns receipt or error.
- `revoke_card` — session end. Clean up.

No `capture` vs `authorize` distinction (agents don't do two-phase payment). No refunds (future spec). No webhooks (the provider is called synchronously from the HTTP pipeline).

### Escalate Verdict

New verdict type extends S026. Only valid for `Operation::Payment`. Represents "this payment needs human approval before it can proceed."

The runtime produces an escalation entry in the journal and returns `PaymentError::Escalated` to the agent. The agent sees this as an error and can adapt (try a cheaper alternative, skip the paid API, ask the user).

The human approval UI is out of scope. S030 produces the escalation event; something else (admin dashboard, Slack bot, CLI prompt) consumes it.

### Virtual Card Lifecycle

```
Agent session starts
  │
  ▼ [payments.enabled = true]
PaymentProvider::create_card(agent_id, max_cost)
  │ Success → card stored in session state
  │ Failure → warning log, agent runs without payment capability
  │
  ... agent executes, payments happen via authorize() ...
  │
  ▼
Agent session ends (normal, kill, budget)
  │
  ▼
PaymentProvider::revoke_card(card)
```

### Config Principles

- `enabled = false` by default. Payments are opt-in.
- API keys referenced by env var name, never stored in config.
- Auto-approve threshold and human-approval-above are separate knobs. The gap between them is the "hooks decide" zone.
- Provider-specific config is only loaded for the active provider.

### Crate Dependencies

```
simulacra-types (leaf)
  ├→ simulacra-payments (trait + types only, no provider SDKs)
  ├→ simulacra-hooks (gains Operation::Payment, Verdict::Escalate)
  │    └→ uses simulacra-payments types in hook context
  ├→ simulacra-http (402 handler)
  │    └→ depends on simulacra-payments + simulacra-hooks
  └→ simulacra-runtime
       └→ simulacra-cli (wires provider, creates card at session start)
```

`simulacra-payments` is a leaf crate. It defines the trait and types. It has zero knowledge of HTTP, hooks, or runtime. Provider adapter crates (future) depend on `simulacra-payments` + the provider SDK.

### Testing Strategy

`MockPaymentProvider` implements `PaymentProvider` in-memory:
- Configurable success/failure responses
- Records all `authorize()` calls for assertion
- Simulated card balance tracking
- Deterministic — no network, no randomness

402 handling tested with recorded HTTP fixtures (fake server returns 402, verify the full flow through to retry).

### Security Considerations

- API keys are env var references, never in config files
- Virtual card budget is capped at agent's `max_cost` — provider-level enforcement mirrors runtime enforcement
- `require_human_approval_above` is a hard gate that hooks cannot override
- All payments are journaled with full context (who, what, why, how much, who approved)
- Card revocation on session end prevents orphaned spending authority
- No payment retries beyond the single retry of the original request — prevents runaway spend from retry storms
