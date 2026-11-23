//! ACP stdio mode: a thin JSON-RPC transport over the session engine.
//!
//! One connection hosts at most one session. `session/new` admits the
//! session (reporting a cwd mismatch instead of adopting one) and
//! advertises the model, reasoning-effort and fast-mode pickers, which
//! follow the provider logins and the model catalog refreshes,
//! `session/prompt`
//! drives one engine turn with follow-up queueing semantics,
//! `session/set_config_option` applies a picker selection, and
//! `session/cancel` / `session/close` stop work. The
//! `_unharnessed/providers/*` extension methods list the providers and run
//! their logins (`providers.rs`); the `_unharnessed/settings/*` ones read
//! and write the global settings file (`settings.rs`), the
//! `_unharnessed/mcp/*` ones list the MCP servers and remove their
//! sign-ins (`mcp_servers.rs`), and the `_unharnessed/skills/*` ones list
//! the skills and turn them on or off (`skills.rs`). `session/list` lists
//! the saved conversations, `session/load` replays the one this process
//! resumed, and `_unharnessed/sessions/delete` deletes one (`history.rs`);
//! `_unharnessed/session/rewind` takes the conversation back to before a
//! prompt, to send it again changed (`rewind.rs`). The
//! `_unharnessed/kernel/*` ones show the kernel's background commands and
//! variables (`kernel.rs`). Every frame leaves
//! through one ordered
//! write queue, so responses and `session/update` notifications interleave
//! exactly in publication order. The process exits when stdin closes or
//! SIGTERM comes, after the final kernel snapshot.

mod commands;
mod compaction_arms;
mod config_options;
mod events;
mod goal_continuation;
mod history;
mod in_process_config;
mod jsonrpc;
mod kernel;
mod mcp;
mod mcp_servers;
mod meta;
mod producer;
mod prompt;
mod providers;
mod rewind;
mod session;
mod settings;
mod skills;
mod steer;
mod types;
mod usage;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use uh_core::autonomous::create_autonomous_runtime_state;
use uh_core::session_engine::engine::SessionEngine;

use jsonrpc::Incoming;
use meta::UnharnessedSessionMeta;
use producer::UpdateProducer;
use session::AcpSession;
use types::{
    initialize_result, session_id_params, AcpSessionUpdate, AcpStopReason, AcpStopReasonResponse,
    NewSessionParams,
};

use config_options::ProviderTargetSlot;
use in_process_config::{admit_session_config, InProcessConfig};

/// Everything the mode needs from the composition root.
pub struct AcpOptions {
    /// The running session engine (`create_session` output).
    pub engine: Arc<SessionEngine>,
    /// The cwd the session actually runs in, fixed at startup.
    pub actual_cwd: PathBuf,
    /// The product version reported in `initialize`.
    pub product_version: String,
    /// The session's resolved model, for session-command executors
    /// (`/compact`) that run their own provider calls.
    pub model: Option<uh_types::ai::Model>,
    /// Resolved request API key for those executors.
    pub api_key: Option<String>,
    /// The agent dir (auth, settings, model catalog).
    pub agent_dir: PathBuf,
    /// The autonomous runtime configuration from the CLI flags.
    pub autonomous_config: Option<uh_core::autonomous::AgentAutonomousConfig>,
    /// The switchable provider target the session's stream reads per call
    /// (the composition's create-time output): a picker model switch swaps
    /// it so the next turn streams on the selected model (TS `setModel`'s
    /// stream re-registration).
    pub provider_target: ProviderTargetSlot,
    /// The provider logins (the composition root's flows over the agent
    /// dir), served as `_unharnessed/providers/*`.
    pub provider_auth: Arc<dyn crate::provider_auth::ProviderAuthCommands>,
}

/// The hosted-session slot plus the in-flight admission bookkeeping.
#[derive(Default)]
struct ConnectionState {
    session: Option<SessionEntry>,
    session_new_in_flight: bool,
    session_close_in_flight: bool,
}

/// Composition-root inputs shared by every handler: the engine plus the
/// fixed process identity (cwd, version) and the session-command inputs
/// (resolved model, api key, agent dir, autonomous config).
#[derive(Clone)]
struct AcpModeState {
    engine: Arc<SessionEngine>,
    actual_cwd: Arc<PathBuf>,
    product_version: Arc<String>,
    /// The session's live model (TS `state.model`): the composition's
    /// resolved model at admission, updated by a picker model switch so
    /// the session-command executors (`/compact`) follow the
    /// switched model, exactly like the TS session's own calls.
    model: Arc<Mutex<Option<uh_types::ai::Model>>>,
    /// The session's live request key: follows a picker model switch's
    /// registry resolution, so the session-command executors authenticate
    /// against the switched model's provider (TS re-resolves auth with
    /// the model).
    api_key: Arc<Mutex<Option<String>>>,
    /// The serialized config queue (TS `configTask`): picker switches and
    /// the trigger-consuming arms observe one another through it, so an
    /// arm never reads the pre-switch model mid-switch.
    config_queue: Arc<tokio::sync::Mutex<()>>,
    agent_dir: Arc<PathBuf>,
    provider_target: ProviderTargetSlot,
    /// The service-tier preference (the fast-mode toggle, persisted as
    /// the settings default): the live target carries it clamped to the
    /// current model.
    service_tier: Arc<Mutex<uh_types::ai::ServiceTier>>,
    autonomous_config: Option<uh_core::autonomous::AgentAutonomousConfig>,
    /// Session-scoped MCP servers live on the connection, exactly like the
    /// TS process-lifetime manager: one owner id fences them and
    /// `session/close` releases. Shared with the engine's prompt gating.
    mcp: Arc<std::sync::Mutex<uh_core::mcp::McpManager>>,
    mcp_owner_id: Arc<String>,
    mcp_server_names: Arc<Mutex<Vec<String>>>,
}

impl AcpModeState {
    /// The session's current model (the live slot the picker switch
    /// updates).
    async fn current_model(&self) -> Option<uh_types::ai::Model> {
        self.model.lock().await.clone()
    }

    /// The session's current request key (the live slot the picker switch
    /// updates).
    pub(super) async fn current_api_key(&self) -> Option<String> {
        self.api_key.lock().await.clone()
    }

    /// The session's model and request key read as ONE pair through the
    /// serialized config queue: a picker model switch holds the same
    /// queue while it swaps the slots, so a boundary arm or a
    /// session-command executor cannot authenticate with the pre-switch
    /// model and the switched provider's key (a torn pair).
    pub(super) async fn model_and_api_key(&self) -> (Option<uh_types::ai::Model>, Option<String>) {
        let _guard = self.config_queue.lock().await;
        (
            self.model.lock().await.clone(),
            self.api_key.lock().await.clone(),
        )
    }
}

/// One hosted session and its in-flight prompt turn, if any.
struct SessionEntry {
    session: Arc<AcpSession>,
    prompt_task: Option<tokio::task::JoinHandle<()>>,
    /// The running turn is a wake the agent started (a client prompt
    /// waits for it instead of being refused).
    prompt_is_wake: bool,
    /// The picker state (TS `AcpSessionEntry`'s configOptions/models).
    config: Arc<InProcessConfig>,
    /// The agent-end refresh subscription (unsubscribed at close so a
    /// closed session stops refreshing — the listener would otherwise
    /// outlive the session and pin its state).
    config_refresh: Option<uh_agent::agent::Subscription>,
}

/// Run the ACP stdio mode until stdin closes or SIGTERM comes. Returns the
/// process exit code.
///
/// # Errors
///
/// Never errors: parse failures answer on stdout and stdin close settles
/// the loop (the `Result` return keeps the stdio entry points uniform).
pub async fn run_acp_mode(options: AcpOptions) -> Result<i32> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(frame) = rx.recv().await {
            let Ok(mut line) = serde_json::to_string(&frame) else {
                continue;
            };
            line.push('\n');
            if stdout.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if stdout.flush().await.is_err() {
                break;
            }
        }
    });

    let state = Arc::new(Mutex::new(ConnectionState::default()));
    let service_tier =
        uh_core::settings::SettingsManager::create(&options.actual_cwd, &options.agent_dir)
            .get_default_service_tier();
    let mode = AcpModeState {
        engine: options.engine.clone(),
        actual_cwd: Arc::new(options.actual_cwd.clone()),
        product_version: Arc::new(options.product_version.clone()),
        model: Arc::new(Mutex::new(options.model.clone())),
        api_key: Arc::new(Mutex::new(options.api_key.clone())),
        config_queue: Arc::new(tokio::sync::Mutex::new(())),
        agent_dir: Arc::new(options.agent_dir.clone()),
        provider_target: options.provider_target.clone(),
        service_tier: Arc::new(Mutex::new(service_tier)),
        autonomous_config: options.autonomous_config.clone(),
        // One MCP store with the engine's prompt gating (the core engine
        // builds it at session assembly): admitted servers reach the model
        // through the same manager the kernel `mcp.*` handlers resolve.
        mcp: options.engine.mcp_manager.clone(),
        mcp_owner_id: Arc::new(uuid::Uuid::new_v4().to_string()),
        mcp_server_names: Arc::new(Mutex::new(Vec::new())),
    };
    // The idle wakes (UNHARNESSED_PLAN.md Q7): the agent starts a turn on
    // its own when a detached `bash()` finishes, a goal backoff passes or
    // a quota park resets while no prompt runs.
    let wake_watcher = tokio::spawn(watch_wakes(state.clone(), mode.clone(), tx.clone()));
    let kernel_watcher = tokio::spawn(kernel::watch(state.clone(), mode.clone(), tx.clone()));
    let providers =
        providers::Providers::new(options.provider_auth.clone(), options.agent_dir.clone());
    let models_watcher = tokio::spawn(watch_models(
        state.clone(),
        mode.clone(),
        providers.auth_changes(),
    ));
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    let terminated = terminated();
    tokio::pin!(terminated);
    loop {
        line.clear();
        tokio::select! {
            read = stdin.read_line(&mut line) => match read {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            },
            () = &mut terminated => break,
        }
        if line.trim().is_empty() {
            continue;
        }
        let request = match jsonrpc::parse_line(&line) {
            Ok(incoming) => incoming,
            Err(error_response) => {
                let _ = tx.send(error_response);
                continue;
            }
        };
        let handler = spawn_handler(
            request,
            state.clone(),
            mode.clone(),
            providers.clone(),
            tx.clone(),
        );
        tokio::select! {
            _ = handler => {}
            () = &mut terminated => break,
        }
    }

    // Exit when the client disconnects or SIGTERM comes: stop a running
    // login (it writes no credential), stop the resident work, release the
    // subscription, fence the producer, and let the writer drain. The
    // teardown is bounded: a turn that does not settle must not cost the
    // kernel snapshot.
    providers.cancel_login();
    wake_watcher.abort();
    kernel_watcher.abort();
    models_watcher.abort();
    let _ = tokio::time::timeout(TEARDOWN_TIMEOUT, teardown(&state, &mode)).await;
    // The final namespace snapshot (TS session dispose): the next process
    // that resumes this session revives it.
    mode.engine.dispose_kernel().await;
    drop(tx);
    let _ = writer.await;
    Ok(0)
}

/// How long the exit waits for the session to settle before the kernel
/// snapshot, which has its own bounds.
const TEARDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// SIGTERM: the client stops the agent like it closes its input, the kernel
/// saved (a signal must not lose the namespace).
async fn terminated() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            terminate.recv().await;
            return;
        }
    }
    std::future::pending::<()>().await;
}

/// Start a wake turn for each wake that comes due. A wake that cannot run
/// yet (a prompt turn holds the session, or no session is open) goes back
/// to the queue until the next change: the turn's end is one.
async fn watch_wakes(
    state: Arc<Mutex<ConnectionState>>,
    mode: AcpModeState,
    tx: producer::FrameSink,
) {
    loop {
        let Some(wake) = mode.engine.next_wake(None).await else {
            continue;
        };
        if let Err(wake) = prompt::start_wake_turn(wake, &state, &mode, &tx).await {
            mode.engine.wakes().requeue(wake);
            mode.engine.wakes().changed().await;
        }
    }
}

/// Keep the pickers in step with what the agent can use. The model
/// catalog refreshes at startup and after each provider login or logout
/// (forced, like the TS registry's auth-change refresh), then hourly; the
/// models are discovered again after each refresh, and at once after a
/// login or logout, before its refresh lands.
async fn watch_models(
    state: Arc<Mutex<ConnectionState>>,
    mode: AcpModeState,
    auth_changes: Arc<tokio::sync::Notify>,
) {
    let catalog = uh_core::models::catalog_for(Some(&mode.agent_dir.join("models.json")));
    let mut hourly = tokio::time::interval(std::time::Duration::from_hours(1));
    // The interval's first tick is immediate: the startup refresh.
    let mut startup = true;
    loop {
        let forced = tokio::select! {
            _ = hourly.tick() => std::mem::take(&mut startup),
            () = auth_changes.notified() => {
                in_process_config::refresh_after_auth_change(&state, &mode).await;
                true
            }
        };
        let (credentials, openrouter) = tokio::task::spawn_blocking({
            let agent_dir = Arc::clone(&mode.agent_dir);
            move || {
                (
                    uh_core::models::prime_credentials_for_dir(&agent_dir),
                    uh_core::models::openrouter_signed_in(&agent_dir),
                )
            }
        })
        .await
        .unwrap_or_default();
        catalog
            .refresh_with_credentials(forced, credentials.as_ref())
            .await;
        if openrouter {
            catalog.refresh_openrouter(forced).await;
        }
        in_process_config::refresh_after_auth_change(&state, &mode).await;
    }
}

/// Stop the hosted session after stdin closes: abort work, settle the prompt
/// task, release the subscription, and fence the producer.
async fn teardown(state: &Arc<Mutex<ConnectionState>>, mode: &AcpModeState) {
    let entry = {
        let mut state = state.lock().await;
        state.session.take()
    };
    let Some(mut entry) = entry else {
        return;
    };
    entry.session.request_cancel();
    entry.session.agent().wait_for_idle().await;
    if let Some(task) = entry.prompt_task.take() {
        let _ = task.await;
    }
    // The refresh listener goes away with the session, then the
    // serialized config work settles before the producer fences.
    if let Some(subscription) = entry.config_refresh.take() {
        subscription.unsubscribe().await;
    }
    let _ = mode.config_queue.lock().await;
    entry.session.unsubscribe().await;
    entry.session.close_producer().await;
}

#[allow(clippy::too_many_arguments)]
fn spawn_handler(
    request: Incoming,
    state: Arc<Mutex<ConnectionState>>,
    mode: AcpModeState,
    providers: providers::Providers,
    tx: producer::FrameSink,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        match request {
            Incoming::Request { id, method, params } => {
                if !providers.handle_request(&id, &method, &params, &tx).await {
                    handle_request(id, method, params, state, mode, tx).await;
                }
            }
            Incoming::Notification { method, params } => {
                if method == providers::LOGIN_CANCEL {
                    providers.cancel_login();
                } else {
                    handle_notification(method, params, state).await;
                }
            }
        }
    })
}

#[allow(clippy::too_many_arguments)]
async fn handle_request(
    id: Value,
    method: String,
    params: Value,
    state: Arc<Mutex<ConnectionState>>,
    mode: AcpModeState,
    tx: producer::FrameSink,
) {
    let (cwd, agent_dir) = (&mode.actual_cwd, &mode.agent_dir);
    if settings::handle_request(&id, &method, &params, cwd, agent_dir, &tx).await
        || mcp_servers::handle_request(&id, &method, &params, cwd, agent_dir, &tx).await
        || skills::handle_request(
            &id,
            &method,
            &params,
            cwd,
            agent_dir,
            &mode.engine.skills,
            &tx,
        )
        .await
        || history::handle_request(&id, &method, &params, &mode.engine, agent_dir, &tx).await
        || kernel::handle_request(&id, &method, &params, &state, &mode, &tx).await
    {
        return;
    }
    match method.as_str() {
        "initialize" => handle_initialize(&id, &params, &mode.product_version, &tx),
        "session/new" => {
            handle_session_new(id, params, state, mode, tx).await;
        }
        "session/load" => {
            handle_session_load(id, params, state, mode, tx).await;
        }
        "session/list" => handle_session_list(&id, &params, &mode, &tx).await,
        "session/prompt" => {
            prompt::handle_session_prompt(id, params, state, mode, tx).await;
        }
        "session/set_config_option" => {
            in_process_config::handle_set_config_option(id, params, state, mode, tx).await;
        }
        "session/close" => {
            handle_session_close(id, params, state, mode, tx).await;
        }
        rewind::REWIND => rewind::handle_rewind(id, params, state, mode, tx).await,
        steer::STEER => steer::handle_steer(id, params, state, mode, tx).await,
        other => {
            let _ = tx.send(jsonrpc::error_response(
                &id,
                jsonrpc::METHOD_NOT_FOUND,
                &format!("\"Method not found\": {other}"),
                Some(&json!({ "method": other })),
            ));
        }
    }
}

fn handle_initialize(id: &Value, params: &Value, product_version: &str, tx: &producer::FrameSink) {
    if let Err(error_response) = validate_initialize(id, params) {
        let _ = tx.send(error_response);
        return;
    }
    let result = serde_json::to_value(initialize_result(product_version)).expect("serializes");
    let _ = tx.send(jsonrpc::response(id, &result));
}

/// The `initialize` schema check the TS SDK performs: the protocol version
/// must be a number. The error body mirrors the observed TS response.
fn validate_initialize(id: &Value, params: &Value) -> std::result::Result<(), Value> {
    let field_error = |received: &str| {
        jsonrpc::error_response(
            id,
            jsonrpc::INVALID_PARAMS,
            "Invalid params",
            Some(&json!({
                "_errors": [],
                "protocolVersion": {
                    "_errors": [format!("Invalid input: expected number, received {received}")]
                },
            })),
        )
    };
    match params.get("protocolVersion") {
        None => Err(field_error("undefined")),
        Some(value) if value.is_number() => Ok(()),
        Some(Value::String(_)) => Err(field_error("string")),
        Some(Value::Bool(_)) => Err(field_error("boolean")),
        Some(Value::Null) => Err(field_error("null")),
        Some(_) => Err(field_error("object")),
    }
}

async fn handle_notification(method: String, params: Value, state: Arc<Mutex<ConnectionState>>) {
    if method != "session/cancel" {
        return;
    }
    let session_id = session_id_params(&params);
    // Only cancel the addressed session: aborting unconditionally would kill
    // whichever turn happens to be running, and leave the real turn's stop
    // reason wrong.
    let state = state.lock().await;
    if let Some(entry) = state
        .session
        .as_ref()
        .filter(|entry| entry.session.id == session_id && entry.prompt_task.is_some())
    {
        // Serialize the flag AND aborts with slot release/admission: neither
        // a stale flag nor a late abort may reach the next prompt or wake.
        entry.session.request_cancel();
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_session_new(
    id: Value,
    params: Value,
    state: Arc<Mutex<ConnectionState>>,
    mode: AcpModeState,
    tx: producer::FrameSink,
) {
    // Reserve the single-session slot before the first await: two
    // concurrent requests must not both pass the empty-slot check while
    // cwd reads are in flight.
    {
        let mut state = state.lock().await;
        if state.session.is_some() || state.session_new_in_flight || state.session_close_in_flight {
            let _ = tx.send(internal_error(
                &id,
                "unharnessed ACP mode hosts one session per connection; start another unharnessed process for a second session",
            ));
            return;
        }
        state.session_new_in_flight = true;
    }

    let result = session_new(&id, params, &mode, tx.clone(), None).await;
    match result {
        // An admission failure has already queued its own error response;
        // only the in-flight flag is left to clear.
        Err(()) => {
            let mut state = state.lock().await;
            state.session_new_in_flight = false;
        }
        Ok((entry, result)) => {
            // Install the session before the admission response leaves: a
            // client that immediately sends `session/set_config_option`
            // must resolve against the installed session, not "Unknown
            // ACP session" (TS assigns `session = entry` before returning
            // the response). The producer gate opens only after the
            // response is queued on the sink, so no held update can
            // precede the admission response.
            let producer = Arc::clone(entry.session.producer());
            let usage = Arc::clone(&entry.session.usage);
            let mut state = state.lock().await;
            state.session_new_in_flight = false;
            state.session = Some(entry);
            drop(state);
            let _ = tx.send(jsonrpc::response(&id, &result));
            producer.commit_session_new_response().await;
            // The slash commands the prompt takes, once per session.
            let commands = AcpSessionUpdate::AvailableCommandsUpdate {
                available_commands: commands::available_commands(&mode.engine),
            };
            producer
                .publish(&commands, 0, meta::UnharnessedEventPhase::Event, None)
                .await;
            usage.changed().await;
        }
    }
}

/// `session/load`: admit the session this process was started on
/// (`--resume`) under its own id, replay its conversation, then answer
/// like `session/new`. The replay goes before the answer (ACP), so the
/// producer gate opens first.
async fn handle_session_load(
    id: Value,
    params: Value,
    state: Arc<Mutex<ConnectionState>>,
    mode: AcpModeState,
    tx: producer::FrameSink,
) {
    {
        let mut state = state.lock().await;
        if state.session.is_some() || state.session_new_in_flight || state.session_close_in_flight {
            let _ = tx.send(internal_error(
                &id,
                "unharnessed ACP mode hosts one session per connection; start another unharnessed process for a second session",
            ));
            return;
        }
        state.session_new_in_flight = true;
    }
    let requested = session_id_params(&params);
    let (held, file, leaf) = {
        let session = mode.engine.session.shared_persistence();
        let session = session.lock().await;
        (
            session.get_session_id().to_string(),
            session.get_session_file().map(Path::to_path_buf),
            session.get_leaf_id().map(str::to_string),
        )
    };
    if requested != held {
        let _ = tx.send(internal_error(
            &id,
            &format!("This process holds session {held}: start unharnessed with --resume <its file> to load {requested}."),
        ));
        state.lock().await.session_new_in_flight = false;
        return;
    }
    let Ok((entry, result)) = session_new(&id, params, &mode, tx.clone(), Some(held)).await else {
        state.lock().await.session_new_in_flight = false;
        return;
    };
    let producer = Arc::clone(entry.session.producer());
    let usage = Arc::clone(&entry.session.usage);
    let loaded = Arc::clone(&entry.session);
    {
        let mut state = state.lock().await;
        state.session_new_in_flight = false;
        state.session = Some(entry);
    }
    producer.commit_session_new_response().await;
    let entries = match file {
        Some(file) => tokio::task::spawn_blocking(move || {
            uh_core::session::manager::load_entries_from_file(&file, false)
        })
        .await
        .unwrap_or_default(),
        None => Vec::new(),
    };
    for turn in history::replay(&entries, leaf.as_deref()) {
        let turn_id = producer.begin_prompt().await;
        for update in &turn {
            producer
                .publish(update, turn_id, meta::UnharnessedEventPhase::Event, None)
                .await;
        }
        producer.finish_prompt(turn_id).await;
    }
    // A kernel that revived the saved variables before the session opened.
    kernel::report_restore(&mode.engine, &loaded.id, &tx).await;
    let _ = tx.send(jsonrpc::response(&id, &result));
    let commands = AcpSessionUpdate::AvailableCommandsUpdate {
        available_commands: commands::available_commands(&mode.engine),
    };
    producer
        .publish(&commands, 0, meta::UnharnessedEventPhase::Event, None)
        .await;
    usage.changed().await;
    loaded.publish_loaded_goal().await;
}

/// `session/list`: the saved conversations (`history.rs`), in the
/// sessions folder this process writes to.
async fn handle_session_list(
    id: &Value,
    params: &Value,
    mode: &AcpModeState,
    tx: &producer::FrameSink,
) {
    let cwd = params.get("cwd").and_then(Value::as_str).map(PathBuf::from);
    let (session_dir, current) = {
        let session = mode.engine.session.shared_persistence();
        let session = session.lock().await;
        (
            session.get_session_dir().to_path_buf(),
            session.get_session_id().to_string(),
        )
    };
    let sessions =
        tokio::task::spawn_blocking(move || history::list(&session_dir, cwd.as_deref(), &current))
            .await
            .unwrap_or_default();
    let _ = tx.send(jsonrpc::response(id, &json!({ "sessions": sessions })));
}

/// Admit one session, under `session_id` (a loaded one) or a new id. On
/// failure the error response has already been queued; on success the
/// admission response is returned UNSENT: the caller installs the entry,
/// queues the response, then opens the producer gate, in that order.
async fn session_new(
    id: &Value,
    params: Value,
    mode: &AcpModeState,
    tx: producer::FrameSink,
    session_id: Option<String>,
) -> std::result::Result<(SessionEntry, Value), ()> {
    let params = NewSessionParams::parse(&params);
    // MCP admission precedes everything else in the session identity: a
    // rejected server list fails the request with the raw error payload.
    // The zod-shaped filter drops schema-invalid entries silently (SDK
    // `vecSkipError`); validation errors are `invalid params` with a
    // `reason`, admission failures internal errors with `details`.
    if let Err(mut response) = mcp::admit_session_servers(&params.mcp_servers, mode).await {
        if let Value::Object(_) = &response {
            if let Some(id_slot) = response.get_mut("id") {
                *id_slot = id.clone();
            }
        }
        let _ = tx.send(response);
        return Err(());
    }
    // The agent's cwd is fixed at startup; a client-supplied cwd is reported
    // back in `_meta` when it differs, never adopted.
    let mut cwd_mismatch = None;
    if let Some(requested) = params.cwd.as_deref().filter(|cwd| !cwd.is_empty()) {
        if !same_cwd(Path::new(requested), &mode.actual_cwd) {
            cwd_mismatch = Some(meta::UnharnessedCwdMeta {
                requested: requested.to_string(),
                actual: mode.actual_cwd.display().to_string(),
            });
        }
    }

    // The session's picker state: discovery and the initial options must
    // not block admission (TS `entry.models = []` on a discovery failure).
    let config = admit_session_config(mode).await;

    let session_id = session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let producer = UpdateProducer::new(session_id.clone(), tx.clone());
    // Autonomous state is session-scoped, like the TS session it backs.
    let autonomous = Arc::new(tokio::sync::Mutex::new(create_autonomous_runtime_state(
        mode.autonomous_config.as_ref(),
        None,
    )));
    let driver: Arc<dyn uh_core::autonomous::AutonomousDriver> = Arc::new(
        uh_core::autonomous::ShellAutonomousDriver::new(mode.actual_cwd.as_path()),
    );
    let session = Arc::new(
        AcpSession::new(
            session_id.clone(),
            mode.engine.clone(),
            mode.model.clone(),
            producer.clone(),
            autonomous,
            driver,
        )
        .await,
    );
    // The engine subscription refreshes the pickers at the end of every
    // agent run (the TS `agent_end` trigger), through the same serialized
    // queue as the request handler. The subscription is retained on the
    // entry so close removes it (a listener that outlives the session
    // keeps refreshing it).
    let config_refresh =
        in_process_config::wire_config_refresh(&session, Arc::clone(&config), mode.clone()).await;

    let saved = {
        let session = mode.engine.session.shared_persistence();
        let session = session.lock().await;
        meta::UnharnessedSavedMeta {
            session_id: session.get_session_id().to_string(),
            path: session
                .get_session_file()
                .map(|path| path.display().to_string()),
        }
    };
    let result = json!({
        "sessionId": session_id,
        "configOptions": *config.published.lock().await,
        "_meta": meta::unharnessed_meta(&UnharnessedSessionMeta {
            cwd: cwd_mismatch,
            saved: Some(saved),
            ..Default::default()
        }),
    });
    Ok((
        SessionEntry {
            session,
            prompt_task: None,
            prompt_is_wake: false,
            config,
            config_refresh: Some(config_refresh),
        },
        result,
    ))
}

async fn handle_session_close(
    id: Value,
    params: Value,
    state: Arc<Mutex<ConnectionState>>,
    mode: AcpModeState,
    tx: producer::FrameSink,
) {
    let session_id = session_id_params(&params);
    // Stop real work, not just local bookkeeping: closing aborts the
    // connection the same way session/cancel does.
    let taken = {
        let mut state = state.lock().await;
        if state.session_close_in_flight {
            None
        } else {
            match state.session.as_ref() {
                Some(entry) if entry.session.id == session_id => {
                    state.session_close_in_flight = true;
                    state.session.take()
                }
                _ => {
                    let _ = tx.send(internal_error(
                        &id,
                        &format!("Unknown ACP session: {session_id}"),
                    ));
                    return;
                }
            }
        }
    };
    let Some(mut entry) = taken else {
        let _ = tx.send(internal_error(
            &id,
            &format!("ACP session is already closing: {session_id}"),
        ));
        return;
    };
    entry.session.request_cancel();
    entry.session.agent().wait_for_idle().await;
    // The cancelled prompt resolves before the close response: the turn task
    // is awaited first and its frames already sit in the write queue.
    if let Some(task) = entry.prompt_task.take() {
        let _ = task.await;
    }
    // The refresh listener goes away with the session (no later agent
    // run refreshes a closed session), then the serialized config work
    // settles before the producer fences (TS `await configTask` in
    // `session/close`).
    if let Some(subscription) = entry.config_refresh.take() {
        subscription.unsubscribe().await;
    }
    let _ = mode.config_queue.lock().await;
    entry.session.unsubscribe().await;
    // Keep the backing session fenced until a replacement ACP session is
    // admitted.
    entry.session.close_producer().await;
    mcp::release_session_servers(&mode).await;
    // Flush the namespace snapshot and release the kernel, revivable: a
    // replacement session on this connection boots a fresh kernel that
    // revives it, and so does a later process resuming the session.
    mode.engine.stop_kernel_snapshot().await;
    let _ = tx.send(jsonrpc::response(&id, &json!({})));
    let mut state = state.lock().await;
    state.session_close_in_flight = false;
}

/// The `session/prompt` success response: the terminal stop reason.
fn stop_reason_response(stop_reason: AcpStopReason) -> Value {
    serde_json::to_value(AcpStopReasonResponse { stop_reason }).expect("serializes")
}

fn internal_error(id: &Value, details: &str) -> Value {
    jsonrpc::error_response(
        id,
        jsonrpc::INTERNAL_ERROR,
        "Internal error",
        Some(&json!({ "details": details })),
    )
}

/// The internal-error response with a null id, for handlers that apply the
/// request id after an async admission decision.
pub(super) fn internal_error_value(details: &str) -> Value {
    internal_error(&Value::Null, details)
}

/// Two paths are the same cwd when their canonical forms match, or when they
/// are the same directory on disk (dev/inode) — the bind-mount and
/// case-normalized-FS cases a lexical comparison misses.
fn same_cwd(requested: &Path, actual: &Path) -> bool {
    let canonical = |path: &Path| -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    };
    let requested = canonical(requested);
    let actual = canonical(actual);
    if requested == actual {
        return true;
    }
    #[cfg(unix)]
    {
        let identity = |path: &Path| -> Option<(u64, u64)> {
            use std::os::unix::fs::MetadataExt;
            let metadata = std::fs::metadata(path).ok()?;
            if metadata.dev() == 0 || metadata.ino() == 0 {
                return None;
            }
            Some((metadata.dev(), metadata.ino()))
        };
        if let (Some(left), Some(right)) = (identity(&requested), identity(&actual)) {
            return left == right;
        }
    }
    false
}
