//! Styled lines and the inline screen: committed lines scroll into the
//! terminal's own scrollback, and one live frame (the running work, the
//! input box, the footer) is redrawn in place below them.

use std::fmt::Write as _;
use std::io::Write;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The few colours the terminal client uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Color {
    Accent,
    Green,
    Red,
    Yellow,
    Magenta,
    /// A 256-colour palette index (the splash field's greys).
    Ansi(u8),
}

impl Color {
    fn sgr(self) -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed(match self {
            Color::Accent => "38;5;117",
            Color::Green => "38;5;114",
            Color::Red => "38;5;203",
            Color::Yellow => "38;5;179",
            Color::Magenta => "38;5;176",
            Color::Ansi(index) => return std::borrow::Cow::Owned(format!("38;5;{index}")),
        })
    }
}

/// A text style: one colour plus the usual attributes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // the SGR attributes are independent flags
pub(crate) struct Style {
    pub fg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
}

impl Style {
    pub const PLAIN: Style = Style {
        fg: None,
        bold: false,
        dim: false,
        italic: false,
        underline: false,
    };

    pub const fn fg(color: Color) -> Style {
        Style {
            fg: Some(color),
            ..Style::PLAIN
        }
    }

    pub const fn dim() -> Style {
        Style {
            dim: true,
            ..Style::PLAIN
        }
    }

    pub const fn bold(self) -> Style {
        Style { bold: true, ..self }
    }

    pub const fn italic(self) -> Style {
        Style {
            italic: true,
            ..self
        }
    }

    pub const fn underline(self) -> Style {
        Style {
            underline: true,
            ..self
        }
    }

    pub const fn dimmed(self) -> Style {
        Style { dim: true, ..self }
    }

    fn sgr(self, color: bool) -> String {
        let mut codes: Vec<std::borrow::Cow<'static, str>> = Vec::new();
        if self.bold {
            codes.push("1".into());
        }
        if self.dim {
            codes.push("2".into());
        }
        if self.italic {
            codes.push("3".into());
        }
        if self.underline {
            codes.push("4".into());
        }
        if color {
            if let Some(fg) = self.fg {
                codes.push(fg.sgr());
            }
        }
        if codes.is_empty() {
            String::new()
        } else {
            format!("\x1b[{}m", codes.join(";"))
        }
    }
}

/// One run of text in one style.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Span {
    pub text: String,
    pub style: Style,
}

/// One terminal row (once wrapped): styled spans, no newline inside.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Line(pub Vec<Span>);

impl Line {
    pub fn new() -> Line {
        Line(Vec::new())
    }

    pub fn styled(text: impl Into<String>, style: Style) -> Line {
        let mut line = Line::new();
        line.push(text, style);
        line
    }

    pub fn plain(text: impl Into<String>) -> Line {
        Line::styled(text, Style::PLAIN)
    }

    /// Append a span, merging it into the last one when the style matches.
    pub fn push(&mut self, text: impl Into<String>, style: Style) {
        let text = text.into();
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.0.last_mut() {
            if last.style == style {
                last.text.push_str(&text);
                return;
            }
        }
        self.0.push(Span { text, style });
    }

    pub fn with(mut self, text: impl Into<String>, style: Style) -> Line {
        self.push(text, style);
        self
    }

    pub fn append(&mut self, other: &Line) {
        for span in &other.0 {
            self.push(span.text.clone(), span.style);
        }
    }

    /// Display width in terminal columns.
    pub fn width(&self) -> usize {
        self.0.iter().map(|span| text_width(&span.text)).sum()
    }

    /// The text without styles (tests and plain logs).
    #[cfg(test)]
    pub fn text(&self) -> String {
        self.0.iter().map(|span| span.text.as_str()).collect()
    }

    pub fn is_blank(&self) -> bool {
        self.0.iter().all(|span| span.text.trim().is_empty())
    }

    /// Cut the line to at most `width` columns, ending in `…` when cut.
    pub fn truncate(&self, width: usize) -> Line {
        if self.width() <= width {
            return self.clone();
        }
        let mut out = Line::new();
        let mut used = 0;
        let budget = width.saturating_sub(1);
        'spans: for span in &self.0 {
            let mut piece = String::new();
            for ch in span.text.chars() {
                let w = char_width(ch);
                if used + w > budget {
                    out.push(piece, span.style);
                    break 'spans;
                }
                used += w;
                piece.push(ch);
            }
            out.push(piece, span.style);
        }
        out.push("…", Style::dim());
        out
    }

    fn to_ansi(&self, color: bool) -> String {
        let mut out = String::new();
        for span in &self.0 {
            let sgr = span.style.sgr(color);
            if sgr.is_empty() {
                out.push_str(&span.text);
            } else {
                out.push_str(&sgr);
                out.push_str(&span.text);
                out.push_str("\x1b[0m");
            }
        }
        out
    }
}

pub(crate) fn char_width(ch: char) -> usize {
    if ch == '\t' {
        return 4;
    }
    ch.width().unwrap_or(0)
}

/// Display width of a text (emoji sequences count once; a tab counts 4).
pub(crate) fn text_width(text: &str) -> usize {
    text.width() + 4 * text.matches('\t').count()
}

/// Word-wrap `line` to `width` columns. The first row starts with
/// `first`, the next ones with `rest` (a hanging indent). Words longer
/// than a row are cut. Tabs become spaces.
pub(crate) fn wrap(line: &Line, width: usize, first: &Line, rest: &Line) -> Vec<Line> {
    // Split into tokens: runs of spaces and runs of non-space characters,
    // each keeping its style.
    let mut tokens: Vec<(String, Style, bool)> = Vec::new();
    for span in &line.0 {
        let mut current = String::new();
        let mut current_space = false;
        for ch in span.text.chars() {
            let ch = if ch == '\t' { ' ' } else { ch };
            let is_space = ch == ' ';
            if !current.is_empty() && is_space != current_space {
                tokens.push((std::mem::take(&mut current), span.style, current_space));
            }
            current_space = is_space;
            current.push(ch);
        }
        if !current.is_empty() {
            tokens.push((current, span.style, current_space));
        }
    }
    // Words that continue across a span boundary stay together: group
    // consecutive non-space tokens into one word.
    let mut words: Vec<(Vec<(String, Style)>, bool)> = Vec::new();
    for (text, style, is_space) in tokens {
        match words.last_mut() {
            Some((parts, last_space)) if *last_space == is_space => parts.push((text, style)),
            _ => words.push((vec![(text, style)], is_space)),
        }
    }

    let mut rows: Vec<Line> = Vec::new();
    let mut row = first.clone();
    let mut row_has_text = false;
    let indent_width = |r: &Line| r.width();
    let mut pending_space: Vec<(String, Style)> = Vec::new();
    for (parts, is_space) in words {
        if is_space {
            if row_has_text {
                pending_space = parts;
            }
            continue;
        }
        let word_width: usize = parts.iter().map(|(t, _)| text_width(t)).sum();
        let space_width: usize = pending_space.iter().map(|(t, _)| text_width(t)).sum();
        if row_has_text && row.width() + space_width + word_width > width {
            rows.push(std::mem::replace(&mut row, rest.clone()));
            row_has_text = false;
            pending_space.clear();
        }
        for (text, style) in pending_space.drain(..) {
            row.push(text, style);
        }
        if row.width() + word_width <= width {
            for (text, style) in parts {
                row.push(text, style);
            }
            row_has_text = true;
            continue;
        }
        // A word longer than the row: cut it by characters.
        for (text, style) in parts {
            for ch in text.chars() {
                let w = char_width(ch);
                if row.width() + w > width && row.width() > indent_width(rest) {
                    rows.push(std::mem::replace(&mut row, rest.clone()));
                }
                row.push(ch.to_string(), style);
                row_has_text = true;
            }
        }
    }
    rows.push(row);
    rows
}

/// The inline screen. Committed lines are printed once; the frame below
/// them is cleared and redrawn on every change.
pub(crate) struct Screen {
    out: std::io::Stdout,
    color: bool,
    /// The row of the frame the cursor was left on.
    cursor_row: usize,
    pub width: usize,
    pub height: usize,
}

impl Screen {
    pub fn new() -> Screen {
        let (width, height) = terminal_size();
        Screen {
            out: std::io::stdout(),
            color: std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty()),
            cursor_row: 0,
            width,
            height,
        }
    }

    pub fn resize(&mut self) {
        let (width, height) = terminal_size();
        self.width = width;
        self.height = height;
    }

    /// The usable width for wrapped text (one column of margin keeps the
    /// terminal out of its pending-wrap state).
    pub fn text_width(&self) -> usize {
        self.width.saturating_sub(1).max(20)
    }

    /// Clear the whole window (Ctrl+L); the frame is drawn again at the top.
    pub fn clear_all(&mut self) {
        let _ = self.out.write_all(b"\x1b[2J\x1b[H");
        let _ = self.out.flush();
        self.cursor_row = 0;
    }

    /// Clear the old frame, print `committed` for good, then draw `frame`
    /// and leave the cursor at `cursor` (row, column) inside it, or hide it.
    pub fn draw(&mut self, committed: &[Line], frame: &[Line], cursor: Option<(usize, usize)>) {
        let mut buf = String::new();
        buf.push_str("\x1b[?25l");
        if self.cursor_row > 0 {
            let _ = write!(buf, "\x1b[{}A", self.cursor_row);
        }
        buf.push_str("\r\x1b[J");
        for line in committed {
            buf.push_str(&line.to_ansi(self.color));
            buf.push_str("\r\n");
        }
        for (index, line) in frame.iter().enumerate() {
            if index > 0 {
                buf.push_str("\r\n");
            }
            buf.push_str(&line.to_ansi(self.color));
        }
        let last = frame.len().saturating_sub(1);
        match cursor {
            Some((row, col)) => {
                let row = row.min(last);
                if last > row {
                    let _ = write!(buf, "\x1b[{}A", last - row);
                }
                buf.push('\r');
                if col > 0 {
                    let _ = write!(buf, "\x1b[{col}C");
                }
                buf.push_str("\x1b[?25h");
                self.cursor_row = row;
            }
            None => self.cursor_row = last,
        }
        let _ = self.out.write_all(buf.as_bytes());
        let _ = self.out.flush();
    }
}

fn terminal_size() -> (usize, usize) {
    match crossterm::terminal::size() {
        Ok((cols, rows)) if cols > 0 && rows > 0 => (usize::from(cols), usize::from(rows)),
        _ => (80, 24),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(rows: &[Line]) -> Vec<String> {
        rows.iter().map(Line::text).collect()
    }

    #[test]
    fn wrap_keeps_short_lines_whole() {
        let rows = wrap(&Line::plain("hello world"), 20, &Line::new(), &Line::new());
        assert_eq!(texts(&rows), vec!["hello world"]);
    }

    #[test]
    fn wrap_breaks_at_spaces_with_hanging_indent() {
        let rows = wrap(
            &Line::plain("one two three four five"),
            12,
            &Line::plain("• "),
            &Line::plain("  "),
        );
        assert_eq!(texts(&rows), vec!["• one two", "  three four", "  five"]);
    }

    #[test]
    fn wrap_cuts_words_longer_than_the_row() {
        let rows = wrap(&Line::plain("abcdefghij"), 4, &Line::new(), &Line::new());
        assert_eq!(texts(&rows), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn wrap_counts_wide_characters_as_two_columns() {
        let rows = wrap(&Line::plain("日本語 日本語"), 7, &Line::new(), &Line::new());
        assert_eq!(texts(&rows), vec!["日本語", "日本語"]);
    }

    #[test]
    fn wrap_keeps_a_word_split_across_styles_together() {
        let line = Line::plain("see ")
            .with("bold", Style::PLAIN.bold())
            .with("ish now", Style::PLAIN);
        let rows = wrap(&line, 9, &Line::new(), &Line::new());
        assert_eq!(texts(&rows), vec!["see", "boldish", "now"]);
    }

    #[test]
    fn wrap_of_an_empty_line_is_one_indent_row() {
        let rows = wrap(&Line::new(), 10, &Line::plain("  "), &Line::plain("  "));
        assert_eq!(texts(&rows), vec!["  "]);
    }

    #[test]
    fn truncate_ends_with_an_ellipsis() {
        assert_eq!(Line::plain("abcdef").truncate(4).text(), "abc…");
        assert_eq!(Line::plain("abc").truncate(4).text(), "abc");
    }
}
