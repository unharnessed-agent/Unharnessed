//! The conversation as the agent tells it over ACP: what the user sent
//! (with its images), what the agent streamed back (reasoning, text,
//! Python cells), what it used, its kernel, its goal, the providers it can
//! use, its settings, the models and the reasoning efforts its session can
//! be set to, and the folder's saved conversations. It can go back to
//! before a message (`REWIND`): that message and what came after then
//! leave it. It reads the frames the VS Code extension reads
//! (`vscode/src/chat.ts`, `session.ts`, `history.ts`), a part of them for
//! now: one prompt at a time, the messages sent into its turn, the turns
//! the agent starts on its own, a saved conversation coming back, where
//! the conversation was compacted. A new agent is a new conversation: it
//! starts again from nothing when one says who the client is (`initialize`).

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::goal::Goal;
use crate::history::{self, Saved};
use crate::kernel::{Kernel, Preview};
use crate::mention::without_links;
use crate::message::{Attached, Message, UNNAMED};
use crate::models::{self, Model};
use crate::motion::Wave;
use crate::providers::{self, Providers};
use crate::settings::Settings;
use crate::slash::{self, Command};
use crate::text::clean;

/// A message into the running turn (`crates/uh-daemon/src/acp/steer.rs`).
pub const STEER: &str = "_unharnessed/session/steer";
/// What stops a command the agent left running (`crates/uh-daemon/src/acp/kernel.rs`).
pub const KILL: &str = "_unharnessed/kernel/kill";
/// Where a prompt says, for this screen and its recordings, that the user
/// did not write it (a button sent it): it shows no message.
pub const QUIET: &str = "/_meta/unharnessedTui/quiet";
/// Takes the conversation back to before a prompt, to send it again
/// changed (`crates/uh-daemon/src/acp/rewind.rs`).
pub const REWIND: &str = "_unharnessed/session/rewind";
/// Where a prompt names its images, for this screen and its recordings:
/// the agent takes their bytes only.
const NAMES: &str = "/_meta/unharnessedTui/images";

/// What a prompt the user did not write carries beside its blocks (`_meta`).
#[must_use]
pub fn quiet() -> Value {
    serde_json::json!({ "unharnessedTui": { "quiet": true } })
}

/// What a prompt with images carries beside its blocks (`_meta`): their names.
#[must_use]
pub fn named(images: &[Attached]) -> Value {
    let names: Vec<&str> = images.iter().map(|image| image.name.as_str()).collect();
    serde_json::json!({ "unharnessedTui": { "images": names } })
}

/// How long before it came a saved conversation is dated: it happened
/// before, so nothing of it moves in, and the panel opens on its variables.
const LONG_AGO: f64 = 1.0e5;

/// A text as it streams in.
#[derive(Clone, Debug, Default)]
pub struct Stream {
    /// As it came.
    raw: String,
    /// Clean, as shown.
    pub text: String,
    pub wave: Wave,
}

impl Stream {
    fn push(&mut self, now: f64, chunk: &str) {
        self.raw.push_str(chunk);
        self.text = clean(&self.raw);
        self.wave
            .arrive(now, u32::try_from(self.text.len()).unwrap_or(u32::MAX));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Done,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug)]
pub enum Kind {
    /// The model's reasoning: when it started, and when it was over.
    Thought {
        stream: Stream,
        started: f64,
        ended: Option<f64>,
    },
    Text(Stream),
    Tool {
        id: String,
        /// `Python cell`, or the tool's name.
        title: String,
        status: ToolStatus,
        /// The cell's code, or the tool's input as JSON.
        input: String,
        output: Option<String>,
        /// The exception a failed cell raised.
        error: Option<String>,
        /// How many images the cell loaded into the context.
        images: usize,
    },
    Notice {
        tone: Tone,
        text: String,
    },
    /// Where the conversation was compacted (`/compact`, or on its own:
    /// `auto`). Once over: when, how many tokens it summarized, and the
    /// summary; while it runs, none of them.
    Compaction {
        auto: bool,
        ended: Option<f64>,
        tokens: u64,
        summary: Option<String>,
    },
    /// The kernel came back with the conversation's variables, or could
    /// not: a line of its own, which lands when it comes.
    Kernel {
        restored: bool,
    },
}

/// What a turn shows, in the order the agent did it.
#[derive(Clone, Debug)]
pub struct Item {
    /// When it came.
    pub at: f64,
    pub kind: Kind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TurnState {
    Running,
    Done,
    Cancelled,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct Turn {
    /// What the user sent; absent on a turn the agent started on its own.
    pub prompt: Option<String>,
    /// The images that went with it.
    pub images: Vec<Attached>,
    /// Where the conversation stood before it, as the agent names it: it
    /// can go back there, for the message to be sent again changed.
    pub rewind: Option<String>,
    /// What woke the agent, on its own turns.
    pub wake: Option<String>,
    pub state: TurnState,
    pub items: Vec<Item>,
    /// When it showed, when the agent began it, when it ended.
    pub at: f64,
    pub began: f64,
    pub ended: Option<f64>,
    /// A turn of a saved conversation that came back: how long it took, and
    /// its reasoning, was not kept.
    pub replayed: bool,
}

impl Turn {
    #[must_use]
    pub fn running(&self) -> bool {
        self.state == TurnState::Running
    }
}

/// What the conversation has used (`_meta.unharnessed.usage`).
#[derive(Clone, Copy, Debug, Default)]
pub struct Usage {
    /// The context's tokens; absent until a reply measures it.
    pub tokens: Option<u64>,
    /// The model's context window; 0 when unknown.
    pub window: u64,
}

/// What a request the client sent was for, until its answer comes.
enum Asked {
    Session,
    Prompt(usize),
    Steer,
    Tail(String),
    Preview(String),
    /// A command the panel stops.
    Kill(String),
    /// An option of the session set: what it was before, to put back if it is refused.
    Option(Box<Set>),
    /// The folder's saved conversations, and one of them deleted.
    History,
    Forget(String),
    /// The conversation taken back to before that turn.
    Rewind(usize),
}

/// The session's options as the screen shows them: the model in use (its
/// value, its name, its provider) and the reasoning effort.
#[derive(Clone)]
struct Set {
    chosen: Option<String>,
    model: Option<String>,
    maker: Option<String>,
    effort: Option<String>,
}

#[derive(Default)]
pub struct Chat {
    pub turns: Vec<Turn>,
    pub usage: Option<Usage>,
    /// The model's name and the reasoning effort, as the session's options give them.
    pub model: Option<String>,
    pub effort: Option<String>,
    /// The efforts the model takes, from the least: the session's options list them.
    pub efforts: Vec<String>,
    /// The models the session can run on, and the value of the one in use.
    pub models: Vec<Model>,
    pub chosen: Option<String>,
    /// The commands, prompt templates and skills the agent takes after a
    /// `/`, as it last listed them.
    pub commands: Arc<Vec<Command>>,
    /// The folder's saved conversations, once the agent listed them; why
    /// it could not, or could not delete one.
    pub history: Option<Vec<Saved>>,
    pub history_error: Option<String>,
    pub kernel: Kernel,
    /// The conversation's goal, while the message box shows one.
    pub goal: Option<Goal>,
    /// The goal's status as the agent last named it, to tell when it changes.
    goal_named: Option<String>,
    /// The providers the agent can use, and the sign-in under way.
    pub providers: Providers,
    /// The agent's settings, its MCP servers and its skills.
    pub settings: Settings,
    /// The turn the next going back cuts the conversation at, and how the
    /// last one went: the turn it cut at, or why it was refused.
    cut: Option<usize>,
    pub rewound: Option<Result<usize, String>>,
    /// The open session's id, as the agent gave it.
    pub session: Option<String>,
    /// The saved conversation the session writes to, as the agent names
    /// it: its id and its file, either of which `--resume` takes.
    pub saved: Option<(String, String)>,
    /// A saved conversation is coming back: the agent replays its turns.
    loading: bool,
    /// The kernel came back with the saved variables: the next list of them is no news.
    restored: bool,
    /// The provider of the model in use.
    pub maker: Option<String>,
    asked: HashMap<i64, Asked>,
    /// Our prompt, sent and not answered yet.
    sent: Option<usize>,
    /// The messages sent into the running turn that it has not taken yet,
    /// each with its request, in the order they went.
    steering: Vec<(i64, Message)>,
    /// The ones it refused, or ended without taking: to send again.
    back: Vec<Message>,
    /// The turn the agent's updates go to: ours, or one it started.
    live: Option<usize>,
    /// The agent's turn ids, ours.
    ids: HashMap<i64, usize>,
    /// The last streamed item's message and kind: chunks of the same join it.
    streaming: Option<String>,
    /// The reasoning under way: its turn and its place there.
    thinking: Option<(usize, usize)>,
    /// The compaction under way: its turn and its place there.
    compacting: Option<(usize, usize)>,
}

impl Chat {
    /// A frame the client sent at `now`.
    pub fn sent(&mut self, now: f64, frame: &Value) {
        let method = frame.get("method").and_then(Value::as_str);
        let Some(id) = frame.get("id").and_then(Value::as_i64) else {
            // A turn the agent started answers no request: stopped, it ends here.
            if let (Some("session/cancel"), Some(live)) = (method, self.live) {
                if Some(live) != self.sent {
                    self.end(now, live, TurnState::Cancelled);
                }
            }
            return;
        };
        let params = frame.get("params");
        let said = |key: &str| {
            params
                .and_then(|params| params.get(key))
                .and_then(Value::as_str)
        };
        match method {
            // A new agent: its conversation starts from nothing.
            Some("initialize") => *self = Chat::default(),
            Some("session/set_config_option") => {
                let before = self.set();
                match (said("configId"), said("value")) {
                    (Some("model"), Some(value)) => self.choose(value),
                    (Some("thought_level"), Some(level)) => self.effort = Some(clean(level)),
                    _ => {}
                }
                self.asked.insert(id, Asked::Option(Box::new(before)));
            }
            // The one that looks for the conversation the agent holds names no folder.
            Some("session/list") if said("cwd").is_some() => {
                self.asked.insert(id, Asked::History);
            }
            Some(history::DELETE) => {
                if let Some(session) = said("sessionId") {
                    self.asked.insert(id, Asked::Forget(session.to_string()));
                }
            }
            Some(method @ ("session/new" | "session/load")) => {
                // A loaded session's turns come before its answer.
                self.loading = method == "session/load";
                self.asked.insert(id, Asked::Session);
            }
            Some(STEER) => {
                self.read_goal();
                self.steering.push((id, message(params)));
                self.asked.insert(id, Asked::Steer);
            }
            Some("session/prompt") => {
                let quiet = frame.pointer(&format!("/params{QUIET}")) == Some(&Value::Bool(true));
                self.prompted(now, id, quiet, params);
            }
            Some("_unharnessed/kernel/tail") => {
                if let Some(command) = params
                    .and_then(|params| params.get("id"))
                    .and_then(Value::as_str)
                {
                    self.asked.insert(id, Asked::Tail(command.to_string()));
                }
            }
            Some(KILL) => {
                if let Some(command) = said("id") {
                    self.kernel.stop(now, command);
                    self.asked.insert(id, Asked::Kill(command.to_string()));
                }
            }
            Some("_unharnessed/kernel/preview") => {
                if let Some(name) = params
                    .and_then(|params| params.get("name"))
                    .and_then(Value::as_str)
                {
                    self.asked.insert(id, Asked::Preview(clean(name)));
                }
            }
            Some(REWIND) => {
                // The turn the screen named; a recording names it by where it goes back to.
                let to = said("to");
                let found = || (self.turns.iter()).position(|turn| turn.rewind.as_deref() == to);
                if let Some(turn) = self.cut.take().or_else(found) {
                    self.asked.insert(id, Asked::Rewind(turn));
                }
            }
            Some(method) => self.other(id, method, params.unwrap_or(&Value::Null)),
            None => {}
        }
    }

    /// Our prompt, sent at `now` as request `id`: its turn starts. `quiet`:
    /// the user did not write it, and it shows no message.
    fn prompted(&mut self, now: f64, id: i64, quiet: bool, params: Option<&Value>) {
        // A goal reached or failed leaves with the next message.
        self.read_goal();
        let Message { text, images } = message(params);
        self.turns.push(Turn {
            prompt: (!quiet).then_some(text),
            images,
            rewind: None,
            wake: None,
            state: TurnState::Running,
            items: Vec::new(),
            at: now,
            began: now,
            ended: None,
            replayed: false,
        });
        let turn = self.turns.len() - 1;
        self.sent = Some(turn);
        self.asked.insert(id, Asked::Prompt(turn));
    }

    /// A request about the settings, or about the providers.
    fn other(&mut self, id: i64, method: &str, params: &Value) {
        if self.settings.sent(id, method, params) {
            return;
        }
        self.providers.sent(id, method, params);
        // A sign-in to an MCP server (`mcp:<id>`) goes by the server's name.
        let sign_in = (self.providers.sign_in.as_mut()).filter(|_| method == providers::LOGIN);
        if let Some(sign_in) = sign_in {
            if let Some(id) = sign_in.provider.strip_prefix("mcp:") {
                let server = self.settings.server(id);
                sign_in.name = server.map_or_else(|| clean(id), |server| server.label.clone());
            }
        }
    }

    /// The answer to `session/new` or `session/load`: the session, the
    /// conversation it writes to, its options.
    fn opened(&mut self, now: f64, frame: &Value) {
        self.loading = false;
        let Some(result) = frame.get("result") else {
            let why = why(frame, "it did not say why");
            let text = format!("The session could not open: {why}");
            self.line(now, vec![(Tone::Error, text)]);
            return;
        };
        self.session = result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let saved = |key: &str| {
            result
                .pointer(&format!("/_meta/unharnessed/saved/{key}"))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        self.saved = saved("sessionId").zip(saved("path"));
        if let Some(options) = result.get("configOptions") {
            self.options(options);
        }
    }

    /// A block of a replayed prompt: its text, or one of its images.
    fn replayed(&mut self, turn: usize, update: &Value) {
        let Some(turn) = self.turns.get_mut(turn) else {
            return;
        };
        if let Some(image) = (update.get("content")).and_then(|block| image(block, UNNAMED)) {
            // A message of images alone is one all the same.
            turn.prompt.get_or_insert_with(String::new);
            turn.images.push(image);
        }
        if let Some(text) = update.pointer("/content/text").and_then(Value::as_str) {
            // As it was written: the links of its mentions are the agent's.
            let text = clean(without_links(text));
            match &mut turn.prompt {
                Some(prompt) if !prompt.is_empty() => {
                    prompt.push('\n');
                    prompt.push_str(&text);
                }
                _ => turn.prompt = Some(text),
            }
        }
    }

    /// A frame the agent sent at `now`.
    pub fn received(&mut self, now: f64, frame: &Value) {
        let params = frame.get("params").unwrap_or(&Value::Null);
        match frame.get("method").and_then(Value::as_str) {
            Some("session/update") => {
                if let Some(update) = params.get("update") {
                    self.update(now, update);
                }
            }
            Some("_unharnessed/kernel/changed") => {
                // The variables a restored kernel came back with were there before.
                let listed = params.get("variables").is_some();
                let at = if listed && std::mem::take(&mut self.restored) {
                    now - LONG_AGO
                } else {
                    now
                };
                self.kernel.changed(at, params);
            }
            Some("_unharnessed/kernel/restored") => {
                let restored = params.get("restored").and_then(Value::as_bool) == Some(true);
                self.restored = restored;
                // It lands now, also in a conversation coming back.
                self.lines(now, vec![Kind::Kernel { restored }]);
            }
            Some(providers::LOGIN_UPDATE) => self.providers.update(params),
            Some(_) => {}
            None => self.answered(now, frame),
        }
    }

    /// The answer to a request the client sent.
    fn answered(&mut self, now: f64, frame: &Value) {
        let Some(id) = frame.get("id").and_then(Value::as_i64) else {
            return;
        };
        let Some(asked) = self.asked.remove(&id) else {
            if !self.settings.answered(id, frame) {
                self.providers.answered(id, frame);
                // Signed in to an MCP server: its list shows it, a restart applies it.
                let signed = self.providers.signed.take();
                if signed.is_some_and(|provider| provider.starts_with("mcp:")) {
                    self.settings.signed_in();
                }
            }
            return;
        };
        let result = frame.get("result");
        match asked {
            Asked::Session => self.opened(now, frame),
            Asked::Prompt(turn) => {
                // The answer ends the last turn of the request.
                let turn = self.sent.unwrap_or(turn);
                let state = match frame.get("error") {
                    Some(_) => TurnState::Failed(why(frame, "The turn failed.")),
                    None => match result
                        .and_then(|result| result.get("stopReason"))
                        .and_then(Value::as_str)
                    {
                        Some("cancelled") => TurnState::Cancelled,
                        _ => TurnState::Done,
                    },
                };
                self.end(now, turn, state);
            }
            Asked::Steer => {
                // Refused: the turn is over, or the message cannot join one.
                let refused = (self.steering.iter()).position(|(request, _)| *request == id);
                if let (Some(_), Some(at)) = (frame.get("error"), refused) {
                    self.back.push(self.steering.remove(at).1);
                }
            }
            Asked::Option(before) => self.set_to(now, frame, *before),
            Asked::History => {
                self.history_error = result
                    .is_none()
                    .then(|| why(frame, "The conversations could not be listed."));
                if let Some(result) = result {
                    self.history = Some(history::listed(result));
                }
            }
            Asked::Forget(session) => {
                self.history_error = (result.is_none()).then(|| why(frame, "It was not deleted."));
                if let (Some(_), Some(saved)) = (result, &mut self.history) {
                    saved.retain(|chat| chat.id != session);
                }
            }
            Asked::Rewind(turn) => {
                self.rewound = Some(match result {
                    Some(_) => {
                        self.forget(turn);
                        Ok(turn)
                    }
                    None => Err(why(frame, "the agent did not say why")),
                });
            }
            // Stopped, or already ended: its list says which. Refused, it runs on.
            Asked::Kill(command) => {
                if result.is_none() {
                    self.kernel.unstopped(&command);
                }
            }
            Asked::Tail(command) => {
                if let Some(tail) = result
                    .and_then(|result| result.get("tail"))
                    .and_then(Value::as_str)
                {
                    self.kernel.tail(now, &command, tail);
                }
            }
            Asked::Preview(name) => {
                let preview = match result {
                    Some(result) => Ok(Preview {
                        text: clean(
                            result
                                .get("preview")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                        ),
                        code: result.get("style").and_then(Value::as_str) == Some("code"),
                        truncated: result.get("truncated").and_then(Value::as_bool) == Some(true),
                    }),
                    // Why it was refused: `busy` while a cell runs, else the kernel's words.
                    None => Err(why(frame, "The value could not be shown.")),
                };
                self.kernel.previewed(&name, preview);
            }
        }
    }

    /// The session's pickers, as the agent gives them whole: the models
    /// and the one in use, the reasoning efforts and the one in use (a
    /// model that takes none has no such option).
    fn options(&mut self, options: &Value) {
        let option = |id: &str| {
            (options.as_array().into_iter().flatten())
                .find(|option| option.get("id").and_then(Value::as_str) == Some(id))
        };
        if let Some(model) = option("model") {
            if model.get("options").is_some() {
                self.models = models::listed(model, &self.models);
            }
            if let Some(current) = model.get("currentValue").and_then(Value::as_str) {
                self.choose(current);
            }
        }
        let effort = option("thought_level");
        let current = effort.and_then(|effort| effort.get("currentValue"));
        self.effort = current.and_then(Value::as_str).map(clean);
        self.efforts = (effort.and_then(|effort| effort.get("options")))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|level| level.get("value").and_then(Value::as_str))
            .map(clean)
            .collect();
    }

    /// The agent's answer to an option set: what they all are now; or,
    /// refused, the screen shows again what is still in use (`before`),
    /// and why.
    fn set_to(&mut self, now: f64, frame: &Value, before: Set) {
        if let Some(options) = frame.pointer("/result/configOptions") {
            return self.options(options);
        }
        let Set {
            chosen,
            model,
            maker,
            effort,
        } = before;
        (self.chosen, self.model, self.maker, self.effort) = (chosen, model, maker, effort);
        let why = why(frame, "the agent did not say why");
        let text = format!("The session was not changed: {why}");
        self.line(now, vec![(Tone::Error, text)]);
    }

    /// The model of value `value` is the one in use.
    fn choose(&mut self, value: &str) {
        let model = self.models.iter().find(|model| model.value == value);
        self.maker = model.map(|model| model.provider.clone());
        self.model = Some(model.map_or_else(|| clean(value), |model| model.name.clone()));
        self.chosen = Some(value.to_string());
    }

    /// The session's options as the screen shows them.
    fn set(&self) -> Set {
        Set {
            chosen: self.chosen.clone(),
            model: self.model.clone(),
            maker: self.maker.clone(),
            effort: self.effort.clone(),
        }
    }

    /// When what comes at `now` happened: long ago, for a saved conversation coming back.
    fn dated(&self, now: f64) -> f64 {
        if self.loading {
            now - LONG_AGO
        } else {
            now
        }
    }

    /// A line of the conversation that is no turn of the agent's: what
    /// happened to its kernel, or to its process.
    fn line(&mut self, now: f64, notices: Vec<(Tone, String)>) {
        let kinds = notices
            .into_iter()
            .map(|(tone, text)| Kind::Notice { tone, text });
        self.lines(now, kinds.collect());
    }

    fn lines(&mut self, now: f64, kinds: Vec<Kind>) {
        self.turns.push(Turn {
            prompt: None,
            images: Vec::new(),
            rewind: None,
            wake: None,
            state: TurnState::Done,
            items: kinds
                .into_iter()
                .map(|kind| Item { at: now, kind })
                .collect(),
            at: now,
            began: now,
            ended: Some(now),
            replayed: false,
        });
    }

    /// A word of the client's own in the conversation's place: what it
    /// waits for before the agent is there.
    pub fn told(&mut self, now: f64, text: &str) {
        self.line(now, vec![(Tone::Info, text.to_string())]);
    }

    /// The agent works: on our prompt, or on a turn it started.
    #[must_use]
    pub fn busy(&self) -> bool {
        self.sent.is_some() || self.live.is_some()
    }

    /// The agent's id for the turn running, once it said it started: a
    /// message can be sent into it.
    #[must_use]
    pub fn joinable(&self) -> Option<i64> {
        let live = self.live.filter(|live| self.turns[*live].running())?;
        (self.ids.iter())
            .find(|(_, turn)| **turn == live)
            .map(|(id, _)| *id)
    }

    /// The messages sent into the running turn that it has not taken yet.
    pub fn steering(&self) -> impl Iterator<Item = &Message> {
        self.steering.iter().map(|(_, message)| message)
    }

    /// The messages the turn did not take, for the client to send again:
    /// they are no longer kept here.
    pub fn returned(&mut self) -> Vec<Message> {
        std::mem::take(&mut self.back)
    }

    /// The messages sent into the running turn are the client's again,
    /// with the ones it already gave back: the user stopped the turn.
    pub fn unsteer(&mut self) -> Vec<Message> {
        let mut all = std::mem::take(&mut self.back);
        all.extend(self.steering.drain(..).map(|(_, text)| text));
        all
    }

    /// No provider is connected, and the model the session opened on is
    /// one of theirs: nothing can answer until one is.
    #[must_use]
    pub fn unconnected(&self) -> bool {
        let listed =
            |maker: &String| (self.providers.list.iter().flatten()).any(|row| row.key.id == *maker);
        self.providers.none_connected() && self.maker.as_ref().is_none_or(listed)
    }

    /// The agent's process ended, `how` (and what it last wrote, `said`):
    /// what ran did not finish, and nothing more comes.
    pub fn stopped(&mut self, now: f64, how: &str, said: &[String]) {
        self.loading = false;
        for turn in [self.sent, self.live].into_iter().flatten() {
            self.end(
                now,
                turn,
                TurnState::Failed("The agent stopped.".to_string()),
            );
        }
        self.kernel.gone(now);
        self.providers.stopped();
        let mut notices = vec![(Tone::Error, clean(how))];
        if !said.is_empty() {
            notices.push((Tone::Info, clean(&said.join("\n"))));
        }
        self.line(now, notices);
    }

    /// The next going back (`REWIND`) is to before turn `turn`: what it
    /// cuts the conversation at, once the agent went back.
    pub fn rewinding(&mut self, turn: usize) {
        self.cut = Some(turn);
    }

    /// The conversation went back to before turn `turn`: that turn and
    /// what came after it are no longer in it.
    fn forget(&mut self, turn: usize) {
        self.turns.truncate(turn);
        self.ids.retain(|_, kept| *kept < turn);
        let gone = |kept: &usize| *kept >= turn;
        if self.sent.as_ref().is_some_and(gone) {
            self.sent = None;
        }
        if self.live.as_ref().is_some_and(gone) {
            self.live = None;
        }
        if self.thinking.is_some_and(|(kept, _)| kept >= turn) {
            self.thinking = None;
        }
        if self.compacting.is_some_and(|(kept, _)| kept >= turn) {
            self.compacting = None;
        }
        self.streaming = None;
    }

    /// `message` was to take the place of an earlier one, and the agent
    /// could not go back to it (`why`): it shows as a turn that failed.
    pub fn unsent(&mut self, now: f64, message: Message, why: &str) {
        self.turns.push(Turn {
            prompt: Some(message.text),
            images: message.images,
            rewind: None,
            wake: None,
            state: TurnState::Failed(format!("It could not go back to the message: {why}")),
            items: Vec::new(),
            at: now,
            began: now,
            ended: Some(now),
            replayed: false,
        });
    }

    /// The goal the box shows was read to its end: one reached or failed leaves it.
    pub fn read_goal(&mut self) {
        if self.goal.as_ref().is_some_and(Goal::over) {
            self.goal = None;
        }
    }

    /// A `session/update`.
    fn update(&mut self, now: f64, update: &Value) {
        // The goal's time counts from when it is told, also for a conversation coming back.
        let told = now;
        let now = self.dated(now);
        let meta = update.pointer("/_meta/unharnessed").unwrap_or(&Value::Null);
        let kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if kind == "available_commands_update" {
            self.commands = Arc::new(slash::listed(update));
            return;
        }
        if kind == "config_option_update" {
            if let Some(options) = update.get("configOptions") {
                self.options(options);
            }
            return;
        }
        if let Some(usage) = meta.get("usage") {
            if usage.get("compacting") == Some(&Value::Bool(true)) {
                self.mark(now);
            }
            self.usage = Some(Usage {
                tokens: usage.get("tokens").and_then(Value::as_u64),
                window: usage.get("window").and_then(Value::as_u64).unwrap_or(0),
            });
            return;
        }
        // The goal, and whether it stands elsewhere than it did.
        let mut moved = false;
        if let Some(goal) = meta.get("goal") {
            self.goal = Goal::read(told, goal, self.goal.as_ref());
            let named = goal.get("status").and_then(Value::as_str);
            moved = named.is_some() && named != self.goal_named.as_deref();
            if moved {
                self.goal_named = named.map(str::to_string);
            }
        }
        let agent_turn = meta
            .get("promptTurnId")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if let Some(start) = meta.get("turnStart") {
            self.begin(now, agent_turn, start);
            return;
        }
        let Some(turn) = self.ids.get(&agent_turn).copied().or(self.live) else {
            return;
        };
        // The goal's change, once: a `/goal` command's answer says the same,
        // and says alone that it was cleared.
        if let Some(goal) = self.goal.as_ref().filter(|_| moved) {
            let text = format!("Goal {}: {}", goal.named, goal.objective);
            self.notice(now, turn, Tone::Info, text);
        }
        match kind {
            "user_message_chunk" => self.replayed(turn, update),
            "agent_thought_chunk" | "agent_message_chunk" => {
                let text = update
                    .pointer("/content/text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let message = update
                    .get("messageId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.stream(now, turn, kind == "agent_thought_chunk", message, text);
            }
            "tool_call" => {
                let text = |key: &str| update.get(key).and_then(Value::as_str);
                let input = match update.get("rawInput") {
                    Some(input) => match input.get("code").and_then(Value::as_str) {
                        Some(code) => clean(code),
                        None => clean(&serde_json::to_string_pretty(input).unwrap_or_default()),
                    },
                    None => String::new(),
                };
                self.push(
                    now,
                    turn,
                    Kind::Tool {
                        id: text("toolCallId").unwrap_or_default().to_string(),
                        title: clean(text("title").unwrap_or("Tool")),
                        status: ToolStatus::Running,
                        input,
                        output: None,
                        error: None,
                        images: 0,
                    },
                );
            }
            "tool_call_update" => self.tool_done(turn, update, meta),
            "session_info_update" => self.info(now, turn, meta),
            _ => {}
        }
    }

    /// What started a turn comes first in it: our prompt, or a wake.
    fn begin(&mut self, now: f64, agent_turn: i64, start: &Value) {
        let origin = start.get("origin").and_then(Value::as_str);
        let rewind = (start.get("rewindTo").and_then(Value::as_str)).map(str::to_string);
        // A turn of the conversation the agent replays: its prompt follows.
        let ours = if self.loading { None } else { self.sent };
        if origin == Some("steer") && !self.loading {
            return self.joined(now, agent_turn, rewind);
        }
        if let (Some("prompt"), Some(sent)) = (origin, ours) {
            self.ids.insert(agent_turn, sent);
            self.live = Some(sent);
            // It may have waited for a turn the agent started.
            self.turns[sent].began = now;
            self.turns[sent].rewind = rewind;
            return;
        }
        let wake = match start.get("wake").and_then(Value::as_str) {
            Some("bash") => Some("a background command ended"),
            Some("quota") => Some("the usage limit reset"),
            Some("goal") => Some("the goal's pause ended"),
            _ => None,
        };
        self.turns.push(Turn {
            prompt: None,
            images: Vec::new(),
            rewind,
            wake: wake.map(str::to_string),
            state: TurnState::Running,
            items: Vec::new(),
            at: now,
            began: now,
            ended: None,
            replayed: self.loading,
        });
        let turn = self.turns.len() - 1;
        self.ids.insert(agent_turn, turn);
        // A replayed turn says itself where it ends.
        if !self.loading {
            self.live = Some(turn);
        }
    }

    /// A message sent into the running turn joined it: what was done so
    /// far is done, what follows is the message's own turn, under it.
    fn joined(&mut self, now: f64, agent_turn: i64, rewind: Option<String>) {
        let message = (!self.steering.is_empty()).then(|| self.steering.remove(0).1);
        let (prompt, images) = match message {
            Some(message) => (Some(message.text), message.images),
            None => (None, Vec::new()),
        };
        let before = self.ids.get(&agent_turn).copied().or(self.live);
        if let Some(before) = before.filter(|before| self.turns[*before].running()) {
            if self
                .thinking
                .is_some_and(|(thinking, _)| thinking == before)
            {
                self.settle(now);
            }
            self.turns[before].state = TurnState::Done;
            self.turns[before].ended = Some(now);
        }
        self.turns.push(Turn {
            prompt,
            images,
            rewind,
            wake: None,
            state: TurnState::Running,
            items: Vec::new(),
            at: now,
            began: now,
            ended: None,
            replayed: false,
        });
        let turn = self.turns.len() - 1;
        // The request's answer ends the turn that took it on.
        if self.sent.is_some() && self.sent == before {
            self.sent = Some(turn);
        }
        self.ids.insert(agent_turn, turn);
        self.live = Some(turn);
        self.streaming = None;
    }

    fn stream(&mut self, now: f64, turn: usize, thought: bool, message: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        let key = format!("{}:{message}", if thought { "thought" } else { "text" });
        if self.streaming.as_deref() == Some(&key) {
            match self.turns[turn].items.last_mut().map(|item| &mut item.kind) {
                Some(Kind::Thought { stream, .. }) if thought => return stream.push(now, text),
                Some(Kind::Text(stream)) if !thought => return stream.push(now, text),
                _ => {}
            }
        }
        let mut stream = Stream::default();
        stream.push(now, text);
        if thought {
            // Reasoning the agent did not say opened: timed from its first words.
            self.push(
                now,
                turn,
                Kind::Thought {
                    stream,
                    started: now,
                    ended: None,
                },
            );
            self.thinking = Some((turn, self.turns[turn].items.len() - 1));
        } else {
            self.push(now, turn, Kind::Text(stream));
        }
        self.streaming = Some(key);
    }

    /// The reasoning under way is over.
    fn settle(&mut self, now: f64) {
        let Some((turn, at)) = self.thinking.take() else {
            return;
        };
        if let Some(Kind::Thought { ended, .. }) = self.turns[turn]
            .items
            .get_mut(at)
            .map(|item| &mut item.kind)
        {
            ended.get_or_insert(now);
        }
    }

    fn push(&mut self, now: f64, turn: usize, kind: Kind) {
        // Whatever comes next, the reasoning before it is over.
        self.settle(now);
        self.turns[turn].items.push(Item { at: now, kind });
        self.streaming = None;
    }

    fn tool_done(&mut self, turn: usize, update: &Value, meta: &Value) {
        let call = update.get("toolCallId").and_then(Value::as_str);
        let Some(Kind::Tool {
            status,
            output,
            error,
            images,
            ..
        }) = self.turns[turn]
            .items
            .iter_mut()
            .map(|item| &mut item.kind)
            .find(|kind| matches!(kind, Kind::Tool { id, .. } if Some(id.as_str()) == call))
        else {
            return;
        };
        match update.get("status").and_then(Value::as_str) {
            Some("completed") => *status = ToolStatus::Done,
            Some("failed") => *status = ToolStatus::Failed,
            _ => {}
        }
        if let Some(blocks) = update.get("content").and_then(Value::as_array) {
            let text: Vec<&str> = blocks
                .iter()
                .map(|block| {
                    block
                        .pointer("/content/text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                })
                .collect();
            *output = Some(clean(&text.join("\n")));
        }
        if let Some(loaded) = (meta.pointer("/ipython/attachments")).and_then(Value::as_array) {
            *images = loaded.len();
        }
        // A cell that raised ran to its end for the agent (its traceback is its result); here it failed.
        if let Some(name) = meta.pointer("/ipython/errorEname").and_then(Value::as_str) {
            *error = Some(clean(name));
            *status = ToolStatus::Failed;
        }
    }

    /// Reasoning opening and closing, a command's answer, a cut answer; the
    /// end of a turn the agent started.
    fn info(&mut self, now: f64, turn: usize, meta: &Value) {
        if let Some(end) = meta.get("turnEnd") {
            let state = match end.get("state").and_then(Value::as_str) {
                Some("cancelled") => TurnState::Cancelled,
                Some("failed") => {
                    let error = end.get("error").and_then(Value::as_str);
                    TurnState::Failed(clean(error.unwrap_or("The turn failed.")))
                }
                _ => TurnState::Done,
            };
            self.end(now, turn, state);
            return;
        }
        match meta.pointer("/thinking/state").and_then(Value::as_str) {
            Some("start") => {
                // It shows at once, before its words (if they ever come).
                let message = meta
                    .pointer("/thinking/messageId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.push(
                    now,
                    turn,
                    Kind::Thought {
                        stream: Stream::default(),
                        started: now,
                        ended: None,
                    },
                );
                self.streaming = Some(format!("thought:{message}"));
                self.thinking = Some((turn, self.turns[turn].items.len() - 1));
            }
            Some("end") => self.settle(now),
            _ => {}
        }
        if meta.get("outputLimit").and_then(Value::as_bool) == Some(true) {
            self.push(
                now,
                turn,
                Kind::Notice {
                    tone: Tone::Info,
                    text: "The answer hit the output limit; the agent went on.".to_string(),
                },
            );
        }
        if let Some(text) = meta.pointer("/command/text").and_then(Value::as_str) {
            let tone = match meta.pointer("/command/severity").and_then(Value::as_str) {
                Some("error") => Tone::Error,
                Some("warning") => Tone::Warning,
                _ => Tone::Info,
            };
            self.notice(now, turn, tone, clean(text));
        }
        if let Some(compaction) = meta.get("compaction").filter(|value| !value.is_null()) {
            self.compacted(now, turn, compaction);
        }
        // Our prompt ends with its answer; a turn the agent started, here.
        if meta.get("phase").and_then(Value::as_str) == Some("responseBoundary")
            && Some(turn) != self.sent
        {
            let failed = meta.get("outcome").and_then(Value::as_str) == Some("error");
            let state = if failed {
                TurnState::Failed("The turn failed.".to_string())
            } else {
                TurnState::Done
            };
            self.end(now, turn, state);
        }
    }

    /// A compaction starts: it shows in the running turn, at its end.
    fn mark(&mut self, now: f64) {
        let Some(turn) = self.live.or(self.sent) else {
            return;
        };
        if self.compacting.is_some() {
            return;
        }
        let auto = !compacts(&self.turns[turn]);
        let kind = Kind::Compaction {
            auto,
            ended: None,
            tokens: 0,
            summary: None,
        };
        self.push(now, turn, kind);
        self.compacting = Some((turn, self.turns[turn].items.len() - 1));
    }

    /// A compaction ended: one that ran says how much it summarized, and
    /// into what; one that did not (skipped, failed, stopped) leaves. One
    /// of a conversation coming back comes over already.
    fn compacted(&mut self, now: f64, turn: usize, compaction: &Value) {
        let Some(tokens) = compaction.get("tokensBefore").and_then(Value::as_u64) else {
            return self.unmark();
        };
        let summary = (compaction.get("summary").and_then(Value::as_str))
            .map(clean)
            .filter(|summary| !summary.trim().is_empty());
        let marked = self.compacting.take().and_then(|(turn, at)| {
            let item = self.turns.get_mut(turn)?.items.get_mut(at)?;
            matches!(item.kind, Kind::Compaction { ended: None, .. }).then_some(item)
        });
        if let Some(item) = marked {
            let auto = matches!(item.kind, Kind::Compaction { auto: true, .. });
            item.kind = Kind::Compaction {
                auto,
                ended: Some(now),
                tokens,
                summary,
            };
        } else {
            let kind = Kind::Compaction {
                auto: !compacts(&self.turns[turn]),
                ended: Some(now),
                tokens,
                summary,
            };
            self.push(now, turn, kind);
        }
    }

    /// The compaction under way leaves: it did not run.
    fn unmark(&mut self) {
        let Some((turn, at)) = self.compacting.take() else {
            return;
        };
        let Some(items) = self.turns.get_mut(turn).map(|turn| &mut turn.items) else {
            return;
        };
        if matches!(
            items.get(at).map(|item| &item.kind),
            Some(Kind::Compaction { ended: None, .. })
        ) {
            items.remove(at);
            // What came after it moved up a place.
            if let Some((thinking, after)) = &mut self.thinking {
                if *thinking == turn && *after > at {
                    *after -= 1;
                }
            }
            self.streaming = None;
        }
    }

    /// Said once per turn.
    fn notice(&mut self, now: f64, turn: usize, tone: Tone, text: String) {
        let said = (self.turns[turn].items.iter())
            .any(|item| matches!(&item.kind, Kind::Notice { text: before, .. } if *before == text));
        if !said {
            self.push(now, turn, Kind::Notice { tone, text });
        }
    }

    fn end(&mut self, now: f64, turn: usize, state: TurnState) {
        if self.thinking.is_some_and(|(thinking, _)| thinking == turn) {
            self.settle(now);
        }
        // Its compaction never said how it ended.
        if self
            .compacting
            .is_some_and(|(compacting, _)| compacting == turn)
        {
            self.unmark();
        }
        let Some(ending) = self.turns.get_mut(turn) else {
            return;
        };
        if ending.running() {
            ending.state = state;
            ending.ended = Some(now);
        }
        if self.sent == Some(turn) {
            self.sent = None;
        }
        if self.live == Some(turn) {
            self.live = None;
        }
        self.streaming = None;
        // The turn is over: the agent let go of what it did not take.
        if !self.busy() {
            let untaken = self.steering.drain(..).map(|(_, text)| text);
            self.back.extend(untaken);
        }
    }

    /// The variables whose value the panel shows and does not have, and the
    /// running commands whose last lines it shows and are due: what to ask
    /// the agent now, `open` telling which rows are open.
    pub fn wanted(&mut self, now: f64, open: &crate::fold::Folds) -> (Vec<String>, Vec<String>) {
        self.kernel.wanted(now, open)
    }

    /// Time passed: what finished leaving goes.
    pub fn tick(&mut self, now: f64) {
        self.kernel.tick(now);
    }
}

/// The turn is `/compact`: a compaction in it is the user's, not one of its own.
fn compacts(turn: &Turn) -> bool {
    let text = turn.prompt.as_deref().unwrap_or_default().trim();
    text.strip_prefix("/compact")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

/// What a prompt's blocks say, as the user wrote it: its text, without
/// the links that go with it, and its images, each by the name the
/// prompt gives it beside its blocks.
fn message(params: Option<&Value>) -> Message {
    let blocks = || {
        (params.and_then(|params| params.get("prompt")))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
    };
    let text: Vec<&str> = blocks()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect();
    let names = params.and_then(|params| params.pointer(NAMES));
    let name = |at: usize| names?.get(at)?.as_str();
    let images = blocks()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("image"))
        .enumerate()
        .filter_map(|(at, block)| image(block, name(at).unwrap_or(UNNAMED)));
    Message {
        text: clean(&text.join("\n")),
        images: images.collect(),
    }
}

/// The image a block of a prompt is, called `name`.
fn image(block: &Value, name: &str) -> Option<Attached> {
    if block.get("type").and_then(Value::as_str) != Some("image") {
        return None;
    }
    Some(Attached {
        name: clean(name),
        mime: block.get("mimeType")?.as_str()?.to_string(),
        data: block.get("data")?.as_str()?.into(),
    })
}

/// Why the agent refused a request: its own words (`data.details`, or
/// `data.reason`), else the error's message, else `otherwise`.
fn why(frame: &Value, otherwise: &str) -> String {
    clean(
        frame
            .pointer("/error/data/details")
            .or_else(|| frame.pointer("/error/data/reason"))
            .or_else(|| frame.pointer("/error/message"))
            .and_then(Value::as_str)
            .unwrap_or(otherwise),
    )
}
