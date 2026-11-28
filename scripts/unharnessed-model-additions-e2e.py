#!/usr/bin/env python3
"""End-to-end checks of the model additions: the sync script on scripted
sources, then the real binary's model picker (the `model` config option of
ACP `session/new`) reading the file the script wrote.

usage: scripts/unharnessed-model-additions-e2e.py <out-dir> [--live]

The sync scenarios run scripts/sync-model-additions.py on a small catalog,
a small models.dev payload, a small Codex model list and small OpenCode
model lists written under <out-dir>/sync, and cover how it can go wrong: a
model no session can use admitted (no tool calls, image output, deprecated,
no price, a live-API id, a model Codex hides from subscribers), a model the
catalog's maintainers left out admitted, a subscription model given the
API's context window, an unmapped Copilot family admitted, an unsupported
OpenCode protocol admitted without an explicit supported fallback, a model
on a transport the client does not compile admitted, an added model
disappearing while the catalog still lacks it, a model staying after the
catalog lists it or its source drops it, a rewrite without a change, and an
unreadable source emptying the file.

The picker scenarios seed an agent dir's caches and start the binary with
UNHARNESSED_OFFLINE set, so nothing reaches the network: the additions the
script wrote appear in the picker with their details and effort levels, a
catalog entry beats an addition with its id, an addition on a transport the
client does not compile is dropped, the additions extend the compiled
fallback, and an empty additions file changes nothing.

--live adds one scenario that does use the network: the binary refreshes
in a fresh agent dir, and `model list` must show what the published
additions file holds (or, while that URL answers 404, the catalog alone).

Each scenario writes <out-dir>/<name>.out: the commands with their output,
or every ACP frame sent (>) and received (<). Provider keys and tokens
from the caller's environment are dropped. UNHARNESSED_BIN picks the binary
(default target/debug/unharnessed). Exits non-zero when a check fails.
"""
import itertools
import json
import os
import queue
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BIN = os.environ.get("UNHARNESSED_BIN", str(ROOT / "target/debug/unharnessed"))
SYNC = ROOT / "scripts/sync-model-additions.py"
ARGS = [arg for arg in sys.argv[1:] if arg != "--live"]
LIVE = "--live" in sys.argv[1:]
if len(ARGS) != 1:
    sys.exit("usage: scripts/unharnessed-model-additions-e2e.py <out-dir> [--live]")
OUT = Path(ARGS[0]).resolve()
OUT.mkdir(parents=True, exist_ok=True)
ENV = {
    name: value
    for name, value in os.environ.items()
    if not name.endswith(("_API_KEY", "_TOKEN")) and not name.startswith("UNHARNESSED_")
}
ENV.update(UNHARNESSED_KERNEL_VENV=str(OUT / "kernel-venv"), DO_NOT_TRACK="1")
CATALOG = json.loads((ROOT / "crates/uh-models/tests/fixtures/catalog.v1.json").read_text())
CATALOG_URL = "https://raw.githubusercontent.com/PrimeIntellect-ai/prime-agent-catalog/main/models/catalog.v1.json"
ADDITIONS_URL = "https://raw.githubusercontent.com/unharnessed-agent/Unharnessed/main/catalog/models.additions.v1.json"
KEYS = {provider: {"type": "api_key", "key": "sk-e2e-test"} for provider in (
    "anthropic", "google", "openai", "github-copilot", "opencode", "opencode-go")}
# A ChatGPT subscription: a token that is not due for a refresh.
KEYS["openai-codex"] = {"type": "oauth", "access": "e2e-access", "refresh": "e2e-refresh",
                        "accountId": "e2e-account", "expires": int((time.time() + 30 * 86400) * 1000)}
TRANSPORTS = {
    "anthropic": ("anthropic-messages", "https://api.anthropic.com"),
    "google": ("google-generative-ai", "https://generativelanguage.googleapis.com/v1beta"),
    "openai": ("openai-responses", "https://api.openai.com/v1"),
    "openai-codex": ("openai-codex-responses", "https://chatgpt.com/backend-api"),
    "github-copilot": ("openai-responses", "https://api.individual.githubcopilot.com"),
    "opencode": ("openai-responses", "https://opencode.ai/zen/v1"),
    "opencode-go": ("openai-completions", "https://opencode.ai/zen/go/v1"),
}
FAILED = []


def check(name, ok, detail=""):
    print(("PASS  " if ok else "FAIL  ") + name + ("" if ok or not detail else f" ({detail})"))
    if not ok:
        FAILED.append(name)


# ---- the sync script on scripted sources ----------------------------------

def described(release, **changes):
    """One models.dev model a session can use, released on `release`."""
    model = {
        "name": "Described", "tool_call": True, "reasoning": True, "release_date": release,
        "reasoning_options": [{"type": "effort", "values": ["low", "medium", "high"]}],
        "modalities": {"input": ["text", "image"], "output": ["text"]},
        "cost": {"input": 1, "output": 2, "cache_read": 0.1},
        "limit": {"context": 200000, "output": 8000},
    }
    model.update(changes)
    return {key: value for key, value in model.items() if value is not None}


def listed(provider, model_id):
    """One catalog entry."""
    api, base_url = TRANSPORTS[provider]
    return {"id": model_id, "name": model_id, "api": api, "provider": provider, "baseUrl": base_url,
            "reasoning": False, "input": ["text"],
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
            "contextWindow": 128000, "maxTokens": 4096}


def shown(context_window, **changes):
    """One model of the Codex list, shown to subscribers."""
    model = {"display_name": "Shown-By-Codex", "visibility": "list", "context_window": context_window,
             "input_modalities": ["text", "image"],
             "supported_reasoning_levels": [{"effort": level} for level in ("low", "medium", "high", "ultra")]}
    model.update(changes)
    return model


def served(model_id, recommended_api, **changes):
    """One current OpenCode endpoint chat model."""
    model = {
        "id": model_id, "name": model_id, "type": "chat", "recommended_api": recommended_api,
        "api": {recommended_api: {"base_url": "https://opencode.example/inference"}},
        "capabilities": {"tools": {"supported": True}, "reasoning": {"supported": True, "effort": ["low", "high"]}},
        "modalities": {"input": ["text", "image/png"], "output": ["text"]},
        "cost": {"text": {"input": {"price": 1, "cache": {"read": 0.1, "write": {"30m": 2, "12h": 4}}},
                          "output": {"price": 3}}},
        "limit": {"context": 100000, "output": 9000},
    }
    model.update(changes)
    return model


def sources():
    """A catalog, and the models.dev payload and Codex list that go with
    it. The catalog lists, per API provider, models released up to
    2026-03-01."""
    catalog = {"schemaVersion": 1, "models": [
        listed("anthropic", "claude-old"), listed("anthropic", "claude-mid"),
        listed("google", "gemini-a"), listed("openai", "gpt-a"), listed("openai-codex", "gpt-a"),
        listed("github-copilot", "copilot-old"), listed("opencode", "zen-old"), listed("opencode-go", "go-old")]}
    max_effort = [{"type": "effort", "values": ["low", "medium", "high", "xhigh", "max"]}]
    models_dev = {
        "anthropic": {"models": {
            "claude-old": described("2026-01-10"),
            "claude-mid": described("2026-03-01"),
            "claude-new": described("2026-04-01", name="Claude New"),
            "claude-fable-5-9": described("2026-05-02", name="Claude Fable 5.9", reasoning_options=max_effort),
            "claude-left-out": described("2026-02-01"),
            "claude-embed": described("2026-05-01", tool_call=False),
            "claude-deprecated": described("2026-05-01", status="deprecated"),
            "claude-no-price": described("2026-05-01", cost=None),
            "claude-no-limit": described("2026-05-01", limit={"context": 200000}),
            "claude-no-date": described(None),
            "claude-bad-date": described("soon"),
        }},
        "google": {"models": {
            "gemini-a": described("2026-02-01"),
            "gemini-new": described("2026-05-01", name="Gemini New"),
            "gemini-9-live-preview": described("2026-05-01"),
            "gemini-audio": described("2026-05-01", modalities={"input": ["text"], "output": ["text", "audio"]}),
        }},
        "openai": {"models": {
            "gpt-a": described("2026-02-01"),
            "gpt-new": described("2026-05-01", name="GPT New", modalities={"input": ["text"], "output": ["text"]}),
            "gpt-image-x": described("2026-05-01", modalities={"input": ["text"], "output": ["image"]}),
        }},
        "github-copilot": {"models": {
            "copilot-old": described("2026-03-01"),
            "copilot-claude-new": described("2026-05-01", family="claude-fable", canonical_model_id="anthropic/claude-fable-5-2"),
            "copilot-gemini-new": described("2026-05-01", family="gemini", canonical_model_id="google/gemini-4-neon"),
            "copilot-gpt-new": described("2026-05-01", family="gpt-sol", canonical_model_id="openai/gpt-6.2-sol"),
            "copilot-mai-new": described("2026-05-01", family="mai-code", canonical_model_id="microsoft/mai-code-2"),
            "copilot-unknown-new": described("2026-05-01"),
        }},
        "opencode": {"models": {}},
        "opencode-go": {"models": {}},
        "some-other-provider": {"models": {"other-new": described("2026-05-01")}},
    }
    # gpt-new has an API twin on models.dev (a 200000 context there),
    # gpt-subscription-only has none.
    codex = {"models": [
        dict(shown(272000), slug="gpt-a"),
        dict(shown(64000), slug="gpt-new"),
        dict(shown(96000, display_name="GPT Subscription Only", input_modalities=["text"]),
             slug="gpt-subscription-only"),
        dict(shown(272000, visibility="hide"), slug="gpt-hidden"),
        dict(shown(None), slug="gpt-no-context"),
    ]}
    opencode = {"data": [
        served("zen-message", "messages"),
        served("zen-google", "generate-content"),
        served("zen-interactions", "interactions", api={
            "interactions": {"base_url": "https://opencode.example/interactions"},
            "generate-content": {"base_url": "https://opencode.example/google"},
        }),
        served("zen-response", "responses"),
        served("zen-completion", "chat-completions"),
        served("zen-no-tools", "responses", capabilities={"tools": {"supported": False}}),
        served("zen-retired-candidate", "embeddings"),
    ]}
    opencode_go = {"data": [
        served("go-message", "messages"),
        served("go-google", "generate-content"),
        served("go-response", "responses"),
        served("go-completion", "chat-completions"),
    ]}
    return catalog, models_dev, codex, opencode, opencode_go


class Sync:
    """Runs of the sync script in one directory, logged to <name>.out."""

    def __init__(self, name):
        self.dir = OUT / "sync" / name
        self.dir.mkdir(parents=True, exist_ok=True)
        self.file = self.dir / "models.additions.v1.json"
        self.file.unlink(missing_ok=True)
        self.log = open(OUT / f"sync-{name}.out", "w")

    def run(self, catalog, models_dev, codex, opencode, opencode_go):
        """Run the script on these sources (a str is written as is)."""
        files = {"--catalog": catalog, "--models-dev": models_dev, "--codex-models": codex,
                 "--opencode-models": opencode, "--opencode-go-models": opencode_go}
        command = [sys.executable, str(SYNC), "--out", str(self.file)]
        for flag, source in files.items():
            path = self.dir / f"{flag[2:]}.json"
            path.write_text(source if isinstance(source, str) else json.dumps(source, indent=1))
            command += [flag, str(path)]
        done = subprocess.run(command, capture_output=True, text=True, env=ENV)
        self.log.write(f"$ {' '.join(command)}\n{done.stdout}{done.stderr}exit {done.returncode}\n\n")
        self.log.flush()
        return done

    def ids(self):
        return [f"{m['provider']}/{m['id']}" for m in json.loads(self.file.read_text())["models"]]

    def entry(self, provider, model_id):
        models = json.loads(self.file.read_text())["models"]
        return next((m for m in models if (m["provider"], m["id"]) == (provider, model_id)), None)


ADMITTED = ["anthropic/claude-fable-5-9", "anthropic/claude-new",
            "github-copilot/copilot-claude-new", "github-copilot/copilot-gemini-new",
            "github-copilot/copilot-gpt-new", "github-copilot/copilot-mai-new",
            "google/gemini-new", "openai/gpt-new",
            "openai-codex/gpt-new", "openai-codex/gpt-subscription-only",
            "opencode/zen-completion", "opencode/zen-google", "opencode/zen-interactions", "opencode/zen-message", "opencode/zen-response",
            "opencode-go/go-completion", "opencode-go/go-message", "opencode-go/go-response"]


def scenario_sync_admission():
    catalog, models_dev, codex, opencode, opencode_go = sources()
    sync = Sync("admission")
    done = sync.run(catalog, models_dev, codex, opencode, opencode_go)
    check("admission: the script exits 0", done.returncode == 0, done.stderr)
    check("admission: only the usable API models released after the catalog's newest, "
          "and the subscription models Codex shows, are added",
          sync.ids() == ADMITTED, sync.ids())
    new = sync.entry("anthropic", "claude-new") or {}
    check("admission: an entry has the provider's transport, the name, prices and limits",
          {k: v for k, v in new.items() if k != "thinkingLevelMap"} == {
              "id": "claude-new", "name": "Claude New", "api": "anthropic-messages", "provider": "anthropic",
              "baseUrl": "https://api.anthropic.com", "reasoning": True, "input": ["text", "image"],
              "cost": {"input": 1, "output": 2, "cacheRead": 0.1, "cacheWrite": 0},
              "contextWindow": 200000, "maxTokens": 8000}, new)
    check("admission: its effort levels are the ones models.dev lists, off left to the provider",
          new.get("thinkingLevelMap") == {"minimal": None, "low": "low", "medium": "medium", "high": "high",
                                          "xhigh": None, "max": None}, new.get("thinkingLevelMap"))
    fable = sync.entry("anthropic", "claude-fable-5-9") or {}
    check("admission: a Fable 5 model cannot turn thinking off and goes up to max",
          fable.get("thinkingLevelMap") == {"off": None, "minimal": None, "low": "low", "medium": "medium",
                                            "high": "high", "xhigh": "xhigh", "max": "max"},
          fable.get("thinkingLevelMap"))
    gpt = sync.entry("openai", "gpt-new") or {}
    check("admission: a text-only model takes text only", gpt.get("input") == ["text"], gpt.get("input"))
    twin = sync.entry("openai-codex", "gpt-new") or {}
    check("admission: a subscription model has the subscription's transport and context window, "
          "and its API twin's name and prices",
          {k: v for k, v in twin.items() if k != "thinkingLevelMap"} == {
              "id": "gpt-new", "name": "GPT New", "api": "openai-codex-responses", "provider": "openai-codex",
              "baseUrl": "https://chatgpt.com/backend-api", "reasoning": True, "input": ["text", "image"],
              "cost": {"input": 1, "output": 2, "cacheRead": 0.1, "cacheWrite": 0},
              "contextWindow": 64000, "maxTokens": 8000}, twin)
    check("admission: its effort levels are the ones Codex lists, without the level the client does not "
          "have, and no off since its API twin has none",
          twin.get("thinkingLevelMap") == {"off": None, "minimal": None, "low": "low", "medium": "medium",
                                           "high": "high", "xhigh": None, "max": None},
          twin.get("thinkingLevelMap"))
    alone = sync.entry("openai-codex", "gpt-subscription-only") or {}
    check("admission: a subscription model without an API twin has Codex's name, no price, the default "
          "output limit, and off left to the provider",
          (alone.get("name"), alone.get("cost"), alone.get("contextWindow"), alone.get("maxTokens"),
           alone.get("input"), "off" in alone.get("thinkingLevelMap", {"off": 0}))
          == ("GPT Subscription Only", {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, 96000, 128000,
              ["text"], False), alone)

    copilot = {m["id"]: m for m in json.loads(sync.file.read_text())["models"] if m["provider"] == "github-copilot"}
    check("admission: Copilot families select the audited wire API",
          copilot["copilot-claude-new"]["api"] == "anthropic-messages"
          and copilot["copilot-gemini-new"]["api"] == "openai-completions"
          and copilot["copilot-gpt-new"]["api"] == "openai-responses"
          and copilot["copilot-mai-new"]["api"] == "openai-responses", copilot)
    check("admission: an unmapped Copilot family fails closed",
          sync.entry("github-copilot", "copilot-unknown-new") is None)
    zen_google = sync.entry("opencode", "zen-google") or {}
    check("admission: an OpenCode Google model pins on the compiled Zen transport",
          (zen_google.get("api"), zen_google.get("baseUrl")) == ("google-generative-ai", "https://opencode.ai/zen/v1"),
          zen_google)
    zen_interactions = sync.entry("opencode", "zen-interactions") or {}
    check("admission: a newer endpoint protocol falls back only to an explicitly offered supported API",
          (zen_interactions.get("api"), zen_interactions.get("baseUrl")) == ("google-generative-ai", "https://opencode.ai/zen/v1"),
          zen_interactions)
    zen_completion = sync.entry("opencode", "zen-completion") or {}
    check("admission: endpoint costs flatten with the lowest cache-write price",
          zen_completion.get("cost") == {"input": 1, "output": 3, "cacheRead": 0.1, "cacheWrite": 2},
          zen_completion.get("cost"))
    check("admission: a model without tool calls is not added", sync.entry("opencode", "zen-no-tools") is None)
    check("admission: an unsupported endpoint API is not added", sync.entry("opencode", "zen-retired-candidate") is None)
    check("admission: a transport without a compiled tuple fails closed",
          sync.entry("opencode-go", "go-google") is None)

    before = sync.file.read_bytes()
    done = sync.run(catalog, models_dev, codex, opencode, opencode_go)
    check("admission: a second run leaves the file as it is",
          sync.file.read_bytes() == before and "unchanged" in done.stdout, done.stdout)


def scenario_sync_lifecycle():
    catalog, models_dev, codex, opencode, opencode_go = sources()
    sync = Sync("lifecycle")
    sync.run(catalog, models_dev, codex, opencode, opencode_go)

    # The catalog gains a model newer than claude-new, without listing it.
    catalog["models"].append(listed("anthropic", "claude-newer"))
    models_dev["anthropic"]["models"]["claude-newer"] = described("2026-06-01")
    done = sync.run(catalog, models_dev, codex, opencode, opencode_go)
    check("lifecycle: an added model stays when the catalog lists a newer one",
          sync.ids() == ADMITTED and "kept    anthropic/claude-new" in done.stdout, sync.ids())
    fresh = Sync("lifecycle-fresh")
    fresh.run(catalog, models_dev, codex, opencode, opencode_go)
    check("lifecycle: the same sources without that history do not add it",
          fresh.ids() == [i for i in ADMITTED if not i.startswith("anthropic/")], fresh.ids())

    catalog["models"] += [listed("anthropic", "claude-new"), listed("openai-codex", "gpt-new")]
    done = sync.run(catalog, models_dev, codex, opencode, opencode_go)
    left = [i for i in ADMITTED if i not in ("anthropic/claude-new", "openai-codex/gpt-new")]
    check("lifecycle: a model the catalog now lists leaves the file",
          sync.ids() == left
          and "removed anthropic/claude-new: the catalog lists it now" in done.stdout
          and "removed openai-codex/gpt-new: the catalog lists it now" in done.stdout, done.stdout)

    models_dev["openai"]["models"]["gpt-new"]["status"] = "deprecated"
    del models_dev["google"]["models"]["gemini-new"]
    next(m for m in codex["models"] if m["slug"] == "gpt-subscription-only")["visibility"] = "hide"
    done = sync.run(catalog, models_dev, codex, opencode, opencode_go)
    remaining = [i for i in ADMITTED if i not in (
        "anthropic/claude-new", "google/gemini-new", "openai/gpt-new",
        "openai-codex/gpt-new", "openai-codex/gpt-subscription-only")]
    check("lifecycle: a model its source deprecates, drops or hides leaves the file",
          sync.ids() == remaining
          and "removed openai/gpt-new: deprecated" in done.stdout
          and "removed google/gemini-new: its source no longer lists it" in done.stdout
          and "removed openai-codex/gpt-subscription-only: not shown to subscribers" in done.stdout, done.stdout)

    for pair in sync.ids():
        provider, model_id = pair.split("/", 1)
        catalog["models"].append(listed(provider, model_id))
    done = sync.run(catalog, models_dev, codex, opencode, opencode_go)
    check("lifecycle: with nothing left to add the file holds an empty list",
          done.returncode == 0 and json.loads(sync.file.read_text()) == {"schemaVersion": 1, "models": []},
          sync.file.read_text())


def scenario_sync_failures():
    catalog, models_dev, codex, opencode, opencode_go = sources()
    sync = Sync("failures")
    sync.run(catalog, models_dev, codex, opencode, opencode_go)
    before = sync.file.read_bytes()

    def refused(name, bad_catalog, bad_models_dev, bad_codex=codex, bad_opencode=opencode, bad_opencode_go=opencode_go):
        done = sync.run(bad_catalog, bad_models_dev, bad_codex, bad_opencode, bad_opencode_go)
        check(f"failures: {name} exits 1 and leaves the file as it is",
              done.returncode == 1 and done.stderr.startswith("error: ") and sync.file.read_bytes() == before,
              f"exit {done.returncode}: {done.stderr.strip()}")

    refused("a models.dev payload that is not JSON", catalog, "<html>502 Bad Gateway</html>")
    refused("a models.dev payload without one of the providers", catalog,
            {k: v for k, v in models_dev.items() if k != "openai"})
    refused("a models.dev payload with no model of a provider", catalog,
            dict(models_dev, google={"models": {}}))
    refused("a models.dev payload that dates none of a provider's catalog models", catalog,
            dict(models_dev, google={"models": {"gemini-new": described("2026-05-01")}}))
    refused("a catalog of another schema version", dict(catalog, schemaVersion=2), models_dev)
    refused("a catalog without models", {"schemaVersion": 1, "models": []}, models_dev)
    refused("a catalog without any model of the subscription",
            dict(catalog, models=[m for m in catalog["models"] if m["provider"] != "openai-codex"]), models_dev)
    refused("a Codex list that is not JSON", catalog, models_dev, "404: Not Found")
    refused("a Codex list of another shape", catalog, models_dev, {"models": [{"name": "gpt-new"}]})
    refused("an OpenCode list that is not JSON", catalog, models_dev, codex, "502 Bad Gateway")
    refused("an OpenCode Go list without a supported chat model", catalog, models_dev, codex, opencode, {"data": []})


# ---- the picker of the real binary -----------------------------------------

def value(provider, model_id):
    return json.dumps([provider, model_id], separators=(",", ":"))


def option(options, option_id):
    return next((o for o in options if o["id"] == option_id), None)


def cache(url, payload):
    return {"url": url, "scope": "public", "fetchedAt": int(time.time() * 1000), "payload": payload}


class Agent:
    """One `unharnessed --mode acp` process in a throwaway agent dir whose
    catalog and additions caches are the given payloads (None: no cache)."""

    def __init__(self, name, catalog, additions):
        self.agent_dir = OUT / name / "agent"
        work = OUT / name / "work"
        models = self.agent_dir / "models"
        for directory in (models, work):
            directory.mkdir(parents=True, exist_ok=True)
        (self.agent_dir / "auth.json").write_text(json.dumps(KEYS, indent=2))
        for file, url, payload in (("provider-model-catalog.v1.json", CATALOG_URL, catalog),
                                   ("model-additions-cache.json", ADDITIONS_URL, additions)):
            (models / file).unlink(missing_ok=True)
            if payload is not None:
                (models / file).write_text(json.dumps(cache(url, payload)))
        extra = {"UNHARNESSED_CODING_AGENT_DIR": str(self.agent_dir), "UNHARNESSED_OFFLINE": "1"}
        self.out = open(OUT / f"{name}.out", "w")
        self.out.write(f"$ {' '.join(f'{k}={v}' for k, v in extra.items())} unharnessed --mode acp\n")
        self.child = subprocess.Popen(
            [BIN, "--mode", "acp"], cwd=work, text=True, env=dict(ENV, **extra),
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=open(OUT / name / "stderr.log", "w"))
        self.work = work
        self.start = time.time()
        self.inbox = queue.Queue()
        self.ids = itertools.count(1)
        threading.Thread(target=self._read, daemon=True).start()
        self.request("initialize", {"protocolVersion": 1, "clientCapabilities": {}})
        answer = self.request("session/new", {"cwd": str(work), "mcpServers": []})
        self.session_id = answer["result"]["sessionId"]
        self.options = answer["result"]["configOptions"]

    def _log(self, arrow, frame):
        self.out.write(f"{time.time() - self.start:6.2f} {arrow} {json.dumps(frame)}\n")
        self.out.flush()

    def _read(self):
        for line in self.child.stdout:
            if line.strip():
                frame = json.loads(line)
                self._log("<", frame)
                self.inbox.put(frame)
        self.inbox.put(None)

    def request(self, method, params, timeout=60):
        request_id = next(self.ids)
        frame = {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}
        self._log(">", frame)
        self.child.stdin.write(json.dumps(frame) + "\n")
        self.child.stdin.flush()
        deadline = time.time() + timeout
        while True:
            answer = self.inbox.get(timeout=max(0.05, deadline - time.time()))
            if answer is None:
                raise EOFError("the agent closed stdout")
            if answer.get("id") == request_id and "method" not in answer:
                return answer

    def choices(self):
        """The picker's models: `{value: details}`."""
        return {c["value"]: c.get("_meta", {}).get("unharnessed", {}).get("model")
                for c in option(self.options, "model")["options"]}

    def select(self, provider, model_id):
        params = {"sessionId": self.session_id, "configId": "model", "value": value(provider, model_id)}
        return self.request("session/set_config_option", params)

    def close(self):
        self.child.stdin.close()
        code = self.child.wait(timeout=30)
        self.out.close()
        return code


def scripted_additions():
    """The file the sync script writes for `sources()`."""
    sync = Sync("for-the-picker")
    sync.run(*sources())
    return json.loads(sync.file.read_text())


def scenario_picker():
    additions = scripted_additions()
    baseline = Agent("picker-baseline", CATALOG, None)
    before = baseline.choices()
    baseline.close()
    agent = Agent("picker", CATALOG, additions)
    after = agent.choices()
    added = {value(m["provider"], m["id"]): m for m in additions["models"]}
    check("picker: without an additions file no scripted model is offered", not set(added) & set(before))
    check("picker: the picker gains exactly the additions",
          set(after) - set(before) == set(added) and set(before) <= set(after),
          sorted(set(after) ^ (set(before) | set(added))))
    codex = after.get(value("openai-codex", "gpt-new")) or {}
    check("picker: a ChatGPT subscriber is offered the added model with the subscription's context window",
          codex.get("contextWindow") == 64000, codex)
    wrong = [(v, after.get(v)) for v, m in added.items() if after.get(v) != {
        "id": m["id"], "name": m["name"], "provider": m["provider"],
        "contextWindow": m["contextWindow"], "input": m["input"], "cost": m["cost"]}]
    check("picker: each addition carries its name, context window, input and cost", not wrong, wrong[:1])
    answer = agent.select("anthropic", "claude-fable-5-9")
    options = answer.get("result", {}).get("configOptions", [])
    levels = [o["value"] for o in (option(options, "thought_level") or {}).get("options", [])]
    check("picker: an added model can be selected",
          (option(options, "model") or {}).get("currentValue") == value("anthropic", "claude-fable-5-9"), answer)
    check("picker: its effort picker offers the levels of the additions file",
          levels == ["low", "medium", "high", "xhigh", "max"], levels)
    check("picker: the agent exits when stdin closes", agent.close() == 0)


def scenario_picker_precedence():
    known = next(m for m in CATALOG["models"] if m["provider"] == "anthropic")
    clash = dict(listed("anthropic", known["id"]), name="An addition must not replace this", contextWindow=1234)
    foreign = dict(listed("anthropic", "claude-elsewhere"), baseUrl="https://elsewhere.example")
    headers = dict(listed("anthropic", "claude-with-headers"), headers={"x-extra": "1"})
    valid = listed("anthropic", "claude-valid-addition")
    agent = Agent("picker-precedence", CATALOG,
                  {"schemaVersion": 1, "models": [clash, foreign, headers, valid]})
    choices = agent.choices()
    shown = choices.get(value("anthropic", known["id"])) or {}
    check("precedence: a catalog model keeps the catalog's details",
          (shown.get("name"), shown.get("contextWindow")) == (known["name"], known["contextWindow"]), shown)
    check("precedence: an addition on another base URL is not offered",
          value("anthropic", "claude-elsewhere") not in choices)
    check("precedence: an addition carrying request headers is not offered",
          value("anthropic", "claude-with-headers") not in choices)
    check("precedence: the valid addition beside them is offered",
          value("anthropic", "claude-valid-addition") in choices)
    agent.close()


def scenario_picker_fallback_and_empty():
    additions = scripted_additions()
    bare = Agent("picker-no-catalog-baseline", None, None)
    before = bare.choices()
    bare.close()
    agent = Agent("picker-no-catalog", None, additions)
    after = agent.choices()
    agent.close()
    added = {value(m["provider"], m["id"]) for m in additions["models"]}
    check("fallback: with no catalog cache the compiled models are offered", len(before) > 0, len(before))
    check("fallback: the additions extend the compiled models", set(after) - set(before) == added,
          sorted(set(after) ^ (set(before) | added)))

    baseline = Agent("picker-empty-baseline", CATALOG, None)
    before = baseline.choices()
    baseline.close()
    agent = Agent("picker-empty", CATALOG, {"schemaVersion": 1, "models": []})
    check("empty: an empty additions file leaves the picker as the catalog makes it", agent.choices() == before)
    agent.close()


# ---- the published file (network) ------------------------------------------

def scenario_live():
    agent_dir = OUT / "live" / "agent"
    agent_dir.mkdir(parents=True, exist_ok=True)
    for stale in (agent_dir / "models").glob("*.json") if (agent_dir / "models").exists() else []:
        stale.unlink()
    (agent_dir / "auth.json").write_text(json.dumps(KEYS, indent=2))
    try:
        with urllib.request.urlopen(ADDITIONS_URL, timeout=30) as response:
            published = json.loads(response.read())
    except urllib.error.HTTPError as error:
        published = error.code
    command = [BIN, "model", "list"]
    done = subprocess.run(command, capture_output=True, text=True, timeout=120,
                          env=dict(ENV, UNHARNESSED_CODING_AGENT_DIR=str(agent_dir)))
    rows = {tuple(line.split()[:2]) for line in done.stdout.splitlines()}
    cached = agent_dir / "models" / "model-additions-cache.json"
    with open(OUT / "live.out", "w") as log:
        log.write(f"GET {ADDITIONS_URL}\n{json.dumps(published, indent=1)}\n\n"
                  f"$ UNHARNESSED_CODING_AGENT_DIR={agent_dir} unharnessed model list\n"
                  f"{done.stdout}{done.stderr}exit {done.returncode}\n\n"
                  f"{cached.name}: {'written' if cached.exists() else 'absent'}\n")
    check("live: `model list` exits 0 and lists the catalog's Anthropic models",
          done.returncode == 0 and ("anthropic", "claude-fable-5") in rows, done.stderr[-300:])
    if isinstance(published, int):
        print(f"NOTE  live: {ADDITIONS_URL} answers {published}: the additions file is not published yet")
        check("live: with no published file no additions cache is written", not cached.exists())
        return
    wanted = {(m["provider"], m["id"]) for m in published["models"] if m["provider"] in KEYS}
    print(f"NOTE  live: the published file holds {sorted('/'.join(pair) for pair in wanted)}")
    check("live: the additions cache is written", cached.exists())
    check("live: `model list` shows every published addition", wanted <= rows, sorted(wanted - rows))


scenario_sync_admission()
scenario_sync_lifecycle()
scenario_sync_failures()
scenario_picker()
scenario_picker_precedence()
scenario_picker_fallback_and_empty()
if LIVE:
    scenario_live()
print(f"\n{len(FAILED)} failed" if FAILED else "\nall checks passed")
sys.exit(1 if FAILED else 0)
