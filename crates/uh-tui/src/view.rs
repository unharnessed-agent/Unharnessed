//! The screen: the conversation on the left, the kernel's panel on the
//! right, the message box under the conversation. It follows the
//! extension's look (`vscode/webview/Chat.tsx`, `Kernel.tsx`, `styles.css`):
//! the same page, surfaces told apart by a step of lightness, the cyan for
//! what is live. On top of the message box, the conversation's goal while
//! it has one (`Goal.tsx`). Over the message box, while an `@` word is written, the
//! folder's files it may name; while the message starts with a `/`, the
//! commands, prompts and skills it may run (`slash`); or the menu the user
//! opened from the bar under it, of the models or of the reasoning effort
//! (`pick`). The message box shows the images the message carries, and
//! says when it holds an earlier message being changed; a message of the
//! conversation shows its images by their names, and a click on it brings
//! it back to be changed. The saved conversations (`history`), the
//! settings (`prefs`) and the providers (`connect`) take the conversation's
//! place while they are open. Everything drawn is a
//! function of the conversation, what the user set on the screen, and the
//! clocks.

use std::collections::HashMap;
use std::sync::Arc;

use crate::canvas::Canvas;
use crate::cell::{python, summary};
use crate::chat::{Chat, Item, Kind, Stream, Tone, ToolStatus, Turn, TurnState};
use crate::connect::{self, Connect, Spot};
use crate::draft::{Draft, Laid};
use crate::fold::{Fold, Folds};
use crate::goal::{clock, Goal, Status};
use crate::history::{self, History};
use crate::kernel::{Command, Kernel, Phase, Variable, LEAVE};
use crate::line::{cells, Line};
use crate::markdown;
use crate::mention::{mentions, name_of, Paths, Word};
use crate::message::{Attached, Message};
use crate::motion::{
    breathe, landing, lit, progress, shine, spinner, FADE_IN, FOLD, THOUGHT_COOL, WAVE_FADE,
};
use crate::past::Past;
use crate::pick::{self, Entry, Pick};
use crate::prefs::{self, Prefs};
use crate::slash::{self, Slash};
use crate::text::{fit, glyphs, width, wrap, wrap_hard, Glyph, Ink};
use crate::theme::{sunk, Rgb, ACCENT, BG, INK, MUTED, PYTHON_BLUE, RAISED, RAISED_2};

/// The page's margin, left and right.
pub const MARGIN: u16 = 2;
/// The conversation's first row on the screen, under the wordmark's.
pub const TOP: u16 = 2;
/// The narrowest screen that shows the kernel's panel beside the conversation.
const PANEL_FROM: u16 = 100;
/// How wide the kernel's panel is once closed: a rail at the screen's
/// edge, a click on which opens it again.
const RAIL: u16 = 3;
/// What closes the kernel's panel, at its top right; and what opens it
/// again, on its rail.
const CLOSE_PANEL: &str = "»";
const OPEN_PANEL: &str = "«";
/// The shortest screen whose message box has air in it and a row under it.
const ROOMY_FROM: u16 = 20;
/// What a line that folds starts with, in cells: its mark (two cells) and a
/// space. What it folds over sits under its words.
const LEAD: usize = 3;
/// The reasoning's window: it grows to this many rows, then keeps the newest in view.
const THOUGHT_ROWS: usize = 4;
/// How long the answer may pause before the turn says again that it works.
const QUIET: f64 = 0.9;
/// The most rows an open cell shows of its code and of what it printed.
const CODE_ROWS: usize = 24;
const OUTPUT_ROWS: usize = 12;
/// What the message box asks for while nothing is written in it (the extension's).
const PLACEHOLDER: &str = "Ask Unharnessed";
/// The mark that stops the agent: a click on it does.
const STOP: char = '■';
/// The mark of what lets a goal that waits go on, and of what ends a goal
/// or stops a command.
const PLAY: char = '▶';
const END: &str = "×";
/// The marks of an image a message carries, and of a message being changed.
const IMAGE: &str = "▣";
const EDIT: &str = "✎";
/// How long a word about the images stays on the message box, in seconds.
pub const SAID: f64 = 6.0;
/// How much of its ink the conversation keeps from the message being
/// changed on: sending it takes that away.
const LEAVES: f32 = 0.35;
/// The most files the `@` menu shows at once.
pub const MENU_ROWS: usize = 8;
/// The widest the `@` menu runs, and the narrowest.
const MENU_MAX: usize = 76;
const MENU_MIN: usize = 34;
/// The most rows the messages that wait take on the message box, each a
/// row; past them, how many more wait.
const QUEUE_ROWS: usize = 6;
/// What a message that waits does, at its row's end under the pointer,
/// each a button: it goes now, comes back to the message box to be
/// changed, leaves. A terminal draws a character no larger than its cell:
/// these are the heaviest of their kind, in bold.
const NOW: &str = "➤";
const CHANGE: &str = "✎";
const DROP: &str = "✖";
/// What plays the messages that wait again, once a stop paused them.
const RESUME: &str = "▶ Resume";

/// The files offered for the `@` word being written in the message box.
#[derive(Clone)]
pub struct Menu {
    pub word: Word,
    /// The files that match what is typed of it, best first.
    pub found: Vec<String>,
    /// The one Enter picks, and the first one in view.
    pub at: usize,
    pub top: usize,
}

/// What the message box holds to be changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edited {
    /// The message turn `turn` starts with.
    Turn(usize),
    /// A message that waits, by its number.
    Waiting(u64),
}

/// A message being changed in the message box: one sent before, or one
/// that waits.
#[derive(Clone)]
pub struct Editing {
    pub of: Edited,
    /// What was in the message box before, which comes back after.
    pub saved: Message,
}

impl Editing {
    /// The turn whose message it is, when it is one sent before.
    #[must_use]
    pub fn turn(&self) -> Option<usize> {
        match self.of {
            Edited::Turn(turn) => Some(turn),
            Edited::Waiting(_) => None,
        }
    }
}

/// The messages that wait, on top of the message box.
#[derive(Clone, Copy)]
pub struct Queue {
    /// How many were ever put in line: the last one's number.
    pub given: u64,
    /// Folded to their head; when they last opened or closed.
    pub shut: bool,
    pub at: f64,
    /// A stop left them waiting: none goes by itself.
    pub paused: bool,
}

impl Default for Queue {
    fn default() -> Queue {
        Queue {
            given: 0,
            shut: false,
            at: f64::NEG_INFINITY,
            paused: false,
        }
    }
}

/// A message written while the agent works: it waits its turn.
#[derive(Clone, Debug)]
pub struct Waiting {
    /// Its number, which a click names it by.
    pub id: u64,
    pub message: Message,
    /// When it joined the line, on the screen's clock.
    pub at: f64,
    /// Back in the message box to be changed: the line waits at it.
    pub held: bool,
}

/// What the user set on the screen.
#[derive(Clone)]
pub struct Ui {
    /// The messages written while the agent works, in the order they go,
    /// and how they show.
    pub waiting: Vec<Waiting>,
    pub queue: Queue,
    /// The messages for the running turn that have not gone into it yet;
    /// they go before the ones that wait.
    pub rushed: Vec<Message>,
    /// The images the message being written carries, and what was last
    /// said of them (one refused, and why) with when.
    pub images: Vec<Attached>,
    pub said: Option<(f64, String)>,
    /// The earlier message being changed, while one is; and whether one
    /// can be: the agent is there, and does not work.
    pub editing: Option<Editing>,
    pub editable: bool,
    /// The messages sent before, for Up and Down to bring back.
    pub past: Past,
    /// The session's folder as the `@` menu lists it, once listed, and how
    /// many times the list changed.
    pub files: Option<Arc<Paths>>,
    pub listed: u64,
    /// The files offered, while the caret is in an `@` word; what a menu
    /// over the message box was dismissed on (the text; while a command's
    /// text is written, the command), which keeps it shut until that changes.
    pub menu: Option<Menu>,
    pub shut: Option<String>,
    /// What a `/` at the start of the message offers: the agent's
    /// commands, prompts and skills, then the screen's own; `None` where
    /// a `/` opens nothing. The menu of them, while the message opens it.
    pub commands: Option<Arc<Vec<slash::Command>>>,
    pub slash: Option<Slash>,
    /// What is open: the cells with their code and output, the reasoning.
    pub folds: Folds,
    /// The message being written.
    pub draft: Draft,
    /// The kernel's panel is open (else a rail at the screen's edge
    /// stands for it), when the screen is wide enough; when it last
    /// opened or closed; its first row in view, when it has more rows
    /// than the screen.
    pub panel: bool,
    pub panel_at: f64,
    pub panel_top: usize,
    /// The conversation's first row in view; `None` follows its end.
    pub top: Option<usize>,
    /// The fold under the pointer.
    pub hover: Option<Fold>,
    /// The providers' screen, while it takes the conversation's place.
    pub connect: Option<Connect>,
    /// The saved conversations' screen, while it does.
    pub history: Option<History>,
    /// The settings' screen, while it does.
    pub prefs: Option<Prefs>,
    /// The menu open over the message box: the models, or the reasoning effort.
    pub pick: Option<Pick>,
    /// When the effort's track last moved, and the level it moved from
    /// (`None`: it opened, and fills from nothing).
    pub slid: Option<(f64, Option<usize>)>,
    /// The screen is a live session's: an `@` in the message box opens the
    /// folder's files, the session's options can be set (the bar's model
    /// and effort open their menus), and its conversation left for another
    /// (the wordmark's row leads to a new one, to the saved ones and to
    /// the settings).
    pub live: bool,
    /// What the pointer is on, of what a click acts on.
    pub over: Option<Spot>,
    /// A note at the top right.
    pub note: String,
    /// The screen's own clock, in seconds: what the user moves follows it.
    pub wall: f64,
    /// The time it is, in seconds since 1970: a saved conversation's age counts from it.
    pub clock: i64,
}

impl Default for Ui {
    fn default() -> Ui {
        Ui {
            waiting: Vec::new(),
            queue: Queue::default(),
            rushed: Vec::new(),
            images: Vec::new(),
            said: None,
            editing: None,
            editable: false,
            past: Past::default(),
            files: None,
            listed: 0,
            menu: None,
            shut: None,
            commands: None,
            slash: None,
            folds: Folds::default(),
            draft: Draft::default(),
            panel: true,
            panel_at: f64::NEG_INFINITY,
            panel_top: 0,
            top: None,
            hover: None,
            connect: None,
            history: None,
            prefs: None,
            pick: None,
            slid: None,
            live: false,
            over: None,
            note: String::new(),
            wall: 0.0,
            clock: 0,
        }
    }
}

impl Ui {
    /// `message` waits its turn, last in line.
    pub fn wait(&mut self, message: Message) {
        let waiting = self.waiting_of(message);
        self.waiting.push(waiting);
    }

    /// `messages` wait, first in line, in their order.
    pub fn wait_first(&mut self, messages: Vec<Message>) {
        let first: Vec<Waiting> = (messages.into_iter())
            .map(|message| self.waiting_of(message))
            .collect();
        self.waiting.splice(0..0, first);
    }

    fn waiting_of(&mut self, message: Message) -> Waiting {
        self.queue.given += 1;
        Waiting {
            id: self.queue.given,
            message,
            at: self.wall,
            held: false,
        }
    }

    /// The messages that wait open on top of the message box, or fold to their head.
    pub fn toggle_queue(&mut self) {
        self.queue.shut = !self.queue.shut;
        self.queue.at = self.wall;
    }
}

/// A row a click acts on: cells `from..to` of row `y` head `fold`.
pub struct Hit {
    y: u16,
    from: u16,
    to: u16,
    fold: Fold,
}

/// Cells `from..to` of row `y`: a click there lands on `spot`.
pub struct Area {
    pub y: u16,
    pub from: u16,
    pub to: u16,
    pub spot: Spot,
}

/// What was drawn, for what the user does next on it.
#[derive(Default)]
pub struct Screen {
    /// How many rows the conversation can scroll, its first row in view,
    /// and how many rows it has.
    pub most: usize,
    pub first: usize,
    pub length: usize,
    /// How wide the message box's text runs: Up and Down move the caret by its rows.
    pub draft: usize,
    hits: Vec<Hit>,
    /// Where the mark that stops the agent is, while it works: its row and its cell.
    stop: Option<(u16, u16)>,
    /// The message's rows in its box: their first cell, how wide they run, how many they are.
    field: Option<(u16, u16, u16, u16)>,
    /// What else a click acts on: the providers' screen's rows and buttons, what leads to it.
    spots: Vec<Area>,
    /// The kernel's panel, while it shows: its first cell, and how many rows it can scroll.
    pub panel: Option<(u16, usize)>,
    /// How many rows of the conversation show, and each turn's first row in it.
    pub rows: usize,
    pub turns: Vec<(usize, usize)>,
}

impl Screen {
    /// A screen that is no conversation: its rows scroll, and a click acts on `spots`.
    #[must_use]
    pub fn page(most: usize, first: usize, spots: Vec<Area>) -> Screen {
        Screen {
            most,
            first,
            spots,
            ..Screen::default()
        }
    }

    /// What a click at cell `x` of row `y` lands on, other than a fold.
    #[must_use]
    pub fn spot_at(&self, x: u16, y: u16) -> Option<&Spot> {
        self.spots
            .iter()
            .find(|area| area.y == y && (area.from..area.to).contains(&x))
            .map(|area| &area.spot)
    }

    /// The fold whose line is at cell `x` of row `y`.
    #[must_use]
    pub fn fold_at(&self, x: u16, y: u16) -> Option<Fold> {
        self.hits
            .iter()
            .find(|hit| hit.y == y && (hit.from..hit.to).contains(&x))
            .map(|hit| hit.fold)
    }

    /// The row of the message box a click at cell `x` of row `y` lands on,
    /// and its cell there; a click beside a row lands on its nearest end.
    #[must_use]
    pub fn field_at(&self, x: u16, y: u16) -> Option<(usize, usize)> {
        let (left, top, wide, rows) = self.field?;
        let inside = (top..top + rows).contains(&y)
            && (left.saturating_sub(2)..left + wide + 2).contains(&x);
        inside.then(|| (usize::from(y - top), usize::from(x.saturating_sub(left))))
    }

    /// The mark that stops the agent is at cell `x` of row `y`, or beside it.
    #[must_use]
    pub fn stop_at(&self, x: u16, y: u16) -> bool {
        self.stop
            .is_some_and(|(row, cell)| y == row && x.abs_diff(cell) <= 1)
    }
}

/// Draws the screen. `kept` holds what the frame before it laid out and
/// that has not moved since.
pub fn draw(canvas: &mut Canvas, chat: &Chat, ui: &Ui, now: f64, kept: &mut Kept) -> Screen {
    let (w, h) = (canvas.width, canvas.height);
    if w < 44 || h < 12 {
        let note = fit(
            glyphs("The window is too small.", Ink::muted(), 0),
            usize::from(w),
        );
        canvas.put(0, 0, w, &note, 1.0);
        return Screen::default();
    }
    if let Some(connect) = &ui.connect {
        header(canvas, ui, false);
        return connect::draw(canvas, chat, ui, connect, now);
    }
    if let Some(prefs) = &ui.prefs {
        header(canvas, ui, false);
        return prefs::draw(canvas, chat, ui, prefs, now);
    }
    if let Some(history) = &ui.history {
        header(canvas, ui, false);
        return history::draw(canvas, chat, ui, history, now);
    }
    let mut spots = header(canvas, ui, true);
    let panel = match (w >= PANEL_FROM, ui.panel) {
        (false, _) => 0,
        (true, true) => (w / 4).clamp(26, 32),
        (true, false) => RAIL,
    };
    let column = if panel > 0 {
        w - panel - 1 - 2 - MARGIN
    } else {
        w - 2 * MARGIN
    };
    let top = TOP;
    let roomy = h >= ROOMY_FROM;
    // The message box grows with the message, to a part of the screen.
    let draft = usize::from(column.saturating_sub(4));
    let most = usize::from(if roomy { ((h - 14) / 3).clamp(1, 8) } else { 1 });
    let mut laid = ui.draft.lay(draft, most, Ink::text());
    for row in &mut laid.rows {
        marked(row, ui.draft.text(), ui.files.as_deref());
    }
    let goal_rows = tops(chat, ui, roomy);
    let box_rows = cells(laid.rows.len().max(1)) + if roomy { 4 } else { 3 } + goal_rows;
    // On top of it all, the messages that wait: in what the screen has
    // left once the conversation has a few rows.
    let left = h.saturating_sub(box_rows + u16::from(roomy) + top + 5);
    let most = usize::from(if roomy { left } else { left.min(1) }).min(QUEUE_ROWS + 5);
    let tray = queue(chat, ui, draft, most, now);
    let tray_rows = cells(tray.rows.len());
    let box_rows = box_rows + tray_rows;
    let composer = h - box_rows - u16::from(roomy);

    // A row of the page stays between the conversation and the message box.
    let area = (top, composer - 1 - top, column);
    let (most, first, length, mut hits, turns) = conversation(canvas, chat, ui, area, now, kept);
    let rows = usize::from(area.1);
    // Over the conversation's last rows; a row of the page stays under it.
    let area = (MARGIN, top, composer - 1, column);
    let menu = match (&ui.pick, &ui.slash, &ui.menu) {
        (Some(pick), ..) => pick::draw(canvas, pick, chat, ui, area),
        (None, Some(slash), _) => slash::draw(canvas, slash, ui, area),
        (None, None, Some(menu)) => self::menu(canvas, menu, ui, area),
        (None, None, None) => None,
    };
    if let Some((covered, picks)) = menu {
        // A click on it is not one on the line it covers.
        hits.retain(|hit| hit.y < covered);
        spots.extend(picks);
    }
    let area = (MARGIN, composer, column, box_rows);
    let (stop, links) = self::composer(canvas, chat, &laid, area, ui, (&tray, goal_rows, now));
    spots.extend(links);
    let field = (!ui.draft.is_empty()).then(|| {
        let wide = column.saturating_sub(4);
        let first = composer + 1 + tray_rows + goal_rows;
        (MARGIN + 2, first, wide, cells(laid.rows.len()))
    });
    let mut scrolls = None;
    let area = (w - panel - 1, top, panel, composer + box_rows - top);
    if panel == RAIL {
        spots.extend(rail(canvas, &chat.kernel, ui, area, now));
    } else if panel > 0 {
        let (rows, marks, most) = self::panel(canvas, &chat.kernel, ui, area, now);
        hits.extend(rows);
        spots.extend(marks);
        scrolls = Some((area.0, most));
    }
    Screen {
        most,
        first,
        length,
        draft,
        hits,
        stop,
        field,
        spots,
        panel: scrolls,
        rows,
        turns,
    }
}

/// What stands over the message in its box, a row each: the goal, the
/// message being changed, the images, what is said of them; and a row
/// before the message where there is room (`roomy`). Answers how many
/// rows they take.
fn tops(chat: &Chat, ui: &Ui, roomy: bool) -> u16 {
    let tops = [
        chat.goal.is_some(),
        ui.editing.is_some(),
        !ui.images.is_empty(),
        noted(chat, ui).is_some(),
    ];
    match cells(tops.into_iter().filter(|top| *top).count()) {
        0 => 0,
        count => count + u16::from(roomy),
    }
}

/// What the message box says of the images: what was just said of one
/// (refused, and why), else that the model in use reads none while the
/// message carries some and no image model stands in for it.
fn noted(chat: &Chat, ui: &Ui) -> Option<(String, bool)> {
    if let Some((at, said)) = &ui.said {
        if ui.wall - at < SAID {
            return Some((said.clone(), true));
        }
    }
    if ui.images.is_empty() || chat.settings.value("imageModel").is_string() {
        return None;
    }
    let model = (chat.models.iter()).find(|model| Some(&model.value) == chat.chosen.as_ref())?;
    let blind = !model.input.is_empty() && !model.input.iter().any(|input| input == "image");
    blind.then(|| {
        let said = format!(
            "{} doesn't read images: set an image model in the settings.",
            model.name
        );
        (said, false)
    })
}

/// The conversation in the rows `area` gives (its first row, how many
/// rows, how wide its text runs), from where the user scrolled it to, or
/// its end. Answers how many rows it can scroll, its first row in view, how
/// many rows it has, the lines a click folds, and each turn's first row.
fn conversation(
    canvas: &mut Canvas,
    chat: &Chat,
    ui: &Ui,
    area: (u16, u16, u16),
    now: f64,
    kept: &mut Kept,
) -> (usize, usize, usize, Vec<Hit>, Vec<(usize, usize)>) {
    let (top, rows, text) = area;
    let rows = usize::from(rows);
    let parts = transcript(chat, ui, usize::from(text), now, kept);
    let length = parts.last().map_or(0, |part| part.from + part.rows);
    let most = length.saturating_sub(rows);
    let first = ui.top.map_or(most, |top| top.min(most));
    let turns = (parts.iter())
        .filter_map(|part| Some((part.turn?, part.from)))
        .collect();
    let mut hits = Vec::new();
    for part in &parts {
        // Only what is in view is laid out again, if it was let go of.
        let seen = part.from < first + rows && part.from + part.rows > first;
        let lines: &[Line] = match (&part.lines, part.turn) {
            (Some(lines), _) => lines,
            (None, Some(index)) if seen => kept.lines(index, || {
                turn_lines(&chat.turns[index], index, ui, usize::from(text), now)
            }),
            (None, Some(index)) => {
                kept.rest(index);
                continue;
            }
            (None, None) => continue,
        };
        for (at, line) in lines.iter().enumerate() {
            let Some(row) = (part.from + at)
                .checked_sub(first)
                .filter(|row| *row < rows)
            else {
                continue;
            };
            let y = top + cells(row);
            canvas.line(MARGIN, y, text, line);
            if let Some(fold) = line.fold {
                // A message is clicked on its card; a line that folds, anywhere on its row.
                let (from, to) = match (fold, &line.fill) {
                    (Fold::Prompt(_), Some(fill)) => (fill.from, fill.to.min(text)),
                    _ => (0, text),
                };
                hits.push(Hit {
                    y,
                    from: MARGIN + from,
                    to: MARGIN + to,
                    fold,
                });
            }
        }
    }
    (most, first, length, hits, turns)
}

/// `glyphs`, drawn from `text`, with each mention of a file or a folder
/// the session's has lying on the accent, as the extension's pill.
fn marked(glyphs: &mut [Glyph], text: &str, files: Option<&Paths>) {
    let Some(files) = files else {
        return;
    };
    if !text.contains('@') {
        return;
    }
    let found = mentions(text, |path| files.knows(path));
    for glyph in glyphs {
        let at = glyph.src as usize;
        if found
            .iter()
            .any(|mention| (mention.start..mention.end).contains(&at))
        {
            glyph.ink = glyph.ink.on(ACCENT, 0.16);
        }
    }
}

/// The `@` menu, over the last rows of the column `area` gives (its left,
/// its first row, the row it ends before, its width): a file per row, its
/// name then where it is, the one Enter picks on a lighter surface; while
/// there is none to show, why. Answers the first row it covers, and where
/// a click picks each file; `None` when the screen has no room for it.
fn menu(
    canvas: &mut Canvas,
    menu: &Menu,
    ui: &Ui,
    area: (u16, u16, u16, u16),
) -> Option<(u16, Vec<Area>)> {
    let shown = menu.found.len().clamp(1, MENU_ROWS).min(pick::room(area));
    if shown == 0 {
        return None;
    }
    let row = |path: &str, lit: bool| {
        let name = name_of(path);
        let ink = if lit { Ink::text() } else { Ink::of(INK, 0.78) };
        let place = if lit { Ink::muted() } else { Ink::faint() };
        joined([
            glyphs(name, ink, 0),
            glyphs("  ", place, 0),
            glyphs(&path[..path.len() - name.len()], place, 0),
        ])
    };
    let widest = (menu.found.iter())
        .map(|path| width(&row(path, false)))
        .max()
        .unwrap_or(0);
    let most = usize::from(area.3).min(MENU_MAX);
    let wide = (widest + 4).clamp(MENU_MIN.min(most), most);
    let inner = wide.saturating_sub(4);
    if menu.found.is_empty() {
        let why = match &ui.files {
            None => "Listing the folder's files…",
            Some(files) if files.is_empty() => "This folder has no file to mention.",
            Some(_) => "No file by that name.",
        };
        // As wide as what it says, where the screen has the room.
        let why = glyphs(why, Ink::muted(), 0);
        let wide = wide.max((width(&why) + 4).min(most));
        let why = Entry::of(fit(why, wide.saturating_sub(4)));
        return pick::frame(canvas, area, wide, vec![why]);
    }
    let from = menu.top.min(menu.found.len() - shown);
    let entries = (menu.found.iter().enumerate().skip(from).take(shown))
        .map(|(at, path)| {
            let chosen = at == menu.at;
            let over = ui.over == Some(Spot::Pick(at));
            Entry {
                glyphs: fit(row(path, chosen || over), inner),
                lit: chosen,
                spots: vec![(0, wide, Spot::Pick(at))],
            }
        })
        .collect();
    pick::frame(canvas, area, wide, entries)
}

/// What the wordmark's row leads to, on its right: a new conversation, and
/// the saved ones.
const NEW: &str = "New chat";
const HISTORY: &str = "History";
const SETTINGS: &str = "Settings";

/// The wordmark, its U in the cyan; on the right the note, or with
/// `links`, in a session that can leave its conversation for another,
/// what leads to a new one and to the saved ones: answers where a click
/// lands on them.
fn header(canvas: &mut Canvas, ui: &Ui, links: bool) -> Vec<Area> {
    let mut mark = glyphs("U", Ink::accent().bold(), 0);
    mark.extend(glyphs("nharnessed", Ink::text().bold(), 0));
    canvas.put(MARGIN, 0, canvas.width, &mark, 1.0);
    let room = usize::from(canvas.width.saturating_sub(MARGIN * 2 + 14));
    if !(links && ui.live) {
        let note = fit(glyphs(&ui.note, Ink::faint(), 0), room);
        let x = canvas.width.saturating_sub(MARGIN + cells(width(&note)));
        canvas.put(x, 0, canvas.width, &note, 1.0);
        return Vec::new();
    }
    let mut areas = Vec::new();
    let mut end = canvas.width.saturating_sub(MARGIN);
    let links = [
        (SETTINGS, Spot::Settings),
        (HISTORY, Spot::History),
        (NEW, Spot::New),
    ];
    for (word, spot) in links {
        let over = ui.over.as_ref() == Some(&spot);
        let word = glyphs(word, if over { Ink::text() } else { Ink::faint() }, 0);
        let x = end.saturating_sub(cells(width(&word)));
        canvas.put(x, 0, end, &word, 1.0);
        areas.push(Area {
            y: 0,
            from: x.saturating_sub(1),
            to: end + 1,
            spot,
        });
        end = x.saturating_sub(3);
    }
    areas
}

/// The message box, a surface `rows` high: the messages that wait on top,
/// while some do; the conversation's goal, while it has one; the message
/// being written (what the box asks for, while there is none) with its
/// caret; and under it the box's bar. Answers where the mark that stops
/// the agent is, while it works (or makes its goal wait), and where a
/// click lands on what the box leads to.
fn composer(
    canvas: &mut Canvas,
    chat: &Chat,
    laid: &Laid,
    area: (u16, u16, u16, u16),
    ui: &Ui,
    (tray, goal_rows, now): (&Tray, u16, f64),
) -> (Option<(u16, u16)>, Vec<Area>) {
    let (x, y, w, rows) = area;
    let wide = usize::from(w);
    for row in 0..rows {
        canvas.line(x, y + row, w, &Line::surface(RAISED, 0, wide));
    }
    let room = wide.saturating_sub(4);
    let mut spots = Vec::new();
    for (at, (row, alpha)) in tray.rows.iter().enumerate() {
        canvas.put(x + 2, y + 1 + cells(at), x + w, row, *alpha);
    }
    spots.extend((tray.spots.iter()).map(|(row, from, to, spot)| Area {
        y: y + 1 + cells(*row),
        from: x + 2 + cells(*from),
        to: x + 2 + cells(*to),
        spot: spot.clone(),
    }));
    let tray_rows = cells(tray.rows.len());
    // What stands over the message, a row each; the message starts
    // `goal_rows` under where it would.
    let mut top = y + 1 + tray_rows;
    let (y, rows) = (y + tray_rows + goal_rows, rows - tray_rows - goal_rows);
    if let Some(goal) = chat.goal.as_ref().filter(|_| goal_rows > 0) {
        let (row, end) = goal_row(goal, room, ui, now);
        let shown = progress(now, goal.shown, FADE_IN) as f32;
        canvas.put(x + 2, top, x + w, &row, shown);
        // A cell on each side of the mark takes the click too.
        spots.push(Area {
            y: top,
            from: x + 2 + cells(end.saturating_sub(1)),
            to: x + 2 + cells(end + 2),
            spot: Spot::EndGoal,
        });
        top += 1;
    }
    if ui.editing.is_some() {
        let (row, end) = editing_row(room, ui);
        canvas.put(x + 2, top, x + w, &row, 1.0);
        spots.push(Area {
            y: top,
            from: x + 2 + cells(end.saturating_sub(1)),
            to: x + 2 + cells(end + 2),
            spot: Spot::Keep,
        });
        top += 1;
    }
    if !ui.images.is_empty() {
        let (row, crosses) = images_row(room, ui);
        canvas.put(x + 2, top, x + w, &row, 1.0);
        spots.extend(crosses.into_iter().map(|(from, to, spot)| Area {
            y: top,
            from: x + 2 + cells(from),
            to: x + 2 + cells(to),
            spot,
        }));
        top += 1;
    }
    if let Some((said, refused)) = noted(chat, ui) {
        let ink = if refused { Ink::danger() } else { Ink::muted() };
        canvas.put(x + 2, top, x + w, &fit(glyphs(&said, ink, 0), room), 1.0);
    }
    let caret = caret(RAISED);
    // While the models are searched, what is typed goes to their menu: the caret is there.
    let writes = !matches!(ui.pick, Some(Pick::Models(_)));
    if laid.rows.iter().all(Vec::is_empty) {
        let mut ask = glyphs(PLACEHOLDER, Ink::faint(), 0);
        if let Some(first) = ask.first_mut().filter(|_| writes) {
            first.ink = caret;
        }
        canvas.put(x + 2, y + 1, x + w, &fit(ask, room), 1.0);
    } else {
        for (at, row) in laid.rows.iter().enumerate() {
            let mut row = row.clone();
            if at == laid.caret.0 && writes {
                row = with_caret(row, laid.caret.1, caret);
            }
            canvas.put(x + 2, y + 1 + cells(at), x + w, &row, 1.0);
        }
    }
    let (bar, links) = bar(chat, room, ui, now);
    let row = y + rows - 2;
    canvas.put(x + 2, row, x + w, &bar, 1.0);
    spots.extend(links.into_iter().map(|(from, to, spot)| Area {
        y: row,
        from: x + 2 + cells(from),
        to: x + 2 + cells(to),
        spot,
    }));
    let mark = |mark: char| {
        (bar.iter().position(|glyph| glyph.ch == mark)).map(|at| x + 2 + cells(width(&bar[..at])))
    };
    if let Some(play) = mark(PLAY) {
        spots.push(Area {
            y: row,
            from: play.saturating_sub(1),
            to: play + 2,
            spot: Spot::Play,
        });
    }
    (mark(STOP).map(|stop| (row, stop)), spots)
}

/// The goal on top of the message box, `room` cells wide: its mark, where
/// it stands, what it is after (blocked: why), and at the row's end what
/// ends it. Answers the row, and the cell of what ends it.
fn goal_row(goal: &Goal, room: usize, ui: &Ui, now: f64) -> (Vec<Glyph>, usize) {
    let mark = match goal.status {
        Status::Active => glyphs("⚑", Ink::accent().alpha(0.55 + 0.45 * breathe(now)), 0),
        Status::Complete => glyphs("✓", Ink::accent(), 0),
        _ => glyphs("⚑", Ink::muted(), 0),
    };
    let head = joined([
        mark,
        glyphs(" ", Ink::faint(), 0),
        glyphs(goal.state(), Ink::text().bold(), 0),
        glyphs("  ", Ink::faint(), 0),
    ]);
    let end = glyphs(
        END,
        if ui.over == Some(Spot::EndGoal) {
            Ink::text()
        } else {
            Ink::faint()
        },
        0,
    );
    // Two cells stay between what it says and what ends it.
    let says = room.saturating_sub(width(&head) + 3);
    let left = joined([head, fit(glyphs(goal.says(), Ink::muted(), 0), says)]);
    let at = room.saturating_sub(1);
    let gap = at.saturating_sub(width(&left));
    (
        joined([left, glyphs(&" ".repeat(gap), Ink::faint(), 0), end]),
        at,
    )
}

/// What says, on top of the message box, that the message in it takes
/// the place of an earlier one, or of one that waits; at the row's end,
/// what leaves that one as it was. Answers the row, and the cell of what
/// leaves it.
fn editing_row(room: usize, ui: &Ui) -> (Vec<Glyph>, usize) {
    let what = match ui.editing.as_ref().map(|editing| editing.of) {
        Some(Edited::Waiting(_)) => "Editing a waiting message",
        _ => "Editing an earlier message",
    };
    let left = joined([
        glyphs(EDIT, Ink::accent(), 0),
        glyphs(" ", Ink::faint(), 0),
        glyphs(what, Ink::text().bold(), 0),
    ]);
    let over = ui.over == Some(Spot::Keep);
    let end = glyphs(END, if over { Ink::text() } else { Ink::faint() }, 0);
    let left = fit(left, room.saturating_sub(3));
    let at = room.saturating_sub(1);
    let gap = at.saturating_sub(width(&left));
    (
        joined([left, glyphs(&" ".repeat(gap), Ink::faint(), 0), end]),
        at,
    )
}

/// The messages that wait, as they stand on top of the message box: its
/// rows, each with how far in it is (an empty one is a row of the box's
/// surface), and where a click lands on them: a row, its first cell, the
/// cell past its last.
#[derive(Default)]
struct Tray {
    rows: Vec<(Vec<Glyph>, f32)>,
    spots: Vec<(usize, usize, usize, Spot)>,
}

/// How many cells the end of a row that waits takes: what it does under
/// the pointer (three buttons of three cells), or what is happening to it.
const QUEUE_END: usize = 9;

/// The messages that wait for the agent (`Composer.tsx`'s queue), `room`
/// cells wide and in `most` rows at the most: a head that says how many,
/// a click on which folds them to it or opens them again; then a row
/// each, in the order they go. First the ones sent into the running turn
/// that it has not taken yet, a cyan dot breathing beside each; then the
/// ones that wait for its end, numbered, the one back in the message box
/// marked so. Under the pointer a row shows at its end what it does: go
/// now, come back to the message box to be changed, leave. A rule under
/// them, then the message box. Folded, the head says what goes next.
/// Paused by a stop, the head says so, and plays them again.
fn queue(chat: &Chat, ui: &Ui, room: usize, most: usize, now: f64) -> Tray {
    let going: Vec<&Message> = chat.steering().chain(ui.rushed.iter()).collect();
    let count = going.len() + ui.waiting.len();
    let mut tray = Tray::default();
    if count == 0 || most == 0 {
        return tray;
    }
    // A row each, as many as fit with the head, the rule and the rows of the box between them.
    let fits = most.saturating_sub(5);
    let open = match (fits, ui.queue.shut) {
        (0, _) => 0.0,
        (_, true) => 1.0 - progress(now, ui.queue.at, FOLD),
        (_, false) => progress(now, ui.queue.at, FOLD),
    };
    let next = (going.first().copied()).or_else(|| ui.waiting.first().map(|w| &w.message));
    let (head, spots) = queue_head(ui, count, next, room, open);
    tray.spots.extend(
        spots
            .into_iter()
            .map(|(from, to, spot)| (0, from, to, spot)),
    );
    tray.rows.push((head, 1.0));
    if most == 1 {
        return tray;
    }
    let blank = || (Vec::new(), 1.0);
    let mut rows: Vec<(Vec<Glyph>, f32)> = Vec::new();
    let mut spots = Vec::new();
    for message in &going {
        let dot = glyphs("●", Ink::accent().alpha(0.55 + 0.45 * breathe(now)), 0);
        let tag = glyphs("Sending", Ink::faint(), 0);
        rows.push((queued(dot, message, tag, false, room), 1.0));
    }
    for (number, waiting) in (1..).zip(&ui.waiting) {
        let (row, its) = waiting_row(waiting, number, ui, room);
        let at = rows.len();
        spots.extend(its.into_iter().map(|(from, to, spot)| (at, from, to, spot)));
        rows.push((row, progress(now, waiting.at, FADE_IN) as f32));
    }
    let fits = fits.max(1);
    if rows.len() > fits {
        let more = rows.len() - (fits - 1);
        rows.truncate(fits - 1);
        spots.retain(|(at, ..)| *at < fits - 1);
        let more = glyphs(&format!("    +{more} more"), Ink::faint(), 0);
        rows.push((more, 1.0));
    }
    // Open: the head, a row of the box, the rows, a row, the rule, a row.
    let rule = glyphs(&"─".repeat(room), Ink::of(INK, 0.1), 0);
    let mut shown = vec![blank()];
    shown.extend(rows);
    shown.extend([blank(), (rule, 1.0), blank()]);
    // Opening, its rows come in from the head down; folding, they leave from the bottom up.
    let keep = ((shown.len() - 1) as f64 * open).round() as usize;
    shown.truncate(keep);
    shown.push(blank());
    tray.rows.extend(shown);
    let reached = tray.rows.len();
    tray.spots.extend(
        (spots.into_iter())
            .map(|(at, start, end, spot)| (2 + at, start, end, spot))
            .filter(|(at, ..)| *at < reached),
    );
    tray
}

/// The head of the messages that wait, `room` cells wide: whether they
/// wait or are paused, and how many; folded (`open` under 1), what goes
/// next beside it; paused, what plays them again at its end. Answers the
/// row, and where a click lands on it.
fn queue_head(
    ui: &Ui,
    count: usize,
    next: Option<&Message>,
    room: usize,
    open: f64,
) -> (Vec<Glyph>, Vec<(usize, usize, Spot)>) {
    let lit = ui.over == Some(Spot::Queue);
    let mut head = joined([
        glyphs(
            if ui.queue.paused { "Paused" } else { "Waiting" },
            if lit { Ink::text() } else { Ink::muted() },
            0,
        ),
        glyphs("  ", Ink::faint(), 0),
        glyphs(&format!(" {count} "), Ink::text().on(INK, 0.1), 0),
    ]);
    let resume = if ui.queue.paused {
        let ink = if ui.over == Some(Spot::Resume) {
            Ink::accent().on(ACCENT, 0.16)
        } else {
            Ink::accent()
        };
        glyphs(&format!(" {RESUME} "), ink, 0)
    } else {
        Vec::new()
    };
    let end = room.saturating_sub(width(&resume));
    if let Some(next) = next.filter(|_| open < 1.0) {
        let mut said = glyphs("   ", Ink::faint(), 0);
        said.extend(glyphs(&gist(next), Ink::faint(), 0));
        let said = fit(said, end.saturating_sub(width(&head) + 2));
        let fade = 1.0 - open as f32;
        head.extend(said.into_iter().map(|mut glyph| {
            glyph.ink.alpha *= fade;
            glyph
        }));
    }
    let mut spots = Vec::new();
    if !resume.is_empty() {
        let gap = end.saturating_sub(width(&head));
        head.extend(glyphs(&" ".repeat(gap), Ink::faint(), 0));
        head.extend(resume);
        spots.push((end, room, Spot::Resume));
    }
    spots.push((0, end, Spot::Queue));
    (head, spots)
}

/// The row of the message that waits `number`th in line, `room` cells
/// wide. Under the pointer it shows at its end what it does, each a
/// button, the one under the pointer lit; a click lands on them all the
/// same. Being changed, it says so. Answers the row, and where a click
/// lands on it.
fn waiting_row(
    waiting: &Waiting,
    number: usize,
    ui: &Ui,
    room: usize,
) -> (Vec<Glyph>, Vec<(usize, usize, Spot)>) {
    let id = waiting.id;
    if waiting.held {
        let tag = glyphs("Editing", Ink::faint(), 0);
        let mark = glyphs(EDIT, Ink::accent(), 0);
        return (queued(mark, &waiting.message, tag, false, room), Vec::new());
    }
    let over = |spot: &Spot| ui.over.as_ref() == Some(spot);
    let (rush, hold, drop) = (Spot::Rush(id), Spot::Hold(id), Spot::Unqueue(id));
    let lit = [&rush, &hold, &drop, &Spot::Waiting(id)]
        .into_iter()
        .any(&over);
    let button = |mark: &str, spot: &Spot, hot: Ink| {
        let ink = if over(spot) {
            hot.on(INK, 0.1)
        } else {
            Ink::muted()
        };
        glyphs(&format!(" {mark} "), ink.bold(), 0)
    };
    let acts = if lit {
        joined([
            button(NOW, &rush, Ink::accent()),
            button(CHANGE, &hold, Ink::text()),
            button(DROP, &drop, Ink::text()),
        ])
    } else {
        Vec::new()
    };
    let end = room.saturating_sub(QUEUE_END);
    let spots = vec![
        (end, end + 3, rush),
        (end + 3, end + 6, hold),
        (end + 6, room, drop),
        (0, end, Spot::Waiting(id)),
    ];
    let mark = glyphs(&number.to_string(), Ink::faint(), 0);
    (queued(mark, &waiting.message, acts, lit, room), spots)
}

/// What a message that waits says on its row: its words on one line, or
/// how many images it carries when it is nothing but them.
fn gist(message: &Message) -> String {
    let words = message
        .text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    match message.images.len() {
        _ if !words.is_empty() => words,
        1 => "1 image".to_string(),
        count => format!("{count} images"),
    }
}

/// A row of a message that waits, `room` cells wide: its mark in two
/// cells, its gist (`lit`, under the pointer), its images when it has
/// words too, and `end` at the row's end.
fn queued(
    mark: Vec<Glyph>,
    message: &Message,
    end: Vec<Glyph>,
    lit: bool,
    room: usize,
) -> Vec<Glyph> {
    let pad = 2usize.saturating_sub(width(&mark));
    let mut row = joined([glyphs(&" ".repeat(pad), Ink::faint(), 0), mark]);
    row.extend(glyphs("  ", Ink::faint(), 0));
    let images = match message.images.len() {
        0 => Vec::new(),
        _ if message.text.trim().is_empty() => Vec::new(),
        1 => glyphs(&format!("  {IMAGE}"), Ink::of(ACCENT, 0.6), 0),
        count => joined([
            glyphs(&format!("  {IMAGE}"), Ink::of(ACCENT, 0.6), 0),
            glyphs(&format!(" {count}"), Ink::faint(), 0),
        ]),
    };
    let ink = if lit { Ink::text() } else { Ink::muted() };
    let says = room.saturating_sub(width(&row) + width(&images) + QUEUE_END + 2);
    row.extend(fit(glyphs(&gist(message), ink, 0), says));
    row.extend(images);
    let gap = room.saturating_sub(width(&row) + width(&end));
    row.extend(glyphs(&" ".repeat(gap), Ink::faint(), 0));
    row.extend(end);
    fit(row, room)
}

/// An image a message carries, as a word: its mark and its name, cut to
/// `most` cells.
fn image(image: &Attached, ink: Ink, most: usize) -> Vec<Glyph> {
    joined([
        glyphs(IMAGE, Ink::of(ACCENT, ink.alpha.min(0.8)), 0),
        glyphs(" ", ink, 0),
        fit(glyphs(&image.name, ink, 0), most),
    ])
}

/// The images the message being written carries, each with what takes it
/// off. Answers the row, and where a click takes each off.
fn images_row(room: usize, ui: &Ui) -> (Vec<Glyph>, Vec<(usize, usize, Spot)>) {
    let mut row = Vec::new();
    let mut spots = Vec::new();
    for (at, attached) in ui.images.iter().enumerate() {
        let spot = Spot::Detach(at);
        let over = ui.over.as_ref() == Some(&spot);
        // A name too long for the row is cut, its cross kept.
        let mut chip = image(attached, Ink::of(INK, 0.82), room.saturating_sub(4));
        chip.extend(glyphs(" ", Ink::faint(), 0));
        chip.extend(glyphs(
            END,
            if over { Ink::text() } else { Ink::faint() },
            0,
        ));
        let from = width(&row) + if row.is_empty() { 0 } else { 3 };
        // The images that do not fit are counted.
        if from + width(&chip) + 6 > room && at + 1 < ui.images.len() {
            let more = format!("   +{}", ui.images.len() - at);
            row.extend(glyphs(&more, Ink::faint(), 0));
            break;
        }
        if !row.is_empty() {
            row.extend(glyphs("   ", Ink::faint(), 0));
        }
        let to = from + width(&chip);
        row.extend(chip);
        spots.push((to.saturating_sub(2), to + 1, spot));
    }
    (fit(row, room), spots)
}

/// The caret's ink on a surface of colour `surface`: the cell turned over.
#[must_use]
pub fn caret(surface: Rgb) -> Ink {
    Ink::of(surface, 1.0).on(INK, 0.82)
}

/// `row` with the caret at its cell `at`: the cell it stands before, in
/// the caret's ink; at the row's end, an empty one.
#[must_use]
pub fn with_caret(mut row: Vec<Glyph>, at: usize, caret: Ink) -> Vec<Glyph> {
    let mut cell = 0;
    let under = row.iter_mut().find(|glyph| {
        let here = cell == at;
        cell += usize::from(glyph.width);
        here
    });
    match under {
        Some(glyph) => glyph.ink = caret,
        None => row.extend(glyphs(" ", caret, 0)),
    }
    row
}

/// What stands in the bar for the model while no provider is connected: a
/// click on it leads to the providers' screen (the extension's).
const CONNECT: &str = "Connect a provider";

/// A reasoning effort as the extension names it.
#[must_use]
pub fn effort_name(level: &str) -> String {
    match level {
        "off" => "Off".to_string(),
        "minimal" => "Minimal".to_string(),
        "low" => "Low".to_string(),
        "medium" => "Medium".to_string(),
        "high" => "High".to_string(),
        "xhigh" => "XHigh".to_string(),
        "max" => "Max".to_string(),
        other => other.to_string(),
    }
}

/// The reasoning effort as a track, a stretch per level, lit up to the
/// current one (the extension's slider, at rest); then its name.
fn effort(chat: &Chat) -> Vec<Glyph> {
    let Some(current) = chat.effort.as_deref() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(at) = chat.efforts.iter().position(|level| level == current) {
        let lit = "━".repeat(at + 1);
        let rest = "━".repeat(chat.efforts.len() - at - 1);
        out.extend(glyphs(&lit, Ink::of(INK, 0.8), 0));
        out.extend(glyphs(&rest, Ink::of(INK, 0.16), 0));
        out.extend(glyphs(" ", Ink::faint(), 0));
    }
    out.extend(glyphs(&effort_name(current), Ink::muted(), 0));
    out
}

/// What opens a menu comes up under the pointer, and while its menu is
/// open: nearer the ink, on a lighter surface.
fn chip(mut glyphs: Vec<Glyph>, lit: bool) -> Vec<Glyph> {
    if lit {
        for glyph in &mut glyphs {
            glyph.ink.alpha += (1.0 - glyph.ink.alpha) * 0.6;
            glyph.ink = glyph.ink.on(INK, 0.1);
        }
    }
    glyphs
}

/// The message box's bar, `room` cells wide: the model and its reasoning
/// effort on the left; on the right how much of the context is used and,
/// while the agent works, the mark of what stops it. Short of room it lets
/// go of the word "context", then of the numbers, then of the effort.
/// While no provider is connected, what leads to their screen stands for
/// the model and the effort. Answers its glyphs, and where a click lands
/// on what it leads to: the providers' screen, the models' menu, the
/// effort's (a model with one level has nothing to choose).
fn bar(chat: &Chat, room: usize, ui: &Ui, now: f64) -> (Vec<Glyph>, Vec<(usize, usize, Spot)>) {
    let space = |cells: usize| glyphs(&" ".repeat(cells), Ink::faint(), 0);
    let over = |spot: Spot| ui.over == Some(spot);
    let unconnected = chat.unconnected();
    let models = ui.live && !unconnected && !chat.models.is_empty();
    let efforts = ui.live && !unconnected && chat.efforts.len() > 1;
    let model = if unconnected {
        let over = over(Spot::Connect);
        let mut ink = Ink::of(ACCENT, if over { 1.0 } else { 0.85 });
        ink.underline = over;
        glyphs(CONNECT, ink, 0)
    } else {
        let open = matches!(ui.pick, Some(Pick::Models(_)));
        let name = chat.model.as_deref().unwrap_or_default();
        chip(
            glyphs(name, Ink::of(INK, 0.88), 0),
            models && (open || over(Spot::Model)),
        )
    };
    let effort = if unconnected {
        Vec::new()
    } else {
        let open = matches!(ui.pick, Some(Pick::Effort));
        chip(effort(chat), efforts && (open || over(Spot::Effort)))
    };
    let both = if effort.is_empty() {
        model.clone()
    } else {
        joined([model.clone(), space(3), effort])
    };
    // Until a reply measures the context, there is nothing to say of it.
    let (mut numbers, mut word) = (Vec::new(), Vec::new());
    if let Some((usage, tokens)) = chat.usage.and_then(|usage| Some((usage, usage.tokens?))) {
        if usage.window > 0 {
            numbers = joined([
                glyphs(&short(tokens), Ink::muted(), 0),
                glyphs(&format!(" of {}", short(usage.window)), Ink::faint(), 0),
            ]);
            word = glyphs(" context", Ink::faint(), 0);
        }
    }
    let stop = stop(chat, ui, now);
    // The stop mark stands before the numbers: they stay where they are when it comes and goes.
    let right = |parts: Vec<Vec<Glyph>>| {
        let mut out = Vec::new();
        for part in parts.into_iter().filter(|part| !part.is_empty()) {
            if !out.is_empty() {
                out.extend(space(2));
            }
            out.extend(part);
        }
        out
    };
    let choices = [
        (
            both.clone(),
            right(vec![stop.clone(), joined([numbers.clone(), word])]),
        ),
        (both.clone(), right(vec![stop.clone(), numbers])),
        (both, stop.clone()),
        (model.clone(), stop.clone()),
    ];
    let fits = |left: &[Glyph], right: &[Glyph]| {
        let right = if right.is_empty() {
            0
        } else {
            width(right) + 2
        };
        width(left) + right <= room
    };
    let named = width(&model);
    let (left, right) = choices
        .into_iter()
        .find(|(left, right)| fits(left, right))
        .unwrap_or_else(|| (fit(model, room.saturating_sub(width(&stop) + 2)), stop));
    // What the left part kept of the model and of the effort.
    let kept = width(&left);
    let mut links = Vec::new();
    if unconnected {
        links.push((0, kept.min(named), Spot::Connect));
    } else if models && named > 0 {
        links.push((0, kept.min(named), Spot::Model));
    }
    if efforts && kept > named + 3 {
        links.push((named + 3, kept, Spot::Effort));
    }
    let gap = room.saturating_sub(kept + width(&right));
    (joined([left, space(gap), right]), links)
}

/// What stands at the bar's right while the agent works or the
/// conversation has a goal: the goal's time, and the mark of what a click
/// does there. The square stops the agent; while a goal is active, it
/// makes the goal wait too. A goal that waits has what lets it go on, and
/// one reached, its check.
fn stop(chat: &Chat, ui: &Ui, now: f64) -> Vec<Glyph> {
    let goal = chat.goal.as_ref();
    let square = || glyphs(STOP.encode_utf8(&mut [0; 4]), Ink::of(INK, 0.8), 0);
    let mark = match goal {
        Some(goal) if goal.going() => square(),
        _ if chat.busy() => square(),
        Some(goal) if goal.waits() => {
            let lit = ui.over == Some(Spot::Play);
            let ink = Ink::accent().alpha(if lit { 1.0 } else { 0.8 });
            glyphs(PLAY.encode_utf8(&mut [0; 4]), ink, 0)
        }
        _ => Vec::new(),
    };
    let done = match goal {
        Some(goal) if goal.status == Status::Complete => glyphs("✓", Ink::accent(), 0),
        _ => Vec::new(),
    };
    let time = goal
        .and_then(|goal| goal.seconds(now))
        .map_or_else(Vec::new, |seconds| glyphs(&clock(seconds), Ink::muted(), 0));
    let mut out = Vec::new();
    for part in [done, time, mark] {
        if !part.is_empty() && !out.is_empty() {
            out.extend(glyphs(" ", Ink::faint(), 0));
        }
        out.extend(part);
    }
    out
}

/// `1.6k`, `100k`, `1.2M`.
fn short(n: u64) -> String {
    let (value, unit) = match n {
        0..1_000 => return n.to_string(),
        1_000..1_000_000 => (n as f64 / 1e3, "k"),
        _ => (n as f64 / 1e6, "M"),
    };
    if value < 10.0 && (value * 10.0).round() % 10.0 != 0.0 {
        format!("{value:.1}{unit}")
    } else {
        format!("{value:.0}{unit}")
    }
}

/// `6s`, `1m 12s`; whole seconds, as the count while it runs showed them.
fn took(seconds: f64) -> String {
    let seconds = (seconds.floor() as u64).max(1);
    match (seconds / 60, seconds % 60) {
        (0, rest) => format!("{rest}s"),
        (minutes, 0) => format!("{minutes}m"),
        (minutes, rest) => format!("{minutes}m {rest}s"),
    }
}

/// A live word: muted, a light crossing it.
fn shining(word: &str, now: f64) -> Vec<Glyph> {
    let letters = word.chars().count();
    let mut out = glyphs(word, Ink::muted(), 0);
    for (at, glyph) in out.iter_mut().enumerate() {
        glyph.ink.alpha = MUTED + (1.0 - MUTED) * shine(now, at, letters);
    }
    out
}

/// Glyph lists end to end.
fn joined(parts: impl IntoIterator<Item = Vec<Glyph>>) -> Vec<Glyph> {
    parts.into_iter().flatten().collect()
}

/// The first rows of `lines` that a fold open by `open` (0 to 1) leaves in view.
fn folded(mut lines: Vec<Line>, open: f64) -> Vec<Line> {
    let keep = (lines.len() as f64 * open.clamp(0.0, 1.0)).round() as usize;
    lines.truncate(keep);
    lines
}

/// What starts a line that folds: a mark in its two cells, then a space.
fn lead(mark: Vec<Glyph>) -> Vec<Glyph> {
    let pad = (LEAD - 1).saturating_sub(width(&mark)) + 1;
    joined([mark, glyphs(&" ".repeat(pad), Ink::faint(), 0)])
}

/// The mark, on a cell's line and on the line of cells in a row, that says
/// which way its fold is: closed, or open.
fn chevron(open: bool) -> Vec<Glyph> {
    glyphs(if open { "▾" } else { "▸" }, Ink::faint(), 0)
}

/// The spinner beside what is under way.
fn spinning(now: f64) -> Vec<Glyph> {
    glyphs(&spinner(now).to_string(), Ink::accent(), 0)
}

/// The mark of a cell. A terminal has no pictures: Python's is its two
/// letters in its blue; a command's, a prompt.
fn mark(python: bool, shell: bool, failed: bool) -> Vec<Glyph> {
    let danger = Ink::danger().alpha(0.85);
    match (python, shell, failed) {
        (false, ..) => glyphs("··", Ink::faint(), 0),
        (true, true, true) => glyphs(">_", danger, 0),
        (true, true, false) => glyphs(">_", Ink::muted(), 0),
        (true, false, true) => glyphs("py", danger, 0),
        (true, false, false) => glyphs("py", Ink::of(PYTHON_BLUE, 1.0), 0),
    }
}

/// The line under the pointer comes up: what is dim on it nears the ink.
fn lifted(mut glyphs: Vec<Glyph>, hover: bool) -> Vec<Glyph> {
    if hover {
        for glyph in &mut glyphs {
            glyph.ink.alpha += (1.0 - glyph.ink.alpha) * 0.7;
        }
    }
    glyphs
}

/// How long after a turn ended nothing of it moves any more: what came in
/// is in, what folded is folded.
const CALM: f64 = 2.0;

/// What a turn that no longer moves drew, kept from one frame to the next:
/// how many rows it takes, and the rows themselves while they are in view.
/// A long conversation is then laid out once, not at every frame.
#[derive(Default)]
pub struct Kept {
    turns: HashMap<usize, KeptTurn>,
    /// What the rows were drawn with, beside each turn's own: the list of
    /// the folder's files (a mention is marked by it), whether a message
    /// can be changed, and the one that is being.
    with: (u64, bool, Option<usize>),
}

struct KeptTurn {
    /// What the rows were drawn from: they are kept while it stays the same.
    from: (usize, u64, (usize, u64, u64)),
    rows: usize,
    lines: Option<Vec<Line>>,
}

impl Kept {
    /// The rows kept of turn `index`, laid out again by `lay` if they were let go of.
    fn lines(&mut self, index: usize, lay: impl FnOnce() -> Vec<Line>) -> &[Line] {
        match self.turns.get_mut(&index) {
            Some(turn) => turn.lines.get_or_insert_with(lay),
            None => &[],
        }
    }

    /// Turn `index` is out of view: its rows are let go of, their count kept.
    fn rest(&mut self, index: usize) {
        if let Some(turn) = self.turns.get_mut(&index) {
            turn.lines = None;
        }
    }
}

/// Nothing of the turn moves at `now`: it ended a while ago, no fold of it
/// is on its way, the pointer is not on it.
fn still(turn: &Turn, index: usize, ui: &Ui, now: f64) -> bool {
    turn.ended.is_some_and(|ended| now - ended > CALM)
        && now - turn.at > CALM
        && !ui.folds.moving_in(index, ui.wall)
        && ui.hover.and_then(Fold::turn) != Some(index)
}

/// A stretch of the conversation's rows: a turn.
struct Part {
    /// Its first row in the conversation, and how many it takes.
    from: usize,
    rows: usize,
    /// The turn it is, when it is one.
    turn: Option<usize>,
    /// Its rows; a turn that no longer moves has them kept instead.
    lines: Option<Vec<Line>>,
}

/// The rows of the page between two turns: one more than between the
/// user's message and what answers it, so a message stays with its answer.
const BETWEEN: usize = 2;

/// The conversation, `width` cells wide: its turns one under the other, two
/// rows of the page between them. The messages that wait are on the
/// message box ([`queue`]).
fn transcript(chat: &Chat, ui: &Ui, width: usize, now: f64, kept: &mut Kept) -> Vec<Part> {
    kept.turns.retain(|index, _| *index < chat.turns.len());
    let drawn = (
        ui.listed,
        ui.editable,
        ui.editing.as_ref().and_then(Editing::turn),
    );
    if kept.with != drawn {
        kept.turns.clear();
        kept.with = drawn;
    }
    let mut parts: Vec<Part> = Vec::new();
    let mut place = |rows: usize, turn: Option<usize>, lines: Option<Vec<Line>>| {
        let end = parts.last().map_or(0, |part| part.from + part.rows);
        parts.push(Part {
            from: if end > 0 { end + BETWEEN } else { 0 },
            rows,
            turn,
            lines,
        });
    };
    for (index, turn) in chat.turns.iter().enumerate() {
        if !still(turn, index, ui, now) {
            kept.turns.remove(&index);
            let lines = turn_lines(turn, index, ui, width, now);
            place(lines.len(), Some(index), Some(lines));
            continue;
        }
        let ended = turn.ended.map_or(0, f64::to_bits);
        let from = (
            width,
            ui.folds.print(index),
            (turn.items.len(), turn.at.to_bits(), ended),
        );
        let rows = match kept.turns.get(&index) {
            Some(turn) if turn.from == from => turn.rows,
            _ => {
                let lines = turn_lines(turn, index, ui, width, now);
                let rows = lines.len();
                let lines = Some(lines);
                kept.turns.insert(index, KeptTurn { from, rows, lines });
                rows
            }
        };
        place(rows, Some(index), None);
    }
    parts
}

/// A turn's items, each run of cells in a row gathered.
enum Piece {
    Item(usize),
    Run(usize, usize),
}

fn pieces(items: &[Item]) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::new();
    for (at, item) in items.iter().enumerate() {
        let tool = matches!(item.kind, Kind::Tool { .. });
        match out.last_mut() {
            Some(Piece::Run(_, end)) if tool => *end = at + 1,
            _ if tool => out.push(Piece::Run(at, at + 1)),
            _ => out.push(Piece::Item(at)),
        }
    }
    out
}

/// Where a done turn's answer begins: the text (and notices) it ended on.
/// What came before it folds, if a cell ran there; else nothing does.
fn answer_at(turn: &Turn) -> Option<usize> {
    if turn.state != TurnState::Done {
        return None;
    }
    let mut at = turn.items.len();
    while at > 0 && matches!(turn.items[at - 1].kind, Kind::Text(_) | Kind::Notice { .. }) {
        at -= 1;
    }
    turn.items[..at]
        .iter()
        .any(|item| matches!(item.kind, Kind::Tool { .. }))
        .then_some(at)
}

/// What a turn starts with: the user's message on its card, which a
/// click brings back to the message box when the conversation can go back
/// to before it; or what woke the agent.
fn opening(turn: &Turn, index: usize, ui: &Ui, width: usize, now: f64) -> Vec<Line> {
    if let Some(prompt) = &turn.prompt {
        let editing = ui.editing.as_ref().and_then(Editing::turn);
        let fold = Fold::Prompt(index);
        let opens = ui.editable && turn.rewind.is_some() && editing != Some(index);
        let card = Card {
            alpha: progress(now, turn.at, FADE_IN),
            fold: opens.then_some(fold),
            lit: opens && ui.hover == Some(fold),
            changed: editing == Some(index),
        };
        return bubble(prompt, &turn.images, width, ui.files.as_deref(), card);
    }
    let Some(wake) = &turn.wake else {
        return Vec::new();
    };
    let text = format!("The agent went on by itself: {wake}.");
    wrap(glyphs(&text, Ink::muted(), 0), width)
        .into_iter()
        .map(Line::of)
        .collect()
}

/// A turn, the conversation's `index`th.
fn turn_lines(turn: &Turn, index: usize, ui: &Ui, width: usize, now: f64) -> Vec<Line> {
    let mut out = Vec::new();
    // Where the conversation stops being what sending the changed message keeps.
    let editing = ui.editing.as_ref().and_then(Editing::turn);
    out.extend(opening(turn, index, ui, width, now));
    let kept = if turn.prompt.is_some() {
        out.len()
    } else {
        usize::MAX
    };
    let running = turn.running();
    let answer = answer_at(turn);
    let split = answer.unwrap_or(turn.items.len());
    let mut work = Vec::new();
    let mut rest = Vec::new();
    for piece in pieces(&turn.items) {
        let (first, block) = match piece {
            Piece::Item(at) => (at, item_lines(turn, index, at, ui, width, now)),
            Piece::Run(start, end) => (start, run_lines(turn, index, start..end, ui, width, now)),
        };
        if block.is_empty() {
            continue;
        }
        let target = if first < split { &mut work } else { &mut rest };
        if !target.is_empty() {
            target.push(Line::blank());
        }
        target.extend(block);
    }
    let mut section = |lines: Vec<Line>| {
        if !lines.is_empty() {
            if !out.is_empty() {
                out.push(Line::blank());
            }
            out.extend(lines);
        }
    };
    match (answer, turn.ended) {
        // What it did before its answer folds in one move into a line.
        (Some(_), Some(ended)) => {
            let fold = Fold::Work(index);
            let tools = turn.items[..split]
                .iter()
                .filter(|item| matches!(item.kind, Kind::Tool { .. }))
                .count();
            let head = joined([
                lead(glyphs("≡", Ink::faint(), 0)),
                glyphs("Worked", Ink::muted(), 0),
                // A conversation that came back shows it without its time.
                glyphs(
                    &if turn.replayed {
                        String::new()
                    } else {
                        format!(" for {}", took(ended - turn.began))
                    },
                    Ink::faint(),
                    0,
                ),
                glyphs(
                    &format!(" · {tools} cell{} ", if tools == 1 { "" } else { "s" }),
                    Ink::faint(),
                    0,
                ),
            ]);
            let mut head = lifted(head, ui.hover == Some(fold));
            let rule = width.saturating_sub(self::width(&head));
            head.extend(glyphs(&"─".repeat(rule), Ink::of(INK, 0.1), 0));
            section(vec![Line::of(head)
                .faded(progress(now, ended, FADE_IN))
                .heads(fold)]);
            let closing = 1.0 - progress(now, ended, FOLD);
            section(folded(work, ui.folds.amount(fold, ui.wall, closing)));
        }
        _ => section(work),
    }
    section(rest);
    if running && !busy(turn, now) {
        let dot = glyphs("●", Ink::accent().alpha(0.35 + 0.65 * breathe(now)), 0);
        section(vec![Line::of(joined([lead(dot), shining("Working", now)]))]);
    }
    match &turn.state {
        TurnState::Cancelled => section(vec![Line::of(glyphs("Stopped", Ink::faint(), 0))]),
        TurnState::Failed(error) => section(
            wrap(glyphs(error, Ink::danger(), 0), width)
                .into_iter()
                .map(Line::of)
                .collect(),
        ),
        TurnState::Running | TurnState::Done => {}
    }
    // Sending the message being changed takes away its answer, and what came after.
    let leaves = match editing {
        Some(editing) if index > editing => 0,
        Some(editing) if index == editing => kept.min(out.len()),
        _ => out.len(),
    };
    for line in &mut out[leaves..] {
        line.alpha *= LEAVES;
    }
    out
}

/// Reasoning, a running cell and the answer as it streams show their own
/// life; otherwise (a pause: the model writing a cell, say) the turn says it works.
fn busy(turn: &Turn, now: f64) -> bool {
    match turn.items.last().map(|item| &item.kind) {
        Some(Kind::Text(stream)) => stream.wave.last().is_some_and(|last| now - last < QUIET),
        Some(Kind::Thought { ended, .. } | Kind::Compaction { ended, .. }) => ended.is_none(),
        Some(Kind::Tool { status, .. }) => *status == ToolStatus::Running,
        _ => false,
    }
}

/// The user's message: a card on the left, where the answer starts too (a
/// terminal's column stops short of the screen's right edge, so a card on
/// the right would float in the middle, or stick to the panel). It is as
/// wide as its text, a row of its surface over and under it, its words set
/// in as the message box sets them, its mentions of `files` marked; then
/// its images, each by its name, a row of the card after the words.
fn bubble(
    prompt: &str,
    images: &[Attached],
    width: usize,
    files: Option<&Paths>,
    card: Card,
) -> Vec<Line> {
    let most = (width * 88 / 100).max(12).min(width);
    let inner = most.saturating_sub(4);
    let mut text = glyphs(prompt, Ink::text(), 0);
    marked(&mut text, prompt, files);
    let mut rows = if prompt.is_empty() {
        Vec::new()
    } else {
        wrap(text, inner)
    };
    if !images.is_empty() {
        if !rows.is_empty() {
            rows.push(Vec::new());
        }
        let mut names = Vec::new();
        for attached in images {
            if !names.is_empty() {
                names.extend(glyphs("   ", Ink::faint(), 0));
            }
            names.extend(image(attached, Ink::muted(), inner.saturating_sub(2)));
        }
        rows.extend(wrap(names, inner));
    }
    let widest = rows.iter().map(|row| self::width(row)).max().unwrap_or(0);
    let to = widest + 4;
    let surface = match (card.changed, card.lit) {
        (true, _) => RAISED_2.mix(ACCENT, 0.14),
        (false, true) => RAISED_2.mix(INK, 0.06),
        (false, false) => RAISED_2,
    };
    let mut out = vec![Line::surface(surface, 0, to)];
    for row in rows {
        out.push(Line::at(2, row).on(surface, 0, to));
    }
    out.push(Line::surface(surface, 0, to));
    (out.into_iter())
        .map(|line| {
            let line = line.faded(card.alpha);
            match card.fold {
                Some(fold) => line.heads(fold),
                None => line,
            }
        })
        .collect()
}

/// How a message's card shows: how far in it is; the fold a click on it
/// stands for, when the message can be changed; whether the pointer is on
/// it; whether it is the one being changed.
#[derive(Clone, Copy)]
struct Card {
    alpha: f64,
    fold: Option<Fold>,
    lit: bool,
    changed: bool,
}

fn item_lines(turn: &Turn, index: usize, at: usize, ui: &Ui, width: usize, now: f64) -> Vec<Line> {
    let item = &turn.items[at];
    let running = turn.running();
    match &item.kind {
        Kind::Thought {
            stream,
            started,
            ended,
        } => {
            let ended = if running {
                *ended
            } else {
                ended.or(turn.ended)
            };
            thought(
                stream,
                // How long a replayed reasoning took was not kept.
                (!turn.replayed).then_some(*started),
                ended,
                Fold::Item(index, at),
                ui,
                width,
                now,
            )
        }
        Kind::Text(stream) => {
            let streaming = running && at + 1 == turn.items.len();
            let text = if streaming {
                markdown::healed(&stream.text)
            } else {
                stream.text.as_str().into()
            };
            let mut lines = markdown::render(&text, width, Ink::text());
            waved(&mut lines, stream, now, WAVE_FADE);
            lines
        }
        Kind::Tool { .. } => tool(item, Fold::Item(index, at), running, ui, width, now),
        Kind::Notice { tone, text } => {
            let ink = match tone {
                Tone::Info => Ink::muted(),
                Tone::Warning => Ink::text(),
                Tone::Error => Ink::danger(),
            };
            wrap(glyphs(text, ink, 0), width)
                .into_iter()
                .map(|row| Line::of(row).faded(progress(now, item.at, FADE_IN)))
                .collect()
        }
        Kind::Compaction { .. } => compaction(item, Fold::Item(index, at), ui, width, now),
        Kind::Kernel { restored: true } => {
            // It streams in blue, as the answer streams in, then settles in the ink.
            let mut words = glyphs("Python kernel restored", Ink::text(), 0);
            landed(&mut words, now - item.at);
            wrap(words, width).into_iter().map(Line::of).collect()
        }
        Kind::Kernel { restored: false } => {
            let words = glyphs("The Python kernel could not be restored", Ink::text(), 0);
            wrap(words, width)
                .into_iter()
                .map(|row| Line::of(row).faded(progress(now, item.at, FADE_IN)))
                .collect()
        }
    }
}

/// How long a compaction's rules take to grow out of it.
const SEAMS: f64 = 0.7;

/// Where the conversation was compacted (the extension's mark), across
/// the conversation. While it runs, its words shine beside the spinner;
/// once over, "Compacted" and how many tokens it summarized, between two
/// rules. Landing, its letters come in blue and settle, the rules grow out
/// of it. A click opens the summary under it, on a surface of its own.
fn compaction(item: &Item, fold: Fold, ui: &Ui, width: usize, now: f64) -> Vec<Line> {
    let Kind::Compaction {
        auto,
        ended,
        tokens,
        summary,
    } = &item.kind
    else {
        return Vec::new();
    };
    let centred = |words: Vec<Glyph>, rule: f64| {
        let words = fit(words, width);
        let room = width.saturating_sub(self::width(&words) + 4);
        let (left, right) = (room / 2, room - room / 2);
        // Each rule grows from the words out.
        let grown = |cells: usize| (cells as f64 * rule).round() as usize;
        let seam = |cells: usize| glyphs(&"─".repeat(grown(cells)), Ink::of(INK, 0.1), 0);
        let space = |cells: usize| glyphs(&" ".repeat(cells), Ink::faint(), 0);
        joined([
            space(left - grown(left)),
            seam(left),
            space(2),
            words,
            space(2),
            seam(right),
        ])
    };
    let Some(ended) = ended else {
        let words = joined([spinning(now), glyphs(" ", Ink::faint(), 0)]);
        let words = joined([words, shining("Compacting the conversation", now)]);
        let line = Line::of(centred(words, 0.0)).faded(progress(now, item.at, FADE_IN));
        return vec![line];
    };
    let label = if *auto {
        "Compacted automatically"
    } else {
        "Compacted"
    };
    let mut words = glyphs(label, Ink::text(), 0);
    words.extend(glyphs(
        &format!(" · {} tokens summarized", short(*tokens)),
        Ink::faint(),
        0,
    ));
    landed(&mut words, now - ended);
    let opens = summary.is_some();
    let words = lifted(words, opens && ui.hover == Some(fold));
    let line = Line::of(centred(words, progress(now, *ended, SEAMS)));
    let Some(summary) = summary else {
        return vec![line];
    };
    let mut out = vec![line.heads(fold)];
    let open = ui.folds.amount(fold, ui.wall, 0.0);
    if open > 0.0 {
        // A row of the page, then the summary on its surface, set in by two cells.
        let face = RAISED;
        let pad = Line::surface(face, 0, width);
        let mut under = vec![Line::blank(), pad.clone()];
        let rows = markdown::render(summary, width.saturating_sub(4), Ink::muted());
        under.extend(rows.into_iter().map(|row| {
            let row = row.shifted(2);
            if row.fill.is_some() {
                row
            } else {
                row.on(face, 0, width)
            }
        }));
        under.push(pad);
        out.extend(folded(under, open));
    }
    out
}

/// `glyphs` landing, `since` seconds after they started to: each comes in
/// blue, a little after the one before it, then turns to its own colour.
pub fn landed(glyphs: &mut [Glyph], since: f64) {
    let of = glyphs.len();
    for (at, glyph) in glyphs.iter_mut().enumerate() {
        let (inn, blue) = landing(since, at, of);
        let own = glyph.ink;
        glyph.ink.rgb = own.rgb.mix(ACCENT, blue);
        glyph.ink.alpha = (own.alpha + (1.0 - own.alpha) * blue) * inn;
    }
}

/// The streaming wave over a text's rows: each glyph is as far in as the
/// front has brought it, a glyph taking `fade` seconds to come in.
fn waved(lines: &mut [Line], stream: &Stream, now: f64, fade: f64) {
    if stream.wave.settled(now, fade) {
        return;
    }
    for glyph in lines.iter_mut().flat_map(|line| &mut line.glyphs) {
        glyph.ink.alpha *= stream.wave.alpha(glyph.src, now, fade);
    }
}

/// The streaming wave over the reasoning's rows. Its words are grey, and
/// from the page to grey the front hardly shows: so each comes in as the
/// answer's words do, up to the ink, then cools to its own grey.
fn warmed(lines: &mut [Line], stream: &Stream, now: f64) {
    if stream.wave.settled(now, WAVE_FADE + THOUGHT_COOL) {
        return;
    }
    for glyph in lines.iter_mut().flat_map(|line| &mut line.glyphs) {
        let Some(since) = stream.wave.since(glyph.src, now) else {
            continue;
        };
        let inn = (since / WAVE_FADE).clamp(0.0, 1.0) as f32;
        let cooled = ((since - WAVE_FADE) / THOUGHT_COOL).clamp(0.0, 1.0) as f32;
        // Slow at first, so the bright front is as long as the answer's.
        let warm = 1.0 - cooled * cooled * (3.0 - 2.0 * cooled);
        let own = glyph.ink.alpha;
        glyph.ink.alpha = inn * (own + (1.0 - own) * warm);
    }
}

/// The model's reasoning. While it thinks, its words stream in a window
/// that grows to a few rows, then keeps the newest in view under a fade;
/// once it is over it folds into "Thought for 6s", which a click opens whole.
fn thought(
    stream: &Stream,
    started: Option<f64>,
    ended: Option<f64>,
    fold: Fold,
    ui: &Ui,
    width: usize,
    now: f64,
) -> Vec<Line> {
    let live = ended.is_none();
    let text = stream.text.trim();
    let opens = !live && !text.is_empty();
    let head = if live {
        let seconds = started.map_or(0.0, |started| now - started);
        joined([
            lead(spinning(now)),
            shining("Thinking", now),
            glyphs(
                &if seconds < 1.0 {
                    String::new()
                } else {
                    format!(" {}", took(seconds))
                },
                Ink::faint(),
                0,
            ),
        ])
    } else {
        // Its mark where the spinner was. No chevron: only the cells' lines carry one.
        joined([
            lead(glyphs("✦", Ink::faint(), 0)),
            glyphs("Thought", Ink::muted(), 0),
            glyphs(
                &started.map_or_else(String::new, |started| {
                    format!(" for {}", took(ended.unwrap_or(now) - started))
                }),
                Ink::faint(),
                0,
            ),
        ])
    };
    let mut head = Line::of(lifted(head, opens && ui.hover == Some(fold)));
    if opens {
        head = head.heads(fold);
    }
    let mut out = vec![head];
    let mut rows: Vec<Line> = wrap(glyphs(text, Ink::muted(), 0), width.saturating_sub(LEAD))
        .into_iter()
        .map(|row| Line::at(LEAD, row))
        .collect();
    warmed(&mut rows, stream, now);
    // A row of the page between the line and the reasoning under it.
    let under = |rows: Vec<Line>| {
        let blank = (!rows.is_empty()).then(Line::blank);
        blank.into_iter().chain(rows)
    };
    if opens && ui.folds.held(fold) {
        // The user's fold: the reasoning whole.
        let rows = under(rows).collect();
        out.extend(folded(rows, ui.folds.amount(fold, ui.wall, 0.0)));
        return out;
    }
    // Under way, its newest rows; then they leave, the oldest first.
    let over = rows.len().saturating_sub(THOUGHT_ROWS);
    rows.drain(..over);
    if over > 0 {
        // The rows about to leave the window fade.
        for (row, alpha) in rows.iter_mut().zip([0.3, 0.65]) {
            row.alpha *= alpha;
        }
    }
    // Over, it folds at once: what the agent does next does not find it open.
    if let Some(ended) = ended {
        let closing = progress(now, ended, FOLD);
        let keep = (rows.len() as f64 * (1.0 - closing)).round() as usize;
        let gone = rows.len() - keep;
        rows.drain(..gone);
    }
    out.extend(under(rows));
    out
}

/// A cell the agent runs, as one line: its mark, what it does, the command
/// it runs (else its telling line of code), and once it ran, the exception
/// it raised. A click opens its code and what came out of it.
fn tool(item: &Item, fold: Fold, running: bool, ui: &Ui, width: usize, now: f64) -> Vec<Line> {
    let Kind::Tool {
        title,
        status,
        input,
        output,
        error,
        images,
        ..
    } = &item.kind
    else {
        return Vec::new();
    };
    let live = running && *status == ToolStatus::Running;
    let failed = *status == ToolStatus::Failed;
    let cell = (title == "Python cell").then(|| summary(input));
    let shell = cell.as_ref().is_some_and(|cell| cell.shell);
    let start = if live {
        joined([lead(spinning(now)), shining("Running", now)])
    } else {
        let verb = if failed {
            glyphs("Failed", Ink::danger(), 0)
        } else {
            glyphs("Ran", Ink::muted(), 0)
        };
        joined([lead(mark(cell.is_some(), shell, failed)), verb])
    };
    let opens = cell.is_some();
    let target = cell.map_or_else(|| title.clone(), |cell| cell.target);
    let meta = match error {
        Some(error) if failed => joined([
            glyphs(" · ", Ink::faint(), 0),
            glyphs(error, Ink::danger().alpha(0.85), 0),
        ]),
        // The images it loaded into the context.
        _ if *images > 0 => {
            let plural = if *images == 1 { "" } else { "s" };
            glyphs(
                &format!(" · {images} image{plural} loaded"),
                Ink::faint(),
                0,
            )
        }
        _ => Vec::new(),
    };
    let end = if opens {
        joined([glyphs(" ", Ink::faint(), 0), chevron(ui.folds.open(fold))])
    } else {
        Vec::new()
    };
    let taken = self::width(&start) + 1 + self::width(&meta) + self::width(&end);
    let head = joined([
        start,
        glyphs(" ", Ink::muted(), 0),
        fit(
            glyphs(&target, Ink::of(INK, 0.82), 0),
            width.saturating_sub(taken),
        ),
        meta,
        end,
    ]);
    let head = Line::of(lifted(head, opens && ui.hover == Some(fold)))
        .faded(progress(now, item.at, FADE_IN));
    if !opens {
        return vec![head];
    }
    let mut out = vec![head.heads(fold)];
    let open = ui.folds.amount(fold, ui.wall, 0.0);
    if open > 0.0 {
        // A row of the page between the line and what it opens on.
        let mut under = vec![Line::blank()];
        under.extend(sheet(input, output.as_deref(), failed, width));
        out.extend(folded(under, open));
    }
    out
}

/// What a cell opens on, one sheet without labels under the line's words:
/// its Python, then a tone deeper what came out of it.
fn sheet(code: &str, output: Option<&str>, failed: bool, width: usize) -> Vec<Line> {
    let room = width.saturating_sub(LEAD + 4);
    if room == 0 {
        return Vec::new();
    }
    let part = |glyphs: Vec<Glyph>, most: usize, face| {
        let mut rows = wrap_hard(glyphs, room);
        let more = rows.len().saturating_sub(most);
        rows.truncate(most);
        if more > 0 {
            let text = format!("… {more} more line{}", if more == 1 { "" } else { "s" });
            rows.push(self::glyphs(&text, Ink::faint(), 0));
        }
        let pad = Line::surface(face, LEAD, width);
        let rows = rows
            .into_iter()
            .map(move |row| Line::at(LEAD + 2, row).on(face, LEAD, width));
        std::iter::once(pad.clone()).chain(rows).chain([pad])
    };
    let mut out: Vec<Line> =
        part(python(code.trim_end(), 0), CODE_ROWS, crate::theme::code()).collect();
    let output = output
        .map(str::trim_end)
        .filter(|output| !output.is_empty());
    if let Some(output) = output {
        let ink = if failed {
            Ink::danger().alpha(0.9)
        } else {
            Ink::muted()
        };
        out.extend(part(glyphs(output, ink, 0), OUTPUT_ROWS, sunk()));
    }
    out
}

/// Cells in a row, `cells` of the turn's items. While they run, each shows
/// on its own; once the agent thinks or writes again (or the turn ends),
/// two or more fold into one line, "Ran 5 Python cells".
fn run_lines(
    turn: &Turn,
    index: usize,
    cells: std::ops::Range<usize>,
    ui: &Ui,
    width: usize,
    now: f64,
) -> Vec<Line> {
    let running = turn.running();
    let count = cells.len();
    let lines = |width: usize| -> Vec<Line> {
        let mut out = Vec::new();
        for at in cells.clone() {
            // A row of the page between two cells, running or not, open or not.
            if !out.is_empty() {
                out.push(Line::blank());
            }
            let item = &turn.items[at];
            out.extend(tool(item, Fold::Item(index, at), running, ui, width, now));
        }
        out
    };
    let since = turn.items.get(cells.end).map(|item| item.at).or(turn.ended);
    let Some(since) = since.filter(|_| count > 1) else {
        return lines(width);
    };
    let fold = Fold::Run(index, cells.start);
    let head = joined([
        lead(mark(true, false, false)),
        glyphs("Ran", Ink::muted(), 0),
        glyphs(&format!(" {count} Python cells "), Ink::of(INK, 0.82), 0),
        chevron(ui.folds.open(fold)),
    ]);
    let head = Line::of(lifted(head, ui.hover == Some(fold)))
        .faded(progress(now, since, FADE_IN))
        .heads(fold);
    let mut out = vec![head];
    let closing = 1.0 - progress(now, since, FOLD);
    // A row of the page between the line and the cells under it.
    let under = std::iter::once(Line::blank()).chain(
        lines(width.saturating_sub(LEAD))
            .into_iter()
            .map(|line| line.shifted(LEAD)),
    );
    out.extend(folded(
        under.collect(),
        ui.folds.amount(fold, ui.wall, closing),
    ));
    out
}

/// What a click acts on in a row of the panel: the row, its cells, the spot.
struct Mark {
    row: usize,
    from: usize,
    to: usize,
    spot: Spot,
}

/// The panel's rows, `inner` cells wide: the servers under their title,
/// then the variables under theirs; or, while the kernel has nothing,
/// what it will show. Answers them, and what a click acts on in them.
fn panel_rows(kernel: &Kernel, ui: &Ui, inner: usize, now: f64) -> (Vec<Line>, Vec<Mark>) {
    let title = |text: &str| Line::of(glyphs(text, Ink::text().bold(), 0));
    let mut lines = Vec::new();
    let mut marks = Vec::new();
    if kernel.is_empty() {
        lines.push(title("Kernel"));
        lines.push(Line::blank());
        let empty =
            "The variables of the agent's REPL and the servers it leaves running show here.";
        lines.extend(
            wrap(glyphs(empty, Ink::faint(), 0), inner)
                .into_iter()
                .map(Line::of),
        );
    }
    if !kernel.commands.is_empty() {
        lines.push(title("Running servers"));
        lines.push(Line::blank());
        for command in &kernel.commands {
            let (rows, spots) = command_lines(command, ui, inner, now);
            let row = lines.len();
            marks.extend((spots.into_iter()).map(|(from, to, spot)| Mark {
                row,
                from,
                to,
                spot,
            }));
            lines.extend(rows);
            // Open, a command keeps a row of the panel before the next one.
            if lines.last().is_some_and(|line| line.fill.is_some()) {
                lines.push(Line::blank());
            }
        }
        if lines
            .last()
            .is_some_and(|line| line.fill.is_some() || !line.glyphs.is_empty())
        {
            lines.push(Line::blank());
        }
    }
    if !kernel.variables.is_empty() {
        let count = kernel
            .variables
            .iter()
            .filter(|row| row.left.is_none())
            .count();
        let count = glyphs(&count.to_string(), Ink::faint(), 0);
        // The title in full where it fits beside the count, else shorter.
        let room = inner.saturating_sub(width(&count) + 1);
        let words = ["Agent's REPL variables", "REPL variables", "Variables"]
            .into_iter()
            .find(|words| words.chars().count() <= room)
            .unwrap_or("Variables");
        let mut title = fit(glyphs(words, Ink::text().bold(), 0), room);
        let gap = inner.saturating_sub(width(&title) + width(&count));
        title.extend(glyphs(&" ".repeat(gap), Ink::faint(), 0));
        title.extend(count);
        lines.push(Line::of(fit(title, inner)));
        lines.push(Line::blank());
        for variable in &kernel.variables {
            lines.extend(variable_lines(variable, ui, inner, now));
        }
    }
    // What a row opened on leaves no empty row at the panel's end to scroll to.
    while lines
        .last()
        .is_some_and(|line| line.fill.is_none() && line.glyphs.is_empty())
    {
        lines.pop();
    }
    (lines, marks)
}

/// The kernel's panel, a surface on the right: the servers the agent left
/// running, then the variables of its REPL, each a row. A click on a row
/// opens it: a server's last lines, the start of a variable's value. A
/// server's row ends on what stops it, and a click on its port opens it in
/// the browser. With more rows than the screen, it shows them from the row
/// the user scrolled it to. Answers the rows a click opens, what else a
/// click acts on, and how many rows it can scroll.
fn panel(
    canvas: &mut Canvas,
    kernel: &Kernel,
    ui: &Ui,
    area: (u16, u16, u16, u16),
    now: f64,
) -> (Vec<Hit>, Vec<Area>, usize) {
    let (x, y, w, h) = area;
    if h < 4 || w < 12 {
        return (Vec::new(), Vec::new(), 0);
    }
    let (lines, marks) = panel_rows(kernel, ui, usize::from(w - 4), now);
    // With more rows than the panel has, its last one is kept to say how
    // many there are under it: a row never gives its place to that.
    let whole = usize::from(h - 2);
    let rows = if lines.len() > whole {
        whole - 1
    } else {
        whole
    };
    let most = lines.len().saturating_sub(rows);
    let top = ui.panel_top.min(most);
    let under = lines.len().saturating_sub(top + rows);
    let more = (under > 0).then(|| Line::of(glyphs(&format!("… {under} more"), Ink::faint(), 0)));
    let surface = Line::surface(RAISED, 0, usize::from(w));
    // What the kernel first has to show comes in over the empty panel's
    // words; all of it comes in as the panel opens.
    let shown = kernel
        .opened
        .map_or(1.0, |opened| progress(now, opened, FADE_IN))
        * progress(ui.wall, ui.panel_at, FADE_IN);
    for row in 0..h {
        canvas.line(x, y + row, w, &surface);
    }
    // What closes it, in its top right corner: where the rail's mark is.
    let close = Spot::Panel;
    let ink = if ui.over.as_ref() == Some(&close) {
        Ink::text()
    } else {
        Ink::faint()
    };
    canvas.put(x + w - 2, y, x + w, &glyphs(CLOSE_PANEL, ink, 0), 1.0);
    if let Some(more) = more {
        canvas.line(x + 2, y + 1 + cells(rows), w - 4, &more.faded(shown));
    }
    let mut hits = Vec::new();
    for (row, line) in lines.into_iter().enumerate().skip(top).take(rows) {
        let y = y + 1 + cells(row - top);
        if let Some(fold) = line.fold {
            hits.push(Hit {
                y,
                from: x + 2,
                to: x + w - 2,
                fold,
            });
        }
        canvas.line(x + 2, y, w - 4, &line.faded(shown));
    }
    let mut spots: Vec<Area> = (marks.into_iter())
        .filter(|mark| (top..top + rows).contains(&mark.row))
        .map(|mark| Area {
            y: y + 1 + cells(mark.row - top),
            from: x + 2 + cells(mark.from),
            to: (x + 2 + cells(mark.to)).min(x + w),
            spot: mark.spot,
        })
        .collect();
    spots.push(Area {
        y,
        from: x + w - 3,
        to: x + w,
        spot: close,
    });
    (hits, spots, most)
}

/// The kernel's panel, closed: a rail at the screen's edge, with on top
/// what opens it again, and under it a dot while a command the agent left
/// runs. A click anywhere on it opens the panel.
fn rail(
    canvas: &mut Canvas,
    kernel: &Kernel,
    ui: &Ui,
    area: (u16, u16, u16, u16),
    now: f64,
) -> Vec<Area> {
    let (x, y, w, h) = area;
    let over = ui.over == Some(Spot::Panel);
    let surface = Line::surface(if over { RAISED_2 } else { RAISED }, 0, usize::from(w));
    for row in 0..h {
        canvas.line(x, y + row, w, &surface);
    }
    let shown = progress(ui.wall, ui.panel_at, FADE_IN) as f32;
    let ink = if over { Ink::text() } else { Ink::muted() };
    canvas.put(x + 1, y, x + w, &glyphs(OPEN_PANEL, ink, 0), shown);
    let running = (kernel.commands.iter()).any(|command| command.phase == Phase::Running);
    if running && h > 3 {
        let dot = glyphs("●", Ink::accent().alpha(0.4 + 0.6 * breathe(now)), 0);
        canvas.put(x + 1, y + 2, x + w, &dot, shown);
    }
    (0..h)
        .map(|row| Area {
            y: y + row,
            from: x,
            to: x + w,
            spot: Spot::Panel,
        })
        .collect()
}

/// How much of a row shows: it comes in when listed, and fades as it leaves.
fn presence(now: f64, shown: f64, leaves: Option<f64>) -> f64 {
    let leaving = leaves.map_or(0.0, |leaves| progress(now, leaves, LEAVE));
    progress(now, shown, FADE_IN) * (1.0 - leaving)
}

/// What a row of the panel opens on: under a row of the panel, rows on the
/// page's grey, set in from the panel's edge.
fn inset(rows: impl IntoIterator<Item = Vec<Glyph>>, inner: usize) -> impl Iterator<Item = Line> {
    let rows = rows
        .into_iter()
        .map(move |row| Line::at(1, fit(row, inner.saturating_sub(2))).on(BG, 0, inner));
    std::iter::once(Line::blank()).chain(rows)
}

/// The cells a command's text keeps before a second port shows beside it.
const COMMAND_ROOM: usize = 12;

/// A command: its mark, its text, its ports, its time, and while it runs
/// what stops it. Open, its last lines under it. Answers its rows, and
/// what a click acts on in the first: its cells, the spot.
fn command_lines(
    command: &Command,
    ui: &Ui,
    inner: usize,
    now: f64,
) -> (Vec<Line>, Vec<(usize, usize, Spot)>) {
    let fold = Fold::Command(command.key);
    let alpha = presence(now, command.shown, command.leaves());
    let running = command.phase == Phase::Running;
    let mark = match command.phase {
        Phase::Running => glyphs("●", Ink::accent().alpha(0.4 + 0.6 * breathe(now)), 0),
        Phase::Ok => glyphs("✓", Ink::accent(), 0),
        Phase::Killed | Phase::Ended => glyphs("●", Ink::faint(), 0),
    };
    let time = match command.phase {
        Phase::Running => took(now - command.started),
        Phase::Killed => "stopped".to_string(),
        Phase::Ok | Phase::Ended => took(command.duration.unwrap_or(0.0)),
    };
    let time = glyphs(&format!(" {time}"), Ink::faint(), 0);
    // What stops it, at the row's end.
    let kill = Spot::Kill(command.id.clone());
    let end = if running {
        let ink = if ui.over.as_ref() == Some(&kill) {
            Ink::text()
        } else {
            Ink::faint()
        };
        joined([glyphs(" ", Ink::faint(), 0), glyphs(END, ink, 0)])
    } else {
        Vec::new()
    };
    let after = width(&time) + width(&end);
    let mut right = Vec::new();
    // The ports' chips: where each starts in what stands at the row's right, and how wide it is.
    let mut chips = Vec::new();
    if running {
        // Its first port, and a second where the command keeps room to be read.
        for (at, port) in command.ports.iter().take(2).enumerate() {
            let chip = format!(" :{port} ");
            let taken = 2 + width(&right) + 1 + chip.chars().count() + after;
            if at > 0 && inner < taken + COMMAND_ROOM {
                break;
            }
            right.extend(glyphs(" ", Ink::faint(), 0));
            let over = ui.over == Some(Spot::Port(*port));
            let ink = Ink::accent().on(ACCENT, if over { 0.26 } else { 0.13 });
            chips.push((width(&right), chip.chars().count(), *port));
            right.extend(glyphs(&chip, ink, 0));
        }
    }
    right.extend(time);
    right.extend(end);
    let room = inner.saturating_sub(2 + width(&right));
    let mut text = fit(glyphs(&command.line, Ink::of(INK, 0.86), 0), room);
    // It ended well: a light runs through it.
    if let (Phase::Ok, Some(ended)) = (command.phase, command.ended) {
        for (at, glyph) in text.iter_mut().enumerate() {
            glyph.ink.rgb = lit(INK, now - ended, at);
        }
    }
    let gap = room.saturating_sub(width(&text));
    let from = 2 + width(&text) + gap;
    let mut spots: Vec<(usize, usize, Spot)> = (chips.into_iter())
        .map(|(at, wide, port)| (from + at, from + at + wide, Spot::Port(port)))
        .collect();
    if running {
        // The cell before the mark, and the one after, take the click too.
        let at = from + width(&right);
        spots.push((at.saturating_sub(2), at + 1, kill));
    }
    let row = joined([
        mark,
        glyphs(" ", Ink::faint(), 0),
        text,
        glyphs(&" ".repeat(gap), Ink::faint(), 0),
        right,
    ]);
    let head = Line::of(lifted(row, ui.hover == Some(fold)))
        .faded(alpha)
        .heads(fold);
    let open = ui.folds.amount(fold, ui.wall, 0.0);
    if open <= 0.0 {
        return (vec![head], spots);
    }
    let under: Vec<Line> = if command.tail.is_empty() {
        inset([glyphs("No output yet", Ink::faint(), 0)], inner).collect()
    } else {
        // A line that just came, comes in.
        let fresh = command.tail.iter().map(|line| {
            line.fresh
                .map_or(1.0, |fresh| progress(now, fresh, 0.26).max(0.15))
        });
        inset(
            command
                .tail
                .iter()
                .map(|line| glyphs(&line.text, Ink::muted(), 0)),
            inner,
        )
        .zip(std::iter::once(1.0).chain(fresh))
        .map(|(row, fresh)| row.faded(fresh))
        .collect()
    };
    let mut out = vec![head];
    out.extend(folded(under, open).into_iter().map(|row| row.faded(alpha)));
    (out, spots)
}

/// The most rows of a variable's value the panel shows.
const PREVIEW_ROWS: usize = 12;

/// A variable: its name, and its type on the right. Open, the start of its
/// value under it.
fn variable_lines(variable: &Variable, ui: &Ui, inner: usize, now: f64) -> Vec<Line> {
    let fold = Fold::Variable(variable.key);
    let alpha = presence(now, variable.shown, variable.left);
    // Each in full where both fit; else the type keeps half the row, or what the name leaves.
    let name = glyphs(&variable.name, Ink::of(INK, 0.86), 0);
    let half = inner.saturating_sub(2) / 2;
    let left = inner.saturating_sub(2 + width(&name));
    let kind = fit(glyphs(&variable.kind, Ink::faint(), 0), half.max(left));
    let room = inner.saturating_sub(width(&kind) + 2);
    let mut name = fit(name, room);
    if let Some(since) = variable.lit {
        for (at, glyph) in name.iter_mut().enumerate() {
            glyph.ink.rgb = lit(INK, now - since, at);
        }
    }
    let gap = inner.saturating_sub(width(&name) + width(&kind));
    let row = joined([name, glyphs(&" ".repeat(gap), Ink::faint(), 0), kind]);
    let head = Line::of(lifted(row, ui.hover == Some(fold)))
        .faded(alpha)
        .heads(fold);
    let open = ui.folds.amount(fold, ui.wall, 0.0);
    if open <= 0.0 {
        return vec![head];
    }
    let room = inner.saturating_sub(2);
    let quiet = |text: &str| wrap(glyphs(text, Ink::faint(), 0), room);
    let mut rows = match &variable.preview {
        Some(Ok(preview)) => {
            let text = glyphs(preview.text.trim_end(), Ink::muted(), 0);
            let mut rows = if preview.code {
                // Its rows are the kernel's; one too long for the panel goes on under itself.
                wrap_hard(text, room)
            } else {
                wrap(text, room)
            };
            let more = preview.truncated || rows.len() > PREVIEW_ROWS;
            rows.truncate(PREVIEW_ROWS);
            if more {
                rows.push(glyphs("…", Ink::faint(), 0));
            }
            rows
        }
        Some(Err(why)) if why == "busy" => quiet("Shown once the agent's code is done"),
        Some(Err(why)) => quiet(why),
        // Not asked yet: what the kernel's list says of it.
        None if variable.meta.is_empty() => quiet("…"),
        None => wrap(glyphs(&variable.meta, Ink::muted(), 0), room),
    };
    if rows.is_empty() {
        rows = quiet("Empty");
    }
    let mut under: Vec<Line> = inset(rows, inner).collect();
    under.push(Line::blank());
    let mut out = vec![head];
    out.extend(folded(under, open).into_iter().map(|row| row.faded(alpha)));
    out
}
