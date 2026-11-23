// The Tier-C/D ruling (fleet-uniform, 2026-09-28): stack-resident futures
// by design on hot paths (boxing 130 fns is allocation-churn with zero
// correctness gain); the fn-length threshold is a style gate, not
// correctness (the harness fns are intentionally linear); 64-bit targets -
// the narrowing sits at OS/protocol boundaries where the values are
// bounded (pid syscalls, epoch/elapsed milliseconds, calendar math,
// guarded parses), and checked conversions would add panic paths where
// silent wrap was deliberate (the one genuinely-suspect family, args.rs's
// parse_positive_u32 lacking its u32::MAX bound, is flagged in the lane
// dossier for the conductor).
#![allow(
    clippy::large_futures,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

//! End-to-end ACP-mode verification: the real binary speaks the ACP
//! JSON-RPC surface over stdio, driven by the scripted faux provider, and
//! the emitted frames are checked against the expected ACP shapes.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// The child plus the tempdir it runs in: the tempdir must outlive the
/// child process (its cwd), so it is held on the struct.
struct AcpChild {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    lines: Receiver<String>,
    next_id: u64,
    /// Held (never read) so the child's cwd directory outlives the process:
    /// dropping the tempdir deletes it and the child's `current_dir` fails.
    _home: tempfile::TempDir,
    spawn_stderr: Option<std::process::ChildStderr>,
}

impl AcpChild {
    /// Wire one spawned process into the reader thread and the handle:
    /// the tempdir is held on the struct so the child's cwd directory
    /// outlives the process.
    fn wrap(mut child: std::process::Child, home: tempfile::TempDir) -> AcpChild {
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        if tx.send(line).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        AcpChild {
            child,
            stdin: Some(stdin),
            lines,
            next_id: 0,
            _home: home,
            spawn_stderr: Some(stderr),
        }
    }

    fn spawn(args: &[&str], script: &serde_json::Value) -> AcpChild {
        let home = tempfile::TempDir::new().unwrap();
        let bin = env!("CARGO_BIN_EXE_unharnessed");
        let child = Command::new(bin)
            .args(args)
            .env("HOME", home.path())
            .env("UNHARNESSED_AGENT_DIR", home.path().join("agent"))
            .env("UNHARNESSED_FAUX_SCRIPT", script.to_string())
            .current_dir(home.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("binary present");
        Self::wrap(child, home)
    }

    fn send(&mut self, frame: &Value) {
        let mut line = serde_json::to_string(&frame).unwrap();
        line.push('\n');
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: &Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        id
    }

    fn notify(&mut self, method: &str, params: &Value) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Read frames until the request `id` answers; returns the answer with
    /// the notifications seen before it, in order.
    fn wait_response(&mut self, id: u64, timeout: Duration) -> (Value, Vec<Value>) {
        let deadline = Instant::now() + timeout;
        let mut notifications = Vec::new();
        loop {
            let timeout_left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !timeout_left.is_zero(),
                "timed out waiting for response {id}"
            );
            match self.lines.recv_timeout(timeout_left) {
                Ok(line) => {
                    let frame: Value = serde_json::from_str(&line).expect("valid JSON line");
                    if frame.get("id").and_then(Value::as_u64) == Some(id)
                        && (frame.get("result").is_some() || frame.get("error").is_some())
                    {
                        return (frame, notifications);
                    }
                    notifications.push(frame);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    panic!("timed out waiting for response {id}")
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("ACP server closed stdout")
                }
            }
        }
    }
}

impl Drop for AcpChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(mut stderr) = self.spawn_stderr.take() {
            use std::io::Read;
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            if !text.is_empty() {
                eprintln!("ACP child stderr: {text}");
            }
        }
    }
}

fn initialize_params() -> Value {
    json!({
        "protocolVersion": 1,
        "clientCapabilities": {},
        "clientInfo": { "name": "acp-e2e", "title": "ACP E2E", "version": "0.0.1" },
    })
}

const TIMEOUT: Duration = Duration::from_mins(1);

/// The TS initialize response shape (capture `ts-happy_path.jsonl`), with the
/// version and sessionId-class fields normalized as volatile.
#[test]
fn acp_initialize_matches_the_ts_golden() {
    let script = json!({ "responses": ["unused"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let id = client.request("initialize", &initialize_params());
    let (response, notifications) = client.wait_response(id, TIMEOUT);
    assert!(notifications.is_empty(), "nothing precedes initialize");
    let result = &response["result"];
    assert_eq!(result["protocolVersion"], 1);
    let capabilities = &result["agentCapabilities"];
    // The saved conversations: `session/load` and `session/list`.
    assert_eq!(capabilities["loadSession"], true);
    assert_eq!(
        capabilities["promptCapabilities"],
        json!({ "image": true, "embeddedContext": true })
    );
    assert_eq!(
        capabilities["sessionCapabilities"],
        json!({ "close": {}, "list": {} })
    );
    // ACP MCP server admission is served, so the TS `mcpCapabilities`
    // flag (http support) is advertised.
    assert_eq!(capabilities["mcpCapabilities"], json!({ "http": true }));
    let info = &result["agentInfo"];
    assert_eq!(info["name"], "unharnessed");
    assert_eq!(info["title"], "Unharnessed");
    assert_eq!(result["_meta"], json!({ "unharnessed": {} }));
}

#[test]
fn acp_second_initialize_is_served() {
    let script = json!({ "responses": ["unused"] });
    let mut client = AcpChild::spawn(&["--mode", "acp"], &script);
    let first = client.request("initialize", &initialize_params());
    let _ = client.wait_response(first, TIMEOUT);
    let second = client.request("initialize", &initialize_params());
    let (response, _) = client.wait_response(second, TIMEOUT);
    assert_eq!(response["result"]["protocolVersion"], 1);
}

#[test]
fn acp_prompt_stream_completion_envelope_and_stop_reason_match_ts() {
    let script = json!({ "responses": ["ACP-OK"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let init = client.request("initialize", &initialize_params());
    let _ = client.wait_response(init, TIMEOUT);
    let new = client.request("session/new", &json!({ "mcpServers": [] }));
    let (new_response, _) = client.wait_response(new, TIMEOUT);
    let session_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let prompt = client.request(
        "session/prompt",
        &json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": "Reply with exactly: ACP-OK" }] }),
    );
    let (prompt_response, updates) = client.wait_response(prompt, TIMEOUT);

    // Frame shape sequence from ts-happy_path.jsonl: the chunk stream, the
    // response boundary, the completion event, the terminal envelope, and
    // then the response. The faux provider emits its text in one chunk.
    let mut shapes = Vec::new();
    for update in &updates {
        let body = &update["params"]["update"];
        let meta = &body["_meta"]["unharnessed"];
        shapes.push((
            body["sessionUpdate"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            meta["phase"].as_str().unwrap_or_default().to_string(),
            meta["outcome"].as_str().map(str::to_string),
            meta["terminalQuiescenceExpected"].as_bool(),
        ));
    }
    let boundary = (
        "session_info_update".to_string(),
        "responseBoundary".to_string(),
        Some("result".to_string()),
        Some(true),
    );
    let completion = (
        "session_info_update".to_string(),
        "event".to_string(),
        None,
        None,
    );
    let terminal = (
        "session_info_update".to_string(),
        "terminalQuiescence".to_string(),
        Some("result".to_string()),
        None,
    );
    // Before the TS sequence: the session's command list and what it has
    // used (published once after `session/new`), then the turn's origin.
    let tags: Vec<&str> = shapes.iter().map(|(tag, _, _, _)| tag.as_str()).collect();
    assert_eq!(
        tags[..4],
        [
            "available_commands_update",
            "session_info_update",
            "session_info_update",
            "agent_message_chunk"
        ]
    );
    assert!(updates[1]["params"]["update"]["_meta"]["unharnessed"]["usage"].is_object());
    // The turn's origin, and where the conversation stood before the
    // prompt (an entry id, to go back to it).
    let turn_start = &updates[2]["params"]["update"]["_meta"]["unharnessed"]["turnStart"];
    assert_eq!(turn_start["origin"], "prompt");
    assert!(
        turn_start["rewindTo"].is_string(),
        "turnStart: {turn_start}"
    );
    assert!(shapes.contains(&boundary), "shapes: {shapes:?}");
    assert!(shapes.contains(&completion), "shapes: {shapes:?}");
    assert!(shapes.contains(&terminal), "shapes: {shapes:?}");
    assert_eq!(shapes.last(), Some(&terminal));

    assert_eq!(
        prompt_response["result"],
        json!({ "stopReason": "end_turn" })
    );

    // Sequences are strictly increasing across the whole turn.
    let mut sequences: Vec<u64> = Vec::new();
    for update in &updates {
        sequences.push(
            update["params"]["update"]["_meta"]["unharnessed"]["eventSequence"]
                .as_u64()
                .unwrap(),
        );
    }
    let mut sorted = sequences.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sequences, sorted, "eventSequence strictly increases");

    let close = client.request("session/close", &json!({ "sessionId": session_id }));
    let (close_response, _) = client.wait_response(close, TIMEOUT);
    assert_eq!(close_response["result"], json!({}));
}

#[test]
fn acp_prompt_chunk_carries_the_assistant_message_id() {
    let script = json!({ "responses": ["ACP-OK"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let init = client.request("initialize", &initialize_params());
    let _ = client.wait_response(init, TIMEOUT);
    let new = client.request("session/new", &json!({ "mcpServers": [] }));
    let (new_response, _) = client.wait_response(new, TIMEOUT);
    let session_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let prompt = client.request(
        "session/prompt",
        &json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": "hello" }] }),
    );
    let (_, updates) = client.wait_response(prompt, TIMEOUT);
    let chunk = updates
        .iter()
        .find(|update| update["params"]["update"]["sessionUpdate"] == "agent_message_chunk")
        .expect("a message chunk");
    assert_eq!(
        chunk["params"]["update"]["messageId"],
        "unharnessed-assistant-1"
    );
    assert_eq!(
        chunk["params"]["update"]["content"],
        json!({ "type": "text", "text": "ACP-OK" })
    );
    // The session's command list, what it has used and the turn's origin
    // are published first.
    assert_eq!(
        chunk["params"]["update"]["_meta"]["unharnessed"],
        json!({ "promptTurnId": 1, "eventSequence": 4, "phase": "event" })
    );
}

#[test]
fn acp_cwd_mismatch_is_reported_not_adopted() {
    let script = json!({ "responses": ["unused"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let init = client.request("initialize", &initialize_params());
    let _ = client.wait_response(init, TIMEOUT);
    let new = client.request("session/new", &json!({ "cwd": "/tmp", "mcpServers": [] }));
    let (new_response, _) = client.wait_response(new, TIMEOUT);
    let meta = &new_response["result"]["_meta"]["unharnessed"]["cwd"];
    assert_eq!(meta["requested"], "/tmp");
    // The actual cwd is the temp dir the client runs in; only the mismatch
    // shape is asserted here (the value is tempdir-random).
    assert!(meta["actual"]
        .as_str()
        .is_some_and(|actual| actual.starts_with(std::path::MAIN_SEPARATOR)));
}

#[test]
fn acp_error_shapes_match_the_ts_goldens() {
    let script = json!({ "responses": ["unused"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let init = client.request("initialize", &initialize_params());
    let _ = client.wait_response(init, TIMEOUT);

    // Unknown session (ts-errors.jsonl): -32603 with the details string.
    let prompt = client.request(
        "session/prompt",
        &json!({ "sessionId": "bogus-session", "prompt": [{ "type": "text", "text": "hi" }] }),
    );
    let (response, _) = client.wait_response(prompt, TIMEOUT);
    assert_eq!(response["error"]["code"], -32603);
    assert_eq!(response["error"]["message"], "Internal error");
    assert_eq!(
        response["error"]["data"]["details"],
        "Unknown ACP session: bogus-session"
    );

    let close = client.request("session/close", &json!({ "sessionId": "bogus-session" }));
    let (response, _) = client.wait_response(close, TIMEOUT);
    assert_eq!(
        response["error"]["data"]["details"],
        "Unknown ACP session: bogus-session"
    );

    let new = client.request("session/new", &json!({ "mcpServers": [] }));
    let (new_response, _) = client.wait_response(new, TIMEOUT);
    let session_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    // Second session/new on a live connection (ts-errors.jsonl).
    let again = client.request("session/new", &json!({ "mcpServers": [] }));
    let (response, _) = client.wait_response(again, TIMEOUT);
    assert_eq!(response["error"]["code"], -32603);
    assert_eq!(
        response["error"]["data"]["details"],
        "unharnessed ACP mode hosts one session per connection; start another unharnessed process for a second session"
    );

    // Unknown method (ts-errors.jsonl): -32601 with the observed message.
    let unknown = client.request("unknown/method", &json!({}));
    let (response, _) = client.wait_response(unknown, TIMEOUT);
    assert_eq!(response["error"]["code"], -32601);
    assert_eq!(
        response["error"]["message"],
        "\"Method not found\": unknown/method"
    );
    assert_eq!(response["error"]["data"]["method"], "unknown/method");

    let close = client.request("session/close", &json!({ "sessionId": session_id }));
    let (response, _) = client.wait_response(close, TIMEOUT);
    assert_eq!(response["result"], json!({}));
}

#[test]
fn acp_initialize_with_string_protocol_version_is_invalid_params() {
    let script = json!({ "responses": ["unused"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let id = client.request(
        "initialize",
        &json!({ "protocolVersion": "1", "clientCapabilities": {} }),
    );
    let (response, _) = client.wait_response(id, TIMEOUT);
    assert_eq!(response["error"]["code"], -32602);
    assert_eq!(response["error"]["message"], "Invalid params");
    assert_eq!(
        response["error"]["data"]["protocolVersion"]["_errors"][0],
        "Invalid input: expected number, received string"
    );
}

#[test]
fn acp_image_block_without_mime_type_is_invalid_params() {
    let script = json!({ "responses": ["unused"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let init = client.request("initialize", &initialize_params());
    let _ = client.wait_response(init, TIMEOUT);
    let new = client.request("session/new", &json!({ "mcpServers": [] }));
    let (new_response, _) = client.wait_response(new, TIMEOUT);
    let session_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let prompt = client.request(
        "session/prompt",
        &json!({ "sessionId": session_id, "prompt": [{ "type": "image", "data": "AAAA" }] }),
    );
    let (response, _) = client.wait_response(prompt, TIMEOUT);
    assert_eq!(response["error"]["code"], -32602);
    assert_eq!(response["error"]["message"], "Invalid params");
    assert_eq!(
        response["error"]["data"]["reason"],
        "image block requires base64 `data` and `mimeType` strings"
    );
}

#[test]
fn acp_cancel_without_an_active_turn_is_a_noop() {
    let script = json!({ "responses": ["unused"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let init = client.request("initialize", &initialize_params());
    let _ = client.wait_response(init, TIMEOUT);
    let new = client.request("session/new", &json!({ "mcpServers": [] }));
    let (new_response, _) = client.wait_response(new, TIMEOUT);
    let session_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    client.notify("session/cancel", &json!({ "sessionId": session_id }));
    // The no-op cancel answers nothing; the session still closes cleanly.
    let close = client.request("session/close", &json!({ "sessionId": session_id }));
    let (close_response, notifications) = client.wait_response(close, TIMEOUT);
    // Only what `session/new` publishes: the session's command list and
    // what it has used.
    assert!(
        notifications.iter().all(|n| {
            let update = &n["params"]["update"];
            update["sessionUpdate"] == "available_commands_update"
                || update["_meta"]["unharnessed"]["usage"].is_object()
        }),
        "a no-op cancel publishes nothing: {notifications:?}"
    );
    assert_eq!(close_response["result"], json!({}));
}

#[test]
fn acp_initialize_advertises_mcp_capabilities() {
    let script = json!({ "responses": ["unused"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let init = client.request("initialize", &initialize_params());
    let (response, _) = client.wait_response(init, TIMEOUT);
    assert_eq!(
        response["result"]["agentCapabilities"]["mcpCapabilities"],
        json!({ "http": true })
    );
}

#[test]
fn acp_mcp_admission_accepts_valid_servers_and_close_releases() {
    let script = json!({ "responses": ["unused"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let init = client.request("initialize", &initialize_params());
    let _ = client.wait_response(init, TIMEOUT);
    let new = client.request(
        "session/new",
        &json!({ "mcpServers": [
            { "name": "capture-stdio", "type": "stdio", "command": "cat", "args": [], "env": [{"name": "A", "value": "1"}] },
            { "name": "capture-http", "type": "http", "url": "https://mcp.invalid/capture", "headers": [{"name": "X-A", "value": "yes"}] },
        ]}),
    );
    let (response, _) = client.wait_response(new, TIMEOUT);
    let session_id = response["result"]["sessionId"]
        .as_str()
        .expect("admission succeeds")
        .to_string();
    let close = client.request("session/close", &json!({ "sessionId": session_id }));
    let (close_response, _) = client.wait_response(close, TIMEOUT);
    assert_eq!(close_response["result"], json!({}));
}

#[test]
fn acp_mcp_admission_rejects_a_second_session_only_when_open() {
    let script = json!({ "responses": ["unused"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let init = client.request("initialize", &initialize_params());
    let _ = client.wait_response(init, TIMEOUT);
    let new = client.request(
        "session/new",
        &json!({ "mcpServers": [
            { "name": "first", "type": "stdio", "command": "cat", "args": [], "env": [] },
        ]}),
    );
    let (response, _) = client.wait_response(new, TIMEOUT);
    let session_id = response["result"]["sessionId"]
        .as_str()
        .expect("admission succeeds")
        .to_string();
    // Rejected admission keeps serving: the single-session error is
    // internal with the raw details, exactly like the TS host.
    let second = client.request(
        "session/new",
        &json!({ "mcpServers": [
            { "name": "second", "type": "stdio", "command": "cat", "args": [], "env": [] },
        ]}),
    );
    let (second_response, _) = client.wait_response(second, TIMEOUT);
    assert_eq!(second_response["error"]["code"], -32603);
    assert_eq!(second_response["error"]["message"], "Internal error");
    assert_eq!(
        second_response["error"]["data"]["details"],
        "unharnessed ACP mode hosts one session per connection; start another unharnessed process for a second session"
    );
    // Close, then a replacement admission with a different server list.
    let close = client.request("session/close", &json!({ "sessionId": session_id }));
    let (close_response, _) = client.wait_response(close, TIMEOUT);
    assert_eq!(close_response["result"], json!({}));
    let replacement = client.request(
        "session/new",
        &json!({ "mcpServers": [
            { "name": "replacement", "type": "stdio", "command": "cat", "args": [], "env": [] },
        ]}),
    );
    let (replacement_response, _) = client.wait_response(replacement, TIMEOUT);
    assert!(
        replacement_response["result"]["sessionId"].is_string(),
        "replacement admission succeeds"
    );
}

#[test]
fn acp_mcp_admission_rejects_invalid_params_with_the_ts_reasons() {
    let script = json!({ "responses": ["unused"] });
    let cases: &[(Value, &str)] = &[
        (
            json!([{ "name": "-bad", "type": "stdio", "command": "cat", "args": [], "env": [] }]),
            "MCP server names must start with an alphanumeric character and contain at most 64 alphanumeric, underscore, or hyphen characters",
        ),
        (
            json!([
                { "name": "dup", "type": "stdio", "command": "cat", "args": [], "env": [] },
                { "name": "dup", "type": "stdio", "command": "cat", "args": [], "env": [] },
            ]),
            "duplicate MCP server name: dup",
        ),
        (
            json!([{ "name": "n", "type": "stdio", "command": "cat\u{0}", "args": [], "env": [] }]),
            "MCP server n has an invalid stdio command",
        ),
        (
            json!([{ "name": "e", "type": "stdio", "command": "cat", "args": [], "env": [
                { "name": "A", "value": "1" }, { "name": "A", "value": "2" },
            ]}]),
            "MCP server e has duplicate environment A",
        ),
        (
            json!([{ "name": "h", "type": "http", "url": "https://mcp.invalid/x", "headers": [
                { "name": "X-A", "value": "1" }, { "name": "x-a", "value": "2" },
            ]}]),
            "MCP server h has duplicate header x-a",
        ),
        (
            json!([{ "name": "s", "type": "sse", "url": "https://mcp.invalid/x", "headers": [] }]),
            "MCP server s uses unsupported sse transport",
        ),
        (
            json!([{ "name": "c", "type": "http", "url": "https://user:pw@mcp.invalid/x", "headers": [] }]),
            "MCP server c must use an HTTP(S) URL without embedded credentials",
        ),
    ];
    for (servers, reason) in cases {
        let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
        let init = client.request("initialize", &initialize_params());
        let _ = client.wait_response(init, TIMEOUT);
        let new = client.request("session/new", &json!({ "mcpServers": servers }));
        let (response, _) = client.wait_response(new, TIMEOUT);
        assert_eq!(response["error"]["code"], -32602, "case {reason}");
        assert_eq!(response["error"]["message"], "Invalid params");
        assert_eq!(response["error"]["data"]["reason"], *reason);
    }
}

#[test]
fn acp_mcp_schema_invalid_entries_are_dropped_like_the_sdk() {
    // The SDK zod filter (`vecSkipError(zMcpServer)`) silently drops
    // entries that miss required fields; admission succeeds with the
    // surviving list — the live TS behavior.
    let script = json!({ "responses": ["unused"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let init = client.request("initialize", &initialize_params());
    let _ = client.wait_response(init, TIMEOUT);
    let new = client.request(
        "session/new",
        // No `env` (required), invalid env item, http without headers.
        &json!({ "mcpServers": [
            { "name": "no-env", "type": "stdio", "command": "cat", "args": [] },
            { "name": "bad-item", "type": "stdio", "command": "cat", "args": [], "env": [{"name": 1, "value": "x"}] },
            { "name": "no-headers", "type": "http", "url": "https://mcp.invalid/x" },
        ]}),
    );
    let (response, _) = client.wait_response(new, TIMEOUT);
    assert!(
        response["result"]["sessionId"].is_string(),
        "schema-invalid entries are dropped, not rejected"
    );
}

#[test]
fn acp_mcp_long_names_fail_at_tool_derivation_with_internal_error() {
    let script = json!({ "responses": ["unused"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let init = client.request("initialize", &initialize_params());
    let _ = client.wait_response(init, TIMEOUT);
    let long = format!("a{}", "b".repeat(50));
    let new = client.request(
        "session/new",
        &json!({ "mcpServers": [
            { "name": long, "type": "stdio", "command": "cat", "args": [], "env": [] },
        ]}),
    );
    let (response, _) = client.wait_response(new, TIMEOUT);
    assert_eq!(response["error"]["code"], -32603);
    assert_eq!(
        response["error"]["data"]["details"],
        format!("Invalid ACP MCP server name: {long}")
    );
}

#[test]
fn acp_compact_command_publishes_the_compaction_meta_and_end_turn() {
    // The faux session is short, so `/compact` skips (TS
    // `CompactionSkippedError`): the observable parity is the
    // `compaction: {}` namespaced update and the normal end_turn response.
    let script = json!({ "responses": ["one answer"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let init = client.request("initialize", &initialize_params());
    let _ = client.wait_response(init, TIMEOUT);
    let new = client.request("session/new", &json!({ "mcpServers": [] }));
    let (new_response, _) = client.wait_response(new, TIMEOUT);
    let session_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let prompt = client.request(
        "session/prompt",
        &json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": "/compact" }] }),
    );
    let (prompt_response, updates) = client.wait_response(prompt, TIMEOUT);

    let compaction = updates
        .iter()
        .find(|update| {
            let meta = &update["params"]["update"]["_meta"]["unharnessed"];
            meta.get("compaction").is_some_and(|value| !value.is_null())
        })
        .expect("a compaction meta frame");
    let meta = &compaction["params"]["update"]["_meta"]["unharnessed"];
    assert_eq!(
        meta["compaction"],
        json!({}),
        "a skipped compaction publishes the empty payload"
    );
    assert_eq!(meta["phase"], "event");

    // The turn settles normally: boundary, completion, terminal, end_turn.
    let boundary = updates.iter().any(|update| {
        let meta = &update["params"]["update"]["_meta"]["unharnessed"];
        meta["phase"] == "responseBoundary" && meta["terminalQuiescenceExpected"] == true
    });
    assert!(boundary, "updates: {updates:?}");
    assert_eq!(
        prompt_response["result"],
        json!({ "stopReason": "end_turn" })
    );
}

#[test]
fn acp_goal_command_publishes_goal_meta_and_runs_the_continuation() {
    // `/goal` start schedules its continuation as the turn's model segment:
    // the goal meta frame precedes the streamed answer, and the usage
    // accounting publishes a second goal frame after the message settles.
    // The tiny budget bounds the goal loop the direct-ACP settle loop now
    // hosts (TS parity: the continuation loop runs inside the same
    // session/prompt request): the crossing turn's budget-limit steer is
    // the second model segment, and the budget_limited goal settles the
    // prompt with end_turn instead of looping forever.
    let script = json!({ "responses": ["GOAL-PROGRESS", "WRAP-UP"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let init = client.request("initialize", &initialize_params());
    let _ = client.wait_response(init, TIMEOUT);
    let new = client.request("session/new", &json!({ "mcpServers": [] }));
    let (new_response, _) = client.wait_response(new, TIMEOUT);
    let session_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let prompt = client.request(
        "session/prompt",
        &json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": "/goal --budget 5 reply with exactly: GOAL-PROGRESS" }] }),
    );
    let (prompt_response, updates) = client.wait_response(prompt, TIMEOUT);

    let goal_frames: Vec<&Value> = updates
        .iter()
        .filter(|update| {
            let meta = &update["params"]["update"]["_meta"]["unharnessed"];
            meta.get("goal").is_some_and(|value| !value.is_null())
        })
        .collect();
    assert!(!goal_frames.is_empty(), "updates: {updates:?}");
    let first = &goal_frames[0]["params"]["update"]["_meta"]["unharnessed"]["goal"];
    assert_eq!(first["status"], "active");
    assert_eq!(first["objective"], "reply with exactly: GOAL-PROGRESS");
    assert_eq!(first["tokensUsed"], 0);
    // A usage update follows the settled message: the tiny budget
    // crosses at the first turn, so the goal is budget_limited before
    // the wrap-up steer segment runs.
    assert!(goal_frames.len() >= 2, "goal frames: {goal_frames:?}");
    let second = &goal_frames[1]["params"]["update"]["_meta"]["unharnessed"]["goal"];
    assert_eq!(second["status"], "budget_limited");
    assert!(second["tokensUsed"].as_u64().unwrap_or(0) > 0);
    // The budget-limit wrap-up steer ran as the prompt's second model
    // segment (its streamed answer is the second scripted response; the
    // faux pacing may split one answer into chunks, so the joined text
    // carries the observable contract).
    let streamed: String = updates
        .iter()
        .filter_map(|update| update["params"]["update"]["content"]["text"].as_str())
        .collect();
    assert!(streamed.contains("GOAL-PROGRESS"), "updates: {updates:?}");
    assert!(streamed.contains("WRAP-UP"), "updates: {updates:?}");
    assert_eq!(
        prompt_response["result"],
        json!({ "stopReason": "end_turn" })
    );
}

#[test]
fn acp_autonomous_token_limit_maps_to_max_tokens_stop_reason() {
    // A one-token budget is exhausted by the first turn: the driver stops
    // with the token limit, the completion envelope carries the autonomous
    // accounting, and the stop reason is `max_tokens`.
    let script = json!({ "responses": ["an answer"] });
    let mut client = AcpChild::spawn(
        &[
            "--mode",
            "acp",
            "--no-session",
            "--autonomous",
            "--autonomous-max-tokens",
            "1",
        ],
        &script,
    );
    let init = client.request("initialize", &initialize_params());
    let _ = client.wait_response(init, TIMEOUT);
    let new = client.request("session/new", &json!({ "mcpServers": [] }));
    let (new_response, _) = client.wait_response(new, TIMEOUT);
    let session_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let prompt = client.request(
        "session/prompt",
        &json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": "do the thing" }] }),
    );
    let (prompt_response, updates) = client.wait_response(prompt, TIMEOUT);

    let completion = updates
        .iter()
        .find(|update| {
            let meta = &update["params"]["update"]["_meta"]["unharnessed"];
            meta["phase"] == "event" && meta.get("autonomous").is_some_and(|v| !v.is_null())
        })
        .expect("an autonomous completion meta");
    let autonomous = &completion["params"]["update"]["_meta"]["unharnessed"]["autonomous"];
    assert_eq!(autonomous["enabled"], true);
    assert_eq!(autonomous["turnsUsed"], 1);

    assert_eq!(
        prompt_response["result"],
        json!({ "stopReason": "max_tokens" })
    );
}

#[test]
fn acp_autonomous_disabled_reports_end_turn_without_accounting() {
    // Without autonomous flags the completion envelope carries no autonomous
    // meta and the stop reason is end_turn.
    let script = json!({ "responses": ["an answer"] });
    let mut client = AcpChild::spawn(&["--mode", "acp", "--no-session"], &script);
    let init = client.request("initialize", &initialize_params());
    let _ = client.wait_response(init, TIMEOUT);
    let new = client.request("session/new", &json!({ "mcpServers": [] }));
    let (new_response, _) = client.wait_response(new, TIMEOUT);
    let session_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let prompt = client.request(
        "session/prompt",
        &json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": "hi" }] }),
    );
    let (prompt_response, updates) = client.wait_response(prompt, TIMEOUT);
    for update in &updates {
        let meta = &update["params"]["update"]["_meta"]["unharnessed"];
        assert!(
            meta.get("autonomous")
                .is_none_or(serde_json::Value::is_null),
            "no autonomous meta"
        );
    }
    assert_eq!(
        prompt_response["result"],
        json!({ "stopReason": "end_turn" })
    );
}

/// Spawn with compaction settings written into the agent dir (the
/// in-process session resolves them at session assembly).
fn spawn_with_compaction_settings(
    args: &[&str],
    script: &serde_json::Value,
    reserve_tokens: u64,
    keep_recent_tokens: u64,
) -> AcpChild {
    let home = tempfile::TempDir::new().unwrap();
    let agent_dir = home.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(
        agent_dir.join("settings.json"),
        json!({
            "compaction": {
                "enabled": true,
                "reserveTokens": reserve_tokens,
                "keepRecentTokens": keep_recent_tokens,
            }
        })
        .to_string(),
    )
    .expect("write settings.json");
    let bin = env!("CARGO_BIN_EXE_unharnessed");
    let child = Command::new(bin)
        .args(args)
        .env("HOME", home.path())
        .env("UNHARNESSED_CODING_AGENT_DIR", &agent_dir)
        .env("UNHARNESSED_FAUX_SCRIPT", script.to_string())
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("binary present");
    AcpChild::wrap(child, home)
}

/// The compaction metas among a turn's updates (the ACP `compaction_end`
/// mapping; a ran compaction carries `tokensBefore`/`summary`, every
/// skipped, failed, or cancelled run the empty payload).
fn compaction_metas(updates: &[Value]) -> Vec<Value> {
    updates
        .iter()
        .filter_map(|update| {
            let meta = &update["params"]["update"]["_meta"]["unharnessed"];
            Some(meta["compaction"].clone()).filter(|value| !value.is_null())
        })
        .collect()
}

// Cancellation failures: a stopped model or summarizer must not restart,
// close must answer after the cancelled prompt, EOF must exit, and a cancel
// must leave the next prompt usable. Synchronize on wire events, not sleeps.
#[derive(Clone, Copy, Debug)]
enum StopTurn {
    Cancel,
    Close,
    Eof,
}

fn cancellation_client(mut script: Value) -> (AcpChild, Value) {
    script["contextWindow"] = json!(20000);
    script["maxTokens"] = json!(2000);
    let mut client = spawn_with_compaction_settings(&["--mode", "acp"], &script, 1, 10);
    let init = client.request("initialize", &initialize_params());
    client.wait_response(init, TIMEOUT);
    let new = client.request("session/new", &json!({}));
    let (response, _) = client.wait_response(new, TIMEOUT);
    (client, response["result"]["sessionId"].clone())
}

fn cancellation_prompt(client: &mut AcpChild, session: &Value, text: &str) -> u64 {
    client.request(
        "session/prompt",
        &json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}),
    )
}

fn wait_cancellation_phase(client: &AcpChild, compacting: bool) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let line = client
            .lines
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the model/compaction must start before cancellation");
        let frame: Value = serde_json::from_str(&line).unwrap();
        let update = &frame["params"]["update"];
        if (compacting && update["_meta"]["unharnessed"]["usage"]["compacting"] == true)
            || (!compacting && update["sessionUpdate"] == "agent_message_chunk")
        {
            return;
        }
        assert!(
            frame.get("result").is_none() && frame.get("error").is_none(),
            "the prompt settled before reaching the required phase: {frame}"
        );
    }
}

fn stop_cancellation_turn(client: &mut AcpChild, session: &Value, prompt: u64, stop: StopTurn) {
    let close = match stop {
        StopTurn::Cancel => {
            client.notify("session/cancel", &json!({"sessionId": session}));
            None
        }
        StopTurn::Close => Some(client.request("session/close", &json!({"sessionId": session}))),
        StopTurn::Eof => {
            drop(client.stdin.take());
            None
        }
    };
    let (answer, updates) = client.wait_response(prompt, Duration::from_secs(5));
    assert_eq!(
        answer["result"]["stopReason"], "cancelled",
        "{stop:?}: {answer}"
    );
    let text: String = updates
        .iter()
        .filter_map(|frame| frame["params"]["update"]["content"]["text"].as_str())
        .collect();
    assert!(
        !text.contains("must not run"),
        "{stop:?} started a model request after cancellation: {updates:?}"
    );
    if let Some(close) = close {
        let (answer, _) = client.wait_response(close, Duration::from_secs(5));
        assert_eq!(answer["result"], json!({}));
    }
    if matches!(stop, StopTurn::Eof) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = client.child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < deadline, "EOF did not stop the process");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    eprintln!("{stop:?}: prompt {prompt} cancelled without restarting the model");
}

#[test]
fn acp_cancel_close_and_eof_stop_streaming() {
    for stop in [StopTurn::Cancel, StopTurn::Close, StopTurn::Eof] {
        let (mut client, session) = cancellation_client(json!({
            "responses": ["in flight ".repeat(1000), "after"], "tokensPerSecond": 1000
        }));
        let prompt = cancellation_prompt(&mut client, &session, "work");
        wait_cancellation_phase(&client, false);
        client.notify("session/cancel", &json!({"sessionId": "wrong session"}));
        let wrong_close = client.request("session/close", &json!({"sessionId": "wrong session"}));
        let (answer, _) = client.wait_response(wrong_close, TIMEOUT);
        assert!(answer.get("error").is_some());
        stop_cancellation_turn(&mut client, &session, prompt, stop);
        if matches!(stop, StopTurn::Cancel) {
            client.notify("session/cancel", &json!({"sessionId": session}));
            let next = cancellation_prompt(&mut client, &session, "again");
            let (answer, updates) = client.wait_response(next, TIMEOUT);
            assert_eq!(answer["result"]["stopReason"], "end_turn");
            let text: String = updates
                .iter()
                .filter_map(|frame| frame["params"]["update"]["content"]["text"].as_str())
                .collect();
            assert_eq!(text, "after");
        }
    }
}

#[test]
fn acp_cancel_close_and_eof_stop_boundary_compaction() {
    for stop in [StopTurn::Cancel, StopTurn::Close, StopTurn::Eof] {
        let (mut client, session) = cancellation_client(json!({"responses": [
            {"text": "word ".repeat(10000), "delayMs": 20},
            {"text": "summary", "delayMs": 15000}, "must not run"
        ]}));
        let prompt = cancellation_prompt(&mut client, &session, "/goal keep working");
        wait_cancellation_phase(&client, true);
        stop_cancellation_turn(&mut client, &session, prompt, stop);
    }
}

#[test]
fn acp_cancel_close_and_eof_stop_pre_turn_compaction() {
    for stop in [StopTurn::Cancel, StopTurn::Close, StopTurn::Eof] {
        let (mut client, session) = cancellation_client(json!({"responses": [
            {"text": "word ".repeat(10000), "delayMs": 20},
            {"stopReason": "error", "errorMessage": "intentional summary failure"},
            {"text": "summary", "delayMs": 15000}, "must not run"
        ]}));
        let seed = cancellation_prompt(&mut client, &session, "seed oversized context");
        let (answer, updates) = client.wait_response(seed, TIMEOUT);
        assert_eq!(answer["result"]["stopReason"], "end_turn");
        assert_eq!(compaction_metas(&updates), vec![json!({})]);
        let prompt = cancellation_prompt(&mut client, &session, "work");
        wait_cancellation_phase(&client, true);
        stop_cancellation_turn(&mut client, &session, prompt, stop);
    }
}

#[test]
fn acp_cancel_close_and_eof_stop_manual_compaction() {
    for stop in [StopTurn::Cancel, StopTurn::Close, StopTurn::Eof] {
        let (mut client, session) = cancellation_client(json!({"responses": [
            {"text": "word ".repeat(1000), "delayMs": 20},
            {"text": "summary", "delayMs": 15000}, "must not run"
        ]}));
        let seed = cancellation_prompt(&mut client, &session, "seed context");
        client.wait_response(seed, TIMEOUT);
        let prompt = cancellation_prompt(&mut client, &session, "/compact");
        wait_cancellation_phase(&client, true);
        stop_cancellation_turn(&mut client, &session, prompt, stop);
    }
}

/// The threshold arm on the ACP turn path: a settled turn whose usage
/// crosses the reserve headroom compacts at the boundary and publishes
/// the `compaction` meta with the summarizer's text (TS `_checkCompaction`
/// threshold arm, binary level).
///
/// Two turns over a 500-token combined ceiling (the f14 battery shape:
/// the window minus the faux harness model's `4_096` per-request output
/// budget and the reserve): the single-turn compaction skips (nothing
/// before the turn to summarize — the skip publishes the empty payload,
/// proving the arm ran), then the second turn's boundary compaction
/// summarizes turn one and publishes its result.
#[test]
fn acp_threshold_auto_compaction_publishes_the_compaction_meta() {
    let script = json!({
        "contextWindow": 128_000,
        "responses": [
            { "text": "turn one reply" },
            { "text": "turn two reply" },
            { "text": "the auto summary" },
        ]
    });
    let mut client = spawn_with_compaction_settings(
        &["--mode", "acp", "--no-session"],
        &script,
        128_000 - 4_096 - 500,
        10,
    );
    let init = client.request("initialize", &initialize_params());
    let _ = client.wait_response(init, TIMEOUT);
    let new = client.request("session/new", &json!({ "mcpServers": [] }));
    let (new_response, _) = client.wait_response(new, TIMEOUT);
    let session_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    // Turn one crosses the headroom: the boundary arm ran and the
    // single-turn skip published the empty payload.
    let prompt = client.request(
        "session/prompt",
        &json!({
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": format!("turn one {}", "x".repeat(8_000)) }],
        }),
    );
    let (prompt_response, updates) = client.wait_response(prompt, TIMEOUT);
    assert_eq!(prompt_response["result"]["stopReason"], "end_turn");
    let metas = compaction_metas(&updates);
    assert!(!metas.is_empty(), "the threshold arm ran: {updates:?}");
    assert!(
        metas
            .iter()
            .all(|meta| meta.as_object().is_some_and(serde_json::Map::is_empty)),
        "the single-turn compaction skipped: {metas:?}"
    );

    // Turn two's boundary: the compaction summarizes turn one and
    // publishes its result.
    let prompt = client.request(
        "session/prompt",
        &json!({
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": "turn two" }],
        }),
    );
    let (prompt_response, updates) = client.wait_response(prompt, TIMEOUT);
    assert_eq!(prompt_response["result"]["stopReason"], "end_turn");
    let metas = compaction_metas(&updates);
    let ran = metas
        .iter()
        .find(|meta| {
            meta["summary"]
                .as_str()
                .is_some_and(|summary| summary.contains("the auto summary"))
        })
        .unwrap_or_else(|| panic!("the compaction ran and published: {metas:?}"));
    assert!(ran["tokensBefore"].as_u64().unwrap() > 0);
}

/// The overflow arm on the ACP turn path: a provider context-overflow
/// error runs one compact-and-retry at the boundary and the retried turn
/// settles the prompt with `end_turn` instead of the error (TS
/// `_checkCompaction` Case 1, binary level).
#[test]
fn acp_overflow_recovery_compacts_and_retries_the_turn() {
    let script = json!({
        "contextWindow": 128_000,
        "responses": [
            { "text": "seed reply" },
            {
                "text": "",
                "stopReason": "error",
                "errorMessage": "prompt is too long: 213462 tokens > 200000 maximum",
            },
            { "text": "the summary" },
            { "text": "recovered reply" },
        ]
    });
    let mut client =
        spawn_with_compaction_settings(&["--mode", "acp", "--no-session"], &script, 1, 10);
    let init = client.request("initialize", &initialize_params());
    let _ = client.wait_response(init, TIMEOUT);
    let new = client.request("session/new", &json!({ "mcpServers": [] }));
    let (new_response, _) = client.wait_response(new, TIMEOUT);
    let session_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    // The seed turn: nothing fires (context far below the headroom).
    let prompt = client.request(
        "session/prompt",
        &json!({
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": format!("seed turn {}", "x".repeat(48_000)) }],
        }),
    );
    let (prompt_response, updates) = client.wait_response(prompt, TIMEOUT);
    assert_eq!(prompt_response["result"]["stopReason"], "end_turn");
    assert!(
        compaction_metas(&updates).is_empty(),
        "nothing fires below the headroom"
    );

    // The overflow probe: the arm compacts once (the summarizer consumed
    // the third scripted response) and the retried turn recovers. The
    // seed turn's drain can outlive its response on a loaded runner (the
    // overflow retry then bounces off the still-running prompt guard) —
    // the probe is re-issued until the session settles (the recovered
    // turn's assertion itself is unchanged and strict).
    let mut probe_attempts = 0;
    let (prompt_response, updates) = loop {
        probe_attempts += 1;
        let prompt = client.request(
            "session/prompt",
            &json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": format!("overflow probe {}", "x".repeat(2_000)) }],
            }),
        );
        let (response, prompt_updates) = client.wait_response(prompt, TIMEOUT);
        // ACP `internal_error` carries the refusal text in `data.details`
        // (`Internal error` is the generic message) — match both fields.
        let refusal_text = response["error"]["data"]["details"]
            .as_str()
            .or_else(|| response["error"]["message"].as_str())
            .unwrap_or_default();
        let refused = response["error"].is_object() && refusal_text.contains("already running");
        if !refused {
            break (response, prompt_updates);
        }
        assert!(
            probe_attempts < 40,
            "the session never settled after the seed turn: {response}"
        );
        std::thread::sleep(std::time::Duration::from_millis(250));
    };
    assert_eq!(
        prompt_response["result"],
        json!({ "stopReason": "end_turn" }),
        "the retry recovered the turn: {prompt_response}"
    );
    let metas = compaction_metas(&updates);
    assert_eq!(metas.len(), 1, "one compaction meta: {metas:?}");
    assert_eq!(metas[0]["summary"], "the summary");
    assert!(metas[0]["tokensBefore"].as_u64().unwrap() > 0);
}
