/// Pure-helper version of the per-server network allowlist check used by
/// `simulacra:http/fetch` (S041 spec § Networking allowlist semantics, assertion
/// 21). `host_port` is the candidate destination as `"host:port"`. Patterns
/// supported in `allowlist`:
///
/// - `"api.github.com:443"` — exact host, exact port
/// - `"*.stripe.com:443"` — single-level subdomain glob, exact port
/// - `"localhost:*"`, `"127.0.0.1:*"` — exact host, any port
///
/// Empty allowlist → `false` (default-deny). Inputs without a colon are
/// rejected.  Host comparison is case-insensitive; port comparison is exact.
pub fn check_network_allowlist(host_port: &str, allowlist: &[String]) -> bool {
    if allowlist.is_empty() {
        return false;
    }
    let Some((cand_host, cand_port)) = split_host_port(host_port) else {
        return false;
    };
    let cand_host_lower = cand_host.to_ascii_lowercase();

    for pattern in allowlist {
        let Some((pat_host, pat_port)) = split_host_port(pattern) else {
            continue;
        };

        if !port_matches(pat_port, cand_port) {
            continue;
        }
        if host_matches(pat_host, &cand_host_lower) {
            return true;
        }
    }
    false
}

/// Split a `"host:port"` string at the *last* colon so that bracketed
/// IPv6 literals like `"[::1]:443"` and bare IPv4/DNS hosts both parse.
fn split_host_port(value: &str) -> Option<(&str, &str)> {
    let idx = value.rfind(':')?;
    let (host, port_with_colon) = value.split_at(idx);
    if host.is_empty() {
        return None;
    }
    Some((host, &port_with_colon[1..]))
}

fn port_matches(pattern_port: &str, candidate_port: &str) -> bool {
    pattern_port == "*" || pattern_port == candidate_port
}

/// Match a host pattern against a (lowercased) candidate host.
///
/// Supports a leading `*.` glob meaning "any single subdomain segment under
/// this parent." `*.example.com` matches `api.example.com` but not
/// `example.com` itself, nor `a.b.example.com`.
fn host_matches(pattern_host: &str, candidate_host_lower: &str) -> bool {
    let pattern_lower = pattern_host.to_ascii_lowercase();
    if let Some(suffix) = pattern_lower.strip_prefix("*.") {
        // Require exactly one extra label in front of `suffix`.
        let Some(prefix) = candidate_host_lower.strip_suffix(suffix) else {
            return false;
        };
        let Some(label) = prefix.strip_suffix('.') else {
            return false;
        };
        // The label itself must be non-empty and contain no dots.
        !label.is_empty() && !label.contains('.')
    } else {
        pattern_lower == candidate_host_lower
    }
}

/// Extract the `host:port` pair from a URL for the allowlist check. Falls
/// back to the protocol-default port when the URL has none.
pub(crate) fn extract_host_port(url_str: &str) -> Option<String> {
    let parsed = url::Url::parse(url_str).ok()?;
    let host = parsed.host_str()?;
    let port = parsed.port_or_known_default()?;
    Some(format!("{host}:{port}"))
}
