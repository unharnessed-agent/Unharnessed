//! The providers' screen: every provider the agent knows, in groups
//! (connected, the servers of this machine, subscriptions, API keys), a
//! row opening on its key or on a question before its credential changes;
//! and the screen of a browser sign-in under way. It takes the place of the
//! conversation, as in the extension (`vscode/webview/Providers.tsx`,
//! `SignIn.tsx`). This file holds what the user set on it and how it is
//! drawn; what the agent says of its providers is in `providers`.

use std::cell::Cell;

use crate::canvas::Canvas;
use crate::chat::Chat;
use crate::draft::Draft;
use crate::line::{cells, Line};
use crate::motion::spinner;
use crate::prefs::{Item, Tab};
use crate::providers::{Key, Prompt, Provider, Providers, SignIn, Teams};
use crate::text::{fit, glyphs, width, wrap, wrap_hard, Glyph, Ink};
use crate::theme::{ACCENT, DANGER, INK, RAISED};
use crate::view::{caret, with_caret, Area, Screen, Ui, MARGIN};

/// The widest the screen's column runs.
const PAGE_MAX: u16 = 80;
/// From this many providers on, a field filters them (the extension's).
const SEARCH_FROM: usize = 9;
/// How long a copy says it was made, in seconds.
const COPIED: f64 = 1.2;
/// The widest a provider's name is given before what is said of it.
const NAME_MAX: usize = 40;

/// What a row is open on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Its key, to write.
    Key,
    /// The question before a sign-in that replaces a credential.
    SignIn,
    /// The question before its credential is removed.
    Leave,
}

/// What a button does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    /// On a row's form: the key is saved, the sign-in starts, the question
    /// before a removal comes, the credential is removed, the form closes.
    Save,
    SignIn,
    Leave,
    Remove,
    Cancel,
    /// On a sign-in: its page opens, its address or its code is copied, its
    /// other way opens, what was written goes, it stops.
    Open,
    Copy,
    Other,
    Continue,
    Stop,
    /// On a sign-in that failed: once more, or back to the list.
    Retry,
    Close,
    /// On a sign-in's teams: the choice stays as it is.
    Keep,
}

/// What a click lands on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Spot {
    /// The screen's title: back to the conversation, or out of the sign-in.
    Back,
    /// A provider's row, and what removes its credential.
    Row(Key),
    Leave(Key),
    Do(Act),
    /// A team of a sign-in, by its place: the personal account first.
    Team(usize),
    /// Under the message box, what leads to this screen.
    Connect,
    /// A file the `@` menu offers, by its place.
    Pick(usize),
    /// Under the message box, the model and the reasoning effort: each
    /// opens its menu. In them, a model by its place among those shown,
    /// and a level by its place among the model's.
    Model,
    Effort,
    Choice(usize),
    Level(usize),
    /// Beside the wordmark: a new conversation, and the saved ones.
    New,
    History,
    /// A saved conversation, by its id, and what deletes it.
    Chat(String),
    Forget(String),
    /// On the kernel's panel: what stops a command the agent left running,
    /// by its id, and a port it listens on, which opens in the browser.
    Kill(String),
    Port(u64),
    /// The kernel's panel: what closes it, and its rail, which opens it.
    Panel,
    /// On the message box, while the conversation has a goal: what lets a
    /// goal that waits go on, and what ends it.
    Play,
    EndGoal,
    /// On the message box: what takes an image off the message, by its
    /// place; and, while an earlier message is being changed, what leaves
    /// it as it was.
    Detach(usize),
    Keep,
    /// Beside the wordmark: the settings.
    Settings,
    /// On top of the message box, the messages that wait: their head,
    /// which folds them or opens them; a message's row, which brings it
    /// back to the box to be changed, as its pencil does; what sends it
    /// into the running turn now; what takes it out of the line. Each by
    /// its number. Once a stop paused them, what plays them again.
    Queue,
    Resume,
    Waiting(u64),
    Rush(u64),
    Hold(u64),
    Unqueue(u64),
    /// On the settings' screen: a tab; a row, to act on it; the switch at
    /// the end of a row that also opens; what removes a row. In an open
    /// form: where the keys go, by its place among the form's stops (a
    /// button is pressed); an option of the choice at that place; a row
    /// of its list.
    Tab(Tab),
    Pref(Item),
    Flip(Item),
    Drop(Item),
    Stop(usize),
    Opt(usize, usize),
    Entry(usize),
}

/// A row's form.
#[derive(Clone)]
pub struct Form {
    pub row: Key,
    pub mode: Mode,
    /// The key being written.
    pub field: Draft,
    /// It asked for its change: why that failed shows on it.
    pub tried: bool,
    /// How many changes had gone through when it opened: the next closes it.
    seen: u32,
}

/// The providers' screen as the user left it.
#[derive(Clone, Default)]
pub struct Connect {
    /// What the rows are filtered by.
    pub query: Draft,
    /// The row the arrows chose.
    pub chosen: Option<Key>,
    pub form: Option<Form>,
    /// The button Enter presses, among those the screen shows.
    pub focus: usize,
    /// What is written for what a sign-in asks, and which prompt it is for.
    pub answer: Draft,
    asked: Option<u64>,
    /// The sign-in's other way is open.
    pub other: bool,
    /// The team the arrows chose.
    pub team: usize,
    /// When the sign-in's address or code was last copied.
    pub copied: Option<f64>,
    /// The first row in view, and whether the chosen row is to be brought into it.
    top: Cell<usize>,
    reveal: Cell<bool>,
}

/// The rows of a group, under its name.
pub struct Group<'a> {
    pub label: &'static str,
    pub rows: Vec<&'a Provider>,
}

/// The providers in their groups, those whose name or id holds `query`.
#[must_use]
pub fn groups<'a>(list: &'a [Provider], query: &str) -> Vec<Group<'a>> {
    let words = query.trim().to_lowercase();
    let shown: Vec<&Provider> = list
        .iter()
        .filter(|row| {
            words.is_empty()
                || format!("{} {}", row.name, row.key.id)
                    .to_lowercase()
                    .contains(&words)
        })
        .collect();
    let group = |label, keep: &dyn Fn(&Provider) -> bool| Group {
        label,
        rows: shown.iter().copied().filter(|row| keep(row)).collect(),
    };
    let mut groups = vec![
        group("Connected", &|row| row.local.is_none() && row.connected),
        group("Local", &|row| row.local.is_some()),
        group("Subscriptions", &|row| {
            row.local.is_none() && !row.connected && row.key.subscription
        }),
        group("API keys", &|row| {
            row.local.is_none() && !row.connected && !row.key.subscription
        }),
    ];
    groups.retain(|group| !group.rows.is_empty());
    groups
}

/// The prompt whose field shows: one the sign-in cannot go on without, or
/// its other way once the user opened it.
#[must_use]
pub fn asking<'a>(sign_in: &'a SignIn, connect: &Connect) -> Option<&'a Prompt> {
    let prompt = sign_in.prompt.as_ref()?;
    (!beside(sign_in) || connect.other).then_some(prompt)
}

/// What the sign-in asks is another way than its page, running beside it.
fn beside(sign_in: &SignIn) -> bool {
    sign_in.url.is_some() && (sign_in.prompt.as_ref()).is_some_and(|prompt| prompt.optional)
}

/// The code a device sign-in gives to enter on its page.
#[must_use]
pub fn code(sign_in: &SignIn) -> Option<&str> {
    let code = (sign_in.instructions.as_deref()?)
        .strip_prefix("Enter code:")?
        .trim();
    (!code.is_empty() && !code.contains(char::is_whitespace)).then_some(code)
}

/// The buttons the screen shows, in the order Tab goes through them.
#[must_use]
pub fn acts(providers: &Providers, connect: &Connect) -> Vec<Act> {
    if let Some(sign_in) = &providers.sign_in {
        if sign_in.error.is_some() {
            return vec![Act::Retry, Act::Close];
        }
        let mut acts = Vec::new();
        if sign_in.url.is_some() {
            acts.extend([Act::Open, Act::Copy]);
        }
        if sign_in.teams.is_some() {
            acts.push(Act::Keep);
        } else if asking(sign_in, connect).is_some() {
            acts.push(Act::Continue);
        } else if beside(sign_in) {
            acts.push(Act::Other);
        }
        acts.push(Act::Stop);
        return acts;
    }
    let Some(form) = &connect.form else {
        return Vec::new();
    };
    let removable = (providers.row(&form.row)).is_some_and(|row| row.removable);
    let first = match form.mode {
        Mode::Key => Act::Save,
        Mode::SignIn => Act::SignIn,
        Mode::Leave => return vec![Act::Remove, Act::Cancel],
    };
    if removable {
        vec![first, Act::Leave, Act::Cancel]
    } else {
        vec![first, Act::Cancel]
    }
}

impl Connect {
    /// A row's form opens.
    pub fn open(&mut self, row: Key, mode: Mode, providers: &Providers) {
        self.chosen = Some(row.clone());
        self.form = Some(Form {
            row,
            mode,
            field: Draft::default(),
            tried: false,
            seen: providers.changes,
        });
        self.focus = 0;
        self.reveal.set(true);
    }

    /// The form closes.
    pub fn close(&mut self) {
        self.form = None;
        self.focus = 0;
    }

    /// The screen follows what the agent said: a form closes once what it
    /// asked for went through, what was written for a prompt goes with it,
    /// and a sign-in that is over leaves nothing set.
    pub fn follow(&mut self, providers: &Providers) {
        if (self.form.as_ref()).is_some_and(|form| form.seen != providers.changes) {
            self.close();
            // The list is in view from its start, where what is connected is; a
            // row that no longer is went back down among the others.
            self.top.set(0);
            let connected = |key: &Key| providers.row(key).is_some_and(|row| row.connected);
            self.chosen = self.chosen.take().filter(connected);
        }
        let asked = (providers.sign_in.as_ref())
            .and_then(|sign_in| sign_in.prompt.as_ref())
            .map(|prompt| prompt.id);
        if asked != self.asked {
            self.asked = asked;
            self.answer = Draft::default();
        }
        if providers.sign_in.is_none() {
            self.other = false;
            self.team = 0;
            self.copied = None;
        }
    }

    /// Where what is typed goes: the open form's key, what a sign-in asks,
    /// else the filter.
    pub fn field(&mut self, providers: &Providers) -> Option<&mut Draft> {
        if let Some(sign_in) = &providers.sign_in {
            return (sign_in.error.is_none() && asking(sign_in, self).is_some())
                .then_some(&mut self.answer);
        }
        match &mut self.form {
            Some(form) if form.mode == Mode::Key => Some(&mut form.field),
            Some(_) => None,
            // The rows are about to change: none of them is chosen, and they show from the first.
            None => {
                self.top.set(0);
                self.chosen = None;
                Some(&mut self.query)
            }
        }
    }

    /// The arrows: the chosen row moves `by` rows through the list, or the
    /// chosen team through the sign-in's.
    pub fn step(&mut self, by: isize, providers: &Providers) {
        if let Some(sign_in) = &providers.sign_in {
            if let Some(teams) = &sign_in.teams {
                self.team = (self.team.saturating_add_signed(by)).min(teams.list.len());
                self.reveal.set(true);
            }
            return;
        }
        if self.form.is_some() {
            return;
        }
        let Some(list) = &providers.list else {
            return;
        };
        let order: Vec<&Key> = groups(list, self.query.text())
            .into_iter()
            .flat_map(|group| group.rows)
            .map(|row| &row.key)
            .collect();
        let at =
            (self.chosen.as_ref()).and_then(|chosen| order.iter().position(|key| *key == chosen));
        let next = match at {
            Some(at) => at
                .saturating_add_signed(by)
                .min(order.len().saturating_sub(1)),
            None if by > 0 => 0,
            None => order.len().saturating_sub(1),
        };
        self.chosen = order.get(next).map(|key| (*key).clone());
        self.reveal.set(true);
    }

    /// The button Enter presses moves `by` places, round the screen's.
    pub fn tab(&mut self, by: isize, providers: &Providers) {
        let count = acts(providers, self).len();
        if count > 0 {
            let at = self.focus.min(count - 1).cast_signed() + by;
            self.focus = at.rem_euclid(count.cast_signed()).cast_unsigned();
        }
    }

    /// The screen's first row in view moves to `to`.
    pub fn show(&mut self, to: usize) {
        self.top.set(to);
    }
}

/// A row of a page, and what a click on its cells `from..to` lands on.
pub struct Row {
    pub line: Line,
    pub spots: Vec<(usize, usize, Spot)>,
}

impl Row {
    #[must_use]
    pub fn of(line: Line) -> Row {
        Row {
            line,
            spots: Vec::new(),
        }
    }

    #[must_use]
    pub fn blank() -> Row {
        Row::of(Line::blank())
    }

    #[must_use]
    pub fn on(mut self, from: usize, to: usize, spot: Spot) -> Row {
        self.spots.push((from, to, spot));
        self
    }
}

/// `text` in rows of `inner` cells, set in by `x`.
pub fn words(text: &str, ink: Ink, x: usize, inner: usize) -> impl Iterator<Item = Row> {
    wrap(glyphs(text, ink, 0), inner.max(8))
        .into_iter()
        .map(move |row| Row::of(Line::at(x, row)))
}

/// How a button looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    /// What the form is for.
    Main,
    Danger,
    Quiet,
    /// A line of text, underlined.
    Link,
}

/// The screen's buttons as they are drawn: which is Enter's, which is
/// under the pointer.
struct Buttons<'a> {
    acts: Vec<Act>,
    focus: usize,
    over: Option<&'a Spot>,
}

impl Buttons<'_> {
    /// A button: its glyphs, a cell of its surface each side of its word.
    fn button(&self, act: Act, label: &str, look: Look, ready: bool) -> (Vec<Glyph>, Spot) {
        let focus = self
            .acts
            .get(self.focus.min(self.acts.len().saturating_sub(1)));
        let lit = ready && (focus == Some(&act) || self.over == Some(&Spot::Do(act)));
        (pill(label, look, lit, ready), Spot::Do(act))
    }
}

/// A button's glyphs, a cell of its surface each side of its word: `lit`
/// when it is Enter's or under the pointer, dim while it cannot be pressed.
#[must_use]
pub fn pill(label: &str, look: Look, lit: bool, ready: bool) -> Vec<Glyph> {
    let ink = match look {
        Look::Main => Ink::of(ACCENT, 0.95).on(ACCENT, if lit { 0.26 } else { 0.13 }),
        Look::Danger => Ink::danger().on(DANGER, if lit { 0.24 } else { 0.12 }),
        Look::Quiet if lit => Ink::text().on(INK, 0.13),
        Look::Quiet => Ink::muted().on(INK, 0.06),
        Look::Link => {
            let mut ink = if lit { Ink::text() } else { Ink::muted() };
            ink.underline = true;
            ink
        }
    };
    let ink = if ready {
        ink
    } else {
        ink.alpha(ink.alpha * 0.4)
    };
    let label = if look == Look::Link {
        label.to_string()
    } else {
        format!(" {label} ")
    };
    glyphs(&label, ink, 0)
}

/// Buttons side by side from cell `x`, a cell of the page between them.
#[must_use]
pub fn side_by_side(buttons: Vec<(Vec<Glyph>, Spot)>, x: usize) -> Row {
    let mut row = Row::of(Line::at(x, Vec::new()));
    let mut at = x;
    for (button, spot) in buttons {
        if !row.line.glyphs.is_empty() {
            row.line.glyphs.extend(glyphs(" ", Ink::faint(), 0));
            at += 1;
        }
        let wide = width(&button);
        row.line.glyphs.extend(button);
        row.spots.push((at, at + wide, spot));
        at += wide;
    }
    row
}

/// A field, `inner` cells wide from cell `x`: a surface three rows high,
/// what is written on its middle row (what it asks for, while nothing is),
/// each character of a secret as a dot. The caret shows while what is
/// typed goes to it.
pub fn field(
    draft: &Draft,
    asks: &str,
    secret: bool,
    live: bool,
    x: usize,
    inner: usize,
) -> Vec<Row> {
    let surface = || Line::surface(RAISED, x, x + inner);
    let laid = draft.lay(inner.saturating_sub(4), 1, Ink::text());
    let mut row = laid.rows.into_iter().next().unwrap_or_default();
    if secret {
        row = row
            .iter()
            .flat_map(|glyph| glyphs(&"•".repeat(usize::from(glyph.width)), glyph.ink, glyph.src))
            .collect();
    }
    let row = if draft.is_empty() {
        let mut ask = fit(glyphs(asks, Ink::faint(), 0), inner.saturating_sub(4));
        if live {
            match ask.first_mut() {
                Some(first) => first.ink = caret(RAISED),
                None => ask = glyphs(" ", caret(RAISED), 0),
            }
        }
        ask
    } else if live {
        with_caret(row, laid.caret.1, caret(RAISED))
    } else {
        row
    };
    vec![
        Row::of(surface()),
        Row::of(Line::at(x + 2, row).on(RAISED, x, x + inner)),
        Row::of(surface()),
    ]
}

/// How a row is connected, or why it is not (the extension's words).
fn status(provider: &Provider) -> Vec<Glyph> {
    let muted = |text: &str| glyphs(text, Ink::muted(), 0);
    if let Some(models) = &provider.local {
        return muted(&models.join(", "));
    }
    let note = provider.note.as_deref();
    if provider.connected {
        let how = match note {
            Some(note) => match note.strip_prefix("env: ") {
                Some(name) => format!(" with {name} from your environment"),
                None => format!(" via {note}"),
            },
            None if provider.key.subscription => " with your subscription".to_string(),
            None => " with a saved API key".to_string(),
        };
        let mut out = glyphs("Connected", Ink::of(ACCENT, 0.9), 0);
        out.extend(muted(&how));
        return out;
    }
    match note {
        Some("subscription configured") => muted("The subscription sign-in is used instead"),
        Some("API key configured") => muted("The saved API key is used instead"),
        Some("expired") => muted("Sign-in expired"),
        Some(note) => muted(note),
        None => Vec::new(),
    }
}

/// What a row's form says before its buttons.
fn says(form: &Form, provider: &Provider, other: Option<&Provider>) -> Option<String> {
    let name = &provider.name;
    match form.mode {
        Mode::Key => {
            // What saving does to the key already in use.
            let outside = (provider.note.as_deref()).and_then(|note| note.strip_prefix("env: "));
            match (provider.connected, outside, other) {
                (true, Some(variable), _) => Some(format!(
                    "{variable} from your environment is in use. A key saved here is used instead."
                )),
                (true, None, _) if provider.removable => {
                    Some("A key is saved. A new one replaces it.".to_string())
                }
                (_, _, Some(other)) => {
                    Some(format!("Saving a key replaces the {} sign-in.", other.name))
                }
                _ => None,
            }
        }
        Mode::SignIn => Some(match other {
            _ if provider.connected => format!(
                "You are signed in to {name}. Sign in again to switch accounts or renew the sign-in."
            ),
            Some(other) if other.removable => {
                format!("Signing in replaces the saved {} API key.", other.name)
            }
            other => format!(
                "Once you are signed in, the subscription is used instead of the {} API key.",
                other.map_or(name.as_str(), |other| other.name.as_str())
            ),
        }),
        Mode::Leave if provider.local.is_some() => Some(format!(
            "Remove {name} and its models from the model menu?"
        )),
        Mode::Leave if provider.key.subscription => Some(format!("Sign out of {name}?")),
        Mode::Leave => Some(format!("Remove the saved {name} API key?")),
    }
}

/// A row's form, under it: what it says, its field, why it failed, its buttons.
fn form_rows(
    form: &Form,
    provider: &Provider,
    providers: &Providers,
    buttons: &Buttons,
    inner: usize,
) -> Vec<Row> {
    let x = 4;
    let inner = inner.saturating_sub(x + 2);
    let mut rows = vec![Row::blank()];
    if let Some(says) = says(form, provider, providers.other(&form.row)) {
        rows.extend(words(&says, Ink::muted(), x, inner));
        rows.push(Row::blank());
    }
    if form.mode == Mode::Key {
        let asks = format!("{} API key", provider.name);
        rows.extend(field(&form.field, &asks, true, true, x, inner));
        rows.push(Row::blank());
    }
    if let Some(error) = providers.error.as_ref().filter(|_| form.tried) {
        rows.extend(words(error, Ink::danger(), x, inner));
        rows.push(Row::blank());
    }
    let ready = !providers.busy;
    let local = provider.local.is_some();
    let made = (buttons.acts.iter())
        .map(|act| match act {
            Act::Save if providers.busy => buttons.button(*act, "Saving", Look::Main, false),
            Act::Save => {
                let written = !form.field.text().trim().is_empty();
                buttons.button(*act, "Save", Look::Main, written)
            }
            Act::SignIn if provider.connected => {
                buttons.button(*act, "Sign in again", Look::Main, true)
            }
            Act::SignIn => buttons.button(*act, "Sign in", Look::Main, true),
            Act::Remove if local => buttons.button(*act, "Remove", Look::Danger, ready),
            Act::Leave | Act::Remove => buttons.button(*act, "Disconnect", Look::Danger, ready),
            _ => buttons.button(*act, "Cancel", Look::Quiet, true),
        })
        .collect();
    rows.push(side_by_side(made, x));
    rows.push(Row::blank());
    rows
}

/// The list: what to do first while nothing is connected, the filter, the
/// groups. Answers its rows, and those of the chosen row among them.
fn list_rows(
    providers: &Providers,
    connect: &Connect,
    buttons: &Buttons,
    inner: usize,
    now: f64,
) -> (Vec<Row>, Option<(usize, usize)>) {
    let Some(list) = &providers.list else {
        let mut wait = glyphs(&spinner(now).to_string(), Ink::accent(), 0);
        wait.extend(glyphs(" Reading the providers", Ink::muted(), 0));
        return (vec![Row::of(Line::of(wait))], None);
    };
    let mut rows = Vec::new();
    if providers.none_connected() {
        let title = glyphs("Connect a provider", Ink::text().bold(), 0);
        rows.push(Row::of(Line::of(title)));
        rows.push(Row::blank());
        let what = "Sign in with a subscription, or add an API key.";
        rows.extend(words(what, Ink::muted(), 0, inner));
        rows.push(Row::blank());
    }
    let query = connect.query.text();
    if list.len() >= SEARCH_FROM || !query.is_empty() {
        let live = connect.form.is_none();
        rows.extend(field(
            &connect.query,
            "Search providers",
            false,
            live,
            0,
            inner,
        ));
        rows.push(Row::blank());
    }
    let groups = groups(list, query);
    if groups.is_empty() {
        let none = format!("No provider matches “{}”.", query.trim());
        rows.extend(words(&none, Ink::muted(), 0, inner));
    }
    // What is said of the rows starts at the same cell, after the longest of their names.
    let names = (groups.iter().flat_map(|group| &group.rows))
        .filter(|row| !status(row).is_empty())
        .map(|row| width(&glyphs(&row.name, Ink::text(), 0)))
        .max()
        .unwrap_or(0)
        .min(NAME_MAX);
    let mut chosen = None;
    for group in groups {
        rows.push(Row::of(Line::of(glyphs(group.label, Ink::faint(), 0))));
        rows.push(Row::blank());
        for provider in group.rows {
            let from = rows.len();
            let open = (connect.form.as_ref()).filter(|form| form.row == provider.key);
            let is_chosen = connect.chosen.as_ref() == Some(&provider.key);
            let over = buttons.over == Some(&Spot::Row(provider.key.clone()));
            rows.push(provider_row(
                provider,
                buttons,
                names,
                inner,
                is_chosen || over,
            ));
            if let Some(warning) = &provider.warning {
                rows.push(Row::blank());
                rows.extend(words(warning, Ink::muted(), 4, inner.saturating_sub(6)));
                rows.push(Row::blank());
            }
            if let Some(form) = open {
                // It starts with a row of the page, which a warning before it left already.
                let spaced = provider.warning.is_some();
                let form = form_rows(form, provider, providers, buttons, inner);
                rows.extend(form.into_iter().skip(usize::from(spaced)));
            }
            if is_chosen {
                chosen = Some((from, rows.len()));
            }
        }
        // A form left open on the group's last row ends on its own row of the page.
        if rows.last().is_some_and(|row| !row.line.glyphs.is_empty()) {
            rows.push(Row::blank());
        }
    }
    (rows, chosen)
}

/// A provider's row: its name, what is said of it, and on the right what
/// removes its credential. Its ink is full when it is chosen or under the
/// pointer.
fn provider_row(
    provider: &Provider,
    buttons: &Buttons,
    names: usize,
    inner: usize,
    lit: bool,
) -> Row {
    let key = &provider.key;
    let leave = if provider.removable || provider.local.is_some() {
        let word = if provider.local.is_some() {
            "Remove"
        } else {
            "Disconnect"
        };
        let over = buttons.over == Some(&Spot::Leave(key.clone()));
        glyphs(word, if over { Ink::text() } else { Ink::faint() }, 0)
    } else {
        Vec::new()
    };
    let room = inner.saturating_sub(
        4 + if leave.is_empty() {
            0
        } else {
            width(&leave) + 2
        },
    );
    let ink = if lit { Ink::text() } else { Ink::of(INK, 0.78) };
    let mut text = fit(glyphs(&provider.name, ink, 0), room.min(NAME_MAX));
    let said = status(provider);
    if !said.is_empty() {
        // At the cell the others start at, or nearer its name where it would not fit.
        let fits = room.saturating_sub(width(&text) + width(&said));
        let gap = (names + 3).saturating_sub(width(&text)).min(fits).max(2);
        text.extend(glyphs(&" ".repeat(gap), Ink::faint(), 0));
        text.extend(said);
        text = fit(text, room);
    }
    let body = 2 + width(&text);
    let mut line = Line::at(2, text);
    if lit {
        line = line.on(RAISED, 0, inner);
    }
    let mut row = Row::of(line);
    if leave.is_empty() {
        return row.on(0, inner, Spot::Row(key.clone()));
    }
    let at = inner.saturating_sub(2 + width(&leave));
    row.line.glyphs.extend(glyphs(
        &" ".repeat(at.saturating_sub(body)),
        Ink::faint(),
        0,
    ));
    row.line.glyphs.extend(leave);
    row.on(0, at.saturating_sub(1), Spot::Row(key.clone())).on(
        at.saturating_sub(1),
        inner,
        Spot::Leave(key.clone()),
    )
}

/// A sign-in's page: what to do on it (its code to enter, when it gives
/// one), what opens it and what copies its address or its code, then the
/// address in full: a terminal on another machine opens nothing here.
fn page_rows(
    sign_in: &SignIn,
    url: &str,
    copied: bool,
    buttons: &Buttons,
    inner: usize,
) -> Vec<Row> {
    let mut rows = Vec::new();
    let code = code(sign_in);
    if let Some(code) = code {
        rows.push(title("Enter this code on the sign-in page"));
        rows.push(Row::blank());
        let spaced: String = code.chars().flat_map(|ch| [ch, ' ']).collect();
        let spaced = glyphs(spaced.trim_end(), Ink::accent().bold(), 0);
        rows.push(Row::of(Line::of(spaced)));
    } else {
        rows.push(title("Sign in with your browser"));
        rows.push(Row::blank());
        let moves = "This screen moves on by itself once you are signed in.";
        rows.extend(words(moves, Ink::muted(), 0, inner));
    }
    rows.push(Row::blank());
    let (open, copy) = match (code.is_some(), copied) {
        (true, true) => ("Copy code and open the page", "Copied"),
        (true, false) => ("Copy code and open the page", "Copy code"),
        (false, true) => ("Open the sign-in page", "Copied"),
        (false, false) => ("Open the sign-in page", "Copy the address"),
    };
    rows.push(side_by_side(
        vec![
            buttons.button(Act::Open, open, Look::Main, true),
            buttons.button(Act::Copy, copy, Look::Quiet, true),
        ],
        0,
    ));
    rows.push(Row::blank());
    rows.extend(
        wrap_hard(glyphs(url, Ink::faint(), 0), inner)
            .into_iter()
            .map(|row| Row::of(Line::of(row))),
    );
    rows.push(Row::blank());
    rows
}

/// The teams a sign-in asks to choose from, the personal account first,
/// the current one marked; then what keeps the choice as it is. Answers
/// its rows, and the chosen team's among them.
fn team_rows(
    teams: &Teams,
    connect: &Connect,
    buttons: &Buttons,
    inner: usize,
) -> (Vec<Row>, usize) {
    let mut rows = vec![title("Choose a team"), Row::blank()];
    let mut chosen = 0;
    let personal = ("Personal account", None, teams.current.is_none());
    let named = teams.list.iter().map(|(id, name, role)| {
        let current = teams.current.as_deref() == Some(id.as_str());
        (name.as_str(), role.as_deref(), current)
    });
    for (at, (name, role, current)) in std::iter::once(personal).chain(named).enumerate() {
        let lit = connect.team == at || buttons.over == Some(&Spot::Team(at));
        let ink = if lit { Ink::text() } else { Ink::of(INK, 0.78) };
        let mut text = glyphs(name, ink, 0);
        if let Some(role) = role {
            text.extend(glyphs(&format!("   {role}"), Ink::muted(), 0));
        }
        if current {
            text.extend(glyphs("   ✓", Ink::accent(), 0));
        }
        let mut line = Line::at(2, fit(text, inner.saturating_sub(4)));
        if lit {
            line = line.on(RAISED, 0, inner);
        }
        if connect.team == at {
            chosen = rows.len();
        }
        rows.push(Row::of(line).on(0, inner, Spot::Team(at)));
    }
    rows.push(Row::blank());
    let keep = buttons.button(Act::Keep, "Keep the current choice", Look::Quiet, true);
    rows.push(side_by_side(vec![keep], 0));
    rows.push(Row::blank());
    (rows, chosen)
}

/// What a sign-in asks to be written: what it says of it, its field, what sends it.
fn prompt_rows(
    prompt: &Prompt,
    label: &str,
    secret: bool,
    connect: &Connect,
    buttons: &Buttons,
    inner: usize,
) -> Vec<Row> {
    let mut rows: Vec<Row> = words(label, Ink::muted(), 0, inner).collect();
    rows.push(Row::blank());
    rows.extend(field(&connect.answer, "", secret, true, 0, inner));
    rows.push(Row::blank());
    let ready = prompt.allow_empty || !connect.answer.text().trim().is_empty();
    let go = buttons.button(Act::Continue, "Continue", Look::Main, ready);
    rows.push(side_by_side(vec![go], 0));
    rows.push(Row::blank());
    rows
}

/// A title on the screen.
fn title(text: &str) -> Row {
    Row::of(Line::of(glyphs(text, Ink::text().bold(), 0)))
}

/// The sign-in under way: its page to open, what it asks, how it goes,
/// its other way, what stops it; or why it failed. Answers its rows, and
/// those of the chosen team among them.
fn sign_in_rows(
    sign_in: &SignIn,
    connect: &Connect,
    buttons: &Buttons,
    inner: usize,
    now: f64,
    wall: f64,
) -> (Vec<Row>, Option<(usize, usize)>) {
    let mut rows = Vec::new();
    if let Some(error) = &sign_in.error {
        rows.extend(words(error, Ink::danger(), 0, inner));
        rows.push(Row::blank());
        rows.push(side_by_side(
            vec![
                buttons.button(Act::Retry, "Try again", Look::Main, true),
                buttons.button(Act::Close, "Back", Look::Quiet, true),
            ],
            0,
        ));
        return (rows, None);
    }
    if let Some(url) = &sign_in.url {
        let copied = (connect.copied).is_some_and(|copied| wall - copied < COPIED);
        rows.extend(page_rows(sign_in, url, copied, buttons, inner));
    }
    let asked = asking(sign_in, connect);
    let other = beside(sign_in);
    let mut seen = None;
    if let (Some(asked), false) = (asked, other) {
        rows.extend(prompt_rows(
            asked,
            &asked.message,
            asked.secret,
            connect,
            buttons,
            inner,
        ));
    }
    if let Some(teams) = &sign_in.teams {
        let (teams, chosen) = team_rows(teams, connect, buttons, inner);
        seen = Some((rows.len() + chosen, rows.len() + chosen + 1));
        rows.extend(teams);
    } else if asked.is_none() || other {
        let says = (sign_in.status.as_deref()).unwrap_or(if sign_in.url.is_some() {
            "Waiting for you to sign in"
        } else {
            "Starting sign-in"
        });
        let mut wait = glyphs(&spinner(now).to_string(), Ink::accent(), 0);
        wait.extend(glyphs(&format!(" {says}"), Ink::muted(), 0));
        rows.push(Row::of(Line::of(fit(wait, inner))));
        rows.push(Row::blank());
    }
    // Prime's other way is a key; the others', the address the browser could not deliver.
    let prime = sign_in.provider == "prime-inference";
    match (asked, other) {
        (Some(asked), true) if prime => {
            rows.extend(prompt_rows(
                asked,
                "Prime API key",
                true,
                connect,
                buttons,
                inner,
            ));
        }
        (Some(asked), true) => {
            let label = "Copy the address of the page your browser ended on (it may not load) and paste it here.";
            rows.extend(prompt_rows(
                asked,
                label,
                asked.secret,
                connect,
                buttons,
                inner,
            ));
        }
        (None, true) => {
            let label = if prime {
                "Use an API key instead"
            } else {
                "Signed in, but nothing happens?"
            };
            let link = buttons.button(Act::Other, label, Look::Link, true);
            rows.push(side_by_side(vec![link], 0));
            rows.push(Row::blank());
        }
        _ => {}
    }
    let stop = buttons.button(Act::Stop, "Cancel", Look::Quiet, true);
    rows.push(side_by_side(vec![stop], 0));
    (rows, seen)
}

/// Draws the providers' screen under the wordmark: its title, a click on
/// which leaves it, then its rows, as many as the screen holds from the
/// first in view.
pub fn draw(canvas: &mut Canvas, chat: &Chat, ui: &Ui, connect: &Connect, now: f64) -> Screen {
    let inner = usize::from(column(canvas));
    let providers = &chat.providers;
    let buttons = Buttons {
        acts: acts(providers, connect),
        focus: connect.focus,
        over: ui.over.as_ref(),
    };
    let (name, (rows, seen)) = match &providers.sign_in {
        Some(sign_in) => (
            sign_in.name.as_str(),
            sign_in_rows(sign_in, connect, &buttons, inner, now, ui.wall),
        ),
        None => (
            "Providers",
            list_rows(providers, connect, &buttons, inner, now),
        ),
    };
    page(
        canvas,
        ui,
        name,
        rows,
        seen,
        (&connect.top, &connect.reveal),
    )
}

/// How wide a page's column runs on `canvas`.
#[must_use]
pub fn column(canvas: &Canvas) -> u16 {
    (canvas.width - 2 * MARGIN).min(PAGE_MAX)
}

/// A page under the wordmark, in the conversation's place: its title
/// `name`, a click on which leaves it, then its `rows`, as many as the
/// screen holds from the first in view (`place`: that row, and whether the
/// rows `seen`, just chosen, are to be brought into view).
pub fn page(
    canvas: &mut Canvas,
    ui: &Ui,
    name: &str,
    rows: Vec<Row>,
    seen: Option<(usize, usize)>,
    place: (&Cell<usize>, &Cell<bool>),
) -> Screen {
    let h = canvas.height;
    let column = column(canvas);
    let inner = usize::from(column);
    let (top_row, reveal) = place;
    let mut spots = Vec::new();
    let back = ui.over == Some(Spot::Back);
    let mut title = glyphs("← ", if back { Ink::text() } else { Ink::muted() }, 0);
    title.extend(glyphs(name, Ink::text().bold(), 0));
    let title = fit(title, inner);
    canvas.put(MARGIN, 2, MARGIN + column, &title, 1.0);
    spots.push(Area {
        y: 2,
        from: MARGIN,
        to: MARGIN + cells(width(&title)),
        spot: Spot::Back,
    });
    let top = 4;
    let room = usize::from(h.saturating_sub(top + 1));
    let most = rows.len().saturating_sub(room);
    let mut first = top_row.get().min(most);
    // The chosen row comes into view when it was just chosen, with what it opened on.
    if let (true, Some((from, to))) = (reveal.take(), seen) {
        if from < first {
            first = from;
        } else if to > first + room {
            first = (to - room).min(from);
        }
    }
    let first = first.min(most);
    top_row.set(first);
    for (at, row) in rows.into_iter().enumerate().skip(first).take(room) {
        let y = top + cells(at - first);
        canvas.line(MARGIN, y, column, &row.line);
        spots.extend(row.spots.into_iter().map(|(from, to, spot)| Area {
            y,
            from: MARGIN + cells(from),
            to: MARGIN + cells(to),
            spot,
        }));
    }
    Screen::page(most, first, spots)
}
