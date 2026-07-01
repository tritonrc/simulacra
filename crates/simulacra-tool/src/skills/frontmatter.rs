use serde::Deserialize;

use super::SkillMeta;

#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    disable_model_invocation: bool,
    #[serde(default = "default_true")]
    allow_implicit_invocation: bool,
    #[serde(default = "default_true")]
    user_invocable: bool,
    #[serde(default)]
    allowed_tools: Vec<String>,
}

fn default_true() -> bool {
    true
}

/// Parse SKILL.md YAML frontmatter into a SkillMeta.
///
/// A valid skill directory requires `SKILL.md` with YAML frontmatter plus a
/// markdown body. The `name` field is the canonical identifier used by both
/// `Skill(command=...)` and `/skill-name`. The `description` field is exposed
/// in the model-visible skill catalog.
///
/// `disable_model_invocation: true` blocks model-triggered invocation.
/// `allow_implicit_invocation: false` hides the skill from model-visible skill
/// metadata and blocks model-triggered invocation without affecting `/skill-name`.
/// `user_invocable: false` blocks `/skill-name` invocation but the skill may
/// still appear in the model-visible catalog if model invocation is enabled.
///
/// `allowed_tools` narrows interactive pre-approval only and does NOT widen
/// capability policy.
pub fn parse_skill_frontmatter(content: &str, vfs_path: &str) -> Result<SkillMeta, String> {
    let (yaml_str, body) = split_yaml_frontmatter(content)?;
    let frontmatter: SkillFrontmatter = serde_yaml::from_str(yaml_str)
        .map_err(|err| format!("invalid SKILL.md YAML frontmatter: {err}"))?;

    let name = required_text(frontmatter.name, "name")?;
    let description = required_text(frontmatter.description, "description")?;
    let body = body.trim();
    if body.is_empty() {
        return Err("SKILL.md requires a markdown body after the YAML frontmatter".into());
    }

    Ok(SkillMeta {
        name,
        description,
        vfs_path: vfs_path.to_string(),
        disable_model_invocation: frontmatter.disable_model_invocation,
        allow_implicit_invocation: frontmatter.allow_implicit_invocation,
        user_invocable: frontmatter.user_invocable,
        allowed_tools: frontmatter
            .allowed_tools
            .into_iter()
            .map(|tool| tool.trim().to_string())
            .filter(|tool| !tool.is_empty())
            .collect(),
        body: Some(body.to_string()),
    })
}

/// Strip YAML frontmatter from a SKILL.md string, returning only the markdown
/// body after the closing delimiter.
#[cfg(feature = "sandbox")]
pub(super) fn strip_yaml_frontmatter(content: &str) -> String {
    match split_yaml_frontmatter(content) {
        Ok((_, body)) => body.trim_start_matches(['\r', '\n']).to_string(),
        Err(_) => content.to_string(),
    }
}

fn split_yaml_frontmatter(content: &str) -> Result<(&str, &str), String> {
    let trimmed = content.trim_start();
    let after_open = trimmed
        .strip_prefix("---")
        .ok_or("SKILL.md must begin with YAML frontmatter (---)")?;
    let after_open = after_open
        .strip_prefix("\r\n")
        .or_else(|| after_open.strip_prefix('\n'))
        .ok_or("SKILL.md frontmatter opening --- must be followed by a newline")?;

    let mut byte_offset = 0;
    for line in after_open.split_inclusive('\n') {
        let line_text = line.trim_end_matches(['\r', '\n']);
        if line_text == "---" {
            let body_start = byte_offset + line.len();
            return Ok((&after_open[..byte_offset], &after_open[body_start..]));
        }
        byte_offset += line.len();
    }

    if after_open[byte_offset..].trim_end_matches('\r') == "---" {
        return Ok((&after_open[..byte_offset], ""));
    }

    Err("SKILL.md frontmatter missing closing ---".into())
}

fn required_text(value: String, field: &str) -> Result<String, String> {
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(format!(
            "SKILL.md frontmatter missing required field: {field}"
        ));
    }
    Ok(value)
}
