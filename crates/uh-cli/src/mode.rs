//! The runtime boundary: typed execution modes and options, mirroring the
//! `resolveAppMode` / `runtimeConfigFromArgs` split in `main.ts`. Crates that
//! provide the real runtime (uh-core session engine, uh-daemon workers,
//! pa-tui) plug in behind [`Runtime::run`] at merge time.

use std::path::PathBuf;

use crate::args::{Args, AutonomousConfig, Mode};

/// The process-level execution mode, mirroring `AppMode` in main.ts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    Interactive,
    Print,
    Json,
    Acp,
}

impl AppMode {
    /// Resolve the execution mode from parsed args and stdin TTY state,
    /// mirroring `resolveAppMode`.
    #[must_use]
    pub fn resolve(parsed: &Args, stdin_is_tty: bool) -> AppMode {
        match parsed.mode {
            Some(Mode::Acp) => AppMode::Acp,
            Some(Mode::Json) => AppMode::Json,
            _ => {
                if parsed.print || !stdin_is_tty {
                    AppMode::Print
                } else {
                    AppMode::Interactive
                }
            }
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            AppMode::Interactive => "interactive",
            AppMode::Print => "print",
            AppMode::Json => "json",
            AppMode::Acp => "acp",
        }
    }

    /// The print output mode, mirroring `toPrintOutputMode`.
    #[must_use]
    pub fn print_output_mode(&self) -> Mode {
        match self {
            AppMode::Json => Mode::Json,
            _ => Mode::Text,
        }
    }
}

/// A seeded persistent goal (`initialGoal` in the runtime config).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitialGoal {
    pub objective: String,
    pub token_budget: Option<u32>,
}

/// The typed per-session runtime configuration, mirroring
/// `AgentSessionRuntimeConfig`. This is the API boundary the uh-core/uh-ai
/// crates consume at merge time.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)] // the mirrored `AgentSessionRuntimeConfig` API shape is deliberate
pub struct RuntimeConfig {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub session_dir: Option<PathBuf>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Vec<String>,
    pub thinking: Option<uh_types::ai::ModelThinkingLevel>,
    pub models: Option<Vec<String>>,
    pub tools: Option<Vec<String>>,
    pub no_tools: bool,
    pub no_builtin_tools: bool,
    pub skills: Vec<PathBuf>,
    pub no_skills: bool,
    pub prompt_templates: Vec<PathBuf>,
    pub no_prompt_templates: bool,
    pub no_context_files: bool,
    pub autonomous: Option<AutonomousConfig>,
    pub initial_goal: Option<InitialGoal>,
    /// `--background-wait-ms` (`None`: the 10-minute default).
    pub background_wait_ms: Option<u64>,
}

/// Session selection options that stay client-side.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)] // the selection's flag set is the deliberate client-side surface
pub struct SessionOptions {
    /// `--continue`/`-c`: the launch surfaces the newest saved session for
    /// the cwd through the agents view (preselected, never a blind reopen)
    /// and falls back to a fresh session without a candidate.
    pub continue_recent: bool,
    pub resume_bare: bool,
    pub resume: Option<String>,
    pub fork: Option<String>,
    pub no_session: bool,
    pub session_dir: Option<PathBuf>,
    /// True when `--cwd` selected the working directory; resumed sessions
    /// then use that directory instead of the header cwd (main.ts
    /// `explicitCwdOverride`).
    pub cwd_from_flag: bool,
}

/// Everything the CLI hands to the runtime, mirroring what `main.ts` computes
/// before entering the mode runners.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// The arguments as parsed, after the public-command rewrites: the
    /// interactive terminal hands them on to its agent process.
    pub argv: Vec<String>,
    pub app_mode: AppMode,
    pub config: RuntimeConfig,
    pub session: SessionOptions,
    pub messages: Vec<String>,
    pub file_args: Vec<String>,
    pub list_models: Option<Option<String>>,
    /// The combined first prompt (stdin + @file text + first message).
    pub initial_message: Option<String>,
    /// The `@file` image attachments for the initial prompt (TS
    /// `initialImages`; only the non-interactive prompt path sends them -
    /// the interactive initial-message image arm is not yet wired).
    pub initial_images: Vec<uh_agent::types::ImageContent>,
    pub verbose: bool,
    pub offline: bool,
}

#[cfg(test)]
impl RunOptions {
    /// Options with only `argv` set (the interactive terminal's tests).
    pub(crate) fn for_terminal_tests(argv: Vec<String>) -> RunOptions {
        RunOptions {
            argv,
            app_mode: AppMode::Interactive,
            config: RuntimeConfig::default(),
            session: SessionOptions::default(),
            messages: Vec::new(),
            file_args: Vec::new(),
            list_models: None,
            initial_message: None,
            initial_images: Vec::new(),
            verbose: false,
            offline: false,
        }
    }
}

/// A missing runtime subsystem, reported as a typed error instead of faked
/// output. The enum makes the merge-time work explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingSubsystem {
    SessionEngine,
    ModelRegistry,
}

impl MissingSubsystem {
    #[must_use]
    pub fn subsystem_name(&self) -> &'static str {
        match self {
            MissingSubsystem::SessionEngine => "the session engine (uh-core)",
            MissingSubsystem::ModelRegistry => "the model registry (uh-ai)",
        }
    }

    #[must_use]
    pub fn error_message(&self) -> String {
        format!(
            "this invocation needs {}, which is not linked into the binary yet",
            self.subsystem_name()
        )
    }
}

impl std::fmt::Display for MissingSubsystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.subsystem_name())
    }
}

impl std::error::Error for MissingSubsystem {}

/// The runtime boundary. `run` executes the requested mode; the current build
/// has only the [`UnavailableRuntime`] implementation, which produces typed
/// [`MissingSubsystem`] errors for every mode that needs unmerged crates.
pub trait Runtime {
    /// Execute the requested mode.
    ///
    /// # Errors
    /// Returns a [`MissingSubsystem`] error naming the subsystem the
    /// requested mode needs when it is not linked into the binary.
    fn run(&self, options: &RunOptions) -> Result<i32, MissingSubsystem>;
}

/// The merge-time placeholder runtime: it never fakes output, it fails loudly
/// with the precise missing subsystem for the requested mode.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableRuntime;

impl Runtime for UnavailableRuntime {
    fn run(&self, options: &RunOptions) -> Result<i32, MissingSubsystem> {
        if options.list_models.is_some() {
            return Err(MissingSubsystem::ModelRegistry);
        }
        let _ = options.app_mode;
        Err(MissingSubsystem::SessionEngine)
    }
}

/// Build the runtime config from parsed args, mirroring `runtimeConfigFromArgs`.
pub fn runtime_config_from_args(
    parsed: &Args,
    cwd: PathBuf,
    agent_dir: PathBuf,
    session_dir: Option<PathBuf>,
) -> RuntimeConfig {
    // isLocalPath: only npm:/git:/github:/http(s):/ssh: sources are not local;
    // everything else resolves against the session cwd (utils/paths.ts).
    let is_local_path = |value: &str| {
        let trimmed = value.trim();
        !(trimmed.starts_with("npm:")
            || trimmed.starts_with("git:")
            || trimmed.starts_with("github:")
            || trimmed.starts_with("http:")
            || trimmed.starts_with("https:")
            || trimmed.starts_with("ssh:"))
    };
    let resolve_cli_path = |paths: &[String]| -> Vec<PathBuf> {
        paths
            .iter()
            .map(|value| {
                if is_local_path(value) {
                    if std::path::Path::new(value).is_absolute() {
                        PathBuf::from(value)
                    } else {
                        cwd.join(value)
                    }
                } else {
                    PathBuf::from(value)
                }
            })
            .collect()
    };
    let skills = resolve_cli_path(&parsed.skills);
    let prompt_templates = resolve_cli_path(&parsed.prompt_templates);
    RuntimeConfig {
        cwd,
        agent_dir,
        session_dir,
        provider: parsed.provider.clone(),
        model: parsed.model.clone(),
        api_key: parsed.api_key.clone(),
        system_prompt: parsed.system_prompt.clone(),
        append_system_prompt: parsed.append_system_prompt.clone(),
        thinking: parsed.thinking,
        models: parsed.models.clone(),
        tools: parsed.tools.clone(),
        no_tools: parsed.no_tools,
        no_builtin_tools: parsed.no_builtin_tools,
        skills,
        no_skills: parsed.no_skills,
        prompt_templates,
        no_prompt_templates: parsed.no_prompt_templates,
        no_context_files: parsed.no_context_files,
        autonomous: AutonomousConfig::from_args(parsed),
        initial_goal: parsed.goal.as_ref().map(|objective| InitialGoal {
            objective: objective.clone(),
            token_budget: parsed.goal_token_budget,
        }),
        background_wait_ms: parsed.background_wait_ms,
    }
}
