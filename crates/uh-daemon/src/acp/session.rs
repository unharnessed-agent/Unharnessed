//! The single-session ACP state machine: admission, prompt lifecycle,
//! cancel, and close over the in-process session engine.
//!
//! One ACP connection drives one session. A second `session/new` is refused
//! rather than silently sharing conversation state, cwd, and queues.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::Mutex;
use uh_agent::agent::{Agent, Subscription};
use uh_agent::stream::AssistantMessageEvent;
use uh_agent::types::{AgentEvent, AgentMessage, Message};

use super::compaction_arms::CompactionArms;
use super::events::{acp_updates_for_event, AcpEngineEvent, MappingState};
use super::jsonrpc;
use super::meta::{
    UnharnessedAutonomousMeta, UnharnessedEventPhase, UnharnessedOutcome,
    UnharnessedQuiescenceMeta, UnharnessedSessionMeta,
};
use super::producer::UpdateProducer;
use super::types::{parse_prompt_blocks, AcpSessionUpdate, ImageBlock, PromptBlockError};
use super::usage::UsageReporter;

/// The session file's mark before a message steered into a running turn.
pub(super) const STEER_CUSTOM_TYPE: &str = "steer";

/// One hosted ACP session: the producer, the engine event subscription, the
/// autonomous run state, and the prompt-lifecycle bookkeeping.
pub struct AcpSession {
    pub id: String,
    producer: Arc<UpdateProducer>,
    subscription: Mutex<Option<Subscription>>,
    agent: Arc<Agent>,
    cancel_requested: Arc<AtomicBool>,
    /// The autonomous run state (`/autonomous` flags and commands mutate it).
    pub autonomous: Arc<Mutex<uh_core::autonomous::AutonomousRuntimeState>>,
    /// The autonomous continuation policy (shell gates in the session cwd).
    autonomous_driver: Arc<dyn uh_core::autonomous::AutonomousDriver>,
    /// The engine handle for goal usage recording on settled messages.
    engine: Arc<uh_core::session_engine::engine::SessionEngine>,
    /// The last goal state published as `_meta.goal`, to detect changes.
    /// Shared with the event subscription so mid-turn changes publish too.
    last_published_goal: Arc<Mutex<uh_core::goals::GoalState>>,
    /// The automatic compaction arm state (the overflow recovery machine
    /// and the in-flight compaction abort slot), TS session-lifetime
    /// state. Shared with the arm implementation
    /// (`compaction_arms.rs`).
    pub(super) arms: Arc<CompactionArms>,
    /// Whether the latest turn's usage crossed the goal's token budget
    /// (the usage listener arms it on `BudgetReached`, the settle loop's
    /// goal boundary consumes it): TS `_shouldStopAfterTurn`'s budget arm.
    goal_budget_crossed: Arc<AtomicBool>,
    /// The context gauge's reports (`_meta.unharnessed.usage`).
    pub(super) usage: Arc<UsageReporter>,
    /// The messages steered into the running turn and not taken yet, by
    /// timestamp: the subscription tells the client when each joins.
    steers: Arc<std::sync::Mutex<Vec<i64>>>,
}

impl AcpSession {
    /// Admit a session: subscribe the engine event feed (with the
    /// autonomous accounting and goal usage hooks) before anything can
    /// publish, so no update is lost between admission and the response.
    pub async fn new(
        id: String,
        engine: Arc<uh_core::session_engine::engine::SessionEngine>,
        model: Arc<Mutex<Option<uh_types::ai::Model>>>,
        producer: Arc<UpdateProducer>,
        autonomous: Arc<Mutex<uh_core::autonomous::AutonomousRuntimeState>>,
        autonomous_driver: Arc<dyn uh_core::autonomous::AutonomousDriver>,
    ) -> AcpSession {
        let mapping = Arc::new(Mutex::new(MappingState::default()));
        let last_published_goal =
            Arc::new(Mutex::new(engine.goal_driver.lock().await.state().clone()));
        // The compaction arm state is shared with the event subscription:
        // a user row that starts an agent run resets the overflow
        // recovery machine at its `message_start` (TS `startsAgentRun`).
        let arms = Arc::new(CompactionArms::new());
        let goal_budget_crossed = Arc::new(AtomicBool::new(false));
        let usage = UsageReporter::new(engine.clone(), model, producer.clone());
        let steers = Arc::new(std::sync::Mutex::new(Vec::new()));
        let cancel_requested = Arc::new(AtomicBool::new(false));
        let subscription = subscribe_engine_events(
            &engine,
            producer.clone(),
            mapping,
            autonomous.clone(),
            autonomous_driver.clone(),
            last_published_goal.clone(),
            arms.clone(),
            goal_budget_crossed.clone(),
            usage.clone(),
            steers.clone(),
            cancel_requested.clone(),
        )
        .await;
        AcpSession {
            id,
            producer,
            subscription: Mutex::new(Some(subscription)),
            agent: engine.session.agent().clone(),
            cancel_requested,
            autonomous,
            autonomous_driver,
            engine,
            last_published_goal,
            arms,
            goal_budget_crossed,
            usage,
            steers,
        }
    }

    /// A message steered into the running turn, by its timestamp: its
    /// `message_start` starts a turn of its own for the client.
    pub fn steered(&self, timestamp: i64) {
        self.steers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(timestamp);
    }

    /// The turn is over: what was steered into it and not taken goes
    /// (the client sends it again).
    pub fn drop_steers(&self) {
        self.steers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.agent.clear_steering_queue();
    }

    /// Release the engine event subscription; no further updates flow.
    pub async fn unsubscribe(&self) {
        let subscription = self.subscription.lock().await.take();
        if let Some(subscription) = subscription {
            subscription.unsubscribe().await;
        }
    }

    /// Fence the producer (a closed session publishes nothing until a
    /// replacement `session/new` is admitted).
    pub async fn close_producer(&self) {
        self.producer.close().await;
    }

    /// Signal the wake watcher that the session may be idle again.
    pub fn poke_wakes(&self) {
        self.engine.wakes().poke();
    }

    pub fn cancel_requested(&self) -> bool {
        self.cancel_requested.load(Ordering::SeqCst)
    }

    /// Stop the entire ACP turn, including work between agent runs.
    /// Call under the turn-slot lock while the session can admit new work.
    pub fn request_cancel(&self) {
        self.cancel_requested.store(true, Ordering::SeqCst);
        self.abort_auto_compaction();
        self.agent.abort();
        self.agent.clear_all_queues();
    }

    /// The cancelled turn has settled: the next prompt or wake runs.
    pub fn clear_cancel(&self) {
        self.cancel_requested.store(false, Ordering::SeqCst);
    }

    /// Consume the goal-budget crossing the latest turn's usage armed
    /// (the settle loop's budget-steer read; TS `_shouldStopAfterTurn`'s
    /// budget arm).
    pub fn take_goal_budget_crossed(&self) -> bool {
        self.goal_budget_crossed.swap(false, Ordering::SeqCst)
    }

    pub fn producer(&self) -> &Arc<UpdateProducer> {
        &self.producer
    }

    pub fn agent(&self) -> &Arc<Agent> {
        &self.agent
    }

    /// Consult the autonomous driver for one settled turn (gate evaluation
    /// runs inside the driver).
    pub async fn autonomous_follow_up(
        &self,
        message: &uh_types::ai::AssistantMessage,
    ) -> uh_core::autonomous::AutonomousFollowUp {
        let mut state = self.autonomous.lock().await;
        self.autonomous_driver.after_turn(&mut state, message).await
    }

    /// The current autonomous status snapshot (the completion envelope
    /// publishes it while a run is enabled).
    pub async fn autonomous_status(&self) -> uh_core::autonomous::AgentAutonomousStatus {
        let state = self.autonomous.lock().await;
        uh_core::autonomous::autonomous_status(&state)
    }

    /// Publish the current goal state as a `_meta.goal` update when it
    /// differs from the last one published on this connection.
    pub async fn publish_goal_update(&self) {
        let goal = self.engine.goal_driver.lock().await.state().clone();
        let changed = {
            let mut last = self.last_published_goal.lock().await;
            if *last == goal {
                return;
            }
            *last = goal.clone();
            true
        };
        if !changed {
            return;
        }
        self.publish_goal(&goal).await;
    }

    /// A loaded conversation's goal, published once it is back (nothing
    /// changed, so the update above stays quiet): the panel shows it.
    pub async fn publish_loaded_goal(&self) {
        let goal = self.engine.goal_driver.lock().await.state().clone();
        if goal.status != uh_core::goals::GoalStatus::Idle {
            self.publish_goal(&goal).await;
        }
    }

    async fn publish_goal(&self, goal: &uh_core::goals::GoalState) {
        self.publish_engine_event(&AcpEngineEvent::GoalUpdate {
            status: goal.status.slug().to_string(),
            objective: goal.objective.clone(),
            token_budget: goal.token_budget,
            tokens_used: Some(goal.tokens_used),
            created_at: goal.created_at,
            updated_at: goal.updated_at,
            last_reason: goal.last_reason.clone(),
        })
        .await;
    }

    /// Publish one adapter event at the active turn (namespaced metas).
    pub async fn publish_engine_event(&self, event: &AcpEngineEvent) {
        let turn_id = self.producer.active_prompt_turn().await;
        let mut mapping = MappingState::default();
        let updates = acp_updates_for_event(event, &mut mapping);
        for update in updates {
            self.producer
                .publish(&update, turn_id, UnharnessedEventPhase::Event, None)
                .await;
        }
        // A compaction rebuilt the context and paid for its summary.
        if matches!(event, AcpEngineEvent::CompactionEnd { .. }) {
            self.usage.changed().await;
        }
    }
}

/// Map the engine loop events onto ACP updates for the session lifetime.
///
/// Message-end hooks mirror the TS session's `message_end` listeners:
/// per-message autonomous usage accounting, and goal usage recording with a
/// `_meta.goal` update whenever the goal state changes mid-turn.
#[allow(clippy::too_many_arguments)]
async fn subscribe_engine_events(
    engine: &Arc<uh_core::session_engine::engine::SessionEngine>,
    producer: Arc<UpdateProducer>,
    mapping: Arc<Mutex<MappingState>>,
    autonomous: Arc<Mutex<uh_core::autonomous::AutonomousRuntimeState>>,
    autonomous_driver: Arc<dyn uh_core::autonomous::AutonomousDriver>,
    last_published_goal: Arc<Mutex<uh_core::goals::GoalState>>,
    arms: Arc<CompactionArms>,
    goal_budget_crossed: Arc<AtomicBool>,
    usage: Arc<UsageReporter>,
    steers: Arc<std::sync::Mutex<Vec<i64>>>,
    cancel_requested: Arc<AtomicBool>,
) -> Subscription {
    let agent = engine.session.agent().clone();
    let weak_agent = Arc::downgrade(&agent);
    let goal_driver = engine.goal_driver.clone();
    let session = engine.session.shared_persistence();
    agent
        .subscribe(move |event, _signal| {
            let producer = producer.clone();
            let mapping = mapping.clone();
            let autonomous = autonomous.clone();
            let autonomous_driver = autonomous_driver.clone();
            let goal_driver = goal_driver.clone();
            let session = session.clone();
            let last_published_goal = last_published_goal.clone();
            let arms = arms.clone();
            let goal_budget_crossed = goal_budget_crossed.clone();
            let usage = usage.clone();
            let steers = steers.clone();
            let cancel_requested = cancel_requested.clone();
            let weak_agent = weak_agent.clone();
            Box::pin(async move {
                // Cover cancellation between an admission check and run
                // registration, including provider retries and wake turns.
                if matches!(event, AgentEvent::AgentStart)
                    && cancel_requested.load(Ordering::SeqCst)
                {
                    if let Some(agent) = weak_agent.upgrade() {
                        agent.abort();
                    }
                    return Err(uh_agent::abort::aborted_error());
                }
                // A user row that starts an agent run resets the overflow
                // recovery machine (TS `startsAgentRun` at
                // `message_start`): the stale attempt state from the
                // previous run never suppresses a fresh overflow.
                if let AgentEvent::MessageStart {
                    message: AgentMessage::Standard(Message::User(_)),
                } = &event
                {
                    arms.reset();
                }
                let turn_id = producer.active_prompt_turn().await;
                // A steered message joins the running turn: from it on, the
                // client shows a turn of its own, which goes back to before it.
                if let AgentEvent::MessageStart {
                    message: AgentMessage::Standard(Message::User(user)),
                } = &event
                {
                    let steered = {
                        let mut steers = steers
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let at = steers.iter().position(|&stamp| stamp == user.timestamp);
                        at.map(|at| steers.remove(at)).is_some()
                    };
                    if steered {
                        // Marked in the session file, before its row: a
                        // replay tells it from a prompt after a stop.
                        let rewind_to = {
                            let mut persistence = session.lock().await;
                            let leaf = persistence.get_leaf_id().map(str::to_string);
                            if let Err(error) = persistence.append_custom_entry(STEER_CUSTOM_TYPE, None) {
                                eprintln!("uh-daemon: the steer mark was not saved: {error:#}");
                            }
                            leaf
                        };
                        let update = AcpSessionUpdate::SessionInfoUpdate {
                            meta: super::meta::unharnessed_meta(&UnharnessedSessionMeta {
                                turn_start: Some(super::meta::UnharnessedTurnStartMeta {
                                    origin: "steer".to_string(),
                                    wake: None,
                                    rewind_to,
                                }),
                                ..Default::default()
                            }),
                        };
                        producer
                            .publish(&update, turn_id, UnharnessedEventPhase::Event, None)
                            .await;
                    }
                }
                let engine_events: Vec<AcpEngineEvent> = project_event(&event);
                for engine_event in engine_events {
                    let updates = {
                        let mut mapping = mapping.lock().await;
                        acp_updates_for_event(&engine_event, &mut mapping)
                    };
                    for update in updates {
                        producer
                            .publish(&update, turn_id, UnharnessedEventPhase::Event, None)
                            .await;
                    }
                }
                // Each settled message grows the context; a reply also
                // measures it and adds what it cost.
                if let AgentEvent::MessageEnd { .. } = &event {
                    usage.changed().await;
                }
                if let uh_agent::types::AgentEvent::MessageEnd {
                    message:
                        uh_agent::types::AgentMessage::Standard(uh_agent::types::Message::Assistant(
                            assistant,
                        )),
                } = &event
                {
                    if let Some(wire) = serde_json::to_value(assistant).ok().and_then(|value| {
                        serde_json::from_value::<uh_types::ai::AssistantMessage>(value).ok()
                    }) {
                        // Autonomous per-message accounting (non-error turns).
                        {
                            let mut state = autonomous.lock().await;
                            autonomous_driver.account_message(&mut state, &wire);
                        }
                        // Goal usage recording mirrors the TS guard
                        // (`_accountGoalUsageForAssistantMessage`): only
                        // turns that were neither errors nor aborted spend
                        // the goal's budget, and only while the goal is
                        // active; a budget crossing moves the goal to
                        // `budget_limited` (the state change publishes
                        // below) and arms the settle loop's wrap-up steer.
                        if !matches!(
                            wire.stop_reason,
                            uh_types::ai::StopReason::Error | uh_types::ai::StopReason::Aborted
                        ) {
                            let mut driver = goal_driver.lock().await;
                            let mut persistence = session.lock().await;
                            // Timestamp is the message identity for the
                            // double-counting guard: the loop does not
                            // assign message ids in-process.
                            let message_id = format!("a-{}", wire.timestamp);
                            // TS `_shouldStopAfterTurn`'s catch: goal
                            // accounting must not interrupt the loop; a
                            // failed persist only warns.
                            match driver
                                .record_assistant_usage(&mut persistence, &message_id, &wire.usage)
                            {
                                Ok(
                                    uh_core::session_engine::goal_driver::UsageOutcome::BudgetReached,
                                ) => {
                                    goal_budget_crossed.store(true, Ordering::SeqCst);
                                }
                                Ok(_) => {}
                                Err(error) => {
                                    eprintln!(
                                        "uh-daemon: goal usage accounting persist failed: {error:#}"
                                    );
                                }
                            }
                        }
                    }
                }
                // A goal state change (usage recorded, budget reached, or a
                // kernel-side complete) publishes a `_meta.goal` update.
                let goal_now = goal_driver.lock().await.state().clone();
                let changed = {
                    let mut last = last_published_goal.lock().await;
                    if *last == goal_now {
                        false
                    } else {
                        *last = goal_now.clone();
                        true
                    }
                };
                if changed {
                    let mut mapping = MappingState::default();
                    let event = AcpEngineEvent::GoalUpdate {
                        status: goal_now.status.slug().to_string(),
                        objective: goal_now.objective.clone(),
                        token_budget: goal_now.token_budget,
                        tokens_used: Some(goal_now.tokens_used),
                        created_at: goal_now.created_at,
                        updated_at: goal_now.updated_at,
                        last_reason: goal_now.last_reason.clone(),
                    };
                    for update in acp_updates_for_event(&event, &mut mapping) {
                        producer
                            .publish(&update, turn_id, UnharnessedEventPhase::Event, None)
                            .await;
                    }
                }
                Ok(())
            })
        })
        .await
}

/// Project one loop event onto the ACP adapter's input vocabulary. Streaming
/// deltas carry a plain string; the discriminator (reasoning vs visible
/// text) lives in the stream event variant.
fn project_event(event: &AgentEvent) -> Vec<AcpEngineEvent> {
    match event {
        AgentEvent::MessageStart {
            message: AgentMessage::Custom(custom),
        } if custom.payload.get("customType").and_then(Value::as_str)
            == Some(uh_core::session_engine::messages::OUTPUT_LIMIT_CUSTOM_TYPE) =>
        {
            vec![AcpEngineEvent::OutputLimit]
        }
        AgentEvent::MessageStart { message } => vec![AcpEngineEvent::MessageStart {
            role: message.role().to_string(),
        }],
        AgentEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => match assistant_message_event.as_ref() {
            AssistantMessageEvent::TextDelta { delta, .. } if !delta.is_empty() => {
                vec![AcpEngineEvent::AssistantDelta {
                    thinking: false,
                    delta: delta.clone(),
                }]
            }
            AssistantMessageEvent::ThinkingDelta { delta, .. } if !delta.is_empty() => {
                vec![AcpEngineEvent::AssistantDelta {
                    thinking: true,
                    delta: delta.clone(),
                }]
            }
            AssistantMessageEvent::ThinkingStart { .. } => {
                vec![AcpEngineEvent::Thinking { started: true }]
            }
            AssistantMessageEvent::ThinkingEnd { .. } => {
                vec![AcpEngineEvent::Thinking { started: false }]
            }
            _ => Vec::new(),
        },
        AgentEvent::MessageEnd { message } => vec![AcpEngineEvent::MessageEnd {
            role: message.role().to_string(),
        }],
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => vec![AcpEngineEvent::ToolExecutionStart {
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            args: args.clone(),
        }],
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => vec![AcpEngineEvent::ToolExecutionEnd {
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            result: serde_json::to_value(result).unwrap_or(Value::Null),
            is_error: *is_error,
        }],
        _ => Vec::new(),
    }
}

/// The transcript as it stood before a turn started, recorded so the turn's
/// own messages can be told apart from everything older. Compaction can fire
/// during a turn and rebuild the message list, so membership is tracked by
/// a content key the rebuild preserves, not by index.
#[derive(Debug, Default)]
pub struct TurnBoundary {
    keys: Vec<String>,
}

impl TurnBoundary {
    pub async fn capture(agent: &Agent) -> TurnBoundary {
        let state = agent.state().await;
        TurnBoundary {
            keys: state.messages.iter().filter_map(message_key).collect(),
        }
    }

    /// Membership check for the wire shape of an assistant message (the
    /// turn loop classifies the latest assistant against the pre-turn
    /// transcript; the key composition is the one `message_key` builds —
    /// compaction drops messages, it does not rewrite them, so the keys
    /// survive the compaction rebuild).
    pub fn contains_wire(&self, assistant: &uh_types::ai::AssistantMessage) -> bool {
        let stop_reason = serde_json::to_value(assistant.stop_reason).unwrap_or(Value::Null);
        let key = json!([
            "assistant",
            assistant.timestamp,
            stop_reason,
            assistant.error_message,
        ])
        .to_string();
        self.keys.contains(&key)
    }
}

/// Key for a kept message: (role, timestamp, stopReason, errorMessage) as the
/// wire tuple. Compaction drops messages; it does not rewrite them.
fn message_key(message: &AgentMessage) -> Option<String> {
    let AgentMessage::Standard(Message::Assistant(assistant)) = message else {
        return None;
    };
    let stop_reason = serde_json::to_value(assistant.stop_reason).unwrap_or(Value::Null);
    Some(
        json!([
            "assistant",
            assistant.timestamp,
            stop_reason,
            assistant.error_message,
        ])
        .to_string(),
    )
}

/// The newest assistant message of the transcript, when one exists (the
/// autonomous driver consults it for the settled turn).
pub async fn latest_assistant_message(agent: &Agent) -> Option<uh_types::ai::AssistantMessage> {
    let state = agent.state().await;
    state.messages.iter().rev().find_map(|message| {
        let AgentMessage::Standard(Message::Assistant(assistant)) = message else {
            return None;
        };
        serde_json::to_value(assistant)
            .ok()
            .and_then(|value| serde_json::from_value::<uh_types::ai::AssistantMessage>(value).ok())
    })
}

/// The frame pair that brackets every settled turn: the completion event and
/// the terminal quiescence envelope. In-process sessions have no RLM
/// children, so quiescence is trivially reached at settlement; the
/// autonomous accounting rides the completion update while a run is
/// enabled.
pub async fn publish_completion_envelope(
    session: &AcpSession,
    turn_id: u64,
    outcome: UnharnessedOutcome,
    autonomous: Option<&UnharnessedAutonomousMeta>,
    remaining_autonomous_continuations: u64,
) -> anyhow::Result<()> {
    let quiescence = UnharnessedQuiescenceMeta {
        remaining_autonomous_continuations,
    };
    let info_meta = UnharnessedSessionMeta {
        quiescence: Some(quiescence),
        autonomous: autonomous.cloned(),
        ..Default::default()
    };
    let completion = AcpSessionUpdate::SessionInfoUpdate {
        meta: super::meta::unharnessed_meta(&info_meta),
    };
    if !session
        .producer()
        .publish(&completion, turn_id, UnharnessedEventPhase::Event, None)
        .await
    {
        anyhow::bail!("Failed to publish ACP completion update");
    }
    let terminal = AcpSessionUpdate::SessionInfoUpdate {
        meta: super::meta::unharnessed_meta(&info_meta),
    };
    if !session
        .producer()
        .publish(
            &terminal,
            turn_id,
            UnharnessedEventPhase::TerminalQuiescence,
            Some(outcome),
        )
        .await
    {
        anyhow::bail!("Failed to publish ACP terminal quiescence update");
    }
    Ok(())
}

/// Publish the correlated response boundary in front of a prompt response.
/// `expected` tells the client whether a terminal quiescence envelope
/// follows (an accepted turn) or not (failed admission).
pub async fn publish_response_boundary(
    session: &AcpSession,
    turn_id: u64,
    expected: bool,
    outcome: UnharnessedOutcome,
) -> anyhow::Result<()> {
    let boundary = AcpSessionUpdate::SessionInfoUpdate {
        meta: super::meta::unharnessed_meta(&UnharnessedSessionMeta {
            terminal_quiescence_expected: Some(expected),
            ..Default::default()
        }),
    };
    if !session
        .producer()
        .publish(
            &boundary,
            turn_id,
            UnharnessedEventPhase::ResponseBoundary,
            Some(outcome),
        )
        .await
    {
        anyhow::bail!("Failed to publish ACP response boundary");
    }
    Ok(())
}

/// Render a prompt-block failure as the ACP invalid-params error. The TS
/// SDK validates the request schema; the Rust port validates the blocks it
/// actually reads.
pub fn prompt_block_error(id: &Value, error: &PromptBlockError) -> Value {
    jsonrpc::error_response(
        id,
        jsonrpc::INVALID_PARAMS,
        "Invalid params",
        Some(&json!({ "reason": error.to_string() })),
    )
}

/// The prompt content admitted into a turn.
#[derive(Debug, Clone, PartialEq)]
pub struct AdmittedPrompt {
    pub text: String,
    pub images: Vec<ImageBlock>,
}

impl AdmittedPrompt {
    pub fn parse(prompt: &[Value]) -> Result<AdmittedPrompt, PromptBlockError> {
        let (text, images) = parse_prompt_blocks(prompt)?;
        Ok(AdmittedPrompt { text, images })
    }
}
