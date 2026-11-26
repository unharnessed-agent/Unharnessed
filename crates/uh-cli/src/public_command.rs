//! Public command routing, ported from `cli/public-command.ts`.

use std::collections::HashSet;

use crate::args::{parse_args, INTERNAL_RUNTIME_COMMAND_MARKER};

use crate::command_registry::{
    find_command_suggestion, format_command_help, format_top_level_help, get_child_command_specs,
    get_command_spec, is_help_command_request, public_command_names, REMOVED_COMMAND_NAMES,
};
use crate::config::APP_NAME;
use crate::global_flags::{extract_help_command_path, rotate_global_flags_before_command};
use crate::mcp_command::run_mcp_management_command;

/// The outcome of routing the argv through the public command layer.
#[derive(Debug, Clone)]
pub struct PublicCommandResult {
    pub handled: bool,
    pub args: Vec<String>,
    /// The process exit code to use once handled.
    pub exit_code: Option<i32>,
}

const HANDLED: fn() -> PublicCommandResult = || PublicCommandResult {
    handled: true,
    args: vec![],
    exit_code: None,
};

fn continue_with(args: Vec<String>) -> PublicCommandResult {
    PublicCommandResult {
        handled: false,
        args,
        exit_code: None,
    }
}

/// The error message used when a routed command needs a runtime subsystem that
/// is not linked into this build yet.
fn fail(message: impl AsRef<str>, hint: Option<String>) -> PublicCommandResult {
    eprintln!("Error: {}", message.as_ref());
    if let Some(hint) = hint {
        eprintln!("{hint}");
    }
    PublicCommandResult {
        handled: true,
        args: vec![],
        exit_code: Some(1),
    }
}

fn handled() -> PublicCommandResult {
    HANDLED()
}

/// A handled invocation whose driver already printed everything, with its own
/// process exit code (shutdown failures exit 1).
fn handled_with_exit(exit_code: i32) -> PublicCommandResult {
    PublicCommandResult {
        handled: true,
        args: vec![],
        exit_code: Some(exit_code),
    }
}

/// Route the argv through the public command layer, mirroring
/// `handlePublicCommand`. All errors are printed directly; the result reports
/// whether the invocation was fully handled and with which exit code.
pub fn handle_public_command(args: &[String]) -> PublicCommandResult {
    let public: HashSet<&str> = public_command_names().into_iter().collect();
    let removed: HashSet<&str> = REMOVED_COMMAND_NAMES.iter().copied().collect();
    let args = rotate_global_flags_before_command(args, &public, &removed);

    if args.first().map(String::as_str) == Some("help") {
        if let Some(help_path) = extract_help_command_path(&args, 1) {
            let help_path_ref: Vec<&str> = help_path.iter().map(String::as_str).collect();
            if is_help_command_request(&help_path_ref) {
                return print_requested_help(&help_path);
            }
        }
    }

    let Some(command) = args.first() else {
        return continue_with(args);
    };
    let command = command.as_str();

    if removed.contains(command) {
        return reject_removed_command(&args);
    }
    if !public.contains(command) {
        return continue_with(args.clone());
    }

    let separator_index = args.iter().position(|a| a == "--");
    let help_index = args.iter().enumerate().position(|(index, arg)| {
        index > 0
            && (separator_index.is_none() || index < separator_index.unwrap())
            && (arg == "--help" || arg == "-h")
    });
    if let Some(help_index) = help_index {
        return print_requested_help(&get_command_path(&args[..help_index]));
    }

    let rest: Vec<String> = args[1..].to_vec();
    match command {
        "mcp" => run_mcp(&rest),
        "model" => rewrite_nested_command("model", "list", "--list-models", &rest),
        "session" => rewrite_nested_command("session", "export", "--export", &rest),
        "prompt" => handled_with_exit(crate::prompt_command::run_prompt_command(&rest)),
        "update" => handled_with_exit(crate::update::run(&rest)),
        _ => continue_with(args.clone()),
    }
}

fn print_requested_help(path: &[String]) -> PublicCommandResult {
    if path.is_empty() {
        println!("{}", format_top_level_help());
        return handled();
    }
    if REMOVED_COMMAND_NAMES.contains(&path[0].as_str()) {
        return reject_removed_command(path);
    }
    let path_ref: Vec<&str> = path.iter().map(String::as_str).collect();
    if let Some(help) = format_command_help(&path_ref) {
        println!("{help}");
        return handled();
    }
    let parent: Vec<&str> = path[..path.len() - 1].iter().map(String::as_str).collect();
    let candidates: Vec<&str> = get_child_command_specs(&parent)
        .into_iter()
        .map(|spec| spec.path[spec.path.len() - 1])
        .collect();
    let suggestion = find_command_suggestion(&path[path.len() - 1], &candidates);
    let mut message = format!("Unknown command: {}", path.join(" "));
    let hint = suggestion.map(|suggestion| {
        let mut full = parent.clone();
        full.push(suggestion);
        format!("Did you mean \"{APP_NAME} help {}\"?", full.join(" "))
    });
    if suggestion.is_none() {
        // The exact TS message includes no hint when there is no suggestion.
        message = format!("Unknown command: {}", path.join(" "));
    }
    fail(message, hint)
}

fn get_command_path(args: &[String]) -> Vec<String> {
    let mut path: Vec<String> = Vec::new();
    for arg in args {
        let mut candidate: Vec<&str> = path.iter().map(String::as_str).collect();
        candidate.push(arg);
        if get_command_spec(&candidate).is_none() {
            break;
        }
        path.push(arg.clone());
    }
    path
}

fn reject_removed_command(args: &[String]) -> PublicCommandResult {
    let command = args.first().map(String::as_str).unwrap_or_default();
    let subcommand = args.get(1).map(String::as_str);
    let replacement = match (command, subcommand) {
        ("daemon", _) => Some("Run \"unharnessed help\" to see the agent commands.".to_string()),
        _ => None,
    };
    let joined: Vec<&str> = args.iter().take(2).map(String::as_str).collect();
    fail(
        format!("Unknown command: {}", joined.join(" ")),
        replacement,
    )
}

fn run_mcp(args: &[String]) -> PublicCommandResult {
    match run_mcp_management_command(args) {
        Ok(message) => {
            println!("{message}");
            handled()
        }
        Err(error) => fail(error.to_string(), None),
    }
}

fn has_positional_arguments(args: &[String]) -> bool {
    let parsed = parse_args(args);
    !parsed.messages.is_empty() || !parsed.file_args.is_empty()
}

fn rewrite_nested_command(
    parent: &str,
    subcommand: &str,
    flag: &str,
    args: &[String],
) -> PublicCommandResult {
    if args.first().map(String::as_str) != Some(subcommand) {
        let candidate = args.first().map(String::as_str);
        return match candidate {
            Some(candidate) => {
                let suggestion = find_command_suggestion(candidate, &[subcommand]);
                fail(
                    format!("Unknown {parent} command: {candidate}"),
                    Some(suggestion.map_or_else(
                        || format!("Run \"{APP_NAME} help {parent}\" for usage."),
                        |s| format!("Did you mean \"{APP_NAME} {parent} {s}\"?"),
                    )),
                )
            }
            None => fail(
                format!("Missing {parent} command."),
                Some(format!("Run \"{APP_NAME} help {parent}\" for usage.")),
            ),
        };
    }
    let usage = get_command_spec(&[parent, subcommand]).map_or_else(
        || format!("{APP_NAME} {parent} {subcommand}"),
        |spec| format!("{APP_NAME} {}", spec.usage),
    );
    let Some((operands, options)) = split_operands_and_options(&args[1..]) else {
        return fail(format!("Usage: {usage}"), None);
    };
    let valid_count = if parent == "model" {
        operands.len() <= 1
    } else {
        !operands.is_empty() && operands.len() <= 2
    };
    if !valid_count {
        return fail(format!("Usage: {usage}"), None);
    }
    let mut args: Vec<String> = vec![
        INTERNAL_RUNTIME_COMMAND_MARKER.to_string(),
        flag.to_string(),
    ];
    args.extend(operands);
    args.extend(options);
    continue_with(args)
}

fn split_operands_and_options(args: &[String]) -> Option<(Vec<String>, Vec<String>)> {
    let options_start = args.iter().position(|arg| arg.starts_with('-'));
    match options_start {
        None => Some((args.to_vec(), vec![])),
        Some(start) => {
            let options = &args[start..];
            if has_positional_arguments(options) {
                return None;
            }
            Some((args[..start].to_vec(), options.to_vec()))
        }
    }
}
