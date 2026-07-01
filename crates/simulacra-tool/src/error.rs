//! Tool crate error types.

/// Errors that can occur during skill discovery and filtering.
#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    /// Two skills resolved to the same frontmatter `name`.
    #[error("duplicate skill name {name:?} discovered at {first_path} and {second_path}")]
    DuplicateSkillName {
        name: String,
        first_path: String,
        second_path: String,
    },

    /// An agent type references a skill that was not found in the VFS.
    #[error(
        "agent type {agent_type:?} references undiscoverable skill {skill:?}: \
         no valid /skills/{skill}/SKILL.md found"
    )]
    UndiscoverableSkill { agent_type: String, skill: String },
}
