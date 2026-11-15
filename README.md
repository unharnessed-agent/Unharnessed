<p align="center">
  <img src=".github/banner.png" alt="Unharnessed" width="100%">
</p>

<p align="center">
  <a href="https://marketplace.visualstudio.com/items?itemName=Unharnessed.unharnessed-agent"><img alt="VS Code" src="https://img.shields.io/badge/VS_Code-Marketplace-86defc?style=for-the-badge&labelColor=1a1a1b&logo=visualstudiocode&logoColor=white"></a>
  <a href="#architecture"><img alt="Rust" src="https://img.shields.io/badge/Rust-1.98-86defc?style=for-the-badge&labelColor=1a1a1b&logo=rust&logoColor=white"></a>
  <a href="#architecture"><img alt="Python" src="https://img.shields.io/badge/Python-3.11+-86defc?style=for-the-badge&labelColor=1a1a1b&logo=python&logoColor=white"></a>
  <a href="#architecture"><img alt="TypeScript" src="https://img.shields.io/badge/TypeScript-webview-86defc?style=for-the-badge&labelColor=1a1a1b&logo=typescript&logoColor=white"></a>
  <a href="#providers-and-failover"><img alt="Providers" src="https://img.shields.io/badge/Providers-30+-86defc?style=for-the-badge&labelColor=1a1a1b"></a>
  <a href="LICENSE"><img alt="License" src="https://img.shields.io/badge/License-MIT-86defc?style=for-the-badge&labelColor=1a1a1b"></a>
</p>

<h3 align="center">A new generation of harness.</h3>

<p align="center">
  <b>One tool: a Python REPL. The model writes the rest.</b>
</p>

<p align="center">
  <a href="https://marketplace.visualstudio.com/items?itemName=Unharnessed.unharnessed-agent"><b>Install in VS Code</b></a>
  &nbsp;·&nbsp;
  <a href="#three-ways-to-run"><b>Three ways to run</b></a>
  &nbsp;·&nbsp;
  <a href="#architecture"><b>Architecture</b></a>
  &nbsp;·&nbsp;
  <a href="LICENSE"><b>MIT License</b></a>
</p>

<p align="center">
  <code>rust</code> · <code>python kernel</code> · <code>vs code</code> · <code>acp</code> · <code>mcp</code> · <code>open source</code>
</p>

---

Unharnessed is a coding agent harness built on one idea: **the model gets exactly one tool**, `ipython`, a persistent CPython REPL. There is no `read`, no `grep`, no `edit_file`, no `web_fetch`. Reading a file, searching a repository, patching forty call sites, starting a dev server or calling an MCP service: all of it is Python the model writes, in a kernel that stays alive for the whole session.

The harness around it (Rust) does the rest of the job: the agent loop, compaction, goals, provider retry and failover, sessions, the VS Code panel. It ships as a VS Code extension, a headless CLI and an [ACP](https://agentclientprotocol.com) agent.

> [!WARNING]
> Unharnessed never asks before running a command. The model's Python and shell commands run with your user permissions, and the kernel is **not** a sandbox. Use it in a container, a VM or a disposable checkout. [Why there are no permission prompts](#no-permission-prompts).

---

## Contents

- [Why one tool](#why-one-tool): the bet
- [The kernel](#the-kernel): what the model actually gets
  - [The whole system prompt](#the-whole-system-prompt)
  - [`bash()`: async by default](#bash-async-by-default)
  - [Variables instead of context](#variables-instead-of-context)
  - [Functions instead of tools](#functions-instead-of-tools)
- [Three ways to run](#three-ways-to-run): Turn, Goal, Autonomous
- [Wake-ups](#wake-ups): the session starts its own turns
- [Compaction](#compaction): the context is summarised, the kernel is not
- [Sessions](#sessions): JSONL, resume, kernel snapshots
- [Skills](#skills) · [`AGENTS.md`](#agentsmd--claudemd) · [MCP](#mcp-inside-the-kernel)
- [Providers and failover](#providers-and-failover): your subscription, any key, local models
- [What we left out](#what-we-left-out): no sub-agents, no memory, no permission prompts
- [Surfaces](#surfaces): VS Code, CLI, ACP
- [Architecture](#architecture) · [Install](#install) · [Build from source](#build-from-source) · [Development](#development)

---

## Why one tool

Every hardcoded tool is a decision made in advance, on the model's behalf: what a read returns, how an edit is shaped, how much output survives. That was a fair trade when models needed rails. Each generation needs fewer.

Frontier models already reach for Python when they can. A tool list makes them pay for it: each call does one thing, costs a round trip, and pours its result into the context, where it stays. Unharnessed is built for where that curve ends: **one tool, and nothing left to take away.**

| | A tool list | **Unharnessed** |
|---|---|---|
| Read, search, edit, check | 4 calls, 4 round trips | **1 cell, 1 round trip** |
| A 40,000-line log | 40,000 lines in the context | **a variable; the 10 lines it needed in the context** |
| A check across 200 files | 200 calls, 200 results | **1 loop, 1 result** |
| Helpers it writes | none | **functions, kept in the session** |
| After compaction | a summary | **a summary, plus every variable, function and running process** |

---

## The kernel

The kernel is a CPython process (`unharnessed-runtime`, Python 3.11+) in a [uv](https://docs.astral.sh/uv/)-managed venv, built once on first run. It speaks newline-delimited JSON with the Rust host ([protocol](unharnessed-runtime/src/rlm/repl.md)):

- **One persistent `__main__` namespace** on one asyncio loop. Cells compile with `PyCF_ALLOW_TOP_LEVEL_AWAIT`, so `await` works at top level, and tasks a cell starts keep running between cells.
- **Output is attributed to the cell that wrote it**, even from an asyncio task the cell spawned. Raw fd writes (subprocesses, C extensions) are shipped separately and never misattributed.
- **The trailing expression is the result**, Jupyter style, capped at 1 MiB of `repr`.
- **Interrupts are real**: `KeyboardInterrupt` lands in the running cell, whether it is in sync code or suspended on an `await`.
- **Pre-installed**: `requests`, `httpx`, `pandas`, `numpy`, `scipy`, `bs4`, `lxml`, `pydantic`, `pyyaml`, ... The model adds more with `uv pip install`, and runs your project through its own environment (`uv run`, `.venv/bin/python`), never by polluting the kernel.

### The whole system prompt

This is the static part of the system prompt, in full. It says what the one tool is and how to use it, once. The rest of the room is the model's.

```markdown
# Unharnessed

You have one tool, `ipython`: a persistent CPython REPL where top-level `await` works. Every other capability is a Python function in that REPL. Messages that start with `[<kind>]` come from the harness, not from the user.

## Functions in the REPL

- `bash(command: str, timeout: float | None = 600) -> BashHandle`: start a shell command in the background and return at once; each call is a new process, a command still running after `timeout` seconds is killed (`None`: no limit), and a notification arrives when a command ends after its cell
  - `BashHandle`: `.pid`, `.running`, `.command`, `.tail(n=50)`, `.output()`, `.poll() -> BashResult | None`, `.kill(sig=SIGTERM, grace=5.0)`, and `await handle -> BashResult`
  - `BashResult`: `.exit_code`, `.output`, `.duration`

## Rules

- Keep in mind that each tool call is a round trip: steps that do not need a result you have not seen yet can go in one call.
- Run programs with `bash()`, not `subprocess` or `os.system`. Write logic, loops and parsing in Python; use `bash()` only to call programs.
- `os.chdir()` and `os.environ` changes persist and apply to later `bash()` calls.
- Store what you read or compute in named variables instead of reading it again.
- Python variables survive compaction, except single values larger than 16 MiB.
- Run an external project through its own environment (`uv run`, `.venv/bin/python`, its documented commands); do not install its dependencies into the kernel.
- Do not hold the turn with `sleep` or long blocking awaits: end the turn and wait for the notification.
- When you are done, stop calling tools and give the final answer, with any assumption you made.
```

After it come only the dynamic parts: the pre-installed packages, the skills inventory, the MCP servers, the environment, and your `AGENTS.md`. `unharnessed prompt` prints the assembled prompt with its layer breakdown.

### `bash()`: async by default

In most harnesses, a command holds the model until it ends: a ten-minute build is ten minutes of waiting. Here **every command starts in the background and hands back a handle**.

```python
build = bash("cargo build --release")          # returns at once
tests = bash("pytest -x", timeout=None)        # no time limit
server = bash("npm run dev", timeout=None)     # keeps running across cells

server.tail(20)                                 # last lines, without waiting
result = await build                            # BashResult(exit_code, output, duration)
```

- Each call is a new process in its own **process group**, so `kill()` takes the whole tree down; an orphan journal reaps leftovers after a crash (a kill-on-close job object on Windows).
- A command still running after `timeout` (10 minutes by default) gets SIGTERM, then SIGKILL, and its notice says so.
- When a command outlives the cell that started it, the model gets a `[bash-done pid:… exit:…]` message when it ends. If the session is idle, that message **starts a new turn** (see [Wake-ups](#wake-ups)).
- `os.chdir()` and `os.environ` changes in Python apply to later `bash()` calls.

### Variables instead of context

With tool calls, every result is poured into the context and stays there. Here what the model reads lands in the kernel, **under a name**, and only what it prints reaches the context.

```python
log = open("server.log").read().splitlines()   # 40,000 lines, kept in the kernel
errors = [l for l in log if "ERROR" in l]
errors[-10:]                                    # the only 10 lines the model reads
```

Ten turns later, `log` is still there: no second read, no second copy in the context.

### Functions instead of tools

The model writes the helpers it needs: plain Python functions, kept as variables. It calls one again whenever it needs it, rewrites it or deletes it, like any other variable. A check across 200 files is one loop in one call, with one result in the context.

```python
async def find_flaky(paths, runs=5):
    results = {}
    for p in paths:
        outcomes = {(await bash(f"pytest -q {p}")).exit_code for _ in range(runs)}
        if len(outcomes) > 1:
            results[p] = outcomes
    return results

flaky = await find_flaky(test_files)   # 200 files, one call
flaky                                   # → 3 flaky tests
```

This is the difference with *code mode*: there, the model's script can only call the tools the harness already built. Here, there is no list to call. The model writes the code that does the work.

---

## Three ways to run

Unharnessed has three ways to drive the model. They differ on one question: **who decides that the work is done?**

| Mode | Who decides it's done | Start it with | Use it for |
|---|---|---|---|
| **Turn** | the model, once | a message | questions, focused changes, back and forth |
| **Goal** | the model, *after auditing its work* | `/goal <objective>` or `--goal` | long tasks that take many turns |
| **Autonomous** | your gates (`cargo test`, `npm run lint`, ...) | `/autonomous on` or `--autonomous` | unattended runs with a hard definition of done |

### Turn

The default. You send a message, the model calls `ipython` as many times as it needs, and the turn ends when it answers without a tool call. The hand comes back to you.

### Goal

A goal is an objective that **persists across turns**. Each time the model stops, the harness sends it a continuation: the objective, the tokens used, the budget left, and an instruction to audit the current state against every requirement before claiming anything. It keeps going until the model calls, from the kernel:

```python
await goal.complete()          # saying "done" is not enough: the goal ends only on this call
await goal.block("reason")     # only the user or an outside change can lift this
```

- **Optional token budget** (`--goal-token-budget`, or `goal.create(..., token_budget=)`): when it runs out, the goal turns `budget_limited` and the model is told to wrap up, not to declare victory.
- **Pause, resume, clear** from `/goal` or from the message box in VS Code, which shows the goal's line and its timer.
- **It can't loop forever.** A turn is *idle* when it calls no tool while no `bash()` command runs. After **3 idle turns in a row**, the harness marks the goal `blocked` with the model's last words as the reason, and the hand comes back (`-p` exits 0).

  Why: on Terminal-Bench, before this rule, a model that was truly stuck kept answering *"Operator action is required, I will not falsely mark the goal complete"* to every continuation: 440 replies and $37 on a task where the official Codex run spent $4.82.
- **Waiting is not idling.** While a `bash()` command runs, a turn without a tool call is the model waiting on it: the next continuation leaves when the last command ends, or every 10 minutes for one that never does (a server).

### Autonomous

Autonomous mode replaces the model's judgement with **yours**: after each turn, the harness runs your gate commands, and the run ends only when they all pass or a limit is hit. A failing gate's output (up to 6,000 characters) is sent back as the next continuation.

```bash
unharnessed -p --autonomous \
  --autonomous-gate "cargo test" \
  --autonomous-gate "cargo clippy -- -D warnings" \
  "Make the parser accept trailing commas"
```

| Limit | Flag | Default |
|---|---|---|
| Follow-up continuations | `--autonomous-max-continuations` | 3 |
| Assistant turns | `--autonomous-max-turns` | 12 |
| Tokens | `--autonomous-max-tokens` | 80,000 |
| Wall clock | `--autonomous-timeout-ms` | 30 min |
| Retries per failed gate | `--autonomous-gate-retries` | 3 |
| Timeout per gate | `--autonomous-gate-timeout-ms` | 5 min |

The harness snapshots the git worktree before each gate run: a gate that failed is not re-run over an unchanged workspace, so a model that changes nothing can't burn retries.

---

## Wake-ups

An idle session doesn't need you to start its next turn. Three events do it:

1. **A background `bash()` ends** after the cell that started it. The model gets `[bash-done pid:… exit:…]` and picks up from there: start a 20-minute build, end the turn, get woken when it's done.
2. **A goal's backoff ends**, or its wait on a running command does.
3. **A provider quota resets** (see [Providers and failover](#providers-and-failover)).

In VS Code and ACP, the agent starts the turn itself and streams it like any other. With `-p`, the process waits for them before exiting, at most `--background-wait-ms` per wait (10 minutes by default, `0` for no wait), then kills what is still running.

---

## Compaction

A long task outgrows the context, and the conversation has to be summarised. What lived only in the conversation is gone. **What lives in the kernel is not**: the data, the running server, the functions the model wrote.

- **Threshold**: compaction runs at a turn boundary when the context nears the model's window, minus a 16,384-token reserve. The last 20,000 tokens are kept verbatim. Both are settings (`compaction.reserveTokens`, `compaction.keepRecentTokens`).
- **Overflow recovery**: if a provider rejects a request for its size, the harness compacts and retries the turn.
- **Manual**: `/compact [instructions]`, where the instructions focus the summary.
- **The model is told what survived.** After a compaction, it gets a `[python-state]` notice listing the names still defined in the kernel:

```text
[python-state]

Your Python kernel persisted through compaction; its remaining variables, imports, and helpers are still available. These names are still defined: log, errors, find_flaky, server.
```

Any single variable above 16 MiB is dropped at compaction, and the notice names it.

---

## Sessions

- **Append-only JSONL** in `~/.unharnessed/sessions`. Resume with `-c` (last session) or `-r <id|path>`, branch with `--fork`, export with `unharnessed session export`.
- **The kernel comes back too.** When a session ends, its namespace is serialised with `dill`, one name at a time (16 MiB per variable, 256 MiB in total). On resume, a fresh kernel revives it, and the model gets a `[python-state-restored]` notice listing what came back and what didn't, before its first turn.
- **One process per session file**: a lease in `~/.unharnessed/session-leases` keeps two processes from writing the same session.

---

## Skills

A skill is a folder with a `SKILL.md` (a name, a one-line description, then instructions). Only the name and description go into the system prompt; the model reads the file when the task matches it.

**A skill with a `pyproject.toml` is a Python skill**: it's installed in the kernel venv and pre-imported, so the model calls it as a plain Python object, no tool schema involved. The three built-in skills are the kernel's API to the harness:

| Skill | In the kernel | What it does |
|---|---|---|
| `goal` | `await goal.create(...)`, `.get()`, `.complete()`, `.block(reason)` | [Goals](#goal) |
| `mcp` | `await mcp.search_tools(...)`, `.call_tool(...)`, ... | [MCP](#mcp-inside-the-kernel) |
| `attach-image` | `await attach_image(*paths)` | puts images in the model's context |

Skills are discovered from `~/.unharnessed/skills`, `<project>/.unharnessed/skills`, `.agents/skills` in the project and its parent directories, and the `skills` setting. `--skill <path>` adds one, `--no-skills` turns discovery off.

### `AGENTS.md` / `CLAUDE.md`

Both are discovered in the project and its parent directories and loaded into the system prompt. This is where what should always hold goes: your conventions, your commands, what to avoid. You write it, you can read it, you can delete it. (`--no-context-files` turns it off.)

### MCP, inside the kernel

MCP servers are **not** exposed as native tools: their tool schemas never fill the context. The model reaches them through the pre-imported `mcp` object, searches for the tool it needs, and gets results as Python values it can filter before reading.

```python
await mcp.search_tools("search documents")
await mcp.call_tool("notion", "notion-search", {"query": "roadmap"})
```

```bash
unharnessed mcp add docs --url https://example.com/mcp --oauth          # HTTP
unharnessed mcp add fs -- npx -y @modelcontextprotocol/server-filesystem .  # stdio
```

In VS Code, the settings screen also lists a catalog of services to connect in one click.

---

## Providers and failover

### Use the subscription you already pay for

Unharnessed is free. Sign in with your browser from the panel, and requests draw on your plan's limits: no API key, no second bill.

| Sign in with | Plan |
|---|---|
| **Claude** | Pro / Max |
| **ChatGPT** | Plus / Pro |
| **GitHub Copilot** | your Copilot plan |
| **Google AI** | Pro / Ultra |
| **Grok** | xAI account |

<details>
<summary><b>Or bring a key</b>: 25 providers and gateways</summary>
<br>

Anthropic · OpenAI · Google Gemini · Google Vertex AI · Amazon Bedrock · Azure OpenAI · Mistral · xAI · DeepSeek · Groq · Cerebras · Fireworks · Hugging Face · OpenRouter · Vercel AI Gateway · Cloudflare AI Gateway · Cloudflare Workers AI · Moonshot AI · Kimi For Coding · MiniMax · ZAI · Xiaomi MiMo · OpenCode Zen · OpenCode Go · Prime Inference

</details>

<details>
<summary><b>Or run it yourself</b>: local model servers</summary>
<br>

Ollama · LM Studio · llama.cpp · vLLM · any OpenAI-compatible server

</details>

The model catalog is fetched live (hourly, with a disk cache and a bundled fallback, so there is no cold start offline). A local `~/.unharnessed/models.json` takes precedence.

### When a provider fails

A long goal shouldn't die on a 529. A failed turn goes through a chain before it reaches you:

1. **Retry** the same provider: 3 times, with exponential backoff from 2 s, honouring the server's `Retry-After`.
2. **Fail over** to another configured provider serving the same model (say, Claude through your subscription, then through Bedrock): 1 s doubling backoff up to 30 s, at most 8 retries for the whole chain.
3. **Wait out a quota.** When a plan's usage limit is hit and the provider says when it resets, the session parks (up to 24 h by default, 7 days at most) and [wakes itself](#wake-ups) when it does, with a note telling the model to pick up where it stopped.

---

## What we left out

Every feature below exists in other harnesses. We left each one out on purpose.

#### No sub-agents

To hand work to a sub-agent, the main model has to write it a brief, and a brief is never the whole task. The sub-agent starts from zero, spends tokens finding out what the first model already knew, then sends back a summary. Something is lost at each hand-off. Here, the model that holds your task makes every call itself, and what it needs to know, it reads first-hand. (Parallelism doesn't need sub-agents: `bash()` and `asyncio` run things side by side.)

#### No memory

A memory is the agent keeping notes about you from one session to the next. The notes pile up and are loaded every time, so they take context whether they matter or not, and they don't expire when you change your mind: the old preference is still in there, arguing with what you ask today. Here, a new session starts from your repository and your request. What should always hold goes in [`AGENTS.md`](#agentsmd--claudemd).

#### No permission prompts

Most harnesses stop before a command and wait for you to approve it; nothing moves until you're back at the screen. Unharnessed never blocks a call, so a goal can run on its own. It's a bet, taken early: models keep getting better at judging what is safe to run, and a harness built around asking would have to be taken apart later. Today it's the riskier choice: until the bet is won, **run it in a container**.

#### A minimal system prompt

Every rule in a system prompt is a decision made for the model, and it sits in the context on every turn. [Ours](#the-whole-system-prompt) is about 1,700 characters.

---

## Surfaces

### VS Code

[**Unharnessed Agent**](https://marketplace.visualstudio.com/items?itemName=Unharnessed.unharnessed-agent) puts the agent in the secondary side bar: sign-in, model and reasoning pickers, the conversation, and a **kernel panel** that shows the running `bash()` commands (with their live tail, and a stop button) and the variables in the kernel.

- **Conversations run in the background.** Each one has its own agent process and its own kernel: switching conversations stops nothing, and several can work at once. The history menu marks the ones working and the ones that finished since you last looked.
- **Idle conversations sleep** after 90 minutes without work: the agent stops and saves its kernel's variables; opening the conversation brings them back. It never sleeps while a turn, a goal or a `bash()` command runs.
- **A context ring** next to send shows how full the window is and how much the provider's cache took, and tells when a compaction is coming.

### CLI

`unharnessed` in a terminal opens a small interactive agent: type, read the answer as it streams, and see each Python cell as a short block (its first lines of code, its last lines of output).

```bash
unharnessed                                       # interactive; /login on the first run
unharnessed "Fix the failing test"                # interactive, with a first message
unharnessed -c                                    # reopen this folder's last conversation
```

- `/login` connects a provider: a subscription (browser sign-in or device code), an API key, a local server (Ollama, LM Studio, llama.cpp, vLLM), or any OpenAI-compatible endpoint. `/logout` removes a credential.
- `/model` and `/effort` pick the model and the reasoning effort; the choice is saved as your default. `/fast` toggles fast mode where the model has it.
- `/new`, `/resume`, `/help`, `/quit`, and the agent's own `/compact`, `/goal`, `/autonomous` and `/skill:…`.
- Ctrl+P switches the model, Shift+Tab steps through the reasoning efforts, Tab on an empty input lists every command.
- Esc interrupts the agent. Typing while it works queues the next message. Alt+Enter or Ctrl+J adds a line.

It is an ACP client of the same binary, like the VS Code extension, so sessions, sign-ins and settings are shared with it.

```bash
unharnessed -p "Compute 2**100 in the kernel"     # text output
unharnessed --mode json -p "..."                  # JSON event stream
unharnessed -c -p "..."                           # continue the last session
unharnessed -p --goal "Port the test suite to pytest" --goal-token-budget 2000000
unharnessed model list                            # every model you have credentials for
unharnessed --model <id> --thinking high -p "..."
unharnessed prompt                                # the assembled system prompt
```

`--mode json` streams every event: message deltas, tool calls, `compaction_start`/`compaction_end`, `goal_update`. Built for benchmarks and scripts.

### ACP

`unharnessed --mode acp` speaks the [Agent Client Protocol](https://agentclientprotocol.com) over stdio, so any ACP client (Zed, ...) can drive it: model, reasoning-effort and fast-mode pickers as config options, follow-up queueing, and wake-up turns streamed as `session/update`. The VS Code extension is itself an ACP client.

---

## Architecture

```text
┌─────────────────────────┐   ACP (JSON-RPC, stdio)   ┌────────────────────────────────┐
│ VS Code extension       │ ────────────────────────▶ │ unharnessed  (Rust)            │
│ TypeScript + React      │                           │                                │
└─────────────────────────┘                           │ uh-cli      -p, logins, tty UI │
                                                      │ uh-daemon   ACP transport      │
              unharnessed -p / --mode json  ────────▶ │ uh-core     session engine     │
                                                      │ uh-agent    turn loop          │
                                                      │ uh-ai       providers          │
                                                      │ uh-models   model catalog      │
                                                      │ uh-types    shared types       │
                                                      └────────────────┬───────────────┘
                                                                       │  NDJSON, stdin/stdout
                                                      ┌────────────────▼───────────────┐
                                                      │ Python kernel (uv venv)        │
                                                      │ rlm.repl · bash · mcp · skills │
                                                      └────────────────────────────────┘
```

| Path | What it is |
|---|---|
| [`crates/uh-types`](crates/uh-types) | the only shared vocabulary: messages, session entries, goal state |
| [`crates/uh-ai`](crates/uh-ai) | provider clients (Anthropic, OpenAI Completions/Responses, Google, Bedrock, Mistral, Azure, ...), usage, overflow detection, a faux provider for tests |
| [`crates/uh-models`](crates/uh-models) | the live model catalog: fetch, disk cache, bundled snapshot, transport pinning |
| [`crates/uh-agent`](crates/uh-agent) | the turn loop: provider stream, tool dispatch, aborts. Tool-agnostic |
| [`crates/uh-core`](crates/uh-core) | the session engine: kernel, compaction, goals, autonomous mode, retry/failover, wakes, sessions, skills, MCP, system prompt |
| [`crates/uh-daemon`](crates/uh-daemon) | the ACP transport and the session leases (no daemon, despite the name) |
| [`crates/uh-cli`](crates/uh-cli) | the `unharnessed` binary: flags, `-p` runtime, provider sign-ins |
| [`unharnessed-runtime`](unharnessed-runtime) | the Python kernel: the REPL protocol, `bash()`, the MCP client, snapshots |
| [`skills`](skills) | the built-in skills: `goal`, `mcp`, `attach-image` |
| [`vscode`](vscode) | the extension and its React webview |

Dependencies go one way, from `uh-types` up to `uh-cli`. Each crate's README states its scope, its non-goals and its public API.

---

## Install

**VS Code**: install [Unharnessed Agent](https://marketplace.visualstudio.com/items?itemName=Unharnessed.unharnessed-agent) from the Marketplace, open the panel and sign in. The agent binary ships inside the extension. It needs [uv](https://docs.astral.sh/uv/) to build its kernel the first time:

```bash
curl -LsSf https://astral.sh/uv/install.sh | sh
```

## Build from source

Rust 1.98.1 (pinned in `rust-toolchain.toml`), [uv](https://docs.astral.sh/uv/), and Node for the extension.

```bash
cargo build --release -p uh-cli --bin unharnessed
./target/release/unharnessed --unharnessed-bootstrap     # build the kernel venv

cd vscode && npm install && npm run build                 # the extension; F5 runs it against the debug binary
npm run package                                           # a .vsix for this platform, agent included
```

Credentials come from `~/.unharnessed/auth.json` (written by the sign-ins) or the providers' usual environment variables. Settings live in `~/.unharnessed/settings.json` and `<project>/.unharnessed/settings.json`.

## Development

- `make check`: fmt, clippy (`-D warnings`), the tests and the release build. CI runs the same gates plus `cargo deny`.
- `scripts/unharnessed-smoke.sh <dir>`: drives the real binary end to end against a real model.
- `scripts/unharnessed-wakes-e2e.py <dir>`: provider retry and wake-ups, in `-p` and ACP, with the faux provider.
- `scripts/unharnessed-skill-loading.sh <dir>`: skill loading, no model needed.
- `scripts/unharnessed-terminal-e2e.py <dir>`: the interactive terminal in a pseudo-terminal (read back with pyte), with the faux provider and a fake OpenAI-compatible endpoint; no network.
- `cd vscode && npm run typecheck`: the extension's types. F5 opens an Extension Development Host with `UNHARNESSED_BIN` pointing at the debug binary.
- `cd vscode && npm run test:e2e`: drives a real VS Code through each scenario of `test/e2e/run.mjs` with the faux provider (`UNHARNESSED_E2E_ONLY=<name>` runs one).
- `UNHARNESSED_E2E_VSIX=<the .vsix> node test/e2e/run.mjs` (from `vscode/`): the same scenarios on a packaged `.vsix`, installed by VS Code.