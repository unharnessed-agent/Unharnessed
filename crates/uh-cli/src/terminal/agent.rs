//! The agent process: `unharnessed --mode acp`, spawned as a child and
//! driven over newline-delimited JSON-RPC, the way the VS Code extension
//! drives it. A reader thread turns stdout lines into frames; the app's
//! loop drains them without blocking.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const STDERR_TAIL: usize = 40;

/// One message from the agent process.
pub(crate) enum Incoming {
    Frame(Value),
    /// A line on stderr (the kernel setup progress, failures).
    Stderr(String),
    /// stdout closed: the process ended.
    Closed,
}

pub(crate) struct Agent {
    child: Child,
    stdin: Option<ChildStdin>,
    incoming: Receiver<Incoming>,
    next_id: u64,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    closed: bool,
}

/// A JSON-RPC error answer, reduced to the text worth showing.
#[derive(Debug, Clone)]
pub(crate) struct RpcError(pub String);

impl Agent {
    /// Start the agent with `args` (after `--mode acp`) in `cwd`.
    pub fn spawn(args: &[String], cwd: &Path) -> std::io::Result<Agent> {
        let exe = std::env::current_exe()?;
        let mut command = Command::new(exe);
        command
            .arg("--mode")
            .arg("acp")
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Its own process group, so a signal sent to the terminal's group
        // (a SIGINT from the shell, a SIGTSTP) does not stop the agent
        // behind the client's back; the client cancels turns itself.
        uh_core::platform::set_new_process_group(&mut command);
        let mut child = command.spawn()?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (tx, incoming) = std::sync::mpsc::channel();
        if let Some(stdout) = stdout {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if line.trim().is_empty() {
                        continue;
                    }
                    if let Ok(frame) = serde_json::from_str::<Value>(&line) {
                        if tx.send(Incoming::Frame(frame)).is_err() {
                            return;
                        }
                    }
                }
                let _ = tx.send(Incoming::Closed);
            });
        }
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(stderr) = stderr {
            let tail = Arc::clone(&stderr_tail);
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines() {
                    let Ok(line) = line else { break };
                    if let Ok(mut tail) = tail.lock() {
                        if tail.len() == STDERR_TAIL {
                            tail.pop_front();
                        }
                        tail.push_back(line.clone());
                    }
                    if tx.send(Incoming::Stderr(line)).is_err() {
                        return;
                    }
                }
            });
        }
        Ok(Agent {
            child,
            stdin,
            incoming,
            next_id: 0,
            stderr_tail,
            closed: false,
        })
    }

    fn send(&mut self, frame: &Value) {
        let Some(stdin) = self.stdin.as_mut() else {
            return;
        };
        let mut line = frame.to_string();
        line.push('\n');
        if stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
            .is_err()
        {
            self.closed = true;
        }
    }

    /// Send a request; its answer arrives later as a frame with this id.
    pub fn request(&mut self, method: &str, params: &Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    pub fn notify(&mut self, method: &str, params: &Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// The next message, if one is waiting.
    pub fn try_next(&mut self) -> Option<Incoming> {
        match self.incoming.try_recv() {
            Ok(message) => Some(message),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                if self.closed {
                    None
                } else {
                    self.closed = true;
                    Some(Incoming::Closed)
                }
            }
        }
    }

    /// The last lines the agent wrote on stderr (shown when it dies).
    pub fn stderr_tail(&self) -> Vec<String> {
        self.stderr_tail
            .lock()
            .map(|tail| tail.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Close stdin so the agent saves its session and kernel, then wait
    /// for it a little; kill it if it does not exit in time.
    pub fn shutdown(&mut self, grace: Duration) {
        self.stdin = None;
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Split a response frame into its result or its error text.
pub(crate) fn response(frame: &Value) -> Result<Value, RpcError> {
    if let Some(error) = frame.get("error") {
        let data = error.get("data");
        let text = data
            .and_then(|d| d.get("details").or_else(|| d.get("reason")))
            .and_then(Value::as_str)
            .or_else(|| error.get("message").and_then(Value::as_str))
            .unwrap_or("the agent refused the request");
        return Err(RpcError(text.to_string()));
    }
    Ok(frame.get("result").cloned().unwrap_or(Value::Null))
}
