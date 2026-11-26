//! How the settings' screen is drawn: its tabs, what a restart would
//! apply, then the tab's rows, the chosen one on a lighter surface, a form
//! open under its row.

use serde_json::Value;

use super::general::{self, Control, Field};
use super::{
    connected, groups, listed, servers, shown_groups, skills, Item, Piece, Prefs, Sheet, Tab,
    LIST_ROWS, SKILL_GROUPS, TABS,
};
use crate::canvas::Canvas;
use crate::chat::Chat;
use crate::connect::{self, field, pill, side_by_side, words, Look, Row, Spot};
use crate::draft::Draft;
use crate::line::Line;
use crate::motion::spinner;
use crate::settings::{Server, Skill, CREATE, FOLDER, INSTRUCTIONS, SERVERS};
use crate::text::{fit, glyphs, width, Glyph, Ink};
use crate::theme::{ACCENT, INK, RAISED, RAISED_2};
use crate::view::{caret, with_caret, Screen, Ui};

/// How far a form is set in under its row.
const IN: usize = 4;
/// How wide the field of a number is, in its row.
const NUMBER: usize = 9;
/// The widest a skill's name, or a server's, is given before what is said of it.
const NAME_MAX: usize = 28;
/// The longest name of a field in a server's form (`Token variable`).
const SERVER_LABEL: usize = 14;

/// A switch: a knob on a track, to the right and in the accent when on.
/// Whole cells: its corners are square in every terminal.
fn switch(on: bool, dim: bool) -> Vec<Glyph> {
    let fade = if dim { 0.45 } else { 1.0 };
    let part = |rgb, alpha: f32| glyphs("  ", Ink::text().on(rgb, alpha * fade), 0);
    if on {
        [part(ACCENT, 0.3), part(ACCENT, 0.95)].concat()
    } else {
        [part(INK, 0.5), part(INK, 0.13)].concat()
    }
}

/// `row` on a surface `wide` cells wide, a cell of it before the first glyph.
fn boxed(row: Vec<Glyph>, wide: usize, surface: crate::theme::Rgb) -> Vec<Glyph> {
    let pad = |cells: usize| glyphs(&" ".repeat(cells), Ink::text().on(surface, 1.0), 0);
    let mut row = fit(row, wide.saturating_sub(1));
    for glyph in &mut row {
        if glyph.ink.bg.is_none() {
            glyph.ink = glyph.ink.on(surface, 1.0);
        }
    }
    let rest = wide.saturating_sub(1 + width(&row));
    [pad(1), row, pad(rest)].concat()
}

/// What a field shows on its row: what is written, with the caret while
/// what is typed goes to it; what it asks for, while nothing is; each
/// character of a secret as a dot.
fn written(
    draft: &Draft,
    asks: &str,
    secret: bool,
    live: bool,
    wide: usize,
    surface: crate::theme::Rgb,
) -> Vec<Glyph> {
    let caret = caret(surface);
    if draft.is_empty() {
        let mut ask = fit(glyphs(asks, Ink::faint(), 0), wide.saturating_sub(1));
        if live {
            match ask.first_mut() {
                Some(first) => first.ink = caret,
                None => ask = glyphs(" ", caret, 0),
            }
        }
        return ask;
    }
    let laid = draft.lay(wide, 1, Ink::text());
    let mut row = laid.rows.into_iter().next().unwrap_or_default();
    if secret {
        row = (row.iter())
            .flat_map(|glyph| glyphs(&"•".repeat(usize::from(glyph.width)), glyph.ink, glyph.src))
            .collect();
    }
    if live {
        with_caret(row, laid.caret.1, caret)
    } else {
        row
    }
}

fn blank(row: &Row) -> bool {
    row.line.glyphs.is_empty() && row.line.fill.is_none()
}

struct Page<'a> {
    rows: Vec<Row>,
    /// The chosen row's rows, with what it opened on.
    seen: Option<(usize, usize)>,
    chat: &'a Chat,
    ui: &'a Ui,
    prefs: &'a Prefs,
    inner: usize,
    now: f64,
}

impl Page<'_> {
    fn over(&self, spot: &Spot) -> bool {
        self.ui.over.as_ref() == Some(spot)
    }

    /// The row is the chosen one, or the pointer is on it.
    fn lit(&self, item: &Item) -> bool {
        let pointed = match &self.ui.over {
            Some(Spot::Pref(over) | Spot::Flip(over) | Spot::Drop(over)) => over == item,
            _ => false,
        };
        pointed || self.prefs.chosen.as_ref() == Some(item)
    }

    fn sheet(&self, item: &Item) -> Option<&Sheet> {
        self.prefs
            .sheet
            .as_ref()
            .filter(|sheet| sheet.item == *item)
    }

    /// A row of the page before what follows, unless one is there.
    fn gap(&mut self) {
        if self.rows.last().is_some_and(|row| !blank(row)) {
            self.rows.push(Row::blank());
        }
    }

    /// The name of a group of rows, a row of the page under it.
    fn label(&mut self, text: &str) {
        self.gap();
        self.rows
            .push(Row::of(Line::of(glyphs(text, Ink::faint(), 0))));
        self.rows.push(Row::blank());
    }

    /// A text of the page, a row of the page under it.
    fn say(&mut self, text: &str, ink: Ink) {
        self.rows.extend(words(text, ink, 0, self.inner));
        self.rows.push(Row::blank());
    }

    /// Rows under a row of the list: a row of the page before and after them.
    fn below(&mut self, rows: Vec<Row>) {
        if rows.is_empty() {
            return;
        }
        self.gap();
        self.rows.extend(rows);
        self.rows.push(Row::blank());
    }

    /// Why a change was refused, under the row it was made from.
    fn refused(&mut self, key: &str) {
        let chat = self.chat;
        if let Some(why) = chat.settings.errors.get(key) {
            let rows = words(why, Ink::danger(), IN, self.inner.saturating_sub(IN + 2)).collect();
            self.below(rows);
        }
    }

    fn wait(&mut self, what: &str) {
        let mut wait = glyphs(&spinner(self.now).to_string(), Ink::accent(), 0);
        wait.extend(glyphs(&format!(" {what}"), Ink::muted(), 0));
        self.rows.push(Row::of(Line::of(wait)));
    }

    /// A row of the list, a row of the page before it: `left` from its
    /// second cell, `right` at its end;
    /// on the lighter surface when it is chosen or under the pointer. A
    /// click on it acts on `item`; on what stands at its end, on `end`,
    /// when that is another thing. Answers where its rows start.
    fn item(
        &mut self,
        item: &Item,
        left: Vec<Glyph>,
        right: Vec<Glyph>,
        end: Option<Spot>,
    ) -> usize {
        // A row of the page between two rows of the list: two switches one
        // over the other would make one block.
        self.gap();
        let inner = self.inner;
        let taken = if right.is_empty() {
            0
        } else {
            width(&right) + 2
        };
        let mut text = fit(left, inner.saturating_sub(4 + taken));
        let at = inner.saturating_sub(2 + width(&right));
        text.extend(glyphs(
            &" ".repeat(at.saturating_sub(2 + width(&text))),
            Ink::faint(),
            0,
        ));
        text.extend(right);
        let mut line = Line::at(2, text);
        if self.lit(item) {
            line = line.on(RAISED, 0, inner);
        }
        let body = Spot::Pref(item.clone());
        let row = match end {
            Some(end) => {
                Row::of(line)
                    .on(0, at.saturating_sub(1), body)
                    .on(at.saturating_sub(1), inner, end)
            }
            None => Row::of(line).on(0, inner, body),
        };
        self.rows.push(row);
        self.rows.len() - 1
    }

    /// The rows from `from` are the chosen row's, when `item` is it.
    fn close(&mut self, item: &Item, from: usize) {
        if self.prefs.chosen.as_ref() == Some(item) {
            self.seen = Some((from, self.rows.len()));
        }
    }

    /// A name as a row gives it: full when the row is lit.
    fn name(&self, text: &str, item: &Item) -> Vec<Glyph> {
        let ink = if self.lit(item) {
            Ink::text()
        } else {
            Ink::of(INK, 0.78)
        };
        glyphs(text, ink, 0)
    }

    /// The tabs, the one in view on a lighter surface.
    fn tabs(&mut self) {
        let mut row = Row::of(Line::of(Vec::new()));
        let mut at = 0;
        for (tab, name) in TABS {
            let spot = Spot::Tab(tab);
            let ink = if tab == self.prefs.tab {
                Ink::text().bold().on(INK, 0.12)
            } else if self.over(&spot) {
                Ink::text()
            } else {
                Ink::muted()
            };
            let word = glyphs(&format!(" {name} "), ink, 0);
            let wide = width(&word);
            row.line.glyphs.extend(word);
            row.line.glyphs.extend(glyphs(" ", Ink::faint(), 0));
            row = row.on(at, at + wide, spot);
            at += wide + 1;
        }
        self.rows.push(row);
        self.rows.push(Row::blank());
    }

    /// What a restart would apply, and what restarts the agent.
    fn notice(&mut self) {
        if self.chat.settings.pending().is_empty() {
            return;
        }
        let item = Item::Restart;
        let said = glyphs(
            "Some changes apply when the agent restarts.",
            Ink::muted(),
            0,
        );
        let button = pill("Restart agent", Look::Main, self.lit(&item), true);
        let from = self.item(&item, said, button, None);
        self.close(&item, from);
        self.rows.push(Row::blank());
    }

    /// The form open under `item`'s row, if it is.
    fn form(&mut self, item: &Item) {
        let prefs = self.prefs;
        if let Some(sheet) = prefs.sheet.as_ref().filter(|sheet| sheet.item == *item) {
            let rows = self.sheet_rows(sheet);
            self.below(rows);
        }
    }

    /// A form's rows, set in under its row: its parts top to bottom, a
    /// row of the page between them.
    fn sheet_rows(&self, sheet: &Sheet) -> Vec<Row> {
        let pieces = sheet.pieces(self.chat);
        let wide = self.inner.saturating_sub(IN + 2);
        let labels = pieces.iter().filter_map(|piece| match piece {
            Piece::Field { label, .. } | Piece::Choice { label, .. } | Piece::List { label } => {
                Some(label.chars().count())
            }
            _ => None,
        });
        // Where the fields start: after the longest of their names; in a
        // server's form, of all the names it can show, so that a choice
        // moves none of its fields.
        let server = matches!(sheet.item, Item::Server(_) | Item::AddServer) && !sheet.asking;
        let lead = match labels.max().unwrap_or(0) {
            0 => 0,
            longest if server => (longest.max(SERVER_LABEL) + 3).min(wide / 2),
            longest => (longest + 3).min(wide / 2),
        };
        let mut rows: Vec<Row> = Vec::new();
        let mut stop = 0;
        for piece in pieces {
            if !rows.is_empty() {
                rows.push(Row::blank());
            }
            match piece {
                Piece::Say(text, ink) => rows.extend(words(&text, ink, IN, wide)),
                Piece::Field {
                    at,
                    label,
                    asks,
                    secret,
                    rows: high,
                } => {
                    let live = sheet.focus == stop;
                    let draft = sheet.fields.get(at).cloned().unwrap_or_default();
                    rows.extend(self.field_rows(
                        &draft,
                        (label, &asks),
                        (secret, live, high),
                        lead,
                        stop,
                    ));
                    stop += 1;
                }
                Piece::Choice { at, label, options } => {
                    let on = sheet.choices.get(at).copied().unwrap_or(0);
                    rows.push(self.choice_row(label, options, on, sheet.focus == stop, lead, stop));
                    stop += 1;
                }
                Piece::List { label } => {
                    rows.extend(self.list_rows(sheet, label, sheet.focus == stop, lead, stop));
                    stop += 1;
                }
                Piece::Buttons(buttons) => {
                    let made = (buttons.into_iter().enumerate())
                        .map(|(place, (_, word, look, ready))| {
                            let spot = Spot::Stop(stop + place);
                            let lit = ready && (sheet.focus == stop + place || self.over(&spot));
                            (pill(&word, look, lit, ready), spot)
                        })
                        .collect::<Vec<_>>();
                    stop += made.len();
                    rows.push(side_by_side(made, IN));
                }
            }
        }
        rows
    }

    /// A field of a form: its name, then its surface with what is written,
    /// `high` rows of it for a text of several lines.
    fn field_rows(
        &self,
        draft: &Draft,
        (label, asks): (&str, &str),
        (secret, live, high): (bool, bool, usize),
        lead: usize,
        stop: usize,
    ) -> Vec<Row> {
        let (from, to) = (IN + lead, self.inner.saturating_sub(2));
        let room = to.saturating_sub(from + 2);
        let named = |first: bool| {
            let ink = if live { Ink::text() } else { Ink::muted() };
            let name = if first { label } else { "" };
            let mut name = fit(glyphs(name, ink, 0), lead.saturating_sub(1));
            let pad = (lead + 1).saturating_sub(width(&name));
            name.extend(glyphs(&" ".repeat(pad), Ink::faint(), 0));
            name
        };
        let row_of = |first: bool, text: Vec<Glyph>| {
            let glyphs = [named(first), text].concat();
            Row::of(Line::at(IN, glyphs).on(RAISED, from, to)).on(from, to, Spot::Stop(stop))
        };
        if high <= 1 {
            return vec![row_of(
                true,
                written(draft, asks, secret, live, room, RAISED),
            )];
        }
        self.prefs.wide.set(room);
        let mut texts: Vec<Vec<Glyph>> = if draft.is_empty() {
            vec![written(draft, asks, false, live, room, RAISED)]
        } else {
            let laid = draft.lay(room, high, Ink::text());
            (laid.rows.into_iter().enumerate())
                .map(|(at, row)| {
                    if live && at == laid.caret.0 {
                        with_caret(row, laid.caret.1, caret(RAISED))
                    } else {
                        row
                    }
                })
                .collect()
        };
        texts.resize(high, Vec::new());
        (texts.into_iter().enumerate())
            .map(|(at, text)| row_of(at == 0, text))
            .collect()
    }

    /// A choice of a few, side by side: the one it is on, on a surface.
    fn choice_row(
        &self,
        label: &str,
        options: &[&str],
        on: usize,
        live: bool,
        lead: usize,
        stop: usize,
    ) -> Row {
        let ink = if live { Ink::text() } else { Ink::muted() };
        let mut name = fit(glyphs(label, ink, 0), lead.saturating_sub(1));
        name.extend(glyphs(
            &" ".repeat(lead.saturating_sub(width(&name))),
            Ink::faint(),
            0,
        ));
        let mut row = Row::of(Line::at(IN, name));
        let mut at = IN + lead;
        for (place, option) in options.iter().enumerate() {
            let spot = Spot::Opt(stop, place);
            let ink = match (place == on, live, self.over(&spot)) {
                (true, true, _) => Ink::of(ACCENT, 0.95).on(ACCENT, 0.2),
                (true, false, _) => Ink::text().on(INK, 0.13),
                (false, _, true) => Ink::text().on(INK, 0.05),
                (false, _, false) => Ink::muted().on(INK, 0.05),
            };
            let word = glyphs(&format!(" {option} "), ink, 0);
            let wide = width(&word);
            row.line.glyphs.extend(word);
            row.line.glyphs.extend(glyphs(" ", Ink::faint(), 0));
            row = row.on(at, at + wide, spot);
            at += wide + 1;
        }
        row
    }

    /// A form's list of models: what searches them, then those the search
    /// leaves, a check on the ones picked, the one Enter acts on lit.
    fn list_rows(
        &self,
        sheet: &Sheet,
        label: &str,
        live: bool,
        lead: usize,
        stop: usize,
    ) -> Vec<Row> {
        let (from, to) = (IN + lead, self.inner.saturating_sub(2));
        let room = to.saturating_sub(from + 2);
        let ink = if live { Ink::text() } else { Ink::muted() };
        let mut name = fit(glyphs(label, ink, 0), lead.saturating_sub(1));
        name.extend(glyphs(
            &" ".repeat((lead + 1).saturating_sub(width(&name))),
            Ink::faint(),
            0,
        ));
        let search = written(
            &sheet.list.query,
            "Search models",
            false,
            live,
            room,
            RAISED,
        );
        let head = Line::at(IN, [name, search].concat()).on(RAISED, from, to);
        let mut rows = vec![Row::of(head).on(from, to, Spot::Stop(stop)), Row::blank()];
        let entries = sheet.entries(self.chat);
        if entries.is_empty() {
            let none = if !sheet.list.query.text().trim().is_empty() {
                format!("No model matches “{}”.", sheet.list.query.text().trim())
            } else if matches!(sheet.item, Item::Setting(_)) {
                "No model reads images.".to_string()
            } else {
                "No model of a connected provider.".to_string()
            };
            rows.extend(words(&none, Ink::muted(), from + 1, room));
            return rows;
        }
        let first = sheet.list.top.min(entries.len().saturating_sub(LIST_ROWS));
        let at = sheet.list.at.min(entries.len() - 1);
        let count = entries.len();
        for (place, entry) in entries.into_iter().enumerate().skip(first).take(LIST_ROWS) {
            let spot = Spot::Entry(place);
            let lit = (live && place == at) || self.over(&spot);
            let mark = if entry.marked { "✓ " } else { "  " };
            let name = if lit { Ink::text() } else { Ink::of(INK, 0.78) };
            let text = [
                glyphs(mark, Ink::accent(), 0),
                glyphs(&entry.name, name, 0),
                glyphs("  ", Ink::faint(), 0),
                glyphs(&entry.maker, Ink::faint(), 0),
            ]
            .concat();
            let mut entry_row = Line::at(from + 1, fit(text, room));
            if lit {
                entry_row = entry_row.on(RAISED, from, to);
            }
            rows.push(Row::of(entry_row).on(from, to, spot));
        }
        let under = count.saturating_sub(first + LIST_ROWS);
        if under > 0 {
            let more = glyphs(&format!("… {under} more"), Ink::faint(), 0);
            rows.push(Row::of(Line::at(from + 3, more)));
        }
        rows
    }

    /// A setting of the General tab: its name, what it does beside it
    /// where that fits (else under it), and what sets it at the row's end.
    fn setting(&mut self, field: &'static Field, dim: bool) {
        let (chat, prefs) = (self.chat, self.prefs);
        let settings = &chat.settings;
        let item = Item::Setting(field.key);
        let open = prefs.sheet.as_ref().filter(|sheet| sheet.item == item);
        let value = settings.value(field.key);
        let fade = |ink: Ink| if dim { ink.alpha(ink.alpha * 0.5) } else { ink };
        let right = match field.control {
            Control::Switch => switch(value == &Value::Bool(true), dim),
            Control::Number(unit, _) => {
                let unit_label = glyphs(&format!(" {}", unit.label()), fade(Ink::faint()), 0);
                let unit_label = if unit.label().is_empty() {
                    Vec::new()
                } else {
                    unit_label
                };
                if let Some(sheet) = open {
                    let draft = sheet.fields.first().cloned().unwrap_or_default();
                    let default = (settings.fallback(field.key).as_u64())
                        .map_or_else(String::new, |default| general::shown(default, unit));
                    let text = written(&draft, &default, false, true, NUMBER - 1, RAISED_2);
                    [boxed(text, NUMBER, RAISED_2), unit_label].concat()
                } else {
                    let set = value != settings.fallback(field.key);
                    let shown = (value.as_u64())
                        .map_or_else(String::new, |value| general::shown(value, unit));
                    let ink = if set { Ink::text() } else { Ink::muted() };
                    [glyphs(&shown, fade(ink), 0), unit_label].concat()
                }
            }
            Control::Model { none, .. } => {
                let named = value.as_str().map(|key| {
                    let model = (chat.models.iter()).find(|model| groups::key(model) == key);
                    model.map_or_else(|| crate::text::clean(key), |model| model.name.clone())
                });
                let ink = if named.is_some() {
                    Ink::text()
                } else {
                    Ink::muted()
                };
                glyphs(named.as_deref().unwrap_or(none), fade(ink), 0)
            }
        };
        let mut left = self.name(field.label, &item);
        if dim {
            for glyph in &mut left {
                glyph.ink.alpha *= 0.5;
            }
        }
        // What it does, beside its name where the row has the room.
        let room = self
            .inner
            .saturating_sub(4 + width(&left) + 3 + width(&right) + 3);
        let beside = !field.help.is_empty() && field.help.chars().count() <= room;
        if beside {
            left.extend(glyphs("   ", Ink::faint(), 0));
            left.extend(glyphs(field.help, fade(Ink::faint()), 0));
        }
        let from = self.item(&item, left, right, None);
        if !beside && !field.help.is_empty() {
            let rows = words(
                field.help,
                fade(Ink::faint()),
                IN,
                self.inner.saturating_sub(IN + 2),
            );
            self.below(rows.collect());
        }
        if let Some(sheet) = open {
            let rows = match field.control {
                Control::Model { .. } => self.list_rows(sheet, "", true, 0, 0),
                _ => (sheet.problem.iter())
                    .flat_map(|why| {
                        words(why, Ink::danger(), IN, self.inner.saturating_sub(IN + 2))
                    })
                    .collect(),
            };
            self.below(rows);
        }
        self.refused(field.key);
        self.close(&item, from);
    }

    fn general(&mut self) {
        if self.chat.settings.view.is_none() {
            return self.wait("Reading the settings");
        }
        for group in &general::GROUPS {
            self.label(group.label);
            if !group.help.is_empty() {
                self.say(group.help, Ink::muted());
            }
            let gated = group.gate.is_some_and(|gate| !self.chat.settings.on(gate));
            for field in group.fields {
                // The gate itself stays live; the rest dims while it is off.
                self.setting(field, gated && Some(field.key) != group.gate);
            }
        }
    }

    /// One of the user's servers: its name, how it stands, where it is,
    /// and the switch of one the file holds.
    fn server(&mut self, id: &str, server: Option<&Server>, names: usize) {
        let item = Item::Server(id.to_string());
        let entry = super::config(self.chat).get(id);
        let catalog = server.is_some_and(|server| !server.user);
        let name = server
            .filter(|_| catalog)
            .map_or(id, |server| server.label.as_str());
        let (status, accent) = servers::status(server);
        let mut left = self.name(name, &item);
        // How the servers stand starts at the same cell, after the longest of their names.
        let gap = (names + 3).saturating_sub(width(&left)).max(3);
        left.extend(glyphs(&" ".repeat(gap), Ink::faint(), 0));
        let ink = if accent {
            Ink::of(ACCENT, 0.9)
        } else {
            Ink::muted()
        };
        left.extend(glyphs(&status, ink, 0));
        if let Some(target) = entry.and_then(servers::target) {
            left.extend(glyphs(&format!(" · {target}"), Ink::faint(), 0));
        }
        let (right, end) = match entry {
            Some(entry) => {
                let on = entry.get("enabled") != Some(&Value::Bool(false));
                (switch(on, false), Some(Spot::Flip(item.clone())))
            }
            None => (Vec::new(), None),
        };
        let from = self.item(&item, left, right, end);
        self.form(&item);
        self.close(&item, from);
    }

    fn mcp(&mut self) {
        let (chat, prefs) = (self.chat, self.prefs);
        if chat.settings.servers.is_none() {
            return self.wait("Reading the MCP servers");
        }
        let help = "The agent calls MCP servers' tools from its Python kernel. Changes apply when it restarts.";
        self.say(help, Ink::muted());
        let listed = listed(chat, prefs.query.text());
        self.label("Your servers");
        let names = (listed.yours.iter())
            .chain(listed.services.iter().map(|service| &service.label))
            .map(|name| name.chars().count())
            .max()
            .unwrap_or(0)
            .min(NAME_MAX);
        for id in &listed.yours {
            self.server(id, chat.settings.server(id), names);
        }
        for service in &listed.services {
            self.server(&service.id, Some(service), names);
        }
        let add = Item::AddServer;
        let left = self.name("+ Add a server", &add);
        let from = self.item(&add, left, Vec::new(), None);
        self.form(&add);
        self.close(&add, from);
        if self.prefs.sheet.is_none() {
            self.refused(SERVERS);
        }
        self.label("Marketplace");
        let Some(offered) = listed.offered else {
            let none = "The Marketplace could not be read. It is tried again within the hour.";
            return self.say(none, Ink::muted());
        };
        let asks = format!("Search {offered} services");
        let live = prefs.sheet.is_none();
        self.rows
            .extend(field(&prefs.query, &asks, false, live, 0, self.inner));
        self.rows.push(Row::blank());
        if listed.market.is_empty() {
            let typed = !prefs.query.text().trim().is_empty();
            let none = if typed {
                "No service matches."
            } else {
                "Every service is added."
            };
            return self.say(none, Ink::muted());
        }
        for service in listed.market {
            let item = Item::Market(service.id.clone());
            let busy = chat.settings.busy(&format!("mcp:{}", service.id));
            // Nothing to add it by: what it needs instead.
            let blocked = !service.sign_in && service.token.is_none();
            let mut left = self.name(&service.label, &item);
            if blocked {
                let why = service.hint.as_deref().unwrap_or("Cannot be added");
                left.extend(glyphs(&format!("   {why}"), Ink::muted(), 0));
            }
            let open = self.sheet(&item).is_some();
            let word = if busy { "Adding" } else { "Add" };
            let ink = match (blocked || busy, self.lit(&item)) {
                (true, _) => Ink::faint().alpha(0.2),
                (false, true) => Ink::accent(),
                (false, false) => Ink::faint(),
            };
            let right = if open {
                Vec::new()
            } else {
                glyphs(word, ink, 0)
            };
            let from = self.item(&item, left, right, None);
            self.form(&item);
            self.close(&item, from);
        }
    }

    /// A skill: its name, what it is for, and its switch. One the running
    /// agent does not have as the switch says is marked: a restart applies it.
    fn skill(&mut self, skill: &Skill, names: usize) {
        let chat = self.chat;
        let settings = &chat.settings;
        let item = Item::Skill(skill.path.clone());
        let key = format!("skill:{}", skill.path);
        let on =
            (settings.drafts.get(&key)).map_or(skill.enabled, |draft| draft == &Value::Bool(true));
        let builtin = skill.source == "builtin";
        let mut left = if builtin {
            glyphs(&skill.name, Ink::of(INK, 0.78), 0)
        } else {
            self.name(&skill.name, &item)
        };
        // What the skills are for starts at the same cell, after the longest of their names.
        let gap = (names + 3).saturating_sub(width(&left)).max(3);
        left.extend(glyphs(&" ".repeat(gap), Ink::faint(), 0));
        left.extend(glyphs(&skill.description, Ink::muted(), 0));
        if builtin {
            // No switch: it is the agent's own, set in its file only.
            let right = if on {
                Vec::new()
            } else {
                glyphs("Off in settings.json", Ink::faint(), 0)
            };
            let room = self.inner.saturating_sub(
                4 + if right.is_empty() {
                    0
                } else {
                    width(&right) + 2
                },
            );
            let mut text = fit(left, room);
            let at = self.inner.saturating_sub(2 + width(&right));
            text.extend(glyphs(
                &" ".repeat(at.saturating_sub(2 + width(&text))),
                Ink::faint(),
                0,
            ));
            text.extend(right);
            self.gap();
            self.rows.push(Row::of(Line::at(2, text)));
            return;
        }
        // One the running agent does not have as its switch says: a restart applies it.
        let mut right = Vec::new();
        if on != skill.active {
            right.extend(glyphs(" restart ", Ink::muted().on(INK, 0.1), 0));
            right.extend(glyphs("  ", Ink::faint(), 0));
        }
        right.extend(switch(on, false));
        let from = self.item(&item, left, right, None);
        self.refused(&key);
        self.close(&item, from);
    }

    fn skills(&mut self) {
        let (chat, prefs) = (self.chat, self.prefs);
        let Some(all) = &chat.settings.skills else {
            return self.wait("Reading the skills");
        };
        let help = "Instructions the agent follows when a task calls for them: it sees each skill's name and description, and reads the rest when needed. Changes apply when it restarts.";
        self.say(help, Ink::muted());
        let new = Item::NewSkill;
        let left = self.name("+ New skill", &new);
        let from = self.item(&new, left, Vec::new(), None);
        self.form(&new);
        if let Some(made) = prefs.made.as_ref().filter(|_| prefs.sheet.is_none()) {
            // Its file whole, on rows of its own: it is where to write next.
            let room = self.inner.saturating_sub(IN + 2);
            let said = "Created. Write its instructions in its file:";
            let mut rows: Vec<Row> = words(said, Ink::muted(), IN, room).collect();
            rows.push(Row::blank());
            let path = glyphs(&crate::text::clean(made), Ink::of(INK, 0.78), 0);
            rows.extend(
                crate::text::wrap_hard(path, room.max(8))
                    .into_iter()
                    .map(|row| Row::of(Line::at(IN, row))),
            );
            self.below(rows);
        }
        if self.prefs.sheet.is_none() {
            self.refused(CREATE);
        }
        self.close(&new, from);
        self.gap();
        let query = prefs.query.text();
        if prefs.searches(chat) {
            let live = prefs.sheet.is_none();
            self.rows.extend(field(
                &prefs.query,
                "Search skills",
                false,
                live,
                0,
                self.inner,
            ));
            self.rows.push(Row::blank());
        }
        let mut found = 0;
        let names = (SKILL_GROUPS.iter())
            .flat_map(|label| skills(chat, query, label))
            .map(|skill| skill.name.chars().count())
            .max()
            .unwrap_or(0)
            .min(NAME_MAX);
        for label in SKILL_GROUPS {
            let rows = skills(chat, query, label);
            if rows.is_empty() {
                continue;
            }
            found += rows.len();
            self.label(label);
            for skill in rows {
                self.skill(skill, names);
            }
        }
        if found == 0 && !all.list.is_empty() {
            self.gap();
            self.say(
                &format!("No skill matches “{}”.", query.trim()),
                Ink::muted(),
            );
        }
        if !all.problems.is_empty() {
            self.label("Not loaded");
            for (message, path) in &all.problems {
                let mut text = glyphs(message, Ink::muted(), 0);
                if let Some(path) = path {
                    text.extend(glyphs(&format!("   {path}"), Ink::faint(), 0));
                }
                self.rows.extend(
                    crate::text::wrap(text, self.inner.saturating_sub(4))
                        .into_iter()
                        .map(|row| Row::of(Line::at(2, row))),
                );
            }
        }
        self.folders(&all.folders);
    }

    /// The folders skills are read from beside the usual ones, and what adds one.
    fn folders(&mut self, folders: &[String]) {
        self.label("Folders");
        let help = "The agent reads ~/.unharnessed/skills, ~/.agents/skills, and the workspace's .unharnessed/skills and .agents/skills. It also reads the folders added here.";
        self.say(help, Ink::muted());
        for folder in folders {
            let item = Item::Folder(folder.clone());
            let left = self.name(folder, &item);
            let drop = Spot::Drop(item.clone());
            let ink = if self.over(&drop) {
                Ink::text()
            } else {
                Ink::faint()
            };
            let right = if self.lit(&item) {
                glyphs("Remove", ink, 0)
            } else {
                Vec::new()
            };
            let from = self.item(&item, left, right, Some(drop));
            self.form(&item);
            self.close(&item, from);
        }
        let add = Item::AddFolder;
        let left = self.name("+ Add a folder", &add);
        let from = self.item(&add, left, Vec::new(), None);
        self.form(&add);
        self.close(&add, from);
        if self.prefs.sheet.is_none() {
            self.refused(FOLDER);
        }
    }

    fn instructions(&mut self) {
        let chat = self.chat;
        if chat.settings.view.is_none() {
            return self.wait("Reading the settings");
        }
        if chat.models.is_empty() {
            return self.say("The groups show once the session is open.", Ink::muted());
        }
        let help = "A group's text joins the agent's instructions whenever it runs on one of the group's models. Changes apply from the next conversation.";
        self.say(help, Ink::muted());
        for (at, models, text) in shown_groups(chat) {
            let item = Item::Group(at);
            let names: Vec<&str> = models.iter().map(|model| model.name.as_str()).collect();
            let mut left = self.name(&names.join(", "), &item);
            // The start of its text, on one line.
            let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
            left.extend(glyphs("   ", Ink::faint(), 0));
            left.extend(glyphs(&crate::text::clean(&line), Ink::muted(), 0));
            let from = self.item(&item, left, Vec::new(), None);
            self.form(&item);
            self.close(&item, from);
        }
        let new = Item::NewGroup;
        let left = self.name("+ New group", &new);
        let from = self.item(&new, left, Vec::new(), None);
        self.form(&new);
        self.close(&new, from);
        if self.prefs.sheet.is_none() {
            self.refused(INSTRUCTIONS);
        }
        if connected(chat).is_empty() {
            self.gap();
            self.say(
                "No provider is connected: a group names the models of connected providers.",
                Ink::muted(),
            );
        }
    }
}

/// Draws the settings' screen under the wordmark: its title, a click on
/// which leaves it, its tabs, then the rows of the one in view.
pub fn draw(canvas: &mut Canvas, chat: &Chat, ui: &Ui, prefs: &Prefs, now: f64) -> Screen {
    let mut page = Page {
        rows: Vec::new(),
        seen: None,
        chat,
        ui,
        prefs,
        inner: usize::from(connect::column(canvas)),
        now,
    };
    page.tabs();
    page.notice();
    match prefs.tab {
        Tab::General => page.general(),
        Tab::Mcp => page.mcp(),
        Tab::Skills => page.skills(),
        Tab::Instructions => page.instructions(),
    }
    // What a row opened on leaves no empty row at the page's end to scroll to.
    while page.rows.last().is_some_and(blank) {
        page.rows.pop();
    }
    let place = (&prefs.top, &prefs.reveal);
    connect::page(canvas, ui, "Settings", page.rows, page.seen, place)
}
