use super::*;

#[test]
fn parser_empty_input_returns_empty_shell_line() {
    let line = crate::parse("");
    assert!(line.items.is_empty());
}

#[test]
fn parser_whitespace_only_returns_empty_shell_line() {
    let line = crate::parse("   \t  ");
    assert!(line.items.is_empty());
}

#[test]
fn parser_single_command_no_args() {
    let line = crate::parse("ls");
    assert_eq!(line.items.len(), 1);
    assert_eq!(line.items[0].pipeline.commands.len(), 1);
    assert_eq!(line.items[0].pipeline.commands[0].program, "ls");
    assert!(line.items[0].pipeline.commands[0].args.is_empty());
    assert!(line.items[0].connector.is_none());
}

#[test]
fn parser_command_with_multiple_args() {
    let line = crate::parse("grep -r pattern dir");
    assert_eq!(line.items.len(), 1);
    let cmd = &line.items[0].pipeline.commands[0];
    assert_eq!(cmd.program, "grep");
    assert_eq!(cmd.args, vec!["-r", "pattern", "dir"]);
}

#[test]
fn parser_pipe_produces_multi_command_pipeline() {
    let line = crate::parse("echo hello | grep hello | wc -l");
    assert_eq!(line.items.len(), 1);
    let pipeline = &line.items[0].pipeline;
    assert_eq!(pipeline.commands.len(), 3);
    assert_eq!(pipeline.commands[0].program, "echo");
    assert_eq!(pipeline.commands[1].program, "grep");
    assert_eq!(pipeline.commands[2].program, "wc");
}

#[test]
fn parser_redirect_truncate() {
    let line = crate::parse("echo hello > /out.txt");
    let cmd = &line.items[0].pipeline.commands[0];
    assert_eq!(cmd.program, "echo");
    assert_eq!(cmd.args, vec!["hello"]);
    assert_eq!(cmd.redirects.len(), 1);
    assert_eq!(cmd.redirects[0].stream, crate::RedirectStream::Stdout);
    assert_eq!(cmd.redirects[0].kind, crate::RedirectKind::Truncate);
    assert_eq!(
        cmd.redirects[0].target,
        crate::RedirectTarget::File("/out.txt".to_string(), false)
    );
}

#[test]
fn parser_redirect_append() {
    let line = crate::parse("echo hello >> /out.txt");
    let cmd = &line.items[0].pipeline.commands[0];
    assert_eq!(cmd.redirects.len(), 1);
    assert_eq!(cmd.redirects[0].stream, crate::RedirectStream::Stdout);
    assert_eq!(cmd.redirects[0].kind, crate::RedirectKind::Append);
    assert_eq!(
        cmd.redirects[0].target,
        crate::RedirectTarget::File("/out.txt".to_string(), false)
    );
}

#[test]
fn parser_double_quoted_string_preserves_spaces() {
    let line = crate::parse("echo \"hello world\"");
    let cmd = &line.items[0].pipeline.commands[0];
    assert_eq!(cmd.program, "echo");
    assert_eq!(cmd.args, vec!["hello world"]);
}

#[test]
fn parser_single_quoted_string_preserves_spaces() {
    let line = crate::parse("echo 'hello world'");
    let cmd = &line.items[0].pipeline.commands[0];
    assert_eq!(cmd.program, "echo");
    assert_eq!(cmd.args, vec!["hello world"]);
}

#[test]
fn parser_and_connector_splits_items() {
    let line = crate::parse("echo a && echo b");
    assert_eq!(line.items.len(), 2);
    assert_eq!(line.items[0].connector, Some(crate::parser::Connector::And));
    assert!(line.items[1].connector.is_none());
    assert_eq!(line.items[0].pipeline.commands[0].program, "echo");
    assert_eq!(line.items[1].pipeline.commands[0].program, "echo");
}

#[test]
fn parser_or_connector_splits_items() {
    let line = crate::parse("false || echo fallback");
    assert_eq!(line.items.len(), 2);
    assert_eq!(line.items[0].connector, Some(crate::parser::Connector::Or));
    assert_eq!(line.items[0].pipeline.commands[0].program, "false");
    assert_eq!(line.items[1].pipeline.commands[0].program, "echo");
}

#[test]
fn parser_dollar_paren_command_substitution_in_word() {
    let line = crate::parse("echo $(whoami)");
    let cmd = &line.items[0].pipeline.commands[0];
    assert_eq!(cmd.program, "echo");
    assert_eq!(cmd.args, vec!["$(whoami)"]);
}

#[test]
fn parser_dollar_var_in_word() {
    let line = crate::parse("echo $HOME");
    let cmd = &line.items[0].pipeline.commands[0];
    assert_eq!(cmd.args, vec!["$HOME"]);
}

#[test]
fn parser_brace_var_in_word() {
    let line = crate::parse("echo ${HOME}");
    let cmd = &line.items[0].pipeline.commands[0];
    assert_eq!(cmd.args, vec!["${HOME}"]);
}

// ---------------------------------------------------------------------------
// Backslash-newline line continuation
// ---------------------------------------------------------------------------

#[test]
fn backslash_newline_continues_command() {
    let line = crate::parse("echo hello \\\nworld");
    assert_eq!(line.items.len(), 1);
    let cmd = &line.items[0].pipeline.commands[0];
    assert_eq!(cmd.program, "echo");
    assert_eq!(cmd.args, vec!["hello", "world"]);
}

#[test]
fn curl_multiline_with_continuation() {
    let input = "curl -s -X POST https://httpbin.org/post \\\n  -H \"Content-Type: application/json\" \\\n  -d '{\"key\":\"value\"}'";
    let line = crate::parse(input);
    assert_eq!(line.items.len(), 1);
    let cmd = &line.items[0].pipeline.commands[0];
    assert_eq!(cmd.program, "curl");
    assert_eq!(
        cmd.args,
        vec![
            "-s",
            "-X",
            "POST",
            "https://httpbin.org/post",
            "-H",
            "Content-Type: application/json",
            "-d",
            "{\"key\":\"value\"}",
        ]
    );
}

#[test]
fn backslash_at_end_without_newline_is_literal() {
    let line = crate::parse("echo hello\\");
    assert_eq!(line.items.len(), 1);
    let cmd = &line.items[0].pipeline.commands[0];
    assert_eq!(cmd.program, "echo");
    assert_eq!(cmd.args, vec!["hello\\"]);
}

// ---------------------------------------------------------------------------
// SH6: Pipeline parent-span lineage (strengthened)
// ---------------------------------------------------------------------------

#[test]
fn pipeline_child_spans_have_shell_pipeline_as_parent() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let (_, spans) = capture_spans(|| run_shell(vfs, HashMap::new(), "echo hello | grep hello"));

    let command_spans = shell_command_spans(&spans);
    assert_eq!(command_spans.len(), 2, "expected 2 shell_command spans");

    // Verify the parent is specifically "shell_pipeline" (not just any span)
    for span in &command_spans {
        assert_eq!(
            span.parent.as_deref(),
            Some("shell_pipeline"),
            "each pipeline stage span must have 'shell_pipeline' as its parent, got {:?}",
            span.parent
        );
    }

    // Verify the shell_pipeline span itself exists in the captured spans
    let pipeline_spans: Vec<_> = spans
        .iter()
        .filter(|s| s.name == "shell_pipeline")
        .collect();
    assert_eq!(
        pipeline_spans.len(),
        1,
        "expected exactly one shell_pipeline span"
    );
}
