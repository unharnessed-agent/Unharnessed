//! The saved conversations: one session file each in the sessions folder
//! (`<agent dir>/sessions/<id>.jsonl`), written as the session runs.
//!
//! - `session/list` `{ cwd? }` answers `{ sessions }`, the newest first, the
//!   ones started in `cwd` only when it is given. Each is `{ sessionId, cwd,
//!   title?, updatedAt, _meta.unharnessed: { path, current? } }`: `title` is
//!   the start of its first prompt; `updatedAt` its file's last write;
//!   `path` its file, which `--resume` opens; `current` marks this process's
//!   own. A session nothing was asked in has no file yet, so it is not
//!   listed.
//! - `session/load` (`mod.rs`) replays the session this process was started
//!   on (`--resume`) as `session/update`s, one turn after the other: what
//!   started it (`turnStart`, then the prompt's `user_message_chunk`s), what
//!   the agent did (as it streamed live, without the reasoning's timing),
//!   then how it ended (`turnEnd`).
//! - `_unharnessed/sessions/delete` `{ sessionId }` removes a session's file
//!   and its artifacts (the kernel's saved state). It answers `{}`, or is
//!   refused with `-32602` and `data.details`: a session another process
//!   holds is not touched, and this process's own is not deleted.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use serde_json::{json, Value};
use uh_core::session::manager::format_iso;
use uh_core::session_engine::engine::SessionEngine;
use uh_types::ai::{AssistantContentBlock, StopReason, UserContent, UserContentBlock};
use uh_types::session::{AgentMessage, FileEntry};
use uh_types::slash_commands::{
    SESSION_SLASH_COMMAND_CUSTOM_TYPE, SESSION_SLASH_COMMAND_RESULT_CUSTOM_TYPE,
};

use super::events::{acp_updates_for_event, AcpEngineEvent, MappingState};
use super::jsonrpc;
use super::meta::{
    unharnessed_meta, UnharnessedCommandMeta, UnharnessedSessionMeta, UnharnessedTurnEndMeta,
    UnharnessedTurnStartMeta,
};
use super::producer::FrameSink;
use super::session::STEER_CUSTOM_TYPE;
use super::types::AcpSessionUpdate;

pub(super) const DELETE: &str = "_unharnessed/sessions/delete";

/// How much of the first prompt a title keeps.
const TITLE_CHARS: usize = 80;

/// The saved sessions in `session_dir`, the newest first; only those
/// started in `cwd` when it is given. `current` is this process's session.
pub(super) fn list(session_dir: &Path, cwd: Option<&Path>, current: &str) -> Vec<Value> {
    let Ok(files) = std::fs::read_dir(session_dir) else {
        return Vec::new();
    };
    let mut saved: Vec<(SystemTime, Value)> = files
        .flatten()
        .map(|file| file.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .filter_map(|path| {
            let session = read_saved(&path)?;
            if cwd.is_some_and(|cwd| !super::same_cwd(Path::new(&session.cwd), cwd)) {
                return None;
            }
            let mut meta = json!({ "path": path.display().to_string() });
            if session.id == current {
                meta["current"] = json!(true);
            }
            let millis = session
                .modified
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |since| since.as_millis() as i64);
            let mut row = json!({
                "sessionId": session.id,
                "cwd": session.cwd,
                "updatedAt": format_iso(millis),
                "_meta": { "unharnessed": meta },
            });
            if let Some(title) = session.title {
                row["title"] = json!(title);
            }
            Some((session.modified, row))
        })
        .collect();
    saved.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    saved.into_iter().map(|(_, row)| row).collect()
}

/// What the list shows of one session file.
struct Saved {
    id: String,
    cwd: String,
    title: Option<String>,
    modified: SystemTime,
}

/// The header and the first prompt of a session file; `None` when it is
/// not one, or nothing was asked in it.
fn read_saved(path: &Path) -> Option<Saved> {
    let file = std::fs::File::open(path).ok()?;
    let modified = file.metadata().and_then(|meta| meta.modified()).ok()?;
    let mut lines = BufReader::new(file).lines();
    let FileEntry::Header { header } = serde_json::from_str(&lines.next()?.ok()?).ok()? else {
        return None;
    };
    // Only a line that may be the first prompt is parsed, and the file is
    // read up to it: the answers and the cells' output come after.
    let prompt = lines
        .map_while(Result::ok)
        .filter(|line| {
            line.contains("\"user\"") || line.contains(SESSION_SLASH_COMMAND_CUSTOM_TYPE)
        })
        .find_map(
            |line| match serde_json::from_str::<FileEntry>(&line).ok()? {
                FileEntry::Message {
                    message: AgentMessage::User(user),
                    ..
                } => Some(user.content.text()),
                FileEntry::CustomMessage { payload, .. }
                    if payload.custom_type == SESSION_SLASH_COMMAND_CUSTOM_TYPE =>
                {
                    Some(payload.content.text())
                }
                _ => None,
            },
        )?;
    Some(Saved {
        id: header.id,
        cwd: header.cwd,
        title: title_of(&prompt),
        modified,
    })
}

/// The prompt on one line, cut to `TITLE_CHARS`; `None` when it has no text
/// (images only).
fn title_of(prompt: &str) -> Option<String> {
    let line = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        return None;
    }
    if line.chars().count() <= TITLE_CHARS {
        return Some(line);
    }
    let cut: String = line.chars().take(TITLE_CHARS - 1).collect();
    Some(format!("{}…", cut.trim_end()))
}

/// The conversation on the branch that ends at `leaf` (else at the last
/// entry), as the updates of each of its turns, in order.
pub(super) fn replay(entries: &[FileEntry], leaf: Option<&str>) -> Vec<Vec<AcpSessionUpdate>> {
    let by_id: std::collections::HashMap<&str, &FileEntry> = entries
        .iter()
        .filter_map(|entry| Some((entry.id()?, entry)))
        .collect();
    let mut at = leaf
        .filter(|leaf| by_id.contains_key(leaf))
        .or_else(|| entries.iter().rev().find_map(FileEntry::id));
    let mut branch = Vec::new();
    // At most every entry once: a damaged file's parents may loop.
    while let Some(entry) = at.and_then(|id| by_id.get(id)) {
        if branch.len() == by_id.len() {
            break;
        }
        branch.push(*entry);
        at = entry.parent_id();
    }
    branch.reverse();

    let mut replay = Replay::default();
    for entry in branch {
        replay.rewind_to = entry.parent_id().map(str::to_string);
        match entry {
            FileEntry::Message { message, .. } => replay.message(message),
            FileEntry::CustomMessage { payload, .. } => replay.custom(
                &payload.custom_type,
                &payload.content,
                payload.display,
                payload.details.as_ref(),
            ),
            FileEntry::Custom { payload, .. } if payload.custom_type == STEER_CUSTOM_TYPE => {
                replay.steered = true;
            }
            FileEntry::Compaction { payload, .. } => {
                replay.push_event(&AcpEngineEvent::CompactionEnd {
                    tokens_before: Some(payload.tokens_before),
                    summary: Some(payload.summary.clone()),
                });
                // It is `/compact`'s answer: its turn is over, as a command's is.
                if replay.command {
                    replay.end = Some(("done", None));
                    replay.open = false;
                }
            }
            _ => {}
        }
    }
    replay.finish();
    replay.turns
}

/// The turns of a conversation as its entries are read.
#[derive(Default)]
struct Replay {
    turns: Vec<Vec<AcpSessionUpdate>>,
    /// The last turn is under way: its prompt is not answered yet.
    open: bool,
    /// How the last turn ended, and why if it failed; published once
    /// nothing more joins it (a retry after a failure, a continuation).
    end: Option<(&'static str, Option<String>)>,
    mapping: MappingState,
    prompts: u64,
    /// The parent of the entry being read: where a prompt's turn goes back.
    rewind_to: Option<String>,
    /// The next prompt was steered into the turn under way.
    steered: bool,
    /// The last turn is a slash command's.
    command: bool,
}

impl Replay {
    fn message(&mut self, message: &AgentMessage) {
        match message {
            AgentMessage::User(user) => {
                let text = user.content.text();
                if text == uh_core::session_engine::provider_park::QUOTA_RESUME_MARKER_TEXT {
                    self.start(Some("quota"));
                } else if text
                    == uh_core::session_engine::goal_driver::GOAL_BACKOFF_WAKE_MARKER_TEXT
                {
                    self.start(Some("goal"));
                } else {
                    // The turn it was steered into went on in its own.
                    if self.open && self.steered {
                        self.end = Some(("done", None));
                    }
                    self.steered = false;
                    self.start_prompt(&user.content);
                }
            }
            AgentMessage::Custom(custom) => self.custom(
                &custom.custom_type,
                &custom.content,
                custom.display,
                custom.details.as_ref(),
            ),
            AgentMessage::Assistant(assistant) => {
                self.go_on();
                let mut events = vec![AcpEngineEvent::MessageStart {
                    role: "assistant".to_string(),
                }];
                for block in &assistant.content {
                    events.push(match block {
                        AssistantContentBlock::Thinking(thinking) => {
                            AcpEngineEvent::AssistantDelta {
                                thinking: true,
                                delta: thinking.thinking.clone(),
                            }
                        }
                        AssistantContentBlock::Text(text) => AcpEngineEvent::AssistantDelta {
                            thinking: false,
                            delta: text.text.clone(),
                        },
                        AssistantContentBlock::ToolCall(call) => {
                            AcpEngineEvent::ToolExecutionStart {
                                tool_call_id: call.id.clone(),
                                tool_name: call.name.clone(),
                                args: Value::Object(call.arguments.clone()),
                            }
                        }
                    });
                }
                events.push(AcpEngineEvent::MessageEnd {
                    role: "assistant".to_string(),
                });
                for event in &events {
                    self.push_event(event);
                }
                let end = match assistant.stop_reason {
                    StopReason::ToolUse => return,
                    StopReason::Stop | StopReason::Length => ("done", None),
                    StopReason::Aborted => ("cancelled", None),
                    StopReason::Error => (
                        "failed",
                        Some(
                            assistant
                                .error_message
                                .clone()
                                .unwrap_or_else(|| "The turn failed.".to_string()),
                        ),
                    ),
                };
                self.end = Some(end);
                self.open = false;
            }
            AgentMessage::ToolResult(result) => {
                self.push_event(&AcpEngineEvent::ToolExecutionEnd {
                    tool_call_id: result.tool_call_id.clone(),
                    tool_name: result.tool_name.clone(),
                    result: json!({ "content": result.content, "details": result.details }),
                    is_error: result.is_error,
                });
            }
            AgentMessage::BashExecution(_) | AgentMessage::CompactionSummary(_) => {}
        }
    }

    /// A row the agent wrote: a slash command and its answer, what woke it,
    /// a goal's continuation, an answer cut at the output limit; the others
    /// are its own bookkeeping.
    fn custom(
        &mut self,
        kind: &str,
        content: &UserContent,
        display: bool,
        details: Option<&Value>,
    ) {
        match kind {
            SESSION_SLASH_COMMAND_CUSTOM_TYPE => {
                self.start_prompt(content);
                self.command = true;
            }
            SESSION_SLASH_COMMAND_RESULT_CUSTOM_TYPE if display => {
                let command = details.and_then(|details| details.get("command"));
                let name = command
                    .and_then(|command| command.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let severity = details
                    .and_then(|details| details.get("severity"))
                    .and_then(Value::as_str)
                    .unwrap_or("info");
                self.go_on();
                let update = info(&UnharnessedSessionMeta {
                    command: Some(UnharnessedCommandMeta {
                        name: name.to_string(),
                        text: content.text(),
                        severity: severity.to_string(),
                    }),
                    ..Default::default()
                });
                self.last().push(update);
                // Its answer ends the command's turn, unless the model goes
                // on in it (a goal it set).
                self.end = Some(("done", None));
                self.open = false;
            }
            // A background command that ended while the agent was idle woke it.
            uh_core::session_engine::messages::ASYNC_BASH_COMPLETION_CUSTOM_TYPE if !self.open => {
                self.start(Some("bash"));
            }
            uh_core::goals::GOAL_CONTEXT_CUSTOM_TYPE if !self.open => self.go_on(),
            // The provider cut the answer at the output limit: the turn
            // went on.
            uh_core::session_engine::messages::OUTPUT_LIMIT_CUSTOM_TYPE => {
                self.go_on();
                self.push_event(&AcpEngineEvent::OutputLimit);
            }
            _ => {}
        }
    }

    /// A prompt's turn: it goes back to where the conversation stood
    /// before the prompt.
    fn start_prompt(&mut self, content: &UserContent) {
        let rewind_to = self.rewind_to.take();
        self.open_turn(None, rewind_to);
        self.prompt(content);
    }

    /// A new turn: a prompt's, or one the agent started on its own (`wake`).
    fn start(&mut self, wake: Option<&str>) {
        self.open_turn(wake, None);
    }

    fn open_turn(&mut self, wake: Option<&str>, rewind_to: Option<String>) {
        self.finish();
        self.command = false;
        let origin = if wake.is_some() { "wake" } else { "prompt" };
        self.turns.push(vec![info(&UnharnessedSessionMeta {
            turn_start: Some(UnharnessedTurnStartMeta {
                origin: origin.to_string(),
                wake: wake.map(str::to_string),
                rewind_to,
            }),
            ..Default::default()
        })]);
        self.open = true;
    }

    /// The prompt's text and images.
    fn prompt(&mut self, content: &UserContent) {
        self.prompts += 1;
        let message_id = format!("unharnessed-user-{}", self.prompts);
        let blocks: Vec<Value> = match content {
            UserContent::Text(text) => vec![json!({ "type": "text", "text": text })],
            UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    UserContentBlock::Image(image) => Some(json!({
                        "type": "image",
                        "mimeType": image.mime_type,
                        "data": image.data,
                    })),
                    other => other
                        .text()
                        .map(|text| json!({ "type": "text", "text": text })),
                })
                .collect(),
        };
        let turn = self.last();
        for content in blocks {
            turn.push(AcpSessionUpdate::UserMessageChunk {
                message_id: message_id.clone(),
                content,
            });
        }
    }

    /// The agent goes on: in the turn under way, else in the one that just
    /// ended (a retry, a continuation), else in a turn of its own.
    fn go_on(&mut self) {
        if self.open {
            return;
        }
        if self.turns.is_empty() {
            self.start(None);
        } else {
            self.end = None;
            self.open = true;
        }
    }

    /// What the agent did, as it streamed live, into the last turn.
    fn push_event(&mut self, event: &AcpEngineEvent) {
        let updates = acp_updates_for_event(event, &mut self.mapping);
        if updates.is_empty() {
            return;
        }
        self.last().extend(updates);
    }

    fn last(&mut self) -> &mut Vec<AcpSessionUpdate> {
        if self.turns.is_empty() {
            self.start(None);
        }
        self.turns.last_mut().expect("a turn was started")
    }

    /// The last turn ends: as it did, or stopped when it was cut short (the
    /// process ended in it).
    fn finish(&mut self) {
        let end = self.end.take().or(self.open.then_some(("cancelled", None)));
        self.open = false;
        if let (Some((state, error)), Some(turn)) = (end, self.turns.last_mut()) {
            turn.push(info(&UnharnessedSessionMeta {
                turn_end: Some(UnharnessedTurnEndMeta {
                    state: state.to_string(),
                    error,
                }),
                ..Default::default()
            }));
        }
    }
}

fn info(meta: &UnharnessedSessionMeta) -> AcpSessionUpdate {
    AcpSessionUpdate::SessionInfoUpdate {
        meta: unharnessed_meta(meta),
    }
}

/// Serve `_unharnessed/sessions/delete`; `false` when `method` is another.
pub(super) async fn handle_request(
    id: &Value,
    method: &str,
    params: &Value,
    engine: &Arc<SessionEngine>,
    agent_dir: &Arc<PathBuf>,
    tx: &FrameSink,
) -> bool {
    if method != DELETE {
        return false;
    }
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let _ = tx.send(match delete(engine, agent_dir, session_id).await {
        Ok(()) => jsonrpc::response(id, &json!({})),
        Err(details) => jsonrpc::error_response(
            id,
            jsonrpc::INVALID_PARAMS,
            "Invalid params",
            Some(&json!({ "details": details })),
        ),
    });
    true
}

async fn delete(
    engine: &Arc<SessionEngine>,
    agent_dir: &Arc<PathBuf>,
    session_id: String,
) -> Result<(), String> {
    let session_dir = {
        let session = engine.session.shared_persistence();
        let session = session.lock().await;
        if session.get_session_id() == session_id {
            return Err(
                "This is the conversation in progress: start a new one to delete it.".to_string(),
            );
        }
        session.get_session_dir().to_path_buf()
    };
    let agent_dir = Arc::clone(agent_dir);
    blocking(move || {
        let path = saved_file(&session_dir, &session_id)?;
        let _lease = hold(&path, &agent_dir)?;
        std::fs::remove_file(&path).map_err(|error| format!("It was not deleted: {error}"))?;
        // The kernel's saved state, beside the sessions folder.
        if let Some(root) = session_dir.parent() {
            let artifacts = root.join("session-artifacts").join(&session_id);
            if artifacts.is_dir() {
                let _ = std::fs::remove_dir_all(artifacts);
            }
        }
        Ok(())
    })
    .await
}

/// The file of the saved session `session_id`.
fn saved_file(session_dir: &Path, session_id: &str) -> Result<PathBuf, String> {
    let well_formed = !session_id.is_empty()
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-');
    let path = session_dir.join(format!("{session_id}.jsonl"));
    if !well_formed || !path.is_file() {
        return Err(format!("No saved conversation {session_id}."));
    }
    Ok(path)
}

/// The session file's lease, held while it is changed: refused when
/// another process holds it.
fn hold(path: &Path, agent_dir: &Path) -> Result<crate::lease::SessionLease, String> {
    crate::lease::acquire_runtime_session_lease(path, agent_dir).map_err(|error| {
        if error
            .downcast_ref::<crate::lease::SessionAlreadyActiveError>()
            .is_some()
        {
            "This conversation is open in another window.".to_string()
        } else {
            format!("The conversation could not be locked: {error:#}")
        }
    })
}

async fn blocking(
    work: impl FnOnce() -> Result<(), String> + Send + 'static,
) -> Result<(), String> {
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|_| Err("The task failed.".to_string()))
}
