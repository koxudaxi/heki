//! Python semantics the IR models, end to end through the real binary: each shape is checked by
//! the rule that reads the fact (a report where CPython runs the effect, none where it does not)
//! and, where the rules cannot show it, by the IR itself (`heki dump-ir`). The Soundness lane's
//! counterexamples of the same shapes are in `e2e_soundness.rs`
//! (`tests/difftest/ir_counterexamples.py`).

mod common;

use common::Project;
use serde_json::Value;

/// `heki check --output-format json <args>` of the project.
fn diagnostics(p: &Project, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["--output-format", "json"];
    all.extend_from_slice(args);
    let (code, stdout, stderr) = p.check(&all);
    assert!(code == 0 || code == 1, "exit {code}: {stderr}");
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"))
}

/// The `(qualname, rule)` pairs `rules` report on the single module `source`.
fn reports(name: &str, source: &str, rules: &str) -> Vec<(String, String)> {
    let p = Project::new(name);
    p.write("case.py", source);
    diagnostics(&p, &["--select", rules])
        .iter()
        .map(|d| {
            (
                d["qualname"].as_str().unwrap().to_owned(),
                d["rule_code"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(q, c)| ((*q).to_owned(), (*c).to_owned()))
        .collect()
}

/// `heki dump-ir` of `source` for Python `version`.
fn dump_ir(name: &str, source: &str, version: &str) -> Value {
    let p = Project::new(name);
    p.write("m.py", source);
    let (code, stdout, stderr) = p.heki(&["dump-ir", "m.py", "--target-version", version]);
    assert_eq!(code, 0, "{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

fn function<'a>(ir: &'a Value, qualname: &str) -> &'a Value {
    ir["functions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["qualname"] == qualname)
        .unwrap_or_else(|| panic!("no function `{qualname}`"))
}

// ======================================================================= replacement fields

#[test]
fn a_yield_in_a_replacement_field_makes_a_generator() {
    let src = "\
import time


def rows():
    f\"{(yield 1)}\"
    time.sleep(1)


async def creates():
    rows()


async def iterates():
    for _ in rows():
        pass
";
    let ir = dump_ir("ir_fstring_yield", src, "py315");
    assert_eq!(function(&ir, "rows")["kind"], "generator");
    // Calling a generator function only creates the generator; iterating it runs the body.
    assert_eq!(
        reports("ir_fstring_yield", src, "EFF101"),
        pairs(&[("iterates", "EFF101")])
    );
}

// ======================================================================== scopes and frames

#[test]
fn a_yield_in_a_definition_header_makes_the_enclosing_function_a_generator() {
    let src = "\
import time


def rows():
    def unused(x=(yield)):
        pass
    time.sleep(1)


async def creates():
    rows()
";
    let ir = dump_ir("ir_header_yield", src, "py315");
    assert_eq!(function(&ir, "rows")["kind"], "generator");
    assert_eq!(function(&ir, "rows.<locals>.unused")["kind"], "sync");
    assert!(reports("ir_header_yield", src, "EFF101").is_empty());
}
