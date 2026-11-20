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
