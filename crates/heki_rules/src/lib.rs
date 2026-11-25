//! Rule registry spanning the families of heki's checks.
//!
//! - Family 1 (`MUT`): Mutation & State Effect (`MUT101`, `MUT103`, `MUT106`, `MUT107`, `MUT108`,
//!   `MUT110`, `MUT111`, `MUT112`, `MUT113`, `MUT114`, `MUT115`, `MUT118`, `MUT119`, `MUT120`,
//!   `MUT201`, `MUT202`, `MUT203`, `MUT204`, `MUT205`, `MUT206`, `MUT207`, `MUT208`)
//! - Family 2 (`EFF` / `DATA`): Execution Effect (`EFF101`, `EFF102`, `EFF103`, `EFF105`, `EFF106`,
//!   `EFF107`, `EFF109`, `EFF110`, `EFF112`, `EFF114`, `EFF115`, `EFF116`, `EFF201`, `DATA101`)
//! - Family 3 (`OPT`): Complexity Rewrite (`OPT101`, `OPT102`, `OPT103`, `OPT110`, `OPT111`,
//!   `OPT112`, `OPT113`, `OPT120`, `OPT121`, `OPT130`); `OPT112` / `OPT113` are Advisory-lane
//!   reports without a rewrite.
//! - Family 4 (`IMP`): Imports (`IMP101`, `IMP102`, `IMP104`, `IMP201`, `IMP202`, `IMP301`,
//!   `IMP302`)

pub mod blind_spots;
pub mod census;
mod contracts;
mod data_rules;
mod eff_rules;
pub mod experimental;
mod imp_rules;
pub mod import_view;
pub mod inspect;
mod mut_rules;
mod opt_rules;
mod patterns;
pub mod precision;
pub mod premises;
pub mod project;
pub mod recommend;
pub mod selection;
pub mod snapshot;
pub mod unmatched;

pub use contracts::{
    ContractRoles, MAX_INVALIDATIONS, declares_ownership, import_facts, inspect_facts,
    opt_in_facts, recommend_facts, summary_contracts, typed_writes,
};
pub use eff_rules::{ROOT_CAUSE_GROUP_THRESHOLD, fold_root_causes};
pub use heki_diagnostics::Certainty;
pub use imp_rules::{
    circular_failure_files, dependency_report_files, import_cycle_files, judges_imports,
    walking_importers, walks_imports,
};
pub use mut_rules::{
    CHAIN_RULES, ChainFacts, EXAMPLE_RULES, ExampleOwners, MemberName, SentinelTestKey,
    StaleWriteKey, chain_facts, class_member_names, deep_copy_files, example_digest,
    family_readers, kept_removals_in, lazy_setup_files, reads_class_members, sentinel_tests_in,
    stale_writes_by_file,
};
pub use selection::{
    EffectiveSelectors, RECOMMEND_HINT, RuleSelection, SelectionError, Selectors, heuristic_hint,
};
pub use snapshot::{SNAPSHOT_FILE, SnapshotError, SummarySnapshot};

use globset::{Glob, GlobMatcher};
use heki_ast::{LineIndex, Span};
use heki_diagnostics::{Diagnostic, RelatedLocation};
use heki_effect::code_kind::CodeKind;
use heki_effect::{
    CallHop, CallSite, ContractClass, EffectSummary, FileSummary, FuncKey, ModuleInput,
    ProjectDecls, ProjectIndex,
};
use heki_env::{Docs, Explain, HekiConfig, Problem, Setting, Severity};
use heki_ir::{IrFunction, ModuleConstants};
use heki_kb::{EffectiveEcosystemKb, EffectiveSoundnessKb};
use heki_semantic::mangling;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
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
    Imp,
}

/// What happens in the code a rule reports, which sets its default severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Consequence {
    /// A bug that can be critical: a wrong result, an exception, a lost update, a deadlock, an
    /// event loop held up for every other task, or a broken contract.
    Bug,
    /// A bug that is rarely harmful: a promise the code makes is broken, such as a read-only
    /// annotation, where the callers seldom notice.
    MinorBug,
    /// Slower code that computes the right result.
    Slowdown,
}

/// When a rule runs without being named by its code or name (ADR-018).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
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

impl Activation {
    /// The name `heki rule --output-format json` writes.
    pub const fn word(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::OptIn => "opt-in",
            Self::Contract => "contract",
        }
    }
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
    /// A proven-safe rewrite that `--fix` applies only with `--include-size-dependent`.
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
        heki_env::problem::rule_page(self.slug)
    }

    /// The severity of the rule's diagnostics unless `lint.severity` names the rule: `error` for
    /// a bug that can be critical, `warning` for a bug that is rarely harmful and for slower code.
    pub const fn default_severity(&self) -> Severity {
        match self.consequence {
            Consequence::Bug => Severity::Error,
            Consequence::MinorBug | Consequence::Slowdown => Severity::Warning,
        }
    }
}

/// The certainty of the eight Soundness-lane `OPT` rules (`OPT101`-`OPT103`, `OPT110`, `OPT111`,
/// `OPT120`, `OPT121`, `OPT130`). Their exact tier is the reports whose rewrite is proven safe,
/// the others are heuristic whatever this says (ADR-018, decision 9). They are certified
/// together. Only `OPT101` and `OPT102`, whose rewrite is faster for every input, run by default
/// (ADR-018, decision 13 and its amendment).
const PROVEN_OPT_TIER: Certainty = Certainty::Exact;

/// Core v1.0 rule catalog (empirically validated in Stage 0-A + `dcg` study).
pub const CORE_RULES: &[RuleMeta] = &[
    RuleMeta {
        code: "MUT101",
        slug: "cached-property-stale-mutation",
        family: RuleFamily::Mut,
        certainty: Certainty::Exact,
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
        activation: Activation::OptIn,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Opt-in: a method other than the initializers rebinds or changes in place an attribute that the `__hash__` of its instances reads.",
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
        code: "MUT110",
        slug: "readonly-param-mutation",
        family: RuleFamily::Mut,
        certainty: Certainty::Exact,
        activation: Activation::Default,
        consequence: Consequence::MinorBug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Handing a `Sequence` / `Mapping` / `AbstractSet` / `ReadOnly` parameter, as itself, to a function whose body (or a helper's) changes it in place at a named line.",
        doc: include_str!("../rule-docs/MUT110.md"),
    },
    RuleMeta {
        code: "MUT111",
        slug: "cached-property-stale-read",
        family: RuleFamily::Mut,
        certainty: Certainty::Exact,
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
        code: "MUT114",
        slug: "cached-result-mutation",
        family: RuleFamily::Mut,
        certainty: Certainty::Exact,
        activation: Activation::Default,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Changing in place the list, dict, or set that a `functools.lru_cache` / `functools.cache` function returned (the cache hands the same object to later calls with the same arguments), directly or through the functions that return it.",
        doc: include_str!("../rule-docs/MUT114.md"),
    },
    RuleMeta {
        code: "MUT115",
        slug: "shallow-copy-nested-mutation",
        family: RuleFamily::Mut,
        certainty: Certainty::Heuristic,
        activation: Activation::OptIn,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Opt-in: changing in place an object below a shallow copy the function made of state that outlives the call (`dict(x)`, `x.copy()`, `copy.copy(x)`, `model_copy()`): the copied value holds the same object, so the change is seen through it too.",
        doc: include_str!("../rule-docs/MUT115.md"),
    },
    RuleMeta {
        code: "MUT118",
        slug: "sentinel-lost-in-deepcopy",
        family: RuleFamily::Mut,
        certainty: Certainty::Heuristic,
        activation: Activation::Default,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "`copy.deepcopy` of an instance holding an `object()` sentinel in an attribute that a function tests against the sentinel by identity (the copy holds a new object, which the test does not match).",
        doc: include_str!("../rule-docs/MUT118.md"),
    },
    RuleMeta {
        code: "MUT119",
        slug: "change-lost-on-new-object",
        family: RuleFamily::Mut,
        certainty: Certainty::Exact,
        activation: Activation::Default,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "An in-place change of a builtin container applied to a new object that nothing else holds and no name keeps (`list(xs).sort()`, `env = os.environ.copy().update(...)`, `obj.settings[k] = v` with a getter returning a new dict): the change is lost when the statement ends.",
        doc: include_str!("../rule-docs/MUT119.md"),
    },
    RuleMeta {
        code: "MUT120",
        slug: "readonly-view-mutation",
        family: RuleFamily::Mut,
        certainty: Certainty::Heuristic,
        activation: Activation::OptIn,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Opt-in: changing a value held through a read-only view (a protocol or class whose members are all properties without a setter) where a type checker does not see it: a helper that does not see the view's type rebinds one of its properties, or code changes in place the container a property returns.",
        doc: include_str!("../rule-docs/MUT120.md"),
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
        activation: Activation::Contract,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Mutating a shared object returned by a declared `readonly-returns` producer, directly or through the functions that return it.",
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
        code: "MUT208",
        slug: "write-without-invalidation",
        family: RuleFamily::Mut,
        certainty: Certainty::Heuristic,
        activation: Activation::Contract,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Writing a declared `invalidation` target (directly, via alias, or via helper) without reaching one of its invalidators on every path to the function's return.",
        doc: include_str!("../rule-docs/MUT208.md"),
    },
    RuleMeta {
        code: "EFF101",
        slug: "async-blocking-call",
        family: RuleFamily::Eff,
        certainty: Certainty::Exact,
        activation: Activation::Default,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "A call in `async def` that always runs a network, database, subprocess, or sleep call that waits on the event loop; its heuristic reports, a call that can reach a blocking call.",
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
        certainty: Certainty::Exact,
        activation: Activation::Default,
        consequence: Consequence::Slowdown,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "A loop over data whose every iteration sends a database statement; its heuristic reports, a loop that can run a database query on every iteration.",
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
        code: "EFF109",
        slug: "lock-reacquired-while-held",
        family: RuleFamily::Eff,
        certainty: Certainty::Heuristic,
        activation: Activation::OptIn,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Opt-in: a non-reentrant lock (`threading.Lock`, `asyncio.Lock`) taken again, directly or through a call, while it is held (the thread or task waits for itself).",
        doc: include_str!("../rule-docs/EFF109.md"),
    },
    RuleMeta {
        code: "EFF110",
        slug: "exclusive-object-fanout",
        family: RuleFamily::Eff,
        certainty: Certainty::Heuristic,
        activation: Activation::OptIn,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Opt-in: an object allowing one operation at a time (`asyncio.StreamReader`, an asyncpg connection, a WebSocket) used by tasks started together (`asyncio.gather`, a task group).",
        doc: include_str!("../rule-docs/EFF110.md"),
    },
    RuleMeta {
        code: "EFF112",
        slug: "set-order-output",
        family: RuleFamily::Eff,
        certainty: Certainty::Heuristic,
        activation: Activation::OptIn,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Opt-in: the iteration order of a set of strings, which changes between runs, reaches a file, a stream, a serializer, or a raised exception's message across a call.",
        doc: include_str!("../rule-docs/EFF112.md"),
    },
    RuleMeta {
        code: "EFF114",
        slug: "recursion-error-as-value",
        family: RuleFamily::Eff,
        certainty: Certainty::Heuristic,
        activation: Activation::OptIn,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Opt-in: an `except` clause around a recursion (a call cycle reached from its `try` body) takes the `RecursionError` and goes on without raising, so the result depends on the stack depth.",
        doc: include_str!("../rule-docs/EFF114.md"),
    },
    RuleMeta {
        code: "EFF115",
        slug: "async-def-as-sync-callback",
        family: RuleFamily::Eff,
        certainty: Certainty::Heuristic,
        activation: Activation::OptIn,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Opt-in: an `async def` handed to a library call that calls it without awaiting what it returns (a thread's target, an executor's function, an `atexit` or signal handler, a `filter` predicate), so the call only creates a coroutine and the body does not run.",
        doc: include_str!("../rule-docs/EFF115.md"),
    },
    RuleMeta {
        code: "EFF116",
        slug: "generator-never-iterated",
        family: RuleFamily::Eff,
        certainty: Certainty::Heuristic,
        activation: Activation::OptIn,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Opt-in: a call of a generator function whose generator nothing iterates (a statement, a value only tested for truth, a function handed to a thread, an `atexit` handler, or a `filter` predicate), so its body does not run.",
        doc: include_str!("../rule-docs/EFF116.md"),
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
        certainty: Certainty::Exact,
        activation: Activation::Default,
        consequence: Consequence::Slowdown,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Loop-carried quadratic `pandas.concat` / `numpy.vstack` / `numpy.append` accumulation.",
        doc: include_str!("../rule-docs/DATA101.md"),
    },
    RuleMeta {
        code: "IMP101",
        slug: "import-cycle",
        family: RuleFamily::Imp,
        certainty: Certainty::Heuristic,
        activation: Activation::OptIn,
        consequence: Consequence::MinorBug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Opt-in: project modules that import each other while they load (an import cycle), reported once per group at the import closing a shortest cycle.",
        doc: include_str!("../rule-docs/IMP101.md"),
    },
    RuleMeta {
        code: "IMP102",
        slug: "circular-import-failure",
        family: RuleFamily::Imp,
        certainty: Certainty::Heuristic,
        activation: Activation::OptIn,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Opt-in: a `from M import name` that raises `ImportError` when a module of its import cycle is imported before the others, because `M` is still loading and has not bound `name`.",
        doc: include_str!("../rule-docs/IMP102.md"),
    },
    RuleMeta {
        code: "IMP104",
        slug: "development-code-import",
        family: RuleFamily::Imp,
        certainty: Certainty::Heuristic,
        activation: Activation::OptIn,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Opt-in: a module of the package the project distributes imports test code, or a module outside the source roots of a src layout, by an import that runs.",
        doc: include_str!("../rule-docs/IMP104.md"),
    },
    RuleMeta {
        code: "IMP201",
        slug: "import-boundary-violation",
        family: RuleFamily::Imp,
        certainty: Certainty::Heuristic,
        activation: Activation::Contract,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "An import that an `import-boundaries` contract does not allow: a forbidden module, or one outside the allowed imports.",
        doc: include_str!("../rule-docs/IMP201.md"),
    },
    RuleMeta {
        code: "IMP202",
        slug: "unresolved-guarded-import",
        family: RuleFamily::Imp,
        certainty: Certainty::Heuristic,
        activation: Activation::Contract,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Strict mode of an `import-boundaries` contract (`unresolved = \"report\"`): an import whose module heki cannot name, which may be one the contract does not allow.",
        doc: include_str!("../rule-docs/IMP202.md"),
    },
    RuleMeta {
        code: "IMP301",
        slug: "development-dependency-at-import-time",
        family: RuleFamily::Imp,
        certainty: Certainty::Heuristic,
        activation: Activation::OptIn,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Opt-in: a package module loads, while it loads, a module whose distribution the project declares only for development (a dependency group), not in its dependencies or extras.",
        doc: include_str!("../rule-docs/IMP301.md"),
    },
    RuleMeta {
        code: "IMP302",
        slug: "optional-dependency-at-import-time",
        family: RuleFamily::Imp,
        certainty: Certainty::Heuristic,
        activation: Activation::OptIn,
        consequence: Consequence::Bug,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Opt-in: importing a top-level package or an entry point's module loads, while it loads, a module whose distribution the project declares only in extras, so the import fails without them.",
        doc: include_str!("../rule-docs/IMP302.md"),
    },
    RuleMeta {
        code: "OPT101",
        slug: "list-quadratic-concat",
        family: RuleFamily::Opt,
        certainty: PROVEN_OPT_TIER,
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
        certainty: PROVEN_OPT_TIER,
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
        certainty: PROVEN_OPT_TIER,
        activation: Activation::OptIn,
        consequence: Consequence::Slowdown,
        size_dependent_fix: false,
        auto_fix_default: false,
        summary: "Opt-in: size-dependent immutable quadratic accumulation (`bytes`/`tuple`/attr `str`).",
        doc: include_str!("../rule-docs/OPT103.md"),
    },
    RuleMeta {
        code: "OPT110",
        slug: "loop-linear-membership",
        family: RuleFamily::Opt,
        certainty: PROVEN_OPT_TIER,
        activation: Activation::OptIn,
        consequence: Consequence::Slowdown,
        size_dependent_fix: true,
        auto_fix_default: false,
        summary: "Opt-in: size-dependent `e in lst` inside a loop -> hoist `set` outside loop.",
        doc: include_str!("../rule-docs/OPT110.md"),
    },
    RuleMeta {
        code: "OPT111",
        slug: "loop-linear-dedup",
        family: RuleFamily::Opt,
        certainty: PROVEN_OPT_TIER,
        activation: Activation::OptIn,
        consequence: Consequence::Slowdown,
        size_dependent_fix: true,
        auto_fix_default: false,
        summary: "Opt-in: size-dependent `if v not in seen: seen.append(v)` -> auxiliary `set`.",
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
        certainty: Certainty::Exact,
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
        certainty: PROVEN_OPT_TIER,
        activation: Activation::OptIn,
        consequence: Consequence::Slowdown,
        size_dependent_fix: true,
        auto_fix_default: false,
        summary: "Opt-in: size-dependent `q.pop(0)` / `del q[0]` / `q = q[1:]` -> `collections.deque`.",
        doc: include_str!("../rule-docs/OPT120.md"),
    },
    RuleMeta {
        code: "OPT121",
        slug: "list-front-insert",
        family: RuleFamily::Opt,
        certainty: PROVEN_OPT_TIER,
        activation: Activation::OptIn,
        consequence: Consequence::Slowdown,
        size_dependent_fix: true,
        auto_fix_default: false,
        summary: "Opt-in: size-dependent `q.insert(0, v)` -> `collections.deque`.",
        doc: include_str!("../rule-docs/OPT121.md"),
    },
    RuleMeta {
        code: "OPT130",
        slug: "loop-invariant-container-rebuild",
        family: RuleFamily::Opt,
        certainty: PROVEN_OPT_TIER,
        activation: Activation::OptIn,
        consequence: Consequence::Slowdown,
        size_dependent_fix: true,
        auto_fix_default: false,
        summary: "Opt-in: size-dependent loop-invariant container reconstruction (`e in set(keys)`).",
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
        values: &'static [&'static str],
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
    #[error(
        "effect-boundaries `{0}` sets `import-probes = \"report\"` but does not forbid `reads-environment`"
    )]
    ImportProbesWithoutEnvironment(String),
    /// A mode key of a contract (`fresh-objects`, `unresolved`) with an unknown value.
    #[error("unknown `{key}` value `{value}` in {section} `{contract}` (expected {expected})")]
    UnknownMode {
        section: &'static str,
        contract: String,
        key: &'static str,
        value: String,
        expected: &'static str,
        values: &'static [&'static str],
    },
    /// A module pattern of an import contract naming a group heki does not know.
    #[error(
        "unknown group `{pattern}` in {context} (expected `<stdlib>`, `<project>`, or `<third-party>`)"
    )]
    ImportGroup {
        pattern: String,
        context: &'static str,
    },
    /// A group in `relative-imports`: a relative import reaches only modules of the importer's
    /// own top-level package, which no group names.
    #[error(
        "group `{pattern}` in {context}: a relative import reaches only the modules of the importer's own top-level package (expected a module or a glob)"
    )]
    RelativeGroup {
        pattern: String,
        context: &'static str,
    },
    /// A value of a key outside the contracts (`imports.cycle-edges`) that the key does not take.
    #[error("unknown `{key}` value `{value}` in {table} (expected {expected})")]
    UnknownValue {
        table: &'static str,
        key: &'static str,
        value: String,
        expected: &'static str,
        values: &'static [&'static str],
    },
    /// An `ignored-imports` entry that is not `importer -> imported`.
    #[error("invalid pair `{pattern}` in {context} (expected `\"importer -> imported\"`)")]
    ImportPair {
        pattern: String,
        context: &'static str,
    },
    #[error("invalid glob `{pattern}` in {context}: {cause}")]
    Glob {
        pattern: String,
        context: &'static str,
        cause: globset::Error,
    },
    /// An entry naming `<external>` other than as `<external>.<name>`.
    #[error(
        "invalid {context} entry `{target}`: `<external>` stands for every module and class outside the project, followed by one name (`<external>._*`)"
    )]
    ExternalMembers {
        target: String,
        context: &'static str,
    },
    /// A `module:qualname` pattern with more than one colon or an empty part.
    #[error(
        "invalid pattern `{pattern}` in {context} (expected `module:qualname`, one colon with a name on each side)"
    )]
    ModulePattern {
        pattern: String,
        context: &'static str,
    },
    /// A call target's argument list (`DataType(is_dict=True)`) heki cannot read.
    #[error(
        "invalid argument list in {context} target `{target}`: {reason} (expected `name(parameter=value, ...)` with `True`, `False`, `None`, an integer, or a quoted string)"
    )]
    InvalidArguments {
        target: String,
        context: &'static str,
        reason: &'static str,
    },
    #[error("the globs of {context} are too many to match together: {cause}")]
    GlobSet {
        context: &'static str,
        cause: globset::Error,
    },
    #[error("{0} `invalidation` contracts declared; at most {MAX_INVALIDATIONS} are supported")]
    TooManyInvalidations(usize),
}

impl Explain for RuleConfigError {
    fn explain(&self) -> Problem {
        let problem = Problem::new(self.to_string());
        let in_table = |table: &str, entry: &str, key: Option<&str>, value: Option<&str>| Setting {
            table: table.to_owned(),
            entry: (!entry.is_empty()).then(|| entry.to_owned()),
            key: key.map(str::to_owned),
            value: value.map(str::to_owned),
        };
        let near = |word: &str, values: &'static [&'static str]| {
            heki_env::problem::nearest(word, values.iter().copied())
                .into_iter()
                .map(str::to_owned)
                .collect()
        };
        let boundary = |name: &str, key: &str, value: Option<&str>| {
            in_table("effect-boundaries", name, Some(key), value)
        };
        let (problem, setting) = match self {
            Self::InvalidTarget { section, target } => (
                problem.help("write a target as a class and its attribute: `\"pkg.model.DataModel.fields\"`, `\"DataModel.*\"`"),
                in_table(section, "", None, Some(target)),
            ),
            Self::ItemTarget { section, target } => (
                problem.help("a read target is `Class.attr`: drop the `[\"key\"]`, or make the entry a `write-owners` target"),
                in_table(section, "", None, Some(target)),
            ),
            Self::UnknownForbid { boundary: name, token, values, .. } => (
                problem.did_you_mean(near(token, values)),
                boundary(name, "forbid", Some(token)),
            ),
            Self::MissingTargets { boundary: name, token, key } => (
                problem.help(format!("add `{key}` to the boundary, or take `{token}` out of `forbid`")),
                boundary(name, "forbid", Some(token)),
            ),
            Self::AllowWritesWithoutUniverse(name) => (
                problem.help("add `write-universe`, the attributes whose writes the boundary judges; `allow-writes` lists the ones of them it allows"),
                boundary(name, "allow-writes", None),
            ),
            Self::ImportProbesWithoutEnvironment(name) => (
                problem.help("add `reads-environment` to `forbid`, or remove `import-probes`"),
                boundary(name, "import-probes", None),
            ),
            Self::UnknownMode { section, contract, key, value, values, .. } => (
                problem.did_you_mean(near(value, values)),
                in_table(section, contract, Some(key), Some(value)),
            ),
            Self::UnknownValue { table, key, value, values, .. } => (
                problem.did_you_mean(near(value, values)),
                in_table(table, "", Some(key), Some(value)),
            ),
            Self::Glob { pattern, context, .. } => (
                problem.help("a pattern is a dotted name with `*`, `?`, or `[...]`: `\"pkg.parser.*\"`, `\"*.render\"`"),
                Setting::of_context(context, Some(pattern)),
            ),
            Self::ImportGroup { pattern, context } => (
                problem.help("name a module, a glob, or one of the groups `<stdlib>`, `<project>`, and `<third-party>`"),
                Setting::of_context(context, Some(pattern)),
            ),
            Self::RelativeGroup { pattern, context } => (
                problem.help("name the modules of the package: `\"pkg._runtime\"`, `\"pkg._runtime.*\"`"),
                Setting::of_context(context, Some(pattern)),
            ),
            Self::ImportPair { pattern, context } => (
                problem.help("write the importing module, an arrow, and the imported one: `\"pkg.cli -> pkg.db\"`"),
                Setting::of_context(context, Some(pattern)),
            ),
            Self::ExternalMembers { target, context } => (
                problem.help("write `<external>.` and the last name of the symbols, a glob or a name: `\"<external>._*\"`"),
                Setting::of_context(context, Some(target)),
            ),
            Self::ModulePattern { pattern, context } => (
                problem.help("write the module, one colon, and the name in it: `\"pkg.db:Session.commit\"`"),
                Setting::of_context(context, Some(pattern)),
            ),
            Self::InvalidArguments { target, context, .. } => (
                problem.help("write the arguments as `name(parameter=value)`: `\"DataType(is_dict=True)\"`"),
                Setting::of_context(context, Some(target)),
            ),
            Self::GlobSet { context, .. } => (
                problem.help("write fewer patterns: a pattern with `*` covers many names"),
                Setting::of_context(context, None),
            ),
            Self::TooManyInvalidations(_) => (
                problem.help("merge the contracts whose targets the same functions invalidate"),
                in_table("invalidation", "", Some("name"), None),
            ),
        };
        let docs = Docs::of_table(&setting.table);
        problem.docs(docs).setting(setting)
    }
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
    pub text_of: Option<&'a TextOf<'a>>,
    /// Record on each heuristic report of a claim with a premise row which premises it left
    /// open and why (`Diagnostic::open_records`, [`premises::OpenCause`]), and which conditions
    /// of the exact tier that are no premise ([`premises::ConditionCause`]). Off, nothing more
    /// is computed than the tiers need.
    pub open_premises: bool,
    /// Where the project's files are, which decides the kind of code of each module
    /// ([`heki_effect::code_kind`]).
    pub layout: ProjectLayout<'a>,
}

/// The project root and how module names derive from the paths below it.
#[derive(Debug, Clone, Copy)]
pub struct ProjectLayout<'a> {
    pub root: &'a Path,
    pub modules: &'a heki_effect::resolver::ModuleLayout,
}

impl ProjectLayout<'_> {
    /// The kind of code of the project module `module` in the file `path`, under `config`.
    pub fn code_kind(&self, path: &Path, module: &str, config: &HekiConfig) -> CodeKind {
        let rel = project::rel_posix(path, self.root);
        heki_effect::code_kind::code_kind(&rel, module, self.modules, config)
    }
}

/// Reads the text of a file by path.
pub type TextOf<'a> = dyn Fn(&Path) -> Option<String> + Sync + 'a;

/// `caller -> [(defining file, call site)]` (see [`CheckContext::sites_by_caller`]).
pub(crate) type SitesByCaller<'a> = FxHashMap<&'a FuncKey, Vec<(&'a Path, &'a CallSite)>>;

/// The call sites of one module by caller qualname and span ([`CheckContext::sites_by_span`]):
/// a function's sites and the sites at one of its instructions are looked up, never scanned.
#[derive(Default)]
pub(crate) struct SitesBySpan<'a>(FxHashMap<&'a str, FxHashMap<Span, Vec<&'a CallSite>>>);

impl<'a> SitesBySpan<'a> {
    /// The call sites of the function `qualname`.
    pub(crate) fn of(&self, qualname: &str) -> FnSites<'_, 'a> {
        FnSites(self.0.get(qualname))
    }

    /// The call sites of `qualname` at `span`, in source order.
    pub(crate) fn at(&self, qualname: &str, span: Span) -> &[&'a CallSite] {
        self.of(qualname).at(span)
    }

    /// Every call site of the module.
    pub(crate) fn all(&self) -> impl Iterator<Item = &'a CallSite> + '_ {
        self.0
            .values()
            .flat_map(|by_span| by_span.values().flatten().copied())
    }
}

/// The call sites of one function by span ([`SitesBySpan::of`]).
#[derive(Clone, Copy)]
pub(crate) struct FnSites<'s, 'a>(Option<&'s FxHashMap<Span, Vec<&'a CallSite>>>);

impl<'s, 'a> FnSites<'s, 'a> {
    /// The sites at `span`, in source order.
    pub(crate) fn at(self, span: Span) -> &'s [&'a CallSite] {
        self.0
            .and_then(|by_span| by_span.get(&span))
            .map_or(&[], Vec::as_slice)
    }

    /// Every site of the function.
    pub(crate) fn all(self) -> impl Iterator<Item = &'a CallSite> + 's {
        self.0
            .into_iter()
            .flat_map(|by_span| by_span.values().flatten().copied())
    }
}

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
    text_of: Option<&'a TextOf<'a>>,
    /// [`CheckExtras::open_premises`].
    pub open_premises: bool,
    /// Why must-facts do not hold, shared by the rules that record open premises (built only
    /// when the run records them).
    pub must_explainer: Option<heki_effect::MustExplainer<'a>>,
    /// [`CheckExtras::layout`]; `None` in a context over the declarations alone, which runs no
    /// rule.
    layout: Option<ProjectLayout<'a>>,
}

/// Whether [`check_project`] runs the rule `code` on the file at the project-relative `rel_path`:
/// `selection` enables it there, and `MUT103` also needs the summary snapshot it compares with
/// (`snapshot`).
pub fn runs_on(selection: &RuleSelection, snapshot: bool, code: &str, rel_path: &str) -> bool {
    selection.is_enabled_for(code, rel_path) && (code != "MUT103" || snapshot)
}

/// Whether `--fix` applies the fix of `d` without options, and the language server offers it as
/// the preferred quick fix: a `safe` + `unconditional` rule's, or a suppression report's.
pub fn fixes_unconditionally(d: &Diagnostic) -> bool {
    project::is_suppression_report(&d.rule_code)
        || find_rule(&d.rule_code).is_some_and(|m| m.auto_fix_default)
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
        open_premises: extras.open_premises,
        must_explainer: extras
            .open_premises
            .then(|| heki_effect::MustExplainer::new(index)),
        layout: Some(extras.layout),
    };
    let mut out = Vec::new();
    mut_rules::mut101_mut111(&cx, &mut out);
    mut_rules::mut107(&cx, &mut out);
    mut_rules::mut113(&cx, &mut out);
    mut_rules::readonly_rules(&cx, &mut out);
    mut_rules::mut112(&cx, &mut out);
    mut_rules::mut118(&cx, &mut out);
    mut_rules::mut208(&cx, &mut out)?;
    mut_rules::mut201(&cx, &mut out)?;
    mut_rules::mut203(&cx, &mut out)?;
    mut_rules::mut204(&cx, &mut out)?;
    imp_rules::imp101(&cx, &mut out)?;
    imp_rules::imp102(&cx, &mut out);
    imp_rules::imp104(&cx, &mut out);
    imp_rules::imp301_imp302(&cx, &mut out);
    imp_rules::imp201_imp202(&cx, &mut out)?;
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
            mut_rules::mut119(&cx, u, &mut d);
            eff_rules::eff102_103(&cx, u, &mut d);
            eff_rules::eff106(&cx, u, &mut d);
            eff_rules::eff107(&cx, u, &mut d);
            eff_rules::eff109(&cx, u, &mut d);
            eff_rules::eff110(&cx, u, &mut d);
            eff_rules::eff112(&cx, u, &mut d);
            eff_rules::eff114(&cx, u, &mut d);
            eff_rules::eff115(&cx, u, &mut d);
            eff_rules::eff116(&cx, u, &mut d);
            data_rules::data101(&cx, u, &mut d);
            opt_rules::opt(&cx, u, &opt_opts, &mut d);
            opt_rules::opt112(&cx, u, &mut d);
            opt_rules::opt113(&cx, u, &mut d);
            d
        })
        .collect();
    out.extend(per_unit.into_iter().flatten());
    heki_diagnostics::sort_diagnostics(&mut out);
    // One report per access, rule, and target: the one kept stands for the contracts of both.
    out.dedup_by(|a, b| {
        let same = a.path == b.path
            && a.span == b.span
            && a.rule_code == b.rule_code
            && a.target == b.target;
        if same && !a.contracts.is_empty() {
            b.contracts.append(&mut a.contracts);
            b.contracts.sort_unstable();
            b.contracts.dedup();
        }
        same
    });
    Ok(out)
}

/// The matchers of the contracts of `config`, for the roles functions have in them and in the
/// ecosystem catalog (`heki inspect`), compiled as the rules compile them.
pub fn contract_roles<'a>(
    config: &'a HekiConfig,
    kb: &'a EffectiveEcosystemKb,
    soundness_kb: &'a EffectiveSoundnessKb,
    index: &'a ProjectIndex,
    decls: &'a ProjectDecls,
    selection: &'a RuleSelection,
) -> Result<ContractRoles<'a>, RuleConfigError> {
    let cx = CheckContext::of_declarations(config, kb, soundness_kb, index, decls, selection);
    ContractRoles::new(&cx)
}

/// The entries of the contracts of `env`'s configuration that match nothing in the files of
/// `index` (`heki check`, `heki recommend`).
pub fn unmatched_entries(
    env: &project::ProjectEnv,
    index: &ProjectIndex,
    selection: &RuleSelection,
) -> Result<Vec<unmatched::UnmatchedEntry>, RuleConfigError> {
    let decls = index.project_decls();
    let (config, kb, soundness) = (&env.cfg, &env.kb, &env.soundness_kb);
    let cx = CheckContext::of_declarations(config, kb, soundness, index, &decls, selection);
    unmatched::unmatched_entries(&cx)
}

/// What the contracts of `env`'s configuration already declare (`heki recommend`).
pub(crate) fn declared_contracts(
    env: &project::ProjectEnv,
    index: &ProjectIndex,
    selection: &RuleSelection,
) -> Result<contracts::Declared, RuleConfigError> {
    let decls = index.project_decls();
    let (config, kb, soundness) = (&env.cfg, &env.kb, &env.soundness_kb);
    let cx = CheckContext::of_declarations(config, kb, soundness, index, &decls, selection);
    contracts::Declared::of(&cx)
}

impl<'a> CheckContext<'a> {
    /// A context over the project's declarations and summaries, with no unit to check: what the
    /// rules' matchers compile from the configuration.
    fn of_declarations(
        config: &'a HekiConfig,
        kb: &'a EffectiveEcosystemKb,
        soundness: &'a EffectiveSoundnessKb,
        index: &'a ProjectIndex,
        decls: &'a ProjectDecls,
        selection: &'a RuleSelection,
    ) -> Self {
        Self {
            config,
            kb,
            soundness,
            index,
            decls,
            units: &[],
            selection,
            unit_by_path: FxHashMap::default(),
            text_of: None,
            open_premises: false,
            must_explainer: None,
            layout: None,
        }
    }

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

    /// The kind of code of the project module `module`, defined in the file `path`.
    pub fn code_kind(&self, path: &Path, module: &str) -> CodeKind {
        let layout = self
            .layout
            .expect("the rules run with the project's layout");
        match self.unit(path) {
            Some(unit) => heki_effect::code_kind::code_kind(
                &unit.rel_path,
                module,
                layout.modules,
                self.config,
            ),
            None => layout.code_kind(path, module, self.config),
        }
    }

    /// The project-relative POSIX path of the file `path`.
    pub fn rel_path(&self, path: &Path) -> String {
        let layout = self
            .layout
            .expect("the rules run with the project's layout");
        match self.unit(path) {
            Some(unit) => unit.rel_path.clone(),
            None => project::rel_posix(path, layout.root),
        }
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

    /// Call sites of `unit` by caller qualname and span.
    pub(crate) fn sites_by_span(&self, unit: &ModuleUnit<'_>) -> SitesBySpan<'a> {
        let mut out = SitesBySpan::default();
        let file = self.index.files.get(&unit.input.path);
        for site in file.into_iter().flat_map(|f| &f.call_sites) {
            out.0
                .entry(site.caller.qualname.as_str())
                .or_default()
                .entry(site.span)
                .or_default()
                .push(site);
        }
        out
    }

    /// What `func` accesses outside the project that a contract names, in its own body and in
    /// the class bodies it runs ([`heki_effect::FileSummary::external_accesses`]), with the
    /// file defining it.
    pub(crate) fn external_accesses<'f>(
        &self,
        func: &'f FuncKey,
    ) -> impl Iterator<Item = (&'a Path, &'a heki_effect::ExternalAccess)> + use<'a, 'f> {
        let index = self.index;
        index.file_of(func).into_iter().flat_map(move |path| {
            let own = index.files[path].external_accesses.get(func);
            own.into_iter().flatten().map(move |a| (path, a))
        })
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

    /// The project functions some code hands on as values (a reference site of one that is no
    /// read only: an argument, a stored attribute, item, or global, a display a module binds to
    /// a name, a `return`; `FileSummary::reference_sites`): they have callers heki does not see.
    pub fn escaping_functions(&self) -> FxHashSet<FuncKey> {
        self.index
            .files
            .values()
            .flat_map(|file| &file.reference_sites)
            .filter(|site| !site.is_read)
            .flat_map(|site| site.project_callees())
            .map(|bc| bc.callee.clone())
            .collect()
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
        chain.iter().map(|hop| self.hop_location(hop)).collect()
    }

    /// The related location of a call chain's hop: where its caller calls its callee.
    pub fn hop_location(&self, hop: &CallHop) -> RelatedLocation {
        self.related_at(
            &hop.path,
            hop.span,
            format!("`{}` calls `{}`", hop.caller.qualname, hop.callee),
        )
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
            open_records: Vec::new(),
            contracts: Vec::new(),
            allowed_examples: Vec::new(),
        });
        out.last_mut()
    }
}

/// Whether a configured name uses glob syntax (`*`, `?`, `[...]`, `{a,b}`) rather than naming
/// one thing literally.
pub(crate) fn has_glob_syntax(text: &str) -> bool {
    text.contains(['*', '?', '[', '{'])
}

/// The characters glob syntax gives a meaning: a name part holding one is compiled as a glob.
const GLOB_CHARACTERS: [char; 7] = ['*', '?', '[', ']', '{', '}', '\\'];

/// Whether the module part of a [`DottedPattern`] can name the module `module` literally: a `:`
/// would end the part, and a character glob syntax gives a meaning would make the pattern match
/// other modules (`notes[1]` matches `notes1`) or fail to parse. A file name gives a module
/// such characters (`[id].py`, `release:notes.py`).
pub(crate) fn names_module_literally(module: &str) -> bool {
    !module.contains(':') && !module.contains(GLOB_CHARACTERS)
}

/// One part of a [`DottedPattern`]: a literal name or a glob.
#[derive(Debug, Clone)]
pub(crate) struct NamePart {
    text: String,
    /// `None` for a literal name, compared as a string.
    glob: Option<GlobMatcher>,
}

impl NamePart {
    /// A bad glob is reported with the configured `pattern` the part belongs to.
    fn new(text: &str, pattern: &str, context: &'static str) -> Result<Self, RuleConfigError> {
        // Building a matcher compiles a regex: a plain name is compared as a string, and only text
        // with a character the glob syntax gives a meaning is compiled (and so validated).
        if !text.contains(GLOB_CHARACTERS) {
            return Ok(Self {
                text: text.to_owned(),
                glob: None,
            });
        }
        let glob = Glob::new(text)
            .map_err(|cause| RuleConfigError::Glob {
                pattern: pattern.to_owned(),
                context,
                cause,
            })?
            .compile_matcher();
        Ok(Self {
            text: text.to_owned(),
            glob: has_glob_syntax(text).then_some(glob),
        })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// The glob of a part with glob syntax (`None`: a literal name, [`Self::text`]).
    pub fn glob(&self) -> Option<&Glob> {
        self.glob.as_ref().map(GlobMatcher::glob)
    }

    fn is_match(&self, name: &str) -> bool {
        self.glob
            .as_ref()
            .map_or(name == self.text, |glob| glob.is_match(name))
    }

    /// Whether the part names `name` or one of its dot-separated suffixes.
    fn matches_suffix(&self, name: &str) -> bool {
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

/// Dotted-name pattern: a glob (`pkg.*.Model`, `*Store.*`, `Mode?.fields`) that matches the name
/// or one of its dot-separated suffixes (`Model.fields` matches `pkg.model.Model.fields`). A
/// pattern with a module part (`pkg:*`, `pkg.model:Model`) names definitions of the project
/// modules it names: the module part is compared with the whole module name and the qualname
/// part with the whole qualname, so `pkg:*` leaves out the submodules of `pkg`, which a suffix or
/// a glob spanning dots would take in.
#[derive(Debug, Clone)]
pub(crate) struct DottedPattern {
    module: Option<NamePart>,
    /// The qualname part, or the whole text of a pattern without a module part.
    name: NamePart,
}

impl DottedPattern {
    pub fn new(text: &str, context: &'static str) -> Result<Self, RuleConfigError> {
        let (module, name) = match text.split_once(':') {
            None => (None, text),
            Some((module, qualname)) => {
                if module.is_empty() || qualname.is_empty() || qualname.contains(':') {
                    return Err(RuleConfigError::ModulePattern {
                        pattern: text.to_owned(),
                        context,
                    });
                }
                (Some(NamePart::new(module, text, context)?), qualname)
            }
        };
        Ok(Self {
            module,
            name: NamePart::new(name, text, context)?,
        })
    }

    /// The module part (`None`: a pattern without one).
    pub fn module(&self) -> Option<&NamePart> {
        self.module.as_ref()
    }

    /// The qualname part, or the whole text of a pattern without a module part.
    pub fn name(&self) -> &NamePart {
        &self.name
    }

    /// Whether the pattern's text names a module body (`<module>`: `pkg.gen:<module>`,
    /// `pkg.*:<module>`, `pkg.gen.<module>`). A glob that only matches it (`pkg:*`) does not.
    pub fn names_module_bodies(&self) -> bool {
        self.name.text.contains("<module>")
    }

    /// Whether the pattern names `name`, which heki knows only as a whole (an attribute, an
    /// external symbol), or one of its dotted suffixes. A pattern with a module part names
    /// project definitions only.
    pub fn matches(&self, name: &str) -> bool {
        self.module.is_none() && self.name.matches_suffix(name)
    }

    /// Whether the pattern names all of `name`, a symbol or attribute outside the project known
    /// by its full dotted name (`os.getenv`, `sys.platform`), never one of its suffixes. A
    /// pattern with a module part names project definitions only.
    pub fn matches_whole(&self, name: &str) -> bool {
        self.module.is_none() && self.name.is_match(name)
    }

    /// Whether the pattern names a project function or class (`fqn` is `module.qualname`), or an
    /// external symbol (`fqn` and `qualname` both the symbol). Without a module part, the
    /// dotted suffixes of `fqn` include the qualname.
    ///
    /// A class member is named either way it is spelled ([`def_spellings`]): as written in its
    /// class (`Parser.__run`) or as the class holds it (`Parser._Parser__run`).
    pub fn matches_def(&self, fqn: &str, qualname: &str) -> bool {
        def_spellings(fqn, qualname).any(|(fqn, qualname)| match &self.module {
            None => self.name.matches_suffix(&fqn),
            Some(module) => {
                module_of(&fqn, &qualname).is_some_and(|m| module.is_match(m))
                    && self.name.is_match(&qualname)
            }
        })
    }

    /// [`Self::matches_def`] of a class (by its fqn alone without its qualname).
    pub fn matches_class(&self, class: &ContractClass) -> bool {
        self.matches_def(&class.fqn, class.qualname.as_deref().unwrap_or(&class.fqn))
    }

    /// Whether the pattern names the definition ([`Self::matches_def`]) or a module, class, or
    /// function containing it.
    pub fn names_within(&self, fqn: &str, qualname: &str) -> bool {
        def_spellings(fqn, qualname).any(|(fqn, qualname)| match &self.module {
            None => with_prefixes(&fqn).any(|n| self.name.matches_suffix(n)),
            Some(module) => {
                module_of(&fqn, &qualname).is_some_and(|m| module.is_match(m))
                    && with_prefixes(&qualname).any(|q| self.name.is_match(q))
            }
        })
    }
}

/// `(fqn, qualname)` of a project definition or attribute, then the same path with its class
/// members spelled the other way ([`mangling::member_path`] of a qualname, which writes them
/// as in the class, [`mangling::source_path`] of an attribute path, which has them as the class
/// holds them), when that differs. An external symbol (`fqn` is `qualname`) has one spelling.
pub(crate) fn def_spellings<'f, 'q>(
    fqn: &'f str,
    qualname: &'q str,
) -> impl Iterator<Item = (Cow<'f, str>, Cow<'q, str>)> {
    let other = module_of(fqn, qualname).and_then(|module| {
        let respelled =
            mangling::member_path(qualname).or_else(|| mangling::source_path(qualname))?;
        Some((
            Cow::Owned(format!("{module}.{respelled}")),
            Cow::Owned(respelled),
        ))
    });
    std::iter::once((Cow::Borrowed(fqn), Cow::Borrowed(qualname))).chain(other)
}

/// The module of a project definition (`fqn` is `module.qualname`); `None` for an external
/// symbol (`fqn` is `qualname`).
pub(crate) fn module_of<'n>(fqn: &'n str, qualname: &str) -> Option<&'n str> {
    fqn.strip_suffix(qualname)?.strip_suffix('.')
}

/// `name` and each of its dotted prefixes (`pkg.mod.f`, then `pkg` and `pkg.mod`).
fn with_prefixes(name: &str) -> impl Iterator<Item = &str> {
    std::iter::once(name).chain(name.match_indices('.').map(|(i, _)| &name[..i]))
}

/// A configured name split at its last dot after the module part (`pkg.model:Model` and `fields`
/// of `pkg.model:Model.fields`); `None` when its qualified name has no dot.
pub(crate) fn split_last_dot(text: &str) -> Option<(&str, &str)> {
    let start = text.find(':').map_or(0, |colon| colon + 1);
    let dot = start + text[start..].rfind('.')?;
    Some((&text[..dot], &text[dot + 1..]))
}

/// The last name of a configured name's qualified name (`replace` of `pkg.store:Store.replace`,
/// `decide` of `pkg:decide`).
pub(crate) fn last_name(text: &str) -> &str {
    text.rsplit(['.', ':']).next().unwrap_or(text)
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
    fn module_patterns_match_the_module_whole() {
        // (pattern, module, qualname, matches_def, names_within)
        for (pattern, module, qualname, def, within) in [
            ("pkg:*", "pkg", "decide", true, true),
            ("pkg:*", "pkg", "<module>", true, true),
            ("pkg:*", "pkg", "Store.add.inner", true, true),
            ("pkg:*", "pkg.sub.user", "sub_user", false, false),
            ("pkg:*", "x.pkg", "decide", false, false),
            ("pkg:_*", "pkg", "_helper", true, true),
            ("pkg:_*", "pkg", "_Store.add", true, true),
            ("pkg:_*", "pkg", "Store._add", false, false),
            ("pkg:Store.*", "pkg", "Store.add", true, true),
            ("pkg.sub:Store.*", "pkg.sub", "Store.add", true, true),
            ("pkg.sub:Store.*", "pkg.sub", "Store", false, false),
            ("pkg.sub:Store", "pkg.sub", "Store.add", false, true),
            ("pkg.sub:Store", "pkg", "sub.Store", false, false),
            ("pkg.*:run", "pkg.jobs.nightly", "run", true, true),
            ("pkg.*:run", "pkg", "run", false, false),
            ("pkg.*:run", "pkg.jobs", "Job.run", false, false),
            ("decide", "pkg", "decide", true, true),
            ("pkg", "pkg.sub", "f", false, true),
        ] {
            let p = DottedPattern::new(pattern, "test").unwrap();
            let fqn = format!("{module}.{qualname}");
            assert_eq!(p.matches_def(&fqn, qualname), def, "{pattern} {fqn}");
            assert_eq!(p.names_within(&fqn, qualname), within, "{pattern} {fqn}");
        }
        // An external symbol, and a name known only as a whole, have no module.
        let p = DottedPattern::new("os.path:join", "test").unwrap();
        assert!(!p.matches_def("os.path.join", "os.path.join"));
        assert!(!p.names_within("os.path.join", "os.path.join"));
        assert!(!p.matches("join"));
        let class = |fqn: &str, qualname: Option<&str>| ContractClass {
            fqn: fqn.to_owned(),
            qualname: qualname.map(str::to_owned),
        };
        let p = DottedPattern::new("pkg.model:Model", "test").unwrap();
        assert!(p.matches_class(&class("pkg.model.Model", Some("Model"))));
        assert!(!p.matches_class(&class("pkg.model.Model", None)));
        assert!(
            DottedPattern::new("Model", "test")
                .unwrap()
                .matches_class(&class("pkg.model.Model", None))
        );
        for bad in ["pkg:", ":f", "a:b:c", "pkg:[x"] {
            assert!(DottedPattern::new(bad, "test").is_err(), "{bad}");
        }
        for (text, split, last) in [
            (
                "pkg.model:Model.fields",
                Some(("pkg.model:Model", "fields")),
                "fields",
            ),
            ("pkg:decide", None, "decide"),
            ("Model.fields", Some(("Model", "fields")), "fields"),
            ("pkg.Model.a:b", None, "b"),
        ] {
            assert_eq!(split_last_dot(text), split, "{text}");
            assert_eq!(last_name(text), last, "{text}");
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
    fn a_rule_is_an_error_for_a_critical_bug_and_a_warning_otherwise() {
        for r in CORE_RULES {
            let expected = match r.consequence {
                Consequence::Bug => Severity::Error,
                Consequence::MinorBug | Consequence::Slowdown => Severity::Warning,
            };
            assert_eq!(r.default_severity(), expected, "{}", r.code);
            // The `OPT` rules report slower code, and every `MUT` rule a bug.
            match r.family {
                RuleFamily::Opt => assert_eq!(r.consequence, Consequence::Slowdown, "{}", r.code),
                RuleFamily::Mut => assert_ne!(r.consequence, Consequence::Slowdown, "{}", r.code),
                RuleFamily::Eff | RuleFamily::Data | RuleFamily::Imp => {}
            }
        }
    }

    #[test]
    fn a_rules_hundreds_digit_says_what_it_needs() {
        // In every family, 1xx rules need no configuration and 2xx rules a declared contract,
        // so `MUT2` selects the contract rules of `MUT`; `IMP3xx` rules read the dependencies
        // the project declares.
        for r in CORE_RULES {
            let number = r.code.trim_start_matches(|c: char| c.is_ascii_uppercase());
            let band = number.chars().next().unwrap();
            let bands = match (r.activation, r.family) {
                (Activation::Contract, _) => "2",
                (_, RuleFamily::Imp) => "13",
                _ => "1",
            };
            assert!(
                number.len() == 3 && bands.contains(band),
                "{}: {:?}",
                r.code,
                r.activation
            );
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
        // An experimental rule runs only where a configuration names it or declares its
        // contract.
        for f in EXPERIMENTAL_FEATURES
            .iter()
            .filter(|f| f.kind == ExperimentalKind::Rule)
        {
            let rule = find_rule(f.target).unwrap();
            assert_eq!(rule.code, f.target);
            assert!(
                matches!(rule.activation, Activation::OptIn | Activation::Contract),
                "{}",
                rule.code
            );
            assert_eq!(experimental_rule(rule.code), Some(f));
        }
        assert_eq!(experimental_rule("MUT107"), None);
    }
}

#[cfg(test)]
mod docs_links {
    use super::CORE_RULES;
    use heki_env::Docs;
    use heki_env::problem::SITE;
    use std::path::{Path, PathBuf};

    /// The anchor the documentation site gives a heading (Python-Markdown's `toc` slug: word
    /// characters, spaces, and hyphens kept, lowercased, runs of spaces and hyphens joined into
    /// one hyphen).
    fn anchor(heading: &str) -> String {
        let kept: String = heading
            .chars()
            .filter(|c| c.is_alphanumeric() || matches!(c, '_' | '-') || c.is_whitespace())
            .collect();
        let mut out = String::new();
        for (i, word) in kept
            .trim()
            .to_lowercase()
            .split(|c: char| c == '-' || c.is_whitespace())
            .filter(|w| !w.is_empty())
            .enumerate()
        {
            if i > 0 {
                out.push('-');
            }
            out.push_str(word);
        }
        out
    }

    /// The file of the page at `path` below the site root: the Markdown of `configuration/`, or
    /// a file the site serves as it is (`llms.txt`).
    fn page_file(docs: &Path, path: &str) -> PathBuf {
        if !path.is_empty() && !path.ends_with('/') {
            return docs.join(path);
        }
        let page = path.trim_end_matches('/');
        let index = docs.join(page).join("index.md");
        if index.exists() {
            index
        } else {
            docs.join(format!("{page}.md"))
        }
    }

    /// Every page and section heki prints a link to is a page of `docs/` with that heading.
    #[test]
    fn every_printed_link_names_a_page_and_a_heading() {
        let docs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs");
        let rule_pages = CORE_RULES.iter().map(|r| r.page_url());
        let docs_pages = Docs::ALL.iter().map(|d| d.url());
        for url in docs_pages.chain(rule_pages) {
            let below = url.strip_prefix(SITE).expect("a link of the site");
            let (path, fragment) = below.split_once('#').unwrap_or((below, ""));
            let file = page_file(&docs, path);
            let text = std::fs::read_to_string(&file)
                .unwrap_or_else(|e| panic!("{url}: no page {}: {e}", file.display()));
            if !fragment.is_empty() {
                let anchors: Vec<String> = text
                    .lines()
                    .filter_map(|l| l.strip_prefix('#'))
                    .map(|h| anchor(h.trim_start_matches('#')))
                    .collect();
                assert!(
                    anchors.iter().any(|a| a == fragment),
                    "{url}: no heading of `{path}` has the anchor `{fragment}`"
                );
            }
        }
        assert_eq!(
            anchor("Rule selection: `[tool.heki.lint]`"),
            "rule-selection-toolhekilint"
        );
        assert_eq!(anchor("How `--fix` applies fixes"), "how-fix-applies-fixes");
    }
}

#[cfg(test)]
mod explain_tests {
    use crate::project::ProjectError;
    use crate::recommend::RecommendError;
    use crate::{RuleConfigError, SelectionError};
    use heki_env::Explain as _;

    #[test]
    fn recommend_errors_explain_as_their_causes() {
        let selector = || SelectionError::UnknownRule {
            selector: "NOPE1".to_owned(),
            origin: "--select",
        };
        let rules = || RuleConfigError::TooManyInvalidations(99);
        let version = || ProjectError::TargetVersion("py39".to_owned());
        let cases = [
            (RecommendError::Selection(selector()), selector().explain()),
            (RecommendError::Rules(rules()), rules().explain()),
            (RecommendError::Project(version()), version().explain()),
        ];
        for (error, cause) in cases {
            assert_eq!(error.explain(), cause);
        }
    }
}
