# S033 — Integration Fabric

**Status:** Active
**Crates:** `simulacra-integration` (new), `simulacra-vfs` (ServiceFs layer), `simulacra-config` (config types), `simulacra-sandbox` (wiring), `simulacra-types` (shared traits)

## Dependencies

- **S001** — Virtual filesystem (VirtualFs trait, path resolution, OverlayFs)
- **S004** — Capability tokens (paths_read gates `/svc/` access)
- **S005** — Journal (integration discovery reads and credential uses are journaled)
- **S010** — Observability conventions
- **S011** — Sandbox composition (ServiceFs wiring into AgentCell)
- **S012** — Built-in tools (file_read, list_dir are the access mechanism for `/svc/`)
- **S017** — Skills (skill namespace at `/var/skills/`, discovery and invocation)
- **S026** — Governance hooks (integration access visible to hooks)
- **S029** — Agent procfs (reference VFS layer pattern)
- **S031** — API server (tenant config, auth, multi-tenancy)

## Scope

The integration fabric manages external service connections for agents: credential lifecycle, service discovery via VFS, and the skill namespace where pre-baked integration knowledge lives.

**In scope:**
- `simulacra-integration` crate: `IntegrationRegistry`, credential storage, OAuth2 token refresh, API key management
- `IntegrationConfig` types for `simulacra.toml`: `[integrations.*]` section with oauth2 and api_key variants
- Tenant-scoped integration grants: `[tenants.*.integrations]`
- `ServiceFs` VFS layer mounting at `/svc/`: read-only virtual files exposing integration metadata
- `/svc/` virtual file tree: README.md, config.json, skills/ listing per integration
- Credential injection into capability layer (JS/Python `fetch()` gets auth headers)
- OAuth2 token lifecycle: automatic background refresh, expiry tracking
- Integration connectivity test on startup
- `IntegrationLister` narrow trait (like `ToolLister`) for VFS layer decoupling
- Skill namespace at `/var/skills/` with three tiers (marketplace, org, team)
- Skill structure: `schema.json`, implementation file, `PROVENANCE.md`
- `CredentialInjector` trait for capability layer auth injection

**Out of scope:**
- Skill marketplace browsing / remote skill download (future spec)
- Skill extraction from agent journals (workflow hardening — future spec)
- Cross-tenant pattern detection (future spec)
- Dynamic integration configuration via API (config-defined only in S033)
- OAuth2 authorization code flow (S033 handles token refresh for pre-provisioned tokens only)
- OIDC identity federation for integration auth
- Integration-specific rate limit enforcement (skills handle this; fabric exposes config)
- MCP server wrapping of integrations (integrations are VFS-discoverable, not MCP-discoverable)
- Virtual card / payment integration for API costs (see S030)

## Context

The integration fabric solves the "last mile" problem. Every enterprise agent needs access to external services (HubSpot, Slack, Linear, Salesforce). Without a managed fabric, each team independently handles OAuth flows, manages API keys, handles token refresh, and deploys somewhere that can reach internal services.

The fabric has three layers that compose:

1. **Credential management (plumbing).** `simulacra-integration` crate. Credentials are configured once by an admin in `simulacra.toml`, stored as environment variable references (never plaintext), and refreshed automatically. The agent and LLM never see credentials.

2. **Service discovery via VFS (`/svc/` mount).** `ServiceFs` in `simulacra-vfs`. Agents browse `/svc/` to discover what integrations are available, read metadata on demand, and understand what each integration provides. This follows the ProcFs pattern exactly: a read-only VFS layer that intercepts paths and generates content dynamically.

3. **Skill namespace (`/var/skills/`).** Pre-baked skills that know how to use integrations. An agent doesn't need to learn the HubSpot API — it reads `/var/skills/hubspot/create-contact/schema.json` and invokes the skill. Skills handle auth injection, pagination, rate limits, and error handling.

The two-layer architecture applies directly:
- The **agent layer** (LLM) discovers through VFS reads: browses `/svc/`, reads skill schemas.
- The **capability layer** (JS/Python) does the actual work: invokes skills that call `fetch()` with credentials injected by the fabric.

The separation ensures credentials are never exposed to the LLM. The fabric injects an `Authorization` header into outbound `fetch()` calls made by skill code. The LLM sees only metadata (base URL, scopes, available skills).

## Design

### Credential types

```rust
/// Authentication method for an integration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AuthMethod {
    #[serde(rename = "oauth2")]
    OAuth2 {
        client_id: String,
        client_secret: String,
        token_url: String,
        scopes: Vec<String>,
        refresh_token: Option<String>,
    },
    #[serde(rename = "api_key")]
    ApiKey {
        key: String,
        #[serde(default = "default_key_placement")]
        placement: String,
    },
}

/// Configuration for a single integration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationConfig {
    #[serde(flatten)]
    pub auth: AuthMethod,
    pub base_url: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub rate_limit_rps: u32,
    #[serde(default)]
    pub skills_path: Option<String>,
}
```

All string values in `AuthMethod` are environment variable **names**, not secrets. Actual values are resolved at runtime.

### IntegrationRegistry

```rust
pub struct IntegrationRegistry {
    integrations: HashMap<String, Arc<IntegrationCredential>>,
    refresh_handles: Vec<tokio::task::JoinHandle<()>>,
}

impl IntegrationRegistry {
    pub fn from_config(integrations: &HashMap<String, IntegrationConfig>) -> Result<Self, IntegrationError>;
    pub async fn test_connectivity(&self) -> HashMap<String, Result<(), IntegrationError>>;
    pub async fn access_token(&self, name: &str) -> Result<String, IntegrationError>;
    pub fn names(&self) -> Vec<String>;
    pub fn metadata(&self, name: &str) -> Option<IntegrationMetadata>;
    pub async fn shutdown(&self);
}
```

### IntegrationLister trait (VFS decoupling)

```rust
pub trait IntegrationLister: Send + Sync + 'static {
    fn integration_names(&self) -> Vec<String>;
    fn integration_metadata(&self, name: &str) -> Option<String>;
    fn integration_readme(&self, name: &str) -> Option<String>;
    fn integration_skill_names(&self, name: &str) -> Vec<String>;
}
```

### ServiceFs VFS layer

```rust
pub struct ServiceFs<V: VirtualFs> {
    inner: V,
    integrations: Arc<dyn IntegrationLister>,
}
```

Path routing:
- `/svc/**` — intercepted by ServiceFs handlers (read-only)
- Everything else — delegated to inner VFS unchanged

### Virtual file tree

```
/svc/
  hubspot/
    README.md           → description, capabilities, available skills
    config.json         → {"base_url": "...", "scopes": [...], "rate_limit_rps": 10, "status": "ok"}
    skills/             → list of available skill names
  slack/
    README.md
    config.json
    skills/
  linear/
    README.md
    config.json
    skills/
```

### Skill namespace (`/var/skills/`)

Skills are real files mounted via VFS host mounts. Each skill is a directory:

```
/var/skills/
  hubspot/
    create-contact/
      schema.json         → input/output schema
      skill.js            → implementation
      PROVENANCE.md       → tier, version, origin, approval
    search-deals/
      schema.json
      skill.py
      PROVENANCE.md
  quarterly-reconciliation/
    schema.json
    skill.py
    PROVENANCE.md
```

PROVENANCE.md frontmatter:

```yaml
---
tier: marketplace | org | team
version: "1.2.0"
origin: "platform" | "extracted" | "authored"
author: "simulacra-platform" | "agent-xyz" | "team-accounting"
approved_by: "admin@acme.com"
approved_at: "2026-03-15T10:00:00Z"
requires_integrations: ["hubspot"]
---
```

### CredentialInjector

```rust
#[async_trait]
pub trait CredentialInjector: Send + Sync + 'static {
    async fn inject_credentials(
        &self,
        url: &str,
        tenant_integrations: &[String],
    ) -> Result<Option<Vec<(String, String)>>, IntegrationError>;
}
```

Wired into `simulacra-fetch`. When JS/Python calls `fetch("https://api.hubapi.com/...")`:
1. Match URL against configured integration base URLs
2. Check tenant has access to the matched integration
3. Get current access token from `IntegrationRegistry`
4. Add `Authorization: Bearer <token>` header
5. Journal the injection (integration name, not token value)

### Config additions

```toml
[integrations.hubspot]
type = "oauth2"
client_id = "HUBSPOT_CLIENT_ID"
client_secret = "HUBSPOT_CLIENT_SECRET"
token_url = "https://api.hubapi.com/oauth/v1/token"
scopes = ["crm.objects.contacts.read", "crm.objects.deals.read"]
base_url = "https://api.hubapi.com"
description = "HubSpot CRM — contacts, deals, pipelines"
rate_limit_rps = 10

[integrations.linear]
type = "api_key"
key = "LINEAR_API_KEY"
base_url = "https://api.linear.app/graphql"
description = "Linear project tracking"

[tenants.onboarding]
agent_type = "onboarding-agent"
integrations = ["hubspot", "slack", "linear"]
```

### VFS composition

```
  OverlayFs (upper: scratch, lower: host mounts including /var/skills/)
       │
       ▼
  ServiceFs (intercepts /svc/**, delegates rest to inner)
       │
       ▼
  ProcFs (intercepts /proc/**, delegates rest to inner)
       │
       ▼
  AgentCell sees unified VFS: /proc + /svc + /var/skills/ + workspace
```

### OAuth2 token refresh lifecycle

1. At startup, resolve env vars and perform initial token exchange.
2. Background `tokio::spawn` task sleeps until 5 minutes before expiry, then refreshes.
3. Failed refresh: retry with exponential backoff (1s, 2s, 4s, 8s, max 60s).
4. After 3 consecutive failures: log error, mark integration degraded.
5. `access_token()` on degraded integration returns `IntegrationError::TokenRefreshFailed`.
6. Successful refresh after degradation clears the degraded state.
7. `shutdown()` cancels all background refresh tasks.

## Behavior

### Integration configuration

1. `SimulacraConfig` deserializes `[integrations.*]` sections into `HashMap<String, IntegrationConfig>`.
2. Each integration config must have a `type` field (`"oauth2"` or `"api_key"`).
3. Config values like `client_id = "HUBSPOT_CLIENT_ID"` are env var names, not secrets.
4. Missing `type` field is a parse error at startup.
5. Missing `base_url` is a parse error at startup.

### Credential resolution

6. `IntegrationRegistry::from_config()` reads env vars for all credential fields.
7. If a required env var is not set, returns `IntegrationError::MissingEnvVar`.
8. API key integrations resolve the `key` env var once at startup.
9. OAuth2 integrations resolve `client_id`, `client_secret`, and optionally `refresh_token` at startup.
10. Resolved credential values are held in memory only. Never written to disk, VFS, or logs.

### OAuth2 token lifecycle

11. OAuth2 integrations with a refresh token perform initial token exchange at startup.
12. Background task refreshes token 5 minutes before expiry.
13. Failed refresh retries with exponential backoff (1s, 2s, 4s, 8s, max 60s).
14. After 3 consecutive failures, integration is marked degraded.
15. `access_token()` on degraded integration returns `TokenRefreshFailed`.
16. Successful refresh after degradation clears degraded state.
17. `shutdown()` cancels all background refresh tasks.

### Connectivity testing

18. `test_connectivity()` performs a lightweight probe for each integration.
19. Failed connectivity is logged as warning, does not prevent startup.
20. Connectivity status is reflected in `/svc/<name>/config.json` as `"status"` field.

### Service discovery VFS (`/svc/`)

21. `list_dir("/svc/")` returns integration names available to the current tenant, sorted.
22. `read("/svc/<name>/README.md")` returns generated markdown describing the integration.
23. `read("/svc/<name>/config.json")` returns JSON with base_url, scopes, rate_limit_rps, status. Never credentials.
24. `list_dir("/svc/<name>/skills/")` returns available skill names for this integration.
25. `read("/svc/<name>/config.json")` never contains credentials, tokens, or env var names.
26. `list_dir("/svc/<nonexistent>/")` returns `VfsError::NotFound`.
27. `read("/svc/<name>/nonexistent")` returns `VfsError::NotFound`.

### Read-only enforcement

28. `write("/svc/<name>/README.md", data)` returns `VfsError::PermissionDenied`.
29. `mkdir("/svc/custom")` returns `VfsError::PermissionDenied`.
30. `remove("/svc/<name>/config.json")` returns `VfsError::PermissionDenied`.
31. All write, remove, and mkdir on `/svc/**` return `VfsError::PermissionDenied`.

### Capability gating

32. `/svc/` access gated by `paths_read` on capability token.
33. Agent with `paths_read = ["/workspace/**"]` cannot read `/svc/`.
34. Agent with `paths_read = ["/svc/hubspot/**"]` can read HubSpot but not Slack.
35. Agent with `paths_read = ["/**"]` can read all `/svc/` paths.
36. Capability checks happen at AgentCell proxy layer, before ServiceFs.

### Tenant-scoped integration access

37. `[tenants.*.integrations]` lists which integrations a tenant's agents can access.
38. `list_dir("/svc/")` returns only integrations granted to the current tenant.
39. `read("/svc/<name>/...")` for unganted integration returns `VfsError::NotFound`.
40. Tenant with `integrations = []` sees empty `/svc/`.
41. Tenant with no `integrations` field has access to no integrations (deny by default).

### Credential injection

42. `fetch()` to a URL matching integration `base_url` gets auth headers injected.
43. API key with `placement = "header"`: adds `Authorization: Bearer <key>`.
44. API key with `placement = "header:X-Api-Key"`: adds `X-Api-Key: <key>`.
45. OAuth2: adds `Authorization: Bearer <access_token>`.
46. URL not matching any integration: no injection, request proceeds normally.
47. Credential injection is journaled (integration name, not token value).
48. Injection respects tenant scope: unganted integration URLs are not injected.

### Skill namespace (`/var/skills/`)

49. Skills mounted from host filesystem via VFS host mounts.
50. `list_dir("/var/skills/")` returns skill directories, sorted.
51. `read("/var/skills/<integration>/<skill>/schema.json")` returns input/output schema.
52. `read("/var/skills/<integration>/<skill>/skill.js")` returns implementation.
53. `read("/var/skills/<integration>/<skill>/PROVENANCE.md")` returns tier, version, origin.
54. Skills from all three tiers coexist in the same namespace.
55. Agent discovers skills by browsing; doesn't need to know the tier.
56. Skill access gated by `paths_read` capability.

### Journaling

57. Every `/svc/` read produces a journal entry.
58. Credential injection produces a journal entry with integration name.
59. Denied `/svc/` reads produce a journal entry recording the denial.
60. `/var/skills/` reads produce journal entries through normal VFS journaling.

### Metadata and existence

61. `list_dir("/svc/")` returns integration names as directory entries.
62. `list_dir("/svc/<name>/")` returns `["README.md", "config.json", "skills"]`.
63. `exists("/svc/<name>/README.md")` returns true for a configured integration.
64. `exists("/svc/nonexistent")` returns false.
65. `metadata("/svc/")` returns directory metadata.
66. `metadata("/svc/<name>/")` returns directory metadata.
67. `metadata("/svc/<name>/README.md")` returns file metadata with correct size.

## Assertions

### Integration configuration
- [x] `SimulacraConfig` deserializes `[integrations.hubspot]` with `type = "oauth2"`.
- [x] `SimulacraConfig` deserializes `[integrations.linear]` with `type = "api_key"`.
- [x] Missing `type` field is a parse error.
- [x] Missing `base_url` is a parse error.
- [x] `[tenants.onboarding]` with `integrations = ["hubspot", "slack"]` deserializes correctly.

### Credential resolution
- [x] `from_config()` succeeds when all env vars are set.
- [x] `from_config()` returns `MissingEnvVar` when a required env var is unset.
- [x] Resolved credentials are not accessible via VFS, logs, or any agent-visible path.
- [x] API key value is resolved from the named env var, not the literal config string.

### OAuth2 token lifecycle
- [x] Initial token exchange at startup.
- [x] Background refresh runs before token expiry.
- [x] Failed refresh retries with exponential backoff.
- [x] After 3 consecutive failures, integration is degraded.
- [x] `access_token()` on degraded integration returns `TokenRefreshFailed`.
- [x] Successful refresh clears degraded state.
- [x] `shutdown()` cancels refresh tasks.

### Connectivity testing
- [x] `test_connectivity()` probes each integration.
- [x] Failed connectivity logged as warning, does not prevent startup.
- [x] Status reflected in `/svc/<name>/config.json`.

### Service discovery VFS
- [x] `list_dir("/svc/")` returns sorted integration names for current tenant.
- [x] `read("/svc/hubspot/README.md")` returns markdown description.
- [x] `read("/svc/hubspot/config.json")` returns JSON with base_url, scopes, status.
- [x] `config.json` never contains credentials.
- [x] `list_dir("/svc/hubspot/skills/")` returns skill names.
- [x] `read("/svc/nonexistent/README.md")` returns `NotFound`.

### Read-only enforcement
- [x] `write("/svc/hubspot/README.md", data)` returns `PermissionDenied`.
- [x] `mkdir("/svc/custom")` returns `PermissionDenied`.
- [x] `remove("/svc/hubspot/config.json")` returns `PermissionDenied`.

### Capability gating
- [x] Agent with `paths_read = ["/workspace/**"]` gets error on `/svc/` reads.
- [x] Agent with `paths_read = ["/svc/hubspot/**"]` can read HubSpot, not Slack.
- [x] Agent with `paths_read = ["/**"]` can read all `/svc/`.
- [x] Capability check before ServiceFs dispatch.

### Tenant-scoped access
- [x] Tenant with `integrations = ["hubspot"]` sees only hubspot in `list_dir("/svc/")`.
- [x] Tenant without hubspot gets `NotFound` for `/svc/hubspot/README.md`.
- [x] Tenant with `integrations = []` sees empty `/svc/`.
- [x] Tenant with no `integrations` field sees empty `/svc/`.

### Credential injection
- [x] `fetch()` to hubspot URL from granted tenant gets auth header.
- [x] `fetch()` to hubspot URL from unganted tenant gets no injection.
- [x] `fetch()` to unrelated URL gets no injection.
- [x] Injection is journaled with integration name, not token.
- [x] OAuth2 injection uses current (possibly refreshed) access token.
- [x] API key injection uses configured placement.

### Skill namespace
- [x] `list_dir("/var/skills/")` returns skill directories.
- [x] `read` of schema.json returns input/output schema.
- [x] `read` of PROVENANCE.md returns tier and version.
- [x] Skills from all three tiers visible in same namespace.
- [x] Skill access gated by `paths_read`.

### Journaling
- [x] `/svc/` read produces journal entry.
- [x] Credential injection produces journal entry.
- [x] Denied `/svc/` read produces journal entry.

### Metadata and existence
- [x] `exists("/svc/hubspot")` true when configured and tenant-granted.
- [x] `exists("/svc/nonexistent")` false.
- [x] `metadata("/svc/")` returns directory metadata.
- [x] `metadata("/svc/hubspot/README.md")` returns file metadata with correct size.

## Observability (see S010)

- [x] `simulacra_svcfs_read` span per read with `simulacra.svcfs.path` and `simulacra.svcfs.integration`.
- [x] `simulacra_svcfs_list_dir` span per list_dir with `simulacra.svcfs.path`.
- [x] `simulacra.svcfs.reads` counter per read with `integration` label.
- [x] `simulacra_integration_token_refresh` span per OAuth2 refresh with `simulacra.integration.name` and `simulacra.integration.result`.
- [x] `simulacra.integration.refresh_failures` counter with `integration` label.
- [x] `simulacra.integration.credential_injections` counter with `integration` label.
- [x] `simulacra.integration.active` gauge tracking healthy integrations.
- [x] `tracing::info!` on registry startup with integration count and names.
- [x] `tracing::info!` on successful token refresh.
- [x] `tracing::warn!` on token refresh failure with attempt count.
- [x] `tracing::warn!` on write attempt to read-only `/svc/`.
- [x] `tracing::warn!` on capability-denied `/svc/` access.
- [x] `tracing::error!` on integration marked degraded.
- [x] `tracing::debug!` on credential injection with integration name and URL host.
