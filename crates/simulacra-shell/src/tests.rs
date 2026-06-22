//! Red tests for `specs/S002-shell.md`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use simulacra_types::VirtualFs;
use simulacra_vfs::MemoryFs;
use tracing_subscriber::layer::SubscriberExt;

use crate::http_proxy::{ShellHttpError, ShellHttpProxy, ShellHttpResponse};
use crate::{CommandResult, ShellExecutor};

#[derive(Debug, Clone)]
struct CapturedSpan {
    name: String,
    fields: HashMap<String, String>,
    parent: Option<String>,
}

struct SpanCaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
}

impl<S> tracing_subscriber::Layer<S> for SpanCaptureLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        attrs.record(&mut visitor);

        let parent = attrs
            .parent()
            .and_then(|parent_id| ctx.span(parent_id))
            .map(|span| span.name().to_string())
            .or_else(|| {
                if attrs.is_contextual() {
                    ctx.current_span()
                        .id()
                        .and_then(|parent_id| ctx.span(parent_id))
                        .map(|span| span.name().to_string())
                } else {
                    None
                }
            });

        self.spans.lock().unwrap().push(CapturedSpan {
            name: attrs.metadata().name().to_string(),
            fields,
            parent,
        });
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if let Some(span_ref) = ctx.span(id) {
            let span_name = span_ref.name().to_string();
            let mut new_fields = HashMap::new();
            let mut visitor = FieldVisitor(&mut new_fields);
            values.record(&mut visitor);

            let mut spans = self.spans.lock().unwrap();
            for captured in spans.iter_mut().rev() {
                if captured.name == span_name {
                    for (key, value) in new_fields {
                        captured.fields.insert(key, value);
                    }
                    break;
                }
            }
        }
    }
}

struct FieldVisitor<'a>(&'a mut HashMap<String, String>);

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

fn run_shell(vfs: &dyn VirtualFs, env: HashMap<String, String>, input: &str) -> CommandResult {
    let mut shell = ShellExecutor::new(vfs, env, None);
    shell.run(input)
}

// Use a global subscriber to avoid callsite interest caching issues.
// When tracing spans are first hit without any subscriber, the callsite is
// cached as Interest::never and `with_default` cannot override it. A global
// subscriber ensures callsites are always registered as active.
static CAPTURED_SPANS: OnceLock<Arc<Mutex<Vec<CapturedSpan>>>> = OnceLock::new();
static CAPTURE_INSTALL: OnceLock<()> = OnceLock::new();
static TEST_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

fn capture_store() -> Arc<Mutex<Vec<CapturedSpan>>> {
    CAPTURE_INSTALL.get_or_init(|| {
        let spans = Arc::new(Mutex::new(Vec::new()));
        CAPTURED_SPANS
            .set(Arc::clone(&spans))
            .expect("capture store should only be initialized once");

        let subscriber =
            tracing_subscriber::registry::Registry::default().with(SpanCaptureLayer { spans });
        tracing::subscriber::set_global_default(subscriber)
            .expect("global tracing subscriber should install");
    });

    Arc::clone(
        CAPTURED_SPANS
            .get()
            .expect("capture store should be installed"),
    )
}

fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_MUTEX
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn capture_spans<T>(f: impl FnOnce() -> T) -> (T, Vec<CapturedSpan>) {
    let _guard = test_guard();
    let spans = capture_store();
    spans.lock().unwrap().clear();
    let result = f();
    let captured = spans.lock().unwrap().clone();
    (result, captured)
}

fn field_matches(span: &CapturedSpan, key: &str, expected: &str) -> bool {
    span.fields
        .get(key)
        .map(|value| value.trim_matches('"') == expected)
        .unwrap_or(false)
}

fn shell_command_spans(spans: &[CapturedSpan]) -> Vec<&CapturedSpan> {
    spans
        .iter()
        .filter(|span| field_matches(span, "simulacra.operation.name", "shell_command"))
        .collect()
}

#[test]
fn echo_hello_writes_stdout_and_returns_zero() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo hello");

    assert_eq!(result.stdout, "hello\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn echo_hello_pipe_grep_hello_returns_match_and_zero() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo hello | grep hello");

    assert_eq!(result.stdout, "hello\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn echo_hello_pipe_grep_world_returns_empty_stdout_and_one() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo hello | grep world");

    assert_eq!(result.stdout, "");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 1);
}

#[test]
fn redirect_truncate_then_cat_reads_back_written_contents() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(
        vfs,
        HashMap::new(),
        "echo hello > /file.txt && cat /file.txt",
    );

    assert_eq!(result.stdout, "hello\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
    assert_eq!(vfs.read("/file.txt").unwrap(), b"hello\n");
}

#[test]
fn redirect_append_accumulates_lines_in_order() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(
        vfs,
        HashMap::new(),
        "echo a >> /f.txt && echo b >> /f.txt && cat /f.txt",
    );

    assert_eq!(result.stdout, "a\nb\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
    assert_eq!(vfs.read("/f.txt").unwrap(), b"a\nb\n");
}

#[test]
fn ls_root_on_empty_vfs_lists_nothing_and_returns_zero() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "ls /");

    assert_eq!(result.stdout, "");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn ls_root_after_creating_files_lists_them_sorted() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    run_shell(vfs, HashMap::new(), "echo x > /c.txt");
    run_shell(vfs, HashMap::new(), "echo x > /a.txt");
    run_shell(vfs, HashMap::new(), "echo x > /b.txt");

    let result = run_shell(vfs, HashMap::new(), "ls /");

    assert_eq!(result.stdout, "a.txt\nb.txt\nc.txt\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn unknown_command_returns_127_and_command_not_found_stderr() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "nonexistent_cmd");

    assert_eq!(result.stdout, "");
    assert_eq!(result.exit_code, 127);
    assert!(
        result.stderr.contains("command not found: nonexistent_cmd"),
        "stderr should mention command-not-found, got {:?}",
        result.stderr
    );
}

#[test]
fn false_and_echo_yes_short_circuits_without_output() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "false && echo yes");

    assert_eq!(result.stdout, "");
    assert_eq!(result.exit_code, 1);
}

#[test]
fn false_or_echo_fallback_executes_right_hand_side() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "false || echo fallback");

    assert_eq!(result.stdout, "fallback\n");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn shell_command_execution_emits_span_with_command_and_exit_code() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let (_, spans) = capture_spans(|| run_shell(vfs, HashMap::new(), "echo hello"));

    let span = spans
        .iter()
        .find(|span| field_matches(span, "simulacra.operation.name", "shell_command"))
        .unwrap_or_else(|| panic!("expected shell_command span, got {spans:#?}"));

    assert!(field_matches(span, "simulacra.shell.command", "echo"));
    assert!(field_matches(span, "simulacra.shell.argc", "1"));
    assert!(field_matches(span, "simulacra.shell.exit_code", "0"));
}

#[test]
fn simulacra_shell_commands_counter_increments_per_command_execution() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let (_, spans) = capture_spans(|| run_shell(vfs, HashMap::new(), "echo left && echo right"));

    let command_spans = shell_command_spans(&spans);

    assert_eq!(
        command_spans.len(),
        2,
        "expected one shell_command emission per executed command so simulacra.shell.commands can increment per execution; got {spans:#?}"
    );
    assert!(command_spans.iter().all(|span| field_matches(
        span,
        "simulacra.shell.command",
        "echo"
    )));
    assert!(
        command_spans
            .iter()
            .all(|span| field_matches(span, "simulacra.shell.argc", "1"))
    );
}

#[test]
fn pipe_chains_emit_parent_span_with_child_stage_spans() {
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let (_, spans) = capture_spans(|| run_shell(vfs, HashMap::new(), "echo hello | grep hello"));

    let command_spans = shell_command_spans(&spans);
    assert_eq!(
        command_spans.len(),
        2,
        "expected a shell_command span for each pipeline stage, got {spans:#?}"
    );

    let parent_name = command_spans[0]
        .parent
        .as_deref()
        .unwrap_or_else(|| panic!("expected pipeline parent span, got {spans:#?}"));

    assert!(
        spans.iter().any(|span| span.name == parent_name),
        "expected captured parent span named {parent_name}, got {spans:#?}"
    );

    for span in command_spans {
        assert_eq!(
            span.parent.as_deref(),
            Some(parent_name),
            "all pipeline stage spans should share the same parent; got {spans:#?}"
        );
    }
}

// ---------------------------------------------------------------------------
// SH1: ${VAR} brace-style expansion
// ---------------------------------------------------------------------------

#[test]
fn brace_style_variable_expansion_replaces_with_env_value() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let mut env = HashMap::new();
    env.insert("HOME".to_string(), "/home/simulacra".to_string());

    let result = run_shell(vfs, env, "echo ${HOME}");

    assert_eq!(result.stdout, "/home/simulacra\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn brace_style_expansion_adjacent_to_text() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let mut env = HashMap::new();
    env.insert("USER".to_string(), "simulacra".to_string());

    let result = run_shell(vfs, env, "echo ${USER}_home");

    assert_eq!(result.stdout, "simulacra_home\n");
    assert_eq!(result.exit_code, 0);
}

// ---------------------------------------------------------------------------
// SH4: Redirect failure paths
// ---------------------------------------------------------------------------

#[test]
fn redirect_to_root_directory_reports_error() {
    // Redirecting to "/" should fail because "/" is a directory, not a file.
    // The executor must report the error: non-zero exit code, stderr message,
    // and stdout preserved (not cleared on failure).
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo hello > /");

    // stdout is preserved because the redirect failed
    assert_eq!(result.stdout, "hello\n");
    // Exit code reflects the redirect failure
    assert_ne!(result.exit_code, 0);
    // stderr contains the redirect error
    assert!(
        result.stderr.contains("redirect"),
        "expected redirect error in stderr, got: {}",
        result.stderr
    );
}

// ---------------------------------------------------------------------------
// SH5: Parser edge case tests
// ---------------------------------------------------------------------------

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
    assert_eq!(cmd.redirects[0].kind, crate::RedirectKind::Truncate);
    assert_eq!(cmd.redirects[0].target, "/out.txt");
}

#[test]
fn parser_redirect_append() {
    let line = crate::parse("echo hello >> /out.txt");
    let cmd = &line.items[0].pipeline.commands[0];
    assert_eq!(cmd.redirects.len(), 1);
    assert_eq!(cmd.redirects[0].kind, crate::RedirectKind::Append);
    assert_eq!(cmd.redirects[0].target, "/out.txt");
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

// ---------------------------------------------------------------------------
// SH7: wc test with exact assertion (replaces loose starts_with)
// ---------------------------------------------------------------------------

#[test]
fn wc_counts_lines_exact() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/f.txt", b"a\nb\nc\n").unwrap();
    let result = run_shell(vfs, HashMap::new(), "wc -l /f.txt");

    assert_eq!(
        result.stdout.trim(),
        "3",
        "wc -l should report exactly '3', got {:?}",
        result.stdout
    );
    assert_eq!(result.exit_code, 0);
}

// ---------------------------------------------------------------------------
// SH8: find test with exact path assertions
// ---------------------------------------------------------------------------

#[test]
fn find_lists_files_with_full_paths() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/d/a.txt", b"a").unwrap();
    vfs.write("/d/sub/b.txt", b"b").unwrap();

    let result = run_shell(vfs, HashMap::new(), "find /d");

    let lines: Vec<&str> = result.stdout.trim().lines().collect();
    // find should list the directory itself and all files/subdirs
    assert!(
        lines.contains(&"/d"),
        "find output should include the search root '/d', got {:?}",
        lines
    );
    assert!(
        lines.contains(&"/d/a.txt"),
        "find output should include '/d/a.txt', got {:?}",
        lines
    );
    assert!(
        lines.contains(&"/d/sub"),
        "find output should include '/d/sub', got {:?}",
        lines
    );
    assert!(
        lines.contains(&"/d/sub/b.txt"),
        "find output should include '/d/sub/b.txt', got {:?}",
        lines
    );
    assert_eq!(result.exit_code, 0);
}

// ---------------------------------------------------------------------------
// GS3: Escaped quotes (implementation bug)
// ---------------------------------------------------------------------------

#[test]
fn parser_escaped_double_quote_inside_double_quotes() {
    // In a POSIX shell: echo "hello \"world\"" should produce: hello "world"
    let line = crate::parse(r#"echo "hello \"world\"""#);
    let cmd = &line.items[0].pipeline.commands[0];
    assert_eq!(cmd.program, "echo");
    assert_eq!(cmd.args, vec!["hello \"world\""]);
}

#[test]
fn executor_escaped_quotes_in_echo() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), r#"echo "hello \"world\"""#);

    assert_eq!(result.stdout, "hello \"world\"\n");
    assert_eq!(result.exit_code, 0);
}

// ---------------------------------------------------------------------------
// GS4: Single quotes suppress variable expansion (implementation bug)
// ---------------------------------------------------------------------------

#[test]
fn single_quotes_suppress_variable_expansion() {
    // In a POSIX shell: echo '$HOME' should produce the literal string $HOME
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let mut env = HashMap::new();
    env.insert("HOME".to_string(), "/home/simulacra".to_string());

    let result = run_shell(vfs, env, "echo '$HOME'");

    assert_eq!(
        result.stdout, "$HOME\n",
        "single-quoted $HOME should be literal, not expanded"
    );
    assert_eq!(result.exit_code, 0);
}

// ---------------------------------------------------------------------------
// GS5: Pipeline stderr from intermediate stages (implementation bug)
// ---------------------------------------------------------------------------

#[test]
fn pipeline_preserves_intermediate_stderr() {
    // When a command in the middle of a pipeline writes to stderr,
    // the final result should include that stderr (or at least not silently discard it).
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    // nonexistent_cmd produces stderr ("command not found") with exit 127
    // piped into echo which succeeds — but the stderr from stage 1 should be preserved
    let result = run_shell(vfs, HashMap::new(), "nonexistent_cmd | echo ok");

    assert!(
        !result.stderr.is_empty(),
        "stderr from earlier pipeline stage should not be silently discarded"
    );
}

// ---------------------------------------------------------------------------
// S002 gap-fill: variable expansion
// ---------------------------------------------------------------------------

#[test]
fn dollar_var_expansion_replaces_with_env_value() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let mut env = HashMap::new();
    env.insert("GREETING".to_string(), "hi there".to_string());

    let result = run_shell(vfs, env, "echo $GREETING");

    assert_eq!(result.stdout, "hi there\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn undefined_variable_expands_to_empty_string() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo $UNDEFINED_VAR");

    assert_eq!(result.stdout, "\n");
    assert_eq!(result.exit_code, 0);
}

// ---------------------------------------------------------------------------
// S002 gap-fill: command substitution
// ---------------------------------------------------------------------------

#[test]
fn command_substitution_captures_stdout() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo $(echo inner)");

    assert_eq!(result.stdout, "inner\n");
    assert_eq!(result.exit_code, 0);
}

// ---------------------------------------------------------------------------
// S002 gap-fill: builtins (mkdir, cp, mv, rm, head, tail, sed, wc, find,
//                          sort, uniq, cut, tr, tee)
// ---------------------------------------------------------------------------

#[test]
fn mkdir_creates_directory_that_ls_can_list() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    run_shell(vfs, HashMap::new(), "mkdir /mydir");
    let result = run_shell(vfs, HashMap::new(), "echo x > /mydir/f.txt && ls /mydir");

    assert_eq!(result.stdout, "f.txt\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn cp_copies_file_content() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    run_shell(vfs, HashMap::new(), "echo hello > /a.txt");
    run_shell(vfs, HashMap::new(), "cp /a.txt /b.txt");
    let result = run_shell(vfs, HashMap::new(), "cat /b.txt");

    assert_eq!(result.stdout, "hello\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn mv_moves_file_so_original_is_gone() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    run_shell(vfs, HashMap::new(), "echo data > /src.txt");
    run_shell(vfs, HashMap::new(), "mv /src.txt /dst.txt");

    let cat_dst = run_shell(vfs, HashMap::new(), "cat /dst.txt");
    assert_eq!(cat_dst.stdout, "data\n");

    let cat_src = run_shell(vfs, HashMap::new(), "cat /src.txt");
    assert_ne!(cat_src.exit_code, 0);
}

#[test]
fn rm_removes_file() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    run_shell(vfs, HashMap::new(), "echo x > /del.txt");
    run_shell(vfs, HashMap::new(), "rm /del.txt");

    let result = run_shell(vfs, HashMap::new(), "cat /del.txt");
    assert_ne!(result.exit_code, 0);
}

#[test]
fn head_returns_first_lines() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/lines.txt", b"a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n")
        .unwrap();
    let result = run_shell(vfs, HashMap::new(), "head -n 3 /lines.txt");

    assert_eq!(result.stdout, "a\nb\nc\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn tail_returns_last_lines() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/lines.txt", b"a\nb\nc\nd\ne\n").unwrap();
    let result = run_shell(vfs, HashMap::new(), "tail -n 2 /lines.txt");

    assert_eq!(result.stdout, "d\ne\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn sed_substitution_replaces_text() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/f.txt", b"hello world\n").unwrap();
    let result = run_shell(vfs, HashMap::new(), "cat /f.txt | sed s/hello/goodbye/");

    assert_eq!(result.stdout, "goodbye world\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn wc_counts_lines() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/f.txt", b"a\nb\nc\n").unwrap();
    let result = run_shell(vfs, HashMap::new(), "wc -l /f.txt");

    assert!(
        result.stdout.trim().starts_with("3"),
        "wc -l should report 3 lines, got {:?}",
        result.stdout
    );
    assert_eq!(result.exit_code, 0);
}

#[test]
fn find_lists_files_recursively() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/d/a.txt", b"a").unwrap();
    vfs.write("/d/sub/b.txt", b"b").unwrap();

    let result = run_shell(vfs, HashMap::new(), "find /d");

    assert!(result.stdout.contains("a.txt"), "find should list a.txt");
    assert!(result.stdout.contains("b.txt"), "find should list b.txt");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn sort_orders_lines_alphabetically() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/f.txt", b"cherry\napple\nbanana\n").unwrap();
    let result = run_shell(vfs, HashMap::new(), "cat /f.txt | sort");

    assert_eq!(result.stdout, "apple\nbanana\ncherry\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn uniq_removes_adjacent_duplicates() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/f.txt", b"a\na\nb\nb\nb\nc\n").unwrap();
    let result = run_shell(vfs, HashMap::new(), "cat /f.txt | uniq");

    assert_eq!(result.stdout, "a\nb\nc\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn cut_extracts_fields() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    vfs.write("/f.txt", b"a:b:c\nx:y:z\n").unwrap();
    let result = run_shell(vfs, HashMap::new(), "cat /f.txt | cut -d : -f 2");

    assert_eq!(result.stdout, "b\ny\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn tr_translates_characters() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo hello | tr l r");

    assert_eq!(result.stdout, "herro\n");
    assert_eq!(result.exit_code, 0);
}

#[test]
fn tee_writes_to_file_and_stdout() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo hello | tee /out.txt");

    assert_eq!(result.stdout, "hello\n");
    assert_eq!(vfs.read("/out.txt").unwrap(), b"hello\n");
    assert_eq!(result.exit_code, 0);
}

// ---------------------------------------------------------------------------
// S002 gap-fill: pipe exit code from rightmost command
// ---------------------------------------------------------------------------

#[test]
fn pipe_exit_code_comes_from_rightmost_command() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    // Left side fails (false = exit 1), right side succeeds (echo = exit 0)
    // Pipe exit code should be 0 (from rightmost)
    let result = run_shell(vfs, HashMap::new(), "false | echo ok");

    assert_eq!(result.exit_code, 0);
}

// ---------------------------------------------------------------------------
// S002 gap-fill: VFS isolation (shell never touches real FS)
// ---------------------------------------------------------------------------

#[test]
fn shell_commands_never_touch_real_filesystem() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let sentinel = "/tmp/simulacra_shell_test_sentinel_should_not_exist.txt";

    // Remove sentinel if it somehow exists
    let _ = std::fs::remove_file(sentinel);

    // Write to the sentinel path through the shell
    run_shell(vfs, HashMap::new(), &format!("echo leaked > {sentinel}"));

    // The real filesystem should NOT have this file
    assert!(
        !std::path::Path::new(sentinel).exists(),
        "shell echo > path should write to VFS, not real filesystem"
    );

    // But VFS should have it
    assert!(vfs.read(sentinel).is_ok(), "VFS should have the file");
}

// ---------------------------------------------------------------------------
// Mock HTTP proxy and curl tests
// ---------------------------------------------------------------------------

/// A mock HTTP proxy that returns a preconfigured response or error.
struct MockShellHttpProxy {
    response: Mutex<Option<Result<ShellHttpResponse, MockHttpError>>>,
    /// Captures the last request for assertion.
    last_request: Mutex<Option<CapturedRequest>>,
}

#[derive(Debug, Clone)]
struct CapturedRequest {
    url: String,
    method: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    timeout_ms: Option<u64>,
}

/// We cannot store `ShellHttpError` directly because it does not implement Clone.
/// Instead we store a variant tag and re-create the error on demand.
#[derive(Debug)]
enum MockHttpError {
    CapabilityDenied(String),
    BudgetExhausted(String),
    NetworkError(String),
    Timeout,
}

impl MockShellHttpProxy {
    fn with_response(status: u16, status_text: &str, body: &str) -> Self {
        Self {
            response: Mutex::new(Some(Ok(ShellHttpResponse {
                status,
                status_text: status_text.to_string(),
                headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
                body: body.as_bytes().to_vec(),
                url: String::new(),
            }))),
            last_request: Mutex::new(None),
        }
    }

    fn with_response_headers(
        status: u16,
        status_text: &str,
        headers: Vec<(String, String)>,
        body: &str,
    ) -> Self {
        Self {
            response: Mutex::new(Some(Ok(ShellHttpResponse {
                status,
                status_text: status_text.to_string(),
                headers,
                body: body.as_bytes().to_vec(),
                url: String::new(),
            }))),
            last_request: Mutex::new(None),
        }
    }

    fn with_error(err: MockHttpError) -> Self {
        Self {
            response: Mutex::new(Some(Err(err))),
            last_request: Mutex::new(None),
        }
    }

    fn last_request(&self) -> CapturedRequest {
        self.last_request
            .lock()
            .unwrap()
            .clone()
            .expect("no request was captured")
    }
}

impl ShellHttpProxy for MockShellHttpProxy {
    fn execute(
        &self,
        url: &str,
        method: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        timeout_ms: Option<u64>,
    ) -> Result<ShellHttpResponse, ShellHttpError> {
        *self.last_request.lock().unwrap() = Some(CapturedRequest {
            url: url.to_string(),
            method: method.to_string(),
            headers: headers.to_vec(),
            body: body.map(|b| b.to_vec()),
            timeout_ms,
        });

        match self.response.lock().unwrap().take() {
            Some(Ok(mut resp)) => {
                resp.url = url.to_string();
                Ok(resp)
            }
            Some(Err(e)) => match e {
                MockHttpError::CapabilityDenied(msg) => Err(ShellHttpError::CapabilityDenied(msg)),
                MockHttpError::BudgetExhausted(msg) => Err(ShellHttpError::BudgetExhausted(msg)),
                MockHttpError::NetworkError(msg) => Err(ShellHttpError::NetworkError(msg)),
                MockHttpError::Timeout => Err(ShellHttpError::Timeout),
            },
            None => panic!("MockShellHttpProxy: response already consumed"),
        }
    }
}

fn run_shell_with_http(
    vfs: &dyn VirtualFs,
    env: HashMap<String, String>,
    http_proxy: &dyn ShellHttpProxy,
    input: &str,
) -> CommandResult {
    let mut shell = ShellExecutor::new(vfs, env, Some(http_proxy));
    shell.run(input)
}

// ---------------------------------------------------------------------------
// Curl tests
// ---------------------------------------------------------------------------

#[test]
fn curl_get_returns_body_in_stdout() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "hello world");

    let result = run_shell_with_http(vfs, HashMap::new(), &proxy, "curl http://example.com");

    assert_eq!(result.stdout, "hello world");
    assert_eq!(result.exit_code, 0);

    let req = proxy.last_request();
    assert_eq!(req.method, "GET");
    assert_eq!(req.url, "http://example.com");
}

#[test]
fn curl_post_with_data() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl -X POST -d 'body data' http://example.com/api",
    );

    assert_eq!(result.stdout, "ok");
    assert_eq!(result.exit_code, 0);

    let req = proxy.last_request();
    assert_eq!(req.method, "POST");
    assert_eq!(req.body.as_deref(), Some(b"body data".as_slice()));
}

#[test]
fn curl_custom_header() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl -H 'X-Custom: myval' http://example.com",
    );

    assert_eq!(result.exit_code, 0);

    let req = proxy.last_request();
    assert!(
        req.headers
            .iter()
            .any(|(k, v)| k == "X-Custom" && v == "myval"),
        "expected X-Custom header, got {:?}",
        req.headers
    );
}

#[test]
fn curl_multiple_headers() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl -H 'X-First: one' -H 'X-Second: two' http://example.com",
    );

    assert_eq!(result.exit_code, 0);

    let req = proxy.last_request();
    assert!(
        req.headers
            .iter()
            .any(|(k, v)| k == "X-First" && v == "one"),
        "expected X-First header, got {:?}",
        req.headers
    );
    assert!(
        req.headers
            .iter()
            .any(|(k, v)| k == "X-Second" && v == "two"),
        "expected X-Second header, got {:?}",
        req.headers
    );
}

#[test]
fn curl_json_shorthand() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "{\"ok\":true}");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        r#"curl --json '{"a":1}' http://example.com/api"#,
    );

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "{\"ok\":true}");

    let req = proxy.last_request();
    assert_eq!(req.method, "POST", "--json should imply POST");
    assert_eq!(
        req.body.as_deref(),
        Some(b"{\"a\":1}".as_slice()),
        "--json body mismatch"
    );
    assert!(
        req.headers
            .iter()
            .any(|(k, v)| k == "Content-Type" && v == "application/json"),
        "expected Content-Type: application/json, got {:?}",
        req.headers
    );
    assert!(
        req.headers
            .iter()
            .any(|(k, v)| k == "Accept" && v == "application/json"),
        "expected Accept: application/json, got {:?}",
        req.headers
    );
}

#[test]
fn curl_output_to_file() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "file content here");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl -o /workspace/out.txt http://example.com/data",
    );

    assert_eq!(result.exit_code, 0);
    // stdout should be empty when -o is used
    assert_eq!(result.stdout, "");
    // File should contain body
    let written = vfs.read("/workspace/out.txt").unwrap();
    assert_eq!(written, b"file content here");
    // stderr should contain transfer summary
    assert!(
        result.stderr.contains("% Total"),
        "expected transfer summary in stderr, got {:?}",
        result.stderr
    );
}

#[test]
fn curl_silent() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "body");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl -s -o /workspace/out.txt http://example.com",
    );

    assert_eq!(result.exit_code, 0);
    // stderr should be empty with -s
    assert_eq!(result.stderr, "", "silent mode should suppress stderr");
    // File should still be written
    assert_eq!(vfs.read("/workspace/out.txt").unwrap(), b"body");
}

#[test]
fn curl_include_headers() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response_headers(
        200,
        "OK",
        vec![("Content-Type".to_string(), "text/html".to_string())],
        "body",
    );

    let result = run_shell_with_http(vfs, HashMap::new(), &proxy, "curl -i http://example.com");

    assert_eq!(result.exit_code, 0);
    assert!(
        result.stdout.starts_with("HTTP/1.1 200 OK\r\n"),
        "expected HTTP status line, got {:?}",
        result.stdout
    );
    assert!(
        result.stdout.contains("Content-Type: text/html\r\n"),
        "expected Content-Type header, got {:?}",
        result.stdout
    );
    assert!(
        result.stdout.contains("\r\n\r\nbody"),
        "expected blank line then body, got {:?}",
        result.stdout
    );
}

#[test]
fn curl_fail_on_error() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(404, "Not Found", "nope");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl -f http://example.com/missing",
    );

    assert_eq!(result.exit_code, 1);
    assert_eq!(result.stdout, "", "-f should suppress body on error");
    assert!(
        result
            .stderr
            .contains("The requested URL returned error: 404 Not Found"),
        "expected error message, got {:?}",
        result.stderr
    );
}

#[test]
fn curl_verbose() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response_headers(
        200,
        "OK",
        vec![("X-Resp".to_string(), "val".to_string())],
        "body",
    );

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl -v http://example.com/path",
    );

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "body");

    // Request line
    assert!(
        result.stderr.contains("> GET /path HTTP/1.1"),
        "expected request line in stderr, got {:?}",
        result.stderr
    );
    // Host header
    assert!(
        result.stderr.contains("> Host: example.com"),
        "expected Host in stderr, got {:?}",
        result.stderr
    );
    // Response status
    assert!(
        result.stderr.contains("< HTTP/1.1 200 OK"),
        "expected response status in stderr, got {:?}",
        result.stderr
    );
    // Response header
    assert!(
        result.stderr.contains("< X-Resp: val"),
        "expected response header in stderr, got {:?}",
        result.stderr
    );
}

#[test]
fn curl_connect_timeout() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl --connect-timeout 2 http://example.com",
    );

    assert_eq!(result.exit_code, 0);

    let req = proxy.last_request();
    assert_eq!(req.timeout_ms, Some(2000), "2 seconds = 2000ms");
}

#[test]
fn curl_data_implies_post() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl -d 'payload' http://example.com",
    );

    assert_eq!(result.exit_code, 0);

    let req = proxy.last_request();
    assert_eq!(req.method, "POST", "-d without -X should default to POST");
}

#[test]
fn curl_unsupported_flag() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl --compressed http://example.com",
    );

    assert_eq!(result.exit_code, 1);
    assert!(
        result.stderr.contains("unsupported option '--compressed'"),
        "expected unsupported option message, got {:?}",
        result.stderr
    );
    assert!(
        result.stderr.contains("Supported:"),
        "error should list supported flags, got {:?}",
        result.stderr
    );
}

#[test]
fn curl_capability_denied() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy =
        MockShellHttpProxy::with_error(MockHttpError::CapabilityDenied("no http allowed".into()));

    let result = run_shell_with_http(vfs, HashMap::new(), &proxy, "curl http://example.com");

    assert_eq!(result.exit_code, 1);
    assert!(
        result
            .stderr
            .contains("curl: capability denied: no http allowed"),
        "got {:?}",
        result.stderr
    );
}

#[test]
fn curl_budget_exhausted() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy =
        MockShellHttpProxy::with_error(MockHttpError::BudgetExhausted("out of tokens".into()));

    let result = run_shell_with_http(vfs, HashMap::new(), &proxy, "curl http://example.com");

    assert_eq!(result.exit_code, 1);
    assert!(
        result
            .stderr
            .contains("curl: budget exhausted: out of tokens"),
        "got {:?}",
        result.stderr
    );
}

#[test]
fn curl_network_error() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy =
        MockShellHttpProxy::with_error(MockHttpError::NetworkError("connection refused".into()));

    let result = run_shell_with_http(vfs, HashMap::new(), &proxy, "curl http://example.com");

    assert_eq!(result.exit_code, 1);
    assert!(
        result
            .stderr
            .contains("curl: network error: connection refused"),
        "got {:?}",
        result.stderr
    );
}

#[test]
fn curl_timeout_error() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_error(MockHttpError::Timeout);

    let result = run_shell_with_http(vfs, HashMap::new(), &proxy, "curl http://example.com");

    assert_eq!(result.exit_code, 1);
    assert!(
        result.stderr.contains("curl: operation timed out"),
        "got {:?}",
        result.stderr
    );
}

#[test]
fn curl_no_proxy() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    // Use run_shell (no proxy) — should fail with no-proxy message
    let result = run_shell(vfs, HashMap::new(), "curl http://example.com");

    assert_eq!(result.exit_code, 1);
    assert!(
        result.stderr.contains("HTTP proxy"),
        "expected HTTP proxy error, got {:?}",
        result.stderr
    );
}

#[test]
fn curl_http_error_without_fail() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(404, "Not Found", "not found page");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl http://example.com/missing",
    );

    // Without -f, 404 should still be exit 0 and body in stdout
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "not found page");
}

#[test]
fn curl_data_raw_sends_body() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl --data-raw 'raw body' http://example.com",
    );

    assert_eq!(result.exit_code, 0);

    let req = proxy.last_request();
    assert_eq!(req.method, "POST", "--data-raw should imply POST");
    assert_eq!(req.body.as_deref(), Some(b"raw body".as_slice()),);
}

#[test]
fn curl_request_long_form() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl --request PUT http://example.com/resource",
    );

    assert_eq!(result.exit_code, 0);

    let req = proxy.last_request();
    assert_eq!(req.method, "PUT");
}

#[test]
fn curl_location_flag_accepted() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "curl -L http://example.com/redirect",
    );

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "ok");
}

#[test]
fn curl_output_vfs_write_error() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "body");

    // Writing to "/" always fails with NotAFile in MemoryFs
    let result = run_shell_with_http(vfs, HashMap::new(), &proxy, "curl -o / http://example.com");

    assert_eq!(result.exit_code, 1);
    assert!(
        result.stderr.contains("curl: /:"),
        "expected VFS write error, got {:?}",
        result.stderr
    );
}

// ---------------------------------------------------------------------------
// Wget tests
// ---------------------------------------------------------------------------

#[test]
fn wget_saves_to_vfs_file() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "csv,data,here");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget http://example.com/data.csv",
    );

    assert_eq!(result.exit_code, 0);
    // Body should be saved to file, not stdout
    assert_eq!(result.stdout, "");
    let saved = vfs.read("data.csv").expect("file should exist in VFS");
    assert_eq!(String::from_utf8(saved).unwrap(), "csv,data,here");
}

#[test]
fn wget_default_filename_index_html() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "<html></html>");

    let result = run_shell_with_http(vfs, HashMap::new(), &proxy, "wget http://example.com/");

    assert_eq!(result.exit_code, 0);
    let saved = vfs.read("index.html").expect("index.html should exist");
    assert_eq!(String::from_utf8(saved).unwrap(), "<html></html>");
}

#[test]
fn wget_output_document() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "content");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget -O /workspace/out.txt http://example.com/page",
    );

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "");
    let saved = vfs
        .read("/workspace/out.txt")
        .expect("output file should exist");
    assert_eq!(String::from_utf8(saved).unwrap(), "content");
}

#[test]
fn wget_stdout_mode() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "body to stdout");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget -O - http://example.com/page",
    );

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "body to stdout");
    // No "Saving to" in stderr
    assert!(
        !result.stderr.contains("Saving to"),
        "stdout mode should not print 'Saving to', got: {:?}",
        result.stderr
    );
}

#[test]
fn wget_quiet() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "quiet body");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget -q http://example.com/data.txt",
    );

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stderr, "");
}

#[test]
fn wget_custom_header() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget --header=X-Custom:val http://example.com/page",
    );

    assert_eq!(result.exit_code, 0);

    let req = proxy.last_request();
    assert!(
        req.headers
            .contains(&("X-Custom".to_string(), "val".to_string())),
        "expected X-Custom header, got {:?}",
        req.headers
    );
}

#[test]
fn wget_post_data() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget --post-data=hello http://example.com/api",
    );

    assert_eq!(result.exit_code, 0);

    let req = proxy.last_request();
    assert_eq!(req.method, "POST");
    assert_eq!(req.body, Some(b"hello".to_vec()));
}

#[test]
fn wget_method_override() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget --method=PUT http://example.com/resource",
    );

    assert_eq!(result.exit_code, 0);

    let req = proxy.last_request();
    assert_eq!(req.method, "PUT");
}

#[test]
fn wget_timeout() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget --timeout=3 http://example.com/slow",
    );

    assert_eq!(result.exit_code, 0);

    let req = proxy.last_request();
    assert_eq!(req.timeout_ms, Some(3000));
}

#[test]
fn wget_unsupported_flag() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "ok");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget --spider http://example.com/",
    );

    assert_eq!(result.exit_code, 1);
    assert!(
        result.stderr.contains("unsupported option '--spider'"),
        "expected unsupported option error, got {:?}",
        result.stderr
    );
    assert!(
        result.stderr.contains("Supported:"),
        "expected supported flags list, got {:?}",
        result.stderr
    );
}

#[test]
fn wget_capability_denied() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy =
        MockShellHttpProxy::with_error(MockHttpError::CapabilityDenied("no http allowed".into()));

    let result = run_shell_with_http(vfs, HashMap::new(), &proxy, "wget http://example.com/");

    assert_eq!(result.exit_code, 1);
    assert!(
        result.stderr.contains("wget: capability denied"),
        "expected capability denied error, got {:?}",
        result.stderr
    );
}

#[test]
fn wget_no_proxy() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "wget http://example.com/");

    assert_eq!(result.exit_code, 1);
    assert!(
        result
            .stderr
            .contains("network commands require HTTP proxy"),
        "expected no-proxy error, got {:?}",
        result.stderr
    );
}

#[test]
fn wget_overwrite_existing() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    // Pre-populate a file
    vfs.write("data.csv", b"old content")
        .expect("pre-populate should succeed");

    let proxy = MockShellHttpProxy::with_response(200, "OK", "new content");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget http://example.com/data.csv",
    );

    assert_eq!(result.exit_code, 0);
    let saved = vfs.read("data.csv").expect("file should exist");
    assert_eq!(
        String::from_utf8(saved).unwrap(),
        "new content",
        "wget should overwrite existing file"
    );
}

#[test]
fn wget_vfs_write_error() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "body");

    // Writing to "/" always fails with MemoryFs
    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget -O / http://example.com/page",
    );

    assert_eq!(result.exit_code, 1);
    assert!(
        result.stderr.contains("wget: /:"),
        "expected VFS write error, got {:?}",
        result.stderr
    );
}

#[test]
fn wget_default_progress_output() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "page content");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget http://example.com/page.html",
    );

    assert_eq!(result.exit_code, 0);
    assert!(
        result.stderr.contains("Resolving"),
        "expected 'Resolving' in stderr, got {:?}",
        result.stderr
    );
    assert!(
        result.stderr.contains("HTTP request sent"),
        "expected 'HTTP request sent' in stderr, got {:?}",
        result.stderr
    );
    assert!(
        result.stderr.contains("Saving to"),
        "expected 'Saving to' in stderr, got {:?}",
        result.stderr
    );
}

#[test]
fn wget_no_check_certificate_accepted() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let proxy = MockShellHttpProxy::with_response(200, "OK", "secure");

    let result = run_shell_with_http(
        vfs,
        HashMap::new(),
        &proxy,
        "wget --no-check-certificate http://example.com/page",
    );

    assert_eq!(result.exit_code, 0);
}

// ---------------------------------------------------------------------------
// papercut-1: POSIX ls flags, cd/pwd/env/which, robust pipeline parsing
// ---------------------------------------------------------------------------

#[test]
fn ls_accepts_dash_l_flag_without_error() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/tmp/a.txt", b"x").unwrap();
    vfs.write("/tmp/b.txt", b"y").unwrap();

    let result = run_shell(vfs, HashMap::new(), "ls -l /tmp");

    assert_eq!(
        result.exit_code, 0,
        "ls -l should succeed, got stderr={:?}",
        result.stderr
    );
    assert!(
        result.stdout.contains("a.txt") && result.stdout.contains("b.txt"),
        "ls -l /tmp must list a.txt and b.txt, got stdout={:?}",
        result.stdout
    );
    assert!(
        !result.stderr.contains("not found"),
        "stderr should not contain 'not found', got {:?}",
        result.stderr
    );
}

#[test]
fn ls_accepts_dash_a_flag_without_error() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/tmp/file", b"x").unwrap();

    let result = run_shell(vfs, HashMap::new(), "ls -a /tmp");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert!(result.stdout.contains("file"));
}

#[test]
fn ls_accepts_combined_la_flag_without_error() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/tmp/foo", b"x").unwrap();

    let result = run_shell(vfs, HashMap::new(), "ls -la /tmp");

    assert_eq!(
        result.exit_code, 0,
        "ls -la /tmp must succeed (no '/-la' path error), got stderr={:?}",
        result.stderr
    );
    assert!(
        result.stdout.contains("foo"),
        "ls -la /tmp must list foo, got stdout={:?}",
        result.stdout
    );
}

#[test]
fn ls_accepts_combined_al_flag_without_error() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/tmp/foo", b"x").unwrap();

    let result = run_shell(vfs, HashMap::new(), "ls -al /tmp");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert!(result.stdout.contains("foo"));
}

#[test]
fn ls_with_only_flag_lists_cwd() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/marker.txt", b"x").unwrap();

    // No path arg, only a flag: must default to cwd ('/'), not error.
    let result = run_shell(vfs, HashMap::new(), "ls -la");

    assert_eq!(
        result.exit_code, 0,
        "ls -la (no path) must succeed, got stderr={:?}",
        result.stderr
    );
    assert!(
        result.stdout.contains("marker.txt"),
        "ls -la must list root, got stdout={:?}",
        result.stdout
    );
}

#[test]
fn ls_with_unknown_flag_does_not_treat_flag_as_path() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/tmp/x", b"x").unwrap();

    // -h is not implemented; it must be ignored as a no-op flag, not parsed as
    // a path. The original bug surfaced as "ls: not found: /-h".
    let result = run_shell(vfs, HashMap::new(), "ls -h /tmp");

    assert!(
        !result.stderr.contains("/-h"),
        "ls must not interpret '-h' as a path, got stderr={:?}",
        result.stderr
    );
}

#[test]
fn pwd_prints_default_root_cwd() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "pwd");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, "/\n");
}

#[test]
fn cd_changes_cwd_and_pwd_reports_it() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.mkdir("/tmp").unwrap();

    // cd and pwd must share state through the same executor invocation.
    let result = run_shell(vfs, HashMap::new(), "cd /tmp && pwd");

    assert_eq!(
        result.exit_code, 0,
        "cd /tmp && pwd must succeed, got stderr={:?}",
        result.stderr
    );
    assert_eq!(
        result.stdout, "/tmp\n",
        "after cd /tmp, pwd must report /tmp, got {:?}",
        result.stdout
    );
}

#[test]
fn cd_to_nonexistent_directory_fails_and_does_not_change_cwd() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "cd /no-such-dir && pwd");

    assert_ne!(result.exit_code, 0, "cd to missing dir must fail");
    assert!(
        result.stderr.to_lowercase().contains("no such")
            || result.stderr.to_lowercase().contains("not"),
        "stderr should mention missing dir, got {:?}",
        result.stderr
    );
    // && short-circuit means pwd does not run, so stdout is empty.
    assert_eq!(result.stdout, "");
}

#[test]
fn cd_to_file_fails() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/file.txt", b"x").unwrap();

    let result = run_shell(vfs, HashMap::new(), "cd /file.txt");

    assert_ne!(result.exit_code, 0, "cd to a file must fail");
    assert!(
        !result.stderr.is_empty(),
        "cd to a file must emit stderr, got empty"
    );
}

#[test]
fn cd_with_relative_path_resolves_against_cwd() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.mkdir("/a").unwrap();
    vfs.mkdir("/a/b").unwrap();

    let result = run_shell(vfs, HashMap::new(), "cd /a && cd b && pwd");

    assert_eq!(
        result.exit_code, 0,
        "relative cd must succeed, got stderr={:?}",
        result.stderr
    );
    assert_eq!(result.stdout, "/a/b\n");
}

#[test]
fn cd_dotdot_walks_up_one_level() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.mkdir("/a").unwrap();
    vfs.mkdir("/a/b").unwrap();

    let result = run_shell(vfs, HashMap::new(), "cd /a/b && cd .. && pwd");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, "/a\n");
}

#[test]
fn cd_dot_slash_subdir_resolves() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.mkdir("/a").unwrap();
    vfs.mkdir("/a/sub").unwrap();

    let result = run_shell(vfs, HashMap::new(), "cd /a && cd ./sub && pwd");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, "/a/sub\n");
}

#[test]
fn cd_dotdot_at_root_stays_at_root() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "cd .. && pwd");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, "/\n");
}

#[test]
fn ls_after_cd_lists_relative_to_cwd() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/tmp/foo", b"x").unwrap();
    vfs.write("/tmp/bar", b"y").unwrap();

    let result = run_shell(vfs, HashMap::new(), "cd /tmp && ls");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert!(
        result.stdout.contains("foo") && result.stdout.contains("bar"),
        "ls in /tmp must list foo and bar, got stdout={:?}",
        result.stdout
    );
}

#[test]
fn env_with_no_args_prints_environment_variables() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    let mut env = HashMap::new();
    env.insert("FOO".to_string(), "bar".to_string());

    let result = run_shell(vfs, env, "env");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert!(
        result.stdout.contains("FOO=bar"),
        "env output must contain FOO=bar, got {:?}",
        result.stdout
    );
}

#[test]
fn env_with_empty_environment_returns_zero_and_no_stderr() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "env");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stderr, "");
}

#[test]
fn env_after_export_shows_new_variable() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "export GREETING=hello && env");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert!(
        result.stdout.contains("GREETING=hello"),
        "env after export must show GREETING=hello, got {:?}",
        result.stdout
    );
}

#[test]
fn which_resolves_known_builtin_to_its_name() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "which echo");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert!(
        result.stdout.contains("echo"),
        "which echo must mention 'echo' in stdout, got {:?}",
        result.stdout
    );
}

#[test]
fn which_resolves_pwd_builtin() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "which pwd");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert!(
        result.stdout.contains("pwd"),
        "which pwd must mention 'pwd' in stdout, got {:?}",
        result.stdout
    );
}

#[test]
fn which_unknown_command_returns_nonzero() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "which definitely_not_a_real_command");

    assert_ne!(
        result.exit_code, 0,
        "which on a missing command must return nonzero"
    );
}

#[test]
fn which_with_no_args_returns_nonzero() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "which");

    assert_ne!(
        result.exit_code, 0,
        "which with no args must return nonzero"
    );
}

// ---------------------------------------------------------------------------
// Pipeline / list semantics — verifying && / ; / || drive a list correctly
// ---------------------------------------------------------------------------

#[test]
fn and_runs_rhs_when_lhs_succeeds() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "true && echo ran");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "ran\n");
}

#[test]
fn and_does_not_run_rhs_when_lhs_fails() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "false && echo should-not-run");

    assert_ne!(result.exit_code, 0, "exit code must be nonzero on failure");
    assert!(
        !result.stdout.contains("should-not-run"),
        "rhs of && must not run when lhs fails, got stdout={:?}",
        result.stdout
    );
}

#[test]
fn and_long_chain_runs_all_when_each_succeeds() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo a && echo b && echo c");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, "a\nb\nc\n");
}

#[test]
fn and_long_chain_short_circuits_on_first_failure() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo a && false && echo c");

    assert_ne!(result.exit_code, 0, "must propagate failure");
    assert_eq!(
        result.stdout, "a\n",
        "echo c must not run after false; stdout={:?}",
        result.stdout
    );
    assert!(!result.stdout.contains("c"));
}

#[test]
fn semicolon_runs_both_unconditionally() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo a ; echo b");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(
        result.stdout, "a\nb\n",
        "both sides of ';' must run, got {:?}",
        result.stdout
    );
}

#[test]
fn semicolon_runs_rhs_even_when_lhs_fails() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "false ; echo after");

    // POSIX: ';' has no short-circuit; the final exit code is the rhs's.
    assert_eq!(
        result.exit_code, 0,
        "after ';' the exit code is the rhs's; stderr={:?}",
        result.stderr
    );
    assert!(
        result.stdout.contains("after"),
        "rhs of ';' must run when lhs fails, got stdout={:?}",
        result.stdout
    );
}

#[test]
fn semicolon_chain_runs_all_three() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo a ; echo b ; echo c");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, "a\nb\nc\n");
}

#[test]
fn parser_semicolon_splits_into_separate_items() {
    let line = crate::parse("echo a ; echo b");
    assert_eq!(
        line.items.len(),
        2,
        "';' must split into two items, got {:?}",
        line.items
    );
    assert_eq!(line.items[0].pipeline.commands[0].program, "echo");
    assert_eq!(line.items[1].pipeline.commands[0].program, "echo");
}

#[test]
fn mixed_semicolon_and_and_chain_executes_correctly() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    // false fails, so '&& echo skipped' is skipped; ';' separates,
    // so 'echo end' runs unconditionally.
    let result = run_shell(vfs, HashMap::new(), "false && echo skipped ; echo end");

    assert!(!result.stdout.contains("skipped"));
    assert!(result.stdout.contains("end"));
}

#[test]
fn echo_hello_and_pwd_runs_both_from_observed_failure() {
    // Mirrors the failing trace: `echo hello && pwd` blew up because
    // `pwd` was missing. With pwd implemented, both must run.
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), "echo hello && pwd");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, "hello\n/\n");
}

#[test]
fn cd_then_ls_from_observed_failure() {
    // Mirrors the failing trace: `cd /tmp && ls`.
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/tmp/marker", b"x").unwrap();

    let result = run_shell(vfs, HashMap::new(), "cd /tmp && ls");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert!(
        result.stdout.contains("marker"),
        "ls after cd /tmp must list marker, got stdout={:?}",
        result.stdout
    );
}

#[test]
fn cat_after_cd_resolves_relative_path_against_cwd() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.write("/workspace/package.json", br#"{"name":"demo"}"#)
        .unwrap();

    let result = run_shell(vfs, HashMap::new(), "cd /workspace && cat package.json");

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, r#"{"name":"demo"}"#);
}

#[test]
fn redirects_after_cd_write_relative_targets_under_cwd() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.mkdir("/workspace").unwrap();

    let result = run_shell(
        vfs,
        HashMap::new(),
        "cd /workspace && echo note > notes.txt",
    );

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(vfs.read("/workspace/notes.txt").unwrap(), b"note\n");
    assert!(
        vfs.read("/notes.txt").is_err(),
        "relative redirect must not write at VFS root"
    );
}

#[test]
fn touch_and_test_bracket_work_with_relative_paths() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;
    vfs.mkdir("/workspace").unwrap();

    let result = run_shell(
        vfs,
        HashMap::new(),
        "cd /workspace && mkdir -p src/lib && touch src/lib/mod.rs && [ -f src/lib/mod.rs ] && test -d src/lib",
    );

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(vfs.read("/workspace/src/lib/mod.rs").unwrap(), b"");
}

#[test]
fn printf_supports_common_string_newline_format() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(vfs, HashMap::new(), r#"printf '%s\n' hello"#);

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, "hello\n");
}

#[test]
fn basename_and_dirname_cover_common_path_splitting() {
    let _guard = test_guard();
    let fs = MemoryFs::new();
    let vfs: &dyn VirtualFs = &fs;

    let result = run_shell(
        vfs,
        HashMap::new(),
        "basename /workspace/src/lib.rs && dirname /workspace/src/lib.rs",
    );

    assert_eq!(result.exit_code, 0, "stderr={:?}", result.stderr);
    assert_eq!(result.stdout, "lib.rs\n/workspace/src\n");
}
