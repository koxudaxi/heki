//! `MUT101` (`cached-property-stale-mutation`) and `MUT111` (`cached-property-stale-read`), both
//! opt-in, end to end: their exact claims, one counterexample per premise of those claims, and the shapes of
//! datamodel-code-generator's parsers that the claims must and must not cover.

mod common;

use common::Project;
use serde_json::Value;

/// `(rule, line, exact)` of every `MUT101` and `MUT111` report, with only the rule's own
/// default selection.
fn tiers(p: &Project) -> Vec<(String, u64, bool)> {
    let (code, stdout, stderr) = p.check(&["--select", "MUT101,MUT111", "--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    diags
        .iter()
        .map(|d| {
            (
                d["rule_code"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["certainty"] == "exact",
            )
        })
        .collect()
}

fn project(name: &str, source: &str) -> Project {
    let p = Project::new(name);
    p.write("pyproject.toml", "[project]\nname = \"t\"\n")
        .write("app.py", source);
    p
}

fn report(rule: &str, line: u64, exact: bool) -> (String, u64, bool) {
    (rule.to_owned(), line, exact)
}

/// The shape of a parser that reads a dialect from the document it parses: `schema_features`
/// reads `self.raw_obj` on its one path, `parse_files` rebinds it in a loop and then only calls
/// project methods and `list.append` on a list it built.
const PARSER: &str = r#"from functools import cached_property


class Parser:
    def __init__(self, raw_obj: dict[str, str]) -> None:
        self.raw_obj = raw_obj

    @cached_property
    def schema_features(self) -> str:
        return self.raw_obj.get("$schema", "")

    def parse_files(self, documents: list[dict[str, str]]) -> list[bool]:
        results = []
        for document in documents:
            self.raw_obj = document
            results.append(self.ref_siblings_apply())
        return results

    def ref_siblings_apply(self) -> bool:
        return self.schema_features.endswith("2020-12/schema")


class FormatsParser:
    def __init__(self, raw_obj: dict[str, str], *, read_uuid_formats: bool) -> None:
        self.raw_obj = raw_obj
        self.read_uuid_formats = read_uuid_formats

    @cached_property
    def data_formats(self) -> tuple[str, ...]:
        return ("uuid", "uuid7") if self.read_uuid_formats else ("uuid",)

    def parse_files(self, documents: list[dict[str, str]]) -> list[bool]:
        results = []
        for document in documents:
            self.raw_obj = document
            results.append("uuid7" in self.data_formats)
        return results


class Constraints:
    def __init__(self, values: dict[str, int]) -> None:
        self._values = dict(values)

    @cached_property
    def has_constraints(self) -> bool:
        return any(value is not None for value in self._values.values())

    def describe(self) -> str:
        return f"constraints={self.has_constraints}"
"#;

#[test]
fn a_parser_rebinding_what_its_cached_dialect_read_is_exact_when_selected() {
    let p = project("mut101_exact_parser", PARSER);
    assert_eq!(
        tiers(&p),
        vec![report("MUT101", 15, true), report("MUT111", 20, true)]
    );
    // Both are opt-in: nothing by default. Selected, nothing on `data_formats` (it reads an
    // option set in `__init__`) or on `has_constraints` (no method changes what it reads after
    // `__init__`).
    let (code, stdout, _) = p.check(&["--output-format", "concise"]);
    assert_eq!(code, 0, "{stdout}");
    let (code, stdout, _) = p.check(&[
        "--output-format",
        "concise",
        "--extend-select",
        "MUT101,MUT111",
    ]);
    assert_eq!(code, 1, "{stdout}");
    assert!(stdout.contains("app.py:15:13: MUT101 "), "{stdout}");
    assert!(stdout.contains("app.py:20:16: MUT111 "), "{stdout}");
    assert!(!stdout.contains("app.py:35:"), "{stdout}");
}

/// The holder of every counterexample: `features` reads `self.raw` (`READ` stands for the read),
/// `replace` rebinds it (`WRITE`), then runs `AFTER`.
fn holder(read: &str, write: &str, after: &str, extra: &str) -> String {
    format!(
        r#"import functools


class Holder:
    def __init__(self, raw: dict[str, str], flag: bool) -> None:
        self.raw = raw
        self.flag = flag
        self.items: list[str] = []

    @functools.cached_property
    def features(self) -> str:
        {read}

    def replace(self, raw: dict[str, str], hook) -> None:
        {write}
        {after}

    def look(self) -> str:
        return self.raw.get("k", "")

    def hook(self) -> str:
        return self.raw.get("k", "")
{extra}"#
    )
}

const READ: &str = r#"return self.raw.get("k", "")"#;
const WRITE: &str = "self.raw = raw";

#[test]
fn every_premise_established_makes_an_exact_report() {
    let p = project("mut101_exact_base", &holder(READ, WRITE, "return None", ""));
    assert_eq!(tiers(&p), vec![report("MUT101", 15, true)]);
    // Through a `self` method that reads on every path, and as an in-place change of a list
    // built in `__init__`.
    let p = project(
        "mut101_exact_helper",
        &holder("return self.look()", WRITE, "return None", ""),
    );
    assert_eq!(tiers(&p), vec![report("MUT101", 15, true)]);
    let p = project(
        "mut101_exact_in_place",
        &holder(
            r#"return ",".join(self.items)"#,
            r#"self.items.append("x")"#,
            "return None",
            "",
        ),
    );
    assert_eq!(tiers(&p), vec![report("MUT101", 15, true)]);
}

#[test]
fn cached_property_stripped_a_subclass_defines_a_plain_property() {
    let extra = "\n\nclass Plain(Holder):\n    @property\n    def features(self) -> str:\n        return \"plain\"\n";
    let p = project(
        "mut101_strip_cached",
        &holder(READ, WRITE, "return None", extra),
    );
    assert_eq!(tiers(&p), vec![report("MUT101", 15, false)]);
    // A subclass that names nothing under the property's name holds no value of it.
    let extra = "\n\nclass Other(Holder):\n    def other(self) -> int:\n        return 1\n";
    let p = project(
        "mut101_strip_cached_holders",
        &holder(READ, WRITE, "return None", extra),
    );
    assert_eq!(tiers(&p), vec![report("MUT101", 15, true)]);
}

#[test]
fn plain_write_stripped_an_in_place_change_of_an_attribute_a_class_may_bind_to_code() {
    let in_place = |extra: &str| {
        holder(
            r#"return ",".join(self.items)"#,
            r#"self.items.append("x")"#,
            "return None",
            extra,
        )
    };
    // A subclass binds the attribute to a property, so the plain read the change assumes may run
    // code.
    let extra = "\n\nclass Sub(Holder):\n    @property\n    def items(self):\n        return []\n";
    let p = project("mut101_strip_in_place_property", &in_place(extra));
    assert_eq!(tiers(&p), vec![report("MUT101", 15, false)]);
    // A base heki does not know may bind it too.
    let source = in_place("")
        .replacen(
            "import functools",
            "import functools; from vendor import Base",
            1,
        )
        .replacen("class Holder:", "class Holder(Base):", 1);
    let p = project("mut101_strip_in_place_unknown_base", &source);
    assert_eq!(tiers(&p), vec![report("MUT101", 15, false)]);
}

#[test]
fn a_helper_defined_on_both_branches_reads_on_every_path_what_both_read() {
    let source = holder("return self.look()", WRITE, "return None", "").replace(
        "    def look(self) -> str:\n        return self.raw.get(\"k\", \"\")\n",
        "    if __debug__:\n        def look(self) -> str:\n            return self.raw.get(\"k\", \"\")\n    else:\n        def look(self) -> str:\n            return self.raw.get(\"k\", \"\")\n",
    );
    let p = project("mut101_helper_both_branches", &source);
    assert_eq!(tiers(&p), vec![report("MUT101", 15, true)]);
}

#[test]
fn reads_on_every_path_stripped_the_read_is_on_one_branch() {
    for read in [
        r#"return self.raw.get("k", "") if self.flag else """#,
        r#"return self.look() if self.flag else """#,
    ] {
        let p = project(
            "mut101_strip_reads",
            &holder(read, WRITE, "return None", ""),
        );
        assert_eq!(tiers(&p), vec![report("MUT101", 15, false)], "{read}");
    }
}

#[test]
fn every_implementation_stripped_an_override_does_not_read() {
    let extra = "\n\nclass Other(Holder):\n    def hook(self) -> str:\n        return \"other\"\n";
    let p = project(
        "mut101_strip_dispatch",
        &holder("return self.hook()", WRITE, "return None", extra),
    );
    assert_eq!(tiers(&p), vec![report("MUT101", 15, false)]);
}

#[test]
fn plain_write_stripped_a_list_subclass_whose_append_changes_nothing() {
    let extra = "\n\nclass Quiet(list):\n    def append(self, item):\n        return None\n";
    let p = project(
        "mut101_strip_plain",
        &holder(
            r#"return ",".join(self.items)"#,
            r#"self.items.append("x")"#,
            "return None",
            extra,
        ),
    );
    assert_eq!(tiers(&p), vec![report("MUT101", 15, false)]);
}

#[test]
fn kept_to_the_return_stripped_code_heki_does_not_follow_runs_after_the_write() {
    for after in [
        "hook(self)",
        "print(hook)",
        "await_nothing = [x for x in hook]",
    ] {
        let p = project("mut101_strip_kept", &holder(READ, WRITE, after, ""));
        assert_eq!(tiers(&p), vec![report("MUT101", 15, false)], "{after}");
    }
    // A library call handed a function the module binds again on some paths runs either.
    let extra = "\n\ndef order(item: str) -> str:\n    return item\n\n\ndef reverse(item: str) -> str:\n    return item[::-1]\n\n\nif __debug__:\n    order = reverse\n";
    let p = project(
        "mut101_strip_kept_rebound",
        &holder(READ, WRITE, r#"sorted(["a"], key=order)"#, extra),
    );
    assert_eq!(tiers(&p), vec![report("MUT101", 15, false)]);
    // A removal on one branch of a project call made after the write.
    let extra = "\n\ndef forget(obj: Holder, flag: bool) -> None:\n    if flag:\n        obj.__dict__.pop(\"features\", None)\n";
    let p = project(
        "mut101_strip_kept_callee",
        &holder(READ, WRITE, "forget(self, self.flag)", extra),
    );
    assert_eq!(tiers(&p), vec![report("MUT101", 15, false)]);
    // Library calls that run no project code keep it.
    let p = project(
        "mut101_kept_library",
        &holder(
            READ,
            WRITE,
            r#"self.items.append(str(1) + ",".join(raw))"#,
            "",
        ),
    );
    assert_eq!(tiers(&p), vec![report("MUT101", 15, true)]);
}

/// A class every special method and attribute hook of which is code of the project.
const HOOKED: &str = r#"


class Hooked:
    value = 0

    def __add__(self, other):
        return 0

    def __eq__(self, other):
        return False

    def __hash__(self):
        return 0

    def __contains__(self, item):
        return False

    def __format__(self, spec):
        return ""

    def __getitem__(self, key):
        return 0

    def __bool__(self):
        return True

    def __neg__(self):
        return 0

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return None

    def __getattr__(self, name):
        return 0

    def __setattr__(self, name, value):
        return None


class Plain:
    value = 0


class Boom(Exception):
    def __init__(self) -> None:
        super().__init__("boom")
"#;

#[test]
fn kept_to_the_return_stripped_implicit_calls_of_project_code() {
    // A special method of an operand's class, an attribute hook, raising a class (which
    // constructs it), a class statement, and an import of a project module run code of the
    // project after the write.
    let typed = |class: &str, after: &str| {
        holder(READ, WRITE, after, HOOKED)
            .replacen(
                "raw: dict[str, str], hook)",
                &format!("raw: dict[str, str], hook: {class})"),
                1,
            )
            .replacen(
                "import functools\n",
                "import functools\nimport json\nimport logging\n",
                1,
            )
    };
    for after in [
        "unused = hook + 1",
        "unused = hook == 1",
        "unused = 1 in hook",
        "unused = {hook}",
        r#"unused = f"{hook}""#,
        "unused = hook[0]",
        "unused = -hook",
        "if hook:\n            return None",
        "with hook:\n            pass",
        "unused = hook.missing",
        "hook.value = 1",
        "del hook.value",
        "try:\n            raise Boom\n        except Boom:\n            pass",
        "class Local:\n            pass",
        "import app",
    ] {
        let p = project("mut101_strip_implicit", &typed("Hooked", after));
        assert_eq!(tiers(&p), vec![report("MUT101", 17, false)], "{after}");
    }
    // `from pkg import name` loads the package, whatever the package binds the name to: here the
    // function of `pkg.impl`, or of the submodule of the same name.
    for (init, module) in [
        ("from pkg.impl import parse\n", "pkg/impl.py"),
        ("from .parse import parse\n", "pkg/parse.py"),
    ] {
        let p = project(
            "mut101_strip_from_package",
            &typed("Plain", "from pkg import parse"),
        );
        p.write("pkg/__init__.py", init)
            .write(module, "def parse():\n    return None\n");
        assert_eq!(tiers(&p), vec![report("MUT101", 17, false)], "{init}");
    }
    // An attribute of a value that may be of either class runs the hook of one.
    let p = project(
        "mut101_strip_union_attr",
        &typed("Hooked | Plain", "unused = hook.value"),
    );
    assert_eq!(tiers(&p), vec![report("MUT101", 17, false)]);
    // An attribute of a value of unknown class may be a property of the project.
    let p = project(
        "mut101_strip_unknown_attr",
        &holder(READ, WRITE, "unused = hook.value", HOOKED),
    );
    assert_eq!(tiers(&p), vec![report("MUT101", 15, false)]);
    // The same on builtin values and on classes without such methods run no code of the project.
    for after in [
        "unused = hook == 1",
        "unused = len(raw) + 1",
        r#"unused = "%d" % len(raw)"#,
        "unused = {len(raw)}",
        r#"unused = {"k": 1}["k"]"#,
        "unused = [len(raw)][0:1]",
        "unused = hook.value",
        "unused = Plain.value",
        "unused = self.__class__",
        "unused = raw.get",
        "unused = json.dumps",
        r#"unused = "abc".upper"#,
        "hook.value = 1",
        "Plain.value = 1",
        "total = 0\n        for key in raw:\n            total += 1",
        "if raw:\n            return None",
        "import json",
    ] {
        let p = project("mut101_kept_implicit", &typed("Plain", after));
        assert_eq!(tiers(&p), vec![report("MUT101", 17, true)], "{after}");
    }
    // An attribute of an object of a library class the catalog covers, read or stored, is a
    // call of that library handed no project code.
    for after in ["unused = hook.level", "hook.level = 10"] {
        let p = project("mut101_kept_library_attr", &typed("logging.Logger", after));
        assert_eq!(tiers(&p), vec![report("MUT101", 17, true)], "{after}");
    }
}

/// What a writer reaches besides `self`: another holder, a class with no property of the
/// name, a registry holding a holder, a module list of them, and project generators.
const REFERENCES: &str = r#"


class Plain:
    value = 0


class Registry:
    def __init__(self, current: Holder) -> None:
        self.current = current

    def reset(self) -> None:
        self.current.features = "replaced"


HOLDERS: list[Holder] = []
REGISTRY = Registry(Holder({}, False))


def reset() -> None:
    REGISTRY.reset()


def drop(h: Holder) -> None:
    del h.features


def take(*args) -> None:
    return None


def rows(h: Holder) -> Iterator[int]:
    del h.features
    yield 1


class Truthy:
    def __bool__(self) -> bool:
        return True


class Text(str):
    def __len__(self) -> int:
        return 1


class Hooked:
    def __eq__(self, other):
        return Truthy()

    def __hash__(self) -> int:
        return 0

    def __format__(self, spec: str) -> str:
        return Text("x")

    def __str__(self) -> str:
        return Text("x")

    def __repr__(self) -> str:
        return "hooked"

    def __iter__(self):
        return iter((1, 2))

    def __index__(self) -> int:
        return 1


class Mode(enum.Enum):
    A = 1


class Desc:
    def __set__(self, obj, value) -> None:
        return None


class WithDesc:
    q = Desc()
    value = 0


class Kind(object):
    pass


class Names(list):
    pass


class Table(dict):
    pass


class Failure(Exception):
    pass
"#;

/// `holder` whose writer runs `before` ahead of the write and `after` it, and takes another
/// holder (`other`), a `Plain`, a `Hooked` (`hook`), a `Mode`, a `WithDesc`, an iterator, a
/// value heki does not know (`exc`), values of unions, a set, a `Kind`, an exception, and
/// `*others`. The write is on line 18.
fn reaching(before: &str, after: &str) -> String {
    holder(
        READ,
        &format!("{before}\n        {WRITE}"),
        after,
        REFERENCES,
    )
    .replacen(
        "import functools\n",
        "import enum\nimport functools\nfrom typing import Iterator\n",
        1,
    )
    .replacen(
        "raw: dict[str, str], hook)",
        "raw: dict[str, str], hook: \"Hooked\", other: \"Holder\", plain: \"Plain\", \
         mode: \"Mode\", w: \"WithDesc\", items: Iterator[str], exc, \
         either: \"Holder | Plain\", mixed: dict[str, str] | list[str], n: int | None, \
         tags: set[str], kind: \"Kind\", error: \"Failure\", names: \"list[str] | Names\", \
         *others)",
        1,
    )
}

#[test]
fn kept_to_the_return_stripped_stores_and_deletions_through_other_references() {
    // A store or a deletion of the property's name through any reference that may be the
    // holder replaces or removes the value, as do a change of an object's `__dict__` under the
    // name, through `setattr`, a store of its `__dict__` or `__class__`, and an item of its
    // `__dict__` under a key heki does not know.
    for after in [
        r#"other.features = "replacement""#,
        r#"HOLDERS[0].features = "x""#,
        "def inner():\n            other.features = \"x\"\n        inner()",
        r#"others[0].features = "x""#,
        "reset()",
        r#"setattr(other, "features", "x")"#,
        r#"other.__dict__["features"] = "x""#,
        r#"vars(other)["features"] = "x""#,
        r#"other.__dict__.update(features="x")"#,
        "del other.features",
        r#"del other.__dict__["features"]"#,
        "other.__dict__ = {}",
        "other.__class__ = Holder",
        r#"other.__dict__[exc] = "x""#,
        "del other.__dict__[exc]",
        "del other.__dict__[str(len(raw))]",
        r#"either.features = "x""#,
        "w.q = 1",
    ] {
        let p = project("mut101_strip_references", &reaching("", after));
        assert_eq!(tiers(&p), vec![report("MUT101", 18, false)], "{after}");
    }
    // A name no property getter has, an object whose classes have no property of the name, a
    // dict that is no `__dict__`, and an attribute no descriptor binds.
    for after in [
        "other.flag = True",
        r#"plain.features = "x""#,
        "del plain.features",
        r#"other.__dict__["unrelated"] = "x""#,
        r#"del other.__dict__["unrelated"]"#,
        r#"{"k": 1}["features"] = 1"#,
        "w.value = 1",
    ] {
        let p = project("mut101_kept_references", &reaching("", after));
        assert_eq!(tiers(&p), vec![report("MUT101", 18, true)], "{after}");
    }
}

#[test]
fn kept_to_the_return_stripped_values_whose_class_is_not_established() {
    // A comparison or a conversion gives what a method of the operand's class returns, whose
    // truth test may run code of the project after the write; a generator, a value annotated
    // with an abstract class, an unpacking, a call's `*` argument, an in-place `+=`, a sequence
    // repeated, and a builtin container's comparison or string run code of the operands, of
    // their items, or of the generator's body; a library class's operator runs the library's,
    // handed an instance of the project; a raise of a value heki cannot class may construct it.
    for (before, after) in [
        ("flag = hook == 0", "if flag:\n            pass"),
        (r#"text = f"{hook}""#, "if text:\n            pass"),
        ("text = str(hook)", "if text:\n            pass"),
        (
            "gen = (drop(self) for _ in range(1))",
            "for _ in gen:\n            pass",
        ),
        ("it = rows(self)", "next(it)"),
        ("", "next(items)"),
        ("", "first, second = hook"),
        ("", "take(*hook)"),
        ("", "self.items += hook"),
        ("", "unused = [0] * hook"),
        ("", "unused = [hook] == [1]"),
        ("", "unused = [hook] < [1]"),
        // A dict compares a key with each key it holds under an equal hash, of classes heki
        // does not establish, and so does a set's union with each item.
        ("", r#"unused = raw["k"]"#),
        ("", "unused = mixed[0]"),
        ("", r#"unused = tags | {"x"}"#),
        ("", "unused = str([hook])"),
        ("", "if mode == Mode.A:\n            pass"),
        (
            "",
            "try:\n            raise exc\n        except Exception:\n            pass",
        ),
        (
            "",
            "try:\n            raise ValueError(\"x\") from exc\n        except Exception:\n            pass",
        ),
    ] {
        let p = project("mut101_strip_established", &reaching(before, after));
        assert_eq!(tiers(&p), vec![report("MUT101", 18, false)], "{after}");
    }
    // The same on values heki builds or whose classes run `object`'s or a builtin type's own.
    for (before, after) in [
        ("flag = len(raw) == 0", "if flag:\n            pass"),
        ("flag = self.flag == True", "if flag:\n            pass"),
        ("flag = plain == other", "if flag:\n            pass"),
        ("flag = n == 1", "if flag:\n            pass"),
        ("flag = mixed == {}", "if flag:\n            pass"),
        ("flag = kind == kind", "if flag:\n            pass"),
        ("", r#"names.append("x")"#),
        ("", r#"unused = Table.fromkeys(["a"])"#),
        (
            "",
            "try:\n            raise error\n        except Exception:\n            pass",
        ),
        (r#"text = f"{len(raw)}""#, "if text:\n            pass"),
        ("text = str(self.flag)", "if text:\n            pass"),
        ("count = len(raw)", "if -count < 0:\n            pass"),
        ("", "first, second = (1, 2)"),
        ("", "unused = [0] * len(raw)"),
        ("", "unused = [len(raw)] == [1]"),
        ("", "unused = [len(raw)] < [1]"),
        ("", "unused = [len(raw)] <= [1]"),
        ("", "unused = str([len(raw)])"),
        (
            "",
            "try:\n            raise ValueError(\"x\") from None\n        except Exception:\n            pass",
        ),
    ] {
        let p = project("mut101_kept_established", &reaching(before, after));
        assert_eq!(
            tiers(&p),
            vec![report("MUT101", 18, true)],
            "{before} / {after}"
        );
    }
}

/// A holder whose `add` runs `write`, a generator that removes the cached value between two
/// items, and a reader of the value.
fn writing(write: &str) -> String {
    format!(
        r#"from collections.abc import Iterator
from functools import cached_property


class Key:
    def __hash__(self) -> int:
        return 0


class Holder:
    def __init__(self) -> None:
        self.items: list[int] = []
        self.counts: dict[Key, int] = {{}}

    @cached_property
    def snapshot(self) -> int:
        return len(self.items) + len(self.counts)

    def add(self, parts: Iterator[int], key: Key) -> None:
        {write}


def forgetting(holder: Holder) -> Iterator[int]:
    yield 1
    del holder.snapshot
    yield 2


def total(holder: Holder) -> int:
    return holder.snapshot
"#
    )
}

#[test]
fn kept_to_the_return_stripped_code_the_writing_instruction_runs() {
    // The writing instruction itself may run code of the project while it changes the value:
    // iterating its argument (which may remove the cached value between two items) or hashing
    // a key.
    for write in [
        "self.items.extend(parts)",
        "self.items.extend(forgetting(self))",
        "self.counts[key] = 1",
    ] {
        let p = project("mut101_strip_writing", &writing(write));
        assert_eq!(
            tiers(&p),
            vec![report("MUT101", 20, false), report("MUT111", 30, false)],
            "{write}"
        );
    }
    let p = project("mut101_kept_writing", &writing("self.items.extend([1, 2])"));
    assert_eq!(
        tiers(&p),
        vec![report("MUT101", 20, true), report("MUT111", 30, true)]
    );
}

/// A reader of `features`, and the classes it may read it on.
fn reader(body: &str, extra: &str) -> String {
    holder(
        READ,
        WRITE,
        "return None",
        &format!("{extra}\n\ndef check(obj: Holder) -> bool:\n    {body}\n"),
    )
}

#[test]
fn mut111_is_exact_when_every_class_of_the_object_runs_an_exact_write() {
    let p = project(
        "mut111_exact",
        &reader(r#"return obj.features.startswith("x")"#, ""),
    );
    assert_eq!(
        tiers(&p),
        vec![report("MUT101", 15, true), report("MUT111", 26, true)]
    );
}

#[test]
fn runs_the_writer_stripped_a_subclass_replaces_the_writer() {
    let extra = "\n\nclass Sibling(Holder):\n    def replace(self, raw, hook) -> None:\n        return None\n";
    let p = project(
        "mut111_strip_runs",
        &reader(r#"return obj.features.startswith("x")"#, extra),
    );
    let found = tiers(&p);
    assert!(found.contains(&report("MUT101", 15, true)), "{found:?}");
    assert!(found.iter().all(|r| r.0 != "MUT111" || !r.2), "{found:?}");
}

#[test]
fn reads_the_property_stripped_a_name_the_code_does_not_show() {
    let p = project(
        "mut111_strip_reads",
        &reader(
            r#"return bool(getattr(obj, "".join(("feat", "ures"))))"#,
            "",
        ),
    );
    assert_eq!(tiers(&p), vec![report("MUT101", 15, true)]);
}

/// `holder` with the cached property's decorator imported by `import` and written `decorator`
/// (the import stays on the first line).
fn holder_decorated(import: &str, decorator: &str, extra: &str) -> String {
    holder(READ, WRITE, "return None", extra)
        .replacen("import functools", import, 1)
        .replacen("@functools.cached_property", decorator, 1)
}

#[test]
fn cached_property_only_of_a_decorator_the_catalog_lists() {
    let cases = [
        // A library's decorator the catalog lists, and `functools`' under an alias.
        (
            "django",
            "from django.utils.functional import cached_property",
            "@cached_property",
            "",
            true,
        ),
        (
            "alias",
            "from functools import cached_property as memo",
            "@memo",
            "",
            true,
        ),
        // A `cached_property` of a library the catalog does not list.
        (
            "lookalike",
            "from mylib import cached_property",
            "@cached_property",
            "",
            false,
        ),
        // Werkzeug keeps the value in a slot on an instance without a `__dict__`.
        (
            "werkzeug",
            "from werkzeug.utils import cached_property",
            "@cached_property",
            "",
            true,
        ),
        (
            "werkzeug_slots",
            "from werkzeug.utils import cached_property",
            "@cached_property",
            "\n\nclass Slotted(Holder):\n    __slots__ = (\"_cache_features\",)\n",
            false,
        ),
    ];
    for (name, import, decorator, extra, exact) in cases {
        let p = project(
            &format!("mut101_cached_{name}"),
            &holder_decorated(import, decorator, extra),
        );
        assert_eq!(tiers(&p), vec![report("MUT101", 15, exact)], "{name}");
    }
}

#[test]
fn plain_write_stripped_a_base_heki_does_not_know() {
    // In the writer's own class: the base may define `__setattr__` or a data descriptor.
    let source = holder(READ, WRITE, "return None", "")
        .replacen(
            "import functools",
            "import functools; from vendor import Base",
            1,
        )
        .replacen("class Holder:", "class Holder(Base):", 1);
    let p = project("mut101_strip_unknown_base", &source);
    assert_eq!(tiers(&p), vec![report("MUT101", 15, false)]);
    // In a project subclass that may be the instance's class, after the class that defines the
    // property (so the getter is still the first definition along its MRO).
    let extra = "\n\nfrom vendor import Mixin\n\n\nclass Traced(Holder, Mixin):\n    pass\n";
    let p = project(
        "mut101_strip_unknown_subclass_base",
        &holder(READ, WRITE, "return None", extra),
    );
    assert_eq!(tiers(&p), vec![report("MUT101", 15, false)]);
}

#[test]
fn setup_steps_stay_heuristic() {
    // A finalizer: no read of the instance follows it.
    let source = holder(READ, WRITE, "return None", "")
        .replacen(
            "def replace(self, raw: dict[str, str], hook) -> None:",
            "def __del__(self) -> None:",
            1,
        )
        .replacen(
            "        self.raw = raw\n        return None\n",
            "        self.raw = {}\n        return None\n",
            1,
        );
    let p = project("mut101_setup_finalizer", &source);
    assert_eq!(tiers(&p), vec![report("MUT101", 15, false)]);
    // A step of two-phase setup: a method `__init__` calls on the instance.
    let source = holder(READ, WRITE, "return None", "").replacen(
        "        self.items: list[str] = []\n",
        "        self.items: list[str] = []\n        self.replace(raw, None)\n",
        1,
    );
    let p = project("mut101_setup_two_phase", &source);
    assert_eq!(tiers(&p), vec![report("MUT101", 16, false)]);
    // Lazy setup through a second cached property that nothing invalidates, which the property's
    // getter reads on every path.
    let lazy = r#"import functools


class Holder:
    def __init__(self, raw: dict[str, str]) -> None:
        self.raw = raw

    @functools.cached_property
    def loaded(self) -> bool:
        self.raw = {"k": "v"}
        return True

    @functools.cached_property
    def features(self) -> str:
        if self.loaded:
            return self.raw.get("k", "")
        return self.raw.get("k", "")
"#;
    let p = project("mut101_setup_lazy", lazy);
    assert_eq!(tiers(&p), vec![report("MUT101", 10, false)]);
    // Once the property's getter does not read it, the write is exact.
    let p = project(
        "mut101_setup_lazy_unread",
        &lazy.replace(
            "        if self.loaded:\n            return self.raw.get(\"k\", \"\")\n",
            "",
        ),
    );
    assert_eq!(tiers(&p), vec![report("MUT101", 10, true)]);
    // A function of the project that removes the lazy property's value by name, whatever else
    // it may run there, makes its getter run again: the write is no setup step.
    let p = project(
        "mut101_setup_lazy_removed",
        &format!("{lazy}\n\ndef forget(obj) -> None:\n    del obj.loaded\n"),
    );
    assert_eq!(tiers(&p), vec![report("MUT101", 10, true)]);
}

#[test]
fn cached_property_stripped_by_a_subclass() {
    // A subclass with a metaclass of its own may turn the attribute into a descriptor.
    let extra =
        "\n\nclass Meta(type):\n    pass\n\n\nclass Sub(Holder, metaclass=Meta):\n    pass\n";
    let p = project(
        "mut101_strip_sub_metaclass",
        &holder(READ, WRITE, "return None", extra),
    );
    assert_eq!(tiers(&p), vec![report("MUT101", 15, false)]);
    // A subclass that runs no write and defines the name as a plain property: the write's claim
    // holds on the classes that run it, but a reader of an object that may be of the subclass
    // may read no cached value.
    let extra = "\n\nclass Sub(Holder):\n    @property\n    def features(self) -> str:\n        return \"plain\"\n\n    def replace(self, raw: dict[str, str], hook) -> None:\n        return None\n";
    let p = project(
        "mut111_strip_sub_plain",
        &reader(r#"return obj.features.startswith("x")"#, extra),
    );
    assert_eq!(
        tiers(&p),
        vec![report("MUT101", 15, true), report("MUT111", 35, false)]
    );
}
