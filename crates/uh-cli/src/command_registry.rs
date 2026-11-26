//! Command specs and help formatting, ported from `cli/command-registry.ts`.

use crate::config::APP_NAME;

/// A registered public command.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub path: &'static [&'static str],
    pub usage: &'static str,
    pub summary: &'static str,
    pub description: Option<&'static str>,
    pub options: &'static [&'static str],
}

impl CommandSpec {
    const fn new(
        path: &'static [&'static str],
        usage: &'static str,
        summary: &'static str,
    ) -> Self {
        CommandSpec {
            path,
            usage,
            summary,
            description: None,
            options: &[],
        }
    }

    const fn description(mut self, description: &'static str) -> Self {
        self.description = Some(description);
        self
    }

    const fn options(mut self, options: &'static [&'static str]) -> Self {
        self.options = options;
        self
    }
}

/// All public command specs, in registry order.
pub const COMMAND_SPECS: &[CommandSpec] = &[
    CommandSpec::new(&["help"], "help [command]", "Show command help"),
    CommandSpec::new(
        &["mcp"],
        "mcp <add|list|get|remove>",
        "Manage user MCP servers",
    ),
    CommandSpec::new(
        &["mcp", "add"],
        "mcp add <name> --url <url> [--bearer-token-env-var <env>|--oauth] [--force]",
        "Add an HTTP or stdio MCP server",
    )
    .description("For stdio, use: mcp add <name> [--cwd <dir>] [--env CHILD=SOURCE] -- <command> [args...]"),
    CommandSpec::new(&["mcp", "list"], "mcp list", "List user MCP servers"),
    CommandSpec::new(&["mcp", "get"], "mcp get <name>", "Show a user MCP server"),
    CommandSpec::new(&["mcp", "remove"], "mcp remove <name>", "Remove a user MCP server"),
    CommandSpec::new(&["model"], "model list [search]", "Inspect available models"),
    CommandSpec::new(&["model", "list"], "model list [search]", "List available models"),
    CommandSpec::new(
        &["session"],
        "session export <file> [output]",
        "Manage saved sessions",
    ),
    CommandSpec::new(
        &["session", "export"],
        "session export <file> [output]",
        "Export a saved session to HTML",
    ),
    CommandSpec::new(
        &["prompt"],
        "prompt [--model <selector>] [--cwd <dir>] [--json]",
        "Print the assembled system prompt with its layer breakdown",
    )
    .description(
        "Assembles the effective system prompt the way a fresh root session does \
and prints the per-layer breakdown (cached static layers, then the dynamic tail) followed by the full prompt text.",
    )
    .options(&[
        "--model <selector>  Assemble for this provider/id model, with its instructions",
        "--cwd <dir>         Assemble for this working directory (default: current)",
        "--json              Print segments and prompt as JSON",
    ]),
    CommandSpec::new(&["update"], "update", "Update to the latest release"),
];

/// Command names that once existed and now print removal guidance.
pub const REMOVED_COMMAND_NAMES: &[&str] =
    &["app", "daemon", "install", "manage", "remove", "uninstall"];

/// The top-level public command names.
pub fn public_command_names() -> Vec<&'static str> {
    COMMAND_SPECS
        .iter()
        .filter(|spec| spec.path.len() == 1)
        .map(|spec| spec.path[0])
        .collect()
}

struct OptionGroup {
    heading: &'static str,
    options: &'static [(&'static str, &'static str)],
}

const TOP_LEVEL_OPTION_GROUPS: &[OptionGroup] = &[
    OptionGroup {
        heading: "Run options",
        options: &[
            ("-p, --print", "Print a response and exit"),
            (
                "--mode <text|json|acp>",
                "Select the output mode (default: text)",
            ),
            ("--cwd <dir>", "Use a specific working directory"),
            (
                "--background-wait-ms <n>",
                "Wait for background work before -p exits (default: 600000)",
            ),
            ("--offline", "Disable startup network operations"),
            ("--verbose", "Force verbose startup"),
        ],
    },
    OptionGroup {
        heading: "Model options",
        options: &[
            ("--provider <name>", "Select a model provider"),
            ("--model <id>", "Select a model"),
            ("--api-key <key>", "Use an API key for this run"),
            (
                "--models <patterns>",
                "Set comma-separated models for cycling",
            ),
            (
                "--thinking <level>",
                "Set reasoning: off, minimal, low, medium, high, xhigh, max",
            ),
        ],
    },
    OptionGroup {
        heading: "Session options",
        options: &[
            ("-c, --continue", "Continue the previous session"),
            (
                "-r, --resume [path|id]",
                "Open the agents view, or resume a saved session",
            ),
            (
                "--fork <path|id>",
                "Fork a saved session into a new session",
            ),
            ("--session-dir <dir>", "Use a custom session directory"),
            ("--no-session", "Do not save the session"),
            (
                "--goal <objective>",
                "Seed a persistent goal for a new root session",
            ),
            (
                "--goal-token-budget <n>",
                "Set a positive token budget for --goal",
            ),
        ],
    },
    OptionGroup {
        heading: "Tool and resource options",
        options: &[
            ("-t, --tools <list>", "Allowlist comma-separated tool names"),
            ("-nt, --no-tools", "Disable all tools by default"),
            (
                "-nbt, --no-builtin-tools",
                "Disable built-in tools by default",
            ),
            ("--skill <path>", "Load a skill (repeatable)"),
            ("-ns, --no-skills", "Disable skill discovery"),
            (
                "--prompt-template <path>",
                "Load a prompt template (repeatable)",
            ),
            (
                "-np, --no-prompt-templates",
                "Disable prompt template discovery",
            ),
            (
                "-nc, --no-context-files",
                "Disable AGENTS.md and CLAUDE.md discovery",
            ),
        ],
    },
    OptionGroup {
        heading: "Prompt options",
        options: &[
            (
                "--system-prompt <text>",
                "Replace the default system prompt",
            ),
            (
                "--append-system-prompt <text>",
                "Append to the system prompt (repeatable)",
            ),
            ("--", "Treat all following arguments as messages"),
        ],
    },
    OptionGroup {
        heading: "Autonomous options",
        options: &[
            (
                "--autonomous",
                "Continue until gates pass or a limit is reached",
            ),
            (
                "--autonomous-gate <command>",
                "Run a completion gate (repeatable)",
            ),
            (
                "--autonomous-gate-retries <n>",
                "Set positive retries per failed gate (default: 3)",
            ),
            (
                "--autonomous-gate-timeout-ms <n>",
                "Set positive per-gate timeout in ms (default: 300000)",
            ),
            (
                "--autonomous-max-continuations <n>",
                "Set positive follow-up limit (default: 3)",
            ),
            (
                "--autonomous-max-turns <n>",
                "Set positive assistant-turn limit (default: 12)",
            ),
            (
                "--autonomous-max-tokens <n>",
                "Set positive token limit (default: 80000)",
            ),
            (
                "--autonomous-timeout-ms <n>",
                "Set positive wall-clock limit in ms (default: 1800000)",
            ),
        ],
    },
    OptionGroup {
        heading: "Help",
        options: &[
            ("-v, --version", "Show version and exit"),
            ("-h, --help", "Show this help"),
        ],
    },
];

/// Look up a command spec by its exact path.
pub fn get_command_spec(path: &[&str]) -> Option<&'static CommandSpec> {
    COMMAND_SPECS.iter().find(|spec| {
        spec.path.len() == path.len() && spec.path.iter().zip(path).all(|(a, b)| a == b)
    })
}

/// The direct child command specs of a path.
pub fn get_child_command_specs(path: &[&str]) -> Vec<&'static CommandSpec> {
    COMMAND_SPECS
        .iter()
        .filter(|spec| {
            spec.path.len() == path.len() + 1
                && path
                    .iter()
                    .enumerate()
                    .all(|(index, segment)| spec.path[index] == *segment)
        })
        .collect()
}

/// True when a `help <path>` request resolves to help rather than a message.
pub fn is_help_command_request(path: &[&str]) -> bool {
    if path.is_empty() || get_command_spec(path).is_some() {
        return true;
    }
    if REMOVED_COMMAND_NAMES.contains(&path[0]) {
        return true;
    }
    if get_command_spec(&path[..1]).is_some() {
        return true;
    }
    let parent = &path[..path.len() - 1];
    let candidates: Vec<&str> = get_child_command_specs(parent)
        .into_iter()
        .map(|spec| spec.path[spec.path.len() - 1])
        .collect();
    find_command_suggestion(path[path.len() - 1], &candidates).is_some()
}

/// Suggest the closest candidate command name. The edit-distance heuristic
/// (`findSlashCommandSuggestion` in core/slash-commands.ts) is shared
/// vocabulary: `uh_types::slash_commands`.
pub fn find_command_suggestion<'a>(input: &str, candidates: &[&'a str]) -> Option<&'a str> {
    uh_types::slash_commands::find_slash_command_suggestion(input, candidates)
}

fn pad_end(value: &str, width: usize) -> String {
    let len = value.chars().count();
    if len >= width {
        value.to_string()
    } else {
        format!("{value}{}", " ".repeat(width - len))
    }
}

/// The full `--help` output, mirroring `formatTopLevelHelp`.
pub fn format_top_level_help() -> String {
    let commands: Vec<&CommandSpec> = COMMAND_SPECS
        .iter()
        .filter(|spec| spec.path.len() == 1)
        .collect();
    let command_width = commands
        .iter()
        .map(|spec| spec.path[0].len())
        .max()
        .unwrap_or(0);
    let options = TOP_LEVEL_OPTION_GROUPS
        .iter()
        .map(|group| {
            let width = group
                .options
                .iter()
                .map(|(option, _)| option.chars().count())
                .max()
                .unwrap_or(0);
            let lines: Vec<String> = group
                .options
                .iter()
                .map(|(option, summary)| format!("  {}  {}", pad_end(option, width), summary))
                .collect();
            format!("{}:\n{}", group.heading, lines.join("\n"))
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let command_lines: Vec<String> = commands
        .iter()
        .map(|spec| {
            format!(
                "  {}  {}",
                pad_end(spec.path[0], command_width),
                spec.summary
            )
        })
        .collect();
    format!(
        "{APP_NAME} - AI coding assistant with a Python REPL tool\n\
         \n\
         Usage:\n\
         \x20 {APP_NAME} [options] [@files...] [message...]\n\
         \x20 {APP_NAME} <command> [args...]\n\
         \n\
         Options:\n\
         {options}\n\
         \n\
         Commands:\n\
         {}\n\
         \n\
         Run \"{APP_NAME} help <command>\" for command details.",
        command_lines.join("\n")
    )
}

/// Per-command help output, mirroring `formatCommandHelp`.
pub fn format_command_help(path: &[&str]) -> Option<String> {
    let spec = get_command_spec(path)?;
    let children = get_child_command_specs(path);
    let mut sections = vec![
        format!("Usage:\n  {APP_NAME} {}", spec.usage),
        String::new(),
        format!("{}.", spec.summary),
    ];
    if let Some(description) = spec.description {
        sections.push(String::new());
        sections.push(description.to_string());
    }
    if !children.is_empty() {
        let width = children
            .iter()
            .map(|child| child.path[child.path.len() - 1].len())
            .max()
            .unwrap_or(0);
        sections.push(String::new());
        sections.push("Commands:".to_string());
        for child in children {
            sections.push(format!(
                "  {}  {}",
                pad_end(child.path[child.path.len() - 1], width),
                child.summary
            ));
        }
    }
    if !spec.options.is_empty() {
        sections.push(String::new());
        sections.push("Options:".to_string());
        for option in spec.options {
            sections.push(format!("  {option}"));
        }
    }
    Some(sections.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestion_thresholds() {
        assert_eq!(
            find_command_suggestion("schedul", &["schedule", "list"]),
            Some("schedule")
        );
        // Two-character tolerance would let "tmp" masquerade as "mcp", so
        // short tokens allow a single-character typo only.
        assert_eq!(find_command_suggestion("tmp", &["mcp"]), None);
        assert_eq!(find_command_suggestion("bogus", &["list"]), None);
    }

    #[test]
    fn help_command_request() {
        assert!(!is_help_command_request(&["bogus"]));
        assert!(is_help_command_request(&[]));
        assert!(is_help_command_request(&["daemon"]));
    }
}
