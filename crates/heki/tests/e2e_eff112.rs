//! `EFF112` (`set-order-output`) end to end: the iteration order of a set of strings reaching an
//! output across a call (a value returned up to the caller that writes it, through helpers and
//! pass-through functions; a value handed down to a writer; a pick; a generator expression; a
//! module constant; a stream; a raised message; one element at a time printed), the writers'
//! sorting switches, and the shapes left silent: a flow inside one function, a set of other
//! values, a sorted value, an order-free use, a set bounded to one element, one element written
//! per file, `logging` and `warnings`, and a switch heki cannot evaluate; both lowerings of
//! comprehensions (a function of their own before Python 3.12), the opt-in selection, and
//! `heki rule`.

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

/// `(path, line, qualname, target)` of every `EFF112` diagnostic.
fn found(diags: &[Value]) -> Vec<(String, u64, String, String)> {
    diags
        .iter()
        .filter(|d| d["rule_code"] == "EFF112")
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

/// The flows of the dcg repro: a set returned by a helper, iterated into a dict another function
/// writes; a set operator with a module constant, formatted through two returns; a pick from an
/// intersection. The shapes left silent: a dict handed on as `**kwargs`, a comparison, a pick
/// bounded to one element, a loop returning a constant, a sorted iteration.
const FLOW: &str = r#"from __future__ import annotations

import json
from pathlib import Path

CONTAINS_KEYWORDS = frozenset({"contains", "minContains", "maxContains"})
CELLS = frozenset({"metavar", "choices"})


def referenced(schema: dict[str, object]) -> set[str]:
    found: set[str] = set()
    for value in schema.values():
        if isinstance(value, dict) and isinstance(ref := value.get("$ref"), str):
            found.add(ref.rsplit("/", 1)[-1])
    return found


def components(schema: dict[str, object], schemas: dict[str, object]) -> dict[str, object]:
    return {name: schemas[name] for name in referenced(schema)}


def write_components(schema: dict[str, object], schemas: dict[str, object], path: Path) -> None:
    path.write_text(json.dumps(components(schema, schemas)), encoding="utf-8")


def sibling_extras(extras: dict[str, object]) -> dict[str, object]:
    keys = CONTAINS_KEYWORDS & extras.keys()
    return {key: extras[key] for key in keys}


def field_text(extras: dict[str, object]) -> str:
    return f"Field(json_schema_extra={sibling_extras(extras)!r})"


def tag_field(name: str, source_name: str, tag_fields: frozenset[str]) -> str | None:
    return next(iter({name, source_name} & tag_fields), None)


def tag_line(name: str, source_name: str, tag_fields: frozenset[str]) -> str:
    return f"    {tag_field(name, source_name, tag_fields)}: Literal['cat']"


def write_model(path: Path, extras: dict[str, object], tag_fields: frozenset[str]) -> None:
    lines = ["class Cat(BaseModel):", tag_line("type_", "type", tag_fields), f"    names: list[str] = {field_text(extras)}"]
    path.write_text("\n".join(lines), encoding="utf-8")


def make_option(**cells: object) -> dict[str, object]:
    return {"metavar": cells.get("metavar"), "choices": cells.get("choices")}


def option(**arguments: object) -> dict[str, object]:
    cells = {name: arguments.pop(name) for name in CELLS & arguments.keys()}
    return make_option(**cells)


def fingerprints_changed(paths: set[str], old: dict[str, int]) -> bool:
    return {path: len(path) for path in paths} != old


def single_format(formats: set[str]) -> str | None:
    if len(formats) > 1:
        return None
    return formats.pop() if formats else None


def write_format(path: Path, formats: set[str]) -> None:
    path.write_text(single_format(formats) or "", encoding="utf-8")


def has_owner(names: set[str], owners: dict[str, str]) -> bool:
    for name in names:
        if name in owners:
            return True
    return False


def write_sorted_components(schema: dict[str, object], schemas: dict[str, object], path: Path) -> None:
    ordered = {name: schemas[name] for name in sorted(referenced(schema))}
    path.write_text(json.dumps(ordered), encoding="utf-8")


def write_option(path: Path, **arguments: object) -> None:
    path.write_text(json.dumps(option(**arguments)), encoding="utf-8")
"#;

/// The forms a linter decides from one expression: no report.
const MESSAGES: &str = r#"from __future__ import annotations


def resolve_profile(profiles: dict[str, str], name: str) -> str:
    visited: set[str] = set()
    while name in profiles:
        if name in visited:
            chain = " -> ".join(visited)
            msg = f"Circular extends detected: {chain} -> {name}"
            raise ValueError(msg)
        visited.add(name)
        name = profiles[name]
    return name


def check_keys(given: dict[str, object], valid: frozenset[str]) -> None:
    invalid = set(given) - valid
    if invalid:
        msg = f"Invalid keys: {invalid}. Valid keys are: {valid}"
        raise ValueError(msg)


def unresolved(refs: dict[str, frozenset[str]]) -> str:
    return ", ".join(f"[class: {path} references: {targets}]" for path, targets in sorted(refs.items()))


def tag_field(name: str, source_name: str, tag_fields: frozenset[str]) -> str | None:
    return next(iter({name, source_name} & tag_fields), None)


def first_owner(names: set[str], owners: dict[str, str]) -> str | None:
    for name in names:
        if name in owners:
            return owners[name]
    return None


def only_kind(kinds: set[str]) -> str | None:
    return kinds.pop() if len(kinds) == 1 else None


def single_format(formats: set[str]) -> str | None:
    if len(formats) > 1:
        return None
    return formats.pop() if formats else None


def sorted_chain(visited: set[str]) -> str:
    return " -> ".join(sorted(visited))


def numbers(values: set[int]) -> str:
    return f"{values}"


def has_owner(names: set[str], owners: dict[str, str]) -> bool:
    for name in names:
        if name in owners:
            return True
    return False
"#;

/// Writers, each with what it writes its argument with.
const WRITERS: &str = r#"import json
import logging
import pprint
import sys
import warnings
from pathlib import Path

import yaml

logger = logging.getLogger(__name__)


def save_json(path: Path, data: object) -> None:
    path.write_text(json.dumps(data))


def save_sorted(path: Path, data: object) -> None:
    path.write_text(json.dumps(data, sort_keys=True))


def save_switched(path: Path, data: object, sort: bool) -> None:
    path.write_text(json.dumps(data, sort_keys=sort))


def save_yaml(path: Path, data: object) -> None:
    path.write_text(yaml.safe_dump(data))


def save_yaml_unsorted(path: Path, data: object) -> None:
    path.write_text(yaml.safe_dump(data, sort_keys=False))


def show(data: object) -> None:
    print(data)


def show_sorted(data: object) -> None:
    pprint.pprint(data)


def show_unsorted(data: object) -> None:
    pprint.pp(data)


def note(data: object) -> None:
    logger.info("data: %s", data)
    warnings.warn(str(data))


def fail(names: list[str]) -> None:
    raise ValueError("unknown: " + ", ".join(names))


def emit(text: str) -> None:
    sys.stdout.write(text)


def fmt(value: object) -> str:
    return f"[{value}]"


class Collector:
    def __init__(self) -> None:
        self.parts: list[str] = []

    def write(self, text: str) -> None:
        self.parts.append(text)


def write_into(manager, text: str) -> None:
    with manager as out:
        out.write(text)
"#;

const SHAPES: &str = r#"from pathlib import Path

from pkg import writers

KINDS = frozenset({"a", "b", "c"})


def names(table: dict[str, int]) -> set[str]:
    return set(table)


def numbers(table: dict[int, str]) -> set[int]:
    return set(table)


def listed(table: dict[str, int]) -> list[str]:
    return list(names(table))


def first(found: set[str]) -> str | None:
    for name in found:
        return name
    return None


def generated(table: dict[str, int]):
    return (name for name in names(table))


def kept_set(table: dict[str, int]) -> set[str]:
    return {name.upper() for name in names(table)}


def ordered(table: dict[str, int]) -> list[str]:
    return sorted(names(table))


def counted(table: dict[str, int]) -> int:
    return len(names(table))


def single(found: set[str]) -> str | None:
    return found.pop() if len(found) == 1 else None


def handed(table: dict[str, int], path: Path) -> None:
    writers.save_json(path, [name for name in names(table)])


def handed_sorted(table: dict[str, int], path: Path) -> None:
    writers.save_sorted(path, {name: 1 for name in names(table)})


def handed_switched(table: dict[str, int], path: Path, sort: bool) -> None:
    writers.save_switched(path, {name: 1 for name in names(table)}, sort)


def handed_yaml(table: dict[str, int], path: Path) -> None:
    writers.save_yaml(path, {name: 1 for name in names(table)})


def handed_yaml_unsorted(table: dict[str, int], path: Path) -> None:
    writers.save_yaml_unsorted(path, {name: 1 for name in names(table)})


def printed(table: dict[str, int]) -> None:
    writers.show(listed(table))


def pretty(table: dict[str, int]) -> None:
    writers.show_sorted({name: 1 for name in names(table)})


def pretty_unsorted(table: dict[str, int]) -> None:
    writers.show_unsorted({name: 1 for name in names(table)})


def logged(table: dict[str, int]) -> None:
    writers.note(listed(table))


def raised(table: dict[str, int]) -> None:
    writers.fail(listed(table))


def emitted(table: dict[str, int]) -> None:
    writers.emit(", ".join(names(table)))


def picked(table: dict[str, int], path: Path) -> None:
    path.write_text(first(names(table)) or "")


def joined(table: dict[str, int], path: Path) -> None:
    path.write_text("".join(generated(table)))


def formatted(table: dict[str, int], path: Path) -> None:
    path.write_text(writers.fmt(listed(table)))


def kinds(path: Path) -> None:
    writers.save_json(path, list(KINDS))


def ints(table: dict[int, str], path: Path) -> None:
    writers.save_json(path, [n for n in numbers(table)])


def from_set(table: dict[str, int], path: Path) -> None:
    writers.save_json(path, sorted(kept_set(table)))


def sorted_out(table: dict[str, int], path: Path) -> None:
    writers.save_json(path, ordered(table))


def count_out(table: dict[str, int], path: Path) -> None:
    writers.save_json(path, counted(table))


def single_out(table: dict[str, int], path: Path) -> None:
    path.write_text(single(names(table)) or "")


def local_only(table: dict[str, int], path: Path) -> None:
    path.write_text(", ".join(names(table)))


def per_file(table: dict[str, int], root: Path) -> None:
    for name in names(table):
        (root / name).write_text(writers.fmt(name))


def per_line(table: dict[str, int]) -> None:
    for name in names(table):
        writers.show(name)


def per_file_helper(table: dict[str, int], root: Path) -> None:
    for name in names(table):
        writers.save_json(root / name, name)


def streamed(table: dict[str, int], path: Path) -> None:
    with open(path, "w") as out:
        for name in names(table):
            out.write(name)


def stream_helper(out, table: dict[str, int]) -> None:
    out.write(", ".join(listed(table)))
"#;

/// The other ways a set gets its order taken, the stream writers, the module constants, and
/// the tests of a set's length.
const MORE: &str = r##"from pathlib import Path

from pkg import writers
from pkg.shapes import KINDS, listed, names

MODES = {"r", "w"}
OTHER = frozenset(sorted(["a", "b"]))
LEVEL = 1
LEVEL = 2


def described(table: dict[str, int]) -> str:
    return f"{names(table)}"


def write_described(table: dict[str, int], path: Path) -> None:
    path.write_text(described(table))


def popped(table: dict[str, int]) -> str:
    return names(table).pop()


def write_popped(table: dict[str, int], path: Path) -> None:
    path.write_text(popped(table))


def tagged(items: list[str]) -> list[str]:
    found = {f"#{item}" for item in items}
    return [tag for tag in found]


def write_tagged(items: list[str], path: Path) -> None:
    writers.save_json(path, tagged(items))


def keyed(table: dict[str, int], ids: set[int], path: Path) -> None:
    writers.save_json(path, [key for key in table.keys() & ids])


def either(table: dict[str, int], flag: bool, path: Path) -> None:
    found = names(table) if flag else set()
    writers.save_json(path, list(found))


def paired(pairs: set[tuple[str, str]], path: Path) -> None:
    writers.save_json(path, [p for p in pairs])


def tabled(table: dict[str, int], path: Path) -> None:
    writers.save_json(path, {"names": list(names(table))})


def save_lines(path: Path, lines: list[str]) -> None:
    with open(path, "w") as out:
        out.write("\n".join(lines))


def save_raw(path: Path, text: str) -> None:
    out = open(path, "w")
    out.write(text)


def lines_out(table: dict[str, int], path: Path) -> None:
    save_lines(path, listed(table))


def raw_out(table: dict[str, int], path: Path) -> None:
    save_raw(path, ", ".join(listed(table)))


def chained(table: dict[str, int], path: Path) -> None:
    writers.save_json(path, [n for n in (names(table) & KINDS) | KINDS])


def literal_set(path: Path) -> None:
    writers.save_json(path, list(frozenset(["x", "y"])))


def modes(path: Path) -> None:
    writers.save_json(path, list(MODES))


def other(path: Path) -> None:
    writers.save_json(path, list(OTHER))


def below_two(found: set[str]) -> str | None:
    if len(found) < 2:
        return found.pop() if found else None
    return None


def two_at_most(found: set[str]) -> str | None:
    if 2 <= len(found):
        return None
    return found.pop() if found else None


def at_least_two(found: set[str]) -> str | None:
    if len(found) >= 2:
        return None
    return found.pop() if found else None


def same_sets(found: set[str], other: set[str]) -> str | None:
    if found == other:
        return found.pop() if found else None
    return None


def bounded_out(found: set[str], path: Path) -> None:
    path.write_text((below_two(found) or "") + (two_at_most(found) or "") + (at_least_two(found) or ""))


def unbounded_out(found: set[str], other: set[str], path: Path) -> None:
    path.write_text(same_sets(found, other) or "")


def maybe_names(flag: bool) -> list[str]:
    found = {"a", "b"} if flag else {"c", "d"}
    return [n for n in found]


def maybe_out(flag: bool, path: Path) -> None:
    writers.save_json(path, maybe_names(flag))


def lengths(items: list[str], path: Path) -> None:
    writers.save_json(path, [n for n in {len(item) for item in items}])


def popped_after(found: set[str]) -> str:
    if len(found) > 1:
        print("many")
    return found.pop()


def at_most(found: set[str]) -> str | None:
    if len(found) <= 1:
        return found.pop() if found else None
    return None


def exactly_one(found: set[str]) -> str | None:
    if len(found) != 1:
        return None
    return found.pop()


def above_five(found: set[str]) -> str | None:
    if len(found) > 5:
        return None
    return found.pop() if found else None


def under_two(found: set[str]) -> str | None:
    if 2 > len(found):
        return found.pop() if found else None
    return None


def over_one(found: set[str]) -> str | None:
    if 1 < len(found):
        return None
    return found.pop() if found else None


def picks_out(found: set[str], path: Path) -> None:
    path.write_text(popped_after(found) + (at_most(found) or "") + (exactly_one(found) or ""))
    path.write_text((under_two(found) or "") + (over_one(found) or ""))


def above_five_out(found: set[str], path: Path) -> None:
    path.write_text(above_five(found) or "")


def collected(table: dict[str, int]) -> list[str]:
    buf = writers.Collector()
    buf.write(", ".join(listed(table)))
    return buf.parts


def variadic(pairs: set[tuple[str, ...]], path: Path) -> None:
    writers.save_json(path, [p for p in pairs])


def limited(found: set[str], count: int) -> str | None:
    if len(found) > count:
        return None
    return found.pop() if found else None


def equal_one(found: set[str]) -> str | None:
    if 1 == len(found):
        return found.pop()
    return None


def at_most_two(found: set[str]) -> str | None:
    if 2 >= len(found):
        return found.pop() if found else None
    return None


def bounds_out(found: set[str], count: int, path: Path) -> None:
    path.write_text((limited(found, count) or "") + (equal_one(found) or ""))
    path.write_text(at_most_two(found) or "")


def bare_tuples(pairs: set[tuple], path: Path) -> None:
    writers.save_json(path, [p for p in pairs])


def tags_either(items: list[str], flag: bool, path: Path) -> None:
    found = {f"#{item}" for item in items} if flag else set()
    writers.save_json(path, [t for t in found])


def tags_kept(items: list[str], path: Path) -> set[str]:
    found = {f"#{item}" for item in items}
    writers.save_json(path, [t for t in found])
    return found


NUMBERS = {1, 2}


def numbers_out(path: Path) -> None:
    writers.save_json(path, list(NUMBERS))
"##;

/// Shapes that keep or lose the order on the way: a raise for one element at a time (also of
/// text made from it), a mapping read back by key, a key read before and stored again, a set
/// filled with the order, a method of an unknown receiver, a sequence of at most one element
/// (also by a test inside `or` / `and`), and `|` of two mappings; and what stays reported, text
/// built element by element in a loop and a set operator on two mappings' key views among it.
const NARROWED: &str = r#"import json
import sys

FLAGS = {"draft", "hidden", "pinned"}
NAMES = {"lat", "latitude", "LAT"}


def require(name):
    if not name.isidentifier():
        raise ValueError(f"not an identifier: {name}")
    return name


def check_all(names: set[str]):
    for name in names:
        require(name)


def message_of_all(names: set[str]):
    raise ValueError("unknown: " + ", ".join(names))


def normalized(fields: dict) -> dict:
    for flag in FLAGS:
        if isinstance(fields.get(flag), str):
            fields[flag] = fields[flag].lower() == "true"
    return fields


def emit(fields: dict) -> None:
    print(json.dumps(normalized(fields)))


def counts(paths: set[str]) -> dict[str, int]:
    found = {}
    for path in paths:
        found[path] = len(path)
    return found


def show_counts(paths: set[str]) -> None:
    summary = counts(paths)
    for path in sorted(summary):
        print(f"{path}: {summary[path]}")


def show_all_counts(paths: set[str]) -> None:
    print(json.dumps(counts(paths)))


def scan(paths: set[str]):
    seen = set()
    seen.update(p.upper() for p in paths)
    return len(paths), seen


def report_scan(paths: set[str]) -> None:
    n, seen = scan(paths)
    print(f"scanned={n} seen={len(seen)}")


def widen(data, names: set[str]):
    for name in names:
        data.insert(0, name)
    return data


def show_widened(data, names: set[str]) -> None:
    sys.stdout.write(str(widen(data, names)))


def gather(names: set[str]):
    out = []
    for name in names:
        out.insert(0, name)
    return out


def show_gathered(names: set[str]) -> None:
    print(gather(names))


def only(names: set[str]) -> str:
    rest = list(names - {"x"})
    if len(rest) > 1:
        return "several"
    return rest[0]


def show_only(names: set[str]) -> None:
    print(only(names))


def first(names: set[str]) -> str:
    rest = list(names)
    return rest[0]


def show_first(names: set[str]) -> None:
    print(first(names))


def column(columns: list[str]) -> str:
    for c in NAMES:
        if c in columns:
            return c
    return ""


def show_column(columns: list[str]) -> None:
    print(json.dumps({"lat": column(columns)}))


def by_comprehension(paths: set[str]) -> None:
    summary = {p: len(p) for p in paths}
    for path in sorted(summary):
        print(f"{path}: {summary[path]}")


def by_dict_call(paths: set[str]) -> dict:
    found = dict()
    for path in paths:
        found[path] = 0
    return found


def show_by_dict_call(paths: set[str]) -> None:
    summary = by_dict_call(paths)
    for path in sorted(summary):
        print(summary[path])


def by_key(table, key: str) -> str:
    return table[key]


def ordered_keys(paths: set[str]) -> list[str]:
    return [p for p in paths]


def show_by_key(paths: set[str]) -> None:
    print(by_key(ordered_keys(paths), "a"))


def into_listed(names: set[str], seed: list[str]):
    out = [s for s in seed]
    for name in names:
        out.insert(0, name)
    return out


def show_into_listed(names: set[str]) -> None:
    print(into_listed(names, []))


def into_typed(names: set[str], out: list[str]):
    for name in names:
        out.insert(0, name)
    return out


def show_into_typed(names: set[str]) -> None:
    print(into_typed(names, []))


def into_mapping(names: set[str]):
    out = dict()
    for name in names:
        out.setdefault(name, 0)
    return out


def show_into_mapping(names: set[str]) -> None:
    print(into_mapping(names))


def rewritten_later(fields: dict, names: set[str]) -> dict:
    for name in names:
        value = fields[name]
        if value:
            fields[name] = str(value)
    return fields


def show_rewritten_later(fields: dict, names: set[str]) -> None:
    print(json.dumps(rewritten_later(fields, names)))


def stored_after_other_read(fields: dict, other: dict, names: set[str]) -> dict:
    for name in names:
        fields[name] = other[name]
    return fields


def show_stored_after_other_read(fields: dict, other: dict, names: set[str]) -> None:
    print(json.dumps(stored_after_other_read(fields, other, names)))


def read_on_one_side(fields: dict, names: set[str], flag: bool) -> dict:
    for name in names:
        if flag:
            print(fields[name])
        fields[name] = 1
    return fields


def show_read_on_one_side(fields: dict, names: set[str], flag: bool) -> None:
    print(json.dumps(read_on_one_side(fields, names, flag)))


def check_formatted(names: set[str]):
    for name in names:
        require(f"pkg.{name}")


def joined_by_hand(names: set[str]) -> str:
    text = ""
    for name in names:
        text += name + ","
    return text


def show_joined_by_hand(names: set[str]) -> None:
    print(joined_by_hand(names))


def formatted_by_hand(names: set[str]) -> str:
    text = ""
    for name in names:
        text = f"{text}{name};"
    return text


def show_formatted_by_hand(names: set[str]) -> None:
    print(formatted_by_hand(names))


def show_line(line: str) -> None:
    print(line)


def show_each(names: set[str]) -> None:
    for name in names:
        show_line(f"- {name}")


def merged(a: dict[str, int], b: dict[str, int]) -> list[str]:
    return list(a | b)


def show_merged(a: dict[str, int], b: dict[str, int]) -> None:
    print(merged(a, b))


def added(a: dict[str, int], b: dict[str, int]) -> list[str]:
    return list(a.keys() - b.keys())


def show_added(a: dict[str, int], b: dict[str, int]) -> None:
    print(added(a, b))


def single_or(paths: set[str], flag: bool) -> str:
    if len(paths) != 1 or flag:
        return ""
    return next(iter(paths))


def single_and(paths: set[str], flag: bool) -> str:
    if flag and len(paths) == 1:
        return next(iter(paths))
    return ""


def either_side(paths: set[str], flag: bool) -> str:
    if flag or len(paths) == 1:
        return next(iter(paths))
    return ""


def show_single(paths: set[str], flag: bool) -> None:
    print(json.dumps([single_or(paths, flag), single_and(paths, flag)]))
    print(json.dumps(either_side(paths, flag)))


def single_of_many(paths: set[str], count: int, flag: bool) -> str:
    if count > 2 or not flag or len(paths) != 1:
        return ""
    return next(iter(paths))


def show_single_of_many(paths: set[str], count: int, flag: bool) -> None:
    print(single_of_many(paths, count, flag))
"#;

fn project(name: &str) -> Project {
    let p = Project::new(name);
    p.write("pkg/__init__.py", "")
        .write("pkg/flow.py", FLOW)
        .write("pkg/messages.py", MESSAGES)
        .write("pkg/writers.py", WRITERS)
        .write("pkg/shapes.py", SHAPES)
        .write("pkg/more.py", MORE)
        .write("pkg/narrowed.py", NARROWED);
    p
}

fn expected() -> Vec<(String, u64, String, String)> {
    vec![
        d(
            "pkg/flow.py",
            23,
            "write_components",
            "components:19->json.dumps",
        ),
        d(
            "pkg/flow.py",
            44,
            "write_model",
            "tag_field:36->pathlib.Path.write_text",
        ),
        d(
            "pkg/flow.py",
            44,
            "write_model",
            "sibling_extras:28->pathlib.Path.write_text",
        ),
        d(
            "pkg/more.py",
            17,
            "write_described",
            "described:13->pathlib.Path.write_text",
        ),
        d(
            "pkg/more.py",
            25,
            "write_popped",
            "popped:21->pathlib.Path.write_text",
        ),
        d("pkg/more.py", 34, "write_tagged", "tagged:30->json.dumps"),
        d("pkg/more.py", 38, "keyed", "keyed:38->json.dumps"),
        d("pkg/more.py", 43, "either", "either:43->json.dumps"),
        d("pkg/more.py", 47, "paired", "paired:47->json.dumps"),
        d("pkg/more.py", 51, "tabled", "tabled:51->json.dumps"),
        d("pkg/more.py", 65, "lines_out", "listed:17->open().write"),
        d("pkg/more.py", 69, "raw_out", "listed:17->open().write"),
        d("pkg/more.py", 73, "chained", "chained:73->json.dumps"),
        d(
            "pkg/more.py",
            77,
            "literal_set",
            "literal_set:77->json.dumps",
        ),
        d("pkg/more.py", 81, "modes", "modes:81->json.dumps"),
        d(
            "pkg/more.py",
            117,
            "unbounded_out",
            "same_sets:108->pathlib.Path.write_text",
        ),
        d(
            "pkg/more.py",
            126,
            "maybe_out",
            "maybe_names:122->json.dumps",
        ),
        d(
            "pkg/more.py",
            170,
            "picks_out",
            "popped_after:136->pathlib.Path.write_text",
        ),
        d(
            "pkg/more.py",
            175,
            "above_five_out",
            "above_five:154->pathlib.Path.write_text",
        ),
        d("pkg/more.py", 185, "variadic", "variadic:185->json.dumps"),
        d(
            "pkg/more.py",
            207,
            "bounds_out",
            "limited:191->pathlib.Path.write_text",
        ),
        d(
            "pkg/more.py",
            208,
            "bounds_out",
            "at_most_two:202->pathlib.Path.write_text",
        ),
        d(
            "pkg/more.py",
            217,
            "tags_either",
            "tags_either:217->json.dumps",
        ),
        d("pkg/more.py", 222, "tags_kept", "tags_kept:222->json.dumps"),
        d(
            "pkg/narrowed.py",
            48,
            "show_all_counts",
            "counts:36->json.dumps",
        ),
        d(
            "pkg/narrowed.py",
            80,
            "show_gathered",
            "gather:74->builtins.print",
        ),
        d(
            "pkg/narrowed.py",
            100,
            "show_first",
            "first:95->builtins.print",
        ),
        d(
            "pkg/narrowed.py",
            111,
            "show_column",
            "column:104->json.dumps",
        ),
        d(
            "pkg/narrowed.py",
            153,
            "show_into_listed",
            "into_listed:147->builtins.print",
        ),
        d(
            "pkg/narrowed.py",
            163,
            "show_into_typed",
            "into_typed:157->builtins.print",
        ),
        d(
            "pkg/narrowed.py",
            174,
            "show_into_mapping",
            "into_mapping:168->builtins.print",
        ),
        d(
            "pkg/narrowed.py",
            196,
            "show_stored_after_other_read",
            "stored_after_other_read:190->json.dumps",
        ),
        d(
            "pkg/narrowed.py",
            208,
            "show_read_on_one_side",
            "read_on_one_side:200->json.dumps",
        ),
        d(
            "pkg/narrowed.py",
            224,
            "show_joined_by_hand",
            "joined_by_hand:218->builtins.print",
        ),
        d(
            "pkg/narrowed.py",
            235,
            "show_formatted_by_hand",
            "formatted_by_hand:229->builtins.print",
        ),
        d(
            "pkg/narrowed.py",
            243,
            "show_each",
            "show_each:243->builtins.print",
        ),
        d(
            "pkg/narrowed.py",
            260,
            "show_added",
            "added:256->builtins.print",
        ),
        d(
            "pkg/narrowed.py",
            283,
            "show_single",
            "either_side:277->json.dumps",
        ),
        d("pkg/shapes.py", 47, "handed", "handed:47->json.dumps"),
        d(
            "pkg/shapes.py",
            63,
            "handed_yaml_unsorted",
            "handed_yaml_unsorted:63->yaml.safe_dump",
        ),
        d("pkg/shapes.py", 67, "printed", "listed:17->builtins.print"),
        d(
            "pkg/shapes.py",
            75,
            "pretty_unsorted",
            "pretty_unsorted:75->pprint.pp",
        ),
        d(
            "pkg/shapes.py",
            83,
            "raised",
            "listed:17->builtins.ValueError",
        ),
        d(
            "pkg/shapes.py",
            87,
            "emitted",
            "emitted:87->sys.stdout.write",
        ),
        d(
            "pkg/shapes.py",
            91,
            "picked",
            "first:21->pathlib.Path.write_text",
        ),
        d(
            "pkg/shapes.py",
            95,
            "joined",
            "generated:27->pathlib.Path.write_text",
        ),
        d(
            "pkg/shapes.py",
            99,
            "formatted",
            "listed:17->pathlib.Path.write_text",
        ),
        d("pkg/shapes.py", 103, "kinds", "kinds:103->json.dumps"),
        d(
            "pkg/shapes.py",
            136,
            "per_line",
            "per_line:136->builtins.print",
        ),
    ]
}

#[test]
fn set_orders_reaching_an_output_across_a_call_are_reported() {
    let p = project("eff112_reports");
    for version in ["py311", "py312"] {
        let diags = diagnostics(
            &p,
            &["--extend-select", "EFF112", "--target-version", version],
        );
        assert_eq!(found(&diags), expected(), "{version}");
    }
}

#[test]
fn a_report_names_where_the_order_is_taken_and_the_calls_to_the_output() {
    let p = project("eff112_messages");
    let diags = diagnostics(&p, &["--extend-select", "EFF112"]);
    let at = |path: &str, line: u64, target: &str| {
        diags
            .iter()
            .find(|d| d["path"] == path && d["line"] == line && d["target"] == target)
            .unwrap_or_else(|| panic!("no EFF112 at {path}:{line}: {diags:#?}"))
    };
    let components = at("pkg/flow.py", 23, "components:19->json.dumps");
    assert_eq!(
        components["message"],
        "the order of a set of strings, taken by iterating a set at line 19 in `components`, reaches `json.dumps` (line 23): it changes between runs, as string hashes are randomized per process"
    );
    assert_eq!(components["severity"], "error");
    assert_eq!(components["certainty"], "heuristic");
    let related: Vec<(u64, &str)> = components["related"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["line"].as_u64().unwrap(), r["message"].as_str().unwrap()))
        .collect();
    assert_eq!(
        related,
        vec![
            (19, "`components` takes the order of a set here"),
            (
                23,
                "`write_components` calls `pkg.flow.components`, which returns it"
            ),
            (23, "`write_components` writes it with `json.dumps`"),
        ]
    );
    assert!(components["help"].as_str().unwrap().contains("sorted(...)"));
    // Handed down to a writer: through the call, with the writer's own call.
    let handed = at("pkg/shapes.py", 47, "handed:47->json.dumps");
    assert!(
        handed["message"]
            .as_str()
            .unwrap()
            .contains("reaches `json.dumps` (line 14) through `pkg.writers.save_json`")
    );
    assert_eq!(
        handed["related"][1]["message"],
        "`handed` calls `pkg.writers.save_json`"
    );
    // A raised exception's message.
    assert!(
        at("pkg/shapes.py", 83, "listed:17->builtins.ValueError")["message"]
            .as_str()
            .unwrap()
            .contains("reaches the message of a raised `ValueError` (line 51)")
    );
    // A pick.
    assert!(
        at("pkg/shapes.py", 91, "first:21->pathlib.Path.write_text")["message"]
            .as_str()
            .unwrap()
            .contains("taken by iterating a set at line 21 in `first`")
    );
}

/// A writer the configuration adds: a project's own sorting switch of a library call, and a
/// library writer the catalog does not list.
#[test]
fn the_configuration_adds_writers() {
    let p = project("eff112_configured");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.ecosystem]]\nsymbol = \"yaml.safe_dump\"\nwrites-in-order = { arg = 0, param = \"data\" }\n",
    );
    let diags = diagnostics(&p, &["--extend-select", "EFF112"]);
    // `yaml.safe_dump` no longer sorts: `handed_yaml` is reported too.
    assert!(
        found(&diags)
            .iter()
            .any(|(path, line, _, _)| path == "pkg/shapes.py" && *line == 59)
    );
}

#[test]
fn the_rule_is_opt_in() {
    let p = project("eff112_opt_in");
    assert!(found(&diagnostics(&p, &[])).is_empty());
    assert!(found(&diagnostics(&p, &["--select", "EFF"])).is_empty());
    assert_eq!(
        found(&diagnostics(&p, &["--select", "set-order-output"])),
        expected()
    );
}

#[test]
fn heki_rule_explains_it() {
    let p = Project::new("eff112_rule");
    let (code, stdout, _) = p.heki(&["rule", "EFF112"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("set-order-output"));
    assert!(stdout.contains("Writes in order"));
    assert!(stdout.contains("experimental"));
}
