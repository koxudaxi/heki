//! Registration contexts through `heki inspect` (`registered`): a registration stores what a
//! factory returns in a slot of the rows of a table, a holder keeps the table, a row is picked
//! and its slot called, the callback builds an object of the class it captured and calls a
//! method of it through an adapter that orders the arguments by the method's signature, and the
//! method calls the handler `getattr(self, <name>)` finds. Each context holds for one
//! registration and one name; the changes under it are those of the handler with the parameter
//! of the registering object's class, while the handler's own facts stay as they are.

mod common;

use common::Project;
use serde_json::Value;

/// `(file, old, new)`: `old` replaced once in the file.
type Edit<'e> = (&'e str, &'e str, &'e str);

/// The `panels` project, each file with the edits `(file, old, new)` applied once.
fn panels(name: &str, edits: &[(&str, &str, &str)]) -> Project {
    let p = Project::new(name);
    p.write("pyproject.toml", "[project]\nname = \"t\"\n");
    p.write("panels/__init__.py", "");
    for (file, text) in common::panels::files(edits) {
        p.write(&format!("panels/{file}"), &text);
    }
    p
}

fn inspect(p: &Project, symbol: &str) -> Value {
    let (code, out, err) = p.heki(&["inspect", symbol, "--output-format", "json"]);
    assert_eq!(code, 0, "{out}\n{err}");
    serde_json::from_str(&out).unwrap()
}

/// `(type, the slots of each change record under the context)` of each context of `symbol`: a
/// context that changes nothing has no record.
fn contexts(p: &Project, symbol: &str) -> Vec<(String, Vec<String>)> {
    let r = inspect(p, symbol);
    assert_eq!(r["changes"]["params"], Value::Array(Vec::new()), "{r:#}");
    records(&r)
}

fn records(r: &Value) -> Vec<(String, Vec<String>)> {
    (r["registered"].as_array().unwrap().iter())
        .map(|c| {
            let records = (c["changes"].as_array().unwrap().iter())
                .map(|ch| {
                    let slots = ch["slots"].as_array().unwrap().iter();
                    let slots: Vec<&str> = slots.map(|s| s.as_str().unwrap()).collect();
                    slots.join(", ")
                })
                .collect();
            (c["type"].as_str().unwrap().to_owned(), records)
        })
        .collect()
}

fn one(class: &str, slot: &str) -> Vec<(String, Vec<String>)> {
    vec![(class.to_owned(), vec![slot.to_owned()])]
}

#[test]
fn each_registration_reaches_its_handler_with_its_owner() {
    let p = panels("dispatch_panels", &[]);
    let r = inspect(&p, "panels.views.Page.store");
    let c = &r["registered"][0];
    assert_eq!(r["registered"].as_array().unwrap().len(), 1, "{r:#}");
    assert_eq!(
        (c["param"].as_str(), c["index"].as_u64()),
        (Some("ledger"), Some(2))
    );
    assert_eq!(c["type"], "panels.owners.Left");
    let site = |s: &Value| {
        (
            s["function"].as_str().unwrap().to_owned(),
            s["line"].as_u64().unwrap(),
        )
    };
    assert_eq!(
        site(&c["registration"]),
        ("panels.owners.Left.entries".to_owned(), 19)
    );
    assert_eq!(
        site(&c["called_at"]),
        ("panels.channel.Channel.serve".to_owned(), 14)
    );
    assert_eq!(
        site(&c["selected_at"]),
        ("panels.views.Commands.choose".to_owned(), 4)
    );
    assert_eq!(
        site(&c["invoked_at"]),
        ("panels.views.Commands.choose".to_owned(), 5)
    );
    assert_eq!(c["condition"]["selector"], "message.action.lower()");
    assert_eq!(c["condition"]["equals"], "store");
    assert_eq!(c["condition"]["others_open"], true);
    let change = &c["changes"][0];
    assert_eq!(change["what"], serde_json::json!(["journal"]));
    assert_eq!(change["direct"], false);
    assert_eq!(change["chain"][0]["calls"], "panels.owners.Left.stamp");
    assert_eq!(
        (change["at"]["path"].as_str(), change["at"]["line"].as_u64()),
        (Some("panels/owners.py"), Some(11))
    );
    assert_eq!(
        contexts(&p, "panels.views.Deck.store"),
        one("panels.owners.Right", "audit")
    );
    // An unregistered method of the same name, and a registered one that changes nothing.
    assert_eq!(contexts(&p, "panels.views.Unused.store"), Vec::new());
    assert_eq!(
        contexts(&p, "panels.views.Page.noop"),
        vec![("panels.owners.Left".to_owned(), Vec::new())]
    );
    let (code, text, err) = p.heki(&["inspect", "panels.views.Page.store"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        text.contains("registered ledger as panels.owners.Left by panels.owners.Left.entries (owners.py:19), when message.action.lower() == \"store\", other names not followed\n  then changes ledger.journal: "),
        "{text}"
    );
}

#[test]
fn two_owners_of_one_handler_keep_their_own_contexts() {
    let p = panels(
        "dispatch_shared",
        &[("owners.py", "capture(Deck, self)", "capture(Page, self)")],
    );
    let mut found = contexts(&p, "panels.views.Page.store");
    found.sort();
    let mut both = one("panels.owners.Left", "journal");
    both.extend(one("panels.owners.Right", "audit"));
    assert_eq!(found, both);
    assert_eq!(contexts(&p, "panels.views.Deck.store"), Vec::new());
}

#[test]
fn a_constant_name_selects_only_its_handler() {
    let p = panels(
        "dispatch_constant",
        &[("views.py", "getattr(self, name)", "getattr(self, \"noop\")")],
    );
    let r = inspect(&p, "panels.views.Page.noop");
    assert_eq!(r["registered"][0]["condition"]["selector"], "\"noop\"");
    assert_eq!(r["registered"][0]["condition"]["others_open"], false);
    assert_eq!(contexts(&p, "panels.views.Page.store"), Vec::new());
    let (code, text, err) = p.heki(&["inspect", "panels.views.Page.noop"]);
    assert_eq!(code, 0, "{err}");
    assert!(text.contains("when \"noop\" == \"noop\"\n"), "{text}");
    let p = panels(
        "dispatch_undefined",
        &[(
            "views.py",
            "getattr(self, name)",
            "getattr(self, \"missing\")",
        )],
    );
    assert_eq!(contexts(&p, "panels.views.Page.store"), Vec::new());
    assert_eq!(contexts(&p, "panels.views.Page.noop"), Vec::new());
}

#[test]
fn a_read_in_place_of_the_change_keeps_the_context_without_it() {
    let p = panels(
        "dispatch_read",
        &[(
            "owners.py",
            "self.journal.append(1)",
            "self.journal.count(1)",
        )],
    );
    assert_eq!(
        contexts(&p, "panels.views.Page.store"),
        vec![("panels.owners.Left".to_owned(), Vec::new())]
    );
    assert_eq!(
        contexts(&p, "panels.views.Deck.store"),
        one("panels.owners.Right", "audit")
    );
}

/// The table reaches the holder by keyword, and the helper picks the row of the registered
/// key: the context stays.
#[test]
fn a_table_passed_by_keyword_and_a_constant_row_key_keep_the_context() {
    let p = panels(
        "dispatch_keyword_table",
        &[
            (
                "owners.py",
                "        return Channel(table)\n\n\nclass Right:",
                "        return Channel(bindings=table)\n\n\nclass Right:",
            ),
            (
                "channel.py",
                "select(self.items, key)",
                "select(self.items, \"first\")",
            ),
        ],
    );
    assert_eq!(
        contexts(&p, "panels.views.Page.store"),
        one("panels.owners.Left", "journal")
    );
    assert_eq!(contexts(&p, "panels.views.Deck.store"), Vec::new());
}

/// Starred operands of a display give their items (an empty one gives none), and so do those
/// of a call, a starred tuple of positions among them.
#[test]
fn starred_operands_of_a_display_hand_on_their_items() {
    let p = panels(
        "dispatch_starred_display",
        &[(
            "adapter.py",
            "    return await callback(*values)",
            "    prefix = []\n    return await callback(*[*prefix, *values])",
        )],
    );
    assert_eq!(
        contexts(&p, "panels.views.Page.store"),
        one("panels.owners.Left", "journal")
    );
    let p = panels(
        "dispatch_starred_tuple",
        &[(
            "adapter.py",
            "    return await callback(*values)",
            "    return await forward(callback, *values)\n\n\nasync def forward(fn, *rest):\n    return await fn(*rest)",
        )],
    );
    assert_eq!(
        contexts(&p, "panels.views.Page.store"),
        one("panels.owners.Left", "journal")
    );
}

/// A constant name is looked up among the members it names only: helpers of another name do
/// not count toward the most handlers a lookup may find.
#[test]
fn a_constant_name_is_followed_past_many_other_members() {
    let helpers: String = (0..9)
        .map(|i| format!("    def help{i}(self):\n        pass\n\n"))
        .collect();
    let p = panels(
        "dispatch_constant_many",
        &[
            (
                "views.py",
                "class Commands:\n",
                &format!("class Commands:\n{helpers}"),
            ),
            (
                "views.py",
                "getattr(self, name)",
                "getattr(self, \"store\")",
            ),
        ],
    );
    assert_eq!(
        contexts(&p, "panels.views.Page.store"),
        one("panels.owners.Left", "journal")
    );
}

/// A lookup in a helper of another module: the lookup and the call of what it found keep the
/// function and file each runs in.
#[test]
fn a_lookup_in_another_module_keeps_its_own_site() {
    let p = panels(
        "dispatch_lookup_elsewhere",
        &[
            (
                "views.py",
                "class Commands:\n",
                "from .lookup import find\n\n\nclass Commands:\n",
            ),
            ("views.py", "getattr(self, name)", "find(self, name)"),
        ],
    );
    p.write(
        "panels/lookup.py",
        "def find(view, name):\n    return getattr(view, name)\n",
    );
    let r = inspect(&p, "panels.views.Page.store");
    let c = &r["registered"][0];
    let site = |s: &Value| {
        (
            s["function"].as_str().map(str::to_owned),
            s["path"].as_str().map(str::to_owned),
            s["line"].as_u64(),
        )
    };
    assert_eq!(
        site(&c["selected_at"]),
        (
            Some("panels.lookup.find".to_owned()),
            Some("panels/lookup.py".to_owned()),
            Some(2)
        ),
        "{r:#}"
    );
    assert_eq!(
        site(&c["invoked_at"]),
        (
            Some("panels.views.Commands.choose".to_owned()),
            Some("panels/views.py".to_owned()),
            Some(8)
        )
    );
}

/// A handler whose parameter is annotated with a base class of both owners: each context types
/// it as its own owner's class.
#[test]
fn a_context_types_an_annotated_parameter_as_its_owner() {
    let p = panels(
        "dispatch_annotated",
        &[
            ("owners.py", "capture(Deck, self)", "capture(Page, self)"),
            (
                "owners.py",
                "from .views import Page, Deck\n",
                "from .views import Page, Deck, Owner\n",
            ),
            ("owners.py", "class Left:", "class Left(Owner):"),
            ("owners.py", "class Right:", "class Right(Owner):"),
            (
                "views.py",
                "class Commands:\n",
                "class Owner:\n    def stamp(self):\n        pass\n\n\nclass Commands:\n",
            ),
            (
                "views.py",
                "    async def store(self, packet, ledger):\n        ledger.stamp()\n\n    async def noop",
                "    async def store(self, packet, ledger: Owner):\n        ledger.stamp()\n\n    async def noop",
            ),
        ],
    );
    let mut found = records(&inspect(&p, "panels.views.Page.store"));
    found.sort();
    let mut both = one("panels.owners.Left", "journal");
    both.extend(one("panels.owners.Right", "audit"));
    assert_eq!(found, both);
}

/// A change of a container held in an item of the parameter is named in the text report.
#[test]
fn the_text_report_names_items_changed_under_a_context() {
    let p = panels(
        "dispatch_item_text",
        &[(
            "views.py",
            "    async def noop(self, packet, ledger):\n        pass\n",
            "    async def noop(self, packet, ledger):\n        ledger[\"k\"].append(1)\n",
        )],
    );
    let (code, text, err) = p.heki(&["inspect", "panels.views.Page.noop"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        text.contains("  then changes ledger[\"k\"]: in place, "),
        "{text}"
    );
}

/// Helpers the join reads for whether the registered row can come back: tests on the way to
/// the return that the row's key and the call's arguments decide (or leave to run time), and
/// returns not reached through plain branches from the loop's body or rows not unpacked into
/// plain targets, which give no context.
#[test]
fn helper_shapes_decide_whether_the_row_comes_back() {
    let select = "def select(rows, wanted):\n    for tag, item in rows:\n        if tag == wanted:\n            return tag, item\n    return None, None\n";
    let skips = "def select(rows, wanted):\n    for tag, item in rows:\n        if tag is None:\n            continue\n        if not tag:\n            continue\n        if tag == 0:\n            continue\n        if tag != wanted:\n            continue\n        if tag == \"unused\":\n            continue\n        if len(tag) < 0:\n            continue\n        return tag, item\n    return None, None\n";
    let unset = "def select(rows, wanted):\n    for tag, item in rows:\n        if wanted is None:\n            return tag, item\n    return None, None\n";
    let merged = "def select(rows, wanted):\n    for tag, item in rows:\n        if tag == wanted:\n            found = 1\n        else:\n            found = 2\n        return tag, item\n    return None, None\n";
    let handler = "def select(rows, wanted):\n    for tag, item in rows:\n        try:\n            wanted.check(tag)\n        except ValueError:\n            return tag, item\n    return None, None\n";
    let in_a_try = "def select(rows, wanted):\n    for tag, item in rows:\n        try:\n            if tag == wanted:\n                return tag, item\n        except ValueError:\n            pass\n    return None, None\n";
    let starred = "def select(rows, wanted):\n    for tag, *rest in rows:\n        if tag == wanted:\n            return tag, rest\n    return None, None\n";
    let key = |k: &'static str| ("channel.py", "select(self.items, key)", k);
    let (first, second, none) = (
        key("select(self.items, \"first\")"),
        key("select(self.items, \"second\")"),
        key("select(self.items, None)"),
    );
    let cases: &[(&str, &str, Option<Edit>, bool)] = &[
        ("skips", skips, None, true),
        ("skips_other_key", skips, Some(second), false),
        ("skips_first_key", skips, Some(first), true),
        ("unset_argument", unset, Some(none), true),
        ("after_a_merge", merged, None, false),
        ("in_a_try", in_a_try, None, true),
        ("in_a_handler", handler, None, false),
        ("starred_target", starred, None, false),
    ];
    let wrong: Vec<&str> = (cases.iter())
        .filter(|(name, helper, call, kept)| {
            let mut edits = vec![("channel.py", select, *helper)];
            edits.extend(*call);
            let p = panels(&format!("dispatch_helper_{name}"), &edits);
            let found = contexts(&p, "panels.views.Page.store");
            found
                != if *kept {
                    one("panels.owners.Left", "journal")
                } else {
                    Vec::new()
                }
        })
        .map(|(name, ..)| *name)
        .collect();
    assert_eq!(
        wrong,
        Vec::<&str>::new(),
        "the helpers the join reads wrongly"
    );
}

#[test]
fn a_registration_without_its_owner_gives_no_context() {
    let p = panels(
        "dispatch_no_owner",
        &[("owners.py", "capture(Page, self)", "capture(Page, None)")],
    );
    assert_eq!(contexts(&p, "panels.views.Page.store"), Vec::new());
    assert_eq!(
        contexts(&p, "panels.views.Deck.store"),
        one("panels.owners.Right", "audit")
    );
}

/// Each edit breaks a premise of the chain the first owner's registration takes.
#[test]
fn premises_that_break_give_no_context() {
    let cases: &[(&str, &[Edit])] = &[
        (
            "missing_keyword",
            &[("adapter.py", "owner=resource", "absent=resource")],
        ),
        (
            "unawaited_callback",
            &[(
                "adapter.py",
                "return await callback(*values)",
                "callback(*values)\n    return None",
            )],
        ),
        (
            "handler_never_called",
            &[(
                "views.py",
                "return await operation(event, owner)",
                "return None",
            )],
        ),
        (
            "attribute_hook",
            &[(
                "views.py",
                "class Commands:\n",
                "class Commands:\n    def __getattr__(self, key):\n        return None\n\n",
            )],
        ),
        (
            "member_replaced",
            &[(
                "views.py",
                "class Unused(Commands):",
                "Commands.choose = lambda *args: None\n\n\nclass Unused(Commands):",
            )],
        ),
        (
            "decorator_replaces",
            &[(
                "adapter.py",
                "async def spread(callback, **items):",
                "def substitute(fn):\n    async def other(*a, **kw):\n        return None\n    return other\n\n\n@substitute\nasync def spread(callback, **items):",
            )],
        ),
        (
            "read_but_never_called",
            &[(
                "channel.py",
                "return await callback(message, stream)",
                "return callback(message, stream)",
            )],
        ),
        (
            "wrong_value",
            &[(
                "adapter.py",
                "values.append(options[field])",
                "values.append(options[\"output\"])",
            )],
        ),
        (
            "keyword_only",
            &[(
                "views.py",
                "async def choose(self, owner, event):",
                "async def choose(self, owner, *, event):",
            )],
        ),
        (
            "table_overwritten_in_the_constructor",
            &[(
                "channel.py",
                "        self.items = bindings\n",
                "        self.items = bindings\n        self.items = []\n",
            )],
        ),
        (
            "table_argument_keyword_only",
            &[(
                "channel.py",
                "def __init__(self, bindings):",
                "def __init__(self, *, bindings):",
            )],
        ),
        (
            "row_unpacked_into_more_targets",
            &[(
                "channel.py",
                "for tag, item in rows:",
                "for tag, item, extra in rows:",
            )],
        ),
        (
            "row_key_of_another_registration",
            &[(
                "channel.py",
                "select(self.items, key)",
                "select(self.items, \"second\")",
            )],
        ),
        (
            "attribute_hook_stored",
            &[(
                "views.py",
                "class Page(Commands):",
                "Commands.__getattr__ = None\n\n\nclass Page(Commands):",
            )],
        ),
        (
            "attribute_hook_assigned",
            &[(
                "views.py",
                "class Commands:\n",
                "def redirect(self, key):\n    return None\n\n\nclass Commands:\n    __getattribute__ = redirect\n\n",
            )],
        ),
        (
            "constant_lookup_that_fails_first",
            &[(
                "views.py",
                "        operation = getattr(self, name)\n",
                "        await getattr(self, \"missing\")(event, owner)\n        operation = getattr(self, name)\n",
            )],
        ),
        (
            "coroutine_awaited_twice",
            &[(
                "views.py",
                "        return await operation(event, owner)",
                "        pending = operation(event, owner)\n        alias = pending\n        await pending\n        return await alias",
            )],
        ),
        (
            "capture_rebound",
            &[(
                "adapter.py",
                "    return receive",
                "    resource = None\n    return receive",
            )],
        ),
        (
            "object_tested",
            &[(
                "adapter.py",
                "        worker = component()\n",
                "        worker = component()\n        if worker:\n            pass\n",
            )],
        ),
        (
            "membership_of_a_list",
            &[(
                "adapter.py",
                "if field not in options:",
                "if field not in order:",
            )],
        ),
        (
            "operator",
            &[(
                "adapter.py",
                "    values = arguments(callback, items)\n",
                "    values = arguments(callback, items) + []\n",
            )],
        ),
        (
            "starred_mapping",
            &[("adapter.py", "callback(*values)", "callback(*items)")],
        ),
        (
            "double_starred",
            &[(
                "adapter.py",
                "spread(worker.choose, event=note",
                "spread(worker.choose, **note, event=note",
            )],
        ),
        (
            "method_attribute",
            &[(
                "adapter.py",
                "spread(worker.choose,",
                "spread(worker.choose.__func__,",
            )],
        ),
        (
            "signature_of_a_mapping",
            &[(
                "adapter.py",
                "reflection.signature(callback)",
                "reflection.signature(options)",
            )],
        ),
        (
            "unknown_builtin",
            &[(
                "adapter.py",
                "        worker = component()\n",
                "        print(note)\n        worker = component()\n",
            )],
        ),
        (
            "calls_an_argument",
            &[(
                "adapter.py",
                "        worker = component()\n",
                "        note(emit)\n        worker = component()\n",
            )],
        ),
        (
            "awaits_a_value",
            &[(
                "views.py",
                "        operation = getattr(self, name)\n",
                "        await name\n        operation = getattr(self, name)\n",
            )],
        ),
        (
            "store_on_an_object",
            &[(
                "adapter.py",
                "        worker = component()\n",
                "        worker = component()\n        worker.flag = 1\n",
            )],
        ),
        (
            "iterates_a_mapping",
            &[("adapter.py", "for field in order:", "for field in options:")],
        ),
        (
            "external_base",
            &[("views.py", "class Commands:", "class Commands(dict):")],
        ),
        (
            "not_an_appender",
            &[(
                "owners.py",
                "            registry.append((key, callback))\n",
                "            registry.append((key, callback))\n\n        def peek():\n            return registry\n",
            )],
        ),
        (
            "table_joined",
            &[(
                "owners.py",
                "        return registry",
                "        alias = registry or []\n        return registry",
            )],
        ),
        (
            "table_sorted",
            &[(
                "owners.py",
                "        return registry",
                "        registry.sort()\n        return registry",
            )],
        ),
        (
            "factory_gives_none",
            &[("adapter.py", "    return receive", "    return None")],
        ),
        (
            "field_written_twice",
            &[(
                "channel.py",
                "    async def serve",
                "    def reset(self):\n        self.items = []\n\n    async def serve",
            )],
        ),
        (
            "too_many_handlers",
            &[(
                "views.py",
                "    async def noop(",
                "    async def extra0(self, packet, ledger):\n        pass\n\n    async def extra1(self, packet, ledger):\n        pass\n\n    async def extra2(self, packet, ledger):\n        pass\n\n    async def extra3(self, packet, ledger):\n        pass\n\n    async def extra4(self, packet, ledger):\n        pass\n\n    async def extra5(self, packet, ledger):\n        pass\n\n    async def extra6(self, packet, ledger):\n        pass\n\n    async def extra7(self, packet, ledger):\n        pass\n\n    async def noop(",
            )],
        ),
        (
            "isinstance_of_another_type",
            &[(
                "adapter.py",
                "        worker = component()\n",
                "        if isinstance(component, int):\n            pass\n        worker = component()\n",
            )],
        ),
        (
            "decorated_dispatcher",
            &[
                (
                    "views.py",
                    "class Commands:\n",
                    "from typing import final\n\n\nclass Commands:\n",
                ),
                (
                    "views.py",
                    "    async def choose(",
                    "    @final\n    async def choose(",
                ),
            ],
        ),
        (
            "bare_raise",
            &[(
                "adapter.py",
                "        worker = component()\n",
                "        worker = component()\n        if worker is not None:\n            raise\n",
            )],
        ),
        (
            "item_of_a_list",
            &[(
                "adapter.py",
                "values.append(options[field])",
                "values.append(values[field])",
            )],
        ),
    ];
    let kept: Vec<&str> = (cases.iter())
        .filter(|(name, edits)| {
            let p = panels(&format!("dispatch_{name}"), edits);
            !contexts(&p, "panels.views.Page.store").is_empty()
        })
        .map(|(name, _)| *name)
        .collect();
    assert_eq!(kept, Vec::<&str>::new(), "the cases that keep a context");
}

const MARKS: &str = r#"LISTED = []


def listed(fn=None, *, label=None):
    def mark(fn):
        fn.listed_label = label or f"listed_{fn.__name__}"
        LISTED.append(fn)
        return fn

    if fn is None:
        return mark
    return mark(fn)


def kinds():
    return {"a", "b", 1.5}
"#;

const WIRING: &str = r#"import inspect
import types

from . import marks


def attach(target, hub, label=None):
    if isinstance(target, types.FunctionType):
        return attach_function(target, hub)
    if not isinstance(target, type):
        raise ValueError(label)
    return attach_class(target, hub=hub)


def attach_function(fn, hub):
    async def run(request, send):
        return await fn(request, hub)

    return run


def attach_class(cls, hub):
    async def run(request, send):
        debug = False
        ratio = 1.5
        screen = cls()
        if inspect.iscoroutinefunction(screen.__call__):
            return await call_async(screen.__call__, request=request, send=send, hub=hub)
        return call_sync(screen.__call__, request=request, send=send, hub=hub)

    run.screen_class = cls
    return run


def project(fn, values, *extra):
    names = inspect.signature(fn).parameters.keys()
    picked = []
    for name in names:
        if name not in values:
            raise TypeError(f"{fn} needs {tuple(names)}")
        picked.append(values[name])
    return picked


@marks.listed
async def call_async(fn, **values):
    return await fn(*project(fn, values))


def call_sync(fn, **values):
    return fn(*project(fn, values))
"#;

const SCREENS: &str = r#"class Screen:
    async def head(self, request, hub):
        return await self.get(request, hub)

    async def ready(self):
        return True

    async def __call__(self, request, hub):
        try:
            handler = getattr(self, request.method.lower())
        except AttributeError:
            return None
        return await handler(request, hub=hub)


class Overview(Screen):
    async def get(self, request, hub):
        hub.permits(request)


class Search(Screen):
    async def get(self, request, hub):
        return hub.permits(request)

    async def post(self, request, hub):
        return None

    async def put(self, request, hub, *, extra):
        return None

    async def patch(self, request, hub, extra=None):
        return None
"#;

const HUB: &str = r#"import re

from .screens import Overview, Search
from .wiring import attach


class Hub:
    def __init__(self):
        self._checks = []

    def record(self, item):
        self._checks.append(item)

    def permits(self, item):
        return self._inspect(item)

    def _inspect(self, item):
        self.record(item)
        return True

    def extras(self):
        return []

    def _table(self):
        table = []
        for extra in self.extras():
            table.append(("extra", attach(extra, self, 0)))

        def add(target, pattern):
            table.append((pattern, target))

        add(attach(Overview, self, None), r"/overview$")
        add(target=attach(Search, self, label="search"), pattern=r"/search$")
        for extra in self.extras():
            add(extra(Search, self), r"/extra$")
        return [((re.compile(p) if isinstance(p, str) else p), target, len(p)) for p, target in table]

    def app(self):
        table = self._table()
        return Router(self, table=table)

    def spare(self):
        return Plain(self._table())


def loose(hub):
    table = []
    table.append(("k", attach(Overview, hub)))
    return table


class Plain:
    pass


class Router:
    def __init__(self, hub, table):
        self.hub = hub
        self.table = table or []

    async def route(self, request, send, path):
        found, target = pick(self.table, path)
        return await target(request, send)

    async def first(self, request, send):
        return await self.table[0][1](request, send)

    async def last(self, request, send):
        found, target = sorted(self.table)
        return await target(request, send)

    async def other(self, request, send):
        found, target = pick([], request)
        return await target(request, send)


def pick(table, path):
    for pattern, target, size in table:
        if pattern == path:
            return pattern, target
        found = pattern.match(path)
        if found is not None:
            return found, target
    return None, None
"#;

/// A class factory chosen by `isinstance`, a callback that tests whether the method is a
/// coroutine function and keeps metadata, an adapter decorated by a function that returns it,
/// a lookup in `try`, a table kept as `table or []` and returned as a comprehension (a function
/// of its own before Python 3.12), and a handler called with a keyword.
#[test]
fn adapters_decorators_and_kept_tables_under_both_comprehension_models() {
    for version in ["py311", "py313"] {
        let p = Project::new(&format!("dispatch_relay_{version}"));
        p.write(
            "pyproject.toml",
            &format!("[project]\nname = \"t\"\n\n[tool.heki]\ntarget-version = \"{version}\"\n"),
        );
        p.write("relay/__init__.py", "");
        p.write("relay/marks.py", MARKS);
        p.write("relay/wiring.py", WIRING);
        p.write("relay/screens.py", SCREENS);
        p.write("relay/hub.py", HUB);
        let r = inspect(&p, "relay.screens.Overview.get");
        let c = &r["registered"][0];
        assert_eq!(
            r["registered"].as_array().unwrap().len(),
            1,
            "{version}: {r:#}"
        );
        assert_eq!(
            (c["param"].as_str(), c["type"].as_str()),
            (Some("hub"), Some("relay.hub.Hub"))
        );
        assert_eq!(c["registration"]["line"], 32);
        assert_eq!(c["condition"]["selector"], "request.method.lower()");
        assert_eq!(c["called_at"]["function"], "relay.hub.Router.route");
        let change = &c["changes"][0];
        assert_eq!(change["slots"], serde_json::json!(["_checks"]));
        let calls: Vec<&str> = (change["chain"].as_array().unwrap().iter())
            .map(|h| h["calls"].as_str().unwrap())
            .collect();
        assert_eq!(
            calls,
            [
                "relay.hub.Hub.permits",
                "relay.hub.Hub._inspect",
                "relay.hub.Hub.record"
            ]
        );
        assert_eq!(
            (
                change["at"]["line"].as_u64(),
                change["at"]["column"].as_u64()
            ),
            (Some(12), Some(9))
        );
        assert_eq!(
            contexts(&p, "relay.screens.Search.post"),
            vec![("relay.hub.Hub".to_owned(), Vec::new())]
        );
        assert_eq!(contexts(&p, "relay.screens.Screen.ready"), Vec::new());
        // A keyword-only parameter the call leaves unbound fails the call; a default does not.
        assert_eq!(contexts(&p, "relay.screens.Search.put"), Vec::new());
        let patched = contexts(&p, "relay.screens.Search.patch");
        assert_eq!(patched, vec![("relay.hub.Hub".to_owned(), Vec::new())]);
        // The comprehension unpacking each row into more targets than it holds raises.
        let copy = "for p, target in table]";
        assert_eq!(HUB.matches(copy).count(), 1);
        p.write(
            "relay/hub.py",
            &HUB.replace(copy, "for p, target, extra in table]"),
        );
        assert_eq!(contexts(&p, "relay.screens.Overview.get"), Vec::new());
    }
}
