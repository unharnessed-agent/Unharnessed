//! The slash commands: the ones the terminal runs itself, and the ones
//! the agent advertises (`/compact`, `/goal`, `/autonomous`, prompt
//! templates, skills), which go to the agent as a prompt.

use serde_json::Value;

/// A command the terminal runs itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Local {
    Help,
    Model,
    Effort,
    Fast,
    Login,
    Logout,
    New,
    Resume,
    Quit,
}

/// One entry of the completion list.
#[derive(Clone, Debug)]
pub(crate) struct Command {
    pub name: String,
    pub hint: String,
    pub description: String,
}

const LOCAL: &[(&str, &str, &str, Local)] = &[
    (
        "model",
        "[search]",
        "Choose the model (saved as your default)",
        Local::Model,
    ),
    (
        "effort",
        "[level]",
        "Choose the reasoning effort",
        Local::Effort,
    ),
    (
        "fast",
        "",
        "Turn fast mode on or off, when the model has it",
        Local::Fast,
    ),
    (
        "login",
        "[provider]",
        "Connect a provider: subscription, API key, local or custom server",
        Local::Login,
    ),
    (
        "logout",
        "[provider]",
        "Remove a stored credential",
        Local::Logout,
    ),
    ("new", "", "Start a new conversation", Local::New),
    (
        "resume",
        "",
        "Reopen a saved conversation from this folder",
        Local::Resume,
    ),
    ("help", "", "Commands and keys", Local::Help),
    ("quit", "", "Leave (the conversation is saved)", Local::Quit),
];

const ALIASES: &[(&str, Local)] = &[
    ("clear", Local::New),
    ("exit", Local::Quit),
    ("thinking", Local::Effort),
    ("models", Local::Model),
];

pub(crate) fn local_commands() -> Vec<Command> {
    LOCAL
        .iter()
        .map(|(name, hint, description, _)| Command {
            name: (*name).to_string(),
            hint: (*hint).to_string(),
            description: (*description).to_string(),
        })
        .collect()
}

/// The agent's commands from `available_commands_update` (built-in
/// skills are left out, as in VS Code: the model loads them itself).
pub(crate) fn agent_commands(update: &Value) -> Vec<Command> {
    let Some(list) = update.get("availableCommands").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter(|command| {
            command
                .pointer("/_meta/unharnessed/source")
                .and_then(Value::as_str)
                != Some("builtin")
        })
        .filter_map(|command| {
            let name = command.get("name")?.as_str()?.to_string();
            let description = command
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let hint = command
                .pointer("/input/hint")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Some(Command {
                name,
                hint,
                description,
            })
        })
        .collect()
}

/// A typed line split into a command name and its argument, when it
/// looks like a command (`/name` followed by nothing or a space).
pub(crate) fn parse(line: &str) -> Option<(&str, &str)> {
    let rest = line.trim_start().strip_prefix('/')?;
    let (name, arg) = match rest.find(char::is_whitespace) {
        Some(index) => (&rest[..index], rest[index..].trim()),
        None => (rest, ""),
    };
    let looks_like_name = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'));
    looks_like_name.then_some((name, arg))
}

/// The local command a name runs, aliases included.
pub(crate) fn find_local(name: &str) -> Option<Local> {
    LOCAL
        .iter()
        .find(|(command, ..)| *command == name)
        .map(|(.., local)| *local)
        .or_else(|| {
            ALIASES
                .iter()
                .find(|(alias, _)| *alias == name)
                .map(|(_, local)| *local)
        })
}

/// The completion candidates for what is typed: only while the line is a
/// lone `/word` being written.
pub(crate) fn complete<'a>(typed: &str, commands: &'a [Command]) -> Vec<&'a Command> {
    let Some(word) = typed.strip_prefix('/') else {
        return Vec::new();
    };
    if word.contains(char::is_whitespace) {
        return Vec::new();
    }
    let mut prefix: Vec<&Command> = commands
        .iter()
        .filter(|c| c.name.starts_with(word))
        .collect();
    if prefix.is_empty() && !word.is_empty() {
        let found = crate::fuzzy::fuzzy_filter(commands, word, |c| c.name.clone());
        let names: Vec<String> = found.iter().map(|c| c.name.clone()).collect();
        prefix = names
            .iter()
            .filter_map(|name| commands.iter().find(|c| &c.name == name))
            .collect();
    }
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_takes_a_name_and_an_argument() {
        assert_eq!(parse("/model gpt"), Some(("model", "gpt")));
        assert_eq!(parse("/goal  ship it "), Some(("goal", "ship it")));
        assert_eq!(parse("/skill:deploy now"), Some(("skill:deploy", "now")));
        assert_eq!(parse("/usr/bin/env is a path"), None);
        assert_eq!(parse("plain text"), None);
        assert_eq!(parse("/"), None);
    }

    #[test]
    fn aliases_reach_their_commands() {
        assert_eq!(find_local("clear"), Some(Local::New));
        assert_eq!(find_local("exit"), Some(Local::Quit));
        assert_eq!(find_local("compact"), None);
    }

    #[test]
    fn completion_prefers_prefixes_then_fuzzy() {
        let commands = local_commands();
        let names = |typed: &str| -> Vec<String> {
            complete(typed, &commands)
                .iter()
                .map(|c| c.name.clone())
                .collect()
        };
        assert_eq!(names("/mo"), vec!["model"]);
        assert_eq!(names("/lo"), vec!["login", "logout"]);
        assert!(names("/mdl").contains(&"model".to_string()));
        assert!(names("/model gpt").is_empty());
        assert!(names("hello").is_empty());
        assert_eq!(names("/").len(), commands.len());
    }

    #[test]
    fn builtin_skills_are_hidden() {
        let update = json!({"availableCommands": [
            {"name": "compact", "description": "Compact", "input": {"hint": "[instructions]"}, "_meta": {"unharnessed": {"kind": "command"}}},
            {"name": "skill:goal", "description": "(skill)", "_meta": {"unharnessed": {"kind": "skill", "source": "builtin"}}},
            {"name": "skill:deploy", "description": "(skill)", "_meta": {"unharnessed": {"kind": "skill", "source": "user"}}}
        ]});
        let names: Vec<String> = agent_commands(&update)
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["compact", "skill:deploy"]);
    }
}
