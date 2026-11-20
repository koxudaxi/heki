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

use heki_env::{EcosystemOverride, PythonVersion};
use serde::de::IntoDeserializer;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
    /// Parse an effect name spelled as in `kb/ecosystem/*.toml` (snake_case, e.g. `"blocking_net"`).
    pub fn from_kb_name(name: &str) -> Result<Self, KbError> {
        let de: serde::de::value::StrDeserializer<'_, serde::de::value::Error> =
            name.into_deserializer();
        Self::deserialize(de).map_err(|_| KbError::InvalidEffect(name.to_owned()))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EcosystemEntry {
    pub symbol: String,
    pub effects: Vec<ExecEffectKind>,
    pub async_safe: bool,
    pub offloads_blocking_arg: Option<u8>,
    pub excluded: bool,
    pub copy_sanitizer: bool,
    pub opaque_to_layer3: bool,
    /// Optional instance class returned by a factory function or method
    /// (e.g. `sqlite3.connect` -> `sqlite3.Connection`, `sqlite3.Connection.cursor` -> `sqlite3.Cursor`).
    pub return_type: Option<String>,
    /// `EFF106`: a `with` block over an instance of this class (a synchronous `threading` lock)
    /// or over the result of this call (a DB transaction starter such as
    /// `sqlalchemy.orm.Session.begin`) holds a lock or a transaction. Async locks are never
    /// flagged.
    pub sync_lock_or_tx: bool,
}

/// Validate a `[[tool.heki.ecosystem]]` override into a Layer 2 entry.
/// `opaque_to_layer3` is decided when merging (see [`EffectiveEcosystemKb::load_builtin`]).
impl TryFrom<&EcosystemOverride> for EcosystemEntry {
    type Error = KbError;

    fn try_from(value: &EcosystemOverride) -> Result<Self, KbError> {
        let effects = value
            .effects
            .iter()
            .map(|name| ExecEffectKind::from_kb_name(name))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            symbol: value.symbol.clone(),
            effects,
            async_safe: value.async_safe,
            offloads_blocking_arg: value.offloads_blocking_arg,
            excluded: value.excluded,
            copy_sanitizer: value.copy_sanitizer,
            opaque_to_layer3: false,
            return_type: None,
            sync_lock_or_tx: false,
        })
    }
}

/// On-disk shape of one `kb/ecosystem/*.toml` file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EcosystemKbFile {
    #[serde(default)]
    opaque_packages: Vec<String>,
    #[serde(default)]
    entries: Vec<EcosystemEntry>,
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
    pub digest: String,
}

impl EffectiveEcosystemKb {
    pub fn materialize(raw: Vec<EcosystemEntry>, opaque_packages: Vec<String>) -> Self {
        let mut entries = BTreeMap::new();
        for entry in raw {
            entries.insert(entry.symbol.clone(), entry);
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
            digest,
        }
    }

    /// Embed `kb/ecosystem/*.toml` via `include_str!` and merge user `overrides`.
    /// User overrides take precedence over built-in entries for the same `symbol` (the whole
    /// entry is replaced). An override inside an opaque package stays `opaque_to_layer3`.
    pub fn load_builtin(overrides: &[EcosystemEntry]) -> Result<Self, KbError> {
        let mut raw = Vec::new();
        let mut opaque_packages = Vec::new();
        for &(file, text) in ECOSYSTEM_KB_FILES {
            let parsed = parse_ecosystem_file(file, text)?;
            raw.extend(parsed.entries);
            opaque_packages.extend(parsed.opaque_packages);
        }
        opaque_packages.sort();
        opaque_packages.dedup();
        for entry in overrides {
            let mut entry = entry.clone();
            let package = top_level_package(&entry.symbol);
            entry.opaque_to_layer3 |= opaque_packages.iter().any(|p| p == package);
            // `materialize` inserts in order, so overrides (pushed last) replace built-ins.
            raw.push(entry);
        }
        Ok(Self::materialize(raw, opaque_packages))
    }

    pub fn lookup(&self, symbol: &str) -> Option<&EcosystemEntry> {
        self.entries.get(symbol)
    }

    pub fn is_opaque_to_layer3(&self, top_level_package: &str) -> bool {
        self.opaque_packages.iter().any(|p| p == top_level_package)
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

    // ---------------------------------------------------------------- Layer 1

    /// docs/ANALYSIS_AND_KB_SPEC.md section 4.1. `builtins.str` / `builtins.bytes` carry no
    /// `return_exact_type`: `__str__` / `__bytes__` may return a subclass that CPython passes
    /// through (witnessed by `tests/sentinel/verify_soundness_kb.py`).
    const LAYER1_REQUIRED: &[(&str, Option<&str>, &[SoundnessParamEffect])] = &[
        ("builtins.iter", None, &[RetainedInReturn]),
        ("builtins.enumerate", None, &[RetainedInReturn]),
        ("builtins.zip", None, &[RetainedInReturn]),
        ("builtins.map", None, &[ReadOnly, RetainedInReturn]),
        ("builtins.filter", None, &[ReadOnly, RetainedInReturn]),
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
        ("builtins.list.remove", None, &[Mutated, ReadOnly]),
        ("builtins.set.add", None, &[Mutated, Escaped]),
        ("builtins.set.update", None, &[Mutated, ReadOnly]),
        ("builtins.set.discard", None, &[Mutated, ReadOnly]),
        ("builtins.set.remove", None, &[Mutated, ReadOnly]),
        ("builtins.set.clear", None, &[Mutated]),
        ("builtins.dict.update", None, &[Mutated, ReadOnly]),
        ("builtins.dict.pop", None, &[Mutated, ReadOnly, ReadOnly]),
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

    // ---------------------------------------------------------------- Layer 2

    fn entry(effects: &[ExecEffectKind]) -> EcosystemEntry {
        EcosystemEntry {
            effects: effects.to_vec(),
            opaque_to_layer3: true,
            ..EcosystemEntry::default()
        }
    }

    fn with_prefix(prefix: &str, names: &[&str]) -> Vec<String> {
        names.iter().map(|n| format!("{prefix}.{n}")).collect()
    }

    /// docs/ANALYSIS_AND_KB_SPEC.md section 4.2, as `(symbols, expected entry without symbol)`.
    fn layer2_required() -> Vec<(Vec<String>, EcosystemEntry)> {
        let verbs8 = &[
            "get", "post", "put", "delete", "patch", "head", "options", "request",
        ];
        let async_safe = EcosystemEntry {
            async_safe: true,
            ..entry(&[])
        };
        let offload = |arg| EcosystemEntry {
            offloads_blocking_arg: Some(arg),
            async_safe: true,
            ..entry(&[])
        };
        let excluded = EcosystemEntry {
            excluded: true,
            ..entry(&[])
        };
        let copy_sanitizer = EcosystemEntry {
            copy_sanitizer: true,
            ..entry(&[])
        };
        let s = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        vec![
            (s(&["time.sleep"]), entry(&[Sleep])),
            (
                with_prefix(
                    "subprocess",
                    &[
                        "run",
                        "call",
                        "check_call",
                        "check_output",
                        "Popen.wait",
                        "Popen.communicate",
                    ],
                ),
                entry(&[BlockingSubprocess]),
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
                    "os.walk",
                    "shutil.copy",
                    "shutil.copy2",
                    "shutil.copytree",
                    "shutil.rmtree",
                ]),
                entry(&[BlockingFs]),
            ),
            (
                with_prefix(
                    "asyncio",
                    &["StreamWriter.write", "sleep", "gather", "wait_for"],
                ),
                async_safe.clone(),
            ),
            (
                s(&[
                    "asyncio.to_thread",
                    "anyio.to_thread.run_sync",
                    "starlette.concurrency.run_in_threadpool",
                    "fastapi.concurrency.run_in_threadpool",
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
                offload(1),
            ),
            (
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
                excluded.clone(),
            ),
            (
                with_prefix("logging", &["debug", "info", "warning", "error"]),
                excluded,
            ),
            (
                s(&[
                    "copy.deepcopy",
                    "copy.copy",
                    "pydantic.BaseModel.model_copy",
                    "builtins.dict.copy",
                    "builtins.list.copy",
                    "builtins.set.copy",
                ]),
                copy_sanitizer,
            ),
            (
                with_prefix("requests", verbs8),
                entry(&[BlockingNet, UnpooledClientAlloc]),
            ),
            (s(&["requests.Session"]), entry(&[UnpooledClientAlloc])),
            (
                with_prefix(
                    "requests.Session",
                    &["get", "post", "put", "delete", "patch", "request"],
                ),
                entry(&[BlockingNet]),
            ),
            (
                with_prefix("httpx", verbs8),
                entry(&[BlockingNet, UnpooledClientAlloc]),
            ),
            (
                s(&[
                    "httpx.Client",
                    "httpx.AsyncClient",
                    "aiohttp.ClientSession",
                    "urllib3.PoolManager",
                    "boto3.client",
                    "boto3.resource",
                    "boto3.Session",
                ]),
                entry(&[UnpooledClientAlloc]),
            ),
            (
                with_prefix(
                    "httpx.Client",
                    &["get", "post", "put", "delete", "patch", "send", "request"],
                ),
                entry(&[BlockingNet]),
            ),
            (
                with_prefix(
                    "httpx.AsyncClient",
                    &["get", "post", "put", "delete", "patch", "send", "request"],
                ),
                async_safe.clone(),
            ),
            (
                with_prefix(
                    "aiohttp.ClientSession",
                    &["get", "post", "put", "delete", "patch", "request"],
                ),
                async_safe,
            ),
            (
                s(&["urllib.request.urlopen"]),
                entry(&[BlockingNet, UnpooledClientAlloc]),
            ),
            (
                s(&["sqlite3.connect"]),
                EcosystemEntry {
                    return_type: Some("sqlite3.Connection".into()),
                    ..entry(&[BlockingDb, UnpooledClientAlloc])
                },
            ),
            (
                s(&["sqlite3.Connection.cursor"]),
                EcosystemEntry {
                    return_type: Some("sqlite3.Cursor".into()),
                    ..entry(&[])
                },
            ),
            (
                s(&["psycopg.connect"]),
                EcosystemEntry {
                    return_type: Some("psycopg.Connection".into()),
                    ..entry(&[BlockingDb, UnpooledClientAlloc])
                },
            ),
            (
                s(&["psycopg.Connection.cursor"]),
                EcosystemEntry {
                    return_type: Some("psycopg.Cursor".into()),
                    ..entry(&[])
                },
            ),
            (
                s(&["psycopg.Connection.execute"]),
                entry(&[BlockingDb, DbQuery]),
            ),
            (
                s(&["psycopg2.connect"]),
                EcosystemEntry {
                    return_type: Some("psycopg2.extensions.connection".into()),
                    ..entry(&[BlockingDb, UnpooledClientAlloc])
                },
            ),
            (
                s(&["psycopg2.extensions.connection.cursor"]),
                EcosystemEntry {
                    return_type: Some("psycopg2.extensions.cursor".into()),
                    ..entry(&[])
                },
            ),
            (
                s(&["pymysql.connect"]),
                EcosystemEntry {
                    return_type: Some("pymysql.connections.Connection".into()),
                    ..entry(&[BlockingDb, UnpooledClientAlloc])
                },
            ),
            (
                s(&["sqlalchemy.create_engine"]),
                EcosystemEntry {
                    return_type: Some("sqlalchemy.engine.Engine".into()),
                    ..entry(&[BlockingDb, UnpooledClientAlloc])
                },
            ),
            (
                s(&["sqlalchemy.engine.Engine.connect"]),
                EcosystemEntry {
                    return_type: Some("sqlalchemy.engine.Connection".into()),
                    ..entry(&[BlockingDb])
                },
            ),
            (
                s(&["redis.Redis", "redis.StrictRedis", "pymongo.MongoClient"]),
                entry(&[BlockingDb, UnpooledClientAlloc]),
            ),
            (
                [
                    with_prefix("sqlite3.Connection", &["execute", "executemany"]),
                    with_prefix(
                        "sqlite3.Cursor",
                        &["execute", "executemany", "fetchall", "fetchone"],
                    ),
                    s(&[
                        "psycopg.Cursor.execute",
                        "psycopg2.extensions.cursor.execute",
                        "sqlalchemy.engine.Connection.execute",
                    ]),
                    with_prefix(
                        "sqlalchemy.orm.Session",
                        &[
                            "execute", "query", "scalars", "scalar", "get", "commit", "flush",
                        ],
                    ),
                    with_prefix("sqlmodel.Session", &["exec", "execute", "get", "commit"]),
                    with_prefix(
                        "django.db.models.query.QuerySet",
                        &[
                            "get", "filter", "all", "first", "exists", "count", "create", "update",
                            "delete",
                        ],
                    ),
                    with_prefix(
                        "redis.Redis",
                        &["get", "set", "hget", "hset", "delete", "pipeline"],
                    ),
                    with_prefix(
                        "pymongo.collection.Collection",
                        &["find", "find_one", "insert_one", "update_one", "delete_one"],
                    ),
                ]
                .concat(),
                entry(&[BlockingDb, DbQuery]),
            ),
            (
                with_prefix(
                    "sqlalchemy.ext.asyncio.AsyncSession",
                    &["execute", "scalars", "scalar", "get", "commit"],
                ),
                EcosystemEntry {
                    async_safe: true,
                    ..entry(&[DbQuery])
                },
            ),
            (
                s(&[
                    "pandas.concat",
                    "numpy.vstack",
                    "numpy.hstack",
                    "numpy.concatenate",
                    "numpy.append",
                ]),
                entry(&[QuadraticRealloc]),
            ),
            (
                with_prefix(
                    "asyncio",
                    &["get_running_loop", "get_event_loop", "new_event_loop"],
                ),
                EcosystemEntry {
                    return_type: Some("asyncio.AbstractEventLoop".into()),
                    ..entry(&[])
                },
            ),
            (
                [LOCK_CLASSES, TRANSACTION_STARTERS]
                    .concat()
                    .iter()
                    .map(|n| (*n).to_owned())
                    .collect(),
                EcosystemEntry {
                    sync_lock_or_tx: true,
                    ..entry(&[])
                },
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
    /// `EFF106` DB transaction starters (call results held by `with`).
    const TRANSACTION_STARTERS: &[&str] = &[
        "sqlalchemy.orm.Session.begin",
        "sqlalchemy.orm.Session.begin_nested",
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
        // Async locks never hold the event loop's thread: they are not registered at all.
        for symbol in [
            "asyncio.Lock",
            "asyncio.Semaphore",
            "asyncio.BoundedSemaphore",
            "asyncio.Condition",
            "anyio.Lock",
            "anyio.Semaphore",
            "sqlalchemy.ext.asyncio.AsyncSession.begin",
        ] {
            assert_eq!(kb.lookup(symbol), None, "{symbol}");
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
        "requests",
        "httpx",
        "aiohttp",
        "urllib3",
        "sqlalchemy",
        "sqlmodel",
        "django",
        "psycopg",
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
    ];

    #[test]
    fn layer2_load_builtin_contains_every_required_entry() {
        let kb = EffectiveEcosystemKb::load_builtin(&[]).unwrap();
        for (symbols, expected) in layer2_required() {
            for symbol in symbols {
                let expected = EcosystemEntry {
                    symbol: symbol.clone(),
                    ..expected.clone()
                };
                assert_eq!(kb.lookup(&symbol), Some(&expected), "{symbol}");
            }
        }
        assert_eq!(kb.digest.len(), 64);
    }

    #[test]
    fn layer2_does_not_register_popen_spawn() {
        let kb = EffectiveEcosystemKb::load_builtin(&[]).unwrap();
        assert_eq!(kb.lookup("subprocess.Popen"), None);
        assert!(kb.lookup("subprocess.Popen.wait").is_some());
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

    #[test]
    fn layer2_user_overrides_take_precedence() {
        let builtin = EffectiveEcosystemKb::load_builtin(&[]).unwrap();
        let overrides = [
            // Replaces the built-in `time.sleep` entry as a whole.
            EcosystemEntry {
                symbol: "time.sleep".into(),
                excluded: true,
                ..EcosystemEntry::default()
            },
            // A project-local symbol is added and is not opaque.
            EcosystemEntry {
                symbol: "mypkg.http.fetch".into(),
                effects: vec![BlockingNet],
                ..EcosystemEntry::default()
            },
        ];
        let kb = EffectiveEcosystemKb::load_builtin(&overrides).unwrap();
        assert_eq!(
            kb.lookup("time.sleep"),
            Some(&EcosystemEntry {
                symbol: "time.sleep".into(),
                excluded: true,
                opaque_to_layer3: true,
                ..EcosystemEntry::default()
            })
        );
        assert_eq!(
            kb.lookup("mypkg.http.fetch"),
            Some(&EcosystemEntry {
                symbol: "mypkg.http.fetch".into(),
                effects: vec![BlockingNet],
                ..EcosystemEntry::default()
            })
        );
        assert_eq!(kb.entries.len(), builtin.entries.len() + 1);
        assert_eq!(kb.opaque_packages, builtin.opaque_packages);
        assert_ne!(kb.digest, builtin.digest);
    }

    #[test]
    fn user_override_config_converts_to_entry() {
        let ok = EcosystemOverride {
            symbol: "mypkg.workers.in_thread".into(),
            effects: vec!["blocking_sync".into()],
            async_safe: true,
            offloads_blocking_arg: Some(0),
            excluded: false,
            copy_sanitizer: true,
        };
        assert_eq!(
            EcosystemEntry::try_from(&ok).unwrap(),
            EcosystemEntry {
                symbol: "mypkg.workers.in_thread".into(),
                effects: vec![BlockingSync],
                async_safe: true,
                offloads_blocking_arg: Some(0),
                excluded: false,
                copy_sanitizer: true,
                opaque_to_layer3: false,
                return_type: None,
                // Overrides cannot declare locks or transactions (`[[tool.heki.ecosystem]]`
                // has no such key).
                sync_lock_or_tx: false,
            }
        );
        for bad in ["blocking-net", "BlockingNet", "network", ""] {
            let cfg = EcosystemOverride {
                symbol: "mypkg.f".into(),
                effects: vec!["sleep".into(), bad.into()],
                ..EcosystemOverride::default()
            };
            let result = EcosystemEntry::try_from(&cfg);
            assert!(
                matches!(&result, Err(KbError::InvalidEffect(name)) if name == bad),
                "{bad}: {result:?}"
            );
        }
    }
}
