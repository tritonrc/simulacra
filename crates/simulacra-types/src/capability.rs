use serde::{Deserialize, Serialize};

use crate::memory::MemoryCapability;

/// URL pattern with wildcard support for network permissions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkPermission(pub String);

/// Glob-style path pattern for filesystem permissions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathPattern(pub String);

/// Capability token assigned to an agent at creation.
/// Checked at the proxy layer before any side-effecting operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CapabilityToken {
    pub network: Vec<NetworkPermission>,
    pub mcp_tools: Vec<String>,
    pub shell: bool,
    pub javascript: bool,
    pub python: bool,
    pub paths_write: Vec<PathPattern>,
    pub paths_read: Vec<PathPattern>,
    pub spawn_types: Vec<String>,
    /// Skill capability patterns using the `skill:<name>` namespace and glob
    /// semantics. An agent's effective skill catalog is the intersection of:
    ///
    /// - the agent type's configured `skills` list;
    /// - the discovered skill registry;
    /// - the capability token's allowed `skill:<name>` patterns.
    ///
    /// A skill never grants capabilities the agent does not already have.
    /// The `CapabilityToken` and `ResourceBudget` always constrain skill behaviour.
    /// Skills are attenuated like other capabilities: a child agent or narrowed
    /// token may expose a subset of the parent's skills but never a superset.
    pub skill_patterns: Vec<String>,
    /// Memory capability — opt-in long-term memory access. Defaults to disabled.
    /// Memory paths (`/var/memory/**`, `/mnt/**`) are NOT gated by `paths_read`/
    /// `paths_write`; they are gated exclusively by `memory.search_scopes` and
    /// `memory.write_scopes`. The permissive engine fallback for untyped agents
    /// MUST keep memory disabled. See S037 §11, §14.
    #[serde(default)]
    pub memory: MemoryCapability,
}

impl CapabilityToken {
    /// Check if this token is a valid subset of `parent`.
    /// Used for capability attenuation on sub-agent spawn.
    pub fn is_subset_of(&self, parent: &CapabilityToken) -> bool {
        // Boolean capabilities: child cannot have what parent lacks
        if self.shell && !parent.shell {
            return false;
        }
        if self.javascript && !parent.javascript {
            return false;
        }
        if self.python && !parent.python {
            return false;
        }
        // Network: each child entry must be covered by a parent entry.
        for child_net in &self.network {
            let child_pat = child_net.0.strip_prefix("net:").unwrap_or(&child_net.0);
            let covered = parent.network.iter().any(|parent_net| {
                let parent_pat = parent_net.0.strip_prefix("net:").unwrap_or(&parent_net.0);
                if parent_pat == "*" {
                    return true;
                }
                if parent_pat == child_pat {
                    return true;
                }
                // Parent wildcard covers child exact host (e.g. *.github.com covers api.github.com)
                // Wildcard matches a single subdomain level only.
                if parent_pat.starts_with("*.") {
                    let dot_suffix = &parent_pat[1..]; // ".github.com"
                    if let Some(prefix) = child_pat.strip_suffix(dot_suffix)
                        && !prefix.contains('.')
                        && !prefix.starts_with('*')
                    {
                        return true;
                    }
                }
                false
            });
            if !covered {
                return false;
            }
        }

        // Spawn types: child must be a subset of parent
        for child_spawn in &self.spawn_types {
            if !parent.spawn_types.contains(child_spawn) {
                return false;
            }
        }

        // MCP tools: each child entry must exist in parent
        for child_tool in &self.mcp_tools {
            if !parent.mcp_tools.contains(child_tool) {
                return false;
            }
        }

        // Write paths: each child entry must be covered by a parent entry (glob-aware)
        for child_path in &self.paths_write {
            if !path_pattern_covered_by(&child_path.0, &parent.paths_write) {
                return false;
            }
        }

        // Read paths: each child entry must be covered by a parent entry (glob-aware)
        for child_path in &self.paths_read {
            if !path_pattern_covered_by(&child_path.0, &parent.paths_read) {
                return false;
            }
        }

        // Skill patterns: child agent skill patterns must be covered by parent.
        // A child or narrowed token may expose a subset of the parent's skills
        // but never a superset.
        // Empty skill_patterns means "allow all skills" (see check_skill),
        // so a child with empty patterns is only a subset if the parent also
        // has empty patterns (i.e. the parent also allows all skills).
        if self.skill_patterns.is_empty() && !parent.skill_patterns.is_empty() {
            return false;
        }
        for child_skill in &self.skill_patterns {
            let covered = parent.skill_patterns.iter().any(|parent_pat| {
                if parent_pat == child_skill {
                    return true;
                }
                // Glob: parent `skill:rust-*` covers child `skill:rust-dev`
                if let Some(prefix) = parent_pat.strip_suffix('*') {
                    return child_skill.starts_with(prefix);
                }
                false
            });
            if !covered {
                return false;
            }
        }

        // Memory: enabled cannot expand. Each child scope must be at or
        // under SOME parent scope (prefix-aware narrowing — children may
        // be narrower than parents but never wider).
        if self.memory.enabled && !parent.memory.enabled {
            return false;
        }
        for child_scope in &self.memory.search_scopes {
            let covered = parent
                .memory
                .search_scopes
                .iter()
                .any(|parent_scope| child_scope.starts_with_prefix(parent_scope));
            if !covered {
                return false;
            }
        }
        for child_scope in &self.memory.write_scopes {
            let covered = parent
                .memory
                .write_scopes
                .iter()
                .any(|parent_scope| child_scope.starts_with_prefix(parent_scope));
            if !covered {
                return false;
            }
        }

        true
    }

    /// Compute the intersection of two capability tokens.
    ///
    /// For booleans, use AND. For lists, use set intersection (items present
    /// in both). The result is the most restrictive combination of both tokens.
    pub fn intersect(&self, other: &CapabilityToken) -> CapabilityToken {
        CapabilityToken {
            network: intersect_network_perms(&self.network, &other.network),
            mcp_tools: self
                .mcp_tools
                .iter()
                .filter(|t| other.mcp_tools.contains(t))
                .cloned()
                .collect(),
            shell: self.shell && other.shell,
            javascript: self.javascript && other.javascript,
            python: self.python && other.python,
            paths_write: intersect_path_patterns(&self.paths_write, &other.paths_write),
            paths_read: intersect_path_patterns(&self.paths_read, &other.paths_read),
            spawn_types: self
                .spawn_types
                .iter()
                .filter(|s| other.spawn_types.contains(s))
                .cloned()
                .collect(),
            // skill_patterns uses "empty = allow all" semantics (unlike other
            // list fields where empty = deny all). Handle this explicitly:
            //   [] ∩ []       = []       (both allow all → allow all)
            //   [] ∩ ["a"]    = ["a"]    (allow all ∩ restricted → restricted)
            //   ["a"] ∩ []    = ["a"]    (restricted ∩ allow all → restricted)
            //   ["a"] ∩ ["b"] = filtered (standard set intersection)
            skill_patterns: if self.skill_patterns.is_empty() {
                other.skill_patterns.clone()
            } else if other.skill_patterns.is_empty() {
                self.skill_patterns.clone()
            } else {
                self.skill_patterns
                    .iter()
                    .filter(|s| other.skill_patterns.contains(s))
                    .cloned()
                    .collect()
            },
            // Memory capability intersection: both must be enabled, scopes
            // are intersected with **prefix-aware narrowing**, not exact
            // vector intersection. A child scope is allowed if it is at or
            // under SOME parent scope. Equivalently: the result is the set
            // of (child ∩ parent) prefixes — children narrowing parents are
            // honored, children expanding beyond parents are dropped.
            //
            // Example:
            //   parent.search_scopes = [/var/memory/self]
            //   child.search_scopes  = [/var/memory/self/notes, /var/memory/users]
            //   intersect            = [/var/memory/self/notes]
            // The /var/memory/users entry is dropped because no parent scope
            // covers it; /var/memory/self/notes is kept because it's under
            // /var/memory/self.
            memory: MemoryCapability {
                enabled: self.memory.enabled && other.memory.enabled,
                search_scopes: intersect_memory_scopes(
                    &self.memory.search_scopes,
                    &other.memory.search_scopes,
                ),
                write_scopes: intersect_memory_scopes(
                    &self.memory.write_scopes,
                    &other.memory.write_scopes,
                ),
            },
        }
    }

    /// Check if a shell command is allowed.
    pub fn check_shell(&self) -> Result<(), CapabilityDenied> {
        if self.shell {
            Ok(())
        } else {
            Err(CapabilityDenied {
                operation: "shell".into(),
                reason: "shell capability not granted".into(),
            })
        }
    }

    /// Check if reading the given path is allowed.
    ///
    /// **Memory paths** (`/var/memory/**` and `/mnt/**`) are gated by
    /// [`MemoryCapability::can_read`] **instead of** the generic
    /// `paths_read` glob, per S037 §11/§14. If the path is a memory path,
    /// this check defers entirely to the memory capability — the generic
    /// `paths_read` glob is not consulted. This means an agent with
    /// `paths_read = "/**"` can NOT read memory paths unless
    /// `MemoryCapability.search_scopes` grants the prefix.
    ///
    /// The path is normalized before checking to prevent traversal attacks
    /// (e.g. `/workspace/../etc/secret` resolves to `/etc/secret`).
    pub fn check_path_read(&self, path: &str) -> Result<(), CapabilityDenied> {
        let path = &normalize_path(path);
        if is_memory_path(path) {
            return check_memory_path_read(&self.memory, path);
        }
        if path_matches(&self.paths_read, path) {
            Ok(())
        } else {
            Err(CapabilityDenied {
                operation: "read_file".into(),
                reason: format!("read access denied for {path}"),
            })
        }
    }

    /// Check if writing the given path is allowed.
    ///
    /// **Memory paths** (`/var/memory/**` and `/mnt/**`) are gated by
    /// [`MemoryCapability::can_write`] **instead of** the generic
    /// `paths_write` glob, per S037 §11/§14. See `check_path_read` for the
    /// rationale.
    ///
    /// Note: `/mnt/**` writes are always denied for agents regardless of
    /// `MemoryCapability` — only the admin ingestion API writes there.
    /// This is enforced at the `MemoryStoreFs` layer too, but rejecting
    /// here gives a clearer error at the sandbox boundary.
    ///
    /// The path is normalized before checking to prevent traversal attacks.
    pub fn check_path_write(&self, path: &str) -> Result<(), CapabilityDenied> {
        let path = &normalize_path(path);
        if is_memory_path(path) {
            return check_memory_path_write(&self.memory, path);
        }
        if path_matches(&self.paths_write, path) {
            Ok(())
        } else {
            Err(CapabilityDenied {
                operation: "write_file".into(),
                reason: format!("write access denied for {path}"),
            })
        }
    }

    /// Check if JavaScript execution is allowed.
    pub fn check_javascript(&self) -> Result<(), CapabilityDenied> {
        if self.javascript {
            Ok(())
        } else {
            Err(CapabilityDenied {
                operation: "javascript".into(),
                reason: "javascript capability not granted".into(),
            })
        }
    }

    /// Check if Python execution is allowed.
    pub fn check_python(&self) -> Result<(), CapabilityDenied> {
        if self.python {
            Ok(())
        } else {
            Err(CapabilityDenied {
                operation: "python".into(),
                reason: "python capability not granted".into(),
            })
        }
    }

    /// Check if a `skill:<name>` is allowed by the capability token.
    ///
    /// Capability checks happen at the call site: before returning a skill body,
    /// Simulacra verifies that the requested skill is allowed. If a skill is
    /// capability denied, the denial reason is preserved in the error.
    pub fn check_skill(&self, skill_name: &str) -> Result<(), CapabilityDenied> {
        // Empty patterns list = all skills allowed (no restrictions)
        if self.skill_patterns.is_empty() {
            return Ok(());
        }
        let target = format!("skill:{skill_name}");
        for pattern in &self.skill_patterns {
            if pattern == "*" || pattern == "skill:*" {
                return Ok(());
            }
            if pattern == &target {
                return Ok(());
            }
            // Glob: `skill:code-*` matches `skill:code-review`
            if let Some(prefix) = pattern.strip_suffix('*')
                && target.starts_with(prefix)
            {
                return Ok(());
            }
        }
        Err(CapabilityDenied {
            operation: format!("skill:{skill_name}"),
            reason: format!("skill {skill_name:?} not allowed by capability token"),
        })
    }

    /// Check if a network request to the given host is allowed.
    pub fn check_network(&self, host: &str) -> Result<(), CapabilityDenied> {
        for perm in &self.network {
            let pattern = &perm.0;
            // Bare "*" means allow all hosts.
            if pattern == "*" {
                return Ok(());
            }
            // Strip optional "net:" prefix for backward compatibility.
            let pat = pattern.strip_prefix("net:").unwrap_or(pattern);
            if pat == host {
                return Ok(());
            }
            // Wildcard subdomain matching requires `*.` prefix (e.g. `*.github.com`).
            // Bare `*github.com` does NOT match — it would allow `evilgithub.com`.
            if let Some(dot_suffix) = pat.strip_prefix("*.")
                && let Some(prefix) = host.strip_suffix(&format!(".{dot_suffix}"))
                && !prefix.contains('.')
            {
                return Ok(());
            }
        }
        Err(CapabilityDenied {
            operation: format!("network:{host}"),
            reason: format!("no network permission for {host}"),
        })
    }
}

/// Error returned when a capability check fails.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityDenied {
    pub operation: String,
    pub reason: String,
}

impl std::fmt::Display for CapabilityDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "capability denied: {} — {}", self.operation, self.reason)
    }
}

impl std::error::Error for CapabilityDenied {}

/// Check if a child path pattern is covered by any parent pattern using glob semantics.
///
/// A child pattern `/workspace/project/**` is covered by parent `/workspace/**`
/// because every path the child matches is also matched by the parent.
fn path_pattern_covered_by(child: &str, parents: &[PathPattern]) -> bool {
    parents.iter().any(|p| {
        let parent = &p.0;
        if parent == child {
            return true;
        }
        // Parent `/**` covers everything.
        if parent == "/**" {
            return true;
        }
        // Parent `/foo/**` covers child `/foo/bar/**` or child `/foo/bar.txt`.
        if let Some(parent_prefix) = parent.strip_suffix("/**") {
            let child_base = child.strip_suffix("/**").unwrap_or(child);
            return child_base.starts_with(parent_prefix)
                && (child_base.len() == parent_prefix.len()
                    || child_base.as_bytes().get(parent_prefix.len()) == Some(&b'/'));
        }
        false
    })
}

/// Intersect two sets of path patterns using glob-aware subset logic.
///
/// A pattern from `a` is kept if it is covered by some pattern in `b`, and
/// vice versa. The narrower pattern is always the one that appears in the result.
fn intersect_path_patterns(a: &[PathPattern], b: &[PathPattern]) -> Vec<PathPattern> {
    let mut out = Vec::new();
    for ap in a {
        if path_pattern_covered_by(&ap.0, b) && !out.contains(ap) {
            out.push(ap.clone());
        }
    }
    for bp in b {
        if path_pattern_covered_by(&bp.0, a) && !out.contains(bp) {
            out.push(bp.clone());
        }
    }
    out
}

/// Check if a child network permission is covered by a parent.
fn network_perm_covered_by(child: &str, parent: &str) -> bool {
    let child_pat = child.strip_prefix("net:").unwrap_or(child);
    let parent_pat = parent.strip_prefix("net:").unwrap_or(parent);
    if parent_pat == "*" {
        return true;
    }
    if parent_pat == child_pat {
        return true;
    }
    // Parent wildcard covers child exact host (e.g. *.github.com covers api.github.com)
    if parent_pat.starts_with("*.") {
        let dot_suffix = &parent_pat[1..]; // ".github.com"
        if let Some(prefix) = child_pat.strip_suffix(dot_suffix)
            && !prefix.contains('.')
            && !prefix.starts_with('*')
        {
            return true;
        }
    }
    false
}

/// Intersect two sets of network permissions using subset-aware logic.
fn intersect_network_perms(
    a: &[NetworkPermission],
    b: &[NetworkPermission],
) -> Vec<NetworkPermission> {
    let mut out = Vec::new();
    for ap in a {
        let covered = b.iter().any(|bp| network_perm_covered_by(&ap.0, &bp.0));
        if covered && !out.contains(ap) {
            out.push(ap.clone());
        }
    }
    for bp in b {
        let covered = a.iter().any(|ap| network_perm_covered_by(&bp.0, &ap.0));
        if covered && !out.contains(bp) {
            out.push(bp.clone());
        }
    }
    out
}

/// Normalize a filesystem path by resolving `.`, `..`, and duplicate `/` segments.
///
/// The normalizer:
/// - Splits on `/`
/// - Skips empty segments (duplicate slashes) and `.`
/// - Pops on `..` (but does not go above root)
/// - Rejoins with `/`
/// - Preserves the leading `/` for absolute paths
fn normalize_path(path: &str) -> String {
    let mut components: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            s => components.push(s),
        }
    }
    if path.starts_with('/') {
        format!("/{}", components.join("/"))
    } else {
        components.join("/")
    }
}

/// Check whether a path matches any of the given glob-style patterns.
///
/// Supported patterns:
/// - `/**` matches any path
/// - `/foo/**` matches any path starting with `/foo/` (or exactly `/foo`)
/// - Exact match: `/foo/bar.txt` matches `/foo/bar.txt`
/// - Empty patterns list = deny all
fn path_matches(patterns: &[PathPattern], path: &str) -> bool {
    patterns.iter().any(|p| {
        let pat = &p.0;
        if pat == "/**" {
            return true;
        }
        if let Some(prefix) = pat.strip_suffix("/**") {
            return path.starts_with(prefix)
                && (path.len() == prefix.len()
                    || path.as_bytes().get(prefix.len()) == Some(&b'/'));
        }
        pat == path
    })
}

/// Prefix-aware intersection of two memory scope lists.
///
/// A scope is included in the result if it is at or under SOME scope in
/// the other list. Result is deduplicated and order-stable on `a` first
/// then `b`.
///
/// Examples:
///   intersect([/var/memory/self], [/var/memory/self/notes]) = [/var/memory/self/notes]
///   intersect([/var/memory/self/notes], [/var/memory/self]) = [/var/memory/self/notes]
///   intersect([/var/memory/self], [/var/memory/users])      = []
///   intersect([/var/memory/self, /var/memory/users], [/var/memory/self/notes]) = [/var/memory/self/notes]
fn intersect_memory_scopes(
    a: &[crate::memory::MemoryPath],
    b: &[crate::memory::MemoryPath],
) -> Vec<crate::memory::MemoryPath> {
    let mut out = Vec::new();
    for ap in a {
        if b.iter().any(|bp| ap.starts_with_prefix(bp)) && !out.contains(ap) {
            out.push(ap.clone());
        }
    }
    for bp in b {
        if a.iter().any(|ap| bp.starts_with_prefix(ap)) && !out.contains(bp) {
            out.push(bp.clone());
        }
    }
    out
}

// ─── Memory path gating ──────────────────────────────────────────────────────
//
// Memory paths (`/var/memory/**` and `/mnt/**`) are gated by `MemoryCapability`,
// not by the generic `paths_read`/`paths_write` globs. The functions below are
// the bridge: they parse the raw path string into a `MemoryPath` and consult
// the memory capability for the read/write decision. See S037 §11/§14.

/// True if the raw path string is under `/var/memory/**` or `/mnt/**`.
/// Used by `check_path_read`/`check_path_write` to dispatch to the memory
/// capability check instead of the generic `paths_*` glob check.
///
/// Delegates to [`crate::memory::MemoryPath::is_memory_path_str`] — the
/// single source of truth shared with `MemoryStoreFs`.
fn is_memory_path(path: &str) -> bool {
    crate::memory::MemoryPath::is_memory_path_str(path)
}

fn check_memory_path_read(
    memory: &crate::memory::MemoryCapability,
    path: &str,
) -> Result<(), CapabilityDenied> {
    use crate::memory::MemoryPath;
    let parsed = MemoryPath::parse(path).map_err(|e| CapabilityDenied {
        operation: "read_file".into(),
        reason: format!("invalid memory path '{path}': {e}"),
    })?;
    if memory.can_read(&parsed) {
        Ok(())
    } else {
        Err(CapabilityDenied {
            operation: "read_file".into(),
            reason: format!(
                "memory read denied for {path}: not in any MemoryCapability.search_scopes"
            ),
        })
    }
}

fn check_memory_path_write(
    memory: &crate::memory::MemoryCapability,
    path: &str,
) -> Result<(), CapabilityDenied> {
    use crate::memory::MemoryPath;
    let parsed = MemoryPath::parse(path).map_err(|e| CapabilityDenied {
        operation: "write_file".into(),
        reason: format!("invalid memory path '{path}': {e}"),
    })?;
    // /mnt/** writes are always denied for agents — admin ingestion only.
    if parsed.is_mnt() {
        return Err(CapabilityDenied {
            operation: "write_file".into(),
            reason: format!("memory write denied for {path}: /mnt is admin-ingested only"),
        });
    }
    if memory.can_write(&parsed) {
        Ok(())
    } else {
        Err(CapabilityDenied {
            operation: "write_file".into(),
            reason: format!(
                "memory write denied for {path}: not in any MemoryCapability.write_scopes"
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_network_permission_allows_matching_host() {
        let token = CapabilityToken {
            network: vec![NetworkPermission("net:api.github.com".into())],
            ..Default::default()
        };

        assert!(token.check_network("api.github.com").is_ok());
    }

    #[test]
    fn exact_network_permission_denies_other_host_with_reason() {
        let token = CapabilityToken {
            network: vec![NetworkPermission("net:api.github.com".into())],
            ..Default::default()
        };

        let denied = token
            .check_network("api.stripe.com")
            .expect_err("unexpectedly allowed a non-granted host");

        assert_eq!(denied.operation, "network:api.stripe.com");
        assert_eq!(denied.reason, "no network permission for api.stripe.com");
    }

    #[test]
    fn wildcard_parent_network_can_attenuate_to_exact_child_host() {
        let parent = CapabilityToken {
            network: vec![NetworkPermission("net:*.github.com".into())],
            ..Default::default()
        };
        let child = CapabilityToken {
            network: vec![NetworkPermission("net:api.github.com".into())],
            ..Default::default()
        };

        assert!(
            child.is_subset_of(&parent),
            "expected child host permission to be accepted as a subset of the parent's wildcard"
        );
    }

    #[test]
    fn exact_host_parent_cannot_attenuate_to_wider_network_wildcard() {
        let parent = CapabilityToken {
            network: vec![NetworkPermission("net:api.github.com".into())],
            ..Default::default()
        };
        let child = CapabilityToken {
            network: vec![NetworkPermission("net:*.github.com".into())],
            ..Default::default()
        };

        assert!(
            !child.is_subset_of(&parent),
            "wider child network capability must be rejected during attenuation"
        );
    }

    #[test]
    fn shell_capability_cannot_expand_from_false_to_true() {
        let parent = CapabilityToken {
            shell: false,
            ..Default::default()
        };
        let child = CapabilityToken {
            shell: true,
            ..Default::default()
        };

        assert!(
            !child.is_subset_of(&parent),
            "child shell access must be rejected when parent shell access is false"
        );
    }

    #[test]
    fn child_spawn_type_must_be_granted_by_parent() {
        let parent = CapabilityToken {
            spawn_types: vec!["worker".into()],
            ..Default::default()
        };
        let child = CapabilityToken {
            spawn_types: vec!["supervisor".into()],
            ..Default::default()
        };

        assert!(
            !child.is_subset_of(&parent),
            "child spawn types must be validated as a subset of the parent's spawn permissions"
        );
    }

    #[test]
    fn wildcard_network_permission_does_not_cover_multi_level_subdomains() {
        let token = CapabilityToken {
            network: vec![NetworkPermission("net:*.example.com".into())],
            ..Default::default()
        };

        let denied = token
            .check_network("sub.sub.example.com")
            .expect_err("multi-level subdomains must not be covered by a single-level wildcard");

        assert_eq!(denied.operation, "network:sub.sub.example.com");
        assert_eq!(
            denied.reason,
            "no network permission for sub.sub.example.com"
        );
    }

    #[test]
    fn bare_wildcard_network_permission_allows_any_host() {
        let token = CapabilityToken {
            network: vec![NetworkPermission("*".into())],
            ..Default::default()
        };

        assert!(token.check_network("esm.sh").is_ok());
        assert!(token.check_network("api.github.com").is_ok());
    }

    #[test]
    fn network_permission_without_net_prefix_allows_matching_host() {
        let token = CapabilityToken {
            network: vec![NetworkPermission("esm.sh".into())],
            ..Default::default()
        };

        assert!(token.check_network("esm.sh").is_ok());
    }

    #[test]
    fn empty_child_skill_patterns_is_not_subset_of_restricted_parent() {
        let parent = CapabilityToken {
            skill_patterns: vec!["skill:rust-dev".into()],
            ..Default::default()
        };
        let child = CapabilityToken {
            // Empty = allow all, which is wider than parent's restricted set
            skill_patterns: vec![],
            ..Default::default()
        };

        assert!(
            !child.is_subset_of(&parent),
            "empty child skill_patterns (allow-all) must not be a subset of a restricted parent"
        );
    }

    #[test]
    fn empty_child_skill_patterns_is_subset_of_empty_parent() {
        let parent = CapabilityToken {
            skill_patterns: vec![],
            ..Default::default()
        };
        let child = CapabilityToken {
            skill_patterns: vec![],
            ..Default::default()
        };

        assert!(
            child.is_subset_of(&parent),
            "both empty (allow-all) should be a valid subset"
        );
    }

    #[test]
    fn intersect_allow_all_skills_with_restricted_yields_restricted() {
        let allow_all = CapabilityToken {
            skill_patterns: vec![],
            ..Default::default()
        };
        let restricted = CapabilityToken {
            skill_patterns: vec!["skill:rust-dev".into()],
            ..Default::default()
        };

        let result = allow_all.intersect(&restricted);
        assert_eq!(result.skill_patterns, vec!["skill:rust-dev".to_string()]);

        // Symmetric
        let result2 = restricted.intersect(&allow_all);
        assert_eq!(result2.skill_patterns, vec!["skill:rust-dev".to_string()]);
    }

    #[test]
    fn intersect_both_allow_all_skills_yields_allow_all() {
        let a = CapabilityToken {
            skill_patterns: vec![],
            ..Default::default()
        };
        let b = CapabilityToken {
            skill_patterns: vec![],
            ..Default::default()
        };

        let result = a.intersect(&b);
        assert!(result.skill_patterns.is_empty());
    }

    #[test]
    fn bare_wildcard_parent_network_covers_any_child_network_permission() {
        let parent = CapabilityToken {
            network: vec![NetworkPermission("*".into())],
            ..Default::default()
        };
        let child = CapabilityToken {
            network: vec![NetworkPermission("net:esm.sh".into())],
            ..Default::default()
        };

        assert!(
            child.is_subset_of(&parent),
            "expected a bare wildcard parent network permission to cover any child network permission"
        );
    }

    // ─── Path normalization (BLOCKER 1) ──────────────────────────────────────

    #[test]
    fn path_traversal_is_blocked_by_normalization() {
        let token = CapabilityToken {
            paths_read: vec![PathPattern("/workspace/**".into())],
            paths_write: vec![PathPattern("/workspace/**".into())],
            ..Default::default()
        };

        // /workspace/../etc/secret normalizes to /etc/secret, which is NOT under /workspace
        token
            .check_path_read("/workspace/../etc/secret")
            .expect_err("traversal must be blocked");
        token
            .check_path_write("/workspace/../etc/secret")
            .expect_err("traversal must be blocked");

        // /workspace/./file normalizes to /workspace/file, which IS under /workspace
        token
            .check_path_read("/workspace/./file")
            .expect("dot segment should normalize and allow");
    }

    #[test]
    fn duplicate_slashes_are_normalized() {
        let token = CapabilityToken {
            paths_read: vec![PathPattern("/workspace/**".into())],
            ..Default::default()
        };
        token
            .check_path_read("/workspace//subdir///file.txt")
            .expect("duplicate slashes should normalize");
    }

    // ─── Path attenuation with glob semantics (WARNING 1) ───────────────────

    #[test]
    fn child_glob_under_parent_glob_is_a_valid_subset() {
        let parent = CapabilityToken {
            paths_read: vec![PathPattern("/workspace/**".into())],
            paths_write: vec![PathPattern("/workspace/**".into())],
            ..Default::default()
        };
        let child = CapabilityToken {
            paths_read: vec![PathPattern("/workspace/project/**".into())],
            paths_write: vec![PathPattern("/workspace/project/**".into())],
            ..Default::default()
        };

        assert!(
            child.is_subset_of(&parent),
            "child /workspace/project/** must be a subset of parent /workspace/**"
        );
    }

    #[test]
    fn intersect_keeps_narrower_path_pattern() {
        let wide = CapabilityToken {
            paths_read: vec![PathPattern("/workspace/**".into())],
            ..Default::default()
        };
        let narrow = CapabilityToken {
            paths_read: vec![PathPattern("/workspace/project/**".into())],
            ..Default::default()
        };

        let result = wide.intersect(&narrow);
        assert_eq!(result.paths_read.len(), 1);
        assert_eq!(result.paths_read[0].0, "/workspace/project/**");
    }

    // ─── Network wildcard safety (WARNING 3) ─────────────────────────────────

    #[test]
    fn bare_star_without_dot_does_not_match_suffix() {
        let token = CapabilityToken {
            network: vec![NetworkPermission("*github.com".into())],
            ..Default::default()
        };

        token
            .check_network("evilgithub.com")
            .expect_err("*github.com must NOT match evilgithub.com");
        token
            .check_network("api.github.com")
            .expect_err("*github.com must NOT match api.github.com (missing dot)");
    }

    #[test]
    fn dotted_wildcard_still_works() {
        let token = CapabilityToken {
            network: vec![NetworkPermission("*.github.com".into())],
            ..Default::default()
        };

        token
            .check_network("api.github.com")
            .expect("*.github.com must match api.github.com");
        token
            .check_network("evilgithub.com")
            .expect_err("*.github.com must NOT match evilgithub.com");
    }

    // ─── Network intersect with wildcards (WARNING 2) ────────────────────────

    #[test]
    fn intersect_network_with_wildcard_parent_keeps_child() {
        let wide = CapabilityToken {
            network: vec![NetworkPermission("*".into())],
            ..Default::default()
        };
        let narrow = CapabilityToken {
            network: vec![NetworkPermission("net:api.github.com".into())],
            ..Default::default()
        };

        let result = wide.intersect(&narrow);
        assert_eq!(result.network.len(), 1);
        assert_eq!(result.network[0].0, "net:api.github.com");
    }

    // ─── Memory path gating (S037 §11/§14) ──────────────────────────────────

    use crate::memory::{MemoryCapability, MemoryPath};

    fn token_with_paths(read: &str, write: &str) -> CapabilityToken {
        CapabilityToken {
            paths_read: vec![PathPattern(read.into())],
            paths_write: vec![PathPattern(write.into())],
            ..Default::default()
        }
    }

    #[test]
    fn check_path_write_rejects_var_memory_when_memory_disabled_even_if_paths_write_is_global() {
        // The whole point of S037 §11: paths_write="/**" must NOT grant
        // write access to /var/memory/**. The MemoryCapability is the
        // only gate.
        let token = token_with_paths("/**", "/**");
        let err = token
            .check_path_write("/var/memory/self/note.md")
            .expect_err("memory write must be denied when memory disabled");
        assert_eq!(err.operation, "write_file");
        assert!(
            err.reason
                .contains("not in any MemoryCapability.write_scopes")
        );
    }

    #[test]
    fn check_path_read_rejects_var_memory_when_memory_disabled_even_if_paths_read_is_global() {
        let token = token_with_paths("/**", "/**");
        let err = token
            .check_path_read("/var/memory/self/note.md")
            .expect_err("memory read must be denied when memory disabled");
        assert_eq!(err.operation, "read_file");
        assert!(
            err.reason
                .contains("not in any MemoryCapability.search_scopes")
        );
    }

    #[test]
    fn check_path_write_allows_var_memory_when_memory_capability_grants_the_scope() {
        let token = CapabilityToken {
            // Note: paths_write does NOT include /var/memory/** — the
            // memory capability is enough on its own.
            paths_write: vec![PathPattern("/workspace/**".into())],
            paths_read: vec![PathPattern("/**".into())],
            memory: MemoryCapability {
                enabled: true,
                search_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
                write_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
            },
            ..Default::default()
        };

        token
            .check_path_write("/var/memory/self/note.md")
            .expect("memory write inside scope must be allowed");
        token
            .check_path_read("/var/memory/self/note.md")
            .expect("memory read inside scope must be allowed");
    }

    #[test]
    fn check_path_write_denies_var_memory_outside_write_scopes_even_when_memory_enabled() {
        let token = CapabilityToken {
            paths_write: vec![PathPattern("/**".into())],
            paths_read: vec![PathPattern("/**".into())],
            memory: MemoryCapability {
                enabled: true,
                search_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
                write_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
            },
            ..Default::default()
        };

        let err = token
            .check_path_write("/var/memory/users/brian.md")
            .expect_err("write outside write_scopes must be denied");
        assert!(
            err.reason
                .contains("not in any MemoryCapability.write_scopes")
        );
    }

    #[test]
    fn check_path_write_always_denies_mnt_for_agents_even_when_memory_enabled() {
        let token = CapabilityToken {
            paths_write: vec![PathPattern("/**".into())],
            paths_read: vec![PathPattern("/**".into())],
            memory: MemoryCapability {
                enabled: true,
                // Even granting /mnt as a write scope shouldn't help —
                // /mnt is admin-only at the agent layer.
                search_scopes: vec![MemoryPath::parse("/mnt").unwrap()],
                write_scopes: vec![MemoryPath::parse("/mnt").unwrap()],
            },
            ..Default::default()
        };

        let err = token
            .check_path_write("/mnt/policies/hr.md")
            .expect_err("/mnt writes must always be denied for agents");
        assert!(err.reason.contains("admin-ingested only"));
    }

    #[test]
    fn check_path_read_allows_mnt_when_memory_grants_search_scope() {
        let token = CapabilityToken {
            paths_read: vec![PathPattern("/workspace/**".into())], // does NOT include /mnt
            paths_write: vec![],
            memory: MemoryCapability {
                enabled: true,
                search_scopes: vec![MemoryPath::parse("/mnt").unwrap()],
                write_scopes: vec![],
            },
            ..Default::default()
        };

        token
            .check_path_read("/mnt/policies/hr.md")
            .expect("/mnt read with memory grant must be allowed");
    }

    #[test]
    fn non_memory_paths_still_use_paths_write_glob() {
        // Make sure the memory dispatch doesn't break the existing
        // non-memory path checks.
        let token = token_with_paths("/workspace/**", "/workspace/**");
        token
            .check_path_write("/workspace/file.md")
            .expect("workspace write must be allowed");
        token
            .check_path_read("/workspace/file.md")
            .expect("workspace read must be allowed");
        token
            .check_path_write("/etc/passwd")
            .expect_err("non-workspace write must be denied");
    }

    #[test]
    fn invalid_memory_path_returns_capability_denied_not_panic() {
        let token = CapabilityToken {
            memory: MemoryCapability {
                enabled: true,
                search_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
                write_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
            },
            ..Default::default()
        };

        // After normalization, /var/memory/self/../escape.md becomes /var/memory/escape.md
        // which is a valid memory path but NOT under /var/memory/self scope
        let err = token
            .check_path_write("/var/memory/self/../escape.md")
            .expect_err("parent traversal must be rejected");
        assert!(
            err.reason.contains("not in any MemoryCapability")
                || err.reason.contains("invalid memory path"),
        );
    }

    // ─── Lookalike paths must NOT be treated as memory paths ───────────────

    #[test]
    fn lookalike_paths_fall_through_to_generic_paths_globs_not_memory() {
        // /var/memory.bak/foo and /mntfoo/bar are NOT under /var/memory or
        // /mnt — they must NOT trigger the memory dispatch and must use
        // the generic paths_write glob check instead.
        let token = token_with_paths("/**", "/**");
        token
            .check_path_write("/var/memory.bak/foo.md")
            .expect("lookalike must use generic glob, not memory");
        token
            .check_path_write("/mntfoo/bar.md")
            .expect("lookalike must use generic glob, not memory");
        token
            .check_path_read("/Var/Memory/file.md")
            .expect("uppercase must NOT match /var/memory case-sensitively");
    }

    #[test]
    fn exact_root_var_memory_with_no_trailing_path_is_a_memory_path() {
        // `/var/memory` (no trailing /) is the root of the memory subtree.
        // It must dispatch to the memory check, not the generic glob.
        let token = token_with_paths("/**", "/**");
        let err = token
            .check_path_write("/var/memory")
            .expect_err("exact /var/memory write must be denied without memory grant");
        assert!(err.reason.contains("not in any MemoryCapability"));
    }

    #[test]
    fn exact_root_mnt_with_no_trailing_path_is_a_memory_path() {
        let token = token_with_paths("/**", "/**");
        let err = token
            .check_path_read("/mnt")
            .expect_err("exact /mnt read must be denied without memory grant");
        assert!(err.reason.contains("not in any MemoryCapability"));
    }

    #[test]
    fn empty_path_is_not_a_memory_path_and_falls_through_to_glob() {
        // Empty string must NOT trigger the memory dispatch. With paths_*
        // = "/**" the generic glob accepts it; the inner VFS is responsible
        // for rejecting empty paths. We just pin the classification here.
        let token = token_with_paths("/**", "/**");
        token
            .check_path_read("")
            .expect("empty path goes through generic glob, not memory");
        token
            .check_path_write("")
            .expect("empty path goes through generic glob, not memory");

        // And with no glob grant, it should be a generic deny (NOT a memory
        // denial — verifying the dispatch did not misclassify).
        let no_grant = CapabilityToken::default();
        let err = no_grant
            .check_path_write("")
            .expect_err("empty path with no grant must be denied");
        assert_eq!(err.operation, "write_file");
        assert!(
            !err.reason.contains("MemoryCapability"),
            "empty path must be denied via generic glob, not memory dispatch; got: {}",
            err.reason
        );
    }

    #[test]
    fn trailing_slash_var_memory_dispatches_to_memory_check() {
        // `/var/memory/` (trailing slash) must dispatch to the memory
        // check. MemoryPath::parse will reject the empty trailing segment,
        // surfacing as a CapabilityDenied with "invalid memory path".
        let token = token_with_paths("/**", "/**");
        let err = token
            .check_path_write("/var/memory/")
            .expect_err("trailing slash root must dispatch to memory and be rejected");
        assert!(
            err.reason.contains("invalid memory path") || err.reason.contains("MemoryCapability"),
            "trailing-slash dispatch should hit memory path, got: {}",
            err.reason
        );
    }

    // ─── Sub-agent inheritance: prefix-aware intersect + is_subset_of ──────

    #[test]
    fn intersect_memory_scopes_narrows_when_child_is_inside_parent() {
        let parent = CapabilityToken {
            memory: MemoryCapability {
                enabled: true,
                search_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
                write_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
            },
            ..Default::default()
        };
        let child = CapabilityToken {
            memory: MemoryCapability {
                enabled: true,
                search_scopes: vec![MemoryPath::parse("/var/memory/self/notes").unwrap()],
                write_scopes: vec![MemoryPath::parse("/var/memory/self/notes").unwrap()],
            },
            ..Default::default()
        };

        let intersected = parent.intersect(&child);
        assert!(intersected.memory.enabled);
        assert_eq!(intersected.memory.search_scopes.len(), 1);
        assert_eq!(
            intersected.memory.search_scopes[0].as_str(),
            "/var/memory/self/notes",
            "child narrower scope must be honored, not dropped to empty"
        );
        assert_eq!(intersected.memory.write_scopes.len(), 1);
    }

    #[test]
    fn intersect_memory_scopes_drops_child_scopes_outside_parent() {
        let parent = CapabilityToken {
            memory: MemoryCapability {
                enabled: true,
                search_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
                write_scopes: vec![],
            },
            ..Default::default()
        };
        let child = CapabilityToken {
            memory: MemoryCapability {
                enabled: true,
                search_scopes: vec![
                    MemoryPath::parse("/var/memory/self/notes").unwrap(), // narrower — kept
                    MemoryPath::parse("/var/memory/users").unwrap(),      // outside — dropped
                ],
                write_scopes: vec![],
            },
            ..Default::default()
        };

        let intersected = parent.intersect(&child);
        let scopes: Vec<&str> = intersected
            .memory
            .search_scopes
            .iter()
            .map(|p| p.as_str())
            .collect();
        assert_eq!(scopes, vec!["/var/memory/self/notes"]);
    }

    #[test]
    fn intersect_memory_disabled_when_either_side_disabled() {
        let parent = CapabilityToken {
            memory: MemoryCapability {
                enabled: true,
                search_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
                write_scopes: vec![],
            },
            ..Default::default()
        };
        let child = CapabilityToken {
            memory: MemoryCapability::default(), // disabled
            ..Default::default()
        };

        let intersected = parent.intersect(&child);
        assert!(!intersected.memory.enabled);
    }

    #[test]
    fn is_subset_of_rejects_child_memory_scope_outside_parent() {
        let parent = CapabilityToken {
            memory: MemoryCapability {
                enabled: true,
                search_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
                write_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
            },
            ..Default::default()
        };
        let child_with_extra = CapabilityToken {
            memory: MemoryCapability {
                enabled: true,
                search_scopes: vec![
                    MemoryPath::parse("/var/memory/self/notes").unwrap(),
                    MemoryPath::parse("/var/memory/users").unwrap(),
                ],
                write_scopes: vec![],
            },
            ..Default::default()
        };

        assert!(
            !child_with_extra.is_subset_of(&parent),
            "child claiming /var/memory/users when parent only grants /var/memory/self must NOT be a subset"
        );
    }

    #[test]
    fn is_subset_of_accepts_child_narrower_memory_scope() {
        let parent = CapabilityToken {
            memory: MemoryCapability {
                enabled: true,
                search_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
                write_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
            },
            ..Default::default()
        };
        let child_narrower = CapabilityToken {
            memory: MemoryCapability {
                enabled: true,
                search_scopes: vec![MemoryPath::parse("/var/memory/self/notes").unwrap()],
                write_scopes: vec![MemoryPath::parse("/var/memory/self/notes").unwrap()],
            },
            ..Default::default()
        };

        assert!(
            child_narrower.is_subset_of(&parent),
            "child narrowing parent's memory scope must be a valid subset"
        );
    }

    #[test]
    fn is_subset_of_rejects_child_enabling_memory_when_parent_disabled() {
        let parent = CapabilityToken {
            memory: MemoryCapability::default(), // disabled
            ..Default::default()
        };
        let child = CapabilityToken {
            memory: MemoryCapability {
                enabled: true,
                search_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
                write_scopes: vec![],
            },
            ..Default::default()
        };

        assert!(
            !child.is_subset_of(&parent),
            "child cannot enable memory when parent has it disabled"
        );
    }
}
