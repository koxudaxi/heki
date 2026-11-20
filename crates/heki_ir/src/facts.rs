//! Size and lifetime facts both lanes share, decided from the code (Profitability and Advisory
//! noise only, NEVER a Soundness premise): how many items a container can hold for its whole
//! life, how many times a loop runs, what a loop's own test caps, and whether a value outlives
//! one iteration of a loop.
//!
//! - A container is *small* when every object a variable may hold comes from a display (list,
//!   tuple, set, or dict literal without `*` / `**` unpacking) of at most [`SMALL_MAX`] items,
//!   or from a module constant bound to one ([`ModuleConstants::displays`]), and nothing can
//!   grow it: no alias of it is grown in place, stored, captured, returned, or passed to code
//!   other than read-only builtins (a `*x` argument passes its items). Shrinking it (`pop`,
//!   `remove`, `del x[i]`, `x[1:]`) keeps it small; tuples are fixed whatever their uses.
//! - A loop's trip count is fixed by the code when its iterable is small, `range(...)` of
//!   integer constants (module constants included), or `enumerate` / `zip` / `reversed` /
//!   `sorted` / `list` / `map` / `.items()` / ... of such iterables ([`FnIndex::iter_bound`]).
//! - `while len(acc) < N` (also one `and` operand) with a constant `N` of at most
//!   [`LEN_CAP_MAX`] caps `acc` ([`crate::NaturalLoop::len_caps`]).

use crate::{
    BinOpKind, BlockId, BuiltinKind, CompareOpKind, ConstLiteral, DictEntry, FunctionId, Inst,
    InstKind, IrFunction, LenCap, LoopBound, NaturalLoop, Terminator, UnaryOpKind, VarId,
};
use heki_ast::Span;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// The most items a container display, or a loop's fixed trip count, may have to count as
/// small.
pub const SMALL_MAX: u32 = 8;

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
    /// conditions, over every load of the name).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub displays: BTreeMap<String, SmallDisplay>,
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
}

/// The objects a variable may hold ([`FnIndex::origins`]).
#[derive(Default)]
struct Origins {
    len: u32,
    kinds: Vec<BuiltinKind>,
    /// The variables whose definition creates (or loads) one of the objects.
    roots: Vec<VarId>,
}

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
        Self { f, module, defs }
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
        let mut origins = Origins::default();
        if !self.origins(v, &mut origins, &mut Vec::new())
            || origins.roots.is_empty()
            || origins.len > SMALL_MAX
        {
            return None;
        }
        let tuples = origins.kinds.iter().all(|k| *k == BuiltinKind::Tuple);
        (tuples || !self.grows(&origins.roots, &origins.kinds, None)).then_some(origins.len)
    }

    /// The display `value` holds when it is stored by the instruction at `sink` (a module
    /// global or an attribute), small and grown by nothing in this function besides that store.
    pub fn stored_display(&self, value: VarId, sink: (BlockId, usize)) -> Option<SmallDisplay> {
        let mut origins = Origins::default();
        let loads = |o: &Origins| {
            o.roots.iter().any(|r| {
                matches!(
                    self.def_inst(*r).map(|i| &i.kind),
                    Some(InstKind::LoadGlobal { .. })
                )
            })
        };
        if !self.origins(value, &mut origins, &mut Vec::new())
            || origins.roots.is_empty()
            || origins.len > SMALL_MAX
            || loads(&origins)
        {
            return None;
        }
        let tuples = origins.kinds.iter().all(|k| *k == BuiltinKind::Tuple);
        (tuples || !self.grows(&origins.roots, &origins.kinds, Some(sink))).then_some(
            SmallDisplay {
                kinds: origins.kinds,
                len: origins.len,
            },
        )
    }

    /// Nothing in this function grows the container a load (of a global or an attribute)
    /// produces, nor lets other code grow it.
    pub fn load_unchanged(&self, load: VarId) -> bool {
        !self.grows(&[load], &[], None)
    }

    /// Collect the objects `v` may hold into `out`: `false` when one of them is not a small
    /// display, a slice of one, or a small module constant. `stack`: the variables being
    /// resolved (a cycle through phis adds nothing).
    fn origins(&self, v: VarId, out: &mut Origins, stack: &mut Vec<VarId>) -> bool {
        if stack.contains(&v) {
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
                .all(|&(_, x)| self.origins(x, out, stack)),
            Def::Inst(..) => self.inst_origins(v, out, stack),
            Def::Param | Def::None => false,
        };
        stack.pop();
        ok
    }

    fn inst_origins(&self, v: VarId, out: &mut Origins, stack: &mut Vec<VarId>) -> bool {
        let Some(inst) = self.def_inst(v) else {
            return false;
        };
        let add = |out: &mut Origins, kind: BuiltinKind, len: usize| {
            out.len = out.len.max(u32::try_from(len).unwrap_or(u32::MAX));
            if !out.kinds.contains(&kind) {
                out.kinds.push(kind);
                out.kinds.sort_by_key(|k| *k as u8);
            }
            out.roots.push(v);
        };
        match &inst.kind {
            InstKind::Copy { src, .. } => self.origins(*src, out, stack),
            InstKind::AllocBuiltin {
                dst,
                kind: kind @ (BuiltinKind::List | BuiltinKind::Tuple | BuiltinKind::Set),
                elems,
            } if !self.f.starred.iter().any(|(d, _)| d == dst) => {
                add(out, *kind, elems.len());
                true
            }
            InstKind::DictLiteral { entries, .. }
                if entries
                    .iter()
                    .all(|e| matches!(e, DictEntry::KeyValue { .. })) =>
            {
                add(out, BuiltinKind::Dict, entries.len());
                true
            }
            // A slice is a new, smaller container of the same kind.
            InstKind::SubscriptLoad {
                obj,
                is_slice: true,
                ..
            } => {
                out.roots.push(v);
                self.origins(*obj, out, stack)
            }
            InstKind::LoadGlobal {
                name,
                is_lazy_import: false,
                ..
            } if !self.module.dynamic => match self.module.displays.get(name) {
                Some(d) => {
                    out.len = out.len.max(d.len);
                    for k in &d.kinds {
                        if !out.kinds.contains(k) {
                            out.kinds.push(*k);
                        }
                    }
                    out.kinds.sort_by_key(|k| *k as u8);
                    out.roots.push(v);
                    true
                }
                None => false,
            },
            _ => false,
        }
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
                    if let InstKind::Copy { dst, src } = &inst.kind
                        && alias.contains(src)
                    {
                        alias.insert(*dst);
                    }
                }
            }
            if alias.len() == before {
                break;
            }
        }
        if alias.iter().any(|v| {
            f.var_name(*v)
                .is_some_and(|n| f.captured_vars.iter().any(|c| c == n))
        }) {
            return true;
        }
        let list_only = !kinds.is_empty() && kinds.iter().all(|k| *k == BuiltinKind::List);
        let mut ops = Vec::new();
        for bb in &f.blocks {
            for (i, inst) in bb.insts.iter().enumerate() {
                if sink == Some((bb.id, i)) {
                    continue;
                }
                ops.clear();
                self.grown_operands(inst, list_only, &mut ops);
                if ops.iter().any(|v| alias.contains(v)) {
                    return true;
                }
            }
            if let Terminator::Return(Some(v)) | Terminator::Raise(Some(v)) = &bb.terminator
                && alias.contains(v)
            {
                return true;
            }
        }
        false
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
                let item_store = list_only
                    && matches!(
                        self.def_inst(*index).map(|i| &i.kind),
                        Some(InstKind::Const {
                            literal: ConstLiteral::Int(_),
                            ..
                        })
                    );
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
                if !reads_only {
                    out.extend(
                        args.iter()
                            .enumerate()
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
            // Binding a module constant: every load of it was checked (`module_constants`).
            InstKind::StoreGlobal { name, value } => {
                if !self.module.displays.contains_key(name) {
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
                    if let InstKind::Copy { dst, src } = &inst.kind
                        && grown.contains(dst)
                    {
                        grown.insert(*src);
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
    /// container ([`Self::container_bound`]), a short string constant, `range(...)` of integer
    /// constants, or a builtin wrapper of one (`enumerate`, `zip` of at least one, `reversed`,
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
        if depth > 16 {
            return None;
        }
        if let Some(n) = self.container_bound(v) {
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
                        self.iter_bound_depth(plain(0)?, leaf, depth + 1)
                    }
                    "reversed" | "iter" | "list" | "tuple" | "set" | "frozenset" | "dict"
                        if args.len() == 1 && kwargs.is_empty() =>
                    {
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
            InstKind::LoadGlobal { .. } | InstKind::LoadName { .. } | InstKind::AttrLoad { .. } => {
                leaf(inst).filter(|n| *n <= SMALL_MAX)
            }
            _ => None,
        }
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

    /// The `len(x) < N`-shaped comparison `cmp` (`N > len(x)`, `<=`, `>=`): `x` and the bound
    /// `len(x)` stays below while the comparison holds, when `N` is a constant of at most
    /// [`LEN_CAP_MAX`].
    fn len_cap(&self, cmp: &Inst) -> Option<LenCap> {
        let InstKind::Compare { op, lhs, rhs, .. } = &cmp.kind else {
            return None;
        };
        let (measured, bound, inclusive) = match op {
            CompareOpKind::Lt => (*lhs, *rhs, false),
            CompareOpKind::LtE => (*lhs, *rhs, true),
            CompareOpKind::Gt => (*rhs, *lhs, false),
            CompareOpKind::GtE => (*rhs, *lhs, true),
            _ => return None,
        };
        let InstKind::Call {
            dst,
            callee_var,
            callee,
            args,
            kwargs,
            ..
        } = &self.def_inst(measured)?.kind
        else {
            return None;
        };
        let var = match args.as_slice() {
            [x] if kwargs.is_empty()
                && !self.f.is_starred(*dst, 0)
                && self.builtin(*callee_var, callee) == Some("len") =>
            {
                *x
            }
            _ => return None,
        };
        let n = self.const_int(bound)? + i64::from(inclusive);
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
        match self.def_inst(iter)?.kind {
            InstKind::MakeIter { iterable, .. } => Some(iterable),
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
        self.lives_across_seen(lp, v, &mut FxHashSet::default())
    }

    fn lives_across_seen(&self, lp: &NaturalLoop, v: VarId, seen: &mut FxHashSet<VarId>) -> bool {
        match self.def(v) {
            Def::Phi(b, i) => {
                b == lp.header
                    || !lp.body.contains(&b)
                    || (seen.insert(v)
                        && self
                            .phi_incoming(b, i)
                            .iter()
                            .all(|&(_, x)| self.lives_across_seen(lp, x, seen)))
            }
            Def::Inst(b, i) if lp.body.contains(&b) => {
                match &self.f.block(b).insts[i as usize].kind {
                    InstKind::Copy { src: x, .. } | InstKind::AttrLoad { obj: x, .. } => {
                        self.lives_across_seen(lp, *x, seen)
                    }
                    InstKind::SubscriptLoad { obj, index, .. } => {
                        self.lives_across_seen(lp, *obj, seen)
                            && self.lives_across_seen(lp, *index, seen)
                    }
                    InstKind::LoadGlobal { .. }
                    | InstKind::LoadDeref { .. }
                    | InstKind::LoadName { .. }
                    | InstKind::Const { .. } => true,
                    _ => false,
                }
            }
            // A parameter, or defined before the loop.
            _ => true,
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

/// The constants of a module (see [`ModuleConstants`]).
pub(crate) fn module_constants(
    functions: &[IrFunction],
    module_fn: FunctionId,
    last_star_import: Option<u32>,
) -> ModuleConstants {
    let module = &functions[module_fn.0 as usize];
    let mut stores: FxHashMap<&str, u32> = FxHashMap::default();
    let mut deleted: FxHashSet<&str> = FxHashSet::default();
    let mut dynamic = !module.a1_hazards.is_empty();
    for f in functions {
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            match &inst.kind {
                InstKind::StoreGlobal { name, .. } => *stores.entry(name).or_default() += 1,
                // `del NAME` (at module level or after `global NAME`) may unbind it.
                InstKind::Delete { name, .. } => {
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
    // A display constant is small only if no function grows what its loads give.
    let names: FxHashSet<&str> = candidates.iter().map(|(n, _)| n.as_str()).collect();
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

/// Refine every loop's [`LoopBound`] and [`NaturalLoop::len_caps`] once the whole module is
/// lowered: names and module constants bound to small displays, `range` of constant
/// expressions, and builtin wrappers become fixed trip counts.
pub(crate) fn refine_loops(
    functions: &mut [IrFunction],
    module_fn: FunctionId,
    hints: &[(FunctionId, Vec<LoopHint>)],
    last_star_import: Option<u32>,
) -> ModuleConstants {
    let constants = module_constants(functions, module_fn, last_star_import);
    let mut updates: Vec<(usize, usize, LoopBound, Vec<LenCap>)> = Vec::new();
    for (fid, loop_hints) in hints {
        let f = &functions[fid.0 as usize];
        let needs = f
            .loops
            .iter()
            .zip(loop_hints)
            .any(|(lp, h)| lp.bound == LoopBound::Unbounded || !h.cap_tests.is_empty());
        if !needs {
            continue;
        }
        let ix = FnIndex::new(f, &constants);
        for (li, (lp, hint)) in f.loops.iter().zip(loop_hints).enumerate() {
            let mut bound = lp.bound;
            if bound == LoopBound::Unbounded
                && let Some(n) = ix
                    .loop_iterable(lp)
                    .and_then(|it| ix.iter_bound(it, &mut |_| None))
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
                    .filter(|i| hint.cap_tests.contains(&i.span))
                    .filter_map(|i| ix.len_cap(i))
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
    constants
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
