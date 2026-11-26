#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = ["pyte", "wcwidth"]
# ///
"""End-to-end checks of the terminal UI on the real binary
(`unharnessed --tui-replay`, crates/uh-tui): a recorded conversation drawn
at given moments (the reasoning's window, a running cell, the kernel's
panel, the answer streaming in, the turn folded, the message box), with the
cells open, in its colours, its surfaces square, on a narrow screen and on
one too small; a recording full of what must never reach a terminal (escape
sequences, controls, wide and combining characters); one of servers the
panel is asked to stop, the agent refusing or not; and the replay in a
pseudo-terminal, its output read by a terminal emulator: what a terminal
shows is the frame drawn without one, the keys work, the mouse opens and
closes what folds and scrolls, the colours are the extension's (24 bits,
else the 256-colour palette, NO_COLOR or not), and the terminal is given
back.

usage: uv run scripts/unharnessed-tui-e2e.py <out-dir> [recording]

The recording defaults to crates/uh-tui/recordings/demo.out
(scripts/unharnessed-tui-record.py writes one); the moments checked are
that recording's. Each scenario writes <out-dir>/<name>.out: each command,
then what it printed (for the pseudo-terminal, what the emulator's screen
showed). UNHARNESSED_BIN picks the binary (default target/debug/unharnessed).
Exits non-zero when a check fails.
"""
import fcntl
import json
import os
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import termios
import time
import unicodedata
from pathlib import Path

import pyte
from wcwidth import wcswidth

ROOT = Path(__file__).resolve().parent.parent
BIN = os.environ.get("UNHARNESSED_BIN", str(ROOT / "target/debug/unharnessed"))
OUT = Path(sys.argv[1]).resolve()
OUT.mkdir(parents=True, exist_ok=True)
DEMO = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else ROOT / "crates/uh-tui/recordings/demo.out"
ENV = {name: value for name, value in os.environ.items() if name not in ("NO_COLOR", "COLORTERM")}
ENV["TERM"] = "xterm-256color"
FAILED = []

# The moments checked, in the demo recording's seconds: the model reasons; its first cell runs; the server
# is up and the agent writes the next cell; cells have run in a row and it reasons again; the answer
# streams; the turn has just ended and its work folds; all is settled, past the recording's end.
THINKING, RUNNING, SERVING, AGAIN, ANSWERING, FOLDING, DONE = 1.5, 5.6, 12.2, 18.1, 20.75, 24.5, 27.6

# The extension's colours (vscode/webview/styles.css, the dark theme).
BG, RAISED, BUBBLE, INK, ACCENT = "1a1a1b", "222223", "2a2a2c", "ececee", "86defc"


def check(name, ok, detail=""):
    print(("PASS  " if ok else "FAIL  ") + name + ("" if ok or detail == "" else f" ({detail})"))
    if not ok:
        FAILED.append(name)


def sgr(colour):
    return ";".join(str(int(colour[at:at + 2], 16)) for at in (0, 2, 4))


class Trace:
    """A scenario's trace: each command and what it printed."""

    def __init__(self, name):
        self.file = open(OUT / f"{name}.out", "w")

    def note(self, text):
        self.file.write(text.rstrip("\n") + "\n")
        self.file.flush()

    def replay(self, recording, *args):
        """`unharnessed --tui-replay <recording> <args>` without a terminal: (exit code, stdout, stderr)."""
        command = [BIN, "--tui-replay", str(recording), *args]
        self.note("$ " + " ".join(["unharnessed", *command[1:]]))
        done = subprocess.run(command, capture_output=True, text=True, env=ENV, stdin=subprocess.DEVNULL, timeout=60)
        self.note(done.stdout + done.stderr + f"[exit {done.returncode}]")
        return done.returncode, done.stdout, done.stderr

    def frames(self, recording, moments, *args):
        """The screen at each moment, as its rows."""
        code, out, err = self.replay(recording, "--at", ",".join(str(m) for m in moments), *args)
        if code != 0:
            raise RuntimeError(f"the replay failed: {err}")
        parts = re.split(r"^== .*\n", out, flags=re.M)[1:]
        return dict(zip(moments, (part.split("\n")[:-1] for part in parts)))


def plain(rows):
    """Rows drawn with their colours, without them."""
    return [re.sub(r"\x1b\[[0-9;]*m", "", row) for row in rows]


def beside_rail(rows, width=120):
    """Rows without the folded panel's rail, which stays put on the right while the conversation scrolls."""
    return [row[:width - 4].rstrip() for row in rows]


def scenario_frames():
    trace = Trace("frames")
    size = (120, 34)
    frames = trace.frames(DEMO, [THINKING, RUNNING, SERVING, AGAIN, ANSWERING, FOLDING, DONE], "--size", "120x34")
    text = {moment: "\n".join(rows) for moment, rows in frames.items()}
    check("frames: every frame has the screen's rows, none wider than the screen",
          all(len(rows) == size[1] and all(wcswidth(row) <= size[0] for row in rows) for rows in frames.values()))
    check("frames: the wordmark and the user's message show from the start",
          "Unharnessed" in frames[THINKING][0] and "Serve the pages in site/" in text[THINKING])
    at = next((at for at, row in enumerate(frames[THINKING]) if "Thinking 1s" in row), None)
    check("frames: the reasoning under way shows its seconds, and its words under them",
          at is not None and frames[THINKING][at + 1][:80].strip() == "" and frames[THINKING][at + 2].lstrip().startswith("The pages are in site/."))
    check("frames: the panel says what it is for while the kernel is empty",
          "Kernel" in text[THINKING] and "Running servers" not in text[THINKING])
    check("frames: reasoning that is over folds into one line, under its own mark",
          "✦  Thought for 2s" in text[RUNNING] and "The pages are in site/." not in text[RUNNING])
    check("frames: a running cell shows its telling line",
          'Running pages = sorted(Path("site").glob("*.html"))' in text[RUNNING])
    check("frames: a Python cell that ran shows Python's mark and its telling line",
          any(row.startswith('  py Ran pages = sorted(Path("site").glob("*.html"))') for row in frames[SERVING]))
    check("frames: a cell that runs a command shows a prompt and the command",
          any(row.startswith("  >_ Ran python3 -u -m http.server 8765 --directory site") for row in frames[SERVING]))
    check("frames: the panel lists the server with its port and what stops it, a row under its title; its last lines stay folded",
          re.search(r"Running servers\n.*\n.*● python3 -u.* :8765 +\d+s ×\n", text[SERVING]) is not None
          and "Serving HTTP on" not in text[SERVING])
    check("frames: the panel lists the variables a row under their title: the name, and the type on the right",
          re.search(r"Agent's REPL variables +3\n.*\n.* pages +list\n.* sizes +dict\n.* server +BashHandle\n", text[SERVING]) is not None)
    check("frames: a variable's value is not on its row",
          not any(part in text[AGAIN] for part in ("3 items", "3 keys", "(name: str)", "'docs.html'")))
    check("frames: the pause between two cells says the agent works", "Working" in text[SERVING])
    # A cell runs after another of the same run, before they fold into one line.
    both = trace.frames(DEMO, [SERVING + 1.8], "--size", "120x34", "--no-panel")[SERVING + 1.8]
    ran = next((at for at, row in enumerate(both) if row.startswith("  >_ Ran python3 -u -m http.server")), None)
    check("frames: a running cell keeps a row of the page from the cell before it",
          ran is not None and both[ran + 1] == "" and re.match(r"  [⠀-⣿]  Running def fetch\(name: str\)", both[ran + 2]) is not None,
          both[ran:ran + 3] if ran is not None else both)
    check("frames: cells in a row fold into one line once the agent thinks again",
          "Ran 3 Python cells" in text[AGAIN] and "Thinking" in text[AGAIN])
    check("frames: the answer streams in as Markdown, its marks never shown",
          "Page weights" in text[ANSWERING] and "• docs.html is the heavy one" in text[ANSWERING] and "**" not in text[ANSWERING])
    # On a screen tall enough to hold the turn whole while its work is still open.
    folding = "\n".join(trace.frames(DEMO, [FOLDING], "--size", "120x60")[FOLDING])
    check("frames: the turn's work is folding into one line right after it ended",
          "Worked for 24s · 4 cells" in folding and "Thought for 2s" in folding)
    check("frames: a table's columns line up, its head over a rule",
          re.search(r"Page +On disk +Share\n +─{34}\n +docs\.html +13,794 B +91%\n +index\.html +1,301 B +9%\n +pricing\.html +30 B +under 1%\n",
                    folding) is not None and "|" not in folding)
    # The turn whole, on a screen tall enough for it.
    done = trace.frames(DEMO, [DONE], "--size", "120x44", "--no-panel")[DONE]
    whole = "\n".join(done)
    check("frames: once done, the turn is its work's line and its answer",
          "Worked for 24s · 4 cells" in whole and "Thought for" not in whole
          and "Tell me if you want docs.html split into sections." in whole)
    check("frames: a fenced block shows its language, then its code a row under it",
          re.search(r"\n\n +python\n\n +sizes\[heaviest\] +# bytes on disk\n +timings\[heaviest\] .*\n\n\n +Page ", whole) is not None)
    check("frames: a row of the page stays between two blocks, under a heading too",
          re.search(r"still running\.\n\n +Page weights\n\n +• docs\.html is the heavy one", whole) is not None
          and re.search(r"under 1%\n\n +Tell me if you want", whole) is not None and "\n\n\n\n" not in whole[whole.index("Worked"):whole.index("Tell me")])
    at = next((at for at, row in enumerate(frames[DONE]) if "Ask Unharnessed" in row), None)
    check("frames: the message box asks for a message; its bar has the model, its effort and the context's use",
          at is not None and re.search(r"Faux Model +━━━━━ Medium +1\.6k of 100k context$", frames[DONE][at + 2]) is not None
          and "↑" not in text[DONE] and "%" not in frames[DONE][at + 2], frames[DONE][at + 2] if at else None)
    check("frames: while the agent works, the bar shows what stops it, before the context's use",
          any(re.search(r"Faux Model +━━━━━ Medium +■  1\.1k of 100k context$", row) for row in frames[RUNNING])
          and not any("■" in row for row in frames[DONE]))
    check("frames: the screen carries no instructions for the keys",
          not any(words in "\n".join(text.values()) for words in ("Replay:", "←→", "q quit")))
    check("frames: every surface is whole cells: no half block, no cut corner",
          not any(ch in rows_text for rows_text in text.values() for ch in "▗▖▝▘▄▀▛▜▙▟▌▐"))
    again = trace.frames(DEMO, [ANSWERING, DONE], "--size", "120x34")
    check("frames: a moment drawn again is the same frame", again[ANSWERING] == frames[ANSWERING] and again[DONE] == frames[DONE])


def scenario_open():
    trace = Trace("open")
    frames = trace.frames(DEMO, [DONE], "--open", "--size", "120x120")
    text = "\n".join(frames[DONE])
    check("open: the work's line, the cells' and the reasoning's are open",
          all(part in text for part in ("≡  Worked for 24s · 4 cells ─", "py Ran 3 Python cells ▾", "✦  Thought for 2s")))
    check("open: a cell shows its code", "def fetch(name: str) -> tuple[int, float]:" in text)
    check("open: a cell shows what it printed", "{'docs.html': 13794, 'index.html': 1301, 'pricing.html': 30}" in text)
    check("open: a failed cell names its exception and shows its traceback",
          "py Failed heaviest = max(sizes, key=sizes.get) · AssertionError ▾" in text
          and "AssertionError: docs.html is 13,794 bytes" in text)
    check("open: the reasoning shows whole", "leave the server running since the" in text)
    check("open: only the cells' lines carry a chevron: the reasoning's and the work's have none",
          not any(mark in row for row in frames[DONE] for mark in "▸▾" if "Thought for" in row or "Worked for" in row)
          and sum("▾" in row for row in frames[DONE]) == 5, [row for row in frames[DONE] if "▾" in row or "▸" in row])
    check("open: the open cells' surfaces are whole cells too", not any(ch in text for ch in "▗▖▝▘▄▀▛▜▙▟▌▐"))


def inks(row):
    """The first channel of each colour a row drawn with --ansi writes characters in (not the empty cells')."""
    runs = re.findall(r"\x1b\[0;38;2;(\d+);[0-9;]*m([^\x1b]*)", row)
    return {int(red) for red, text in runs if text.strip()}


def colours(rows, key):
    """The colours of one kind (38: text, 48: behind) a frame drawn with --ansi uses."""
    return set(re.findall(rf"{key};2;(\d+;\d+;\d+)", "\n".join(rows)))


def scenario_colours():
    trace = Trace("colours")
    frames = trace.frames(DEMO, [SERVING, ANSWERING, DONE], "--ansi", "--size", "120x34")
    behind = colours(frames[SERVING], 48)
    check("colours: the page, the panel, the message box and the user's message are the extension's greys",
          {sgr(BG), sgr(RAISED), sgr(BUBBLE)} <= behind, behind)
    check("colours: the page's grey is behind every row's first cell",
          all(row.startswith(f"\x1b[0;38;2;{sgr(INK)};48;2;{sgr(BG)}") for row in frames[SERVING]))
    check("colours: the cyan is there, the ink too", {sgr(ACCENT), sgr(INK)} <= colours(frames[SERVING], 38))
    # The streaming wave: the words just arrived are between the page's grey and the ink.
    # Without the panel: the row holds the answer alone.
    frames = trace.frames(DEMO, [ANSWERING, DONE], "--ansi", "--no-panel", "--size", "120x34")
    row = next(row for row in frames[ANSWERING] if "index.html" in plain([row])[0])
    greys = {int(c.split(";")[0]) for c in colours([row], 38)}
    check("colours: the answer's newest words are coming in, between the page and the ink",
          len({g for g in greys if 26 < g < 236}) >= 3, sorted(greys))
    row = next(row for row in frames[DONE] if "index.html" in plain([row])[0])
    greys = {int(c.split(";")[0]) for c in colours([row], 38)}
    # 97: the bullet, faint.
    check("colours: once settled, they are all in", {g for g in greys if 26 < g < 236} == {97}, sorted(greys))
    frames.update(trace.frames(DEMO, [THINKING, SERVING], "--ansi", "--no-panel", "--size", "120x34"))
    row = next(row for row in frames[SERVING] if "py Ran pages" in plain([row])[0])
    check("colours: Python's mark is in Python's blue, both letters",
          re.search(r"38;2;75;139;190;48;2;[0-9;]+mpy\x1b", row) is not None
          and not any(r > 200 and g > 180 and b < 90 for r, g, b in (map(int, c.split(";")) for c in colours([row], 38))),
          sorted(colours([row], 38)))
    late = THINKING + 0.45
    frames.update(trace.frames(DEMO, [late], "--ansi", "--no-panel", "--size", "120x34"))
    at = next(at for at, row in enumerate(plain(frames[late])) if "Thinking" in row)
    last = next(row for row in reversed(frames[late][at + 2:at + 7]) if plain([row])[0].strip())
    greys = inks(last)
    # 144: the reasoning's grey, once a word has been in for a while.
    check("colours: the reasoning's newest words come in as the answer's do: from the page up past its grey",
          len({g for g in greys if 26 < g < 144}) >= 3 and max(greys) > 200, sorted(greys))
    # The row runs the page's width: its newest words came in a tenth of a second ago.
    cooled = late + 0.1
    frames.update(trace.frames(DEMO, [cooled], "--ansi", "--no-panel", "--size", "120x34"))
    first = inks(frames[cooled][at + 2])
    check("colours: then they cool to the reasoning's grey", first == {144}, sorted(first))
    check("colours: nothing but colour and style reaches the output",
          all(re.sub(r"\x1b\[[0-9;]*m", "", row).isprintable() for rows in frames.values() for row in rows))


def scenario_sizes():
    trace = Trace("sizes")
    narrow = trace.frames(DEMO, [ANSWERING], "--size", "80x24")[ANSWERING]
    text = "\n".join(narrow)
    check("sizes: a narrow screen keeps the conversation and drops the panel",
          "Page weights" in text and "REPL variables" not in text and len(narrow) == 24
          and all(wcswidth(row) <= 80 for row in narrow))
    check("sizes: its message box keeps the model, the effort, the context's use and the button",
          any(re.search(r"^ +Faux Model +━━━━━ Medium +■  1\.4k of 100k context$", row) for row in narrow), narrow[-4:])
    slim = trace.frames(DEMO, [ANSWERING], "--size", "44x30")[ANSWERING]
    check("sizes: on a slim screen the bar lets go of the context's use, not of the model, the effort nor the stop",
          any(re.search(r"^ +Faux Model +━━━━━ Medium +■$", row) for row in slim) and all(wcswidth(row) <= 44 for row in slim),
          slim[-4:])
    off = trace.frames(DEMO, [ANSWERING], "--no-panel", "--size", "120x34")[ANSWERING]
    check("sizes: without the panel the conversation takes its room",
          "REPL variables" not in "\n".join(off) and "Page weights" in "\n".join(off))
    small = trace.frames(DEMO, [ANSWERING], "--size", "30x8")[ANSWERING]
    check("sizes: a screen too small says so", small[0] == "The window is too small." and len(small) == 8)
    one = trace.frames(DEMO, [ANSWERING], "--size", "1x1")[ANSWERING]
    check("sizes: a screen of one cell draws", len(one) == 1 and wcswidth(one[0]) <= 1)


# What must never reach a terminal, and what breaks a row's width.
ESCAPES = "\x1b[31mred\x1b[0m \x1b]0;owned\x07title \x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\ \x1b[2J\x1b[Hhome bell\x07 nul\x00 c1\x9b31m"
WIDE = "漢字かな 🙂 e\u0301te\u0301 \u202egnp.exe\u202c"


def hostile(path):
    """A recording whose every text carries escapes, controls and wide characters."""
    session = "s"
    meta = lambda turn=1, **more: {"unharnessed": {"promptTurnId": turn, "phase": "event", **more}}
    update = lambda kind, **fields: {"jsonrpc": "2.0", "method": "session/update",
                                     "params": {"sessionId": session, "update": {"sessionUpdate": kind, **fields}}}
    chunk = lambda kind, text: update(kind, messageId="m1", content={"type": "text", "text": text}, _meta=meta())
    frames = [
        (">", {"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/", "mcpServers": []}}),
        ("<", {"jsonrpc": "2.0", "id": 1, "result": {"sessionId": session, "configOptions": [
            {"id": "model", "currentValue": "m", "options": [{"value": "m", "name": f"Model {ESCAPES}"}]}]}}),
        (">", {"jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {
            "sessionId": session, "prompt": [{"type": "text", "text": f"{WIDE} {ESCAPES}\n\tsecond line " + "x" * 300}]}}),
        ("<", update("session_info_update", _meta=meta(turnStart={"origin": "prompt"}))),
        ("<", update("session_info_update", _meta=meta(goal={
            "status": "blocked", "objective": f"aim {ESCAPES}\n{WIDE}", "lastReason": f"why {ESCAPES}\n{WIDE} " + "z" * 200,
            "createdAt": 1000, "updatedAt": 3724000}))),
        ("<", chunk("agent_thought_chunk", f"thinking {ESCAPES} {WIDE}")),
        ("<", chunk("agent_message_chunk", f"# Title {ESCAPES}\n\n- item {WIDE}\n- `code {ESCAPES}`\n\n")),
        ("<", chunk("agent_message_chunk", "```sh\nprogress 10%\rprogress 100%\n\tindented " + "y" * 200 + "\n```\n")),
        ("<", chunk("agent_message_chunk", f"| a | b |\n|---|---|\n| {WIDE} | {ESCAPES} |\n")),
        ("<", update("tool_call", toolCallId="t1", title="Python cell", status="in_progress",
                     rawInput={"code": f"print('{ESCAPES}')  # {WIDE}\nx = '\x1b[1mbold'"}, _meta=meta())),
        ("<", update("tool_call_update", toolCallId="t1", status="completed",
                     content=[{"type": "content", "content": {"type": "text", "text": f"{ESCAPES}\r\n{WIDE}\nloading 1%\rloading 100%"}}],
                     _meta=meta())),
        ("<", {"jsonrpc": "2.0", "method": "_unharnessed/kernel/changed", "params": {"sessionId": session, "variables": [
            {"name": f"v\x1b[31m{WIDE}", "type": f"t\x07{WIDE}", "rev": "1", "value": ESCAPES},
            {"name": "n" * 80, "type": "T" * 80, "rev": "2", "signature": "(" + "a, " * 80 + ")"}]}}),
        ("<", {"jsonrpc": "2.0", "method": "_unharnessed/kernel/changed", "params": {"sessionId": session, "commands": [
            {"id": "c1", "command": f"serve {ESCAPES} {WIDE}", "startedAt": "2026-01-01T00:00:00+00:00",
             "status": "running", "exitCode": None, "killed": False, "ports": [8000, 8001, 8002]}]}}),
        (">", {"jsonrpc": "2.0", "id": 3, "method": "_unharnessed/kernel/tail", "params": {"sessionId": session, "id": "c1", "lines": 7}}),
        ("<", {"jsonrpc": "2.0", "id": 3, "result": {"tail": f"{ESCAPES}\nbuilding 3%\rbuilding 99%\n{WIDE}"}}),
        (">", {"jsonrpc": "2.0", "id": 4, "method": "_unharnessed/kernel/preview", "params": {"sessionId": session, "name": f"v\x1b[31m{WIDE}"}}),
        ("<", {"jsonrpc": "2.0", "id": 4, "result": {"preview": f"{ESCAPES}\nreading 2%\rreading 98%\n{WIDE}", "style": "code", "truncated": True}}),
        (">", {"jsonrpc": "2.0", "id": 5, "method": "_unharnessed/kernel/preview", "params": {"sessionId": session, "name": "n" * 80}}),
        ("<", {"jsonrpc": "2.0", "id": 5, "error": {"code": -32603, "message": "Internal error", "data": {"details": f"refused {ESCAPES}"}}}),
        ("<", {"jsonrpc": "2.0", "id": 2, "result": {"stopReason": "end_turn"}}),
    ]
    lines = ["$ written by scripts/unharnessed-tui-e2e.py"]
    lines += [f"{0.5 + at * 0.1:7.3f} {arrow} {json.dumps(frame)}" for at, (arrow, frame) in enumerate(frames)]
    path.write_text("\n".join(lines) + "\n")


def scenario_hostile():
    trace = Trace("hostile")
    recording = OUT / "hostile.recording"
    hostile(recording)
    sizes = [(120, 60), (61, 40), (44, 30)]
    drawn = {}
    for columns, rows in sizes:
        for extra in ([], ["--open"]):
            frames = trace.frames(recording, [1.0, 1.75, 5.0], "--ansi", "--size", f"{columns}x{rows}", *extra)
            drawn[(columns, rows, bool(extra))] = frames
    every = [(size, row) for size, frames in drawn.items() for rows in frames.values() for row in rows]
    check("hostile: the only escape sequences in the output are the frame's own colours",
          all(re.fullmatch(r"(?:\x1b\[0(?:;[0-9]+)*m|[^\x1b])*", row) for _, row in every))
    bare = [(size, re.sub(r"\x1b\[[0-9;]*m", "", row)) for size, row in every]
    check("hostile: no control, no text-reordering character is in the output",
          all(unicodedata.category(ch) not in ("Cc", "Cf") or ch == "\u200d" for _, row in bare for ch in row),
          sorted({hex(ord(ch)) for _, row in bare for ch in row if unicodedata.category(ch) in ("Cc", "Cf")}))
    wrong = [(size[:2], wcswidth(row)) for size, row in bare if wcswidth(row) != size[0]]
    check("hostile: every row is exactly as wide as the screen", not wrong, wrong[:5])
    text = "\n".join(plain(drawn[(120, 60, True)][5.0]))
    check("hostile: the texts themselves are there, without what was around them",
          all(part in text for part in ("red title link", "漢字かな 🙂 e\u0301te\u0301", "progress 100%", "loading 100%"))
          and "progress 10%" not in text and "owned" not in text, text[:400])
    check("hostile: the panel keeps the ports it has room for, two at the most, and cuts what is too long",
          text.count(":8000") == 1 and ":8002" not in text and "…" in text)
    check("hostile: both variables are listed, whatever their names hold",
          re.search(r"REPL variables +2 *\n", text) is not None and re.search(r"v漢字かな 🙂\S* +t漢字かな", text) is not None
          and re.search(r" n+… +T+… *\n", text) is not None,
          [row[76:] for row in text.split("\n") if "漢字" in row[76:] or "nnn" in row])
    bar = next((row for row in text.split("\n") if "Model red" in row), "")
    check("hostile: a model without a name of its own shows its option's, in the message box's bar", "Model red title link" in bar, bar)
    goal = [row for row in text.split("\n") if "⚑" in row]
    check("hostile: a goal shows on one row of the message box whatever it holds, cut before what ends it, its time on the bar",
          len(goal) == 1 and re.search(r"⚑ Blocked  why red title link .*漢字かな 🙂.* z+…  ×", goal[0]) is not None and "owned" not in goal[0]
          and re.search(r"1:02:03 ▶", bar) is not None and re.search(r"Goal blocked: aim red title link .*漢字かな 🙂", text) is not None, goal)

    # What the panel's rows open on comes from outside too: opened with the mouse, in a terminal.
    term = Terminal(trace, ["--from", "5", "--paused"], env={"COLORTERM": "truecolor"}, rows=60, recording=recording)
    term.waits("REPL variables")
    for words, label in (("nnnnn", "the second variable"), ("v漢字かな", "the first variable"), ("● serve", "the command")):
        term.click(100, term.find(words, 86), label)
        term.read(0.7)
    opened = "\n".join(term.settles("the panel's rows open", 0.3))
    check("hostile: a command's last lines, a variable's value and a refusal show clean once opened",
          all(part in opened for part in ("building 99%", "reading 98%", "refused red title link")) and "building 3%" not in opened
          and "reading 2%" not in opened and "owned" not in opened, opened[:300])
    term.keys(b"\x11", "ctrl+q")
    check("hostile: opened or not, nothing of it reached the terminal as a command (no title set, no bell)",
          term.exit() == 0 and term.screen.title == "" and b"\x1b]" not in term.raw and b"\x07" not in term.raw,
          (term.screen.title, term.raw.count(b"\x1b]"), term.raw.count(b"\x07")))


def stops(path):
    """A recording of two servers the panel is asked to stop: the agent refuses for the first, and stops the second."""
    session = "s"
    listed = lambda *commands: {"jsonrpc": "2.0", "method": "_unharnessed/kernel/changed",
                                "params": {"sessionId": session, "commands": list(commands)}}
    server = lambda id, port, **more: {"id": id, "command": f"serve-{id}", "startedAt": "2026-01-01T00:00:00+00:00",
                                       "status": "running", "exitCode": None, "killed": False, "ports": [port], **more}
    kill = lambda request, id: {"jsonrpc": "2.0", "id": request, "method": "_unharnessed/kernel/kill",
                                "params": {"sessionId": session, "id": id}}
    frames = [
        (0.5, ">", {"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/", "mcpServers": []}}),
        (0.6, "<", {"jsonrpc": "2.0", "id": 1, "result": {"sessionId": session}}),
        (0.7, "<", listed(server("one", 8000), server("two", 9000))),
        (2.0, ">", kill(2, "one")),
        (2.5, "<", {"jsonrpc": "2.0", "id": 2, "error": {"code": -32603, "message": "Internal error"}}),
        (3.0, ">", kill(3, "two")),
        (3.2, "<", {"jsonrpc": "2.0", "id": 3, "result": {"killed": True}}),
        (3.4, "<", listed(server("one", 8000), server("two", 9000, status="finished", exitCode=-15, killed=True,
                                                      ports=[], durationMs=2700))),
        (9.0, "<", listed(server("one", 8000))),
    ]
    lines = ["$ written by scripts/unharnessed-tui-e2e.py"]
    lines += [f"{at:7.3f} {arrow} {json.dumps(frame)}" for at, arrow, frame in frames]
    path.write_text("\n".join(lines) + "\n")


def scenario_stops():
    trace = Trace("stops")
    recording = OUT / "stops.recording"
    stops(recording)
    asked, refused, stopping, told, gone = 2.2, 2.8, 3.1, 3.6, 8.0
    frames = trace.frames(recording, [1.0, asked, refused, stopping, told, gone], "--size", "120x30")
    row = lambda moment, id: next((row[88:].strip() for row in frames[moment] if f"serve-{id}" in row), None)
    runs = lambda shown: shown is not None and re.fullmatch(r"● serve-\w+ +:\d+ +\d+s ×", shown) is not None
    check("stops: a server that runs has what stops it at its row's end", runs(row(1.0, "one")) and runs(row(1.0, "two")),
          (row(1.0, "one"), row(1.0, "two")))
    stopped = lambda shown: shown is not None and re.fullmatch(r"● serve-\w+ +stopped", shown) is not None
    check("stops: a server the panel asked to stop shows stopped before the agent answers, without what stops it",
          stopped(row(asked, "one")) and runs(row(asked, "two")), row(asked, "one"))
    check("stops: one the agent refused to stop runs on, and shows so", runs(row(refused, "one")), row(refused, "one"))
    check("stops: one the agent stopped stays shown stopped, then leaves the panel; the other stays",
          stopped(row(stopping, "two")) and stopped(row(told, "two")) and row(gone, "two") is None
          and all(runs(row(moment, "one")) for moment in (stopping, told, gone)),
          [row(moment, "two") for moment in (stopping, told, gone)])


def edits(path):
    """A recording of a message sent with images, a cell that loads some, and the conversation taken back to before
    a message: refused once, then done, and the message sent again, changed."""
    session = "s"
    update = lambda turn, body, **meta: {"jsonrpc": "2.0", "method": "session/update", "params": {  # noqa: E731
        "sessionId": session, "update": {**body, "_meta": {"unharnessed": {"promptTurnId": turn, **meta}}}}}
    info = lambda turn, **meta: update(turn, {"sessionUpdate": "session_info_update"}, **meta)  # noqa: E731
    says = lambda turn, text: update(turn, {"sessionUpdate": "agent_message_chunk", "messageId": f"m{turn}",  # noqa: E731
                                            "content": {"type": "text", "text": text}})
    prompt = lambda request, blocks, **more: {"jsonrpc": "2.0", "id": request, "method": "session/prompt",  # noqa: E731
                                              "params": {"sessionId": session, "prompt": blocks, **more}}
    done = lambda request: {"jsonrpc": "2.0", "id": request, "result": {"stopReason": "end_turn"}}  # noqa: E731
    rewind = lambda request, to: {"jsonrpc": "2.0", "id": request, "method": "_unharnessed/session/rewind",  # noqa: E731
                                  "params": {"sessionId": session, "to": to}}
    image = {"type": "image", "mimeType": "image/png", "data": "iVBORw0KGgo="}
    frames = [
        (0.5, ">", {"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/", "mcpServers": []}}),
        (0.6, "<", {"jsonrpc": "2.0", "id": 1, "result": {"sessionId": session}}),
        (1.0, ">", prompt(2, [{"type": "text", "text": "first words"}])),
        (1.1, "<", info(1, turnStart={"origin": "prompt", "rewindTo": "before-first"})),
        (1.2, "<", says(1, "First answer.")),
        (1.3, "<", done(2)),
        (2.0, ">", prompt(3, [{"type": "text", "text": "look at these"}, image, image],
                          _meta={"unharnessedTui": {"images": ["shot \x1b[31mone.png", "two.png"]}})),
        (2.1, "<", info(2, turnStart={"origin": "prompt", "rewindTo": "before-second"})),
        (2.2, "<", update(2, {"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "Python cell",
                              "rawInput": {"code": "attach_image('plot.png')"}})),
        (2.4, "<", update(2, {"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "completed",
                              "content": [{"type": "content", "content": {"type": "text", "text": "Loaded."}}]},
                          ipython={"attachments": [{"mimeType": "image/png"}, {"mimeType": "image/png"}]})),
        (2.6, "<", says(2, "Second answer.")),
        (2.7, "<", done(3)),
        (4.0, ">", rewind(4, "nowhere")),
        (4.2, "<", {"jsonrpc": "2.0", "id": 4, "error": {"code": -32602, "message": "Invalid params",
                                                         "data": {"details": "nowhere is not in the conversation"}}}),
        (6.0, ">", rewind(5, "before-second")),
        (6.2, "<", {"jsonrpc": "2.0", "id": 5, "result": {}}),
        (7.0, ">", prompt(6, [{"type": "text", "text": "look again"}])),
        (7.1, "<", info(3, turnStart={"origin": "prompt", "rewindTo": "before-second"})),
        (7.2, "<", says(3, "Third answer.")),
        (7.3, "<", done(6)),
    ]
    lines = ["$ written by scripts/unharnessed-tui-e2e.py"]
    lines += [f"{at:7.3f} {arrow} {json.dumps(frame)}" for at, arrow, frame in frames]
    path.write_text("\n".join(lines) + "\n")


def scenario_edits():
    trace = Trace("edits")
    recording = OUT / "edits.recording"
    edits(recording)
    sent, refused, back, again = 3.8, 5.0, 6.5, 12.0
    frames = trace.frames(recording, [sent, refused, back, again], "--size", "100x50", "--open", "--no-panel")
    text = {moment: "\n".join(rows) for moment, rows in frames.items()}
    check("edits: a message sent with images shows them by their names, clean of what would drive the terminal",
          re.search(r"look at these *\n *\n +▣ shot one\.png +▣ two\.png", text[sent]) is not None and "\x1b" not in text[sent], text[sent])
    check("edits: a cell that loaded images into the context says how many",
          re.search(r"py +Ran attach_image\('plot\.png'\) · 2 images loaded", text[sent]) is not None, text[sent])
    check("edits: a going back the agent refused leaves the conversation as it is",
          all(words in text[refused] for words in ("first words", "First answer.", "look at these", "Second answer.")), text[refused])
    check("edits: once the agent went back to before a message, that message and what came after are gone",
          "first words" in text[back] and "First answer." in text[back] and "look at these" not in text[back]
          and "Second answer." not in text[back] and "▣" not in text[back], text[back])
    check("edits: the message sent again, changed, takes its place",
          all(words in text[again] for words in ("First answer.", "look again", "Third answer."))
          and "look at these" not in text[again], text[again])


class Terminal:
    """The replay in a pseudo-terminal, its output read by a terminal emulator."""

    def __init__(self, trace, args, env=None, columns=120, rows=34, recording=DEMO):
        self.trace = trace
        self.screen = pyte.Screen(columns, rows)
        self.stream = pyte.ByteStream(self.screen)
        self.raw = b""
        command = [BIN, "--tui-replay", str(recording), *args]
        shown = " ".join(f"{name}={value}" for name, value in (env or {}).items())
        trace.note(f"$ {shown} unharnessed {' '.join(command[1:])}   # in a {columns}x{rows} pseudo-terminal")
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.execve(BIN, command, dict(ENV, **(env or {})))
        self.resize(columns, rows)

    def resize(self, columns, rows):
        self.screen.resize(rows, columns)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))
        os.kill(self.pid, signal.SIGWINCH)

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
            self.stream.feed(data)

    def rows(self):
        return [row.rstrip() for row in self.screen.display]

    def shows(self, wanted, label, timeout=5.0):
        """Whether the screen comes to show the rows `wanted`."""
        end = time.time() + timeout
        while self.rows() != wanted and time.time() < end:
            self.read(0.1)
        self.trace.note(f"-- the screen, {label}:\n" + "\n".join(self.rows()))
        return self.rows() == wanted

    def keys(self, keys, label):
        self.trace.note(f"-- keys: {label}")
        os.write(self.fd, keys)

    def mouse(self, what, column, row, label):
        """A mouse report (SGR, as terminals send them) at a cell, counted from 0."""
        code, last = {"press": (0, "M"), "release": (0, "m"), "move": (35, "M"), "up": (64, "M"), "down": (65, "M")}[what]
        self.trace.note(f"-- mouse: {label} ({what} at column {column}, row {row})")
        os.write(self.fd, f"\x1b[<{code};{column + 1};{row + 1}{last}".encode())

    def click(self, column, row, label):
        self.mouse("press", column, row, label)
        self.mouse("release", column, row, label)

    def find(self, words, column=0):
        """The first row that shows `words` (from `column` on), else None."""
        return next((at for at, row in enumerate(self.rows()) if words in row[column:]), None)

    def waits(self, words, timeout=5.0):
        """Reads until a row shows `words`."""
        end = time.time() + timeout
        while self.find(words) is None and time.time() < end:
            self.read(0.1)

    def settles(self, label, seconds=0.9):
        """The screen once nothing moves on it any more."""
        self.read(seconds)
        self.trace.note(f"-- the screen, {label}:\n" + "\n".join(self.rows()))
        return self.rows()

    def cell(self, row, column):
        return self.screen.buffer[row][column]

    def exit(self, timeout=5.0):
        """Its exit code, once it has left; None when it does not."""
        end = time.time() + timeout
        while time.time() < end:
            self.read(0.05)
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                code = os.waitstatus_to_exitcode(status)
                self.trace.note(f"[exit {code}]")
                return code
        os.kill(self.pid, signal.SIGKILL)
        os.waitpid(self.pid, 0)
        self.trace.note("[killed: it did not leave]")
        return None


def gave_back(raw):
    """Left the alternate screen it entered, and showed the cursor again after hiding it."""
    return (raw.find(b"\x1b[?1049h") >= 0 and raw.rfind(b"\x1b[?1049l") > raw.find(b"\x1b[?1049h")
            and raw.rfind(b"\x1b[?25h") > raw.rfind(b"\x1b[?25l"))


def scenario_terminal():
    trace = Trace("terminal")
    want = lambda *args: trace.frames(DEMO, [ANSWERING], *args)[ANSWERING]
    term = Terminal(trace, ["--from", str(ANSWERING), "--paused"], env={"COLORTERM": "truecolor"})
    check("terminal: what the terminal shows is the frame drawn without one",
          term.shows(want("--size", "120x34"), "paused while the answer streams"))
    said = term.rows()[3].index("Serve the pages")
    check("terminal: the page, the wordmark and the user's message have the extension's colours",
          (term.cell(0, 0).bg, term.cell(0, 2).fg, term.cell(0, 3).fg, term.cell(3, said).bg) == (BG, ACCENT, INK, BUBBLE),
          (term.cell(0, 0).bg, term.cell(0, 2).fg, term.cell(0, 3).fg, term.cell(3, said).bg))
    check("terminal: each frame reaches the terminal whole (synchronized updates)",
          term.raw.count(b"\x1b[?2026h") >= 1 and term.raw.count(b"\x1b[?2026h") - term.raw.count(b"\x1b[?2026l") in (0, 1))
    term.keys(b"\x0f", "ctrl+o")
    check("terminal: ctrl+o opens the cells", term.shows(want("--open", "--size", "120x34"), "after ctrl+o"))
    term.keys(b"\x0b", "ctrl+k")
    check("terminal: ctrl+k hides the kernel's panel", term.shows(want("--open", "--no-panel", "--size", "120x34"), "after ctrl+k"))
    latest = term.rows()
    term.keys(b"\x1b[5~", "page up")
    term.read(0.5)
    up = term.rows()
    trace.note("-- the screen, after page up:\n" + "\n".join(up))
    # A page is the conversation's rows but one: the row that was first is now the last.
    check("terminal: page up scrolls the conversation back a page, keeping a row of it",
          up != latest and beside_rail(up)[26] == beside_rail(latest)[2] and not any("Page weights" in row for row in up),
          (up[26], latest[2]))
    term.keys(b"\x1b[H", "home")
    term.read(0.5)
    trace.note("-- the screen, after home:\n" + "\n".join(term.rows()))
    check("terminal: home goes to its start", "Serve the pages in site/" in term.rows()[3], term.rows()[3])
    term.keys(b"\x1b[F", "end")
    check("terminal: end comes back to the latest", term.shows(want("--open", "--no-panel", "--size", "120x34"), "after end"))
    term.resize(90, 30)
    check("terminal: the screen follows the window's size",
          term.shows(want("--open", "--no-panel", "--size", "90x30"), "resized to 90x30"))
    term.keys(b"\x11", "ctrl+q")
    check("terminal: ctrl+q leaves, exit code 0", term.exit() == 0)
    check("terminal: it gives the terminal back (the main screen, the cursor)", gave_back(term.raw))

    term = Terminal(trace, ["--speed", "2"], env={"COLORTERM": "truecolor"})
    term.read(0.6)
    first = "\n".join(term.rows())
    trace.note("-- the screen, playing at 2x, after 0.6 s:\n" + first)
    term.read(2.4)
    later = "\n".join(term.rows())
    trace.note("-- the screen, playing at 2x, after 3 s:\n" + later)
    check("terminal: the replay plays: the conversation moves on by itself",
          "Thinking" in first and "Running pages" in later and re.search(r"replay \d+\.\d+s of", later) is not None)
    term.keys(b"\x10", "ctrl+p")
    term.read(0.4)
    paused = term.rows()
    term.read(0.6)
    check("terminal: ctrl+p pauses it", "paused" in paused[0] and term.rows() == paused, paused[0])
    term.keys(b"\x12", "ctrl+r")
    term.read(0.4)
    check("terminal: ctrl+r goes back to the start", re.search(r"replay 0\.0s of", term.rows()[0]) is not None, term.rows()[0])
    term.keys(b"\x03", "ctrl+c")
    check("terminal: ctrl+c leaves, exit code 0", term.exit() == 0)
    check("terminal: it gives the terminal back then too", gave_back(term.raw))

    term = Terminal(trace, ["--from", str(ANSWERING), "--paused"])
    term.read(1.0)
    check("terminal: without 24-bit colours it uses the 256-colour palette",
          b"48;5;234" in term.raw and b"48;2;" not in term.raw and (term.cell(0, 0).bg, term.cell(3, said).bg) == ("1c1c1c", "262626"),
          (term.cell(0, 0).bg, term.cell(3, said).bg))
    term.keys(b"\x11", "ctrl+q")
    check("terminal: and leaves the same", term.exit() == 0)

    term = Terminal(trace, ["--from", str(ANSWERING), "--paused"], env={"COLORTERM": "truecolor", "NO_COLOR": "1"})
    term.read(1.0)
    check("terminal: NO_COLOR does not take the screen's surfaces away", term.cell(0, 0).bg == BG, term.cell(0, 0).bg)
    term.keys(b"\x11", "ctrl+q")
    term.exit()


def scenario_mouse():
    """How the mouse can fail, listed before the code: a click misses its line by a row or a column; it acts
    on what does not fold (text, the panel, the message box, reasoning under way); a fold does not move while
    the replay is paused; the line clicked slides away under the pointer; a second click does not close; in an
    open run the wrong cell opens; the wheel scrolls the wrong way or never comes back to following the end;
    the line under the pointer does not come up, or stays up once the pointer left; a report off the screen
    breaks the replay; the terminal keeps reporting the mouse once the replay has left."""
    trace = Trace("mouse")
    want = lambda moment: trace.frames(DEMO, [moment], "--no-panel", "--size", "120x44")[moment]
    term = Terminal(trace, ["--from", str(DONE), "--paused", "--no-panel"], env={"COLORTERM": "truecolor"}, rows=44)
    check("mouse: with the mouse reported, the terminal still shows the frame drawn without one",
          term.shows(want(DONE), "paused once the turn is done, its work folded"))
    folded = term.rows()
    work = term.find("Worked for 24s")
    check("mouse: the turn's work is one closed line", work is not None and "≡  Worked for 24s · 4 cells ─" in folded[work])

    dim = term.cell(work, 5).fg
    term.mouse("move", 30, work, "over the work's line")
    term.read(0.5)
    lit = term.cell(work, 5).fg
    term.mouse("move", 30, work + 2, "off it")
    term.read(0.5)
    check("mouse: the line under the pointer comes up, and goes back once the pointer left",
          int(lit, 16) > int(dim, 16) and term.cell(work, 5).fg == dim, (dim, lit, term.cell(work, 5).fg))

    for label, column, row in (("the answer's text", 10, work + 2), ("the message box", 10, len(folded) - 5),
                               ("the page's margin beside the line", 0, work), ("past the text's column", 115, work)):
        term.click(column, row, label)
    check("mouse: a click on what does not fold changes nothing", term.settles("after clicks beside") == folded)

    term.click(40, work, "the work's line")
    opened = term.settles("after a click on the work's line")
    run = term.find("py Ran 3 Python cells ▸")
    check("mouse: a click opens the turn's work, while the replay is paused",
          "≡  Worked for 24s · 4 cells ─" in opened[work] and term.find("Thought for 2s") is not None and run is not None)
    check("mouse: the line clicked stays under the pointer", term.find("Worked for 24s") == work)

    term.click(8, run, "the cells' line")
    term.settles("after a click on the cells' line")
    cell = term.find("py Ran def fetch(name: str)")
    check("mouse: a click on the cells' line lists them under it, each closed",
          term.find("py Ran 3 Python cells ▾") == run and cell is not None and cell > run
          and term.find(">_ Ran python3 -u -m http.server") is not None and term.find("with urlopen") is None)
    term.click(20, cell, "the cell that defines fetch")
    term.settles("after a click on a cell")
    doc = '"""A page\'s bytes over HTTP, and the milliseconds it took."""'
    check("mouse: a click on a cell opens that cell, under its line, and no other",
          term.find("py Ran def fetch(name: str) -> tuple[int, float]: ▾") == cell
          and term.find(doc) is not None and term.find("server = bash(") is None)
    check("mouse: a row of the page stays between the cells' line and the cells, and between a cell's line and its code",
          term.rows()[run + 1] == "" and term.rows()[cell + 1] == "" and term.find("import time") == cell + 3)
    term.click(20, cell, "the same cell")
    closed = term.settles("after a second click on it")
    check("mouse: a second click closes it", term.find(doc) is None and term.find("py Ran def fetch(name: str)") == cell)

    # The work is open and its line was kept in place: the conversation now runs past the screen's end.
    box = closed[-6:]
    term.mouse("down", 40, 10, "the wheel, down")
    term.mouse("down", 40, 10, "the wheel, down")
    down = term.settles("after the wheel went down twice", 0.5)
    check("mouse: the wheel scrolls the conversation, three rows a notch; the message box stays",
          beside_rail(down[2:31]) == beside_rail(closed[8:37]) and down[-6:] == box and down[:2] == closed[:2])
    term.mouse("up", 40, 10, "the wheel, up")
    term.mouse("up", 40, 10, "the wheel, up")
    check("mouse: and back", term.settles("after the wheel went up twice", 0.5) == closed)
    for _ in range(12):
        term.mouse("down", 40, 10, "the wheel, down")
    end = term.settles("after the wheel went down to the end", 0.5)
    check("mouse: at the conversation's end the wheel stops", "Tell me if you want docs.html split into sections." in end[-8])

    term.click(500, 500, "off the screen")
    term.keys(b"\x0f", "ctrl+o")
    term.read(0.5)
    term.keys(b"\x0f", "ctrl+o")
    check("mouse: a report off the screen breaks nothing: ctrl+o still opens all, then closes all, and the end is followed again",
          term.settles("after ctrl+o twice") == folded)
    term.keys(b"\x11", "ctrl+q")
    check("mouse: ctrl+q leaves, exit code 0", term.exit() == 0)
    on, off = term.raw.find(b"\x1b[?1000h"), term.raw.rfind(b"\x1b[?1000l")
    check("mouse: the terminal reports the mouse during the replay, and no longer after", 0 <= on < off, (on, off))

    # The kernel's panel: its rows open one by one.
    term = Terminal(trace, ["--from", str(DONE), "--paused"], env={"COLORTERM": "truecolor"}, rows=44)
    term.waits("REPL variables")
    before = term.settles("paused once the turn is done, with the panel", 0.3)
    panel = before[term.find("Running servers")].index("Running servers")
    fetch = term.find("fetch ", panel)
    term.click(panel + 10, fetch, "the variable fetch")
    opened = term.settles("after a click on a variable")
    check("mouse: a click on a variable opens the start of its value, a row under its own; its type stays on the right",
          opened[fetch][panel:].split() == ["fetch", "function"] and opened[fetch + 1][panel:].strip() == ""
          and opened[fetch + 2][panel:].strip().startswith("def fetch(name: str)")
          and '"""A page\'s bytes over HTTP' in "".join(row[panel:].strip() for row in opened[fetch + 2:fetch + 6])
          and term.find("timings ", panel) > fetch + 3, opened[fetch:fetch + 6])
    heaviest = term.find("heaviest ", panel)
    term.click(panel + 10, heaviest, "the variable heaviest")
    check("mouse: another opens beside it", term.settles("after a click on another")[heaviest + 2][panel:].strip() == "docs.html")
    term.click(panel + 10, fetch, "the variable fetch, again")
    check("mouse: a second click closes the first; the other stays open",
          term.settles("after a second click")[fetch + 1][panel:].split() == ["timings", "dict"]
          and term.find("docs.html", panel) is not None)
    server = term.find("● python3", panel)
    check("mouse: a command's last lines are folded", term.find("Serving HTTP on", panel) is None)
    term.click(panel + 10, server, "the server")
    rows = term.settles("after a click on the command")
    check("mouse: a click on a command opens its last lines, a row under its own",
          rows[server + 1][panel:].strip() == "" and rows[server + 2][panel:].strip().startswith("Serving HTTP on"), rows[server:server + 3])
    term.keys(b"\x0f", "ctrl+o")
    rows = term.settles("after ctrl+o")
    check("mouse: ctrl+o opens the conversation's folds, not the panel's rows",
          any("docs.html is over the budget I set" in row for row in rows) and not any("PosixPath" in row for row in rows)
          and rows[term.find("fetch ", panel) + 1][panel:].split() == ["timings", "dict"])
    term.keys(b"\x11", "ctrl+q")
    term.exit()

    term = Terminal(trace, ["--from", str(THINKING), "--paused", "--no-panel"], env={"COLORTERM": "truecolor"}, rows=44)
    term.shows(want(THINKING), "paused while the model reasons")
    thinking = term.rows()
    at = term.find("Thinking 1s")
    term.click(8, at, "the reasoning under way")
    check("mouse: reasoning under way does not fold", at is not None and term.settles("after a click on it") == thinking)
    term.keys(b"\x03", "ctrl+c")
    check("mouse: ctrl+c leaves and gives the mouse back too",
          term.exit() == 0 and 0 <= term.raw.find(b"\x1b[?1000h") < term.raw.rfind(b"\x1b[?1000l"))

    term = Terminal(trace, ["--from", str(AGAIN + 0.5), "--speed", "0.5", "--no-panel"], env={"COLORTERM": "truecolor"}, rows=44)
    term.waits("py Ran 3 Python cells ▸")
    run = term.find("py Ran 3 Python cells ▸")
    term.click(8, run, "the cells' line, while the answer streams")
    term.read(1.2)
    streaming = term.rows()
    trace.note("-- the screen, 1.2 s after the click, the answer streaming:\n" + "\n".join(streaming))
    check("mouse: a fold opens while the conversation plays, its line in place",
          run is not None and term.find("py Ran 3 Python cells ▾") == run and term.find(">_ Ran python3") is not None)
    term.keys(b"\x11", "ctrl+q")
    term.exit()


def scenario_writing():
    """How writing in the message box can fail, listed before the code: a letter typed is taken for one of
    the replay's keys (it pauses, opens the folds, hides the panel, restarts, leaves) or never shows; what the
    box asks for stays under the text; Enter sends, or breaks the line; a line break cannot be typed; the box
    does not grow with the message, grows without end, or runs past its width; a paste loses its lines, or
    brings an escape sequence to the terminal; an edit key (backspace, the arrows, the word and line removals)
    does nothing; the terminal keeps marking the pastes once the replay has left."""
    trace = Trace("writing")
    term = Terminal(trace, ["--from", str(DONE), "--paused"], env={"COLORTERM": "truecolor"})
    term.waits("Ask Unharnessed")
    empty = term.settles("paused once the turn is done", 0.3)
    box = term.find("Ask Unharnessed")
    term.keys(b"ok, speak to q or r", "letters that were the replay's keys, and spaces")
    typed = term.settles("after typing", 0.5)
    check("writing: what is typed shows in the message box, in place of what it asked for",
          typed[box].strip().startswith("ok, speak to q or r") and term.find("Ask Unharnessed") is None, typed[box])
    check("writing: no letter was taken for a key of the replay: the screen is the same around the message",
          typed[:box] == empty[:box] and typed[box + 1:] == empty[box + 1:])
    term.keys(b"\r", "enter")
    check("writing: Enter sends nothing and breaks no line", term.settles("after enter", 0.4) == typed)
    term.keys(b"\x1b\r", "alt+enter")
    term.keys("deuxième ligne 🙂".encode(), "a second line")
    two = term.settles("after a second line", 0.5)
    check("writing: a line break is typed with Alt+Enter; the box grows by a row, the conversation gives it",
          two[box - 1].strip().startswith("ok, speak to q or r") and two[box].strip().startswith("deuxième ligne 🙂")
          and two[box + 2:] == empty[box + 2:] and two[box - 2].strip() == "", two[box - 2:box + 1])
    term.keys(b"\x7f\x7f", "backspace twice")
    term.keys(b"\x1b[D\x1b[D\x1b[D", "left three times")
    term.keys(b"X", "a letter")
    term.keys(b"\x01", "ctrl+a")
    term.keys(b"> ", "two characters at the line's start")
    edited = term.settles("after edits", 0.5)
    check("writing: backspace removes a whole character, the arrows move the caret, what is typed goes in there",
          edited[box].strip().startswith("> deuxième liXgne") and "🙂" not in edited[box], edited[box])
    term.keys(b"\x05\x17\x17", "ctrl+e, then ctrl+w twice")
    check("writing: Ctrl+W removes the word before the caret",
          term.settles("after ctrl+w", 0.4)[box].strip().split("   ")[0] == ">")
    term.keys(b"\x15", "ctrl+u")
    term.keys(b"\x7f", "backspace")
    back = term.settles("after ctrl+u and a backspace", 0.4)
    check("writing: Ctrl+U empties the line, a backspace joins it to the one before: the box is one row again",
          back[box].strip().startswith("ok, speak to q or r") and back[:box] == empty[:box])
    term.keys(b"\x1b[200~ pasted one\rpasted \x1b[31mtwo\x1b]0;owned\x07\r\nthree\x1b[201~", "a paste of three lines, with escape sequences")
    pasted = term.settles("after a paste", 0.5)
    check("writing: a paste keeps its lines and brings nothing else",
          [row.strip().split("   ")[0] for row in pasted[box - 2:box + 1]] == ["ok, speak to q or r pasted one", "pasted two", "three"]
          and term.screen.title == "", pasted[box - 2:box + 1])
    term.keys(b"\x15\x7f\x15\x7f\x15", "the lines removed")
    term.keys(b"word " * 120, "six hundred characters")
    long = term.settles("after a long message", 0.8)
    bar = term.find("Faux Model")
    rows = [row for row in long if row.strip().startswith("word")]
    check("writing: a long message wraps inside the box, which stops growing and keeps its bar and the conversation",
          bar == len(long) - 3 and 2 <= len(rows) <= 8 and all(len(row.rstrip()) <= 85 for row in rows)
          and term.find("Tell me if you want docs.html") is not None, (bar, len(rows)))
    term.keys(b"\x10", "ctrl+p")
    term.read(0.4)
    check("writing: the replay's keys are chords now: Ctrl+P plays it on", "paused" not in term.rows()[0], term.rows()[0])
    term.keys(b"\x03", "ctrl+c")
    check("writing: ctrl+c leaves, exit code 0", term.exit() == 0)
    on, off = term.raw.find(b"\x1b[?2004h"), term.raw.rfind(b"\x1b[?2004l")
    check("writing: the terminal marks the pastes during the replay, and no longer after", 0 <= on < off, (on, off))


def scenario_refusals():
    trace = Trace("refusals")
    code, out, err = trace.replay(DEMO)
    check("refusals: without a terminal the replay says how to draw a frame instead",
          code == 1 and out == "" and "needs a terminal" in err and "--at" in err, err)
    code, out, err = trace.replay(OUT / "absent.out", "--at", "1")
    check("refusals: a recording that is not there is named", code == 1 and "absent.out" in err, err)
    broken = OUT / "broken.recording"
    broken.write_text("$ x\n  0.100 < {\"jsonrpc\": \"2.0\"}\n  0.200 ? {}\n")
    code, out, err = trace.replay(broken, "--at", "1")
    check("refusals: a recording that cannot be read says which line", code == 1 and "line 3" in err, err)
    for args, said in ((["--at", "soon"], "--at"), (["--size", "wide"], "--size"), (["--speed", "0"], "--speed"), (["--fast"], "--fast")):
        code, out, err = trace.replay(DEMO, *args)
        check(f"refusals: {' '.join(args)} is refused with the usage", code == 2 and said in err and "usage:" in err, err)
    code, out, err = trace.replay("--at", "1")
    check("refusals: no recording is refused with the usage", code == 2 and "usage:" in err, err)


only = os.environ.get("UNHARNESSED_E2E_ONLY")
for scenario in (scenario_frames, scenario_open, scenario_colours, scenario_sizes, scenario_hostile, scenario_stops,
                 scenario_edits, scenario_terminal, scenario_mouse, scenario_writing, scenario_refusals):
    name = scenario.__name__[len("scenario_"):]
    if only and only != name:
        continue
    try:
        scenario()
    except Exception as error:  # noqa: BLE001
        check(f"{name}: ran to its end", False, repr(error))

print(f"\n{len(FAILED)} failed" if FAILED else "\nall passed")
sys.exit(1 if FAILED else 0)
