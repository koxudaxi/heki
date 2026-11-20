//! Differential oracle for the incremental language server (docs/lsp.md): after every step of
//! an editing session over a small multi-file project, the diagnostics the server published for
//! every file must equal a fresh `heki check --output-format json` of the same files and
//! configuration.
//!
//! The session opens every file, then runs the steps of [`steps`]. A step edits and saves,
//! creates, or deletes files, or changes the configuration, the way an editor and its file
//! watcher report it (`workspace/didChangeWatchedFiles`). After it, the harness waits until the
//! server is idle ([`Lsp::settle`]: `heki/stats` reports no work in flight, and every
//! publication it counts has arrived; no sleeps) and compares, file by file, the range, code,
//! severity, message, and related locations of every diagnostic. A step also names diagnostics
//! the CLI must (or must not) report, so it exercises what it describes, and a step marked
//! `cutoff` must stop at the edited files (Early Cutoff). A new step is a few lines.

mod common;

use common::Project;
use common::lsp::{Lsp, uri};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

// ------------------------------------------------------------------------------------ project

const CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
extend-select = ["MUT108"]

[[tool.heki.write-owners]]
name = "registry"
targets = ["Registry.entries"]
allowed-writers = ["Store.*"]
unresolved = "report"

[[tool.heki.read-owners]]
name = "vault"
targets = ["Vault.secret"]
allowed-readers = ["Store.*"]

[[tool.heki.read-owners]]
name = "layer"
targets = ["Ledger.balance"]
forbidden-readers = ["pkg.layer.*"]

[[tool.heki.call-owners]]
name = "ledger"
targets = ["Ledger.post", "Receipt"]
allowed-callers = ["Store.*"]

[[tool.heki.effect-boundaries]]
name = "pure"
scopes = ["pkg.pure"]
forbid = ["reads-targets", "calls"]
read-targets = ["Vault.secret"]
call-targets = ["Ledger.post", "Receipt"]

[[tool.heki.effect-boundaries]]
name = "pass"
scopes = ["pkg.pure.tidy"]
allow-writes = ["Ledger.rows"]
write-universe = ["Ledger.*"]
[[tool.heki.readonly-returns]]
name = "catalog"
producers = ["pkg.catalog.Catalog.entry"]
deep = true

[[tool.heki.invalidation]]
name = "index"
targets = ["Node.kids"]
invalidators = ["Index.refresh"]
"#;

// An `async def` two modules above a sync helper (`EFF101` with its chain).
const LOW: &str = "import requests\n\n\ndef fetch(url):\n    return url\n";
const LOW_BLOCKING: &str = "import requests\n\n\ndef fetch(url):\n    return requests.get(url)\n";
const MID: &str = "from pkg.low import fetch\n\n\ndef helper(url):\n    return fetch(url)\n";
const MID_BROKEN: &str = "from pkg.low import fetch\n\n\ndef helper(url):\n    return fetch(url\n";
const TOP: &str =
    "from pkg.mid import helper\n\n\nasync def handler(url):\n    return helper(url)\n";

// Property getters: `MUT101` (a `@cached_property` read), `MUT107` (a property `__hash__`
// reads), `EFF101` (a getter read from an `async def` in another file).
const SHAPES: &str = "from functools import cached_property\n\n\nclass Box:\n    def __init__(self):\n        self.items = []\n        self.size = 0\n\n    @cached_property\n    def total(self):\n        return self.size\n\n    def add(self, x):\n        self.items.append(x)\n";
const SHAPES_READING: &str = "from functools import cached_property\n\n\nclass Box:\n    def __init__(self):\n        self.items = []\n        self.size = 0\n\n    @cached_property\n    def total(self):\n        return len(self.items)\n\n    def add(self, x):\n        self.items.append(x)\n";
const KEYS: &str = "class Key:\n    def __init__(self, code, name):\n        self.code = code\n        self.name = name\n\n    @property\n    def ident(self):\n        return self.code\n\n    def __hash__(self):\n        return hash(self.ident)\n\n    def rename(self, name):\n        self.name = name\n";
const KEYS_BY_NAME: &str = "class Key:\n    def __init__(self, code, name):\n        self.code = code\n        self.name = name\n\n    @property\n    def ident(self):\n        return self.name\n\n    def __hash__(self):\n        return hash(self.ident)\n\n    def rename(self, name):\n        self.name = name\n";
const REMOTE: &str =
    "import requests\n\n\nclass Client:\n    @property\n    def data(self):\n        return {}\n";
const REMOTE_BLOCKING: &str = "import requests\n\n\nclass Client:\n    @property\n    def data(self):\n        return requests.get(\"https://example.com\")\n";
const READER: &str =
    "from pkg.remote import Client\n\n\nasync def load(c: Client):\n    return c.data\n";

// Guarded effects (`EFF102`): loops in other modules call a helper with a constant argument
// and with its default; the helper gains the `if cache:` guard the constant switches off, its
// default changes, and it loses the guard again. A connection class gains, then loses, a lazy
// initialization that a loop calls on one object.
const GUARD_NET: &str = "from urllib.request import urlopen\n\n\ndef fetch(url, cache=True):\n    return urlopen(url)\n";
const GUARD_NET_GUARDED: &str = "from urllib.request import urlopen\n\n\ndef fetch(url, cache=True):\n    if cache:\n        return urlopen(url)\n    return None\n";
const GUARD_USE: &str = "from pkg.guard_net import fetch\n\n\ndef sync(urls):\n    for u in urls:\n        fetch(u, cache=False)\n";
const GUARD_DEFAULT_USE: &str =
    "from pkg.guard_net import fetch\n\n\ndef sync(urls):\n    for u in urls:\n        fetch(u)\n";
const GUARD_CONN: &str = "from urllib.request import urlopen\n\n\nclass Conn:\n    def __init__(self):\n        self._sock = None\n\n    def send(self):\n        self._sock = urlopen(\"https://example.com\")\n";
const GUARD_CONN_USE: &str = "from pkg.guard_conn import Conn\n\n\ndef run(c: Conn, xs):\n    for x in xs:\n        c.send()\n";

// Contracts: private helpers writing / reading guarded attributes report themselves without
// callers, nothing when only allowed functions call them, and at a non-allowed caller.
const MODEL: &str = "class Registry:\n    def __init__(self):\n        self.entries = []\n\n\nclass Vault:\n    def __init__(self):\n        self.secret = \"\"\n";
const OPS: &str = "from pkg.model import Registry, Vault\n\n\ndef _append(r: Registry, x):\n    r.entries.append(x)\n\n\ndef _peek(v: Vault):\n    return v.secret\n";
const STORE: &str = "from pkg.model import Registry, Vault\n\n\nclass Store:\n    def put(self, r: Registry, x):\n        return x\n";
const STORE_CALLING: &str = "from pkg.model import Registry, Vault\nfrom pkg.ops import _append, _peek\n\n\nclass Store:\n    def put(self, r: Registry, x):\n        _append(r, x)\n\n    def look(self, v: Vault):\n        return _peek(v)\n";
const SERVICE: &str =
    "from pkg.model import Registry, Vault\n\n\ndef sync(r: Registry, v: Vault):\n    return r\n";
const SERVICE_CALLING: &str = "from pkg.model import Registry, Vault\nfrom pkg.ops import _append, _peek\n\n\ndef sync(r: Registry, v: Vault):\n    _append(r, 1)\n    return _peek(v)\n";

// Contract extensions (`MUT204`, `EFF201` reads / calls / allowed writes, `forbidden-readers`):
// a private helper in another module constructs a guarded dataclass (no effect, so no summary
// changes), helpers below boundaries read, call, and write, a private helper reads for a
// forbidden layer, and writers, readers, and callers two modules below their callers become
// reachable from a non-allowed function (the allowed-context walk climbs every caller).
const LEDGER: &str = "from dataclasses import dataclass\n\n\nclass Ledger:\n    def __init__(self):\n        self.rows = []\n        self.balance = 0\n\n    def post(self, amount):\n        return amount\n\n\n@dataclass\nclass Receipt:\n    amount: int\n";
const ACCT_OPS: &str =
    "from pkg.ledger import Ledger, Receipt\n\n\ndef _record(l: Ledger):\n    return None\n";
const ACCOUNTS: &str = "from pkg.acct_ops import _record\nfrom pkg.ledger import Ledger\n\n\ndef settle(l: Ledger):\n    return _record(l)\n";
const PURE_OPS: &str = "from pkg.ledger import Ledger, Receipt\nfrom pkg.model import Vault\n\n\ndef helper(v: Vault, l: Ledger):\n    return None\n\n\ndef grow(l: Ledger):\n    l.rows.append(1)\n";
const PURE: &str = "from pkg.ledger import Ledger\nfrom pkg.model import Vault\nfrom pkg.pure_ops import grow, helper\n\n\ndef decide(v: Vault, l: Ledger):\n    return helper(v, l)\n\n\ndef tidy(l: Ledger):\n    grow(l)\n";
const LAYER_OPS: &str = "from pkg.ledger import Ledger\n\n\ndef _bal(l: Ledger):\n    return 0\n";
const LAYER_VIEW: &str = "from pkg.layer_ops import _bal\nfrom pkg.ledger import Ledger\n\n\ndef show(l: Ledger):\n    return _bal(l)\n";
const OUTSIDE_VIEW: &str = "from pkg.layer_ops import _bal\nfrom pkg.ledger import Ledger\n\n\ndef summary(l: Ledger):\n    return _bal(l)\n";
const CHAIN_LOW: &str = "from pkg.ledger import Ledger\nfrom pkg.model import Registry, Vault\n\n\ndef stamp(r: Registry):\n    r.entries.append(1)\n\n\ndef peek(v: Vault):\n    return v.secret\n\n\ndef book(l: Ledger):\n    return l.post(1)\n";
const CHAIN_MID: &str = "from pkg.chain_low import book, peek, stamp\n\n\ndef relay(r, v, l):\n    stamp(r)\n    peek(v)\n    return book(l)\n";
const CHAIN_TOP: &str = "from pkg.chain_mid import relay\n\n\nclass Store:\n    def run(self, r, v, l):\n        return relay(r, v, l)\n\n\ndef outside(r, v, l):\n    return None\n";

// Constant keys, objects under construction, and the strict mode: a `__dict__` key names the
// guarded attribute through a `Final` constant of another module; a private helper fills a new
// object its caller hands it; a helper writes a target on a parameter its caller types.
const KEY_CONSTS: &str = "from typing import Final\n\nKEY: Final = \"entries\"\n";
const KEY_WRITER: &str = "from pkg.key_consts import KEY\nfrom pkg.model import Registry\n\n\ndef mark(r: Registry):\n    r.__dict__[KEY] = []\n";
const FRESH_OPS: &str = "from pkg.model import Registry\n\nKEPT = []\n\n\ndef _fill(r: Registry):\n    r.entries.append(1)\n";
const FRESH_BUILD: &str = "from pkg.fresh_ops import _fill\nfrom pkg.model import Registry\n\n\ndef build():\n    r = Registry()\n    _fill(r)\n    return r\n";
const STRICT_OPS: &str = "def _reset(r):\n    r.entries = []\n";
const STRICT_USE: &str = "from pkg.model import Registry\nfrom pkg.strict_ops import _reset\n\n\ndef run(r: Registry):\n    _reset(r)\n";

// Files: a module a caller already imports is created, then deleted; a stub, then a package of
// the same name, is put next to a module, then removed.
const USES_EXTRA: &str = "from pkg.extra import slow\n\n\nasync def go():\n    return slow()\n";
const EXTRA: &str = "import time\n\n\ndef slow():\n    time.sleep(1)\n";
const SVC: &str =
    "import requests\n\n\ndef ping():\n    return requests.get(\"https://example.com\")\n";
const SVC_STUB: &str = "def ping() -> None: ...\n";
const SVC_PACKAGE: &str = "def ping():\n    return None\n";
const SVC_USER: &str = "from pkg.svc import ping\n\n\nasync def poll():\n    return ping()\n";

// Declarations: a module global instance stored on `self`, a return type another module's
// attribute type comes from, a class family across modules, and an override in a subclass
// module (class hierarchy analysis).
const CONF: &str = "import requests\n\nsession = requests.Session()\n";
const API: &str = "from pkg.conf import session\n\n\nclass Api:\n    def __init__(self):\n        self.http = session\n\n    async def get(self):\n        return self.http.get(\"https://example.com\")\n";
const ENGINES: &str = "import time\n\n\nclass Engine:\n    def run(self):\n        return 1\n\n\nclass Turbo:\n    def run(self):\n        time.sleep(1)\n\n\ndef make() -> Engine:\n    return Engine()\n";
const HOLDER: &str = "from pkg.engines import make\n\n\nclass Holder:\n    def __init__(self):\n        self.engine = make()\n";
const GARAGE: &str =
    "from pkg.holder import Holder\n\n\nasync def drive(h: Holder):\n    return h.engine.run()\n";
const BASE: &str = "from functools import cached_property\n\n\nclass Base:\n    def __init__(self):\n        self.items = []\n\n    @cached_property\n    def count(self):\n        return len(self.items)\n";
const BASE_CONSTANT: &str = "from functools import cached_property\n\n\nclass Base:\n    def __init__(self):\n        self.items = []\n\n    @cached_property\n    def count(self):\n        return 0\n";
const DERIVED: &str = "from pkg.base import Base\n\n\nclass Derived(Base):\n    def push(self, x):\n        self.items.append(x)\n";
// `MUT101` on a value the cache holds by reference: a cached property returns a tuple holding
// `self.items` and a mixin helper's result; a subclass in a third module changes `items` in
// place. The cache is stale once the helper reads `items`.
const MIXIN: &str = "class Mixin:\n    def _extra(self):\n        return 0\n";
const MIXIN_READING: &str = "class Mixin:\n    def _extra(self):\n        return len(self.items)\n";
const HOLDER_BASE: &str = "from functools import cached_property\n\nfrom pkg.mixin import Mixin\n\n\nclass Holding(Mixin):\n    def __init__(self):\n        self.items = []\n\n    @cached_property\n    def pair(self):\n        return (self.items, self._extra())\n";
const HOLDER_SUB: &str = "from pkg.holding import Holding\n\n\nclass Sub(Holding):\n    def push(self, x):\n        self.items.append(x)\n";
const SHAPE: &str = "class Shape:\n    def area(self):\n        return 0\n";
const SQUARE: &str = "import time\n\nfrom pkg.shape import Shape\n\n\nclass Square(Shape):\n    def side(self):\n        time.sleep(1)\n";
const DRAW: &str =
    "from pkg.shape import Shape\n\n\nasync def render(s: Shape):\n    return s.area()\n";

// A package re-exports classes of its submodule (`from pkg.ui.impl import ...` in
// `pkg/ui/__init__.py`); another module uses them through the package only. A method becomes a
// property getter (read from an `async def`, read by `__hash__`), and a class gains a base.
const UI_INIT: &str = "from pkg.ui.impl import Labeled, Widget\n";
const UI_BASE: &str =
    "import time\n\n\nclass Refreshing:\n    def refresh(self):\n        time.sleep(1)\n";
const UI_IMPL: &str = "import requests\n\nfrom pkg.ui.base import Refreshing\n\n\nclass Widget:\n    def data(self):\n        return requests.get(\"https://example.com\")\n\n\nclass Labeled:\n    def __init__(self, name):\n        self.name = name\n\n    def label(self):\n        return self.name\n";
const CONSUMER: &str = "from pkg.ui import Labeled, Widget\n\n\nasync def show(w: Widget):\n    return w.data\n\n\nasync def update(w: Widget):\n    return w.refresh()\n\n\nclass Tag(Labeled):\n    def __hash__(self):\n        return hash(self.label)\n\n    def rename(self, name):\n        self.name = name\n";

// `MUT108`: a dataclass whose field a helper in another module changes in place becomes frozen,
// and a decorator in a third module becomes a `dataclass_transform(frozen_default=True)`.
const RECORDS: &str = "from typing import dataclass_transform\n\n\n@dataclass_transform()\ndef record(cls):\n    return cls\n";
const FROZEN_MODEL: &str = "from dataclasses import dataclass, field\n\nfrom pkg.frozen.records import record\n\n\n@dataclass\nclass Config:\n    tags: list[str] = field(default_factory=list)\n\n\n@record\nclass Entry:\n    names: list[str]\n";
const FROZEN_HELPERS: &str = "def add_default(tags):\n    tags.append(\"default\")\n\n\ndef clear_names(entry):\n    entry.names.clear()\n";
const CONFIGURE: &str = "from pkg.frozen.helpers import add_default, clear_names\nfrom pkg.frozen.model import Config, Entry\n\n\ndef configure(config: Config) -> None:\n    add_default(config.tags)\n\n\ndef reset(entry: Entry) -> None:\n    clear_names(entry)\n";

// Where a body runs: a helper an `async def` in another module awaits gains an offloading
// decorator (`@sync_to_async`, `FunctionDecl::offloaded_by`), and a helper whose result an
// `async def` iterates turns into a generator (`EffectSummary::is_generator`): the report moves
// from the call to the iteration.
const OFFLOAD: &str = "import requests\n\n\ndef fetch_profile():\n    return requests.post(\"https://example.com\")\n";
const OFFLOAD_DECORATED: &str = "import requests\nfrom asgiref.sync import sync_to_async\n\n\n@sync_to_async\ndef fetch_profile():\n    return requests.post(\"https://example.com\")\n";
const VIEW: &str = "from pkg.offload import fetch_profile\n\n\nasync def view():\n    return await fetch_profile()\n";
const FEED: &str =
    "import httpx\n\n\ndef rows():\n    return [httpx.get(\"https://example.com\")]\n";
const FEED_GENERATOR: &str =
    "import httpx\n\n\ndef rows():\n    yield httpx.get(\"https://example.com\")\n";
const FEED_USER: &str = "from pkg.feed import rows\n\n\nasync def stream():\n    pending = rows()\n    for row in pending:\n        print(row)\n";
// A private generator writing a guarded attribute, handed to a helper in another module: its
// write runs where the helper iterates it, and it reports itself once the helper stops.
const FILLS: &str = "from pkg.model import Registry\n\n\ndef _fill(r: Registry):\n    r.entries.append(1)\n    yield r\n";
const DRAIN: &str = "def drain(g):\n    for _ in g:\n        pass\n";
const DRAIN_RETURNING: &str = "def drain(g):\n    return g\n";
const HANDOFF: &str = "from pkg.drain import drain\nfrom pkg.fills import _fill\nfrom pkg.model import Registry\n\n\ndef hand(r: Registry):\n    drain(_fill(r))\n";
// `MUT202`: a wrapper in another module starts returning a declared producer's value (or an
// `@lru_cache` result), and stops again. `MUT109`: a helper in another module stops announcing a
// change its caller makes, and a caller in another module stops announcing after calling a
// public writer.
const CATALOG: &str = "from functools import lru_cache\n\n\nclass Catalog:\n    def entry(self, key) -> dict:\n        return {}\n\n    def fresh(self, key) -> dict:\n        return {}\n\n\n@lru_cache\ndef defaults() -> dict:\n    return {}\n";
const LOOKUP: &str = "from pkg.catalog import Catalog, defaults\n\n\ndef find(c: Catalog, key) -> dict:\n    return c.fresh(key)\n";
const LOOKUP_SHARED: &str = "from pkg.catalog import Catalog, defaults\n\n\ndef find(c: Catalog, key) -> dict:\n    return c.entry(key)\n";
const LOOKUP_CACHED: &str = "from pkg.catalog import Catalog, defaults\n\n\ndef find(c: Catalog, key) -> dict:\n    return defaults()\n";
const TAGGER: &str = "from pkg.catalog import Catalog\nfrom pkg.lookup import find\n\n\ndef tag(c: Catalog):\n    find(c, \"k\")[\"tag\"] = 1\n";
const NODES: &str = "class Node:\n    def __init__(self):\n        self.kids = []\n\n\nclass Index:\n    def refresh(self):\n        self.stale = False\n\n\ndef attach(node: Node, kid):\n    node.kids.append(kid)\n";
const ANNOUNCE: &str =
    "from pkg.nodes import Index\n\n\ndef announce(index: Index):\n    index.refresh()\n";
const ANNOUNCE_SILENT: &str =
    "from pkg.nodes import Index\n\n\ndef announce(index: Index):\n    return index\n";
const GROW: &str = "from pkg.announce import announce\nfrom pkg.nodes import Index, Node, attach\n\n\ndef grow(index: Index, node: Node, kid):\n    node.kids.append(kid)\n    announce(index)\n\n\ndef adopt(index: Index, node: Node, kid):\n    attach(node, kid)\n    index.refresh()\n";
const GROW_SILENT_ADOPT: &str = "from pkg.announce import announce\nfrom pkg.nodes import Index, Node, attach\n\n\ndef grow(index: Index, node: Node, kid):\n    node.kids.append(kid)\n    announce(index)\n\n\ndef adopt(index: Index, node: Node, kid):\n    attach(node, kid)\n";
// A public writer two calls below the function that announces its change, each in its own module.
const GRAFT: &str =
    "from pkg.nodes import Node\n\n\ndef graft(node: Node, kid):\n    node.kids.append(kid)\n";
const RELAY: &str = "from pkg.graft import graft\nfrom pkg.nodes import Node\n\n\ndef forward(node: Node, kid):\n    graft(node, kid)\n";
const PLANTER: &str = "from pkg.nodes import Index, Node\nfrom pkg.relay import forward\n\n\ndef plant(index: Index, node: Node, kid):\n    forward(node, kid)\n    index.refresh()\n";
// A private helper of another module becomes a generator: its write moves from its call, before
// the invalidator, to the iteration after it.
const KID_GEN: &str = "from pkg.nodes import Node\n\n\ndef _grow_kids(node: Node, kid):\n    node.kids.append(kid)\n    return [kid]\n";
const KID_GEN_GENERATOR: &str = "from pkg.nodes import Node\n\n\ndef _grow_kids(node: Node, kid):\n    node.kids.append(kid)\n    yield kid\n";
const KID_USER: &str = "from pkg.kid_gen import _grow_kids\nfrom pkg.nodes import Index, Node\n\n\ndef plant_all(index: Index, node: Node, kid):\n    pending = _grow_kids(node, kid)\n    index.refresh()\n    for _ in pending:\n        pass\n";
const PLANTER_SILENT: &str = "from pkg.nodes import Index, Node\nfrom pkg.relay import forward\n\n\ndef plant(index: Index, node: Node, kid):\n    forward(node, kid)\n";

// Assumption A1(b): one module patching `builtins` makes no module trust builtin names, so the
// size-dependent rewrites that rely on them (`OPT110` builds a `set(...)`) are not proven.
const LOOPS: &str = "def member(n):\n    allowed = list(range(0, 50, 3))\n    hits = []\n    for p in range(n):\n        if p in allowed:\n            hits.append(p)\n    return hits\n";
const COMPAT: &str = "import builtins\n\nbuiltins.len = len\n";
// A stub never runs: its `import builtins` patches nothing.
const SPELLED_STUB: &str = "import builtins\n\n\ndef name(x: builtins.str) -> None: ...\n";

// Small displays other files read (`OPT112`): a module-level list another module scans in a
// loop, and an instance attribute whose class family (a subclass in a third module) may grow it.
const LEVELS: &str = "LEVELS = [\"low\", \"mid\", \"high\"]\n";
const LEVELS_GROWN: &str =
    "LEVELS = [\"low\", \"mid\", \"high\"]\n\n\ndef add(level):\n    LEVELS.append(level)\n";
const LEVELS_USER: &str = "from pkg.levels import LEVELS\n\n\ndef ranks(xs):\n    out = []\n    for x in xs:\n        out.append(LEVELS.index(x))\n    return out\n";
const SNIFF: &str =
    "class Sniffer:\n    def __init__(self):\n        self.preferred = [\",\", \";\"]\n";
const SNIFF_MORE: &str = "from pkg.sniff import Sniffer\n\n\nclass Wider(Sniffer):\n    pass\n";
const SNIFF_MORE_GROWING: &str = "from pkg.sniff import Sniffer\n\n\nclass Wider(Sniffer):\n    def widen(self, d):\n        self.preferred.append(d)\n";
const SNIFF_USER: &str = "from pkg.sniff import Sniffer\n\n\ndef guess(s: Sniffer, rows):\n    for row in rows:\n        s.preferred.index(row)\n";

// `MUT110` (default on): read-only parameters handed to helpers in other modules. Helpers start
// and stop changing their parameter in place (`append`, item assignment, `sort`, `reverse`), one
// call below the caller and two (a chain over three modules, whose end moves between functions),
// and a package re-exports a helper.
const RO_HELPERS: &str =
    "def push(items, x):\n    return x\n\n\ndef put(table, key):\n    return key\n";
const RO_HELPERS_MUTATING: &str =
    "def push(items, x):\n    items.append(x)\n\n\ndef put(table, key):\n    table[key] = 1\n";
const RO_CALLERS: &str = "from collections.abc import Mapping, Sequence\n\nfrom pkg.ro.helpers import push, put\n\n\ndef add(items: Sequence[int]) -> None:\n    push(items, 1)\n\n\ndef store(table: Mapping[str, int]) -> None:\n    put(table, \"k\")\n";
const RO_LOW: &str = "def sort_values(values):\n    return sorted(values)\n\n\ndef _reset(values):\n    values[0] = 0\n";
const RO_MID: &str =
    "from pkg.ro.low import sort_values\n\n\ndef relay(values):\n    sort_values(values)\n";
const RO_STATS: &str = "from collections.abc import Sequence\n\nfrom pkg.ro.mid import relay\n\n\ndef median(values: Sequence[float]) -> float:\n    relay(values)\n    return values[len(values) // 2]\n";
const RO_INIT: &str = "from pkg.ro.impl import scale\n";
const RO_IMPL: &str = "def scale(values):\n    return [v * 2 for v in values]\n\n\ndef scale_in_place(values):\n    values[0] = 0\n";
const RO_PLOT: &str = "from collections.abc import Sequence\n\nfrom pkg.ro import scale\n\n\ndef plot(values: Sequence[float]) -> None:\n    scale(values)\n";
// Receiver typing: the class of a loop target comes from the element type of an attribute
// annotation in another module.
const SHELF: &str = r#"from typing import Self

from pkg.model import Registry


class Shelf:
    registries: list[Registry]

    @property
    def main(self) -> Registry:
        return self.registries[0]

    def again(self) -> Self:
        return self
"#;
const FILL: &str = "from pkg.shelf import Shelf\n\n\ndef fill(shelf: Shelf, x):\n    for r in shelf.registries:\n        r.entries.append(x)\n";
const FILL_MAIN: &str = "from pkg.shelf import Shelf\n\n\ndef fill_main(shelf: Shelf, x):\n    shelf.again().main.entries.append(x)\n";
// A protocol in one module, the class implementing it structurally in another.
const VIEWS: &str = "from typing import Protocol\n\n\nclass EntriesView(Protocol):\n    entries: list\n    tag: str\n\n\ndef fill_view(view: EntriesView, x):\n    view.entries.append(x)\n";
// The class body only declares names (no function changes): only the protocol's
// implementations change.
const TAGGED: &str =
    "from pkg.model import Registry\n\n\nclass TaggedRegistry(Registry):\n    tag: str = \"t\"\n";
const UNTAGGED: &str =
    "from pkg.model import Registry\n\n\nclass TaggedRegistry(Registry):\n    label: str = \"t\"\n";

/// The project before the first step (every file is opened).
fn initial_files() -> Vec<(&'static str, String)> {
    [
        ("pkg/__init__.py", ""),
        ("pkg/low.py", LOW),
        ("pkg/mid.py", MID),
        ("pkg/top.py", TOP),
        ("pkg/shapes.py", SHAPES),
        ("pkg/keys.py", KEYS),
        ("pkg/remote.py", REMOTE),
        ("pkg/reader.py", READER),
        ("pkg/guard_net.py", GUARD_NET),
        ("pkg/guard_use.py", GUARD_USE),
        ("pkg/guard_default_use.py", GUARD_DEFAULT_USE),
        ("pkg/guard_conn.py", GUARD_CONN),
        ("pkg/guard_conn_use.py", GUARD_CONN_USE),
        ("pkg/model.py", MODEL),
        ("pkg/ops.py", OPS),
        ("pkg/store.py", STORE),
        ("pkg/service.py", SERVICE),
        ("pkg/uses_extra.py", USES_EXTRA),
        ("pkg/svc.py", SVC),
        ("pkg/svc_user.py", SVC_USER),
        ("pkg/conf.py", CONF),
        ("pkg/api.py", API),
        ("pkg/engines.py", ENGINES),
        ("pkg/holder.py", HOLDER),
        ("pkg/garage.py", GARAGE),
        ("pkg/base.py", BASE),
        ("pkg/derived.py", DERIVED),
        ("pkg/mixin.py", MIXIN),
        ("pkg/holding.py", HOLDER_BASE),
        ("pkg/sub.py", HOLDER_SUB),
        ("pkg/shape.py", SHAPE),
        ("pkg/square.py", SQUARE),
        ("pkg/draw.py", DRAW),
        ("pkg/ui/__init__.py", UI_INIT),
        ("pkg/ui/base.py", UI_BASE),
        ("pkg/ui/impl.py", UI_IMPL),
        ("pkg/consumer.py", CONSUMER),
        ("pkg/frozen/__init__.py", ""),
        ("pkg/frozen/records.py", RECORDS),
        ("pkg/frozen/model.py", FROZEN_MODEL),
        ("pkg/frozen/helpers.py", FROZEN_HELPERS),
        ("pkg/frozen/configure.py", CONFIGURE),
        ("pkg/ro/__init__.py", RO_INIT),
        ("pkg/ro/helpers.py", RO_HELPERS),
        ("pkg/ro/callers.py", RO_CALLERS),
        ("pkg/ro/low.py", RO_LOW),
        ("pkg/ro/mid.py", RO_MID),
        ("pkg/ro/stats.py", RO_STATS),
        ("pkg/ro/impl.py", RO_IMPL),
        ("pkg/plot.py", RO_PLOT),
        ("pkg/loops.py", LOOPS),
        ("pkg/compat.py", COMPAT),
        ("pkg/ledger.py", LEDGER),
        ("pkg/acct_ops.py", ACCT_OPS),
        ("pkg/accounts.py", ACCOUNTS),
        ("pkg/pure_ops.py", PURE_OPS),
        ("pkg/pure.py", PURE),
        ("pkg/layer_ops.py", LAYER_OPS),
        ("pkg/layer/__init__.py", ""),
        ("pkg/layer/view.py", LAYER_VIEW),
        ("pkg/outside_view.py", OUTSIDE_VIEW),
        ("pkg/chain_low.py", CHAIN_LOW),
        ("pkg/chain_mid.py", CHAIN_MID),
        ("pkg/chain_top.py", CHAIN_TOP),
        ("pkg/offload.py", OFFLOAD),
        ("pkg/view.py", VIEW),
        ("pkg/feed.py", FEED),
        ("pkg/feed_user.py", FEED_USER),
        ("pkg/fills.py", FILLS),
        ("pkg/drain.py", DRAIN),
        ("pkg/handoff.py", HANDOFF),
        ("pkg/levels.py", LEVELS),
        ("pkg/levels_user.py", LEVELS_USER),
        ("pkg/sniff.py", SNIFF),
        ("pkg/sniff_more.py", SNIFF_MORE),
        ("pkg/sniff_user.py", SNIFF_USER),
        ("pkg/catalog.py", CATALOG),
        ("pkg/lookup.py", LOOKUP),
        ("pkg/tagger.py", TAGGER),
        ("pkg/nodes.py", NODES),
        ("pkg/announce.py", ANNOUNCE),
        ("pkg/grow.py", GROW),
        ("pkg/graft.py", GRAFT),
        ("pkg/relay.py", RELAY),
        ("pkg/planter.py", PLANTER),
        ("pkg/kid_gen.py", KID_GEN),
        ("pkg/kid_user.py", KID_USER),
        ("pkg/shelf.py", SHELF),
        ("pkg/fill.py", FILL),
        ("pkg/fill_main.py", FILL_MAIN),
        ("pkg/views.py", VIEWS),
        ("pkg/tagged.py", TAGGED),
        ("pkg/key_consts.py", KEY_CONSTS),
        ("pkg/key_writer.py", KEY_WRITER),
        ("pkg/fresh_ops.py", FRESH_OPS),
        ("pkg/fresh_build.py", FRESH_BUILD),
        ("pkg/strict_ops.py", STRICT_OPS),
        ("pkg/strict_use.py", STRICT_USE),
    ]
    .into_iter()
    .map(|(rel, text)| (rel, text.to_owned()))
    .collect()
}

// -------------------------------------------------------------------------------------- steps

/// What an editor does to the project.
enum Action {
    /// Edit an open document and save it: `didChange`, write the file, `didSave` with the text.
    Save(&'static str, String),
    /// An unsaved edit of an open document (`didChange` only); a later `Save` writes the disk.
    Change(&'static str, String),
    /// A new file: written, announced with `workspace/didChangeWatchedFiles`, then opened.
    Create(&'static str, String),
    /// Close the document, delete the file, and announce the deletion.
    Delete(&'static str),
    /// Replace the configuration (`pyproject.toml`) and announce the change.
    Configure(String),
}

struct Step {
    name: &'static str,
    actions: Vec<Action>,
    /// `(file, code)` the CLI reports after the step.
    present: &'static [(&'static str, &'static str)],
    /// `(file, code)` the CLI does not report after the step.
    absent: &'static [(&'static str, &'static str)],
    /// Early Cutoff: no other file is re-analyzed or re-published.
    cutoff: bool,
}

impl Step {
    fn new(name: &'static str, actions: Vec<Action>) -> Self {
        Self {
            name,
            actions,
            present: &[],
            absent: &[],
            cutoff: false,
        }
    }

    fn present(mut self, codes: &'static [(&'static str, &'static str)]) -> Self {
        self.present = codes;
        self
    }

    fn absent(mut self, codes: &'static [(&'static str, &'static str)]) -> Self {
        self.absent = codes;
        self
    }

    fn cutoff(mut self) -> Self {
        self.cutoff = true;
        self
    }
}

fn save(rel: &'static str, text: &str) -> Action {
    Action::Save(rel, text.to_owned())
}

/// Step 4: `MUT108` (opt-in, enabled in the project's configuration). The frozen-ness of a class
/// comes from a decorator in another module, and the in-place change from a helper in a third.
/// The helper then moves the change of the field to a function of its own and back while its
/// summary stays the same: the call chain of the report ends at the first function whose body
/// changes the field (`heki_rules::ChainFacts`, as in [`steps_mut110`], for a slot of the
/// argument).
fn steps_mut108() -> Vec<Step> {
    let transform = RECORDS.replace(
        "@dataclass_transform()",
        "@dataclass_transform(frozen_default=True)",
    );
    let frozen = FROZEN_MODEL.replace("@dataclass\n", "@dataclass(frozen=True)\n");
    let trimming = |body: &str| {
        format!(
            "{}\n\ndef _trim(entry):\n    entry.names[1:] = []\n",
            FROZEN_HELPERS.replace("    entry.names.clear()\n", body)
        )
    };
    vec![
        Step::new(
            "a decorator in another module becomes a frozen dataclass transform",
            vec![Action::Save("pkg/frozen/records.py", transform)],
        )
        .present(&[("pkg/frozen/configure.py", "MUT108")]),
        Step::new(
            "the helper also changes the field through a function of its own",
            vec![Action::Save(
                "pkg/frozen/helpers.py",
                trimming("    _trim(entry)\n    entry.names[0] = \"\"\n"),
            )],
        )
        .present(&[("pkg/frozen/configure.py", "MUT108")]),
        Step::new(
            "the helper changes the field only through that function: a second hop",
            vec![Action::Save(
                "pkg/frozen/helpers.py",
                trimming("    _trim(entry)\n"),
            )],
        )
        .present(&[("pkg/frozen/configure.py", "MUT108")]),
        Step::new(
            "the helper clears the field itself again",
            vec![save("pkg/frozen/helpers.py", FROZEN_HELPERS)],
        )
        .present(&[("pkg/frozen/configure.py", "MUT108")]),
        Step::new(
            "the transform is not frozen by default again",
            vec![save("pkg/frozen/records.py", RECORDS)],
        )
        .absent(&[("pkg/frozen/configure.py", "MUT108")]),
        Step::new(
            "a dataclass a helper in another module changes in place becomes frozen",
            vec![Action::Save("pkg/frozen/model.py", frozen)],
        )
        .present(&[("pkg/frozen/configure.py", "MUT108")]),
        Step::new(
            "the dataclass is not frozen again",
            vec![save("pkg/frozen/model.py", FROZEN_MODEL)],
        )
        .absent(&[("pkg/frozen/configure.py", "MUT108")]),
    ]
}

/// Receiver typing: the element types of declarations in another module type the receivers of
/// a function, so editing those declarations re-checks it.
fn steps_receiver_typing() -> Vec<Step> {
    vec![
        Step::new(
            "the attribute holds strings instead of registries",
            vec![save(
                "pkg/shelf.py",
                &SHELF.replace("list[Registry]", "list[str]"),
            )],
        )
        .absent(&[("pkg/fill.py", "MUT201")]),
        Step::new(
            "the attribute holds registries again",
            vec![save("pkg/shelf.py", SHELF)],
        )
        .present(&[("pkg/fill.py", "MUT201")]),
        Step::new(
            "the property returns a string",
            vec![save(
                "pkg/shelf.py",
                &SHELF.replace(") -> Registry:", ") -> str:"),
            )],
        )
        .absent(&[("pkg/fill_main.py", "MUT201")]),
        Step::new(
            "the property returns a registry again",
            vec![save("pkg/shelf.py", SHELF)],
        )
        .present(&[("pkg/fill_main.py", "MUT201")]),
        Step::new(
            "the method returns an int instead of Self",
            vec![save("pkg/shelf.py", &SHELF.replace("-> Self:", "-> int:"))],
        )
        .absent(&[("pkg/fill_main.py", "MUT201")]),
        Step::new(
            "the method returns Self again",
            vec![save("pkg/shelf.py", SHELF)],
        )
        .present(&[("pkg/fill_main.py", "MUT201")]),
        Step::new(
            "the class stops implementing the protocol",
            vec![save("pkg/tagged.py", UNTAGGED)],
        )
        .absent(&[("pkg/views.py", "MUT201")]),
        Step::new(
            "the class implements the protocol again",
            vec![save("pkg/tagged.py", TAGGED)],
        )
        .present(&[("pkg/views.py", "MUT201")]),
    ]
}

/// Step 5b: the contract extensions. A call of a function without effects (constructing a
/// dataclass) changes no summary, so the reports of the private helper's callers and of the
/// boundary above follow the call facts; the allowed-context walk reads the calls of callers two
/// modules up.
fn steps_contract_extensions() -> Vec<Step> {
    let constructing = ACCT_OPS.replace("    return None\n", "    return Receipt(1)\n");
    let helper = |body: &str| PURE_OPS.replace("    return None\n", body);
    let two_reads = LAYER_OPS.replace("    return 0\n", "    return l.balance + l.balance\n");
    // The function exists before: only its calls change, not the declarations.
    let outside = CHAIN_TOP.replace(
        "def outside(r, v, l):\n    return None\n",
        "def outside(r, v, l):\n    return relay(r, v, l)\n",
    );
    vec![
        Step::new(
            "a private helper in another module constructs a guarded dataclass",
            vec![Action::Save("pkg/acct_ops.py", constructing)],
        )
        .present(&[("pkg/accounts.py", "MUT204")]),
        Step::new(
            "the helper stops constructing it",
            vec![save("pkg/acct_ops.py", ACCT_OPS)],
        )
        .absent(&[("pkg/accounts.py", "MUT204")]),
        Step::new(
            "a helper below a boundary reads a guarded attribute",
            vec![Action::Save(
                "pkg/pure_ops.py",
                helper("    return v.secret\n"),
            )],
        )
        .present(&[("pkg/pure.py", "EFF201")]),
        Step::new(
            "the helper calls a guarded method instead",
            vec![Action::Save(
                "pkg/pure_ops.py",
                helper("    return l.post(1)\n"),
            )],
        )
        .present(&[("pkg/pure.py", "EFF201"), ("pkg/pure_ops.py", "MUT204")]),
        Step::new(
            "the helper constructs a guarded dataclass instead",
            vec![Action::Save(
                "pkg/pure_ops.py",
                helper("    return Receipt(1)\n"),
            )],
        )
        .present(&[("pkg/pure.py", "EFF201"), ("pkg/pure_ops.py", "MUT204")]),
        Step::new(
            "a pass helper also writes an attribute the pass does not allow",
            vec![Action::Save(
                "pkg/pure_ops.py",
                PURE_OPS.replace(
                    "    l.rows.append(1)\n",
                    "    l.rows.append(1)\n    l.balance = 0\n",
                ),
            )],
        )
        .present(&[("pkg/pure.py", "EFF201")]),
        Step::new(
            "the helpers are pure again",
            vec![save("pkg/pure_ops.py", PURE_OPS)],
        )
        .absent(&[("pkg/pure.py", "EFF201")]),
        Step::new(
            "a private helper of a forbidden layer starts reading",
            vec![Action::Save(
                "pkg/layer_ops.py",
                LAYER_OPS.replace("    return 0\n", "    return l.balance\n"),
            )],
        )
        .present(&[("pkg/layer/view.py", "MUT203")])
        .absent(&[("pkg/outside_view.py", "MUT203")]),
        Step::new(
            "a second read inside the helper: still one report",
            vec![Action::Save("pkg/layer_ops.py", two_reads)],
        )
        .present(&[("pkg/layer/view.py", "MUT203")]),
        Step::new(
            "the helper stops reading",
            vec![save("pkg/layer_ops.py", LAYER_OPS)],
        )
        .absent(&[("pkg/layer/view.py", "MUT203")]),
        Step::new(
            "a function two modules above the writers that is no allowed caller calls them",
            vec![Action::Save("pkg/chain_top.py", outside)],
        )
        .present(&[
            ("pkg/chain_low.py", "MUT201"),
            ("pkg/chain_low.py", "MUT203"),
            ("pkg/chain_low.py", "MUT204"),
        ]),
        Step::new(
            "only the allowed caller is left two modules above",
            vec![save("pkg/chain_top.py", CHAIN_TOP)],
        )
        .absent(&[
            ("pkg/chain_low.py", "MUT201"),
            ("pkg/chain_low.py", "MUT203"),
            ("pkg/chain_low.py", "MUT204"),
        ]),
    ]
}

/// Where a body runs (`EFF101`): an offloading decorator added to and removed from a function
/// another module awaits, and a helper turned into a generator and back.
fn steps_body_runs() -> Vec<Step> {
    vec![
        Step::new(
            "a function another module awaits gains an offloading decorator",
            vec![save("pkg/offload.py", OFFLOAD_DECORATED)],
        )
        .absent(&[("pkg/view.py", "EFF101")]),
        Step::new(
            "the offloading decorator is removed",
            vec![save("pkg/offload.py", OFFLOAD)],
        )
        .present(&[("pkg/view.py", "EFF101")]),
        Step::new(
            "a helper becomes a generator: the report moves to the iteration",
            vec![save("pkg/feed.py", FEED_GENERATOR)],
        )
        .present(&[("pkg/feed_user.py", "EFF101")]),
        Step::new(
            "the generator becomes a helper again",
            vec![save("pkg/feed.py", FEED)],
        )
        .present(&[("pkg/feed_user.py", "EFF101")]),
        Step::new(
            "the helper a private generator is handed to stops iterating it",
            vec![save("pkg/drain.py", DRAIN_RETURNING)],
        )
        .present(&[("pkg/fills.py", "MUT201")])
        .absent(&[("pkg/handoff.py", "MUT201")]),
        Step::new(
            "the helper iterates it again",
            vec![save("pkg/drain.py", DRAIN)],
        )
        .present(&[("pkg/handoff.py", "MUT201")])
        .absent(&[("pkg/fills.py", "MUT201")]),
    ]
}

/// `OPT112`'s small displays in other files, read through the declarations
/// (`ModuleDecls::small_globals`, `ClassDecl::attr_displays`): the user of a module-level list
/// is re-checked when another function starts growing the list, and the user of an attribute
/// when a subclass in a third module starts growing it.
fn steps_small_displays() -> Vec<Step> {
    vec![
        Step::new(
            "a module-level list another file scans in a loop starts growing",
            vec![save("pkg/levels.py", LEVELS_GROWN)],
        )
        .present(&[("pkg/levels_user.py", "OPT112")]),
        Step::new(
            "the module-level list is a small display again",
            vec![save("pkg/levels.py", LEVELS)],
        )
        .absent(&[("pkg/levels_user.py", "OPT112")]),
        Step::new(
            "a subclass in another module grows an attribute another file scans",
            vec![save("pkg/sniff_more.py", SNIFF_MORE_GROWING)],
        )
        .present(&[("pkg/sniff_user.py", "OPT112")]),
        Step::new(
            "the subclass stops growing it",
            vec![save("pkg/sniff_more.py", SNIFF_MORE)],
        )
        .absent(&[("pkg/sniff_user.py", "OPT112")]),
    ]
}

/// Constant keys, objects under construction, and the strict mode. A `Final` key
/// constant is a declaration another module's summary resolves names through; whether a helper
/// keeps the object its caller hands it is a summary fact of the helper; whether a parameter is
/// typed is a fact of the callers the strict mode walks.
fn steps_keys_fresh_strict() -> Vec<Step> {
    let storing = FRESH_OPS.replace(
        "    r.entries.append(1)\n",
        "    KEPT.append(r)\n    r.entries.append(1)\n",
    );
    vec![
        Step::new(
            "the `Final` key constant of another module names another attribute",
            vec![Action::Save(
                "pkg/key_consts.py",
                KEY_CONSTS.replace("\"entries\"", "\"other\""),
            )],
        )
        .absent(&[("pkg/key_writer.py", "MUT201")]),
        Step::new(
            "the constant names the guarded attribute again",
            vec![save("pkg/key_consts.py", KEY_CONSTS)],
        )
        .present(&[("pkg/key_writer.py", "MUT201")]),
        Step::new(
            "a helper starts storing the fresh object before writing it",
            vec![Action::Save("pkg/fresh_ops.py", storing)],
        )
        .present(&[("pkg/fresh_build.py", "MUT201")]),
        Step::new(
            "the helper only fills the fresh object again",
            vec![save("pkg/fresh_ops.py", FRESH_OPS)],
        )
        .absent(&[("pkg/fresh_build.py", "MUT201")]),
        Step::new(
            "the caller stops typing the object it hands the helper",
            vec![Action::Save(
                "pkg/strict_use.py",
                STRICT_USE.replace("def run(r: Registry):", "def run(r):"),
            )],
        )
        .present(&[("pkg/strict_ops.py", "MUT205")])
        .absent(&[("pkg/strict_use.py", "MUT201")]),
        Step::new(
            "the caller types it again",
            vec![save("pkg/strict_use.py", STRICT_USE)],
        )
        .present(&[("pkg/strict_use.py", "MUT201")])
        .absent(&[("pkg/strict_ops.py", "MUT205")]),
    ]
}

/// `MUT110` (default on): a `Sequence` / `Mapping` parameter handed to a helper in another
/// module that changes it in place. What the helper changes is in its summary, how its
/// parameter is annotated in its declarations (`FunctionDecl::readonly_params` /
/// `mutable_params`), and the call chain of a report walks the own summaries and calls of the
/// functions in other modules down to the first one whose body makes the change
/// (`heki_rules::ChainFacts`): a helper that moves the change to or from a function it calls
/// keeps its summary but changes the chain. The callers' annotations change in their own file
/// only (Early Cutoff); a helper is also reached through a package re-export.
fn steps_mut110() -> Vec<Step> {
    let callers_mutable = RO_CALLERS
        .replace("items: Sequence[int]", "items: list[int]")
        .replace("table: Mapping[str, int]", "table: dict[str, int]");
    let helpers_mutable = RO_HELPERS_MUTATING
        .replace(
            "def push(items, x):",
            "def push(items: list[int], x: int) -> None:",
        )
        .replace(
            "def put(table, key):",
            "def put(table: dict[str, int], key: str) -> None:",
        );
    let low = |body: &str| RO_LOW.replace("    return sorted(values)\n", body);
    let low_resetting = low("    _reset(values)\n    values[-1] = 0\n");
    let mid_readonly = format!(
        "from collections.abc import Sequence\n\n{}",
        RO_MID.replace("def relay(values):", "def relay(values: Sequence[float]):")
    );
    let mid_commented = format!("# relays\n{RO_MID}");
    let impl_changing = RO_IMPL.replace(
        "    return [v * 2 for v in values]\n",
        "    values.reverse()\n",
    );
    vec![
        Step::new(
            "a helper in another module starts changing its parameters in place",
            vec![save("pkg/ro/helpers.py", RO_HELPERS_MUTATING)],
        )
        .present(&[("pkg/ro/callers.py", "MUT110")]),
        Step::new(
            "the callers' parameters are annotated mutable",
            vec![Action::Save("pkg/ro/callers.py", callers_mutable)],
        )
        .absent(&[("pkg/ro/callers.py", "MUT110")])
        .cutoff(),
        Step::new(
            "the callers' parameters are read-only again",
            vec![save("pkg/ro/callers.py", RO_CALLERS)],
        )
        .present(&[("pkg/ro/callers.py", "MUT110")])
        .cutoff(),
        Step::new(
            "the helper's parameters are annotated mutable: the type checker reports the arguments",
            vec![Action::Save("pkg/ro/helpers.py", helpers_mutable)],
        )
        .absent(&[("pkg/ro/callers.py", "MUT110")]),
        Step::new(
            "the helper's parameters are unannotated again",
            vec![save("pkg/ro/helpers.py", RO_HELPERS_MUTATING)],
        )
        .present(&[("pkg/ro/callers.py", "MUT110")]),
        Step::new(
            "the helper stops changing its parameters",
            vec![save("pkg/ro/helpers.py", RO_HELPERS)],
        )
        .absent(&[("pkg/ro/callers.py", "MUT110")]),
        Step::new(
            "a helper two modules below the caller starts sorting its parameter in place",
            vec![Action::Save("pkg/ro/low.py", low("    values.sort()\n"))],
        )
        .present(&[("pkg/ro/stats.py", "MUT110")]),
        Step::new(
            "a comment above the call in the middle module moves the chain's second hop",
            vec![Action::Save("pkg/ro/mid.py", mid_commented.clone())],
        )
        .present(&[("pkg/ro/stats.py", "MUT110")]),
        Step::new(
            "the middle helper also changes the parameter itself: the chain ends there",
            vec![save(
                "pkg/ro/mid.py",
                &format!("{mid_commented}    values[0] = 0\n"),
            )],
        )
        .present(&[("pkg/ro/stats.py", "MUT110")]),
        Step::new(
            "the middle helper only hands the parameter on again",
            vec![Action::Save("pkg/ro/mid.py", mid_commented)],
        )
        .present(&[("pkg/ro/stats.py", "MUT110")]),
        Step::new(
            "the helper two modules below also hands the parameter to a function of its own",
            vec![Action::Save("pkg/ro/low.py", low_resetting.clone())],
        )
        .present(&[("pkg/ro/stats.py", "MUT110")]),
        Step::new(
            "the helper two modules below changes it only through that function: a third hop",
            vec![Action::Save("pkg/ro/low.py", low("    _reset(values)\n"))],
        )
        .present(&[("pkg/ro/stats.py", "MUT110")]),
        Step::new(
            "the helper two modules below changes the parameter itself again",
            vec![Action::Save("pkg/ro/low.py", low_resetting.clone())],
        )
        .present(&[("pkg/ro/stats.py", "MUT110")]),
        Step::new(
            "a comment above the helper two modules below, whose positions no chain shows",
            vec![save(
                "pkg/ro/low.py",
                &format!("# helpers\n{low_resetting}"),
            )],
        )
        .present(&[("pkg/ro/stats.py", "MUT110")])
        .cutoff(),
        Step::new(
            "the middle helper's parameter is annotated read-only: the report moves there",
            vec![Action::Save("pkg/ro/mid.py", mid_readonly)],
        )
        .present(&[("pkg/ro/mid.py", "MUT110")])
        .absent(&[("pkg/ro/stats.py", "MUT110")]),
        Step::new(
            "the middle helper's parameter is unannotated again",
            vec![save("pkg/ro/mid.py", RO_MID)],
        )
        .present(&[("pkg/ro/stats.py", "MUT110")])
        .absent(&[("pkg/ro/mid.py", "MUT110")]),
        Step::new(
            "the helper two modules below stops changing the parameter",
            vec![save("pkg/ro/low.py", RO_LOW)],
        )
        .absent(&[("pkg/ro/stats.py", "MUT110")]),
        Step::new(
            "a function a package re-exports starts changing its parameter in place",
            vec![Action::Save("pkg/ro/impl.py", impl_changing)],
        )
        .present(&[("pkg/plot.py", "MUT110")]),
        Step::new(
            "the re-exported function copies again",
            vec![save("pkg/ro/impl.py", RO_IMPL)],
        )
        .absent(&[("pkg/plot.py", "MUT110")]),
        Step::new(
            "the package re-exports a function that changes its parameter under the same name",
            vec![save(
                "pkg/ro/__init__.py",
                "from pkg.ro.impl import scale_in_place as scale\n",
            )],
        )
        .present(&[("pkg/plot.py", "MUT110")]),
        Step::new(
            "the package re-exports the copying function again",
            vec![save("pkg/ro/__init__.py", RO_INIT)],
        )
        .absent(&[("pkg/plot.py", "MUT110")]),
    ]
}

/// `MUT202` return flow and `MUT109`: facts of a function in another module that a caller's
/// report reads (the wrapper's protected return, the helper's announcement), and a caller's
/// announcement after a call that a callee's report reads.
fn steps_mut202_mut109() -> Vec<Step> {
    vec![
        Step::new(
            "a wrapper in another module starts returning a declared producer's value",
            vec![save("pkg/lookup.py", LOOKUP_SHARED)],
        )
        .present(&[("pkg/tagger.py", "MUT202")]),
        Step::new(
            "the wrapper returns an `@lru_cache` result instead",
            vec![save("pkg/lookup.py", LOOKUP_CACHED)],
        )
        .present(&[("pkg/tagger.py", "MUT202")]),
        Step::new(
            "the wrapper returns a fresh value again",
            vec![save("pkg/lookup.py", LOOKUP)],
        )
        .absent(&[("pkg/tagger.py", "MUT202")]),
        Step::new(
            "a helper in another module stops calling the invalidator",
            vec![save("pkg/announce.py", ANNOUNCE_SILENT)],
        )
        .present(&[("pkg/grow.py", "MUT109")]),
        Step::new(
            "the helper calls the invalidator again",
            vec![save("pkg/announce.py", ANNOUNCE)],
        )
        .absent(&[("pkg/grow.py", "MUT109")]),
        Step::new(
            "the only caller of a public writer in another module stops announcing",
            vec![save("pkg/grow.py", GROW_SILENT_ADOPT)],
        )
        .present(&[("pkg/nodes.py", "MUT109")]),
        Step::new(
            "the caller announces after the call again",
            vec![save("pkg/grow.py", GROW)],
        )
        .absent(&[("pkg/nodes.py", "MUT109")]),
        Step::new(
            "the caller two calls above a public writer stops announcing",
            vec![save("pkg/planter.py", PLANTER_SILENT)],
        )
        .present(&[("pkg/graft.py", "MUT109")]),
        Step::new(
            "the distant caller announces again",
            vec![save("pkg/planter.py", PLANTER)],
        )
        .absent(&[("pkg/graft.py", "MUT109")]),
        Step::new(
            "a helper in another module becomes a generator iterated after the invalidator",
            vec![save("pkg/kid_gen.py", KID_GEN_GENERATOR)],
        )
        .present(&[("pkg/kid_user.py", "MUT109")]),
        Step::new(
            "the helper returns a list again",
            vec![save("pkg/kid_gen.py", KID_GEN)],
        )
        .absent(&[("pkg/kid_user.py", "MUT109")]),
    ]
}

fn steps() -> Vec<Step> {
    let mut steps = vec![
        // 1. The initial index.
        Step::new("initial index", Vec::new()).present(&[
            ("pkg/ops.py", "MUT201"),
            ("pkg/ops.py", "MUT203"),
            ("pkg/svc_user.py", "EFF101"),
            ("pkg/guard_use.py", "EFF102"),
            ("pkg/guard_default_use.py", "EFF102"),
            ("pkg/guard_conn_use.py", "EFF102"),
            ("pkg/fill.py", "MUT201"),
            ("pkg/fill_main.py", "MUT201"),
            ("pkg/views.py", "MUT201"),
        ]),
        // 2. A sync helper two modules below an `async def` starts and stops blocking.
        Step::new(
            "the helper two modules below the async def starts blocking",
            vec![save("pkg/low.py", LOW_BLOCKING)],
        )
        .present(&[("pkg/top.py", "EFF101")]),
        Step::new(
            "a comment after the blocking call",
            vec![save("pkg/low.py", &format!("{LOW_BLOCKING}# done\n"))],
        )
        .present(&[("pkg/top.py", "EFF101")])
        .cutoff(),
        Step::new(
            "a comment in the async caller",
            vec![save("pkg/top.py", &format!("# entry points\n{TOP}"))],
        )
        .present(&[("pkg/top.py", "EFF101")])
        .cutoff(),
        Step::new(
            "a comment above the blocking call moves the chain's last hop",
            vec![save(
                "pkg/low.py",
                &format!("# network helpers\n{LOW_BLOCKING}# done\n"),
            )],
        )
        .present(&[("pkg/top.py", "EFF101")]),
        // 6. A syntax error in the middle of an edit of a callee, saved, then the fix.
        Step::new(
            "the middle callee stops parsing",
            vec![
                Action::Change("pkg/mid.py", MID.replace("fetch(url)", "fetch(")),
                save("pkg/mid.py", MID_BROKEN),
            ],
        )
        .absent(&[("pkg/top.py", "EFF101")]),
        Step::new(
            "the middle callee parses again",
            vec![save("pkg/mid.py", MID)],
        )
        .present(&[("pkg/top.py", "EFF101")]),
        Step::new(
            "the helper two modules below stops blocking",
            vec![save("pkg/low.py", LOW)],
        )
        .absent(&[("pkg/top.py", "EFF101")]),
        // 3. Property getters.
        Step::new(
            "a cached property starts reading a mutated attribute",
            vec![save("pkg/shapes.py", SHAPES_READING)],
        )
        .present(&[("pkg/shapes.py", "MUT101")]),
        Step::new(
            "the cached property stops reading it",
            vec![save("pkg/shapes.py", SHAPES)],
        )
        .absent(&[("pkg/shapes.py", "MUT101")]),
        Step::new(
            "__hash__ reads a property whose getter reads a mutated attribute",
            vec![save("pkg/keys.py", KEYS_BY_NAME)],
        )
        .present(&[("pkg/keys.py", "MUT107")]),
        Step::new(
            "the getter reads a constant attribute again",
            vec![save("pkg/keys.py", KEYS)],
        )
        .absent(&[("pkg/keys.py", "MUT107")]),
        Step::new(
            "a getter read from an async def in another file starts blocking",
            vec![save("pkg/remote.py", REMOTE_BLOCKING)],
        )
        .present(&[("pkg/reader.py", "EFF101")]),
        Step::new(
            "the getter stops blocking",
            vec![save("pkg/remote.py", REMOTE)],
        )
        .absent(&[("pkg/reader.py", "EFF101")]),
        // Guarded effects across modules.
        Step::new(
            "a helper in another file gains the guard its caller's constant switches off",
            vec![save("pkg/guard_net.py", GUARD_NET_GUARDED)],
        )
        .present(&[("pkg/guard_default_use.py", "EFF102")])
        .absent(&[("pkg/guard_use.py", "EFF102")]),
        Step::new(
            "the guarded parameter's default becomes falsy",
            vec![save(
                "pkg/guard_net.py",
                &GUARD_NET_GUARDED.replace("cache=True", "cache=False"),
            )],
        )
        .absent(&[("pkg/guard_default_use.py", "EFF102")]),
        Step::new(
            "the helper loses its guard",
            vec![save("pkg/guard_net.py", GUARD_NET)],
        )
        .present(&[
            ("pkg/guard_use.py", "EFF102"),
            ("pkg/guard_default_use.py", "EFF102"),
        ]),
        Step::new(
            "a connection class in another file connects lazily",
            vec![save(
                "pkg/guard_conn.py",
                &GUARD_CONN.replace(
                    "        self._sock = urlopen",
                    "        if self._sock is None:\n            self._sock = urlopen",
                ),
            )],
        )
        .absent(&[("pkg/guard_conn_use.py", "EFF102")]),
        Step::new(
            "the connection class connects on every call again",
            vec![save("pkg/guard_conn.py", GUARD_CONN)],
        )
        .present(&[("pkg/guard_conn_use.py", "EFF102")]),
    ];
    // 4. `MUT108`, `MUT110`, then where bodies run.
    steps.extend(steps_mut108());
    steps.extend(steps_mut110());
    steps.extend(steps_body_runs());
    steps.extend(steps_small_displays());
    steps.extend(steps_mut202_mut109());
    steps.extend(steps_receiver_typing());
    steps.extend([
        // 5. Contracts: the reports of private helpers move between them and their callers.
        Step::new(
            "only allowed functions call the private helpers",
            vec![save("pkg/store.py", STORE_CALLING)],
        )
        .absent(&[("pkg/ops.py", "MUT201"), ("pkg/ops.py", "MUT203")]),
        Step::new(
            "a non-allowed caller of the private helpers",
            vec![save("pkg/service.py", SERVICE_CALLING)],
        )
        .present(&[("pkg/service.py", "MUT201"), ("pkg/service.py", "MUT203")]),
        Step::new(
            "the non-allowed caller stops calling them",
            vec![save("pkg/service.py", SERVICE)],
        )
        .absent(&[("pkg/service.py", "MUT201"), ("pkg/service.py", "MUT203")]),
        Step::new(
            "no caller is left: the helpers report themselves",
            vec![save("pkg/store.py", STORE)],
        )
        .present(&[("pkg/ops.py", "MUT201"), ("pkg/ops.py", "MUT203")]),
    ]);
    steps.extend(steps_contract_extensions());
    steps.extend(steps_keys_fresh_strict());
    steps.extend([
        // 7. Files.
        Step::new(
            "a module its caller imports is created",
            vec![Action::Create("pkg/extra.py", EXTRA.to_owned())],
        )
        .present(&[("pkg/uses_extra.py", "EFF101")]),
        Step::new(
            "the module is deleted",
            vec![Action::Delete("pkg/extra.py")],
        )
        .absent(&[("pkg/uses_extra.py", "EFF101")]),
        Step::new(
            "a stub next to a module",
            vec![Action::Create("pkg/svc.pyi", SVC_STUB.to_owned())],
        ),
        Step::new("the stub is removed", vec![Action::Delete("pkg/svc.pyi")])
            .present(&[("pkg/svc_user.py", "EFF101")]),
        Step::new(
            "the module is deleted while its stub stays",
            vec![
                Action::Create("pkg/svc.pyi", SVC_STUB.to_owned()),
                Action::Delete("pkg/svc.py"),
            ],
        )
        .absent(&[("pkg/svc_user.py", "EFF101")]),
        Step::new(
            "the module is back next to its stub",
            vec![Action::Create("pkg/svc.py", SVC.to_owned())],
        )
        .present(&[("pkg/svc_user.py", "EFF101")]),
        Step::new(
            "the stub is removed again",
            vec![Action::Delete("pkg/svc.pyi")],
        )
        .present(&[("pkg/svc_user.py", "EFF101")]),
        // Python imports a package over the module of the same name.
        Step::new(
            "a package next to the module of the same name",
            vec![Action::Create("pkg/svc/__init__.py", SVC_PACKAGE.to_owned())],
        )
        .absent(&[("pkg/svc_user.py", "EFF101")]),
        Step::new(
            "the package's module is edited",
            vec![save("pkg/svc.py", &format!("{SVC}# edited\n"))],
        )
        .absent(&[("pkg/svc_user.py", "EFF101")]),
        Step::new(
            "the package is removed",
            vec![Action::Delete("pkg/svc/__init__.py")],
        )
        .present(&[("pkg/svc_user.py", "EFF101")]),
        // The configuration changes while the server runs.
        Step::new(
            "the configuration gains a write owner",
            vec![Action::Configure(format!(
                "{CONFIG}\n[[tool.heki.write-owners]]\nname = \"boxes\"\ntargets = [\"Box.items\"]\n"
            ))],
        )
        .present(&[("pkg/shapes.py", "MUT201")]),
        Step::new(
            "the write owner is removed again",
            vec![Action::Configure(CONFIG.to_owned())],
        )
        .absent(&[("pkg/shapes.py", "MUT201")]),
        Step::new(
            "the configuration becomes invalid",
            vec![Action::Configure(CONFIG.replace(
                "extend-select = [\"MUT108\"]",
                "extend-select = [\"MUT108\"]\nselect = [\"NOPE1\"]",
            ))],
        )
        .absent(&[("pkg/svc_user.py", "EFF101")]),
        Step::new(
            "the configuration is valid again",
            vec![Action::Configure(CONFIG.to_owned())],
        )
        .present(&[("pkg/svc_user.py", "EFF101")]),
        // Project-wide assumptions.
        Step::new(
            "the module patching builtins is deleted",
            vec![Action::Delete("pkg/compat.py")],
        )
        .present(&[("pkg/loops.py", "OPT110")]),
        Step::new(
            "a typing stub imports builtins for its annotations",
            vec![Action::Create("pkg/spelled.pyi", SPELLED_STUB.to_owned())],
        )
        .present(&[("pkg/loops.py", "OPT110")]),
        Step::new(
            "a module starts patching builtins",
            vec![Action::Create("pkg/compat.py", COMPAT.to_owned())],
        )
        .absent(&[("pkg/loops.py", "OPT110")]),
        // Declarations other modules resolve names through.
        Step::new(
            "a module that stores an imported global on self",
            vec![save("pkg/api.py", &format!("# client\n{API}"))],
        ),
        Step::new(
            "a factory's return type changes the attribute type of another module's class",
            vec![save(
                "pkg/engines.py",
                &ENGINES.replace(
                    "def make() -> Engine:\n    return Engine()",
                    "def make() -> Turbo:\n    return Turbo()",
                ),
            )],
        )
        .present(&[("pkg/garage.py", "EFF101")]),
        Step::new(
            "the factory returns the plain engine again",
            vec![save("pkg/engines.py", ENGINES)],
        )
        .absent(&[("pkg/garage.py", "EFF101")]),
        Step::new(
            "a base class's cached property stops reading what a subclass module mutates",
            vec![save("pkg/base.py", BASE_CONSTANT)],
        )
        .absent(&[("pkg/derived.py", "MUT101")]),
        Step::new(
            "the cached property reads it again",
            vec![save("pkg/base.py", BASE)],
        )
        .present(&[("pkg/derived.py", "MUT101")]),
        Step::new(
            "the base class's cached property returns the object the subclass changes in place",
            vec![save(
                "pkg/base.py",
                &BASE.replace("return len(self.items)", "return self.items"),
            )],
        )
        .absent(&[("pkg/derived.py", "MUT101")]),
        Step::new(
            "the getter returns a copy of it instead",
            vec![save(
                "pkg/base.py",
                &BASE.replace("return len(self.items)", "return list(self.items)"),
            )],
        )
        .present(&[("pkg/derived.py", "MUT101")]),
        Step::new(
            "a mixin helper of a cached property holding the object starts reading it",
            vec![save("pkg/mixin.py", MIXIN_READING)],
        )
        .present(&[("pkg/sub.py", "MUT101")]),
        Step::new(
            "the helper stops reading it",
            vec![save("pkg/mixin.py", MIXIN)],
        )
        .absent(&[("pkg/sub.py", "MUT101")]),
        Step::new(
            "a subclass module starts overriding the method an async def calls",
            vec![save(
                "pkg/square.py",
                &SQUARE.replace("def side", "def area"),
            )],
        )
        .present(&[("pkg/draw.py", "EFF101")]),
        Step::new(
            "the override is removed",
            vec![save("pkg/square.py", SQUARE)],
        )
        .absent(&[("pkg/draw.py", "EFF101")]),
        // Declarations reached through a re-export.
        Step::new(
            "a re-exported class's method becomes a property getter that blocks",
            vec![save(
                "pkg/ui/impl.py",
                &UI_IMPL.replace("    def data", "    @property\n    def data"),
            )],
        )
        .present(&[("pkg/consumer.py", "EFF101")]),
        Step::new(
            "the getter `__hash__` reads becomes a property reading a mutated attribute",
            vec![save(
                "pkg/ui/impl.py",
                &UI_IMPL
                    .replace("    def data", "    @property\n    def data")
                    .replace("    def label", "    @property\n    def label"),
            )],
        )
        .present(&[("pkg/consumer.py", "MUT107")]),
        Step::new(
            "the re-exported class gains a base class whose method blocks",
            vec![save(
                "pkg/ui/impl.py",
                &UI_IMPL.replace("class Widget:", "class Widget(Refreshing):"),
            )],
        )
        .present(&[("pkg/consumer.py", "EFF101")])
        .absent(&[("pkg/consumer.py", "MUT107")]),
        Step::new(
            "the re-exported classes are plain again",
            vec![save("pkg/ui/impl.py", UI_IMPL)],
        )
        .absent(&[("pkg/consumer.py", "EFF101"), ("pkg/consumer.py", "MUT107")]),
    ]);
    steps
}

// ---------------------------------------------------------------------------------- the oracle

/// One diagnostic in LSP coordinates: `(file, (line, UTF-16 column) start, end)`.
type Range = (String, (u64, u64), (u64, u64));

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Diag {
    range: Range,
    code: String,
    severity: u64,
    message: String,
    related: Vec<(Range, String)>,
}

/// Byte offset -> LSP position (0-based line, UTF-16 column) in `text`.
fn position(text: &str, offset: u64) -> (u64, u64) {
    let before = &text[..usize::try_from(offset).unwrap()];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    (
        before.matches('\n').count() as u64,
        before[line_start..].encode_utf16().count() as u64,
    )
}

fn cli_range(p: &Project, file: &str, span: &Value) -> Range {
    let text = p.read(file);
    (
        file.to_owned(),
        position(&text, span["start"].as_u64().unwrap()),
        position(&text, span["end"].as_u64().unwrap()),
    )
}

/// `heki check --output-format json` of the project, by file.
fn cli(p: &Project) -> BTreeMap<String, Vec<Diag>> {
    let (code, out, err) = p.check(&["--output-format", "json"]);
    assert!(matches!(code, 0..=2), "exit {code}: {err}");
    // An invalid configuration: nothing is checked (exit 2, an error, no report).
    let diags: Vec<Value> = if code == 2 && out.is_empty() {
        Vec::new()
    } else {
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}:\n{out}\n{err}"))
    };
    let mut by_file: BTreeMap<String, Vec<Diag>> = BTreeMap::new();
    for d in diags {
        let file = d["path"].as_str().unwrap().to_owned();
        let related = d["related"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    cli_range(p, r["path"].as_str().unwrap(), &r["span"]),
                    r["message"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        by_file.entry(file.clone()).or_default().push(Diag {
            range: cli_range(p, &file, &d["span"]),
            code: d["rule_code"].as_str().unwrap().to_owned(),
            severity: match d["severity"].as_str().unwrap() {
                "error" => 1,
                "warning" => 2,
                other => panic!("severity {other}"),
            },
            message: format!(
                "{} ({})",
                d["message"].as_str().unwrap(),
                d["rule_slug"].as_str().unwrap()
            ),
            related,
        });
    }
    by_file
}

fn lsp_range(root: &str, location_uri: &Value, range: &Value) -> Range {
    let u = location_uri.as_str().unwrap();
    let file = u
        .strip_prefix(root)
        .unwrap_or_else(|| panic!("{u} is outside {root}"));
    let at = |p: &Value| {
        (
            p["line"].as_u64().unwrap(),
            p["character"].as_u64().unwrap(),
        )
    };
    (file.to_owned(), at(&range["start"]), at(&range["end"]))
}

/// The latest diagnostics the server published, by file.
fn published(lsp: &Lsp, root: &str) -> BTreeMap<String, Vec<Diag>> {
    let mut by_file = BTreeMap::new();
    for (u, diags) in &lsp.latest {
        let list: Vec<Diag> = diags
            .iter()
            .map(|d| Diag {
                range: lsp_range(root, &json!(u), &d["range"]),
                code: d["code"].as_str().unwrap().to_owned(),
                severity: d["severity"].as_u64().unwrap(),
                message: d["message"].as_str().unwrap().to_owned(),
                related: d["relatedInformation"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|r| {
                        (
                            lsp_range(root, &r["location"]["uri"], &r["location"]["range"]),
                            r["message"].as_str().unwrap().to_owned(),
                        )
                    })
                    .collect(),
            })
            .collect();
        let file = u.strip_prefix(root).unwrap().to_owned();
        by_file.insert(file, list);
    }
    by_file
}

/// Per-file differences between the server and the CLI (empty: they agree).
fn differences(
    server: &BTreeMap<String, Vec<Diag>>,
    reference: &BTreeMap<String, Vec<Diag>>,
) -> Vec<String> {
    let files: BTreeSet<&String> = server.keys().chain(reference.keys()).collect();
    let mut out = Vec::new();
    for file in files {
        let mut got = server.get(file).cloned().unwrap_or_default();
        let mut want = reference.get(file).cloned().unwrap_or_default();
        got.sort();
        want.sort();
        if got != want {
            out.push(format!(
                "{file}:\n  server: {got:#?}\n  heki check: {want:#?}"
            ));
        }
    }
    out
}

struct Session {
    p: Project,
    lsp: Lsp,
    /// `file://<root>/`.
    root: String,
    versions: BTreeMap<&'static str, i32>,
}

impl Session {
    fn start(name: &str) -> Self {
        let p = Project::new(name);
        p.write("pyproject.toml", CONFIG);
        let files = initial_files();
        for (rel, text) in &files {
            p.write(rel, text);
        }
        let root = format!("{}/", uri(&p.dir));
        let mut lsp = Lsp::start(&p.dir, &["server", "--stdio"]);
        lsp.request(
            "initialize",
            json!({
                "processId": null,
                "rootUri": uri(&p.dir),
                "capabilities": {"workspace": {"didChangeWatchedFiles": {"dynamicRegistration": true}}}
            }),
        );
        lsp.notify("initialized", json!({}));
        let mut s = Self {
            p,
            lsp,
            root,
            versions: BTreeMap::new(),
        };
        for (rel, text) in files {
            s.open(rel, &text);
        }
        s
    }

    fn uri(&self, rel: &str) -> String {
        format!("{}{rel}", self.root)
    }

    fn open(&mut self, rel: &'static str, text: &str) {
        let u = self.uri(rel);
        self.lsp.open(&u, text);
        self.versions.insert(rel, 1);
    }

    fn change(&mut self, rel: &'static str, text: &str) {
        let version = self.versions.get_mut(rel).expect("an open document");
        *version += 1;
        let v = *version;
        let u = self.uri(rel);
        self.lsp.change(&u, v, text);
    }

    fn watched(&mut self, rel: &str, kind: u8) {
        let u = self.uri(rel);
        self.lsp.notify(
            "workspace/didChangeWatchedFiles",
            json!({"changes": [{"uri": u, "type": kind}]}),
        );
    }

    fn apply(&mut self, action: &Action) {
        match action {
            Action::Save(rel, text) => {
                self.change(rel, text);
                self.p.write(rel, text);
                let u = self.uri(rel);
                self.lsp.notify(
                    "textDocument/didSave",
                    json!({"textDocument": {"uri": u}, "text": text}),
                );
            }
            Action::Change(rel, text) => self.change(rel, text),
            Action::Create(rel, text) => {
                self.p.write(rel, text);
                self.watched(rel, 1);
                self.open(rel, text);
            }
            Action::Delete(rel) => {
                let u = self.uri(rel);
                self.lsp
                    .notify("textDocument/didClose", json!({"textDocument": {"uri": u}}));
                self.versions.remove(rel);
                std::fs::remove_file(self.p.dir.join(rel)).unwrap();
                self.watched(rel, 3);
            }
            Action::Configure(text) => {
                self.p.write("pyproject.toml", text);
                self.watched("pyproject.toml", 2);
            }
        }
    }

    /// Run one step and compare the server with the CLI; returns the disagreements.
    fn run(&mut self, step: &Step) -> Vec<String> {
        let before = self.lsp.settle();
        let first = self.lsp.publications.len();
        for action in &step.actions {
            self.apply(action);
        }
        let after = self.lsp.settle();
        let reference = cli(&self.p);
        for (file, code) in step.present {
            assert!(
                reference
                    .get(*file)
                    .is_some_and(|d| d.iter().any(|x| x.code == *code)),
                "step `{}`: the CLI does not report {code} in {file}: {reference:#?}",
                step.name
            );
        }
        for (file, code) in step.absent {
            assert!(
                !reference
                    .get(*file)
                    .is_some_and(|d| d.iter().any(|x| x.code == *code)),
                "step `{}`: the CLI reports {code} in {file}: {reference:#?}",
                step.name
            );
        }
        let mut out: Vec<String> = differences(&published(&self.lsp, &self.root), &reference)
            .into_iter()
            .map(|d| format!("step `{}`: {d}", step.name))
            .collect();
        if step.cutoff {
            let edited: BTreeSet<String> = step
                .actions
                .iter()
                .map(|a| match a {
                    Action::Save(rel, _)
                    | Action::Change(rel, _)
                    | Action::Create(rel, _)
                    | Action::Delete(rel) => self.uri(rel),
                    Action::Configure(_) => self.uri("pyproject.toml"),
                })
                .collect();
            let others: BTreeSet<&String> = self.lsp.publications[first..]
                .iter()
                .filter(|u| !edited.contains(*u))
                .collect();
            if after["dependent_reanalyses"] != before["dependent_reanalyses"]
                || after["last_summary_changed"] != false
                || !others.is_empty()
            {
                out.push(format!(
                    "step `{}`: no Early Cutoff: stats before {before}, after {after}; re-published {others:?}",
                    step.name
                ));
            }
        }
        out
    }
}

#[test]
fn lsp_diagnostics_equal_a_full_check_after_every_step() {
    let mut s = Session::start("lsp_oracle");
    let mut failures = Vec::new();
    for step in steps() {
        failures.extend(s.run(&step));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    s.lsp.stop(true);
}
