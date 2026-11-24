//! A call through a name the project binds to another object (assumption R1), end to end
//! through the real binary: the call's targets are no longer every implementation it can run,
//! so `EFF101` does not claim the chain and `EFF111` reports it with "Every implementation"
//! open. The stores are those of the module defining the name (a `del` of the global, a
//! decorator applied by hand, a `del` in the class body, a store on the class object, also
//! through `setattr`, `__code__`, also in a function, `__bases__`, `__class__` through `self`,
//! and `globals()`) and those of another module (by name, through `setattr` with a string
//! literal, `del` and `delattr`, `__code__`, `__bases__`, on a class a subclass reaches through
//! `super()`, on a property, and `setattr` under a name heki cannot read on a module), for a
//! call by name, on an instance, on the class object, and through `super()`. The names nothing
//! binds again keep the exact report. A name the module body or the class body binds last to
//! one named object on every path is that object (`assigned = fast`, `body = fast`,
//! `Made = Other`, none of which waits), and one bound so on a branch is either object, a
//! target without the fact. `MUT114`'s claim that the changed object is what a cached function
//! returned rests on the same targets, and a module constant set through `setattr` is no
//! constant.
//!
//! Every fixture marks each line with the report it must get ([`MARKS`]).

mod common;

use common::Project;
use serde_json::Value;

/// `(path, line, rule, open premises as (premise, cause))` of one report.
type Report = (String, u64, String, Vec<(String, String)>);

/// The mark ending a line, the rule reporting the line, and the premises its report leaves
/// open as `(premise, cause)` (none: exact).
type Mark = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
);

const MARKS: &[Mark] = &[
    ("# EFF101", "EFF101", &[]),
    (
        "# EFF111",
        "EFF111",
        &[("Every implementation", "incomplete-targets")],
    ),
    (
        "# EFF111 either",
        "EFF111",
        &[("Every implementation", "implementation-without-the-fact")],
    ),
    (
        "# MUT114 heuristic",
        "MUT114",
        &[("Same object", "not-the-result-on-every-path")],
    ),
    ("# MUT114 exact", "MUT114", &[]),
];

/// Every report of the rules [`MARKS`] names, before reports are grouped (`heki
/// dump-open-premises`).
fn reported(p: &Project) -> Vec<Report> {
    let select = "EFF101,EFF111,MUT114";
    let (code, stdout, stderr) = p.heki(&["dump-open-premises", "--select", select, "."]);
    assert_eq!(code, 0, "{stderr}");
    let out: Value = serde_json::from_str(&stdout).unwrap();
    let mut found: Vec<_> = out["reports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let open = r["open_premises"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| {
                    (
                        o["premise"].as_str().unwrap().to_owned(),
                        o["cause"].as_str().unwrap().to_owned(),
                    )
                })
                .collect();
            (
                r["path"].as_str().unwrap().to_owned(),
                r["line"].as_u64().unwrap(),
                r["rule"].as_str().unwrap().to_owned(),
                open,
            )
        })
        .collect();
    found.sort();
    found
}

/// The reports the marks of `files` ask for.
fn marked(files: &[(&str, &str)]) -> Vec<Report> {
    let mut out = Vec::new();
    for (path, text) in files {
        for (i, line) in text.lines().enumerate() {
            let Some((_, rule, open)) = MARKS.iter().find(|(m, ..)| line.trim_end().ends_with(m))
            else {
                continue;
            };
            let open = open
                .iter()
                .map(|(p, c)| ((*p).to_owned(), (*c).to_owned()))
                .collect();
            out.push(((*path).to_owned(), i as u64 + 1, (*rule).to_owned(), open));
        }
    }
    out.sort();
    out
}

fn check_marks(name: &str, files: &[(&str, &str)]) {
    let p = Project::new(name);
    p.write("pyproject.toml", "[project]\nname = \"t\"\n");
    p.write("pkg/__init__.py", "");
    for (path, text) in files {
        p.write(path, text);
    }
    assert_eq!(reported(&p), marked(files));
}

/// The module defining the names binds them again.
const OWN_MODULE: &str = r#"import functools
import os
import time


def fast(*args):
    return 1


def assigned():
    time.sleep(1)


def branch():
    time.sleep(1)


def deleted():
    time.sleep(1)


def recoded():
    time.sleep(1)


def cached():
    time.sleep(1)


def kept():
    time.sleep(1)


def late():
    time.sleep(1)


def patch_later():
    late.__code__ = fast.__code__


class Other:
    def run(self):
        return 1


class Base:
    def run(self):
        time.sleep(1)


class Rebased(Base):
    pass


class Plain:
    def run(self):
        time.sleep(1)


class Morph:
    def run(self):
        time.sleep(1)

    def morph(self):
        self.__class__ = Other


class C:
    def body(self):
        time.sleep(1)

    body = fast

    def removed(self):
        time.sleep(1)

    del removed

    def stored(self):
        time.sleep(1)

    def set_by_name(self):
        time.sleep(1)

    def recoded(self):
        time.sleep(1)

    def kept(self):
        time.sleep(1)


class Made:
    def __init__(self):
        time.sleep(1)


class Built:
    def __init__(self):
        time.sleep(1)


assigned = fast
if os.environ.get("FAST"):
    branch = fast
del deleted
deleted = fast
recoded.__code__ = fast.__code__
cached = functools.lru_cache(cached)
C.stored = fast
setattr(C, "set_by_name", fast)
C.recoded.__code__ = fast.__code__
Rebased.__bases__ = (Other,)
Made = Other
Built.__init__ = fast
"#;

/// `globals()` may bind any name of the module.
const DYNAMIC: &str = r#"import time


def swapped():
    time.sleep(1)


globals()["swapped"] = print
"#;

const OWN_MODULE_CALLS: &str = r#"from pkg import dynamic, lib


async def assigned():
    lib.assigned()


async def branch():
    lib.branch()  # EFF111 either


async def deleted():
    lib.deleted()  # EFF111


async def recoded():
    lib.recoded()  # EFF111


async def cached():
    lib.cached()  # EFF111


async def kept():
    lib.kept()  # EFF101


async def late():
    lib.late()  # EFF111


async def body():
    lib.C().body()


async def removed():
    lib.C().removed()  # EFF111


async def stored():
    lib.C().stored()  # EFF111


async def set_by_name():
    lib.C().set_by_name()  # EFF111


async def method_recoded():
    lib.C().recoded()  # EFF111


async def method_kept():
    lib.C().kept()  # EFF101


async def rebased():
    lib.Rebased().run()  # EFF111


async def plain():
    lib.Plain().run()  # EFF101


async def morph(m: lib.Morph):
    m.run()  # EFF111


async def made():
    lib.Made()


async def built():
    lib.Built()  # EFF111


async def swapped():
    dynamic.swapped()  # EFF111
"#;

#[test]
fn a_name_its_own_module_binds_again_is_no_exact_target() {
    check_marks(
        "rebound_own_module",
        &[
            ("pkg/lib.py", OWN_MODULE),
            ("pkg/dynamic.py", DYNAMIC),
            ("pkg/app.py", OWN_MODULE_CALLS),
        ],
    );
}

/// The calls within the module defining the names see its stores too.
const IN_MODULE: &str = r#"import time


def fast(*args):
    return 1


def assigned():
    time.sleep(1)


def kept():
    time.sleep(1)


class C:
    def stored(self):
        time.sleep(1)

    def kept(self):
        time.sleep(1)


assigned = fast
C.stored = fast


async def calls_assigned():
    assigned()


async def calls_kept():
    kept()  # EFF101


async def calls_stored(c: C):
    c.stored()  # EFF111


async def calls_method_kept(c: C):
    c.kept()  # EFF101
"#;

#[test]
fn a_call_in_the_module_rebinding_the_name_is_no_exact_target() {
    check_marks("rebound_in_module", &[("pkg/lib.py", IN_MODULE)]);
}

const LIB: &str = r#"import time


def stored():
    time.sleep(1)


def set_by_name():
    time.sleep(1)


def deleted():
    time.sleep(1)


def deleted_by_call():
    time.sleep(1)


def recoded():
    time.sleep(1)


def kept():
    time.sleep(1)


class Other:
    def run(self):
        return 1


class Rebased:
    def run(self):
        time.sleep(1)


class Parent:
    def run(self):
        time.sleep(1)


class C:
    def stored(self):
        time.sleep(1)

    def set_by_name(self):
        time.sleep(1)

    def recoded(self):
        time.sleep(1)

    def kept(self):
        time.sleep(1)

    @property
    def prop(self):
        time.sleep(1)
        return 1

    @property
    def kept_prop(self):
        time.sleep(1)
        return 1
"#;

const UNNAMED: &str = r#"import time


def swapped():
    time.sleep(1)
"#;

/// Another module binds the names of `lib` and `unnamed` again.
const PATCHES: &str = r#"from pkg import lib, unnamed


def fast(*args):
    return 1


lib.stored = fast
setattr(lib, "set_by_name", fast)
del lib.deleted
delattr(lib, "deleted_by_call")
lib.recoded.__code__ = fast.__code__
lib.C.stored = fast
setattr(lib.C, "set_by_name", fast)
lib.C.recoded.__code__ = fast.__code__
lib.C.prop = property(fast)
lib.Rebased.__bases__ = (lib.Other,)
lib.Parent.run = fast
for name in ["swapped"]:
    setattr(unnamed, name, fast)
"#;

const OTHER_MODULE_CALLS: &str = r#"from pkg import lib, unnamed


class Child(lib.Parent):
    def run(self):
        super().run()


async def stored():
    lib.stored()  # EFF111


async def set_by_name():
    lib.set_by_name()  # EFF111


async def deleted():
    lib.deleted()  # EFF111


async def deleted_by_call():
    lib.deleted_by_call()  # EFF111


async def recoded():
    lib.recoded()  # EFF111


async def kept():
    lib.kept()  # EFF101


async def method_stored():
    lib.C().stored()  # EFF111


async def method_set_by_name():
    lib.C().set_by_name()  # EFF111


async def method_recoded():
    lib.C().recoded()  # EFF111


async def method_kept():
    lib.C().kept()  # EFF101


async def on_the_class(c: lib.C):
    lib.C.stored(c)  # EFF111


async def kept_on_the_class(c: lib.C):
    lib.C.kept(c)  # EFF101


async def prop():
    return lib.C().prop  # EFF111


async def kept_prop():
    return lib.C().kept_prop  # EFF101


async def rebased():
    lib.Rebased().run()  # EFF111


async def through_super():
    Child().run()  # EFF111


async def swapped():
    unnamed.swapped()  # EFF111
"#;

#[test]
fn a_name_another_module_binds_again_is_no_exact_target() {
    check_marks(
        "rebound_elsewhere",
        &[
            ("pkg/lib.py", LIB),
            ("pkg/unnamed.py", UNNAMED),
            ("pkg/patches.py", PATCHES),
            ("pkg/app.py", OTHER_MODULE_CALLS),
        ],
    );
}

/// Without the stores, every call keeps its exact report.
#[test]
fn the_same_names_bound_once_are_exact_targets() {
    let calls = OTHER_MODULE_CALLS.replace("# EFF111", "# EFF101");
    check_marks(
        "rebound_nowhere",
        &[
            ("pkg/lib.py", LIB),
            ("pkg/unnamed.py", UNNAMED),
            ("pkg/patches.py", "from pkg import lib, unnamed\n"),
            ("pkg/app.py", &calls),
        ],
    );
}

const CACHED: &str = r#"from functools import lru_cache


@lru_cache
def fields(name):
    return [name, "id"]


@lru_cache
def kept(name):
    return [name, "id"]
"#;

const CACHED_PATCH: &str = r#"from pkg import cached


def fresh(name):
    return [name]


cached.fields = fresh
"#;

const CACHED_CALLS: &str = r#"from pkg import cached


def extended(name):
    result = cached.fields(name)
    result.append("extra")  # MUT114 heuristic
    return result


def extended_kept(name):
    result = cached.kept(name)
    result.append("extra")  # MUT114 exact
    return result
"#;

/// The changed list is not established to be what the cached function returned once another
/// module binds the function's name to another one.
#[test]
fn a_cached_function_bound_again_hands_out_no_established_result() {
    check_marks(
        "rebound_cached",
        &[
            ("pkg/cached.py", CACHED),
            ("pkg/patches.py", CACHED_PATCH),
            ("pkg/app.py", CACHED_CALLS),
        ],
    );
}

const SETTINGS: &str = "PAUSE = 0\nQUIET = 0\n";

const SETTINGS_PATCH: &str = "from pkg import settings\n\nsetattr(settings, \"PAUSE\", 5)\n";

const PAUSES: &str = r#"import time

from pkg import settings


async def paused():
    time.sleep(settings.PAUSE)  # EFF101


async def quiet():
    time.sleep(settings.QUIET)
"#;

/// `setattr` with a string literal binds a module constant again as a store by name does: the
/// pause is no longer the constant `0`, and an argument heki cannot evaluate waits (R6).
#[test]
fn a_constant_set_through_setattr_is_no_constant() {
    check_marks(
        "rebound_constant",
        &[
            ("pkg/settings.py", SETTINGS),
            ("pkg/patches.py", SETTINGS_PATCH),
            ("pkg/app.py", PAUSES),
        ],
    );
}
