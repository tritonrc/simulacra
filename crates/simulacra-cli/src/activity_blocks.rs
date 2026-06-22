//! Activity block rendering for interactive mode (S019).
//!
//! Renders real-time activity blocks for tool execution, sub-agent delegation,
//! and model thinking. Each block transitions through phases:
//! in-progress (spinner active) → completed (static summary).
//!
//! ## Rendering rules
//!
//! - `ToolStart` opens an activity block with a spinner, tool name, and a
//!   truncated argument summary. The spinner animates while work is active.
//! - `ToolOutput` lines appear in a tail window (last 3 lines visible, default: 3).
//!   When output exceeds the tail window, a `... +N lines` hidden_count indicator
//!   appears and updates as new lines arrive.
//! - `ToolFinish` transitions the block to completed: a filled dot for success or
//!   an X indicator for errors. Summary shows `Done · {duration}` or
//!   `Error (exit code {N}) · {duration}`.
//! - `ChildSpawned` opens a child activity block with spinner + agent_type + task summary.
//! - `ChildActivity` events render within the child's activity block using the same
//!   rules recursively (recursive activity block nesting).
//! - `ChildFinished` transitions the child block to completed with stats:
//!   `Done ({tool uses} tool uses · {tokens} tokens · {duration})`.
//! - `ThinkStart` opens a single-line thinking block with a thinking indicator (+)
//!   and label "Thinking...".
//! - While thinking is in-progress, the line updates in-place showing: elapsed time,
//!   tokens received so far, and think duration.
//! - `ThinkEnd` finalizes the thinking line to a static summary showing
//!   `{duration} · ↓ {token_count} tokens · thought for {think_duration}`.
//! - `ThinkDelta` content is not rendered to the terminal — the CLI does not display
//!   thinking text. ThinkDelta data is available in the event stream for server
//!   consumers and logging only.

use std::collections::HashMap;
use std::time::Instant;

use simulacra_types::ActivityEvent;

/// Tail window size: how many recent output lines are visible.
const TAIL_WINDOW_SIZE: usize = 3; // default: 3

/// Rendering state for a single activity block.
#[derive(Debug)]
struct ActivityBlock {
    /// Block kind: "tool", "agent", or "thinking".
    #[allow(dead_code)]
    kind: String,
    /// Display name (tool name, agent_type, or "Thinking...").
    name: String,
    /// Truncated argument/task summary for the header line.
    summary: String,
    /// When the block was opened.
    started_at: Instant,
    /// Tail window of recent output lines.
    tail_lines: Vec<String>,
    /// Total lines received (for hidden_count = total - TAIL_WINDOW_SIZE).
    total_lines: usize,
    /// Whether the block is completed.
    completed: bool,
    /// Whether the block ended with an error.
    is_error: bool,
    /// Completion summary text.
    completion_summary: String,
}

impl ActivityBlock {
    fn hidden_count(&self) -> usize {
        self.total_lines.saturating_sub(TAIL_WINDOW_SIZE)
    }

    fn push_output(&mut self, line: String) {
        self.total_lines += 1;
        self.tail_lines.push(line);
        if self.tail_lines.len() > TAIL_WINDOW_SIZE {
            self.tail_lines.remove(0);
        }
    }
}

/// Manages all active activity blocks and renders them.
#[derive(Debug, Default)]
pub struct ActivityBlockRenderer {
    /// Active blocks keyed by their ID (tool_call_id, child_id, or "thinking").
    blocks: HashMap<String, ActivityBlock>,
    /// Nested child blocks (child_id → parent block mapping).
    #[allow(dead_code)]
    children: HashMap<String, String>,
    /// Spinner frame index for animation.
    spinner_frame: usize,
}

/// Spinner animation frames.
const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

impl ActivityBlockRenderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process an activity event and return lines to render.
    pub fn process_event(&mut self, event: &ActivityEvent) -> Vec<String> {
        match event {
            // ToolStart opens an activity block with spinner + tool name + argument summary
            ActivityEvent::ToolStart {
                tool_call_id,
                name,
                arguments,
            } => {
                let summary = truncate_argument_summary(arguments);
                let block = ActivityBlock {
                    kind: "tool".into(),
                    name: name.clone(),
                    summary,
                    started_at: Instant::now(),
                    tail_lines: Vec::new(),
                    total_lines: 0,
                    completed: false,
                    is_error: false,
                    completion_summary: String::new(),
                };
                self.blocks.insert(tool_call_id.clone(), block);
                self.render_block_header(tool_call_id)
            }

            // ToolOutput lines appear in the tail window (last 3 lines visible)
            ActivityEvent::ToolOutput { tool_call_id, line } => {
                if let Some(block) = self.blocks.get_mut(tool_call_id) {
                    block.push_output(line.clone());
                    self.render_block_body(tool_call_id)
                } else {
                    vec![]
                }
            }

            // ToolFinish transitions block to completed with filled dot or X indicator
            ActivityEvent::ToolFinish {
                tool_call_id,
                name: _,
                is_error,
                duration_ms,
                exit_code,
            } => {
                if let Some(block) = self.blocks.get_mut(tool_call_id) {
                    block.completed = true;
                    block.is_error = *is_error;
                    block.completion_summary = if *is_error {
                        if let Some(code) = exit_code {
                            format!("Error (exit code {code}) · {}ms", duration_ms)
                        } else {
                            format!("Error · {}ms", duration_ms)
                        }
                    } else {
                        format!("Done · {}ms", duration_ms)
                    };
                    self.render_block_completed(tool_call_id)
                } else {
                    vec![]
                }
            }

            // ChildSpawned opens a child activity block with agent_type and task summary
            ActivityEvent::ChildSpawned {
                child_id,
                agent_type,
                task,
            } => {
                let block = ActivityBlock {
                    kind: "agent".into(),
                    name: agent_type.clone(),
                    summary: truncate_text(task, 60),
                    started_at: Instant::now(),
                    tail_lines: Vec::new(),
                    total_lines: 0,
                    completed: false,
                    is_error: false,
                    completion_summary: String::new(),
                };
                self.blocks.insert(child_id.clone(), block);
                self.render_block_header(child_id)
            }

            // ChildActivity events render within the child's activity block
            // using the same rules recursively (recursive activity block nesting)
            ActivityEvent::ChildActivity {
                child_id,
                agent_type: _,
                event,
            } => {
                // Process the inner event recursively
                let inner_lines = self.process_event(event);
                // If the child block exists, add inner output to its tail
                if let Some(block) = self.blocks.get_mut(child_id) {
                    for line in &inner_lines {
                        block.push_output(format!("  {line}"));
                    }
                }
                inner_lines
            }

            // ChildFinished transitions child block to completed with stats
            ActivityEvent::ChildFinished {
                child_id,
                agent_type: _,
                exit_reason: _,
                duration_ms,
                tool_uses,
                token_count,
            } => {
                if let Some(block) = self.blocks.get_mut(child_id) {
                    block.completed = true;
                    block.completion_summary = format!(
                        "Done ({} tool uses · {} tokens · {}ms)",
                        tool_uses, token_count, duration_ms
                    );
                    self.render_block_completed(child_id)
                } else {
                    vec![]
                }
            }

            // ThinkStart opens a single-line thinking block with thinking indicator
            ActivityEvent::ThinkStart => {
                let block = ActivityBlock {
                    kind: "thinking".into(),
                    name: "Thinking...".into(),
                    summary: String::new(),
                    started_at: Instant::now(),
                    tail_lines: Vec::new(),
                    total_lines: 0,
                    completed: false,
                    is_error: false,
                    completion_summary: String::new(),
                };
                self.blocks.insert("thinking".into(), block);
                vec![format!("+ Thinking...")]
            }

            // ThinkDelta content is not rendered to the terminal.
            // The CLI does not display thinking text. ThinkDelta data
            // stays in the event stream for server consumers and logging.
            ActivityEvent::ThinkDelta { .. } => {
                // Not rendered in CLI — content available for server/logging only
                vec![]
            }

            // ThinkEnd finalizes the thinking line to a static summary
            // showing elapsed time, tokens received, and think duration
            ActivityEvent::ThinkEnd {
                think_duration_ms,
                think_tokens,
            } => {
                if let Some(block) = self.blocks.get_mut("thinking") {
                    let elapsed = block.started_at.elapsed();
                    block.completed = true;
                    block.completion_summary = format!(
                        "thought for {}ms · ↓ {} tokens",
                        think_duration_ms, think_tokens
                    );
                    vec![format!(
                        "+ {:.1}s · ↓ {} tokens · thought for {}ms",
                        elapsed.as_secs_f64(),
                        think_tokens,
                        think_duration_ms
                    )]
                } else {
                    vec![]
                }
            }

            // Token and TurnComplete don't produce activity blocks
            ActivityEvent::Token { .. } | ActivityEvent::TurnComplete => vec![],
        }
    }

    /// Render the header line for an in-progress block.
    fn render_block_header(&mut self, id: &str) -> Vec<String> {
        let spinner = self.next_spinner();
        if let Some(block) = self.blocks.get(id) {
            vec![format!("{} {}({})", spinner, block.name, block.summary)]
        } else {
            vec![]
        }
    }

    /// Render the body (tail window) for an in-progress block.
    fn render_block_body(&self, id: &str) -> Vec<String> {
        if let Some(block) = self.blocks.get(id) {
            let mut lines = Vec::new();
            let hidden = block.hidden_count();
            if hidden > 0 {
                lines.push(format!("  ... +{} lines", hidden));
            }
            for tail_line in &block.tail_lines {
                lines.push(format!("└  {}", tail_line));
            }
            lines
        } else {
            vec![]
        }
    }

    /// Render the completed state for a block.
    fn render_block_completed(&self, id: &str) -> Vec<String> {
        if let Some(block) = self.blocks.get(id) {
            let indicator = if block.is_error {
                "✗" // X indicator for errors
            } else {
                "●" // filled dot for success
            };
            vec![
                format!("{} {}({})", indicator, block.name, block.summary),
                format!("└  {}", block.completion_summary),
            ]
        } else {
            vec![]
        }
    }

    /// Render the in-progress thinking line with elapsed, tokens received, and think duration.
    #[allow(dead_code)]
    fn render_thinking_progress(&self, think_tokens: u64) -> Vec<String> {
        if let Some(block) = self.blocks.get("thinking") {
            let elapsed = block.started_at.elapsed();
            vec![format!(
                "+ Thinking... ({:.1}s · ↓ {} tokens received · think duration {:.1}s)",
                elapsed.as_secs_f64(),
                think_tokens,
                elapsed.as_secs_f64(),
            )]
        } else {
            vec![]
        }
    }

    fn next_spinner(&mut self) -> char {
        let frame = SPINNER_FRAMES[self.spinner_frame % SPINNER_FRAMES.len()];
        self.spinner_frame += 1;
        frame
    }
}

/// Truncate an argument summary for display in the header line.
fn truncate_argument_summary(arguments: &serde_json::Value) -> String {
    let s = arguments.to_string();
    truncate_text(&s, 80)
}

fn truncate_text(s: &str, max_len: usize) -> String {
    // Collapse whitespace (newlines, tabs) to single spaces for single-line display.
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.len() <= max_len {
        collapsed
    } else {
        // Byte-indexed slicing would panic if `max_len - 3` lands in the
        // middle of a multi-byte UTF-8 codepoint (e.g. emoji or CJK
        // character). Walk back to the nearest char boundary so the slice
        // is always valid regardless of the input encoding.
        let mut cut = max_len.saturating_sub(3);
        while cut > 0 && !collapsed.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}...", &collapsed[..cut])
    }
}
