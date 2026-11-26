#!/usr/bin/env python3
"""End-to-end checks of the shell behind the kernel's `bash()` on the real
binary, a real kernel and a local fake provider (no network): the system
prompt names the OS and the shell, `bash()` runs bash on macOS and Linux and
PowerShell on Windows (UTF-8 output, a native program's exit code kept),
`bash(cmd, shell="bash")` runs bash everywhere (Git Bash on Windows, when
installed), and any other `shell` is refused.

usage: scripts/unharnessed-shell-e2e.py <out-dir>

Writes <out-dir>/shell.out (the command run, then its json-mode stdout),
shell.err and shell-requests.json (the provider requests). The kernel venv
lives in <out-dir> (UNHARNESSED_BIN picks the binary, default
target/debug/unharnessed). Exits non-zero when a check fails.
"""
import json
import os
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
EXE = ".exe" if os.name == "nt" else ""
BIN = str(Path(os.environ.get("UNHARNESSED_BIN", ROOT / f"target/debug/unharnessed{EXE}")).resolve())
OUT = Path(sys.argv[1]).resolve()
HOME = OUT / "home"
HOME.mkdir(parents=True, exist_ok=True)
WINDOWS = os.name == "nt"
GIT_BASH = WINDOWS and any(
    Path(path).exists()
    for path in (r"C:\Program Files\Git\bin\bash.exe", r"C:\Program Files (x86)\Git\bin\bash.exe")
)
# A throwaway HOME: no credentials, so the fake provider is the only model.
BASE_ENV = {k: v for k, v in os.environ.items() if not k.startswith(("UNHARNESSED_", "UH_", "XDG_"))}
BASE_ENV.update(HOME=str(HOME), USERPROFILE=str(HOME), UNHARNESSED_OFFLINE="1", DO_NOT_TRACK="1",
                UV_CACHE_DIR=os.environ.get("UV_CACHE_DIR", str(Path.home() / ".cache/uv")),
                UNHARNESSED_PACKAGE_DIR=str(ROOT), UNHARNESSED_KERNEL_VENV=str(OUT / "kernel-venv"))
FAILED = []
REQUESTS = []

# One cell: each command's result as a JSON line the checks read back.
CELL = r"""
import json
async def run(name, command, **kw):
    try:
        r = await bash(command, **kw)
        print(json.dumps({"name": name, "exit": r.exit_code, "output": r.output}))
    except Exception as e:
        print(json.dumps({"name": name, "error": f"{type(e).__name__}: {e}"}))
if WINDOWS:
    await run("default", "Write-Output 'h\u00e9llo \u2713'; $PSVersionTable.PSEdition")
    await run("exit", "cmd /c exit 3")
    await run("cmdlet-error", "Get-Item C:\\no\\such\\file")
else:
    await run("default", "echo $0; printf 'h\u00e9llo \u2713'")
    await run("exit", "exit 3")
await run("bash", "echo $BASH_VERSION; printf 'h\u00e9llo \u2713'", shell="bash")
await run("refused", "echo x", shell="zsh")
""".replace("WINDOWS", repr(WINDOWS))
STEPS = [{"code": CELL}, {"text": "DONE"}]


def check(name, ok, detail=""):
    print(("PASS  " if ok else "FAIL  ") + name + ("" if ok else f" ({detail})"), flush=True)
    if not ok:
        FAILED.append(name)


class Provider(BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        REQUESTS.append(body)
        step = STEPS.pop(0) if STEPS else {"text": "DONE"}
        if "code" in step:
            delta = {"role": "assistant", "tool_calls": [{"index": 0, "id": f"call_{len(REQUESTS)}", "type": "function",
                                                          "function": {"name": "ipython",
                                                                       "arguments": json.dumps({"code": step["code"]})}}]}
            finish = "tool_calls"
        else:
            delta = {"role": "assistant", "content": step["text"]}
            finish = "stop"
        chunks = [{"choices": [{"index": 0, "delta": delta}]},
                  {"choices": [{"index": 0, "delta": {}, "finish_reason": finish}],
                   "usage": {"prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110}}]
        wire = "".join("data: " + json.dumps(dict(c, id="e2e", model="e2e-model")) + "\n\n" for c in chunks)
        wire = (wire + "data: [DONE]\n\n").encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(wire)))
        self.end_headers()
        self.wfile.write(wire)

    def log_message(self, *args):
        pass


SERVER = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
threading.Thread(target=SERVER.serve_forever, daemon=True).start()

work = OUT / "shell"
agent = work / "agent"
agent.mkdir(parents=True, exist_ok=True)
model = {"id": "e2e-model", "name": "E2E", "reasoning": False, "input": ["text"], "contextWindow": 128000,
         "maxTokens": 4096, "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}}
(agent / "models.json").write_text(json.dumps({"providers": {"e2e": {
    "api": "openai-completions", "baseUrl": f"http://127.0.0.1:{SERVER.server_port}/v1",
    "apiKey": "e2e-local-dummy", "models": [model]}}}))

env = dict(BASE_ENV, UNHARNESSED_AGENT_DIR=str(agent), UNHARNESSED_CODING_AGENT_DIR=str(agent))
args = [BIN, "-p", "--mode", "json", "--model", "e2e/e2e-model", "--session-dir", str(work / "sessions"), "go"]
# The first run provisions the kernel venv.
result = subprocess.run(args, cwd=work, env=env, capture_output=True, timeout=900, stdin=subprocess.DEVNULL)
stdout = result.stdout.decode("utf-8", "replace")
stderr = result.stderr.decode("utf-8", "replace")
(OUT / "shell.out").write_text(f"$ {' '.join(args)}\n# exit {result.returncode}\n{stdout}", encoding="utf-8")
(OUT / "shell.err").write_text(stderr, encoding="utf-8")
(OUT / "shell-requests.json").write_text(json.dumps(REQUESTS, indent=1), encoding="utf-8")
check("exits 0", result.returncode == 0, f"exit {result.returncode}: {stderr[-300:]}")

# The prompt the provider received.
system = next((m["content"] for m in (REQUESTS[0]["messages"] if REQUESTS else []) if m["role"] == "system"), "")
system = system if isinstance(system, str) else "".join(part.get("text", "") for part in system)
os_name = {"darwin": "macOS", "linux": "Linux", "win32": "Windows"}[sys.platform]
check(f"prompt: OS is {os_name}", f"\nOS: {os_name}\n" in system)
shell_line = next((line for line in system.splitlines() if line.startswith("Shell: ")), "")
if WINDOWS:
    check("prompt: bash() runs PowerShell", "runs PowerShell" in shell_line or "runs Windows PowerShell" in shell_line,
          shell_line)
    check("prompt: shell=\"bash\" told only with Git Bash", ('shell="bash"' in shell_line) == GIT_BASH, shell_line)
else:
    check("prompt: bash() runs bash, shell= not told", shell_line == "Shell: `bash()` runs bash.", shell_line)

# The cell's results.
frames = [json.loads(line) for line in stdout.splitlines() if line.startswith("{")]
texts = ["".join(block.get("text", "") for block in f["message"].get("content", []))
         for f in frames if f.get("type") == "message_end" and f.get("message", {}).get("role") == "toolResult"]
runs = {}
for text in texts:
    for line in text.splitlines():
        if line.startswith('{"name"'):
            entry = json.loads(line)
            runs[entry["name"]] = entry
print(json.dumps(runs, indent=1, ensure_ascii=False))
hello = "h\u00e9llo \u2713"
default = runs.get("default", {})
if WINDOWS:
    check("default: PowerShell, UTF-8 output", default.get("exit") == 0 and hello in default.get("output", "")
          and any(edition in default.get("output", "") for edition in ("Core", "Desktop")), default)
    check("default: a native program's exit code is kept", runs.get("exit", {}).get("exit") == 3, runs.get("exit"))
    check("default: a failed cmdlet exits 1", runs.get("cmdlet-error", {}).get("exit") == 1, runs.get("cmdlet-error"))
else:
    check("default: bash, UTF-8 output", default.get("exit") == 0 and "bash" in default.get("output", "")
          and hello in default.get("output", ""), default)
    check("default: exit code kept", runs.get("exit", {}).get("exit") == 3, runs.get("exit"))
in_bash = runs.get("bash", {})
if WINDOWS and not GIT_BASH:
    check('shell="bash" without Git Bash: teaching error', "Git Bash" in in_bash.get("error", ""), in_bash)
else:
    check('shell="bash": bash runs', in_bash.get("exit") == 0 and hello in in_bash.get("output", "")
          and in_bash.get("output", "").strip().splitlines()[0][:1].isdigit(), in_bash)
check("other shell refused", runs.get("refused", {}).get("error", "").startswith("ValueError"), runs.get("refused"))

print("\n" + ("FAILED: " + ", ".join(FAILED) if FAILED else "all checks passed"))
sys.exit(1 if FAILED else 0)
