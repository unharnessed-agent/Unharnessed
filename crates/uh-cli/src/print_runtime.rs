//! The headless print runtime: single-shot prompt -> answer over the uh-core
//! session engine with a real uh-ai provider. Port of the text-mode half of
//! modes/print-mode.ts wired onto `create_session` (the Rust engine facade).

use std::sync::Arc;

use uh_agent::types::Model as AgentModel;
use uh_core::session::discovery::{
    find_most_recent_session_for_cwd, resolve_session_path, ResolvedSession, SessionSelectorError,
};
use uh_types::ai::Model;

use crate::headless_autonomous::{autonomous_runtime_config, HeadlessAutonomous};
use crate::mode::{AppMode, MissingSubsystem, RunOptions};
use uh_agent::stream::{LlmContext, StreamFn, StreamRequestOptions};
use uh_core::session_engine::provider_adapter::{
    json_round_trip, map_thinking_level, stream_once, switchable_stream_fn, ProviderTarget,
};
use uh_core::session_engine::session_events::agent_event_json;

/// The runtime: implements the print (text) mode against the merged session
/// engine. Modes not wired here still report their typed missing subsystem.
pub struct PrintRuntime;

impl crate::mode::Runtime for PrintRuntime {
    fn run(&self, options: &RunOptions) -> Result<i32, MissingSubsystem> {
        // `model list` takes the full runtime path in every mode and exits
        // (TS main: listModels runs after session assembly, before any mode
        // transport, and exits 0).
        if options.list_models.is_some() {
            return match crate::list_models::run(options) {
                Ok(code) => Ok(code),
                Err(message) => {
                    eprintln!("Error: {message}");
                    Ok(1)
                }
            };
        }
        match options.app_mode {
            // Runtime failures print themselves and exit non-zero; the typed
            // MissingSubsystem channel stays reserved for unwired subsystems.
            AppMode::Print | AppMode::Json => match run_print_mode(options) {
                Ok(code) => Ok(code),
                Err(message) => {
                    eprintln!("Error: {message}");
                    Ok(1)
                }
            },
            // A terminal launch without `-p`/`--mode`: the interactive
            // terminal, an ACP client of this same binary. Its output must
            // be a terminal too.
            AppMode::Interactive => {
                if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                    Ok(crate::terminal::run(options))
                } else {
                    eprintln!(
                        "Error: the interactive terminal needs a terminal on stdout; use -p/--print, --mode json, or --mode acp"
                    );
                    Ok(1)
                }
            }
            // ACP mode: a thin JSON-RPC stdio transport over the same
            // in-process session engine the print mode uses.
            AppMode::Acp => match run_acp_mode(options) {
                Ok(code) => Ok(code),
                Err(error) => {
                    eprintln!("Error: {error:#}");
                    Ok(1)
                }
            },
        }
    }
}

/// The ACP headless mode: build the in-process session engine the same way
/// the print mode does, then serve the ACP JSON-RPC surface over stdio until
/// the client disconnects.
fn run_acp_mode(options: &RunOptions) -> Result<i32, String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let code = rt.block_on(acp_mode_main(options));
    // After SIGTERM the stdin reader is still blocked in its read, which
    // nothing can cancel: the runtime gives the other blocking work a
    // moment, not that read forever.
    rt.shutdown_timeout(std::time::Duration::from_secs(1));
    code
}

async fn acp_mode_main(options: &RunOptions) -> Result<i32, String> {
    let config = &options.config;
    let engine = build_headless_engine_parts(options).await?;
    let exit_code = uh_daemon::acp::run_acp_mode(uh_daemon::acp::AcpOptions {
        engine: std::sync::Arc::new(engine.engine),
        actual_cwd: config.cwd.clone(),
        product_version: crate::config::version().to_string(),
        model: Some(engine.model),
        api_key: engine.api_key,
        agent_dir: config.agent_dir.clone(),
        provider_target: engine.provider_target,
        provider_auth: std::sync::Arc::new(crate::provider_login::ProviderAuth::new(
            config.cwd.clone(),
            config.agent_dir.clone(),
        )),
        autonomous_config: options
            .config
            .autonomous
            .as_ref()
            .map(autonomous_runtime_config),
    })
    .await
    .map_err(|error| format!("{error:#}"))?;
    Ok(exit_code)
}

fn run_print_mode(options: &RunOptions) -> Result<i32, String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    rt.block_on(print_mode_main(options))
}

async fn print_mode_main(options: &RunOptions) -> Result<i32, String> {
    let headless = build_headless_engine(options).await?;
    let engine = std::sync::Arc::new(headless.engine);
    // The CLI `--goal` seed (TS constructor seeding): a fresh root branch
    // starts the goal and queues its continuation context as the first
    // turn's leading row; a resumed or already-seeded branch keeps its
    // persisted goal.
    if let Some(goal) = &options.config.initial_goal {
        engine
            .seed_initial_goal(&goal.objective, goal.token_budget.map(u64::from))
            .await
            .map_err(|error| format!("{error:#}"))?;
    }
    let outcome = run_prompts_and_emit(
        &engine,
        &headless.model,
        headless.api_key.clone(),
        &headless.provider_target,
        options,
    )
    .await;
    // The kernel shutdown (TS session dispose): a persisted session takes
    // its final namespace snapshot here (the debounced flush cannot outlive
    // this process) for the next `-c`/`-r` run to revive, and the
    // background `bash()` commands still alive die with the kernel
    // (UNHARNESSED_PLAN.md Q7), with or without a session.
    engine.dispose_kernel().await;
    outcome
}

/// Assemble the in-process session engine for a headless run: model
/// resolution, session persistence, and the engine facade. The faux-script
/// seam (`UNHARNESSED_FAUX_SCRIPT`) drives the same assembly without the
/// network; verification harness only, never set by the product.
///
/// The switchable provider target the session's stream reads per call
/// (shared with the ACP mode, whose picker model switches swap it live).
pub type ProviderTargetSlot = std::sync::Arc<std::sync::RwLock<Option<ProviderTarget>>>;

/// The assembled headless engine plus the model and request auth it runs
/// on, so host transports can drive session-command executors
/// (compact) with the session's own model.
struct HeadlessEngine {
    engine: uh_core::session_engine::engine::SessionEngine,
    model: Model,
    api_key: Option<String>,
    /// The live provider target: the stream reads it per call, and the
    /// ACP mode's picker switches swap it (TS `setModel`'s stream
    /// re-registration; `set_model` swaps it without rebuilding the
    /// session).
    provider_target: ProviderTargetSlot,
}

async fn build_headless_engine_parts(options: &RunOptions) -> Result<HeadlessEngine, String> {
    let (engine, lease) = build_headless_engine_parts_with_lease(options).await?;
    std::mem::forget(lease);
    Ok(engine)
}

/// The same assembly, returning the opened session's runtime lease
/// alongside (a long-lived connection holds it on the engine handle so a
/// replacement releases it with the engine it guarded; the one-shot
/// modes forget it for the process lifetime).
async fn build_headless_engine_parts_with_lease(
    options: &RunOptions,
) -> Result<(HeadlessEngine, Option<uh_daemon::lease::SessionLease>), String> {
    let (session_manager, lease) = select_session_manager_with_lease(options)?;
    if let Ok(script) = std::env::var("UNHARNESSED_FAUX_SCRIPT") {
        let engine = build_faux_engine_with(options, &script, session_manager).await?;
        return Ok((engine, lease));
    }
    let engine = build_headless_engine_with(options, session_manager).await?;
    Ok((engine, lease))
}

/// The session-manager selection every engine build shares
/// (`--no-session` keeps the engine in-memory; anything else resolves
/// through the flag order), returning the opened session's runtime
/// lease.
fn select_session_manager_with_lease(
    options: &RunOptions,
) -> Result<
    (
        Option<uh_core::session::manager::SessionManager>,
        Option<uh_daemon::lease::SessionLease>,
    ),
    String,
> {
    if options.session.no_session {
        return Ok((None, None));
    }
    let (manager, lease) = build_session_manager_with_lease(options)?;
    Ok((Some(manager), lease))
}

/// The real-provider engine assembly over one session-manager selection
/// (the print/json path and the RPC mode's replacement builds).
async fn build_headless_engine_with(
    options: &RunOptions,
    session_manager: Option<uh_core::session::manager::SessionManager>,
) -> Result<HeadlessEngine, String> {
    let config = &options.config;

    // Model registry: composed catalog + models.json with real auth.
    let auth = uh_core::auth::AuthStorage::create(&config.agent_dir);
    let mut registry =
        uh_core::models::ModelRegistry::create(auth, config.agent_dir.join("models.json"));
    registry.load_private_authorization_from_cache();
    let settings = uh_core::settings::SettingsManager::create(&config.cwd, &config.agent_dir);
    let model = select_model(
        &mut registry,
        config.provider.as_deref(),
        config.model.as_deref(),
        &settings,
    )?;

    // Resolve request auth once (single-shot mode): the merged headers
    // ship on the request (the TS `getApiKeyAndHeaders` single-owner path;
    // TS #2497 removed the provider-side team-header fallback, so the
    // stored team / `PRIME_TEAM_ID` reach the wire through these headers).
    let resolved = registry.get_api_key_and_headers(&model, model.headers.as_ref());

    // The stream reads the provider target per call (the switchable seam
    // the ACP pickers swap on a model switch; image-model routing swaps it
    // per dispatched batch).
    // The settings' service tier (the fast mode) rides the target where
    // the model serves it (TS `clampServiceTier`); the default tier sends
    // nothing.
    let service_tier =
        uh_types::ai::clamp_service_tier(Some(&model), Some(settings.get_default_service_tier()))
            .filter(|tier| *tier != uh_types::ai::ServiceTier::Default);
    let provider_target: ProviderTargetSlot =
        std::sync::Arc::new(std::sync::RwLock::new(Some(ProviderTarget {
            api_key: resolved.api_key.clone(),
            model: model.clone(),
            service_tier,
            headers: resolved.headers.clone(),
        })));
    // The armed image route's target, shared with the stream seam: while
    // an episode is armed the stream serves THIS target (TS keeps the
    // routed override over the whole turn, picker switches included), so
    // a concurrent `set_model` picker write to the slot below cannot
    // redirect an in-flight routed turn's continuation requests. The
    // settle clears the slot, and the switch lands there (the settle's
    // still-routed guard leaves it).
    let armed_target: std::sync::Arc<std::sync::Mutex<Option<ProviderTarget>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let stream_fn = route_authoritative_stream_fn(
        std::sync::Arc::clone(&provider_target),
        std::sync::Arc::clone(&armed_target),
    );
    // TS settings.imageModel routing (the headless surfaces' host seam):
    // image-attaching batches on a session model without image input
    // route to the configured image model or fail the turn with the
    // actionable refusal naming the setting.
    // The routing decision reads the resolved session model live (a
    // mid-run switch rewrites the serving slot) and receives the LIVE
    // thinking level per batch (the engine passes its agent state's level,
    // so a mid-run `/effort` or model switch never routes at a stale level).
    let image_model_router = headless_image_model_router(
        &provider_target,
        std::sync::Arc::clone(&armed_target),
        config.cwd.clone(),
        config.agent_dir.clone(),
        model.clone(),
    );
    let agent_model: AgentModel = json_round_trip(&model).ok_or("model conversion failed")?;

    // TS `sdk.ts` seeds the Agent's queue modes from the settings manager
    // (`steeringMode`/`followUpMode`): the print runtime reads the same
    // settings, so the agent-level queues drain per
    // the configured modes (the steering default is "all"; follow-ups
    // keep "one-at-a-time").
    let queue_settings = uh_core::settings::SettingsManager::create(&config.cwd, &config.agent_dir);
    let queue_mode = |mode: uh_core::settings::QueueModeSetting| match mode {
        uh_core::settings::QueueModeSetting::All => uh_agent::agent::QueueMode::All,
        uh_core::settings::QueueModeSetting::OneAtATime => uh_agent::agent::QueueMode::OneAtATime,
    };
    let steering_mode = Some(queue_mode(queue_settings.get_steering_mode()));
    let follow_up_mode = Some(queue_mode(queue_settings.get_follow_up_mode()));
    // TS `createAgentSessionServices` builds every CLI session — print
    // included — on a manager whose `getUserServers`/`getCatalogSources`
    // closures re-read settings on every resolution (construction and
    // each later `refresh()`: the API the remote-catalog change
    // subscription drives mid-session), and whose declared local catalog
    // sources resolve. The session's `mcp.config` host handler keeps
    // serving the registration-time integrations (the uh-core handler
    // design, shared with the daemon worker). Auth construction blocks;
    // run it off the async runtime like the engine's own gating does.
    let mcp_manager = {
        let cwd = config.cwd.clone();
        let agent_dir = config.agent_dir.clone();
        let manager = tokio::task::spawn_blocking(move || {
            crate::mcp_login::cli_mcp_manager(&cwd, &agent_dir)
        })
        .await
        .map_err(|error| format!("MCP manager construction failed: {error}"))?;
        std::sync::Arc::new(std::sync::Mutex::new(manager))
    };
    let engine = uh_core::session_engine::engine::create_session(
        uh_core::session_engine::engine::SessionEngineConfig {
            steering_mode,
            follow_up_mode,
            cwd: config.cwd.clone(),
            agent_dir: config.agent_dir.clone(),
            mcp_manager: Some(mcp_manager),
            model: Some(agent_model),
            thinking_level: Some(resolve_thinking_level(config, &model)),
            stream_fn: Some(stream_fn),
            tools: builtin_tools(&config.cwd),
            custom_system_prompt: config.system_prompt.clone(),
            prompt_guidelines: config.append_system_prompt.clone(),
            generic_mcp_servers: vec![],
            session_manager,
            extra_host_handlers: None,
            additional_skill_paths: config
                .skills
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            additional_prompt_paths: config
                .prompt_templates
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            extra_builtin_skill_overrides: vec![],
            model_info: Some(model.clone()),
            // TS print/headless sessions build through the same
            // `createDefaultRuntimeFactory` runtime (prewarmIpythonKernel:
            // true), so the kernel boots in the background at creation;
            // the engine's depth-0 gate matches the TS session's.
            prewarm_ipython_kernel: Some(true),
            on_background_work_settled: None,
            queued_goal_context_purge: None,
            queued_steering_probe: None,
            image_model_router: Some(image_model_router),
        },
    )
    .await
    .map_err(|error| format!("{error:#}"))?;
    Ok(HeadlessEngine {
        engine,
        model,
        api_key: resolved.api_key,
        provider_target,
    })
}

/// The headless stream seam over the shared provider-target slot with the
/// armed image route kept AUTHORITATIVE while an episode is armed (TS
/// keeps the routed override over the whole turn, mid-turn picker switches
/// included): a `set_model` picker write to the slot lands only when the
/// settle clears the armed target, exactly when TS's next dispatch would
/// re-evaluate against the new selection.
fn route_authoritative_stream_fn(
    provider_target: ProviderTargetSlot,
    armed_target: std::sync::Arc<std::sync::Mutex<Option<ProviderTarget>>>,
) -> StreamFn {
    std::sync::Arc::new(
        move |_requested: AgentModel, context: LlmContext, options: StreamRequestOptions| {
            let armed = armed_target
                .lock()
                .expect("armed image target lock")
                .clone();
            let target = armed
                .or_else(|| {
                    provider_target
                        .read()
                        .expect("provider target lock")
                        .clone()
                })
                .expect("provider target set before the first stream");
            let ProviderTarget {
                api_key,
                model,
                service_tier,
                headers,
            } = target;
            Box::pin(async move {
                stream_once(&model, api_key, service_tier, headers, context, options)
            })
        },
    )
}

/// The headless image-model router (TS `resolveImageModelOverride` over the
/// CLI's settings + registry, applied to the session's swappable stream
/// target): the routing decision for one dispatched batch — `Err` is the
/// actionable refusal that fails the turn — and the serving-target swap
/// (`None` restores the session target).
fn headless_image_model_router(
    provider_target: &std::sync::Arc<
        std::sync::RwLock<Option<uh_core::session_engine::provider_adapter::ProviderTarget>>,
    >,
    armed_target: std::sync::Arc<
        std::sync::Mutex<Option<uh_core::session_engine::provider_adapter::ProviderTarget>>,
    >,
    cwd: std::path::PathBuf,
    agent_dir: std::path::PathBuf,
    session_model: uh_types::ai::Model,
) -> uh_core::session_engine::image_model_routing::ImageModelRouter {
    // The pre-route session target, captured at the FIRST arm (not at
    // build): a mid-run model switch rewrites the live slot, and the
    // capture-then-restore contract (arm -> serve the route -> settle ->
    // restore) must return the SWITCHED-TO target, never the build-time
    // snapshot. Cleared on every settle so the next arm re-captures
    // whatever the session serves by then.
    let armed_from = std::sync::Arc::new(std::sync::Mutex::new(None));
    // `armed_target` (the caller's slot, shared with the stream seam) holds
    // the routed target the arm wrote, so the settle can tell a slot that
    // still holds the route from one a mid-run `/model` switch rewrote.
    let armed_to = armed_target;
    let decide_agent_dir = agent_dir.clone();
    let decide_provider_target = std::sync::Arc::clone(provider_target);
    let decide_armed_from = std::sync::Arc::clone(&armed_from);
    let decide = std::sync::Arc::new(
        move |carries_images: bool,
              thinking_level: uh_types::ai::ModelThinkingLevel|
              -> Result<Option<uh_core::models::ResolvedImageModel>, String> {
            if !carries_images {
                return Ok(None);
            }
            // The routing decision runs at commit and needs the SESSION
            // model. During an armed episode the live slot holds the
            // ROUTED target (a consecutive image batch re-decides before
            // the previous episode settles), so the capture is the
            // session model; un-armed, the live slot is the session
            // target (a mid-run model switch rewrote it). The build-time
            // pair is the fallback only when both are somehow empty.
            let armed_capture = decide_armed_from
                .lock()
                .expect("armed-from lock")
                .as_ref()
                .map(
                    |target: &uh_core::session_engine::provider_adapter::ProviderTarget| {
                        target.model.clone()
                    },
                );
            let session_model = armed_capture
                .or_else(|| {
                    decide_provider_target
                        .read()
                        .expect("provider target lock")
                        .as_ref()
                        .map(|target| target.model.clone())
                })
                .unwrap_or_else(|| session_model.clone());
            let settings = uh_core::settings::SettingsManager::create(&cwd, &decide_agent_dir);
            let image_model_reference = settings.get_image_model();
            let block_images = settings.get_block_images();
            let auth = uh_core::auth::AuthStorage::create(&decide_agent_dir);
            let mut registry =
                uh_core::models::ModelRegistry::create(auth, decide_agent_dir.join("models.json"));
            registry.load_private_authorization_from_cache();
            let available: Vec<uh_types::ai::Model> =
                registry.get_available().into_iter().cloned().collect();
            // Route acceptance uses the same resolved-auth result the arm
            // path installs: a provider can be signed in (the status
            // probe) while its key resolution still fails, and a route
            // accepted on the status probe alone would arm an
            // unauthenticated target — the image turn's content would
            // reach the provider without credentials instead of the
            // actionable unresolvable-reference refusal (TS resolves the
            // auth at request time and fails the turn before any request
            // leaves; the port refuses the reference up front).
            let resolvable_auth: std::collections::HashSet<(String, String)> = available
                .iter()
                .filter(|model| {
                    registry
                        .get_api_key_and_headers(model, model.headers.as_ref())
                        .ok
                })
                .map(|model| (model.provider.clone(), model.id.clone()))
                .collect();
            uh_core::models::resolve_image_model_override(
                &uh_core::models::ImageModelRoutingInputs {
                    session_model: &session_model,
                    thinking_level,
                    service_tier: None,
                    image_model_reference: image_model_reference.as_deref(),
                    available_models: &available,
                    // Keyed (provider, id): one provider's authenticated
                    // row must not vouch for another provider's same-id
                    // model (the catalog allows shared ids across
                    // providers).
                    has_configured_auth: &|model| {
                        resolvable_auth.contains(&(model.provider.clone(), model.id.clone()))
                    },
                    block_images,
                },
            )
        },
    );
    let swap_target = {
        let provider_target = std::sync::Arc::clone(provider_target);
        let armed_to = std::sync::Arc::clone(&armed_to);
        std::sync::Arc::new(move |route: Option<&uh_core::models::ResolvedImageModel>| {
            if let Some(resolved) = route {
                // The first swap of the episode captures the session
                // target it replaces (the later arms re-write the slot,
                // so only the arm preceding them holds it).
                let mut armed_from = armed_from.lock().expect("armed-from lock");
                if armed_from.is_none() {
                    armed_from.clone_from(&provider_target.read().expect("provider target lock"));
                }
                // The routed model's request auth resolves like the
                // session model's did at startup (registry + headers).
                let auth = uh_core::auth::AuthStorage::create(&agent_dir);
                let mut registry =
                    uh_core::models::ModelRegistry::create(auth, agent_dir.join("models.json"));
                registry.load_private_authorization_from_cache();
                let resolved_auth = registry
                    .get_api_key_and_headers(&resolved.model, resolved.model.headers.as_ref());
                let target = uh_core::session_engine::provider_adapter::ProviderTarget {
                    api_key: resolved_auth.api_key,
                    headers: resolved_auth.headers,
                    model: resolved.model.clone(),
                    service_tier: resolved.service_tier,
                };
                // The arm records the routed target it writes so the
                // settle's still-routed guard can tell a slot the route
                // still holds from one a mid-run `/model` switch rewrote
                // (without this write the guard always passes).
                *armed_to.lock().expect("armed-to lock") = Some(target.clone());
                *provider_target.write().expect("provider target lock") = Some(target);
            } else {
                // Restore the captured session target ONLY when the slot
                // still holds the routed target the arm wrote: a mid-run
                // `/model` switch rewrote the slot with the new session
                // target, and the settle must not drag requests back to
                // the pre-route model.
                let captured = armed_from.lock().expect("armed-from lock").take();
                let routed = armed_to.lock().expect("armed-to lock").take();
                let current = provider_target
                    .read()
                    .expect("provider target lock")
                    .clone();
                // The full serving target, credentials included: an ACP
                // model switch may keep the same model id while rotating
                // its api key or headers, and the guard must treat that
                // slot as switched, not as the route's own.
                let still_routed = match (&current, &routed) {
                    (Some(current), Some(routed)) => {
                        current.model.id == routed.model.id
                            && current.service_tier == routed.service_tier
                            && current.api_key == routed.api_key
                            && current.headers == routed.headers
                    }
                    _ => true,
                };
                if still_routed {
                    if let Some(target) = captured.or(current) {
                        *provider_target.write().expect("provider target lock") = Some(target);
                    }
                }
            }
        })
    };
    uh_core::session_engine::image_model_routing::ImageModelRouter {
        decide,
        swap_target,
    }
}

/// The engine alone (callers that do not drive session commands).
async fn build_headless_engine(options: &RunOptions) -> Result<HeadlessEngine, String> {
    build_headless_engine_parts(options).await
}

/// The session header line: the session file's `type: "session"` entry in
/// the TS wire shape and field order (`getSessionHeader` ->
/// `JSON.stringify`), so a fresh run reports the same identity row the TS
/// json stream leads with.
async fn session_header_json(
    engine: &uh_core::session_engine::engine::SessionEngine,
) -> Option<String> {
    let persistence = engine.session.shared_persistence();
    let session = persistence.lock().await;
    let header = session.get_header()?;
    // The TS field order, optional fields only when present.
    let mut object = serde_json::Map::new();
    let field = |map: &mut serde_json::Map<String, serde_json::Value>,
                 key: &str,
                 value: Option<serde_json::Value>| {
        if let Some(value) = value {
            map.insert(key.to_string(), value);
        }
    };
    field(&mut object, "type", Some(serde_json::json!("session")));
    field(
        &mut object,
        "version",
        header.version.map(serde_json::Value::from),
    );
    field(&mut object, "id", Some(serde_json::json!(header.id)));
    field(
        &mut object,
        "timestamp",
        Some(serde_json::json!(header.timestamp)),
    );
    field(&mut object, "cwd", Some(serde_json::json!(header.cwd)));
    field(
        &mut object,
        "parentSession",
        header
            .parent_session
            .as_ref()
            .map(|parent| serde_json::json!(parent)),
    );
    field(
        &mut object,
        "rlmDepth",
        header.rlm_depth.map(serde_json::Value::from),
    );
    field(
        &mut object,
        "git",
        header
            .git
            .as_ref()
            .map(|git| serde_json::to_value(git).unwrap_or(serde_json::Value::Null)),
    );
    Some(serde_json::Value::Object(object).to_string())
}

fn select_model(
    registry: &mut uh_core::models::ModelRegistry,
    provider: Option<&str>,
    model: Option<&str>,
    settings: &uh_core::settings::SettingsManager,
) -> Result<Model, String> {
    let available: Vec<Model> = registry.get_available().into_iter().cloned().collect();
    let Some(model_name) = model else {
        // No model selection (TS `findInitialModel`): the saved default
        // (the last picker choice) while its provider has credentials,
        // then the registry's featured default, then the first model with
        // credentials.
        let saved = settings
            .get_default_provider()
            .zip(settings.get_default_model())
            .and_then(|(provider, id)| {
                available
                    .iter()
                    .find(|model| model.provider == provider && model.id == id)
            });
        if let Some(saved) = saved {
            return Ok(saved.clone());
        }
        if let Some(default) = uh_core::models::find_preferred_default_model(&available) {
            return Ok(default.clone());
        }
        if let Some(first) = available.first() {
            return Ok(first.clone());
        }
        let all: Vec<Model> = registry.get_all().to_vec();
        return all.first().cloned().ok_or_else(|| {
            "No models available. Check your installation or add models to models.json.".to_string()
        });
    };
    let resolved = uh_core::models::resolve_cli_model(provider, model_name, &available);
    if let Some(error) = resolved.error {
        return Err(error);
    }
    resolved
        .model
        .ok_or_else(|| "No matching model found.".to_string())
}

/// Resolve the session thinking level with the sdk.ts `createAgentSession`
/// order: the CLI flag, then the settings default, then "medium" — always
/// clamped to what the model supports.
fn resolve_thinking_level(
    config: &crate::mode::RuntimeConfig,
    model: &Model,
) -> uh_agent::types::ThinkingLevel {
    use uh_types::ai::ModelThinkingLevel;
    let settings = uh_core::settings::SettingsManager::create(&config.cwd, &config.agent_dir);
    let requested = config
        .thinking
        .or_else(|| {
            settings
                .get_default_thinking_level()
                .map(uh_core::settings::ThinkingLevelSetting::model_level)
        })
        // TS `DEFAULT_THINKING_LEVEL`.
        .unwrap_or(ModelThinkingLevel::Medium);
    let clamped = uh_ai::models::clamp_thinking_level(model, requested);
    map_thinking_level(clamped)
}

/// The headless session-manager resolution, mirroring the flag order of
/// TS `createSessionManager` (noSession -> fork -> resume -> continue ->
/// create). `--no-session` never reaches here: the caller passes `None` to
/// the engine, which builds the in-memory manager itself. The opened
/// session's runtime lease returns alongside (a long-lived connection
/// holds it on the engine handle; the one-shot modes forget it for the
/// process lifetime).
fn build_session_manager_with_lease(
    options: &RunOptions,
) -> Result<
    (
        uh_core::session::manager::SessionManager,
        Option<uh_daemon::lease::SessionLease>,
    ),
    String,
> {
    use uh_core::session::manager::SessionManager;
    let cwd = options.config.cwd.clone();
    let session_dir = options
        .session
        .session_dir
        .clone()
        .unwrap_or_else(|| options.config.agent_dir.join("sessions"));
    // TS `createSessionManager`'s fork arm: every resolution shape forks —
    // a GLOBAL session is exactly what --fork is for (a different
    // project's session copied into this cwd) — with no daemon-active
    // guard: the copy writes a fresh file, never the hosted source.
    if let Some(selector) = &options.session.fork {
        // A leading `~` expands against the home dir (the resume
        // selector's convention; the interactive fork arm matches).
        let expanded = crate::config::expand_tilde_path(selector);
        let selector = expanded.to_string_lossy();
        let resolved = resolve_session_path(&selector, &cwd, &session_dir)
            .map_err(|error| render_selector_error(&error))?;
        let source = match resolved {
            ResolvedSession::Path(path)
            | ResolvedSession::Local(path)
            | ResolvedSession::Global { path, .. } => path,
        };
        let manager = SessionManager::fork_from(&source, &cwd, &session_dir)?;
        // The materialized fork leases its own file before the engine
        // writes it (the fresh-session rule): another process resuming
        // the new file can never become a second writer while this
        // engine appends — the source was only read, never leased.
        return Ok(lease_fresh_manager(manager));
    }
    // main.ts `explicitCwdOverride`: with --cwd, the flag's directory wins
    // over the stored session cwd on resume.
    let explicit_cwd_override = options.session.cwd_from_flag.then_some(cwd.as_path());
    if let Some(selector) = &options.session.resume {
        let resolved = resolve_session_path(selector, &cwd, &session_dir)
            .map_err(|error| render_selector_error(&error))?;
        return match resolved {
            ResolvedSession::Path(path) | ResolvedSession::Local(path) => {
                let lease = session_open_guard(&path)?;
                // A failed open's early return drops the lease (released),
                // never leaving an orphaned hold behind.
                let manager = open_session_file(&path, &session_dir, &cwd, explicit_cwd_override)?;
                Ok((manager, Some(lease)))
            }
            ResolvedSession::Global {
                path: _,
                cwd: session_cwd,
            } => {
                // Print mode has no fork prompt; mirror the TS non-TTY path.
                Err(format!(
                    "session {selector} belongs to a different project ({}). Pass --fork {selector} to use it here, or run from that project's directory.",
                    session_cwd.display()
                ))
            }
        };
    }
    if options.session.continue_recent {
        let most_recent = find_most_recent_session_for_cwd(&session_dir, &cwd);
        return match most_recent {
            Some(path) => {
                let lease = session_open_guard(&path)?;
                let manager = open_session_file(&path, &session_dir, &cwd, explicit_cwd_override)?;
                Ok((manager, Some(lease)))
            }
            None => Ok(fresh_session_with_lease(&cwd, &session_dir)),
        };
    }
    Ok(fresh_session_with_lease(&cwd, &session_dir))
}

/// Build a FRESH persisted manager and lease its eagerly selected file
/// before the engine can write it (the replacement `New` path's rule —
/// TS leases the freshly created session too, `acquireReplacementLease`):
/// another process can never claim the first lease while this one writes.
fn fresh_session_with_lease(
    cwd: &std::path::Path,
    session_dir: &std::path::Path,
) -> (
    uh_core::session::manager::SessionManager,
    Option<uh_daemon::lease::SessionLease>,
) {
    let manager = uh_core::session::manager::SessionManager::persisted(cwd, session_dir);
    lease_fresh_manager(manager)
}

/// Lease a freshly materialized session file before the engine can write
/// it (the fresh `create`/`continue` paths and the `--fork` copy): the
/// UNGATED runtime acquire the resume path and the daemon's replacement
/// `New` arm share — `acquire_session_lease` answers `Ok(None)` whenever
/// the env gate is unset, so it would leave production fresh sessions
/// unleased. A fresh file's lease cannot be contended (its uuid is new);
/// an acquire failure here is environmental (the lease directory), so
/// the session proceeds with a warning instead of failing startup.
fn lease_fresh_manager(
    manager: uh_core::session::manager::SessionManager,
) -> (
    uh_core::session::manager::SessionManager,
    Option<uh_daemon::lease::SessionLease>,
) {
    let lease = match manager.get_session_file() {
        Some(path) => {
            match uh_daemon::lease::acquire_runtime_session_lease(
                path,
                &crate::config::get_agent_dir(),
            ) {
                Ok(lease) => Some(lease),
                Err(error) => {
                    eprintln!("unharnessed: could not lease the fresh session file: {error:#}");
                    None
                }
            }
        }
        None => None,
    };
    (manager, lease)
}

/// Open a session file with the TS `SessionManager.open` cwd semantics: an
/// explicit `--cwd` override wins, else the header's cwd, falling back to the
/// process cwd for unreadable or new files. Resumed sessions keep the
/// missing-cwd guard from main.ts.
/// Guard an in-process open of a persisted session file: acquire the
/// runtime lease. Returns the HELD lease — the caller owns its lifetime
/// (the one-shot print paths forget it for the process lifetime; a
/// long-lived connection holds it per session and drops it with the
/// engine it guards).
fn session_open_guard(
    session_path: &std::path::Path,
) -> Result<uh_daemon::lease::SessionLease, String> {
    // The session store is shared between processes (and with the
    // TypeScript product): the runtime lease table is the one ownership
    // record the shared agent dir offers, so a live holder refuses the
    // in-process open - a print-mode run over a held file would be a
    // second writer on it.
    let agent_dir = crate::config::get_agent_dir();
    // Acquire, not observe: a probe leaves a window where a daemon worker
    // (or another CLI) acquires the file's runtime lease after the check
    // and before this in-process open - two writers on one file. The
    // acquire is atomic against the shared lease table: a live foreign
    // holder answers with the session-hold refusal, and the returned
    // lease is the caller's to hold (the one-shot print run IS the
    // writer and forgets it for the process lifetime, whose dead pid
    // the liveness probes treat as released).
    match uh_daemon::lease::acquire_runtime_session_lease(session_path, &agent_dir) {
        Ok(lease) => Ok(lease),
        Err(error) => {
            let Some(active) = error.downcast_ref::<uh_daemon::lease::SessionAlreadyActiveError>()
            else {
                // The lease table itself failed (io, permissions): never
                // silently proceed over an undeterminable ownership record.
                return Err(format!(
                    "could not verify the session file is not held: {error:#}"
                ));
            };
            Err(uh_daemon::hold_refusal::refusal_message(
                &uh_daemon::hold_refusal::HoldIdentity {
                    pid: active.holder_pid,
                    active_session_id: active.active_session_id.clone(),
                },
                Some(session_path),
            ))
        }
    }
}

fn open_session_file(
    path: &std::path::Path,
    session_dir: &std::path::Path,
    fallback_cwd: &std::path::Path,
    explicit_cwd_override: Option<&std::path::Path>,
) -> Result<uh_core::session::manager::SessionManager, String> {
    let session_cwd = explicit_cwd_override.map_or_else(
        || {
            let header = uh_core::session::manager::read_session_header(path);
            header.filter(|header| !header.cwd.is_empty()).map_or_else(
                || fallback_cwd.to_path_buf(),
                |header| std::path::PathBuf::from(&header.cwd),
            )
        },
        std::path::Path::to_path_buf,
    );
    let manager = uh_core::session::manager::SessionManager::open(&session_cwd, session_dir, path);
    // main.ts getMissingSessionCwdIssue: a session stored against a deleted
    // directory must not silently continue somewhere else.
    if !manager.get_cwd().exists() {
        let session_file = manager
            .get_session_file()
            .map(|path| format!("\nSession file: {}", path.display()))
            .unwrap_or_default();
        return Err(format!(
            "Stored session working directory does not exist: {}{session_file}\nCurrent working directory: {}",
            manager.get_cwd().display(),
            fallback_cwd.display()
        ));
    }
    Ok(manager)
}

/// Render a selector failure with the main.ts formatting: the error message
/// plus the browse hint.
pub(crate) fn render_selector_error(error: &SessionSelectorError) -> String {
    format!(
        "{}.{}\nOpen unharnessed and press left-arrow to browse sessions.",
        error.message(),
        error.suggestion().unwrap_or_default()
    )
}

/// Model tools for the print runtime: `ipython` only (the TS product exposes
/// only the REPL tool to the model; `bash` and `edit` live in the kernel).
/// The engine adds the kernel-backed `ipython` tool itself.
fn builtin_tools(_cwd: &std::path::Path) -> Vec<Arc<dyn uh_agent::types::AgentTool>> {
    Vec::new()
}

/// How long `-p` waits, once idle, for a pending wake (`--background-wait-ms`).
const DEFAULT_BACKGROUND_WAIT_MS: u64 = 10 * 60 * 1000;

/// The boundaries every settled print turn crosses, prompt or wake.
#[allow(clippy::too_many_arguments)]
async fn settle_turn(
    engine: &uh_core::session_engine::engine::SessionEngine,
    boundary: &mut crate::print_boundary::TurnBoundary,
    goal: &crate::print_goal::PrintGoalSurface,
    autonomous: &HeadlessAutonomous,
    model: &Model,
    api_key: Option<String>,
) -> Result<(), String> {
    // The settled-turn boundary (TS `agent_end`): the provider retry of a
    // failed turn, the overflow compact-and-retry arm, and the threshold
    // arm. The outcomes persist in the session entries the
    // terminal result reads.
    boundary
        .run_at_settled_turn(engine, model, api_key.clone())
        .await?;
    // The goal boundary's queue drain: the threshold-held continuation
    // (minted ahead of the boundary's compaction) and the budget-limit
    // steer (armed at the crossing turn's message end) run as this
    // invocation's follow-up turns, each crossing the same boundary
    // pair; a turn that still ends in a terminal error fails an active
    // goal once the arms could not save it (TS
    // `_finishGoalForTerminalAssistantMessage` at `agent_end`, after
    // `_checkCompaction`).
    let goal_owns_boundary = goal
        .drive_boundary(engine, boundary, model, api_key.clone())
        .await?;
    // The autonomous arm runs only when the goal does not own the
    // boundary (TS `_getContinuationMessages`: the goal arm takes
    // exclusive priority; autonomous is never consulted while a goal
    // is active).
    if !goal_owns_boundary {
        // The held threshold continuation drains as this invocation's
        // follow-up turn (TS's queued `followUp` admission); its own
        // natural end churns the in-run hook again. The stop surfaces
        // only through the headless exit contract (TS: no row, no
        // stream frame).
        autonomous
            .drive_boundary(engine, boundary, model, api_key)
            .await
            .map_err(|error| format!("{error:#}"))?;
    }
    Ok(())
}

/// Admit prompts, stream json events when requested, and decide the exit code
/// from the headless terminal result plus the autonomous gate contract.
/// Shared by the real and faux paths. The turn-boundary compaction checks
/// (the overflow compact-and-retry arm and the threshold arm) run through
/// [`crate::print_boundary::TurnBoundary`] at every prompt's quiescent
/// boundaries. The autonomous continuation loop rides the agent's
/// natural-turn-end hook (the TS in-run shape: continuations churn inside
/// the one prompt wait with no run boundary between them); a held
/// threshold continuation drains through the boundary pair, and a stop
/// surfaces only through the headless exit contract (TS: no row, no
/// stream frame).
///
/// Once the prompts settle, the run waits for the session's wakes (a
/// background `bash()` that outlived its cell, the goal's backoff, a quota
/// park, a goal's wait on a command) and runs each as a turn through the same
/// boundaries. Each wait is bounded by `--background-wait-ms` and by the
/// autonomous run's deadline, except a goal's wait on a command, which has its
/// own window; the commands still alive at exit die with the kernel. A blocked
/// goal ends the run: nobody here can answer it.
async fn run_prompts_and_emit(
    engine: &std::sync::Arc<uh_core::session_engine::engine::SessionEngine>,
    model: &Model,
    api_key: Option<String>,
    provider_target: &ProviderTargetSlot,
    options: &RunOptions,
) -> Result<i32, String> {
    let wake_limit = std::time::Duration::from_millis(
        options
            .config
            .background_wait_ms
            .unwrap_or(DEFAULT_BACKGROUND_WAIT_MS),
    );
    let json_mode = options.app_mode == AppMode::Json;
    let mut unsubscribe: Option<uh_agent::agent::Subscription> = None;
    if json_mode {
        if let Some(header) = session_header_json(engine).await {
            println!("{header}");
        }
        unsubscribe = Some(
            engine
                .session
                .agent()
                .subscribe(|event, _signal| {
                    Box::pin(async move {
                        if let Some(json) = agent_event_json(&event) {
                            println!("{json}");
                        }
                        Ok(())
                    })
                })
                .await,
        );
    }
    // The goal continuation surface (the #252 residue): the usage
    // accounting publishes `goal_update` frames, the in-loop hook runs an
    // active goal's continuations inside the same agent run (the TS
    // `getContinuationMessages` seam), and the driver drains the queued
    // turns (the budget-limit steer, the threshold-held continuation) as
    // this invocation's follow-up runs. Wired in every output mode — the
    // loop runs identically in text mode, only silently.
    let goal = std::sync::Arc::new(crate::print_goal::PrintGoalSurface::new(json_mode));
    goal.seed_publish_baseline(engine).await;
    let goal_accounting = goal.wire_accounting(engine, engine.session.agent()).await;
    // The autonomous run (the verifier/eval composition seam): the CLI
    // flags enable it, a no-flag session starts disabled and `/autonomous`
    // rewrites it live. Per-message accounting runs against the one shared
    // state, and the composed in-run continuation hook drives both the
    // CLI-flag run and the flipped session state (TS: the continuation rides
    // the agent loop's natural-turn-end hook, in-run).
    let autonomous = std::sync::Arc::new(match options.config.autonomous.as_ref() {
        Some(config) => HeadlessAutonomous::from_cli(config, &options.config.cwd),
        None => HeadlessAutonomous::disabled(&options.config.cwd),
    });
    let accounting = autonomous.wire_accounting(engine.session.agent()).await;
    // The composed natural-turn-end hook (TS `_getContinuationMessages`):
    // the goal arm first (exclusive priority), the autonomous arm on the
    // fall-through, the boundary gates shared (queued input, a requested
    // compaction, the threshold arm's held continuation).
    crate::print_autonomous::wire_continuation_hook(
        engine,
        engine.session.agent(),
        model,
        &goal,
        &autonomous,
    );
    let mut boundary = crate::print_boundary::TurnBoundary::new(json_mode).with_retry(
        crate::print_boundary::PrintRetry {
            cwd: options.config.cwd.clone(),
            agent_dir: options.config.agent_dir.clone(),
            provider_target: std::sync::Arc::clone(provider_target),
            failover: std::env::var_os("UNHARNESSED_FAUX_SCRIPT").is_none(),
            max_park: wake_limit,
        },
    );
    // The autonomous runtime state the session-command executor mutates —
    // the run's own shared state (the session always carries one, TS
    // `createAgentSession`), so `/autonomous` rewrites the state the hook,
    // the accounting, and the exit contract read.
    let autonomous_state = autonomous.state_handle();
    // A failed session command rejects the prompt wait (TS print-mode's
    // catch): the raw error prints to stderr and the run exits 1 without
    // the later prompts or the terminal selection.
    let mut command_failure: Option<String> = None;
    // The `@file` image attachments ride the initial prompt only (TS
    // `initialImages`); the later CLI messages stay text.
    'prompts: for (prompt, images) in options
        .initial_message
        .iter()
        .map(|prompt| (prompt, options.initial_images.clone()))
        .chain(options.messages.iter().map(|prompt| (prompt, Vec::new())))
    {
        // Session commands (TS `_normalizeSubmission`'s `sessionCommand`
        // arm) never reach the model loop: the pre-turn boundary stays
        // theirs to skip and the prompt's turn never exists.
        if let Some(command) = engine.session.classify_session_command(prompt) {
            let execution = crate::print_session_command::execute_prompt_session_command(
                engine,
                &goal,
                model,
                api_key.clone(),
                &autonomous_state,
                &command,
            )
            .await;
            if let Some(error) = execution.error {
                command_failure = Some(error);
                break 'prompts;
            }
            // A `/goal` start (or resume) scheduled its continuation as
            // queued session input: the prompt wait drains it inside the
            // same wait, as the queued turn with its action frames.
            if let Some(continuation) = execution.continuation_message {
                goal.run_session_command_continuation(
                    engine,
                    &mut boundary,
                    model,
                    api_key.clone(),
                    &continuation,
                )
                .await?;
            }
            // The same queue drain a settled turn gets: held continuations
            // and armed steers run as this prompt's follow-up turns.
            goal.drive_boundary(engine, &mut boundary, model, api_key.clone())
                .await?;
            continue;
        }
        // The pre-turn boundary (TS `_runPreTurnCompaction`, the full
        // `_checkCompaction` pass): a stale overflow error from a previous
        // run gets its recovery attempt, and a resumed context above the
        // reserve headroom compacts before the admitted prompt runs on the
        // compacted context.
        boundary
            .run_pre_turn(engine, model, api_key.clone())
            .await?;
        engine
            .session
            .prompt_with_images(
                prompt,
                images,
                uh_core::session_engine::PromptOptions::default(),
            )
            .await
            .map_err(|error| format!("{error:#}"))?;
        engine.session.agent().wait_for_idle().await;
        settle_turn(
            engine,
            &mut boundary,
            &goal,
            &autonomous,
            model,
            api_key.clone(),
        )
        .await?;
    }
    // The idle wakes, each run as one more turn until none is left or a
    // wait runs out (UNHARNESSED_PLAN.md Q7).
    if command_failure.is_none() {
        loop {
            // A finished goal ends the run: a completed one first stops the
            // commands it left running, so none of them wakes it again.
            if engine.stop_after_completed_goal().await
                || engine.goal_state().await.status == uh_types::goal::GoalStatus::Blocked
            {
                break;
            }
            let limit = autonomous
                .remaining_time()
                .await
                .map_or(wake_limit, |remaining| remaining.min(wake_limit));
            let Some(wake) = engine.next_wake(Some(limit)).await else {
                break;
            };
            boundary
                .run_pre_turn(engine, model, api_key.clone())
                .await?;
            match wake {
                uh_core::session_engine::wakes::Wake::BashDone(messages) => {
                    engine.session.prompt_injected_messages(&messages).await
                }
                uh_core::session_engine::wakes::Wake::Marker(text) => {
                    engine
                        .session
                        .prompt(text, uh_core::session_engine::PromptOptions::default())
                        .await
                }
                uh_core::session_engine::wakes::Wake::GoalContinuation => {
                    let Some(message) = engine.take_owed_goal_continuation().await else {
                        continue;
                    };
                    engine.session.prompt_injected_message(&message).await
                }
            }
            .map_err(|error| format!("{error:#}"))?;
            engine.session.agent().wait_for_idle().await;
            settle_turn(
                engine,
                &mut boundary,
                &goal,
                &autonomous,
                model,
                api_key.clone(),
            )
            .await?;
        }
    }
    goal_accounting.unsubscribe().await;
    accounting.unsubscribe().await;
    if let Some(subscription) = unsubscribe {
        subscription.unsubscribe().await;
    }
    // The rejected prompt wait (TS print-mode's catch): print the raw
    // command error to stderr and exit 1 — no later prompts ran and the
    // terminal selection is skipped.
    if let Some(error) = command_failure {
        eprintln!("{error}");
        return Ok(1);
    }
    let state = engine.session.agent().state().await;
    let messages: Vec<uh_types::session::AgentMessage> =
        state.messages.iter().filter_map(json_round_trip).collect();
    let result = uh_core::session_engine::headless::select_headless_terminal_result(&messages);
    // The TS print-mode exit contract (modes/print-mode.ts): json mode
    // never derives the exit code from the terminal selection — the event
    // stream carries everything, and only the autonomous gates (or a thrown
    // error) exit non-zero. Text mode prints the primary message (an error
    // primary to stderr with exit 1, a settled answer to stdout) and the
    // trailing compaction-outcome disclosures to stderr. A run with no
    // terminal message — e.g. an overflow turn dropped by the
    // compact-and-retry recovery whose outcome row is the only surface —
    // prints nothing and leaves the exit code to the outcome rows.
    let mut exit_code = 0;
    if !json_mode {
        if let Some(primary) = result.primary {
            if let Some(stderr) = primary.stderr_text(&mut exit_code) {
                eprintln!("{stderr}");
            }
            if exit_code == 0 {
                if let Some(text) = primary.stdout_text() {
                    println!("{text}");
                }
            }
        }
        for outcome in result.compaction_outcomes {
            eprintln!("{}", outcome.content);
            if outcome.outcome == "failed" {
                exit_code = 1;
            }
        }
    }
    // The TS print-mode autonomous contract applies to both output modes.
    let goal_blocked = engine.goal_state().await.status == uh_types::goal::GoalStatus::Blocked;
    if let Some(stderr) = autonomous.exit_stderr(goal_blocked).await {
        eprintln!("{stderr}");
        exit_code = 1;
    }
    Ok(exit_code)
}

/// The faux-script engine: identical session assembly, scripted provider.
/// The faux assembly over one session-manager selection (the RPC mode's
/// replacement builds share it under the same script).
async fn build_faux_engine_with(
    options: &RunOptions,
    script: &str,
    session_manager: Option<uh_core::session::manager::SessionManager>,
) -> Result<HeadlessEngine, String> {
    let config = &options.config;
    let script: serde_json::Value = serde_json::from_str(script)
        .map_err(|error| format!("invalid UNHARNESSED_FAUX_SCRIPT: {error}"))?;
    // Response entries: a plain string answers with fixed text;
    // `{"systemPrompt": true}` answers with the request's system prompt
    // (binary-level verification of session assembly; never used by the
    // product); any other object goes through the shared faux-script
    // parser the daemon worker seam uses — `{"text": ...}`,
    // `{"content": [...]}` blocks (thinking, text, tool calls), and the
    // scripted `stopReason`/`errorMessage`/`delayMs` fields the
    // overflow-recovery harnesses script provider error turns with.
    let response_steps: Vec<uh_ai::faux::FauxResponseStep> = script
        .get("responses")
        .and_then(serde_json::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .map(|entry| match entry {
                    serde_json::Value::String(text) => Ok(uh_ai::faux::FauxResponseStep::Message(
                        uh_ai::faux::faux_assistant_text_message(
                            text,
                            uh_ai::faux::FauxAssistantMessageOptions::default(),
                        ),
                    )),
                    serde_json::Value::Object(map)
                        if map.get("systemPrompt").and_then(serde_json::Value::as_bool)
                            == Some(true) =>
                    {
                        Ok(uh_ai::faux::FauxResponseStep::Factory(std::sync::Arc::new(
                            |context, _options, _call, _model| {
                                Ok(uh_ai::faux::faux_assistant_text_message(
                                    context.system_prompt.as_deref().unwrap_or_default(),
                                    uh_ai::faux::FauxAssistantMessageOptions::default(),
                                ))
                            },
                        )))
                    }
                    serde_json::Value::Object(_) => {
                        uh_ai::faux::script::parse_faux_script(&serde_json::json!({
                            "responses": [entry]
                        }))
                        .map(|parsed| {
                            let mut steps = parsed.responses.into_iter();
                            let first = steps
                                .next()
                                .expect("an object entry parses into one response step");
                            debug_assert!(steps.next().is_none());
                            first
                        })
                    }
                    _ => Ok(uh_ai::faux::FauxResponseStep::Message(
                        uh_ai::faux::faux_assistant_text_message(
                            "",
                            uh_ai::faux::FauxAssistantMessageOptions::default(),
                        ),
                    )),
                })
                .collect::<Result<Vec<_>, String>>()
        })
        .ok_or_else(|| "UNHARNESSED_FAUX_SCRIPT requires a responses array".to_string())??;
    // The same faux-script model contract as the daemon worker seam: a
    // `reasoning` model makes the harness script thinking-capable turns so
    // thinking-level resolution can be verified without the network.
    let reasoning = script
        .get("reasoning")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    // The script pins the context window (the harness contract):
    // threshold/overflow verifiers size it to the probe they run.
    let context_window = script
        .get("contextWindow")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(100_000);
    // The script paces the stream like a model writing (the shared
    // script contract): a client sees each delta arrive on its own.
    let tokens_per_second = script
        .get("tokensPerSecond")
        .and_then(serde_json::Value::as_f64)
        .filter(|rate| *rate > 0.0);
    // The script prices the model in the catalog's shape (dollars per
    // million tokens: `input`, `output`, `cacheRead`, `cacheWrite`), so
    // what a conversation costs can be verified without a paid provider.
    let cost = script
        .get("cost")
        .cloned()
        .and_then(|cost| serde_json::from_value::<uh_types::ai::ModelCost>(cost).ok());
    // The stable faux identity (`api: "faux"`, `provider: "faux"`) the
    // daemon's scripted engine registers under: verification fixtures can
    // declare faux-provider models in models.json, and the ACP pickers'
    // in-process discovery then resolves them like real auth-configured
    // models.
    let registration =
        uh_ai::faux::register_faux_provider(uh_ai::faux::RegisterFauxProviderOptions {
            api: Some("faux".to_string()),
            provider: Some("faux".to_string()),
            models: Some(vec![uh_ai::faux::FauxModelDefinition {
                id: "faux-1".to_string(),
                name: Some("Faux Model".to_string()),
                reasoning: Some(reasoning),
                input: Some(vec![uh_types::ai::ModelInput::Text]),
                cost,
                context_window: Some(context_window),
                max_tokens: Some(4_096),
            }]),
            tokens_per_second,
            ..Default::default()
        });
    registration.set_responses(response_steps);
    let model = registration.get_model();
    let agent_model = json_round_trip(&model).ok_or("model conversion failed")?;
    let provider_target: ProviderTargetSlot =
        std::sync::Arc::new(std::sync::RwLock::new(Some(ProviderTarget {
            api_key: None,
            model: model.clone(),
            service_tier: None,
            headers: None,
        })));
    let stream_fn = switchable_stream_fn(std::sync::Arc::clone(&provider_target));
    // The faux path shares the session-manager wiring (persist / --no-session
    // / --resume / --continue) with the real provider path so binary-level
    // tests can verify persistence without the network.
    let engine = uh_core::session_engine::engine::create_session(
        uh_core::session_engine::engine::SessionEngineConfig {
            steering_mode: None,
            follow_up_mode: None,
            cwd: config.cwd.clone(),
            agent_dir: config.agent_dir.clone(),
            mcp_manager: None,
            model: Some(agent_model),
            thinking_level: Some(resolve_thinking_level(config, &model)),
            stream_fn: Some(stream_fn),
            tools: builtin_tools(&config.cwd),
            custom_system_prompt: config.system_prompt.clone(),
            prompt_guidelines: config.append_system_prompt.clone(),
            generic_mcp_servers: vec![],
            session_manager,
            extra_host_handlers: None,
            additional_skill_paths: vec![],
            additional_prompt_paths: vec![],
            extra_builtin_skill_overrides: vec![],
            model_info: Some(model.clone()),
            // The faux engine is a Rust-only verification harness, not a
            // product surface: no background kernel boot in tests.
            prewarm_ipython_kernel: None,
            on_background_work_settled: None,
            queued_goal_context_purge: None,
            queued_steering_probe: None,
            image_model_router: None,
        },
    )
    .await
    .map_err(|error| format!("{error:#}"))?;
    Ok(HeadlessEngine {
        engine,
        model,
        api_key: None,
        provider_target,
    })
}

#[cfg(test)]
mod tests {
    // --- print-mode MCP wiring (TS `createAgentSessionServices` parity) ---

    /// The arm records the routed target it writes, so the settle
    /// restores the captured session target only while the slot still
    /// holds the route; a mid-run `/model` switch rewrote the slot with
    /// the new session target, and the settle must leave it (the
    /// regression this pins: the arm once skipped the `armed_to` write,
    /// so the settle's still-routed guard always passed and dragged the
    /// slot back to the pre-route session target).
    #[test]
    fn headless_image_router_settle_preserves_a_mid_run_model_switch() {
        fn fixture_model(id: &str) -> uh_types::ai::Model {
            uh_types::ai::Model {
                id: id.to_string(),
                name: id.to_string(),
                api: "anthropic-messages".to_string(),
                provider: "anthropic".to_string(),
                base_url: "https://x".to_string(),
                reasoning: true,
                thinking_level_map: None,
                input: vec![
                    uh_types::ai::ModelInput::Text,
                    uh_types::ai::ModelInput::Image,
                ],
                cost: uh_types::ai::ModelCost {
                    input: 1.0.into(),
                    output: 2.0.into(),
                    cache_read: 0.0.into(),
                    cache_write: 0.0.into(),
                },
                context_window: 200_000,
                max_tokens: 8192,
                featured: None,
                headers: None,
                compat: None,
            }
        }
        fn target(
            model: uh_types::ai::Model,
        ) -> uh_core::session_engine::provider_adapter::ProviderTarget {
            uh_core::session_engine::provider_adapter::ProviderTarget {
                api_key: None,
                headers: None,
                model,
                service_tier: None,
            }
        }
        let home = tempfile::TempDir::new().unwrap();
        let agent_dir = home.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let session_model = fixture_model("session-model");
        let provider_target =
            std::sync::Arc::new(std::sync::RwLock::new(Some(target(session_model.clone()))));
        let armed_target: std::sync::Arc<
            std::sync::Mutex<Option<uh_core::session_engine::provider_adapter::ProviderTarget>>,
        > = std::sync::Arc::new(std::sync::Mutex::new(None));
        let router = super::headless_image_model_router(
            &provider_target,
            std::sync::Arc::clone(&armed_target),
            home.path().to_path_buf(),
            agent_dir,
            session_model,
        );
        let expected_route = uh_core::models::ResolvedImageModel {
            model: fixture_model("image-model"),
            thinking_level: uh_types::ai::ModelThinkingLevel::High,
            service_tier: None,
        };
        // Arm: the slot now serves the routed image model.
        (router.swap_target)(Some(&expected_route));
        assert_eq!(
            provider_target.read().unwrap().as_ref().unwrap().model.id,
            "image-model"
        );
        // A mid-run `/model` switch rewrites the live slot with the new
        // session target while the route is still armed.
        let switched_to = target(fixture_model("switched-model"));
        *provider_target.write().unwrap() = Some(switched_to);
        // Settle: the switch wins; the settle must not drag the slot back
        // to the pre-route session target.
        (router.swap_target)(None);
        assert_eq!(
            provider_target.read().unwrap().as_ref().unwrap().model.id,
            "switched-model"
        );
        // The next episode captures the live slot at ITS first arm, so its
        // baseline is the post-switch session model: the plain arm ->
        // serve -> settle contract restores that baseline (the
        // capture-at-arm, restore-at-settle pair).
        (router.swap_target)(Some(&expected_route));
        (router.swap_target)(None);
        assert_eq!(
            provider_target.read().unwrap().as_ref().unwrap().model.id,
            "switched-model"
        );
    }

    /// The print session's MCP manager serves a settings-declared server
    /// through the `mcp.config` host request the kernel dispatches
    /// (`rlm/mcp.py` resolution), and resolves its settings LIVE: a
    /// settings rewrite reaches the next `refresh()` — the re-resolver
    /// the remote-catalog change subscription drives mid-session. The
    /// `mcp.config` handler itself keeps the registration-time
    /// integrations (the uh-core handler design, shared with the daemon
    /// worker), so the pre-refresh handler still answers the old roster —
    /// asserted here so the test states the real production behavior.
    #[tokio::test]
    async fn print_mode_mcp_manager_serves_settings_servers_and_resolves_live() {
        let home = tempfile::TempDir::new().unwrap();
        let cwd = home.path().to_path_buf();
        let agent_dir = home.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("settings.json"),
            serde_json::json!({
                "mcpServers": {
                    "fixture-echo": {
                        "type": "stdio",
                        "command": "python3",
                        "args": ["echo.py"]
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let built_manager = crate::mcp_login::cli_mcp_manager(&cwd, &agent_dir);
        assert_eq!(
            built_manager.get_enabled_persistent_generic_servers(),
            vec!["fixture-echo".to_string()]
        );
        let manager = std::sync::Arc::new(std::sync::Mutex::new(built_manager));
        // The kernel's config host request serves the declared server with
        // the declared stdio config (registration-time integrations).
        let mut handlers = uh_core::kernel::shared::HostRequestHandlers::default();
        uh_core::mcp::McpManager::register_host_handlers(&manager, &mut handlers);
        let config = handlers.get("mcp.config").unwrap().clone();
        let result = config(uh_core::kernel::shared::HostRequestPayload {
            data: serde_json::json!({ "server": "fixture-echo" }),
            cell_source_code: None,
        })
        .await
        .unwrap();
        assert_eq!(result["type"], "stdio");
        assert_eq!(result["command"], "python3");
        assert_eq!(result["args"], serde_json::json!(["echo.py"]));
        // A settings rewrite reaches the same manager on the next refresh:
        // the closures re-read settings per resolution.
        std::fs::write(
            agent_dir.join("settings.json"),
            serde_json::json!({
                "mcpServers": {
                    "second-echo": {
                        "type": "stdio",
                        "command": "node",
                        "args": ["echo.mjs"]
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        {
            let mut manager = manager.lock().unwrap();
            manager.refresh();
            assert_eq!(
                manager.get_enabled_persistent_generic_servers(),
                vec!["second-echo".to_string()]
            );
        }
        // The already-registered handler keeps its registration-time
        // integrations — the registration shape a live session dispatches.
        let result = config(uh_core::kernel::shared::HostRequestPayload {
            data: serde_json::json!({ "server": "fixture-echo" }),
            cell_source_code: None,
        })
        .await
        .unwrap();
        assert_eq!(
            result["command"], "python3",
            "the registered handler serves its registration-time integrations"
        );
        let missing = config(uh_core::kernel::shared::HostRequestPayload {
            data: serde_json::json!({ "server": "second-echo" }),
            cell_source_code: None,
        })
        .await
        .unwrap();
        assert!(
            missing.as_object().unwrap().is_empty(),
            "the pre-refresh handler does not know the new server"
        );
    }

    /// Local service-catalog sources (`mcpCatalogSources`) reach the print
    /// manager's catalog resolution (TS `getCatalogSources`): the declared
    /// file's entry surfaces as a local descriptor, and dropping the
    /// declaration withdraws it on the next resolve — the same live
    /// settings read as the user-server closure.
    #[test]
    fn print_mode_mcp_manager_resolves_declared_catalog_sources() {
        let home = tempfile::TempDir::new().unwrap();
        let agent_dir = home.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let catalog = home.path().join("local-catalog.json");
        std::fs::write(
            &catalog,
            serde_json::json!({
                "version": 1,
                "entries": [{
                    "server": "my-local", "service": "my-local", "label": "My Local",
                    "url": "https://my-local.example/mcp", "aliases": [],
                    "transport": { "type": "http", "url": "https://my-local.example/mcp" },
                    "auth": { "strategy": "oauth", "clientRegistration": "dynamic" },
                    "setup": { "status": "ready" },
                    "verification": { "status": "unverified" },
                    "legacyBuiltin": false,
                    "provenance": [{ "source": "user" }]
                }]
            })
            .to_string(),
        )
        .unwrap();
        let settings = |sources: &[&str]| {
            serde_json::json!({
                "mcpCatalogSources": sources
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>()
            })
            .to_string()
        };
        std::fs::write(
            agent_dir.join("settings.json"),
            settings(&[&catalog.display().to_string()]),
        )
        .unwrap();
        let mut manager = crate::mcp_login::cli_mcp_manager(home.path(), &agent_dir);
        let my_local = manager
            .service_descriptors()
            .iter()
            .find(|service| service.service_id == "my-local")
            .expect("declared source entry resolved");
        assert!(my_local.local_source);
        // Live: dropping the declaration withdraws the entry on refresh.
        std::fs::write(agent_dir.join("settings.json"), settings(&[])).unwrap();
        manager.refresh();
        assert!(
            !manager
                .service_descriptors()
                .iter()
                .any(|service| service.service_id == "my-local"),
            "the dropped source no longer resolves"
        );
    }
}
