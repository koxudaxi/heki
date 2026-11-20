//! `MUT204` in the incremental LSP server: a call of a target added in another file publishes the
//! diagnostic, at the caller of the private helper that makes the call. The construction of a
//! dataclass has no effects, so no summary changes (Early Cutoff holds) and the call facts alone
//! make the server re-check the caller's file; removing the call clears it again.

mod common;

use common::Project;
use common::lsp::{Lsp, codes, uri};
use serde_json::{Value, json};

const CONFIG: &str = r#"[[call-owners]]
name = "facts"
targets = ["FinalFacts"]
allowed-callers = ["pkg.store.*"]
"#;

const MODEL: &str = "from dataclasses import dataclass\n\n\n@dataclass\nclass FinalFacts:\n    names: tuple[str, ...]\n";
const HELPERS: &str =
    "from pkg.model import FinalFacts\n\n\ndef _seal() -> None:\n    return None\n";
const HELPERS_CONSTRUCTING: &str =
    "from pkg.model import FinalFacts\n\n\ndef _seal() -> None:\n    FinalFacts(names=())\n";
const APP: &str = "from pkg.helpers import _seal\n\n\ndef build() -> None:\n    _seal()\n";
const APP_DIRECT: &str = "from pkg.helpers import _seal\nfrom pkg.model import FinalFacts\n\n\ndef build() -> None:\n    _seal()\n\n\ndef direct() -> FinalFacts:\n    return FinalFacts(names=())\n";

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

#[test]
fn lsp_call_owner_diagnostics_follow_calls_added_in_other_files() {
    let p = Project::new("lsp_call_owners");
    p.write("heki.toml", CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", MODEL)
        .write("pkg/helpers.py", HELPERS)
        .write("pkg/app.py", APP);
    let (app, helpers) = (
        uri(&p.dir.join("pkg/app.py")),
        uri(&p.dir.join("pkg/helpers.py")),
    );
    let mut lsp = Lsp::start(&p.dir, &["server", "--stdio"]);
    lsp.request(
        "initialize",
        json!({"processId": null, "rootUri": uri(&p.dir), "capabilities": {}}),
    );
    lsp.notify("initialized", json!({}));
    lsp.ready();
    lsp.open(&app, APP);
    lsp.diagnostics(&app, <[Value]>::is_empty);

    // The private helper in another file starts constructing `FinalFacts`: the report goes to its
    // caller `build`, whose file is re-checked although no summary changed.
    lsp.open(&helpers, HELPERS);
    let before = lsp.settle();
    save(&mut lsp, &p, "pkg/helpers.py", 2, HELPERS_CONSTRUCTING);
    let diags = lsp.diagnostics(&app, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["MUT204"]);
    assert_eq!(
        diags[0]["range"]["start"],
        json!({"line": 4, "character": 4})
    );
    assert!(
        diags[0]["message"]
            .as_str()
            .unwrap()
            .contains("`build` constructs `FinalFacts` via `pkg.helpers._seal`"),
        "{diags:#?}"
    );
    let stats = lsp.settle();
    assert_eq!(stats["last_summary_changed"], false, "{stats}");
    assert!(
        stats["dependent_reanalyses"].as_u64() > before["dependent_reanalyses"].as_u64(),
        "{before} {stats}"
    );
    lsp.diagnostics(&helpers, <[Value]>::is_empty);

    // A construction added in the open file itself is reported there.
    save(&mut lsp, &p, "pkg/app.py", 2, APP_DIRECT);
    let diags = lsp.diagnostics(&app, |d| d.len() == 2);
    assert_eq!(codes(&diags), vec!["MUT204", "MUT204"]);

    // The helper stops constructing it: only the direct construction is left.
    save(&mut lsp, &p, "pkg/helpers.py", 3, HELPERS);
    let diags = lsp.diagnostics(&app, |d| d.len() == 1);
    assert_eq!(
        diags[0]["range"]["start"],
        json!({"line": 9, "character": 11})
    );
    lsp.stop(true);
}
