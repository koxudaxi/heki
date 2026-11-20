//! A minimal LSP client over `heki server --stdio` (JSON-RPC with `Content-Length` framing).

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

pub const TIMEOUT: Duration = Duration::from_secs(20);

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
    pub fn start(dir: &Path, args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_heki"))
            .args(args)
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

    /// The next message from the server (`None` on timeout), recorded on arrival: published
    /// diagnostics update [`Lsp::latest`], and a request of the server (`client/registerCapability`)
    /// is answered with `null`.
    fn receive(&mut self, timeout: Duration) -> Option<Value> {
        let msg = self.rx.recv_timeout(timeout).ok()?;
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
        Some(msg)
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
        let started = Instant::now();
        let status = self.child.wait().unwrap();
        assert!(status.success());
        assert!(started.elapsed() < TIMEOUT);
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
        let reply = self.wait(|m| m["id"] == id && m.get("method").is_none());
        assert!(reply.get("error").is_none(), "{method}: {reply}");
        reply["result"].clone()
    }

    /// The first message (backlog first) matching `pred`.
    pub fn wait(&mut self, pred: impl Fn(&Value) -> bool) -> Value {
        if let Some(i) = self.backlog.iter().position(&pred) {
            return self.backlog.remove(i);
        }
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let msg = self
                .receive(left)
                .unwrap_or_else(|| panic!("timed out; backlog: {:#?}", self.backlog));
            if pred(&msg) {
                return msg;
            }
            self.backlog.push(msg);
        }
    }

    /// Wait until the server is idle and every diagnostic it published has arrived: poll
    /// `heki/stats` until no work is in flight, then read until `published` notifications have
    /// been received. Returns the final stats. Deterministic: it depends on what the server
    /// reports, not on how long anything takes.
    pub fn settle(&mut self) -> Value {
        let started = Instant::now();
        let stats = loop {
            let stats = self.request("heki/stats", Value::Null);
            if stats["in_flight"] == 0 {
                break stats;
            }
            assert!(
                started.elapsed() < TIMEOUT,
                "the server never became idle: {stats}"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let published = stats["published"].as_u64().unwrap();
        while self.published < published {
            let left = TIMEOUT.saturating_sub(started.elapsed());
            let msg = self.receive(left).unwrap_or_else(|| {
                panic!(
                    "{} of {published} publications arrived; backlog: {:#?}",
                    self.published, self.backlog
                )
            });
            self.backlog.push(msg);
        }
        stats
    }

    /// The next `publishDiagnostics` for `uri` satisfying `pred`.
    pub fn diagnostics(&mut self, uri: &str, pred: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        let msg = self.wait(|m| {
            m["method"] == "textDocument/publishDiagnostics"
                && m["params"]["uri"] == uri
                && pred(m["params"]["diagnostics"].as_array().unwrap())
        });
        msg["params"]["diagnostics"].as_array().unwrap().clone()
    }

    /// Wait until the server has applied `n` open documents (`heki/stats`): notifications are
    /// handled concurrently, so a later `initialized` could otherwise index first.
    pub fn opened(&mut self, n: usize) {
        let started = Instant::now();
        while self.request("heki/stats", Value::Null)["open_documents"] != n {
            assert!(
                started.elapsed() < TIMEOUT,
                "the documents were never opened"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait until the background index is ready (`heki/stats`).
    pub fn ready(&mut self) {
        let started = Instant::now();
        while self.request("heki/stats", Value::Null)["ready"] != true {
            assert!(started.elapsed() < TIMEOUT, "the index never became ready");
            std::thread::sleep(Duration::from_millis(20));
        }
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
        self.wait(|m| {
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
