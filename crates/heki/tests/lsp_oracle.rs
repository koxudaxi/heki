//! Differential oracle for the incremental language server (docs/lsp.md): after every step of
//! an editing session over a small multi-file project, the diagnostics the server published for
//! every file must equal a fresh `heki check --output-format json` of the same files and
//! configuration.
//!
//! The steps come in groups (`steps_*`), and each group is a test of its own (`sessions!`): a
//! session that opens every file, then runs the group's steps. A step edits and saves,
//! creates, or deletes files, or changes the configuration, the way an editor and its file
//! watcher report it (`workspace/didChangeWatchedFiles`). After it, the harness waits until the
//! server is idle ([`Lsp::settle`]: `heki/stats` reports no work in flight, and every
//! publication it counts has arrived; no sleeps) and compares, file by file, the range, code,
//! severity, certainty, message (labelled for a heuristic rule), related locations, the places of
//! its contracts, and help (the last related entry, at the diagnostic's own range) of every
//! diagnostic. A step also names diagnostics
//! the CLI must (or must not) report, so it exercises what it describes, and a step marked
//! `cutoff` must stop at the edited files (Early Cutoff). A new step is a few lines; a new group
//! is a function and a line in `sessions!`.

mod common;

use common::Project;
use common::lsp::{Lsp, uri};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

// ------------------------------------------------------------------------------------ project

const CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
extend-select = ["MUT101", "MUT107", "MUT108", "MUT111", "EFF107", "MUT112", "MUT113", "EFF109", "EFF110", "EFF112", "EFF114", "EFF115", "EFF116", "EFF117", "EFF118", "MUT115", "MUT118", "MUT119", "MUT120", "OPT103", "OPT110", "OPT111", "OPT120", "OPT121", "OPT130"]

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

[[tool.heki.import-boundaries]]
name = "imports"
importers = ["pkg.imp_user"]
forbidden-imports = ["pkg.imp_db", "pkg.imp_pkg.sub"]

[[tool.heki.import-boundaries]]
name = "startup"
importers = ["pkg.imp_cli"]
forbidden-imports = ["pkg.imp_heavy"]
reach = "import-time"

[[tool.heki.import-boundaries]]
name = "private"
importers = ["pkg.imp_priv"]
forbidden-imports = ["pkg.imp_privmod:_*", "pkg.imp_banned"]
unresolved = "report"

[[tool.heki.import-boundaries]]
name = "copied"
importers = ["pkg.imp_rt.*"]
allowed-imports = ["<stdlib>"]
relative-imports = ["pkg.imp_rt.*"]

[[tool.heki.invalidation]]
name = "dial"
targets = ["pkg.dials:*.level"]
invalidators = ["pkg.dials:recalibrate"]

[[tool.heki.effect-boundaries]]
name = "portable"
scopes = ["pkg.portable", "pkg.portable_boot:<module>"]
forbid = ["reads-environment"]

[[tool.heki.effect-boundaries]]
name = "rack-view"
scopes = ["pkg.rack_view"]
forbid = ["writes-targets"]
write-targets = ["Rack.items"]
allowed-callees = ["pkg.rack_ops.empty"]

[[tool.heki.effect-boundaries]]
name = "extras-pass"
scopes = ["pkg.extras_pass"]
write-universe = ['Extras.values["init"]']
allowed-callees = ["pkg.extras.trusted_value"]

[[tool.heki.write-owners]]
name = "rack"
targets = ["Rack.items"]
allowed-writers = ["pkg.rack_ops.empty"]

[[tool.heki.effect-boundaries]]
name = "copy-pass"
scopes = ["pkg.copy_pass", "pkg.guarded_build", "pkg.link_pass", "pkg.link_model_pass", "pkg.link_neg_pass", "pkg.owned_pass", "pkg.owned_flag_pass"]
write-universe = ["Ticket.flag", "Part.flag", "Holder.flag"]
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
// `MUT107` in a subclass in another module: whether a store of `self.key` is a plain store
// comes from the base class's declarations (`ClassDecl::properties`, its `__setattr__`, and a
// base heki names no class for, `ClassDecl::custom_metaclass`).
const TOKEN_BASE: &str = "class Token:\n    def __init__(self, key):\n        self.key = key\n\n    def __hash__(self):\n        return hash(self.key)\n";
const TOKEN_USER: &str = "from pkg.token_base import Token\n\n\nclass Renamed(Token):\n    def rename(self, key):\n        self.key = key\n";
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
const CATALOG_NAP: &str = "import time\n\n\ndef nap(n: float) -> None:\n    time.sleep(1)\n";
const CATALOG_USE: &str =
    "from pkg.catalog_nap import nap\n\n\nasync def wait() -> None:\n    nap(0)\n";
const CATALOG_QUERY: &str = "import sqlite3\n\n\ndef first(db: sqlite3.Connection) -> object:\n    return db.execute(\"SELECT 1\").fetchone()\n";
const CATALOG_QUERY_USE: &str = "import sqlite3\n\nfrom pkg.catalog_nap import first\n\nDB = sqlite3.connect(\"cache.db\")\n\n\nasync def wait() -> object:\n    return first(DB)\n";
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
// A scope function of `reads-environment`, a module body the boundary names, and the helper
// below both, in another module.
const PORTABLE_OPS: &str = "import os\nimport sys\n\n\ndef choose():\n    return \"posix\"\n";
const PORTABLE: &str =
    "from pkg.portable_ops import choose\n\n\ndef build():\n    return choose()\n";
const PORTABLE_BOOT: &str = "from pkg.portable_ops import choose\n\nMODE = choose()\n";
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
// A store operation in another module writes the entry its caller passes (`set_value(e, "init", v)`).
const EXTRAS: &str = "class Extras:\n    def __init__(self) -> None:\n        self.values: dict[str, object] = {}\n\n\ndef set_value(e: Extras, key: str, value: object) -> None:\n    e.values[key] = value\n";
const EXTRAS_PASS: &str = "from pkg.extras import Extras, set_value\n\n\ndef prepare(e: Extras) -> None:\n    set_value(e, \"init\", 1)\n";
// Two routes to the store operation passing the same key: one through an allowed callee, one
// through a relay that is not allowed.
const EXTRAS_ROUTES: &str = "\n\ndef trusted_value(e: Extras, key: str) -> None:\n    set_value(e, key, 1)\n\n\ndef relay_value(e: Extras, key: str) -> None:\n    _forward_value(e, key)\n\n\ndef _forward_value(e: Extras, key: str) -> None:\n    set_value(e, key, 1)\n";
const EXTRAS_TRUSTED_PASS: &str = "from pkg.extras import Extras, relay_value, trusted_value\n\n\ndef prepare(e: Extras) -> None:\n    trusted_value(e, \"init\")\n";
const KEY_CONSTS: &str = "from typing import Final\n\nKEY: Final = \"entries\"\n";
const KEY_WRITER: &str = "from pkg.key_consts import KEY\nfrom pkg.model import Registry\n\n\ndef mark(r: Registry):\n    r.__dict__[KEY] = []\n";
const KEY_HOOK: &str = "\n\ndef __getattr__(name):\n    value = object()\n    globals()[name] = value\n    return value\n";
const KEY_WARM: &str =
    "from pkg import key_consts\n\n\ndef warm():\n    key_consts.__getattr__(\"KEY\")\n";
const KEY_LOOP: &str = "from pkg.key_consts import KEYS\nfrom pkg.model import Registry\n\n\ndef mark_all(r: Registry):\n    for key in KEYS:\n        r.__dict__[key] = []\n";
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
const RESTORING: &str = "import httpx\n\n\nclass Restoring:\n    def __init__(self):\n        self.client = httpx.Client()\n\n    def __setstate__(self, state):\n        self.__dict__.update(state)\n\n    async def handle(self):\n        self.client.get(\"https://example.com\")\n";
const SETSTATE_CALL: &str = "import httpx\n\nfrom pkg.restoring import Restoring\n\n\ndef load(target: Restoring):\n    target.__setstate__({\"client\": httpx.AsyncClient()})\n";
const SETSTATE_READ: &str = "import httpx\n\nfrom pkg.restoring import Restoring\n\n\ndef load(target: Restoring):\n    restore = getattr(target, \"__setstate__\")\n    restore({\"client\": httpx.AsyncClient()})\n";
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
// A maker storing what it makes, and a helper storing what it is handed, only when a flag their
// callers in another module leave false is true.
const GUARDED_KEEPS: &str = "import copy\n\nfrom pkg.copy_ops import Ticket\n\nKEPT = []\n\n\ndef make(t: Ticket, keep=False) -> Ticket:\n    c = copy.copy(t)\n    if keep:\n        KEPT.append(c)\n    return c\n\n\ndef hold(t: Ticket, keep=False):\n    if keep:\n        KEPT.append(t)\n";
const GUARDED_BUILD: &str = "import copy\n\nfrom pkg.copy_ops import Ticket, mark\nfrom pkg.guarded_keeps import hold, make\n\n\ndef build_made(t: Ticket):\n    c = make(t)\n    mark(c)\n    return c\n\n\ndef build_held(t: Ticket):\n    c = copy.copy(t)\n    hold(c)\n    mark(c)\n    return c\n";
// A copy helper returning an element of an optional table or a copy, a public helper marking
// its parameter, and one returning its parameter, in another module than the boundary's scope
// using them on copies.
const COPY_OPS: &str = "import copy\n\nKEPT = []\n\n\nclass Ticket:\n    def __init__(self):\n        self.flag = False\n\n\ndef copy_or_reuse(t: Ticket, reuse: dict[int, Ticket] | None = None) -> Ticket:\n    if reuse and (hit := reuse.get(id(t))) is not None:\n        return hit\n    return copy.copy(t)\n\n\ndef mark(t: Ticket) -> None:\n    t.flag = True\n\n\ndef stamp(t: Ticket) -> Ticket:\n    return t\n";
// A factory in another module returning a new ticket it links a new part into: a pass keeping
// the part it reads out of the result before marking the result (`ReturnsNew::New::linked`).
const LINK_OPS: &str = "from pkg.copy_ops import Ticket\n\n\ndef make() -> Ticket:\n    made, part = Ticket(), Ticket()\n    made.part = part\n    return made\n";
const LINK_PASS: &str = "from pkg.copy_ops import KEPT, Ticket\nfrom pkg.link_ops import make\n\n\ndef run() -> Ticket:\n    made = make()\n    KEPT.append(made.part)\n    made.flag = True\n    return made\n";
// A pass keeping the negation of a new ticket it links another new one into: the result holds
// nothing of the ticket while no class the ticket may be defines `__neg__`.
const LINK_NEG_PASS: &str = "from pkg.copy_ops import KEPT, Ticket\n\n\ndef run() -> Ticket:\n    copied, part = Ticket(), Ticket()\n    part.part = copied\n    KEPT.append(-part)\n    copied.flag = True\n    return copied\n";
// A pydantic model of another module that two new objects of a pass link: a write of it stores
// the value itself only while no class it may be configures the model
// (`ProjectDecls::attr_write_stores_value`).
const LINK_MODEL: &str = "from typing import Any\n\nfrom pydantic import BaseModel\n\n\nclass Part(BaseModel):\n    flag: bool = False\n    parent: Any = None\n";
const LINK_MODEL_SUB: &str = "from pydantic import ConfigDict\n\nfrom pkg.link_model import Part\n\n\nclass Validated(Part):\n    model_config = ConfigDict(validate_assignment=True)\n";
const LINK_MODEL_PASS: &str = "from pkg.link_model import Part\n\n\ndef run() -> Part:\n    copied, part = Part(), Part()\n    part.parent = copied\n    copied.flag = True\n    return copied\n";
// A class of another module whose constructor makes a list for each instance after calling the
// constructor of a base in a third module, and a pass storing a new ticket into the list of a new
// instance (`ClassDecl::plain_init`).
const OWNED_BASE: &str =
    "SHARED_STATE = {}\n\n\nclass Base:\n    def __init__(self):\n        self.ready = True\n";
const OWNED_MODEL: &str = "from pkg.owned_base import Base\n\nSHARED = []\n\n\nclass Holder(Base):\n    def __init__(self):\n        super().__init__()\n        self.parts: list = []\n";
const OWNED_PASS: &str = "from pkg.copy_ops import Ticket\nfrom pkg.owned_model import Holder\n\n\ndef run() -> Holder:\n    copied = Holder()\n    part = Ticket()\n    copied.parts.append(part)\n    part.flag = True\n    return copied\n";
// A pass writing an instance it constructs: new while the constructors it runs keep nothing of
// it, the base's in a third module included (`ProjectDecls::constructor`).
const OWNED_FLAG_PASS: &str = "from pkg.owned_model import Holder\n\n\ndef run() -> Holder:\n    copied = Holder()\n    copied.flag = True\n    return copied\n";
const COPY_PASS: &str = "from pkg.copy_ops import Ticket, copy_or_reuse, mark, stamp\n\n\ndef run(t: Ticket) -> Ticket:\n    copied = stamp(copy_or_reuse(t))\n    copied.flag = True\n    mark(copied)\n    return copied\n";
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
// The class of an object typed with a class of another module: `type(obj)` and a lambda's
// closure cell.
const SEAL_TYPE: &str = "from pkg.seal import Seal\n\n\ndef crack_type(s: Seal):\n    type(s).break_open(s)\n\n\ndef peek_later(s: Seal):\n    return lambda: s.code\n";
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
// `MUT108` of a field every construction fills with an object rejecting changes
// (`ModuleDecls::field_values`): the construction in another module switches between a
// `MappingProxyType` and a dict.
const HELD_MODEL: &str = "from collections.abc import Mapping\nfrom dataclasses import dataclass\n\n\n@dataclass(frozen=True)\nclass Held:\n    meta: Mapping[str, int]\n";
const HELD_MAKE: &str = "from types import MappingProxyType\n\nfrom pkg.held.model import Held\n\n\ndef make() -> Held:\n    return Held(MappingProxyType({}))\n";
const HELD_WRITE: &str = "from typing import Any, cast\n\nfrom pkg.held.model import Held\n\n\ndef write(held: Held) -> None:\n    cast(\"Any\", held.meta)[\"n\"] = 1\n";

// `MUT120`: a read-only view (a protocol of properties without a setter) in one module, helpers
// that do not see its type in a second, and a function holding the view in a third.
const RO_VIEW: &str = "from typing import Protocol\n\n\nclass FieldView(Protocol):\n    @property\n    def required(self) -> bool: ...\n\n    @property\n    def extras(self) -> dict[str, object]: ...\n";
const RO_VIEW_HELPERS: &str = "def flip(field):\n    return field.required\n\n\ndef tag(field):\n    field.extras[\"k\"] = 1\n";
const RO_VIEW_DECIDE: &str = "from pkg.roview.helpers import flip, tag\nfrom pkg.roview.model import FieldView\n\n\ndef decide(field: FieldView) -> None:\n    flip(field)\n    tag(field)\n";

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
// `MUT202` / `MUT114`: a wrapper in another module starts returning a declared producer's value
// (or an `@lru_cache` result), and stops again. `MUT208`: a helper in another module stops
// announcing a change its caller makes, and a caller in another module stops announcing after
// calling a public writer.
const CATALOG: &str = "from functools import lru_cache\n\n\nclass Catalog:\n    def entry(self, key) -> dict:\n        return {}\n\n    def fresh(self, key) -> dict:\n        return {}\n\n\n@lru_cache\ndef defaults() -> dict:\n    return {}\n";
const LOOKUP: &str = "from pkg.catalog import Catalog, defaults\n\n\ndef find(c: Catalog, key) -> dict:\n    return c.fresh(key)\n";
const LOOKUP_SHARED: &str = "from pkg.catalog import Catalog, defaults\n\n\ndef find(c: Catalog, key) -> dict:\n    return c.entry(key)\n";
const LOOKUP_CACHED: &str = "from pkg.catalog import Catalog, defaults\n\n\ndef find(c: Catalog, key) -> dict:\n    return defaults()\n";
const LOOKUP_SHARED_ITEM: &str = "from pkg.catalog import Catalog, defaults\n\n\ndef find(c: Catalog, key) -> dict:\n    return c.entry(key)[\"inner\"]\n";
// `MUT114`: a method of a cached object in another module fills the object's own memo, then
// changes its state instead.
const GLYPHS: &str = "from functools import lru_cache\n\n\nclass Glyphs:\n    def __init__(self):\n        self._width_cache = {}\n        self.seen = []\n\n    def width(self, ch):\n        if ch not in self._width_cache:\n            self._width_cache[ch] = len(ch)\n        return self._width_cache[ch]\n\n\n@lru_cache\ndef shared_glyphs() -> Glyphs:\n    return Glyphs()\n";
const GLYPHS_RECORDING: &str = "from functools import lru_cache\n\n\nclass Glyphs:\n    def __init__(self):\n        self._width_cache = {}\n        self.seen = []\n\n    def width(self, ch):\n        self.seen.append(ch)\n        return len(ch)\n\n\n@lru_cache\ndef shared_glyphs() -> Glyphs:\n    return Glyphs()\n";
const GLYPH_USER: &str = "from pkg.glyphs import shared_glyphs\n\n\ndef measure(text):\n    return sum(shared_glyphs().width(ch) for ch in text)\n\n\ndef first(text):\n    return shared_glyphs().width(text[0])\n";
// `MUT114`: whether a cached function in another module hands out a list on every return
// (`EffectSummary::cached_result`) decides which claim the caller's report states.
const FIELDS: &str = "from functools import lru_cache\n\n\n@lru_cache\ndef fields(name):\n    return [name, \"id\"]\n";
const FIELDS_MAYBE: &str = "from functools import lru_cache\n\n\n@lru_cache\ndef fields(name):\n    if not name:\n        return None\n    return [name, \"id\"]\n";
const FIELDS_USER: &str = "from pkg.fields import fields\n\n\ndef extended(name):\n    result = fields(name)\n    result.append(\"extra\")\n    return result\n";
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
// A private method of a `Registry` subclass filling the entries of its own `self` in a
// generator expression, which reads `self` from a closure cell, and a subclass in another module
// whose initializer calls it (`steps_captured_parameters`).
const REG_LOAD: &str = "from pkg.model import Registry\n\n\nclass Loading(Registry):\n    def _load(self, peer: Registry, xs):\n        any(self.entries.append(x) for x in xs)\n";
const REG_SUB: &str = "from pkg.model import Registry\nfrom pkg.reg_load import Loading\n\n\nclass Loaded(Loading):\n    def __init__(self, other: Registry, xs):\n        super().__init__()\n        self._load(other, xs)\n";
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
// The rows a loop steps through (`OPT103`), by the class another module binds the cursor's name
// to: the rewrite is exact while it is `sqlite3.Cursor`, whose `execute` the catalog sizes, and
// heuristic once it is a project class ("Trip count open").
const STORAGE: &str = "from sqlite3 import Cursor\n";
const STORAGE_OWN: &str = "class Cursor:\n    def execute(self, query):\n        return [query]\n";
const ROWS_USER: &str = "from pkg.storage import Cursor\n\n\ndef ids(cursor: Cursor):\n    out = ()\n    for row in cursor.execute(\"SELECT id FROM t\"):\n        out = out + (row,)\n    return out\n";
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
// `OPT113`'s exact tier needs advancing the inner loop to run no project code: a project
// subclass of `list` with its own `__iter__`, which an argument annotated `list` may be (R2),
// leaves it open.
const JOIN_USER: &str = "from pkg.rows import Row\n\n\ndef join(xs: list[Row], ys: list[Row]):\n    out = []\n    for a in xs:\n        for b in ys:\n            if a.k == b.k:\n                out.append(b)\n    return out\n";
// The compared key is a plain field of a class in another module, then a property.
const ROWS: &str = "from dataclasses import dataclass\n\n\n@dataclass\nclass Row:\n    k: int\n";
const ROWS_PROPERTY: &str = "class Row:\n    @property\n    def k(self):\n        return 1\n";
const LOUD_LIST: &str = "class LoudList(list):\n    def __iter__(self):\n        print(\"next\")\n        return super().__iter__()\n";
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
// A function returning what a function of another module returns of its parameter, which the
// caller hands its read-only parameter to before changing what comes back.
const RO_SAME: &str = "def same(x):\n    return x\n";
const RO_FORWARD: &str =
    "from pkg.ro.same import same\n\n\ndef forwarded(x):\n    return same(x)\n";
const RO_FORWARD_USE: &str = "from collections.abc import Sequence\n\nfrom pkg.ro.forward import forwarded\n\n\ndef change(x):\n    if isinstance(x, list):\n        x.reverse()\n\n\ndef two(xs: Sequence[int]) -> None:\n    change(forwarded(xs))\n";
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

// Sentinels lost in deep copies (`MUT118`): a module binds an `object()` sentinel, a dataclass
// in a second module takes it as a field default, a third module tests the field against it,
// and a fourth deep-copies the dataclass.
const MARKS: &str = "MISSING = object()\n";
const MARKS_SENTINEL: &str =
    "from typing_extensions import Sentinel\n\nMISSING = Sentinel(\"MISSING\")\n";
const MARKED_SPEC: &str = "from dataclasses import dataclass\n\nfrom pkg.marks import MISSING\n\n\n@dataclass\nclass Spec:\n    name: str\n    value: object = MISSING\n";
const MARKED_SPEC_HOOKED: &str = "from dataclasses import dataclass\n\nfrom pkg.marks import MISSING\n\n\n@dataclass\nclass Spec:\n    name: str\n    value: object = MISSING\n\n    def __deepcopy__(self, memo):\n        return self\n";
const MARK_TESTS: &str = "from pkg.marked_spec import Spec\nfrom pkg.marks import MISSING\n\n\ndef given(spec: Spec) -> bool:\n    return spec.value is not MISSING\n";
const MARK_TESTS_NONE: &str = "from pkg.marked_spec import Spec\nfrom pkg.marks import MISSING\n\n\ndef given(spec: Spec) -> bool:\n    return spec.value is not None\n";
const MARK_COPIES: &str = "import copy\n\nfrom pkg.marked_spec import Spec\n\n\ndef isolated(spec: Spec) -> Spec:\n    return copy.deepcopy(spec)\n";
const MARK_HOLDER: &str = "import copy\n\nfrom pkg.marked_spec import Spec\n\n\nclass Holder:\n    def __init__(self, spec: Spec):\n        self.spec = spec\n\n\ndef isolated_holder(holder: Holder):\n    return copy.deepcopy(holder)\n";
const MARK_SUB: &str = "import copy\n\nfrom pkg.marked_spec import Spec\n\n\nclass NamedSpec(Spec):\n    pass\n\n\ndef isolated_named(spec: NamedSpec) -> NamedSpec:\n    return copy.deepcopy(spec)\n";

// Changes lost on new objects (`MUT119`): a function in another module changes an item of what
// a property returns, whose getter returns a copy of its field, then the field itself, then a
// copy it keeps on the instance.
const LOST_SETTINGS: &str = "class Settings:\n    def __init__(self):\n        self._values = {}\n        self._last = None\n\n    @property\n    def values(self):\n        return dict(self._values)\n";
const LOST_SETTINGS_FIELD: &str = "class Settings:\n    def __init__(self):\n        self._values = {}\n        self._last = None\n\n    @property\n    def values(self):\n        return self._values\n";
const LOST_SETTINGS_KEPT: &str = "class Settings:\n    def __init__(self):\n        self._values = {}\n        self._last = None\n\n    @property\n    def values(self):\n        copied = dict(self._values)\n        self._last = copied\n        return copied\n";
const LOST_USE: &str = "from pkg.lost_settings import Settings\n\n\ndef enable(settings: Settings):\n    settings.values[\"debug\"] = True\n";
// An item store on what a function returns that returns what a function in a third module
// builds: a dict, then a set. Only the kind of the new container changes, no summary does.
const LOST_MAKE: &str = "def make():\n    return dict()\n";
const LOST_MAKE_SET: &str = "def make():\n    return set()\n";
const LOST_MIDDLE: &str = "from pkg.lost_make import make\n\n\ndef options():\n    return make()\n";
const LOST_ITEM: &str =
    "from pkg.lost_middle import options\n\n\ndef enable():\n    options()[\"debug\"] = True\n";
// An append to what a function returns that returns what a function in another module returns,
// which calls it back (one SCC, whose members a full build gives no fact from each other): the
// callee builds a set, then a list (the caller's facts stay the same), then stops calling back.
const LOST_LOOP_COLLECT: &str = "from pkg.lost_loop_rebuild import rebuilt\n\n\ndef collect(refresh):\n    if refresh:\n        rebuilt()\n    return set()\n";
const LOST_LOOP_COLLECT_NEW: &str = "from pkg.lost_loop_rebuild import rebuilt\n\n\ndef collect(refresh):\n    if refresh:\n        rebuilt()\n    return []\n";
const LOST_LOOP_COLLECT_ALONE: &str = "def collect(refresh):\n    return []\n";
const LOST_LOOP_REBUILD: &str = "from pkg.lost_loop_collect import collect\n\n\ndef rebuilt():\n    return collect(False)\n\n\ndef enable():\n    rebuilt().append(1)\n";

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

// The tiers of `EFF101` reports: an `async def` calls a method on a base class whose
// implementation always blocks; a subclass in a third module starts overriding it without the
// blocking call, then with another one, and the base's call becomes conditional.
const EXACT_BASE: &str = "import requests\n\n\nclass Base:\n    def fetch(self, url):\n        return requests.get(url)\n";
const EXACT_BASE_CONDITIONAL: &str = "import requests\n\n\nclass Base:\n    def fetch(self, url):\n        if url:\n            return requests.get(url)\n        return None\n";
const EXACT_BASE_QUIET: &str = "class Base:\n    def fetch(self, url):\n        return url\n";
const EXACT_SUB: &str = "class Quiet:\n    def fetch(self, url):\n        return url\n";
const EXACT_SUB_OVERRIDING: &str = "from pkg.exact_base import Base\n\n\nclass Quiet(Base):\n    def fetch(self, url):\n        return url\n";
const EXACT_SUB_BLOCKING: &str = "import requests\n\nfrom pkg.exact_base import Base\n\n\nclass Quiet(Base):\n    def fetch(self, url):\n        return requests.post(url)\n";
const EXACT_USE: &str =
    "from pkg.exact_base import Base\n\n\nasync def serve(b: Base):\n    b.fetch(\"u\")\n";
// A loop over the rows a function of another module returns (`FunctionDecl::returns`).
const ROWS_DB: &str = "import sqlite3\n\nCONN = sqlite3.connect(\"app.db\")\n\n\ndef authors():\n    return CONN.execute(\"SELECT id FROM a\").fetchall()\n\n\ndef book(i):\n    return CONN.execute(\"SELECT * FROM b WHERE a = ?\", (i,)).fetchone()\n";
const ROWS_DB_LIMITED: &str = "import sqlite3\n\nCONN = sqlite3.connect(\"app.db\")\n\n\ndef authors():\n    return CONN.execute(\"SELECT id FROM a LIMIT 3\").fetchall()\n\n\ndef book(i):\n    return CONN.execute(\"SELECT * FROM b WHERE a = ?\", (i,)).fetchone()\n";
const ROWS_DB_THROUGH: &str = "import sqlite3\n\nCONN = sqlite3.connect(\"app.db\")\n\n\ndef _all():\n    return CONN.execute(\"SELECT id FROM a\")\n\n\ndef authors():\n    return _all()\n\n\ndef book(i):\n    return CONN.execute(\"SELECT * FROM b WHERE a = ?\", (i,)).fetchone()\n";
// A connection a factory of another module returns (`ReturnFact::ty`), opened on a path a module
// constant names (`ModuleDecls::file_constants`).
const FACTORY_DB: &str = "import sqlite3\nfrom pathlib import Path\n\nDB_PATH = Path(__file__).parent / \"app.db\"\n\n\ndef connect() -> sqlite3.Connection:\n    return sqlite3.connect(DB_PATH)\n";
const FACTORY_DB_MEMORY: &str = "import sqlite3\nfrom pathlib import Path\n\nDB_PATH = Path(__file__).parent / \"app.db\"\n\n\ndef connect() -> sqlite3.Connection:\n    return sqlite3.connect(\":memory:\")\n";
const FACTORY_DB_UNKNOWN_PATH: &str = "import os\nimport sqlite3\n\nDB_PATH = os.environ[\"DB\"]\n\n\ndef connect() -> sqlite3.Connection:\n    return sqlite3.connect(DB_PATH)\n";
const FACTORY_USE: &str = "from pkg.factory_db import connect\n\n\nasync def serve():\n    return connect().execute(\"SELECT 1\")\n";
// A factory without a return annotation constructing a class of a third module
// (`ReturnFact::ty`): its callers get an instance of the class while it is one.
const MADE_MODELS: &str = "import time\n\n\nclass Waiter:\n    def wait(self) -> None:\n        time.sleep(1)\n\n\nclass Quiet:\n    def wait(self) -> None:\n        pass\n";
const MADE_MODELS_FUNCTION: &str =
    "def Waiter():\n    return None\n\n\nclass Quiet:\n    def wait(self) -> None:\n        pass\n";
const MADE_FACTORY: &str =
    "from pkg.made_models import Waiter\n\n\ndef make():\n    return Waiter()\n";
const MADE_FACTORY_EITHER: &str = "from pkg.made_models import Quiet, Waiter\n\n\ndef make(flag=False):\n    if flag:\n        return Quiet()\n    return Waiter()\n";
const MADE_USE: &str =
    "from pkg.made_factory import make\n\n\nasync def serve() -> None:\n    make().wait()\n";
// The same factory constructing the class by a member of its module (`made_models.Waiter()`):
// another file rebinding that member drops the class.
const MADE_MEMBER_FACTORY: &str =
    "from pkg import made_models\n\n\ndef make_member():\n    return made_models.Waiter()\n";
const MADE_MEMBER_USE: &str = "from pkg.made_member import make_member\n\n\nasync def serve_member() -> None:\n    make_member().wait()\n";
const MADE_MEMBER_REBINDING: &str = "from pkg import made_models\n\n\ndef swap() -> None:\n    made_models.Waiter = made_models.Quiet\n";
// A method returning an attribute of `self` that `__init__` binds to a class of a third module
// (`ReturnFact::self_attr`, `ProjectDecls::held_attr`): a store from another file (of another
// class in a subclass, through a receiver of the class, through a receiver of no known class,
// under a name heki cannot read) drops the class.
const HELD_ATTR: &str = "from pkg.made_models import Waiter\n\n\nclass Holder:\n    def __init__(self) -> None:\n        self._waiter = Waiter()\n\n    def waiter(self):\n        return self._waiter\n";
const HELD_ATTR_USE: &str = "from pkg.held_attr import Holder\n\n\nasync def serve_held(h: Holder) -> None:\n    h.waiter().wait()\n";
const HELD_ATTR_SUBCLASS: &str = "from pkg.held_attr import Holder\nfrom pkg.made_models import Quiet\n\n\nclass QuietHolder(Holder):\n    def __init__(self) -> None:\n        super().__init__()\n        self._waiter = Quiet()\n";
const HELD_ATTR_SWAP: &str = "from pkg.held_attr import Holder\nfrom pkg.made_models import Waiter\n\n\ndef renew(h: Holder) -> None:\n    h._waiter = Waiter()\n";
const HELD_ATTR_UNTYPED: &str = "def clear(h) -> None:\n    h._waiter = None\n";
const HELD_ATTR_UNNAMED: &str = "from pkg.made_models import Quiet\n\n\ndef replace(h, name) -> None:\n    setattr(h, name, Quiet())\n";
// An attribute a class's type parameter types (`ClassDecl::param_attrs`), read in another file
// on an instance whose annotation gives the argument (`Site[Waiter]`), and a base another
// module binds only under `if TYPE_CHECKING:` (`ModuleDecls::type_checking_only`): the class
// holds while those declarations say so, and a method found through that base is no exact
// target while the module binds the name only there.
const GENERIC_SITE: &str = "from typing import Generic, TypeVar\n\nT = TypeVar(\"T\")\n\n\nclass Site(Generic[T]):\n    obj: T\n";
const GENERIC_SITE_OBJECT: &str = "from typing import Generic, TypeVar\n\nT = TypeVar(\"T\")\n\n\nclass Site(Generic[T]):\n    obj: object\n";
const GENERIC_SITE_USE: &str = "from pkg.generic_site import Site\nfrom pkg.made_models import Waiter\n\n\nasync def serve_site(site: Site[Waiter]) -> None:\n    site.obj.wait()\n";
const LAZY_MODELS: &str = "from typing import TYPE_CHECKING, Any\n\nif TYPE_CHECKING:\n    from pkg.made_models import Waiter as Waiter\n\n\ndef __getattr__(name: str) -> Any:\n    from pkg import made_models\n\n    return getattr(made_models, name)\n";
const LAZY_MODELS_RUNTIME: &str = "from pkg.made_models import Waiter as Waiter\n";
const LAZY_MODELS_ELSE: &str = "from typing import TYPE_CHECKING, Any\n\nif TYPE_CHECKING:\n    from pkg.made_models import Waiter as Waiter\nelse:\n    Waiter: Any = object\n";
const LAZY_USE: &str = "from pkg.lazy_models import Waiter\n\n\nclass LazyWaiter(Waiter):\n    pass\n\n\nasync def serve_lazy(w: LazyWaiter) -> None:\n    w.wait()\n";
// A base another module serves from its `__getattr__` (`ModuleDecls::getattr_exports`): the
// class a factory read from a table by the name defines, while no other file rebinds the table,
// and the class object a factory annotated `-> type[Waiter]` gives, whose targets are heuristic.
const SERVED_MODELS: &str = "from typing import Any\n\n\ndef _waiter() -> type:\n    from pkg.made_models import Waiter\n\n    class _Served(Waiter):\n        pass\n\n    return _Served\n\n\n_EXPORTS = {\"Waiter\": _waiter}\n\n\ndef __getattr__(name: str) -> Any:\n    return _EXPORTS[name]()\n";
const SERVED_MODELS_DECLARED: &str = "from typing import Any\n\nfrom pkg.made_models import Waiter as _Waiter\n\n\ndef _waiter() -> type[_Waiter]:\n    return _Waiter\n\n\n_EXPORTS = {\"Waiter\": _waiter}\n\n\ndef __getattr__(name: str) -> Any:\n    return _EXPORTS[name]()\n";
const SERVED_USE: &str = "from pkg.served_models import Waiter\n\n\nclass ServedWaiter(Waiter):\n    pass\n\n\nasync def serve_served(w: ServedWaiter) -> None:\n    w.wait()\n";
const SERVED_PATCH: &str =
    "import pkg.served_models\n\n\ndef patch() -> None:\n    pkg.served_models._EXPORTS = {}\n";
const SERVED_PATCH_NONE: &str = "def patch() -> None:\n    return None\n";
// A base another module serves from the `__getattr__` a factory of a third module makes
// (`GetattrExports::made_by`, `ModuleDecls::hook_factories`): served while the factory's hook
// stores nothing into the module, and not while its `cache` default is true.
const HOOK_FACTORY: &str = "def _load(name, entry):\n    if isinstance(entry, str):\n        raise ImportError(name)\n    return entry()\n\n\ndef module_hooks(module_name, table, *, cache=False):\n    def module_getattr(name):\n        if name not in table:\n            raise AttributeError(name)\n        value = _load(name, table[name])\n        if cache:\n            setattr(__import__(module_name), name, value)\n        return value\n\n    return module_getattr, None\n";
const MADE_SERVED: &str = "from pkg.hook_factory import module_hooks\n\n\ndef _waiter() -> type:\n    from pkg.made_models import Waiter\n\n    class _Made(Waiter):\n        pass\n\n    return _Made\n\n\n_EXPORTS = {\"Waiter\": _waiter}\n__getattr__, __dir__ = module_hooks(__name__, _EXPORTS)\n";
const MADE_SERVED_USE: &str = "from pkg.made_served import Waiter\n\n\nclass MadeWaiter(Waiter):\n    pass\n\n\nasync def serve_made(w: MadeWaiter) -> None:\n    w.wait()\n";
// A factory function and a class attribute annotated with the callable they hold
// (`Callable[..., Waiter]`, `FunctionDecl::return_type`, `ClassDecl` attribute types): calling
// what they hold in other files gives a `Waiter` while the annotation names that one class.
const CALLABLE_FACTORY: &str = "from collections.abc import Callable\n\nfrom pkg.made_models import Waiter\n\n\ndef waiter_factory() -> Callable[..., Waiter]: ...\n\n\nclass Settings:\n    factory: Callable[..., Waiter]\n";
const CALLABLE_FACTORY_MAYBE: &str = "from collections.abc import Callable\n\nfrom pkg.made_models import Waiter\n\n\ndef waiter_factory() -> Callable[..., Waiter | None]: ...\n\n\nclass Settings:\n    factory: Callable[..., Waiter]\n";
const CALLABLE_FACTORY_EITHER: &str = "from collections.abc import Callable\n\nfrom pkg.made_models import Quiet, Waiter\n\n\ndef waiter_factory() -> Callable[..., Waiter]: ...\n\n\nclass Settings:\n    factory: Callable[..., Waiter | Quiet]\n";
const CALLABLE_USE: &str = "from pkg.callable_factory import waiter_factory\n\n\nasync def serve_factory() -> None:\n    waiter_factory()().wait()\n";
const CALLABLE_ATTRIBUTE_USE: &str = "from pkg.callable_factory import Settings\n\n\nasync def serve_attribute(s: Settings) -> None:\n    s.factory().wait()\n";
// Functions values hold, defined in one module and called in others: the closure a factory
// returns (`ReturnFact::ty`), a named tuple's field (`ClassDecl::init_fields`), and a
// `staticmethod(partial(...))` class attribute (`NamedAttr::body_partial`).
const HELD_DEFS: &str = "import time\nfrom collections.abc import Callable\nfrom functools import partial\nfrom typing import NamedTuple\n\n\ndef nap() -> None:\n    time.sleep(1)\n\n\ndef rest() -> None:\n    pass\n\n\ndef make_waiter() -> Callable[[], None]:\n    def waiter() -> None:\n        time.sleep(1)\n\n    return waiter\n\n\nclass View(NamedTuple):\n    run: Callable[[], None]\n\n\nclass Holder:\n    run = staticmethod(partial(nap))\n\n\ndef doze(seconds: float = 1.0) -> None:\n    time.sleep(seconds)\n\n\nTABLE = {\"doze\": partial(doze)}\n";
const HELD_DEFS_POSITIONED_TABLE: &str = "import time\nfrom collections.abc import Callable\nfrom functools import partial\nfrom typing import NamedTuple\n\n\ndef nap() -> None:\n    time.sleep(1)\n\n\ndef rest() -> None:\n    pass\n\n\ndef make_waiter() -> Callable[[], None]:\n    def waiter() -> None:\n        time.sleep(1)\n\n    return waiter\n\n\nclass View(NamedTuple):\n    run: Callable[[], None]\n\n\nclass Holder:\n    run = staticmethod(partial(nap))\n\n\ndef doze(seconds: float = 1.0) -> None:\n    time.sleep(seconds)\n\n\nTABLE = {\"doze\": partial(doze, 2.0)}\n";
const HELD_DEFS_QUIET_FACTORY: &str = "import time\nfrom collections.abc import Callable\nfrom functools import partial\nfrom typing import NamedTuple\n\n\ndef nap() -> None:\n    time.sleep(1)\n\n\ndef rest() -> None:\n    pass\n\n\ndef make_waiter() -> Callable[[], None]:\n    return rest\n\n\nclass View(NamedTuple):\n    run: Callable[[], None]\n\n\nclass Holder:\n    run = staticmethod(partial(nap))\n\n\ndef doze(seconds: float = 1.0) -> None:\n    time.sleep(seconds)\n\n\nTABLE = {\"doze\": partial(doze)}\n";
const HELD_DEFS_PLAIN_VIEW: &str = "import time\nfrom collections.abc import Callable\nfrom functools import partial\n\n\ndef nap() -> None:\n    time.sleep(1)\n\n\ndef rest() -> None:\n    pass\n\n\ndef make_waiter() -> Callable[[], None]:\n    def waiter() -> None:\n        time.sleep(1)\n\n    return waiter\n\n\nclass View:\n    def __init__(self, run: Callable[[], None]) -> None:\n        self.run = run\n\n\nclass Holder:\n    run = staticmethod(partial(nap))\n\n\ndef doze(seconds: float = 1.0) -> None:\n    time.sleep(seconds)\n\n\nTABLE = {\"doze\": partial(doze)}\n";
const HELD_DEFS_QUIET_PARTIAL: &str = "import time\nfrom collections.abc import Callable\nfrom functools import partial\nfrom typing import NamedTuple\n\n\ndef nap() -> None:\n    time.sleep(1)\n\n\ndef rest() -> None:\n    pass\n\n\ndef make_waiter() -> Callable[[], None]:\n    def waiter() -> None:\n        time.sleep(1)\n\n    return waiter\n\n\nclass View(NamedTuple):\n    run: Callable[[], None]\n\n\nclass Holder:\n    run = staticmethod(partial(rest))\n\n\ndef doze(seconds: float = 1.0) -> None:\n    time.sleep(seconds)\n\n\nTABLE = {\"doze\": partial(doze)}\n";
const HELD_FACTORY: &str = "from pkg.held_defs import make_waiter\n\n\nasync def through_factory() -> None:\n    make_waiter()()\n";
const HELD_FIELD: &str = "from pkg.held_defs import View, nap\n\n\nasync def through_field() -> None:\n    View(nap).run()\n";
const HELD_PARTIAL: &str = "from pkg.held_defs import Holder\n\n\nasync def through_partial() -> None:\n    Holder().run()\n";
const HELD_TABLE: &str = "from pkg.held_defs import TABLE\n\n\nasync def through_table(kind: str) -> None:\n    TABLE[kind]()\n";
const ROWS_USE: &str = "from pkg.rows_db import authors, book\n\n\ndef each():\n    for r in authors():\n        book(r[0])\n";
// The third module binds the base's method to another function: the call may run that
// function instead. The base's class body binds it to another function on every path: the call
// runs that function.
const EXACT_SUB_REBINDING: &str = "from pkg import exact_base\n\n\ndef quiet(self, url):\n    return url\n\n\nsetattr(exact_base.Base, \"fetch\", quiet)\n";
const EXACT_BASE_REBOUND: &str = "import requests\n\n\ndef _quiet(self, url):\n    return url\n\n\nclass Base:\n    def fetch(self, url):\n        return requests.get(url)\n\n    fetch = _quiet\n";
// The base's class body binds the method to another function on some paths: the call may run
// either, so the report is not exact.
const EXACT_BASE_QUIET_ON_A_PATH: &str = "import os\n\nimport requests\n\n\ndef _quiet(self, url):\n    return url\n\n\nclass Base:\n    def fetch(self, url):\n        return requests.get(url)\n\n    if os.environ.get(\"QUIET\"):\n        fetch = _quiet\n";
// A module binds a blocking function's name to another function on some paths, then on every
// path, read from another module.
const EXACT_FETCH: &str = "import requests\n\n\ndef fetch(url):\n    return requests.get(url)\n";
const EXACT_FETCH_ON_A_PATH: &str = "import os\n\nimport requests\n\n\ndef _quiet(url):\n    return url\n\n\ndef fetch(url):\n    return requests.get(url)\n\n\nif os.environ.get(\"QUIET\"):\n    fetch = _quiet\n";
const EXACT_FETCH_REBOUND: &str = "import requests\n\n\ndef _quiet(url):\n    return url\n\n\ndef fetch(url):\n    return requests.get(url)\n\n\nfetch = _quiet\n";
const EXACT_STAR: &str = "TIMEOUT = 1\n";
const EXACT_STAR_REBINDING: &str = "TIMEOUT = 1\n\n\ndef fetch(url):\n    return url\n";
const EXACT_FETCH_USE: &str =
    "from pkg import exact_fetch\n\n\nasync def serve():\n    exact_fetch.fetch(\"u\")\n";
// A class whose module star-imports another module after it: the import binds the class's name
// again once that module defines it, and may bind it to anything while the project has no such
// module; an instance made through the name is then of either class, or of none heki knows.
const EXACT_CLIENT: &str = "import requests\n\n\nclass Client:\n    def fetch(self, url):\n        return requests.get(url)\n\n\nfrom pkg.exact_rebind import *\n";
const EXACT_REBIND_CLIENT: &str = "class Client:\n    def fetch(self, url):\n        return url\n";
const EXACT_CLIENT_USE: &str = "from pkg import exact_client\n\n\nasync def serve():\n    exact_client.Client().fetch(\"u\")\n";
// A class a function binds under `global` (`ModuleDecls::made_in`): the module's name holds it
// once the module body runs the function on every path, and may be unbound when it runs on a
// branch; without the declaration the class is the function's own.
const EXACT_MADE: &str = "import time\n\n\ndef _define():\n    global Made\n\n    class Made:\n        def __init__(self):\n            time.sleep(1)\n\n\n_define()\n";
const EXACT_MADE_ON_A_BRANCH: &str = "import os\nimport time\n\n\ndef _define():\n    global Made\n\n    class Made:\n        def __init__(self):\n            time.sleep(1)\n\n\nif os.environ.get(\"MADE\"):\n    _define()\n";
const EXACT_MADE_LOCAL: &str = "import time\n\n\ndef _define():\n    class Made:\n        def __init__(self):\n            time.sleep(1)\n\n\n_define()\n";
const EXACT_MADE_USE: &str =
    "from pkg import exact_made\n\n\nasync def serve():\n    exact_made.Made()\n";
// The base's method gets decorators: `typing.final` returns the method it is handed, and a
// decorator outside the ones heki knows may replace it, also one named `final` that another
// library defines.
const EXACT_BASE_FINAL: &str = "from typing import final\n\nimport requests\n\n\nclass Base:\n    @final\n    def fetch(self, url):\n        return requests.get(url)\n";
const EXACT_BASE_OTHER_FINAL: &str = "from mylib import final\n\nimport requests\n\n\nclass Base:\n    @final\n    def fetch(self, url):\n        return requests.get(url)\n";
const EXACT_BASE_TRACED: &str = "from typing import final\n\nimport requests\n\n\ndef traced(fn):\n    return fn\n\n\nclass Base:\n    @final\n    @traced\n    def fetch(self, url):\n        return requests.get(url)\n";

// Store types (`ModuleDecls::global_types`, `ClassDecl::attr_stores`): an `async def` calls a
// method on a module global, in its module and through an import, and on an instance attribute;
// another function of the module, a third module, and a subclass in another file start and stop
// storing an async client there.
const ST_CLIENT: &str = "import httpx\n\nclient = httpx.Client()\n\n\ndef configure():\n    return None\n\n\nasync def handler():\n    client.get(\"u\")\n";
const ST_CLIENT_REBOUND: &str = "import httpx\n\nclient = httpx.Client()\n\n\ndef configure():\n    global client\n    client = httpx.AsyncClient()\n\n\nasync def handler():\n    client.get(\"u\")\n";
const ST_USE: &str =
    "from pkg.st_client import client\n\n\nasync def serve():\n    client.get(\"u\")\n";
const ST_REBIND: &str = "def configure():\n    return None\n";
const ST_REBIND_STORES: &str = "import httpx\n\nimport pkg.st_client\n\n\ndef configure():\n    pkg.st_client.client = httpx.AsyncClient()\n";
const ST_REBIND_SAME: &str = "import httpx\n\nimport pkg.st_client\n\n\ndef configure():\n    pkg.st_client.client = httpx.Client(timeout=1)\n";
const ST_HOLDER: &str = "import httpx\n\n\nclass Holder:\n    def __init__(self):\n        self.client = httpx.Client()\n\n    async def handle(self):\n        self.client.get(\"u\")\n";
const ST_CHILD: &str = "class Other:\n    pass\n";
const ST_CHILD_SWAPPING: &str = "import httpx\n\nfrom pkg.st_holder import Holder\n\n\nclass Child(Holder):\n    def swap(self):\n        self.client = httpx.AsyncClient()\n";
const ST_CHILD_SAME: &str = "import httpx\n\nfrom pkg.st_holder import Holder\n\n\nclass Child(Holder):\n    def swap(self):\n        self.client = httpx.Client(timeout=1)\n";
const ST_CHILD_ANY: &str = "import httpx\n\nfrom pkg.st_holder import Holder\n\n\nclass Child(Holder):\n    def swap(self, name, value):\n        setattr(self, name, value)\n";
const ST_CHILD_MIXED: &str = "from pkg.st_holder import Holder\nfrom pkg.st_mixin import Mixin\n\n\nclass Child(Mixin, Holder):\n    pass\n";
const ST_CHILD_PARAM: &str = "import httpx\n\nfrom pkg.st_holder import Holder\n\n\ndef use(holder: Holder):\n    holder.client = httpx.AsyncClient()\n";
const ST_MIXIN: &str = "class Mixin:\n    def swap(self):\n        return None\n";
const ST_MIXIN_SWAPPING: &str = "import httpx\n\n\nclass Mixin:\n    def swap(self):\n        self.client = httpx.AsyncClient()\n";
// A global whose stores read another (`active = client`): its type holds until another module
// stores into the global it reads.
const ST_ACTIVE: &str = "import httpx\n\nclient = httpx.Client()\nother = httpx.Client()\nactive = client\n\n\ndef activate():\n    global active\n    active = client\n\n\nasync def handler():\n    active.get(\"u\")\n";
const ST_ACTIVE_STORES: &str = "def configure():\n    return None\n";
const ST_ACTIVE_STORES_CLIENT: &str = "import httpx\n\nimport pkg.st_active\n\n\ndef configure():\n    pkg.st_active.client = httpx.AsyncClient()\n";
const ST_ACTIVE_STORES_OTHER: &str = "import httpx\n\nimport pkg.st_active\n\n\ndef configure():\n    pkg.st_active.other = httpx.AsyncClient()\n";
// An attribute whose stores read another (`self.active = self.client`): its type holds until
// another module stores into the attribute it reads.
const ST_ATTR_ACTIVE: &str = "import httpx\n\n\nclass Holder:\n    def __init__(self):\n        self.client = httpx.Client()\n        self.active = self.client\n\n    def activate(self):\n        self.active = self.client\n\n    async def handle(self):\n        self.active.get(\"u\")\n";
const ST_ATTR_STORES: &str = "def configure():\n    return None\n";
const ST_ATTR_STORES_CLIENT: &str = "import httpx\n\nfrom pkg.st_attr_active import Holder\n\n\ndef configure(holder: Holder):\n    holder.client = httpx.AsyncClient()\n";
const ST_ATTR_STORES_OTHER: &str = "import httpx\n\nfrom pkg.st_attr_active import Holder\n\n\ndef configure(holder: Holder):\n    holder.other = httpx.AsyncClient()\n";
// A store through an instance another module keeps in an attribute or a global, which only the
// stores of that attribute or global type.
const ST_KEEPER: &str = "def keep():\n    return None\n";
const ST_KEEPER_ATTR: &str = "import httpx\n\nfrom pkg.st_holder import Holder\n\n\nclass Keeper:\n    def __init__(self):\n        self.holder = Holder()\n\n    def swap(self):\n        self.holder.client = httpx.AsyncClient()\n";
const ST_KEEPER_GLOBAL: &str = "import httpx\n\nfrom pkg.st_holder import Holder\n\nholder = Holder()\n\n\ndef swap():\n    holder.client = httpx.AsyncClient()\n";
// A star import of a module whose `__all__` does not list the global keeps its type, until the
// module appends the global's name to its `__all__`.
const ST_STAR: &str = "import httpx\n\nclient = httpx.Client()\n\nfrom pkg.st_exporting import *  # noqa: E402, F403\n\n\nasync def handler():\n    client.get(\"u\")\n";
const ST_EXPORTING: &str = "__all__ = [\"other\"]\n\nother = 1\n";
const ST_EXPORTING_APPENDED: &str = "import httpx\n\n__all__ = [\"other\"]\n__all__.append(\"client\")\n\nother = 1\nclient = httpx.AsyncClient()\n";
// A global annotated with the library class of the connection it holds keeps where the
// connection was opened: its readers in other modules wait while it is opened on a file.
const ST_LITE: &str = "import sqlite3\n\nconn: sqlite3.Connection = sqlite3.connect(\"app.db\")\n";
const ST_LITE_IN_MEMORY: &str =
    "import sqlite3\n\nconn: sqlite3.Connection = sqlite3.connect(\":memory:\")\n";
const ST_LITE_USE: &str =
    "from pkg.st_lite import conn\n\n\nasync def serve():\n    conn.execute(\"SELECT 1\")\n";

// Method lookups (`ProjectDecls::method_read_runs_no_code`): a helper looks up a method before it
// sleeps, inside a `try` whose handler goes on; a subclass in another module comes and goes.
const ML_BASE: &str = "class Steady:\n    def ready(self):\n        return True\n";
const ML_USE: &str = "import time\n\nfrom pkg.ml_base import Steady\n\n\ndef wait_ready(s: Steady):\n    try:\n        s.ready\n        time.sleep(1)\n    except ValueError:\n        pass\n\n\nasync def serve(s: Steady):\n    wait_ready(s)\n";
const ML_SHADOW_GETTER: &str = "from pkg.ml_base import Steady\n\n\nclass Shadow(Steady):\n    @property\n    def ready(self):\n        raise ValueError(\"not ready\")\n";
const ML_SHADOW_METHOD: &str = "from pkg.ml_base import Steady\n\n\nclass Shadow(Steady):\n    def ready(self):\n        return False\n";
const ML_SHADOW_HOOKED: &str = "from pkg.ml_base import Steady\n\n\nclass Shadow(Steady):\n    def __getattribute__(self, name):\n        return object.__getattribute__(self, name)\n";

// Imported literal constants (`TypeCtx::global_literal`): an `async def` sleeps for a constant
// another module binds; the constant switches between no time and a wait, and a third module
// starts and stops rebinding it.
const LC_SETTINGS: &str = "PAUSE = 0\n";
const LC_SETTINGS_WAIT: &str = "PAUSE = 1\n";
const LC_SETTINGS_COMPUTED: &str = "BASE = 2\nPAUSE = BASE - 2\n";
const LC_USE: &str = "import time\n\nfrom pkg.lc_settings import PAUSE\n\n\nasync def nap():\n    time.sleep(PAUSE)\n";
const LC_REBIND: &str = "def configure():\n    return None\n";
const LC_REBIND_STORES: &str =
    "import pkg.lc_settings\n\n\ndef configure():\n    pkg.lc_settings.PAUSE = 5\n";

// Witnesses through a helper (`MustExec::alternatives`): a helper calls a method whose two
// implementations wait in different calls, the one in another module through a longer chain, so
// the helper's shortest chain is the other's. An `async def` reaches the helper through a module
// that imports neither: editing the far implementation's wait changes only an alternative
// witness, and narrowing the helper's parameter to the other implementation changes only the
// helper's own alternatives (its chain stays).
const WIT_REPO: &str = "import requests\n\n\nclass Repo:\n    def get(self, key):\n        raise NotImplementedError\n\n\nclass Remote(Repo):\n    def get(self, key):\n        return requests.get(key)\n";
const WIT_LOCAL: &str = "import requests\n\nfrom pkg.wit_repo import Repo\n\n\ndef _send(key):\n    return requests.post(key)\n\n\nclass Local(Repo):\n    def get(self, key):\n        return _send(key)\n";
const WIT_LOCAL_PUT: &str = "import requests\n\nfrom pkg.wit_repo import Repo\n\n\ndef _send(key):\n    return requests.put(key)\n\n\nclass Local(Repo):\n    def get(self, key):\n        return _send(key)\n";
const WIT_LOCAL_QUIET: &str = "from pkg.wit_repo import Repo\n\n\ndef _send(key):\n    return key\n\n\nclass Local(Repo):\n    def get(self, key):\n        return _send(key)\n";
const WIT_FETCH: &str =
    "from pkg.wit_repo import Repo\n\n\ndef fetch(r: Repo, key):\n    return r.get(key)\n";
const WIT_FETCH_REMOTE: &str =
    "from pkg.wit_repo import Remote\n\n\ndef fetch(r: Remote, key):\n    return r.get(key)\n";
const WIT_MID: &str =
    "from pkg.wit_fetch import fetch\n\n\ndef load(r):\n    return fetch(r, \"k\")\n";
const WSEL_KINDS: &str = "class Request:\n    pass\n\n\nclass Response:\n    pass\n";
const WSEL_KINDS_PROTOCOL: &str = "from typing import Protocol, runtime_checkable\n\n\nclass Request:\n    pass\n\n\n@runtime_checkable\nclass Response(Protocol):\n    def render(self) -> str: ...\n";
const WSEL_RENDER: &str = "import time\n\nfrom pkg.wsel_kinds import Response\n\n\ndef wait(item):\n    time.sleep(1)\n\n\ndef request_segments(item):\n    wait(item)\n\n\ndef response_segments(item):\n    time.sleep(1)\n\n\ndef append(item):\n    if isinstance(item, Response):\n        return response_segments(item)\n    return request_segments(item)\n";
const WSEL_RETRY: &str = "import time\n\n\ndef retry(send):\n    try:\n        send()\n        time.sleep(0.1)\n    except OSError:\n        time.sleep(1)\n";
const WSEL_RETRY_IN_HANDLER: &str = "import time\n\n\ndef retry(send):\n    try:\n        send()\n    except OSError:\n        time.sleep(1)\n";
const WSEL_SERVE: &str = "from pkg.wsel_kinds import Request\nfrom pkg.wsel_render import append\nfrom pkg.wsel_retry import retry\n\n\nasync def compact():\n    append(Request())\n\n\nasync def send():\n    retry(print)\n";
const WIT_SERVE: &str = "from pkg.wit_mid import load\nfrom pkg.wit_repo import Repo\n\n\nasync def serve(r: Repo):\n    load(r)\n";

// Queue capacities (a heuristic `EFF101` on `queue.Queue.put`, a wait on another thread): a queue one module builds and another
// module's class stores, an `async def` putting on its parameter whose caller is in another
// module, and a class constant bounding a queue that a subclass in another module may define
// again.
const Q_TELEMETRY: &str = "import queue\n\n\nclass Telemetry:\n    def __init__(self, events: queue.Queue | None):\n        self._events = events\n\n    def enqueue(self, item):\n        if self._events is not None:\n            self._events.put(item)\n";
const Q_OWNER: &str = "import queue\n\nfrom pkg.q_telemetry import Telemetry\n\n\nclass Topology:\n    def __init__(self):\n        self._events = queue.Queue()\n        self.telemetry = Telemetry(self._events)\n";
const Q_PUBLISH: &str = "from pkg.q_telemetry import Telemetry\n\n\nasync def publish(t: Telemetry):\n    t.enqueue(1)\n";
const Q_DRAIN: &str = "import queue\n\n\nasync def drain(q: queue.Queue):\n    q.put(1)\n";
const Q_RUN: &str = "import queue\n\nfrom pkg.q_drain import drain\n\n\nasync def run():\n    await drain(queue.Queue(5))\n";
const Q_RUNNER: &str = "import queue\n\n\nclass Runner:\n    _MAX = 16\n\n    def __init__(self):\n        self._queue = queue.Queue(maxsize=self._MAX)\n\n    def stop(self):\n        self._queue.put(None, timeout=1.0)\n";
const Q_SMALL: &str =
    "from pkg.q_runner import Runner\n\n\nclass SmallRunner(Runner):\n    label = \"small\"\n";
const Q_HALT: &str =
    "from pkg.q_runner import Runner\n\n\nasync def halt(r: Runner):\n    r.stop()\n";
const Q_EVENTS: &str = "import queue\n\nEVENTS = queue.Queue()\n";
const Q_MIRROR: &str = "import queue\n\nfrom pkg.q_events import EVENTS\n\nMINE: queue.Queue = EVENTS\n\n\nasync def mirror():\n    MINE.put(1)\n";
const Q_LOCAL: &str =
    "async def local():\n    from pkg.q_events import EVENTS\n\n    EVENTS.put(1)\n";

// Locks taken again (`EFF109`): a subclass method holding the lock its base class's helper in
// another module takes, and a function holding a module global that a helper takes only when
// its argument says so.
const LOCK_POOL: &str = "import threading\n\nLOCK = threading.Lock()\n\n\nclass Pool:\n    def __init__(self):\n        self._lock = threading.Lock()\n\n    def size(self):\n        with self._lock:\n            return 1\n\n\ndef refill(force=False):\n    if force:\n        with LOCK:\n            return 1\n    return 0\n";
const LOCK_POOL_PLAIN: &str = "import threading\n\nLOCK = threading.Lock()\n\n\nclass Pool:\n    def __init__(self):\n        self._lock = threading.Lock()\n\n    def size(self):\n        return 1\n\n\ndef refill(force=False):\n    if force:\n        with LOCK:\n            return 1\n    return 0\n";
const LOCK_USE: &str = "from pkg.lock_pool import LOCK, Pool, refill\n\n\nclass Spool(Pool):\n    def grow(self):\n        with self._lock:\n            return self.size()\n\n\ndef drain():\n    with LOCK:\n        return refill()\n";
const LOCK_USE_FORCED: &str = "from pkg.lock_pool import LOCK, Pool, refill\n\n\nclass Spool(Pool):\n    def grow(self):\n        return self.size()\n\n\ndef drain():\n    with LOCK:\n        return refill(force=True)\n";

// One reader two tasks use (`EFF110`): the coroutine in another module that reads it, and a
// function gathering two of them.
const FAN_READ: &str = "import asyncio\n\n\nasync def read_line(reader: asyncio.StreamReader):\n    return await reader.readline()\n";
const FAN_READ_NOTHING: &str =
    "import asyncio\n\n\nasync def read_line(reader: asyncio.StreamReader):\n    return b\"\"\n";
const FAN_READ_LOCKED: &str = "import asyncio\n\nLOCK = asyncio.Lock()\n\n\nasync def read_line(reader: asyncio.StreamReader):\n    async with LOCK:\n        return await reader.readline()\n";
const FAN_USE: &str = "import asyncio\n\nfrom pkg.fan_read import read_line\n\n\nasync def both(reader: asyncio.StreamReader):\n    return await asyncio.gather(read_line(reader), read_line(reader))\n";

// Kept values (`MUT101` / `MUT111` exact claims): a cached property reads `raw` through a helper
// of a base class in another file; the writer calls a method of that base class and a function of
// a third file after its write; a subclass in a fourth file holds no value of the property, and
// a reader in a fifth reads it.
const KEPT_BASE: &str = "class Base:\n    def _key(self) -> str:\n        return self.raw.get(\"k\", \"\")\n\n    def after(self) -> None:\n        return None\n";
const KEPT_BASE_ONE_BRANCH: &str = "class Base:\n    def _key(self) -> str:\n        if self.flag:\n            return self.raw.get(\"k\", \"\")\n        return \"\"\n\n    def after(self) -> None:\n        return None\n";
const KEPT_BASE_INVALIDATING: &str = "class Base:\n    def _key(self) -> str:\n        return self.raw.get(\"k\", \"\")\n\n    def after(self) -> None:\n        self.__dict__.pop(\"features\", None)\n";
const KEPT_DROP: &str = "def cleanup(obj) -> None:\n    return None\n";
const KEPT_DROP_DELETING: &str = "def cleanup(obj) -> None:\n    del obj.features\n";
const KEPT_DROP_STORING: &str = "from typing import TYPE_CHECKING\n\nif TYPE_CHECKING:\n    from pkg.kept_holder import Holder\n\n\ndef cleanup(obj: \"Holder\") -> None:\n    obj.features = None\n";
const KEPT_DROP_STORING_SIZE: &str = "from typing import TYPE_CHECKING\n\nif TYPE_CHECKING:\n    from pkg.kept_holder import Holder\n\n\ndef cleanup(obj: \"Holder\") -> None:\n    obj.size = 0\n";
const KEPT_HOLDER: &str = "from functools import cached_property\n\nfrom pkg.kept_base import Base\nfrom pkg.kept_drop import cleanup\n\n\nclass Holder(Base):\n    def __init__(self, raw: dict[str, str], flag: bool) -> None:\n        self.raw = raw\n        self.flag = flag\n\n    @cached_property\n    def features(self) -> str:\n        return self._key()\n\n    def replace(self, raw: dict[str, str]) -> None:\n        self.raw = raw\n        self.after()\n        cleanup(self)\n";
const KEPT_HOLDER_SIZE: &str = "from functools import cached_property\n\nfrom pkg.kept_base import Base\nfrom pkg.kept_drop import cleanup\n\n\nclass Holder(Base):\n    def __init__(self, raw: dict[str, str], flag: bool) -> None:\n        self.raw = raw\n        self.flag = flag\n\n    @cached_property\n    def features(self) -> str:\n        return self._key()\n\n    @cached_property\n    def size(self) -> int:\n        return len(self.raw)\n\n    def replace(self, raw: dict[str, str]) -> None:\n        self.raw = raw\n        self.after()\n        cleanup(self)\n";
const KEPT_SUB: &str = "from pkg.kept_holder import Holder\n\n\nclass Sub(Holder):\n    def extra(self) -> int:\n        return 1\n";
const KEPT_SUB_PLAIN: &str = "from pkg.kept_holder import Holder\n\n\nclass Sub(Holder):\n    @property\n    def features(self) -> str:\n        return \"plain\"\n";
const KEPT_LAZY: &str = "import functools\n\n\nclass Lazy:\n    def __init__(self, raw: dict[str, str]) -> None:\n        self.raw = raw\n\n    @functools.cached_property\n    def loaded(self) -> bool:\n        self.raw = {\"k\": \"v\"}\n        return True\n\n    @functools.cached_property\n    def features(self) -> str:\n        if self.loaded:\n            return self.raw.get(\"k\", \"\")\n        return self.raw.get(\"k\", \"\")\n";
const KEPT_FORGET: &str = "def forget(obj) -> None:\n    return None\n";
const KEPT_FORGET_DELETING: &str = "def forget(obj) -> None:\n    del obj.loaded\n";
const KEPT_READER: &str =
    "from pkg.kept_holder import Holder\n\n\ndef show(h: Holder) -> str:\n    return h.features\n";

// A dict built in a set's order in one module and written in another (`EFF112`): the builder
// sorts and stops, the writer sorts the keys and stops.
const ORDER_SRC: &str =
    "def table(d: dict[str, int]) -> dict[str, int]:\n    return {k: d[k] for k in set(d)}\n";
const ORDER_SRC_SORTED: &str = "def table(d: dict[str, int]) -> dict[str, int]:\n    return {k: d[k] for k in sorted(set(d))}\n";
const ORDER_USE: &str = "import json\nfrom pathlib import Path\n\nfrom pkg.order_src import table\n\n\ndef save(d: dict[str, int], path: Path) -> None:\n    path.write_text(json.dumps(table(d)))\n";
const ORDER_USE_SORTED: &str = "import json\nfrom pathlib import Path\n\nfrom pkg.order_src import table\n\n\ndef save(d: dict[str, int], path: Path) -> None:\n    path.write_text(json.dumps(table(d), sort_keys=True))\n";
// A clause around a call into another module (`EFF114`): the call cycle closes and opens there.
const REC_HANDLER: &str = "from pkg.rec_walk import descend\n\n\ndef visit(node):\n    return descend(node)\n\n\ndef measure(node):\n    try:\n        return visit(node)\n    except Exception:\n        return 0\n";
// A handler reporting the error and returning its exit status through a constant of another
// module (`EFF114`): the status is an integer, a string, or bound twice.
const REC_STATUS_CODES: &str = "FAILED = 4\n";
const REC_STATUS_CODES_TEXT: &str = "FAILED = \"4\"\n";
const REC_STATUS_CODES_REBOUND: &str = "FAILED = 4\nFAILED = 5\n";
const REC_STATUS: &str = "import sys\n\nfrom pkg.rec_status_codes import FAILED\n\n\ndef visit(node):\n    return sum(visit(child) for child in node)\n\n\ndef main(node):\n    try:\n        visit(node)\n        return 0\n    except Exception as error:\n        print(error, file=sys.stderr)\n        return FAILED\n";
const REC_WALK: &str = "def descend(node):\n    return len(node)\n";
const REC_WALK_BACK: &str = "from pkg import rec_handler\n\n\ndef descend(node):\n    return sum(rec_handler.visit(child) for child in node)\n";
// A function of another module handed to a thread and to `filter` (`EFF115`): it changes between
// `def` and `async def` there.
const CORO_JOBS: &str = "def flush(n):\n    return n\n";
const CORO_JOBS_ASYNC: &str = "async def flush(n):\n    return n\n";
const CORO_USE: &str = "import threading\n\nfrom pkg.coro_jobs import flush\n\n\ndef start(items):\n    threading.Thread(target=flush, args=(1,)).start()\n    return list(filter(flush, items))\n";
// Generators nothing iterates (`EFF116`): a function of another module changes between a plain
// function and a generator function, and so do a method and its override in a third module.
const GEN_JOBS: &str = "def rows(n):\n    return [n]\n";
const GEN_JOBS_GENERATOR: &str = "def rows(n):\n    yield n\n";
const GEN_BASE: &str = "class Source:\n    def rows(self):\n        yield 1\n";
const GEN_BASE_PLAIN: &str = "class Source:\n    def rows(self):\n        return [1]\n";
const GEN_SUB: &str = "from pkg.gen_base import Source\n\n\nclass Cached(Source):\n    def rows(self):\n        yield 2\n";
const GEN_SUB_PLAIN: &str = "from pkg.gen_base import Source\n\n\nclass Cached(Source):\n    def rows(self):\n        return [2]\n";
const GEN_USE: &str = "import threading\n\nfrom pkg.gen_base import Source\nfrom pkg.gen_jobs import rows\n\n\ndef start(n, source: Source):\n    rows(n)\n    threading.Thread(target=rows, args=(n,)).start()\n    source.rows()\n    if not rows(n):\n        return 0\n    return 1\n";

// State a thread writes holding a lock (`EFF117`): the thread's writer, the helper taking the
// lock, and the start of the thread are in the base class's module; a subclass in another module
// writes the state.
const LOCKED_BASE: &str = "import threading\n\n\nclass Counter:\n    def __init__(self):\n        self._lock = threading.Lock()\n        self.count = 0\n\n    def start(self):\n        threading.Thread(target=self._tick).start()\n\n    def _tick(self):\n        with self._lock:\n            self.count += 1\n\n    def _acquire(self):\n        self._lock.acquire()\n\n    def _give(self):\n        self._lock.release()\n";
const LOCKED_USE: &str = "from pkg.locked_base import Counter\n\n\nclass Resettable(Counter):\n    def reset(self):\n        self.count = 0\n";
const LOCKED_USE_HELD: &str = "from pkg.locked_base import Counter\n\n\nclass Resettable(Counter):\n    def reset(self):\n        self._acquire()\n        self.count = 0\n        self._give()\n";
// A module list one module appends to holding a lock and another clears holding none; a third
// module runs the appending function, directly or on a thread.
const LOCKED_LIST: &str = "import threading\n\n_lock = threading.Lock()\nseen: list[str] = []\n\n\ndef add(line: str) -> None:\n    with _lock:\n        seen.append(line)\n";
const LOCKED_LIST_CLEAR: &str =
    "from pkg import locked_list\n\n\ndef clear() -> None:\n    locked_list.seen.clear()\n";
const LOCKED_LIST_RUN: &str =
    "from pkg.locked_list import add\n\n\ndef run(line: str) -> None:\n    add(line)\n";

// A `finally` body awaiting a helper in another module that sleeps (`EFF118`), and a third module
// calling the function holding the cleanup.
const CLEANUP_HELPERS: &str =
    "import anyio\n\n\nasync def stop() -> None:\n    await anyio.sleep(1)\n";
const CLEANUP_USE: &str = "from pkg.cleanup_helpers import stop\n\n\nasync def close() -> None:\n    try:\n        pass\n    finally:\n        await stop()\n        print(\"stopped\")\n";
const CLEANUP_CALLER: &str = "import anyio\n\nfrom pkg.cleanup_use import close\n\n\nasync def shutdown() -> None:\n    with anyio.CancelScope(shield=True):\n        await close()\n";

// `MUT115`: the declarations of a copied class and of the builtin containers' subclasses in
// other modules, and a helper in another module, decide whether a change below a copy states
// the exact claim.
const COPIED_NODE: &str =
    "class Node:\n    def __init__(self) -> None:\n        self.children: list[int] = []\n";
const COPIED_NODE_HOOKED: &str = "class Node:\n    def __init__(self) -> None:\n        self.children: list[int] = []\n\n    def __copy__(self):\n        node = Node()\n        node.children = list(self.children)\n        return node\n";
const COPY_USER: &str = "import copy\n\nfrom pkg.copied_node import Node\n\n\ndef adopted(node: Node) -> Node:\n    clone = copy.copy(node)\n    clone.children.append(1)\n    return clone\n\n\ndef with_base(options: dict[str, list[str]]) -> None:\n    result = options.copy()\n    result[\"bases\"].append(\"x\")\n";
const COPY_HELPER: &str = "def grow(rows):\n    return len(rows)\n";
const COPY_HELPER_GROWING: &str = "def grow(rows):\n    rows.append(1)\n";
const COPY_HANDED: &str = "from pkg.copy_helper import grow\n\n\ndef handed(options: dict[str, list[int]]) -> None:\n    result = dict(options)\n    grow(result[\"rows\"])\n";
// A copy hook bound on the copied class after its body, from another module, and a decorator of
// another module that reads the class or binds a copy hook on it.
const COPY_PATCH: &str = "from pkg.copied_node import Node\n\n\ndef _copy(self):\n    return self\n\n\nNode.__copy__ = _copy\n";
const COPIED_NODE_DECORATED: &str = "from pkg.copy_deco import register\n\n\n@register\nclass Node:\n    def __init__(self) -> None:\n        self.children: list[int] = []\n";
const COPY_DECO: &str = "def register(cls):\n    print(cls.__name__)\n    return cls\n";
const COPY_DECO_HOOK: &str = "def _copy(self):\n    return self\n\n\ndef register(cls):\n    cls.__copy__ = _copy\n    return cls\n";
// A store under a name built at run time through a value that may be a class object: a name
// that is never a copy hook's, one that may be, and the same one through an instance.
const COPY_FILL_NO_HOOK: &str =
    "def fill(target, name, value):\n    setattr(target, f\"_cached_{name}\", value)\n";
const COPY_FILL_HOOK: &str =
    "def fill(target, name, value):\n    setattr(target, f\"__{name}__\", value)\n";
const COPY_FILL_INSTANCE: &str = "import types\n\n\ndef fill(name, value):\n    target = types.SimpleNamespace()\n    setattr(target, f\"__{name}__\", value)\n";

// `MUT202` through summaries of changes below a parameter's items: a helper two modules below
// changes an item of its own copy of what it is handed, then gives that item a new list first; a
// relay in between hands the helper a copy of its parameter.
const ITEM_TAG: &str = "def tag(x):\n    draft = dict(x)\n    draft[\"tags\"].append(1)\n";
const ITEM_TAG_REBINDING: &str =
    "def tag(x):\n    draft = dict(x)\n    draft[\"tags\"] = []\n    draft[\"tags\"].append(1)\n";
const ITEM_RELAY: &str = "from pkg.item_tag import tag\n\n\ndef relay(x):\n    tag(dict(x))\n";
const ITEM_USER: &str = "from pkg.catalog import Catalog\nfrom pkg.item_relay import relay\n\n\ndef use(c: Catalog):\n    relay(c.entry(\"k\"))\n";

/// The project before the first step (every file is opened).
fn initial_files() -> Vec<(&'static str, String)> {
    [
        ("pkg/__init__.py", ""),
        ("pkg/low.py", LOW),
        ("pkg/mid.py", MID),
        ("pkg/top.py", TOP),
        ("pkg/shapes.py", SHAPES),
        ("pkg/keys.py", KEYS),
        ("pkg/token_base.py", TOKEN_BASE),
        ("pkg/token_user.py", TOKEN_USER),
        ("pkg/remote.py", REMOTE),
        ("pkg/reader.py", READER),
        ("pkg/kept_base.py", KEPT_BASE),
        ("pkg/kept_drop.py", KEPT_DROP),
        ("pkg/kept_holder.py", KEPT_HOLDER),
        ("pkg/kept_sub.py", KEPT_SUB),
        ("pkg/kept_reader.py", KEPT_READER),
        ("pkg/kept_lazy.py", KEPT_LAZY),
        ("pkg/kept_forget.py", KEPT_FORGET),
        ("pkg/guard_net.py", GUARD_NET),
        ("pkg/guard_use.py", GUARD_USE),
        ("pkg/guard_default_use.py", GUARD_DEFAULT_USE),
        ("pkg/guard_conn.py", GUARD_CONN),
        ("pkg/catalog_nap.py", CATALOG_NAP),
        ("pkg/catalog_use.py", CATALOG_USE),
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
        ("pkg/held/__init__.py", ""),
        ("pkg/held/model.py", HELD_MODEL),
        ("pkg/held/make.py", HELD_MAKE),
        ("pkg/held/write.py", HELD_WRITE),
        ("pkg/roview/__init__.py", ""),
        ("pkg/roview/model.py", RO_VIEW),
        ("pkg/roview/helpers.py", RO_VIEW_HELPERS),
        ("pkg/roview/decide.py", RO_VIEW_DECIDE),
        ("pkg/ro/__init__.py", RO_INIT),
        ("pkg/ro/helpers.py", RO_HELPERS),
        ("pkg/ro/callers.py", RO_CALLERS),
        ("pkg/ro/low.py", RO_LOW),
        ("pkg/ro/mid.py", RO_MID),
        ("pkg/ro/stats.py", RO_STATS),
        ("pkg/ro/impl.py", RO_IMPL),
        ("pkg/ro/same.py", RO_SAME),
        ("pkg/ro/forward.py", RO_FORWARD),
        ("pkg/ro/forward_use.py", RO_FORWARD_USE),
        ("pkg/own_prepare.py", OWN_PREPARE),
        ("pkg/own_wrap.py", OWN_WRAP),
        ("pkg/own_use.py", OWN_USE),
        ("pkg/own_base.py", OWN_BASE),
        ("pkg/own_sub.py", OWN_SUB),
        ("pkg/lock_pool.py", LOCK_POOL),
        ("pkg/lock_use.py", LOCK_USE),
        ("pkg/fan_read.py", FAN_READ),
        ("pkg/fan_use.py", FAN_USE),
        ("pkg/order_src.py", ORDER_SRC),
        ("pkg/order_use.py", ORDER_USE),
        ("pkg/rec_handler.py", REC_HANDLER),
        ("pkg/rec_walk.py", REC_WALK),
        ("pkg/rec_status.py", REC_STATUS),
        ("pkg/rec_status_codes.py", REC_STATUS_CODES),
        ("pkg/coro_jobs.py", CORO_JOBS),
        ("pkg/coro_use.py", CORO_USE),
        ("pkg/locked_base.py", LOCKED_BASE),
        ("pkg/locked_use.py", LOCKED_USE),
        ("pkg/gen_jobs.py", GEN_JOBS),
        ("pkg/gen_base.py", GEN_BASE),
        ("pkg/gen_sub.py", GEN_SUB),
        ("pkg/gen_use.py", GEN_USE),
        ("pkg/plot.py", RO_PLOT),
        ("pkg/loops.py", LOOPS),
        ("pkg/compat.py", COMPAT),
        ("pkg/ledger.py", LEDGER),
        ("pkg/acct_ops.py", ACCT_OPS),
        ("pkg/accounts.py", ACCOUNTS),
        ("pkg/pure_ops.py", PURE_OPS),
        ("pkg/pure.py", PURE),
        ("pkg/portable_ops.py", PORTABLE_OPS),
        ("pkg/portable.py", PORTABLE),
        ("pkg/portable_boot.py", PORTABLE_BOOT),
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
        ("pkg/rows.py", ROWS),
        ("pkg/join_user.py", JOIN_USER),
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
        ("pkg/item_tag.py", ITEM_TAG),
        ("pkg/item_relay.py", ITEM_RELAY),
        ("pkg/item_user.py", ITEM_USER),
        ("pkg/lookup.py", LOOKUP),
        ("pkg/tagger.py", TAGGER),
        ("pkg/fields.py", FIELDS),
        ("pkg/fields_user.py", FIELDS_USER),
        ("pkg/glyphs.py", GLYPHS),
        ("pkg/glyph_user.py", GLYPH_USER),
        ("pkg/copied_node.py", COPIED_NODE),
        ("pkg/copy_user.py", COPY_USER),
        ("pkg/copy_helper.py", COPY_HELPER),
        ("pkg/copy_handed.py", COPY_HANDED),
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
        ("pkg/extras.py", EXTRAS),
        ("pkg/extras_pass.py", EXTRAS_PASS),
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
        ("pkg/restoring.py", RESTORING),
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
        ("pkg/marks.py", MARKS),
        ("pkg/marked_spec.py", MARKED_SPEC),
        ("pkg/mark_tests.py", MARK_TESTS),
        ("pkg/mark_copies.py", MARK_COPIES),
        ("pkg/lost_settings.py", LOST_SETTINGS),
        ("pkg/lost_use.py", LOST_USE),
        ("pkg/lost_make.py", LOST_MAKE),
        ("pkg/lost_middle.py", LOST_MIDDLE),
        ("pkg/lost_item.py", LOST_ITEM),
        ("pkg/lost_loop_collect.py", LOST_LOOP_COLLECT),
        ("pkg/lost_loop_rebuild.py", LOST_LOOP_REBUILD),
        ("pkg/fresh_ops.py", FRESH_OPS),
        ("pkg/fresh_build.py", FRESH_BUILD),
        ("pkg/guarded_keeps.py", GUARDED_KEEPS),
        ("pkg/guarded_build.py", GUARDED_BUILD),
        ("pkg/copy_ops.py", COPY_OPS),
        ("pkg/copy_pass.py", COPY_PASS),
        ("pkg/link_ops.py", LINK_OPS),
        ("pkg/link_pass.py", LINK_PASS),
        ("pkg/link_neg_pass.py", LINK_NEG_PASS),
        ("pkg/link_model.py", LINK_MODEL),
        ("pkg/link_model_pass.py", LINK_MODEL_PASS),
        ("pkg/owned_base.py", OWNED_BASE),
        ("pkg/owned_model.py", OWNED_MODEL),
        ("pkg/owned_pass.py", OWNED_PASS),
        ("pkg/owned_flag_pass.py", OWNED_FLAG_PASS),
        ("pkg/strict_ops.py", STRICT_OPS),
        ("pkg/strict_use.py", STRICT_USE),
        ("pkg/q_telemetry.py", Q_TELEMETRY),
        ("pkg/q_owner.py", Q_OWNER),
        ("pkg/q_publish.py", Q_PUBLISH),
        ("pkg/q_drain.py", Q_DRAIN),
        ("pkg/q_run.py", Q_RUN),
        ("pkg/q_runner.py", Q_RUNNER),
        ("pkg/q_small.py", Q_SMALL),
        ("pkg/q_halt.py", Q_HALT),
        ("pkg/q_events.py", Q_EVENTS),
        ("pkg/q_mirror.py", Q_MIRROR),
        ("pkg/q_local.py", Q_LOCAL),
        ("pkg/subscriptions.py", SUBSCRIPTIONS),
        ("pkg/sub_drop.py", SUB_DROP),
        ("pkg/sub_ops.py", SUB_OPS),
        ("pkg/tally.py", TALLY),
        ("pkg/tally_use.py", TALLY_USE),
        ("pkg/exact_base.py", EXACT_BASE),
        ("pkg/exact_sub.py", EXACT_SUB),
        ("pkg/exact_use.py", EXACT_USE),
        ("pkg/rows_db.py", ROWS_DB),
        ("pkg/rows_use.py", ROWS_USE),
        ("pkg/factory_db.py", FACTORY_DB),
        ("pkg/factory_use.py", FACTORY_USE),
        ("pkg/made_models.py", MADE_MODELS),
        ("pkg/made_factory.py", MADE_FACTORY),
        ("pkg/made_use.py", MADE_USE),
        ("pkg/held_defs.py", HELD_DEFS),
        ("pkg/held_factory.py", HELD_FACTORY),
        ("pkg/held_field.py", HELD_FIELD),
        ("pkg/held_partial.py", HELD_PARTIAL),
        ("pkg/held_table.py", HELD_TABLE),
        ("pkg/st_client.py", ST_CLIENT),
        ("pkg/st_use.py", ST_USE),
        ("pkg/st_rebind.py", ST_REBIND),
        ("pkg/st_holder.py", ST_HOLDER),
        ("pkg/st_child.py", ST_CHILD),
        ("pkg/st_mixin.py", ST_MIXIN),
        ("pkg/st_active.py", ST_ACTIVE),
        ("pkg/st_active_stores.py", ST_ACTIVE_STORES),
        ("pkg/st_attr_active.py", ST_ATTR_ACTIVE),
        ("pkg/st_attr_stores.py", ST_ATTR_STORES),
        ("pkg/st_keeper.py", ST_KEEPER),
        ("pkg/st_star.py", ST_STAR),
        ("pkg/st_exporting.py", ST_EXPORTING),
        ("pkg/st_lite.py", ST_LITE),
        ("pkg/st_lite_use.py", ST_LITE_USE),
        ("pkg/lc_settings.py", LC_SETTINGS),
        ("pkg/lc_use.py", LC_USE),
        ("pkg/ml_base.py", ML_BASE),
        ("pkg/ml_use.py", ML_USE),
        ("pkg/lc_rebind.py", LC_REBIND),
        ("pkg/wit_repo.py", WIT_REPO),
        ("pkg/wit_local.py", WIT_LOCAL),
        ("pkg/wit_fetch.py", WIT_FETCH),
        ("pkg/wit_mid.py", WIT_MID),
        ("pkg/wit_serve.py", WIT_SERVE),
        ("pkg/wsel_kinds.py", WSEL_KINDS),
        ("pkg/wsel_render.py", WSEL_RENDER),
        ("pkg/wsel_retry.py", WSEL_RETRY),
        ("pkg/wsel_serve.py", WSEL_SERVE),
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
    /// `(file, [(line, code)])`: every diagnostic the CLI reports in the file after the step, by
    /// its 1-based line and its code with the tier.
    reported: Option<(&'static str, Reported)>,
    /// Early Cutoff: no other file is re-analyzed or re-published.
    cutoff: bool,
    /// The edit changes a fact callers read, though no diagnostic shows it: the server
    /// re-analyzes a caller's file (the opposite of `cutoff`).
    propagates: bool,
    /// `(function, parameters)`: the container parameters `heki inspect` says the function
    /// only reads after the step.
    only_reads: Option<(&'static str, &'static [&'static str])>,
    /// `(function, parameter, kinds)`: the kinds of read `heki inspect` says reach the
    /// parameter's object after the step.
    reads: Option<(&'static str, &'static str, &'static [&'static str])>,
    /// `(function, [(type, records)])`: the registration contexts `heki inspect` gives the
    /// function after the step, with the slots of each change record under it.
    registered: Option<(&'static str, Registered)>,
}

type Registered = &'static [(&'static str, &'static [&'static str])];

type Reported = &'static [(u64, &'static str)];

impl Step {
    fn new(name: &'static str, actions: Vec<Action>) -> Self {
        Self {
            name,
            actions,
            present: &[],
            absent: &[],
            reported: None,
            cutoff: false,
            propagates: false,
            only_reads: None,
            reads: None,
            registered: None,
        }
    }

    fn registered(mut self, function: &'static str, contexts: Registered) -> Self {
        self.registered = Some((function, contexts));
        self
    }

    fn present(mut self, codes: &'static [(&'static str, &'static str)]) -> Self {
        self.present = codes;
        self
    }

    fn absent(mut self, codes: &'static [(&'static str, &'static str)]) -> Self {
        self.absent = codes;
        self
    }

    fn reported(mut self, file: &'static str, at: Reported) -> Self {
        self.reported = Some((file, at));
        self
    }

    fn cutoff(mut self) -> Self {
        self.cutoff = true;
        self
    }

    fn propagates(mut self) -> Self {
        self.propagates = true;
        self
    }

    fn only_reads(mut self, function: &'static str, params: &'static [&'static str]) -> Self {
        self.only_reads = Some((function, params));
        self
    }

    fn reads(
        mut self,
        function: &'static str,
        param: &'static str,
        kinds: &'static [&'static str],
    ) -> Self {
        self.reads = Some((function, param, kinds));
        self
    }
}

/// The tier of `MUT101` and of the `MUT111` reader behind it follows the facts other files give
/// the writer and the getter: a helper of a base class the getter reads through
/// (`KeptFacts::must_read_self_attrs`), a method of that class called after the write that
/// starts invalidating, a function of another file called after the write that starts removing
/// the kept value (`KeptFacts::removes_kept`), a subclass that defines the name as a plain
/// property (the cached property is no longer the first definition along its MRO), a function
/// of another file called after the write that stores the property's name on its argument, or a
/// name that a property defined later in another file comes to have (the stores a summary keeps
/// are those of names of property getters: `DeclKeys::property_names`), and a function anywhere
/// in the project that removes the value of a lazy setup's cached property by name
/// (`FileSummary::kept_removals`).
fn steps_kept_values() -> Vec<Step> {
    const BOTH: &[(&str, &str)] = &[
        ("pkg/kept_holder.py", "MUT101"),
        ("pkg/kept_reader.py", "MUT111"),
    ];
    vec![
        Step::new(
            "the base-class helper the getter calls reads the attribute on one branch only",
            vec![save("pkg/kept_base.py", KEPT_BASE_ONE_BRANCH)],
        )
        .present(BOTH),
        Step::new(
            "the base-class method called after the write starts invalidating the property",
            vec![save("pkg/kept_base.py", KEPT_BASE_INVALIDATING)],
        )
        .absent(BOTH),
        Step::new(
            "the base class is as before; a function of another file called after the write \
             starts removing the kept value",
            vec![
                save("pkg/kept_base.py", KEPT_BASE),
                save("pkg/kept_drop.py", KEPT_DROP_DELETING),
            ],
        )
        .present(BOTH),
        Step::new(
            "the function stores the property's name on its argument instead",
            vec![save("pkg/kept_drop.py", KEPT_DROP_STORING)],
        )
        .present(BOTH),
        Step::new(
            "the function stores a name no property has",
            vec![save("pkg/kept_drop.py", KEPT_DROP_STORING_SIZE)],
        )
        .present(BOTH),
        Step::new(
            "the holder's file adds a cached property of that name, reading the written \
             attribute: the store in the unchanged file replaces its value",
            vec![save("pkg/kept_holder.py", KEPT_HOLDER_SIZE)],
        )
        .present(BOTH),
        Step::new(
            "the holder is as before",
            vec![save("pkg/kept_holder.py", KEPT_HOLDER)],
        )
        .present(BOTH),
        Step::new(
            "the function stops; a subclass in another file defines the name as a plain property",
            vec![
                save("pkg/kept_drop.py", KEPT_DROP),
                save("pkg/kept_sub.py", KEPT_SUB_PLAIN),
            ],
        )
        .present(BOTH),
        Step::new(
            "the subclass is as before",
            vec![save("pkg/kept_sub.py", KEPT_SUB)],
        )
        .present(BOTH),
        Step::new(
            "a function of another file that nothing calls starts removing the value of a lazy \
             setup's cached property: its write is no setup step any more",
            vec![save("pkg/kept_forget.py", KEPT_FORGET_DELETING)],
        )
        .present(&[("pkg/kept_lazy.py", "MUT101")]),
        Step::new(
            "the function stops removing it",
            vec![save("pkg/kept_forget.py", KEPT_FORGET)],
        )
        .present(&[("pkg/kept_lazy.py", "MUT101")]),
    ]
}

/// Exact effect reports follow every implementation of a call across modules (the targets of
/// `CallSite::complete` read the subclasses of another file) and the must-facts of the callees
/// (`EffectSummary::must_exec`, compared by Early Cutoff), and every binding of the name called
/// that may be its last in the module or class body of another file, a star import after the
/// definition included, which binds what the module it imports defines
/// (`ProjectDecls::name_holds`), also for the class of an instance made through the name.
fn steps_exact_effects() -> Vec<Step> {
    vec![
        Step::new(
            "a subclass in another file overrides the method without blocking",
            vec![save("pkg/exact_sub.py", EXACT_SUB_OVERRIDING)],
        )
        .present(&[("pkg/exact_use.py", "EFF101 [heuristic]")])
        .absent(&[("pkg/exact_use.py", "EFF101")]),
        Step::new(
            "the override blocks too",
            vec![save("pkg/exact_sub.py", EXACT_SUB_BLOCKING)],
        )
        .present(&[("pkg/exact_use.py", "EFF101")])
        .absent(&[("pkg/exact_use.py", "EFF101 [heuristic]")]),
        Step::new(
            "the base method blocks only on some paths",
            vec![save("pkg/exact_base.py", EXACT_BASE_CONDITIONAL)],
        )
        .present(&[("pkg/exact_use.py", "EFF101 [heuristic]")])
        .absent(&[("pkg/exact_use.py", "EFF101")]),
        Step::new(
            "the base method blocks on every path again, and the override stops overriding",
            vec![
                save("pkg/exact_base.py", EXACT_BASE),
                save("pkg/exact_sub.py", EXACT_SUB),
            ],
        )
        .present(&[("pkg/exact_use.py", "EFF101")])
        .absent(&[("pkg/exact_use.py", "EFF101 [heuristic]")]),
        // `ModuleDecls::rebound_globals` of another module: a name bound again leaves the call's
        // targets incomplete (`ProjectDecls::lookup_rebound`). Bound again on every path of the
        // base's class body, the name holds the other function (`NamedAttr::body`).
        Step::new(
            "the third module binds the base's method to another function",
            vec![save("pkg/exact_sub.py", EXACT_SUB_REBINDING)],
        )
        .present(&[("pkg/exact_use.py", "EFF101 [heuristic]")])
        .absent(&[("pkg/exact_use.py", "EFF101")]),
        Step::new(
            "it stops binding the method",
            vec![save("pkg/exact_sub.py", EXACT_SUB)],
        )
        .present(&[("pkg/exact_use.py", "EFF101")])
        .absent(&[("pkg/exact_use.py", "EFF101 [heuristic]")]),
        Step::new(
            "the base's class body binds the method to another function",
            vec![save("pkg/exact_base.py", EXACT_BASE_REBOUND)],
        )
        .absent(&[
            ("pkg/exact_use.py", "EFF101"),
            ("pkg/exact_use.py", "EFF101 [heuristic]"),
        ]),
        Step::new(
            "the base binds it once again",
            vec![save("pkg/exact_base.py", EXACT_BASE)],
        )
        .present(&[("pkg/exact_use.py", "EFF101")])
        .absent(&[("pkg/exact_use.py", "EFF101 [heuristic]")]),
        Step::new(
            "the base's class body binds the method to another function on some paths",
            vec![save("pkg/exact_base.py", EXACT_BASE_QUIET_ON_A_PATH)],
        )
        .present(&[("pkg/exact_use.py", "EFF101 [heuristic]")])
        .absent(&[("pkg/exact_use.py", "EFF101")]),
        Step::new(
            "the base defines the method once again, and a module calls a blocking function",
            vec![
                save("pkg/exact_base.py", EXACT_BASE),
                Action::Create("pkg/exact_fetch.py", EXACT_FETCH.to_owned()),
                Action::Create("pkg/exact_fetch_use.py", EXACT_FETCH_USE.to_owned()),
            ],
        )
        .present(&[
            ("pkg/exact_use.py", "EFF101"),
            ("pkg/exact_fetch_use.py", "EFF101"),
        ]),
        Step::new(
            "the function's module binds its name to another function on some paths",
            vec![save("pkg/exact_fetch.py", EXACT_FETCH_ON_A_PATH)],
        )
        .present(&[("pkg/exact_fetch_use.py", "EFF101 [heuristic]")])
        .absent(&[("pkg/exact_fetch_use.py", "EFF101")]),
        Step::new(
            "the module binds the name to the other function on every path",
            vec![save("pkg/exact_fetch.py", EXACT_FETCH_REBOUND)],
        )
        .absent(&[
            ("pkg/exact_fetch_use.py", "EFF101"),
            ("pkg/exact_fetch_use.py", "EFF101 [heuristic]"),
        ]),
        Step::new(
            "the function's module star-imports a module binding no such name after the function",
            vec![
                save(
                    "pkg/exact_fetch.py",
                    &format!("{EXACT_FETCH}\n\nfrom pkg.exact_star import *\n"),
                ),
                Action::Create("pkg/exact_star.py", EXACT_STAR.to_owned()),
            ],
        )
        .present(&[("pkg/exact_fetch_use.py", "EFF101")])
        .absent(&[("pkg/exact_fetch_use.py", "EFF101 [heuristic]")]),
        Step::new(
            "the star-imported module starts defining the function's name",
            vec![save("pkg/exact_star.py", EXACT_STAR_REBINDING)],
        )
        .present(&[("pkg/exact_fetch_use.py", "EFF101 [heuristic]")])
        .absent(&[("pkg/exact_fetch_use.py", "EFF101")]),
        Step::new(
            "the star-imported module stops defining it",
            vec![save("pkg/exact_star.py", EXACT_STAR)],
        )
        .present(&[("pkg/exact_fetch_use.py", "EFF101")])
        .absent(&[("pkg/exact_fetch_use.py", "EFF101 [heuristic]")]),
        Step::new(
            "a class's module star-imports a module the project does not have after the class",
            vec![
                Action::Create("pkg/exact_client.py", EXACT_CLIENT.to_owned()),
                Action::Create("pkg/exact_client_use.py", EXACT_CLIENT_USE.to_owned()),
            ],
        )
        .absent(&[
            ("pkg/exact_client_use.py", "EFF101"),
            ("pkg/exact_client_use.py", "EFF101 [heuristic]"),
        ]),
        Step::new(
            "the star-imported module is added, binding no such name",
            vec![Action::Create("pkg/exact_rebind.py", EXACT_STAR.to_owned())],
        )
        .present(&[("pkg/exact_client_use.py", "EFF101")])
        .absent(&[("pkg/exact_client_use.py", "EFF101 [heuristic]")]),
        Step::new(
            "it starts defining the class's name",
            vec![save("pkg/exact_rebind.py", EXACT_REBIND_CLIENT)],
        )
        .present(&[("pkg/exact_client_use.py", "EFF101 [heuristic]")])
        .absent(&[("pkg/exact_client_use.py", "EFF101")]),
        Step::new(
            "the star-imported module is removed",
            vec![Action::Delete("pkg/exact_rebind.py")],
        )
        .absent(&[
            ("pkg/exact_client_use.py", "EFF101"),
            ("pkg/exact_client_use.py", "EFF101 [heuristic]"),
        ]),
        Step::new(
            "a module binds a class under `global` in a function it runs on every path",
            vec![
                Action::Create("pkg/exact_made.py", EXACT_MADE.to_owned()),
                Action::Create("pkg/exact_made_use.py", EXACT_MADE_USE.to_owned()),
            ],
        )
        .present(&[("pkg/exact_made_use.py", "EFF101")]),
        Step::new(
            "it runs the function on a branch only",
            vec![save("pkg/exact_made.py", EXACT_MADE_ON_A_BRANCH)],
        )
        .present(&[("pkg/exact_made_use.py", "EFF101 [heuristic]")])
        .absent(&[("pkg/exact_made_use.py", "EFF101")]),
        Step::new(
            "the class stays local to the function",
            vec![save("pkg/exact_made.py", EXACT_MADE_LOCAL)],
        )
        .absent(&[
            ("pkg/exact_made_use.py", "EFF101"),
            ("pkg/exact_made_use.py", "EFF101 [heuristic]"),
        ]),
        Step::new(
            "the function binds it under `global` again",
            vec![save("pkg/exact_made.py", EXACT_MADE)],
        )
        .present(&[("pkg/exact_made_use.py", "EFF101")])
        .absent(&[("pkg/exact_made_use.py", "EFF101 [heuristic]")]),
        Step::new(
            "the base's method is decorated with `typing.final`",
            vec![save("pkg/exact_base.py", EXACT_BASE_FINAL)],
        )
        .present(&[("pkg/exact_use.py", "EFF101")])
        .absent(&[("pkg/exact_use.py", "EFF101 [heuristic]")]),
        Step::new(
            "a decorator heki does not know is added below it",
            vec![save("pkg/exact_base.py", EXACT_BASE_TRACED)],
        )
        .present(&[("pkg/exact_use.py", "EFF101 [heuristic]")])
        .absent(&[("pkg/exact_use.py", "EFF101")]),
        Step::new(
            "only `typing.final` is left",
            vec![save("pkg/exact_base.py", EXACT_BASE_FINAL)],
        )
        .present(&[("pkg/exact_use.py", "EFF101")])
        .absent(&[("pkg/exact_use.py", "EFF101 [heuristic]")]),
        Step::new(
            "`final` is imported from another library",
            vec![save("pkg/exact_base.py", EXACT_BASE_OTHER_FINAL)],
        )
        .present(&[("pkg/exact_use.py", "EFF101 [heuristic]")])
        .absent(&[("pkg/exact_use.py", "EFF101")]),
        Step::new(
            "`final` is imported from `typing` again",
            vec![save("pkg/exact_base.py", EXACT_BASE_FINAL)],
        )
        .present(&[("pkg/exact_use.py", "EFF101")])
        .absent(&[("pkg/exact_use.py", "EFF101 [heuristic]")]),
    ]
}

/// A call of a factory in another file gives what every return of the factory gives
/// (`ReturnFact::ty`): a connection on the file a module constant names, one in memory, or, on a
/// path heki cannot decide, a connection in an unknown state; for a factory without a return
/// annotation, an instance of the class of a third module that every return constructs, while
/// every return constructs that one and it is a class; for a method returning an attribute of
/// `self`, the class the class's own methods bind it to, while no other file stores into it; and
/// a call of what a factory or an attribute annotated `Callable[..., Waiter]` holds, while that
/// annotation names one class.
fn steps_returned_types() -> Vec<Step> {
    const MADE: &[(&str, &str)] = &[("pkg/made_use.py", "EFF101")];
    const MEMBER: &[(&str, &str)] = &[("pkg/made_member_use.py", "EFF101")];
    const HELD: &[(&str, &str)] = &[("pkg/held_attr_use.py", "EFF101")];
    const EXACT: &[(&str, &str)] = &[("pkg/factory_use.py", "EFF101")];
    const HEURISTIC: &[(&str, &str)] = &[("pkg/factory_use.py", "EFF101 [heuristic]")];
    const NEITHER: &[(&str, &str)] = &[
        ("pkg/factory_use.py", "EFF101"),
        ("pkg/factory_use.py", "EFF101 [heuristic]"),
    ];
    const CALLABLE_FACTORY_CALL: &[(&str, &str)] = &[("pkg/callable_use.py", "EFF101")];
    const CALLABLE_ATTRIBUTE: &[(&str, &str)] = &[("pkg/callable_attribute_use.py", "EFF101")];
    const CALLABLE_BOTH: &[(&str, &str)] = &[
        ("pkg/callable_use.py", "EFF101"),
        ("pkg/callable_attribute_use.py", "EFF101"),
    ];
    vec![
        Step::new(
            "the factory opens a database in memory",
            vec![save("pkg/factory_db.py", FACTORY_DB_MEMORY)],
        )
        .absent(NEITHER),
        Step::new(
            "it opens the path an environment variable gives",
            vec![save("pkg/factory_db.py", FACTORY_DB_UNKNOWN_PATH)],
        )
        .present(HEURISTIC)
        .absent(EXACT),
        Step::new(
            "it opens the file its path constant names again",
            vec![save("pkg/factory_db.py", FACTORY_DB)],
        )
        .present(EXACT)
        .absent(HEURISTIC),
        Step::new(
            "a comment above the unannotated factory",
            vec![save(
                "pkg/made_factory.py",
                &format!("# factories\n{MADE_FACTORY}"),
            )],
        )
        .present(MADE)
        .cutoff(),
        Step::new(
            "the unannotated factory returns one of two classes",
            vec![save("pkg/made_factory.py", MADE_FACTORY_EITHER)],
        )
        .absent(MADE),
        Step::new(
            "it constructs the waiting class on every path again",
            vec![save("pkg/made_factory.py", MADE_FACTORY)],
        )
        .present(MADE),
        Step::new(
            "the class it constructs becomes a function",
            vec![save("pkg/made_models.py", MADE_MODELS_FUNCTION)],
        )
        .absent(MADE),
        Step::new(
            "the class is defined again",
            vec![save("pkg/made_models.py", MADE_MODELS)],
        )
        .present(MADE),
        Step::new(
            "a factory constructs the class by a member of its module",
            vec![
                Action::Create("pkg/made_member.py", MADE_MEMBER_FACTORY.to_owned()),
                Action::Create("pkg/made_member_use.py", MADE_MEMBER_USE.to_owned()),
            ],
        )
        .present(MEMBER),
        Step::new(
            "another file rebinds that member",
            vec![Action::Create(
                "pkg/made_member_rebinding.py",
                MADE_MEMBER_REBINDING.to_owned(),
            )],
        )
        .absent(MEMBER),
        Step::new(
            "the rebinding is removed",
            vec![Action::Delete("pkg/made_member_rebinding.py")],
        )
        .present(MEMBER),
        Step::new(
            "a method returns an attribute of self that __init__ binds to the waiting class",
            vec![
                Action::Create("pkg/held_attr.py", HELD_ATTR.to_owned()),
                Action::Create("pkg/held_attr_use.py", HELD_ATTR_USE.to_owned()),
            ],
        )
        .present(HELD),
        Step::new(
            "a subclass in another file stores another class there",
            vec![Action::Create(
                "pkg/held_attr_subclass.py",
                HELD_ATTR_SUBCLASS.to_owned(),
            )],
        )
        .absent(HELD),
        Step::new(
            "the subclass is removed",
            vec![Action::Delete("pkg/held_attr_subclass.py")],
        )
        .present(HELD),
        Step::new(
            "another file stores the same class through a receiver of the class",
            vec![Action::Create(
                "pkg/held_attr_swap.py",
                HELD_ATTR_SWAP.to_owned(),
            )],
        )
        .absent(HELD),
        Step::new(
            "that store is removed",
            vec![Action::Delete("pkg/held_attr_swap.py")],
        )
        .present(HELD),
        Step::new(
            "another file stores the attribute through a receiver of no known class",
            vec![Action::Create(
                "pkg/held_attr_untyped.py",
                HELD_ATTR_UNTYPED.to_owned(),
            )],
        )
        .absent(HELD),
        Step::new(
            "that store is removed",
            vec![Action::Delete("pkg/held_attr_untyped.py")],
        )
        .present(HELD),
        Step::new(
            "another file stores under a name it cannot read through a receiver of no known class",
            vec![Action::Create(
                "pkg/held_attr_unnamed.py",
                HELD_ATTR_UNNAMED.to_owned(),
            )],
        )
        .absent(HELD),
        Step::new(
            "that store is removed",
            vec![Action::Delete("pkg/held_attr_unnamed.py")],
        )
        .present(HELD),
        Step::new(
            "a factory and an attribute hold a callable giving the waiting class",
            vec![
                Action::Create("pkg/callable_factory.py", CALLABLE_FACTORY.to_owned()),
                Action::Create("pkg/callable_use.py", CALLABLE_USE.to_owned()),
                Action::Create(
                    "pkg/callable_attribute_use.py",
                    CALLABLE_ATTRIBUTE_USE.to_owned(),
                ),
            ],
        )
        .present(CALLABLE_BOTH),
        Step::new(
            "a comment above the factory",
            vec![save(
                "pkg/callable_factory.py",
                &format!("# factories\n{CALLABLE_FACTORY}"),
            )],
        )
        .present(CALLABLE_BOTH)
        .cutoff(),
        Step::new(
            "the factory's callable may give None",
            vec![save("pkg/callable_factory.py", CALLABLE_FACTORY_MAYBE)],
        )
        .present(CALLABLE_ATTRIBUTE)
        .absent(CALLABLE_FACTORY_CALL),
        Step::new(
            "the attribute's callable gives either of two classes",
            vec![save("pkg/callable_factory.py", CALLABLE_FACTORY_EITHER)],
        )
        .present(CALLABLE_FACTORY_CALL)
        .absent(CALLABLE_ATTRIBUTE),
        Step::new(
            "both give the waiting class again",
            vec![save("pkg/callable_factory.py", CALLABLE_FACTORY)],
        )
        .present(CALLABLE_BOTH),
    ]
}

/// A read in another file of an attribute annotated with a type parameter of its class, on an
/// instance annotated with an argument, while the class's annotation names the parameter; a
/// class based on a name another module binds only under `if TYPE_CHECKING:`, while that module
/// binds it no other way: the wait its method makes is heuristic until the module imports the
/// name at run time; and a class based on a name another module's `__getattr__` serves, exact
/// while no file rebinds what the hook reads and heuristic while it serves a class object, also
/// where the hook is what a factory of a third module makes, while that hook stores nothing.
fn steps_declared_types() -> Vec<Step> {
    const SITE: &[(&str, &str)] = &[("pkg/generic_site_use.py", "EFF101")];
    const LAZY: &[(&str, &str)] = &[("pkg/lazy_use.py", "EFF101 [heuristic]")];
    const LAZY_EXACT: &[(&str, &str)] = &[("pkg/lazy_use.py", "EFF101")];
    const SERVED: &[(&str, &str)] = &[("pkg/served_use.py", "EFF101")];
    const SERVED_HEURISTIC: &[(&str, &str)] = &[("pkg/served_use.py", "EFF101 [heuristic]")];
    const MADE: &[(&str, &str)] = &[("pkg/made_use.py", "EFF101")];
    vec![
        Step::new(
            "an attribute typed by the class's type parameter holds the waiting class",
            vec![
                Action::Create("pkg/generic_site.py", GENERIC_SITE.to_owned()),
                Action::Create("pkg/generic_site_use.py", GENERIC_SITE_USE.to_owned()),
            ],
        )
        .present(SITE),
        Step::new(
            "the attribute is annotated with object",
            vec![save("pkg/generic_site.py", GENERIC_SITE_OBJECT)],
        )
        .absent(SITE),
        Step::new(
            "the type parameter again",
            vec![save("pkg/generic_site.py", GENERIC_SITE)],
        )
        .present(SITE),
        Step::new(
            "a base bound only under TYPE_CHECKING in another module",
            vec![
                Action::Create("pkg/lazy_models.py", LAZY_MODELS.to_owned()),
                Action::Create("pkg/lazy_use.py", LAZY_USE.to_owned()),
            ],
        )
        .present(LAZY),
        Step::new(
            "that module also binds the name at run time",
            vec![save("pkg/lazy_models.py", LAZY_MODELS_ELSE)],
        )
        .absent(LAZY),
        Step::new(
            "only the TYPE_CHECKING import again",
            vec![save("pkg/lazy_models.py", LAZY_MODELS)],
        )
        .present(LAZY),
        Step::new(
            "that module imports the name at run time",
            vec![save("pkg/lazy_models.py", LAZY_MODELS_RUNTIME)],
        )
        .present(LAZY_EXACT)
        .absent(LAZY),
        Step::new(
            "the import only under TYPE_CHECKING once more",
            vec![save("pkg/lazy_models.py", LAZY_MODELS)],
        )
        .present(LAZY)
        .absent(LAZY_EXACT),
        Step::new(
            "a base another module serves from its __getattr__",
            vec![
                Action::Create("pkg/served_models.py", SERVED_MODELS.to_owned()),
                Action::Create("pkg/served_use.py", SERVED_USE.to_owned()),
            ],
        )
        .present(SERVED),
        Step::new(
            "another file rebinds the table the hook reads",
            vec![Action::Create(
                "pkg/served_patch.py",
                SERVED_PATCH.to_owned(),
            )],
        )
        .absent(SERVED)
        .absent(SERVED_HEURISTIC),
        Step::new(
            "that file no longer rebinds it",
            vec![save("pkg/served_patch.py", SERVED_PATCH_NONE)],
        )
        .present(SERVED),
        Step::new(
            "the factory is annotated with the class object instead",
            vec![save("pkg/served_models.py", SERVED_MODELS_DECLARED)],
        )
        .present(SERVED_HEURISTIC)
        .absent(SERVED),
        Step::new(
            "the factory defines the class again",
            vec![save("pkg/served_models.py", SERVED_MODELS)],
        )
        .present(SERVED)
        .absent(SERVED_HEURISTIC),
        Step::new(
            "a base served by the __getattr__ a factory of another module makes",
            vec![
                Action::Create("pkg/hook_factory.py", HOOK_FACTORY.to_owned()),
                Action::Create("pkg/made_served.py", MADE_SERVED.to_owned()),
                Action::Create("pkg/made_use.py", MADE_SERVED_USE.to_owned()),
            ],
        )
        .present(MADE),
        Step::new(
            "the factory's hook stores what it serves by default",
            vec![save(
                "pkg/hook_factory.py",
                &HOOK_FACTORY.replace("cache=False", "cache=True"),
            )],
        )
        .absent(MADE),
        Step::new(
            "the factory's hook stores nothing by default again",
            vec![save("pkg/hook_factory.py", HOOK_FACTORY)],
        )
        .present(MADE),
    ]
}

/// The function a value holds, decided in the module defining it: a factory returning a function
/// that does not wait, a class that is no named tuple (whose attribute holds what a parameter
/// declares), and a partial class attribute of a function that does not wait each take the wait
/// away from their callers in other modules, and the definitions written back bring it back.
fn steps_held_functions() -> Vec<Step> {
    const FACTORY: &[(&str, &str)] = &[("pkg/held_factory.py", "EFF101")];
    const FIELD: &[(&str, &str)] = &[("pkg/held_field.py", "EFF101 [heuristic]")];
    const PARTIAL: &[(&str, &str)] = &[("pkg/held_partial.py", "EFF101")];
    const TABLE: &[(&str, &str)] = &[("pkg/held_table.py", "EFF101")];
    const ALL: &[(&str, &str)] = &[
        ("pkg/held_factory.py", "EFF101"),
        ("pkg/held_field.py", "EFF101 [heuristic]"),
        ("pkg/held_partial.py", "EFF101"),
    ];
    vec![
        Step::new(
            "the factory returns a function that does not wait",
            vec![save("pkg/held_defs.py", HELD_DEFS_QUIET_FACTORY)],
        )
        .absent(FACTORY)
        .present(&[
            ("pkg/held_field.py", "EFF101 [heuristic]"),
            ("pkg/held_partial.py", "EFF101"),
        ]),
        Step::new(
            "the view is a plain class",
            vec![save("pkg/held_defs.py", HELD_DEFS_PLAIN_VIEW)],
        )
        .absent(FIELD)
        .present(&[
            ("pkg/held_factory.py", "EFF101"),
            ("pkg/held_partial.py", "EFF101"),
        ]),
        Step::new(
            "the partial holds a function that does not wait",
            vec![save("pkg/held_defs.py", HELD_DEFS_QUIET_PARTIAL)],
        )
        .absent(PARTIAL)
        .present(&[
            ("pkg/held_factory.py", "EFF101"),
            ("pkg/held_field.py", "EFF101 [heuristic]"),
        ]),
        Step::new(
            "every definition waits again",
            vec![save("pkg/held_defs.py", HELD_DEFS)],
        )
        .present(ALL),
        Step::new(
            "the partial object in the table holds a position",
            vec![save("pkg/held_defs.py", HELD_DEFS_POSITIONED_TABLE)],
        )
        .absent(TABLE)
        .present(ALL),
        Step::new(
            "the partial object in the table holds nothing again",
            vec![save("pkg/held_defs.py", HELD_DEFS)],
        )
        .present(TABLE),
    ]
}

const HAND_DEFS: &str =
    "import time\n\n\ndef blocking():\n    time.sleep(1)\n\n\ndef quiet():\n    pass\n";
const HAND_HELPERS: &str = "def call(fn):\n    return fn()\n";
const HAND_USE: &str = "from pkg.hand_defs import blocking\nfrom pkg.hand_helpers import call\n\n\nasync def use():\n    call(blocking)\n";

/// A helper of another module calls the function it is handed (`FunctionDecl::calls_params`),
/// so the caller passing a function that waits runs it there (`EFF101`); the helper stores the
/// function instead, then calls it in a nested function it calls; the caller passes a function
/// that does not wait, then the waiting one again.
fn steps_handed_callables() -> Vec<Step> {
    const USE: &[(&str, &str)] = &[("pkg/hand_use.py", "EFF101")];
    vec![
        Step::new(
            "a helper of another module calls the function it is handed",
            vec![
                Action::Create("pkg/hand_defs.py", HAND_DEFS.to_owned()),
                Action::Create("pkg/hand_helpers.py", HAND_HELPERS.to_owned()),
                Action::Create("pkg/hand_use.py", HAND_USE.to_owned()),
            ],
        )
        .present(USE),
        Step::new(
            "the helper keeps the function instead of calling it",
            vec![save(
                "pkg/hand_helpers.py",
                "KEPT = []\n\n\ndef call(fn):\n    KEPT.append(fn)\n",
            )],
        )
        .absent(USE),
        Step::new(
            "the helper calls the function in a nested function it calls",
            vec![save(
                "pkg/hand_helpers.py",
                "def call(fn):\n    def once():\n        fn()\n\n    once()\n",
            )],
        )
        .present(USE),
        Step::new(
            "the caller passes a function that does not wait",
            vec![save(
                "pkg/hand_use.py",
                &HAND_USE.replace("blocking", "quiet"),
            )],
        )
        .absent(USE),
        Step::new(
            "the caller passes the waiting function again",
            vec![save("pkg/hand_use.py", HAND_USE)],
        )
        .present(USE),
    ]
}

/// The items a loop over a function's result steps through are those every return of the
/// function gives (`FunctionDecl::returns`): a query in another module whose text fixes its
/// rows makes the loop's report heuristic, and rows returned through a helper of that module
/// are followed.
fn steps_returned_rows() -> Vec<Step> {
    const EXACT: &[(&str, &str)] = &[("pkg/rows_use.py", "EFF103")];
    const HEURISTIC: &[(&str, &str)] = &[("pkg/rows_use.py", "EFF103 [heuristic]")];
    vec![
        Step::new(
            "the row helper's query fixes how many rows it gives",
            vec![save("pkg/rows_db.py", ROWS_DB_LIMITED)],
        )
        .present(HEURISTIC)
        .absent(EXACT),
        Step::new(
            "it returns the rows of a helper of its own module",
            vec![save("pkg/rows_db.py", ROWS_DB_THROUGH)],
        )
        .present(EXACT)
        .absent(HEURISTIC),
        Step::new(
            "it returns its own query's rows again",
            vec![save("pkg/rows_db.py", ROWS_DB)],
        )
        .present(EXACT)
        .absent(HEURISTIC),
    ]
}

/// A global's and an attribute's type is what every store of it binds: a store of another class
/// in another function of the module, in a third module, in a subclass or a mixin of it in
/// another file, or through a parameter of the class makes the calls on it unresolved, which the
/// language server sees in every reader; so does a store of another class into a global that
/// the stores of another global read. A global annotated with the class of a connection is the
/// connection the store opens: in memory, its reader in another module does not wait.
fn steps_store_types() -> Vec<Step> {
    const READERS: &[(&str, &str)] = &[("pkg/st_client.py", "EFF101"), ("pkg/st_use.py", "EFF101")];
    const HOLDER: &[(&str, &str)] = &[("pkg/st_holder.py", "EFF101")];
    const ACTIVE: &[(&str, &str)] = &[("pkg/st_active.py", "EFF101")];
    const ATTR_ACTIVE: &[(&str, &str)] = &[("pkg/st_attr_active.py", "EFF101")];
    const STAR: &[(&str, &str)] = &[("pkg/st_star.py", "EFF101")];
    const LITE: &[(&str, &str)] = &[("pkg/st_lite_use.py", "EFF101")];
    vec![
        Step::new(
            "another function rebinds the global to an async client",
            vec![save("pkg/st_client.py", ST_CLIENT_REBOUND)],
        )
        .absent(READERS),
        Step::new(
            "the function stops rebinding it",
            vec![save("pkg/st_client.py", ST_CLIENT)],
        )
        .present(READERS),
        Step::new(
            "a third module stores an async client into the global",
            vec![save("pkg/st_rebind.py", ST_REBIND_STORES)],
        )
        .absent(READERS),
        Step::new(
            "the third module stores a client of the same class",
            vec![save("pkg/st_rebind.py", ST_REBIND_SAME)],
        )
        .present(READERS),
        Step::new(
            "the third module stores an async client again",
            vec![save("pkg/st_rebind.py", ST_REBIND_STORES)],
        )
        .absent(READERS),
        Step::new(
            "the third module stops storing into it",
            vec![save("pkg/st_rebind.py", ST_REBIND)],
        )
        .present(READERS),
        Step::new(
            "a subclass in another file stores an async client into the attribute",
            vec![save("pkg/st_child.py", ST_CHILD_SWAPPING)],
        )
        .absent(HOLDER),
        Step::new(
            "the subclass goes away",
            vec![save("pkg/st_child.py", ST_CHILD)],
        )
        .present(HOLDER),
        Step::new(
            "the subclass swaps the client again",
            vec![save("pkg/st_child.py", ST_CHILD_SWAPPING)],
        )
        .absent(HOLDER),
        // The hierarchy stays: only what the subclass stores changes.
        Step::new(
            "the subclass stores a client of the same class",
            vec![save("pkg/st_child.py", ST_CHILD_SAME)],
        )
        .present(HOLDER),
        Step::new(
            "the subclass stores an attribute heki cannot name",
            vec![save("pkg/st_child.py", ST_CHILD_ANY)],
        )
        .absent(HOLDER),
        Step::new(
            "the subclass stores a client of the same class again",
            vec![save("pkg/st_child.py", ST_CHILD_SAME)],
        )
        .present(HOLDER),
        Step::new(
            "the subclass takes a mixin from a third file",
            vec![save("pkg/st_child.py", ST_CHILD_MIXED)],
        )
        .present(HOLDER),
        Step::new(
            "the mixin stores an async client into the attribute",
            vec![save("pkg/st_mixin.py", ST_MIXIN_SWAPPING)],
        )
        .absent(HOLDER),
        Step::new(
            "the mixin stops storing",
            vec![save("pkg/st_mixin.py", ST_MIXIN)],
        )
        .present(HOLDER),
        Step::new(
            "a function stores an async client through a parameter of the class",
            vec![save("pkg/st_child.py", ST_CHILD_PARAM)],
        )
        .absent(HOLDER),
        Step::new(
            "the function goes away",
            vec![save("pkg/st_child.py", ST_CHILD)],
        )
        .present(HOLDER),
        Step::new(
            "another module stores an async client into the global the store reads",
            vec![save("pkg/st_active_stores.py", ST_ACTIVE_STORES_CLIENT)],
        )
        .absent(ACTIVE),
        Step::new(
            "it stores into another global",
            vec![save("pkg/st_active_stores.py", ST_ACTIVE_STORES_OTHER)],
        )
        .present(ACTIVE),
        Step::new(
            "it stores into the read global again",
            vec![save("pkg/st_active_stores.py", ST_ACTIVE_STORES_CLIENT)],
        )
        .absent(ACTIVE),
        Step::new(
            "it stops storing",
            vec![save("pkg/st_active_stores.py", ST_ACTIVE_STORES)],
        )
        .present(ACTIVE),
        Step::new(
            "another module stores an async client into the attribute the store reads",
            vec![save("pkg/st_attr_stores.py", ST_ATTR_STORES_CLIENT)],
        )
        .absent(ATTR_ACTIVE),
        Step::new(
            "it stores into another attribute",
            vec![save("pkg/st_attr_stores.py", ST_ATTR_STORES_OTHER)],
        )
        .present(ATTR_ACTIVE),
        Step::new(
            "it stores into the read attribute again",
            vec![save("pkg/st_attr_stores.py", ST_ATTR_STORES_CLIENT)],
        )
        .absent(ATTR_ACTIVE),
        Step::new(
            "it stops storing into the attribute",
            vec![save("pkg/st_attr_stores.py", ST_ATTR_STORES)],
        )
        .present(ATTR_ACTIVE),
        Step::new(
            "a class of another module stores an async client through an instance it keeps",
            vec![save("pkg/st_keeper.py", ST_KEEPER_ATTR)],
        )
        .absent(HOLDER),
        Step::new(
            "the module keeps the instance in a global instead",
            vec![save("pkg/st_keeper.py", ST_KEEPER_GLOBAL)],
        )
        .absent(HOLDER),
        Step::new(
            "the module stops keeping it",
            vec![save("pkg/st_keeper.py", ST_KEEPER)],
        )
        .present(HOLDER),
        Step::new(
            "the star-imported module appends the global's name to its `__all__`",
            vec![save("pkg/st_exporting.py", ST_EXPORTING_APPENDED)],
        )
        .absent(STAR),
        Step::new(
            "it stops appending",
            vec![save("pkg/st_exporting.py", ST_EXPORTING)],
        )
        .present(STAR),
        Step::new(
            "the annotated global's connection is opened in memory",
            vec![save("pkg/st_lite.py", ST_LITE_IN_MEMORY)],
        )
        .absent(LITE),
        Step::new(
            "it is opened on a file again",
            vec![save("pkg/st_lite.py", ST_LITE)],
        )
        .present(LITE),
    ]
}

/// `report-unused-suppressions`: a suppression of `EFF101` in a caller's file still matches the
/// report when a subclass in another file makes it heuristic, and turns unused, and back, as the
/// base class in a third file stops and starts blocking (`unused-suppression` follows the reports
/// of other files' changes, as the reports it judges do).
fn steps_unused_suppressions() -> Vec<Step> {
    let reporting = CONFIG.replacen(
        "[tool.heki.lint]\n",
        "[tool.heki.lint]\nreport-unused-suppressions = true\n",
        1,
    );
    let suppressed = EXACT_USE.replace(
        "b.fetch(\"u\")\n",
        "b.fetch(\"u\")  # heki: ignore[EFF101]\n",
    );
    vec![
        Step::new(
            "a suppression of `EFF101` on its line suppresses its report",
            vec![
                Action::Configure(reporting),
                save("pkg/exact_use.py", &suppressed),
            ],
        )
        .absent(&[
            ("pkg/exact_use.py", "EFF101"),
            ("pkg/exact_use.py", "unused-suppression"),
        ]),
        Step::new(
            "a subclass in another file overrides the method without blocking: the report is \
             heuristic, under the code the suppression names",
            vec![save("pkg/exact_sub.py", EXACT_SUB_OVERRIDING)],
        )
        .absent(&[
            ("pkg/exact_use.py", "EFF101"),
            ("pkg/exact_use.py", "EFF101 [heuristic]"),
            ("pkg/exact_use.py", "unused-suppression"),
        ]),
        Step::new(
            "the base's method stops blocking: the suppression is unused",
            vec![save("pkg/exact_base.py", EXACT_BASE_QUIET)],
        )
        .present(&[("pkg/exact_use.py", "unused-suppression")]),
        Step::new(
            "the base blocks again: the suppression is used again",
            vec![save("pkg/exact_base.py", EXACT_BASE)],
        )
        .absent(&[
            ("pkg/exact_use.py", "EFF101 [heuristic]"),
            ("pkg/exact_use.py", "unused-suppression"),
        ]),
    ]
}

/// A catalog argument read from an imported constant (`ModuleDecls::literal_constants`): a sleep
/// of no time is no wait, one of a second is, and so is one of a constant another module rebinds.
/// A constant computed from another holds its value too (`heki_ir::ModuleConstants`).
fn steps_imported_constants() -> Vec<Step> {
    vec![
        Step::new(
            "the imported constant becomes a wait",
            vec![save("pkg/lc_settings.py", LC_SETTINGS_WAIT)],
        )
        .present(&[("pkg/lc_use.py", "EFF101")]),
        Step::new(
            "it becomes no time computed from another constant",
            vec![save("pkg/lc_settings.py", LC_SETTINGS_COMPUTED)],
        )
        .absent(&[("pkg/lc_use.py", "EFF101")]),
        Step::new(
            "it becomes a wait again",
            vec![save("pkg/lc_settings.py", LC_SETTINGS_WAIT)],
        )
        .present(&[("pkg/lc_use.py", "EFF101")]),
        Step::new(
            "it becomes no time again",
            vec![save("pkg/lc_settings.py", LC_SETTINGS)],
        )
        .absent(&[("pkg/lc_use.py", "EFF101")]),
        Step::new(
            "a third module starts rebinding it",
            vec![save("pkg/lc_rebind.py", LC_REBIND_STORES)],
        )
        .present(&[("pkg/lc_use.py", "EFF101")]),
        Step::new(
            "the third module stops rebinding it",
            vec![save("pkg/lc_rebind.py", LC_REBIND)],
        )
        .absent(&[("pkg/lc_use.py", "EFF101")]),
    ]
}

/// Looking up a method raises nothing only when no class the value may be runs code on the read
/// (`ProjectDecls::method_read_runs_no_code`): a subclass in another module shadowing the method
/// with a getter, or defining `__getattribute__`, puts an exception edge before the wait.
fn steps_method_lookups() -> Vec<Step> {
    vec![
        Step::new(
            "a subclass in another module shadows the method with a getter",
            vec![Action::Create(
                "pkg/ml_shadow.py",
                ML_SHADOW_GETTER.to_owned(),
            )],
        )
        .present(&[("pkg/ml_use.py", "EFF101 [heuristic]")])
        .absent(&[("pkg/ml_use.py", "EFF101")]),
        Step::new(
            "the subclass overrides it with a method",
            vec![save("pkg/ml_shadow.py", ML_SHADOW_METHOD)],
        )
        .present(&[("pkg/ml_use.py", "EFF101")])
        .absent(&[("pkg/ml_use.py", "EFF101 [heuristic]")]),
        Step::new(
            "the subclass defines `__getattribute__`",
            vec![save("pkg/ml_shadow.py", ML_SHADOW_HOOKED)],
        )
        .present(&[("pkg/ml_use.py", "EFF101 [heuristic]")])
        .absent(&[("pkg/ml_use.py", "EFF101")]),
        Step::new(
            "the subclass is deleted",
            vec![Action::Delete("pkg/ml_shadow.py")],
        )
        .present(&[("pkg/ml_use.py", "EFF101")])
        .absent(&[("pkg/ml_use.py", "EFF101 [heuristic]")]),
    ]
}

const XN_BASE: &str = "class Root:\n    pass\n";
const XN_BASE_NEW: &str =
    "class Root:\n    def __new__(cls):\n        return object.__new__(cls)\n";
const XN_CORE: &str = "from pkg.xn_base import Root\n\n\nclass Notes(Root):\n    def preview(self) -> int:\n        return 1\n\n\ndef prepare(notes: Notes) -> int:\n    return notes.preview()\n";
const XN_EXACT: &str = "from pkg.xn_core import Notes, prepare\n\n\nasync def concrete() -> int:\n    return prepare(Notes())\n";
const XN_OPEN: &str = "from pkg.xn_core import Notes, prepare\n\n\nasync def from_argument(notes: Notes) -> int:\n    return prepare(notes)\n";
const XN_GATE: &str = "import time\n\nfrom pkg.xn_core import Notes, prepare\n\n\nclass Gate(Notes):\n    def preview(self) -> int:\n        time.sleep(1)\n        return 1\n\n\nasync def gated() -> int:\n    return prepare(Gate())\n";
const XN_HUB: &str = "from pkg.xn_core import Notes, prepare\n";
const XN_HUB_REBOUND: &str = "from pkg.xn_core import Notes, prepare\nfrom pkg.xn_gate import Gate\n\n\ndef use_gate() -> None:\n    global Notes\n    Notes = Gate\n";
const XN_VIA: &str = "from pkg import xn_hub\n\n\nasync def via_hub() -> int:\n    return xn_hub.prepare(xn_hub.Notes())\n";
const XN_PATCH: &str = "from pkg.xn_base import Root\n\n\ndef make(cls: type) -> object:\n    return object.__new__(cls)\n\n\nRoot.__new__ = staticmethod(make)\n";

/// A subclass in another module overriding a method a helper calls on its parameter: a caller
/// passing an instance of exactly the base class runs only the base's implementation, a caller
/// passing its own parameter or an instance of the subclass may run the subclass's
/// (`EffectSummary::provided_effects`), as the subclass is added and removed. A `__new__` added
/// to a base in a third module, in its body or by a store on the class object, makes the
/// construction no exact instance, and so does a module the class is read through binding the
/// name again.
fn steps_exact_instances() -> Vec<Step> {
    vec![
        Step::new(
            "a helper calls a method on its parameter, called with an exact instance and with a parameter",
            vec![
                Action::Create("pkg/xn_base.py", XN_BASE.to_owned()),
                Action::Create("pkg/xn_core.py", XN_CORE.to_owned()),
                Action::Create("pkg/xn_exact.py", XN_EXACT.to_owned()),
                Action::Create("pkg/xn_open.py", XN_OPEN.to_owned()),
            ],
        )
        .absent(&[
            ("pkg/xn_exact.py", "EFF101 [heuristic]"),
            ("pkg/xn_open.py", "EFF101 [heuristic]"),
        ]),
        Step::new(
            "a subclass in another module overrides the method with a blocking one",
            vec![Action::Create("pkg/xn_gate.py", XN_GATE.to_owned())],
        )
        .present(&[
            ("pkg/xn_open.py", "EFF101 [heuristic]"),
            ("pkg/xn_gate.py", "EFF101 [heuristic]"),
        ])
        .absent(&[
            ("pkg/xn_exact.py", "EFF101 [heuristic]"),
            ("pkg/xn_exact.py", "EFF101"),
        ]),
        Step::new(
            "the base of the class defines `__new__`",
            vec![save("pkg/xn_base.py", XN_BASE_NEW)],
        )
        .present(&[
            ("pkg/xn_exact.py", "EFF101 [heuristic]"),
            ("pkg/xn_open.py", "EFF101 [heuristic]"),
        ]),
        Step::new(
            "the base defines no `__new__` again",
            vec![save("pkg/xn_base.py", XN_BASE)],
        )
        .present(&[("pkg/xn_open.py", "EFF101 [heuristic]")])
        .absent(&[("pkg/xn_exact.py", "EFF101 [heuristic]")]),
        Step::new(
            "another module stores a `__new__` on the base",
            vec![Action::Create("pkg/xn_patch.py", XN_PATCH.to_owned())],
        )
        .present(&[("pkg/xn_exact.py", "EFF101 [heuristic]")]),
        Step::new(
            "the module storing the `__new__` is deleted",
            vec![Action::Delete("pkg/xn_patch.py")],
        )
        .absent(&[("pkg/xn_exact.py", "EFF101 [heuristic]")]),
        Step::new(
            "the class is constructed through a module re-exporting it",
            vec![
                Action::Create("pkg/xn_hub.py", XN_HUB.to_owned()),
                Action::Create("pkg/xn_via.py", XN_VIA.to_owned()),
            ],
        )
        .absent(&[("pkg/xn_via.py", "EFF101 [heuristic]")]),
        Step::new(
            "the re-exporting module binds the name to the subclass in a function",
            vec![save("pkg/xn_hub.py", XN_HUB_REBOUND)],
        )
        .present(&[("pkg/xn_via.py", "EFF101 [heuristic]")]),
        Step::new(
            "the re-exporting module binds the name no more",
            vec![save("pkg/xn_hub.py", XN_HUB)],
        )
        .absent(&[("pkg/xn_via.py", "EFF101 [heuristic]")]),
        Step::new(
            "the subclass is deleted",
            vec![Action::Delete("pkg/xn_gate.py")],
        )
        .absent(&[
            ("pkg/xn_exact.py", "EFF101 [heuristic]"),
            ("pkg/xn_open.py", "EFF101 [heuristic]"),
            ("pkg/xn_via.py", "EFF101 [heuristic]"),
        ]),
    ]
}

const PV: &str = "class Pv:\n    def __run(self) -> int:\n        return 1\n\n    def go(self) -> int:\n        return self.__run()\n";
const PV_USE: &str = "from pkg.pv import Pv\n\n\ndef use(p: Pv) -> int:\n    return p._Pv__run()\n";
const PV_SUB: &str = "from pkg.pv import Pv\n\n\nclass Sub(Pv):\n    def __run(self) -> int:\n        return 2\n\n    def own(self) -> int:\n        return self.__run()\n\n    def peek(self) -> int:\n        return self._Pv__run()\n";
const PV_DOC: &str = "from functools import cached_property\n\n\nclass PvDoc:\n    def __init__(self) -> None:\n        self.size = 0\n\n    @cached_property\n    def total(self) -> int:\n        return self.size + 1\n\n    def __grow(self) -> None:\n        self.size = 1\n";
const PV_SHOW: &str =
    "from pkg.pv_doc import PvDoc\n\n\ndef show(doc: PvDoc) -> int:\n    return doc.total\n";

/// A private method (`def __run`, the member `_Pv__run`) called by its mangled name from other
/// modules: the call follows the class's declarations when the method is renamed and back, and a
/// subclass's own `__run` is another method, which `self.__run` in the subclass calls. A private
/// method (`_PvDoc__grow`) that leaves a cached property stale makes its reads in another module
/// stale reads until it invalidates the property.
fn steps_private_names() -> Vec<Step> {
    let contract = format!(
        "{CONFIG}\n[[tool.heki.call-owners]]\nname = \"private-run\"\ntargets = [\"pkg.pv.Pv.__run\"]\nallowed-callers = [\"pkg.pv:Pv.*\"]\n"
    );
    vec![
        Step::new(
            "a module calls a private method of a class in another module by its mangled name",
            vec![
                Action::Configure(contract),
                Action::Create("pkg/pv.py", PV.to_owned()),
                Action::Create("pkg/pv_use.py", PV_USE.to_owned()),
            ],
        )
        .present(&[("pkg/pv_use.py", "MUT204")])
        .absent(&[("pkg/pv.py", "MUT204")]),
        Step::new(
            "the private method is renamed",
            vec![save("pkg/pv.py", &PV.replace("__run", "__walk"))],
        )
        .absent(&[("pkg/pv_use.py", "MUT204")]),
        Step::new(
            "it is named back, and a subclass in another module defines its own `__run`",
            vec![
                save("pkg/pv.py", PV),
                Action::Create("pkg/pv_sub.py", PV_SUB.to_owned()),
            ],
        )
        .present(&[("pkg/pv_use.py", "MUT204"), ("pkg/pv_sub.py", "MUT204")]),
        Step::new(
            "the subclass calls only its own `__run`",
            vec![save(
                "pkg/pv_sub.py",
                &PV_SUB.replace("self._Pv__run()", "self.__run()"),
            )],
        )
        .absent(&[("pkg/pv_sub.py", "MUT204")]),
        Step::new(
            "a private method leaves a cached property stale that another module reads",
            vec![
                Action::Create("pkg/pv_doc.py", PV_DOC.to_owned()),
                Action::Create("pkg/pv_show.py", PV_SHOW.to_owned()),
            ],
        )
        .present(&[("pkg/pv_show.py", "MUT111"), ("pkg/pv_doc.py", "MUT101")]),
        Step::new(
            "the private method invalidates the cached property",
            vec![save(
                "pkg/pv_doc.py",
                &PV_DOC.replace(
                    "self.size = 1\n",
                    "self.size = 1\n        self.__dict__.pop(\"total\", None)\n",
                ),
            )],
        )
        .absent(&[("pkg/pv_show.py", "MUT111"), ("pkg/pv_doc.py", "MUT101")]),
        Step::new(
            "the files and the contract are removed",
            vec![
                Action::Delete("pkg/pv_show.py"),
                Action::Delete("pkg/pv_doc.py"),
                Action::Delete("pkg/pv_sub.py"),
                Action::Delete("pkg/pv_use.py"),
                Action::Delete("pkg/pv.py"),
                Action::Configure(CONFIG.to_owned()),
            ],
        ),
    ]
}

/// A helper over a dispatch keeps every implementation's wait (`MustExec::alternatives`, compared
/// by Early Cutoff): the report through it names and relates each, and follows an edit of the
/// wait that is not on the helper's shortest chain.
fn steps_helper_witnesses() -> Vec<Step> {
    vec![
        Step::new(
            "the implementation off the shortest chain waits in another call",
            vec![save("pkg/wit_local.py", WIT_LOCAL_PUT)],
        )
        .present(&[("pkg/wit_serve.py", "EFF101")])
        .absent(&[("pkg/wit_serve.py", "EFF101 [heuristic]")]),
        Step::new(
            "it stops waiting",
            vec![save("pkg/wit_local.py", WIT_LOCAL_QUIET)],
        )
        .present(&[("pkg/wit_serve.py", "EFF101 [heuristic]")])
        .absent(&[("pkg/wit_serve.py", "EFF101")]),
        Step::new("it waits again", vec![save("pkg/wit_local.py", WIT_LOCAL)])
            .present(&[("pkg/wit_serve.py", "EFF101")])
            .absent(&[("pkg/wit_serve.py", "EFF101 [heuristic]")]),
        Step::new(
            "the helper's parameter names only the implementation on its shortest chain",
            vec![save("pkg/wit_fetch.py", WIT_FETCH_REMOTE)],
        )
        .present(&[("pkg/wit_serve.py", "EFF101")])
        .absent(&[("pkg/wit_serve.py", "EFF101 [heuristic]")]),
        Step::new(
            "the helper's parameter names the base class again",
            vec![save("pkg/wit_fetch.py", WIT_FETCH)],
        )
        .present(&[("pkg/wit_serve.py", "EFF101")])
        .absent(&[("pkg/wit_serve.py", "EFF101 [heuristic]")]),
    ]
}

/// The chain a report shows when another module changes what it is chosen by: the class an
/// `isinstance` dispatch names turns into a protocol (an exact instance of another class no
/// longer rules out its branch) and back, and the normal path of a helper stops reaching the
/// effect (the handler's call is the witness) and reaches it again.
fn steps_witness_ways() -> Vec<Step> {
    vec![
        Step::new(
            "the dispatch class becomes a protocol",
            vec![save("pkg/wsel_kinds.py", WSEL_KINDS_PROTOCOL)],
        )
        .present(&[("pkg/wsel_serve.py", "EFF101 [heuristic]")]),
        Step::new(
            "it is a plain class again",
            vec![save("pkg/wsel_kinds.py", WSEL_KINDS)],
        )
        .present(&[("pkg/wsel_serve.py", "EFF101 [heuristic]")]),
        Step::new(
            "the helper waits only in its handler",
            vec![save("pkg/wsel_retry.py", WSEL_RETRY_IN_HANDLER)],
        )
        .present(&[("pkg/wsel_serve.py", "EFF101 [heuristic]")]),
        Step::new(
            "it waits on the normal path again",
            vec![save("pkg/wsel_retry.py", WSEL_RETRY)],
        )
        .present(&[("pkg/wsel_serve.py", "EFF101 [heuristic]")]),
    ]
}

fn save(rel: &'static str, text: &str) -> Action {
    Action::Save(rel, text.to_owned())
}

/// `references = "calls"` on the `ledger` call contract and the `pure` boundary: a private
/// helper's reference handing a guarded class on is reported at its caller in another module,
/// and below the boundary, while no summary changes; it turns into a call and back, and the key
/// leaves the configuration again (the summaries then carry no reference sites).
fn steps_references() -> Vec<Step> {
    let counting = CONFIG
        .replace(
            "targets = [\"Ledger.post\", \"Receipt\"]\nallowed-callers = [\"Store.*\"]\n",
            "targets = [\"Ledger.post\", \"Receipt\"]\nallowed-callers = [\"Store.*\"]\nreferences = \"calls\"\n",
        )
        .replace(
            "call-targets = [\"Ledger.post\", \"Receipt\"]\n",
            "call-targets = [\"Ledger.post\", \"Receipt\"]\nreferences = \"calls\"\n",
        );
    let record = |body: &str| ACCT_OPS.replace("    return None\n", body);
    let helper = |body: &str| PURE_OPS.replace("    return None\n", body);
    vec![
        Step::new(
            "the call contracts count references as calls, and a private helper hands a guarded dataclass on",
            vec![
                Action::Configure(counting),
                save("pkg/acct_ops.py", &record("    return Receipt\n")),
            ],
        )
        .present(&[("pkg/accounts.py", "MUT204")]),
        Step::new(
            "the helper constructs it instead",
            vec![save("pkg/acct_ops.py", &record("    return Receipt(1)\n"))],
        )
        .present(&[("pkg/accounts.py", "MUT204")]),
        Step::new(
            "the helper hands it on again, from a display",
            vec![save("pkg/acct_ops.py", &record("    return [Receipt]\n"))],
        )
        .present(&[("pkg/accounts.py", "MUT204")]),
        Step::new(
            "a helper below the boundary hands the dataclass on",
            vec![save("pkg/pure_ops.py", &helper("    return Receipt\n"))],
        )
        .present(&[("pkg/pure.py", "EFF201"), ("pkg/pure_ops.py", "MUT204")]),
        Step::new(
            "the helper below the boundary only tests an instance of it",
            vec![save(
                "pkg/pure_ops.py",
                &helper("    return isinstance(v, Receipt)\n"),
            )],
        )
        .absent(&[("pkg/pure.py", "EFF201"), ("pkg/pure_ops.py", "MUT204")]),
        Step::new(
            "the helper hands it on again, and the key leaves the configuration",
            vec![
                save("pkg/pure_ops.py", &helper("    return Receipt\n")),
                Action::Configure(CONFIG.to_owned()),
            ],
        )
        .absent(&[
            ("pkg/accounts.py", "MUT204"),
            ("pkg/pure.py", "EFF201"),
            ("pkg/pure_ops.py", "MUT204"),
        ]),
        Step::new(
            "the helpers are restored",
            vec![
                save("pkg/acct_ops.py", ACCT_OPS),
                save("pkg/pure_ops.py", PURE_OPS),
            ],
        )
        .absent(&[("pkg/accounts.py", "MUT204")]),
    ]
}

const RECEIPT_HOLDER: &str =
    "from pkg.ledger import Receipt\n\n\nclass Holder:\n    kept = Receipt(1)\n";

/// A class body at module level constructs the guarded dataclass of another module: the module
/// body makes the call, which follows the dataclass's module when it renames the class and back.
fn steps_class_bodies() -> Vec<Step> {
    vec![
        Step::new(
            "a class attribute's value constructs the guarded dataclass",
            vec![Action::Create(
                "pkg/receipt_holder.py",
                RECEIPT_HOLDER.to_owned(),
            )],
        )
        .present(&[("pkg/receipt_holder.py", "MUT204")]),
        Step::new(
            "the dataclass's module renames it",
            vec![save(
                "pkg/ledger.py",
                &LEDGER.replace("class Receipt", "class Slip"),
            )],
        )
        .absent(&[("pkg/receipt_holder.py", "MUT204")]),
        Step::new("the name comes back", vec![save("pkg/ledger.py", LEDGER)])
            .present(&[("pkg/receipt_holder.py", "MUT204")]),
        Step::new(
            "the class body is removed",
            vec![Action::Delete("pkg/receipt_holder.py")],
        ),
    ]
}

/// `references = "calls-and-bases"` and `attribution = "lexical"` on the `ledger` call contract:
/// a class of another module derives from the guarded dataclass, whose renaming and restoring
/// in its own module turns the base off and on; then the private helper's construction is
/// judged at the helper instead of its caller, until the keys leave the configuration.
fn steps_bases_and_attribution() -> Vec<Step> {
    let contract = "targets = [\"Ledger.post\", \"Receipt\"]\nallowed-callers = [\"Store.*\"]\n";
    let bases = CONFIG.replace(
        contract,
        &format!("{contract}references = \"calls-and-bases\"\n"),
    );
    let lexical = CONFIG.replace(
        contract,
        &format!("{contract}references = \"calls-and-bases\"\nattribution = \"lexical\"\n"),
    );
    let derived = format!("{ACCT_OPS}\n\nclass Late(Receipt):\n    pass\n");
    let constructs = ACCT_OPS.replace("    return None\n", "    return Receipt(1)\n");
    vec![
        Step::new(
            "the ledger contract counts class bases, and another module derives from the guarded dataclass",
            vec![Action::Configure(bases), save("pkg/acct_ops.py", &derived)],
        )
        .present(&[("pkg/acct_ops.py", "MUT204")]),
        Step::new(
            "the guarded dataclass is renamed",
            vec![save("pkg/ledger.py", &LEDGER.replace("class Receipt", "class Slip"))],
        )
        .absent(&[("pkg/acct_ops.py", "MUT204")]),
        Step::new(
            "the guarded dataclass is restored",
            vec![save("pkg/ledger.py", LEDGER)],
        )
        .present(&[("pkg/acct_ops.py", "MUT204")]),
        Step::new(
            "the contract judges lexically, and the private helper constructs the dataclass",
            vec![
                Action::Configure(lexical),
                save("pkg/acct_ops.py", &constructs),
            ],
        )
        .present(&[("pkg/acct_ops.py", "MUT204")])
        .absent(&[("pkg/accounts.py", "MUT204")]),
        Step::new(
            "the keys leave the configuration",
            vec![Action::Configure(CONFIG.to_owned())],
        )
        .present(&[("pkg/accounts.py", "MUT204")])
        .absent(&[("pkg/acct_ops.py", "MUT204")]),
        Step::new(
            "the helper is restored",
            vec![save("pkg/acct_ops.py", ACCT_OPS)],
        )
        .absent(&[("pkg/accounts.py", "MUT204")]),
    ]
}

/// A `call-owners` target and a `call-targets` entry of the `pure` boundary with an argument
/// list: a call passing the constant through a `Final` name of another module is reported, at
/// the caller and at the scope function above a helper making it, until that module binds the
/// name to another string (the callers are analyzed again for the declaration they read), and
/// the caller's report goes when it passes another literal.
fn steps_argument_lists() -> Vec<Step> {
    let config = format!(
        "{CONFIG}\n[[tool.heki.call-owners]]\nname = \"posting\"\ntargets = ['Ledger.post(amount=\"big\")']\n"
    )
    .replace(
        "call-targets = [\"Ledger.post\", \"Receipt\"]\n",
        "call-targets = ['Ledger.post(amount=\"big\")', \"Receipt\"]\n",
    );
    let amounts = |value: &str| format!("from typing import Final\n\nBIG: Final = \"{value}\"\n");
    // A method of a `Store`, which the `ledger` contract allows to post.
    let pay = |argument: &str| {
        format!(
            "from pkg.amounts import BIG\nfrom pkg.ledger import Ledger\n\n\nclass Store:\n    def pay(self, l: Ledger):\n        return l.post({argument})\n"
        )
    };
    let helper = format!(
        "from pkg.amounts import BIG\n{}",
        PURE_OPS.replace("    return None\n", "    return l.post(BIG)\n")
    );
    vec![
        Step::new(
            "calls pass the constant of an argument list through a name of another module",
            vec![
                Action::Configure(config),
                Action::Create("pkg/amounts.py", amounts("big")),
                Action::Create("pkg/posting.py", pay("BIG")),
                save("pkg/pure_ops.py", &helper),
            ],
        )
        .present(&[("pkg/posting.py", "MUT204"), ("pkg/pure.py", "EFF201")]),
        Step::new(
            "the other module binds the name to another string",
            vec![save("pkg/amounts.py", &amounts("small"))],
        )
        .absent(&[("pkg/posting.py", "MUT204"), ("pkg/pure.py", "EFF201")]),
        Step::new(
            "it binds the constant again",
            vec![save("pkg/amounts.py", &amounts("big"))],
        )
        .present(&[("pkg/posting.py", "MUT204"), ("pkg/pure.py", "EFF201")]),
        Step::new(
            "the caller passes another literal",
            vec![save("pkg/posting.py", &pay("\"small\""))],
        )
        .present(&[("pkg/pure.py", "EFF201")])
        .absent(&[("pkg/posting.py", "MUT204")]),
        Step::new(
            "the files and the contracts are restored",
            vec![
                save("pkg/pure_ops.py", PURE_OPS),
                Action::Delete("pkg/posting.py"),
                Action::Delete("pkg/amounts.py"),
                Action::Configure(CONFIG.to_owned()),
            ],
        )
        .absent(&[("pkg/pure.py", "EFF201")]),
    ]
}

/// A call target naming every callee by its argument list: a call of a parameter passing a
/// `Final` string of another module is reported while the string matches the target's glob,
/// until that module binds the name to another string, and again when it binds it back.
fn steps_callee_globs() -> Vec<Step> {
    let config = format!(
        "{CONFIG}\n[[tool.heki.call-owners]]\nname = \"backend-module\"\ntargets = ['*(module=\"pkg.backend.*\")']\nforbidden-callers = [\"pkg.glob_use:*\"]\n"
    );
    let names = |value: &str| format!("from typing import Final\n\nBACKEND: Final = \"{value}\"\n");
    let using = "from pkg.backend_names import BACKEND\n\n\ndef build(factory):\n    return factory(module=BACKEND)\n";
    vec![
        Step::new(
            "a call of a parameter passes a string the glob matches",
            vec![
                Action::Configure(config),
                Action::Create("pkg/backend_names.py", names("pkg.backend.dataclass")),
                Action::Create("pkg/glob_use.py", using.to_owned()),
            ],
        )
        .present(&[("pkg/glob_use.py", "MUT204")]),
        Step::new(
            "the other module binds the name to another string",
            vec![save("pkg/backend_names.py", &names("pkg.frontend"))],
        )
        .absent(&[("pkg/glob_use.py", "MUT204")]),
        Step::new(
            "it binds a string the glob matches again",
            vec![save("pkg/backend_names.py", &names("pkg.backend.msgspec"))],
        )
        .present(&[("pkg/glob_use.py", "MUT204")]),
        Step::new(
            "the files and the contracts are restored",
            vec![
                Action::Delete("pkg/glob_use.py"),
                Action::Delete("pkg/backend_names.py"),
                Action::Configure(CONFIG.to_owned()),
            ],
        )
        .absent(&[("pkg/glob_use.py", "MUT204")]),
    ]
}

/// A call target naming every callee, below a boundary scope and a private helper's caller in
/// another module: editing only the string a helper passes to a parameter's call changes no
/// summary, yet the scope's `EFF201` and the caller's `MUT204` follow it.
fn steps_unbound_helpers() -> Vec<Step> {
    let config = format!(
        "{CONFIG}\n[[tool.heki.effect-boundaries]]\nname = \"no-backend\"\nscopes = [\"pkg.ub_scope.run\"]\nforbid = [\"calls\"]\ncall-targets = ['*(module=\"pkg.backend.*\")']\n\n[[tool.heki.call-owners]]\nname = \"backend-calls\"\ntargets = ['*(module=\"pkg.backend.*\")']\nforbidden-callers = [\"pkg.ub_scope:*\"]\n"
    );
    let helper = |value: &str| {
        format!(
            "def _make(factory):\n    return factory(module=\"{value}\")\n\n\ndef build(factory):\n    return factory(module=\"{value}\")\n"
        )
    };
    let scope = "from pkg.ub_helper import _make, build\n\n\ndef run(factory):\n    return build(factory)\n\n\ndef make(factory):\n    return _make(factory)\n";
    const REPORTS: &[(&str, &str)] =
        &[("pkg/ub_scope.py", "EFF201"), ("pkg/ub_scope.py", "MUT204")];
    vec![
        Step::new(
            "helpers of another module pass a string the glob matches",
            vec![
                Action::Configure(config),
                Action::Create("pkg/ub_helper.py", helper("pkg.backend.dataclass")),
                Action::Create("pkg/ub_scope.py", scope.to_owned()),
            ],
        )
        .present(REPORTS),
        Step::new(
            "the helpers pass another string",
            vec![save("pkg/ub_helper.py", &helper("pkg.frontend"))],
        )
        .absent(REPORTS),
        Step::new(
            "they pass a string the glob matches again",
            vec![save("pkg/ub_helper.py", &helper("pkg.backend.msgspec"))],
        )
        .present(REPORTS),
        Step::new(
            "the files and the contracts are restored",
            vec![
                Action::Delete("pkg/ub_scope.py"),
                Action::Delete("pkg/ub_helper.py"),
                Action::Configure(CONFIG.to_owned()),
            ],
        )
        .absent(REPORTS),
    ]
}

/// A call target with a module part naming a name the package binds to a class of another
/// module: a read of it through the package object is reported while the package imports the
/// class, not once the package stops binding it, and again when it binds it back.
fn steps_module_members() -> Vec<Step> {
    let config = format!(
        "{CONFIG}\n[[tool.heki.call-owners]]\nname = \"root-kind\"\ntargets = [\"pkg.rexp:Kind\"]\nforbidden-callers = [\"pkg.rexp_use:*\"]\nreferences = \"reads\"\n"
    );
    let reexport = "from pkg.rexp.enums import Kind as Kind\n";
    let using = "import pkg.rexp\n\n\ndef first():\n    return pkg.rexp.Kind.A\n";
    vec![
        Step::new(
            "a function reads the package's name through the package object",
            vec![
                Action::Configure(config),
                Action::Create("pkg/rexp/__init__.py", reexport.to_owned()),
                Action::Create(
                    "pkg/rexp/enums.py",
                    "import enum\n\n\nclass Kind(enum.Enum):\n    A = 1\n".to_owned(),
                ),
                Action::Create("pkg/rexp_use.py", using.to_owned()),
            ],
        )
        .present(&[("pkg/rexp_use.py", "MUT204")]),
        Step::new(
            "the package stops binding the name",
            vec![save("pkg/rexp/__init__.py", "")],
        )
        .absent(&[("pkg/rexp_use.py", "MUT204")]),
        Step::new(
            "it imports the class again",
            vec![save("pkg/rexp/__init__.py", reexport)],
        )
        .present(&[("pkg/rexp_use.py", "MUT204")]),
        Step::new(
            "the files and the contracts are restored",
            vec![
                Action::Delete("pkg/rexp_use.py"),
                Action::Delete("pkg/rexp/enums.py"),
                Action::Delete("pkg/rexp/__init__.py"),
                Action::Configure(CONFIG.to_owned()),
            ],
        )
        .absent(&[("pkg/rexp_use.py", "MUT204")]),
    ]
}

/// Step 4: `MUT108` (opt-in, enabled in the project's configuration). The frozen-ness of a class
/// comes from a decorator in another module, and the in-place change from a helper in a third.
/// The helper then moves the change of the field to a function of its own and back while its
/// summary stays the same: the call chain of the report ends at the first function whose body
/// changes the field (`heki_rules::ChainFacts`, as in [`steps_mut110`], for a slot of the
/// argument). Last, the construction of another frozen class in another module switches the
/// object it stores in a field between a `MappingProxyType`, which rejects the change, and a
/// dict (`ModuleDecls::field_values`).
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
        Step::new(
            "the construction in another module fills the frozen field with a dict",
            vec![Action::Save(
                "pkg/held/make.py",
                HELD_MAKE.replace("Held(MappingProxyType({}))", "Held({})"),
            )],
        )
        .present(&[("pkg/held/write.py", "MUT108")]),
        Step::new(
            "the construction fills it with a read-only view again",
            vec![save("pkg/held/make.py", HELD_MAKE)],
        )
        .absent(&[("pkg/held/write.py", "MUT108")]),
        Step::new(
            "a construction through the module attribute fills it with a dict",
            vec![Action::Save(
                "pkg/held/make.py",
                "from types import MappingProxyType\n\nfrom pkg.held import model\n\n\ndef make() -> model.Held:\n    return model.Held(MappingProxyType({}))\n\n\ndef other() -> model.Held:\n    return model.Held({})\n".to_owned(),
            )],
        )
        .present(&[("pkg/held/write.py", "MUT108")]),
        Step::new(
            "the qualified constructions fill it with read-only views again",
            vec![save("pkg/held/make.py", HELD_MAKE)],
        )
        .absent(&[("pkg/held/write.py", "MUT108")]),
    ]
}

/// `MUT120` (opt-in, enabled in the project's configuration): a helper in another module starts
/// rebinding a property of a read-only view the caller hands it, then moves the write to a
/// function of its own and back while its summary stays the same (the chain ends at the first
/// function whose body rebinds the property, `heki_rules::ChainFacts`), and the view in a third
/// module gains a setter, which makes it no read-only view.
fn steps_mut120() -> Vec<Step> {
    let rebinding = |flip: &str| {
        format!(
            "{flip}\n\ndef _set(field):\n    field.required = True\n\n\ndef tag(field):\n    field.extras[\"k\"] = 1\n"
        )
    };
    let setter = format!(
        "{RO_VIEW}\n    @required.setter\n    def required(self, value: bool) -> None: ...\n"
    );
    vec![
        Step::new(
            "a helper in another module starts rebinding a property of the view",
            vec![Action::Save(
                "pkg/roview/helpers.py",
                rebinding("def flip(field):\n    field.required = True\n"),
            )],
        )
        .present(&[("pkg/roview/decide.py", "MUT120")]),
        Step::new(
            "the helper rebinds the property only through a function of its own: a second hop",
            vec![Action::Save(
                "pkg/roview/helpers.py",
                rebinding("def flip(field):\n    _set(field)\n"),
            )],
        )
        .present(&[("pkg/roview/decide.py", "MUT120")]),
        Step::new(
            "the helper rebinds the property itself again",
            vec![Action::Save(
                "pkg/roview/helpers.py",
                rebinding("def flip(field):\n    field.required = True\n"),
            )],
        )
        .present(&[("pkg/roview/decide.py", "MUT120")]),
        Step::new(
            "the view gains a setter: it is no read-only view",
            vec![Action::Save("pkg/roview/model.py", setter)],
        )
        .absent(&[("pkg/roview/decide.py", "MUT120")]),
        Step::new(
            "the setter is gone again",
            vec![save("pkg/roview/model.py", RO_VIEW)],
        )
        .present(&[("pkg/roview/decide.py", "MUT120")]),
        Step::new(
            "the helper only reads the view again",
            vec![save("pkg/roview/helpers.py", RO_VIEW_HELPERS)],
        )
        .present(&[("pkg/roview/decide.py", "MUT120")]),
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
        Step::new(
            "a module calls a method of `type(obj)` and reads its object in a lambda",
            vec![Action::Create("pkg/seal_type.py", SEAL_TYPE.to_owned())],
        )
        .present(&[
            ("pkg/seal_type.py", "MUT204"),
            ("pkg/seal_type.py", "MUT203"),
        ]),
        Step::new(
            "the object's class becomes a protocol: `type(obj)` is of unknown class",
            vec![save(
                "pkg/seal.py",
                &SEAL.replace(
                    "class Seal:",
                    "from typing import Protocol\n\n\nclass Seal(Protocol):",
                ),
            )],
        )
        .present(&[("pkg/seal_type.py", "MUT207")])
        .absent(&[("pkg/seal_type.py", "MUT204")]),
        Step::new(
            "the protocol is a class again",
            vec![save("pkg/seal.py", SEAL)],
        )
        .present(&[("pkg/seal_type.py", "MUT204")]),
        Step::new(
            "the module is deleted",
            vec![Action::Delete("pkg/seal_type.py")],
        ),
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
    // A pass helper constructs a ledger whose initializer hands a private helper its own `self`,
    // then the ledger the pass was handed.
    let opening = |handed: &str| {
        format!(
            "{}\n\nclass Opened(Ledger):\n    def __init__(self, other: Ledger):\n        super().__init__()\n        _open({handed})\n\n\ndef _open(l: Ledger):\n    l.balance = 0\n",
            PURE_OPS.replace(
                "    l.rows.append(1)\n",
                "    l.rows.append(1)\n    Opened(l)\n"
            )
        )
    };
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
            "a pass helper constructs a ledger whose initializer hands its `self` to a helper writing it",
            vec![Action::Save("pkg/pure_ops.py", opening("self"))],
        )
        .absent(&[("pkg/pure.py", "EFF201")]),
        Step::new(
            "the initializer hands the helper the pass's ledger instead",
            vec![Action::Save("pkg/pure_ops.py", opening("other"))],
        )
        .present(&[("pkg/pure.py", "EFF201")]),
        Step::new(
            "the helpers are pure once more",
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

/// Owner-contract targets naming symbols outside the project (`FileSummary::external_accesses`):
/// a module calls and hands on a private function of `sys` another module re-exports, so which
/// symbol the call and the read are of follows what the re-export binds, in another file.
fn steps_outside_targets() -> Vec<Step> {
    let contract = format!(
        "{CONFIG}\n[[tool.heki.call-owners]]\nname = \"outside-calls\"\ntargets = [\"<external>._*\"]\nforbidden-callers = [\"pkg.frames:*\"]\n\n[[tool.heki.read-owners]]\nname = \"outside-reads\"\ntargets = [\"sys._getframe\"]\nforbidden-readers = [\"pkg.frames:*\"]\n"
    );
    let compat = "from sys import _getframe\n";
    let frames = "from pkg.frame_compat import _getframe\n\n\ndef caller():\n    return _getframe(1)\n\n\ndef handed():\n    return _getframe\n";
    vec![
        Step::new(
            "a module calls and hands on a private function of `sys` another module re-exports",
            vec![
                Action::Configure(contract),
                Action::Create("pkg/frame_compat.py", compat.to_owned()),
                Action::Create("pkg/frames.py", frames.to_owned()),
            ],
        )
        .present(&[("pkg/frames.py", "MUT204"), ("pkg/frames.py", "MUT203")])
        .absent(&[("pkg/frame_compat.py", "MUT203")]),
        Step::new(
            "the re-export binds a public function instead",
            vec![save(
                "pkg/frame_compat.py",
                "from os import getcwd as _getframe\n",
            )],
        )
        .absent(&[("pkg/frames.py", "MUT204"), ("pkg/frames.py", "MUT203")]),
        Step::new(
            "it binds the private function again",
            vec![save("pkg/frame_compat.py", compat)],
        )
        .present(&[("pkg/frames.py", "MUT204"), ("pkg/frames.py", "MUT203")]),
    ]
}

/// A glob target of a strict `read-owners` contract on an object of unknown class matches only
/// the members a class it covers has from project code, among them the names any module writes
/// on objects of unknown class (`heki_rules::class_member_names`): a write of `_cache` in a
/// module the reader neither imports nor calls makes the reader's untyped `parser._cache` a
/// `MUT206`, and its removal clears it.
fn steps_glob_members() -> Vec<Step> {
    let contract = format!(
        "{CONFIG}\n[[tool.heki.read-owners]]\nname = \"parser-state\"\ntargets = [\"pkg.glob_model.Parser._*\"]\nallowed-readers = [\"pkg.glob_model:*\"]\nunresolved = \"report\"\n"
    );
    let model = "class Parser:\n    def __init__(self) -> None:\n        self._state = 1\n";
    let peek = "def peek(parser):\n    return parser._cache\n";
    let fill = |body: &str| format!("def fill(obj):\n    {body}\n");
    vec![
        Step::new(
            "a strict glob target, and an untyped read of a name no class has",
            vec![
                Action::Configure(contract),
                Action::Create("pkg/glob_model.py", model.to_owned()),
                Action::Create("pkg/glob_peek.py", peek.to_owned()),
                Action::Create("pkg/glob_fill.py", fill("return None")),
            ],
        )
        .absent(&[("pkg/glob_peek.py", "MUT206")]),
        Step::new(
            "another module writes the name on an object of unknown class",
            vec![save("pkg/glob_fill.py", &fill("obj._cache = 1"))],
        )
        .present(&[("pkg/glob_peek.py", "MUT206")]),
        Step::new(
            "the write goes",
            vec![save("pkg/glob_fill.py", &fill("return None"))],
        )
        .absent(&[("pkg/glob_peek.py", "MUT206")]),
    ]
}

/// Step 5c: environment reads below an effect boundary. No summary records them, so the report
/// of the scope function above, and of the module body the boundary names, follows the helper's
/// own accesses: a read of `sys.platform`, a call of `os.getenv` (whose call site the report's
/// chain shows), the call moving down a line, an import of a module that is the project's only
/// while its file exists, and none.
fn steps_environment() -> Vec<Step> {
    let choose = |body: &str| PORTABLE_OPS.replace("    return \"posix\"\n", body);
    vec![
        Step::new(
            "a helper below a boundary starts reading `sys.platform`",
            vec![Action::Save(
                "pkg/portable_ops.py",
                choose("    return sys.platform\n"),
            )],
        )
        .present(&[("pkg/portable.py", "EFF201"), ("pkg/portable_boot.py", "EFF201")]),
        Step::new(
            "the helper calls `os.getenv` instead",
            vec![Action::Save(
                "pkg/portable_ops.py",
                choose("    return os.getenv(\"OS\")\n"),
            )],
        )
        .present(&[("pkg/portable.py", "EFF201"), ("pkg/portable_boot.py", "EFF201")]),
        Step::new(
            "the call moves down a line",
            vec![Action::Save(
                "pkg/portable_ops.py",
                choose("    name = \"OS\"\n    return os.getenv(name)\n"),
            )],
        )
        .present(&[("pkg/portable.py", "EFF201"), ("pkg/portable_boot.py", "EFF201")]),
        Step::new(
            "the helper imports a module the project does not have",
            vec![Action::Save(
                "pkg/portable_ops.py",
                choose(
                    "    from importlib import import_module\n\n    return import_module(\"pkg.portable_plugin\")\n",
                ),
            )],
        )
        .present(&[("pkg/portable.py", "EFF201"), ("pkg/portable_boot.py", "EFF201")]),
        Step::new(
            "the module it imports is created in the project",
            vec![Action::Create(
                "pkg/portable_plugin.py",
                "VALUE = 1\n".to_owned(),
            )],
        )
        .absent(&[("pkg/portable.py", "EFF201"), ("pkg/portable_boot.py", "EFF201")]),
        Step::new(
            "the module is deleted again",
            vec![Action::Delete("pkg/portable_plugin.py")],
        )
        .present(&[("pkg/portable.py", "EFF201"), ("pkg/portable_boot.py", "EFF201")]),
        Step::new(
            "the helper stops reading the environment",
            vec![save("pkg/portable_ops.py", PORTABLE_OPS)],
        )
        .absent(&[("pkg/portable.py", "EFF201"), ("pkg/portable_boot.py", "EFF201")]),
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
        .present(&[("pkg/worker_view.py", "EFF103 [heuristic]")])
        .absent(&[("pkg/worker_view.py", "EFF101 [heuristic]")]),
        Step::new(
            "the worker's query is removed",
            vec![save("pkg/worker_ops.py", WORKER_OPS)],
        )
        .absent(&[("pkg/worker_view.py", "EFF103 [heuristic]")]),
        // A function a builtin calls once per item: its caller is re-checked when the
        // function in another module starts or stops querying.
        Step::new(
            "the sort key in another module starts a query per item",
            vec![Action::Save(
                "pkg/rank_key.py",
                RANK_KEY.replace("    return i\n", "    return session.execute(i)\n"),
            )],
        )
        .present(&[("pkg/rank_user.py", "EFF103 [heuristic]")]),
        Step::new(
            "the sort key's query is removed",
            vec![save("pkg/rank_key.py", RANK_KEY)],
        )
        .absent(&[("pkg/rank_user.py", "EFF103 [heuristic]")]),
        Step::new(
            "a helper becomes a generator: the report moves to the iteration",
            vec![save("pkg/feed.py", FEED_GENERATOR)],
        )
        .present(&[("pkg/feed_user.py", "EFF101 [heuristic]")]),
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
        // `ProjectDecls::subclasses_of` and `mro_method`: a join over an argument annotated
        // `list` changes tier when another module defines or removes a subclass of `list` that
        // iterates its own way.
        Step::new(
            "another module defines a subclass of list with its own __iter__",
            vec![Action::Create("pkg/loud.py", LOUD_LIST.to_owned())],
        )
        .present(&[("pkg/join_user.py", "OPT113")]),
        Step::new(
            "the module with the subclass is deleted",
            vec![Action::Delete("pkg/loud.py")],
        )
        .present(&[("pkg/join_user.py", "OPT113")]),
        // `ProjectDecls::plain_attr`: the compared key turns into a property in another module
        // and back.
        Step::new(
            "the class of the joined items makes the key a property",
            vec![save("pkg/rows.py", ROWS_PROPERTY)],
        )
        .present(&[("pkg/join_user.py", "OPT113")]),
        Step::new(
            "the key is a dataclass field again",
            vec![save("pkg/rows.py", ROWS)],
        )
        .present(&[("pkg/join_user.py", "OPT113")]),
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
        // "Trip count open" of a rewrite (`DataOrigins::of_loop_items`): the rows of a call the
        // catalog sizes, through the class another module binds the receiver's annotation to. The
        // report stays, exact or heuristic, and the server must change its certainty with the CLI.
        Step::new(
            "a module loops over the rows of a cursor class another module re-exports",
            vec![
                Action::Create("pkg/storage.py", STORAGE.to_owned()),
                Action::Create("pkg/rows_user.py", ROWS_USER.to_owned()),
            ],
        )
        .present(&[("pkg/rows_user.py", "OPT103")]),
        Step::new(
            "the other module defines a cursor class of its own",
            vec![save("pkg/storage.py", STORAGE_OWN)],
        )
        .present(&[("pkg/rows_user.py", "OPT103")]),
        Step::new(
            "it re-exports the library's cursor again",
            vec![save("pkg/storage.py", STORAGE)],
        )
        .present(&[("pkg/rows_user.py", "OPT103")]),
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
/// of the helper, and so are the tests of its parameters under which a helper keeps that object
/// or the one it returns, and whether a copy helper returns an element of an optional table only
/// where the table defaults to a value without elements, and whether a factory's result holds
/// itself through the new objects it links; whether a model's write validates the value (its
/// class keywords, or a configuration in a subclass of another module), and whether an operator
/// on a linked object runs project code that may return what it reads out of it, is a
/// declaration of the modules of its classes; whether a helper returns its parameter
/// is a declaration of its module; whether a parameter is typed, and with which class, is a fact of
/// the callers the strict modes walk (`MUT205` writes, `MUT206` reads, `MUT207` calls), and
/// whether that class defines a method a call target naming no class names is a declaration of
/// the class's module.
fn steps_keys_fresh_strict() -> Vec<Step> {
    let storing = FRESH_OPS.replace(
        "    r.entries.append(1)\n",
        "    KEPT.append(r)\n    r.entries.append(1)\n",
    );
    let fixed_key = EXTRAS.replace("e.values[key]", "e.values[\"other\"]");
    vec![
        Step::new(
            "a store operation in another module stops writing the entry its caller passes",
            vec![Action::Save("pkg/extras.py", fixed_key)],
        )
        .absent(&[("pkg/extras_pass.py", "EFF201")]),
        Step::new(
            "the store operation writes the entry its caller passes again",
            vec![save("pkg/extras.py", EXTRAS)],
        )
        .present(&[("pkg/extras_pass.py", "EFF201")]),
        Step::new(
            "the scope function passes the key only through an allowed callee of another module",
            vec![
                save("pkg/extras.py", &format!("{EXTRAS}{EXTRAS_ROUTES}")),
                save("pkg/extras_pass.py", EXTRAS_TRUSTED_PASS),
            ],
        )
        .absent(&[("pkg/extras_pass.py", "EFF201")]),
        Step::new(
            "it passes the same key through a relay that is not allowed too",
            vec![save(
                "pkg/extras_pass.py",
                &format!("{EXTRAS_TRUSTED_PASS}    relay_value(e, \"init\")\n"),
            )],
        )
        .present(&[("pkg/extras_pass.py", "EFF201")]),
        Step::new(
            "the store module and the scope function are as before",
            vec![
                save("pkg/extras.py", EXTRAS),
                save("pkg/extras_pass.py", EXTRAS_PASS),
            ],
        )
        .present(&[("pkg/extras_pass.py", "EFF201")]),
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
            "the key module caches missing names in its `__getattr__`: the constant stays",
            vec![Action::Save(
                "pkg/key_consts.py",
                format!("{KEY_CONSTS}{KEY_HOOK}"),
            )],
        )
        .present(&[("pkg/key_writer.py", "MUT201")]),
        Step::new(
            "another module calls the hook itself, which may rebind the `Final` constant",
            vec![Action::Create("pkg/key_warm.py", KEY_WARM.to_owned())],
        )
        .absent(&[("pkg/key_writer.py", "MUT201")]),
        Step::new(
            "the key module drops its hook while the other module still calls it",
            vec![save("pkg/key_consts.py", KEY_CONSTS)],
        )
        .present(&[("pkg/key_writer.py", "MUT201")]),
        Step::new(
            "the hook is back while the other module calls it",
            vec![Action::Save(
                "pkg/key_consts.py",
                format!("{KEY_CONSTS}{KEY_HOOK}"),
            )],
        )
        .absent(&[("pkg/key_writer.py", "MUT201")]),
        Step::new(
            "the module calling the hook is deleted",
            vec![Action::Delete("pkg/key_warm.py")],
        )
        .present(&[("pkg/key_writer.py", "MUT201")]),
        Step::new(
            "the hook caches under another key, which may rebind the constant",
            vec![Action::Save(
                "pkg/key_consts.py",
                format!(
                    "{KEY_CONSTS}{}",
                    KEY_HOOK.replace("[name]", "[name.upper()]")
                ),
            )],
        )
        .absent(&[("pkg/key_writer.py", "MUT201")]),
        Step::new(
            "the hook caches under its own name what a generator expression over the globals makes",
            vec![Action::Save(
                "pkg/key_consts.py",
                format!(
                    "{KEY_CONSTS}{}",
                    KEY_HOOK.replace("object()", "tuple(n for n in globals())")
                ),
            )],
        )
        .present(&[("pkg/key_writer.py", "MUT201")]),
        Step::new(
            "the generator expression's body changes the globals, which may rebind the constant",
            vec![Action::Save(
                "pkg/key_consts.py",
                format!(
                    "{KEY_CONSTS}{}",
                    KEY_HOOK.replace(
                        "object()",
                        "tuple(globals().pop(n) for n in list(globals()))"
                    )
                ),
            )],
        )
        .absent(&[("pkg/key_writer.py", "MUT201")]),
        Step::new(
            "a tuple of the constant, which a loop in another module takes the key from",
            vec![
                Action::Save(
                    "pkg/key_consts.py",
                    format!("{KEY_CONSTS}KEYS: Final = (KEY,)\n"),
                ),
                Action::Create("pkg/key_loop.py", KEY_LOOP.to_owned()),
            ],
        )
        .present(&[("pkg/key_loop.py", "MUT201")]),
        Step::new(
            "the tuple holds a value that is no constant",
            vec![Action::Save(
                "pkg/key_consts.py",
                format!("{KEY_CONSTS}KEYS: Final = (KEY, str(1))\n"),
            )],
        )
        .absent(&[("pkg/key_loop.py", "MUT201")]),
        Step::new(
            "the key module is as before, and the loop is deleted",
            vec![
                save("pkg/key_consts.py", KEY_CONSTS),
                Action::Delete("pkg/key_loop.py"),
            ],
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
        Step::new(
            "the constructor of another module's class binds the list to a shared one",
            vec![Action::Save(
                "pkg/owned_model.py",
                OWNED_MODEL.replace("list = []", "list = SHARED"),
            )],
        )
        .present(&[("pkg/owned_pass.py", "EFF201")]),
        Step::new(
            "the constructor makes a new list again",
            vec![save("pkg/owned_model.py", OWNED_MODEL)],
        )
        .absent(&[("pkg/owned_pass.py", "EFF201")]),
        Step::new(
            "the base constructor of a third module replaces the instance's namespace",
            vec![Action::Save(
                "pkg/owned_base.py",
                OWNED_BASE.replace("self.ready = True", "self.__dict__ = SHARED_STATE"),
            )],
        )
        .present(&[("pkg/owned_pass.py", "EFF201")]),
        Step::new(
            "the base constructor only stores an attribute again",
            vec![save("pkg/owned_base.py", OWNED_BASE)],
        )
        .absent(&[("pkg/owned_pass.py", "EFF201")]),
        Step::new(
            "the base constructor of a third module keeps the instance",
            vec![Action::Save(
                "pkg/owned_base.py",
                OWNED_BASE.replace("self.ready = True", "SHARED_STATE[0] = self"),
            )],
        )
        .present(&[
            ("pkg/owned_flag_pass.py", "EFF201"),
            ("pkg/owned_pass.py", "EFF201"),
        ]),
        Step::new(
            "the base constructor keeps nothing again",
            vec![save("pkg/owned_base.py", OWNED_BASE)],
        )
        .absent(&[
            ("pkg/owned_flag_pass.py", "EFF201"),
            ("pkg/owned_pass.py", "EFF201"),
        ]),
        Step::new(
            "the copy helper of another module defaults its table to a shared dict",
            vec![Action::Save(
                "pkg/copy_ops.py",
                COPY_OPS.replace("| None = None", "= {}"),
            )],
        )
        .present(&[("pkg/copy_pass.py", "EFF201")]),
        Step::new(
            "the copy helper's table defaults to `None` again",
            vec![save("pkg/copy_ops.py", COPY_OPS)],
        )
        .absent(&[("pkg/copy_pass.py", "EFF201")]),
        Step::new(
            "the marking helper of another module keeps the ticket before marking it",
            vec![Action::Save(
                "pkg/copy_ops.py",
                COPY_OPS.replace(
                    "    t.flag = True\n",
                    "    KEPT.append(t)\n    t.flag = True\n",
                ),
            )],
        )
        .present(&[("pkg/copy_pass.py", "EFF201")]),
        Step::new(
            "the marking helper only marks the ticket again",
            vec![save("pkg/copy_ops.py", COPY_OPS)],
        )
        .absent(&[("pkg/copy_pass.py", "EFF201")]),
        Step::new(
            "the stamping helper of another module returns a shared ticket instead of its argument",
            vec![Action::Save(
                "pkg/copy_ops.py",
                format!("{COPY_OPS}\n\nSHARED = Ticket()\n")
                    .replace("    return t\n", "    return SHARED\n"),
            )],
        )
        .present(&[("pkg/copy_pass.py", "EFF201")]),
        Step::new(
            "the stamping helper returns its argument again",
            vec![save("pkg/copy_ops.py", COPY_OPS)],
        )
        .absent(&[("pkg/copy_pass.py", "EFF201")]),
        Step::new(
            "the maker and the holder of another module store only when the flag is false",
            vec![Action::Save(
                "pkg/guarded_keeps.py",
                GUARDED_KEEPS.replace("if keep:", "if not keep:"),
            )],
        )
        .present(&[("pkg/guarded_build.py", "EFF201")]),
        Step::new(
            "the maker stores whatever it is passed, the holder only when the flag is true",
            vec![Action::Save(
                "pkg/guarded_keeps.py",
                GUARDED_KEEPS.replacen("    if keep:\n        KEPT", "    KEPT", 1),
            )],
        )
        .present(&[("pkg/guarded_build.py", "EFF201")]),
        Step::new(
            "both store only when the flag is true again",
            vec![save("pkg/guarded_keeps.py", GUARDED_KEEPS)],
        )
        .absent(&[("pkg/guarded_build.py", "EFF201")]),
        Step::new(
            "the factory of another module links its part back to the result",
            vec![Action::Save(
                "pkg/link_ops.py",
                LINK_OPS.replace(
                    "    made.part = part\n",
                    "    made.part = part\n    part.part = made\n",
                ),
            )],
        )
        .present(&[("pkg/link_pass.py", "EFF201")]),
        Step::new(
            "the factory links only the part into the result again",
            vec![save("pkg/link_ops.py", LINK_OPS)],
        )
        .absent(&[("pkg/link_pass.py", "EFF201")]),
        Step::new(
            "the linked model of another module validates assignments by a class keyword",
            vec![Action::Save(
                "pkg/link_model.py",
                LINK_MODEL.replace("(BaseModel)", "(BaseModel, validate_assignment=True)"),
            )],
        )
        .present(&[("pkg/link_model_pass.py", "EFF201")]),
        Step::new(
            "the keyword is gone again",
            vec![save("pkg/link_model.py", LINK_MODEL)],
        )
        .absent(&[("pkg/link_model_pass.py", "EFF201")]),
        Step::new(
            "a subclass in a new module configures the model",
            vec![Action::Create(
                "pkg/link_model_sub.py",
                LINK_MODEL_SUB.to_owned(),
            )],
        )
        .present(&[("pkg/link_model_pass.py", "EFF201")]),
        Step::new(
            "the subclass is deleted",
            vec![Action::Delete("pkg/link_model_sub.py")],
        )
        .absent(&[("pkg/link_model_pass.py", "EFF201")]),
        Step::new(
            "the linked ticket's negation returns what the ticket holds",
            vec![Action::Save(
                "pkg/copy_ops.py",
                COPY_OPS.replace(
                    "        self.flag = False\n",
                    "        self.flag = False\n\n    def __neg__(self):\n        return self.part\n",
                ),
            )],
        )
        .present(&[("pkg/link_neg_pass.py", "EFF201")]),
        Step::new(
            "the ticket has no negation again",
            vec![save("pkg/copy_ops.py", COPY_OPS)],
        )
        .absent(&[("pkg/link_neg_pass.py", "EFF201")]),
    ]
}

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
            "the getter returns the container of a cast of its parameter",
            vec![save(
                "pkg/getter_src.py",
                &format!(
                    "from typing import cast\n{}",
                    GETTER_SRC.replace("return r.entries", "return cast(Registry, r).entries")
                ),
            )],
        )
        .present(&[("pkg/getter_use.py", "MUT201")]),
        Step::new(
            "the caller changes the container a pass-through function hands back",
            vec![
                save(
                    "pkg/getter_src.py",
                    &format!("{GETTER_SRC}\n\ndef same(x):\n    return x\n"),
                ),
                save(
                    "pkg/getter_use.py",
                    &GETTER_USE
                        .replace("import entries_of", "import entries_of, same")
                        .replace("entries_of(r).append(1)", "same(r.entries).append(1)"),
                ),
            ],
        )
        .present(&[("pkg/getter_use.py", "MUT201")]),
        Step::new(
            "the pass-through function returns a copy",
            vec![save(
                "pkg/getter_src.py",
                &format!("{GETTER_SRC}\n\ndef same(x):\n    return list(x)\n"),
            )],
        )
        .absent(&[("pkg/getter_use.py", "MUT201")]),
        Step::new(
            "the getter and its caller are back",
            vec![
                save("pkg/getter_src.py", GETTER_SRC),
                save("pkg/getter_use.py", GETTER_USE),
            ],
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
        Step::new(
            "another module calls `__setstate__` itself",
            vec![Action::Create(
                "pkg/setstate_call.py",
                SETSTATE_CALL.to_owned(),
            )],
        )
        .absent(&[("pkg/restoring.py", "EFF101")]),
        Step::new(
            "the module calling it is deleted",
            vec![Action::Delete("pkg/setstate_call.py")],
        )
        .present(&[("pkg/restoring.py", "EFF101")]),
        Step::new(
            "another module reads the method by its name, to call it later",
            vec![Action::Create(
                "pkg/setstate_read.py",
                SETSTATE_READ.to_owned(),
            )],
        )
        .absent(&[("pkg/restoring.py", "EFF101")]),
        Step::new(
            "it names another method",
            vec![save(
                "pkg/setstate_read.py",
                &SETSTATE_READ.replace("__setstate__", "__getstate__"),
            )],
        )
        .present(&[("pkg/restoring.py", "EFF101")]),
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

/// `MUT110` (default on): a `Sequence` / `Mapping` parameter handed to a helper in another
/// module that changes it in place. What the helper changes is in its summary, how its
/// parameter is annotated in its declarations (`FunctionDecl::readonly_params` /
/// `mutable_params`), and the call chain of a report walks the own summaries and calls of the
/// functions in other modules down to the first one whose body makes the change
/// (`heki_rules::ChainFacts`): a helper that moves the change to or from a function it calls
/// keeps its summary but changes the chain. The callers' annotations change in their own file
/// only (Early Cutoff); a helper is also reached through a package re-export. A function
/// returning what a function of another module returns of its parameter hands the caller's
/// object back as itself when both do (`FunctionDecl::returns_param`, which completing its
/// module's declarations decides against the other module's): either one copying instead
/// changes what the caller's call hands back, and so does either function becoming a name the
/// project binds again (`ProjectDecls::call_rebound`).
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
        // The report names the line of the change (`EffectSummary::param_inplace_sites`): a
        // comment above it moves the line while every summary stays the same.
        Step::new(
            "a comment above the helper two modules below moves the line the report names",
            vec![save(
                "pkg/ro/low.py",
                &format!("# helpers\n{low_resetting}"),
            )],
        )
        .present(&[("pkg/ro/stats.py", "MUT110")]),
        Step::new(
            "a comment between the helper's call and its change moves only the line the report names",
            vec![save(
                "pkg/ro/low.py",
                &format!(
                    "# helpers\n{}",
                    low_resetting.replace("    values[-1] = 0\n", "    # the last one\n    values[-1] = 0\n")
                ),
            )],
        )
        .present(&[("pkg/ro/stats.py", "MUT110")]),
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
        Step::new(
            "the function returning what another module's function returns copies instead",
            vec![save(
                "pkg/ro/forward.py",
                &RO_FORWARD.replace("return same(x)", "return list(x)"),
            )],
        )
        .absent(&[("pkg/ro/forward_use.py", "MUT110")]),
        Step::new(
            "it returns what the other module's function returns again",
            vec![save("pkg/ro/forward.py", RO_FORWARD)],
        )
        .present(&[("pkg/ro/forward_use.py", "MUT110")]),
        Step::new(
            "the function two modules below copies: what the call returns is a copy",
            vec![save("pkg/ro/same.py", "def same(x):\n    return list(x)\n")],
        )
        .absent(&[("pkg/ro/forward_use.py", "MUT110")]),
        Step::new(
            "the function two modules below returns its parameter again",
            vec![save("pkg/ro/same.py", RO_SAME)],
        )
        .present(&[("pkg/ro/forward_use.py", "MUT110")]),
        Step::new(
            "the module below binds that function again: the call may run another",
            vec![save(
                "pkg/ro/same.py",
                &format!("{RO_SAME}\n\ndef swap():\n    global same\n    same = list\n"),
            )],
        )
        .absent(&[("pkg/ro/forward_use.py", "MUT110")]),
        Step::new(
            "it binds the function once again",
            vec![save("pkg/ro/same.py", RO_SAME)],
        )
        .present(&[("pkg/ro/forward_use.py", "MUT110")]),
        Step::new(
            "the forwarding module stores another object into the function below",
            vec![save(
                "pkg/ro/forward.py",
                &format!("import pkg.ro.same as m\n{RO_FORWARD}\n\ndef swap():\n    m.same = list\n"),
            )],
        )
        .absent(&[("pkg/ro/forward_use.py", "MUT110")]),
        Step::new(
            "it stores nothing there again",
            vec![save("pkg/ro/forward.py", RO_FORWARD)],
        )
        .present(&[("pkg/ro/forward_use.py", "MUT110")]),
    ]
}

/// `MUT115`: the declarations a change below a copy reads in other modules (the copied class's
/// copy hooks, the project subclasses of the builtin containers) and a helper's summary.
fn steps_mut115() -> Vec<Step> {
    vec![
        Step::new(
            "the copied class in another module copies its children itself: the claim is open",
            vec![save("pkg/copied_node.py", COPIED_NODE_HOOKED)],
        )
        .present(&[("pkg/copy_user.py", "MUT115")]),
        Step::new(
            "the copied class leaves copying to `copy.copy` again",
            vec![save("pkg/copied_node.py", COPIED_NODE)],
        )
        .present(&[("pkg/copy_user.py", "MUT115")]),
        Step::new(
            "a new module subclasses dict and replaces its `copy` (R2): the claim is open",
            vec![Action::Create(
                "pkg/deep_dict.py",
                "class DeepDict(dict):\n    def copy(self):\n        return {k: list(v) for k, v in self.items()}\n".to_owned(),
            )],
        )
        .present(&[("pkg/copy_user.py", "MUT115")]),
        Step::new(
            "the subclass is deleted",
            vec![Action::Delete("pkg/deep_dict.py")],
        )
        .present(&[("pkg/copy_user.py", "MUT115")]),
        Step::new(
            "the helper in another module starts changing the list it is handed",
            vec![save("pkg/copy_helper.py", COPY_HELPER_GROWING)],
        )
        .present(&[("pkg/copy_handed.py", "MUT115")]),
        Step::new(
            "the helper only reads it again",
            vec![save("pkg/copy_helper.py", COPY_HELPER)],
        )
        .absent(&[("pkg/copy_handed.py", "MUT115")]),
        Step::new(
            "a new module binds a `__copy__` on the copied class after its body: the claim is open",
            vec![Action::Create("pkg/copy_patch.py", COPY_PATCH.to_owned())],
        )
        .present(&[("pkg/copy_user.py", "MUT115")]),
        Step::new(
            "the module is deleted",
            vec![Action::Delete("pkg/copy_patch.py")],
        )
        .present(&[("pkg/copy_user.py", "MUT115")]),
        Step::new(
            "a decorator of a new module that hands the class back decorates the copied class",
            vec![
                Action::Create("pkg/copy_deco.py", COPY_DECO.to_owned()),
                save("pkg/copied_node.py", COPIED_NODE_DECORATED),
            ],
        )
        .present(&[("pkg/copy_user.py", "MUT115")]),
        Step::new(
            "the decorator binds a copy hook on the class: the claim is open",
            vec![save("pkg/copy_deco.py", COPY_DECO_HOOK)],
        )
        .present(&[("pkg/copy_user.py", "MUT115")]),
        Step::new(
            "the decorator only reads the class again",
            vec![save("pkg/copy_deco.py", COPY_DECO)],
        )
        .present(&[("pkg/copy_user.py", "MUT115")]),
        Step::new(
            "the copied class is no longer decorated, and the decorator's module is deleted",
            vec![
                save("pkg/copied_node.py", COPIED_NODE),
                Action::Delete("pkg/copy_deco.py"),
            ],
        )
        .present(&[("pkg/copy_user.py", "MUT115")]),
        Step::new(
            "a new module stores under a built name that is never a copy hook's",
            vec![Action::Create(
                "pkg/copy_fill.py",
                COPY_FILL_NO_HOOK.to_owned(),
            )],
        )
        .present(&[("pkg/copy_user.py", "MUT115")]),
        Step::new(
            "the built name may be a copy hook's: the claim is open",
            vec![save("pkg/copy_fill.py", COPY_FILL_HOOK)],
        )
        .present(&[("pkg/copy_user.py", "MUT115")]),
        Step::new(
            "the store goes through an instance",
            vec![save("pkg/copy_fill.py", COPY_FILL_INSTANCE)],
        )
        .present(&[("pkg/copy_user.py", "MUT115")]),
        Step::new("the module is deleted", vec![Action::Delete("pkg/copy_fill.py")])
            .present(&[("pkg/copy_user.py", "MUT115")]),
    ]
}

/// `MUT202` through the summaries' changes below a parameter's items (`ELEMENTS_PATH`), handed on
/// through a copy of a parameter (`ArgOrigin::param_copy`) across modules.
fn steps_item_changes() -> Vec<Step> {
    vec![
        Step::new(
            "the helper two modules below gives the copied item a new list before changing it",
            vec![save("pkg/item_tag.py", ITEM_TAG_REBINDING)],
        )
        .absent(&[("pkg/item_user.py", "MUT202")]),
        Step::new(
            "the helper changes the item its copy shares again",
            vec![save("pkg/item_tag.py", ITEM_TAG)],
        )
        .present(&[("pkg/item_user.py", "MUT202")]),
        Step::new(
            "the relay hands the helper its parameter's copy of a copy",
            vec![save(
                "pkg/item_relay.py",
                &ITEM_RELAY.replace("tag(dict(x))", "tag(dict(dict(x)))"),
            )],
        )
        .present(&[("pkg/item_user.py", "MUT202")]),
        Step::new(
            "the relay hands the helper a new dict",
            vec![save(
                "pkg/item_relay.py",
                &ITEM_RELAY.replace("tag(dict(x))", "tag({\"tags\": []})"),
            )],
        )
        .absent(&[("pkg/item_user.py", "MUT202")]),
        Step::new(
            "the relay hands it a copy of its parameter again",
            vec![save("pkg/item_relay.py", ITEM_RELAY)],
        )
        .present(&[("pkg/item_user.py", "MUT202")]),
    ]
}

/// `MUT202` / `MUT114` return flow and `MUT208`: facts of a function in another module that a
/// caller's report reads (the wrapper's protected return, the helper's announcement), and a
/// caller's announcement after a call that a callee's report reads.
fn steps_mut114_mut202_mut208() -> Vec<Step> {
    vec![
        Step::new(
            "a cached object's method in another module stops filling a memo and records",
            vec![save("pkg/glyphs.py", GLYPHS_RECORDING)],
        )
        .present(&[("pkg/glyph_user.py", "MUT114")]),
        Step::new(
            "the method fills its memo again",
            vec![save("pkg/glyphs.py", GLYPHS)],
        )
        .absent(&[("pkg/glyph_user.py", "MUT114")]),
        Step::new(
            "a cached function in another module may return `None`: the report states less",
            vec![save("pkg/fields.py", FIELDS_MAYBE)],
        )
        .present(&[("pkg/fields_user.py", "MUT114")]),
        Step::new(
            "the cached function returns a list on every path again",
            vec![save("pkg/fields.py", FIELDS)],
        )
        .present(&[("pkg/fields_user.py", "MUT114")]),
        // Under R2 the list may be an instance of a project subclass: one in another module that
        // overrides `append` leaves the claim open (`ProjectDecls::method_not_overridden`).
        Step::new(
            "a new module subclasses list and overrides append",
            vec![Action::Create(
                "pkg/frozen_list.py",
                "class FrozenList(list):\n    def append(self, item):\n        pass\n".to_owned(),
            )],
        )
        .present(&[("pkg/fields_user.py", "MUT114")]),
        Step::new(
            "the subclass is deleted",
            vec![Action::Delete("pkg/frozen_list.py")],
        )
        .present(&[("pkg/fields_user.py", "MUT114")]),
        Step::new(
            "a wrapper in another module starts returning a declared producer's value",
            vec![save("pkg/lookup.py", LOOKUP_SHARED)],
        )
        .present(&[("pkg/tagger.py", "MUT202")]),
        Step::new(
            "the wrapper returns an `@lru_cache` result instead",
            vec![save("pkg/lookup.py", LOOKUP_CACHED)],
        )
        .present(&[("pkg/tagger.py", "MUT114")])
        .absent(&[("pkg/tagger.py", "MUT202")]),
        Step::new(
            "the wrapper returns a fresh value again",
            vec![save("pkg/lookup.py", LOOKUP)],
        )
        .absent(&[("pkg/tagger.py", "MUT202"), ("pkg/tagger.py", "MUT114")]),
        Step::new(
            "a helper in another module stops calling the invalidator",
            vec![save("pkg/announce.py", ANNOUNCE_SILENT)],
        )
        .present(&[("pkg/grow.py", "MUT208")]),
        Step::new(
            "the helper calls the invalidator again",
            vec![save("pkg/announce.py", ANNOUNCE)],
        )
        .absent(&[("pkg/grow.py", "MUT208")]),
        Step::new(
            "the only caller of a public writer in another module stops announcing",
            vec![save("pkg/grow.py", GROW_SILENT_ADOPT)],
        )
        .present(&[("pkg/nodes.py", "MUT208")]),
        Step::new(
            "the caller announces after the call again",
            vec![save("pkg/grow.py", GROW)],
        )
        .absent(&[("pkg/nodes.py", "MUT208")]),
        Step::new(
            "the caller two calls above a public writer stops announcing",
            vec![save("pkg/planter.py", PLANTER_SILENT)],
        )
        .present(&[("pkg/graft.py", "MUT208")]),
        Step::new(
            "the distant caller announces again",
            vec![save("pkg/planter.py", PLANTER)],
        )
        .absent(&[("pkg/graft.py", "MUT208")]),
        Step::new(
            "a helper in another module becomes a generator iterated after the invalidator",
            vec![save("pkg/kid_gen.py", KID_GEN_GENERATOR)],
        )
        .present(&[("pkg/kid_user.py", "MUT208")]),
        Step::new(
            "the helper returns a list again",
            vec![save("pkg/kid_gen.py", KID_GEN)],
        )
        .absent(&[("pkg/kid_user.py", "MUT208")]),
        Step::new(
            "a helper in another module starts changing its untyped argument in place",
            vec![save("pkg/kid_count.py", KID_COUNT_APPENDING)],
        )
        .present(&[("pkg/kid_stock.py", "MUT208")]),
        Step::new(
            "the helper only reads its argument again",
            vec![save("pkg/kid_count.py", KID_COUNT)],
        )
        .absent(&[("pkg/kid_stock.py", "MUT208")]),
    ]
}

/// The configuration with a second, shallow `readonly-returns` table over `Catalog.entry`,
/// with `fail` as given.
fn two_catalog_tables(fail: bool) -> String {
    format!(
        "{CONFIG}\n[[tool.heki.readonly-returns]]\nname = \"catalog-gate\"\nproducers = [\"pkg.catalog.Catalog.entry\"]\nfail = {fail}\n"
    )
}

/// `MUT202` per contract: which `readonly-returns` tables protect what a wrapper in another
/// module returns is a fact of the wrapper's summary, though the caller's report reads the same
/// with either: returning an item of the producer's value leaves it to the deep table.
fn steps_readonly_contracts() -> Vec<Step> {
    vec![
        Step::new(
            "a wrapper in another module returns a declared producer's value",
            vec![save("pkg/lookup.py", LOOKUP_SHARED)],
        )
        .present(&[("pkg/tagger.py", "MUT202")]),
        Step::new(
            "a shallow table over the producer that fails the run is added",
            vec![Action::Configure(two_catalog_tables(true))],
        )
        .present(&[("pkg/tagger.py", "MUT202")]),
        Step::new(
            "the wrapper returns an item of the producer's value, which only the deep table protects",
            vec![save("pkg/lookup.py", LOOKUP_SHARED_ITEM)],
        )
        .present(&[("pkg/tagger.py", "MUT202")])
        .propagates(),
        Step::new(
            "the wrapper returns the producer's value itself again",
            vec![save("pkg/lookup.py", LOOKUP_SHARED)],
        )
        .present(&[("pkg/tagger.py", "MUT202")])
        .propagates(),
        Step::new(
            "the shallow table stops failing the run",
            vec![Action::Configure(two_catalog_tables(false))],
        )
        .present(&[("pkg/tagger.py", "MUT202")]),
        Step::new(
            "the shallow table fails the run again",
            vec![Action::Configure(two_catalog_tables(true))],
        )
        .present(&[("pkg/tagger.py", "MUT202")]),
        Step::new(
            "the shallow table is removed and the wrapper returns a fresh value",
            vec![
                Action::Configure(CONFIG.to_owned()),
                save("pkg/lookup.py", LOOKUP),
            ],
        )
        .absent(&[("pkg/tagger.py", "MUT202")]),
    ]
}

/// An `invalidation` target names a class by its module (`pkg.dials:*.level`): the writes of a
/// class that moves to another module behind a re-export are no longer its target, directly or
/// through a helper (`MUT208`).
fn steps_module_patterns() -> Vec<Step> {
    vec![
        Step::new(
            "a class a module pattern names moves to another module behind a re-export",
            vec![
                Action::Write("pkg/dials_impl.py", DIALS_IMPL.to_owned()),
                save("pkg/dials.py", DIALS_REEXPORT),
            ],
        )
        .absent(&[("pkg/dial_user.py", "MUT208")]),
        Step::new("the class moves back", vec![save("pkg/dials.py", DIALS)])
            .present(&[("pkg/dial_user.py", "MUT208")]),
    ]
}

/// The parameters of its enclosing function that a nested scope reads from closure cells are
/// parameters of the scope (`CapturedParams`): a private method's generator expression filling
/// the entries of its `self` writes the method's own receiver, which an initializer in another
/// module calls while it initializes (`MUT201`). The generator expression turns to the method's
/// other parameter, which the initializer binds to an argument, and back; then the method rebinds
/// `self` before it, so the cell is no parameter any more.
fn steps_captured_parameters() -> Vec<Step> {
    let peer = REG_LOAD.replace("any(self.entries", "any(peer.entries");
    let rebound = REG_LOAD.replace("        any(", "        self = peer\n        any(");
    vec![
        Step::new(
            "an initializer in another module calls a private method filling its own entries in a generator expression",
            vec![
                Action::Create("pkg/reg_load.py", REG_LOAD.to_owned()),
                Action::Create("pkg/reg_sub.py", REG_SUB.to_owned()),
            ],
        )
        .absent(&[("pkg/reg_sub.py", "MUT201"), ("pkg/reg_load.py", "MUT201")]),
        Step::new(
            "the generator expression fills the entries of the method's other parameter",
            vec![Action::Save("pkg/reg_load.py", peer)],
        )
        .present(&[("pkg/reg_sub.py", "MUT201")]),
        Step::new(
            "the generator expression fills the method's own entries again",
            vec![save("pkg/reg_load.py", REG_LOAD)],
        )
        .absent(&[("pkg/reg_sub.py", "MUT201")]),
        Step::new(
            "the method rebinds `self` to its other parameter before the generator expression",
            vec![Action::Save("pkg/reg_load.py", rebound)],
        )
        .present(&[("pkg/reg_sub.py", "MUT201")]),
        Step::new(
            "the method stops rebinding `self`",
            vec![save("pkg/reg_load.py", REG_LOAD)],
        )
        .absent(&[("pkg/reg_sub.py", "MUT201")]),
    ]
}

/// A helper handed a typed field starts changing it itself, besides through the allowed
/// callee, while its final summary stays the same: the writes the boundary and the owner
/// contract count are decided from the helpers' own changes (`EFF201`, `MUT201`). A change of
/// an entry of the field's value is one of the field too, unless only the allowed callee makes
/// it; an attribute write on an entry is one only while the helper does not know the entry's
/// class, which an annotation alone changes (the helper changes the same paths of its
/// parameter either way).
fn steps_indirect_helpers() -> Vec<Step> {
    let changing = RACK_OPS.replace("    empty(xs)\n", "    xs.append(1)\n    empty(xs)\n");
    let entry = RACK_OPS.replace("    empty(xs)\n", "    xs[0].append(1)\n    empty(xs)\n");
    let allowed_entry = RACK_OPS.replace("    xs.clear()\n", "    xs[0].clear()\n");
    let untyped_attr = RACK_OPS.replace("    empty(xs)\n", "    xs[0].label = 1\n    empty(xs)\n");
    let typed_attr = format!(
        "from pkg.rack import Rack\n\n\n{}",
        untyped_attr.replace("def refill(xs):", "def refill(xs: list[Rack]):")
    );
    let writes = &[
        ("pkg/rack_view.py", "EFF201"),
        ("pkg/rack_view.py", "MUT201"),
    ];
    vec![
        Step::new(
            "a helper handed a field changes an entry of it outside the allowed callee",
            vec![Action::Save("pkg/rack_ops.py", entry)],
        )
        .present(writes),
        Step::new(
            "only the allowed callee changes an entry of the field",
            vec![Action::Save("pkg/rack_ops.py", allowed_entry)],
        )
        .absent(writes),
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
        Step::new(
            "the helper sets an attribute of an entry whose class it does not know",
            vec![Action::Save("pkg/rack_ops.py", untyped_attr)],
        )
        .present(writes),
        Step::new(
            "the helper types the entries: the attribute is their class's",
            vec![Action::Save("pkg/rack_ops.py", typed_attr)],
        )
        .absent(writes),
        Step::new(
            "the helper is back to its first form",
            vec![save("pkg/rack_ops.py", RACK_OPS)],
        )
        .absent(writes),
    ]
}

/// A method that changes its receiver in place, called on a field's value, writes the field at
/// the call, as a builtin container's changing method does, also in a helper handed the value:
/// the method's summary, in another file, decides it.
fn steps_receiver_changes() -> Vec<Step> {
    let reading = "class Shelf(list):\n    def stock(self):\n        return len(self)\n\n\nclass Rack:\n    def __init__(self):\n        self.items = Shelf()\n";
    let stocking = reading.replace("        return len(self)\n", "        self.append(1)\n");
    let direct = "from pkg.rack import Rack\n\n\ndef show(s: Rack):\n    s.items.stock()\n";
    let helper = "from pkg.rack import Shelf\n\n\ndef empty(xs):\n    xs.clear()\n\n\ndef refill(xs: Shelf):\n    xs.stock()\n    empty(xs)\n";
    let writes = &[
        ("pkg/rack_view.py", "EFF201"),
        ("pkg/rack_view.py", "MUT201"),
    ];
    vec![
        Step::new(
            "a field holds an object whose method only reads it",
            vec![
                save("pkg/rack.py", reading),
                save("pkg/rack_view.py", direct),
            ],
        )
        .absent(writes),
        Step::new(
            "the method changes its receiver: calling it on the field writes the field",
            vec![save("pkg/rack.py", &stocking)],
        )
        .present(writes),
        Step::new(
            "a helper handed the field calls the method before the allowed callee",
            vec![
                save("pkg/rack_view.py", RACK_VIEW),
                save("pkg/rack_ops.py", helper),
            ],
        )
        .present(writes),
        Step::new(
            "the method only reads its receiver again",
            vec![save("pkg/rack.py", reading)],
        )
        .absent(writes),
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
                Action::Configure(CONFIG.replace(", \"MUT112\", \"MUT113\"", "")),
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

const ABC_BOARD: &str = "from collections.abc import Mapping\nfrom functools import cached_property\n\n\nclass Board:\n    def __init__(self, raw: dict[str, str]) -> None:\n        self.raw = raw\n\n    @cached_property\n    def title(self) -> str:\n        return self.raw.get(\"k\", \"\")\n\n    def replace(self, raw: dict[str, str], rows: Mapping[str, str]) -> None:\n        self.raw = raw\n        for _ in rows:\n            pass\n";
const ABC_HEADING: &str = "from pkg.abc_board import Board\n\n\ndef heading(board: Board) -> str:\n    return board.title\n";
const ABC_ROWS: &str = "class Rows:\n    def __iter__(self):\n        return iter(())\n";
const ABC_ROWS_MAPPING: &str = "from collections.abc import Mapping\n\n\nclass Rows(Mapping):\n    def __getitem__(self, key):\n        raise KeyError(key)\n\n    def __len__(self):\n        return 0\n\n    def __iter__(self):\n        return iter(())\n";
const ABC_ROWS_REGISTERED: &str = "from collections.abc import Mapping\n\n\nclass Rows:\n    def __iter__(self):\n        return iter(())\n\n\nMapping.register(Rows)\n";

/// The tier of `MUT101` and of the `MUT111` reader behind it on a value of `Mapping` follows the
/// classes other files make one (`ProjectDecls::implementers`): the CLI's tier turns heuristic
/// when another file's class starts deriving from `Mapping` with its own `__iter__`, which the
/// writer runs after the write, and exact when it stops; then the same when the file registers
/// the class with `Mapping`, and stops. The server's tier must follow each save.
fn steps_container_abcs() -> Vec<Step> {
    const BOTH: &[(&str, &str)] = &[
        ("pkg/abc_board.py", "MUT101"),
        ("pkg/abc_heading.py", "MUT111"),
    ];
    vec![
        Step::new(
            "a writer iterating a `Mapping` after leaving a cached property stale, and its reader",
            vec![
                Action::Create("pkg/abc_board.py", ABC_BOARD.to_owned()),
                Action::Create("pkg/abc_heading.py", ABC_HEADING.to_owned()),
                Action::Create("pkg/abc_rows.py", ABC_ROWS.to_owned()),
            ],
        )
        .present(BOTH),
        Step::new(
            "another file's class derives from `Mapping` and binds `__iter__`",
            vec![save("pkg/abc_rows.py", ABC_ROWS_MAPPING)],
        )
        .present(BOTH),
        Step::new(
            "the class derives from nothing again",
            vec![save("pkg/abc_rows.py", ABC_ROWS)],
        )
        .present(BOTH),
        Step::new(
            "the file registers the class with `Mapping`",
            vec![save("pkg/abc_rows.py", ABC_ROWS_REGISTERED)],
        )
        .present(BOTH),
        Step::new(
            "the `register` call goes",
            vec![save("pkg/abc_rows.py", ABC_ROWS)],
        )
        .present(BOTH),
    ]
}

/// `MUT118`: a deep copy follows the sentinel, the class, and the tests in other modules (the
/// declarations its check reads, and the sentinel tests of each file,
/// `heki_rules::sentinel_tests_in`): the sentinel becomes one that copies as itself and back,
/// the class gains a copy hook and loses it, the test goes and comes back a line lower (its
/// related location moves), a new module deep-copies an instance of a subclass, and another
/// deep-copies a holder whose typed attribute keeps the dataclass while the dataclass gains a
/// copy hook and loses it.
fn steps_mut118() -> Vec<Step> {
    let lower = MARK_TESTS.replace("def given", "# The field was given.\ndef given");
    vec![
        Step::new(
            "a deep copy of a dataclass whose default is a sentinel another module tests",
            Vec::new(),
        )
        .present(&[("pkg/mark_copies.py", "MUT118")]),
        Step::new(
            "the sentinel copies as itself",
            vec![save("pkg/marks.py", MARKS_SENTINEL)],
        )
        .absent(&[("pkg/mark_copies.py", "MUT118")]),
        Step::new(
            "the sentinel is an `object()` again",
            vec![save("pkg/marks.py", MARKS)],
        )
        .present(&[("pkg/mark_copies.py", "MUT118")]),
        Step::new(
            "the class gains `__deepcopy__`",
            vec![save("pkg/marked_spec.py", MARKED_SPEC_HOOKED)],
        )
        .absent(&[("pkg/mark_copies.py", "MUT118")]),
        Step::new(
            "the class loses it",
            vec![save("pkg/marked_spec.py", MARKED_SPEC)],
        )
        .present(&[("pkg/mark_copies.py", "MUT118")]),
        Step::new(
            "the test module compares with `None` instead",
            vec![save("pkg/mark_tests.py", MARK_TESTS_NONE)],
        )
        .absent(&[("pkg/mark_copies.py", "MUT118")]),
        Step::new(
            "the test comes back, a line lower",
            vec![Action::Save("pkg/mark_tests.py", lower)],
        )
        .present(&[("pkg/mark_copies.py", "MUT118")]),
        Step::new(
            "the test moves again",
            vec![save("pkg/mark_tests.py", MARK_TESTS)],
        )
        .present(&[("pkg/mark_copies.py", "MUT118")]),
        Step::new(
            "the testing function is renamed to a name of the same length",
            vec![save(
                "pkg/mark_tests.py",
                &MARK_TESTS.replace("def given", "def check"),
            )],
        )
        .present(&[("pkg/mark_copies.py", "MUT118")]),
        Step::new("and back", vec![save("pkg/mark_tests.py", MARK_TESTS)])
            .present(&[("pkg/mark_copies.py", "MUT118")]),
        Step::new(
            "a new module deep-copies an instance of a subclass",
            vec![Action::Create("pkg/mark_sub.py", MARK_SUB.to_owned())],
        )
        .present(&[
            ("pkg/mark_sub.py", "MUT118"),
            ("pkg/mark_copies.py", "MUT118"),
        ]),
        Step::new(
            "a new module deep-copies a holder whose typed attribute keeps the dataclass",
            vec![Action::Create("pkg/mark_holder.py", MARK_HOLDER.to_owned())],
        )
        .present(&[("pkg/mark_holder.py", "MUT118")]),
        Step::new(
            "the dataclass gains `__deepcopy__` again",
            vec![save("pkg/marked_spec.py", MARKED_SPEC_HOOKED)],
        )
        .absent(&[("pkg/mark_holder.py", "MUT118")]),
        Step::new(
            "and loses it",
            vec![save("pkg/marked_spec.py", MARKED_SPEC)],
        )
        .present(&[("pkg/mark_holder.py", "MUT118")]),
    ]
}

/// `MUT119`: an item store through a property of a class in another module, by the final fact
/// of its getter: the getter returns a copy of its field, then the field itself, then a copy it
/// also keeps on the instance. Then an item store on what a function returns, two modules above
/// the function building it, when only the kind it builds changes. Then an append to what a
/// function returns whose callee in another module calls it back: the server recomputes the
/// caller with the edited callee as one SCC, as a full build does.
fn steps_mut119() -> Vec<Step> {
    vec![
        Step::new(
            "an item store on the copy a property in another module returns",
            Vec::new(),
        )
        .present(&[("pkg/lost_use.py", "MUT119")]),
        Step::new(
            "the getter returns its field",
            vec![save("pkg/lost_settings.py", LOST_SETTINGS_FIELD)],
        )
        .absent(&[("pkg/lost_use.py", "MUT119")]),
        Step::new(
            "the getter returns a copy again",
            vec![save("pkg/lost_settings.py", LOST_SETTINGS)],
        )
        .present(&[("pkg/lost_use.py", "MUT119")]),
        Step::new(
            "the getter keeps the copy on the instance",
            vec![save("pkg/lost_settings.py", LOST_SETTINGS_KEPT)],
        )
        .absent(&[("pkg/lost_use.py", "MUT119")]),
        Step::new(
            "and returns a copy it does not keep",
            vec![save("pkg/lost_settings.py", LOST_SETTINGS)],
        )
        .present(&[
            ("pkg/lost_use.py", "MUT119"),
            ("pkg/lost_item.py", "MUT119"),
        ]),
        Step::new(
            "the function two modules below builds a set",
            vec![save("pkg/lost_make.py", LOST_MAKE_SET)],
        )
        .absent(&[("pkg/lost_item.py", "MUT119")]),
        Step::new(
            "and a dict again",
            vec![save("pkg/lost_make.py", LOST_MAKE)],
        )
        .present(&[("pkg/lost_item.py", "MUT119")]),
        Step::new(
            "a function calling its caller back builds a list instead of a set",
            vec![save("pkg/lost_loop_collect.py", LOST_LOOP_COLLECT_NEW)],
        )
        .absent(&[("pkg/lost_loop_rebuild.py", "MUT119")]),
        Step::new(
            "and stops calling back",
            vec![save("pkg/lost_loop_collect.py", LOST_LOOP_COLLECT_ALONE)],
        )
        .present(&[("pkg/lost_loop_rebuild.py", "MUT119")]),
    ]
}

const TABLE_BASE: &str = "class Base:\n    pass\n";
const TABLE_BASE_UNKNOWN: &str = "from unknown_lib import Base\n";
const TABLES: &str = "import functools\n\nfrom pkg.table_base import Base\n\n\nclass Listed(Base):\n    pass\n\n\nclass CachedListed(Listed):\n    @functools.lru_cache(maxsize=None)\n    def rows(self, k: str) -> list[str]:\n        return [k, \"id\"]\n";
const TABLE_USER: &str = "from pkg.tables import Listed\n\n\ndef add(table: Listed, k):\n    xs = table.rows(k)\n    xs.append(\"x\")\n    return xs\n";

/// `MUT114`: the targets of a call decided by a module the caller does not import: the
/// receiver's class defines no such method and its base is a project class, so the call runs
/// the cached method of the one subclass with it; once that module imports the base from a
/// module heki knows nothing of, the base may define the method, and the server changes the
/// report's certainty with the CLI.
fn steps_mut114_targets() -> Vec<Step> {
    vec![
        Step::new(
            "a change of what a cached method of a subclass returns",
            vec![
                Action::Create("pkg/table_base.py", TABLE_BASE.to_owned()),
                Action::Create("pkg/tables.py", TABLES.to_owned()),
                Action::Create("pkg/table_user.py", TABLE_USER.to_owned()),
            ],
        )
        .present(&[("pkg/table_user.py", "MUT114")]),
        Step::new(
            "the base of the receiver's class becomes one heki knows nothing of",
            vec![save("pkg/table_base.py", TABLE_BASE_UNKNOWN)],
        )
        .present(&[("pkg/table_user.py", "MUT114")]),
        Step::new(
            "the base is a project class again",
            vec![save("pkg/table_base.py", TABLE_BASE)],
        )
        .present(&[("pkg/table_user.py", "MUT114")]),
    ]
}

/// The blocking helper of `pkg/low.py` with a module-level cache that it reads (`False`) or
/// clears (`True`) after its blocking call, so that the call keeps its position.
fn low_with_cache(clears: bool) -> String {
    let use_cache = if clears { "clear()" } else { "get(url)" };
    format!(
        "# network helpers\nimport requests\n\nCACHE = {{}}\n\n\ndef fetch(url):\n    response = requests.get(url)\n    CACHE.{use_cache}\n    return response\n# done\n"
    )
}

/// Queue capacities: a `put` waits only on a queue known to be bounded, which the module
/// building the queue, the callers binding a parameter, and the class family's own stores
/// decide, each possibly in another module than the `put`.
fn steps_queue_capacity() -> Vec<Step> {
    vec![
        Step::new(
            "the module building a queue another module's class stores gives it a maxsize",
            vec![save(
                "pkg/q_owner.py",
                &Q_OWNER.replace("queue.Queue()", "queue.Queue(maxsize=8)"),
            )],
        )
        .present(&[("pkg/q_publish.py", "EFF101 [heuristic]")]),
        Step::new(
            "it builds the queue unbounded again",
            vec![save("pkg/q_owner.py", Q_OWNER)],
        )
        .absent(&[("pkg/q_publish.py", "EFF101 [heuristic]")]),
        Step::new(
            "the caller of an async def putting on its parameter passes an unbounded queue",
            vec![save(
                "pkg/q_run.py",
                &Q_RUN.replace("queue.Queue(5)", "queue.Queue()"),
            )],
        )
        .absent(&[("pkg/q_drain.py", "EFF101 [heuristic]")]),
        Step::new(
            "it passes a bounded queue again",
            vec![save("pkg/q_run.py", Q_RUN)],
        )
        .present(&[("pkg/q_drain.py", "EFF101 [heuristic]")]),
        Step::new(
            "a subclass in another module defines the constant bounding the queue again",
            vec![save(
                "pkg/q_small.py",
                &Q_SMALL.replace("label = \"small\"", "_MAX = 0"),
            )],
        )
        .absent(&[("pkg/q_halt.py", "EFF101 [heuristic]")]),
        Step::new(
            "the subclass leaves the constant to its base again",
            vec![save("pkg/q_small.py", Q_SMALL)],
        )
        .present(&[("pkg/q_halt.py", "EFF101 [heuristic]")]),
        Step::new(
            "the module a global copies its queue from, and a function imports it from, bounds it",
            vec![save(
                "pkg/q_events.py",
                &Q_EVENTS.replace("queue.Queue()", "queue.Queue(4)"),
            )],
        )
        .present(&[
            ("pkg/q_mirror.py", "EFF101 [heuristic]"),
            ("pkg/q_local.py", "EFF101 [heuristic]"),
        ]),
        Step::new(
            "it builds the queue unbounded again",
            vec![save("pkg/q_events.py", Q_EVENTS)],
        )
        .absent(&[
            ("pkg/q_mirror.py", "EFF101 [heuristic]"),
            ("pkg/q_local.py", "EFF101 [heuristic]"),
        ]),
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

/// `EFF109`: the holder follows the locks its callees in another module take (their summaries'
/// `LockFacts::acquires`): a base-class helper stops and starts taking the lock a subclass method
/// holds, and the holder of a module global starts passing the argument that leads another
/// helper to it. It also follows the names they bind (`LockFacts::rebinds`) and the tests of
/// state on the way to the lock (`CallSite::state_tests`), and the summaries carry the facts
/// only while the rule is selected (`OptInFacts`). The exact tier follows whether the helper
/// takes the lock on every return (`LockAcquire::on_every_return`), and a report goes when a
/// method of the other module gives the lock back first (`LockFacts::gives_back`).
fn steps_eff109() -> Vec<Step> {
    let more = LOCK_POOL.replace(
        "            return 1\n\n\ndef refill",
        "            return 1\n\n    def checked_size(self):\n        if self.closed:\n            return 0\n        return self.size()\n\n\ndef reset():\n    global LOCK\n    LOCK = threading.Lock()\n\n\ndef refill",
    );
    let resetting = "from pkg.lock_pool import LOCK, Pool, refill, reset\n\n\nclass Spool(Pool):\n    def grow(self):\n        return self.size()\n\n\ndef drain():\n    with LOCK:\n        reset()\n        return refill(force=True)\n";
    let one_path = LOCK_POOL.replace(
        "    def size(self):\n        with self._lock:\n            return 1\n",
        "    def size(self):\n        if self.ready:\n            with self._lock:\n                return 1\n        return 0\n",
    );
    let pausing = LOCK_POOL.replace(
        "\n\ndef refill",
        "\n    def pause(self):\n        self._lock.release()\n\n\ndef refill",
    );
    let paused = LOCK_USE.replace(
        "        with self._lock:\n            return self.size()\n",
        "        with self._lock:\n            self.pause()\n            return self.size()\n",
    );
    let wrapped = "from pkg.lock_pool import LOCK, Pool, refill\n\n\nclass Spool(Pool):\n    def grow(self):\n        with self._lock:\n            self.closed = True\n            return self.checked_size()\n\n\ndef drain():\n    with LOCK:\n        return refill()\n";
    vec![
        Step::new(
            "the base class's helper in another module stops taking the lock",
            vec![save("pkg/lock_pool.py", LOCK_POOL_PLAIN)],
        )
        .absent(&[("pkg/lock_use.py", "EFF109")]),
        Step::new(
            "the helper takes the lock again",
            vec![save("pkg/lock_pool.py", LOCK_POOL)],
        )
        .present(&[("pkg/lock_use.py", "EFF109")]),
        Step::new(
            "the subclass stops holding the lock, and the other holder passes the argument that \
             leads the helper to the global lock",
            vec![save("pkg/lock_use.py", LOCK_USE_FORCED)],
        )
        .present(&[("pkg/lock_use.py", "EFF109")]),
        Step::new(
            "the helper no longer takes the global lock",
            vec![save(
                "pkg/lock_pool.py",
                &LOCK_POOL.replace(
                    "        with LOCK:\n            return 1\n",
                    "        return 1\n",
                ),
            )],
        )
        .absent(&[("pkg/lock_use.py", "EFF109")]),
        Step::new(
            "EFF109 is ignored, and the helper's module changes",
            vec![
                Action::Configure(CONFIG.replace(
                    "[tool.heki.lint]\n",
                    "[tool.heki.lint]\nignore = [\"EFF109\"]\n",
                )),
                Action::Save("pkg/lock_pool.py", format!("# Pools.\n{LOCK_POOL}")),
            ],
        )
        .absent(&[("pkg/lock_use.py", "EFF109")]),
        Step::new(
            "EFF109 is no longer ignored",
            vec![Action::Configure(CONFIG.to_owned())],
        )
        .present(&[("pkg/lock_use.py", "EFF109")]),
        Step::new(
            "the other holder calls a function of the helper's module that binds the global lock \
             again before the helper takes it",
            vec![
                Action::Save("pkg/lock_pool.py", more.clone()),
                save("pkg/lock_use.py", resetting),
            ],
        )
        .absent(&[("pkg/lock_use.py", "EFF109")]),
        Step::new(
            "that function no longer binds the lock",
            vec![Action::Save(
                "pkg/lock_pool.py",
                more.replace(
                    "    global LOCK\n    LOCK = threading.Lock()\n",
                    "    pass\n",
                ),
            )],
        )
        .present(&[("pkg/lock_use.py", "EFF109")]),
        Step::new(
            "the subclass holds the lock, sets a flag, and calls a wrapper of the other module \
             that tests it before the helper",
            vec![
                Action::Save("pkg/lock_pool.py", more.clone()),
                save("pkg/lock_use.py", wrapped),
            ],
        )
        .absent(&[("pkg/lock_use.py", "EFF109")]),
        Step::new(
            "the wrapper no longer tests the flag",
            vec![Action::Save(
                "pkg/lock_pool.py",
                more.replace("        if self.closed:\n            return 0\n", ""),
            )],
        )
        .present(&[("pkg/lock_use.py", "EFF109")]),
        Step::new(
            "both files as they were",
            vec![
                save("pkg/lock_pool.py", LOCK_POOL),
                save("pkg/lock_use.py", LOCK_USE),
            ],
        )
        .present(&[("pkg/lock_use.py", "EFF109")]),
        Step::new(
            "the helper takes the lock on one path only: the report is heuristic",
            vec![Action::Save("pkg/lock_pool.py", one_path)],
        )
        .present(&[("pkg/lock_use.py", "EFF109")]),
        Step::new(
            "the helper takes it on every return again: the report is exact",
            vec![save("pkg/lock_pool.py", LOCK_POOL)],
        )
        .present(&[("pkg/lock_use.py", "EFF109")]),
        Step::new(
            "the subclass calls a method of the base class that gives the lock back first",
            vec![
                Action::Save("pkg/lock_pool.py", pausing.clone()),
                Action::Save("pkg/lock_use.py", paused),
            ],
        )
        .absent(&[("pkg/lock_use.py", "EFF109")]),
        Step::new(
            "that method no longer gives the lock back",
            vec![Action::Save(
                "pkg/lock_pool.py",
                pausing.replace("        self._lock.release()\n", "        pass\n"),
            )],
        )
        .present(&[("pkg/lock_use.py", "EFF109")]),
        Step::new(
            "both files as they were again",
            vec![
                save("pkg/lock_pool.py", LOCK_POOL),
                save("pkg/lock_use.py", LOCK_USE),
            ],
        )
        .present(&[("pkg/lock_use.py", "EFF109")]),
    ]
}

/// `EFF110`: the fan-out follows what the coroutines it starts use in another module
/// (`EffectSummary::exclusive_uses`): the coroutine stops reading, reads again, and reads under a
/// lock.
fn steps_eff110() -> Vec<Step> {
    vec![
        Step::new(
            "the coroutine in another module stops reading the reader",
            vec![save("pkg/fan_read.py", FAN_READ_NOTHING)],
        )
        .absent(&[("pkg/fan_use.py", "EFF110")]),
        Step::new(
            "the coroutine reads it again",
            vec![save("pkg/fan_read.py", FAN_READ)],
        )
        .present(&[("pkg/fan_use.py", "EFF110")]),
        Step::new(
            "the coroutine reads it under a lock",
            vec![save("pkg/fan_read.py", FAN_READ_LOCKED)],
        )
        .absent(&[("pkg/fan_use.py", "EFF110")]),
        Step::new(
            "the coroutine reads it without the lock again",
            vec![save("pkg/fan_read.py", FAN_READ)],
        )
        .present(&[("pkg/fan_use.py", "EFF110")]),
    ]
}

/// `EFF112`: whether a call's result is in a set's order comes from the callee's summary
/// (`EffectSummary::set_order`), and whether the writer keeps it from the writer's own call: the
/// builder in another module sorts and stops sorting, the writer sorts the keys and stops.
fn steps_eff112() -> Vec<Step> {
    vec![
        Step::new(
            "the builder in another module sorts the set first",
            vec![save("pkg/order_src.py", ORDER_SRC_SORTED)],
        )
        .absent(&[("pkg/order_use.py", "EFF112")]),
        Step::new(
            "the builder iterates the set again",
            vec![save("pkg/order_src.py", ORDER_SRC)],
        )
        .present(&[("pkg/order_use.py", "EFF112")]),
        Step::new(
            "the writer sorts the keys",
            vec![save("pkg/order_use.py", ORDER_USE_SORTED)],
        )
        .absent(&[("pkg/order_use.py", "EFF112")]),
        Step::new(
            "the writer stops sorting them",
            vec![save("pkg/order_use.py", ORDER_USE)],
        )
        .present(&[("pkg/order_use.py", "EFF112")]),
    ]
}

/// `EFF114`: whether a call in the `try` body runs a call cycle comes from the summaries of the
/// functions it calls (`EffectSummary::call_cycle`): the module below starts calling back into
/// the handler's module, closing a cycle, and stops again.
fn steps_eff114() -> Vec<Step> {
    vec![
        Step::new(
            "the module below calls back into the handler's module",
            vec![save("pkg/rec_walk.py", REC_WALK_BACK)],
        )
        .present(&[("pkg/rec_handler.py", "EFF114")]),
        Step::new(
            "the module below stops calling back",
            vec![save("pkg/rec_walk.py", REC_WALK)],
        )
        .absent(&[("pkg/rec_handler.py", "EFF114")]),
        Step::new(
            "the exit status constant of another module becomes a string",
            vec![save("pkg/rec_status_codes.py", REC_STATUS_CODES_TEXT)],
        )
        .present(&[("pkg/rec_status.py", "EFF114")]),
        Step::new(
            "it is an integer again",
            vec![save("pkg/rec_status_codes.py", REC_STATUS_CODES)],
        )
        .absent(&[("pkg/rec_status.py", "EFF114")]),
        Step::new(
            "it is bound twice",
            vec![save("pkg/rec_status_codes.py", REC_STATUS_CODES_REBOUND)],
        )
        .present(&[("pkg/rec_status.py", "EFF114")]),
        Step::new(
            "it is bound once again",
            vec![save("pkg/rec_status_codes.py", REC_STATUS_CODES)],
        )
        .absent(&[("pkg/rec_status.py", "EFF114")]),
    ]
}

/// `EFF115`: whether the function a thread and `filter` are handed creates a coroutine comes
/// from its declaration in another module, which changes between `def` and `async def`.
fn steps_eff115() -> Vec<Step> {
    vec![
        Step::new(
            "the handed function becomes an async def",
            vec![save("pkg/coro_jobs.py", CORO_JOBS_ASYNC)],
        )
        .present(&[("pkg/coro_use.py", "EFF115")]),
        Step::new(
            "the handed function becomes a plain def again",
            vec![save("pkg/coro_jobs.py", CORO_JOBS)],
        )
        .absent(&[("pkg/coro_use.py", "EFF115")]),
    ]
}

/// `EFF117`: whether the subclass's write is reported comes from the base class's module, whose
/// thread writes the state holding the lock, whose helper takes the lock, and which starts the
/// thread; the locked write's line, which the report shows, moves there too. Whether a module
/// list cleared without the lock is reported comes from a third module, which starts or stops
/// running the locked writer on a thread: neither the writer nor the guarded states change.
fn steps_eff117() -> Vec<Step> {
    let unlocked = LOCKED_BASE.replace(
        "        with self._lock:\n            self.count += 1\n",
        "        self.count += 1\n",
    );
    let unstarted = LOCKED_BASE.replace(
        "        threading.Thread(target=self._tick).start()\n",
        "        self._tick()\n",
    );
    let coroutine = LOCKED_BASE.replace("    def _tick(self):", "    async def _tick(self):");
    let on_thread = format!(
        "import threading\n\n{}",
        LOCKED_LIST_RUN.replace(
            "    add(line)\n",
            "    threading.Thread(target=add, args=(line,)).start()\n",
        )
    );
    vec![
        Step::new(
            "the thread's writer in another module stops holding the lock",
            vec![save("pkg/locked_base.py", &unlocked)],
        )
        .absent(&[("pkg/locked_use.py", "EFF117")]),
        Step::new(
            "it holds the lock again",
            vec![save("pkg/locked_base.py", LOCKED_BASE)],
        )
        .present(&[("pkg/locked_use.py", "EFF117")]),
        Step::new(
            "the lines above the locked write move",
            vec![save(
                "pkg/locked_base.py",
                &format!("# counters\n{LOCKED_BASE}"),
            )],
        )
        .present(&[("pkg/locked_use.py", "EFF117")]),
        Step::new(
            "the subclass takes the lock through the base class's helper",
            vec![save("pkg/locked_use.py", LOCKED_USE_HELD)],
        )
        .absent(&[("pkg/locked_use.py", "EFF117")]),
        Step::new(
            "the subclass writes without the lock again",
            vec![save("pkg/locked_use.py", LOCKED_USE)],
        )
        .present(&[("pkg/locked_use.py", "EFF117")]),
        Step::new(
            "the thread's target becomes a coroutine function, which a thread only creates",
            vec![save("pkg/locked_base.py", &coroutine)],
        )
        .absent(&[("pkg/locked_use.py", "EFF117")]),
        Step::new(
            "it is a plain function again",
            vec![save("pkg/locked_base.py", LOCKED_BASE)],
        )
        .present(&[("pkg/locked_use.py", "EFF117")]),
        Step::new(
            "the base class no longer starts the thread",
            vec![save("pkg/locked_base.py", &unstarted)],
        )
        .absent(&[("pkg/locked_use.py", "EFF117")]),
        Step::new(
            "a module list is appended to holding a lock, cleared elsewhere, and run directly",
            vec![
                Action::Create("pkg/locked_list.py", LOCKED_LIST.to_owned()),
                Action::Create("pkg/locked_list_clear.py", LOCKED_LIST_CLEAR.to_owned()),
                Action::Create("pkg/locked_list_run.py", LOCKED_LIST_RUN.to_owned()),
            ],
        )
        .absent(&[("pkg/locked_list_clear.py", "EFF117")]),
        Step::new(
            "a third module runs the locked writer on a thread",
            vec![save("pkg/locked_list_run.py", &on_thread)],
        )
        .present(&[("pkg/locked_list_clear.py", "EFF117")]),
        Step::new(
            "it runs the writer directly again",
            vec![save("pkg/locked_list_run.py", LOCKED_LIST_RUN)],
        )
        .absent(&[("pkg/locked_list_clear.py", "EFF117")]),
    ]
}

/// `EFF116`: whether a call's callees are generator functions comes from their declarations in
/// other modules, a function's and a method's and its override's, which change between a
/// generator function and a plain one; a call that may run a plain override is no report.
/// `EFF118`: whether the `finally` body's await is reported comes from the helper's module, whose
/// sleep stops or starts being shielded, and from the module calling the function holding the
/// cleanup, which runs it shielded or not.
fn steps_eff118() -> Vec<Step> {
    let shielded_helper = CLEANUP_HELPERS.replace(
        "    await anyio.sleep(1)\n",
        "    with anyio.CancelScope(shield=True):\n        await anyio.sleep(1)\n",
    );
    let unshielded_caller = CLEANUP_CALLER.replace(
        "    with anyio.CancelScope(shield=True):\n        await close()\n",
        "    await close()\n",
    );
    vec![
        Step::new(
            "a `finally` body awaits a helper in another module that sleeps",
            vec![
                Action::Create("pkg/cleanup_helpers.py", CLEANUP_HELPERS.to_owned()),
                Action::Create("pkg/cleanup_use.py", CLEANUP_USE.to_owned()),
            ],
        )
        .present(&[("pkg/cleanup_use.py", "EFF118")]),
        Step::new(
            "the helper's module shields the sleep",
            vec![save("pkg/cleanup_helpers.py", &shielded_helper)],
        )
        .absent(&[("pkg/cleanup_use.py", "EFF118")]),
        Step::new(
            "the sleep is unshielded again",
            vec![save("pkg/cleanup_helpers.py", CLEANUP_HELPERS)],
        )
        .present(&[("pkg/cleanup_use.py", "EFF118")]),
        Step::new(
            "the only call of the function holding the cleanup runs it shielded",
            vec![Action::Create(
                "pkg/cleanup_caller.py",
                CLEANUP_CALLER.to_owned(),
            )],
        )
        .absent(&[("pkg/cleanup_use.py", "EFF118")]),
        Step::new(
            "the call stops shielding it",
            vec![save("pkg/cleanup_caller.py", &unshielded_caller)],
        )
        .present(&[("pkg/cleanup_use.py", "EFF118")]),
        Step::new(
            "it shields it again",
            vec![save("pkg/cleanup_caller.py", CLEANUP_CALLER)],
        )
        .absent(&[("pkg/cleanup_use.py", "EFF118")]),
    ]
}

fn steps_eff116() -> Vec<Step> {
    vec![
        Step::new(
            "the dropped function becomes a generator function",
            vec![save("pkg/gen_jobs.py", GEN_JOBS_GENERATOR)],
        )
        .present(&[("pkg/gen_use.py", "EFF116")]),
        Step::new(
            "the function becomes a plain function again",
            vec![save("pkg/gen_jobs.py", GEN_JOBS)],
        )
        .present(&[("pkg/gen_use.py", "EFF116")]),
        Step::new(
            "an override in a third module becomes a plain function",
            vec![save("pkg/gen_sub.py", GEN_SUB_PLAIN)],
        )
        .absent(&[("pkg/gen_use.py", "EFF116")]),
        Step::new(
            "the base method becomes a plain function too",
            vec![save("pkg/gen_base.py", GEN_BASE_PLAIN)],
        )
        .absent(&[("pkg/gen_use.py", "EFF116")]),
        Step::new(
            "both become generator functions again",
            vec![
                save("pkg/gen_base.py", GEN_BASE),
                save("pkg/gen_sub.py", GEN_SUB),
            ],
        )
        .present(&[("pkg/gen_use.py", "EFF116")]),
    ]
}

const LLM_CLIENT: &str = "from openai import OpenAI\n\nclient = OpenAI()\n";

const LLM_VIEW: &str = "from pkg.llm_client import client\n\n\nasync def answer():\n    return client.chat.completions.create(model=\"m\")\n";

/// Attribute types of library classes (`[[attributes]]`): the class of a client another module
/// constructs, a project subclass binding the attribute, and a local client of the other class.
fn steps_attribute_types() -> Vec<Step> {
    vec![
        Step::new(
            "a module reads the calls of a client another module constructs",
            vec![
                Action::Create("pkg/llm_client.py", LLM_CLIENT.to_owned()),
                Action::Create("pkg/llm_view.py", LLM_VIEW.to_owned()),
            ],
        )
        .present(&[("pkg/llm_view.py", "EFF101")]),
        Step::new(
            "the other module constructs the asynchronous client",
            vec![save(
                "pkg/llm_client.py",
                &LLM_CLIENT.replace("OpenAI", "AsyncOpenAI"),
            )],
        )
        .absent(&[("pkg/llm_view.py", "EFF101")]),
        Step::new(
            "it constructs the synchronous client again",
            vec![save("pkg/llm_client.py", LLM_CLIENT)],
        )
        .present(&[("pkg/llm_view.py", "EFF101")]),
        Step::new(
            "a project subclass of the client binds the attribute",
            vec![Action::Create(
                "pkg/llm_mine.py",
                "from openai import OpenAI\n\n\nclass Mine(OpenAI):\n    chat = None\n".to_owned(),
            )],
        )
        .absent(&[("pkg/llm_view.py", "EFF101")]),
        Step::new(
            "the subclass binds it no more",
            vec![save(
                "pkg/llm_mine.py",
                "from openai import OpenAI\n\n\nclass Mine(OpenAI):\n    pass\n",
            )],
        )
        .present(&[("pkg/llm_view.py", "EFF101")]),
        Step::new(
            "the reading function uses an asynchronous client of its own",
            vec![save(
                "pkg/llm_view.py",
                &LLM_VIEW.replace(
                    "    return client.",
                    "    from openai import AsyncOpenAI\n\n    client = AsyncOpenAI()\n    return client.",
                ),
            )],
        )
        .absent(&[("pkg/llm_view.py", "EFF101")])
        .cutoff(),
        Step::new(
            "it reads the module's client again",
            vec![save("pkg/llm_view.py", LLM_VIEW)],
        )
        .present(&[("pkg/llm_view.py", "EFF101")])
        .cutoff(),
    ]
}

/// The receivers whose class the language or the standard library fixes: an instance's
/// namespace is a dict while reading it runs no code of its class, a `NamedTuple` instance has
/// `_replace`, and a `ContextVar[T]` gives a `T`, each decided by declarations in another module;
/// a value typed with a protocol keeps its namespace open, and a compatibility module
/// re-exporting `typing.Protocol` or defining a class of that name decides whether its class is
/// one.
fn steps_builtin_receivers() -> Vec<Step> {
    const MODEL: &str = "from contextvars import ContextVar\nfrom typing import NamedTuple\n\n\nclass Holder:\n    def __init__(self) -> None:\n        self.required = False\n\n\nclass Memo:\n    def bump(self, h: Holder) -> None:\n        pass\n\n\nclass Spot(NamedTuple):\n    module: str\n\n\nMEMO: ContextVar[Memo] = ContextVar(\"memo\")\n";
    const PEEK: &str = "from pkg.rcv_model import Holder\n\n\ndef peek(h: Holder) -> object:\n    return h.__dict__.get(\"x\")\n";
    const MOVED: &str = "from pkg.rcv_model import Spot\n\n\ndef moved(s: Spot) -> Spot:\n    return s._replace(module=\"m\")\n";
    const BUMPED: &str = "from pkg.rcv_model import MEMO, Holder\n\n\ndef bumped(h: Holder) -> None:\n    MEMO.get().bump(h)\n";
    // A base named `Protocol` that a compatibility module may define or re-export.
    const OWN_PROTOCOL: &str = "class Protocol:\n    pass\n";
    const ALL: &[(&str, &str)] = &[
        ("pkg/rcv_peek.py", "EFF201"),
        ("pkg/rcv_moved.py", "EFF201"),
        ("pkg/rcv_bumped.py", "EFF201"),
    ];
    let through_compat = MODEL
        .replace(
            "from typing import NamedTuple\n",
            "from typing import NamedTuple\n\nfrom pkg.rcv_compat import Protocol\n",
        )
        .replace("class Holder:", "class Holder(Protocol):");
    let boundary = format!(
        "{CONFIG}\n[[tool.heki.effect-boundaries]]\nname = \"receivers\"\nscopes = [\"pkg.rcv_peek\", \"pkg.rcv_moved\", \"pkg.rcv_bumped\"]\nwrite-universe = [\"Holder.required\"]\nallow-writes = []\nunresolved = \"report\"\n"
    );
    vec![
        Step::new(
            "modules read an instance's namespace, copy a named tuple, and call what a context \
             variable holds",
            vec![
                Action::Configure(boundary),
                Action::Create("pkg/rcv_model.py", MODEL.to_owned()),
                Action::Create("pkg/rcv_peek.py", PEEK.to_owned()),
                Action::Create("pkg/rcv_moved.py", MOVED.to_owned()),
                Action::Create("pkg/rcv_bumped.py", BUMPED.to_owned()),
            ],
        )
        .absent(ALL),
        Step::new(
            "the class defines `__getattribute__`: reading its namespace may run code",
            vec![save(
                "pkg/rcv_model.py",
                &MODEL.replace(
                    "        self.required = False\n",
                    "        self.required = False\n\n    def __getattribute__(self, name: str) -> object:\n        return object.__getattribute__(self, name)\n",
                ),
            )],
        )
        .present(&[("pkg/rcv_peek.py", "EFF201")])
        .absent(&[("pkg/rcv_moved.py", "EFF201"), ("pkg/rcv_bumped.py", "EFF201")]),
        Step::new(
            "the named tuple becomes a plain class",
            vec![save(
                "pkg/rcv_model.py",
                &MODEL.replace("class Spot(NamedTuple):", "class Spot:"),
            )],
        )
        .present(&[("pkg/rcv_moved.py", "EFF201")])
        .absent(&[("pkg/rcv_peek.py", "EFF201"), ("pkg/rcv_bumped.py", "EFF201")]),
        Step::new(
            "the context variable loses its type argument",
            vec![save(
                "pkg/rcv_model.py",
                &MODEL.replace("MEMO: ContextVar[Memo] =", "MEMO ="),
            )],
        )
        .present(&[("pkg/rcv_bumped.py", "EFF201")])
        .absent(&[("pkg/rcv_peek.py", "EFF201"), ("pkg/rcv_moved.py", "EFF201")]),
        Step::new(
            "the class names a base a compatibility module defines",
            vec![
                Action::Create("pkg/rcv_compat.py", OWN_PROTOCOL.to_owned()),
                save("pkg/rcv_model.py", &through_compat),
            ],
        )
        .absent(ALL),
        Step::new(
            "the compatibility module re-exports `typing.Protocol`: the class is a protocol",
            vec![save("pkg/rcv_compat.py", "from typing import Protocol\n")],
        )
        .present(&[("pkg/rcv_peek.py", "EFF201")])
        .absent(&[("pkg/rcv_moved.py", "EFF201"), ("pkg/rcv_bumped.py", "EFF201")]),
        Step::new(
            "the compatibility module defines the base again: the class is no protocol",
            vec![save("pkg/rcv_compat.py", OWN_PROTOCOL)],
        )
        .absent(ALL),
        Step::new(
            "the declarations are as before",
            vec![save("pkg/rcv_model.py", MODEL)],
        )
        .absent(ALL),
        Step::new(
            "the files and the boundary are removed",
            vec![
                Action::Delete("pkg/rcv_compat.py"),
                Action::Delete("pkg/rcv_peek.py"),
                Action::Delete("pkg/rcv_moved.py"),
                Action::Delete("pkg/rcv_bumped.py"),
                Action::Delete("pkg/rcv_model.py"),
                Action::Configure(CONFIG.to_owned()),
            ],
        ),
    ]
}

/// The initial index.
fn steps_initial_index() -> Vec<Step> {
    vec![Step::new("initial index", Vec::new()).present(&[
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
        ("pkg/lock_use.py", "EFF109"),
        ("pkg/fan_use.py", "EFF110"),
        ("pkg/order_use.py", "EFF112"),
    ])]
}

/// A sync helper two modules below an `async def` starts blocking; comments around the call
/// are Early Cutoffs, a module global the helper changes and a call it cannot resolve are not,
/// and a syntax error in the middle callee hides the chain until it parses again; then the
/// helper stops blocking.
fn steps_blocking_chain() -> Vec<Step> {
    vec![
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
        // A callee starts and stops changing a module global: no rule reports it, but the
        // callers' summaries carry it (`heki inspect`'s purity), so the edit is no Early Cutoff.
        Step::new(
            "the helper's module gains a cache",
            vec![save("pkg/low.py", &low_with_cache(false))],
        )
        .present(&[("pkg/top.py", "EFF101")]),
        Step::new(
            "the helper below the async def starts clearing the module's cache",
            vec![save("pkg/low.py", &low_with_cache(true))],
        )
        .present(&[("pkg/top.py", "EFF101")])
        .propagates(),
        Step::new(
            "the helper stops clearing the cache",
            vec![save("pkg/low.py", &low_with_cache(false))],
        )
        .present(&[("pkg/top.py", "EFF101")])
        .propagates(),
        Step::new(
            "the cache goes away",
            vec![save(
                "pkg/low.py",
                &format!("# network helpers\n{LOW_BLOCKING}# done\n"),
            )],
        )
        .present(&[("pkg/top.py", "EFF101")]),
        // A callee starts and stops making a call heki cannot resolve: no rule reports it, but
        // `heki inspect` names it in the purity of every caller, so the edit is no Early Cutoff.
        Step::new(
            "the helper below the async def starts a call heki cannot resolve",
            vec![save(
                "pkg/low.py",
                &format!(
                    "# network helpers\n{}# done\n",
                    LOW_BLOCKING.replace("    return", "    url.strip()\n    return")
                ),
            )],
        )
        .present(&[("pkg/top.py", "EFF101")])
        .propagates(),
        Step::new(
            "the helper stops making the call",
            vec![save(
                "pkg/low.py",
                &format!("# network helpers\n{LOW_BLOCKING}# done\n"),
            )],
        )
        .present(&[("pkg/top.py", "EFF101")])
        .propagates(),
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
    ]
}

const READS_LOW: &str = "def tally(values):\n    return len(values)\n";

const READS_TOP: &str = "from pkg.reads.low import tally\n\n\ndef total(items: list[int]) -> int:\n    return tally(items)\n";

/// The file `file` of `common::panels` with `edits`.
fn panels_file(file: &str, edits: &[(&str, &str, &str)]) -> String {
    (common::panels::files(edits).into_iter())
        .find_map(|(f, text)| (f == file).then_some(text))
        .unwrap()
}

/// The second owner registers the first owner's view too.
const SHARED: (&str, &str, &str) = ("owners.py", "capture(Deck, self)", "capture(Page, self)");
const BOTH_OWNERS: Registered = &[
    ("pkg.reg.owners.Left", &["journal"]),
    ("pkg.reg.owners.Right", &["audit"]),
];
const DISPATCHED: &str = "pkg.reg.views.Page.store";

/// Registration contexts (`EffectSummary::dispatch`), which no diagnostic shows: callbacks two
/// owners register in one file reach a handler of another through a table kept in a third, an
/// adapter, and a lookup by name, and `heki inspect` gives one context per owner. A registration,
/// the signature the adapter fills, and the final write are edited across files and restored:
/// each owner's context follows its own registration only, every file's diagnostics stay equal
/// to `heki check`'s, and a comment keeps the facts (Early Cutoff).
fn steps_registrations() -> Vec<Step> {
    let owners = |edits: &[(&str, &str, &str)]| panels_file("owners.py", edits);
    let keyword_only = (
        "views.py",
        "async def choose(self, owner, event):",
        "async def choose(self, owner, *, event):",
    );
    let read = (
        "owners.py",
        "self.journal.append(1)",
        "self.journal.count(1)",
    );
    vec![
        Step::new(
            "two owners register callbacks for one view",
            vec![
                Action::Create("pkg/reg/__init__.py", String::new()),
                Action::Create("pkg/reg/views.py", panels_file("views.py", &[])),
                Action::Create("pkg/reg/channel.py", panels_file("channel.py", &[])),
                Action::Create("pkg/reg/adapter.py", panels_file("adapter.py", &[])),
                Action::Create("pkg/reg/owners.py", owners(&[SHARED])),
            ],
        )
        .registered(DISPATCHED, BOTH_OWNERS),
        Step::new(
            "the second owner registers another view",
            vec![save("pkg/reg/owners.py", &owners(&[]))],
        )
        .registered(DISPATCHED, &[("pkg.reg.owners.Left", &["journal"])]),
        Step::new(
            "it registers the first view again",
            vec![save("pkg/reg/owners.py", &owners(&[SHARED]))],
        )
        .registered(DISPATCHED, BOTH_OWNERS),
        Step::new(
            "the dispatching method takes the event by keyword only",
            vec![save(
                "pkg/reg/views.py",
                &panels_file("views.py", &[keyword_only]),
            )],
        )
        .registered(DISPATCHED, &[]),
        Step::new(
            "it takes the event by position again",
            vec![save("pkg/reg/views.py", &panels_file("views.py", &[]))],
        )
        .registered(DISPATCHED, BOTH_OWNERS),
        Step::new(
            "the first owner's final write becomes a read",
            vec![save("pkg/reg/owners.py", &owners(&[SHARED, read]))],
        )
        .registered(
            DISPATCHED,
            &[
                ("pkg.reg.owners.Left", &[]),
                ("pkg.reg.owners.Right", &["audit"]),
            ],
        ),
        Step::new(
            "it writes again",
            vec![save("pkg/reg/owners.py", &owners(&[SHARED]))],
        )
        .registered(DISPATCHED, BOTH_OWNERS),
        Step::new(
            "a comment above the adapter",
            vec![save(
                "pkg/reg/adapter.py",
                &format!(
                    "# Orders the arguments.\n{}",
                    panels_file("adapter.py", &[])
                ),
            )],
        )
        .cutoff()
        .registered(DISPATCHED, BOTH_OWNERS),
    ]
}

/// The parameters a function only reads (`EffectSummary::only_read_params`), which no
/// diagnostic shows: a callee in another file starts changing its parameter, or keeping it,
/// and the caller's fact goes after the save, in the server's index (it re-analyzes the
/// caller's file) and in `heki inspect` alike; an edit that keeps the fact stops at the file.
/// The kinds of read the callee makes (`EffectSummary::read_kinds`) follow the same way: it
/// starts reading an item of its parameter, then measures it again.
fn steps_only_read() -> Vec<Step> {
    let low = |body: &str| READS_LOW.replace("    return len(values)\n", body);
    vec![
        Step::new(
            "a caller hands its list to a helper in another file that only reads it",
            vec![
                Action::Create("pkg/reads/__init__.py", String::new()),
                Action::Create("pkg/reads/low.py", READS_LOW.to_owned()),
                Action::Create("pkg/reads/top.py", READS_TOP.to_owned()),
            ],
        )
        .only_reads("pkg.reads.top.total", &["items"])
        .reads("pkg.reads.top.total", "items", &["size and truth"]),
        Step::new(
            "the helper starts changing its parameter",
            vec![save(
                "pkg/reads/low.py",
                &low("    values.append(0)\n    return len(values)\n"),
            )],
        )
        .propagates()
        .only_reads("pkg.reads.top.total", &[]),
        Step::new(
            "the helper only reads it again",
            vec![save("pkg/reads/low.py", READS_LOW)],
        )
        .propagates()
        .only_reads("pkg.reads.top.total", &["items"]),
        Step::new(
            "the helper starts putting its parameter in a list",
            vec![save("pkg/reads/low.py", &low("    return len([values])\n"))],
        )
        .propagates()
        .only_reads("pkg.reads.top.total", &[]),
        Step::new(
            "the helper stops putting it in a list",
            vec![save("pkg/reads/low.py", READS_LOW)],
        )
        .propagates()
        .only_reads("pkg.reads.top.total", &["items"]),
        Step::new(
            "a comment in the helper",
            vec![save("pkg/reads/low.py", &format!("# counts\n{READS_LOW}"))],
        )
        .cutoff()
        .only_reads("pkg.reads.top.total", &["items"]),
        Step::new(
            "the helper starts reading an item of its parameter",
            vec![save("pkg/reads/low.py", &low("    return values[0]\n"))],
        )
        .propagates()
        .only_reads("pkg.reads.top.total", &["items"])
        .reads("pkg.reads.top.total", "items", &["item access"]),
        Step::new(
            "the helper measures its parameter again",
            vec![save("pkg/reads/low.py", READS_LOW)],
        )
        .propagates()
        .reads("pkg.reads.top.total", "items", &["size and truth"]),
    ]
}

/// Property getters: a cached property, a property `__hash__` reads, the setter or
/// `__setattr__` a base class in another module gives a store, and a getter read from an
/// `async def` in another file.
fn steps_property_getters() -> Vec<Step> {
    vec![
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
            "the base class in another module makes `key` a property: the subclass's store runs its setter",
            vec![Action::Save(
                "pkg/token_base.py",
                format!(
                    "{TOKEN_BASE}\n    @property\n    def key(self):\n        return self.__dict__[\"key\"]\n\n    @key.setter\n    def key(self, value):\n        self.__dict__[\"key\"] = value\n"
                ),
            )],
        )
        .absent(&[("pkg/token_user.py", "MUT107")]),
        Step::new(
            "the base class defines `__setattr__` instead",
            vec![Action::Save(
                "pkg/token_base.py",
                format!(
                    "{TOKEN_BASE}\n    def __setattr__(self, name, value):\n        object.__setattr__(self, name, value)\n"
                ),
            )],
        )
        .absent(&[("pkg/token_user.py", "MUT107")]),
        Step::new(
            "the base class is built on a base heki names no class for",
            vec![Action::Save(
                "pkg/token_base.py",
                format!(
                    "def make_base():\n    return object\n\n\n{}",
                    TOKEN_BASE.replace("class Token:", "class Token(make_base()):")
                ),
            )],
        )
        .absent(&[("pkg/token_user.py", "MUT107")]),
        Step::new(
            "the base class is plain again",
            vec![save("pkg/token_base.py", TOKEN_BASE)],
        )
        .present(&[("pkg/token_user.py", "MUT107")]),
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
    ]
}

/// Guarded effects across modules.
fn steps_guarded_effects() -> Vec<Step> {
    vec![
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
            "a helper in another file comes to sleep for its parameter, which the caller passes as zero",
            vec![save(
                "pkg/catalog_nap.py",
                &CATALOG_NAP.replace("sleep(1)", "sleep(n)"),
            )],
        )
        .absent(&[
            ("pkg/catalog_use.py", "EFF101"),
            ("pkg/catalog_use.py", "EFF101 [heuristic]"),
        ]),
        Step::new(
            "the caller passes a sleep the catalog has no case for",
            vec![save(
                "pkg/catalog_use.py",
                &CATALOG_USE.replace("nap(0)", "nap(5)"),
            )],
        )
        .present(&[("pkg/catalog_use.py", "EFF101")]),
        Step::new(
            "the caller passes zero again",
            vec![save("pkg/catalog_use.py", CATALOG_USE)],
        )
        .absent(&[
            ("pkg/catalog_use.py", "EFF101"),
            ("pkg/catalog_use.py", "EFF101 [heuristic]"),
        ]),
        Step::new(
            "the helper sleeps for a constant again",
            vec![save("pkg/catalog_nap.py", CATALOG_NAP)],
        )
        .present(&[("pkg/catalog_use.py", "EFF101")]),
        Step::new(
            "the helper queries the connection it is passed, a connection opened on a file",
            vec![
                save("pkg/catalog_nap.py", CATALOG_QUERY),
                save("pkg/catalog_use.py", CATALOG_QUERY_USE),
            ],
        )
        .present(&[("pkg/catalog_use.py", "EFF101")]),
        Step::new(
            "the caller passes a connection to a database in memory",
            vec![save(
                "pkg/catalog_use.py",
                &CATALOG_QUERY_USE.replace("cache.db", ":memory:"),
            )],
        )
        .absent(&[
            ("pkg/catalog_use.py", "EFF101"),
            ("pkg/catalog_use.py", "EFF101 [heuristic]"),
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
        .present(&[("pkg/buf_default_use.py", "EFF101 [heuristic]")])
        .absent(&[("pkg/buf_use.py", "EFF101 [heuristic]")]),
        Step::new(
            "the reader tests an attribute a buffer may lack",
            vec![save(
                "pkg/buf_read.py",
                &BUF_READ_HASATTR.replace("\"read\")", "\"name\")"),
            )],
        )
        .present(&[("pkg/buf_use.py", "EFF101 [heuristic]")]),
        Step::new(
            "the reader rebinds a missing argument to a buffer before its `isinstance` test",
            vec![save("pkg/buf_read.py", BUF_READ_REBINDING)],
        )
        .absent(&[
            ("pkg/buf_use.py", "EFF101 [heuristic]"),
            ("pkg/buf_default_use.py", "EFF101 [heuristic]"),
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
    ]
}

/// Contracts: the reports of private helpers move between them and their callers.
fn steps_contracts() -> Vec<Step> {
    vec![
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
    ]
}

// The allowed functions the help of a contract report names, in other files.
const STORE_WRITING: &str = "from pkg.model import Registry, Vault\n\n\nclass Store:\n    def put(self, r: Registry, x):\n        r.entries.append(x)\n";
const STORE_WRITING_MOVED: &str = "from pkg.model import Registry, Vault\n\n\n# The store.\nclass Store:\n    def put(self, r: Registry, x):\n        r.entries.append(x)\n";
const STORE_MORE: &str = "from pkg.ledger import Ledger\nfrom pkg.model import Registry, Vault\n\n\nclass Store:\n    def add(self, r: Registry):\n        r.entries.append(1)\n\n    def look(self, v: Vault):\n        return v.secret\n\n    def pay(self, l: Ledger):\n        return l.post(1)\n";
// A subclass of the call target `Receipt`, constructed outside the allowed callers, and the
// allowed `Store` starting to construct it.
const SLIPS: &str = "from pkg.ledger import Receipt\n\n\nclass Slip(Receipt):\n    pass\n\n\ndef issue():\n    return Slip(1)\n";
const STORE_ISSUING: &str = "from pkg.model import Registry, Vault\nfrom pkg.slips import Slip\n\n\nclass Store:\n    def put(self, r: Registry, x):\n        return x\n\n    def issue(self):\n        return Slip(2)\n";
const STORE_NOT_ISSUING: &str = "from pkg.model import Registry, Vault\nfrom pkg.slips import Slip\n\n\nclass Store:\n    def put(self, r: Registry, x):\n        return x\n\n    def issue(self):\n        return 2 if Slip else 3\n";
// A write no path announces, whose help names `pkg.grow.grow`: its announcement comes from
// `pkg.announce`.
const CARELESS: &str =
    "from pkg.nodes import Node\n\n\ndef careless(node: Node, kid):\n    node.kids.append(kid)\n";
// Owner targets named by private members (`Purse.__count`), which the class's own methods write
// and read under the mangled names (`_Purse__count`).
const PURSE_CONTRACTS: &str = "\n[[tool.heki.write-owners]]\nname = \"purse\"\ntargets = [\"Purse.__count\"]\nallowed-writers = [\"Purse.*\"]\n\n[[tool.heki.read-owners]]\nname = \"purse-seal\"\ntargets = [\"Purse.__seal\"]\nallowed-readers = [\"Purse.*\"]\n";
const PURSE: &str = "class Purse:\n    def __init__(self):\n        self.__count = []\n        self.__seal = 0\n\n    def add(self, x):\n        return x\n\n    def seal(self):\n        return 0\n";
const PURSE_WRITING: &str = "class Purse:\n    def __init__(self):\n        self.__count = []\n        self.__seal = 0\n\n    def add(self, x):\n        self.__count.append(x)\n\n    def seal(self):\n        return 0\n";
const PURSE_READING: &str = "class Purse:\n    def __init__(self):\n        self.__count = []\n        self.__seal = 0\n\n    def add(self, x):\n        return x\n\n    def seal(self):\n        print(self.__seal)\n        return 0\n";
const PURSE_USE: &str = "from pkg.purse import Purse\n\n\ndef poke(t: Purse):\n    t._Purse__count.append(1)\n\n\ndef peek(t: Purse):\n    return t._Purse__seal\n";

/// The help of a contract report names the allowed functions of other files that do what it
/// shows: a function starts and stops writing the target, a module with another one comes and
/// goes, and a line added above one changes nothing the help names.
fn steps_allowed_examples() -> Vec<Step> {
    vec![
        Step::new(
            "an allowed writer starts writing the target itself",
            vec![save("pkg/store.py", STORE_WRITING)],
        )
        .present(&[("pkg/ops.py", "MUT201")]),
        Step::new(
            "a comment above the allowed writer",
            vec![save("pkg/store.py", STORE_WRITING_MOVED)],
        )
        .present(&[("pkg/ops.py", "MUT201")])
        .cutoff(),
        Step::new(
            "a new module defines another allowed writer, reader, and caller",
            vec![Action::Create("pkg/store_more.py", STORE_MORE.to_owned())],
        )
        .present(&[("pkg/ops.py", "MUT201")]),
        Step::new(
            "the new module is deleted",
            vec![Action::Delete("pkg/store_more.py")],
        )
        .present(&[("pkg/ops.py", "MUT201")]),
        Step::new(
            "the allowed writer stops writing the target",
            vec![save("pkg/store.py", STORE)],
        )
        .present(&[("pkg/ops.py", "MUT201")]),
        Step::new(
            "a module constructs a subclass of a call target outside the allowed callers",
            vec![Action::Create("pkg/slips.py", SLIPS.to_owned())],
        )
        .present(&[("pkg/slips.py", "MUT204")]),
        Step::new(
            "an allowed caller that reads the subclass",
            vec![save("pkg/store.py", STORE_NOT_ISSUING)],
        )
        .present(&[("pkg/slips.py", "MUT204")]),
        Step::new(
            "the allowed caller starts constructing the subclass",
            vec![save("pkg/store.py", STORE_ISSUING)],
        )
        .present(&[("pkg/slips.py", "MUT204")]),
        Step::new(
            "the allowed caller stops constructing it",
            vec![save("pkg/store.py", STORE_NOT_ISSUING)],
        )
        .present(&[("pkg/slips.py", "MUT204")]),
        Step::new(
            "a module writes an invalidation target no path announces",
            vec![Action::Create("pkg/careless.py", CARELESS.to_owned())],
        )
        .present(&[("pkg/careless.py", "MUT208")]),
        Step::new(
            "the helper announcing an allowed writer's change in another module stops",
            vec![save("pkg/announce.py", ANNOUNCE_SILENT)],
        )
        .present(&[("pkg/careless.py", "MUT208"), ("pkg/grow.py", "MUT208")]),
        Step::new(
            "the helper announces the change again",
            vec![save("pkg/announce.py", ANNOUNCE)],
        )
        .present(&[("pkg/careless.py", "MUT208")])
        .absent(&[("pkg/grow.py", "MUT208")]),
        Step::new(
            "a module writes and reads private owner targets of a class in another module",
            vec![
                Action::Configure(format!("{CONFIG}{PURSE_CONTRACTS}")),
                Action::Create("pkg/purse.py", PURSE.to_owned()),
                Action::Create("pkg/purse_use.py", PURSE_USE.to_owned()),
            ],
        )
        .present(&[
            ("pkg/purse_use.py", "MUT201"),
            ("pkg/purse_use.py", "MUT203"),
        ]),
        Step::new(
            "a method of the class starts writing its private target",
            vec![save("pkg/purse.py", PURSE_WRITING)],
        )
        .present(&[
            ("pkg/purse_use.py", "MUT201"),
            ("pkg/purse_use.py", "MUT203"),
        ]),
        Step::new(
            "it stops, and another starts reading the other private target",
            vec![save("pkg/purse.py", PURSE_READING)],
        )
        .present(&[
            ("pkg/purse_use.py", "MUT201"),
            ("pkg/purse_use.py", "MUT203"),
        ]),
        Step::new(
            "the methods neither write nor read them",
            vec![save("pkg/purse.py", PURSE)],
        )
        .present(&[
            ("pkg/purse_use.py", "MUT201"),
            ("pkg/purse_use.py", "MUT203"),
        ]),
    ]
}

// `include-overrides`: a write owner and an invalidation contract whose method entries name
// the overrides in the subclasses of their classes. The classes are in one module, a middle
// class in a second, the subclasses in a third, and a private helper in a fourth.
const OVERRIDE_CONTRACTS: &str = "\n[[tool.heki.write-owners]]\nname = \"shelf\"\ntargets = [\"Shelf.items\"]\nallowed-writers = [\"pkg.ov_base:Keeper.store\"]\ninclude-overrides = true\n\n[[tool.heki.invalidation]]\nname = \"catalog\"\ntargets = [\"Leaf.kids\"]\ninvalidators = [\"Catalog.refresh\"]\ninclude-overrides = true\n";
const OV_BASE: &str = "class Shelf:\n    def __init__(self) -> None:\n        self.items: list[int] = []\n\n\nclass Keeper:\n    def store(self, shelf: Shelf) -> None:\n        shelf.items.append(1)\n\n\nclass Leaf:\n    def __init__(self) -> None:\n        self.kids: list[int] = []\n\n\nclass Catalog:\n    def refresh(self) -> None:\n        pass\n";
const OV_MID: &str = "from pkg.ov_base import Catalog, Keeper\n\n\nclass Mid(Keeper):\n    pass\n\n\nclass MidCatalog(Catalog):\n    pass\n";
const OV_MID_BASELESS: &str = "from pkg.ov_base import Catalog, Keeper\n\n\nclass Mid:\n    pass\n\n\nclass MidCatalog:\n    pass\n";
const OV_HELP: &str = "from pkg.ov_base import Shelf\n\n\ndef _fill(shelf: Shelf) -> None:\n    shelf.items.append(2)\n";
const OV_SUB_WRITING: &str = "from pkg.ov_base import Shelf\nfrom pkg.ov_mid import Mid\n\n\nclass Sub(Mid):\n    def store(self, shelf: Shelf) -> None:\n        shelf.items.append(3)\n";
const OV_SUB_HELPED: &str = "from pkg.ov_base import Shelf\nfrom pkg.ov_help import _fill\nfrom pkg.ov_mid import Mid\n\n\nclass Sub(Mid):\n    def store(self, shelf: Shelf) -> None:\n        _fill(shelf)\n";
const OV_GROW: &str = "from pkg.ov_base import Leaf\nfrom pkg.ov_mid import MidCatalog\n\n\nclass FastCatalog(MidCatalog):\n    def refresh(self) -> None:\n        pass\n\n\ndef grow(leaf: Leaf, catalog: FastCatalog) -> None:\n    leaf.kids.append(1)\n    catalog.refresh()\n";

/// `include-overrides`: an override added in a new subclass, a helper that runs only inside an
/// override, a base removed from a class above the override in another module, and an
/// invalidation contract whose invalidator's override announces a write.
fn steps_include_overrides() -> Vec<Step> {
    vec![
        Step::new(
            "contracts with the key, and a private helper no function calls",
            vec![
                Action::Configure(format!("{CONFIG}{OVERRIDE_CONTRACTS}")),
                Action::Create("pkg/ov_base.py", OV_BASE.to_owned()),
                Action::Create("pkg/ov_mid.py", OV_MID.to_owned()),
                Action::Create("pkg/ov_help.py", OV_HELP.to_owned()),
            ],
        )
        .present(&[("pkg/ov_help.py", "MUT201")]),
        Step::new(
            "a new subclass in another module overrides the allowed writer",
            vec![Action::Create("pkg/ov_sub.py", OV_SUB_WRITING.to_owned())],
        )
        .absent(&[("pkg/ov_sub.py", "MUT201")]),
        Step::new(
            "the override hands its write to the private helper",
            vec![save("pkg/ov_sub.py", OV_SUB_HELPED)],
        )
        .absent(&[("pkg/ov_help.py", "MUT201"), ("pkg/ov_sub.py", "MUT201")]),
        Step::new(
            "the class between the override and the allowed writer drops its base",
            vec![save("pkg/ov_mid.py", OV_MID_BASELESS)],
        )
        .present(&[("pkg/ov_sub.py", "MUT201")]),
        Step::new("the base is back", vec![save("pkg/ov_mid.py", OV_MID)])
            .absent(&[("pkg/ov_help.py", "MUT201"), ("pkg/ov_sub.py", "MUT201")]),
        Step::new(
            "a write announced by an override of the invalidator",
            vec![Action::Create("pkg/ov_grow.py", OV_GROW.to_owned())],
        )
        .absent(&[("pkg/ov_grow.py", "MUT208")]),
        Step::new(
            "the class between the override and the invalidator drops its base",
            vec![save("pkg/ov_mid.py", OV_MID_BASELESS)],
        )
        .present(&[("pkg/ov_grow.py", "MUT208"), ("pkg/ov_sub.py", "MUT201")]),
        Step::new(
            "the base is back again",
            vec![save("pkg/ov_mid.py", OV_MID)],
        )
        .absent(&[("pkg/ov_grow.py", "MUT208"), ("pkg/ov_sub.py", "MUT201")]),
        Step::new(
            "the override is removed",
            vec![Action::Delete("pkg/ov_sub.py")],
        )
        .present(&[("pkg/ov_help.py", "MUT201")]),
    ]
}

/// Files: a module created and deleted, a stub next to a module, and a package over the
/// module of the same name.
fn steps_files() -> Vec<Step> {
    vec![
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
            vec![Action::Create(
                "pkg/svc/__init__.py",
                SVC_PACKAGE.to_owned(),
            )],
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
    ]
}

/// The configuration changes while the server runs.
fn steps_configuration() -> Vec<Step> {
    vec![
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
                "[tool.heki.lint]\n",
                "[tool.heki.lint]\nselect = [\"NOPE1\"]\n",
            ))],
        )
        .absent(&[("pkg/svc_user.py", "EFF101")]),
        Step::new(
            "the configuration is valid again",
            vec![Action::Configure(CONFIG.to_owned())],
        )
        .present(&[("pkg/svc_user.py", "EFF101")]),
        // The oracle compares the severity: the server republishes the report as a warning.
        Step::new(
            "the configuration makes the blocking-call rule a warning",
            vec![Action::Configure(format!(
                "{CONFIG}\n[tool.heki.lint.severity]\nEFF101 = \"warning\"\n"
            ))],
        )
        .present(&[("pkg/svc_user.py", "EFF101")]),
        Step::new(
            "the rule is an error again",
            vec![Action::Configure(CONFIG.to_owned())],
        )
        .present(&[("pkg/svc_user.py", "EFF101")]),
        // The places of the contracts are compared too: each step moves the ones a report shows.
        Step::new(
            "a comment above the contracts moves the place of each",
            vec![Action::Configure(format!("# contracts\n\n{CONFIG}"))],
        )
        .present(&[("pkg/svc_user.py", "EFF101")]),
        Step::new(
            "a write owner in an included file",
            vec![
                Action::Write("contracts/boxes.toml", BOXES.to_owned()),
                Action::Configure(with_boxes()),
            ],
        )
        .present(&[("pkg/shapes.py", "MUT201")]),
        Step::new(
            "the included file's table moves down",
            vec![Action::Write(
                "contracts/boxes.toml",
                format!("# boxes\n\n{BOXES}"),
            )],
        )
        .present(&[("pkg/shapes.py", "MUT201")]),
        Step::new(
            "the included table names where it is defined",
            vec![Action::Write(
                "contracts/boxes.toml",
                format!("{BOXES}source = \"gen/boxes.py:7\"\nentry-sources = {{ \"Box.items\" = \"gen/boxes.py:9\" }}\n"),
            )],
        )
        .present(&[("pkg/shapes.py", "MUT201")]),
        // A report whose access matches an entry by its pattern points at that entry.
        Step::new(
            "contracts of an included file match by the globs of a group",
            vec![
                Action::Write("contracts/dials.toml", DIAL_CONTRACTS.to_owned()),
                Action::Configure(with_included("contracts/dials.toml")),
            ],
        )
        .present(DIAL_CONTRACT_REPORTS),
        Step::new(
            "the group moves down",
            vec![Action::Write(
                "contracts/dials.toml",
                format!("# dials\n\n{DIAL_CONTRACTS}"),
            )],
        )
        .present(DIAL_CONTRACT_REPORTS),
        Step::new(
            "the included file is gone from the configuration",
            vec![Action::Configure(CONFIG.to_owned())],
        )
        .absent(&[("pkg/shapes.py", "MUT201")]),
    ]
}

/// An effect boundary, an invalidation contract, and an import boundary of an included file
/// whose reports match their entries by globs ([`DIAL_CONTRACT_REPORTS`]), two of them from a
/// group.
const DIAL_CONTRACTS: &str = "\
[groups]
levels = [\"Gauge.lev*\"]

[[effect-boundaries]]
name = \"turning\"
scopes = [\"pkg.dial_user.turn\"]
write-universe = [\"@levels\"]

[[invalidation]]
name = \"gauges\"
targets = [\"Node.kids\", \"@levels\"]
invalidators = [\"pkg.dials:recalibrate\"]

[[import-boundaries]]
name = \"dial-layers\"
importers = [\"pkg.dial_user\"]
forbidden-imports = [\"pkg.imp_db\", \"pkg.dial_o*\"]
";

const DIAL_CONTRACT_REPORTS: &[(&str, &str)] = &[
    ("pkg/dial_user.py", "EFF201"),
    ("pkg/dial_user.py", "MUT208"),
    ("pkg/dial_user.py", "IMP201"),
];

/// [`CONFIG`] including `file`.
fn with_included(file: &str) -> String {
    CONFIG.replace(
        "[tool.heki.lint]\n",
        &format!("[tool.heki]\ninclude = [\"{file}\"]\n\n[tool.heki.lint]\n"),
    )
}

/// A write owner of an included file ([`with_boxes`]).
const BOXES: &str = "[[write-owners]]\nname = \"boxes\"\ntargets = [\"Box.items\"]\n";

/// [`CONFIG`] including `contracts/boxes.toml`.
fn with_boxes() -> String {
    with_included("contracts/boxes.toml")
}

const MEMBER_PYPROJECT: &str = "[project]\nname = \"member\"\n";
const MEMBER_HELPER: &str = "import time\n\n\ndef wait():\n    time.sleep(1)\n";
const MEMBER_CALLER: &str = "from acme_core.io import wait\n\n\nasync def go():\n    wait()\n";
const MEMBER_CALL: &[(&str, &str)] = &[("packages/api/acme_api/server.py", "EFF101")];

/// The members of a workspace are source roots: the workspace tables, a member's
/// `pyproject.toml`, and a `hatch.toml` change while the server runs.
fn steps_workspace_members() -> Vec<Step> {
    let uv = format!("{CONFIG}\n[tool.uv.workspace]\nmembers = [\"packages/*\"]\n");
    vec![
        Step::new(
            "two directories that are not yet members",
            vec![
                Action::Write("packages/core/pyproject.toml", MEMBER_PYPROJECT.to_owned()),
                Action::Create(
                    "packages/core/src/acme_core/io.py",
                    MEMBER_HELPER.to_owned(),
                ),
                Action::Write("packages/api/pyproject.toml", MEMBER_PYPROJECT.to_owned()),
                Action::Create("packages/api/acme_api/__init__.py", String::new()),
                Action::Create("packages/api/acme_api/server.py", MEMBER_CALLER.to_owned()),
            ],
        )
        .absent(MEMBER_CALL),
        Step::new(
            "the configuration makes them workspace members",
            vec![Action::Configure(uv.clone())],
        )
        .present(MEMBER_CALL),
        Step::new(
            "a member loses its pyproject.toml",
            vec![Action::Delete("packages/core/pyproject.toml")],
        )
        .absent(MEMBER_CALL),
        Step::new(
            "the member's pyproject.toml is back",
            vec![Action::Write(
                "packages/core/pyproject.toml",
                MEMBER_PYPROJECT.to_owned(),
            )],
        )
        .present(MEMBER_CALL),
        Step::new(
            "an exclude glob drops the member",
            vec![Action::Configure(format!(
                "{uv}exclude = [\"packages/core\"]\n"
            ))],
        )
        .absent(MEMBER_CALL),
        Step::new(
            "a hatch.toml lists the members",
            vec![Action::Write(
                "hatch.toml",
                "[envs.default]\nworkspace.members = [\"packages/core\"]\n".to_owned(),
            )],
        )
        .present(MEMBER_CALL),
        Step::new(
            "the hatch.toml is deleted",
            vec![Action::Delete("hatch.toml")],
        )
        .absent(MEMBER_CALL),
    ]
}

/// Project-wide assumptions: a module patching builtins, and a typing stub importing them.
fn steps_project_assumptions() -> Vec<Step> {
    vec![
        Step::new(
            "the module patching builtins is deleted",
            vec![Action::Delete("pkg/compat.py")],
        )
        .present(&[("pkg/loops.py", "OPT110")]),
        // The size-dependent rewrites are opt-in: without their codes the report goes.
        Step::new(
            "the configuration stops naming the size-dependent rules",
            vec![Action::Configure(CONFIG.replace(
                ", \"OPT103\", \"OPT110\", \"OPT111\", \"OPT120\", \"OPT121\", \"OPT130\"",
                "",
            ))],
        )
        .absent(&[("pkg/loops.py", "OPT110")]),
        Step::new(
            "the configuration names them again",
            vec![Action::Configure(CONFIG.to_owned())],
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
    ]
}

/// Declarations other modules resolve names through.
fn steps_declarations() -> Vec<Step> {
    vec![
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
        .present(&[("pkg/draw.py", "EFF101 [heuristic]")]),
        Step::new(
            "the override is removed",
            vec![save("pkg/square.py", SQUARE)],
        )
        .absent(&[("pkg/draw.py", "EFF101 [heuristic]")]),
    ]
}

/// Declarations reached through a re-export.
fn steps_re_exports() -> Vec<Step> {
    vec![
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
    ]
}

const PARSER_MODULE: &str = "import time\n\n\ndef parse(x):\n    time.sleep(1)\n    return x\n";
const PARSER_USER: &str =
    "from pkg.parser import parse\n\n\nasync def handle(x):\n    return parse(x)\n";

/// `from pkg.parser import parse` takes the submodule `pkg.parser.parse` until the package's
/// `__init__.py` binds `parse` through it, and the function from then on.
fn steps_package_names() -> Vec<Step> {
    vec![
        Step::new(
            "a package has a module named like the module's function",
            vec![
                Action::Create("pkg/parser/__init__.py", String::new()),
                Action::Create("pkg/parser/parse.py", PARSER_MODULE.to_owned()),
                Action::Create("pkg/parser_user.py", PARSER_USER.to_owned()),
            ],
        )
        .absent(&[("pkg/parser_user.py", "EFF101")]),
        Step::new(
            "the package re-exports the function over the module",
            vec![save("pkg/parser/__init__.py", "from .parse import parse\n")],
        )
        .present(&[("pkg/parser_user.py", "EFF101")]),
        Step::new(
            "the package binds the name to the module's function",
            vec![save(
                "pkg/parser/__init__.py",
                "from . import parse\n\nparse = parse.parse\n",
            )],
        )
        .present(&[("pkg/parser_user.py", "EFF101")]),
        Step::new(
            "the package binds the name to the module",
            vec![save("pkg/parser/__init__.py", "from . import parse\n")],
        )
        .absent(&[("pkg/parser_user.py", "EFF101")]),
    ]
}

/// `import-boundaries`: an import added to the importer, a module created that turns a `from`
/// import of a name into an import of the submodule, with `reach`, an import added to, moved
/// in, and removed from a module the importer imports, and a relative import rewritten with the
/// package's full name and back.
fn steps_import_boundaries() -> Vec<Step> {
    vec![
        Step::new(
            "a module imports nothing forbidden",
            vec![
                Action::Create("pkg/imp_db.py", String::new()),
                Action::Create("pkg/imp_pkg/__init__.py", String::new()),
                Action::Create(
                    "pkg/imp_user.py",
                    "import json\nfrom pkg.imp_pkg import sub\n".to_owned(),
                ),
            ],
        )
        .absent(&[("pkg/imp_user.py", "IMP201")]),
        Step::new(
            "the package gets the submodule the from-import names",
            vec![Action::Create("pkg/imp_pkg/sub.py", String::new())],
        )
        .present(&[("pkg/imp_user.py", "IMP201")]),
        Step::new(
            "the submodule is gone again",
            vec![Action::Delete("pkg/imp_pkg/sub.py")],
        )
        .absent(&[("pkg/imp_user.py", "IMP201")]),
        Step::new(
            "the module imports a forbidden module",
            vec![save("pkg/imp_user.py", "import json\nimport pkg.imp_db\n")],
        )
        .present(&[("pkg/imp_user.py", "IMP201")]),
        Step::new(
            "a start-up module imports a light one",
            vec![
                Action::Create("pkg/imp_heavy.py", String::new()),
                Action::Create("pkg/imp_mid.py", String::new()),
                Action::Create("pkg/imp_cli.py", "import pkg.imp_mid\n".to_owned()),
            ],
        )
        .absent(&[("pkg/imp_cli.py", "IMP201")]),
        Step::new(
            "the light module imports the heavy one",
            vec![save("pkg/imp_mid.py", "import pkg.imp_heavy\n")],
        )
        .present(&[("pkg/imp_cli.py", "IMP201")]),
        Step::new(
            "the import moves down a line",
            vec![save("pkg/imp_mid.py", "\nimport pkg.imp_heavy\n")],
        )
        .present(&[("pkg/imp_cli.py", "IMP201")]),
        Step::new(
            "the light module imports it only when a function runs",
            vec![save(
                "pkg/imp_mid.py",
                "def load():\n    import pkg.imp_heavy\n",
            )],
        )
        .absent(&[("pkg/imp_cli.py", "IMP201")]),
        Step::new(
            "a copied package's module imports another relatively",
            vec![
                Action::Create("pkg/imp_rt/__init__.py", String::new()),
                Action::Create("pkg/imp_rt/b.py", String::new()),
                Action::Create("pkg/imp_rt/a.py", "from . import b\n".to_owned()),
            ],
        )
        .absent(&[("pkg/imp_rt/a.py", "IMP201")]),
        Step::new(
            "the import names the package by its full name",
            vec![save("pkg/imp_rt/a.py", "from pkg.imp_rt import b\n")],
        )
        .present(&[("pkg/imp_rt/a.py", "IMP201")]),
        Step::new(
            "the import is relative again",
            vec![save("pkg/imp_rt/a.py", "from . import b\n")],
        )
        .absent(&[("pkg/imp_rt/a.py", "IMP201")]),
        Step::new(
            "a module imports a public name of a module that forbids its private names",
            vec![
                Action::Create(
                    "pkg/imp_privmod.py",
                    "def _hidden():\n    pass\n\n\ndef shown():\n    pass\n".to_owned(),
                ),
                Action::Create(
                    "pkg/imp_priv.py",
                    "from pkg.imp_privmod import shown\n".to_owned(),
                ),
            ],
        )
        .absent(&[("pkg/imp_priv.py", "IMP201")]),
        Step::new(
            "the import takes a private name",
            vec![save(
                "pkg/imp_priv.py",
                "from pkg.imp_privmod import _hidden\n",
            )],
        )
        .present(&[("pkg/imp_priv.py", "IMP201")]),
        Step::new(
            "a method of an object of unknown class may import a module by name",
            vec![save(
                "pkg/imp_priv.py",
                "def load(loader):\n    return loader.import_module(\"pkg.imp_privmod\")\n",
            )],
        )
        .present(&[("pkg/imp_priv.py", "IMP202")])
        .absent(&[("pkg/imp_priv.py", "IMP201")]),
        Step::new(
            "a relative import climbs past the top-level package",
            vec![save("pkg/imp_priv.py", "from .. import outside\n")],
        )
        .present(&[("pkg/imp_priv.py", "IMP202")]),
        Step::new(
            "the import stays inside the package",
            vec![save("pkg/imp_priv.py", "from . import imp_privmod\n")],
        )
        .absent(&[("pkg/imp_priv.py", "IMP202")]),
    ]
}

const STAR_IMPL: &str = "import time\n\n__all__ = [\"fetch\"]\n\n\ndef fetch(url):\n    time.sleep(1)\n    return url\n";
const STAR_USER: &str =
    "from pkg.star import fetch\n\n\nasync def handle(url):\n    return fetch(url)\n";

/// `from pkg.star import fetch` where `pkg/star/__init__.py` re-exports with a star import: the
/// function while the module the import names, or one it star-imports in turn, exports it.
fn steps_star_imports() -> Vec<Step> {
    let unlisted = STAR_IMPL.replace("__all__ = [\"fetch\"]\n", "");
    vec![
        Step::new(
            "a package re-exports a module's `__all__` with a star import",
            vec![
                Action::Create("pkg/star/__init__.py", "from ._impl import *\n".to_owned()),
                Action::Create("pkg/star/_impl.py", STAR_IMPL.to_owned()),
                Action::Create("pkg/star_user.py", STAR_USER.to_owned()),
            ],
        )
        .present(&[("pkg/star_user.py", "EFF101")]),
        Step::new(
            "the module's `__all__` no longer lists the function",
            vec![save(
                "pkg/star/_impl.py",
                &STAR_IMPL.replace("[\"fetch\"]", "[]"),
            )],
        )
        .absent(&[("pkg/star_user.py", "EFF101")]),
        Step::new(
            "without `__all__`, the module's public names are re-exported",
            vec![save("pkg/star/_impl.py", &unlisted)],
        )
        .present(&[("pkg/star_user.py", "EFF101")]),
        Step::new(
            "the package star-imports another module, which star-imports the first",
            vec![
                Action::Create("pkg/star/_api.py", "from ._impl import *\n".to_owned()),
                save("pkg/star/__init__.py", "from ._api import *\n"),
            ],
        )
        .present(&[("pkg/star_user.py", "EFF101")]),
        Step::new(
            "the first module makes the function private",
            vec![save(
                "pkg/star/_impl.py",
                &unlisted.replace("def fetch", "def _fetch"),
            )],
        )
        .absent(&[("pkg/star_user.py", "EFF101")]),
        Step::new(
            "the module the package star-imports defines the function itself",
            vec![save(
                "pkg/star/_api.py",
                "import time\n\nfrom ._impl import *\n\n\ndef fetch(url):\n    time.sleep(1)\n    return url\n",
            )],
        )
        .present(&[("pkg/star_user.py", "EFF101")]),
        Step::new(
            "the package no longer star-imports",
            vec![save("pkg/star/__init__.py", "")],
        )
        .absent(&[("pkg/star_user.py", "EFF101")]),
    ]
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

impl Diag {
    /// The code with its tier, as a step names it ([`common::with_tier`]).
    fn tiered_code(&self) -> &str {
        common::with_tier(&self.code, &self.certainty)
    }
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

/// A place given by its 1-based line and byte column -> the empty LSP range there.
fn cli_point(p: &Project, file: &str, line: u64, column: u64) -> Range {
    let text = std::fs::read_to_string(p.dir.join(file)).unwrap_or_default();
    let shown = text.lines().nth(usize::try_from(line - 1).unwrap());
    let before = |l: &str| {
        l.get(..usize::try_from(column - 1).unwrap())
            .map(str::to_owned)
    };
    let character = shown
        .and_then(before)
        .map_or(column - 1, |b| b.encode_utf16().count() as u64);
    let at = (line - 1, character);
    (file.to_owned(), at, at)
}

/// `heki check --output-format json` of the project, by file.
fn cli(p: &Project) -> BTreeMap<String, Vec<Diag>> {
    let (code, out, err) = p.check(&["--output-format", "json"]);
    assert!(matches!(code, 0..=2), "exit {code}: {err}");
    let out: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}:\n{out}\n{err}"));
    // An invalid configuration: nothing is checked (exit 2, the error object, no report).
    let diags: Vec<Value> = if code == 2 && out.get("error").is_some() {
        Vec::new()
    } else {
        serde_json::from_value(out).unwrap()
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
            .chain(
                d["contract_locations"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|c| {
                        let at = |key: &str| c[key].as_u64().unwrap();
                        let file = c["path"].as_str().unwrap();
                        (
                            cli_point(p, file, at("line"), at("column")),
                            c["message"].as_str().unwrap().to_owned(),
                        )
                    }),
            )
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

    /// The second session's project, with its effect snapshot (`MUT103`).
    fn start_layout(name: &str) -> Self {
        Self::start_with(name, EDGE_CONFIG, edge_files(), true)
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
                    .is_some_and(|d| d.iter().any(|x| x.tiered_code() == *code)),
                "step `{}`: the CLI does not report {code} in {file}: {reference:#?}",
                step.name
            );
        }
        for (file, code) in step.absent {
            assert!(
                !reference
                    .get(*file)
                    .is_some_and(|d| d.iter().any(|x| x.tiered_code() == *code)),
                "step `{}`: the CLI reports {code} in {file}: {reference:#?}",
                step.name
            );
        }
        if let Some((file, expected)) = step.reported {
            let mut found: Vec<(u64, &str)> = (reference.get(file).into_iter().flatten())
                .map(|d| (d.range.1.0 + 1, d.tiered_code()))
                .collect();
            found.sort_unstable();
            let mut expected = expected.to_vec();
            expected.sort_unstable();
            assert_eq!(
                found, expected,
                "step `{}`: the CLI's diagnostics in {file}",
                step.name
            );
        }
        let mut out: Vec<String> = differences(&published(&self.lsp, &self.root), &reference)
            .into_iter()
            .map(|d| format!("step `{}`: {d}", step.name))
            .collect();
        if let Some((function, param, expected)) = step.reads {
            let (code, json, err) = self
                .p
                .heki(&["inspect", function, "--output-format", "json"]);
            assert_eq!(code, 0, "step `{}`: {err}", step.name);
            let report: Value = serde_json::from_str(&json).unwrap();
            let kinds: Vec<&str> = (report["only_reads"]["reads"][param].as_array())
                .map_or(Vec::new(), |k| {
                    k.iter().map(|k| k.as_str().unwrap()).collect()
                });
            if kinds != expected {
                out.push(format!(
                    "step `{}`: `heki inspect {function}` reads {param} as {kinds:?}, not {expected:?}",
                    step.name
                ));
            }
        }
        if let Some((function, expected)) = step.registered {
            let (code, json, err) = self
                .p
                .heki(&["inspect", function, "--output-format", "json"]);
            assert_eq!(code, 0, "step `{}`: {err}", step.name);
            let report: Value = serde_json::from_str(&json).unwrap();
            let text = |v: &Value| v.as_str().unwrap().to_owned();
            let mut found: Vec<(String, Vec<String>)> = (report["registered"].as_array().unwrap())
                .iter()
                .map(|c| {
                    // One entry per change record: a context that changes nothing has none.
                    let changes = c["changes"].as_array().unwrap().iter();
                    let records = changes.map(|ch| {
                        let slots: Vec<String> =
                            ch["slots"].as_array().unwrap().iter().map(text).collect();
                        slots.join(", ")
                    });
                    (text(&c["type"]), records.collect())
                })
                .collect();
            found.sort();
            let want: Vec<(String, Vec<String>)> = (expected.iter())
                .map(|(t, slots)| {
                    (
                        (*t).to_owned(),
                        slots.iter().map(|s| (*s).to_owned()).collect(),
                    )
                })
                .collect();
            if found != want {
                out.push(format!(
                    "step `{}`: `heki inspect {function}` gives the contexts {found:?}, not {want:?}",
                    step.name
                ));
            }
        }
        if let Some((function, expected)) = step.only_reads {
            let (code, json, err) = self
                .p
                .heki(&["inspect", function, "--output-format", "json"]);
            assert_eq!(code, 0, "step `{}`: {err}", step.name);
            let report: Value = serde_json::from_str(&json).unwrap();
            let params: Vec<&str> = report["only_reads"]["params"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p.as_str().unwrap())
                .collect();
            if params != expected {
                out.push(format!(
                    "step `{}`: `heki inspect {function}` only reads {params:?}, not {expected:?}",
                    step.name
                ));
            }
        }
        if step.propagates && after["dependent_reanalyses"] == before["dependent_reanalyses"] {
            out.push(format!(
                "step `{}`: no caller's file was re-analyzed: stats before {before}, after {after}",
                step.name
            ));
        }
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

/// Runs `steps` in `s`, then fails with the disagreements of every step.
fn run_steps(mut s: Session, steps: &[Step]) {
    let mut failures = Vec::new();
    for step in steps {
        failures.extend(s.run(step));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    s.lsp.stop(true);
}

/// A module of tests, one per group of steps: each test runs its group in a session of its
/// own, which `start` opens over the initial files. The groups are independent, so the tests
/// run in parallel and a failure names its group.
macro_rules! sessions {
    ($module:ident, $start:path, { $($test:ident: $steps:ident,)* }) => {
        mod $module {
            use super::*;
            $(
                #[test]
                fn $test() {
                    let name = concat!("lsp_oracle_", stringify!($module), "_", stringify!($test));
                    run_steps($start(name), &$steps());
                }
            )*
        }
    };
}

sessions!(editing, Session::start, {
    initial_index: steps_initial_index,
    blocking_chain: steps_blocking_chain,
    property_getters: steps_property_getters,
    kept_values: steps_kept_values,
    guarded_effects: steps_guarded_effects,
    mut108: steps_mut108,
    mut110: steps_mut110,
    only_read: steps_only_read,
    registrations: steps_registrations,
    mut120: steps_mut120,
    aliases_and_getters: steps_aliases_and_getters,
    protocol_reads: steps_protocol_reads,
    body_runs: steps_body_runs,
    task_arguments: steps_task_arguments,
    small_displays: steps_small_displays,
    queue_capacity: steps_queue_capacity,
    mut114_mut202_mut208: steps_mut114_mut202_mut208,
    readonly_contracts: steps_readonly_contracts,
    mut114_targets: steps_mut114_targets,
    mut115: steps_mut115,
    item_changes: steps_item_changes,
    module_patterns: steps_module_patterns,
    indirect_helpers: steps_indirect_helpers,
    receiver_changes: steps_receiver_changes,
    captured_parameters: steps_captured_parameters,
    inherited_writers: steps_inherited_writers,
    hash_writers: steps_hash_writers,
    mut111: steps_mut111,
    container_abcs: steps_container_abcs,
    mut118: steps_mut118,
    mut119: steps_mut119,
    mut112: steps_mut112,
    eff109: steps_eff109,
    mut113: steps_mut113,
    eff110: steps_eff110,
    eff112: steps_eff112,
    eff114: steps_eff114,
    eff115: steps_eff115,
    eff116: steps_eff116,
    eff117: steps_eff117,
    eff118: steps_eff118,
    attribute_types: steps_attribute_types,
    builtin_receivers: steps_builtin_receivers,
    receiver_typing: steps_receiver_typing,
    effect_index: steps_effect_index,
    exact_effects: steps_exact_effects,
    returned_rows: steps_returned_rows,
    returned_types: steps_returned_types,
    declared_types: steps_declared_types,
    held_functions: steps_held_functions,
    handed_callables: steps_handed_callables,
    store_types: steps_store_types,
    unused_suppressions: steps_unused_suppressions,
    helper_witnesses: steps_helper_witnesses,
    witness_ways: steps_witness_ways,
    imported_constants: steps_imported_constants,
    method_lookups: steps_method_lookups,
    exact_instances: steps_exact_instances,
    private_names: steps_private_names,
    contracts: steps_contracts,
    allowed_examples: steps_allowed_examples,
    contract_extensions: steps_contract_extensions,
    include_overrides: steps_include_overrides,
    references: steps_references,
    class_bodies: steps_class_bodies,
    bases_and_attribution: steps_bases_and_attribution,
    argument_lists: steps_argument_lists,
    callee_globs: steps_callee_globs,
    unbound_helpers: steps_unbound_helpers,
    module_members: steps_module_members,
    environment: steps_environment,
    outside_targets: steps_outside_targets,
    glob_members: steps_glob_members,
    keys_fresh_strict: steps_keys_fresh_strict,
    files: steps_files,
    configuration: steps_configuration,
    workspace_members: steps_workspace_members,
    project_assumptions: steps_project_assumptions,
    declarations: steps_declarations,
    re_exports: steps_re_exports,
    package_names: steps_package_names,
    star_imports: steps_star_imports,
    import_boundaries: steps_import_boundaries,
});

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

/// The initial index of the second session.
fn steps_layout_initial_index() -> Vec<Step> {
    vec![
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
    ]
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

sessions!(layout, Session::start_layout, {
    initial_index: steps_layout_initial_index,
    shared_module_name: steps_shared_module_name,
    unread_module: steps_unread_module,
    boundary_calls: steps_boundary_calls,
    recursive_snapshot: steps_recursive_snapshot,
    moved_lines: steps_moved_lines,
    discarded_buffer: steps_discarded_buffer,
    src_layout: steps_src_layout,
    source_roots: steps_source_roots,
    ignore_files: steps_ignore_files,
    forwarded_generator: steps_forwarded_generator,
    swapped_calls: steps_swapped_calls,
    baseline: steps_baseline,
});

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
    let s = Session::start_below_working_tree("lsp_oracle_working_tree", files);
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
    run_steps(s, &steps);
}

// ------------------------------------------------------------- a queue's extend argument

// `list.extend` asks its argument for its length and a `deque` does not, so `OPT120` offers its
// fix for a display and not for an instance of a class with `__len__`: the oracle compares the
// help, which carries that verdict. A project of its own: the main one patches `builtins`, which
// keeps the Soundness lane out of every module.
const QUEUE_CONFIG: &str =
    "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"OPT120\", \"OPT121\"]\n";

fn queue_drain(extended_by: &str) -> String {
    format!(
        "class Sized:\n    def __iter__(self):\n        return iter([1])\n\n    def __len__(self):\n        return 1\n\n\ndef drain(items):\n    q = list(items)\n    out = []\n    while q:\n        out.append(q.pop(0))\n        if not out[-1]:\n            q.extend({extended_by})\n    return out\n"
    )
}

#[test]
fn lsp_diagnostics_equal_a_full_check_as_a_queue_changes_its_extend_argument() {
    let files = vec![
        ("pkg/__init__.py", String::new()),
        ("pkg/drain_queue.py", queue_drain("[1, 2]")),
    ];
    let s = Session::start_with("lsp_oracle_queue_extend", QUEUE_CONFIG, files, false);
    let drained: &[(&str, &str)] = &[("pkg/drain_queue.py", "OPT120")];
    let steps = [
        Step::new("initial index", Vec::new()).present(drained),
        Step::new(
            "the queue is extended by an object whose class defines __len__",
            vec![save("pkg/drain_queue.py", &queue_drain("Sized()"))],
        )
        .present(drained)
        .cutoff(),
        Step::new(
            "the queue is extended by a list display again",
            vec![save("pkg/drain_queue.py", &queue_drain("[1, 2]"))],
        )
        .present(drained)
        .cutoff(),
    ];
    run_steps(s, &steps);
}

// ------------------------------------------------ calls through bound values across modules

// What a call through a value one module binds runs, decided from that module's declarations:
// a table of functions (`ModuleDecls::tables`, what other modules change in place in
// `ModuleDecls::stores_elsewhere`), a class attribute bound to a `staticmethod`
// (`ClassDecl::named_attrs`, a subclass binding the name to a method), a function returning a
// module it imports (`FunctionDecl::returned_import`), and the type a `TypeVar`'s bound or a
// type alias names (`ModuleDecls::type_names`), also through a module re-exporting the alias.
const BOUND_CONFIG: &str = "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"EFF201\"]\n\n[[tool.heki.effect-boundaries]]\nname = \"uses\"\nscopes = [\"pkg.use_*.*\"]\nwrite-universe = [\"Field.required\"]\nallow-writes = []\n";
const BOUND_MODEL: &str = "class Field:\n    def __init__(self):\n        self.required = False\n";
const BOUND_HELPERS: &str = "from pkg.model import Field\n\n\ndef require(field: Field):\n    field.required = True\n\n\ndef skip(field: Field):\n    pass\n";
const BOUND_DEFS: &str = "from typing import Protocol, TypeAlias, TypeVar\n\nfrom pkg.helpers import require, skip\nfrom pkg.model import Field\n\nTABLE = {\"run\": require, \"skip\": skip}\n\n\nclass Holder:\n    RUN = staticmethod(require)\n\n\ndef ops():\n    from pkg import helpers\n\n    return helpers\n\n\nclass View(Protocol):\n    @property\n    def fields(self) -> list[Field]: ...\n\n\nclass Other:\n    pass\n\n\nV = TypeVar(\"V\", bound=View)\n\n\nclass Writer:\n    def touch(self, field: Field):\n        field.required = True\n\n\nclass Reader:\n    def touch(self, field: Field):\n        pass\n\n\nTouching: TypeAlias = Writer | Reader\nTouchers = list[Writer]\n";
const BOUND_TABLE_USE: &str =
    "from pkg.defs import TABLE\n\n\ndef use(field):\n    TABLE[\"run\"](field)\n";
const BOUND_ALIAS_USE: &str =
    "from pkg.defs import Holder\n\n\ndef use(holder: Holder, field):\n    holder.RUN(field)\n";
const BOUND_ALIAS_READ_USE: &str = "from pkg.defs import Holder\n\n\ndef use(holder: Holder, field):\n    run = holder.RUN\n    run(field)\n";
const BOUND_DEFS_PATCHING: &str =
    "\n\ndef patch(run):\n    from pkg import defs\n\n    defs.TABLE[\"run\"] = run\n";
const BOUND_MODULE_USE: &str =
    "from pkg.defs import ops\n\n\ndef use(field):\n    ops().require(field)\n";
const BOUND_TYPE_VAR_USE: &str = "from pkg.defs import V\n\n\ndef use(view: V):\n    for field in view.fields:\n        field.required = True\n";
const BOUND_TYPE_ALIAS_USE: &str =
    "from pkg.defs import Touching\n\n\ndef use(value: Touching, field):\n    value.touch(field)\n";
const BOUND_EXPORTS: &str = "from pkg.defs import Touchers\n";
const BOUND_EXPORTED_ALIAS_USE: &str = "from pkg.exports import Touchers\n\n\ndef use(values: Touchers, field):\n    for value in values:\n        value.touch(field)\n";
// The items of a list attribute another module appends to (`StoredElsewhere::changed`).
const BOUND_LISTED: &str = "from pkg.model import Field\n\n\nclass Model:\n    def __init__(self, fields: list[Field]):\n        self.fields = self.validate(fields) if fields else []\n\n    def validate(self, fields: list[Field]) -> list[Field]:\n        return fields\n";
const BOUND_LIST_USE: &str = "from pkg.listed import Model\n\n\ndef use(model: Model):\n    for field in model.fields:\n        field.required = True\n";
const BOUND_APPENDING: &str = "from pkg.listed import Model\nfrom pkg.model import Field\n\n\ndef add(model: Model, field: Field):\n    model.fields.append(field)\n";
const BOUND_REGISTERING: &str = "from pkg.defs import TABLE\nfrom pkg.helpers import require\n\n\ndef register():\n    TABLE[\"other\"] = require\n";

#[test]
fn lsp_diagnostics_equal_a_full_check_through_bound_values_across_modules() {
    let files = vec![
        ("pkg/__init__.py", String::new()),
        ("pkg/model.py", BOUND_MODEL.to_owned()),
        ("pkg/helpers.py", BOUND_HELPERS.to_owned()),
        ("pkg/defs.py", BOUND_DEFS.to_owned()),
        ("pkg/use_table.py", BOUND_TABLE_USE.to_owned()),
        ("pkg/use_alias.py", BOUND_ALIAS_USE.to_owned()),
        ("pkg/use_module.py", BOUND_MODULE_USE.to_owned()),
        ("pkg/use_type_var.py", BOUND_TYPE_VAR_USE.to_owned()),
        ("pkg/use_type_alias.py", BOUND_TYPE_ALIAS_USE.to_owned()),
        ("pkg/exports.py", BOUND_EXPORTS.to_owned()),
        (
            "pkg/use_exported_alias.py",
            BOUND_EXPORTED_ALIAS_USE.to_owned(),
        ),
    ];
    let s = Session::start_with("lsp_oracle_bound_values", BOUND_CONFIG, files, false);
    let all: &[(&str, &str)] = &[
        ("pkg/use_table.py", "EFF201"),
        ("pkg/use_alias.py", "EFF201"),
        ("pkg/use_module.py", "EFF201"),
        ("pkg/use_type_var.py", "EFF201"),
        ("pkg/use_type_alias.py", "EFF201"),
        ("pkg/use_exported_alias.py", "EFF201"),
    ];
    let steps = [
        Step::new("initial index", Vec::new()).present(all),
        Step::new(
            "the table binds the key to the function that writes nothing",
            vec![save(
                "pkg/defs.py",
                &BOUND_DEFS.replace("{\"run\": require, \"skip\": skip}", "{\"run\": skip}"),
            )],
        )
        .absent(&[("pkg/use_table.py", "EFF201")]),
        Step::new("the table binds it to the writer again", vec![save("pkg/defs.py", BOUND_DEFS)])
            .present(&[("pkg/use_table.py", "EFF201")]),
        Step::new(
            "another module changes the table in place",
            vec![Action::Create(
                "pkg/register.py",
                "from pkg.defs import TABLE\n\n\ndef register(name, run):\n    TABLE[name] = run\n"
                    .to_owned(),
            )],
        )
        .absent(&[("pkg/use_table.py", "EFF201")]),
        Step::new(
            "the module changing it is deleted",
            vec![Action::Delete("pkg/register.py")],
        )
        .present(&[("pkg/use_table.py", "EFF201")]),
        Step::new(
            "a subclass binds the attribute to a method of its own",
            vec![Action::Create(
                "pkg/sub.py",
                "from pkg.defs import Holder\n\n\nclass Sub(Holder):\n    def RUN(self, field):\n        pass\n"
                    .to_owned(),
            )],
        )
        .absent(&[("pkg/use_alias.py", "EFF201")]),
        Step::new("the subclass is deleted", vec![Action::Delete("pkg/sub.py")])
            .present(&[("pkg/use_alias.py", "EFF201")]),
        Step::new(
            "the function returns another module",
            vec![save(
                "pkg/defs.py",
                &BOUND_DEFS.replace("    from pkg import helpers\n\n    return helpers", "    from pkg import model\n\n    return model"),
            )],
        )
        .absent(&[("pkg/use_module.py", "EFF201")]),
        Step::new(
            "the type variable's bound is a class without the fields",
            vec![save("pkg/defs.py", &BOUND_DEFS.replace("bound=View", "bound=Other"))],
        )
        .present(&[("pkg/use_module.py", "EFF201")])
        .absent(&[("pkg/use_type_var.py", "EFF201")]),
        Step::new("the bound is the view again", vec![save("pkg/defs.py", BOUND_DEFS)])
            .present(all),
        Step::new(
            "the type alias names only the class that writes nothing",
            vec![save(
                "pkg/defs.py",
                &BOUND_DEFS.replace("Touching: TypeAlias = Writer | Reader", "Touching: TypeAlias = Reader"),
            )],
        )
        .absent(&[("pkg/use_type_alias.py", "EFF201")]),
        Step::new("the alias names the writer again", vec![save("pkg/defs.py", BOUND_DEFS)])
            .present(all),
        Step::new(
            "the re-exported alias names a list of the class that writes nothing",
            vec![save(
                "pkg/defs.py",
                &BOUND_DEFS.replace("Touchers = list[Writer]", "Touchers = list[Reader]"),
            )],
        )
        .absent(&[("pkg/use_exported_alias.py", "EFF201")]),
        Step::new("it names a list of the writer again", vec![save("pkg/defs.py", BOUND_DEFS)])
            .present(all),
        Step::new(
            "the table's module changes it through its own module object",
            vec![save("pkg/defs.py", &format!("{BOUND_DEFS}{BOUND_DEFS_PATCHING}"))],
        )
        .absent(&[("pkg/use_table.py", "EFF201")]),
        Step::new("it no longer changes the table", vec![save("pkg/defs.py", BOUND_DEFS)])
            .present(all),
        Step::new(
            "a module calls the attribute it reads, which binds the function that writes nothing",
            vec![
                Action::Create("pkg/use_alias_read.py", BOUND_ALIAS_READ_USE.to_owned()),
                save(
                    "pkg/defs.py",
                    &BOUND_DEFS.replace("RUN = staticmethod(require)", "RUN = staticmethod(skip)"),
                ),
            ],
        )
        .absent(&[
            ("pkg/use_alias.py", "EFF201"),
            ("pkg/use_alias_read.py", "EFF201"),
        ]),
        Step::new(
            "a subclass binds the attribute to the writer",
            vec![Action::Create(
                "pkg/sub.py",
                "from pkg.defs import Holder\nfrom pkg.helpers import require\n\n\nclass Sub(Holder):\n    RUN = staticmethod(require)\n"
                    .to_owned(),
            )],
        )
        .present(&[
            ("pkg/use_alias.py", "EFF201"),
            ("pkg/use_alias_read.py", "EFF201"),
        ]),
        Step::new("the subclass is deleted", vec![Action::Delete("pkg/sub.py")]).absent(&[
            ("pkg/use_alias.py", "EFF201"),
            ("pkg/use_alias_read.py", "EFF201"),
        ]),
        Step::new("the class binds the writer again", vec![save("pkg/defs.py", BOUND_DEFS)])
            .present(&[
                ("pkg/use_alias.py", "EFF201"),
                ("pkg/use_alias_read.py", "EFF201"),
            ]),
        // What another module's changes in place add joins the items (`StoredElsewhere::changed`).
        Step::new(
            "another module appends a field to a list attribute",
            vec![
                Action::Create("pkg/listed.py", BOUND_LISTED.to_owned()),
                Action::Create("pkg/use_list.py", BOUND_LIST_USE.to_owned()),
                Action::Create("pkg/appending.py", BOUND_APPENDING.to_owned()),
            ],
        )
        .present(&[("pkg/use_list.py", "EFF201")]),
        Step::new(
            "it appends a value of unknown type",
            vec![save(
                "pkg/appending.py",
                &BOUND_APPENDING.replace("field: Field", "field"),
            )],
        )
        .absent(&[("pkg/use_list.py", "EFF201")]),
        Step::new(
            "it appends a field again",
            vec![save("pkg/appending.py", BOUND_APPENDING)],
        )
        .present(&[("pkg/use_list.py", "EFF201")]),
        Step::new(
            "the table binds the key to the function that writes nothing, and another module adds the writer",
            vec![
                save(
                    "pkg/defs.py",
                    &BOUND_DEFS.replace("{\"run\": require, \"skip\": skip}", "{\"run\": skip}"),
                ),
                Action::Create("pkg/register.py", BOUND_REGISTERING.to_owned()),
            ],
        )
        .present(&[("pkg/use_table.py", "EFF201")]),
        Step::new(
            "it adds a function of unknown type",
            vec![save(
                "pkg/register.py",
                &BOUND_REGISTERING
                    .replace("def register():", "def register(run):")
                    .replace("= require", "= run"),
            )],
        )
        .absent(&[("pkg/use_table.py", "EFF201")]),
        Step::new(
            "it adds the writer again",
            vec![save("pkg/register.py", BOUND_REGISTERING)],
        )
        .present(&[("pkg/use_table.py", "EFF201")]),
    ];
    run_steps(s, &steps);
}

// ------------------------------------------------------------------------------ import cycles

const CYCLES_CONFIG: &str = "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"IMP101\"]\n";
const TYPED_F: &str = "from typing import TYPE_CHECKING\n\nif TYPE_CHECKING:\n    import pkg.g\n";

/// `IMP101` reads the imports of every module: an edit of one module's imports closes or breaks
/// a cycle reported in another, also an import leaving `TYPE_CHECKING` and, once `cycle-edges`
/// counts them, an import added inside a function, which changes no declaration of its module.
/// The report's related location moves with the import, a package's `__init__` closes a cycle
/// through the parent package an import loads first, a module created turns a `from` import of
/// a name into an import of the submodule, which then closes a cycle, and a package binding the
/// name (no import changes) takes it back. A baseline entry of a group holding a group's
/// modules covers its report, in the editor as in `heki check`.
#[test]
fn lsp_diagnostics_equal_a_full_check_through_import_cycles() {
    let files = vec![
        ("pkg/__init__.py", String::new()),
        ("pkg/a.py", "import pkg.base\nimport pkg.b\n".to_owned()),
        ("pkg/b.py", "import json\n".to_owned()),
        ("pkg/base.py", String::new()),
    ];
    let s = Session::start_with("lsp_oracle_import_cycles", CYCLES_CONFIG, files, false);
    let steps = [
        Step::new("a module imports another", Vec::new()).absent(&[("pkg/b.py", "IMP101")]),
        Step::new(
            "the other module imports it back",
            vec![save("pkg/b.py", "import json\nimport pkg.a\n")],
        )
        .present(&[("pkg/b.py", "IMP101")]),
        Step::new(
            "the first module's import moves down a line",
            vec![save("pkg/a.py", "import pkg.base\n\nimport pkg.b\n")],
        )
        .present(&[("pkg/b.py", "IMP101")]),
        Step::new(
            "the first module imports the other only when a function runs",
            vec![save("pkg/a.py", "import pkg.base\n\n\ndef load():\n    import pkg.b\n")],
        )
        .absent(&[("pkg/b.py", "IMP101")]),
        Step::new(
            "the configuration counts the imports in functions",
            vec![Action::Configure(format!(
                "{CYCLES_CONFIG}\n[tool.heki.imports]\ncycle-edges = [\"body\", \"function\"]\n"
            ))],
        )
        .present(&[("pkg/b.py", "IMP101")]),
        Step::new(
            "the configuration counts only what runs while a module loads",
            vec![Action::Configure(CYCLES_CONFIG.to_owned())],
        )
        .absent(&[("pkg/b.py", "IMP101")]),
        Step::new(
            "the first module imports the other while it loads again",
            vec![save(
                "pkg/a.py",
                "import pkg.base\nimport pkg.b\n\n\ndef load():\n    pass\n",
            )],
        )
        .present(&[("pkg/b.py", "IMP101")]),
        Step::new(
            "the module closing the cycle is deleted",
            vec![Action::Delete("pkg/b.py")],
        ),
        Step::new(
            "a module imports a module of a package",
            vec![
                Action::Create("app/__init__.py", String::new()),
                Action::Create("app/sub/__init__.py", String::new()),
                Action::Create("app/sub/leaf.py", String::new()),
                Action::Create("app/a.py", "import app.sub.leaf\n".to_owned()),
            ],
        )
        .absent(&[("app/sub/__init__.py", "IMP101")]),
        Step::new(
            "the package's __init__ imports the module back",
            vec![save("app/sub/__init__.py", "from app import a\n")],
        )
        .present(&[("app/sub/__init__.py", "IMP101")]),
        Step::new(
            "the module imports a name of its own package",
            vec![Action::Create("pkg/e.py", "from pkg import c\n".to_owned())],
        )
        .absent(&[("pkg/e.py", "IMP101")]),
        Step::new(
            "a submodule of that name is created",
            vec![Action::Create("pkg/c.py", String::new())],
        )
        .absent(&[("pkg/e.py", "IMP101")]),
        Step::new(
            "the submodule imports the module back",
            vec![save("pkg/c.py", "import pkg.e\n")],
        )
        .present(&[("pkg/e.py", "IMP101")]),
        Step::new(
            "the package binds the name, which the from-import may take instead",
            vec![save("pkg/__init__.py", "c = None\n")],
        )
        .absent(&[("pkg/e.py", "IMP101")]),
        Step::new(
            "the package binds nothing again",
            vec![save("pkg/__init__.py", "")],
        )
        .present(&[("pkg/e.py", "IMP101")]),
        Step::new(
            "the submodule no longer imports the module",
            vec![save("pkg/c.py", "")],
        )
        .absent(&[("pkg/e.py", "IMP101")]),
        Step::new(
            "a module imports another only under TYPE_CHECKING, which imports it back",
            vec![
                Action::Create("pkg/f.py", TYPED_F.to_owned()),
                Action::Create("pkg/g.py", "import pkg.f\n".to_owned()),
            ],
        )
        .absent(&[("pkg/g.py", "IMP101")]),
        Step::new(
            "the import leaves TYPE_CHECKING",
            vec![save("pkg/f.py", &TYPED_F.replace("if TYPE_CHECKING:\n    ", ""))],
        )
        .present(&[("pkg/g.py", "IMP101")]),
        Step::new(
            "the configuration counts the imports in functions, and a module's function imports nothing",
            vec![
                Action::Configure(format!(
                    "{CYCLES_CONFIG}\n[tool.heki.imports]\ncycle-edges = [\"body\", \"function\"]\n"
                )),
                Action::Create("pkg/h.py", "def load():\n    pass\n".to_owned()),
                Action::Create("pkg/i.py", "import pkg.h\n".to_owned()),
            ],
        )
        .absent(&[("pkg/i.py", "IMP101")]),
        Step::new(
            "the function imports the module that imports its module",
            vec![save("pkg/h.py", "def load():\n    import pkg.i\n")],
        )
        .present(&[("pkg/i.py", "IMP101")]),
        Step::new(
            "a baseline records a group holding the two modules and one more",
            vec![
                Action::Write("heki-baseline.toml", group_baseline("pkg.h, pkg.i, pkg.j")),
                Action::Configure(format!(
                    "{}\n[tool.heki.imports]\ncycle-edges = [\"body\", \"function\"]\n",
                    CYCLES_CONFIG.replace(
                        "[tool.heki.lint]",
                        "[tool.heki]\nbaseline = \"heki-baseline.toml\"\n\n[tool.heki.lint]"
                    )
                )),
            ],
        )
        .absent(&[("pkg/i.py", "IMP101")]),
        Step::new(
            "the baseline's group lacks one of the two modules",
            vec![Action::Write("heki-baseline.toml", group_baseline("pkg.h, pkg.j"))],
        )
        .present(&[("pkg/i.py", "IMP101")]),
    ];
    run_steps(s, &steps);
}

const CUTS_CONFIG: &str = "[project]\nname = \"acme\"\ndependencies = []\n\n[project.optional-dependencies]\nplot = [\"matplotlib\"]\n\n[tool.heki.lint]\nselect = [\"IMP101\", \"IMP302\"]\n";

/// The help of `IMP101` and `IMP302` names the one import every cycle or every chain goes
/// through, which the imports of other modules decide: a second cycle, or a second chain, that
/// an edit of another module adds takes the name away, and the edit undone brings it back. So
/// do the reads of what the imports bind while the modules load, in the module and in another
/// one that takes the name from it.
#[test]
fn lsp_diagnostics_equal_a_full_check_through_import_cuts() {
    let files = vec![
        ("acme/__init__.py", "from acme import api\n".to_owned()),
        ("acme/api.py", "from acme import viz\n".to_owned()),
        ("acme/viz.py", "import matplotlib\n".to_owned()),
        ("acme/ra.py", "import acme.rb\n".to_owned()),
        ("acme/rb.py", "import acme.rc\n".to_owned()),
        ("acme/rc.py", "import acme.ra\n".to_owned()),
    ];
    let s = Session::start_with("lsp_oracle_import_cuts", CUTS_CONFIG, files, false);
    let both = &[("acme/rc.py", "IMP101"), ("acme/viz.py", "IMP302")];
    let steps = [
        Step::new(
            "one import closes every cycle and carries every chain",
            Vec::new(),
        )
        .present(both),
        Step::new(
            "a module of the ring closes a second cycle through another module",
            vec![
                Action::Create("acme/rd.py", "import acme.rb\n".to_owned()),
                save("acme/rb.py", "import acme.rc\nimport acme.rd\n"),
            ],
        )
        .present(both),
        Step::new(
            "the package imports the module with the extra directly too",
            vec![save("acme/__init__.py", "from acme import api, viz\n")],
        )
        .present(both),
        Step::new(
            "the second cycle and the second chain are gone",
            vec![
                save("acme/rb.py", "import acme.rc\n"),
                save("acme/__init__.py", "from acme import api\n"),
            ],
        )
        .present(both),
        Step::new(
            "two modules of the ring read what their imports bind while they load",
            vec![
                save("acme/rb.py", "import acme.rc\n\nOWNER = acme.rc\n"),
                save("acme/rc.py", "import acme.ra\n\nOWNER = acme.ra\n"),
            ],
        )
        .present(both),
        Step::new(
            "the third module reads it while it loads too",
            vec![save("acme/ra.py", "import acme.rb\n\nOWNER = acme.rb\n")],
        )
        .present(both),
        Step::new(
            "the third module reads it only in a function",
            vec![save(
                "acme/ra.py",
                "import acme.rb\n\n\ndef owner():\n    return acme.rb\n",
            )],
        )
        .present(both),
        Step::new(
            "the third module takes a name of the second and reads it only in a function",
            vec![save(
                "acme/ra.py",
                "from acme.rb import OWNER\n\n\ndef owner():\n    return OWNER\n",
            )],
        )
        .present(both),
        Step::new(
            "another module takes that name from the third while it loads",
            vec![Action::Create(
                "acme/rx.py",
                "from acme.ra import OWNER\n\nCOPY = OWNER\n".to_owned(),
            )],
        )
        .present(both),
        Step::new(
            "it takes the name only in a function",
            vec![save(
                "acme/rx.py",
                "def owner():\n    from acme.ra import OWNER\n\n    return OWNER\n",
            )],
        )
        .present(both),
        Step::new(
            "it reads the name off the module while it loads",
            vec![save(
                "acme/rx.py",
                "import acme.ra\n\nCOPY = acme.ra.OWNER\n",
            )],
        )
        .present(both),
        Step::new(
            "the module that read the name is gone",
            vec![Action::Delete("acme/rx.py")],
        )
        .present(both),
    ];
    run_steps(s, &steps);
}

/// A baseline whose one entry records the `IMP101` group of the modules `target` lists.
fn group_baseline(target: &str) -> String {
    format!(
        "version = 2\n\n[[entries]]\nrule_code = \"IMP101\"\ntarget = \"{target}\"\ncount = 1\n"
    )
}

// ------------------------------------------------------------------ circular imports that fail

const FAILURES_CONFIG: &str =
    "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"IMP102\"]\n";
const FAILING_A: &str = "import pkg.b\n\n\ndef helper_a():\n    return 1\n";

/// `IMP102` reads the imports and the order of the module bodies of every module: an edit of one
/// module adds or removes the report in another. Binding the name before the import, a handler
/// of any import error around the failing import or around the import that leads to it,
/// another module storing the name, and a function that may load the cycle called before the
/// import each take the report away, and undoing them brings it back. A handler of a missing
/// module takes nothing of it.
#[test]
fn lsp_diagnostics_equal_a_full_check_through_circular_imports() {
    let files = vec![
        ("pkg/__init__.py", String::new()),
        ("pkg/a.py", FAILING_A.to_owned()),
        ("pkg/b.py", "import json\n".to_owned()),
    ];
    let s = Session::start_with("lsp_oracle_circular_imports", FAILURES_CONFIG, files, false);
    let steps = [
        Step::new("a module imports another", Vec::new()).absent(&[("pkg/b.py", "IMP102")]),
        Step::new(
            "the other module imports a name of the first, which binds it after its import",
            vec![save("pkg/b.py", "from pkg.a import helper_a\n")],
        )
        .present(&[("pkg/b.py", "IMP102")]),
        Step::new(
            "the first module binds the name before its import",
            vec![save(
                "pkg/a.py",
                "def helper_a():\n    return 1\n\n\nimport pkg.b\n",
            )],
        )
        .absent(&[("pkg/b.py", "IMP102")]),
        Step::new(
            "the first module binds the name after its import again",
            vec![save("pkg/a.py", FAILING_A)],
        )
        .present(&[("pkg/b.py", "IMP102")]),
        Step::new(
            "a handler takes the error",
            vec![save(
                "pkg/b.py",
                "try:\n    from pkg.a import helper_a\nexcept ImportError:\n    helper_a = None\n",
            )],
        )
        .absent(&[("pkg/b.py", "IMP102")]),
        Step::new(
            "the handler takes only a missing module",
            vec![save(
                "pkg/b.py",
                "try:\n    from pkg.a import helper_a\nexcept ModuleNotFoundError:\n    helper_a = None\n",
            )],
        )
        .present(&[("pkg/b.py", "IMP102")]),
        Step::new(
            "the handler goes",
            vec![save("pkg/b.py", "from pkg.a import helper_a\n")],
        )
        .present(&[("pkg/b.py", "IMP102")]),
        Step::new(
            "the first module imports the other under a handler of a missing module",
            vec![save(
                "pkg/a.py",
                "try:\n    import pkg.b\nexcept ModuleNotFoundError:\n    pass\n\n\ndef helper_a():\n    return 1\n",
            )],
        )
        .present(&[("pkg/b.py", "IMP102")]),
        Step::new(
            "its handler takes any import error",
            vec![save(
                "pkg/a.py",
                "try:\n    import pkg.b\nexcept ImportError:\n    pass\n\n\ndef helper_a():\n    return 1\n",
            )],
        )
        .absent(&[("pkg/b.py", "IMP102")]),
        Step::new(
            "the first module imports the other plainly again",
            vec![save("pkg/a.py", FAILING_A)],
        )
        .present(&[("pkg/b.py", "IMP102")]),
        Step::new(
            "another module stores the name into the first",
            vec![Action::Create(
                "pkg/c.py",
                "import pkg.a\n\n\ndef install():\n    pkg.a.helper_a = len\n".to_owned(),
            )],
        )
        .absent(&[("pkg/b.py", "IMP102")]),
        Step::new("that module is deleted", vec![Action::Delete("pkg/c.py")])
            .present(&[("pkg/b.py", "IMP102")]),
        Step::new(
            "the first module calls a function before its import",
            vec![
                Action::Create("pkg/loader.py", "def load():\n    pass\n".to_owned()),
                save(
                    "pkg/a.py",
                    &format!("from pkg.loader import load\n\nload()\n{FAILING_A}"),
                ),
            ],
        )
        .present(&[("pkg/b.py", "IMP102")]),
        Step::new(
            "the function imports a module of the cycle",
            vec![save("pkg/loader.py", "def load():\n    import pkg.b\n")],
        )
        .absent(&[("pkg/b.py", "IMP102")]),
        Step::new(
            "the function imports nothing again",
            vec![save("pkg/loader.py", "def load():\n    pass\n")],
        )
        .present(&[("pkg/b.py", "IMP102")]),
    ];
    run_steps(s, &steps);
}

// --------------------------------------------- development code and dependencies at import time

const DEPENDENCIES_CONFIG: &str = "[project]\nname = \"pkg\"\ndependencies = [\"httpx\"]\n\n[project.optional-dependencies]\nredis = [\"redis\"]\n\n[dependency-groups]\ntest = [\"pytest\"]\n\n[tool.heki.lint]\nselect = [\"IMP104\", \"IMP301\", \"IMP302\"]\n";

/// `IMP302` reads what the base module loads, and `IMP301` what the package modules load,
/// through every module's imports: an edit of the package's `__init__` adds or removes the
/// report in the module it loads, also through a module that is no package code, and a handler
/// around that import that takes the missing module takes it away, unless it raises it again.
/// The declarations decide them: moving a distribution into
/// the dependencies, or `module-distributions` naming another one, takes the report away, and
/// dependencies the build backend computes take every report away.
/// `IMP104` follows the module an import names, which a deleted file no longer is.
#[test]
fn lsp_diagnostics_equal_a_full_check_through_dependencies_at_import_time() {
    let files = vec![
        ("pkg/__init__.py", String::new()),
        ("pkg/cache.py", "import redis\n".to_owned()),
        ("pkg/clock.py", "import json\n".to_owned()),
        ("helpers.py", "import pytest\n".to_owned()),
        ("tests/__init__.py", String::new()),
        ("tests/factories.py", String::new()),
    ];
    let s = Session::start_with(
        "lsp_oracle_import_dependencies",
        DEPENDENCIES_CONFIG,
        files,
        false,
    );
    let steps = [
        Step::new("no base module loads the extra's module", Vec::new())
            .absent(&[("pkg/cache.py", "IMP302"), ("helpers.py", "IMP301")]),
        Step::new(
            "the package imports it",
            vec![save("pkg/__init__.py", "from pkg import cache\n")],
        )
        .present(&[("pkg/cache.py", "IMP302")]),
        Step::new(
            "a handler takes its error",
            vec![save(
                "pkg/__init__.py",
                "try:\n    from pkg import cache\nexcept ImportError:\n    cache = None\n",
            )],
        )
        .absent(&[("pkg/cache.py", "IMP302")]),
        Step::new(
            "the handler takes only a missing module",
            vec![save(
                "pkg/__init__.py",
                "try:\n    from pkg import cache\nexcept ModuleNotFoundError:\n    cache = None\n",
            )],
        )
        .absent(&[("pkg/cache.py", "IMP302")]),
        Step::new(
            "the handler raises it again",
            vec![save(
                "pkg/__init__.py",
                "try:\n    from pkg import cache\nexcept ModuleNotFoundError:\n    raise\n",
            )],
        )
        .present(&[("pkg/cache.py", "IMP302")]),
        Step::new(
            "the handler gives a fallback again",
            vec![save(
                "pkg/__init__.py",
                "try:\n    from pkg import cache\nexcept ModuleNotFoundError:\n    cache = None\n",
            )],
        )
        .absent(&[("pkg/cache.py", "IMP302")]),
        Step::new(
            "the handler goes",
            vec![save("pkg/__init__.py", "from pkg import cache\n")],
        )
        .present(&[("pkg/cache.py", "IMP302")]),
        Step::new(
            "a package module imports a development dependency",
            vec![save("pkg/clock.py", "import pytest\n")],
        )
        .present(&[("pkg/clock.py", "IMP301")]),
        Step::new(
            "the dependency is declared",
            vec![Action::Configure(
                DEPENDENCIES_CONFIG.replace("[\"httpx\"]", "[\"httpx\", \"pytest\"]"),
            )],
        )
        .absent(&[("pkg/clock.py", "IMP301")]),
        Step::new(
            "it is a development dependency again",
            vec![Action::Configure(DEPENDENCIES_CONFIG.to_owned())],
        )
        .present(&[("pkg/clock.py", "IMP301")]),
        Step::new(
            "the package loads a module that is no package code",
            vec![save("pkg/__init__.py", "from pkg import cache\nimport helpers\n")],
        )
        .present(&[("helpers.py", "IMP301"), ("pkg/cache.py", "IMP302")]),
        Step::new(
            "it no longer does",
            vec![save("pkg/__init__.py", "from pkg import cache\n")],
        )
        .absent(&[("helpers.py", "IMP301")]),
        Step::new(
            "the user's map names a dependency for the extra's module",
            vec![Action::Configure(format!(
                "{DEPENDENCIES_CONFIG}\n[tool.heki.imports]\nmodule-distributions = {{ redis = [\"httpx\"] }}\n"
            ))],
        )
        .absent(&[("pkg/cache.py", "IMP302")]),
        Step::new(
            "the map goes",
            vec![Action::Configure(DEPENDENCIES_CONFIG.to_owned())],
        )
        .present(&[("pkg/cache.py", "IMP302")]),
        Step::new(
            "the build backend computes the dependencies",
            vec![Action::Configure(DEPENDENCIES_CONFIG.replace(
                "dependencies = [\"httpx\"]",
                "dynamic = [\"dependencies\"]",
            ))],
        )
        .absent(&[("pkg/cache.py", "IMP302"), ("pkg/clock.py", "IMP301")]),
        Step::new(
            "they are listed again",
            vec![Action::Configure(DEPENDENCIES_CONFIG.to_owned())],
        )
        .present(&[("pkg/cache.py", "IMP302"), ("pkg/clock.py", "IMP301")]),
        Step::new(
            "a package module imports test code",
            vec![save("pkg/clock.py", "import tests.factories\n")],
        )
        .present(&[("pkg/clock.py", "IMP104")]),
        Step::new(
            "the test module is deleted",
            vec![Action::Delete("tests/factories.py")],
        )
        .absent(&[("pkg/clock.py", "IMP104")]),
        Step::new(
            "it is back",
            vec![Action::Create("tests/factories.py", String::new())],
        )
        .present(&[("pkg/clock.py", "IMP104")]),
    ];
    run_steps(s, &steps);
}

// ---------------------------------------------- what contracts cannot attribute across modules

// What a strict contract reports from another module's facts: an effect boundary the call
// heki cannot resolve in a helper of another file (`OptInTables::blind_spots`), and a call
// owner a helper another file hands on as a value (`FileSummary::reference_sites`).
const STRICT_CONFIG: &str = "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"EFF201\", \"MUT204\"]\n\n[[tool.heki.effect-boundaries]]\nname = \"checks\"\nscopes = [\"pkg.checks.*\"]\nwrite-universe = [\"Field.required\"]\nallow-writes = []\nunresolved = \"report\"\n\n[[tool.heki.call-owners]]\nname = \"settles\"\ntargets = [\"Store.require\"]\nallowed-callers = [\"pkg.passes.settle\"]\nunresolved = \"report\"\n";
const STRICT_MODEL: &str = "class Field:\n    def __init__(self):\n        self.required = False\n\n\nclass Store:\n    def require(self, field: Field):\n        field.required = True\n";
const STRICT_HELPER: &str =
    "from pkg.model import Field\n\n\ndef call(run, field: Field):\n    pass\n";
const STRICT_CHECKS: &str = "from pkg import helper\nfrom pkg.model import Field\n\n\ndef check(run, field: Field):\n    helper.call(run, field)\n";
const STRICT_PASSES: &str = "from pkg.model import Field, Store\n\n\ndef _require(store: Store, field: Field):\n    store.require(field)\n\n\ndef settle(store: Store, field: Field):\n    _require(store, field)\n";

#[test]
fn lsp_diagnostics_equal_a_full_check_through_what_contracts_cannot_attribute() {
    let files = vec![
        ("pkg/__init__.py", String::new()),
        ("pkg/model.py", STRICT_MODEL.to_owned()),
        ("pkg/helper.py", STRICT_HELPER.to_owned()),
        ("pkg/checks.py", STRICT_CHECKS.to_owned()),
        ("pkg/passes.py", STRICT_PASSES.to_owned()),
    ];
    let s = Session::start_with("lsp_oracle_strict", STRICT_CONFIG, files, false);
    let steps = [
        Step::new("initial index", Vec::new())
            .absent(&[("pkg/checks.py", "EFF201"), ("pkg/passes.py", "MUT204")]),
        Step::new(
            "the helper calls what it is handed",
            vec![save(
                "pkg/helper.py",
                &STRICT_HELPER.replace("    pass", "    run(field)"),
            )],
        )
        .present(&[("pkg/checks.py", "EFF201")]),
        Step::new(
            "the helper calls nothing again",
            vec![save("pkg/helper.py", STRICT_HELPER)],
        )
        .absent(&[("pkg/checks.py", "EFF201")]),
        Step::new(
            "another module keeps the private helper in a list",
            vec![Action::Create(
                "pkg/registry.py",
                "from pkg.passes import _require\n\nHANDLERS = [_require]\n".to_owned(),
            )],
        )
        .present(&[("pkg/passes.py", "MUT204")]),
        Step::new(
            "that module is deleted",
            vec![Action::Delete("pkg/registry.py")],
        )
        .absent(&[("pkg/passes.py", "MUT204")]),
        Step::new(
            "a check constructs a field that names its attribute at run time",
            vec![
                save("pkg/model.py", &strict_model("self")),
                save("pkg/checks.py", &format!("{STRICT_CHECKS}{STRICT_BUILD}")),
            ],
        )
        .absent(&[("pkg/checks.py", "EFF201")]),
        Step::new(
            "the initializer writes an object it is handed",
            vec![save("pkg/model.py", &strict_model("source"))],
        )
        .present(&[("pkg/checks.py", "EFF201")]),
        Step::new(
            "the initializer writes its own instance again",
            vec![save("pkg/model.py", &strict_model("self"))],
        )
        .absent(&[("pkg/checks.py", "EFF201")]),
    ];
    run_steps(s, &steps);
}

const STRICT_BUILD: &str = "\n\ndef build():\n    return Field()\n";

/// [`STRICT_MODEL`] with an initializer writing an attribute named at run time on `written`.
fn strict_model(written: &str) -> String {
    STRICT_MODEL.replace(
        "    def __init__(self):\n        self.required = False\n",
        &format!(
            "    def __init__(self, name=\"required\", source=None):\n        setattr({written}, name, False)\n"
        ),
    )
}

// ------------------------------------------------- argument lists across modules (session 4)

// A call target with an argument list, passed a number constant of another module by keyword:
// the constant's value and whether a keyword binds the parameter come from the other modules'
// declarations (`ModuleDecls::literal_constants`, `FunctionDecl::positional_only`).
const MARKED_CONFIG: &str = "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"levels\"\ntargets = [\"pkg.sig.levels(level=0)\"]\n";
const MARKED_LEVELS: &str = "def levels(level=None, **options):\n    return level\n";
const MARKED_POSITIONAL: &str = "def levels(level=None, /, **options):\n    return level\n";
const MARKED_USE: &str = "from pkg.limits import LIMIT\nfrom pkg.sig import levels\n\n\ndef use():\n    return levels(level=LIMIT)\n";
const MARKED_REBIND: &str = "from pkg import limits\n\n\ndef reset():\n    limits.LIMIT = 0\n";

#[test]
fn lsp_diagnostics_equal_a_full_check_through_argument_lists_across_modules() {
    let files = vec![
        ("pkg/__init__.py", String::new()),
        ("pkg/sig.py", MARKED_LEVELS.to_owned()),
        ("pkg/limits.py", "LIMIT = 0\n".to_owned()),
        ("pkg/use.py", MARKED_USE.to_owned()),
    ];
    let mut s = Session::start_with("lsp_oracle_argument_lists", MARKED_CONFIG, files, false);
    let reported: &[(&str, &str)] = &[("pkg/use.py", "MUT204")];
    let steps = [
        Step::new("initial index", Vec::new()).present(reported),
        Step::new(
            "the constant's module binds it to another number",
            vec![save("pkg/limits.py", "LIMIT = 1\n")],
        )
        .absent(reported),
        Step::new(
            "it binds the number of the list again",
            vec![save("pkg/limits.py", "LIMIT = 0\n")],
        )
        .present(reported),
        Step::new(
            "a third module starts rebinding it",
            vec![Action::Create("pkg/rebind.py", MARKED_REBIND.to_owned())],
        )
        .absent(reported),
        Step::new(
            "the third module is deleted",
            vec![Action::Delete("pkg/rebind.py")],
        )
        .present(reported),
        Step::new(
            "the parameter becomes positional-only: the keyword lands in `**options`",
            vec![save("pkg/sig.py", MARKED_POSITIONAL)],
        )
        .absent(reported),
        Step::new(
            "a keyword binds it again",
            vec![save("pkg/sig.py", MARKED_LEVELS)],
        )
        .present(reported),
    ];
    let mut failures = Vec::new();
    for step in &steps {
        failures.extend(s.run(step));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    s.lsp.stop(true);
}

// ------------------------------------------------ builders of a library base class (session)

// A call target with an argument list, met by `model_construct` / `model_validate` calls on a
// model of another module: whether the call builds the class, and from which values, comes from
// the class's bases there (the catalog's `builds_receiver` of `pydantic.BaseModel`'s methods)
// and from a method a project base may define over them.
const BUILDERS_CONFIG: &str = "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"maps\"\ntargets = [\"pkg.models.DataType(is_dict=True)\"]\n";
const BUILDERS_MODEL: &str =
    "from pydantic import BaseModel\n\n\nclass DataType(BaseModel):\n    is_dict: bool = False\n";
const BUILDERS_PLAIN: &str = "class DataType:\n    is_dict: bool = False\n";
const BUILDERS_BASE: &str =
    "from pydantic import BaseModel\n\n\nclass Base(BaseModel):\n    pass\n";
const BUILDERS_OWN: &str = "from pydantic import BaseModel\n\n\nclass Base(BaseModel):\n    @classmethod\n    def model_construct(cls, **values):\n        return None\n\n    @classmethod\n    def model_validate(cls, obj):\n        return None\n";
const BUILDERS_ON_BASE: &str =
    "from pkg.base import Base\n\n\nclass DataType(Base):\n    is_dict: bool = False\n";
const BUILDERS_USE: &str = "from pkg.models import DataType\n\n\ndef construct():\n    return DataType.model_construct(is_dict=True)\n\n\ndef validate():\n    return DataType.model_validate({\"is_dict\": True})\n";

#[test]
fn lsp_diagnostics_equal_a_full_check_through_builders_of_a_library_base() {
    let files = vec![
        ("pkg/__init__.py", String::new()),
        ("pkg/models.py", BUILDERS_MODEL.to_owned()),
        ("pkg/base.py", BUILDERS_BASE.to_owned()),
        ("pkg/use.py", BUILDERS_USE.to_owned()),
    ];
    let mut s = Session::start_with("lsp_oracle_builders", BUILDERS_CONFIG, files, false);
    let reported: &[(&str, &str)] = &[("pkg/use.py", "MUT204")];
    let steps = [
        Step::new("initial index", Vec::new()).present(reported),
        Step::new(
            "the class no longer derives from the model base: its builders are unknown",
            vec![save("pkg/models.py", BUILDERS_PLAIN)],
        )
        .absent(reported),
        Step::new(
            "it derives from a project base deriving from the model base",
            vec![save("pkg/models.py", BUILDERS_ON_BASE)],
        )
        .present(reported),
        Step::new(
            "the project base defines the builders over the library's",
            vec![save("pkg/base.py", BUILDERS_OWN)],
        )
        .absent(reported),
        Step::new(
            "it inherits the library's again",
            vec![save("pkg/base.py", BUILDERS_BASE)],
        )
        .present(reported),
    ];
    let mut failures = Vec::new();
    for step in &steps {
        failures.extend(s.run(step));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    s.lsp.stop(true);
}

// ------------------------------------------- accesses through protocols a contract names

const PROTOCOL_CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT201", "MUT203"]

[[tool.heki.write-owners]]
name = "writes"
targets = ["pkg.base.Parser._*"]
forbidden-writers = ["pkg.render:*"]
allowed-protocols = ["*Host"]

[[tool.heki.read-owners]]
name = "reads"
targets = ["pkg.base.Parser._*"]
forbidden-readers = ["pkg.render:*"]
allowed-protocols = ["*Host"]
"#;

const PROTOCOL_BASE: &str = "class Parser:\n    def __init__(self) -> None:\n        self._graph_builder = None\n        self._flag = False\n";
const PROTOCOL_HOST: &str = "from typing import Protocol\n\n\nclass RenderHost(Protocol):\n    _graph_builder: object\n    _flag: bool\n";
const PROTOCOL_RENDER: &str = "from pkg.host import RenderHost\n\n\ndef mark(parser: RenderHost) -> None:\n    parser._graph_builder = None\n\n\ndef peek(parser: RenderHost) -> object:\n    return parser._graph_builder\n";

/// `allowed-protocols` with the protocol in another module than the accesses through it: whether
/// it declares the member decides for the write (`TypedFieldWriteSite::protocol`) and the read,
/// as the protocol drops the declaration and declares it again.
#[test]
fn lsp_diagnostics_equal_a_full_check_through_protocols_a_contract_names() {
    let files = vec![
        ("pkg/__init__.py", String::new()),
        ("pkg/base.py", PROTOCOL_BASE.to_owned()),
        ("pkg/host.py", PROTOCOL_HOST.to_owned()),
        ("pkg/render.py", PROTOCOL_RENDER.to_owned()),
    ];
    let mut s = Session::start_with(
        "lsp_oracle_allowed_protocols",
        PROTOCOL_CONFIG,
        files,
        false,
    );
    let reported: &[(&str, &str)] = &[("pkg/render.py", "MUT201"), ("pkg/render.py", "MUT203")];
    let steps = [
        Step::new("initial index", Vec::new()).absent(reported),
        Step::new(
            "the protocol stops declaring the member",
            vec![save(
                "pkg/host.py",
                &PROTOCOL_HOST.replace("    _graph_builder: object\n", ""),
            )],
        )
        .present(reported),
        Step::new(
            "the protocol declares it again",
            vec![save("pkg/host.py", PROTOCOL_HOST)],
        )
        .absent(reported),
    ];
    let mut failures = Vec::new();
    for step in &steps {
        failures.extend(s.run(step));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    s.lsp.stop(true);
}

// ------------------------------------------ targets reached through the protocols they implement

const IMPLEMENTED_CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT203", "MUT204"]

[[tool.heki.call-owners]]
name = "build"
targets = ["pkg.impl:Impl.build"]
allowed-callers = ["pkg.impl:*"]

[[tool.heki.read-owners]]
name = "table"
targets = ["pkg.impl.Impl.table"]
allowed-readers = ["pkg.impl:*"]
"#;

const IMPLEMENTED_HOST: &str = "from typing import Protocol\n\n\nclass Host(Protocol):\n    table: dict\n\n    def build(self) -> None: ...\n";
const IMPLEMENTED_IMPL: &str = "class Impl:\n    def __init__(self) -> None:\n        self.table = {}\n\n    def build(self) -> None:\n        pass\n";
const IMPLEMENTED_OTHER: &str =
    "class Other:\n    def __init__(self) -> None:\n        self.table = {}\n";
const IMPLEMENTED_USE: &str = "from pkg.host import Host\n\n\ndef through(h: Host) -> None:\n    h.build()\n    print(h.table)\n";

/// A read and a call through a protocol are a target's while the target's class is the one
/// implementing the protocol: the accessing module stays as it is while a class of another module
/// starts implementing the protocol and stops again, and the target's class stops and starts.
#[test]
fn lsp_diagnostics_equal_a_full_check_as_classes_start_and_stop_implementing_a_protocol() {
    let files = vec![
        ("pkg/__init__.py", String::new()),
        ("pkg/host.py", IMPLEMENTED_HOST.to_owned()),
        ("pkg/impl.py", IMPLEMENTED_IMPL.to_owned()),
        ("pkg/other.py", IMPLEMENTED_OTHER.to_owned()),
        ("pkg/use.py", IMPLEMENTED_USE.to_owned()),
    ];
    let mut s = Session::start_with(
        "lsp_oracle_protocol_implementations",
        IMPLEMENTED_CONFIG,
        files,
        false,
    );
    let reported: &[(&str, &str)] = &[("pkg/use.py", "MUT203"), ("pkg/use.py", "MUT204")];
    let other_implements = IMPLEMENTED_OTHER.replace(
        "        self.table = {}\n",
        "        self.table = {}\n\n    def build(self) -> None:\n        pass\n",
    );
    let steps = [
        Step::new("initial index", Vec::new()).present(reported),
        Step::new(
            "a class outside the target's family starts implementing the protocol",
            vec![save("pkg/other.py", &other_implements)],
        )
        .absent(reported),
        Step::new(
            "it stops implementing it",
            vec![save("pkg/other.py", IMPLEMENTED_OTHER)],
        )
        .present(reported),
        Step::new(
            "the target's class stops implementing the protocol",
            vec![save(
                "pkg/impl.py",
                &IMPLEMENTED_IMPL.replace("        self.table = {}\n", "        self.rows = {}\n"),
            )],
        )
        .absent(reported),
        Step::new(
            "it implements it again",
            vec![save("pkg/impl.py", IMPLEMENTED_IMPL)],
        )
        .present(reported),
    ];
    let mut failures = Vec::new();
    for step in &steps {
        failures.extend(s.run(step));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    s.lsp.stop(true);
}

const STATIC_MEMBERS_CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT204"]

[[tool.heki.call-owners]]
name = "members"
targets = ["pkg.impl:Impl.build", "pkg.impl:Impl.make"]
allowed-callers = ["pkg.impl:*"]
"#;

const STATIC_MEMBERS_HOST: &str = "from typing import Protocol\n\n\nclass Host(Protocol):\n    size: int\n\n    @staticmethod\n    def build() -> None: ...\n\n    @classmethod\n    def make(cls) -> None: ...\n";
const STATIC_MEMBERS_IMPL: &str = "class Impl:\n    def __init__(self) -> None:\n        self.size = 0\n\n    @staticmethod\n    def build() -> None:\n        pass\n\n    @classmethod\n    def make(cls) -> None:\n        pass\n";
const STATIC_MEMBERS_OTHER: &str =
    "class Other:\n    def __init__(self) -> None:\n        self.size = 0\n";
const STATIC_MEMBERS_USE: &str =
    "from pkg.host import Host\n\n\ndef through(h: Host) -> None:\n    h.build()\n    h.make()\n";

/// The static and class methods called through a protocol are a target's while the target's
/// class alone implements the protocol, as the receiver tells the protocol: the calling module
/// stays as it is while a class of another module starts implementing the protocol and stops
/// again, and the protocol stops declaring the methods and declares them again.
#[test]
fn lsp_diagnostics_equal_a_full_check_for_static_and_class_members_through_a_protocol() {
    let files = vec![
        ("pkg/__init__.py", String::new()),
        ("pkg/host.py", STATIC_MEMBERS_HOST.to_owned()),
        ("pkg/impl.py", STATIC_MEMBERS_IMPL.to_owned()),
        ("pkg/other.py", STATIC_MEMBERS_OTHER.to_owned()),
        ("pkg/use.py", STATIC_MEMBERS_USE.to_owned()),
    ];
    let mut s = Session::start_with(
        "lsp_oracle_protocol_static_members",
        STATIC_MEMBERS_CONFIG,
        files,
        false,
    );
    let reported: &[(&str, &str)] = &[("pkg/use.py", "MUT204")];
    let other_implements = STATIC_MEMBERS_OTHER.replace(
        "        self.size = 0\n",
        "        self.size = 0\n\n    @staticmethod\n    def build() -> None:\n        pass\n\n    @classmethod\n    def make(cls) -> None:\n        pass\n",
    );
    let undeclared = STATIC_MEMBERS_HOST
        .split("\n\n    @staticmethod")
        .next()
        .unwrap_or_default()
        .to_owned()
        + "\n";
    let steps = [
        Step::new("initial index", Vec::new()).present(reported),
        Step::new(
            "a class outside the target's family starts implementing the protocol",
            vec![save("pkg/other.py", &other_implements)],
        )
        .absent(reported),
        Step::new(
            "it stops implementing it",
            vec![save("pkg/other.py", STATIC_MEMBERS_OTHER)],
        )
        .present(reported),
        Step::new(
            "the protocol stops declaring the static and class methods",
            vec![save("pkg/host.py", &undeclared)],
        ),
        Step::new(
            "it declares them again",
            vec![save("pkg/host.py", STATIC_MEMBERS_HOST)],
        )
        .present(reported),
    ];
    let mut failures = Vec::new();
    for step in &steps {
        failures.extend(s.run(step));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    s.lsp.stop(true);
}

// ------------------------------------- `__dict__` keys another `write-owners` contract names

const LISTED_KEYS_CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT201"]

[[tool.heki.write-owners]]
name = "caches"
targets = ["pkg.model.Field._cache_key"]
allowed-writers = ["pkg.store:*"]
unlisted-dict-keys = "report"

[[tool.heki.write-owners]]
name = "types"
targets = ["pkg.base.Base._type_cache"]
allowed-writers = ["pkg.parser:*"]
"#;

const LISTED_KEYS_BASE: &str = "class Base:\n    pass\n";
const LISTED_KEYS_MODEL: &str = "from pkg.base import Base\n\n\nclass Field(Base):\n    pass\n";
const LISTED_KEYS_PARSER: &str = "from pkg.model import Field\n\n\ndef cache(field: Field) -> None:\n    field.__dict__[\"_type_cache\"] = 1\n";

/// `unlisted-dict-keys` with the key named by another contract's target on a base class in
/// another module: whether the written object's class derives from it decides whether the key is
/// listed, as the class drops the base and takes it again.
#[test]
fn lsp_diagnostics_equal_a_full_check_for_keys_another_contract_names() {
    let files = vec![
        ("pkg/__init__.py", String::new()),
        ("pkg/base.py", LISTED_KEYS_BASE.to_owned()),
        ("pkg/model.py", LISTED_KEYS_MODEL.to_owned()),
        ("pkg/parser.py", LISTED_KEYS_PARSER.to_owned()),
    ];
    let mut s = Session::start_with("lsp_oracle_listed_keys", LISTED_KEYS_CONFIG, files, false);
    let reported: &[(&str, &str)] = &[("pkg/parser.py", "MUT201")];
    let steps = [
        Step::new("initial index", Vec::new()).absent(reported),
        Step::new(
            "the class stops deriving from the base the other contract names",
            vec![save(
                "pkg/model.py",
                &LISTED_KEYS_MODEL.replace("class Field(Base):", "class Field:"),
            )],
        )
        .present(reported),
        Step::new(
            "the class derives from the base again",
            vec![save("pkg/model.py", LISTED_KEYS_MODEL)],
        )
        .absent(reported),
    ];
    let mut failures = Vec::new();
    for step in &steps {
        failures.extend(s.run(step));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    s.lsp.stop(true);
}

// ------------------------- `__dict__` keys no target names, on an object of unknown class

const UNKNOWN_CLASS_KEYS_CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT201", "MUT205"]

[[tool.heki.write-owners]]
name = "caches"
targets = ["pkg.model.Field._type_cache"]
allowed-writers = ["pkg.store:*"]
unlisted-dict-keys = "report"
unresolved = "report"
"#;

const UNKNOWN_CLASS_KEYS_MODEL: &str = "class Field:\n    pass\n\n\nclass Other:\n    pass\n";
const UNKNOWN_CLASS_KEYS_PASSES: &str =
    "def fill(obj) -> None:\n    obj.__dict__[\"_new_cache\"] = 1\n";
const UNKNOWN_CLASS_KEYS_USE: &str = "from pkg.model import Field, Other\nfrom pkg.passes import fill\n\n\ndef run(obj: Other) -> None:\n    fill(obj)\n";

/// `unlisted-dict-keys` with `unresolved`: a helper writing a key no target names on its
/// untyped parameter is reported when a caller in another module hands it an instance of a
/// target's class, and not when the only caller hands it another class, as the caller's
/// annotation changes and changes back.
#[test]
fn lsp_diagnostics_equal_a_full_check_for_new_keys_on_objects_of_unknown_class() {
    let files = vec![
        ("pkg/__init__.py", String::new()),
        ("pkg/model.py", UNKNOWN_CLASS_KEYS_MODEL.to_owned()),
        ("pkg/passes.py", UNKNOWN_CLASS_KEYS_PASSES.to_owned()),
        ("pkg/use.py", UNKNOWN_CLASS_KEYS_USE.to_owned()),
    ];
    let mut s = Session::start_with(
        "lsp_oracle_unknown_class_keys",
        UNKNOWN_CLASS_KEYS_CONFIG,
        files,
        false,
    );
    let reported: &[(&str, &str)] = &[("pkg/passes.py", "MUT205")];
    let steps = [
        Step::new("initial index", Vec::new()).absent(reported),
        Step::new(
            "the caller hands the helper an instance of the target's class",
            vec![save(
                "pkg/use.py",
                &UNKNOWN_CLASS_KEYS_USE.replace("obj: Other", "obj: Field"),
            )],
        )
        .present(reported),
        Step::new(
            "the caller hands it another class again",
            vec![save("pkg/use.py", UNKNOWN_CLASS_KEYS_USE)],
        )
        .absent(reported),
    ];
    let mut failures = Vec::new();
    for step in &steps {
        failures.extend(s.run(step));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    s.lsp.stop(true);
}

// ------------------------------------- `__dict__` keys a loop takes from a tuple of constants

const TUPLE_KEYS_CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT201"]

[[tool.heki.write-owners]]
name = "caches"
targets = ["pkg.model.Field._type_cache"]
allowed-writers = ["pkg.store:*"]
"#;

const TUPLE_KEYS_KEYS: &str = "TYPE_KEY = \"_type_cache\"\nNAME_KEY = \"_name_cache\"\n";
const TUPLE_KEYS_PASSES: &str = "from pkg.keys import NAME_KEY, TYPE_KEY\nfrom pkg.model import Field\n\n\ndef reset(field: Field) -> None:\n    for key in (TYPE_KEY, NAME_KEY):\n        field.__dict__[key] = None\n";

/// A key a loop takes from a tuple of string constants another module defines writes each of
/// them: the write of the target is reported, not once the constant names another key, and again
/// when it names the target.
#[test]
fn lsp_diagnostics_equal_a_full_check_for_keys_from_a_tuple_of_constants() {
    let files = vec![
        ("pkg/__init__.py", String::new()),
        ("pkg/keys.py", TUPLE_KEYS_KEYS.to_owned()),
        ("pkg/model.py", "class Field:\n    pass\n".to_owned()),
        ("pkg/passes.py", TUPLE_KEYS_PASSES.to_owned()),
    ];
    let mut s = Session::start_with("lsp_oracle_tuple_keys", TUPLE_KEYS_CONFIG, files, false);
    let reported: &[(&str, &str)] = &[("pkg/passes.py", "MUT201")];
    let steps = [
        Step::new("initial index", Vec::new()).present(reported),
        Step::new(
            "the constant names another key",
            vec![save(
                "pkg/keys.py",
                &TUPLE_KEYS_KEYS.replace("\"_type_cache\"", "\"_kind_cache\""),
            )],
        )
        .absent(reported),
        Step::new(
            "it names the target again",
            vec![save("pkg/keys.py", TUPLE_KEYS_KEYS)],
        )
        .present(reported),
    ];
    let mut failures = Vec::new();
    for step in &steps {
        failures.extend(s.run(step));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    s.lsp.stop(true);
}

// ------------------------------------------- a target whose attribute is renamed and back

const RENAMED_TARGET_CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT201"]

[[tool.heki.write-owners]]
name = "caches"
targets = ["pkg.model.Field._cache"]
allowed-writers = ["pkg.store:*"]
"#;

const RENAMED_TARGET_MODEL: &str =
    "class Field:\n    def __init__(self) -> None:\n        self._cache: dict = {}\n";
const RENAMED_TARGET_USE: &str =
    "from pkg.model import Field\n\n\ndef reset(field: Field) -> None:\n    field._cache = {}\n";

/// A rename of the attribute a target names (the entry then matches nothing, which `heki check`
/// warns about apart from its reports) and back: the write in another module stops and starts
/// being a report, in the server as in a full check.
#[test]
fn lsp_diagnostics_equal_a_full_check_as_a_target_attribute_is_renamed() {
    let files = vec![
        ("pkg/__init__.py", String::new()),
        ("pkg/model.py", RENAMED_TARGET_MODEL.to_owned()),
        ("pkg/use.py", RENAMED_TARGET_USE.to_owned()),
    ];
    let mut s = Session::start_with(
        "lsp_oracle_renamed_target",
        RENAMED_TARGET_CONFIG,
        files,
        false,
    );
    let reported: &[(&str, &str)] = &[("pkg/use.py", "MUT201")];
    let renamed = |text: &str| text.replace("_cache", "_memo");
    let steps = [
        Step::new("initial index", Vec::new()).present(reported),
        Step::new(
            "the attribute is renamed where it is defined and written",
            vec![
                save("pkg/model.py", &renamed(RENAMED_TARGET_MODEL)),
                save("pkg/use.py", &renamed(RENAMED_TARGET_USE)),
            ],
        )
        .absent(reported),
        Step::new(
            "the old name comes back",
            vec![
                save("pkg/model.py", RENAMED_TARGET_MODEL),
                save("pkg/use.py", RENAMED_TARGET_USE),
            ],
        )
        .present(reported),
    ];
    let mut failures = Vec::new();
    for step in &steps {
        failures.extend(s.run(step));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    s.lsp.stop(true);
}

// ------------------------------------ allowed protocols with and without their subprotocols

const SUBPROTOCOL_CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT201", "MUT203"]

[[tool.heki.write-owners]]
name = "writes"
targets = ["pkg.*._*"]
forbidden-writers = ["pkg.render:*"]
allowed-protocols = ["*Host"]

[[tool.heki.read-owners]]
name = "reads"
targets = ["pkg.*._*"]
forbidden-readers = ["pkg.render:*"]
allowed-protocols = ["*Host"]
protocol-subclasses = false
"#;

const SUBPROTOCOL_BASE: &str =
    "class Parser:\n    def __init__(self) -> None:\n        self._flag = False\n";
const SUBPROTOCOL_HOST: &str =
    "from typing import Protocol\n\n\nclass GraphHost(Protocol):\n    _flag: bool\n";
const SUBPROTOCOL_FLAGS: &str =
    "from typing import Protocol\n\n\nclass RuntimeFlags(Protocol):\n    _flag: bool\n";
const SUBPROTOCOL_FLAGS_DERIVED: &str = "from typing import Protocol\n\nfrom pkg.host import GraphHost\n\n\nclass RuntimeFlags(GraphHost, Protocol):\n    _flag: bool\n";
const SUBPROTOCOL_RENDER: &str = "from pkg.flags import RuntimeFlags\n\n\ndef mark(flags: RuntimeFlags) -> None:\n    flags._flag = True\n\n\ndef peek(flags: RuntimeFlags) -> bool:\n    return flags._flag\n";

/// `allowed-protocols` covering subprotocols (the write contract) and naming the matching
/// protocols only (`protocol-subclasses = false`, the read contract), as the protocol used in
/// one module starts deriving from an allowed protocol of another and stops again.
#[test]
fn lsp_diagnostics_equal_a_full_check_through_subprotocols_of_allowed_protocols() {
    let files = vec![
        ("pkg/__init__.py", String::new()),
        ("pkg/base.py", SUBPROTOCOL_BASE.to_owned()),
        ("pkg/host.py", SUBPROTOCOL_HOST.to_owned()),
        ("pkg/flags.py", SUBPROTOCOL_FLAGS.to_owned()),
        ("pkg/render.py", SUBPROTOCOL_RENDER.to_owned()),
    ];
    let mut s = Session::start_with("lsp_oracle_subprotocols", SUBPROTOCOL_CONFIG, files, false);
    let both: &[(&str, &str)] = &[("pkg/render.py", "MUT201"), ("pkg/render.py", "MUT203")];
    let steps = [
        Step::new("initial index", Vec::new()).present(both),
        Step::new(
            "the protocol starts deriving from an allowed protocol",
            vec![save("pkg/flags.py", SUBPROTOCOL_FLAGS_DERIVED)],
        )
        .absent(&[("pkg/render.py", "MUT201")])
        .present(&[("pkg/render.py", "MUT203")]),
        Step::new(
            "the protocol derives from it no more",
            vec![save("pkg/flags.py", SUBPROTOCOL_FLAGS)],
        )
        .present(both),
    ];
    let mut failures = Vec::new();
    for step in &steps {
        failures.extend(s.run(step));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    s.lsp.stop(true);
}

// ---------------------------------------------- class bases named in their scope (session 4)

const SCOPED_BASES_CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT204"]

[[tool.heki.call-owners]]
name = "home"
targets = ["pkg.core.Rule"]
allowed-callers = ["pkg.core.*"]
references = "calls-and-bases"
"#;

const SCOPED_CORE: &str = "class Rule:\n    pass\n";
// A local alias of the imported class is the base; a parameter of its name is another value.
const SCOPED_SITE: &str = "from pkg.core import Rule\n\n\ndef through_alias():\n    Base = Rule\n\n    class Derived(Base):\n        pass\n\n    return Derived\n\n\ndef from_parameter(Rule):\n    class Derived(Rule):\n        pass\n\n    return Derived\n";

/// `references = "calls-and-bases"` with a base typed where its class statement runs: renaming
/// the guarded class in its own module turns off the base a function passes through a local
/// alias of the import, restoring it turns it on, and the alias becoming a parameter turns it off.
#[test]
fn lsp_diagnostics_equal_a_full_check_for_bases_named_in_their_scope() {
    let files = vec![
        ("pkg/__init__.py", String::new()),
        ("pkg/core.py", SCOPED_CORE.to_owned()),
        ("pkg/site.py", SCOPED_SITE.to_owned()),
    ];
    let mut s = Session::start_with("lsp_oracle_scoped_bases", SCOPED_BASES_CONFIG, files, false);
    let steps = [
        Step::new("initial index", Vec::new()).present(&[("pkg/site.py", "MUT204")]),
        Step::new(
            "the guarded class is renamed",
            vec![save(
                "pkg/core.py",
                &SCOPED_CORE.replace("class Rule", "class Slip"),
            )],
        )
        .absent(&[("pkg/site.py", "MUT204")]),
        Step::new(
            "the guarded class is restored",
            vec![save("pkg/core.py", SCOPED_CORE)],
        )
        .present(&[("pkg/site.py", "MUT204")]),
        Step::new(
            "the alias becomes a parameter",
            vec![save(
                "pkg/site.py",
                &SCOPED_SITE.replace(
                    "through_alias():\n    Base = Rule\n",
                    "through_alias(Base):\n",
                ),
            )],
        )
        .absent(&[("pkg/site.py", "MUT204")]),
    ];
    let mut failures = Vec::new();
    for step in &steps {
        failures.extend(s.run(step));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    s.lsp.stop(true);
}

// --------------------------------------------------------------------- reads of a guarded class

const READS_CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT204"]

[[tool.heki.call-owners]]
name = "home"
targets = ["pkg.core.Rule"]
allowed-callers = ["pkg.core.*"]
references = "reads"
"#;

const READS_CORE: &str = "class Rule:\n    pass\n";
const READS_OPS: &str =
    "from pkg.core import Rule\n\n\ndef _matches(value):\n    return isinstance(value, Rule)\n";
const READS_SITE: &str =
    "from pkg.ops import _matches\n\n\ndef check(value):\n    return _matches(value)\n";

/// `references = "reads"`: a private helper of another module reads the guarded class, which is
/// reported at its caller; the read turns into a class pattern, goes away, comes back, the class
/// is renamed and restored in its own module, and the key stops counting reads.
#[test]
fn lsp_diagnostics_equal_a_full_check_for_reads_of_a_guarded_class() {
    let files = vec![
        ("pkg/__init__.py", String::new()),
        ("pkg/core.py", READS_CORE.to_owned()),
        ("pkg/ops.py", READS_OPS.to_owned()),
        ("pkg/site.py", READS_SITE.to_owned()),
    ];
    let mut s = Session::start_with("lsp_oracle_reads", READS_CONFIG, files, false);
    let reported: &[(&str, &str)] = &[("pkg/site.py", "MUT204")];
    let helper = |body: &str| READS_OPS.replace("    return isinstance(value, Rule)\n", body);
    let steps = [
        Step::new("initial index", Vec::new()).present(reported),
        Step::new(
            "the helper matches a class pattern instead",
            vec![save(
                "pkg/ops.py",
                &helper("    match value:\n        case Rule():\n            return True\n    return False\n"),
            )],
        )
        .present(reported),
        Step::new(
            "the helper reads an attribute of its argument only",
            vec![save("pkg/ops.py", &helper("    return value.flag\n"))],
        )
        .absent(reported),
        Step::new(
            "the helper reads the class again",
            vec![save("pkg/ops.py", READS_OPS)],
        )
        .present(reported),
        Step::new(
            "the guarded class is renamed",
            vec![save(
                "pkg/core.py",
                &READS_CORE.replace("class Rule", "class Slip"),
            )],
        )
        .absent(reported),
        Step::new(
            "the guarded class is restored",
            vec![save("pkg/core.py", READS_CORE)],
        )
        .present(reported),
        Step::new(
            "the key counts bases only",
            vec![Action::Configure(
                READS_CONFIG.replace("\"reads\"", "\"calls-and-bases\""),
            )],
        )
        .absent(reported),
    ];
    let mut failures = Vec::new();
    for step in &steps {
        failures.extend(s.run(step));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    s.lsp.stop(true);
}

// --------------------------------------- the class hierarchy class bodies name in their scope

const HIERARCHY_CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT204"]

[[tool.heki.call-owners]]
name = "home"
targets = ["Rule.check"]
allowed-callers = ["pkg.core.*", "pkg.site.*"]
"#;

const HIERARCHY_CORE: &str = "class Rule:\n    def check(self):\n        return True\n";
// Classes nested in a class body: one inherits `Rule.check`, one overrides it.
const HIERARCHY_SITE: &str = "from pkg.core import Rule\n\n\nclass Holder:\n    class Inherits(Rule):\n        pass\n\n    class Overrides(Rule):\n        def check(self):\n            return False\n";
// `Holder.Inherits` finds `Rule.check` along its MRO, `Holder.Overrides` overrides it as a
// subclass of `Rule`.
const HIERARCHY_MRO: &str =
    "from pkg.site import Holder\n\n\ndef through_mro(d: Holder.Inherits):\n    return d.check()\n";
const HIERARCHY_FAMILY: &str = "from pkg.site import Holder\n\n\ndef through_family(d: Holder.Overrides):\n    return d.check()\n";

/// The bases of class statements in a class body, named where they run (`ClassDecl::bases`),
/// read by other files through the MRO and the subclass family: the class body binding the base's
/// name before its classes turns both reports off, binding it after them turns them on (Python
/// reads the module's name then), and binding it before them again turns them off.
#[test]
fn lsp_diagnostics_equal_a_full_check_while_a_class_body_shadows_a_base() {
    let files = vec![
        ("pkg/__init__.py", String::new()),
        ("pkg/core.py", HIERARCHY_CORE.to_owned()),
        ("pkg/site.py", HIERARCHY_SITE.to_owned()),
        ("pkg/use_mro.py", HIERARCHY_MRO.to_owned()),
        ("pkg/use_family.py", HIERARCHY_FAMILY.to_owned()),
    ];
    let shadowing =
        HIERARCHY_SITE.replace("class Holder:\n", "class Holder:\n    Rule = object\n\n");
    let mut s = Session::start_with("lsp_oracle_hierarchy_scope", HIERARCHY_CONFIG, files, false);
    let both = &[
        ("pkg/use_mro.py", "MUT204"),
        ("pkg/use_family.py", "MUT204"),
    ];
    let steps = [
        Step::new("initial index", Vec::new()).present(both),
        Step::new(
            "the class body binds the base's name before its classes",
            vec![save("pkg/site.py", &shadowing)],
        )
        .absent(both),
        Step::new(
            "the class body binds it after its classes",
            vec![save(
                "pkg/site.py",
                &format!("{HIERARCHY_SITE}\n    Rule = object\n"),
            )],
        )
        .present(both),
        Step::new(
            "the class body binds it before its classes again",
            vec![save("pkg/site.py", &shadowing)],
        )
        .absent(both),
    ];
    let mut failures = Vec::new();
    for step in &steps {
        failures.extend(s.run(step));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    s.lsp.stop(true);
}

// ----------------------------------------------------------- package paths an import goes through

const PATHS_CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "plugins"
scopes = ["app.loader"]
forbid = ["reads-environment"]
"#;

// A name under `app`, which an installed module may answer once a package under `app` changes
// its `__path__`, and a module of a package, which only a replaced path takes elsewhere.
const PATHS_LOADER: &str = "from importlib import import_module\n\n\ndef load(name):\n    return import_module(f\"app.{name}\")\n\n\ndef builtin():\n    return import_module(\"app.plugins.builtin\")\n";

/// The `__path__` of packages below the names a module imports (`ModuleDecls::path_changes`, read
/// across files through `ProjectDecls::changed_paths`): a package gains, changes, and loses a
/// redirect while another file imports through it, a package with one appears and disappears
/// below the imported name, and another module changes the package's path.
#[test]
fn lsp_diagnostics_equal_a_full_check_while_package_paths_change() {
    let files = vec![
        ("app/__init__.py", String::new()),
        ("app/plugins/__init__.py", String::new()),
        ("app/plugins/builtin.py", "VALUE = 1\n".to_owned()),
        ("app/loader.py", PATHS_LOADER.to_owned()),
    ];
    let mut s = Session::start_with("lsp_oracle_package_paths", PATHS_CONFIG, files, false);
    let steps = [
        Step::new("initial index", Vec::new()).absent(&[("app/loader.py", "EFF201")]),
        Step::new(
            "the package appends a directory to its path",
            vec![save("app/plugins/__init__.py", "__path__.append(\"/opt/plugins\")\n")],
        )
        .present(&[("app/loader.py", "EFF201")]),
        Step::new(
            "the package replaces its path",
            vec![save("app/plugins/__init__.py", "__path__ = [\"/opt/plugins\"]\n")],
        )
        .present(&[("app/loader.py", "EFF201")]),
        Step::new(
            "the package leaves its path alone again",
            vec![save("app/plugins/__init__.py", "")],
        )
        .absent(&[("app/loader.py", "EFF201")]),
        Step::new(
            "a package changing its path appears below the imported name",
            vec![Action::Create(
                "app/extra/__init__.py",
                "__path__.insert(0, \"/opt/extra\")\n".to_owned(),
            )],
        )
        .present(&[("app/loader.py", "EFF201")]),
        Step::new(
            "the package is deleted",
            vec![Action::Delete("app/extra/__init__.py")],
        )
        .absent(&[("app/loader.py", "EFF201")]),
        Step::new(
            "another module replaces the package's path",
            vec![Action::Create(
                "app/boot.py",
                "import app.plugins\n\napp.plugins.__path__ = [\"/opt/plugins\"]\n".to_owned(),
            )],
        )
        .present(&[("app/loader.py", "EFF201")]),
        Step::new(
            "the module only lists the package's modules",
            vec![save(
                "app/boot.py",
                "import pkgutil\n\nimport app.plugins\n\nPLUGINS = list(pkgutil.iter_modules(app.plugins.__path__))\n",
            )],
        )
        .absent(&[("app/loader.py", "EFF201")]),
    ];
    let mut failures = Vec::new();
    for step in &steps {
        failures.extend(s.run(step));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    s.lsp.stop(true);
}

// A constant passed by keyword through a package re-exporting it: whether the package binds the
// name again (`ModuleDecls::rebound_imports`) decides whether the import still holds the
// constant there.
const RELAYED_USE: &str = "from pkg.relay import LIMIT\nfrom pkg.sig import levels\n\n\ndef use():\n    return levels(level=LIMIT)\n";
const RELAY_REBINDS: &str =
    "from pkg.limits import LIMIT\n\n\ndef reset():\n    global LIMIT\n    LIMIT = 3\n";

#[test]
fn lsp_diagnostics_equal_a_full_check_through_a_re_export_its_package_binds_again() {
    let files = vec![
        ("pkg/__init__.py", String::new()),
        ("pkg/sig.py", MARKED_LEVELS.to_owned()),
        ("pkg/limits.py", "LIMIT = 0\n".to_owned()),
        (
            "pkg/relay/__init__.py",
            "from pkg.limits import LIMIT\n".to_owned(),
        ),
        ("pkg/use.py", RELAYED_USE.to_owned()),
    ];
    let mut s = Session::start_with("lsp_oracle_rebound_imports", MARKED_CONFIG, files, false);
    let reported: &[(&str, &str)] = &[("pkg/use.py", "MUT204")];
    let steps = [
        Step::new("initial index", Vec::new()).present(reported),
        Step::new(
            "the package re-exporting the constant binds the name again",
            vec![save("pkg/relay/__init__.py", RELAY_REBINDS)],
        )
        .absent(reported),
        Step::new(
            "it only re-exports it again",
            vec![save(
                "pkg/relay/__init__.py",
                "from pkg.limits import LIMIT\n",
            )],
        )
        .present(reported),
    ];
    let mut failures = Vec::new();
    for step in &steps {
        failures.extend(s.run(step));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    s.lsp.stop(true);
}

// ------------------------------------------------------ class objects across modules (session)

const CLASS_OBJECTS_CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT203", "MUT204"]

[[tool.heki.call-owners]]
name = "dict-builder"
targets = ["pkg.types.DataType(is_dict=True)"]
forbidden-callers = ["pkg.use:*"]

[[tool.heki.read-owners]]
name = "members"
targets = ["pkg.types.DataType.HOLDS_MEMBERS"]
forbidden-readers = ["pkg.use:*"]
"#;

const CLASS_OBJECTS_TYPES: &str = "class DataType:\n    HOLDS_MEMBERS = True\n\n    def __init__(self, *, is_dict: bool = False) -> None:\n        self.is_dict = is_dict\n";
// The property and the function returning a class object live in another module than their
// callers.
const CLASS_OBJECTS_BASE: &str = "from pkg.types import DataType\n\n\nclass Base:\n    @property\n    def data_type(self) -> type[DataType]:\n        return DataType\n\n\ndef kind() -> type[DataType]:\n    return DataType\n";
const CLASS_OBJECTS_USE: &str = "from pkg.base import Base, kind\nfrom pkg.types import DataType\n\n\nclass Use(Base):\n    def build(self) -> DataType:\n        return self.data_type(is_dict=True)\n\n\ndef returned() -> DataType:\n    return kind()(is_dict=True)\n\n\ndef read(model_type: type[DataType]) -> bool:\n    return model_type.HOLDS_MEMBERS\n";

/// A `type[C]` annotation names the class object of `C` from the declarations of other modules:
/// the property and the function returning one are declared in another module than the calls of
/// their values, and the class the annotation names becomes a protocol (whose class objects are
/// of no known class) and a class again.
#[test]
fn lsp_diagnostics_equal_a_full_check_for_class_objects_across_modules() {
    let files = vec![
        ("pkg/__init__.py", String::new()),
        ("pkg/types.py", CLASS_OBJECTS_TYPES.to_owned()),
        ("pkg/base.py", CLASS_OBJECTS_BASE.to_owned()),
        ("pkg/use.py", CLASS_OBJECTS_USE.to_owned()),
    ];
    let mut s = Session::start_with(
        "lsp_oracle_class_objects",
        CLASS_OBJECTS_CONFIG,
        files,
        false,
    );
    let both: &[(&str, &str)] = &[("pkg/use.py", "MUT203"), ("pkg/use.py", "MUT204")];
    let steps = [
        Step::new("initial index", Vec::new()).present(both),
        Step::new(
            "the property and the function return an instance",
            vec![save(
                "pkg/base.py",
                &CLASS_OBJECTS_BASE.replace("-> type[DataType]", "-> DataType"),
            )],
        )
        .present(&[("pkg/use.py", "MUT203")])
        .absent(&[("pkg/use.py", "MUT204")]),
        Step::new(
            "they return the class object again",
            vec![save("pkg/base.py", CLASS_OBJECTS_BASE)],
        )
        .present(both),
        Step::new(
            "the class becomes a protocol",
            vec![save(
                "pkg/types.py",
                &format!(
                    "from typing import Protocol\n\n\n{}",
                    CLASS_OBJECTS_TYPES.replace("class DataType:", "class DataType(Protocol):")
                ),
            )],
        )
        .absent(both),
        Step::new(
            "it is a class again",
            vec![save("pkg/types.py", CLASS_OBJECTS_TYPES)],
        )
        .present(both),
    ];
    let mut failures = Vec::new();
    for step in &steps {
        failures.extend(s.run(step));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    s.lsp.stop(true);
}

// The attribute is annotated in a base class of another module than the subclass calling it.
const CLASS_OBJECT_ATTRIBUTE_BASE: &str = "from pkg.types import DataType\n\n\nclass Parser:\n    def __init__(self) -> None:\n        self.field_type: type[DataType] = DataType\n";
const CLASS_OBJECT_ATTRIBUTE_USE: &str = "from pkg.base import Parser\nfrom pkg.types import DataType\n\n\nclass OpenAPIParser(Parser):\n    def build(self) -> DataType:\n        return self.field_type(is_dict=True)\n";

/// A call of a class object an attribute declares constructs its class from the declarations of
/// another module: the subclass calling it sees the base class's annotation change to one of no
/// known class and back.
#[test]
fn lsp_diagnostics_equal_a_full_check_for_class_object_attributes_across_modules() {
    let files = vec![
        ("pkg/__init__.py", String::new()),
        ("pkg/types.py", CLASS_OBJECTS_TYPES.to_owned()),
        ("pkg/base.py", CLASS_OBJECT_ATTRIBUTE_BASE.to_owned()),
        ("pkg/use.py", CLASS_OBJECT_ATTRIBUTE_USE.to_owned()),
    ];
    let mut s = Session::start_with(
        "lsp_oracle_class_object_attributes",
        CLASS_OBJECTS_CONFIG,
        files,
        false,
    );
    let constructs: &[(&str, &str)] = &[("pkg/use.py", "MUT204")];
    let steps = [
        Step::new("initial index", Vec::new()).present(constructs),
        Step::new(
            "the attribute is declared `Any`",
            vec![save(
                "pkg/base.py",
                &format!(
                    "from typing import Any\n\n{}",
                    CLASS_OBJECT_ATTRIBUTE_BASE.replace(": type[DataType] =", ": Any =")
                ),
            )],
        )
        .absent(constructs),
        Step::new(
            "it is declared a class object again",
            vec![save("pkg/base.py", CLASS_OBJECT_ATTRIBUTE_BASE)],
        )
        .present(constructs),
    ];
    let mut failures = Vec::new();
    for step in &steps {
        failures.extend(s.run(step));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    s.lsp.stop(true);
}

const MANAGER_CONFIG: &str = "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"EFF103\"]\n";
const MANAGER_MODELS: &str = "from django.db import models\n\nfrom pkg.managers import StreamManager\n\n\nclass Stream(models.Model):\n    objects = StreamManager()\n";
const MANAGER_CLASS: &str =
    "from django.db import models\n\n\nclass StreamManager(models.Manager):\n    pass\n";
const MANAGER_USE: &str = "from pkg.models import Stream\n\n\ndef fetch(ids: list[int]) -> None:\n    for i in ids:\n        Stream.objects.get(id=i)\n";

/// A model's manager is the class its body binds, which another module declares: the query per
/// item through it goes when the manager class stops deriving from `models.Manager`, and when
/// the model binds another value or another module stores one as its `objects`, is heuristic
/// while the manager binds `get_queryset`, and comes back.
#[test]
fn lsp_diagnostics_equal_a_full_check_for_model_managers_across_modules() {
    let files = vec![
        ("pkg/__init__.py", String::new()),
        ("pkg/managers.py", MANAGER_CLASS.to_owned()),
        ("pkg/models.py", MANAGER_MODELS.to_owned()),
        ("pkg/use.py", MANAGER_USE.to_owned()),
    ];
    let mut s = Session::start_with("lsp_oracle_model_managers", MANAGER_CONFIG, files, false);
    let queries: &[(&str, &str)] = &[("pkg/use.py", "EFF103")];
    let guessed: &[(&str, &str)] = &[("pkg/use.py", "EFF103 [heuristic]")];
    let steps = [
        Step::new("initial index", Vec::new()).present(queries),
        Step::new(
            "the manager class derives from no manager",
            vec![save(
                "pkg/managers.py",
                &MANAGER_CLASS.replace("(models.Manager)", ""),
            )],
        )
        .absent(queries),
        Step::new(
            "it derives from `models.Manager` again",
            vec![save("pkg/managers.py", MANAGER_CLASS)],
        )
        .present(queries),
        Step::new(
            "the manager binds `get_queryset`, which its query methods call",
            vec![save(
                "pkg/managers.py",
                &MANAGER_CLASS.replace(
                    "    pass\n",
                    "    def get_queryset(self):\n        return super().get_queryset()\n",
                ),
            )],
        )
        .present(guessed)
        .absent(queries),
        Step::new(
            "it binds none again",
            vec![save("pkg/managers.py", MANAGER_CLASS)],
        )
        .present(queries),
        Step::new(
            "the model binds another value",
            vec![save(
                "pkg/models.py",
                &MANAGER_MODELS.replace("StreamManager()", "None"),
            )],
        )
        .absent(queries),
        Step::new(
            "it binds the manager again",
            vec![save("pkg/models.py", MANAGER_MODELS)],
        )
        .present(queries),
        Step::new(
            "another module stores another object as the model's `objects`",
            vec![Action::Create(
                "pkg/patch.py",
                "from pkg.models import Stream\n\nStream.objects = None\n".to_owned(),
            )],
        )
        .absent(queries),
        Step::new(
            "that module is deleted",
            vec![Action::Delete("pkg/patch.py")],
        )
        .present(queries),
    ];
    let mut failures = Vec::new();
    for step in &steps {
        failures.extend(s.run(step));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    s.lsp.stop(true);
}

const OVERRIDE_MODELS: &str = "from django.db import models\n\n\nclass Stream(models.Model):\n    def save(self, *args, **kwargs):\n        self.touched = True\n        super().save(*args, **kwargs)\n";
const OVERRIDE_QUERYSETS: &str = "from django.db import models\n\n\nclass StreamQuerySet(models.QuerySet):\n    def live(self):\n        return self.filter(live=True)\n";
const OVERRIDE_USE: &str = "from pkg.models import Stream\n\n\ndef add(ids: list[int]) -> None:\n    for i in ids:\n        Stream.objects.create(id=i)\n\n\ndef fetch(ids: list[int]) -> None:\n    for i in ids:\n        Stream.objects.filter(id=i).get()\n";

/// A model's `save` behind `QuerySet.create`, and a project `QuerySet` subclass, in other
/// modules: the insert per item is heuristic while `save` may return without calling
/// `super().save()`, the lookup per item while the subclass binds a name `get` looks up, and
/// both come back.
#[test]
fn lsp_diagnostics_equal_a_full_check_for_django_overrides_across_modules() {
    let files = vec![
        ("pkg/__init__.py", String::new()),
        ("pkg/models.py", OVERRIDE_MODELS.to_owned()),
        ("pkg/querysets.py", OVERRIDE_QUERYSETS.to_owned()),
        ("pkg/use.py", OVERRIDE_USE.to_owned()),
    ];
    let mut s = Session::start_with("lsp_oracle_django_overrides", MANAGER_CONFIG, files, false);
    // `add()` creates on line 6, `fetch()` looks up on line 11.
    let exact: Reported = &[(6, "EFF103"), (11, "EFF103")];
    let steps = [
        Step::new("initial index", Vec::new()).reported("pkg/use.py", exact),
        Step::new(
            "`save` returns early on one path",
            vec![save(
                "pkg/models.py",
                &OVERRIDE_MODELS.replace(
                    "        self.touched = True\n",
                    "        if self.touched:\n            return\n",
                ),
            )],
        )
        .reported("pkg/use.py", &[(6, "EFF103 [heuristic]"), (11, "EFF103")]),
        Step::new(
            "it calls `super().save()` on every path again",
            vec![save("pkg/models.py", OVERRIDE_MODELS)],
        )
        .reported("pkg/use.py", exact),
        Step::new(
            "the queryset binds `_fetch_all`",
            vec![save(
                "pkg/querysets.py",
                &OVERRIDE_QUERYSETS.replace("    def live(self):", "    def _fetch_all(self):"),
            )],
        )
        .reported("pkg/use.py", &[(6, "EFF103"), (11, "EFF103 [heuristic]")]),
        Step::new(
            "it only adds a method again",
            vec![save("pkg/querysets.py", OVERRIDE_QUERYSETS)],
        )
        .reported("pkg/use.py", exact),
    ];
    let mut failures = Vec::new();
    for step in &steps {
        failures.extend(s.run(step));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    s.lsp.stop(true);
}
