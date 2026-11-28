//! Targets naming the items of the instances of a project class deriving from a builtin `dict`,
//! `list`, or `set` (`Class[*]`, `Class["key"]`), end to end through the real binary: item
//! stores and deletions, the mutating methods the class inherits, changes in place of an item,
//! a helper handed the instance, an attribute holding one, `write-owners` and effect boundaries,
//! and what stays unreported (a value of unknown class, a class replacing the item store).

mod common;

use common::Project;
use serde_json::Value;

fn diagnostics(p: &Project) -> Vec<Value> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

/// `(path, line, code, qualname, target)` of every diagnostic.
fn found(diags: &[Value]) -> Vec<(String, u64, String, String, String)> {
    diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
                d["qualname"].as_str().unwrap().to_owned(),
                d["target"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn d(
    path: &str,
    line: u64,
    code: &str,
    qualname: &str,
    target: &str,
) -> (String, u64, String, String, String) {
    (
        path.into(),
        line,
        code.into(),
        qualname.into(),
        target.into(),
    )
}

fn message_at<'d>(diags: &'d [Value], path: &str, line: u64) -> &'d str {
    diags
        .iter()
        .find(|x| x["path"] == path && x["line"] == line)
        .unwrap_or_else(|| panic!("no diagnostic at {path}:{line}: {diags:#?}"))["message"]
        .as_str()
        .unwrap()
}

const CONTAINERS: &str = r#"from collections import defaultdict


class Imports(defaultdict[str, set[str]]):
    def add_from(self, module: str, name: str) -> None:
        self[module].add(name)


class Names(list[str]):
    pass


class Checked(dict[str, int]):
    def __setitem__(self, key: str, value: int) -> None:
        super().__setitem__(key, value)


class Plain:
    pass
"#;

const MODEL: &str = r#"from pkg.containers import Imports


class Model:
    def __init__(self) -> None:
        self.imports: Imports = Imports(set)
"#;

const RUN: &str = r#"from pkg.containers import Checked, Imports, Names
from pkg.model import Model


def run(imports: Imports) -> None:
    imports["typing"].add("Any")
    imports.setdefault("os", set()).add("path")
    del imports["sys"]


def store(imports: Imports, key: str) -> None:
    imports[key] = set()


def clear(imports: Imports) -> None:
    imports.clear()


def append(names: Names) -> None:
    names.append("x")


def through_attribute(model: Model) -> None:
    model.imports["typing"].add("Any")


def fill(target) -> None:
    target["k"] = {"v"}


def hand(imports: Imports) -> None:
    fill(imports)


def checked(c: Checked) -> None:
    c["k"] = 1


def untyped(d) -> None:
    d["typing"].add("Any")


def read(imports: Imports) -> int:
    return len(imports["typing"])


def computed(imports: Imports) -> None:
    imports[str(len(imports))] = set()
"#;

fn items_project(name: &str, contracts: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\", \"MUT203\", \"EFF201\"]\n\n\
             {contracts}"
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/containers.py", CONTAINERS)
    .write("pkg/model.py", MODEL)
    .write("pkg/run.py", RUN);
    p
}

/// `Class[*]` names every item of the instances of the class: item stores and deletions, the
/// mutating methods the class inherits, changes in place of an item (also of the entry a
/// `setdefault` returns), a helper handed the instance (reported at the call), and an attribute
/// holding an instance. The class's own methods are writers like any other. A value of unknown
/// class, a read, and a class whose own `__setitem__` replaces the store write nothing here.
#[test]
fn an_items_target_names_every_change_of_an_item() {
    let contract = "[[tool.heki.write-owners]]\nname = \"imports\"\n\
         targets = [\"pkg.containers.Imports[*]\", \"pkg.containers.Names[*]\", \
         \"pkg.containers.Checked[*]\"]\nallowed-writers = [\"pkg.containers.*\"]\n";
    let p = items_project("items_all", contract);
    let diags = diagnostics(&p);
    let r = "pkg/run.py";
    let imports = "pkg.containers.Imports[*]";
    assert_eq!(
        found(&diags),
        vec![
            d(r, 6, "MUT201", "run", imports),
            d(r, 7, "MUT201", "run", imports),
            d(r, 8, "MUT201", "run", imports),
            d(r, 12, "MUT201", "store", imports),
            d(r, 16, "MUT201", "clear", imports),
            d(r, 20, "MUT201", "append", "pkg.containers.Names[*]"),
            d(r, 24, "MUT201", "through_attribute", imports),
            d(r, 32, "MUT201", "hand", imports),
            d(r, 48, "MUT201", "computed", imports),
        ]
    );
    assert_eq!(
        message_at(&diags, r, 6),
        "`run` writes `pkg.containers.Imports[*]` (in place inside an item) outside the write owners of `imports`"
    );
}

/// `Class["key"]` names the entry of that constant key, as `Class.attr["key"]` does: not a
/// write under another or a computed key, nor of every item at once. A class's own methods
/// are writers like any other, also the `__setitem__` replacing the item store.
#[test]
fn an_entry_target_names_one_item_and_own_methods_are_writers() {
    let contract = "[[tool.heki.write-owners]]\nname = \"typing\"\n\
         targets = ['pkg.containers.Imp*[\"typing\"]']\nallowed-writers = [\"pkg.model.*\"]\n\n\
         [[tool.heki.write-owners]]\nname = \"checked\"\n\
         targets = [\"pkg.containers.Checked[*]\"]\nallowed-writers = [\"pkg.run.*\"]\n";
    let p = items_project("items_entry", contract);
    let diags = diagnostics(&p);
    let (c, r) = ("pkg/containers.py", "pkg/run.py");
    let typing = "pkg.containers.Imp*[\"typing\"]";
    assert_eq!(
        found(&diags),
        vec![
            d(
                c,
                15,
                "MUT201",
                "Checked.__setitem__",
                "pkg.containers.Checked[*]"
            ),
            d(r, 6, "MUT201", "run", typing),
            d(r, 24, "MUT201", "through_attribute", typing),
        ]
    );
    // A glob target's report names the entry it matched.
    assert!(
        message_at(&diags, r, 6).starts_with("`run` writes `pkg.containers.Imports[\"typing\"]`"),
        "{diags:#?}"
    );
}

/// An effect boundary's `write-targets`, `write-universe`, and `allow-writes` take the same
/// entries: an attribute pattern (`Model.*`) never names the items, an items entry of the
/// universe reports them under its text, and a write under a key heki cannot tell names the
/// items of the class as the container.
#[test]
fn effect_boundaries_take_items_entries() {
    let contracts = "[[tool.heki.effect-boundaries]]\nname = \"imports-fixed\"\n\
         scopes = [\"pkg.run.run\", \"pkg.run.hand\"]\nforbid = [\"writes-targets\"]\n\
         write-targets = [\"Imports[*]\"]\n\n\
         [[tool.heki.effect-boundaries]]\nname = \"model-only\"\n\
         scopes = [\"pkg.run.through_attribute\"]\n\
         write-universe = [\"pkg.model.Model.*\"]\nallow-writes = [\"pkg.model.Model.imports\"]\n\n\
         [[tool.heki.effect-boundaries]]\nname = \"with-items\"\n\
         scopes = [\"pkg.run.through_attribute\", \"pkg.run.clear\"]\n\
         write-universe = [\"pkg.model.Model.*\", \"pkg.containers.Imports[*]\"]\n\
         allow-writes = [\"pkg.model.Model.imports\"]\n\n\
         [[tool.heki.effect-boundaries]]\nname = \"typing-fixed\"\n\
         scopes = [\"pkg.run.computed\"]\nforbid = [\"writes-targets\"]\n\
         write-targets = ['Imports[\"typing\"]']\n";
    let p = items_project("items_boundaries", contracts);
    let diags = diagnostics(&p);
    let (r, items) = ("pkg/run.py", "pkg.containers.Imports[*]");
    assert_eq!(
        found(&diags),
        vec![
            d(r, 7, "EFF201", "run", "imports-fixed:Imports[*]"),
            d(r, 16, "EFF201", "clear", &format!("with-items:{items}")),
            d(
                r,
                24,
                "EFF201",
                "through_attribute",
                &format!("with-items:{items}")
            ),
            d(r, 32, "EFF201", "hand", "imports-fixed:Imports[*]"),
            d(r, 48, "EFF201", "computed", "typing-fixed:Imports[?]"),
        ]
    );
    assert_eq!(
        message_at(&diags, r, 24),
        "`through_attribute` writes `pkg.containers.Imports[*]` (in place inside an item), which effect boundary `with-items` does not allow"
    );
    assert_eq!(
        message_at(&diags, r, 48),
        "`computed` writes an entry of `Imports` under a key heki cannot tell (in place), where effect boundary `typing-fixed` names some of its entries"
    );
}

/// An items entry names something when a class of its family holds items; a read target
/// naming items is a configuration error, as an entry target is.
#[test]
fn items_entries_are_judged_and_write_targets_only() {
    let contract = "[[tool.heki.write-owners]]\nname = \"imports\"\n\
         targets = [\"pkg.containers.Imports[*]\", \"pkg.model.Model[*]\", \
         \"pkg.containers.Nope[*]\"]\nallowed-writers = [\"pkg.containers.*\"]\n";
    let p = items_project("items_unmatched", contract);
    let (code, stdout, stderr) = p.check(&[]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let warned = |entry: &str, names_no: &str| {
        format!(
            "warning: `targets` entry `{entry}` of write-owners `imports` (from pyproject.toml:9; \
             contract at pyproject.toml:7) names no {names_no} in the analyzed files\n"
        )
    };
    assert!(
        stderr.contains(&warned(
            "pkg.model.Model[*]",
            "items of `pkg.model.Model`, a class deriving from no builtin `dict`, `list`, or `set`,"
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(&warned("pkg.containers.Nope[*]", "class")),
        "{stderr}"
    );
    assert!(!stderr.contains("Imports[*]`"), "{stderr}");
    let p = items_project(
        "items_no_class",
        "[[tool.heki.write-owners]]\nname = \"imports\"\ntargets = [\"[*]\"]\n",
    );
    let (code, stdout, stderr) = p.check(&[]);
    assert_eq!(code, 2, "{stdout}{stderr}");
    assert!(
        stderr.contains("invalid write-owners target `[*]`"),
        "{stderr}"
    );
    let p = items_project(
        "items_read",
        "[[tool.heki.read-owners]]\nname = \"r\"\ntargets = [\"pkg.containers.Imports[*]\"]\n",
    );
    let (code, stdout, stderr) = p.check(&[]);
    assert_eq!(code, 2, "{stdout}{stderr}");
    assert!(
        stderr.contains(
            "invalid read-owners target `pkg.containers.Imports[*]`: a container entry (`Class.attr[\"key\"]`) or the items of a class (`Class[*]`) is a write target only"
        ),
        "{stderr}"
    );
}
