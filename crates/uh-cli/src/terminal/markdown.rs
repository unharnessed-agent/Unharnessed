//! A small line-by-line Markdown renderer for the streamed answer. The
//! answer arrives in pieces, so the renderer works one complete line at a
//! time and only keeps the code-fence state between lines. A marker that
//! is not closed on its line stays literal, so a half-streamed line renders
//! the same way it will once complete.

use super::screen::{text_width, wrap, Color, Line, Style};

const INDENT: &str = "  ";

/// The renderer state carried from one line to the next.
#[derive(Clone, Debug, Default)]
pub(crate) struct Markdown {
    fence: Option<String>,
}

impl Markdown {
    /// Render one complete source line into wrapped rows.
    pub fn line(&mut self, src: &str, width: usize) -> Vec<Line> {
        let trimmed = src.trim_start();
        if let Some(fence) = &self.fence {
            if trimmed.starts_with(fence.as_str())
                && trimmed
                    .trim_end()
                    .chars()
                    .all(|c| c == fence.chars().next().unwrap_or('`'))
            {
                self.fence = None;
                return Vec::new();
            }
            return code_line(src, width);
        }
        if let Some(fence) = fence_marker(trimmed) {
            self.fence = Some(fence);
            return Vec::new();
        }
        block_line(src, width)
    }

    /// Render a line that is still streaming, without moving the state.
    pub fn preview(&self, src: &str, width: usize) -> Vec<Line> {
        self.clone().line(src, width)
    }

    #[cfg(test)]
    pub fn in_code_block(&self) -> bool {
        self.fence.is_some()
    }
}

fn fence_marker(trimmed: &str) -> Option<String> {
    for marker in ["```", "~~~"] {
        if trimmed.starts_with(marker) {
            let ch = marker.chars().next().unwrap_or('`');
            let run: String = trimmed.chars().take_while(|c| *c == ch).collect();
            return Some(run);
        }
    }
    None
}

fn code_line(src: &str, width: usize) -> Vec<Line> {
    let gutter = Line::styled(format!("{INDENT}▏ "), Style::dim());
    let text = src.replace('\t', "    ");
    let line = Line::styled(text, Style::fg(Color::Yellow));
    wrap(&line, width, &gutter, &gutter)
}

fn block_line(src: &str, width: usize) -> Vec<Line> {
    let trimmed = src.trim();
    if trimmed.is_empty() {
        return vec![Line::new()];
    }
    if is_rule(trimmed) {
        let len = width.saturating_sub(INDENT.len()).min(48);
        return vec![Line::plain(INDENT).with("─".repeat(len), Style::dim())];
    }
    if let Some((level, text)) = heading(trimmed) {
        let style = if level <= 2 {
            Style::fg(Color::Accent).bold()
        } else {
            Style::PLAIN.bold()
        };
        let line = inline(text, style);
        return wrap(&line, width, &Line::plain(INDENT), &Line::plain(INDENT));
    }
    if trimmed.starts_with('|') {
        return vec![table_row(trimmed).truncate(width)];
    }
    if let Some(rest) = quote(src) {
        let bar = Line::plain(INDENT).with("│ ", Style::dim());
        let line = inline(rest, Style::PLAIN.italic());
        return wrap(&line, width, &bar, &bar);
    }
    if let Some((depth, marker, rest)) = list_item(src) {
        let pad = " ".repeat(depth * 2);
        let first =
            Line::plain(format!("{INDENT}{pad}")).with(marker.clone(), Style::fg(Color::Accent));
        let hang = Line::plain(format!("{INDENT}{pad}{}", " ".repeat(text_width(&marker))));
        let line = inline(rest, Style::PLAIN);
        return wrap(&line, width, &first, &hang);
    }
    let line = inline(trimmed, Style::PLAIN);
    wrap(&line, width, &Line::plain(INDENT), &Line::plain(INDENT))
}

fn is_rule(trimmed: &str) -> bool {
    let compact: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    compact.len() >= 3
        && ['-', '*', '_']
            .iter()
            .any(|marker| compact.chars().all(|c| c == *marker))
}

fn heading(trimmed: &str) -> Option<(usize, &str)> {
    let level = trimmed.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&level) {
        let rest = &trimmed[level..];
        if rest.starts_with(' ') {
            return Some((level, rest.trim().trim_end_matches('#').trim_end()));
        }
    }
    None
}

fn quote(src: &str) -> Option<&str> {
    let trimmed = src.trim_start();
    let rest = trimmed.strip_prefix('>')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

/// A list item: (nesting depth, rendered marker, text).
fn list_item(src: &str) -> Option<(usize, String, &str)> {
    let spaces = src.len() - src.trim_start().len();
    let depth = spaces / 2;
    let trimmed = src.trim_start();
    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(bullet) {
            let rest = rest.trim_start();
            if let Some(task) = rest.strip_prefix("[ ] ") {
                return Some((depth, "☐ ".to_string(), task));
            }
            if let Some(task) = rest
                .strip_prefix("[x] ")
                .or_else(|| rest.strip_prefix("[X] "))
            {
                return Some((depth, "☑ ".to_string(), task));
            }
            return Some((depth, "• ".to_string(), rest));
        }
    }
    let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() && digits.len() <= 4 {
        let after = &trimmed[digits.len()..];
        for close in [". ", ") "] {
            if let Some(rest) = after.strip_prefix(close) {
                return Some((depth, format!("{digits}. "), rest.trim_start()));
            }
        }
    }
    None
}

fn table_row(trimmed: &str) -> Line {
    let is_separator = trimmed.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '));
    if is_separator {
        let width = text_width(trimmed);
        return Line::plain(INDENT).with("─".repeat(width), Style::dim());
    }
    let mut line = Line::plain(INDENT);
    let cells: Vec<&str> = trimmed.trim_matches('|').split('|').collect();
    line.push("│", Style::dim());
    for cell in cells {
        line.push(" ", Style::PLAIN);
        line.append(&inline(cell.trim(), Style::PLAIN));
        line.push(" │", Style::dim());
    }
    line
}

/// Render inline Markdown (code, bold, italic, strike, links) in `base`.
pub(crate) fn inline(text: &str, base: Style) -> Line {
    let chars: Vec<char> = text.chars().collect();
    let mut line = Line::new();
    let mut bold = false;
    let mut italic = false;
    let mut strike = false;
    let mut i = 0;
    let style = |bold: bool, italic: bool, strike: bool| {
        let mut style = base;
        if bold {
            style = style.bold();
        }
        if italic {
            style = style.italic();
        }
        if strike {
            style = style.dimmed();
        }
        style
    };
    while i < chars.len() {
        let c = chars[i];
        // Backslash escapes.
        if c == '\\' && i + 1 < chars.len() && chars[i + 1].is_ascii_punctuation() {
            line.push(chars[i + 1].to_string(), style(bold, italic, strike));
            i += 2;
            continue;
        }
        // Inline code: a run of backticks closed by the same run.
        if c == '`' {
            let run = count(&chars, i, '`');
            if let Some(end) = find_run(&chars, i + run, '`', run) {
                let code: String = chars[i + run..end].iter().collect();
                let code =
                    if code.starts_with(' ') && code.ends_with(' ') && !code.trim().is_empty() {
                        code[1..code.len() - 1].to_string()
                    } else {
                        code
                    };
                line.push(code, Style::fg(Color::Yellow));
                i = end + run;
                continue;
            }
            line.push("`".repeat(run), style(bold, italic, strike));
            i += run;
            continue;
        }
        // Links: [text](url).
        if c == '[' {
            if let Some((label, url, next)) = link(&chars, i) {
                let label_line = inline(&label, Style::fg(Color::Accent).underline());
                line.append(&label_line);
                if url != label && !url.is_empty() {
                    line.push(format!(" ({url})"), Style::dim());
                }
                i = next;
                continue;
            }
        }
        // Bold and strike: double markers that close later on the line.
        if (c == '*' || c == '_' || c == '~') && i + 1 < chars.len() && chars[i + 1] == c {
            let opening = if c == '~' { !strike } else { !bold };
            let can_open = opening
                && i + 2 < chars.len()
                && !chars[i + 2].is_whitespace()
                && find_double(&chars, i + 2, c).is_some()
                && (c != '_' || i == 0 || !chars[i - 1].is_alphanumeric());
            let can_close = !opening && i > 0 && !chars[i - 1].is_whitespace();
            if can_open || can_close {
                if c == '~' {
                    strike = !strike;
                } else {
                    bold = !bold;
                }
                i += 2;
                continue;
            }
            // A double marker that neither opens nor closes stays as typed.
            if c != '~' || !strike {
                line.push(format!("{c}{c}"), style(bold, italic, strike));
                i += 2;
                continue;
            }
        }
        // Italic: a single marker that closes later on the line; `_` only
        // at word edges, so snake_case stays as written.
        if c == '*' || c == '_' {
            let next_is_word = i + 1 < chars.len() && !chars[i + 1].is_whitespace();
            let prev_is_word = i > 0 && chars[i - 1].is_alphanumeric();
            if italic {
                let next_is_alnum = i + 1 < chars.len() && chars[i + 1].is_alphanumeric();
                let can_close =
                    i > 0 && !chars[i - 1].is_whitespace() && (c == '*' || !next_is_alnum);
                if can_close {
                    italic = false;
                    i += 1;
                    continue;
                }
            } else {
                let can_open = next_is_word
                    && (c == '*' || !prev_is_word)
                    && find_single(&chars, i + 1, c).is_some();
                if can_open {
                    italic = true;
                    i += 1;
                    continue;
                }
            }
        }
        line.push(c.to_string(), style(bold, italic, strike));
        i += 1;
    }
    line
}

fn count(chars: &[char], start: usize, ch: char) -> usize {
    chars[start..].iter().take_while(|c| **c == ch).count()
}

/// The index of the next run of exactly `len` copies of `ch` from `start`.
fn find_run(chars: &[char], start: usize, ch: char, len: usize) -> Option<usize> {
    let mut i = start;
    while i < chars.len() {
        if chars[i] == ch {
            let run = count(chars, i, ch);
            if run == len {
                return Some(i);
            }
            i += run;
        } else {
            i += 1;
        }
    }
    None
}

fn find_double(chars: &[char], start: usize, ch: char) -> Option<usize> {
    (start..chars.len().saturating_sub(1))
        .find(|&i| chars[i] == ch && chars[i + 1] == ch && i > 0 && !chars[i - 1].is_whitespace())
}

fn find_single(chars: &[char], start: usize, ch: char) -> Option<usize> {
    (start..chars.len()).find(|&i| {
        chars[i] == ch
            && !chars[i - 1].is_whitespace()
            && (i + 1 >= chars.len() || chars[i + 1] != ch)
            && (ch == '*' || i + 1 >= chars.len() || !chars[i + 1].is_alphanumeric())
    })
}

/// `[label](url)` starting at `start`: (label, url, index after it).
fn link(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    let close = (start + 1..chars.len()).find(|&i| chars[i] == ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = (close + 2..chars.len()).find(|&i| chars[i] == ')')?;
    let label: String = chars[start + 1..close].iter().collect();
    let url: String = chars[close + 2..end].iter().collect();
    if label.is_empty() || url.contains(' ') {
        return None;
    }
    Some((label, url, end + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(src: &str) -> Vec<String> {
        let mut md = Markdown::default();
        src.lines()
            .flat_map(|line| md.line(line, 40))
            .map(|line| line.text())
            .collect()
    }

    fn spans(text: &str) -> Vec<(String, Style)> {
        inline(text, Style::PLAIN)
            .0
            .into_iter()
            .map(|span| (span.text, span.style))
            .collect()
    }

    #[test]
    fn bold_and_code_lose_their_markers() {
        assert_eq!(
            render("Il y a **312** fichiers dans `crates`."),
            vec!["  Il y a 312 fichiers dans crates."]
        );
        let parts = spans("a **b** c");
        assert_eq!(parts[1], ("b".to_string(), Style::PLAIN.bold()));
    }

    #[test]
    fn an_unclosed_marker_stays_literal() {
        assert_eq!(
            render("2 * 3 = 6 and **open"),
            vec!["  2 * 3 = 6 and **open"]
        );
        assert_eq!(render("a `tick"), vec!["  a `tick"]);
    }

    #[test]
    fn snake_case_is_not_italic() {
        assert_eq!(
            render("call my_func_name now"),
            vec!["  call my_func_name now"]
        );
        let parts = spans("an _emphasis_ here");
        assert_eq!(parts[1], ("emphasis".to_string(), Style::PLAIN.italic()));
    }

    #[test]
    fn star_italic_and_bold_nest() {
        let parts = spans("***both*** x");
        assert!(parts
            .iter()
            .any(|(text, style)| text == "both" && style.bold && style.italic));
    }

    #[test]
    fn code_spans_keep_markers_inside() {
        let parts = spans("run `a*b*c` ok");
        assert_eq!(parts[1], ("a*b*c".to_string(), Style::fg(Color::Yellow)));
        let parts = spans("``has ` tick``");
        assert_eq!(parts[0].0, "has ` tick");
    }

    #[test]
    fn links_show_the_label_and_the_address() {
        assert_eq!(
            render("see [docs](https://x.dev) now"),
            vec!["  see docs (https://x.dev) now"]
        );
        assert_eq!(render("[x] not a link"), vec!["  [x] not a link"]);
    }

    #[test]
    fn headings_rules_and_quotes() {
        assert_eq!(render("## Title ##"), vec!["  Title"]);
        assert_eq!(render("---")[0].trim_start().chars().next(), Some('─'));
        assert_eq!(render("> quoted *text*"), vec!["  │ quoted text"]);
        assert_eq!(render("#hashtag"), vec!["  #hashtag"]);
    }

    #[test]
    fn lists_get_bullets_and_hanging_indents() {
        assert_eq!(
            render("- first\n  - nested\n3. third\n- [x] done"),
            vec!["  • first", "    • nested", "  3. third", "  ☑ done",]
        );
        let long = render("- one two three four five six seven eight nine ten");
        assert_eq!(
            long,
            vec![
                "  • one two three four five six seven",
                "    eight nine ten"
            ]
        );
    }

    #[test]
    fn code_fences_are_hidden_and_their_lines_kept_verbatim() {
        assert_eq!(
            render("```python\nx = **y**\n```\nafter"),
            vec!["  ▏ x = **y**", "  after",]
        );
        let mut md = Markdown::default();
        md.line("```", 40);
        assert!(md.in_code_block());
        md.line("```", 40);
        assert!(!md.in_code_block());
    }

    #[test]
    fn a_longer_fence_is_not_closed_by_a_shorter_one() {
        assert_eq!(render("````\n```\n````\nx"), vec!["  ▏ ```", "  x"]);
    }

    #[test]
    fn tables_get_box_bars() {
        assert_eq!(
            render("| a | b |\n|---|---|\n| **1** | 2 |"),
            vec!["  │ a │ b │", "  ─────────", "  │ 1 │ 2 │",]
        );
    }

    #[test]
    fn preview_does_not_move_the_fence_state() {
        let md = Markdown::default();
        md.preview("```", 40);
        assert!(!md.in_code_block());
    }

    #[test]
    fn escapes_and_blank_lines() {
        assert_eq!(render(r"\*not italic\*"), vec!["  *not italic*"]);
        let mut md = Markdown::default();
        let blank: Vec<String> = md.line("", 40).iter().map(Line::text).collect();
        assert_eq!(blank, vec![""]);
    }
}
