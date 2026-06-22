# Simulacra — Product Vision

## The Problem

Every enterprise is being asked to deploy AI agents. Business units want them NOW. CIOs are caught between demand and risk:

- Developers are already running OpenClaw/Claude Code on company machines (shadow AI)
- Security teams are panicking — agents have full host access, no audit trails, secrets in env vars
- Compliance needs provable audit trails for everything an agent does
- Finance has no visibility into what agents cost
- Nobody can answer: "what did the agent do, why did it do it, and who authorized it?"

The market has agent capabilities (OpenClaw, Claude Code, Cowork) but no governance. Running these on enterprise infrastructure is a non-starter for any CISO.

But there's a second problem that hits even harder: **the last mile.**

A team at a hackathon builds a brilliant onboarding automation. It pulls from HubSpot, writes to Linear, messages in Slack. Then Monday comes. Where does it run? How does it access those APIs with real credentials? Who maintains it? The tool dies on a laptop or gets dumped on Vercel with hardcoded tokens. Multiply this by every team, every quarter, every business unit trying to ship agent-powered workflows.

The gap isn't capability — teams can build agents. The gap is **infrastructure**: deployment, integration management, credential governance, and operational lifecycle. Every team solves (or fails to solve) the same problems independently.

## The Product

Simulacra is an **agent workforce platform** — the full-stack infrastructure for running AI agents as first-class participants in an enterprise. It combines agent capabilities with governance, integration management, deployment infrastructure, and operational tooling.

Think of it as: **OpenClaw for enterprises** crossed with **workflow management** crossed with **hosting/deployment infrastructure.**

**The users** are John in accounting and Francine in customer success. They @mention the bot, describe what they need, and the agent does it. They don't know or care about the technology underneath.

**The builder** is the team at the hackathon who creates an onboarding agent over the weekend — and needs it running in production on Monday with proper credentials, audit trails, and no Vercel.

**The buyer** is the CIO who needs to enable AI agents org-wide with controls that satisfy the CISO, comply with regulations, and give finance cost visibility.

## The Two-Layer Architecture

```
┌──────────────────────────────────────────────────┐
│  Agent Layer (LLM)                               │
│  Reasons, plans, orchestrates, decides           │
│  Reads files, discovers tools, makes decisions   │
│  Sees: VFS namespace (/proc, /mcp, /mnt, /etc)  │
└──────────────────┬───────────────────────────────┘
                   │ invokes
┌──────────────────▼───────────────────────────────┐
│  Capability Layer                                │
│  JavaScript · Python · Shell · WASM · Rust       │
│  Does the actual work:                           │
│    fetch() → HTTP APIs (HubSpot, Slack, Linear)  │
│    fs.read/write → data processing               │
│    subprocess → system operations                │
│  Governed by: capabilities, hooks, budgets, VFS  │
└──────────────────────────────────────────────────┘
```

The LLM doesn't call Salesforce. It writes a JS script that calls Salesforce. Or invokes a Python function. Or triggers a WASM module. The **capability layer** is where outcomes happen. The **agent layer** is where decisions happen.

This separation is critical because:
- The LLM needs *discovery* — what can I connect to, what's the schema?
- The capability layer needs *access* — credentials, network permissions, API endpoints
- Governance spans both layers uniformly — hooks, capabilities, journal, budget

## Core Capabilities

### 1. Integration Fabric — The Hosting Problem, Solved

The number one blocker for enterprise agent adoption is not intelligence — it's plumbing. Every agent needs access to HubSpot, Linear, Slack, Salesforce, Jira, BigQuery, S3, internal APIs. Today, each team independently:
- Figures out OAuth flows for each service
- Manages API keys and refresh tokens
- Handles rate limits and error retries
- Deploys somewhere that can reach internal services
- Prays nobody leaks credentials

**Simulacra provides a managed integration fabric.** Integrations are configured once by an admin, mounted into agent namespaces, and governed centrally.

```toml
# Admin configures once:
[integrations.hubspot]
type = "oauth2"
client_id = "HUBSPOT_CLIENT_ID"        # env var reference
client_secret = "HUBSPOT_CLIENT_SECRET"
scopes = ["crm.objects.contacts.read", "crm.objects.deals.read"]
token_refresh = true

[integrations.slack]
type = "oauth2"
client_id = "SLACK_CLIENT_ID"
client_secret = "SLACK_CLIENT_SECRET"
scopes = ["chat:write", "channels:read"]

[integrations.linear]
type = "api_key"
key = "LINEAR_API_KEY"
```

```toml
# Agents get access via their tenant config:
[tenants.onboarding]
agent_type = "onboarding-agent"
integrations = ["hubspot", "slack", "linear"]
# Agent sees: /mnt/hubspot/, /mnt/slack/, /mnt/linear/
# Credentials are NEVER exposed to the agent or LLM
```

**What this means for the hackathon team:** They build their onboarding agent over the weekend. On Monday, the admin adds it as a Simulacra agent type, grants it access to HubSpot + Linear + Slack, and it's running in production. No credential management. No deployment pipeline. No Vercel. The agent runs inside Simulacra with governed access to every service it needs.

**What this means for the CISO:** Every API credential is managed centrally. Token refresh is automatic. Access is auditable. Credential rotation doesn't require touching agent code. Revoking access is one config change.

### 2. Deployment Infrastructure — Where Agents Live

Agents need a place to run. Not a laptop. Not Vercel. Not a Kubernetes cluster that the team has to manage.

Simulacra is the deployment target:

**Single binary, multi-tenant.** One Simulacra instance runs accounting agents, onboarding agents, customer success agents, and ops agents — each isolated by namespace with their own VFS root, budget, hooks, and integration access.

**Multiple deployment modes:**
- **SaaS** — Simulacra-hosted, multi-tenant, agents run on our infrastructure
- **Managed BYOC** — Simulacra-managed, runs in the customer's cloud (VPC, private subnets)
- **Self-hosted** — customer runs the binary, we provide support and updates

**Multiple trigger surfaces:**
- Human: @mention in Slack/Teams, chat in web widget
- Scheduled: cron jobs, calendar triggers
- Event-driven: webhooks from CRM/ticketing/monitoring
- Agent-to-agent: one agent spawning or messaging another
- API: programmatic task creation from internal systems

The hackathon team doesn't think about hosting. They define an agent type (system prompt, capabilities, integrations) and Simulacra runs it. Scaling, isolation, credential injection, logging, monitoring — all handled by the platform.

### 3. Task Execution

Users describe what they need in natural language. The agent executes — with the right tools, permissions, and budget. One-off tasks, recurring tasks, event-triggered tasks.

The agent has access to the capability layer — JS, Python, Shell, WASM — which is where actual work happens. The capability layer has governed access to external services through the integration fabric. The agent discovers what's available by browsing the VFS.

### 4. Governance by Construction

Not "we trust the AI to behave." The runtime enforces:

- **Capability tokens** — agents can only do what's explicitly permitted
- **Governance hooks** — Rack-style middleware that scans, filters, and gates every operation
- **Resource budgets** — hard caps on tokens, turns, cost, compute
- **Sandboxed execution** — VFS, WASM, filtered env, gated network
- **Full audit trail** — every tool call, every LLM turn, every hook decision journaled
- **Integration governance** — credentials never exposed to agents, access revocable per-service

A hook written in 20 lines of JavaScript blocks PII from leaving the system. Not because the model chose not to leak it — because the runtime physically prevented it.

### 5. Auditable Artifacts

Agents produce outputs — reports, analyses, emails, spreadsheets. Every artifact carries a provable audit trail: what data went in, what tools processed it, what hooks scanned it, who authorized the agent to run.

### 6. Workflow Hardening

The killer differentiator. Over repeated runs, agent workflows evolve from expensive LLM reasoning into cheap deterministic scripts:

**Phase 1: Passive.** Agent uses LLM reasoning to figure out a task. Expensive, non-deterministic. But the journal records every step.

**Phase 2: Learned.** Agent recognizes a repeated task, uses an extracted skill. Faster, cheaper, more consistent.

**Phase 3: Hardened.** After N stable runs, the workflow is extracted as a deterministic script. Runs without LLM. Costs pennies. Identical quality every time. LLM re-engages only when the script fails.

**Phase 4: Proactive.** The platform analyzes agent activity across ALL users. Detects patterns no individual would see. Surfaces automation opportunities to the CIO.

An artifact from a hardened workflow carries proof: "Generated by `revenue-variance.py`, extracted from 47 successful LLM-guided runs, approved by the accounting team, deterministic." The CIO can sign off on that. The auditor can verify it.

### 7. Agent Spend Management

Agents are economic actors. They consume paid external services.

**Virtual cards** — pre-funded per agent/session/team. Spending cap enforced by the card issuer. Every transaction traced and attributed.

**402 Payment Required** — the HTTP status code reserved since 1997 for machine-to-machine payments. An API returns 402, the agent's payment system handles it transparently (like redirect handling), governance hooks approve the spend, the request retries. The agent never thinks about money. The CIO sees every dollar.

### 8. Agent Personality

Agents have identity — a role-based persona with institutional knowledge, communication style, and behavioral traits. "Atlas, the Accounting Assistant" knows your chart of accounts, your fiscal year calendar, and speaks in clear business language. Personality is data (config + VFS files), not code.

## The Agent Workforce Platform

Agents are employees. They need the same organizational infrastructure as human workers.

### Agent HR — Lifecycle Management

Create agents (hiring). Configure capabilities and integrations (role assignment). Assess performance (reviews). Deprecate and retire (offboarding). Version agent types. Promote from staging to production.

An admin creates an "onboarding-agent" type with access to HubSpot, Linear, and Slack. Deploys it with a budget of $50/month. Three months later, reviews its journal — it completed 847 onboarding tasks, hardened 12 workflows into scripts, had 3 governance denials (all correct). The admin promotes it from "pilot" to "production" and increases its budget.

### Agent Spend Management — Budgets and Payments

Per-agent, per-team, per-project budgets. Virtual cards for external purchases. 402 payment flows. Cost attribution down to individual tasks. Finance gets a dashboard showing: this agent type costs $X/month, processes Y tasks, saves Z hours of human work.

### Agent Security & Compliance — Governance at Scale

Capability tokens, governance hooks, DLP scanning, behavioral monitoring. But also: compliance reporting ("show me every agent action that touched PII in Q1"), risk modeling ("this agent type has a 2% denial rate — investigate"), and the "hippocratic oath" enforcement — agents literally cannot take actions that governance rules prohibit, regardless of what the LLM wants to do.

### Agent Management — Performance and Accountability

Are agents doing what they're supposed to do? Grading work product. Measuring task success rates. SLA monitoring. Detecting drift (an agent that used to complete tasks in 3 turns now takes 8 — why?). Flagging anomalies. The journal is the performance record — every decision, every tool call, every outcome is auditable.

### Agent Collaboration — Working Together

Agent-to-agent: one agent spawns another for a subtask, or agents communicate through shared VFS mounts. Agent-to-human: approval workflows, input requests, Slack threads. Shared workspaces: agents read and write to shared `/tmp/` or `/mnt/` paths. The collaboration model is the filesystem — no special messaging protocol.

### Agent Training & Development — The Virtuous Loop

How do you ensure agents don't repeat the same discovery and learning every time?

**Level 1: Agent memory.** `/var/memory/` in VFS. An agent learns that the quarterly report pulls from tables X, Y, Z in BigQuery. It writes that to memory. Next quarter, it reads memory first. Individual agent gets smarter over time.

**Level 2: Organizational knowledge.** Skill extraction across agents. Agent-A figures out how to reconcile accounts. That workflow hardens into a skill. Agent-B (different team, different tenant) gets access to the same skill library. The org gets smarter, not just one agent.

**Level 3: Platform intelligence.** Cross-tenant pattern detection. Simulacra notices that 40% of accounting agents across all customers perform the same quarterly variance analysis. It extracts a shared skill, offers it to all tenants. The *platform* gets smarter. This is the network effect — every customer's agent work improves every other customer's agents.

The capability layer is where this compounds. When a workflow hardens from LLM reasoning into a deterministic Python script, that script IS the institutional knowledge. It's auditable, versioned, testable, fast, cheap. And critically — it's **code that can be built upon.**

```
LLM reasons through task (expensive, non-deterministic)
  → Agent writes JS/Python to automate it (still agent-driven)
    → Script stabilizes across N runs (deterministic, cheap)
      → Script becomes a shared skill (organizational asset)
        → Platform detects patterns across orgs (network effect)
          → Future agents inherit the skill AND build on top of it
```

### The compounding knowledge loop

This is not just caching. It's **institutional knowledge that versions and evolves.**

Agent-A figures out how to reconcile quarterly accounts. The workflow hardens into `reconcile-accounts.py` (v1). Three months later, the finance team restructures their chart of accounts. Agent-A's LLM re-engages, adapts the script, produces `reconcile-accounts.py` (v2). The diff is reviewable. The old version is preserved. The adaptation is auditable.

Meanwhile, the platform observes: Agent-A (accounting, Acme Corp) and Agent-B (accounting, Beta Inc) both wrote nearly identical quarterly reconciliation scripts. It extracts the common pattern into a shared skill: `quarterly-reconciliation` — parameterized by chart of accounts, fiscal calendar, and data source. Both agents now use the shared skill. When one improves it, the improvement is available to the other.

This is how the platform gets smarter:

**Observation:** The platform continuously analyzes agent journals across all tenants. Not the data (which is tenant-isolated) — the *patterns*. What tool sequences appear repeatedly? What scripts stabilize? What workflows recur across different organizations?

**Extraction:** When a pattern crosses a confidence threshold, the platform extracts it: "47 different accounting agents across 12 customers all perform a monthly close process with the same 5-step structure." It generates a parameterized skill template.

**Proposal:** The platform surfaces the pattern to admins: "We've detected a shared workflow for monthly close. 12 of your peers use this pattern. Want to adopt it?" The admin reviews, approves, and the skill appears in the library.

**Evolution:** Skills are versioned code, not frozen artifacts. An agent using `monthly-close` v3 can propose improvements. The improvement goes through governance review. If approved, it becomes v4. Every agent using the skill gets the upgrade.

The result: **every agent run is a contribution to the organization's (and the platform's) institutional knowledge.** The 100th time an agent does a quarterly report, it costs pennies, runs deterministically, and produces a higher-quality output than the 1st time — because it's building on the accumulated learning of every previous run, every peer agent, and every customer who's solved a similar problem.

## The Filesystem Is the Platform

The deepest architectural bet in Simulacra is that **the virtual filesystem is the universal interface for agents.** Not tools. Not protocols. Files.

This is the Plan 9 insight applied to AI: everything that matters — runtime state, external services, inter-agent communication, configuration, integrations — is mounted in the agent's filesystem namespace. The agent reads and writes files. That's it.

### Why this works for agents (when it didn't fully land for humans)

Plan 9's "everything is a file" model was technically superior but struggled with adoption because developers found it unfamiliar. The critical difference: **LLM agents are natural filesystem users.** An agent doesn't get confused by the abstraction. It doesn't care whether `/mcp/slack/send_message` is a "real file" or a virtual mount backed by an API call. It reads a path, gets data. It writes to a path, something happens. The cognitive mismatch that limited Plan 9 doesn't apply.

### The three roles of the VFS

The VFS serves the two-layer architecture differently:

**1. Discovery (LLM layer).** The agent reads files to understand its world. What am I? What can I connect to? What skills exist? What did I learn last time?

```
/proc/           Runtime introspection — identity, budget, capabilities
/etc/            Configuration, personality, governance rules
/var/skills/     Available skills and their schemas
/var/memory/     What this agent learned in previous sessions
/svc/            Available integrations and their capabilities
```

The LLM doesn't call HubSpot. It reads `/svc/hubspot/README.md` to understand what's available, reads `/var/skills/hubspot/create-contact/` to find a pre-built skill, and invokes it through the capability layer.

**2. Configuration (capability layer).** JS/Python/Shell read mounted config and credentials to know how to connect to services. The VFS provides endpoints and injected auth; the capability layer's native I/O (`fetch()`, `requests.get()`) does the actual work.

```
/etc/integrations/hubspot.json    → endpoint URL, rate limit config
# Credentials injected by the mount layer — never visible as file content
```

The capability layer doesn't need the VFS to invoke APIs — it has `fetch()`. What it needs from the VFS is knowing *what to call and how*, with credentials injected transparently.

**Why not force invocation through VFS?** Plan 9 solved request/response over files using file descriptors — you open a file, write a query, read the response on the same fd. The fd carries session state between the write and read. Simulacra's VFS is stateless (`read(path)` and `write(path, data)` are independent calls with no shared fd). We could add file descriptors, but that would mean reworking every VFS layer for a problem the capability layer already solves natively. The right boundary: VFS for discovery, configuration, and governed state. Native I/O (`fetch()`, `requests.get()`) for invocation. Governance hooks see both — the VFS reads during discovery AND the network calls during execution.

**3. Governed state (both layers).** Persistent data that needs audit trail and capability gating: agent memory, workflow checkpoints, artifacts, shared data.

```
/var/memory/     Persistent knowledge across sessions
/var/state/      Workflow checkpoints and resumable state
/proc/mailbox/   Artifact output
/mnt/            Governed data mounts (CRM records, query results)
/tmp/            Scratch space for collaborating agents
```

### Pre-baked integrations: skills, not just plumbing

Here's where the knowledge loop and the VFS intersect. Today, every agent that wants to use HubSpot has to learn the HubSpot API from scratch — discover endpoints, figure out auth, handle pagination, manage rate limits. That's hundreds of tokens of LLM reasoning, repeated every time, by every agent, at every customer.

**Integrations should be pre-baked skills.** "HubSpot" isn't just a credential mount and an API endpoint — it's a versioned, approved, platform-provided skill package:

```
/var/skills/
  hubspot/                          ← platform-provided, pre-baked
    README.md                       ← what this integration can do
    create-contact/
      schema.json                   ← input schema
      skill.py                      ← hardened script (v3.2)
      CHANGELOG.md                  ← version history
    search-deals/
      schema.json
      skill.py
    sync-pipeline/
      schema.json
      skill.py
  linear/                           ← platform-provided
    create-issue/
      ...
    sync-cycle/
      ...
  quarterly-reconciliation/         ← org-extracted skill
    schema.json
    skill.py
    PROVENANCE.md                   ← "extracted from 47 runs across 3 agents"
  onboarding-flow/                  ← team-created skill
    schema.json
    skill.js
```

### How skills get into the system

Skills arrive through three distinct paths — but they all land in the same namespace, governed the same way.

**Path 1: Enterprise provisioning.** The CIO signs off: "We use HubSpot, Slack, and Linear." IT configures credentials, the admin enables those integrations. Skills for those services are pre-loaded from the Simulacra marketplace — production-grade, versioned, SLA-backed. Day one, every agent that's granted HubSpot access gets pre-baked skills for contacts, deals, pipelines. No agent reinvents the HubSpot API.

```toml
# simulacra.toml — IT provisions this
[integrations.hubspot]
type = "oauth2"
client_id = "HUBSPOT_CLIENT_ID"
# Skills auto-loaded: hubspot/create-contact, hubspot/search-deals, ...
```

**Path 2: User-initiated install.** Josh in accounting needs QuickBooks. The platform has a marketplace — a directory of available integration skill packages. Josh requests it. The request goes through governance approval (admin review, security check, budget allocation). On approval, the QuickBooks skill package is installed and mounted for Josh's agent. The admin didn't have to plan for this; Josh discovered the need, the platform facilitated it, governance gated it.

```
Josh: "I need to pull invoices from QuickBooks"
  → Agent checks /var/skills/ — no QuickBooks skills found
  → Agent checks /sys/marketplace/ — QuickBooks package available
  → Agent proposes: "Install quickbooks integration? Requires approval."
  → Admin approves → skills installed → Josh's agent has QuickBooks access
```

**Path 3: Organic emergence.** Nobody planned for this. Agents across the org keep talking to Linear — creating issues, syncing cycles, querying projects. Each agent figures out the API independently. The platform observes: 15 agents across 4 teams are all making similar Linear API calls. Some handle pagination better. Some have cleaner error handling. The platform extracts the pattern, assembles the best-of-breed implementation into a `linear/` skill package, and surfaces it: "Your agents are spending 340 tokens/task figuring out the Linear API. We've extracted a skill package that reduces this to zero. Approve?"

```
Week 1:  Agent-A writes inline JS to create Linear issues (works, messy)
Week 3:  Agent-B writes a slightly better version (handles rate limits)
Week 6:  Agent-C figures out bulk operations
Week 8:  Platform observes the pattern across 15 agents
         → Extracts linear/create-issue skill (best of A, B, C)
         → Proposes to admin
Week 9:  Admin approves → all agents use the extracted skill
Week 12: Agent-D improves pagination → proposes change → skill v2
         → All agents get the upgrade
```

### Tiers are a pipeline, not categories

The three tiers are **maturity stages of the same artifact**, not separate buckets. A skill moves upward through the pipeline as it proves itself:

```
Inline code (an agent writes JS to solve a problem)
  │
  ▼
Team tier (the code stabilizes, gets a name, lives in /var/skills/)
  │  ← governance review, usage metrics
  ▼
Org tier (platform extracts pattern across agents/teams, admin approves)
  │  ← cross-tenant pattern detection
  ▼
Marketplace tier (common across customers, Simulacra ships it)
```

| Tier | What it means | How it gets here | Trust model |
|---|---|---|---|
| **Team** | One team uses it, it works for them | Agent hardened a workflow, or team uploaded it | Team-owned, limited blast radius |
| **Org** | Multiple teams use it, platform extracted it | Auto-detected across agents, admin-approved | Governance-reviewed, org-wide |
| **Marketplace** | Common across customers, Simulacra maintains it | Cross-tenant pattern or Simulacra-authored | Versioned, tested, SLA-backed |

All three tiers live in `/var/skills/`. The agent doesn't know or care which tier a skill came from. It reads the schema, invokes through the capability layer. The governance and provenance metadata is in the skill's `PROVENANCE.md` — where it came from, how it was validated, who approved it, what version it is.

**The organic emergence path (Path 3) is just workflow hardening applied to integrations.** It's not a separate mechanism. The same observation → extraction → proposal loop that turns a repeated quarterly report into a deterministic script also turns scattered Linear API calls into a pre-baked `linear/create-issue` skill. The insight is that integration knowledge and business workflow knowledge harden through the same pipeline.

**Skills move in both directions.** A marketplace skill (HubSpot v3) gets adapted by an org agent (adds custom field mapping). The adaptation hardens into an org-tier skill. The platform notices the custom field pattern is common across customers → folds it back into the marketplace skill (HubSpot v4). The ecosystem improves from every direction — bottom-up from agent work, top-down from platform curation.

### Why this matters

Without this, every agent at every company reinvents the same API integrations. An agent at Acme figures out HubSpot pagination. An agent at Beta figures out the same thing a week later. An agent at Gamma hits the same rate limit bug a month later. Millions of LLM tokens burned rediscovering solved problems.

With this, the first agent's work becomes a platform asset. The second agent gets it for free. The third never hits the bug because v2 already fixed it. **The platform's marginal cost of integrations trends toward zero as usage grows.**

The integration fabric (credentials, OAuth, token refresh) is the plumbing layer. Skills are the knowledge layer on top. An agent with access to HubSpot gets both: the credential mount *and* the pre-baked skills that know how to use it.

### MCP compatibility

MCP remains the protocol for connecting to external tool servers — it's the standard for tool discovery and invocation, and we don't abandon it. But the VFS changes how agents *find* and *use* MCP tools:

- MCP tool definitions are NOT stuffed into the LLM context window
- Instead, the agent browses `/svc/` to discover available integrations
- Schemas are read on demand — only when the agent decides it needs that specific tool
- Invocation goes through the capability layer (which speaks MCP to the server)
- Capability gating via `paths_read`/`paths_write` controls what integrations each agent can even discover

**Context window stays clean.** Tokens spent on tool schemas go from O(all tools) to O(tools actually used). With pre-baked skills, the agent often doesn't need to read a schema at all — it just invokes a skill that already knows the API.

### The extended namespace

```
/proc/           Runtime introspection (implemented)
/svc/            Available integrations and MCP servers
/etc/            Configuration, personality, governance rules
/var/skills/     Versioned skill library (platform + org + team)
/var/memory/     Persistent agent knowledge across sessions
/var/state/      Workflow checkpoints and resumable state
/mnt/            Governed data mounts (read-heavy)
/sys/            Platform state (cluster load, tenant quotas, peer agents)
/tmp/            Shared scratch space for collaborating agents
/proc/mailbox/   Writable artifact output
```

Each mount point is capability-gated, journaled, hook-visible. One security model for everything.

### What this means for the hackathon team

Before Simulacra:
1. Build the agent over the weekend ✓
2. Monday: figure out OAuth for HubSpot (2 days)
3. Manage Slack tokens securely (1 day)
4. Find somewhere to host it (Vercel? EC2? A Raspberry Pi under someone's desk?)
5. Hardcode credentials because there's no secrets management
6. Ship it with no audit trail, no governance, no budget controls
7. CISO finds out, shuts it down
8. Agent dies. Work lost.

With Simulacra:
1. Build the agent over the weekend ✓
2. Monday: admin adds `agent_type = "onboarding-agent"`, grants `integrations = ["hubspot", "slack", "linear"]`
3. Agent is running in production. Governed. Auditable. Budgeted. Integrated.
4. Agent gets better over time (workflow hardening, memory, skills)
5. Team's onboarding flow hardens into a versioned skill
6. Other teams adopt the skill. It improves. The org compounds knowledge.
7. CISO reviews the audit trail, approves expansion

**The gap isn't capability — it's the last mile.** Simulacra is the last mile.

## Architecture

```
┌──────────────────────────────────────────────┐
│  Channels                                    │
│  Slack · Teams · Web · API · Embeddable      │
├──────────────────────────────────────────────┤
│  Gateway                                     │
│  Auth · Routing · Tenancy · Rate Limits      │
├──────────────────────────────────────────────┤
│  Simulacra Engine                                │
│  Agent Loop · Providers · Hooks · Budget     │
│  VFS Namespace:                              │
│    /proc  /mcp  /etc  /var  /mnt  /sys       │
│  Shell · JS · Python · WASM                  │
│  Journal · OTel · Payments                   │
├──────────────────────────────────────────────┤
│  Integration Fabric                          │
│  OAuth2/OIDC · API Keys · Token Refresh      │
│  HubSpot · Slack · Linear · Salesforce · ... │
│  Credential Vault · Access Audit · Rotation  │
├──────────────────────────────────────────────┤
│  Control Plane                               │
│  Agent HR · Security & Compliance · Mgmt     │
│  Workflow Hardening · Cost Attribution        │
│  Pattern Detection · Skill Library           │
│  Admin Dashboard · Audit · Policy            │
└──────────────────────────────────────────────┘
```

**Single Rust binary.** No containers required. No service mesh. `cargo install simulacra` and you're running.

**Multi-model.** Claude, Llama, Qwen, GPT-OSS, any OpenAI-compatible endpoint. The enterprise picks the model; the platform governs it identically.

**Deployable anywhere.** SaaS, managed BYOC, or self-hosted. Same binary, pluggable backends for auth, storage, and telemetry.

## Risk Model

Three-part system on agent activity, all implemented through governance hooks:

- **DLP (Data Loss Prevention)** — hooks scan tool outputs for PII, credentials, sensitive data before it reaches the LLM or leaves the system
- **Behavioral monitoring** — detect anomalous patterns across agent activity (reconnaissance, boundary probing, unusual tool call sequences)
- **Risk/loss modeling** — quantify exposure from agent actions, flag high-risk operations for human review

## Execution Isolation

Four levels — a deployment choice, not an architecture choice:

1. **Semaphore + spawn_blocking** — lightweight, good for single-tenant
2. **Fork worker processes** — process-level isolation, crash containment
3. **WASM sandbox** — memory isolation, fuel metering, capability-based I/O
4. **MicroVMs** — hardware-level isolation for multi-tenant SaaS

The agent never knows what level it's running at. Same interface, different backend. Enterprises pick the isolation matching their threat model.

## The Competitive Position

The agent capability market is exploding. OpenClaw, Claude Code, Cowork, and a dozen others let individuals use AI agents. None of them solve the enterprise problem:

| | OpenClaw / Claude Code | Simulacra |
|---|---|---|
| **Users** | Developers | Everyone (Slack/Teams) |
| **Deployment** | Developer laptop | Enterprise infrastructure, any cloud |
| **Integrations** | Each team DIY | Managed fabric, admin-configured |
| **Credentials** | Env vars, hardcoded | Centralized vault, auto-rotation |
| **Governance** | None (trust the model) | Runtime-enforced (hooks, capabilities, budgets) |
| **Audit** | Maybe logs | Full journal — every operation, provable |
| **PII protection** | None | Hooks scan before LLM sees data |
| **Cost control** | None | Per-agent, per-team, per-project budgets |
| **Payments** | N/A | 402 flow, virtual cards, governance-gated |
| **Workflow evolution** | Every run is LLM-dependent | LLM → skill → deterministic script |
| **After the hackathon** | Dies on a laptop | Running in production Monday morning |

Simulacra is not competing with agent frameworks. It's competing with the spreadsheet of reasons a CISO gives for saying "no" to AI agents — **and** the graveyard of hackathon projects that had no place to live.

## The MVP target: virtual coworkers

The hardening target for the entire platform is the **virtual coworker loop.** Everything else is infrastructure in service of this.

A virtual coworker is an agent with:
- A **persona** (name, role, tone, expertise — Atlas the Finance analyst, Sol the Customer Success rep, Nova the Ops generalist)
- **Triggers** — cron, webhook, Slack @mention, email, file drop
- **Integrations** — credentials to the systems its role needs (HubSpot, Linear, Slack, S3, internal APIs)
- **Memory** — long-term knowledge that compounds across runs
- **Accruing skills** — workflows that start as LLM reasoning and harden into deterministic code

Simulacra reaches MVP when:
1. Multiple coworkers with distinct roles and personalities run on the same platform
2. Each coworker **accrues knowledge** — what to do, what not to do, who to ask, which API calls work
3. Coworkers **share knowledge** through entity-keyed memory (Sol's notes on a customer are visible to Nova when Nova is asked about that customer)
4. The platform **observes patterns** across coworkers and surfaces common workflows as proposed shared skills
5. The **compounding loop** is measurable: the 100th run of a task costs a fraction of the 1st, because most reasoning is now cached in memory or hardened skills
6. A new coworker can be **spawned from a template** with zero custom code, inheriting the full infrastructure

This loop requires long-term memory, which is specified in `specs/S037-memory-and-semantic-retrieval.md`. The key design decision: **RAG and semantic memory are the same subsystem** — top-K semantic retrieval over tenant-scoped content, with two producer paths (admin ingestion for docs, agent writes for learned knowledge) and one retrieval interface (`semantic_search` tool). Local-first embeddings, SQLite vector store, single binary — consistent with the rest of Simulacra.

## Build Order

1. **API server** — HTTP API wrapping the Simulacra engine (S031 — implemented)
2. **Integration fabric** — managed OAuth2/API key integrations, VFS-mounted (S033 — implemented)
3. **Task files & artifacts** — file in → agent → file out enterprise lifecycle (S036 — implemented)
4. **Memory + semantic retrieval** — the compounding knowledge loop (S037 — specced)
5. **Virtual coworker demo** — three coworkers exercising the MVP proof
6. **Slack bot** — first user-facing surface (where employees already are)
7. **Admin dashboard** — agent types, hooks, audit trails, budgets, integrations
8. **Workflow hardening** — curator agent, skill extraction, pattern detection (S038 — future)
9. **Payment integration** — virtual cards, 402 flow (S030 — specced)
10. **Event gateway** — webhook, schedule, email triggers (S032 — implemented)
11. **Embeddable widget** — drop-in chat for web apps
