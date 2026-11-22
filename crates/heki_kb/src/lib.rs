//! Three-layer Knowledge Base with startup projection (Materialization).
//!
//! Architecture Invariant (ADR-005):
//! - `EffectiveSoundnessKB` (Layer 1) contains ZERO advisory effect fields.
//!   Any entry whose `verified_max_python < target_python` is automatically downgraded
//!   to `Unknown` during materialization.
//! - `EffectiveEcosystemKB` (Layer 2) holds all Execution Effect sources, `async_safe`,
//!   `offloads_blocking_arg`, `excluded` (logging), `copy_sanitizer` (MUT202), and
//!   `opaque_to_layer3 = true` (preventing Layer 3 from descending into stdlib/Layer 2).
//!
//! The built-in TOML files under `kb/` are embedded with `include_str!`; every struct they
//! deserialize into uses `#[serde(deny_unknown_fields)]`, so an advisory field written into
//! a Layer 1 file (or a Layer 1 field written into a Layer 2 file) fails to load.

use heki_env::{CopySanitizerSetting, EcosystemOverride, PythonVersion};
use serde::de::IntoDeserializer;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

/// Embedded Layer 1 files (`kb/soundness/*.toml`): `(path relative to the repository, contents)`.
const SOUNDNESS_KB_FILES: &[(&str, &str)] = &[(
    "kb/soundness/builtins.toml",
    include_str!("../../../kb/soundness/builtins.toml"),
)];

/// Embedded Layer 2 files (`kb/ecosystem/*.toml`): `(path relative to the repository, contents)`.
const ECOSYSTEM_KB_FILES: &[(&str, &str)] = &[
    (
        "kb/ecosystem/stdlib.toml",
        include_str!("../../../kb/ecosystem/stdlib.toml"),
    ),
    (
        "kb/ecosystem/web_db_data.toml",
        include_str!("../../../kb/ecosystem/web_db_data.toml"),
    ),
];

#[derive(Debug, Error)]
pub enum KbError {
    #[error("failed to parse embedded KB TOML `{file}`: {cause}")]
    Toml {
        file: &'static str,
        cause: toml::de::Error,
    },
    #[error("invalid effect kind `{0}` in user ecosystem override")]
    InvalidEffect(String),
    #[error("invalid copy-sanitizer `{0}` in user ecosystem override (`shallow` or `deep`)")]
    InvalidCopyDepth(String),
    #[error(
        "invalid one-at-a-time `{0}` in user ecosystem override (`always`, `when_unbound`, or `unbinds`)"
    )]
    InvalidOneAtATime(String),
    #[error(
        "`attribute-type` in the user ecosystem override of `{0}` takes no other key: it describes reading an attribute, not calling it"
    )]
    AttributeTypeWithCallKeys(String),
}

/// Layer 1 parameter effect (Soundness lane ONLY — no advisory I/O effects).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SoundnessParamEffect {
    ReadOnly,
    Mutated,
    /// Return value retains an internal reference / view / iterator to this parameter
    /// (e.g., `iter`, `enumerate`, `zip`, `map`, `filter`, `reversed`, `dict.keys`, `memoryview`).
    RetainedInReturn,
    Escaped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoundnessEntry {
    pub symbol: String,
    pub return_exact_type: Option<String>,
    pub param_effects: Vec<SoundnessParamEffect>,
    pub verified_max_python: PythonVersion,
}

/// On-disk shape of one `kb/soundness/*.toml` file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SoundnessKbFile {
    #[serde(default)]
    entries: Vec<SoundnessEntry>,
}

fn parse_soundness_file(file: &'static str, text: &str) -> Result<Vec<SoundnessEntry>, KbError> {
    toml::from_str::<SoundnessKbFile>(text)
        .map(|parsed| parsed.entries)
        .map_err(|cause| KbError::Toml { file, cause })
}

/// Materialized Layer 1 KB exposed exclusively to `heki_soundness`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EffectiveSoundnessKb {
    pub entries: BTreeMap<String, SoundnessEntry>,
    pub digest: String,
}

impl EffectiveSoundnessKb {
    /// Materialize Layer 1 entries for `target_python`, dropping entries past `verified_max_python`.
    pub fn materialize(raw: Vec<SoundnessEntry>, target_python: PythonVersion) -> Self {
        let mut entries = BTreeMap::new();
        for entry in raw {
            if entry.verified_max_python >= target_python {
                entries.insert(entry.symbol.clone(), entry);
            }
        }
        let digest = blake3::hash(
            serde_json::to_string(&entries)
                .unwrap_or_default()
                .as_bytes(),
        )
        .to_hex()
        .to_string();
        Self { entries, digest }
    }

    /// Embed `kb/soundness/*.toml` via `include_str!` and project for `target_python`.
    /// Entries with `verified_max_python < target_python` are dropped (downgraded to Unknown).
    pub fn load_builtin(target_python: PythonVersion) -> Result<Self, KbError> {
        let mut raw = Vec::new();
        for &(file, text) in SOUNDNESS_KB_FILES {
            raw.extend(parse_soundness_file(file, text)?);
        }
        Ok(Self::materialize(raw, target_python))
    }

    pub fn lookup(&self, symbol: &str) -> Option<&SoundnessEntry> {
        self.entries.get(symbol)
    }
}

/// Classes whose instances are futures, never coroutines: `asyncio.create_task` rejects them, and
/// `asyncio.ensure_future` returns an `asyncio` one unchanged and rejects a
/// `concurrent.futures.Future` (`EFF107`: no new task). A `return_type` may name one without
/// catalogued methods: the class itself is the fact.
pub const FUTURE_CLASSES: &[&str] = &[
    "asyncio.Future",
    "asyncio.Task",
    "asyncio.futures.Future",
    "asyncio.tasks.Task",
    "concurrent.futures.Future",
];

/// Execution Effect tags owned exclusively by Layer 2 (`heki_effect`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecEffectKind {
    BlockingNet,
    BlockingDb,
    BlockingSubprocess,
    BlockingSync,
    Sleep,
    BlockingFs,
    UnpooledClientAlloc,
    DbQuery,
    QuadraticRealloc,
}

impl ExecEffectKind {
    /// The effects that wait on the calling thread (`EFF101` / `EFF105` / `EFF106`), as opposed
    /// to the loop-sensitive costs `EFF102` / `EFF103` / `DATA101` count.
    pub const BLOCKING: &[Self] = &[
        Self::BlockingNet,
        Self::BlockingDb,
        Self::BlockingSubprocess,
        Self::BlockingSync,
        Self::Sleep,
        Self::BlockingFs,
    ];

    /// Whether the effect is one of [`Self::BLOCKING`].
    pub fn blocks(self) -> bool {
        Self::BLOCKING.contains(&self)
    }

    /// What a blocking effect waits for; `None` for the loop-sensitive costs.
    pub const fn wait_kind(self) -> Option<WaitKind> {
        match self {
            Self::BlockingNet
            | Self::BlockingDb
            | Self::BlockingSubprocess
            | Self::Sleep
            | Self::BlockingFs => Some(WaitKind::Io),
            Self::BlockingSync => Some(WaitKind::Thread),
            Self::UnpooledClientAlloc | Self::DbQuery | Self::QuadraticRealloc => None,
        }
    }

    /// Parse an effect name spelled as in `kb/ecosystem/*.toml` (snake_case, e.g. `"blocking_net"`).
    pub fn from_kb_name(name: &str) -> Result<Self, KbError> {
        let de: serde::de::value::StrDeserializer<'_, serde::de::value::Error> =
            name.into_deserializer();
        Self::deserialize(de).map_err(|_| KbError::InvalidEffect(name.to_owned()))
    }
}

/// How a method of a class lets tasks call it on one object at a time (`EFF110`): a second task
/// calling one while another task is inside one of them raises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OneAtATime {
    /// Always (`asyncio.StreamReader.readline`, asyncpg's `Connection.fetch`).
    Always,
    /// While the object holds no connection: before its first operation, and after one that
    /// gives the connection back (SQLAlchemy's `AsyncSession.execute`).
    WhenUnbound,
    /// As [`Self::WhenUnbound`], and the object holds no connection after it
    /// (`AsyncSession.commit`).
    Unbinds,
}

impl OneAtATime {
    /// The setting's name in `[[tool.heki.ecosystem]]`.
    pub fn from_name(name: &str) -> Result<Self, KbError> {
        match name {
            "always" => Ok(Self::Always),
            "when_unbound" => Ok(Self::WhenUnbound),
            "unbinds" => Ok(Self::Unbinds),
            _ => Err(KbError::InvalidOneAtATime(name.to_owned())),
        }
    }
}

/// What a blocking call waits for ([`ExecEffectKind::wait_kind`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WaitKind {
    /// Input or output the call performs: the network, a database, a child process, a sleep,
    /// the file system. The call waits whatever other threads do.
    Io,
    /// Another thread's progress (`blocking_sync`: a lock, an event, a condition, a barrier, a
    /// thread, a queue, a future). The call returns at once when that thread has already done
    /// its part, which is state at run time.
    Thread,
}

/// When a database call sends a statement or otherwise waits on the database
/// ([`EcosystemEntry::emits`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Emits {
    /// On every call.
    #[default]
    Always,
    /// Only when the session's identity map does not hold the object (`Session.get`).
    OnIdentityMapMiss,
    /// Only when the session or connection holds pending changes or an open transaction
    /// (`Session.flush`, a driver's `commit()`).
    IfPending,
    /// Only when the receiver has not loaded its results (Django's `QuerySet.count()` on an
    /// evaluated query set counts its cache).
    UnlessCached,
    /// Only when no open connection is at hand (a pool checkout, Django's
    /// `connection.cursor()`).
    UnlessConnected,
    /// Its waits (`blocking_db`) only when the connection's database is on disk, which heki did
    /// not see opened: one opened from a parameter (`sqlite3.connect(path)` with
    /// `path=":memory:"` passed in) or only named by an annotation may be in memory. It sends
    /// its statements either way.
    IfOnDisk,
}

/// What the items of a call's result are when the environment decides how many there are
/// ([`EcosystemEntry::items`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultItems {
    /// The rows of a query.
    Rows,
    /// The lines of a file or of a child process's output.
    Lines,
    /// The entries of a directory.
    Entries,
    /// Text read from a file or a child process: the pieces `str.splitlines` cuts are its
    /// lines (iterated as it is, a string gives characters).
    Text,
}

/// How much of its operand a copy sanitizer copies (`MUT202`): the object only, so the values
/// below it stay shared (`copy.copy`), or everything below it too (`copy.deepcopy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyDepth {
    Shallow,
    Deep,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EcosystemEntry {
    pub symbol: String,
    pub effects: Vec<ExecEffectKind>,
    pub async_safe: bool,
    pub offloads_blocking_arg: Option<u8>,
    pub excluded: bool,
    /// The call returns a copy of its operand (`MUT202`), this deep.
    pub copy_sanitizer: Option<CopyDepth>,
    /// The method is a class's `__setattr__` or `__delattr__` that stores the value (or deletes
    /// the attribute) on the instance itself, as its own attribute, or raises: it hands the
    /// value to no other object. The class installs no descriptor for such an attribute and
    /// defines no `__getattribute__`, so reading the attribute back finds the stored value. The
    /// value stored may be a validated conversion, and validators of the project may run
    /// (`pydantic.BaseModel` with `validate_assignment`).
    pub stores_on_instance: bool,
    /// An instance of this class built without a value for a field starts from the field's
    /// default itself when the default is hashable, and from a deep copy of an unhashable one
    /// (pydantic 2's `BaseModel`, through `pydantic-core`). A field that validates its default
    /// (`validate_default=True`, or the model's `validate_default` setting) then holds what the
    /// validation returns, which need not be that object.
    pub keeps_hashable_defaults: bool,
    pub opaque_to_layer3: bool,
    /// Optional instance class returned by a factory function or method
    /// (e.g. `sqlite3.connect` -> `sqlite3.Connection`, `sqlite3.Connection.cursor` -> `sqlite3.Cursor`),
    /// or [`SELF_RETURN_TYPE`] for a method returning an instance of its receiver's class.
    pub return_type: Option<String>,
    /// `EFF106`: a `with` block over an instance of this class (a synchronous `threading` lock)
    /// or over the result of this call (a DB transaction starter such as
    /// `sqlalchemy.orm.Session.begin`) holds a lock or a transaction. Async locks are never
    /// flagged.
    pub sync_lock_or_tx: bool,
    /// `EFF109`: an instance of this class is a lock its holder cannot take again: acquiring it
    /// while the same thread or task holds it waits forever (`threading.Lock`, `asyncio.Lock`).
    pub non_reentrant_lock: bool,
    /// `EFF110`: tasks may not be inside this method on one object at once (`async for` over an
    /// instance runs its class's `__anext__`).
    pub one_at_a_time: Option<OneAtATime>,
    /// What the call does instead when an argument is known (the first case that holds).
    pub cases: Vec<ArgCase>,
    /// Index of a callable argument the call runs on the calling thread, handing it a
    /// synchronous facade over an async database connection as its first argument
    /// (SQLAlchemy's `AsyncSession.run_sync(fn)`): the database I/O through the facade is
    /// awaited for it, its other blocking calls block.
    pub sync_facade_arg: Option<u8>,
    /// The call waits until its receiver, a future, is done: on one known to be done (under
    /// `if f.done():`, or after `await asyncio.wrap_future(f)`), it returns at once and does
    /// not block (`concurrent.futures.Future.result`).
    pub waits_until_done: bool,
    /// Awaiting the object the call returns completes only once the future passed at this
    /// position is done (`asyncio.wrap_future(f)`).
    pub done_when_awaited: Option<u8>,
    /// When the call's database effects (`blocking_db`, `db_query`) happen: on every call, or
    /// only in a state of the session, connection, or receiver the call does not show.
    pub emits: Emits,
    /// What the items of the call's result are, when the environment decides how many there
    /// are (the origin of a loop's items: `EFF103`).
    pub items: Option<ResultItems>,
    /// The library API a method of a catalog class is, as reports name it: the class of what a
    /// call returns in one case (`sqlite3.connect(":memory:")`) has the methods of a library
    /// class (`sqlite3.Connection.execute`).
    pub api: Option<String>,
    /// Whether a `with` statement over an instance of this class, or over the result of this
    /// call, can suppress the exception its body raises: `Some(false)` when `__exit__` /
    /// `__aexit__` returns a false value on every path, `Some(true)` when it can return a true
    /// one (`contextlib.suppress`), `None` when the catalog does not say.
    pub exit_suppresses: Option<bool>,
    /// Calling the symbol (`os.getenv`) or reading it (`sys.platform`) depends on the
    /// environment the program runs in: the `reads-environment` effect boundaries forbid it.
    pub environment: Option<EnvironmentAccess>,
    /// An instance of this class is a queue that the argument passed here bounds
    /// (`queue.Queue(maxsize)`): a positive integer limits how many items it holds; leaving it
    /// out, zero, or a negative integer leaves it unbounded.
    pub bounded_by: Option<ArgSlot>,
    /// The call waits only while its receiver, a queue, is full, which only a bounded one can be
    /// (`queue.Queue.put`): it blocks only where the receiver is known to be bounded
    /// ([`Self::bounded_by`]).
    pub waits_while_full: bool,
    /// `EFF112`: the call writes an argument, or serializes it into text, in the order of its
    /// elements (`json.dumps`, `pathlib.Path.write_text`, `print`).
    pub writes_in_order: Option<OrderedOutput>,
}

/// What a call writes in the order of its elements (`EcosystemEntry::writes_in_order`): a
/// mapping's keys and a sequence's items in their iteration order, text as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OrderedOutput {
    /// The argument written: at position `arg` (the receiver not counted), or passed by the
    /// keyword `param`; with neither, every positional argument (`print`).
    pub arg: Option<u8>,
    pub param: Option<String>,
    /// The keyword argument that sorts the keys of the mappings it writes first (`sort_keys`,
    /// `sort_dicts`).
    pub sorted_by: Option<String>,
    /// The call sorts them when it is not passed `sorted_by`.
    pub sorted_by_default: bool,
}

/// How a symbol tagged `environment` reads the environment (`EcosystemEntry::environment`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentAccess {
    /// Calling the function reads it (`os.getenv`, `platform.system`).
    Call,
    /// Reading the module attribute does (`os.environ`, `sys.platform`).
    Read,
    /// Calling the function imports or finds the module its argument `name` (position 0) names,
    /// a relative name anchored at its argument `package` (position 1), as
    /// `importlib.import_module` does: it reads the environment unless the module is the
    /// project's own.
    Import,
}

/// An argument of a catalogued call: the one at position `arg` (the receiver not counted), or
/// the one passed by the keyword `param`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArgSlot {
    pub arg: Option<u8>,
    pub param: Option<String>,
}

/// One case of an entry (`EcosystemEntry::cases`): when the argument at position `arg` (the
/// receiver not counted), or passed by the keyword `param`, passes every test the case sets,
/// the call has the case's `effects` and `return_type` (those it sets) instead of the entry's.
/// An argument the call does not pass, or may pass through `*` / `**` unpacking, passes no
/// test, so the entry's own facts are those of the unknown argument.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArgCase {
    pub arg: Option<u8>,
    pub param: Option<String>,
    /// A literal equal to this one (`false`; `0`, which a float `0.0` equals too).
    pub equals: Option<ArgLiteral>,
    /// A string literal starting with one of these prefixes.
    pub prefix: Vec<String>,
    /// A string literal that is not empty.
    pub nonempty: bool,
    /// An instance of one of these classes.
    pub instance_of: Vec<String>,
    /// An integer that sets one of these named flags: the flag itself, or a `|` with it as an
    /// operand (`socket.AI_PASSIVE | socket.AI_NUMERICHOST`).
    pub flag: Vec<String>,
    /// A string literal equal to one of these.
    pub one_of: Vec<String>,
    /// A string literal that is an IPv4 address in dotted-decimal form or an IPv6 address.
    pub numeric_address: bool,
    /// A string literal containing one of these.
    pub contains: Vec<String>,
    /// A value established to name a file ([`ArgFact::names_file`]).
    pub names_file: bool,
    pub effects: Option<Vec<ExecEffectKind>>,
    pub return_type: Option<String>,
    /// When the call waits in this case, instead of the entry's `emits`.
    pub emits: Option<Emits>,
}

/// A literal `ArgCase::equals` compares with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ArgLiteral {
    Bool(bool),
    Int(i64),
}

/// What the analysis knows of an argument a call passes ([`EcosystemEntry::case_for`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ArgFact {
    pub literal: Option<LiteralValue>,
    /// The value is established to name a file: a literal or a module constant bound once with
    /// no in-memory form (`":memory:"`, `file::memory:`, `mode=memory`), `__file__`, or a path
    /// built only from such parts.
    pub names_file: bool,
    /// The class of the value, when it is an instance of a known one.
    pub class: Option<String>,
    /// The named constants the value sets with `|` (one name alone sets itself): each flag is
    /// set whatever the other operands are.
    pub flags: Vec<String>,
}

/// A constant argument.
#[derive(Debug, Clone, PartialEq)]
pub enum LiteralValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

impl ArgCase {
    fn holds(&self, fact: &ArgFact) -> bool {
        let literal = fact.literal.as_ref();
        let text = match literal {
            Some(LiteralValue::Str(s)) => Some(s.as_str()),
            _ => None,
        };
        let equals = self.equals.is_none_or(|want| match (want, literal) {
            (ArgLiteral::Bool(b), Some(LiteralValue::Bool(v))) => b == *v,
            (ArgLiteral::Int(n), Some(LiteralValue::Int(v))) => n == *v,
            (ArgLiteral::Int(n), Some(LiteralValue::Float(v))) => n as f64 == *v,
            _ => false,
        });
        let prefix = self.prefix.is_empty()
            || text.is_some_and(|t| self.prefix.iter().any(|p| t.starts_with(p.as_str())));
        let nonempty = !self.nonempty || text.is_some_and(|t| !t.is_empty());
        let instance = self.instance_of.is_empty()
            || fact
                .class
                .as_ref()
                .is_some_and(|c| self.instance_of.contains(c));
        let flag = self.flag.is_empty() || fact.flags.iter().any(|f| self.flag.contains(f));
        let one_of =
            self.one_of.is_empty() || text.is_some_and(|t| self.one_of.iter().any(|o| o == t));
        let contains = self.contains.is_empty()
            || text.is_some_and(|t| self.contains.iter().any(|c| t.contains(c.as_str())));
        let file = !self.names_file || fact.names_file;
        let numeric =
            !self.numeric_address || text.is_some_and(|t| t.parse::<std::net::IpAddr>().is_ok());
        equals && prefix && nonempty && instance && flag && one_of && numeric && contains && file
    }
}

impl EcosystemEntry {
    /// The entry as the analyses read it: an `async_safe` API never blocks the event loop, so
    /// it keeps only its loop-sensitive effects (`db_query`, `unpooled_client_alloc`).
    fn normalized(mut self) -> Self {
        if self.async_safe {
            self.effects.retain(|e| !e.blocks());
            for case in &mut self.cases {
                if let Some(effects) = &mut case.effects {
                    effects.retain(|e| !e.blocks());
                }
            }
        }
        self
    }

    /// The first case the call's arguments select: `arg(position, keyword)` tells what the
    /// call passes there (`None`: nothing known, or not passed).
    pub fn case_for(
        &self,
        mut arg: impl FnMut(Option<u8>, Option<&str>) -> Option<ArgFact>,
    ) -> Option<&ArgCase> {
        self.cases
            .iter()
            .find(|case| arg(case.arg, case.param.as_deref()).is_some_and(|fact| case.holds(&fact)))
    }

    /// How calling or reading the symbol reads the environment, as `reads-environment`
    /// boundaries forbid it: never for an `excluded` entry, which is never reported.
    pub fn environment_access(&self) -> Option<EnvironmentAccess> {
        self.environment.filter(|_| !self.excluded)
    }

    pub fn effects_in<'e>(&'e self, case: Option<&'e ArgCase>) -> &'e [ExecEffectKind] {
        case.and_then(|c| c.effects.as_deref())
            .unwrap_or(&self.effects)
    }

    /// When the call waits: its case's `emits`, or the entry's.
    pub fn emits_in(&self, case: Option<&ArgCase>) -> Emits {
        case.and_then(|c| c.emits).unwrap_or(self.emits)
    }

    /// The class of what the call returns, when the catalog names one.
    pub fn return_type_in<'e>(&'e self, case: Option<&'e ArgCase>) -> Option<&'e str> {
        self.declared_return_in(case)
            .filter(|t| *t != SELF_RETURN_TYPE)
    }

    /// Whether the method returns an instance of its receiver's class ([`SELF_RETURN_TYPE`]).
    pub fn returns_receiver_in(&self, case: Option<&ArgCase>) -> bool {
        self.declared_return_in(case) == Some(SELF_RETURN_TYPE)
    }

    fn declared_return_in<'e>(&'e self, case: Option<&'e ArgCase>) -> Option<&'e str> {
        case.and_then(|c| c.return_type.as_deref())
            .or(self.return_type.as_deref())
    }
}

/// The `return_type` of a method returning an instance of its receiver's class: a fluent
/// builder (`openai.OpenAI.with_options`) called on an instance of a project subclass returns
/// one of that subclass.
pub const SELF_RETURN_TYPE: &str = "Self";

/// A validated `[[tool.heki.ecosystem]]` entry: the keys it sets on the entry for `symbol`
/// (`None`: left out, the built-in value stays).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EcosystemPatch {
    pub symbol: String,
    pub effects: Option<Vec<ExecEffectKind>>,
    pub async_safe: Option<bool>,
    pub offloads_blocking_arg: Option<u8>,
    pub excluded: Option<bool>,
    /// `Some(None)`: no copy sanitizer.
    pub copy_sanitizer: Option<Option<CopyDepth>>,
    pub return_type: Option<String>,
    pub sync_lock_or_tx: Option<bool>,
    pub non_reentrant_lock: Option<bool>,
    pub one_at_a_time: Option<OneAtATime>,
    pub writes_in_order: Option<OrderedOutput>,
    /// The `[[attributes]]` row of `symbol` ([`AttributeType`]); a patch setting it sets
    /// nothing else.
    pub attribute_type: Option<String>,
}

impl EcosystemPatch {
    fn apply(&self, entry: &mut EcosystemEntry) {
        if let Some(effects) = &self.effects {
            entry.effects.clone_from(effects);
        }
        if let Some(v) = self.async_safe {
            entry.async_safe = v;
        }
        if let Some(k) = self.offloads_blocking_arg {
            entry.offloads_blocking_arg = Some(k);
        }
        if let Some(v) = self.excluded {
            entry.excluded = v;
        }
        if let Some(depth) = self.copy_sanitizer {
            entry.copy_sanitizer = depth;
        }
        if let Some(t) = &self.return_type {
            entry.return_type = Some(t.clone());
        }
        if let Some(v) = self.sync_lock_or_tx {
            entry.sync_lock_or_tx = v;
        }
        if let Some(v) = self.non_reentrant_lock {
            entry.non_reentrant_lock = v;
        }
        if let Some(v) = self.one_at_a_time {
            entry.one_at_a_time = Some(v);
        }
        if let Some(v) = &self.writes_in_order {
            entry.writes_in_order = Some(v.clone());
        }
    }
}

/// Validate a `[[tool.heki.ecosystem]]` override (its effect names).
impl TryFrom<&EcosystemOverride> for EcosystemPatch {
    type Error = KbError;

    fn try_from(value: &EcosystemOverride) -> Result<Self, KbError> {
        let effects = value
            .effects
            .as_ref()
            .map(|names| {
                names
                    .iter()
                    .map(|name| ExecEffectKind::from_kb_name(name))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        let copy_sanitizer = match &value.copy_sanitizer {
            None => None,
            Some(CopySanitizerSetting::Flag(false)) => Some(None),
            Some(CopySanitizerSetting::Flag(true)) => Some(Some(CopyDepth::Shallow)),
            Some(CopySanitizerSetting::Depth(depth)) => Some(Some(match depth.as_str() {
                "shallow" => CopyDepth::Shallow,
                "deep" => CopyDepth::Deep,
                _ => return Err(KbError::InvalidCopyDepth(depth.clone())),
            })),
        };
        let patch = Self {
            symbol: value.symbol.clone(),
            effects,
            async_safe: value.async_safe,
            offloads_blocking_arg: value.offloads_blocking_arg,
            excluded: value.excluded,
            copy_sanitizer,
            return_type: value.return_type.clone(),
            sync_lock_or_tx: value.sync_lock_or_tx,
            non_reentrant_lock: value.non_reentrant_lock,
            one_at_a_time: value
                .one_at_a_time
                .as_deref()
                .map(OneAtATime::from_name)
                .transpose()?,
            writes_in_order: value.writes_in_order.as_ref().map(|w| OrderedOutput {
                arg: w.arg,
                param: w.param.clone(),
                sorted_by: w.sorted_by.clone(),
                sorted_by_default: w.sorted_by_default,
            }),
            attribute_type: value.attribute_type.clone(),
        };
        let sets_call_keys = Self {
            symbol: String::new(),
            attribute_type: None,
            ..patch.clone()
        } != Self::default();
        if patch.attribute_type.is_some() && sets_call_keys {
            return Err(KbError::AttributeTypeWithCallKeys(patch.symbol));
        }
        Ok(patch)
    }
}

/// What a library call runs of the values it is handed (`[[invokes]]`): an entry lists each
/// argument the call runs code of; an argument it does not list runs nothing there. The code a
/// value runs is its class's: the special methods named (looked up on the metaclass for a class
/// object), its items' special methods, or the value itself when the call calls it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Invokes {
    pub symbol: String,
    /// The call returns an iterator that runs what `values` names again while it is iterated
    /// (`enumerate`, `map`).
    pub lazy: bool,
    pub values: Vec<InvokedValue>,
}

/// One argument of an [`Invokes`] entry and what the call runs of it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InvokedValue {
    /// The argument at this position (the receiver not counted), or at any position from it on
    /// with `rest`.
    pub arg: Option<u8>,
    pub rest: bool,
    /// The argument passed by this keyword (`"*"`: by any keyword).
    pub param: Option<String>,
    /// The receiver of a method.
    pub receiver: bool,
    /// The special methods the call runs on the value's class; `"*"`: any attribute of it (a
    /// read by a name the call is handed).
    pub methods: Vec<String>,
    /// The special methods the call runs on the class of each item the value holds.
    pub item_methods: Vec<String>,
    /// The call calls the value itself.
    pub calls: bool,
}

impl InvokedValue {
    /// Whether this entry names the argument at `position` (`None`: passed by keyword
    /// `keyword`).
    pub fn names(&self, position: Option<u8>, keyword: Option<&str>) -> bool {
        match (position, keyword) {
            (Some(i), _) => self.arg.is_some_and(|a| a == i || (self.rest && i > a)),
            (None, Some(k)) => matches!(self.param.as_deref(), Some(p) if p == k || p == "*"),
            (None, None) => false,
        }
    }
}

/// A cached-property decorator the catalog lists as keeping the getter's value in the instance
/// `__dict__` under the property's name (`[[cached_properties]]`): a later read returns it until
/// something removes it, and `del obj.<name>` or a removal from `obj.__dict__` makes the next read
/// run the getter again. Only a decorator listed here makes a cached property an exact claim's;
/// any other `cached_property` is one by its name only (R3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CachedPropertyDecorator {
    /// The implementation's qualified name, as an import resolves it.
    pub symbol: String,
    /// The versions whose source the entry was checked against.
    pub versions: String,
    /// Why the value is kept in the instance `__dict__`.
    pub keeps: String,
    /// How `del obj.<name>` and `obj.__dict__.pop(<name>)` act on the kept value.
    pub invalidation: String,
    /// Where an instance without a `__dict__` keeps the value instead (a slot), if anywhere;
    /// without this the decorator keeps nothing on such an instance.
    #[serde(default)]
    pub slot: Option<String>,
}

/// What reading an attribute on an instance of a library class gives (`[[attributes]]`): in
/// every release the distribution's `verified` range admits ([`Distribution`]), the value read
/// has the catalogued members of the class `type`. It says nothing of the value's identity or
/// of what the read runs, so a plain attribute, a property, and a cached property are alike to
/// it. A separate table from `[[entries]]`, which describe calls: an attribute is no method a
/// class takes from its base.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttributeType {
    /// `package.module.Class.attribute`, the class named as an import, a constructor call, a
    /// `return_type`, or another row's `type` names it.
    pub symbol: String,
    #[serde(rename = "type")]
    pub class: String,
}

/// A distribution whose catalog rows were checked against its releases
/// (`[[distributions]]`): every entry and attribute row under `packages` holds in each release
/// `verified` admits. The analysis never reads the project's versions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Distribution {
    /// The name on PyPI (`google-genai`).
    pub name: String,
    /// The import packages whose rows it covers (`google.genai`).
    pub packages: Vec<String>,
    /// A PEP 440 version specifier (`>=1.66,<3`).
    pub verified: String,
}

/// On-disk shape of one `kb/ecosystem/*.toml` file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EcosystemKbFile {
    #[serde(default)]
    opaque_packages: Vec<String>,
    #[serde(default)]
    distributions: Vec<Distribution>,
    #[serde(default)]
    entries: Vec<EcosystemEntry>,
    #[serde(default)]
    attributes: Vec<AttributeType>,
    #[serde(default)]
    invokes: Vec<Invokes>,
    #[serde(default)]
    cached_properties: Vec<CachedPropertyDecorator>,
}

fn parse_ecosystem_file(file: &'static str, text: &str) -> Result<EcosystemKbFile, KbError> {
    toml::from_str(text).map_err(|cause| KbError::Toml { file, cause })
}

/// Top-level package of a dotted symbol (`"requests.Session.get"` -> `"requests"`).
fn top_level_package(symbol: &str) -> &str {
    symbol.split_once('.').map_or(symbol, |(head, _)| head)
}

/// Materialized Layer 2 KB exposed exclusively to `heki_effect`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EffectiveEcosystemKb {
    pub entries: BTreeMap<String, EcosystemEntry>,
    pub opaque_packages: Vec<String>,
    /// The symbols `[[tool.heki.ecosystem]]` names ([`Self::declared`]).
    pub user_symbols: BTreeSet<String>,
    /// What each listed library call runs of the values it is handed ([`Invokes`]).
    pub invokes: BTreeMap<String, Invokes>,
    /// The cached-property decorators known to keep the value in the instance `__dict__`
    /// ([`CachedPropertyDecorator`]).
    pub cached_properties: BTreeMap<String, CachedPropertyDecorator>,
    /// `Class.attribute` -> the class of what reading it on an instance gives
    /// ([`AttributeType`]), the built-in rows and the users' `attribute-type`.
    pub attributes: BTreeMap<String, String>,
    /// The releases each distribution's rows were checked against ([`Distribution`]); the
    /// analysis never reads them.
    pub distributions: Vec<Distribution>,
    pub digest: String,
}

impl EffectiveEcosystemKb {
    pub fn materialize(raw: Vec<EcosystemEntry>, opaque_packages: Vec<String>) -> Self {
        let mut entries = BTreeMap::new();
        for entry in raw {
            entries.insert(entry.symbol.clone(), entry.normalized());
        }
        let digest = blake3::hash(
            format!(
                "{}|{}",
                serde_json::to_string(&entries).unwrap_or_default(),
                opaque_packages.join(",")
            )
            .as_bytes(),
        )
        .to_hex()
        .to_string();
        Self {
            entries,
            opaque_packages,
            user_symbols: BTreeSet::new(),
            invokes: BTreeMap::new(),
            cached_properties: BTreeMap::new(),
            attributes: BTreeMap::new(),
            distributions: Vec::new(),
            digest,
        }
    }

    /// Embed `kb/ecosystem/*.toml` via `include_str!` and merge user `overrides`, in order:
    /// each sets the keys it lists on the entry for its `symbol` (a built-in one, or a new
    /// one). A new entry inside an opaque package is `opaque_to_layer3`.
    pub fn load_builtin(overrides: &[EcosystemPatch]) -> Result<Self, KbError> {
        let mut raw: BTreeMap<String, EcosystemEntry> = BTreeMap::new();
        let mut opaque_packages = Vec::new();
        let mut invokes = BTreeMap::new();
        let mut cached_properties = BTreeMap::new();
        let mut attributes = BTreeMap::new();
        let mut distributions = Vec::new();
        for &(file, text) in ECOSYSTEM_KB_FILES {
            let parsed = parse_ecosystem_file(file, text)?;
            distributions.extend(parsed.distributions);
            raw.extend(parsed.entries.into_iter().map(|e| (e.symbol.clone(), e)));
            attributes.extend(parsed.attributes.into_iter().map(|a| (a.symbol, a.class)));
            opaque_packages.extend(parsed.opaque_packages);
            invokes.extend(parsed.invokes.into_iter().map(|i| (i.symbol.clone(), i)));
            cached_properties.extend(
                parsed
                    .cached_properties
                    .into_iter()
                    .map(|c| (c.symbol.clone(), c)),
            );
        }
        opaque_packages.sort();
        opaque_packages.dedup();
        for patch in overrides {
            if let Some(class) = &patch.attribute_type {
                attributes.insert(patch.symbol.clone(), class.clone());
                continue;
            }
            let package = top_level_package(&patch.symbol);
            let entry = raw
                .entry(patch.symbol.clone())
                .or_insert_with(|| EcosystemEntry {
                    symbol: patch.symbol.clone(),
                    opaque_to_layer3: opaque_packages.iter().any(|p| p == package),
                    ..EcosystemEntry::default()
                });
            patch.apply(entry);
        }
        let mut kb = Self::materialize(raw.into_values().collect(), opaque_packages);
        let listed = serde_json::to_string(&invokes).unwrap_or_default();
        let caches = serde_json::to_string(&cached_properties).unwrap_or_default();
        let read = serde_json::to_string(&attributes).unwrap_or_default();
        kb.digest = blake3::hash(format!("{}|{listed}|{caches}|{read}", kb.digest).as_bytes())
            .to_hex()
            .to_string();
        kb.invokes = invokes;
        kb.cached_properties = cached_properties;
        kb.attributes = attributes;
        kb.distributions = distributions;
        kb.user_symbols = overrides
            .iter()
            .filter(|p| p.attribute_type.is_none())
            .map(|p| p.symbol.clone())
            .collect();
        if !kb.user_symbols.is_empty() {
            let users: Vec<&str> = kb.user_symbols.iter().map(String::as_str).collect();
            let keyed = format!("{}|{}", kb.digest, users.join(","));
            kb.digest = blake3::hash(keyed.as_bytes()).to_hex().to_string();
        }
        Ok(kb)
    }

    /// The entry of `symbol` when the project's configuration names it: a declaration of one
    /// of the project's own functions. A function the project defines is followed through its
    /// body otherwise, also when the built-in catalog lists its name (a package's own source).
    pub fn declared(&self, symbol: &str) -> Option<&EcosystemEntry> {
        self.user_symbols
            .contains(symbol)
            .then(|| self.entries.get(symbol))
            .flatten()
    }

    pub fn lookup(&self, symbol: &str) -> Option<&EcosystemEntry> {
        self.entries.get(symbol)
    }

    pub fn is_opaque_to_layer3(&self, top_level_package: &str) -> bool {
        self.opaque_packages.iter().any(|p| p == top_level_package)
    }

    /// What the call of `symbol` runs of the values it is handed, when the catalog lists it
    /// ([`Invokes`]).
    pub fn invoked(&self, symbol: &str) -> Option<&Invokes> {
        self.invokes.get(symbol)
    }

    /// The catalog's entry for the cached-property decorator `symbol`, when it lists it as
    /// keeping the value in the instance `__dict__` ([`CachedPropertyDecorator`]).
    pub fn cached_property(&self, symbol: &str) -> Option<&CachedPropertyDecorator> {
        self.cached_properties.get(symbol)
    }

    /// The class of what reading `attr` on an instance of exactly the library class `class`
    /// gives, when the catalog lists it ([`AttributeType`]).
    pub fn attribute_type(&self, class: &str, attr: &str) -> Option<&str> {
        self.attributes
            .get(&format!("{class}.{attr}"))
            .map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ExecEffectKind::*;
    use SoundnessParamEffect::*;
    use std::collections::BTreeSet;

    const PY316: PythonVersion = PythonVersion {
        major: 3,
        minor: 16,
    };

    #[test]
    fn layer1_downgrades_entries_past_verified_max_python() {
        let kb = EffectiveSoundnessKb::materialize(
            vec![
                SoundnessEntry {
                    symbol: "builtins.iter".into(),
                    return_exact_type: None,
                    param_effects: vec![SoundnessParamEffect::RetainedInReturn],
                    verified_max_python: PythonVersion::PY315,
                },
                SoundnessEntry {
                    symbol: "builtins.old_only".into(),
                    return_exact_type: None,
                    param_effects: vec![SoundnessParamEffect::ReadOnly],
                    verified_max_python: PythonVersion::PY312,
                },
            ],
            PythonVersion::PY315,
        );
        assert!(kb.entries.contains_key("builtins.iter"));
        assert!(!kb.entries.contains_key("builtins.old_only"));
    }

    /// docs/ANALYSIS_AND_KB_SPEC.md section 4.1. `builtins.str` / `builtins.bytes` carry no
    /// `return_exact_type`: `__str__` / `__bytes__` may return a subclass that CPython passes
    /// through (witnessed by `tests/sentinel/verify_soundness_kb.py`).
    const LAYER1_REQUIRED: &[(&str, Option<&str>, &[SoundnessParamEffect])] = &[
        ("builtins.iter", None, &[RetainedInReturn]),
        ("builtins.enumerate", None, &[RetainedInReturn]),
        ("builtins.zip", None, &[RetainedInReturn]),
        ("builtins.map", None, &[RetainedInReturn, RetainedInReturn]),
        (
            "builtins.filter",
            None,
            &[RetainedInReturn, RetainedInReturn],
        ),
        ("builtins.reversed", None, &[RetainedInReturn]),
        ("builtins.memoryview", None, &[RetainedInReturn]),
        ("builtins.dict.keys", None, &[RetainedInReturn]),
        ("builtins.dict.values", None, &[RetainedInReturn]),
        ("builtins.dict.items", None, &[RetainedInReturn]),
        ("builtins.len", Some("int"), &[ReadOnly]),
        ("builtins.bool", Some("bool"), &[ReadOnly]),
        ("builtins.int", Some("int"), &[ReadOnly]),
        ("builtins.float", Some("float"), &[ReadOnly]),
        ("builtins.str", None, &[ReadOnly]),
        ("builtins.bytes", None, &[ReadOnly]),
        ("builtins.range", None, &[ReadOnly]),
        ("builtins.list", Some("list"), &[ReadOnly]),
        ("builtins.set", Some("set"), &[ReadOnly]),
        ("builtins.dict", Some("dict"), &[ReadOnly]),
        ("builtins.tuple", Some("tuple"), &[ReadOnly]),
        ("builtins.list.append", None, &[Mutated, Escaped]),
        ("builtins.list.extend", None, &[Mutated, ReadOnly]),
        ("builtins.list.insert", None, &[Mutated, ReadOnly, Escaped]),
        ("builtins.list.pop", None, &[Mutated, ReadOnly]),
        ("builtins.list.clear", None, &[Mutated]),
        ("builtins.list.reverse", None, &[Mutated]),
        ("builtins.list.sort", None, &[Mutated]),
        ("builtins.list.remove", None, &[Mutated, Escaped]),
        ("builtins.set.add", None, &[Mutated, Escaped]),
        ("builtins.set.update", None, &[Mutated, ReadOnly]),
        ("builtins.set.discard", None, &[Mutated, Escaped]),
        ("builtins.set.remove", None, &[Mutated, Escaped]),
        ("builtins.set.clear", None, &[Mutated]),
        ("builtins.dict.update", None, &[Mutated, ReadOnly]),
        (
            "builtins.dict.pop",
            None,
            &[Mutated, Escaped, RetainedInReturn],
        ),
        (
            "builtins.dict.setdefault",
            None,
            &[Mutated, Escaped, Escaped],
        ),
        ("builtins.dict.clear", None, &[Mutated]),
        ("builtins.list.copy", Some("list"), &[ReadOnly]),
        ("builtins.set.copy", Some("set"), &[ReadOnly]),
        ("builtins.dict.copy", Some("dict"), &[ReadOnly]),
    ];

    #[test]
    fn layer1_load_builtin_contains_every_required_entry() {
        let kb = EffectiveSoundnessKb::load_builtin(PythonVersion::PY315).unwrap();
        for &(symbol, ret, effects) in LAYER1_REQUIRED {
            let expected = SoundnessEntry {
                symbol: symbol.into(),
                return_exact_type: ret.map(str::to_owned),
                param_effects: effects.to_vec(),
                verified_max_python: PythonVersion::PY315,
            };
            assert_eq!(kb.lookup(symbol), Some(&expected), "{symbol}");
        }
        assert_eq!(kb.entries.len(), LAYER1_REQUIRED.len());
        assert_eq!(kb.lookup("builtins.sorted"), None);
        assert_eq!(kb.digest.len(), 64);
    }

    #[test]
    fn layer1_load_builtin_projects_by_target_python() {
        let py312 = EffectiveSoundnessKb::load_builtin(PythonVersion::PY312).unwrap();
        let py315 = EffectiveSoundnessKb::load_builtin(PythonVersion::PY315).unwrap();
        assert_eq!(py312.entries, py315.entries);
        // Nothing is verified past 3.15: a newer target downgrades every entry to Unknown.
        let py316 = EffectiveSoundnessKb::load_builtin(PY316).unwrap();
        assert!(py316.entries.is_empty());
        assert_eq!(py316.lookup("builtins.iter"), None);
        assert_ne!(py316.digest, py315.digest);
    }

    #[test]
    fn layer1_rejects_advisory_fields() {
        for field in [
            "effects = [\"blocking_net\"]",
            "async_safe = true",
            "offloads_blocking_arg = 0",
            "excluded = true",
            "copy_sanitizer = true",
            "opaque_to_layer3 = true",
            "sync_lock_or_tx = true",
        ] {
            let text = format!(
                "[[entries]]\nsymbol = \"builtins.len\"\nparam_effects = [\"read_only\"]\n\
                 verified_max_python = {{ major = 3, minor = 15 }}\n{field}\n"
            );
            let err = parse_soundness_file("test.toml", &text).unwrap_err();
            assert!(
                matches!(
                    err,
                    KbError::Toml {
                        file: "test.toml",
                        ..
                    }
                ),
                "{field}"
            );
            assert!(err.to_string().contains("unknown field"), "{field}: {err}");
        }
        let err =
            parse_soundness_file("test.toml", "opaque_packages = [\"builtins\"]\n").unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn layer1_rejects_unknown_param_effect() {
        let text = "[[entries]]\nsymbol = \"builtins.len\"\nparam_effects = [\"blocking_net\"]\n\
                    verified_max_python = { major = 3, minor = 15 }\n";
        assert!(parse_soundness_file("test.toml", text).is_err());
    }

    fn entry(effects: &[ExecEffectKind]) -> EcosystemEntry {
        EcosystemEntry {
            effects: effects.to_vec(),
            opaque_to_layer3: true,
            ..EcosystemEntry::default()
        }
    }

    /// `e` typing its result as an instance of `class` (factories).
    fn returning(class: &str, e: EcosystemEntry) -> EcosystemEntry {
        EcosystemEntry {
            return_type: Some(class.into()),
            ..e
        }
    }

    /// `e` with tasks kept to one at a time in it (`EFF110`).
    fn one_at_a_time(kind: OneAtATime, e: EcosystemEntry) -> EcosystemEntry {
        EcosystemEntry {
            one_at_a_time: Some(kind),
            ..e
        }
    }

    /// `methods` of each of `classes`.
    fn with_prefix_all(classes: &[&str], methods: &[&str]) -> Vec<String> {
        classes
            .iter()
            .flat_map(|c| with_prefix(c, methods))
            .collect()
    }

    /// The methods tasks may call on one object one at a time (`EFF110`), and the factories
    /// typing those objects.
    fn one_at_a_time_apis() -> Vec<(Vec<String>, EcosystemEntry)> {
        let always = || one_at_a_time(OneAtATime::Always, async_safe(entry(&[])));
        let asyncio_websockets = [
            "websockets.ClientConnection",
            "websockets.ServerConnection",
            "websockets.asyncio.client.ClientConnection",
            "websockets.asyncio.server.ServerConnection",
        ];
        let legacy_websockets = [
            "websockets.WebSocketClientProtocol",
            "websockets.WebSocketServerProtocol",
            "websockets.legacy.protocol.WebSocketCommonProtocol",
        ];
        vec![
            (
                [
                    with_prefix(
                        "asyncio.StreamReader",
                        &["read", "readline", "readexactly", "readuntil", "__anext__"],
                    ),
                    with_prefix_all(ASYNCPG_CONNECTIONS, ASYNCPG_BULK),
                    with_prefix_all(ASYNCPG_CONNECTIONS, &["prepare"]),
                    with_prefix_all(
                        &asyncio_websockets,
                        &["recv", "recv_streaming", "__anext__"],
                    ),
                    with_prefix_all(&legacy_websockets, &["recv", "__anext__"]),
                    with_prefix_all(
                        &[
                            "aiohttp.ClientWebSocketResponse",
                            "aiohttp.web.WebSocketResponse",
                        ],
                        &[
                            "receive",
                            "receive_str",
                            "receive_bytes",
                            "receive_json",
                            "__anext__",
                        ],
                    ),
                ]
                .concat(),
                always(),
            ),
            (
                with_prefix_all(ASYNCPG_CONNECTIONS, ASYNCPG_QUERIES),
                one_at_a_time(OneAtATime::Always, async_safe(entry(&[DbQuery]))),
            ),
            (
                s(&["asyncpg.connect"]),
                async_safe(returning(
                    "asyncpg.Connection",
                    entry(&[UnpooledClientAlloc]),
                )),
            ),
            (
                s(&["anyio.create_task_group"]),
                async_safe(returning("anyio.abc.TaskGroup", entry(&[]))),
            ),
            (
                s(&["anyio.abc.TaskGroup.start_soon"]),
                async_safe(entry(&[])),
            ),
            (
                s(&["websockets.connect", "websockets.asyncio.client.connect"]),
                async_safe(returning(
                    "websockets.asyncio.client.ClientConnection",
                    entry(&[]),
                )),
            ),
            (
                s(&["aiohttp.ClientSession.ws_connect"]),
                async_safe(returning("aiohttp.ClientWebSocketResponse", entry(&[]))),
            ),
        ]
    }

    fn async_safe(e: EcosystemEntry) -> EcosystemEntry {
        EcosystemEntry {
            async_safe: true,
            ..e
        }
    }

    fn with_prefix(prefix: &str, names: &[&str]) -> Vec<String> {
        names.iter().map(|n| format!("{prefix}.{n}")).collect()
    }

    fn s(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    const BLOCKING: &[ExecEffectKind] = ExecEffectKind::BLOCKING;

    const VERBS8: &[&str] = &[
        "get", "post", "put", "delete", "patch", "head", "options", "request",
    ];
    /// Request methods of `httpx.Client` / `httpx.AsyncClient`.
    const HTTPX_CLIENT_VERBS: &[&str] = &[
        "get", "post", "put", "delete", "patch", "head", "options", "request", "send", "stream",
    ];
    const SQLITE3_EXECUTES: &[&str] = &["execute", "executemany", "executescript"];
    /// DB-API cursor methods that send a statement.
    const DBAPI_EXECUTES: &[&str] = &["execute", "executemany", "callproc"];
    const COMMIT_ROLLBACK: &[&str] = &["commit", "rollback"];
    const SA_CONNECTION_QUERIES: &[&str] = &["execute", "exec_driver_sql", "scalar", "scalars"];
    const SA_SESSION_QUERIES: &[&str] = &[
        "execute", "scalars", "scalar", "get", "get_one", "refresh", "commit", "flush",
    ];
    /// `sqlalchemy.orm.Query` methods that run the query (iterating it included).
    const SA_QUERY_RUNS: &[&str] = &[
        "all",
        "first",
        "one",
        "one_or_none",
        "scalar",
        "count",
        "get",
        "update",
        "delete",
        "__iter__",
    ];
    /// `sqlalchemy.orm.Query` methods that only build another `Query`.
    const SA_QUERY_BUILDERS: &[&str] = &[
        "filter",
        "filter_by",
        "where",
        "join",
        "outerjoin",
        "order_by",
        "group_by",
        "having",
        "limit",
        "offset",
        "slice",
        "distinct",
        "options",
        "with_entities",
        "add_columns",
        "select_from",
        "execution_options",
        "populate_existing",
        "with_for_update",
        "params",
        "yield_per",
        "union",
        "union_all",
        "correlate",
        "enable_eagerloads",
        "from_statement",
        "with_parent",
    ];
    const SA_ASYNC_SESSION_QUERIES: &[&str] = &[
        "execute",
        "scalars",
        "scalar",
        "get",
        "get_one",
        "refresh",
        "commit",
        "flush",
        "stream",
        "stream_scalars",
    ];
    const SQLMODEL_SESSION_QUERIES: &[&str] = &[
        "exec", "execute", "scalars", "scalar", "get", "get_one", "refresh", "commit", "flush",
    ];
    /// The async session classes (`EFF110`: one operation at a time while unbound).
    const ASYNC_SESSIONS: &[&str] = &[
        "sqlalchemy.ext.asyncio.AsyncSession",
        "sqlmodel.ext.asyncio.session.AsyncSession",
    ];
    const DJANGO_QUERYSET: &str = "django.db.models.query.QuerySet";
    /// Django `QuerySet` methods that run the query (their `a*` variants are coroutines).
    const QUERYSET_RUNS: &[&str] = &[
        "get",
        "first",
        "last",
        "exists",
        "count",
        "create",
        "update",
        "delete",
        "get_or_create",
        "update_or_create",
        "aggregate",
    ];
    /// Django `QuerySet` and pymongo `Collection` bulk APIs: many rows in one statement.
    const QUERYSET_BULK: &[&str] = &["bulk_create", "in_bulk"];
    const MONGO_BULK: &[&str] = &["insert_many", "bulk_write"];
    const REDIS_COMMANDS: &[&str] = &[
        "get",
        "set",
        "setex",
        "mget",
        "mset",
        "delete",
        "exists",
        "expire",
        "ttl",
        "incr",
        "decr",
        "hget",
        "hset",
        "hmget",
        "hgetall",
        "hdel",
        "lpush",
        "rpush",
        "lpop",
        "rpop",
        "lrange",
        "sadd",
        "srem",
        "smembers",
        "zadd",
        "zrange",
        "publish",
        "keys",
        "eval",
        "incrby",
        "incrbyfloat",
        "decrby",
        "getset",
        "getdel",
        "setnx",
        "expireat",
        "pexpire",
        "persist",
        "pttl",
        "unlink",
        "rename",
        "hincrby",
        "hexists",
        "hkeys",
        "hvals",
        "hlen",
        "hsetnx",
        "llen",
        "lindex",
        "lrem",
        "lset",
        "ltrim",
        "blpop",
        "brpop",
        "sismember",
        "smismember",
        "scard",
        "spop",
        "srandmember",
        "zrem",
        "zscore",
        "zincrby",
        "zcard",
        "zcount",
        "zrank",
        "zrangebyscore",
        "zrevrange",
        "zrevrangebyscore",
        "zremrangebyscore",
        "xadd",
        "xread",
        "xreadgroup",
        "xrange",
        "xrevrange",
        "xlen",
        "xack",
        "xdel",
        "xtrim",
        "evalsha",
    ];
    /// asyncpg methods that send one statement, and those that send many rows at once.
    const ASYNCPG_QUERIES: &[&str] = &["execute", "fetch", "fetchrow", "fetchval"];
    const ASYNCPG_BULK: &[&str] = &[
        "executemany",
        "fetchmany",
        "copy_from_table",
        "copy_from_query",
        "copy_to_table",
        "copy_records_to_table",
    ];
    /// The spellings of asyncpg's connection and pool classes (re-exported at the top level).
    const ASYNCPG_CONNECTIONS: &[&str] = &["asyncpg.Connection", "asyncpg.connection.Connection"];
    const ASYNCPG_POOLS: &[&str] = &["asyncpg.Pool", "asyncpg.pool.Pool"];
    const MOTOR: &str = "motor.motor_asyncio";
    const MONGO_COLLECTION_OPS: &[&str] = &[
        "find_one",
        "insert_one",
        "update_one",
        "update_many",
        "replace_one",
        "delete_one",
        "delete_many",
        "count_documents",
        "estimated_document_count",
        "distinct",
        "aggregate",
        "find_one_and_update",
        "find_one_and_replace",
        "find_one_and_delete",
    ];
    /// `boto3.client` operations that are network calls only.
    const BOTO3_CLIENT_CALLS: &[&str] = &[
        "get_object",
        "put_object",
        "delete_object",
        "delete_objects",
        "head_object",
        "copy_object",
        "list_objects",
        "list_objects_v2",
        "list_buckets",
        "upload_file",
        "download_file",
        "upload_fileobj",
        "download_fileobj",
        "create_multipart_upload",
        "upload_part",
        "complete_multipart_upload",
        "batch_get_item",
        "batch_write_item",
        "transact_get_items",
        "transact_write_items",
        "send_message",
        "send_message_batch",
        "receive_message",
        "delete_message",
        "delete_message_batch",
        "publish",
        "invoke",
        "assume_role",
        "get_caller_identity",
        "get_secret_value",
        "get_parameter",
        "get_parameters",
        "get_parameters_by_path",
        "encrypt",
        "decrypt",
        "put_record",
        "put_records",
        "put_record_batch",
        "put_events",
        "send_email",
        // DynamoDB reads that page through results in loops by design.
        "query",
        "scan",
    ];
    /// DynamoDB item reads and writes (client operations and `Table` actions).
    const DYNAMODB_ITEM_CALLS: &[&str] = &["get_item", "put_item", "update_item", "delete_item"];
    /// pandas readers of a URL or a buffer, with the name of their source parameter.
    const PANDAS_URL_READERS: &[(&str, &str)] = &[
        ("read_csv", "filepath_or_buffer"),
        ("read_table", "filepath_or_buffer"),
        ("read_fwf", "filepath_or_buffer"),
        ("read_excel", "io"),
        ("read_json", "path_or_buf"),
        ("read_parquet", "path"),
        ("read_feather", "path"),
        ("read_orc", "path"),
        ("read_pickle", "filepath_or_buffer"),
        ("read_xml", "path_or_buffer"),
        ("read_stata", "filepath_or_buffer"),
    ];
    const PANDAS_URL_SCHEMES: &[&str] = &[
        "http://", "https://", "ftp://", "s3://", "gs://", "gcs://", "az://", "abfs://",
        "abfss://", "hdfs://", "sftp://",
    ];

    /// The source of a reader is a URL to download (`effects`) or a buffer to parse.
    fn source_case(param: &str, url: bool) -> ArgCase {
        ArgCase {
            arg: Some(0),
            param: Some(param.into()),
            prefix: if url {
                s(PANDAS_URL_SCHEMES)
            } else {
                Vec::new()
            },
            instance_of: if url {
                Vec::new()
            } else {
                s(&["io.StringIO", "io.BytesIO"])
            },
            effects: Some(if url { vec![BlockingNet] } else { Vec::new() }),
            ..ArgCase::default()
        }
    }

    /// `cursor(name)` with a name returns a server-side cursor of `class`.
    fn named_cursor(class: &str) -> ArgCase {
        ArgCase {
            arg: Some(0),
            param: Some("name".into()),
            nonempty: true,
            return_type: Some(class.into()),
            ..ArgCase::default()
        }
    }

    fn with_cases(cases: Vec<ArgCase>, e: EcosystemEntry) -> EcosystemEntry {
        EcosystemEntry { cases, ..e }
    }

    /// Every Layer 2 entry (docs/ANALYSIS_AND_KB_SPEC.md section 4.2), as `(symbols, expected
    /// entry without symbol)`.
    fn layer2_required() -> Vec<(Vec<String>, EcosystemEntry)> {
        [
            one_at_a_time_apis(),
            standard_library(),
            http_clients(),
            boto3_clients(),
            sync_databases(),
            async_databases(),
            data_libraries(),
            locks_and_transactions(),
            pandas_url_readers(),
            synchronous_waits(),
            lazy_iterators(),
            environment_reads(),
            ordered_outputs(),
            pydantic_models(),
            llm_requests(),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// The requests of the LLM SDKs, reached through the clients' attributes, and the builders
    /// returning a client of the receiver's class.
    fn llm_requests() -> Vec<(Vec<String>, EcosystemEntry)> {
        const BUILDERS: &[&str] = &["with_options", "copy"];
        vec![
            (
                with_prefix_all(
                    &["openai.OpenAI", "openai.AzureOpenAI", "anthropic.Anthropic"],
                    BUILDERS,
                ),
                returning(SELF_RETURN_TYPE, entry(&[])),
            ),
            (
                with_prefix_all(
                    &[
                        "openai.AsyncOpenAI",
                        "openai.AsyncAzureOpenAI",
                        "anthropic.AsyncAnthropic",
                    ],
                    BUILDERS,
                ),
                async_safe(returning(SELF_RETURN_TYPE, entry(&[]))),
            ),
            (
                s(&[
                    "openai.resources.chat.Completions.create",
                    "openai.resources.responses.Responses.create",
                    "openai.resources.Embeddings.create",
                    "anthropic.resources.Messages.create",
                    "google.genai.models.Models.generate_content",
                    "google.genai.models.Models.embed_content",
                ]),
                entry(&[BlockingNet]),
            ),
            (
                s(&[
                    "openai.resources.chat.AsyncCompletions.create",
                    "openai.resources.responses.AsyncResponses.create",
                    "openai.resources.AsyncEmbeddings.create",
                    "anthropic.resources.AsyncMessages.create",
                    "google.genai.models.AsyncModels.generate_content",
                    "google.genai.models.AsyncModels.embed_content",
                ]),
                async_safe(entry(&[])),
            ),
        ]
    }

    /// How a pydantic model stores and deletes its attributes, and what a field left out at
    /// construction holds.
    fn pydantic_models() -> Vec<(Vec<String>, EcosystemEntry)> {
        vec![
            (
                with_prefix("pydantic.BaseModel", &["__setattr__", "__delattr__"]),
                EcosystemEntry {
                    stores_on_instance: true,
                    ..entry(&[])
                },
            ),
            (
                s(&["pydantic.BaseModel"]),
                EcosystemEntry {
                    keeps_hashable_defaults: true,
                    ..entry(&[])
                },
            ),
        ]
    }

    /// `EFF112`: the calls writing an argument in the order of its elements, with no other fact
    /// (the file writes of `pathlib` are in [`ORDERED_OUTPUTS`] too).
    fn ordered_outputs() -> Vec<(Vec<String>, EcosystemEntry)> {
        vec![(
            s(&[
                "builtins.print",
                "json.dumps",
                "json.dump",
                "pprint.pprint",
                "pprint.pformat",
                "pprint.pp",
                "yaml.dump",
                "yaml.safe_dump",
            ]),
            entry(&[]),
        )]
    }

    /// The calls and module attributes `reads-environment` effect boundaries forbid; they carry
    /// no other fact.
    fn environment_reads() -> Vec<(Vec<String>, EcosystemEntry)> {
        const PLATFORM: &[&str] = &[
            "architecture",
            "machine",
            "node",
            "platform",
            "processor",
            "python_build",
            "python_compiler",
            "python_branch",
            "python_implementation",
            "python_revision",
            "python_version",
            "python_version_tuple",
            "release",
            "system",
            "version",
            "uname",
            "win32_ver",
            "win32_edition",
            "win32_is_iot",
            "mac_ver",
            "libc_ver",
            "freedesktop_os_release",
            "android_ver",
            "ios_ver",
        ];
        let tagged = |access| EcosystemEntry {
            environment: Some(access),
            ..entry(&[])
        };
        vec![
            (
                [
                    s(&["os.getenv", "os.getenvb"]),
                    with_prefix("platform", PLATFORM),
                    s(&[
                        "importlib.metadata.version",
                        "importlib.metadata.distribution",
                        "shutil.which",
                        "sysconfig.get_config_var",
                    ]),
                ]
                .concat(),
                tagged(EnvironmentAccess::Call),
            ),
            (
                s(&[
                    "os.environ",
                    "os.environb",
                    "os.name",
                    "sys.platform",
                    "sys.version",
                    "sys.version_info",
                    "sys.hexversion",
                    "sys.implementation",
                    "sys.flags",
                ]),
                tagged(EnvironmentAccess::Read),
            ),
            (
                s(&["importlib.util.find_spec", "importlib.import_module"]),
                tagged(EnvironmentAccess::Import),
            ),
        ]
    }

    fn standard_library() -> Vec<(Vec<String>, EcosystemEntry)> {
        let offload = |arg| EcosystemEntry {
            offloads_blocking_arg: Some(arg),
            ..async_safe(entry(&[]))
        };
        let excluded = EcosystemEntry {
            excluded: true,
            ..entry(&[])
        };
        let copy_sanitizer = |depth| EcosystemEntry {
            copy_sanitizer: Some(depth),
            ..entry(&[])
        };
        // The case of an argument that ends the wait at once.
        let ends_wait = |arg: u8, param: Option<&str>, case: ArgCase| ArgCase {
            arg: Some(arg),
            param: param.map(Into::into),
            effects: Some(Vec::new()),
            ..case
        };
        let zero = ArgCase {
            equals: Some(ArgLiteral::Int(0)),
            ..ArgCase::default()
        };
        let local_host = |param: Option<&str>| {
            vec![
                ends_wait(
                    0,
                    param,
                    ArgCase {
                        numeric_address: true,
                        ..ArgCase::default()
                    },
                ),
                ends_wait(
                    0,
                    param,
                    ArgCase {
                        one_of: s(&["localhost"]),
                        ..ArgCase::default()
                    },
                ),
            ]
        };
        vec![
            (
                s(&["time.sleep"]),
                with_cases(vec![ends_wait(0, None, zero.clone())], entry(&[Sleep])),
            ),
            (
                with_prefix(
                    "subprocess",
                    &[
                        "run",
                        "call",
                        "check_call",
                        "check_output",
                        "Popen.communicate",
                    ],
                ),
                entry(&[BlockingSubprocess]),
            ),
            (
                s(&["subprocess.Popen.wait"]),
                with_cases(
                    vec![ends_wait(0, Some("timeout"), zero)],
                    entry(&[BlockingSubprocess]),
                ),
            ),
            (
                s(&["contextlib.suppress"]),
                EcosystemEntry {
                    exit_suppresses: Some(true),
                    ..entry(&[])
                },
            ),
            (
                s(&[
                    "builtins.open",
                    "io.open",
                    "pathlib.Path.read_text",
                    "pathlib.Path.read_bytes",
                    "pathlib.Path.write_text",
                    "pathlib.Path.write_bytes",
                    "pathlib.Path.open",
                    "os.listdir",
                    "os.scandir",
                    "os.walk().__iter__",
                    "shutil.copy",
                    "shutil.copy2",
                    "shutil.copytree",
                    "shutil.rmtree",
                    "os.stat",
                    "os.path.exists",
                    "os.path.isfile",
                    "os.path.isdir",
                    "os.path.realpath",
                    "pathlib.Path.exists",
                    "pathlib.Path.stat",
                    "pathlib.Path.is_file",
                    "pathlib.Path.is_dir",
                    "pathlib.Path.resolve",
                    "starlette.staticfiles.StaticFiles.lookup_path",
                ]),
                entry(&[BlockingFs]),
            ),
            (s(&["socket.getfqdn"]), entry(&[BlockingNet])),
            (
                s(&["socket.gethostbyname"]),
                with_cases(local_host(None), entry(&[BlockingNet])),
            ),
            // A numeric host is parsed, never resolved.
            (
                s(&["socket.getaddrinfo"]),
                with_cases(
                    [
                        local_host(Some("host")),
                        vec![ArgCase {
                            arg: Some(5),
                            param: Some("flags".into()),
                            flag: s(&[
                                "socket.AI_NUMERICHOST",
                                "socket.AddressInfo.AI_NUMERICHOST",
                            ]),
                            effects: Some(Vec::new()),
                            ..ArgCase::default()
                        }],
                    ]
                    .concat(),
                    entry(&[BlockingNet]),
                ),
            ),
            (
                with_prefix("asyncio", &["StreamWriter.write", "sleep", "wait_for"]),
                async_safe(entry(&[])),
            ),
            (
                s(&["asyncio.gather"]),
                returning("asyncio.Future", async_safe(entry(&[]))),
            ),
            (
                s(&[
                    "asyncio.to_thread",
                    "anyio.to_thread.run_sync",
                    "anyio.to_process.run_sync",
                    "anyio.to_interpreter.run_sync",
                    "trio.to_thread.run_sync",
                    "starlette.concurrency.run_in_threadpool",
                    "fastapi.concurrency.run_in_threadpool",
                    "asgiref.sync.sync_to_async",
                    "asyncer.asyncify",
                    "channels.db.database_sync_to_async",
                ]),
                offload(0),
            ),
            (
                with_prefix(
                    "asyncio",
                    &[
                        "AbstractEventLoop.run_in_executor",
                        "BaseEventLoop.run_in_executor",
                    ],
                ),
                returning("asyncio.Future", offload(1)),
            ),
            (
                [
                    with_prefix(
                        "logging.Logger",
                        &[
                            "debug",
                            "info",
                            "warning",
                            "error",
                            "critical",
                            "exception",
                            "log",
                        ],
                    ),
                    with_prefix(
                        "logging",
                        &[
                            "debug",
                            "info",
                            "warning",
                            "error",
                            "critical",
                            "exception",
                            "log",
                        ],
                    ),
                ]
                .concat(),
                excluded,
            ),
            (
                s(&["copy.deepcopy", "pydantic.BaseModel.__deepcopy__"]),
                copy_sanitizer(CopyDepth::Deep),
            ),
            // `MUT114`: `html.escape(text).split("\n")` is a list by construction.
            (s(&["html.escape"]), returning("builtins.str", entry(&[]))),
            (
                // `model_copy(deep=True)` copies deeply (`heki_effect::readonly`).
                s(&[
                    "copy.copy",
                    "copy.replace",
                    "dataclasses.replace",
                    "pydantic.BaseModel.model_copy",
                    "builtins.dict.copy",
                    "builtins.list.copy",
                    "builtins.set.copy",
                    "builtins.dict",
                    "builtins.list",
                    "builtins.tuple",
                    "builtins.set",
                    "builtins.frozenset",
                    "builtins.bytearray",
                    "collections.OrderedDict",
                    "collections.deque",
                ]),
                copy_sanitizer(CopyDepth::Shallow),
            ),
            (
                with_prefix(
                    "asyncio",
                    &["get_running_loop", "get_event_loop", "new_event_loop"],
                ),
                returning("asyncio.AbstractEventLoop", entry(&[])),
            ),
            (
                with_prefix(
                    "asyncio",
                    &[
                        "AbstractEventLoop.create_future",
                        "BaseEventLoop.create_future",
                    ],
                ),
                returning("asyncio.Future", entry(&[])),
            ),
            (
                s(&["sqlite3.connect"]),
                with_cases(
                    {
                        let in_memory = |case: ArgCase| ArgCase {
                            arg: Some(0),
                            param: Some("database".into()),
                            effects: Some(vec![UnpooledClientAlloc]),
                            return_type: Some(SQLITE3_MEMORY.into()),
                            ..case
                        };
                        vec![
                            in_memory(ArgCase {
                                one_of: s(&[":memory:"]),
                                ..ArgCase::default()
                            }),
                            in_memory(ArgCase {
                                prefix: s(&["file::memory:"]),
                                ..ArgCase::default()
                            }),
                            in_memory(ArgCase {
                                contains: s(&["mode=memory"]),
                                ..ArgCase::default()
                            }),
                            ArgCase {
                                arg: Some(0),
                                param: Some("database".into()),
                                names_file: true,
                                return_type: Some(SQLITE3_FILE.into()),
                                emits: Some(Emits::Always),
                                ..ArgCase::default()
                            },
                        ]
                    },
                    returning(
                        "sqlite3.Connection",
                        entry(&[BlockingDb, UnpooledClientAlloc]),
                    ),
                ),
            ),
            // The connections of an in-memory database and of one named by a parameter.
            (
                [
                    with_prefix(SQLITE3_MEMORY, SQLITE3_EXECUTES),
                    with_prefix(&format!("{SQLITE3_MEMORY}.cursor()"), SQLITE3_EXECUTES),
                ]
                .concat(),
                returning(&format!("{SQLITE3_MEMORY}.cursor()"), entry(&[DbQuery])),
            ),
            (
                s(&[&format!("{SQLITE3_MEMORY}.cursor")]),
                returning(&format!("{SQLITE3_MEMORY}.cursor()"), entry(&[])),
            ),
            (
                [
                    with_prefix(SQLITE3_FILE, SQLITE3_EXECUTES),
                    with_prefix(&format!("{SQLITE3_FILE}.cursor()"), SQLITE3_EXECUTES),
                ]
                .concat(),
                returning(
                    &format!("{SQLITE3_FILE}.cursor()"),
                    entry(&[BlockingDb, DbQuery]),
                ),
            ),
            (
                s(&[&format!("{SQLITE3_FILE}.cursor")]),
                returning(&format!("{SQLITE3_FILE}.cursor()"), entry(&[])),
            ),
            (
                [
                    with_prefix(SQLITE3_FILE, COMMIT_ROLLBACK),
                    with_prefix(
                        &format!("{SQLITE3_FILE}.cursor()"),
                        &["fetchone", "fetchmany", "fetchall"],
                    ),
                ]
                .concat(),
                entry(&[BlockingDb]),
            ),
            (
                s(&["sqlite3.Connection.cursor"]),
                returning("sqlite3.Cursor", entry(&[])),
            ),
            (
                [
                    with_prefix("sqlite3.Connection", SQLITE3_EXECUTES),
                    with_prefix("sqlite3.Cursor", SQLITE3_EXECUTES),
                ]
                .concat(),
                returning("sqlite3.Cursor", entry(&[BlockingDb, DbQuery])),
            ),
            (
                // Reading further rows of an executed statement is not a new query.
                [
                    with_prefix("sqlite3.Connection", COMMIT_ROLLBACK),
                    with_prefix("sqlite3.Cursor", &["fetchone", "fetchmany", "fetchall"]),
                ]
                .concat(),
                entry(&[BlockingDb]),
            ),
        ]
    }

    fn http_clients() -> Vec<(Vec<String>, EcosystemEntry)> {
        vec![
            (
                [
                    s(&["httpx.stream"]),
                    with_prefix("requests", VERBS8),
                    with_prefix("httpx", VERBS8),
                ]
                .concat(),
                entry(&[BlockingNet, UnpooledClientAlloc]),
            ),
            // A `data:` URL is decoded in memory.
            (
                s(&["urllib.request.urlopen"]),
                with_cases(
                    vec![ArgCase {
                        arg: Some(0),
                        param: Some("url".into()),
                        prefix: s(&["data:"]),
                        effects: Some(Vec::new()),
                        ..ArgCase::default()
                    }],
                    entry(&[BlockingNet, UnpooledClientAlloc]),
                ),
            ),
            (
                [
                    with_prefix("requests.Session", VERBS8),
                    s(&["requests.Session.send", "urllib3.request"]),
                    with_prefix("httpx.Client", HTTPX_CLIENT_VERBS),
                    with_prefix("urllib3.PoolManager", &["request", "urlopen"]),
                    with_prefix("urllib3.ProxyManager", &["request", "urlopen"]),
                    with_prefix("urllib3.HTTPConnectionPool", &["request", "urlopen"]),
                    with_prefix("urllib3.HTTPSConnectionPool", &["request", "urlopen"]),
                    s(&[
                        "celery.Celery.send_task",
                        "celery.Task.delay",
                        "celery.Task.apply_async",
                    ]),
                ]
                .concat(),
                entry(&[BlockingNet]),
            ),
            (
                // Constructors that do no I/O until the client is used.
                s(&[
                    "requests.Session",
                    "httpx.Client",
                    "urllib3.PoolManager",
                    "urllib3.ProxyManager",
                    "urllib3.HTTPConnectionPool",
                    "urllib3.HTTPSConnectionPool",
                    "boto3.client",
                    "boto3.resource",
                    "redis.Redis",
                    "redis.StrictRedis",
                    "redis.ConnectionPool",
                    "redis.BlockingConnectionPool",
                    "redis.ConnectionPool.from_url",
                    "redis.BlockingConnectionPool.from_url",
                    "psycopg_pool.ConnectionPool",
                    "openai.OpenAI",
                    "openai.AzureOpenAI",
                    "anthropic.Anthropic",
                    "google.genai.Client",
                ]),
                entry(&[UnpooledClientAlloc]),
            ),
            (
                // `connect=True` (the default) resolves a `mongodb+srv://` URI at once.
                s(&["pymongo.MongoClient"]),
                with_cases(
                    vec![
                        ArgCase {
                            arg: Some(4),
                            param: Some("connect".into()),
                            equals: Some(ArgLiteral::Bool(false)),
                            effects: Some(vec![UnpooledClientAlloc]),
                            ..ArgCase::default()
                        },
                        ArgCase {
                            arg: Some(0),
                            param: Some("host".into()),
                            prefix: s(&["mongodb+srv://"]),
                            effects: Some(vec![BlockingNet, UnpooledClientAlloc]),
                            ..ArgCase::default()
                        },
                    ],
                    entry(&[UnpooledClientAlloc]),
                ),
            ),
            (
                s(&["requests.session"]),
                returning("requests.Session", entry(&[UnpooledClientAlloc])),
            ),
            (
                s(&["urllib3.connection_from_url"]),
                returning("urllib3.HTTPConnectionPool", entry(&[UnpooledClientAlloc])),
            ),
            (
                s(&[
                    "httpx.AsyncClient",
                    "aiohttp.ClientSession",
                    "aiohttp.request",
                    "redis.asyncio.Redis",
                    "redis.asyncio.ConnectionPool",
                    "redis.asyncio.BlockingConnectionPool",
                    "redis.asyncio.ConnectionPool.from_url",
                    "redis.asyncio.BlockingConnectionPool.from_url",
                    "pymongo.AsyncMongoClient",
                    "psycopg_pool.AsyncConnectionPool",
                    "motor.motor_asyncio.AsyncIOMotorClient",
                    "openai.AsyncOpenAI",
                    "openai.AsyncAzureOpenAI",
                    "anthropic.AsyncAnthropic",
                ]),
                async_safe(entry(&[UnpooledClientAlloc])),
            ),
            (
                [
                    with_prefix("httpx.AsyncClient", HTTPX_CLIENT_VERBS),
                    with_prefix("aiohttp.ClientSession", VERBS8),
                    s(&["redis.asyncio.Redis.scan"]),
                ]
                .concat(),
                async_safe(entry(&[])),
            ),
        ]
    }

    fn boto3_clients() -> Vec<(Vec<String>, EcosystemEntry)> {
        vec![
            (
                s(&["boto3.Session.client"]),
                returning("boto3.client", entry(&[UnpooledClientAlloc])),
            ),
            (
                s(&["boto3.Session.resource"]),
                returning("boto3.resource", entry(&[UnpooledClientAlloc])),
            ),
            (
                [
                    with_prefix("boto3.client", BOTO3_CLIENT_CALLS),
                    with_prefix(
                        "boto3.resource.Bucket",
                        &[
                            "put_object",
                            "upload_file",
                            "download_file",
                            "upload_fileobj",
                            "download_fileobj",
                        ],
                    ),
                    with_prefix(
                        "boto3.resource.Object",
                        &[
                            "get",
                            "put",
                            "delete",
                            "load",
                            "upload_file",
                            "download_file",
                        ],
                    ),
                    with_prefix("boto3.resource.Table", &["query", "scan"]),
                ]
                .concat(),
                entry(&[BlockingNet]),
            ),
            (
                [
                    with_prefix("boto3.client", DYNAMODB_ITEM_CALLS),
                    with_prefix("boto3.resource.Table", DYNAMODB_ITEM_CALLS),
                ]
                .concat(),
                entry(&[BlockingNet, DbQuery]),
            ),
        ]
    }

    fn sync_databases() -> Vec<(Vec<String>, EcosystemEntry)> {
        vec![
            (
                [
                    with_prefix("sqlalchemy.engine.Connection", SA_CONNECTION_QUERIES),
                    with_prefix("sqlalchemy.orm.Session", SA_SESSION_QUERIES),
                    with_prefix("sqlalchemy.orm.Query", SA_QUERY_RUNS),
                    with_prefix("sqlmodel.Session", SQLMODEL_SESSION_QUERIES),
                    s(&["psycopg.Cursor.executemany"]),
                    with_prefix("psycopg2.extensions.cursor", DBAPI_EXECUTES),
                    with_prefix("pymysql.cursors.Cursor", DBAPI_EXECUTES),
                    with_prefix("django.db.backends.utils.CursorWrapper", DBAPI_EXECUTES),
                    with_prefix(DJANGO_QUERYSET, QUERYSET_RUNS),
                    s(&[
                        "redis.client.Pipeline.execute",
                        "redis.client.Pipeline.watch",
                    ]),
                    with_prefix("redis.Redis", REDIS_COMMANDS),
                    s(&["pymongo.database.Database.command"]),
                    with_prefix("pymongo.collection.Collection", MONGO_COLLECTION_OPS),
                    with_prefix("pandas", &["read_sql", "read_sql_query", "read_sql_table"]),
                ]
                .concat(),
                entry(&[BlockingDb, DbQuery]),
            ),
            (
                [
                    with_prefix("sqlalchemy.engine.Connection", COMMIT_ROLLBACK),
                    with_prefix("psycopg.Connection", COMMIT_ROLLBACK),
                    with_prefix("psycopg2.extensions.connection", COMMIT_ROLLBACK),
                    with_prefix("pymysql.connections.Connection", COMMIT_ROLLBACK),
                    // Connects a `defer_connect=True` connection.
                    s(&["pymysql.connections.Connection.connect"]),
                    // A cursor loop or a bulk API by design: blocking, but no N+1 query.
                    s(&["redis.Redis.scan", "pandas.DataFrame.to_sql"]),
                    with_prefix(DJANGO_QUERYSET, QUERYSET_BULK),
                    with_prefix("pymongo.collection.Collection", MONGO_BULK),
                ]
                .concat(),
                entry(&[BlockingDb]),
            ),
            (
                s(&["sqlalchemy.create_engine", "sqlmodel.create_engine"]),
                returning("sqlalchemy.engine.Engine", entry(&[UnpooledClientAlloc])),
            ),
            (
                s(&["sqlalchemy.engine.Engine.connect"]),
                returning("sqlalchemy.engine.Connection", entry(&[BlockingDb])),
            ),
            (
                s(&["sqlalchemy.engine.Engine.begin"]),
                EcosystemEntry {
                    sync_lock_or_tx: true,
                    ..returning("sqlalchemy.engine.Connection", entry(&[BlockingDb]))
                },
            ),
            (
                // Its sessions stay untyped: `class_=AsyncSession` would make them async.
                s(&["sqlalchemy.orm.sessionmaker"]),
                returning("sqlalchemy.orm.sessionmaker", entry(&[])),
            ),
            (
                // Lazy: they only build a `Query`, which runs when executed or iterated.
                [
                    s(&["sqlalchemy.orm.Session.query"]),
                    with_prefix("sqlalchemy.orm.Query", SA_QUERY_BUILDERS),
                ]
                .concat(),
                returning("sqlalchemy.orm.Query", entry(&[])),
            ),
            (
                s(&["psycopg.connect", "psycopg.Connection.connect"]),
                returning(
                    "psycopg.Connection",
                    entry(&[BlockingDb, UnpooledClientAlloc]),
                ),
            ),
            (
                s(&["psycopg.Connection.cursor"]),
                with_cases(
                    vec![named_cursor("psycopg.ServerCursor")],
                    returning("psycopg.Cursor", entry(&[])),
                ),
            ),
            (
                s(&["psycopg.ServerCursor.execute"]),
                returning("psycopg.ServerCursor", entry(&[BlockingDb, DbQuery])),
            ),
            (
                s(&["psycopg.ServerCursor.executemany"]),
                entry(&[BlockingDb, DbQuery]),
            ),
            (
                // Each fetch sends `FETCH` for rows of the declared statement.
                with_prefix(
                    "psycopg.ServerCursor",
                    &["fetchone", "fetchmany", "fetchall", "scroll", "__iter__"],
                ),
                entry(&[BlockingDb]),
            ),
            (
                // A new cursor, or the cursor itself, holding the result.
                s(&["psycopg.Connection.execute", "psycopg.Cursor.execute"]),
                returning("psycopg.Cursor", entry(&[BlockingDb, DbQuery])),
            ),
            (
                s(&["psycopg_pool.ConnectionPool.connection"]),
                returning("psycopg.Connection", entry(&[BlockingDb])),
            ),
            (
                s(&["psycopg2.connect"]),
                returning(
                    "psycopg2.extensions.connection",
                    entry(&[BlockingDb, UnpooledClientAlloc]),
                ),
            ),
            (
                s(&["psycopg2.extensions.connection.cursor"]),
                returning("psycopg2.extensions.cursor", entry(&[])),
            ),
            (
                s(&["pymysql.connect"]),
                with_cases(
                    vec![ArgCase {
                        param: Some("defer_connect".into()),
                        equals: Some(ArgLiteral::Bool(true)),
                        effects: Some(vec![UnpooledClientAlloc]),
                        ..ArgCase::default()
                    }],
                    returning(
                        "pymysql.connections.Connection",
                        entry(&[BlockingDb, UnpooledClientAlloc]),
                    ),
                ),
            ),
            (
                s(&["pymysql.connections.Connection.cursor"]),
                returning("pymysql.cursors.Cursor", entry(&[])),
            ),
            (
                s(&["django.db.connection.cursor"]),
                returning(
                    "django.db.backends.utils.CursorWrapper",
                    entry(&[BlockingDb]),
                ),
            ),
            (
                // Lazy: they only build another QuerySet.
                with_prefix(DJANGO_QUERYSET, &["filter", "exclude", "all", "order_by"]),
                returning(DJANGO_QUERYSET, entry(&[])),
            ),
            (
                s(&["redis.from_url", "redis.Redis.from_url"]),
                returning("redis.Redis", entry(&[UnpooledClientAlloc])),
            ),
            (
                // Only buffers commands until `execute()`; a buffered command returns the pipeline.
                [
                    s(&["redis.Redis.pipeline"]),
                    with_prefix("redis.client.Pipeline", REDIS_COMMANDS),
                ]
                .concat(),
                returning("redis.client.Pipeline", entry(&[])),
            ),
            (
                with_prefix(
                    "pymongo.MongoClient",
                    &["get_database", "get_default_database"],
                ),
                returning("pymongo.database.Database", entry(&[])),
            ),
            (
                s(&["pymongo.database.Database.get_collection"]),
                returning("pymongo.collection.Collection", entry(&[])),
            ),
            // An in-process database that may be in memory: its statements wait on nothing known.
            (
                s(&["duckdb.connect"]),
                returning("duckdb.DuckDBPyConnection", entry(&[UnpooledClientAlloc])),
            ),
            (
                s(&["duckdb.DuckDBPyConnection.execute", "duckdb.execute"]),
                returning("duckdb.DuckDBPyConnection", entry(&[DbQuery])),
            ),
            (
                s(&["duckdb.DuckDBPyConnection.cursor"]),
                returning("duckdb.DuckDBPyConnection", entry(&[])),
            ),
        ]
    }

    fn async_databases() -> Vec<(Vec<String>, EcosystemEntry)> {
        let async_queryset_runs: Vec<String> = QUERYSET_RUNS
            .iter()
            .map(|m| format!("{DJANGO_QUERYSET}.a{m}"))
            .collect();
        vec![
            (
                [
                    with_prefix(
                        "sqlalchemy.ext.asyncio.AsyncConnection",
                        SA_CONNECTION_QUERIES,
                    ),
                    s(&["sqlalchemy.ext.asyncio.AsyncConnection.stream"]),
                    s(&["psycopg.AsyncCursor.executemany"]),
                    async_queryset_runs,
                    s(&[
                        "redis.asyncio.client.Pipeline.execute",
                        "redis.asyncio.client.Pipeline.watch",
                    ]),
                    with_prefix("redis.asyncio.Redis", REDIS_COMMANDS),
                    s(&["pymongo.asynchronous.database.AsyncDatabase.command"]),
                    with_prefix(
                        "pymongo.asynchronous.collection.AsyncCollection",
                        MONGO_COLLECTION_OPS,
                    ),
                ]
                .concat(),
                async_safe(entry(&[DbQuery])),
            ),
            (
                [
                    QUERYSET_BULK
                        .iter()
                        .map(|m| format!("{DJANGO_QUERYSET}.a{m}"))
                        .collect(),
                    with_prefix(
                        "pymongo.asynchronous.collection.AsyncCollection",
                        MONGO_BULK,
                    ),
                ]
                .concat(),
                async_safe(entry(&[])),
            ),
            (
                [
                    with_prefix(
                        "sqlalchemy.ext.asyncio.AsyncSession",
                        SA_ASYNC_SESSION_QUERIES,
                    ),
                    with_prefix(
                        "sqlmodel.ext.asyncio.session.AsyncSession",
                        SQLMODEL_SESSION_QUERIES,
                    ),
                ]
                .concat()
                .into_iter()
                .filter(|q| !q.ends_with(".commit"))
                .collect(),
                one_at_a_time(OneAtATime::WhenUnbound, async_safe(entry(&[DbQuery]))),
            ),
            (
                with_prefix_all(ASYNC_SESSIONS, &["commit"]),
                one_at_a_time(OneAtATime::Unbinds, async_safe(entry(&[DbQuery]))),
            ),
            (
                with_prefix_all(ASYNC_SESSIONS, &["rollback", "close"]),
                one_at_a_time(OneAtATime::Unbinds, async_safe(entry(&[]))),
            ),
            (
                // `fn(sync_session, *args)` runs on the loop's thread through a greenlet.
                s(&["sqlalchemy.ext.asyncio.AsyncConnection.run_sync"]),
                EcosystemEntry {
                    sync_facade_arg: Some(0),
                    ..async_safe(entry(&[]))
                },
            ),
            (
                with_prefix_all(ASYNC_SESSIONS, &["run_sync"]),
                EcosystemEntry {
                    sync_facade_arg: Some(0),
                    ..one_at_a_time(OneAtATime::WhenUnbound, async_safe(entry(&[])))
                },
            ),
            (
                s(&["sqlalchemy.ext.asyncio.create_async_engine"]),
                async_safe(returning(
                    "sqlalchemy.ext.asyncio.AsyncEngine",
                    entry(&[UnpooledClientAlloc]),
                )),
            ),
            (
                with_prefix("sqlalchemy.ext.asyncio.AsyncEngine", &["connect", "begin"]),
                async_safe(returning(
                    "sqlalchemy.ext.asyncio.AsyncConnection",
                    entry(&[]),
                )),
            ),
            (
                s(&["sqlalchemy.ext.asyncio.async_sessionmaker"]),
                async_safe(returning(
                    "sqlalchemy.ext.asyncio.async_sessionmaker",
                    entry(&[]),
                )),
            ),
            (
                with_prefix(
                    "sqlalchemy.ext.asyncio.async_sessionmaker",
                    &["__call__", "begin"],
                ),
                async_safe(returning("sqlalchemy.ext.asyncio.AsyncSession", entry(&[]))),
            ),
            (
                s(&["psycopg.AsyncConnection.connect"]),
                async_safe(returning(
                    "psycopg.AsyncConnection",
                    entry(&[UnpooledClientAlloc]),
                )),
            ),
            (
                s(&["psycopg.AsyncConnection.cursor"]),
                with_cases(
                    vec![named_cursor("psycopg.AsyncServerCursor")],
                    async_safe(returning("psycopg.AsyncCursor", entry(&[]))),
                ),
            ),
            (
                s(&["psycopg.AsyncServerCursor.execute"]),
                async_safe(returning("psycopg.AsyncServerCursor", entry(&[DbQuery]))),
            ),
            (
                s(&["psycopg.AsyncServerCursor.executemany"]),
                async_safe(entry(&[DbQuery])),
            ),
            (
                s(&[
                    "psycopg.AsyncConnection.execute",
                    "psycopg.AsyncCursor.execute",
                ]),
                async_safe(returning("psycopg.AsyncCursor", entry(&[DbQuery]))),
            ),
            (
                s(&["psycopg_pool.AsyncConnectionPool.connection"]),
                async_safe(returning("psycopg.AsyncConnection", entry(&[]))),
            ),
            (
                s(&["redis.asyncio.from_url", "redis.asyncio.Redis.from_url"]),
                async_safe(returning(
                    "redis.asyncio.Redis",
                    entry(&[UnpooledClientAlloc]),
                )),
            ),
            (
                [
                    s(&["redis.asyncio.Redis.pipeline"]),
                    with_prefix("redis.asyncio.client.Pipeline", REDIS_COMMANDS),
                ]
                .concat(),
                async_safe(returning("redis.asyncio.client.Pipeline", entry(&[]))),
            ),
            (
                with_prefix(
                    "pymongo.AsyncMongoClient",
                    &["get_database", "get_default_database"],
                ),
                async_safe(returning(
                    "pymongo.asynchronous.database.AsyncDatabase",
                    entry(&[]),
                )),
            ),
            (
                s(&["pymongo.asynchronous.database.AsyncDatabase.get_collection"]),
                async_safe(returning(
                    "pymongo.asynchronous.collection.AsyncCollection",
                    entry(&[]),
                )),
            ),
            (
                s(&["asyncpg.create_pool"]),
                async_safe(returning("asyncpg.Pool", entry(&[UnpooledClientAlloc]))),
            ),
            (
                // Each query checks a connection out for itself: tasks may share the pool.
                with_prefix_all(ASYNCPG_POOLS, ASYNCPG_QUERIES),
                async_safe(entry(&[DbQuery])),
            ),
            (
                with_prefix_all(ASYNCPG_POOLS, ASYNCPG_BULK),
                async_safe(entry(&[])),
            ),
            (
                with_prefix_all(ASYNCPG_POOLS, &["acquire"]),
                async_safe(returning("asyncpg.Connection", entry(&[]))),
            ),
            (
                with_prefix(
                    &format!("{MOTOR}.AsyncIOMotorClient"),
                    &["get_database", "get_default_database", "__getitem__"],
                ),
                async_safe(returning(
                    &format!("{MOTOR}.AsyncIOMotorDatabase"),
                    entry(&[]),
                )),
            ),
            (
                with_prefix(
                    &format!("{MOTOR}.AsyncIOMotorDatabase"),
                    &["get_collection", "__getitem__"],
                ),
                async_safe(returning(
                    &format!("{MOTOR}.AsyncIOMotorCollection"),
                    entry(&[]),
                )),
            ),
            (
                [
                    s(&[&format!("{MOTOR}.AsyncIOMotorDatabase.command")]),
                    with_prefix(
                        &format!("{MOTOR}.AsyncIOMotorCollection"),
                        &MONGO_COLLECTION_OPS
                            .iter()
                            .copied()
                            .filter(|m| *m != "aggregate")
                            .collect::<Vec<_>>(),
                    ),
                ]
                .concat(),
                async_safe(entry(&[DbQuery])),
            ),
            (
                with_prefix(&format!("{MOTOR}.AsyncIOMotorCollection"), MONGO_BULK),
                async_safe(entry(&[])),
            ),
        ]
    }

    fn data_libraries() -> Vec<(Vec<String>, EcosystemEntry)> {
        // polars downloads an `http(s)://` source itself; other URL schemes are not listed.
        let polars_url = ArgCase {
            prefix: s(&["http://", "https://"]),
            ..source_case("source", true)
        };
        let buffer = |arg: u8| ArgCase {
            arg: Some(arg),
            param: Some("f".into()),
            instance_of: s(&["io.BytesIO"]),
            effects: Some(Vec::new()),
            ..ArgCase::default()
        };
        vec![
            (
                with_prefix(
                    "polars",
                    &[
                        "read_csv",
                        "read_parquet",
                        "read_json",
                        "read_ndjson",
                        "read_ipc",
                        "read_avro",
                        "read_excel",
                        "read_ods",
                    ],
                ),
                with_cases(
                    vec![polars_url, source_case("source", false)],
                    entry(&[BlockingFs]),
                ),
            ),
            (
                with_prefix("polars", &["read_database", "read_database_uri"]),
                entry(&[BlockingDb, DbQuery]),
            ),
            (
                s(&["polars.DataFrame.write_database"]),
                entry(&[BlockingDb]),
            ),
            (
                s(&["torch.load"]),
                with_cases(vec![buffer(0)], entry(&[BlockingFs])),
            ),
            (
                s(&["torch.save"]),
                with_cases(vec![buffer(1)], entry(&[BlockingFs])),
            ),
            (
                // Local paths only.
                with_prefix("pandas", &["read_hdf", "read_spss"]),
                entry(&[BlockingFs]),
            ),
            (
                s(&["pandas.read_sas"]),
                with_cases(
                    vec![source_case("filepath_or_buffer", false)],
                    entry(&[BlockingFs]),
                ),
            ),
            (
                [
                    s(&["pandas.concat"]),
                    with_prefix(
                        "numpy",
                        &[
                            "vstack",
                            "hstack",
                            "dstack",
                            "column_stack",
                            "concatenate",
                            "append",
                            "insert",
                        ],
                    ),
                    with_prefix(
                        "torch",
                        &[
                            "cat",
                            "concat",
                            "concatenate",
                            "vstack",
                            "hstack",
                            "dstack",
                            "column_stack",
                            "row_stack",
                        ],
                    ),
                ]
                .concat(),
                entry(&[QuadraticRealloc]),
            ),
        ]
    }

    fn locks_and_transactions() -> Vec<(Vec<String>, EcosystemEntry)> {
        vec![
            (
                // `sqlalchemy.engine.Engine.begin` (`sync_databases`) also connects and types its result.
                [LOCK_CLASSES, TRANSACTION_STARTERS]
                    .concat()
                    .iter()
                    .filter(|n| {
                        **n != "sqlalchemy.engine.Engine.begin"
                            && !SAVEPOINTS.contains(*n)
                            && !NON_REENTRANT_LOCKS.contains(*n)
                    })
                    .map(|n| (*n).to_owned())
                    .collect(),
                EcosystemEntry {
                    sync_lock_or_tx: true,
                    ..entry(&[])
                },
            ),
            (
                s(&["threading.Lock"]),
                EcosystemEntry {
                    sync_lock_or_tx: true,
                    non_reentrant_lock: true,
                    ..entry(&[])
                },
            ),
            (
                s(&["asyncio.Lock", "asyncio.Condition", "multiprocessing.Lock"]),
                EcosystemEntry {
                    non_reentrant_lock: true,
                    ..entry(&[])
                },
            ),
            (
                s(SAVEPOINTS),
                EcosystemEntry {
                    sync_lock_or_tx: true,
                    ..entry(&[BlockingDb])
                },
            ),
        ]
    }

    fn pandas_url_readers() -> Vec<(Vec<String>, EcosystemEntry)> {
        PANDAS_URL_READERS
            .iter()
            .map(|&(reader, param)| {
                (
                    vec![format!("pandas.{reader}")],
                    with_cases(
                        vec![source_case(param, true), source_case(param, false)],
                        entry(&[BlockingFs]),
                    ),
                )
            })
            .collect()
    }

    /// Calls returning an iterator that does the I/O while it is iterated.
    fn lazy_iterators() -> Vec<(Vec<String>, EcosystemEntry)> {
        const CURSOR_BUILDERS: &[&str] = &[
            "sort",
            "limit",
            "skip",
            "batch_size",
            "hint",
            "max_time_ms",
            "max_await_time_ms",
            "where",
            "collation",
            "comment",
            "allow_disk_use",
            "min",
            "max",
            "add_option",
            "remove_option",
            "rewind",
            "clone",
        ];
        const CURSOR_RUNS: &[&str] = &["to_list", "distinct", "explain"];
        let cursor = "pymongo.cursor.Cursor";
        let async_cursor = "pymongo.asynchronous.cursor.AsyncCursor";
        let motor_cursor = format!("{MOTOR}.AsyncIOMotorCursor");
        let motor_command_cursor = format!("{MOTOR}.AsyncIOMotorLatentCommandCursor");
        vec![
            (s(&["os.walk"]), returning("os.walk()", entry(&[]))),
            (
                [
                    s(&["pymongo.collection.Collection.find"]),
                    with_prefix(cursor, CURSOR_BUILDERS),
                ]
                .concat(),
                returning(cursor, entry(&[])),
            ),
            (
                [
                    s(&[&format!("{cursor}.__iter__")]),
                    with_prefix(cursor, CURSOR_RUNS),
                ]
                .concat(),
                entry(&[BlockingDb, DbQuery]),
            ),
            (
                [
                    s(&["pymongo.asynchronous.collection.AsyncCollection.find"]),
                    with_prefix(async_cursor, CURSOR_BUILDERS),
                ]
                .concat(),
                async_safe(returning(async_cursor, entry(&[]))),
            ),
            (
                [
                    s(&[&format!("{async_cursor}.__aiter__")]),
                    with_prefix(async_cursor, CURSOR_RUNS),
                ]
                .concat(),
                async_safe(entry(&[DbQuery])),
            ),
            // Motor's `aggregate()` returns a lazy command cursor, unlike pymongo's async one.
            (
                [
                    s(&[&format!("{MOTOR}.AsyncIOMotorCollection.find")]),
                    with_prefix(&motor_cursor, CURSOR_BUILDERS),
                ]
                .concat(),
                async_safe(returning(&motor_cursor, entry(&[]))),
            ),
            (
                s(&[&format!("{MOTOR}.AsyncIOMotorCollection.aggregate")]),
                async_safe(returning(&motor_command_cursor, entry(&[]))),
            ),
            (
                [
                    s(&[&format!("{motor_cursor}.__aiter__")]),
                    with_prefix(&motor_cursor, CURSOR_RUNS),
                    with_prefix(&motor_command_cursor, &["__aiter__", "to_list"]),
                ]
                .concat(),
                async_safe(entry(&[DbQuery])),
            ),
        ]
    }

    /// Waits on the calling thread, with the arguments that make them return at once.
    fn synchronous_waits() -> Vec<(Vec<String>, EcosystemEntry)> {
        let zero = |arg, param: &str| ArgCase {
            arg: Some(arg),
            param: Some(param.into()),
            equals: Some(ArgLiteral::Int(0)),
            effects: Some(Vec::new()),
            ..ArgCase::default()
        };
        let off = |arg, param: &str| ArgCase {
            equals: Some(ArgLiteral::Bool(false)),
            ..zero(arg, param)
        };
        let waits = |cases: Vec<ArgCase>| with_cases(cases, entry(&[BlockingSync]));
        let queues = ["queue.Queue", "queue.LifoQueue", "queue.PriorityQueue"];
        let executors = [
            "concurrent.futures.ThreadPoolExecutor",
            "concurrent.futures.ProcessPoolExecutor",
        ];
        vec![
            (
                LOCK_CLASSES
                    .iter()
                    .map(|c| format!("{c}.acquire"))
                    .collect(),
                waits(vec![off(0, "blocking"), zero(1, "timeout")]),
            ),
            (
                s(&[
                    "threading.Condition.wait",
                    "threading.Event.wait",
                    "threading.Barrier.wait",
                    "threading.Thread.join",
                ]),
                waits(vec![zero(0, "timeout")]),
            ),
            (
                s(&[
                    "concurrent.futures.Future.result",
                    "concurrent.futures.Future.exception",
                ]),
                EcosystemEntry {
                    waits_until_done: true,
                    ..waits(vec![zero(0, "timeout")])
                },
            ),
            (
                s(&["asyncio.wrap_future"]),
                EcosystemEntry {
                    done_when_awaited: Some(0),
                    ..returning("asyncio.Future", entry(&[]))
                },
            ),
            (
                s(&["threading.Condition.wait_for", "concurrent.futures.wait"]),
                waits(vec![zero(1, "timeout")]),
            ),
            (
                [
                    queues.iter().map(|q| format!("{q}.get")).collect(),
                    s(&["queue.SimpleQueue.get"]),
                ]
                .concat(),
                waits(vec![off(0, "block"), zero(1, "timeout")]),
            ),
            (
                queues.iter().map(|q| (*q).to_owned()).collect(),
                EcosystemEntry {
                    bounded_by: Some(ArgSlot {
                        arg: Some(0),
                        param: Some("maxsize".into()),
                    }),
                    ..entry(&[])
                },
            ),
            (
                queues.iter().map(|q| format!("{q}.put")).collect(),
                EcosystemEntry {
                    waits_while_full: true,
                    ..waits(vec![off(1, "block"), zero(2, "timeout")])
                },
            ),
            (
                queues.iter().map(|q| format!("{q}.join")).collect(),
                entry(&[BlockingSync]),
            ),
            (
                executors.iter().map(|e| format!("{e}.shutdown")).collect(),
                waits(vec![off(0, "wait")]),
            ),
            (
                executors.iter().map(|e| format!("{e}.submit")).collect(),
                returning("concurrent.futures.Future", entry(&[])),
            ),
        ]
    }

    /// `EFF106` synchronous locks (instances held by `with`).
    const LOCK_CLASSES: &[&str] = &[
        "threading.Lock",
        "threading.RLock",
        "threading.Semaphore",
        "threading.BoundedSemaphore",
        "threading.Condition",
    ];
    /// `EFF109` locks the holder cannot take again (instances held by `with` / `async with` or
    /// a blocking `acquire()`).
    const NON_REENTRANT_LOCKS: &[&str] = &[
        "threading.Lock",
        "asyncio.Lock",
        "asyncio.Condition",
        "multiprocessing.Lock",
    ];
    /// Transaction starters that send `SAVEPOINT` (or flush the session) at the call.
    const SAVEPOINTS: &[&str] = &[
        "sqlalchemy.orm.Session.begin_nested",
        "sqlalchemy.engine.Connection.begin_nested",
        "sqlmodel.Session.begin_nested",
    ];
    /// `EFF106` DB transaction starters (call results held by `with`).
    const TRANSACTION_STARTERS: &[&str] = &[
        "sqlalchemy.orm.Session.begin",
        "sqlalchemy.orm.Session.begin_nested",
        "sqlalchemy.orm.sessionmaker.begin",
        "sqlalchemy.engine.Connection.begin",
        "sqlalchemy.engine.Connection.begin_nested",
        "sqlalchemy.engine.Engine.begin",
        "sqlmodel.Session.begin",
        "sqlmodel.Session.begin_nested",
        "django.db.transaction.atomic",
        "psycopg.Connection.transaction",
    ];

    #[test]
    fn layer2_flags_exactly_the_synchronous_locks_and_transaction_starters() {
        let kb = EffectiveEcosystemKb::load_builtin(&[]).unwrap();
        let flagged: BTreeSet<&str> = kb
            .entries
            .values()
            .filter(|e| e.sync_lock_or_tx)
            .map(|e| e.symbol.as_str())
            .collect();
        let expected: BTreeSet<&str> = [LOCK_CLASSES, TRANSACTION_STARTERS]
            .concat()
            .into_iter()
            .collect();
        assert_eq!(flagged, expected);
        // Async locks never hold the event loop's thread: none is flagged (async transaction
        // starters such as `AsyncEngine.begin` only type their results).
        for symbol in [
            "asyncio.Lock",
            "asyncio.Semaphore",
            "asyncio.BoundedSemaphore",
            "asyncio.Condition",
            "anyio.Lock",
            "anyio.Semaphore",
            "sqlalchemy.ext.asyncio.AsyncSession.begin",
            "psycopg.AsyncConnection.transaction",
        ] {
            assert!(
                kb.lookup(symbol).is_none_or(|e| !e.sync_lock_or_tx),
                "{symbol}"
            );
        }
    }

    #[test]
    fn layer2_flags_exactly_the_non_reentrant_locks() {
        let kb = EffectiveEcosystemKb::load_builtin(&[]).unwrap();
        let flagged: BTreeSet<&str> = kb
            .entries
            .values()
            .filter(|e| e.non_reentrant_lock)
            .map(|e| e.symbol.as_str())
            .collect();
        assert_eq!(flagged, NON_REENTRANT_LOCKS.iter().copied().collect());
        // A reentrant lock, a condition over one by default, and semaphores, whose count the
        // class does not say, are not.
        for symbol in [
            "threading.RLock",
            "threading.Condition",
            "threading.Semaphore",
            "asyncio.Semaphore",
        ] {
            assert!(
                kb.lookup(symbol).is_none_or(|e| !e.non_reentrant_lock),
                "{symbol}"
            );
        }
    }

    const REQUIRED_OPAQUE: &[&str] = &[
        "asyncio",
        "socket",
        "ssl",
        "subprocess",
        "time",
        "io",
        "os",
        "pathlib",
        "shutil",
        "sqlite3",
        "urllib",
        "http",
        "logging",
        "copy",
        "functools",
        "itertools",
        "collections",
        "threading",
        "multiprocessing",
        "concurrent",
        "dataclasses",
        "queue",
        "requests",
        "httpx",
        "aiohttp",
        "urllib3",
        "sqlalchemy",
        "sqlmodel",
        "django",
        "psycopg",
        "psycopg_pool",
        "psycopg2",
        "pymysql",
        "redis",
        "pymongo",
        "boto3",
        "botocore",
        "grpc",
        "pandas",
        "numpy",
        "pydantic",
        "anyio",
        "trio",
        "starlette",
        "fastapi",
        "asgiref",
        "asyncer",
        "channels",
    ];

    /// The classes of the connections `sqlite3.connect` opens on an in-memory database and on
    /// one a parameter names.
    const SQLITE3_MEMORY: &str = "sqlite3.connect(\":memory:\")";
    const SQLITE3_FILE: &str = "sqlite3.connect(file)";

    /// The database calls that wait only in a state the call does not show.
    const CONDITIONAL_EMITTERS: &[(Emits, &[&str])] = &[
        (
            Emits::OnIdentityMapMiss,
            &[
                "sqlalchemy.orm.Session.get",
                "sqlalchemy.orm.Session.get_one",
                "sqlalchemy.orm.Query.get",
                "sqlalchemy.ext.asyncio.AsyncSession.get",
                "sqlalchemy.ext.asyncio.AsyncSession.get_one",
                "sqlmodel.Session.get",
                "sqlmodel.Session.get_one",
                "sqlmodel.ext.asyncio.session.AsyncSession.get",
                "sqlmodel.ext.asyncio.session.AsyncSession.get_one",
            ],
        ),
        (
            Emits::IfPending,
            &[
                "sqlalchemy.orm.Session.flush",
                "sqlalchemy.orm.Session.commit",
                "sqlalchemy.ext.asyncio.AsyncSession.flush",
                "sqlalchemy.ext.asyncio.AsyncSession.commit",
                "sqlmodel.Session.flush",
                "sqlmodel.Session.commit",
                "sqlmodel.ext.asyncio.session.AsyncSession.flush",
                "sqlmodel.ext.asyncio.session.AsyncSession.commit",
                "sqlalchemy.engine.Connection.commit",
                "sqlalchemy.engine.Connection.rollback",
                "sqlite3.Connection.commit",
                "sqlite3.Connection.rollback",
                "psycopg.Connection.commit",
                "psycopg.Connection.rollback",
                "psycopg2.extensions.connection.commit",
                "psycopg2.extensions.connection.rollback",
                "sqlite3.connect(file).commit",
                "sqlite3.connect(file).rollback",
            ],
        ),
        (
            Emits::IfOnDisk,
            &[
                "sqlite3.connect",
                "sqlite3.Connection.execute",
                "sqlite3.Connection.executemany",
                "sqlite3.Connection.executescript",
                "sqlite3.Cursor.execute",
                "sqlite3.Cursor.executemany",
                "sqlite3.Cursor.executescript",
                "sqlite3.Cursor.fetchone",
                "sqlite3.Cursor.fetchmany",
                "sqlite3.Cursor.fetchall",
            ],
        ),
        (
            Emits::UnlessCached,
            &[
                "django.db.models.query.QuerySet.first",
                "django.db.models.query.QuerySet.last",
                "django.db.models.query.QuerySet.exists",
                "django.db.models.query.QuerySet.count",
                "django.db.models.query.QuerySet.afirst",
                "django.db.models.query.QuerySet.alast",
                "django.db.models.query.QuerySet.aexists",
                "django.db.models.query.QuerySet.acount",
            ],
        ),
        (
            Emits::UnlessConnected,
            &[
                "sqlalchemy.engine.Engine.connect",
                "sqlalchemy.engine.Engine.begin",
                "psycopg_pool.ConnectionPool.connection",
                "django.db.connection.cursor",
            ],
        ),
    ];

    /// One row of [`ORDERED_OUTPUTS`]: `(arg, param, sorted_by, sorted_by_default, symbols)`.
    type OrderedOutputRow = (
        Option<u8>,
        Option<&'static str>,
        Option<&'static str>,
        bool,
        &'static [&'static str],
    );

    /// `EFF112`: what each call writes in the order of its elements.
    const ORDERED_OUTPUTS: &[OrderedOutputRow] = &[
        (None, None, None, false, &["builtins.print"]),
        (
            Some(0),
            Some("data"),
            None,
            false,
            &["pathlib.Path.write_text", "pathlib.Path.write_bytes"],
        ),
        (
            Some(0),
            Some("obj"),
            Some("sort_keys"),
            false,
            &["json.dumps", "json.dump"],
        ),
        (
            Some(0),
            Some("object"),
            Some("sort_dicts"),
            true,
            &["pprint.pprint", "pprint.pformat"],
        ),
        (
            Some(0),
            Some("object"),
            Some("sort_dicts"),
            false,
            &["pprint.pp"],
        ),
        (
            Some(0),
            Some("data"),
            Some("sort_keys"),
            true,
            &["yaml.dump", "yaml.safe_dump"],
        ),
    ];

    /// The calls whose results' items the environment decides the number of.
    const RESULT_ITEMS: &[(ResultItems, &[&str])] = &[
        (
            ResultItems::Rows,
            &[
                "sqlite3.Connection.execute",
                "sqlite3.Cursor.execute",
                "sqlite3.Cursor.fetchall",
                "sqlite3.Cursor.fetchmany",
                "sqlalchemy.orm.Session.execute",
                "sqlalchemy.orm.Session.scalars",
                "sqlalchemy.orm.Query.all",
                "sqlalchemy.engine.Connection.execute",
                "sqlalchemy.engine.Connection.scalars",
                "sqlalchemy.ext.asyncio.AsyncSession.execute",
                "sqlalchemy.ext.asyncio.AsyncSession.scalars",
                "sqlmodel.Session.exec",
                "sqlmodel.Session.execute",
                "sqlmodel.Session.scalars",
                "psycopg.Connection.execute",
                "psycopg.Cursor.execute",
                "asyncpg.Connection.fetch",
                "asyncpg.connection.Connection.fetch",
                "asyncpg.Pool.fetch",
                "asyncpg.pool.Pool.fetch",
                "pymongo.collection.Collection.find",
                "pymongo.collection.Collection.aggregate",
                "sqlite3.connect(\":memory:\").execute",
                "sqlite3.connect(\":memory:\").executemany",
                "sqlite3.connect(\":memory:\").executescript",
                "sqlite3.connect(\":memory:\").cursor().execute",
                "sqlite3.connect(\":memory:\").cursor().executemany",
                "sqlite3.connect(\":memory:\").cursor().executescript",
                "sqlite3.connect(file).execute",
                "sqlite3.connect(file).executemany",
                "sqlite3.connect(file).executescript",
                "sqlite3.connect(file).cursor().execute",
                "sqlite3.connect(file).cursor().executemany",
                "sqlite3.connect(file).cursor().executescript",
                "sqlite3.connect(file).cursor().fetchmany",
                "sqlite3.connect(file).cursor().fetchall",
            ],
        ),
        (
            ResultItems::Lines,
            &["builtins.open", "io.open", "pathlib.Path.open"],
        ),
        (ResultItems::Text, &["pathlib.Path.read_text"]),
        (
            ResultItems::Entries,
            &["os.listdir", "os.scandir", "os.walk"],
        ),
    ];

    /// The classes and factories whose context manager's exit returns a false value on every
    /// path.
    const EXITS_NEVER_SUPPRESS: &[&str] = &[
        "builtins.open",
        "threading.Lock",
        "threading.RLock",
        "threading.Semaphore",
        "threading.BoundedSemaphore",
        "threading.Condition",
        "urllib.request.urlopen",
        "sqlite3.connect",
        "requests.Session",
        "httpx.Client",
        "httpx.AsyncClient",
        "aiohttp.ClientSession",
        "urllib3.PoolManager",
        "urllib3.ProxyManager",
        "urllib3.HTTPConnectionPool",
        "urllib3.HTTPSConnectionPool",
        "psycopg_pool.ConnectionPool",
        "psycopg_pool.AsyncConnectionPool",
        "redis.Redis",
        "redis.StrictRedis",
        "redis.asyncio.Redis",
        "pymongo.MongoClient",
        "pymongo.AsyncMongoClient",
        "psycopg.connect",
        "psycopg2.connect",
        "pymysql.connect",
        "sqlalchemy.engine.Engine.connect",
        "sqlalchemy.engine.Engine.begin",
        "sqlalchemy.orm.Session.begin",
        "psycopg_pool.ConnectionPool.connection",
    ];

    #[test]
    fn layer2_load_builtin_contains_exactly_the_required_entries() {
        let kb = EffectiveEcosystemKb::load_builtin(&[]).unwrap();
        let mut required = BTreeSet::new();
        for (symbols, expected) in layer2_required() {
            for symbol in symbols {
                let emits = CONDITIONAL_EMITTERS
                    .iter()
                    .find(|(_, symbols)| symbols.contains(&symbol.as_str()))
                    .map_or(Emits::Always, |(emits, _)| *emits);
                let exit_suppresses = if EXITS_NEVER_SUPPRESS.contains(&symbol.as_str()) {
                    Some(false)
                } else {
                    expected.exit_suppresses
                };
                let items = RESULT_ITEMS
                    .iter()
                    .find(|(_, symbols)| symbols.contains(&symbol.as_str()))
                    .map(|(items, _)| *items);
                // The methods of the classes of what `sqlite3.connect` returns are the library's.
                let api = [SQLITE3_MEMORY, SQLITE3_FILE].iter().find_map(|class| {
                    let method = symbol.strip_prefix(&format!("{class}."))?;
                    Some(match method.strip_prefix("cursor().") {
                        Some(m) => format!("sqlite3.Cursor.{m}"),
                        None => format!("sqlite3.Connection.{method}"),
                    })
                });
                let writes_in_order = ORDERED_OUTPUTS
                    .iter()
                    .find(|(.., symbols)| symbols.contains(&symbol.as_str()))
                    .map(
                        |(arg, param, sorted_by, sorted_by_default, _)| OrderedOutput {
                            arg: *arg,
                            param: param.map(Into::into),
                            sorted_by: sorted_by.map(Into::into),
                            sorted_by_default: *sorted_by_default,
                        },
                    );
                let expected = EcosystemEntry {
                    symbol: symbol.clone(),
                    emits,
                    exit_suppresses,
                    items,
                    api,
                    writes_in_order,
                    ..expected.clone()
                };
                assert_eq!(kb.lookup(&symbol), Some(&expected), "{symbol}");
                required.insert(symbol);
            }
        }
        // Every entry is pinned: the catalog is the only source of library effects.
        let unpinned: Vec<&String> = kb
            .entries
            .keys()
            .filter(|k| !required.contains(*k))
            .collect();
        assert!(unpinned.is_empty(), "{unpinned:?}");
        assert_eq!(kb.digest.len(), 64);
    }

    /// No async API can fire `EFF101`: async APIs are often called without a direct `await`
    /// (`async with`, `asyncio.gather(...)`, `asyncio.wait_for(...)`), where the `is_awaited`
    /// filter does not apply, so an `async_safe` entry never carries a blocking effect and every
    /// entry of an async namespace is `async_safe`.
    #[test]
    fn layer2_async_apis_carry_no_blocking_effect() {
        const ASYNC_NAMESPACES: &[&str] = &[
            "httpx.AsyncClient",
            "aiohttp.",
            "redis.asyncio.",
            "sqlalchemy.ext.asyncio.",
            "sqlmodel.ext.asyncio.",
            "psycopg.Async",
            "psycopg_pool.Async",
            "pymongo.Async",
            "pymongo.asynchronous.",
            "asyncpg.",
            "websockets.",
            "motor.motor_asyncio.",
            "openai.Async",
            "anthropic.Async",
        ];
        let kb = EffectiveEcosystemKb::load_builtin(&[]).unwrap();
        let mut seen = BTreeSet::new();
        for (symbol, e) in &kb.entries {
            if e.async_safe {
                assert!(
                    !e.effects.iter().any(|x| BLOCKING.contains(x)),
                    "{symbol}: {:?}",
                    e.effects
                );
            }
            if let Some(ns) = ASYNC_NAMESPACES.iter().find(|ns| symbol.starts_with(**ns)) {
                assert!(e.async_safe, "{symbol}");
                seen.insert(*ns);
            }
        }
        assert_eq!(seen.len(), ASYNC_NAMESPACES.len(), "{seen:?}");
    }

    /// A `return_type` naming a class without catalogued methods would leave the factory's
    /// result unresolved without any sign (a typo, or a class spelled another way), unless the
    /// class itself is the fact ([`FUTURE_CLASSES`]) or a builtin type, whose methods the
    /// receiver typing knows. A call of a capitalized name is typed as
    /// an instance of that name (`TypeCtx::call_result`), so a `return_type` there would be
    /// ignored: aliases such as `redis.StrictRedis` cannot be declared this way.
    #[test]
    fn layer2_return_types_name_catalogued_classes() {
        let kb = EffectiveEcosystemKb::load_builtin(&[]).unwrap();
        for (symbol, e) in &kb.entries {
            if let Some(class) = e.return_type_in(None) {
                let methods = format!("{class}.");
                assert!(
                    kb.entries.keys().any(|k| k.starts_with(&methods))
                        || FUTURE_CLASSES.contains(&class)
                        || class.starts_with("builtins."),
                    "{symbol} -> {class}"
                );
                let name = symbol.rsplit('.').next().unwrap_or(symbol);
                assert!(!name.starts_with(char::is_uppercase), "{symbol}");
            }
        }
    }

    /// Every case names its argument, tests it, and changes something; a case's return type
    /// names a catalogued class, as an entry's does.
    #[test]
    fn layer2_cases_test_a_named_argument() {
        let kb = EffectiveEcosystemKb::load_builtin(&[]).unwrap();
        for (symbol, e) in &kb.entries {
            for case in &e.cases {
                assert!(case.arg.is_some() || case.param.is_some(), "{symbol}");
                assert!(
                    case.equals.is_some()
                        || !case.prefix.is_empty()
                        || case.nonempty
                        || !case.instance_of.is_empty()
                        || !case.flag.is_empty()
                        || !case.one_of.is_empty()
                        || case.numeric_address
                        || !case.contains.is_empty()
                        || case.names_file,
                    "{symbol}"
                );
                assert!(
                    case.effects.is_some() || case.return_type.is_some(),
                    "{symbol}"
                );
                if let Some(class) = &case.return_type {
                    let methods = format!("{class}.");
                    assert!(
                        kb.entries.keys().any(|k| k.starts_with(&methods)),
                        "{symbol} -> {class}"
                    );
                }
            }
        }
    }

    /// The thread waits are told apart from input and output by their effect; the database
    /// calls that wait only in a state the call does not show say so, and so do the context
    /// managers whose exit never suppresses an exception.
    #[test]
    fn layer2_wait_kinds_emitters_and_exits() {
        let kb = EffectiveEcosystemKb::load_builtin(&[]).unwrap();
        assert_eq!(BlockingSync.wait_kind(), Some(WaitKind::Thread));
        for e in [
            BlockingNet,
            BlockingDb,
            BlockingSubprocess,
            Sleep,
            BlockingFs,
        ] {
            assert_eq!(e.wait_kind(), Some(WaitKind::Io), "{e:?}");
        }
        for e in [UnpooledClientAlloc, DbQuery, QuadraticRealloc] {
            assert_eq!(e.wait_kind(), None, "{e:?}");
        }
        assert!(
            ExecEffectKind::BLOCKING
                .iter()
                .all(|e| e.wait_kind().is_some())
        );
        let emits = |symbol: &str| kb.lookup(symbol).unwrap().emits;
        assert_eq!(emits("sqlalchemy.orm.Session.execute"), Emits::Always);
        assert_eq!(
            emits("sqlalchemy.orm.Session.get"),
            Emits::OnIdentityMapMiss
        );
        assert_eq!(emits("sqlalchemy.orm.Session.flush"), Emits::IfPending);
        assert_eq!(emits("sqlite3.Connection.commit"), Emits::IfPending);
        assert_eq!(
            emits("pymysql.connections.Connection.commit"),
            Emits::Always
        );
        assert_eq!(
            emits("django.db.models.query.QuerySet.count"),
            Emits::UnlessCached
        );
        assert_eq!(emits("django.db.models.query.QuerySet.get"), Emits::Always);
        assert_eq!(
            emits("sqlalchemy.engine.Engine.connect"),
            Emits::UnlessConnected
        );
        // Only database calls carry a condition on their database effects.
        for (symbol, e) in &kb.entries {
            if e.emits != Emits::Always {
                assert!(
                    e.effects.iter().any(|k| matches!(k, BlockingDb | DbQuery)),
                    "{symbol}"
                );
            }
        }
        let exits = |symbol: &str| kb.lookup(symbol).unwrap().exit_suppresses;
        assert_eq!(exits("contextlib.suppress"), Some(true));
        assert_eq!(exits("builtins.open"), Some(false));
        assert_eq!(exits("threading.Lock"), Some(false));
        assert_eq!(exits("sqlite3.connect"), Some(false));
        assert_eq!(exits("requests.Session"), Some(false));
        assert_eq!(exits("sqlite3.Connection.execute"), None);
    }

    /// A wait the arguments end at once: no sleep, a polled child, a numeric or local host, a
    /// `data:` URL.
    #[test]
    fn layer2_waits_that_arguments_end_at_once() {
        let kb = EffectiveEcosystemKb::load_builtin(&[]).unwrap();
        let text = |t: &str| ArgFact {
            literal: Some(LiteralValue::Str(t.into())),
            ..ArgFact::default()
        };
        let zero = ArgFact {
            literal: Some(LiteralValue::Int(0)),
            ..ArgFact::default()
        };
        for (symbol, fact, waits) in [
            ("time.sleep", zero.clone(), false),
            ("time.sleep", ArgFact::default(), true),
            ("subprocess.Popen.wait", zero.clone(), false),
            ("socket.getaddrinfo", text("127.0.0.1"), false),
            ("socket.getaddrinfo", text("localhost"), false),
            ("socket.getaddrinfo", text("example.com"), true),
            ("socket.gethostbyname", text("::1"), false),
            ("socket.gethostbyname", text("example.com"), true),
            ("urllib.request.urlopen", text("data:,hello"), false),
            ("urllib.request.urlopen", text("https://example.com"), true),
        ] {
            let e = kb.lookup(symbol).unwrap();
            let case = e.case_for(|i, _| (i == Some(0)).then(|| fact.clone()));
            assert_eq!(!e.effects_in(case).is_empty(), waits, "{symbol} {fact:?}");
        }
    }

    #[test]
    fn layer2_does_not_register_popen_spawn() {
        let kb = EffectiveEcosystemKb::load_builtin(&[]).unwrap();
        assert_eq!(kb.lookup("subprocess.Popen"), None);
        assert!(kb.lookup("subprocess.Popen.wait").is_some());
    }

    /// Calls that do no I/O, or whose effect depends on what the catalog cannot see, are left
    /// out: client-side cursors already hold their rows, boto3 signs URLs locally,
    /// `to_csv()` without a path returns a string, `read_html` parses text as
    /// often as it fetches, and `sessionmaker()` builds `AsyncSession`s with `class_=AsyncSession`.
    #[test]
    fn layer2_leaves_local_and_undecidable_calls_unregistered() {
        let kb = EffectiveEcosystemKb::load_builtin(&[]).unwrap();
        for symbol in [
            "psycopg.Cursor.fetchone",
            "psycopg.Cursor.fetchall",
            "psycopg.AsyncCursor.fetchall",
            "psycopg2.extensions.cursor.fetchall",
            "pymysql.cursors.Cursor.fetchall",
            "boto3.client.generate_presigned_url",
            "boto3.client.get_paginator",
            "httpx.Client.build_request",
            "httpx.AsyncClient.build_request",
            "pandas.DataFrame.to_csv",
            // A session only holds configuration; its `client()` / `resource()` allocate.
            "boto3.Session",
            "pandas.read_html",
            "sqlalchemy.orm.sessionmaker.__call__",
        ] {
            assert_eq!(kb.lookup(symbol), None, "{symbol}");
        }
    }

    #[test]
    fn layer2_opaque_packages_cover_stdlib_and_every_registered_package() {
        let kb = EffectiveEcosystemKb::load_builtin(&[]).unwrap();
        for package in REQUIRED_OPAQUE {
            assert!(kb.is_opaque_to_layer3(package), "{package}");
        }
        for (symbol, entry) in &kb.entries {
            assert!(entry.opaque_to_layer3, "{symbol}");
            assert!(
                kb.is_opaque_to_layer3(top_level_package(symbol)),
                "{symbol}: package not listed in opaque_packages"
            );
        }
        assert!(!kb.is_opaque_to_layer3("mypkg"));
        // Only exact top-level package names match.
        assert!(!kb.is_opaque_to_layer3("asyncio.subprocess"));
        assert!(!kb.is_opaque_to_layer3("request"));
    }

    #[test]
    fn layer2_rejects_unknown_and_layer1_fields() {
        for field in [
            "param_effects = [\"read_only\"]",
            "return_exact_type = \"int\"",
            "verified_max_python = { major = 3, minor = 15 }",
        ] {
            let text = format!("[[entries]]\nsymbol = \"time.sleep\"\n{field}\n");
            let err = parse_ecosystem_file("test.toml", &text).unwrap_err();
            assert!(err.to_string().contains("unknown field"), "{field}: {err}");
        }
        let text = "[[entries]]\nsymbol = \"time.sleep\"\neffects = [\"sleeep\"]\n";
        assert!(parse_ecosystem_file("test.toml", text).is_err());
    }

    #[test]
    fn builtin_files_have_no_duplicate_symbols() {
        let mut seen = BTreeSet::new();
        for &(file, text) in SOUNDNESS_KB_FILES {
            for e in parse_soundness_file(file, text).unwrap() {
                assert!(
                    seen.insert(e.symbol.clone()),
                    "{file}: duplicate {}",
                    e.symbol
                );
            }
        }
        let mut seen = BTreeSet::new();
        for &(file, text) in ECOSYSTEM_KB_FILES {
            for e in parse_ecosystem_file(file, text).unwrap().entries {
                assert!(
                    seen.insert(e.symbol.clone()),
                    "{file}: duplicate {}",
                    e.symbol
                );
            }
        }
    }

    /// A case holds when the argument passes every test it sets; the first case that holds
    /// decides the effects and the return type, and an unknown argument selects none.
    #[test]
    fn argument_cases_select_by_literal_prefix_class_and_flag() {
        let lit = |v: LiteralValue| ArgFact {
            literal: Some(v),
            ..ArgFact::default()
        };
        let inst = |c: &str| ArgFact {
            class: Some(c.into()),
            ..ArgFact::default()
        };
        let off = ArgCase {
            arg: Some(0),
            param: Some("timeout".into()),
            equals: Some(ArgLiteral::Int(0)),
            effects: Some(Vec::new()),
            ..ArgCase::default()
        };
        let no_wait = ArgCase {
            param: Some("block".into()),
            equals: Some(ArgLiteral::Bool(false)),
            effects: Some(Vec::new()),
            ..ArgCase::default()
        };
        let url = ArgCase {
            arg: Some(0),
            prefix: s(&["https://"]),
            effects: Some(vec![BlockingNet]),
            ..ArgCase::default()
        };
        let named = ArgCase {
            arg: Some(0),
            nonempty: true,
            return_type: Some("m.Named".into()),
            ..ArgCase::default()
        };
        let buffer = ArgCase {
            arg: Some(0),
            instance_of: s(&["io.StringIO"]),
            effects: Some(Vec::new()),
            ..ArgCase::default()
        };
        let numeric = ArgCase {
            arg: Some(5),
            param: Some("flags".into()),
            flag: s(&["socket.AI_NUMERICHOST"]),
            effects: Some(Vec::new()),
            ..ArgCase::default()
        };
        let flags = |names: &[&str]| ArgFact {
            flags: s(names),
            ..ArgFact::default()
        };
        let local = ArgCase {
            arg: Some(0),
            one_of: s(&["localhost"]),
            effects: Some(Vec::new()),
            ..ArgCase::default()
        };
        let address = ArgCase {
            arg: Some(0),
            numeric_address: true,
            effects: Some(Vec::new()),
            ..ArgCase::default()
        };
        let text = |t: &str| lit(LiteralValue::Str(t.into()));
        for (case, fact, holds) in [
            (&local, text("localhost"), true),
            (&local, text("localhost.example.com"), false),
            (&local, inst("builtins.str"), false),
            (&address, text("127.0.0.1"), true),
            (&address, text("::1"), true),
            (&address, text("2001:db8::8a2e:370:7334"), true),
            (&address, text("example.com"), false),
            (&address, text("10.0.0"), false),
            (&address, lit(LiteralValue::Int(0)), false),
            (&off, lit(LiteralValue::Int(0)), true),
            (&off, lit(LiteralValue::Float(0.0)), true),
            (&off, lit(LiteralValue::Int(1)), false),
            (&off, lit(LiteralValue::Bool(false)), false),
            (&off, inst("builtins.int"), false),
            (&no_wait, lit(LiteralValue::Bool(false)), true),
            (&no_wait, lit(LiteralValue::Int(0)), false),
            (&url, lit(LiteralValue::Str("https://x".into())), true),
            (&url, lit(LiteralValue::Str("rows.csv".into())), false),
            (&url, inst("builtins.str"), false),
            (&named, lit(LiteralValue::Str("export".into())), true),
            (&named, lit(LiteralValue::Str(String::new())), false),
            (&buffer, inst("io.StringIO"), true),
            (&buffer, inst("io.BytesIO"), false),
            (&numeric, flags(&["socket.AI_NUMERICHOST"]), true),
            (
                &numeric,
                flags(&["socket.AI_PASSIVE", "socket.AI_NUMERICHOST"]),
                true,
            ),
            (&numeric, flags(&["socket.AI_PASSIVE"]), false),
            (&numeric, lit(LiteralValue::Int(4)), false),
        ] {
            assert_eq!(case.holds(&fact), holds, "{case:?} {fact:?}");
        }
        let e = EcosystemEntry {
            cases: vec![off.clone(), url.clone()],
            ..returning("m.Default", entry(&[BlockingSync]))
        };
        let pick = |facts: &[(Option<u8>, Option<&str>, ArgFact)]| {
            e.case_for(|i, k| {
                facts
                    .iter()
                    .find(|(fi, fk, _)| (fi.is_some() && *fi == i) || (fk.is_some() && *fk == k))
                    .map(|(_, _, f)| f.clone())
            })
            .cloned()
        };
        assert_eq!(pick(&[]), None);
        assert_eq!(
            pick(&[(None, Some("timeout"), lit(LiteralValue::Int(0)))]),
            Some(off.clone())
        );
        let chosen = pick(&[(Some(0), None, lit(LiteralValue::Str("https://x".into())))]);
        assert_eq!(chosen, Some(url.clone()));
        assert_eq!(e.effects_in(chosen.as_ref()), &[BlockingNet]);
        assert_eq!(e.effects_in(None), &[BlockingSync]);
        assert_eq!(e.return_type_in(chosen.as_ref()), Some("m.Default"));
        let named_entry = EcosystemEntry {
            cases: vec![named.clone()],
            ..e.clone()
        };
        assert_eq!(named_entry.return_type_in(Some(&named)), Some("m.Named"));
        assert_eq!(named_entry.effects_in(Some(&named)), &[BlockingSync]);
    }

    #[test]
    fn layer2_user_overrides_set_only_the_keys_they_list() {
        let builtin = EffectiveEcosystemKb::load_builtin(&[]).unwrap();
        let patch = |symbol: &str| EcosystemPatch {
            symbol: symbol.into(),
            ..EcosystemPatch::default()
        };
        let overrides = [
            EcosystemPatch {
                excluded: Some(true),
                ..patch("time.sleep")
            },
            // The factory keeps the return type the override does not mention.
            EcosystemPatch {
                effects: Some(vec![BlockingDb]),
                ..patch("sqlite3.connect")
            },
            // A project-local symbol is added and is not opaque.
            EcosystemPatch {
                effects: Some(vec![BlockingNet]),
                ..patch("mypkg.http.fetch")
            },
            EcosystemPatch {
                return_type: Some("sqlite3.Connection".into()),
                ..patch("mypkg.db.open")
            },
            EcosystemPatch {
                sync_lock_or_tx: Some(true),
                ..patch("mypkg.db.open")
            },
            EcosystemPatch {
                non_reentrant_lock: Some(true),
                ..patch("mypkg.sync.Mutex")
            },
            EcosystemPatch {
                writes_in_order: Some(OrderedOutput {
                    sorted_by: Some("sort_keys".into()),
                    ..OrderedOutput::default()
                }),
                ..patch("json.dumps")
            },
            // An async-safe API keeps only its loop-sensitive effects.
            EcosystemPatch {
                async_safe: Some(true),
                effects: Some(vec![BlockingNet, DbQuery]),
                ..patch("vendor.fetch")
            },
            EcosystemPatch {
                async_safe: Some(true),
                ..patch("requests.get")
            },
            // Also the effects of its argument cases (an SRV lookup of a `mongodb+srv://` host).
            EcosystemPatch {
                async_safe: Some(true),
                ..patch("pymongo.MongoClient")
            },
            // An attribute row, which adds no entry.
            EcosystemPatch {
                attribute_type: Some("vendor.Chat".into()),
                ..patch("vendor.Client.chat")
            },
        ];
        let kb = EffectiveEcosystemKb::load_builtin(&overrides).unwrap();
        let got = |symbol: &str| kb.lookup(symbol).cloned().unwrap();
        assert_eq!(
            got("time.sleep"),
            EcosystemEntry {
                excluded: true,
                ..builtin.lookup("time.sleep").cloned().unwrap()
            }
        );
        assert_eq!(
            got("sqlite3.connect"),
            EcosystemEntry {
                effects: vec![BlockingDb],
                ..builtin.lookup("sqlite3.connect").cloned().unwrap()
            }
        );
        assert_eq!(
            got("mypkg.http.fetch"),
            EcosystemEntry {
                symbol: "mypkg.http.fetch".into(),
                effects: vec![BlockingNet],
                ..EcosystemEntry::default()
            }
        );
        // Later overrides of a symbol add to the earlier ones.
        assert_eq!(
            got("mypkg.db.open"),
            EcosystemEntry {
                symbol: "mypkg.db.open".into(),
                return_type: Some("sqlite3.Connection".into()),
                sync_lock_or_tx: true,
                ..EcosystemEntry::default()
            }
        );
        assert_eq!(
            got("mypkg.sync.Mutex"),
            EcosystemEntry {
                symbol: "mypkg.sync.Mutex".into(),
                non_reentrant_lock: true,
                ..EcosystemEntry::default()
            }
        );
        // A project may describe another sorting switch of a library call.
        assert_eq!(
            got("json.dumps").writes_in_order,
            Some(OrderedOutput {
                sorted_by: Some("sort_keys".into()),
                ..OrderedOutput::default()
            })
        );
        assert_eq!(
            got("vendor.fetch"),
            EcosystemEntry {
                symbol: "vendor.fetch".into(),
                effects: vec![DbQuery],
                async_safe: true,
                ..EcosystemEntry::default()
            }
        );
        assert_eq!(
            got("requests.get"),
            EcosystemEntry {
                symbol: "requests.get".into(),
                ..async_safe(entry(&[UnpooledClientAlloc]))
            }
        );
        let mongo = got("pymongo.MongoClient");
        assert!(mongo.async_safe);
        assert_eq!(
            mongo
                .cases
                .iter()
                .map(|c| c.effects.clone().unwrap_or_default())
                .collect::<Vec<_>>(),
            [vec![UnpooledClientAlloc], vec![UnpooledClientAlloc]]
        );
        assert_eq!(kb.entries.len(), builtin.entries.len() + 4);
        assert_eq!(
            kb.attribute_type("vendor.Client", "chat"),
            Some("vendor.Chat")
        );
        assert_eq!(kb.attributes.len(), builtin.attributes.len() + 1);
        assert_eq!(kb.declared("vendor.Client.chat"), None);
        // Only what the configuration names is a declaration of project code.
        assert!(kb.declared("mypkg.db.open").is_some());
        assert!(kb.declared("time.sleep").is_some());
        assert_eq!(kb.declared("sqlite3.Connection.execute"), None);
        assert_eq!(builtin.declared("time.sleep"), None);
        assert_eq!(kb.opaque_packages, builtin.opaque_packages);
        assert_ne!(kb.digest, builtin.digest);
    }

    /// Every attribute row names a class with catalogued members and is no entry (an attribute
    /// is not a method the class defines), and the releases of its package that the row was
    /// checked against are listed ([`Distribution`]).
    #[test]
    fn attribute_rows_name_catalogued_classes_in_verified_distributions() {
        let kb = EffectiveEcosystemKb::load_builtin(&[]).unwrap();
        let distributions = &kb.distributions;
        let operators = ["~=", "==", "!=", "<=", ">=", "<", ">"];
        for d in distributions {
            assert!(!d.name.is_empty() && !d.packages.is_empty(), "{d:?}");
            for clause in d.verified.split(',') {
                let version = operators
                    .iter()
                    .find_map(|op| clause.trim().strip_prefix(op))
                    .unwrap_or_else(|| panic!("{}: {clause}", d.name));
                assert!(version.starts_with(|c: char| c.is_ascii_digit()), "{d:?}");
            }
        }
        let under = |symbol: &str, package: &str| {
            symbol
                .strip_prefix(package)
                .is_some_and(|rest| rest.starts_with('.'))
        };
        for (symbol, class) in &kb.attributes {
            let members = format!("{class}.");
            assert!(
                kb.entries.keys().any(|k| k.starts_with(&members))
                    || kb.attributes.keys().any(|k| k.starts_with(&members)),
                "{symbol} -> {class}"
            );
            assert_eq!(kb.lookup(symbol), None, "{symbol}");
            assert!(
                distributions
                    .iter()
                    .any(|d| d.packages.iter().any(|p| under(symbol, p))),
                "{symbol}"
            );
        }
        // From the class an async-safe constructor builds, no attribute row or result reaches
        // a call that blocks: the asynchronous client's resources are classes of their own.
        let mut reached: Vec<String> = kb
            .entries
            .iter()
            .filter(|(s, e)| {
                e.async_safe
                    && s.rsplit('.')
                        .next()
                        .is_some_and(|n| n.starts_with(char::is_uppercase))
            })
            .map(|(s, _)| s.clone())
            .collect();
        let mut seen = BTreeSet::new();
        while let Some(class) = reached.pop() {
            if !seen.insert(class.clone()) {
                continue;
            }
            let members = format!("{class}.");
            for (symbol, e) in kb.entries.range(members.clone()..) {
                let Some(member) = symbol.strip_prefix(&members) else {
                    break;
                };
                assert!(
                    !e.effects.iter().any(|x| x.blocks()),
                    "{symbol}, reached from an async-safe constructor"
                );
                reached.extend(
                    e.return_type_in(None)
                        .filter(|_| !member.contains('.'))
                        .map(str::to_owned),
                );
            }
            for (symbol, t) in kb.attributes.range(members.clone()..) {
                if !symbol.starts_with(&members) {
                    break;
                }
                reached.push(t.clone());
            }
        }
        // `return_type = "Self"`: an instance of the receiver's class, never a class of that
        // name.
        let builder = EcosystemEntry {
            return_type: Some(SELF_RETURN_TYPE.into()),
            ..EcosystemEntry::default()
        };
        assert!(builder.returns_receiver_in(None));
        assert_eq!(builder.return_type_in(None), None);
        let factory = EcosystemEntry {
            return_type: Some("sqlite3.Connection".into()),
            ..EcosystemEntry::default()
        };
        assert!(!factory.returns_receiver_in(None));
        assert_eq!(factory.return_type_in(None), Some("sqlite3.Connection"));
    }

    #[test]
    fn invokes_and_cached_property_entries_are_described_and_part_of_the_digest() {
        let kb = EffectiveEcosystemKb::load_builtin(&[]).unwrap();
        let isinstance = kb.invoked("builtins.isinstance").expect("listed");
        assert!(!isinstance.lazy);
        assert_eq!(
            isinstance.values[0].methods,
            ["__class__", "__getattribute__"]
        );
        assert!(isinstance.values[1].names(Some(1), None));
        assert!(!isinstance.values[1].names(Some(2), None));
        assert!(
            kb.invoked("builtins.id")
                .is_some_and(|e| e.values.is_empty())
        );
        let map = kb.invoked("builtins.map").expect("listed");
        assert!(map.lazy && map.values[0].calls);
        // `rest`: every position from the argument on.
        assert!(map.values[1].names(Some(1), None) && map.values[1].names(Some(4), None));
        let sorted = kb.invoked("builtins.sorted").expect("listed");
        assert!(sorted.values[1].names(None, Some("key")));
        assert!(!sorted.values[1].names(None, Some("reverse")));
        let format = kb.invoked("builtins.str.format").expect("listed");
        assert!(format.values[1].names(None, Some("anything")));
        assert_eq!(kb.invoked("time.sleep"), None);
        // Cached-property decorators: each entry says which source it was read against, why the
        // value is in the instance `__dict__`, and how removing it acts.
        assert!(kb.cached_property("functools.cached_property").is_some());
        let werkzeug = kb
            .cached_property("werkzeug.utils.cached_property")
            .expect("listed");
        assert!(werkzeug.slot.is_some());
        assert_eq!(kb.cached_property("cached_property"), None);
        assert!(kb.cached_properties.values().all(|c| {
            !c.versions.is_empty() && !c.keeps.is_empty() && !c.invalidation.is_empty()
        }));
        // The table is no entry of the catalog: other facts never see it.
        assert_eq!(kb.lookup("builtins.isinstance"), None);
        let empty = EffectiveEcosystemKb::materialize(
            kb.entries.values().cloned().collect(),
            kb.opaque_packages.clone(),
        );
        assert_ne!(kb.digest, empty.digest);
        // A value no entry names, by neither position nor keyword.
        assert!(!InvokedValue::default().names(None, None));
    }

    #[test]
    fn user_override_config_converts_to_patch() {
        let ok = EcosystemOverride {
            symbol: "mypkg.workers.in_thread".into(),
            effects: Some(vec!["unpooled_client_alloc".into()]),
            async_safe: Some(true),
            offloads_blocking_arg: Some(0),
            excluded: Some(false),
            copy_sanitizer: Some(CopySanitizerSetting::Depth("deep".into())),
            return_type: Some("mypkg.workers.Handle".into()),
            sync_lock_or_tx: Some(false),
            non_reentrant_lock: Some(true),
            one_at_a_time: Some("when_unbound".into()),
            writes_in_order: Some(heki_env::OrderedOutputSetting {
                arg: None,
                param: Some("payload".into()),
                sorted_by: Some("sort_keys".into()),
                sorted_by_default: true,
            }),
            attribute_type: None,
        };
        assert_eq!(
            EcosystemPatch::try_from(&ok).unwrap(),
            EcosystemPatch {
                symbol: "mypkg.workers.in_thread".into(),
                effects: Some(vec![UnpooledClientAlloc]),
                async_safe: Some(true),
                offloads_blocking_arg: Some(0),
                excluded: Some(false),
                copy_sanitizer: Some(Some(CopyDepth::Deep)),
                return_type: Some("mypkg.workers.Handle".into()),
                sync_lock_or_tx: Some(false),
                non_reentrant_lock: Some(true),
                one_at_a_time: Some(OneAtATime::WhenUnbound),
                writes_in_order: Some(OrderedOutput {
                    arg: None,
                    param: Some("payload".into()),
                    sorted_by: Some("sort_keys".into()),
                    sorted_by_default: true,
                }),
                attribute_type: None,
            }
        );
        // An attribute's type is a row of its own: it takes no key of a call.
        let read = EcosystemOverride {
            symbol: "vendor.Client.chat".into(),
            attribute_type: Some("vendor.Chat".into()),
            ..EcosystemOverride::default()
        };
        assert_eq!(
            EcosystemPatch::try_from(&read).unwrap(),
            EcosystemPatch {
                symbol: "vendor.Client.chat".into(),
                attribute_type: Some("vendor.Chat".into()),
                ..EcosystemPatch::default()
            }
        );
        let mixed = EcosystemOverride {
            effects: Some(vec!["blocking_net".into()]),
            ..read.clone()
        };
        assert!(matches!(
            EcosystemPatch::try_from(&mixed),
            Err(KbError::AttributeTypeWithCallKeys(s)) if s == "vendor.Client.chat"
        ));
        for (name, kind) in [
            ("always", OneAtATime::Always),
            ("unbinds", OneAtATime::Unbinds),
        ] {
            let cfg = EcosystemOverride {
                symbol: "mypkg.Channel.recv".into(),
                one_at_a_time: Some(name.into()),
                ..EcosystemOverride::default()
            };
            assert_eq!(
                EcosystemPatch::try_from(&cfg).unwrap().one_at_a_time,
                Some(kind)
            );
        }
        let cfg = EcosystemOverride {
            symbol: "mypkg.Channel.recv".into(),
            one_at_a_time: Some("sometimes".into()),
            ..EcosystemOverride::default()
        };
        assert!(matches!(
            EcosystemPatch::try_from(&cfg),
            Err(KbError::InvalidOneAtATime(v)) if v == "sometimes"
        ));
        // `true` is a shallow copy (the values below stay protected), `false` none.
        for (setting, depth) in [
            (CopySanitizerSetting::Flag(true), Some(CopyDepth::Shallow)),
            (CopySanitizerSetting::Flag(false), None),
            (
                CopySanitizerSetting::Depth("shallow".into()),
                Some(CopyDepth::Shallow),
            ),
        ] {
            let cfg = EcosystemOverride {
                symbol: "mypkg.f".into(),
                copy_sanitizer: Some(setting),
                ..EcosystemOverride::default()
            };
            assert_eq!(
                EcosystemPatch::try_from(&cfg).unwrap().copy_sanitizer,
                Some(depth)
            );
        }
        let cfg = EcosystemOverride {
            symbol: "mypkg.f".into(),
            copy_sanitizer: Some(CopySanitizerSetting::Depth("full".into())),
            ..EcosystemOverride::default()
        };
        assert!(matches!(
            EcosystemPatch::try_from(&cfg),
            Err(KbError::InvalidCopyDepth(d)) if d == "full"
        ));
        // Keys left out stay unset.
        let bare = EcosystemOverride {
            symbol: "mypkg.f".into(),
            ..EcosystemOverride::default()
        };
        assert_eq!(
            EcosystemPatch::try_from(&bare).unwrap(),
            EcosystemPatch {
                symbol: "mypkg.f".into(),
                ..EcosystemPatch::default()
            }
        );
        for bad in ["blocking-net", "BlockingNet", "network", ""] {
            let cfg = EcosystemOverride {
                symbol: "mypkg.f".into(),
                effects: Some(vec!["sleep".into(), bad.into()]),
                ..EcosystemOverride::default()
            };
            let result = EcosystemPatch::try_from(&cfg);
            assert!(
                matches!(&result, Err(KbError::InvalidEffect(name)) if name == bad),
                "{bad}: {result:?}"
            );
        }
    }
}
