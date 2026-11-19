//! `heki server --stdio` end to end over JSON-RPC (docs/DETAILED_SPEC_M3_M7.md section 5.3):
//! initialize → initialized → didOpen → publishDiagnostics → codeAction → didChange (Early
//! Cutoff) → didChange + didSave (propagation to callers and importers) → shutdown → exit.

mod common;

use common::Project;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(20);

struct Lsp {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<Value>,
    next_id: u64,
    /// Messages read while waiting for something else.
    backlog: Vec<Value>,
}

impl Lsp {
    fn start(dir: &Path, args: &[&str]) -> Self {
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
        }
    }

    fn write(&mut self, msg: &Value) {
        let body = msg.to_string();
        let stdin = self.stdin.as_mut().unwrap();
        write!(stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        stdin.flush().unwrap();
    }

    /// `shutdown` + `exit`; `close` also closes stdin like an editor does.
    fn stop(mut self, close: bool) {
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

    fn message(method: &str, params: Value) -> Value {
        let mut msg = json!({"jsonrpc": "2.0", "method": method});
        if !params.is_null() {
            msg["params"] = params;
        }
        msg
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.write(&Self::message(method, params));
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
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
    fn wait(&mut self, pred: impl Fn(&Value) -> bool) -> Value {
        if let Some(i) = self.backlog.iter().position(&pred) {
            return self.backlog.remove(i);
        }
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let msg = self
                .rx
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("timed out; backlog: {:#?}", self.backlog));
            if pred(&msg) {
                return msg;
            }
            self.backlog.push(msg);
        }
    }

    /// The next `publishDiagnostics` for `uri` satisfying `pred`.
    fn diagnostics(&mut self, uri: &str, pred: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        let msg = self.wait(|m| {
            m["method"] == "textDocument/publishDiagnostics"
                && m["params"]["uri"] == uri
                && pred(m["params"]["diagnostics"].as_array().unwrap())
        });
        msg["params"]["diagnostics"].as_array().unwrap().clone()
    }
}

fn uri(path: &Path) -> String {
    format!("file://{}", std::fs::canonicalize(path).unwrap().display())
}

fn codes(diags: &[Value]) -> Vec<String> {
    diags
        .iter()
        .map(|d| d["code"].as_str().unwrap().to_owned())
        .collect()
}

const UTIL: &str = "def helper():\n    return 1\n";
const API: &str = "from pkg.util import helper\n\n\nasync def handler():\n    helper()\n\n\ndef build(items):\n    x = []\n    for i in items:\n        x = x + [i]\n    return x\n";

#[test]
fn lsp_session_publishes_fixes_and_cuts_off_unchanged_summaries() {
    let p = Project::new("lsp");
    p.write("pkg/__init__.py", "")
        .write("pkg/util.py", UTIL)
        .write("pkg/api.py", API);
    let api = uri(&p.dir.join("pkg/api.py"));
    let util = uri(&p.dir.join("pkg/util.py"));
    let mut lsp = Lsp::start(&p.dir, &["server", "--stdio"]);

    let init = lsp.request(
        "initialize",
        json!({"processId": null, "rootUri": uri(&p.dir), "capabilities": {}}),
    );
    assert_eq!(init["serverInfo"]["name"], "heki");
    assert_eq!(init["capabilities"]["textDocumentSync"]["change"], 1);
    assert_eq!(
        init["capabilities"]["codeActionProvider"]["codeActionKinds"],
        json!(["quickfix", "refactor.rewrite.heki"])
    );
    lsp.notify("initialized", json!({}));
    lsp.notify(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": api, "languageId": "python", "version": 1, "text": API}}),
    );
    let diags = lsp.diagnostics(&api, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["OPT101"]);
    let opt = &diags[0];
    assert_eq!(opt["source"], "heki");
    assert_eq!(
        opt["range"],
        json!({"start": {"line": 10, "character": 8}, "end": {"line": 10, "character": 19}})
    );

    // Quick fix + suppression for the OPT101 diagnostic.
    let actions = lsp.request(
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": api},
            "range": opt["range"],
            "context": {"diagnostics": [opt]}
        }),
    );
    let actions = actions.as_array().unwrap();
    assert_eq!(actions.len(), 2, "{actions:#?}");
    assert_eq!(actions[0]["kind"], "quickfix");
    assert_eq!(actions[0]["isPreferred"], true);
    assert_eq!(
        actions[0]["edit"]["changes"][&api][0]["newText"],
        "x.append(i)"
    );
    assert_eq!(
        actions[1]["edit"]["changes"][&api][0]["newText"],
        "  # heki: ignore[OPT101]"
    );
    assert_eq!(
        actions[1]["edit"]["changes"][&api][0]["range"]["start"],
        json!({"line": 10, "character": 19})
    );

    // A comment-only edit: Early Cutoff, no other file re-analyzed.
    let commented = format!("# a comment\n{API}");
    lsp.notify(
        "textDocument/didChange",
        json!({"textDocument": {"uri": api, "version": 2}, "contentChanges": [{"text": commented}]}),
    );
    let diags = lsp.diagnostics(&api, |d| {
        d.first().is_some_and(|x| x["range"]["start"]["line"] == 11)
    });
    assert_eq!(codes(&diags), vec!["OPT101"]);
    let stats = lsp.request("heki/stats", Value::Null);
    assert_eq!(stats["ready"], true);
    assert_eq!(stats["last_summary_changed"], false, "{stats}");
    assert_eq!(stats["last_decls_changed"], false, "{stats}");
    assert_eq!(stats["dependent_reanalyses"], 0, "{stats}");
    assert_eq!(stats["indexed_files"], 3);

    // `helper` now blocks: its caller `handler` (another file) gets EFF101 on save.
    lsp.notify(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": util, "languageId": "python", "version": 1, "text": UTIL}}),
    );
    let blocking = "import time\n\n\ndef helper():\n    time.sleep(1)\n    return 1\n";
    lsp.notify(
        "textDocument/didChange",
        json!({"textDocument": {"uri": util, "version": 2}, "contentChanges": [{"text": blocking}]}),
    );
    std::fs::write(p.dir.join("pkg/util.py"), blocking).unwrap();
    lsp.notify(
        "textDocument/didSave",
        json!({"textDocument": {"uri": util}, "text": blocking}),
    );
    let diags = lsp.diagnostics(&api, |d| codes(d).contains(&"EFF101".to_owned()));
    let eff = diags.iter().find(|d| d["code"] == "EFF101").unwrap();
    let related = eff["relatedInformation"].as_array().unwrap();
    assert_eq!(related[0]["location"]["uri"], api);
    assert_eq!(related[1]["location"]["uri"], util);
    assert_eq!(
        related[1]["location"]["range"]["start"],
        json!({"line": 4, "character": 4})
    );
    let stats = lsp.request("heki/stats", Value::Null);
    assert!(
        stats["dependent_reanalyses"].as_u64().unwrap() >= 1,
        "{stats}"
    );

    // A declaration change (new public function) re-extracts importers on save.
    let extended = format!("{blocking}\n\ndef other():\n    return 2\n");
    lsp.notify(
        "textDocument/didChange",
        json!({"textDocument": {"uri": util, "version": 3}, "contentChanges": [{"text": extended}]}),
    );
    lsp.diagnostics(&util, |_| true);
    let stats = lsp.request("heki/stats", Value::Null);
    assert_eq!(stats["last_decls_changed"], true, "{stats}");

    // Closing clears the document's diagnostics; shutdown + exit ends the process cleanly.
    lsp.notify(
        "textDocument/didClose",
        json!({"textDocument": {"uri": api}}),
    );
    lsp.diagnostics(&api, <[Value]>::is_empty);
    lsp.stop(true);
}

#[test]
fn lsp_alias_and_files_outside_the_project() {
    let p = Project::new("lsp_alias");
    p.write(
        "a.py",
        "def f(items):\n    x = []\n    for i in items:\n        x = x + [i]\n    return x\n",
    );
    let mut lsp = Lsp::start(&p.dir, &["lsp"]);
    lsp.request(
        "initialize",
        json!({"processId": null, "workspaceFolders": [{"uri": uri(&p.dir), "name": "t"}], "capabilities": {}}),
    );
    // Opened before indexing finishes: diagnostics come once the index is ready.
    let a = uri(&p.dir.join("a.py"));
    lsp.notify(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": a, "languageId": "python", "version": 1, "text": p.read("a.py")}}),
    );
    lsp.notify("initialized", json!({}));
    let diags = lsp.diagnostics(&a, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["OPT101"]);
    // A syntax error clears the diagnostics instead of keeping stale ones.
    lsp.notify(
        "textDocument/didChange",
        json!({"textDocument": {"uri": a, "version": 2}, "contentChanges": [{"text": "def f(:\n"}]}),
    );
    lsp.diagnostics(&a, <[Value]>::is_empty);
    // Code actions on an unknown document.
    let none = lsp.request(
        "textDocument/codeAction",
        json!({"textDocument": {"uri": "file:///nowhere.py"}, "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "context": {"diagnostics": []}}),
    );
    assert_eq!(none, Value::Null);
    // A client that keeps stdin open after `exit`: the server still ends.
    lsp.stop(false);
}
