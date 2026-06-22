use super::types::{InteractiveInput, InteractiveOutput};

/// Real terminal I/O using crossterm for raw mode and event-based input.
///
/// In raw mode, crossterm's `event::read()` returns individual key events.
/// We manually buffer characters, handle echo, backspace, and Enter.
pub struct TerminalIo {
    raw_mode_enabled: bool,
    history: Vec<String>,
    history_cursor: usize,
}

impl TerminalIo {
    pub fn new() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self {
            raw_mode_enabled: true,
            history: Vec::new(),
            history_cursor: 0,
        })
    }

    /// Read a line using crossterm key events. Handles echo, backspace,
    /// Up/Down history navigation, and Enter to submit.
    /// Returns None on Ctrl-D (EOF) or read error.
    fn read_line_raw(&mut self, prompt: &str) -> Option<String> {
        use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
        use std::io::Write;

        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "{prompt}");
        let _ = stdout.flush();

        let mut buf = String::new();
        self.history_cursor = self.history.len();

        loop {
            let event = event::read().ok()?;
            match event {
                Event::Key(KeyEvent {
                    code: KeyCode::Enter,
                    ..
                }) => {
                    // Echo newline (raw mode needs \r\n)
                    let _ = write!(stdout, "\r\n");
                    let _ = stdout.flush();
                    if !buf.is_empty() {
                        self.history.push(buf.clone());
                    }
                    return Some(buf);
                }
                Event::Key(KeyEvent {
                    code: KeyCode::Char('d'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }) => {
                    // Ctrl-D = EOF
                    let _ = write!(stdout, "\r\n");
                    let _ = stdout.flush();
                    return None;
                }
                Event::Key(KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }) => {
                    // Ctrl-C at prompt — return special sentinel
                    let _ = write!(stdout, "^C\r\n");
                    let _ = stdout.flush();
                    return Some("\x03".to_string()); // ETX character
                }
                Event::Key(KeyEvent {
                    code: KeyCode::Backspace,
                    ..
                }) if !buf.is_empty() => {
                    buf.pop();
                    // Move cursor back, overwrite with space, move back again
                    let _ = write!(stdout, "\x08 \x08");
                    let _ = stdout.flush();
                }
                Event::Key(KeyEvent {
                    code: KeyCode::Up, ..
                }) if self.history_cursor > 0 => {
                    self.history_cursor -= 1;
                    // Clear current line and replace with history entry
                    let _ = write!(stdout, "\r{prompt}{}", " ".repeat(buf.len()));
                    buf = self.history[self.history_cursor].clone();
                    let _ = write!(stdout, "\r{prompt}{buf}");
                    let _ = stdout.flush();
                }
                Event::Key(KeyEvent {
                    code: KeyCode::Down,
                    ..
                }) if self.history_cursor < self.history.len() => {
                    self.history_cursor += 1;
                    let _ = write!(stdout, "\r{prompt}{}", " ".repeat(buf.len()));
                    if self.history_cursor < self.history.len() {
                        buf = self.history[self.history_cursor].clone();
                    } else {
                        buf.clear();
                    }
                    let _ = write!(stdout, "\r{prompt}{buf}");
                    let _ = stdout.flush();
                }
                Event::Key(KeyEvent {
                    code: KeyCode::Char(ch),
                    modifiers,
                    ..
                }) if !modifiers.contains(KeyModifiers::CONTROL) => {
                    buf.push(ch);
                    // Echo the character
                    let _ = write!(stdout, "{ch}");
                    let _ = stdout.flush();
                }
                _ => {}
            }
        }
    }
}

impl Drop for TerminalIo {
    fn drop(&mut self) {
        if self.raw_mode_enabled {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

impl InteractiveInput for TerminalIo {
    fn read_line(&mut self) -> Option<String> {
        self.read_line_raw("> ")
    }

    fn read_approval(&mut self) -> Option<String> {
        self.read_line_raw("")
    }

    fn is_tty(&self) -> bool {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal()
    }
}

impl InteractiveOutput for TerminalIo {
    fn write_line(&mut self, text: &str) {
        use std::io::Write;
        let mut stdout = std::io::stdout();
        // In raw mode, \n alone moves down without returning to column 0.
        // Split on \n so every line gets a proper \r\n.
        for (i, line) in text.split('\n').enumerate() {
            if i > 0 {
                let _ = write!(stdout, "\r\n");
            }
            let _ = write!(stdout, "{line}");
        }
        let _ = write!(stdout, "\r\n");
        let _ = stdout.flush();
    }

    fn clear(&mut self) {
        let mut stdout = std::io::stdout();
        let _ = crossterm::execute!(
            stdout,
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
            crossterm::cursor::MoveTo(0, 0)
        );
    }

    fn restore_terminal(&mut self) {
        if self.raw_mode_enabled {
            let _ = crossterm::terminal::disable_raw_mode();
            self.raw_mode_enabled = false;
        }
    }
}

// ---------------------------------------------------------------------------
// Spinner — visual activity indicator while the agent is working
// ---------------------------------------------------------------------------

pub(crate) fn start_spinner() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async {
        use std::io::Write;
        let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let mut i = 0;
        loop {
            {
                let mut stdout = std::io::stdout();
                // Move to column 0, clear line, write spinner
                let _ = write!(stdout, "\r{} Thinking...", frames[i % frames.len()]);
                let _ = stdout.flush();
            }
            i += 1;
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        }
    })
}

pub(crate) fn stop_spinner(handle: tokio::task::JoinHandle<()>) {
    handle.abort();
}

pub(crate) fn clear_spinner_line() {
    use std::io::Write;
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "\r\x1b[2K");
    let _ = stdout.flush();
}

/// Replace the spinner line with a permanent "Thinking" summary showing elapsed time.
pub(crate) fn finalize_thinking_line(started: std::time::Instant) {
    use std::io::Write;
    let elapsed = started.elapsed();
    let mut stdout = std::io::stdout();
    let _ = write!(
        stdout,
        "\r\x1b[2K● Thinking ({:.1}s)\r\n",
        elapsed.as_secs_f64()
    );
    let _ = stdout.flush();
}

pub fn generate_uuid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let pid = std::process::id() as u128;
    let combined = nanos ^ (pid << 64);
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (combined >> 96) as u32,
        (combined >> 80) as u16,
        (combined >> 68) as u16 & 0x0FFF,
        ((combined >> 52) as u16 & 0x3FFF) | 0x8000,
        combined & 0xFFFF_FFFF_FFFF
    )
}
