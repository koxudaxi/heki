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

pub mod cache_flow;
mod callgraph;
mod construction;
pub mod consumption;
mod contracts;
pub mod guards;
mod infer;
mod invalidation;
pub mod iterations;
pub mod ownership;
pub mod readonly;
mod report;
pub mod resolver;
mod scc;
mod summary;
pub mod types;

pub use cache_flow::{
    CacheEvent, CacheFlow, FlowBlock, SelfAttrWrite, attr_writes, self_attr_writes,
};
pub use contracts::Contracts;
pub use invalidation::{
    FlowWrite, FuncFlow, INITIALIZERS, Reach, SiteFlow, UnmetWrite, body_runs_at,
    builds_written_object, delegates_to_callers, is_initializer, reach_across,
};
pub use iterations::{
    BetweenSteps, ContainerChange, ContainerIteration, IteratedContainer, IteratedResult,
    IterationFacts, ResultIteration,
};
pub use ownership::{LocalOrigins, Origins, ResultOf};
pub use readonly::{ProtectedReturn, ReturnStep, Shared};
pub use report::{BoundaryReport, boundary_reports, boundary_reports_in};
pub use resolver::{
    Callee, ClassDecl, DeclKeys, DeclPasses, FunctionDecl, ModuleDecls, ModuleLayout, ProjectDecls,
    ReadOnlyAttr, ReadOnlyKind, StrConstant, StrValue, TypeCtx, Val, aliases_of, detect_src_layout,
    func_qualname, module_name_for_path, resolve_relative_module, self_aliases,
};
pub use summary::{BLOCKING_EFFECTS, CONTAINER_MUTATORS, offloaded_callable};

use heki_ast::Span;
use heki_ir::ModuleIr;
use heki_kb::{EffectiveEcosystemKb, ExecEffectKind};
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
/// of a typed write site without its position, chain, and object (`EffectSummary::
/// typed_field_writes`). A caller asks it whether a callee already sees the type of what it
/// writes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TypedFieldWrite {
    pub class_qualname: String,
    pub attr: String,
    pub kind: WriteKind,
    /// `TypedFieldWriteSite::fresh`.
    pub fresh: bool,
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
        self.mro.iter().any(|c| c == other.class()) || other.mro.iter().any(|c| c == self.class())
    }
}

pub type SharedLine = Arc<ClassLine>;

/// [`EffectSummary::receiver_effects`].
pub type ReceiverEffects = BTreeMap<ExecEffectKind, Vec<(SharedLine, SharedChain)>>;

/// What a method call's receiver tells about the implementations its callees run
/// ([`EffectSummary::receiver_effects`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dispatch {
    /// The receiver's class (a receiver of one project class, the class a constructor makes,
    /// or the caller's own `self`), when a callee is inherited from a proper base of it: the
    /// callee's facts may come from implementations of classes unrelated to it.
    pub receiver: Option<SharedLine>,
    /// For a call on the caller's own `self`: the callees (by index) that are overrides of
    /// subclasses, each with its class (it runs only when `self` is an instance of it).
    pub providers: Vec<(u16, SharedLine)>,
}

/// The hops of a kept chain, owned (what a report shows).
pub fn owned_chain(chain: &[Arc<CallHop>]) -> Vec<CallHop> {
    chain.iter().map(|hop| CallHop::clone(hop)).collect()
}

/// Zero the position of every hop of a kept chain (a hop shared with other chains is copied).
fn zero_hop_spans(chain: &mut [Arc<CallHop>]) {
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
    /// (`EffectSummary::returns_new`).
    pub returns: Vec<FuncKey>,
    /// Earlier calls handed the object to these `(callee, parameter)`: none may keep it
    /// (`EffectSummary::retains_params`).
    pub keeps: Vec<(FuncKey, u8)>,
}

impl NewCond {
    /// The conditions, sorted and without duplicates.
    pub fn new(mut returns: Vec<FuncKey>, mut keeps: Vec<(FuncKey, u8)>) -> Self {
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

    pub fn returns(&self) -> &[FuncKey] {
        self.0.as_ref().map_or(&[], |c| &c.returns)
    }

    pub fn keeps(&self) -> &[(FuncKey, u8)] {
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    },
}

impl ReturnsNew {
    /// The kind of a resolved `New` (a final summary's).
    pub fn kind(&self) -> Option<NewKind> {
        match self {
            Self::New { kind, cond } if cond.is_empty() => Some(*kind),
            _ => None,
        }
    }
}

/// A write heki cannot tie to a class (`MUT205`): an attribute written on an object of unknown
/// class, or a computed attribute name written on a project class. A local fact, never
/// propagated.
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
    /// Fully qualified class name of the read object (`pkg.model.Reference`).
    pub class_qualname: String,
    pub attr: String,
    pub kind: ReadKind,
    /// Parameters of the reading function the read object may be, as itself (bit 0: `self`).
    pub obj_params: u64,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadKind {
    /// `obj.attr`.
    DirectAttr,
    /// `getattr(obj, "attr"[, default])` / `object.__getattribute__(obj, "attr")`.
    GetAttr,
    /// `obj.__dict__["attr"]` / `obj.__dict__.get("attr")`.
    DictDirect,
}

impl ReadKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DirectAttr => "direct_attr",
            Self::GetAttr => "getattr",
            Self::DictDirect => "dict_direct",
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
}

impl WriteKind {
    /// Rebinding the attribute (or deleting it): the container it held, if any, is unchanged.
    pub fn rebinds(self) -> bool {
        matches!(
            self,
            Self::DirectAttr | Self::AttrDelete | Self::SetAttr | Self::DictDirect
        )
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
        }
    }
}

/// Unified interprocedural summary propagated bottom-up over Tarjan SCCs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectSummary {
    // Family 2: Execution Effect
    pub direct_exec_effects: BTreeSet<ExecEffectKind>,
    pub transitive_exec_effects: BTreeSet<ExecEffectKind>,
    /// Shortest known call chain reaching each effect (direct or transitive).
    pub exec_chains: BTreeMap<ExecEffectKind, SharedChain>,
    /// The execution effects reached only through implementations that subclasses of a
    /// method's class give its own `self` (a mixin method's `self.execute_command()`): per
    /// effect, each class providing one, with the shortest chain through it. A call on a
    /// receiver of known class runs only those of the classes related to it.
    pub receiver_effects: Option<Box<ReceiverEffects>>,

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
    /// `"a"` = write to / mutation of the argument's attribute `a`.
    pub mutated_param_paths: BTreeMap<u8, BTreeSet<String>>,
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
    /// `mutated_param_paths` that are not just a rebinding of the attribute (`p.a = v`). A
    /// shallow copy of the argument shares exactly these objects (`MUT202`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub changed_param_attrs: BTreeMap<u8, BTreeSet<String>>,
    /// `param index -> spans` of the function's own changes of the argument (a local fact,
    /// never propagated: a rule walking the call graph counts each change once, in the body
    /// making it; `EFF201` `mutates-params`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub param_change_sites: BTreeMap<u8, BTreeSet<Span>>,
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
    /// Names of the private methods the function calls on any receiver (`obj._name(...)`),
    /// resolved or not: `MUT107` leaves the methods a class family's initializers call this
    /// way to initialization. A local fact: never propagated.
    pub private_calls: BTreeSet<String>,
    /// The function may return a value shared with a read-only producer's other callers: it is a
    /// producer itself (`MUT202`; seeded by the declared producers it returns from, propagated
    /// from the project callees whose result it returns).
    pub returns_protected: Option<ProtectedReturn>,
    /// `invalidation` contracts (bits) whose invalidators every normal path of the function
    /// reaches, directly or through calls (`MUT109`; propagated).
    pub must_invalidate: u64,
    /// Writes to `invalidation` targets that some normal path leaves unannounced: the function's
    /// own, and those the private helpers it calls pass up (`MUT109`; propagated).
    pub unmet_invalidation: BTreeSet<UnmetWrite>,
    /// Parameters the function may keep beyond the call: stored in an object, a container, a
    /// global, or a closure cell, yielded, captured by a closure, handed to code heki does not
    /// follow, or to a project function that keeps them. Reading, writing, or passing a
    /// parameter to project functions that do not keep it keeps nothing (`fresh-objects`).
    pub retains_params: u64,
    /// The function returns a new object that nothing else holds (`fresh-objects`).
    pub returns_new: ReturnsNew,
    /// What the object the function returns may be, path by path: a new one, state that
    /// outlives the call, one of its parameters, or an element of one (`MUT112`; propagated from
    /// the project callees whose result it returns, [`Self::returns_results`]).
    #[serde(default, skip_serializing_if = "Origins::is_empty")]
    pub returns_origins: Origins,
    /// The project calls of the function whose results it may return (`MUT112`): propagation
    /// adds what they return to `returns_origins`, through the calls whose results their
    /// arguments may be ([`ArgOrigin::origins`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub returns_results: Vec<ResultOf>,
    /// The phis the returned object may be ([`Self::origin_nodes`] positions; `MUT112`, local).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub returns_nodes: Vec<u32>,
    /// What each phi the origin walks of the function meet may be ([`LocalOrigins::nodes`]),
    /// each walked once (`MUT112`, local).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub origin_nodes: Vec<LocalOrigins>,
    /// The function's own writes heki cannot tie to a class (`MUT205`; never propagated).
    pub unresolved_write_sites: BTreeSet<UnresolvedWriteSite>,
    /// The function's own reads and calls heki cannot tie to a class (`MUT206`, `MUT207`; never
    /// propagated).
    pub unresolved_access_sites: BTreeSet<UnresolvedAccessSite>,

    // Async offload & Sync-over-Async relaxation (dcg empirical findings)
    pub offloads_blocking_params: u64,
    pub sync_driven_only: bool,
    /// A call in this function (not its callees) could not be resolved, or CHA exceeded
    /// `resolver::CHA_CANDIDATE_LIMIT` candidates.
    pub unknown_calls: bool,

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
    /// Module-relative class qualname of a method (`Outer.Inner`), as in `IrFunction`.
    pub owner_class_qualname: Option<String>,
    pub decorators: Vec<String>,

    /// The guards every occurrence of an execution effect depends on (`guards`); an effect
    /// without an entry runs whenever the function does.
    pub exec_guards: BTreeMap<ExecEffectKind, EffectGuards>,

    /// Execution effects an ecosystem entry naming the function keeps from its callers: every
    /// one when it is `excluded`, the blocking ones when it is `async_safe` (applied to the
    /// final summary, whatever the callees bring).
    pub declared_hidden: BTreeSet<ExecEffectKind>,
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
    /// `isinstance(x, C)` / `isinstance(x, (C, D))`: the fully qualified classes, sorted.
    IsInstance(Vec<String>),
    /// `hasattr(x, "name")` with a string literal.
    HasAttr(String),
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
}

/// The exact class of a value a function knows: a constant, a display or comprehension, or a
/// buffer it constructs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
/// value decides it, its truthiness (`False`, `None`, `0`, `""`, and empty displays are falsy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConstArg {
    pub class: ConstClass,
    pub truthy: Option<bool>,
}

/// What a caller knows about the value of a callee parameter at one call, when the argument
/// binds the parameter exactly: the receiver, a keyword, a position before any `*` unpacking,
/// or the literal default of an omitted parameter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArgBinding {
    pub constant: Option<ConstArg>,
    /// The caller's own input the argument is (a guard subject of the caller).
    pub input: Option<GuardSubject>,
    /// Caller parameters the argument may come from: itself, its attributes and elements, and
    /// the results of calls on them.
    pub derived_params: u64,
    /// The argument may be an object the caller constructs, or come from one.
    pub derived_fresh: bool,
    /// The loops around the call (bit `d - 1`: the one at depth `d`) across whose iterations
    /// the argument stays one object: those that do not construct it.
    pub stays_in_loops: u64,
}

impl EffectSummary {
    /// Drop the execution effects in `hidden`, with their chains and guards, here and in what
    /// iterating the returned object runs.
    pub(crate) fn hide_exec_effects(&mut self, hidden: &BTreeSet<ExecEffectKind>) {
        if hidden.is_empty() {
            return;
        }
        self.direct_exec_effects.retain(|e| !hidden.contains(e));
        self.transitive_exec_effects.retain(|e| !hidden.contains(e));
        self.exec_chains.retain(|e, _| !hidden.contains(e));
        if let Some(r) = &mut self.receiver_effects {
            r.retain(|e, _| !hidden.contains(e));
            if r.is_empty() {
                self.receiver_effects = None;
            }
        }
        self.exec_guards.retain(|e, _| !hidden.contains(e));
        if let Some(iterated) = &mut self.iterated {
            iterated.hide_exec_effects(hidden);
        }
    }

    /// Copy with every source location zeroed: Early Cutoff compares facts, not positions,
    /// so an edit that only moves code does not notify callers.
    pub fn without_locations(&self) -> Self {
        let mut out = self.clone();
        for chain in out.exec_chains.values_mut() {
            zero_hop_spans(chain);
        }
        for (_, chain) in out
            .receiver_effects
            .iter_mut()
            .flat_map(|r| r.values_mut())
            .flatten()
        {
            zero_hop_spans(chain);
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
        out.param_change_sites.clear();
        out.iterations = out.iterations.map(|f| Box::new(f.without_locations()));
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

    /// Equality of everything callers copy into their own summaries: the facts, and the
    /// positions of call chains and write sites. Only the positions of the function's own read
    /// sites, parameter and container changes, iterations, and unresolved accesses are left out:
    /// those are never propagated.
    pub fn same_for_callers(&self, other: &Self) -> bool {
        let without_local = |s: &Self| Self {
            typed_field_read_sites: BTreeSet::new(),
            param_change_sites: BTreeMap::new(),
            iterations: s.iterations.as_ref().and_then(|f| f.propagated()),
            unresolved_write_sites: BTreeSet::new(),
            unresolved_access_sites: BTreeSet::new(),
            ..s.clone()
        };
        without_local(self) == without_local(other) && self.same_facts(other)
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

    /// Add the facts of the typed write sites to `typed_field_writes` (propagation adds those
    /// of the callees).
    pub(crate) fn refresh_typed_field_writes(&mut self) {
        for s in &self.typed_field_write_sites {
            let fact = TypedFieldWrite {
                class_qualname: s.class_qualname.clone(),
                attr: s.attr.clone(),
                kind: s.kind,
                fresh: s.fresh,
            };
            self.typed_field_writes.insert(fact);
        }
        if let Some(i) = &mut self.iterated {
            i.refresh_typed_field_writes();
        }
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
    /// lazily (`memo_param_paths`); `attrs`: only below those of its attributes.
    pub fn changes_param_state(&self, p: u8, attrs: Option<&BTreeSet<String>>) -> bool {
        let memos = self.memo_param_paths.as_deref().and_then(|m| m.get(&p));
        let paths = self.mutated_param_paths.get(&p).into_iter().flatten();
        paths
            .filter(|path| attrs.is_none_or(|a| a.contains(*path)))
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

    /// The chain by which the function reaches execution effect `e` on a receiver of class
    /// `receiver` (`None`: unknown), or `None` when only implementations of classes unrelated to
    /// it reach `e` ([`Self::receiver_effects`]).
    pub fn effect_chain_for(
        &self,
        e: ExecEffectKind,
        receiver: Option<&ClassLine>,
    ) -> Option<&[Arc<CallHop>]> {
        match (
            self.receiver_effects.as_ref().and_then(|r| r.get(&e)),
            receiver,
        ) {
            (Some(provided), Some(r)) => provided
                .iter()
                .filter(|(p, _)| p.related(r))
                .map(|(_, chain)| chain.as_slice())
                .min_by_key(|chain| chain.len()),
            _ => Some(self.exec_chains.get(&e).map_or(&[][..], Vec::as_slice)),
        }
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArgOrigin {
    /// Caller parameter `i` itself (`None`) or its attribute `a` (`Some(a)`).
    pub param: Option<(u8, Option<String>)>,
    /// A slot of caller parameter `i` itself, one level deep (`p.a`, `p["k"]`).
    pub param_slot: Option<(u8, Slot)>,
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
    /// The argument is an instance the caller constructs (`Model(...)`), or an attribute of one.
    pub fresh: bool,
    /// Where the argument comes from among the objects that existed before the call (`MUT109`;
    /// only computed when an `invalidation` contract is configured).
    pub reach: Reach,
    /// The `invalidation` targets a callee may write through the argument without knowing them
    /// (`MUT109`; `None` unless a contract names `typed_field`, or an attribute of
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
}

/// [`ArgOrigin::targets`]: what the argument's class and field are to the `invalidation`
/// contracts, which propagation cannot look up (it has no class hierarchy).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArgTargets {
    /// The contracts whose targets include the field the argument is (`typed_field`): changing
    /// the value in place writes it.
    pub field: u64,
    /// Each of the [`ArgOrigin::instance_classes`], first, with its project bases, when a
    /// contract names an attribute of one of them (empty otherwise): [`Contracts::target_of`]
    /// with an attribute a callee writes on the instance, of the class the write is tied to.
    pub classes: Vec<Vec<String>>,
}

impl ArgOrigin {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Implementers {
    /// The project classes implementing the protocol structurally (none: no project class does).
    Listed(Vec<String>),
    /// More classes than the candidate limit, or a protocol without members.
    Unlisted,
}

/// One resolved project callee with the caller-side origin of each callee parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallTarget {
    /// Project functions (1 candidate, or up to `CHA_CANDIDATE_LIMIT` CHA candidates).
    Project(Vec<BoundCallee>),
    /// A Layer 2 catalog API (only recorded when it carries effects or offloads).
    Layer2 {
        symbol: String,
        /// Effects after the `is_awaited` filter.
        effects: BTreeSet<ExecEffectKind>,
        offloads_blocking_arg: Option<u8>,
        async_safe: bool,
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
    /// What follows the call on every normal path of the caller (`MUT109`; only computed when
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
    /// The callees bind the receiver of a method call (or the object a constructor makes) to
    /// their first parameter (a static method's binding aside), as the resolution of the call
    /// says (`Callee::Project`).
    pub binds_recv: bool,
    /// For a callable a library API runs (`Runner::Worker`, `Runner::SyncFacade`,
    /// `Runner::PerItem`): its position among the call's arguments (`None` for a keyword:
    /// `sorted(xs, key=f)`). Run on a worker, the callees take the positional arguments after
    /// it; through a sync facade or per item, none of the call's.
    pub handed_callable: Option<u8>,
    /// For a callable a builtin calls once per item (`Runner::PerItem`): the builtin and where
    /// it takes the items from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_item: Option<Box<PerItem>>,
    /// What the receiver tells about the implementations the callees run (`None`: nothing).
    pub dispatch: Option<Box<Dispatch>>,
    /// A call on `self` whose result nothing reads (`self.prepare()` as a statement): it
    /// reaches what its caller computes only through the attributes it writes (`MUT101`,
    /// [`EffectSummary::returns_self_attrs`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub result_discarded: bool,
    /// The object the call returns outlives one of the caller's uses of it
    /// ([`iterations::held_results`]): it escapes (stored, yielded, captured, handed to code heki
    /// does not follow), or a step that may leave a generator suspended (`next()`, a loop that
    /// leaves early) is followed by another use. Any code may run while the generator is
    /// suspended (`MUT113`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub result_held: bool,
}

/// How an API runs a callable it is handed ([`CallSite::runner`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Runner {
    /// Called directly, on the caller's thread.
    #[default]
    Caller,
    /// On a worker thread: an offloading API's callable (`asyncio.to_thread(f, x)`), a function
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
}

/// A builtin calling the callees of a site once per item ([`Runner::PerItem`]): `EFF102` and
/// `EFF103` count it as a loop as long as the items, which they size as a `for` loop's
/// iterable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerItem {
    /// The builtin (`builtins.sorted`).
    pub api: String,
    pub items: Items,
}

/// Where a builtin takes the items it calls a callable with ([`PerItem::items`]).
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
    /// Whether the effect `e` of the callees does not block the caller's thread here.
    pub fn hides(&self, e: ExecEffectKind) -> bool {
        match self.runner {
            Runner::Caller | Runner::PerItem => false,
            Runner::Worker => e.blocks(),
            Runner::SyncFacade => e == ExecEffectKind::BlockingDb,
        }
    }
}

impl CallSite {
    /// The class of the receiver ([`Dispatch::receiver`]).
    pub fn receiver(&self) -> Option<&ClassLine> {
        self.dispatch.as_ref()?.receiver.as_deref()
    }

    /// The class of the subclass providing the callee `bc` of a call on the caller's own
    /// `self` ([`Dispatch::providers`]).
    pub fn provider(&self, bc: &BoundCallee) -> Option<&SharedLine> {
        let i = self
            .project_callees()
            .iter()
            .position(|b| std::ptr::eq(b, bc))?;
        self.dispatch
            .as_ref()?
            .providers
            .iter()
            .find(|(k, _)| usize::from(*k) == i)
            .map(|(_, line)| line)
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
    /// Declarations other modules need for name and receiver resolution (shared with the
    /// project declarations built from the index).
    pub decls: Arc<ModuleDecls>,
    /// What summarizing the file read of the project declarations (completing its own
    /// declarations included): a change of any of these keys can change the summary. Recorded
    /// for the language server's index only (`summarize_project_with_reads`, the incremental
    /// updates); empty otherwise.
    pub decl_reads: DeclKeys,
    /// The `MUT109` flow facts of the file's functions (local, IR-free; empty unless an
    /// `invalidation` contract is configured).
    pub flows: FxHashMap<FuncKey, FuncFlow>,
    /// The `MUT101` facts of the file's methods that write a `self` attribute themselves, one
    /// per definition in source order (local, IR-free).
    pub cache_flows: FxHashMap<FuncKey, Vec<CacheFlow>>,
}

impl FileSummary {
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
#[derive(Debug, Default)]
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
    pub(crate) skips_typed_writes: bool,
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
pub(crate) fn module_precedence(path: &Path) -> (bool, bool, &Path) {
    let package = path.file_stem().is_some_and(|s| s == "__init__");
    (!is_stub(path), package, path)
}

/// Summarize every module of a project and propagate effects to a fixpoint.
pub fn analyze_project(modules: &[ModuleInput<'_>], kb: &EffectiveEcosystemKb) -> ProjectIndex {
    analyze_project_with(modules, kb, None)
}

/// [`analyze_project`] with the declared contracts the summaries depend on (`MUT202` producers,
/// `MUT109` invalidation contracts); the index keeps them for incremental updates.
pub fn analyze_project_with(
    modules: &[ModuleInput<'_>],
    kb: &EffectiveEcosystemKb,
    contracts: Option<Arc<dyn Contracts>>,
) -> ProjectIndex {
    analyze_project_for(
        modules,
        kb,
        contracts,
        TypedWrites::Propagated,
        OptInFacts::ALL,
    )
}

/// [`analyze_project_with`], propagating typed writes and extracting the opt-in facts only when
/// a rule reads them.
pub fn analyze_project_for(
    modules: &[ModuleInput<'_>],
    kb: &EffectiveEcosystemKb,
    contracts: Option<Arc<dyn Contracts>>,
    typed_writes: TypedWrites,
    opt_in: OptInFacts,
) -> ProjectIndex {
    let files = summarize(modules, kb, false, contracts.as_deref(), opt_in);
    ProjectIndex::build_for(files, contracts, typed_writes, opt_in)
}

/// Whether propagation carries typed writes (`EffectSummary::typed_field_write_sites` and
/// `typed_field_writes`) from callees to callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypedWrites {
    Propagated,
    /// No enabled rule reads propagated typed writes: each function keeps its own only.
    Local,
}

/// The facts of the opt-in rules that summaries and propagation compute only when a rule
/// reads them: `MUT112`'s return origins (`EffectSummary::returns_origins`, `returns_results`,
/// `returns_nodes`, `origin_nodes`, `ArgOrigin::origins`) and `MUT113`'s iteration facts
/// (`EffectSummary::iterations`, `CallSite::result_held`). `CallSite::returned`, which `MUT202`
/// reads too, is always computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptInFacts {
    pub return_origins: bool,
    pub iterations: bool,
}

impl OptInFacts {
    /// Every opt-in fact.
    pub const ALL: Self = Self {
        return_origins: true,
        iterations: true,
    };
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
    kb: &EffectiveEcosystemKb,
    contracts: Option<&dyn Contracts>,
    opt_in: OptInFacts,
) -> Vec<FileSummary> {
    summarize(modules, kb, true, contracts, opt_in)
}

fn summarize(
    modules: &[ModuleInput<'_>],
    kb: &EffectiveEcosystemKb,
    track: bool,
    contracts: Option<&dyn Contracts>,
    opt_in: OptInFacts,
) -> Vec<FileSummary> {
    let (decls, own) = resolver::project_decls_for(modules, kb, None, track);
    if track {
        // Every module's reads go to the one recorder of `decls`: one module at a time.
        return modules
            .iter()
            .zip(own)
            .map(|(m, own)| summary::summarize_module(m, &decls, kb, own, track, contracts, opt_in))
            .collect();
    }
    // A module's local summary depends on it and the declarations only (indexed collect: input
    // order).
    modules
        .par_iter()
        .zip(own)
        .map(|(m, own)| summary::summarize_module(m, &decls, kb, own, track, contracts, opt_in))
        .collect()
}

impl ProjectIndex {
    /// Build an index from local file summaries and propagate the whole project.
    pub fn build(files: Vec<FileSummary>) -> Self {
        Self::build_with(files, None)
    }

    /// Whether callers got the typed writes of their callees ([`TypedWrites`]).
    pub fn propagates_typed_writes(&self) -> bool {
        !self.skips_typed_writes
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
            skips_typed_writes: typed_writes == TypedWrites::Local,
            opt_in,
            ..Self::default()
        };
        for mut file in files {
            file.functions = file.local_functions.clone();
            index.add_file_links(&file);
            index.files.insert(file.path.clone(), file);
        }
        let all: FxHashSet<FuncKey> = index.all_functions().collect();
        callgraph::propagate(&mut index, &all);
        callgraph::recompute_sync_driven(&mut index);
        index
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
                for e in effects {
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
                        // A guard the call decides against: the effect does not run here.
                        let Some(through) =
                            guards::through_call(site, bc, facts.exec_guards.get(&e))
                        else {
                            continue;
                        };
                        let Some(tail) = facts.effect_chain_for(e, site.receiver()) else {
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

    /// What `site` hands its caller of the project callee summarized by `callee`: the one
    /// home of "does a body run here". A call runs a plain function's body, or an `async def`'s
    /// when it is awaited or driven (calling one only creates a coroutine), never a generator
    /// function's (calling one only creates the generator); the `Iterate`, `Return`, and
    /// `Through` sites of the object a call returns run what iterating it runs.
    pub fn delivery<'s>(&self, site: &'s CallSite, callee: &EffectSummary) -> Option<Delivery<'s>> {
        callgraph::delivery(|k| self.summary(k), site, callee)
    }

    /// Final summary of `func`, if known.
    pub fn summary(&self, func: &FuncKey) -> Option<&EffectSummary> {
        self.shared_summary(func).map(Arc::as_ref)
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
    /// module name replaces the earlier ones).
    fn preferred_decls(&self) -> Vec<Arc<ModuleDecls>> {
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
            own,
            true,
            self.contracts.as_deref(),
            self.opt_in,
        );
        self.update_file(file)
    }

    /// [`Self::reanalyze_module`] against cached project declarations (both passes, kept up
    /// to date in place): the edit costs the size of the module unless its declarations
    /// changed. Also returns what changed in the declarations (empty: nothing), which every
    /// file whose summary or diagnostics read it depends on.
    pub fn reanalyze_module_cached(
        &mut self,
        input: &ModuleInput<'_>,
        kb: &EffectiveEcosystemKb,
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
        let mut previous: FxHashMap<FuncKey, Arc<EffectSummary>> = FxHashMap::default();
        let mut owned_before: FxHashSet<FuncKey> = FxHashSet::default();
        if let Some(old) = &old_file {
            for (k, v) in &old.functions {
                if self.func_files.get(k) == Some(&path) {
                    owned_before.insert(k.clone());
                    previous.insert(k.clone(), v.clone());
                }
            }
            self.remove_file_links(old);
        }
        new_file.functions = new_file.local_functions.clone();
        // What the callers decided before the edit: the recomputation below reports a function
        // only when its flag differs from that.
        for (func, s) in &mut new_file.functions {
            if previous.get(func).is_some_and(|old| old.sync_driven_only) {
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
        self.files.insert(path, new_file);
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

        // Region 0: the file's functions plus every function sharing a (cross-file) SCC with
        // them, so cycles cannot keep stale effects of the old version alive.
        let mut region = callgraph::scc_closure(self, &own);
        for func in &region {
            if !own.contains(func)
                && let Some(s) = self.shared_summary(func)
            {
                previous.insert(func.clone(), Arc::clone(s));
            }
        }
        callgraph::reset_to_local(self, &region);
        callgraph::propagate(self, &region);
        let sync_changed = callgraph::recompute_sync_driven_for(self, &sync_candidates);

        let mut changed: BTreeSet<FuncKey> = BTreeSet::new();
        // Same facts, but positions callers copy (call chains, write sites) moved.
        let mut moved: BTreeSet<FuncKey> = BTreeSet::new();
        for func in &region {
            let now = self.summary(func);
            match (previous.get(func), now) {
                (Some(old), Some(new)) if old.same_facts(new) => {
                    if !old.same_for_callers(new) {
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

        // Callers whose facts depend on changed summaries (Early Cutoff when there are none),
        // then callers that only copy moved positions: their summaries are refreshed too, so
        // the index keeps what a full build computes, but no caller's facts change.
        let affected = self.callers_closure(&changed, &FxHashSet::default());
        let excluded: FxHashSet<FuncKey> = changed.iter().chain(&affected).cloned().collect();
        let relocated = self.callers_closure(&moved, &excluded);
        let summary_changed = !changed.is_empty();
        let changed: Vec<FuncKey> = changed.into_iter().collect();
        if affected.is_empty() && relocated.is_empty() {
            return IncrementalUpdate {
                summary_changed,
                affected_callers: Vec::new(),
                changed,
            };
        }
        region = affected.iter().chain(&relocated).cloned().collect();
        let before: FxHashMap<FuncKey, EffectSummary> = affected
            .iter()
            .filter_map(|k| Some((k.clone(), self.summary(k)?.clone())))
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
                        .zip(self.summary(k))
                        .is_none_or(|(old, new)| !old.same_facts(new))
                })
                .cloned(),
        );
        changed.sort();
        changed.dedup();
        IncrementalUpdate {
            summary_changed,
            affected_callers: affected,
            changed,
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
