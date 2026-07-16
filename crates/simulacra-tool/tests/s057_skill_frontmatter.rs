use simulacra_tool::parse_skill_frontmatter;

#[test]
fn mcp_servers_frontmatter_rejects_a_non_array_dependency() {
    let content = "---\nname: repo-work\ndescription: Work with repository issues.\nmcp_servers: github\n---\nUse the repository catalog.\n";

    let error = parse_skill_frontmatter(content, "/skills/repo-work/SKILL.md")
        .expect_err("mcp_servers must be an array of configured server names");

    assert!(
        error.contains("mcp_servers"),
        "the invalid dependency error should name mcp_servers, got: {error}"
    );
}

#[test]
fn mcp_servers_frontmatter_rejects_non_string_dependencies() {
    let content = "---\nname: repo-work\ndescription: Work with repository issues.\nmcp_servers:\n  - github\n  - 42\n---\nUse the repository catalog.\n";

    let error = parse_skill_frontmatter(content, "/skills/repo-work/SKILL.md")
        .expect_err("every mcp_servers dependency must be a server-name string");

    assert!(
        error.contains("mcp_servers"),
        "the invalid dependency error should name mcp_servers, got: {error}"
    );
}

#[test]
fn mcp_servers_frontmatter_rejects_blank_dependencies() {
    let content = "---\nname: repo-work\ndescription: Work with repository issues.\nmcp_servers:\n  - github\n  - '   '\n---\nUse the repository catalog.\n";

    let error = parse_skill_frontmatter(content, "/skills/repo-work/SKILL.md")
        .expect_err("blank configured MCP server names must invalidate the skill");

    assert!(
        error.contains("mcp_servers"),
        "the invalid dependency error should name mcp_servers, got: {error}"
    );
}
