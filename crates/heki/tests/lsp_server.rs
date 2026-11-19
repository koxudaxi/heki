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

/// URI of a file that may not exist (its directory is canonicalized when it exists).
fn uri_in(dir: &Path, rel: &str) -> String {
    format!(
        "file://{}/{rel}",
        std::fs::canonicalize(dir).unwrap().display()
    )
}

impl Lsp {
    /// Wait until the background index is ready (`heki/stats`).
    fn ready(&mut self) {
        let started = Instant::now();
        while self.request("heki/stats", Value::Null)["ready"] != true {
            assert!(started.elapsed() < TIMEOUT, "the index never became ready");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn open(&mut self, uri: &str, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument": {"uri": uri, "languageId": "python", "version": 1, "text": text}}),
        );
    }

    fn change(&mut self, uri: &str, version: i32, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({"textDocument": {"uri": uri, "version": version}, "contentChanges": [{"text": text}]}),
        );
    }

    /// The next `window/logMessage` containing `needle`.
    fn log(&mut self, needle: &str) -> Value {
        self.wait(|m| {
            m["method"] == "window/logMessage"
                && m["params"]["message"]
                    .as_str()
                    .is_some_and(|t| t.contains(needle))
        })
    }

    fn code_actions(&mut self, uri: &str, range: &Value, diags: &[Value]) -> Vec<Value> {
        self.request(
            "textDocument/codeAction",
            json!({"textDocument": {"uri": uri}, "range": range, "context": {"diagnostics": diags}}),
        )
        .as_array()
        .unwrap()
        .clone()
    }
}

const UNSAVED: &str = "from functools import cached_property\n\n\nclass Box:\n    def __init__(self):\n        self.items = []\n        self._setup()\n\n    def _setup(self):\n        self.items.append(0)\n\n    @cached_property\n    def total(self):\n        return len(self.items)\n\n    def add(self, x):\n        self.items.append(x)\n\n\ndef build(items):\n    x = []\n    for i in items:\n        x = x + [i]\n    return x\n";

#[test]
fn lsp_unsaved_and_outside_documents_and_stats_before_indexing() {
    let p = Project::new("lsp_unsaved");
    let outside = Project::new("lsp_unsaved_outside");
    p.write("pkg/__init__.py", "").write("pkg/util.py", UTIL);
    outside.write("x.py", API);
    let mut lsp = Lsp::start(&p.dir, &["server", "--stdio"]);
    // No root in `initialize`: the working directory is the workspace.
    lsp.request(
        "initialize",
        json!({"processId": null, "rootUri": null, "capabilities": {}}),
    );
    assert_eq!(
        lsp.request("heki/stats", Value::Null),
        json!({"ready": false})
    );
    // Opened before indexing: a new file that is not saved yet, a file outside the
    // workspace, and a file in a directory that does not exist.
    let unsaved = uri_in(&p.dir, "pkg/new_mod.py");
    let other = uri(&outside.dir.join("x.py"));
    let lost = uri_in(&p.dir, "nowhere/y.py");
    lsp.open(&unsaved, UNSAVED);
    lsp.open(&other, API);
    lsp.open(
        &lost,
        "def f(items):\n    x = []\n    for i in items:\n        x = x + [i]\n    return x\n",
    );
    lsp.notify("initialized", json!({}));
    // The unsaved file is checked against an index that does not know it yet: its local
    // rules run (`OPT101`), its interprocedural facts wait for the first edit.
    let diags = lsp.diagnostics(&unsaved, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["OPT101"]);
    assert_eq!(
        diags[0]["range"]["start"],
        json!({"line": 22, "character": 8})
    );
    lsp.diagnostics(&other, <[Value]>::is_empty);
    let diags = lsp.diagnostics(&lost, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["OPT101"]);
    lsp.ready();
    // Once indexed: the first edit summarizes the new file (`MUT101` needs its summaries).
    lsp.change(&unsaved, 2, UNSAVED);
    let diags = lsp.diagnostics(&unsaved, |d| d.len() == 2);
    assert_eq!(codes(&diags), vec!["MUT101", "OPT101"]);
    // A file outside the workspace opened later gets no diagnostics either.
    let later = uri_in(&outside.dir, "later.py");
    lsp.open(&later, API);
    lsp.diagnostics(&later, <[Value]>::is_empty);
    // Saving a file that is neither open nor on disk does nothing.
    lsp.notify(
        "textDocument/didSave",
        json!({"textDocument": {"uri": uri_in(&p.dir, "pkg/ghost.py")}}),
    );
    let stats = lsp.request("heki/stats", Value::Null);
    assert_eq!(stats["edited_reanalyses"], 1, "{stats}");
    lsp.stop(true);
}

const BOX: &str = "from functools import cached_property\n\n\nclass Box:\n    def __init__(self):\n        self.items = []\n\n    @cached_property\n    def total(self):\n        return len(self.items)\n\n    def add(self, x):\n        self.items.append(x)\n\n    def drop(self):\n        self.items.clear()\n";

#[test]
fn lsp_unsaved_module_next_to_its_indexed_stub() {
    let p = Project::new("lsp_stub");
    p.write("pkg/__init__.py", "").write(
        "pkg/box.pyi",
        "from functools import cached_property\n\n\nclass Box:\n    @cached_property\n    def total(self) -> int: ...\n    def add(self, x: int) -> None: ...\n",
    );
    let mut lsp = Lsp::start(&p.dir, &["server", "--stdio"]);
    lsp.request(
        "initialize",
        json!({"processId": null, "rootUri": uri(&p.dir), "capabilities": {}}),
    );
    // Opened before indexing and not saved: the index knows `pkg.box` from its stub only.
    let module = uri_in(&p.dir, "pkg/box.py");
    lsp.open(&module, BOX);
    lsp.notify("initialized", json!({}));
    // The stub's summaries do not stand in for the document's own methods (the index knows
    // `Box.add` from the stub only, and `Box.drop` not at all): no `MUT101` before the
    // document is summarized.
    lsp.diagnostics(&module, <[Value]>::is_empty);
    lsp.ready();
    lsp.change(&module, 2, BOX);
    let diags = lsp.diagnostics(&module, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["MUT101", "MUT101"]);
    assert_eq!(
        diags[1]["range"]["start"],
        json!({"line": 15, "character": 8})
    );
    lsp.stop(true);
}

#[cfg(unix)]
#[test]
fn lsp_without_a_root_in_a_removed_working_directory() {
    // Neither `initialize` nor the process names a root: the working directory is removed
    // after the server starts, so the server indexes `.`, which holds nothing.
    let dir = std::env::temp_dir().join(format!("heki_e2e_lsp_gone_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut lsp = Lsp::start(&dir, &["server", "--stdio"]);
    std::fs::remove_dir(&dir).unwrap();
    lsp.request(
        "initialize",
        json!({"processId": null, "rootUri": null, "capabilities": {}}),
    );
    lsp.notify("initialized", json!({}));
    let log = lsp.log("heki: indexed 0 file(s)");
    assert_eq!(log["params"]["type"], 3, "{log}");
    lsp.ready();
    assert_eq!(lsp.request("heki/stats", Value::Null)["indexed_files"], 0);
    lsp.stop(true);
}

#[test]
fn lsp_debounce_close_and_pending_refresh_of_unopened_files() {
    let p = Project::new("lsp_pending");
    let caller = "from pkg.util import helper\n\n\nasync def handler():\n    helper()\n";
    // Write owners make every per-file check look at the other indexed files too.
    p.write(
        "heki.toml",
        "[lint.severity]\nEFF101 = \"warning\"\n\n[[write-owners]]\nname = \"store\"\ntargets = [\"Model.fields\"]\nallowed-writers = [\"Store.*\"]\n",
    )
        .write("pkg/__init__.py", "")
        .write("pkg/util.py", UTIL)
        .write("pkg/api.py", caller)
        .write("pkg/gone.py", caller)
        .write("pkg/bad.py", caller);
    let util = uri(&p.dir.join("pkg/util.py"));
    let api = uri(&p.dir.join("pkg/api.py"));
    let mut lsp = Lsp::start(&p.dir, &["server", "--stdio"]);
    lsp.request(
        "initialize",
        json!({"processId": null, "rootUri": uri(&p.dir), "capabilities": {}}),
    );
    lsp.notify("initialized", json!({}));
    lsp.ready();
    lsp.open(&util, UTIL);
    lsp.diagnostics(&util, <[Value]>::is_empty);
    // Two edits inside the debounce: only the last one is analyzed.
    let blocking = "import time\n\n\ndef helper():\n    time.sleep(1)\n";
    lsp.change(
        &util,
        2,
        "import time\n\n\ndef helper():\n    time.sleep(2)\n",
    );
    lsp.change(&util, 3, blocking);
    lsp.diagnostics(&util, <[Value]>::is_empty);
    let stats = lsp.request("heki/stats", Value::Null);
    assert_eq!(
        stats["edited_reanalyses"], 2,
        "open + one debounced edit: {stats}"
    );
    // Before the save, one queued caller disappears and another stops parsing.
    std::fs::remove_file(p.dir.join("pkg/gone.py")).unwrap();
    p.write("pkg/bad.py", "def handler(:\n");
    // A save without text re-reads the open document; queued callers come from the disk.
    lsp.notify(
        "textDocument/didSave",
        json!({"textDocument": {"uri": util}}),
    );
    let diags = lsp.diagnostics(&api, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["EFF101"]);
    assert_eq!(diags[0]["severity"], 2, "lint.severity = warning");
    let related = diags[0]["relatedInformation"].as_array().unwrap();
    assert_eq!(related[0]["location"]["uri"], api);
    assert_eq!(related[1]["location"]["uri"], util);
    let log = lsp.log("syntax error");
    assert_eq!(log["params"]["type"], 2);
    assert!(
        log["params"]["message"]
            .as_str()
            .unwrap()
            .contains("pkg/bad.py:1:13"),
        "{log}"
    );
    // Closing a document inside the debounce drops its pending analysis.
    lsp.change(&util, 4, UTIL);
    lsp.notify(
        "textDocument/didClose",
        json!({"textDocument": {"uri": util}}),
    );
    lsp.diagnostics(&util, <[Value]>::is_empty);
    std::thread::sleep(DEBOUNCE_WAIT);
    let stats = lsp.request("heki/stats", Value::Null);
    assert_eq!(stats["edited_reanalyses"], 3, "{stats}");
    assert_eq!(stats["dependent_reanalyses"], 1, "{stats}");
    lsp.stop(true);
}

/// Longer than the server's 250 ms debounce.
const DEBOUNCE_WAIT: Duration = Duration::from_millis(600);

#[test]
fn lsp_code_actions_for_rewrites_plain_diagnostics_and_other_ranges() {
    let p = Project::new("lsp_actions");
    let text = "import time\n\n\ndef member(n):\n    allowed = list(range(0, 50, 3))\n    hits = []\n    for p in range(n):\n        if p in allowed:\n            hits.append(p)\n    return hits\n\n\nasync def handler():\n    time.sleep(1)\n";
    p.write("a.py", text);
    let a = uri(&p.dir.join("a.py"));
    let mut lsp = Lsp::start(&p.dir, &["server", "--stdio"]);
    lsp.request(
        "initialize",
        json!({"processId": null, "rootUri": uri(&p.dir), "capabilities": {}}),
    );
    lsp.notify("initialized", json!({}));
    lsp.ready();
    lsp.open(&a, text);
    let diags = lsp.diagnostics(&a, |d| d.len() == 2);
    assert_eq!(codes(&diags), vec!["OPT110", "EFF101"]);
    // A proven-safe size-dependent rewrite is a `refactor.rewrite.heki` action.
    let actions = lsp.code_actions(&a, &diags[0]["range"], &diags[..1]);
    assert_eq!(actions.len(), 2, "{actions:#?}");
    assert_eq!(actions[0]["kind"], "refactor.rewrite.heki");
    assert_eq!(
        actions[0]["title"],
        "Apply the proven-safe OPT110 rewrite (size-dependent)"
    );
    assert_eq!(actions[0]["isPreferred"], false);
    assert_eq!(
        actions[1]["title"],
        "Suppress OPT110 with `# heki: ignore[OPT110]`"
    );
    // A diagnostic without a fix only offers the suppression.
    let actions = lsp.code_actions(&a, &diags[1]["range"], &diags[1..]);
    assert_eq!(actions.len(), 1, "{actions:#?}");
    assert_eq!(
        actions[0]["edit"]["changes"][&a][0]["range"]["start"],
        json!({"line": 13, "character": 17})
    );
    // A range away from every diagnostic has no actions.
    let none = json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}});
    assert!(lsp.code_actions(&a, &none, &[]).is_empty());
    lsp.stop(true);
}

#[test]
fn lsp_reports_an_invalid_configuration() {
    let p = Project::new("lsp_bad_config");
    p.write("heki.toml", "[lint]\nselect = [\"NOPE1\"]\n")
        .write("a.py", UTIL);
    let mut lsp = Lsp::start(&p.dir, &["lsp"]);
    lsp.request(
        "initialize",
        json!({"processId": null, "rootUri": uri(&p.dir), "capabilities": {}}),
    );
    lsp.notify("initialized", json!({}));
    let log = lsp.log("unknown rule selector `NOPE1`");
    assert_eq!(log["params"]["type"], 2);
    assert_eq!(
        lsp.request("heki/stats", Value::Null),
        json!({"ready": false})
    );
    // Without a workspace, documents are not analyzed (the request reads every message
    // sent before its reply).
    let a = uri(&p.dir.join("a.py"));
    lsp.open(&a, UTIL);
    std::thread::sleep(DEBOUNCE_WAIT);
    assert_eq!(lsp.request("heki/stats", Value::Null)["ready"], false);
    assert!(
        !lsp.backlog
            .iter()
            .any(|m| m["method"] == "textDocument/publishDiagnostics"),
        "{:#?}",
        lsp.backlog
    );
    lsp.stop(true);
}
