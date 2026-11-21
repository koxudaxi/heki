//! Size and lifetime facts both lanes share, decided from the code (Profitability and Advisory
//! noise only, NEVER a Soundness premise): how many items a container can hold for its whole
//! life, how many times a loop runs, what a loop's own test caps, and whether a value outlives
//! one iteration of a loop.
//!
//! - A container is *small* when every object a variable may hold comes from a display (list,
//!   tuple, set, or dict literal without `*` / `**` unpacking), a string constant, a builtin
//!   constructor (`list`, `tuple`, `set`, `frozenset`, `dict`, `sorted`) of an iterable the code
//!   fixes, a slice with constant bounds (`xs[:3]`), a comprehension, or a module constant bound
//!   to one ([`ModuleConstants::displays`]), holding at most [`SMALL_MAX`] items, and nothing can
//!   grow it past that: no alias of it is grown in place, stored, captured, returned, or passed
//!   to code other than read-only builtins (a `*x` argument passes its items), except by adding
//!   items to a new object a fixed number of times (`out.append(x)` in a loop over a small
//!   display, as a comprehension does). Shrinking it (`pop`, `remove`, `del x[i]`, `x[1:]`)
//!   keeps it small; tuples and strings are fixed whatever their uses.
//! - A loop's trip count is fixed by the code when its iterable is small, `range(...)` of
//!   integer constants (module constants included), or `enumerate` / `zip` / `reversed` /
//!   `sorted` / `list` / `map` / `.items()` / ... of such iterables ([`FnIndex::iter_bound`]),
//!   or when a test every path to the loop passes caps the iterable's length
//!   (`if len(cards) > 6: raise ...`, [`FnIndex::guarded_len`]).
//! - `while len(acc) < N` (also one `and` operand) with a constant `N` of at most
//!   [`LEN_CAP_MAX`] caps `acc` ([`crate::NaturalLoop::len_caps`]).

use crate::{
    BinOpKind, BlockId, BuiltinKind, CompareOpKind, ConstLiteral, DictEntry, FunctionId, Inst,
    InstKind, IrFunction, LenCap, LoopBound, NaturalLoop, Terminator, UnaryOpKind, VarId,
};
use heki_ast::Span;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

/// The most items a container display, or a loop's fixed trip count, may have to count as
/// small: enough for the tables code spells out by hand (the months, the HTTP methods, a list
/// of operation names), so that a loop over one is not data-sized and a scan of one costs a
/// fixed factor.
pub const SMALL_MAX: u32 = 16;

/// The largest constant `N` of a `while len(acc) < N` test that caps `acc` (one page: the
/// accumulator never re-copies more than that).
pub const LEN_CAP_MAX: u32 = 4096;

/// Builtins that only read a container passed to them (iterate it, measure it, copy it): an
/// argument of theirs is not grown.
const READ_ONLY_BUILTINS: &[&str] = &[
    "all",
    "any",
    "ascii",
    "bool",
    "dict",
    "enumerate",
    "filter",
    "format",
    "frozenset",
    "hash",
    "id",
    "isinstance",
    "iter",
    "len",
    "list",
    "map",
    "max",
    "min",
    "print",
    "range",
    "repr",
    "reversed",
    "set",
    "sorted",
    "str",
    "sum",
    "tuple",
    "type",
    "zip",
];

/// Methods of the builtin containers that never add an item to their receiver (they read it,
/// shrink it, reorder it, or return a new object).
const NON_GROWING_METHODS: &[&str] = &[
    "__contains__",
    "__getitem__",
    "__iter__",
    "__len__",
    "clear",
    "copy",
    "count",
    "difference",
    "difference_update",
    "discard",
    "get",
    "index",
    "intersection",
    "intersection_update",
    "isdisjoint",
    "issubset",
    "issuperset",
    "items",
    "keys",
    "pop",
    "popitem",
    "remove",
    "reverse",
    "sort",
    "symmetric_difference",
    "union",
    "values",
];

/// Builtins whose result yields at most the items of their one iterable argument.
const SAME_ITEMS: &[&str] = &[
    "reversed",
    "iter",
    "list",
    "tuple",
    "set",
    "frozenset",
    "dict",
];

/// A display (or several, merged by control flow) that nothing grows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmallDisplay {
    /// The builtin kinds of the displays (sorted).
    pub kinds: Vec<BuiltinKind>,
    /// The most items any of them holds.
    pub len: u32,
}

impl SmallDisplay {
    /// Every display is of one of `kinds`.
    pub fn is_of(&self, kinds: &[BuiltinKind]) -> bool {
        self.kinds.iter().all(|k| kinds.contains(k))
    }

    /// Every display is a tuple or a string: no use of one can change its size.
    pub fn is_immutable(&self) -> bool {
        !self.kinds.is_empty() && self.kinds.iter().all(|k| is_immutable_kind(*k))
    }
}

fn is_immutable_kind(kind: BuiltinKind) -> bool {
    matches!(kind, BuiltinKind::Tuple | BuiltinKind::Str)
}

/// Module-level names the code fixes: bound once, by the module body, outside loops, and
/// never rebound (no other store, `del`, `global` assignment, later star import of a public
/// name, or dynamic globals).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleConstants {
    /// Integer constants (`ATTEMPTS = 5`, `LIMIT = BASE * 2`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ints: BTreeMap<String, i64>,
    /// Small displays that no function of the module grows ([`FnIndex::container_bound`]'s
    /// conditions, over every load of the name; tuples and strings whatever their uses).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub displays: BTreeMap<String, SmallDisplay>,
    /// What a comprehension lowered as a function of its own returns (before 3.12, a
    /// `__comp_<id>` call), by that function's id, when the code fixes its size.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub comprehensions: BTreeMap<u32, SmallDisplay>,
    /// Names of the builtins the facts rely on (`range`, `len`, ...) that the module rebinds.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub shadowed: BTreeSet<String>,
    /// Module globals may be bound behind the IR (`exec` or `globals()` at module level): no
    /// name is fixed and no builtin is trusted. (A `from m import *` only rebinds the public
    /// names bound before it.)
    #[serde(default)]
    pub dynamic: bool,
}

impl ModuleConstants {
    fn trusts_builtin(&self, name: &str) -> bool {
        !self.dynamic && !self.shadowed.contains(name)
    }
}

/// Syntax-level loop facts the builder records for [`refine_loops`].
#[derive(Debug, Clone, Default)]
pub(crate) struct LoopHint {
    /// A `for` loop whose body is a retry body (a `try` that leaves the loop on success or
    /// sleeps in a handler): a fixed trip count makes it a retry loop.
    pub retry_body: bool,
    /// Spans of the `len(x) < N`-shaped comparisons a `while` test requires (the test itself
    /// or its `and` operands).
    pub cap_tests: Vec<Span>,
}

/// Where a variable is defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Def {
    None,
    Param,
    Inst(BlockId, u32),
    Phi(BlockId, u32),
}

/// The definitions of one function's variables and the facts above, computed on demand.
pub struct FnIndex<'a> {
    pub f: &'a IrFunction,
    module: &'a ModuleConstants,
    defs: Vec<Def>,
    /// [`Self::container_bound`] of each value asked so far. A value whose answer is being
    /// decided reads as not fixed: a size that depends on itself is not fixed by the code.
    bounds: RefCell<FxHashMap<VarId, Option<u32>>>,
    /// [`Self::trip_count`] of each loop (by its index in `f.loops`) asked so far, in the same
    /// way.
    trips: RefCell<FxHashMap<usize, Option<u32>>>,
    /// The predecessors of each block of `f` (normal and exception edges).
    preds: OnceCell<Vec<Vec<BlockId>>>,
    /// The values `f` passes to `len()` (after copies): only their lengths can be tested.
    measured: OnceCell<FxHashSet<VarId>>,
    /// The length caps the branches of `f` establish ([`Self::guarded_len`]).
    guards: OnceCell<Vec<LenGuard>>,
    /// The fixed trip counts `f.loops` record were decided with these constants (see
    /// [`Self::redeciding_trips`]).
    recorded_trips: bool,
}

/// The objects a variable may hold ([`FnIndex::origins`]).
#[derive(Default, Clone)]
struct Origins {
    len: u32,
    kinds: Vec<BuiltinKind>,
    /// The variables whose definition creates (or loads) one of the objects.
    roots: Vec<VarId>,
    /// The variables whose objects are collected (phis may share inputs: each is resolved
    /// once).
    resolved: FxHashSet<VarId>,
    /// Some object is a slice of a container of unknown kind (`xs[:3]`): its size is known,
    /// its kind is not.
    opaque: bool,
    /// Some object is not one this function makes for itself (a module constant's, which
    /// every call shares, or one it is handed): adding to it adds up beyond what this function
    /// counts.
    shared: bool,
}

impl Origins {
    fn add(&mut self, root: VarId, kind: BuiltinKind, len: usize) {
        self.len = self.len.max(u32::try_from(len).unwrap_or(u32::MAX));
        if !self.kinds.contains(&kind) {
            self.kinds.push(kind);
            self.kinds.sort_by_key(|k| *k as u8);
        }
        self.roots.push(root);
    }

    fn add_display(&mut self, root: VarId, d: &SmallDisplay) {
        for k in &d.kinds {
            self.add(root, *k, d.len as usize);
        }
        self.roots.dedup();
    }

    /// Tuples and strings only: nothing can change their size.
    fn immutable(&self) -> bool {
        !self.opaque && !self.kinds.is_empty() && self.kinds.iter().all(|k| is_immutable_kind(*k))
    }
}

/// `len(var) <= bound` on every path through the edge into `edge_to` (its one predecessor
/// branches on a test implying it).
#[derive(Debug, Clone, Copy)]
struct LenGuard {
    edge_to: BlockId,
    var: VarId,
    bound: u32,
}

/// The caps of [`FnIndex::implied_caps`] decided so far: per value and outcome (one being
/// decided implies nothing), and per block for the tests on the single-predecessor way into it.
struct CapWalk<'p> {
    preds: &'p [Vec<BlockId>],
    values: FxHashMap<(VarId, bool), Vec<(VarId, u32)>>,
    blocks: FxHashMap<BlockId, Vec<(VarId, u32)>>,
}

/// Where a container's object may go without it counting as grown ([`FnIndex::growth`]):
/// the store that binds it (a module global or an attribute), or the function's return
/// (the result of a comprehension function).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sink {
    Store(BlockId, usize),
    Return,
}

/// The walk of [`FnIndex::lives_across`]: the phis it reached, and the answer of each
/// instruction.
#[derive(Default)]
struct Lifetimes {
    seen: FxHashSet<VarId>,
    answered: FxHashMap<VarId, (bool, bool)>,
}

/// Methods that add at most one item to the builtin container they are called on.
const ONE_ITEM_METHODS: &[(&str, usize)] = &[
    ("append", 1),
    ("add", 1),
    ("appendleft", 1),
    ("insert", 2),
    ("setdefault", 1),
    ("setdefault", 2),
];

/// Methods that add the items of their one argument.
const ITEMS_METHODS: &[&str] = &["extend", "update"];

/// How deep [`FnIndex::container_bound`] and [`FnIndex::iter_bound`] may ask each other
/// (`list(sorted(set(...)))` and the like); past it a size is not fixed.
const MAX_NESTED_SIZES: u32 = 16;

impl<'a> FnIndex<'a> {
    pub fn new(f: &'a IrFunction, module: &'a ModuleConstants) -> Self {
        let mut defs = vec![Def::None; f.var_names.len()];
        let mut set = |v: VarId, d: Def| {
            if let Some(slot) = defs.get_mut(v.0 as usize) {
                *slot = d;
            }
        };
        for p in &f.params {
            set(p.var, Def::Param);
        }
        for bb in &f.blocks {
            for (i, phi) in bb.phis.iter().enumerate() {
                set(phi.dst, Def::Phi(bb.id, i as u32));
            }
            for (i, inst) in bb.insts.iter().enumerate() {
                if let Some(d) = inst.kind.dst() {
                    set(d, Def::Inst(bb.id, i as u32));
                }
            }
        }
        Self {
            f,
            module,
            defs,
            bounds: RefCell::default(),
            trips: RefCell::default(),
            preds: OnceCell::new(),
            measured: OnceCell::new(),
            guards: OnceCell::new(),
            recorded_trips: true,
        }
    }

    /// This index with the fixed trip counts `f.loops` record decided again from its
    /// constants, which lack some display the recorded counts may rest on.
    pub fn redeciding_trips(mut self) -> Self {
        self.recorded_trips = false;
        self
    }

    fn def(&self, v: VarId) -> Def {
        self.defs.get(v.0 as usize).copied().unwrap_or(Def::None)
    }

    /// The instruction defining `v`, if any.
    pub fn def_inst(&self, v: VarId) -> Option<&'a Inst> {
        match self.def(v) {
            Def::Inst(b, i) => Some(&self.f.block(b).insts[i as usize]),
            _ => None,
        }
    }

    fn phi_incoming(&self, b: BlockId, i: u32) -> &'a [(BlockId, VarId)] {
        &self.f.block(b).phis[i as usize].incoming
    }

    /// `v` with the copies leading to it undone (SSA copies form chains ending at their
    /// source).
    fn copied_from(&self, mut v: VarId) -> VarId {
        for _ in 0..self.defs.len() {
            match self.def_inst(v).map(|i| &i.kind) {
                Some(InstKind::Copy { src, .. }) => v = *src,
                _ => break,
            }
        }
        v
    }

    /// The builtin a call names, when it is one of the read-only builtins these facts know and
    /// the real one: a global load of the callee's name that the module never rebinds.
    pub fn builtin(&self, callee_var: Option<VarId>, callee: &str) -> Option<&'a str> {
        match &self.def_inst(callee_var?)?.kind {
            InstKind::LoadGlobal {
                name,
                is_lazy_import: false,
                ..
            } if name == callee
                && READ_ONLY_BUILTINS.contains(&name.as_str())
                && self.module.trusts_builtin(name) =>
            {
                Some(name.as_str())
            }
            _ => None,
        }
    }

    // ------------------------------------------------------------------------- containers

    /// An upper bound on the items `v` holds for its whole life, when it is small (see the
    /// module documentation): `None` when `v` may hold a container the code does not fix or
    /// that something may grow.
    pub fn container_bound(&self, v: VarId) -> Option<u32> {
        self.container_bound_nested(v, 0)
    }

    fn container_bound_nested(&self, v: VarId, depth: u32) -> Option<u32> {
        if let Some(known) = self.bounds.borrow().get(&v) {
            return *known;
        }
        self.bounds.borrow_mut().insert(v, None);
        let bound = self.decide_bound(v, depth);
        self.bounds.borrow_mut().insert(v, bound);
        bound
    }

    fn decide_bound(&self, v: VarId, depth: u32) -> Option<u32> {
        let mut origins = Origins::default();
        if !self.origins(v, &mut origins, &mut Vec::new(), depth)
            || origins.roots.is_empty()
            || origins.len > SMALL_MAX
        {
            return None;
        }
        let added = if origins.immutable() {
            0
        } else {
            self.growth(&origins, None, None)?
        };
        origins.len.checked_add(added).filter(|n| *n <= SMALL_MAX)
    }

    /// The display `value` holds when it is stored by the instruction at `sink` (a module
    /// global or an attribute), small and grown by nothing in this function besides that store
    /// and the items the function adds to it a fixed number of times.
    pub fn stored_display(&self, value: VarId, sink: (BlockId, usize)) -> Option<SmallDisplay> {
        self.display_at(value, Sink::Store(sink.0, sink.1))
    }

    /// What a comprehension function returns, when it is small: its accumulator, which its one
    /// `return` gives.
    fn returned_display(&self) -> Option<SmallDisplay> {
        let returned = self.f.blocks.iter().find_map(|bb| match bb.terminator {
            Terminator::Return(value) => value,
            _ => None,
        })?;
        self.display_at(returned, Sink::Return)
    }

    fn display_at(&self, value: VarId, sink: Sink) -> Option<SmallDisplay> {
        let mut origins = Origins::default();
        let loads = |o: &Origins| {
            o.roots.iter().any(|r| {
                matches!(
                    self.def_inst(*r).map(|i| &i.kind),
                    Some(InstKind::LoadGlobal { .. })
                )
            })
        };
        if !self.origins(value, &mut origins, &mut Vec::new(), 0)
            || origins.roots.is_empty()
            || origins.opaque
            || origins.len > SMALL_MAX
            || loads(&origins)
        {
            return None;
        }
        let added = if origins.immutable() {
            0
        } else {
            self.growth(&origins, Some(sink), None)?
        };
        let len = origins.len.checked_add(added).filter(|n| *n <= SMALL_MAX)?;
        Some(SmallDisplay {
            kinds: origins.kinds,
            len,
        })
    }

    /// Nothing in this function grows the container a load (of a global or an attribute)
    /// produces, nor lets other code grow it.
    pub fn load_unchanged(&self, load: VarId) -> bool {
        !self.grows(&[load], &[], None)
    }

    /// Collect the objects `v` may hold into `out`: `false` when one of them is not a small
    /// display, a slice of one, or a small module constant. `stack`: the variables being
    /// resolved (a cycle through phis adds nothing). `depth`: how deep the size questions this
    /// one is part of already are.
    fn origins(&self, v: VarId, out: &mut Origins, stack: &mut Vec<VarId>, depth: u32) -> bool {
        if stack.contains(&v) || out.resolved.contains(&v) {
            return true;
        }
        if stack.len() > 32 {
            return false;
        }
        stack.push(v);
        let ok = match self.def(v) {
            Def::Phi(b, i) => self
                .phi_incoming(b, i)
                .iter()
                .all(|&(_, x)| self.origins(x, out, stack, depth)),
            Def::Inst(b, i) => {
                self.inst_origins(v, &self.f.block(b).insts[i as usize], out, stack, depth)
            }
            Def::Param | Def::None => false,
        };
        stack.pop();
        // A variable that fails fails the whole walk: only resolved ones are kept.
        if ok {
            out.resolved.insert(v);
        }
        ok
    }

    fn inst_origins(
        &self,
        v: VarId,
        inst: &Inst,
        out: &mut Origins,
        stack: &mut Vec<VarId>,
        depth: u32,
    ) -> bool {
        match &inst.kind {
            InstKind::Copy { src, .. } => self.origins(*src, out, stack, depth),
            InstKind::AllocBuiltin {
                dst,
                kind:
                    kind @ (BuiltinKind::List
                    | BuiltinKind::Tuple
                    | BuiltinKind::Set
                    | BuiltinKind::Dict),
                elems,
            } if !self.f.starred.iter().any(|(d, _)| d == dst) => {
                out.add(v, *kind, elems.len());
                true
            }
            InstKind::DictLiteral { entries, .. }
                if entries
                    .iter()
                    .all(|e| matches!(e, DictEntry::KeyValue { .. })) =>
            {
                out.add(v, BuiltinKind::Dict, entries.len());
                true
            }
            InstKind::Const {
                literal: ConstLiteral::Str(s),
                ..
            } => {
                out.add(v, BuiltinKind::Str, s.chars().count());
                true
            }
            InstKind::Call {
                dst,
                callee_var,
                callee,
                args,
                kwargs,
                ..
            } => {
                if let Some(d) = comprehension_id(*callee_var, callee)
                    .and_then(|id| self.module.comprehensions.get(&id))
                {
                    out.add_display(v, d);
                    return true;
                }
                match self.constructed(*dst, *callee_var, callee, args, kwargs, depth) {
                    Some((kind, len)) => {
                        out.add(v, kind, len as usize);
                        true
                    }
                    None => false,
                }
            }
            // A slice is a new, smaller container of the same kind; with constant bounds it
            // holds at most as many items as they span, whatever it slices and however that
            // grows.
            InstKind::SubscriptLoad {
                obj,
                index,
                is_slice: true,
                ..
            } => match self.slice_len(*index) {
                Some(n) => {
                    match self.made_kind(*obj) {
                        Some(kind) => out.add(v, kind, n as usize),
                        None => {
                            out.opaque = true;
                            out.len = out.len.max(n);
                            out.roots.push(v);
                        }
                    }
                    true
                }
                None => {
                    out.roots.push(v);
                    self.origins(*obj, out, stack, depth)
                }
            },
            InstKind::LoadGlobal {
                name,
                is_lazy_import: false,
                ..
            } if !self.module.dynamic => match self.module.displays.get(name) {
                Some(d) => {
                    out.add_display(v, d);
                    out.shared = true;
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    /// The builtin kind of the container the instruction defining `v` (after copies) makes: a
    /// list, tuple, or set display, a string constant, or a builtin constructor call (what a
    /// slice can slice).
    fn made_kind(&self, v: VarId) -> Option<BuiltinKind> {
        match &self.def_inst(self.copied_from(v))?.kind {
            InstKind::AllocBuiltin { kind, .. } => Some(*kind),
            InstKind::Const {
                literal: ConstLiteral::Str(_),
                ..
            } => Some(BuiltinKind::Str),
            InstKind::Call {
                callee_var, callee, ..
            } => match self.builtin(*callee_var, callee)? {
                "list" | "sorted" => Some(BuiltinKind::List),
                "tuple" => Some(BuiltinKind::Tuple),
                "set" | "frozenset" => Some(BuiltinKind::Set),
                "dict" => Some(BuiltinKind::Dict),
                _ => None,
            },
            _ => None,
        }
    }

    /// The kind and the most items of the new container a builtin constructor call builds:
    /// `list()`, `dict()`, or `list(x)` / `tuple(x)` / `set(x)` / `frozenset(x)` / `dict(x)` /
    /// `sorted(x)` of an iterable the code fixes.
    fn constructed(
        &self,
        dst: VarId,
        callee_var: Option<VarId>,
        callee: &str,
        args: &[VarId],
        kwargs: &[(Option<String>, VarId)],
        depth: u32,
    ) -> Option<(BuiltinKind, u32)> {
        let name = self.builtin(callee_var, callee)?;
        let kind = self.made_kind(dst)?;
        let keywords_ok = match name {
            "sorted" => kwargs
                .iter()
                .all(|(k, _)| matches!(k.as_deref(), Some("key" | "reverse"))),
            _ => kwargs.is_empty(),
        };
        if !keywords_ok || self.f.is_starred(dst, 0) {
            return None;
        }
        let [arg] = args else {
            return (args.is_empty() && name != "sorted").then_some((kind, 0));
        };
        let n = self.iter_bound_depth(*arg, &mut |_| None, depth + 1)?;
        Some((kind, n))
    }

    /// The most items a slice by the constant bounds `index` holds, whatever the length of
    /// what it slices: `[:k]`, `[a:b]`, `[-k:]`, `[-a:-b]` (a positive step divides them).
    fn slice_len(&self, index: VarId) -> Option<u32> {
        // A slice passes the tuple of its three bounds (`None` where one is left out).
        let parts: [VarId; 3] = self.def_inst(index)?.kind.operands().try_into().ok()?;
        let bound = |v: VarId| -> Result<Option<i64>, ()> {
            match &self.def_inst(self.copied_from(v)).map(|i| &i.kind) {
                Some(InstKind::Const {
                    literal: ConstLiteral::None,
                    ..
                }) => Ok(None),
                _ => self.const_int(v).map(Some).ok_or(()),
            }
        };
        let [lower, upper, step] = parts.map(bound);
        let (lower, upper, step) = (lower.ok()?, upper.ok()?, step.ok()?);
        let step = match step {
            None => 1,
            Some(s) if s > 0 => s,
            Some(_) => return None,
        };
        // A stop from the start caps the slice past its start; a start from the end caps it
        // before its stop (or the end).
        let from_start = upper
            .filter(|hi| *hi >= 0)
            .map(|hi| hi - lower.unwrap_or(0).max(0));
        let from_end = lower.filter(|lo| *lo < 0).map(|lo| {
            lo.saturating_neg()
                .saturating_add(upper.filter(|hi| *hi < 0).unwrap_or(0))
        });
        let span = from_start.into_iter().chain(from_end).min()?.max(0);
        let n = span.checked_add(step - 1)? / step;
        u32::try_from(n).ok()
    }

    /// The most items `origins` gain in this function besides their own: `None` when some
    /// alias of them may be grown without a fixed count (see [`Self::grows`]). A new object
    /// may gain items a fixed number of times: an `append` (`add`, `insert`, ...) or an
    /// item store into a dict counts once per run of its block, `extend(x)` / `update(x)` as
    /// many as `x` yields, and a block runs once per iteration of each loop around it that the
    /// object outlives ([`Self::runs`]). A shared object (a module constant's) gains none.
    /// `within`: the blocks to look at (by block id), all when `None`.
    fn growth(
        &self,
        origins: &Origins,
        sink: Option<Sink>,
        within: Option<&[bool]>,
    ) -> Option<u32> {
        // The methods of a container of unknown kind may add any number of items.
        let counted = !origins.shared && !origins.opaque;
        let alias = self.aliases(&origins.roots);
        if self.captured(&alias) {
            return None;
        }
        let kinds = &origins.kinds;
        let list_only =
            !origins.opaque && !kinds.is_empty() && kinds.iter().all(|k| *k == BuiltinKind::List);
        let dict_only =
            !origins.opaque && !kinds.is_empty() && kinds.iter().all(|k| *k == BuiltinKind::Dict);
        let mut added: u32 = 0;
        let mut ops = Vec::new();
        let blocks =
            self.f.blocks.iter().filter(|bb| {
                within.is_none_or(|w| w.get(bb.id.0 as usize).copied().unwrap_or(false))
            });
        for bb in blocks {
            for (i, inst) in bb.insts.iter().enumerate() {
                if sink == Some(Sink::Store(bb.id, i)) {
                    continue;
                }
                if counted
                    && let Some(items) = self.items_added(inst, (bb.id, i), &alias, dict_only)
                {
                    let runs = self.runs(bb.id, &origins.roots, &alias)?;
                    added = added.checked_add(items?.checked_mul(runs)?)?;
                    if added > SMALL_MAX {
                        return None;
                    }
                    continue;
                }
                ops.clear();
                self.grown_operands(inst, list_only, &mut ops);
                if ops.iter().any(|v| alias.contains(v)) {
                    return None;
                }
            }
            match &bb.terminator {
                Terminator::Return(Some(v)) if sink == Some(Sink::Return) && alias.contains(v) => {}
                Terminator::Return(Some(v)) | Terminator::Raise(Some(v)) if alias.contains(v) => {
                    return None;
                }
                _ => {}
            }
        }
        Some(added)
    }

    /// Whether some alias of `roots` (through copies and phis) may be grown: grown in place,
    /// stored, captured, returned, yielded, or passed to code other than read-only builtins.
    /// `kinds` are the containers' kinds (an element store keeps a list's size); `sink` is a
    /// store that is allowed (the global or attribute the display is bound to).
    fn grows(
        &self,
        roots: &[VarId],
        kinds: &[BuiltinKind],
        sink: Option<(BlockId, usize)>,
    ) -> bool {
        let origins = Origins {
            kinds: kinds.to_vec(),
            roots: roots.to_vec(),
            shared: true,
            ..Origins::default()
        };
        self.growth(&origins, sink.map(|(b, i)| Sink::Store(b, i)), None)
            .is_none()
    }

    /// The variables that may hold the objects of `roots`: they, and the copies and phis of
    /// them (and `min` / `max` of several, which return one).
    fn aliases(&self, roots: &[VarId]) -> FxHashSet<VarId> {
        let f = self.f;
        let mut alias: FxHashSet<VarId> = roots.iter().copied().collect();
        loop {
            let before = alias.len();
            for bb in &f.blocks {
                for phi in &bb.phis {
                    if phi.incoming.iter().any(|(_, x)| alias.contains(x)) {
                        alias.insert(phi.dst);
                    }
                }
                for inst in &bb.insts {
                    if let Some((dst, sources)) = self.alias_of_operands(inst)
                        && sources.iter().any(|x| alias.contains(x))
                    {
                        alias.insert(dst);
                    }
                }
            }
            if alias.len() == before {
                return alias;
            }
        }
    }

    /// Some of `alias` is a name a closure captures: anything may happen to it.
    fn captured(&self, alias: &FxHashSet<VarId>) -> bool {
        alias.iter().any(|v| {
            self.f
                .var_name(*v)
                .is_some_and(|n| self.f.captured_vars.iter().any(|c| c == n))
        })
    }

    /// The items `inst` (at `at`) adds to a container among `alias` that it is called on
    /// (`Some(None)`: a number the code does not fix), when it adds items and lets no alias go
    /// anywhere else; `None` when `inst` is no such addition (`dict_only`: the containers are
    /// dicts, so an item store adds at most one key).
    fn items_added(
        &self,
        inst: &Inst,
        at: (BlockId, usize),
        alias: &FxHashSet<VarId>,
        dict_only: bool,
    ) -> Option<Option<u32>> {
        match &inst.kind {
            InstKind::MethodCall {
                dst,
                recv,
                method,
                args,
                kwargs,
                ..
            } if alias.contains(recv)
                && kwargs.is_empty()
                && !args.iter().any(|a| alias.contains(a))
                && !(0..args.len()).any(|i| self.f.is_starred(*dst, i)) =>
            {
                if ONE_ITEM_METHODS.contains(&(method.as_str(), args.len())) {
                    Some(Some(1))
                } else if ITEMS_METHODS.contains(&method.as_str()) && args.len() == 1 {
                    // The argument is only read here (`extend([a, b])`, `update({"k": v})`).
                    Some(self.iter_bound(args[0], &mut |_| None).or_else(|| {
                        self.display_at(args[0], Sink::Store(at.0, at.1))
                            .map(|d| d.len)
                    }))
                } else {
                    None
                }
            }
            InstKind::SubscriptStore {
                obj, index, value, ..
            } if dict_only
                && alias.contains(obj)
                && !alias.contains(index)
                && !alias.contains(value) =>
            {
                Some(Some(1))
            }
            _ => None,
        }
    }

    /// How many times the block `b` runs for one object of `roots`: the product of the trip
    /// counts of the loops around `b` that some root is made outside of, or that carry an
    /// object of `alias` from one iteration to the next (a phi of the loop's header holds
    /// it). `None` when one of them is not fixed.
    fn runs(&self, b: BlockId, roots: &[VarId], alias: &FxHashSet<VarId>) -> Option<u32> {
        let made_in = |lp: &NaturalLoop| {
            roots
                .iter()
                .all(|r| matches!(self.def(*r), Def::Inst(rb, _) | Def::Phi(rb, _) if lp.body.contains(&rb)))
        };
        let carried = |lp: &NaturalLoop| {
            self.f
                .block(lp.header)
                .phis
                .iter()
                .any(|p| alias.contains(&p.dst))
        };
        let mut n: u32 = 1;
        for (li, lp) in self.f.loops.iter().enumerate() {
            if lp.body.contains(&b) && (!made_in(lp) || carried(lp)) {
                n = n.checked_mul(self.trip_count(li)?)?;
            }
        }
        Some(n)
    }

    /// The result of `inst` when it may be one of the objects of some of its operands: a
    /// copy, or `min` / `max` of several arguments, which returns one of them.
    fn alias_of_operands<'i>(&self, inst: &'i Inst) -> Option<(VarId, &'i [VarId])> {
        match &inst.kind {
            InstKind::Copy { dst, src } => Some((*dst, std::slice::from_ref(src))),
            InstKind::Call {
                dst,
                callee_var,
                callee,
                args,
                ..
            } if matches!(self.builtin(*callee_var, callee), Some("min" | "max")) => {
                Some((*dst, args))
            }
            _ => None,
        }
    }

    /// Push the operands of `inst` that it may grow or hand to code that may grow them
    /// (`list_only`: the containers are lists, so an element store by a constant index keeps
    /// their size).
    fn grown_operands(&self, inst: &Inst, list_only: bool, out: &mut Vec<VarId>) {
        let starred = |dst: VarId, i: usize| self.f.is_starred(dst, i);
        match &inst.kind {
            InstKind::Copy { .. }
            | InstKind::MakeIter { .. }
            | InstKind::IterNext { .. }
            | InstKind::Compare { .. }
            | InstKind::UnaryOp { .. }
            | InstKind::SubscriptLoad { .. }
            | InstKind::SubscriptDelete { .. }
            | InstKind::AttrDelete { .. }
            | InstKind::Delete { .. }
            | InstKind::YieldFrom { .. }
            | InstKind::Const { .. }
            | InstKind::LoadGlobal { .. }
            | InstKind::LoadDeref { .. }
            | InstKind::LoadName { .. }
            | InstKind::CatchException { .. }
            | InstKind::Import { .. } => {}
            InstKind::AttrStore { value, .. } => out.push(*value),
            InstKind::SubscriptStore {
                obj, index, value, ..
            } => {
                out.push(*value);
                let item_store = list_only && self.is_int_constant(*index);
                if !item_store {
                    out.push(*obj);
                }
            }
            InstKind::BinOp {
                lhs, rhs, in_place, ..
            } => {
                if *in_place {
                    out.extend([*lhs, *rhs]);
                }
            }
            InstKind::MethodCall {
                dst,
                recv,
                method,
                args,
                kwargs,
                ..
            } => {
                if !NON_GROWING_METHODS.contains(&method.as_str()) {
                    out.push(*recv);
                }
                let str_recv = matches!(
                    self.def_inst(*recv).map(|i| &i.kind),
                    Some(InstKind::Const {
                        literal: ConstLiteral::Str(_) | ConstLiteral::Bytes,
                        ..
                    })
                );
                if !str_recv {
                    out.extend(
                        args.iter()
                            .enumerate()
                            .filter(|(i, _)| !starred(*dst, *i))
                            .map(|(_, a)| *a),
                    );
                }
                out.extend(kwargs.iter().map(|(_, v)| *v));
            }
            InstKind::Call {
                dst,
                callee_var,
                callee,
                args,
                kwargs,
                ..
            } => {
                let reads_only = self
                    .builtin(*callee_var, callee)
                    .is_some_and(|n| READ_ONLY_BUILTINS.contains(&n));
                // A comprehension function only iterates its first argument (the outermost
                // iterable); the others are the names its body reads.
                let iterated = usize::from(comprehension_id(*callee_var, callee).is_some());
                if !reads_only {
                    out.extend(
                        args.iter()
                            .enumerate()
                            .skip(iterated)
                            .filter(|(i, _)| !starred(*dst, *i))
                            .map(|(_, a)| *a),
                    );
                }
                out.extend(kwargs.iter().map(|(_, v)| *v));
            }
            InstKind::AllocBuiltin { dst, elems, .. } => out.extend(
                elems
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !starred(*dst, *i))
                    .map(|(_, e)| *e),
            ),
            InstKind::DictLiteral { entries, .. } => {
                for e in entries {
                    if let DictEntry::KeyValue { key, value } = e {
                        out.extend([*key, *value]);
                    }
                }
            }
            InstKind::MakeGenExpr { captured, .. } | InstKind::MakeClosure { captured, .. } => {
                out.extend(captured);
            }
            // Binding a module constant: every load of it was checked (`module_constants`). An
            // import binding its name is another name of the imported global, not a new place
            // it escapes to.
            InstKind::StoreGlobal { name, value } => {
                let import_binding = matches!(
                    self.def_inst(*value).map(|i| &i.kind),
                    Some(InstKind::Import { .. })
                ) && self.f.var_name(*value) == Some(name.as_str());
                if !self.module.displays.contains_key(name) && !import_binding {
                    out.push(*value);
                }
            }
            // A bound method (`f = xs.append`), a closure cell, a sent or awaited value, a
            // context manager: anything may happen to it.
            InstKind::AttrLoad { obj: v, .. }
            | InstKind::StoreDeref { value: v, .. }
            | InstKind::Await { value: v, .. }
            | InstKind::EnterWith { mgr: v, .. }
            | InstKind::ExitWith { mgr: v, .. } => out.push(*v),
            InstKind::Yield { value, .. } => out.extend(*value),
        }
    }

    /// `v` is an integer literal (an item store `xs[0] = v` keeps a list's size).
    fn is_int_constant(&self, v: VarId) -> bool {
        let kind = self.def_inst(v).map(|i| &i.kind);
        matches!(
            kind,
            Some(InstKind::Const {
                literal: ConstLiteral::Int(_),
                ..
            })
        )
    }

    /// The variables whose object something in this function may grow, directly or through an
    /// alias (copies and phis), for loads of several globals or attributes at once.
    pub fn grown_vars(&self) -> FxHashSet<VarId> {
        let f = self.f;
        let mut grown: FxHashSet<VarId> = FxHashSet::default();
        let mut ops = Vec::new();
        for bb in &f.blocks {
            for inst in &bb.insts {
                self.grown_operands(inst, false, &mut ops);
            }
            if let Terminator::Return(Some(v)) | Terminator::Raise(Some(v)) = &bb.terminator {
                ops.push(*v);
            }
        }
        grown.extend(ops);
        for (i, name) in f.var_names.iter().enumerate() {
            if name
                .as_deref()
                .is_some_and(|n| f.captured_vars.iter().any(|c| c == n))
            {
                grown.insert(VarId(i as u32));
            }
        }
        // An alias's growth is its source's.
        loop {
            let before = grown.len();
            for bb in &f.blocks {
                for phi in &bb.phis {
                    if grown.contains(&phi.dst) {
                        grown.extend(phi.incoming.iter().map(|(_, v)| *v));
                    }
                }
                for inst in &bb.insts {
                    if let Some((dst, sources)) = self.alias_of_operands(inst)
                        && grown.contains(&dst)
                    {
                        grown.extend(sources);
                    }
                }
            }
            if grown.len() == before {
                return grown;
            }
        }
    }

    // ------------------------------------------------------------------------------ loops

    /// An upper bound on the items iterating `v` yields, when the code fixes it: a small
    /// container ([`Self::container_bound`], short strings and constant slices included),
    /// `range(...)` of integer constants, or a builtin wrapper of one (`enumerate`, `zip` of at least one, `reversed`,
    /// `sorted`, `list`, `tuple`, `set`, `frozenset`, `dict`, `iter`, `map` / `filter` of one
    /// iterable, `.items()` / `.keys()` / `.values()`). `leaf` decides a load of a global or an
    /// attribute this module's constants do not cover (the Advisory lane knows other modules).
    pub fn iter_bound(&self, v: VarId, leaf: &mut dyn FnMut(&Inst) -> Option<u32>) -> Option<u32> {
        self.iter_bound_depth(v, leaf, 0)
    }

    fn iter_bound_depth(
        &self,
        v: VarId,
        leaf: &mut dyn FnMut(&Inst) -> Option<u32>,
        depth: u32,
    ) -> Option<u32> {
        if depth > MAX_NESTED_SIZES {
            return None;
        }
        if let Some(n) = self.container_bound_nested(v, depth) {
            return Some(n);
        }
        let inst = self.def_inst(v)?;
        let small = |n: usize| u32::try_from(n).ok().filter(|n| *n <= SMALL_MAX);
        match &inst.kind {
            InstKind::Copy { src, .. } => self.iter_bound_depth(*src, leaf, depth + 1),
            InstKind::Const {
                literal: ConstLiteral::Str(s),
                ..
            } => small(s.chars().count()),
            InstKind::Call {
                dst,
                callee_var,
                callee,
                args,
                kwargs,
                ..
            } => {
                let name = self.builtin(*callee_var, callee)?;
                let plain = |i: usize| args.get(i).copied().filter(|_| !self.f.is_starred(*dst, i));
                let keywords = |allowed: &[&str]| {
                    kwargs
                        .iter()
                        .all(|(k, _)| k.as_deref().is_some_and(|k| allowed.contains(&k)))
                };
                match name {
                    "range" if kwargs.is_empty() && (1..=3).contains(&args.len()) => {
                        let ints: Option<Vec<i64>> = (0..args.len())
                            .map(|i| plain(i).and_then(|a| self.const_int(a)))
                            .collect();
                        range_len(&ints?).and_then(|n| small(n as usize))
                    }
                    "enumerate" if args.len() <= 2 && keywords(&["start"]) => {
                        self.iter_bound_depth(plain(0)?, leaf, depth + 1)
                    }
                    "sorted" if args.len() == 1 && keywords(&["key", "reverse"]) => {
                        self.not_grown(*dst, "sorted")?;
                        self.iter_bound_depth(plain(0)?, leaf, depth + 1)
                    }
                    n if SAME_ITEMS.contains(&n) && args.len() == 1 && kwargs.is_empty() => {
                        self.not_grown(*dst, n)?;
                        self.iter_bound_depth(plain(0)?, leaf, depth + 1)
                    }
                    "map" | "filter" if args.len() == 2 && kwargs.is_empty() => {
                        self.iter_bound_depth(plain(1)?, leaf, depth + 1)
                    }
                    "zip" if keywords(&["strict"]) => (0..args.len())
                        .filter_map(plain)
                        .filter_map(|a| self.iter_bound_depth(a, leaf, depth + 1))
                        .min(),
                    _ => None,
                }
            }
            InstKind::MethodCall {
                recv,
                method,
                args,
                kwargs,
                ..
            } if args.is_empty()
                && kwargs.is_empty()
                && matches!(method.as_str(), "items" | "keys" | "values") =>
            {
                self.iter_bound_depth(*recv, leaf, depth + 1)
            }
            // A module body reads an imported name as a copy of its `Import`.
            InstKind::LoadGlobal { .. }
            | InstKind::LoadName { .. }
            | InstKind::AttrLoad { .. }
            | InstKind::Import { .. } => leaf(inst).filter(|n| *n <= SMALL_MAX),
            _ => None,
        }
    }

    /// `Some` unless `built`, the result of the builtin `constructor`, is a new mutable
    /// container that something may grow (`xs = list(src)` then `xs.extend(more)`): its
    /// items are then no longer its input's.
    fn not_grown(&self, built: VarId, constructor: &str) -> Option<()> {
        let kind = match constructor {
            "list" | "sorted" => BuiltinKind::List,
            "set" => BuiltinKind::Set,
            "dict" => BuiltinKind::Dict,
            // Immutable, or an iterator over its input.
            _ => return Some(()),
        };
        (!self.grows(&[built], &[kind], None)).then_some(())
    }

    /// The value of an integer expression the code fixes: a literal, a module constant, or
    /// `+`, `-`, `*` and negation of them.
    pub fn const_int(&self, v: VarId) -> Option<i64> {
        self.const_int_depth(v, 0)
    }

    fn const_int_depth(&self, v: VarId, depth: u32) -> Option<i64> {
        if depth > 16 {
            return None;
        }
        match &self.def_inst(v)?.kind {
            InstKind::Const {
                literal: ConstLiteral::Int(n),
                ..
            } => Some(*n),
            InstKind::Copy { src, .. } => self.const_int_depth(*src, depth + 1),
            InstKind::LoadGlobal {
                name,
                is_lazy_import: false,
                ..
            } if !self.module.dynamic => self.module.ints.get(name).copied(),
            InstKind::UnaryOp {
                op: UnaryOpKind::USub,
                operand,
                ..
            } => self.const_int_depth(*operand, depth + 1)?.checked_neg(),
            InstKind::UnaryOp {
                op: UnaryOpKind::UAdd,
                operand,
                ..
            } => self.const_int_depth(*operand, depth + 1),
            InstKind::BinOp {
                op,
                lhs,
                rhs,
                in_place: false,
                ..
            } => {
                let (a, b) = (
                    self.const_int_depth(*lhs, depth + 1)?,
                    self.const_int_depth(*rhs, depth + 1)?,
                );
                match op {
                    BinOpKind::Add => a.checked_add(b),
                    BinOpKind::Sub => a.checked_sub(b),
                    BinOpKind::Mult => a.checked_mul(b),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// The comparison `lhs <op> rhs` of a `while` test as a cap: `len(x) < N` (`N > len(x)`,
    /// `<=`, `>=`) gives `x` and the bound `len(x)` stays below while it holds, when `N` is a
    /// constant of at most [`LEN_CAP_MAX`].
    fn len_cap(&self, op: CompareOpKind, lhs: VarId, rhs: VarId) -> Option<LenCap> {
        let (measured, bound, inclusive) = match op {
            CompareOpKind::Lt => (lhs, rhs, false),
            CompareOpKind::LtE => (lhs, rhs, true),
            CompareOpKind::Gt => (rhs, lhs, false),
            CompareOpKind::GtE => (rhs, lhs, true),
            _ => return None,
        };
        let var = self.len_argument(measured)?;
        let n = self.const_int(bound)?.checked_add(i64::from(inclusive))?;
        let limit = u32::try_from(n).ok().filter(|n| *n <= LEN_CAP_MAX)?;
        Some(LenCap { var, limit })
    }

    /// The iterable of the `for` loop (or comprehension clause) `lp`: the operand of the
    /// `MakeIter` its header's `IterNext` advances (`None` for a `while` loop).
    pub fn loop_iterable(&self, lp: &NaturalLoop) -> Option<VarId> {
        let first = self.f.block(lp.header).insts.first()?;
        let InstKind::IterNext { iter, .. } = first.kind else {
            return None;
        };
        // The iterator is a `MakeIter`, whose one operand is the iterable.
        self.def_inst(iter)?.kind.operands().first().copied()
    }

    /// How many times the loop `f.loops[li]` runs at most, when the code fixes it: its
    /// recorded bound, else what its iterable yields or a test before it caps
    /// ([`Self::fixed_trip`]).
    fn trip_count(&self, li: usize) -> Option<u32> {
        if let Some(known) = self.trips.borrow().get(&li) {
            return *known;
        }
        let lp = self.f.loops.get(li)?;
        let trips = match lp.bound {
            LoopBound::ConstSmall(n) if self.recorded_trips => Some(n),
            LoopBound::DaemonPolling => None,
            // A retry loop over a fixed range records no count.
            LoopBound::ConstSmall(_) | LoopBound::RetryPattern | LoopBound::Unbounded => {
                self.trips.borrow_mut().insert(li, None);
                self.fixed_trip(lp, &mut |_| None)
            }
        };
        self.trips.borrow_mut().insert(li, trips);
        trips
    }

    /// The trip count the code fixes for the `for` loop `lp`: what its iterable yields
    /// ([`Self::iter_bound`], `leaf` deciding loads of globals and attributes), or the length a
    /// test every path to the loop passes caps it at ([`Self::guarded_len`]).
    pub fn fixed_trip(
        &self,
        lp: &NaturalLoop,
        leaf: &mut dyn FnMut(&Inst) -> Option<u32>,
    ) -> Option<u32> {
        let iterable = self.loop_iterable(lp)?;
        let made_iter = self.f.block(lp.header).insts.first()?.kind.operands();
        // The `MakeIter` of the header's `IterNext`, before the loop.
        let (at, _) = self.f.def_inst(*made_iter.first()?)?;
        self.iter_bound(iterable, leaf)
            .or_else(|| self.guarded_len(iterable, at, &lp.body))
    }

    /// The most items `v` holds in the block `at`, and while the blocks `during` run, by the
    /// tests before it: the smallest `N` of a `len(v) <= N` (or `< N + 1`, `== N`, the negation
    /// of `>` / `>=`) that every path to `at` passes, also through `not`, `and`, `or`, and
    /// comparison chains (`if not 1 <= len(v) <= 6: raise ...`), when nothing on the way to `at`
    /// or in `during` (a loop over `v`, whose iterator sees what the body appends) grows `v`;
    /// what happens to it after (`return {"cards": cards}`) leaves the length there as it is.
    fn guarded_len(&self, v: VarId, at: BlockId, during: &[BlockId]) -> Option<u32> {
        let v = self.copied_from(v);
        let measured = self.measured.get_or_init(|| {
            self.f
                .blocks
                .iter()
                .flat_map(|b| &b.insts)
                .filter_map(|i| Some(self.copied_from(self.len_argument(i.kind.dst()?)?)))
                .collect()
        });
        if !measured.contains(&v) {
            return None;
        }
        let bound = self
            .len_guards()
            .iter()
            .filter(|g| g.var == v && g.bound <= SMALL_MAX && self.dominates(g.edge_to, at))
            .map(|g| g.bound)
            .min()?;
        let mut on_the_way = self.reaching(at);
        for b in during {
            on_the_way[b.0 as usize] = true;
        }
        let origins = Origins {
            roots: vec![v],
            shared: true,
            ..Origins::default()
        };
        self.growth(&origins, None, Some(&on_the_way))
            .and(Some(bound))
    }

    fn preds(&self) -> &[Vec<BlockId>] {
        self.preds.get_or_init(|| self.f.predecessors())
    }

    /// Membership (by block id) of the blocks from which `b` can be reached (normal and
    /// exception edges), `b` included.
    fn reaching(&self, b: BlockId) -> Vec<bool> {
        let preds = self.preds();
        let mut seen = vec![false; self.f.blocks.len()];
        let mut stack = vec![b];
        while let Some(cur) = stack.pop() {
            if !std::mem::replace(&mut seen[cur.0 as usize], true) {
                stack.extend(preds[cur.0 as usize].iter().copied());
            }
        }
        seen
    }

    /// The length caps of `f`'s branches: for each edge into a block whose one predecessor
    /// branches on a test, the `len(x) <= N` facts the test's outcome on that edge implies.
    fn len_guards(&self) -> &[LenGuard] {
        self.guards.get_or_init(|| {
            let mut walk = CapWalk {
                preds: self.preds(),
                values: FxHashMap::default(),
                blocks: FxHashMap::default(),
            };
            let mut out = Vec::new();
            for bb in &self.f.blocks {
                let Terminator::Branch {
                    cond,
                    then_bb,
                    else_bb,
                } = bb.terminator
                else {
                    continue;
                };
                for (to, holds) in [(then_bb, true), (else_bb, false)] {
                    if self.preds().get(to.0 as usize).map(Vec::as_slice) != Some(&[bb.id]) {
                        continue;
                    }
                    for (var, bound) in self.implied_caps(cond, holds, &mut walk) {
                        out.push(LenGuard {
                            edge_to: to,
                            var,
                            bound,
                        });
                    }
                }
            }
            out
        })
    }

    /// The `(x, N)` with `len(x) <= N` that `v` being `holds` implies. A short-circuit `and` /
    /// `or` or a comparison chain is a phi of the values its
    /// operands give on the edges into it: an operand the edge already decided otherwise
    /// cannot be the outcome, and every other one implies a cap with the tests on the way to
    /// its edge (`len(x) > 5 or flag` is false only when `flag` is, after `len(x) > 5` was
    /// false).
    fn implied_caps(&self, v: VarId, holds: bool, walk: &mut CapWalk<'_>) -> Vec<(VarId, u32)> {
        if let Some(known) = walk.values.get(&(v, holds)) {
            return known.clone();
        }
        walk.values.insert((v, holds), Vec::new());
        let caps = match self.def(v) {
            Def::Inst(..) => match self.def_inst(v).map(|i| &i.kind) {
                Some(InstKind::Copy { src, .. }) => self.implied_caps(*src, holds, walk),
                Some(InstKind::UnaryOp {
                    op: UnaryOpKind::Not,
                    operand,
                    ..
                }) => self.implied_caps(*operand, !holds, walk),
                Some(InstKind::Compare { op, lhs, rhs, .. }) => self
                    .compared_len(*op, *lhs, *rhs, holds)
                    .into_iter()
                    .collect(),
                _ => Vec::new(),
            },
            Def::Phi(b, i) => {
                let mut common: Option<Vec<(VarId, u32)>> = None;
                for &(pred, x) in self.phi_incoming(b, i) {
                    // The value `x` has on the edge from a block branching on it.
                    let decided = match self.f.block(pred).terminator {
                        Terminator::Branch {
                            cond,
                            then_bb,
                            else_bb,
                        } if cond == x && (then_bb == b) != (else_bb == b) => Some(then_bb == b),
                        _ => None,
                    };
                    if decided.is_some_and(|value| value != holds) {
                        continue;
                    }
                    let mut caps = self.implied_caps(x, holds, walk);
                    caps.extend(self.caps_on_the_way(pred, walk));
                    common = Some(match common {
                        None => caps,
                        Some(before) => before
                            .into_iter()
                            .filter_map(|(var, n)| {
                                caps.iter()
                                    .find(|(w, _)| *w == var)
                                    .map(|(_, m)| (var, n.max(*m)))
                            })
                            .collect(),
                    });
                }
                common.unwrap_or_default()
            }
            Def::Param | Def::None => Vec::new(),
        };
        walk.values.insert((v, holds), caps.clone());
        caps
    }

    /// The caps the tests on the straight way into the end of block `b` imply: the branches
    /// into it and into its single-predecessor ancestors.
    fn caps_on_the_way(&self, b: BlockId, walk: &mut CapWalk<'_>) -> Vec<(VarId, u32)> {
        let single_pred = |walk: &CapWalk<'_>, blk: BlockId| match walk
            .preds
            .get(blk.0 as usize)
            .map(Vec::as_slice)
        {
            Some(&[p]) if p != blk => Some(p),
            _ => None,
        };
        // The way up to the first block answered already or without a single predecessor (a
        // cycle of such blocks is dead code).
        let mut way = Vec::new();
        let mut on_way = FxHashSet::default();
        let mut cur = b;
        let mut caps = loop {
            if let Some(known) = walk.blocks.get(&cur) {
                break known.clone();
            }
            match single_pred(walk, cur) {
                Some(p) if on_way.insert(cur) => {
                    way.push((cur, p));
                    cur = p;
                }
                _ => break Vec::new(),
            }
        };
        for &(blk, p) in way.iter().rev() {
            if let Terminator::Branch {
                cond,
                then_bb,
                else_bb,
            } = self.f.block(p).terminator
                && then_bb != else_bb
            {
                caps.extend(self.implied_caps(cond, then_bb == blk, walk));
            }
            walk.blocks.insert(blk, caps.clone());
        }
        caps
    }

    /// `(x, N)` when `lhs <op> rhs` being `holds` means `len(x) <= N` for a constant `N`.
    fn compared_len(
        &self,
        op: CompareOpKind,
        lhs: VarId,
        rhs: VarId,
        holds: bool,
    ) -> Option<(VarId, u32)> {
        use CompareOpKind::{Eq, Gt, GtE, Lt, LtE, NotEq};
        let op = match (op, holds) {
            (op, true) => op,
            (Lt, false) => GtE,
            (LtE, false) => Gt,
            (Gt, false) => LtE,
            (GtE, false) => Lt,
            (NotEq, false) => Eq,
            _ => return None,
        };
        if op == Eq {
            let (measured, bound) = match self.len_argument(lhs) {
                Some(x) => (x, rhs),
                None => (self.len_argument(rhs)?, lhs),
            };
            let n = u32::try_from(self.const_int(bound)?).ok()?;
            return Some((self.copied_from(measured), n));
        }
        let cap = self.len_cap(op, lhs, rhs)?;
        Some((self.copied_from(cap.var), cap.limit.checked_sub(1)?))
    }

    /// `x` when `v` is `len(x)` of the builtin.
    fn len_argument(&self, v: VarId) -> Option<VarId> {
        let InstKind::Call {
            dst,
            callee_var,
            callee,
            args,
            kwargs,
            ..
        } = &self.def_inst(v)?.kind
        else {
            return None;
        };
        match args.as_slice() {
            [x] if kwargs.is_empty()
                && !self.f.is_starred(*dst, 0)
                && self.builtin(*callee_var, callee) == Some("len") =>
            {
                Some(*x)
            }
            _ => None,
        }
    }

    /// Every path from the entry to `b` (normal and exception edges) runs through `d`.
    fn dominates(&self, d: BlockId, b: BlockId) -> bool {
        if d == b {
            return true;
        }
        let mut seen = vec![false; self.f.blocks.len()];
        let mut stack = vec![BlockId(0)];
        while let Some(cur) = stack.pop() {
            if cur == d || std::mem::replace(&mut seen[cur.0 as usize], true) {
                continue;
            }
            if cur == b {
                return false;
            }
            let block = self.f.block(cur);
            stack.extend(block.terminator.successors());
            stack.extend(block.exc_target);
        }
        true
    }

    // --------------------------------------------------------------------------- lifetimes

    /// Whether the object `v` holds outlives one iteration of `lp`: through `Copy`, `AttrLoad`,
    /// and `SubscriptLoad` by a loop-invariant index, its base object is a parameter, a global,
    /// defined before the loop, a loop-carried phi of the loop header, or a phi merging only
    /// such objects. A list built in the iteration (`parts = [...]` inside the loop), the loop
    /// element, or an object created from it (`row.cells`, `make(row)`) does not.
    pub fn lives_across(&self, lp: &NaturalLoop, v: VarId) -> bool {
        self.lives_across_seen(lp, v, &mut Lifetimes::default()).0
    }

    /// [`Self::lives_across`], and whether the answer reached a phi of the loop body (a phi
    /// reached again answers no). An instruction reached again answers as before without the
    /// walk: what it did, and no if it reached such a phi, which is now seen (the answer is a
    /// conjunction: once one part says no, the whole does).
    fn lives_across_seen(&self, lp: &NaturalLoop, v: VarId, walk: &mut Lifetimes) -> (bool, bool) {
        match self.def(v) {
            Def::Phi(b, i) if b != lp.header && lp.body.contains(&b) => {
                let lives = walk.seen.insert(v)
                    && self
                        .phi_incoming(b, i)
                        .iter()
                        .all(|&(_, x)| self.lives_across_seen(lp, x, walk).0);
                (lives, true)
            }
            Def::Inst(b, i) if lp.body.contains(&b) => {
                if let Some(&(lives, phi)) = walk.answered.get(&v) {
                    return (lives && !phi, phi);
                }
                let answer = match &self.f.block(b).insts[i as usize].kind {
                    InstKind::Copy { src: x, .. } | InstKind::AttrLoad { obj: x, .. } => {
                        self.lives_across_seen(lp, *x, walk)
                    }
                    InstKind::SubscriptLoad { obj, index, .. } => {
                        let (obj_lives, obj_phi) = self.lives_across_seen(lp, *obj, walk);
                        if obj_lives {
                            let (index_lives, index_phi) = self.lives_across_seen(lp, *index, walk);
                            (index_lives, obj_phi || index_phi)
                        } else {
                            (false, obj_phi)
                        }
                    }
                    InstKind::LoadGlobal { .. }
                    | InstKind::LoadDeref { .. }
                    | InstKind::LoadName { .. }
                    | InstKind::Const { .. } => (true, false),
                    _ => (false, false),
                };
                walk.answered.insert(v, answer);
                answer
            }
            // A parameter, a loop-carried phi of the header, or defined before the loop.
            _ => (true, false),
        }
    }
}

/// The number of items `range(*args)` yields (`None` for a zero step).
fn range_len(args: &[i64]) -> Option<i64> {
    let (start, stop, step) = match *args {
        [stop] => (0, stop, 1),
        [start, stop] => (start, stop, 1),
        [start, stop, step] => (start, stop, step),
        _ => return None,
    };
    let span = if step > 0 {
        stop.checked_sub(start)?
    } else if step < 0 {
        start.checked_sub(stop)?
    } else {
        return None;
    };
    let step = step.checked_abs()?;
    Some(if span <= 0 {
        0
    } else {
        span.checked_add(step - 1)? / step
    })
}

// ---------------------------------------------------------------------------- module pass

/// The id of the comprehension function a call runs (`__comp_<id>`, a comprehension before
/// 3.12).
fn comprehension_id(callee_var: Option<VarId>, callee: &str) -> Option<u32> {
    match callee_var {
        None => callee.strip_prefix("__comp_")?.parse().ok(),
        Some(_) => None,
    }
}

/// The constants of a module (see [`ModuleConstants`]), knowing what its comprehension
/// functions return (`comprehensions`).
fn module_constants(
    functions: &[IrFunction],
    module_fn: FunctionId,
    last_star_import: Option<u32>,
    comprehensions: BTreeMap<u32, SmallDisplay>,
) -> ModuleConstants {
    let module = &functions[module_fn.0 as usize];
    let mut stores: FxHashMap<&str, u32> = FxHashMap::default();
    let mut deleted: FxHashSet<&str> = FxHashSet::default();
    let mut dynamic = !module.a1_hazards.is_empty();
    for f in functions {
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            match &inst.kind {
                InstKind::StoreGlobal { name, .. } => *stores.entry(name).or_default() += 1,
                // `del NAME` at module level or after `global NAME` unbinds it.
                InstKind::Delete {
                    name, global: true, ..
                } => {
                    deleted.insert(name);
                }
                InstKind::Call {
                    callee,
                    callee_var: Some(_),
                    ..
                } if callee == "globals" => dynamic = true,
                _ => {}
            }
        }
    }
    let shadowed: BTreeSet<String> = READ_ONLY_BUILTINS
        .iter()
        .filter(|b| stores.contains_key(**b) || deleted.contains(**b))
        .map(|b| (*b).to_owned())
        .collect();
    let mut out = ModuleConstants {
        shadowed,
        dynamic,
        comprehensions,
        ..ModuleConstants::default()
    };
    if dynamic {
        return out;
    }
    // Ints first: displays and later ints may use them (`range(N)` only through loads).
    let ix = FnIndex::new(module, &out);
    let mut ints = BTreeMap::new();
    let mut candidates: Vec<(String, SmallDisplay)> = Vec::new();
    for bb in &module.blocks {
        if module.is_in_loop(bb.id) {
            continue;
        }
        for (i, inst) in bb.insts.iter().enumerate() {
            let InstKind::StoreGlobal { name, value } = &inst.kind else {
                continue;
            };
            let star_rebinds =
                last_star_import.is_some_and(|s| inst.span.start < s) && !name.starts_with('_');
            if stores.get(name.as_str()) != Some(&1)
                || deleted.contains(name.as_str())
                || star_rebinds
            {
                continue;
            }
            if let Some(n) = ix.const_int(*value) {
                ints.insert(name.clone(), n);
            } else if let Some(d) = ix.stored_display(*value, (bb.id, i)) {
                candidates.push((name.clone(), d));
            }
        }
    }
    out.ints = ints;
    if candidates.is_empty() {
        return out;
    }
    // A display constant is small only if no function grows what its loads give (nothing
    // can grow a tuple or a string).
    let names: FxHashSet<&str> = candidates
        .iter()
        .filter(|(_, d)| !d.is_immutable())
        .map(|(n, _)| n.as_str())
        .collect();
    let mut changed: FxHashSet<String> = FxHashSet::default();
    for f in functions {
        if f.id == module_fn {
            continue;
        }
        let loads: Vec<(VarId, &str)> = f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|inst| match &inst.kind {
                InstKind::LoadGlobal { dst, name, .. } | InstKind::LoadName { dst, name }
                    if names.contains(name.as_str()) =>
                {
                    Some((*dst, name.as_str()))
                }
                _ => None,
            })
            .collect();
        if loads.is_empty() {
            continue;
        }
        let grown = FnIndex::new(f, &out).grown_vars();
        for (dst, name) in loads {
            if grown.contains(&dst) {
                changed.insert(name.to_owned());
            }
        }
    }
    out.displays = candidates
        .into_iter()
        .filter(|(n, _)| !changed.contains(n))
        .collect();
    out
}

/// The functions that receive their first iterable from the function creating them (a
/// generator expression, a comprehension before 3.12): that creator and the iterable.
fn passed_iterables(functions: &[IrFunction]) -> FxHashMap<FunctionId, (FunctionId, VarId)> {
    let mut out = FxHashMap::default();
    for f in functions {
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            match &inst.kind {
                InstKind::MakeGenExpr {
                    func_id: Some(child),
                    outer_iter,
                    ..
                } => {
                    out.insert(*child, (f.id, *outer_iter));
                }
                InstKind::Call {
                    callee_var,
                    callee,
                    args,
                    ..
                } => {
                    if let (Some(id), Some(first)) =
                        (comprehension_id(*callee_var, callee), args.first())
                    {
                        out.insert(FunctionId(id), (f.id, *first));
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// How many rounds [`refine_loops`] takes at most: what a comprehension function returns can
/// bound a loop, whose comprehension's result it then bounds in turn.
const REFINE_ROUNDS: usize = 3;

/// Refine every loop's [`LoopBound`] and [`NaturalLoop::len_caps`] once the whole module is
/// lowered: names and module constants bound to small displays, `range` of constant
/// expressions, builtin wrappers, and iterables a test caps (`if len(xs) > 6: raise`) become
/// fixed trip counts; and decide what the module's comprehension functions return.
pub(crate) fn refine_loops(
    functions: &mut [IrFunction],
    module_fn: FunctionId,
    hints: &[(FunctionId, Vec<LoopHint>)],
    last_star_import: Option<u32>,
) -> ModuleConstants {
    let passed_iterables = passed_iterables(functions);
    for (child, passed) in &passed_iterables {
        if let Some(f) = functions.get_mut(child.0 as usize) {
            f.passed_from = Some(*passed);
        }
    }
    let mut constants = module_constants(functions, module_fn, last_star_import, BTreeMap::new());
    for _ in 0..REFINE_ROUNDS {
        refine_round(functions, hints, &constants);
        let returned = comprehension_results(functions, &constants);
        if returned == constants.comprehensions {
            break;
        }
        constants = module_constants(functions, module_fn, last_star_import, returned);
    }
    constants
}

/// What each comprehension function returns, when the code fixes its size (its loop runs over
/// the iterable its creator passes, which [`refine_round`] bounds).
fn comprehension_results(
    functions: &[IrFunction],
    constants: &ModuleConstants,
) -> BTreeMap<u32, SmallDisplay> {
    let ids: FxHashSet<u32> = functions
        .iter()
        .flat_map(|f| f.blocks.iter().flat_map(|b| &b.insts))
        .filter_map(|inst| match &inst.kind {
            InstKind::Call {
                callee_var, callee, ..
            } => comprehension_id(*callee_var, callee),
            _ => None,
        })
        .collect();
    let mut out = BTreeMap::new();
    for id in ids {
        if let Some(d) = functions
            .get(id as usize)
            .and_then(|f| FnIndex::new(f, constants).returned_display())
        {
            out.insert(id, d);
        }
    }
    out
}

/// One pass of [`refine_loops`] over the loops still `Unbounded` (and every loop's caps).
fn refine_round(
    functions: &mut [IrFunction],
    hints: &[(FunctionId, Vec<LoopHint>)],
    constants: &ModuleConstants,
) {
    let mut updates: Vec<(usize, usize, LoopBound, Vec<LenCap>)> = Vec::new();
    for (fid, loop_hints) in hints {
        let f = &functions[fid.0 as usize];
        // The iterable a generator expression or a comprehension function receives as `.0`
        // is bounded where its creator evaluated it.
        let passed_bound = |it: VarId| {
            let (creator, iterable) = f.passed_from?;
            (f.var_name(it) == Some(".0")).then_some(())?;
            FnIndex::new(&functions[creator.0 as usize], constants)
                .iter_bound(iterable, &mut |_| None)
        };
        let needs = f
            .loops
            .iter()
            .zip(loop_hints)
            .any(|(lp, h)| lp.bound == LoopBound::Unbounded || !h.cap_tests.is_empty());
        if !needs {
            continue;
        }
        let ix = FnIndex::new(f, constants);
        for (li, (lp, hint)) in f.loops.iter().zip(loop_hints).enumerate() {
            let mut bound = lp.bound;
            if bound == LoopBound::Unbounded
                && let Some(n) = ix
                    .fixed_trip(lp, &mut |_| None)
                    .or_else(|| ix.loop_iterable(lp).and_then(passed_bound))
            {
                bound = if hint.retry_body {
                    LoopBound::RetryPattern
                } else {
                    LoopBound::ConstSmall(n)
                };
            }
            let caps: Vec<LenCap> = if hint.cap_tests.is_empty() {
                Vec::new()
            } else {
                lp.body
                    .iter()
                    .flat_map(|b| &f.block(*b).insts)
                    .filter_map(|i| match &i.kind {
                        InstKind::Compare { op, lhs, rhs, .. }
                            if hint.cap_tests.contains(&i.span) =>
                        {
                            ix.len_cap(*op, *lhs, *rhs)
                        }
                        _ => None,
                    })
                    .collect()
            };
            if bound != lp.bound || !caps.is_empty() {
                updates.push((fid.0 as usize, li, bound, caps));
            }
        }
    }
    for (fi, li, bound, caps) in updates {
        let lp = &mut functions[fi].loops[li];
        lp.bound = bound;
        lp.len_caps = caps;
    }
}

#[cfg(test)]
mod tests {
    use super::range_len;

    #[test]
    fn range_len_matches_python() {
        for (args, n) in [
            (vec![3], 3),
            (vec![-2], 0),
            (vec![2, 5], 3),
            (vec![5, 2], 0),
            (vec![0, 10, 3], 4),
            (vec![10, 0, -3], 4),
            (vec![0, 10, -1], 0),
        ] {
            assert_eq!(range_len(&args), Some(n), "{args:?}");
        }
        assert_eq!(range_len(&[0, 1, 0]), None);
        assert_eq!(range_len(&[]), None);
    }
}
