//! `unharnessed --tui-replay <recording>`: a recorded conversation played
//! on the screen at the pace it happened, or drawn at given moments into
//! text (`--at`), without a terminal. The frames go through the same
//! conversation and the same views as a live session's.

use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::canvas::Canvas;
use crate::chat::Chat;
use crate::desk::{self, Desk, FRAME, FRAME_LEAST};
use crate::fold::Folds;
use crate::recording::{self, Frame};
use crate::view::{self, Kept, Ui};

const USAGE: &str = "usage: unharnessed --tui-replay <recording> [--speed <factor>] [--from <seconds>] [--paused] [--open] [--no-panel]
       unharnessed --tui-replay <recording> --at <seconds>[,<seconds>...] [--size <columns>x<rows>] [--ansi] [--open] [--no-panel]

Plays a conversation recorded over ACP (scripts/unharnessed-tui-record.py) in the terminal.
With --at, prints the screen at each of those moments instead: as plain text, or with its colours (--ansi).";

struct Options {
    recording: String,
    speed: f64,
    /// Where the replay starts, and whether it starts paused.
    from: f64,
    paused: bool,
    /// The screen as it starts: the folds, the panel.
    ui: Ui,
    at: Option<Vec<f64>>,
    size: (u16, u16),
    ansi: bool,
}

fn options(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        recording: String::new(),
        speed: 1.0,
        from: 0.0,
        paused: false,
        ui: Ui::default(),
        at: None,
        size: (120, 36),
        ansi: false,
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--speed" => {
                let text = value("--speed")?;
                options.speed = text
                    .parse()
                    .ok()
                    .filter(|speed: &f64| speed.is_finite() && *speed > 0.0)
                    .ok_or_else(|| format!("--speed: `{text}` is not a positive number"))?;
            }
            "--from" => {
                let text = value("--from")?;
                options.from = text
                    .parse()
                    .ok()
                    .filter(|from: &f64| from.is_finite() && *from >= 0.0)
                    .ok_or_else(|| format!("--from: `{text}` is not a number of seconds"))?;
            }
            "--paused" => options.paused = true,
            "--at" => {
                let text = value("--at")?;
                let mut moments = Vec::new();
                for part in text.split(',') {
                    let moment: f64 = part
                        .trim()
                        .parse()
                        .ok()
                        .filter(|moment: &f64| moment.is_finite() && *moment >= 0.0)
                        .ok_or_else(|| format!("--at: `{part}` is not a number of seconds"))?;
                    moments.push(moment);
                }
                moments.sort_by(f64::total_cmp);
                options.at = Some(moments);
            }
            "--size" => {
                let text = value("--size")?;
                let size = text
                    .split_once('x')
                    .and_then(|(columns, rows)| Some((columns.parse().ok()?, rows.parse().ok()?)))
                    .filter(|&(columns, rows): &(u16, u16)| columns > 0 && rows > 0);
                options.size =
                    size.ok_or_else(|| format!("--size: `{text}` is not <columns>x<rows>"))?;
            }
            "--ansi" => options.ansi = true,
            "--open" => options.ui.folds = Folds::all(true),
            "--no-panel" => options.ui.panel = false,
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            path if options.recording.is_empty() => options.recording = path.to_string(),
            extra => return Err(format!("unexpected argument {extra}")),
        }
    }
    if options.recording.is_empty() {
        return Err("no recording given".to_string());
    }
    Ok(options)
}

/// Runs the replay command with what followed `--tui-replay`; answers the
/// process's exit code.
#[must_use]
pub fn run(args: &[String]) -> i32 {
    let options = match options(args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("Error: {error}\n{USAGE}");
            return 2;
        }
    };
    let frames = std::fs::read_to_string(&options.recording)
        .map_err(|error| error.to_string())
        .and_then(|text| recording::parse(&text));
    let frames = match frames {
        Ok(frames) => frames,
        Err(error) => {
            eprintln!("Error: {}: {error}", options.recording);
            return 1;
        }
    };
    let done = match &options.at {
        Some(moments) => capture(&frames, &options, moments),
        None if io::stdout().is_terminal() && io::stdin().is_terminal() => play(&frames, &options),
        None => {
            eprintln!("Error: the replay needs a terminal; --at <seconds> prints the screen without one\n{USAGE}");
            return 1;
        }
    };
    match done {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    }
}

/// Where the replay stands, for the top of the screen.
fn note(now: f64, length: f64, paused: bool) -> String {
    let paused = if paused { " · paused" } else { "" };
    format!("replay {:.1}s of {length:.1}s{paused}", now.min(length))
}

/// The conversation as the recording has it at a moment.
struct Session<'a> {
    frames: &'a [Frame],
    next: usize,
    chat: Chat,
}

impl<'a> Session<'a> {
    fn new(frames: &'a [Frame]) -> Session<'a> {
        Session {
            frames,
            next: 0,
            chat: Chat::default(),
        }
    }

    /// The frames that came up to `now` reach the conversation.
    fn advance(&mut self, now: f64) {
        while let Some(frame) = self.frames.get(self.next).filter(|frame| frame.at <= now) {
            self.chat.tick(frame.at);
            if frame.sent {
                self.chat.sent(frame.at, &frame.json);
            } else {
                self.chat.received(frame.at, &frame.json);
            }
            self.next += 1;
        }
        self.chat.tick(now);
    }

    fn length(&self) -> f64 {
        self.frames.last().map_or(0.0, |frame| frame.at)
    }
}

/// The screen at each of `moments`, printed.
fn capture(frames: &[Frame], options: &Options, moments: &[f64]) -> io::Result<()> {
    let mut session = Session::new(frames);
    let (columns, rows) = options.size;
    let mut ui = options.ui.clone();
    // Kept from a moment to the next, as from a frame to the next on the screen.
    let mut kept = Kept::default();
    let mut out = io::stdout().lock();
    for &moment in moments {
        session.advance(moment);
        // The screen of a replay paused at that moment.
        ui.note = note(moment, session.length(), true);
        let mut canvas = Canvas::new(columns, rows);
        let _ = view::draw(&mut canvas, &session.chat, &ui, moment, &mut kept);
        writeln!(out, "== {moment:.3}s {columns}x{rows}")?;
        out.write_all(
            if options.ansi {
                canvas.ansi()
            } else {
                canvas.text()
            }
            .as_bytes(),
        )?;
    }
    out.flush()
}

/// The replay's clock: the recording's seconds, at `speed`, unless paused.
struct Clock {
    /// The recording's time at `since`.
    base: f64,
    since: Instant,
    speed: f64,
    paused: bool,
}

impl Clock {
    fn now(&self) -> f64 {
        if self.paused {
            self.base
        } else {
            self.base + self.since.elapsed().as_secs_f64() * self.speed
        }
    }

    fn set(&mut self, to: f64) {
        self.base = to.max(0.0);
        self.since = Instant::now();
    }
}

/// The recording on the screen, until the user leaves.
fn play(frames: &[Frame], options: &Options) -> io::Result<()> {
    let mut terminal = desk::enter()?;
    let done = play_on(&mut terminal, frames, options);
    desk::leave();
    done
}

/// The replay as the user drives it.
struct Player<'a> {
    frames: &'a [Frame],
    session: Session<'a>,
    clock: Clock,
    desk: Desk,
}

impl Player<'_> {
    /// Going back: the conversation is read again from the start.
    fn seek(&mut self, to: f64) {
        if to < self.clock.now() {
            self.session = Session::new(self.frames);
        }
        self.clock.set(to);
    }

    /// A key; answers whether the replay goes on. What is typed goes to the
    /// message box, so the replay's own keys are chords: Ctrl+P pauses,
    /// Shift+Left and Shift+Right seek, Ctrl+R restarts, Ctrl+O opens the
    /// folds, Ctrl+K hides the panel, Ctrl+C or Esc leaves. Enter alone
    /// would send the message: a replay sends nothing.
    fn key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Char('c' | 'q') if ctrl => return false,
            KeyCode::Esc => return false,
            KeyCode::Char('p') if ctrl => {
                let now = self.clock.now();
                self.clock.set(now);
                self.clock.paused = !self.clock.paused;
            }
            KeyCode::Char('r') if ctrl => self.seek(0.0),
            KeyCode::Left if shift => self.seek(self.clock.now() - 2.0),
            KeyCode::Right if shift => self.seek(self.clock.now() + 2.0),
            KeyCode::Char('o') if ctrl => self.desk.ui.folds.toggle_all(),
            KeyCode::Char('k') if ctrl => self.desk.toggle_panel(),
            _ => {
                self.desk.write(key);
            }
        }
        true
    }
}

fn play_on(
    terminal: &mut ratatui::DefaultTerminal,
    frames: &[Frame],
    options: &Options,
) -> io::Result<()> {
    let started = Instant::now();
    let mut player = Player {
        frames,
        session: Session::new(frames),
        clock: Clock {
            base: options.from,
            since: Instant::now(),
            speed: options.speed,
            paused: options.paused,
        },
        desk: Desk::new(options.ui.clone()),
    };
    let length = player.session.length();
    loop {
        let now = player.clock.now();
        player.session.advance(now);
        player.desk.ui.note = note(now, length, player.clock.paused);
        player.desk.ui.wall = started.elapsed().as_secs_f64();
        player.desk.draw(terminal, &player.session.chat, now)?;
        let drawn = Instant::now();

        // What plays follows the replay's clock: paused, only a fold moves.
        let wait = if player.clock.paused && !player.desk.moving() {
            250
        } else {
            FRAME
        };
        if !event::poll(Duration::from_millis(wait))? {
            continue;
        }
        std::thread::sleep(Duration::from_millis(FRAME_LEAST).saturating_sub(drawn.elapsed()));
        // Everything that waits is taken before the next frame: a pointer
        // on its way sends many moves.
        loop {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if !player.key(key) {
                        return Ok(());
                    }
                }
                Event::Mouse(mouse) => {
                    player.desk.mouse(mouse);
                }
                Event::Paste(text) => player.desk.ui.draft.insert(&text),
                _ => {}
            }
            if !event::poll(Duration::ZERO)? {
                break;
            }
        }
    }
}
