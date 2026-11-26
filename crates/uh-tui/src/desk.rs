//! The terminal as the conversation's screen, for a replay and for a live
//! session alike: raw mode, the alternate screen, the mouse and the pastes;
//! what the user writes in the message box (with the files an `@` offers,
//! the commands a `/` offers, the messages sent before, and an earlier
//! message brought back to be changed), what they scroll (the conversation, or
//! the kernel's panel under the pointer) and open; and the
//! frames, of which only one that differs from the last reaches the
//! terminal.

use std::io;

use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, KeyCode,
    KeyEvent, KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ratatui::crossterm::ExecutableCommand;

use crate::canvas::Canvas;
use crate::chat::Chat;
use crate::connect::Spot;
use crate::draft::Draft;
use crate::fold::{Fold, Folds};
use crate::mention::{self, mentions};
use crate::message::Message;
use crate::motion::{FADE_IN, FOLD, LANDS};
use crate::past::Past;
use crate::slash::{self, Kind, Slash};
use crate::theme::Depth;
use crate::view::{self, Edited, Editing, Kept, Menu, Screen, Ui, MENU_ROWS};

/// How many rows a notch of the wheel scrolls.
const WHEEL: usize = 3;
/// The frames' pace while something moves, and the least time between two
/// when the user acts (a pointer on its way must not hurry them), in ms.
pub const FRAME: u64 = 33;
pub const FRAME_LEAST: u64 = 16;

/// Takes the terminal: raw mode, the alternate screen, the mouse reported
/// and the pastes marked. A panic gives it back too (the process ends with
/// the screen: the hook stays).
///
/// # Errors
///
/// The terminal refused the mouse or the pastes: it is given back first.
pub fn enter() -> io::Result<ratatui::DefaultTerminal> {
    // The screen is the application's own: its surfaces are colours, so they
    // are drawn even where NO_COLOR asks command output to stay plain.
    ratatui::crossterm::style::force_color_output(true);
    let terminal = ratatui::init();
    let restore = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        release();
        restore(info);
    }));
    // A paste comes whole, its line ends not taken for Enter.
    let taken = io::stdout()
        .execute(EnableMouseCapture)
        .and_then(|out| out.execute(EnableBracketedPaste))
        .map(|_| ());
    if let Err(error) = taken {
        leave();
        return Err(error);
    }
    // Enter with Ctrl or Shift told apart from Enter, where the terminal
    // can (the kitty keyboard protocol); the others take no notice.
    let _ = io::stdout().execute(PushKeyboardEnhancementFlags(
        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
    ));
    Ok(terminal)
}

/// Gives the terminal back as it was.
pub fn leave() {
    release();
    ratatui::restore();
}

/// The terminal stops reporting the mouse and marking the pastes.
fn release() {
    let _ = io::stdout().execute(PopKeyboardEnhancementFlags);
    let _ = io::stdout().execute(DisableMouseCapture);
    let _ = io::stdout().execute(DisableBracketedPaste);
}

/// What a click landed on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Click {
    /// A line that folds: it opened, or closed.
    Fold(Fold),
    /// The message of that turn: it is to be changed.
    Edit(usize),
    /// The mark that stops the agent.
    Stop,
    /// A row or a button of a screen, what leads to one, or a row of a
    /// menu over the message box.
    Spot(Spot),
    /// Beside the menu open over the message box: it closes.
    Away,
}

/// What a dismissal of the menu over the message box holds for, the
/// message being `text`: the text; while a command's text is written
/// (`written` bytes of it, at the end), the command, whatever is written
/// for it.
fn dismissal(text: &str, written: Option<usize>) -> &str {
    &text[..text.len() - written.unwrap_or(0).min(text.len())]
}

/// A key that edits `draft`: what is typed is written, the arrows move the
/// caret. A message takes line breaks (`lines`); a field is one line.
/// Answers whether the key was one of them.
pub fn edit(draft: &mut Draft, key: KeyEvent, lines: bool) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Home => draft.home(),
        KeyCode::End => draft.end(),
        KeyCode::Char('a') if ctrl => draft.home(),
        KeyCode::Char('e') if ctrl => draft.end(),
        KeyCode::Left if alt || ctrl => draft.word_left(),
        KeyCode::Right if alt || ctrl => draft.word_right(),
        KeyCode::Char('b') if alt => draft.word_left(),
        KeyCode::Char('f') if alt => draft.word_right(),
        KeyCode::Left => draft.left(),
        KeyCode::Right => draft.right(),
        KeyCode::Backspace if alt => draft.delete_word(),
        KeyCode::Char('w') if ctrl => draft.delete_word(),
        KeyCode::Char('u') if ctrl => draft.delete_to_start(),
        KeyCode::Backspace => draft.backspace(),
        KeyCode::Char('h') if ctrl => draft.backspace(),
        KeyCode::Delete => draft.delete(),
        KeyCode::Char('d') if ctrl => draft.delete(),
        // A line break in the message.
        KeyCode::Enter if lines && (alt || shift) => draft.insert("\n"),
        KeyCode::Char('j') if lines && ctrl => draft.insert("\n"),
        KeyCode::Char(ch) if !ctrl && !alt => draft.insert(ch.encode_utf8(&mut [0; 4])),
        _ => return false,
    }
    true
}

/// The screen as the user drives it.
pub struct Desk {
    pub ui: Ui,
    /// What the terminal shows, and where the pointer is on it.
    screen: Screen,
    pointer: Option<(u16, u16)>,
    /// The terminal's rows.
    height: u16,
    /// The frame the terminal shows.
    shown: Option<Canvas>,
    /// What the last frame laid out, for the next.
    kept: Kept,
    depth: Depth,
    /// The line opened in the view's last rows, while what it opens grows.
    revealing: Option<Reveal>,
}

/// How few rows of the view under a line clicked open let it rise to show
/// what it opens.
const REVEAL: usize = 4;

/// A line clicked open where what it opens would not show: its row in the
/// conversation, the first row in view then, and how many rows the
/// conversation had.
#[derive(Clone, Copy)]
struct Reveal {
    line: usize,
    first: usize,
    length: usize,
}

impl Desk {
    #[must_use]
    pub fn new(ui: Ui) -> Desk {
        Desk {
            ui,
            screen: Screen::default(),
            pointer: None,
            height: 0,
            shown: None,
            kept: Kept::default(),
            depth: Depth::detect(),
            revealing: None,
        }
    }

    /// Another conversation takes the screen: nothing of what the user
    /// set on the one before stays, but what is being written.
    pub fn fresh(&mut self) {
        self.keep();
        self.kept = Kept::default();
        self.revealing = None;
        let ui = &mut self.ui;
        ui.folds = Folds::default();
        ui.past = Past::default();
        ui.waiting.clear();
        ui.rushed.clear();
        (ui.top, ui.hover, ui.history, ui.pick) = (None, None, None, None);
        ui.panel_top = 0;
    }

    /// A message, `written`, comes back to the message box to be changed:
    /// one sent before, whose turn comes into view, or one that waits,
    /// at which the line then waits. What was being written there is kept
    /// for after.
    pub fn edit(&mut self, of: Edited, written: Message) {
        let ui = &mut self.ui;
        // From one message to another, what is kept is still what was being written.
        let saved = match ui.editing.take() {
            Some(editing) => editing.saved,
            None => Message {
                text: ui.draft.take(),
                images: std::mem::take(&mut ui.images),
            },
        };
        for waiting in &mut ui.waiting {
            waiting.held = of == Edited::Waiting(waiting.id);
        }
        ui.draft = Draft::holding(&written.text);
        ui.images = written.images;
        ui.editing = Some(Editing { of, saved });
        ui.past.leave();
        let Edited::Turn(turn) = of else {
            self.suggest();
            return;
        };
        let row = (self.screen.turns.iter()).find(|(index, _)| *index == turn);
        if let Some((_, row)) = row {
            let first = self.screen.first;
            if *row < first || *row + 3 > first + self.screen.rows {
                self.show(row.saturating_sub(1));
            }
        }
        self.suggest();
    }

    /// The message being changed is left as it was: what was being
    /// written before comes back to the message box, the line no longer
    /// waits at it. Answers the message that was in the box, when one was
    /// being changed.
    pub fn keep(&mut self) -> Option<Message> {
        let ui = &mut self.ui;
        let editing = ui.editing.take()?;
        for waiting in &mut ui.waiting {
            waiting.held = false;
        }
        let written = Message {
            text: ui.draft.take(),
            images: std::mem::replace(&mut ui.images, editing.saved.images),
        };
        ui.draft = Draft::holding(&editing.saved.text);
        ui.past.leave();
        self.suggest();
        Some(written)
    }

    /// The conversation went back to before turn `turn`: nothing set on
    /// that turn and the ones after stays.
    pub fn forget_from(&mut self, turn: usize) {
        self.kept = Kept::default();
        self.ui.folds.forget_from(turn);
    }

    /// The conversation's row `to` comes first in view, or that of the
    /// screen in its place.
    pub fn show(&mut self, to: usize) {
        self.revealing = None;
        let to = to.min(self.screen.most);
        // Several moves may come before the next frame: each starts from the last.
        self.screen.first = to;
        if let Some(connect) = &mut self.ui.connect {
            connect.show(to);
            return;
        }
        if let Some(prefs) = &mut self.ui.prefs {
            prefs.show(to);
            return;
        }
        if let Some(history) = &mut self.ui.history {
            history.show(to);
            return;
        }
        // At its end, it follows what comes.
        self.ui.top = (to < self.screen.most).then_some(to);
    }

    /// The conversation `rows` down (up, when negative) from where it shows.
    pub fn scroll(&mut self, rows: isize) {
        self.show(self.screen.first.saturating_add_signed(rows));
    }

    /// A key for the message box or the conversation's place: what is typed
    /// is written, the arrows move the caret, bring back the messages sent
    /// before, or scroll. Answers whether the key was one of them.
    pub fn write(&mut self, key: KeyEvent) -> bool {
        let page = self.page();
        let width = self.screen.draft;
        let ui = &mut self.ui;
        match key.code {
            KeyCode::PageUp => self.scroll(-page),
            KeyCode::PageDown => self.scroll(page),
            // In a message of several rows the arrows move its caret; from
            // its first row Up goes back through the messages sent before,
            // and Down returns; with none sent, they scroll the conversation.
            KeyCode::Up => {
                if ui.draft.vertical(-1, width) {
                } else if ui.past.is_empty() {
                    self.scroll(-1);
                } else if let Some(before) = ui.past.older(ui.draft.text()) {
                    ui.draft = Draft::holding(before);
                }
            }
            KeyCode::Down => {
                if ui.draft.vertical(1, width) {
                } else if ui.past.is_empty() {
                    self.scroll(1);
                } else if let Some(after) = ui.past.newer() {
                    ui.draft = Draft::holding(&after);
                }
            }
            KeyCode::Home if ui.draft.is_empty() => self.show(0),
            KeyCode::End if ui.draft.is_empty() => self.show(usize::MAX),
            _ => {
                let was = ui.draft.text().len();
                // A mention goes whole: one backspace right after it.
                let whole = (key.code == KeyCode::Backspace && key.modifiers.is_empty())
                    .then_some(ui.files.as_deref())
                    .flatten()
                    .and_then(|files| {
                        (mentions(ui.draft.text(), |path| files.knows(path)).into_iter())
                            .find(|mention| mention.end == ui.draft.caret())
                    });
                let known = match whole {
                    Some(mention) => {
                        ui.draft.cut(mention.start, mention.end);
                        true
                    }
                    None => edit(&mut ui.draft, key, true),
                };
                // A message brought back and changed is the one being written.
                if ui.draft.text().len() != was {
                    ui.past.leave();
                }
                return known;
            }
        }
        true
    }

    /// What was pasted goes into the message.
    pub fn paste(&mut self, text: &str) {
        self.ui.draft.insert(text);
        self.ui.past.leave();
        self.suggest();
    }

    /// The menus over the message box follow the message. One that starts
    /// with a `/` opens the commands, prompts and skills that match what
    /// is typed; once a space follows a command's name, its row stays
    /// alone. The `@` word the caret is in opens the files that match what
    /// is typed of it, also in a command's text. A menu the user dismissed
    /// stays shut until the message changes; a message brought back is
    /// not one being written, and opens none of the commands.
    pub fn suggest(&mut self) {
        let ui = &mut self.ui;
        let text = ui.draft.text();
        let offer = (ui.commands.as_deref())
            .filter(|_| !ui.past.browsing())
            .and_then(|commands| slash::offer(text, commands));
        let listing = offer.as_ref().is_some_and(|offer| offer.written.is_none());
        let word = (ui.live && !listing)
            .then(|| mention::word_at(text, ui.draft.caret()))
            .flatten();
        let written = offer.as_ref().and_then(|offer| offer.written);
        let key = dismissal(text, written.filter(|_| word.is_none()));
        if ui.shut.as_deref().is_some_and(|shut| shut != key) {
            ui.shut = None;
        }
        if ui.shut.is_some() {
            (ui.menu, ui.slash) = (None, None);
            return;
        }
        ui.slash = offer.filter(|_| word.is_none()).map(|offer| {
            // The same offer keeps its place; another starts at its first command.
            let same = (ui.slash.as_ref()).filter(|slash| slash.found == offer.found);
            let (at, top) = same.map_or((0, 0), |slash| (slash.at, slash.top));
            Slash {
                found: offer.found,
                written: offer.written,
                at,
                top,
            }
        });
        ui.menu = word.map(|word| {
            let found = (ui.files.as_ref()).map_or_else(Vec::new, |files| files.find(&word.query));
            // The same offer keeps its place; another starts at its first file.
            let same = (ui.menu.as_ref()).filter(|menu| menu.word == word && menu.found == found);
            let (at, top) = same.map_or((0, 0), |menu| (menu.at, menu.top));
            Menu {
                word,
                found,
                at,
                top,
            }
        });
    }

    /// A key for the menu open over the message box, of the files or of
    /// the commands: the arrows choose a row, Enter and Tab pick it, Esc
    /// dismisses the menu. Answers whether the key was the menu's: Enter
    /// on a command that goes at once (`pick`) is still to send it.
    pub fn menu_key(&mut self, key: KeyEvent) -> bool {
        let ui = &mut self.ui;
        let (count, at) = match (&ui.slash, &ui.menu) {
            (Some(slash), _) => (slash.count(), slash.at),
            (None, Some(menu)) => (menu.found.len(), menu.at),
            (None, None) => return false,
        };
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT | KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                let written = ui.slash.as_ref().and_then(|slash| slash.written);
                ui.shut = Some(dismissal(ui.draft.text(), written).to_string());
                (ui.menu, ui.slash) = (None, None);
            }
            KeyCode::Up | KeyCode::Down if count > 0 => {
                let down = key.code == KeyCode::Down;
                if let Some(slash) = &mut ui.slash {
                    slash.step(down);
                } else if let Some(menu) = &mut ui.menu {
                    menu.at = (menu.at + if down { 1 } else { count - 1 }) % count;
                    menu.top = (menu.top)
                        .min(menu.at)
                        .max((menu.at + 1).saturating_sub(MENU_ROWS));
                }
            }
            // Tab only completes: the command's name, and a space for what it takes.
            KeyCode::Tab if count > 0 && plain && ui.slash.is_some() => self.complete(at),
            KeyCode::Enter | KeyCode::Tab if count > 0 && plain => return !self.pick(at),
            _ => return false,
        }
        true
    }

    /// The row at place `at` of the menu open over the message box is
    /// picked. The `@` word under the caret becomes the mention of that
    /// file, a space after it. A command is written whole, a space after
    /// it for what it takes; one that takes nothing more (`Slash::goes`)
    /// is written as it runs. Answers whether the message is to go at once.
    pub fn pick(&mut self, at: usize) -> bool {
        if let Some(slash) = &self.ui.slash {
            let Some(command) = slash.found.get(at).filter(|_| slash.written.is_none()) else {
                return false;
            };
            if !Slash::goes(command, self.ui.draft.text()) {
                self.complete(at);
                return false;
            }
            // One of the screen's own goes by its name, however it was found.
            if command.kind == Kind::Own {
                self.ui.draft = Draft::holding(&format!("/{}", command.name));
            }
            self.ui.slash = None;
            return true;
        }
        let Some(menu) = self.ui.menu.take() else {
            return false;
        };
        let Some(path) = menu.found.get(at) else {
            return false;
        };
        let draft = &mut self.ui.draft;
        let rest = &draft.text()[menu.word.end.min(draft.text().len())..];
        let spaces = rest.len() - rest.trim_start_matches(' ').len();
        draft.cut(menu.word.start, menu.word.end + spaces);
        draft.insert(&format!("{} ", mention::written(path)));
        self.ui.past.leave();
        self.suggest();
        false
    }

    /// The message becomes the command at place `at` of the `/` menu, a
    /// space after its name: what it takes is written next.
    fn complete(&mut self, at: usize) {
        let Some(slash) = self.ui.slash.take() else {
            return;
        };
        if let Some(command) = slash.found.get(at) {
            self.ui.draft = Draft::holding(&format!("/{} ", command.name));
            self.ui.past.leave();
        }
        self.suggest();
    }

    /// How many rows a page key moves the screen by.
    #[must_use]
    pub fn page(&self) -> isize {
        isize::from(self.height.saturating_sub(10).max(1).cast_signed())
    }

    /// The mouse: the wheel scrolls, a click opens or closes the line it is
    /// on. Answers what was clicked.
    pub fn mouse(&mut self, mouse: MouseEvent) -> Option<Click> {
        self.pointer = Some((mouse.column, mouse.row));
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // While a menu is open over the message box, a click is on
                // one of its rows, or closes it.
                if self.ui.pick.is_some() {
                    let spot = self.screen.spot_at(mouse.column, mouse.row);
                    return Some(match spot {
                        Some(spot @ (Spot::Choice(_) | Spot::Level(_))) => {
                            Click::Spot(spot.clone())
                        }
                        _ => Click::Away,
                    });
                }
                if self.screen.stop_at(mouse.column, mouse.row) {
                    return Some(Click::Stop);
                }
                if let Some(spot) = self.screen.spot_at(mouse.column, mouse.row) {
                    match spot {
                        Spot::Panel => self.toggle_panel(),
                        Spot::Queue => self.ui.toggle_queue(),
                        _ => return Some(Click::Spot(spot.clone())),
                    }
                    return None;
                }
                if let Some((row, cell)) = self.screen.field_at(mouse.column, mouse.row) {
                    self.ui.draft.point(row, cell, self.screen.draft);
                    self.suggest();
                    return None;
                }
                let fold = self.screen.fold_at(mouse.column, mouse.row)?;
                if let Fold::Prompt(turn) = fold {
                    return Some(Click::Edit(turn));
                }
                // The line clicked stays where it is: what it folds over
                // opens under it, and the conversation waits there. In the
                // view's last rows, it rises to show what opens.
                let first = self.screen.first;
                if self.ui.top.is_none() {
                    self.ui.top = Some(first);
                }
                let opens = !self.ui.folds.open(fold);
                self.ui.folds.toggle(fold, self.ui.wall);
                let at = usize::from(mouse.row.saturating_sub(view::TOP));
                let under = self.screen.rows.saturating_sub(at + 1);
                self.revealing = (opens && under < REVEAL).then_some(Reveal {
                    line: first + at,
                    first,
                    length: self.screen.length,
                });
                return Some(Click::Fold(fold));
            }
            MouseEventKind::ScrollUp => self.wheel(mouse.column, -WHEEL.cast_signed()),
            MouseEventKind::ScrollDown => self.wheel(mouse.column, WHEEL.cast_signed()),
            _ => {}
        }
        None
    }

    /// The wheel, with the pointer at cell `x`: over the kernel's panel it
    /// scrolls the panel's rows, when it has more than the screen;
    /// elsewhere, the conversation.
    fn wheel(&mut self, x: u16, rows: isize) {
        match self.screen.panel {
            Some((from, most)) if x >= from => {
                // Several moves may come before the next frame: each starts from the last.
                let top = self.ui.panel_top.min(most);
                self.ui.panel_top = top.saturating_add_signed(rows).min(most);
            }
            _ => self.scroll(rows),
        }
    }

    /// A line opened in the view's last rows, where what it opens would
    /// not show: it rises as that grows, to the view's middle at the most.
    fn reveal(&mut self) {
        let Some(reveal) = self.revealing else {
            return;
        };
        let grown = self.screen.length.saturating_sub(reveal.length);
        let rise = (reveal.line - reveal.first).saturating_sub(self.screen.rows / 2);
        self.ui.top = Some(reveal.first + grown.min(rise));
        if !self.ui.folds.moving(self.ui.wall) {
            self.revealing = None;
        }
    }

    /// The kernel's panel opens, or closes to its rail.
    pub fn toggle_panel(&mut self) {
        self.ui.panel = !self.ui.panel;
        self.ui.panel_at = self.ui.wall;
        self.ui.panel_top = 0;
    }

    /// A fold the user set is on its way, or the panel, the messages that
    /// wait, or the effort's track.
    #[must_use]
    pub fn moving(&self) -> bool {
        let wall = self.ui.wall;
        self.ui.folds.moving(wall)
            || wall - self.ui.panel_at < FADE_IN
            || wall - self.ui.queue.at < FOLD
            || (self.ui.slid).is_some_and(|(at, _)| wall - at < LANDS)
    }

    /// The conversation at `now`, on the terminal.
    ///
    /// # Errors
    ///
    /// The terminal could not be written.
    pub fn draw(
        &mut self,
        terminal: &mut ratatui::DefaultTerminal,
        chat: &Chat,
        now: f64,
    ) -> io::Result<()> {
        let size = terminal.size()?;
        self.height = size.height;
        // Twice when the line under the pointer changed: it lifts in the same frame.
        for _ in 0..2 {
            let mut canvas = Canvas::new(size.width, size.height);
            self.screen = view::draw(&mut canvas, chat, &self.ui, now, &mut self.kept);
            // Only a frame that differs from the last goes to the terminal, whole.
            if self.shown.as_ref() != Some(&canvas) {
                io::stdout().execute(BeginSynchronizedUpdate)?;
                terminal.draw(|frame| canvas.render(frame.buffer_mut(), self.depth))?;
                io::stdout().execute(EndSynchronizedUpdate)?;
                self.shown = Some(canvas);
            }
            // The panel's rows left, or closed: it shows no further than its last.
            let most = self.screen.panel.map_or(0, |(_, most)| most);
            self.ui.panel_top = self.ui.panel_top.min(most);
            let top = self.ui.top;
            self.reveal();
            // Back at the conversation's end once a fold has settled, it follows again.
            if !self.moving() && self.ui.top.is_some_and(|top| top >= self.screen.most) {
                self.ui.top = None;
            }
            // The line under the pointer, as the screen now is.
            let hover = self.pointer.and_then(|(x, y)| self.screen.fold_at(x, y));
            let over = (self.pointer)
                .and_then(|(x, y)| self.screen.spot_at(x, y))
                .cloned();
            if hover == self.ui.hover && over == self.ui.over && self.ui.top == top {
                break;
            }
            self.ui.hover = hover;
            self.ui.over = over;
        }
        Ok(())
    }
}
