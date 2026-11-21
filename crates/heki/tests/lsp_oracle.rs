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
//! severity, certainty, message (labelled for a heuristic rule), related locations, and help (the
//! last related entry, at the diagnostic's own range) of every diagnostic. A step also names diagnostics
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
extend-select = ["MUT101", "MUT108", "MUT111", "EFF107", "MUT112", "MUT113"]

[[tool.heki.write-owners]]
name = "registry"
targets = ["Registry.entries"]
allowed-writers = ["Store.*"]
unresolved = "report"
message = "add entries through `Store`"

[[tool.heki.read-owners]]
name = "vault"
targets = ["Vault.secret", "Locker.secret"]
allowed-readers = ["Store.*"]

[[tool.heki.read-owners]]
name = "layer"
targets = ["Ledger.balance"]
forbidden-readers = ["pkg.layer.*"]

[[tool.heki.call-owners]]
name = "ledger"
targets = ["Ledger.post", "Receipt"]
allowed-callers = ["Store.*"]

[[tool.heki.read-owners]]
name = "seal"
targets = ["Seal.code"]
unresolved = "report"

[[tool.heki.call-owners]]
name = "seal-break"
targets = ["Seal.break_open"]
unresolved = "report"

[[tool.heki.call-owners]]
name = "reseal"
targets = ["reseal"]
unresolved = "report"

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

[[tool.heki.effect-boundaries]]
name = "render"
scopes = ["pkg.gate.render", "pkg.walk_b.walk"]
forbid = ["blocking-net", "sleep"]
allowed-callees = ["requests.get"]
[[tool.heki.ecosystem]]
symbol = "pkg.natives.fetch"
effects = ["blocking_net"]

[[tool.heki.readonly-returns]]
name = "catalog"
producers = ["pkg.catalog.Catalog.entry"]
deep = true

[[tool.heki.invalidation]]
name = "index"
targets = ["Node.kids"]
invalidators = ["Index.refresh"]

[[tool.heki.invalidation]]
name = "dial"
targets = ["pkg.dials:*.level"]
invalidators = ["pkg.dials:recalibrate"]

[[tool.heki.effect-boundaries]]
name = "rack-view"
scopes = ["pkg.rack_view"]
forbid = ["writes-targets"]
write-targets = ["Rack.items"]
allowed-callees = ["pkg.rack_ops.empty"]

[[tool.heki.write-owners]]
name = "rack"
targets = ["Rack.items"]
allowed-writers = ["pkg.rack_ops.empty"]
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
const GUARD_GLOBAL: &str = "from urllib.request import urlopen\n\n_conn = None\n\n\ndef conn():\n    global _conn\n    _conn = urlopen(\"https://example.com\")\n    return _conn\n";
const GUARD_GLOBAL_USE: &str =
    "from pkg.guard_global import conn\n\n\ndef run(xs):\n    for x in xs:\n        conn()\n";
const GUARD_CONN_CACHED: &str = "from functools import cached_property\nfrom urllib.request import urlopen\n\n\nclass Conn:\n    def __init__(self):\n        self._sock = None\n\n    @cached_property\n    def sock(self):\n        return urlopen(\"https://example.com\")\n\n    def send(self):\n        return self.sock\n";

// Guards a buffer decides (`EFF101`): a reader in another module gains a `hasattr` test that a
// buffer argument decides, then tests an attribute a buffer may lack, then rebinds a missing
// argument to a buffer (a test of the merged value), and loses its tests again.
const BUF_READ: &str = "from io import BytesIO\nfrom urllib.request import urlopen\n\n\ndef read_any(src=None):\n    return urlopen(src).read()\n";
const BUF_READ_HASATTR: &str = "from io import BytesIO\nfrom urllib.request import urlopen\n\n\ndef read_any(src=None):\n    if hasattr(src, \"read\"):\n        return src.read()\n    return urlopen(src).read()\n";
const BUF_READ_REBINDING: &str = "from io import BytesIO\nfrom urllib.request import urlopen\n\n\ndef read_any(src=None):\n    if src is None:\n        src = BytesIO()\n    if isinstance(src, (str, bytes)):\n        return urlopen(src).read()\n    return src.read()\n";
const BUF_USE: &str = "from io import BytesIO\n\nfrom pkg.buf_read import read_any\n\n\nasync def load(data):\n    return read_any(BytesIO(data))\n";
const BUF_DEFAULT_USE: &str =
    "from pkg.buf_read import read_any\n\n\nasync def load():\n    return read_any()\n";

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

// The index across files: two modules drive each other's private coroutine by hand
// (`sync_driven_only`), a coroutine a sync function drives calls a blocking helper in another
// module, two functions in two modules call each other (one is a boundary's scope function),
// and a boundary allowing `requests.get` reaches a helper making two calls of it.
const DRIVE_A: &str = "from pkg.drive_b import _cb\n\n\nasync def _ca():\n    return 1\n\n\ndef drive_b():\n    _cb().send(None)\n";
const DRIVE_B: &str = "from pkg.drive_a import _ca\n\n\nasync def _cb():\n    return 1\n\n\ndef drive_a():\n    _ca().send(None)\n";
const SLOW_LEAF: &str = "import time\n\n\ndef block():\n    time.sleep(1)\n";
const WORKER: &str = "from pkg.slow_leaf import block\n\n\nasync def _work():\n    block()\n";
const WORKER_DRIVER: &str =
    "from pkg.worker import _work\n\n\ndef drive():\n    _work().send(None)\n";
const WALK_A: &str = "import time\n\nfrom pkg import walk_b\n\n\ndef walk(n):\n    if n > 0:\n        walk_b.walk(n - 1)\n";
const WALK_B: &str =
    "from pkg import walk_a\n\n\ndef walk(n):\n    if n > 0:\n        walk_a.walk(n - 1)\n";
const GATE_NET: &str =
    "import requests\n\n\ndef fetch(url):\n    requests.get(url)\n    requests.get(url)\n";
const GATE: &str = "from pkg.gate_net import fetch\n\n\ndef render(url):\n    fetch(url)\n";
// A mixin shared by a sync and an async client in other modules, called on an async client.
const MIXIN_COMMANDS: &str =
    "class Commands:\n    def masters(self):\n        return self.execute_command(\"MASTERS\")\n";
const SYNC_CLIENT: &str = "from pkg.mixin_commands import Commands\n\n\nclass SyncClient(Commands):\n    def execute_command(self, *args):\n        return args\n";
const ASYNC_CLIENT: &str = "from pkg.mixin_commands import Commands\n\n\nclass AsyncClient(Commands):\n    async def execute_command(self, *args):\n        return args\n";
const ASYNC_USER: &str = "from pkg.async_client import AsyncClient\n\n\nasync def poll(c: AsyncClient):\n    await c.masters()\n";

// Constant keys, objects under construction, and the strict mode: a `__dict__` key names the
// guarded attribute through a `Final` constant of another module; a private helper fills a new
// object its caller hands it; a helper writes a target on a parameter its caller types.
const KEY_CONSTS: &str = "from typing import Final\n\nKEY: Final = \"entries\"\n";
const KEY_WRITER: &str = "from pkg.key_consts import KEY\nfrom pkg.model import Registry\n\n\ndef mark(r: Registry):\n    r.__dict__[KEY] = []\n";
// A class constant names the guarded attribute for the class's own methods.
const KEY_CLASS: &str = "from typing import ClassVar\n\nfrom pkg.model import Registry\n\n\nclass Keys:\n    KEY: ClassVar[str] = \"entries\"\n\n    def mark(self, r: Registry):\n        r.__dict__[self.KEY] = []\n";
// Module aliases and getters: a module alias of `object.__setattr__`, imported by another
// module, writes the guarded attribute; a function returning the guarded container hands it to
// the caller changing it.
const ALIAS_SRC: &str = "_set = object.__setattr__\n";
const ALIAS_USE: &str = "from pkg.alias_src import _set\nfrom pkg.model import Registry\n\n\ndef mark(r: Registry):\n    _set(r, \"entries\", [])\n";
const ALIAS_REBIND: &str = "from pkg import alias_src\n\nalias_src._set = print\n";
// A read through a protocol declaring the attribute is judged by the protocol; once the
// protocol no longer declares it, by the class implementing it.
const PROTO: &str = "from typing import Protocol\n\n\nclass Safe(Protocol):\n    secret: str\n\n\nclass Locker:\n    def __init__(self):\n        self.secret = \"\"\n        self.size = 0\n";
const PROTO_USE: &str = "from pkg.proto import Safe\n\n\ndef peek(s: Safe):\n    return s.secret\n";
const GETTER_SRC: &str =
    "from pkg.model import Registry\n\n\ndef entries_of(r: Registry):\n    return r.entries\n";
const GETTER_USE: &str = "from pkg.getter_src import entries_of\nfrom pkg.model import Registry\n\n\ndef grow(r: Registry):\n    entries_of(r).append(1)\n";
// A getter of a base class hands out the guarded container on a subclass, until the subclass
// binds the name itself.
const LISTING_BASE: &str = "from pkg.model import Registry\n\n\nclass ListingBase(Registry):\n    @property\n    def listing(self):\n        return self.entries\n";
const LISTING_CLS: &str =
    "from pkg.listing_base import ListingBase\n\n\nclass Listing(ListingBase):\n    pass\n";
const LISTING_USE: &str =
    "from pkg.listing_cls import Listing\n\n\ndef grow(l: Listing):\n    l.listing.append(1)\n";
// A name the package does not bind (a compiled submodule would bind it) is what the
// configuration's catalog entry says; bound by a re-export, it is the function re-exported.
const NATIVES_USE: &str = "from pkg.natives import fetch\n\n\nasync def pull():\n    fetch()\n";
const KEY_REBIND: &str =
    "from pkg import key_consts\n\n\ndef reset():\n    key_consts.KEY = \"other\"\n";
const KEY_SUBCLASS: &str =
    "from pkg.key_class import Keys\n\n\nclass OtherKeys(Keys):\n    KEY = \"other\"\n";
const KEY_CLASS_REBIND: &str =
    "from pkg.key_class import Keys\n\n\ndef reset():\n    Keys.KEY = \"other\"\n";
const KEY_SUBCLASS_REBIND: &str = "from pkg.key_class import Keys\n\n\nclass QuietKeys(Keys):\n    pass\n\n\nQuietKeys.KEY = \"other\"\n";
// Another module reads the class constant on an instance, as the class's own method does.
const KEY_CLASS_READER: &str = "from pkg.key_class import Keys\nfrom pkg.model import Registry\n\n\ndef mark_with(keys: Keys, r: Registry):\n    r.__dict__[keys.KEY] = []\n";
// A `Final` class constant of a class a decorator of another module builds; a third module
// reads it on the class object. With `dataclass_transform` on the decorator, the class's
// annotations declare fields, whose class attribute the transform's runtime decides.
const FIELD_DECO: &str = "def model(cls):\n    return cls\n";
const FIELD_DECO_TRANSFORM: &str = "from typing import dataclass_transform\n\n\n@dataclass_transform()\ndef model(cls):\n    return cls\n";
const FIELD_KEYS: &str = "from typing import Final\n\nfrom pkg.field_deco import model\n\n\n@model\nclass FieldKeys:\n    KEY: Final[str] = \"entries\"\n";
const FIELD_READER: &str = "from pkg.field_keys import FieldKeys\nfrom pkg.model import Registry\n\n\ndef mark_fields(r: Registry):\n    r.__dict__[FieldKeys.KEY] = []\n";
// A subclass lists a base defining the name before the class: its MRO finds the base's value.
const KEY_OTHER_FIRST: &str = "from typing import ClassVar\n\nfrom pkg.key_class import Keys\n\n\nclass OtherKey:\n    KEY: ClassVar[str] = \"other\"\n\n\nclass OtherFirstKeys(OtherKey, Keys):\n    pass\n";
const FRESH_OPS: &str = "from pkg.model import Registry\n\nKEPT = []\n\n\ndef _fill(r: Registry):\n    r.entries.append(1)\n";
const FRESH_BUILD: &str = "from pkg.fresh_ops import _fill\nfrom pkg.model import Registry\n\n\ndef build():\n    r = Registry()\n    _fill(r)\n    return r\n";
const STRICT_OPS: &str = "def _reset(r):\n    r.entries = []\n";
// The strict reads and calls: helpers that type nothing read and call a guarded attribute and
// method; a caller in another module binds their object to another class, then to the guarded
// one.
const SEAL: &str = "class Seal:\n    def __init__(self):\n        self.code = \"\"\n\n    def break_open(self):\n        return None\n";
const SEAL_PEEK: &str = "def peek(s):\n    return s.code\n\n\ndef crack(s):\n    s.break_open()\n";
const SEAL_USE: &str = "from pkg.seal_peek import crack, peek\n\n\nclass Plain:\n    code = \"\"\n\n    def break_open(self):\n        return None\n\n\ndef use(p: Plain):\n    peek(p)\n    crack(p)\n";
// A call target naming no class (`reseal`): a helper that types nothing calls the method on an
// object its caller in another module binds to a class defining it, in a third module.
const SEALER: &str = "class Sealer:\n    def reseal(self):\n        return None\n";
const RESEAL_HELPER: &str = "def redo(s):\n    s.reseal()\n";
const RESEAL_USE: &str = "from pkg.reseal_helper import redo\nfrom pkg.sealer import Sealer\n\n\ndef again(s: Sealer):\n    redo(s)\n";
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
// A cached property holding `items` calls a mixin helper in another module for what it does
// (result discarded); a subclass in a third module changes `items` in place. The cache is stale
// once the helper writes an attribute the getter returns.
const PREP_MIXIN: &str =
    "class PrepMixin:\n    def _prepare(self):\n        print(len(self.items))\n";
const PREP_MIXIN_WRITING: &str =
    "class PrepMixin:\n    def _prepare(self):\n        self.count = len(self.items)\n";
const PREPARED: &str = "from functools import cached_property\n\nfrom pkg.prep_mixin import PrepMixin\n\n\nclass Prepared(PrepMixin):\n    def __init__(self):\n        self.items = []\n        self.count = 0\n\n    @cached_property\n    def view(self):\n        self._prepare()\n        return (self.items, self.count)\n";
const PREP_SUB: &str = "from pkg.prepared import Prepared\n\n\nclass Grown(Prepared):\n    def push(self, x):\n        self.items.append(x)\n";
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
// A function run on a worker thread, awaited per item in another module (`EFF103`).
const WORKER_OPS: &str = "from asgiref.sync import sync_to_async\nfrom sqlalchemy.orm import Session\n\n\n@sync_to_async\ndef load(session: Session, i):\n    return i\n";
const WORKER_VIEW: &str = "from pkg.worker_ops import load\n\n\nasync def load_all(session, ids):\n    for i in ids:\n        await load(session, i)\n";
const RANK_KEY: &str =
    "from sqlalchemy.orm import Session\n\n\ndef rank(session: Session, i):\n    return i\n";
const RANK_USER: &str = "from functools import partial\n\nfrom pkg.rank_key import rank\n\n\ndef ordered(session, ids):\n    return sorted(ids, key=partial(rank, session))\n";
const FEED: &str =
    "import httpx\n\n\ndef rows():\n    return [httpx.get(\"https://example.com\")]\n";
const FEED_GENERATOR: &str =
    "import httpx\n\n\ndef rows():\n    yield httpx.get(\"https://example.com\")\n";
// A task factory's argument from another module (`EFF107`): `ensure_future` of a plain method's
// result makes no new task (`FunctionDecl::makes_no_coroutine`), of an `async def`'s or a
// decorated method's result it may.
const WAITS: &str = "import asyncio\n\n\ndef logged(fn):\n    return fn\n\n\nclass Event:\n    def wait(self):\n        return asyncio.get_running_loop().create_future()\n";
const WAITS_ASYNC: &str = "import asyncio\n\n\ndef logged(fn):\n    return fn\n\n\nclass Event:\n    async def wait(self):\n        await asyncio.sleep(0)\n";
const WAITS_DECORATED: &str = "import asyncio\n\n\ndef logged(fn):\n    return fn\n\n\nclass Event:\n    @logged\n    def wait(self):\n        return asyncio.get_running_loop().create_future()\n";
const WAITER: &str = "import asyncio\n\nfrom pkg.waits import Event\n\n\nasync def watch(e: Event):\n    f = asyncio.ensure_future(e.wait())\n    await asyncio.sleep(0)\n";
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
// `MUT202`: a method of a cached object in another module fills the object's own memo, then
// changes its state instead.
const GLYPHS: &str = "from functools import lru_cache\n\n\nclass Glyphs:\n    def __init__(self):\n        self._width_cache = {}\n        self.seen = []\n\n    def width(self, ch):\n        if ch not in self._width_cache:\n            self._width_cache[ch] = len(ch)\n        return self._width_cache[ch]\n\n\n@lru_cache\ndef shared_glyphs() -> Glyphs:\n    return Glyphs()\n";
const GLYPHS_RECORDING: &str = "from functools import lru_cache\n\n\nclass Glyphs:\n    def __init__(self):\n        self._width_cache = {}\n        self.seen = []\n\n    def width(self, ch):\n        self.seen.append(ch)\n        return len(ch)\n\n\n@lru_cache\ndef shared_glyphs() -> Glyphs:\n    return Glyphs()\n";
const GLYPH_USER: &str = "from pkg.glyphs import shared_glyphs\n\n\ndef measure(text):\n    return sum(shared_glyphs().width(ch) for ch in text)\n\n\ndef first(text):\n    return shared_glyphs().width(text[0])\n";
const TAGGER: &str = "from pkg.catalog import Catalog\nfrom pkg.lookup import find\n\n\ndef tag(c: Catalog):\n    find(c, \"k\")[\"tag\"] = 1\n";
const NODES: &str = "class Node:\n    def __init__(self):\n        self.kids = []\n\n\nclass Index:\n    def refresh(self):\n        self.stale = False\n\n\ndef attach(node: Node, kid):\n    node.kids.append(kid)\n";
// A class an `invalidation` target names by its module (`pkg.dials:*.level`): written in
// another module directly and through an untyped helper of a third.
const DIALS: &str = "class Gauge:\n    def __init__(self) -> None:\n        self.level = 0\n\n\ndef recalibrate() -> None: ...\n";
const DIALS_REEXPORT: &str =
    "from pkg.dials_impl import Gauge\n\n\ndef recalibrate() -> None: ...\n";
const DIALS_IMPL: &str = "class Gauge:\n    def __init__(self) -> None:\n        self.level = 0\n";
const DIAL_OPS: &str = "def set_level(obj) -> None:\n    obj.level = 2\n";
const DIAL_USER: &str = "from pkg.dial_ops import set_level\nfrom pkg.dials import Gauge\n\n\ndef turn(g: Gauge) -> None:\n    g.level = 1\n\n\ndef push(g: Gauge) -> None:\n    set_level(g)\n";
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
// A helper of another module starts changing the argument it does not type in place: the
// caller's argument is a target field, so the caller writes it.
const KID_COUNT: &str = "def _count(xs):\n    return len(xs)\n";
const KID_COUNT_APPENDING: &str = "def _count(xs):\n    xs.append(0)\n    return len(xs)\n";
const KID_STOCK: &str = "from pkg.kid_count import _count\nfrom pkg.nodes import Node\n\n\ndef stock(node: Node):\n    return _count(node.kids)\n";
const KID_USER: &str = "from pkg.kid_gen import _grow_kids\nfrom pkg.nodes import Index, Node\n\n\ndef plant_all(index: Index, node: Node, kid):\n    pending = _grow_kids(node, kid)\n    index.refresh()\n    for _ in pending:\n        pass\n";
// Helpers handed a typed field (`EFF201` / `MUT201` `indirect_helper` writes): `refill` only
// leaves the change to `empty`, which the boundary and the owner allow, then also changes the
// list itself. Its summary already says that it changes its parameter, so it stays the same.
const RACK: &str = "class Rack:\n    def __init__(self):\n        self.items = []\n";
const RACK_OPS: &str = "def empty(xs):\n    xs.clear()\n\n\ndef refill(xs):\n    empty(xs)\n";
const RACK_VIEW: &str = "from pkg.rack import Rack\nfrom pkg.rack_ops import refill\n\n\ndef show(s: Rack):\n    refill(s.items)\n";
// Inherited writers (`MUT101`): a subclass in another module gains a cached property that an
// inherited method's write leaves stale, then overrides that method.
const NAMED: &str = "class Named:\n    def __init__(self, name):\n        self.name = name\n\n    def rename(self, name):\n        self.name = name\n";
const LABELED: &str = "from pkg.named import Named\n\n\nclass Labeled(Named):\n    pass\n";
const LABELED_CACHED: &str = "from functools import cached_property\n\nfrom pkg.named import Named\n\n\nclass Labeled(Named):\n    @cached_property\n    def label(self):\n        return self.name.upper()\n";
const PLANTER_SILENT: &str = "from pkg.nodes import Index, Node\nfrom pkg.relay import forward\n\n\ndef plant(index: Index, node: Node, kid):\n    forward(node, kid)\n";
// Two-phase initialization (`MUT101`): an `__init__` of a subclass in another module starts and
// stops calling a private writer of its base.
const SHEET: &str = "from functools import cached_property\n\n\nclass Sheet:\n    def __init__(self):\n        self.cells = []\n\n    @cached_property\n    def size(self):\n        return len(self.cells)\n\n    def _reset(self):\n        self.cells = []\n";
const GRID: &str = "from pkg.sheet import Sheet\n\n\nclass Grid(Sheet):\n    pass\n";
const GRID_RESETTING: &str = "from pkg.sheet import Sheet\n\n\nclass Grid(Sheet):\n    def __init__(self):\n        super().__init__()\n        self._reset()\n";
const GRID_INITIALIZING: &str = "from pkg.sheet import Sheet\n\n\nclass Grid(Sheet):\n    def __init__(self):\n        super().__init__()\n";
// What `MUT107` takes from a `__hash__` and an `__init__` in another module: whether the hash
// hands an attribute whole to `hash()` (an in-place change of it is then the value's own), which
// private helpers initialization runs through others, and whether a class between the writer and
// the hash defines `__eq__` alone (its instances cannot be hashed).
const STAMP: &str = "class Stamp:\n    def __init__(self, parts):\n        self.parts = parts\n        self.name = \"\"\n        self._load(parts)\n\n    def __hash__(self):\n        return hash((self.parts, self.name)) ^ len(self.parts)\n\n    def _load(self, parts):\n        self.loaded = True\n";
const STAMP_MIDDLE: &str = "from pkg.stamp import Stamp\n\n\nclass Middle(Stamp):\n    def _prepare(self, parts):\n        self._decode(parts)\n";
const STAMP_MIDDLE_EQ: &str = "from pkg.stamp import Stamp\n\n\nclass Middle(Stamp):\n    def __eq__(self, other):\n        return self is other\n\n    def _prepare(self, parts):\n        self._decode(parts)\n";
const STAMPED: &str = "from pkg.stamp_middle import Middle\n\n\nclass Stamped(Middle):\n    def add(self, part):\n        self.parts.append(part)\n\n    def _decode(self, parts):\n        self.name = str(parts)\n";

// Lazy initialization (`MUT101`): a cached property reads a connection only through a mixin
// helper in another module that initializes it lazily, so the lazy initialization of a
// subclass in a third module cannot find it unset after; the helper stops initializing it.
const CONN_MIXIN: &str = "class ConnMixin:\n    _conn = None\n\n    def _get_conn(self):\n        if self._conn is None:\n            self._conn = object()\n        return self._conn\n";
const CONN_MIXIN_READING: &str =
    "class ConnMixin:\n    _conn = None\n\n    def _get_conn(self):\n        return self._conn\n";
const CONN_CLIENT: &str = "from functools import cached_property\n\nfrom pkg.conn_mixin import ConnMixin\n\n\nclass Client(ConnMixin):\n    @cached_property\n    def db(self):\n        return self._get_conn()\n";
const CONN_POOLED: &str = "from pkg.conn_client import Client\n\n\nclass Pooled(Client):\n    def connect(self):\n        if self._conn is None:\n            self._conn = object()\n";

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
// A module-level list its own loop treats as fixed, and the module that may grow it.
const QUEUE_CFG: &str =
    "import httpx\n\nQUEUE = []\n\n\ndef run():\n    for item in QUEUE:\n        httpx.Client()\n";
const QUEUE_FEEDER: &str =
    "from pkg import queue_cfg\n\n\ndef peek():\n    return len(queue_cfg.QUEUE)\n";
const QUEUE_FEEDER_GROWING: &str =
    "from pkg import queue_cfg\n\n\ndef feed(items):\n    queue_cfg.QUEUE.extend(items)\n";
const SCHEMA: &str = "import sqlite3\n\nSQL = \"CREATE TABLE t (x)\"\n\n\ndef setup(conn: sqlite3.Connection, rows):\n    for row in rows:\n        conn.execute(SQL)\n";
const SCHEMA_READER: &str = "from pkg import schema\n\n\ndef peek():\n    return schema.SQL\n";
const SCHEMA_REBINDING: &str =
    "from pkg import schema\n\n\ndef reconfigure():\n    schema.SQL = \"SELECT 1\"\n";
const QUEUE_READER: &str = "import httpx\n\nfrom pkg.queue_cfg import QUEUE\n\n\ndef each():\n    for item in QUEUE:\n        httpx.Client()\n";
const QUEUE_MAPPER: &str = "import httpx\n\nfrom pkg.queue_cfg import QUEUE\n\n\ndef each():\n    return list(map(lambda item: httpx.Client(), QUEUE))\n";
const LEVELS_USER: &str = "from pkg.levels import LEVELS\n\n\ndef ranks(xs):\n    out = []\n    for x in xs:\n        out.append(LEVELS.index(x))\n    return out\n";
// A module body reads an imported name as a copy of its `Import`: its loop over the list is
// fixed while the list is.
const LEVELS_DOCS: &str =
    "import httpx\n\nfrom pkg.levels import LEVELS\n\nfor level in LEVELS:\n    httpx.Client()\n";
// A module-level tuple stays fixed whatever another module does with it (`chain` puts it in a
// list); a list another module hands to code that may grow it does not.
const STMTS: &str = "STATEMENTS = (\"create a\", \"create b\")\n";
const STMTS_LIST: &str = "STATEMENTS = [\"create a\", \"create b\"]\n";
const STMTS_CHAIN: &str = "from pkg.stmts import STATEMENTS\n\n\ndef chain():\n    out = []\n    out.append(STATEMENTS)\n    return out\n";
const STMTS_RUN: &str = "import httpx\n\nfrom pkg.stmts import STATEMENTS\n\n\ndef run():\n    for statement in STATEMENTS:\n        httpx.Client()\n";
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
// A helper that does not type its parameter, handed a protocol-typed object.
const VIEW_HANDED: &str = "from pkg.views import EntriesView\n\n\ndef add(obj, x):\n    obj.entries.append(x)\n\n\ndef fill_handed(view: EntriesView, x):\n    add(view, x)\n";
// The class body only declares names (no function changes): only the protocol's
// implementations change.
const TAGGED: &str =
    "from pkg.model import Registry\n\n\nclass TaggedRegistry(Registry):\n    tag: str = \"t\"\n";
const UNTAGGED: &str =
    "from pkg.model import Registry\n\n\nclass TaggedRegistry(Registry):\n    label: str = \"t\"\n";

// Stale cached properties (`MUT111`): a module reads a cached property of `Journal` (another
// module) whose writer leaves it stale, and reads it on a subclass in a third module whose
// override replaces the writer with one that only drops the cached value, then calls it through
// `super()` instead (the declarations stay the same).
const JOURNAL: &str = "from functools import cached_property\n\n\nclass Journal:\n    def __init__(self, entries):\n        self.entries = entries\n\n    @cached_property\n    def latest(self):\n        return self.entries[-1]\n\n    def refresh(self, entries):\n        self.entries = entries\n";
const JOURNAL_INVALIDATING: &str = "from functools import cached_property\n\n\nclass Journal:\n    def __init__(self, entries):\n        self.entries = entries\n\n    @cached_property\n    def latest(self):\n        return self.entries[-1]\n\n    def refresh(self, entries):\n        self.entries = entries\n        self.__dict__.pop(\"latest\", None)\n";
const SAFE_JOURNAL: &str = "from pkg.journal import Journal\n\n\nclass SafeJournal(Journal):\n    def refresh(self, entries):\n        self.__dict__.pop(\"latest\", None)\n";
const SAFE_JOURNAL_DELEGATING: &str = "from pkg.journal import Journal\n\n\nclass SafeJournal(Journal):\n    def refresh(self, entries):\n        super().refresh(list(entries))\n";
const HEADLINE: &str = "from pkg.journal import Journal\nfrom pkg.safe_journal import SafeJournal\n\n\ndef headline(journal: Journal):\n    return journal.latest\n\n\ndef safe_headline(journal: SafeJournal):\n    return journal.latest\n";

// Values new on one path and borrowed on another (`MUT112`): a caller two modules above a
// function that copies its argument (through a wrapper in a third module), and a class whose
// subclass in another module reads the list its `__init__` may borrow from the caller.
const OWN_PREPARE: &str =
    "def prepare(config):\n    if config is None:\n        return {}\n    return dict(config)\n";
const OWN_PREPARE_SAME: &str =
    "def prepare(config):\n    if config is None:\n        return {}\n    return config\n";
const OWN_WRAP: &str = "from pkg.own_prepare import prepare\n\n\ndef prepare_twice(config):\n    return prepare(config)\n";
const OWN_WRAP_NESTED: &str = "from pkg.own_prepare import prepare\n\n\ndef prepare_twice(config):\n    return prepare(prepare(config))\n";
const OWN_USE: &str = "from pkg.own_wrap import prepare_twice\n\n\ndef configure(config):\n    cfg = prepare_twice(config)\n    cfg[\"debug\"] = True\n    return cfg\n";
const OWN_BASE: &str =
    "class Box:\n    def __init__(self, items=None):\n        self.items = items or []\n";
const OWN_SUB: &str = "from pkg.own_base import Box\n\n\nclass Bag(Box):\n    def order(self):\n        return sorted(self.items)\n";
const OWN_SUB_SORTING: &str = "from pkg.own_base import Box\n\n\nclass Bag(Box):\n    def order(self):\n        self.items.sort()\n";
// Containers changed while iterated (`MUT113`): a generator kept on the instance iterates a dict
// that a subclass in another module starts changing (the change then moves a line down, the
// generator takes a snapshot and stops, the subclass hands the dict to a helper of a third module
// that changes it, and the helper stops changing it), and a private generator whose consumer in
// another module starts changing the dict in its loop, then changes another instance's dict
// instead, then keeps the generator, then only counts again, then steps it by hand before
// counting the rest, then counts again; then a helper there iterating the generator, whose
// result a function keeps, starts suspending inside its loop (its calls stay the same).
const SUBSCRIPTIONS: &str = "class Subscriptions:\n    def __init__(self):\n        self.by_node = {}\n        self._cursor = self._cycle()\n\n    def _cycle(self):\n        while True:\n            yield from self.by_node.values()\n\n    def next_subscriber(self):\n        return next(self._cursor)\n";
const SUB_DROP: &str = "from pkg.subscriptions import Subscriptions\n\n\nclass Dropping(Subscriptions):\n    def size(self):\n        return len(self.by_node)\n";
const SUB_DROP_CHANGING: &str = "from pkg.subscriptions import Subscriptions\n\n\nclass Dropping(Subscriptions):\n    def drop(self, node):\n        self.by_node.pop(node, None)\n";
const SUB_DROP_HANDING: &str = "from pkg.sub_ops import forget\nfrom pkg.subscriptions import Subscriptions\n\n\nclass Dropping(Subscriptions):\n    def drop(self, node):\n        forget(self.by_node, node)\n";
const SUB_OPS: &str = "def forget(table, node):\n    table.pop(node, None)\n";
const TALLY: &str = "class Tally:\n    def __init__(self):\n        self.counts = {}\n\n    def _walk(self):\n        for k in self.counts:\n            yield k\n\n    def remove(self, k):\n        del self.counts[k]\n";
const TALLY_USE: &str = "from pkg.tally import Tally\n\n\ndef total(t: Tally):\n    n = 0\n    for k in t._walk():\n        n += 1\n    return n\n";
const TALLY_COUNTING: &str = "from pkg.tally import Tally\n\n\ndef _counting(t: Tally):\n    n = 0\n    for k in t._walk():\n        n += 1\n    return n\n\n\ndef start(t: Tally):\n    t.cursor = _counting(t)\n";

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
        ("pkg/buf_read.py", BUF_READ),
        ("pkg/buf_use.py", BUF_USE),
        ("pkg/buf_default_use.py", BUF_DEFAULT_USE),
        ("pkg/guard_global.py", GUARD_GLOBAL),
        ("pkg/guard_global_use.py", GUARD_GLOBAL_USE),
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
        ("pkg/prep_mixin.py", PREP_MIXIN),
        ("pkg/prepared.py", PREPARED),
        ("pkg/prep_sub.py", PREP_SUB),
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
        ("pkg/own_prepare.py", OWN_PREPARE),
        ("pkg/own_wrap.py", OWN_WRAP),
        ("pkg/own_use.py", OWN_USE),
        ("pkg/own_base.py", OWN_BASE),
        ("pkg/own_sub.py", OWN_SUB),
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
        ("pkg/worker_ops.py", WORKER_OPS),
        ("pkg/rank_key.py", RANK_KEY),
        ("pkg/rank_user.py", RANK_USER),
        ("pkg/worker_view.py", WORKER_VIEW),
        ("pkg/feed.py", FEED),
        ("pkg/feed_user.py", FEED_USER),
        ("pkg/waits.py", WAITS),
        ("pkg/waiter.py", WAITER),
        ("pkg/fills.py", FILLS),
        ("pkg/drain.py", DRAIN),
        ("pkg/handoff.py", HANDOFF),
        ("pkg/levels.py", LEVELS),
        ("pkg/levels_user.py", LEVELS_USER),
        ("pkg/levels_docs.py", LEVELS_DOCS),
        ("pkg/stmts.py", STMTS),
        ("pkg/stmts_chain.py", STMTS_CHAIN),
        ("pkg/stmts_run.py", STMTS_RUN),
        ("pkg/queue_cfg.py", QUEUE_CFG),
        ("pkg/queue_feeder.py", QUEUE_FEEDER),
        ("pkg/queue_reader.py", QUEUE_READER),
        ("pkg/schema.py", SCHEMA),
        ("pkg/schema_reader.py", SCHEMA_READER),
        ("pkg/queue_mapper.py", QUEUE_MAPPER),
        ("pkg/sniff.py", SNIFF),
        ("pkg/sniff_more.py", SNIFF_MORE),
        ("pkg/sniff_user.py", SNIFF_USER),
        ("pkg/catalog.py", CATALOG),
        ("pkg/lookup.py", LOOKUP),
        ("pkg/tagger.py", TAGGER),
        ("pkg/glyphs.py", GLYPHS),
        ("pkg/glyph_user.py", GLYPH_USER),
        ("pkg/nodes.py", NODES),
        ("pkg/announce.py", ANNOUNCE),
        ("pkg/dials.py", DIALS),
        ("pkg/dial_ops.py", DIAL_OPS),
        ("pkg/dial_user.py", DIAL_USER),
        ("pkg/grow.py", GROW),
        ("pkg/graft.py", GRAFT),
        ("pkg/relay.py", RELAY),
        ("pkg/planter.py", PLANTER),
        ("pkg/kid_gen.py", KID_GEN),
        ("pkg/kid_user.py", KID_USER),
        ("pkg/kid_count.py", KID_COUNT),
        ("pkg/kid_stock.py", KID_STOCK),
        ("pkg/shelf.py", SHELF),
        ("pkg/rack.py", RACK),
        ("pkg/rack_ops.py", RACK_OPS),
        ("pkg/rack_view.py", RACK_VIEW),
        ("pkg/named.py", NAMED),
        ("pkg/labeled.py", LABELED),
        ("pkg/sheet.py", SHEET),
        ("pkg/grid.py", GRID),
        ("pkg/stamp.py", STAMP),
        ("pkg/stamp_middle.py", STAMP_MIDDLE),
        ("pkg/stamped.py", STAMPED),
        ("pkg/conn_mixin.py", CONN_MIXIN),
        ("pkg/conn_client.py", CONN_CLIENT),
        ("pkg/conn_pooled.py", CONN_POOLED),
        ("pkg/fill.py", FILL),
        ("pkg/fill_main.py", FILL_MAIN),
        ("pkg/views.py", VIEWS),
        ("pkg/view_handed.py", VIEW_HANDED),
        ("pkg/tagged.py", TAGGED),
        ("pkg/drive_a.py", DRIVE_A),
        ("pkg/drive_b.py", DRIVE_B),
        ("pkg/slow_leaf.py", SLOW_LEAF),
        ("pkg/worker.py", WORKER),
        ("pkg/worker_driver.py", WORKER_DRIVER),
        ("pkg/walk_a.py", WALK_A),
        ("pkg/walk_b.py", WALK_B),
        ("pkg/gate_net.py", GATE_NET),
        ("pkg/gate.py", GATE),
        ("pkg/mixin_commands.py", MIXIN_COMMANDS),
        ("pkg/sync_client.py", SYNC_CLIENT),
        ("pkg/async_client.py", ASYNC_CLIENT),
        ("pkg/async_user.py", ASYNC_USER),
        ("pkg/key_consts.py", KEY_CONSTS),
        ("pkg/key_writer.py", KEY_WRITER),
        ("pkg/alias_src.py", ALIAS_SRC),
        ("pkg/alias_use.py", ALIAS_USE),
        ("pkg/getter_src.py", GETTER_SRC),
        ("pkg/getter_use.py", GETTER_USE),
        ("pkg/proto.py", PROTO),
        ("pkg/proto_use.py", PROTO_USE),
        ("pkg/listing_base.py", LISTING_BASE),
        ("pkg/listing_cls.py", LISTING_CLS),
        ("pkg/listing_use.py", LISTING_USE),
        ("pkg/natives/__init__.py", ""),
        ("pkg/natives_use.py", NATIVES_USE),
        ("pkg/key_class.py", KEY_CLASS),
        ("pkg/key_class_reader.py", KEY_CLASS_READER),
        ("pkg/field_deco.py", FIELD_DECO),
        ("pkg/field_keys.py", FIELD_KEYS),
        ("pkg/field_reader.py", FIELD_READER),
        ("pkg/seal.py", SEAL),
        ("pkg/seal_peek.py", SEAL_PEEK),
        ("pkg/sealer.py", SEALER),
        ("pkg/reseal_helper.py", RESEAL_HELPER),
        ("pkg/reseal_use.py", RESEAL_USE),
        ("pkg/journal.py", JOURNAL),
        ("pkg/headline.py", HEADLINE),
        ("pkg/safe_journal.py", SAFE_JOURNAL),
        ("pkg/fresh_ops.py", FRESH_OPS),
        ("pkg/fresh_build.py", FRESH_BUILD),
        ("pkg/strict_ops.py", STRICT_OPS),
        ("pkg/strict_use.py", STRICT_USE),
        ("pkg/subscriptions.py", SUBSCRIPTIONS),
        ("pkg/sub_drop.py", SUB_DROP),
        ("pkg/sub_ops.py", SUB_OPS),
        ("pkg/tally.py", TALLY),
        ("pkg/tally_use.py", TALLY_USE),
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
    /// Close the document without saving it: its unsaved text is discarded.
    Close(&'static str),
    /// Write a file that is not open (created or changed outside the editor) and announce it.
    Write(&'static str, String),
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
        .absent(&[("pkg/views.py", "MUT201"), ("pkg/view_handed.py", "MUT201")]),
        Step::new(
            "the class implements the protocol again",
            vec![save("pkg/tagged.py", TAGGED)],
        )
        .present(&[("pkg/views.py", "MUT201"), ("pkg/view_handed.py", "MUT201")]),
    ]
}

/// The index across files: facts its callers decide (`sync_driven_only`), members of a call
/// cycle in other files, the catalog calls below an effect boundary, and the implementations a
/// mixin's method runs for a receiver of known class.
fn steps_effect_index() -> Vec<Step> {
    vec![
        Step::new(
            "a comment in a module whose coroutine another module drives by hand",
            vec![save("pkg/drive_a.py", &format!("# drivers\n{DRIVE_A}"))],
        )
        .absent(&[("pkg/drive_a.py", "EFF101"), ("pkg/drive_b.py", "EFF101")])
        .cutoff(),
        Step::new(
            "a blank line above the blocking call below a hand-driven coroutine",
            vec![save(
                "pkg/slow_leaf.py",
                &SLOW_LEAF.replace("    time.sleep", "\n    time.sleep"),
            )],
        )
        .absent(&[("pkg/worker.py", "EFF101")]),
        Step::new(
            "the coroutine's module is edited after its callee moved",
            vec![save("pkg/worker.py", &format!("{WORKER}# done\n"))],
        )
        .absent(&[("pkg/worker.py", "EFF101")]),
        Step::new(
            "a function calling a boundary's scope function in another module starts sleeping",
            vec![save(
                "pkg/walk_a.py",
                &WALK_A.replace("    if n > 0:", "    time.sleep(1)\n    if n > 0:"),
            )],
        )
        .present(&[("pkg/walk_b.py", "EFF201")]),
        Step::new(
            "the function stops sleeping",
            vec![save("pkg/walk_a.py", WALK_A)],
        )
        .absent(&[("pkg/walk_b.py", "EFF201")]),
        Step::new(
            "the first of two allowed catalog calls below a boundary becomes a forbidden one",
            vec![save(
                "pkg/gate_net.py",
                &GATE_NET.replacen("requests.get", "requests.put", 1),
            )],
        )
        .present(&[("pkg/gate.py", "EFF201")]),
        Step::new(
            "both calls are allowed again",
            vec![save("pkg/gate_net.py", GATE_NET)],
        )
        .absent(&[("pkg/gate.py", "EFF201")]),
        Step::new(
            "the sync client sharing a mixin with the async one starts blocking",
            vec![save(
                "pkg/sync_client.py",
                &format!(
                    "import time\n\n{}",
                    SYNC_CLIENT.replace("return args", "time.sleep(1)")
                ),
            )],
        )
        .absent(&[("pkg/async_user.py", "EFF101")]),
        Step::new(
            "the async client's implementation becomes a blocking sync one",
            vec![save(
                "pkg/async_client.py",
                &format!(
                    "import time\n\n{}",
                    ASYNC_CLIENT
                        .replace("async def", "def")
                        .replace("return args", "time.sleep(1)")
                ),
            )],
        )
        .present(&[("pkg/async_user.py", "EFF101")]),
        Step::new(
            "the async client is async again",
            vec![save("pkg/async_client.py", ASYNC_CLIENT)],
        )
        .absent(&[("pkg/async_user.py", "EFF101")]),
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
            "a function run on a worker thread starts a query per call",
            vec![Action::Save(
                "pkg/worker_ops.py",
                WORKER_OPS.replace("    return i\n", "    return session.execute(i)\n"),
            )],
        )
        .present(&[("pkg/worker_view.py", "EFF103")])
        .absent(&[("pkg/worker_view.py", "EFF101")]),
        Step::new(
            "the worker's query is removed",
            vec![save("pkg/worker_ops.py", WORKER_OPS)],
        )
        .absent(&[("pkg/worker_view.py", "EFF103")]),
        // A function a builtin calls once per item: its caller is re-checked when the
        // function in another module starts or stops querying.
        Step::new(
            "the sort key in another module starts a query per item",
            vec![Action::Save(
                "pkg/rank_key.py",
                RANK_KEY.replace("    return i\n", "    return session.execute(i)\n"),
            )],
        )
        .present(&[("pkg/rank_user.py", "EFF103")]),
        Step::new(
            "the sort key's query is removed",
            vec![save("pkg/rank_key.py", RANK_KEY)],
        )
        .absent(&[("pkg/rank_user.py", "EFF103")]),
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

/// What a task factory's argument from another module is (`EFF107`): the method it calls becomes
/// an `async def` and a plain `def` again, then gains a decorator that may replace it.
fn steps_task_arguments() -> Vec<Step> {
    vec![
        Step::new(
            "the method whose result is scheduled becomes an async def: a new task",
            vec![save("pkg/waits.py", WAITS_ASYNC)],
        )
        .present(&[("pkg/waiter.py", "EFF107")]),
        Step::new(
            "the method returns a future again",
            vec![save("pkg/waits.py", WAITS)],
        )
        .absent(&[("pkg/waiter.py", "EFF107")]),
        Step::new(
            "the method gains a decorator that may replace it",
            vec![save("pkg/waits.py", WAITS_DECORATED)],
        )
        .present(&[("pkg/waiter.py", "EFF107")]),
        Step::new(
            "the decorator is removed",
            vec![save("pkg/waits.py", WAITS)],
        )
        .absent(&[("pkg/waiter.py", "EFF107")]),
    ]
}

/// `OPT112`'s small displays in other files, read through the declarations
/// (`ModuleDecls::small_globals`, `ClassDecl::attr_displays`): the user of a module-level list
/// (also a module body's loop over it) is re-checked when another function starts growing the
/// list, and the user of an attribute when a subclass in a third module starts growing it;
/// `EFF102`'s fixed loops over a module-level list another module grows
/// (`ModuleDecls::grown_globals`), also as the items of a builtin calling a function once per
/// item, and over a tuple, which nothing grows.
fn steps_small_displays() -> Vec<Step> {
    vec![
        Step::new(
            "a module-level list another file scans in a loop starts growing",
            vec![save("pkg/levels.py", LEVELS_GROWN)],
        )
        .present(&[
            ("pkg/levels_user.py", "OPT112"),
            ("pkg/levels_docs.py", "EFF102"),
        ]),
        Step::new(
            "the module-level list is a small display again",
            vec![save("pkg/levels.py", LEVELS)],
        )
        .absent(&[
            ("pkg/levels_user.py", "OPT112"),
            ("pkg/levels_docs.py", "EFF102"),
        ]),
        Step::new(
            "a module-level tuple another module puts in a list becomes a list",
            vec![save("pkg/stmts.py", STMTS_LIST)],
        )
        .present(&[("pkg/stmts_run.py", "EFF102")]),
        Step::new("it is a tuple again", vec![save("pkg/stmts.py", STMTS)])
            .absent(&[("pkg/stmts_run.py", "EFF102")]),
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
        // `ModuleDecls::grown_globals`: the module owning the list and a third module reading
        // it are re-checked when another module starts or stops growing it.
        Step::new(
            "another module starts growing a module-level list",
            vec![save("pkg/queue_feeder.py", QUEUE_FEEDER_GROWING)],
        )
        .present(&[
            ("pkg/queue_cfg.py", "EFF102"),
            ("pkg/queue_reader.py", "EFF102"),
            ("pkg/queue_mapper.py", "EFF102"),
        ]),
        Step::new(
            "it only reads the list again",
            vec![save("pkg/queue_feeder.py", QUEUE_FEEDER)],
        )
        .absent(&[
            ("pkg/queue_cfg.py", "EFF102"),
            ("pkg/queue_reader.py", "EFF102"),
            ("pkg/queue_mapper.py", "EFF102"),
        ]),
        // `ModuleDecls::rebound_globals`: a DDL constant is a query statement no longer once
        // another module rebinds it, and DDL again once that module only reads it.
        Step::new(
            "another module rebinds a module's DDL constant",
            vec![save("pkg/schema_reader.py", SCHEMA_REBINDING)],
        )
        .present(&[("pkg/schema.py", "EFF103")]),
        Step::new(
            "it only reads the constant again",
            vec![save("pkg/schema_reader.py", SCHEMA_READER)],
        )
        .absent(&[("pkg/schema.py", "EFF103")]),
    ]
}

/// Constant keys, objects under construction, and the strict modes. A key constant is a
/// declaration another module's summary resolves names through: one without `Final` stops
/// being one when a third module rebinds it, and a class constant when a subclass in another
/// module defines the name again or lists a base defining it first, or code rebinds it on the
/// class or a subclass, and a `Final` one when a decorator in another module building its class
/// becomes a `dataclass_transform` (a project-wide fact); whether a helper keeps the object its caller hands it is a summary fact
/// of the helper; whether a parameter is typed, and with which class, is a fact of
/// the callers the strict modes walk (`MUT205` writes, `MUT206` reads, `MUT207` calls), and
/// whether that class defines a method a call target naming no class names is a declaration of
/// the class's module.
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
            "the key constant loses `Final`: bound once, it still names the attribute",
            vec![Action::Save(
                "pkg/key_consts.py",
                KEY_CONSTS.replace("KEY: Final = ", "KEY = "),
            )],
        )
        .present(&[("pkg/key_writer.py", "MUT201")]),
        Step::new(
            "another module rebinds the constant",
            vec![Action::Create("pkg/key_rebind.py", KEY_REBIND.to_owned())],
        )
        .absent(&[("pkg/key_writer.py", "MUT201")]),
        Step::new(
            "the module rebinding it is deleted",
            vec![Action::Delete("pkg/key_rebind.py")],
        )
        .present(&[("pkg/key_writer.py", "MUT201")]),
        Step::new(
            "the key constant is `Final` again",
            vec![save("pkg/key_consts.py", KEY_CONSTS)],
        )
        .present(&[("pkg/key_writer.py", "MUT201")]),
        Step::new(
            "a subclass in another module defines the class constant again",
            vec![Action::Create(
                "pkg/key_subclass.py",
                KEY_SUBCLASS.to_owned(),
            )],
        )
        .absent(&[
            ("pkg/key_class.py", "MUT201"),
            ("pkg/key_class_reader.py", "MUT201"),
        ]),
        Step::new(
            "the subclass is deleted",
            vec![Action::Delete("pkg/key_subclass.py")],
        )
        .present(&[
            ("pkg/key_class.py", "MUT201"),
            ("pkg/key_class_reader.py", "MUT201"),
        ]),
        Step::new(
            "another module rebinds the `ClassVar` constant on its class",
            vec![Action::Create(
                "pkg/key_class_rebind.py",
                KEY_CLASS_REBIND.to_owned(),
            )],
        )
        .absent(&[
            ("pkg/key_class.py", "MUT201"),
            ("pkg/key_class_reader.py", "MUT201"),
        ]),
        Step::new(
            "the module rebinding it is deleted",
            vec![Action::Delete("pkg/key_class_rebind.py")],
        )
        .present(&[
            ("pkg/key_class.py", "MUT201"),
            ("pkg/key_class_reader.py", "MUT201"),
        ]),
        Step::new(
            "a subclass in another module is given its own value of the class constant",
            vec![Action::Create(
                "pkg/key_subclass_rebind.py",
                KEY_SUBCLASS_REBIND.to_owned(),
            )],
        )
        .absent(&[
            ("pkg/key_class.py", "MUT201"),
            ("pkg/key_class_reader.py", "MUT201"),
        ]),
        Step::new(
            "the subclass storing it is deleted",
            vec![Action::Delete("pkg/key_subclass_rebind.py")],
        )
        .present(&[
            ("pkg/key_class.py", "MUT201"),
            ("pkg/key_class_reader.py", "MUT201"),
        ]),
        Step::new(
            "a reader of the class constant is edited, its declarations unchanged",
            vec![Action::Save(
                "pkg/key_class_reader.py",
                format!("{KEY_CLASS_READER}# The class constant names the attribute.\n"),
            )],
        )
        .present(&[("pkg/key_class_reader.py", "MUT201")])
        .cutoff(),
        Step::new(
            "a subclass in another module lists a base defining the name before the class",
            vec![Action::Create(
                "pkg/key_other_first.py",
                KEY_OTHER_FIRST.to_owned(),
            )],
        )
        .absent(&[
            ("pkg/key_class.py", "MUT201"),
            ("pkg/key_class_reader.py", "MUT201"),
        ]),
        Step::new(
            "the subclass listing the other base is deleted",
            vec![Action::Delete("pkg/key_other_first.py")],
        )
        .present(&[
            ("pkg/key_class.py", "MUT201"),
            ("pkg/key_class_reader.py", "MUT201"),
        ]),
        Step::new(
            "the decorator building the class becomes a `dataclass_transform`",
            vec![save("pkg/field_deco.py", FIELD_DECO_TRANSFORM)],
        )
        .present(&[("pkg/field_reader.py", "MUT205")])
        .absent(&[("pkg/field_reader.py", "MUT201")]),
        Step::new(
            "the decorator is a plain function again",
            vec![save("pkg/field_deco.py", FIELD_DECO)],
        )
        .present(&[("pkg/field_reader.py", "MUT201")])
        .absent(&[("pkg/field_reader.py", "MUT205")]),
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
        Step::new(
            "a caller in another module binds the helpers' object to another class",
            vec![Action::Create("pkg/seal_use.py", SEAL_USE.to_owned())],
        )
        .absent(&[
            ("pkg/seal_peek.py", "MUT206"),
            ("pkg/seal_peek.py", "MUT207"),
        ]),
        Step::new(
            "the caller binds a guarded object",
            vec![Action::Save(
                "pkg/seal_use.py",
                SEAL_USE
                    .replace("def use(p: Plain):", "def use(p: Seal):")
                    .replace(
                        "import crack, peek\n",
                        "import crack, peek\nfrom pkg.seal import Seal\n",
                    ),
            )],
        )
        .present(&[
            ("pkg/seal_peek.py", "MUT206"),
            ("pkg/seal_peek.py", "MUT207"),
        ]),
        Step::new(
            "the caller binds the other class again",
            vec![save("pkg/seal_use.py", SEAL_USE)],
        )
        .absent(&[
            ("pkg/seal_peek.py", "MUT206"),
            ("pkg/seal_peek.py", "MUT207"),
        ]),
        Step::new(
            "the caller is deleted",
            vec![Action::Delete("pkg/seal_use.py")],
        )
        .present(&[
            ("pkg/seal_peek.py", "MUT206"),
            ("pkg/seal_peek.py", "MUT207"),
        ]),
        Step::new(
            "the class bound to the helper's object renames the method a target names",
            vec![save(
                "pkg/sealer.py",
                &SEALER.replace("def reseal(self):", "def close(self):"),
            )],
        )
        .absent(&[("pkg/reseal_helper.py", "MUT207")]),
        Step::new(
            "the class defines the method again",
            vec![save("pkg/sealer.py", SEALER)],
        )
        .present(&[("pkg/reseal_helper.py", "MUT207")]),
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
fn steps_aliases_and_getters() -> Vec<Step> {
    vec![
        Step::new(
            "the module alias is bound to another function",
            vec![save("pkg/alias_src.py", "_set = print\n")],
        )
        .absent(&[("pkg/alias_use.py", "MUT201")]),
        Step::new(
            "the module alias names `object.__setattr__` again",
            vec![save("pkg/alias_src.py", ALIAS_SRC)],
        )
        .present(&[("pkg/alias_use.py", "MUT201")]),
        Step::new(
            "another module rebinds the alias",
            vec![Action::Create(
                "pkg/alias_rebind.py",
                ALIAS_REBIND.to_owned(),
            )],
        )
        .absent(&[("pkg/alias_use.py", "MUT201")]),
        Step::new(
            "the module rebinding it is deleted",
            vec![Action::Delete("pkg/alias_rebind.py")],
        )
        .present(&[("pkg/alias_use.py", "MUT201")]),
        Step::new(
            "the getter returns a copy of the container",
            vec![Action::Save(
                "pkg/getter_src.py",
                GETTER_SRC.replace("return r.entries", "return list(r.entries)"),
            )],
        )
        .absent(&[("pkg/getter_use.py", "MUT201")]),
        Step::new(
            "the getter returns the container again",
            vec![save("pkg/getter_src.py", GETTER_SRC)],
        )
        .present(&[("pkg/getter_use.py", "MUT201")]),
        Step::new(
            "the subclass binds the getter's name as a class attribute",
            vec![save(
                "pkg/listing_cls.py",
                &LISTING_CLS.replace("    pass\n", "    listing = []\n"),
            )],
        )
        .absent(&[("pkg/listing_use.py", "MUT201")]),
        Step::new(
            "the subclass binds the name no more",
            vec![save("pkg/listing_cls.py", LISTING_CLS)],
        )
        .present(&[("pkg/listing_use.py", "MUT201")]),
        Step::new(
            "the package re-exports a project function under the catalogued name",
            vec![
                Action::Create(
                    "pkg/natives/impl.py",
                    "def fetch():\n    return None\n".to_owned(),
                ),
                save(
                    "pkg/natives/__init__.py",
                    "from pkg.natives.impl import fetch\n",
                ),
            ],
        )
        .absent(&[("pkg/natives_use.py", "EFF101")]),
        Step::new(
            "the package binds the name no more",
            vec![save("pkg/natives/__init__.py", "")],
        )
        .present(&[("pkg/natives_use.py", "EFF101")]),
    ]
}

fn steps_protocol_reads() -> Vec<Step> {
    vec![
        Step::new(
            "the protocol no longer declares the attribute read through it",
            vec![Action::Save(
                "pkg/proto.py",
                PROTO.replace("    secret: str\n", "    size: int\n"),
            )],
        )
        .present(&[("pkg/proto_use.py", "MUT203")]),
        Step::new(
            "the protocol declares the attribute again",
            vec![save("pkg/proto.py", PROTO)],
        )
        .absent(&[("pkg/proto_use.py", "MUT203")]),
    ]
}

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
            "a cached object's method in another module stops filling a memo and records",
            vec![save("pkg/glyphs.py", GLYPHS_RECORDING)],
        )
        .present(&[("pkg/glyph_user.py", "MUT202")]),
        Step::new(
            "the method fills its memo again",
            vec![save("pkg/glyphs.py", GLYPHS)],
        )
        .absent(&[("pkg/glyph_user.py", "MUT202")]),
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
        Step::new(
            "a helper in another module starts changing its untyped argument in place",
            vec![save("pkg/kid_count.py", KID_COUNT_APPENDING)],
        )
        .present(&[("pkg/kid_stock.py", "MUT109")]),
        Step::new(
            "the helper only reads its argument again",
            vec![save("pkg/kid_count.py", KID_COUNT)],
        )
        .absent(&[("pkg/kid_stock.py", "MUT109")]),
    ]
}

/// An `invalidation` target names a class by its module (`pkg.dials:*.level`): the writes of a
/// class that moves to another module behind a re-export are no longer its target, directly or
/// through a helper (`MUT109`).
fn steps_module_patterns() -> Vec<Step> {
    vec![
        Step::new(
            "a class a module pattern names moves to another module behind a re-export",
            vec![
                Action::Write("pkg/dials_impl.py", DIALS_IMPL.to_owned()),
                save("pkg/dials.py", DIALS_REEXPORT),
            ],
        )
        .absent(&[("pkg/dial_user.py", "MUT109")]),
        Step::new("the class moves back", vec![save("pkg/dials.py", DIALS)])
            .present(&[("pkg/dial_user.py", "MUT109")]),
    ]
}

/// A helper handed a typed field starts changing it itself, besides through the allowed
/// callee, while its final summary stays the same: the writes the boundary and the owner
/// contract count are decided from the helpers' own changes (`EFF201`, `MUT201`).
fn steps_indirect_helpers() -> Vec<Step> {
    let changing = RACK_OPS.replace("    empty(xs)\n", "    xs.append(1)\n    empty(xs)\n");
    vec![
        Step::new(
            "a helper handed a field also changes it outside the allowed callee",
            vec![Action::Save("pkg/rack_ops.py", changing)],
        )
        .present(&[
            ("pkg/rack_view.py", "EFF201"),
            ("pkg/rack_view.py", "MUT201"),
        ]),
        Step::new(
            "the helper leaves the change to the allowed callee again",
            vec![save("pkg/rack_ops.py", RACK_OPS)],
        )
        .absent(&[
            ("pkg/rack_view.py", "EFF201"),
            ("pkg/rack_view.py", "MUT201"),
        ]),
    ]
}

/// A class's family includes the methods it inherits: the inherited writer's report appears in
/// the base class's file when a subclass in another module starts caching what it writes, and
/// goes away when the subclass overrides the writer. A private writer of the base that a
/// subclass's `__init__` calls initializes in two phases. A lazy initialization in a subclass
/// follows whether the helper of a base class's mixin, the only reader, initializes lazily.
fn steps_inherited_writers() -> Vec<Step> {
    let overriding = format!(
        "{LABELED_CACHED}\n    def rename(self, name):\n        self.name = name\n        del self.label\n"
    );
    vec![
        Step::new(
            "a subclass in another module caches what an inherited method writes",
            vec![save("pkg/labeled.py", LABELED_CACHED)],
        )
        .present(&[("pkg/named.py", "MUT101")]),
        Step::new(
            "the subclass overrides the writer and invalidates the cache",
            vec![Action::Save("pkg/labeled.py", overriding)],
        )
        .absent(&[("pkg/named.py", "MUT101"), ("pkg/labeled.py", "MUT101")]),
        Step::new(
            "the subclass no longer caches",
            vec![save("pkg/labeled.py", LABELED)],
        )
        .absent(&[("pkg/named.py", "MUT101")]),
        Step::new(
            "an `__init__` of a subclass in another module calls the base's private writer",
            vec![save("pkg/grid.py", GRID_RESETTING)],
        )
        .absent(&[("pkg/sheet.py", "MUT101")]),
        Step::new(
            "the subclass's `__init__` stops calling it",
            vec![save("pkg/grid.py", GRID_INITIALIZING)],
        )
        .present(&[("pkg/sheet.py", "MUT101")]),
        Step::new(
            "the mixin helper a cached property reads through stops initializing lazily",
            vec![save("pkg/conn_mixin.py", CONN_MIXIN_READING)],
        )
        .present(&[("pkg/conn_pooled.py", "MUT101")]),
        Step::new(
            "the helper initializes lazily again",
            vec![save("pkg/conn_mixin.py", CONN_MIXIN)],
        )
        .absent(&[
            ("pkg/conn_pooled.py", "MUT101"),
            ("pkg/conn_mixin.py", "MUT101"),
        ]),
    ]
}

/// `MUT107`: what a hash and an initializer in other modules decide for a writer in a subclass:
/// the hash starts hashing the changed attribute whole, the base's private helper starts reaching
/// the subclass's writer from `__init__` through a helper of the class in between (then a public
/// method calls that helper too, so the writer runs after initialization), and that class defines
/// `__eq__` alone.
fn steps_hash_writers() -> Vec<Step> {
    let whole = STAMP.replace(" ^ len(self.parts)", "");
    let loading = whole.replace("self.loaded = True", "self._prepare(parts)");
    let refreshing =
        format!("{loading}\n    def refresh(self, parts):\n        self._prepare(parts)\n");
    vec![
        Step::new(
            "the hash in another module hashes the changed list whole",
            vec![Action::Save("pkg/stamp.py", whole)],
        )
        .present(&[("pkg/stamped.py", "MUT107")]),
        Step::new(
            "initialization reaches the writer through private helpers in two other modules",
            vec![Action::Save("pkg/stamp.py", loading)],
        )
        .absent(&[("pkg/stamped.py", "MUT107")]),
        Step::new(
            "a public method there also calls the helper in between",
            vec![Action::Save("pkg/stamp.py", refreshing)],
        )
        .present(&[("pkg/stamped.py", "MUT107")]),
        Step::new(
            "the hash reads the list's length again",
            vec![save("pkg/stamp.py", STAMP)],
        )
        .present(&[("pkg/stamped.py", "MUT107")]),
        Step::new(
            "a class in another module between them defines `__eq__` alone",
            vec![save("pkg/stamp_middle.py", STAMP_MIDDLE_EQ)],
        )
        .absent(&[("pkg/stamped.py", "MUT107")]),
        Step::new(
            "the class in between stops defining it",
            vec![save("pkg/stamp_middle.py", STAMP_MIDDLE)],
        )
        .present(&[("pkg/stamped.py", "MUT107")]),
    ]
}

/// `MUT112`: what a function returns reaches its callers through a wrapper in another module
/// (`EffectSummary::return_origins`), also when the wrapper hands it a call's result
/// (`ReturnOrigins::returned`), and a store in `self.<attr>` follows the in-place changes
/// that the methods of its class family make in other modules (`heki_rules::family_readers`).
fn steps_mut112() -> Vec<Step> {
    vec![
        // The summaries carry the facts of `MUT112` and `MUT113` only while a rule reads them
        // (`OptInFacts`): a configuration selecting them again re-indexes with the facts.
        Step::new(
            "MUT112 and MUT113 are no longer selected, and the function two modules below stops copying its argument",
            vec![
                Action::Configure(CONFIG.replace(", \"MUT112\", \"MUT113\"]", "]")),
                save("pkg/own_prepare.py", OWN_PREPARE_SAME),
            ],
        )
        .absent(&[("pkg/own_use.py", "MUT112")]),
        Step::new(
            "MUT112 and MUT113 are selected again",
            vec![Action::Configure(CONFIG.to_owned())],
        )
        .present(&[("pkg/own_use.py", "MUT112")]),
        Step::new(
            "the function two modules below stops copying its argument",
            vec![save("pkg/own_prepare.py", OWN_PREPARE_SAME)],
        )
        .present(&[("pkg/own_use.py", "MUT112")]),
        Step::new(
            "the function copies its argument again",
            vec![save("pkg/own_prepare.py", OWN_PREPARE)],
        )
        .absent(&[("pkg/own_use.py", "MUT112")]),
        Step::new(
            "the wrapper hands the function the result of another call of it",
            vec![save("pkg/own_wrap.py", OWN_WRAP_NESTED)],
        )
        .absent(&[("pkg/own_use.py", "MUT112")]),
        Step::new(
            "the function under the nested call stops copying its argument",
            vec![save("pkg/own_prepare.py", OWN_PREPARE_SAME)],
        )
        .present(&[("pkg/own_use.py", "MUT112")]),
        Step::new(
            "the function copies its argument again, and the wrapper calls it once",
            vec![
                save("pkg/own_prepare.py", OWN_PREPARE),
                save("pkg/own_wrap.py", OWN_WRAP),
            ],
        )
        .absent(&[("pkg/own_use.py", "MUT112")]),
        Step::new(
            "a subclass method in another module starts sorting the stored list in place",
            vec![save("pkg/own_sub.py", OWN_SUB_SORTING)],
        )
        .present(&[("pkg/own_base.py", "MUT112")]),
        Step::new(
            "the subclass method only reads the list again",
            vec![save("pkg/own_sub.py", OWN_SUB)],
        )
        .absent(&[("pkg/own_base.py", "MUT112")]),
    ]
}

/// `MUT111`: the readers of a cached property follow its writers in other modules (the stale
/// writes of a file, `heki_rules::stale_writes_by_file`): an invalidation added and removed, the
/// writer moving, and a subclass's override that starts and stops reaching the writer through
/// `super()` (`safe_headline` is reported only in between; `headline` stays).
fn steps_mut111() -> Vec<Step> {
    let moved = JOURNAL.replace(
        "    def refresh",
        "    # Replace every entry.\n    def refresh",
    );
    vec![
        Step::new(
            "a reader in another module of a cached property left stale",
            Vec::new(),
        )
        .present(&[("pkg/headline.py", "MUT111"), ("pkg/journal.py", "MUT101")]),
        Step::new(
            "the writer invalidates the cached property",
            vec![save("pkg/journal.py", JOURNAL_INVALIDATING)],
        )
        .absent(&[("pkg/headline.py", "MUT111"), ("pkg/journal.py", "MUT101")]),
        Step::new(
            "the writer stops invalidating it, one line lower",
            vec![Action::Save("pkg/journal.py", moved)],
        )
        .present(&[("pkg/headline.py", "MUT111")]),
        Step::new(
            "the subclass's override reaches the writer through `super()`",
            vec![save("pkg/safe_journal.py", SAFE_JOURNAL_DELEGATING)],
        )
        .present(&[("pkg/headline.py", "MUT111")]),
        Step::new(
            "the override replaces the writer again",
            vec![save("pkg/safe_journal.py", SAFE_JOURNAL)],
        )
        .present(&[("pkg/headline.py", "MUT111")]),
    ]
}

/// `MUT113`: a generator's report follows the methods of its class family in other modules that
/// change the container it iterates (`heki_rules::family_readers`), where they change it (the
/// places other files show, `shown_places`), and what its callers in other modules do with it
/// (`EffectSummary::result_iterations`, `CallSite::result_held`).
fn steps_mut113() -> Vec<Step> {
    let moved = SUB_DROP_CHANGING.replace(
        "        self.by_node.pop",
        "        # Forget the node.\n        self.by_node.pop",
    );
    let snapshot = SUBSCRIPTIONS.replace(
        "yield from self.by_node.values()",
        "yield from list(self.by_node.values())",
    );
    let reading = SUB_OPS.replace("    table.pop(node, None)", "    return table.get(node)");
    let pruning = TALLY_USE.replace("        n += 1\n", "        t.remove(k)\n");
    let pruning_another = TALLY_USE
        .replace("def total(t: Tally):", "def total(t: Tally, other: Tally):")
        .replace("        n += 1\n", "        other.remove(k)\n");
    let suspending =
        TALLY_COUNTING.replace("        n += 1\n", "        n += 1\n        yield n\n");
    let keeping = TALLY_USE.replace(
        "    n = 0\n    for k in t._walk():\n        n += 1\n    return n\n",
        "    t.cursor = t._walk()\n    return 0\n",
    );
    let stepping = TALLY_USE.replace(
        "    for k in t._walk():\n",
        "    g = t._walk()\n    next(g)\n    for k in g:\n",
    );
    vec![
        Step::new(
            "a subclass in another module changes the dict a kept generator iterates",
            vec![save("pkg/sub_drop.py", SUB_DROP_CHANGING)],
        )
        .present(&[("pkg/subscriptions.py", "MUT113")]),
        Step::new(
            "a comment above the subclass's change moves it a line down",
            vec![Action::Save("pkg/sub_drop.py", moved)],
        )
        .present(&[("pkg/subscriptions.py", "MUT113")]),
        Step::new(
            "the generator iterates a snapshot",
            vec![Action::Save("pkg/subscriptions.py", snapshot)],
        )
        .absent(&[("pkg/subscriptions.py", "MUT113")]),
        Step::new(
            "the generator iterates the dict itself again",
            vec![save("pkg/subscriptions.py", SUBSCRIPTIONS)],
        )
        .present(&[("pkg/subscriptions.py", "MUT113")]),
        Step::new(
            "the subclass hands the dict to a helper of a third module that changes it",
            vec![save("pkg/sub_drop.py", SUB_DROP_HANDING)],
        )
        .present(&[("pkg/subscriptions.py", "MUT113")]),
        Step::new(
            "the helper only reads the dict",
            vec![Action::Save("pkg/sub_ops.py", reading)],
        )
        .absent(&[("pkg/subscriptions.py", "MUT113")]),
        Step::new(
            "a loop in another module over a private generator changes the dict it iterates",
            vec![Action::Save("pkg/tally_use.py", pruning)],
        )
        .present(&[("pkg/tally.py", "MUT113")]),
        Step::new(
            "the loop removes the keys from another tally instead",
            vec![Action::Save("pkg/tally_use.py", pruning_another)],
        )
        .absent(&[("pkg/tally.py", "MUT113")]),
        Step::new(
            "the other module keeps the generator instead",
            vec![Action::Save("pkg/tally_use.py", keeping)],
        )
        .present(&[("pkg/tally.py", "MUT113")]),
        Step::new(
            "the other module only counts the items again",
            vec![save("pkg/tally_use.py", TALLY_USE)],
        )
        .absent(&[("pkg/tally.py", "MUT113")]),
        Step::new(
            "the other module steps the generator by hand before counting the rest",
            vec![Action::Save("pkg/tally_use.py", stepping)],
        )
        .present(&[("pkg/tally.py", "MUT113")]),
        Step::new(
            "the other module counts every item again",
            vec![save("pkg/tally_use.py", TALLY_USE)],
        )
        .absent(&[("pkg/tally.py", "MUT113")]),
        Step::new(
            "a helper iterates the generator and a function keeps the helper's result",
            vec![save("pkg/tally_use.py", TALLY_COUNTING)],
        )
        .absent(&[("pkg/tally.py", "MUT113")]),
        Step::new(
            "the helper suspends inside its loop",
            vec![Action::Save("pkg/tally_use.py", suspending)],
        )
        .present(&[("pkg/tally.py", "MUT113")]),
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
            ("pkg/buf_use.py", "EFF101"),
            ("pkg/buf_default_use.py", "EFF101"),
            ("pkg/guard_global_use.py", "EFF102"),
            ("pkg/fill.py", "MUT201"),
            ("pkg/fill_main.py", "MUT201"),
            ("pkg/views.py", "MUT201"),
            ("pkg/alias_use.py", "MUT201"),
            ("pkg/getter_use.py", "MUT201"),
            ("pkg/listing_use.py", "MUT201"),
            ("pkg/natives_use.py", "EFF101"),
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
            "the connection class connects in a cached property",
            vec![save("pkg/guard_conn.py", GUARD_CONN_CACHED)],
        )
        .absent(&[("pkg/guard_conn_use.py", "EFF102")]),
        Step::new(
            "the cached property becomes a plain property",
            vec![save(
                "pkg/guard_conn.py",
                &GUARD_CONN_CACHED.replace("@cached_property", "@property"),
            )],
        )
        .present(&[("pkg/guard_conn_use.py", "EFF102")]),
        Step::new(
            "the connection class connects on every call again",
            vec![save("pkg/guard_conn.py", GUARD_CONN)],
        )
        .present(&[("pkg/guard_conn_use.py", "EFF102")]),
        Step::new(
            "a reader in another file gains the `hasattr` test a buffer argument decides",
            vec![save("pkg/buf_read.py", BUF_READ_HASATTR)],
        )
        .present(&[("pkg/buf_default_use.py", "EFF101")])
        .absent(&[("pkg/buf_use.py", "EFF101")]),
        Step::new(
            "the reader tests an attribute a buffer may lack",
            vec![save(
                "pkg/buf_read.py",
                &BUF_READ_HASATTR.replace("\"read\")", "\"name\")"),
            )],
        )
        .present(&[("pkg/buf_use.py", "EFF101")]),
        Step::new(
            "the reader rebinds a missing argument to a buffer before its `isinstance` test",
            vec![save("pkg/buf_read.py", BUF_READ_REBINDING)],
        )
        .absent(&[
            ("pkg/buf_use.py", "EFF101"),
            ("pkg/buf_default_use.py", "EFF101"),
        ]),
        Step::new(
            "the reader loses its tests",
            vec![save("pkg/buf_read.py", BUF_READ)],
        )
        .present(&[
            ("pkg/buf_use.py", "EFF101"),
            ("pkg/buf_default_use.py", "EFF101"),
        ]),
        Step::new(
            "a helper in another file keeps its connection in a module global",
            vec![save(
                "pkg/guard_global.py",
                &GUARD_GLOBAL.replace(
                    "    _conn = urlopen",
                    "    if _conn is None:\n        _conn = urlopen",
                ),
            )],
        )
        .absent(&[("pkg/guard_global_use.py", "EFF102")]),
        Step::new(
            "the helper connects on every call again",
            vec![save("pkg/guard_global.py", GUARD_GLOBAL)],
        )
        .present(&[("pkg/guard_global_use.py", "EFF102")]),
    ];
    // 4. `MUT108`, `MUT110`, then where bodies run.
    steps.extend(steps_mut108());
    steps.extend(steps_mut110());
    steps.extend(steps_aliases_and_getters());
    steps.extend(steps_protocol_reads());
    steps.extend(steps_body_runs());
    steps.extend(steps_task_arguments());
    steps.extend(steps_small_displays());
    steps.extend(steps_mut202_mut109());
    steps.extend(steps_module_patterns());
    steps.extend(steps_indirect_helpers());
    steps.extend(steps_inherited_writers());
    steps.extend(steps_hash_writers());
    steps.extend(steps_mut111());
    steps.extend(steps_mut112());
    steps.extend(steps_mut113());
    steps.extend(steps_receiver_typing());
    steps.extend(steps_effect_index());
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
            "the configuration gains a write owner with a message",
            vec![Action::Configure(format!(
                "{CONFIG}\n[[tool.heki.write-owners]]\nname = \"boxes\"\ntargets = [\"Box.items\"]\nmessage = \"fill boxes in their constructor\"\n"
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
                "extend-select = [\"MUT101\", \"MUT108\", \"MUT111\", \"EFF107\", \"MUT112\", \"MUT113\"]",
                "extend-select = [\"MUT101\", \"MUT108\", \"MUT111\", \"EFF107\", \"MUT112\", \"MUT113\"]\nselect = [\"NOPE1\"]",
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
            "the base class gets a metaclass of its own, which may give its subclasses descriptors",
            vec![save(
                "pkg/base.py",
                &BASE.replace(
                    "class Base:",
                    "class Meta(type):\n    pass\n\n\nclass Base(metaclass=Meta):",
                ),
            )],
        )
        .absent(&[("pkg/derived.py", "MUT101")]),
        Step::new(
            "the base class is created by `type` again",
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
            "a mixin helper whose result the cached property discards starts writing what it returns",
            vec![save("pkg/prep_mixin.py", PREP_MIXIN_WRITING)],
        )
        .present(&[("pkg/prep_sub.py", "MUT101")]),
        Step::new(
            "the helper stops writing it",
            vec![save("pkg/prep_mixin.py", PREP_MIXIN)],
        )
        .absent(&[("pkg/prep_sub.py", "MUT101"), ("pkg/prep_mixin.py", "MUT101")]),
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
    certainty: String,
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
        let range = cli_range(p, &file, &d["span"]);
        let help = d["help"]
            .as_str()
            .map(|h| (range.clone(), format!("help: {h}")));
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
            .chain(help)
            .collect();
        by_file.entry(file.clone()).or_default().push(Diag {
            range,
            code: d["rule_code"].as_str().unwrap().to_owned(),
            severity: match d["severity"].as_str().unwrap() {
                "error" => 1,
                "warning" => 2,
                other => panic!("severity {other}"),
            },
            certainty: d["certainty"].as_str().unwrap().to_owned(),
            // The server labels a heuristic rule's message as the text formats do.
            message: match d["certainty"].as_str().unwrap() {
                "heuristic" => format!("{} [heuristic]", d["message"].as_str().unwrap()),
                "exact" => d["message"].as_str().unwrap().to_owned(),
                other => panic!("certainty {other}"),
            },
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
                certainty: d["data"]["certainty"].as_str().unwrap().to_owned(),
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
    /// The Git working tree holding the project directory, when it is not the project itself.
    _working_tree: Option<Project>,
}

impl Session {
    fn start(name: &str) -> Self {
        Self::start_with(name, CONFIG, initial_files(), false)
    }

    /// A session over `files` with the configuration `config`; `snapshot`: the effect summaries
    /// are saved (`heki snapshot --write`) before the server starts, for `MUT103`.
    fn start_with(
        name: &str,
        config: &str,
        files: Vec<(&'static str, String)>,
        snapshot: bool,
    ) -> Self {
        let p = Project::new(name);
        p.write("pyproject.toml", config);
        Self::start_in(p, None, files, snapshot)
    }

    /// A session over `files` in `proj/` of a Git working tree without any configuration file:
    /// the server, like `heki check` run there, takes the workspace folder as the project root.
    fn start_below_working_tree(name: &str, files: Vec<(&'static str, String)>) -> Self {
        let tree = Project::new(name);
        std::fs::remove_file(tree.dir.join("pyproject.toml")).unwrap();
        std::fs::create_dir_all(tree.dir.join(".git")).unwrap();
        let p = Project {
            dir: tree.dir.join("proj"),
        };
        std::fs::create_dir_all(&p.dir).unwrap();
        Self::start_in(p, Some(tree), files, false)
    }

    fn start_in(
        p: Project,
        working_tree: Option<Project>,
        files: Vec<(&'static str, String)>,
        snapshot: bool,
    ) -> Self {
        for (rel, text) in &files {
            p.write(rel, text);
        }
        if snapshot {
            assert_eq!(p.heki(&["snapshot", "--write"]).0, 0);
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
            _working_tree: working_tree,
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
            Action::Close(rel) => {
                let u = self.uri(rel);
                self.lsp
                    .notify("textDocument/didClose", json!({"textDocument": {"uri": u}}));
                self.versions.remove(rel);
            }
            Action::Write(rel, text) => {
                let created = !self.p.dir.join(rel).exists();
                self.p.write(rel, text);
                self.watched(rel, if created { 1 } else { 2 });
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
                    | Action::Delete(rel)
                    | Action::Close(rel)
                    | Action::Write(rel, _) => self.uri(rel),
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

// ------------------------------------------------------- files, layout, and buffers (session 2)

const EDGE_CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
extend-select = ["MUT103", "MUT113"]

[[tool.heki.effect-boundaries]]
name = "net"
scopes = ["app.render.render"]
forbid = ["blocking-net"]
allowed-callees = ["requests.get"]
"#;

// An effect boundary allowing one blocking API of a kind it forbids, reached through a helper.
const RENDER: &str = "from app.fetcher import fetch\n\n\ndef render(url):\n    return fetch(url)\n";
const FETCHER: &str =
    "import requests\n\n\ndef fetch(url):\n    requests.get(url)\n    return requests.get(url)\n";
// Two public functions calling each other (one recursive component across files), in the
// effect snapshot.
const PING: &str = "from app import pong\n\n\ndef f(n, seen):\n    if n:\n        return pong.g(n - 1, seen)\n    return 0\n";
const PONG: &str = "from app import ping\n\n\ndef g(n, seen):\n    if n:\n        return ping.f(n - 1, seen)\n    return 0\n";
// An `async def` reaching a blocking call in another file, with a comment above the call.
const SLOW: &str = "import time\n\n\ndef wait():\n    #x\n    time.sleep(1)\n";
const HANDLER: &str = "from app.slow import wait\n\n\nasync def handle():\n    wait()\n";
// A module file and a package of the same name (`app.dup`): Python imports the package.
const DUP_MODULE: &str = "def one():\n    pass\n";
const DUP_PACKAGE: &str = "import time\n\n\ndef two():\n    time.sleep(1)\n";
const DUP_USER: &str = "from app.dup import two\n\n\nasync def use():\n    two()\n";
// A catalog module's name, which a project module takes over (`steps_unread_module`).
const SHADOW_USER: &str = "import httpx\n\n\nasync def fetch(url):\n    return httpx.get(url)\n";
const SHADOW: &str = "def get(url):\n    return url\n";
const SHADOW_BROKEN: &str = "def get(url):\n    return url\nprint \"done\"\n";
const SHADOW_BLOCKING: &str = "import time\n\n\ndef get(url):\n    time.sleep(1)\n";
// Python runs it; heki does not read the encoding.
const SHADOW_UNREAD: &str =
    "# -*- coding: cp1252 -*-\nimport time\n\n\ndef get(url):\n    time.sleep(1)\n";
// A helper under `src/` without package markers, imported by its `src`-layout name.
const SRC_UTIL: &str = "import time\n\n\ndef wait():\n    time.sleep(1)\n";
const SRC_CALLER: &str = "from pkg.util import wait\n\n\nasync def call():\n    wait()\n";
const GENERATED: &str = "import time\n\n\nasync def tick():\n    time.sleep(1)\n";
// Helpers under the other source roots: `python/` (detected), `t/` (named after the project,
// holding `t/t`), and `lib/` (only when `src` lists it).
const PY_CALLER: &str = "from tool.util import wait\n\n\nasync def call():\n    wait()\n";
const NAMED_CALLER: &str = "from t.helper import wait\n\n\nasync def call():\n    wait()\n";
const LIB_CALLER: &str = "from kit.slow import wait\n\n\nasync def call():\n    wait()\n";
// The private generator of `TALLY` (`MUT113`), returned by a function of a second module and
// consumed in a third. No owner contract is declared here: the server re-checks no file below
// an edited function for one.
const TALLY_FORWARD: &str =
    "from app.tally import Tally\n\n\ndef _forward(t: Tally):\n    return t._walk()\n";
// A deque a kept generator iterates (`MUT113`), changed by a helper of another module it is
// handed to.
const RING: &str = "from collections import deque\n\nfrom app.ring_ops import turn\n\n\nclass Ring:\n    def __init__(self) -> None:\n        self.ring: deque[int] = deque()\n        self._cursor = self._cycle()\n\n    def _cycle(self):\n        while True:\n            yield from self.ring\n\n    def spin(self) -> None:\n        turn(self.ring)\n";
const RING_OPS: &str = "def turn(q):\n    q.rotate(1)\n";
// A dict a kept generator iterates (`MUT113`), changed by subclass methods of another module:
// through the first of two calls of one length on a line, and directly, before another dict.
const SPOOL: &str = "class Spool:\n    def __init__(self):\n        self.entries = {}\n        self.backlog = {}\n        self._cursor = self._cycle()\n\n    def _cycle(self):\n        while True:\n            yield from self.entries\n";
const SPOOL_SUB: &str = "from app.spool import Spool\n\n\nclass Pruned(Spool):\n    def prune(self):\n        self._drop(); self._seen()\n\n    def _drop(self):\n        self.entries.clear()\n\n    def _seen(self):\n        return len(self.entries)\n\n    def trim(self):\n        self.entries.clear(); self.backlog.clear()\n";
const TALLY_LISTED: &str = "from app.tally import Tally\nfrom app.tally_forward import _forward\n\n\ndef listed(t: Tally):\n    return list(_forward(t))\n";

fn edge_files() -> Vec<(&'static str, String)> {
    [
        ("app/__init__.py", ""),
        ("app/render.py", RENDER),
        ("app/fetcher.py", FETCHER),
        ("app/ping.py", PING),
        ("app/pong.py", PONG),
        ("app/slow.py", SLOW),
        ("app/handler.py", HANDLER),
        ("app/dup.py", DUP_MODULE),
        ("app/dup/__init__.py", DUP_PACKAGE),
        ("app/dup_user.py", DUP_USER),
        ("app/shadow_user.py", SHADOW_USER),
        ("src/pkg/util.py", SRC_UTIL),
        ("caller.py", SRC_CALLER),
        ("python/tool/util.py", SRC_UTIL),
        ("py_caller.py", PY_CALLER),
        ("t/t/helper.py", SRC_UTIL),
        ("named_caller.py", NAMED_CALLER),
        ("lib/kit/slow.py", SRC_UTIL),
        ("lib_caller.py", LIB_CALLER),
        ("app/tally.py", TALLY),
        ("app/tally_forward.py", TALLY_FORWARD),
        ("app/tally_listed.py", TALLY_LISTED),
        ("app/ring.py", RING),
        ("app/ring_ops.py", RING_OPS),
        ("app/spool.py", SPOOL),
        ("app/spool_sub.py", SPOOL_SUB),
    ]
    .into_iter()
    .map(|(rel, text)| (rel, text.to_owned()))
    .collect()
}

fn edge_steps() -> Vec<Step> {
    let mut steps = vec![
        Step::new("initial index", Vec::new())
            .present(&[
                ("app/handler.py", "EFF101"),
                ("app/dup_user.py", "EFF101"),
                ("app/shadow_user.py", "EFF101"),
                ("caller.py", "EFF101"),
                ("py_caller.py", "EFF101"),
                ("named_caller.py", "EFF101"),
            ])
            .absent(&[("app/render.py", "EFF201"), ("lib_caller.py", "EFF101")]),
    ];
    steps.extend(steps_shared_module_name());
    steps.extend(steps_unread_module());
    steps.extend(steps_boundary_calls());
    steps.extend(steps_recursive_snapshot());
    steps.extend(steps_moved_lines());
    steps.extend(steps_discarded_buffer());
    steps.extend(steps_src_layout());
    steps.extend(steps_source_roots());
    steps.extend(steps_ignore_files());
    steps.extend(steps_forwarded_generator());
    steps.extend(steps_swapped_calls());
    steps.extend(steps_baseline());
    steps
}

/// `MUT113`: a module steps by hand, then consumes, a private generator of another module that a
/// function of a third one returns. The generator's report follows its object through the
/// function returning it (`heki_rules::family_readers`), while no summary changes; then the
/// module stops calling that function, which only the calls before the edit lead from to the
/// generator. Then a helper
/// a kept generator's method hands its deque to stores an item of it instead of rotating it
/// (`IterationFacts::changes_param_containers`), and rotates it again.
fn steps_forwarded_generator() -> Vec<Step> {
    let stepping = TALLY_LISTED.replace(
        "    return list(_forward(t))\n",
        "    g = _forward(t)\n    next(g)\n    return list(g)\n",
    );
    let stopped = TALLY_LISTED.replace("    return list(_forward(t))\n", "    return []\n");
    vec![
        Step::new(
            "a module steps by hand the generator a function of another module returns",
            vec![Action::Save("app/tally_listed.py", stepping)],
        )
        .present(&[("app/tally.py", "MUT113")]),
        Step::new(
            "the module stops calling the forwarding function",
            vec![Action::Save("app/tally_listed.py", stopped)],
        )
        .absent(&[("app/tally.py", "MUT113")]),
        Step::new(
            "the module lists the forwarded generator again",
            vec![save("app/tally_listed.py", TALLY_LISTED)],
        )
        .absent(&[("app/tally.py", "MUT113")]),
        Step::new(
            "a helper of another module only stores an item of the deque it is handed",
            vec![save("app/ring_ops.py", "def turn(q):\n    q[0] = 0\n")],
        )
        .absent(&[("app/ring.py", "MUT113")]),
        Step::new(
            "the helper rotates the deque again",
            vec![save("app/ring_ops.py", RING_OPS)],
        )
        .present(&[("app/ring.py", "MUT113")]),
    ]
}

/// `MUT113`: two calls of one length on a line swap, so every call site keeps its span while the
/// call changing the dict a kept generator of another module iterates moves to the other one;
/// then two changes of one length swap the same way. The generator's report shows where each
/// method changes the dict.
fn steps_swapped_calls() -> Vec<Step> {
    let swapped = SPOOL_SUB.replace("self._drop(); self._seen()", "self._seen(); self._drop()");
    let changes_swapped = SPOOL_SUB.replace(
        "self.entries.clear(); self.backlog.clear()",
        "self.backlog.clear(); self.entries.clear()",
    );
    vec![
        Step::new(
            "two calls of one length on a line swap, the change moving to the second",
            vec![Action::Save("app/spool_sub.py", swapped)],
        )
        .present(&[("app/spool.py", "MUT113")]),
        Step::new(
            "the calls swap back",
            vec![save("app/spool_sub.py", SPOOL_SUB)],
        )
        .present(&[("app/spool.py", "MUT113")]),
        Step::new(
            "two changes of one length on a line swap, the dict's moving to the second",
            vec![Action::Save("app/spool_sub.py", changes_swapped)],
        )
        .present(&[("app/spool.py", "MUT113")]),
    ]
}

/// A module file and a package sharing one name: the package wins, whichever is edited.
fn steps_shared_module_name() -> Vec<Step> {
    vec![
        Step::new(
            "the module file of a name a package holds is edited",
            vec![save("app/dup.py", &DUP_MODULE.replace("one", "one_more"))],
        )
        .present(&[("app/dup_user.py", "EFF101")]),
        Step::new(
            "the package is edited",
            vec![save(
                "app/dup/__init__.py",
                &format!("# package\n{DUP_PACKAGE}"),
            )],
        )
        .present(&[("app/dup_user.py", "EFF101")]),
        Step::new(
            "the package stops blocking",
            vec![save(
                "app/dup/__init__.py",
                &DUP_PACKAGE.replace("time.sleep(1)", "pass"),
            )],
        )
        .absent(&[("app/dup_user.py", "EFF101")]),
        Step::new(
            "the package blocks again",
            vec![save("app/dup/__init__.py", DUP_PACKAGE)],
        )
        .present(&[("app/dup_user.py", "EFF101")]),
    ]
}

/// A project module takes the name of a module the catalog lists: a name under it resolves to the
/// project's module, and to nothing while that module does not parse, never to the catalog.
fn steps_unread_module() -> Vec<Step> {
    vec![
        Step::new(
            "a module that does not parse takes a catalog module's name",
            vec![Action::Create("httpx.py", SHADOW_BROKEN.to_owned())],
        )
        .absent(&[("app/shadow_user.py", "EFF101")]),
        Step::new("the module parses", vec![save("httpx.py", SHADOW)])
            .absent(&[("app/shadow_user.py", "EFF101")]),
        Step::new(
            "the module stops parsing again",
            vec![save("httpx.py", SHADOW_BROKEN)],
        )
        .absent(&[("app/shadow_user.py", "EFF101")]),
        Step::new("the module blocks", vec![save("httpx.py", SHADOW_BLOCKING)])
            .present(&[("app/shadow_user.py", "EFF101")]),
        Step::new(
            "an unsaved edit is discarded while the disk holds an encoding heki does not read",
            vec![
                Action::Change("httpx.py", SHADOW.to_owned()),
                Action::Write("httpx.py", SHADOW_UNREAD.to_owned()),
                Action::Close("httpx.py"),
            ],
        )
        .absent(&[("app/shadow_user.py", "EFF101")]),
        Step::new(
            "the closed module is written blocking on the disk",
            vec![Action::Write("httpx.py", SHADOW_BLOCKING.to_owned())],
        )
        .present(&[("app/shadow_user.py", "EFF101")]),
        Step::new(
            "the closed module is written in an encoding heki does not read",
            vec![Action::Write("httpx.py", SHADOW_UNREAD.to_owned())],
        )
        .absent(&[("app/shadow_user.py", "EFF101")]),
        Step::new("the module is deleted", vec![Action::Delete("httpx.py")])
            .present(&[("app/shadow_user.py", "EFF101")]),
    ]
}

/// Effect boundaries read the ecosystem calls below them, not only the summaries.
fn steps_boundary_calls() -> Vec<Step> {
    vec![
        Step::new(
            "a call the boundary allows becomes one of the same effect it forbids",
            vec![save(
                "app/fetcher.py",
                &FETCHER.replacen("requests.get", "requests.put", 1),
            )],
        )
        .present(&[("app/render.py", "EFF201")]),
        Step::new(
            "a second forbidden call of the same effect",
            vec![save(
                "app/fetcher.py",
                &FETCHER.replace("requests.get", "requests.put"),
            )],
        )
        .present(&[("app/render.py", "EFF201")]),
        Step::new(
            "the calls are allowed again",
            vec![save("app/fetcher.py", FETCHER)],
        )
        .absent(&[("app/render.py", "EFF201")]),
    ]
}

/// `MUT103`: both members of a recursive component across files gain the effect.
fn steps_recursive_snapshot() -> Vec<Step> {
    vec![
        Step::new(
            "a function calling a function in another file that calls it back mutates an argument",
            vec![save(
                "app/ping.py",
                &PING.replace("    return 0\n", "    seen.append(n)\n    return 0\n"),
            )],
        )
        .present(&[("app/ping.py", "MUT103"), ("app/pong.py", "MUT103")]),
        Step::new("it stops mutating it", vec![save("app/ping.py", PING)])
            .absent(&[("app/ping.py", "MUT103"), ("app/pong.py", "MUT103")]),
    ]
}

/// A call moves down a line, its byte span unchanged: the chain shown in another file moves
/// with it.
fn steps_moved_lines() -> Vec<Step> {
    vec![
        Step::new(
            "a comment above a blocking call becomes a blank line",
            vec![save("app/slow.py", &SLOW.replace("    #x\n", "    #\n\n"))],
        )
        .present(&[("app/handler.py", "EFF101")]),
    ]
}

/// An unsaved edit discarded by closing the document: the disk counts again.
fn steps_discarded_buffer() -> Vec<Step> {
    vec![
        Step::new(
            "an unsaved edit stops the helper blocking",
            vec![Action::Change(
                "app/slow.py",
                "def wait():\n    pass\n".to_owned(),
            )],
        )
        .present(&[("app/handler.py", "EFF101")]),
        Step::new(
            "the edit is discarded and the caller saved",
            vec![
                Action::Close("app/slow.py"),
                save("app/handler.py", &format!("# saved\n{HANDLER}")),
            ],
        )
        .present(&[("app/handler.py", "EFF101")]),
    ]
}

/// `src/` is a source root until it becomes a package itself: then every module name under it
/// changes, and the caller's `pkg.util` no longer names the helper.
fn steps_src_layout() -> Vec<Step> {
    vec![
        Step::new(
            "a package marker under src",
            vec![Action::Write("src/pkg/__init__.py", String::new())],
        )
        .present(&[("caller.py", "EFF101")]),
        Step::new(
            "src becomes a package itself",
            vec![Action::Write("src/__init__.py", String::new())],
        )
        .absent(&[("caller.py", "EFF101")]),
        Step::new(
            "src is a source root again",
            vec![Action::Delete("src/__init__.py")],
        )
        .present(&[("caller.py", "EFF101")]),
    ]
}

/// The source roots besides `src/`: detected ones change with their package markers and the
/// project's name, and `src` replaces them.
fn steps_source_roots() -> Vec<Step> {
    let listing = |dirs: &str| {
        EDGE_CONFIG.replace(
            "[tool.heki.lint]",
            &format!("[tool.heki]\nsrc = [{dirs}]\n\n[tool.heki.lint]"),
        )
    };
    vec![
        Step::new(
            "python becomes a package",
            vec![Action::Write("python/__init__.py", String::new())],
        )
        .absent(&[("py_caller.py", "EFF101")]),
        Step::new(
            "python is a source root again",
            vec![Action::Delete("python/__init__.py")],
        )
        .present(&[("py_caller.py", "EFF101")]),
        Step::new(
            "the project is renamed",
            vec![Action::Configure(
                EDGE_CONFIG.replace("name = \"t\"", "name = \"u\""),
            )],
        )
        .absent(&[("named_caller.py", "EFF101")])
        .present(&[("py_caller.py", "EFF101")]),
        Step::new(
            "src lists lib and leaves out python",
            vec![Action::Configure(listing("\"src\", \"lib\", \"t\""))],
        )
        .present(&[
            ("lib_caller.py", "EFF101"),
            ("caller.py", "EFF101"),
            ("named_caller.py", "EFF101"),
        ])
        .absent(&[("py_caller.py", "EFF101")]),
        Step::new(
            "the detected source roots again",
            vec![Action::Configure(EDGE_CONFIG.to_owned())],
        )
        .present(&[("py_caller.py", "EFF101"), ("named_caller.py", "EFF101")])
        .absent(&[("lib_caller.py", "EFF101")]),
    ]
}

/// Ignore files decide which files belong to the project.
fn steps_ignore_files() -> Vec<Step> {
    vec![
        Step::new(
            "an ignore file lists a module",
            vec![Action::Write(
                ".hekiignore",
                "app/generated.py\n".to_owned(),
            )],
        ),
        Step::new(
            "the ignored module is created",
            vec![Action::Write("app/generated.py", GENERATED.to_owned())],
        )
        .absent(&[("app/generated.py", "EFF101")]),
        Step::new(
            "the ignore file lists a module that has diagnostics",
            vec![Action::Write(
                ".hekiignore",
                "app/generated.py\napp/handler.py\n".to_owned(),
            )],
        )
        .absent(&[("app/handler.py", "EFF101")]),
        Step::new(
            "the ignore file is emptied",
            vec![Action::Write(".hekiignore", String::new())],
        )
        .present(&[("app/generated.py", "EFF101"), ("app/handler.py", "EFF101")]),
    ]
}

/// A baseline of `handle`'s blocking calls in `app/handler.py`, allowing `count` of them.
fn handler_baseline(count: u32) -> String {
    format!(
        "version = 2\n\n[[entries]]\npath = \"app/handler.py\"\nqualname = \"handle\"\nrule_code = \"EFF101\"\ntarget = \"time.sleep\"\ncount = {count}\n"
    )
}

/// A Ratchet baseline in the configuration applies to the editor too.
fn steps_baseline() -> Vec<Step> {
    let configured = EDGE_CONFIG.replace(
        "[tool.heki.lint]",
        "[tool.heki]\nbaseline = \"heki-baseline.toml\"\n\n[tool.heki.lint]",
    );
    vec![
        Step::new(
            "a baseline records the blocking call of the handler",
            vec![
                Action::Write("heki-baseline.toml", handler_baseline(1)),
                Action::Configure(configured),
            ],
        )
        .absent(&[("app/handler.py", "EFF101")])
        .present(&[("app/dup_user.py", "EFF101")]),
        Step::new(
            "a second blocking call in the handler",
            vec![save(
                "app/handler.py",
                &format!("# saved\n{HANDLER}    wait()\n"),
            )],
        )
        .present(&[("app/handler.py", "EFF101")]),
        Step::new(
            "the baseline grows to both calls",
            vec![Action::Write("heki-baseline.toml", handler_baseline(2))],
        )
        .absent(&[("app/handler.py", "EFF101")]),
    ]
}

#[test]
fn lsp_diagnostics_equal_a_full_check_through_file_layout_and_buffer_changes() {
    let mut s = Session::start_with("lsp_oracle_edges", EDGE_CONFIG, edge_files(), true);
    let mut failures = Vec::new();
    for step in edge_steps() {
        failures.extend(s.run(&step));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    s.lsp.stop(true);
}

// ------------------------------------------- a project below a Git working tree (session 3)

// `src/defs.py` is `defs` under the project root: four statuses, a small constant (`OPT113`).
const DEFS: &str =
    "STATUS_ORDER = [\"available\", \"caveated\", \"approximate\", \"unavailable\"]\n";
const JOIN: &str = "from defs import STATUS_ORDER\n\n\ndef f(metrics):\n    return [m for s in STATUS_ORDER for m in metrics if m.status == s]\n";

#[test]
fn lsp_diagnostics_equal_a_full_check_below_a_git_working_tree() {
    let files = vec![
        ("src/defs.py", DEFS.to_owned()),
        ("b_func.py", JOIN.to_owned()),
    ];
    let mut s = Session::start_below_working_tree("lsp_oracle_working_tree", files);
    let steps = [
        Step::new("initial index", Vec::new()).absent(&[("b_func.py", "OPT113")]),
        Step::new(
            "src becomes a package",
            vec![Action::Write("src/__init__.py", String::new())],
        )
        .present(&[("b_func.py", "OPT113")]),
        Step::new(
            "src is a source root again",
            vec![Action::Delete("src/__init__.py")],
        )
        .absent(&[("b_func.py", "OPT113")]),
    ];
    let mut failures = Vec::new();
    for step in &steps {
        failures.extend(s.run(step));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    s.lsp.stop(true);
}
