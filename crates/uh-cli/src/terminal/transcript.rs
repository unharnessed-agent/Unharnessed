//! The conversation as the terminal shows it: the streamed answer
//! (rendered as Markdown line by line), the Python cells as short blocks,
//! the thinking time, and the harness notices. Finished pieces are
//! returned as committed lines; what is still moving is drawn by
//! [`Transcript::live`].

use std::time::{Duration, Instant};

use serde_json::Value;

use super::markdown::Markdown;
use super::screen::{wrap, Color, Line, Style};

const CODE_PREVIEW_LINES: usize = 6;
const OUTPUT_PREVIEW_LINES: usize = 4;
const FAILURE_PREVIEW_LINES: usize = 8;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

struct RunningTool {
    id: String,
    title: String,
    code: String,
    started: Instant,
}

pub(crate) struct Transcript {
    md: Markdown,
    /// The streamed text not yet ended by a newline.
    partial: String,
    message: Option<String>,
    thinking: Option<Instant>,
    tools: Vec<RunningTool>,
    /// Replaying a saved conversation: no timings.
    pub replaying: bool,
}

impl Transcript {
    pub fn new() -> Transcript {
        Transcript {
            md: Markdown::default(),
            partial: String::new(),
            message: None,
            thinking: None,
            tools: Vec::new(),
            replaying: false,
        }
    }

    pub fn is_moving(&self) -> bool {
        self.thinking.is_some() || !self.tools.is_empty() || !self.partial.is_empty()
    }

    /// A piece of the answer.
    pub fn text(&mut self, message: Option<&str>, text: &str, width: usize) -> Vec<Line> {
        let mut out = Vec::new();
        if message.map(str::to_string) != self.message {
            out.extend(self.flush(width));
            self.message = message.map(str::to_string);
        }
        out.extend(self.end_thinking());
        self.partial.push_str(&sanitize(text));
        while let Some(newline) = self.partial.find('\n') {
            let line: String = self.partial.drain(..=newline).collect();
            out.extend(self.md.line(line.trim_end_matches('\n'), width));
        }
        out
    }

    /// Commit the unfinished line and close the current message.
    pub fn flush(&mut self, width: usize) -> Vec<Line> {
        let mut out = Vec::new();
        if !self.partial.is_empty() {
            let line = std::mem::take(&mut self.partial);
            out.extend(self.md.line(&line, width));
        }
        self.md = Markdown::default();
        self.message = None;
        out
    }

    pub fn start_thinking(&mut self) {
        if self.thinking.is_none() {
            self.thinking = Some(Instant::now());
        }
    }

    /// Close the thinking indicator; a pause of a second or more leaves a
    /// short trace.
    pub fn end_thinking(&mut self) -> Vec<Line> {
        let Some(started) = self.thinking.take() else {
            return Vec::new();
        };
        let elapsed = started.elapsed();
        if self.replaying || elapsed < Duration::from_secs(1) {
            return Vec::new();
        }
        vec![Line::styled(
            format!("  ∴ thought for {}", duration(elapsed)),
            Style::dim().italic(),
        )]
    }

    pub fn tool_start(&mut self, update: &Value, width: usize) -> Vec<Line> {
        let mut out = self.flush(width);
        out.extend(self.end_thinking());
        let id = str_field(update, "toolCallId").to_string();
        let title = tool_title(str_field(update, "title"));
        let input = update.get("rawInput");
        let code = input
            .and_then(|input| input.get("code").or_else(|| input.get("command")))
            .and_then(Value::as_str)
            .map_or_else(
                || input.map(Value::to_string).unwrap_or_default(),
                str::to_string,
            );
        self.tools.push(RunningTool {
            id,
            title,
            code: sanitize(&code),
            started: Instant::now(),
        });
        out
    }

    /// A cell ended. `interrupted`: the user stopped the turn, so a failed
    /// cell was cut short rather than broken.
    pub fn tool_end(&mut self, update: &Value, interrupted: bool, width: usize) -> Vec<Line> {
        let id = str_field(update, "toolCallId");
        let Some(index) = self.tools.iter().position(|tool| tool.id == id) else {
            return Vec::new();
        };
        let tool = self.tools.remove(index);
        let output: String = update
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|block| block.pointer("/content/text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let ename = update
            .pointer("/_meta/unharnessed/ipython/errorEname")
            .and_then(Value::as_str);
        let failed = str_field(update, "status") == "failed" || ename.is_some();
        let elapsed = (!self.replaying).then(|| tool.started.elapsed());
        if failed && interrupted && ename.is_none() {
            return tool_block(
                &tool.title,
                &tool.code,
                &ToolState::Interrupted,
                elapsed,
                None,
                width,
            );
        }
        let state = if failed {
            ToolState::Failed(ename.map(str::to_string))
        } else {
            ToolState::Done
        };
        tool_block(
            &tool.title,
            &tool.code,
            &state,
            elapsed,
            Some(&sanitize(&output)),
            width,
        )
    }

    /// The turn stopped: commit what was still moving as interrupted.
    pub fn interrupt(&mut self, width: usize) -> Vec<Line> {
        let mut out = self.flush(width);
        out.extend(self.end_thinking());
        for tool in std::mem::take(&mut self.tools) {
            out.extend(tool_block(
                &tool.title,
                &tool.code,
                &ToolState::Interrupted,
                Some(tool.started.elapsed()),
                None,
                width,
            ));
        }
        out
    }

    /// What is still moving: the line being written, the running cells,
    /// the thinking indicator.
    pub fn live(&self, width: usize, now: Instant) -> Vec<Line> {
        let mut out = Vec::new();
        if !self.partial.is_empty() {
            out.extend(self.md.preview(&self.partial, width));
        }
        for tool in &self.tools {
            let elapsed = now.saturating_duration_since(tool.started);
            out.extend(tool_block(
                &tool.title,
                &tool.code,
                &ToolState::Running(spinner(now)),
                Some(elapsed),
                None,
                width,
            ));
        }
        if let Some(started) = self.thinking {
            out.push(Line::styled(
                format!(
                    "  {} thinking {}",
                    spinner(now),
                    duration(now.saturating_duration_since(started))
                ),
                Style::dim().italic(),
            ));
        }
        out
    }
}

enum ToolState {
    Running(&'static str),
    Done,
    Failed(Option<String>),
    Interrupted,
}

fn tool_title(title: &str) -> String {
    match title {
        "Python cell" | "ipython" => "python".to_string(),
        "" => "tool".to_string(),
        other => other.to_string(),
    }
}

/// The short block of one cell: a header, the first lines of the code,
/// and the last lines of the output.
fn tool_block(
    title: &str,
    code: &str,
    state: &ToolState,
    elapsed: Option<Duration>,
    output: Option<&str>,
    width: usize,
) -> Vec<Line> {
    let gutter = Style::dim();
    let mut header = Line::plain("  ");
    match state {
        ToolState::Running(frame) => header.push(*frame, Style::fg(Color::Accent)),
        ToolState::Done => header.push("●", Style::fg(Color::Green)),
        ToolState::Failed(_) => header.push("✗", Style::fg(Color::Red)),
        ToolState::Interrupted => header.push("■", Style::fg(Color::Yellow)),
    }
    header.push(format!(" {title}"), Style::PLAIN.bold());
    if let ToolState::Failed(Some(ename)) = state {
        header.push(" · ", Style::dim());
        header.push(ename.clone(), Style::fg(Color::Red));
    }
    if let ToolState::Interrupted = state {
        header.push(" · interrupted", Style::fg(Color::Yellow));
    }
    if let Some(elapsed) = elapsed {
        header.push(format!(" · {}", duration(elapsed)), Style::dim());
    }
    let mut out = vec![header];

    let code = code.trim_end();
    let lines: Vec<&str> = code.lines().collect();
    for line in lines.iter().take(CODE_PREVIEW_LINES) {
        let row = Line::plain("  ")
            .with("│ ", gutter)
            .with(line.replace('\t', "    "), Style::PLAIN);
        out.push(row.truncate(width));
    }
    if lines.len() > CODE_PREVIEW_LINES {
        out.push(Line::plain("  ").with(
            format!("│ … {} more lines", lines.len() - CODE_PREVIEW_LINES),
            gutter,
        ));
    }

    let Some(output) = output else {
        return out;
    };
    let (restarted, output) = strip_kernel_reset(output);
    if restarted {
        out.push(
            Line::plain("  ")
                .with("│ ", gutter)
                .with("the kernel restarted", Style::fg(Color::Yellow)),
        );
    }
    let output = output.trim_end_matches(['\n', ' ']);
    let output = output.trim_start_matches('\n');
    if output.trim().is_empty() {
        out.push(
            Line::plain("  ")
                .with("╰─ ", gutter)
                .with("no output", gutter),
        );
        return out;
    }
    let failed = matches!(state, ToolState::Failed(_));
    let keep = if failed {
        FAILURE_PREVIEW_LINES
    } else {
        OUTPUT_PREVIEW_LINES
    };
    let lines: Vec<&str> = output.lines().collect();
    let hidden = lines.len().saturating_sub(keep);
    let style = if failed {
        Style::fg(Color::Red)
    } else {
        Style::PLAIN
    };
    let mut first = true;
    let mut lead = |out: &mut Vec<Line>, text: Line| {
        let prefix = if first { "╰─ " } else { "   " };
        first = false;
        let mut row = Line::plain("  ").with(prefix, gutter);
        row.append(&text);
        out.push(row.truncate(width));
    };
    if hidden > 0 {
        lead(
            &mut out,
            Line::styled(format!("… {hidden} lines above"), gutter),
        );
    }
    for line in &lines[hidden..] {
        lead(&mut out, Line::styled(line.replace('\t', "    "), style));
    }
    out
}

/// Remove the kernel-restart notice the harness prefixes to a cell's text.
fn strip_kernel_reset(output: &str) -> (bool, &str) {
    const CLOSE: &str = "</ipython_kernel_reset>";
    if output.trim_start().starts_with("<ipython_kernel_reset>") {
        if let Some(end) = output.find(CLOSE) {
            return (true, &output[end + CLOSE.len()..]);
        }
    }
    (false, output)
}

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

pub(crate) fn spinner(now: Instant) -> &'static str {
    // A process-wide clock is enough: only the frame index matters.
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let start = *START.get_or_init(Instant::now);
    let ticks = now.saturating_duration_since(start).as_millis() / 80;
    let index = usize::try_from(ticks % 10).unwrap_or(0);
    SPINNER[index]
}

/// `0.4s`, `12s`, `3m 05s`.
pub(crate) fn duration(elapsed: Duration) -> String {
    let millis = elapsed.as_millis();
    if millis < 10_000 {
        let tenths = millis / 100;
        return format!("{}.{}s", tenths / 10, tenths % 10);
    }
    let secs = elapsed.as_secs();
    if secs < 60 {
        return format!("{secs}s");
    }
    format!("{}m {:02}s", secs / 60, secs % 60)
}

/// Drop terminal escape sequences and control characters from text the
/// agent or a program produced, so they cannot move the cursor or recolour
/// the screen. Newlines and tabs stay.
pub(crate) fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => {
                // CSI (`ESC [ ... final`), OSC (`ESC ] ... BEL|ESC \`), or a
                // two-character escape.
                match chars.peek() {
                    Some('[') => {
                        chars.next();
                        for next in chars.by_ref() {
                            if ('@'..='~').contains(&next) {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        chars.next();
                        while let Some(next) = chars.next() {
                            if next == '\x07' {
                                break;
                            }
                            if next == '\x1b' && chars.peek() == Some(&'\\') {
                                chars.next();
                                break;
                            }
                        }
                    }
                    Some(_) => {
                        chars.next();
                    }
                    None => {}
                }
            }
            '\r' => {
                if chars.peek() != Some(&'\n') {
                    out.push('\n');
                }
            }
            '\n' | '\t' => out.push(ch),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// One harness notice line (`icon text`), wrapped.
pub(crate) fn notice(icon: &str, text: &str, style: Style, width: usize) -> Vec<Line> {
    let mut out = Vec::new();
    for (index, line) in sanitize(text).lines().enumerate() {
        let lead = if index == 0 {
            Line::plain("  ").with(format!("{icon} "), style)
        } else {
            Line::plain("  ").with(" ".repeat(super::screen::text_width(icon) + 1), style)
        };
        let rest = Line::plain(" ".repeat(lead.width()));
        out.extend(wrap(&Line::styled(line, style), width, &lead, &rest));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::text).collect()
    }

    #[test]
    fn streamed_text_commits_whole_lines_only() {
        let mut transcript = Transcript::new();
        assert!(transcript.text(Some("m1"), "Hel", 40).is_empty());
        let lines = transcript.text(Some("m1"), "lo **world**\nand", 40);
        assert_eq!(texts(&lines), vec!["  Hello world"]);
        assert_eq!(texts(&transcript.live(40, Instant::now())), vec!["  and"]);
        assert_eq!(texts(&transcript.flush(40)), vec!["  and"]);
        assert!(!transcript.is_moving());
    }

    #[test]
    fn a_new_message_closes_the_previous_one() {
        let mut transcript = Transcript::new();
        transcript.text(Some("m1"), "first", 40);
        let lines = transcript.text(Some("m2"), "second\n", 40);
        assert_eq!(texts(&lines), vec!["  first", "  second"]);
    }

    #[test]
    fn a_cell_renders_as_a_short_block() {
        let mut transcript = Transcript::new();
        transcript.replaying = true;
        transcript.tool_start(
            &json!({"toolCallId": "t1", "title": "Python cell", "rawInput": {"code": "rs = list(x)\nlen(rs)"}}),
            40,
        );
        let lines = transcript.tool_end(
            &json!({"toolCallId": "t1", "status": "completed", "content": [{"type": "content", "content": {"type": "text", "text": "312\n"}}]}),
            false,
            40,
        );
        assert_eq!(
            texts(&lines),
            vec!["  ● python", "  │ rs = list(x)", "  │ len(rs)", "  ╰─ 312"]
        );
    }

    #[test]
    fn long_code_and_output_are_shortened() {
        let code: Vec<String> = (1..=9).map(|n| format!("x{n}")).collect();
        let output: Vec<String> = (1..=10).map(|n| format!("line {n}")).collect();
        let lines = tool_block(
            "python",
            &code.join("\n"),
            &ToolState::Done,
            None,
            Some(&output.join("\n")),
            40,
        );
        let texts = texts(&lines);
        assert!(texts.contains(&"  │ … 3 more lines".to_string()));
        assert!(texts.contains(&"  ╰─ … 6 lines above".to_string()));
        assert_eq!(texts.last().map(String::as_str), Some("     line 10"));
    }

    #[test]
    fn a_failed_cell_names_its_exception() {
        let lines = tool_block(
            "python",
            "1/0",
            &ToolState::Failed(Some("ZeroDivisionError".into())),
            None,
            Some("Traceback\nZeroDivisionError: division by zero"),
            60,
        );
        assert_eq!(lines[0].text(), "  ✗ python · ZeroDivisionError");
        assert_eq!(
            lines.last().map(Line::text).as_deref(),
            Some("     ZeroDivisionError: division by zero")
        );
    }

    #[test]
    fn empty_output_and_kernel_restarts() {
        let lines = tool_block("python", "x = 1", &ToolState::Done, None, Some(""), 40);
        assert_eq!(
            lines.last().map(Line::text).as_deref(),
            Some("  ╰─ no output")
        );
        let lines = tool_block(
            "python",
            "x",
            &ToolState::Done,
            None,
            Some("<ipython_kernel_reset>\nreset\n</ipython_kernel_reset>\n\n5"),
            40,
        );
        assert_eq!(
            texts(&lines)[2..],
            ["  │ the kernel restarted".to_string(), "  ╰─ 5".to_string()]
        );
    }

    #[test]
    fn sanitize_drops_escape_sequences_and_controls() {
        assert_eq!(sanitize("\x1b[31mred\x1b[0m ok"), "red ok");
        assert_eq!(sanitize("a\x1b]0;title\x07b"), "ab");
        assert_eq!(sanitize("a\x1b]8;;http://x\x1b\\b"), "ab");
        assert_eq!(sanitize("x\x08y\tz\r\nw\rv"), "xy\tz\nw\nv");
    }
}
