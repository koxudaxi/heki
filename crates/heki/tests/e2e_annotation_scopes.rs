//! Where an annotation gives a receiver its class, end to end through the real binary, judged by
//! `MUT205` (a write on an object of unknown class under a write owner that reports them): in the
//! `finally` body of a `try` whose paths leave without running it again, for the builtin
//! exception classes, for the standard library calls the catalog types, and for an annotation
//! naming an import of the function itself.
//!
//! Every line that must be reported ends in `# unresolved` (`MUT205`) or `# write` (`MUT201`, a
//! write of a guarded attribute on a class heki knows); every other line must not be.

mod common;

use common::Project;
use serde_json::Value;

const CONFIG: &str = r#"[project]
name = "t"

[tool.heki]
target-version = "py311"

[tool.heki.lint]
select = ["MUT201", "MUT205"]

[[tool.heki.write-owners]]
name = "cache"
targets = ["pkg.model.Node._cache"]
allowed-writers = ["pkg.store:*"]
unlisted-dict-keys = "report"
unresolved = "report"
"#;

const MODEL: &str = "class Node:\n    pass\n";

/// The write `obj.__dict__["_mark"] = 1` is judged on the class of `obj`: a typed receiver is no
/// unresolved write.
const FINALLY: &str = r#"class Plain:
    def restore(self) -> int:
        try:
            return 1
        finally:
            self.__dict__["_mark"] = 1


def work() -> None:
    pass


def body_cannot_raise(plain: Plain) -> None:
    try:
        pass
    finally:
        plain.__dict__["_mark"] = 1


def body_returns(plain: Plain) -> int:
    try:
        return 1
    finally:
        plain.__dict__["_mark"] = 1


def body_never_leaves(plain: Plain) -> None:
    try:
        while True:
            pass
    finally:
        plain.__dict__["_mark"] = 1


def body_raises(plain: Plain) -> None:
    try:
        work()
    except ValueError:
        pass
    finally:
        plain.__dict__["_mark"] = 1


def nested(plain: Plain) -> int:
    try:
        try:
            return 1
        finally:
            plain.__dict__["_inner"] = 1
    finally:
        plain.__dict__["_outer"] = 1


def not_annotated(plain) -> int:
    try:
        return 1
    finally:
        plain.__dict__["_mark"] = 1  # unresolved


def rebound_in_the_block(plain: Plain, other) -> int:
    try:
        return 1
    finally:
        plain = other
        plain.__dict__["_mark"] = 1  # unresolved


def after_the_block(plain: Plain) -> None:
    try:
        work()
    finally:
        pass
    plain.__dict__["_mark"] = 1
"#;

/// A builtin exception class is a known class: its instances have no member a project contract
/// names. An annotation of `Any`, or none, gives none.
const EXCEPTIONS: &str = r#"from typing import Any

import types


def base(error: BaseException) -> None:
    error.__dict__["__notes__"] = []


def value_error(error: ValueError) -> None:
    error.__dict__["__notes__"] = []


def os_error(error: "OSError") -> None:
    error.__dict__["__notes__"] = []


def built() -> None:
    error = KeyError("k")
    error.__dict__["__notes__"] = []


def a_module(module: types.ModuleType) -> None:
    module.__dict__["__builtins__"] = {}


def any_class(error: Any) -> None:
    error.__dict__["__notes__"] = []  # unresolved


def not_annotated(error) -> None:
    error.__dict__["__notes__"] = []  # unresolved
"#;

/// A project class named like a builtin exception is that class, not the builtin: its subclass
/// of `Node` is judged on the key it writes.
const SHADOWED_EXCEPTION: &str = r#"from pkg.model import Node


class ValueError(Node):
    pass


def project_class(error: ValueError) -> None:
    error.__dict__["_cache"] = 1  # write
"#;

/// `importlib.util.module_from_spec` gives no class (a custom loader's `create_module` may give
/// any object), an annotation of the result does, and an annotation of a local names the imports
/// of its own function, which shadow the module's.
const STDLIB: &str = r#"import importlib.abc
import importlib.machinery
import importlib.util
import types

import vendor
from pkg.model import Node


def made() -> None:
    module = types.ModuleType("made")
    module.__dict__["__builtins__"] = {}


def from_spec(spec: importlib.machinery.ModuleSpec) -> None:
    module = importlib.util.module_from_spec(spec)
    module.__dict__["__builtins__"] = {}  # unresolved


def from_spec_through_a_module_alias(spec: importlib.machinery.ModuleSpec) -> None:
    from importlib import util

    module = util.module_from_spec(spec)
    module.__dict__["__builtins__"] = {}  # unresolved


def from_spec_annotated(spec: importlib.machinery.ModuleSpec) -> None:
    module: types.ModuleType = importlib.util.module_from_spec(spec)
    module.__dict__["__builtins__"] = {}


def from_spec_annotated_through_a_local_import(spec: importlib.machinery.ModuleSpec) -> None:
    import types

    module: types.ModuleType = importlib.util.module_from_spec(spec)
    module.__dict__["__builtins__"] = {}


class ProjectLoader(importlib.abc.Loader):
    def create_module(self, spec: importlib.machinery.ModuleSpec) -> Node:
        return Node()


def from_a_custom_loader() -> None:
    spec = importlib.util.spec_from_loader("made", ProjectLoader())
    module = importlib.util.module_from_spec(spec)
    module.__dict__["__builtins__"] = {}  # unresolved


def from_unknown_call() -> None:
    module = vendor.load()
    module.__dict__["__builtins__"] = {}  # unresolved


def local_import(spec: importlib.machinery.ModuleSpec) -> None:
    import types as local_types

    module: local_types.ModuleType = vendor.load(spec)
    module.__dict__["__builtins__"] = {}


def local_import_of_a_name(spec: importlib.machinery.ModuleSpec) -> None:
    from types import ModuleType as Mod

    module: Mod = vendor.load(spec)
    module.__dict__["__builtins__"] = {}


def local_import_in_a_string(spec: importlib.machinery.ModuleSpec) -> None:
    import types as local_types

    module: "local_types.ModuleType | None" = vendor.load(spec)
    module.__dict__["__builtins__"] = {}


def not_imported(spec: importlib.machinery.ModuleSpec) -> None:
    module: local_types.ModuleType = vendor.load(spec)
    module.__dict__["__builtins__"] = {}  # unresolved


def imported_in_another_function(spec: importlib.machinery.ModuleSpec) -> None:
    module: other_types.ModuleType = vendor.load(spec)
    module.__dict__["__builtins__"] = {}  # unresolved


def imports_in_another_function() -> None:
    import types as other_types

    other_types.ModuleType("x")
"#;

/// `(path, line, kind)` of every `MUT201` (`write`) and `MUT205` (`unresolved`) report.
fn reported(p: &Project) -> Vec<(String, u64, &'static str)> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<(String, u64, &'static str)> = diags
        .iter()
        .filter_map(|d| {
            let kind = match d["rule_code"].as_str().unwrap() {
                "MUT201" => "write",
                "MUT205" => {
                    let message = d["message"].as_str().unwrap();
                    assert!(
                        message.contains("on an object of unknown class"),
                        "{message}"
                    );
                    "unresolved"
                }
                _ => return None,
            };
            Some((
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                kind,
            ))
        })
        .collect();
    out.sort();
    out
}

/// `(path, line, kind)` of every line of `files` ending in `# write` or `# unresolved`.
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
    p.write("pkg/store.py", "");
    for (path, text) in files {
        p.write(path, text);
    }
    assert_eq!(reported(&p), marked(files));
}

#[test]
fn an_annotation_holds_in_a_finally_body_no_path_enters_again() {
    check_marks("finally_bodies", &[("pkg/finally_bodies.py", FINALLY)]);
}

#[test]
fn builtin_exception_classes_are_known_classes() {
    check_marks(
        "exceptions",
        &[
            ("pkg/exceptions.py", EXCEPTIONS),
            ("pkg/shadowed.py", SHADOWED_EXCEPTION),
        ],
    );
}

#[test]
fn standard_library_results_and_function_local_imports_type_a_receiver() {
    check_marks("stdlib", &[("pkg/stdlib.py", STDLIB)]);
}

const BLOCKING_WORKER: &str =
    "import time\n\n\nclass Worker:\n    def work(self) -> None:\n        time.sleep(1)\n";
const QUIET_WORKER: &str = "class Worker:\n    def work(self) -> None:\n        return None\n";

/// An annotation reads a name the function binds once, by one import: the class of that import.
const ONE_LOCAL_IMPORT: &str = r#"async def run(value):
    from pkg.blocking import Worker

    worker: Worker = value
    worker.work()
"#;

/// A name the function binds more than once names no class: which binding the annotation reads
/// is not decided, so the blocking `Worker` is not the receiver's class.
const SEVERAL_BINDINGS: &str = r#"async def two_imports(value):
    from pkg.blocking import Worker
    from pkg.quiet import Worker

    worker: Worker = value
    worker.work()


async def one_import_per_branch(value, flag):
    if flag:
        from pkg.blocking import Worker
    else:
        from pkg.quiet import Worker

    worker: Worker = value
    worker.work()


async def an_import_and_an_assignment(value):
    from pkg.blocking import Worker

    Worker = value
    worker: Worker = value
    worker.work()
"#;

/// The lines of the `EFF101` reports of each file.
fn blocking_reports(name: &str, files: &[(&str, &str)]) -> Vec<(String, u64)> {
    let p = Project::new(name);
    p.write("pyproject.toml", "[project]\nname = \"t\"\n");
    p.write("pkg/__init__.py", "");
    p.write("pkg/blocking.py", BLOCKING_WORKER);
    p.write("pkg/quiet.py", QUIET_WORKER);
    for (path, text) in files {
        p.write(path, text);
    }
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<(String, u64)> = diags
        .iter()
        .filter(|d| d["rule_code"] == "EFF101")
        .map(|d| {
            assert_eq!(d["certainty"], "exact", "{d}");
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
            )
        })
        .collect();
    out.sort();
    out
}

#[test]
fn a_function_local_import_types_an_annotation_only_when_it_is_the_one_binding() {
    assert_eq!(
        blocking_reports("one_local_import", &[("pkg/one.py", ONE_LOCAL_IMPORT)]),
        [("pkg/one.py".to_owned(), 5)]
    );
    assert_eq!(
        blocking_reports("several_bindings", &[("pkg/several.py", SEVERAL_BINDINGS)]),
        []
    );
}

/// A name the module or the function binds is that binding, not the builtin of the same name:
/// an object heki does not know gives no class. `IOError` and `EnvironmentError` are no class
/// heki types.
const SHADOWED_BUILTIN: &str = r#"def make_error_class() -> type:
    return type("Error", (Exception,), {})


ValueError = make_error_class()


def shadowed_by_the_module(error: ValueError) -> None:
    error.__dict__["__notes__"] = []  # unresolved


def shadowed_by_the_function(value) -> None:
    KeyError = make_error_class()
    error: KeyError = value
    error.__dict__["__notes__"] = []  # unresolved


def io_error(error: IOError) -> None:
    error.__dict__["__notes__"] = []  # unresolved


def environment_error(error: EnvironmentError) -> None:
    error.__dict__["__notes__"] = []  # unresolved


def not_shadowed(error: TypeError) -> None:
    error.__dict__["__notes__"] = []
"#;

#[test]
fn a_binding_of_the_module_or_the_function_shadows_a_builtin_class() {
    check_marks("shadowed_builtin", &[("pkg/shadowed.py", SHADOWED_BUILTIN)]);
}

/// A project class named `Optional`, in `pkg/wrappers.py`, whose `work` blocks.
const WRAPPERS: &str = "import time\nfrom typing import Generic, TypeVar\n\nT = TypeVar(\"T\")\n\n\nclass Optional(Generic[T]):\n    def work(self) -> None:\n        time.sleep(1)\n";

/// An annotation unwraps `Optional[...]` only when the name reads `typing.Optional`: a project
/// class of that name, imported by the function or the module, is the receiver's class.
const WRAPPER_NAMES: &str = r#"from typing import Optional as Maybe

from pkg.quiet import Worker
from pkg.wrappers import Optional


async def imported_by_the_function(value):
    from pkg.wrappers import Optional

    worker: Optional[Worker] = value
    worker.work()


async def imported_by_the_module(value):
    worker: Optional[Worker] = value
    worker.work()


async def typing_under_another_name(value):
    from pkg.blocking import Worker

    worker: Maybe[Worker] = value
    worker.work()


async def typing_imported_by_the_function(value):
    from typing import Optional

    from pkg.quiet import Worker

    worker: Optional[Worker] = value
    worker.work()
"#;

#[test]
fn an_annotation_unwraps_a_wrapper_only_when_it_names_the_typing_object() {
    let p = "pkg/wrapper_names.py";
    assert_eq!(
        blocking_reports(
            "wrapper_names",
            &[("pkg/wrappers.py", WRAPPERS), (p, WRAPPER_NAMES)]
        ),
        [(p.to_owned(), 11), (p.to_owned(), 16), (p.to_owned(), 23)]
    );
}

/// The result of `Callable[..., T]` is read in the function's scope: a function-local import of
/// the name shadows the module's alias that may be `None`.
const CALLABLE_RESULT: &str = r#"from typing import Callable, Optional

from pkg.quiet import Worker

Maybe = Optional[Worker]


async def local_import_of_the_result(make):
    from pkg.blocking import Worker as Maybe

    factory: Callable[[], Maybe] = make
    factory().work()


async def the_module_alias(make):
    factory: Callable[[], Maybe] = make
    factory().work()
"#;

/// An import in a `finally` body, which is lowered once per way out of the `try`, is one binding.
const FINALLY_IMPORT: &str = r#"async def run(value, step):
    try:
        step()
    finally:
        from pkg.blocking import Worker

    worker: Worker = value
    worker.work()
"#;

/// A name the function binds itself, other than by one import, names no class, though the
/// module imports it.
const LOCAL_ASSIGNMENT: &str = r#"from pkg.blocking import Worker


async def rebound(value, other):
    Worker = other
    worker: Worker = value
    worker.work()


def outer(value, other):
    Worker = other

    async def rebound_nonlocal():
        nonlocal Worker
        Worker = other
        worker: Worker = value
        worker.work()

    return rebound_nonlocal


async def a_wrapper_name_bound_otherwise(value, wrap):
    Optional = wrap
    worker: Optional[Worker] = value
    worker.work()


async def not_rebound(value):
    worker: Worker = value
    worker.work()
"#;

#[test]
fn a_callable_result_reads_the_function_scope_first() {
    assert_eq!(
        blocking_reports("callable_result", &[("pkg/callable.py", CALLABLE_RESULT)]),
        [("pkg/callable.py".to_owned(), 12)]
    );
}

#[test]
fn an_import_in_a_finally_body_is_one_binding() {
    assert_eq!(
        blocking_reports(
            "finally_import",
            &[("pkg/finally_import.py", FINALLY_IMPORT)]
        ),
        [("pkg/finally_import.py".to_owned(), 8)]
    );
}

#[test]
fn a_local_binding_other_than_one_import_shadows_the_module_import() {
    assert_eq!(
        blocking_reports("local_assignment", &[("pkg/assigned.py", LOCAL_ASSIGNMENT)]),
        [("pkg/assigned.py".to_owned(), 30)]
    );
}
