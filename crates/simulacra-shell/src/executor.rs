//! Shell executor — runs parsed shell lines against a [`VirtualFs`].

use std::collections::HashMap;

use simulacra_types::{VfsError, VirtualFs};
use tracing::{Span, field};

use crate::CommandResult;
use crate::ShellExternalCommand;
use crate::builtins;
use crate::http_proxy::ShellHttpProxy;
use crate::parser::{self, Connector, ShellLine};

/// Executes shell commands against a virtual filesystem.
pub struct ShellExecutor<'a> {
    vfs: &'a dyn VirtualFs,
    env: HashMap<String, String>,
    http_proxy: Option<&'a dyn ShellHttpProxy>,
    external: Option<&'a dyn ShellExternalCommand>,
    last_status: i32,
    /// Current working directory. Always normalized to an absolute path.
    /// `cd` updates it; `pwd` reads it; relative path arguments to `ls`
    /// (and other path-using builtins) are resolved against it.
    cwd: String,
}

impl<'a> ShellExecutor<'a> {
    /// Create a new executor with the given VFS, environment variables, and
    /// optional HTTP proxy for intercepting network commands (curl, wget).
    ///
    /// The shell's initial working directory is `/`. Use [`Self::with_cwd`]
    /// to override (e.g. when restoring persisted state).
    pub fn new(
        vfs: &'a dyn VirtualFs,
        env: HashMap<String, String>,
        http_proxy: Option<&'a dyn ShellHttpProxy>,
    ) -> Self {
        Self {
            vfs,
            env,
            http_proxy,
            external: None,
            last_status: 0,
            cwd: "/".to_string(),
        }
    }

    /// Builder: set the initial working directory. The path is normalized
    /// to an absolute form; if it does not exist or is not a directory,
    /// the cwd silently falls back to `/`.
    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        let candidate = normalize_path(&cwd.into(), "/");
        self.cwd = if dir_exists(self.vfs, &candidate) {
            candidate
        } else {
            "/".to_string()
        };
        self
    }

    /// Builder: set the initial working directory, returning an error instead
    /// of falling back when the path is missing or not a directory.
    pub fn try_with_cwd(mut self, cwd: impl Into<String>) -> Result<Self, VfsError> {
        let candidate = normalize_path(&cwd.into(), "/");
        let metadata = self.vfs.metadata(&candidate)?;
        if !metadata.is_dir {
            return Err(VfsError::NotADirectory(candidate));
        }
        self.cwd = candidate;
        Ok(self)
    }

    /// Builder: attach a mediated runner for commands such as `node` or
    /// `python` that should participate in shell pipes and redirects.
    pub fn with_external(mut self, external: &'a dyn ShellExternalCommand) -> Self {
        self.external = Some(external);
        self
    }

    /// Returns the current working directory.
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Consume the executor and return its environment variables.
    ///
    /// This allows callers to persist shell state (env vars) across
    /// multiple executor lifetimes.
    pub fn into_env(self) -> HashMap<String, String> {
        self.env
    }

    /// Consume the executor and return its working directory.
    ///
    /// Pair with [`Self::into_env`] to persist full shell state.
    pub fn into_cwd(self) -> String {
        self.cwd
    }

    /// Execute a shell command string.
    pub fn run(&mut self, input: &str) -> CommandResult {
        let line = parser::parse(input);
        self.execute_line(&line)
    }

    fn execute_line(&mut self, line: &ShellLine) -> CommandResult {
        if line.items.is_empty() {
            return CommandResult::success("");
        }

        let mut accumulated_stdout = String::new();
        let mut accumulated_stderr = String::new();
        let mut last_exit_code = 0;

        let mut previous_connector = None;

        for item in &line.items {
            let should_execute = match previous_connector {
                None | Some(Connector::Semicolon) => true,
                Some(Connector::And) => last_exit_code == 0,
                Some(Connector::Or) => last_exit_code != 0,
            };

            if should_execute {
                let result = self.execute_pipeline(&item.pipeline);
                accumulated_stdout.push_str(&result.stdout);
                accumulated_stderr.push_str(&result.stderr);
                last_exit_code = result.exit_code;
                self.last_status = last_exit_code;
            }

            previous_connector = item.connector;
        }

        CommandResult {
            stdout: accumulated_stdout,
            stderr: accumulated_stderr,
            exit_code: last_exit_code,
        }
    }

    fn execute_pipeline(&mut self, pipeline: &crate::parser::Pipeline) -> CommandResult {
        if pipeline.commands.is_empty() {
            return CommandResult::success("");
        }

        let is_pipe_chain = pipeline.commands.len() > 1;
        let pipeline_span = if is_pipe_chain {
            tracing::info_span!("shell_pipeline")
        } else {
            Span::none()
        };
        let _pipeline_guard = pipeline_span.enter();

        let mut stdin = String::new();

        // Execute commands in sequence, piping stdout → stdin
        let last_idx = pipeline.commands.len() - 1;
        let mut last_result = CommandResult::success("");
        let mut accumulated_stderr = String::new();

        for (i, cmd) in pipeline.commands.iter().enumerate() {
            let cmd_span = tracing::info_span!(
                "shell_command",
                simulacra.operation.name = "shell_command",
                simulacra.shell.command = cmd.program.as_str(),
                simulacra.shell.argc = cmd.args.len(),
                simulacra.shell.exit_code = field::Empty,
            );
            let _cmd_guard = cmd_span.enter();

            let command_stdin = cmd.heredoc.as_deref().unwrap_or(&stdin);
            let mut result = self.execute_command(cmd, command_stdin);

            let cwd = self.cwd.clone();
            let vfs = self.vfs;
            if let Err(message) = crate::redirects::apply_redirects(
                &mut result,
                &cmd.redirects,
                vfs,
                &cwd,
                |target| self.expand_vars(target),
            ) {
                result.exit_code = 1;
                result.stderr.push_str(&message);
            }
            cmd_span.record("simulacra.shell.exit_code", result.exit_code as i64);

            if i < last_idx {
                stdin = result.stdout.clone();
            }

            accumulated_stderr.push_str(&result.stderr);

            last_result = result;
        }

        last_result.stderr = accumulated_stderr;
        last_result
    }

    fn execute_command(&mut self, cmd: &crate::parser::Command, stdin: &str) -> CommandResult {
        let program = if cmd.program_literal {
            cmd.program.clone()
        } else {
            self.expand_vars(&cmd.program)
        };
        let args: Vec<String> = cmd
            .args
            .iter()
            .enumerate()
            .map(|(i, a)| {
                if cmd.literal_args.get(i).copied().unwrap_or(false) {
                    a.clone()
                } else {
                    self.expand_vars(a)
                }
            })
            .collect();

        if program.is_empty() {
            return CommandResult::success("");
        }

        // Handle export: set environment variables
        if program == "export" {
            for arg in &args {
                if let Some((key, value)) = arg.split_once('=') {
                    self.env.insert(key.to_string(), value.to_string());
                }
            }
            return CommandResult::success("");
        }

        // ── Stateful builtins (need executor state) ──────────────────
        // These cannot be in `try_builtin` because that table is pure.

        // `cd [dir]` — update self.cwd. With no arg, cd to '/'.
        if program == "cd" {
            return self.builtin_cd(&args);
        }

        // `pwd` — print current working directory.
        if program == "pwd" {
            return CommandResult::success(format!("{}\n", self.cwd));
        }

        // `env` — print environment as KEY=VALUE lines (sorted for determinism).
        if program == "env" {
            let mut keys: Vec<&String> = self.env.keys().collect();
            keys.sort();
            let mut out = String::new();
            for key in keys {
                if let Some(value) = self.env.get(key) {
                    out.push_str(&format!("{key}={value}\n"));
                }
            }
            return CommandResult::success(out);
        }

        // `which CMD ...` — for each name, print the name if it's a known
        // builtin (or `cd`/`pwd`/`env`/`which`/`export`), else stderr + nonzero.
        if program == "which" {
            return self.builtin_which(&args);
        }

        // Try builtins first
        if let Some(result) =
            builtins::try_builtin(&program, &args, stdin, self.vfs, self.http_proxy, &self.cwd)
        {
            return result;
        }

        if let Some(external) = self.external
            && let Some(result) = external.run_external(&program, &args, stdin, &self.cwd)
        {
            return result;
        }

        // Unknown command
        CommandResult::error(127, format!("command not found: {program}\n"))
    }

    /// `cd` builtin — updates the executor's cwd. POSIX-ish:
    ///   - no args → `/` (we do not have $HOME semantics)
    ///   - relative arg resolved against current cwd
    ///   - target must exist and be a directory
    fn builtin_cd(&mut self, args: &[String]) -> CommandResult {
        let target = if args.is_empty() {
            "/".to_string()
        } else {
            resolve_against_cwd(&args[0], &self.cwd)
        };

        match self.vfs.metadata(&target) {
            Ok(m) if m.is_dir => {
                self.cwd = target;
                CommandResult::success("")
            }
            Ok(_) => CommandResult::error(1, format!("cd: not a directory: {target}\n")),
            Err(_) => CommandResult::error(1, format!("cd: no such file or directory: {target}\n")),
        }
    }

    /// `which` builtin — reports whether each argument is a known shell command.
    /// We don't have a $PATH; everything is either a builtin or unknown.
    fn builtin_which(&self, args: &[String]) -> CommandResult {
        if args.is_empty() {
            return CommandResult::error(1, "which: missing operand\n".to_string());
        }
        let mut out = String::new();
        let mut all_found = true;
        for name in args {
            if is_known_command(name) {
                out.push_str(&format!("{name}: shell builtin\n"));
            } else {
                all_found = false;
            }
        }
        if all_found {
            CommandResult::success(out)
        } else {
            CommandResult {
                stdout: out,
                stderr: String::new(),
                exit_code: 1,
            }
        }
    }

    /// Expand `$VAR`, `${VAR}`, `$?`, and `$(cmd)` in a string.
    fn expand_vars(&mut self, input: &str) -> String {
        let env = self.env.clone();
        let last_status = self.last_status;
        crate::expansion::expand_vars(input, &env, last_status, |cmd| self.run(cmd))
    }
}

// ── Path helpers ─────────────────────────────────────────────────────

/// Normalize a path. Mirrors the VFS `normalize` helper: collapses `.`, `..`,
/// double slashes, and resolves `..` that would escape root to `/`.
/// If `path` is relative, it is treated as relative to `base`.
pub(crate) fn normalize_path(path: &str, base: &str) -> String {
    let combined = if path.starts_with('/') {
        path.to_string()
    } else if base == "/" {
        format!("/{path}")
    } else {
        format!("{base}/{path}")
    };

    let mut parts: Vec<&str> = Vec::new();
    for seg in combined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    }
}

/// Resolve a possibly-relative path argument against the current working
/// directory. Always returns an absolute, normalized path.
pub(crate) fn resolve_against_cwd(arg: &str, cwd: &str) -> String {
    normalize_path(arg, cwd)
}

/// Returns `true` iff `path` exists in the VFS and is a directory.
fn dir_exists(vfs: &dyn VirtualFs, path: &str) -> bool {
    matches!(vfs.metadata(path), Ok(m) if m.is_dir)
}

/// Whether `name` is a known shell command (builtin or executor-level).
/// Used by `which` to decide whether to report success.
fn is_known_command(name: &str) -> bool {
    matches!(
        name,
        "echo"
            | "cat"
            | "ls"
            | "mkdir"
            | "grep"
            | "rg"
            | "true"
            | "false"
            | "cp"
            | "mv"
            | "rm"
            | "head"
            | "tail"
            | "sed"
            | "wc"
            | "find"
            | "sort"
            | "uniq"
            | "cut"
            | "tr"
            | "tee"
            | "awk"
            | "jq"
            | "curl"
            | "wget"
            | "touch"
            | "test"
            | "["
            | "printf"
            | "basename"
            | "dirname"
            | "node"
            | "nodejs"
            | "python"
            | "python3"
            | "cd"
            | "pwd"
            | "env"
            | "which"
            | "export"
    )
}
