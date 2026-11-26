//! The menus that open over the message box, on the conversation's last
//! rows: the models the session can run on, with a field that searches
//! them, and the reasoning effort's levels on a track (the extension's
//! `vscode/webview/models.tsx` and `Effort.tsx`). This file holds what the
//! user set on them and how they are drawn; `frame` is the surface they
//! share with the `@` menu.

use crate::canvas::Canvas;
use crate::chat::Chat;
use crate::connect::Spot;
use crate::draft::Draft;
use crate::line::{cells, Line};
use crate::models::{maker, matching, specs, subscribed, Model};
use crate::motion::progress;
use crate::providers::Provider;
use crate::text::{fit, glyphs, width, Glyph, Ink};
use crate::theme::{ACCENT, INK, RAISED_2, WHITE};
use crate::view::{caret, effort_name, landed, with_caret, Area, Ui};

/// The most models the menu shows at once.
pub const ROWS: usize = 10;
/// The widest the models' menu runs, and the narrowest.
const WIDE_MAX: usize = 88;
const WIDE_MIN: usize = 40;
/// The cells before each column of what is said of a model.
const COLUMN: usize = 3;
/// How much of a model's name and its provider's the price leaves room for, at the least.
const NAME_LEAST: usize = 30;

/// The models' menu as the user left it.
#[derive(Clone, Default)]
pub struct Models {
    /// What the models are searched by.
    pub query: Draft,
    /// The one Enter picks, among those shown, and the first one in view.
    pub at: usize,
    pub top: usize,
}

/// The menu that is open over the message box.
#[derive(Clone)]
pub enum Pick {
    Models(Models),
    Effort,
}

impl Models {
    /// The menu opens on the model in use.
    #[must_use]
    pub fn on(chat: &Chat) -> Models {
        let mut models = Models::default();
        let shown = models.shown(chat);
        let current = |model: &&Model| Some(&model.value) == chat.chosen.as_ref();
        models.at = shown.iter().position(current).unwrap_or(0);
        models.reveal(shown.len());
        models
    }

    /// The models the search leaves, in the menu's order.
    #[must_use]
    pub fn shown<'a>(&self, chat: &'a Chat) -> Vec<&'a Model> {
        let providers = chat.providers.list.as_deref().unwrap_or_default();
        matching(
            &chat.models,
            self.query.text(),
            providers,
            chat.maker.as_deref(),
        )
    }

    /// The arrows: the model Enter picks moves `by` rows, round the `count` shown.
    pub fn step(&mut self, by: isize, count: usize) {
        if count > 0 {
            let at = self.at.min(count - 1).cast_signed() + by;
            self.at = at.rem_euclid(count.cast_signed()).cast_unsigned();
        }
        self.reveal(count);
    }

    /// The wheel: the rows in view move `by`, of the `count` shown.
    pub fn roll(&mut self, by: isize, count: usize) {
        let most = count.saturating_sub(ROWS);
        self.top = self.top.saturating_add_signed(by).min(most);
    }

    /// A new search starts at its first result.
    pub fn searched(&mut self) {
        self.at = 0;
        self.top = 0;
    }

    /// The model Enter picks is in view.
    fn reveal(&mut self, count: usize) {
        let most = count.saturating_sub(ROWS);
        self.top = (self.top)
            .min(self.at)
            .max((self.at + 1).saturating_sub(ROWS))
            .min(most);
    }
}

/// A row of a menu: its glyphs, whether it lies on the lighter surface of
/// the one Enter picks, and what a click on its cells `from..to` lands on
/// (counted from the menu's left edge).
pub struct Entry {
    pub glyphs: Vec<Glyph>,
    pub lit: bool,
    pub spots: Vec<(usize, usize, Spot)>,
}

impl Entry {
    #[must_use]
    pub fn of(glyphs: Vec<Glyph>) -> Entry {
        Entry {
            glyphs,
            lit: false,
            spots: Vec::new(),
        }
    }

    #[must_use]
    pub fn blank() -> Entry {
        Entry::of(Vec::new())
    }
}

/// How many rows a menu can show in the column `area` gives (its left, its
/// first row, the row it ends before, its width): a row of its surface
/// goes over and under them, and a row of the page over it.
#[must_use]
pub fn room(area: (u16, u16, u16, u16)) -> usize {
    let (_, top, bottom, _) = area;
    usize::from(bottom.saturating_sub(top)).saturating_sub(3)
}

/// A menu over the last rows of the column `area` gives, `wide` cells
/// wide: its surface, then its rows set in by two cells. Answers the first
/// row it covers, and where a click lands on what; `None` when it has no
/// row, or the screen no room for them.
pub fn frame(
    canvas: &mut Canvas,
    area: (u16, u16, u16, u16),
    wide: usize,
    entries: Vec<Entry>,
) -> Option<(u16, Vec<Area>)> {
    let (x, _, bottom, column) = area;
    if entries.is_empty() || entries.len() > room(area) {
        return None;
    }
    let wide = wide.min(usize::from(column));
    let first = bottom - cells(entries.len()) - 2;
    canvas.wipe(first - 1, x, x + column);
    for y in first..bottom {
        canvas.wipe(y, x, x + column);
        canvas.line(x, y, column, &Line::surface(RAISED_2, 0, wide));
    }
    let end = x + cells(wide);
    let mut spots = Vec::new();
    for (at, entry) in entries.into_iter().enumerate() {
        let y = first + 1 + cells(at);
        if entry.lit {
            canvas.fill(y, x, end, INK, 0.08);
        }
        canvas.put(x + 2, y, end, &entry.glyphs, 1.0);
        spots.extend(entry.spots.into_iter().map(|(from, to, spot)| Area {
            y,
            from: (x + cells(from)).min(end),
            to: (x + cells(to)).min(end),
            spot,
        }));
    }
    Some((first - 1, spots))
}

/// Draws the open menu over the last rows of the column `area` gives.
/// Answers the first row it covers, and where a click lands on what;
/// `None` when the screen has no room for it.
pub fn draw(
    canvas: &mut Canvas,
    pick: &Pick,
    chat: &Chat,
    ui: &Ui,
    area: (u16, u16, u16, u16),
) -> Option<(u16, Vec<Area>)> {
    let column = usize::from(area.3);
    match pick {
        Pick::Models(models) => {
            let (wide, entries) = models_entries(models, chat, ui, column, room(area));
            frame(canvas, area, wide, entries)
        }
        Pick::Effort => {
            let (wide, entries) = effort_entries(chat, ui, column);
            frame(canvas, area, wide, entries)
        }
    }
}

/// What is said of the models, each thing in a column of its own as
/// wide as its longest, set to the right: the context window, then what a
/// million tokens cost, in and out.
struct Said {
    /// Each column's width; those of the price are let go of where the menu is narrow.
    wide: [usize; 3],
    columns: usize,
}

impl Said {
    /// How many cells the columns take, with the cells before each.
    fn width(&self) -> usize {
        (self.wide.iter().take(self.columns))
            .filter(|wide| **wide > 0)
            .map(|wide| wide + COLUMN)
            .sum()
    }

    fn of(&self, model: &Model, priced: bool, ink: Ink) -> Vec<Glyph> {
        let said = specs(model, priced);
        let columns = (said.iter().zip(self.wide).take(self.columns)).filter(|(_, wide)| *wide > 0);
        // Each to the right of its column, the cells before it kept.
        let cells = columns.flat_map(|(said, wide)| {
            let said = said.as_deref().unwrap_or_default();
            let pad = (wide + COLUMN).saturating_sub(said.chars().count());
            [" ".repeat(pad), said.to_string()]
        });
        glyphs(&cells.collect::<String>(), ink, 0)
    }
}

/// A model's row, `inner` cells wide: its name, its provider's, and on the
/// right what is `said` of it and the check of the one in use.
fn model_row(
    model: &Model,
    providers: &[Provider],
    said: &Said,
    (lit, used): (bool, bool),
    inner: usize,
) -> Vec<Glyph> {
    let ink = if lit { Ink::text() } else { Ink::of(INK, 0.78) };
    let dim = if lit { Ink::muted() } else { Ink::faint() };
    let mut left = glyphs(&model.name, ink, 0);
    left.extend(glyphs("  ", dim, 0));
    left.extend(glyphs(&maker(providers, &model.provider), dim, 0));
    // The check keeps its cells on every row: what is said ends at the same one.
    let check = if used {
        glyphs("  ✓", Ink::accent(), 0)
    } else {
        glyphs("   ", dim, 0)
    };
    let priced = !subscribed(providers, &model.provider);
    let right = [said.of(model, priced, Ink::faint()), check].concat();
    let left = fit(left, inner.saturating_sub(width(&right)));
    let gap = inner.saturating_sub(width(&left) + width(&right));
    [left, glyphs(&" ".repeat(gap), dim, 0), right].concat()
}

/// The models' menu, in a column `column` cells wide with room for `room`
/// rows: its width, then the search, a row of its surface, and the models
/// the search leaves (why, while there is none).
fn models_entries(
    models: &Models,
    chat: &Chat,
    ui: &Ui,
    column: usize,
    room: usize,
) -> (usize, Vec<Entry>) {
    let providers = chat.providers.list.as_deref().unwrap_or_default();
    let priced = |model: &Model| !subscribed(providers, &model.provider);
    // As wide as the widest of all its models: it keeps its width, and
    // its columns theirs, while they are searched.
    let mut said = Said {
        wide: [0; 3],
        columns: 3,
    };
    let mut names = 0;
    for model in &chat.models {
        let name = format!("{}  {}", model.name, maker(providers, &model.provider));
        names = names.max(width(&glyphs(&name, Ink::text(), 0)));
        for (wide, said) in said.wide.iter_mut().zip(specs(model, priced(model))) {
            *wide = (*wide).max(said.map_or(0, |said| said.chars().count()));
        }
    }
    let most = column.min(WIDE_MAX);
    // Where the names would be cut short for it, the price is let go of.
    if NAME_LEAST.min(names) + said.width() + 3 + 4 > most {
        said.columns = 1;
    }
    let wide = (names + said.width() + 3 + 4).clamp(WIDE_MIN.min(most), most);
    let inner = wide.saturating_sub(4);

    let caret = caret(RAISED_2);
    let search = if models.query.is_empty() {
        let mut ask = glyphs("Search models", Ink::faint(), 0);
        if let Some(first) = ask.first_mut() {
            first.ink = caret;
        }
        ask
    } else {
        let laid = models.query.lay(inner, 1, Ink::text());
        let row = laid.rows.into_iter().next().unwrap_or_default();
        with_caret(row, laid.caret.1, caret)
    };
    let mut entries = vec![Entry::of(search), Entry::blank()];
    let shown = models.shown(chat);
    if shown.is_empty() {
        let none = if chat.models.is_empty() {
            "No model yet.".to_string()
        } else {
            format!("No model matches “{}”.", models.query.text().trim())
        };
        entries.push(Entry::of(fit(glyphs(&none, Ink::muted(), 0), inner)));
        return (wide, entries);
    }
    let rows = shown.len().min(ROWS).min(room.saturating_sub(2)).max(1);
    let from = models.top.min(shown.len() - rows);
    let at = models.at.min(shown.len() - 1);
    for (place, model) in shown.into_iter().enumerate().skip(from).take(rows) {
        let spot = Spot::Choice(place);
        let lit = place == at;
        let over = ui.over.as_ref() == Some(&spot);
        let used = Some(&model.value) == chat.chosen.as_ref();
        entries.push(Entry {
            glyphs: model_row(model, providers, &said, (lit || over, used), inner),
            lit,
            spots: vec![(0, wide, spot)],
        });
    }
    (wide, entries)
}

/// How long the effort's fill takes to reach a level.
const SLIDE: f64 = 0.34;

/// The effort's menu: its name, a row of its surface, then a track with a
/// stretch per level, filled in the accent up to the one in use (the
/// extension's slider), each level's name under its stretch. Opening, the
/// fill runs out from nothing; a level chosen, it runs to it, its end
/// bright on the way, and the level's name lands.
fn effort_entries(chat: &Chat, ui: &Ui, column: usize) -> (usize, Vec<Entry>) {
    let names: Vec<String> = chat
        .efforts
        .iter()
        .map(|level| effort_name(level))
        .collect();
    let wides: Vec<usize> = (names.iter())
        .map(|name| name.chars().count().max(3))
        .collect();
    let total = |gap: usize| wides.iter().sum::<usize>() + gap * wides.len().saturating_sub(1);
    let gap = if total(2) + 4 <= column { 2 } else { 1 };
    let used = (chat.effort.as_ref())
        .and_then(|current| chat.efforts.iter().position(|level| level == current));
    // Where each level's stretch starts on the track, in cells.
    let starts: Vec<usize> = (wides.iter())
        .scan(0, |start, wide| {
            let at = *start;
            *start += wide + gap;
            Some(at)
        })
        .collect();
    let end = |level: usize| {
        starts
            .get(level)
            .zip(wides.get(level))
            .map(|(start, wide)| start + wide)
    };
    let target = used.and_then(end).unwrap_or(0) as f64;
    let (moved, from) = match ui.slid {
        Some((at, from)) => (
            progress(ui.wall, at, SLIDE),
            from.and_then(end).unwrap_or(0) as f64,
        ),
        None => (1.0, target),
    };
    let fill = from + (target - from) * moved;
    let rush = (1.0 - moved) as f32;
    let top = chat.efforts.len().saturating_sub(1);
    let (mut track, mut labels, mut spots) = (Vec::new(), Vec::new(), Vec::new());
    let mut cell: u32 = 0;
    for (at, (name, wide)) in names.iter().zip(&wides).enumerate() {
        let spot = Spot::Level(at);
        let over = ui.over.as_ref() == Some(&spot);
        if at > 0 {
            // The fill runs on between the levels it covers.
            for _ in 0..gap {
                let lit = f64::from(cell) + 0.5 < fill;
                let ink = Ink::of(ACCENT, 1.0);
                track.extend(glyphs(if lit { "━" } else { " " }, ink, 0));
                cell += 1;
            }
            labels.extend(glyphs(&" ".repeat(gap), Ink::faint(), 0));
        }
        for _ in 0..*wide {
            let centre = f64::from(cell) + 0.5;
            let ink = if centre < fill {
                // Its end comes near white while it runs.
                let near = (1.0 - (fill - centre) / 3.0).max(0.0) as f32 * rush;
                Ink::of(ACCENT.mix(WHITE, 0.75 * near), 1.0)
            } else if over {
                Ink::of(ACCENT, 0.4)
            } else {
                Ink::of(INK, 0.16)
            };
            track.extend(glyphs("━", ink, 0));
            cell += 1;
        }
        let ink = match (used == Some(at), over) {
            // The top level stays in the accent, as the extension's does.
            (true, _) if at == top => Ink::accent(),
            (true, _) => Ink::text(),
            (false, true) => Ink::of(INK, 0.78),
            (false, false) => Ink::faint(),
        };
        let mut label = glyphs(&format!("{name:<wide$}"), ink, 0);
        if let Some((when, _)) = ui.slid.filter(|_| used == Some(at)) {
            let letters = name.chars().count().min(label.len());
            landed(&mut label[..letters], ui.wall - when);
        }
        labels.extend(label);
        // A click between two levels lands on the nearer one; the rows start two cells in.
        let start = starts[at];
        let (left, right) = (2 + start, 2 + start + wide);
        spots.push((left.saturating_sub(gap / 2), right + gap - gap / 2, spot));
    }
    let wide = (total(gap) + 4).max(20);
    let entries = vec![
        Entry::of(glyphs("Reasoning effort", Ink::muted(), 0)),
        Entry::blank(),
        Entry {
            glyphs: track,
            lit: false,
            spots: spots.clone(),
        },
        Entry {
            glyphs: labels,
            lit: false,
            spots,
        },
    ];
    (wide, entries)
}
