//! A project of four modules that registers callbacks, keeps them in tables, and dispatches to
//! handlers by name (`heki inspect`'s registration contexts): `Left` and `Right` each register a
//! callback for a view class with themselves as the owner, `Channel` keeps the table and calls
//! the row it picks, the adapter orders the arguments by the method's signature, and
//! `Commands.choose` calls the handler `getattr` finds.

pub const OWNERS: &str = r#"from .adapter import package as capture
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

pub const ADAPTER: &str = r#"import inspect as reflection


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

pub const CHANNEL: &str = r#"def select(rows, wanted):
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

pub const VIEWS: &str = r#"class Commands:
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

/// The modules, each with the edits `(file, old, new)` for it applied (each `old` once).
pub fn files(edits: &[(&str, &str, &str)]) -> Vec<(&'static str, String)> {
    let all = [
        ("owners.py", OWNERS),
        ("adapter.py", ADAPTER),
        ("channel.py", CHANNEL),
        ("views.py", VIEWS),
    ];
    (all.into_iter())
        .map(|(file, text)| {
            let mut text = text.to_owned();
            for (_, old, new) in edits.iter().filter(|(f, _, _)| *f == file) {
                assert_eq!(text.matches(old).count(), 1, "{file}: {old}");
                text = text.replace(old, new);
            }
            (file, text)
        })
        .collect()
}
