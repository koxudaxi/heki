use super::*;
use crate::{ModuleIr, build_module_ir};
use heki_ast::parse_module;
use heki_env::{PythonVersion, RuntimeProfile};
use heki_semantic::analyze_module;

fn build(src: &str) -> ModuleIr {
    let parsed = parse_module(src).expect("valid python");
    let profile = RuntimeProfile::from(PythonVersion::PY315);
    let semantic = analyze_module(&parsed, src, profile);
    build_module_ir(&parsed, src, &semantic, profile)
}

/// On every return as called and as returned, and on every iteration of the innermost loop
/// around it as called and as returned.
type Facts = (
    Answer<Must>,
    Answer<Must>,
    Option<(Answer<Must>, Answer<Must>)>,
);

/// The facts of the one call of function `name` whose source text is `call`, or starts with
/// it when `call` does not end its argument list.
fn facts_with(
    src: &str,
    name: &str,
    call: &str,
    suppresses: &dyn Fn(VarId) -> Answer<bool>,
) -> Facts {
    let ir = build(src);
    let f = ir
        .functions
        .iter()
        .find(|f| f.name == name)
        .expect("function");
    let ix = FnIndex::new(f, &ir.constants);
    let bindings = ModuleBindings::of(&ir);
    let member = |m: &str, a: &str| m == "os" && a == "path";
    let reader = PathReader {
        bindings: Some(&bindings),
        module_member: &member,
        exit_suppresses: suppresses,
    };
    let budget = Budget::of_function("test", f);
    let paths = Paths::new(&ix, &reader, &budget);
    let mut found = Vec::new();
    for bb in &f.blocks {
        for (i, inst) in bb.insts.iter().enumerate() {
            let is_call = matches!(
                inst.kind,
                InstKind::Call { .. } | InstKind::MethodCall { .. }
            );
            let text = &src[inst.span.start as usize..inst.span.end as usize];
            // The whole call, or the text it starts with when no call is that text.
            if !is_call || !(text == call || (text.starts_with(call) && !call.ends_with(')'))) {
                continue;
            }
            let at = (bb.id, i);
            let iteration = ix.shape().innermost_index(bb.id).map(|li| {
                (
                    paths.on_every_iteration(li, at, Progress::Called),
                    paths.on_every_iteration(li, at, Progress::Returned),
                )
            });
            found.push((
                paths.on_every_return(at, Progress::Called),
                paths.on_every_return(at, Progress::Returned),
                iteration,
            ));
        }
    }
    assert_eq!(found.len(), 1, "{call} in {name}: {found:?}");
    found.remove(0)
}

fn may_suppress(_: VarId) -> Answer<bool> {
    Answer::Open
}

fn facts(src: &str, name: &str, call: &str) -> Facts {
    facts_with(src, name, call, &may_suppress)
}

const EVERY: Answer<Must> = Answer::Known(Must::Every);
const NOT_EVERY: Answer<Must> = Answer::Known(Must::NotEvery);
const DEAD: Answer<Must> = Answer::Known(Must::Dead);
const NEVER: Answer<Must> = Answer::Known(Must::NeverCompletes);

#[test]
fn a_call_runs_whenever_the_function_returns_by_the_edges_into_its_handlers() {
    let src = r#"
import subprocess
import os

def plain(db):
    db.fetch()
    return 1

def branch(db, flag):
    if flag:
        db.fetch()
    return 1

def own_handler(cmd):
    try:
        r = subprocess.run(["git", cmd], timeout=10)
    except FileNotFoundError:
        return None
    return r

def hop_caught(db):
    try:
        raw = db.get_setting("k")
    except Exception:
        raw = {}
    return raw

def earlier_try(path, t):
    try:
        cwd = path.cwd()
    except OSError:
        return None
    t.join(cwd)

def receiver_first(conn):
    try:
        rows = conn.execute("q").fetchall()
    except ValueError:
        return []
    return rows

def argument_first(conn, row):
    try:
        conn.send(row.id)
    except ValueError:
        return None

def module_member(p):
    try:
        os.path.join(p)
    except TypeError:
        return None

def subscript_first(cache, db, k):
    try:
        db.fetch(cache[k])
    except KeyError:
        return None

def handler_reraises(db):
    try:
        db.fetch(1)
    except ValueError:
        raise
    return 1

def never_returns(db):
    db.fetch()
    raise RuntimeError()
"#;
    for (name, call, called, returned) in [
        ("plain", "db.fetch", EVERY, EVERY),
        ("branch", "db.fetch", NOT_EVERY, NOT_EVERY),
        // The call's own exception still means it was called; the handler takes it after.
        ("own_handler", "subprocess.run", EVERY, NOT_EVERY),
        ("hop_caught", "db.get_setting", EVERY, NOT_EVERY),
        // A handler of an earlier `try` returns without the call.
        ("earlier_try", "t.join", NOT_EVERY, NOT_EVERY),
        // The receiver's call runs before `fetchall` in the same statement.
        (
            "receiver_first",
            "conn.execute(\"q\").fetchall()",
            NOT_EVERY,
            NOT_EVERY,
        ),
        ("receiver_first", "conn.execute(\"q\")", EVERY, NOT_EVERY),
        // An attribute load of an argument may raise before the call; so may a subscript.
        ("argument_first", "conn.send", NOT_EVERY, NOT_EVERY),
        ("subscript_first", "db.fetch", NOT_EVERY, NOT_EVERY),
        // A member of a module bound by `import` does not.
        ("module_member", "os.path.join", EVERY, NOT_EVERY),
        ("handler_reraises", "db.fetch", EVERY, EVERY),
        ("never_returns", "db.fetch", NEVER, NEVER),
    ] {
        let (c, r, _) = facts(src, name, call);
        assert_eq!((c, r), (called, returned), "{name}: {call}");
    }
}

#[test]
fn the_ir_alone_takes_every_load_as_one_that_may_raise() {
    let src = r#"
import subprocess

def own_handler(cmd):
    try:
        subprocess.run(["git", cmd])
    except FileNotFoundError:
        return None
"#;
    let ir = build(src);
    let f = ir
        .functions
        .iter()
        .find(|f| f.name == "own_handler")
        .unwrap();
    let ix = FnIndex::new(f, &ir.constants);
    let budget = Budget::of_function("test", f);
    let paths = Paths::new(&ix, &PathReader::ir(), &budget);
    let at = f
        .blocks
        .iter()
        .find_map(|bb| {
            let i = bb
                .insts
                .iter()
                .position(|i| matches!(i.kind, InstKind::MethodCall { .. }))?;
            Some((bb.id, i))
        })
        .unwrap();
    assert_eq!(paths.on_every_return(at, Progress::Called), NOT_EVERY);
}

#[test]
fn a_with_body_goes_on_past_its_exit_unless_the_reader_says_no_manager_suppresses() {
    let src = r#"
def read(open_, p, sub):
    with open_(p) as fh:
        data = fh.read()
        sub.run(data)
    return 1
"#;
    let (called, _, _) = facts(src, "read", "sub.run");
    assert_eq!(called, NOT_EVERY);
    let (called, _, _) = facts_with(src, "read", "sub.run", &|_| Answer::Known(false));
    assert_eq!(called, EVERY);
    let (called, _, _) = facts_with(src, "read", "sub.run", &|_| Answer::Known(true));
    assert_eq!(called, NOT_EVERY);
}

#[test]
fn a_call_runs_on_every_iteration_that_completes() {
    let src = r#"
def loops(rows, db, json):
    for row in rows:
        db.first(row)
        if row:
            db.sometimes(row)
        try:
            v = json.loads(row)
        except ValueError:
            continue
        db.after_try(v)
    for row in rows:
        if row:
            break
        db.before_break(row)
    for row in rows:
        db.leaves(row)
        break
    for row in rows:
        for cell in row:
            db.inner(cell)
"#;
    for (call, called, returned) in [
        ("db.first", EVERY, EVERY),
        ("db.sometimes", NOT_EVERY, NOT_EVERY),
        // The handler of `json.loads` goes on to the next item without the call.
        ("db.after_try", NOT_EVERY, NOT_EVERY),
        ("json.loads", EVERY, NOT_EVERY),
        // An iteration that breaks does not complete.
        ("db.before_break", EVERY, EVERY),
        // No iteration completes: every one leaves the loop.
        ("db.leaves", NEVER, NEVER),
        ("db.inner", EVERY, EVERY),
    ] {
        let (_, _, iteration) = facts(src, "loops", call);
        assert_eq!(iteration, Some((called, returned)), "{call}");
    }
}

#[test]
fn an_inner_loop_that_may_not_run_leaves_the_outer_iteration_without_its_call() {
    let src = r#"
def nested(rows, db):
    for row in rows:
        for cell in row:
            db.inner(cell)
"#;
    let ir = build(src);
    let f = ir.functions.iter().find(|f| f.name == "nested").unwrap();
    let ix = FnIndex::new(f, &ir.constants);
    let budget = Budget::of_function("test", f);
    let paths = Paths::new(&ix, &PathReader::ir(), &budget);
    let at = f
        .blocks
        .iter()
        .find_map(|bb| {
            let i = bb.insts.iter().position(
                |i| matches!(&i.kind, InstKind::MethodCall { method, .. } if method == "inner"),
            )?;
            Some((bb.id, i))
        })
        .unwrap();
    let outer = f.loops.iter().position(|lp| lp.depth == 1).unwrap();
    assert_eq!(
        paths.on_every_iteration(outer, at, Progress::Called),
        NOT_EVERY
    );
}

#[test]
fn branches_the_module_constants_decide_leave_dead_code() {
    let src = r#"
import typing
from typing import TYPE_CHECKING

_IS_SYNC = False
MODE = "async"
LEVEL = 0

def f(sock):
    if not _IS_SYNC:
        sock.live()
    else:
        sock.dead_bool()
    if TYPE_CHECKING:
        sock.dead_typing()
    if typing.TYPE_CHECKING:
        sock.dead_typing_attr()
    if MODE == "sync":
        sock.dead_compare()
    if LEVEL:
        sock.dead_int()
    if _IS_SYNC and sock:
        sock.dead_and()
    if not _IS_SYNC or sock:
        sock.live_or()
    while False:
        sock.dead_while()
    sock.after()
"#;
    for (call, called) in [
        ("sock.live()", EVERY),
        ("sock.dead_bool", DEAD),
        ("sock.dead_typing()", DEAD),
        ("sock.dead_typing_attr", DEAD),
        ("sock.dead_compare", DEAD),
        ("sock.dead_int", DEAD),
        ("sock.dead_and", DEAD),
        ("sock.live_or", EVERY),
        ("sock.dead_while", DEAD),
        ("sock.after", EVERY),
    ] {
        let (c, _, _) = facts(src, "f", call);
        assert_eq!(c, called, "{call}");
    }
}

#[test]
fn a_name_the_module_binds_conditionally_may_be_unbound() {
    let src = r#"
try:
    import ujson as json
except ImportError:
    pass

import subprocess

def maybe(x):
    try:
        json.loads(x)
    except Exception:
        return None

def bound(x):
    try:
        subprocess.run(x)
    except Exception:
        return None
"#;
    let (called, _, _) = facts(src, "maybe", "json.loads");
    assert_eq!(called, NOT_EVERY);
    let (called, _, _) = facts(src, "bound", "subprocess.run");
    assert_eq!(called, EVERY);
}

#[test]
fn constant_comparisons_decide_only_what_python_fixes() {
    use CompareOpKind::{Eq, Is, IsNot, Lt, NotEq};
    for (op, a, b, decided) in [
        (Eq, Scalar::Int(1), Scalar::Int(1), Some(true)),
        (NotEq, Scalar::Str("a"), Scalar::Str("b"), Some(true)),
        // `1 == True` mixes kinds.
        (Eq, Scalar::Int(1), Scalar::Bool(true), None),
        (Is, Scalar::None, Scalar::None, Some(true)),
        (IsNot, Scalar::Bool(true), Scalar::Bool(false), Some(true)),
        (Is, Scalar::Int(0), Scalar::None, Some(false)),
        (Is, Scalar::Int(1), Scalar::Int(1), None),
        (Lt, Scalar::Int(1), Scalar::Int(2), None),
    ] {
        assert_eq!(Scalar::compare(op, a, b), decided, "{op:?} {a:?} {b:?}");
    }
}

#[test]
fn literal_tests_decide_by_python_truth() {
    let src = r#"
NAME = ""
NOTHING = None

def f(sock):
    if NAME:
        sock.dead_str()
    if 0.0:
        sock.dead_float()
    if None:
        sock.dead_none()
    if "x":
        sock.live_str()
    sock.after()
"#;
    for (call, called) in [
        ("sock.dead_str", DEAD),
        ("sock.dead_float", DEAD),
        ("sock.dead_none", DEAD),
        ("sock.live_str", EVERY),
        ("sock.after", EVERY),
    ] {
        let (c, _, _) = facts(src, "f", call);
        assert_eq!(c, called, "{call}");
    }
}

#[test]
fn globals_bound_behind_the_ir_or_by_a_star_import_are_not_trusted() {
    for (header, called) in [
        ("import subprocess\nexec('x = 1')\n", NOT_EVERY),
        ("import subprocess\nfrom os import *\n", NOT_EVERY),
        ("import subprocess\n", EVERY),
    ] {
        let src = format!(
            "{header}\ndef run(xs):\n    try:\n        len(xs)\n    except Exception:\n        return None\n"
        );
        let (c, _, _) = facts(&src, "run", "len(xs)");
        assert_eq!(c, called, "{header}");
    }
}

#[test]
fn a_display_that_unpacks_iterates_its_operand_which_may_raise() {
    let src = r#"
import subprocess

def listed(x):
    try:
        subprocess.run([x, "-v"])
    except Exception:
        return None

def unpacked(xs):
    try:
        subprocess.run([*xs])
    except Exception:
        return None
"#;
    let (c, _, _) = facts(src, "listed", "subprocess.run");
    assert_eq!(c, EVERY);
    let (c, _, _) = facts(src, "unpacked", "subprocess.run");
    assert_eq!(c, NOT_EVERY);
}

#[test]
fn facts_past_the_budget_are_open() {
    let src = r#"
def loop(rows, db):
    for row in rows:
        db.fetch(row)
"#;
    let ir = build(src);
    let f = ir.functions.iter().find(|f| f.name == "loop").unwrap();
    let ix = FnIndex::new(f, &ir.constants);
    let at = f
        .blocks
        .iter()
        .find_map(|bb| {
            let i = bb
                .insts
                .iter()
                .position(|i| matches!(i.kind, InstKind::MethodCall { .. }))?;
            Some((bb.id, i))
        })
        .unwrap();
    for steps in [1, 12] {
        let budget = Budget::per_source_byte("test", f, 0, steps, steps);
        let paths = Paths::new(&ix, &PathReader::ir(), &budget);
        assert_eq!(paths.on_every_return(at, Progress::Called), Answer::Open);
        assert_eq!(
            paths.on_every_iteration(0, at, Progress::Called),
            Answer::Open
        );
    }
    let budget = Budget::of_function("test", f);
    let paths = Paths::new(&ix, &PathReader::ir(), &budget);
    assert_eq!(
        paths.on_every_iteration(7, at, Progress::Called),
        Answer::Open
    );
    assert_eq!(paths.on_every_iteration(0, at, Progress::Called), EVERY);
    assert_eq!(paths.on_every_iteration(0, at, Progress::Returned), EVERY);
    assert_eq!(ix.decided().never_taken(f.loops[0].header), None);
}

#[test]
fn the_remaining_shapes_of_tests_raises_and_regions() {
    let src = r#"
import os
import json
del json

def f(sock, flag, conn, row, db):
    if sock.ready:
        sock.maybe()
    if (False if flag else False):
        sock.dead_phi()
    try:
        conn.send(row.a.b)
    except ValueError:
        return None
    try:
        json.loads(row)
    except Exception:
        return None
    try:
        if flag:
            raise ValueError()
        db.fetch()
    except ValueError:
        return None
    for r in row:
        db.inside(r)
    db.outside()
"#;
    for (call, called) in [
        ("sock.maybe()", NOT_EVERY),
        ("sock.dead_phi()", DEAD),
        ("conn.send(row.a.b)", NOT_EVERY),
        ("json.loads(row)", NOT_EVERY),
        ("db.fetch()", NOT_EVERY),
    ] {
        let (c, _, _) = facts(src, "f", call);
        assert_eq!(c, called, "{call}");
    }
    let ir = build(src);
    let f = ir.functions.iter().find(|f| f.name == "f").unwrap();
    let ix = FnIndex::new(f, &ir.constants);
    let bindings = ModuleBindings::of(&ir);
    let budget = Budget::of_function("test", f);
    // The reader that knows only the bindings knows no module member.
    let paths = Paths::new(&ix, &PathReader::with_bindings(&bindings), &budget);
    let at = |method: &str| {
        f.blocks
            .iter()
            .find_map(|bb| {
                let i = bb.insts.iter().position(
                    |i| matches!(&i.kind, InstKind::MethodCall { method: m, .. } if m == method),
                )?;
                Some((bb.id, i))
            })
            .unwrap()
    };
    // A point outside a loop never runs on its iterations.
    assert_eq!(
        paths.on_every_iteration(0, at("outside"), Progress::Called),
        DEAD
    );
    assert_eq!(
        paths.on_every_iteration(0, at("inside"), Progress::Called),
        EVERY
    );
}

#[test]
fn a_member_of_an_imported_module_is_known_only_to_a_reader_that_says_so() {
    let src = r#"
import os

def f(p):
    with open(p):
        pass
    try:
        os.path.join(p)
    except TypeError:
        return None
"#;
    let ir = build(src);
    let f = ir.functions.iter().find(|f| f.name == "f").unwrap();
    let ix = FnIndex::new(f, &ir.constants);
    let bindings = ModuleBindings::of(&ir);
    let budget = Budget::of_function("test", f);
    let paths = Paths::new(&ix, &PathReader::with_bindings(&bindings), &budget);
    let at = f
        .blocks
        .iter()
        .find_map(|bb| {
            let i = bb.insts.iter().position(
                |i| matches!(&i.kind, InstKind::MethodCall { method, .. } if method == "join"),
            )?;
            Some((bb.id, i))
        })
        .unwrap();
    assert_eq!(paths.on_every_return(at, Progress::Called), NOT_EVERY);
    let (called, _, _) = facts(src, "f", "os.path.join(p)");
    assert_eq!(called, EVERY);
}
