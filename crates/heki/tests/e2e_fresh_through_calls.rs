//! Objects under construction across calls (`fresh-objects = "allow"`), end to end through the
//! real binary: a helper that returns a new object, or an element of an optional argument the
//! call leaves at a default without elements; and a helper changing its parameter, at calls that
//! hand it a new object.

mod common;

use common::Project;
use serde_json::Value;

const MODEL: &str = r#"from __future__ import annotations

import copy


class Node:
    def __init__(self) -> None:
        self.flag = False
        self.items: list[int] = []


CACHE: dict[int, Node] = {}
KEPT: list[Node] = []


def copy_node(node: Node) -> Node:
    return copy.copy(node)


def deep_node(node: Node) -> Node:
    return copy.deepcopy(node)


def copy_tree(node: Node, replacements: dict[int, Node] | None = None) -> Node:
    if replacements and (replacement := replacements.get(id(node))) is not None:
        return replacement
    return copy_node(node)


def first_or_copy(node: Node, pool: list[Node] | None = None, *, flag: bool = False) -> Node:
    if pool:
        for candidate in pool:
            return candidate
    return copy_node(node)


def copy_or_cached(node: Node) -> Node:
    if (hit := CACHE.get(id(node))) is not None:
        return hit
    return copy_node(node)


def copy_with_memo(node: Node, memo: dict[int, Node] = {}) -> Node:
    if (hit := memo.get(id(node))) is not None:
        return hit
    return copy_node(node)


def copy_or_named(node: Node, name: str = "first") -> Node:
    if name:
        for candidate in name:
            return candidate
    return copy_node(node)


def set_flag(node: Node) -> None:
    node.flag = True


def keep_then_set(node: Node) -> None:
    KEPT.append(node)
    node.flag = True


def add_item(node: Node) -> None:
    node.items.append(1)


def relay_flag(node: Node) -> None:
    set_flag(node)


def set_any(obj) -> None:
    obj.flag = True


def flag_each(node: Node):
    node.flag = True
    yield node


def remember(node: Node) -> None:
    KEPT.append(node)


def count(pool: list[Node] | None) -> int:
    return len(pool or ())


def remember_then_set(node: Node) -> None:
    remember(node)
    node.flag = True


def first_of(pool: list[Node] | None = None) -> Node:
    count(pool)
    for candidate in pool:
        return candidate
    raise LookupError
"#;

/// The scope functions, with the line of each report the boundary is expected to make.
const PASSES: &str = r#"from __future__ import annotations

from pkg.model import (
    KEPT,
    Node,
    add_item,
    copy_node,
    copy_or_cached,
    copy_or_named,
    copy_tree,
    copy_with_memo,
    deep_node,
    first_of,
    first_or_copy,
    flag_each,
    keep_then_set,
    relay_flag,
    remember_then_set,
    set_any,
    set_flag,
)


def copies_without_replacements(node: Node) -> Node:
    copied = copy_tree(node)
    copied.flag = True
    return copied


def copies_with_none(node: Node) -> Node:
    copied = copy_tree(node, None)
    copied.flag = True
    other = copy_tree(node, replacements=None)
    other.flag = True
    return copied


def copies_without_a_pool(node: Node) -> Node:
    copied = first_or_copy(node, flag=True)
    copied.flag = True
    return copied


def hands_the_copy_to_a_helper(node: Node) -> Node:
    copied = copy_node(node)
    set_flag(copied)
    return copied


def hands_the_copy_through_a_relay(node: Node) -> Node:
    copied = copy_node(node)
    relay_flag(copied)
    return copied


def fills_a_deep_copy_in_place(node: Node) -> Node:
    copied = deep_node(node)
    add_item(copied)
    return copied


def hands_a_deep_copy_to_an_untyped_helper(node: Node) -> Node:
    copied = deep_node(node)
    set_any(copied)
    return copied


def copies_with_replacements(node: Node, table: dict[int, Node]) -> Node:
    copied = copy_tree(node, table)
    copied.flag = True
    return copied


def copies_with_a_cache(node: Node) -> Node:
    copied = copy_or_cached(node)
    copied.flag = True
    return copied


def copies_with_a_memo_default(node: Node) -> Node:
    copied = copy_with_memo(node)
    copied.flag = True
    return copied


def copies_with_a_named_default(node: Node) -> Node:
    copied = copy_or_named(node)
    copied.flag = True
    return copied


def copies_with_unpacked_options(node: Node, options: dict[str, Node]) -> Node:
    copied = copy_tree(node, **options)
    copied.flag = True
    return copied


def copies_from_an_empty_list(node: Node) -> Node:
    copied = first_or_copy(node, [])
    copied.flag = True
    return copied


def hands_the_input_to_a_helper(node: Node) -> None:
    set_flag(node)


def hands_a_copy_and_the_input(node: Node) -> Node:
    copied = copy_node(node)
    set_flag(copied)
    set_flag(node)
    return copied


def hands_the_copy_to_a_keeper(node: Node) -> Node:
    copied = copy_node(node)
    keep_then_set(copied)
    return copied


def keeps_the_copy_first(node: Node) -> Node:
    copied = copy_node(node)
    KEPT.append(copied)
    set_flag(copied)
    return copied


def fills_a_shallow_copy_in_place(node: Node) -> Node:
    copied = copy_node(node)
    add_item(copied)
    return copied


def relays_the_input(node: Node) -> None:
    relay_flag(node)


def hands_the_input_to_an_untyped_helper(node: Node) -> None:
    set_any(node)


def hands_a_deep_copy_and_the_input_to_an_untyped_helper(node: Node) -> Node:
    copied = deep_node(node)
    set_any(copied)
    set_any(node)
    return copied


def hands_the_copy_to_a_remembering_helper(node: Node) -> Node:
    copied = copy_node(node)
    remember_then_set(copied)
    return copied


def takes_an_element_only(node: Node) -> Node:
    taken = first_of()
    taken.flag = True
    return taken


def iterates_a_generator_over_the_copy(node: Node) -> Node:
    copied = copy_node(node)
    for _ in flag_each(copied):
        pass
    return copied
"#;

const CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "no-node-writes"
scopes = ["pkg.passes:*"]
write-universe = ["Node.flag", "Node.items"]
"#;

fn project(name: &str, config: &str) -> Project {
    let p = Project::new(name);
    p.write("pyproject.toml", config)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", MODEL)
        .write("pkg/passes.py", PASSES);
    p
}

fn diagnostics(p: &Project) -> Vec<Value> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

/// `(qualname, line, message)` of every report in `pkg/passes.py`.
fn reports(diags: &[Value]) -> Vec<(String, u64, String)> {
    let mut out: Vec<(String, u64, String)> = diags
        .iter()
        .filter(|d| d["path"] == "pkg/passes.py")
        .map(|d| {
            (
                d["qualname"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["message"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

/// The line of the first line of `PASSES` holding `text` after the definition of `func`.
fn line_of(func: &str, text: &str) -> u64 {
    let lines: Vec<&str> = PASSES.lines().collect();
    let def = lines
        .iter()
        .position(|l| l.starts_with(&format!("def {func}(")))
        .unwrap_or_else(|| panic!("no {func}"));
    let at = (lines[def..].iter())
        .position(|l| l.contains(text))
        .unwrap_or_else(|| panic!("no {text} in {func}"));
    u64::try_from(def + at + 1).unwrap()
}

#[test]
fn new_objects_through_unpassed_arguments_and_helpers_changing_their_parameters() {
    let p = project("fresh_through_calls", CONFIG);
    let found = reports(&diagnostics(&p));
    let at = |func: &str, text: &str| (func.to_owned(), line_of(func, text));
    let lines: Vec<(String, u64)> = found.iter().map(|(q, l, _)| (q.clone(), *l)).collect();
    let mut expected = vec![
        // The table is passed: the result may be one of its elements.
        at("copies_with_replacements", "copied.flag"),
        // An element of a module-level cache, or of a dict default shared by every call.
        at("copies_with_a_cache", "copied.flag"),
        at("copies_with_a_memo_default", "copied.flag"),
        // A default string with characters has elements.
        at("copies_with_a_named_default", "copied.flag"),
        // Unpacked keywords may pass the table, and a list may get elements.
        at("copies_with_unpacked_options", "copied.flag"),
        at("copies_from_an_empty_list", "copied.flag"),
        // The helper is handed the input, directly, through a relay, or by a second call.
        at("hands_the_input_to_a_helper", "set_flag(node)"),
        at("hands_a_copy_and_the_input", "set_flag(node)"),
        at("relays_the_input", "relay_flag(node)"),
        // The helper keeps the copy before it writes it, or the caller does.
        at("hands_the_copy_to_a_keeper", "keep_then_set"),
        at("keeps_the_copy_first", "set_flag(copied)"),
        // A shallow copy's list is the original's.
        at("fills_a_shallow_copy_in_place", "add_item"),
        // An untyped helper handed the input changes a typed object it cannot see.
        at("hands_the_input_to_an_untyped_helper", "set_any(node)"),
        at(
            "hands_a_deep_copy_and_the_input_to_an_untyped_helper",
            "set_any(node)",
        ),
        // The helper hands the copy to a function keeping it before it writes it.
        at(
            "hands_the_copy_to_a_remembering_helper",
            "remember_then_set",
        ),
        // Every return gives an element: there is no new object to take.
        at("takes_an_element_only", "taken.flag"),
        // A generator's body runs where it is iterated, and the generator holds its argument.
        at("iterates_a_generator_over_the_copy", "flag_each(copied)"),
    ];
    expected.sort();
    assert_eq!(lines, expected, "{found:#?}");
    let message = |func: &str| {
        let (_, _, m) = found.iter().find(|(q, _, _)| q == func).unwrap();
        m.clone()
    };
    assert!(
        message("hands_a_copy_and_the_input").contains("via `pkg.model.set_flag`"),
        "{found:#?}"
    );
    assert!(
        message("relays_the_input").contains("via `pkg.model.relay_flag`"),
        "{found:#?}"
    );
    // `fresh-objects = "report"`: the writes that construct their objects are reported too.
    let strict = project(
        "fresh_through_calls_report",
        &format!("{CONFIG}fresh-objects = \"report\"\n"),
    );
    let all = reports(&diagnostics(&strict));
    for (func, text) in [
        ("copies_without_replacements", "copied.flag"),
        ("copies_with_none", "copied.flag"),
        ("copies_without_a_pool", "copied.flag"),
        ("hands_the_copy_to_a_helper", "set_flag(copied)"),
        ("hands_the_copy_through_a_relay", "relay_flag(copied)"),
        ("fills_a_deep_copy_in_place", "add_item"),
    ] {
        let want = at(func, text);
        assert!(
            all.iter().any(|(q, l, _)| (q.clone(), *l) == want),
            "{want:?} in {all:#?}"
        );
    }
}

#[test]
fn writes_heki_cannot_tie_to_a_class_construct_an_object_handed_new() {
    let p = project(
        "fresh_through_calls_unresolved",
        &format!("{CONFIG}unresolved = \"report\"\n"),
    );
    let found = reports(&diagnostics(&p));
    let untyped: Vec<(String, u64)> = (found.iter())
        .filter(|(_, _, m)| m.contains("unknown class"))
        .map(|(q, l, _)| (q.clone(), *l))
        .collect();
    let mut expected = vec![
        (
            "hands_the_input_to_an_untyped_helper".to_owned(),
            line_of("hands_the_input_to_an_untyped_helper", "set_any(node)"),
        ),
        // At the call handing the input over, after the one handing a copy.
        (
            "hands_a_deep_copy_and_the_input_to_an_untyped_helper".to_owned(),
            line_of(
                "hands_a_deep_copy_and_the_input_to_an_untyped_helper",
                "set_any(node)",
            ),
        ),
    ];
    expected.sort();
    assert_eq!(untyped, expected, "{found:#?}");
}

#[test]
fn write_owners_take_the_result_of_a_call_leaving_the_table_out_as_new() {
    let p = Project::new("fresh_through_calls_owners");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\"]\n\n[[tool.heki.write-owners]]\nname = \"nodes\"\ntargets = [\"Node.flag\"]\nallowed-writers = [\"pkg.model.*\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write("pkg/model.py", MODEL)
    .write("pkg/passes.py", PASSES);
    let found = reports(&diagnostics(&p));
    let names: Vec<&str> = found.iter().map(|(q, _, _)| q.as_str()).collect();
    assert!(
        !names.contains(&"copies_without_replacements"),
        "{found:#?}"
    );
    assert!(!names.contains(&"copies_with_none"), "{found:#?}");
    assert!(names.contains(&"copies_with_replacements"), "{found:#?}");
    assert!(names.contains(&"copies_with_a_memo_default"), "{found:#?}");
}

const BUILT_MODEL: &str = r#"from __future__ import annotations

from typing import Protocol


class Field:
    def __init__(self, name: str) -> None:
        self.name = name
        self.origin: str | None = None
        self.alias_scope: str | None = None


class Model:
    def __init__(self) -> None:
        self.fields: list[Field] = []


SHARED = Field("shared")
KEPT: list[Field] = []


class Builder(Protocol):
    def build_field(self, name: str) -> Field: ...


class Parser:
    def build_field(self, name: str) -> Field:
        return Field(name)


def with_origin(field: Field, kind: str) -> Field:
    field.origin = kind
    return field


def with_kept_origin(field: Field, kind: str) -> Field:
    KEPT.append(field)
    field.origin = kind
    return field


def with_origin_or_shared(field: Field, kind: str) -> Field:
    if kind:
        return field
    return SHARED


def append_field(model: Model, field: Field) -> None:
    model.fields.append(field)


def record_scope(field: Field, scope: str) -> None:
    field.alias_scope = scope
"#;

const BUILT_PASSES: &str = r#"from __future__ import annotations

from pkg.model import (
    Builder,
    Field,
    Model,
    Parser,
    append_field,
    record_scope,
    with_kept_origin,
    with_origin,
    with_origin_or_shared,
)


def through_the_protocol(parser: Builder) -> Field:
    field = with_origin(parser.build_field("x"), "tag")
    record_scope(field, "scope")
    return field


def through_the_class(parser: Parser) -> Field:
    field = with_origin(parser.build_field("x"), "tag")
    record_scope(field, "scope")
    return field


def records_before_appending(parser: Builder, model: Model) -> Field:
    field = with_origin(parser.build_field("x"), "tag")
    record_scope(field, "scope")
    append_field(model, field)
    return field


def tags_the_input(field: Field) -> Field:
    tagged = with_origin(field, "tag")
    record_scope(tagged, "scope")
    return tagged


def keeps_while_tagging(parser: Builder) -> Field:
    field = with_kept_origin(parser.build_field("x"), "tag")
    record_scope(field, "scope")
    return field


def may_take_the_shared_one(parser: Builder) -> Field:
    field = with_origin_or_shared(parser.build_field("x"), "tag")
    record_scope(field, "scope")
    return field


def appends_before_recording(parser: Builder, model: Model) -> Field:
    field = with_origin(parser.build_field("x"), "tag")
    append_field(model, field)
    record_scope(field, "scope")
    return field
"#;

#[test]
fn an_object_a_helper_hands_back_is_the_one_it_was_handed() {
    let p = Project::new("fresh_through_calls_handed_back");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"EFF201\"]\n\n[[tool.heki.effect-boundaries]]\nname = \"no-field-writes\"\nscopes = [\"pkg.passes:*\"]\nwrite-universe = [\"Field.origin\", \"Field.alias_scope\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write("pkg/model.py", BUILT_MODEL)
    .write("pkg/passes.py", BUILT_PASSES);
    let mut found: Vec<(String, String)> = reports(&diagnostics(&p))
        .into_iter()
        .map(|(q, _, m)| {
            let target = m.split('`').nth(3).unwrap_or_default().to_owned();
            (q, target)
        })
        .collect();
    let q = |func: &str, target: &str| (func.to_owned(), target.to_owned());
    // A field the parser builds (a constructor call in the one implementation of the protocol's
    // method), handed through a helper returning it, is written while nothing holds it.
    // Reported: the input handed through; a helper keeping it, or returning a shared field on
    // another path; the field held by the model before the write.
    let mut expected = vec![
        q("tags_the_input", "Field.origin"),
        q("tags_the_input", "Field.alias_scope"),
        q("keeps_while_tagging", "Field.origin"),
        q("keeps_while_tagging", "Field.alias_scope"),
        q("may_take_the_shared_one", "Field.alias_scope"),
        q("appends_before_recording", "Field.alias_scope"),
    ];
    expected.sort();
    found.sort();
    assert_eq!(found, expected);
}
