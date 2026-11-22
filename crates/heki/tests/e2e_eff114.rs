//! `EFF114` (`recursion-error-as-value`) end to end: an `except` clause that takes the
//! `RecursionError` of a call cycle its `try` body reaches and goes on without raising (a method
//! cycle, a generator expression, a cycle across modules, a helper above the cycle, a `with` body,
//! an inner `try`, an awaited coroutine, a generator iterated there, a function `map` calls, a
//! module body), the clause shapes that take it (bare, `BaseException`, a tuple, a class imported
//! from `builtins`), and the shapes left silent: a clause taking no `RecursionError`, one after a
//! clause that re-raises it or whose classes heki cannot tell, a clause raising on a path or
//! exiting, a CLI boundary, a body reaching no cycle (or one only through a worker thread, a
//! coroutine only created, or a generator only made), `except*`, a `finally` body on the way, and
//! a module whose `Exception` is its own class; the opt-in selection, and `heki rule`.

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
"#;
const REBOUND: &str = r#"from pkg.errors import Exception
from pkg.shapes import depth


def guarded(tree: dict) -> int:
    try:
        return depth(tree)
    except Exception:
        return -1
"#;

fn project(name: &str) -> Project {
    let p = Project::new(name);
    p.write("pkg/__init__.py", "")
        .write("pkg/walk.py", WALK)
        .write("pkg/shapes.py", SHAPES)
        .write("pkg/cycle_a.py", CYCLE_A)
        .write("pkg/cycle_b.py", CYCLE_B)
        .write("pkg/errors.py", ERRORS)
        .write("pkg/rebound.py", REBOUND);
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
            d("pkg/walk.py", 18, "RefWalker.is_circular"),
            d("pkg/walk.py", 48, "nesting"),
        ]
    );
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
    // A helper above the cycle.
    assert!(
        at(&diags, "pkg/shapes.py", 45)["message"]
            .as_str()
            .unwrap()
            .contains("the `RecursionError` a recursion below `depth_of` can raise")
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
