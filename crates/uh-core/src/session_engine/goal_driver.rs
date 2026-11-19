//! The goal driver: goal-state lifecycle, usage accounting, budget limits,
//! and continuation context. Port of the goal machinery in agent-session.ts
//! (the `_goalState` half), with persistence via `thread_goal_state` custom
//! entries and the branch-seed/reload rules.

use uh_types::session::CustomMessage;

use crate::goals::{
    create_goal_context_message, empty_goal_state, goal_token_delta_for_usage,
    normalize_goal_state, validate_goal_budget, validate_goal_objective, GoalContextKind,
    GoalState, GoalStatus, GOAL_STATE_CUSTOM_TYPE,
};
use crate::session::manager::SessionManager;

/// The goal timer's contract (operator ruling 2026-09-28):
/// `time_used_seconds` is the goal's AGE — the plain wall clock since
/// the goal's creation, computed fresh from `created_at` on every read.
/// Nothing folds and nothing accumulates: the pre-ruling
/// `accounting_started_at` anchor compounded each accounting write's
/// elapsed-since-anchor onto the persisted `time_used_seconds` (the
/// operator's 2h-old goal read 73h; the orchestrator's 1.4h goal read
/// 242.8h across 388 persisted rows), so the anchor machinery is deleted
/// entirely — `created_at` is set once at [`GoalDriver::start`] and never
/// re-based, making the stale-anchor class structurally impossible.
///
/// TS divergence (agent-session.ts `_goalWithAccountedWallClock`): TS
/// re-baselines an `_goalAccountingStartedAt` anchor at each accounting
/// fold and charges only active-status wall clock. The creation-based
/// ruling is simpler and deliberately different: a paused or idle goal
/// still displays its age, and the timer never depends on any anchor.
#[must_use]
pub fn creation_elapsed_seconds(created_at: Option<u64>, now: u64) -> u64 {
    created_at.map_or(0, |created| now.saturating_sub(created) / 1000)
}

/// What happened after accounting one assistant turn's usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageOutcome {
    Accounted,
    /// The goal hit its token budget and moved to `budget_limited`.
    BudgetReached,
    /// The goal was not active; usage was ignored.
    Ignored,
}

pub struct GoalDriver {
    state: GoalState,
    /// Ids of assistant messages already counted (double-counting guard).
    accounted_messages: std::collections::HashSet<String>,
    /// A continuation owed behind a running `bash()` (TS
    /// `_goalContinuationAwaitsRlmWork`, there behind RLM descendants).
    /// In-memory only (never persisted, never rehydrated): a restart kills
    /// the kernel and its commands.
    owed_continuation: bool,
    /// When the owed continuation goes even though a command still runs
    /// (epoch ms; 0 outside a wait).
    command_wait_until_ms: u64,
    /// The last turn that worked beside an older command and was relaunched
    /// at once (its timestamp): the next one in a row waits
    /// ([`GoalDriver::continuation_waits`]). In-memory, like the wait.
    relaunched_beside_a_command: Option<i64>,
    /// The goal completed and its host has not yet stopped the commands it
    /// left running ([`GoalDriver::take_completed`]). In-memory.
    completed_unhandled: bool,
    /// A minted continuation its surface has not admitted yet. While set,
    /// no mint site mints or re-arms another (the
    /// pending-never-re-arms contract: exactly one continuation per owed
    /// boundary — a queued-but-unconsumed continuation never duplicates).
    /// The surface releases it at admission
    /// ([`GoalDriver::continuation_consumed`]); a rollback or a goal
    /// going inactive drops it with the mint. In-memory only (never
    /// persisted, never rehydrated). An atomic behind an [`Arc`] so
    /// admission surfaces that cannot take the async driver lock (a
    /// spawned settle task, a nested `block_on`) release it through
    /// [`GoalDriver::pending_continuation_handle`].
    pending_continuation: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The consecutive-no-progress continuation bookkeeping (the hot-loop
    /// killer, the 402 diagnosis's (b)): a mint consult that finds the
    /// just-settled turn produced no output counts once (keyed by the
    /// turn's timestamp), arms the doubling backoff window, and at the
    /// cap finishes the goal. A progress turn resets the streak. In-memory
    /// only: the restart paths are guarded by the stale-row handling at
    /// rehydration and the mint's own progress check re-derives from the
    /// just-settled turn.
    no_progress_streak: u32,
    no_progress_backoff_until_ms: u64,
    /// The quota-park corpse's refusal window (in-memory; separate from
    /// the backoff window so the wake machinery never schedules a probe
    /// into a parked session — the park's own wake owns the retry): a
    /// re-consult of the parked corpse, or of the older row the
    /// pair-drop exposes, refuses inside it.
    parked_refusal_until_ms: u64,
    /// The last turn the streak counted (the re-consult dedup): adopted
    /// from the durable `no_progress_turn_ms` so a restart never
    /// re-counts the same corpse.
    counted_no_progress_turn_ms: Option<i64>,
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

mod progress;

pub use progress::{
    terminal_provider_failure, turn_produced_no_output, GOAL_BACKOFF_WAKE_MARKER_TEXT,
};
use progress::{TurnFacts, COMMAND_WAIT_MS, CONTINUATION_NO_PROGRESS_CAP};

impl GoalDriver {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: empty_goal_state(),
            accounted_messages: std::collections::HashSet::default(),
            owed_continuation: false,
            command_wait_until_ms: 0,
            relaunched_beside_a_command: None,
            completed_unhandled: false,
            pending_continuation: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            no_progress_streak: 0,
            no_progress_backoff_until_ms: 0,
            parked_refusal_until_ms: 0,
            counted_no_progress_turn_ms: None,
        }
    }

    /// Rehydrate the driver from the session branch (latest persisted
    /// entry). The restore-resurrection guard (the 402 diagnosis's (d)):
    /// an `active` newest row whose trailing turn failed on a provider
    /// error (the terminal finish never persisted — a worker death or
    /// restart interrupted the settle) adopts the failure as the goal's
    /// terminal state instead of resurrecting the loop; the resume sites
    /// then deliver no continuations into the dead provider.
    #[must_use]
    pub fn load_persisted(session: &SessionManager) -> Self {
        let mut state = Self::latest_persisted_state(session);
        if state.status == GoalStatus::Active {
            if let Some(error) = session.stale_active_goal_failure() {
                state = GoalState {
                    active: false,
                    status: GoalStatus::Error,
                    last_reason: Some(error.clone()),
                    last_error: Some(error),
                    ..state
                };
            }
        }
        Self::restore_persisted(state)
    }

    /// The branch's latest valid persisted goal state (TS
    /// `_loadPersistedGoalState`: newest-first scan over the branch's
    /// custom entries; `emptyGoalState()` when no valid entry exists).
    pub fn latest_persisted_state(session: &SessionManager) -> GoalState {
        session.active_goal_state().unwrap_or_else(empty_goal_state)
    }

    /// Reload the goal state from the session's current branch, once the
    /// conversation went back to an earlier message: the branch's latest
    /// persisted entry adopts as-is, even when it is older — the goal
    /// state follows the branch cut. The timer needs no anchor work here —
    /// the creation-based contract reads the adopted state's `created_at`
    /// directly (see [`creation_elapsed_seconds`]' divergence note).
    pub fn reload_from_branch(&mut self, session: &SessionManager) {
        let reloaded = Self::latest_persisted_state(session);
        // The previous goal's pending mint and its armed deferral belong to
        // the abandoned branch — keeping either would block (or
        // mis-deliver) the adopted goal's continuations until the next
        // pause/start/clear. Every other state-adoption site drops both
        // with the replaced goal; the branch reload does the same.
        self.continuation_consumed();
        self.owed_continuation = false;
        self.command_wait_until_ms = 0;
        self.relaunched_beside_a_command = None;
        // The adopted goal's own durable streak applies (a different goal
        // never inherits the previous goal's strikes); the backoff window
        // does not survive the move.
        self.no_progress_streak = reloaded.no_progress_streak.unwrap_or(0);
        self.no_progress_backoff_until_ms = 0;
        self.parked_refusal_until_ms = 0;
        self.counted_no_progress_turn_ms = reloaded.no_progress_turn_ms;
        // The restore-resurrection guard (the 402 diagnosis's (d)) applies
        // to the branch move exactly as it does to `load_persisted`: an
        // adopted `active` row with a terminal provider failure settled
        // after it never resurrects — going back onto the branch must not
        // revive a goal whose provider died before the settle's error row
        // landed.
        self.state = if reloaded.status == GoalStatus::Active {
            match session.stale_active_goal_failure() {
                Some(error) => GoalState {
                    active: false,
                    status: GoalStatus::Error,
                    last_reason: Some(error.clone()),
                    last_error: Some(error),
                    ..reloaded
                },
                None => reloaded,
            }
        } else {
            reloaded
        };
    }

    /// Adopt an already-persisted goal state without re-persisting it: a
    /// recovery rebuild continues the durable state verbatim (the
    /// `thread_goal_state` row already records it, and the creation-based
    /// timer reads the adopted `created_at` — no anchor, no downtime
    /// accrual beyond the age the ruling defines).
    #[must_use]
    pub fn restore_persisted(state: GoalState) -> Self {
        let mut driver = Self::new();
        driver.restore_from_persisted(state);
        driver
    }

    /// [`GoalDriver::restore_persisted`]'s in-place form, for the driver
    /// behind the session's shared handle: adopts the persisted state
    /// without re-persisting (the durable row already exists) and without
    /// resetting the per-message double-counting guard (a fresh build
    /// starts it empty anyway). The mint bookkeeping (the owed flag, the
    /// pending guard) stays quiescent: descendant quiescence and a
    /// queued-but-unconsumed continuation are live-session facts.
    pub fn restore_from_persisted(&mut self, state: GoalState) {
        self.state = normalize_goal_state(state);
        // The no-progress streak is durable: a rebuilt driver adopts the
        // persisted strikes (a worker restart cannot reset the streak and
        // un-cap a degenerate loop). The backoff window itself is not —
        // a restart outlives it, so the next consult passes the gate.
        self.no_progress_streak = self.state.no_progress_streak.unwrap_or(0);
        self.no_progress_backoff_until_ms = 0;
        self.parked_refusal_until_ms = 0;
        self.counted_no_progress_turn_ms = self.state.no_progress_turn_ms;
        self.owed_continuation = false;
        self.command_wait_until_ms = 0;
        self.relaunched_beside_a_command = None;
        self.continuation_consumed();
    }

    #[must_use]
    pub fn state(&self) -> &GoalState {
        &self.state
    }

    /// The served goal state: `time_used_seconds` reads the goal's
    /// creation-based age, computed fresh from `created_at` on every read
    /// (so an actively pursued goal's timer ticks live, an idle goal never
    /// compounds, and a paused goal shows its age). A state without
    /// `created_at` (a pre-contract row normalized before the backfill)
    /// keeps its last persisted `time_used_seconds`.
    #[must_use]
    pub fn state_with_creation_elapsed(&self) -> GoalState {
        match self.state.created_at {
            Some(created_at) => GoalState {
                time_used_seconds: creation_elapsed_seconds(Some(created_at), now_millis()),
                ..self.state.clone()
            },
            None => self.state.clone(),
        }
    }

    /// Whether the branch may be seeded with an initial goal: only bootstrap
    /// entries (model/thinking changes) and no prior persisted goal.
    #[must_use]
    pub fn is_branch_seedable(session: &SessionManager) -> bool {
        !session.has_non_bootstrap_entries()
    }

    /// Start a new goal (validates objective and budget).
    ///
    /// # Errors
    ///
    /// Returns an error when the objective or budget fails validation, or
    /// when the new goal state cannot be persisted.
    pub fn start(
        &mut self,
        session: &mut SessionManager,
        objective_text: &str,
        token_budget: Option<u64>,
    ) -> anyhow::Result<GoalState> {
        let objective = validate_goal_objective(objective_text)?;
        let budget = validate_goal_budget(token_budget)?;
        let now = now_millis();
        let goal = GoalState {
            active: true,
            status: GoalStatus::Active,
            goal_id: Some(uuid::Uuid::new_v4().to_string()),
            objective: Some(objective),
            token_budget: budget,
            tokens_used: 0,
            time_used_seconds: 0,
            continuations_used: 0,
            created_at: Some(now),
            // A fresh goal never inherits the previous goal's no-progress
            // streak: its own three-strike budget starts at 0.
            no_progress_streak: Some(0),
            no_progress_turn_ms: None,
            idle_streak: Some(0),
            updated_at: Some(now),
            last_reason: None,
            last_error: None,
        };
        let previous_accounted = std::mem::take(&mut self.accounted_messages);
        let previous_owed = self.owed_continuation;
        let previous_command_wait = self.command_wait_until_ms;
        let previous_relaunched_beside = self.relaunched_beside_a_command;
        let previous_pending = self.pending_continuation();
        let previous_completed = self.completed_unhandled;
        // TS `_startGoal`: a fresh goal starts with no owed continuation
        // (and no pending one — `_clearQueuedGoalContexts` drops the queued
        // contexts with the state change) — and with a fresh no-progress
        // streak: a replacement goal never inherits the terminal goal's
        // strikes.
        self.owed_continuation = false;
        self.command_wait_until_ms = 0;
        self.relaunched_beside_a_command = None;
        // The new goal's commands are not the finished one's leftovers.
        self.completed_unhandled = false;
        let previous_streak = self.no_progress_streak;
        let previous_backoff = self.no_progress_backoff_until_ms;
        let previous_parked = self.parked_refusal_until_ms;
        let previous_counted = self.counted_no_progress_turn_ms;
        self.no_progress_streak = 0;
        self.no_progress_backoff_until_ms = 0;
        self.parked_refusal_until_ms = 0;
        self.counted_no_progress_turn_ms = None;
        self.continuation_consumed();
        if let Err(error) = self.set_state(session, goal) {
            // A failed start leaves the previous goal's bookkeeping intact
            // (the no-progress cap included).
            self.accounted_messages = previous_accounted;
            self.completed_unhandled = previous_completed;
            self.owed_continuation = previous_owed;
            self.command_wait_until_ms = previous_command_wait;
            self.relaunched_beside_a_command = previous_relaunched_beside;
            self.no_progress_streak = previous_streak;
            self.no_progress_backoff_until_ms = previous_backoff;
            self.parked_refusal_until_ms = previous_parked;
            self.counted_no_progress_turn_ms = previous_counted;
            if previous_pending {
                self.mark_continuation_pending();
            }
            return Err(error);
        }
        Ok(self.state_with_creation_elapsed())
    }

    /// Clear the goal entirely (empty state).
    ///
    /// # Errors
    ///
    /// Returns an error when the cleared goal state cannot be persisted.
    pub fn clear(&mut self, session: &mut SessionManager) -> anyhow::Result<()> {
        self.set_state(session, empty_goal_state())?;
        // TS `_clearGoal` routes through `_clearQueuedGoalContexts`, which
        // drops any owed or pending continuation with the queued contexts.
        self.owed_continuation = false;
        self.continuation_consumed();
        Ok(())
    }

    fn set_state(&mut self, session: &mut SessionManager, next: GoalState) -> anyhow::Result<()> {
        let now = now_millis();
        let normalized = normalize_goal_state(GoalState {
            updated_at: Some(now),
            ..next
        });
        // The creation-based timer: every durable row carries the goal's
        // age at the write — `time_used_seconds` recomputes from
        // `created_at` (never accumulates), so a rehydrated session serves
        // the same contract the live read does.
        let normalized = match normalized.created_at {
            Some(created_at) => GoalState {
                time_used_seconds: creation_elapsed_seconds(Some(created_at), now),
                ..normalized
            },
            None => normalized,
        };
        let value = serde_json::to_value(&normalized)?;
        session.append_custom_entry(GOAL_STATE_CUSTOM_TYPE, Some(value))?;
        session.flush_now()?;
        // A goal leaving the active state drops its pending mint with the
        // queued contexts (TS `_clearQueuedGoalContexts` at the pause/
        // complete/clear state changes): a continuation owed to a dead
        // goal never wedges the next one's mint. Nor does one still owed
        // behind a command.
        if normalized.status != GoalStatus::Active {
            self.continuation_consumed();
            self.owed_continuation = false;
            self.command_wait_until_ms = 0;
            self.relaunched_beside_a_command = None;
        }
        self.state = normalized;
        Ok(())
    }

    /// Account one assistant turn's usage. Double-counts are suppressed by
    /// message id. Returns whether the budget was reached.
    ///
    /// # Errors
    ///
    /// Returns an error when the accounted goal state cannot be persisted.
    pub fn record_assistant_usage(
        &mut self,
        session: &mut SessionManager,
        message_id: &str,
        usage: &uh_types::ai::Usage,
    ) -> anyhow::Result<UsageOutcome> {
        if self.state.status != GoalStatus::Active {
            return Ok(UsageOutcome::Ignored);
        }
        // The double-counting guard stays open until the state is durable:
        // a failed persist must let the retry account this message again.
        if self.accounted_messages.contains(message_id) {
            return Ok(UsageOutcome::Ignored);
        }
        let token_delta = goal_token_delta_for_usage(usage.input as i64, usage.output as i64);
        let next_goal = GoalState {
            tokens_used: self.state.tokens_used + token_delta,
            ..self.state.clone()
        };
        let budget_reached = next_goal
            .token_budget
            .is_some_and(|budget| next_goal.tokens_used >= budget);
        let outcome = if budget_reached {
            let token_budget = next_goal.token_budget;
            let budget_reason = token_budget
                .map(|budget| format!("Reached {budget} token goal budget"))
                .unwrap_or_default();
            self.set_state(
                session,
                GoalState {
                    active: false,
                    status: GoalStatus::BudgetLimited,
                    last_reason: Some(budget_reason),
                    last_error: None,
                    ..next_goal
                },
            )?;
            UsageOutcome::BudgetReached
        } else {
            self.set_state(session, next_goal)?;
            UsageOutcome::Accounted
        };
        // Account the message only after the durable write lands.
        self.accounted_messages.insert(message_id.to_string());
        Ok(outcome)
    }

    /// Pause the goal (no-op when not active).
    ///
    /// # Errors
    ///
    /// Returns an error when the paused goal state cannot be persisted.
    pub fn pause(&mut self, session: &mut SessionManager, reason: &str) -> anyhow::Result<()> {
        if self.state.status != GoalStatus::Active {
            return Ok(());
        }
        self.set_state(
            session,
            GoalState {
                active: false,
                status: GoalStatus::Paused,
                last_reason: Some(reason.to_string()),
                last_error: None,
                ..self.state.clone()
            },
        )?;
        // TS `_pauseGoal` routes through `_clearQueuedGoalContexts`, which
        // drops any owed or pending continuation with the queued contexts —
        // after the durable write, so a failed persist keeps the previous
        // goal's deferral exactly as it was.
        self.owed_continuation = false;
        self.continuation_consumed();
        Ok(())
    }

    /// Resume a paused/blocked/budget-limited goal, its idle turns forgotten.
    /// Returns the continuation context message when the goal becomes active
    /// again.
    ///
    /// # Errors
    ///
    /// Returns an error when the resumed goal state or its continuation
    /// message cannot be persisted.
    pub fn resume(
        &mut self,
        session: &mut SessionManager,
    ) -> anyhow::Result<Option<CustomMessage>> {
        if self.state.objective.is_none() {
            return Ok(None);
        }
        if !matches!(
            self.state.status,
            GoalStatus::Paused | GoalStatus::Blocked | GoalStatus::BudgetLimited
        ) {
            return Ok(None);
        }
        let exhausted = self
            .state
            .token_budget
            .is_some_and(|budget| self.state.tokens_used >= budget);
        let next_status = if exhausted {
            GoalStatus::BudgetLimited
        } else {
            GoalStatus::Active
        };
        self.set_state(
            session,
            GoalState {
                active: next_status == GoalStatus::Active,
                status: next_status,
                // TS `_resumeGoal`: the reason is only set for an exhausted
                // budget (which stays budget_limited); a live resume clears it.
                last_reason: exhausted.then(|| "Goal token budget already reached".to_string()),
                last_error: None,
                idle_streak: Some(0),
                ..self.state.clone()
            },
        )?;
        if next_status == GoalStatus::Active {
            return Ok(
                create_goal_context_message(&self.state, GoalContextKind::Continuation).ok(),
            );
        }
        Ok(None)
    }

    /// Complete the goal (host `goal.complete()`).
    ///
    /// # Errors
    ///
    /// Returns an error when the completed goal state cannot be persisted.
    pub fn complete(&mut self, session: &mut SessionManager) -> anyhow::Result<()> {
        if self.state.objective.is_none() || self.state.status == GoalStatus::Idle {
            return Ok(());
        }
        let completing = self.state.status != GoalStatus::Complete;
        self.set_state(
            session,
            GoalState {
                active: false,
                status: GoalStatus::Complete,
                last_reason: Some("Goal achieved".to_string()),
                last_error: None,
                ..self.state.clone()
            },
        )?;
        self.completed_unhandled |= completing;
        Ok(())
    }

    /// Whether the goal completed since the last call: the host then stops
    /// the commands it left running, so none of them wakes a session whose
    /// work is done.
    pub fn take_completed(&mut self) -> bool {
        std::mem::take(&mut self.completed_unhandled)
    }

    /// Block the active goal (host `goal.block()`): it stops relaunching and
    /// waits for the user (no-op when not active). The reason is capped like
    /// the one the idle counter writes.
    ///
    /// # Errors
    ///
    /// Returns an error when the blocked goal state cannot be persisted.
    pub fn block(&mut self, session: &mut SessionManager, reason: &str) -> anyhow::Result<()> {
        if self.state.status != GoalStatus::Active {
            return Ok(());
        }
        self.set_state(
            session,
            GoalState {
                active: false,
                status: GoalStatus::Blocked,
                last_reason: Some(progress::capped_reason(reason)),
                last_error: None,
                ..self.state.clone()
            },
        )
    }

    /// Terminal-assistant handling: `aborted` keeps the goal, `error` fails it.
    ///
    /// # Errors
    ///
    /// Returns an error when the terminal goal state cannot be persisted.
    pub fn finish_for_terminal_message(
        &mut self,
        session: &mut SessionManager,
        stop_reason: uh_types::ai::StopReason,
        error_message: Option<&str>,
    ) -> anyhow::Result<()> {
        use uh_types::ai::StopReason;
        if self.state.status != GoalStatus::Active {
            return Ok(());
        }
        if let StopReason::Error = stop_reason {
            let reason = error_message
                .filter(|message| !message.is_empty())
                .unwrap_or("Assistant response failed");
            self.set_state(
                session,
                GoalState {
                    active: false,
                    status: GoalStatus::Error,
                    last_reason: Some(reason.to_string()),
                    last_error: Some(reason.to_string()),
                    ..self.state.clone()
                },
            )?;
        }
        Ok(())
    }

    /// The continuation of a turn that just settled: what
    /// [`GoalDriver::next_continuation_message`] mints, once the turn is
    /// judged. `running_commands` holds when each `bash()` still running
    /// started (Unix ms). An idle turn counts toward `blocked`; a turn
    /// waiting on a command mints nothing and leaves the continuation owed.
    ///
    /// # Errors
    ///
    /// Returns an error when the goal state or its context message cannot be
    /// persisted or built.
    pub fn continuation_after_turn(
        &mut self,
        session: &mut SessionManager,
        last_turn: Option<&uh_agent::types::AssistantMessage>,
        running_commands: &[u64],
    ) -> anyhow::Result<Option<CustomMessage>> {
        if self.state.status != GoalStatus::Active {
            return Ok(None);
        }
        let facts = TurnFacts::read(session, running_commands);
        let message = self.mint_continuation(session, last_turn, Some(facts))?;
        // This boundary delivers: nothing stays owed behind it.
        if message.is_some() {
            self.owed_continuation = false;
            self.command_wait_until_ms = 0;
        }
        Ok(message)
    }

    /// Build the next continuation context, consuming one continuation
    /// slot. The state change persists (TS `_getGoalContinuationMessages`
    /// and `_maybeResumeGoalContinuationAfterRlmWork` both run the mint
    /// through `_setGoalState`, which appends the `thread_goal_state`
    /// entry before the continuation turn is admitted).
    ///
    /// # Errors
    ///
    /// Returns an error when the continuation-consumed goal state or its
    /// context message cannot be persisted or built.
    pub fn next_continuation_message(
        &mut self,
        session: &mut SessionManager,
        last_turn: Option<&uh_agent::types::AssistantMessage>,
    ) -> anyhow::Result<Option<CustomMessage>> {
        self.mint_continuation(session, last_turn, None)
    }

    fn mint_continuation(
        &mut self,
        session: &mut SessionManager,
        last_turn: Option<&uh_agent::types::AssistantMessage>,
        facts: Option<TurnFacts>,
    ) -> anyhow::Result<Option<CustomMessage>> {
        if self.state.status != GoalStatus::Active || self.state.objective.is_none() {
            return Ok(None);
        }
        // SANCTIONED DIVERGENCE (the 402 diagnosis, operator ruling): the
        // mint checks progress. TS `_getGoalContinuationMessages` mints
        // whenever the goal is Active — whether the previous turn
        // produced 2,000 tokens of work or an empty 402 corpse is
        // invisible to it, so a dead provider drove the operator's
        // 64-continuation hot loop. The just-settled turn now gates the
        // mint: a provider failure finishes the goal (mirroring
        // `finish_for_terminal_message`), a no-output turn counts toward
        // the consecutive cap with backoff, and only a turn that produced
        // output continues the loop.
        if let Some(turn) = last_turn {
            if !self.progress_gate(session, turn, facts)? {
                return Ok(None);
            }
        }

        // The cap enforcement is UNCONDITIONAL: a restored goal already at
        // the cap (a restart or a failed terminal persist between the
        // streak row and the error row) finishes at the FIRST consult —
        // the examined-turn gate must not shield a cap that already
        // struck from the terminal transition.
        if self.state.status == GoalStatus::Active
            && self.no_progress_streak >= CONTINUATION_NO_PROGRESS_CAP
        {
            let reason =
                "Goal continuation cap reached: consecutive turns made no progress".to_string();
            self.set_state(
                session,
                GoalState {
                    active: false,
                    status: GoalStatus::Error,
                    no_progress_streak: Some(self.no_progress_streak),
                    last_reason: Some(reason.clone()),
                    last_error: Some(reason),
                    ..self.state.clone()
                },
            )?;
            return Ok(None);
        }
        // The backoff gate (and the parked-refusal window): a consult
        // inside either window mints nothing; the next boundary after the
        // window re-mints (the goal stays Active — a delay, not a death).
        // The parked window never reaches `backoff_wake_at` — no probe
        // wake is ever scheduled for a parked session.
        let now = now_millis();
        if now < self.no_progress_backoff_until_ms || now < self.parked_refusal_until_ms {
            return Ok(None);
        }
        // The model is waiting on a command: no relaunch now. It goes when
        // the last command ends, or at the end of the window for one that
        // never does ([`GoalDriver::owed_continuation_due`]).
        if facts.is_some_and(|facts| self.continuation_waits(facts, last_turn)) {
            self.mark_continuation_owed();
            if self.owed_continuation && self.command_wait_until_ms == 0 {
                self.command_wait_until_ms = now + COMMAND_WAIT_MS;
            }
            return Ok(None);
        }
        // The pending-never-re-arms contract: a continuation minted but
        // not yet admitted by its surface blocks every further mint —
        // exactly one continuation per owed boundary, never duplicates
        // (the operator's dock saw one boundary deliver several "Goal
        // continuation" turns).
        if self.pending_continuation() {
            return Ok(None);
        }
        self.set_state(
            session,
            GoalState {
                continuations_used: self.state.continuations_used + 1,
                last_reason: None,
                last_error: None,
                ..self.state.clone()
            },
        )?;
        let message = create_goal_context_message(&self.state, GoalContextKind::Continuation).ok();
        // The mint is owed to the calling surface until it admits the
        // turn ([`GoalDriver::continuation_consumed`]); a rollback
        // un-mints it.
        if message.is_some() {
            self.mark_continuation_pending();
        }
        Ok(message)
    }

    /// TS `_getGoalContinuationMessages`'s quiescence arm: the natural
    /// turn end defers the continuation while a command runs. The mint
    /// consumes nothing while it waits; the wake delivers it
    /// (`take_owed_continuation`).
    ///
    /// TS arms only when nothing is already queued
    /// (`_goalContinuationAwaitsRlmWork ||= !hasQueuedMessages()`): a
    /// minted-but-unadmitted continuation IS this boundary's queued
    /// delivery, so arming beside it would leave the flag set when the
    /// pending mint admits — a later settle would deliver a SECOND
    /// continuation for the same boundary. The pending guard makes the
    /// arm a no-op instead.
    pub fn mark_continuation_owed(&mut self) {
        if self.pending_continuation() {
            return;
        }
        self.owed_continuation = true;
    }

    /// Whether a continuation is currently owed behind a command
    /// (TS `_goalContinuationAwaitsRlmWork`).
    #[must_use]
    pub fn owes_continuation(&self) -> bool {
        self.owed_continuation
    }

    /// Whether the owed continuation can go now: the commands ended or their
    /// window closed, and no other gate would refuse the mint.
    #[must_use]
    pub fn owed_continuation_due(&self, command_running: bool) -> bool {
        let now = now_millis();
        self.owed_continuation
            && self.state.status == GoalStatus::Active
            && !self.pending_continuation()
            && now >= self.no_progress_backoff_until_ms
            && now >= self.parked_refusal_until_ms
            && (!command_running || now >= self.command_wait_until_ms)
    }

    /// When the owed continuation goes even if a command still runs: an idle
    /// host wakes at this instant.
    #[must_use]
    pub fn owed_continuation_wake_at(&self) -> Option<u64> {
        (self.owed_continuation && self.state.status == GoalStatus::Active).then(|| {
            // Match the delivery gates so an expired command wait cannot
            // spin the wake watcher while either refusal window is open.
            self.command_wait_until_ms
                .max(self.no_progress_backoff_until_ms)
                .max(self.parked_refusal_until_ms)
        })
    }

    /// TS `_maybeResumeGoalContinuationAfterRlmWork`: deliver the owed
    /// continuation once, consuming one slot. Clears the flag — an
    /// inactive goal drops the deferral (minting nothing), a live one
    /// mints; a failed mint restores the deferral so the boundary
    /// retries. `None` when no continuation was owed or the goal
    /// cannot mint.
    ///
    /// # Errors
    ///
    /// Returns the mint error of the owed continuation (the deferral is
    /// restored for the next boundary).
    pub fn take_owed_continuation(
        &mut self,
        session: &mut SessionManager,
        last_turn: Option<&uh_agent::types::AssistantMessage>,
    ) -> anyhow::Result<Option<CustomMessage>> {
        let owed = self.owed_continuation;
        if !owed {
            return Ok(None);
        }
        // A pending continuation holds the deferral: the owed boundary
        // delivers once the previous mint is admitted, never beside it.
        if self.pending_continuation() {
            return Ok(None);
        }
        self.owed_continuation = false;
        match self.next_continuation_message(session, last_turn) {
            Ok(None) => {
                // The mint refused for progress reasons (the goal
                // finished, the pending guard, or the backoff window):
                // an inactive goal drops the deferral (TS); a live goal
                // in backoff keeps it, so a later boundary after the
                // window still delivers the owed continuation.
                if self.state.status == GoalStatus::Active {
                    self.owed_continuation = true;
                }
                Ok(None)
            }
            Ok(message) => {
                self.command_wait_until_ms = 0;
                Ok(message)
            }
            Err(error) => {
                // A failed mint (the durable continuation slot never landed)
                // restores the deferral: the natural boundary retries instead
                // of silently dropping the owed continuation.
                self.owed_continuation = true;
                Err(error)
            }
        }
    }

    /// The minted continuation's surface admitted it (the queue push or
    /// the in-run handoff): the pending guard releases, so the next
    /// boundary may mint again. TS clears `_goalContinuationAwaitsRlmWork`
    /// at the same point (`_admitSessionInput`'s follow-up admission).
    pub fn continuation_consumed(&mut self) {
        self.pending_continuation
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether a minted continuation is still waiting for its surface's
    /// admission (the pending-never-re-arms guard).
    #[must_use]
    pub fn pending_continuation(&self) -> bool {
        self.pending_continuation
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The pending guard's lock-free handle: admission surfaces that
    /// cannot take the async driver lock (a spawned settle task, a nested
    /// `block_on`, the worker's abort-cancel) release the guard through
    /// it (`store(false)`) — the driver's own mint sites still read and
    /// set it under the driver lock.
    #[must_use]
    pub fn pending_continuation_handle(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        std::sync::Arc::clone(&self.pending_continuation)
    }

    /// Arm the pending guard: one minted continuation is owed to the
    /// calling surface until it admits the turn.
    fn mark_continuation_pending(&mut self) {
        self.pending_continuation
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Roll back one just-minted continuation (TS `_getContinuationMessages`
    /// restores the goal snapshot when new session input arrived during the
    /// mint; the threshold-cancel rollback decrements the same way): the
    /// next boundary re-mints instead of double-counting.
    ///
    /// # Errors
    ///
    /// Returns an error when the rolled-back goal state cannot be
    /// persisted.
    pub fn rollback_continuation_mint(
        &mut self,
        session: &mut SessionManager,
    ) -> anyhow::Result<()> {
        if self.state.continuations_used == 0 {
            // The rolled-back mint never reaches a turn: the pending guard
            // releases with the slot.
            self.continuation_consumed();
            return Ok(());
        }
        let rolled_back = self.set_state(
            session,
            GoalState {
                continuations_used: self.state.continuations_used - 1,
                ..self.state.clone()
            },
        );
        // The rolled-back mint never reaches a turn, so its pending
        // guard releases on both outcomes (`set_state` persists before
        // assigning, so a failed write leaves the slot charged at the
        // mint's increment — in memory and on disk, consistently). The
        // CALLER must drop the re-owe on that failure branch: the slot
        // stays spent, and a re-minted follow-up would double-charge
        // the eventual turn.
        self.continuation_consumed();
        rolled_back
    }

    /// The current consecutive-no-output-turn streak (the durable cap
    /// counter); the in-module tests read it directly.
    #[cfg(test)]
    #[must_use]
    pub fn no_progress_streak(&self) -> u32 {
        self.no_progress_streak
    }

    /// The armed no-progress backoff window's deadline, when the goal is
    /// Active and the window is still open: an idle in-process host wakes
    /// at this instant (`session_engine::wakes`, the 402 diagnosis's (b)
    /// — without it, the refusal would stall the goal until an unrelated
    /// boundary event, and the advertised 10s/20s/40s retry would never
    /// run).
    #[must_use]
    pub fn backoff_wake_at(&self) -> Option<u64> {
        let until = self.no_progress_backoff_until_ms;
        (self.state.status == GoalStatus::Active && until > now_millis()).then_some(until)
    }

    /// Whether the goal drives session wake-ups.
    #[must_use]
    pub fn owns_continuation_wakeup(&self) -> bool {
        self.state.status == GoalStatus::Active && self.state.objective.is_some()
    }

    /// The active objective, when set and active.
    #[must_use]
    pub fn active_objective(&self) -> Option<String> {
        (self.state.status == GoalStatus::Active)
            .then(|| self.state.objective.clone())
            .flatten()
    }
}

impl Default for GoalDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
