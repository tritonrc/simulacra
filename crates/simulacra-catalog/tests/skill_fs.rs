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
fn list_dir_root_returns_skill_directories() {
    let fs = CatalogSkillFs::new(vec![skill("alpha", "a", None), skill("beta", "b", None)]);

    let entries = fs.list_dir("/").unwrap();
    let entry_set: HashSet<String> = entries.into_iter().collect();

    assert_eq!(entry_set.len(), 2, "expected exactly two entries");
    assert!(entry_set.contains("alpha"));
    assert!(entry_set.contains("beta"));
}

#[test]
fn list_dir_skill_directory_returns_skill_md() {
    let fs = CatalogSkillFs::new(vec![skill("alpha", "a", None)]);

    assert_eq!(fs.list_dir("/alpha").unwrap(), vec!["SKILL.md"]);
    assert_eq!(fs.list_dir("/alpha/").unwrap(), vec!["SKILL.md"]);
}

#[test]
fn invalid_path_segment_skill_names_are_not_exposed() {
    let fs = CatalogSkillFs::new(vec![
        skill("alpha", "a", None),
        skill("bad/name", "b", None),
        skill(".", "dot", None),
        skill("..", "dotdot", None),
    ]);

    assert_eq!(fs.list_dir("/").unwrap(), vec!["alpha"]);
    assert!(matches!(
        fs.read("/bad/name/SKILL.md").unwrap_err(),
        VfsError::NotFound(_)
    ));
    assert!(matches!(
        fs.read("/./SKILL.md").unwrap_err(),
        VfsError::NotFound(_)
    ));
    assert!(matches!(
        fs.read("/../SKILL.md").unwrap_err(),
        VfsError::NotFound(_)
    ));
}

#[test]
fn read_returns_body_with_frontmatter_when_metadata_present() {
    let metadata = json!({"name": "alpha", "description": "d", "version": 2});
    let fs = CatalogSkillFs::new(vec![skill("alpha", "Hello body.", Some(metadata.clone()))]);

    let bytes = fs.read("/alpha/SKILL.md").unwrap();
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
fn read_returns_frontmatter_from_row_fields_when_metadata_absent() {
    let fs = CatalogSkillFs::new(vec![skill("alpha", "Hello body.", None)]);

    let bytes = fs.read("/alpha/SKILL.md").unwrap();
    let rendered = String::from_utf8(bytes).unwrap();

    assert!(rendered.starts_with("---\n"));
    assert!(rendered.contains("name: alpha"));
    assert!(rendered.contains("description: alpha"));
    assert!(rendered.ends_with("Hello body."));
}

#[test]
fn read_missing_returns_noent() {
    let fs = CatalogSkillFs::new(vec![]);

    let err = fs.read("/missing/SKILL.md").unwrap_err();

    assert!(matches!(err, VfsError::NotFound(_)));
}

#[test]
fn write_returns_readonly_error() {
    let fs = CatalogSkillFs::new(vec![skill("alpha", "body", None)]);

    let err = fs.write("/alpha/SKILL.md", b"new body").unwrap_err();

    assert!(matches!(err, VfsError::PermissionDenied(_)));
}

#[test]
fn remove_returns_readonly_error() {
    let fs = CatalogSkillFs::new(vec![skill("alpha", "body", None)]);

    let err = fs.remove("/alpha/SKILL.md").unwrap_err();

    assert!(matches!(err, VfsError::PermissionDenied(_)));
}

#[test]
fn shadows_host_skill_with_same_name() {
    let host = MemoryFs::new();
    host.mkdir("/alpha").unwrap();
    host.write("/alpha/SKILL.md", b"host body").unwrap();

    let catalog = CatalogSkillFs::new(vec![skill("alpha", "catalog body", None)]);
    let overlay = OverlayFs::new(Box::new(host), Box::new(catalog));

    let bytes = overlay.read("/alpha/SKILL.md").unwrap();

    assert!(String::from_utf8(bytes).unwrap().contains("catalog body"));
}

#[test]
fn metadata_reports_root_skill_dir_and_skill_file_shapes() {
    let fs = CatalogSkillFs::new(vec![skill("alpha", "body", None)]);

    let root = fs.metadata("/").unwrap();
    assert!(root.is_dir);
    assert!(!root.is_file);

    let dir = fs.metadata("/alpha").unwrap();
    assert!(dir.is_dir);
    assert!(!dir.is_file);

    let file = fs.metadata("/alpha/SKILL.md").unwrap();
    assert!(file.is_file);
    assert!(!file.is_dir);
    assert!(file.size > 0);
}
