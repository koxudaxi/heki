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

const OWNERS: &str = r#"from .adapter import package as capture
from .channel import Channel
from .views import Page, Deck


class Left:
    def __init__(self):
        self.journal = []

    def stamp(self):
        self.journal.append(1)

    def entries(self):
        registry = []

        def save(key, callback):
            registry.append((key, callback))

        save("first", capture(Page, self))
        return registry

    def start(self):
        table = self.entries()
        return Channel(table)


class Right:
    def __init__(self):
        self.audit = []

    def stamp(self):
        self.audit.append(1)

    def entries(self):
        index = []

        def save(key, callback):
            index.append((key, callback))

        save("second", capture(Deck, self))
        return index

    def start(self):
        table = self.entries()
        return Channel(table)
"#;

const ADAPTER: &str = r#"import inspect as reflection


def arguments(callback, options):
    order = reflection.signature(callback).parameters.keys()
    values = []
    for field in order:
        if field not in options:
            raise TypeError(field)
        values.append(options[field])
    return values


async def spread(callback, **items):
    values = arguments(callback, items)
    return await callback(*values)


def package(component, resource):
    async def receive(note, emit):
        worker = component()
        return await spread(worker.choose, event=note, owner=resource, output=emit)
    return receive
"#;

const CHANNEL: &str = r#"def select(rows, wanted):
    for tag, item in rows:
        if tag == wanted:
            return tag, item
    return None, None


class Channel:
    def __init__(self, bindings):
        self.items = bindings

    async def serve(self, message, stream, key):
        tag, callback = select(self.items, key)
        return await callback(message, stream)
"#;

const VIEWS: &str = r#"class Commands:
    async def choose(self, owner, event):
        name = event.action.lower()
        operation = getattr(self, name)
        return await operation(event, owner)


class Page(Commands):
    async def store(self, packet, ledger):
        ledger.stamp()

    async def noop(self, packet, ledger):
        pass


class Deck(Commands):
    async def store(self, packet, ledger):
        ledger.stamp()


class Unused(Commands):
    async def store(self, packet, ledger):
        ledger.stamp()
"#;

/// The `panels` project, each file with the edits `(file, old, new)` applied once.
fn panels(name: &str, edits: &[(&str, &str, &str)]) -> Project {
    let p = Project::new(name);
    p.write("pyproject.toml", "[project]\nname = \"t\"\n");
    p.write("panels/__init__.py", "");
    for (file, text) in [
        ("owners.py", OWNERS),
        ("adapter.py", ADAPTER),
        ("channel.py", CHANNEL),
        ("views.py", VIEWS),
    ] {
        let mut text = text.to_owned();
        for (_, old, new) in edits.iter().filter(|(f, _, _)| *f == file) {
            assert_eq!(text.matches(old).count(), 1, "{file}: {old}");
            text = text.replace(old, new);
        }
        p.write(&format!("panels/{file}"), &text);
    }
    p
}

fn inspect(p: &Project, symbol: &str) -> Value {
    let (code, out, err) = p.heki(&["inspect", symbol, "--output-format", "json"]);
    assert_eq!(code, 0, "{out}\n{err}");
    serde_json::from_str(&out).unwrap()
}

/// `(type, slots changed under the context)` of each context of `symbol`.
fn contexts(p: &Project, symbol: &str) -> Vec<(String, Vec<String>)> {
    let r = inspect(p, symbol);
    assert_eq!(r["changes"]["params"], Value::Array(Vec::new()), "{r:#}");
    (r["registered"].as_array().unwrap().iter())
        .map(|c| {
            let slots = (c["changes"].as_array().unwrap().iter())
                .flat_map(|ch| ch["slots"].as_array().unwrap().clone())
                .map(|s| s.as_str().unwrap().to_owned())
                .collect();
            (c["type"].as_str().unwrap().to_owned(), slots)
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
    let cases: &[(&str, &[(&str, &str, &str)])] = &[
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
    ];
    for (name, edits) in cases {
        let p = panels(&format!("dispatch_{name}"), edits);
        assert_eq!(
            contexts(&p, "panels.views.Page.store"),
            Vec::new(),
            "{name}"
        );
    }
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
    return {"a", "b"}
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
            table.append(("extra", attach(extra, self)))

        def add(target, pattern):
            table.append((pattern, target))

        add(attach(Overview, self, None), r"/overview$")
        add(attach(Search, self, label="search"), r"/search$")
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
    for pattern, target in table:
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
    }
}
