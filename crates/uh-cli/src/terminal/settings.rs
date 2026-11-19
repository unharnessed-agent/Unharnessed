//! The settings commands: `/help`, `/model`, `/effort`, `/fast`, and what
//! the footer reads (the model, the sign-in state).

use std::fmt::Write as _;

use serde_json::{json, Value};

use super::agent::RpcError;
use super::commands;
use super::picker::{Item, Picker};
use super::screen::{wrap, Color, Line, Style};
use super::{App, Block};

impl App {
    pub(super) fn help(&mut self) {
        let width = self.width();
        let mut lines = vec![Line::styled("  Commands", Style::fg(Color::Accent).bold())];
        let mut all = commands::local_commands();
        all.extend(
            self.agent_commands
                .iter()
                .filter(|c| !c.name.starts_with("skill:"))
                .cloned(),
        );
        let skills = self
            .agent_commands
            .iter()
            .filter(|c| c.name.starts_with("skill:"))
            .count();
        let name_width = all
            .iter()
            .map(|c| c.name.len() + c.hint.len() + 2)
            .max()
            .unwrap_or(0);
        for command in &all {
            let mut line =
                Line::plain("    ").with(format!("/{}", command.name), Style::PLAIN.bold());
            let mut used = command.name.len() + 1;
            if !command.hint.is_empty() {
                line.push(format!(" {}", command.hint), Style::dim());
                used += command.hint.len() + 1;
            }
            line.push(
                " ".repeat(name_width.saturating_sub(used) + 2),
                Style::PLAIN,
            );
            line.push(command.description.clone(), Style::dim());
            lines.push(line.truncate(width));
        }
        if skills > 0 {
            let plural = if skills == 1 { "" } else { "s" };
            lines.push(
                Line::plain("    ")
                    .with("/skill:", Style::PLAIN.bold())
                    .with("<name>", Style::dim())
                    .with(
                        format!("  {skills} skill{plural}: type /skill: to browse them"),
                        Style::dim(),
                    )
                    .truncate(width),
            );
        }
        lines.push(Line::new());
        lines.push(Line::styled("  Keys", Style::fg(Color::Accent).bold()));
        for (keys, what) in [
            ("enter", "send (while the agent works: queue for after)"),
            ("alt+enter · ctrl+j · \\ enter", "new line"),
            ("↑ ↓", "earlier messages"),
            (
                "tab",
                "complete a /command; on an empty input, all commands",
            ),
            ("ctrl+p", "switch the model"),
            ("shift+tab", "next reasoning effort"),
            ("esc · ctrl+c", "interrupt the agent"),
            ("ctrl+c twice · ctrl+d", "quit (the conversation is saved)"),
        ] {
            lines.push(
                Line::plain("    ")
                    .with(format!("{keys:<30}"), Style::PLAIN.bold())
                    .with(what, Style::dim())
                    .truncate(width),
            );
        }
        lines.push(Line::new());
        lines.extend(wrap(
            &Line::styled("The model has one tool, a Python REPL that stays alive. It runs with your permissions and never asks first: use a container or a disposable checkout for anything you care about.", Style::dim()),
            width,
            &Line::plain("  "),
            &Line::plain("  "),
        ));
        self.commit(Block::Notice, lines);
    }

    pub(super) fn config_option(&self, id: &str) -> Option<&Value> {
        self.config
            .iter()
            .find(|option| option.get("id").and_then(Value::as_str) == Some(id))
    }

    pub(super) fn config_value(&self, id: &str) -> Option<&Value> {
        self.config_option(id)
            .and_then(|option| option.get("currentValue"))
    }

    pub(super) fn current_model(&self) -> Option<(String, String)> {
        let value = self.config_value("model")?.as_str()?;
        let pair: Vec<String> = serde_json::from_str(value).ok()?;
        match pair.as_slice() {
            [provider, id] => Some((provider.clone(), id.clone())),
            _ => None,
        }
    }

    pub(super) fn current_model_name(&self) -> Option<String> {
        let (_, id) = self.current_model()?;
        Some(id)
    }

    /// Whether the current model can be reached: its provider is
    /// connected, or is not a provider one signs in to at all.
    pub(super) fn signed_in(&self) -> bool {
        if self.providers.is_empty() || self.session_id.is_none() {
            return true;
        }
        let connected = |row: &Value| row.get("connected").and_then(Value::as_bool) == Some(true);
        let Some((provider, _)) = self.current_model() else {
            return self.providers.iter().any(connected);
        };
        let rows: Vec<&Value> = self
            .providers
            .iter()
            .filter(|row| row.get("id").and_then(Value::as_str) == Some(provider.as_str()))
            .collect();
        rows.is_empty() || rows.into_iter().any(connected)
    }

    pub(super) fn refresh_providers(&mut self) {
        if let Ok(result) = self.call("_unharnessed/providers/list", &json!({})) {
            if let Some(list) = result.get("providers").and_then(Value::as_array) {
                self.providers.clone_from(list);
            }
        }
    }

    pub(super) fn set_config(&mut self, id: &str, value: &Value) -> bool {
        let Some(session) = self.session_id.clone() else {
            return false;
        };
        match self.call(
            "session/set_config_option",
            &json!({"sessionId": session, "configId": id, "value": value}),
        ) {
            Ok(result) => {
                if let Some(options) = result.get("configOptions").and_then(Value::as_array) {
                    self.config.clone_from(options);
                }
                true
            }
            Err(RpcError(error)) => {
                self.note("✗", &error, Style::fg(Color::Red));
                false
            }
        }
    }

    /// The model picker; `provider` keeps only that provider's models.
    pub(super) fn pick_model(&mut self, query: &str, provider: Option<&str>) {
        let Some(option) = self.config_option("model").cloned() else {
            self.note(
                "·",
                "no model yet: /login connects a provider",
                Style::fg(Color::Yellow),
            );
            return;
        };
        let current = option
            .get("currentValue")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut options: Vec<Value> = option
            .get("options")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let model_provider = |o: &Value| {
            o.pointer("/_meta/unharnessed/model/provider")
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        if let Some(provider) = provider {
            let scoped: Vec<Value> = options
                .iter()
                .filter(|o| model_provider(o).as_deref() == Some(provider))
                .cloned()
                .collect();
            if !scoped.is_empty() {
                options = scoped;
            }
        }
        let items: Vec<Item> = options
            .iter()
            .map(|o| {
                let meta = o.pointer("/_meta/unharnessed/model");
                let id = meta.and_then(|m| m.get("id")).and_then(Value::as_str);
                let provider = meta
                    .and_then(|m| m.get("provider"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let name = meta.and_then(|m| m.get("name")).and_then(Value::as_str);
                let label = id
                    .or(name)
                    .unwrap_or_else(|| o.get("name").and_then(Value::as_str).unwrap_or("?"));
                let mut detail = provider.to_string();
                if let Some(window) = meta
                    .and_then(|m| m.get("contextWindow"))
                    .and_then(Value::as_u64)
                {
                    let _ = write!(detail, " · {} context", compact_count(window));
                }
                if let Some(cost) = meta.and_then(|m| m.get("cost")) {
                    let input = cost.get("input").and_then(Value::as_f64).unwrap_or(0.0);
                    let output = cost.get("output").and_then(Value::as_f64).unwrap_or(0.0);
                    if input > 0.0 || output > 0.0 {
                        let _ = write!(detail, " · ${input}/${output} per M");
                    }
                }
                Item::new(label, detail)
                    .marked(o.get("value").and_then(Value::as_str) == Some(current.as_str()))
            })
            .collect();
        if items.len() <= 1 && !self.signed_in() {
            self.note(
                "·",
                "no provider is connected yet: /login first",
                Style::fg(Color::Yellow),
            );
            return;
        }
        let title = match provider {
            Some(provider) => format!("Model · {provider}"),
            None => "Model".to_string(),
        };
        let picker = Picker::new(title, items)
            .with_filter(query)
            .hint("saved as your default");
        let Some(index) = self.pick(picker).and_then(|chosen| chosen.first().copied()) else {
            return;
        };
        let Some(value) = options.get(index).and_then(|o| o.get("value")).cloned() else {
            return;
        };
        if value.as_str() == Some(current.as_str()) {
            let name = self.current_model_name().unwrap_or_default();
            self.note(
                "✓",
                &format!("model: {name} (already in use)"),
                Style::fg(Color::Green),
            );
            return;
        }
        if self.set_config("model", &value) {
            let name = self.current_model_name().unwrap_or_default();
            self.note(
                "✓",
                &format!("model: {name} (your default from now on)"),
                Style::fg(Color::Green),
            );
        }
    }

    pub(super) fn pick_effort(&mut self, arg: &str) {
        let Some(option) = self.config_option("thought_level").cloned() else {
            self.note(
                "·",
                "this model has no reasoning effort to choose",
                Style::dim(),
            );
            return;
        };
        let current = option
            .get("currentValue")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let levels: Vec<String> = option
            .get("options")
            .and_then(Value::as_array)
            .map(|options| {
                options
                    .iter()
                    .filter_map(|o| o.get("value").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let level = if arg.is_empty() {
            let items = levels
                .iter()
                .map(|level| Item::new(level.clone(), "").marked(*level == current))
                .collect();
            match self
                .pick(Picker::new("Reasoning effort", items))
                .and_then(|chosen| chosen.first().copied())
            {
                Some(index) => levels[index].clone(),
                None => return,
            }
        } else if levels.iter().any(|level| level == arg) {
            arg.to_string()
        } else {
            self.note(
                "·",
                &format!("this model's levels: {}", levels.join(", ")),
                Style::fg(Color::Yellow),
            );
            return;
        };
        if level != current && self.set_config("thought_level", &json!(level)) {
            self.note(
                "✓",
                &format!("reasoning effort: {level}"),
                Style::fg(Color::Green),
            );
        }
    }

    /// Shift+Tab: the next reasoning effort the model has, round the list.
    pub(super) fn cycle_effort(&mut self) {
        let Some(option) = self.config_option("thought_level").cloned() else {
            self.flash("this model has no reasoning effort to choose", Style::dim());
            return;
        };
        let current = option
            .get("currentValue")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let levels: Vec<String> = option
            .get("options")
            .and_then(Value::as_array)
            .map(|options| {
                options
                    .iter()
                    .filter_map(|o| o.get("value").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if levels.len() < 2 {
            return;
        }
        let index = levels
            .iter()
            .position(|level| *level == current)
            .map_or(0, |i| (i + 1) % levels.len());
        let next = levels[index].clone();
        if self.set_config("thought_level", &json!(next)) {
            let ladder: Vec<String> = levels
                .iter()
                .map(|level| {
                    if *level == next {
                        format!("[{level}]")
                    } else {
                        level.clone()
                    }
                })
                .collect();
            self.flash(
                &format!("reasoning effort: {}", ladder.join(" ")),
                Style::fg(Color::Accent),
            );
        }
    }

    /// Tab on an empty input: every command in a picker.
    pub(super) fn command_palette(&mut self) {
        let all = self.all_commands();
        let items: Vec<Item> = all
            .iter()
            .map(|command| {
                let label = if command.hint.is_empty() {
                    format!("/{}", command.name)
                } else {
                    format!("/{} {}", command.name, command.hint)
                };
                Item::new(label, command.description.clone())
            })
            .collect();
        let Some(index) = self
            .pick(Picker::new("Commands", items))
            .and_then(|chosen| chosen.first().copied())
        else {
            return;
        };
        let command = &all[index];
        // A command that takes an argument waits for it in the input.
        if command.hint.starts_with('<') {
            self.editor.set_text(&format!("/{} ", command.name));
        } else {
            let line = format!("/{}", command.name);
            self.submit(line);
        }
    }

    pub(super) fn toggle_fast(&mut self) {
        let Some(on) = self.config_value("fast_mode").and_then(Value::as_bool) else {
            self.note("·", "this model has no fast mode", Style::dim());
            return;
        };
        if self.set_config("fast_mode", &json!(!on)) {
            let text = if on {
                "fast mode off"
            } else {
                "fast mode on (priority tier, it costs more)"
            };
            self.note("✓", text, Style::fg(Color::Green));
        }
    }
}

/// `1.2K`, `200K`, `1M`.
pub(super) fn compact_count(count: u64) -> String {
    if count >= 1_000_000 {
        let tenths = count / 100_000;
        if tenths.is_multiple_of(10) {
            return format!("{}M", tenths / 10);
        }
        return format!("{}.{}M", tenths / 10, tenths % 10);
    }
    if count >= 1_000 {
        let tenths = count / 100;
        if count >= 10_000 || tenths.is_multiple_of(10) {
            return format!("{}K", count / 1_000);
        }
        return format!("{}.{}K", tenths / 10, tenths % 10);
    }
    count.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_compact() {
        assert_eq!(compact_count(950), "950");
        assert_eq!(compact_count(1_200), "1.2K");
        assert_eq!(compact_count(200_000), "200K");
        assert_eq!(compact_count(1_000_000), "1M");
        assert_eq!(compact_count(1_048_576), "1M");
        assert_eq!(compact_count(1_500_000), "1.5M");
    }
}
