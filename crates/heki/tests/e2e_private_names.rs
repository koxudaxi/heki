//! Python's private name mangling end to end through the real binary. In class `C`, a private
//! name (`self.__run`, `def __run`, `__limit = 3`) is the member `_C__run`: a receiver typed
//! `C`, a subclass, or `type[C]` reaching `_C__run` reads or calls the member `__run` of `C`,
//! and a contract target may name it either way (`C.__run` or `C._C__run`). A subclass's own
//! private name is another member, which overrides nothing of its base.

mod common;

use common::Project;
use serde_json::Value;

/// `(path, line, column, rule, qualname)` of every diagnostic of `heki check`.
fn found(p: &Project) -> Vec<(String, u64, u64, String, String)> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json", "src"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<_> = diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["column"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
                d["qualname"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

fn d(
    path: &str,
    line: u64,
    column: u64,
    rule: &str,
    qualname: &str,
) -> (String, u64, u64, String, String) {
    (path.into(), line, column, rule.into(), qualname.into())
}

const BASE: &str = r#"class Parser:
    def __runner(self) -> int:
        return 1

    @staticmethod
    def __postprocess(value: int) -> int:
        return value

    def run(self) -> int:
        return self.__runner()


class JsonParser(Parser):
    def peek(self) -> int:
        return self._Parser__runner()
"#;

const OUTSIDE: &str = r#"from pkg.base import Parser


def typed_call(parser: Parser) -> int:
    return parser._Parser__runner()


def class_call() -> int:
    return Parser._Parser__postprocess(2)


def typed_read(parser: Parser) -> object:
    return parser._Parser__runner


def untyped_call(parser) -> int:
    return parser._Parser__runner()
"#;

/// The call and read contracts over the private methods of `Parser`, their targets spelled
/// `targets`.
fn parser_contracts(targets: &str) -> String {
    format!(
        r#"[[call-owners]]
name = "parser-mangled"
targets = {targets}
allowed-callers = ["pkg.base:Parser", "pkg.base:Parser.*"]
attribution = "lexical"
unresolved = "report"

[[read-owners]]
name = "parser-mangled-read"
targets = {targets}
allowed-readers = ["pkg.base:Parser", "pkg.base:Parser.*"]
attribution = "lexical"
unresolved = "report"
"#
    )
}

fn parser_project(name: &str, targets: &str) -> Project {
    let p = Project::new(name);
    p.write("src/pkg/__init__.py", "")
        .write("src/pkg/base.py", BASE)
        .write("src/pkg/outside.py", OUTSIDE)
        .write("heki.toml", &parser_contracts(targets));
    p
}

/// What the call and read contracts report on the private methods of `Parser`: a typed
/// receiver, the class object, and a subclass's `self` reaching the mangled name call or read
/// the method; an untyped receiver is an unresolved call; `Parser`'s own methods are allowed.
fn parser_reports() -> Vec<(String, u64, u64, String, String)> {
    vec![
        d("src/pkg/base.py", 15, 16, "MUT204", "JsonParser.peek"),
        d("src/pkg/outside.py", 5, 12, "MUT204", "typed_call"),
        d("src/pkg/outside.py", 9, 12, "MUT204", "class_call"),
        d("src/pkg/outside.py", 13, 12, "MUT203", "typed_read"),
        d("src/pkg/outside.py", 17, 12, "MUT207", "untyped_call"),
    ]
}

#[test]
fn mangled_names_reach_the_private_members_of_their_class() {
    let p = parser_project(
        "private_names_both",
        r#"["pkg.base.Parser._Parser__*", "pkg.base.Parser.__runner", "pkg.base.Parser.__postprocess"]"#,
    );
    assert_eq!(found(&p), parser_reports());
}

#[test]
fn a_target_names_a_private_member_either_way() {
    for (name, targets) in [
        ("private_names_held", r#"["pkg.base.Parser._Parser__*"]"#),
        (
            "private_names_written",
            r#"["pkg.base.Parser.__runner", "pkg.base.Parser.__postprocess"]"#,
        ),
        (
            "private_names_module_part",
            r#"["pkg.base:Parser.__runner", "pkg.base:Parser._Parser__postprocess"]"#,
        ),
    ] {
        let p = parser_project(name, targets);
        assert_eq!(found(&p), parser_reports(), "{targets}");
    }
}

/// A subclass's private name is its own member: `self.__runner` in `JsonParser` is
/// `_JsonParser__runner`, which neither calls `Parser.__runner` nor overrides it, and a class
/// whose name is underscores only mangles nothing.
#[test]
fn a_subclass_private_name_is_its_own_member() {
    let p = Project::new("private_names_subclass");
    p.write("src/pkg/__init__.py", "")
        .write(
            "src/pkg/base.py",
            r#"class Parser:
    def __runner(self) -> int:
        return 1

    def run(self) -> int:
        return self.__runner()


class JsonParser(Parser):
    def __runner(self) -> int:
        return 2

    def own(self) -> int:
        return self.__runner()


class _:
    def __runner(self) -> int:
        return 3
"#,
        )
        .write(
            "src/pkg/outside.py",
            r#"from pkg.base import JsonParser, _


def sub_call(p: JsonParser) -> int:
    return p._JsonParser__runner()


def plain(p: _) -> int:
    return p.__runner()
"#,
        )
        .write(
            "heki.toml",
            r#"[[call-owners]]
name = "runner"
targets = ["pkg.base.Parser.__runner", "pkg.base._.__runner"]
allowed-callers = ["pkg.base:Parser.*"]
attribution = "lexical"
"#,
        );
    assert_eq!(
        found(&p),
        vec![d("src/pkg/outside.py", 9, 12, "MUT204", "plain")]
    );
}

/// Private attributes are members too: what `__init__` stores in `self.__cache`, a class
/// attribute `__limit`, read and written through `_Parser__cache` and `_Parser__limit` by a
/// typed receiver, a class object, or `type[Parser]`, and read on an object of unknown class.
#[test]
fn private_attributes_and_class_receivers() {
    let p = Project::new("private_names_attributes");
    p.write("src/pkg/__init__.py", "")
        .write(
            "src/pkg/base.py",
            r#"class Parser:
    __limit = 3

    def __init__(self) -> None:
        self.__cache: dict[str, int] = {}

    def __runner(self) -> int:
        return self.__limit

    @classmethod
    def build(cls) -> int:
        return cls.__runner(cls())

    def run(self) -> int:
        self.__cache["k"] = 1
        return self.__runner()
"#,
        )
        .write(
            "src/pkg/outside.py",
            r#"from pkg.base import Parser


def through_type(t: type[Parser]) -> int:
    return t._Parser__runner(Parser())


def read_cache(p: Parser) -> object:
    return p._Parser__cache


def write_cache(p: Parser) -> None:
    p._Parser__cache = {}


def read_limit() -> int:
    return Parser._Parser__limit


def unknown_limit(p) -> int:
    return p._Parser__limit
"#,
        )
        .write(
            "heki.toml",
            r#"[[call-owners]]
name = "runner"
targets = ["pkg.base.Parser.__runner"]
allowed-callers = ["pkg.base:Parser.*"]
attribution = "lexical"

[[read-owners]]
name = "state-read"
targets = ["pkg.base.Parser.__cache", "pkg.base.Parser.__limit"]
allowed-readers = ["pkg.base:Parser.*"]
attribution = "lexical"
unresolved = "report"

[[write-owners]]
name = "state-write"
targets = ["pkg.base.Parser.__cache"]
allowed-writers = ["pkg.base:Parser.*"]
attribution = "lexical"
"#,
        );
    assert_eq!(
        found(&p),
        vec![
            d("src/pkg/outside.py", 5, 12, "MUT204", "through_type"),
            d("src/pkg/outside.py", 9, 12, "MUT203", "read_cache"),
            d("src/pkg/outside.py", 13, 5, "MUT201", "write_cache"),
            d("src/pkg/outside.py", 17, 12, "MUT203", "read_limit"),
            d("src/pkg/outside.py", 21, 12, "MUT206", "unknown_limit"),
        ]
    );
}

/// `(line, rule, qualname)` of what `heki check --select <rules>` reports in `src`.
fn selected(p: &Project, rules: &str) -> Vec<(u64, String, String)> {
    let (code, stdout, stderr) = p.check(&["--select", rules, "--output-format", "json", "src"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<_> = diags
        .iter()
        .map(|d| {
            (
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
                d["qualname"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

/// A private property is a member under its mangled name too: storing `self.__key` runs the
/// property `__key` (or raises without a setter) instead of changing what the cached property
/// or the hash read, while storing the private plain attribute `self.__size` is reported.
#[test]
fn a_private_property_is_no_plain_store() {
    let p = Project::new("private_names_properties");
    p.write("src/pkg/__init__.py", "").write(
        "src/pkg/doc.py",
        r#"from functools import cached_property


class Doc:
    def __init__(self, text: str) -> None:
        self._text = text
        self.__size = 0

    @property
    def __key(self) -> str:
        return self._text

    @cached_property
    def total(self) -> int:
        return len(self.__key) + self.__size

    def __hash__(self) -> int:
        return hash((self.__key, self.__size))

    def grow(self) -> None:
        self.__size = 1

    def rekey(self, key: str) -> None:
        self.__key = key
"#,
    );
    assert_eq!(
        selected(&p, "MUT101,MUT107"),
        vec![
            (21, "MUT101".to_owned(), "Doc.grow".to_owned()),
            (21, "MUT107".to_owned(), "Doc.grow".to_owned()),
        ]
    );
}

/// A call target without a class (`__run`) names the member of any class: an untyped
/// `other.__run()` in class `Driver` calls `_Driver__run`, which it matches, while a target
/// with a class (`Runner.__run`) matches only the spelling of that class.
#[test]
fn a_private_target_without_a_class_matches_any_class() {
    let source = r#"class Runner:
    def __run(self) -> int:
        return 1

    def again(self, other) -> int:
        return other.__run()


class Driver:
    def drive(self, other) -> int:
        return other.__run()

    def walk(self, other) -> int:
        return other.__runner()


def plain(other) -> int:
    return other.__run()
"#;
    for (name, target, expected) in [
        (
            "private_names_bare_target",
            "__run",
            vec![
                (6, "MUT207", "Runner.again"),
                (11, "MUT207", "Driver.drive"),
                (18, "MUT207", "plain"),
            ],
        ),
        (
            "private_names_class_target",
            "pkg.base.Runner.__run",
            vec![(6, "MUT207", "Runner.again"), (18, "MUT207", "plain")],
        ),
    ] {
        let p = Project::new(name);
        p.write("src/pkg/__init__.py", "")
            .write("src/pkg/base.py", source)
            .write(
                "heki.toml",
                &format!(
                    r#"[[call-owners]]
name = "runner"
targets = ["{target}"]
allowed-callers = ["pkg.other:*"]
attribution = "lexical"
unresolved = "report"
"#
                ),
            );
        let expected: Vec<_> = expected
            .into_iter()
            .map(|(line, rule, qualname)| (line, rule.to_owned(), qualname.to_owned()))
            .collect();
        assert_eq!(selected(&p, "MUT207"), expected, "{target}");
    }
}

/// A private cached property is found by its mangled name: `self.__total` reads the cached
/// property `__total` (`MUT111`), and a cached property `__prepare` that the getter of `total`
/// reads is a lazy setup step, whose write stays heuristic (`MUT101`). A private plain
/// attribute is read with no report.
#[test]
fn a_private_cached_property_is_found_by_its_member_name() {
    let p = Project::new("private_names_cached_properties");
    p.write("src/pkg/__init__.py", "").write(
        "src/pkg/doc.py",
        r#"from functools import cached_property


class Doc:
    def __init__(self) -> None:
        self.size = 0
        self.__seen = 0

    @cached_property
    def __total(self) -> int:
        return self.size + 1

    def grow(self) -> None:
        self.size = 1

    def show(self) -> int:
        return self.__total

    def seen(self) -> int:
        return self.__seen


class Lazy:
    def __init__(self) -> None:
        self.size = 0

    @cached_property
    def __prepare(self) -> int:
        self.size = 2
        return 1

    @cached_property
    def total(self) -> int:
        return self.__prepare + self.size
"#,
    );
    let (code, stdout, stderr) = p.check(&[
        "--select",
        "MUT101,MUT111",
        "--output-format",
        "json",
        "src",
    ]);
    assert!(code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut reports: Vec<_> = diags
        .iter()
        .map(|d| {
            (
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap(),
                d["qualname"].as_str().unwrap(),
                d["certainty"].as_str().unwrap(),
            )
        })
        .collect();
    reports.sort();
    assert_eq!(
        reports,
        vec![
            (14, "MUT101", "Doc.grow", "exact"),
            (17, "MUT111", "Doc.show", "exact"),
            (29, "MUT101", "Lazy.__prepare", "heuristic"),
        ]
    );
}
