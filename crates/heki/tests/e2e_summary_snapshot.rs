//! `heki snapshot --write` and `MUT103` on modules with stubs (end to end, through the real
//! binary).

mod common;

use common::Project;
use serde_json::Value;

fn mut103(p: &Project) -> Vec<(String, String)> {
    let (code, stdout, stderr) = p.check(&["--select", "MUT103", "--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["target"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn snapshot(p: &Project) -> Value {
    serde_json::from_str(&p.read(".heki-summary-snapshot.json")).unwrap()
}

#[test]
fn a_stub_naming_positional_only_parameters_differently_keeps_the_snapshot_valid() {
    let p = Project::new("snapshot_stub_names");
    p.write("mod.py", "def fill(items, /):\n    items.append(1)\n")
        .write("mod.pyi", "def fill(buffer: list[int], /) -> None: ...\n");
    assert_eq!(p.heki(&["snapshot", "--write"]).0, 0);
    // The implementation names the parameter.
    assert_eq!(
        snapshot(&p)["functions"]["mod:fill"],
        serde_json::json!({"mutates_params": ["items"]})
    );
    assert_eq!(mut103(&p), vec![]);
    // A new effect is still a regression, reported once, in the implementation.
    p.write(
        "mod.py",
        "import time\n\n\ndef fill(items, /):\n    items.append(1)\n    time.sleep(1)\n",
    );
    assert_eq!(
        mut103(&p),
        vec![("mod.py".to_owned(), "transitive_exec_effects:sleep".to_owned())]
    );
}
