/// Simple glob pattern matcher for mcp_tools capability checks.
///
/// Supports `*` as a wildcard that matches any sequence of characters
/// (including empty) within a single segment, and `**` is not treated
/// specially — `*` is greedy within the matched portion.
pub(crate) fn glob_match(pattern: &str, value: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        // No wildcard — exact match
        return pattern == value;
    }

    let mut pos = 0;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            // First segment must be a prefix
            if !value.starts_with(part) {
                return false;
            }
            pos = part.len();
        } else if i == parts.len() - 1 {
            // Last segment must be a suffix
            if !value[pos..].ends_with(part) {
                return false;
            }
            pos = value.len();
        } else {
            // Middle segments must appear in order
            match value[pos..].find(part) {
                Some(idx) => pos += idx + part.len(),
                None => return false,
            }
        }
    }
    true
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    pub(crate) fn glob_match_exact() {
        assert!(glob_match("foo", "foo"));
    }

    #[test]
    pub(crate) fn glob_match_exact_mismatch() {
        assert!(!glob_match("foo", "bar"));
    }

    #[test]
    pub(crate) fn glob_match_wildcard_suffix() {
        assert!(glob_match("mcp:server:*", "mcp:server:tool1"));
    }

    #[test]
    pub(crate) fn glob_match_wildcard_matches_empty() {
        assert!(glob_match("mcp:server:*", "mcp:server:"));
    }

    #[test]
    pub(crate) fn glob_match_wildcard_prefix() {
        assert!(glob_match("*:tool", "server:tool"));
    }

    #[test]
    pub(crate) fn glob_match_wildcard_middle() {
        assert!(glob_match("mcp:*:tool", "mcp:server:tool"));
    }

    #[test]
    pub(crate) fn glob_match_wildcard_no_match() {
        assert!(!glob_match("mcp:server:*", "other:server:tool"));
    }

    #[test]
    pub(crate) fn glob_match_empty_pattern_empty_value() {
        assert!(glob_match("", ""));
    }

    #[test]
    pub(crate) fn glob_match_empty_pattern_nonempty_value() {
        assert!(!glob_match("", "foo"));
    }

    #[test]
    pub(crate) fn glob_match_nonempty_pattern_empty_value() {
        assert!(!glob_match("foo", ""));
    }

    #[test]
    pub(crate) fn glob_match_star_matches_everything() {
        assert!(glob_match("*", "anything-at-all"));
    }

    #[test]
    pub(crate) fn glob_match_star_matches_empty_string() {
        assert!(glob_match("*", ""));
    }
}
