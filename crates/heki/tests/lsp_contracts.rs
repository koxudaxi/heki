//! Contract rules in the incremental LSP server: editing and saving a callee in another file
//! updates a scope function's `EFF201` diagnostic, editing a private helper's read updates the
//! `MUT203` diagnostic at its caller (Early Cutoff does not hide either), an edit that only
//! moves code changes neither, and a caller that stops calling a private helper hands the
//! report back to the helper's own file.

mod common;

use common::Project;
use common::lsp::{DEBOUNCE_WAIT, Lsp, codes, uri};
use serde_json::{Value, json};

const CONFIG: &str = r#"[[read-owners]]
name = "kids"
targets = ["Reference.children"]

[[effect-boundaries]]
name = "pure"
scopes = ["*.render"]
forbid = ["blocking"]
"#;

const MODEL: &str = "class Reference:\n    def __init__(self):\n        self.children = []\n        self.name = \"\"\n";
const HELPERS: &str =
    "from pkg.model import Reference\n\n\ndef _kids(r: Reference):\n    return r.name\n";
const HELPERS_READING: &str =
    "from pkg.model import Reference\n\n\ndef _kids(r: Reference):\n    return r.children\n";
const APP: &str = "from pkg.helpers import _kids\nfrom pkg.model import Reference\n\n\ndef build(r: Reference):\n    return _kids(r)\n";
const APP_WITHOUT_CALL: &str = "from pkg.helpers import _kids\nfrom pkg.model import Reference\n\n\ndef build(r: Reference):\n    return r.name\n";
const FETCH: &str = "def get_json(url):\n    return url\n";
const FETCH_BLOCKING: &str =
    "import requests\n\n\ndef get_json(url):\n    return requests.get(url)\n";
const VIEW: &str = "from pkg.fetch import get_json\n\n\nclass Model:\n    def render(self):\n        return get_json(\"x\")\n";

/// Edit, write, and save `rel` (the way an editor saves a changed document).
fn save(lsp: &mut Lsp, p: &Project, rel: &str, version: i32, text: &str) {
    let u = uri(&p.dir.join(rel));
    lsp.change(&u, version, text);
    p.write(rel, text);
    lsp.notify(
        "textDocument/didSave",
        json!({"textDocument": {"uri": u}, "text": text}),
    );
}

/// After `save` of `rel`: its own diagnostics, then the engine counters of the save.
fn saved(lsp: &mut Lsp, p: &Project, rel: &str) -> Value {
    lsp.diagnostics(&uri(&p.dir.join(rel)), |_| true);
    std::thread::sleep(DEBOUNCE_WAIT);
    lsp.request("heki/stats", Value::Null)
}

fn published_for(lsp: &Lsp, u: &str) -> bool {
    lsp.backlog
        .iter()
        .any(|m| m["method"] == "textDocument/publishDiagnostics" && m["params"]["uri"] == u)
}

#[test]
fn lsp_contract_diagnostics_follow_callee_and_helper_edits() {
    let p = Project::new("lsp_contracts");
    p.write("heki.toml", CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", MODEL)
        .write("pkg/helpers.py", HELPERS)
        .write("pkg/app.py", APP)
        .write("pkg/fetch.py", FETCH)
        .write("pkg/view.py", VIEW);
    let (app, view) = (
        uri(&p.dir.join("pkg/app.py")),
        uri(&p.dir.join("pkg/view.py")),
    );
    let mut lsp = Lsp::start(&p.dir, &["server", "--stdio"]);
    lsp.request(
        "initialize",
        json!({"processId": null, "rootUri": uri(&p.dir), "capabilities": {}}),
    );
    lsp.notify("initialized", json!({}));
    lsp.ready();
    lsp.open(&view, VIEW);
    lsp.diagnostics(&view, <[Value]>::is_empty);
    lsp.open(&app, APP);
    lsp.diagnostics(&app, <[Value]>::is_empty);

    // EFF201: the callee in another file starts blocking; saving it re-checks the scope
    // function's file.
    lsp.open(&uri(&p.dir.join("pkg/fetch.py")), FETCH);
    save(&mut lsp, &p, "pkg/fetch.py", 2, FETCH_BLOCKING);
    let diags = lsp.diagnostics(&view, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["EFF201"]);
    assert_eq!(
        diags[0]["range"]["start"],
        json!({"line": 5, "character": 15})
    );
    let related = diags[0]["relatedInformation"].as_array().unwrap();
    assert_eq!(related[0]["location"]["uri"], view);
    assert_eq!(
        related[1]["location"]["uri"],
        uri(&p.dir.join("pkg/fetch.py"))
    );
    assert_eq!(
        related[1]["location"]["range"]["start"],
        json!({"line": 4, "character": 11})
    );

    // MUT203: the private helper starts reading `children`; the read is reported at its
    // caller in another file.
    lsp.open(&uri(&p.dir.join("pkg/helpers.py")), HELPERS);
    save(&mut lsp, &p, "pkg/helpers.py", 2, HELPERS_READING);
    let diags = lsp.diagnostics(&app, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["MUT203"]);
    assert_eq!(
        diags[0]["range"]["start"],
        json!({"line": 5, "character": 11})
    );
    assert!(
        diags[0]["message"]
            .as_str()
            .unwrap()
            .contains("via `pkg.helpers._kids`"),
        "{diags:#?}"
    );

    // Edits that only move code: Early Cutoff, no other file re-checked or re-published.
    let before = lsp.request("heki/stats", Value::Null);
    lsp.backlog.clear();
    save(
        &mut lsp,
        &p,
        "pkg/fetch.py",
        3,
        &format!("# moved\n\n{FETCH_BLOCKING}"),
    );
    let stats = saved(&mut lsp, &p, "pkg/fetch.py");
    save(
        &mut lsp,
        &p,
        "pkg/helpers.py",
        3,
        &format!("# moved\n\n{HELPERS_READING}"),
    );
    let stats_helpers = saved(&mut lsp, &p, "pkg/helpers.py");
    for s in [&stats, &stats_helpers] {
        assert_eq!(s["last_summary_changed"], false, "{s}");
        assert_eq!(
            s["dependent_reanalyses"], before["dependent_reanalyses"],
            "{s}"
        );
    }
    assert!(!published_for(&lsp, &view), "{:#?}", lsp.backlog);
    assert!(!published_for(&lsp, &app), "{:#?}", lsp.backlog);

    // The caller stops calling the helper: the helper has no project caller left, so it
    // reports its read itself (its file is re-checked); calling it again moves the report back.
    let helpers = uri(&p.dir.join("pkg/helpers.py"));
    save(&mut lsp, &p, "pkg/app.py", 2, APP_WITHOUT_CALL);
    let diags = lsp.diagnostics(&helpers, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["MUT203"]);
    assert_eq!(
        diags[0]["range"]["start"],
        json!({"line": 6, "character": 11})
    );
    lsp.diagnostics(&app, <[Value]>::is_empty);
    save(&mut lsp, &p, "pkg/app.py", 3, APP);
    lsp.diagnostics(&helpers, <[Value]>::is_empty);
    lsp.diagnostics(&app, |d| codes(d) == ["MUT203"]);

    // The helper stops reading `children`: its caller's diagnostic goes away.
    save(&mut lsp, &p, "pkg/helpers.py", 4, HELPERS);
    lsp.diagnostics(&app, <[Value]>::is_empty);
    lsp.stop(true);
}
