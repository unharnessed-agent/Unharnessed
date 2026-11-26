//! Layered system-prompt assembly: static layer files first (cacheable),
//! every session-specific value last. See the parent module docs.

use crate::platform::KernelShells;
use crate::settings::ModelInstructions;
use crate::skills::{format_skills_for_prompt, Skill};

use super::layers;

/// Whether a prompt segment belongs to the cache-stable prefix or the
/// session-specific tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    /// Cacheable static prefix content (layer files, or a user replacement).
    Static,
    /// Session-specific tail content; must never leak into [`SegmentKind::Static`].
    Dynamic,
}

/// One named slice of the assembled prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSegment {
    /// Stable segment name (`core`, `usage`, `packages`, `environment`, ...).
    pub name: &'static str,
    pub kind: SegmentKind,
    /// Provenance: the layer file, or what generated the segment.
    pub source: &'static str,
    pub text: String,
}

impl PromptSegment {
    fn static_segment(name: &'static str, source: &'static str, text: String) -> Self {
        Self {
            name,
            kind: SegmentKind::Static,
            source,
            text,
        }
    }

    fn dynamic_segment(name: &'static str, source: &'static str, text: String) -> Self {
        Self {
            name,
            kind: SegmentKind::Dynamic,
            source,
            text,
        }
    }
}

/// The assembled prompt plus its per-layer breakdown. `cached_prefix_len` is
/// the byte length of the static prefix: `assembled[..cached_prefix_len]` is
/// byte-identical across every session that shares the same custom-prompt
/// and model-selection inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPromptBreakdown {
    pub segments: Vec<PromptSegment>,
    pub assembled: String,
    pub cached_prefix_len: usize,
}

/// Inputs for one session's prompt. Static inputs (`custom_prompt`, `model`,
/// `model_instructions`) select the cached prefix; everything else feeds the
/// dynamic tail.
#[derive(Debug, Default)]
pub struct BuildSystemPromptOptions<'a> {
    /// Replaces the layered static prefix with user text. The dynamic tail
    /// still applies.
    pub custom_prompt: Option<String>,
    /// Resolved model selector (`provider/id`), selecting its instructions.
    pub model: Option<&'a str>,
    /// The `modelInstructions` groups: the texts of those listing `model`
    /// follow the static prefix.
    pub model_instructions: &'a [ModelInstructions],
    /// Whether the resolved model accepts image input, when known.
    pub vision_capable: Option<bool>,
    /// The shells the kernel's `bash()` is given, for the OS and shell
    /// lines; `None` leaves both out.
    pub shells: Option<KernelShells>,
    /// Active tools. Tool schemas carry tool descriptions outside the prompt.
    pub selected_tools: Option<Vec<&'a str>>,
    /// Additional guideline bullets appended to the dynamic tail.
    pub prompt_guidelines: Option<Vec<String>>,
    /// Working directory.
    pub cwd: String,
    /// Conversation log path.
    pub messages_path: Option<String>,
    /// Pre-loaded context files (path, content).
    pub context_files: Vec<(String, String)>,
    /// Pre-loaded skills.
    pub skills: Vec<Skill>,
    /// Enabled user-configured generic MCP servers.
    pub generic_mcp_servers: Vec<String>,
}

/// Build the system prompt (assembled text only).
#[must_use]
pub fn build_system_prompt(options: &BuildSystemPromptOptions) -> String {
    system_prompt_breakdown(options).assembled
}

/// Build the system prompt with its per-layer breakdown.
pub fn system_prompt_breakdown(options: &BuildSystemPromptOptions) -> SystemPromptBreakdown {
    let mut segments: Vec<PromptSegment> = Vec::new();

    // The static prefix: the user's replacement prompt, or the layered files.
    match options.custom_prompt.as_deref() {
        Some(custom) if !custom.is_empty() => {
            segments.push(PromptSegment::static_segment(
                "custom",
                "--system-prompt",
                custom.to_string(),
            ));
        }
        _ => {
            segments.push(PromptSegment::static_segment(
                "core",
                layers::layer_source("core").unwrap_or_default(),
                layers::CORE_LAYER.trim().to_string(),
            ));
        }
    }
    // Then the instructions the settings give the model, after either.
    let instructions = model_instructions_for(options.model_instructions, options.model);
    if !instructions.is_empty() {
        segments.push(PromptSegment::static_segment(
            "model-instructions",
            "settings modelInstructions",
            instructions.join("\n\n"),
        ));
    }
    let cached_prefix_len = segments
        .iter()
        .map(|segment| segment.text.len())
        .sum::<usize>()
        + 2 * segments.len().saturating_sub(1);

    // The dynamic tail, in fixed order: packages -> project context ->
    // skills inventory -> MCP servers -> environment -> session role ->
    // additional guidance.
    let tools: Vec<&str> = options
        .selected_tools
        .clone()
        .unwrap_or_else(|| vec!["ipython"]);
    let has_ipython = tools.contains(&"ipython");
    let has_file_access = has_ipython || tools.contains(&"bash");

    segments.push(PromptSegment::dynamic_segment(
        "packages",
        "kernel bootstrap defaults",
        packages_section(),
    ));

    let context = context_files_section(&options.context_files);
    if !context.is_empty() {
        segments.push(PromptSegment::dynamic_segment(
            "project-context",
            "AGENTS.md discovery",
            context,
        ));
    }

    let visible_skills: Vec<&Skill> = options
        .skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .collect();
    if has_file_access && !visible_skills.is_empty() {
        let inventory = format_skills_for_prompt(&options.skills).trim().to_string();
        segments.push(PromptSegment::dynamic_segment(
            "skills-inventory",
            "skill discovery",
            inventory,
        ));
    }

    if has_ipython {
        let mcp = format_generic_mcp_guidance(&options.generic_mcp_servers);
        if !mcp.is_empty() {
            segments.push(PromptSegment::dynamic_segment(
                "mcp-servers",
                "generic MCP settings",
                mcp,
            ));
        }
    }

    segments.push(PromptSegment::dynamic_segment(
        "environment",
        "session configuration",
        environment_section(options, has_ipython),
    ));

    let role = session_role_section(has_ipython);
    if !role.is_empty() {
        segments.push(PromptSegment::dynamic_segment(
            "session-role",
            "tool selection",
            role,
        ));
    }

    let guidelines = options
        .prompt_guidelines
        .as_deref()
        .map(format_prompt_guidelines)
        .unwrap_or_default();
    if !guidelines.is_empty() {
        segments.push(PromptSegment::dynamic_segment(
            "additional-guidance",
            "prompt guidelines",
            format!("# Additional Guidance\n\n{guidelines}"),
        ));
    }

    let assembled = segments
        .iter()
        .map(|segment| segment.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    SystemPromptBreakdown {
        segments,
        assembled,
        cached_prefix_len,
    }
}

/// The texts of the groups that list `model`, in order.
#[must_use]
pub fn model_instructions_for(groups: &[ModelInstructions], model: Option<&str>) -> Vec<String> {
    let Some(model) = model else {
        return Vec::new();
    };
    groups
        .iter()
        .filter(|group| group.models.iter().any(|listed| listed.trim() == model))
        .map(|group| group.text.trim())
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .collect()
}

fn packages_section() -> String {
    use crate::kernel::bootstrap::default_rlm_extra_import_labels;
    let mut lines = vec![format!(
        "Pre-installed Python packages: {}.",
        default_rlm_extra_import_labels().join(", ")
    )];
    lines.push(
        "Install additional packages with `uv pip install <pkg>` (this is a uv-managed venv with no pip module)."
            .to_string(),
    );
    lines.join("\n")
}

fn context_files_section(context_files: &[(String, String)]) -> String {
    if context_files.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "# Project Context".to_string(),
        String::new(),
        "Project-specific instructions and guidelines:".to_string(),
        String::new(),
    ];
    for (file_path, content) in context_files {
        lines.push(format!("## {file_path}\n\n{content}"));
    }
    lines.join("\n")
}

fn environment_section(options: &BuildSystemPromptOptions, has_ipython: bool) -> String {
    let cwd = options.cwd.replace('\\', "/");
    let messages_path = options
        .messages_path
        .clone()
        .unwrap_or_else(|| "not persisted".to_string())
        .replace('\\', "/");
    let mut lines = vec![
        format!("Current date: {}", today()),
        format!("Working directory: {cwd}"),
    ];
    if let Some(shells) = &options.shells {
        lines.push(format!("OS: {}", os_name()));
        if has_ipython {
            lines.push(format!("Shell: {}", shell_note(shells)));
        }
    }
    match options.vision_capable {
        Some(true) => lines.push(
            "Image input: this model can see images; `attach_image` loads them into context."
                .to_string(),
        ),
        Some(false) => lines.push(
            "Image input: this model cannot see images; `attach_image` errors for it.".to_string(),
        ),
        None => {}
    }
    // Last: providers cut the cached system block before this line.
    lines.push(format!(
        "{}{messages_path}",
        uh_types::ai::SESSION_TAIL_PREFIX
    ));
    lines.join("\n")
}

fn os_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macOS",
        "linux" => "Linux",
        "windows" => "Windows",
        other => other,
    }
}

/// What `bash()` runs: bash, or on Windows the PowerShell and Git Bash the
/// kernel is given.
fn shell_note(shells: &KernelShells) -> String {
    if !cfg!(windows) {
        return "`bash()` runs bash.".to_string();
    }
    let git_bash = shells.git_bash.is_some();
    let mut note = match shells.powershell.as_deref() {
        Some(path) if path.ends_with("pwsh.exe") => {
            "`bash()` runs PowerShell 7 (pwsh), so write PowerShell, not bash \
             (`Get-ChildItem`, `Select-String`, `$env:NAME`)."
        }
        Some(_) => {
            "`bash()` runs Windows PowerShell 5.1, so write PowerShell, not bash \
             (`Get-ChildItem`, `Select-String`, `$env:NAME`); it has no `&&` or `||`, chain with `;`."
        }
        None if git_bash => return "`bash()` runs Git Bash.".to_string(),
        None => return "`bash()` finds neither PowerShell nor Git Bash, so it fails.".to_string(),
    }
    .to_string();
    if git_bash {
        note.push_str(
            " `bash(command, shell=\"bash\")` runs Git Bash instead, for a project's `.sh` scripts.",
        );
    }
    note
}

fn session_role_section(has_ipython: bool) -> String {
    if has_ipython {
        return String::new();
    }
    "This session has no Python REPL (`ipython` tool): the programmatic tools described above are unavailable here."
        .to_string()
}

fn today() -> String {
    // UTC date in YYYY-MM-DD form; the prompt is date context only.
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() / 86_400);
    // Civil-from-days algorithm (Howard Hinnant).
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn format_prompt_guidelines(guidelines: &[String]) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut list = Vec::new();
    for guideline in guidelines {
        let normalized = guideline.trim();
        if !normalized.is_empty() && seen.insert(normalized.to_string()) {
            list.push(format!("- {normalized}"));
        }
    }
    list.join("\n")
}

fn format_generic_mcp_guidance(servers: &[String]) -> String {
    let mut enabled: Vec<&String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for server in servers {
        if seen.insert(server) {
            enabled.push(server);
        }
    }
    enabled.sort();
    if enabled.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "# Generic MCP Connections".to_string(),
        String::new(),
        "Generic MCP connections are accessed through the pre-imported Python `mcp` object in the Python REPL, not as top-level native tool namespaces or installed Python skills.".to_string(),
        format!(
            "Enabled generic MCP servers: {}.",
            enabled
                .iter()
                .map(|server| format!("`{server}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ];
    for server in &enabled {
        lines.push(format!(
            "For `{server}`, first discover its tools with `await mcp.list_tools(\"{server}\")`, then call one with `await mcp.call_tool(\"{server}\", \"<tool>\", arguments)`."
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{create_synthetic_source_info, SkillKind, SourceScope};
    use std::path::PathBuf;

    fn skill(name: &str, import: Option<&str>) -> Skill {
        Skill {
            name: name.to_string(),
            description: format!("Skill {name}"),
            file_path: PathBuf::from("/skills/SKILL.md"),
            base_dir: PathBuf::from("/skills"),
            source_info: create_synthetic_source_info("/skills", "user", SourceScope::User, None),
            disable_model_invocation: false,
            kind: if import.is_some() {
                SkillKind::Python
            } else {
                SkillKind::Markdown
            },
            python: import.map(|import_name| crate::skills::SkillPythonMetadata {
                import_name: import_name.to_string(),
                package_path: PathBuf::from("/skills"),
                pyproject_path: PathBuf::from("/skills/pyproject.toml"),
            }),
        }
    }

    fn base_options() -> BuildSystemPromptOptions<'static> {
        BuildSystemPromptOptions {
            cwd: "/w".to_string(),
            messages_path: Some("/log.jsonl".to_string()),
            model: Some("mock/mock-1"),
            skills: vec![
                skill("web-search", Some("websearch")),
                skill("agent-message", Some("agent_message")),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn default_prompt_is_layered_with_static_prefix_first() {
        let breakdown = system_prompt_breakdown(&base_options());
        let prompt = &breakdown.assembled;
        // Static layers lead, in order.
        assert!(prompt.starts_with("# Unharnessed"));
        assert_eq!(breakdown.segments[0].kind, SegmentKind::Static);
        assert_eq!(breakdown.segments[0].name, "core");
        let names: Vec<&str> = breakdown
            .segments
            .iter()
            .map(|segment| segment.name)
            .collect();
        assert_eq!(
            names[..2],
            ["core", "packages"],
            "the core layer, then the dynamic tail"
        );
        // The cached prefix is exactly the static segments.
        assert_eq!(
            &prompt[..breakdown.cached_prefix_len],
            layers::static_prefix()
        );
        // Dynamic values live strictly after the prefix.
        let tail = &prompt[breakdown.cached_prefix_len..];
        assert!(tail.contains("Working directory: /w"));
        assert!(tail.contains("Conversation log: /log.jsonl"));
        assert!(tail.contains("<available_skills>"));
        assert!(tail.contains("Pre-installed Python packages: requests, httpx,"));
    }

    #[test]
    fn dynamic_tail_isolates_the_cached_prefix() {
        let mut other = base_options();
        other.cwd = "/somewhere/else".to_string();
        other.messages_path = None;
        other.generic_mcp_servers = vec!["slack".to_string()];
        other.context_files = vec![("AGENTS.md".to_string(), "Rule one.".to_string())];
        let left = system_prompt_breakdown(&base_options());
        let right = system_prompt_breakdown(&other);
        // Different sessions share one byte-identical cacheable prefix.
        assert_eq!(
            left.assembled[..left.cached_prefix_len],
            right.assembled[..right.cached_prefix_len]
        );
        assert_eq!(left.cached_prefix_len, right.cached_prefix_len);
        // And the tails differ in the session-specific values.
        assert!(right.assembled[right.cached_prefix_len..]
            .contains("Enabled generic MCP servers: `slack`"));
    }

    #[test]
    fn custom_prompt_replaces_layers_keeps_tail() {
        let mut options = base_options();
        options.custom_prompt = Some("Be terse.".to_string());
        options.context_files = vec![("AGENTS.md".to_string(), "Rule one.".to_string())];
        let breakdown = system_prompt_breakdown(&options);
        let prompt = &breakdown.assembled;
        assert!(prompt.starts_with("Be terse."));
        assert_eq!(breakdown.segments[0].name, "custom");
        assert!(prompt.contains("## AGENTS.md\n\nRule one."));
        assert!(prompt.contains("Working directory: /w"));
        assert!(prompt.contains("Current date: "));
        // The layered defaults are gone.
        assert!(!prompt.contains("# Unharnessed"));
    }

    #[test]
    fn mcp_and_guidelines_are_tail_segments() {
        let mut options = base_options();
        options.generic_mcp_servers = vec!["t".to_string(), "t".to_string()];
        options.prompt_guidelines = Some(vec!["be careful".to_string(), "be careful".to_string()]);
        let breakdown = system_prompt_breakdown(&options);
        let names: Vec<&str> = breakdown
            .segments
            .iter()
            .map(|segment| segment.name)
            .collect();
        assert!(names.contains(&"mcp-servers"));
        assert!(names.contains(&"additional-guidance"));
        assert!(breakdown.assembled.contains("# Generic MCP Connections"));
        assert!(breakdown
            .assembled
            .contains("Enabled generic MCP servers: `t`."));
        assert!(breakdown
            .assembled
            .contains("# Additional Guidance\n\n- be careful"));
    }

    #[test]
    fn static_segments_sit_in_the_cached_prefix() {
        // The composition contract: every static segment (the core layer,
        // and the model's instructions when it has some) lands inside the
        // cached prefix.
        let breakdown = system_prompt_breakdown(&base_options());
        let static_segments = breakdown
            .segments
            .iter()
            .filter(|segment| segment.kind == SegmentKind::Static);
        for segment in static_segments {
            assert_eq!(
                breakdown
                    .assembled
                    .find(&segment.text)
                    .map(|at| at < breakdown.cached_prefix_len),
                Some(true),
                "static segment {} sits inside the cached prefix",
                segment.name
            );
        }
    }
}
