//! The live session: the terminal UI over a running agent, as its ACP
//! client. It starts the agent, opens a session on it (the saved
//! conversation it was launched on, when there is one), then the message
//! box sends, the conversation streams back, and the kernel's panel asks
//! for what it shows, and stops a server at a click. A message's `@`
//! mentions go with it as links, a command goes under the name the agent
//! knows it by (the `/` menu offers the agent's and the screen's own), and
//! a message can be sent into the turn that runs. A goal waits, goes on and ends from the message box, with
//! commands the user does not have to write. The providers' screen
//! acts through it too: a key
//! saved, a credential removed, a sign-in started, answered and stopped.
//! So do the menus of the session's model and reasoning effort, and the
//! saved conversations' screen: opening one, or a new one, lets go of the
//! agent (it saves its kernel and exits on its own) and starts another on
//! it, one conversation at a time. The settings' screen acts through it:
//! what it asks for goes to the agent, a sign-in to an MCP server takes
//! the providers' sign-in screen, and a restart starts another agent on
//! the conversation. A message goes with the images attached to it (files
//! dropped on the terminal, the paths VS Code's terminal types for them
//! taken as such; the clipboard's image, at Ctrl+V or at a Cmd+V the
//! terminal pastes nothing for); one that takes an
//! earlier message's place goes once the agent went back to before that
//! one. The frames exchanged can be recorded, in the format `--tui-replay`
//! plays.

use std::fs::File;
use std::io::{self, BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use serde_json::{json, Value};

use crate::agent::{Agent, Launch, Told};
use crate::chat::{self, Chat, KILL, REWIND, STEER};
use crate::clipboard::{self, Clip};
use crate::connect::{self, Act, Connect, Mode, Spot};
use crate::desk::{self, Click, Desk, FRAME, FRAME_LEAST};
use crate::files;
use crate::goal::{Goal, Status};
use crate::history::{self, History};
use crate::kernel::TAIL_LINES;
use crate::mention::{self, Paths};
use crate::message::{self, Attached, Message, MOST, UNNAMED};
use crate::pick::{self, Models, Pick};
use crate::prefs::{Prefs, Tab, Wish};
use crate::providers::{self, Key};
use crate::settings;
use crate::slash::{self, Command};
use crate::view::{Edited, Editing, Ui};

/// How often the screen is drawn again while nothing moves: a running
/// command's time goes on.
const IDLE: u64 = 250;
/// How long after something happened the screen keeps the pace of what
/// moves: what came in finishes coming in, what leaves the panel leaves.
const SETTLE: f64 = 4.0;
/// The most frames taken between two drawings.
const BATCH: usize = 2000;
/// How long the agent has to save its kernel and exit once its input
/// closed (the extension's), and after how long the wait is told.
const EXIT_WAIT: Duration = Duration::from_secs(40);
const EXIT_QUIET: Duration = Duration::from_millis(1200);
/// How long a button of the goal's waits for the goal to move before it
/// acts again (the extension's).
const GOAL_WAIT: f64 = 5.0;
/// How many of the agent's last error lines are kept, to show if it stops.
const SAID: usize = 20;
/// The requests whose parameter of that name is a secret: recorded, it is left out.
const SECRETS: [(&str, &str); 3] = [
    ("_unharnessed/providers/set_api_key", "apiKey"),
    ("_unharnessed/providers/login_answer", "answer"),
    (settings::MCP_SET_TOKEN, "token"),
];
/// How long a letter of what the terminal writes at once waits for the
/// next, and for how long it is held so: VS Code's terminal types the
/// path of a file dropped on it, in pieces of 50 characters 5 ms apart.
const RUN_GAP: Duration = Duration::from_millis(25);
const RUN_MOST: Duration = Duration::from_millis(400);

/// An image sent with a message.
#[derive(Clone, Debug)]
pub struct Image {
    pub mime: String,
    /// Its bytes, in base64.
    pub data: String,
}

/// How a live session starts.
#[derive(Clone, Debug)]
pub struct Start {
    pub launch: Launch,
    /// The agent's arguments for another conversation than the launch's:
    /// a new one, and a saved one (`--resume=<file>` is added to them).
    pub fresh: Vec<String>,
    pub saved: Vec<String>,
    /// The session's folder.
    pub cwd: String,
    /// The agent was launched on a saved conversation: it comes back first.
    pub resumes: bool,
    /// The launch left the conversation to choose (`--resume` alone): the
    /// session opens on the saved ones.
    pub choose: bool,
    /// The message the launch came with, and its images: sent first. Then
    /// the launch's other messages, each in its turn.
    pub first: Option<String>,
    pub images: Vec<Image>,
    pub then: Vec<String>,
    /// Where to record the frames exchanged.
    pub record: Option<PathBuf>,
    /// Opens an address in the user's browser: a port a server of the
    /// agent's listens on.
    pub open: fn(&str),
    /// Told before the conversation: a newer release, to update to.
    pub news: Option<String>,
}

/// What reaches the session: from the terminal, or from the agent.
enum Came {
    Screen(Event),
    /// From the agent started `n`th.
    Agent(u64, Told),
    /// The agent started `n`th, let go of, has exited.
    Left(u64),
    /// The session's folder was listed: its files.
    Files(Vec<String>),
    /// The clipboard was read: its image, or why there is none; and the
    /// paste that asked, a file's name, written when the clipboard does
    /// not hold that file.
    Clip(Result<Clip, String>, Option<String>),
    /// The terminal can no longer be read.
    Gone,
}

/// Where the session is with the agent.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// `initialize` asked: its request.
    Starting(i64),
    /// The saved conversation the agent holds is looked for (`session/list`).
    Finding(i64),
    /// `session/new` or `session/load` asked.
    Opening,
    Open,
    /// The conversation to open is held by the agent that leaves it: its
    /// own agent starts once that one has exited.
    Waiting,
    /// The agent ended.
    Over,
}

/// A message that is to take an earlier one's place: the agent goes back
/// to before that one first.
struct Edit {
    /// The earlier message's turn, and where the conversation stood before it.
    turn: usize,
    to: String,
    message: Message,
    /// The agent was asked to go back.
    asked: bool,
}

/// An agent the session let go of: it saves its kernel and exits.
struct Leaving {
    /// It was started `n`th.
    run: u64,
    /// The saved conversation it holds until it has exited.
    held: Option<String>,
    thread: JoinHandle<()>,
}

struct Session {
    desk: Desk,
    chat: Chat,
    agent: Option<Agent>,
    stage: Stage,
    start: Start,
    record: Option<BufWriter<File>>,
    /// The last request's id.
    asked: i64,
    clock: Instant,
    /// When something last happened.
    stirred: f64,
    /// What the agent last wrote for whoever reads its errors.
    said: Vec<String>,
    /// Why the agent ended before a session opened.
    failed: Option<Vec<String>>,
    /// The providers' screen was offered, or there was no need: once per session.
    offered: bool,
    /// Where what comes is told: the folder's files come from a thread of their own.
    tell: mpsc::Sender<Came>,
    /// What is being read on a thread of its own.
    reading: Reading,
    /// How many agents were started: the last one is the session's.
    run: u64,
    /// The agents let go of that have not exited yet.
    leaving: Vec<Leaving>,
    /// The conversation an agent waits to start on, and its arguments.
    next: Option<(String, Vec<String>)>,
    /// The saved conversations are to be listed once the session is open.
    owed: bool,
    /// The goal's command a button sent (`/goal pause`, `resume`, `clear`):
    /// it goes once nothing runs, before any message.
    control: Option<&'static str>,
    /// When a button of the goal's last acted, where the goal stood, and
    /// what it sent.
    pressed: Option<(f64, Status, &'static str)>,
    /// The agent's commands the `/` menu was last made from.
    commands: Arc<Vec<Command>>,
    /// The message that waits for the agent to go back to an earlier one.
    edit: Option<Edit>,
    /// The sign-in on the screen is to an MCP server, started from the
    /// settings: they come back when it is over.
    serving: bool,
}

/// What the session reads on threads of their own, while it does.
#[derive(Default)]
struct Reading {
    /// The session's folder, listed.
    folder: bool,
    clipboard: bool,
}

impl Session {
    fn now(&self) -> f64 {
        self.clock.elapsed().as_secs_f64()
    }

    /// A frame to the agent: the conversation reads it, the recording keeps it.
    fn out(&mut self, frame: &Value) {
        let now = self.now();
        self.chat.sent(now, frame);
        self.keep(now, '>', frame);
        if let Some(agent) = &self.agent {
            agent.send(frame);
        }
    }

    fn ask(&mut self, method: &str, params: &Value) -> i64 {
        self.asked += 1;
        let frame =
            json!({ "jsonrpc": "2.0", "id": self.asked, "method": method, "params": params });
        self.out(&frame);
        self.asked
    }

    fn notify(&mut self, method: &str, params: &Value) {
        self.out(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// A line of the recording; a recording that cannot be written stops.
    fn keep(&mut self, now: f64, arrow: char, frame: &Value) {
        let Some(record) = &mut self.record else {
            return;
        };
        let method = frame.get("method").and_then(Value::as_str);
        let secret = SECRETS
            .iter()
            .find(|(secret, _)| Some(*secret) == method)
            .map(|(_, param)| *param);
        let line = match secret {
            Some(param) if frame.pointer(&format!("/params/{param}")).is_some() => {
                let mut frame = frame.clone();
                frame["params"][param] = json!("<redacted>");
                frame.to_string()
            }
            _ => frame.to_string(),
        };
        let written = writeln!(record, "{now:7.3} {arrow} {line}").and_then(|()| record.flush());
        if written.is_err() {
            self.record = None;
        }
    }

    /// The first request: who the client is.
    fn begin(&mut self) {
        let id = self.ask(
            "initialize",
            &json!({
                "protocolVersion": 1,
                "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false }, "terminal": false },
                "clientInfo": { "name": "unharnessed-tui", "title": "Unharnessed terminal UI", "version": env!("CARGO_PKG_VERSION") },
            }),
        );
        self.stage = Stage::Starting(id);
        // Told once, after `initialize` started the conversation from nothing.
        if let Some(news) = self.start.news.take() {
            let now = self.now();
            self.chat.told(now, &news);
        }
    }

    /// The open session's id.
    fn open(&self) -> Option<String> {
        (self.stage == Stage::Open)
            .then(|| self.chat.session.clone())
            .flatten()
    }

    fn open_session(&mut self, saved: Option<&str>) {
        let mut params = json!({ "cwd": self.start.cwd, "mcpServers": [] });
        let method = match saved {
            Some(id) => {
                params["sessionId"] = json!(id);
                "session/load"
            }
            None => "session/new",
        };
        self.ask(method, &params);
        self.stage = Stage::Opening;
    }

    /// A frame from the agent.
    fn frame(&mut self, frame: &Value) {
        let now = self.now();
        self.keep(now, '<', frame);
        let answers = frame.get("id").and_then(Value::as_i64);
        match self.stage {
            Stage::Starting(id) if answers == Some(id) => {
                if let Some(error) = frame.get("error") {
                    let message = error.get("message").and_then(Value::as_str);
                    let why = format!(
                        "The agent refused to start: {}",
                        message.unwrap_or("it did not say why")
                    );
                    self.chat.stopped(now, &why, &[]);
                    self.stage = Stage::Over;
                    return;
                }
                self.ask(providers::LIST, &json!({}));
                // What this agent started with, and how it takes images.
                self.read_settings();
                if self.start.resumes {
                    // Whatever folder it was started in: a file given by its path may be from another.
                    let id = self.ask("session/list", &json!({}));
                    self.stage = Stage::Finding(id);
                } else {
                    self.open_session(None);
                }
            }
            Stage::Finding(id) if answers == Some(id) => {
                // The conversation the agent holds, once something was asked in it.
                let held = frame
                    .pointer("/result/sessions")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .find(|session| {
                        session.pointer("/_meta/unharnessed/current") == Some(&json!(true))
                    })
                    .and_then(|session| session.get("sessionId"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                self.open_session(held.as_deref());
            }
            _ => {
                self.chat.received(now, frame);
                self.offer();
                self.rewound(now);
                if self.stage == Stage::Opening && self.chat.session.is_some() {
                    self.stage = Stage::Open;
                    if std::mem::take(&mut self.owed) {
                        self.relist();
                    }
                    // The messages of a conversation that came back are there to bring back.
                    for turn in &self.chat.turns {
                        self.desk
                            .ui
                            .past
                            .keep(turn.prompt.as_deref().unwrap_or_default());
                    }
                }
                // What the turn did not take goes first of what waits.
                let untaken = self.chat.returned();
                self.desk.ui.wait_first(untaken);
                self.follow();
            }
        }
    }

    /// The agent went back to before an earlier message, or refused to:
    /// the message that takes its place goes, or shows with why it did not.
    fn rewound(&mut self, now: f64) {
        let Some(rewound) = self.chat.rewound.take() else {
            return;
        };
        let Some(edit) = self.edit.take() else {
            return;
        };
        match (rewound, self.open()) {
            (Ok(turn), Some(session)) => {
                self.desk.forget_from(turn);
                self.prompt(&session, &edit.message);
            }
            (Ok(_), None) => {}
            (Err(why), _) => self.chat.unsent(now, edit.message, &why),
        }
    }

    /// The agent's settings are asked for; its servers and its skills too,
    /// while their screen shows.
    fn read_settings(&mut self) {
        self.ask(settings::GET, &json!({}));
        if self.desk.ui.prefs.is_some() {
            self.ask(settings::MCP_LIST, &json!({}));
            self.ask(settings::SKILLS_LIST, &json!({}));
        }
    }

    /// An agent is there to ask.
    fn asks(&self) -> bool {
        self.agent.is_some() && !matches!(self.stage, Stage::Over | Stage::Waiting)
    }

    /// The `/` menu offers what the agent now lists, and the screen's own:
    /// a new agent lists its commands again.
    fn offer(&mut self) {
        if Arc::ptr_eq(&self.commands, &self.chat.commands) {
            return;
        }
        self.commands = Arc::clone(&self.chat.commands);
        self.desk.ui.commands = Some(Arc::new(slash::with_own(&self.commands)));
        self.desk.suggest();
    }

    /// The session's folder is listed again, for the `@` menu and the
    /// mentions it marks: from a thread, a large folder takes a moment.
    fn list(&mut self) {
        if std::mem::replace(&mut self.reading.folder, true) {
            return;
        }
        let (folder, tell) = (PathBuf::from(&self.start.cwd), self.tell.clone());
        std::thread::spawn(move || {
            let _ = tell.send(Came::Files(files::list(&folder)));
        });
    }

    /// The folder's files, as just listed.
    fn listed(&mut self, files: Vec<String>) {
        self.reading.folder = false;
        let files = Paths::of(files);
        let ui = &mut self.desk.ui;
        if ui.files.as_deref() != Some(&files) {
            ui.files = Some(Arc::new(files));
            ui.listed += 1;
        }
        self.desk.suggest();
    }

    /// The agent's output closed: it ended. Answers whether the session
    /// goes on: an agent that ended before any session opened leaves
    /// nothing to show but why, which the terminal keeps better.
    fn closed(&mut self) -> bool {
        let Some(agent) = self.agent.take() else {
            return true;
        };
        let how = match agent.stop(Duration::from_secs(2)) {
            Some(status) => match status.code() {
                Some(code) => format!("The agent stopped (exit code {code})."),
                None => "The agent stopped (killed by a signal).".to_string(),
            },
            None => "The agent stopped.".to_string(),
        };
        let said = std::mem::take(&mut self.said);
        // The launch's own agent: one started since leaves the screen to say why.
        if !matches!(self.stage, Stage::Open | Stage::Over) && self.run == 1 {
            self.failed = Some(if said.is_empty() { vec![how] } else { said });
            return false;
        }
        self.stage = Stage::Over;
        let now = self.now();
        self.chat.stopped(now, &how, &said);
        self.unqueue();
        // Its providers and its settings went with it: the conversation says what happened.
        self.leave();
        self.desk.ui.prefs = None;
        true
    }

    /// What goes by itself once the agent can take it: the launch's
    /// message, the messages that waited, what the panel shows.
    fn tick(&mut self) {
        let now = self.now();
        self.chat.tick(now);
        // A server added, changed or removed changes their list.
        if self.chat.settings.stale() && self.asks() {
            self.ask(settings::MCP_LIST, &json!({}));
        }
        let idle = self.open().is_some() && !self.chat.busy() && self.edit.is_none();
        self.desk.ui.editable = idle;
        let Some(session) = self.open() else {
            return;
        };
        if !self.offered && self.chat.providers.list.is_some() {
            self.offered = true;
            // A new conversation that nothing can answer yet starts on the providers.
            let new = self.start.first.is_none()
                && self.desk.ui.waiting.is_empty()
                && self.chat.turns.is_empty()
                && self.desk.ui.history.is_none();
            if new && self.chat.unconnected() {
                self.desk.ui.connect = Some(Connect::default());
            }
        }
        let ui = &mut self.desk.ui;
        // Once none waits, the next ones open on the message box, and go by themselves.
        if ui.waiting.is_empty() && ui.rushed.is_empty() && self.chat.steering().next().is_none() {
            (ui.queue.shut, ui.queue.paused) = (false, false);
        }
        if let Some(edit) = &mut self.edit {
            // Nothing else goes before the message that takes an earlier
            // one's place: the agent goes back first, once nothing runs.
            if !edit.asked && !self.chat.busy() {
                edit.asked = true;
                let (turn, to) = (edit.turn, edit.to.clone());
                self.chat.rewinding(turn);
                self.ask(REWIND, &json!({ "sessionId": session, "to": to }));
            }
        } else if let Some(command) = self.control.take_if(|_| !self.chat.busy()) {
            // The user did not write it: it shows no message.
            self.ask(
                "session/prompt",
                &json!({
                    "sessionId": session,
                    "prompt": [{ "type": "text", "text": command }],
                    "_meta": chat::quiet(),
                }),
            );
        } else if !self.chat.busy() {
            // Nothing runs: what was for the turn goes first, as a prompt;
            // then the first that waits, unless it is being changed.
            if let Some(text) = self.start.first.take() {
                let images = std::mem::take(&mut self.start.images);
                let images = images.into_iter().map(|image| Attached {
                    name: UNNAMED.to_string(),
                    mime: image.mime,
                    data: image.data.into(),
                });
                let first = Message {
                    text,
                    images: images.collect(),
                };
                self.prompt(&session, &first);
            } else if !ui.rushed.is_empty() {
                let next = ui.rushed.remove(0);
                self.prompt(&session, &next);
            } else if !ui.queue.paused && ui.waiting.first().is_some_and(|waiting| !waiting.held) {
                let next = ui.waiting.remove(0).message;
                self.prompt(&session, &next);
            }
        } else if let Some(turn) = self.chat.joinable() {
            // Into the running turn, once the agent said it started: it
            // reads them before its next step.
            for message in std::mem::take(&mut ui.rushed) {
                let mut params = self.blocks(&message);
                params["sessionId"] = json!(session);
                params["turn"] = json!(turn);
                self.ask(STEER, &params);
            }
        }
        let (names, commands) = self.chat.wanted(now, &self.desk.ui.folds);
        for name in names {
            self.ask(
                "_unharnessed/kernel/preview",
                &json!({ "sessionId": session, "name": name }),
            );
        }
        for id in commands {
            self.ask(
                "_unharnessed/kernel/tail",
                &json!({ "sessionId": session, "id": id, "lines": TAIL_LINES }),
            );
        }
    }

    /// What goes to the agent for a message, as a request's parameters:
    /// its blocks (its text, a command under the name the agent knows it
    /// by; a link for each of its `@` mentions that names something in the
    /// folder; its images) and, beside them, its images' names.
    fn blocks(&self, message: &Message) -> Value {
        let commands = self.desk.ui.commands.as_deref();
        let text = &message.text;
        let text = &commands.map_or_else(|| text.clone(), |known| slash::sent(text, known));
        let mut blocks = vec![json!({ "type": "text", "text": text })];
        let links = mention::links(Path::new(&self.start.cwd), text);
        blocks.extend(
            (links.iter())
                .map(|(uri, name)| json!({ "type": "resource_link", "uri": uri, "name": name })),
        );
        blocks.extend(
            message.images.iter().map(
                |image| json!({ "type": "image", "mimeType": image.mime, "data": &*image.data }),
            ),
        );
        let mut params = json!({ "prompt": blocks });
        if !message.images.is_empty() {
            params["_meta"] = chat::named(&message.images);
        }
        params
    }

    fn prompt(&mut self, session: &str, message: &Message) {
        let mut params = self.blocks(message);
        params["sessionId"] = json!(session);
        self.ask("session/prompt", &params);
        // The conversation follows what comes.
        self.desk.ui.top = None;
    }

    /// Enter: the message goes, or waits its turn while the agent works;
    /// `now`, it goes into the turn that runs. On an empty message box,
    /// the first message that waits goes into it. What names something of
    /// the screen's own does that instead. Answers whether the session
    /// goes on.
    fn submit(&mut self, now: bool) -> bool {
        let command = self.desk.ui.draft.text().trim().to_string();
        let own = matches!(
            command.as_str(),
            "/quit"
                | "/exit"
                | "/login"
                | "/new"
                | "/resume"
                | "/history"
                | "/model"
                | "/effort"
                | "/settings"
        );
        // Without an agent there is no one to send a message to: it stays written.
        if self.stage == Stage::Over && !own {
            return true;
        }
        let text = self.desk.ui.draft.take();
        let text = text.trim();
        match text {
            "/quit" | "/exit" => return false,
            "/login" => self.connect(),
            "/new" => self.new_chat(),
            "/resume" | "/history" => self.history(),
            "/model" => self.models(),
            "/effort" => self.effort(),
            "/settings" => self.settings(),
            _ => {}
        }
        if own {
            return true;
        }
        let ui = &mut self.desk.ui;
        if text.is_empty() && ui.images.is_empty() {
            if self.chat.busy() && !ui.waiting.is_empty() && ui.editing.is_none() {
                ui.rushed.push(ui.waiting.remove(0).message);
            }
            return true;
        }
        ui.past.keep(text);
        let message = Message {
            text: text.to_string(),
            images: std::mem::take(&mut ui.images),
        };
        ui.said = None;
        // It takes the place of a message that waits, in the line.
        if let Some(Edited::Waiting(id)) = ui.editing.as_ref().map(|editing| editing.of) {
            self.desk.keep();
            let ui = &mut self.desk.ui;
            match ui.waiting.iter_mut().find(|waiting| waiting.id == id) {
                Some(waiting) => waiting.message = message,
                None => ui.wait(message),
            }
            return true;
        }
        // It takes an earlier message's place: the agent goes back to
        // before that one first, and what was being written comes back.
        if let Some(turn) = ui.editing.as_ref().and_then(Editing::turn) {
            self.desk.keep();
            let to = (self.chat.turns.get(turn)).and_then(|turn| turn.rewind.clone());
            if let Some(to) = to {
                self.edit = Some(Edit {
                    turn,
                    to,
                    message,
                    asked: false,
                });
                self.desk.ui.top = None;
                return true;
            }
        }
        let ui = &mut self.desk.ui;
        // While the ones that wait are paused, a message sent goes at once.
        if now || (ui.queue.paused && !self.chat.busy()) {
            ui.rushed.push(message);
        } else {
            ui.wait(message);
        }
        ui.top = None;
        true
    }

    /// The messages that waited come back to the message box, before what
    /// is written there: first the ones the running turn had not taken.
    fn unqueue(&mut self) {
        // A message that waits being changed: it is as the box has it.
        let held = (self.desk.ui.waiting.iter()).position(|waiting| waiting.held);
        if let Some(at) = held {
            if let Some(changed) = self.desk.keep() {
                if changed.text.trim().is_empty() && changed.images.is_empty() {
                    self.desk.ui.waiting.remove(at);
                } else {
                    self.desk.ui.waiting[at].message = changed;
                }
            }
        }
        let ui = &mut self.desk.ui;
        ui.queue.paused = false;
        let mut parts = self.chat.unsteer();
        // The message that waited to take an earlier one's place comes back as one more.
        parts.extend(self.edit.take().map(|edit| edit.message));
        parts.append(&mut ui.rushed);
        parts.extend(ui.waiting.drain(..).map(|waiting| waiting.message));
        if parts.is_empty() {
            return;
        }
        let mut texts = Vec::new();
        let mut images = Vec::new();
        for message in parts {
            texts.extend((!message.text.is_empty()).then_some(message.text));
            images.extend(message.images);
        }
        let written = ui.draft.take();
        texts.extend((!written.is_empty()).then_some(written));
        ui.draft.insert(&texts.join("\n\n"));
        // Their images with them, before the ones of what is written.
        images.append(&mut ui.images);
        images.truncate(MOST);
        ui.images = images;
    }

    /// The running turn stops; what waited for its end no longer goes by
    /// itself: it waits on the message box, first what was sent into the
    /// turn and not taken, until one of it is sent or they play again.
    fn stop(&mut self) {
        self.pause();
        if let Some(session) = self.open().filter(|_| self.chat.busy()) {
            self.notify("session/cancel", &json!({ "sessionId": session }));
        }
    }

    /// What waited for the running turn, or was on its way into it, waits
    /// until the user sends it. The message that waited to take an earlier
    /// one's place comes back to the message box, before what is written.
    fn pause(&mut self) {
        let mut first = self.chat.unsteer();
        let ui = &mut self.desk.ui;
        first.append(&mut ui.rushed);
        ui.wait_first(first);
        ui.queue.paused = !ui.waiting.is_empty();
        if let Some(edit) = self.edit.take() {
            let ui = &mut self.desk.ui;
            let written = ui.draft.take();
            let texts = [edit.message.text, written];
            let texts: Vec<_> = texts.into_iter().filter(|text| !text.is_empty()).collect();
            ui.draft.insert(&texts.join("\n\n"));
            let mut images = edit.message.images;
            images.append(&mut ui.images);
            images.truncate(MOST);
            ui.images = images;
        }
    }

    /// Esc, or the mark at the box's right: the running turn stops. While
    /// a goal is active the agent would go on by itself: the goal waits
    /// too (`/goal pause`, once the turn has stopped).
    fn halt(&mut self) {
        if !self.chat.goal.as_ref().is_some_and(Goal::going) {
            return self.stop();
        }
        if self.goal_acts("/goal pause") {
            self.stop();
        }
    }

    /// A goal that waits goes on (`/goal resume`), once nothing runs.
    fn resume_goal(&mut self) {
        if self.chat.goal.as_ref().is_some_and(Goal::waits) {
            self.goal_acts("/goal resume");
        }
    }

    /// The goal goes: the running turn stops, then `/goal clear`. One
    /// reached or failed just leaves the box.
    fn end_goal(&mut self) {
        if self.chat.goal.as_ref().is_some_and(Goal::over) {
            self.chat.read_goal();
        } else if self.goal_acts("/goal clear") {
            self.stop();
        }
    }

    /// A button of the goal's sends `command`, first in line; pressed
    /// again before the goal moved, it does not act twice. Answers
    /// whether it acted.
    fn goal_acts(&mut self, command: &'static str) -> bool {
        let Some(status) = self.chat.goal.as_ref().map(|goal| goal.status) else {
            return false;
        };
        let now = self.now();
        let pressed = (now, status, command);
        let again = |(at, stood, sent): (f64, Status, &str)| {
            sent == command && stood == status && now - at < GOAL_WAIT
        };
        if self.open().is_none() || self.pressed.is_some_and(again) {
            return false;
        }
        self.pressed = Some(pressed);
        self.control = Some(command);
        true
    }

    /// A key; answers whether the session goes on. Enter sends (with
    /// Ctrl, into the running turn), Esc stops the agent; Ctrl+C empties
    /// the message box, else stops the agent, else leaves, as Ctrl+D does
    /// on an empty box and Ctrl+Q always. While a menu of the files or of
    /// the commands is open, the arrows, Enter, Tab and Esc are its own.
    fn key(&mut self, key: KeyEvent) -> bool {
        if self.desk.ui.connect.is_some() {
            return self.page_key(key);
        }
        if self.desk.ui.prefs.is_some() {
            return self.prefs_key(key);
        }
        if self.desk.ui.history.is_some() {
            return self.history_key(key);
        }
        if self.desk.ui.pick.is_some() {
            return self.pick_key(key);
        }
        let offered = self.desk.ui.menu.is_some();
        let goes_on = self.desk.menu_key(key) || self.box_key(key);
        self.desk.suggest();
        // Each time the menu opens, on the folder as it now is.
        if !offered && self.desk.ui.menu.is_some() {
            self.list();
        }
        goes_on
    }

    /// A key for the message box and the conversation.
    fn box_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT | KeyModifiers::CONTROL);
        let ui = &self.desk.ui;
        let working = self.chat.busy()
            || !ui.waiting.is_empty()
            || !ui.rushed.is_empty()
            || self.edit.is_some();
        let (editing, empty) = (ui.editing.is_some(), ui.draft.is_empty());
        let shift = key.modifiers == KeyModifiers::SHIFT;
        match key.code {
            KeyCode::Char('q') if ctrl => return false,
            KeyCode::Char('d') if ctrl && empty && ui.images.is_empty() => return false,
            // The earlier message is left as it was, before anything else.
            KeyCode::Esc | KeyCode::Char('c') if editing && (key.code == KeyCode::Esc || ctrl) => {
                self.desk.keep();
            }
            KeyCode::Char('c') if ctrl => {
                if !empty || !ui.images.is_empty() {
                    self.desk.ui.draft.take();
                    self.desk.ui.images.clear();
                } else if working {
                    self.halt();
                } else {
                    return false;
                }
            }
            KeyCode::Esc => self.halt(),
            // The messages sent before, one by one, to change one.
            KeyCode::Up if shift => self.edit_step(-1),
            KeyCode::Down if shift => self.edit_step(1),
            KeyCode::Char('v') if ctrl || key.modifiers.contains(KeyModifiers::SUPER) => {
                self.clip(None);
            }
            KeyCode::Char('s') if ctrl => self.settings(),
            // On an empty message, the last image goes.
            KeyCode::Backspace if empty && !ui.images.is_empty() => {
                self.desk.ui.images.pop();
            }
            KeyCode::Enter if plain => return self.submit(false),
            KeyCode::Enter if key.modifiers == KeyModifiers::CONTROL => return self.submit(true),
            KeyCode::Char('o') if ctrl => self.desk.ui.folds.toggle_all(),
            KeyCode::Char('k') if ctrl => self.desk.toggle_panel(),
            KeyCode::Char('n') if ctrl => self.new_chat(),
            KeyCode::Char('r') if ctrl => self.history(),
            KeyCode::Char('p') if ctrl => self.models(),
            KeyCode::Char('t') if ctrl => self.effort(),
            _ => {
                self.desk.write(key);
            }
        }
        true
    }

    /// The message of turn `turn` comes back to the message box to be
    /// changed, when the agent can go back to before it.
    fn edit(&mut self, turn: usize) {
        if !self.desk.ui.editable {
            return;
        }
        let found = (self.chat.turns.get(turn)).filter(|turn| turn.rewind.is_some());
        let Some((text, images)) =
            found.and_then(|turn| Some((turn.prompt.clone()?, &turn.images)))
        else {
            return;
        };
        let written = Message {
            text,
            images: images.clone(),
        };
        self.desk.edit(Edited::Turn(turn), written);
    }

    /// The message that waits numbered `id` comes back to the message box
    /// to be changed; the line waits at it.
    fn hold(&mut self, id: u64) {
        let found = (self.desk.ui.waiting.iter()).find(|waiting| waiting.id == id);
        if let Some(written) = found.map(|waiting| waiting.message.clone()) {
            self.desk.edit(Edited::Waiting(id), written);
        }
    }

    /// The message that waits numbered `id` goes into the running turn
    /// now, or first once nothing runs.
    fn rush(&mut self, id: u64) {
        let ui = &mut self.desk.ui;
        let at = (ui.waiting.iter()).position(|waiting| waiting.id == id && !waiting.held);
        if let Some(at) = at {
            let message = ui.waiting.remove(at).message;
            ui.rushed.push(message);
        }
    }

    /// From the message being changed to the one sent before it (`by`
    /// -1) or after it (1), among those the agent can go back to; with
    /// none being changed, to the last one; past the last, the message
    /// box is as it was.
    fn edit_step(&mut self, by: isize) {
        let can = |turn: &crate::chat::Turn| turn.prompt.is_some() && turn.rewind.is_some();
        let turns: Vec<usize> = (self.chat.turns.iter().enumerate())
            .filter(|(_, turn)| can(turn))
            .map(|(at, _)| at)
            .collect();
        let on = (self.desk.ui.editing.as_ref())
            .and_then(Editing::turn)
            .and_then(|editing| turns.iter().position(|turn| *turn == editing));
        let to = match (on, by < 0) {
            (None, true) => turns.len().checked_sub(1),
            (None, false) => None,
            (Some(on), true) => Some(on.saturating_sub(1)),
            (Some(on), false) => Some(on + 1),
        };
        match to.and_then(|to| turns.get(to)) {
            Some(turn) => self.edit(*turn),
            None => {
                self.desk.keep();
            }
        }
    }

    /// The images at `paths`, or what was read of the clipboard, join the
    /// message: as many as it still takes, each refused one said with why.
    fn attach(&mut self, found: Vec<Result<Attached, String>>) {
        let settings = &self.chat.settings;
        let ui = &mut self.desk.ui;
        let mut said = None;
        if settings.on("images.blockImages") {
            said = Some(
                "Images are blocked: turn off “Block images” in the settings to send them."
                    .to_string(),
            );
        } else {
            for image in found {
                match image {
                    Ok(_) if ui.images.len() >= MOST => {
                        said = Some(format!("A message carries {MOST} images at most."));
                    }
                    Ok(image) => ui.images.push(image),
                    Err(why) => said = Some(why),
                }
            }
        }
        ui.said = said.map(|said| (ui.wall, said));
    }

    /// Whether the agent expects large images shrunk: it refuses one past
    /// its limit then. Until it said, it does.
    fn shrinks(&self) -> bool {
        self.chat.settings.view.is_none() || self.chat.settings.on("images.autoResize")
    }

    /// Ctrl+V, Cmd+V: the image on the clipboard joins the message. The
    /// system's tool is asked on a thread of its own: it takes a moment.
    /// `asked`: what was pasted, a file's name: the file joins it when the
    /// clipboard holds it, else the name is written.
    fn clip(&mut self, asked: Option<String>) {
        if std::mem::replace(&mut self.reading.clipboard, true) {
            if let Some(text) = asked {
                self.desk.paste(&text);
            }
            return;
        }
        let tell = self.tell.clone();
        std::thread::spawn(move || {
            let _ = tell.send(Came::Clip(clipboard::image(), asked));
        });
    }

    /// The clipboard was read.
    fn clipped(&mut self, read: Result<Clip, String>, asked: Option<String>) {
        self.reading.clipboard = false;
        let shrinks = self.shrinks();
        let image = match (read, asked) {
            (Ok(Clip::File(path)), Some(text))
                if path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy() == text) =>
            {
                message::read(&path, shrinks)
            }
            (_, Some(text)) => {
                self.desk.paste(&text);
                return;
            }
            (Ok(Clip::File(path)), None) => message::read(&path, shrinks),
            (Ok(Clip::Image(bytes)), None) => {
                message::attach(&message::unnamed(&self.desk.ui.images), &bytes, shrinks)
            }
            (Err(why), None) => Err(why),
        };
        self.attach(vec![image]);
    }

    /// The settings' screen opens, on what the agent now says of them.
    fn settings(&mut self) {
        if !self.asks() {
            return;
        }
        self.leave();
        (self.desk.ui.history, self.desk.ui.pick) = (None, None);
        self.desk.ui.prefs = Some(Prefs::on(Tab::General));
        self.ask(providers::LIST, &json!({}));
        self.read_settings();
    }

    /// What the settings' screen asked for.
    fn wishes(&mut self, wishes: Vec<Wish>) {
        for wish in wishes {
            match wish {
                Wish::Ask(method, params) => {
                    if self.asks() {
                        self.ask(method, &params);
                    }
                }
                Wish::SignIn(server) => {
                    // Its screen takes over until it is done.
                    if self.asks() && !self.chat.providers.signing_in() {
                        self.serving = true;
                        self.desk.ui.connect = Some(Connect::default());
                        let provider = format!("mcp:{server}");
                        self.ask(providers::LOGIN, &json!({ "providerId": provider }));
                    }
                }
                Wish::Restart => self.restart(),
                Wish::Leave => self.desk.ui.prefs = None,
            }
        }
        self.follow();
    }

    /// The agent restarts on its conversation (a new one, when nothing was
    /// asked in it yet): what it reads when it starts applies. The
    /// settings stay on the screen.
    fn restart(&mut self) {
        let mut prefs = self.desk.ui.prefs.take();
        if let Some(prefs) = &mut prefs {
            prefs.restarted();
        }
        let saved = (self.chat.saved.clone()).filter(|(_, file)| Path::new(file).exists());
        self.turn_to(saved);
        self.desk.ui.prefs = prefs;
    }

    /// A key on the settings' screen; answers whether the session goes on.
    fn prefs_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.desk.page();
        let Some(prefs) = &mut self.desk.ui.prefs else {
            return true;
        };
        match key.code {
            KeyCode::Char('q') if ctrl => return false,
            // What opened the screen closes it.
            KeyCode::Char('s') if ctrl => self.desk.ui.prefs = None,
            KeyCode::PageUp => self.desk.scroll(-page),
            KeyCode::PageDown => self.desk.scroll(page),
            _ => {
                let wishes = prefs.key(key, &self.chat);
                self.wishes(wishes);
            }
        }
        true
    }

    /// An agent starts with `args`: the session's, from now on.
    fn launch(&mut self, args: Vec<String>) {
        self.run += 1;
        let (run, tell) = (self.run, self.tell.clone());
        let launch = Launch {
            args,
            ..self.start.launch.clone()
        };
        let agent = Agent::start(&launch, move |told| {
            let _ = tell.send(Came::Agent(run, told));
        });
        match agent {
            Ok(agent) => {
                self.agent = Some(agent);
                self.begin();
            }
            Err(error) => {
                let now = self.now();
                let how = format!("The agent could not start: {error}");
                self.chat.stopped(now, &how, &[]);
                self.stage = Stage::Over;
            }
        }
    }

    /// The session's agent is let go of: its input closes, it saves its
    /// kernel and exits, on a thread of its own; until then it holds its
    /// conversation.
    fn let_go(&mut self) {
        let Some(agent) = self.agent.take() else {
            return;
        };
        let held = (self.chat.saved.as_ref()).map(|(id, _)| id.clone());
        let (run, tell) = (self.run, self.tell.clone());
        let thread = std::thread::spawn(move || {
            let _ = agent.stop(EXIT_WAIT);
            let _ = tell.send(Came::Left(run));
        });
        self.leaving.push(Leaving { run, held, thread });
    }

    /// An agent let go of has exited: the conversation that waited for it starts.
    fn left(&mut self, run: u64) {
        let Some(at) = self.leaving.iter().position(|leaving| leaving.run == run) else {
            return;
        };
        let _ = self.leaving.remove(at).thread.join();
        let held =
            |id: &String| (self.leaving.iter()).any(|leaving| leaving.held.as_ref() == Some(id));
        if self.next.as_ref().is_some_and(|(id, _)| !held(id)) {
            if let Some((_, args)) = self.next.take() {
                self.launch(args);
            }
        }
    }

    /// The session leaves its conversation for another: a new one, or the
    /// saved one of that id and file. Its agent is let go of and another
    /// starts; what waited to be sent comes back to the message box, and
    /// what is written there stays.
    fn turn_to(&mut self, saved: Option<(String, String)>) {
        self.unqueue();
        self.let_go();
        self.leave();
        self.desk.fresh();
        self.chat = Chat::default();
        self.said.clear();
        (self.next, self.owed) = (None, false);
        (self.control, self.pressed) = (None, None);
        (self.edit, self.serving) = (None, false);
        self.start.first = None;
        self.start.images.clear();
        self.start.resumes = saved.is_some();
        let Some((id, file)) = saved else {
            return self.launch(self.start.fresh.clone());
        };
        let mut args = self.start.saved.clone();
        args.push(format!("--resume={file}"));
        // The agent that leaves it holds its file until it has exited.
        if (self.leaving.iter()).any(|leaving| leaving.held.as_ref() == Some(&id)) {
            self.stage = Stage::Waiting;
            self.next = Some((id, args));
            let now = self.now();
            (self.chat).told(now, "Waiting for this conversation's kernel to be saved…");
        } else {
            self.launch(args);
        }
    }

    /// A new conversation, unless this one is: nothing was asked in it yet.
    fn new_chat(&mut self) {
        let new = self.chat.turns.is_empty() && !self.chat.busy() && self.stage != Stage::Over;
        if new {
            self.leave();
            (self.desk.ui.history, self.desk.ui.pick) = (None, None);
        } else {
            self.turn_to(None);
        }
    }

    /// The saved conversations' screen opens, on what the agent now lists
    /// of them; an agent that ended is replaced first, on a new conversation.
    fn history(&mut self) {
        if self.stage == Stage::Over {
            self.turn_to(None);
        }
        self.leave();
        self.desk.ui.pick = None;
        self.desk.ui.history = Some(History::default());
        self.relist();
    }

    /// The folder's saved conversations are asked for, now or once the session is open.
    fn relist(&mut self) {
        if self.open().is_some() {
            self.ask("session/list", &json!({ "cwd": self.start.cwd }));
        } else {
            self.owed = true;
        }
    }

    /// A saved conversation takes the screen; the one that is there already stays.
    fn resume(&mut self, id: &str) {
        let saved = (self.chat.history.iter().flatten()).find(|chat| chat.id == id);
        let Some(saved) = saved.cloned() else {
            return;
        };
        if saved.current {
            self.desk.ui.history = None;
        } else {
            self.turn_to(Some((saved.id, saved.path)));
        }
    }

    /// What deletes a saved conversation: its row asks once more, then it is.
    fn forget(&mut self, id: &str) {
        let Some(history) = &mut self.desk.ui.history else {
            return;
        };
        if history.asking.as_deref() == Some(id) {
            history.asking = None;
            self.ask(history::DELETE, &json!({ "sessionId": id }));
        } else {
            history.ask(id);
        }
    }

    /// A key on the saved conversations' screen; answers whether the
    /// session goes on. The arrows choose a row, Enter opens it, Delete
    /// asks to delete it and Enter then does; Esc steps back; what is typed
    /// filters the rows.
    fn history_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.desk.page();
        let saved = self.chat.history.as_deref().unwrap_or_default();
        let Some(history) = &mut self.desk.ui.history else {
            return true;
        };
        let on = history
            .on(saved)
            .map(|chat| (chat.id.clone(), chat.current));
        // Where Delete has nothing of the filter to delete, it is the row's.
        let ended = history.query.caret() == history.query.text().len();
        match key.code {
            KeyCode::Char('q') if ctrl => return false,
            // What opened the screen closes it.
            KeyCode::Char('r') if ctrl => self.desk.ui.history = None,
            KeyCode::Esc | KeyCode::Char('c') if key.code == KeyCode::Esc || ctrl => {
                if history.asking.take().is_some() {
                } else if !history.query.is_empty() {
                    history.query.take();
                    history.searched();
                } else {
                    self.desk.ui.history = None;
                }
            }
            KeyCode::Enter => match (history.asking.clone(), on) {
                (Some(id), _) => self.forget(&id),
                (None, Some((id, _))) => self.resume(&id),
                (None, None) => {}
            },
            KeyCode::Delete if ended => {
                if let Some((id, false)) = on {
                    self.forget(&id);
                }
            }
            KeyCode::Up => history.step(-1, saved),
            KeyCode::Down => history.step(1, saved),
            KeyCode::PageUp => self.desk.scroll(-page),
            KeyCode::PageDown => self.desk.scroll(page),
            _ => {
                let was = history.query.text().to_string();
                desk::edit(&mut history.query, key, false);
                if history.query.text() != was {
                    history.searched();
                }
            }
        }
        true
    }

    /// The models' menu opens over the message box, or closes.
    fn models(&mut self) {
        let open = matches!(self.desk.ui.pick, Some(Pick::Models(_)));
        let can = self.open().is_some() && !self.chat.models.is_empty();
        self.desk.ui.pick = (!open && can).then(|| Pick::Models(Models::on(&self.chat)));
    }

    /// The reasoning effort's menu opens over the message box, or closes:
    /// a model with one level has nothing to choose.
    fn effort(&mut self) {
        let open = matches!(self.desk.ui.pick, Some(Pick::Effort));
        let can = self.open().is_some() && self.chat.efforts.len() > 1;
        self.desk.ui.pick = (!open && can).then_some(Pick::Effort);
        // Opening, its track fills from nothing.
        if self.desk.ui.pick.is_some() {
            self.desk.ui.slid = Some((self.desk.ui.wall, None));
        }
    }

    /// An option of the session is set: the agent answers what they all are then.
    fn set(&mut self, option: &str, value: &str) {
        if let Some(session) = self.open() {
            let params = json!({ "sessionId": session, "configId": option, "value": value });
            self.ask("session/set_config_option", &params);
        }
    }

    /// The model at place `at` of the menu becomes the session's; the menu closes.
    fn choose(&mut self, at: usize) {
        let Some(Pick::Models(models)) = &self.desk.ui.pick else {
            return;
        };
        let value = (models.shown(&self.chat).get(at)).map(|model| model.value.clone());
        self.desk.ui.pick = None;
        if let Some(value) = value.filter(|value| Some(value) != self.chat.chosen.as_ref()) {
            self.set("model", &value);
        }
    }

    /// The reasoning effort goes to the level at place `at` of the model's.
    fn level(&mut self, at: usize) {
        let level = self.chat.efforts.get(at).cloned();
        if let Some(level) = level.filter(|level| Some(level) != self.chat.effort.as_ref()) {
            let from = (self.chat.effort.as_ref())
                .and_then(|current| self.chat.efforts.iter().position(|level| level == current));
            self.desk.ui.slid = Some((self.desk.ui.wall, from));
            self.set("thought_level", &level);
        }
    }

    /// A key while a menu is open over the message box; answers whether
    /// the session goes on. On the models: the arrows choose one, Enter
    /// picks it, what is typed searches them. On the effort: the arrows
    /// set it. Esc closes the menu.
    fn pick_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let efforts = self.chat.efforts.len();
        let level = (self.chat.effort.as_ref())
            .and_then(|current| self.chat.efforts.iter().position(|level| level == current));
        let Some(pick) = &mut self.desk.ui.pick else {
            return true;
        };
        match (pick, key.code) {
            (_, KeyCode::Char('q')) if ctrl => return false,
            (_, KeyCode::Esc) => self.desk.ui.pick = None,
            (Pick::Models(models), code) => {
                let count = models.shown(&self.chat).len();
                let page = pick::ROWS.cast_signed() - 1;
                match code {
                    KeyCode::Char('c') if ctrl && models.query.is_empty() => {
                        self.desk.ui.pick = None;
                    }
                    KeyCode::Char('c') if ctrl => {
                        models.query.take();
                        models.searched();
                    }
                    KeyCode::Char('p') if ctrl => self.desk.ui.pick = None,
                    KeyCode::Enter => {
                        let at = models.at;
                        self.choose(at);
                    }
                    KeyCode::Up => models.step(-1, count),
                    KeyCode::Down => models.step(1, count),
                    KeyCode::PageUp => models.step(-page.min(models.at.cast_signed()), count),
                    KeyCode::PageDown => {
                        let left = count.saturating_sub(models.at + 1).cast_signed();
                        models.step(page.min(left), count);
                    }
                    _ => {
                        let was = models.query.text().to_string();
                        desk::edit(&mut models.query, key, false);
                        if models.query.text() != was {
                            models.searched();
                        }
                    }
                }
            }
            (Pick::Effort, code) => {
                let to = match code {
                    KeyCode::Left | KeyCode::Down => level.map(|level| level.saturating_sub(1)),
                    KeyCode::Right | KeyCode::Up => level.map(|level| level + 1),
                    KeyCode::Home => Some(0),
                    KeyCode::End => Some(efforts),
                    _ => None,
                };
                match (to, code) {
                    (Some(to), _) => self.level(to.min(efforts.saturating_sub(1))),
                    (None, KeyCode::Enter) => self.desk.ui.pick = None,
                    (None, KeyCode::Char('c' | 't')) if ctrl => self.desk.ui.pick = None,
                    // What is typed is for the message: the menu closes on it.
                    (None, KeyCode::Char(_)) if !ctrl => {
                        self.desk.ui.pick = None;
                        return self.key(key);
                    }
                    _ => {}
                }
            }
        }
        true
    }

    /// The providers' screen opens, on what the agent now says of them.
    fn connect(&mut self) {
        if self.stage == Stage::Over {
            return;
        }
        self.desk.ui.connect = Some(Connect::default());
        self.ask(providers::LIST, &json!({}));
    }

    /// The providers' screen gives the conversation its place back.
    fn leave(&mut self) {
        self.desk.ui.connect = None;
        self.desk.ui.over = None;
    }

    /// The providers' screen follows what the agent said of them.
    fn follow(&mut self) {
        // A sign-in to an MCP server is over: the settings come back.
        if self.serving && self.chat.providers.sign_in.is_none() {
            self.serving = false;
            self.desk.ui.connect = None;
        }
        if let Some(connect) = &mut self.desk.ui.connect {
            connect.follow(&self.chat.providers);
        }
        if let Some(prefs) = &mut self.desk.ui.prefs {
            prefs.follow(&self.chat);
        }
    }

    /// A step back on the providers' screen: out of a sign-in (it stops, or
    /// its failure is left), else out of the open form, else the filter
    /// empties, else the conversation comes back. `whole`: straight back to
    /// the conversation from the list.
    fn back(&mut self, whole: bool) {
        if let Some(sign_in) = &self.chat.providers.sign_in {
            if sign_in.error.is_some() {
                self.chat.providers.dismiss();
                self.follow();
            } else {
                self.notify(providers::LOGIN_CANCEL, &json!({}));
            }
            return;
        }
        let Some(connect) = &mut self.desk.ui.connect else {
            return;
        };
        if whole {
            self.leave();
        } else if connect.form.is_some() {
            connect.close();
        } else if !connect.query.is_empty() {
            connect.query.take();
        } else {
            self.leave();
        }
    }

    /// A key on the providers' screen; answers whether the session goes on.
    /// The arrows choose a row, Enter opens it or presses the form's
    /// button, Tab goes round the buttons, Esc steps back; what is typed
    /// goes to the open field, else filters the rows.
    fn page_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.desk.page();
        let providers = &self.chat.providers;
        let Some(connect) = &mut self.desk.ui.connect else {
            return true;
        };
        match key.code {
            KeyCode::Char('q') if ctrl => return false,
            KeyCode::Char('c') if ctrl => {
                match connect.field(providers).filter(|field| !field.is_empty()) {
                    Some(field) => {
                        field.take();
                    }
                    None => self.back(false),
                }
            }
            KeyCode::Esc => self.back(false),
            KeyCode::Enter => self.enter(),
            KeyCode::Tab => connect.tab(1, providers),
            KeyCode::BackTab => connect.tab(-1, providers),
            KeyCode::Up => connect.step(-1, providers),
            KeyCode::Down => connect.step(1, providers),
            KeyCode::PageUp => self.desk.scroll(-page),
            KeyCode::PageDown => self.desk.scroll(page),
            _ => {
                if let Some(field) = connect.field(providers) {
                    desk::edit(field, key, false);
                }
            }
        }
        true
    }

    /// Enter on the providers' screen: the button it is on; on a sign-in's
    /// teams, the chosen one; on the list, the chosen row, or the first of
    /// those the filter leaves.
    fn enter(&mut self) {
        let providers = &self.chat.providers;
        let Some(connect) = &self.desk.ui.connect else {
            return;
        };
        if (providers.sign_in.as_ref()).is_some_and(|sign_in| sign_in.teams.is_some()) {
            return self.team(connect.team);
        }
        let acts = connect::acts(providers, connect);
        if let Some(act) = acts.get(connect.focus.min(acts.len().saturating_sub(1))) {
            return self.act(*act);
        }
        let Some(list) = &providers.list else {
            return;
        };
        let query = connect.query.text();
        let first = (!query.trim().is_empty())
            .then(|| connect::groups(list, query).into_iter().next())
            .flatten()
            .and_then(|group| group.rows.first().map(|row| row.key.clone()));
        if let Some(key) = connect.chosen.clone().or(first) {
            self.activate(key);
        }
    }

    /// A row of the list opens: on its key, on the question before a
    /// sign-in that replaces a credential, or straight on its sign-in.
    /// Open already, it closes.
    fn activate(&mut self, key: Key) {
        let providers = &self.chat.providers;
        let (Some(row), Some(connect)) = (providers.row(&key), &mut self.desk.ui.connect) else {
            return;
        };
        let open = (connect.form.as_ref()).is_some_and(|form| form.row == key);
        if open {
            connect.close();
        } else if row.local.is_some() {
            connect.open(key, Mode::Leave, providers);
        } else if !row.browser {
            connect.open(key, Mode::Key, providers);
        } else if row.connected || providers.other(&key).is_some() {
            connect.open(key, Mode::SignIn, providers);
        } else {
            connect.chosen = Some(key.clone());
            self.sign_in(&key.id);
        }
    }

    /// A browser sign-in starts, unless one is under way.
    fn sign_in(&mut self, provider: &str) {
        if self.chat.providers.signing_in() {
            return;
        }
        if let Some(connect) = &mut self.desk.ui.connect {
            connect.close();
        }
        self.ask(providers::LOGIN, &json!({ "providerId": provider }));
    }

    /// A team of the sign-in's is chosen: the personal account is the first.
    fn team(&mut self, at: usize) {
        let Some(teams) =
            (self.chat.providers.sign_in.as_ref()).and_then(|sign_in| sign_in.teams.as_ref())
        else {
            return;
        };
        let team = match at.checked_sub(1) {
            None => Value::Null,
            Some(at) => match teams.list.get(at) {
                Some((id, ..)) => json!(id),
                None => return,
            },
        };
        let answer =
            json!({ "promptId": teams.prompt, "answer": { "kind": "team", "teamId": team } });
        self.ask(providers::LOGIN_ANSWER, &answer);
        self.follow();
    }

    /// A button of the providers' screen.
    fn act(&mut self, act: Act) {
        let providers = &self.chat.providers;
        let Some(connect) = &mut self.desk.ui.connect else {
            return;
        };
        let sign_in = providers.sign_in.as_ref();
        match act {
            Act::Cancel => connect.close(),
            Act::Leave => {
                if let Some(form) = &mut connect.form {
                    form.mode = Mode::Leave;
                    form.tried = false;
                }
                connect.focus = 0;
            }
            Act::Save | Act::Remove | Act::SignIn => {
                let Some(form) = &mut connect.form else {
                    return;
                };
                let id = form.row.id.clone();
                if act == Act::SignIn {
                    return self.sign_in(&id);
                }
                let key = form.field.text().trim().to_string();
                if providers.busy || (act == Act::Save && key.is_empty()) {
                    return;
                }
                form.tried = true;
                let local = (providers.row(&form.row)).is_some_and(|row| row.local.is_some());
                match (act, local) {
                    (Act::Save, _) => self.ask(
                        providers::SET_API_KEY,
                        &json!({ "providerId": id, "apiKey": key }),
                    ),
                    (_, true) => self.ask(providers::LOCAL_REMOVE, &json!({ "providerId": id })),
                    (_, false) => self.ask(providers::LOGOUT, &json!({ "providerId": id })),
                };
            }
            Act::Open | Act::Copy => {
                let Some(sign_in) = sign_in else {
                    return;
                };
                // The code goes with the page it is entered on.
                let text = match (connect::code(sign_in), act) {
                    (Some(code), _) => Some(code),
                    (None, Act::Copy) => sign_in.url.as_deref(),
                    (None, _) => None,
                };
                if let Some(text) = text {
                    copy(text);
                    connect.copied = Some(self.desk.ui.wall);
                }
                if act == Act::Open {
                    self.ask(providers::LOGIN_OPEN, &json!({}));
                }
            }
            Act::Other => {
                connect.other = true;
                let acts = connect::acts(providers, connect);
                connect.focus = (acts.iter())
                    .position(|act| *act == Act::Continue)
                    .unwrap_or(0);
            }
            Act::Continue => {
                let Some(prompt) = sign_in.and_then(|sign_in| connect::asking(sign_in, connect))
                else {
                    return;
                };
                let value = connect.answer.text().trim().to_string();
                if value.is_empty() && !prompt.allow_empty {
                    return;
                }
                let answer =
                    json!({ "promptId": prompt.id, "answer": { "kind": "text", "value": value } });
                self.ask(providers::LOGIN_ANSWER, &answer);
                self.follow();
            }
            Act::Keep => {
                let Some(teams) = sign_in.and_then(|sign_in| sign_in.teams.as_ref()) else {
                    return;
                };
                let answer = json!({ "promptId": teams.prompt, "answer": { "kind": "dismiss" } });
                self.ask(providers::LOGIN_ANSWER, &answer);
                self.follow();
            }
            Act::Stop => {
                if providers.signing_in() {
                    self.notify(providers::LOGIN_CANCEL, &json!({}));
                }
            }
            Act::Retry => {
                let Some(provider) = sign_in.map(|sign_in| sign_in.provider.clone()) else {
                    return;
                };
                self.chat.providers.dismiss();
                self.sign_in(&provider);
            }
            Act::Close => {
                self.chat.providers.dismiss();
                self.follow();
            }
        }
    }

    /// A click on what acts: a row or a button of a screen, what leads to
    /// one, a row of a menu. Answers whether the session goes on: a click
    /// on `/quit` leaves.
    fn spot(&mut self, spot: Spot) -> bool {
        match spot {
            Spot::Connect => self.connect(),
            Spot::Back if self.desk.ui.connect.is_some() => self.back(true),
            Spot::Back if self.desk.ui.prefs.is_some() => self.desk.ui.prefs = None,
            Spot::Back => self.desk.ui.history = None,
            Spot::Settings => self.settings(),
            Spot::Tab(_)
            | Spot::Pref(_)
            | Spot::Flip(_)
            | Spot::Drop(_)
            | Spot::Stop(_)
            | Spot::Opt(..)
            | Spot::Entry(_) => {
                if let Some(prefs) = &mut self.desk.ui.prefs {
                    let wishes = prefs.click(&spot, &self.chat);
                    self.wishes(wishes);
                }
            }
            Spot::Detach(at) => {
                let images = &mut self.desk.ui.images;
                if at < images.len() {
                    images.remove(at);
                }
            }
            Spot::Keep => {
                self.desk.keep();
            }
            Spot::Queue => self.desk.ui.toggle_queue(),
            Spot::Resume => self.desk.ui.queue.paused = false,
            Spot::Waiting(id) | Spot::Hold(id) => self.hold(id),
            Spot::Rush(id) => self.rush(id),
            Spot::Unqueue(id) => {
                let ui = &mut self.desk.ui;
                ui.waiting
                    .retain(|waiting| waiting.id != id || waiting.held);
            }
            Spot::Model => self.models(),
            Spot::Effort => self.effort(),
            Spot::Choice(at) => self.choose(at),
            Spot::Level(at) => self.level(at),
            Spot::New => self.new_chat(),
            Spot::History => self.history(),
            Spot::Chat(id) => self.resume(&id),
            Spot::Forget(id) => self.forget(&id),
            Spot::Row(key) => self.activate(key),
            Spot::Leave(key) => {
                let providers = &self.chat.providers;
                let Some(connect) = &mut self.desk.ui.connect else {
                    return true;
                };
                let open = (connect.form.as_ref())
                    .is_some_and(|form| form.row == key && form.mode == Mode::Leave);
                if open {
                    connect.close();
                } else {
                    connect.open(key, Mode::Leave, providers);
                }
            }
            Spot::Do(act) => self.act(act),
            Spot::Team(at) => self.team(at),
            Spot::Pick(at) => {
                if self.desk.pick(at) {
                    return self.submit(false);
                }
            }
            Spot::Kill(id) => {
                if let Some(session) = self.open() {
                    self.ask(KILL, &json!({ "sessionId": session, "id": id }));
                }
            }
            Spot::Port(port) => (self.start.open)(&format!("http://localhost:{port}/")),
            // The desk opens and closes the panel itself, in a replay too.
            Spot::Panel => self.desk.toggle_panel(),
            Spot::Play => self.resume_goal(),
            Spot::EndGoal => self.end_goal(),
        }
        true
    }

    /// Something came; answers whether the session goes on.
    fn take(&mut self, came: Came) -> bool {
        self.stirred = self.now();
        match came {
            Came::Screen(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                return self.key(key)
            }
            Came::Screen(Event::Mouse(mouse)) => return self.mouse(mouse),
            Came::Screen(Event::Paste(text)) => self.paste(&text),
            Came::Screen(_) => {}
            // What an agent let go of still says is no longer the session's.
            Came::Agent(run, _) if run != self.run => {}
            Came::Agent(_, Told::Frame(frame)) => self.frame(&frame),
            Came::Agent(_, Told::Said(line)) => {
                self.said.push(line);
                let extra = self.said.len().saturating_sub(SAID);
                self.said.drain(..extra);
            }
            Came::Agent(_, Told::Closed) => return self.closed(),
            Came::Left(run) => self.left(run),
            Came::Files(files) => self.listed(files),
            Came::Clip(read, asked) => self.clipped(read, asked),
            Came::Gone => return false,
        }
        true
    }

    /// The mouse: over the models' menu the wheel moves its rows; a click
    /// beside an open menu closes it. Answers whether the session goes on.
    fn mouse(&mut self, mouse: MouseEvent) -> bool {
        if let Some(Pick::Models(models)) = &mut self.desk.ui.pick {
            let by = match mouse.kind {
                MouseEventKind::ScrollUp => -3,
                MouseEventKind::ScrollDown => 3,
                _ => 0,
            };
            if by != 0 {
                let count = models.shown(&self.chat).len();
                models.roll(by, count);
                return true;
            }
        }
        // Over a form's list of models, the wheel moves its rows.
        let ui = &mut self.desk.ui;
        if let (Some(prefs), Some(Spot::Entry(_))) = (&mut ui.prefs, &ui.over) {
            let by = match mouse.kind {
                MouseEventKind::ScrollUp => -3,
                MouseEventKind::ScrollDown => 3,
                _ => 0,
            };
            if by != 0 && prefs.roll(by, &self.chat) {
                return true;
            }
        }
        match self.desk.mouse(mouse) {
            Some(Click::Stop) => self.halt(),
            Some(Click::Spot(spot)) => return self.spot(spot),
            Some(Click::Away) => self.desk.ui.pick = None,
            Some(Click::Edit(turn)) => self.edit(turn),
            Some(Click::Fold(_)) | None => {}
        }
        true
    }

    /// What was pasted goes where what is typed does. A field is one
    /// line: a key copied with its line's end comes without it.
    fn paste(&mut self, text: &str) {
        let line = text.replace(['\r', '\n'], "");
        let ui = &mut self.desk.ui;
        if let Some(connect) = &mut ui.connect {
            if let Some(field) = connect.field(&self.chat.providers) {
                field.insert(&line);
            }
        } else if let Some(prefs) = &mut ui.prefs {
            prefs.paste(text, &self.chat);
        } else if let Some(history) = &mut ui.history {
            history.query.insert(&line);
            history.searched();
        } else if let Some(Pick::Models(models)) = &mut ui.pick {
            models.query.insert(&line);
            models.searched();
        } else {
            ui.pick = None;
            // Files dropped on the terminal come as their paths: the images among them join the message.
            let home = std::env::var_os("HOME").map(PathBuf::from);
            match message::images(text, home.as_deref()) {
                Some(paths) => {
                    let shrinks = self.shrinks();
                    let read = paths.iter().map(|path| message::read(path, shrinks));
                    self.attach(read.collect());
                }
                // Cmd+V on an image alone: the terminal pastes the text it found, none.
                None if text.is_empty() => self.clip(None),
                None if message::named(text) => self.clip(Some(text.to_string())),
                None => self.desk.paste(text),
            }
        }
    }

    /// The session on the screen, until the user leaves.
    fn play(
        &mut self,
        terminal: &mut ratatui::DefaultTerminal,
        comes: &Receiver<Came>,
    ) -> io::Result<()> {
        loop {
            self.tick();
            let now = self.now();
            self.desk.ui.wall = now;
            self.desk.ui.clock = (SystemTime::now().duration_since(SystemTime::UNIX_EPOCH))
                .map_or(0, |since| since.as_secs().cast_signed());
            self.desk.draw(terminal, &self.chat, now)?;
            let drawn = Instant::now();
            // The saved conversations are being read: their screen says so, turning.
            let reading = self.desk.ui.history.is_some() && self.chat.history.is_none();
            let lively = self.chat.busy()
                || self.desk.moving()
                || self.chat.providers.signing_in()
                || reading
                || now - self.stirred < SETTLE;
            let wait = Duration::from_millis(if lively { FRAME } else { IDLE });
            match comes.recv_timeout(wait) {
                Ok(came) => {
                    if !self.take(came) {
                        return Ok(());
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
            // Everything that waits is taken before the next frame, and
            // what comes in the moment after the last one: a pointer on its
            // way sends many moves, a streaming answer many frames.
            for _ in 0..BATCH {
                let left = Duration::from_millis(FRAME_LEAST).saturating_sub(drawn.elapsed());
                let Ok(came) = comes.recv_timeout(left) else {
                    break;
                };
                if !self.take(came) {
                    return Ok(());
                }
            }
        }
    }
}

/// The recording's file, its first line written: the command.
fn recording(start: &Start) -> io::Result<Option<BufWriter<File>>> {
    let Some(path) = &start.record else {
        return Ok(None);
    };
    let mut file = BufWriter::new(File::create(path)?);
    // Without the launch's arguments: a key may be among them.
    writeln!(
        file,
        "$ cd {} && unharnessed  # the terminal UI's frames with its agent",
        start.cwd
    )?;
    Ok(Some(file))
}

/// The terminal read from a thread of its own, until `left` says the
/// session is over: the session then waits for the terminal and for the
/// agent at once.
fn reader(tell: mpsc::Sender<Came>, left: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut next = None;
        while !left.load(Ordering::Relaxed) {
            let read = match next.take() {
                Some(event) => Ok(event),
                None => match event::poll(Duration::from_millis(50)) {
                    Ok(false) => continue,
                    Ok(true) => event::read(),
                    Err(error) => Err(error),
                },
            };
            let Ok(event) = read else {
                let _ = tell.send(Came::Gone);
                break;
            };
            // A path starts so: what follows at once may be a file dropped on VS Code's terminal.
            let mut events = vec![event];
            if typed(&events[0]).is_some_and(|first| matches!(first, '\'' | '"' | '/')) {
                next = at_once(&mut events, home.as_deref());
            }
            for event in events {
                if tell.send(Came::Screen(event)).is_err() {
                    return;
                }
            }
        }
    })
}

/// The character a key types: one pressed alone or with Shift.
fn typed(event: &Event) -> Option<char> {
    match event {
        Event::Key(KeyEvent {
            code: KeyCode::Char(typed),
            modifiers,
            kind: KeyEventKind::Press,
            ..
        }) if (*modifiers - KeyModifiers::SHIFT).is_empty() => Some(*typed),
        _ => None,
    }
}

/// What the terminal writes at once after the first of `keys`, read into
/// them: the path of a file dropped on VS Code's terminal comes so, typed
/// and not pasted. When they are the paths of images, they turn into the
/// paste the other terminals make of them. Answers what came after them.
fn at_once(keys: &mut Vec<Event>, home: Option<&Path>) -> Option<Event> {
    let mut text: String = keys.iter().filter_map(typed).collect();
    let since = Instant::now();
    let mut after = None;
    while since.elapsed() < RUN_MOST && matches!(event::poll(RUN_GAP), Ok(true)) {
        let Ok(event) = event::read() else {
            break;
        };
        if let Some(typed) = typed(&event) {
            text.push(typed);
            keys.push(event);
        } else {
            after = Some(event);
            break;
        }
    }
    if keys.len() > 1 && message::images(&text, home).is_some() {
        *keys = vec![Event::Paste(text)];
    }
    after
}

/// Runs a live session in the terminal; answers the process's exit code.
#[must_use]
pub fn run(start: Start) -> i32 {
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        eprintln!(
            "Error: the terminal UI needs a terminal; use -p/--print, --mode json, or --mode acp"
        );
        return 1;
    }
    let record = match recording(&start) {
        Ok(record) => record,
        Err(error) => {
            let path = start.record.as_deref().unwrap_or_else(|| "".as_ref());
            eprintln!("Error: cannot record to {}: {error}", path.display());
            return 1;
        }
    };
    let (tell, comes) = mpsc::channel::<Came>();
    let agent = Agent::start(&start.launch, {
        let tell = tell.clone();
        move |told| {
            let _ = tell.send(Came::Agent(1, told));
        }
    });
    let agent = match agent {
        Ok(agent) => agent,
        Err(error) => {
            eprintln!(
                "Error: the agent could not start ({}): {error}",
                start.launch.program.display()
            );
            return 1;
        }
    };
    let mut terminal = match desk::enter() {
        Ok(terminal) => terminal,
        Err(error) => {
            eprintln!("Error: {error}");
            return 1;
        }
    };
    let left = Arc::new(AtomicBool::new(false));
    let reader = reader(tell.clone(), Arc::clone(&left));
    let mut ui = Ui {
        commands: Some(Arc::new(slash::with_own(&[]))),
        live: true,
        // `--resume` alone: the saved conversations, to choose from.
        history: start.choose.then(History::default),
        ..Ui::default()
    };
    for text in &start.then {
        ui.wait(Message::of(text));
    }
    let mut session = Session {
        desk: Desk::new(ui),
        chat: Chat::default(),
        agent: Some(agent),
        stage: Stage::Opening,
        owed: start.choose,
        control: None,
        pressed: None,
        commands: Arc::default(),
        edit: None,
        serving: false,
        start,
        record,
        asked: 0,
        clock: Instant::now(),
        stirred: 0.0,
        said: Vec::new(),
        failed: None,
        offered: false,
        tell,
        reading: Reading::default(),
        run: 1,
        leaving: Vec::new(),
        next: None,
    };
    session.begin();
    session.list();
    let played = session.play(&mut terminal, &comes);
    // The terminal is the shell's again before the agent is waited for.
    left.store(true, Ordering::Relaxed);
    let _ = reader.join();
    desk::leave();
    session.let_go();
    farewell(session.leaving.drain(..).map(|leaving| leaving.thread));
    if let Some(lines) = session.failed {
        for line in lines {
            eprintln!("{line}");
        }
        return 1;
    }
    // Its file is written at its first prompt.
    if let Some((id, _)) =
        (session.chat.saved.as_ref()).filter(|(_, file)| Path::new(file).exists())
    {
        println!("To come back to this conversation: unharnessed --resume {id}");
    }
    match played {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    }
}

/// The agents let go of, each on its thread `going`, save their kernel
/// and exit: the session waits for them. A wait that lasts is told. An
/// interrupt meanwhile ends this process only: the agents, each in its own
/// process group, save all the same.
fn farewell(going: impl Iterator<Item = JoinHandle<()>>) {
    let (done, waits) = mpsc::channel::<()>();
    let teller = std::thread::spawn(move || {
        if waits.recv_timeout(EXIT_QUIET) == Err(RecvTimeoutError::Timeout) {
            eprintln!("Waiting for the agent to save its kernel…");
        }
    });
    for thread in going {
        let _ = thread.join();
    }
    let _ = done.send(());
    let _ = teller.join();
}

/// `text` to the user's clipboard, through the terminal (OSC 52): it
/// reaches the machine the terminal is on, whichever one the agent runs
/// on. A terminal that does not take it leaves the clipboard as it was.
fn copy(text: &str) {
    let coded = message::base64(text.as_bytes());
    let mut out = io::stdout();
    let _ = write!(out, "\x1b]52;c;{coded}\x07").and_then(|()| out.flush());
}
