//! The prompt turn: admission, session-command execution, the autonomous
//! continuation loop, and the correlated settlement in front of the
//! response. One prompt turn at a time; the turn runs as its own task so
//! the reader loop can keep serving session/cancel and session/close.
//!
//! The agent also starts turns on its own ([`start_wake_turn`]): a detached
//! `bash()` completion, a goal backoff or a quota reset wakes an idle
//! session (`UNHARNESSED_PLAN.md` Q7). Such a turn streams like a prompt
//! turn but answers no request; a client prompt waits for it to settle.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Mutex;

use uh_core::autonomous::{AgentAutonomousStatus, AutonomousFollowUp, AutonomousStopReason};
use uh_core::session_engine::session_commands::execute_session_command;
use uh_core::session_engine::session_commands::SessionCommandParams;
use uh_core::session_engine::turn_retry::{RetriedTurn, TurnRetryHost};
use uh_core::session_engine::wakes::Wake;
use uh_core::session_engine::{PromptOptions, PromptOutcome, StreamingBehavior};

use super::compaction_arms::{CompactionCheckRun, ThresholdGoalQueue};
use super::goal_continuation;
use super::internal_error;
use super::meta::{self, UnharnessedAutonomousMeta, UnharnessedEventPhase, UnharnessedOutcome};
use super::session::{self, AcpSession, TurnBoundary};
use super::stop_reason_response;
use super::types::{AcpSessionUpdate, PromptParams};
use super::{events, jsonrpc, producer, AcpModeState, AcpStopReason, ConnectionState};

/// The `_meta.autonomous` accounting for a completion update: shared with
/// the daemon-attached settlement (meta.rs).
fn autonomous_meta(status: &AgentAutonomousStatus) -> UnharnessedAutonomousMeta {
    meta::autonomous_meta(status)
}

pub(super) async fn handle_session_prompt(
    id: Value,
    raw_params: Value,
    state: Arc<Mutex<ConnectionState>>,
    mode: AcpModeState,
    tx: producer::FrameSink,
) {
    if admit_session_prompt(&id, &raw_params, &state, &mode, &tx).await {
        return;
    }
    // A wake turn the agent started holds the session: the prompt waits
    // for it on its own task (the reader keeps serving session/cancel),
    // then is admitted again.
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            if !wake_turn_running(&state).await
                && admit_session_prompt(&id, &raw_params, &state, &mode, &tx).await
            {
                break;
            }
        }
    });
}

/// Admit one `session/prompt`. `false` when a wake turn holds the session:
/// nothing was answered, the caller admits again once it settles. The slot
/// is taken under one state lock, so a wake turn cannot start in between.
async fn admit_session_prompt(
    id: &Value,
    raw_params: &Value,
    state: &Arc<Mutex<ConnectionState>>,
    mode: &AcpModeState,
    tx: &producer::FrameSink,
) -> bool {
    let params = PromptParams::parse(raw_params);
    // Admission: one prompt turn at a time, behind any started cancellation.
    let mut guard = state.lock().await;
    let closing = guard.session_close_in_flight;
    let Some(entry) = guard.session.as_mut() else {
        let _ = tx.send(internal_error(
            id,
            &format!("Unknown ACP session: {}", params.session_id),
        ));
        return true;
    };
    if closing {
        let _ = tx.send(internal_error(
            id,
            &format!("ACP session is closing: {}", params.session_id),
        ));
        return true;
    }
    if entry.prompt_task.is_some() {
        if entry.prompt_is_wake {
            return false;
        }
        let _ = tx.send(internal_error(
            id,
            "A prompt turn is already running for this ACP session",
        ));
        return true;
    }
    let session = entry.session.clone();
    let turn_id = session.producer().begin_prompt().await;
    if session.cancel_requested() {
        // This prompt was admitted after a cancellation started; it is
        // dropped by the cancel, so report the protocol stop reason instead
        // of a request error.
        session.producer().finish_prompt(turn_id).await;
        let _ = tx.send(jsonrpc::response(
            id,
            &stop_reason_response(AcpStopReason::Cancelled),
        ));
        return true;
    }

    let admitted_prompt = match session::AdmittedPrompt::parse(&params.prompt) {
        Ok(prompt) => prompt,
        Err(error) => {
            session.producer().finish_prompt(turn_id).await;
            let _ = tx.send(session::prompt_block_error(id, &error));
            return true;
        }
    };
    if let Some(refusal) = image_refusal(mode, &admitted_prompt.images) {
        session.producer().finish_prompt(turn_id).await;
        let _ = tx.send(jsonrpc::error_response(
            id,
            jsonrpc::INVALID_PARAMS,
            "Invalid params",
            Some(&serde_json::json!({ "details": refusal })),
        ));
        return true;
    }

    // The turn runs as its own task so the reader loop can keep serving
    // session/cancel and session/close while it settles.
    entry.prompt_task = Some(tokio::spawn(run_prompt_turn(
        TurnStart::Prompt {
            id: id.clone(),
            prompt: admitted_prompt,
        },
        params.session_id.clone(),
        turn_id,
        session,
        state.clone(),
        mode.clone(),
        tx.clone(),
    )));
    entry.prompt_is_wake = false;
    true
}

/// The base64 payload an image may carry under `images.autoResize`: the
/// CLI's `@file` limit, under the providers' 5 MB.
const MAX_IMAGE_BASE64_BYTES: usize = 4_500_000;

/// Why the prompt's images cannot go, under the image settings read now
/// (they act on the next prompt, like the other live settings):
/// `images.blockImages` refuses them all; under `images.autoResize` an
/// image past the limit is refused, the agent having nothing to shrink it
/// with (the client shrinks it first).
pub(super) fn image_refusal(
    mode: &AcpModeState,
    images: &[super::types::ImageBlock],
) -> Option<String> {
    if images.is_empty() {
        return None;
    }
    let settings = uh_core::settings::SettingsManager::create(
        mode.actual_cwd.as_path(),
        mode.agent_dir.as_path(),
    );
    if settings.get_block_images() {
        return Some(
            "Images are blocked: turn off \"Block images\" in the settings to send them."
                .to_string(),
        );
    }
    let largest = images.iter().map(|image| image.data.len()).max()?;
    if settings.get_image_auto_resize() && largest > MAX_IMAGE_BASE64_BYTES {
        return Some(format!(
            "An image is {:.1} MB once encoded, over the {:.1} MB limit: shrink it, or turn off \"Shrink large images\" in the settings to send it as it is.",
            megabytes(largest),
            megabytes(MAX_IMAGE_BASE64_BYTES)
        ));
    }
    None
}

#[allow(clippy::cast_precision_loss)]
fn megabytes(bytes: usize) -> f64 {
    bytes as f64 / 1_000_000.0
}

async fn wake_turn_running(state: &Arc<Mutex<ConnectionState>>) -> bool {
    state
        .lock()
        .await
        .session
        .as_ref()
        .is_some_and(|entry| entry.prompt_task.is_some() && entry.prompt_is_wake)
}

/// What starts a prompt turn.
pub(super) enum TurnStart {
    /// A client `session/prompt`; its JSON-RPC id answers the turn.
    Prompt {
        id: Value,
        prompt: session::AdmittedPrompt,
    },
    /// A wake the agent runs on its own: no request waits for it.
    Wake(Wake),
}

/// Start a wake turn when the session is open and idle. The wake comes
/// back when a prompt turn holds the session (it runs once that turn has
/// answered), the session is closing or cancelled, or none is open.
pub(super) async fn start_wake_turn(
    wake: Wake,
    state: &Arc<Mutex<ConnectionState>>,
    mode: &AcpModeState,
    tx: &producer::FrameSink,
) -> Result<(), Wake> {
    let mut guard = state.lock().await;
    let closing = guard.session_close_in_flight;
    let Some(entry) = guard.session.as_mut() else {
        return Err(wake);
    };
    if closing || entry.prompt_task.is_some() || entry.session.cancel_requested() {
        return Err(wake);
    }
    let turn_id = entry.session.producer().begin_prompt().await;
    // The slot is set under the same lock the turn's settlement takes to
    // clear it, so a quick turn can never leave a finished task behind.
    entry.prompt_task = Some(tokio::spawn(run_prompt_turn(
        TurnStart::Wake(wake),
        entry.session.id.clone(),
        turn_id,
        entry.session.clone(),
        state.clone(),
        mode.clone(),
        tx.clone(),
    )));
    entry.prompt_is_wake = true;
    Ok(())
}

/// Admit a wake's turn: a `bash()` completion runs on its injected row, a
/// timed wake on its marker prompt, a goal's finished wait on its continuation
/// (a turn may have delivered it meanwhile: nothing runs then).
async fn admit_wake(
    mode: &AcpModeState,
    session: &AcpSession,
    wake: Wake,
) -> anyhow::Result<PromptOutcome> {
    match wake {
        Wake::BashDone(messages) => {
            mode.engine
                .session
                .prompt_injected_messages(&messages)
                .await
        }
        Wake::Marker(text) => {
            mode.engine
                .session
                .prompt(text, PromptOptions::default())
                .await
        }
        Wake::GoalContinuation => match mode.engine.take_owed_goal_continuation().await {
            Some(message) => prompt_goal_continuation(mode, session, &message, true).await,
            None => Ok(PromptOutcome::Prompt),
        },
    }
}

/// Withdraw a cancelled, unadmitted mint. Start/resume contexts and budget
/// steers do not spend a continuation slot and must not roll one back.
async fn prompt_goal_continuation(
    mode: &AcpModeState,
    session: &AcpSession,
    message: &uh_types::session::CustomMessage,
    minted: bool,
) -> anyhow::Result<PromptOutcome> {
    if session.cancel_requested() {
        if minted {
            goal_continuation::rollback_goal_mint(mode).await;
        }
        return Err(uh_agent::abort::aborted_error());
    }
    mode.engine.session.prompt_injected_message(message).await
}

/// Re-issue a failed turn under the provider retry/failover policy. The
/// chain's future is not `Send` (its quota-park seam), so it runs on a
/// blocking thread; the session's cancel stops its backoff waits.
async fn retry_failed_turn(
    mode: &AcpModeState,
    session: &Arc<AcpSession>,
    failed: &uh_types::ai::AssistantMessage,
) -> anyhow::Result<RetriedTurn> {
    let failed = uh_core::session_engine::provider_adapter::json_round_trip(failed)
        .ok_or_else(|| anyhow::anyhow!("the failed turn could not be converted for a retry"))?;
    let engine = Arc::clone(&mode.engine);
    let cwd = std::path::PathBuf::clone(&mode.actual_cwd);
    let agent_dir = std::path::PathBuf::clone(&mode.agent_dir);
    let provider_target = Arc::clone(&mode.provider_target);
    let session = Arc::clone(session);
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        runtime.block_on(async {
            let host = TurnRetryHost {
                cwd: &cwd,
                agent_dir: &agent_dir,
                provider_target: &provider_target,
                failover: std::env::var_os("UNHARNESSED_FAUX_SCRIPT").is_none(),
                max_park: None,
            };
            // ACP has no retry update; the outcome row persists.
            engine
                .retry_failed_turn(&host, failed, &|| session.cancel_requested(), &mut |_| {})
                .await
        })
    })
    .await
    .map_err(|error| anyhow::anyhow!("the provider retry task failed: {error}"))?
}

/// One prompt turn: admission into the engine (a model turn or a session
/// command), the autonomous continuation loop, and the correlated boundary /
/// completion envelope in front of the response (a wake turn answers no
/// request).
#[allow(clippy::too_many_arguments)]
async fn run_prompt_turn(
    start: TurnStart,
    session_id: String,
    turn_id: u64,
    session: Arc<AcpSession>,
    state: Arc<Mutex<ConnectionState>>,
    mode: AcpModeState,
    tx: producer::FrameSink,
) {
    // Where the conversation stands before the prompt: going back there
    // sends it again, changed.
    let rewind_to = match &start {
        TurnStart::Prompt { .. } => {
            let persistence = mode.engine.session.shared_persistence();
            let session = persistence.lock().await;
            session.get_leaf_id().map(str::to_string)
        }
        TurnStart::Wake(_) => None,
    };
    // What started the turn comes first in it: a client tells its own
    // prompt from a wake by it.
    let turn_start = meta::UnharnessedTurnStartMeta {
        origin: match &start {
            TurnStart::Prompt { .. } => "prompt",
            TurnStart::Wake(_) => "wake",
        }
        .to_string(),
        wake: match &start {
            TurnStart::Prompt { .. } => None,
            TurnStart::Wake(wake) => Some(wake.kind().to_string()),
        },
        rewind_to,
    };
    session
        .producer()
        .publish(
            &AcpSessionUpdate::SessionInfoUpdate {
                meta: meta::unharnessed_meta(&meta::UnharnessedSessionMeta {
                    turn_start: Some(turn_start),
                    ..Default::default()
                }),
            },
            turn_id,
            UnharnessedEventPhase::Event,
            None,
        )
        .await;
    let boundary = TurnBoundary::capture(mode.engine.session.agent()).await;
    // The pre-turn compaction arms (TS `_runPreTurnCompaction`,
    // `beforeModelSelection`): a stale overflow from the previous run
    // recovers on the newly admitted prompt, then a threshold crossing
    // compacts before the turn runs. Session commands never reach the
    // prompt commit in TS, so they never fire the arms; a busy agent
    // queues the prompt below without a session-level boundary (the
    // settled-turn check after the drain covers the turn).
    let session_command = match &start {
        TurnStart::Prompt { prompt, .. } => mode
            .engine
            .session
            .classify_session_command(&prompt.text)
            .is_some(),
        TurnStart::Wake(_) => false,
    };
    if !session_command {
        let busy = mode.engine.session.agent().state().await.is_streaming;
        if !busy {
            session.run_pre_turn_compaction(&mode).await;
        }
    }
    if session.cancel_requested() {
        let id = match &start {
            TurnStart::Prompt { id, .. } => Some(id),
            TurnStart::Wake(_) => None,
        };
        cancelled_before_start(&state, &session, id, &session_id, turn_id, tx).await;
        return;
    }
    let (id, admission) = match start {
        TurnStart::Prompt { id, prompt } => {
            let admission = mode
                .engine
                .session
                .prompt_with_images(
                    &prompt.text,
                    prompt
                        .images
                        .into_iter()
                        .map(|image| uh_agent::types::ImageContent {
                            data: image.data,
                            mime_type: image.mime_type,
                        })
                        .collect(),
                    PromptOptions {
                        streaming_behavior: Some(StreamingBehavior::FollowUp),
                        queue_if_busy: true,
                        ..Default::default()
                    },
                )
                .await;
            (Some(id), admission)
        }
        TurnStart::Wake(wake) => (None, admit_wake(&mode, &session, wake).await),
    };

    // The settled-turn outcome: the failure text (a failed turn errors the
    // request), the autonomous stop, and whether the turn ran at all.
    let mut turn_failure: Option<String> = None;
    let mut autonomous_stop: Option<(
        uh_core::autonomous::AutonomousStopReason,
        Box<AgentAutonomousStatus>,
    )> = None;
    let ran_model_turn;

    match admission {
        // Session commands (compact/goal/autonomous) never admit a
        // model turn: the durable echo row replaces the user row. Execute
        // them and publish their namespaced events.
        Ok(PromptOutcome::SessionCommand(command)) => {
            match run_session_command_segment(&mode, &session, &command, &mut turn_failure).await {
                Ok(ran) => ran_model_turn = ran,
                Err(_) if session.cancel_requested() => {
                    cancelled_before_start(&state, &session, id.as_ref(), &session_id, turn_id, tx)
                        .await;
                    return;
                }
                Err(error) => {
                    // Command execution could not start (no resolved model):
                    // an admission-style error boundary, never an invented
                    // terminal-quiescence update.
                    let _ = session::publish_response_boundary(
                        &session,
                        turn_id,
                        false,
                        UnharnessedOutcome::Error,
                    )
                    .await;
                    session.producer().finish_prompt(turn_id).await;
                    if let Some(id) = &id {
                        let _ = tx.send(internal_error(id, &format!("{error:#}")));
                    }
                    clear_prompt_slot(&state, &session_id).await;
                    return;
                }
            }
        }
        Ok(PromptOutcome::Prompt) => {
            ran_model_turn = true;
        }
        Err(_) if session.cancel_requested() => {
            cancelled_before_start(&state, &session, id.as_ref(), &session_id, turn_id, tx).await;
            return;
        }
        Err(error) => {
            // Failed prompt admission gets one correlated error boundary; it
            // never gets an invented terminal-quiescence update.
            let _ = session::publish_response_boundary(
                &session,
                turn_id,
                false,
                UnharnessedOutcome::Error,
            )
            .await;
            session.producer().finish_prompt(turn_id).await;
            if let Some(id) = &id {
                let _ = tx.send(internal_error(id, &format!("{error:#}")));
            }
            clear_prompt_slot(&state, &session_id).await;
            return;
        }
    }

    if ran_model_turn {
        mode.engine.session.agent().wait_for_idle().await;
    }

    // The turn-settlement loop: classify each settled turn, run the
    // automatic compaction arms at its boundary (TS `_checkCompaction` at
    // `agent_end`), then ask the autonomous driver what follows. A continuation runs as the next turn of the same
    // prompt, so every boundary in the run hosts its arms.
    loop {
        if session.cancel_requested() || !ran_model_turn {
            break;
        }
        let Some(final_message) =
            session::latest_assistant_message(mode.engine.session.agent()).await
        else {
            break;
        };
        // The newest assistant predates the turn: the turn appended
        // none, so there is nothing to classify.
        if boundary.contains_wire(&final_message) {
            break;
        }
        // An aborted turn resets the overflow machine and settles (TS
        // `_checkCompaction`'s abort arm).
        if final_message.stop_reason == uh_types::ai::StopReason::Aborted {
            session.reset_overflow_recovery();
            break;
        }
        // A failed turn first goes through the provider retry/failover
        // chain (TS `_handleRetryableError` precedes `_checkCompaction` at
        // `agent_end`): the retried turn is the one the boundary classifies,
        // and a quota park settles the prompt until its wake resumes it.
        let final_message = if final_message.stop_reason == uh_types::ai::StopReason::Error {
            match retry_failed_turn(&mode, &session, &final_message).await {
                Ok(retried) if retried.parked => break,
                Ok(retried) => {
                    uh_core::session_engine::provider_adapter::json_round_trip(&retried.message)
                        .unwrap_or(final_message)
                }
                Err(error) => {
                    turn_failure = Some(format!("{error:#}"));
                    break;
                }
            }
        } else {
            final_message
        };
        // A settled non-error assistant message resets the overflow
        // machine (TS resets `_overflowRecovery` at every non-error
        // assistant `message_end`), then the boundary check runs.
        if final_message.stop_reason != uh_types::ai::StopReason::Error {
            session.reset_overflow_recovery();
        }
        // TS `_checkCompaction` at `agent_end`: the overflow arm (Case 1,
        // with its compact-and-retry), then the threshold arm (which, under
        // the settled boundary's queue policy, mints the goal continuation
        // before it compacts — the held turn runs after the boundary like
        // the TS post-compaction continue).
        let (check, threshold_continuation) = session
            .check_compaction(&mode, &final_message, ThresholdGoalQueue::Queue)
            .await;
        if session.cancel_requested() {
            if threshold_continuation.is_some() {
                // A cancellation between the mint and the run withdraws
                // the queued continuation (the TS cancel clears the
                // queue and rolls the slot back).
                goal_continuation::rollback_goal_mint(&mode).await;
            }
            break;
        }
        // The overflow arm compacted, or the threshold stopped the run
        // mid tool chain: the compacted context re-issues the turn without
        // a new user message (TS `agent.continue()`); the settled run
        // re-enters this loop through its own boundary.
        if check == CompactionCheckRun::OverflowRetry
            || mode.engine.session.take_threshold_stop().await
        {
            if let Err(error) = mode.engine.session.agent().continue_run().await {
                turn_failure = Some(format!("{error:#}"));
                break;
            }
            mode.engine.session.agent().wait_for_idle().await;
            continue;
        }
        // A failed turn ends the run with its error once the boundary
        // check could not save it (an overflow recovery that re-issued
        // handled it above).
        if final_message.stop_reason == uh_types::ai::StopReason::Error {
            // TS `_finishGoalForTerminalAssistantMessage` at `agent_end`:
            // the failed turn fails an active goal (the state change
            // publishes before the response settles).
            goal_continuation::fail_goal_for_terminal_error(
                &mode,
                &session,
                final_message.error_message.as_deref(),
            )
            .await;
            turn_failure = Some(
                final_message
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "the model request failed".to_string()),
            );
            break;
        }
        // The threshold arm's held goal continuation runs as the
        // post-compaction turn (TS `_schedulePostCompactionContinue` over
        // the queued follow-up): the pre-turn compaction arms run before
        // it like any admitted prompt, and its own settled boundary
        // re-enters this loop.
        if let Some(message) = threshold_continuation {
            if !run_goal_continuation_segment(&mode, &session, message, true, &mut turn_failure)
                .await
            {
                break;
            }
            continue;
        }
        if session.cancel_requested() {
            break;
        }
        // TS `_getContinuationMessages` at the agent loop's natural turn
        // end: the goal arm runs before the autonomous arm with exclusive
        // priority — the budget-limit wrap-up steer first (the turn that
        // crossed the goal budget), then the natural continuation mint
        // (an active goal mints one continuation per settled boundary).
        // A minted turn runs as the next turn of the same prompt (the
        // pre-turn compaction arms run before it like any admitted
        // prompt); no active goal falls through to the autonomous arm.
        match goal_continuation::goal_follow_up(&mode, &session).await {
            goal_continuation::GoalFollowUp::Turn(message) => {
                let minted = message
                    .details
                    .as_ref()
                    .is_some_and(|details| details["kind"] == "continuation");
                if !run_goal_continuation_segment(
                    &mode,
                    &session,
                    *message,
                    minted,
                    &mut turn_failure,
                )
                .await
                {
                    break;
                }
            }
            // A blocked goal hands the session back to the user: no
            // autonomous turn, no wake runs behind it.
            goal_continuation::GoalFollowUp::None
                if mode.engine.goal_state().await.status == uh_types::goal::GoalStatus::Blocked =>
            {
                break;
            }
            goal_continuation::GoalFollowUp::None => {
                // A goal that just completed stops the commands it left
                // running: none of them wakes a session whose work is done.
                mode.engine.stop_after_completed_goal().await;
                match session.autonomous_follow_up(&final_message).await {
                    // A wake that came due during the run (a detached
                    // `bash()` completion, a goal backoff, a quota reset)
                    // waits for this turn to answer: the wake watcher then
                    // runs it as a turn of its own, as a reload shows it.
                    AutonomousFollowUp::Inactive => break,
                    AutonomousFollowUp::Continue { text } => {
                        // An injected continuation runs as the next turn
                        // of the same prompt (a fresh user row: the
                        // pre-turn compaction arms run before it, like
                        // any admitted prompt); its failure settles the
                        // prompt.
                        session.run_pre_turn_compaction(&mode).await;
                        if session.cancel_requested() {
                            break;
                        }
                        if let Err(error) = mode
                            .engine
                            .session
                            .prompt(
                                &text,
                                PromptOptions {
                                    streaming_behavior: Some(StreamingBehavior::FollowUp),
                                    queue_if_busy: true,
                                    ..Default::default()
                                },
                            )
                            .await
                        {
                            turn_failure = Some(format!("{error:#}"));
                            break;
                        }
                        mode.engine.session.agent().wait_for_idle().await;
                    }
                    AutonomousFollowUp::Stop { reason, status } => {
                        // The stop writes no row (the TS shape): the stop
                        // reason and status ride the completion update and
                        // the response's stop reason.
                        autonomous_stop = Some((reason, status));
                        break;
                    }
                }
            }
        }
    }

    settle_turn(
        &state,
        &session,
        id.as_ref(),
        &session_id,
        turn_id,
        turn_failure,
        autonomous_stop,
        tx,
    )
    .await;
}

/// A stop that landed before the turn got going (the admission saw the
/// aborted agent): the turn is cancelled, not failed.
async fn cancelled_before_start(
    state: &Arc<Mutex<ConnectionState>>,
    session: &Arc<AcpSession>,
    id: Option<&Value>,
    session_id: &str,
    turn_id: u64,
    tx: producer::FrameSink,
) {
    settle_turn(state, session, id, session_id, turn_id, None, None, tx).await;
}

/// Execute one session command and publish its namespaced events:
/// compaction outcomes, plus any goal state change. A goal
/// start/resume schedules its continuation context as the turn's model
/// segment after the command settles.
async fn run_session_command_segment(
    mode: &AcpModeState,
    session: &Arc<AcpSession>,
    command: &uh_core::session_engine::slash_commands::SessionSlashCommand,
    turn_failure: &mut Option<String>,
) -> anyhow::Result<bool> {
    // The command executor runs on one model/key pair read through the
    // config queue: a concurrent picker switch cannot hand `/compact`
    // the pre-switch model with the switched provider's key.
    let (model, api_key) = mode.model_and_api_key().await;
    if session.cancel_requested() {
        return Err(uh_agent::abort::aborted_error());
    }
    let Some(model) = model else {
        // Unreachable in practice (the engine assembly requires a model);
        // fail as a request error instead of a turn failure.
        anyhow::bail!("No model available to run the session command");
    };
    // The autonomous guard scopes tightly around the executor call: a
    // scheduled continuation prompts the model below, and the event
    // listener's per-message accounting must be able to take the same
    // mutex while that turn runs.
    let compacts = command.name == "compact";
    let controller = compacts.then(|| session.compaction_abort_controller());
    let signal = controller.as_ref().map(|controller| controller.signal());
    if compacts {
        session.usage.compacting(true).await;
    }
    let execution = {
        let mut autonomous = session.autonomous.lock().await;
        let mut params = SessionCommandParams {
            model: &model,
            api_key,
            autonomous: &mut autonomous,
            abort: signal.as_ref(),
        };
        execute_session_command(&mode.engine, &mut params, command).await
    };
    if compacts {
        session.usage.compacting(false).await;
    }
    if let Some(controller) = controller {
        session.arms.clear_abort_controller(&controller);
    }

    // A compaction publishes its namespaced event whether it ran (result
    // fields present) or skipped (the TS compaction_end with an undefined
    // result): the skip is observable, not silent.
    let compaction_event = if command.name == "compact" {
        Some(match &execution.compaction {
            Some(compaction) => events::AcpEngineEvent::CompactionEnd {
                tokens_before: Some(compaction.result.tokens_before),
                summary: Some(compaction.result.summary.clone()),
            },
            None => events::AcpEngineEvent::CompactionEnd {
                tokens_before: None,
                summary: None,
            },
        })
    } else {
        None
    };
    if let Some(event) = compaction_event {
        publish_engine_event(session, &event).await;
    }
    publish_command_output(session, command.name, &execution).await;
    if command.name == "compact" && execution.compaction.is_some() {
        // TS `compact()`'s `didCompact` + active-goal finally arm: a
        // successful manual compact with an active goal mints the owed
        // continuation (the `||= !hasQueuedMessages()` arm — the direct
        // ACP never queues work behind a session command) and the
        // continuation runs as the turn's model segment, the scheduled
        // continue over the queued follow-up.
        if let Some(message) = goal_continuation::mint_goal_continuation(mode, session).await {
            return Ok(
                run_goal_continuation_segment(mode, session, message, true, turn_failure).await,
            );
        }
    }

    // Any goal state change (start/status/clear/pause/resume) publishes.
    session.publish_goal_update().await;

    // A scheduled goal continuation runs as the turn's model segment; its
    // settled turn participates in the settle loop like any model turn.
    if let Some(message) = execution.continuation_message {
        return Ok(
            run_goal_continuation_segment(mode, session, message, false, turn_failure).await,
        );
    }
    Ok(false)
}

/// Run one goal-continuation segment (a goal-context row minted by the
/// goal commands or the compact-with-active-goal continue) as the turn's
/// model segment: the pre-turn compaction arms run before it like any
/// admitted prompt (TS `_runPreTurnCompaction`), the injected custom row
/// is the turn's one representation (TS's prepared-turn primary record),
/// and the settled turn's boundary re-enters the settle loop.
async fn run_goal_continuation_segment(
    mode: &AcpModeState,
    session: &Arc<AcpSession>,
    message: uh_types::session::CustomMessage,
    minted: bool,
    turn_failure: &mut Option<String>,
) -> bool {
    session.run_pre_turn_compaction(mode).await;
    if let Err(error) = prompt_goal_continuation(mode, session, &message, minted).await {
        *turn_failure = Some(format!("{error:#}"));
        return false;
    }
    mode.engine.session.agent().wait_for_idle().await;
    true
}

/// Publish what a session command answered: its shown result rows (the
/// `/goal` status, a refusal), then a skipped compaction's reason.
async fn publish_command_output(
    session: &Arc<AcpSession>,
    name: &str,
    execution: &uh_core::session_engine::session_commands::SessionCommandExecution,
) {
    let rows = execution
        .messages
        .iter()
        .filter(|row| {
            row.display
                && row.custom_type
                    == uh_types::slash_commands::SESSION_SLASH_COMMAND_RESULT_CUSTOM_TYPE
        })
        .map(|row| {
            let severity = row
                .details
                .as_ref()
                .and_then(|details| details.get("severity"))
                .and_then(Value::as_str)
                .unwrap_or("info");
            (row.content.text(), severity)
        });
    let skipped = execution
        .compaction_skipped
        .map(|reason| (reason.to_string(), "warning"));
    let turn_id = session.producer().active_prompt_turn().await;
    for (text, severity) in rows.chain(skipped) {
        let update = AcpSessionUpdate::SessionInfoUpdate {
            meta: meta::unharnessed_meta(&meta::UnharnessedSessionMeta {
                command: Some(meta::UnharnessedCommandMeta {
                    name: name.to_string(),
                    text,
                    severity: severity.to_string(),
                }),
                ..Default::default()
            }),
        };
        session
            .producer()
            .publish(&update, turn_id, UnharnessedEventPhase::Event, None)
            .await;
    }
}

/// Publish one adapter event through the session producer at the active
/// turn.
async fn publish_engine_event(session: &Arc<AcpSession>, event: &events::AcpEngineEvent) {
    session.publish_engine_event(event).await;
}

/// Settle one finished turn: the correlated boundary envelope, the response,
/// and the slot bookkeeping. The autonomous status rides the completion
/// update while a run is enabled, and the stop reason maps the driver's
/// stop outcome (`max_tokens` for token exhaustion, `max_turn_requests`
/// for every other limit, `end_turn` otherwise).
#[allow(clippy::too_many_arguments)]
async fn settle_turn(
    state: &Arc<Mutex<ConnectionState>>,
    session: &Arc<AcpSession>,
    id: Option<&Value>,
    session_id: &str,
    turn_id: u64,
    turn_failure: Option<String>,
    autonomous_stop: Option<(
        uh_core::autonomous::AutonomousStopReason,
        Box<AgentAutonomousStatus>,
    )>,
    tx: producer::FrameSink,
) {
    if session.cancel_requested() {
        // A cancellation before the response boundary resolves the request
        // with the protocol stop reason and no boundary frames.
        session.producer().finish_prompt(turn_id).await;
        if let Some(id) = id {
            let _ = tx.send(jsonrpc::response(
                id,
                &stop_reason_response(AcpStopReason::Cancelled),
            ));
        }
        clear_prompt_slot(state, session_id).await;
        return;
    }

    let outcome = if turn_failure.is_some() {
        UnharnessedOutcome::Error
    } else {
        UnharnessedOutcome::Result
    };
    // The response boundary precedes the correlated response; the completion
    // event and terminal quiescence envelope follow it in publication order.
    if session::publish_response_boundary(session, turn_id, true, outcome)
        .await
        .is_err()
    {
        session.producer().finish_prompt(turn_id).await;
        if let Some(id) = id {
            let _ = tx.send(internal_error(
                id,
                "Failed to publish ACP response boundary",
            ));
        }
        clear_prompt_slot(state, session_id).await;
        return;
    }

    // The completion update carries the autonomous accounting while a run
    // is enabled (the stop status when the driver stopped the run, the live
    // snapshot otherwise).
    let autonomous_status = if let Some((_, status)) = &autonomous_stop {
        Some((**status).clone())
    } else {
        let status = session.autonomous_status().await;
        status.enabled.then_some(status)
    };
    let autonomous_meta = autonomous_status.as_ref().map(autonomous_meta);
    // The remaining continuation slots the quiescence observation reports:
    // the configured budget minus what the run consumed (zero when no
    // autonomous run is active).
    let remaining_continuations = autonomous_status.as_ref().map_or(0, |status| {
        status
            .limits
            .max_continuations
            .saturating_sub(status.continuations_used)
    });
    if session::publish_completion_envelope(
        session,
        turn_id,
        outcome,
        autonomous_meta.as_ref(),
        remaining_continuations,
    )
    .await
    .is_err()
    {
        session.producer().finish_prompt(turn_id).await;
        if let Some(id) = id {
            let _ = tx.send(internal_error(
                id,
                "Failed to publish ACP completion update",
            ));
        }
        clear_prompt_slot(state, session_id).await;
        return;
    }

    session.producer().finish_prompt(turn_id).await;
    let stop_reason = match autonomous_stop.as_ref().map(|(reason, _)| reason) {
        None => AcpStopReason::EndTurn,
        Some(AutonomousStopReason::Limit(
            uh_core::autonomous::AutonomousLimitReason::MaxTokens,
        )) => AcpStopReason::MaxTokens,
        Some(AutonomousStopReason::Limit(_)) => AcpStopReason::MaxTurnRequests,
        Some(AutonomousStopReason::GatePassed | AutonomousStopReason::GateRetryExhausted) => {
            AcpStopReason::EndTurn
        }
    };
    if let Some(id) = id {
        let response = match turn_failure {
            Some(failure) => internal_error(id, &format!("unharnessed turn failed: {failure}")),
            None => jsonrpc::response(id, &stop_reason_response(stop_reason)),
        };
        let _ = tx.send(response);
    }
    clear_prompt_slot(state, session_id).await;
}

/// The running turn released the prompt slot; close/EOF no longer awaits
/// it, and the wake watcher may start the next wake turn.
async fn clear_prompt_slot(state: &Arc<Mutex<ConnectionState>>, session_id: &str) {
    let mut state = state.lock().await;
    if let Some(entry) = state.session.as_mut() {
        if entry.session.id == session_id {
            entry.prompt_task = None;
            entry.prompt_is_wake = false;
            entry.session.drop_steers();
            // A cancel stops the turn it was sent during, not the next.
            entry.session.clear_cancel();
            entry.session.poke_wakes();
        }
    }
}
