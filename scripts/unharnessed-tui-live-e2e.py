#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = ["pyte"]
# ///
"""End-to-end checks of the live terminal UI on the real binary
(`unharnessed` in a terminal, crates/uh-tui): in a pseudo-terminal, its
output read by a terminal emulator, over its own agent process. A session
opens; a message is written and sent, the answer streams back with its cell
and the kernel's panel; a variable and a server open on what the kernel
says of them; a message written while the agent works waits, Esc stops the
agent and brings it back to the message box; the session leaves, its agent
with it, and says how to come back; the saved conversation comes back
(`--resume`), its kernel restored; a launch with a message sends it, and
its options reach the agent; an agent that dies is told; a launch that
cannot work says why on the terminal it gives back; with no provider
connected, the session opens on the providers' screen, where a key is
saved and removed and a browser sign-in is started, stopped, and fails; an
`@` in the message box offers the folder's files, the ones picked go to the
agent as links, a message is sent into the running turn, Up brings back the
messages sent before, and a click moves the caret; the bar's model opens the
models, searched and picked, its effort the levels, set with the arrows and
with a click; a new conversation starts on another agent, the saved ones are
listed, opened (one whose agent is still leaving it too), filtered and
deleted, and `--resume` alone opens on them; a server's row on the panel
opens its port in the browser and stops it, the wheel scrolls a panel
with more rows than the screen, and the panel closes to a rail and opens
again; `/compact` leaves a line across the conversation that opens on the
summary, and comes back with it; a goal shows on the message box with its
time, Esc makes it wait, it comes back with its conversation, goes on at a
click, and is ended at another; a `/` first in the message offers the
agent's commands, the prompt templates and the skills, then the screen's
own: what is typed filters them, Enter, Tab or a click picks one, a skill
goes to the agent under its whole name, and one of the screen's own runs at
once; the settings open in the conversation's place: a switch, a number and
a model are written, and the agent restarts on its conversation; an MCP
server is added, changed, turned off, signed in to and removed, a skill made
and turned off, a folder of skills added and removed; a Marketplace service
is added by a token and signed out of; groups of instructions are made,
changed and deleted; a message sent is brought back, changed and sent again,
the conversation gone back to before it; images dropped on the terminal or
pasted from the clipboard go with a message, and what is no image, one too
many, or any while they are blocked, is refused; Cmd+V attaches the
clipboard's image, a file copied in Finder comes as the file, a path VS
Code's terminal types for a dropped file and a screenshot's thumbnail are
attached, each image by its name.

usage: uv run scripts/unharnessed-tui-live-e2e.py <out-dir>

The model is the faux provider (UNHARNESSED_FAUX_SCRIPT); its cells run in
a real kernel, whose venv is made once under <out-dir>. Nothing reaches the
network (the scenarios that have no faux model are given a proxy that
refuses every connection, and the MCP server a sign-in is tried on is this
machine's, on a port nothing listens on); the clipboard read is the test's
own, never the user's (the system's tools are stood in for); the agent's dir and HOME are under
<out-dir>, and provider keys, tokens and cloud credentials from the
caller's environment are dropped. Each scenario writes
<out-dir>/live-<name>.out (each command, the keys, what the emulator's
screen showed) and <out-dir>/live-<name>.rec, the frames the UI exchanged
with its agent: `unharnessed --tui-replay` plays them. UNHARNESSED_BIN
picks the binary (default target/debug/unharnessed). Exits non-zero when a
check fails.
"""
import base64
import fcntl
import json
import os
import pty
import re
import select
import shutil
import signal
import socket
import struct
import subprocess
import sys
import termios
import time
from pathlib import Path

import pyte

ROOT = Path(__file__).resolve().parent.parent
BIN = os.environ.get("UNHARNESSED_BIN", str(ROOT / "target/debug/unharnessed"))
OUT = Path(sys.argv[1]).resolve()
OUT.mkdir(parents=True, exist_ok=True)
FAILED = []


def free_port():
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


PORT = free_port()


def call(code):
    return {"type": "toolCall", "name": "ipython", "arguments": {"code": code}}


# What the faux model answers, one entry per request, from the first again in each agent process.
SCRIPT = {
    "responses": [
        {"content": [
            {"type": "thinking", "thinking": "I will note the sizes and serve the pages."},
            call('sizes = {"index.html": 1301, "docs.html": 13794}\n'
                 f'server = bash("python3 -u -m http.server {PORT}", timeout=None)\n'
                 'import time; time.sleep(1.5)\n'
                 'sizes'),
        ]},
        {"content": [{"type": "text", "text": f"Serving on `localhost:{PORT}`."}]},
        {"content": [{"type": "text", "text": "word " * 600}]},
        {"content": [{"type": "text", "text": "Third answer."}]},
    ],
    "reasoning": True,
    "tokensPerSecond": 40,
    "modelName": "Faux Model",
}


def check(name, ok, detail=""):
    print(("PASS  " if ok else "FAIL  ") + name + ("" if ok or detail == "" else f" ({detail})"))
    if not ok:
        FAILED.append(name)


def alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


def listening(port):
    with socket.socket() as probe:
        probe.settimeout(0.5)
        return probe.connect_ex(("127.0.0.1", port)) == 0


class Trace:
    def __init__(self, name):
        self.name = name
        self.file = open(OUT / f"live-{name}.out", "w")

    def note(self, text):
        self.file.write(text.rstrip("\n") + "\n")
        self.file.flush()


class Home:
    """An agent dir, a HOME and a working folder of their own, and the environment that points at them."""

    def __init__(self, name, faux=True, script=SCRIPT):
        self.base = OUT / f"live-{name}"
        shutil.rmtree(self.base, ignore_errors=True)
        self.work = self.base / "work"
        for folder in ("agent", "home", "work"):
            (self.base / folder).mkdir(parents=True)
        self.env = {
            name: value
            for name, value in os.environ.items()
            if not name.endswith(("_API_KEY", "_TOKEN")) and not name.startswith(("UNHARNESSED_", "AWS_", "GOOGLE_"))
            and name != "NO_COLOR"
        }
        self.env.update(
            UNHARNESSED_KERNEL_VENV=str(OUT / "kernel-venv"),
            UNHARNESSED_OFFLINE="1",
            DO_NOT_TRACK="1",
            UNHARNESSED_CODING_AGENT_DIR=str(self.base / "agent"),
            HOME=str(self.base / "home"),
            UNHARNESSED_FAUX_SCRIPT=json.dumps(script),
            TERM="xterm-256color",
            COLORTERM="truecolor",
        )
        if not faux:
            # The real providers, none of them reachable: nothing listens on the discard port.
            del self.env["UNHARNESSED_FAUX_SCRIPT"]
            for proxy in ("HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY"):
                self.env[proxy] = self.env[proxy.lower()] = "http://127.0.0.1:9"


class Terminal:
    """`unharnessed <args>` in a pseudo-terminal, its output read by a terminal emulator."""

    def __init__(self, trace, home, args, record=None, columns=120, rows=36, cwd=None):
        self.trace = trace
        self.screen = pyte.Screen(columns, rows)
        self.stream = pyte.ByteStream(self.screen)
        self.raw = b""
        env = dict(home.env)
        if record is not None:
            env["UNHARNESSED_TUI_RECORD"] = str(record)
        cwd = cwd or home.work
        trace.note(f"$ cd {cwd} && unharnessed {' '.join(args)}   # in a {columns}x{rows} pseudo-terminal, "
                   "the faux provider answering")
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.chdir(cwd)
            os.execve(BIN, [BIN, *args], env)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))

    def read(self, seconds):
        end = time.time() + seconds
        while time.time() < end:
            ready, _, _ = select.select([self.fd], [], [], 0.05)
            if not ready:
                continue
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                return
            if not data:
                return
            self.raw += data
            # What turns the kitty keyboard protocol on and off, which a terminal obeys or ignores: the emulator
            # would print it.
            self.stream.feed(re.sub(rb"\x1b\[[<>]1u", b"", data))

    def rows(self):
        return [row.rstrip() for row in self.screen.display]

    def text(self):
        return "\n".join(self.rows())

    def find(self, words):
        """Where the screen first shows `words`: (column, row), else None."""
        for at, row in enumerate(self.rows()):
            if words in row:
                return row.index(words), at
        return None

    def waits(self, words, label, timeout=30.0):
        """Whether the screen comes to show `words`; the screen then goes to the trace."""
        end = time.time() + timeout
        while words not in self.text() and time.time() < end:
            self.read(0.1)
        self.read(0.4)
        self.trace.note(f"-- the screen, {label}:\n" + self.text())
        return words in self.text()

    def leaves(self, words, label, timeout=15.0):
        """Whether the screen comes to no longer show `words`; the screen then goes to the trace."""
        end = time.time() + timeout
        while words in self.text() and time.time() < end:
            self.read(0.1)
        self.read(0.4)
        self.trace.note(f"-- the screen, {label}:\n" + self.text())
        return words not in self.text()

    def until(self, holds, label, timeout=15.0):
        """Whether `holds()` comes to be true of the screen; the screen then goes to the trace."""
        end = time.time() + timeout
        while not holds() and time.time() < end:
            self.read(0.1)
        self.read(0.4)
        self.trace.note(f"-- the screen, {label}:\n" + self.text())
        return bool(holds())

    def click_on(self, words, label):
        """A click on the first place the screen shows `words`, a cell in."""
        column, row = self.find(words)
        self.click(column + 1, row, label)

    def settles(self, label, seconds=0.8):
        self.read(seconds)
        self.trace.note(f"-- the screen, {label}:\n" + self.text())
        return self.text()

    def keys(self, keys, label):
        self.trace.note(f"-- keys: {label}")
        os.write(self.fd, keys.encode() if isinstance(keys, str) else keys)

    def click(self, column, row, label):
        """A press and a release (SGR mouse reports, as terminals send them) at a cell, counted from 0."""
        self.trace.note(f"-- mouse: {label} (click at column {column}, row {row})")
        os.write(self.fd, f"\x1b[<0;{column + 1};{row + 1}M\x1b[<0;{column + 1};{row + 1}m".encode())

    def wheel(self, column, row, down, label, times=1):
        """The wheel turned `times` notches with the pointer at a cell, counted from 0."""
        self.trace.note(f"-- mouse: {label} ({times} notches at column {column}, row {row})")
        os.write(self.fd, f"\x1b[<{65 if down else 64};{column + 1};{row + 1}M".encode() * times)

    def agent(self):
        """The agent's pid: the UI's only child process."""
        found = subprocess.run(["pgrep", "-P", str(self.pid)], capture_output=True, text=True).stdout.split()
        return int(found[0]) if found else None

    def agents(self):
        """The pids of the UI's child processes: its agent, and the ones it let go of that have not exited yet."""
        found = subprocess.run(["pgrep", "-P", str(self.pid)], capture_output=True, text=True).stdout.split()
        return [int(pid) for pid in found]

    def exit(self, timeout=20.0):
        """Its exit code, once it has left; None when it does not."""
        end = time.time() + timeout
        while time.time() < end:
            self.read(0.05)
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                code = os.waitstatus_to_exitcode(status)
                self.trace.note(f"-- after it left, the terminal shows:\n{self.text().strip()}\n[exit {code}]")
                return code
        os.kill(self.pid, signal.SIGKILL)
        os.waitpid(self.pid, 0)
        self.trace.note("[killed: it did not leave]")
        return None

    def given_back(self):
        """The terminal is the shell's again: the alternate screen left, the mouse and the pastes let go of."""
        return all(self.raw.rfind(off) > self.raw.rfind(on) >= 0
                   for on, off in ((b"\x1b[?1049h", b"\x1b[?1049l"), (b"\x1b[?1000h", b"\x1b[?1000l"),
                                   (b"\x1b[?2004h", b"\x1b[?2004l"), (b"\x1b[>1u", b"\x1b[<1u")))


def frames(record):
    """The frames of a recording: (seconds, sent, frame)."""
    out = []
    for line in Path(record).read_text().splitlines():
        if line.startswith("$") or not line.strip():
            continue
        at, arrow, frame = line.strip().split(" ", 2)
        out.append((float(at), arrow == ">", json.loads(frame)))
    return out


def sent(record, method):
    return [frame for _, by_us, frame in frames(record) if by_us and frame.get("method") == method]


def prompts(record):
    return [frame["params"]["prompt"][0]["text"] for frame in sent(record, "session/prompt")]


def sets(record, key=None):
    """The settings written, in order: (key, value), or the values written under `key`."""
    written = [(frame["params"]["key"], frame["params"]["value"]) for frame in sent(record, "_unharnessed/settings/set")]
    return written if key is None else [value for name, value in written if name == key]


def stored(home):
    """The agent's global settings file, as it now is."""
    path = home.base / "agent" / "settings.json"
    return json.loads(path.read_text()) if path.exists() else {}


def switched(term, words):
    """Whether the switch at the end of the row that shows `words` is on: its knob, the lighter half, on the right."""
    column, row = term.find(words)
    light = lambda at: sum(int(term.screen.buffer[row][at].bg[part:part + 2], 16) for part in (0, 2, 4)) \
        if len(term.screen.buffer[row][at].bg) == 6 else 0  # noqa: E731
    # The page's column is 80 cells from the margin; the switch takes the four before its last two.
    return light(78) > light(76)


def png(width=4, height=3):
    """A small PNG, whole: one the providers would read."""
    import zlib
    chunk = lambda kind, data: struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))  # noqa: E731
    rows = b"".join(b"\x00" + b"\xc8\x1e\x1e" * width for _ in range(height))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))


def scenario_session(state):
    trace = Trace("session")
    home = state["home"] = Home("session")
    record = OUT / "live-session.rec"
    term = Terminal(trace, home, [], record=record)
    check("session: it opens on the agent's model and its reasoning effort, the message box empty",
          term.waits("Faux Model", "once the session is open") and "Medium" in term.text() and "Ask Unharnessed" in term.text())
    check("session: nothing is sent to the model before the user writes", prompts(record) == [])

    term.keys("serve the pages\r", "a message, then Enter")
    answered = term.waits("Serving on", "once the first answer is in", timeout=90)
    text = term.settles("the first turn settled", 2.5)
    check("session: the message sent shows, and the answer streams back under what the agent did",
          answered and "serve the pages" in text and re.search(r"≡  Worked for \d+s · 1 cell", text) is not None, text)
    check("session: the message went to the agent as it was written", prompts(record) == ["serve the pages"])
    check("session: the panel lists the server the cell left running, with its port, and its variables",
          re.search(rf"● python3 .*:{PORT} +\d+s", text) is not None
          and re.search(r"sizes +dict", text) is not None and re.search(r"server +BashHandle", text) is not None, text)
    check("session: the context used shows once the answer measured it", re.search(r"[\d.]+k? of 100k context", text) is not None)
    check("session: the server answers on its port", listening(PORT))

    column, row = term.find("sizes")
    term.click(column + 1, row, "on the variable sizes")
    check("session: a click on a variable asks the kernel for its value, shown under it",
          term.waits("'index.html': 1301", "the variable open", timeout=10)
          and [frame["params"]["name"] for frame in sent(record, "_unharnessed/kernel/preview")] == ["sizes"])
    column, row = term.find("● python3")
    term.click(column + 3, row, "on the server")
    check("session: a click on a server asks for its last lines, shown under it",
          term.waits("Serving HTTP", "the server open", timeout=10) and len(sent(record, "_unharnessed/kernel/tail")) >= 1)

    term.keys("say a lot\r", "a second message")
    streaming = term.waits("word word", "the long answer streaming")
    check("session: the mark that stops the agent shows while it works", streaming and term.find("■") is not None)
    term.keys("and then this\r", "a third message, while the agent works")
    text = term.settles("a message waiting")
    check("session: a message written while the agent works waits: shown, not sent",
          "and then this" in text and prompts(record) == ["serve the pages", "say a lot"], prompts(record))
    term.keys("\x1b", "Esc")
    stopped = term.waits("Stopped", "the agent stopped")
    text = term.settles("after the stop")
    box = term.find("Faux Model")[1]
    check("session: Esc stops the agent, and the turn says so",
          stopped and len(sent(record, "session/cancel")) == 1 and term.find("■") is None, text)
    check("session: the message that waited is back in the message box, not sent",
          "and then this" in term.rows()[box - 2] and text.count("and then this") == 1
          and prompts(record) == ["serve the pages", "say a lot"], term.rows()[box - 2])
    term.keys("\r", "Enter")
    check("session: sent again, it goes and is answered",
          term.waits("Third answer.", "the third answer") and prompts(record)[2:] == ["and then this"])

    term.keys("not for now", "a message, not sent")
    term.settles("a message written")
    term.keys("\x03", "Ctrl+C")
    text = term.settles("after Ctrl+C on a written message")
    check("session: Ctrl+C empties the message box, and the session goes on",
          "not for now" not in text and "Ask Unharnessed" in text and "Third answer." in text, text)
    agent = term.agent()
    term.keys("\x03", "Ctrl+C on an empty message box")
    code = term.exit()
    shell = term.text()
    check("session: Ctrl+C on an empty message box leaves, the terminal given back", code == 0 and term.given_back(), code)
    resume = re.search(r"unharnessed --resume ([0-9a-f-]{36})", shell)
    check("session: leaving, it says how to come back to the conversation", resume is not None, shell)
    state["session"] = resume.group(1) if resume else None
    time.sleep(1.0)
    check("session: its agent left with it, and the server the agent ran",
          agent is not None and not alive(agent) and not listening(PORT), agent)

    # What it exchanged with its agent replays: the same conversation, without an agent.
    done = subprocess.run([BIN, "--tui-replay", str(record), "--at", "999", "--size", "120x50", "--no-panel"],
                          capture_output=True, text=True, stdin=subprocess.DEVNULL, timeout=60)
    trace.note(f"$ unharnessed --tui-replay {record} --at 999 --size 120x50 --no-panel\n{done.stdout}{done.stderr}[exit {done.returncode}]")
    check("session: the frames it recorded replay into the same conversation",
          done.returncode == 0 and all(words in done.stdout for words in
                                       ("serve the pages", "Worked for", "say a lot", "Stopped", "and then this", "Third answer.")))


def scenario_resume(state):
    trace = Trace("resume")
    home, session = state.get("home"), state.get("session")
    if home is None or session is None:
        check("resume: the session before it left a conversation to come back to", False)
        return
    record = OUT / "live-resume.rec"
    term = Terminal(trace, home, ["--resume", session], record=record)
    back = term.waits("Third answer.", "the conversation back")
    text = term.settles("the conversation back, settled", 2.0)
    check("resume: the conversation comes back: its messages, its answers, where it was stopped",
          back and all(words in text for words in ("say a lot", "Stopped", "and then this", "Third answer.")), text)
    check("resume: it was loaded from the agent, not sent again",
          len(sent(record, "session/load")) == 1 and sent(record, "session/new") == [] and prompts(record) == [])
    check("resume: the kernel is restored, and the panel lists its variables again",
          "Python kernel restored" in text and re.search(r"sizes +dict", text) is not None, text)
    term.keys("\x1b[5~" * 6, "Page Up, to the conversation's start")
    text = term.settles("the conversation's start")
    check("resume: a turn that came back shows what it did folded, without a time it no longer has",
          "serve the pages" in text and re.search(r"≡  Worked · 1 cell", text) is not None, text)
    term.keys("/quit\r", "/quit")
    check("resume: /quit leaves, the terminal given back", term.exit() == 0 and term.given_back())

    # The same conversation, by its file, from another folder.
    saved = next((home.base / "agent" / "sessions").glob("*.jsonl"))
    elsewhere = home.base / "elsewhere"
    elsewhere.mkdir()
    term = Terminal(trace, home, ["--resume", str(saved)], cwd=elsewhere)
    back = term.waits("Third answer.", "the conversation back, from another folder")
    check("resume: a conversation given by its file comes back from another folder too",
          back and "and then this" in term.text(), term.text())
    term.keys("\x11", "Ctrl+Q")
    term.exit()


def scenario_launch(state):
    trace = Trace("launch")
    home = Home("launch")
    record = OUT / "live-launch.rec"
    term = Terminal(trace, home, ["--thinking", "high", "serve them", "--", "--no-session"], record=record)
    answered = term.waits("Serving on", "the launch's message answered", timeout=90)
    text = term.settles("settled", 2.0)
    check("launch: a message given at the launch is sent once the session is open",
          answered and prompts(record)[:1] == ["serve them"] and "serve them" in text, prompts(record))
    check("launch: the launch's options reach the agent", "High" in text, text)
    following = term.waits("word word", "the launch's second message being answered")
    check("launch: what follows -- is a message even if it looks like an option: it is sent in its turn, not given to the agent",
          following and prompts(record) == ["serve them", "--no-session"]
          and any((home.base / "agent" / "sessions").glob("*.jsonl")), prompts(record))

    agent = term.agent()
    term.keys("kept for later", "a message, not sent")
    term.settles("before the agent dies")
    trace.note(f"-- the agent (pid {agent}) is killed")
    os.kill(agent, signal.SIGKILL)
    told = term.waits("The agent stopped", "the agent dead", timeout=15)
    text = term.settles("after the agent died", 2.0)
    check("launch: an agent that dies is told in the conversation, and the turn it was on fails",
          told and "killed by a signal" in text and text.count("The agent stopped") == 2 and term.find("■") is None, text)
    check("launch: its kernel went with it: the panel lets go of its rows",
          re.search(r"sizes +dict", text) is None and "● python3" not in text, text)
    term.keys("\r", "Enter")
    text = term.settles("after Enter without an agent")
    box = term.find("Faux Model")[1]
    check("launch: without an agent a message is not sent, and stays written",
          "kept for later" in term.rows()[box - 2] and len(prompts(record)) == 2, term.rows()[box - 2])
    term.keys("\x11", "Ctrl+Q")
    check("launch: Ctrl+Q leaves, the terminal given back", term.exit() == 0 and term.given_back())


def scenario_providers(state):
    trace = Trace("providers")
    home = Home("providers", faux=False)
    record = OUT / "live-providers.rec"
    auth = home.base / "agent" / "auth.json"
    key = "sk-e2e-" + "k" * 24
    saved = lambda: json.loads(auth.read_text()) if auth.exists() else {}  # noqa: E731
    term = Terminal(trace, home, [], record=record)
    opened = term.waits("API keys", "a new conversation, no provider connected")
    text = term.text()
    check("providers: with no provider connected, a new conversation opens on the providers, to connect one",
          opened and all(words in text for words in ("Connect a provider", "Subscriptions", "GitHub Copilot", "OpenAI"))
          and "Connected" not in text and "Ask Unharnessed" not in text, text)

    term.keys("mistral", "what filters the providers")
    text = term.settles("the providers filtered")
    check("providers: what is typed filters the providers",
          "Mistral" in text and "OpenAI" not in text and "Subscriptions" not in text, text)
    term.keys("\r", "Enter")
    check("providers: Enter opens the first of them on its key", term.waits("Mistral API key", "the key's form"))
    term.keys(f"\x1b[200~{key}\n\x1b[201~", "a key pasted, its line's end with it")
    text = term.settles("a key written")
    check("providers: a key written shows as dots, never as it is",
          "sk-e2e" not in text and "•" * len(key) in text and saved() == {}, text)
    term.keys("\r", "Enter")
    connected = term.waits("Connected with a saved API key", "the key saved")
    check("providers: Enter saves the key, without the line's end, and the provider is listed as connected",
          connected and saved().get("mistral", {}).get("key") == key and "Mistral API key" not in term.text(), saved())
    kept = [frame["params"] for frame in sent(record, "_unharnessed/providers/set_api_key")]
    check("providers: the recording keeps that a key was saved, not the key",
          kept == [{"providerId": "mistral", "apiKey": "<redacted>"}] and "sk-e2e" not in record.read_text(), kept)

    term.keys("\x1b", "Esc")
    text = term.settles("after Esc on a filter")
    check("providers: Esc empties the filter first", "Search providers" in text and "OpenAI" in text, text)
    term.keys("\x1b", "Esc again")
    back = term.waits("Ask Unharnessed", "the conversation")
    text = term.settles("the conversation, a provider connected")
    # The model the agent moved the session to, as it told it.
    models = [option for _, by_us, frame in frames(record) if not by_us
              for option in frame.get("params", {}).get("update", {}).get("configOptions", []) if option["id"] == "model"]
    provider, _ = json.loads(models[-1]["currentValue"])
    names = [choice["_meta"]["unharnessed"]["model"]["name"] for option in models for choice in option["options"]
             if choice["value"] == models[-1]["currentValue"] and "_meta" in choice]
    check("providers: Esc then gives the conversation back, on a model of the provider just connected",
          back and "Connect a provider" not in text and "API keys" not in text and provider == "mistral"
          and names[0] in text, (provider, names, text))

    term.keys("/login\r", "/login")
    again = term.waits("API keys", "the providers again")
    check("providers: /login comes back to the providers, the connected one first",
          again and "Connected with a saved API key" in term.text() and "Sign in with a subscription" not in term.text(), term.text())
    term.click_on("Disconnect", "on Disconnect")
    asked = term.waits("Remove the saved Mistral API key?", "the question before the key is removed")
    check("providers: removing a key asks first", asked and saved().get("mistral", {}).get("key") == key)
    term.keys("\r", "Enter")
    removed = term.leaves("Connected", "the key removed")
    check("providers: confirmed, the key is removed and the provider no longer connected",
          removed and "mistral" not in saved() and len(sent(record, "_unharnessed/providers/logout")) == 1, saved())

    term.click_on("Anthropic (Claude Pro/Max)", "on a subscription")
    signing = term.waits("Sign in with your browser", "a browser sign-in under way")
    text = term.text()
    urls = [frame["params"]["update"]["url"] for _, by_us, frame in frames(record)
            if not by_us and frame.get("method") == "_unharnessed/providers/login_update"
            and frame["params"]["update"]["kind"] == "url"]
    shown = "".join(row.strip() for row in term.rows() if row.strip() and " " not in row.strip())
    check("providers: a subscription starts its sign-in, the address of its page shown in full",
          signing and len(urls) == 1 and urls[0].startswith("https://claude.ai/") and urls[0] in shown, text)
    term.click_on("Copy the address", "on Copy the address")
    term.settles("the address copied", 0.4)
    copies = re.findall(rb"\x1b\]52;c;([A-Za-z0-9+/=]+)\x07", term.raw)
    check("providers: the address is copied through the terminal",
          [base64.b64decode(copy).decode() for copy in copies] == urls and "Copied" in term.text(), copies)
    term.click_on("Signed in, but nothing happens?", "on the other way")
    check("providers: the sign-in's other way opens on a field for the address the browser ended on",
          term.waits("your browser ended on", "the other way open") and "Continue" in term.text())
    term.keys("\x1b", "Esc")
    stopped = term.waits("Subscriptions", "the sign-in stopped")
    check("providers: Esc stops the sign-in and comes back to the providers",
          stopped and "Sign in with your browser" not in term.text()
          and len(sent(record, "_unharnessed/providers/login_cancel")) == 1, term.text())

    term.click_on("GitHub Copilot", "on another subscription")
    asks = term.waits("GitHub Enterprise URL/domain", "a sign-in that asks something first")
    check("providers: once a sign-in is stopped another starts, and asks what it needs", asks and "Continue" in term.text(), term.text())
    term.keys("\r", "Enter, nothing written")
    failed = term.waits("Failed to login to GitHub Copilot", "the sign-in failed: nothing is reachable", timeout=40)
    answers = [frame["params"] for frame in sent(record, "_unharnessed/providers/login_answer")]
    check("providers: what it asked is answered; the sign-in failing, the screen says why and offers to try again",
          failed and "Try again" in term.text() and len(answers) == 1 and answers[0]["answer"] == "<redacted>", term.text())
    term.click_on("Back", "on Back")
    check("providers: Back leaves the failure for the providers", term.waits("Subscriptions", "the providers") and "Failed to login" not in term.text())
    term.click_on("← Providers", "on the screen's title")
    home_again = term.waits("Ask Unharnessed", "the conversation")
    text = term.settles("the conversation, no provider connected")
    check("providers: a click on the title gives the conversation back, where the bar leads to the providers again",
          home_again and "Connect a provider" in text and "API keys" not in text, text)
    term.click_on("Connect a provider", "on what the bar says")
    check("providers: a click on it opens the providers", term.waits("API keys", "the providers, from the bar"))
    check("providers: no sign-in page was opened that the user did not ask for", sent(record, "_unharnessed/providers/login_open") == [])
    term.keys("\x11", "Ctrl+Q")
    check("providers: Ctrl+Q leaves from the providers, the terminal given back", term.exit() == 0 and term.given_back())


# What the faux model answers in the message box's scenario: two turns that each wait in a cell.
NAPS = {
    "responses": [
        {"content": [{"type": "thinking", "thinking": "A short nap."}, call('import time; time.sleep(5)\n"slept"')]},
        {"content": [{"type": "text", "text": "Answer after the nap."}]},
        {"content": [call('import time; time.sleep(30)\n"slept again"')]},
        {"content": [{"type": "text", "text": "word " * 200}]},
    ],
    "reasoning": True,
    "tokensPerSecond": 40,
    "modelName": "Faux Model",
}


def scenario_composer(state):
    trace = Trace("composer")
    home = Home("composer", script=NAPS)
    record = OUT / "live-composer.rec"
    for path in ("README.md", "src/main.rs", "src/mod.rs", "src/view/main.rs", "docs/read me.md", "build/out.rs"):
        (home.work / path).parent.mkdir(parents=True, exist_ok=True)
        (home.work / path).write_text("\n")
    (home.work / ".gitignore").write_text("build/\n")
    subprocess.run(["git", "init", "-q"], cwd=home.work, check=True)
    term = Terminal(trace, home, [], record=record)
    term.waits("Faux Model", "once the session is open")

    def box():
        """The message box's first row, as the screen shows it."""
        return term.rows()[term.find("Faux Model")[1] - 2].strip()

    term.keys("look at @ma", "a message, an @ word in it")
    offered = term.waits("main.rs  src/view/", "the folder's files offered")
    names = [row.strip() for row in term.rows() if row.strip().startswith("main.rs")]
    check("composer: an @ in the message box offers the folder's files that match what follows it, by name",
          offered and names == ["main.rs  src/", "main.rs  src/view/"], names)
    term.keys("\x1b[B\r", "Down, then Enter")
    text = term.settles("a file picked")
    check("composer: Enter picks the file the arrows chose: it is written as a mention, and the menu leaves",
          box() == "look at @src/view/main.rs" and "main.rs  src/" not in text and prompts(record) == [], box())
    term.keys("@read", "another @ word")
    term.waits("read me.md  docs/", "the files offered again")
    term.keys("\x1b[B\t", "Down, then Tab")
    term.settles("a file with a space picked")
    check("composer: a path with a space is written in quotes", box() == 'look at @src/view/main.rs @"docs/read me.md"', box())
    term.keys("\x7f\x7f", "Backspace, twice: the space, then the mention")
    term.settles("a mention removed")
    check("composer: a backspace right after a mention removes it whole", box() == "look at @src/view/main.rs", box())
    term.keys("@out", "an @ word that names a file git ignores")
    nothing = term.waits("No file by that name.", "nothing to offer")
    check("composer: a file git ignores is not offered", nothing and "out.rs" not in term.text().replace("@out", ""), term.text())
    term.keys("\x1b", "Esc")
    text = term.settles("the menu dismissed")
    check("composer: Esc dismisses the menu and nothing else: the message stays, the agent is not stopped",
          "No file by that name." not in text and box() == "look at @src/view/main.rs @out"
          and sent(record, "session/cancel") == [], box())
    term.keys("\x17and @README.md.\r", "Ctrl+W, a mention typed whole and the dot that ends the sentence, Enter")
    term.waits("Running", "the message sent, the agent working")
    blocks = sent(record, "session/prompt")[0]["params"]["prompt"]
    links = [(block["name"], block["uri"]) for block in blocks if block["type"] == "resource_link"]
    check("composer: the message goes as it was written, with a link for each file it mentions",
          blocks[0] == {"type": "text", "text": "look at @src/view/main.rs and @README.md."}
          and [name for name, _ in links] == ["src/view/main.rs", "README.md"]
          and all(uri == (home.work / name).resolve().as_uri() for name, uri in links), blocks)

    term.keys("also say hi\x1b[13;5u", "a message, then Ctrl+Enter as a terminal with the kitty keyboard protocol sends it")
    term.waits("also say hi", "a message on its way into the turn")
    steered = sent(record, "_unharnessed/session/steer")
    check("composer: Ctrl+Enter sends the message into the turn that runs, not after it",
          len(steered) == 1 and steered[0]["params"]["prompt"] == [{"type": "text", "text": "also say hi"}]
          and isinstance(steered[0]["params"]["turn"], int) and len(prompts(record)) == 1, steered)
    answered = term.waits("Answer after the nap.", "the turn went on with the message", timeout=60)
    text = term.settles("the turn over", 2.0)
    order = [term.find(words)[1] for words in ("Worked for", "also say hi", "Answer after the nap.") if term.find(words)]
    check("composer: the agent takes it at its next step: what it did before is done, its answer comes under the message",
          answered and order == sorted(order) and len(order) == 3 and text.count("also say hi") == 1
          and len(prompts(record)) == 1, text)

    term.keys("nap again\r", "a second message")
    term.waits("Running", "the agent working again")
    term.keys("first waiting\r", "a message, while the agent works")
    term.keys("second waiting\r", "another")
    term.settles("two messages waiting")
    check("composer: Enter while the agent works keeps the messages for after the turn",
          len(sent(record, "_unharnessed/session/steer")) == 1 and len(prompts(record)) == 2)
    term.keys("\r", "Enter, the message box empty")
    term.settles("the first of them on its way into the turn", 1.0)
    steered = [frame["params"]["prompt"][0]["text"] for frame in sent(record, "_unharnessed/session/steer")]
    check("composer: Enter on an empty message box sends the first that waits into the turn",
          steered == ["also say hi", "first waiting"], steered)
    term.keys("\x1b", "Esc")
    stopped = term.waits("Stopped", "the agent stopped")
    text = term.settles("after the stop")
    row = term.find("Faux Model")[1]
    written = [line.strip() for line in term.rows()[row - 4:row - 1]]
    check("composer: stopped, the turn gives back the message it had not taken, with the one that waited: none is sent",
          stopped and written == ["first waiting", "", "second waiting"] and len(prompts(record)) == 2
          and text.count("first waiting") == 1, written)

    term.keys("\x03", "Ctrl+C")
    term.settles("the message box emptied")
    seen = []
    for key, label in (("\x1b[A", "Up"), ("\x1b[A", "Up"), ("\x1b[A", "Up"), ("\x1b[B", "Down"), ("\x1b[B", "Down"), ("\x1b[B", "Down")):
        term.keys(key, label)
        term.settles(f"after {label}", 0.3)
        seen.append(box())
    check("composer: Up brings back the messages sent before, the newest first, and Down returns to the empty box",
          seen == ["second waiting", "first waiting", "nap again", "first waiting", "second waiting", "Ask Unharnessed"], seen)

    term.keys("abcdef", "a word")
    term.settles("a word written", 0.3)
    column, row = term.find("abcdef")
    term.click(column + 2, row, "in the word, on its third letter")
    term.keys("X", "a letter")
    term.settles("a letter written where the click landed", 0.3)
    check("composer: a click in the message box puts the caret where it lands", box() == "abXcdef", box())
    term.keys("\x03@mod", "Ctrl+C, then an @ word")
    term.waits("mod.rs  src/", "a file offered")
    term.click_on("mod.rs  src/", "on the file offered")
    term.settles("a file picked with a click", 0.4)
    check("composer: a click on a file offered picks it", box() == "@src/mod.rs", box())
    term.keys("\x03go on\r", "Ctrl+C, then a third message")
    term.waits("word word", "a long answer streaming")
    term.keys("/compact\x1b[13;5u", "a command, then Ctrl+Enter")
    refused = term.waits("Session is too short to compact", "the command's answer, after the turn", timeout=60)
    errors = [frame["error"]["data"]["details"] for _, by_us, frame in frames(record) if not by_us and "error" in frame]
    steered = [frame["params"]["prompt"][0]["text"] for frame in sent(record, "_unharnessed/session/steer")]
    check("composer: what the running turn refuses to take waits for its end, then goes",
          refused and steered[-1] == "/compact" and errors == ["A command cannot join a running turn."]
          and prompts(record)[2:] == ["go on", "/compact"], (steered, errors, prompts(record)))
    term.keys("\x11", "Ctrl+Q")
    code = term.exit()
    # Where the screen was full the shell's line wraps: the rows end to end.
    resume = re.search(r"unharnessed --resume ([0-9a-f-]{36})", "".join(term.screen.display))
    check("composer: it leaves, the terminal and its keyboard given back", code == 0 and term.given_back() and resume is not None)

    term = Terminal(trace, home, ["--resume", resume.group(1)] if resume else [])
    back = term.waits("/compact", "the conversation back")
    term.keys("\x1b[5~" * 6, "Page Up, to the conversation's start")
    text = term.settles("the conversation's start", 1.5)
    check("composer: a conversation that comes back shows its messages as they were written, without their links",
          back and "look at @src/view/main.rs and @README.md." in text and "file://" not in text
          and "also say hi" in text, text)
    term.keys("\x1b[A", "Up")
    term.settles("after Up", 0.4)
    check("composer: and Up brings them back to the message box", box() == "/compact", box())
    term.keys("\x11", "Ctrl+Q")
    term.exit()

    done = subprocess.run([BIN, "--tui-replay", str(record), "--at", "999", "--size", "120x90", "--no-panel"],
                          capture_output=True, text=True, stdin=subprocess.DEVNULL, timeout=60)
    trace.note(f"$ unharnessed --tui-replay {record} --at 999 --size 120x90 --no-panel\n{done.stdout}{done.stderr}[exit {done.returncode}]")
    rows = done.stdout.splitlines()
    order = [next((at for at, row in enumerate(rows) if words in row), None)
             for words in ("look at @src", "also say hi", "Answer after the nap.", "nap again", "Stopped")]
    check("composer: the frames it recorded replay into the same conversation, the message that joined the turn in its place",
          done.returncode == 0 and None not in order and order == sorted(order) and "first waiting" not in done.stdout, order)


# How the screen names a reasoning effort.
EFFORTS = {"off": "Off", "minimal": "Minimal", "low": "Low", "medium": "Medium", "high": "High", "xhigh": "XHigh", "max": "Max"}


def options(record):
    """The session's options as the agent last gave them: {id: option}."""
    last = {}
    for _, by_us, frame in frames(record):
        if by_us:
            continue
        given = frame.get("result", {}).get("configOptions") or frame.get("params", {}).get("update", {}).get("configOptions")
        if given:
            last = {option["id"]: option for option in given}
    return last


def scenario_pickers(state):
    trace = Trace("pickers")
    home = Home("pickers", faux=False)
    record = OUT / "live-pickers.rec"
    # Keys no provider would take: their models are listed all the same, and nothing is asked of them.
    keys = {provider: {"type": "api_key", "key": "sk-e2e-not-a-key"} for provider in ("anthropic", "openai", "groq", "xai")}
    (home.base / "agent" / "auth.json").write_text(json.dumps(keys))
    term = Terminal(trace, home, [], record=record)
    term.waits("Ask Unharnessed", "once the session is open")
    term.settles("the session open", 1.0)
    first = options(record)
    models = {choice["value"]: choice["_meta"]["unharnessed"]["model"] for choice in first["model"]["options"]}
    current = first["model"]["currentValue"]
    name = lambda value: models[value]["name"]  # noqa: E731
    sets = lambda: [frame["params"] for frame in sent(record, "session/set_config_option")]  # noqa: E731

    def bar():
        """The row under the message box, as the screen shows it."""
        return term.rows()[term.find("Ask Unharnessed")[1] + 2]

    check("pickers: the session opens on a model of a connected provider, named under the message box",
          len({model["provider"] for model in models.values()}) >= 3 and name(current) in bar(), bar())
    levels = [level["value"] for level in first.get("thought_level", {}).get("options", [])]
    check("pickers: the model's reasoning effort shows beside it",
          len(levels) > 1 and EFFORTS[first["thought_level"]["currentValue"]] in bar(), bar())

    term.click_on(name(current), "on the model's name")
    opened = term.waits("Search models", "the models' menu")
    text = term.text()
    used = [row for row in term.rows() if "✓" in row]
    check("pickers: a click on the model opens the models, each with its provider, its window and its price; the one in use has the check",
          opened and all(name(value) in text for value in models) and len(used) == 1 and name(current) in used[0]
          and "/M in" in used[0] and "/M out" in used[0], text)
    other = next(value for value in models if models[value]["provider"] != models[current]["provider"])
    term.keys(name(other).lower(), "the name of another model")
    text = term.settles("the models searched")
    hidden = [value for value in models if name(other).lower() not in f"{name(value)} {models[value]['id']}".lower()]
    check("pickers: what is typed searches the models",
          name(other) in text and hidden and all(name(value) not in text.replace(bar(), "") for value in hidden), text)
    term.keys("\r", "Enter")
    picked = term.leaves("Search models", "the model picked")
    text = term.settles("the session on another model", 1.0)
    check("pickers: Enter picks the model: the agent is told, and the bar names it",
          picked and sets() == [{"sessionId": sets()[0]["sessionId"], "configId": "model", "value": other}]
          and name(other) in bar() and name(current) not in bar(), (sets(), bar()))
    now = options(record)
    has = len(now.get("thought_level", {}).get("options", [])) > 1
    shown = any(label in bar() for label in EFFORTS.values())
    check("pickers: the effort beside the model is the new model's: none for a model that takes none", has == shown, (has, bar()))

    term.keys("\x10", "Ctrl+P")
    term.waits("Search models", "the models' menu again")
    column, row = term.find("Kernel")
    term.click(column + 1, row, "beside the menu")
    closed = term.leaves("Search models", "the menu closed")
    check("pickers: a click beside the menu closes it, and picks nothing", closed and len(sets()) == 1 and name(other) in bar())
    term.keys("/model\r", "/model")
    term.waits("Search models", "the models' menu, from the message box")
    term.click_on(name(current), "on the first model's row")
    term.leaves("Search models", "the first model picked again")
    text = term.settles("back on the first model", 1.0)
    check("pickers: /model opens the menu too, and a click on a row picks it",
          sets()[-1]["value"] == current and name(current) in bar() and "/model" not in text, (sets()[-1], bar()))

    at = levels.index(options(record)["thought_level"]["currentValue"])
    term.keys("/effort\r", "/effort")
    opened = term.waits("Reasoning effort", "the effort's menu")
    labels = term.rows()[term.find("Reasoning effort")[1] + 3].split()
    check("pickers: /effort opens the effort on a track, a stretch for each of the model's levels",
          opened and labels == [EFFORTS[level] for level in levels], labels)
    step, key = (1, "\x1b[C") if at + 1 < len(levels) else (-1, "\x1b[D")
    term.keys(key, "Right" if step == 1 else "Left")
    text = term.settles("the effort moved", 1.0)
    check("pickers: an arrow sets the effort a level over: the agent is told, and the bar says it",
          sets()[-1]["configId"] == "thought_level" and sets()[-1]["value"] == levels[at + step]
          and EFFORTS[levels[at + step]] in bar() and "Reasoning effort" in text, (sets()[-1], bar()))
    term.click_on(EFFORTS[levels[0]], "on the lowest level")
    text = term.settles("the effort set with a click", 1.0)
    check("pickers: a click on a level sets it",
          sets()[-1]["value"] == levels[0] and EFFORTS[levels[0]] in bar()
          and options(record)["thought_level"]["currentValue"] == levels[0], (sets()[-1], bar()))
    term.keys("\x1b", "Esc")
    closed = term.leaves("Reasoning effort", "the effort's menu closed")
    check("pickers: Esc closes the menu, and the session goes on", closed and "Ask Unharnessed" in term.text())
    check("pickers: nothing was sent to a model", prompts(record) == [])
    term.keys("\x11", "Ctrl+Q")
    check("pickers: Ctrl+Q leaves, the terminal given back", term.exit() == 0 and term.given_back())


# What the faux model answers in the saved conversations' scenario, from the first again in each agent: an
# answer; a cell that leaves the kernel a variable slow to save, so the agent takes seconds to exit; a turn
# that waits in a cell.
SLOW = """import time
class Slow:
    def __reduce__(self):
        time.sleep(4)
        return (Slow, ())
slow = Slow()
"made\""""
TOPICS = {
    "responses": [
        {"content": [{"type": "text", "text": "First answer."}]},
        {"content": [call(SLOW)]},
        {"content": [{"type": "text", "text": "Made."}]},
        {"content": [call('import time; time.sleep(60)\n"slept"')]},
        {"content": [{"type": "text", "text": "Late answer."}]},
    ],
    "reasoning": True,
    "tokensPerSecond": 40,
    "modelName": "Faux Model",
}


def scenario_history(state):
    trace = Trace("history")
    home = Home("history", script=TOPICS)
    record = OUT / "live-history.rec"
    files = lambda: sorted((home.base / "agent" / "sessions").glob("*.jsonl"))  # noqa: E731
    term = Terminal(trace, home, [], record=record)
    term.waits("Faux Model", "once the session is open")
    term.keys("about the docs\r", "a message")
    term.waits("First answer.", "the first conversation answered")
    first = term.agent()
    term.click_on("New chat", "on New chat")
    emptied = term.leaves("about the docs", "a new conversation")
    term.settles("the new conversation open", 1.5)
    second = term.agent()
    check("history: New chat starts a new conversation, on an agent of its own; the one before is let go of",
          emptied and "Ask Unharnessed" in term.text() and second not in (None, first) and not alive(first)
          and len(sent(record, "session/new")) == 2, (first, second))
    term.keys("about the tests\r", "a message")
    term.waits("First answer.", "the second conversation answered")
    term.keys("make it slow\r", "a second message: the kernel gets a variable slow to save")
    term.waits("Made.", "the kernel holding it", timeout=60)
    term.keys("take a nap\r", "a third message, which the agent works on")
    term.waits("Running", "the agent working in a cell")

    term.click_on("History", "on History")
    listed = term.waits("about the docs", "the saved conversations")
    rows = [row.strip() for row in term.rows() if "about the" in row]
    check("history: History lists the folder's conversations, the newest first, the one that is open with the check",
          listed and len(rows) == 2 and rows[0].startswith("about the tests") and rows[0].endswith("✓")
          and rows[1].startswith("about the docs") and rows[1].endswith("just now") and len(files()) == 2, rows)
    term.keys("\r", "Enter")
    back = term.waits("about the docs", "the first conversation back")
    text = term.settles("the first conversation", 0.3)
    check("history: Enter opens the one that is not open: it comes back, on an agent started on its file",
          back and "First answer." in text and "about the tests" not in text and "← History" not in text
          and len(sent(record, "session/load")) == 1, text)

    # The agent it left was working: it stops its turn and saves its kernel before it lets go of its file.
    term.keys("/resume\r", "/resume")
    term.waits("about the tests", "the saved conversations again")
    term.keys("\x1b[A\r", "Up, to the one just left, then Enter")
    waited = term.waits("Waiting for this conversation's kernel to be saved", "the conversation waiting for its agent", timeout=10)
    held = alive(second) and len(sent(record, "session/load")) == 1
    check("history: a conversation whose agent is still leaving it waits for that agent to exit", waited and held, term.text())
    back = term.waits("take a nap", "the second conversation back", timeout=60)
    text = term.settles("the second conversation", 1.5)
    check("history: it then opens on an agent of its own, the turn it was left in stopped",
          back and "about the tests" in text and "Made." in text and "Stopped" in text and "Late answer." not in text
          and "Waiting for this" not in text and len(sent(record, "session/load")) == 2
          and len(term.agents()) == 1 and not alive(second), text)

    term.keys("\x12", "Ctrl+R")
    term.waits("about the docs", "the saved conversations, a third time")
    term.keys("DOCS", "what filters them")
    text = term.settles("the conversations filtered")
    check("history: what is typed filters the conversations", "about the docs" in text and "about the tests" not in text, text)
    term.keys("\x1b", "Esc")
    text = term.settles("the filter emptied")
    check("history: Esc empties the filter first", "about the tests" in text and "about the docs" in text, text)
    term.keys("\x1b[3~", "Delete")
    text = term.settles("a row asking once more")
    asking = [row for row in term.rows() if "about the docs" in row]
    check("history: Delete asks once more on the row, and nothing is deleted yet",
          asking and asking[0].rstrip().endswith("Delete") and len(files()) == 2
          and sent(record, "_unharnessed/sessions/delete") == [], asking)
    term.keys("\x1b", "Esc")
    text = term.settles("the question left")
    kept = [row for row in term.rows() if "about the docs" in row]
    check("history: Esc leaves the question, the conversation kept",
          kept and re.search(r"Delete +(just now|\d+ min ago)$", kept[0].rstrip()) is not None and len(files()) == 2
          and "← History" in text, kept)
    term.click_on("Delete", "on Delete")
    term.settles("the row asking again", 0.4)
    term.click_on("Delete", "on Delete, again")
    gone = term.leaves("about the docs", "the conversation deleted")
    check("history: a click on Delete, then another, deletes the conversation: its row and its file",
          gone and len(files()) == 1 and len(sent(record, "_unharnessed/sessions/delete")) == 1
          and "about the tests" in term.text(), term.text())
    term.click_on("← History", "on the screen's title")
    check("history: a click on the title gives the conversation back", term.waits("take a nap", "the conversation"))
    term.keys("/new\r", "/new")
    term.leaves("take a nap", "a new conversation, from the message box")
    term.settles("the new conversation open", 1.5)
    new = term.agent()
    term.keys("\x0e", "Ctrl+N, in a conversation nothing was asked in")
    term.settles("still the new conversation", 1.0)
    check("history: /new starts a new conversation; in one nothing was asked in, there is nothing to start",
          len(sent(record, "session/new")) == 3 and term.agent() == new and "Ask Unharnessed" in term.text())
    term.keys("\x11", "Ctrl+Q")
    code = term.exit()
    check("history: it leaves, the terminal given back, no agent left behind",
          code == 0 and term.given_back() and not any(alive(pid) for pid in (first, second, new) if pid), code)

    done = subprocess.run([BIN, "--tui-replay", str(record), "--at", "999", "--size", "120x40", "--no-panel"],
                          capture_output=True, text=True, stdin=subprocess.DEVNULL, timeout=60)
    trace.note(f"$ unharnessed --tui-replay {record} --at 999 --size 120x40 --no-panel\n{done.stdout}{done.stderr}[exit {done.returncode}]")
    check("history: the frames it recorded replay to the conversation it ended on, not to all of them end to end",
          done.returncode == 0 and "Ask Unharnessed" in done.stdout and "about the" not in done.stdout, done.stdout)

    # `--resume` alone: the conversations to choose from, before anything else.
    term = Terminal(trace, home, ["--resume"])
    opened = term.waits("about the tests", "the saved conversations, at the launch")
    text = term.text()
    check("history: --resume alone opens on the folder's conversations",
          opened and "← History" in text and "Ask Unharnessed" not in text and "about the docs" not in text, text)
    term.keys("\r", "Enter")
    back = term.waits("take a nap", "the conversation chosen")
    check("history: and Enter opens the one chosen", back and "about the tests" in term.text() and "← History" not in term.text())
    term.keys("\x11", "Ctrl+Q")
    code = term.exit()
    check("history: it leaves on the conversation chosen, and says how to come back to it",
          code == 0 and term.given_back()
          and re.search(r"unharnessed --resume [0-9a-f-]{36}", "".join(term.screen.display)) is not None, code)



def answers(record, method):
    """The agent's answers to the requests of that method, in the order they were asked."""
    asked = [frame["id"] for frame in sent(record, method)]
    by_id = {frame["id"]: frame for _, by_us, frame in frames(record) if not by_us and "id" in frame and "method" not in frame}
    return [by_id[request] for request in asked if request in by_id]


def received_result(record, method):
    return [frame["result"] for frame in answers(record, method) if "result" in frame]


def received_error(record, method):
    return [frame["error"] for frame in answers(record, method) if "error" in frame]


def received(record, method):
    return [frame for _, by_us, frame in frames(record) if not by_us and frame.get("method") == method]


def scenario_kernel(state):
    trace = Trace("kernel")
    port = free_port()
    many = "\n".join(f"v{at} = {at}" for at in range(40))
    script = {"responses": [
        {"content": [call(f'{many}\nserver = bash("python3 -u -m http.server {port}", timeout=None)\n'
                          'import time; time.sleep(1.5)')]},
        {"content": [{"type": "text", "text": "Serving."}]},
        {"content": [{"type": "text", "text": "Noted, it was stopped."}]},
    ], "modelName": "Faux Model"}
    home = Home("kernel", script=script)
    # What the UI opens a page with: here, it only notes the address.
    opened = home.base / "opened.txt"
    browser = home.base / "browser.sh"
    browser.write_text(f'#!/bin/sh\necho "$@" >> {opened}\n')
    browser.chmod(0o755)
    home.env["BROWSER"] = str(browser)
    record = OUT / "live-kernel.rec"
    term = Terminal(trace, home, [], record=record, rows=30)
    term.waits("Faux Model", "once the session is open")
    term.keys("serve\r", "a message")
    term.waits("Serving.", "the answer", timeout=90)
    text = term.settles("the panel with more rows than the screen", 2.0)
    listed = [command for frame in received(record, "_unharnessed/kernel/changed")
              for command in frame["params"].get("commands", [])]
    check("kernel: a server's row ends on what stops it, and the panel says how many rows are under its last",
          re.search(rf"● python3.*:{port} +\d+s ×", text) is not None and re.search(r"… \d+ more", text) is not None
          and "v39" not in text and listed and listening(port), text)

    column, row = term.find("Running servers")
    answer = term.find("Serving.")
    term.wheel(column + 2, row + 6, True, "the wheel down, over the panel", times=20)
    text = term.settles("the panel scrolled to its end")
    check("kernel: the wheel over the panel scrolls the panel to its last rows, and not the conversation",
          "v39" in text and "Running servers" not in text and re.search(r"… \d+ more", text) is None
          and term.find("Serving.") == answer, text)
    term.wheel(4, row + 6, False, "the wheel up, over the conversation", times=5)
    text = term.settles("the wheel over the conversation")
    check("kernel: the wheel over the conversation leaves the panel where it is", "v39" in text and "Running servers" not in text, text)
    term.wheel(column + 2, row + 6, False, "the wheel up, over the panel", times=20)
    text = term.settles("the panel back at its first rows")
    check("kernel: the wheel back up shows the panel's first rows again", "Running servers" in text and "v39" not in text, text)

    term.click_on(f":{port}", "on the server's port")
    term.settles("after the click on the port", 1.5)
    pages = opened.read_text().split() if opened.exists() else []
    check("kernel: a click on a server's port opens it in the browser, and does nothing else to the server",
          pages == [f"http://localhost:{port}/"] and listening(port) and sent(record, "_unharnessed/kernel/kill") == []
          and sent(record, "_unharnessed/kernel/tail") == [], pages)

    column, row = term.find("×")
    term.click(column, row, "on what stops the server")
    stopped = term.waits("stopped", "the server shown stopped", timeout=5)
    kills = [frame["params"]["id"] for frame in sent(record, "_unharnessed/kernel/kill")]
    check("kernel: a click on what stops a server shows it stopped at once and asks the agent to stop it, by its id",
          stopped and kills == [listed[0]["id"]] and sent(record, "_unharnessed/kernel/tail") == [], kills)
    gone = term.leaves("Running servers", "the server's row gone")
    woken = term.waits("Noted, it was stopped.", "the agent told of it", timeout=30)
    text = term.settles("after the server stopped", 1.0)
    check("kernel: the server stops, its row leaves the panel, and the agent says what it makes of it",
          gone and woken and not listening(port) and "a background command ended" in text and "v0" in text, text)

    def worked():
        """How far the line the turn's work folds into runs, in cells."""
        row = next(row for row in term.rows() if "Worked for" in row)
        return row.rindex("─") + 1

    before = worked()
    column, row = term.find("»")
    term.click(column, row, "on the mark at the panel's top right")
    text = term.settles("the panel closed")
    rail = term.find("«")
    check("kernel: the mark at the panel's top closes it to a rail at the screen's edge, where the mark that opens it "
          "is, and the conversation takes the panel's room",
          rail == (column, row) and "REPL variables" not in text and "v0" not in text and worked() > before + 20,
          (rail, before, worked(), text))
    term.click(column, row + 8, "on the rail, under its mark")
    text = term.settles("the panel open again")
    check("kernel: a click anywhere on the rail opens the panel again, the conversation back to its width",
          "REPL variables" in text and "v0" in text and term.find("«") is None and worked() == before, text)
    term.keys("\x0b", "Ctrl+K")
    text = term.settles("the panel closed by the keys")
    check("kernel: Ctrl+K closes it too", term.find("«") == (column, row) and "REPL variables" not in text, text)
    term.keys("\x11", "Ctrl+Q")
    check("kernel: the session leaves", term.exit(timeout=60) == 0 and term.given_back())
    done = subprocess.run([BIN, "--tui-replay", str(record), "--at", "999", "--size", "120x40"],
                          capture_output=True, text=True, stdin=subprocess.DEVNULL, timeout=60)
    trace.note(f"$ unharnessed --tui-replay {record} --at 999 --size 120x40\n{done.stdout}{done.stderr}[exit {done.returncode}]")
    check("kernel: its recording replays to a panel without the server",
          done.returncode == 0 and "Noted, it was stopped." in done.stdout and "Running servers" not in done.stdout
          and "REPL variables" in done.stdout, done.stdout)


def scenario_goal(state):
    trace = Trace("goal")
    # A first agent that never gets to the goal's end, then one that does: each starts on its own answers.
    first = {"responses": [{"text": "step one", "delayMs": 1500}, {"text": "step two", "delayMs": 60000}],
             "modelName": "Faux Model"}
    second = {"responses": [
        {"content": [call("print(await goal.complete())")]},
        {"content": [{"type": "text", "text": "All shipped."}]},
        {"text": "tidying", "delayMs": 60000},
        {"content": [{"type": "text", "text": "spare"}]},
    ], "modelName": "Faux Model"}
    home = Home("goal", script=first)
    record = OUT / "live-goal.rec"
    term = Terminal(trace, home, [], record=record)
    term.waits("Faux Model", "once the session is open")

    def box():
        """The message box's rows, from the goal's to the bar's."""
        rows = term.rows()
        return "\n".join(rows[term.find("Ask Unharnessed")[1] - 2:])

    def bar():
        return term.rows()[term.find("Faux Model")[1]]

    def clock():
        """The goal's time on the box's bar, in seconds."""
        found = re.search(r"(\d+):(\d\d)", bar())
        return int(found.group(1)) * 60 + int(found.group(2)) if found else None

    check("goal: without a goal, the message box says nothing of one", "⚑" not in term.text() and "×" not in term.text())
    term.keys("/goal ship it\r", "a goal")
    term.waits("step one", "the goal's first step", timeout=60)
    text = term.settles("the goal active", 1.0)
    check("goal: a goal shows on top of the message box, with its time and the mark that makes it wait",
          re.search(r"⚑ Goal  ship it +×", box()) is not None and re.search(r"\d:\d\d ■", box()) is not None
          and "Goal active: ship it" in text, box())
    before = clock()
    term.settles("two seconds on", 2.2)
    check("goal: its time counts while it is active", before is not None and clock() >= before + 2, (before, clock()))

    term.keys("\x1b", "Esc")
    term.keys("\x1b", "Esc again, at once")
    paused = term.waits("⚑ Paused", "the goal paused", timeout=20)
    text = term.settles("after the pause", 1.0)
    told = prompts(record)
    quiet = [frame["params"].get("_meta") for frame in sent(record, "session/prompt")]
    check("goal: Esc stops the turn and makes the goal wait, once, with a command the user did not write",
          paused and len(sent(record, "session/cancel")) == 1 and told == ["/goal ship it", "/goal pause"]
          and quiet[0] is None and quiet[1] is not None, (told, quiet))
    check("goal: the conversation says the goal waits, without a message of the user's for it",
          "Stopped" in text and "Goal paused: ship it" in text and "/goal pause" not in text
          and re.search(r"⚑ Paused  ship it +×", box()) is not None and re.search(r"\d:\d\d ▶", box()) is not None, text)
    before = clock()
    term.settles("two seconds on", 2.2)
    check("goal: its time stands still while it waits", before is not None and clock() == before, (before, clock()))
    term.keys("\x11", "Ctrl+Q")
    check("goal: the session leaves", term.exit(timeout=60) == 0)

    home.env["UNHARNESSED_FAUX_SCRIPT"] = json.dumps(second)
    record = OUT / "live-goal-back.rec"
    term = Terminal(trace, home, ["--continue"], record=record)
    back = term.waits("⚑ Paused", "the conversation back, its goal with it", timeout=60)
    term.settles("the conversation back", 1.5)
    check("goal: a goal that waits comes back with its conversation",
          back and re.search(r"⚑ Paused  ship it +×", box()) is not None and re.search(r"\d:\d\d ▶", box()) is not None
          and prompts(record) == [], box())
    column, row = term.find("▶")
    term.click(column, row, "on what lets the goal go on")
    done = term.waits("✓ Done", "the goal reached", timeout=60)
    text = term.settles("after the goal was reached", 1.5)
    check("goal: a click on what lets it go on sends the agent on, to the goal's end",
          done and prompts(record) == ["/goal resume"] and "All shipped." in text and "Goal complete: ship it" in text
          and "/goal resume" not in text, (prompts(record), text))
    check("goal: a goal reached shows so, with the time it took",
          re.search(r"✓ Done  ship it +×", box()) is not None and re.search(r"✓ \d:\d\d", bar()) is not None
          and "▶" not in box() and "■" not in box(), box())
    column, row = term.find("×")
    term.click(column, row, "on what ends the goal")
    left = term.leaves("✓ Done", "the goal gone from the box")
    check("goal: a click on what ends a goal reached takes it off the box, and asks the agent nothing",
          left and "⚑" not in box() and prompts(record) == ["/goal resume"], box())

    term.keys("/goal tidy up\r", "another goal")
    active = term.waits("⚑ Goal  tidy up", "the second goal", timeout=30)
    term.settles("the second goal active", 1.0)
    column, row = term.find("×")
    term.click(column, row, "on what ends the goal")
    cleared = term.waits("Goal cleared.", "the goal ended", timeout=30)
    text = term.settles("after the goal ended", 1.0)
    check("goal: a click on what ends a goal that runs stops the turn and clears the goal",
          active and cleared and len(sent(record, "session/cancel")) == 1
          and prompts(record) == ["/goal resume", "/goal tidy up", "/goal clear"] and "/goal clear" not in text
          and "⚑" not in box() and "■" not in box(), (prompts(record), text))
    term.keys("\x11", "Ctrl+Q")
    check("goal: the session leaves, the terminal given back", term.exit(timeout=60) == 0 and term.given_back())

    done = subprocess.run([BIN, "--tui-replay", str(OUT / "live-goal.rec"), "--at", "999", "--size", "120x40"],
                          capture_output=True, text=True, stdin=subprocess.DEVNULL, timeout=60)
    trace.note(f"$ unharnessed --tui-replay {OUT / 'live-goal.rec'} --at 999 --size 120x40\n{done.stdout}{done.stderr}"
               f"[exit {done.returncode}]")
    check("goal: a recording replays with its goal on the message box",
          done.returncode == 0 and re.search(r"⚑ Paused  ship it +×", done.stdout) is not None
          and "Goal paused: ship it" in done.stdout and "/goal pause" not in done.stdout, done.stdout)


# What the faux model answers a prompt template and a skill with.
SLASH = {
    "responses": [
        {"content": [{"type": "text", "text": "Reviewed."}]},
        {"content": [{"type": "text", "text": "Tidied."}]},
        {"content": [{"type": "text", "text": "Spare."}]},
    ],
    "tokensPerSecond": 200,
    "modelName": "Faux Model",
}


def scenario_slash(state):
    trace = Trace("slash")
    home = Home("slash", script=SLASH)
    record = OUT / "live-slash.rec"
    # A prompt template of the user's, one of the folder's, and a skill of the user's.
    for path, text in (
        (home.base / "agent/prompts/review.md",
         '---\ndescription: Review a change for bugs\nargument-hint: "[what to review]"\n---\nReview this carefully: $ARGUMENTS\n'),
        (home.work / ".unharnessed/prompts/ship.md", "Ship it now, with care.\n"),
        (home.base / "agent/skills/tidy/SKILL.md",
         "---\nname: tidy\ndescription: Use this to tidy a folder.\n---\nPut each file where it belongs.\n"),
        (home.work / "notes.md", "\n"),
    ):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    term = Terminal(trace, home, [], record=record)
    term.waits("Faux Model", "once the session is open")

    def box():
        """The message box's first row, as the screen shows it."""
        return term.rows()[term.find("Faux Model")[1] - 2].strip()

    def menu():
        """The rows right over the message box, up to the first empty one: the menu's, while one is open."""
        rows, at, block = term.rows(), term.find("Faux Model")[1] - 6, []
        while at >= 0 and rows[at].strip():
            block.insert(0, rows[at].strip())
            at -= 1
        return block

    def alone():
        """The menu while a command's text is written: its row, then what it does, a row between them."""
        about = menu()
        return term.rows()[term.find("Faux Model")[1] - 6 - len(about) - 1].strip(), " ".join(about)

    term.keys("/", "a slash, first in the message")
    opened = term.waits("/quit", "the commands offered")
    names = [row.split()[0] for row in menu()]
    check("slash: a / first in the message offers the agent's commands, the prompts and the skills, then the screen's own",
          opened and names == ["/compact", "/goal", "/review", "/ship", "/skill:tidy", "/new", "/history", "/model",
                               "/effort", "/settings", "/login", "/quit"], names)
    text = term.text()
    check("slash: each with what it takes and what it does; the agent's built-in skills and /autonomous are not offered",
          re.search(r"/review \[what to review\] +Review a change for bugs", text) is not None
          and re.search(r"/skill:tidy instructions +Use this to tidy a folder\.", text) is not None
          and "attach-image" not in text and "autonomous" not in text, text)
    term.keys("re", "what a name starts with")
    term.leaves("/compact", "the commands that match")
    names = [row.split()[0] for row in menu()]
    check("slash: what is typed keeps the commands that match it, by another name too (/resume)",
          names == ["/review", "/history"], names)
    term.keys("\r", "Enter")
    term.leaves("/history", "the command picked")
    check("slash: Enter picks the first: it is written, not sent, and its row stays alone with what it does",
          box() == "/review" and alone() == ("/review [what to review]", "Review a change for bugs")
          and prompts(record) == [], (box(), alone()))
    term.keys("the diff\r", "what it takes, then Enter")
    answered = term.waits("Reviewed.", "the prompt's answer")
    check("slash: Enter then sends the command with what was written for it",
          answered and prompts(record) == ["/review the diff"] and "Review a change for bugs" not in term.text(),
          prompts(record))

    term.keys("/Tidy the docs", "a skill by its short name, in another case, typed whole")
    shown = term.waits("Use this to tidy a folder.", "the skill's row")
    term.keys("\r", "Enter")
    answered = term.waits("Tidied.", "the skill's answer")
    check("slash: a skill typed by its short name goes to the agent under the name it knows",
          shown and answered and prompts(record)[1:] == ["/skill:tidy the docs"], prompts(record))

    term.keys("/co", "the start of a name")
    term.waits("Summarizes the conversation", "the command offered")
    term.keys("\x1b", "Esc")
    shut = term.leaves("Summarizes the conversation", "the menu dismissed")
    kept = box()
    term.keys("m", "a letter more")
    again = term.waits("Summarizes the conversation", "the menu back")
    check("slash: Esc dismisses the menu and nothing else, until the message changes",
          shut and kept == "/co" and again and box() == "/com" and sent(record, "session/cancel") == [], kept)
    term.keys("\x03/\x1b[B\r", "Ctrl+C, a slash, Down, Enter")
    term.leaves("/compact", "the second command picked")
    term.keys("x", "a letter")
    term.settles("a letter after the command", 0.4)
    check("slash: the arrows choose the command Enter picks; a space follows its name, for what it takes",
          box() == "/goal x" and alone()[0] == "/goal objective" and "turn after turn" in alone()[1], (box(), alone()))
    term.keys("\x03/sh\t", "Ctrl+C, the start of a name, Tab")
    term.keys("y", "a letter")
    term.settles("a command completed", 0.6)
    check("slash: Tab completes the name", box() == "/ship y" and alone() == ("/ship", "Ship it now, with care."),
          (box(), alone()))
    term.keys("\x03/", "Ctrl+C, a slash")
    term.waits("/quit", "the commands offered")
    term.click_on("/ship", "on a command offered")
    term.leaves("/quit", "a command picked with a click")
    check("slash: a click on a command picks it", box() == "/ship" and alone() == ("/ship", "Ship it now, with care."),
          (box(), alone()))

    term.keys("\x03/mo\r", "Ctrl+C, the start of one of the screen's own, Enter")
    models = term.waits("Search models", "the models' menu")
    check("slash: one of the screen's own runs at once, half typed: nothing goes to the agent",
          models and "Ask Unharnessed" in term.text() and len(prompts(record)) == 2, term.text())
    term.keys("\x1b", "Esc")
    term.leaves("Search models", "the models' menu closed")
    term.keys("/GOAL\r", "a command's name typed whole in capitals, Enter")
    told = term.waits("No active goal.", "the command's answer")
    check("slash: a name typed whole is sent at Enter, under the name the agent knows",
          told and prompts(record)[2:] == ["/goal"], prompts(record))

    term.keys("/review @no", "an @ word in a command's text")
    files = term.waits("notes.md", "the folder's files offered")
    covered = "Review a change for bugs" not in term.text()
    term.keys("\r", "Enter")
    term.waits("Review a change for bugs", "a file picked, the command's row back")
    check("slash: an @ in a command's text offers the folder's files, and Enter picks one: the command is not sent",
          files and covered and box() == "/review @notes.md" and len(prompts(record)) == 3, box())
    term.keys("\x03/tmp/x", "Ctrl+C, then a path")
    text = term.settles("a path written first", 0.6)
    term.keys("\x03/zz", "Ctrl+C, then a name no command has")
    none = term.waits("No command, prompt or skill by that name.", "nothing to offer")
    check("slash: a path opens no menu, and a name no command has says so",
          none and "No command" not in text and "/tmp/x" in text, text)
    term.keys("\x03", "Ctrl+C")
    term.settles("the message box emptied", 0.4)
    seen = []
    for _ in range(3):
        term.keys("\x1b[A", "Up")
        text = term.settles("after Up", 0.4)
        seen.append((box(), "Gives the agent" in text or "Use this to tidy" in text or "Review a change" in text))
    check("slash: Up goes back through the commands sent before, as they were written, and the menu stays out of the way",
          seen == [("/GOAL", False), ("/Tidy the docs", False), ("/review the diff", False)], seen)

    term.keys("\x03/q\r", "Ctrl+C, the start of /quit, Enter")
    code = term.exit()
    check("slash: /quit leaves, the terminal given back", code == 0 and term.given_back())
    done = subprocess.run([BIN, "--tui-replay", str(record), "--at", "999", "--size", "120x60", "--no-panel"],
                          capture_output=True, text=True, stdin=subprocess.DEVNULL, timeout=60)
    trace.note(f"$ unharnessed --tui-replay {record} --at 999 --size 120x60 --no-panel\n{done.stdout}{done.stderr}[exit {done.returncode}]")
    rows = done.stdout.splitlines()
    order = [next((at for at, row in enumerate(rows) if words in row), None)
             for words in ("/review the diff", "Reviewed.", "/skill:tidy the docs", "Tidied.", "/goal", "No active goal.")]
    check("slash: the frames it recorded replay into the same conversation, each command as it went",
          done.returncode == 0 and None not in order and order == sorted(order), order)


ANSWERS = {"responses": [{"content": [{"type": "text", "text": f"Answer {at}."}]} for at in range(1, 13)],
           "modelName": "Faux Model"}


def scenario_settings(state):
    trace = Trace("settings")
    home = Home("settings", script=ANSWERS)
    record = OUT / "live-settings.rec"
    term = Terminal(trace, home, [], record=record)
    term.waits("Faux Model", "once the session is open")
    term.keys("hello\r", "a message, then Enter")
    term.waits("Answer 1.", "its answer")
    first = term.agent()

    term.click_on("Settings", "on Settings, beside the wordmark")
    opened = term.waits("Compaction", "the settings")
    text = term.settles("the General tab", 0.6)
    check("settings: a click on Settings opens them in the conversation's place, on the General tab",
          opened and "← Settings" in text and re.search(r"General +MCP +Skills +Instructions", text) is not None
          and "hello" not in text and "Ask Unharnessed" not in text, text)
    check("settings: each setting shows what the agent uses, a number in its unit",
          re.search(r"Retries +3$", text, re.M) is not None and re.search(r"First wait +2 s$", text, re.M) is not None
          and re.search(r"Longest wait +60 s$", text, re.M) is not None
          and re.search(r"Summary model +Writes the summaries\. +Same as the chat model$", text, re.M) is not None
          and switched(term, "Compact automatically") and switched(term, "Retry failed requests"), text)
    check("settings: nothing is written, and nothing waits for a restart, before the user changes something",
          sets(record) == [] and "restarts" not in text and stored(home) == {})

    term.keys("\x1b[B\r", "Down to the first setting, then Enter")
    noticed = term.waits("Some changes apply when the agent restarts.", "the switch turned off")
    check("settings: Enter flips the chosen switch: the agent writes it, and the screen says a restart applies it",
          noticed and sets(record) == [("compaction.enabled", False)] and not switched(term, "Compact automatically")
          and stored(home) == {"compaction": {"enabled": False}} and "Restart agent" in term.text(), (sets(record), stored(home)))

    term.click_on("Retries  ", "on the number of retries")
    term.settles("the number's field open", 0.4)
    term.keys("1.5\r", "a number the setting does not take, then Enter")
    refused = term.waits("A whole number from 0 to 100.", "why it is no number for it")
    check("settings: a number the setting does not take stays in its field with why, and is not written",
          refused and len(sets(record)) == 1 and re.search(r"Retries +1\.5", term.text()) is not None, term.text())
    term.keys("\x7f\x7f\x7f7\r", "the field emptied, another number, Enter")
    written = term.leaves("A whole number", "the number written")
    text = term.settles("the retries set", 0.6)
    check("settings: a number it takes is written at Enter, and shows in the row",
          written and sets(record, "retry.maxRetries") == [7] and re.search(r"Retries +7$", text, re.M) is not None, sets(record))
    term.click_on("First wait", "on the first wait")
    term.settles("its field open", 0.4)
    term.keys("2.5\r", "seconds, with a fraction")
    term.waits("2.5 s", "the wait set")
    check("settings: seconds are written as the milliseconds the file counts in",
          sets(record, "retry.baseDelayMs") == [2500] and stored(home).get("retry") == {"maxRetries": 7, "baseDelayMs": 2500},
          stored(home))
    term.click_on("First wait", "on the first wait again")
    term.settles("its field open on what is set", 0.4)
    held = re.search(r"First wait +2\.5 +s", term.text()) is not None
    term.keys("\x7f\x7f\x7f\r", "the field emptied, Enter")
    term.waits("First wait                                                               2 s", "the default back")
    check("settings: a field opens on what is set, and emptied puts the default back: the key leaves the file",
          held and sets(record, "retry.baseDelayMs") == [2500, None] and stored(home).get("retry") == {"maxRetries": 7},
          (sets(record), stored(home)))
    term.click_on("Summary model", "on the summary model")
    listed = term.waits("Search models", "the models to choose from")
    rows = term.rows()
    at = term.find("Search models")[1]
    check("settings: a model setting opens on the models, none of them first and checked",
          listed and rows[at + 2].strip() == "✓ Same as the chat model" and rows[at + 3].split() == ["Faux", "Model", "faux"], rows[at:at + 4])
    term.keys("\x1b[B\r", "Down, then Enter")
    picked = term.leaves("Search models", "a model picked")
    text = term.settles("the summary model set", 0.6)
    check("settings: Enter picks the model: it is written by its provider and id, and named in the row",
          picked and sets(record, "auxiliaryModel") == ["faux/faux-1"]
          and re.search(r"Summary model +Writes the summaries\. +Faux Model$", text, re.M) is not None, sets(record))

    starts = len(sent(record, "initialize"))
    term.click_on("Restart agent", "on Restart agent")
    restarted = term.leaves("Some changes apply", "the agent restarted", timeout=60)
    text = term.settles("the settings after the restart", 1.0)
    check("settings: Restart agent starts another agent, the settings stay on the screen, and nothing waits for a restart any more",
          restarted and len(sent(record, "initialize")) == starts + 1 and term.agent() not in (None, first)
          and "← Settings" in text and not switched(term, "Compact automatically")
          and re.search(r"Retries +7$", text, re.M) is not None, text)
    term.keys("\x1b", "Esc")
    back = term.waits("Answer 1.", "the conversation back", timeout=30)
    text = term.settles("the conversation after the restart", 1.0)
    check("settings: Esc leaves them: the conversation the agent restarted on is there",
          back and "hello" in text and "← Settings" not in text and "Ask Unharnessed" in text, text)
    loads = sent(record, "session/load")
    check("settings: the agent restarted on the saved conversation", len(loads) == 1, loads)

    term.keys("/settings\r", "/settings")
    again = term.waits("Compaction", "the settings, from the message box")
    term.keys("\t", "Tab")
    tab = term.waits("Your servers", "the next tab")
    term.keys("\x1b[Z\x1b[Z", "Shift+Tab, twice")
    last = term.waits("New group", "the last tab, round from the first")
    term.keys("\x13", "Ctrl+S")
    closed = term.leaves("← Settings", "the settings closed")
    check("settings: /settings opens them too, Tab goes through the tabs, and Ctrl+S closes them",
          again and tab and last and closed and "Ask Unharnessed" in term.text() and len(prompts(record)) == 1, term.text())
    term.keys("\x11", "Ctrl+Q")
    check("settings: the session leaves", term.exit(timeout=60) == 0 and term.given_back())


def scenario_servers(state):
    trace = Trace("servers")
    home = Home("servers", script=ANSWERS)
    record = OUT / "live-servers.rec"
    # A folder of skills of the user's, elsewhere than where the agent looks.
    more = home.base / "more-skills"
    (more / "tidy").mkdir(parents=True)
    (more / "tidy" / "SKILL.md").write_text("---\nname: tidy\ndescription: Use this to tidy a folder.\n---\nPut each file where it belongs.\n")
    term = Terminal(trace, home, [], record=record)
    term.waits("Faux Model", "once the session is open")
    term.keys("\x13", "Ctrl+S")
    term.waits("Compaction", "the settings")
    term.click_on("MCP", "on the MCP tab")
    opened = term.waits("Your servers", "the MCP tab")
    text = term.settles("the MCP tab", 0.6)
    check("servers: the MCP tab lists the user's servers, none yet, and says when the Marketplace cannot be read",
          opened and "+ Add a server" in text and "The Marketplace could not be read." in text, text)
    servers = lambda: sets(record, "mcpServers")  # noqa: E731

    term.click_on("+ Add a server", "on what adds a server")
    term.waits("Reached by", "the server's form")
    term.keys("my docs", "a name the agent could not address")
    said = term.waits("A name is letters, digits, - and _", "why it cannot be its name")
    term.keys("\x7f" * 7 + "docs\t\thttp://127.0.0.1:9/mcp\t\x1b[C", "a name, Tab past how it is reached, its URL, Tab, Right")
    told = term.waits("Once saved, sign in from this row.", "the sign-in set to OAuth")
    term.keys("\r", "Enter")
    saved = term.waits("docs   Not signed in · 127.0.0.1:9", "the server listed")
    text = term.settles("the server saved", 0.6)
    check("servers: a server reached by a URL is added from its form: its name is checked, Enter saves it, and it is listed",
          said and told and saved and servers() == [{"docs": {"type": "http", "url": "http://127.0.0.1:9/mcp", "oauth": True}}]
          and "Reached by" not in text and "Some changes apply when the agent restarts." in text, (servers(), text))

    term.click_on("+ Add a server", "on what adds a server")
    term.waits("Reached by", "the form again")
    term.keys("docs", "the name of the server there is")
    taken = term.waits("There is a server named docs already.", "the name refused")
    term.keys("\r", "Enter")
    term.settles("after Enter on a name that is taken", 0.5)
    refused = len(servers()) == 1
    term.keys("\x7f" * 4 + "gh\t\x1b[C", "another name, Tab, Right: reached by a command")
    term.waits("Variables  ", "the command's fields")
    term.keys('\tnpx -y "@x/my server"\tGH_TOKEN KEY=OTHER\r', "its command, the variables it gets, Enter")
    saved = term.waits("gh     Ready · npx", "the second server listed")
    check("servers: a name another server has is refused; one reached by a command is saved with its arguments and variables",
          taken and refused and saved and servers()[1:] == [{
              "docs": {"type": "http", "url": "http://127.0.0.1:9/mcp", "oauth": True},
              "gh": {"type": "stdio", "command": "npx", "args": ["-y", "@x/my server"],
                     "env": {"GH_TOKEN": {"env": "GH_TOKEN"}, "KEY": {"env": "OTHER"}}}}], servers())

    on = switched(term, "gh     Ready")
    column, row = term.find("gh     Ready")
    term.click(77, row, "on the server's switch")
    off = term.waits("gh     Off · npx", "the server turned off")
    check("servers: a click on a server's switch turns it off in the file, and its row says so",
          on and off and not switched(term, "gh     Off") and servers()[-1]["gh"].get("enabled") is False
          and "Reached by" not in term.text(), servers()[-1])

    term.click_on("docs   Not signed in", "on the first server")
    term.waits("Sign in", "its form, with its sign-in")
    logins = lambda: [frame["params"]["providerId"] for frame in sent(record, "_unharnessed/providers/login")]  # noqa: E731
    term.keys("\r", "Enter")
    failed = term.waits("Try again", "the sign-in failed", timeout=60)
    text = term.settles("the sign-in's screen", 0.5)
    check("servers: Enter on a server that is not signed in starts its sign-in, on a screen of its own under its name",
          failed and logins() == ["mcp:docs"] and "← docs" in text and "← Settings" not in text, text)
    term.click_on("Back", "on Back")
    back = term.waits("← Settings", "the settings back")
    check("servers: leaving the sign-in comes back to the settings, the server's form still open",
          back and "Reached by" in term.text() and "Your servers" in term.text(), term.text())
    column, row = term.find("Name ")
    term.click(column + 20, row, "in the name's field")
    term.keys("2", "a character more")
    term.click_on("Save", "on Save")
    renamed = term.waits("docs2   Not signed in", "the server renamed")
    check("servers: a click in a field puts the keys there; saved under another name, the server keeps its place and what it was",
          renamed and list(servers()[-1]) == ["docs2", "gh"] and servers()[-1]["docs2"] == servers()[0]["docs"], servers()[-1])
    term.click_on("gh      Off", "on the second server")
    term.waits("Remove", "its form")
    term.click_on("Remove", "on Remove")
    asked = term.waits("Remove gh?", "the question before it is removed")
    before = len(servers())
    term.keys("\r", "Enter")
    gone = term.leaves("gh      Off", "the server removed")
    check("servers: Remove asks once more, then the server leaves the file and the list",
          asked and before == len(servers()) - 1 and gone and list(servers()[-1]) == ["docs2"]
          and stored(home)["mcpServers"] == servers()[-1], servers()[-1])

    term.click_on("Skills", "on the Skills tab")
    opened = term.waits("Built in", "the Skills tab")
    text = term.settles("the Skills tab", 0.6)
    check("servers: the Skills tab lists the agent's own skills, without a switch, and the folders it reads",
          opened and re.search(r"attach-image +Use this to see an image\.$", text, re.M) is not None
          and "+ New skill" in text and "+ Add a folder" in text and "Yours" not in text, text)
    term.click_on("+ New skill", "on what makes a skill")
    term.waits("Description", "the skill's form")
    term.keys("PDF", "a name in capitals")
    said = term.waits("Lowercase letters, digits and single hyphens", "why it cannot be its name")
    term.keys("\x7f\x7f\x7fpdf-reports\tWrite PDF reports.\r", "a name, Tab, what it is for, Enter")
    made = term.waits("Created.", "the skill made")
    text = term.settles("the skill listed", 0.6)
    file = home.base / "agent" / "skills" / "pdf-reports" / "SKILL.md"
    creates = [frame["params"] for frame in sent(record, "_unharnessed/skills/create")]
    check("servers: a new skill is made from its form: its file is written, the screen says where, and it is listed as waiting for a restart",
          said and made and creates == [{"name": "pdf-reports", "description": "Write PDF reports."}] and file.exists()
          and "agent/skills/pdf-reports/SKILL.md" in text.replace("\n      ", "") and re.search(r"pdf-reports +Write PDF reports\. +restart$", text, re.M) is not None
          and "Yours" in text and switched(term, "Write PDF reports."), text)
    term.keys("\r", "Enter, on the skill just made")
    flipped = term.until(lambda: re.search(r" restart$", term.text(), re.M) is None, "the skill turned off")
    text = term.settles("the skill off", 0.6)
    toggles = [(Path(frame["params"]["path"]).parent.name, frame["params"]["enabled"])
               for frame in sent(record, "_unharnessed/skills/set")]
    check("servers: Enter on a skill turns it off: the agent is told, and nothing waits for a restart for it any more",
          flipped and toggles == [("pdf-reports", False)] and not switched(term, "Write PDF reports.")
          and re.search(r"pdf-reports +Write PDF reports\.$", text, re.M) is not None, (toggles, text))

    # The rows stand apart: what adds a folder is below the screen's end.
    term.keys("\x1b[6~", "Page Down")
    term.waits("+ Add a folder", "what adds a folder")
    term.click_on("+ Add a folder", "on what adds a folder")
    term.settles("the folder's field", 0.4)
    term.keys("/nowhere/at/all\r", "a folder that is not there, Enter")
    refused = term.waits("no folder at /nowhere/at/all", "why it was not added")
    term.keys("\x15" + str(more) + "\r", "the field emptied, a folder of skills, Enter")
    added = term.waits("tidy ", "the folder's skill listed")
    term.keys("\x1b[6~", "Page Down")
    text = term.settles("the folder added", 0.6)
    check("servers: a folder that is not there is refused with the agent's words; one that is, is added, and its skills are listed",
          refused and added and str(more) in text and stored(home).get("skills") is not None, text)
    term.click_on(more.name, "on the folder")
    asked = term.waits("Stop reading skills from", "the question before it is removed")
    term.keys("\r", "Enter")
    gone = term.leaves(more.name, "the folder gone")
    term.keys("\x1b[5~", "Page Up")
    text = term.settles("the skills without the folder's", 0.6)
    removes = [frame["params"]["path"] for frame in sent(record, "_unharnessed/skills/remove_folder")]
    check("servers: a folder is removed once its row asked again, and its skills leave the list",
          asked and gone and removes == [str(more)] and "tidy" not in text and "pdf-reports" in text, text)
    starts = len(sent(record, "initialize"))
    term.click_on("Restart agent", "on Restart agent")
    restarted = term.leaves("Some changes apply", "the agent restarted", timeout=60)
    text = term.settles("the skills after the restart", 1.0)
    check("servers: a restart before anything was asked starts the agent on a new conversation, the settings still on the screen",
          restarted and len(sent(record, "initialize")) == starts + 1 and sent(record, "session/load") == []
          and "← Settings" in text and "pdf-reports" in text and re.search(r" restart$", text, re.M) is None, text)
    term.keys("\x11", "Ctrl+Q")
    check("servers: the session leaves", term.exit(timeout=60) == 0 and term.given_back())


def scenario_instructions(state):
    trace = Trace("instructions")
    home = Home("instructions", faux=False)
    record = OUT / "live-instructions.rec"
    # Keys no provider would take: their models are listed all the same, and nothing is asked of them.
    keys = {provider: {"type": "api_key", "key": "sk-e2e-not-a-key"} for provider in ("anthropic", "openai", "groq", "xai")}
    (home.base / "agent" / "auth.json").write_text(json.dumps(keys))
    term = Terminal(trace, home, [], record=record)
    term.waits("Ask Unharnessed", "once the session is open")
    term.settles("the session open", 1.0)
    models = [choice["_meta"]["unharnessed"]["model"] for choice in options(record)["model"]["options"]]
    key = lambda model: f"{model['provider']}/{model['id']}"  # noqa: E731
    first, second = models[0], next(model for model in models if model["provider"] != models[0]["provider"])
    groups = lambda: sets(record, "modelInstructions")  # noqa: E731

    term.keys("/settings\r", "/settings")
    term.waits("Compaction", "the settings")
    term.click_on("Instructions", "on the Instructions tab")
    opened = term.waits("+ New group", "the Instructions tab")
    check("instructions: the tab opens on no group, and what makes one", opened and "A group's text joins" in term.text(), term.text())
    term.click_on("+ New group", "on what makes a group")
    listed = term.waits("Search models", "the group's form")
    text = term.settles("the models to check", 0.5)
    check("instructions: a group's form lists the models of the connected providers, none checked, and cannot be saved yet",
          listed and first["name"] in text and "✓" not in text and "Instructions" in text, text)
    term.keys(second["name"].lower(), "the name of a model")
    term.settles("the models searched", 0.5)
    term.keys("\r", "Enter")
    checked = term.waits("✓ " + second["name"], "the model checked")
    term.keys("\r", "Enter again")
    unchecked = term.leaves("✓ " + second["name"], "the model unchecked")
    term.keys("\r\x1b", "Enter once more, then Esc: the search emptied")
    term.settles("the model checked, the search emptied", 0.5)
    term.keys("\tBe brief.\x1b[13;2uNo summary.\r", "Tab to the text, a line, Shift+Enter, another, Enter")
    saved = term.waits("Be brief. No summary.", "the group saved, on its row")
    text = term.settles("the group listed", 0.5)
    check("instructions: Enter checks the model the arrows are on and unchecks it; the text takes lines; saved, the group shows by its models and its text",
          checked and unchecked and saved and groups() == [[{"models": [key(second)], "text": "Be brief.\nNo summary."}]]
          and re.search(rf"{re.escape(second['name'])} +Be brief\. No summary\.", text) is not None and "Search models" not in text,
          (groups(), text))

    # A model the file names whose provider is not connected: the screen does not show it, and keeps it.
    path = home.base / "agent" / "settings.json"
    file = json.loads(path.read_text())
    file["modelInstructions"][0]["models"].insert(0, "elsewhere/old-model")
    path.write_text(json.dumps(file))
    term.keys("\x1b", "Esc")
    term.waits("Ask Unharnessed", "the conversation")
    term.keys("\x13", "Ctrl+S")
    term.waits("Compaction", "the settings, read again")
    term.click_on("Instructions", "on the Instructions tab")
    term.waits("Be brief.", "the group")
    hidden = "old-model" not in term.text()
    term.click_on("Be brief.", "on the group")
    term.waits("Search models", "the group's form")
    opened = "✓ " + second["name"] in term.text()
    term.keys(first["name"].lower() + "\r", "another model searched, Enter")
    term.waits("✓ " + first["name"], "the other model checked")
    term.click_on("Save", "on Save")
    term.waits(f"{second['name']}, {first['name']}", "the group with both models")
    check("instructions: a group opens on its models and its text; saved again, it keeps the models the screen does not show",
          hidden and opened and groups()[-1] == [{"models": ["elsewhere/old-model", key(second), key(first)],
                                                  "text": "Be brief.\nNo summary."}], groups()[-1])
    term.click_on("Be brief.", "on the group")
    term.waits("Delete", "the group's form")
    term.click_on("Delete", "on Delete")
    asked = term.waits("Delete this group?", "the question before it is deleted")
    count = len(groups())
    term.keys("\r", "Enter")
    gone = term.leaves("Be brief.", "the group deleted")
    check("instructions: Delete asks once more, then the group leaves the file",
          asked and count == len(groups()) - 1 and gone and groups()[-1] is None and "modelInstructions" not in stored(home),
          (groups()[-1], stored(home)))

    term.click_on("General", "on the General tab")
    term.waits("Compaction", "the General tab")
    term.keys("\x1b[6~", "Page Down")
    term.waits("Image model", "the images' settings")
    term.click_on("Image model", "on the image model")
    term.waits("Search models", "the models that read images")
    term.settles("the image models", 0.5)
    blind = [model for model in models if "image" not in model["input"]]
    seeing = [model for model in models if "image" in model["input"]]
    term.keys(blind[0]["name"].lower() if blind else "zzzz", "the name of a model that reads no image")
    none = term.waits("No model matches", "no such model among them")
    term.keys("\x15" + seeing[0]["name"].lower() + "\r", "the search emptied, a model that reads images, Enter")
    picked = term.leaves("Search models", "the image model picked")
    check("instructions: the image model is chosen among the models that read images, and written by its provider and id",
          none and picked and sets(record, "imageModel") == [key(seeing[0])] and stored(home).get("imageModel") == key(seeing[0]),
          (sets(record, "imageModel"), stored(home)))
    check("instructions: nothing was asked of a model", sent(record, "session/prompt") == [])
    term.keys("\x11", "Ctrl+Q")
    check("instructions: the session leaves", term.exit(timeout=60) == 0 and term.given_back())


def scenario_rewind(state):
    trace = Trace("rewind")
    home = Home("rewind", script=ANSWERS)
    record = OUT / "live-rewind.rec"
    term = Terminal(trace, home, [], record=record)
    term.waits("Faux Model", "once the session is open")
    for at, message in enumerate(("one", "two", "three"), 1):
        term.keys(message + "\r", "a message, then Enter")
        term.waits(f"Answer {at}.", "its answer")
    term.settles("three turns", 1.0)

    def box():
        """The message box's rows, down to its bar."""
        rows = term.rows()
        top = next(at for at in range(len(rows) - 1, 0, -1) if not rows[at].strip() and at < term.find("Faux Model")[1] - 3)
        return [row.strip() for row in rows[top + 1:term.find("Faux Model")[1]] if row.strip()]

    starts = [frame["params"]["update"]["_meta"]["unharnessed"]["turnStart"] for frame in received(record, "session/update")
              if "turnStart" in frame["params"]["update"].get("_meta", {}).get("unharnessed", {})]
    rewinds = lambda: [frame["params"]["to"] for frame in sent(record, "_unharnessed/session/rewind")]  # noqa: E731
    term.keys("a draft", "something written, not sent")
    term.settles("a draft in the message box", 0.4)
    term.click_on("two", "on the second message")
    editing = term.waits("Editing an earlier message", "the message back in the message box")
    check("rewind: a click on a message sent brings it back to the message box, which says it is being changed",
          editing and box()[0].startswith("✎ Editing an earlier message") and box()[0].endswith("×") and box()[1:] == ["two"],
          box())
    term.keys("\x1b", "Esc")
    left = term.leaves("Editing an earlier message", "the message left as it was")
    check("rewind: Esc leaves the message as it was: what was being written comes back, nothing is asked of the agent",
          left and box() == ["a draft"] and rewinds() == [] and len(prompts(record)) == 3 and sent(record, "session/cancel") == [],
          box())

    term.keys("\x1b[1;2A", "Shift+Up")
    term.waits("Editing an earlier message", "the last message back")
    last = box()[-1]
    term.keys("\x1b[1;2A", "Shift+Up again")
    term.settles("the message before it", 0.5)
    before = box()[-1]
    term.keys("\x1b[1;2B\x1b[1;2B", "Shift+Down, twice")
    out = term.leaves("Editing an earlier message", "past the last message")
    check("rewind: Shift+Up goes back through the messages sent, Shift+Down returns, and past the last the box is as it was",
          (last, before) == ("three", "two") and out and box() == ["a draft"] and rewinds() == [], (last, before, box()))

    term.keys("\x1b[1;2A\x1b[1;2A", "Shift+Up, twice")
    term.settles("the second message being changed", 0.5)
    term.keys(", changed\r", "more words, then Enter")
    answered = term.waits("Answer 4.", "the changed message's answer")
    text = term.settles("the conversation gone back", 1.0)
    check("rewind: sent, the changed message takes the earlier one's place: the agent goes back to before it, then gets the new one",
          answered and rewinds() == [starts[1]["rewindTo"]] and prompts(record)[3:] == ["two, changed"]
          and [frame.get("method") for _, by_us, frame in frames(record) if by_us][-2:] == ["_unharnessed/session/rewind", "session/prompt"],
          (rewinds(), prompts(record)))
    check("rewind: the earlier message, its answer and what came after are gone from the conversation; what was being written is back",
          "two, changed" in text and "Answer 1." in text and "Answer 2." not in text and "three" not in text
          and "Answer 3." not in text and box() == ["a draft"], text)

    term.keys("\x03\x11", "Ctrl+C, Ctrl+Q")
    code = term.exit(timeout=60)
    check("rewind: the session leaves", code == 0 and term.given_back())
    back = OUT / "live-rewind-back.rec"
    term = Terminal(trace, home, ["--continue"], record=back)
    restored = term.waits("Answer 4.", "the conversation back", timeout=60)
    text = term.settles("the conversation as it was left", 1.0)
    check("rewind: the conversation comes back as it was left, without what the going back took away",
          restored and "two, changed" in text and "three" not in text and "Answer 2." not in text, text)
    term.click_on("one", "on the first message")
    term.waits("Editing an earlier message", "the first message back in the box")
    term.keys("\x15first of all\r", "the box emptied, other words, Enter")
    answered = term.waits("Answer 1.", "the new first answer")
    text = term.settles("the conversation gone back to its start", 1.0)
    check("rewind: a message of a conversation that came back can be changed too",
          answered and len(sent(back, "_unharnessed/session/rewind")) == 1 and prompts(back) == ["first of all"]
          and "two, changed" not in text and "Answer 4." not in text and "first of all" in text, text)
    term.keys("\x11", "Ctrl+Q")
    term.exit(timeout=60)
    done = subprocess.run([BIN, "--tui-replay", str(record), "--at", "999", "--size", "120x40"],
                          capture_output=True, text=True, stdin=subprocess.DEVNULL, timeout=60)
    trace.note(f"$ unharnessed --tui-replay {record} --at 999 --size 120x40\n{done.stdout}{done.stderr}[exit {done.returncode}]")
    check("rewind: its recording replays to the conversation gone back",
          done.returncode == 0 and "two, changed" in done.stdout and "Answer 4." in done.stdout
          and "three" not in done.stdout and "Answer 2." not in done.stdout, done.stdout)


def scenario_images(state):
    trace = Trace("images")
    home = Home("images", script=ANSWERS)
    record = OUT / "live-images.rec"
    # Images to drop on the terminal, a file that only has an image's name, and a clipboard that holds an image:
    # the system's tools are stood in for, the user's clipboard is not read.
    shots = home.base / "my shots"
    shots.mkdir()
    picture = png()
    (shots / "red one.png").write_bytes(picture)
    (shots / "photo.JPG").write_bytes(b"\xff\xd8\xff\xe0" + b"\x00" * 64)
    (shots / "notes.png").write_text("not an image\n")
    tools = home.base / "tools"
    tools.mkdir()
    clip = png(2, 2)
    (tools / "clip.png").write_bytes(clip)
    (tools / "clip.said").write_text(f"«data PNGf{clip.hex().upper()}»\n")
    for name, body in (("osascript", f"cat '{tools / 'clip.said'}'"),
                       ("wl-paste", f"cat '{tools / 'clip.png'}'"), ("xclip", f"cat '{tools / 'clip.png'}'")):
        (tools / name).write_text(f"#!/bin/sh\n{body}\n")
        (tools / name).chmod(0o755)
    home.env["PATH"] = f"{tools}:{home.env['PATH']}"
    term = Terminal(trace, home, [], record=record)
    term.waits("Faux Model", "once the session is open")
    paste = lambda text, label: term.keys(f"\x1b[200~{text}\x1b[201~", label)  # noqa: E731

    def chips():
        """The images the message box shows, and what it says under them."""
        rows = term.rows()
        at = next((at for at, row in enumerate(rows) if "▣" in row and "×" in row), None)
        if at is None:
            return [], ""
        return re.findall(r"▣ (.+?) ×", rows[at]), rows[at + 1].strip()

    paste(f"{str(shots).replace(' ', chr(92) + ' ')}/red\\ one.png '{shots}/photo.JPG' ", "two images dropped on the terminal: their paths")
    term.waits("photo.JPG", "the images on the message box")
    names, said = chips()
    check("images: the paths of images dropped on the terminal attach them to the message, which shows each by its name",
          names == ["red one.png", "photo.JPG"] and "Ask Unharnessed" in term.text() and prompts(record) == [], (names, term.text()))
    check("images: the box says so when the model in use reads no image and no image model stands in",
          said == "Faux Model doesn't read images: set an image model in the settings.", said)
    paste(f"{str(shots).replace(' ', chr(92) + ' ')}/notes.png", "a file that only has an image's name")
    refused = term.waits("notes.png is not a PNG, JPEG, GIF or WebP image.", "why it was not attached")
    check("images: a file that is no image is refused by what it holds, and the others stay", refused and chips()[0] == names, chips())
    paste(f"look at {shots}/photo.JPG please", "a path in a sentence")
    term.waits("please", "the sentence written")
    check("images: a paste that is more than paths is written in the message as it is",
          f"look at {shots}/photo.JPG please" in " ".join(row.strip() for row in term.rows()) and chips()[0] == names, term.text())
    term.keys("\x15", "Ctrl+U: the message emptied")
    term.settles("the message emptied, its images kept", 0.4)
    column, row = term.find("red one.png ×")
    term.click(column + len("red one.png "), row, "on what takes the first image off")
    term.until(lambda: chips()[0] == ["photo.JPG"], "the first image off")
    first = chips()[0]
    term.keys("\x7f", "Backspace, on an empty message")
    term.until(lambda: chips()[0] == [], "the last image off")
    check("images: a click on an image's cross takes it off, and Backspace on an empty message the last one",
          first == ["photo.JPG"] and chips() == ([], "") and "doesn't read images" not in term.text(), term.text())

    term.keys("\x16", "Ctrl+V")
    pasted = term.until(lambda: chips()[0] == ["Image 1"], "the clipboard's image on the message box")
    paste(f"'{shots}/red one.png'", "an image's path, quoted")
    term.until(lambda: len(chips()[0]) == 2, "two images")
    term.keys("what are these?\r", "a message, then Enter")
    answered = term.waits("Answer 1.", "its answer")
    text = term.settles("the message sent with its images", 1.0)
    request = sent(record, "session/prompt")[0]["params"]
    blocks = request["prompt"]
    check("images: Ctrl+V attaches the image on the clipboard; sent, the message carries each image whole, by its kind",
          pasted and answered and [block["type"] for block in blocks] == ["text", "image", "image"]
          and blocks[0]["text"] == "what are these?"
          and [(block["mimeType"], base64.b64decode(block["data"])) for block in blocks[1:]]
          == [("image/png", clip), ("image/png", picture)], [dict(block, data=len(block.get("data", ""))) for block in blocks])
    check("images: the message sent shows with its images' names, and the message box has none left",
          re.search(r"▣ Image 1 +▣ red one\.png", text) is not None and chips() == ([], "")
          and request["_meta"] == {"unharnessedTui": {"images": ["Image 1", "red one.png"]}}, text)

    paste(f"'{shots}/photo.JPG'", "an image's path")
    term.until(lambda: chips()[0] == ["photo.JPG"], "the image on the message box")
    term.keys("\r", "Enter, with nothing written")
    answered = term.waits("Answer 2.", "the answer to a message of an image alone")
    blocks = sent(record, "session/prompt")[1]["params"]["prompt"]
    check("images: a message of an image alone is sent too",
          answered and [(block["type"], block.get("mimeType")) for block in blocks] == [("text", None), ("image", "image/jpeg")],
          [block["type"] for block in blocks])

    term.click_on("what are these?", "on the first message")
    term.waits("Editing an earlier message", "the message back in the box")
    back = chips()[0]
    term.keys("\x1b", "Esc")
    term.leaves("Editing an earlier message", "the message left as it was")
    check("images: a message brought back to be changed comes with its images", back == ["Image 1", "red one.png"], back)
    many = " ".join(f"'{shots}/red one.png'" for _ in range(11))
    paste(many, "eleven images at once")
    full = term.waits("A message carries 10 images at most.", "the eleventh refused")
    check("images: a message carries ten images at most", full and "+" in term.text() and "▣" in term.text(), term.text())
    term.keys("\x03", "Ctrl+C")
    term.until(lambda: chips() == ([], ""), "the message emptied, its images with it")

    term.keys("\x13", "Ctrl+S")
    term.waits("Compaction", "the settings")
    term.keys("\x1b[6~\x1b[6~", "Page Down, twice")
    term.waits("Block images", "the images' settings")
    term.click_on("Block images", "on Block images")
    term.until(lambda: sets(record, "images.blockImages") == [True] and switched(term, "Block images"), "images blocked")
    term.keys("\x1b", "Esc")
    term.waits("Ask Unharnessed", "the conversation")
    paste(f"'{shots}/red one.png'", "an image's path, while images are blocked")
    blocked = term.waits("Images are blocked", "why it was not attached")
    check("images: while the settings block images, none is attached, and the box says how to send them",
          blocked and "turn off “Block images” in the settings" in term.text() and "▣ red one.png ×" not in term.text(), term.text())
    term.keys("\x11", "Ctrl+Q")
    check("images: the session leaves", term.exit(timeout=60) == 0 and term.given_back())

    term = Terminal(trace, home, ["--continue"], record=OUT / "live-images-back.rec")
    restored = term.waits("Answer 2.", "the conversation back", timeout=60)
    text = term.settles("the conversation as it was left", 1.0)
    check("images: the conversation comes back with each message's images, by what the agent kept of them",
          restored and "what are these?" in text and re.search(r"▣ image +▣ image", text) is not None
          and len(re.findall(r"▣ image", text)) == 3, text)
    term.keys("\x11", "Ctrl+Q")
    term.exit(timeout=60)
    done = subprocess.run([BIN, "--tui-replay", str(record), "--at", "999", "--size", "120x40"],
                          capture_output=True, text=True, stdin=subprocess.DEVNULL, timeout=60)
    trace.note(f"$ unharnessed --tui-replay {record} --at 999 --size 120x40\n{done.stdout}{done.stderr}[exit {done.returncode}]")
    check("images: its recording replays with the images' names",
          done.returncode == 0 and re.search(r"▣ Image 1 +▣ red one\.png", done.stdout) is not None
          and "▣ photo.JPG" in done.stdout, done.stdout)


def scenario_clipboard(state):
    """How an image could still be missed, listed before the code: Cmd+V on a clipboard of an image alone pastes
    nothing (the terminal finds no text), or Cmd+V comes as a key; a file copied in Finder brings its icon, or is
    pasted as its name (VS Code); a name pasted that the clipboard does not hold is lost; the path VS Code's terminal
    types for a dropped file is written, not attached, or the keys held to see it are lost (`/` opens nothing); a
    screenshot's thumbnail dropped is cut at the narrow space macOS puts before `PM`, or is written once macOS put it
    away; an image without a name is called "pasted image"."""
    trace = Trace("clipboard")
    home = Home("clipboard", script=ANSWERS)
    record = OUT / "live-clipboard.rec"
    shots = home.base / "Desktop"
    shots.mkdir()
    captured, red, quoted, held = png(3, 3), png(5, 2), png(2, 5), png(4, 4)
    capture = shots / "Capture d’écran 2026-11-23 à 20.50.12.png"
    capture.write_bytes(captured)
    (shots / "red one.png").write_bytes(red)
    (shots / "it's.png").write_bytes(quoted)
    # Where macOS keeps a screenshot while its thumbnail shows; the name has the narrow space it puts before PM.
    staging = home.base / "T" / "TemporaryItems" / "NSIRD_screencaptureui_Bth1wV"
    staging.mkdir(parents=True)
    thumbnail = staging / "Screenshot 2026-11-23 at 8.50.12\u202fPM.png"
    thumbnail.write_bytes(held)
    # The clipboard, stood in for: what osascript says of it is in a file, changed as the checks go.
    tools = home.base / "tools"
    tools.mkdir()
    clip = png(2, 2)
    said = tools / "clip.said"
    holds = lambda text: said.write_text(text)  # noqa: E731
    holds(f"«data PNGf{clip.hex().upper()}»\n")
    (tools / "osascript").write_text(f"#!/bin/sh\ncat '{said}'\n")
    (tools / "osascript").chmod(0o755)
    home.env["PATH"] = f"{tools}:{home.env['PATH']}"
    term = Terminal(trace, home, [], record=record, columns=160)
    term.waits("Faux Model", "once the session is open")
    paste = lambda text, label: term.keys(f"\x1b[200~{text}\x1b[201~", label)  # noqa: E731

    def typed(text, label):
        """What VS Code's terminal writes for a file dropped on it: the path typed, not pasted, in pieces of 50
        characters 5 ms apart."""
        trace.note(f"-- typed at once, as VS Code does for a drop: {label}")
        data = text.encode()
        for at in range(0, len(data), 50):
            os.write(term.fd, data[at:at + 50])
            time.sleep(0.005)

    def chips():
        rows = term.rows()
        at = next((at for at, row in enumerate(rows) if "▣" in row and "×" in row), None)
        if at is None:
            return [], ""
        return re.findall(r"▣ (.+?) ×", rows[at]), rows[at + 1].strip()

    def box():
        """What is written in the message box: its row, the second above the bar."""
        rows = term.rows()
        bar = max(at for at, row in enumerate(rows) if row.strip().startswith("Faux Model"))
        return rows[bar - 2].strip()

    paste("", "Cmd+V on a clipboard of an image alone: the terminal pastes nothing")
    first = term.until(lambda: chips()[0] == ["Image 1"], "the clipboard's image on the message box")
    term.keys("\x1b[118;9u", "Cmd+V, reported as a key (the kitty keyboard protocol)")
    second = term.until(lambda: chips()[0] == ["Image 1", "Image 2"], "a second one")
    check("clipboard: Cmd+V attaches the clipboard's image when the terminal pastes nothing for it, or reports the key; "
          "an image without a name is numbered",
          first and second and "pasted image" not in term.text() and box() == "Ask Unharnessed", (chips(), box()))

    holds(f"file {capture}\n")
    term.keys("\x16", "Ctrl+V, a screenshot copied in Finder")
    finder = term.until(lambda: len(chips()[0]) == 3, "the file on the message box")
    holds(f"file {shots / 'red one.png'}\n")
    paste("red one.png", "Cmd+V in VS Code of a file copied in Finder: its name pasted")
    named = term.until(lambda: len(chips()[0]) == 4, "the file on the message box")
    check("clipboard: a file copied in Finder attaches the file, by its name, whether it comes by Ctrl+V or as its name",
          finder and named and chips()[0][2:] == [capture.name, "red one.png"] and box() == "Ask Unharnessed",
          (chips(), box()))
    holds("")
    paste("notes.png", "a name the clipboard holds no file of")
    written = term.until(lambda: box() == "notes.png", "the name written")
    term.keys("\x16", "Ctrl+V, on a clipboard of text")
    told = term.waits("The clipboard holds no image.", "why nothing was attached")
    check("clipboard: a name the clipboard holds no file of is written; Ctrl+V on no image says so",
          written and told and len(chips()[0]) == 4, (chips(), box()))
    term.keys("\x15", "Ctrl+U")
    term.until(lambda: box() == "Ask Unharnessed", "the message emptied")

    term.keys("what are these?\r", "a message, then Enter")
    answered = term.waits("Answer 1.", "its answer")
    request = sent(record, "session/prompt")[0]["params"]
    images = [base64.b64decode(block["data"]) for block in request["prompt"] if block["type"] == "image"]
    check("clipboard: sent, the images are the clipboard's and the files', and go by their names",
          answered and images == [clip, clip, captured, red]
          and request["_meta"] == {"unharnessedTui": {"images": ["Image 1", "Image 2", capture.name, "red one.png"]}},
          request.get("_meta"))

    typed(f"'{shots}/it\\'s.png'", "a file dropped on VS Code's terminal, quoted as it quotes it")
    dropped = term.until(lambda: chips()[0] == ["it's.png"], "the image on the message box")
    check("clipboard: the path VS Code's terminal types for a dropped image attaches it, and nothing is written",
          dropped and box() == "Ask Unharnessed", (chips(), box()))
    typed("/nowhere/at/all.png", "a path typed at once that is no file")
    written = term.until(lambda: box() == "/nowhere/at/all.png", "the path written")
    term.keys("\x15", "Ctrl+U")
    term.until(lambda: box() == "Ask Unharnessed", "the message emptied")
    term.keys("/", "a slash, alone")
    menu = term.waits("/compact", "the commands offered")
    term.keys("\x15", "Ctrl+U")
    term.leaves("/compact", "the commands gone")
    check("clipboard: what is typed at once and is no image is written as it is, and a key alone still does what it does",
          written and menu, term.text())

    escaped = str(thumbnail).replace(" ", "\\ ")
    paste(f"{escaped} ", "a screenshot's thumbnail dropped on the terminal: its path, spaces escaped")
    shot = term.until(lambda: len(chips()[0]) == 2, "the screenshot on the message box")
    check("clipboard: a screenshot's thumbnail dropped attaches it by its name, the narrow space before PM kept",
          shot and chips()[0][1] == thumbnail.name and box() == "Ask Unharnessed", (chips(), box()))
    gone = staging / "Screenshot 2026-11-23 at 8.51.40\u202fPM.png"
    paste(str(gone).replace(" ", "\\ "), "a thumbnail macOS has already put away")
    told = term.waits("was gone before it could be read", "why it was not attached")
    check("clipboard: a thumbnail macOS put away is said to be gone, its path not written",
          told and len(chips()[0]) == 2 and box() == "Ask Unharnessed", (chips(), box()))
    term.keys("\r", "Enter")
    answered = term.waits("Answer 2.", "its answer")
    request = sent(record, "session/prompt")[1]["params"]
    images = [base64.b64decode(block["data"]) for block in request["prompt"] if block["type"] == "image"]
    check("clipboard: the dropped images go whole, by their names",
          answered and images == [quoted, held]
          and request["_meta"] == {"unharnessedTui": {"images": ["it's.png", thumbnail.name]}}, request.get("_meta"))
    term.keys("\x11", "Ctrl+Q")
    check("clipboard: the session leaves", term.exit(timeout=60) == 0 and term.given_back())


def scenario_market(state):
    trace = Trace("market")
    # The real providers, none reachable, and the Marketplace as the agent last saved it: the catalog the
    # repository's tests read, in the agent's own cache file.
    home = Home("market", faux=False)
    record = OUT / "live-market.rec"
    catalog = json.loads((ROOT / "crates/uh-core/tests/fixtures/mcp/plugins-catalog.v2.json").read_text())
    (home.base / "agent" / "mcp-service-catalog.v2.json").write_text(json.dumps({
        "url": "https://raw.githubusercontent.com/PrimeIntellect-ai/prime-agent-catalog/main/plugins/catalog.v2.json",
        "scope": "public", "fetchedAt": int(time.time() * 1000), "payload": catalog}))
    keys = {provider: {"type": "api_key", "key": "sk-e2e-not-a-key"} for provider in ("anthropic", "openai")}
    (home.base / "agent" / "auth.json").write_text(json.dumps(keys))
    term = Terminal(trace, home, [], record=record, rows=44)
    term.waits("Ask Unharnessed", "once the session is open")
    term.keys("/settings\r", "/settings")
    term.waits("Compaction", "the settings")
    term.click_on("MCP", "on the MCP tab")
    opened = term.waits("Marketplace", "the MCP tab")
    text = term.settles("the Marketplace", 1.0)
    listed = received_result(record, "_unharnessed/mcp/list")[-1]["servers"]
    services = sorted((server for server in listed if server["source"] == "catalog"), key=lambda server: server["label"].lower())
    check("market: the Marketplace lists the catalog's services, A to Z, each with what adds it, under a search of them all",
          opened and len(services) > 20 and f"Search {len(services)} services" in text
          and re.search(rf"{re.escape(services[0]['label'])} .* Add$", text, re.M) is not None
          and text.index(services[0]["label"]) < text.index(services[1]["label"]), text)
    token = next(server for server in services if server.get("token"))
    term.keys(token["label"].lower(), "a service's name")
    found = term.until(lambda: services[0]["label"] not in term.text() and token["label"] in term.text(), "the services searched")
    term.keys("\r", "Enter")
    asked = term.waits("Cancel", "the token it is added by")
    check("market: what is typed searches the services; one added by a token opens on its field, named as the service names it",
          found and asked and token["token"] in term.text(), (token, term.text()))
    term.keys("tok-e2e-not-a-token", "a token")
    typed = term.settles("the token written", 0.5)
    term.keys("\r", "Enter")
    added = term.waits(f"{token['label']}   Signed in", "the service among the user's servers", timeout=60)
    text = term.settles("the service added", 0.5)
    asked = sent(record, "_unharnessed/mcp/set_token")
    check("market: a token shows as dots while it is written, and is not in the recording",
          "•" * 19 in typed and "tok-e2e-not-a-token" not in typed and "tok-e2e-not-a-token" not in Path(record).read_text()
          and asked[0]["params"] == {"id": token["id"], "token": "<redacted>"}, (typed, asked[0]["params"]))
    check("market: once the agent took the token, the service leaves the Marketplace for the user's servers, and a restart applies it",
          added and len(asked) == 1 and "No service matches." in text and "Cancel" not in text
          and "Some changes apply when the agent restarts." in text, text)
    term.click_on(f"{token['label']}   Signed in", "on the service")
    opened = term.waits("Sign out", "what the service opens on")
    term.keys("\r", "Enter")
    out = term.waits(f"{token['label']}  ", "the service back in the Marketplace")
    text = term.settles("the service signed out of", 0.5)
    outs = [frame["params"]["id"] for frame in sent(record, "_unharnessed/mcp/logout")]
    check("market: a service of the user's opens on its sign-out; signed out of, it goes back to the Marketplace",
          opened and out and outs == [token["id"]] and "Signed in" not in text
          and re.search(rf"{re.escape(token['label'])} .* Add$", text, re.M) is not None, text)
    oauth = next(server for server in services if server["signIn"] and not server.get("token"))
    term.keys("\x15" + oauth["label"].lower(), "the search emptied, another service's name")
    term.until(lambda: token["label"] not in term.text() and oauth["label"] in term.text(), "the services searched again")
    term.click_on(oauth["label"], "on the service")
    failed = term.waits("Try again", "its sign-in, which cannot reach it", timeout=90)
    text = term.settles("the sign-in's screen", 0.5)
    logins = [frame["params"]["providerId"] for frame in sent(record, "_unharnessed/providers/login")]
    check("market: a service added by a sign-in starts it at once, on a screen of its own under the service's name",
          failed and logins == [f"mcp:{oauth['id']}"] and f"← {oauth['label']}" in text, text)
    term.keys("\x1b", "Esc")
    back = term.waits("Marketplace", "the Marketplace back")
    check("market: leaving the sign-in comes back to the Marketplace, the service still to add",
          back and oauth["label"] in term.text() and "Your servers" in term.text(), term.text())
    term.keys("\x11", "Ctrl+Q")
    check("market: the session leaves", term.exit(timeout=60) == 0 and term.given_back())


# What the faux model answers in the compaction's scenario: two answers; then, for the compaction, the summary and
# where the turn it cut stood; then an answer after it.
COMPACT = {"responses": [
    {"content": [{"type": "text", "text": "First answer. " * 30}]},
    {"content": [{"type": "text", "text": "Second answer. " * 30}]},
    {"content": [{"type": "text", "text": "## Goal\n- Say hello.\n\n## Progress\n- The user said hi; the agent answered."}]},
    {"content": [{"type": "text", "text": "Where the turn stood."}]},
    {"content": [{"type": "text", "text": "Third answer."}]},
], "modelName": "Faux Model"}


def scenario_compact(state):
    trace = Trace("compact")
    home = Home("compact", script=COMPACT)
    # The agent keeps next to nothing of the conversation as it is: a short one is worth compacting.
    (home.base / "agent" / "settings.json").write_text(json.dumps({"compaction": {"keepRecentTokens": 1}}))
    record = OUT / "live-compact.rec"
    term = Terminal(trace, home, [], record=record)
    term.waits("Faux Model", "once the session is open")
    for message, answer in (("hi", "First answer."), ("again", "Second answer.")):
        term.keys(f"{message}\r", f"{message}, then Enter")
        term.waits(answer, "its answer", timeout=60)
        term.settles("the turn over", 1.0)
    term.keys("/compact\r", "/compact")
    done = term.waits("tokens summarized", "the conversation compacted", timeout=60)
    text = term.settles("the compaction over", 1.5)
    ended = [frame["params"]["update"]["_meta"]["unharnessed"]["compaction"] for frame in received(record, "session/update")
             if "compaction" in frame["params"]["update"].get("_meta", {}).get("unharnessed", {})]
    tokens = ended[0]["tokensBefore"] if ended else 0
    shown = f"{tokens / 1000:.1f}k" if 1000 <= tokens < 10000 else str(tokens)
    line = re.search(rf"^ *─+  Compacted · {re.escape(shown)} tokens summarized  ─+$", text, re.M)
    check("compact: /compact ends on a line across the conversation: Compacted, and how many tokens it summarized",
          done and len(ended) == 1 and line is not None and "Say hello." not in text and "Stopped" not in text, text)
    term.click_on("Compacted", "on the line")
    opened = term.waits("Say hello.", "the summary under it")
    text = term.text()
    check("compact: a click on the line opens the summary under it",
          opened and "The user said hi; the agent answered." in text and "## Goal" not in text, text)
    term.click_on("Compacted", "on the line again")
    closed = term.leaves("Say hello.", "the summary folded")
    check("compact: another click folds it", closed and "tokens summarized" in term.text(), term.text())
    term.keys("go on\r", "a message after it")
    went = term.waits("Third answer.", "its answer", timeout=60)
    check("compact: the conversation goes on under the line", went and "tokens summarized" in term.text(), term.text())
    term.keys("\x03", "Ctrl+C on an empty message box")
    term.exit()
    resume = re.search(r"unharnessed --resume ([0-9a-f-]{36})", term.text())
    if resume is None:
        check("compact: leaving, it says how to come back", False, term.text())
        return
    term = Terminal(trace, home, ["--resume", resume.group(1)])
    back = term.waits("Third answer.", "the conversation back")
    text = term.settles("the conversation back, settled", 1.5)
    check("compact: coming back, the line is where it was, its turn over, not stopped",
          back and re.search(rf"─+  Compacted · {re.escape(shown)} tokens summarized  ─+", text) is not None
          and "Stopped" not in text, text)
    term.click_on("Compacted", "on the line")
    opened = term.waits("Say hello.", "the summary under it")
    check("compact: it opens on the summary there too", opened, term.text())
    term.keys("\x11", "Ctrl+Q")
    check("compact: the session leaves", term.exit(timeout=60) == 0 and term.given_back())


def scenario_refusals(state):
    trace = Trace("refusals")
    home = Home("refusals")
    term = Terminal(trace, home, ["--resume", "no-such-session"])
    code = term.exit()
    check("refusals: an agent that cannot start says why on the terminal, given back",
          code == 1 and "No session found" in term.text() and term.given_back(), term.text().strip())
    term = Terminal(trace, home, [], record=home.base / "absent" / "x.rec")
    code = term.exit()
    check("refusals: a recording that cannot be written is refused before the screen is taken",
          code == 1 and "cannot record" in term.text() and b"\x1b[?1049h" not in term.raw, term.text().strip())
    # A terminal to read from, none to draw on.
    reader, writer = pty.openpty()
    trace.note(f"$ cd {home.work} && unharnessed | cat   # its input a pseudo-terminal")
    done = subprocess.run([BIN], cwd=home.work, env=home.env, stdin=writer, capture_output=True, text=True, timeout=30)
    os.close(reader)
    os.close(writer)
    trace.note(done.stdout + done.stderr + f"[exit {done.returncode}]")
    check("refusals: without a terminal to draw on it points at the headless modes",
          done.returncode == 1 and done.stdout == "" and "needs a terminal" in done.stderr and "--print" in done.stderr, done.stderr)


only = os.environ.get("UNHARNESSED_E2E_ONLY")
state = {}
for scenario in (scenario_session, scenario_resume, scenario_launch, scenario_providers, scenario_composer,
                 scenario_pickers, scenario_history, scenario_kernel, scenario_compact, scenario_goal, scenario_slash,
                 scenario_settings, scenario_servers, scenario_instructions,
                 scenario_market, scenario_rewind, scenario_images, scenario_clipboard,
                 scenario_refusals):
    name = scenario.__name__[len("scenario_"):]
    if only and only != name:
        continue
    try:
        scenario(state)
    except Exception as error:  # noqa: BLE001
        check(f"{name}: ran to its end", False, repr(error))

print(f"\n{len(FAILED)} failed" if FAILED else "\nall passed")
sys.exit(1 if FAILED else 0)
