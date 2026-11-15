//! The interactive terminal: `unharnessed` in a terminal, without `-p`.
//!
//! It is an ACP client, like the VS Code extension: it starts
//! `unharnessed --mode acp` as a child process and drives it over
//! JSON-RPC, so the agent, its sessions, the sign-ins and the model
//! picker are the same code the extension uses. This module only draws
//! and reads keys: an inline screen whose finished lines scroll into the
//! terminal's scrollback, with a live area and the input box below them.

mod agent;
mod commands;
mod editor;
mod login;
mod markdown;
mod picker;
mod screen;
mod session;
mod settings;
mod splash;
mod transcript;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::{json, Value};

use self::agent::{response, Agent, Incoming, RpcError};
use self::commands::{Command, Local};
use self::editor::{prompt_marker, Editor, EditorAction};
use self::picker::{Picker, PickerAction};
use self::screen::{wrap, Color, Line, Screen, Style};
use self::transcript::{duration, notice, spinner, Transcript};
use crate::mode::RunOptions;

const TICK: Duration = Duration::from_millis(40);
const ANIMATION: Duration = Duration::from_millis(80);
const COMPLETION_ROWS: usize = 6;
const FLASH_FOR: Duration = Duration::from_millis(2500);
const RETRY_AFTER: Duration = Duration::from_millis(250);

/// Run the interactive terminal until the user leaves. Returns the exit
/// code.
pub(crate) fn run(options: &RunOptions) -> i32 {
    let raw = match RawMode::enable() {
        Ok(raw) => raw,
        Err(error) => {
            eprintln!("Error: cannot use this terminal ({error}); use -p/--print instead");
            return 1;
        }
    };
    install_panic_hook();
    let mut app = match App::new(options) {
        Ok(app) => app,
        Err(error) => {
            drop(raw);
            eprintln!("Error: {error}");
            return 1;
        }
    };
    let code = app.main(options);
    app.leave();
    drop(app);
    drop(raw);
    code
}

/// Raw mode, bracketed paste and (where the terminal has it) the keyboard
/// protocol that tells Shift+Enter apart; all undone on drop, and by the
/// panic hook.
struct RawMode;

/// Whether the keyboard protocol was pushed (the panic hook reads it too).
static ENHANCED_KEYS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

impl RawMode {
    fn enable() -> std::io::Result<RawMode> {
        crossterm::terminal::enable_raw_mode()?;
        // From here on, the drop restores the terminal on every path.
        let raw = RawMode;
        let mut out = std::io::stdout();
        crossterm::execute!(out, crossterm::event::EnableBracketedPaste)?;
        let enhanced = std::env::var_os("UNHARNESSED_PLAIN_KEYS").is_none()
            && matches!(
                crossterm::terminal::supports_keyboard_enhancement(),
                Ok(true)
            );
        if enhanced {
            let pushed = crossterm::execute!(
                out,
                crossterm::event::PushKeyboardEnhancementFlags(
                    crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                )
            );
            ENHANCED_KEYS.store(pushed.is_ok(), std::sync::atomic::Ordering::Relaxed);
        }
        Ok(raw)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn restore_terminal() {
    let mut out = std::io::stdout();
    if ENHANCED_KEYS.swap(false, std::sync::atomic::Ordering::Relaxed) {
        let _ = crossterm::execute!(out, crossterm::event::PopKeyboardEnhancementFlags);
    }
    let _ = crossterm::execute!(
        out,
        crossterm::event::DisableBracketedPaste,
        crossterm::cursor::Show
    );
    let _ = crossterm::terminal::disable_raw_mode();
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        eprintln!();
        previous(info);
    }));
}

/// A turn in progress: the user's prompt, or one the agent started itself
/// (a background command finished, a goal continues).
struct Turn {
    started: Instant,
    /// The `promptTurnId` the agent gave it, once known.
    id: Option<u64>,
    /// Started by the agent (no prompt request answers it).
    wake: bool,
    cancelling: bool,
    /// The user's message, for a turn they started.
    text: Option<String>,
}

enum Modal {
    Picker(Picker),
    Ask(Ask),
}

struct Ask {
    title: String,
    note: String,
    placeholder: String,
    editor: Editor,
    allow_empty: bool,
}

/// How an [`App::ask_until`] prompt ended.
enum Asked {
    Answer(String),
    Cancelled,
    /// The stop condition fired before an answer.
    Stopped,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Block {
    User,
    Text,
    Tool,
    Notice,
}

#[allow(clippy::struct_excessive_bools)] // independent UI flags
pub(crate) struct App {
    screen: Screen,
    editor: Editor,
    agent: Agent,
    cwd: PathBuf,
    base_args: Vec<String>,
    session_id: Option<String>,
    /// The saved session file, once the conversation has one.
    saved_path: Option<String>,
    config: Vec<Value>,
    agent_commands: Vec<Command>,
    providers: Vec<Value>,
    usage: Option<Value>,
    transcript: Transcript,
    turn: Option<Turn>,
    prompt_request: Option<u64>,
    queue: VecDeque<String>,
    /// Requests whose answer a blocking call waits for.
    awaiting: HashSet<u64>,
    responses: HashMap<u64, Value>,
    login_updates: VecDeque<Value>,
    modal: Option<Modal>,
    /// A status line shown while the agent starts (the kernel setup).
    status: Option<String>,
    flash: Option<(Line, Instant)>,
    last_interrupt: Option<Instant>,
    completion: usize,
    completion_hidden: bool,
    background: usize,
    goal_status: Option<String>,
    pending: Vec<Line>,
    last_block: Option<Block>,
    last_blank: bool,
    dirty: bool,
    last_draw: Instant,
    dead: bool,
    quit: bool,
    /// Lines sent with Enter while the app waited on the agent (at start):
    /// submitted once it is free.
    deferred: VecDeque<String>,
    /// A message the agent refused because its last turn was still
    /// closing: sent again at that time, without a second echo.
    retry: Option<(String, Instant)>,
    /// The opening animation, while it plays.
    splash: Option<splash::Splash>,
}

impl App {
    fn new(options: &RunOptions) -> Result<App, String> {
        let agent_dir = &options.config.agent_dir;
        let history =
            (!agent_dir.as_os_str().is_empty()).then(|| agent_dir.join("prompt-history.jsonl"));
        let cwd = options.config.cwd.clone();
        let (base_args, start_args) = session::agent_args(options);
        let args = [base_args.clone(), start_args].concat();
        let agent = Agent::spawn(&args, &cwd)
            .map_err(|error| format!("cannot start the agent: {error}"))?;
        Ok(App {
            screen: Screen::new(),
            editor: Editor::new(history),
            agent,
            cwd,
            base_args,
            deferred: VecDeque::new(),
            retry: None,
            splash: None,
            session_id: None,
            saved_path: None,
            config: Vec::new(),
            agent_commands: Vec::new(),
            providers: Vec::new(),
            usage: None,
            transcript: Transcript::new(),
            turn: None,
            prompt_request: None,
            queue: VecDeque::new(),
            awaiting: HashSet::new(),
            responses: HashMap::new(),
            login_updates: VecDeque::new(),
            modal: None,
            status: None,
            flash: None,
            last_interrupt: None,
            completion: 0,
            completion_hidden: false,
            background: 0,
            goal_status: None,
            pending: Vec::new(),
            last_block: None,
            last_blank: true,
            dirty: true,
            last_draw: Instant::now(),
            dead: false,
            quit: false,
        })
    }

    fn main(&mut self, options: &RunOptions) -> i32 {
        self.splash = Some(splash::Splash::new());
        let connected = self.connect(session::Start::of(options));
        self.finish_splash();
        if let Err(error) = connected {
            self.commit(
                Block::Notice,
                notice("✗", &error, Style::fg(Color::Red), self.width()),
            );
            for line in self.agent.stderr_tail().iter().rev().take(8).rev() {
                self.commit(
                    Block::Notice,
                    vec![Line::styled(format!("    {line}"), Style::dim())],
                );
            }
            self.draw(true);
            return 1;
        }
        if !options.initial_images.is_empty() {
            self.note("·", "@image arguments are not sent from the interactive terminal yet; use -p for images", Style::fg(Color::Yellow));
        }
        self.onboarding();
        if let Some(message) = options
            .initial_message
            .clone()
            .filter(|m| !m.trim().is_empty())
        {
            self.submit(message);
        }
        while !self.quit {
            if let Some(line) = self.deferred.pop_front() {
                self.submit(line);
                continue;
            }
            if self
                .retry
                .as_ref()
                .is_some_and(|(_, at)| Instant::now() >= *at)
                && self.turn.is_none()
            {
                if let Some((text, _)) = self.retry.take() {
                    self.request_prompt(&text);
                }
            }
            let event = self.pump(TICK);
            self.on_event(event);
        }
        0
    }

    fn width(&self) -> usize {
        self.screen.text_width()
    }

    // ── Output ──────────────────────────────────────────────────────────

    /// Wait for the opening animation to play out, then make its last
    /// frame the header, above everything committed while it played.
    fn finish_splash(&mut self) {
        while self
            .splash
            .as_ref()
            .is_some_and(|splash| !splash.settled(Instant::now()))
        {
            let event = self.pump(TICK);
            self.on_event(event);
        }
        let Some(splash) = self.splash.take() else {
            return;
        };
        let width = self.width();
        let mut header = vec![Line::new()];
        header.extend(splash.render(width, Instant::now()));
        header.push(Line::new());
        header.push(
            Line::styled(
                "  One tool: a Python REPL that stays alive. The model writes the rest.",
                Style::dim(),
            )
            .truncate(width),
        );
        header.push(
            Line::plain("  ")
                .with(crate::config::version(), Style::fg(Color::Accent))
                .with(format!(" · {}", self.short_cwd()), Style::dim())
                .truncate(width),
        );
        header.push(
            Line::plain("  ")
                .with("ctrl+p", Style::PLAIN.bold())
                .with(" model · ", Style::dim())
                .with("shift+tab", Style::PLAIN.bold())
                .with(" effort · ", Style::dim())
                .with("tab", Style::PLAIN.bold())
                .with(" commands · ", Style::dim())
                .with("esc", Style::PLAIN.bold())
                .with(" interrupt", Style::dim())
                .truncate(width),
        );
        header.push(Line::new());
        let rest = std::mem::take(&mut self.pending);
        self.pending = header;
        self.pending.extend(rest);
        self.last_blank = self.pending.last().is_some_and(Line::is_blank);
        self.dirty = true;
    }

    /// Queue finished lines for the next draw, with one blank line between
    /// blocks of different kinds and never two blank lines in a row.
    fn commit(&mut self, block: Block, lines: Vec<Line>) {
        if lines.is_empty() {
            return;
        }
        let gap = match (self.last_block, block) {
            (Some(Block::Tool | Block::User), _) | (Some(_), Block::Tool | Block::User) => true,
            (Some(previous), _) => {
                previous != block && block != Block::Notice && previous != Block::Notice
            }
            (None, _) => false,
        };
        if gap && !self.last_blank {
            self.pending.push(Line::new());
            self.last_blank = true;
        }
        for line in lines {
            let blank = line.is_blank();
            if blank && self.last_blank {
                continue;
            }
            self.last_blank = blank;
            self.pending.push(line);
        }
        self.last_block = Some(block);
        self.dirty = true;
    }

    fn note(&mut self, icon: &str, text: &str, style: Style) {
        let lines = notice(icon, text, style, self.width());
        self.commit(Block::Notice, lines);
    }

    fn flash(&mut self, text: &str, style: Style) {
        self.flash = Some((Line::styled(text, style), Instant::now()));
        self.dirty = true;
    }

    fn animating(&self) -> bool {
        self.turn.is_some()
            || self.status.is_some()
            || self.splash.is_some()
            || self.transcript.is_moving()
    }

    fn draw(&mut self, force: bool) {
        let now = Instant::now();
        let tick = self.animating() && now.duration_since(self.last_draw) >= ANIMATION;
        if !(force || self.dirty || tick || !self.pending.is_empty()) {
            return;
        }
        if self
            .flash
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > FLASH_FOR)
        {
            self.flash = None;
        }
        let (frame, cursor) = self.frame(now);
        // While the opening plays, what is committed waits under it.
        let committed = if self.splash.is_some() {
            Vec::new()
        } else {
            std::mem::take(&mut self.pending)
        };
        self.screen.draw(&committed, &frame, cursor);
        self.dirty = false;
        self.last_draw = now;
    }

    fn frame(&self, now: Instant) -> (Vec<Line>, Option<(usize, usize)>) {
        let width = self.width();
        let mut rows = match &self.splash {
            Some(splash) => {
                let mut rows = vec![Line::new()];
                rows.extend(splash.render(width, now));
                rows.push(Line::new());
                rows
            }
            None => Vec::new(),
        };
        rows.extend(self.transcript.live(width, now));
        if let Some(turn) = &self.turn {
            let mut line = Line::plain("  ");
            if !self.transcript.is_moving() {
                line.push(format!("{} ", spinner(now)), Style::fg(Color::Accent));
            }
            let label = if turn.cancelling {
                "interrupting"
            } else {
                "working"
            };
            line.push(
                format!("{label} {}", duration(turn.started.elapsed())),
                Style::dim(),
            );
            line.push(" · esc to interrupt", Style::dim());
            rows.push(line);
        } else if let Some(status) = &self.status {
            rows.push(
                Line::plain("  ")
                    .with(format!("{} ", spinner(now)), Style::fg(Color::Accent))
                    .with(status.clone(), Style::dim())
                    .truncate(width),
            );
        }
        for queued in &self.queue {
            let first = queued.lines().next().unwrap_or_default();
            rows.push(
                Line::plain("  ")
                    .with("↳ next  ", Style::fg(Color::Accent))
                    .with(first.to_string(), Style::dim())
                    .truncate(width),
            );
        }
        let rule = Line::styled("─".repeat(width), Style::dim());
        let mut cursor;
        let height = self.screen.height.saturating_sub(1).max(6);
        match &self.modal {
            Some(Modal::Picker(picker)) => {
                rows.push(Line::new());
                let (picker_rows, (row, col)) = picker.render(width, height.saturating_sub(2));
                cursor = Some((rows.len() + row, col));
                rows.extend(picker_rows);
            }
            Some(Modal::Ask(ask)) => {
                rows.push(Line::new());
                rows.extend(wrap(
                    &Line::styled(ask.title.clone(), Style::fg(Color::Accent).bold()),
                    width,
                    &Line::plain("  "),
                    &Line::plain("  "),
                ));
                if !ask.note.is_empty() {
                    for line in wrap(
                        &Line::styled(ask.note.clone(), Style::dim()),
                        width,
                        &Line::plain("  "),
                        &Line::plain("  "),
                    ) {
                        rows.push(line);
                    }
                }
                rows.push(rule.clone());
                let (input, (row, col)) =
                    ask.editor.render(width, &prompt_marker(), &ask.placeholder);
                cursor = Some((rows.len() + row, col));
                rows.extend(input);
                rows.push(rule);
                rows.push(Line::styled("  enter confirm · esc cancel", Style::dim()));
            }
            None => {
                rows.push(rule.clone());
                let (input, (row, col)) = self.editor.render(
                    width,
                    &prompt_marker(),
                    "Ask anything about this folder · tab for commands",
                );
                cursor = Some((rows.len() + row, col));
                rows.extend(input);
                rows.push(rule);
                let completions = self.completions();
                if completions.is_empty() {
                    rows.push(self.footer(width));
                } else {
                    rows.extend(self.completion_rows(&completions, width));
                }
            }
        }
        // Too tall for the window: drop live rows from the top.
        if rows.len() > height {
            let drop = rows.len() - height;
            rows.drain(..drop);
            cursor = cursor.map(|(row, col)| (row.saturating_sub(drop), col));
        }
        (rows, cursor)
    }

    fn footer(&self, width: usize) -> Line {
        let mut left = Line::plain("  ");
        if let Some((flash, _)) = &self.flash {
            left.append(flash);
            return left.truncate(width);
        }
        let mut parts: Vec<(String, Style)> = Vec::new();
        if let Some(model) = self.current_model_name() {
            parts.push((model, Style::PLAIN));
        }
        if let Some(level) = self.config_value("thought_level").and_then(Value::as_str) {
            if level != "off" {
                parts.push((level.to_string(), Style::dim()));
            }
        }
        if self.config_value("fast_mode").and_then(Value::as_bool) == Some(true) {
            parts.push(("fast".to_string(), Style::fg(Color::Yellow)));
        }
        if let Some(usage) = &self.usage {
            let tokens = usage.get("tokens").and_then(Value::as_u64);
            let window = usage.get("window").and_then(Value::as_u64).unwrap_or(0);
            if let (Some(tokens), true) = (tokens, window > 0) {
                let percent = tokens.saturating_mul(100) / window;
                parts.push((format!("context {percent}%"), Style::dim()));
            }
            if let Some(cost) = usage.get("cost").and_then(Value::as_f64) {
                if cost > 0.0 {
                    parts.push((format!("${cost:.3}"), Style::dim()));
                }
            }
            if usage.get("compacting").and_then(Value::as_bool) == Some(true) {
                parts.push(("compacting…".to_string(), Style::fg(Color::Yellow)));
            }
        }
        if self.background > 0 {
            let plural = if self.background == 1 { "" } else { "s" };
            parts.push((
                format!("{} background command{plural}", self.background),
                Style::fg(Color::Accent),
            ));
        }
        if let Some(goal) = &self.goal_status {
            parts.push((format!("goal {goal}"), Style::fg(Color::Magenta)));
        }
        if !self.signed_in() {
            parts.push((
                "not signed in · /login".to_string(),
                Style::fg(Color::Yellow),
            ));
        }
        for (index, (text, style)) in parts.into_iter().enumerate() {
            if index > 0 {
                left.push(" · ", Style::dim());
            }
            left.push(text, style);
        }
        let right = "/ commands";
        let room = width.saturating_sub(left.width() + right.len() + 2);
        if room > 0 {
            left.push(" ".repeat(room), Style::PLAIN);
            left.push(right, Style::dim());
        }
        left.truncate(width)
    }

    // ── The loop ────────────────────────────────────────────────────────

    /// Take in what the agent sent, redraw, and wait up to `timeout` for a
    /// terminal event.
    fn pump(&mut self, timeout: Duration) -> Option<Event> {
        while let Some(message) = self.agent.try_next() {
            self.on_incoming(message);
        }
        self.draw(false);
        match crossterm::event::poll(timeout) {
            Ok(true) => crossterm::event::read().ok(),
            _ => None,
        }
    }

    fn on_event(&mut self, event: Option<Event>) {
        match event {
            Some(Event::Key(key)) if key.kind != KeyEventKind::Release => self.on_key(key),
            Some(Event::Paste(text)) => {
                self.editor.insert_str(&text);
                self.dirty = true;
            }
            Some(Event::Resize(..)) => self.on_resize(),
            _ => {}
        }
    }

    fn on_resize(&mut self) {
        self.screen.resize();
        self.dirty = true;
    }

    fn on_key(&mut self, key: KeyEvent) {
        self.dirty = true;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.interrupt();
            return;
        }
        if key.code == KeyCode::Esc {
            if self.turn.is_some() {
                self.cancel_turn();
            } else {
                self.completion_hidden = true;
            }
            return;
        }
        if ctrl && key.code == KeyCode::Char('d') && self.editor.is_empty() {
            self.quit = true;
            return;
        }
        if ctrl && key.code == KeyCode::Char('l') {
            self.screen.resize();
            self.screen.clear_all();
            return;
        }
        // The quick switches (pi's keys): the model, the reasoning effort,
        // and every command from an empty input.
        if ctrl && key.code == KeyCode::Char('p') {
            self.pick_model("", None);
            return;
        }
        if key.code == KeyCode::BackTab {
            self.cycle_effort();
            return;
        }
        if key.code == KeyCode::Tab && self.editor.is_empty() {
            self.command_palette();
            return;
        }
        let completions = self.completions();
        if !completions.is_empty() {
            let count = completions.len().min(COMPLETION_ROWS);
            let selected = completions
                .get(self.completion.min(count - 1))
                .map(|c| c.name.clone());
            match key.code {
                KeyCode::Up => {
                    self.completion = self.completion.checked_sub(1).unwrap_or(count - 1);
                    return;
                }
                KeyCode::Down => {
                    self.completion = (self.completion + 1) % count;
                    return;
                }
                KeyCode::Tab => {
                    if let Some(name) = selected {
                        self.editor.set_text(&format!("/{name} "));
                    }
                    return;
                }
                KeyCode::Enter if !key.modifiers.contains(KeyModifiers::ALT) => {
                    if let Some(name) = selected {
                        let typed = self.editor.text();
                        self.editor.clear();
                        let line = if typed == format!("/{name}") {
                            typed
                        } else {
                            format!("/{name}")
                        };
                        self.completion = 0;
                        self.submit(line);
                    }
                    return;
                }
                _ => {}
            }
        }
        match self.editor.key(key) {
            EditorAction::Submit(text) => self.submit(text),
            EditorAction::Edited => {
                self.completion = 0;
                self.completion_hidden = false;
            }
            EditorAction::Ignored => {}
        }
    }

    /// Ctrl+C: interrupt the agent, else clear the input; twice within two
    /// seconds always quits, even while the agent works.
    fn interrupt(&mut self) {
        let again = self
            .last_interrupt
            .is_some_and(|at| at.elapsed() < Duration::from_secs(2));
        self.last_interrupt = Some(Instant::now());
        if again {
            self.quit = true;
        } else if self.turn.is_some() {
            self.cancel_turn();
            self.flash(
                "interrupting · ctrl+c again to quit",
                Style::fg(Color::Yellow),
            );
        } else if !self.editor.is_empty() {
            self.editor.clear();
        } else {
            self.flash("press ctrl+c again to quit", Style::fg(Color::Yellow));
        }
    }

    fn completions(&self) -> Vec<Command> {
        if self.completion_hidden || self.modal.is_some() {
            return Vec::new();
        }
        let typed = self.editor.text();
        if !typed.starts_with('/') || typed.contains('\n') {
            return Vec::new();
        }
        let all = self.all_commands();
        commands::complete(&typed, &all)
            .into_iter()
            .cloned()
            .collect()
    }

    fn all_commands(&self) -> Vec<Command> {
        let mut all = commands::local_commands();
        all.extend(self.agent_commands.iter().cloned());
        all
    }

    fn completion_rows(&self, completions: &[Command], width: usize) -> Vec<Line> {
        let count = completions.len().min(COMPLETION_ROWS);
        let selected = self.completion.min(count - 1);
        let name_width = completions
            .iter()
            .take(count)
            .map(|c| {
                c.name.len()
                    + 1
                    + if c.hint.is_empty() {
                        0
                    } else {
                        c.hint.len() + 1
                    }
            })
            .max()
            .unwrap_or(0);
        let mut rows = Vec::new();
        for (index, command) in completions.iter().take(count).enumerate() {
            let chosen = index == selected;
            let mut line = Line::new();
            line.push(
                if chosen { "  ❯ " } else { "    " },
                Style::fg(Color::Accent),
            );
            let name_style = if chosen {
                Style::fg(Color::Accent).bold()
            } else {
                Style::PLAIN
            };
            line.push(format!("/{}", command.name), name_style);
            let mut used = command.name.len() + 1;
            if !command.hint.is_empty() {
                line.push(format!(" {}", command.hint), Style::dim());
                used += command.hint.len() + 1;
            }
            line.push(
                " ".repeat(name_width.saturating_sub(used) + 2),
                Style::PLAIN,
            );
            line.push(command.description.clone(), Style::dim());
            rows.push(line.truncate(width));
        }
        if completions.len() > count {
            rows.push(Line::styled(
                format!("    … {} more", completions.len() - count),
                Style::dim(),
            ));
        }
        rows
    }

    // ── What the agent sends ────────────────────────────────────────────

    fn on_incoming(&mut self, message: Incoming) {
        match message {
            Incoming::Frame(frame) => self.on_frame(frame),
            Incoming::Stderr(line) => {
                // The kernel setup narrates itself on stderr (`› ...`).
                if let Some(rest) = line.strip_prefix("› ") {
                    if self.session_id.is_none() {
                        self.status = Some(rest.to_string());
                        self.dirty = true;
                    }
                }
            }
            Incoming::Closed => self.on_agent_closed(),
        }
    }

    fn on_frame(&mut self, frame: Value) {
        if let Some(id) = frame.get("id").and_then(Value::as_u64) {
            if frame.get("result").is_some() || frame.get("error").is_some() {
                if self.awaiting.remove(&id) {
                    self.responses.insert(id, frame);
                } else if self.prompt_request == Some(id) {
                    self.on_prompt_answer(&frame);
                }
                return;
            }
        }
        let method = frame
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = frame.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "session/update" => {
                if let Some(update) = params.get("update") {
                    self.on_update(update);
                }
            }
            "_unharnessed/providers/login_update" => self.login_updates.push_back(params),
            "_unharnessed/kernel/changed" => {
                if let Some(commands) = params.get("commands").and_then(Value::as_array) {
                    self.background = commands
                        .iter()
                        .filter(|c| c.get("status").and_then(Value::as_str) == Some("running"))
                        .count();
                    self.dirty = true;
                }
            }
            "_unharnessed/kernel/restored" => {
                if params.get("restored").and_then(Value::as_bool) == Some(true) {
                    self.note("↺", "the kernel's variables are back", Style::dim());
                } else {
                    self.note(
                        "↺",
                        "some kernel variables could not be restored",
                        Style::fg(Color::Yellow),
                    );
                }
            }
            _ => {}
        }
    }

    fn on_update(&mut self, update: &Value) {
        let width = self.width();
        let kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "agent_message_chunk" => {
                let text = update
                    .pointer("/content/text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let message = update.get("messageId").and_then(Value::as_str);
                let lines = self.transcript.text(message, text, width);
                self.commit(Block::Text, lines);
            }
            "agent_thought_chunk" => self.transcript.start_thinking(),
            "tool_call" => {
                let lines = self.transcript.tool_start(update, width);
                self.commit(Block::Text, lines);
            }
            "tool_call_update" => {
                let interrupted = self.turn.as_ref().is_some_and(|turn| turn.cancelling);
                let lines = self.transcript.tool_end(update, interrupted, width);
                self.commit(Block::Tool, lines);
            }
            "user_message_chunk" => {
                let lines = self.transcript.flush(width);
                self.commit(Block::Text, lines);
                match update.pointer("/content/type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = update
                            .pointer("/content/text")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        self.echo(text);
                    }
                    Some("image") => self.echo("[image]"),
                    _ => {}
                }
            }
            "available_commands_update" => self.agent_commands = commands::agent_commands(update),
            "config_option_update" => {
                if let Some(options) = update.get("configOptions").and_then(Value::as_array) {
                    self.config.clone_from(options);
                }
            }
            "session_info_update" => {
                if let Some(meta) = update.pointer("/_meta/unharnessed") {
                    self.on_info(meta);
                }
            }
            _ => {}
        }
        self.dirty = true;
    }

    fn on_info(&mut self, meta: &Value) {
        let width = self.width();
        if let Some(usage) = meta.get("usage") {
            self.usage = Some(usage.clone());
        }
        let turn_id = meta.get("promptTurnId").and_then(Value::as_u64);
        if let Some(start) = meta.get("turnStart") {
            match start.get("origin").and_then(Value::as_str) {
                Some("wake") if self.turn.is_none() && !self.transcript.replaying => {
                    self.turn = Some(Turn {
                        started: Instant::now(),
                        id: turn_id,
                        wake: true,
                        cancelling: false,
                        text: None,
                    });
                    let why = match start.get("wake").and_then(Value::as_str) {
                        Some("bash") => "a background command finished",
                        Some("quota") => "the usage limit reset; picking up again",
                        Some("goal") => "continuing the goal",
                        _ => "the agent picked up again",
                    };
                    self.note("↻", why, Style::fg(Color::Accent).dimmed());
                }
                Some("prompt") => {
                    if let Some(turn) = self.turn.as_mut() {
                        turn.id = turn_id;
                    }
                }
                _ => {}
            }
        }
        if let Some(thinking) = meta.get("thinking") {
            if thinking.get("state").and_then(Value::as_str) == Some("start") {
                self.transcript.start_thinking();
            } else {
                let lines = self.transcript.end_thinking();
                self.commit(Block::Text, lines);
            }
        }
        if meta.get("outputLimit").and_then(Value::as_bool) == Some(true) {
            self.note(
                "…",
                "the answer hit the output limit; the agent goes on",
                Style::dim(),
            );
        }
        if let Some(compaction) = meta.get("compaction") {
            if let Some(tokens) = compaction.get("tokensBefore").and_then(Value::as_u64) {
                let text = format!("compacted the conversation ({} tokens summarized; the kernel keeps every variable)", settings::compact_count(tokens));
                self.note("⟳", &text, Style::fg(Color::Accent).dimmed());
            }
        }
        if let Some(goal) = meta.get("goal") {
            let status = goal
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("idle")
                .to_string();
            let changed = self.goal_status.as_deref() != Some(status.as_str())
                && !(self.goal_status.is_none() && status == "idle");
            if changed {
                if status == "idle" {
                    self.note("◎", "goal cleared", Style::fg(Color::Magenta));
                } else {
                    let objective = goal
                        .get("objective")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let reason = goal
                        .get("lastReason")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let mut text = format!("goal {status}");
                    if !objective.is_empty() {
                        let _ = write!(text, ": {objective}");
                    }
                    if !reason.is_empty() && status != "active" {
                        let _ = write!(text, " ({reason})");
                    }
                    self.note("◎", &text, Style::fg(Color::Magenta));
                }
            }
            self.goal_status = (status != "idle").then_some(status);
        }
        if let Some(command) = meta.get("command") {
            let text = command
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let style = match command.get("severity").and_then(Value::as_str) {
                Some("error") => Style::fg(Color::Red),
                Some("warning") => Style::fg(Color::Yellow),
                _ => Style::PLAIN,
            };
            if !text.trim().is_empty() {
                let lines = notice("▸", text, style, width);
                self.commit(Block::Notice, lines);
            }
        }
        if let Some(end) = meta.get("turnEnd") {
            // Replay only: how a saved turn ended.
            let lines = self.transcript.flush(width);
            self.commit(Block::Text, lines);
            match end.get("state").and_then(Value::as_str) {
                Some("failed") => {
                    let error = end
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("The turn failed.");
                    self.note("✗", error, Style::fg(Color::Red));
                }
                Some("cancelled") => self.note("■", "interrupted", Style::fg(Color::Yellow)),
                _ => {}
            }
            self.commit(Block::Notice, vec![Line::new()]);
            self.last_block = None;
        }
        // A turn the agent started has no answer: its last update ends it.
        let phase = meta.get("phase").and_then(Value::as_str);
        let ends = phase == Some("terminalQuiescence")
            || (phase == Some("responseBoundary")
                && meta
                    .get("terminalQuiescenceExpected")
                    .and_then(Value::as_bool)
                    == Some(false));
        let is_wake = self
            .turn
            .as_ref()
            .is_some_and(|turn| turn.wake && (turn.id.is_none() || turn.id == turn_id));
        if ends && is_wake {
            if meta.get("outcome").and_then(Value::as_str) == Some("error") {
                self.end_turn(Some("The turn failed."), false);
            } else {
                self.end_turn(None, false);
            }
        }
    }

    fn on_prompt_answer(&mut self, frame: &Value) {
        self.prompt_request = None;
        let user_cancelled = self.turn.as_ref().is_some_and(|turn| turn.cancelling);
        let text = self.turn.as_ref().and_then(|turn| turn.text.clone());
        match response(frame) {
            Ok(result) => {
                let cancelled =
                    result.get("stopReason").and_then(Value::as_str) == Some("cancelled");
                self.end_turn(None, cancelled);
                // Stopped by something else (a cancel still landing on the
                // turn before): the message must not be lost.
                if cancelled && !user_cancelled {
                    if let Some(text) = text.filter(|_| self.editor.is_empty()) {
                        self.editor.set_text(&text);
                        self.flash(
                            "that message did not go through; it is back in the input",
                            Style::fg(Color::Yellow),
                        );
                    }
                }
            }
            // The agent had not freed the turn yet: send it again shortly.
            Err(RpcError(error)) if error.contains("prompt turn is already running") => {
                self.turn = None;
                if let Some(text) = text {
                    self.retry = Some((text, Instant::now() + RETRY_AFTER));
                }
            }
            Err(RpcError(error)) => {
                let error = error
                    .strip_prefix("unharnessed turn failed: ")
                    .unwrap_or(&error)
                    .to_string();
                self.end_turn(Some(&error), false);
            }
        }
    }

    fn on_agent_closed(&mut self) {
        if self.dead {
            return;
        }
        self.dead = true;
        self.status = None;
        if self.turn.is_some() {
            let lines = self.transcript.interrupt(self.width());
            self.commit(Block::Tool, lines);
            self.turn = None;
            self.prompt_request = None;
        }
        if self.session_id.is_some() {
            self.note("✗", "the agent stopped", Style::fg(Color::Red));
            for line in self.agent.stderr_tail().iter().rev().take(6).rev() {
                self.commit(
                    Block::Notice,
                    vec![Line::styled(format!("    {line}"), Style::dim())],
                );
            }
            self.note(
                "",
                "/new starts a new conversation, /resume reopens a saved one",
                Style::dim(),
            );
        }
    }

    // ── Turns ───────────────────────────────────────────────────────────

    fn echo(&mut self, text: &str) {
        let width = self.width();
        let mut lines = Vec::new();
        for (index, line) in transcript::sanitize(text).lines().enumerate() {
            let lead = if index == 0 {
                prompt_marker()
            } else {
                Line::plain("  ")
            };
            lines.extend(wrap(&Line::plain(line), width, &lead, &Line::plain("  ")));
        }
        if lines.is_empty() {
            lines.push(prompt_marker());
        }
        self.commit(Block::User, lines);
    }

    /// What Enter does with a typed line.
    fn submit(&mut self, text: String) {
        self.editor.remember(&text);
        if let Some((name, arg)) = commands::parse(&text) {
            if let Some(local) = commands::find_local(name) {
                let arg = arg.to_string();
                self.run_local(local, &arg);
                return;
            }
            if self.agent_commands.iter().all(|c| c.name != name) {
                let names: Vec<String> =
                    self.all_commands().iter().map(|c| c.name.clone()).collect();
                let names: Vec<&str> = names.iter().map(String::as_str).collect();
                let hint = crate::command_registry::find_command_suggestion(name, &names)
                    .map(|s| format!(" Did you mean /{s}?"))
                    .unwrap_or_default();
                self.flash(
                    &format!("Unknown command /{name}.{hint}"),
                    Style::fg(Color::Yellow),
                );
                self.editor.set_text(&text);
                return;
            }
        }
        if self.dead {
            self.flash(
                "the agent stopped: /new starts a new conversation",
                Style::fg(Color::Red),
            );
            self.editor.set_text(&text);
            return;
        }
        if self.turn.is_some() {
            self.queue.push_back(text);
            self.flash("queued: it goes out when the agent finishes", Style::dim());
            return;
        }
        self.send_prompt(&text);
    }

    fn send_prompt(&mut self, text: &str) {
        if self.session_id.is_none() {
            return;
        }
        self.echo(text);
        self.request_prompt(text);
    }

    fn request_prompt(&mut self, text: &str) {
        let Some(session) = self.session_id.clone() else {
            return;
        };
        let id = self.agent.request(
            "session/prompt",
            &json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}),
        );
        self.prompt_request = Some(id);
        self.turn = Some(Turn {
            started: Instant::now(),
            id: None,
            wake: false,
            cancelling: false,
            text: Some(text.to_string()),
        });
    }

    fn cancel_turn(&mut self) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        if turn.cancelling {
            return;
        }
        turn.cancelling = true;
        let wake = turn.wake;
        self.flash = None;
        if let Some(session) = self.session_id.clone() {
            self.agent
                .notify("session/cancel", &json!({"sessionId": session}));
        }
        // Messages waiting for this turn come back to the input box: the
        // user stopped the agent and decides again.
        if !self.queue.is_empty() {
            let mut queued: Vec<String> = self.queue.drain(..).collect();
            if !self.editor.is_empty() {
                queued.push(self.editor.text());
            }
            self.editor.set_text(&queued.join("\n\n"));
        }
        if wake {
            // A turn the agent started gets no answer when cancelled.
            self.end_turn(None, true);
        }
    }

    fn end_turn(&mut self, error: Option<&str>, cancelled: bool) {
        let width = self.width();
        let lines = self.transcript.interrupt(width);
        self.commit(Block::Text, lines);
        if let Some(error) = error {
            self.note("✗", error, Style::fg(Color::Red));
        } else if cancelled {
            self.note("■", "interrupted", Style::fg(Color::Yellow));
        }
        self.commit(Block::Notice, vec![Line::new()]);
        self.last_block = None;
        self.turn = None;
        if let Some(next) = self.queue.pop_front() {
            self.send_prompt(&next);
        }
    }

    // ── Blocking helpers (the loop keeps running inside them) ──────────

    /// Send a request and wait for its answer, still drawing and taking
    /// in updates. Ctrl+C stops waiting.
    fn call(&mut self, method: &str, params: &Value) -> Result<Value, RpcError> {
        if self.dead {
            return Err(RpcError("the agent is not running".to_string()));
        }
        let id = self.agent.request(method, params);
        self.awaiting.insert(id);
        loop {
            if let Some(frame) = self.responses.remove(&id) {
                return response(&frame);
            }
            if self.dead {
                self.awaiting.remove(&id);
                return Err(RpcError("the agent stopped".to_string()));
            }
            match self.pump(TICK) {
                Some(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c')
                    {
                        self.awaiting.remove(&id);
                        return Err(RpcError("cancelled".to_string()));
                    }
                    // Typing goes on in the input box; Enter keeps the line
                    // for when the agent is free.
                    if let EditorAction::Submit(line) = self.editor.key(key) {
                        self.deferred.push_back(line);
                        self.flash("sent once the agent is ready", Style::dim());
                    }
                    self.dirty = true;
                }
                Some(Event::Paste(text)) => {
                    self.editor.insert_str(&text);
                    self.dirty = true;
                }
                Some(Event::Resize(..)) => self.on_resize(),
                _ => {}
            }
        }
    }

    /// Open a picker and wait for a choice.
    fn pick(&mut self, picker: Picker) -> Option<Vec<usize>> {
        self.modal = Some(Modal::Picker(picker));
        self.dirty = true;
        let chosen = loop {
            if self.dead {
                break None;
            }
            match self.pump(TICK) {
                Some(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                    let Some(Modal::Picker(picker)) = self.modal.as_mut() else {
                        break None;
                    };
                    self.dirty = true;
                    match picker.key(key) {
                        PickerAction::Continue => {}
                        PickerAction::Done(chosen) => break Some(chosen),
                        PickerAction::Cancel => break None,
                    }
                }
                Some(Event::Resize(..)) => self.on_resize(),
                _ => {}
            }
        };
        self.modal = None;
        self.dirty = true;
        chosen
    }

    /// Ask for one line of text.
    fn ask(
        &mut self,
        title: &str,
        note: &str,
        placeholder: &str,
        masked: bool,
        initial: &str,
        allow_empty: bool,
    ) -> Option<String> {
        match self.ask_until(
            title,
            note,
            placeholder,
            masked,
            initial,
            allow_empty,
            |_| false,
        ) {
            Asked::Answer(answer) => Some(answer),
            Asked::Cancelled | Asked::Stopped => None,
        }
    }

    /// Ask for one line of text until `stop` says the answer is no longer
    /// needed (a browser sign-in finished while the paste field was open).
    #[allow(clippy::too_many_arguments)]
    fn ask_until(
        &mut self,
        title: &str,
        note: &str,
        placeholder: &str,
        masked: bool,
        initial: &str,
        allow_empty: bool,
        mut stop: impl FnMut(&mut App) -> bool,
    ) -> Asked {
        let mut editor = Editor::new(None);
        editor.masked = masked;
        editor.set_text(initial);
        self.modal = Some(Modal::Ask(Ask {
            title: title.to_string(),
            note: note.to_string(),
            placeholder: placeholder.to_string(),
            editor,
            allow_empty,
        }));
        self.dirty = true;
        let outcome = loop {
            if self.dead {
                break Asked::Cancelled;
            }
            if stop(self) {
                break Asked::Stopped;
            }
            let event = self.pump(TICK);
            let Some(Modal::Ask(ask)) = self.modal.as_mut() else {
                break Asked::Cancelled;
            };
            match event {
                Some(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                    self.dirty = true;
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    if key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('c')) {
                        break Asked::Cancelled;
                    }
                    if key.code == KeyCode::Enter {
                        let text = ask.editor.text().trim().to_string();
                        if !text.is_empty() || ask.allow_empty {
                            break Asked::Answer(text);
                        }
                        continue;
                    }
                    let _ = ask.editor.key(key);
                }
                Some(Event::Paste(text)) => {
                    ask.editor.insert_str(text.trim());
                    self.dirty = true;
                }
                Some(Event::Resize(..)) => self.on_resize(),
                _ => {}
            }
        };
        self.modal = None;
        self.dirty = true;
        outcome
    }

    // ── Local commands ──────────────────────────────────────────────────

    fn run_local(&mut self, local: Local, arg: &str) {
        match local {
            Local::Help => self.help(),
            Local::Model => self.pick_model(arg, None),
            Local::Effort => self.pick_effort(arg),
            Local::Fast => self.toggle_fast(),
            Local::Login => self.login(arg),
            Local::Logout => self.logout(arg),
            Local::New => self.restart(None, "new conversation"),
            Local::Resume => {
                let picked = match self.pick_saved() {
                    Ok(picked) => picked,
                    Err(error) => {
                        self.note("✗", &error, Style::fg(Color::Red));
                        return;
                    }
                };
                if let Some(saved) = picked {
                    let banner = format!("resumed: {}", saved.title);
                    self.restart(Some(saved.path), &banner);
                }
            }
            Local::Quit => self.quit = true,
        }
    }

    /// The first run with nothing connected: say so and open the sign-in.
    fn onboarding(&mut self) {
        if self.dead || self.signed_in() {
            return;
        }
        self.note("·", "No provider is connected yet. Pick one to sign in: a subscription, an API key, or a local or custom server.", Style::fg(Color::Yellow));
        self.login("");
    }

    fn leave(&mut self) {
        if self.turn.is_some() {
            self.cancel_turn();
        }
        let saved = self.saved_path.is_some() && !self.dead;
        self.status = Some("saving the conversation".to_string());
        self.draw(true);
        self.agent.shutdown(Duration::from_secs(10));
        self.status = None;
        if saved {
            self.commit(
                Block::Notice,
                vec![Line::styled(
                    "  saved · unharnessed -c picks it up again",
                    Style::dim(),
                )],
            );
        }
        let committed = std::mem::take(&mut self.pending);
        self.screen.draw(&committed, &[], None);
    }
}
