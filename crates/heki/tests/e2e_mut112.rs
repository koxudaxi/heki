//! `MUT112` (`conditional-ownership-mutation`) end to end: a value that is a new object on one
//! path and borrowed state on another, then changed in place. The shapes (`p or []`, a
//! conditional expression, `d.get(k, {})`, `getattr(o, n, {})`, an `if` rebinding, a callee
//! returning a memo or its argument, a closure changing the enclosing function's value, a
//! parameter tested for `None` and stored on `self`, a value stored on `self` that another
//! method changes), changes through helpers and across modules, every exclusion, the opt-in
//! selection, `heki rule`, and the incremental LSP server.

mod common;

use common::Project;
use common::lsp::{Lsp, codes, uri};
use serde_json::{Value, json};

fn diagnostics(p: &Project, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["--output-format", "json"];
    all.extend_from_slice(args);
    let (code, stdout, stderr) = p.check(&all);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

/// `(path, line, qualname, target)` of every `MUT112` diagnostic.
fn found(diags: &[Value]) -> Vec<(String, u64, String, String)> {
    diags
        .iter()
        .filter(|d| d["rule_code"] == "MUT112")
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

fn at<'v>(diags: &'v [Value], path: &str, line: u64) -> &'v Value {
    diags
        .iter()
        .find(|d| d["path"] == path && d["line"] == line)
        .unwrap_or_else(|| panic!("no diagnostic at {path}:{line}: {diags:#?}"))
}

fn message(diag: &Value) -> &str {
    diag["message"].as_str().unwrap()
}

/// `(path, line)` of a diagnostic's related locations.
fn related(diag: &Value) -> Vec<(String, u64)> {
    diag["related"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["path"].as_str().unwrap().to_owned(),
                r["line"].as_u64().unwrap(),
            )
        })
        .collect()
}

/// Each function changes in place a value that is new on one path and borrowed on another.
const SHAPES: &str = r#"import typing


def scopes_or_empty(scopes=None):
    use = scopes or []
    use.extend(["read"])
    return use


def merged(defaults, extra):
    options = dict(defaults, **extra) if extra else defaults
    options.setdefault("priority", 0)
    return options


def entry(registry, key):
    seen = registry.get(key, {})
    seen["hits"] = 1


def tags_of(obj):
    tags = getattr(obj, "tags", [])
    tags.append("x")


def rebound(shared, fresh):
    x = shared
    if fresh:
        x = []
    x.append(1)


def cast_back(shared, fresh):
    x = [] if fresh else typing.cast(list, shared)
    x += [1]
    x.sort()


GLOBAL_TAGS = []


def global_or_new(fresh):
    tags = [] if fresh else GLOBAL_TAGS
    tags.append("x")


def add_default(items):
    items.append("default")


def through_helper(items=None):
    chosen = items or []
    add_default(chosen)
    return chosen


class Conn:
    def __init__(self, retry_on_error=None, retry_on_timeout=False):
        if retry_on_error is None:
            retry_on_error = []
        if retry_on_timeout:
            retry_on_error.append(TimeoutError)
        self.retry_on_error = retry_on_error


def autoretry(task, **options):
    retry_kwargs = options.get("retry_kwargs", getattr(task, "retry_kwargs", {}))

    def run(*args):
        retry_kwargs["countdown"] = 1
        return task.retry(*args, **retry_kwargs)

    task.run = run


def drop_task_id(defaults, extra):
    options = dict(defaults, **extra) if extra else defaults
    options.pop("task_id", None)
"#;

/// A class whose memo another module's caller changes, and a function returning its argument
/// on one path: the callers are in `callers.py`.
const SOURCES: &str = r#"class Task:
    _exec_options = None

    def _get_exec_options(self):
        if self._exec_options is None:
            self._exec_options = {"queue": "default"}
        return self._exec_options

    def exec_options(self):
        return self._get_exec_options()


def prepare(config):
    if config is None:
        return {}
    if not isinstance(config, dict):
        config = {k: getattr(config, k) for k in dir(config)}
    return config


def prepare_twice(config):
    return prepare(config)


def fresh_copy(config):
    return dict(config)
"#;

const CALLERS: &str = r#"from pkg.sources import Task, fresh_copy, prepare, prepare_twice


class Job(Task):
    def apply_async(self, **options):
        preopts = self.exec_options()
        options = dict(preopts, **options) if options else preopts
        options.setdefault("ignore_result", True)
        return options


class Wrapper:
    def __init__(self, config, check=True):
        if check:
            self.config_dict = prepare_twice(config)
        else:
            self.config_dict = config
        self.copied = fresh_copy(config) if check else {}

    def core_config(self):
        config = self.config_dict
        config["validate_by_name"] = True
        self.copied["seen"] = True


def configure(config):
    cfg = prepare(config)
    cfg["debug"] = True
    return cfg
"#;

#[test]
fn mut112_conditional_ownership_shapes() {
    let p = Project::new("mut112_shapes");
    p.write("pkg/__init__.py", "")
        .write("pkg/shapes.py", SHAPES)
        .write("pkg/sources.py", SOURCES)
        .write("pkg/callers.py", CALLERS);
    // Opt-in: nothing without the selection.
    assert!(found(&diagnostics(&p, &[])).is_empty());
    let diags = diagnostics(&p, &["--extend-select", "MUT112"]);
    let (s, c) = ("pkg/shapes.py", "pkg/callers.py");
    assert_eq!(
        found(&diags),
        vec![
            d(c, 8, "Job.apply_async", "options"),
            d(c, 15, "Wrapper.__init__", "self.config_dict"),
            d(c, 28, "configure", "cfg"),
            d(s, 6, "scopes_or_empty", "use"),
            d(s, 12, "merged", "options"),
            d(s, 18, "entry", "seen"),
            d(s, 23, "tags_of", "tags"),
            d(s, 30, "rebound", "x"),
            d(s, 35, "cast_back", "x"),
            d(s, 44, "global_or_new", "tags"),
            d(s, 53, "through_helper", "chosen"),
            d(s, 62, "Conn.__init__", "retry_on_error"),
            d(s, 70, "autoretry.<locals>.run", "retry_kwargs"),
            d(s, 78, "drop_task_id", "options"),
        ]
    );
    assert_eq!(
        message(at(&diags, s, 6)),
        "`scopes_or_empty` changes `use` in place, which is a new object on one path and the parameter `scopes` on another"
    );
    assert_eq!(
        diags[0]["help"].as_str().unwrap(),
        "copy the borrowed value before changing it (`list(...)`, `dict(...)`, `.copy()`), so that every path changes an object of its own"
    );
    // A callee returning a memo, through a wrapper in another module: the message names the
    // function the caller calls.
    assert_eq!(
        message(at(&diags, c, 8)),
        "`Job.apply_async` changes `options` in place, which is a new object on one path and what `Task.exec_options` returns on another"
    );
    assert_eq!(
        message(at(&diags, s, 18)),
        "`entry` changes `seen` in place, which is a new object on one path and an element of the parameter `registry` on another"
    );
    assert_eq!(
        message(at(&diags, s, 23)),
        "`tags_of` changes `tags` in place, which is a new object on one path and the attribute `tags` on another"
    );
    assert_eq!(
        message(at(&diags, s, 44)),
        "`global_or_new` changes `tags` in place, which is a new object on one path and the global `GLOBAL_TAGS` on another"
    );
    // A helper changing its argument: the call is reported, with the helper.
    let helper = at(&diags, s, 53);
    assert_eq!(related(helper), vec![(s.to_owned(), 53)]);
    assert!(
        helper["related"][0]["message"]
            .as_str()
            .unwrap()
            .ends_with("calls `pkg.shapes.add_default`"),
        "{helper:#}"
    );
    // A closure changes the enclosing function's value.
    assert_eq!(
        message(at(&diags, s, 70)),
        "`autoretry.<locals>.run` changes `retry_kwargs` in place, which is a new object on one path and an element of the parameter `options` on another"
    );
    // The value stored on `self` and changed by another method: reported at the store, with the
    // change.
    let stored = at(&diags, c, 15);
    assert_eq!(
        message(stored),
        "`Wrapper.__init__` stores in `self.config_dict` a value that is a new object on one path and the parameter `config` on another, and `Wrapper.core_config` changes it in place"
    );
    assert_eq!(
        stored["help"].as_str().unwrap(),
        "store a copy of the borrowed value (`list(...)`, `dict(...)`, `.copy()`), so that the instance owns what its methods change"
    );
    assert_eq!(related(stored), vec![(c.to_owned(), 22)]);
}

/// More sources of borrowed state and of new objects, one function each.
const MORE: &str = r#"import functools

import numpy as np


class Opts:
    def __init__(self):
        self._items = []
        self.options = {}

    @property
    def items(self):
        return self._items


def from_attribute(holder, extra):
    opts = holder.options if not extra else {}
    opts["x"] = 1


def from_slice(rows, copy):
    x = rows[:] if copy else rows
    x.append(1)


def view_or_new(arr: np.ndarray, flag):
    x = arr[:] if flag else []
    y = x if flag else [0]
    y[0] = 1


def named_getattr(obj, flag):
    tags = getattr(obj, "tags") if flag else []
    tags.append("x")


def from_property(o: Opts, flag):
    items = o.items if flag else []
    items.append(1)


def first(rows):
    return rows[0]


def first_of(rows):
    return first(rows)


def element_returned(rows, flag):
    x = first_of(rows) if flag else []
    x.append(1)


@functools.lru_cache
def cached():
    return []


def from_cache(flag):
    x = cached() if flag else []
    x.append(1)


def packed(**kw):
    return kw


def packed_item(**kw):
    return kw["x"]


def from_packed(shared, flag):
    x = packed(a=1) if flag else shared
    x["k"] = 1


def from_packed_item(flag):
    x = packed_item(x=[]) if flag else []
    x.append(1)


def registry_rows():
    global ROWS
    rows = []
    ROWS = rows
    return rows


def from_registry(flag):
    x = registry_rows() if flag else []
    x.append(1)


def stored_globally(flag):
    global LAST
    rows = []
    LAST = rows
    x = rows if flag else []
    x.append(1)


def outer_cell(flag):
    acc = None

    def inner():
        nonlocal acc
        fresh = []
        acc = fresh
        x = fresh if flag else []
        x.append(1)

    return inner


def unseeded(fetch):
    data = fetch()

    def inner():
        x = data or {}
        x["k"] = 1

    return inner


if np:
    def choose(rows):
        return []
else:
    def choose(rows):
        return rows


def from_duplicate(rows):
    x = choose(rows)
    if not x:
        x = []
    x.append(1)


def from_comprehension(given, copy):
    rows = [r for r in given] if copy else given
    rows.append(1)


def defaulted_tags(obj):
    return getattr(obj, "tags", [])


def from_defaulted_tags(obj):
    x = defaulted_tags(obj)
    x.append(1)


DEFAULT_ROWS = []


def head_of(*rows):
    return rows[0]


def head_default():
    return head_of(DEFAULT_ROWS)


def from_head_default(flag):
    x = head_default() if flag else []
    x.append(1)


def unnamed_default(options, x):
    options.get("tags", []).append(x)


def unrelated_identity(shared, a, b):
    x = shared
    if a is b:
        x = []
    x.append(1)
"#;

#[test]
fn mut112_more_sources_and_returns() {
    let p = Project::new("mut112_more");
    p.write("pkg/__init__.py", "").write("pkg/more.py", MORE);
    let diags = diagnostics(&p, &["--select", "MUT112"]);
    let m = "pkg/more.py";
    let expected = vec![
        d(m, 18, "from_attribute", "opts"),
        d(m, 23, "from_slice", "x"),
        d(m, 29, "view_or_new", "y"),
        d(m, 34, "named_getattr", "tags"),
        d(m, 39, "from_property", "items"),
        d(m, 52, "element_returned", "x"),
        d(m, 62, "from_cache", "x"),
        d(m, 75, "from_packed", "x"),
        d(m, 80, "from_packed_item", "x"),
        d(m, 92, "from_registry", "x"),
        d(m, 100, "stored_globally", "x"),
        d(m, 111, "outer_cell.<locals>.inner", "x"),
        d(m, 121, "unseeded.<locals>.inner", "x"),
        d(m, 138, "from_duplicate", "x"),
        d(m, 143, "from_comprehension", "rows"),
        d(m, 152, "from_defaulted_tags", "x"),
        d(m, 168, "from_head_default", "x"),
        d(m, 172, "unnamed_default", "a value"),
        d(m, 179, "unrelated_identity", "x"),
    ];
    assert_eq!(found(&diags), expected, "{diags:#?}");
    // A change of a value without a name (a call's result) names no variable.
    assert_eq!(
        message(at(&diags, m, 172)),
        "`unnamed_default` changes a value in place, which is a new object on one path and an element of the parameter `options` on another"
    );
    for (line, borrow) in [
        (18, "the attribute `options`"),
        (29, "the parameter `arr`"),
        (34, "the attribute `tags`"),
        (39, "what `Opts.items` returns"),
        (52, "an element of the parameter `rows`"),
        (62, "what `cached` returns"),
        (75, "the parameter `shared`"),
        (80, "what `packed_item` returns"),
        (92, "what `registry_rows` returns"),
        (100, "the global `LAST`"),
        (111, "the enclosing function's `acc`"),
        (121, "the enclosing function's `data`"),
        // A callee returning an attribute or its default.
        (152, "what `defaulted_tags` returns"),
        // A callee returning what its `*args` holds: the caller's global.
        (168, "what `head_default` returns"),
        // An `is` test of two other values leaves the borrowed path.
        (179, "the parameter `shared`"),
    ] {
        assert!(
            message(at(&diags, m, line)).ends_with(&format!("{borrow} on another")),
            "{line}: {}",
            message(at(&diags, m, line))
        );
    }
    // Python 3.11 builds a comprehension in a function of its own: its result is new too.
    let py311 = diagnostics(&p, &["--select", "MUT112", "--target-version", "py311"]);
    assert_eq!(found(&py311), expected);
}

/// None of these is reported.
const NOT_REPORTED: &str = r#"def accumulate(node, out=None):
    if out is None:
        out = []
    out.append(node)
    for child in node.children:
        accumulate(child, out)
    return out


def accumulate_expr(node, out=None):
    out = [] if out is None else out
    out.append(node)
    return out


def both_new(flag):
    x = [] if flag else [0]
    x.append(1)


def copied_first(scopes=None):
    use = scopes or []
    use = list(use)
    use.append(1)


def changes_below(rows=None):
    use = rows or []
    use[0].append(1)


def only_read(scopes=None):
    use = scopes or []
    return len(use)


def keep_stored(table, key):
    rows = table.get(key)
    if rows is None:
        rows = table[key] = []
    rows.append(key)


def new_returned():
    return []


def from_new_callee(given, flag):
    rows = given if flag else new_returned()
    return rows


def rebinding(scopes=None):
    use = scopes or []
    use = use + ["x"]
    return use


class Holder:
    def __init__(self, items=None):
        self.items = list(items or [])
        self.cache = {} if items is None else items
        self._memo = items or {}

    def again(self):
        self.items = self.items or []
        self.items.append(1)

    def fill(self, key):
        self.cache[key] = 1
        self._memo[key] = 1

    def in_method(self, other):
        target = self if other is None else other
        target.items.append(1)


class Plain:
    def __init__(self, items=None):
        self.items = items or []

    def size(self):
        return len(self.items)


def attribute_store(node, flag):
    target = node if flag else Plain()
    target.items = []


def append_rows(rows):
    rows[0].append(1)


def below_through_helper(rows=None):
    use = rows or []
    append_rows(use)


def remove_from_default(table, name):
    table.get("properties", {}).pop(name, None)
    found = table.get("required", [])
    del found[0]


def copy_on_write(child, drop):
    result = child
    for field in drop:
        if result is child:
            result = child.copy()
        result.pop(field)
    return result


def copy_on_write_attr(node, keys):
    kwargs = node.kwargs
    for key in keys:
        if kwargs is node.kwargs:
            kwargs = kwargs.copy()
        kwargs[key] = 1


def default_in_loop(values, members=None):
    for i, v in enumerate(values):
        if members is None:
            members = list(values)
        members[i] = v
    return members


def picked(flag, shared):
    local = []
    (local if flag else shared).append(1)
    return local


def forever(items):
    current = items
    while True:
        current = []
        if len(current) < 3:
            break
    current.append(1)


def settled_by_setattr(namespace):
    current = getattr(namespace, "overrides", None) or []
    current.append("x")
    setattr(namespace, "overrides", current)


def stored_new(table, key):
    rows = table.get(key)
    if rows is None:
        rows = []
        table.setdefault(key, rows)
    rows.append(key)


def element_of_new(paths):
    root = []
    for p in paths:
        branch = root
        branch = find_or_add(branch, p)
        branch.append(p)
    return root


def both_new_then_stored(flag, target, value):
    extra = {} if flag else {"value": value}
    extra.update(value)
    target.extra = extra


def memo_by_key(table, key):
    rows = table.get(key) or []
    table[key] = rows
    rows.append(key)


def counter(start=None):
    total = start or 0
    total += 1
    return total


def keep(fn):
    return fn


@keep
def decorated(rows):
    return rows


def from_decorated(rows, flag):
    x = decorated(rows) if flag else []
    x.append(1)


def returns_new_forever(rows):
    current = rows
    while True:
        current = []
        break
    return current


def from_forever(rows, flag):
    x = returns_new_forever(rows) if flag else []
    x.append(1)


def copy_on_write_expr(child, key):
    result = child
    result = child.copy() if result is child else result
    result[key] = 1


def copy_on_write_negated(child, keys):
    result = child
    for key in keys:
        if not result is not child:
            result = child.copy()
        result[key] = 1


def accumulate_negated(node, out=None):
    if not out is None:
        out.append(node)
        return out
    out = [node]
    out.append(node)
    return out


def find_or_add(branch, key):
    for node in branch:
        if key in node:
            return node[key]
    new = []
    branch.append({key: new})
    return new


def copy_on_write_swapped(child, drop):
    result = child
    for field in drop:
        if child is result:
            result = child.copy()
        result.pop(field)
    return result


def copy_on_write_logged(child, keys, log):
    result = child
    for key in keys:
        if result is child:
            result = child.copy()
        else:
            log(result)
        result[key] = 1


def tail(rows):
    return rows[1:]


def from_tail(rows, flag):
    x = tail(rows) if flag else []
    x.append(1)
"#;

#[test]
fn mut112_exclusions() {
    let p = Project::new("mut112_negatives");
    p.write("pkg/__init__.py", "")
        .write("pkg/negatives.py", NOT_REPORTED);
    let diags = diagnostics(&p, &["--select", "MUT112"]);
    assert_eq!(found(&diags), vec![], "{diags:#?}");
}

/// The accumulator idiom replaces a parameter with a new object where an `is` test finds it is
/// `None`, on any shape of that side (an `elif`, a conditional expression, an empty side). A
/// new object that test does not select is another path, and a test that replaces nothing
/// (`raise`) leaves the parameter borrowed.
const ACCUMULATORS: &str = r#"def checked_then_chosen(shared, fresh):
    if shared is None:
        raise ValueError("shared is required")
    x = [] if fresh else shared
    x.append(1)


def defaulted_then_chosen(out=None, fresh=False):
    if out is None:
        out = []
    x = [] if fresh else out
    x.append(1)


def accumulate_unless(node, out=None, skip=False):
    if out is None:
        out = []
    elif skip:
        return out
    out.append(node)
    return out


def accumulate_ordered(node, out=None, ordered=False):
    if out is None:
        out = list() if ordered else []
    out.append(node)
    return out


def accumulate_into(node, out=None):
    acc = []
    if out is not None:
        acc = out
    acc.append(node)
    return acc
"#;

#[test]
fn mut112_accumulators_exclude_their_replacements_only() {
    let p = Project::new("mut112_accumulators");
    p.write("pkg/__init__.py", "")
        .write("pkg/accumulators.py", ACCUMULATORS);
    let diags = diagnostics(&p, &["--select", "MUT112"]);
    let a = "pkg/accumulators.py";
    assert_eq!(
        found(&diags),
        vec![
            d(a, 5, "checked_then_chosen", "x"),
            d(a, 12, "defaulted_then_chosen", "x"),
        ],
        "{diags:#?}"
    );
    assert!(message(at(&diags, a, 5)).ends_with("the parameter `shared` on another"));
    assert!(message(at(&diags, a, 12)).ends_with("the parameter `out` on another"));
}

/// Values stored where they borrow from, and in the same attribute of another object. Each read
/// of `self`, of a global, or of a variable of the enclosing function is the same object, and so
/// is a copy of a reference.
const STORED_BACK: &str = r#"CONFIG = None


class Adopted:
    def __init__(self, other):
        self.items = other.items or []

    def add(self, x):
        self.items.append(x)


class Ensured:
    def __init__(self):
        self.items = self.items or []

    def add(self, x):
        self.items.append(x)


def stored_in_another(node, other):
    current = other.overrides or []
    node.overrides = current
    current.append("x")


def set_in_another(namespace, other):
    current = getattr(other, "overrides", None) or []
    setattr(namespace, "overrides", current)
    current.append("x")


def stored_back_through_alias(node):
    alias = node
    current = node.overrides or []
    alias.overrides = current
    current.append("x")


def stored_back_in_global():
    current = CONFIG.overrides or []
    CONFIG.overrides = current
    current.append("x")


def stored_back_by_closure(node):
    def ensure():
        current = node.overrides or []
        node.overrides = current
        current.append("x")

    return ensure
"#;

#[test]
fn mut112_stored_back_only_into_the_same_object() {
    let p = Project::new("mut112_stored_back");
    p.write("pkg/__init__.py", "")
        .write("pkg/stored.py", STORED_BACK);
    let diags = diagnostics(&p, &["--select", "MUT112"]);
    let s = "pkg/stored.py";
    assert_eq!(
        found(&diags),
        vec![
            d(s, 6, "Adopted.__init__", "self.items"),
            d(s, 23, "stored_in_another", "current"),
            d(s, 29, "set_in_another", "current"),
        ],
        "{diags:#?}"
    );
    assert_eq!(
        message(at(&diags, s, 6)),
        "`Adopted.__init__` stores in `self.items` a value that is a new object on one path and the attribute `items` on another, and `Adopted.add` changes it in place"
    );
}

/// Callees that return borrowed state only where no run gets: after a `while True:` loop that
/// no `break` leaves, or under `if False:`. Their callers' values are new on every path; a
/// `break` makes the return after the loop reachable.
const UNREACHED_RETURNS: &str = r#"import functools


@functools.lru_cache
def cached():
    return []


def ready():
    return True


def new_unless_never(shared):
    if False:
        return shared
    return []


def new_until_ready(shared):
    while True:
        if ready():
            return []
    return shared


def new_until_ready_cached():
    while True:
        if ready():
            return []
    return cached()


def shared_after_break(shared):
    while True:
        if ready():
            break
        return []
    return shared


def use_never(shared):
    x = new_unless_never(shared)
    x.append(1)


def use_until_ready(shared):
    x = new_until_ready(shared)
    x.append(1)


def use_until_ready_cached():
    x = new_until_ready_cached()
    x.append(1)


def use_after_break(shared):
    x = shared_after_break(shared)
    x.append(1)
"#;

#[test]
fn mut112_follows_only_the_returns_a_run_reaches() {
    let p = Project::new("mut112_unreached_returns");
    p.write("pkg/__init__.py", "")
        .write("pkg/returns.py", UNREACHED_RETURNS);
    let diags = diagnostics(&p, &["--select", "MUT112"]);
    let r = "pkg/returns.py";
    assert_eq!(
        found(&diags),
        vec![d(r, 58, "use_after_break", "x")],
        "{diags:#?}"
    );
    assert!(
        message(&diags[0]).ends_with("the parameter `shared` on another"),
        "{}",
        message(&diags[0])
    );
}

/// Callees in another module return what other calls return: an argument that is itself a
/// call's result, through any number of nested calls, an element of a call's result, and a call
/// whose result feeds its own argument in a loop. A nested call of a new object is new.
const NESTED_CALLEES: &str = r#"def identity(x):
    return x


def first(rows):
    return rows[0]


def nested(shared, flag):
    return identity(identity(shared)) if flag else []


def nested_element(rows, flag):
    return first(identity(rows)) if flag else []


def element_of_call(rows, flag):
    return identity(rows)[0] if flag else []


def looped(shared, times):
    x = shared
    for _ in range(times):
        x = identity(x)
    return x if times else []


def fresh_nested(flag):
    return identity(identity([])) if flag else []
"#;

const NESTED_CALLERS: &str = r#"from pkg.nested import element_of_call, fresh_nested, looped, nested, nested_element


def use_nested(shared, flag):
    x = nested(shared, flag)
    x.append(1)


def use_nested_element(rows, flag):
    x = nested_element(rows, flag)
    x.append(1)


def use_element_of_call(rows, flag):
    x = element_of_call(rows, flag)
    x.append(1)


def use_looped(shared, times):
    x = looped(shared, times)
    x.append(1)


def use_fresh_nested(flag):
    x = fresh_nested(flag)
    x.append(1)
"#;

/// A sentinel test whose sentinel side never reaches the value's phi selects no replacement:
/// here the loop's back edge passes `if out is None: raise`, and the loop header also takes the
/// list made before the loop, so `acc` is new on one path and the caller's `out` on another.
const GUARDED_LOOP: &str = r#"def gather(items, out=None):
    acc = []
    for item in items:
        if out is None:
            raise ValueError(item)
        acc = out
    acc.append(1)
    return acc
"#;

#[test]
fn mut112_reports_a_loop_value_a_raising_sentinel_test_guards() {
    let p = Project::new("mut112_guarded_loop");
    p.write("pkg/__init__.py", "")
        .write("pkg/guarded.py", GUARDED_LOOP);
    let diags = diagnostics(&p, &["--select", "MUT112"]);
    assert_eq!(
        found(&diags),
        vec![d("pkg/guarded.py", 7, "gather", "acc")],
        "{diags:#?}"
    );
}

/// What a callee returns, followed back through a `get` default (an element of the parameter, or
/// the new default), a slice (a copy of a list or of an unknown value, a view of a NumPy array),
/// and a display unpacking a parameter (a new dict).
const RETURNED_PARTS: &str = r#"import numpy as np


def rows_of(table, key):
    return table.get(key, [])


def add(table, key, row):
    rows = rows_of(table, key)
    rows.append(row)


def tail(rows):
    return rows[1:]


def use_tail(rows, flag):
    x = tail(rows) if flag else rows
    x.append(1)


def use_tail_only(rows):
    x = tail(rows)
    x.append(1)


def window(arr: np.ndarray):
    return arr[1:]


def use_window(arr: np.ndarray, flag):
    x = window(arr) if flag else []
    x[0] = 1


def use_window_only(arr: np.ndarray, flag):
    x = window(arr) if flag else arr
    x[0] = 1


def merged(base, k):
    return {**base, k: 1}


def use_merged(base, k, flag):
    x = merged(base, k) if flag else base
    x["y"] = 2
"#;

#[test]
fn mut112_follows_returned_defaults_slices_and_unpacking_displays() {
    let p = Project::new("mut112_returned_parts");
    p.write("pkg/__init__.py", "")
        .write("pkg/returns.py", RETURNED_PARTS);
    let diags = diagnostics(&p, &["--select", "MUT112"]);
    let f = "pkg/returns.py";
    assert_eq!(
        found(&diags),
        vec![
            d(f, 10, "add", "rows"),
            d(f, 19, "use_tail", "x"),
            d(f, 33, "use_window", "x"),
            d(f, 47, "use_merged", "x"),
        ],
        "{diags:#?}"
    );
    assert!(message(at(&diags, f, 10)).ends_with("an element of the parameter `table` on another"));
    assert!(message(at(&diags, f, 33)).ends_with("the parameter `arr` on another"));
}

/// Two definitions of one function under a condition, each returning through a phi: what the
/// second returns through a call of a phi's value refers to its own phis, after the first's.
const CONDITIONAL_DEFINITIONS: &str = r#"FAST = True


def identity(x):
    return x


if FAST:
    def pick(rows, flag, other):
        z = [] if flag else [0]
        return z if other else []
else:
    def pick(rows, flag, other):
        y = rows if flag else []
        return identity(y) if other else []


def use_pick(rows, flag, other):
    x = pick(rows, flag, other)
    x.append(1)
"#;

#[test]
fn mut112_follows_the_phis_of_both_conditional_definitions() {
    let p = Project::new("mut112_conditional_definitions");
    p.write("pkg/__init__.py", "")
        .write("pkg/pick.py", CONDITIONAL_DEFINITIONS);
    let diags = diagnostics(&p, &["--select", "MUT112"]);
    let f = "pkg/pick.py";
    assert_eq!(found(&diags), vec![d(f, 20, "use_pick", "x")], "{diags:#?}");
    assert!(message(at(&diags, f, 20)).ends_with("the parameter `rows` on another"));
}

#[test]
fn mut112_follows_returned_calls_through_nested_calls() {
    let p = Project::new("mut112_nested_calls");
    p.write("pkg/__init__.py", "")
        .write("pkg/nested.py", NESTED_CALLEES)
        .write("pkg/callers.py", NESTED_CALLERS);
    let diags = diagnostics(&p, &["--select", "MUT112"]);
    let c = "pkg/callers.py";
    assert_eq!(
        found(&diags),
        vec![
            d(c, 6, "use_nested", "x"),
            d(c, 11, "use_nested_element", "x"),
            d(c, 16, "use_element_of_call", "x"),
            d(c, 21, "use_looped", "x"),
        ],
        "{diags:#?}"
    );
    for (line, borrow) in [
        (6, "the parameter `shared`"),
        (11, "an element of the parameter `rows`"),
        (16, "an element of the parameter `rows`"),
        (21, "the parameter `shared`"),
    ] {
        assert!(
            message(at(&diags, c, line)).ends_with(&format!("{borrow} on another")),
            "{line}: {}",
            message(at(&diags, c, line))
        );
    }
}

/// An attribute of an object the function makes itself is no borrowed state, and a copy that a
/// flag of the caller's picks is the function's choice; an attribute of a parameter, and a copy
/// that another test picks (of the value, or of other state), still count, as does a copy whose
/// edge other branches join first.
const PICKED: &str = r#"class Collector:
    def __init__(self):
        self.errors = []


def collect(tree):
    if not tree:
        return []
    visitor = Collector()
    return visitor.errors


def validate(tree):
    errors = collect(tree)
    errors.append("checked")
    return errors


def local_attribute(flag):
    visitor = Collector()
    alias = visitor
    errors = alias.errors if flag else []
    errors.append(1)
    named = getattr(Collector(), "errors", [])
    named.append(1)
    bare = getattr(Collector(), "errors")
    bare.append(1)


def parameter_attribute(node, flag):
    errors = node.errors if flag else []
    errors.append(1)


def fill(image, mask, value, in_place=False):
    if not in_place:
        image = image.copy()
    image[mask] = value
    return image


def extended(items, copy=True):
    items = list(items) if copy else items
    items.append(1)
    return items


def by_mode(items, mode="copy"):
    chosen = list(items) if mode == "copy" else items
    chosen.append(1)


def either(items, copy, force):
    wanted = copy or force
    if wanted:
        items = list(items)
    items.append(1)


def copied_first(items, flag):
    snapshot = list(items)
    chosen = snapshot if flag else items
    chosen.append(1)


def ordered(items, order=False):
    chosen = sorted(items) if order else items
    chosen.append(1)


def drop_cached(task_ids, cached):
    ids = task_ids if isinstance(task_ids, set) else set(task_ids)
    ids.difference_update(cached)
    return ids


def restricted(caps, config):
    if config.get("bundle2"):
        caps = set(caps)
        caps.add("bundle2")
    if config.get("narrow"):
        caps.add("narrow")
    return caps


def when_given(items):
    if items:
        items = list(items)
    items.append(1)


def joined_first(items, copy, verbose):
    if copy:
        if verbose:
            print("copying")
        items = list(items)
    items.append(1)
"#;

#[test]
fn mut112_leaves_out_attributes_of_new_objects_and_copies_a_flag_picks() {
    let p = Project::new("mut112_picked");
    p.write("pkg/__init__.py", "")
        .write("pkg/picked.py", PICKED);
    let diags = diagnostics(&p, &["--select", "MUT112"]);
    assert_eq!(
        found(&diags),
        vec![
            d("pkg/picked.py", 32, "parameter_attribute", "errors"),
            d("pkg/picked.py", 68, "ordered", "chosen"),
            d("pkg/picked.py", 73, "drop_cached", "ids"),
            d("pkg/picked.py", 82, "restricted", "caps"),
            d("pkg/picked.py", 89, "when_given", "items"),
            d("pkg/picked.py", 97, "joined_first", "items"),
        ]
    );
}

/// A `MUT112` report, a suppression, and the `heki rule` page.
const ONE: &str = "def collect(name, scopes=None):\n    use = scopes or []\n    use.append(name)  # heki: ignore[MUT112]\n    return use\n\n\ndef collect_all(name, scopes=None):\n    use = scopes or []\n    use.append(name)\n    return use\n";

#[test]
fn mut112_selection_suppression_and_rule_page() {
    let p = Project::new("mut112_selection");
    p.write("pkg/__init__.py", "").write("pkg/one.py", ONE);
    assert!(diagnostics(&p, &["--select", "MUT"]).is_empty());
    let diags = diagnostics(&p, &["--select", "MUT112"]);
    assert_eq!(
        found(&diags),
        vec![d("pkg/one.py", 9, "collect_all", "use")]
    );
    assert_eq!(diags[0]["rule_slug"], "conditional-ownership-mutation");
    assert!(diags[0]["fix"].is_null());
    assert_eq!(
        found(&diagnostics(
            &p,
            &["--select", "conditional-ownership-mutation"]
        ))
        .len(),
        1
    );
    let (code, doc, _) = p.heki(&["rule", "MUT112"]);
    assert_eq!(code, 0);
    assert!(
        doc.starts_with("# conditional-ownership-mutation (MUT112)\n\n"),
        "{doc}"
    );
    let sections = [
        "\n## What it does\n",
        "\n## Why is this bad?\n",
        "\n## Example\n",
        "\nUse instead:\n",
        "\n## Fix safety\nNo automatic fix.\n",
        "\n## Options\n",
    ];
    let positions: Vec<usize> = sections
        .iter()
        .map(|s| doc.find(s).unwrap_or_else(|| panic!("{s}: {doc}")))
        .collect();
    assert!(positions.is_sorted(), "{doc}");
}

// ================================================================================== LSP

const LSP_CONFIG: &str =
    "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"MUT112\"]\n";
const LSP_PREPARE_COPY: &str =
    "def prepare(config):\n    if config is None:\n        return {}\n    return dict(config)\n";
const LSP_PREPARE_SAME: &str =
    "def prepare(config):\n    if config is None:\n        return {}\n    return config\n";
const LSP_USE: &str = "from pkg.prepare import prepare\n\n\ndef configure(config):\n    cfg = prepare(config)\n    cfg[\"debug\"] = True\n    return cfg\n";
const LSP_BASE: &str =
    "class Box:\n    def __init__(self, items=None):\n        self.items = items or []\n";
const LSP_SUB: &str = "from pkg.base import Box\n\n\nclass Bag(Box):\n    def order(self):\n        return sorted(self.items)\n";
const LSP_SUB_SORTING: &str = "from pkg.base import Box\n\n\nclass Bag(Box):\n    def order(self):\n        self.items.sort()\n";

/// Edit, write, and save `rel` (the way an editor saves a changed document).
fn save(lsp: &mut Lsp, p: &Project, rel: &str, version: i32, text: &str) {
    let u = uri(&p.dir.join(rel));
    lsp.change(&u, version, text);
    p.write(rel, text);
    lsp.notify(
        "textDocument/didSave",
        json!({"textDocument": {"uri": u}, "text": text}),
    );
}

#[test]
fn mut112_lsp_follows_returns_and_class_families_in_other_files() {
    let p = Project::new("mut112_lsp");
    p.write("pyproject.toml", LSP_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/prepare.py", LSP_PREPARE_COPY)
        .write("pkg/use.py", LSP_USE)
        .write("pkg/base.py", LSP_BASE)
        .write("pkg/sub.py", LSP_SUB);
    let use_ = uri(&p.dir.join("pkg/use.py"));
    let base = uri(&p.dir.join("pkg/base.py"));
    let mut lsp = Lsp::start(&p.dir, &["server", "--stdio"]);
    lsp.request(
        "initialize",
        json!({"processId": null, "rootUri": uri(&p.dir), "capabilities": {}}),
    );
    lsp.notify("initialized", json!({}));
    lsp.ready();
    lsp.open(&use_, LSP_USE);
    lsp.open(&base, LSP_BASE);
    lsp.diagnostics(&use_, <[Value]>::is_empty);
    lsp.diagnostics(&base, <[Value]>::is_empty);

    // The callee in another file stops copying its argument: the caller's result is new on one
    // path and the caller's parameter on the other.
    lsp.open(&uri(&p.dir.join("pkg/prepare.py")), LSP_PREPARE_COPY);
    save(&mut lsp, &p, "pkg/prepare.py", 2, LSP_PREPARE_SAME);
    let diags = lsp.diagnostics(&use_, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["MUT112"]);
    assert_eq!(
        diags[0]["range"]["start"],
        json!({"line": 5, "character": 4})
    );
    save(&mut lsp, &p, "pkg/prepare.py", 3, LSP_PREPARE_COPY);
    lsp.diagnostics(&use_, <[Value]>::is_empty);

    // A method of a subclass in another file starts changing the stored attribute in place (its
    // class keeps its methods): the store in the base class is reported.
    lsp.open(&uri(&p.dir.join("pkg/sub.py")), LSP_SUB);
    save(&mut lsp, &p, "pkg/sub.py", 2, LSP_SUB_SORTING);
    let diags = lsp.diagnostics(&base, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["MUT112"]);
    assert_eq!(
        diags[0]["range"]["start"],
        json!({"line": 2, "character": 8})
    );
    save(&mut lsp, &p, "pkg/sub.py", 3, LSP_SUB);
    lsp.diagnostics(&base, <[Value]>::is_empty);
    lsp.stop(true);
}

/// A shallow copy is a new object holding the elements of what it copies: an item of a copy of
/// a parameter is the parameter's item, borrowed, also when a callee returns it.
const COPIED_ELEMENTS: &str = r#"import copy


def first_tags(rows):
    return list(rows)[0]


def tags_of(options, flag):
    tags = dict(options)["tags"] if flag else []
    tags.append("x")


def attr_of(node, flag):
    children = copy.copy(node).children if flag else []
    children.append(1)


def returned(rows, flag):
    tags = first_tags(rows) if flag else []
    tags.append("x")


def deep(options, flag):
    tags = copy.deepcopy(options)["tags"] if flag else []
    tags.append("x")


def the_copy_itself(options, flag):
    merged = dict(options) if flag else {}
    merged["k"] = 1
"#;

#[test]
fn mut112_takes_an_element_of_a_shallow_copy_as_the_copied_values() {
    let p = Project::new("mut112_copied_elements");
    p.write("pkg/__init__.py", "")
        .write("pkg/copies.py", COPIED_ELEMENTS);
    let diags = diagnostics(&p, &["--select", "MUT112"]);
    let f = "pkg/copies.py";
    assert_eq!(
        found(&diags),
        vec![
            d(f, 10, "tags_of", "tags"),
            d(f, 15, "attr_of", "children"),
            d(f, 20, "returned", "tags"),
        ],
        "{diags:#?}"
    );
}

/// An in-place operator on an instance of a project subclass of a builtin container changes it
/// as on the builtin; one on a number binds a new object.
#[test]
fn mut112_an_in_place_operator_changes_a_subclass_of_a_builtin() {
    let p = Project::new("mut112_builtin_subclass");
    p.write(
        "m.py",
        r#"import typing


class Items(list):
    pass


def subclass_cast_back(shared, fresh):
    x = Items() if fresh else typing.cast(Items, shared)
    x += [1]


def int_or_shared(shared, fresh):
    n = 0 if fresh else typing.cast(int, shared)
    n += 1
    return n
"#,
    );
    let diags = diagnostics(&p, &["--extend-select", "MUT112"]);
    assert_eq!(
        found(&diags),
        vec![d("m.py", 10, "subclass_cast_back", "x")]
    );
}

/// Values stored at constant keys of builtin containers and read back: a key holding a value new
/// on one path and the caller's on another is followed as a variable is, through a display, `get`,
/// a branch, a loop, an unpacking before it, and a `try`. Not followed: another key, a negative
/// one, a key opened between the store and the read (a store with an unknown key or `True`, a
/// deletion, `update`, `|=`, a call the container is handed to, any call or `yield` for a
/// parameter or a container stored elsewhere, a store through another container that may be it,
/// an unpacking after it), a container of unknown class, a key overwritten, and a new object
/// stored in the caller's state (`opts["s"] = {}` where the key is missing), which is that
/// state's on every path.
const CONSTANT_KEYS: &str = r#"def log():
    pass


def prepare(d):
    pass


def local_slot(headers=None):
    kwargs = {}
    kwargs["headers"] = headers or {}
    kwargs["headers"]["X"] = "1"
    return kwargs


def param_slot(opts: dict, headers=None):
    opts["headers"] = headers or {}
    opts["headers"]["X"] = "1"


def display_slot(headers=None):
    kwargs = {"timeout": 1, "headers": headers or {}}
    kwargs["headers"]["X"] = "1"
    return kwargs


def read_by_get(headers=None):
    kwargs = {}
    kwargs["headers"] = headers or {}
    kwargs.get("headers")["X"] = "1"
    return kwargs


def list_item(rows=None):
    table = [None]
    table[0] = rows or []
    table[0].append(1)
    return table


def after_branch(flag, headers=None):
    kwargs = {}
    if flag:
        kwargs["headers"] = headers or {}
    else:
        kwargs["headers"] = {"X": "0"}
    kwargs["headers"]["X"] = "1"
    return kwargs


def unpacked_before(extra, headers=None):
    kwargs = {**extra, "headers": headers or {}}
    kwargs["headers"]["X"] = "1"
    return kwargs


def in_loop(items, headers=None):
    out = []
    for item in items:
        kwargs = {}
        kwargs["headers"] = headers or {}
        kwargs["headers"][item] = "1"
        out.append(kwargs)
    return out


def configure(opts: dict):
    if "stream" not in opts:
        opts["stream"] = {}
    opts["stream"]["usage"] = True


def new_stored():
    kwargs = {}
    kwargs["headers"] = {}
    kwargs["headers"]["X"] = "1"
    return kwargs


def settled(opts: dict):
    opts["s"] = opts.get("s") or {}
    opts["s"]["u"] = 1


def any_key(name, headers=None):
    kwargs = {}
    kwargs["headers"] = headers or {}
    kwargs[name] = "1"
    kwargs["headers"]["X"] = "1"
    return kwargs


def updated_between(extra, headers=None):
    kwargs = {}
    kwargs["headers"] = headers or {}
    kwargs.update(extra)
    kwargs["headers"]["X"] = "1"
    return kwargs


def handed(headers=None):
    kwargs = {}
    kwargs["headers"] = headers or {}
    prepare(kwargs)
    kwargs["headers"]["X"] = "1"
    return kwargs


def param_then_call(opts: dict, headers=None):
    opts["headers"] = headers or {}
    log()
    opts["headers"]["X"] = "1"


def untyped_param(opts, headers=None):
    opts["headers"] = headers or {}
    opts["headers"]["X"] = "1"


class Client:
    def escaped_then_call(self, headers=None):
        kwargs = {}
        self.kwargs = kwargs
        kwargs["headers"] = headers or {}
        log()
        kwargs["headers"]["X"] = "1"

    def through_another(self, headers=None):
        kwargs = {}
        self.kwargs = kwargs
        kwargs["headers"] = headers or {}
        self.kwargs["headers"] = {}
        kwargs["headers"]["X"] = "1"


def other_key(headers=None):
    kwargs = {}
    kwargs["a"] = headers or {}
    kwargs["b"] = {}
    kwargs["b"]["X"] = "1"
    return kwargs


def overwritten(headers=None):
    kwargs = {}
    kwargs["h"] = headers or {}
    kwargs["h"] = {}
    kwargs["h"]["X"] = "1"
    return kwargs


def negative_index(rows=None):
    table = [None]
    table[0] = rows or []
    table[-1].append(1)
    return table


def unpacked_after(extra, headers=None):
    kwargs = {"headers": headers or {}, **extra}
    kwargs["headers"]["X"] = "1"
    return kwargs


def aliased(flag, headers=None):
    kwargs = {}
    kwargs["headers"] = headers or {}
    other = kwargs if flag else {}
    other["headers"] = {}
    kwargs["headers"]["X"] = "1"
    return kwargs


def deleted_between(headers=None):
    kwargs = {"timeout": 1}
    kwargs["headers"] = headers or {}
    del kwargs["timeout"]
    kwargs["headers"]["X"] = "1"
    return kwargs


def param_then_unknown_key(opts: dict, cache: dict, name, headers=None):
    opts["headers"] = headers or {}
    del cache[name]
    opts["headers"]["X"] = "1"


def merged_in_place(extra, headers=None):
    kwargs = {}
    kwargs["headers"] = headers or {}
    kwargs |= extra
    kwargs["headers"]["X"] = "1"
    return kwargs


def streamed(opts: dict, headers=None):
    opts["headers"] = headers or {}
    yield
    opts["headers"]["X"] = "1"


def flag_key(headers=None):
    kwargs = {1: headers or {}}
    kwargs[True] = {}
    kwargs[1]["X"] = "1"
    return kwargs


def in_try(headers=None):
    kwargs = {}
    try:
        kwargs["headers"] = headers or {}
        log()
    except ValueError:
        kwargs["headers"] = {}
    kwargs["headers"]["X"] = "1"
    return kwargs
"#;

#[test]
fn mut112_follows_values_stored_at_constant_keys() {
    let p = Project::new("mut112_constant_keys");
    p.write("pkg/__init__.py", "")
        .write("pkg/keys.py", CONSTANT_KEYS);
    let diags = diagnostics(&p, &["--select", "MUT112"]);
    let k = "pkg/keys.py";
    assert_eq!(
        found(&diags),
        vec![
            d(k, 12, "local_slot", "kwargs[\"headers\"]"),
            d(k, 18, "param_slot", "opts[\"headers\"]"),
            d(k, 23, "display_slot", "kwargs[\"headers\"]"),
            d(k, 30, "read_by_get", "kwargs.get(\"headers\")"),
            d(k, 37, "list_item", "table[0]"),
            d(k, 47, "after_branch", "kwargs[\"headers\"]"),
            d(k, 53, "unpacked_before", "kwargs[\"headers\"]"),
            d(k, 62, "in_loop", "kwargs[\"headers\"]"),
            d(k, 216, "in_try", "kwargs[\"headers\"]"),
        ],
        "{diags:#?}"
    );
    assert_eq!(
        message(at(&diags, k, 12)),
        "`local_slot` changes `kwargs[\"headers\"]` in place, which is a new object on one path and the parameter `headers` on another"
    );
}
