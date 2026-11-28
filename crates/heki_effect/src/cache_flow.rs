//! `MUT101` facts (docs/ANALYSIS_AND_KB_SPEC.md section 2.3): a method's own writes of `self`
//! attributes, and the control flow from them to the points that invalidate or read a cached
//! property of its instance.
//!
//! They are read from the IR once, when the method is summarized, and kept in
//! [`crate::FileSummary::cache_flows`]: whether a write leaves a cached value behind is then
//! decided from these facts and the summaries alone, also for a method of a file that is not
//! checked (the language server checks one file).

use crate::readonly::{ContainerKind, ReturnStep, Shared};
use crate::resolver::namespace_load;
use crate::{CallSite, FuncKey, TypeCtx, Val, aliases_of, container_mutator, guards, self_aliases};
use heki_ast::Span;
use heki_ir::walk::{self, Answer, Budget, Def, FnShape, Use, Visit};
use heki_ir::{
    BlockId, BuiltinKind, CompareOpKind, ConstLiteral, InstKind, IrFunction, Terminator, VarId,
};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The `MUT101` facts of one definition of a method that writes a `self` attribute itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheFlow {
    /// The attribute and property names the writes and events refer to, by index.
    pub names: Vec<String>,
    /// The method's own writes of `self.<attr>`, in source order.
    pub writes: Vec<SelfAttrWrite>,
    /// Every block of the method, by `BlockId`.
    pub blocks: Vec<FlowBlock>,
}

/// A write of `self.<attr>` in a method's own body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfAttrWrite {
    /// The attribute, an index into [`CacheFlow::names`].
    pub attr: u32,
    pub span: Span,
    pub block: BlockId,
    /// The write's instruction index in `block`.
    pub index: u32,
    /// `setdefault`, or a subscript store guarded by `if k not in self.<attr>:`.
    pub memo_shaped: bool,
    /// `self.<attr> = ...` / `del self.<attr>`: the attribute is rebound, not changed in place.
    pub rebinds: bool,
    /// The write runs only where a `None` / falsy test of `self.<attr>` found it unset
    /// (`if self.<attr> is None:`, `if not self.<attr>:`): a lazy initialization.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lazy_init: bool,
    /// What `MUT101`'s exact claim reads of the write ([`KeptWrite`]), boxed as `cache_flow`
    /// computes it only while an enabled rule reads it (`OptInFacts::kept_values`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept: Option<Box<KeptWrite>>,
}

/// What `MUT101`'s exact claim reads of a write ([`SelfAttrWrite::kept`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeptWrite {
    /// The literal key of an item store or delete (`self.<attr>["k"] = v`), as
    /// `EffectSummary::mutates_self_attrs` keys it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// An in-place change of a builtin container the attribute holds by its type (`MUT101`'s
    /// "Plain write").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<InPlaceChange>,
}

/// An in-place change of the builtin `list`, `dict`, or `set` an attribute holds by its type
/// (a construction or an annotation, R2): the method that makes it, called by name or run by
/// an item store or delete (`__setitem__`, `__delitem__`). Whether a project subclass of the
/// container replaces that method is decided where the claim is made
/// (`ProjectDecls::method_not_overridden`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InPlaceChange {
    pub kind: ContainerKind,
    pub method: String,
    pub by_name: bool,
}

/// The values kept in an instance `__dict__` (a `@cached_property`'s) a function may remove or
/// replace, on any object (`KeptFacts::removes_kept`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeptRemoval {
    #[default]
    Nothing,
    /// The values kept under these names: `del obj.<name>`, an item of an object's `__dict__`
    /// stored or removed under the key `<name>`.
    Named(Box<BTreeSet<String>>),
    /// Any: clearing or replacing an object's `__dict__`, waiting while other code runs
    /// (`await`), or running code heki does not follow (a call it cannot resolve, a library
    /// call outside the catalog, a catalog call handed a callable, an instance of a project
    /// class, or a value of unknown type, a call whose targets are not every implementation it
    /// can run).
    Any,
}

impl KeptRemoval {
    /// Whether the value kept under `name` may be removed or replaced.
    pub fn removes(&self, name: &str) -> bool {
        match self {
            Self::Nothing => false,
            Self::Named(names) => names.contains(name),
            Self::Any => true,
        }
    }

    pub fn add(&mut self, name: &str) {
        match self {
            Self::Nothing => *self = Self::Named(Box::new(BTreeSet::from([name.to_owned()]))),
            Self::Named(names) => {
                names.insert(name.to_owned());
            }
            Self::Any => {}
        }
    }

    pub fn set_any(&mut self) {
        *self = Self::Any;
    }

    pub fn join(&mut self, other: &Self) {
        match other {
            Self::Nothing => {}
            Self::Named(names) => names.iter().for_each(|n| self.add(n)),
            Self::Any => self.set_any(),
        }
    }
}

/// What one instruction may do to the values kept in some object's `__dict__`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeptRemovalKind {
    /// Remove the value kept under this name, so that the getter runs again.
    Named(String),
    /// Replace the value kept under this name with another (a store).
    Replaces(String),
    /// Remove or replace any.
    Any,
    /// Hand control to the generator's consumer (`yield`), which may run anything before the
    /// method resumes; a caller of the method is that consumer, so it is no fact of the
    /// method's summary.
    Suspends,
    /// Run the bodies of the project generators whose call is at this span (a step of iterating
    /// what they return): what they remove is in their summaries.
    Runs(Span),
}

/// An instruction of a method that may remove a kept value ([`KeptRemovalKind`]).
#[derive(Debug, Clone)]
pub(crate) struct KeptPoint {
    pub block: BlockId,
    pub index: u32,
    pub kind: KeptRemovalKind,
}

impl SelfAttrWrite {
    /// A write that fills a memo rather than changing what was computed from the object: a
    /// memo-shaped write (`setdefault`, a store under `if k not in <obj>.<attr>:`), or an
    /// in-place change of an attribute named as a memo or cache ([`is_memo_name`]). Rebinding
    /// such an attribute is no memo operation (`self.cache_size = n`).
    pub fn fills_memo(&self, attr: &str) -> bool {
        self.memo_shaped || !self.rebinds && is_memo_name(attr)
    }
}

/// Memo / cache dictionaries by name: a word of the attribute's name is `cache...` or `memo...`
/// (`self._cache`, `self.memo`, `self._resultCache`, `self.memoized`, `_CACHE`,
/// `HTTPCache`), not `memory`. A capital starts a word after a small letter, and after a
/// capital when a small letter follows it: a run of capitals is one word.
pub fn is_memo_name(attr: &str) -> bool {
    let mut words: Vec<String> = Vec::new();
    for part in attr.split('_') {
        let chars: Vec<char> = part.chars().collect();
        let mut word = String::new();
        for (i, c) in chars.iter().enumerate() {
            let after_small = i > 0 && chars[i - 1].is_ascii_lowercase();
            let ends_capitals = i > 0
                && chars[i - 1].is_ascii_uppercase()
                && chars.get(i + 1).is_some_and(char::is_ascii_lowercase);
            if c.is_ascii_uppercase() && (after_small || ends_capitals) {
                words.push(std::mem::take(&mut word));
            }
            word.push(c.to_ascii_lowercase());
        }
        words.push(word);
    }
    words
        .iter()
        .any(|w| w.starts_with("cache") || w.starts_with("memo") && !w.starts_with("memor"))
}

/// A container a function fills with results computed from its arguments and hands them out
/// from again, as `functools.cache` keeps them ([`hand_memo`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandMemo {
    /// The container as the function names it: `self._cache`, `cls._cache`, `Configs._cache`,
    /// `_CACHE`.
    pub container: String,
    /// What a returned value shares with a kept one, joined over the returns that hand one out.
    pub shared: Shared,
}

/// A [`HandMemo`], and whether the memo keeps every result of its function.
pub(crate) struct MemoFound {
    pub memo: HandMemo,
    /// Every return a run can reach hands out, through copies and phis, the object the memo
    /// keeps under one key (the value it reads or the value it stores there), and the
    /// container's name says cache or memo ([`is_memo_name`]): a container of state the callers
    /// share on purpose is rarely named so.
    pub exact: bool,
}

/// The holder number of a module global that is itself a memo container ([`hand_memo`]).
const GLOBAL_CONTAINER: u32 = u32::MAX;
/// The holder number of a method's receiver (`self`, `cls`) ([`hand_memo`]).
const RECEIVER: u32 = 0;

/// How a returned value reads a container under a key.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lookup {
    /// `c[k]`.
    Item,
    /// `c.get(k)`.
    Get,
    /// `c.get(k, default)`: the default is no kept object.
    GetOr,
    /// `c.setdefault(k, v)`: the kept value, after storing `v` when `k` was missing.
    SetDefault,
}

/// Where a value is read from a container under a key: the instruction's block and index, the
/// container, the key, and the lookup ([`container_read`]).
type ContainerRead = ((BlockId, u32), VarId, VarId, Lookup);

/// A value the function returns that reads a container that outlives the call under a key.
struct MemoRead<'f> {
    value: VarId,
    /// The block and index of the instruction reading it.
    at: (BlockId, u32),
    held: Held<'f>,
    key: VarId,
    lookup: Lookup,
}

/// A store of a computed value into a container under a key derived from the arguments.
struct MemoFill<'f> {
    held: Held<'f>,
    key: VarId,
    value: VarId,
    at: (BlockId, u32),
}

/// The memo `f` keeps its results in, when it is a cache by hand: what it returns on some path
/// is read from a container that outlives the call (an attribute of the method's receiver, an
/// attribute of a module global such as a class, or a module global), under a key derived from
/// its arguments, where the function also stores into that container, under the same key, a
/// value computed from them, and the read runs on some path that passes no such store, so a
/// later call with the same arguments gets the kept object.
///
/// Not a memo: a container the function only reads (a cache object's accessor), one it fills
/// with a new object built from none of its arguments or with an argument as it is (a registry
/// of state the callers share on purpose), one it rebinds other than lazily (a container made
/// per call), a key derived from none of the arguments, and returns that only copy the kept
/// value. `receiver` says whether the first parameter is the method's `self` / `cls`;
/// `returned` is [`crate::readonly::returned_values`]. Past the function's budget, none.
pub(crate) fn hand_memo(
    shape: &FnShape<'_>,
    live: &crate::ownership::Live,
    receiver: bool,
    returned: &FxHashMap<VarId, ReturnStep>,
) -> Option<MemoFound> {
    let f = shape.f;
    // Most functions return no item of a container.
    let mut reads: Vec<(VarId, ContainerRead)> = returned
        .keys()
        .filter_map(|v| Some((*v, container_read(shape, *v)?)))
        .collect();
    if reads.is_empty() {
        return None;
    }
    reads.sort_by_key(|(v, _)| shape.inst(*v).map(|i| i.span));
    let key_params: FxHashSet<VarId> = f
        .params
        .iter()
        .skip(usize::from(receiver))
        .map(|p| p.var)
        .collect();
    if key_params.is_empty() {
        return None;
    }
    let mut objects: FxHashMap<VarId, u32> = FxHashMap::default();
    if receiver {
        objects.extend(self_vars(f).into_iter().map(|v| (v, RECEIVER)));
    }
    let mut globals: Vec<&str> = Vec::new();
    let mut containers: Vec<(VarId, Held<'_>)> = Vec::new();
    for inst in f.blocks.iter().flat_map(|b| &b.insts) {
        if let InstKind::LoadGlobal { dst, name, .. } = &inst.kind {
            let n = globals.iter().position(|g| *g == name).unwrap_or_else(|| {
                globals.push(name);
                globals.len() - 1
            });
            objects.insert(*dst, u32::try_from(n + 1).unwrap_or(GLOBAL_CONTAINER - 1));
            containers.push((*dst, (GLOBAL_CONTAINER, name.as_str())));
        }
    }
    let alias = holders(shape, &objects, &containers);
    let reads: Vec<MemoRead<'_>> = reads
        .into_iter()
        .filter_map(|(value, (at, container, key, lookup))| {
            Some(MemoRead {
                value,
                at,
                held: *alias.get(&container)?,
                key,
                lookup,
            })
        })
        .collect();
    if reads.is_empty() {
        return None;
    }
    let budget = Budget::of_function("cache_flow::hand_memo", f);
    let derived = derived_from(shape, &budget, &key_params)?;
    // A result computed from the arguments (`Schema(raw)`, `{"name": key}`), not an argument as
    // it is nor a new object built from none of them (`[]`, `State()`).
    let computed =
        |v: VarId| derived.contains(&v) && !matches!(shape.def(shape.copy_root(v)), Def::Param);
    let mut fills: Vec<MemoFill<'_>> = Vec::new();
    let mut rebound: FxHashSet<Held<'_>> = FxHashSet::default();
    for (held, w, op, _) in write_ops(shape, &objects, &alias) {
        let Some((_, method, _)) = op else {
            if !w.lazy_init {
                rebound.insert(held);
            }
            continue;
        };
        let (key, value) = match (method, &f.block(w.block).insts[w.index as usize].kind) {
            ("__setitem__", InstKind::SubscriptStore { index, value, .. }) => (*index, *value),
            ("setdefault", InstKind::MethodCall { args, .. }) if args.len() == 2 => {
                (args[0], args[1])
            }
            _ => continue,
        };
        if derived.contains(&key) && computed(value) {
            fills.push(MemoFill {
                held,
                key,
                value,
                at: (w.block, w.index),
            });
        }
    }
    for inst in f.blocks.iter().flat_map(|b| &b.insts) {
        if let InstKind::StoreGlobal { name, .. } = &inst.kind {
            rebound.insert((GLOBAL_CONTAINER, name.as_str()));
        }
    }
    let filled_under = |read: &MemoRead<'_>| {
        fills
            .iter()
            .any(|w| w.held == read.held && same_key(shape, &budget, read.key, w.key))
    };
    let hits: Vec<(&MemoRead<'_>, Shared)> = reads
        .iter()
        .filter(|r| !rebound.contains(&r.held) && derived.contains(&r.key) && filled_under(r))
        .filter(|r| {
            r.lookup == Lookup::SetDefault || runs_before_fill(shape, live, &budget, &fills, r)
        })
        .filter_map(|r| {
            let shared = returned.get(&r.value)?.apply(Shared::produced(true));
            shared.object.then_some((r, shared))
        })
        .collect();
    let (first, _) = hits.first()?;
    let held = first.held;
    let stored: Vec<&MemoFill<'_>> = fills.iter().filter(|w| w.held == held).collect();
    let shared = (hits.iter().filter(|(r, _)| r.held == held).map(|(_, s)| *s))
        .chain(
            stored
                .iter()
                .filter_map(|w| Some(returned.get(&w.value)?.apply(Shared::produced(true)))),
        )
        .fold(Shared::default(), Shared::join);
    // The objects kept under the first hit's key: the values read there and those stored there.
    let one_key = |k: VarId| same_key(shape, &budget, first.key, k);
    let kept: FxHashSet<VarId> = (hits.iter())
        .filter(|(r, _)| r.held == held && r.lookup != Lookup::GetOr && one_key(r.key))
        .map(|(r, _)| r.value)
        .chain(stored.iter().filter(|w| one_key(w.key)).map(|w| w.value))
        .map(|v| shape.copy_root(v))
        .collect();
    let (holder, name) = held;
    let container = match holder {
        GLOBAL_CONTAINER => name.to_owned(),
        RECEIVER => format!("{}.{name}", f.params.first().map_or("self", |p| &p.name)),
        n => format!("{}.{name}", globals[n as usize - 1]),
    };
    let exact = is_memo_name(name) && every_return_kept(shape, live, &budget, &kept);
    Some(MemoFound {
        memo: HandMemo { container, shared },
        exact,
    })
}

/// Where `v` is read from a container under a key.
fn container_read(shape: &FnShape<'_>, v: VarId) -> Option<ContainerRead> {
    let Def::Inst(block, index) = shape.def(v) else {
        return None;
    };
    let at = (block, index);
    match &shape.f.block(block).insts[index as usize].kind {
        InstKind::SubscriptLoad {
            obj,
            index,
            is_slice: false,
            ..
        } => Some((at, *obj, *index, Lookup::Item)),
        InstKind::MethodCall {
            recv,
            method,
            args,
            kwargs,
            star_from: None,
            ..
        } if kwargs.is_empty() => match (method.as_str(), args.as_slice()) {
            ("get", [k]) => Some((at, *recv, *k, Lookup::Get)),
            ("get", [k, _]) => Some((at, *recv, *k, Lookup::GetOr)),
            ("setdefault", [k, _]) => Some((at, *recv, *k, Lookup::SetDefault)),
            _ => None,
        },
        _ => None,
    }
}

/// Whether `read` runs on some path from the entry that passes no store of `fills` into its
/// container: the call finds the key kept and returns what an earlier call stored. Blocks are
/// visited once; a block with such a store leaves only through its exception edge.
fn runs_before_fill(
    shape: &FnShape<'_>,
    live: &crate::ownership::Live,
    budget: &Budget,
    fills: &[MemoFill<'_>],
    read: &MemoRead<'_>,
) -> bool {
    let f = shape.f;
    let (block, index) = read.at;
    let mut first_fill: FxHashMap<BlockId, u32> = FxHashMap::default();
    for w in fills.iter().filter(|w| w.held == read.held) {
        let at = first_fill.entry(w.at.0).or_insert(w.at.1);
        *at = (*at).min(w.at.1);
    }
    let found = walk::reach(
        budget,
        f.blocks.first().map(|b| b.id),
        |b, next| {
            let bb = f.block(b);
            if !first_fill.contains_key(&b) {
                let succs = bb.terminator.successors();
                next.extend(succs.into_iter().filter(|s| live.edge(b, *s)));
            }
            next.extend(bb.exc_target);
        },
        |b| {
            let fill = first_fill.get(&b).copied().unwrap_or(u32::MAX);
            if b == block && index < fill {
                Visit::Stop(())
            } else {
                Visit::Expand
            }
        },
    );
    found == Answer::Known(Some(()))
}

/// Whether every return a run can reach hands out, through copies and phis (along the edges a
/// run can take), one of the `kept` values (copy roots).
fn every_return_kept(
    shape: &FnShape<'_>,
    live: &crate::ownership::Live,
    budget: &Budget,
    kept: &FxHashSet<VarId>,
) -> bool {
    let f = shape.f;
    let mut returned = Vec::new();
    for b in f.blocks.iter().filter(|b| live.block(b.id)) {
        match b.terminator {
            Terminator::Return(Some(v)) => returned.push(v),
            Terminator::Return(None) => return false,
            _ => {}
        }
    }
    // The walk visits copy roots only: a copy hands out what its root holds.
    let other = walk::reach(
        budget,
        returned.into_iter().map(|v| shape.copy_root(v)),
        |v, next| {
            if let Def::Phi(b, i) = shape.def(v) {
                let phi = &f.block(b).phis[i as usize];
                let live_in = phi.incoming.iter().filter(|(from, _)| live.edge(*from, b));
                next.extend(live_in.map(|(_, v)| shape.copy_root(*v)));
            }
        },
        |v| {
            if kept.contains(&v) {
                Visit::Prune
            } else if matches!(shape.def(v), Def::Phi(..)) {
                Visit::Expand
            } else {
                Visit::Stop(())
            }
        },
    );
    other == Answer::Known(None)
}

/// The values derived from `params`: they themselves, and every value an instruction or a phi
/// computes from one (`id(raw)`, `(a, b)`, `name.lower()`), each reached once. Past `budget`,
/// none.
fn derived_from(
    shape: &FnShape<'_>,
    budget: &Budget,
    params: &FxHashSet<VarId>,
) -> Option<FxHashSet<VarId>> {
    let f = shape.f;
    let mut derived: FxHashSet<VarId> = FxHashSet::default();
    let walked = walk::reach(
        budget,
        params.iter().copied(),
        |v, next| {
            next.extend(shape.readers(v).iter().filter_map(|u| match *u {
                Use::Inst(b, i) => f.block(b).insts[i as usize].kind.dst(),
                Use::Phi(b, i) => Some(f.block(b).phis[i as usize].dst),
                Use::Terminator(_) => None,
            }));
        },
        |v| {
            derived.insert(v);
            Visit::<()>::Expand
        },
    );
    (!walked.is_open()).then_some(derived)
}

/// One block of a method: its normal control flow and the points that invalidate or read a
/// cached property, by instruction index.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowBlock {
    pub succs: Vec<BlockId>,
    /// The block ends in `return`.
    pub returns: bool,
    /// Where an exception raised in the block continues.
    pub exc_target: Option<BlockId>,
    pub events: Vec<(u32, CacheEvent)>,
}

/// What one instruction does to the cached properties of the method's own instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheEvent {
    /// `del self.<name>` (a name index).
    DeleteAttr(u32),
    /// Reading `self.<name>`: a cached property named so computes its value again.
    LoadAttr(u32),
    /// Removing the entry of a constant key from `self.__dict__` (`del self.__dict__[k]`,
    /// `.pop(k)`, `.__delitem__(k)`), with the key as a literal or a `Final` string constant.
    RemoveEntry(u32),
    /// `self.__dict__.clear()`.
    ClearDict,
    /// A call whose call site has a `self` receiver: what it does is in its callees' summaries.
    SelfCall(Span),
    /// A point that may remove or replace the value kept under a name (a name index) in some
    /// object's `__dict__` ([`KeptRemovalKind::Named`]).
    MayRemove(u32),
    /// A point that may remove or replace any kept value ([`KeptRemovalKind::Any`],
    /// [`KeptRemovalKind::Suspends`]).
    MayRemoveAny,
    /// A project call at the point that no `SelfCall` event stands for (another receiver, a
    /// property getter a read of `self.<name>` runs), or a step of iterating what the project
    /// generators called at the span return: what it removes is in its callees' summaries.
    Calls(Span),
}

impl CacheFlow {
    pub fn name(&self, index: u32) -> &str {
        &self.names[index as usize]
    }

    /// The method's own writes of `self.<attr>`, in source order.
    pub fn writes_of<'s>(&'s self, attr: &'s str) -> impl Iterator<Item = &'s SelfAttrWrite> {
        self.writes
            .iter()
            .filter(move |w| self.name(w.attr) == attr)
    }

    /// The attributes the method initializes lazily (a write of
    /// [`SelfAttrWrite::lazy_init`]).
    pub fn lazily_initialized(&self) -> impl Iterator<Item = &str> {
        self.writes
            .iter()
            .filter(|w| w.lazy_init)
            .map(|w| self.name(w.attr))
    }
}

/// The `MUT101` facts of `f` (`None` for a function that is no method or writes no `self`
/// attribute itself). `sites` are the function's call sites, `types` its inferred values, and
/// `kept` the points of it that may remove a kept value (`None` unless
/// [`crate::OptInFacts::kept_values`]).
pub(crate) fn cache_flow(
    shape: &FnShape<'_>,
    key: &FuncKey,
    sites: &[CallSite],
    tcx: &TypeCtx<'_>,
    types: &FxHashMap<VarId, Val>,
    kept: Option<&[KeptPoint]>,
) -> Option<CacheFlow> {
    let f = shape.f;
    if f.is_class_body || f.is_module_body || f.owner_class_qualname.is_none() {
        return None;
    }
    let writes = attr_write_ops(shape, &self_objects(f), &[]);
    if writes.is_empty() {
        return None;
    }
    let mut names = Names::default();
    let mut writes: Vec<SelfAttrWrite> = writes
        .into_iter()
        .map(|((_, attr), w, op, key)| SelfAttrWrite {
            attr: names.index(attr),
            kept: kept.and_then(|_| KeptWrite::of(key, op, types)),
            ..w
        })
        .collect();
    // The collection reuses the buffer of the larger tuples it maps from; the index keeps the
    // flow for the whole run.
    writes.shrink_to_fit();
    let blocks = flow_blocks(shape, key, sites, tcx, types, kept, &mut names);
    Some(CacheFlow {
        names: names.list,
        writes,
        blocks,
    })
}

impl KeptWrite {
    /// The facts of a write with the literal key `key` and the in-place op `op` (`None` when it
    /// has neither).
    fn of(
        key: Option<String>,
        op: Option<InPlaceOp<'_>>,
        types: &FxHashMap<VarId, Val>,
    ) -> Option<Box<Self>> {
        let change = op.and_then(|op| in_place_change(op, types));
        (key.is_some() || change.is_some()).then(|| Box::new(Self { key, change }))
    }
}

/// [`KeptWrite::change`]: the in-place write `op` changes, through one of its changing
/// methods or an item store or delete, the builtin `list`, `dict`, or `set` its receiver holds
/// by the function's types.
fn in_place_change(op: InPlaceOp<'_>, types: &FxHashMap<VarId, Val>) -> Option<InPlaceChange> {
    let (recv, method, by_name) = op;
    let kind = match types.get(&recv)? {
        Val::Inst(class, _) => ContainerKind::ALL
            .into_iter()
            .find(|k| k.class() == class.as_str())?,
        _ => return None,
    };
    let changes = if by_name {
        kind.mutator(method)
    } else {
        kind.takes_item_stores()
    };
    changes.then(|| InPlaceChange {
        kind,
        method: method.to_owned(),
        by_name,
    })
}

#[derive(Default)]
struct Names {
    list: Vec<String>,
    index: FxHashMap<String, u32>,
}

impl Names {
    fn index(&mut self, name: &str) -> u32 {
        if let Some(&i) = self.index.get(name) {
            return i;
        }
        let i = u32::try_from(self.list.len()).unwrap_or(u32::MAX);
        self.list.push(name.to_owned());
        self.index.insert(name.to_owned(), i);
        i
    }
}

/// The variables of `f` that are its `self` (its first parameter, through copies).
fn self_vars(f: &IrFunction) -> FxHashSet<VarId> {
    f.params
        .first()
        .map(|p| self_aliases(f, p.var))
        .unwrap_or_default()
}

/// Writes of `self.<attr>` in `f`'s own body with their attribute, in source order (none
/// without a `self` parameter): rebinding or deleting it, and changing the container it holds
/// in place (also through local copies of `self.<attr>`). `self.__dict__` itself is no
/// attribute a property reads.
pub fn self_attr_writes<'f>(shape: &FnShape<'f>) -> Vec<(&'f str, SelfAttrWrite)> {
    attr_writes(shape, &self_objects(shape.f))
}

/// The variables of `f` that are its `self`, as the one object [`attr_writes`] reads.
fn self_objects(f: &IrFunction) -> FxHashMap<VarId, u32> {
    self_vars(f).into_iter().map(|v| (v, 0)).collect()
}

/// An attribute of one of the objects [`attr_writes`] reads: the object's number and the name.
pub type Held<'f> = (u32, &'f str);

/// [`self_attr_writes`] of several objects at once (`self`, parameters, shared values): the
/// writes of `<obj>.<attr>` in `f`'s own body, where `objects` gives each variable holding one
/// of them the object's number. Each object is read on its own: a test of one object's attribute
/// says nothing of another's.
pub fn attr_writes<'f>(
    shape: &FnShape<'f>,
    objects: &FxHashMap<VarId, u32>,
) -> Vec<(&'f str, SelfAttrWrite)> {
    container_writes(shape, objects, &[])
}

/// [`attr_writes`], and the changes of the containers `containers` holds itself, each as the
/// attribute it is given (a parameter's default container).
pub fn container_writes<'f>(
    shape: &FnShape<'f>,
    objects: &FxHashMap<VarId, u32>,
    containers: &[(VarId, Held<'f>)],
) -> Vec<(&'f str, SelfAttrWrite)> {
    attr_write_ops(shape, objects, containers)
        .into_iter()
        .map(|((_, attr), w, _, _)| (attr, w))
        .collect()
}

/// A write that changes the value of `<obj>.<attr>` in place: the variable holding the value,
/// the method that changes it (`__setitem__` for an item store, `__delitem__` for an item
/// delete), and whether the method is called by its name.
type InPlaceOp<'f> = (VarId, &'f str, bool);

/// One write [`attr_write_ops`] finds: the attribute written, the write, the [`InPlaceOp`] of a
/// write that does not rebind the attribute, and the literal key of an item store or delete
/// ([`KeptWrite::key`]).
type WriteOp<'f> = (
    Held<'f>,
    SelfAttrWrite,
    Option<InPlaceOp<'f>>,
    Option<String>,
);

/// The variables holding `<obj>.<attr>` for the objects `objects` holds, and the containers
/// `containers` holds itself, through copies (their copy roots, [`FnShape::copy_root`]).
struct Holders<'s, 'f> {
    shape: &'s FnShape<'f>,
    roots: FxHashMap<VarId, Held<'f>>,
}

impl<'f> Holders<'_, 'f> {
    fn get(&self, v: &VarId) -> Option<&Held<'f>> {
        self.roots.get(&self.shape.copy_root(*v))
    }
}

fn holders<'s, 'f>(
    shape: &'s FnShape<'f>,
    objects: &FxHashMap<VarId, u32>,
    containers: &[(VarId, Held<'f>)],
) -> Holders<'s, 'f> {
    let mut roots: FxHashMap<VarId, Held<'f>> = containers.iter().copied().collect();
    for inst in shape.f.blocks.iter().flat_map(|b| &b.insts) {
        if let InstKind::AttrLoad { dst, obj, attr } = &inst.kind
            && let Some(&o) = objects.get(obj)
            && attr != "__dict__"
        {
            roots.insert(*dst, (o, attr));
        }
    }
    Holders { shape, roots }
}

/// [`attr_writes`], each as a [`WriteOp`].
fn attr_write_ops<'f>(
    shape: &FnShape<'f>,
    objects: &FxHashMap<VarId, u32>,
    containers: &[(VarId, Held<'f>)],
) -> Vec<WriteOp<'f>> {
    write_ops(shape, objects, &holders(shape, objects, containers))
}

/// [`attr_write_ops`] with the holders ([`holders`]) found.
fn write_ops<'f>(
    shape: &FnShape<'f>,
    objects: &FxHashMap<VarId, u32>,
    alias: &Holders<'_, 'f>,
) -> Vec<WriteOp<'f>> {
    let f = shape.f;
    let mut defs: FxHashMap<VarId, &InstKind> = FxHashMap::default();
    for inst in f.blocks.iter().flat_map(|b| &b.insts) {
        if let Some(d) = inst.kind.dst() {
            defs.insert(d, &inst.kind);
        }
    }
    // The branches finding a key of `<obj>.<attr>` missing, by the block they lead into, with
    // the key tested: `if k not in <obj>.<attr>:`, the `else` of `if k in <obj>.<attr>:`, and a
    // `None` or falsy test of `<obj>.<attr>.get(k)`.
    let mut membership: FxHashMap<BlockId, Vec<(Held<'_>, VarId)>> = FxHashMap::default();
    for p in &f.blocks {
        if let Terminator::Branch {
            cond,
            then_bb,
            else_bb,
        } = &p.terminator
            && let Some(InstKind::Compare { op, lhs, rhs, .. }) = defs.get(cond)
            && matches!(op, CompareOpKind::In | CompareOpKind::NotIn)
            && let Some(&held) = alias.get(rhs)
        {
            let missing = if *op == CompareOpKind::NotIn {
                *then_bb
            } else {
                *else_bb
            };
            membership.entry(missing).or_default().push((held, *lhs));
        }
    }
    let def = |v: &VarId| defs.get(v).copied();
    let looked_up = |v: &VarId| match def(&shape.copy_root(*v))? {
        InstKind::MethodCall {
            recv, method, args, ..
        } if method == "get" && args.len() == 1 => Some((*alias.get(recv)?, args[0])),
        _ => None,
    };
    for t in guards::unset_branches(shape, &def, looked_up) {
        membership.entry(t.init).or_default().push(t.subject);
    }
    let budget = Budget::of_function("cache_flow::same_key", f);
    let guarded = |bb: BlockId, held: (u32, &str), index: &VarId| {
        membership.get(&bb).is_some_and(|tests| {
            tests
                .iter()
                .any(|(h, k)| *h == held && same_key(shape, &budget, *k, *index))
        })
    };
    // `(block, index, span, (object, attr), memo_shaped, in_place, key)` of each write (no
    // `in_place` op: it rebinds or deletes the attribute).
    let mut found = Vec::new();
    for bb in &f.blocks {
        for (index, inst) in bb.insts.iter().enumerate() {
            let rebound = match &inst.kind {
                InstKind::AttrDelete { obj, attr } => Some((*obj, attr.as_str())),
                kind => guards::stored_attr(shape, kind),
            }
            .filter(|(_, attr)| *attr != "__dict__");
            let w = match &inst.kind {
                _ if rebound.is_some() => rebound.and_then(|(obj, attr)| {
                    objects.get(&obj).map(|&o| ((o, attr), false, None, None))
                }),
                InstKind::SubscriptStore {
                    obj,
                    index,
                    key_const,
                    ..
                } => alias.get(obj).map(|&held| {
                    let op = (*obj, "__setitem__", false);
                    (
                        held,
                        guarded(bb.id, held, index),
                        Some(op),
                        key_const.clone(),
                    )
                }),
                InstKind::SubscriptDelete { obj, key_const, .. } => alias.get(obj).map(|&held| {
                    let op = (*obj, "__delitem__", false);
                    (held, false, Some(op), key_const.clone())
                }),
                InstKind::MethodCall { recv, method, .. } if container_mutator(method) => {
                    alias.get(recv).map(|&held| {
                        let op = (*recv, method.as_str(), true);
                        (held, method == "setdefault", Some(op), None)
                    })
                }
                _ => None,
            };
            if let Some((held, memo_shaped, in_place, key)) = w {
                found.push((bb.id, index, inst.span, held, memo_shaped, in_place, key));
            }
        }
    }
    if found.is_empty() {
        return Vec::new();
    }
    let mut written: FxHashMap<&str, FxHashSet<u32>> = FxHashMap::default();
    for (_, _, _, (o, attr), _, _, _) in &found {
        written.entry(attr).or_default().insert(*o);
    }
    let lazy = lazy_init_blocks(shape, objects, &written);
    let mut out: Vec<WriteOp<'f>> = found
        .into_iter()
        .map(|(block, index, span, held, memo_shaped, in_place, key)| {
            let write = SelfAttrWrite {
                attr: 0,
                span,
                block,
                index: u32::try_from(index).unwrap_or(u32::MAX),
                memo_shaped,
                rebinds: in_place.is_none(),
                lazy_init: lazy
                    .iter()
                    .any(|(h, blocks)| *h == held && blocks[block.0 as usize]),
                kept: None,
            };
            (held, write, in_place, key)
        })
        .collect();
    out.sort_by_key(|(_, w, _, _)| w.span);
    out
}

/// Whether the keys `tested` and `stored` are the same: one value, equal constants, or the same
/// expression of the same values (`id(raw)` or `name.lower()` evaluated twice, `(a, b)` built
/// twice, `obj.name` read twice). Each pair of values is compared once; past `budget` the keys
/// are not the same.
fn same_key(shape: &FnShape<'_>, budget: &Budget, tested: VarId, stored: VarId) -> bool {
    let root = |v: VarId| shape.copy_root(v);
    let found = walk::reach(
        budget,
        [(root(tested), root(stored))],
        |(a, b), next| {
            let operands = |v: VarId| shape.inst(v).map(|i| i.kind.operands()).unwrap_or_default();
            let pairs = operands(a).into_iter().zip(operands(b));
            next.extend(pairs.map(|(a, b)| (root(a), root(b))));
        },
        |(a, b)| {
            if a == b {
                return Visit::Prune;
            }
            match (shape.inst(a), shape.inst(b)) {
                (Some(x), Some(y)) if same_head(&x.kind, &y.kind) => Visit::Expand,
                _ => Visit::Stop(()),
            }
        },
    );
    found == Answer::Known(None)
}

/// Whether two instructions compute the same from operands taken in the same order: equal
/// constants, the same global, the same attribute, a call of the same arity without keywords, a
/// method of the same name, or a tuple of the same length.
fn same_head(x: &InstKind, y: &InstKind) -> bool {
    let plain =
        |kwargs: &[(Option<String>, VarId)], star: &Option<u8>| kwargs.is_empty() && star.is_none();
    match (x, y) {
        (InstKind::Const { literal: l, .. }, InstKind::Const { literal: r, .. }) => {
            l == r && !matches!(l, ConstLiteral::Other | ConstLiteral::Bytes)
        }
        (InstKind::LoadGlobal { name: l, .. }, InstKind::LoadGlobal { name: r, .. }) => l == r,
        (InstKind::AttrLoad { attr: l, .. }, InstKind::AttrLoad { attr: r, .. }) => l == r,
        (
            InstKind::Call {
                callee_var: Some(_),
                args: a1,
                kwargs: k1,
                star_from: s1,
                ..
            },
            InstKind::Call {
                callee_var: Some(_),
                args: a2,
                kwargs: k2,
                star_from: s2,
                ..
            },
        ) => a1.len() == a2.len() && plain(k1, s1) && plain(k2, s2),
        (
            InstKind::MethodCall {
                method: m1,
                args: a1,
                kwargs: k1,
                star_from: s1,
                ..
            },
            InstKind::MethodCall {
                method: m2,
                args: a2,
                kwargs: k2,
                star_from: s2,
                ..
            },
        ) => m1 == m2 && a1.len() == a2.len() && plain(k1, s1) && plain(k2, s2),
        (
            InstKind::AllocBuiltin {
                kind: BuiltinKind::Tuple,
                elems: e1,
                ..
            },
            InstKind::AllocBuiltin {
                kind: BuiltinKind::Tuple,
                elems: e2,
                ..
            },
        ) => e1.len() == e2.len(),
        _ => false,
    }
}

/// The attributes of the objects `objects` holds that `f` initializes lazily, among the
/// `written` ones, each with the blocks (by id) that only the "unset" outcome of one of its
/// `None` / falsy tests leads to ([`guards::lazy_init_branches`], [`guards::dominated`]): every
/// path from the entry to such a block runs through the branch (normal and exception edges).
/// The walks share the function's budget ([`guards::Cells`]); past it none is kept.
fn lazy_init_blocks<'f>(
    shape: &FnShape<'_>,
    objects: &FxHashMap<VarId, u32>,
    written: &FxHashMap<&'f str, FxHashSet<u32>>,
) -> Vec<((u32, &'f str), Vec<bool>)> {
    let mut cells = guards::Cells::of(shape.f);
    let mut found = Vec::new();
    let branches = guards::lazy_init_branches(shape)
        .into_iter()
        .filter_map(|b| {
            let o = *objects.get(&b.obj)?;
            let (attr, by) = written.get_key_value(b.attr.as_str())?;
            by.contains(&o).then_some(((o, *attr), b.init))
        });
    for (held, init) in branches {
        found.push((held, guards::dominated(shape, init, &mut cells)));
        if cells.spent() {
            return Vec::new();
        }
    }
    found
}

/// The blocks of `f` (whose variables have `types`) with the points that invalidate or read a
/// cached property of its own instance.
fn flow_blocks(
    shape: &FnShape<'_>,
    key: &FuncKey,
    sites: &[CallSite],
    tcx: &TypeCtx<'_>,
    types: &FxHashMap<VarId, Val>,
    kept: Option<&[KeptPoint]>,
    names: &mut Names,
) -> Vec<FlowBlock> {
    let f = shape.f;
    let self_vars = self_vars(f);
    let dicts = aliases_of(
        f,
        f.blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|i| namespace_load(&i.kind, |v| types.get(&v)))
            .filter(|(_, obj)| self_vars.contains(obj))
            .map(|(dst, _)| dst),
    );
    // A key naming a property: a string literal or a `Final` string constant.
    let key_name = |v: VarId| {
        shape
            .str_const(v)
            .map(str::to_owned)
            .or_else(|| tcx.str_constant_in(shape, v, types))
    };
    let self_calls: FxHashSet<Span> = sites
        .iter()
        .filter(|s| s.recv_is_self && &s.caller == key)
        .map(|s| s.span)
        .collect();
    let project_calls: FxHashSet<Span> = sites
        .iter()
        .filter(|s| &s.caller == key && !s.project_callees().is_empty())
        .map(|s| s.span)
        .collect();
    let mut kept_at: FxHashMap<(BlockId, u32), Vec<&KeptRemovalKind>> = FxHashMap::default();
    for p in kept.into_iter().flatten() {
        kept_at.entry((p.block, p.index)).or_default().push(&p.kind);
    }
    let mut blocks = vec![FlowBlock::default(); f.blocks.len()];
    let kept_events = |bb: BlockId, at: u32, names: &mut Names, events: &mut Vec<_>| {
        for kind in kept_at.get(&(bb, at)).into_iter().flatten() {
            let event = match kind {
                KeptRemovalKind::Named(name) | KeptRemovalKind::Replaces(name) => {
                    CacheEvent::MayRemove(names.index(name))
                }
                KeptRemovalKind::Any | KeptRemovalKind::Suspends => CacheEvent::MayRemoveAny,
                KeptRemovalKind::Runs(span) => CacheEvent::Calls(*span),
            };
            events.push((at, event));
        }
    };
    for bb in &f.blocks {
        let mut events = Vec::new();
        for (index, inst) in bb.insts.iter().enumerate() {
            let at = u32::try_from(index).unwrap_or(u32::MAX);
            kept_events(bb.id, at, names, &mut events);
            let self_call = matches!(
                inst.kind,
                InstKind::MethodCall { .. } | InstKind::Call { .. }
            ) && self_calls.contains(&inst.span);
            if kept.is_some() && !self_call && project_calls.contains(&inst.span) {
                events.push((at, CacheEvent::Calls(inst.span)));
            }
            let event = match &inst.kind {
                InstKind::AttrDelete { obj, attr } if self_vars.contains(obj) => {
                    Some(CacheEvent::DeleteAttr(names.index(attr)))
                }
                InstKind::AttrLoad { obj, attr, .. }
                    if self_vars.contains(obj) && attr != "__dict__" =>
                {
                    Some(CacheEvent::LoadAttr(names.index(attr)))
                }
                InstKind::SubscriptDelete { obj, index, .. } if dicts.contains(obj) => {
                    key_name(*index).map(|k| CacheEvent::RemoveEntry(names.index(&k)))
                }
                InstKind::MethodCall {
                    recv, method, args, ..
                } if dicts.contains(recv) => match method.as_str() {
                    "clear" => Some(CacheEvent::ClearDict),
                    "pop" | "__delitem__" => args
                        .first()
                        .and_then(|a| key_name(*a))
                        .map(|k| CacheEvent::RemoveEntry(names.index(&k))),
                    _ => None,
                },
                InstKind::MethodCall { .. } | InstKind::Call { .. }
                    if self_calls.contains(&inst.span) =>
                {
                    Some(CacheEvent::SelfCall(inst.span))
                }
                _ => None,
            };
            if let Some(event) = event {
                events.push((u32::try_from(index).unwrap_or(u32::MAX), event));
            }
        }
        // The block's end (a branch's truth test, a `raise`) is a point past its instructions.
        let end = u32::try_from(bb.insts.len()).unwrap_or(u32::MAX);
        kept_events(bb.id, end, names, &mut events);
        // The index keeps the flow for the whole run: no room for more events.
        events.shrink_to_fit();
        blocks[bb.id.0 as usize] = FlowBlock {
            succs: bb.terminator.successors(),
            returns: matches!(bb.terminator, Terminator::Return(_)),
            exc_target: bb.exc_target,
            events,
        };
    }
    blocks
}
