//! Advisory / Effect lane (`Family 1: MUT1xx/MUT2xx` & `Family 2: EFF/DATA`).
//!
//! Designed from Day 1 for **incremental LSP execution** (ADR-017):
//! - Per-file `FileSummary` is stored in memory; `IrFunction` bodies are NOT retained
//!   after summarization. A `FileSummary` keeps the function's *local* facts
//!   (`local_functions` + `call_sites`), so interprocedural propagation can be recomputed
//!   for any region of the project without IR.
//! - On file edits, `ProjectIndex::update_file` compares old vs new `EffectSummary`
//!   (**Early Cutoff**): if summaries did not change (>95% of edits), reverse call-graph
//!   propagation is skipped completely.
//!
//! Pipeline (docs/ANALYSIS_AND_KB_SPEC.md section 2):
//! 1. `resolver`: module names, import maps, project declarations, 4-priority receiver types, CHA.
//! 2. `summary`: local (one function) effect extraction into `EffectSummary` + `CallSite`s,
//!    with the guards of each effect and call site (`guards`).
//! 3. `scc` + `callgraph`: Tarjan SCC reverse-topological worklist fixpoint; `guards` maps a
//!    callee's guards across each call site (pruning the effects an argument switches off).
//! 4. `report`: Boundary-Only Reporting.

mod blind_spots;
pub use blind_spots::{Blind, BlindCall, BlindSpotReach, UnresolvedReason};
pub mod cache_flow;
mod callgraph;
pub mod cancellation;
pub use callgraph::{HopGap, MustExplainer, MustGap, SiteMust};
pub mod capacity;
pub mod code_kind;
mod paths;
pub use paths::with_paths;
mod construction;
pub mod consumption;
mod contracts;
pub(crate) mod decorators;
mod edited;
pub use edited::{FactChange, OffsetMap, Restated};
mod environment;
pub mod fresh;
pub mod guarded;
pub mod guards;
pub mod held;
pub mod imports;
mod infer;
mod invalidation;
mod invokes;
pub mod iterations;
pub mod locks;
pub mod one_at_a_time;
mod only_read;
pub use only_read::{
    AnnotationFacts, AnnotationReads, ContainerParams, LiteralAnnotation, ParamAnnotation,
    ReadKinds, ReadOverride, read_override,
};
pub mod dispatch;
pub mod order_flow;
pub mod ownership;
mod partial;
mod query;
pub mod readonly;
mod report;
pub mod resolver;
mod scc;
mod summary;
pub mod types;
mod witness;

pub use cache_flow::{
    CacheEvent, CacheFlow, FlowBlock, InPlaceChange, KeptRemoval, SelfAttrWrite, attr_writes,
    self_attr_writes,
};
pub use capacity::{Bound, Capacity, CapacityOf, CapacityWait, ParamOf, SiteQueues, WhileFull};
pub use contracts::{ContractClass, Contracts, OverrideRoles};
pub use environment::{AccessKind, ExternalAccess};
pub use invalidation::{
    CoveredAfter, FlowWrite, FuncFlow, INITIALIZERS, Reach, SiteFlow, UnmetWrite, body_runs_at,
    builds_written_object, delegates_to_callers, initializes_own_self, is_initializer,
    reach_across,
};
pub use iterations::{
    BetweenSteps, ContainerChange, ContainerIteration, IteratedContainer, IteratedResult,
    IterationFacts, ResultIteration,
};
pub use locks::{LockAcquire, LockFacts, ObjRef, SiteLock, StateTests};
pub use one_at_a_time::ExclusiveUse;
pub use ownership::{LocalOrigins, Origins, ResultOf, ReturnOrigins};
pub use query::{CallerSite, Callers, CallsByCaller, ChangeChain, HandedChange, ParamChange};
pub use readonly::container_mutator;
pub use readonly::{
    CacheDecorator, CachedResult, ContainerFamily, ContainerKind, ContractSharing, FamilyLevel,
    FamilyRole, ProtectedReturn, ReturnStep, Shared,
};
pub use report::{BoundaryReport, boundary_reports, boundary_reports_in};
pub use resolver::{
    Callee, CapturedParams, ClassDecl, CopyMade, DeclKeys, DeclPasses, FamilyMembers, FamilyOpen,
    ForwardedReturn, ForwardingCall, FunctionDecl, HeldSentinel, ModuleDecls, ModuleLayout,
    NamedAttr, ProjectDecls, ReadOnlyAttr, ReadOnlyKind, StrConstant, StrValue, TypeCtx, Val,
    aliases_of, detect_source_roots, func_qualname, is_source_dir, module_name_for_path,
    module_prefix, resolve_relative_module, self_aliases,
};
pub use summary::BLOCKING_EFFECTS;

use heki_ast::Span;
use heki_ir::ModuleIr;
use heki_kb::{EffectiveEcosystemKb, EffectiveSoundnessKb, ExecEffectKind};
use heki_semantic::SemanticModel;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Canonical function key across files (stable across line movements).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FuncKey {
    pub module: String,
    pub qualname: String,
}

impl FuncKey {
    /// Dotted fully qualified name (`pkg.mod.Class.method`).
    pub fn fqn(&self) -> String {
        format!("{}.{}", self.module, self.qualname)
    }
}

/// Attribute / constant-key access path (distinguishes disjoint dict keys for `MUT101`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AttrKeyPath {
    pub attr: String,
    pub const_key: Option<String>,
}

/// A project-class attribute write a function reaches through any number of calls: the facts
/// of a typed write site without its position and chain (`EffectSummary::typed_field_writes`).
/// A caller asks it whether a callee already sees the type of what it writes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TypedFieldWrite {
    pub class_qualname: String,
    pub attr: String,
    pub kind: WriteKind,
    /// The written object is under construction: `TypedFieldWriteSite::fresh`, or the object
    /// a class's own initialization builds ([`initializes_own_self`]), written by the
    /// initializer or by a helper it passes `self` to.
    pub fresh: bool,
    /// `TypedFieldWriteSite::obj_params`, mapped through every call as for the sites: a
    /// helper's write reaches the initializer handing it `self` as a write of its own `self`.
    pub obj_params: u64,
}

/// The objects (`obj_params`) [`EffectSummary::typed_field_writes`] keeps apart for one write
/// (field, kind, and freshness). A call can hand a helper a choice of its parameters (`h(p0 if
/// flag else p1)`), so each level of a chain of helpers calling the one below twice could double
/// them; past the bound they are one fact on an object that may be any of them, as a write site
/// on several parameters is.
const WRITTEN_OBJECTS_KEPT_APART: usize = 8;

/// Join the objects of each write of `writes` that has more than
/// [`WRITTEN_OBJECTS_KEPT_APART`]. It depends only on the set, not on the order the facts came
/// in, so a summary stays the same however its callees' facts reach it.
fn join_written_objects(writes: &mut BTreeSet<TypedFieldWrite>) {
    // The facts order by field, kind, freshness, and then object: one write's facts are a run.
    let same = |a: &TypedFieldWrite, b: &TypedFieldWrite| {
        (&a.class_qualname, &a.attr, a.kind, a.fresh)
            == (&b.class_qualname, &b.attr, b.kind, b.fresh)
    };
    let mut runs: Vec<(TypedFieldWrite, usize)> = Vec::new();
    for w in writes.iter() {
        match runs.last_mut() {
            Some((first, n)) if same(first, w) => *n += 1,
            _ => runs.push((w.clone(), 1)),
        }
    }
    for (first, n) in runs
        .into_iter()
        .filter(|(_, n)| *n > WRITTEN_OBJECTS_KEPT_APART)
    {
        let joined: Vec<TypedFieldWrite> = writes.range(first.clone()..).take(n).cloned().collect();
        let obj_params = joined.iter().fold(0, |mask, w| mask | w.obj_params);
        for w in &joined {
            writes.remove(w);
        }
        writes.insert(TypedFieldWrite {
            obj_params,
            ..first
        });
    }
}

/// One hop in an interprocedural call chain (for Boundary-Only & LSP RelatedLocation).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CallHop {
    pub caller: FuncKey,
    /// `FuncKey::fqn()` of a project function, or a Layer 2 symbol.
    pub callee: String,
    pub span: Span,
    pub path: PathBuf,
}

/// A call chain kept in a summary. Propagation copies a callee's chains into its callers, so
/// the hops are shared: a chain through a call points to the hop that call made, and extending
/// a chain copies pointers. `Arc` compares, orders, and hashes by value, so the chains do too.
pub type SharedChain = Vec<Arc<CallHop>>;

/// A project class and its MRO (the class first), shared by every call site and fact naming it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ClassLine {
    pub mro: Vec<String>,
}

impl ClassLine {
    pub fn class(&self) -> &str {
        &self.mro[0]
    }

    /// Whether an instance of one class may be an instance of the other: one derives from the
    /// other (a class is related to itself).
    pub fn related(&self, other: &ClassLine) -> bool {
        self.derives_from(other) || other.derives_from(self)
    }

    /// Whether an instance of exactly this class is an instance of `other`: `other` is on its
    /// MRO (the class itself included).
    pub fn derives_from(&self, other: &ClassLine) -> bool {
        self.mro.iter().any(|c| c == other.class())
    }
}

pub type SharedLine = Arc<ClassLine>;

/// A parameter being an instance of `provider`, a subclass of its class that gives a call on it
/// the implementation a way of [`EffectSummary::provided_effects`] runs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Need {
    /// The parameter (a method's `self` is 0).
    pub param: u8,
    pub provider: SharedLine,
}

/// One way a function reaches an execution effect only through implementations that subclasses
/// of parameters' classes give calls on those parameters ([`EffectSummary::provided_effects`]):
/// it runs only when every parameter of `needs` is an instance of its provider. A subclass's
/// override of a call on one parameter that reaches the override another subclass gives a call
/// on a second parameter needs both.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Provided {
    /// Sorted, without repeats, never empty.
    pub needs: Vec<Need>,
    /// The shortest chain through the providers' implementations.
    pub chain: SharedChain,
}

/// [`EffectSummary::provided_effects`].
pub type ProvidedEffects = BTreeMap<ExecEffectKind, Vec<Provided>>;

/// One way a function reaches an execution effect, kept for the chain a report shows
/// ([`EffectSummary::witness_ways`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WitnessWay {
    /// The `isinstance` tests of the function's parameters the way's calls are behind, sorted:
    /// a call whose arguments decide one against the way does not take it.
    pub needs: Vec<InputGuard>,
    /// A call on the chain is in an `except` handler or a `finally` body: the way is cleanup
    /// after an error, not the work the function does.
    pub cleanup: bool,
    pub chain: SharedChain,
}

/// [`EffectSummary::witness_ways`].
pub type WitnessWays = BTreeMap<ExecEffectKind, Vec<WitnessWay>>;

/// Most ways one execution effect keeps for its witness ([`EffectSummary::witness_ways`]).
pub const WITNESS_LIMIT: usize = 8;

/// Most ways one execution effect is kept as reached only through subclasses' implementations
/// ([`EffectSummary::provided_effects`]), and most calls on the chain of one way: past either,
/// the effect is taken to run whatever the parameters are. Each way keeps a chain of its own, so
/// the two bound what a function keeps of them, also down a long chain of helpers handing a
/// parameter on.
pub const PROVIDED_LIMIT: usize = 16;
pub const PROVIDED_CHAIN_LIMIT: usize = 32;

/// The execution effects a function has run whenever it returns normally, each by a chain on
/// which every call is made on every path that returns (exception edges included), every
/// call's targets are all the implementations it can run (R2, R5), and the last call is a
/// catalog call that has the effect whatever its state and arguments heki cannot see (R3, R6).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MustExec {
    /// No path returns normally (the function always raises, or never ends): whatever runs
    /// "whenever it returns" holds of it.
    pub never_returns: bool,
    /// The function's name is bound to more than one definition (`def` under `if` and `else`):
    /// a call runs one of them, and neither one's facts are the call's (R1 holds for names
    /// bound once).
    pub bound_twice: bool,
    pub effects: EffectChains,
    /// For an effect whose chains end in different calls (implementations of one call reaching
    /// different waits), every such call: the witnesses beside the kept chain's last call.
    #[serde(default)]
    pub alternatives: EffectChains,
    /// The effects the function has run whenever it returns when its callers pass values that
    /// pass tests of its parameters ([`GivenChains`]); none of them is one of [`Self::effects`].
    #[serde(default)]
    pub given: GivenChains,
}

/// [`MustExec::given`]: per effect, chains that hold when the arguments bound to some of the
/// function's parameters pass the tests the chain needs ([`ValueTest`], decided at each call as
/// the call decides an argument of its own, or handed on to the caller's parameter). At most
/// [`MAX_GIVEN_TESTS`] tests per chain and [`MAX_GIVEN_CHAINS`] chains per effect: those needing
/// the fewest tests, then the shortest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GivenChains(Vec<GivenChain>);

/// Tests of values bound to a function's parameters, each with the parameter's position.
pub type ParamTests = Vec<(u8, ValueTest)>;

/// A chain of [`GivenChains`]: it holds when the value bound to each parameter of `needs` passes
/// its test (sorted by parameter).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GivenChain {
    pub effect: ExecEffectKind,
    pub needs: ParamTests,
    pub chain: SharedChain,
    /// For implementations of one call ending in different calls, every such call, as
    /// [`MustExec::alternatives`] keeps them for a chain that holds whatever is passed; empty
    /// when they end in the chain's last call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub witnesses: SharedChain,
}

/// The tests a chain of [`GivenChains`] needs, at most.
pub const MAX_GIVEN_TESTS: usize = 4;
/// The chains of one effect [`GivenChains`] keeps, at most.
pub const MAX_GIVEN_CHAINS: usize = 4;

impl GivenChains {
    /// The chains of `e`.
    pub fn of(&self, e: ExecEffectKind) -> impl Iterator<Item = &GivenChain> {
        self.0.iter().filter(move |c| c.effect == e)
    }

    pub fn effects(&self) -> impl Iterator<Item = ExecEffectKind> + '_ {
        self.0.iter().map(|c| c.effect)
    }

    /// Keeps `chain` of `e` given `needs`, with the calls its implementations end in
    /// (`witnesses`, [`GivenChain::witnesses`]), unless a chain of `e` needing some of them is
    /// no longer; it replaces the chains needing more. Past the bounds the chains needing the
    /// most tests go first.
    pub fn insert(
        &mut self,
        e: ExecEffectKind,
        mut needs: Vec<(u8, ValueTest)>,
        chain: SharedChain,
        witnesses: SharedChain,
    ) {
        needs.sort();
        needs.dedup();
        if needs.len() > MAX_GIVEN_TESTS {
            return;
        }
        let within = |small: &[(u8, ValueTest)], large: &[(u8, ValueTest)]| {
            small.iter().all(|n| large.contains(n))
        };
        let covered = self.of(e).any(|c| {
            within(&c.needs, &needs) && (c.needs != needs || c.chain.len() <= chain.len())
        });
        if covered {
            return;
        }
        self.0
            .retain(|c| c.effect != e || !within(&needs, &c.needs));
        self.0.push(GivenChain {
            effect: e,
            needs,
            chain,
            witnesses,
        });
        self.0.sort_by(|a, b| {
            (a.effect, a.needs.len(), a.chain.len()).cmp(&(b.effect, b.needs.len(), b.chain.len()))
        });
        let mut kept = 0;
        self.0.retain(|c| {
            kept = if c.effect == e { kept + 1 } else { kept };
            c.effect != e || kept <= MAX_GIVEN_CHAINS
        });
    }

    pub fn retain(&mut self, keep: impl FnMut(&GivenChain) -> bool) {
        self.0.retain(keep);
    }

    pub fn chains_mut(&mut self) -> impl Iterator<Item = &mut SharedChain> {
        self.0
            .iter_mut()
            .flat_map(|c| [&mut c.chain, &mut c.witnesses])
    }
}

/// A chain per effect, sorted by effect. A function has run one or two effects whenever it
/// returns, which a vector holds in a tenth of the bytes of a tree's first node.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EffectChains(Vec<(ExecEffectKind, SharedChain)>);

impl EffectChains {
    pub fn get(&self, e: &ExecEffectKind) -> Option<&SharedChain> {
        self.0.iter().find(|(k, _)| k == e).map(|(_, chain)| chain)
    }

    pub fn keys(&self) -> impl Iterator<Item = &ExecEffectKind> {
        self.0.iter().map(|(k, _)| k)
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut SharedChain> {
        self.0.iter_mut().map(|(_, chain)| chain)
    }

    /// Set the chain of `e`, replacing the one it had.
    pub fn insert(&mut self, e: ExecEffectKind, chain: SharedChain) {
        self.remove(&e);
        self.0.push((e, chain));
        self.0.sort_by_key(|(k, _)| *k);
    }

    /// Set the chain of `e` unless it has one.
    pub fn insert_new(&mut self, e: ExecEffectKind, chain: impl FnOnce() -> SharedChain) {
        if self.get(&e).is_none() {
            self.insert(e, chain());
        }
    }

    pub fn remove(&mut self, e: &ExecEffectKind) {
        self.0.retain(|(k, _)| k != e);
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&ExecEffectKind) -> bool) {
        self.0.retain(|(k, _)| keep(k));
    }
}

impl MustExec {
    /// The facts of a function whose name is bound to more than one definition.
    pub fn of_name_bound_twice() -> Self {
        Self {
            bound_twice: true,
            ..Self::default()
        }
    }

    /// Whether every normal return of the function has run `e`, and the chain that shows it
    /// (empty for a function that never returns normally).
    pub fn chain(&self, e: ExecEffectKind) -> Option<&[Arc<CallHop>]> {
        if self.bound_twice {
            return None;
        }
        if self.never_returns {
            return Some(&[]);
        }
        self.effects.get(&e).map(Vec::as_slice)
    }
}

/// What running a function's body runs of a call cycle: a function that can call itself again
/// through calls whose bodies run on the caller's stack (called there, or a generator iterated
/// there; never a coroutine only created, nor a callable a worker thread runs), so that a deep
/// enough input raises `RecursionError` ([`EffectSummary::call_cycle`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallCycle {
    /// A function it calls, through any number of such calls, is on a call cycle.
    Reaches,
    /// The function itself is on a call cycle.
    On,
}

/// Whether a caller has made a call whenever it returns normally (`heki_ir::Paths`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OnEveryReturn {
    /// The call was made, also on a path where it raised.
    pub called: bool,
    /// The call returned.
    pub returned: bool,
    /// Some path reaches the call: it is not behind a branch the code decides against
    /// (`if not _IS_SYNC:` with `_IS_SYNC = False`).
    pub live: bool,
}

/// The bit of `e` in a mask of execution effects (`CallTarget::Layer2::unconditional`).
/// The path of [`EffectSummary::mutated_param_paths`] and
/// [`EffectSummary::changed_param_attrs`] for a change of an object the argument holds as an item
/// or an element of its iteration, or of one below it (`p["k"].append(x)`, `p[0].name = v`,
/// `for row in p: row.clear()`), also through a shallow copy of the argument (`dict(p)["k"]`).
/// No attribute has this name.
pub const ELEMENTS_PATH: &str = "[]";

pub const fn effect_bit(e: ExecEffectKind) -> u16 {
    1 << (e as u16)
}

/// What a method call's receiver tells about the implementations its callees run
/// ([`EffectSummary::provided_effects`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dispatch {
    /// The receiver's class (a receiver of one project class, the class a constructor makes,
    /// or the caller's own `self`), when a callee is inherited from a proper base of it: the
    /// callee's facts may come from implementations of classes unrelated to it.
    pub receiver: Option<SharedLine>,
    /// For a call on a parameter of the caller as itself (its own `self`, or an annotated
    /// parameter): the callees (by index) that are overrides of subclasses, each with its class
    /// (it runs only when the parameter is an instance of it).
    pub providers: Vec<(u16, SharedLine)>,
    /// The parameter of the caller the receiver is when there are `providers` (`self`: 0).
    #[serde(default)]
    pub receiver_param: u8,
    /// The method a project protocol declares, when the receiver is typed with the protocol
    /// (`h._helper()` with `h: Host`): the call contracts judge the call by that declaration,
    /// the type the code calls it through, while the callees are the implementations it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared: Option<FuncKey>,
    /// The project protocol the receiver is typed with, when it declares the method
    /// (`pkg.Host` of `h.build()` with `h: Host`), as the receiver expression tells it: a static
    /// method binds no receiver to a parameter, and a class method binds the class, so the
    /// callees' parameters do not tell it. A reference keeps it too, without [`Self::declared`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    /// Parameters of the caller the receiver may be, as itself (`cls.m()`, `self.m()`,
    /// `super().m()`: bit 0), for a call of a class method, which binds the class and no value of
    /// the caller, or of a static method, which binds no receiver at all: the first bound
    /// parameter of the callees does not tell it ([`CallSite::recv_params`]). Kept only when a
    /// `call-owners` contract allows calls through a method's own receiver
    /// ([`Contracts::own_receiver_calls`]).
    #[serde(default)]
    pub recv_params: u64,
}

/// The hops of a kept chain, owned (what a report shows).
pub fn owned_chain(chain: &[Arc<CallHop>]) -> Vec<CallHop> {
    chain.iter().map(|hop| CallHop::clone(hop)).collect()
}

/// A parameter mask that names no parameter: left out of a serialized fact.
fn no_params(mask: &u64) -> bool {
    *mask == 0
}

/// Zero the position of every hop of a kept chain (a hop shared with other chains is copied).
pub(crate) fn zero_hop_spans(chain: &mut [Arc<CallHop>]) {
    for hop in chain {
        Arc::make_mut(hop).span = Span { start: 0, end: 0 };
    }
}

/// Write site detail for MUT201.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TypedFieldWriteSite {
    /// Fully qualified class name (`pkg.model.DataModel`).
    pub class_qualname: String,
    pub attr: String,
    pub kind: WriteKind,
    /// Span of the write (or of the helper call for `IndirectHelper`) in the file of the
    /// function that performs it: the summary owner when `via_chain` is empty, else the
    /// last hop's callee.
    pub span: Span,
    pub via_chain: SharedChain,
    /// Parameters of the summary owner the written object may be, as itself (bit 0: a
    /// method's own `self`), mapped through every hop. Empty for an object that is no
    /// parameter: a fresh instance, an attribute of one, a call result.
    pub obj_params: u64,
    /// The written object is an instance that the function holding the original write
    /// constructs itself (`Model(...)`, `cls(...)`, through copies) and fills before anyone
    /// else holds it (`MUT108`).
    pub fresh: bool,
    /// The constant key of the entry the write changes in the container held in `attr`
    /// (`obj.attr["k"] = v`, `del obj.attr[KEY]`, `obj.attr.pop("k")`, `obj.attr["k"][i] = v`):
    /// the first key below the attribute, a string literal or a module-level `Final` string
    /// constant. `None` for every other write (rebinding, a computed key, `clear()`, ...).
    pub key: Option<String>,
    /// Whether the write is part of constructing the object it writes (`fresh-objects`).
    pub construction: Construction,
    /// How the ownership contracts treat the functions of `via_chain`
    /// ([`Contracts::writer_contexts`], joined over its hops): the chains of one write that
    /// differ in it are kept apart, so a path through an allowed writer does not stand for one
    /// around it.
    pub contexts: u64,
    /// The written object is typed with a project protocol that declares the attribute (itself
    /// or through its protocol bases): the write is one of the protocol's member, the site whose
    /// class is the protocol, and of each implementation's attribute, the other sites of the
    /// write, and every one of them carries this. A flag, not the protocol's name: it fits the
    /// room the site's layout leaves. Computed only when a `write-owners` contract allows writes
    /// through protocols ([`Contracts::write_protocols`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub through_protocol: bool,
    /// Where the key of an item write comes from when `key` holds no constant (computed only
    /// when an ownership contract is declared).
    #[serde(default)]
    pub key_from: EntryKey,
    /// A copy, in a caller, of a callee's item write keyed by one of the callee's parameters,
    /// with the key the call passes in `key` or `key_from` (`set_extra(field, "init", v)` writing
    /// `field.extras[key]`): it stands for the entry targets (`Class.attr["key"]`) the write may
    /// hit, which the callee's own site leaves to its callers.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub passed: bool,
}

/// Where the key of an item write comes from when [`TypedFieldWriteSite::key`] holds no
/// constant.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum EntryKey {
    /// `key` is the write's constant key, or the write changes no single entry.
    #[default]
    Written,
    /// Parameter `p` of the summary owner: each call decides the entry it writes.
    Param(u8),
    /// A key heki cannot tell: one the function computes, or an argument that is neither a
    /// constant nor a parameter of the caller.
    Unknown,
}

/// How a function's new object was made: a shallow copy shares the containers of the original
/// (`copy.copy(x)`, `x.model_copy()`), a constructor call or a deep copy owns its containers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NewKind {
    Copy,
    Own,
}

/// Conditions on the summary owner's own calls under which an object it holds is new and kept by
/// nobody else (local facts; propagation resolves them against the callees' final summaries).
/// One pointer: most objects carry none, and every bound call argument has one.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NewCond(Option<Box<CondParts>>);

#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CondParts {
    /// The object comes from calls of these project functions: each must return a new object
    /// at its call (`EffectSummary::returns_new`).
    pub returns: Vec<NewFrom>,
    /// Earlier stores of the object: each may keep it.
    pub keeps: Vec<Keep>,
}

/// A store that may keep an object ([`CondParts::keeps`]). `at` tells what the store's call binds
/// and the tests of the function's inputs that hold where it runs: a store under `if register:`
/// keeps the object only at the calls of the function leaving `register` true.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Keep {
    pub by: KeptBy,
    pub at: CallTests,
}

/// What a [`Keep`] needs to keep the object.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeptBy {
    /// Nothing: the store keeps it.
    Store,
    /// A hand-over to parameter `.1` of the project function `.0`: it keeps the object when that
    /// callee keeps the parameter there ([`EffectSummary::retains_params`]).
    Callee(FuncKey, u8),
    /// A value read out of the result of the project function `.0` and stored: it may be the
    /// result itself when that function's result holds itself through objects it made
    /// (`ReturnsNew::New::linked`).
    ReadBack(FuncKey),
    /// A link into an object a call of a project function returned (`Escape::Into`): the value
    /// is as public as its holder, kept unless that call returns a new object there.
    Holder(NewFrom),
}

/// What a call tells of the tests its callee makes of its parameters ([`MayGiven`]): the
/// arguments that decide such a test or hand it on to the caller, by parameter, and the tests of
/// the caller's inputs that hold where the call runs (`CallSite::guards`).
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CallTests {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<(u8, ArgView)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known: Vec<InputGuard>,
}

/// An argument of [`CallTests`]: a constant, which decides a test of the parameter by its class
/// and truthiness, or the caller's own input, to which a test of the parameter is handed on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArgView {
    Const(ConstArg),
    Input(GuardSubject),
    /// The caller's own input, which the caller changes in place somewhere: only a test that
    /// holds of the object whatever changes in it (`None`, `isinstance`) is handed on.
    ChangedInput(GuardSubject),
}

/// A fact a function may have only where tests of its inputs pass: the alternatives, each the
/// tests ([`InputGuard`], sorted) that hold where the fact arises. A call that decides a test of
/// every alternative against it does not get the fact; the tests it leaves to the caller's own
/// inputs are the caller's (`guards::given_at_call`). Without alternatives the fact never
/// arises; with an alternative of no tests it arises whatever is passed, as it does past
/// [`MAX_GIVEN_TESTS`] tests in an alternative or [`MAX_GIVEN_CHAINS`] alternatives (the bounds
/// of [`GivenChains`]). One pointer: most facts have none.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MayGiven(Option<Box<Alternatives>>);

/// The boxed alternatives of a [`MayGiven`], so it stays one pointer.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
struct Alternatives(Vec<Vec<InputGuard>>);

impl MayGiven {
    /// The fact arises where the tests `tests` hold.
    pub fn given(tests: Vec<InputGuard>) -> Self {
        let mut out = Self::default();
        out.add(tests);
        out
    }

    pub fn alternatives(&self) -> &[Vec<InputGuard>] {
        self.0.as_deref().map_or(&[], |a| a.0.as_slice())
    }

    /// The fact never arises.
    pub fn never(&self) -> bool {
        self.0.is_none()
    }

    /// The fact arises whatever is passed.
    pub fn always(&self) -> bool {
        self.alternatives().iter().any(Vec::is_empty)
    }

    /// Add the alternative `tests`, unless one needing some of them is there; it replaces the
    /// alternatives needing more.
    pub fn add(&mut self, mut tests: Vec<InputGuard>) {
        tests.sort();
        tests.dedup();
        if tests.len() > MAX_GIVEN_TESTS {
            tests.clear();
        }
        let within =
            |small: &[InputGuard], large: &[InputGuard]| small.iter().all(|t| large.contains(t));
        let all = &mut self.0.get_or_insert_with(Box::default).0;
        if all.iter().any(|a| within(a, &tests)) {
            return;
        }
        all.retain(|a| !within(&tests, a));
        all.push(tests);
        all.sort();
        if all.len() > MAX_GIVEN_CHAINS {
            *all = vec![Vec::new()];
        }
    }

    /// The fact arises where it does in either.
    pub fn join(&mut self, other: &Self) {
        for tests in other.alternatives() {
            self.add(tests.clone());
        }
    }
}

/// A project function whose result an object is, at one call of it ([`CondParts::returns`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NewFrom {
    pub callee: FuncKey,
    /// The callee's parameters the call binds to a value that has no elements
    /// ([`ConstArg::has_no_elements`]), so the returns giving an element of one of them return
    /// nothing there ([`ReturnsNew::kind_at`]).
    #[serde(default)]
    pub empty_params: u64,
    /// What the call binds, for the stores of the result the callee makes only under tests of
    /// its parameters (`ReturnsNew::New::kept`).
    #[serde(default)]
    pub at: CallTests,
}

impl NewCond {
    /// The conditions, sorted and without duplicates.
    pub fn new(mut returns: Vec<NewFrom>, mut keeps: Vec<Keep>) -> Self {
        returns.sort();
        returns.dedup();
        keeps.sort();
        keeps.dedup();
        if returns.is_empty() && keeps.is_empty() {
            return Self(None);
        }
        Self(Some(Box::new(CondParts { returns, keeps })))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    pub fn returns(&self) -> &[NewFrom] {
        self.0.as_ref().map_or(&[], |c| &c.returns)
    }

    pub fn keeps(&self) -> &[Keep] {
        self.0.as_ref().map_or(&[], |c| &c.keeps)
    }

    /// Both conditions.
    pub fn and(&self, other: &Self) -> Self {
        Self::new(
            self.returns()
                .iter()
                .chain(other.returns())
                .cloned()
                .collect(),
            self.keeps().iter().chain(other.keeps()).cloned().collect(),
        )
    }
}

/// Whether a write is part of constructing the object it writes: the object was allocated in the
/// same call chain (a constructor call, a copy, a project function returning a new object) and
/// nothing else holds it yet. A shallow copy's containers are the original's, so only rebinding
/// its attributes constructs it.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Construction {
    #[default]
    No,
    Yes,
    /// Construction when `cond` holds; `own`: the object must own its containers (an in-place
    /// change). Local summaries only.
    Pending {
        cond: NewCond,
        own: bool,
    },
    /// The object is the summary owner's parameter `param`, kept by nobody before the write (when
    /// `cond` holds; final summaries carry no condition): construction at a caller that hands it
    /// a new object.
    Param {
        param: u8,
        own: bool,
        cond: NewCond,
    },
}

/// What a call argument is for the construction of the objects its callee writes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NewArg {
    #[default]
    Old,
    /// A new object of the caller, kept by nobody before the call when `cond` holds.
    New { kind: NewKind, cond: NewCond },
    /// The caller's parameter `param`, kept by nobody before the call when `cond` holds.
    Param { param: u8, cond: NewCond },
}

/// Whether a function returns a new object nobody else holds (`New` with a condition only in
/// local summaries).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReturnsNew {
    #[default]
    No,
    New {
        kind: NewKind,
        cond: NewCond,
        /// Parameters some returns give an element of instead (an item, `get`, `pop`,
        /// `setdefault`, an iteration step): the result is new only at a call binding each of
        /// them to a value that has no elements, where those returns return nothing.
        #[serde(default)]
        elements_of: u64,
        /// The function may store the result before it returns it where these tests of its
        /// inputs pass (final summaries): the result is new only at a call deciding them against
        /// the store ([`Self::at_call`]).
        #[serde(default, skip_serializing_if = "MayGiven::never")]
        kept: MayGiven,
        /// The result may hold itself through objects the function made and stored in one
        /// another (`part.parent = copied; copied.part = part`): a value read out of it may be
        /// the result, so storing that value ends the result's freshness at the call.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        linked: bool,
    },
}

impl ReturnsNew {
    /// Whether a value read out of the result may be the result itself
    /// (`ReturnsNew::New::linked`).
    pub fn linked(&self) -> bool {
        matches!(self, Self::New { linked: true, .. })
    }

    /// The kind of a resolved `New` (a final summary's) at every call.
    pub fn kind(&self) -> Option<NewKind> {
        self.kind_at(0)
    }

    /// The kind of a resolved `New` at a call binding the parameters `empty_params` to values
    /// that have no elements ([`NewFrom::empty_params`]).
    pub fn kind_at(&self, empty_params: u64) -> Option<NewKind> {
        let (kind, kept) = self.at_call(empty_params)?;
        kept.never().then_some(kind)
    }

    /// The kind of a resolved `New` at a call binding `empty_params` as [`Self::kind_at`] says,
    /// and the tests under which the function may have stored the result
    /// (`ReturnsNew::New::kept`), for the call to decide.
    pub fn at_call(&self, empty_params: u64) -> Option<(NewKind, &MayGiven)> {
        match self {
            Self::New {
                kind,
                cond,
                elements_of,
                kept,
                ..
            } if cond.is_empty() && elements_of & !empty_params == 0 => Some((*kind, kept)),
            _ => None,
        }
    }
}

/// A write heki cannot tie to a class (`MUT205`): an attribute written on an object of unknown
/// class, or a computed attribute name written on a project class or on an object of unknown
/// class. A local fact, never propagated.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct UnresolvedWriteSite {
    pub what: Unresolved,
    pub kind: WriteKind,
    pub span: Span,
    /// Parameters of the writing function the written object may be (bit 0: `self`).
    pub obj_params: u64,
    /// Whether the write constructs the object it writes (`fresh-objects`; a parameter is left
    /// to the callers, `Construction::No`).
    pub construction: Construction,
    /// The project protocol the object is typed with although its class is unknown
    /// ([`UnresolvedAccessSite::protocol`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unresolved {
    /// `obj.<attr>` written (rebound, deleted, or its container changed) on an object of
    /// unknown class.
    Receiver(String),
    /// A computed attribute name (`setattr(obj, name, v)`, `obj.__dict__[name] = v`) written on
    /// an instance of this project class (fqn).
    Name(String),
    /// A computed attribute name written on an object of unknown class: any attribute of any
    /// class.
    Anything,
    /// An attribute an instance of this project class (fqn) has already, or the class
    /// declares, written under a name heki cannot read: removed (`del obj.__dict__[name]`),
    /// copied from the namespace of an instance of the class or a subclass
    /// (`for key, value in src.__dict__.items(): obj.__dict__[key] = value`), or one of the
    /// fields the class or a subclass declares (`for name in type(src).model_fields`). It adds
    /// no attribute.
    Existing(String),
}

/// A read or a call heki cannot tie to a class (`MUT206`, `MUT207`): an attribute read, or a
/// method called, on an object of unknown class, named like a target of a `read-owners` or
/// `call-owners` contract with `unresolved = "report"`. A local fact, never propagated.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct UnresolvedAccessSite {
    /// The attribute read or the method called.
    pub name: String,
    pub access: Access,
    pub span: Span,
    /// Parameters of the accessing function the object is on every path (bit 0: `self`).
    pub obj_params: u64,
    /// The project protocol the object is typed with although its class is unknown: an
    /// instance of a protocol whose implementations heki cannot list, or a class object typed
    /// `type[P]`, whose class may be any implementation's. `allowed-protocols` allows the
    /// access when it names the protocol and the protocol declares the member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
}

/// What an [`UnresolvedAccessSite`] does with its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Read(ReadKind),
    /// `obj.name(...)`, also through a bound-method alias or `getattr(obj, "name")(...)`.
    Call,
}

/// Typed field read site for declarative Read Ownership (`MUT203`). A local fact: it is never
/// propagated to callers (the rule walks the reverse call graph instead), so every final
/// summary keeps exactly its own reads.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TypedFieldReadSite {
    /// Fully qualified class name of the read object (`pkg.model.Reference`), or of the class a
    /// class object read through is (`Reference.KIND`, `cls.KIND`).
    pub class_qualname: String,
    pub attr: String,
    pub kind: ReadKind,
    /// Parameters of the reading function the read object may be, as itself (bit 0: `self`).
    pub obj_params: u64,
    pub span: Span,
    /// The read is one of a class implementing the project protocol the object is typed with,
    /// which declares the attribute: the read and call contracts judge it by the protocol
    /// ([`Self::counts_for_contracts`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub through_protocol: bool,
    /// The read is through a class object: it reads the class attribute, never a value an
    /// instance holds (a `cached_property` gives its descriptor there).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub class_object: bool,
}

impl TypedFieldReadSite {
    /// Whether the read and call contracts count the read: one through a protocol declaring
    /// the attribute counts as the read of the protocol, not as one of each implementation (the
    /// contracts tie that read to a target's class when its family alone implements the
    /// protocol).
    pub fn counts_for_contracts(&self) -> bool {
        !self.through_protocol
    }
}

/// A comparison by identity of an attribute of an object of a project class with an `object()`
/// sentinel (`obj.attr is SENTINEL`, `is not`, on either side): a local fact of the comparing
/// function ([`EffectSummary::sentinel_tests`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SentinelTest {
    /// Fully qualified name of a class the object may be an instance of, one test per class.
    pub class_qualname: String,
    pub attr: String,
    /// The sentinel's canonical fqn ([`ProjectDecls::object_sentinel`]).
    pub sentinel: String,
    /// The object is an instance of that one class or, under R2, of a project subclass (by
    /// construction or annotation): no other class, protocol, or union member.
    pub only: bool,
    pub span: Span,
}

/// A call of `copy.deepcopy` with one argument and no `memo`, of an object heki knows the
/// project classes of: a local fact of the calling function ([`EffectSummary::deep_copies`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DeepCopySite {
    /// Fully qualified names of the project classes the argument may be an instance of.
    pub classes: Vec<String>,
    /// The argument is an instance of the one class `classes` holds or, under R2, of a
    /// project subclass (by construction or annotation).
    pub only: bool,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadKind {
    /// `obj.attr`.
    DirectAttr,
    /// `getattr(obj, "attr"[, default])` / `object.__getattribute__(obj, "attr")`.
    GetAttr,
    /// `hasattr(obj, "attr")`, which evaluates the attribute.
    HasAttr,
    /// `obj.__dict__["attr"]` / `obj.__dict__.get("attr")`.
    DictDirect,
}

impl ReadKind {
    /// The kind in words, for report messages.
    pub fn describe(self) -> &'static str {
        match self {
            Self::DirectAttr => "by attribute",
            Self::GetAttr => "with `getattr`",
            Self::HasAttr => "with `hasattr`",
            Self::DictDirect => "as a `__dict__` item",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteKind {
    DirectAttr,
    AttrDelete,
    ContainerMut,
    AliasContainerMut,
    SetAttr,
    DictDirect,
    IndirectHelper,
    /// An entry of an entry of the container held in the attribute changed in place
    /// (`obj.attr["k"][name] = v`, `obj.attr["k"].append(v)`): `TypedFieldWriteSite::key` is the
    /// first key.
    ItemMut,
    /// An entry of the container a typed field holds changed in place by a helper the field is
    /// handed to, which does not see its class (`helper(obj.attr)` doing `box["k"].add(v)`): the
    /// [`Self::IndirectHelper`] write of an entry, kept apart from it as [`Self::ItemMut`] is
    /// from [`Self::ContainerMut`].
    IndirectItemMut,
}

impl WriteKind {
    /// Rebinding the attribute (or deleting it): the container it held, if any, is unchanged.
    pub fn rebinds(self) -> bool {
        matches!(
            self,
            Self::DirectAttr | Self::AttrDelete | Self::SetAttr | Self::DictDirect
        )
    }

    /// A write made at a call by a helper that does not see the written object's class: the
    /// function handing the object over makes it.
    pub fn through_helper(self) -> bool {
        matches!(self, Self::IndirectHelper | Self::IndirectItemMut)
    }
}

/// A named slot of an object: an attribute (`obj.a`) or a constant string key (`obj["a"]`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Slot {
    Attr(String),
    Key(String),
}

impl WriteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DirectAttr => "direct_attr",
            Self::AttrDelete => "attr_delete",
            Self::ContainerMut => "container_mut",
            Self::AliasContainerMut => "alias_container_mut",
            Self::SetAttr => "set_attr",
            Self::DictDirect => "dict_direct",
            Self::IndirectHelper => "indirect_helper",
            Self::ItemMut => "item_mut",
            Self::IndirectItemMut => "indirect_item_mut",
        }
    }
}

impl WriteKind {
    /// The kind in words, for report messages (`as_str` is the stable machine name).
    pub fn describe(self) -> &'static str {
        match self {
            Self::DirectAttr => "by attribute",
            Self::AttrDelete => "by deleting the attribute",
            Self::ContainerMut => "in place",
            Self::AliasContainerMut => "in place through an alias",
            Self::SetAttr => "with `setattr`",
            Self::DictDirect => "as a `__dict__` item",
            Self::IndirectHelper => "through a helper",
            Self::ItemMut => "in place inside an item",
            Self::IndirectItemMut => "inside an item, through a helper",
        }
    }
}

/// What the catalog says of a cached property's decorator ([`KeptFacts::listed_cache`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedCache {
    /// An instance without a `__dict__` keeps the value in a slot instead
    /// (`heki_kb::CachedPropertyDecorator::slot`).
    pub in_slot: bool,
}

/// What `MUT101`'s exact claim reads of a function ([`EffectSummary::kept`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeptFacts {
    /// The `self` attributes the function reads on every path on which it returns normally,
    /// exception edges into handlers included: its own loads that returned, whose value is
    /// read as a whole or, when every use of a load reads an item of a constant key, with that
    /// key (an attribute read as a whole covers each of its keys); and the facts of the `self`
    /// methods it calls and properties it reads on every such path, when the call's targets
    /// are every implementation it can run and each runs its body at every call (a cached
    /// property or function, or a callee of the function's own call-graph component, adds
    /// nothing; over several targets, what every one reads).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub must_read_self_attrs: BTreeSet<AttrKeyPath>,
    /// The values kept in an instance `__dict__` (a `@cached_property`'s) the function may
    /// remove or replace, on any object, itself or through the calls it makes
    /// ([`KeptRemoval`]).
    #[serde(default)]
    pub removes_kept: KeptRemoval,
    /// The cached property's decorator is one the catalog lists as keeping the value in the
    /// instance `__dict__` (`heki_kb::CachedPropertyDecorator`); any other `cached_property` is
    /// one by its name only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listed_cache: Option<ListedCache>,
}

/// Unified interprocedural summary propagated bottom-up over Tarjan SCCs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectSummary {
    // Family 2: Execution Effect
    pub direct_exec_effects: BTreeSet<ExecEffectKind>,
    pub transitive_exec_effects: BTreeSet<ExecEffectKind>,
    /// The call chain a report shows for each effect (direct or transitive) when it knows
    /// nothing of the arguments: one on the normal path before one through cleanup code, then
    /// the shortest, the first found among equals ([`Self::witness_ways`]).
    pub exec_chains: BTreeMap<ExecEffectKind, SharedChain>,
    /// The ways to an effect a call may choose its witness among, kept only when the chain of
    /// [`Self::exec_chains`] alone does not tell: some way is behind `isinstance` tests of the
    /// parameters (`if isinstance(item, Response): ...`), or the kept chain runs through cleanup
    /// code. Per effect, the best chain of each set of tests (the empty set included); a way
    /// with no more tests and no worse chain than another leaves no room for it. A call whose
    /// arguments decide a test against a way takes another one (`witness::ways_at_call`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub witness_ways: Option<Box<WitnessWays>>,
    /// The execution effects reached only through implementations that subclasses of a
    /// parameter's class give a call on the parameter (a mixin method's
    /// `self.execute_command()`, `helper(notes: Notes)` calling `notes.preview()`): per effect,
    /// each way's parameters and the classes providing the implementations it runs, with the
    /// shortest chain through them ([`Provided`]). A call passing an instance of exactly a class
    /// runs only the ways of the classes on its MRO, a call on a receiver of known class only
    /// those of the classes related to it ([`callgraph::arrival`]).
    pub provided_effects: Option<Box<ProvidedEffects>>,
    /// What the function has run whenever it returns normally ([`MustExec`]); `None` when it
    /// established nothing, or when no enabled rule reads it ([`OptInFacts::must_effects`]).
    pub must_exec: Option<Box<MustExec>>,

    // Family 1: Mutation & State Effect (MUT1xx / MUT2xx)
    pub mutates_params: u64,
    /// Parameters the function itself types with a project class (`self` of a method, an
    /// annotated parameter): its typed write sites, and those of its callees, already cover
    /// every change of them, so a caller passing an instance adds no `IndirectHelper` site.
    pub typed_params: u64,
    pub escapes_params: u64,
    /// Parameters the function returns a quadratic reallocation of on some path: the result of
    /// a `QuadraticRealloc` call taking the parameter (`return np.append(acc, x)`,
    /// `return pd.concat([df, part])`), or of a project call doing so whose result it returns
    /// (`DATA101` follows an accumulation through such helpers).
    pub reallocates_params: u64,
    /// Parameters iterated to exhaustion on every path that returns normally when the body
    /// runs at the call (`MUT106`: passing a single-pass iterator consumes it).
    pub consumes_params: u64,
    /// Parameters passed (as themselves) to project calls only where the call runs on every
    /// path that returns normally: `consumes_params` propagates through those calls alone.
    pub unconditional_forwards: u64,
    /// Parameters an iteration steps on some path when the body runs (`for`, comprehensions,
    /// unpacking, `yield from`, the consuming calls and lazy wrappers of `consumption`, project
    /// calls iterating them): a generator passed there has its body run there. `next()` runs a
    /// generator only to its first `yield` and does not count.
    pub iterates_params: u64,
    /// `param index -> paths`: `""` = container mutation of the argument itself,
    /// `"a"` = write to / mutation of the argument's attribute `a`, [`ELEMENTS_PATH`] = a change
    /// of an object the argument holds as an item or an element, or of one below it.
    pub mutated_param_paths: BTreeMap<u8, BTreeSet<String>>,
    /// Parameters (bit `i`) whose entries the function changes as part of the container, itself
    /// or through its calls: in place (`p["k"].add(v)`, `for row in p: row.clear()`), or by an
    /// attribute write that heki ties to no project class (`p[0].name = v` with `p: list`), also
    /// below an entry through items and such attributes. An attribute write on an entry of a
    /// project class (`for f in p: f.name = v` with `p: list[Field]`, or a call handing the entry
    /// to a function typing it) is that class's write instead. A helper handed a typed field's
    /// container writes the field when it so changes an entry ([`WriteKind::IndirectItemMut`]).
    /// A subset of the [`ELEMENTS_PATH`] changes, only with an ownership contract
    /// ([`Contracts::ownership`]).
    #[serde(default, skip_serializing_if = "no_params")]
    pub entry_change_params: u64,
    /// `param index -> paths` of `mutated_param_paths` that every change, in the function and
    /// through its calls, only fills a memo or initializes lazily
    /// ([`cache_flow::SelfAttrWrite::fills_memo`], `lazy_init`): an in-place change of an
    /// attribute named as a memo or cache (`p._cache[k] = v`, also by a call changing the object
    /// it holds: `p._cache.set(k, v)`), a memo-shaped store
    /// (`setdefault`, a store under `if k not in p.<attr>:`), or a store of `p.<attr>` where a
    /// `None` / falsy test of it found it unset. What the object computes stays the same
    /// (`MUT202`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memo_param_paths: Option<Box<BTreeMap<u8, BTreeSet<String>>>>,
    /// `param index -> attributes` of the argument whose objects are changed, in place or
    /// deeper (`p.a.append(...)`, `p.a.b = v`), also through local copies and calls: the
    /// `mutated_param_paths` that are not just a rebinding of the attribute (`p.a = v`), and
    /// [`ELEMENTS_PATH`] when objects it holds as items or elements are. A shallow copy of the
    /// argument shares exactly these objects (`MUT202`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub changed_param_attrs: BTreeMap<u8, BTreeSet<String>>,
    /// `param index -> spans` of the function's own changes of the argument (a local fact,
    /// never propagated: a rule walking the call graph counts each change once, in the body
    /// making it; `EFF201` `mutates-params`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub param_change_sites: BTreeMap<u8, BTreeSet<Span>>,
    /// `param index -> spans` of the function's own in-place changes of the argument itself
    /// (`p.append(x)`, `p[k] = v`, also through a local alias) where its value is a builtin
    /// container (`list`, `dict`, `set`, `bytearray`, `deque`, ...) by annotation, construction,
    /// or a type test: the changes `MUT110` names. A local fact, never propagated.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub param_inplace_sites: BTreeMap<u8, BTreeSet<Span>>,
    /// The `param_inplace_sites` of parameters annotated read-only
    /// (`FunctionDecl::readonly_params`) where the changed value is one a call handed back as
    /// itself (`cast(list, items).append(1)`): typed by that call, it is no type checker's
    /// report (`MUT110`). A local fact, never propagated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retyped_param_changes: Option<Box<BTreeMap<u8, BTreeSet<Span>>>>,
    /// `param index -> slots`: the containers held in these slots of the argument itself
    /// (`p.a`, `p["k"]`) are changed in place (`p.a.append(...)`, `p["k"][i] = v`), also through
    /// local copies and calls. Rebinding a slot, `setattr`, and `__dict__` writes do not count,
    /// nor do changes deeper inside (`p.a.b.append(...)`) (`MUT108`).
    pub inplace_param_slots: BTreeMap<u8, BTreeSet<Slot>>,
    pub reads_self_attrs: BTreeSet<AttrKeyPath>,
    /// `@cached_property` getters only: the `self` attributes whose objects the result holds by
    /// reference and that the getter reads for nothing else. Every read of `self.<attr>` in its
    /// body is returned as itself or put, as itself, into a display or a container that is
    /// returned (`{"k": self.a}`, `(x, self.a)`, `d["k"] = self.a`); no `self` method or
    /// property it calls reads the attribute, except one whose result it discards that writes
    /// nothing reaching the result (`self.finalize()` before `return self.tasks`), and it hands
    /// `self` to no other code. An in-place change of such an attribute is seen through the
    /// cached value (`MUT101`).
    pub returns_self_attrs: BTreeSet<String>,
    /// The `self` attributes the function reads only where they are initialized lazily: by
    /// itself (it writes `self.<attr>` where a `None` / falsy test of it found it unset:
    /// `if self._conn is None: self._conn = connect()`), or by the `self` methods and
    /// properties it calls, none of which reads the attribute otherwise. Running the function
    /// leaves such an attribute set (`MUT101`).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub lazily_read_self_attrs: BTreeSet<String>,
    /// `__hash__` methods only: the `self` attributes whose value every read hands, as itself,
    /// to `hash()` (alone or as an element of a tuple display it hashes) or calls `__hash__`
    /// on, and that no `self` method or property it calls reads; it hands `self` to no other
    /// code. The hash is then the value's own: changing that value in place (an item store, a
    /// mutator method) changes it only through the value's own `__hash__`, which its class
    /// answers for, and a builtin container that can change in place cannot be hashed at all
    /// (`MUT107`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hashed_whole_self_attrs: Option<Box<BTreeSet<String>>>,
    pub mutates_self_attrs: BTreeSet<AttrKeyPath>,
    /// `MUT113`: what the function changes in place of its instance's containers and how it
    /// iterates them and the objects its project calls return (`None` when it does none of
    /// these, as most functions; read through [`Self::iteration_facts`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iterations: Option<Box<IterationFacts>>,
    pub invalidates_cached: BTreeSet<String>,
    /// What `MUT101`'s exact claim reads of the function ([`KeptFacts`]), boxed as it is
    /// computed only while an enabled rule reads it ([`OptInFacts::kept_values`]; `MUT101`).
    /// `None` when every fact is empty, so that two summaries of the same facts are equal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept: Option<Box<KeptFacts>>,
    /// The facts of every typed write the function reaches, through any callee.
    pub typed_field_writes: BTreeSet<TypedFieldWrite>,
    /// The function's own writes of project-class attributes, the `IndirectHelper` writes of
    /// its own call sites, and the write sites of its private callees ([`delegates_to_callers`]):
    /// a private helper hands its writes to its callers, a public function reports its own
    /// (`MUT201` reports at the outermost function of a chain of private helpers, `EFF201`
    /// walks the call graph itself), so a public callee's write sites are not copied into its
    /// callers; [`Self::typed_field_writes`] still has their facts.
    pub typed_field_write_sites: BTreeSet<TypedFieldWriteSite>,
    /// The function's own reads of project-class attributes (`MUT203`; never propagated).
    pub typed_field_read_sites: BTreeSet<TypedFieldReadSite>,
    /// The function's own comparisons by identity of a project-class attribute with an
    /// `object()` sentinel (`MUT118`; never propagated). Boxed: few functions have any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sentinel_tests: Option<Box<BTreeSet<SentinelTest>>>,
    /// The function's own `copy.deepcopy` calls of one argument of a project class (`MUT118`;
    /// never propagated). Boxed: few functions have any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deep_copies: Option<Box<BTreeSet<DeepCopySite>>>,
    /// Names of the private methods the function calls on any receiver (`obj._name(...)`),
    /// resolved or not: `MUT107` leaves the methods a class family's initializers call this
    /// way to initialization. A local fact: never propagated.
    pub private_calls: BTreeSet<String>,
    /// The function may return a value shared with a read-only producer's other callers: it is a
    /// producer itself (`MUT202`; seeded by the declared producers it returns from, propagated
    /// from the project callees whose result it returns). Boxed: few functions have one.
    pub returns_protected: Option<Box<ProtectedReturn>>,
    /// `invalidation` contracts (bits) whose invalidators every normal path of the function
    /// reaches, directly or through calls (`MUT208`; propagated).
    pub must_invalidate: u64,
    /// Writes to `invalidation` targets that some normal path leaves unannounced: the function's
    /// own, and those the private helpers it calls pass up (`MUT208`; propagated).
    pub unmet_invalidation: BTreeSet<UnmetWrite>,
    /// Parameters the function may keep beyond the call: stored in an object, a container, a
    /// global, or a closure cell, yielded, captured by a closure, handed to code heki does not
    /// follow, or to a project function that keeps them. Reading, writing, or passing a
    /// parameter to project functions that do not keep it keeps nothing (`fresh-objects`).
    /// Changed through [`Self::keep_params`].
    pub retains_params: u64,
    /// The parameters of [`Self::retains_params`] the function keeps only where tests of its
    /// inputs pass, with those tests; one without an entry is kept whatever is passed. `None`
    /// without any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retains_given: Option<Box<BTreeMap<u8, MayGiven>>>,
    /// The positions (bit `i`: parameter `i`, then the captured values of
    /// `FunctionDecl::captured_params`) whose object the function only reads, leaving changes of
    /// the object itself to [`Self::mutated_param_paths`] ([`Self::only_read_mask`] joins the
    /// two): no value that may be the object, or a view of it (an iterator, a `dict` view, a
    /// generator over it), is stored (an attribute, an item, a global, a closure cell, a
    /// display), captured by a nested function, yielded, returned, or handed to a call other than
    /// a project call whose targets are every implementation it can run, binding it to a
    /// parameter, not a constructor, a library call Layer 1 lists as reading that argument, or a
    /// cast handing it back as itself (`ownership::handed_back`, whose result is the object), nor
    /// to the special method of an operator's other operand that is not a builtin's; no item
    /// read of a `defaultdict`, whose `__missing__` inserts the key; and every project function
    /// it is handed to only reads it there. Inside a call cycle the
    /// greatest fixpoint. `0` while [`OptInFacts::only_read`] is off, and for a function heki does
    /// not model (a stub's, one with an A1 hazard).
    pub only_read_params: u64,
    /// The parameters annotated with a builtin list, dict, or set, or a mutable abstract
    /// container, and what the function's own body does with them ([`ContainerParams`]); `None`
    /// for a function without one, and while [`OptInFacts::only_read`] is off. A local fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_params: Option<Box<ContainerParams>>,
    /// The kinds of read that reach the object of each position ([`ReadKinds`]): the
    /// function's own reads of a value that may be it or a view of it, joined with those of
    /// every project function it hands the object to, as [`Self::only_read_params`] is (a
    /// position outside it may have fewer). A position without one is absent. `None` without
    /// any, and while [`OptInFacts::only_read`] is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_kinds: Option<Box<BTreeMap<u8, ReadKinds>>>,
    /// The function returns a new object that nothing else holds (`fresh-objects`).
    pub returns_new: ReturnsNew,
    /// `MUT112`: what the object the function returns may be, path by path: a new one, state
    /// that outlives the call, one of its parameters, or an element of one, propagated from the
    /// project callees whose result it returns, through the calls whose results their arguments
    /// may be ([`ArgOrigin::origins`]). `None` when the function returns nothing `MUT112` knows
    /// of, as every function while `MUT112` does not run; read through
    /// [`Self::return_origin_facts`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_origins: Option<Box<ReturnOrigins>>,
    /// The function's own writes heki cannot tie to a class (`MUT205`; never propagated).
    pub unresolved_write_sites: BTreeSet<UnresolvedWriteSite>,
    /// The function's own reads and calls heki cannot tie to a class (`MUT206`, `MUT207`; never
    /// propagated).
    pub unresolved_access_sites: BTreeSet<UnresolvedAccessSite>,
    /// The `EFF109` facts: the non-reentrant locks the function takes and the names it may
    /// bind (`None` when it does neither, as most functions; read through
    /// [`Self::lock_facts`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locks: Option<Box<LockFacts>>,
    /// The `EFF110` facts: the objects allowing one operation at a time the function uses when
    /// its body runs, itself or through the calls that run there on its task, in its own names,
    /// unless it holds a lock there (`None` when it uses none, as most functions; read through
    /// [`Self::exclusive_uses`]; propagated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusive: Option<Box<BTreeMap<ObjRef, ExclusiveUse>>>,
    /// The `EFF117` facts: the thread locks the function returns holding and its writes of
    /// shared state made holding one (`None` when it has neither, as most functions, and while
    /// the rule does not run: [`OptInFacts::guarded`]; local).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guarded: Option<Box<guarded::GuardFacts>>,

    // Async offload & Sync-over-Async relaxation (dcg empirical findings)
    pub offloads_blocking_params: u64,
    pub sync_driven_only: bool,
    /// A call in this function (not its callees) could not be resolved, CHA exceeded
    /// `resolver::CHA_CANDIDATE_LIMIT` candidates, or the function has an A1 hazard (`eval`,
    /// `exec`, `locals()`, frame introspection, a body too large to model).
    pub unknown_calls: bool,
    /// The module globals and closure cells the function changes; `None` for the many
    /// functions that change none and whose nested functions capture no new container. Shared:
    /// a final summary holds its local summary's facts until propagation changes them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_state: Option<Arc<SharedStateFacts>>,

    /// What the function contributes to registration contexts (only with
    /// `OptInFacts::dispatch`: `heki inspect` and the language server): local, never propagated
    /// to callers, and read by no rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch: Option<Arc<dispatch::DispatchFacts>>,

    /// What iterating the object this plain function returns runs: the bodies of the
    /// generators it returns (directly, through lazy wrappers, or through the functions whose
    /// results it returns), in terms of its own parameters. Never set for a generator
    /// function: iterating its result runs its own body, which these fields describe.
    pub iterated: Option<Box<EffectSummary>>,

    // Function metadata
    pub is_async: bool,
    /// A generator function (`FunctionDecl::is_generator`): calling it only creates the
    /// generator; its body runs where the generator is iterated.
    pub is_generator: bool,
    pub is_cached_property: bool,
    pub is_lru_cache: bool,
    /// What running the function's body runs of a call cycle (`EFF114`; propagated, computed
    /// only while the rule runs: [`OptInFacts::call_cycles`]; `None`: none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_cycle: Option<CallCycle>,
    /// A `functools` cache keeps the function's results, each a builtin container of one kind
    /// (`MUT114`; a local fact, never propagated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_result: Option<readonly::CachedResult>,
    /// Module-relative class qualname of a method (`Outer.Inner`), as in `IrFunction`.
    pub owner_class_qualname: Option<String>,
    pub decorators: Vec<String>,
    /// A decorator may not run the body on every call or may hand out another object than it
    /// returns: not every decorator is bound, in the function's module, to one that keeps the
    /// result (`decorators::keeps_returned_value`). Callers read it here, without the module's
    /// bindings (`MustGap::Decorated`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub decorator_may_replace: bool,

    /// The guards every occurrence of an execution effect depends on (`guards`); an effect
    /// without an entry runs whenever the function does.
    pub exec_guards: BTreeMap<ExecEffectKind, EffectGuards>,

    /// Execution effects an ecosystem entry naming the function keeps from its callers: every
    /// one when it is `excluded`, the blocking ones when it is `async_safe` (applied to the
    /// final summary, whatever the callees bring).
    pub declared_hidden: BTreeSet<ExecEffectKind>,

    /// The function's own waits on queues whose capacity what calls of other functions bind
    /// decides, such as an attribute a constructor's parameter fills ([`capacity`]): propagation
    /// adds their effects where the project shows the queue bounded (local).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capacity_waits: Vec<CapacityWait>,

    /// What the function does with set iteration order (`EFF112`; `None` when it does nothing
    /// with one, as most functions, and while the rule does not run:
    /// [`OptInFacts::set_order`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_order: Option<Box<order_flow::SetOrderFacts>>,
    /// What the contracts with `include-overrides` say of the function as an override
    /// ([`Contracts::override_roles`]; a local fact, `None` while they say nothing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_roles: Option<Box<OverrideRoles>>,
}

/// The shared state a function changes ([`EffectSummary::shared_state`]). The lists are sorted
/// and hold each entry once ([`SharedStateFacts::normalize`]); most hold one or two entries, so
/// they are vectors of their exact length.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedStateFacts {
    /// The module globals and the closure cells of enclosing functions the function's own body
    /// rebinds, deletes, or changes in place (an object reached from one through attributes,
    /// items, iteration, and element accessors: `CACHE.clear()`, `REGISTRY[k] = v`,
    /// `settings.DEBUG = True`), each with the span of its first change. A local fact, never
    /// propagated.
    pub changed: Vec<(SharedState, Span)>,
    /// The function's body, or a body that runs in it, changes a module global (propagated).
    pub globals: bool,
    /// The closure cells the function's body, or a body that runs in it, changes, except those
    /// of its own variables that stay inside its call: a variable it rebinds through a nested
    /// function, or one it binds only to new containers (`new_cells`; propagated).
    pub cells: Vec<CellChange>,
    /// The function's variables that nested functions capture and that it binds only to new
    /// containers (`out = []`): a nested function changing them in place changes an object of
    /// this call. A local fact.
    pub new_cells: Vec<String>,
    /// The shared state the function's calls hand to project functions, as an argument or as
    /// the receiver, by call: changed when the callee changes that parameter
    /// ([`EffectSummary::changes_argument`]). A local fact.
    pub handed: Vec<HandedState>,
}

impl SharedStateFacts {
    fn is_empty(&self) -> bool {
        self.changed.is_empty()
            && !self.globals
            && self.cells.is_empty()
            && self.new_cells.is_empty()
            && self.handed.is_empty()
    }

    /// Sort the lists, keep each entry once (the first change of each state), and drop their
    /// spare room.
    pub(crate) fn normalize(&mut self) {
        self.changed
            .sort_by(|(a, x), (b, y)| a.cmp(b).then(x.start.cmp(&y.start)));
        self.changed.dedup_by(|later, first| later.0 == first.0);
        self.cells.sort();
        self.cells.dedup();
        self.new_cells.sort();
        self.new_cells.dedup();
        self.handed.sort();
        self.handed.dedup();
        self.changed.shrink_to_fit();
        self.cells.shrink_to_fit();
        self.new_cells.shrink_to_fit();
        self.handed.shrink_to_fit();
    }
}

/// Shared state handed to a callee: the object of `state` (or one reached from it) is
/// argument `param` of callee `callee` (its index among the call's project callees) at the
/// call at `site` (`helper(CACHE)`, `registry.add(x)` on a module-level object).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HandedState {
    pub site: Span,
    pub callee: usize,
    pub param: u8,
    pub state: SharedState,
}

/// State that outlives a call: a module global, or the closure cell of a variable of an
/// enclosing function ([`SharedStateFacts::changed`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharedState {
    Global(String),
    Cell(CellChange),
}

/// A change of the closure cell of variable `name` of the function `owner` (a qualname in the
/// changing function's module), made by a function nested in it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CellChange {
    pub owner: String,
    pub name: String,
    /// The variable is rebound (`nonlocal n; n += 1`), not its object changed in place.
    pub rebinds: bool,
}

/// An input of a function that a guard tests: parameter `param` itself, or its attribute
/// `attr` (one the function never assigns).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GuardSubject {
    pub param: u8,
    pub attr: Option<String>,
}

/// What a guard tests about its subject.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardTest {
    /// `if x:`.
    Truthy,
    /// `if x is None:` (`x == None`).
    IsNone,
    /// `isinstance(x, C)` / `isinstance(x, (C, D))`.
    IsInstance(InstanceTest),
    /// `hasattr(x, "name")` with a string literal.
    HasAttr(String),
    /// What the catalog says a call the function makes with the value does ([`ValueTest`]).
    Value(Box<ValueTest>),
}

impl GuardTest {
    /// The attribute a `hasattr` test names.
    pub fn attr(&self) -> Option<&str> {
        match self {
            Self::HasAttr(name) => Some(name),
            _ => None,
        }
    }
}

/// The classes an `isinstance` test names ([`GuardTest::IsInstance`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct InstanceTest {
    /// The fully qualified classes, sorted.
    pub classes: Vec<String>,
    /// Every class is a project class whose MRO holds only project classes and `object` and
    /// names no metaclass: an object is an instance of one exactly when one is on its class's
    /// MRO (no ABC registration, protocol, or `__instancecheck__` answers otherwise), so an
    /// instance of exactly a class without them on its MRO fails the test.
    pub nominal: bool,
}

/// A test of a value by the catalog's facts on a call made with it, for one effect of that
/// call ([`GuardTest::Value`]): a caller binding the value decides it from what it knows of the
/// argument ([`ArgBinding::fact`]), as the call decides an argument of its own.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueTest {
    /// The value is the argument the call's cases test: the first case it passes gives the
    /// outcome, `otherwise` when it passes none. The cases keep only their tests.
    Cases {
        cases: Vec<(heki_kb::ArgCase, bool)>,
        otherwise: bool,
    },
    /// The value is the call's receiver, or the object the receiver was returned from by calls
    /// of catalog methods: an instance of one of these catalog classes (the states of the
    /// library class the call names, `heki_kb::EffectiveEcosystemKb::states_of`) gives the
    /// outcome beside it. Of any other class the outcome is not known.
    States(Vec<(String, bool)>),
}

/// A test of an input, and the outcome (`holds`) an effect needs to run.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct InputGuard {
    pub subject: GuardSubject,
    pub test: GuardTest,
    pub holds: bool,
}

/// The guards common to every occurrence of one execution effect in a function.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGuards {
    /// Tests of the function's inputs that every occurrence needs.
    pub inputs: BTreeSet<InputGuard>,
    /// Lazy initialization: every occurrence runs only while an attribute of an object (or a
    /// module global) is unset, and the function then sets it, or in a
    /// `functools.cached_property` getter, so it runs at most once per object. The objects come
    /// from the parameters in this mask (`0`: from none, such as a pooled connection or a module
    /// global). `None`: some occurrence runs on every call.
    pub once_per: Option<u64>,
    /// Every occurrence waits on the queue the function's parameter `p` holds, which it does
    /// only when the queue is bounded: each call decides by what it binds there ([`capacity`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounded_queue: Option<u8>,
}

/// The exact class of a value a function knows: a constant, a display or comprehension, or a
/// buffer it constructs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstClass {
    None,
    Bool,
    Int,
    Float,
    Str,
    Bytes,
    List,
    Tuple,
    Set,
    Dict,
    /// `io.StringIO(...)`.
    StringIo,
    /// `io.BytesIO(...)`.
    BytesIo,
    /// `...`, a complex number.
    Other,
}

/// An argument (or a literal default) whose class the caller knows: its class and, where the
/// value decides it, its truthiness (`False`, `None`, `0`, `""`, and empty displays are falsy)
/// and, for an integer, whether it is above zero (a default may be negative: `size=-1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ConstArg {
    pub class: ConstClass,
    pub truthy: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub positive: Option<bool>,
}

/// A constant a call passes for a parameter ([`CallSite::arguments`]), in the kinds a call
/// target's argument list names.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ArgValue {
    None,
    Bool(bool),
    Int(i64),
    Str(String),
}

impl ArgValue {
    /// The value of a literal (`None` for a literal of another kind).
    pub fn of_literal(literal: &heki_ir::ConstLiteral) -> Option<Self> {
        use heki_ir::ConstLiteral;
        match literal {
            ConstLiteral::None => Some(Self::None),
            ConstLiteral::Bool(b) => Some(Self::Bool(*b)),
            ConstLiteral::Int(n) => Some(Self::Int(*n)),
            ConstLiteral::Str(s) => Some(Self::Str(s.clone())),
            _ => None,
        }
    }

    /// The value a constant's class and truthiness decide: `None`, `True`, `False`, `0`, `""`.
    pub fn decided(c: &ConstArg) -> Option<Self> {
        match (c.class, c.truthy) {
            (ConstClass::None, _) => Some(Self::None),
            (ConstClass::Bool, Some(b)) => Some(Self::Bool(b)),
            (ConstClass::Int, Some(false)) => Some(Self::Int(0)),
            (ConstClass::Str, Some(false)) => Some(Self::Str(String::new())),
            _ => None,
        }
    }
}

/// [`CallSite::arguments`]: each parameter a call target's argument list names
/// ([`Contracts::marked_params`]) that the call binds to a constant it establishes, with it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArgumentValues(pub Vec<(String, ArgValue)>);

impl ArgumentValues {
    /// The constant passed for parameter `name`.
    pub fn get(&self, name: &str) -> Option<&ArgValue> {
        self.0.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }
}

/// What a caller knows about the value of a callee parameter at one call, when the argument
/// binds the parameter exactly: the receiver, a keyword, a position before any `*` unpacking,
/// or the literal default of an omitted parameter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArgBinding {
    /// What the caller knows of the value, kept when it can decide a test of the callee: a
    /// constant, a value of a class it knows exactly ([`ConstArg`]), or a class, flag, or file
    /// name a catalog case or method tells apart ([`GuardTest::Value`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fact: Option<Box<heki_kb::ArgFact>>,
    /// The caller's own input the argument is (a guard subject of the caller).
    pub input: Option<GuardSubject>,
    /// The caller changes that input in place somewhere ([`GuardInputs::changed_inputs`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub input_changed: bool,
    /// Caller parameters the argument may come from: itself, its attributes and elements, and
    /// the results of calls on them.
    pub derived_params: u64,
    /// The argument may be an object the caller constructs, or come from one.
    pub derived_fresh: bool,
    /// The loops around the call (bit `d - 1`: the one at depth `d`) across whose iterations
    /// the argument stays one object: those that do not construct it.
    pub stays_in_loops: u64,
    /// The argument may be computed from the caller's parameters (`resolver::from_parameter`):
    /// a case of a catalog call its value selects is the caller's callers' to decide (R6).
    /// Computed while an enabled rule reads [`EffectSummary::must_exec`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_params: bool,
}

impl ArgBinding {
    /// The argument's constant view ([`ConstArg::of_fact`]).
    pub fn constant(&self) -> Option<ConstArg> {
        self.fact.as_deref().and_then(ConstArg::of_fact)
    }

    /// The string constant the argument is, for the item writes a callee keys by the parameter
    /// ([`EntryKey::Param`]).
    pub fn text(&self) -> Option<&str> {
        match self.fact.as_deref()?.literal.as_ref()? {
            heki_kb::LiteralValue::Str(s) => Some(s),
            _ => None,
        }
    }
}

impl EffectSummary {
    /// The tests under which the function keeps its parameter `p` ([`Self::retains_given`]):
    /// never, for a parameter it does not keep.
    pub fn retained_given(&self, p: u8) -> MayGiven {
        // A parameter past the 64 the mask holds is taken to be kept whatever is passed.
        if (self.retains_params.checked_shr(p.into())).is_some_and(|rest| rest & 1 == 0) {
            return MayGiven::default();
        }
        (self.retains_given.as_deref().and_then(|g| g.get(&p)))
            .map_or_else(|| MayGiven::given(Vec::new()), Clone::clone)
    }

    /// Keep the parameters of `mask` where `given` holds, besides where they are kept already.
    pub(crate) fn keep_params(&mut self, mask: u64, given: &MayGiven) {
        if given.never() {
            return;
        }
        for p in (0..64u8).filter(|p| mask & (1u64 << p) != 0) {
            let had = self.retained_given(p);
            let mut now = had.clone();
            now.join(given);
            self.retains_params |= 1u64 << p;
            if now == had {
                continue;
            }
            let entries = self.retains_given.get_or_insert_with(Box::default);
            if now.always() {
                entries.remove(&p);
            } else {
                entries.insert(p, now);
            }
            if entries.is_empty() {
                self.retains_given = None;
            }
        }
    }

    /// [`KeptFacts::must_read_self_attrs`] (none while the facts are not computed).
    pub fn must_read_self_attrs(&self) -> &BTreeSet<AttrKeyPath> {
        static NONE: BTreeSet<AttrKeyPath> = BTreeSet::new();
        self.kept
            .as_ref()
            .map_or(&NONE, |k| &k.must_read_self_attrs)
    }

    /// [`KeptFacts::removes_kept`] (nothing while the facts are not computed).
    pub fn removes_kept(&self) -> &KeptRemoval {
        static NOTHING: KeptRemoval = KeptRemoval::Nothing;
        self.kept.as_ref().map_or(&NOTHING, |k| &k.removes_kept)
    }

    /// [`KeptFacts::listed_cache`].
    pub fn listed_cache(&self) -> Option<ListedCache> {
        self.kept.as_ref().and_then(|k| k.listed_cache)
    }

    /// The [`KeptFacts`] to add to, made on the first need.
    pub(crate) fn kept_mut(&mut self) -> &mut KeptFacts {
        self.kept.get_or_insert_with(Box::default)
    }

    /// Adds `removes` to [`KeptFacts::removes_kept`], making the facts only when it removes
    /// something.
    pub(crate) fn join_removes_kept(&mut self, removes: &KeptRemoval) {
        if *removes != KeptRemoval::Nothing {
            self.kept_mut().removes_kept.join(removes);
        }
    }

    /// Joins the [`KeptFacts`] of another definition bound to the same name: a call runs one of
    /// them, so the function reads on every path what both read, and removes what either does.
    pub(crate) fn merge_kept(&mut self, other: Option<&KeptFacts>) {
        let mut kept = self.kept.take().unwrap_or_default();
        kept.must_read_self_attrs = match other {
            Some(other) => callgraph::must_reads_of_both(
                &kept.must_read_self_attrs,
                &other.must_read_self_attrs,
            ),
            None => BTreeSet::new(),
        };
        if let Some(other) = other {
            kept.removes_kept.join(&other.removes_kept);
        }
        self.kept = (*kept != KeptFacts::default()).then_some(kept);
    }

    /// [`SharedStateFacts::changed`].
    pub fn changed_state(&self) -> &[(SharedState, Span)] {
        self.shared_state.as_ref().map_or(&[], |f| &f.changed)
    }

    /// [`SharedStateFacts::globals`].
    pub fn changes_globals(&self) -> bool {
        self.shared_state.as_ref().is_some_and(|f| f.globals)
    }

    /// [`SharedStateFacts::cells`].
    pub fn changes_cells(&self) -> &[CellChange] {
        self.shared_state.as_ref().map_or(&[], |f| &f.cells)
    }

    /// [`SharedStateFacts::new_cells`].
    pub fn new_cells(&self) -> &[String] {
        self.shared_state.as_ref().map_or(&[], |f| &f.new_cells)
    }

    /// The shared state handed at the call at `site` to its project callee `callee`
    /// ([`SharedStateFacts::handed`]).
    pub fn handed_at(&self, site: Span, callee: usize) -> impl Iterator<Item = &HandedState> {
        let handed = self
            .shared_state
            .as_ref()
            .map_or(&[][..], |f| &f.handed[..]);
        let from = handed.partition_point(|h| h.site.start < site.start);
        handed[from..]
            .iter()
            .take_while(move |h| h.site.start == site.start)
            .filter(move |h| h.site == site && h.callee == callee)
    }

    /// Whether the function changes the object its parameter `j` is: in place, an attribute
    /// of it, or a container held in one of its slots.
    pub fn changes_argument(&self, j: u8) -> bool {
        self.mutated_param_paths.contains_key(&j)
            || self
                .inplace_param_slots
                .get(&j)
                .is_some_and(|s| !s.is_empty())
    }

    /// Change the shared-state facts with `change` and normalize them, keeping `None` when they
    /// stay empty (so that a summary without them equals one that never had them).
    pub(crate) fn change_shared_state(&mut self, change: impl FnOnce(&mut SharedStateFacts)) {
        let mut facts = self.shared_state.take().unwrap_or_default();
        let f = Arc::make_mut(&mut facts);
        change(f);
        f.normalize();
        if !f.is_empty() {
            self.shared_state = Some(facts);
        }
    }

    /// Change the shared-state facts while the function's body is read; the caller normalizes
    /// them once at the end ([`SharedStateFacts::normalize`]).
    pub(crate) fn shared_state_mut(&mut self) -> &mut SharedStateFacts {
        Arc::make_mut(self.shared_state.get_or_insert_with(Arc::default))
    }

    /// Drop the execution effects in `hidden`, with their chains and guards, here and in what
    /// iterating the returned object runs.
    pub(crate) fn hide_exec_effects(&mut self, hidden: &BTreeSet<ExecEffectKind>) {
        if hidden.is_empty() {
            return;
        }
        self.direct_exec_effects.retain(|e| !hidden.contains(e));
        self.transitive_exec_effects.retain(|e| !hidden.contains(e));
        self.exec_chains.retain(|e, _| !hidden.contains(e));
        if let Some(w) = &mut self.witness_ways {
            w.retain(|e, _| !hidden.contains(e));
            if w.is_empty() {
                self.witness_ways = None;
            }
        }
        if let Some(r) = &mut self.provided_effects {
            r.retain(|e, _| !hidden.contains(e));
            if r.is_empty() {
                self.provided_effects = None;
            }
        }
        self.exec_guards.retain(|e, _| !hidden.contains(e));
        if let Some(must) = &mut self.must_exec {
            must.effects.retain(|e| !hidden.contains(e));
            must.alternatives.retain(|e| !hidden.contains(e));
            must.given.retain(|c| !hidden.contains(&c.effect));
        }
        if let Some(iterated) = &mut self.iterated {
            iterated.hide_exec_effects(hidden);
        }
    }

    /// Copy with every source location zeroed: Early Cutoff compares facts, not positions,
    /// so an edit that only moves code does not notify callers.
    pub fn without_locations(&self) -> Self {
        let mut out = self.clone();
        if let Some(d) = &mut out.dispatch {
            *d = Arc::new(d.without_locations());
        }
        for chain in out.exec_chains.values_mut() {
            zero_hop_spans(chain);
        }
        for chain in out.must_exec.iter_mut().flat_map(|m| {
            m.effects
                .values_mut()
                .chain(m.alternatives.values_mut())
                .chain(m.given.chains_mut())
        }) {
            zero_hop_spans(chain);
        }
        for provided in out
            .provided_effects
            .iter_mut()
            .flat_map(|r| r.values_mut())
            .flatten()
        {
            zero_hop_spans(&mut provided.chain);
        }
        for way in out
            .witness_ways
            .iter_mut()
            .flat_map(|w| w.values_mut())
            .flatten()
        {
            zero_hop_spans(&mut way.chain);
        }
        out.typed_field_write_sites = out
            .typed_field_write_sites
            .into_iter()
            .map(|mut site| {
                site.span = Span { start: 0, end: 0 };
                zero_hop_spans(&mut site.via_chain);
                site
            })
            .collect();
        out.typed_field_read_sites = out
            .typed_field_read_sites
            .into_iter()
            .map(|mut site| {
                site.span = Span { start: 0, end: 0 };
                site
            })
            .collect();
        out.sentinel_tests = out.sentinel_tests.map(|tests| {
            Box::new(
                tests
                    .into_iter()
                    .map(|mut t| {
                        t.span = Span { start: 0, end: 0 };
                        t
                    })
                    .collect(),
            )
        });
        out.deep_copies = out.deep_copies.map(|sites| {
            Box::new(
                sites
                    .into_iter()
                    .map(|mut d| {
                        d.span = Span { start: 0, end: 0 };
                        d
                    })
                    .collect(),
            )
        });
        out.param_change_sites.clear();
        if let Some(f) = &mut out.shared_state {
            let f = Arc::make_mut(f);
            for (_, span) in &mut f.changed {
                *span = Span { start: 0, end: 0 };
            }
            for h in &mut f.handed {
                h.site = Span { start: 0, end: 0 };
            }
        }
        out.param_inplace_sites.clear();
        out.retyped_param_changes = None;
        out.guarded = out.guarded.map(|g| Box::new(g.without_locations()));
        for w in &mut out.capacity_waits {
            w.hop.span = Span { start: 0, end: 0 };
        }
        out.iterations = out.iterations.map(|f| Box::new(f.without_locations()));
        out.locks = out.locks.map(|l| Box::new(l.without_locations()));
        out.exclusive =
            (out.exclusive).map(|uses| Box::new(one_at_a_time::without_locations(&uses)));
        out.iterated = out.iterated.map(|i| Box::new(i.without_locations()));
        if let Some(r) = &mut out.returns_protected {
            zero_hop_spans(&mut r.chain);
        }
        out.unresolved_write_sites = out
            .unresolved_write_sites
            .into_iter()
            .map(|mut site| {
                site.span = Span { start: 0, end: 0 };
                site
            })
            .collect();
        out.unresolved_access_sites = out
            .unresolved_access_sites
            .into_iter()
            .map(|mut site| {
                site.span = Span { start: 0, end: 0 };
                site
            })
            .collect();
        out.unmet_invalidation = out
            .unmet_invalidation
            .into_iter()
            .map(|mut w| {
                w.span = Span { start: 0, end: 0 };
                w.origin = Span { start: 0, end: 0 };
                zero_hop_spans(&mut w.via_chain);
                w
            })
            .collect();
        out
    }

    /// Location-insensitive equality used by Early Cutoff.
    pub fn same_facts(&self, other: &Self) -> bool {
        self.without_locations() == other.without_locations()
    }

    /// [`Self::same_facts`] of everything but [`Self::dispatch`], which only the join over the
    /// whole index reads (`dispatch::contexts`), never a caller.
    pub fn same_facts_for_callers(&self, other: &Self) -> bool {
        let without = |s: &Self| Self {
            dispatch: None,
            ..s.without_locations()
        };
        without(self) == without(other)
    }

    /// Equality of everything callers copy into their own summaries: the facts, and the
    /// positions of call chains and write sites. Only the positions of the function's own read
    /// sites, parameter, container, and global changes, iterations, and unresolved accesses are
    /// left out: those are never propagated.
    pub fn same_for_callers(&self, other: &Self) -> bool {
        let without_local = |s: &Self| Self {
            typed_field_read_sites: BTreeSet::new(),
            sentinel_tests: None,
            deep_copies: None,
            param_change_sites: BTreeMap::new(),
            shared_state: s.shared_state.as_ref().and_then(|f| {
                let propagated = SharedStateFacts {
                    changed: Vec::new(),
                    handed: Vec::new(),
                    ..(**f).clone()
                };
                (!propagated.is_empty()).then(|| Arc::new(propagated))
            }),
            param_inplace_sites: BTreeMap::new(),
            retyped_param_changes: None,
            iterations: s.iterations.as_ref().and_then(|f| f.propagated()),
            unresolved_write_sites: BTreeSet::new(),
            unresolved_access_sites: BTreeSet::new(),
            dispatch: None,
            ..s.clone()
        };
        without_local(self) == without_local(other) && self.same_facts(other)
    }

    /// [`Self::sentinel_tests`], none when there are none.
    pub fn sentinel_test_sites(&self) -> impl Iterator<Item = &SentinelTest> {
        self.sentinel_tests.iter().flat_map(|t| t.iter())
    }

    /// [`Self::deep_copies`], none when there are none.
    pub fn deep_copy_sites(&self) -> impl Iterator<Item = &DeepCopySite> {
        self.deep_copies.iter().flat_map(|d| d.iter())
    }

    /// [`Self::iterations`], empty when there are none.
    pub fn iteration_facts(&self) -> &IterationFacts {
        self.iterations
            .as_deref()
            .unwrap_or(&iterations::NO_ITERATION_FACTS)
    }

    /// [`Self::iterations`], to add facts to.
    pub(crate) fn iteration_facts_mut(&mut self) -> &mut IterationFacts {
        self.iterations.get_or_insert_with(Box::default)
    }

    /// [`Self::locks`], empty when there are none.
    pub fn lock_facts(&self) -> &LockFacts {
        self.locks.as_deref().unwrap_or(&locks::NO_LOCK_FACTS)
    }

    /// [`Self::locks`], to add facts to.
    pub(crate) fn lock_facts_mut(&mut self) -> &mut LockFacts {
        self.locks.get_or_insert_with(Box::default)
    }

    /// Add the `EFF109` facts of `other` ([`LockFacts::absorb`]).
    pub(crate) fn absorb_lock_facts(&mut self, other: Option<Box<LockFacts>>) {
        if let Some(other) = other {
            self.lock_facts_mut().absorb(*other);
        }
    }

    /// [`Self::exclusive`], empty when there are none.
    pub fn exclusive_uses(&self) -> &BTreeMap<ObjRef, ExclusiveUse> {
        self.exclusive
            .as_deref()
            .unwrap_or(&one_at_a_time::NO_EXCLUSIVE_USES)
    }

    /// [`Self::exclusive`], to add uses to.
    pub(crate) fn exclusive_uses_mut(&mut self) -> &mut BTreeMap<ObjRef, ExclusiveUse> {
        self.exclusive.get_or_insert_with(Box::default)
    }

    /// Add the `EFF110` uses of another summary ([`one_at_a_time::add_use`]).
    pub(crate) fn absorb_exclusive_uses(
        &mut self,
        other: impl IntoIterator<Item = (ObjRef, ExclusiveUse)>,
    ) {
        for (obj, u) in other {
            one_at_a_time::add_use(self.exclusive_uses_mut(), obj, u);
        }
    }

    /// [`Self::return_origins`], empty when there are none.
    pub fn return_origin_facts(&self) -> &ReturnOrigins {
        self.return_origins
            .as_deref()
            .unwrap_or(&ownership::NO_RETURN_ORIGINS)
    }

    /// Add the facts of the typed write sites of `func` (this summary's function) to
    /// `typed_field_writes`, also for what iterating its result runs (propagation adds those of
    /// the callees).
    pub(crate) fn refresh_typed_field_writes(&mut self, func: &FuncKey) {
        // `initializes_own_self` of an object that may be `self`.
        let initializes = initializes_own_self(func, self, 1);
        self.add_site_facts(initializes);
        if let Some(i) = &mut self.iterated {
            i.add_site_facts(initializes);
        }
    }

    /// Add the facts of this summary's typed write sites to `typed_field_writes`; `initializes`:
    /// the function is an initializer, whose writes of what may be its `self` are fresh. Then
    /// join the objects of each write past [`WRITTEN_OBJECTS_KEPT_APART`].
    fn add_site_facts(&mut self, initializes: bool) {
        for s in &self.typed_field_write_sites {
            let fact = TypedFieldWrite {
                class_qualname: s.class_qualname.clone(),
                attr: s.attr.clone(),
                kind: s.kind,
                fresh: s.fresh || initializes && s.obj_params & 1 != 0,
                obj_params: s.obj_params,
            };
            self.typed_field_writes.insert(fact);
        }
        join_written_objects(&mut self.typed_field_writes);
    }

    /// Record that the function changes parameter `p` at `path` (`mutated_param_paths`);
    /// `memo`: this change only fills a memo or initializes lazily. A path stays in
    /// `memo_param_paths` only while every change of it does.
    pub(crate) fn note_param_change(&mut self, p: u8, path: String, memo: bool) {
        let paths = self.mutated_param_paths.entry(p).or_default();
        let all_memos = self.memo_param_paths.get_or_insert_with(Box::default);
        let memos = all_memos.entry(p).or_default();
        let changed_otherwise = paths.contains(&path) && !memos.contains(&path);
        if memo && !changed_otherwise {
            memos.insert(path.clone());
        } else {
            memos.remove(&path);
        }
        if memos.is_empty() {
            all_memos.remove(&p);
        }
        if all_memos.is_empty() {
            self.memo_param_paths = None;
        }
        paths.insert(path);
        self.mutates_params |= 1u64 << p;
    }

    /// Whether the function changes parameter `p` other than by filling a memo or initializing
    /// lazily (`memo_param_paths`); `attrs`: only below those of its attributes (and items,
    /// [`ELEMENTS_PATH`]).
    pub fn changes_param_state(&self, p: u8, attrs: Option<&BTreeSet<String>>) -> bool {
        self.changes_param_paths(p, |path| attrs.is_none_or(|a| a.contains(path)))
    }

    /// [`Self::changes_param_state`] of the argument's own object: itself or its attributes,
    /// leaving out the changes of the objects it holds as items or elements.
    pub fn changes_param_object(&self, p: u8) -> bool {
        self.changes_param_paths(p, |path| path != ELEMENTS_PATH)
    }

    fn changes_param_paths(&self, p: u8, counts: impl Fn(&str) -> bool) -> bool {
        let memos = self.memo_param_paths.as_deref().and_then(|m| m.get(&p));
        let paths = self.mutated_param_paths.get(&p).into_iter().flatten();
        paths
            .filter(|path| counts(path))
            .any(|path| memos.is_none_or(|m| !m.contains(path)))
    }

    /// Add the `self` attributes `other` reads to the ones this summary reads (its own or a
    /// `self` call's facts): an attribute stays read only lazily
    /// ([`Self::lazily_read_self_attrs`]) or only as the value hashed
    /// ([`Self::hashed_whole_self_attrs`]) when neither reads it otherwise.
    pub(crate) fn add_self_reads(&mut self, other: &EffectSummary) {
        self.lazily_read_self_attrs = read_only_as(self, other, |s| &s.lazily_read_self_attrs);
        let hashed = read_only_as(self, other, hashed_whole);
        self.hashed_whole_self_attrs = (!hashed.is_empty()).then(|| Box::new(hashed));
        self.reads_self_attrs
            .extend(other.reads_self_attrs.iter().cloned());
    }

    /// An `@abstractmethod`: a call never runs it (an override does), so it guarantees
    /// nothing a call of one of its overrides lacks.
    pub fn is_abstract(&self) -> bool {
        is_abstract_method(&self.decorators)
    }

    /// Whether a call binds the instance it is made on to the function's first parameter: a
    /// method that is neither a static nor a class method.
    pub fn binds_self(&self) -> bool {
        self.owner_class_qualname.is_some()
            && !self.decorators.iter().any(|d| {
                let head = d.split('(').next().unwrap_or(d).trim();
                matches!(
                    head.rsplit('.').next(),
                    Some("staticmethod" | "classmethod")
                )
            })
    }

    /// Which of the function's facts its own body is: a generator function's body runs when
    /// its result is iterated.
    pub fn body_facts(&self) -> Facts {
        if self.is_generator {
            Facts::Result
        } else {
            Facts::Body
        }
    }

    /// The function's facts of one kind: its body's (this summary; a generator function's body
    /// runs only when its result is iterated), or what iterating its result runs.
    pub fn facts(&self, which: Facts) -> Option<&EffectSummary> {
        match which {
            Facts::Body => (!self.is_generator).then_some(self),
            Facts::Result if self.is_generator => Some(self),
            Facts::Result => self.iterated.as_deref(),
        }
    }
}

/// The two kinds of facts a call site can hand on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Facts {
    /// What running the function's body does.
    Body,
    /// What iterating the object the function returns does: a generator function's body, or a
    /// plain function's `EffectSummary::iterated`.
    Result,
}

/// What a call site hands its caller of one project callee ([`ProjectIndex::delivery`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delivery<'a> {
    /// The callee's facts that run.
    pub from: Facts,
    /// Where they run: in the caller's body (`Body`; a generator caller's body is its
    /// `Result`), or (`Result`) when the object the caller returns is iterated.
    pub into: Facts,
    /// The project function the facts run inside (`SiteRole::Through`), which iterates the
    /// callee's result.
    pub via: Option<&'a FuncKey>,
}

/// Where a call argument comes from, as seen by the caller (used to map callee facts back).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArgOrigin {
    /// Caller parameter `i` itself (`None`) or its attribute `a` (`Some(a)`).
    pub param: Option<(u8, Option<String>)>,
    /// A slot of caller parameter `i` itself, one level deep (`p.a`, `p["k"]`).
    pub param_slot: Option<(u8, Slot)>,
    /// The caller parameters `i` whose items or elements the argument may be, or be below,
    /// each with the path a change of it is to the parameter ([`ELEMENTS_PATH`], or the
    /// attribute it is below: `p.a[0]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub param_elements: Vec<(u8, String)>,
    /// The caller parameters (bit `i`) whose entries the argument may be as part of the
    /// container ([`EffectSummary::entry_change_params`]): an item or element of the parameter
    /// or of a shallow copy of it, or below one through items and attributes of objects heki
    /// ties to no project class. Only with an ownership contract.
    #[serde(default, skip_serializing_if = "no_params")]
    pub param_entries: u64,
    /// A shallow copy of caller parameter `i` itself (`None`) or of its attribute `a`
    /// (`dict(p)`, `copy.copy(p.a)`): it shares the objects below its top level with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param_copy: Option<(u8, Option<String>)>,
    /// `self.<attr>` of the calling method.
    pub self_attr: Option<String>,
    /// `obj.<attr>` where `obj` has project class type: `(class_fqn, attr)`.
    pub typed_field: Option<(String, String)>,
    /// An instance of project class `class_fqn`.
    pub typed_instance: Option<String>,
    /// When `typed_instance` is a project protocol: the classes implementing it, which the
    /// argument is an instance of too ([`ArgOrigin::instance_classes`]). Only computed when a
    /// contract is declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implementers: Option<Implementers>,
    /// Caller parameters the argument may be, as itself (through copies and phis).
    pub may_params: u64,
    /// The argument is a value a call handed back as itself (`typing.cast(T, p)`, a project
    /// function returning its parameter): a type checker types it by that call, so it reports
    /// no mismatch of the original's annotation at this call.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retyped: bool,
    /// The argument is an instance the caller constructs (`Model(...)`), or an attribute of one.
    pub fresh: bool,
    /// The class the caller types the argument with (`resolver::Val::Inst`: a construction, a
    /// call's annotated result, a copy), with its MRO: the argument is an instance of it or of a
    /// subclass. `None` for the caller's parameter passed as itself, whose callers decide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<SharedLine>,
    /// The argument is an instance of exactly [`Self::class`], never a subclass's: the caller
    /// constructs it by calling the class by name, or it is the object a construction of the
    /// class named so binds to its `__init__`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exact: bool,
    /// Where the argument comes from among the objects that existed before the call (`MUT208`;
    /// only computed when an `invalidation` contract is configured).
    pub reach: Reach,
    /// The `invalidation` targets a callee may write through the argument without knowing them
    /// (`MUT208`; `None` unless a contract names `typed_field`, or an attribute of
    /// `typed_instance`'s class).
    pub targets: Option<Box<ArgTargets>>,
    /// What the caller knows about the parameter's value (`None`: not bound exactly).
    pub binding: Option<ArgBinding>,
    /// The argument (for `typed_field`, the object holding the attribute) is a new object of the
    /// caller, or its parameter, that nothing kept before this call (`fresh-objects`).
    pub new_object: NewArg,
    /// What the argument may be, path by path, when the caller may return the call's result as
    /// itself (`MUT112`: a callee returning its parameter returns the argument), also as the
    /// result of another project call of the caller; `None` for any other call (boxed: few
    /// arguments have one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origins: Option<Box<LocalOrigins>>,
    /// The argument is a function the callee calls through this parameter
    /// ([`resolver::FunctionDecl::calls_params`]), every function it can be known: the call's
    /// sites of handed callables run them.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub handed: bool,
}

/// [`ArgOrigin::targets`]: what the argument's class and field are to the `invalidation`
/// contracts, which propagation cannot look up (it has no class hierarchy).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArgTargets {
    /// The contracts whose targets include the field the argument is (`typed_field`): changing
    /// the value in place writes it.
    pub field: u64,
    /// Each of the [`ArgOrigin::instance_classes`], first, with its project bases, when a
    /// contract names an attribute of one of them (empty otherwise): [`Contracts::target_of`]
    /// with an attribute a callee writes on the instance, of the class the write is tied to.
    pub classes: Vec<Vec<ContractClass>>,
}

impl ArgOrigin {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The argument is the constant `None` (passed, or a default the call leaves): calling it
    /// raises `TypeError` before any function runs.
    pub fn is_none(&self) -> bool {
        let fact = self.binding.as_ref().and_then(|b| b.fact.as_deref());
        fact.is_some_and(|f| f.exact && f.class.as_deref() == Some(ConstClass::None.name()))
    }

    /// The argument as a callee run in another process gets it: a copy made by pickling, of the
    /// argument's class and value, that nothing else holds. Nothing ties it to an object of the
    /// caller.
    pub fn pickled(&self) -> Self {
        Self {
            typed_instance: self.typed_instance.clone(),
            implementers: self.implementers.clone(),
            class: self.class.clone(),
            exact: self.exact,
            binding: self.binding.clone(),
            fresh: true,
            new_object: NewArg::New {
                kind: NewKind::Copy,
                cond: NewCond::default(),
            },
            ..Self::default()
        }
    }

    /// The project classes the ownership rules tie the argument to: `typed_instance`, and the
    /// implementations of the protocol it is typed with, as for a write through the object
    /// itself (a target on the protocol and one on an implementation both cover it).
    pub fn instance_classes(&self) -> impl Iterator<Item = &String> {
        let implementers = match &self.implementers {
            Some(Implementers::Listed(classes)) => classes.as_slice(),
            _ => &[],
        };
        self.typed_instance.iter().chain(implementers)
    }

    /// Whether the argument is typed with a protocol whose implementations heki cannot list:
    /// its class is unknown.
    pub fn of_unlisted_protocol(&self) -> bool {
        self.implementers == Some(Implementers::Unlisted)
    }
}

/// The classes a protocol-typed argument is an instance of ([`ArgOrigin::implementers`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Implementers {
    /// The project classes implementing the protocol structurally (none: no project class does).
    Listed(Vec<String>),
    /// More classes than the candidate limit, or a protocol without members.
    Unlisted,
}

/// One resolved project callee with the caller-side origin of each callee parameter.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BoundCallee {
    pub callee: FuncKey,
    /// Indexed by the callee's parameter position (receiver included for bound methods).
    pub params: Vec<ArgOrigin>,
}

impl BoundCallee {
    /// The caller parameters the objects bound to the callee parameters in `mask` may be (the
    /// mapping of `obj_params` across this call).
    pub fn caller_params_of(&self, mask: u64) -> u64 {
        callgraph::bound_params(mask, self)
    }

    /// The key the call passes for parameter `p` of the callee: a string constant, the
    /// caller's own parameter, or a value heki cannot tell.
    pub fn passed_key(&self, p: u8) -> (Option<String>, EntryKey) {
        let arg = self.params.get(usize::from(p));
        if let Some(text) = arg.and_then(|a| a.binding.as_ref()?.text()) {
            return (Some(text.to_owned()), EntryKey::Written);
        }
        match arg.and_then(|a| a.param.as_ref()) {
            Some((q, None)) => (None, EntryKey::Param(*q)),
            _ => (None, EntryKey::Unknown),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CallTarget {
    /// Project functions (1 candidate, or up to `CHA_CANDIDATE_LIMIT` CHA candidates).
    Project(Vec<BoundCallee>),
    /// A Layer 2 catalog API (only recorded when it carries effects or offloads).
    Layer2 {
        /// The catalog symbol, or the library API a catalog class's method is
        /// (`heki_kb::EcosystemEntry::api`).
        symbol: String,
        /// Effects after the `is_awaited` filter.
        effects: BTreeSet<ExecEffectKind>,
        offloads_blocking_arg: Option<u8>,
        async_safe: bool,
        /// The effects the call has whatever the state and the arguments it does not show
        /// ([`effect_bit`]s; computed with `CallSite::complete`): its entry `emits` them on
        /// every call, and no argument a caller passes in can select a case of it.
        unconditional: u16,
    },
}

/// What reaching the project callees of a call site runs ([`ProjectIndex::delivery`]). A
/// call's site is a `Call`; the object a project call returns gets one more site wherever the
/// calling function iterates it, returns it, or passes it to a project call, with the call's
/// callees and parameter bindings and the location of that use.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SiteRole {
    /// The call itself: the callee's body runs here, unless calling it only creates a
    /// coroutine (an `async def` neither awaited nor driven) or a generator.
    #[default]
    Call,
    /// The object the call returns is iterated here: a `for` loop, a comprehension, unpacking,
    /// `yield from`, a consuming call (`list(...)`, `str.join`), or a lazy wrapper iterated here
    /// (`enumerate(...)`, `zip(...)`). A generator's body runs here, and what a function
    /// returning a generator hands out.
    Iterate,
    /// The object the call returns is returned by the (plain) calling function: iterating the
    /// caller's result runs it (`EffectSummary::iterated`).
    Return,
    /// The object the call returns is passed to a project call here, as parameter `.1` of its
    /// candidate `.0`, which iterates it when its body runs at its call (`iterated == false`)
    /// or when its result is iterated (`iterated == true`): here, or (`returned`) where the
    /// caller's own result is, which that result is (`relay(rows())` returned by a plain
    /// function).
    Through {
        via: Vec<(FuncKey, u8)>,
        iterated: bool,
        returned: bool,
    },
}

/// A call site retained in `FileSummary` (IR-free) for propagation and rule evaluation. Reading
/// a property (`obj.attr` whose `attr` is a getter) is one too: it calls the getter, at the read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallSite {
    pub caller: FuncKey,
    pub target: CallTarget,
    pub is_awaited: bool,
    pub span: Span,
    /// Natural-loop nesting depth of the call (`EFF102` / `EFF103`).
    pub loop_depth: u16,
    /// Receiver is the calling method's `self` (`self.helper()`, `super().m()`, `self.prop`).
    pub recv_is_self: bool,
    /// The returned coroutine is driven by hand (`.send(None)` or a `run_sync` driver).
    pub drives_coroutine_manually: bool,
    /// Fqn of the project class the call constructs (`Model(...)`, `cls(...)` in a class
    /// method). A class without a project `__init__` still gets a site, with no callees: the
    /// call ownership rules (`MUT204`) see every construction.
    pub constructs: Option<String>,
    /// What reaching the callees here runs (`Call` for every Layer 2 site).
    pub role: SiteRole,
    /// The calling function may return the call's result, through this step (`MUT202` return
    /// flow: a read-only callee makes the caller a producer).
    pub returned: Option<ReturnStep>,
    /// What follows the call on every normal path of the caller (`MUT208`; only computed when
    /// an `invalidation` contract is configured).
    pub flow: Option<SiteFlow>,
    /// Tests of the caller's inputs that hold whenever the call runs (`guards`).
    pub guards: Vec<InputGuard>,
    /// The call runs inside a lazy initialization of the caller (`EffectGuards::once_per`).
    pub once_per: Option<u64>,
    /// The loops around the call (bit `d - 1`: the one at depth `d`) across whose iterations it
    /// runs at most once (`EFF102` / `EFF103`): every way on from it leaves the loop, or a lazy
    /// initialization of the caller around it is tested in each iteration, on an object the loop
    /// does not construct, a module global, or a local variable the loop carries.
    pub once_loops: u64,
    /// How the callees run when an API runs them for the caller (`Runner`).
    pub runner: Runner,
    /// For a callable an offloading API runs (`Runner::Worker`): where its worker runs it. In
    /// another process, the bound parameters are copies ([`ArgOrigin::pickled`]).
    #[serde(default, skip_serializing_if = "heki_kb::OffloadsTo::is_thread")]
    pub offloads_to: heki_kb::OffloadsTo,
    /// The callees bind the receiver of a method call (or the object a constructor makes) to
    /// their first parameter (a static method's binding aside), as the resolution of the call
    /// says (`Callee::Project`).
    pub binds_recv: bool,
    /// For a callable a library API runs (`Runner::Worker`, `Runner::SyncFacade`,
    /// `Runner::PerItem`): its position among the call's arguments (`None` for a keyword:
    /// `sorted(xs, key=f)`). Run on a worker, the callees take the positional arguments after
    /// it; through a sync facade or per item, none of the call's.
    pub handed_callable: Option<u8>,
    /// For a callable a builtin (`Runner::PerItem`) or a pool's worker (`Runner::Worker`:
    /// `ex.map(f, xs)`) calls once per item: the API and where it takes the items from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_item: Option<Box<PerItem>>,
    /// What the receiver tells about the implementations the callees run (`None`: nothing).
    pub dispatch: Option<Box<Dispatch>>,
    /// A call on `self` whose result nothing reads (`self.prepare()` as a statement): it
    /// reaches what its caller computes only through the attributes it writes (`MUT101`,
    /// [`EffectSummary::returns_self_attrs`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub result_discarded: bool,
    /// What the call knows of queues ([`capacity`]; boxed: few calls have any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queues: Option<Box<SiteQueues>>,
    /// The object the call returns outlives one of the caller's uses of it
    /// ([`iterations::held_results`]): it escapes (stored, yielded, captured, handed to code heki
    /// does not follow), or a step that may leave a generator suspended (`next()`, a loop that
    /// leaves early) is followed by another use. Any code may run while the generator is
    /// suspended (`MUT113`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub result_held: bool,
    /// A reference site that is a base of a class statement (`class Derived(Base)`): the class
    /// it makes holds the base, and constructing that class runs the base's initialization
    /// unless it overrides it ([`Contracts::reference_bases`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_base: bool,
    /// A reference site that uses its target without handing it on or calling it
    /// (`Keys.of(...)`, `isinstance(x, Keys)`, `case Keys():`, [`Contracts::reference_reads`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_read: bool,
    /// The constants the call passes for the parameters a call target's argument list names
    /// (computed only for those, [`Contracts::marked_params`]; boxed: few calls have any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Box<ArgumentValues>>,
    /// The tests of state on every path to the call (`EFF109`: a lock its callees take behind
    /// a test of state the holder changes is not reported; only computed while the rule runs,
    /// `None` when there is none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_tests: Option<Arc<heki_ir::walk::Answer<StateTests>>>,
    /// The locks the caller certainly holds at the call (only computed while a rule that reads
    /// them runs).
    #[serde(default, skip_serializing_if = "LockHeld::is_none")]
    pub locks: LockHeld,
    /// Whether the call runs shielded from the cancellation of the AnyIO or Trio scopes around
    /// the caller (only computed while `EFF118` runs, [`cancellation::ShieldPoints`]).
    #[serde(default, skip_serializing_if = "ShieldAt::is_unshielded")]
    pub shield: ShieldAt,
    /// The targets are every implementation the call can run under R2 and R5: the receiver
    /// is no protocol, every member of a union resolves, and no project subclass's
    /// implementation is missing; for a read that runs property getters, every class the
    /// object may be finds one of them first (`TypeCtx::getter_targets_complete`). Computed
    /// with [`Self::on_every_return`]. A site where the call's result is iterated or awaited has
    /// the call's: the bodies it runs there are its targets'. A comprehension in a frame of its
    /// own (`__comp_<id>`) and a generator expression run the code written there.
    pub complete: bool,
    /// Whether the caller has made the call whenever it returns normally (computed when an
    /// enabled rule reads [`EffectSummary::must_exec`]).
    pub on_every_return: OnEveryReturn,
    /// The call is in an `except` handler (reached only through an exception edge) or a
    /// `finally` body: cleanup code, a witness only where no call on the normal path reaches
    /// the effect ([`EffectSummary::witness_ways`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cleanup: bool,
    /// For a site of the object a call returns (where it is iterated, returned, handed to a
    /// project call, or awaited): the span of that call. `None` for a call's own site.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub made_by: Option<Span>,
    /// The name the site loads its target by when it reads it from a project module object
    /// that binds it to a definition of another module (`pkg.Kind` of `pkg.enums.Kind`, which
    /// `pkg` imports): a call target with a module part judges the access by it
    /// ([`Contracts::module_members`]; boxed: few sites have one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module_member: Option<Box<FuncKey>>,
}

/// The locks the caller of a call certainly holds ([`CallSite::locks`]); one byte, as a call
/// site is kept per call of the project.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LockHeld {
    #[default]
    None,
    /// A lock: what the callees use one operation at a time is kept to one task at a time
    /// (`EFF110`).
    Exclusive,
    /// A thread lock ([`guarded::LockPoints`]): a callee called only so runs holding one
    /// (`EFF117`).
    Thread,
    /// Both.
    Both,
}

impl LockHeld {
    pub fn of(exclusive: bool, thread: bool) -> Self {
        match (exclusive, thread) {
            (false, false) => Self::None,
            (true, false) => Self::Exclusive,
            (false, true) => Self::Thread,
            (true, true) => Self::Both,
        }
    }

    pub fn is_none(&self) -> bool {
        *self == Self::None
    }

    pub fn exclusive(self) -> bool {
        matches!(self, Self::Exclusive | Self::Both)
    }

    pub fn thread(self) -> bool {
        matches!(self, Self::Thread | Self::Both)
    }
}

/// Whether a point of a function runs shielded from the cancellation of the scopes around it
/// ([`CallSite::shield`]); ordered so that the stronger of two answers is the greater.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub enum ShieldAt {
    /// No cancel scope with a shield around it.
    #[default]
    Unshielded,
    /// A cancel scope around it may shield it or not: its `shield` is no constant, is set on
    /// the scope, or the scope is made elsewhere.
    Open,
    /// A cancel scope made with `shield=True` around it on every path.
    Shielded,
}

impl ShieldAt {
    pub fn is_unshielded(&self) -> bool {
        *self == Self::Unshielded
    }
}

/// How an API runs a callable it is handed ([`CallSite::runner`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Runner {
    /// Called directly, on the caller's thread.
    #[default]
    Caller,
    /// On a worker thread: an offloading API's callable (`asyncio.to_thread(f, x)`,
    /// `ex.submit(f, x)`, once per item with [`CallSite::per_item`]: `ex.map(f, xs)`), a function
    /// decorated with `@sync_to_async`. Everything it does happens, but its blocking effects
    /// never block the caller's thread.
    Worker,
    /// On the caller's thread with a synchronous facade over an async database connection
    /// (SQLAlchemy's `AsyncSession.run_sync(f)`, Layer 2 `sync_facade_arg`): its database I/O
    /// through the facade is awaited for it; its other blocking calls block.
    SyncFacade,
    /// On the caller's thread, once per item of what a builtin iterates (`map(f, xs)`,
    /// `sorted(xs, key=f)`: [`CallSite::per_item`]). The items are its arguments, none of them a
    /// value of the caller.
    PerItem,
    /// On the caller's thread, by a project callee it is handed to, through the parameter that
    /// callee calls (`each(xs, visit)` calling `visit`): whether, on which paths, and how often
    /// the callee calls it are the callee's, so no fact of the site holds on every path. A callee
    /// run on a worker runs it there (`Runner::Worker`).
    Helper,
}

/// A builtin or a pool calling the callees of a site once per item ([`CallSite::per_item`]):
/// `EFF102` and `EFF103` count it as a loop as long as the items, which they size as a `for`
/// loop's iterable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerItem {
    /// The API (`builtins.sorted`, `concurrent.futures.ThreadPoolExecutor.map`).
    pub api: String,
    pub items: Items,
}

/// Where a builtin or a pool takes the items it calls a callable with ([`PerItem::items`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Items {
    /// The call's positional arguments at these positions, walked together: as many items as
    /// the shortest has (`map(f, xs, ys)`). None when the builtin compares the arguments
    /// themselves (`max(a, b, key=f)`).
    Args(Vec<u8>),
    /// The receiver (`xs.sort(key=f)`).
    Receiver,
    /// `*` unpacking hides which values they are (`map(f, *columns)`).
    Unpacked,
}

impl CallSite {
    /// The call a site stands for, shared by the call's own site and every site of the object
    /// it returns: the call's span and target. Other calls of the same callees with the same
    /// arguments have the target too, but not the span.
    pub fn of_call(&self) -> (Span, &CallTarget) {
        (self.made_by.unwrap_or(self.span), &self.target)
    }

    /// The site without its source positions (Early Cutoff compares what callers see of their
    /// calls without them).
    pub fn unplaced(&self) -> Self {
        Self {
            span: Span { start: 0, end: 0 },
            made_by: None,
            ..self.clone()
        }
    }

    /// Whether the targets' bodies run at the call, on the caller's thread: a callable an API
    /// runs (on a worker, once per item) or a project callee calls ([`Runner::Helper`]) does
    /// not, nor does what a call returns when it is iterated or handed on. Only such a site's completeness and paths ([`Self::complete`],
    /// [`Self::on_every_return`]) are about those bodies.
    pub fn runs_targets_here(&self) -> bool {
        self.role == SiteRole::Call && self.runner == Runner::Caller
    }

    /// How far the call has to get for the caller to have had its effect `e`: a Layer 2 call
    /// once it was made where a raised call has had it ([`ExecEffectKind::had_when_raised`]),
    /// any other call once it returned (a project callee that raised may not have run its
    /// effect).
    pub fn progress_for(&self, e: ExecEffectKind) -> heki_ir::Progress {
        match self.target {
            CallTarget::Layer2 { .. } if e.had_when_raised() => heki_ir::Progress::Called,
            _ => heki_ir::Progress::Returned,
        }
    }

    /// Whether the caller has had the call's effect `e` whenever it returns normally, as its
    /// must-facts take it ([`Self::progress_for`]).
    pub fn made_on_every_return(&self, e: ExecEffectKind) -> bool {
        match self.progress_for(e) {
            heki_ir::Progress::Called => self.on_every_return.called,
            heki_ir::Progress::Returned => self.on_every_return.returned,
        }
    }

    /// Whether the callees run in another process or interpreter, on copies of the arguments
    /// made by pickling: what they change of them, and of module state, stays there; what they
    /// run (their queries, their clients) still happens.
    pub fn passes_copies(&self) -> bool {
        self.offloads_to == heki_kb::OffloadsTo::Process
    }

    /// Whether the callees may run in another process ([`Self::passes_copies`]): the site cannot
    /// tell a process pool from a thread pool.
    pub fn may_pass_copies(&self) -> bool {
        !self.offloads_to.is_thread()
    }

    /// Whether the effect `e` of the callees does not reach the caller here: blocking work on a
    /// worker thread does not block the caller's thread, and a checkpoint reached on another
    /// thread or under a shield (or a scope that may shield) does not cancel the caller.
    pub fn hides(&self, e: ExecEffectKind) -> bool {
        if e == ExecEffectKind::Checkpoint {
            return self.runner == Runner::Worker || !self.shield.is_unshielded();
        }
        match self.runner {
            Runner::Caller | Runner::PerItem | Runner::Helper => false,
            Runner::Worker => e.blocks(),
            Runner::SyncFacade => e == ExecEffectKind::BlockingDb,
        }
    }
}

impl CallSite {
    /// The method the protocol typing the receiver declares ([`Dispatch::declared`]).
    pub fn declared(&self) -> Option<&FuncKey> {
        self.dispatch.as_ref()?.declared.as_ref()
    }

    /// The constant the call passes for the parameter `name` ([`Self::arguments`]).
    pub fn argument(&self, name: &str) -> Option<&ArgValue> {
        self.arguments.as_ref()?.get(name)
    }

    /// Parameters of the caller the receiver of a class method's or a static method's call may be
    /// ([`Dispatch::recv_params`]).
    pub fn recv_params(&self) -> u64 {
        self.dispatch.as_ref().map_or(0, |d| d.recv_params)
    }

    /// The project protocol the receiver is typed with ([`Dispatch::protocol`]).
    pub fn receiver_protocol(&self) -> Option<&str> {
        self.dispatch.as_ref()?.protocol.as_deref()
    }

    /// The class of the receiver ([`Dispatch::receiver`]).
    pub fn receiver(&self) -> Option<&ClassLine> {
        self.dispatch.as_ref()?.receiver.as_deref()
    }

    /// The parameter of the caller the receiver is and the class of the subclass providing the
    /// callee `bc`, for a call on a parameter of the caller ([`Dispatch::providers`]).
    pub fn provider(&self, bc: &BoundCallee) -> Option<(u8, &SharedLine)> {
        let i = self
            .project_callees()
            .iter()
            .position(|b| std::ptr::eq(b, bc))?;
        let dispatch = self.dispatch.as_ref()?;
        dispatch
            .providers
            .iter()
            .find(|(k, _)| usize::from(*k) == i)
            .map(|(_, line)| (dispatch.receiver_param, line))
    }

    /// The resolved project callees (none for a Layer 2 API).
    pub fn project_callees(&self) -> &[BoundCallee] {
        match &self.target {
            CallTarget::Project(callees) => callees,
            CallTarget::Layer2 { .. } => &[],
        }
    }

    /// The resolved project callees, to change (`None` for a Layer 2 API).
    pub fn project_callees_mut(&mut self) -> Option<&mut Vec<BoundCallee>> {
        match &mut self.target {
            CallTarget::Project(callees) => Some(callees),
            CallTarget::Layer2 { .. } => None,
        }
    }
}

/// Instructions a `CallSite` can be anchored at (its `span`): calls, property reads, and the
/// iterations that run a generator's body (`for` headers, `yield from`, unpacking subscripts).
/// Rules looking call sites up by instruction look at these.
pub fn anchors_call_site(kind: &heki_ir::InstKind) -> bool {
    use heki_ir::InstKind;
    matches!(
        kind,
        InstKind::Call { .. }
            | InstKind::MethodCall { .. }
            | InstKind::AttrLoad { .. }
            | InstKind::MakeIter { .. }
            | InstKind::YieldFrom { .. }
            | InstKind::SubscriptLoad { .. }
    )
}

/// Lightweight per-file summary retained in `ProjectIndex` (IR bodies are dropped).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSummary {
    pub path: PathBuf,
    pub module_name: String,
    pub content_hash: String,
    /// Final (interprocedurally propagated) summaries; filled by `ProjectIndex`. A final summary
    /// that propagation leaves equal to the local one is that one.
    pub functions: FxHashMap<FuncKey, Arc<EffectSummary>>,
    /// Local (one-function) summaries before propagation.
    pub local_functions: FxHashMap<FuncKey, Arc<EffectSummary>>,
    pub call_sites: Vec<CallSite>,
    /// The references handing a project function, method, or class on to be called later
    /// (passed, stored, returned, yielded, or raised), each as the site of what calling the
    /// value calls, at the load (local, IR-free; empty unless a call contract sets
    /// `references = "calls"`, [`Contracts::references`]). None is a call: no call edge,
    /// effect, or binding comes from them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reference_sites: Vec<CallSite>,
    /// Declarations other modules need for name and receiver resolution (shared with the
    /// project declarations built from the index).
    pub decls: Arc<ModuleDecls>,
    /// What summarizing the file read of the project declarations (completing its own
    /// declarations included): a change of any of these keys can change the summary. Recorded
    /// for the language server's index only (`summarize_project_with_reads`, the incremental
    /// updates); empty otherwise.
    pub decl_reads: DeclKeys,
    /// The `MUT208` flow facts of the file's functions (local, IR-free; empty unless an
    /// `invalidation` contract is configured).
    pub flows: FxHashMap<FuncKey, FuncFlow>,
    /// The `MUT101` facts of the file's methods that write a `self` attribute themselves, one
    /// per definition in source order (local, IR-free).
    pub cache_flows: FxHashMap<FuncKey, Vec<CacheFlow>>,
    /// What each function of the file accesses outside the project that a contract names, in
    /// its own body and in the class bodies it runs (local, IR-free; empty unless a contract
    /// names such an access, [`Contracts::external_call`]).
    #[serde(default, skip_serializing_if = "FxHashMap::is_empty")]
    pub external_accesses: FxHashMap<FuncKey, Vec<ExternalAccess>>,
    /// The calls each function of the file makes that heki binds to no project function or
    /// class, in its own body and in the class bodies it runs, passing constants for the
    /// parameters of a call target naming every callee (local, IR-free; empty unless such a
    /// target is configured, [`Contracts::any_callee_arguments`]).
    #[serde(default, skip_serializing_if = "FxHashMap::is_empty")]
    pub unbound_calls: FxHashMap<FuncKey, Vec<UnboundCall>>,
    /// The facts of the file's functions that only opt-in readers ask for (IR-free; `None`
    /// while the file has none of them, so that a run without them keeps one word per file).
    /// Read through [`Self::blind_calls`], [`Self::unawaited_handoffs`], and
    /// [`ProjectIndex::fresh_return`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opt_in_tables: Option<Box<OptInTables>>,
}

/// The opt-in facts of a file's functions, each table by function
/// ([`FileSummary::opt_in_tables`]); a table stays empty unless its [`OptInFacts`] flag is set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptInTables {
    /// The calls heki is blind to, in the function's own body and in the class bodies it runs,
    /// each once, in source order ([`OptInFacts::blind_spots`]).
    #[serde(default, skip_serializing_if = "FxHashMap::is_empty")]
    pub blind_spots: FxHashMap<FuncKey, Vec<BlindCall>>,
    /// The project functions the function hands to a library call that calls them without
    /// awaiting what they return, in its own body and in the class bodies it runs, in source
    /// order ([`OptInFacts::unawaited_handoffs`]).
    #[serde(default, skip_serializing_if = "FxHashMap::is_empty")]
    pub unawaited_handoffs: FxHashMap<FuncKey, Vec<UnawaitedHandoff>>,
    /// The names under which the file's functions themselves remove or replace the value an
    /// object keeps in its `__dict__` (`del obj.<name>`, a removal of the constant key
    /// `<name>` from a `__dict__`; [`OptInFacts::kept_values`]). Read through
    /// [`FileSummary::kept_removals`].
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub kept_removals: BTreeSet<String>,
    /// `MUT119`: the functions whose every returned value is a builtin container of one kind
    /// that the call made and that nothing else holds, local and final facts
    /// ([`OptInFacts::fresh_returns`]). Read through [`ProjectIndex::fresh_return`].
    #[serde(default)]
    pub fresh_returns: fresh::FreshReturns,
    /// The file's import sites ([`imports::ModuleImports`]; local, IR-free; `None` unless
    /// [`OptInFacts::imports`], and for a typing stub, whose imports never run). Read through
    /// [`FileSummary::imports`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imports: Option<Box<imports::ModuleImports>>,
    /// By qualified name, what a read-only annotation of the parameters of each function with
    /// one needs besides its summary ([`OptInFacts::suggestions`]; [`AnnotationFacts`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotation_facts: BTreeMap<String, AnnotationFacts>,
}

/// A call heki binds to no project function or class: of a function or class outside the
/// project, or of a value of unknown class (a parameter, an untyped value, a method of an
/// object of unknown class). Only a keyword it writes, or an entry of a `**` mapping the
/// function builds, binds a parameter of what it calls: a position or a default is no
/// constant heki can tie to a name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnboundCall {
    pub span: Span,
    /// The constants it passes so for the parameters the argument lists of call targets name.
    pub arguments: ArgumentValues,
}

/// A project function handed to a library call that calls it as a plain function and never
/// awaits what it returns (`heki_kb::EffectiveEcosystemKb::unawaited_callable`):
/// `threading.Thread`'s target, `atexit.register(f)`, `filter(f, xs)`, a `functools.partial` of
/// one, a bound method.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnawaitedHandoff {
    /// The library call.
    pub span: Span,
    /// The library API, as the catalog names it.
    pub api: String,
    /// What calling the handed value calls: one function, or every implementation of a method.
    pub callees: Vec<FuncKey>,
    /// The API tests what the callable returns for truth (`heki_kb::UnawaitedCall::tests_result`).
    pub tests_result: bool,
    /// The API gives what the callable returns back to its caller
    /// (`heki_kb::UnawaitedCall::hands_back`).
    pub hands_back: bool,
    /// Where the API runs the handed value when not on the caller's thread
    /// (`heki_kb::UnawaitedCall::runs_on`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runs_on: Option<heki_kb::RunsOn>,
    /// `callees` are every implementation calling the handed value can run, as
    /// [`CallSite::complete`] says of a call.
    pub complete: bool,
}

impl FileSummary {
    /// [`OptInTables::blind_spots`], by function.
    pub fn blind_calls(&self) -> impl Iterator<Item = (&FuncKey, &[BlindCall])> {
        self.opt_in_tables
            .iter()
            .flat_map(|t| &t.blind_spots)
            .map(|(f, calls)| (f, calls.as_slice()))
    }

    /// [`OptInTables::blind_spots`] of `func`: none when the table was not built.
    pub fn blind_calls_of(&self, func: &FuncKey) -> &[BlindCall] {
        self.opt_in_tables
            .as_ref()
            .and_then(|t| t.blind_spots.get(func))
            .map_or(&[], Vec::as_slice)
    }

    /// [`OptInTables::unawaited_handoffs`], by function.
    pub fn unawaited_handoffs(&self) -> impl Iterator<Item = (&FuncKey, &[UnawaitedHandoff])> {
        self.opt_in_tables
            .iter()
            .flat_map(|t| &t.unawaited_handoffs)
            .map(|(f, handoffs)| (f, handoffs.as_slice()))
    }

    /// The opt-in tables of the file, made when it first has one of their facts.
    pub(crate) fn opt_in_tables_mut(&mut self) -> &mut OptInTables {
        self.opt_in_tables.get_or_insert_with(Box::default)
    }

    /// [`OptInTables::kept_removals`] (none while the facts are not computed).
    pub fn kept_removals(&self) -> impl Iterator<Item = &str> {
        self.opt_in_tables
            .iter()
            .flat_map(|t| &t.kept_removals)
            .map(String::as_str)
    }

    /// The file's import sites ([`OptInTables::imports`]; none while the facts are not computed).
    pub fn imports(&self) -> Option<&imports::ModuleImports> {
        self.opt_in_tables.as_ref()?.imports.as_deref()
    }

    /// The final facts as the local ones, where propagation starts.
    fn reset_final_facts(&mut self) {
        self.functions = self.local_functions.clone();
        if let Some(tables) = &mut self.opt_in_tables {
            tables.fresh_returns.resolved = tables.fresh_returns.local.clone();
        }
    }

    /// The summary of a project file heki could not read or parse: a module that defines
    /// nothing heki knows ([`ModuleDecls::unread_module`]).
    pub fn unread(module: &UnreadModule) -> Self {
        Self {
            path: module.path.clone(),
            module_name: module.module_name.clone(),
            decls: Arc::new(ModuleDecls::unread_module(
                &module.module_name,
                module.is_package,
            )),
            ..Self::default()
        }
    }

    /// Project call edges `(caller, callee, span)`, the source of the reverse call graph: every
    /// resolved project callee of every call site.
    pub fn call_edges(&self) -> impl Iterator<Item = (&FuncKey, &FuncKey, Span)> + '_ {
        self.call_sites.iter().flat_map(|site| {
            site.project_callees()
                .iter()
                .map(move |bc| (&site.caller, &bc.callee, site.span))
        })
    }
}

/// A function's final facts: its summary and the facts kept beside it
/// ([`OptInTables::fresh_returns`]). Early Cutoff compares all of them.
#[derive(Clone)]
struct FinalFacts {
    summary: Arc<EffectSummary>,
    fresh_return: Option<fresh::FreshReturn>,
}

impl FinalFacts {
    /// What callers read of `file`'s `func` after propagation.
    fn of(file: &FileSummary, func: &FuncKey) -> Option<Self> {
        let tables = file.opt_in_tables.as_ref();
        Some(Self {
            summary: Arc::clone(file.functions.get(func)?),
            fresh_return: tables
                .and_then(|t| t.fresh_returns.resolved.get(func))
                .cloned(),
        })
    }

    /// Location-insensitive equality ([`EffectSummary::same_facts`]).
    fn same_facts(&self, other: &Self) -> bool {
        self.summary.same_facts(&other.summary) && self.fresh_return == other.fresh_return
    }

    /// [`Self::same_facts`] of what callers read ([`EffectSummary::same_facts_for_callers`]).
    fn same_facts_for_callers(&self, other: &Self) -> bool {
        self.summary.same_facts_for_callers(&other.summary)
            && self.fresh_return == other.fresh_return
    }
}

/// Result of an incremental file update (Early Cutoff indicator for `heki_lsp`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncrementalUpdate {
    /// True if any function's `EffectSummary` changed and callers need re-evaluation.
    pub summary_changed: bool,
    /// Functions (transitive callers) that depend on changed summaries.
    pub affected_callers: Vec<FuncKey>,
    /// The functions of the file (and of call cycles through it) whose facts changed or that
    /// disappeared, and the callers whose facts changed with them, sorted.
    pub changed: Vec<FuncKey>,
    /// The file's import facts changed, wherever its sites are written
    /// ([`imports::ModuleImports::same_facts`]), or it appeared or disappeared with some: the
    /// import graph changed.
    pub imports_changed: bool,
}

/// A file of the project that heki could not read or parse, by the module it would define.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadModule {
    pub path: PathBuf,
    pub module_name: String,
    pub is_package: bool,
}

/// One module handed to the analyzer. The IR is borrowed only during summarization.
pub struct ModuleInput<'a> {
    pub path: PathBuf,
    pub module_name: String,
    pub is_package: bool,
    /// [`ModuleLayout::root_is_package`] of the project.
    pub root_is_package: bool,
    pub content_hash: String,
    pub semantic: &'a SemanticModel,
    pub ir: &'a ModuleIr,
}

/// One execution effect that runs at a call site, with the shortest chain from the site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteEffect {
    pub effect: ExecEffectKind,
    /// Starts with the hop of this call site; ends at the Layer 2 sink.
    pub chain: Vec<CallHop>,
    /// Project callee the effect comes through (`None` for a Layer 2 API).
    pub callee: Option<FuncKey>,
    /// The effect comes from an awaited / driven `async def` callee (its own body).
    pub callee_is_async: bool,
    /// The loops around the call (bit `d - 1`: the one at depth `d`) across whose iterations the
    /// effect runs at most once (`CallSite::once_loops`), also once per object where the objects
    /// stay the same (`EFF102` / `EFF103`).
    pub once_loops: u64,
}

/// Incremental project-wide summary & reverse call-graph index.
#[derive(Debug, Default, Clone)]
pub struct ProjectIndex {
    pub files: FxHashMap<PathBuf, FileSummary>,
    pub reverse_call_graph: FxHashMap<FuncKey, FxHashSet<FuncKey>>,
    /// Defining file of every summarized function.
    pub(crate) func_files: FxHashMap<FuncKey, PathBuf>,
    /// The declared contracts the summaries depend on (`None`: no contract facts).
    pub(crate) contracts: Option<Arc<dyn Contracts>>,
    /// Propagation leaves typed writes where they are made: callers get neither the write sites
    /// of their callees, nor their write facts, nor `IndirectHelper` sites. Set when no
    /// enabled rule reads them; every function keeps its own.
    pub(crate) typed_writes: TypedWrites,
    /// What the project callers bind to the parameters queue waits ask about, from the local
    /// facts of every file (computed before propagation).
    pub(crate) param_capacities: capacity::ParamCapacities,
    /// The opt-in facts the summaries carry ([`OptInFacts`]); incremental updates extract the
    /// same.
    pub(crate) opt_in: OptInFacts,
}

// The index is shared between the threads of a check, so it must stay `Sync`. The engines of
// `heki_ir::walk` that keep per-check state are not, so one stored in the index fails to compile
// here: their memos live in their owner for one analysis (ADR-017).
const _: () = {
    struct Shared<T: Sync>(std::marker::PhantomData<T>);
    let _ = Shared::<ProjectIndex>(std::marker::PhantomData);
};

/// True for `.pyi` typing stubs (prefer the sibling `.py` implementation when both exist).
pub fn is_stub(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "pyi")
}

/// Which of the files sharing a module name defines it, the greatest winning: an
/// implementation before its `.pyi` stub, a package (`pkg/__init__.py`) before a module file of
/// the same name (`pkg.py`, which Python never imports next to it), then the greater path. Full
/// and incremental analysis both pick the owner this way, whatever order they meet the files in.
pub fn module_precedence(path: &Path) -> (bool, bool, &Path) {
    let package = path.file_stem().is_some_and(|s| s == "__init__");
    (!is_stub(path), package, path)
}

/// Summarize every module of a project and propagate effects to a fixpoint.
pub fn analyze_project(modules: &[ModuleInput<'_>], kb: &EffectiveEcosystemKb) -> ProjectIndex {
    analyze_project_with(modules, kb, None)
}

/// [`analyze_project`] with the declared contracts the summaries depend on (`MUT202` producers,
/// `MUT208` invalidation contracts); the index keeps them for incremental updates.
pub fn analyze_project_with(
    modules: &[ModuleInput<'_>],
    kb: &EffectiveEcosystemKb,
    contracts: Option<Arc<dyn Contracts>>,
) -> ProjectIndex {
    analyze_project_for(
        modules,
        &[],
        kb,
        None,
        contracts,
        TypedWrites::Propagated,
        OptInFacts::ALL,
    )
}

/// [`analyze_project_with`] with the project's files heki could not read or parse (`unread`),
/// propagating typed writes and extracting the opt-in facts only when a rule reads them.
pub fn analyze_project_for(
    modules: &[ModuleInput<'_>],
    unread: &[UnreadModule],
    kb: &EffectiveEcosystemKb,
    soundness: Option<&EffectiveSoundnessKb>,
    contracts: Option<Arc<dyn Contracts>>,
    typed_writes: TypedWrites,
    opt_in: OptInFacts,
) -> ProjectIndex {
    let files = summarize(
        modules,
        unread,
        kb,
        soundness,
        false,
        contracts.as_deref(),
        opt_in,
    );
    ProjectIndex::build_for(files, contracts, typed_writes, opt_in)
}

/// A project index taken apart ([`ProjectIndex::into_files`]): its files' local summaries, with
/// their final facts reset to the local ones, and what they were summarized with; no call graph
/// and nothing propagation derived. A summary replaced here drops the one it replaces, and
/// [`Self::build`] propagates once.
pub struct IndexFiles {
    pub files: FxHashMap<PathBuf, FileSummary>,
    contracts: Option<Arc<dyn Contracts>>,
    typed_writes: TypedWrites,
    opt_in: OptInFacts,
}

impl IndexFiles {
    /// The local summary of `input`, a module of these files edited or as it is, against the
    /// project declarations `decls`, with the facts these files are summarized with: what the
    /// module's own functions do, before propagation ([`FileSummary::local_functions`]).
    pub fn summarize(
        &self,
        input: &ModuleInput<'_>,
        kb: &EffectiveEcosystemKb,
        soundness: Option<&EffectiveSoundnessKb>,
        decls: &DeclPasses,
    ) -> FileSummary {
        let own = resolver::own_module_decls(input, kb, decls);
        summary::summarize_module(
            input,
            decls.completed(),
            kb,
            soundness,
            own,
            false,
            self.contracts.as_deref(),
            self.opt_in,
        )
    }

    /// The index of these files, built as [`ProjectIndex::build_for`] builds one (one
    /// propagation), in the order the project's files are summarized in.
    pub fn build(self) -> ProjectIndex {
        let mut files: Vec<FileSummary> = self.files.into_values().collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        ProjectIndex::build_for(files, self.contracts, self.typed_writes, self.opt_in)
    }
}

/// Whether propagation carries typed writes (`EffectSummary::typed_field_write_sites` and
/// `typed_field_writes`) from callees to callers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TypedWrites {
    #[default]
    Propagated,
    /// No enabled rule reads propagated typed writes: each function keeps its own only.
    Local,
}

/// The facts that summaries and propagation compute only when something reads them: the
/// opt-in rules' facts, `MUT112`'s return origins (`EffectSummary::return_origins`,
/// `ArgOrigin::origins`), `MUT113`'s iteration facts (`EffectSummary::iterations`,
/// `CallSite::result_held`), `EFF109`'s lock facts (`EffectSummary::locks`,
/// `CallSite::state_tests`), and `EFF110`'s one-at-a-time uses (`EffectSummary::exclusive`,
/// `CallSite::locks`), `EFF112`'s set-order flows (`EffectSummary::set_order`), `EFF114`'s
/// call cycles (`EffectSummary::call_cycle`), and the shared state functions change
/// (`EffectSummary::shared_state`), which only `heki inspect` reads, the calls heki is blind to
/// (`OptInTables::blind_spots`), which only `heki recommend` reads, `MUT119`'s new returned
/// containers (`OptInTables::fresh_returns`), and the import facts (`OptInTables::imports`),
/// which `heki inspect --imports` reads.
/// `CallSite::returned`, which `MUT202` reads too, is always computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptInFacts {
    pub return_origins: bool,
    pub iterations: bool,
    pub locks: bool,
    pub exclusive: bool,
    pub shared_state: bool,
    /// [`EffectSummary::must_exec`] and what computes it at call sites.
    pub must_effects: bool,
    /// What `MUT101`'s exact claim reads of a kept value: [`EffectSummary::must_read_self_attrs`],
    /// [`KeptFacts::removes_kept`], the points of a method that may remove one
    /// ([`CacheEvent::MayRemove`]), and the call-site facts they are computed from
    /// ([`CallSite::complete`], [`CallSite::on_every_return`]).
    pub kept_values: bool,
    pub blind_spots: bool,
    pub set_order: bool,
    pub call_cycles: bool,
    /// [`OptInTables::unawaited_handoffs`] (`EFF115`, `EFF116`).
    pub unawaited_handoffs: bool,
    /// [`EffectSummary::guarded`] and [`CallSite::locks`] (`EFF117`).
    pub guarded: bool,
    /// The cancellation checkpoints awaits reach (`ExecEffectKind::Checkpoint`) and
    /// [`CallSite::shield`] (`EFF118`).
    pub checkpoints: bool,
    /// [`OptInTables::fresh_returns`].
    pub fresh_returns: bool,
    /// [`OptInTables::imports`].
    pub imports: bool,
    /// [`EffectSummary::only_read_params`], [`EffectSummary::read_kinds`], and
    /// [`EffectSummary::container_params`], which `heki inspect` shows and the language server
    /// keeps current.
    pub only_read: bool,
    /// What `heki recommend`'s read-only annotations read besides [`Self::only_read`]:
    /// [`OptInTables::annotation_facts`], and the references handing a project function on
    /// ([`FileSummary::reference_sites`]), its uses as a value.
    pub suggestions: bool,
    /// [`EffectSummary::dispatch`], which `heki inspect`'s registration contexts read and the
    /// language server keeps current.
    pub dispatch: bool,
}

impl OptInFacts {
    /// Every opt-in fact a rule or `heki inspect` reads. The blind spots, which neither reads,
    /// are left out, and so are the registration facts, which summary snapshots do not record.
    pub const ALL: Self = Self {
        return_origins: true,
        iterations: true,
        locks: true,
        exclusive: true,
        shared_state: true,
        must_effects: true,
        kept_values: true,
        blind_spots: false,
        set_order: true,
        call_cycles: true,
        unawaited_handoffs: true,
        guarded: true,
        checkpoints: true,
        fresh_returns: true,
        imports: true,
        only_read: true,
        suggestions: false,
        dispatch: false,
    };

    /// Whether the call sites carry [`CallSite::complete`] and [`CallSite::on_every_return`]
    /// (`EFF114` builds its cycles over complete calls).
    pub fn site_paths(self) -> bool {
        self.must_effects || self.kept_values || self.call_cycles
    }
}

impl Default for OptInFacts {
    fn default() -> Self {
        Self::ALL
    }
}

/// The local summaries of every module of a project with `contracts`, recording what each read
/// of the project declarations (`FileSummary::decl_reads`), for an index that incremental
/// updates keep current. [`ProjectIndex::build_with`] propagates them; it needs no IR, so the
/// caller can drop the modules first.
pub fn summarize_project_with_reads(
    modules: &[ModuleInput<'_>],
    unread: &[UnreadModule],
    kb: &EffectiveEcosystemKb,
    soundness: Option<&EffectiveSoundnessKb>,
    contracts: Option<&dyn Contracts>,
    opt_in: OptInFacts,
) -> Vec<FileSummary> {
    summarize(modules, unread, kb, soundness, true, contracts, opt_in)
}

/// The summaries of `modules`, then one per `unread` module.
fn summarize(
    modules: &[ModuleInput<'_>],
    unread: &[UnreadModule],
    kb: &EffectiveEcosystemKb,
    soundness: Option<&EffectiveSoundnessKb>,
    track: bool,
    contracts: Option<&dyn Contracts>,
    opt_in: OptInFacts,
) -> Vec<FileSummary> {
    let unread: Vec<FileSummary> = unread.iter().map(FileSummary::unread).collect();
    let known = unread
        .iter()
        .map(|f| (f.path.clone(), Arc::clone(&f.decls)))
        .collect();
    let (decls, own) = resolver::project_decls_for(modules, kb, Some(known), track);
    let mut files: Vec<FileSummary> = if track {
        // Every module's reads go to the one recorder of `decls`: one module at a time.
        modules
            .iter()
            .zip(own)
            .map(|(m, own)| {
                summary::summarize_module(m, &decls, kb, soundness, own, track, contracts, opt_in)
            })
            .collect()
    } else {
        // A module's local summary depends on it and the declarations only (indexed collect:
        // input order).
        modules
            .par_iter()
            .zip(own)
            .map(|(m, own)| {
                summary::summarize_module(m, &decls, kb, soundness, own, track, contracts, opt_in)
            })
            .collect()
    };
    files.extend(unread);
    files
}

impl ProjectIndex {
    /// Build an index from local file summaries and propagate the whole project.
    pub fn build(files: Vec<FileSummary>) -> Self {
        Self::build_with(files, None)
    }

    /// Whether callers got the typed writes of their callees ([`TypedWrites`]).
    pub fn propagates_typed_writes(&self) -> bool {
        self.typed_writes == TypedWrites::Propagated
    }

    /// The opt-in facts the summaries carry.
    pub fn opt_in_facts(&self) -> OptInFacts {
        self.opt_in
    }

    /// [`Self::build`] for summaries extracted with `contracts`.
    pub fn build_with(files: Vec<FileSummary>, contracts: Option<Arc<dyn Contracts>>) -> Self {
        Self::build_for(files, contracts, TypedWrites::Propagated, OptInFacts::ALL)
    }

    /// [`Self::build_with`], propagating typed writes as `typed_writes` says, for summaries that
    /// carry the opt-in facts `opt_in`; incremental updates keep doing so.
    pub fn build_for(
        mut files: Vec<FileSummary>,
        contracts: Option<Arc<dyn Contracts>>,
        typed_writes: TypedWrites,
        opt_in: OptInFacts,
    ) -> Self {
        // The owners of shared module names last (`add_file_links` keeps them either way).
        files.sort_by(|a, b| module_precedence(&a.path).cmp(&module_precedence(&b.path)));
        let mut index = Self {
            contracts,
            typed_writes,
            opt_in,
            ..Self::default()
        };
        for mut file in files {
            file.reset_final_facts();
            index.add_file_links(&file);
            index.files.insert(file.path.clone(), file);
        }
        index.param_capacities = capacity::ParamCapacities::of(&index);
        let all: FxHashSet<FuncKey> = index.all_functions().collect();
        callgraph::propagate(&mut index, &all);
        callgraph::recompute_sync_driven(&mut index);
        index
    }

    /// The index's file summaries with their final facts reset to the local ones, the call
    /// graph and what propagation derived dropped, to build the index again with some of them
    /// replaced ([`IndexFiles`]).
    pub fn into_files(self) -> IndexFiles {
        let Self {
            mut files,
            contracts,
            typed_writes,
            opt_in,
            reverse_call_graph,
            func_files,
            param_capacities,
        } = self;
        // What propagation derived goes before a rebuild derives it again: never two at once.
        drop((reverse_call_graph, func_files, param_capacities));
        for file in files.values_mut() {
            file.reset_final_facts();
        }
        IndexFiles {
            files,
            contracts,
            typed_writes,
            opt_in,
        }
    }

    /// Whether every implementation the call at `site` (in `path`) can run has run `e`
    /// whenever the call returns, with the chain and the witnesses that show it ([`SiteMust`];
    /// `None` when that is not established, or when no enabled rule reads must-facts).
    pub fn site_must(&self, path: &Path, site: &CallSite, e: ExecEffectKind) -> Option<SiteMust> {
        // Without must-facts no call site is complete, so nothing is established.
        callgraph::site_must_chain(self, path, site, e)
    }

    /// Effects that run at `site` (in file `path`): the Layer 2 API's effects after the
    /// `is_awaited` filter, or the direct and transitive effects of each project callee's facts
    /// the site runs in its caller's body ([`Self::delivery`]) whose guards the call does not
    /// decide against (`guards`). Shared by Boundary-Only Reporting and the loop rules.
    pub fn site_effects(&self, path: &Path, site: &CallSite) -> Vec<SiteEffect> {
        let mut out = Vec::new();
        match &site.target {
            CallTarget::Layer2 {
                symbol, effects, ..
            } => {
                for e in effects
                    .iter()
                    .chain(self.while_full_effects(site).into_iter().flatten())
                {
                    out.push(SiteEffect {
                        effect: *e,
                        chain: vec![callgraph::site_hop(site, path, symbol.clone())],
                        callee: None,
                        callee_is_async: false,
                        once_loops: site.once_loops,
                    });
                }
            }
            CallTarget::Project(callees) => {
                for bc in callees {
                    // Facts handed to whoever iterates the caller's result do not run here.
                    let Some((callee, d, facts)) = self.summary(&bc.callee).and_then(|callee| {
                        let d = self
                            .delivery(site, callee)
                            .filter(|d| d.into == Facts::Body)?;
                        Some((callee, d, callee.facts(d.from)?))
                    }) else {
                        continue;
                    };
                    let lead = callgraph::lead_hops(site, path, &bc.callee, d.via);
                    let rewrite = callgraph::rewrites(callee, d.from);
                    let effects: BTreeSet<ExecEffectKind> = facts
                        .direct_exec_effects
                        .iter()
                        .chain(&facts.transitive_exec_effects)
                        .copied()
                        .collect();
                    for e in effects {
                        if site.hides(e) {
                            continue;
                        }
                        // A guard the call decides against: the effect does not run here. A
                        // queue the caller's parameter holds is bounded when its own callers
                        // all bind known queues and one is bounded.
                        let Some(through) = guards::through_call(
                            site,
                            bc,
                            facts.exec_guards.get(&e),
                            &self.param_capacities,
                        ) else {
                            continue;
                        };
                        let queue = through.guards.bounded_queue.map(|p| CapacityOf {
                            params: BTreeSet::from([(site.caller.clone(), p)]),
                            ..CapacityOf::default()
                        });
                        if queue.is_some_and(|q| !self.param_capacities.bounded(&q)) {
                            continue;
                        }
                        let Some(tail) = callgraph::arrival(facts, callee, e, site, bc).tail()
                        else {
                            continue;
                        };
                        let chain = callgraph::extend_chain(&lead, tail, rewrite);
                        out.push(SiteEffect {
                            effect: e,
                            chain: owned_chain(&chain),
                            callee: Some(bc.callee.clone()),
                            callee_is_async: callee.is_async,
                            once_loops: through.once_loops,
                        });
                    }
                }
            }
        }
        out
    }

    /// The effects a call of a Layer 2 API has besides its own (`CallTarget::Layer2::effects`):
    /// those it has only on a queue, which the project shows bounded (`SiteQueues::while_full`).
    pub fn while_full_effects<'s>(
        &self,
        site: &'s CallSite,
    ) -> Option<&'s BTreeSet<ExecEffectKind>> {
        self.param_capacities.while_full(site)
    }

    /// What `site` hands its caller of the project callee summarized by `callee`: the one
    /// home of "does a body run here". A call runs a plain function's body, or an `async def`'s
    /// when it is awaited or driven (calling one only creates a coroutine), never a generator
    /// function's (calling one only creates the generator); the `Iterate`, `Return`, and
    /// `Through` sites of the object a call returns run what iterating it runs.
    pub fn delivery<'s>(&self, site: &'s CallSite, callee: &EffectSummary) -> Option<Delivery<'s>> {
        callgraph::delivery(|k| self.summary(k), site, callee)
    }

    /// The construction, in the caller, of a write the callee `bc.callee` makes to the object
    /// bound to its parameter `param` (`Construction::Param`; `own`: an in-place change), at the
    /// call `site` delivering its facts `d`: `Yes` when the call hands it a new object nothing
    /// kept, `Param` when the caller's own parameter, as propagation decides it for the writes a
    /// callee hands its callers.
    pub fn construction_at_call(
        &self,
        call: (&CallSite, &BoundCallee, &Delivery<'_>),
        param: u8,
        own: bool,
    ) -> Construction {
        callgraph::construction_at_call(self, call, param, own)
    }

    /// Final summary of `func`, if known.
    pub fn summary(&self, func: &FuncKey) -> Option<&EffectSummary> {
        self.shared_summary(func).map(Arc::as_ref)
    }

    /// The final `MUT119` fact of `func` ([`fresh::FreshReturns::resolved`]), if it has one.
    pub fn fresh_return(&self, func: &FuncKey) -> Option<&fresh::FreshReturn> {
        let file = self.files.get(self.func_files.get(func)?)?;
        file.opt_in_tables
            .as_ref()?
            .fresh_returns
            .resolved
            .get(func)
    }

    fn final_facts(&self, func: &FuncKey) -> Option<FinalFacts> {
        FinalFacts::of(self.files.get(self.func_files.get(func)?)?, func)
    }

    fn shared_summary(&self, func: &FuncKey) -> Option<&Arc<EffectSummary>> {
        self.files
            .get(self.func_files.get(func)?)?
            .functions
            .get(func)
    }

    /// Declarations of every indexed module (for re-summarizing an edited module), preferring
    /// the owner ([`module_precedence`]) among the files sharing a module name.
    pub fn project_decls(&self) -> ProjectDecls {
        ProjectDecls::new(self.preferred_decls())
    }

    /// [`Self::project_decls`] with its pass 1, for [`Self::reanalyze_module_cached`].
    pub fn decl_passes(&self) -> DeclPasses {
        DeclPasses::new(self.preferred_decls())
    }

    /// Every module's declarations in [`module_precedence`] order (a later file sharing a
    /// module name replaces the earlier ones), from which [`DeclPasses::new`] and
    /// [`ProjectDecls::new`] build the project's.
    pub fn preferred_decls(&self) -> Vec<Arc<ModuleDecls>> {
        let mut files: Vec<&FileSummary> = self.files.values().collect();
        files.sort_by(|a, b| module_precedence(&a.path).cmp(&module_precedence(&b.path)));
        files.into_iter().map(|f| Arc::clone(&f.decls)).collect()
    }

    /// Declarations currently indexed for `module_name`: those of its owner
    /// ([`module_precedence`]).
    pub fn module_decls(&self, module_name: &str) -> Option<Arc<ModuleDecls>> {
        self.files
            .values()
            .filter(|f| f.module_name == module_name)
            .max_by(|a, b| module_precedence(&a.path).cmp(&module_precedence(&b.path)))
            .map(|f| Arc::clone(&f.decls))
    }

    /// Re-summarize one edited module against the index's declarations and update it.
    pub fn reanalyze_module(
        &mut self,
        input: &ModuleInput<'_>,
        kb: &EffectiveEcosystemKb,
        soundness: Option<&EffectiveSoundnessKb>,
    ) -> IncrementalUpdate {
        let others: Vec<(PathBuf, Arc<ModuleDecls>)> = self
            .files
            .values()
            .filter(|f| f.path != input.path)
            .map(|f| (f.path.clone(), Arc::clone(&f.decls)))
            .collect();
        let (decls, own) =
            resolver::project_decls_for(std::slice::from_ref(input), kb, Some(others), true);
        let own = own.into_iter().next().expect("one module in, one out");
        let file = summary::summarize_module(
            input,
            &decls,
            kb,
            soundness,
            own,
            true,
            self.contracts.as_deref(),
            self.opt_in,
        );
        self.update_file(file)
    }

    /// The index under one registration context's premise: parameter `param` of `func`, a
    /// function of `input`'s module, holds an instance of `class`. The module is summarized again
    /// with that type and propagated as an edit is; the index itself is left as it is, so no
    /// unconditional fact, report, or fix reads the premise.
    pub fn with_param_type(
        &self,
        input: &ModuleInput<'_>,
        kb: &EffectiveEcosystemKb,
        soundness: Option<&EffectiveSoundnessKb>,
        func: &FuncKey,
        param: u8,
        class: &str,
    ) -> ProjectIndex {
        let others: Vec<(PathBuf, Arc<ModuleDecls>)> = (self.files.values())
            .filter(|f| f.path != input.path)
            .map(|f| (f.path.clone(), Arc::clone(&f.decls)))
            .collect();
        let (decls, own) =
            resolver::project_decls_for(std::slice::from_ref(input), kb, Some(others), false);
        let (own, _) = own.into_iter().next().expect("one module in, one out");
        let key = (func.qualname.clone(), usize::from(param));
        let seeded = resolver::SeededParams::from_iter([(key, types::Ty::inst(class))]);
        let file = summary::summarize_module_seeded(
            input,
            &decls,
            kb,
            soundness,
            own,
            self.contracts.as_deref(),
            self.opt_in,
            &seeded,
        );
        let mut scratch = self.clone();
        scratch.update_file(file);
        scratch
    }

    /// [`Self::reanalyze_module`] against cached project declarations (both passes, kept up
    /// to date in place): the edit costs the size of the module unless its declarations
    /// changed. Also returns what changed in the declarations (empty: nothing), which every
    /// file whose summary or diagnostics read it depends on.
    pub fn reanalyze_module_cached(
        &mut self,
        input: &ModuleInput<'_>,
        kb: &EffectiveEcosystemKb,
        soundness: Option<&EffectiveSoundnessKb>,
        decls: &mut DeclPasses,
    ) -> (IncrementalUpdate, DeclKeys) {
        // Another file owns the module name: its declarations stay the module's.
        let masked = self.files.values().any(|f| {
            f.module_name == input.module_name
                && f.path != input.path
                && module_precedence(&f.path) > module_precedence(&input.path)
        });
        let (changed, own) = if masked {
            (
                DeclKeys::default(),
                resolver::own_module_decls(input, kb, decls),
            )
        } else {
            let (changed, own, reads) = resolver::update_module_decls(input, kb, decls);
            (changed, (own, reads))
        };
        let file = summary::summarize_module(
            input,
            decls.completed(),
            kb,
            soundness,
            own,
            true,
            self.contracts.as_deref(),
            self.opt_in,
        );
        (self.update_file(file), changed)
    }

    /// Update a single file's summary and report whether downstream callers are affected.
    ///
    /// `new_file` carries local facts (`local_functions`, `call_sites`); its
    /// final summaries are recomputed here. Callers are re-propagated only when a final
    /// summary of the file changed (location-insensitively) — **Early Cutoff**.
    pub fn update_file(&mut self, mut new_file: FileSummary) -> IncrementalUpdate {
        let path = new_file.path.clone();
        let old_file = self.files.remove(&path);
        let old_imports = old_file.as_ref().and_then(|f| f.imports());
        let imports_changed = match (old_imports, new_file.imports()) {
            (Some(old), Some(new)) => !old.same_facts(new),
            (old, new) => old.is_some() != new.is_some(),
        };
        let mut previous: FxHashMap<FuncKey, FinalFacts> = FxHashMap::default();
        let mut owned_before: FxHashSet<FuncKey> = FxHashSet::default();
        if let Some(old) = &old_file {
            for k in old.functions.keys() {
                if self.func_files.get(k) == Some(&path) {
                    owned_before.insert(k.clone());
                    previous.extend(FinalFacts::of(old, k).map(|facts| (k.clone(), facts)));
                }
            }
            self.remove_file_links(old);
        }
        new_file.reset_final_facts();
        // What the callers decided before the edit: the recomputation below reports a function
        // only when its flag differs from that.
        for (func, s) in &mut new_file.functions {
            if previous
                .get(func)
                .is_some_and(|old| old.summary.sync_driven_only)
            {
                Arc::make_mut(s).sync_driven_only = true;
            }
        }
        self.add_file_links(&new_file);
        let own: FxHashSet<FuncKey> = new_file
            .local_functions
            .keys()
            .filter(|k| self.func_files.get(*k) == Some(&path))
            .cloned()
            .collect();
        // `sync_driven_only` can only change for the file's functions and the callees of its
        // old and new call sites.
        let mut sync_candidates: FxHashSet<FuncKey> = own.clone();
        for file in old_file.iter().chain(std::iter::once(&new_file)) {
            sync_candidates.extend(file.local_functions.keys().cloned());
            for (_, callee, _) in file.call_edges() {
                sync_candidates.insert(callee.clone());
            }
        }
        self.files.insert(path.clone(), new_file);
        // A function the file stopped defining falls back to the highest-ranked other file
        // defining it (a `.pyi` stub next to its `.py` implementation), as a full build links it.
        for func in owned_before.iter().filter(|f| !own.contains(*f)) {
            let fallback = self
                .files
                .values()
                .filter(|f| f.local_functions.contains_key(func))
                .map(|f| &f.path)
                .max_by(|a, b| module_precedence(a).cmp(&module_precedence(b)));
            if let Some(other) = fallback {
                self.func_files.insert(func.clone(), other.clone());
            }
        }
        // The functions elsewhere waiting on a queue whose capacity the edit changed (what the
        // file's calls bind to a parameter): their effects change without an edit of their own.
        let touched = old_file.iter().chain(self.files.get(&path));
        let recapacitated = if self.param_capacities.touched_by(touched) {
            let capacities = capacity::ParamCapacities::of(self);
            let changed = capacities.changed(&self.param_capacities);
            self.param_capacities = capacities;
            changed
        } else {
            FxHashSet::default()
        };
        let waiting: FxHashSet<FuncKey> = if recapacitated.is_empty() {
            FxHashSet::default()
        } else {
            self.files
                .values()
                .flat_map(|f| {
                    let asking = capacity::asking(f, &recapacitated);
                    asking
                        .into_iter()
                        .filter(|k| self.func_files.get(*k) == Some(&f.path))
                })
                .cloned()
                .collect()
        };

        // Region 0: the file's functions (and those waiting on queues it changed) plus every
        // function sharing a (cross-file) SCC with them, so cycles cannot keep stale effects of
        // the old version alive.
        let seeds: FxHashSet<FuncKey> = own.iter().chain(&waiting).cloned().collect();
        let mut region = callgraph::scc_closure(self, &seeds);
        for func in &region {
            if !own.contains(func)
                && let Some(facts) = self.final_facts(func)
            {
                previous.insert(func.clone(), facts);
            }
        }
        callgraph::reset_to_local(self, &region);
        callgraph::propagate(self, &region);
        let sync_changed = callgraph::recompute_sync_driven_for(self, &sync_candidates);

        let mut changed: BTreeSet<FuncKey> = BTreeSet::new();
        // Same facts, but positions callers copy (call chains, write sites) moved.
        let mut moved: BTreeSet<FuncKey> = BTreeSet::new();
        // Only facts no caller reads changed (the registration facts): the function's own
        // record changed, its callers' did not.
        let mut own_only: BTreeSet<FuncKey> = BTreeSet::new();
        for func in &region {
            match (previous.get(func), self.final_facts(func)) {
                (Some(old), Some(new)) if old.same_facts_for_callers(&new) => {
                    if !old.same_facts(&new) {
                        own_only.insert(func.clone());
                    }
                    if !old.summary.same_for_callers(&new.summary) {
                        moved.insert(func.clone());
                    }
                }
                _ => {
                    changed.insert(func.clone());
                }
            }
        }
        for func in owned_before {
            if !own.contains(&func) {
                changed.insert(func);
            }
        }
        changed.extend(sync_changed);
        // A wait whose queue changed changes the reports at its own call even where the
        // function blocks already for another reason (its summary unchanged).
        changed.extend(waiting);

        // Callers whose facts depend on changed summaries (Early Cutoff when there are none),
        // then callers that only copy moved positions: their summaries are refreshed too, so
        // the index keeps what a full build computes, but no caller's facts change.
        let affected = self.callers_closure(&changed, &FxHashSet::default());
        let excluded: FxHashSet<FuncKey> = changed.iter().chain(&affected).cloned().collect();
        let relocated = self.callers_closure(&moved, &excluded);
        let summary_changed = !changed.is_empty();
        changed.extend(own_only);
        let changed: Vec<FuncKey> = changed.into_iter().collect();
        if affected.is_empty() && relocated.is_empty() {
            return IncrementalUpdate {
                summary_changed,
                affected_callers: Vec::new(),
                changed,
                imports_changed,
            };
        }
        // Whole SCCs, as a full build computes them: an edited function sharing one with a caller
        // is recomputed with it, not taken as a finished callee (`Resolver::in_scc`).
        let callers: FxHashSet<FuncKey> = affected.iter().chain(&relocated).cloned().collect();
        region = callgraph::scc_closure(self, &callers);
        let before: FxHashMap<FuncKey, FinalFacts> = affected
            .iter()
            .filter_map(|k| Some((k.clone(), self.final_facts(k)?)))
            .collect();
        callgraph::reset_to_local(self, &region);
        callgraph::propagate(self, &region);
        callgraph::recompute_sync_driven_for(self, &sync_candidates);
        // The callers whose facts changed with them (a cached property whose helper now reads
        // an attribute): the rules reading their summaries elsewhere see the change too.
        let mut changed = changed;
        changed.extend(
            affected
                .iter()
                .filter(|k| {
                    before
                        .get(*k)
                        .zip(self.final_facts(k))
                        .is_none_or(|(old, new)| !old.same_facts(&new))
                })
                .cloned(),
        );
        changed.sort();
        changed.dedup();
        IncrementalUpdate {
            summary_changed,
            affected_callers: affected,
            changed,
            imports_changed,
        }
    }

    /// Transitive callers of `roots` on the reverse call graph, breadth first (sorted per
    /// level), without `roots` and `excluded`.
    fn callers_closure(
        &self,
        roots: &BTreeSet<FuncKey>,
        excluded: &FxHashSet<FuncKey>,
    ) -> Vec<FuncKey> {
        let mut visited: FxHashSet<FuncKey> = roots.iter().chain(excluded).cloned().collect();
        let mut queue: std::collections::VecDeque<FuncKey> = roots.iter().cloned().collect();
        let mut out = Vec::new();
        while let Some(cur) = queue.pop_front() {
            let mut callers: Vec<&FuncKey> = self
                .reverse_call_graph
                .get(&cur)
                .map(|s| s.iter().collect())
                .unwrap_or_default();
            callers.sort();
            for caller in callers {
                if visited.insert(caller.clone()) {
                    out.push(caller.clone());
                    queue.push_back(caller.clone());
                }
            }
        }
        out
    }

    /// Defining file of a summarized function.
    pub fn file_of(&self, func: &FuncKey) -> Option<&Path> {
        self.func_files.get(func).map(PathBuf::as_path)
    }

    /// The declaration of a summarized function, from its file's declarations.
    pub(crate) fn function_decl(&self, func: &FuncKey) -> Option<&FunctionDecl> {
        let decls = &self.files.get(self.func_files.get(func)?)?.decls;
        decls.functions.get(&func.qualname).map(Box::as_ref)
    }

    /// Whether the call `site` changes in place, through `bc`'s callee whose body runs at the
    /// call, the receiver it is made on (`callgraph::changes_receiver`): the call is then that
    /// change itself, as a builtin container's changing method is.
    pub fn changes_receiver(&self, site: &CallSite, bc: &BoundCallee) -> bool {
        self.summary(&bc.callee).is_some_and(|callee| {
            self.delivery(site, callee)
                .filter(|d| d.into == Facts::Body && d.via.is_none())
                .and_then(|d| callee.facts(d.from))
                .is_some_and(|facts| callgraph::changes_receiver(self, site, bc, facts))
        })
    }

    /// Drop a deleted file: its functions disappear and their callers are re-propagated (the
    /// update of an empty module, so Early Cutoff and `affected_callers` work as for edits;
    /// an unknown path changes nothing).
    pub fn remove_file(&mut self, path: &Path) -> IncrementalUpdate {
        let empty = FileSummary {
            path: path.to_path_buf(),
            ..FileSummary::default()
        };
        let update = self.update_file(empty);
        self.files.remove(path);
        update
    }

    pub(crate) fn all_functions(&self) -> impl Iterator<Item = FuncKey> + '_ {
        self.files
            .values()
            .flat_map(|f| f.local_functions.keys().cloned())
    }

    fn add_file_links(&mut self, file: &FileSummary) {
        for func in file.local_functions.keys() {
            let owned_elsewhere = self.func_files.get(func).is_some_and(|p| {
                p != &file.path && module_precedence(p) > module_precedence(&file.path)
            });
            if !owned_elsewhere {
                self.func_files.insert(func.clone(), file.path.clone());
            }
        }
        for (caller, callee, _) in file.call_edges() {
            self.reverse_call_graph
                .entry(callee.clone())
                .or_default()
                .insert(caller.clone());
        }
    }

    fn remove_file_links(&mut self, file: &FileSummary) {
        for func in file.local_functions.keys() {
            if self.func_files.get(func) == Some(&file.path) {
                self.func_files.remove(func);
            }
        }
        for (caller, callee, _) in file.call_edges() {
            if let Some(set) = self.reverse_call_graph.get_mut(callee) {
                set.remove(caller);
                if set.is_empty() {
                    self.reverse_call_graph.remove(callee);
                }
            }
        }
    }
}

/// No attributes (a fact a summary does not carry).
static NO_ATTRS: BTreeSet<String> = BTreeSet::new();

/// [`EffectSummary::hashed_whole_self_attrs`], none when the summary carries none.
fn hashed_whole(s: &EffectSummary) -> &BTreeSet<String> {
    s.hashed_whole_self_attrs.as_deref().unwrap_or(&NO_ATTRS)
}

/// The `self` attributes `a` and `b` together read only in the way `kind` holds: those either
/// holds, minus those either reads otherwise.
fn read_only_as(
    a: &EffectSummary,
    b: &EffectSummary,
    kind: impl Fn(&EffectSummary) -> &BTreeSet<String>,
) -> BTreeSet<String> {
    if kind(a).is_empty() && kind(b).is_empty() {
        return BTreeSet::new();
    }
    let otherwise = |s: &EffectSummary| -> BTreeSet<String> {
        s.reads_self_attrs
            .iter()
            .filter(|r| !kind(s).contains(&r.attr))
            .map(|r| r.attr.clone())
            .collect()
    };
    let otherwise = [otherwise(a), otherwise(b)];
    kind(a)
        .iter()
        .chain(kind(b))
        .filter(|attr| !otherwise.iter().any(|o| o.contains(*attr)))
        .cloned()
        .collect()
}

/// Whether `decorators` make a function an abstract method (`@abc.abstractmethod`).
pub(crate) fn is_abstract_method(decorators: &[String]) -> bool {
    decorators.iter().any(|d| {
        let base = d.split('(').next().unwrap_or(d).trim();
        base == "abstractmethod" || base.ends_with(".abstractmethod")
    })
}

#[cfg(test)]
mod tests;
