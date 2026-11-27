//! `EFF114` (`recursion-error-as-value`) end to end: an `except` clause that takes the
//! `RecursionError` of a call cycle its `try` body reaches and goes on without raising (a method
//! cycle, a generator expression, a cycle across modules, a helper above the cycle, a `with` body,
//! an inner `try`, an awaited coroutine, a generator iterated there or in a helper, a function
//! `map` calls, a module body), the clause shapes that take it (bare, `BaseException`, a tuple, a
//! class imported from `builtins`), and the shapes left silent: a clause taking no
//! `RecursionError`, one after a clause that re-raises it or whose classes heki cannot tell (a
//! tuple, a name from a dependency, the standard library, or a star import), a clause raising on a
//! path or exiting, a CLI boundary, a body reaching no cycle (or one only through a worker thread,
//! a coroutine only created, or a generator only made), `except*`, a `finally` body on the way, and
//! a module whose `Exception` is its own class; the opt-in selection, and `heki rule`. Cycles
//! are closed only by calls heki establishes: a cycle through a protocol member found by name is
//! none, while the way down to a cycle may go through one.

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

/// `(path, line, qualname)` of every `EFF114` diagnostic.
fn found(diags: &[Value]) -> Vec<(String, u64, String)> {
    diags
        .iter()
        .filter(|d| d["rule_code"] == "EFF114")
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["qualname"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn d(path: &str, line: u64, qualname: &str) -> (String, u64, String) {
    (path.into(), line, qualname.into())
}

fn at<'v>(diags: &'v [Value], path: &str, line: u64) -> &'v Value {
    diags
        .iter()
        .find(|d| d["rule_code"] == "EFF114" && d["path"] == path && d["line"] == line)
        .unwrap_or_else(|| panic!("no EFF114 at {path}:{line}: {diags:#?}"))
}

const SELECT: &[&str] = &["--extend-select", "EFF114"];

/// The shapes of the reported recursion (`is_circular`, `nesting`) and of the code left silent
/// next to them: `RecursionError` re-raised first, a body reaching no cycle, a CLI boundary.
const WALK: &str = r#"from __future__ import annotations

import json
import sys
from pathlib import Path


class RefWalker:
    def __init__(self, graph: dict[str, list[str]]) -> None:
        self.graph = graph
        self._circular: dict[str, bool] = {}

    def is_circular(self, ref: str) -> bool:
        if ref in self._circular:
            return self._circular[ref]
        try:
            result = self._reaches(ref, ref, set())
        except Exception:
            result = True
        self._circular[ref] = result
        return result

    def is_circular_strict(self, ref: str) -> bool:
        try:
            return self._reaches(ref, ref, set())
        except RecursionError:
            raise
        except Exception:
            return True

    def _reaches(self, target: str, ref: str, seen: set[str]) -> bool:
        for following in self.graph.get(ref, []):
            if following == target:
                return True
            if following not in seen:
                seen.add(following)
                if self._walk(target, following, seen):
                    return True
        return False

    def _walk(self, target: str, ref: str, seen: set[str]) -> bool:
        return self._reaches(target, ref, seen)


def nesting(tree: dict[str, object]) -> int | None:
    try:
        return 1 + max((nesting(value) or 0 for value in tree.values() if isinstance(value, dict)), default=0)
    except RecursionError:
        return None


def load(path: str) -> object:
    try:
        return json.loads(Path(path).read_text(encoding="utf-8"))
    except Exception:
        return None


def main(graph: dict[str, list[str]], ref: str) -> int:
    try:
        circular = RefWalker(graph)._reaches(ref, ref, set())
    except Exception as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print("circular" if circular else "not circular")
    return 0
"#;

/// One recursion (`depth`, through a generator expression) and the clauses around calls of it.
const SHAPES: &str = r#"import asyncio
import logging
import sys
import threading
import traceback
from builtins import RuntimeError as Failure

logger = logging.getLogger(__name__)
LOCK = threading.Lock()
ERRORS = (KeyError, ValueError)


def depth(tree: dict) -> int:
    return 1 + max((depth(v) for v in tree.values() if isinstance(v, dict)), default=0)


def depth_of(tree: dict) -> int:
    return depth(tree)


def bare(tree: dict) -> int:
    try:
        return depth(tree)
    except:
        return -1


def base(tree: dict) -> int:
    try:
        return depth(tree)
    except BaseException:
        return -1


def tupled(tree: dict) -> int:
    try:
        return depth(tree)
    except (ValueError, Failure):
        return -1


def below(tree: dict) -> int:
    try:
        return depth_of(tree)
    except Exception:
        return -1


def narrow(tree: dict) -> int | None:
    try:
        return depth(tree)
    except (KeyError, ValueError):
        return None


def unknown_first(tree: dict) -> int | None:
    try:
        return depth(tree)
    except ERRORS:
        return None
    except Exception:
        return None


def reraised_on_a_path(tree: dict) -> int | None:
    try:
        return depth(tree)
    except Exception as error:
        if isinstance(error, RecursionError):
            raise
        return None


def exits(tree: dict) -> int:
    try:
        return depth(tree)
    except Exception:
        sys.exit(2)


def cleanup() -> None:
    pass


def logged_exception(tree: dict) -> int:
    try:
        print(depth(tree))
    except Exception:
        logger.exception("cannot measure the tree")
        return 2
    return 0


def logged_with_exc_info(tree: dict) -> int:
    try:
        print(depth(tree))
    except Exception:
        logger.error("cannot measure the tree", exc_info=True)
        return 2
    return 0


def logged_error(tree: dict) -> int:
    try:
        print(depth(tree))
    except Exception as error:
        logging.error("cannot measure the tree: %s", error)
        return 2
    return 0


def logged_text(tree: dict) -> int:
    try:
        print(depth(tree))
    except Exception as error:
        logger.error(f"cannot measure the tree: {error}")
        return 2
    return 0


def logged_field(tree: dict) -> int:
    try:
        print(depth(tree))
    except Exception as error:
        logger.error(f"{error!r}")
        return 2
    return 0


def written(tree: dict) -> int:
    try:
        print(depth(tree))
    except:
        sys.stderr.write("cannot measure the tree\n")
        return 1
    return 0


def traced(tree: dict) -> int:
    try:
        print(depth(tree))
    except Exception:
        traceback.print_exc()
        return 1
    return 0


def logged_without_the_error(tree: dict) -> int:
    try:
        print(depth(tree))
    except Exception:
        logger.warning("cannot measure the tree")
        return 2
    return 0


def printed_then_none(tree: dict) -> int | None:
    try:
        return depth(tree)
    except Exception as error:
        print(error, file=sys.stderr)
        return None


def status_without_report(tree: dict) -> int:
    try:
        print(depth(tree))
    except Exception:
        cleanup()
        print("failed")
        return 1
    return 0


def locked(tree: dict) -> int:
    try:
        with LOCK:
            return depth(tree)
    except Exception:
        return -1


def inner_try(tree: dict) -> int:
    try:
        try:
            return depth(tree)
        except KeyError:
            return 0
    except Exception:
        return -1


def inner_takes(tree: dict) -> int:
    try:
        try:
            return depth(tree)
        except RecursionError:
            raise
    except Exception:
        return -1


def through_finally(tree: dict) -> int:
    try:
        try:
            return depth(tree)
        finally:
            cleanup()
    except Exception:
        return -1


async def offloaded(tree: dict) -> int:
    try:
        return await asyncio.to_thread(depth, tree)
    except Exception:
        return -1


async def walk_async(tree: dict) -> int:
    return 1 + max([await walk_async(v) for v in tree.values()], default=0)


async def scheduled(tree: dict) -> None:
    try:
        asyncio.ensure_future(walk_async(tree))
    except Exception:
        return None


async def awaited(tree: dict) -> int:
    try:
        return await walk_async(tree)
    except Exception:
        return -1


def walk_gen(tree: dict):
    yield tree
    for v in tree.values():
        if isinstance(v, dict):
            yield from walk_gen(v)


def counted(tree: dict) -> int:
    count = 0
    try:
        for _ in walk_gen(tree):
            count += 1
    except RecursionError:
        return -1
    return count


def handed_back(tree: dict):
    try:
        return walk_gen(tree)
    except RecursionError:
        return None


def mapped(tree: dict) -> list[int]:
    try:
        return list(map(depth, tree.values()))
    except RecursionError:
        return []


class Node:
    def __init__(self, children: list["Node"]) -> None:
        self.children = children

    def size(self) -> int:
        try:
            return 1 + sum([child.size() for child in self.children])
        except RecursionError:
            return 0


def starred(tree: dict) -> None:
    try:
        depth(tree)
    except* Exception:
        pass


TREE: dict = {}
try:
    DEPTH = depth(TREE)
except RecursionError:
    DEPTH = -1


def parsed(text: str) -> int:
    try:
        return int(text)
    except Exception:
        return 0


def given(tree: dict, errors: tuple) -> int | None:
    try:
        return depth(tree)
    except errors:
        return None


def both(tree: dict) -> int | None:
    try:
        depth(tree)
    except ERRORS:
        pass
    try:
        return depth(tree)
    except Exception:
        return None


def status_after_a_library_call(tree: dict) -> int:
    try:
        print(depth(tree))
    except Exception:
        threading.get_ident()
        return 1
    return 0


def finally_tested(tree: dict, verbose: bool) -> int:
    try:
        return depth(tree)
    except KeyError:
        return 0
    finally:
        if verbose:
            print("done")


def _fail(error: Exception) -> None:
    if isinstance(error, KeyError):
        raise ValueError("missing") from error
    raise RuntimeError("failed") from error


def failing(tree: dict) -> int:
    try:
        return depth(tree)
    except Exception as error:
        _fail(error)


def error_types() -> tuple:
    return (KeyError,)


def computed_classes(tree: dict) -> int | None:
    try:
        return depth(tree)
    except error_types():
        return None


def cancelled(tree: dict) -> int | None:
    try:
        return depth(tree)
    except asyncio.CancelledError:
        return None


def helper_first(tree: dict) -> int:
    try:
        depth_of(tree)
        return depth(tree)
    except Exception:
        return -1


def below_again(tree: dict) -> int:
    try:
        return depth_of(tree)
    except Exception:
        return -1


def walks(tree: dict):
    return walk_gen(tree)


def iterated_result(tree: dict) -> int:
    try:
        return len([node for node in walks(tree)])
    except RecursionError:
        return -1


def walked_length(tree: dict) -> int:
    return len([node for node in walks(tree)])


def measured_walk(tree: dict) -> int:
    try:
        return walked_length(tree)
    except RecursionError:
        return -1


def logged_by_a_logger_of_no_known_class(tree: dict, log) -> int:
    try:
        print(depth(tree))
    except Exception as error:
        log.error("cannot measure the tree: %s", error)
        return 2
    return 0
"#;

/// A cycle across two modules, through a generator expression in the second.
const CYCLE_A: &str = r#"from pkg import cycle_b


def visit(node: dict) -> int:
    return cycle_b.descend(node)


def measure(node: dict) -> int:
    try:
        return visit(node)
    except Exception:
        return 0
"#;
const CYCLE_B: &str = r#"from pkg import cycle_a


def descend(node: dict) -> int:
    return sum(cycle_a.visit(child) for child in node.values())
"#;

/// A module whose `Exception` is a class of the project.
const ERRORS: &str = r#"class Exception(ValueError):
    pass


Failure = Exception
Fatal = RuntimeError
"#;
const REBOUND: &str = r#"from pkg.errors import Exception
from pkg.shapes import depth


def guarded(tree: dict) -> int:
    try:
        return depth(tree)
    except Exception:
        return -1
"#;

/// Clauses before an `except Exception`: names from a dependency (`library`, not installed) and
/// the standard library (`threading.ThreadError` is `RuntimeError`), which may hold
/// `RecursionError`, and a project class and a builtin, which cannot, also through a module alias
/// (`errors.Failure`); and a module alias of `RuntimeError` (`errors.Fatal`), which takes it.
const EARLIER: &str = r#"import threading

import library
from library import ERRORS
from pkg import errors
from pkg.errors import Exception as ProjectError
from pkg.shapes import depth


def imported_tuple_first(tree: dict) -> int | None:
    try:
        return depth(tree)
    except ERRORS:
        raise
    except Exception:
        return None


def library_class_first(tree: dict) -> int | None:
    try:
        return depth(tree)
    except (KeyError, library.Error):
        raise
    except Exception:
        return None


def standard_library_class_first(tree: dict) -> int | None:
    try:
        return depth(tree)
    except threading.ThreadError:
        raise
    except Exception:
        return None


def project_class_first(tree: dict) -> int | None:
    try:
        return depth(tree)
    except ProjectError:
        raise
    except Exception:
        return None


def builtin_class_first(tree: dict) -> int | None:
    try:
        return depth(tree)
    except KeyError:
        raise
    except Exception:
        return None


def project_alias_first(tree: dict) -> int | None:
    try:
        return depth(tree)
    except errors.Failure:
        raise
    except Exception:
        return None


def builtin_alias(tree: dict) -> int | None:
    try:
        return depth(tree)
    except errors.Fatal:
        return None
"#;

/// A name only a star import from a dependency may bind.
const STARRED: &str = r#"from library import *
from pkg.shapes import depth


def star_imported_first(tree: dict) -> int | None:
    try:
        return depth(tree)
    except ERRORS:
        raise
    except Exception:
        return None
"#;

fn project(name: &str) -> Project {
    let p = Project::new(name);
    p.write("pkg/__init__.py", "")
        .write("pkg/walk.py", WALK)
        .write("pkg/shapes.py", SHAPES)
        .write("pkg/cycle_a.py", CYCLE_A)
        .write("pkg/cycle_b.py", CYCLE_B)
        .write("pkg/errors.py", ERRORS)
        .write("pkg/rebound.py", REBOUND)
        .write("pkg/earlier.py", EARLIER)
        .write("pkg/starred.py", STARRED);
    p
}

#[test]
fn clauses_turning_a_recursion_error_into_a_value_are_reported() {
    let p = project("eff114_reports");
    let diags = diagnostics(&p, SELECT);
    assert_eq!(
        found(&diags),
        vec![
            d("pkg/cycle_a.py", 11, "measure"),
            d("pkg/earlier.py", 42, "project_class_first"),
            d("pkg/earlier.py", 51, "builtin_class_first"),
            d("pkg/earlier.py", 60, "project_alias_first"),
            d("pkg/earlier.py", 67, "builtin_alias"),
            d("pkg/shapes.py", 24, "bare"),
            d("pkg/shapes.py", 31, "base"),
            d("pkg/shapes.py", 38, "tupled"),
            d("pkg/shapes.py", 45, "below"),
            d("pkg/shapes.py", 151, "logged_without_the_error"),
            d("pkg/shapes.py", 160, "printed_then_none"),
            d("pkg/shapes.py", 168, "status_without_report"),
            d("pkg/shapes.py", 179, "locked"),
            d("pkg/shapes.py", 189, "inner_try"),
            d("pkg/shapes.py", 234, "awaited"),
            d("pkg/shapes.py", 250, "counted"),
            d("pkg/shapes.py", 265, "mapped"),
            d("pkg/shapes.py", 276, "Node.size"),
            d("pkg/shapes.py", 290, "<module>"),
            d("pkg/shapes.py", 315, "both"),
            d("pkg/shapes.py", 322, "status_after_a_library_call"),
            d("pkg/shapes.py", 373, "helper_first"),
            d("pkg/shapes.py", 380, "below_again"),
            d("pkg/shapes.py", 391, "iterated_result"),
            d("pkg/shapes.py", 402, "measured_walk"),
            d("pkg/walk.py", 18, "RefWalker.is_circular"),
            d("pkg/walk.py", 48, "nesting"),
        ]
    );
}

/// A class heki knows only by name (from a dependency, the standard library, or a star import)
/// may be `RecursionError` or a class above it, so a clause of one that re-raises leaves the
/// later `except Exception` unreported.
#[test]
fn a_clause_after_one_of_classes_heki_knows_only_by_name_is_not_reported() {
    let p = project("eff114_known_by_name");
    let silent = [
        "imported_tuple_first",
        "library_class_first",
        "standard_library_class_first",
        "star_imported_first",
    ];
    let reported: Vec<String> = found(&diagnostics(&p, SELECT))
        .into_iter()
        .map(|(_, _, qualname)| qualname)
        .collect();
    for qualname in silent {
        assert!(!reported.iter().any(|q| q == qualname), "{qualname}");
    }
    assert!(reported.iter().any(|q| q == "project_class_first"));
}

#[test]
fn a_report_names_the_clause_the_recursion_and_the_call_in_the_try_body() {
    let p = project("eff114_messages");
    let diags = diagnostics(&p, SELECT);
    let circular = at(&diags, "pkg/walk.py", 18);
    assert_eq!(
        circular["message"],
        "`except Exception` takes the `RecursionError` the recursion of `RefWalker._reaches` can raise, and no path through it raises, so `RefWalker.is_circular` goes on with a value instead of the error"
    );
    assert_eq!(circular["target"], "Exception->RefWalker._reaches");
    assert_eq!(circular["severity"], "error");
    assert_eq!(circular["certainty"], "heuristic");
    assert_eq!(circular["column"], 9);
    assert_eq!(circular["related"][0]["line"], 17);
    assert_eq!(
        circular["related"][0]["message"],
        "`RefWalker.is_circular` runs `RefWalker._reaches` in the `try` body"
    );
    assert!(
        circular["help"]
            .as_str()
            .unwrap()
            .contains("except RecursionError: raise")
    );
    // The function's own recursion, through the generator expression it runs.
    let nesting = at(&diags, "pkg/walk.py", 48);
    assert!(nesting["message"].as_str().unwrap().starts_with(
        "`except RecursionError` takes the `RecursionError` the recursion of `nesting` can raise"
    ));
    assert_eq!(
        nesting["related"][0]["message"],
        "`nesting` runs a generator expression in the `try` body"
    );
    // A helper above the cycle: the calls down to it are related locations too.
    let below = at(&diags, "pkg/shapes.py", 45);
    assert!(
        below["message"]
            .as_str()
            .unwrap()
            .contains("the `RecursionError` the recursion of `depth` below `depth_of` can raise")
    );
    assert_eq!(below["related"][1]["line"], 18);
    assert_eq!(below["related"][1]["message"], "`depth_of` calls `depth`");
    // A call of a function on the cycle is the witness over an earlier call above it.
    assert_eq!(
        at(&diags, "pkg/shapes.py", 373)["target"],
        "Exception->depth"
    );
    // A cycle reached through what iterating a returned generator runs.
    assert!(
        at(&diags, "pkg/shapes.py", 391)["message"]
            .as_str()
            .unwrap()
            .contains("the `RecursionError` a recursion below `walks` can raise")
    );
    // A helper iterating it, past `walks` decided by the report before.
    assert!(
        at(&diags, "pkg/shapes.py", 402)["message"]
            .as_str()
            .unwrap()
            .contains("the `RecursionError` a recursion below `walked_length` can raise")
    );
    assert!(
        at(&diags, "pkg/shapes.py", 24)["message"]
            .as_str()
            .unwrap()
            .starts_with("the bare `except` takes")
    );
    assert_eq!(at(&diags, "pkg/shapes.py", 24)["target"], "bare->depth");
    // Another module's function on the cycle.
    assert!(
        at(&diags, "pkg/cycle_a.py", 11)["message"]
            .as_str()
            .unwrap()
            .contains("the recursion of `visit`")
    );
}

#[test]
fn the_rule_is_opt_in() {
    let p = project("eff114_opt_in");
    assert!(found(&diagnostics(&p, &[])).is_empty());
    assert!(found(&diagnostics(&p, &["--select", "EFF"])).is_empty());
    assert!(!found(&diagnostics(&p, &["--select", "recursion-error-as-value"])).is_empty());
}

#[test]
fn heki_rule_explains_it() {
    let p = Project::new("eff114_rule");
    let (code, stdout, _) = p.heki(&["rule", "EFF114"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("recursion-error-as-value"));
    assert!(stdout.contains("Completes normally"));
    assert!(stdout.contains("experimental"));
}

const STATUS: &str = r#"import sys
import traceback

import codes
from codes import FAILED as IMPORTED_ERROR

PYTHON_ERROR = 4
REBOUND_ERROR = 5
OPEN_ERROR = int(sys.argv[0])
TEXT_ERROR = "4"
REBOUND_ERROR = 6


def visit(children):
    for group in children:
        visit(group)


def named_status(children):
    try:
        visit(children)
        return 0
    except Exception:
        traceback.print_exc()
        return PYTHON_ERROR


def rebound_status(children):
    try:
        visit(children)
        return 0
    except Exception:
        traceback.print_exc()
        return REBOUND_ERROR


def open_status(children):
    try:
        visit(children)
        return 0
    except Exception:
        traceback.print_exc()
        return OPEN_ERROR


def text_status(children):
    try:
        visit(children)
        return 0
    except Exception:
        traceback.print_exc()
        return TEXT_ERROR


def attribute_status(children):
    try:
        visit(children)
        return 0
    except Exception:
        traceback.print_exc()
        return codes.FAILED


class Config:
    status = 4


def parameter_status(children, config):
    try:
        visit(children)
        return 0
    except Exception:
        traceback.print_exc()
        return config.status


def instance_status(children):
    config = Config()
    try:
        visit(children)
        return 0
    except Exception:
        traceback.print_exc()
        return config.status


def imported_status(children):
    try:
        visit(children)
        return 0
    except Exception:
        traceback.print_exc()
        return IMPORTED_ERROR


def fallback_value(children):
    try:
        visit(children)
    except RuntimeError:
        return []
    return children
"#;

/// A name bound once to an integer is an exit status like the literal; a name bound twice or to
/// a value heki cannot read, and a returned list, are not.
#[test]
fn a_module_constant_status_is_a_cli_boundary() {
    let p = Project::new("eff114_status");
    p.write("codes.py", "FAILED = 3\n")
        .write("status.py", STATUS);
    let reported: Vec<String> = found(&diagnostics(&p, SELECT))
        .into_iter()
        .map(|(_, _, qualname)| qualname)
        .collect();
    assert_eq!(
        reported,
        [
            "rebound_status",
            "open_status",
            "text_status",
            "parameter_status",
            "instance_status",
            "fallback_value"
        ]
    );
}

/// Cycles over the calls heki establishes: a getter reading the same member of an object typed
/// with a protocol closes no cycle, while direct and mutual recursion, a helper calling into one,
/// a method of a known class with an override off the cycle, an override of an abstract method
/// called in a comprehension, a cached function, and one defined twice close one, and the way
/// down to a cycle may go through a protocol call. Every report is heuristic.
const CYCLES: &str = r#"import functools
import os
from abc import ABC, abstractmethod
from typing import Protocol

FAST = bool(os.environ.get("FAST"))


def walk(node):
    total = 1
    for child in node.children:
        total += walk(child)
    return total


def measure(node):
    try:
        return walk(node)
    except Exception:
        return 0


class Backend(Protocol):
    @property
    def bits(self) -> int: ...


class Wrapper:
    def __init__(self, backend: Backend) -> None:
        self._backend = backend

    @property
    def bits(self) -> int:
        return self._backend.bits


def window(w: Wrapper) -> int:
    try:
        return w.bits
    except Exception:
        return 15


class Runner(Protocol):
    def run(self, node) -> int: ...


class Walker:
    def run(self, node) -> int:
        return walk(node)


def through_protocol(r: Runner, node):
    try:
        return r.run(node)
    except Exception:
        return 0


def even(n):
    if n == 0:
        return True
    return odd(n - 1)


def odd(n):
    if n == 0:
        return False
    return even(n - 1)


def parity(n):
    try:
        return even(n)
    except Exception:
        return None


def outer(node):
    return walk(node) + 1


def through_helper(node):
    try:
        return outer(node)
    except Exception:
        return 0


class Visitor:
    def visit(self, node):
        for child in node.children:
            self.visit(child)


class Printer(Visitor):
    def visit(self, node):
        return None


def visited(v: Visitor, node):
    try:
        v.visit(node)
    except Exception:
        return False
    return True


class Shape(ABC):
    @abstractmethod
    def area(self) -> int: ...


class Group(Shape):
    def __init__(self, parts: list[Shape]) -> None:
        self.parts = parts

    def area(self) -> int:
        return sum([part.area() for part in self.parts])


def group_area(group: Group) -> int:
    try:
        return group.area()
    except Exception:
        return 0


@functools.lru_cache(maxsize=None)
def cached_depth(node) -> int:
    return 1 + max([cached_depth(c) for c in node], default=0)


def cached(node):
    try:
        return cached_depth(node)
    except Exception:
        return 0


if FAST:
    def fold(x):
        return fold(x[1:]) if x else 0
else:
    def fold(x):
        return fold(x[:-1]) if x else 0


def folded(x):
    try:
        return fold(x)
    except Exception:
        return 0
"#;

#[test]
fn cycles_are_closed_by_calls_heki_establishes_and_reached_through_any_call() {
    let p = Project::new("eff114_established_cycles");
    p.write("pkg/__init__.py", "")
        .write("pkg/cycles.py", CYCLES);
    let diags = diagnostics(&p, SELECT);
    let path = "pkg/cycles.py";
    assert_eq!(
        found(&diags),
        vec![
            d(path, 19, "measure"),
            d(path, 56, "through_protocol"),
            d(path, 75, "parity"),
            d(path, 86, "through_helper"),
            d(path, 104, "visited"),
            d(path, 125, "group_area"),
            d(path, 137, "cached"),
            d(path, 152, "folded"),
        ]
    );
    for report in diags.iter().filter(|r| r["rule_code"] == "EFF114") {
        assert_eq!(report["certainty"], "heuristic", "{report:#?}");
    }
}
