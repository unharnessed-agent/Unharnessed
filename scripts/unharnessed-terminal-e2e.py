#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["pyte>=0.8"]
# ///
"""End-to-end checks of the interactive terminal (`unharnessed` without -p)
on the real binary, in a pseudo-terminal read back by a terminal emulator
(pyte). No credentials, and no network once the kernel venv is built: the
faux provider plays the model, and a local fake OpenAI-compatible server
plays a custom endpoint.

usage: scripts/unharnessed-terminal-e2e.py <out-dir>

Each scenario writes <out-dir>/<name>.out: the command, then the screen at
each step. The kernel venv lives in <out-dir> (UNHARNESSED_BIN picks the
binary, default target/debug/unharnessed). Exits non-zero when a check
fails.
"""
import fcntl
import http.server
import json
import os
import pty
import select
import shutil
import struct
import subprocess
import sys
import termios
import threading
import time
from pathlib import Path

import pyte

ROOT = Path(__file__).resolve().parent.parent
BIN = os.environ.get("UNHARNESSED_BIN", str(ROOT / "target/debug/unharnessed"))
OUT = Path(sys.argv[1]).resolve()
OUT.mkdir(parents=True, exist_ok=True)
COLS, ROWS = 90, 32
FAILED = []


def check(name, ok, detail=""):
    print(("PASS  " if ok else "FAIL  ") + name + ("" if ok else f" ({detail})"))
    if not ok:
        FAILED.append(name)


def call(code):
    return {"content": [{"type": "toolCall", "name": "ipython", "arguments": {"code": code}}]}


def processes_using(marker):
    rows = subprocess.run(["ps", "-axo", "pid=,command="], capture_output=True, text=True).stdout
    return [row.strip() for row in rows.splitlines() if marker in row]


def settle(timeout=30):
    """Wait until the previous scenario's agent and kernel are gone (they
    share the kernel venv with the next one)."""
    end = time.time() + timeout
    while time.time() < end:
        if not processes_using(f"{BIN} --mode acp") and not processes_using(str(OUT / "kernel-venv")):
            return
        time.sleep(0.1)


class Term:
    """The binary in a pseudo-terminal, its screen kept by pyte."""

    def __init__(self, name, args, script=None, fresh=True):
        settle()
        self.name = name
        self.home = OUT / "home"
        if fresh:
            shutil.rmtree(self.home, ignore_errors=True)
        (self.home / "agent").mkdir(parents=True, exist_ok=True)
        work = OUT / "work"
        work.mkdir(exist_ok=True)
        self.screen = pyte.Screen(COLS, ROWS)
        self.stream = pyte.ByteStream(self.screen)
        env = {k: v for k, v in os.environ.items() if not k.endswith("_API_KEY")}
        env.update(
            HOME=str(self.home),
            UNHARNESSED_CODING_AGENT_DIR=str(self.home / "agent"),
            UNHARNESSED_KERNEL_VENV=str(OUT / "kernel-venv"),
            UNHARNESSED_PACKAGE_DIR=os.environ.get("UNHARNESSED_PACKAGE_DIR", str(ROOT)),
            UV_CACHE_DIR=os.environ.get("UV_CACHE_DIR", str(Path.home() / ".cache/uv")),
            UNHARNESSED_OFFLINE="1",
            UNHARNESSED_PLAIN_KEYS="1",
            TERM="xterm-256color",
            DO_NOT_TRACK="1",
        )
        if script is not None:
            env["UNHARNESSED_FAUX_SCRIPT"] = json.dumps(script)
        self.log = open(OUT / f"{name}.out", "a")
        self.log.write(f"$ unharnessed {' '.join(args)}\n")
        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(work)
            os.execve(BIN, [BIN, *args], env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        self.pid, self.fd = pid, fd

    def pump(self, seconds=0.05):
        end = time.time() + seconds
        while (left := end - time.time()) > 0:
            ready, _, _ = select.select([self.fd], [], [], left)
            if not ready:
                return
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                return
            if not data:
                return
            self.stream.feed(data)

    def text(self):
        return "\n".join(line.rstrip() for line in self.screen.display)

    def wait(self, needle, timeout=60):
        end = time.time() + timeout
        while time.time() < end:
            self.pump(0.1)
            if needle in self.text():
                return True
        self.snap(f"timed out waiting for {needle!r}")
        return False

    def send(self, data, pause=0.2):
        os.write(self.fd, data.encode() if isinstance(data, str) else data)
        self.pump(pause)

    def type(self, text):
        for ch in text:
            self.send(ch, 0.01)
        self.pump(0.2)

    def idle(self, timeout=60):
        """Wait until no turn runs (its status line is gone)."""
        end = time.time() + timeout
        while time.time() < end:
            self.pump(0.1)
            if "to interrupt" not in self.text():
                return True
        self.snap("timed out waiting for the turn to end")
        return False

    def snap(self, label):
        lines = self.text().split("\n")
        while lines and not lines[-1]:
            lines.pop()
        self.log.write(f"──── {label}\n" + "\n".join(lines) + "\n")
        self.log.flush()
        return "\n".join(lines)

    def quit(self, timeout=20):
        """Ctrl+C twice; the exit code, or None when it had to be killed."""
        self.send("\x15", 0.05)
        self.send("\x03", 0.05)
        self.send("\x03", 0.05)
        end = time.time() + timeout
        while time.time() < end:
            self.pump(0.1)
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.snap("after exit")
                return os.waitstatus_to_exitcode(status)
        os.kill(self.pid, 9)
        return None


def basic():
    script = {"tokensPerSecond": 80, "responses": [
        call("from pathlib import Path\nlen([p for p in Path('.').iterdir()])"),
        {"text": "There are **3** entries in `work`.\n\n- one\n- two\n\n```python\nprint('hi')\n```\nDone."},
    ]}
    for name in "abc":
        (OUT / "work" / name).parent.mkdir(exist_ok=True)
        (OUT / "work" / name).write_text(name)
    t = Term("basic", ["--no-session"], script)
    check("basic: the input box and the model show", t.wait("faux-1"))
    check("basic: the opening leaves the wordmark and the keys", t.wait("ctrl+p model") and "╰──╯ ╵ ╵ ╵ ╵ ╰─┘" in t.text(), t.text())
    t.send("\t")
    check("basic: tab on an empty input lists the commands", t.wait("Commands", 5) and "/model [search]" in t.text())
    t.send("\x1b")
    t.send("\x10")
    check("basic: ctrl+p opens the model picker", t.wait("saved as your default", 5))
    t.send("\x1b")
    t.send("\x1b[Z")
    check("basic: shift+tab answers for the effort", t.wait("no reasoning effort", 5))
    t.type("how many files?")
    t.send("\r")
    check("basic: the answer arrives", t.wait("Done.") and t.wait("╰─ 3"))
    screen = t.snap("answered")
    check("basic: the cell is a short block", "● python" in screen and "╰─ 3" in screen, screen)
    check("basic: Markdown markers are rendered", "There are 3 entries in work." in screen and "• one" in screen, screen)
    check("basic: code blocks keep their text", "▏ print('hi')" in screen, screen)
    t.type("/mo")
    check("basic: /mo completes to /model", t.wait("/model [search]", 5))
    t.send("\x1b")
    t.send("\x15")
    check("basic: ctrl+c twice quits with code 0", t.quit() == 0)


def interrupt_and_queue():
    script = {"responses": [
        call("import time\nfor _ in range(120):\n    time.sleep(0.5)"),
        {"text": "unused"},
    ]}
    t = Term("interrupt", ["--no-session"], script)
    t.wait("faux-1")
    t.type("run something long")
    t.send("\r")
    check("interrupt: the running cell shows", t.wait("python ·"))
    t.pump(1.0)
    t.type("and then this")
    t.send("\r")
    check("interrupt: a message typed meanwhile is queued", t.wait("↳ next  and then this", 5))
    t.send("\x1b")
    check("interrupt: esc stops the turn", t.wait("■ interrupted", 20) and t.wait("› and then this", 5))
    screen = t.snap("interrupted")
    check("interrupt: the cell is marked interrupted", "■ python · interrupted" in screen, screen)
    check("interrupt: the queued message comes back to the input", "› and then this" in screen, screen)
    t.send("\x15")
    t.type("/help")
    t.send("\r")
    t.wait("interrupt the agent", 5)
    screen = t.snap("help")
    check("interrupt: /help lists commands and keys", "/login [provider]" in screen and "interrupt the agent" in screen, screen)
    check("interrupt: quits with code 0", t.quit() == 0)


def onboarding_api_key():
    t = Term("onboarding", [])
    check("onboarding: with nothing connected the sign-in opens", t.wait("Sign in"))
    t.type("openai")
    t.wait("❯   OpenAI", 5)
    t.send("\r")
    check("onboarding: an API-key provider asks for the key", t.wait("OpenAI API key"))
    t.type("sk-test-not-a-key")
    t.wait("•" * 17, 5)
    screen = t.snap("key typed")
    check("onboarding: the key is masked", "sk-test" not in screen and "•••" in screen, screen)
    t.send("\r")
    check("onboarding: the provider's models open after sign-in", t.wait("Model · openai", 20))
    # Whatever the catalog holds: take the highlighted model.
    t.send("\r")
    check("onboarding: choosing a model confirms it", t.wait("✓ model: ", 20))
    auth = json.loads((t.home / "agent/auth.json").read_text())
    settings = json.loads((t.home / "agent/settings.json").read_text())
    check("onboarding: the key is stored", auth.get("openai", {}).get("key") == "sk-test-not-a-key", auth)
    check("onboarding: an OpenAI model is the saved default", settings.get("defaultProvider") == "openai", settings)
    footer = t.snap("signed in").split("\n")[-1]
    check("onboarding: the footer shows the chosen model", settings.get("defaultModel", "?") in footer, footer)
    check("onboarding: quits with code 0", t.quit() == 0)


SEEN = []


class FakePortal(http.server.BaseHTTPRequestHandler):
    """An OpenAI-compatible endpoint: /v1/models and streamed completions."""

    def log_message(self, *args):
        pass

    def authorized(self):
        SEEN.append((self.command, self.path, self.headers.get("Authorization")))
        if self.headers.get("Authorization") == "Bearer portal-test":
            return True
        self.send_response(401)
        self.end_headers()
        return False

    def do_GET(self):
        if not self.authorized():
            return
        if not self.path.rstrip("/").endswith("/v1/models"):
            self.send_response(404)
            self.end_headers()
            return
        body = json.dumps({"object": "list", "data": [{"id": "Hermes-4-70B"}, {"id": "Hermes-4-405B"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"{}")
        if not self.authorized():
            return
        SEEN.append(("model", request.get("model")))
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        for piece in ["Hello ", "from the ", "**fake portal**", "."]:
            chunk = {"choices": [{"index": 0, "delta": {"content": piece}, "finish_reason": None}]}
            self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
            self.wfile.flush()
            time.sleep(0.1)
        done = {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 900, "completion_tokens": 6, "total_tokens": 906}}
        self.wfile.write(f"data: {json.dumps(done)}\n\ndata: [DONE]\n\n".encode())


def custom_endpoint_and_resume():
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), FakePortal)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{server.server_address[1]}/v1"
    t = Term("custom", [])
    t.wait("Sign in")
    t.type("custom endpoint")
    t.send("\r")
    check("custom: the endpoint address is asked", t.wait("Endpoint address"))
    t.type(url)
    t.send("\r")
    t.wait("API key")
    t.type("portal-test")
    t.send("\r")
    check("custom: a name is asked", t.wait("How the provider shows"))
    t.send("\x15")
    t.type("Nous Portal")
    t.send("\r")
    check("custom: the endpoint's models are listed", t.wait("Hermes-4-405B"))
    t.send("\r")
    check("custom: the new provider's models open", t.wait("Model · nous-portal", 20))
    t.send("\x1b")
    t.wait("/ commands", 5)
    t.type("say hi")
    t.send("\r")
    check("custom: the answer streams from the endpoint", t.wait("Hello from the fake portal.") and t.idle())
    check("custom: every request carried the key", all(auth == "Bearer portal-test" for _, _, auth in [s for s in SEEN if len(s) == 3]), SEEN)
    check("custom: the chosen model was used", ("model", "Hermes-4-70B") in SEEN, SEEN)
    check("custom: quitting says how to resume", t.quit() == 0 and "unharnessed -c" in t.text(), t.text())

    t = Term("resume", ["-c"], fresh=False)
    # (The first process left with code 0, so its agent released the file.)
    check("resume: -c replays the saved conversation", t.wait("conversation resumed") and "Hello from the fake portal." in t.text(), t.text())
    t.type("/resume")
    t.send("\r")
    check("resume: /resume lists the saved conversations", t.wait("Resume a conversation", 10) and "say hi" in t.snap("picker"))
    t.send("\x1b")
    t.wait("/ commands", 5)
    check("resume: quits with code 0", t.quit() == 0)
    server.shutdown()


def wake_turn():
    script = {"responses": [
        call("job = bash('sleep 2; echo finished')"),
        {"text": "Started it in the background."},
        call("(await job).output"),
        {"text": "The background job printed `finished`."},
    ]}
    t = Term("wake", ["--no-session"], script)
    t.wait("faux-1")
    t.send("\x1b[200~first line\nsecond line\x1b[201~")
    check("wake: a pasted two-line message stays in the box", t.wait("› first line\n  second line", 5))
    t.send("\r")
    t.wait("Started it in the background.")
    check("wake: the footer counts the background command", t.wait("1 background command", 10))
    check("wake: the finished command wakes the agent", t.wait("a background command finished", 30))
    check("wake: the woken turn answers", t.wait("printed finished.", 30))
    check("wake: quits with code 0", t.quit() == 0)


def quit_while_working():
    t = Term("quit-working", ["--no-session"], {"responses": [call("import time\ntime.sleep(60)")]})
    t.wait("faux-1")
    t.type("work for a minute")
    t.send("\r")
    t.wait("python ·")
    code = t.quit()
    check("quit while working: ctrl+c twice quits with code 0", code == 0, code)
    leftover = processes_using(f"{BIN} --mode acp")
    check("quit while working: no agent process is left", not leftover, leftover)


# Build the kernel venv once, before the scenarios (the only step that may
# need the network, for uv's first download).
bootstrap = subprocess.run(
    [BIN, "--unharnessed-bootstrap"], capture_output=True, text=True,
    env=dict(os.environ, UNHARNESSED_KERNEL_VENV=str(OUT / "kernel-venv"),
             UNHARNESSED_PACKAGE_DIR=os.environ.get("UNHARNESSED_PACKAGE_DIR", str(ROOT))),
)
check("setup: the kernel venv builds", bootstrap.returncode == 0, bootstrap.stderr[-500:])

for scenario in [basic, quit_while_working, interrupt_and_queue, onboarding_api_key, custom_endpoint_and_resume, wake_turn]:
    try:
        scenario()
    except Exception as error:  # a crashed scenario is a failed check
        check(f"{scenario.__name__}: ran to the end", False, repr(error))

print(f"\n{'FAILED: ' + ', '.join(FAILED) if FAILED else 'all checks passed'}  (screens in {OUT})")
sys.exit(1 if FAILED else 0)
