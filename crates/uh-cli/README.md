# uh-cli

The `unharnessed` binary and composition root.

## Scope

- Argument parsing, mode selection and exit codes. Modes: `-p` (print, `text`
  or `json` via `--mode`) and `--mode acp`. Public commands: `help`,
  `mcp add|list|get|remove`, `model list`, `session export`, `prompt`. The old
  TUI, the daemon modes and their commands (`agents`, `attach`, `status`,
  `doctor`, `shutdown`, `update`, `schedule`, ...) were removed; the old names
  print removal guidance.
- **Interactive terminal** (`terminal/`): a terminal launch without `-p`. It is
  an ACP client, like the VS Code extension: it spawns `unharnessed --mode acp`
  and drives it over JSON-RPC (`agent.rs`), so it holds no session logic. It
  draws an inline screen (`screen.rs`: committed lines scroll into the
  terminal's scrollback, one live frame is redrawn below them), the streamed
  answer as Markdown (`markdown.rs`), the cells as short blocks
  (`transcript.rs`), an input box (`editor.rs`), pickers (`picker.rs`), the
  slash commands (`commands.rs`) and the `_unharnessed/providers/*` sign-ins
  (`login.rs`), split by responsibility (`session.rs`: start, `-c`,
  `--resume`, `/new`, `/resume`; `settings.rs`: `/model`, `/effort`,
  `/fast`, `/help`). The agent process gets this run's own flags; only the
  messages, the `@file` arguments and the session choice stay with the
  terminal. Sent messages are kept in `<agent dir>/prompt-history.jsonl`
  (plain text, last 500) for ↑. Verifier:
  `scripts/unharnessed-terminal-e2e.py`.
- **Print runtime** (`print_runtime.rs`): drives the in-process session engine
  for `-p`. It admits the prompts, runs the turn-boundary compaction checks
  (`print_boundary.rs`: provider retry/failover, overflow compact-and-retry,
  threshold), emits the json event stream (session header,
  `message_update` deltas, tool events, `compaction_start`/`compaction_end`,
  `goal_update`, `session_action_update`), and picks the terminal result
  (stdout, stderr, exit code). Once the prompts settle, it runs the session's
  wakes (a background `bash()` completion, a goal backoff, a quota reset) as
  further turns, waiting at most `--background-wait-ms` (default 10 minutes)
  each time; a goal waiting on a running command outlasts that limit. The kernel shuts down before the process exits: a persisted
  session flushes its snapshot, so `-c`/`-r` revive the Python namespace, and
  the background commands still running die with it.
- **Goals and autonomous mode** (`print_goal.rs`, `print_autonomous.rs`): the
  `--goal` seed rides the first turn; the in-loop continuation hook runs an
  active goal's or the autonomous run's natural continuations inside one agent
  run, with the budget-limit steer and the threshold-held continuation drained
  as follow-up runs. A goal that ends `blocked` ends the run with exit code 0.
- **Session slash commands** (`print_session_command.rs`): prompts that are
  `/compact`, `/goal` or `/autonomous` run through the uh-core session-command
  executor instead of reaching the model.
- **ACP** (`--mode acp`): wires uh-daemon's ACP transport to the same headless
  engine builder, and hands it the provider logins.
- **Provider logins** (`provider_login.rs`, `subscription_login.rs`,
  `codex_subscription_login.rs`, `prime_inference_login.rs`): the provider
  list, the API-key store and the browser sign-ins, implementing uh-daemon's
  `provider_auth` contract over the auth-panel channel. The ACP
  `_unharnessed/providers/*` methods drive them (the VS Code extension's
  login screens).
- **MCP login logic** kept for a future front-end (`mcp_login.rs`,
  `client_auth.rs`): the MCP auth flows, driven through the same channel.
- **Packaged layout**: the binary plus `package.json`, `unharnessed-runtime/`
  and `skills/`, resolved from the executable's directory; the hidden
  `--unharnessed-bootstrap` flag pre-builds the kernel venv. Verifier:
  `tests/packaged_layout_e2e.rs`.

## Non-goals

No business logic: it composes uh-core, uh-daemon, uh-ai and uh-agent through
their public APIs.

## Public API

The `unharnessed` executable surface (flags, subcommands, exit codes);
`print_runtime` is public for the integration verifiers. Everything else is
`pub(crate)`.

## Depends on

uh-types, uh-ai, uh-agent, uh-core, uh-daemon (one-way, composition root).
Verifiers: `tests/print_runtime_e2e.rs`, `tests/acp_mode_e2e.rs`,
`tests/acp_config_option_e2e.rs`, `tests/continue_guard_e2e.rs`.
