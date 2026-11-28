//! A minimal LSP client over `heki server --stdio` (JSON-RPC with `Content-Length` framing).
//!
//! A wait for what the server publishes ends when the server is idle (`heki/stats`), so a
//! publication that never comes fails as soon as the server has nothing left to do, however
//! long its work took. Only a server that stops answering waits out [`HANG`].

use super::{HANG, wait_or_hang};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

pub struct Lsp {
    pub child: Child,
    pub stdin: Option<ChildStdin>,
    pub rx: Receiver<Value>,
    pub next_id: u64,
    /// Messages read while waiting for something else.
    pub backlog: Vec<Value>,
    /// `publishDiagnostics` notifications received so far (compared with `heki/stats`'
    /// `published` by [`Lsp::settle`]).
    pub published: u64,
    /// The URI of every `publishDiagnostics` received, in arrival order.
    pub publications: Vec<String>,
    /// The latest diagnostics published for each URI.
    pub latest: BTreeMap<String, Vec<Value>>,
}

impl Lsp {
    /// Start `heki <args>`, a `server` command, with `--heuristic` after the command (as
    /// `Project::check` passes it; `--no-heuristic` in `args` turns it off again).
    pub fn start(dir: &Path, args: &[&str]) -> Self {
        Self::start_with_env(dir, args, &[])
    }

    /// [`Lsp::start`] with the environment variables `env` set for the server.
    pub fn start_with_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Self {
        let (command, options) = args.split_first().expect("a server command");
        let mut child = Command::new(env!("CARGO_BIN_EXE_heki"))
            .arg(command)
            .arg("--heuristic")
            .args(options)
            .envs(env.iter().copied())
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    if let Some(v) = line.strip_prefix("Content-Length: ") {
                        len = v.parse().unwrap();
                    }
                }
                let mut body = vec![0u8; len];
                if reader.read_exact(&mut body).is_err() {
                    return;
                }
                if tx.send(serde_json::from_slice(&body).unwrap()).is_err() {
                    return;
                }
            }
        });
        Self {
            child,
            stdin: Some(stdin),
            rx,
            next_id: 1,
            backlog: Vec::new(),
            published: 0,
            publications: Vec::new(),
            latest: BTreeMap::new(),
        }
    }

    /// The next message from the server, which must come within `timeout` (`what` is waited
    /// for), recorded on arrival: published diagnostics update [`Lsp::latest`], and a request of
    /// the server (`client/registerCapability`) is answered with `null`.
    fn receive(&mut self, timeout: Duration, what: &str) -> Value {
        let msg = match self.rx.recv_timeout(timeout) {
            Ok(msg) => msg,
            Err(RecvTimeoutError::Timeout) => panic!(
                "{what} did not come within {HANG:?}: the server hangs; backlog: {:#?}",
                self.backlog
            ),
            Err(RecvTimeoutError::Disconnected) => panic!(
                "{what}: the server exited ({:?}); backlog: {:#?}",
                self.child.try_wait(),
                self.backlog
            ),
        };
        if msg["method"] == "textDocument/publishDiagnostics" {
            self.published += 1;
            let params = &msg["params"];
            let uri = params["uri"].as_str().unwrap().to_owned();
            self.publications.push(uri.clone());
            self.latest
                .insert(uri, params["diagnostics"].as_array().unwrap().clone());
        } else if msg.get("method").is_some() && msg.get("id").is_some() {
            self.write(&json!({"jsonrpc": "2.0", "id": msg["id"], "result": null}));
        }
        msg
    }

    pub fn write(&mut self, msg: &Value) {
        let body = msg.to_string();
        let stdin = self.stdin.as_mut().unwrap();
        write!(stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        stdin.flush().unwrap();
    }

    /// `shutdown` + `exit`; `close` also closes stdin like an editor does.
    pub fn stop(mut self, close: bool) {
        assert_eq!(self.request("shutdown", Value::Null), Value::Null);
        self.notify("exit", Value::Null);
        if close {
            self.stdin = None;
        }
        let status = wait_or_hang(&mut self.child, "the server after `exit`");
        assert!(status.success(), "{status:?}");
    }

    /// Close stdin without `exit`, like a client that went away, and wait for the server to end.
    pub fn hang_up(mut self) -> ExitStatus {
        self.stdin = None;
        wait_or_hang(&mut self.child, "the server after its input ended")
    }

    pub fn message(method: &str, params: Value) -> Value {
        let mut msg = json!({"jsonrpc": "2.0", "method": method});
        if !params.is_null() {
            msg["params"] = params;
        }
        msg
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        self.write(&Self::message(method, params));
    }

    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let mut msg = Self::message(method, params);
        msg["id"] = json!(id);
        self.write(&msg);
        let reply = self.wait(&format!("the reply to `{method}`"), |m| {
            m["id"] == id && m.get("method").is_none()
        });
        assert!(reply.get("error").is_none(), "{method}: {reply}");
        reply["result"].clone()
    }

    /// The first message (backlog first) matching `pred`, which must arrive within [`HANG`]. A
    /// reply comes once the analysis holding the server's state has ended, so a slow analysis
    /// delays it without failing it.
    fn wait(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        if let Some(i) = self.backlog.iter().position(&pred) {
            return self.backlog.remove(i);
        }
        let deadline = Instant::now() + HANG;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let msg = self.receive(left, what);
            if pred(&msg) {
                return msg;
            }
            self.backlog.push(msg);
        }
    }

    /// `heki/stats` once `done` holds of them, polled while the server works; past [`HANG`]
    /// the server never got there (`what`).
    fn stats_until(&mut self, what: &str, done: impl Fn(&Value) -> bool) -> Value {
        let started = Instant::now();
        loop {
            let stats = self.request("heki/stats", Value::Null);
            if done(&stats) {
                return stats;
            }
            assert!(started.elapsed() < HANG, "{what} within {HANG:?}: {stats}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Wait until the server is idle and every diagnostic it published has arrived: poll
    /// `heki/stats` until no work is in flight, then read until `published` notifications have
    /// been received. Returns the final stats. It depends on what the server reports, not on how
    /// long its work takes.
    pub fn settle(&mut self) -> Value {
        let stats = self.stats_until("the server never became idle", |s| s["in_flight"] == 0);
        let published = stats["published"].as_u64().unwrap();
        while self.published < published {
            let what = format!("{} of {published} publications", self.published);
            let msg = self.receive(HANG, &what);
            self.backlog.push(msg);
        }
        stats
    }

    /// The first `publishDiagnostics` for `uri` satisfying `pred` (backlog first). Once the
    /// server is idle it publishes nothing more, so the test fails then if none came.
    pub fn diagnostics(&mut self, uri: &str, pred: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        let matches = |m: &Value| {
            m["method"] == "textDocument/publishDiagnostics"
                && m["params"]["uri"] == uri
                && pred(m["params"]["diagnostics"].as_array().unwrap())
        };
        if self.backlog.iter().all(|m| !matches(m)) {
            self.settle();
        }
        let Some(i) = self.backlog.iter().position(matches) else {
            panic!(
                "the server is idle and published no diagnostics of {uri} that match; latest: {:#?}",
                self.latest.get(uri)
            )
        };
        let msg = self.backlog.remove(i);
        msg["params"]["diagnostics"].as_array().unwrap().clone()
    }

    /// Wait until the server has applied `n` open documents (`heki/stats`): notifications are
    /// handled concurrently, so a later `initialized` could otherwise index first.
    pub fn opened(&mut self, n: usize) {
        self.stats_until("the documents were never opened", |s| {
            s["open_documents"] == n
        });
    }

    /// Wait until the background index is ready (`heki/stats`).
    pub fn ready(&mut self) {
        self.stats_until("the index never became ready", |s| s["ready"] == true);
    }

    pub fn open(&mut self, uri: &str, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument": {"uri": uri, "languageId": "python", "version": 1, "text": text}}),
        );
    }

    pub fn change(&mut self, uri: &str, version: i32, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({"textDocument": {"uri": uri, "version": version}, "contentChanges": [{"text": text}]}),
        );
    }

    /// The next `window/logMessage` containing `needle`.
    pub fn log(&mut self, needle: &str) -> Value {
        self.wait(&format!("a log of `{needle}`"), |m| {
            m["method"] == "window/logMessage"
                && m["params"]["message"]
                    .as_str()
                    .is_some_and(|t| t.contains(needle))
        })
    }

    pub fn code_actions(&mut self, uri: &str, range: &Value, diags: &[Value]) -> Vec<Value> {
        self.request(
            "textDocument/codeAction",
            json!({"textDocument": {"uri": uri}, "range": range, "context": {"diagnostics": diags}}),
        )
        .as_array()
        .unwrap()
        .clone()
    }
}

impl Drop for Lsp {
    /// A test that fails before the server has ended leaves no server behind.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The canonical form of an existing path, without the `\\?\` prefix `std::fs::canonicalize`
/// adds on Windows: below it, the `/` of a relative path joined to it would not separate
/// components.
fn canonical(path: &Path) -> PathBuf {
    let path = std::fs::canonicalize(path).unwrap();
    match path.to_str().and_then(|p| p.strip_prefix(r"\\?\")) {
        Some(plain) if plain.as_bytes().get(1) == Some(&b':') => PathBuf::from(plain),
        _ => path,
    }
}

pub fn uri(path: &Path) -> String {
    url::Url::from_file_path(canonical(path))
        .unwrap()
        .to_string()
}

/// URI of a file that may not exist (its directory is canonicalized when it exists).
pub fn uri_in(dir: &Path, rel: &str) -> String {
    url::Url::from_file_path(canonical(dir).join(rel))
        .unwrap()
        .to_string()
}

pub fn codes(diags: &[Value]) -> Vec<String> {
    diags
        .iter()
        .map(|d| d["code"].as_str().unwrap().to_owned())
        .collect()
}
