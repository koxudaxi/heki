//! What counts as a read of a contract's target, end to end through the real binary:
//! `hasattr(obj, "NAME")` with a constant name reads `NAME` as `getattr` does, and with
//! `references = "reads"` a class-body or module-level binding of a target is a read of it (the
//! uses of a class attribute or a global are not all followed back to the target), while a
//! local, an import, and an annotation are not.

mod common;

use common::Project;
use serde_json::Value;

/// `(line, column, rule, message)` of every report of `heki check`, sorted.
fn found(p: &Project) -> Vec<(u64, u64, String, String)> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json", "pkg"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<_> = diags
        .iter()
        .map(|d| {
            (
                d["line"].as_u64().unwrap(),
                d["column"].as_u64().unwrap(),
                common::tiered_code(d).to_owned(),
                d["message"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

const BACKEND: &str = "class Backend:\n    SPECIAL = 1\n\n\nclass Model:\n    pass\n";

const PARSER: &str = r#"from pkg.model import Backend


def by_getattr(model: Backend):
    return getattr(model, "SPECIAL")


def by_hasattr(model: Backend):
    return hasattr(model, "SPECIAL")


def by_hasattr_unknown(model):
    return hasattr(model, "SPECIAL")


def by_other_name(model: Backend):
    return hasattr(model, "OTHER")


def by_computed_name(model: Backend, name: str):
    return hasattr(model, name)
"#;

#[test]
fn hasattr_with_a_constant_name_is_a_read() {
    let p = Project::new("binding_reads_hasattr");
    p.write(
        "heki.toml",
        r#"[lint]
select = ["MUT203", "MUT206"]

[[read-owners]]
name = "backend-attribute"
targets = ["pkg.model.Backend.SPECIAL"]
forbidden-readers = ["pkg.parser:*"]
unresolved = "report"
attribution = "lexical"
"#,
    )
    .write("pkg/__init__.py", "")
    .write("pkg/model.py", BACKEND)
    .write("pkg/parser.py", PARSER);
    let reads = |line, column, rule, reader: &str, how: &str, on: &str| {
        (
            line,
            column,
            rule,
            format!(
                "`{reader}` reads `{on}` ({how}) inside the forbidden readers of `backend-attribute`"
            ),
        )
    };
    let typed = |line, reader: &str, how: &str| {
        let (line, column, rule, message) =
            reads(line, 12, "MUT203", reader, how, "pkg.model.Backend.SPECIAL");
        (line, column, rule.to_owned(), message)
    };
    let found = found(&p);
    assert_eq!(found.len(), 3, "{found:#?}");
    assert_eq!(found[0], typed(5, "by_getattr", "with `getattr`"));
    assert_eq!(found[1], typed(9, "by_hasattr", "with `hasattr`"));
    assert_eq!(
        found[2],
        (
            13,
            12,
            "MUT206".to_owned(),
            "`by_hasattr_unknown` reads `SPECIAL` (with `hasattr`) on an object of unknown class inside the forbidden readers of `backend-attribute`: heki cannot tell whether it reads `pkg.model.Backend.SPECIAL`".to_owned()
        )
    );
}

const CONFIG: &str = r#"from pkg import backend
from pkg.backend import Model
from pkg.backend import Model as Imported

Alias = Model


class Config:
    model_type: type = backend.Model
    imported = Model
    same_name = Model
    held = [backend.Model]
    named = backend.Model.__name__
    nothing = 1
    from pkg.backend import Model as InBody

    def method(self) -> type:
        local = backend.Model
        return local


MODULE_LEVEL = backend.Model


def default():
    local = backend.Model
    return local


def annotated(kind: backend.Model) -> backend.Model:
    value: backend.Model = kind
    return value


def called():
    return backend.make()
"#;

const BACKEND_MODULE: &str = "class Model:\n    pass\n\n\ndef make():\n    return Model()\n";

fn config_project(name: &str, references: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "heki.toml",
        &format!(
            r#"[lint]
select = ["MUT204"]

[[call-owners]]
name = "config-backend-reference"
targets = ["pkg.backend.Model"]
forbidden-callers = ["pkg.config:*", "pkg.config:<module>"]
{references}attribution = "lexical"
"#
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/backend.py", BACKEND_MODULE)
    .write("pkg/config.py", CONFIG);
    p
}

fn lines(p: &Project) -> Vec<(u64, String)> {
    found(p)
        .into_iter()
        .map(|(line, _, _, message)| (line, message))
        .collect()
}

#[test]
fn a_class_body_or_module_level_binding_of_a_target_is_a_read() {
    let reads = |line: u64, function: &str| {
        (
            line,
            format!(
                "`{function}` reads `pkg.backend.Model` inside the forbidden callers of `config-backend-reference`"
            ),
        )
    };
    let hands = |line: u64, function: &str| {
        (
            line,
            format!(
                "`{function}` hands on `pkg.backend.Model` inside the forbidden callers of `config-backend-reference`"
            ),
        )
    };
    let p = config_project("binding_reads_on", "references = \"reads\"\n");
    assert_eq!(
        lines(&p),
        [
            // The class-body bindings, the module-level one, and an alias of an imported name.
            reads(5, "<module>"),
            reads(9, "<module>"),
            reads(10, "<module>"),
            reads(11, "<module>"),
            reads(12, "<module>"),
            reads(13, "<module>"),
            // A local keeps its rule: its own use counts.
            hands(18, "Config.method"),
            reads(22, "<module>"),
            hands(26, "default"),
        ],
        "{:#?}",
        found(&p)
    );
    // Without the key only calls count, and with `calls` the bindings are no use handing it on.
    let calls = config_project("binding_reads_off", "");
    assert_eq!(lines(&calls), [], "{:#?}", found(&calls));
}

const DISCARDED: &str = r#"from pkg import backend

backend.Model


class Config:
    backend.Model
    model_type: type = backend.Model
    [*backend.Model]
    held = [backend.Model]


DEFAULT = backend.Model
"#;

#[test]
fn a_discarded_expression_is_no_binding() {
    let p = config_project("binding_reads_discarded", "references = \"reads\"\n");
    p.write("pkg/config.py", DISCARDED);
    let read = |line: u64| {
        (
            line,
            "`<module>` reads `pkg.backend.Model` inside the forbidden callers of `config-backend-reference`"
                .to_owned(),
        )
    };
    assert_eq!(lines(&p), [read(8), read(10), read(13)], "{:#?}", found(&p));
}
