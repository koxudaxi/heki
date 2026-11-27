//! Which functions a call runs, end to end through the real binary: methods found along
//! Python's MRO (C3 linearization), constructing a named class (its own `__init__`, not a
//! subclass's) and calling a method through it, class methods called through their class,
//! calling an instance (`__call__`), constructions through a module, the targets of `with`
//! statements, module-level names bound twice, a package's name over a submodule of the same
//! name (`from pkg import parse`), the names a star import binds, the parameters `*` arguments
//! bind, project
//! methods named like container mutators, augmented assignments, the bases of class statements
//! in functions, what calling and awaiting an `async def` give, and the types of globals and
//! instance attributes, which every store of each must agree on.
//!
//! Every fixture marks the lines that must be reported with the rule code in a comment
//! (`# EFF101`); every other line must not be.

mod common;

use common::Project;
use serde_json::Value;

const CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
extend-select = ["MUT108"]

[[tool.heki.write-owners]]
name = "model"
targets = ["Model.items", "Holder.stats", "Holder.tally"]
"#;

/// `(path, line, rule)` of every diagnostic of `rules`.
fn reported(p: &Project, rules: &[&str]) -> Vec<(String, u64, String)> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<(String, u64, String)> = diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                common::tiered_code(d).to_owned(),
            )
        })
        .filter(|(_, _, r)| rules.contains(&r.as_str()))
        .collect();
    out.sort();
    out
}

/// The `(path, line, rule)` marks of `files`: a line ending in `# <RULE>` for each of `rules`.
fn marked(files: &[(&str, &str)], rules: &[&str]) -> Vec<(String, u64, String)> {
    let mut out = Vec::new();
    for (path, text) in files {
        for (i, line) in text.lines().enumerate() {
            for rule in rules {
                if line.trim_end().ends_with(&format!("# {rule}")) {
                    out.push(((*path).to_owned(), i as u64 + 1, (*rule).to_owned()));
                }
            }
        }
    }
    out.sort();
    out
}

fn check_marks(name: &str, files: &[(&str, &str)], rules: &[&str]) {
    let rules = common::with_tiers(rules);
    let rules = rules.as_slice();
    let p = Project::new(name);
    p.write("pyproject.toml", CONFIG);
    p.write("pkg/__init__.py", "");
    for (path, text) in files {
        p.write(path, text);
    }
    assert_eq!(reported(&p, rules), marked(files, rules));
}

/// `Leaf(Left, Right)` finds `Right.read` before `Root.read`, which `Left` inherits.
const DIAMOND: &str = r#"import time


class Root:
    def read(self):
        return 1


class Left(Root):
    pass


class Right(Root):
    def read(self):
        time.sleep(1)


class Leaf(Left, Right):
    pass


class Plain(Left):
    pass


async def leaf(x: Leaf):
    x.read()  # EFF101


async def plain(x: Plain):
    x.read()
"#;

#[test]
fn methods_are_found_along_the_c3_linearization() {
    check_marks("mro_c3", &[("pkg/diamond.py", DIAMOND)], &["EFF101"]);
}

/// A named class constructs itself and finds its own methods; `cls` in a class method may be
/// any subclass.
const NAMED: &str = r#"import time


class Base:
    def read(self):
        return 1

    @classmethod
    def make(cls):
        return cls()


class Slow(Base):
    def __init__(self):
        time.sleep(1)

    def read(self):
        time.sleep(1)


async def construct_base():
    Base()


async def construct_slow():
    Slow()  # EFF101


async def through_base(obj):
    Base.read(obj)


async def through_slow(obj):
    Slow.read(obj)  # EFF101


async def through_cls():
    Base.make()  # EFF101 [heuristic]
"#;

#[test]
fn a_named_class_constructs_itself_and_finds_its_own_methods() {
    check_marks("named_class", &[("pkg/named.py", NAMED)], &["EFF101"]);
}

/// A class method called through its class binds the class to `cls`: the first argument is
/// its second parameter. A static method and a plain function called through the class bind
/// none.
const CLASS_CALLS: &str = r#"from pkg.model import Model


class Store:
    @classmethod
    def drain(cls, xs):
        xs.clear()

    @classmethod
    def drain_through(cls, xs):
        cls.drain(xs)

    @classmethod
    def keep(cls, xs, ys):
        ys.clear()

    @staticmethod
    def wipe(xs):
        xs.clear()

    def own(self, xs):
        xs.clear()


def through_class(m: Model, other: list):
    Store.drain(m.items)  # MUT201
    Store.drain_through(m.items)  # MUT201
    Store.keep(m.items, other)
    Store.wipe(m.items)  # MUT201
    Store.own(Store(), m.items)  # MUT201
    Store.own(m.items, other)
"#;

const MODEL: &str = r#"class Model:
    def __init__(self):
        self.items = []
"#;

#[test]
fn class_methods_called_through_their_class_bind_the_class() {
    check_marks(
        "class_calls",
        &[("pkg/model.py", MODEL), ("pkg/store.py", CLASS_CALLS)],
        &["MUT201"],
    );
}

/// Calling an instance runs its `__call__` with the instance as `self`, and returns what that
/// returns; constructing a class returns the new instance.
const CALLABLE: &str = r#"import functools


@functools.cache
def shared():
    return []


class Provider:
    def __call__(self):
        return shared()


class Holder(Provider):
    def __init__(self):
        self.items = []


def via(p: Provider):
    return p()


def use(p: Provider):
    via(Provider()).clear()  # MUT114
    p().clear()  # MUT114
    Holder().items.clear()
"#;

#[test]
fn calling_an_instance_runs_its_call_method_on_it() {
    check_marks("callable", &[("pkg/callable.py", CALLABLE)], &["MUT114"]);
}

/// Constructing a frozen dataclass through its module makes a new object, as importing it does.
const RECORDS: &str = r#"from dataclasses import dataclass


@dataclass(frozen=True)
class Record:
    items: list
"#;

const USES: &str = r#"from pkg import records
from pkg.records import Record


def through_module():
    r = records.Record([])
    r.items.append(1)
    return r


def imported():
    r = Record([])
    r.items.append(1)
    return r


def given(r: records.Record):
    r.items.append(1)  # MUT108
"#;

#[test]
fn constructing_through_a_module_makes_a_new_object() {
    check_marks(
        "qualified_construction",
        &[("pkg/records.py", RECORDS), ("pkg/uses.py", USES)],
        &["MUT108"],
    );
}

/// The target of `with m as r` is what `m.__enter__()` returns (`await m.__aenter__()` for
/// `async with`): the declared return type, else the manager itself.
const MANAGERS: &str = r#"import time


class Resource:
    def read(self):
        time.sleep(1)


class Manager:
    def __enter__(self) -> Resource:
        return Resource()

    def __exit__(self, *exc):
        return None

    def read(self):
        return None


class AsyncManager:
    async def __aenter__(self) -> Resource:
        return Resource()

    async def __aexit__(self, *exc):
        return None

    def read(self):
        return None


class Itself:
    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return None

    def read(self):
        time.sleep(1)


async def entered():
    with Manager() as r:
        r.read()  # EFF101


async def entered_async():
    async with AsyncManager() as r:
        r.read()  # EFF101


async def manager_itself(m: Manager):
    m.read()
    with Itself() as i:
        i.read()  # EFF101
"#;

#[test]
fn a_with_target_is_what_the_manager_enters() {
    check_marks(
        "with_targets",
        &[("pkg/managers.py", MANAGERS)],
        &["EFF101"],
    );
}

/// A module-level name is what its module binds last: a definition after an import of the same
/// name shadows it (also for the modules importing the name), and an import after a definition
/// shadows the definition. A name bound on different paths keeps its import.
const SHADOWS: &str = r#"import time
from time import sleep

import requests
from pkg.slow import helper


def sleep(delay):
    pass


def helper():
    return None


def fetch(url):
    return None


from requests import get as fetch


class Session:
    def __init__(self):
        time.sleep(1)


from requests import Session

try:
    from requests import post
except ImportError:
    def post(url):
        return None


async def local(url):
    sleep(1)
    helper()
    fetch(url)  # EFF101
"#;

const SLOW: &str = r#"import time


def helper():
    time.sleep(1)
"#;

const SHADOW_USERS: &str = r#"from pkg.shadows import Session, fetch, helper, post, sleep


async def handler(url):
    sleep(1)
    helper()
    fetch(url)  # EFF101
    post(url)  # EFF101
    Session()
"#;

#[test]
fn a_module_name_is_what_its_module_binds_last() {
    check_marks(
        "shadowed_imports",
        &[
            ("pkg/shadows.py", SHADOWS),
            ("pkg/slow.py", SLOW),
            ("pkg/users.py", SHADOW_USERS),
        ],
        &["EFF101"],
    );
}

/// A module `parse.py` in every package below, with a function of its own name.
const PARSE_MODULE: &str = r#"import time


def parse(x):
    time.sleep(1)
    return x


def helper():
    time.sleep(1)
"#;

/// `from pkg.rebind_rel import parse` takes the attribute `parse` of the package once its
/// `__init__.py` has run, and the submodule only when the package binds no such name. A package
/// binding it through the submodule binds it after the submodule was loaded, for good: the
/// function. A package binding it otherwise holds it only until something first imports the
/// submodule, which then replaces it: either object, so no report. A `from` import of the
/// submodule itself (`from pkg.rebind_abs.parse import helper`) takes the module.
const PACKAGE_NAME_USERS: &str = r#"import pkg.rebind_abs
import pkg.rebind_rel.parse as imported_as
from pkg.elsewhere import parse as elsewhere_parse
from pkg.facade import parse as facade_parse
from pkg.own_def import parse as own_parse
from pkg.plain import parse as plain_parse
from pkg.rebind_abs import parse as abs_parse
from pkg.rebind_abs.parse import helper
from pkg.rebind_alias import parse as alias_parse
from pkg.rebind_rel import parse as rel_parse
from pkg.self_import import parse as self_parse


async def absolute_reexport(x):
    return abs_parse(x)  # EFF101


async def relative_reexport(x):
    return rel_parse(x)  # EFF101


async def rebound_by_alias(x):
    return alias_parse(x)  # EFF101


async def package_attribute(x):
    return pkg.rebind_abs.parse(x)  # EFF101


async def import_as(x):
    return imported_as(x)  # EFF101


async def through_a_facade(x):
    return facade_parse(x)  # EFF101


async def from_the_submodule():
    helper()  # EFF101


async def submodule_of_an_empty_init(x):
    plain_parse.helper()  # EFF101
    return plain_parse(x)


async def submodule_the_init_imports(x):
    self_parse.helper()  # EFF101
    return self_parse(x)


async def bound_before_any_load(x):
    return own_parse(x)


async def bound_from_another_module(x):
    return elsewhere_parse(x)
"#;

const REBIND_ALIAS_INIT: &str = r#"from . import parse

parse = parse.parse


async def inside(x):
    return parse(x)  # EFF101
"#;

const OWN_DEF_INIT: &str = r#"import time


def parse(x):
    time.sleep(1)
    return x
"#;

#[test]
fn a_package_name_over_a_submodule_is_what_the_package_binds_last() {
    check_marks(
        "package_names",
        &[
            (
                "pkg/rebind_abs/__init__.py",
                "from pkg.rebind_abs.parse import parse\n",
            ),
            ("pkg/rebind_abs/parse.py", PARSE_MODULE),
            ("pkg/rebind_rel/__init__.py", "from .parse import parse\n"),
            ("pkg/rebind_rel/parse.py", PARSE_MODULE),
            ("pkg/rebind_alias/__init__.py", REBIND_ALIAS_INIT),
            ("pkg/rebind_alias/parse.py", PARSE_MODULE),
            ("pkg/plain/__init__.py", ""),
            ("pkg/plain/parse.py", PARSE_MODULE),
            ("pkg/self_import/__init__.py", "from . import parse\n"),
            ("pkg/self_import/parse.py", PARSE_MODULE),
            ("pkg/own_def/__init__.py", OWN_DEF_INIT),
            ("pkg/own_def/parse.py", PARSE_MODULE),
            ("pkg/elsewhere/__init__.py", "from .core import parse\n"),
            ("pkg/elsewhere/core.py", PARSE_MODULE),
            ("pkg/elsewhere/parse.py", PARSE_MODULE),
            ("pkg/facade.py", "from pkg.rebind_rel import parse\n"),
            ("pkg/users.py", PACKAGE_NAME_USERS),
        ],
        &["EFF101"],
    );
}

/// `from pkg.unread import parse` where the package's `__init__.py` does not parse: what it
/// binds is not known, so the call is taken neither as the submodule's function nor as an
/// attribute of the package.
#[test]
fn a_package_name_over_a_submodule_is_open_when_the_init_is_unread() {
    let p = Project::new("package_name_unread_init");
    p.write("pyproject.toml", CONFIG);
    p.write("pkg/__init__.py", "");
    p.write("pkg/unread/__init__.py", "from .parse import parse\n");
    p.write("pkg/unread/parse.py", PARSE_MODULE);
    p.write(
        "pkg/users.py",
        "from pkg.unread import parse\n\n\nasync def go(x):\n    return parse(x)\n",
    );
    let (code, stdout, stderr) = p.check(&["--output-format", "concise"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stdout.contains("pkg/users.py:5:12: EFF101 "), "{stdout}");
    p.write("pkg/unread/__init__.py", "print \"done\"\n");
    let (code, stdout, stderr) = p.check(&["--output-format", "concise"]);
    assert_eq!(code, 2, "{stdout}");
    assert!(
        stderr.contains("error: pkg/unread/__init__.py:1:7: syntax error: "),
        "{stderr}"
    );
    assert!(!stdout.contains("EFF101"), "{stdout}");
}

#[test]
fn a_package_import_bound_again_is_open() {
    let p = Project::new("package_import_bound_again");
    p.write("pyproject.toml", CONFIG);
    p.write("pkg/__init__.py", "");
    p.write("pkg/rebound/parse.py", PARSE_MODULE);
    p.write(
        "pkg/users.py",
        "from pkg.rebound import parse\n\n\nasync def go(x):\n    return parse(x)\n",
    );
    for init in [
        "from .parse import parse\n\nif FAST:\n    parse = lambda x: x\n",
        "try:\n    from ._speedups import parse\nexcept ImportError:\n    from .parse import parse\n",
    ] {
        p.write("pkg/rebound/__init__.py", init);
        let (code, stdout, stderr) = p.check(&["--output-format", "concise"]);
        assert_eq!(code, 0, "{init}\n{stdout}{stderr}");
        assert!(!stdout.contains("EFF101"), "{init}\n{stdout}");
    }
}

/// A module of public functions that block, and a private one.
const STAR_IMPL: &str = r#"import time


def fetch(url):
    time.sleep(1)
    return url


def _hidden(url):
    time.sleep(1)
    return url
"#;

/// A module whose `__all__` lists one of its two blocking functions.
const LISTED_IMPL: &str = r#"import time

__all__ = ["fetch"]


def fetch(url):
    time.sleep(1)
    return url


def unlisted(url):
    time.sleep(1)
    return url
"#;

/// A package re-exporting with `from ._impl import *`, which reads the names it binds too.
const LISTED_INIT: &str = r#"from ._impl import *  # noqa: F403


async def inside(url):
    return fetch(url)  # EFF101
"#;

/// The names a star import binds read through the package: those of the module's `__all__`,
/// without one its public names, also down a chain of star imports, as the package's name over a
/// submodule, and a submodule an `__all__` lists. A name the module's `__all__` does not list, a
/// private name, a name read through an `__all__` heki cannot read (also through a module
/// star-importing that package), one two star imports bind to different objects, and one a
/// star import of a module outside the project may bind are not the module's function, nor is a
/// star-imported name over a submodule it is not read through.
const STAR_USERS: &str = r#"from pkg import listed
from pkg import public
from pkg.chain import deep
from pkg.coreparse import core
from pkg.coreparse import parse as core_parse
from pkg.dynamic import fetch as dynamic_fetch
from pkg.fallback import fetch as fallback_fetch
from pkg.listed import fetch, unlisted
from pkg.parsepkg import parse
from pkg.public import fetch as public_fetch
from pkg.rewrap import fetch as rewrapped_fetch
from pkg.shadowed import fetch as shadowed_fetch
from pkg.tools import tool


async def through_the_package(url):
    return fetch(url)  # EFF101


async def attribute_of_the_package(url):
    return listed.fetch(url)  # EFF101


async def not_in_all(url):
    return unlisted(url)


async def public_name(url):
    return public_fetch(url)  # EFF101


async def private_name(url):
    return public._hidden(url)


async def down_a_chain(url):
    return deep(url)  # EFF101


async def through_an_unreadable_all(url):
    return dynamic_fetch(url)


async def through_a_package_with_an_unreadable_all(url):
    return rewrapped_fetch(url)


async def bound_by_two_star_imports(url):
    return fallback_fetch(url)


async def after_a_star_import_from_outside(url):
    return shadowed_fetch(url)


async def submodule_function(x):
    return parse(x)  # EFF101


async def over_a_submodule(x):
    return core_parse(x)


async def a_submodule_of_the_package(x):
    return core.parse(x)  # EFF101


async def a_submodule_all_lists(x):
    return tool.parse(x)  # EFF101
"#;

/// Constants a star import binds, read in the importing module: a sleep of no time and a
/// `data:` URL end at once, as the literals would. Two star imports binding a name to two
/// constants leave its value open.
const STAR_CONSTANTS: &str = r#"import time
from urllib.request import urlopen

from pkg.settings import *  # noqa: F403
from pkg.more_settings import *  # noqa: F403


async def no_pause():
    time.sleep(PAUSE)


async def a_wait():
    time.sleep(WAIT)  # EFF101


async def local_page():
    urlopen(PAGE).read()


async def remote_page():
    urlopen(REMOTE).read()  # EFF101


async def bound_twice():
    time.sleep(TWICE)  # EFF101
"#;

#[test]
fn a_star_import_binds_the_names_of_the_module_it_imports() {
    let dynamic_impl = format!(
        "{STAR_IMPL}\n\n__all__ = [name for name in dir() if not name.startswith(\"_\")]\n"
    );
    check_marks(
        "star_imports",
        &[
            ("pkg/listed/__init__.py", LISTED_INIT),
            ("pkg/listed/_impl.py", LISTED_IMPL),
            ("pkg/public/__init__.py", "from ._impl import *\n"),
            ("pkg/public/_impl.py", STAR_IMPL),
            ("pkg/chain/__init__.py", "from .first import *\n"),
            ("pkg/chain/first.py", "from .second import *\n"),
            (
                "pkg/chain/second.py",
                "from .third import *\n\n__all__ = [\"deep\"]\n",
            ),
            ("pkg/chain/third.py", &STAR_IMPL.replace("fetch", "deep")),
            (
                "pkg/dynamic/__init__.py",
                "from ._impl import *\n\n\ndef size(url):\n    return len(url)\n",
            ),
            ("pkg/dynamic/_impl.py", &dynamic_impl),
            ("pkg/rewrap.py", "from pkg.dynamic import *\n"),
            (
                "pkg/fallback/__init__.py",
                "try:\n    from ._fast import *\nexcept ImportError:\n    from ._slow import *\n",
            ),
            ("pkg/fallback/_fast.py", "def fetch(url):\n    return url\n"),
            ("pkg/fallback/_slow.py", STAR_IMPL),
            ("pkg/fallback/fetch.py", ""),
            (
                "pkg/shadowed/__init__.py",
                "from ._impl import *\nfrom os.path import *\n",
            ),
            ("pkg/shadowed/_impl.py", STAR_IMPL),
            ("pkg/parsepkg/__init__.py", "from .parse import *\n"),
            ("pkg/parsepkg/parse.py", PARSE_MODULE),
            ("pkg/coreparse/__init__.py", "from .core import *\n"),
            ("pkg/coreparse/core.py", PARSE_MODULE),
            ("pkg/coreparse/parse.py", PARSE_MODULE),
            ("pkg/toolbox/__init__.py", "__all__ = [\"tool\"]\n"),
            ("pkg/toolbox/tool.py", PARSE_MODULE),
            ("pkg/tools.py", "from pkg.toolbox import *\n"),
            ("pkg/star_users.py", STAR_USERS),
            (
                "pkg/settings.py",
                "PAUSE = 0\nWAIT = 1\nPAGE = \"data:,x\"\nREMOTE = \"https://example.com\"\nTWICE = 0\n",
            ),
            ("pkg/more_settings.py", "TWICE = 0\n"),
            ("pkg/paced.py", STAR_CONSTANTS),
        ],
        &["EFF101"],
    );
}

/// A star import of a module heki could not read binds names it cannot name: the call is taken
/// for no function.
#[test]
fn a_star_import_of_an_unread_module_binds_open_names() {
    let p = Project::new("star_import_unread");
    p.write("pyproject.toml", CONFIG);
    p.write("pkg/__init__.py", "");
    p.write("pkg/star/__init__.py", "from ._impl import *\n");
    p.write("pkg/star/_impl.py", STAR_IMPL);
    p.write(
        "pkg/users.py",
        "from pkg.star import fetch\n\n\nasync def go(url):\n    return fetch(url)\n",
    );
    let (code, stdout, stderr) = p.check(&["--output-format", "concise"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stdout.contains("pkg/users.py:5:12: EFF101 "), "{stdout}");
    p.write("pkg/star/_impl.py", "print \"done\"\n");
    let (code, stdout, stderr) = p.check(&["--output-format", "concise"]);
    assert_eq!(code, 2, "{stdout}");
    assert!(
        stderr.contains("pkg/star/_impl.py:1:7: syntax error: "),
        "{stderr}"
    );
    assert!(!stdout.contains("EFF101"), "{stdout}");
}

/// The issue's shape: `heki inspect` follows a call of a name re-exported by a star import into
/// the project's function, read as an imported name and as an attribute of the package.
#[test]
fn inspect_follows_a_star_re_export() {
    let p = Project::new("star_reexport_inspect");
    p.write("pyproject.toml", CONFIG);
    p.write("pkg/__init__.py", "");
    p.write(
        "pkg/utils/_utils.py",
        "import requests\n\n__all__ = [\"fetch\"]\n\n\ndef fetch(url):\n    return requests.get(url)\n",
    );
    p.write("pkg/utils/__init__.py", "from ._utils import *\n");
    p.write(
        "pkg/m.py",
        "from .utils import fetch\nfrom . import utils\n\n\ndef a(url):\n    return fetch(url)\n\n\ndef b(url):\n    return utils.fetch(url)\n",
    );
    for function in ["pkg.m.a", "pkg.m.b"] {
        let (code, stdout, stderr) = p.heki(&["inspect", function]);
        assert_eq!(code, 0, "{stderr}");
        assert!(stdout.contains("impure: effects"), "{function}: {stdout}");
        assert!(
            stdout.contains("blocking_net (called), 2 calls down"),
            "{function}: {stdout}"
        );
    }
}

/// An argument unpacked with `*` binds the items of a display of known length one by one; past
/// one of unknown length (also one unpacked inside the display), the parameter an argument binds
/// is not known.
const UNPACKED: &str = r#"from pkg.model import Model


def fill(ignored, xs):
    xs.append(1)


def empty_pack(m: Model):
    fill(*(), None, m.items)  # MUT201


def one_item_pack(m: Model):
    fill(*(None,), m.items)  # MUT201


def whole_pack(m: Model):
    fill(*[None, m.items])  # MUT201


def first_slot(m: Model, rest):
    fill(m.items, *rest)


def nested_pack(m: Model, rest):
    fill(*(None, *rest), m.items)


def plain(m: Model):
    fill(m.items, None)
"#;

#[test]
fn unpacked_arguments_bind_the_items_of_known_displays() {
    check_marks(
        "unpacked_arguments",
        &[("pkg/model.py", MODEL), ("pkg/unpacked.py", UNPACKED)],
        &["MUT201"],
    );
}

/// A method named like a container mutator changes its receiver in place only when the
/// receiver's class does not define it: a project method's own summary decides.
const HOLDERS: &str = r#"class Stats:
    def clear(self):
        return None


class Tally(list):
    pass


class Counter:
    def __init__(self):
        self.seen = []

    def clear(self):
        self.seen.clear()


class Holder:
    def __init__(self):
        self.stats = Stats()
        self.tally = Tally()
        self.counter = Counter()


def reset(h: Holder):
    h.stats.clear()
    h.tally.clear()  # MUT201
    h.counter.clear()
"#;

#[test]
fn a_project_method_named_like_a_container_mutator_decides_itself() {
    check_marks(
        "named_mutators",
        &[("pkg/holders.py", HOLDERS)],
        &["MUT201"],
    );
}

/// An augmented assignment changes a built-in container in place (`items += ys` extends the list
/// a local name holds); on a copy or an immutable value it binds a new object.
const AUGMENTED: &str = r#"from pkg.model import Model


def extend_alias(m: Model):
    items = m.items
    items += [1]  # MUT201


def through_attribute(m: Model):
    m.items += [1]  # MUT201


def copied(m: Model):
    items = list(m.items)
    items += [1]


def counted(m: Model):
    n = len(m.items)
    n += 1
    return n
"#;

#[test]
fn augmented_assignments_change_containers_in_place() {
    check_marks(
        "augmented_assignments",
        &[("pkg/model.py", MODEL), ("pkg/augmented.py", AUGMENTED)],
        &["MUT201"],
    );
}

/// A mixin's method calling a method on its own `self` runs the implementation of the class
/// the receiver is: on a receiver of known class, only the implementations of classes related to
/// it count (an async client sharing the mixin with a sync one runs the async one).
const MIXINS: &str = r#"import time


class Commands:
    def masters(self):
        return self.execute_command("MASTERS")

    def relay(self):
        return self.masters()


class SyncClient(Commands):
    def execute_command(self, *args):
        time.sleep(1)


class SyncPipeline(SyncClient):
    pass


class AsyncClient(Commands):
    async def execute_command(self, *args):
        return args


class AsyncCommands(Commands):
    async def amasters(self):
        return self.masters()


async def on_async(c: AsyncClient):
    await c.masters()
    await c.relay()


async def on_sync(c: SyncClient):
    c.masters()  # EFF101 [heuristic]
    c.relay()  # EFF101 [heuristic]


async def on_sync_subclass(c: SyncPipeline):
    c.masters()  # EFF101 [heuristic]


async def on_any(c: Commands):
    c.masters()  # EFF101 [heuristic]


class Sentinel:
    def __init__(self):
        self.clients = [AsyncClient()]

    async def each(self):
        for c in self.clients:
            await c.masters()
"#;

#[test]
fn a_mixin_method_runs_the_implementations_of_its_receivers_class() {
    check_marks("mixins", &[("pkg/mixins.py", MIXINS)], &["EFF101"]);
}

/// An effect a method reaches through a subclass's implementation on its own `self` and also
/// through a plain call runs whatever `self` is; of two chains through one implementation, the
/// shorter one is kept.
const HOOKS: &str = r#"import time


def pause():
    time.sleep(1)


class Base:
    def run(self):
        self.flush()
        pause()

    def run_short_first(self):
        self.flush()
        self.slow_path()

    def run_long_first(self):
        self.slow_path()
        self.flush()

    def slow_path(self):
        self.flush()


class FileSink(Base):
    def flush(self):
        time.sleep(1)


class Quiet(Base):
    def flush(self):
        pass


async def on_quiet(q: Quiet):
    q.run()  # EFF101
    q.run_short_first()
    q.run_long_first()


async def on_file(f: FileSink):
    f.run_short_first()  # EFF101 [heuristic]
    f.run_long_first()  # EFF101 [heuristic]
"#;

#[test]
fn an_effect_through_self_and_a_plain_call_runs_for_every_receiver() {
    check_marks("self_hooks", &[("pkg/hooks.py", HOOKS)], &["EFF101"]);
    let p = Project::new("self_hooks_chains");
    p.write("pkg/__init__.py", "").write("pkg/hooks.py", HOOKS);
    let (_, stdout, _) = p.check(&["--select", "EFF101", "--output-format", "json"]);
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    // `(line of the report, line of the hop into `FileSink.flush`)`.
    let hops: Vec<(u64, u64)> = diags
        .iter()
        .filter(|d| d["qualname"] == "on_file")
        .map(|d| {
            (
                d["line"].as_u64().unwrap(),
                d["related"][1]["line"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(hops, [(42, 14), (43, 19)], "{diags:#?}");
}

/// A helper calling a method on its parameter runs a subclass's override only when the argument
/// may be an instance of the subclass: an object the caller constructs by calling the class by
/// name (directly, through a copy, through its module) is exactly that class, and so is the
/// object such a construction hands its `__init__`; calling it or reading its property runs its
/// class's implementations alone. A parameter, `cls(...)`, a factory's result, or an instance of
/// the subclass may be one. The helpers handing the parameter on keep the question for their
/// callers (`EffectSummary::provided_effects`), also where one subclass's override reaches
/// another's on what it is handed (`Rack.put` on a factory's result).
const EXACT_CORE: &str = r#"class Notes:
    def __init__(self):
        self.setup()

    def setup(self):
        pass

    def preview(self):
        return 1

    def run(self):
        return self.preview()

    def __call__(self):
        return 1

    @property
    def size(self):
        return 1


def prepare(notes: Notes):
    return notes.preview()


def make() -> Notes:
    return Notes()


class Shelf:
    def put(self, notes: Notes):
        return 1


def store(shelf: Shelf):
    return shelf.put(make())


def hand_on(notes: Notes):
    return prepare(notes)


async def concrete():
    prepare(Notes())


async def from_argument(notes: Notes):
    prepare(notes)  # EFF101 [heuristic]


async def direct():
    Notes().preview()


async def copied():
    n = Notes()
    n.run()


async def handed_on():
    hand_on(Notes())


async def handed_on_open(notes: Notes):
    hand_on(notes)  # EFF101 [heuristic]


async def constructed():
    return Notes()


async def called():
    Notes()()


async def sized():
    return Notes().size


async def made():
    prepare(make())  # EFF101 [heuristic]


async def stored(shelf: Shelf):
    store(shelf)  # EFF101 [heuristic]


async def stored_exact():
    store(Shelf())


class Maker:
    @classmethod
    async def make(cls):
        prepare(cls())  # EFF101 [heuristic]
"#;

const EXACT_GATE: &str = r#"import time

from pkg import core
from pkg.core import Notes, prepare


class Gate(Notes):
    def setup(self):
        time.sleep(1)

    def preview(self):
        time.sleep(1)

    def __call__(self):
        time.sleep(1)

    @property
    def size(self):
        time.sleep(1)
        return 1


class Rack(core.Shelf):
    def put(self, notes: Notes):
        return notes.preview()


async def subclass():
    g = Gate()  # EFF101 [heuristic]
    prepare(g)  # EFF101 [heuristic]


async def through_module():
    core.prepare(core.Notes())
"#;

/// A class with a `__new__` of its own may hand out an instance of another class, and a class
/// name the project binds to another class may construct that one: neither construction is
/// exactly the class.
const NOT_EXACT: &str = r#"import time


class Plain:
    def run(self):
        return 1


class Slow(Plain):
    def run(self):
        time.sleep(1)


class Pooled(Plain):
    def __new__(cls):
        return object.__new__(Slow)


def call(p: Plain):
    return p.run()


async def made():
    call(Plain())  # EFF101 [heuristic]


async def pooled():
    call(Pooled())  # EFF101 [heuristic]


def patch():
    global Plain
    Plain = Slow
"#;

#[test]
fn an_exact_instance_runs_only_its_classs_implementations() {
    check_marks(
        "exact_instances",
        &[("pkg/core.py", EXACT_CORE), ("pkg/gate.py", EXACT_GATE)],
        &["EFF101"],
    );
    check_marks("not_exact", &[("pkg/plain.py", NOT_EXACT)], &["EFF101"]);
}

/// A class whose bases C3 cannot linearize keeps the depth-first, left-to-right order: a base
/// order its subclass contradicts (Python rejects the class statement, as the test expects),
/// and bases that name the class itself (a class extending an imported class of its own name
/// with a mixin).
const UNORDERED: &str = r#"import time

import pytest

from pkg.parsers import Parser


class Mixin:
    def extra(self):
        time.sleep(1)


class Parser(Parser, Mixin):
    pass


async def on_parser(p: Parser):
    p.extra()  # EFF101


class X:
    def read(self):
        time.sleep(1)


class Y:
    def read(self):
        pass


class A(X, Y):
    pass


class B(Y, X):
    pass


def rejects_an_inconsistent_order():
    with pytest.raises(TypeError):

        class Both(A, B):
            async def run(self):
                self.read()  # EFF101
"#;

#[test]
fn bases_c3_cannot_linearize_keep_the_depth_first_order() {
    check_marks(
        "unordered_bases",
        &[
            (
                "pkg/parsers.py",
                "class Parser:\n    def parse(self):\n        return None\n",
            ),
            ("pkg/unordered.py", UNORDERED),
        ],
        &["EFF101"],
    );
}

/// A class method's `cls` stays any subclass of the class also where one path rebinds it to the
/// class itself: constructing it runs any subclass's `__init__`. The class's own name constructs
/// exactly that class.
const CLASS_FACTORIES: &str = r#"import time


class Model:
    def __init__(self):
        pass

    @classmethod
    def build(cls, base):
        if base:
            cls = Model
        return cls()

    @classmethod
    def plain(cls):
        return Model()


class Slow(Model):
    def __init__(self):
        time.sleep(1)


async def make():
    Slow.build(False)  # EFF101 [heuristic]
    Slow.plain()
"#;

#[test]
fn a_class_method_constructs_any_subclass_through_cls() {
    check_marks(
        "class_factories",
        &[("pkg/factories.py", CLASS_FACTORIES)],
        &["EFF101"],
    );
}

/// The call chain of a report runs for the receiver's class: an effect a hook reaches only in a
/// subclass's override is not the path shown for a receiver of another subclass.
#[test]
fn a_chain_runs_for_the_receivers_class() {
    let p = Project::new("receiver_chains");
    p.write("pyproject.toml", CONFIG);
    p.write("pkg/__init__.py", "");
    p.write(
        "pkg/hooks.py",
        r#"import time


def pause():
    time.sleep(1)


class Base:
    def run(self):
        self.flush()
        pause()


class FileSink(Base):
    def flush(self):
        time.sleep(1)


class Quiet(Base):
    def flush(self):
        pass


async def on_quiet(q: Quiet):
    q.run()


async def on_file(f: FileSink):
    f.run()
"#,
    );
    let (_, stdout, _) = p.check(&["--output-format", "json"]);
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let chain = |line: u64| -> Vec<String> {
        let d = diags
            .iter()
            .find(|d| d["line"] == line && d["rule_code"] == "EFF101")
            .unwrap_or_else(|| panic!("no EFF101 at {line}: {diags:#?}"));
        d["related"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["message"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(
        chain(25),
        vec![
            "`on_quiet` calls `pkg.hooks.Base.run`",
            "`Base.run` calls `pkg.hooks.pause`",
            "`pause` calls `time.sleep`",
        ]
    );
    assert_eq!(chain(29).len(), 3);
}

/// A class statement evaluates its bases before it binds its own name: `class Parser(Parser)`
/// derives from the `Parser` the module imported, whose methods it inherits. A base the module
/// rebound before the class statement is the new binding.
const SHADOWING_BASES: &str = r#"import time

from pkg.parser_base import Parser


class Mixin:
    def extra(self):
        time.sleep(1)


class Parser(Parser, Mixin):
    def own(self):
        return 2


async def through_both(p: Parser):
    p.extra()  # EFF101
    p.parse()  # EFF101
"#;

/// A base reached through a module the class's own name shadows.
const SHADOWING_MODULE: &str = r#"from pkg import parser_base


class parser_base(parser_base.Parser):
    pass


async def through_module(p: parser_base):
    p.parse()  # EFF101
"#;

/// Classes deriving from each other (Python rejects the second statement): the MRO walks
/// each class once.
const CYCLE: &str = r#"import time


class Mixin:
    def read(self):
        time.sleep(1)


class A(B, Mixin):
    pass


class B(A):
    pass


async def on_cycle(a: A):
    a.read()  # EFF101
"#;

const REBOUND_FIRST: &str = r#"from pkg.parser_base import Parser


class Parser:
    def parse(self):
        return 1


class Child(Parser):
    pass


async def through_local(c: Child):
    c.parse()
"#;

const PARSER_BASE: &str = r#"import time


class Parser:
    def parse(self):
        time.sleep(1)
"#;

#[test]
fn a_class_derives_from_the_import_its_own_name_shadows() {
    check_marks(
        "shadowing_bases",
        &[
            ("pkg/parser_base.py", PARSER_BASE),
            ("pkg/shadowing.py", SHADOWING_BASES),
            ("pkg/shadowing_module.py", SHADOWING_MODULE),
            ("pkg/rebound.py", REBOUND_FIRST),
            ("pkg/cycle.py", CYCLE),
        ],
        &["EFF101"],
    );
}

const SCOPED_BASE_CLASSES: &str = r#"import time


class Parser:
    def parse(self):
        time.sleep(1)


class Quiet:
    def parse(self):
        return 1


class Calm:
    def parse(self):
        return 1
"#;

/// A class statement in a function takes each base as the value it passes there: a parameter
/// of the imported class's name is another class, whose methods the class does not inherit and
/// whose subclasses it is not; the class the function reads from the module is the base.
const SCOPED_BASES: &str = r#"import time

from pkg.base_classes import Calm, Parser, Quiet


async def through_a_parameter(Parser):
    class Local(Parser):
        pass

    Local().parse()


async def through_the_module():
    class Local(Parser):
        pass

    Local().parse()  # EFF101


def overriding_a_parameter(Quiet):
    class Loud(Quiet):
        def parse(self):
            time.sleep(1)

    return Loud


async def through_quiet(q: Quiet):
    q.parse()


def overriding_the_module_class():
    class Loud(Calm):
        def parse(self):
            time.sleep(1)

    return Loud


async def through_calm(c: Calm):
    c.parse()  # EFF101 [heuristic]
"#;

/// A function too large to model holding a class statement: heki does not know the value it
/// passes as a base, here a parameter of the imported class's name.
fn unmodeled_class_statement() -> String {
    let mut lines = vec![
        "import time".to_owned(),
        String::new(),
        "from pkg.base_classes import Quiet".to_owned(),
        String::new(),
        String::new(),
        "def huge(Quiet, xs):".to_owned(),
        "    class Huge(Quiet):".to_owned(),
        "        def parse(self):".to_owned(),
        "            time.sleep(1)".to_owned(),
        String::new(),
    ];
    let mut pad = "    ".to_owned();
    for i in 0..14 {
        lines.push(format!("{pad}try:"));
        lines.push(format!("{pad}    if xs:"));
        lines.push(format!("{pad}        return {i}"));
        lines.push(format!("{pad}    print(xs)"));
        lines.push(format!("{pad}finally:"));
        pad.push_str("    ");
    }
    lines.push(format!("{pad}print(Huge)"));
    format!("{}\n", lines.join("\n"))
}

#[test]
fn a_class_statement_in_a_function_names_its_bases_there() {
    check_marks(
        "scoped_bases",
        &[
            ("pkg/base_classes.py", SCOPED_BASE_CLASSES),
            ("pkg/scoped.py", SCOPED_BASES),
            ("pkg/huge.py", &unmodeled_class_statement()),
        ],
        &["EFF101"],
    );
}

/// Bases heki names no class for are classes of their own: in `Leaf(Left, Right)`, with an
/// unknown base before `Root` in `Left` and after it in `Right`, the C3 linearization puts
/// `Right` before `Root`, so `Right.parse` overrides `Root.parse` as in Python.
const UNKNOWN_BASES_APART: &str = r#"import time


def make_a():
    return object


def make_b():
    return object


class Root:
    def parse(self):
        time.sleep(1)


class Left(make_a(), Root):
    pass


class Right(Root, make_b()):
    def parse(self):
        return 1


class Leaf(Left, Right):
    pass


async def through_leaf(x: Leaf):
    x.parse()


class Quiet:
    def parse(self):
        return 1


class Up(make_a(), Quiet):
    pass


class Down(Quiet, make_b()):
    def parse(self):
        time.sleep(1)


class Bottom(Up, Down):
    pass


async def through_bottom(x: Bottom):
    x.parse()  # EFF101
"#;

#[test]
fn unknown_bases_never_order_each_other() {
    check_marks(
        "unknown_bases_apart",
        &[("pkg/diamond.py", UNKNOWN_BASES_APART)],
        &["EFF101"],
    );
}

/// Calling an `async def` gives a coroutine, and awaiting it gives what the function's
/// annotation says it returns: the awaited store's method runs, a coroutine has no `save`, and
/// awaiting either of two coroutines gives either result, also for a coroutine kept in an
/// attribute or a global and awaited in another function.
const AWAITED: &str = r#"import time


class Store:
    def save(self):
        time.sleep(1)


async def open_store() -> Store:
    return Store()


class Pool:
    async def acquire(self) -> Store:
        return Store()


class Cache:
    def save(self):
        time.sleep(1)


async def open_cache() -> Cache:
    return Cache()


async def awaited(pool: Pool):
    store = await open_store()
    store.save()  # EFF101
    other = await pool.acquire()
    other.save()  # EFF101
    pending = open_store()
    ready = await pending
    ready.save()  # EFF101


async def either(flag):
    pending = open_store() if flag else open_cache()
    obj = await pending
    obj.save()  # EFF101


class Holder:
    def __init__(self):
        self.pending = open_store()
        self.factory = open_store

    async def run(self):
        store = await self.pending
        store.save()  # EFF101


ready = open_store()
factory = open_store


async def use_global():
    store = await ready
    store.save()  # EFF101


async def never_awaited(pool: Pool):
    store = open_store()
    store.save()
    other = pool.acquire()
    other.save()
"#;

/// A project coroutine and a catalogued awaitable (typed by what awaiting it gives) merged
/// before the `await`: the awaited connection runs a query per item either way.
const MIXED_AWAITED: &str = r#"import psycopg


async def connect() -> psycopg.AsyncConnection:
    return await psycopg.AsyncConnection.connect("dbname=t")


async def load(ids, flag):
    pending = connect() if flag else psycopg.AsyncConnection.connect("dbname=t")
    conn = await pending
    for i in ids:
        await conn.execute("SELECT 1")  # EFF103


async def each_awaited(ids, flag):
    conn = await connect() if flag else await psycopg.AsyncConnection.connect("dbname=t")
    for i in ids:
        await conn.execute("SELECT 1")  # EFF103
"#;

#[test]
fn a_call_of_an_async_def_is_a_coroutine_of_what_awaiting_it_gives() {
    check_marks(
        "awaited",
        &[
            ("pkg/awaited.py", AWAITED),
            ("pkg/mixed_awaited.py", MIXED_AWAITED),
        ],
        &["EFF101", "EFF103"],
    );
}

/// A module global's type is what every store binds it to: the module body's, a function's
/// after `global client`, and another module's. A store of another class, of a value of unknown
/// type, or through `globals()` leaves the call unresolved; one of the same class, or `None` (on
/// which the call raises), keeps it. An annotated global keeps its declaration.
const REBOUND_GLOBALS: &str = r#"import httpx

from pkg.factory import make

client = httpx.Client()


def configure() -> None:
    global client
    client = httpx.AsyncClient()


async def rebound() -> None:
    client.get("https://example.com")


same = httpx.Client()


def reset() -> None:
    global same
    same = httpx.Client(timeout=5)


def close() -> None:
    global same
    same.close()
    same = None


async def kept() -> None:
    same.get("https://example.com")  # EFF101


opened = httpx.Client()


def reopen(factory) -> None:
    global opened
    opened = factory()


async def unknown() -> None:
    opened.get("https://example.com")


declared: httpx.Client = httpx.Client()


def redeclare() -> None:
    global declared
    declared = make()


async def annotated() -> None:
    declared.get("https://example.com")  # EFF101


def nested() -> None:
    def inner() -> None:
        global inner_client
        inner_client = httpx.AsyncClient()

    inner()


inner_client = httpx.Client()


async def through_nested() -> None:
    inner_client.get("https://example.com")


elsewhere = httpx.Client()


async def from_another_module() -> None:
    elsewhere.get("https://example.com")


pool = [httpx.Client()]


def drop_first() -> None:
    global pool
    pool = pool[1:]


async def each() -> None:
    for client in pool:
        client.get("https://example.com")  # EFF101
"#;

const REBINDER: &str = r#"import httpx

from pkg import rebound
from pkg.rebound import same


def rebind() -> None:
    rebound.elsewhere = httpx.AsyncClient()
    rebound.same = httpx.Client(timeout=1)


async def read_across() -> None:
    rebound.client.get("https://example.com")
    same.get("https://example.com")  # EFF101
    rebound.same.get("https://example.com")  # EFF101
"#;

const FACTORY: &str = r#"import httpx


def make() -> httpx.Client:
    return httpx.Client()
"#;

/// `globals()` may store any global of its module.
const DYNAMIC_GLOBALS: &str = r#"import httpx

client = httpx.Client()


def configure(name, value) -> None:
    globals()[name] = value


async def handler() -> None:
    client.get("https://example.com")
"#;

/// A star import may rebind the names the module body stores before it, not those it stores
/// after it on every path; one under `if TYPE_CHECKING:` never runs.
const STAR_GLOBALS: &str = r#"from typing import TYPE_CHECKING

import httpx

before = httpx.Client()

from pkg.factory import *  # noqa: E402, F403

after = httpx.Client()
checked = httpx.Client()

if TYPE_CHECKING:
    from pkg.factory import *  # noqa: F403


async def read_before() -> None:
    before.get("https://example.com")


async def read_after() -> None:
    after.get("https://example.com")  # EFF101


async def read_checked() -> None:
    checked.get("https://example.com")  # EFF101
"#;

/// A star import on one branch leaves the names the other branch stores to what it binds; so is
/// a string constant bound there no constant, and `setattr` through it stores an attribute heki
/// cannot name. A store after both branches binds the name for good.
const STAR_BRANCHES: &str = r#"import os

import httpx

if os.environ.get("PLUGIN"):
    from pkg.factory import *  # noqa: F403
else:
    branched = httpx.Client()
    KEY = "other"

LISTED = "other"


async def read_branched() -> None:
    branched.get("https://example.com")


class Keyed:
    def __init__(self) -> None:
        self.client = httpx.Client()
        self.other = None

    def configure(self) -> None:
        setattr(self, KEY, httpx.AsyncClient())

    async def handle(self) -> None:
        self.client.get("https://example.com")


class Listed:
    def __init__(self) -> None:
        self.client = httpx.Client()
        self.other = None

    def configure(self) -> None:
        setattr(self, LISTED, httpx.AsyncClient())

    async def handle(self) -> None:
        self.client.get("https://example.com")  # EFF101
"#;

/// A fallback an `except` clause binds when the star import fails.
const STAR_FALLBACK: &str = r#"import httpx

try:
    from pkg.factory import *  # noqa: F403
except ImportError:
    fallback = httpx.Client()


async def read_fallback() -> None:
    fallback.get("https://example.com")
"#;

#[test]
fn a_global_has_the_type_every_store_of_it_binds() {
    check_marks(
        "rebound_globals",
        &[
            ("pkg/rebound.py", REBOUND_GLOBALS),
            ("pkg/rebinder.py", REBINDER),
            ("pkg/factory.py", FACTORY),
            ("pkg/dynamic_globals.py", DYNAMIC_GLOBALS),
            ("pkg/star_globals.py", STAR_GLOBALS),
            ("pkg/star_branches.py", STAR_BRANCHES),
            ("pkg/star_fallback.py", STAR_FALLBACK),
        ],
        &["EFF101"],
    );
}

/// An instance attribute `__init__` binds has the type every store into an instance agrees
/// on: of the class's other methods, of a subclass's, through `setattr` or the instance's
/// namespace, and of a function nested in a method through its closure cell (into what the cell
/// holds when the method rebinds `self`). A store of the
/// same class, of `None`, of a value computed from the attribute that keeps its type, or one
/// that only `__init__` makes keeps it, and so does an annotation; a namespace written under a
/// key naming another attribute does too.
const REBOUND_ATTRS: &str = r#"import httpx


class Swapped:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def swap(self) -> None:
        self.client = httpx.AsyncClient()

    async def handle(self) -> None:
        self.client.get("https://example.com")


class Kept:
    def __init__(self) -> None:
        self.client = httpx.Client()
        self.other = self.client

    def reset(self) -> None:
        self.client = httpx.Client(timeout=5)

    def close(self) -> None:
        self.client.close()
        self.client = None

    async def handle(self) -> None:
        self.client.get("https://example.com")  # EFF101


class OnlyInit:
    def __init__(self) -> None:
        self.client = httpx.Client()

    async def handle(self) -> None:
        self.client.get("https://example.com")  # EFF101


async def through_parameter(a: Swapped, b: OnlyInit) -> None:
    a.client.get("https://example.com")
    b.client.get("https://example.com")  # EFF101


class Declared:
    def __init__(self) -> None:
        self.client: httpx.Client = httpx.Client()

    def swap(self, other) -> None:
        self.client = other

    async def handle(self) -> None:
        self.client.get("https://example.com")  # EFF101


class Base:
    def __init__(self) -> None:
        self.client = httpx.Client()

    async def handle(self) -> None:
        self.client.get("https://example.com")


class AsyncChild(Base):
    def __init__(self) -> None:
        super().__init__()
        self.client = httpx.AsyncClient()


class SetAttr:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def swap(self) -> None:
        setattr(self, "client", httpx.AsyncClient())

    async def handle(self) -> None:
        self.client.get("https://example.com")


class Options:
    def __init__(self, **options) -> None:
        self.client = httpx.Client()
        for name, value in options.items():
            setattr(self, name, value)

    async def handle(self) -> None:
        self.client.get("https://example.com")


class Namespace:
    def __init__(self, state) -> None:
        self.client = httpx.Client()
        self.__dict__.update(state)

    def __repr__(self) -> str:
        return f"{self.__dict__}"

    async def handle(self) -> None:
        self.client.get("https://example.com")


class ReadsNamespace:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def __repr__(self) -> str:
        return f"{self.__dict__} {vars(self).get('client')}"

    async def handle(self) -> None:
        self.client.get("https://example.com")  # EFF101


class Callback:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def on_reconnect(self):
        def swap() -> None:
            self.client = httpx.AsyncClient()

        return swap

    async def handle(self) -> None:
        self.client.get("https://example.com")


class Lent:
    def __init__(self) -> None:
        self.client = httpx.Client()

    async def handle(self) -> None:
        self.client.get("https://example.com")


class Lender:
    def lend(self, other: Lent) -> None:
        def swap() -> None:
            self.client = httpx.AsyncClient()

        self = other
        swap()


class KeyedNamespace:
    def __init__(self) -> None:
        self.client = httpx.Client()
        self.__dict__.setdefault("seen", set())

    async def handle(self) -> None:
        self.client.get("https://example.com")  # EFF101


class Trimmed:
    def __init__(self) -> None:
        self.clients = [httpx.Client()]

    def drop_first(self) -> None:
        self.clients = self.clients[1:]

    def snapshot(self) -> None:
        self.clients = self.clients.copy()

    async def handle(self) -> None:
        for client in self.clients:
            client.get("https://example.com")  # EFF101


class Replaced:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def fetch(self) -> None:
        self.client = self.client.get("https://example.com")

    async def handle(self) -> None:
        self.client.get("https://example.com")
"#;

#[test]
fn an_attribute_has_the_type_every_store_into_an_instance_binds() {
    check_marks(
        "rebound_attrs",
        &[("pkg/rebound_attrs.py", REBOUND_ATTRS)],
        &["EFF101"],
    );
}

/// `setattr` and `vars` called on their module (`builtins.setattr(self, name, v)`) are the
/// builtins, and `__setattr__` called on a project class stores as `object.__setattr__` does; a
/// function of a project module that is also named `setattr` stores nothing.
const QUALIFIED_BUILTINS: &str = r#"import builtins

import httpx

from pkg import helpers


class ThroughModule:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def swap(self) -> None:
        builtins.setattr(self, "client", httpx.AsyncClient())

    async def handle(self) -> None:
        self.client.get("https://example.com")


class ThroughModuleVars:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def swap(self) -> None:
        builtins.vars(self)["client"] = httpx.AsyncClient()

    async def handle(self) -> None:
        self.client.get("https://example.com")


class Plain:
    def __init__(self) -> None:
        self.client = httpx.Client()

    async def handle(self) -> None:
        self.client.get("https://example.com")


class ThroughClass(Plain):
    def swap(self) -> None:
        Plain.__delattr__(self, "client")
        Plain.__setattr__(self, "client", httpx.AsyncClient())


class ThroughHelper:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def swap(self) -> None:
        helpers.setattr(self, "client", httpx.AsyncClient())

    async def handle(self) -> None:
        self.client.get("https://example.com")  # EFF101
"#;

const HELPERS: &str = r#"def setattr(obj, name, value) -> None:
    print(obj, name, value)
"#;

#[test]
fn a_builtin_called_on_its_module_stores_as_the_builtin() {
    check_marks(
        "qualified_builtins",
        &[
            ("pkg/qualified_builtins.py", QUALIFIED_BUILTINS),
            ("pkg/helpers.py", HELPERS),
        ],
        &["EFF101"],
    );
}

/// A store reading another attribute (`self.active = self.client`) binds what reading it gives
/// once every store is joined: the join over the class's family, here and in another module.
const READ_ATTRS: &str = r#"import httpx


class Base:
    def __init__(self) -> None:
        self.client = httpx.Client()
        self.active = self.client

    def activate(self) -> None:
        self.active = self.client

    async def handle(self) -> None:
        self.active.get("https://example.com")


class Child(Base):
    def swap(self) -> None:
        self.client = httpx.AsyncClient()


class Alone:
    def __init__(self) -> None:
        self.client = httpx.Client()
        self.active = self.client

    def activate(self) -> None:
        self.active = self.client

    async def handle(self) -> None:
        self.active.get("https://example.com")  # EFF101


class Extended:
    def __init__(self) -> None:
        self.client = httpx.Client()
        self.active = self.client

    async def handle(self) -> None:
        self.active.get("https://example.com")
"#;

const EXTENDING: &str = r#"import httpx

from pkg.read_attrs import Extended


class Swapping(Extended):
    def swap(self) -> None:
        self.client = httpx.AsyncClient()
"#;

/// A store reading another global binds what reading it gives with the stores of other
/// modules joined.
const READ_GLOBALS: &str = r#"import httpx

clients = [httpx.Client()]
active = [httpx.Client()]


def activate() -> None:
    global active
    active = clients[:]


async def handle() -> None:
    for c in active:
        c.get("https://example.com")


kept = [httpx.Client()]
current = [httpx.Client()]


def use_kept() -> None:
    global current
    current = kept[:]


async def handle_kept() -> None:
    for c in current:
        c.get("https://example.com")  # EFF101
"#;

const SWAPS_GLOBALS: &str = r#"import httpx

import pkg.read_globals


def swap() -> None:
    pkg.read_globals.clients = [httpx.AsyncClient()]
"#;

#[test]
fn a_store_reading_a_name_binds_what_every_store_of_that_name_gives() {
    check_marks(
        "read_names",
        &[
            ("pkg/read_attrs.py", READ_ATTRS),
            ("pkg/extending.py", EXTENDING),
            ("pkg/read_globals.py", READ_GLOBALS),
            ("pkg/swaps_globals.py", SWAPS_GLOBALS),
        ],
        &["EFF101"],
    );
}

/// Another module stores into a global through any receiver the inference types as the
/// module: an import in a function, `setattr`, and the module's namespace. A store of the same
/// class keeps the type.
const STORED_INTO: &str = r#"import httpx

local_import = httpx.Client()
by_setattr = httpx.Client()
by_namespace = httpx.Client()
same = httpx.Client()


async def handle() -> None:
    local_import.get("https://example.com")
    by_setattr.get("https://example.com")
    by_namespace.get("https://example.com")
    same.get("https://example.com")  # EFF101
"#;

const STORES_INTO: &str = r#"import httpx

import pkg.stored_into
from pkg import stored_into as cfg


def through_local_import() -> None:
    import pkg.stored_into

    pkg.stored_into.local_import = httpx.AsyncClient()


def through_setattr() -> None:
    setattr(pkg.stored_into, "by_setattr", httpx.AsyncClient())


def through_namespace() -> None:
    vars(cfg)["by_namespace"] = httpx.AsyncClient()
    vars(cfg)["same"] = httpx.Client(timeout=1)
"#;

#[test]
fn another_module_stores_into_a_global_through_any_receiver_of_its_module() {
    check_marks(
        "stored_into",
        &[
            ("pkg/stored_into.py", STORED_INTO),
            ("pkg/stores_into.py", STORES_INTO),
        ],
        &["EFF101"],
    );
}

/// Stores into an instance the inference can type: `self.__dict__ = state` stores any
/// attribute, and so does replacing the namespace or the class through `object.__setattr__` or
/// `setattr`; `super().__setattr__`, an alias of `object.__setattr__`, a `Final` name, and a
/// keyword of the namespace's `update` are named stores; deletions store nothing; a class
/// method's new instance and an annotated parameter are instances of the class.
const RECEIVERS: &str = r#"from typing import Final

import httpx

_set = object.__setattr__
KEY: Final = "client"


class DictAssign:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def load(self, state) -> None:
        self.__dict__ = state

    async def handle(self) -> None:
        self.client.get("https://example.com")


class FrozenDict:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def load(self, state) -> None:
        object.__setattr__(self, "__dict__", state)

    async def handle(self) -> None:
        self.client.get("https://example.com")


class ClassSwap:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def become(self, cls) -> None:
        setattr(self, "__class__", cls)

    async def handle(self) -> None:
        self.client.get("https://example.com")


class SuperSetattr:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def swap(self) -> None:
        super().__setattr__("client", httpx.AsyncClient())

    async def handle(self) -> None:
        self.client.get("https://example.com")


class AliasSet:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def swap(self) -> None:
        _set(self, "client", httpx.AsyncClient())

    async def handle(self) -> None:
        self.client.get("https://example.com")


class FinalName:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def swap(self) -> None:
        setattr(self, KEY, httpx.Client(timeout=1))

    async def handle(self) -> None:
        self.client.get("https://example.com")  # EFF101


class Factory:
    def __init__(self) -> None:
        self.client = httpx.Client()

    @classmethod
    def make_async(cls) -> "Factory":
        inst = cls()
        inst.client = httpx.AsyncClient()
        return inst

    async def handle(self) -> None:
        self.client.get("https://example.com")


class Helper:
    def __init__(self) -> None:
        self.client = httpx.Client()
        use_async(self)

    async def handle(self) -> None:
        self.client.get("https://example.com")


def use_async(obj: Helper) -> None:
    obj.client = httpx.AsyncClient()


class KeywordUpdate:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def swap(self) -> None:
        self.__dict__.update(client=httpx.AsyncClient())

    async def handle(self) -> None:
        self.client.get("https://example.com")


class Deleted:
    def __init__(self) -> None:
        self.client = httpx.Client()
        self.other = httpx.Client()

    def close(self) -> None:
        delattr(self, "other")
        object.__delattr__(self, "other")
        self.__dict__.pop("other", None)

    async def handle(self) -> None:
        self.client.get("https://example.com")  # EFF101


class Reset:
    def __init__(self) -> None:
        self.client = httpx.Client()

    async def handle(self) -> None:
        self.client.get("https://example.com")  # EFF101


def reset(obj: Reset) -> None:
    obj.client = httpx.Client(timeout=1)
"#;

const ELSEWHERE: &str = r#"import httpx

from pkg.receivers import Reset


class Owner:
    def __init__(self) -> None:
        self.client = httpx.Client()

    async def handle(self) -> None:
        self.client.get("https://example.com")


def swap(reset: Reset) -> None:
    reset.client = httpx.Client(timeout=2)
"#;

const OTHER_OWNER: &str = r#"import httpx

from pkg.elsewhere import Owner


def swap(owner: Owner) -> None:
    owner.client = httpx.AsyncClient()
"#;

/// Instances another module keeps in an attribute or a global, which only the stores of that
/// attribute or global type.
const HELD: &str = r#"import httpx


class Held:
    def __init__(self) -> None:
        self.client = httpx.Client()

    async def handle(self) -> None:
        self.client.get("https://example.com")


class HeldGlobally:
    def __init__(self) -> None:
        self.client = httpx.Client()

    async def handle(self) -> None:
        self.client.get("https://example.com")


class HeldPastTheRounds:
    def __init__(self) -> None:
        self.client = httpx.Client()

    async def handle(self) -> None:
        self.client.get("https://example.com")


class Kept:
    def __init__(self) -> None:
        self.client = httpx.Client()

    async def handle(self) -> None:
        self.client.get("https://example.com")  # EFF101
"#;

const KEEPERS: &str = r#"import httpx

from pkg.held import Held, HeldGlobally, Kept


class Holder:
    def __init__(self) -> None:
        self.held = Held()
        self.kept = Kept()

    def swap(self) -> None:
        self.held.client = httpx.AsyncClient()
        self.kept.other = httpx.AsyncClient()


holder = HeldGlobally()


def swap() -> None:
    holder.client = httpx.AsyncClient()
"#;

/// The stores of `__init__` read each attribute the one before binds, past the rounds the types
/// of the module's stores take: the first round's types hold, and they type `self.held`.
const KEEPER_PAST_THE_ROUNDS: &str = r#"import httpx

from pkg.held import HeldPastTheRounds, Kept


class Holder:
    def __init__(self) -> None:
        self.a0 = httpx.Client()
        self.a1 = self.a0
        self.a2 = self.a1
        self.a3 = self.a2
        self.a4 = self.a3
        self.a5 = self.a4
        self.held = HeldPastTheRounds()
        self.kept = Kept()

    def swap(self) -> None:
        self.held.client = httpx.AsyncClient()
        self.kept.other = httpx.AsyncClient()
"#;

#[test]
fn stores_into_an_instance_through_any_receiver_the_inference_types() {
    check_marks(
        "receivers",
        &[
            ("pkg/receivers.py", RECEIVERS),
            ("pkg/elsewhere.py", ELSEWHERE),
            ("pkg/other_owner.py", OTHER_OWNER),
            ("pkg/held.py", HELD),
            ("pkg/keepers.py", KEEPERS),
            ("pkg/keeper_past_the_rounds.py", KEEPER_PAST_THE_ROUNDS),
        ],
        &["EFF101"],
    );
}

/// `__setstate__` restores what an instance held when pickle or copy took the state: unless
/// the family builds the state otherwise (`__reduce__`, a `__getstate__` reading anything but
/// the instance, on one branch or in one of its definitions) or project code calls
/// `__setstate__` itself.
const RESTORED: &str = r#"import sys

import httpx


class Restored:
    __attrs__ = ["client"]

    def __init__(self) -> None:
        self.client = httpx.Client()
        self.active = self.client

    def __getstate__(self):
        return {attr: getattr(self, attr, None) for attr in self.__attrs__}

    def __setstate__(self, state) -> None:
        for attr, value in state.items():
            setattr(self, attr, value)

    async def handle(self) -> None:
        self.client.get("https://example.com")  # EFF101


class Namespace:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def __setstate__(self, state) -> None:
        self.__dict__.update(state)

    async def handle(self) -> None:
        self.client.get("https://example.com")  # EFF101


class Reduced:
    def __init__(self) -> None:
        self.client = httpx.Client()
        self.active = self.client

    def __reduce__(self):
        return (Reduced, (), {"client": httpx.AsyncClient()})

    def __setstate__(self, state) -> None:
        self.__dict__.update(state)

    async def handle(self) -> None:
        self.client.get("https://example.com")


class Built:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def __getstate__(self):
        return {"client": httpx.AsyncClient()}

    def __setstate__(self, state) -> None:
        self.__dict__.update(state)

    async def handle(self) -> None:
        self.client.get("https://example.com")


class Transformed:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def __setstate__(self, state) -> None:
        for attr, value in state.items():
            setattr(self, attr, wrap(value))

    async def handle(self) -> None:
        self.client.get("https://example.com")


def wrap(value):
    return value


class OwnDict:
    def __init__(self) -> None:
        self.client = httpx.Client()
        self.active = self.client

    def __getstate__(self):
        if self.client is None:
            return vars(self).copy()
        return self.__dict__.copy()

    def __setstate__(self, state) -> None:
        self.__dict__ = state

    async def handle(self) -> None:
        self.active.get("https://example.com")  # EFF101


class OwnLiteral:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def __getstate__(self):
        state = {"client": self.client, "kind": "literal"}
        state["copy"] = self.client
        return state

    def __setstate__(self, state) -> None:
        state = dict(state)
        state.pop("kind", None)
        self.__dict__.update(state)

    async def handle(self) -> None:
        self.client.get("https://example.com")  # EFF101


class ReturnsNamespace:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def __getstate__(self):
        return self.__dict__

    def __setstate__(self, state) -> None:
        self.__dict__.update(state)


class Mixed:
    def __init__(self) -> None:
        self.client = httpx.Client()
        self.n = 0

    def __getstate__(self):
        return {"n": self.n + 1, "client": self.client}

    def __setstate__(self, state) -> None:
        self.__dict__.update(state)

    async def handle(self) -> None:
        self.client.get("https://example.com")


class Updated:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def __getstate__(self):
        state = {"client": self.client}
        state.update(extra())
        return state

    def __setstate__(self, state) -> None:
        self.__dict__.update(state)

    async def handle(self) -> None:
        self.client.get("https://example.com")


class Called:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def __getstate__(self):
        return extra()

    def __setstate__(self, state) -> None:
        self.__dict__.update(state)

    async def handle(self) -> None:
        self.client.get("https://example.com")


class Branched:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def __setstate__(self, state) -> None:
        if state.get("version") == 2:
            state = dict(state)
        self.__dict__.update(state)

    async def handle(self) -> None:
        self.client.get("https://example.com")  # EFF101


class Constant:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def __setstate__(self, state) -> None:
        for attr in state:
            setattr(self, attr, 0)

    async def handle(self) -> None:
        self.client.get("https://example.com")


class Versioned:
    def __init__(self) -> None:
        self.client = httpx.Client()

    if sys.version_info >= (3, 11):

        def __getstate__(self):
            return self.__dict__.copy()

    else:

        def __getstate__(self):
            return {"client": httpx.AsyncClient()}

    def __setstate__(self, state) -> None:
        self.__dict__.update(state)

    async def handle(self) -> None:
        self.client.get("https://example.com")


class Redefined:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def __getstate__(self):
        return {"client": httpx.AsyncClient()}

    def __getstate__(self):  # noqa: F811
        return self.__dict__.copy()

    def __setstate__(self, state) -> None:
        self.__dict__.update(state)

    async def handle(self) -> None:
        self.client.get("https://example.com")


class Unpacked:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def __getstate__(self):
        return {**self.__dict__}

    def __setstate__(self, state) -> None:
        self.__dict__.update(state)

    async def handle(self) -> None:
        self.client.get("https://example.com")


def extra():
    return {"client": httpx.AsyncClient()}
"#;

const CALLS_SETSTATE: &str = r#"import httpx

from pkg.restored import Restored


def load(target: Restored) -> None:
    target.__setstate__({"client": httpx.AsyncClient()})
"#;

/// The method read and called later, under its name as a string.
const READS_SETSTATE: &str = r#"import httpx

from pkg.restored import Restored


def load(target: Restored) -> None:
    restore = getattr(target, "__setstate__")
    restore({"client": httpx.AsyncClient()})
"#;

/// A class whose `__setstate__` restores its namespace, and whose other method then stores an
/// attribute heki cannot name: a read of its attributes made before that method is visited is
/// read again, so a global copying one is not left with the restored type.
const RESTORED_THEN_UNNAMED: &str = r#"import httpx

copied = httpx.Client()
kept = httpx.Client()


class Unnamed:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def __setstate__(self, state) -> None:
        self.__dict__.update(state)

    def remember(self) -> None:
        global copied
        copied = self.client

    def configure(self, name, value) -> None:
        self.client.close()
        setattr(self, name, value)


class Restored:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def __setstate__(self, state) -> None:
        self.__dict__.update(state)

    def remember(self) -> None:
        global kept
        kept = self.client


async def handle() -> None:
    copied.get("https://example.com")


async def handle_kept() -> None:
    kept.get("https://example.com")  # EFF101
"#;

#[test]
fn setstate_restores_what_an_instance_held() {
    check_marks("restored", &[("pkg/restored.py", RESTORED)], &["EFF101"]);
    // A direct call may hand `__setstate__` anything, and so may the method read and called
    // later.
    let unmarked = RESTORED.replace("  # EFF101", "");
    for (name, caller) in [
        ("restored_called", CALLS_SETSTATE),
        ("restored_read", READS_SETSTATE),
    ] {
        check_marks(
            name,
            &[
                ("pkg/restored.py", unmarked.as_str()),
                ("pkg/calls_setstate.py", caller),
            ],
            &["EFF101"],
        );
    }
    check_marks(
        "restored_then_unnamed",
        &[("pkg/restored_then_unnamed.py", RESTORED_THEN_UNNAMED)],
        &["EFF101"],
    );
}

/// A star import binds the names its module's `__all__` lists, when the module fixes it: the
/// module body's stores of other names before it keep their types.
const STAR_EXPORTS: &str = r#"import httpx

listed = httpx.Client()
unlisted = httpx.Client()
in_tuple = httpx.Client()

from pkg.exporting import *  # noqa: E402, F403
from pkg.exporting_tuple import *  # noqa: E402, F403


async def handle() -> None:
    listed.get("https://example.com")
    unlisted.get("https://example.com")  # EFF101
    in_tuple.get("https://example.com")
"#;

const EXPORTING: &str = r#"import httpx

__all__ = ["listed"]

listed = httpx.AsyncClient()
"#;

const EXPORTING_TUPLE: &str = r#"import httpx

__all__ = ("in_tuple",)

in_tuple = httpx.AsyncClient()
"#;

/// An `__all__` bound on some paths only, or a list the module body changes after binding it,
/// does not fix what the star import binds: without `__all__` it binds every public name, and an
/// appended name is exported too.
const EXPORTING_ON_ONE_PATH: &str = r#"import sys

import httpx

if sys.platform == "win32":
    __all__ = ("other",)

other = 1
client = httpx.AsyncClient()
"#;

const EXPORTING_APPENDED: &str = r#"import httpx

__all__ = ["other"]
try:
    import pkg.speedups  # noqa: F401

    __all__.append("client")
except ImportError:
    pass

other = 1
client = httpx.AsyncClient()
"#;

const STAR_ON_ONE_PATH: &str = r#"import httpx

client = httpx.Client()

from pkg.exporting_on_one_path import *  # noqa: E402, F403


async def handle() -> None:
    client.get("https://example.com")
"#;

#[test]
fn a_star_import_rebinds_the_names_its_module_exports() {
    check_marks(
        "star_exports",
        &[
            ("pkg/star_exports.py", STAR_EXPORTS),
            ("pkg/exporting.py", EXPORTING),
            ("pkg/exporting_tuple.py", EXPORTING_TUPLE),
            ("pkg/exporting_on_one_path.py", EXPORTING_ON_ONE_PATH),
            ("pkg/exporting_appended.py", EXPORTING_APPENDED),
            ("pkg/star_on_one_path.py", STAR_ON_ONE_PATH),
            (
                "pkg/star_appended.py",
                STAR_ON_ONE_PATH.replace("on_one_path", "appended").as_str(),
            ),
        ],
        &["EFF101"],
    );
}

/// A module whose `__all__` is neither a list nor a tuple of string literals has exports heki does
/// not fix (a set makes the star import raise `TypeError`): the star import may rebind every
/// name.
const EXPORTING_SET: &str = r#"import httpx

__all__ = {"listed"}

listed = httpx.AsyncClient()
"#;

const STAR_SET: &str = r#"import httpx

listed = httpx.Client()

from pkg.exporting_set import *  # noqa: E402, F403


async def handle() -> None:
    listed.get("https://example.com")
"#;

/// An attribute declared by a subclass keeps the type its base's stores bind, and a subclass
/// storing attributes under names the code does not give leaves the base's reads unknown.
const FAMILIES: &str = r#"import httpx


class Annotated:
    def __init__(self) -> None:
        self.client = httpx.Client()

    async def handle(self) -> None:
        self.client.get("https://example.com")  # EFF101


class AnnotatedSub(Annotated):
    client: httpx.Client


class Open:
    def __init__(self) -> None:
        self.client = httpx.Client()

    async def handle(self) -> None:
        self.client.get("https://example.com")


class OpenSub(Open):
    def put(self, name: str) -> None:
        setattr(self, name, httpx.AsyncClient())


class AnnotatedReader:
    def __init__(self, source: Annotated) -> None:
        self.copy = source.client

    async def handle(self) -> None:
        self.copy.get("https://example.com")  # EFF101


class OpenReader:
    def __init__(self, source: Open) -> None:
        self.copy = source.client

    async def handle(self) -> None:
        self.copy.get("https://example.com")
"#;

const FAMILY_BASE: &str = r#"import httpx


class Base:
    def __init__(self) -> None:
        self.client = httpx.Client()

    async def handle(self) -> None:
        self.client.get("https://example.com")  # EFF101
"#;

const FAMILY_SUB: &str = r#"import httpx

from pkg.family_base import Base


class Sub(Base):
    client: httpx.Client
"#;

/// A module storing through a module that re-exports the name binds the re-exporting module's
/// name: the module's own name keeps its type.
const REEXPORTED: &str = r#"import httpx

import pkg.reexporting

shared = httpx.Client()


def swap() -> None:
    pkg.reexporting.shared = httpx.AsyncClient()


async def handle() -> None:
    shared.get("https://example.com")  # EFF101
"#;

const REEXPORTING: &str = r#"from pkg.reexported import shared  # noqa: F401
"#;

#[test]
fn stores_through_families_and_re_exports_keep_what_they_bind() {
    check_marks(
        "store_edges",
        &[
            ("pkg/exporting_set.py", EXPORTING_SET),
            ("pkg/star_set.py", STAR_SET),
            ("pkg/families.py", FAMILIES),
            ("pkg/family_base.py", FAMILY_BASE),
            ("pkg/family_sub.py", FAMILY_SUB),
            ("pkg/reexported.py", REEXPORTED),
            ("pkg/reexporting.py", REEXPORTING),
        ],
        &["EFF101"],
    );
}

/// More spellings of stores and deletions, recognized by the value of the callee: `setattr`
/// through `builtins`, a local alias, an import in the function (also the function's only sign
/// of a store), a closure cell, or names bound on both branches; deletions and namespace reads
/// store nothing; a namespace handed on, merged, updated from keywords or another dict, or
/// written under another method stores any attribute; stores through a union of classes, the
/// module itself, and in place.
const FORMS: &str = r#"import builtins
import collections
from typing import Protocol

import httpx

import pkg.forms


class Spelled:
    def __init__(self) -> None:
        self.client = httpx.Client()
        self.items = [httpx.Client()]
        self.pool = [httpx.Client()]

    def through_builtins(self) -> None:
        builtins.setattr(self, "client", httpx.Client(timeout=1))

    def through_local_alias(self) -> None:
        put = setattr
        put(self, "client", httpx.Client(timeout=2))

    def through_import(self) -> None:
        from builtins import setattr as put

        put(self, "client", httpx.Client(timeout=3))

    def through_cell(self) -> None:
        put = setattr

        def inner() -> None:
            put(self, "client", httpx.Client(timeout=4))

        inner()

    def through_branches(self, fast: bool) -> None:
        if fast:
            put = setattr
        else:
            put = setattr
        put(self, "client", httpx.Client(timeout=5))

    def deletions(self, name) -> None:
        delattr(self, name)
        del self.items
        self.__delattr__("items")
        object.__delattr__(self, "items")
        del self.__dict__["items"]
        self.__dict__.clear()
        vars(self).popitem()

    def namespace_reads(self) -> bool:
        vars(self).setdefault("other")
        if self.__dict__:
            return "client" in vars(self)
        return False

    def grow(self) -> None:
        self.items += [httpx.Client()]

    def reset_pool(self) -> None:
        self.pool = collections.deque()

    def grow_pool(self) -> None:
        self.pool += [httpx.Client()]

    async def handle(self) -> None:
        self.client.get("https://example.com")  # EFF101
        for c in self.items:
            c.get("https://example.com")  # EFF101


class ImportedSetattr:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def swap(self, client: httpx.AsyncClient) -> None:
        from builtins import setattr as put

        put(self, "client", client)

    async def handle(self) -> None:
        self.client.get("https://example.com")


def fill(namespace) -> None:
    namespace.clear()


class HandsOn:
    def __init__(self) -> None:
        self.client = httpx.Client()
        self.__dict__.__ior__({"client": httpx.AsyncClient()})

    def state(self):
        return self.__dict__

    async def handle(self) -> None:
        self.client.get("https://example.com")


class Merged:
    def __init__(self, other: dict) -> None:
        self.client = httpx.Client()
        namespace = self.__dict__ if other else other
        namespace["client"] = other

    async def handle(self) -> None:
        self.client.get("https://example.com")


class Passed:
    def __init__(self) -> None:
        self.client = httpx.Client()
        fill(vars(self))

    async def handle(self) -> None:
        self.client.get("https://example.com")


class Keywords:
    def __init__(self, **options) -> None:
        self.client = httpx.Client()
        self.__dict__.update(**options)

    async def handle(self) -> None:
        self.client.get("https://example.com")


class SetItem:
    def __init__(self) -> None:
        self.client = httpx.Client()
        self.__dict__.__setitem__("client", httpx.AsyncClient())

    async def handle(self) -> None:
        self.client.get("https://example.com")


class Unnamed:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def swap(self, name) -> None:
        super().__setattr__(name, httpx.AsyncClient())

    async def handle(self) -> None:
        self.client.get("https://example.com")


class Left:
    def __init__(self) -> None:
        self.client = httpx.Client()

    async def handle(self) -> None:
        self.client.get("https://example.com")


class Right:
    def __init__(self) -> None:
        self.client = httpx.Client()

    async def handle(self) -> None:
        self.client.get("https://example.com")


def swap_either(obj: Left | Right) -> None:
    obj.client = httpx.AsyncClient()


class HasClient(Protocol):
    def ping(self) -> None: ...


def through_protocol(obj: HasClient) -> None:
    obj.client = httpx.AsyncClient()


module_client = httpx.Client()
module_items = [httpx.Client()]
pool = [httpx.Client()]


def rebind_self() -> None:
    pkg.forms.module_client = httpx.AsyncClient()


def grow_self() -> None:
    pkg.forms.module_items += [httpx.Client()]


def grow_pool() -> None:
    global pool
    pool += [httpx.Client()]


async def module_reads() -> None:
    module_client.get("https://example.com")
    for c in module_items:
        c.get("https://example.com")  # EFF101
    for c in pool:
        c.get("https://example.com")  # EFF101
"#;

/// A store reading another name: the reads of the fixpoint through a property, an annotation,
/// an attribute no `__init__` binds, a base in another module, a class storing names heki
/// cannot name, a store of unknown type, an attribute rebound in place to an `int`, and a global
/// of unknown type. A cycle of stores no value enters leaves the module the first round's types.
const READS: &str = r#"import httpx

from pkg.forms import Left


class WithProperty:
    def __init__(self) -> None:
        self.client = httpx.Client()
        self.backup = self.client
        self.typed: httpx.Client = httpx.Client()
        self.copy_of_typed = self.typed
        self.active = self.current

    @property
    def current(self) -> httpx.Client:
        return self.client

    def start(self) -> None:
        self.later = httpx.Client()
        self.after = self.later

    async def handle(self) -> None:
        self.active.get("https://example.com")  # EFF101
        self.copy_of_typed.get("https://example.com")  # EFF101


class Below(Left):
    def swap(self) -> None:
        self.client = httpx.Client(timeout=1)
        self.previous = self.client


class AnyAndRead:
    def __init__(self) -> None:
        self.client = httpx.Client()
        self.active = self.client

    def load(self, name) -> None:
        setattr(self, name, self.client)

    async def handle(self) -> None:
        self.active.get("https://example.com")


class OpenInit:
    def __init__(self, make) -> None:
        self.client = make()
        self.active = self.client

    async def handle(self) -> None:
        self.active.get("https://example.com")


class Counter:
    def __init__(self) -> None:
        self.count = 0
        self.total = self.count

    def bump(self) -> None:
        self.count += 1
        self.total = self.count


class Wide:
    def __init__(self) -> None:
        self.client = httpx.Client()
        self.active = self.client

    async def handle(self) -> None:
        self.active.get("https://example.com")


class WideChild(Wide):
    def swap(self, client) -> None:
        self.client = client


def factory():
    return None


unknown = factory()
copied = unknown
"#;

const STORE_CYCLE: &str = r#"class Cycle:
    def __init__(self) -> None:
        self.a = self.b
        self.b = self.a

    async def handle(self) -> None:
        self.a.get("https://example.com")
"#;

/// Modules other modules store into: under names heki cannot read, an attribute a store of the
/// module reads, in place, of the same class, of unknown type, and `None`.
const TARGET_ANY: &str = r#"import httpx

client = httpx.Client()


async def handle() -> None:
    client.get("https://example.com")
"#;

const TARGET_ATTR: &str = r#"import httpx


class Holder:
    def __init__(self) -> None:
        self.client = httpx.Client()
        self.active = self.client

    async def handle(self) -> None:
        self.client.get("https://example.com")  # EFF101
        self.active.get("https://example.com")


class Opaque:
    def __init__(self) -> None:
        self.client = httpx.Client()

    async def handle(self) -> None:
        self.client.get("https://example.com")
"#;

const TARGET_GLOB: &str = r#"import httpx

items = [httpx.Client()]
pool = [httpx.Client()]
shared = httpx.Client()
unknown = httpx.Client()
cleared = httpx.Client()


async def handle() -> None:
    for c in items:
        c.get("https://example.com")  # EFF101
    for c in pool:
        c.get("https://example.com")  # EFF101
    shared.get("https://example.com")  # EFF101
    unknown.get("https://example.com")
    cleared.get("https://example.com")  # EFF101
"#;

const TARGET_SELF_ANY: &str = r#"import httpx

client = httpx.Client()


def configure(name) -> None:
    import pkg.target_self_any

    setattr(pkg.target_self_any, name, client)


async def handle() -> None:
    client.get("https://example.com")
"#;

const STORES_ELSEWHERE: &str = r#"import httpx

import pkg.target_any
import pkg.target_glob
from pkg.target_attr import Holder, Opaque


def unnamed(name, opaque: Opaque) -> None:
    setattr(pkg.target_any, name, httpx.AsyncClient())
    setattr(opaque, name, httpx.AsyncClient())


def same_class(holder: Holder) -> None:
    holder.client = httpx.Client(timeout=1)
    pkg.target_glob.items = [httpx.Client()]
    pkg.target_glob.shared = httpx.Client(timeout=1)


def others(make) -> None:
    pkg.target_glob.items += [httpx.Client()]
    pkg.target_glob.unknown = make()
    pkg.target_glob.cleared = None
"#;

const MORE_ELSEWHERE: &str = r#"import httpx

import pkg.target_glob


def again(make) -> None:
    pkg.target_glob.shared = httpx.Client(timeout=2)
    pkg.target_glob.unknown = make()
    pkg.target_glob.items += [httpx.Client()]
    pkg.target_glob.pool += [httpx.Client()]
"#;

// Two projects: past a number of sites, one report names the others.
#[test]
fn every_spelling_of_a_store_and_every_read_of_one() {
    check_marks(
        "store_forms",
        &[
            ("pkg/forms.py", FORMS),
            ("pkg/reads.py", READS),
            ("pkg/store_cycle.py", STORE_CYCLE),
        ],
        &["EFF101"],
    );
    check_marks(
        "store_targets",
        &[
            ("pkg/target_any.py", TARGET_ANY),
            ("pkg/target_attr.py", TARGET_ATTR),
            ("pkg/target_glob.py", TARGET_GLOB),
            ("pkg/target_self_any.py", TARGET_SELF_ANY),
            ("pkg/stores_elsewhere.py", STORES_ELSEWHERE),
            ("pkg/more_elsewhere.py", MORE_ELSEWHERE),
        ],
        &["EFF101"],
    );
}

/// A global changed in place through the module's own object is typed by its name alone: what
/// the changes add joins its type once its stores establish it, also from a method whose
/// stores heki decides with the attributes it reads (`self.client`). An annotated global keeps
/// its declaration.
const SELF_CHANGED: &str = r#"import httpx

import pkg.forms

grown = []
typed: list[httpx.Client] = []


class Grower:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def grow(self) -> None:
        pkg.forms.grown.append(self.client)


def grow_typed() -> None:
    pkg.forms.typed.append(httpx.Client())


async def reads() -> None:
    for c in grown:
        c.get("https://example.com")  # EFF101
    for c in typed:
        c.get("https://example.com")  # EFF101
"#;

#[test]
fn a_global_changed_through_its_module_joins_what_the_changes_add() {
    check_marks(
        "store_self_changed",
        &[("pkg/forms.py", SELF_CHANGED)],
        &["EFF101"],
    );
}

/// A `__setstate__` that also reads an attribute heki is deciding is solved with the stores, and
/// its restores hold the type its `__init__` gave; a base and its subclass, read through
/// one family, are decided together.
const REVIVED: &str = r#"import httpx


class Revived:
    def __init__(self) -> None:
        self.client = httpx.Client()

    def __setstate__(self, state) -> None:
        for attr, value in state.items():
            setattr(self, attr, value)
        self.client.close()

    async def handle(self) -> None:
        self.client.get("https://example.com")  # EFF101


class Open:
    def __init__(self) -> None:
        self.client = httpx.Client()


class OpenSub(Open):
    def put(self, name: str) -> None:
        setattr(self, name, httpx.AsyncClient())


class ReaderOfBase:
    def __init__(self, source: Open) -> None:
        self.copy = source.client

    async def handle(self) -> None:
        self.copy.get("https://example.com")


class ReaderOfSub:
    def __init__(self, source: OpenSub) -> None:
        self.copy = source.client

    async def handle(self) -> None:
        self.copy.get("https://example.com")
"#;

#[test]
fn restores_and_families_are_solved_with_the_stores_they_read() {
    check_marks("revived", &[("pkg/revived.py", REVIVED)], &["EFF101"]);
}
