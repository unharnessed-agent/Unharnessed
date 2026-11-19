//! The list picker the commands open (`/model`, `/login`, `/resume`, ...):
//! type to filter, arrows to move, Enter to choose, Esc to close. With
//! `multi`, Space toggles items and Enter confirms the ticked set.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::screen::{text_width, Color, Line, Style};

#[derive(Clone, Debug)]
pub(crate) struct Item {
    pub label: String,
    pub detail: String,
    /// A mark in front of the label (the current model, a connected
    /// provider).
    pub mark: bool,
}

impl Item {
    pub fn new(label: impl Into<String>, detail: impl Into<String>) -> Item {
        Item {
            label: label.into(),
            detail: detail.into(),
            mark: false,
        }
    }

    pub fn marked(mut self, mark: bool) -> Item {
        self.mark = mark;
        self
    }
}

pub(crate) enum PickerAction {
    Continue,
    /// The chosen item indexes (one, or the ticked set with `multi`).
    Done(Vec<usize>),
    Cancel,
}

pub(crate) struct Picker {
    pub title: String,
    pub hint: String,
    items: Vec<Item>,
    filter: String,
    /// Index into the filtered list.
    selected: usize,
    /// The ticked items, for a multi-select picker.
    ticked: Option<Vec<bool>>,
    mark_symbol: &'static str,
}

impl Picker {
    pub fn new(title: impl Into<String>, items: Vec<Item>) -> Picker {
        let selected_item = items.iter().position(|item| item.mark).unwrap_or(0);
        let mut picker = Picker {
            title: title.into(),
            hint: String::new(),
            items,
            filter: String::new(),
            selected: 0,
            ticked: None,
            mark_symbol: "●",
        };
        picker.selected = picker
            .visible()
            .iter()
            .position(|index| *index == selected_item)
            .unwrap_or(0);
        picker
    }

    pub fn mark_symbol(mut self, symbol: &'static str) -> Picker {
        self.mark_symbol = symbol;
        self
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Picker {
        self.hint = hint.into();
        self
    }

    /// A multi-select picker with every item ticked as `ticked` says.
    pub fn multi(mut self, ticked: Vec<bool>) -> Picker {
        self.ticked = Some(ticked);
        self
    }

    pub fn with_filter(mut self, filter: &str) -> Picker {
        self.filter = filter.to_string();
        self.selected = 0;
        self
    }

    /// The item indexes that match the filter, best first.
    fn visible(&self) -> Vec<usize> {
        if self.filter.trim().is_empty() {
            return (0..self.items.len()).collect();
        }
        let indexes: Vec<usize> = (0..self.items.len()).collect();
        crate::fuzzy::fuzzy_filter(&indexes, &self.filter, |index| {
            let item = &self.items[*index];
            format!("{} {}", item.label, item.detail)
        })
    }

    pub fn key(&mut self, key: KeyEvent) -> PickerAction {
        let visible = self.visible();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return PickerAction::Cancel,
            KeyCode::Char('c') if ctrl => return PickerAction::Cancel,
            KeyCode::Up => {
                self.selected = self
                    .selected
                    .checked_sub(1)
                    .unwrap_or(visible.len().saturating_sub(1));
            }
            KeyCode::Char('p') if ctrl => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => {
                self.selected = if self.selected + 1 >= visible.len() {
                    0
                } else {
                    self.selected + 1
                };
            }
            KeyCode::Char('n') if ctrl => {
                self.selected = (self.selected + 1).min(visible.len().saturating_sub(1));
            }
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(8),
            KeyCode::PageDown => {
                self.selected = (self.selected + 8).min(visible.len().saturating_sub(1));
            }
            KeyCode::Char(' ') if self.ticked.is_some() => {
                if let (Some(index), Some(ticked)) =
                    (visible.get(self.selected), self.ticked.as_mut())
                {
                    ticked[*index] = !ticked[*index];
                }
            }
            KeyCode::Char('a') if ctrl && self.ticked.is_some() => {
                if let Some(ticked) = self.ticked.as_mut() {
                    let all = visible.iter().all(|index| ticked[*index]);
                    for index in &visible {
                        ticked[*index] = !all;
                    }
                }
            }
            KeyCode::Enter => {
                if let Some(ticked) = &self.ticked {
                    let chosen: Vec<usize> = (0..self.items.len())
                        .filter(|index| ticked[*index])
                        .collect();
                    if !chosen.is_empty() {
                        return PickerAction::Done(chosen);
                    }
                    return PickerAction::Continue;
                }
                if let Some(index) = visible.get(self.selected) {
                    return PickerAction::Done(vec![*index]);
                }
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
            }
            KeyCode::Char('u') if ctrl => {
                self.filter.clear();
                self.selected = 0;
            }
            KeyCode::Char(ch) if !ctrl => {
                self.filter.push(ch);
                self.selected = 0;
            }
            _ => {}
        }
        PickerAction::Continue
    }

    /// The picker rows, at most `max_rows` of them.
    pub fn render(&self, width: usize, max_rows: usize) -> (Vec<Line>, (usize, usize)) {
        let visible = self.visible();
        let mut out = Vec::new();
        out.push(Line::plain("  ").with(self.title.clone(), Style::fg(Color::Accent).bold()));
        let mut search = Line::plain("  ").with("⌕ ", Style::dim());
        let cursor_col = search.width() + text_width(&self.filter);
        if self.filter.is_empty() {
            search.push("type to filter", Style::dim());
        } else {
            search.push(self.filter.clone(), Style::PLAIN);
        }
        out.push(search.truncate(width));
        let room = max_rows.saturating_sub(4).clamp(3, 12);
        let start = if self.selected >= room {
            self.selected + 1 - room
        } else {
            0
        };
        let label_width = visible
            .iter()
            .map(|index| text_width(&self.items[*index].label))
            .max()
            .unwrap_or(0)
            .min(width / 2);
        if visible.is_empty() {
            out.push(Line::styled("    nothing matches", Style::dim()));
        }
        for (row, index) in visible.iter().enumerate().skip(start).take(room) {
            let item = &self.items[*index];
            let selected = row == self.selected;
            let mut line = Line::new();
            line.push(
                if selected { "  ❯ " } else { "    " },
                Style::fg(Color::Accent),
            );
            if let Some(ticked) = &self.ticked {
                if ticked[*index] {
                    line.push("◉ ", Style::fg(Color::Green));
                } else {
                    line.push("○ ", Style::dim());
                }
            } else if item.mark {
                line.push(format!("{} ", self.mark_symbol), Style::fg(Color::Green));
            } else {
                line.push("  ", Style::PLAIN);
            }
            let label_style = if selected {
                Style::PLAIN.bold()
            } else {
                Style::PLAIN
            };
            line.push(item.label.clone(), label_style);
            if !item.detail.is_empty() {
                let pad = label_width.saturating_sub(text_width(&item.label)) + 2;
                line.push(" ".repeat(pad), Style::PLAIN);
                line.push(item.detail.clone(), Style::dim());
            }
            out.push(line.truncate(width));
        }
        let more = visible.len().saturating_sub(start + room);
        let mut footer = Line::plain("  ");
        if more > 0 {
            footer.push(format!("↓ {more} more · "), Style::dim());
        }
        let keys = if self.ticked.is_some() {
            "space tick · ctrl-a all · enter confirm · esc cancel"
        } else {
            "↑↓ move · enter choose · esc cancel"
        };
        footer.push(keys, Style::dim());
        if !self.hint.is_empty() {
            footer.push(format!(" · {}", self.hint), Style::dim());
        }
        out.push(footer.truncate(width));
        (out, (1, cursor_col))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(picker: &mut Picker, code: KeyCode) -> PickerAction {
        picker.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn items() -> Vec<Item> {
        vec![
            Item::new("claude-haiku-4-5", "anthropic"),
            Item::new("gpt-5-mini", "openai").marked(true),
            Item::new("hermes-4-70b", "nous"),
        ]
    }

    #[test]
    fn starts_on_the_marked_item() {
        let mut picker = Picker::new("Model", items());
        assert!(
            matches!(press(&mut picker, KeyCode::Enter), PickerAction::Done(v) if v == vec![1])
        );
    }

    #[test]
    fn typing_filters_and_enter_picks_the_best_match() {
        let mut picker = Picker::new("Model", items());
        for ch in "herm".chars() {
            press(&mut picker, KeyCode::Char(ch));
        }
        assert!(
            matches!(press(&mut picker, KeyCode::Enter), PickerAction::Done(v) if v == vec![2])
        );
    }

    #[test]
    fn arrows_wrap_around() {
        let mut picker = Picker::new("Model", items()).with_filter("");
        press(&mut picker, KeyCode::Up);
        assert!(
            matches!(press(&mut picker, KeyCode::Enter), PickerAction::Done(v) if v == vec![2])
        );
    }

    #[test]
    fn multi_select_ticks_with_space() {
        let mut picker = Picker::new("Models", items()).multi(vec![false, false, false]);
        assert!(matches!(
            press(&mut picker, KeyCode::Enter),
            PickerAction::Continue
        ));
        press(&mut picker, KeyCode::Char(' '));
        press(&mut picker, KeyCode::Down);
        press(&mut picker, KeyCode::Char(' '));
        assert!(
            matches!(press(&mut picker, KeyCode::Enter), PickerAction::Done(v) if v == vec![1, 2])
        );
    }
}
