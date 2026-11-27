#!/usr/bin/env python3
"""End-to-end checks of the live provider model lists on the real binary,
signed in the way a user signs in: `/login` with an API key (the ACP
`_unharnessed/providers/set_api_key` call the terminal UI and VS Code make).

After the login, the model picker lists the models the provider's own
`/models` lists and no catalog does (Kimi K3.1 Pro on the Kimi Code
subscription), one of them answers, the list is cached per key (the key
never written to the cache), served offline by the next process, gone at
logout, and cleared when the provider refuses the key.

usage: KIMI_API_KEY=sk-kimi-... scripts/unharnessed-provider-models-e2e.py <out-dir>

This runs against the live api.kimi.com: the key must be a Kimi Code
subscription key. The script hands it to the agent through the login call
only; the agent processes never see it in their environment. Each scenario
runs in its own agent dir (<out-dir>/<name>/agent) and writes
<out-dir>/<name>.out: the command, then every ACP frame sent (>) and
received (<) with the seconds since start, or the CLI command and its
output. The key is replaced by <KIMI_API_KEY> in every trace. Provider keys
and tokens from the caller's environment are dropped. UNHARNESSED_BIN picks
the binary (default target/debug/unharnessed). Exits non-zero when a check
fails.
"""
import hashlib
import hmac
import itertools
import json
import os
import queue
import shutil
import stat
import subprocess
import sys
import threading
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BIN = os.environ.get("UNHARNESSED_BIN", str(ROOT / "target/debug/unharnessed"))
KEY = os.environ.get("KIMI_API_KEY", "")
if len(sys.argv) != 2 or not KEY:
    sys.exit(__doc__)
OUT = Path(sys.argv[1]).resolve()
OUT.mkdir(parents=True, exist_ok=True)
ENV = {
    name: value
    for name, value in os.environ.items()
    if not name.endswith(("_API_KEY", "_TOKEN")) and not name.startswith("UNHARNESSED_")
}
ENV.update(UNHARNESSED_KERNEL_VENV=str(OUT / "kernel-venv"), DO_NOT_TRACK="1")
PROVIDER = "kimi-coding"
LIST_URL = "https://api.kimi.com/coding/v1/models?limit=1000"
CACHE = Path("models/provider-models") / f"{PROVIDER}.json"
SET_API_KEY = "_unharnessed/providers/set_api_key"
LOGOUT = "_unharnessed/providers/logout"
# Until the request layer sends `content-type: application/json` by itself
# (PR #4), api.kimi.com refuses every chat request without it.
CONTENT_TYPE = {"providers": {PROVIDER: {"headers": {"content-type": "application/json"}}}}
FAILED = []


def check(name, ok, detail=""):
    print(("PASS  " if ok else "FAIL  ") + name + ("" if ok or not detail else f" ({detail})"))
    if not ok:
        FAILED.append(name)


def hide(text):
    return text.replace(KEY, "<KIMI_API_KEY>")


def scope(key):
    message = b"unharnessed:provider-model-list:v1\0" + PROVIDER.encode()
    return hmac.new(key.encode(), message, hashlib.sha256).hexdigest()


def value(model_id):
    return json.dumps([PROVIDER, model_id], separators=(",", ":"))


def option(options, option_id):
    return next((o for o in options if o["id"] == option_id), None)


def choices(options):
    found = option(options, "model")
    return {c["value"] for c in found["options"]} if found else set()


class Agent:
    """One `unharnessed --mode acp` process in a throwaway agent dir."""

    def __init__(self, name, agent_dir=None, offline=False, run=1):
        self.agent_dir = agent_dir or OUT / name / "agent"
        work = OUT / name / "work"
        self.agent_dir.mkdir(parents=True, exist_ok=True)
        work.mkdir(parents=True, exist_ok=True)
        extra = {"UNHARNESSED_CODING_AGENT_DIR": str(self.agent_dir)}
        if offline:
            extra["UNHARNESSED_OFFLINE"] = "1"
        suffix = "" if run == 1 else f"-{run}"
        self.out = open(OUT / f"{name}{suffix}.out", "w")
        command = " ".join(f"{key}={value}" for key, value in extra.items())
        self.out.write(f"$ {command} unharnessed --mode acp\n")
        self.child = subprocess.Popen(
            [BIN, "--mode", "acp"], cwd=work, text=True, env=dict(ENV, **extra),
            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=open(OUT / name / f"stderr{suffix}.log", "w"))
        self.work = work
        self.start = time.time()
        self.inbox = queue.Queue()
        self.answers = {}
        self.config_updates = []
        self.text = []
        self.ids = itertools.count(1)
        threading.Thread(target=self._read, daemon=True).start()
        self.request("initialize", {"protocolVersion": 1, "clientCapabilities": {}})
        self.session_id = None

    def _log(self, arrow, frame):
        self.out.write(f"{time.time() - self.start:6.2f} {arrow} {hide(json.dumps(frame))}\n")
        self.out.flush()

    def _read(self):
        for line in self.child.stdout:
            if line.strip():
                frame = json.loads(line)
                self._log("<", frame)
                self.inbox.put(frame)
        self.inbox.put(None)

    def _pump(self, deadline):
        frame = self.inbox.get(timeout=max(0.05, deadline - time.time()))
        if frame is None:
            raise EOFError("the agent closed stdout")
        if "id" in frame and "method" not in frame:
            self.answers[frame["id"]] = frame
        elif frame.get("method") == "session/update":
            update = frame["params"]["update"]
            if update.get("sessionUpdate") == "config_option_update":
                self.config_updates.append(update["configOptions"])
            elif update.get("sessionUpdate") == "agent_message_chunk":
                self.text.append(update.get("content", {}).get("text", ""))

    def request(self, method, params, timeout=30):
        request_id = next(self.ids)
        frame = {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}
        self._log(">", frame)
        self.child.stdin.write(json.dumps(frame) + "\n")
        self.child.stdin.flush()
        deadline = time.time() + timeout
        while request_id not in self.answers:
            self._pump(deadline)
        return self.answers[request_id]

    def new_session(self):
        answer = self.request("session/new", {"cwd": str(self.work), "mcpServers": []}, timeout=120)
        self.session_id = answer["result"]["sessionId"]
        return answer["result"]["configOptions"]

    def config_update(self, matches, since, timeout=60):
        """The first config update from the `since`-th on that `matches`."""
        deadline = time.time() + timeout
        seen = since
        while True:
            for options in self.config_updates[seen:]:
                if matches(options):
                    return options
            seen = len(self.config_updates)
            try:
                self._pump(deadline)
            except queue.Empty:
                return None

    def close(self):
        self.child.stdin.close()
        code = self.child.wait(timeout=30)
        self.out.close()
        return code


def cli(name, args, agent_dir):
    env = dict(ENV, UNHARNESSED_CODING_AGENT_DIR=str(agent_dir))
    result = subprocess.run([BIN, *args], env=env, capture_output=True, text=True, timeout=300)
    output = hide(result.stdout + result.stderr)
    with open(OUT / f"{name}.out", "a") as out:
        out.write(f"$ UNHARNESSED_CODING_AGENT_DIR={agent_dir} unharnessed {' '.join(args)}\n{output}(exit {result.returncode})\n\n")
    return {line.split()[1] for line in output.splitlines() if line.startswith(PROVIDER + " ")}


# What the subscription lists, read here with the same key.
request = urllib.request.Request(LIST_URL, headers={"x-api-key": KEY, "anthropic-version": "2023-06-01"})
with urllib.request.urlopen(request, timeout=30) as response:
    LISTED = {model["id"] for model in json.load(response)["data"]}

# 1. /login: the picker gains every model the subscription lists.
agent = Agent("login")
(agent.agent_dir / "models.json").write_text(json.dumps(CONTENT_TYPE))
before_login = choices(agent.new_session())
since = len(agent.config_updates)
answer = agent.request(SET_API_KEY, {"providerId": PROVIDER, "apiKey": KEY})
check("login: the API key is saved", answer.get("result", {}).get("outcome") == "done", hide(json.dumps(answer)))
wanted = {value(model_id) for model_id in LISTED}
options = agent.config_update(lambda o: wanted <= choices(o), since)
check("login: the picker lists every model the subscription lists", options is not None,
      sorted(wanted - choices(agent.config_updates[-1] if agent.config_updates else [])))
auth = json.loads((agent.agent_dir / "auth.json").read_text())
check("login: the key is stored as an API key", auth.get(PROVIDER, {}).get("type") == "api_key")
catalog = json.loads((agent.agent_dir / "models/provider-model-catalog.v1.json").read_text())["payload"]["models"]
known = {m["id"] for m in catalog if m["provider"] == PROVIDER}
discovered = sorted(LISTED - known)
check("login: the subscription lists models the catalog does not", bool(discovered))
print("      discovered: " + ", ".join(discovered))
check("login: before the login, no discovered model", not {value(m) for m in discovered} & before_login)

# 2. The cache: per key, mode 0600, the key never written.
cache = agent.agent_dir / CACHE
text = cache.read_text() if cache.exists() else ""
check("cache: written beside the catalog", bool(text))
check("cache: scoped by a keyed hash of the key", bool(text) and json.loads(text)["scope"] == scope(KEY))
check("cache: the key is not in the file", KEY not in text)
check("cache: mode 0600", cache.exists() and stat.S_IMODE(cache.stat().st_mode) == 0o600)

# 3. A discovered model answers.
model = next((m for m in discovered if "highspeed" not in m), discovered[0] if discovered else "")
answer = agent.request("session/set_config_option",
                       {"sessionId": agent.session_id, "configId": "model", "value": value(model)})
check(f"chat: {model} is selected", "result" in answer, hide(json.dumps(answer))[:300])
answer = agent.request("session/prompt", {"sessionId": agent.session_id,
                       "prompt": [{"type": "text", "text": "Reply with exactly: OK"}]}, timeout=300)
reply = "".join(agent.text).strip()
check(f"chat: {model} answers", reply == "OK", hide(json.dumps(answer))[:300] + " " + reply[:200])
check("login: the agent exits when stdin closes", agent.close() == 0)

# 4. The next process, offline: the cached list serves the stored key.
offline = Agent("offline", agent_dir=agent.agent_dir, offline=True)
options = offline.new_session()
check("offline: the next process lists the discovered models",
      {value(m) for m in discovered} <= choices(options))
check("offline: the agent exits when stdin closes", offline.close() == 0)
shown = cli("offline", ["--offline", "model", "list", PROVIDER], agent.agent_dir)
check("offline: `model list` with the stored key lists them", set(discovered) <= shown, sorted(shown))

# 5. Logout: the discovered models leave the picker.
logout = Agent("logout", agent_dir=agent.agent_dir, offline=True)
logout.new_session()
since = len(logout.config_updates)
answer = logout.request(LOGOUT, {"providerId": PROVIDER})
check("logout: done", answer.get("result", {}).get("outcome") == "done", hide(json.dumps(answer)))
options = logout.config_update(lambda o: not {value(m) for m in discovered} & choices(o), since)
check("logout: the discovered models are gone", options is not None)
check("logout: the agent exits when stdin closes", logout.close() == 0)

# 6. A refused key clears its list: a revoked key's cache, two hours old.
revoked = OUT / "revoked" / "agent"
revoked.mkdir(parents=True, exist_ok=True)
shutil.copytree(agent.agent_dir / "models", revoked / "models", dirs_exist_ok=True)
fake = "sk-kimi-revoked-e2e"
(revoked / "auth.json").write_text(json.dumps({PROVIDER: {"type": "api_key", "key": fake}}))
snapshot = json.loads(text)
snapshot.update(scope=scope(fake), fetchedAt=snapshot["fetchedAt"] - 2 * 3_600_000)
(revoked / CACHE).write_text(json.dumps(snapshot))
shown = cli("revoked", ["--offline", "model", "list", PROVIDER], revoked)
check("revoked: offline, the stale list still serves", set(discovered) <= shown, sorted(shown))
shown = cli("revoked", ["model", "list", PROVIDER], revoked)
check("revoked: after the 401 the list is gone", not set(discovered) & shown, sorted(shown))
check("revoked: the cache file is removed", not (revoked / CACHE).exists())

print(f"\n{len(FAILED)} failed" if FAILED else "\nall checks passed")
sys.exit(1 if FAILED else 0)
