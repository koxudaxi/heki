//! Rule registry spanning all 3 families derived from the single Effect & Alias theory.
//!
//! - Family 1 (`MUT`): Mutation & State Effect (`MUT101`, `MUT103`, `MUT106`, `MUT107`, `MUT108`,
//!   `MUT109`, `MUT110`, `MUT111`, `MUT112`, `MUT113`, `MUT201`, `MUT202`, `MUT203`, `MUT204`,
//!   `MUT205`, `MUT206`, `MUT207`)
//! - Family 2 (`EFF` / `DATA`): Execution Effect (`EFF101`, `EFF102`, `EFF103`, `EFF105`, `EFF106`,
//!   `EFF107`, `EFF201`, `DATA101`)
//! - Family 3 (`OPT`): Complexity Rewrite (`OPT101`, `OPT102`, `OPT103`, `OPT110`, `OPT111`,
//!   `OPT112`, `OPT113`, `OPT120`, `OPT121`, `OPT130`); `OPT112` / `OPT113` are Advisory-lane
//!   reports without a rewrite.

mod contracts;
mod data_rules;
mod eff_rules;
pub mod experimental;
mod mut_rules;
mod opt_rules;
pub mod project;
pub mod selection;
pub mod snapshot;

pub use contracts::{MAX_INVALIDATIONS, opt_in_facts, summary_contracts, typed_writes};
pub use eff_rules::{ROOT_CAUSE_GROUP_THRESHOLD, fold_root_causes};
pub use heki_diagnostics::Certainty;
pub use mut_rules::{
    CHAIN_RULES, ChainFacts, StaleWriteKey, chain_facts, family_readers, stale_writes_by_file,
};
pub use selection::{RuleSelection, SelectionError, Selectors, heuristic_hint};
pub use snapshot::{SNAPSHOT_FILE, SnapshotError, SummarySnapshot};

use globset::{Glob, GlobMatcher};
use heki_ast::{LineIndex, Span};
use heki_diagnostics::{Diagnostic, RelatedLocation};
use heki_effect::{
    CallHop, CallSite, EffectSummary, FileSummary, FuncKey, ModuleInput, ProjectDecls, ProjectIndex,
};
use heki_env::{HekiConfig, Severity};
use heki_ir::{IrFunction, ModuleConstants};
use heki_kb::{EffectiveEcosystemKb, EffectiveSoundnessKb};
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RuleFamily {
    Mut,
    Eff,
    Data,
    Opt,
}

/// What happens in the code a rule reports, which sets its default severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Consequence {
    /// A bug: a wrong result, an exception, a lost update, a deadlock, or a broken contract.
    Bug,
    /// Slower code that computes the right result.
    Slowdown,
}

/// When a rule runs without being named by its code or name (ADR-018).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Activation {
    /// In the default selection, and matched by a prefix or `ALL`, while the rule is exact;
    /// whatever its certainty with `lint.heuristic`.
    Default,
    /// Only by its exact code or name.
    OptIn,
    /// Selected as an exact `Default` rule is, whatever its certainty: it reports only what a
    /// declared contract asks for, so declaring the contract is what selects it.
    Contract,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RuleMeta {
    pub code: &'static str,
    pub slug: &'static str,
    pub family: RuleFamily,
    pub certainty: Certainty,
    pub activation: Activation,
    pub consequence: Consequence,
    pub auto_fix_default: bool,
    /// A proven-safe rewrite that `--fix` applies only with `--include-size-dependent` or in a
    /// function a `--profile` dump shows hot.
    pub size_dependent_fix: bool,
    pub summary: &'static str,
    /// Ruff-style documentation (`crates/heki_rules/rule-docs/<CODE>.md`): printed by
    /// `heki rule <CODE>` and published as the rule's page.
    pub doc: &'static str,
}

impl RuleMeta {
    /// Whether the rule runs without being named: in the default selection and for a prefix or
    /// `ALL`. `heuristic` is `lint.heuristic`.
    pub const fn runs_unnamed(&self, heuristic: bool) -> bool {
        match self.activation {
            Activation::Default => heuristic || matches!(self.certainty, Certainty::Exact),
            Activation::Contract => true,
            Activation::OptIn => false,
        }
    }

    /// Whether the enabled rule shows its heuristic reports, those of a heuristic rule or the
    /// single heuristic reports of an exact one (an `OPT` report whose rewrite is not proven):
    /// where a heuristic rule would run, that is with `lint.heuristic` (`heuristic`), when
    /// `select` / `extend-select` names its code or name (`named`), or for a contract rule.
    pub const fn shows_heuristic_reports(&self, named: bool, heuristic: bool) -> bool {
        match self.activation {
            Activation::Default => {
                heuristic || named || matches!(self.certainty, Certainty::Heuristic)
            }
            Activation::OptIn | Activation::Contract => true,
        }
    }

    /// What `heki rule <CODE>` and the rule's page say about a heuristic rule: what its
    /// certainty means and how it is selected. `None` for an exact rule.
    pub fn heuristic_note(&self) -> Option<String> {
        if self.certainty == Certainty::Exact {
            return None;
        }
        let runs = match self.activation {
            Activation::Default => {
                "It runs when `select` or `extend-select` names its code or name, and with \
                 `lint.heuristic = true` (`--heuristic`) also by default and for its prefixes \
                 and `ALL`."
            }
            Activation::OptIn => {
                "It runs only when `select` or `extend-select` names its code or name."
            }
            Activation::Contract => "It runs when its contract is declared.",
        };
        Some(format!(
            "This rule is heuristic: a premise of its claim can be open (\"What a report \
             states\" lists them), so a report can be wrong about the code. {runs}"
        ))
    }

    /// The rule's published page.
    pub fn page_url(&self) -> String {
        format!("{RULE_PAGES}{}/", self.slug)
    }

    /// The severity of the rule's diagnostics unless `lint.severity` names the rule: `error` for
    /// a bug, `warning` for slower code.
    pub const fn default_severity(&self) -> Severity {
        match self.consequence {
            Consequence::Bug => Severity::Error,
            Consequence::Slowdown => Severity::Warning,
        }
    }
}

/// Where the rule pages are published: the documentation's `site_url` (`zensical.toml`).
pub const RULE_PAGES: &str = "https://heki.koxudaxi.dev/rules/";

/// Core v1.0 rule catalog (empirically validated in Stage 0-A + `dcg` study).
pub const CORE_RULES: &[RuleMeta] = &[
    RuleMeta {
        code: "MUT101",
        slug: "cached-property-stale-mutation",
        family: RuleFamily::Mut,
        certainty: Certainty::Heuristic,
        activation: Activation::OptIn,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Mutating an attribute read by `@cached_property` without invalidating the cache.",
        doc: include_str!("../rule-docs/MUT101.md"),
    },
    RuleMeta {
        code: "MUT103",
        slug: "summary-effect-regression",
        family: RuleFamily::Mut,
        certainty: Certainty::Heuristic,
        activation: Activation::OptIn,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Interprocedural mutation/escape/effect regression against saved summary snapshot.",
        doc: include_str!("../rule-docs/MUT103.md"),
    },
    RuleMeta {
        code: "MUT106",
        slug: "iterator-double-consumption",
        family: RuleFamily::Mut,
        certainty: Certainty::Heuristic,
        activation: Activation::OptIn,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Opt-in: consuming a single-pass iterator or generator a second time (the second pass sees no items).",
        doc: include_str!("../rule-docs/MUT106.md"),
    },
    RuleMeta {
        code: "MUT107",
        slug: "hash-attr-mutation",
        family: RuleFamily::Mut,
        certainty: Certainty::Heuristic,
        activation: Activation::Default,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Mutating an attribute read by `__hash__` after initialization (stored `set` / `dict` entries become unreachable).",
        doc: include_str!("../rule-docs/MUT107.md"),
    },
    RuleMeta {
        code: "MUT108",
        slug: "readonly-type-mutation",
        family: RuleFamily::Mut,
        certainty: Certainty::Heuristic,
        activation: Activation::OptIn,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Opt-in: changing in place a field that a standard type declares read-only (frozen dataclass / `NamedTuple` / `dataclass_transform` fields, `Final` / `ReadOnly` attributes, `ReadOnly` `TypedDict` keys), directly, through an alias, or in a helper.",
        doc: include_str!("../rule-docs/MUT108.md"),
    },
    RuleMeta {
        code: "MUT109",
        slug: "write-without-invalidation",
        family: RuleFamily::Mut,
        certainty: Certainty::Heuristic,
        activation: Activation::Contract,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Writing a declared `invalidation` target (directly, via alias, or via helper) without reaching one of its invalidators on every path to the function's return.",
        doc: include_str!("../rule-docs/MUT109.md"),
    },
    RuleMeta {
        code: "MUT110",
        slug: "readonly-param-mutation",
        family: RuleFamily::Mut,
        certainty: Certainty::Heuristic,
        activation: Activation::Default,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Handing a `Sequence` / `Mapping` / `ReadOnly` parameter to a function that changes it in place (directly, through an alias, or through helpers).",
        doc: include_str!("../rule-docs/MUT110.md"),
    },
    RuleMeta {
        code: "MUT111",
        slug: "cached-property-stale-read",
        family: RuleFamily::Mut,
        certainty: Certainty::Heuristic,
        activation: Activation::OptIn,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Opt-in: reading a `@cached_property` that a method of its class family can leave stale (a write `MUT101` reports).",
        doc: include_str!("../rule-docs/MUT111.md"),
    },
    RuleMeta {
        code: "MUT112",
        slug: "conditional-ownership-mutation",
        family: RuleFamily::Mut,
        certainty: Certainty::Heuristic,
        activation: Activation::OptIn,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Opt-in: changing in place a value that is a new object on one path and borrowed state (a parameter, an attribute, a global, or what a function returns of such state) on another.",
        doc: include_str!("../rule-docs/MUT112.md"),
    },
    RuleMeta {
        code: "MUT113",
        slug: "iterated-container-mutation",
        family: RuleFamily::Mut,
        certainty: Certainty::Heuristic,
        activation: Activation::OptIn,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Opt-in: a dict, set, or deque of the instance changed while a method iterates it, by another method while a generator is suspended in the loop or by a function the loop's body calls (the next step raises `RuntimeError`).",
        doc: include_str!("../rule-docs/MUT113.md"),
    },
    RuleMeta {
        code: "MUT201",
        slug: "write-owner-violation",
        family: RuleFamily::Mut,
        certainty: Certainty::Heuristic,
        activation: Activation::Contract,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Writing a guarded attribute/container (directly, via alias, or via helper) outside declared `write-owners`.",
        doc: include_str!("../rule-docs/MUT201.md"),
    },
    RuleMeta {
        code: "MUT202",
        slug: "readonly-return-mutation",
        family: RuleFamily::Mut,
        certainty: Certainty::Heuristic,
        // Not `Contract`: it also reports without one (`@lru_cache` / `@cache` results).
        activation: Activation::Default,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Mutating a shared object returned by a declared `readonly-returns` producer or `@lru_cache`, directly or through the functions that return it.",
        doc: include_str!("../rule-docs/MUT202.md"),
    },
    RuleMeta {
        code: "MUT203",
        slug: "read-owner-violation",
        family: RuleFamily::Mut,
        certainty: Certainty::Heuristic,
        activation: Activation::Contract,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Reading a guarded attribute (directly, via alias, `getattr`, or `__dict__`) outside declared `read-owners`.",
        doc: include_str!("../rule-docs/MUT203.md"),
    },
    RuleMeta {
        code: "MUT204",
        slug: "call-owner-violation",
        family: RuleFamily::Mut,
        certainty: Certainty::Heuristic,
        activation: Activation::Contract,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Calling a guarded function or method, or constructing a guarded class (directly, via a bound method, `getattr`, or a helper), outside declared `call-owners`.",
        doc: include_str!("../rule-docs/MUT204.md"),
    },
    RuleMeta {
        code: "MUT205",
        slug: "unresolved-guarded-write",
        family: RuleFamily::Mut,
        certainty: Certainty::Heuristic,
        activation: Activation::Contract,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Strict mode of a `write-owners` contract (`unresolved = \"report\"`): writing an attribute named like a target on an object of unknown class, or a computed attribute name on a guarded class.",
        doc: include_str!("../rule-docs/MUT205.md"),
    },
    RuleMeta {
        code: "MUT206",
        slug: "unresolved-guarded-read",
        family: RuleFamily::Mut,
        certainty: Certainty::Heuristic,
        activation: Activation::Contract,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Strict mode of a `read-owners` contract (`unresolved = \"report\"`): reading an attribute named like a target on an object of unknown class.",
        doc: include_str!("../rule-docs/MUT206.md"),
    },
    RuleMeta {
        code: "MUT207",
        slug: "unresolved-guarded-call",
        family: RuleFamily::Mut,
        certainty: Certainty::Heuristic,
        activation: Activation::Contract,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Strict mode of a `call-owners` contract (`unresolved = \"report\"`): calling a method named like a target on an object of unknown class.",
        doc: include_str!("../rule-docs/MUT207.md"),
    },
    RuleMeta {
        code: "EFF101",
        slug: "async-blocking-call",
        family: RuleFamily::Eff,
        certainty: Certainty::Heuristic,
        activation: Activation::Default,
        consequence: Consequence::Slowdown,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Direct or indirect blocking network/DB/subprocess/sleep call reachable from `async def`.",
        doc: include_str!("../rule-docs/EFF101.md"),
    },
    RuleMeta {
        code: "EFF102",
        slug: "loop-unpooled-client",
        family: RuleFamily::Eff,
        certainty: Certainty::Heuristic,
        activation: Activation::Default,
        consequence: Consequence::Slowdown,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Allocating an unpooled HTTP/DB/RPC client or sessionless request inside a loop.",
        doc: include_str!("../rule-docs/EFF102.md"),
    },
    RuleMeta {
        code: "EFF103",
        slug: "loop-db-query",
        family: RuleFamily::Eff,
        certainty: Certainty::Heuristic,
        activation: Activation::Default,
        consequence: Consequence::Slowdown,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Explicit DB query / external RPC executed inside a loop (N+1).",
        doc: include_str!("../rule-docs/EFF103.md"),
    },
    RuleMeta {
        code: "EFF105",
        slug: "async-blocking-fs",
        family: RuleFamily::Eff,
        certainty: Certainty::Heuristic,
        activation: Activation::OptIn,
        consequence: Consequence::Slowdown,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Opt-in: blocking local filesystem I/O reachable from `async def`.",
        doc: include_str!("../rule-docs/EFF105.md"),
    },
    RuleMeta {
        code: "EFF106",
        slug: "lock-or-tx-held-across-io",
        family: RuleFamily::Eff,
        certainty: Certainty::Heuristic,
        activation: Activation::OptIn,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Opt-in: `await` or blocking network I/O while holding a `threading` lock or a DB transaction.",
        doc: include_str!("../rule-docs/EFF106.md"),
    },
    RuleMeta {
        code: "EFF107",
        slug: "unretained-async-task",
        family: RuleFamily::Eff,
        certainty: Certainty::Heuristic,
        activation: Activation::OptIn,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Opt-in: an `asyncio.create_task` result bound to a local variable but never awaited, returned, or stored.",
        doc: include_str!("../rule-docs/EFF107.md"),
    },
    RuleMeta {
        code: "EFF201",
        slug: "effect-boundary-violation",
        family: RuleFamily::Eff,
        certainty: Certainty::Heuristic,
        activation: Activation::Contract,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Reaching a forbidden effect (directly or through calls) from a declared `effect-boundaries` scope.",
        doc: include_str!("../rule-docs/EFF201.md"),
    },
    RuleMeta {
        code: "DATA101",
        slug: "loop-carried-concat",
        family: RuleFamily::Data,
        certainty: Certainty::Heuristic,
        activation: Activation::Default,
        consequence: Consequence::Slowdown,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Loop-carried quadratic `pandas.concat` / `numpy.vstack` / `numpy.append` accumulation.",
        doc: include_str!("../rule-docs/DATA101.md"),
    },
    RuleMeta {
        code: "OPT101",
        slug: "list-quadratic-concat",
        family: RuleFamily::Opt,
        certainty: Certainty::Heuristic,
        activation: Activation::Default,
        consequence: Consequence::Slowdown,
        size_dependent_fix: false,
        auto_fix_default: true,
        summary: "Unconditionally profitable `list` quadratic concatenation (`x = x + [e]` -> `x.append(e)`).",
        doc: include_str!("../rule-docs/OPT101.md"),
    },
    RuleMeta {
        code: "OPT102",
        slug: "set-dict-quadratic-merge",
        family: RuleFamily::Opt,
        certainty: Certainty::Heuristic,
        activation: Activation::Default,
        consequence: Consequence::Slowdown,
        size_dependent_fix: false,
        auto_fix_default: true,
        summary: "Unconditionally profitable `set`/`dict` quadratic merge (`{**d, ...}` with `d` first).",
        doc: include_str!("../rule-docs/OPT102.md"),
    },
    RuleMeta {
        code: "OPT103",
        slug: "immutable-quadratic-concat",
        family: RuleFamily::Opt,
        certainty: Certainty::Heuristic,
        activation: Activation::Default,
        consequence: Consequence::Slowdown,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Size-dependent immutable quadratic accumulation (`bytes`/`tuple`/attr `str`).",
        doc: include_str!("../rule-docs/OPT103.md"),
    },
    RuleMeta {
        code: "OPT110",
        slug: "loop-linear-membership",
        family: RuleFamily::Opt,
        certainty: Certainty::Heuristic,
        activation: Activation::Default,
        consequence: Consequence::Slowdown,
        size_dependent_fix: true,
        auto_fix_default: false,
        summary: "Size-dependent `e in lst` inside a loop -> hoist `set` outside loop.",
        doc: include_str!("../rule-docs/OPT110.md"),
    },
    RuleMeta {
        code: "OPT111",
        slug: "loop-linear-dedup",
        family: RuleFamily::Opt,
        certainty: Certainty::Heuristic,
        activation: Activation::Default,
        consequence: Consequence::Slowdown,
        size_dependent_fix: true,
        auto_fix_default: false,
        summary: "Size-dependent `if v not in seen: seen.append(v)` -> auxiliary `set`.",
        doc: include_str!("../rule-docs/OPT111.md"),
    },
    RuleMeta {
        code: "OPT112",
        slug: "loop-linear-list-lookup",
        family: RuleFamily::Opt,
        certainty: Certainty::Heuristic,
        activation: Activation::Default,
        consequence: Consequence::Slowdown,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Size-dependent `list.index` / `list.remove` / `list.count` inside a loop (repeated linear scans).",
        doc: include_str!("../rule-docs/OPT112.md"),
    },
    RuleMeta {
        code: "OPT113",
        slug: "nested-loop-equijoin",
        family: RuleFamily::Opt,
        certainty: Certainty::Heuristic,
        activation: Activation::Default,
        consequence: Consequence::Slowdown,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Size-dependent nested-loop equi-join (`for a in A: for b in B: if a.k == b.k`) -> `dict` index.",
        doc: include_str!("../rule-docs/OPT113.md"),
    },
    RuleMeta {
        code: "OPT120",
        slug: "list-front-pop",
        family: RuleFamily::Opt,
        certainty: Certainty::Heuristic,
        activation: Activation::Default,
        consequence: Consequence::Slowdown,
        size_dependent_fix: true,
        auto_fix_default: false,
        summary: "Size-dependent `q.pop(0)` / `del q[0]` / `q = q[1:]` -> `collections.deque`.",
        doc: include_str!("../rule-docs/OPT120.md"),
    },
    RuleMeta {
        code: "OPT121",
        slug: "list-front-insert",
        family: RuleFamily::Opt,
        certainty: Certainty::Heuristic,
        activation: Activation::Default,
        consequence: Consequence::Slowdown,
        size_dependent_fix: true,
        auto_fix_default: false,
        summary: "Size-dependent `q.insert(0, v)` -> `collections.deque`.",
        doc: include_str!("../rule-docs/OPT121.md"),
    },
    RuleMeta {
        code: "OPT130",
        slug: "loop-invariant-container-rebuild",
        family: RuleFamily::Opt,
        certainty: Certainty::Heuristic,
        activation: Activation::Default,
        consequence: Consequence::Slowdown,
        size_dependent_fix: true,
        auto_fix_default: false,
        summary: "Size-dependent loop-invariant container reconstruction (`e in set(keys)`).",
        doc: include_str!("../rule-docs/OPT130.md"),
    },
];

/// Look up a rule by either code (`EFF101`) or slug (`async-blocking-call`).
pub fn find_rule(query: &str) -> Option<&'static RuleMeta> {
    CORE_RULES
        .iter()
        .find(|r| r.code.eq_ignore_ascii_case(query) || r.slug.eq_ignore_ascii_case(query))
}

// ==================================================================== project check

/// One analyzed module: its summarization input plus source text for positions.
pub struct ModuleUnit<'a> {
    pub input: ModuleInput<'a>,
    pub source: &'a str,
    /// Project-relative POSIX path (`per-file-ignores`, test-path relaxation).
    pub rel_path: String,
    pub lines: LineIndex,
}

#[derive(Debug, Error)]
pub enum RuleConfigError {
    /// A `targets` / `write-targets` entry of `section` that is not `Class.attr`.
    #[error("invalid {section} target `{target}` (expected `Class.attr`)")]
    InvalidTarget {
        section: &'static str,
        target: String,
    },
    /// A container entry (`Class.attr["key"]`) among read targets.
    #[error(
        "invalid {section} target `{target}`: a container entry (`Class.attr[\"key\"]`) is a write target only; reads are of `Class.attr`"
    )]
    ItemTarget {
        section: &'static str,
        target: String,
    },
    #[error(
        "unknown `forbid` token `{token}` in effect-boundaries `{boundary}` (expected {expected})"
    )]
    UnknownForbid {
        boundary: String,
        token: String,
        expected: &'static str,
    },
    /// A `forbid` token of an effect boundary without the targets it needs.
    #[error("effect-boundaries `{boundary}` forbids `{token}` but sets no `{key}`")]
    MissingTargets {
        boundary: String,
        token: &'static str,
        key: &'static str,
    },
    #[error("effect-boundaries `{0}` sets `allow-writes` but no `write-universe`")]
    AllowWritesWithoutUniverse(String),
    /// A mode key of a contract (`fresh-objects`, `unresolved`) with an unknown value.
    #[error("unknown `{key}` value `{value}` in {section} `{contract}` (expected {expected})")]
    UnknownMode {
        section: &'static str,
        contract: String,
        key: &'static str,
        value: String,
        expected: &'static str,
    },
    #[error("invalid glob `{pattern}` in {context}: {cause}")]
    Glob {
        pattern: String,
        context: &'static str,
        cause: globset::Error,
    },
    #[error("the globs of {context} are too many to match together: {cause}")]
    GlobSet {
        context: &'static str,
        cause: globset::Error,
    },
    #[error("{0} `invalidation` contracts declared; at most {MAX_INVALIDATIONS} are supported")]
    TooManyInvalidations(usize),
}

/// Inputs beyond the Advisory lane: the Layer 1 KB for `OPT` (Soundness lane) and the saved
/// summary snapshot for `MUT103`.
pub struct CheckExtras<'a> {
    pub soundness_kb: &'a EffectiveSoundnessKb,
    /// `None`: no snapshot was loaded (`MUT103` reports nothing).
    pub snapshot: Option<&'a SummarySnapshot>,
    /// Declarations of every indexed module, when the caller keeps them up to date (the LSP
    /// engine); otherwise they are built from the index.
    pub decls: Option<&'a ProjectDecls>,
    /// Whether some module of the project patches `builtins` (Assumption A1(b), see
    /// [`project::ParsedFile::patches_builtins`]), when the caller knows it for files it does
    /// not check (the LSP engine checks one); otherwise decided from the checked files.
    pub project_patches_builtins: Option<bool>,
    /// The text of a file that is not checked, for the positions of the call-chain hops in it
    /// (the LSP engine checks one file).
    pub text_of: Option<&'a TextOf>,
}

/// Reads the text of a file by path.
pub type TextOf = dyn Fn(&Path) -> Option<String> + Sync;

/// `caller -> [(defining file, call site)]` (see [`CheckContext::sites_by_caller`]).
pub(crate) type SitesByCaller<'a> = FxHashMap<&'a FuncKey, Vec<(&'a Path, &'a CallSite)>>;

/// Shared state of one `heki check` run.
pub(crate) struct CheckContext<'a> {
    pub config: &'a HekiConfig,
    pub kb: &'a EffectiveEcosystemKb,
    pub soundness: &'a EffectiveSoundnessKb,
    pub index: &'a ProjectIndex,
    pub decls: &'a ProjectDecls,
    pub units: &'a [ModuleUnit<'a>],
    pub selection: &'a RuleSelection,
    unit_by_path: FxHashMap<&'a Path, usize>,
    text_of: Option<&'a TextOf>,
}

/// Run every enabled rule over an analyzed project.
/// Inline suppressions and baselines are applied by the caller (`heki check`).
pub fn check_project(
    units: &[ModuleUnit<'_>],
    config: &HekiConfig,
    kb: &EffectiveEcosystemKb,
    index: &ProjectIndex,
    selection: &RuleSelection,
    extras: &CheckExtras<'_>,
) -> Result<Vec<Diagnostic>, RuleConfigError> {
    let owned_decls;
    let decls = match extras.decls {
        Some(d) => d,
        None => {
            owned_decls = index.project_decls();
            &owned_decls
        }
    };
    let cx = CheckContext {
        config,
        kb,
        soundness: extras.soundness_kb,
        index,
        decls,
        units,
        selection,
        unit_by_path: units
            .iter()
            .enumerate()
            .map(|(i, u)| (u.input.path.as_path(), i))
            .collect(),
        text_of: extras.text_of,
    };
    let mut out = Vec::new();
    mut_rules::mut101_mut111(&cx, &mut out);
    mut_rules::mut107(&cx, &mut out);
    mut_rules::mut113(&cx, &mut out);
    mut_rules::mut108_mut110(&cx, &mut out);
    mut_rules::mut112(&cx, &mut out);
    mut_rules::mut109(&cx, &mut out)?;
    mut_rules::mut201(&cx, &mut out)?;
    mut_rules::mut203(&cx, &mut out)?;
    mut_rules::mut204(&cx, &mut out)?;
    eff_rules::eff101_105(&cx, &mut out);
    eff_rules::eff201(&cx, &mut out)?;
    if let Some(snap) = extras.snapshot {
        snapshot::mut103(&cx, snap, &mut out);
    }
    let readonly = mut_rules::ReadOnlyReturns::new(&cx)?;
    // Assumption A1(b) is project-wide: one module patching `builtins` taints them all.
    let opt_opts = heki_soundness::AnalysisOptions {
        project_patches_builtins: extras.project_patches_builtins.unwrap_or_else(|| {
            units
                .iter()
                .any(|u| project::runtime_patches_builtins(&u.input))
        }),
        disabled_obligations: Vec::new(),
    };
    let per_unit: Vec<Vec<Diagnostic>> = units
        .par_iter()
        .map(|u| {
            let mut d = Vec::new();
            mut_rules::mut106(&cx, u, &mut d);
            mut_rules::mut202(&cx, u, &readonly, &mut d);
            eff_rules::eff102_103(&cx, u, &mut d);
            eff_rules::eff106(&cx, u, &mut d);
            eff_rules::eff107(&cx, u, &mut d);
            data_rules::data101(&cx, u, &mut d);
            opt_rules::opt(&cx, u, &opt_opts, &mut d);
            opt_rules::opt112(&cx, u, &mut d);
            opt_rules::opt113(&cx, u, &mut d);
            d
        })
        .collect();
    out.extend(per_unit.into_iter().flatten());
    heki_diagnostics::sort_diagnostics(&mut out);
    out.dedup_by(|a, b| {
        a.path == b.path && a.span == b.span && a.rule_code == b.rule_code && a.target == b.target
    });
    Ok(out)
}

impl<'a> CheckContext<'a> {
    /// The unit's module constants without the displays another project module may change
    /// (`ProjectDecls::changed_elsewhere`): the constants the Advisory rules take as fixed.
    /// What its comprehension functions return goes with them, as it may rest on one.
    pub fn fixed_constants<'u>(&self, unit: &'u ModuleUnit<'_>) -> Cow<'u, ModuleConstants> {
        let own = &unit.input.ir.constants;
        let changed: Vec<&String> = own
            .displays
            .iter()
            .filter(|(n, d)| {
                self.decls
                    .changed_elsewhere(&format!("{}.{n}", unit.input.module_name), d)
            })
            .map(|(n, _)| n)
            .collect();
        if changed.is_empty() {
            return Cow::Borrowed(own);
        }
        let mut fixed = own.clone();
        for n in changed {
            fixed.displays.remove(n);
        }
        fixed.comprehensions.clear();
        Cow::Owned(fixed)
    }

    pub fn unit(&self, path: &Path) -> Option<&'a ModuleUnit<'a>> {
        self.unit_by_path.get(path).map(|&i| &self.units[i])
    }

    /// Whether `code` runs on `unit` (selection + `per-file-ignores`).
    pub fn enabled(&self, code: &str, unit: &ModuleUnit<'_>) -> bool {
        self.selection.is_enabled_for(code, &unit.rel_path)
    }

    /// `FuncKey` of an IR function of `unit`.
    pub fn key(unit: &ModuleUnit<'_>, f: &IrFunction) -> FuncKey {
        FuncKey {
            module: unit.input.module_name.clone(),
            qualname: heki_effect::func_qualname(f).to_owned(),
        }
    }

    /// Call sites of `unit` keyed by `(caller qualname, span)`.
    pub fn sites_by_span(
        &self,
        unit: &ModuleUnit<'_>,
    ) -> FxHashMap<(String, Span), Vec<&'a CallSite>> {
        let mut out: FxHashMap<(String, Span), Vec<&CallSite>> = FxHashMap::default();
        let file = self.index.files.get(&unit.input.path);
        for site in file.into_iter().flat_map(|f| &f.call_sites) {
            out.entry((site.caller.qualname.clone(), site.span))
                .or_default()
                .push(site);
        }
        out
    }

    /// Every summarized function once, from its defining file, sorted by key.
    pub fn defined_functions(&self) -> Vec<(&'a FuncKey, &'a EffectSummary)> {
        let mut funcs: Vec<(&FuncKey, &EffectSummary)> = self
            .index
            .files
            .iter()
            .flat_map(|(path, f)| {
                f.functions
                    .iter()
                    .filter(move |(k, _)| self.index.file_of(k) == Some(path.as_path()))
                    .map(|(k, s)| (k, &**s))
            })
            .collect();
        funcs.sort_by(|a, b| a.0.cmp(b.0));
        funcs
    }

    /// Call sites of every summarized function with their file, keyed by caller, from its
    /// defining file and in source order (the IR-free call graph the contract rules walk).
    pub fn sites_by_caller(&self) -> SitesByCaller<'a> {
        self.by_caller(|file| &file.call_sites)
    }

    /// The reference sites of every summarized function (`FileSummary::reference_sites`), as
    /// [`Self::sites_by_caller`] keeps the call sites.
    pub fn reference_sites_by_caller(&self) -> SitesByCaller<'a> {
        self.by_caller(|file| &file.reference_sites)
    }

    fn by_caller(&self, sites: impl Fn(&'a FileSummary) -> &'a [CallSite]) -> SitesByCaller<'a> {
        let mut out = SitesByCaller::default();
        for (path, file) in &self.index.files {
            for site in sites(file) {
                if self.index.file_of(&site.caller) == Some(path.as_path()) {
                    out.entry(&site.caller)
                        .or_default()
                        .push((path.as_path(), site));
                }
            }
        }
        for sites in out.values_mut() {
            sites.sort_by_key(|(_, s)| s.span);
        }
        out
    }

    fn related(&self, chain: &[CallHop]) -> Vec<RelatedLocation> {
        chain
            .iter()
            .map(|hop| {
                self.related_at(
                    &hop.path,
                    hop.span,
                    format!("`{}` calls `{}`", hop.caller.qualname, hop.callee),
                )
            })
            .collect()
    }

    /// A related location at `span` of `path`.
    pub fn related_at(&self, path: &Path, span: Span, message: String) -> RelatedLocation {
        // A file that is not checked: the position comes from its text.
        let (line, column) = self
            .unit(path)
            .map(|u| u.lines.line_col(span.start))
            .or_else(|| {
                let text = (self.text_of?)(path)?;
                Some(LineIndex::from_source(&text).line_col(span.start))
            })
            .unwrap_or((0, 0));
        RelatedLocation {
            path: path.to_path_buf(),
            line,
            column,
            span,
            message,
        }
    }

    /// Build a diagnostic (severity from `lint.severity`, else `RuleMeta::default_severity`);
    /// `None` when `code` does not run on `unit`. `message` is one sentence saying what
    /// happens; what to do about it goes into `help`, never into the message.
    #[allow(clippy::too_many_arguments)]
    pub fn emit<'o>(
        &self,
        out: &'o mut Vec<Diagnostic>,
        code: &'static str,
        unit: &ModuleUnit<'_>,
        span: Span,
        qualname: &str,
        target: String,
        message: String,
        help: Option<String>,
        chain: &[CallHop],
    ) -> Option<&'o mut Diagnostic> {
        if !self.enabled(code, unit) {
            return None;
        }
        let meta = find_rule(code).expect("emitted rules are registered");
        let severity = self
            .config
            .lint
            .severity
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(meta.code) || k.eq_ignore_ascii_case(meta.slug))
            .map(|(_, v)| *v)
            .unwrap_or(meta.default_severity());
        let (line, column) = unit.lines.line_col(span.start);
        out.push(Diagnostic {
            path: unit.input.path.clone(),
            line,
            column,
            span,
            qualname: qualname.to_owned(),
            rule_code: meta.code.to_owned(),
            rule_slug: meta.slug.to_owned(),
            target,
            severity,
            certainty: meta.certainty,
            message,
            help,
            evidence: None,
            blocker: None,
            related: self.related(chain),
            fix: None,
            sites: 1,
            fold_key: None,
        });
        out.last_mut()
    }
}

/// Whether a configured name uses glob syntax (`*`, `?`, `[...]`, `{a,b}`) rather than naming
/// one thing literally.
pub(crate) fn has_glob_syntax(text: &str) -> bool {
    text.contains(['*', '?', '[', '{'])
}

/// Dotted-name pattern: a glob (`pkg.*.Model`, `*Store.*`, `Mode?.fields`) that matches the name
/// or one of its dot-separated suffixes (`Model.fields` matches `pkg.model.Model.fields`).
#[derive(Debug, Clone)]
pub(crate) struct DottedPattern {
    text: String,
    /// `None` for a literal name, compared as a string.
    glob: Option<GlobMatcher>,
}

impl DottedPattern {
    pub fn new(text: &str, context: &'static str) -> Result<Self, RuleConfigError> {
        let glob = Glob::new(text)
            .map_err(|cause| RuleConfigError::Glob {
                pattern: text.to_owned(),
                context,
                cause,
            })?
            .compile_matcher();
        Ok(Self {
            text: text.to_owned(),
            glob: has_glob_syntax(text).then_some(glob),
        })
    }

    /// The configured text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The glob of a pattern with glob syntax (`None`: a literal name).
    pub fn glob(&self) -> Option<&Glob> {
        self.glob.as_ref().map(GlobMatcher::glob)
    }

    pub fn matches(&self, name: &str) -> bool {
        let Some(glob) = &self.glob else {
            return name == self.text
                || name
                    .strip_suffix(self.text.as_str())
                    .is_some_and(|head| head.ends_with('.'));
        };
        glob.is_match(name)
            || name
                .match_indices('.')
                .any(|(i, _)| glob.is_match(&name[i + 1..]))
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod registry_tests {
    use super::*;

    #[test]
    fn dotted_patterns_match_suffixes_and_globs() {
        let p = DottedPattern::new("Model.fields", "test").unwrap();
        assert!(p.matches("pkg.model.Model.fields"));
        assert!(p.matches("Model.fields"));
        assert!(!p.matches("pkg.BigModel.fields"));
        let g = DottedPattern::new("Store.*", "test").unwrap();
        assert!(g.matches("Store.add_field"));
        assert!(g.matches("pkg.store.Store.add_field"));
        assert!(!g.matches("pkg.parser.parse"));
        for (pattern, name) in [
            ("Mode?.fields", "pkg.models.Model.fields"),
            ("Model.[fg]ields", "pkg.models.Model.fields"),
            ("Model.{fields,items}", "pkg.models.Model.items"),
        ] {
            let p = DottedPattern::new(pattern, "test").unwrap();
            assert!(p.matches(name), "{pattern} {name}");
            assert!(!p.matches("pkg.models.Model.other"), "{pattern}");
        }
    }

    #[test]
    fn rules_run_unnamed_by_activation_and_certainty() {
        let rule = |certainty, activation| RuleMeta {
            certainty,
            activation,
            ..*find_rule("EFF102").unwrap()
        };
        use Activation::{Contract, OptIn};
        use Certainty::{Exact, Heuristic};
        let on = Activation::Default;
        // Whether it runs without being named, and whether it shows its heuristic reports when
        // it runs unnamed, both without and with `lint.heuristic`; named, every rule shows them.
        for (certainty, activation, plain, with_heuristic, shows_plain) in [
            (Exact, on, true, true, false),
            (Heuristic, on, false, true, true),
            (Exact, Contract, true, true, true),
            (Heuristic, Contract, true, true, true),
            (Exact, OptIn, false, false, true),
            (Heuristic, OptIn, false, false, true),
        ] {
            let r = rule(certainty, activation);
            assert_eq!(
                (r.runs_unnamed(false), r.runs_unnamed(true)),
                (plain, with_heuristic),
                "{certainty:?} {activation:?}"
            );
            assert_eq!(
                [
                    r.shows_heuristic_reports(false, false),
                    r.shows_heuristic_reports(false, true),
                    r.shows_heuristic_reports(true, false),
                ],
                [shows_plain, true, true],
                "{certainty:?} {activation:?}"
            );
            assert_eq!(
                r.heuristic_note().is_some(),
                certainty == Heuristic,
                "{certainty:?} {activation:?}"
            );
        }
    }

    #[test]
    fn a_rule_is_an_error_for_a_bug_and_a_warning_for_slower_code() {
        for r in CORE_RULES {
            let expected = match r.consequence {
                Consequence::Bug => Severity::Error,
                Consequence::Slowdown => Severity::Warning,
            };
            assert_eq!(r.default_severity(), expected, "{}", r.code);
            // The `OPT` rules report slower code, and every `MUT` rule a bug.
            match r.family {
                RuleFamily::Opt => assert_eq!(r.consequence, Consequence::Slowdown, "{}", r.code),
                RuleFamily::Mut => assert_eq!(r.consequence, Consequence::Bug, "{}", r.code),
                RuleFamily::Eff | RuleFamily::Data => {}
            }
        }
    }

    #[test]
    fn resolves_rules_by_code_or_slug() {
        assert_eq!(find_rule("EFF101").unwrap().slug, "async-blocking-call");
        assert_eq!(find_rule("write-owner-violation").unwrap().code, "MUT201");
        assert!(find_rule("OPT101").unwrap().auto_fix_default);
        assert!(!find_rule("OPT110").unwrap().auto_fix_default);
        // `size_dependent_fix` names exactly the size-dependent Soundness-lane rewrites.
        for r in CORE_RULES {
            let lane = heki_soundness::OptRule::ALL
                .iter()
                .find(|o| o.code() == r.code);
            let expected = lane.is_some_and(|o| {
                o.profitability() == heki_soundness::ProfitabilityTier::SizeDependent
                    && r.code != "OPT103"
            });
            assert_eq!(r.size_dependent_fix, expected, "{}", r.code);
        }
    }

    #[test]
    fn experimental_entries_are_well_formed_and_their_rules_opt_in() {
        use experimental::{EXPERIMENTAL_FEATURES, ExperimentalKind, experimental_rule};
        let mut ids = std::collections::BTreeSet::new();
        for f in EXPERIMENTAL_FEATURES {
            assert!(ids.insert(f.id), "{}: listed twice", f.id);
            let kind = serde_json::to_value(f.kind).unwrap();
            assert!(
                f.id.starts_with(&format!("{}.", kind.as_str().unwrap())),
                "{}",
                f.id
            );
            let version: Vec<_> = f.since.split('.').collect();
            assert!(
                version.len() == 3 && version.iter().all(|n| n.parse::<u32>().is_ok()),
                "{}: since {}",
                f.id,
                f.since
            );
            // `heki rule` and the pages put the reason between other sentences.
            assert!(f.reason.ends_with('.'), "{}", f.id);
        }
        // An experimental rule runs only where a configuration names it.
        for f in EXPERIMENTAL_FEATURES
            .iter()
            .filter(|f| f.kind == ExperimentalKind::Rule)
        {
            let rule = find_rule(f.target).unwrap();
            assert_eq!((rule.code, rule.activation), (f.target, Activation::OptIn));
            assert_eq!(experimental_rule(rule.code), Some(f));
        }
        assert_eq!(experimental_rule("MUT107"), None);
    }
}
