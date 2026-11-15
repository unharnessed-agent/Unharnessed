//! `/login` and `/logout`: the agent's provider sign-ins
//! (`_unharnessed/providers/*`), drawn in the terminal. Four ways in: an
//! API key, a browser sign-in (with a paste fallback or a device code), a
//! local model server, or any OpenAI-compatible endpoint.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::agent::RpcError;
use super::picker::{Item, Picker};
use super::screen::{Color, Line, Style};
use super::transcript::sanitize;
use super::{App, Asked, Block, TICK};

const LOCAL_ROW: &str = "local";
const CUSTOM_ROW: &str = "custom";
const MODELS_WAIT: Duration = Duration::from_secs(5);

/// A provider row of `_unharnessed/providers/list`.
struct Provider {
    id: String,
    name: String,
    method: String,
    login: String,
    connected: bool,
    removable: bool,
    note: String,
    local: bool,
}

impl Provider {
    fn from(row: &Value) -> Option<Provider> {
        let text = |key: &str| {
            row.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        Some(Provider {
            id: row.get("id")?.as_str()?.to_string(),
            name: text("name"),
            method: text("method"),
            login: text("login"),
            connected: row
                .get("connected")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            removable: row
                .get("removable")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            note: text("note"),
            local: row.get("local").is_some(),
        })
    }
}

/// What the user picked in `/login`.
enum Choice {
    Provider(Provider),
    Local,
    Custom,
}

impl App {
    fn provider_rows(&mut self) -> Option<Vec<Provider>> {
        match self.call("_unharnessed/providers/list", &json!({})) {
            Ok(result) => {
                let rows = result
                    .get("providers")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                self.providers.clone_from(&rows);
                Some(rows.iter().filter_map(Provider::from).collect())
            }
            Err(RpcError(error)) => {
                self.note(
                    "✗",
                    &format!("cannot list the providers: {error}"),
                    Style::fg(Color::Red),
                );
                None
            }
        }
    }

    pub(super) fn login(&mut self, query: &str) {
        let Some(rows) = self.provider_rows() else {
            return;
        };
        let mut items: Vec<Item> = rows
            .iter()
            .map(|p| {
                let mut detail = match (p.method.as_str(), p.login.as_str()) {
                    ("subscription", _) => "subscription · browser sign-in".to_string(),
                    _ if p.local => "local server".to_string(),
                    (_, "browser") => "browser sign-in or API key".to_string(),
                    _ => "API key".to_string(),
                };
                if p.connected {
                    detail.push_str(" · connected");
                }
                if !p.note.is_empty() {
                    let _ = write!(detail, " · {}", p.note);
                }
                Item::new(p.name.clone(), detail).marked(p.connected)
            })
            .collect();
        items.push(Item::new(
            "Local server",
            "Ollama, LM Studio, llama.cpp, vLLM on this machine",
        ));
        items.push(Item::new(
            "Custom endpoint",
            "any OpenAI-compatible API: Nous Portal, Together, a gateway…",
        ));
        let picker = Picker::new("Sign in", items)
            .with_filter(query)
            .mark_symbol("✓");
        let Some(index) = self.pick(picker).and_then(|chosen| chosen.first().copied()) else {
            return;
        };
        let mut rows = rows;
        let choice = match index.checked_sub(rows.len()) {
            None => Choice::Provider(rows.swap_remove(index)),
            Some(0) => Choice::Local,
            Some(_) => Choice::Custom,
        };
        let signed_in = match choice {
            Choice::Provider(provider) if provider.login == "apiKey" => {
                self.login_api_key(&provider)
            }
            Choice::Provider(provider) => self.login_browser(&provider),
            Choice::Local => self.login_server(LOCAL_ROW),
            Choice::Custom => self.login_server(CUSTOM_ROW),
        };
        if let Some(provider) = signed_in {
            self.after_sign_in(&provider);
        }
    }

    /// Show a settled sign-in result; the provider id when it worked.
    fn settled(&mut self, provider: &str, result: Result<Value, RpcError>) -> Option<String> {
        match result {
            Ok(result) => {
                if let Some(list) = result.get("providers").and_then(Value::as_array) {
                    self.providers.clone_from(list);
                }
                let message = result
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match result.get("outcome").and_then(Value::as_str) {
                    Some("done") => {
                        let message = if message.is_empty() {
                            "signed in"
                        } else {
                            message
                        };
                        self.note("✓", message, Style::fg(Color::Green));
                        if let Some(warning) = self
                            .providers
                            .iter()
                            .find(|p| p.get("id").and_then(Value::as_str) == Some(provider))
                            .and_then(|p| p.get("warning"))
                            .and_then(Value::as_str)
                        {
                            let warning = warning.to_string();
                            self.note("⚠", &warning, Style::fg(Color::Yellow));
                        }
                        Some(provider.to_string())
                    }
                    Some("cancelled") => {
                        self.note("·", "sign-in cancelled", Style::dim());
                        None
                    }
                    _ => {
                        let message = if message.is_empty() {
                            "the sign-in failed"
                        } else {
                            message
                        };
                        self.note("✗", message, Style::fg(Color::Red));
                        None
                    }
                }
            }
            Err(RpcError(error)) => {
                self.note("✗", &error, Style::fg(Color::Red));
                None
            }
        }
    }

    fn login_api_key(&mut self, provider: &Provider) -> Option<String> {
        let title = format!("{} API key", provider.name);
        let note = "Stored in ~/.unharnessed/auth.json. Environment variables keep working too.";
        let key = self.ask(&title, note, "paste the key", true, "", false)?;
        let result = self.call(
            "_unharnessed/providers/set_api_key",
            &json!({"providerId": provider.id, "apiKey": key}),
        );
        self.settled(&provider.id, result)
    }

    /// A browser sign-in: the agent sends the page to open, maybe a code
    /// to type there, maybe a field to paste the final address into.
    fn login_browser(&mut self, provider: &Provider) -> Option<String> {
        let id = self.agent.request(
            "_unharnessed/providers/login",
            &json!({"providerId": provider.id}),
        );
        self.awaiting.insert(id);
        self.status = Some(format!("signing in to {}", provider.name));
        let remote =
            std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some();
        let mut cancelled = false;
        let outcome = loop {
            if let Some(frame) = self.responses.remove(&id) {
                break super::agent::response(&frame);
            }
            if self.dead {
                break Err(RpcError("the agent stopped".to_string()));
            }
            self.show_login_progress(remote);
            if let Some(update) = self.next_login_question() {
                let kind = update
                    .pointer("/update/kind")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if kind == "team" {
                    self.answer_team(&update);
                } else {
                    // Prime's paste field takes an API key, which the
                    // server sends as a plain prompt: mask it by provider,
                    // as VS Code does (vscode/webview/SignIn.tsx).
                    let secret = update.pointer("/update/secret").and_then(Value::as_bool)
                        == Some(true)
                        || provider.id == "prime-inference";
                    cancelled |= self.answer_prompt(&update, id, secret);
                }
                continue;
            }
            match self.pump(TICK) {
                Some(crossterm::event::Event::Key(key))
                    if key.kind != crossterm::event::KeyEventKind::Release =>
                {
                    let ctrl = key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL);
                    let stop = key.code == crossterm::event::KeyCode::Esc
                        || (ctrl && key.code == crossterm::event::KeyCode::Char('c'));
                    if stop && !cancelled {
                        cancelled = true;
                        self.agent
                            .notify("_unharnessed/providers/login_cancel", &json!({}));
                        self.status = Some("cancelling the sign-in".to_string());
                    }
                }
                Some(crossterm::event::Event::Resize(..)) => self.on_resize(),
                _ => {}
            }
        };
        self.status = None;
        self.login_updates.clear();
        self.settled(&provider.id, outcome)
    }

    /// Commit the progress lines, the sign-in page and the device code;
    /// leave the questions queued.
    fn show_login_progress(&mut self, remote: bool) {
        let mut keep = std::collections::VecDeque::new();
        while let Some(update) = self.login_updates.pop_front() {
            let body = update.get("update").cloned().unwrap_or(Value::Null);
            let text =
                |key: &str| sanitize(body.get(key).and_then(Value::as_str).unwrap_or_default());
            match body.get("kind").and_then(Value::as_str) {
                Some("progress") => self.note("·", &text("message"), Style::dim()),
                Some("waiting") => self.status = Some(text("message")),
                Some("url") => {
                    let url = text("url");
                    let instructions = text("instructions");
                    let opened = !remote;
                    if opened {
                        self.open_sign_in_page();
                    }
                    let lead = if opened {
                        "Your browser opens on this page:"
                    } else {
                        "Open this page to sign in:"
                    };
                    self.note("→", lead, Style::fg(Color::Accent));
                    // One unbroken line, so the address copies whole.
                    self.commit(
                        Block::Notice,
                        vec![Line::plain("    ").with(url, Style::fg(Color::Accent).underline())],
                    );
                    if let Some(code) = instructions.strip_prefix("Enter code:") {
                        self.commit(
                            Block::Notice,
                            vec![Line::plain("    ")
                                .with("code  ", Style::dim())
                                .with(code.trim().to_string(), Style::fg(Color::Yellow).bold())],
                        );
                    } else if !instructions.is_empty() {
                        self.note(" ", &instructions, Style::dim());
                    }
                }
                _ => keep.push_back(update),
            }
        }
        self.login_updates = keep;
    }

    fn next_login_question(&mut self) -> Option<Value> {
        let index = self.login_updates.iter().position(|update| {
            matches!(
                update.pointer("/update/kind").and_then(Value::as_str),
                Some("prompt" | "team")
            )
        })?;
        self.login_updates.remove(index)
    }

    /// Ask the flow's question. A field that is only an alternative to the
    /// browser closes by itself when the browser sign-in ends. Returns
    /// whether the user cancelled the sign-in.
    fn answer_prompt(&mut self, update: &Value, login: u64, secret: bool) -> bool {
        let body = update.get("update").cloned().unwrap_or(Value::Null);
        let prompt_id = body.get("promptId").cloned().unwrap_or(Value::Null);
        let message = sanitize(
            body.get("message")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        let allow_empty = body.get("allowEmpty").and_then(Value::as_bool) == Some(true);
        let optional = body.get("optional").and_then(Value::as_bool) == Some(true);
        let note = if optional {
            "Finish in the browser, or paste the address the browser ended on here."
        } else {
            ""
        };
        let asked = self.ask_until(&message, note, "", secret, "", allow_empty, |app| {
            app.show_login_progress(true);
            app.responses.contains_key(&login) || app.dead
        });
        match asked {
            Asked::Answer(answer) => {
                let answered = self.call(
                    "_unharnessed/providers/login_answer",
                    &json!({"promptId": prompt_id, "answer": {"kind": "text", "value": answer}}),
                );
                // A refused answer leaves the prompt open on the server:
                // say why and ask again.
                if let Err(RpcError(error)) = answered {
                    self.note("✗", &error, Style::fg(Color::Red));
                    self.login_updates.push_front(update.clone());
                }
                false
            }
            Asked::Cancelled => {
                self.agent
                    .notify("_unharnessed/providers/login_cancel", &json!({}));
                self.status = Some("cancelling the sign-in".to_string());
                true
            }
            Asked::Stopped => false,
        }
    }

    fn answer_team(&mut self, update: &Value) {
        let body = update.get("update").cloned().unwrap_or(Value::Null);
        let prompt_id = body.get("promptId").cloned().unwrap_or(Value::Null);
        let teams: Vec<Value> = body
            .get("teams")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let current = body
            .get("current")
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut items = vec![Item::new("Personal account", "").marked(current.is_none())];
        for team in &teams {
            let id = team.get("id").and_then(Value::as_str).unwrap_or_default();
            let name = team.get("name").and_then(Value::as_str).unwrap_or(id);
            let role = team.get("role").and_then(Value::as_str).unwrap_or_default();
            items.push(Item::new(name, role).marked(current.as_deref() == Some(id)));
        }
        let answer = match self
            .pick(Picker::new("Which account?", items))
            .and_then(|chosen| chosen.first().copied())
        {
            Some(0) => json!({"kind": "team", "teamId": null}),
            Some(index) => {
                json!({"kind": "team", "teamId": teams[index - 1].get("id").cloned().unwrap_or(Value::Null)})
            }
            None => json!({"kind": "dismiss"}),
        };
        let answered = self.call(
            "_unharnessed/providers/login_answer",
            &json!({"promptId": prompt_id, "answer": answer}),
        );
        if let Err(RpcError(error)) = answered {
            self.note("✗", &error, Style::fg(Color::Red));
            self.login_updates.push_front(update.clone());
        }
    }

    /// Ask the agent to open the sign-in page (its answer only says the
    /// opener started; nobody waits for it).
    fn open_sign_in_page(&mut self) {
        let _ = self
            .agent
            .request("_unharnessed/providers/login_open", &json!({}));
    }

    /// A local server (scanned, or typed) or a custom endpoint: probe it,
    /// pick its models, save it in models.json.
    fn login_server(&mut self, kind: &str) -> Option<String> {
        let custom = kind == CUSTOM_ROW;
        let mut base_url = String::new();
        let mut api_key: Option<String> = None;
        let mut found: Option<Value> = None;
        if !custom {
            let scanned = match self.call("_unharnessed/providers/local_scan", &json!({})) {
                Ok(result) => result
                    .get("servers")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
                Err(RpcError(error)) => {
                    self.note("✗", &error, Style::fg(Color::Red));
                    return None;
                }
            };
            let mut items: Vec<Item> = scanned
                .iter()
                .map(|server| {
                    let name = server
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("server");
                    let url = server
                        .get("baseUrl")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let count = server
                        .get("models")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len);
                    Item::new(name, format!("{url} · {count} models"))
                })
                .collect();
            items.push(Item::new("Another address…", "type the server's address"));
            let title = if scanned.is_empty() {
                "No local server answered"
            } else {
                "Local server"
            };
            let index = self.pick(Picker::new(title, items))?.first().copied()?;
            if let Some(server) = scanned.get(index) {
                found = Some(server.clone());
            }
        }
        if found.is_none() {
            let (title, placeholder) = if custom {
                (
                    "Endpoint address",
                    "https://inference-api.nousresearch.com/v1",
                )
            } else {
                ("Server address", "localhost:11434")
            };
            let note = if custom {
                "The OpenAI-compatible base URL, the one that ends before /chat/completions."
            } else {
                ""
            };
            base_url = self.ask(title, note, placeholder, false, "", false)?;
            if custom {
                let key = self.ask(
                    "API key",
                    "Leave empty if the endpoint needs none. Saved in ~/.unharnessed/models.json.",
                    "paste the key",
                    true,
                    "",
                    true,
                )?;
                api_key = Some(key).filter(|key| !key.is_empty());
            }
            found = Some(self.probe(&base_url, &mut api_key)?);
        }
        let server = found?;
        let models: Vec<Value> = server
            .get("models")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if models.is_empty() {
            self.note("✗", "the server lists no model", Style::fg(Color::Red));
            return None;
        }
        let server_url = server
            .get("baseUrl")
            .and_then(Value::as_str)
            .unwrap_or(&base_url)
            .to_string();
        let mut name = server
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("local")
            .to_string();
        if custom {
            let suggestion = host_name(&server_url);
            name = self.ask(
                "Name",
                "How the provider shows in /model.",
                "",
                false,
                &suggestion,
                false,
            )?;
        }
        let ticked = vec![models.len() <= 12; models.len()];
        let items: Vec<Item> = models
            .iter()
            .map(|model| {
                let id = model.get("id").and_then(Value::as_str).unwrap_or("?");
                let mut detail = Vec::new();
                if let Some(window) = model.get("contextWindow").and_then(Value::as_u64) {
                    detail.push(format!(
                        "{} context",
                        super::settings::compact_count(window)
                    ));
                }
                if model.get("reasoning").and_then(Value::as_bool) == Some(true) {
                    detail.push("reasoning".to_string());
                }
                if model.get("image").and_then(Value::as_bool) == Some(true) {
                    detail.push("images".to_string());
                }
                Item::new(id, detail.join(" · "))
            })
            .collect();
        let chosen = self.pick(Picker::new("Models to add", items).multi(ticked))?;
        let picked: Vec<Value> = chosen
            .iter()
            .filter_map(|index| models.get(*index).cloned())
            .collect();
        let id = slug(&name);
        let mut params = json!({"id": id, "name": name, "baseUrl": server_url, "models": picked});
        if let Some(key) = api_key {
            params["apiKey"] = json!(key);
        }
        let result = self.call("_unharnessed/providers/local_save", &params);
        // The saved provider id may differ (made unique): find it by URL.
        self.settled(&id, result)
            .map(|_| self.provider_for_url(&server_url).unwrap_or(id))
    }

    /// Probe an address; ask for a key when the server wants one.
    fn probe(&mut self, base_url: &str, api_key: &mut Option<String>) -> Option<Value> {
        for _ in 0..3 {
            let mut params = json!({"baseUrl": base_url});
            if let Some(key) = api_key.as_ref() {
                params["apiKey"] = json!(key);
            }
            self.status = Some(format!("asking {base_url} for its models"));
            let result = self.call("_unharnessed/providers/local_probe", &params);
            self.status = None;
            let result = match result {
                Ok(result) => result,
                Err(RpcError(error)) => {
                    self.note("✗", &error, Style::fg(Color::Red));
                    return None;
                }
            };
            if let Some(server) = result.get("server") {
                return Some(server.clone());
            }
            let kind = result
                .pointer("/error/kind")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let message = result
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("no answer")
                .to_string();
            if kind == "needsKey" {
                self.note("·", &message, Style::fg(Color::Yellow));
                let key = self.ask(
                    "API key",
                    "The server asks for a key.",
                    "paste the key",
                    true,
                    "",
                    false,
                )?;
                *api_key = Some(key);
                continue;
            }
            self.note("✗", &message, Style::fg(Color::Red));
            return None;
        }
        None
    }

    fn provider_for_url(&self, url: &str) -> Option<String> {
        self.providers
            .iter()
            .find(|p| {
                p.pointer("/local/baseUrl")
                    .and_then(Value::as_str)
                    .map(|u| u.trim_end_matches('/'))
                    == Some(url.trim_end_matches('/'))
            })
            .and_then(|p| p.get("id").and_then(Value::as_str))
            .map(str::to_string)
    }

    /// After a sign-in, the agent refreshes its models; offer the new
    /// provider's models right away.
    fn after_sign_in(&mut self, provider: &str) {
        let before = self.config_option("model").cloned();
        let has_provider_models = |app: &App| {
            app.config_option("model")
                .and_then(|option| option.get("options"))
                .and_then(Value::as_array)
                .is_some_and(|options| {
                    options.iter().any(|o| {
                        o.pointer("/_meta/unharnessed/model/provider")
                            .and_then(Value::as_str)
                            == Some(provider)
                    })
                })
        };
        let deadline = Instant::now() + MODELS_WAIT;
        self.status = Some("loading the models".to_string());
        while Instant::now() < deadline && !has_provider_models(self) && !self.dead {
            let event = self.pump(TICK);
            if let Some(crossterm::event::Event::Key(key)) = event {
                if key.code == crossterm::event::KeyCode::Esc {
                    break;
                }
            }
        }
        self.status = None;
        self.dirty = true;
        let changed = self.config_option("model") != before.as_ref();
        if has_provider_models(self) {
            self.pick_model("", Some(provider));
        } else if changed {
            self.pick_model("", None);
        } else {
            self.note("·", "/model shows the models you can use now", Style::dim());
        }
    }

    pub(super) fn logout(&mut self, query: &str) {
        let Some(rows) = self.provider_rows() else {
            return;
        };
        let removable: Vec<Provider> = rows
            .into_iter()
            .filter(|p| p.removable || p.local)
            .collect();
        if removable.is_empty() {
            self.note("·", "no stored credential to remove (keys from environment variables stay where they are)", Style::dim());
            return;
        }
        let items: Vec<Item> = removable
            .iter()
            .map(|p| {
                let what = if p.local {
                    "local server"
                } else if p.method == "subscription" {
                    "subscription"
                } else {
                    "API key"
                };
                Item::new(p.name.clone(), what)
            })
            .collect();
        let Some(index) = self
            .pick(Picker::new("Sign out of", items).with_filter(query))
            .and_then(|c| c.first().copied())
        else {
            return;
        };
        let provider = &removable[index];
        let method = if provider.local {
            "_unharnessed/providers/local_remove"
        } else {
            "_unharnessed/providers/logout"
        };
        let result = self.call(method, &json!({"providerId": provider.id}));
        let id = provider.id.clone();
        let _ = self.settled(&id, result);
    }
}

/// A short name from an address: `inference-api.nousresearch.com` →
/// `nousresearch`.
fn host_name(url: &str) -> String {
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split(['/', ':'])
        .next()
        .unwrap_or_default();
    if host == "localhost" || host.parse::<std::net::IpAddr>().is_ok() || host.starts_with('[') {
        return "custom".to_string();
    }
    let parts: Vec<&str> = host.split('.').filter(|part| !part.is_empty()).collect();
    let name = match parts.as_slice() {
        [] => "custom",
        [one] => one,
        [.., name, _tld] => name,
    };
    name.to_string()
}

/// A provider id from a display name.
fn slug(name: &str) -> String {
    let mut out = String::new();
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "custom".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_names_are_short() {
        assert_eq!(
            host_name("https://inference-api.nousresearch.com/v1"),
            "nousresearch"
        );
        assert_eq!(host_name("http://localhost:11434/v1"), "custom");
        assert_eq!(host_name("http://127.0.0.1:18080/v1"), "custom");
        assert_eq!(host_name("gw.example.co"), "example");
    }
}
