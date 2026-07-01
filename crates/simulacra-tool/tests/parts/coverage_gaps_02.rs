// ---------------------------------------------------------------------------
// FT2: parse_skill_frontmatter unit tests
// ---------------------------------------------------------------------------

#[test]
fn parse_skill_frontmatter_extracts_name_and_description() {
    let content = "\
---
name: code-review
description: Review code for quality
---
# Code Review Skill

Detailed instructions here.
";
    let meta = parse_skill_frontmatter(content, "/skills/cr/SKILL.md").unwrap();
    assert_eq!(meta.name, "code-review");
    assert_eq!(meta.description, "Review code for quality");
    assert_eq!(meta.vfs_path, "/skills/cr/SKILL.md");
    assert!(!meta.disable_model_invocation);
    assert!(meta.allow_implicit_invocation);
    assert!(meta.user_invocable);
    assert!(meta.allowed_tools.is_empty());
}

#[test]
fn parse_skill_frontmatter_missing_opening_delimiter_returns_error() {
    let content = "name: oops\n---\nBody here.\n";
    let err = parse_skill_frontmatter(content, "/skills/bad/SKILL.md").unwrap_err();
    assert!(
        err.contains("frontmatter"),
        "expected error about frontmatter, got: {err}"
    );
}

#[test]
fn parse_skill_frontmatter_missing_closing_delimiter_returns_error() {
    let content = "---\nname: oops\ndescription: bad\n";
    let err = parse_skill_frontmatter(content, "/skills/bad/SKILL.md").unwrap_err();
    assert!(
        err.contains("closing"),
        "expected error about closing delimiter, got: {err}"
    );
}

#[test]
fn parse_skill_frontmatter_missing_name_field_returns_error() {
    let content = "---\ndescription: no name\n---\nBody here.\n";
    let err = parse_skill_frontmatter(content, "/skills/bad/SKILL.md").unwrap_err();
    assert!(
        err.contains("name"),
        "expected error about missing name, got: {err}"
    );
}

#[test]
fn parse_skill_frontmatter_missing_description_field_returns_error() {
    let content = "---\nname: orphan\n---\nBody here.\n";
    let err = parse_skill_frontmatter(content, "/skills/bad/SKILL.md").unwrap_err();
    assert!(
        err.contains("description"),
        "expected error about missing description, got: {err}"
    );
}

#[test]
fn parse_skill_frontmatter_empty_body_returns_error() {
    let content = "---\nname: empty\ndescription: no body\n---\n";
    let err = parse_skill_frontmatter(content, "/skills/bad/SKILL.md").unwrap_err();
    assert!(
        err.contains("body"),
        "expected error about missing body, got: {err}"
    );
}

#[test]
fn parse_skill_frontmatter_reads_disable_model_invocation() {
    let content = "\
---
name: internal
description: Internal only
disable_model_invocation: true
---
# Internal Skill

Body text.
";
    let meta = parse_skill_frontmatter(content, "/skills/internal/SKILL.md").unwrap();
    assert!(meta.disable_model_invocation);
}

#[test]
fn parse_skill_frontmatter_reads_user_invocable_false() {
    let content = "\
---
name: hidden
description: Not user-invocable
user_invocable: false
---
# Hidden Skill

Body text.
";
    let meta = parse_skill_frontmatter(content, "/skills/hidden/SKILL.md").unwrap();
    assert!(!meta.user_invocable);
}

#[test]
fn parse_skill_frontmatter_reads_allow_implicit_invocation_false() {
    let content = "\
---
name: quiet
description: User-triggered only
allow_implicit_invocation: false
---
# Quiet Skill

Body text.
";
    let meta = parse_skill_frontmatter(content, "/skills/quiet/SKILL.md").unwrap();
    assert!(!meta.allow_implicit_invocation);
}

#[test]
fn parse_skill_frontmatter_reads_allowed_tools_list() {
    let content = "\
---
name: builder
description: Build things
allowed_tools:
- shell_exec
- file_write
---
# Builder Skill

Body text.
";
    let meta = parse_skill_frontmatter(content, "/skills/builder/SKILL.md").unwrap();
    assert_eq!(meta.allowed_tools, vec!["shell_exec", "file_write"]);
}

#[test]
fn parse_skill_frontmatter_accepts_real_yaml_scalars_and_sequences() {
    let content = "\
---
name: \"review:rust\"
description: \"Use cargo: fmt, clippy, and test.\"
allowed_tools: [\"shell_exec\", \"file_read\"]
---
# Rust Review

Body text.
";
    let meta = parse_skill_frontmatter(content, "/skills/review/SKILL.md").unwrap();
    assert_eq!(meta.name, "review:rust");
    assert_eq!(meta.description, "Use cargo: fmt, clippy, and test.");
    assert_eq!(meta.allowed_tools, vec!["shell_exec", "file_read"]);
}

#[test]
fn parse_skill_frontmatter_populates_body_field() {
    let content = "\
---
name: test
description: Test skill
---
# Test Skill

This is the body.
";
    let meta = parse_skill_frontmatter(content, "/skills/test/SKILL.md").unwrap();
    assert!(meta.body.is_some());
    let body = meta.body.unwrap();
    assert!(
        body.contains("This is the body"),
        "expected body to contain content, got: {body}"
    );
}
