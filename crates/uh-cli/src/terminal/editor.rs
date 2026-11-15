//! The input box: a small multi-line editor with history. Pure state: the
//! app feeds it keys and draws what [`Editor::render`] returns.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::screen::{char_width, Color, Line, Style};

const HISTORY_LIMIT: usize = 500;

/// What a key did to the editor.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EditorAction {
    /// Nothing the app needs to know (the text or cursor may have moved).
    Edited,
    /// Enter on a non-empty buffer.
    Submit(String),
    /// The key is not an editing key.
    Ignored,
}

pub(crate) struct Editor {
    text: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    /// The history entry shown, counted from the newest (`None`: the draft).
    browsing: Option<usize>,
    draft: Vec<char>,
    history_file: Option<PathBuf>,
    pub masked: bool,
}

impl Editor {
    pub fn new(history_file: Option<PathBuf>) -> Editor {
        let history = history_file
            .as_deref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map(|text| {
                text.lines()
                    .filter_map(|line| serde_json::from_str::<String>(line).ok())
                    .collect()
            })
            .unwrap_or_default();
        Editor {
            text: Vec::new(),
            cursor: 0,
            history,
            browsing: None,
            draft: Vec::new(),
            history_file,
            masked: false,
        }
    }

    pub fn text(&self) -> String {
        self.text.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn set_text(&mut self, text: &str) {
        self.text = text.chars().collect();
        self.cursor = self.text.len();
        self.browsing = None;
    }

    pub fn clear(&mut self) {
        self.set_text("");
    }

    /// Insert pasted text at the cursor (newlines kept, `\r\n` folded).
    pub fn insert_str(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        for ch in text.chars() {
            self.insert(ch);
        }
    }

    fn insert(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += 1;
    }

    /// Record a submitted entry (newest last, no repeats in a row) and
    /// append it to the history file.
    pub fn remember(&mut self, entry: &str) {
        if entry.trim().is_empty() || self.history.last().is_some_and(|last| last == entry) {
            return;
        }
        self.history.push(entry.to_string());
        if self.history.len() > HISTORY_LIMIT {
            let drop = self.history.len() - HISTORY_LIMIT;
            self.history.drain(..drop);
        }
        if let Some(path) = &self.history_file {
            let lines: Vec<String> = self
                .history
                .iter()
                .filter_map(|entry| serde_json::to_string(entry).ok())
                .collect();
            let _ = std::fs::write(path, lines.join("\n") + "\n");
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> EditorAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Enter if alt || shift => self.insert('\n'),
            KeyCode::Char('j') if ctrl => self.insert('\n'),
            KeyCode::Enter => {
                // A trailing backslash continues the message on a new line.
                if self.cursor == self.text.len() && self.text.last() == Some(&'\\') {
                    self.text.pop();
                    self.cursor -= 1;
                    self.insert('\n');
                    return EditorAction::Edited;
                }
                let text = self.text();
                if text.trim().is_empty() {
                    return EditorAction::Edited;
                }
                self.clear();
                return EditorAction::Submit(text);
            }
            KeyCode::Char('a') if ctrl => self.cursor = self.line_start(),
            KeyCode::Char('e') if ctrl => self.cursor = self.line_end(),
            KeyCode::Home => self.cursor = self.line_start(),
            KeyCode::End => self.cursor = self.line_end(),
            KeyCode::Char('b') if ctrl => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Char('f') if ctrl => self.cursor = (self.cursor + 1).min(self.text.len()),
            KeyCode::Char('b') if alt => self.cursor = self.word_left(),
            KeyCode::Char('f') if alt => self.cursor = self.word_right(),
            KeyCode::Left if alt || ctrl => self.cursor = self.word_left(),
            KeyCode::Right if alt || ctrl => self.cursor = self.word_right(),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.text.len()),
            KeyCode::Up => self.up(),
            KeyCode::Down => self.down(),
            KeyCode::Backspace if alt || ctrl => self.delete_word_back(),
            KeyCode::Char('w') if ctrl => self.delete_word_back(),
            KeyCode::Char('h') if ctrl => self.backspace(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => {
                if self.cursor < self.text.len() {
                    self.text.remove(self.cursor);
                }
            }
            KeyCode::Char('d') if ctrl && !self.text.is_empty() => {
                if self.cursor < self.text.len() {
                    self.text.remove(self.cursor);
                }
            }
            KeyCode::Char('u') if ctrl => {
                let start = self.line_start();
                self.text.drain(start..self.cursor);
                self.cursor = start;
            }
            KeyCode::Char('k') if ctrl => {
                let end = self.line_end();
                self.text.drain(self.cursor..end);
            }
            KeyCode::Char(ch) if !ctrl && !alt => self.insert(ch),
            _ => return EditorAction::Ignored,
        }
        EditorAction::Edited
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.text.remove(self.cursor);
        }
    }

    fn delete_word_back(&mut self) {
        let start = self.word_left();
        self.text.drain(start..self.cursor);
        self.cursor = start;
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor]
            .iter()
            .rposition(|c| *c == '\n')
            .map_or(0, |i| i + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .iter()
            .position(|c| *c == '\n')
            .map_or(self.text.len(), |i| self.cursor + i)
    }

    fn word_left(&self) -> usize {
        let mut i = self.cursor;
        while i > 0 && !self.text[i - 1].is_alphanumeric() {
            i -= 1;
        }
        while i > 0 && self.text[i - 1].is_alphanumeric() {
            i -= 1;
        }
        i
    }

    fn word_right(&self) -> usize {
        let mut i = self.cursor;
        let len = self.text.len();
        while i < len && !self.text[i].is_alphanumeric() {
            i += 1;
        }
        while i < len && self.text[i].is_alphanumeric() {
            i += 1;
        }
        i
    }

    /// Up: move a line up inside a multi-line message, else go back in
    /// history.
    fn up(&mut self) {
        let start = self.line_start();
        if start == 0 {
            self.history_back();
            return;
        }
        let column = self.cursor - start;
        let prev_start = self.text[..start - 1]
            .iter()
            .rposition(|c| *c == '\n')
            .map_or(0, |i| i + 1);
        let prev_len = start - 1 - prev_start;
        self.cursor = prev_start + column.min(prev_len);
    }

    fn down(&mut self) {
        let end = self.line_end();
        if end == self.text.len() {
            self.history_forward();
            return;
        }
        let column = self.cursor - self.line_start();
        let next_start = end + 1;
        let next_end = self.text[next_start..]
            .iter()
            .position(|c| *c == '\n')
            .map_or(self.text.len(), |i| next_start + i);
        self.cursor = next_start + column.min(next_end - next_start);
    }

    fn history_back(&mut self) {
        let next = match self.browsing {
            None => 0,
            Some(index) => index + 1,
        };
        if next >= self.history.len() {
            return;
        }
        if self.browsing.is_none() {
            self.draft = self.text.clone();
        }
        self.browsing = Some(next);
        self.text = self.history[self.history.len() - 1 - next]
            .chars()
            .collect();
        self.cursor = self.text.len();
    }

    fn history_forward(&mut self) {
        match self.browsing {
            None => {}
            Some(0) => {
                self.browsing = None;
                self.text = std::mem::take(&mut self.draft);
                self.cursor = self.text.len();
            }
            Some(index) => {
                self.browsing = Some(index - 1);
                self.text = self.history[self.history.len() - index].chars().collect();
                self.cursor = self.text.len();
            }
        }
    }

    /// The rows of the input box and the cursor position (row, column).
    /// The first row starts with `prompt`; the next ones are indented to
    /// line up under it. Rows wrap at `width` columns.
    pub fn render(
        &self,
        width: usize,
        prompt: &Line,
        placeholder: &str,
    ) -> (Vec<Line>, (usize, usize)) {
        let indent = prompt.width();
        let room = width.saturating_sub(indent).max(4);
        let mut rows: Vec<Line> = Vec::new();
        let mut row = prompt.clone();
        let mut col = 0;
        let mut cursor = (0, indent);
        if self.text.is_empty() {
            row.push(placeholder, Style::dim());
            return (vec![row], (0, indent));
        }
        let style = Style::PLAIN;
        for (index, ch) in self.text.iter().enumerate() {
            if index == self.cursor {
                cursor = (rows.len(), indent + col);
            }
            if *ch == '\n' {
                rows.push(std::mem::replace(&mut row, Line::plain(" ".repeat(indent))));
                col = 0;
                continue;
            }
            let shown = if self.masked { '•' } else { *ch };
            let w = char_width(shown);
            if col + w > room {
                rows.push(std::mem::replace(&mut row, Line::plain(" ".repeat(indent))));
                col = 0;
                if index == self.cursor {
                    cursor = (rows.len(), indent);
                }
            }
            if shown == '\t' {
                row.push("    ", style);
            } else {
                row.push(shown.to_string(), style);
            }
            col += w;
        }
        if self.cursor == self.text.len() {
            if col >= room {
                rows.push(std::mem::replace(&mut row, Line::plain(" ".repeat(indent))));
                col = 0;
            }
            cursor = (rows.len(), indent + col);
        }
        rows.push(row);
        (rows, cursor)
    }
}

/// The prompt marker of the input box.
pub(crate) fn prompt_marker() -> Line {
    Line::styled("› ", Style::fg(Color::Accent).bold())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    fn typed(text: &str) -> Editor {
        let mut editor = Editor::new(None);
        editor.insert_str(text);
        editor
    }

    #[test]
    fn enter_submits_and_clears() {
        let mut editor = typed("hello");
        assert_eq!(
            editor.key(key(KeyCode::Enter)),
            EditorAction::Submit("hello".into())
        );
        assert!(editor.is_empty());
        assert_eq!(editor.key(key(KeyCode::Enter)), EditorAction::Edited);
    }

    #[test]
    fn a_trailing_backslash_continues_on_a_new_line() {
        let mut editor = typed("one\\");
        assert_eq!(editor.key(key(KeyCode::Enter)), EditorAction::Edited);
        editor.insert_str("two");
        assert_eq!(
            editor.key(key(KeyCode::Enter)),
            EditorAction::Submit("one\ntwo".into())
        );
    }

    #[test]
    fn word_deletion_and_line_kills() {
        let mut editor = typed("git commit -m");
        editor.key(ctrl('w'));
        assert_eq!(editor.text(), "git commit -");
        editor.key(ctrl('a'));
        editor.key(ctrl('k'));
        assert_eq!(editor.text(), "");
        let mut editor = typed("abc def");
        editor.key(ctrl('u'));
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn history_walks_back_and_restores_the_draft() {
        let mut editor = Editor::new(None);
        editor.remember("first");
        editor.remember("second");
        editor.remember("second");
        editor.insert_str("draft");
        editor.key(key(KeyCode::Up));
        assert_eq!(editor.text(), "second");
        editor.key(key(KeyCode::Up));
        assert_eq!(editor.text(), "first");
        editor.key(key(KeyCode::Up));
        assert_eq!(editor.text(), "first");
        editor.key(key(KeyCode::Down));
        editor.key(key(KeyCode::Down));
        assert_eq!(editor.text(), "draft");
    }

    #[test]
    fn up_and_down_move_inside_a_multi_line_message() {
        let mut editor = typed("abcd\nxy");
        editor.key(key(KeyCode::Up));
        editor.insert_str("!");
        assert_eq!(editor.text(), "ab!cd\nxy");
    }

    #[test]
    fn render_wraps_and_places_the_cursor() {
        let editor = typed("abcdefgh");
        let (rows, cursor) = editor.render(6, &Line::plain("> "), "");
        let texts: Vec<String> = rows.iter().map(Line::text).collect();
        assert_eq!(texts, vec!["> abcd", "  efgh", "  "]);
        assert_eq!(cursor, (2, 2));
        let editor = typed("ab\ncd");
        let (rows, cursor) = editor.render(20, &Line::plain("> "), "");
        assert_eq!(rows.len(), 2);
        assert_eq!(cursor, (1, 4));
    }

    #[test]
    fn paste_folds_carriage_returns() {
        let editor = typed("a\r\nb\rc");
        assert_eq!(editor.text(), "a\nb\nc");
    }
}
