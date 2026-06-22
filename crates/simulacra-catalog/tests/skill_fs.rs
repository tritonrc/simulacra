use std::collections::HashSet;

use chrono::Utc;
use serde_json::{Value, json};
use simulacra_catalog::models::Skill;
use simulacra_catalog::{CatalogSkillFs, SkillId, TenantId};
use simulacra_types::{VfsError, VirtualFs};
use simulacra_vfs::{MemoryFs, OverlayFs};

fn skill(name: &str, body: &str, metadata: Option<serde_json::Value>) -> Skill {
    let now = Utc::now();
    Skill {
        id: SkillId::new(),
        tenant_id: TenantId::from("tenant"),
        name: name.to_owned(),
        description: None,
        body: body.to_owned(),
        metadata,
        created_at: now,
        updated_at: now,
    }
}

#[test]
fn list_dir_root_returns_skill_filenames() {
    let fs = CatalogSkillFs::new(vec![skill("alpha", "a", None), skill("beta", "b", None)]);

    let entries = fs.list_dir("/").unwrap();
    let entry_set: HashSet<String> = entries.into_iter().collect();

    assert_eq!(entry_set.len(), 2, "expected exactly two entries");
    assert!(entry_set.contains("alpha.md"));
    assert!(entry_set.contains("beta.md"));
}

#[test]
fn read_returns_body_with_frontmatter_when_metadata_present() {
    let metadata = json!({"name": "alpha", "description": "d", "version": 2});
    let fs = CatalogSkillFs::new(vec![skill("alpha", "Hello body.", Some(metadata.clone()))]);

    let bytes = fs.read("/alpha.md").unwrap();
    let rendered = String::from_utf8(bytes).unwrap();

    // Structural assertion — split on the YAML delimiter.
    // Expected layout: "---\n" + yaml + "---\n\n" + body
    assert!(
        rendered.starts_with("---\n"),
        "rendered output must start with ---\\n; got: {rendered}"
    );

    // Strip the leading delimiter.
    let after_open = rendered.strip_prefix("---\n").expect("starts with ---\\n");

    // Find the closing delimiter "\n---\n" (so the YAML block is followed by ---).
    // Accept either "\n---\n\n" + body or "---\n\n" + body.
    let close_marker = "---\n";
    let close_idx = after_open
        .find(&format!("\n{close_marker}"))
        .map(|i| i + 1) // position of the start of "---\n"
        .or_else(|| after_open.find(close_marker))
        .expect("frontmatter must be terminated by ---\\n");

    let yaml_section = &after_open[..close_idx];
    // Body starts after the closing "---\n".
    let after_close = &after_open[close_idx + close_marker.len()..];
    // The frontmatter block must be followed by a blank line, then the body.
    let body = after_close.strip_prefix('\n').unwrap_or(after_close);

    assert_eq!(body, "Hello body.", "body did not match");

    // Closing delimiter position must come before the body.
    let body_start_idx = rendered.find("Hello body.").expect("body present");
    let closing_idx = rendered
        .rfind(&format!("\n{close_marker}"))
        .map(|i| i + 1)
        .or_else(|| rendered[..body_start_idx].rfind(close_marker))
        .expect("closing delimiter present");
    assert!(
        closing_idx < body_start_idx,
        "closing --- delimiter must appear before the body"
    );

    // Parse the YAML middle section as JSON via serde_json::from_str
    // -> use a YAML-shaped structural check: each top-level key must appear with its value.
    // We assert that round-tripping works by deserializing the YAML using a
    // manual line-based check (avoid serde_yaml since it's not a workspace dep).
    let mut parsed: serde_json::Map<String, Value> = serde_json::Map::new();
    for line in yaml_section.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let (k, v) = line.split_once(':').expect("YAML line must contain ':'");
        let key = k.trim().to_owned();
        let raw = v.trim();
        // Strip optional surrounding quotes.
        let stripped = raw
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            .unwrap_or(raw);
        let value: Value = if let Ok(n) = stripped.parse::<i64>() {
            Value::Number(n.into())
        } else {
            Value::String(stripped.to_owned())
        };
        parsed.insert(key, value);
    }
    let parsed_value = Value::Object(parsed);
    assert_eq!(
        parsed_value, metadata,
        "parsed YAML frontmatter must round-trip to input metadata; got {parsed_value} vs {metadata}"
    );
}

#[test]
fn read_returns_body_only_when_metadata_absent() {
    let fs = CatalogSkillFs::new(vec![skill("alpha", "Hello body.", None)]);

    let bytes = fs.read("/alpha.md").unwrap();

    assert_eq!(String::from_utf8(bytes).unwrap(), "Hello body.");
}

#[test]
fn read_missing_returns_noent() {
    let fs = CatalogSkillFs::new(vec![]);

    let err = fs.read("/missing.md").unwrap_err();

    assert!(matches!(err, VfsError::NotFound(_)));
}

#[test]
fn write_returns_readonly_error() {
    let fs = CatalogSkillFs::new(vec![skill("alpha", "body", None)]);

    let err = fs.write("/alpha.md", b"new body").unwrap_err();

    assert!(matches!(err, VfsError::PermissionDenied(_)));
}

#[test]
fn remove_returns_readonly_error() {
    let fs = CatalogSkillFs::new(vec![skill("alpha", "body", None)]);

    let err = fs.remove("/alpha.md").unwrap_err();

    assert!(matches!(err, VfsError::PermissionDenied(_)));
}

#[test]
fn shadows_host_skill_with_same_name() {
    let host = MemoryFs::new();
    host.write("/alpha.md", b"host body").unwrap();

    let catalog = CatalogSkillFs::new(vec![skill("alpha", "catalog body", None)]);
    let overlay = OverlayFs::new(Box::new(host), Box::new(catalog));

    let bytes = overlay.read("/alpha.md").unwrap();

    assert_eq!(String::from_utf8(bytes).unwrap(), "catalog body");
}
