//! `heki server --stdio` end to end over JSON-RPC (docs/lsp.md):
//! initialize → initialized → didOpen → publishDiagnostics → codeAction → didChange (Early
//! Cutoff) → didChange + didSave (propagation to callers and importers) → shutdown → exit.

mod common;

use common::Project;
use common::lsp::{Lsp, codes, uri, uri_in};
use serde_json::{Value, json};

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
    // The rule's certainty, and its page, which the site's address names.
    assert_eq!(opt["data"], json!({"certainty": "exact"}));
    let site = common::repo_file("zensical.toml")
        .lines()
        .find_map(|l| {
            l.strip_prefix("site_url = \"")?
                .strip_suffix('"')
                .map(str::to_owned)
        })
        .unwrap();
    assert_eq!(
        opt["codeDescription"]["href"],
        format!("{site}/rules/list-quadratic-concat/")
    );
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
    // The message is the sentence alone, labelled for a heuristic rule; the help follows the
    // hops, at the diagnostic itself.
    assert_eq!(
        eff["message"],
        "`handler` reaches the blocking call `time.sleep` on the event loop [heuristic]"
    );
    assert_eq!(related.len(), 3, "{related:#?}");
    assert_eq!(related[2]["location"]["uri"], api);
    assert_eq!(related[2]["location"]["range"], eff["range"]);
    assert_eq!(
        related[2]["message"],
        "help: use `await asyncio.sleep(...)`, or offload it with `asyncio.to_thread`"
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
    // Once the debounced analysis of this edit is published (not an earlier publication).
    let stats = lsp.settle();
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

const UNSAVED: &str = "from functools import cached_property\n\n\nclass Box:\n    def __init__(self):\n        self.items = []\n        self._setup()\n\n    def _setup(self):\n        self.items.append(0)\n\n    @cached_property\n    def total(self):\n        return len(self.items)\n\n    def add(self, x):\n        self.items.append(x)\n\n\ndef build(items):\n    x = []\n    for i in items:\n        x = x + [i]\n    return x\n";

#[test]
fn lsp_unsaved_and_outside_documents_and_stats_before_indexing() {
    let p = Project::new("lsp_unsaved");
    let outside = Project::new("lsp_unsaved_outside");
    p.write("pkg/__init__.py", "")
        .write("pkg/util.py", UTIL)
        .write("heki.toml", "[lint]\nextend-select = [\"MUT101\"]\n");
    outside.write("x.py", API);
    let mut lsp = Lsp::start(&p.dir, &["server", "--stdio"]);
    // No root in `initialize`: the working directory is the workspace.
    lsp.request(
        "initialize",
        json!({"processId": null, "rootUri": null, "capabilities": {}}),
    );
    assert_eq!(
        lsp.request("heki/stats", Value::Null),
        json!({"ready": false, "open_documents": 0, "in_flight": 0, "published": 0})
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
    // The unsaved files are indexed with the project, as they will be once saved: their
    // interprocedural facts (`MUT101`) are there from the start.
    let diags = lsp.diagnostics(&unsaved, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["MUT101", "OPT101"]);
    assert_eq!(
        diags[1]["range"]["start"],
        json!({"line": 22, "character": 8})
    );
    lsp.diagnostics(&other, <[Value]>::is_empty);
    let diags = lsp.diagnostics(&lost, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["OPT101"]);
    lsp.ready();
    // Whether the documents opened before indexing are analyzed again once the index is ready
    // depends on which of their analyses ran first; count the edits from here.
    let edits_before = lsp.settle()["edited_reanalyses"].as_u64().unwrap();
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
    let stats = lsp.settle();
    assert_eq!(stats["edited_reanalyses"], edits_before + 1, "{stats}");
    lsp.stop(true);
}

const BOX: &str = "from functools import cached_property\n\n\nclass Box:\n    def __init__(self):\n        self.items = []\n\n    @cached_property\n    def total(self):\n        return len(self.items)\n\n    def add(self, x):\n        self.items.append(x)\n\n    def drop(self):\n        self.items.clear()\n";

#[test]
fn lsp_unsaved_module_next_to_its_indexed_stub() {
    let p = Project::new("lsp_stub");
    p.write("pkg/__init__.py", "")
        .write("heki.toml", "[lint]\nextend-select = [\"MUT101\"]\n")
        .write(
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
    lsp.opened(1);
    lsp.notify("initialized", json!({}));
    // The document is indexed with the project and outranks the stub: its own methods are
    // summarized (`Box.add`, and `Box.drop`, which the stub does not declare).
    let diags = lsp.diagnostics(&module, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["MUT101", "MUT101"]);
    lsp.ready();
    lsp.change(&module, 2, BOX);
    let diags = lsp.diagnostics(&module, |d| d.len() == 2);
    assert_eq!(codes(&diags), vec!["MUT101", "MUT101"]);
    assert_eq!(
        diags[1]["range"]["start"],
        json!({"line": 15, "character": 8})
    );
    lsp.stop(true);
}

#[test]
fn lsp_without_a_root_indexes_the_working_directory_its_documents_are_in() {
    // Neither `initialize` nor a workspace folder names a root, so the server indexes its
    // working directory. The process may get that directory spelled otherwise than the
    // editor's document paths (an 8.3 short name such as `RUNNER~1` on Windows), and the
    // documents still belong to the indexed project.
    let p = Project::new("lsp_no_root");
    let blocking = "import time\n\n\ndef helper():\n    time.sleep(1)\n";
    let caller = "from pkg.util import helper\n\n\nasync def handler():\n    helper()\n";
    p.write("pkg/__init__.py", "")
        .write("pkg/util.py", blocking)
        .write("pkg/api.py", caller);
    let api = uri(&p.dir.join("pkg/api.py"));
    let mut lsp = Lsp::start(&p.dir, &["server", "--stdio"]);
    lsp.request(
        "initialize",
        json!({"processId": null, "rootUri": null, "capabilities": {}}),
    );
    lsp.notify("initialized", json!({}));
    lsp.ready();
    lsp.open(&api, caller);
    let diags = lsp.diagnostics(&api, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["EFF101"]);
    lsp.stop(true);
}

// Unix only: Windows does not remove a directory that is a process's working directory.
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
        "[lint.severity]\nEFF101 = \"error\"\n\n[[write-owners]]\nname = \"store\"\ntargets = [\"Model.fields\"]\nallowed-writers = [\"Store.*\"]\n",
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
    assert_eq!(diags[0]["severity"], 1, "lint.severity = error");
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
    let stats = lsp.settle();
    assert_eq!(stats["edited_reanalyses"], 3, "{stats}");
    // `api.py`, then `util.py`: the callers that left the project (`gone.py`, `bad.py`) took
    // their calls of `helper` with them.
    assert_eq!(stats["dependent_reanalyses"], 2, "{stats}");
    assert_eq!(stats["indexed_files"], 3, "{stats}");
    lsp.stop(true);
}

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
        lsp.settle(),
        json!({"ready": false, "open_documents": 0, "in_flight": 0, "published": 0})
    );
    // Without a workspace, documents are not analyzed.
    let a = uri(&p.dir.join("a.py"));
    lsp.open(&a, UTIL);
    assert_eq!(
        lsp.settle(),
        json!({"ready": false, "open_documents": 1, "in_flight": 0, "published": 0})
    );
    lsp.stop(true);
}

#[test]
fn lsp_applies_document_changes_in_message_order() {
    let p = Project::new("lsp_order");
    let quadratic =
        "def f(items):\n    x = []\n    for i in items:\n        x = x + [i]\n    return x\n";
    p.write("a.py", quadratic).write("b.py", quadratic);
    let (a, b) = (uri(&p.dir.join("a.py")), uri(&p.dir.join("b.py")));
    let mut lsp = Lsp::start(&p.dir, &["server", "--stdio"]);
    lsp.request(
        "initialize",
        json!({"processId": null, "rootUri": uri(&p.dir), "capabilities": {}}),
    );
    lsp.notify("initialized", json!({}));
    lsp.ready();
    // Sent back to back: the change applies to the opened document, whatever the analysis of
    // the open does meanwhile.
    lsp.open(&a, quadratic);
    lsp.change(&a, 2, "def f(items):\n    return list(items)\n");
    // A document closed right after it was opened stays closed.
    lsp.open(&b, quadratic);
    lsp.notify("textDocument/didClose", json!({"textDocument": {"uri": b}}));
    let stats = lsp.settle();
    assert_eq!(stats["open_documents"], 1, "{stats}");
    assert_eq!(lsp.latest[&a], Vec::<Value>::new());
    assert_eq!(lsp.latest[&b], Vec::<Value>::new());
    lsp.stop(true);
}

/// A server indexing `p`, ready for documents (`capabilities` of the client).
fn ready_server(p: &Project, capabilities: Value) -> Lsp {
    let mut lsp = Lsp::start(&p.dir, &["server", "--stdio"]);
    lsp.request(
        "initialize",
        json!({"processId": null, "rootUri": uri(&p.dir), "capabilities": capabilities}),
    );
    lsp.notify("initialized", json!({}));
    lsp.ready();
    lsp
}

#[test]
fn lsp_positions_and_suppressions_in_documents_with_cr_line_breaks() {
    let p = Project::new("lsp_cr");
    let text = "import time  # heki: ignore[EFF101]\r\rasync def h():\r    time.sleep(1)\r";
    p.write("a.py", text);
    let a = uri(&p.dir.join("a.py"));
    let mut lsp = ready_server(&p, json!({}));
    lsp.open(&a, text);
    let diags = lsp.diagnostics(&a, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["EFF101"]);
    assert_eq!(
        diags[0]["range"]["start"],
        json!({"line": 3, "character": 4})
    );
    // The suppression goes at the end of that line, before its line break.
    let actions = lsp.code_actions(&a, &diags[0]["range"], &diags);
    assert_eq!(
        actions[0]["edit"]["changes"][&a][0]["range"]["start"],
        json!({"line": 3, "character": 17})
    );
    lsp.stop(true);
}

#[test]
fn lsp_withholds_suppressions_that_would_land_in_a_continuation_or_a_string() {
    let p = Project::new("lsp_suppress_where");
    let text = "import time\n\n\nasync def a():\n    return time.sleep(1) \\\n        or 0\n\n\nasync def b():\n    return time.sleep(2) or \"\"\"x\n    y\"\"\"\n\n\nasync def c():\n    return (time.sleep(3),\n            0)\n";
    p.write("a.py", text);
    let a = uri(&p.dir.join("a.py"));
    let mut lsp = ready_server(&p, json!({}));
    lsp.open(&a, text);
    let diags = lsp.diagnostics(&a, |d| d.len() == 3);
    assert_eq!(codes(&diags), vec!["EFF101", "EFF101", "EFF101"]);
    // A comment after the `\` would end the statement there; one on the `"""` line would be
    // part of the string. Inside brackets a comment is fine.
    for (d, offered) in diags.iter().zip([false, false, true]) {
        let actions = lsp.code_actions(&a, &d["range"], std::slice::from_ref(d));
        assert_eq!(!actions.is_empty(), offered, "{d}: {actions:#?}");
    }
    lsp.stop(true);
}

#[test]
fn lsp_exit_codes_follow_the_shutdown_handshake() {
    let p = Project::new("lsp_exit");
    let status = |lsp: &mut Lsp| {
        let started = std::time::Instant::now();
        loop {
            if let Some(status) = lsp.child.try_wait().unwrap() {
                return status.code();
            }
            assert!(
                started.elapsed() < common::lsp::TIMEOUT,
                "the server never ended"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    };
    // A client may take its time between `shutdown` and `exit`; `exit` ends the process with 0
    // even while stdin stays open.
    let mut lsp = ready_server(&p, json!({}));
    assert_eq!(lsp.request("shutdown", Value::Null), Value::Null);
    std::thread::sleep(std::time::Duration::from_millis(2500));
    assert!(
        lsp.child.try_wait().unwrap().is_none(),
        "ended before `exit`"
    );
    lsp.notify("exit", Value::Null);
    assert_eq!(status(&mut lsp), Some(0));
    // `exit` without `shutdown`, or stdin closing without either: 1.
    let mut lsp = ready_server(&p, json!({}));
    lsp.notify("exit", Value::Null);
    assert_eq!(status(&mut lsp), Some(1));
    let mut lsp = ready_server(&p, json!({}));
    lsp.stdin = None;
    assert_eq!(status(&mut lsp), Some(1));
}

#[test]
fn lsp_code_actions_only_edit_the_analyzed_version() {
    let p = Project::new("lsp_stale_actions");
    let text =
        "def f(items):\n    x = []\n    for i in items:\n        x = x + [i]\n    return x\n";
    p.write("a.py", text);
    let a = uri(&p.dir.join("a.py"));
    let mut lsp = ready_server(
        &p,
        json!({"workspace": {"workspaceEdit": {"documentChanges": true}}}),
    );
    lsp.open(&a, text);
    let diags = lsp.diagnostics(&a, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["OPT101"]);
    // An edit inside the debounce: the published fix was computed on version 1.
    lsp.change(&a, 2, &text.replace("x + [i]", "x + [0]"));
    let stale = lsp.request(
        "textDocument/codeAction",
        json!({"textDocument": {"uri": a}, "range": diags[0]["range"], "context": {"diagnostics": diags}}),
    );
    assert_eq!(stale, Value::Null);
    lsp.settle();
    let diags = lsp.latest[&a].clone();
    let actions = lsp.code_actions(&a, &diags[0]["range"], &diags);
    assert_eq!(actions.len(), 2, "{actions:#?}");
    // Versioned edits for a client that applies them: it rejects them for any other version.
    let change = &actions[0]["edit"]["documentChanges"][0];
    assert_eq!(change["textDocument"], json!({"uri": a, "version": 2}));
    assert_eq!(change["edits"][0]["newText"], "x.append(0)");
    assert_eq!(
        actions[1]["edit"]["documentChanges"][0]["textDocument"]["version"],
        2
    );
    lsp.stop(true);
}

#[test]
fn lsp_keeps_open_documents_through_reloads_and_deletions_until_they_close() {
    let p = Project::new("lsp_unsaved_reload");
    let caller = "from helper import wait\n\n\nasync def handler():\n    wait()\n";
    p.write("caller.py", caller);
    let (helper, handler) = (uri_in(&p.dir, "helper.py"), uri(&p.dir.join("caller.py")));
    let mut lsp = ready_server(
        &p,
        json!({"workspace": {"didChangeWatchedFiles": {"dynamicRegistration": true}}}),
    );
    // A new module, never saved, that the caller already imports.
    lsp.open(&helper, "import time\n\n\ndef wait():\n    time.sleep(1)\n");
    lsp.open(&handler, caller);
    let eff101 = |lsp: &mut Lsp| {
        lsp.settle();
        codes(&lsp.latest[&handler]) == ["EFF101"]
    };
    assert!(eff101(&mut lsp));
    // A configuration change indexes the workspace again, with the open documents.
    p.write("pyproject.toml", "[project]\nname = \"t\"\n\n[tool.heki]\n");
    let config = uri(&p.dir.join("pyproject.toml"));
    lsp.notify(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": config, "type": 2}]}),
    );
    assert!(eff101(&mut lsp), "{:?}", lsp.latest[&handler]);
    // The caller's file is deleted on the disk while its document stays open.
    std::fs::remove_file(p.dir.join("caller.py")).unwrap();
    lsp.notify(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": handler, "type": 3}]}),
    );
    assert!(eff101(&mut lsp), "{:?}", lsp.latest[&handler]);
    // Closed, the never-saved helper leaves the project.
    lsp.notify(
        "textDocument/didClose",
        json!({"textDocument": {"uri": helper}}),
    );
    lsp.notify(
        "textDocument/didSave",
        json!({"textDocument": {"uri": handler}, "text": caller}),
    );
    lsp.settle();
    assert_eq!(lsp.latest[&handler], Vec::<Value>::new());
    let stats = lsp.request("heki/stats", Value::Null);
    assert_eq!(stats["indexed_files"], 1, "{stats}");
    lsp.stop(true);
}

#[test]
fn lsp_reports_invalid_rule_options_as_a_failed_load() {
    let p = Project::new("lsp_bad_targets");
    let text = "import time\n\n\nasync def h():\n    time.sleep(1)\n";
    p.write("a.py", text);
    let a = uri(&p.dir.join("a.py"));
    let mut lsp = ready_server(
        &p,
        json!({"workspace": {"didChangeWatchedFiles": {"dynamicRegistration": true}}}),
    );
    lsp.open(&a, text);
    assert_eq!(
        codes(&lsp.diagnostics(&a, |d| !d.is_empty())),
        vec!["EFF101"]
    );
    // A write owner whose target lacks its class: `heki check` exits with 2 before reporting.
    p.write(
        "heki.toml",
        "[[write-owners]]\nname = \"store\"\ntargets = [\"fields\"]\n",
    );
    assert_eq!(p.check(&[]).0, 2);
    lsp.notify(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&p.dir.join("heki.toml")), "type": 1}]}),
    );
    let log = lsp.log("invalid write-owners target `fields`");
    assert_eq!(log["params"]["type"], 2);
    let stats = lsp.settle();
    assert_eq!(stats["ready"], false, "{stats}");
    assert_eq!(lsp.latest[&a], Vec::<Value>::new());
    lsp.stop(true);
}

#[test]
fn lsp_reloads_when_a_save_finds_the_module_layout_switched() {
    // A client that watches no files: `src/__init__.py` appears without a notification, and the
    // next save reloads the workspace, as `heki check` would see the project.
    let p = Project::new("lsp_layout_on_save");
    let caller = "from pkg.util import helper\n\n\nasync def handler():\n    helper()\n";
    p.write("src/pkg/__init__.py", "")
        .write(
            "src/pkg/util.py",
            "import time\n\n\ndef helper():\n    time.sleep(1)\n",
        )
        .write("caller.py", caller);
    let a = uri(&p.dir.join("caller.py"));
    let mut lsp = ready_server(&p, json!({}));
    lsp.open(&a, caller);
    assert_eq!(
        codes(&lsp.diagnostics(&a, |d| !d.is_empty())),
        vec!["EFF101"]
    );
    // `src` becomes a package: its modules are `src.pkg.*`, and `pkg.util` names nothing.
    p.write("src/__init__.py", "");
    lsp.notify("textDocument/didSave", json!({"textDocument": {"uri": a}}));
    lsp.diagnostics(&a, <[Value]>::is_empty);
    assert_eq!(p.check(&["--output-format", "concise"]).1, "");
    lsp.stop(true);
}

/// A heuristic report: handing the list to `sink` blocks the proof of the rewrite.
const API_BLOCKED: &str = "def build(items, sink):\n    x = []\n    for i in items:\n        x = x + [i]\n        sink(x)\n    return x\n";

#[test]
fn the_server_runs_the_heuristic_rules_as_the_command_line_and_the_configuration_say() {
    let p = Project::new("lsp_heuristic");
    p.write("pkg/__init__.py", "")
        .write("pkg/api.py", API_BLOCKED);
    let api = uri(&p.dir.join("pkg/api.py"));
    let published = |args: &[&str]| {
        let mut lsp = Lsp::start(&p.dir, args);
        lsp.request(
            "initialize",
            json!({"processId": null, "rootUri": uri(&p.dir), "capabilities": {}}),
        );
        lsp.notify("initialized", json!({}));
        lsp.notify(
            "textDocument/didOpen",
            json!({"textDocument": {"uri": api, "languageId": "python", "version": 1, "text": API_BLOCKED}}),
        );
        lsp.settle();
        let codes = codes(&lsp.latest.get(&api).cloned().unwrap_or_default());
        lsp.stop(true);
        codes
    };
    // `Lsp::start` passes `--heuristic`; `--no-heuristic` after it wins: the exact rule then
    // withholds its heuristic report.
    assert_eq!(published(&["server"]), ["OPT101"]);
    assert_eq!(
        published(&["server", "--no-heuristic"]),
        Vec::<String>::new()
    );
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nheuristic = true\n",
    );
    assert_eq!(
        published(&["server", "--no-heuristic"]),
        Vec::<String>::new()
    );
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"OPT101\"]\n",
    );
    assert_eq!(published(&["server", "--no-heuristic"]), ["OPT101"]);
}
