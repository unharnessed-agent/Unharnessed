//! Starting the agent process and opening its session: a new
//! conversation, or a saved one (`-c`, `--resume`, `--fork`, `/resume`).
//! The agent holds one session per process, so `/new` and `/resume` start
//! a new process.

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use super::agent::{Agent, RpcError};
use super::picker::{Item, Picker};
use super::screen::{Color, Line, Style};
use super::transcript::Transcript;
use super::{App, Block};
use crate::mode::RunOptions;

/// How the first session opens.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Start {
    /// A new conversation (`--fork` too: the agent already holds the copy).
    Fresh,
    /// The agent was started on a saved conversation (`-c`, `--resume <x>`).
    Resume,
    /// A bare `--resume`: ask which one.
    Pick,
}

impl Start {
    pub(super) fn of(options: &RunOptions) -> Start {
        if options.session.resume_bare {
            Start::Pick
        } else if options.session.continue_recent || options.session.resume.is_some() {
            Start::Resume
        } else {
            Start::Fresh
        }
    }
}

/// One saved conversation, as `session/list` lists it.
pub(super) struct Saved {
    id: String,
    pub path: String,
    pub title: String,
    updated: String,
    /// The one the running agent holds.
    current: bool,
}

/// This run's arguments for the agent process, which parses them the same
/// way: `(base, start)`. `start` holds the session choice (`-c`,
/// `--resume <x>`, `--fork <x>`), used for the first process only; `base`
/// holds every other flag. The messages, the `@file` arguments and a bare
/// `--resume` stay with the terminal, and `--cwd` becomes absolute (the
/// agent starts in that folder already).
pub(super) fn agent_args(options: &RunOptions) -> (Vec<String>, Vec<String>) {
    let argv = &options.argv;
    let parsed = crate::args::parse_args(argv);
    let mut messages = parsed.messages.clone();
    let mut base = Vec::new();
    let mut start = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        let next = argv.get(i + 1);
        match arg.as_str() {
            "-c" | "--continue" => start.push(arg.clone()),
            "--resume" | "-r" => match next {
                Some(value)
                    if !value.is_empty() && !value.starts_with('-') && !value.starts_with('@') =>
                {
                    start.extend([arg.clone(), value.clone()]);
                    i += 1;
                }
                Some(value) if value.is_empty() => i += 1,
                _ => {}
            },
            "--fork" => {
                if let Some(value) = next {
                    start.extend([arg.clone(), value.clone()]);
                    i += 1;
                }
            }
            "--cwd" => {
                base.extend([arg.clone(), options.config.cwd.display().to_string()]);
                i += 1;
            }
            _ if arg.starts_with('@') && parsed.file_args.contains(&arg[1..].to_string()) => {}
            _ if arg.starts_with('@') && parsed.file_args.contains(arg) => {}
            _ => {
                if let Some(index) = messages.iter().position(|message| message == arg) {
                    messages.remove(index);
                } else {
                    base.push(arg.clone());
                }
            }
        }
        i += 1;
    }
    (base, start)
}

impl App {
    /// Talk to the freshly started agent and open its session.
    pub(super) fn connect(&mut self, start: Start) -> Result<(), String> {
        self.status = Some("starting the agent".to_string());
        self.dirty = true;
        self.initialize()?;
        let mut expect_saved = start == Start::Resume;
        if start == Start::Pick {
            if let Some(saved) = self.pick_saved()? {
                self.respawn(&["--resume".to_string(), saved.path])?;
                self.initialize()?;
                expect_saved = true;
            }
        }
        // The agent may hold a saved conversation (started with `-c`,
        // `--resume` or `--fork`): load it, which replays it.
        let held = self
            .saved_sessions()?
            .into_iter()
            .find(|saved| saved.current);
        if let Some(held) = held {
            if self.load(&held.id) {
                return Ok(());
            }
        } else if expect_saved {
            self.note(
                "·",
                "no saved conversation to reopen in this folder; starting a new one",
                Style::fg(Color::Yellow),
            );
        }
        let result = self
            .call(
                "session/new",
                &json!({"cwd": self.cwd.display().to_string(), "mcpServers": []}),
            )
            .map_err(|RpcError(error)| format!("the agent did not open a session: {error}"))?;
        self.open_session(&result);
        Ok(())
    }

    /// Open the saved session the agent holds; its turns replay as updates
    /// before the answer.
    fn load(&mut self, id: &str) -> bool {
        self.transcript.replaying = true;
        let loaded = self.call(
            "session/load",
            &json!({"cwd": self.cwd.display().to_string(), "mcpServers": [], "sessionId": id}),
        );
        let lines = self.transcript.flush(self.width());
        self.commit(Block::Text, lines);
        self.transcript.replaying = false;
        match loaded {
            Ok(result) => {
                self.open_session(&result);
                self.note(
                    "↺",
                    "conversation resumed",
                    Style::fg(Color::Accent).dimmed(),
                );
                true
            }
            Err(RpcError(error)) => {
                self.note(
                    "✗",
                    &format!("could not reopen the conversation ({error}); starting a new one"),
                    Style::fg(Color::Red),
                );
                false
            }
        }
    }

    fn initialize(&mut self) -> Result<(), String> {
        self.call(
            "initialize",
            &json!({
                "protocolVersion": 1,
                "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
                "clientInfo": {"name": "unharnessed-terminal", "title": "Unharnessed terminal", "version": crate::config::version()}
            }),
        )
        .map(|_| ())
        .map_err(|RpcError(error)| format!("the agent did not start: {error}"))
    }

    fn open_session(&mut self, result: &Value) {
        self.session_id = result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string);
        self.saved_path = result
            .pointer("/_meta/unharnessed/saved/path")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(options) = result.get("configOptions").and_then(Value::as_array) {
            self.config.clone_from(options);
        }
        self.status = None;
        self.refresh_providers();
        self.dirty = true;
    }

    /// Replace the agent process (the terminal stays as it is). `extra`
    /// adds the session choice, if any.
    fn respawn(&mut self, extra: &[String]) -> Result<(), String> {
        self.agent.shutdown(Duration::from_secs(10));
        let mut args = self.base_args.clone();
        args.extend_from_slice(extra);
        self.agent = Agent::spawn(&args, &self.cwd)
            .map_err(|error| format!("cannot start the agent: {error}"))?;
        self.dead = false;
        self.session_id = None;
        self.saved_path = None;
        self.awaiting.clear();
        self.responses.clear();
        self.login_updates.clear();
        self.turn = None;
        self.prompt_request = None;
        self.usage = None;
        self.background = 0;
        self.goal_status = None;
        self.transcript = Transcript::new();
        Ok(())
    }

    fn saved_sessions(&mut self) -> Result<Vec<Saved>, String> {
        let result = self
            .call(
                "session/list",
                &json!({"cwd": self.cwd.display().to_string()}),
            )
            .map_err(|RpcError(error)| format!("cannot list the saved conversations: {error}"))?;
        let rows = result
            .get("sessions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(rows
            .iter()
            .filter_map(|row| {
                let text = |key: &str| row.get(key).and_then(Value::as_str).map(str::to_string);
                Some(Saved {
                    id: text("sessionId")?,
                    path: row
                        .pointer("/_meta/unharnessed/path")?
                        .as_str()?
                        .to_string(),
                    title: text("title").unwrap_or_else(|| "(no title)".to_string()),
                    updated: text("updatedAt").unwrap_or_default(),
                    current: row
                        .pointer("/_meta/unharnessed/current")
                        .and_then(Value::as_bool)
                        == Some(true),
                })
            })
            .collect())
    }

    pub(super) fn pick_saved(&mut self) -> Result<Option<Saved>, String> {
        let mut saved = self.saved_sessions()?;
        if saved.is_empty() {
            self.note(
                "·",
                "no saved conversation in this folder yet",
                Style::dim(),
            );
            return Ok(None);
        }
        let items: Vec<Item> = saved
            .iter()
            .map(|s| Item::new(s.title.clone(), ago(&s.updated)).marked(s.current))
            .collect();
        let picker = Picker::new("Resume a conversation", items).mark_symbol("•");
        Ok(self
            .pick(picker)
            .and_then(|chosen| chosen.first().copied())
            .map(|index| saved.swap_remove(index)))
    }

    /// `/new` (no `path`) or `/resume` (the chosen conversation's file).
    pub(super) fn restart(&mut self, path: Option<String>, banner: &str) {
        if self.turn.is_some() {
            self.cancel_turn();
        }
        self.queue.clear();
        self.status = Some("saving the conversation".to_string());
        self.draw(true);
        let extra = path
            .map(|path| vec!["--resume".to_string(), path])
            .unwrap_or_default();
        let start = if extra.is_empty() {
            Start::Fresh
        } else {
            Start::Resume
        };
        if let Err(error) = self.respawn(&extra) {
            self.status = None;
            self.note("✗", &error, Style::fg(Color::Red));
            self.dead = true;
            return;
        }
        let rule = format!("── {banner} ");
        let width = self.width();
        let tail = "─".repeat(width.saturating_sub(rule.chars().count() + 4).min(40));
        let line = Line::plain("  ")
            .with(format!("{rule}{tail}"), Style::dim())
            .truncate(width);
        self.commit(Block::Notice, vec![Line::new(), line, Line::new()]);
        if let Err(error) = self.connect(start) {
            self.status = None;
            self.note("✗", &error, Style::fg(Color::Red));
            self.dead = true;
        }
    }

    /// The working folder as the header shows it (`~/…` under the home).
    pub(super) fn short_cwd(&self) -> String {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        match home
            .as_deref()
            .and_then(|home| self.cwd.strip_prefix(home).ok())
        {
            Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Some(rest) => Path::new("~").join(rest).display().to_string(),
            None => self.cwd.display().to_string(),
        }
    }
}

/// "3 min ago", "yesterday", from an ISO timestamp.
fn ago(iso: &str) -> String {
    let Some(then) = iso_epoch_seconds(iso) else {
        return iso.to_string();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let secs = now.saturating_sub(then);
    match secs {
        0..60 => "just now".to_string(),
        60..3_600 => format!("{} min ago", secs / 60),
        3_600..86_400 => format!("{} h ago", secs / 3_600),
        86_400..172_800 => "yesterday".to_string(),
        _ => format!("{} days ago", secs / 86_400),
    }
}

/// Seconds since the epoch of an ISO 8601 UTC timestamp
/// (`2026-11-16T10:00:00.123Z`, or with a `+00:00` offset).
fn iso_epoch_seconds(iso: &str) -> Option<u64> {
    let number = |range: std::ops::Range<usize>| iso.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hour * 3_600 + minute * 60 + second).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_timestamps_parse() {
        assert_eq!(iso_epoch_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            iso_epoch_seconds("2026-11-16T10:00:00.123+00:00"),
            Some(1_794_823_200)
        );
        assert_eq!(
            iso_epoch_seconds("2024-02-29T23:59:59Z"),
            Some(1_709_251_199)
        );
        assert_eq!(iso_epoch_seconds("yesterday"), None);
    }

    fn options(argv: &[&str]) -> RunOptions {
        let argv: Vec<String> = argv.iter().map(|arg| (*arg).to_string()).collect();
        let parsed = crate::args::parse_args(&argv);
        let mut options = RunOptions::for_terminal_tests(argv);
        options.session.resume_bare = parsed.resume_bare;
        options.config.cwd = std::path::PathBuf::from("/abs/project");
        options
    }

    #[test]
    fn the_agent_gets_every_flag_but_the_terminal_ones() {
        let (base, start) = agent_args(&options(&[
            "--model",
            "gpt-5",
            "--no-tools",
            "--api-key",
            "sk-x",
            "-c",
            "fix the test",
        ]));
        assert_eq!(
            base,
            ["--model", "gpt-5", "--no-tools", "--api-key", "sk-x"]
        );
        assert_eq!(start, ["-c"]);
    }

    #[test]
    fn session_choice_and_cwd_are_split_out() {
        let (base, start) = agent_args(&options(&["--resume", "abc", "--cwd", "sub"]));
        assert_eq!(base, ["--cwd", "/abs/project"]);
        assert_eq!(start, ["--resume", "abc"]);
        let (base, start) = agent_args(&options(&["-r", "--thinking", "high"]));
        assert_eq!(base, ["--thinking", "high"]);
        assert!(start.is_empty());
        let (base, start) = agent_args(&options(&["--fork", "old.jsonl", "@notes.md", "go"]));
        assert!(base.is_empty());
        assert_eq!(start, ["--fork", "old.jsonl"]);
    }

    #[test]
    fn a_message_equal_to_a_flag_value_keeps_the_flag_value() {
        let (base, _) = agent_args(&options(&["--model", "hi", "hi"]));
        assert_eq!(base, ["--model", "hi"]);
    }
}
