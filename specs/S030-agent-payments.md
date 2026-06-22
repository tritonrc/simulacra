# S030 — Agent Spend Management

**Status:** Active
**Crates:** `simulacra-payments` (new), `simulacra-http`, `simulacra-hooks`, `simulacra-types`, `simulacra-runtime`

## Dependencies

- **S006** — Resource budgets (budget enforcement, `max_cost` / `used_cost`)
- **S010** — Observability conventions
- **S024** — HTTP client (`simulacra-http`, outbound request pipeline)
- **S026** — Governance hooks (payment hook operation type, verdict model)

## Scope

Agent spend management: pluggable payment providers, transparent 402 Payment Required handling in the HTTP layer, payment governance hooks, budget integration, and full payment audit trail via the journal.

**In scope:**
- New `simulacra-payments` crate — `PaymentProvider` trait, `VirtualCard`, `PaymentReceipt`, payment error types
- `PaymentProvider` trait — pluggable abstraction for card issuers (create card, authorize, revoke)
- 402 Payment Required handler in `simulacra-http` — transparent intercept, payment extraction, retry
- `payment` operation type in the governance hook pipeline (S026 extension)
- `Escalate` verdict — new verdict type for routing payments to human approval
- Payment journal entries (`JournalEntryKind::Payment`)
- Budget integration — payments deduct from `ResourceBudget.used_cost`
- `[payments]` config section in `simulacra.toml`
- `MockPaymentProvider` for testing (in-memory, deterministic)
- Auto-approve threshold (payments below threshold skip human approval)

**Out of scope:**
- Specific provider implementations (Stripe Issuing, Lithic, Marqeta — those are plugins)
- Crypto / stablecoin / blockchain payments
- Human approval UI (admin dashboard, Slack integration — those consume the escalation queue)
- Recurring billing / subscriptions
- Multi-currency conversion (provider's responsibility)
- Refunds / chargebacks (future spec)
- Payment retry with exponential backoff (single retry after successful payment)

## Context

HTTP 402 Payment Required has existed since HTTP/1.1 but was "reserved for future use" because the web was human-centric. In a machine-to-machine world where agents call paid APIs, 402 is exactly right: the server says "pay me," the client pays, the server delivers. This is HTTP's native payment flow, finally realized.

Agents are economic actors. An agent analyzing financial data might call a paid market data API. An agent doing research might hit a paid search endpoint. The runtime must handle this transparently — the agent asks for data, the runtime handles payment, the agent gets data. Like how HTTP clients transparently follow 301 redirects, `simulacra-http` transparently handles 402 responses.

But transparent doesn't mean uncontrolled. Every payment passes through the governance hook pipeline (S026). Enterprise policy determines what gets auto-approved, what needs human sign-off, and what gets denied. The budget (S006) is the hard physics — when the money runs out, the agent stops spending. Hooks are the policy layer on top.

The `PaymentProvider` trait abstracts the payment mechanism. S030 defines the interface; provider implementations are plugins. A Stripe Issuing adapter, a Lithic adapter, a mock for testing — all implement the same trait. The runtime doesn't know or care which payment rail is underneath.

## Design

### PaymentProvider trait

```rust
use rust_decimal::Decimal;

/// Pluggable payment provider abstraction.
/// Implementations: Stripe Issuing, Lithic, mock, etc.
pub trait PaymentProvider: Send + Sync {
    /// Create a virtual card for an agent session.
    fn create_card(
        &self,
        agent_id: &str,
        budget: Decimal,
    ) -> Result<VirtualCard, PaymentError>;

    /// Authorize a payment against a virtual card.
    fn authorize(
        &self,
        card: &VirtualCard,
        amount: Decimal,
        vendor: &str,
        reason: &str,
    ) -> Result<PaymentReceipt, PaymentError>;

    /// Revoke a virtual card (session end or budget exhausted).
    fn revoke_card(&self, card: &VirtualCard) -> Result<(), PaymentError>;
}
```

### Core types

```rust
pub struct VirtualCard {
    pub card_id: String,
    pub last_four: String,
    pub agent_id: String,
    pub budget: Decimal,
    pub spent: Decimal,
    pub currency: String,
    pub active: bool,
}

pub struct PaymentReceipt {
    pub authorization_id: String,
    pub card_last_four: String,
    pub amount: Decimal,
    pub currency: String,
    pub vendor: String,
    pub reason: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

pub enum PaymentError {
    /// Card has insufficient funds.
    InsufficientFunds { available: Decimal, requested: Decimal },
    /// Card has been revoked or is inactive.
    CardInactive { card_id: String },
    /// Payment provider rejected the transaction.
    ProviderRejected { reason: String },
    /// Governance hook denied the payment.
    PolicyDenied { reason: String },
    /// Payment was escalated to human approval.
    Escalated { reason: String, escalation_id: String },
    /// Budget exhausted — hard stop.
    BudgetExhausted { remaining: Decimal, requested: Decimal },
    /// Provider communication failure.
    ProviderError { source: Box<dyn std::error::Error + Send + Sync> },
}
```

### 402 Payment Required flow

The 402 handler lives in `simulacra-http`, layered on top of the existing HTTP pipeline. It is transparent to the agent.

```
Agent code calls fetch("https://api.vendor.com/data")
  → simulacra-http sends GET request
  → Server returns HTTP 402 Payment Required
  → simulacra-http intercepts 402 (does NOT return it to the agent)
  → Parses payment details from 402 response body
  → Constructs PaymentRequest from extracted details
  → Invokes governance hook pipeline (Operation::Payment, Phase::Before)
  → Hook returns Continue → proceed to payment
  → Hook returns Deny → return PaymentError::PolicyDenied to agent
  → Hook returns Escalate → return PaymentError::Escalated to agent
  → Checks ResourceBudget (hard stop)
  → Calls PaymentProvider::authorize()
  → Deducts amount from ResourceBudget.used_cost
  → Writes Payment journal entry
  → Retries original HTTP request (single retry)
  → Returns successful response to agent (agent never saw the 402)
```

### 402 response parsing

The 402 response body must contain payment details. S030 supports a structured JSON format:

```json
{
    "payment_required": {
        "amount": "0.50",
        "currency": "USD",
        "vendor": "api.datavendor.com",
        "payment_url": "https://api.datavendor.com/pay",
        "reason": "Financial data query — Q1 2026",
        "idempotency_key": "req_abc123"
    }
}
```

If the 402 body does not contain a parseable `payment_required` object, the 402 is returned to the agent as-is (not all 402s are machine-payable).

### Escalate verdict

S030 extends S026's verdict model with a new verdict type for payments:

```rust
pub enum Verdict {
    Continue { modified_context: Option<String> },
    Deny { reason: String },
    Kill { reason: String },
    /// Route to human approval queue. Payment-only.
    Escalate { reason: String },
}
```

`Escalate` is only valid for `Operation::Payment`. If returned for other operation types, it is treated as `Deny` with a warning log.

### Payment governance hook context

```json
{
    "vendor": "api.datavendor.com",
    "amount": "0.50",
    "currency": "USD",
    "reason": "Financial data query for Q1 analysis",
    "payment_url": "https://api.datavendor.com/pay",
    "agent_id": "agent-abc123",
    "user": "john@company.com",
    "team": "accounting",
    "session_spend_total": "2.30",
    "budget_remaining": "47.70",
    "auto_approve_eligible": true
}
```

`auto_approve_eligible` is true when the amount is below `auto_approve_threshold` from config. Hooks can override this (a hook may deny even auto-approvable payments based on vendor, time of day, etc.).

### Budget integration

Payments deduct from `ResourceBudget.used_cost` (S006). The budget check happens **after** the governance hook but **before** the provider call. This ordering is intentional:

1. Governance hooks decide policy (should we pay?)
2. Budget check enforces physics (can we pay?)
3. Provider executes the payment (do we pay?)

If `used_cost + amount > max_cost`, the payment fails with `PaymentError::BudgetExhausted`. The agent sees "budget exhausted" and must adapt (stop making paid calls, ask for budget increase, etc.).

### Journal

New journal entry kind:

```rust
pub struct PaymentJournalEntry {
    pub vendor: String,
    pub amount: Decimal,
    pub currency: String,
    pub card_last_four: String,
    pub authorization_id: String,
    pub reason: String,
    pub hook_decision: String,  // "auto-approved", "approved by spend-policy", etc.
    pub budget_remaining: Decimal,
    pub idempotency_key: Option<String>,
}
```

Recorded as `JournalEntryKind::Payment` after a successful payment, before the retry request is sent.

### Config

```toml
[payments]
enabled = false                          # opt-in, not opt-out
provider = "mock"                        # "stripe", "lithic", "mock", "manual"
default_currency = "USD"
auto_approve_threshold = "5.00"          # auto-approve payments under this amount
require_human_approval_above = "100.00"  # force escalation above this amount

[payments.stripe]
api_key_env = "STRIPE_API_KEY"           # env var name, never the key itself

[payments.lithic]
api_key_env = "LITHIC_API_KEY"

[[hooks.payment]]
name = "spend-policy"
runtime = "js"
module = "hooks/spend-policy.js"
timeout_ms = 200
```

Config types:

```rust
pub struct PaymentsConfig {
    pub enabled: bool,
    pub provider: String,
    pub default_currency: String,
    pub auto_approve_threshold: Decimal,
    pub require_human_approval_above: Option<Decimal>,
}
```

### Crate position

```
simulacra-types (leaf)
  ├→ simulacra-payments (PaymentProvider trait, types, MockPaymentProvider)
  ├→ simulacra-hooks (pipeline — gains Operation::Payment, Verdict::Escalate)
  ├→ simulacra-http (402 handler — depends on simulacra-payments + simulacra-hooks)
  └→ ...
       └→ simulacra-cli (wires PaymentProvider impl, passes to simulacra-http)
```

`simulacra-payments` depends only on `simulacra-types`. It does NOT depend on `simulacra-http` or any provider SDK. Provider SDKs are dependencies of the specific adapter crates (e.g., a future `simulacra-payments-stripe`), not of the core trait crate.

## Behavior

### 402 interception

1. When `simulacra-http` receives an HTTP 402 response, it attempts to parse the response body as a `payment_required` JSON object.
2. If the body contains a valid `payment_required` object, the 402 is intercepted and the payment flow begins. The 402 is NOT returned to the agent.
3. If the body does not contain a valid `payment_required` object, the 402 is returned to the agent as a normal HTTP response.
4. If `[payments] enabled = false`, all 402 responses are passed through to the agent unmodified.

### Payment authorization flow

5. After extracting payment details from the 402, `simulacra-http` invokes the governance hook pipeline with `Operation::Payment` and the payment context.
6. If no payment hooks are configured and the amount is below `auto_approve_threshold`, the payment proceeds automatically.
7. If no payment hooks are configured and the amount is at or above `auto_approve_threshold`, the payment is denied with reason "no payment policy configured for amount above auto-approve threshold."
8. If a hook returns `Deny`, the payment does not execute. `PaymentError::PolicyDenied` is returned. The agent sees a tool error with the denial reason.
9. If a hook returns `Escalate`, the payment does not execute. `PaymentError::Escalated` is returned. The agent sees a message indicating the payment is pending human approval.
10. If a hook returns `Kill`, the agent is terminated (same as S026 kill behavior).
11. If a hook returns `Continue`, the payment proceeds to budget check.

### Budget enforcement

12. After governance approval, the payment amount is checked against `ResourceBudget`. If `used_cost + amount > max_cost` (and `max_cost > 0`), the payment fails with `PaymentError::BudgetExhausted`.
13. If `max_cost = 0` (unlimited), the budget check passes regardless of amount.
14. Budget deduction (`used_cost += amount`) happens after the provider confirms the authorization, not before. If the provider rejects, the budget is unchanged.

### Provider execution

15. After budget check passes, `PaymentProvider::authorize()` is called with the virtual card, amount, vendor, and reason.
16. If the provider returns `Ok(PaymentReceipt)`, the payment succeeded.
17. If the provider returns any `PaymentError`, the payment failed. The error is propagated to the agent as a tool error.

### Journal

18. After a successful payment, a `JournalEntryKind::Payment` entry is written with vendor, amount, currency, card last four, authorization ID, reason, hook decision, and remaining budget.
19. The journal entry is written before the retry request is sent (journal before return invariant).
20. Failed payment attempts (denied by hook, budget exhausted, provider rejected) are NOT journaled as `Payment` entries. They are journaled as `HookDenial` (if denied by hook) or logged at WARN (if budget/provider failure).

### HTTP retry

21. After successful payment and journal write, `simulacra-http` retries the original HTTP request exactly once.
22. The retry uses the same method, URL, headers, and body as the original request.
23. If the retry also returns 402, it is returned to the agent as-is (no infinite payment loops).
24. If the retry returns any other status code, it is returned to the agent as the response.

### Virtual card lifecycle

25. `PaymentProvider::create_card()` is called during agent session initialization when `[payments] enabled = true`.
26. The card budget is set to the agent's `ResourceBudget.max_cost`.
27. `PaymentProvider::revoke_card()` is called when the agent session ends (normal exit, kill, or budget exhaustion).
28. If card creation fails, the agent starts without payment capability. 402 responses will be passed through as-is with a warning log.

### Escalation

29. `Verdict::Escalate` is only valid for `Operation::Payment`. If returned for any other operation type, it is treated as `Verdict::Deny` with reason "escalation is only valid for payment operations" and a warning is logged.
30. When `require_human_approval_above` is set and the payment amount exceeds it, the pipeline injects an automatic `Escalate` verdict before hooks run. Hooks cannot override this — it is a hard policy gate.
31. Escalated payments produce a journal entry of kind `JournalEntryKind::PaymentEscalated` with the escalation reason and a reference ID for the approval queue.

### Config parsing

32. `[payments]` section is optional. If absent, payments are disabled (equivalent to `enabled = false`).
33. `[payments] enabled = false` disables all payment handling. 402 responses pass through.
34. `provider` must be a recognized provider name. Unrecognized names return a config error at startup.
35. `auto_approve_threshold` defaults to `"0.00"` if not set (all payments require hooks or human approval).
36. `default_currency` defaults to `"USD"`.
37. Provider-specific sub-sections (e.g., `[payments.stripe]`) are parsed only when that provider is selected.
38. `api_key_env` specifies the environment variable name, never the raw key. Config parsing validates that the env var exists at startup and returns a clear error if missing.

## Assertions

### 402 interception

- [ ] HTTP 402 with valid `payment_required` body is intercepted (not returned to agent).
- [ ] HTTP 402 without `payment_required` body is passed through to agent.
- [ ] HTTP 402 interception is skipped when `[payments] enabled = false`.
- [ ] `payment_required` body with missing required fields (amount, currency, vendor) is treated as unparseable — 402 passes through.

### Payment authorization

- [ ] Payment below `auto_approve_threshold` with no hooks configured proceeds automatically.
- [ ] Payment at or above `auto_approve_threshold` with no hooks configured is denied.
- [ ] Payment denied by hook returns `PaymentError::PolicyDenied` with reason.
- [ ] Payment escalated by hook returns `PaymentError::Escalated` with reason and escalation ID.
- [ ] `Kill` verdict from payment hook terminates the agent.
- [ ] `Continue` with modified context passes modification to next hook and to payment execution.
- [ ] Multiple payment hooks chain in config order (same as S026).

### Budget enforcement

- [ ] Payment fails with `BudgetExhausted` when `used_cost + amount > max_cost`.
- [ ] Payment passes budget check when `max_cost = 0` (unlimited).
- [ ] Budget `used_cost` is incremented only after provider confirms authorization.
- [ ] Budget is unchanged when provider rejects the payment.

### Provider

- [ ] `PaymentProvider::authorize()` is called with correct card, amount, vendor, reason.
- [ ] Provider `InsufficientFunds` error is propagated to the agent.
- [ ] Provider `CardInactive` error is propagated to the agent.
- [ ] Provider `ProviderRejected` error is propagated to the agent.

### Journal

- [ ] Successful payment produces a `JournalEntryKind::Payment` entry with all required fields.
- [ ] Journal entry is written before the retry request is sent.
- [ ] Failed payments (denied, budget exhausted, provider rejected) do not produce `Payment` journal entries.
- [ ] Escalated payments produce a `JournalEntryKind::PaymentEscalated` entry.

### HTTP retry

- [ ] After successful payment, original request is retried exactly once.
- [ ] Retry uses same method, URL, headers, body as original request.
- [ ] Second 402 on retry is returned to agent (no infinite payment loop).
- [ ] Non-402 retry response is returned to agent.

### Virtual card lifecycle

- [ ] Card is created during agent session init when payments enabled.
- [ ] Card budget matches agent's `max_cost`.
- [ ] Card is revoked on agent session end.
- [ ] Card creation failure degrades gracefully (agent starts, 402s pass through, warning logged).

### Escalation

- [ ] `Escalate` verdict on non-payment operation is treated as `Deny` with warning.
- [ ] `require_human_approval_above` forces escalation regardless of hook verdicts.
- [ ] Escalation produces `PaymentEscalated` journal entry with reference ID.

### Config

- [ ] Missing `[payments]` section means payments disabled.
- [ ] `enabled = false` disables 402 interception.
- [ ] Unrecognized provider name returns config error at startup.
- [ ] `auto_approve_threshold` defaults to `"0.00"`.
- [ ] `api_key_env` is validated at startup — missing env var returns clear error.
- [ ] Provider-specific config is only parsed when that provider is selected.

## Observability (see S010)

- [ ] `simulacra_payment_authorize` span per payment attempt with `simulacra.payment.vendor`, `simulacra.payment.amount`, `simulacra.payment.currency`, `simulacra.payment.verdict`.
- [ ] `simulacra.payments.authorized` counter with `vendor`, `currency` labels.
- [ ] `simulacra.payments.denied` counter with `vendor`, `reason_category` labels (policy, budget, provider).
- [ ] `simulacra.payments.escalated` counter with `vendor` label.
- [ ] `simulacra.payments.total_spend` counter (monotonic) with `vendor`, `currency` labels — tracks cumulative spend.
- [ ] `simulacra.payments.budget_remaining` gauge updated after each payment.
- [ ] `tracing::info!` on successful payment with vendor, amount, authorization ID.
- [ ] `tracing::warn!` on payment denied (policy or budget).
- [ ] `tracing::warn!` on payment escalated.
- [ ] `tracing::error!` on provider failure.
- [ ] `tracing::warn!` on card creation failure during session init.
- [ ] `tracing::debug!` on 402 interception with URL and parsed payment details.
