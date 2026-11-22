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

mod origin;
mod paths;

pub use origin::{Carries, ItemOrigin, ItemOrigins, ItemSource};
pub use paths::{Decided, ModuleBindings, Must, PathReader, Paths, Progress};

use crate::walk::{Answer, Def, FnShape};
use crate::{
    BinOpKind, BlockId, BuiltinKind, CompareOpKind, ConstLiteral, DictEntry, FunctionId,
    FunctionKind, Inst, InstKind, IrFunction, LenCap, LoopBound, NaturalLoop, Terminator,
    UnaryOpKind, VarId,
};
use heki_ast::Span;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

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

/// The most times a loop runs, as the code bounds it ([`FnIndex::fixed_trips`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trips {
    /// The code fixes the count, whatever the input: its iterable is built from constants
    /// (`range(1000)`, a display, a string constant), a test pins its length (`len(x) == 40`),
    /// or a counter the code starts and steps runs it. The loop may still leave earlier.
    Fixed(u32),
    /// A constant only caps what may be input: a length test (`if len(x) > 1000: raise`), a
    /// slice (`rows[:100]`), a `zip` with an iterable the code does not bound. An upper bound,
    /// not a fixed count.
    Capped(u32),
}

impl Trips {
    fn of(n: u32, fixed: bool) -> Self {
        if fixed {
            Self::Fixed(n)
        } else {
            Self::Capped(n)
        }
    }

    /// The most times the loop runs.
    pub fn at_most(self) -> u32 {
        match self {
            Self::Fixed(n) | Self::Capped(n) => n,
        }
    }
}

/// A read down attributes and constant keys from a root ([`FnIndex::access_path`]).
#[derive(Debug, Clone, PartialEq, Eq)]
struct AccessPath {
    root: VarId,
    /// From the value up to the root.
    keys: Vec<PathKey>,
    /// The default of a last read by `get(key, default)`.
    default: Option<VarId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PathKey {
    Attr(String),
    Str(String),
    Int(i64),
}

/// How many reads an access path may chain.
const MAX_ACCESS_STEPS: u32 = 8;

/// The key a subscript or `get` reads: its string constant, or an integer the code fixes.
fn path_key(key_const: Option<&str>, index: Option<paths::Scalar<'_>>) -> Option<PathKey> {
    match (key_const, index) {
        (Some(k), _) | (None, Some(paths::Scalar::Str(k))) => Some(PathKey::Str(k.to_owned())),
        (None, Some(paths::Scalar::Int(n))) => Some(PathKey::Int(n)),
        _ => None,
    }
}

/// What a builtin wrapper of iterables yields ([`FnIndex::item_step`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ItemStep {
    /// `range(...)` of these (unstarred) arguments.
    Range(Vec<VarId>),
    /// The items of one iterable, or pairs holding them: `list`, `tuple`, `set`, `frozenset`,
    /// `dict`, `iter`, `reversed`, `sorted`, `enumerate`, and `.items()` / `.keys()` /
    /// `.values()`.
    Same(VarId),
    /// Some of the items of one iterable: `filter`.
    Kept(VarId),
    /// One result per item of one iterable: `map` of one.
    Mapped(VarId),
    /// Tuples of the items of these (unstarred) iterables, as many as the shortest yields: `zip`.
    Zip(Vec<VarId>),
}

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
    /// String constants (`SCHEMA = "CREATE TABLE ..."`). A string is immutable, so the name
    /// holds this text whatever the code does with it. A name bound to another (`OLD =
    /// SCHEMA`) shares its text.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub strs: BTreeMap<String, Arc<str>>,
    /// Tuple displays of string constants, their items in order (`STATEMENTS = ("CREATE ...",
    /// ...)`): a tuple is immutable too. Aliases and items share the texts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub str_tuples: BTreeMap<String, Arc<[Arc<str>]>>,
    /// Boolean constants (`_IS_SYNC = False`), and `typing.TYPE_CHECKING`, which is false
    /// whenever the code runs: a name bound to it by `from typing import TYPE_CHECKING`, and
    /// `t.TYPE_CHECKING` for a name `t` bound to `import typing` (the dotted key).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bools: BTreeMap<String, bool>,
    /// Names of the builtins the facts rely on (`range`, `len`, ...) that the module rebinds.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub shadowed: BTreeSet<String>,
    /// Module globals may be bound behind the IR (`exec`, or a change through the dict
    /// `globals()` returns: [`writes_globals`]): no name is fixed and no builtin is trusted.
    /// (A `from m import *` only rebinds the public names bound before it.)
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
    /// The loop does not step through items: a `while` loop, or a `for` loop whose body never
    /// reads its target (`for _ in range(30):`). When every iteration of such a loop waits, it
    /// polls or retries.
    pub repeats: bool,
    /// `while True:`: a loop that waits on every iteration is a resident one, not a retry.
    pub forever: bool,
}

/// The facts above for one function, computed on demand.
pub struct FnIndex<'a> {
    pub f: &'a IrFunction,
    /// The tables of `f` the facts' walks read, for the owner's other walks too.
    pub shape: FnShape<'a>,
    module: &'a ModuleConstants,
    /// [`Self::container_bound`] of each value asked so far. A value whose answer is being
    /// decided reads as not fixed: a size that depends on itself is not fixed by the code.
    bounds: RefCell<FxHashMap<VarId, Option<u32>>>,
    /// [`Self::trip_count`] of each loop (by its index in `f.loops`) asked so far, in the same
    /// way.
    trips: RefCell<FxHashMap<usize, Option<u32>>>,
    /// [`Self::display_at`] of each value and sink asked so far, in the same way: two lists that
    /// extend each other hold no display the code fixes.
    sink_displays: RefCell<FxHashMap<(VarId, Sink), Option<SmallDisplay>>>,
    /// The values `f` passes to `len()` (after copies): only their lengths can be tested.
    measured: OnceCell<FxHashSet<VarId>>,
    /// The length caps the branches of `f` establish ([`Self::guarded_len`]).
    guards: OnceCell<Vec<LenGuard>>,
    /// The branches the code of `f` decides ([`Self::decided`]).
    decided: OnceCell<Decided>,
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
/// branches on the test `cond`, which is `holds` on that edge, implying it).
#[derive(Debug, Clone, Copy)]
struct LenGuard {
    edge_to: BlockId,
    var: VarId,
    bound: u32,
    cond: VarId,
    holds: bool,
}

/// One cap per measured value, its smallest: the caps of one list all hold at once. Keeping
/// one per value bounds every list by the values the function measures, however often tests
/// repeat the same caps.
fn tightest(mut caps: Vec<(VarId, u32)>) -> Vec<(VarId, u32)> {
    caps.sort_unstable();
    caps.dedup_by_key(|(var, _)| *var);
    caps
}

/// The caps of [`FnIndex::implied_caps`] decided so far: per value and outcome (one being
/// decided implies nothing), and per block for the tests on the single-predecessor way into it.
#[derive(Default)]
struct CapWalk {
    values: FxHashMap<(VarId, bool), Vec<(VarId, u32)>>,
    blocks: FxHashMap<BlockId, Vec<(VarId, u32)>>,
}

/// Where a container's object may go without it counting as grown ([`FnIndex::growth`]):
/// the store that binds it (a module global or an attribute), or the function's return
/// (the result of a comprehension function).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
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
        Self::with_shape(FnShape::new(f), module)
    }

    /// The facts of the function whose tables an owner built before.
    pub fn with_shape(shape: FnShape<'a>, module: &'a ModuleConstants) -> Self {
        Self {
            f: shape.f,
            shape,
            module,
            bounds: RefCell::default(),
            trips: RefCell::default(),
            sink_displays: RefCell::default(),
            measured: OnceCell::new(),
            guards: OnceCell::new(),
            decided: OnceCell::new(),
            recorded_trips: true,
        }
    }

    /// This index with the fixed trip counts `f.loops` record decided again from its
    /// constants, which lack some display the recorded counts may rest on.
    pub fn redeciding_trips(mut self) -> Self {
        self.recorded_trips = false;
        self
    }

    fn phi_incoming(&self, b: BlockId, i: u32) -> &'a [(BlockId, VarId)] {
        &self.f.block(b).phis[i as usize].incoming
    }

    /// The builtin a call names, when it is one of the read-only builtins these facts know and
    /// the real one: a global load of the callee's name that the module never rebinds.
    pub fn builtin(&self, callee_var: Option<VarId>, callee: &str) -> Option<&'a str> {
        match &self.shape.inst(callee_var?)?.kind {
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
        if let Some(known) = self.sink_displays.borrow().get(&(value, sink)) {
            return known.clone();
        }
        self.sink_displays.borrow_mut().insert((value, sink), None);
        let display = self.decide_display(value, sink);
        self.sink_displays
            .borrow_mut()
            .insert((value, sink), display.clone());
        display
    }

    fn decide_display(&self, value: VarId, sink: Sink) -> Option<SmallDisplay> {
        let mut origins = Origins::default();
        let loads = |o: &Origins| {
            o.roots.iter().any(|r| {
                matches!(
                    self.shape.inst(*r).map(|i| &i.kind),
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
        let ok = match self.shape.def(v) {
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
    /// list, tuple, or set display, a string constant or an f-string that joins text, or a
    /// builtin constructor call (what a slice can slice).
    fn made_kind(&self, v: VarId) -> Option<BuiltinKind> {
        match &self.shape.inst(self.shape.copy_root(v))?.kind {
            InstKind::AllocBuiltin { kind, .. } => Some(*kind),
            InstKind::Const {
                literal: ConstLiteral::Str(_),
                ..
            }
            | InstKind::BuildString { .. } => Some(BuiltinKind::Str),
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
        let n = self.iter_bound_depth(*arg, &mut |_| None, depth + 1, SMALL_MAX, &mut false)?;
        Some((kind, n))
    }

    /// The most items a slice by the constant bounds `index` holds, whatever the length of
    /// what it slices: `[:k]`, `[a:b]`, `[-k:]`, `[-a:-b]` (a positive step divides them).
    fn slice_len(&self, index: VarId) -> Option<u32> {
        // A slice passes the tuple of its three bounds (`None` where one is left out).
        let parts: [VarId; 3] = self.shape.inst(index)?.kind.operands().try_into().ok()?;
        let bound = |v: VarId| -> Result<Option<i64>, ()> {
            match &self.shape.inst(self.shape.copy_root(v)).map(|i| &i.kind) {
                Some(InstKind::Const {
                    literal: ConstLiteral::None,
                    ..
                }) => Ok(None),
                _ => self.fixed_int(v).map(Some).ok_or(()),
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
            roots.iter().all(|r| {
                let def = self.shape.def(*r);
                matches!(def, Def::Inst(rb, _) | Def::Phi(rb, _) if self.shape.in_body(lp, rb))
            })
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
            if self.shape.in_loop(li, b) && (!made_in(lp) || carried(lp)) {
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
            | InstKind::Convert { .. }
            | InstKind::Format { .. }
            | InstKind::BuildString { .. }
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
                    self.shape.inst(*recv).map(|i| &i.kind),
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
                    self.shape.inst(*value).map(|i| &i.kind),
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
        let kind = self.shape.inst(v).map(|i| &i.kind);
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
        self.iter_bound_depth(v, leaf, 0, SMALL_MAX, &mut false)
    }

    /// [`Self::iter_bound`] up to `cap` items. Past [`SMALL_MAX`] it also counts what holds any
    /// number of items the code fixes: a slice by constant bounds, a tuple display, a list or set
    /// display nothing grows, and a module's string and string-tuple constants. `capped` is set
    /// when the bound only caps what may be input (a slice, a `zip` with an iterable it does not
    /// bound) instead of coming from collections the code fixes.
    fn iter_bound_depth(
        &self,
        v: VarId,
        leaf: &mut dyn FnMut(&Inst) -> Option<u32>,
        depth: u32,
        cap: u32,
        capped: &mut bool,
    ) -> Option<u32> {
        if depth > MAX_NESTED_SIZES {
            return None;
        }
        if let Some(n) = self.container_bound_nested(v, depth) {
            return Some(n);
        }
        let inst = self.shape.inst(v)?;
        let within = |n: usize| u32::try_from(n).ok().filter(|n| *n <= cap);
        if let Some((step, built)) = self.item_step(inst) {
            if let (Some(constructor), Some(dst)) = (built, inst.kind.dst()) {
                self.not_grown(dst, constructor)?;
            }
            return match step {
                ItemStep::Range(args) => {
                    let ints: Option<Vec<i64>> = args.iter().map(|a| self.fixed_int(*a)).collect();
                    range_len(&ints?).and_then(|n| within(n as usize))
                }
                ItemStep::Same(x) | ItemStep::Kept(x) | ItemStep::Mapped(x) => {
                    self.iter_bound_depth(x, leaf, depth + 1, cap, capped)
                }
                ItemStep::Zip(args) => {
                    let bounds: Vec<Option<u32>> = args
                        .into_iter()
                        .map(|a| self.iter_bound_depth(a, leaf, depth + 1, cap, capped))
                        .collect();
                    *capped |= bounds.iter().any(Option::is_none);
                    bounds.into_iter().flatten().min()
                }
            };
        }
        let past_small = cap > SMALL_MAX;
        match &inst.kind {
            InstKind::Copy { src, .. } => self.iter_bound_depth(*src, leaf, depth + 1, cap, capped),
            InstKind::Const {
                literal: ConstLiteral::Str(s),
                ..
            } => within(s.chars().count()),
            InstKind::SubscriptLoad {
                index,
                is_slice: true,
                ..
            } if past_small && !self.grows(&[v], &[], None) => {
                *capped = true;
                self.slice_len(*index)
            }
            InstKind::AllocBuiltin { dst, kind, elems }
                if past_small
                    && (0..elems.len()).all(|i| !self.f.is_starred(*dst, i))
                    && (is_immutable_kind(*kind) || !self.grows(&[v], &[*kind], None)) =>
            {
                within(elems.len())
            }
            InstKind::LoadGlobal {
                name,
                is_lazy_import: false,
                ..
            } if past_small
                && !self.module.dynamic
                && (self.module.strs.contains_key(name)
                    || self.module.str_tuples.contains_key(name)) =>
            {
                let n = self.module.strs.get(name).map_or_else(
                    || self.module.str_tuples.get(name).map_or(0, |t| t.len()),
                    |s| s.chars().count(),
                );
                within(n)
            }
            // A module body reads an imported name as a copy of its `Import`.
            InstKind::LoadGlobal { .. }
            | InstKind::LoadName { .. }
            | InstKind::AttrLoad { .. }
            | InstKind::Import { .. } => leaf(inst).filter(|n| *n <= cap),
            _ => None,
        }
    }

    /// The builtin wrapper `inst` calls on iterables, with what it takes of them: the builtin
    /// that the module does not rebind, called in a form whose items the result follows. The
    /// constructor named second builds a new container whose items stay its input's only while
    /// nothing grows it ([`Self::not_grown`]).
    fn item_step(&self, inst: &Inst) -> Option<(ItemStep, Option<&'static str>)> {
        match &inst.kind {
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
                let step = match name {
                    "range" if kwargs.is_empty() && (1..=3).contains(&args.len()) => {
                        ItemStep::Range((0..args.len()).map(plain).collect::<Option<_>>()?)
                    }
                    "enumerate" if args.len() <= 2 && keywords(&["start"]) => {
                        ItemStep::Same(plain(0)?)
                    }
                    "sorted" if args.len() == 1 && keywords(&["key", "reverse"]) => {
                        return Some((ItemStep::Same(plain(0)?), Some("sorted")));
                    }
                    n if SAME_ITEMS.contains(&n) && args.len() == 1 && kwargs.is_empty() => {
                        let constructor = SAME_ITEMS.iter().find(|c| **c == n).copied();
                        return Some((ItemStep::Same(plain(0)?), constructor));
                    }
                    "map" if args.len() == 2 && kwargs.is_empty() => ItemStep::Mapped(plain(1)?),
                    "filter" if args.len() == 2 && kwargs.is_empty() => ItemStep::Kept(plain(1)?),
                    "zip" if keywords(&["strict"]) => {
                        ItemStep::Zip((0..args.len()).filter_map(plain).collect())
                    }
                    _ => return None,
                };
                Some((step, None))
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
                Some((ItemStep::Same(*recv), None))
            }
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

    /// The instruction defining the value `v` holds, through copies.
    fn source(&self, v: VarId) -> Option<&'a Inst> {
        self.shape.inst(self.shape.copy_root(v))
    }

    /// The value of an integer expression the code fixes: a literal, a module constant, or
    /// `+`, `-`, `*` and negation of them.
    pub fn fixed_int(&self, v: VarId) -> Option<i64> {
        self.const_int_depth(v, 0)
    }

    fn const_int_depth(&self, v: VarId, depth: u32) -> Option<i64> {
        if depth > 16 {
            return None;
        }
        match &self.shape.inst(v)?.kind {
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
        let n = self.fixed_int(bound)?.checked_add(i64::from(inclusive))?;
        let limit = u32::try_from(n).ok().filter(|n| *n <= LEN_CAP_MAX)?;
        Some(LenCap { var, limit })
    }

    /// How many times the `while` loop `lp` runs at most when it counts with an integer the code
    /// fixes, if at most `cap` times: its test compares the counter with a constant
    /// (`while i < 6:`, `while 0 <= n:`) or tests the counter itself (`while tries:`, counting
    /// down to zero), the counter starts at a constant, and every iteration that goes on steps it
    /// by one constant (`i += 1` on each path back to the test). The test's false side leaves the
    /// loop, so an operand of `and` bounds the loop and one of `or` does not.
    pub fn counted_trips(&self, lp: &NaturalLoop, cap: u32) -> Option<u32> {
        let header = self.f.block(lp.header);
        let cond = match header.terminator {
            Terminator::Branch {
                cond,
                then_bb,
                else_bb,
            } if self.shape.in_body(lp, then_bb) && !self.shape.in_body(lp, else_bb) => cond,
            _ => return None,
        };
        // The test as `(counter, limit)` and whether it holds for their order.
        let (counter, limit, op, counter_first) = match self.shape.def(cond) {
            Def::Phi(..) => (cond, 0, CompareOpKind::NotEq, true),
            Def::Inst(b, i) if b == lp.header => match header.insts[i as usize].kind {
                InstKind::Compare { op, lhs, rhs, .. } => match self.fixed_int(rhs) {
                    Some(n) => (lhs, n, op, true),
                    None => (rhs, self.fixed_int(lhs)?, op, false),
                },
                _ => return None,
            },
            _ => return None,
        };
        let body: FxHashSet<BlockId> = lp.body.iter().copied().collect();
        let (start, step) = self.counter(&body, lp.header, counter)?;
        let mut value = start;
        // Run the test and the step as the loop does, up to one trip past a small count.
        for trips in 0..=SMALL_MAX {
            let order = if counter_first {
                value.cmp(&limit)
            } else {
                limit.cmp(&value)
            };
            if !compare_holds(op, order)? {
                return Some(trips);
            }
            value = value.checked_add(step)?;
        }
        if cap <= SMALL_MAX {
            return None;
        }
        // The counter as the left operand: `limit < i` tests `i > limit`.
        let op = if counter_first { op } else { mirrored(op) };
        counted_trips_past(start, step, limit, op).filter(|n| *n <= cap)
    }

    /// The start and the step of the counter `v` of the loop with `header` and `body`: a phi of
    /// the header whose input from before the loop is an integer constant and whose input along
    /// every path back is `v` plus (or minus) one integer constant.
    fn counter(&self, body: &FxHashSet<BlockId>, header: BlockId, v: VarId) -> Option<(i64, i64)> {
        let Def::Phi(b, i) = self.shape.def(v) else {
            return None;
        };
        if b != header {
            return None;
        }
        let mut start = None;
        let mut step = None;
        for &(pred, x) in self.phi_incoming(b, i) {
            // The value that enters the loop (the header's only predecessor outside it).
            if !body.contains(&pred) {
                start = Some(self.fixed_int(x)?);
                continue;
            }
            let this = match &self.source(x)?.kind {
                InstKind::BinOp {
                    op: BinOpKind::Add,
                    lhs,
                    rhs,
                    ..
                } if *lhs == v => self.fixed_int(*rhs)?,
                InstKind::BinOp {
                    op: BinOpKind::Add,
                    lhs,
                    rhs,
                    ..
                } if *rhs == v => self.fixed_int(*lhs)?,
                InstKind::BinOp {
                    op: BinOpKind::Sub,
                    lhs,
                    rhs,
                    ..
                } if *lhs == v => self.fixed_int(*rhs)?.checked_neg()?,
                _ => return None,
            };
            if this == 0 || step.is_some_and(|s| s != this) {
                return None;
            }
            step = Some(this);
        }
        Some((start?, step?))
    }

    /// The iterable of the `for` loop (or comprehension clause) `lp`: the operand of the
    /// `MakeIter` its header's `IterNext` advances (`None` for a `while` loop).
    pub fn loop_iterable(&self, lp: &NaturalLoop) -> Option<VarId> {
        let first = self.f.block(lp.header).insts.first()?;
        let InstKind::IterNext { iter, .. } = first.kind else {
            return None;
        };
        // The iterator is a `MakeIter`, whose one operand is the iterable.
        self.shape.inst(iter)?.kind.operands().first().copied()
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
                self.fixed_trip(lp, &mut |_| None, SMALL_MAX)
            }
        };
        self.trips.borrow_mut().insert(li, trips);
        trips
    }

    /// The trip count the code fixes for the `for` loop `lp`, up to `cap`: what its iterable
    /// yields ([`Self::iter_bound`], `leaf` deciding loads of globals and attributes), or the
    /// length a test every path to the loop passes caps it at ([`Self::guarded_len`]).
    pub fn fixed_trip(
        &self,
        lp: &NaturalLoop,
        leaf: &mut dyn FnMut(&Inst) -> Option<u32>,
        cap: u32,
    ) -> Option<u32> {
        let iterable = self.loop_iterable(lp)?;
        let made_iter = self.f.block(lp.header).insts.first()?.kind.operands();
        // The `MakeIter` of the header's `IterNext`, before the loop.
        let (at, _) = self.shape.defining(*made_iter.first()?)?;
        self.iter_bound_depth(iterable, leaf, 0, cap, &mut false)
            .or_else(|| self.guarded_len(iterable, at, &lp.body, cap, &mut false))
    }

    /// The most times the loop `lp` runs, when something the code states bounds it at any count
    /// ([`Trips`]): what its iterable yields (past [`SMALL_MAX`] too: `range(1000)`, a display or
    /// string constant of any length, `rows[:100]`, `zip(rows, range(50))`), a length test every
    /// path to the loop passes, on the iterable or on the same access path (up to
    /// [`LEN_CAP_MAX`]), or the counter a `while` loop steps ([`Self::counted_trips`]). `Open`
    /// when nothing these facts read bounds it. No claim rests on a size: an open count only
    /// leaves the loop data-sized to the rules that read it so. `leaf` decides loads of globals
    /// and attributes.
    pub fn fixed_trips(
        &self,
        lp: &NaturalLoop,
        leaf: &mut dyn FnMut(&Inst) -> Option<u32>,
    ) -> Answer<Trips> {
        let Some(iterable) = self.loop_iterable(lp) else {
            return self
                .counted_trips(lp, u32::MAX)
                .map_or(Answer::Open, |n| Answer::Known(Trips::Fixed(n)));
        };
        let mut capped = false;
        if let Some(n) = self.iter_bound_depth(iterable, leaf, 0, u32::MAX, &mut capped) {
            return Answer::Known(Trips::of(n, !capped));
        }
        // The `MakeIter` of the header's `IterNext`, before the loop.
        let made = self
            .f
            .block(lp.header)
            .insts
            .first()
            .map(|i| i.kind.operands());
        let at = made
            .and_then(|m| m.first().copied())
            .and_then(|m| self.shape.defining(m))
            .map(|(b, _)| b);
        let mut pinned = false;
        let guarded =
            at.and_then(|at| self.guarded_len(iterable, at, &lp.body, LEN_CAP_MAX, &mut pinned));
        match (guarded, lp.bound) {
            (Some(n), _) => Answer::Known(Trips::of(n, pinned)),
            // A count the IR records without saying where it comes from (the iterable a
            // generator expression's creator passes): only a bound.
            (None, LoopBound::ConstSmall(n)) if self.recorded_trips => {
                Answer::Known(Trips::Capped(n))
            }
            (None, _) => Answer::Open,
        }
    }

    /// The most items `v` holds in the block `at`, and while the blocks `during` run, by the
    /// tests before it: the smallest `N` of a `len(v) <= N` (or `< N + 1`, `== N`, the negation
    /// of `>` / `>=`) that every path to `at` passes, also through `not`, `and`, `or`, and
    /// comparison chains (`if not 1 <= len(v) <= 6: raise ...`), when nothing on the way to `at`
    /// or in `during` (a loop over `v`, whose iterator sees what the body appends) grows `v`;
    /// what happens to it after (`return {"cards": cards}`) leaves the length there as it is.
    fn guarded_len(
        &self,
        v: VarId,
        at: BlockId,
        during: &[BlockId],
        cap: u32,
        pinned: &mut bool,
    ) -> Option<u32> {
        let v = self.shape.copy_root(v);
        let measured = self.measured.get_or_init(|| {
            self.f
                .blocks
                .iter()
                .flat_map(|b| &b.insts)
                .filter_map(|i| Some(self.shape.copy_root(self.len_argument(i.kind.dst()?)?)))
                .collect()
        });
        let path = self.access_path(v);
        if !measured.contains(&v) && path.is_none() {
            return None;
        }
        // Every path from the entry to `at` (normal and exception edges) runs through the guarded
        // edge: also when no path from the entry reaches `at`, since none avoids the edge either.
        let shape = &self.shape;
        let guards_at =
            |to: BlockId| to == at || !shape.reachable(at) || shape.dominators().dominates(to, at);
        let mut on_the_way = self.reaching(at);
        for b in during {
            on_the_way[b.0 as usize] = true;
        }
        let unchanged = |values: Vec<VarId>| {
            let origins = Origins {
                roots: values,
                shared: true,
                ..Origins::default()
            };
            self.growth(&origins, None, Some(&on_the_way)).is_some()
        };
        let guards: Vec<&LenGuard> = self
            .len_guards()
            .iter()
            .filter(|g| g.bound <= cap && guards_at(g.edge_to))
            .collect();
        // A test of the value itself holds while nothing on the way grows it.
        let same: Vec<&LenGuard> = guards.iter().copied().filter(|g| g.var == v).collect();
        let mut tested: Vec<&LenGuard> = if !same.is_empty() && unchanged(vec![v]) {
            same
        } else {
            Vec::new()
        };
        // A test of another read of the same access path from the same root (`if
        // len(report.get("runs", [])) != 40: raise`, then `for run in report["runs"]`), whose
        // `get` default the test rejects so that the key is there, holds while nothing on the
        // way grows the reads or changes the root: a store into it, a deletion from it, or a call
        // it is passed to (reading it is what the path does).
        if let Some(p) = &path {
            let linked: Vec<&LenGuard> = guards
                .iter()
                .copied()
                .filter(|g| {
                    g.var != v
                        && self.access_path(g.var).is_some_and(|q| {
                            q.root == p.root
                                && q.keys == p.keys
                                && q.default.is_none_or(|d| {
                                    self.rejects_length(d, g.bound, self.pins_len(g))
                                })
                        })
                })
                .collect();
            let root = self.aliases(&[p.root]);
            let mut ops = Vec::new();
            let root_changed = self
                .f
                .blocks
                .iter()
                .filter(|bb| on_the_way[bb.id.0 as usize])
                .flat_map(|bb| &bb.insts)
                .any(|i| match &i.kind {
                    InstKind::AttrLoad { .. } => false,
                    InstKind::AttrStore { obj, .. }
                    | InstKind::AttrDelete { obj, .. }
                    | InstKind::SubscriptDelete { obj, .. }
                        if root.contains(obj) =>
                    {
                        true
                    }
                    _ => {
                        ops.clear();
                        self.grown_operands(i, false, &mut ops);
                        ops.iter().any(|o| root.contains(o))
                    }
                });
            let mut values = vec![v];
            values.extend(linked.iter().map(|g| g.var));
            if !linked.is_empty() && !root_changed && unchanged(values) {
                tested.extend(linked);
            }
        }
        let bound = tested.iter().map(|g| g.bound).min()?;
        *pinned = tested.iter().any(|g| g.bound == bound && self.pins_len(g));
        Some(bound)
    }

    /// The access path `v` is read down: its root and the attributes and constant keys from
    /// it (`report["runs"]`, `cfg.items`, `report.get("runs", [])`, whose default it keeps).
    fn access_path(&self, v: VarId) -> Option<AccessPath> {
        let shape = &self.shape;
        let mut keys = Vec::new();
        let mut default = None;
        let mut cur = shape.copy_root(v);
        for step in 0..MAX_ACCESS_STEPS {
            let key = match shape.inst(cur).map(|i| &i.kind) {
                Some(InstKind::AttrLoad { obj, attr, .. }) => {
                    cur = shape.copy_root(*obj);
                    PathKey::Attr(attr.clone())
                }
                Some(InstKind::SubscriptLoad {
                    obj,
                    index,
                    key_const,
                    is_slice: false,
                    ..
                }) => {
                    let key = path_key(key_const.as_deref(), paths::scalar(self, *index))?;
                    cur = shape.copy_root(*obj);
                    key
                }
                // Only the last read may be a `get`: a missing key gives the default there.
                Some(InstKind::MethodCall {
                    recv,
                    method,
                    args,
                    kwargs,
                    star_from: None,
                    ..
                }) if step == 0
                    && method == "get"
                    && kwargs.is_empty()
                    && matches!(args.len(), 1 | 2) =>
                {
                    let key = path_key(None, paths::scalar(self, args[0]))?;
                    default = args.get(1).copied();
                    cur = shape.copy_root(*recv);
                    key
                }
                _ => break,
            };
            keys.push(key);
        }
        (!keys.is_empty()).then_some(AccessPath {
            root: cur,
            keys,
            default,
        })
    }

    /// Whether the test giving `g` fixes the length at its bound (`len(x) == N`, or `!= N`
    /// leaving), not only caps it.
    fn pins_len(&self, g: &LenGuard) -> bool {
        let shape = &self.shape;
        // Each `not` around the comparison flips the outcome the edge gives it.
        let not_of = |&(v, holds): &(VarId, bool)| match shape.inst(v).map(|i| &i.kind) {
            Some(InstKind::UnaryOp {
                op: UnaryOpKind::Not,
                operand,
                ..
            }) => Some((shape.copy_root(*operand), !holds)),
            _ => None,
        };
        let start = (shape.copy_root(g.cond), g.holds);
        let (v, holds) = std::iter::successors(Some(start), not_of)
            .take(MAX_ACCESS_STEPS as usize)
            .fold(start, |_, step| step);
        let Some(InstKind::Compare { op, lhs, rhs, .. }) = shape.inst(v).map(|i| &i.kind) else {
            return false;
        };
        let equal = matches!(
            (op, holds),
            (CompareOpKind::Eq, true) | (CompareOpKind::NotEq, false)
        );
        let bound =
            |x: VarId| paths::scalar(self, x) == Some(paths::Scalar::Int(i64::from(g.bound)));
        let measures = |x: VarId| self.len_argument(x).is_some();
        equal && ((measures(*lhs) && bound(*rhs)) || (measures(*rhs) && bound(*lhs)))
    }

    /// Whether a test that the length is at most `bound` (exactly `bound` when `pinned`) fails
    /// for the value `default`: `None` has no length, and a literal one whose length breaks it.
    fn rejects_length(&self, default: VarId, bound: u32, pinned: bool) -> bool {
        let shape = &self.shape;
        let len = match shape.inst(shape.copy_root(default)).map(|i| &i.kind) {
            Some(InstKind::Const {
                literal: ConstLiteral::None,
                ..
            }) => return true,
            Some(InstKind::Const {
                literal: ConstLiteral::Str(text),
                ..
            }) => text.chars().count(),
            Some(InstKind::AllocBuiltin { dst, elems, .. })
                if shape.first_starred(*dst).is_none() =>
            {
                elems.len()
            }
            _ => return false,
        };
        let len = u32::try_from(len).unwrap_or(u32::MAX);
        len > bound || (pinned && len != bound)
    }

    /// Membership (by block id) of the blocks from which `b` can be reached (normal and
    /// exception edges), `b` included.
    fn reaching(&self, b: BlockId) -> Vec<bool> {
        let mut seen = vec![false; self.f.blocks.len()];
        let mut stack = vec![b];
        while let Some(cur) = stack.pop() {
            if !std::mem::replace(&mut seen[cur.0 as usize], true) {
                stack.extend(self.shape.preds(cur).iter().copied());
            }
        }
        seen
    }

    /// The length caps of `f`'s branches: for each edge into a block whose one predecessor
    /// branches on a test, the `len(x) <= N` facts the test's outcome on that edge implies.
    fn len_guards(&self) -> &[LenGuard] {
        self.guards.get_or_init(|| {
            let mut walk = CapWalk::default();
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
                    if self.shape.preds(to) != [bb.id] {
                        continue;
                    }
                    for (var, bound) in self.implied_caps(cond, holds, &mut walk) {
                        out.push(LenGuard {
                            edge_to: to,
                            var,
                            bound,
                            cond,
                            holds,
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
    fn implied_caps(&self, v: VarId, holds: bool, walk: &mut CapWalk) -> Vec<(VarId, u32)> {
        if let Some(known) = walk.values.get(&(v, holds)) {
            return known.clone();
        }
        walk.values.insert((v, holds), Vec::new());
        let caps = match self.shape.def(v) {
            Def::Inst(..) => match self.shape.inst(v).map(|i| &i.kind) {
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
        let caps = tightest(caps);
        walk.values.insert((v, holds), caps.clone());
        caps
    }

    /// The caps the tests on the straight way into the end of block `b` imply: the branches
    /// into it and into its single-predecessor ancestors.
    fn caps_on_the_way(&self, b: BlockId, walk: &mut CapWalk) -> Vec<(VarId, u32)> {
        let single_pred = |blk: BlockId| match self.shape.preds(blk) {
            &[p] if p != blk => Some(p),
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
            match single_pred(cur) {
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
                caps = tightest(caps);
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
            let n = u32::try_from(self.fixed_int(bound)?).ok()?;
            return Some((self.shape.copy_root(measured), n));
        }
        let cap = self.len_cap(op, lhs, rhs)?;
        Some((self.shape.copy_root(cap.var), cap.limit.checked_sub(1)?))
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
        } = &self.shape.inst(v)?.kind
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
        match self.shape.def(v) {
            Def::Phi(b, i) if b != lp.header && self.shape.in_body(lp, b) => {
                let lives = walk.seen.insert(v)
                    && self
                        .phi_incoming(b, i)
                        .iter()
                        .all(|&(_, x)| self.lives_across_seen(lp, x, walk).0);
                (lives, true)
            }
            Def::Inst(b, i) if self.shape.in_body(lp, b) => {
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

/// Whether every path from the header of `lp` back to it runs a call that waits (a sleep, or a
/// wait with a timeout): the loop is paced by time, as a poll or a retry is, and the data
/// decides nothing about how often it runs. An exception edge leaves a block before its wait.
fn waits_every_iteration(shape: &FnShape<'_>, lp: &NaturalLoop) -> bool {
    let f = shape.f;
    let waits = |b: BlockId| f.block(b).insts.iter().any(|i| waits_for_time(shape, i));
    if !lp.body.iter().any(|b| waits(*b)) {
        return false;
    }
    let body: FxHashSet<BlockId> = lp.body.iter().copied().collect();
    let mut seen: FxHashSet<BlockId> = FxHashSet::default();
    let mut stack = vec![lp.header];
    while let Some(b) = stack.pop() {
        if !seen.insert(b) {
            continue;
        }
        let bb = f.block(b);
        let next = if waits(b) {
            Vec::new()
        } else {
            bb.terminator.successors()
        };
        for s in next.into_iter().chain(bb.exc_target) {
            if s == lp.header {
                return false;
            }
            if body.contains(&s) {
                stack.push(s);
            }
        }
    }
    true
}

/// A call that waits before the program goes on: a sleep of a duration other than a literal
/// zero (`time.sleep(5)`, `await asyncio.sleep(delay)`; `sleep(0)` only yields), or a wait
/// with a timeout that is not a literal zero (`stop.wait(1)`, `event.wait(timeout=delay)`;
/// `wait(timeout=0)` returns at once), awaited in a coroutine. A `wait` given anything else
/// may wait for futures that are done already (`asyncio.wait(tasks)`).
fn waits_for_time(shape: &FnShape<'_>, inst: &Inst) -> bool {
    let f = shape.f;
    let (name, args, kwargs, is_awaited) = match &inst.kind {
        InstKind::Call {
            callee,
            args,
            kwargs,
            is_awaited,
            ..
        } => (
            callee.rsplit('.').next().unwrap_or(callee),
            args,
            kwargs,
            *is_awaited,
        ),
        InstKind::MethodCall {
            method,
            args,
            kwargs,
            is_awaited,
            ..
        } => (method.as_str(), args, kwargs, *is_awaited),
        _ => return false,
    };
    // In a coroutine, a call it does not await may only build the coroutine of a wait that
    // runs elsewhere (`asyncio.create_task(asyncio.sleep(1))`).
    let coroutine = matches!(f.kind, FunctionKind::Async | FunctionKind::AsyncGenerator);
    if coroutine && !is_awaited {
        return false;
    }
    let seconds = |v: VarId| match shape.inst(v).map(|i| &i.kind) {
        Some(InstKind::Const {
            literal: ConstLiteral::Int(n),
            ..
        }) => Some(*n as f64),
        Some(InstKind::Const {
            literal: ConstLiteral::Float(x),
            ..
        }) => x.parse::<f64>().ok(),
        _ => None,
    };
    match name {
        "sleep" => args
            .first()
            .or(kwargs.first().map(|(_, v)| v))
            .is_some_and(|v| seconds(*v) != Some(0.0)),
        "wait" => {
            kwargs.iter().any(|(k, t)| {
                k.as_deref() == Some("timeout") && seconds(*t).is_none_or(|s| s > 0.0)
            }) || matches!(args.as_slice(), [t] if seconds(*t).is_some_and(|s| s > 0.0))
        }
        _ => false,
    }
}

/// Whether integers in `order` pass the comparison `op` (`None` for a test of identity or
/// membership).
/// The operator that tests `b <op'> a` exactly when `a <op> b` holds.
fn mirrored(op: CompareOpKind) -> CompareOpKind {
    match op {
        CompareOpKind::Lt => CompareOpKind::Gt,
        CompareOpKind::LtE => CompareOpKind::GtE,
        CompareOpKind::Gt => CompareOpKind::Lt,
        CompareOpKind::GtE => CompareOpKind::LtE,
        symmetric => symmetric,
    }
}

/// How many times a test `i <op> limit` holds for `i` = `start`, `start + step`, ... before it
/// first fails, when the counter moves towards the limit so that it fails once and for all:
/// `None` when it never fails or the operator does not order the counter.
fn counted_trips_past(start: i64, step: i64, limit: i64, op: CompareOpKind) -> Option<u32> {
    let (start, step, limit) = (i128::from(start), i128::from(step), i128::from(limit));
    // The trips while `distance` stays at least zero, shrinking by `by` (> 0) per trip.
    let while_non_negative = |distance: i128, by: i128| {
        if distance < 0 { 0 } else { distance / by + 1 }
    };
    let trips = match op {
        CompareOpKind::Lt if step > 0 => while_non_negative(limit - 1 - start, step),
        CompareOpKind::LtE if step > 0 => while_non_negative(limit - start, step),
        CompareOpKind::Gt if step < 0 => while_non_negative(start - limit - 1, -step),
        CompareOpKind::GtE if step < 0 => while_non_negative(start - limit, -step),
        CompareOpKind::NotEq if (limit - start) % step == 0 && (limit - start) / step >= 0 => {
            (limit - start) / step
        }
        _ => return None,
    };
    u32::try_from(trips).ok()
}

fn compare_holds(op: CompareOpKind, order: std::cmp::Ordering) -> Option<bool> {
    Some(match op {
        CompareOpKind::Eq => order.is_eq(),
        CompareOpKind::NotEq => order.is_ne(),
        CompareOpKind::Lt => order.is_lt(),
        CompareOpKind::LtE => order.is_le(),
        CompareOpKind::Gt => order.is_gt(),
        CompareOpKind::GtE => order.is_ge(),
        _ => return None,
    })
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

/// The size of a string or a tuple of `len` items, when it is small.
fn immutable_display(kind: BuiltinKind, len: usize) -> Option<SmallDisplay> {
    let len = u32::try_from(len).ok().filter(|n| *n <= SMALL_MAX)?;
    Some(SmallDisplay {
        kinds: vec![kind],
        len,
    })
}

/// The texts of a module's string constants and tuples of them ([`ModuleConstants::strs`],
/// [`ModuleConstants::str_tuples`]), one allocation per string or tuple the module writes: a
/// name bound to another (`S1 = S0`) and a tuple item share it, so the tables grow with the
/// source, not with its aliases.
#[derive(Default)]
struct SharedTexts {
    /// By the string's `Const`.
    texts: FxHashMap<VarId, Arc<str>>,
    /// By the tuple's display.
    tuples: FxHashMap<VarId, Arc<[Arc<str>]>>,
}

impl SharedTexts {
    /// The string constant `v` holds.
    fn text(&mut self, ix: &FnIndex<'_>, v: VarId) -> Option<Arc<str>> {
        match &ix.source(v)?.kind {
            InstKind::Const {
                dst,
                literal: ConstLiteral::Str(s),
            } => Some(
                self.texts
                    .entry(*dst)
                    .or_insert_with(|| Arc::from(s.as_str()))
                    .clone(),
            ),
            _ => None,
        }
    }

    /// The items of the tuple display of string constants `v` holds.
    fn tuple(&mut self, ix: &FnIndex<'_>, v: VarId) -> Option<Arc<[Arc<str>]>> {
        let InstKind::AllocBuiltin {
            dst,
            kind: BuiltinKind::Tuple,
            elems,
        } = &ix.source(v)?.kind
        else {
            return None;
        };
        if let Some(items) = self.tuples.get(dst) {
            return Some(items.clone());
        }
        let items: Arc<[Arc<str>]> = (elems.iter().enumerate())
            .map(|(i, e)| self.text(ix, *e).filter(|_| !ix.f.is_starred(*dst, i)))
            .collect::<Option<_>>()?;
        self.tuples.insert(*dst, items.clone());
        Some(items)
    }
}

/// The id of the comprehension function a call runs (`__comp_<id>`, a comprehension before
/// 3.12, called with the iterable of its first `for` clause).
pub fn comprehension_id(callee_var: Option<VarId>, callee: &str) -> Option<u32> {
    match callee_var {
        None => callee.strip_prefix("__comp_")?.parse().ok(),
        Some(_) => None,
    }
}

/// Methods of the dict of module globals that change nothing (`globals().get(name)`).
const READING_DICT_METHODS: &[&str] = &[
    "get",
    "keys",
    "values",
    "items",
    "copy",
    "__getitem__",
    "__contains__",
    "__len__",
    "__iter__",
];

/// Builtins that only read the dict they are handed as their first argument (`len(globals())`,
/// `sorted(globals())`).
const READING_BUILTINS: &[&str] = &[
    "len",
    "list",
    "tuple",
    "sorted",
    "dict",
    "set",
    "frozenset",
    "iter",
    "repr",
    "str",
    "bool",
];

/// Whether `f` may bind a module global behind the IR through the dict `globals()` returns
/// (`vars()` and `locals()` are A1 hazards of their own), when the builtins `module` trusts are
/// the real ones. Storing or deleting an item, calling a method that changes the dict, or
/// handing it on where other code may change it (a stored, returned, or merged value, an
/// argument of a call other than the object a real reading builtin reads, or an operand whose
/// operation runs another object's method) may bind any name, also one declared `Final`, which
/// the runtime does not enforce. Reading the dict (`globals().get(name)`, `globals()[name]`,
/// `name in globals()`, iterating it) binds nothing. The builtin `globals` itself handed on (`g
/// = globals`) may be called anywhere, and a call of whatever the code binds to that name may
/// return the dict.
fn writes_globals(f: &IrFunction, module: &ModuleConstants) -> bool {
    let dict_call = |kind: &InstKind| match kind {
        InstKind::Call {
            dst,
            callee,
            callee_var: Some(_),
            args,
            kwargs,
            ..
        } if args.is_empty() && kwargs.is_empty() && callee == "globals" => Some(*dst),
        _ => None,
    };
    let insts = || f.blocks.iter().flat_map(|b| &b.insts);
    let dicts: FxHashSet<VarId> = insts().filter_map(|i| dict_call(&i.kind)).collect();
    let names_globals = |kind: &InstKind| match kind {
        InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. } => name == "globals",
        _ => false,
    };
    if dicts.is_empty() && !insts().any(|i| names_globals(&i.kind)) {
        return false;
    }
    let ix = FnIndex::new(f, module);
    let is_dict = |v: VarId| dicts.contains(&ix.shape.copy_root(v));
    let callees: FxHashSet<VarId> = insts()
        .filter_map(|i| match &i.kind {
            InstKind::Call {
                callee_var: Some(c),
                ..
            } => Some(*c),
            _ => None,
        })
        .collect();
    let reads_only = |kind: &InstKind| match kind {
        InstKind::Copy { .. } | InstKind::MakeIter { .. } => true,
        // `obj[d]` hands the dict to `obj.__getitem__`.
        InstKind::SubscriptLoad { obj, .. } => is_dict(*obj),
        // `d in obj` runs `obj.__contains__(d)`, and a rich comparison runs the other operand's
        // method (reflected, when the dict's own returns `NotImplemented`) unless that is the
        // dict too.
        InstKind::Compare { op, lhs, rhs, .. } => match op {
            CompareOpKind::Is | CompareOpKind::IsNot => true,
            CompareOpKind::In | CompareOpKind::NotIn => is_dict(*rhs),
            _ => is_dict(*lhs) && is_dict(*rhs),
        },
        InstKind::MethodCall {
            recv,
            method,
            args,
            kwargs,
            ..
        } => {
            is_dict(*recv)
                && READING_DICT_METHODS.contains(&method.as_str())
                && !args
                    .iter()
                    .chain(kwargs.iter().map(|(_, v)| v))
                    .any(|v| is_dict(*v))
        }
        // Past the first argument the dict is no object read: `iter(f, d)` compares every item
        // `f` returns with it, through the item's `__eq__`.
        InstKind::Call {
            callee,
            callee_var,
            args,
            kwargs,
            ..
        } => {
            ix.builtin(*callee_var, callee)
                .is_some_and(|b| READING_BUILTINS.contains(&b))
                && kwargs.is_empty()
                && !args.iter().skip(1).any(|a| is_dict(*a))
        }
        _ => false,
    };
    f.blocks.iter().any(|bb| {
        bb.phis
            .iter()
            .any(|phi| phi.incoming.iter().any(|(_, v)| is_dict(*v)))
            || bb.insts.iter().any(|inst| {
                let handed_on = names_globals(&inst.kind)
                    && inst.kind.dst().is_some_and(|d| !callees.contains(&d));
                let changed =
                    inst.kind.operands().into_iter().any(is_dict) && !reads_only(&inst.kind);
                handed_on || changed
            })
            || match bb.terminator {
                Terminator::Return(Some(v)) | Terminator::Raise(Some(v)) => is_dict(v),
                _ => false,
            }
    })
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
        comprehensions,
        ..ModuleConstants::default()
    };
    // A builtin the module does not rebind is the real one unless some function changes the
    // globals behind the IR, which is what this decides: when no function does so with the
    // builtins taken as real, none does.
    out.dynamic =
        !module.a1_hazards.is_empty() || functions.iter().any(|f| writes_globals(f, &out));
    if out.dynamic {
        return out;
    }
    // Ints first: displays and later ints may use them (`range(N)` only through loads).
    let ix = FnIndex::new(module, &out);
    let mut ints = BTreeMap::new();
    let mut strs = BTreeMap::new();
    let mut str_tuples = BTreeMap::new();
    let mut bools = BTreeMap::new();
    let mut shared = SharedTexts::default();
    let mut candidates: Vec<(String, SmallDisplay)> = Vec::new();
    for bb in &module.blocks {
        if ix.shape.loop_depth(bb.id) > 0 {
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
            // A string constant, or a tuple of them, is one entry: its text, whose length is its
            // size (both are immutable, whatever their uses).
            if let Some(n) = ix.fixed_int(*value) {
                ints.insert(name.clone(), n);
            } else if let Some((key, b)) = bool_constant(&ix, name, *value) {
                bools.insert(key, b);
            } else if let Some(text) = shared.text(&ix, *value) {
                candidates.extend(
                    immutable_display(BuiltinKind::Str, text.chars().count())
                        .map(|d| (name.clone(), d)),
                );
                strs.insert(name.clone(), text);
            } else if let Some(items) = shared.tuple(&ix, *value) {
                candidates.extend(
                    immutable_display(BuiltinKind::Tuple, items.len()).map(|d| (name.clone(), d)),
                );
                str_tuples.insert(name.clone(), items);
            } else if let Some(d) = ix.stored_display(*value, (bb.id, i)) {
                candidates.push((name.clone(), d));
            }
        }
    }
    out.ints = ints;
    out.strs = strs;
    out.str_tuples = str_tuples;
    out.bools = bools;
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

/// The entry of [`ModuleConstants::bools`] the module global `name` bound to `value` makes: a
/// `True` / `False` literal, `TYPE_CHECKING` imported from `typing` (or `typing_extensions`), or
/// the module `typing` itself, whose `TYPE_CHECKING` the dotted key holds.
fn bool_constant(ix: &FnIndex<'_>, name: &str, value: VarId) -> Option<(String, bool)> {
    const TYPING: [&str; 2] = ["typing", "typing_extensions"];
    match &ix.source(value)?.kind {
        InstKind::Const {
            literal: ConstLiteral::Bool(b),
            ..
        } => Some((name.to_owned(), *b)),
        // A package's own `typing` module (`from .typing import TYPE_CHECKING`) is not the
        // standard library's.
        InstKind::Import {
            module: Some(module),
            name: imported,
            is_lazy: false,
            relative: false,
            ..
        } if TYPING.contains(&module.as_str()) && imported == "TYPE_CHECKING" => {
            Some((name.to_owned(), false))
        }
        InstKind::Import {
            module: None,
            name: imported,
            is_lazy: false,
            relative: false,
            ..
        } if TYPING.contains(&imported.as_str()) => Some((format!("{name}.TYPE_CHECKING"), false)),
        _ => None,
    }
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
                    .fixed_trip(lp, &mut |_| None, SMALL_MAX)
                    .or_else(|| ix.loop_iterable(lp).and_then(passed_bound))
            {
                bound = if hint.retry_body {
                    LoopBound::RetryPattern
                } else {
                    LoopBound::ConstSmall(n)
                };
            }
            if bound == LoopBound::Unbounded {
                bound = if let Some(n) = ix.counted_trips(lp, SMALL_MAX) {
                    LoopBound::ConstSmall(n)
                } else if hint.repeats && waits_every_iteration(&ix.shape, lp) {
                    if hint.forever {
                        LoopBound::DaemonPolling
                    } else {
                        LoopBound::RetryPattern
                    }
                } else {
                    LoopBound::Unbounded
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
    use super::{CompareOpKind, compare_holds, counted_trips_past, mirrored, range_len};

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

    #[test]
    fn a_mirrored_comparison_holds_for_the_operands_swapped() {
        use CompareOpKind::{Eq, Gt, GtE, Lt, LtE, NotEq};
        use std::cmp::Ordering::{Equal, Greater, Less};
        for op in [Lt, LtE, Gt, GtE, Eq, NotEq] {
            for order in [Less, Equal, Greater] {
                assert_eq!(
                    compare_holds(op, order),
                    compare_holds(mirrored(op), order.reverse()),
                    "{op:?} {order:?}"
                );
            }
        }
    }

    #[test]
    fn counted_trips_past_matches_running_the_loop() {
        use CompareOpKind::{Gt, GtE, Lt, LtE, NotEq};
        let run = |start: i64, step: i64, limit: i64, op: CompareOpKind| {
            let mut value = start;
            for trips in 0..10_000u32 {
                let order = value.cmp(&limit);
                if !compare_holds(op, order)? {
                    return Some(trips);
                }
                value += step;
            }
            None
        };
        for (start, step, limit, op) in [
            (0, 1, 1000, Lt),
            (0, 3, 1000, Lt),
            (5, 1, 0, Lt),
            (0, 2, 999, LtE),
            (1000, -1, 0, Gt),
            (1000, -7, 3, Gt),
            (1000, -1, 0, GtE),
            (0, 4, 1000, NotEq),
            (0, 3, 1000, NotEq),
            (0, -1, 10, NotEq),
            (0, -1, 10, Lt),
        ] {
            assert_eq!(
                counted_trips_past(start, step, limit, op),
                run(start, step, limit, op),
                "{start} {step} {limit} {op:?}"
            );
        }
    }
}
