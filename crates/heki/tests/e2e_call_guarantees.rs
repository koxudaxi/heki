//! What a call guarantees, end to end through the real binary: a fact that must hold on every
//! path (an announcement of `MUT109`, a consumption of `MUT106`) holds at a call only when
//! every implementation the call may run guarantees it (an abstract method never runs).
//!
//! Every fixture marks the lines that must be reported with the rule code in a comment
//! (`# MUT109`); every other line must not be.

mod common;

use common::Project;
use serde_json::Value;

const CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
extend-select = ["MUT106"]

[[tool.heki.invalidation]]
name = "index"
targets = ["Model.x", "Model.items"]
invalidators = ["Index.refresh"]
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

const MODEL: &str = r#"class Model:
    def __init__(self):
        self.x = 0
        self.items = []


class Index:
    def refresh(self):
        return None
"#;

/// A hook whose base does nothing announces nothing; an abstract base never runs.
const HOOKS: &str = r#"import abc

from pkg.model import Index, Model


class Hook:
    def done(self):
        return None


class NotifyingHook(Hook):
    def __init__(self, idx: Index):
        self.idx = idx

    def done(self):
        self.idx.refresh()


class Notifier(abc.ABC):
    @abc.abstractmethod
    def done(self):
        ...


class IndexNotifier(Notifier):
    def __init__(self, idx: Index):
        self.idx = idx

    def done(self):
        self.idx.refresh()


def through_any_hook(m: Model, hook: Hook):
    m.x = 1  # MUT109
    hook.done()


def through_a_notifying_hook(m: Model, hook: NotifyingHook):
    m.x = 1
    hook.done()


def through_an_abstract_notifier(m: Model, notifier: Notifier):
    m.x = 1
    notifier.done()
"#;

#[test]
fn an_announcement_through_a_call_needs_every_implementation_to_announce() {
    check_marks(
        "announcing_implementations",
        &[("pkg/model.py", MODEL), ("pkg/hooks.py", HOOKS)],
        &["MUT109"],
    );
}

/// Forwarding an iterator to a method consumes it only when every override consumes it.
const TAKERS: &str = r#"import abc


class Base:
    def take(self, it):
        pass


class Drain(Base):
    def take(self, it):
        list(it)


class Taker(abc.ABC):
    @abc.abstractmethod
    def take(self, it):
        ...


class Draining(Taker):
    def take(self, it):
        list(it)


def forward(b: Base, it):
    b.take(it)


def forward_abstract(t: Taker, it):
    t.take(it)


def through_base(lines):
    it = iter(lines)
    forward(Base(), it)
    return list(it)


def through_abstract(lines, t: Taker):
    it = iter(lines)
    forward_abstract(t, it)
    return list(it)  # MUT106


def directly_abstract(lines, t: Taker):
    it = iter(lines)
    t.take(it)
    return list(it)  # MUT106
"#;

#[test]
fn a_consumption_through_a_call_needs_every_implementation_to_consume() {
    check_marks(
        "consuming_implementations",
        &[("pkg/takers.py", TAKERS)],
        &["MUT106"],
    );
}
