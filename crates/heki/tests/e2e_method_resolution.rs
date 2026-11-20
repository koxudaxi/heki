//! Which functions a call runs, end to end through the real binary: methods found along
//! Python's MRO (C3 linearization).
//!
//! Every fixture marks the lines that must be reported with the rule code in a comment
//! (`# EFF101`); every other line must not be.

mod common;

use common::Project;
use serde_json::Value;

const CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
extend-select = ["MUT108"]

[[tool.heki.write-owners]]
name = "model"
targets = ["Model.items"]
"#;

/// `(path, line, rule)` of every diagnostic of `rules`.
fn reported(p: &Project, rules: &[&str]) -> Vec<(String, u64, String)> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<(String, u64, String)> = diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
            )
        })
        .filter(|(_, _, r)| rules.contains(&r.as_str()))
        .collect();
    out.sort();
    out
}

/// The `(path, line, rule)` marks of `files`: a line ending in `# <RULE>` for each of `rules`.
fn marked(files: &[(&str, &str)], rules: &[&str]) -> Vec<(String, u64, String)> {
    let mut out = Vec::new();
    for (path, text) in files {
        for (i, line) in text.lines().enumerate() {
            for rule in rules {
                if line.trim_end().ends_with(&format!("# {rule}")) {
                    out.push(((*path).to_owned(), i as u64 + 1, (*rule).to_owned()));
                }
            }
        }
    }
    out.sort();
    out
}

fn check_marks(name: &str, files: &[(&str, &str)], rules: &[&str]) {
    let p = Project::new(name);
    p.write("pyproject.toml", CONFIG);
    p.write("pkg/__init__.py", "");
    for (path, text) in files {
        p.write(path, text);
    }
    assert_eq!(reported(&p, rules), marked(files, rules));
}

/// `Leaf(Left, Right)` finds `Right.read` before `Root.read`, which `Left` inherits.
const DIAMOND: &str = r#"import time


class Root:
    def read(self):
        return 1


class Left(Root):
    pass


class Right(Root):
    def read(self):
        time.sleep(1)


class Leaf(Left, Right):
    pass


class Plain(Left):
    pass


async def leaf(x: Leaf):
    x.read()  # EFF101


async def plain(x: Plain):
    x.read()
"#;

#[test]
fn methods_are_found_along_the_c3_linearization() {
    check_marks("mro_c3", &[("pkg/diamond.py", DIAMOND)], &["EFF101"]);
}
