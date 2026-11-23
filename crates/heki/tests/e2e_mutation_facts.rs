//! What the mutation rules (`MUT101`, `MUT107`, `MUT110`, `MUT114`, `MUT202`) take as a cache, an
//! invalidation, a shared value, and a change: decorators known by their binding, invalidation on
//! every path, inherited writers, copies, and generator bodies (end to end, through the real
//! binary).

mod common;

use common::Project;
use serde_json::Value;

fn project(name: &str, config: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!("[project]\nname = \"t\"\n\n{config}"),
    );
    p
}

/// `(path, line, qualname)` of every diagnostic of `rule`.
fn reports(p: &Project, rule: &str) -> Vec<(String, u64, String)> {
    let (code, stdout, stderr) = p.check(&["--select", rule, "--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    diags
        .iter()
        .map(|d| {
            assert_eq!(d["rule_code"], rule, "{d:#}");
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["qualname"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn at(path: &str, line: u64, qualname: &str) -> (String, u64, String) {
    (path.to_owned(), line, qualname.to_owned())
}

#[test]
fn cache_decorators_are_known_by_what_their_name_is_bound_to() {
    let p = project("cache_decorator_bindings", "");
    p.write(
        "aliased.py",
        r#"from functools import cached_property as cached


class C:
    def __init__(self, n: int) -> None:
        self.n = n

    @cached
    def doubled(self) -> int:
        return self.n * 2

    def set_n(self, n: int) -> None:
        self.n = n
"#,
    )
    .write(
        "memo.py",
        r#"import functools
from functools import lru_cache as _lru_cache


def cache(fn):
    return fn


@cache
def fresh() -> list[int]:
    return [1]


@_lru_cache
def shared() -> list[int]:
    return [1]


@functools.cache
def also_shared() -> list[int]:
    return [1]


def use() -> None:
    fresh().append(2)
    shared().append(2)
    also_shared().append(2)
"#,
    );
    assert_eq!(reports(&p, "MUT101"), vec![at("aliased.py", 13, "C.set_n")]);
    // `cache` is the module's own identity decorator: `fresh()` returns a new list.
    assert_eq!(
        reports(&p, "MUT114"),
        vec![at("memo.py", 26, "use"), at("memo.py", 27, "use")]
    );
}

#[test]
fn only_memo_dictionary_operations_are_exempt_by_name() {
    let p = project("memo_names", "");
    p.write(
        "app.py",
        r#"from functools import cached_property


class C:
    def __init__(self, n: int) -> None:
        self.memory_limit = n
        self.cache_size = n
        self._cache: dict[int, int] = {}
        self._resultCache: dict[int, int] = {}

    @cached_property
    def summary(self) -> int:
        return self.memory_limit + self.cache_size + len(self._cache) + len(self._resultCache)

    def set_limit(self, n: int) -> None:
        self.memory_limit = n

    def resize(self, n: int) -> None:
        self.cache_size = n

    def remember(self, k: int, v: int) -> None:
        self._cache[k] = v
        self._resultCache[k] = v

    def __hash__(self) -> int:
        return hash((self.memory_limit, self.cache_size, len(self._cache)))
"#,
    );
    // Rebinding a scalar whose name mentions `memory` or `cache` changes what the cache and the
    // hash were computed from; filling a memo dictionary in place does not count.
    let expected = vec![
        at("app.py", 16, "C.set_limit"),
        at("app.py", 19, "C.resize"),
    ];
    assert_eq!(reports(&p, "MUT101"), expected);
    assert_eq!(reports(&p, "MUT107"), expected);
}

#[test]
fn an_in_place_change_of_an_attribute_of_several_container_types_is_a_change() {
    let p = project("union_container", "");
    p.write(
        "app.py",
        r#"from functools import cached_property


class C:
    def __init__(self, tags: list[str] | set[str]) -> None:
        self.tags: list[str] | set[str] = tags

    @cached_property
    def count(self) -> int:
        return len(self.tags)

    def reset(self) -> None:
        self.tags.clear()
"#,
    );
    // The attribute holds a list or a set: the change is one of either, so the cached count
    // goes stale whichever it is.
    assert_eq!(reports(&p, "MUT101"), vec![at("app.py", 13, "C.reset")]);
}

const BASE: &str = r#"class Base:
    def __init__(self, name: str) -> None:
        self.name = name

    def rename(self, name: str) -> None:
        self.name = name

    def retitle(self, name: str) -> None:
        self.name = name
"#;

#[test]
fn methods_a_class_inherits_write_what_its_caches_and_hash_read() {
    let p = project("inherited_writers", "");
    p.write("pkg/__init__.py", "")
        .write("pkg/base.py", BASE)
        .write(
            "pkg/child.py",
            r#"from functools import cached_property

from pkg.base import Base


class Child(Base):
    @cached_property
    def label(self) -> str:
        return self.name.upper()

    def retitle(self, name: str) -> None:
        self.name = name
        del self.label

    def __hash__(self) -> int:
        return hash(self.name)
"#,
        );
    // `Base.rename` runs on `Child` instances; `Base.retitle` does not (`Child` overrides it,
    // and its override invalidates the cache).
    assert_eq!(
        reports(&p, "MUT101"),
        vec![at("pkg/base.py", 6, "Base.rename")]
    );
    assert_eq!(
        reports(&p, "MUT107"),
        vec![
            at("pkg/base.py", 6, "Base.rename"),
            at("pkg/child.py", 12, "Child.retitle"),
        ]
    );
}

#[test]
fn a_hash_of_an_attribute_value_itself_leaves_that_values_changes_to_its_own_class() {
    let p = project("hashed_whole", "");
    p.write("pkg/__init__.py", "")
        .write(
            "pkg/seq.py",
            r#"class Sequence:
    def __init__(self, data):
        self._data = data

    def strip(self, chars):
        if not isinstance(self._data, bytearray):
            raise TypeError("the sequence is immutable")
        self._data[:] = self._data.strip(chars)
        return self

    def replace(self, data):
        self._data = data


class TableWriter:
    def __init__(self):
        self.items = []

    def __hash__(self):
        # Hashable once `done` has made the items a tuple.
        return hash(self.items)

    def write(self, item):
        self.items.append(item)

    def done(self):
        self.items = tuple(self.items)


class Proxy:
    def __init__(self, wrapped):
        self.__wrapped__ = wrapped
        self.kind = "proxy"

    def __hash__(self):
        wrapped = self.__wrapped__
        return hash((self.kind, wrapped))

    def __setitem__(self, key, value):
        self.__wrapped__[key] = value


class Counted:
    def __init__(self):
        self.items = []

    def __hash__(self):
        return hash(self.items) ^ len(self.items)

    def write(self, item):
        self.items.append(item)


class Copied:
    def __init__(self):
        self.items = []

    def __hash__(self):
        return hash(tuple(self.items))

    def write(self, item):
        self.items.append(item)


class Measured:
    def __init__(self):
        self.items = []

    def __hash__(self):
        return hash(self.items) ^ self._size()

    def _size(self):
        return len(self.items)

    def write(self, item):
        self.items.append(item)


class Keyed:
    def __init__(self, key):
        self.key = key

    def __hash__(self):
        return self.key.__hash__()

    def extend(self, part):
        self.key.append(part)


class Indexed:
    def __init__(self):
        self.items = []

    def __hash__(self):
        return hash(self.items[0])

    def write(self, item):
        self.items.append(item)


class Typed:
    def __init__(self):
        self.items = []

    def __hash__(self):
        return hash((type(self), self.items))

    def write(self, item):
        self.items.append(item)
"#,
        )
        .write(
            "pkg/frozen.py",
            r#"from pkg.seq import Sequence


class FrozenSequence(Sequence):
    def __hash__(self):
        return hash(self._data)
"#,
        );
    // `hash(self._data)` hashes the value itself: a `bytearray` (the only value `strip` changes
    // in place), a list, or a container behind a proxy cannot be hashed at all, and a project
    // value answers for its own hash. Rebinding the attribute, and a hash that also reads the
    // length, a copy, an item, or a helper's view of it, or that hands `self` on, still changes
    // with the value.
    assert_eq!(
        reports(&p, "MUT107"),
        vec![
            at("pkg/seq.py", 12, "Sequence.replace"),
            at("pkg/seq.py", 27, "TableWriter.done"),
            at("pkg/seq.py", 51, "Counted.write"),
            at("pkg/seq.py", 62, "Copied.write"),
            at("pkg/seq.py", 76, "Measured.write"),
            at("pkg/seq.py", 98, "Indexed.write"),
            at("pkg/seq.py", 109, "Typed.write"),
        ]
    );
}

#[test]
fn a_writer_whose_class_hashes_otherwise_leaves_the_base_hash_alone() {
    let p = project("hash_owner", "");
    p.write("pkg/__init__.py", "")
        .write(
            "pkg/fields.py",
            r#"class Field:
    def __init__(self, name):
        self.name = name

    def __hash__(self):
        return hash(self.name)

    def relabel(self, name):
        self.name = name


class MetaField(Field):
    name = None
"#,
        )
        .write(
            "pkg/keys.py",
            r#"import threading

from pkg.fields import Field, MetaField


class CompositeKey(MetaField):
    def __init__(self, *field_names):
        self.field_names = field_names

    def __hash__(self):
        return hash(self.field_names)

    def bind(self, name):
        self.name = name


class Identity(Field):
    def __eq__(self, other):
        return self is other

    def rename(self, name):
        self.name = name


class Column(Field):
    def rename(self, name):
        self.name = name


class Restored(Field):
    def __eq__(self, other):
        return self is other

    __hash__ = Field.__hash__

    def rename(self, name):
        self.name = name


class Worker(threading.Thread, Field):
    def rename(self, name):
        self.name = name


class Labeled(MetaField):
    def __hash__(self):
        return hash(("labeled", self.name))


class Tagged(MetaField):
    def __hash__(self):
        return hash(("tagged", self.name))
"#,
        );
    // `CompositeKey` instances hash with their own `__hash__`, and `Identity` instances (an
    // `__eq__` without `__hash__`) cannot be hashed: neither writer changes a `Field` hash.
    // `Restored` hashes with `Field.__hash__` again, and so does `Worker` (heki does not read
    // what the external class before `Field` defines). `Field.relabel` runs on instances of every
    // class here, and is reported once.
    assert_eq!(
        reports(&p, "MUT107"),
        vec![
            at("pkg/fields.py", 9, "Field.relabel"),
            at("pkg/keys.py", 27, "Column.rename"),
            at("pkg/keys.py", 37, "Restored.rename"),
            at("pkg/keys.py", 42, "Worker.rename"),
        ]
    );
}

#[test]
fn private_helpers_initialization_reaches_through_other_helpers_initialize() {
    let p = project("init_helper_chains", "");
    p.write(
        "app.py",
        r#"class Key:
    def __init__(self, data=None, filename=None):
        self.public = None
        if data is not None:
            self._from_data(data)
        if filename is not None:
            self._from_file(filename)

    def __hash__(self):
        return hash(self.public)

    def _from_data(self, data):
        self._decode(data)

    def _from_file(self, filename):
        with open(filename, "rb") as f:
            self._decode(f.read())

    def _decode(self, data):
        self.public = data[:32]

    def _reload(self, data):
        self.public = data

    def refresh(self, data):
        self._reload(data)


class Client:
    def __init__(self, uri):
        self.options = None
        self._topology()

    def __hash__(self):
        return hash(self.options)

    def _topology(self):
        self._resolve()

    def _resolve(self):
        self.options = "resolved"

    def query(self):
        self._topology()
"#,
    );
    // `_decode` runs only inside initialization (through `_from_data` / `_from_file`);
    // `_reload` runs from the public `refresh` instead, and `_resolve` through `_topology`,
    // which `query` calls too.
    assert_eq!(
        reports(&p, "MUT107"),
        vec![
            at("app.py", 23, "Key._reload"),
            at("app.py", 41, "Client._resolve")
        ]
    );
}

#[test]
fn a_write_leaves_no_stale_cached_value_on_any_path() {
    let p = project("invalidation_paths", "");
    p.write(
        "app.py",
        r#"from functools import cached_property


class C:
    def __init__(self, n: int) -> None:
        self.n = n

    @cached_property
    def doubled(self) -> int:
        return self.n * 2

    def _reset(self) -> None:
        del self.doubled

    def only_if(self, n: int, clear: bool) -> None:
        self.n = n
        if clear:
            self.__dict__.pop("doubled", None)

    def before(self, n: int) -> None:
        self.__dict__.pop("doubled", None)
        self.n = n

    def before_reading(self, n: int) -> int:
        self.__dict__.pop("doubled", None)
        old = self.doubled
        self.n = n
        return old

    def always(self, n: int) -> None:
        self.n = n
        self.__dict__.pop("doubled", None)

    def both_branches(self, n: int, fast: bool) -> None:
        self.n = n
        if fast:
            del self.doubled
        else:
            self._reset()

    def through_helper(self, n: int) -> None:
        self.n = n
        self._reset()

    def or_raise(self, n: int) -> None:
        self.n = n
        if n < 0:
            raise ValueError(n)
        self.__dict__.clear()
"#,
    );
    // An invalidation before the write also leaves nothing stale when nothing reads the
    // property in between (`before`); reading it computes the old value again
    // (`before_reading`).
    assert_eq!(
        reports(&p, "MUT101"),
        vec![
            at("app.py", 16, "C.only_if"),
            at("app.py", 27, "C.before_reading")
        ]
    );
}

const DEEP_SHARED: &str = "[[tool.heki.readonly-returns]]\nname = \"shared\"\nproducers = [\"app.shared\"]\ndeep = true\n";

#[test]
fn a_value_shared_on_one_branch_stays_shared_after_the_merge() {
    let p = project("shared_merge", DEEP_SHARED);
    p.write(
        "app.py",
        r#"CACHE = [[1], [2]]


def shared() -> list[list[int]]:
    return CACHE


def copy_first(flag: bool) -> None:
    x = shared().copy() if flag else shared()
    x.clear()


def copy_second(flag: bool) -> None:
    x = shared() if flag else shared().copy()
    x.clear()


def in_branches(flag: bool) -> None:
    if flag:
        x = shared().copy()
    else:
        x = shared()
    x.clear()


def copies_only(flag: bool) -> None:
    x = shared().copy() if flag else list(shared())
    x.clear()
"#,
    );
    assert_eq!(
        reports(&p, "MUT202"),
        vec![
            at("app.py", 10, "copy_first"),
            at("app.py", 15, "copy_second"),
            at("app.py", 23, "in_branches"),
        ]
    );
}

#[test]
fn container_constructors_and_unpacking_displays_copy_only_the_top_level() {
    let p = project("shared_constructors", "");
    p.write(
        "app.py",
        r#"from functools import lru_cache


@lru_cache(maxsize=None)
def schema(name):
    return {"fields": [name], "meta": {"a": 1}}


@lru_cache(maxsize=None)
def rows(name):
    return [[name]]


def copied(name):
    return dict(schema(name))


def splatted(name):
    return {**schema(name)}


def extend(name):
    s = dict(schema(name))
    s["fields"].append("x")


def splat(name):
    s = {**schema(name)}
    s["meta"]["b"] = 2


def first_row(name):
    r = list(rows(name))
    r[0].append("x")


def each_row(name):
    for r in tuple(rows(name)):
        r.append("x")


def unpacked(name):
    r = [*rows(name)]
    r[0].clear()


def through_wrapper(name):
    copied(name)["fields"].append("x")
    splatted(name)["meta"]["b"] = 2


def retitle(name):
    s = dict(schema(name))
    s["fields"] = ["x"]
    t = {**schema(name), "extra": []}
    t["extra"].append(1)
    fresh = {"fields": []}
    u = {**schema(name), **fresh}
    u["fields"].append(1)
    w = dict(schema(name), fields=[])
    w["fields"].append(1)
    r = list(rows(name))
    r.append(["y"])
    copied(name)["new"] = 1
"#,
    );
    // The new container holds the cached one's values: changing them changes the cache, while
    // changing the new container's own items does not. A dict adding values of its own is not
    // followed (`t["extra"]` is its own list, and so are `u["fields"]` and `w["fields"]`).
    assert_eq!(
        reports(&p, "MUT114"),
        vec![
            at("app.py", 24, "extend"),
            at("app.py", 29, "splat"),
            at("app.py", 34, "first_row"),
            at("app.py", 39, "each_row"),
            at("app.py", 44, "unpacked"),
            at("app.py", 48, "through_wrapper"),
            at("app.py", 49, "through_wrapper"),
        ]
    );
}

#[test]
fn a_copy_method_of_a_project_class_copies_as_deeply_as_its_body() {
    let p = project("project_copies", "");
    p.write(
        "app.py",
        r#"from functools import lru_cache


class Image:
    def __init__(self):
        self.info = {}

    def copy(self):
        return self._new()

    def _new(self):
        new = Image()
        new.info = self.info.copy()
        return new


@lru_cache(maxsize=None)
def cached_image(name) -> Image:
    return Image()


@lru_cache(maxsize=None)
def cached_info(name) -> dict[str, list[str]]:
    return {"modes": [name]}


def hopper(name) -> Image:
    return cached_image(name).copy()


def mark(name):
    im = hopper(name)
    im.info["transparency"] = 0


def mark_cached(name):
    cached_image(name).info["transparency"] = 0


def extend_info(name):
    info = cached_info(name).copy()
    info["modes"].append("RGB")
"#,
    );
    // `Image.copy` gives the copy a new `info`; a dict's `copy` shares the lists it holds.
    assert_eq!(
        reports(&p, "MUT114"),
        vec![
            at("app.py", 37, "mark_cached"),
            at("app.py", 42, "extend_info"),
        ]
    );
}

#[test]
fn a_change_below_a_shallow_copy_of_a_parameter_changes_the_parameter() {
    let p = project("param_copies", "");
    p.write(
        "app.py",
        r#"import copy
from functools import lru_cache


class Post:
    def __init__(self, title, tags):
        self.title = title
        self.tags = tags


@lru_cache(maxsize=None)
def post(name):
    return Post(name, [name])


def tag(p, t):
    draft = copy.copy(p)
    draft.tags.append(t)
    return draft


def tag_copy_of_copy(p, t):
    draft = copy.copy(copy.copy(p))
    draft.tags.append(t)


def retitle(p, title):
    draft = copy.copy(p)
    draft.title = title
    return draft


def retag(p, t):
    draft = copy.copy(p)
    draft.tags = []
    draft.tags.append(t)
    return draft


def retag_by_name(p, t):
    draft = copy.copy(p)
    setattr(draft, "tags", [])
    draft.tags.append(t)
    return draft


def retag_by_dict(p, t):
    draft = copy.copy(p)
    draft.__dict__.update(tags=[])
    draft.tags.append(t)
    return draft


def retag_by_vars(p, t):
    draft = copy.copy(p)
    vars(draft)["tags"] = []
    draft.tags.append(t)
    return draft


def retag_in_place(p, t):
    draft = copy.copy(p)
    draft.__dict__ |= {"tags": []}
    draft.tags.append(t)
    return draft


def use(name):
    tag(post(name), "x")
    tag_copy_of_copy(post(name), "x")
    retitle(post(name), "y")
    retag(post(name), "z")
    retag_by_name(post(name), "z")
    retag_by_dict(post(name), "z")
    retag_by_vars(post(name), "z")
    retag_in_place(post(name), "z")
"#,
    );
    // The copy holds the cached post's tag list; its own title is its own, and so is a tag list
    // it is given before it changes it (also through its namespace: `vars(draft)`, `|=`).
    assert_eq!(
        reports(&p, "MUT114"),
        vec![at("app.py", 69, "use"), at("app.py", 70, "use")]
    );
}

#[test]
fn filling_a_memo_of_a_shared_object_leaves_what_it_computes_alone() {
    let p = project("shared_memos", "");
    p.write("pkg/__init__.py", "")
        .write(
            "pkg/strip.py",
            r#"class FifoCache:
    def __init__(self):
        self._entries = {}
        self.hits = 0

    def get(self, key):
        self.hits += 1
        return self._entries.get(key)

    def set(self, key, value):
        self._entries[key] = value


class Strip:
    def __init__(self, segments):
        self._segments = segments
        self._glyph_cache = FifoCache()
        self.counts = FifoCache()
        self._style_cache = {}
        self._runtime = None
        self._decoders = {}
        self._ranges = None
        self.history = []

    def apply_style(self, style):
        cached = self._style_cache.get(style)
        if cached is not None:
            return cached
        styled = Strip([(s, style) for s in self._segments])
        self._style_cache[style] = styled
        return styled

    def runtime(self):
        if self._runtime is None:
            self._runtime = object()
        return self._runtime

    def decoder(self, config):
        if config not in self._decoders:
            self._decoders[config] = object()
        return self._decoders[config]

    def default_decoder(self):
        if "default" not in self._decoders:
            self._decoders["default"] = object()
        if 0 not in self._decoders:
            self._decoders[0] = object()

    def misfill(self):
        if "probe" not in self._decoders:
            self._decoders["live"] = object()

    def refill(self, tested, stored):
        if tested not in self._decoders:
            self._decoders[stored] = object()

    def contains(self, value):
        bounds = self._ranges
        if bounds is None:
            bounds = self._ranges = sorted(self._segments)
        return value in bounds

    def glyph(self, ch):
        found = self._glyph_cache.get(ch)
        if found is None:
            found = ch.upper()
            self._glyph_cache.set(ch, found)
        return found

    def count(self, ch):
        self.counts.set(ch, 1)

    def record(self, event):
        self.history.append(event)

    def reset_styles(self):
        self._style_cache = {}


def known_bases(node):
    if node.bases_known is None:
        node.bases_known = all(node.bases)
    return node.bases_known


def copy_runtime(source, target):
    if source._runtime is None:
        target._runtime = object()
"#,
        )
        .write(
            "pkg/render.py",
            r#"from functools import lru_cache

from pkg.strip import Strip, copy_runtime, known_bases


@lru_cache(maxsize=None)
def blank(width) -> Strip:
    return Strip([" " * width])


def render(width, style):
    line = blank(width)
    line.apply_style(style)
    line.runtime()
    line.decoder(style)
    line.default_decoder()
    line.contains(" ")
    known_bases(line)
    line.glyph("a")
    line._style_cache[style] = None
    return line


def render_logged(width):
    line = blank(width)
    line.misfill()
    line.refill("a", "b")
    line.count("a")
    copy_runtime(Strip([]), line)
    other = blank(width + 1)
    if other._runtime is None:
        line._runtime = 1
    line.record("render")
    line.reset_styles()
    line.history.append("direct")
"#,
        );
    // A memo (by name, also a cache object whose methods change it, or filled under `not in` the
    // key it stores) and a lazy initialization (also through a local copy of the attribute, also
    // by a function taking the object) only cache what the object computes; storing a key other
    // than the one found missing, counting into an object of the same class not named as a memo,
    // setting an attribute that a test of another object found unset, appending to its history,
    // or replacing a memo changes it.
    assert_eq!(
        reports(&p, "MUT114"),
        vec![
            at("pkg/render.py", 26, "render_logged"),
            at("pkg/render.py", 27, "render_logged"),
            at("pkg/render.py", 28, "render_logged"),
            at("pkg/render.py", 29, "render_logged"),
            at("pkg/render.py", 32, "render_logged"),
            at("pkg/render.py", 33, "render_logged"),
            at("pkg/render.py", 34, "render_logged"),
            at("pkg/render.py", 35, "render_logged"),
        ]
    );
}

/// The memo fills and lazy initializations written without a `None` test of the attribute or a
/// `not in` test of the memo: `hasattr`, `getattr(..., None)`, `try` / `except AttributeError`,
/// a store through `object.__setattr__`, `setattr`, or `__setattr__`, a fill under a `None` or
/// falsy `.get(k)`, the `else` of a `k in` test, a key computed twice by the same expression
/// (a call, a method, a tuple, an attribute), and a container a helper has as its default.
#[test]
fn memo_fills_written_by_hand_leave_a_shared_object_alone() {
    let p = project("shared_memo_shapes", "");
    p.write(
        "node.py",
        r#"from functools import lru_cache


class Node:
    def __init__(self, name):
        self.name = name
        self._h = None
        self._u = None
        self._index = {}

    def by_hasattr(self):
        if not hasattr(self, "_size"):
            self._size = len(self.name)
        return self._size

    def by_getattr(self):
        found = getattr(self, "_upper", None)
        if found is None:
            found = self._upper = self.name.upper()
        return found

    def by_try(self):
        try:
            return self._lower
        except KeyError:
            raise
        except AttributeError:
            self._lower = self.name.lower()
            return self._lower

    def __hash__(self):
        if self._h is None:
            object.__setattr__(self, "_h", hash(self.name))
        return self._h

    def by_setattr(self):
        if self._u is None:
            setattr(self, "_u", self.name)
            self.__setattr__("_u", self.name)
        return self._u

    def by_get(self, key):
        found = self._index.get(key.lower())
        if not found:
            found = self._index[key.lower()] = len(key)
        return found

    def by_in(self, a, b):
        if (a, b) in self._index:
            return self._index[(a, b)]
        self._index[(a, b)] = 0
        return 0

    def by_identity(self, raw):
        if self._index.get(id(raw)) is None:
            self._index[id(raw)] = raw.name
        return self._index[id(raw)]

    def by_attribute(self, raw):
        if raw.name not in self._index:
            self._index[raw.name] = 1

    def other_error(self):
        try:
            return self._title
        except KeyError:
            self._title = self.name.title()
            return self._title

    def bare_except(self):
        try:
            return self._label
        except:
            self._label = self.name
            return self._label

    def other_key(self):
        if "probe" not in self._index:
            self._index[KEY] = 0

    def closes(self):
        try:
            self._closed = True
        finally:
            if self.name:
                self._closed = False


KEY = "probe"


def fill(key, _seen={}):
    if key not in _seen:
        _seen[key] = 1


def drain(key, _seen={}):
    _seen.clear()


@lru_cache(maxsize=None)
def node(name) -> Node:
    return Node(name)


@lru_cache(maxsize=None)
def table(name) -> dict:
    return {}


def kept(name, raw):
    n = node(name)
    n.by_hasattr()
    n.by_getattr()
    n.by_try()
    n.__hash__()
    n.by_setattr()
    n.by_get("k")
    n.by_in(1, 2)
    n.by_identity(raw)
    n.by_attribute(raw)
    fill("k", table(name))


def changed(name):
    n = node(name)
    n.other_error()
    n.bare_except()
    n.other_key()
    n.closes()
    drain("k", table(name))
"#,
    );
    assert_eq!(
        reports(&p, "MUT114"),
        vec![
            at("node.py", 127, "changed"),
            at("node.py", 128, "changed"),
            at("node.py", 129, "changed"),
            at("node.py", 130, "changed"),
            at("node.py", 131, "changed"),
        ]
    );
}

#[test]
fn a_slice_of_a_shared_list_is_a_new_list_holding_the_shared_elements() {
    let p = project("shared_slices", DEEP_SHARED);
    p.write(
        "app.py",
        r#"import functools

CACHE = [[1], [2]]


def shared() -> list[list[int]]:
    return CACHE


@functools.cache
def rows() -> list[int]:
    return [1]


def window() -> None:
    w = shared()[:]
    w.clear()
    rows()[1:].append(2)


def element_of_slice() -> None:
    shared()[:][0].append(3)


def window_of_unknown(xs) -> None:
    w = xs()[:]
    w.clear()


def sliced() -> list[list[int]]:
    return shared()[:]


def through_wrapper() -> None:
    sliced().clear()
    sliced()[0].append(4)


@functools.cache
def untyped_rows():
    return [1]


def defensive_copy() -> None:
    untyped_rows()[:].append(5)
"#,
    );
    // A wrapper returning a slice hands out a new list holding the shared elements; a slice of a
    // value of unknown class is a copy too.
    assert_eq!(
        reports(&p, "MUT202"),
        vec![
            at("app.py", 22, "element_of_slice"),
            at("app.py", 36, "through_wrapper")
        ]
    );
}

#[test]
fn a_generator_changes_what_it_is_handed_only_where_it_is_iterated() {
    let p = project(
        "generator_changes",
        "[[tool.heki.readonly-returns]]\nname = \"shared\"\nproducers = [\"app.shared\"]\n",
    );
    p.write(
        "app.py",
        r#"from collections.abc import Sequence
from dataclasses import dataclass


@dataclass(frozen=True)
class Record:
    items: list[int]


CACHE = [1]


def shared() -> list[int]:
    return CACHE


def wipe(xs):
    xs.clear()
    yield 1


def made_only(xs: Sequence[int], rec: Record) -> None:
    wipe(xs)
    wipe(rec.items)
    wipe(shared())


def consumed(xs: Sequence[int], rec: Record) -> None:
    list(wipe(xs))
    for _ in wipe(rec.items):
        pass
    sum(wipe(shared()))
"#,
    );
    assert_eq!(reports(&p, "MUT110"), vec![at("app.py", 29, "consumed")]);
    assert_eq!(reports(&p, "MUT108"), vec![at("app.py", 30, "consumed")]);
    assert_eq!(reports(&p, "MUT202"), vec![at("app.py", 32, "consumed")]);
}

/// A call runs what the sites of the object it returns run: where that object is iterated,
/// handed to a function iterating it, or awaited. Another call of the same function with the
/// same argument makes another object, whose iteration is not the first call's.
#[test]
fn a_call_runs_what_iterating_its_own_object_runs_not_another_calls() {
    let p = project(
        "generator_calls_apart",
        "[[tool.heki.readonly-returns]]\nname = \"shared\"\nproducers = [\"app.load\"]\n",
    );
    p.write(
        "app.py",
        r#"def load():
    return [1, 2, 3]


def gen(xs):
    xs.append(1)
    yield 1


def consume(items):
    for _ in items:
        pass


def sibling():
    v = load()
    kept = gen(v)
    for _ in gen(v):
        pass
    return kept is None


def later():
    v = load()
    rows = gen(v)
    for _ in rows:
        pass


def handed():
    v = load()
    consume(gen(v))


async def grow(xs):
    xs.append(1)


async def awaited():
    v = load()
    kept = grow(v)
    pending = grow(v)
    await pending
    return kept is None
"#,
    );
    assert_eq!(
        reports(&p, "MUT202"),
        vec![
            at("app.py", 18, "sibling"),
            at("app.py", 25, "later"),
            at("app.py", 32, "handed"),
            at("app.py", 42, "awaited"),
        ]
    );
}

#[test]
fn any_and_all_stop_early_and_leave_the_rest_of_an_iterator() {
    let p = project("short_circuit_consumers", "");
    p.write(
        "app.py",
        r#"import time


def after_any() -> list[int]:
    it = iter([1, 2])
    if any(it):
        return list(it)
    return []


def after_all() -> list[int]:
    it = iter([1, 2])
    all(it)
    return list(it)


def after_sum() -> list[int]:
    it = iter([1, 2])
    sum(it)
    return list(it)


def any_after_list() -> bool:
    it = iter([1, 2])
    list(it)
    return any(it)


def slow():
    time.sleep(1)
    yield 1


async def handler() -> bool:
    return any(slow())
"#,
    );
    let (code, stdout, stderr) = p.check(&[
        "--select",
        "MUT106,EFF101,EFF111",
        "--output-format",
        "concise",
    ]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    // `any` still runs the generator's body on the event loop.
    assert_eq!(lines.len(), 3, "{stdout}");
    assert!(lines[0].starts_with("app.py:20:12: MUT106"), "{stdout}");
    assert!(lines[1].starts_with("app.py:26:12: MUT106"), "{stdout}");
    assert!(lines[2].starts_with("app.py:35:12: EFF111"), "{stdout}");
}

#[test]
fn a_helper_rebinding_a_shallow_copys_attribute_leaves_the_shared_value_alone() {
    let p = project(
        "shallow_copy_helpers",
        "[[tool.heki.readonly-returns]]\nname = \"box\"\nproducers = [\"app.box\"]\ndeep = true\n",
    );
    p.write(
        "app.py",
        r#"import copy


class Inner:
    def __init__(self) -> None:
        self.n = 0


class Box:
    def __init__(self) -> None:
        self.name = "old"
        self.tags: list[str] = []
        self.inner = Inner()


BOX = Box()


def box() -> Box:
    return BOX


def rename(b):
    b.name = "new"


def tag(b):
    b.tags.append("x")


def bump(b):
    b.inner.n += 1


def clear_all(xs):
    xs.clear()


def tag_through(b):
    clear_all(b.tags)


def use() -> None:
    rename(copy.copy(box()))
    tag(copy.copy(box()))
    bump(copy.copy(box()))
    tag_through(copy.copy(box()))
"#,
    );
    assert_eq!(
        reports(&p, "MUT202"),
        vec![
            at("app.py", 45, "use"),
            at("app.py", 46, "use"),
            at("app.py", 47, "use")
        ]
    );
}

#[test]
fn a_change_below_an_item_or_element_of_a_parameter_changes_the_parameter() {
    let p = project(
        "param_elements",
        r#"[[tool.heki.readonly-returns]]
name = "schemas"
producers = ["registry.schema", "registry.flat"]
deep = true

[[tool.heki.readonly-returns]]
name = "flat"
producers = ["registry.flat_only"]
"#,
    );
    p.write(
        "registry.py",
        r#"def schema(name):
    return {}


def flat_only(name):
    return {}
"#,
    )
    .write(
        "helpers.py",
        r#"import copy
from collections import OrderedDict


def by_key(orig):
    orig["tags"].append(1)


def by_index(orig):
    orig[0].append(1)


def by_iteration(orig):
    for row in orig:
        row.clear()


def by_items_view(orig):
    for _, rows in orig.items():
        rows.append(1)


def by_get(orig):
    orig.get("tags").append(1)


def below_an_attribute(orig):
    orig.groups[0].append(1)


def through_dict(orig):
    draft = dict(orig)
    draft["tags"].append(1)


def through_method(orig):
    draft = orig.copy()
    draft["tags"].append(1)


def through_unpacking(orig):
    draft = {**orig}
    draft["tags"].append(1)


def through_list(orig):
    draft = list(orig)
    draft[0].append(1)


def through_slice(orig):
    draft = orig[:]
    draft[0].append(1)


def through_ordered_dict(orig):
    draft = OrderedDict(orig)
    draft["tags"].append(1)


def through_copy_iteration(orig):
    for row in list(orig):
        row.clear()


def through_copy_attribute_item(orig):
    draft = copy.copy(orig)
    draft.groups["x"] = 1


def changes_its_item(x):
    x["tags"].append(1)


def changes_itself(x):
    x.append(1)


def handed_a_copy(orig):
    changes_its_item(dict(orig))


def handed_an_item(orig):
    changes_itself(orig["tags"])


def rebinds_the_item(orig):
    draft = dict(orig)
    draft["tags"] = []
    draft["tags"].append(1)


def inserts_first(orig):
    draft = list(orig)
    draft.insert(0, [])
    draft[0].append(1)


def rebinds_and_changes(x):
    x["tags"] = []
    x["tags"].append(1)


def handed_a_copy_it_rebinds(orig):
    rebinds_and_changes(dict(orig))
"#,
    )
    .write(
        "use.py",
        r#"from helpers import (
    below_an_attribute,
    by_get,
    by_index,
    by_items_view,
    by_iteration,
    by_key,
    handed_a_copy,
    handed_a_copy_it_rebinds,
    handed_an_item,
    inserts_first,
    rebinds_the_item,
    through_copy_attribute_item,
    through_copy_iteration,
    through_dict,
    through_list,
    through_method,
    through_ordered_dict,
    through_slice,
    through_unpacking,
)
from registry import flat_only, schema


def use(name):
    by_key(schema(name))
    by_index(schema(name))
    by_iteration(schema(name))
    by_items_view(schema(name))
    by_get(schema(name))
    below_an_attribute(schema(name))
    through_dict(schema(name))
    through_method(schema(name))
    through_unpacking(schema(name))
    through_list(schema(name))
    through_slice(schema(name))
    through_ordered_dict(schema(name))
    through_copy_iteration(schema(name))
    through_copy_attribute_item(schema(name))
    handed_a_copy(schema(name))
    handed_an_item(schema(name))
    rebinds_the_item(schema(name))
    inserts_first(schema(name))
    handed_a_copy_it_rebinds(schema(name))
    by_key(flat_only(name))
"#,
    );
    // Each helper changes an object the shared value holds, directly or through its own shallow
    // copy; one that first gives the copy a new item changes only that. A producer that is not
    // deep shares only its object, not what it holds.
    let expected: Vec<_> = (26..=41).map(|line| at("use.py", line, "use")).collect();
    assert_eq!(reports(&p, "MUT202"), expected);

    // The helpers' own summaries say so, also for `EFF201`'s `mutates-params`.
    let p = project(
        "param_elements_boundary",
        r#"[[tool.heki.effect-boundaries]]
name = "pure"
scopes = ["helpers.*"]
forbid = ["mutates-params"]
"#,
    );
    p.write(
        "helpers.py",
        r#"import copy
from collections import deque

from inner import changes_its_item, copies_a_part, rebinds_and_changes


def by_key(orig):
    orig["tags"].append(1)


def through_list(orig):
    draft = list(orig)
    draft[0].append(1)


def handed_a_copy(orig):
    changes_its_item(dict(orig))


def handed_a_part_of_a_copy(orig):
    changes_its_item(dict(orig)["rows"])


def rebinds_the_item(orig):
    draft = dict(orig)
    draft["tags"] = []
    draft["tags"].append(1)


def handed_a_copy_it_rebinds(orig):
    rebinds_and_changes(dict(orig))


def inserts_into_a_deque_copy(orig):
    draft = deque(orig)
    draft.appendleft([])
    draft[0].append(1)


def handed_a_copy_copying_a_part(orig):
    copies_a_part(copy.copy(orig))
"#,
    )
    .write(
        "inner.py",
        r#"import copy


def changes_its_item(x):
    x["tags"].append(1)


def copies_a_part(q):
    clears(copy.copy(q.children))


def clears(xs):
    xs.clear()


def rebinds_and_changes(x):
    x["tags"] = []
    x["tags"].append(1)
"#,
    );
    assert_eq!(
        reports(&p, "EFF201"),
        vec![
            at("helpers.py", 7, "by_key"),
            at("helpers.py", 11, "through_list"),
            at("helpers.py", 16, "handed_a_copy"),
            at("helpers.py", 20, "handed_a_part_of_a_copy"),
        ]
    );
    let (_, stdout, _) = p.heki(&["inspect", "helpers.through_list", "."]);
    assert!(stdout.contains("orig[...]"), "{stdout}");
    // A store under the same key into another object leaves the item possibly the function's
    // own: the container the item holds is changed in place, with no change of the parameter's
    // elements recorded, so no line of the source is shown for it.
    p.write(
        "slots.py",
        "def other_key(orig, other):\n    other[\"tags\"] = 1\n    orig[\"tags\"].append(1)\n",
    );
    let (_, stdout, _) = p.heki(&["inspect", "slots.other_key", "."]);
    assert!(
        stdout.contains("orig[\"tags\"]  in place, directly\n"),
        "{stdout}"
    );
}
