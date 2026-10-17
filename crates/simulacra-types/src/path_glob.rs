//! The path patterns a capability grants: `/**` (everything), `/dir/**`
//! (`/dir` and everything below it), `/dir/*` (the entries directly inside
//! `/dir`), and an exact path. Nothing else is a pattern: a `*` anywhere else
//! is matched literally, so callers that accept patterns from a model refuse
//! one with [`is_supported`] rather than grant a path nothing can reach.

/// Whether `pattern` is one of the forms this module matches.
pub fn is_supported(pattern: &str) -> bool {
    let body = pattern
        .strip_suffix("/**")
        .or_else(|| pattern.strip_suffix("/*"))
        .unwrap_or(pattern);
    pattern.starts_with('/') && !body.contains('*')
}

/// Whether `path` (normalised, absolute) is granted by `pattern`.
pub(crate) fn matches(pattern: &str, path: &str) -> bool {
    if pattern == "/**" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return within(prefix, path);
    }
    if let Some(dir) = pattern.strip_suffix("/*") {
        return directly_inside(dir, path);
    }
    pattern == path
}

/// Whether every path `child` grants is also granted by `parent`.
pub(crate) fn covers(parent: &str, child: &str) -> bool {
    if parent == child || parent == "/**" {
        return true;
    }
    if let Some(prefix) = parent.strip_suffix("/**") {
        let base = child
            .strip_suffix("/**")
            .or_else(|| child.strip_suffix("/*"))
            .unwrap_or(child);
        return within(prefix, base);
    }
    if let Some(dir) = parent.strip_suffix("/*") {
        // Only an exact path directly inside `dir` is narrower than `/dir/*`.
        return !child.contains('*') && directly_inside(dir, child);
    }
    false
}

/// The pattern granting exactly the paths both `a` and `b` grant, if any.
/// Usually that is the narrower of the two; the one overlap neither covers is
/// `/dir/*` against `/dir/name/**`, which share only the path `/dir/name`.
pub(crate) fn intersect(a: &str, b: &str) -> Option<String> {
    if covers(b, a) {
        return Some(a.to_owned());
    }
    if covers(a, b) {
        return Some(b.to_owned());
    }
    let shared_entry = |star: &str, subtree: &str| {
        let dir = star.strip_suffix("/*")?;
        let base = subtree.strip_suffix("/**")?;
        directly_inside(dir, base).then(|| base.to_owned())
    };
    shared_entry(a, b).or_else(|| shared_entry(b, a))
}

/// `path` is `prefix` or below it, on a segment boundary.
fn within(prefix: &str, path: &str) -> bool {
    path.starts_with(prefix)
        && (path.len() == prefix.len() || path.as_bytes().get(prefix.len()) == Some(&b'/'))
}

/// `path` is one segment below `dir`.
fn directly_inside(dir: &str, path: &str) -> bool {
    path.strip_prefix(dir)
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|name| !name.is_empty() && !name.contains('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_star_grants_the_entries_directly_inside_a_directory() {
        assert!(matches("/artifacts/*", "/artifacts/probe.md"));
        assert!(!matches("/artifacts/*", "/artifacts/sub/x.md"));
        assert!(!matches("/artifacts/*", "/artifacts"));
        assert!(!matches("/artifacts/*", "/artifactsx/y"));
    }

    #[test]
    fn the_existing_forms_match_as_before() {
        assert!(matches("/**", "/anything/at/all"));
        assert!(matches("/a/**", "/a"));
        assert!(matches("/a/**", "/a/b/c"));
        assert!(!matches("/a/**", "/ab"));
        assert!(matches("/a/b.txt", "/a/b.txt"));
        assert!(!matches("/a/b.txt", "/a/b.txt/c"));
    }

    #[test]
    fn coverage_never_widens_a_single_star() {
        assert!(covers("/**", "/artifacts/*"));
        assert!(covers("/artifacts/**", "/artifacts/*"));
        assert!(covers("/artifacts/*", "/artifacts/x.md"));
        assert!(covers("/artifacts/*", "/artifacts/*"));
        assert!(!covers("/artifacts/*", "/artifacts/**"));
        assert!(!covers("/artifacts/*", "/artifacts/sub/x.md"));
        assert!(!covers("/artifacts/*", "/artifacts"));
        assert!(!covers("/artifacts/sub/**", "/artifacts/*"));
    }

    #[test]
    fn a_single_star_and_a_subtree_below_one_entry_share_exactly_that_entry() {
        assert_eq!(
            intersect("/artifacts/*", "/artifacts/report/**").as_deref(),
            Some("/artifacts/report")
        );
        assert_eq!(
            intersect("/artifacts/report/**", "/artifacts/*").as_deref(),
            Some("/artifacts/report")
        );
        assert_eq!(intersect("/artifacts/*", "/artifacts/a/b/**"), None);
        assert_eq!(intersect("/artifacts/*", "/notes/**"), None);
        assert_eq!(
            intersect("/**", "/artifacts/*").as_deref(),
            Some("/artifacts/*")
        );
        assert_eq!(
            intersect("/artifacts/*", "/artifacts/x.md").as_deref(),
            Some("/artifacts/x.md")
        );
    }

    #[test]
    fn only_the_matched_forms_are_supported() {
        for ok in ["/**", "/a/**", "/a/*", "/a/b.txt", "/"] {
            assert!(is_supported(ok), "{ok}");
        }
        for bad in ["/a/*.md", "/a/**/b", "*", "a/**", "/a*", "/a/*/b"] {
            assert!(!is_supported(bad), "{bad}");
        }
    }
}
