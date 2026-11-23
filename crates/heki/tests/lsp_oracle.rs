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
//! severity, certainty, message (labelled for a heuristic rule), related locations, and help (the
//! last related entry, at the diagnostic's own range) of every diagnostic. A step also names diagnostics
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
extend-select = ["MUT101", "MUT107", "MUT108", "MUT111", "EFF107", "MUT112", "MUT113", "EFF109", "EFF110", "EFF112", "EFF114", "EFF115", "MUT115", "MUT118", "MUT119"]

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
// (or an `@lru_cache` result), and stops again. `MUT109`: a helper in another module stops
// announcing a change its caller makes, and a caller in another module stops announcing after
// calling a public writer.
const CATALOG: &str = "from functools import lru_cache\n\n\nclass Catalog:\n    def entry(self, key) -> dict:\n        return {}\n\n    def fresh(self, key) -> dict:\n        return {}\n\n\n@lru_cache\ndef defaults() -> dict:\n    return {}\n";
const LOOKUP: &str = "from pkg.catalog import Catalog, defaults\n\n\ndef find(c: Catalog, key) -> dict:\n    return c.fresh(key)\n";
const LOOKUP_SHARED: &str = "from pkg.catalog import Catalog, defaults\n\n\ndef find(c: Catalog, key) -> dict:\n    return c.entry(key)\n";
const LOOKUP_CACHED: &str = "from pkg.catalog import Catalog, defaults\n\n\ndef find(c: Catalog, key) -> dict:\n    return defaults()\n";
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

// Exact effect reports (`EFF101` / `EFF111`): an `async def` calls a method on a base class whose
// implementation always blocks; a subclass in a third module starts overriding it without the
// blocking call, then with another one, and the base's call becomes conditional.
const EXACT_BASE: &str = "import requests\n\n\nclass Base:\n    def fetch(self, url):\n        return requests.get(url)\n";
const EXACT_BASE_CONDITIONAL: &str = "import requests\n\n\nclass Base:\n    def fetch(self, url):\n        if url:\n            return requests.get(url)\n        return None\n";
const EXACT_SUB: &str = "class Quiet:\n    def fetch(self, url):\n        return url\n";
const EXACT_SUB_OVERRIDING: &str = "from pkg.exact_base import Base\n\n\nclass Quiet(Base):\n    def fetch(self, url):\n        return url\n";
const EXACT_SUB_BLOCKING: &str = "import requests\n\nfrom pkg.exact_base import Base\n\n\nclass Quiet(Base):\n    def fetch(self, url):\n        return requests.post(url)\n";
const EXACT_USE: &str =
    "from pkg.exact_base import Base\n\n\nasync def serve(b: Base):\n    b.fetch(\"u\")\n";
// The third module binds the base's method to another function, and the base's class body
// binds it again: the call may run that function instead.
const EXACT_SUB_REBINDING: &str = "from pkg import exact_base\n\n\ndef quiet(self, url):\n    return url\n\n\nsetattr(exact_base.Base, \"fetch\", quiet)\n";
const EXACT_BASE_REBOUND: &str = "import requests\n\n\ndef _quiet(self, url):\n    return url\n\n\nclass Base:\n    def fetch(self, url):\n        return requests.get(url)\n\n    fetch = _quiet\n";

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
const WIT_SERVE: &str = "from pkg.wit_mid import load\nfrom pkg.wit_repo import Repo\n\n\nasync def serve(r: Repo):\n    load(r)\n";

// Queue capacities (`EFF111` on `queue.Queue.put`, a wait on another thread): a queue one module builds and another
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
const REC_WALK: &str = "def descend(node):\n    return len(node)\n";
const REC_WALK_BACK: &str = "from pkg import rec_handler\n\n\ndef descend(node):\n    return sum(rec_handler.visit(child) for child in node)\n";
// A function of another module handed to a thread and to `filter` (`EFF115`): it changes between
// `def` and `async def` there.
const CORO_JOBS: &str = "def flush(n):\n    return n\n";
const CORO_JOBS_ASYNC: &str = "async def flush(n):\n    return n\n";
const CORO_USE: &str = "import threading\n\nfrom pkg.coro_jobs import flush\n\n\ndef start(items):\n    threading.Thread(target=flush, args=(1,)).start()\n    return list(filter(flush, items))\n";

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
        ("pkg/lock_pool.py", LOCK_POOL),
        ("pkg/lock_use.py", LOCK_USE),
        ("pkg/fan_read.py", FAN_READ),
        ("pkg/fan_use.py", FAN_USE),
        ("pkg/order_src.py", ORDER_SRC),
        ("pkg/order_use.py", ORDER_USE),
        ("pkg/rec_handler.py", REC_HANDLER),
        ("pkg/rec_walk.py", REC_WALK),
        ("pkg/coro_jobs.py", CORO_JOBS),
        ("pkg/coro_use.py", CORO_USE),
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
    /// The edit changes a fact callers read, though no diagnostic shows it: the server
    /// re-analyzes a caller's file (the opposite of `cutoff`).
    propagates: bool,
}

impl Step {
    fn new(name: &'static str, actions: Vec<Action>) -> Self {
        Self {
            name,
            actions,
            present: &[],
            absent: &[],
            cutoff: false,
            propagates: false,
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

    fn propagates(mut self) -> Self {
        self.propagates = true;
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
/// (`EffectSummary::must_exec`, compared by Early Cutoff).
fn steps_exact_effects() -> Vec<Step> {
    vec![
        Step::new(
            "a subclass in another file overrides the method without blocking",
            vec![save("pkg/exact_sub.py", EXACT_SUB_OVERRIDING)],
        )
        .present(&[("pkg/exact_use.py", "EFF111")])
        .absent(&[("pkg/exact_use.py", "EFF101")]),
        Step::new(
            "the override blocks too",
            vec![save("pkg/exact_sub.py", EXACT_SUB_BLOCKING)],
        )
        .present(&[("pkg/exact_use.py", "EFF101")])
        .absent(&[("pkg/exact_use.py", "EFF111")]),
        Step::new(
            "the base method blocks only on some paths",
            vec![save("pkg/exact_base.py", EXACT_BASE_CONDITIONAL)],
        )
        .present(&[("pkg/exact_use.py", "EFF111")])
        .absent(&[("pkg/exact_use.py", "EFF101")]),
        Step::new(
            "the base method blocks on every path again, and the override stops overriding",
            vec![
                save("pkg/exact_base.py", EXACT_BASE),
                save("pkg/exact_sub.py", EXACT_SUB),
            ],
        )
        .present(&[("pkg/exact_use.py", "EFF101")])
        .absent(&[("pkg/exact_use.py", "EFF111")]),
        // `ModuleDecls::rebound_globals` of another module and `ModuleDecls::rebound_definitions`
        // of the base's: a name bound again leaves the call's targets incomplete
        // (`ProjectDecls::lookup_rebound`).
        Step::new(
            "the third module binds the base's method to another function",
            vec![save("pkg/exact_sub.py", EXACT_SUB_REBINDING)],
        )
        .present(&[("pkg/exact_use.py", "EFF111")])
        .absent(&[("pkg/exact_use.py", "EFF101")]),
        Step::new(
            "it stops binding the method",
            vec![save("pkg/exact_sub.py", EXACT_SUB)],
        )
        .present(&[("pkg/exact_use.py", "EFF101")])
        .absent(&[("pkg/exact_use.py", "EFF111")]),
        Step::new(
            "the base's class body binds the method again",
            vec![save("pkg/exact_base.py", EXACT_BASE_REBOUND)],
        )
        .present(&[("pkg/exact_use.py", "EFF111")])
        .absent(&[("pkg/exact_use.py", "EFF101")]),
        Step::new(
            "the base binds it once again",
            vec![save("pkg/exact_base.py", EXACT_BASE)],
        )
        .present(&[("pkg/exact_use.py", "EFF101")])
        .absent(&[("pkg/exact_use.py", "EFF111")]),
    ]
}

/// A global's and an attribute's type is what every store of it binds: a store of another class
/// in another function of the module, in a third module, in a subclass or a mixin of it in
/// another file, or through a parameter of the class makes the calls on it unresolved, which the
/// language server sees in every reader; so does a store of another class into a global that
/// the stores of another global read.
fn steps_store_types() -> Vec<Step> {
    const READERS: &[(&str, &str)] = &[("pkg/st_client.py", "EFF101"), ("pkg/st_use.py", "EFF101")];
    const HOLDER: &[(&str, &str)] = &[("pkg/st_holder.py", "EFF101")];
    const ACTIVE: &[(&str, &str)] = &[("pkg/st_active.py", "EFF101")];
    const ATTR_ACTIVE: &[(&str, &str)] = &[("pkg/st_attr_active.py", "EFF101")];
    const STAR: &[(&str, &str)] = &[("pkg/st_star.py", "EFF101")];
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
    ]
}

/// `report-unused-suppressions`: a suppression of `EFF101` in a caller's file turns unused, and
/// back, as a subclass in another file stops and starts blocking (`unused-suppression` follows
/// the reports of other files' changes, as the reports it judges do).
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
            "a subclass in another file overrides the method without blocking: the suppression is unused",
            vec![save("pkg/exact_sub.py", EXACT_SUB_OVERRIDING)],
        )
        .present(&[
            ("pkg/exact_use.py", "unused-suppression"),
            ("pkg/exact_use.py", "EFF111"),
        ]),
        Step::new(
            "the override blocks too: the suppression is used again",
            vec![save("pkg/exact_sub.py", EXACT_SUB_BLOCKING)],
        )
        .absent(&[
            ("pkg/exact_use.py", "EFF101"),
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
        .present(&[("pkg/ml_use.py", "EFF111")])
        .absent(&[("pkg/ml_use.py", "EFF101")]),
        Step::new(
            "the subclass overrides it with a method",
            vec![save("pkg/ml_shadow.py", ML_SHADOW_METHOD)],
        )
        .present(&[("pkg/ml_use.py", "EFF101")])
        .absent(&[("pkg/ml_use.py", "EFF111")]),
        Step::new(
            "the subclass defines `__getattribute__`",
            vec![save("pkg/ml_shadow.py", ML_SHADOW_HOOKED)],
        )
        .present(&[("pkg/ml_use.py", "EFF111")])
        .absent(&[("pkg/ml_use.py", "EFF101")]),
        Step::new(
            "the subclass is deleted",
            vec![Action::Delete("pkg/ml_shadow.py")],
        )
        .present(&[("pkg/ml_use.py", "EFF101")])
        .absent(&[("pkg/ml_use.py", "EFF111")]),
    ]
}

const PV: &str = "class Pv:\n    def __run(self) -> int:\n        return 1\n\n    def go(self) -> int:\n        return self.__run()\n";
const PV_USE: &str = "from pkg.pv import Pv\n\n\ndef use(p: Pv) -> int:\n    return p._Pv__run()\n";
const PV_SUB: &str = "from pkg.pv import Pv\n\n\nclass Sub(Pv):\n    def __run(self) -> int:\n        return 2\n\n    def own(self) -> int:\n        return self.__run()\n\n    def peek(self) -> int:\n        return self._Pv__run()\n";

/// A private method (`def __run`, the member `_Pv__run`) called by its mangled name from other
/// modules: the call follows the class's declarations when the method is renamed and back, and a
/// subclass's own `__run` is another method, which `self.__run` in the subclass calls.
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
            "the files and the contract are removed",
            vec![
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
        .absent(&[("pkg/wit_serve.py", "EFF111")]),
        Step::new(
            "it stops waiting",
            vec![save("pkg/wit_local.py", WIT_LOCAL_QUIET)],
        )
        .present(&[("pkg/wit_serve.py", "EFF111")])
        .absent(&[("pkg/wit_serve.py", "EFF101")]),
        Step::new("it waits again", vec![save("pkg/wit_local.py", WIT_LOCAL)])
            .present(&[("pkg/wit_serve.py", "EFF101")])
            .absent(&[("pkg/wit_serve.py", "EFF111")]),
        Step::new(
            "the helper's parameter names only the implementation on its shortest chain",
            vec![save("pkg/wit_fetch.py", WIT_FETCH_REMOTE)],
        )
        .present(&[("pkg/wit_serve.py", "EFF101")])
        .absent(&[("pkg/wit_serve.py", "EFF111")]),
        Step::new(
            "the helper's parameter names the base class again",
            vec![save("pkg/wit_fetch.py", WIT_FETCH)],
        )
        .present(&[("pkg/wit_serve.py", "EFF101")])
        .absent(&[("pkg/wit_serve.py", "EFF111")]),
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
        .present(&[("pkg/worker_view.py", "EFF113")])
        .absent(&[("pkg/worker_view.py", "EFF111")]),
        Step::new(
            "the worker's query is removed",
            vec![save("pkg/worker_ops.py", WORKER_OPS)],
        )
        .absent(&[("pkg/worker_view.py", "EFF113")]),
        // A function a builtin calls once per item: its caller is re-checked when the
        // function in another module starts or stops querying.
        Step::new(
            "the sort key in another module starts a query per item",
            vec![Action::Save(
                "pkg/rank_key.py",
                RANK_KEY.replace("    return i\n", "    return session.execute(i)\n"),
            )],
        )
        .present(&[("pkg/rank_user.py", "EFF113")]),
        Step::new(
            "the sort key's query is removed",
            vec![save("pkg/rank_key.py", RANK_KEY)],
        )
        .absent(&[("pkg/rank_user.py", "EFF113")]),
        Step::new(
            "a helper becomes a generator: the report moves to the iteration",
            vec![save("pkg/feed.py", FEED_GENERATOR)],
        )
        .present(&[("pkg/feed_user.py", "EFF111")]),
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
/// of the helper; whether a parameter is typed, and with which class, is a fact of
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

/// `MUT202` / `MUT114` return flow and `MUT109`: facts of a function in another module that a
/// caller's report reads (the wrapper's protected return, the helper's announcement), and a
/// caller's announcement after a call that a callee's report reads.
fn steps_mut114_mut202_mut109() -> Vec<Step> {
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
        .present(&[("pkg/q_publish.py", "EFF111")]),
        Step::new(
            "it builds the queue unbounded again",
            vec![save("pkg/q_owner.py", Q_OWNER)],
        )
        .absent(&[("pkg/q_publish.py", "EFF111")]),
        Step::new(
            "the caller of an async def putting on its parameter passes an unbounded queue",
            vec![save(
                "pkg/q_run.py",
                &Q_RUN.replace("queue.Queue(5)", "queue.Queue()"),
            )],
        )
        .absent(&[("pkg/q_drain.py", "EFF111")]),
        Step::new(
            "it passes a bounded queue again",
            vec![save("pkg/q_run.py", Q_RUN)],
        )
        .present(&[("pkg/q_drain.py", "EFF111")]),
        Step::new(
            "a subclass in another module defines the constant bounding the queue again",
            vec![save(
                "pkg/q_small.py",
                &Q_SMALL.replace("label = \"small\"", "_MAX = 0"),
            )],
        )
        .absent(&[("pkg/q_halt.py", "EFF111")]),
        Step::new(
            "the subclass leaves the constant to its base again",
            vec![save("pkg/q_small.py", Q_SMALL)],
        )
        .present(&[("pkg/q_halt.py", "EFF111")]),
        Step::new(
            "the module a global copies its queue from, and a function imports it from, bounds it",
            vec![save(
                "pkg/q_events.py",
                &Q_EVENTS.replace("queue.Queue()", "queue.Queue(4)"),
            )],
        )
        .present(&[("pkg/q_mirror.py", "EFF111"), ("pkg/q_local.py", "EFF111")]),
        Step::new(
            "it builds the queue unbounded again",
            vec![save("pkg/q_events.py", Q_EVENTS)],
        )
        .absent(&[("pkg/q_mirror.py", "EFF111"), ("pkg/q_local.py", "EFF111")]),
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
/// only while the rule is selected (`OptInFacts`).
fn steps_eff109() -> Vec<Step> {
    let more = LOCK_POOL.replace(
        "            return 1\n\n\ndef refill",
        "            return 1\n\n    def checked_size(self):\n        if self.closed:\n            return 0\n        return self.size()\n\n\ndef reset():\n    global LOCK\n    LOCK = threading.Lock()\n\n\ndef refill",
    );
    let resetting = "from pkg.lock_pool import LOCK, Pool, refill, reset\n\n\nclass Spool(Pool):\n    def grow(self):\n        return self.size()\n\n\ndef drain():\n    with LOCK:\n        reset()\n        return refill(force=True)\n";
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
            "EFF109 is no longer selected, and the helper's module changes",
            vec![
                Action::Configure(CONFIG.replace(", \"EFF109\"", "")),
                Action::Save("pkg/lock_pool.py", format!("# Pools.\n{LOCK_POOL}")),
            ],
        )
        .absent(&[("pkg/lock_use.py", "EFF109")]),
        Step::new(
            "EFF109 is selected again",
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

/// The initial index.
fn steps_initial_index() -> Vec<Step> {
    vec![Step::new("initial index", Vec::new()).present(&[
        ("pkg/ops.py", "MUT201"),
        ("pkg/ops.py", "MUT203"),
        ("pkg/svc_user.py", "EFF101"),
        ("pkg/guard_use.py", "EFF102"),
        ("pkg/guard_default_use.py", "EFF102"),
        ("pkg/guard_conn_use.py", "EFF102"),
        ("pkg/buf_use.py", "EFF111"),
        ("pkg/buf_default_use.py", "EFF111"),
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
        .present(&[("pkg/buf_default_use.py", "EFF111")])
        .absent(&[("pkg/buf_use.py", "EFF111")]),
        Step::new(
            "the reader tests an attribute a buffer may lack",
            vec![save(
                "pkg/buf_read.py",
                &BUF_READ_HASATTR.replace("\"read\")", "\"name\")"),
            )],
        )
        .present(&[("pkg/buf_use.py", "EFF111")]),
        Step::new(
            "the reader rebinds a missing argument to a buffer before its `isinstance` test",
            vec![save("pkg/buf_read.py", BUF_READ_REBINDING)],
        )
        .absent(&[
            ("pkg/buf_use.py", "EFF111"),
            ("pkg/buf_default_use.py", "EFF111"),
        ]),
        Step::new(
            "the reader loses its tests",
            vec![save("pkg/buf_read.py", BUF_READ)],
        )
        .present(&[
            ("pkg/buf_use.py", "EFF111"),
            ("pkg/buf_default_use.py", "EFF111"),
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
        .present(&[("pkg/careless.py", "MUT109")]),
        Step::new(
            "the helper announcing an allowed writer's change in another module stops",
            vec![save("pkg/announce.py", ANNOUNCE_SILENT)],
        )
        .present(&[("pkg/careless.py", "MUT109"), ("pkg/grow.py", "MUT109")]),
        Step::new(
            "the helper announces the change again",
            vec![save("pkg/announce.py", ANNOUNCE)],
        )
        .present(&[("pkg/careless.py", "MUT109")])
        .absent(&[("pkg/grow.py", "MUT109")]),
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
                "extend-select = [\"MUT101\", \"MUT107\", \"MUT108\", \"MUT111\", \"EFF107\", \"MUT112\", \"MUT113\", \"EFF109\", \"EFF110\", \"EFF112\", \"EFF114\", \"EFF115\", \"MUT115\", \"MUT118\", \"MUT119\"]",
                "extend-select = [\"MUT101\", \"MUT107\", \"MUT108\", \"MUT111\", \"EFF107\", \"MUT112\", \"MUT113\", \"EFF109\", \"EFF110\", \"EFF112\", \"EFF114\", \"EFF115\", \"MUT115\", \"MUT118\", \"MUT119\"]\nselect = [\"NOPE1\"]",
            ))],
        )
        .absent(&[("pkg/svc_user.py", "EFF101")]),
        Step::new(
            "the configuration is valid again",
            vec![Action::Configure(CONFIG.to_owned())],
        )
        .present(&[("pkg/svc_user.py", "EFF101")]),
    ]
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
        .present(&[("pkg/draw.py", "EFF111")]),
        Step::new(
            "the override is removed",
            vec![save("pkg/square.py", SQUARE)],
        )
        .absent(&[("pkg/draw.py", "EFF111")]),
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
    aliases_and_getters: steps_aliases_and_getters,
    protocol_reads: steps_protocol_reads,
    body_runs: steps_body_runs,
    task_arguments: steps_task_arguments,
    small_displays: steps_small_displays,
    queue_capacity: steps_queue_capacity,
    mut114_mut202_mut109: steps_mut114_mut202_mut109,
    mut115: steps_mut115,
    item_changes: steps_item_changes,
    module_patterns: steps_module_patterns,
    indirect_helpers: steps_indirect_helpers,
    captured_parameters: steps_captured_parameters,
    inherited_writers: steps_inherited_writers,
    hash_writers: steps_hash_writers,
    mut111: steps_mut111,
    mut118: steps_mut118,
    mut119: steps_mut119,
    mut112: steps_mut112,
    eff109: steps_eff109,
    mut113: steps_mut113,
    eff110: steps_eff110,
    eff112: steps_eff112,
    eff114: steps_eff114,
    eff115: steps_eff115,
    attribute_types: steps_attribute_types,
    receiver_typing: steps_receiver_typing,
    effect_index: steps_effect_index,
    exact_effects: steps_exact_effects,
    store_types: steps_store_types,
    unused_suppressions: steps_unused_suppressions,
    helper_witnesses: steps_helper_witnesses,
    imported_constants: steps_imported_constants,
    method_lookups: steps_method_lookups,
    private_names: steps_private_names,
    contracts: steps_contracts,
    allowed_examples: steps_allowed_examples,
    contract_extensions: steps_contract_extensions,
    references: steps_references,
    class_bodies: steps_class_bodies,
    bases_and_attribution: steps_bases_and_attribution,
    argument_lists: steps_argument_lists,
    environment: steps_environment,
    keys_fresh_strict: steps_keys_fresh_strict,
    files: steps_files,
    configuration: steps_configuration,
    workspace_members: steps_workspace_members,
    project_assumptions: steps_project_assumptions,
    declarations: steps_declarations,
    re_exports: steps_re_exports,
    package_names: steps_package_names,
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
