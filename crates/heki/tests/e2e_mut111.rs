//! `MUT111` (`cached-property-stale-read`) end to end: the functions reading a
//! `@cached_property` that a method of its class family can leave stale (a write `MUT101`
//! finds). The reads in a function's own body on `self`, on a parameter of the class in another
//! module, through `getattr` and `__dict__`, and in a property getter; which classes' instances
//! run a writer (subclasses, siblings, overrides, `super()`); the exclusions `MUT101` makes; the
//! opt-in selection, suppressions, and `heki rule`.

mod common;

use common::Project;
use serde_json::Value;

fn diagnostics(p: &Project, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["--output-format", "json"];
    all.extend_from_slice(args);
    let (code, stdout, stderr) = p.check(&all);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

/// `(path, line, qualname, target)` of every `MUT111` diagnostic.
fn reads(diags: &[Value]) -> Vec<(String, u64, String, String)> {
    diags
        .iter()
        .filter(|d| d["rule_code"] == "MUT111")
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["qualname"].as_str().unwrap().to_owned(),
                d["target"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn d(path: &str, line: u64, qualname: &str, target: &str) -> (String, u64, String, String) {
    (path.into(), line, qualname.into(), target.into())
}

/// `dialect` goes stale when `parse_all` rebinds `raw`; `title` reads only what `__init__` sets,
/// and `set_flag` invalidates `strict` on every path.
const PARSER: &str = r#"from functools import cached_property


class Parser:
    def __init__(self, raw: dict[str, str], name: str) -> None:
        self.raw = raw
        self.name = name
        self.flags: dict[str, bool] = {}

    @cached_property
    def dialect(self) -> str:
        return self.raw.get("$schema", "")

    @cached_property
    def title(self) -> str:
        return self.name.title()

    @cached_property
    def strict(self) -> bool:
        return bool(self.flags)

    def parse_all(self, docs: list[dict[str, str]]) -> None:
        for doc in docs:
            self.raw = doc
            self.parse_one()

    def set_flag(self, key: str) -> None:
        self.flags[key] = True
        self.__dict__.pop("strict", None)

    def parse_one(self) -> bool:
        if self.dialect:
            return self.dialect == "draft-07"
        return False

    @property
    def is_draft(self) -> bool:
        return getattr(self, "dialect") == "draft"

    def computed(self) -> str | None:
        return self.__dict__.get("dialect")

    def label(self) -> str:
        return self.title + str(self.strict)

    def through_property(self) -> bool:
        return self.is_draft
"#;

const HELPERS: &str = r#"from pkg.parser import Parser


def dialect_of(parser: Parser) -> str:
    return parser.dialect


def title_of(parser: Parser) -> str:
    return parser.title


def through_helper(parser: Parser) -> str:
    return dialect_of(parser)
"#;

#[test]
fn mut111_reports_own_reads_on_self_and_on_objects_of_the_class() {
    let p = Project::new("mut111_reads");
    p.write("pkg/__init__.py", "")
        .write("pkg/parser.py", PARSER)
        .write("pkg/helpers.py", HELPERS);
    let diags = diagnostics(&p, &["--extend-select", "MUT111"]);
    let (parser, helpers) = ("pkg/parser.py", "pkg/helpers.py");
    // Reported once per function, at its first read: on `self`, in a property getter through
    // `getattr`, the cached value itself through `__dict__`, and on a parameter in another
    // module. Not reported: `title` (set only by `__init__`) and `strict` (invalidated on every
    // path), and the functions that read `dialect` only through the property or the helper.
    assert_eq!(
        reads(&diags),
        vec![
            d(helpers, 5, "dialect_of", "Parser.dialect"),
            d(parser, 32, "Parser.parse_one", "Parser.dialect"),
            d(parser, 38, "Parser.is_draft", "Parser.dialect"),
            d(parser, 41, "Parser.computed", "Parser.dialect"),
        ],
        "{diags:#?}"
    );
    let first = diags.iter().find(|d| d["rule_code"] == "MUT111").unwrap();
    assert_eq!(first["rule_slug"], "cached-property-stale-read");
    assert_eq!(
        first["message"],
        "`dialect_of` reads `dialect`, which `Parser.parse_all` can leave stale"
    );
    assert_eq!(
        first["help"],
        "invalidate it in `Parser.parse_all` after the change, compute it without caching, or read it only before `Parser.parse_all` runs"
    );
    assert!(first["fix"].is_null());
    let related = first["related"].as_array().unwrap();
    assert_eq!(related.len(), 1, "{related:#?}");
    assert_eq!(related[0]["path"], parser);
    assert_eq!(related[0]["line"], 24);
    assert_eq!(
        related[0]["message"],
        "`Parser.parse_all` changes `self.raw` without invalidating `dialect`"
    );
}

/// `Growing.grow` and `Loader.load` leave the totals stale; `ResettingLoader` replaces the
/// writer with one that invalidates, `CheckedLoader` calls it through `super()`,
/// `ReplacingLoader` replaces it with another stale writer, and `CheckedResettingLoader` calls
/// it through `super()` and invalidates.
const MODELS: &str = r#"from functools import cached_property


class Base:
    def __init__(self) -> None:
        self.items: list[int] = []

    @cached_property
    def total(self) -> int:
        return len(self.items)


class Growing(Base):
    def grow(self, x: int) -> None:
        self.items.append(x)


class Fixed(Base):
    def show(self) -> int:
        return self.total


def show_base(b: Base) -> int:
    return b.total


class Loader:
    def __init__(self) -> None:
        self.rows: list[int] = []

    @cached_property
    def count(self) -> int:
        return len(self.rows)

    def load(self, rows: list[int]) -> None:
        self.rows = rows


class CheckedLoader(Loader):
    def load(self, rows: list[int]) -> None:
        super().load(rows)
        self.check()

    def check(self) -> int:
        return self.count


class ResettingLoader(Loader):
    def load(self, rows: list[int]) -> None:
        self.rows = rows
        del self.count

    def check(self) -> int:
        return self.count


class ReplacingLoader(Loader):
    def load(self, rows: list[int]) -> None:
        self.rows = list(rows)

    def check(self) -> int:
        return self.count


class CheckedResettingLoader(Loader):
    def load(self, rows: list[int]) -> None:
        super().load(rows)
        self.__dict__.pop("count", None)

    def check(self) -> int:
        return self.count
"#;

#[test]
fn mut111_follows_the_writers_an_instance_runs() {
    let p = Project::new("mut111_instances");
    p.write("pkg/__init__.py", "")
        .write("pkg/models.py", MODELS);
    let diags = diagnostics(&p, &["--extend-select", "MUT111"]);
    let m = "pkg/models.py";
    // A `Base` may be a `Growing`; a `Fixed` never runs `Growing.grow`. `Loader.load` runs on a
    // `CheckedLoader` through `super()`, not on a `ResettingLoader`; a `ReplacingLoader` runs
    // its own stale writer; a `CheckedResettingLoader` invalidates after the call.
    assert_eq!(
        reads(&diags),
        vec![
            d(m, 24, "show_base", "Base.total"),
            d(m, 45, "CheckedLoader.check", "Loader.count"),
            d(m, 62, "ReplacingLoader.check", "Loader.count"),
        ],
        "{diags:#?}"
    );
    let message = |qualname: &str| {
        diags
            .iter()
            .find(|d| d["rule_code"] == "MUT111" && d["qualname"] == qualname)
            .unwrap()["message"]
            .clone()
    };
    assert_eq!(
        message("CheckedLoader.check"),
        "`CheckedLoader.check` reads `count`, which `Loader.load` can leave stale"
    );
    assert_eq!(
        message("ReplacingLoader.check"),
        "`ReplacingLoader.check` reads `count`, which `ReplacingLoader.load` can leave stale"
    );
}

/// A subclass in another module reads the property of its base, whose writer is in the base's
/// module, and so does a reader typed with the base.
const BASE: &str = r#"from functools import cached_property


class Document:
    def __init__(self, text: str) -> None:
        self.text = text

    @cached_property
    def words(self) -> int:
        return len(self.text.split())

    def replace(self, text: str) -> None:
        self.text = text
"#;

const DERIVED: &str = r#"from pkg.base import Document


class Page(Document):
    def append(self, more: str) -> None:
        self.text = self.text + more

    def summary(self) -> str:
        return f"{self.words} words"


def count_words(doc: Document) -> int:
    return doc.words
"#;

#[test]
fn mut111_reports_readers_in_other_modules_than_the_writer() {
    let p = Project::new("mut111_modules");
    p.write("pkg/__init__.py", "")
        .write("pkg/base.py", BASE)
        .write("pkg/derived.py", DERIVED);
    let diags = diagnostics(&p, &["--select", "MUT111"]);
    let derived = "pkg/derived.py";
    assert_eq!(
        reads(&diags),
        vec![
            d(derived, 9, "Page.summary", "Document.words"),
            d(derived, 13, "count_words", "Document.words"),
        ],
        "{diags:#?}"
    );
    // `MUT101` need not be selected. Every writer that runs on the instances is related.
    assert!(diags.iter().all(|d| d["rule_code"] == "MUT111"));
    let summary = &diags[0];
    assert_eq!(
        summary["message"],
        "`Page.summary` reads `words`, which `Document.replace` can leave stale"
    );
    let related: Vec<(&str, u64)> = summary["related"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["path"].as_str().unwrap(), r["line"].as_u64().unwrap()))
        .collect();
    assert_eq!(related, vec![("pkg/base.py", 13), (derived, 6)]);
    assert_eq!(
        diags[1]["message"],
        "`count_words` reads `words`, which `Document.replace` can leave stale"
    );
}

#[test]
fn mut111_is_opt_in_and_counts_suppressed_writes() {
    let p = Project::new("mut111_selection");
    p.write("pkg/__init__.py", "")
        .write("pkg/parser.py", PARSER)
        .write("pkg/helpers.py", HELPERS);
    // Neither the default rules nor the `MUT` prefix select it, or the opt-in `MUT101`.
    for args in [&[][..], &["--select", "MUT"]] {
        let diags = diagnostics(&p, args);
        assert!(reads(&diags).is_empty(), "{diags:#?}");
        assert!(
            diags.iter().all(|d| d["rule_code"] != "MUT101"),
            "{diags:#?}"
        );
    }
    let by_slug = diagnostics(&p, &["--select", "cached-property-stale-read"]);
    assert_eq!(reads(&by_slug).len(), 4);

    // A suppressed write still makes the property stale; `per-file-ignores` and an inline
    // suppression at a read remove the reports there.
    p.write(
        "pkg/parser.py",
        &PARSER
            .replace("self.raw = doc\n", "self.raw = doc  # heki: ignore[MUT101]\n")
            .replace(
                "return self.__dict__.get(\"dialect\")\n",
                "return self.__dict__.get(\"dialect\")  # heki: ignore[MUT111]\n",
            ),
    )
    .write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"MUT111\"]\nper-file-ignores = { \"pkg/helpers.py\" = [\"MUT111\"] }\n",
    );
    let diags = diagnostics(&p, &[]);
    assert!(
        diags.iter().all(|d| d["rule_code"] == "MUT111"),
        "{diags:#?}"
    );
    let qualnames: Vec<String> = reads(&diags).into_iter().map(|r| r.2).collect();
    assert_eq!(qualnames, vec!["Parser.parse_one", "Parser.is_draft"]);

    let (code, doc, _) = p.heki(&["rule", "MUT111"]);
    assert_eq!(code, 0);
    assert!(
        doc.starts_with("# cached-property-stale-read (MUT111)\n\n"),
        "{doc}"
    );
    let sections = [
        "\n## What it does\n",
        "\n## Why is this bad?\n",
        "\n## Example\n",
        "\nUse instead:\n",
        "\n## How to fix\n",
        "\n## Fix safety\n",
    ];
    let positions: Vec<usize> = sections
        .iter()
        .map(|s| doc.find(s).unwrap_or_else(|| panic!("{s}: {doc}")))
        .collect();
    assert!(positions.is_sorted(), "{doc}");
}
