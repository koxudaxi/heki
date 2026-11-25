//! Receivers whose class the language or the standard library fixes, end to end through the real
//! binary, under an effect boundary that reports what it cannot judge (`unresolved = "report"`):
//! an instance's own namespace (`obj.__dict__`, `vars(obj)`), a builtin dict when reading it runs
//! no code; the methods a `NamedTuple` class gets (`_replace`, `_make`, `_asdict`, `_fields`,
//! `_field_defaults`); and the value a holder returns: a `ContextVar[T]`'s `get()`, with the
//! default it is passed, and a future's `result()`.
//!
//! Every scope marks the lines that must be reported with what is reported there (`# write`: a
//! write of `Field.required`, `# unresolved`: a call heki cannot resolve); every other line must
//! not be.

mod common;

use common::Project;
use serde_json::Value;

const CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "no-required-writes"
scopes = ["pkg.passes.*"]
write-universe = ["Field.required"]
allow-writes = []
unresolved = "report"
"#;

const MODEL: &str = r#"import asyncio
import concurrent.futures
from contextvars import ContextVar
from typing import Any, Final, NamedTuple

from pydantic import BaseModel
from vendor import Unknown


class Field:
    def __init__(self) -> None:
        self.required = False


class Model(BaseModel):
    required: bool = False


class Hooked:
    def __getattribute__(self, name: str) -> Any:
        return object.__getattribute__(self, name)


class OwnDict:
    @property
    def __dict__(self) -> Any:
        return {}


class Outside(Unknown):
    pass


class Memo:
    def bump(self, field: Field) -> None:
        field.required = True


class Other:
    def bump(self, field: Field) -> None:
        field.required = True


class Context(NamedTuple):
    module: str

    def require(self, field: Field) -> None:
        field.required = True


class Base(NamedTuple):
    module: str


class Renamed(Base):
    def _replace(self, **changes: Any) -> "Renamed":
        FIELD.required = True
        return self


def reads_dict_entry(field: Field) -> object:
    return field.__dict__.get("_origin")


def make_memo() -> Memo:
    return Memo()


FIELD = Field()
MEMO: Final[ContextVar[Memo | None]] = ContextVar("memo", default=None)
PLAIN: ContextVar[Memo] = ContextVar("plain")
UNTYPED = ContextVar("untyped")
POOL = concurrent.futures.ThreadPoolExecutor()
"#;

/// `obj.__dict__` and `vars(obj)` of an instance whose classes read it plainly (a project class,
/// a pydantic model, a member of a union) are a dict: its reads resolve, and `pop` and item
/// stores stay writes of the attribute they name. A class object's namespace is a read-only
/// proxy, and a class defining `__getattribute__` or binding `__dict__`, a base heki knows
/// nothing of, an unannotated parameter, `vars()` of the frame, and a `vars` the module binds
/// give no class.
const NAMESPACES: &str = r#"from typing import Any

from pkg.model import Field, Hooked, Memo, Model, Outside, OwnDict, reads_dict_entry


def dict_get(field: Field) -> object:
    return field.__dict__.get("_origin")


def vars_get(field: Field) -> object:
    return vars(field).get("_origin")


def views(field: Field) -> object:
    return list(field.__dict__.keys()), list(vars(field).items()), list(field.__dict__.values())


def contains(field: Field, name: str) -> bool:
    return name in field.__dict__ or field.__dict__.get(name) is None


def model_get(model: Model) -> object:
    return model.__dict__.get("_origin")


def previous_get(fields: list[Field]) -> None:
    previous = None
    for field in fields:
        if previous is not None:
            previous.__dict__.get("_origin")
        previous = field


def union_get(obj: Field | Memo) -> object:
    return obj.__dict__.get("_origin")


def through_a_helper(field: Field) -> object:
    return reads_dict_entry(field)


def pops_required(field: Field) -> object:
    return field.__dict__.pop("required", None)  # write


def stores_required(field: Field) -> None:
    vars(field)["required"] = True  # write


def class_namespace(cls: type[Field]) -> object:
    return cls.__dict__.get("required")  # unresolved


def hooked(obj: Hooked) -> object:
    return obj.__dict__.get("x")  # unresolved


def own_dict(obj: OwnDict) -> object:
    return obj.__dict__.get("x")  # unresolved


def outside(obj: Outside) -> object:
    return obj.__dict__.get("x")  # unresolved


def untyped(obj) -> object:
    return obj.__dict__.get("x")  # unresolved


def frame() -> object:
    return vars().get("x")  # unresolved
"#;

const SHADOWED: &str = r#"from pkg.model import Field


def vars(obj: object) -> object:
    return obj


def shadowed(field: Field) -> object:
    return vars(field).get("x")  # unresolved
"#;

/// `_replace` copies the instance and `_make` builds one of the class it is called on (also on
/// an instance), so a method called on their result is the class's; `_asdict()` is a new dict,
/// `_fields` a tuple, `_field_defaults` a dict. A subclass defining `_replace` again runs its
/// definition, on an instance typed with the base too.
const NAMED_TUPLES: &str = r#"from pkg.model import Base, Context, Field, Renamed


def replaces(context: Context) -> Context:
    return context._replace(module="m")


def as_dict(context: Context) -> object:
    return context._asdict().get("module")


def fields(context: Context) -> object:
    return context._fields.index("module"), context._field_defaults.get("module")


def made(context: Context) -> object:
    return Context._make(["m"])._asdict(), context._make(["m"])._replace(module="n")


def copy_is_a_context(context: Context, field: Field) -> None:
    context._replace(module="m").require(field)  # write


def made_is_a_context(field: Field) -> None:
    Context._make(["m"]).require(field)  # write


def overridden(base: Base) -> Base:
    return base._replace(module="m")  # write


def subclass(renamed: Renamed) -> Base:
    return renamed._replace(module="m")  # write
"#;

/// `ContextVar[T].get()` is a `T`, with `get(default)` also the default's class, and `set`
/// returns a `Token` whose `var` is the variable; a future's `result()` is what the function it
/// runs returns. A variable without a type argument, a default of no known class, and a default
/// a `*` argument may pass give no class.
const HELD: &str = r#"import asyncio

from pkg.model import MEMO, PLAIN, POOL, UNTYPED, Field, Memo, Other, make_memo


def bumps(field: Field) -> None:
    if (memo := MEMO.get()) is not None:
        memo.bump(field)  # write


def bumps_plain(field: Field) -> None:
    PLAIN.get().bump(field)  # write


def bumps_default(field: Field) -> None:
    PLAIN.get(Other()).bump(field)  # write


def tokens(field: Field) -> None:
    token = PLAIN.set(Memo())
    token.var.reset(token)


def a_future(field: Field) -> None:
    POOL.submit(make_memo).result().bump(field)  # write


async def an_asyncio_future(field: Field) -> None:
    future = asyncio.get_running_loop().run_in_executor(None, make_memo)
    await future
    future.result().bump(field)  # write


def untyped(field: Field) -> None:
    UNTYPED.get().bump(field)  # unresolved


def unknown_default(field: Field, default) -> None:
    PLAIN.get(default).bump(field)  # unresolved


def starred_default(field: Field, defaults: tuple[Other, ...]) -> None:
    PLAIN.get(*defaults).bump(field)  # unresolved
"#;

/// `(path, line, kind)` of every `EFF201` report: `write` for a write of `Field.required`,
/// `unresolved` for a call heki cannot resolve.
fn reported(p: &Project) -> Vec<(String, u64, &'static str)> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<(String, u64, &'static str)> = diags
        .iter()
        .filter(|d| d["rule_code"] == "EFF201")
        .map(|d| {
            let message = d["message"].as_str().unwrap();
            let kind = if message.contains("a call heki cannot resolve") {
                "unresolved"
            } else if message.contains("writes `Field.required`") {
                "write"
            } else {
                panic!("{message}")
            };
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                kind,
            )
        })
        .collect();
    out.sort();
    out
}

/// The `(path, line, kind)` of every line of `files` ending in `# write` or `# unresolved`.
fn marked(files: &[(&str, &str)]) -> Vec<(String, u64, &'static str)> {
    let mut out = Vec::new();
    for (path, text) in files {
        for (i, line) in text.lines().enumerate() {
            let line = line.trim_end();
            for kind in ["write", "unresolved"] {
                if line.ends_with(&format!("# {kind}")) {
                    out.push(((*path).to_owned(), i as u64 + 1, kind));
                }
            }
        }
    }
    out.sort();
    out
}

fn check_marks(name: &str, files: &[(&str, &str)]) {
    let p = Project::new(name);
    p.write("pyproject.toml", CONFIG);
    p.write("pkg/__init__.py", "");
    p.write("pkg/model.py", MODEL);
    p.write("pkg/passes/__init__.py", "");
    for (path, text) in files {
        p.write(path, text);
    }
    assert_eq!(reported(&p), marked(files));
}

#[test]
fn an_instance_namespace_is_a_dict_where_reading_it_runs_no_code() {
    check_marks(
        "namespaces",
        &[
            ("pkg/passes/namespaces.py", NAMESPACES),
            ("pkg/passes/shadowed.py", SHADOWED),
        ],
    );
}

#[test]
fn the_methods_a_named_tuple_class_gets() {
    check_marks(
        "named_tuples",
        &[("pkg/passes/named_tuples.py", NAMED_TUPLES)],
    );
}

#[test]
fn the_value_a_context_variable_or_a_future_holds() {
    check_marks("held", &[("pkg/passes/held.py", HELD)]);
}
