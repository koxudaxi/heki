//! RPO worklist abstract interpretation (docs/ANALYSIS_AND_KB_SPEC.md sections 3.1–3.2).
//!
//! - Allocation-site abstraction: every allocating instruction owns one [`AllocId`]; a site in
//!   a loop summarizes all the objects it creates, so a variable still holding an object from
//!   an earlier iteration stays in that site's alias set (conservative).
//! - Every SSA definition is a strong update: the defined variable leaves all alias / derived
//!   sets before joining the ones of its new value (loops re-define SSA variables).
//! - Phis are evaluated per edge (parallel copies from the predecessor's state).
//! - Exception edges carry the join of the states at every point of the block, and every
//!   source name with several SSA versions present is joined over all of them: a handler may
//!   observe any binding the name had when the exception was raised.
//! - Only the Layer 1 `EffectiveSoundnessKb` is consulted, and only for builtin names that are
//!   not shadowed in the module (Assumption A1(b)); type annotations are never read.

use crate::{
    AllocId, ElemKinds, EscapeWitness, ExactTypeVal, FrameState, KindMask, UniqueState, join_unique,
};
use heki_ast::Span;
use heki_ir::walk::{self, FnShape};
use heki_ir::{
    BinOpKind, BlockId, BuiltinKind, CompareOpKind, ConstLiteral, DictEntry, Inst, InstKind,
    IrFunction, ModuleIr, Terminator, UnaryOpKind, VarId,
};
use heki_kb::{EffectiveSoundnessKb, SoundnessParamEffect};
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

pub(crate) type Allocs = SmallVec<[AllocId; 2]>;

/// Passes over the blocks before the analysis gives up (every candidate is then `Blocked`).
const MAX_PASSES: usize = 64;

/// Builtin names that resolve to the real `builtins` in one module (Assumption A1(b)).
#[derive(Debug, Clone, Default)]
pub struct BuiltinScope {
    all_shadowed: bool,
    /// Module globals may be bound behind the IR (`from m import *`, `exec(...)` at module
    /// level, `globals()`), so not even an imported name keeps its binding.
    dynamic_globals: bool,
    shadowed: FxHashSet<String>,
}

impl BuiltinScope {
    /// `project_patches_builtins`: some module of the project rebinds attributes of `builtins`.
    pub fn for_module(ir: &ModuleIr, project_patches_builtins: bool) -> Self {
        let module_fn = ir.function(ir.module_fn);
        // `exec(...)` / `globals()[...]` at module level may bind any name.
        let mut dynamic_globals = ir.has_star_import || !module_fn.a1_hazards.is_empty();
        let mut shadowed = FxHashSet::default();
        for f in &ir.functions {
            for bb in &f.blocks {
                for inst in &bb.insts {
                    match &inst.kind {
                        InstKind::StoreGlobal { name, .. } => {
                            shadowed.insert(name.clone());
                        }
                        InstKind::Call {
                            callee,
                            callee_var: Some(_),
                            ..
                        } if callee == "globals" => dynamic_globals = true,
                        _ => {}
                    }
                }
            }
        }
        Self {
            all_shadowed: dynamic_globals
                || project_patches_builtins
                || crate::module_patches_builtins(ir),
            dynamic_globals,
            shadowed,
        }
    }

    pub fn resolves(&self, name: &str) -> bool {
        !self.all_shadowed && !self.shadowed.contains(name)
    }

    pub fn dynamic_globals(&self) -> bool {
        self.dynamic_globals
    }
}

/// One use of an SSA variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UseSite {
    Inst(BlockId, usize),
    /// Incoming value of a phi in `block` along the edge from `pred`.
    Phi {
        block: BlockId,
        pred: BlockId,
    },
    Term(BlockId),
}

/// Per-function facts shared by the interpreter, liveness, and the obligations.
pub(crate) struct FnCtx<'a> {
    pub f: &'a IrFunction,
    /// The function's tables: definitions, predecessors, reverse postorder, loops.
    pub shape: &'a FnShape<'a>,
    pub kb: &'a EffectiveSoundnessKb,
    pub builtins: &'a BuiltinScope,
    pub uses: Vec<Vec<UseSite>>,
    site_base: Vec<u32>,
    captured: Vec<bool>,
    name_groups: Vec<Vec<VarId>>,
}

/// Effects of one instruction recorded during the post-fixpoint replay.
#[derive(Debug, Clone, Default)]
pub(crate) struct InstEffects {
    /// Allocation sites whose contents this instruction mutates.
    pub mutated: Allocs,
}

/// Fixpoint result: the state at the entry of each reachable block.
pub(crate) struct Analysis {
    pub block_in: Vec<Option<FrameState>>,
    pub converged: bool,
}

/// Snapshot of one variable's abstract value (parallel phi copies, exception-edge joins).
#[derive(Debug, Clone)]
struct VarValue {
    ty: Option<ExactTypeVal>,
    unique: Option<UniqueState>,
    points_to: Allocs,
    points_to_incomplete: bool,
    derives_from: Allocs,
}

fn sorted_insert<T: Ord + Copy>(v: &mut Vec<T>, x: T) {
    if let Err(pos) = v.binary_search(&x) {
        v.insert(pos, x);
    }
}

fn sorted_remove<T: Ord + Copy>(v: &mut Vec<T>, x: T) {
    if let Ok(pos) = v.binary_search(&x) {
        v.remove(pos);
    }
}

fn allocs_union(a: &mut Allocs, b: &[AllocId]) {
    for &x in b {
        if let Err(pos) = a.binary_search(&x) {
            a.insert(pos, x);
        }
    }
}

pub(crate) fn join_type(a: &ExactTypeVal, b: &ExactTypeVal) -> ExactTypeVal {
    match (a, b) {
        (ExactTypeVal::Exact { ty: ta, .. }, ExactTypeVal::Exact { ty: tb, .. }) if ta == tb => {
            a.clone()
        }
        (ExactTypeVal::Unknown { .. }, _) => a.clone(),
        (_, ExactTypeVal::Unknown { .. }) => b.clone(),
        _ => ExactTypeVal::Unknown {
            reason: "different builtin types merge here".into(),
        },
    }
}

/// Join `other` into `st` (CFG merge).
pub(crate) fn join_into(st: &mut FrameState, other: &FrameState) {
    for (v, t) in &other.var_type {
        match st.var_type.get_mut(v) {
            Some(cur) => *cur = join_type(cur, t),
            None => {
                st.var_type.insert(*v, t.clone());
            }
        }
    }
    for (v, u) in &other.var_unique {
        match st.var_unique.get_mut(v) {
            Some(cur) => *cur = join_unique(cur, u),
            None => {
                st.var_unique.insert(*v, u.clone());
            }
        }
    }
    for (a, members) in &other.alias_sets {
        let cur = st.alias_sets.entry(*a).or_default();
        for &m in members {
            sorted_insert(cur, m);
        }
    }
    for (a, members) in &other.derived_refs {
        let cur = st.derived_refs.entry(*a).or_default();
        for &m in members {
            sorted_insert(cur, m);
        }
    }
    for (v, bases) in &other.derives_from {
        allocs_union(st.derives_from.entry(*v).or_default(), bases);
    }
    for (v, pts) in &other.points_to {
        allocs_union(st.points_to.entry(*v).or_default(), pts);
    }
    for (a, e) in &other.alloc_elems {
        match st.alloc_elems.get_mut(a) {
            Some(cur) => *cur = cur.join(*e),
            None => {
                st.alloc_elems.insert(*a, *e);
            }
        }
    }
    st.points_to_incomplete
        .extend(other.points_to_incomplete.iter().copied());
}

impl FrameState {
    /// Allocation sites `v` may refer to (empty for values the analysis does not track).
    pub fn allocs_of(&self, v: VarId) -> Allocs {
        self.points_to.get(&v).cloned().unwrap_or_default()
    }

    pub fn is_unique(&self, v: VarId) -> bool {
        matches!(self.var_unique.get(&v), Some(UniqueState::Unique { .. }))
    }

    /// Why `v` may be shared (`None`: it is unique, or the analysis holds nothing for it).
    pub fn shared_witness(&self, v: VarId) -> Option<&EscapeWitness> {
        match self.var_unique.get(&v)? {
            UniqueState::Shared { witness } => Some(witness),
            UniqueState::Unique { .. } => None,
        }
    }

    pub fn exact_type(&self, v: VarId) -> Option<BuiltinKind> {
        match self.var_type.get(&v) {
            Some(ExactTypeVal::Exact { ty, .. }) => Some(*ty),
            _ => None,
        }
    }

    /// Strong update: `v` is about to be (re)defined.
    fn kill(&mut self, v: VarId) {
        self.var_type.remove(&v);
        self.var_unique.remove(&v);
        self.points_to_incomplete.remove(&v);
        if let Some(pts) = self.points_to.remove(&v) {
            for a in pts {
                if let Some(m) = self.alias_sets.get_mut(&a) {
                    sorted_remove(m, v);
                }
            }
        }
        if let Some(bases) = self.derives_from.remove(&v) {
            for a in bases {
                if let Some(m) = self.derived_refs.get_mut(&a) {
                    sorted_remove(m, v);
                }
            }
        }
    }

    fn value(&self, v: VarId) -> VarValue {
        VarValue {
            ty: self.var_type.get(&v).cloned(),
            unique: self.var_unique.get(&v).cloned(),
            points_to: self.allocs_of(v),
            points_to_incomplete: self.points_to_incomplete.contains(&v),
            derives_from: self.derives_from.get(&v).cloned().unwrap_or_default(),
        }
    }

    fn assign(&mut self, v: VarId, val: VarValue) {
        self.kill(v);
        if let Some(t) = val.ty {
            self.var_type.insert(v, t);
        }
        if let Some(u) = val.unique {
            self.var_unique.insert(v, u);
        }
        for &a in &val.points_to {
            sorted_insert(self.alias_sets.entry(a).or_default(), v);
        }
        if !val.points_to.is_empty() {
            self.points_to.insert(v, val.points_to);
        }
        if val.points_to_incomplete {
            self.points_to_incomplete.insert(v);
        }
        for &a in &val.derives_from {
            sorted_insert(self.derived_refs.entry(a).or_default(), v);
        }
        if !val.derives_from.is_empty() {
            self.derives_from.insert(v, val.derives_from);
        }
    }

    /// A value defined without an allocation site of this frame comes from outside it.
    fn define(&mut self, v: VarId, ty: ExactTypeVal, unique: UniqueState, points_to: Allocs) {
        self.assign(
            v,
            VarValue {
                ty: Some(ty),
                unique: Some(unique),
                points_to_incomplete: points_to.is_empty(),
                points_to,
                derives_from: Allocs::new(),
            },
        );
    }

    /// `dst` also keeps a reference into everything `bases` refer to (iterators, views).
    fn add_derived(&mut self, dst: VarId, bases: &[VarId]) {
        let mut all = self.derives_from.get(&dst).cloned().unwrap_or_default();
        for &b in bases {
            allocs_union(&mut all, &self.allocs_of(b));
            if let Some(d) = self.derives_from.get(&b) {
                let d = d.clone();
                allocs_union(&mut all, &d);
            }
        }
        for &a in &all {
            sorted_insert(self.derived_refs.entry(a).or_default(), dst);
        }
        if !all.is_empty() {
            self.derives_from.insert(dst, all);
        }
    }

    /// Transitive escape (spec section 4.2): every variable that may share an object with `v`
    /// becomes `Shared`; if `v` is a derived reference, so does everything it derives from.
    /// The code that receives the objects may replace their elements whenever it runs, so their
    /// element kinds become unknown.
    pub(crate) fn mark_escaped(&mut self, v: VarId, witness: EscapeWitness, span: Span) {
        let reached = self.allocs_of(v);
        let bases = self.derives_from.get(&v).cloned().unwrap_or_default();
        for a in reached.iter().chain(&bases) {
            if let Some(e) = self.alloc_elems.get_mut(a) {
                *e = ElemKinds::Unknown;
            }
        }
        let mut direct: Vec<VarId> = Vec::new();
        for a in self.allocs_of(v) {
            if let Some(m) = self.alias_sets.get(&a) {
                direct.extend_from_slice(m);
            }
        }
        let mut derived: Vec<VarId> = Vec::new();
        if let Some(bases) = self.derives_from.get(&v) {
            for a in bases {
                if let Some(m) = self.alias_sets.get(a) {
                    derived.extend_from_slice(m);
                }
            }
        }
        for m in direct {
            if let Some(u) = self.var_unique.get_mut(&m) {
                *u = UniqueState::Shared {
                    witness: witness.clone(),
                };
            }
        }
        for m in derived {
            if let Some(u) = self.var_unique.get_mut(&m) {
                *u = UniqueState::Shared {
                    witness: EscapeWitness::DerivedRefEscapedAt(span),
                };
            }
        }
        if let Some(u) = self.var_unique.get_mut(&v) {
            *u = UniqueState::Shared { witness };
        }
    }
}

/// `seq[index]` reads one element: the index is an `int` or `bool`, not a `slice` object
/// (`seq[s]` with `s = slice(0, 1)` is a new sequence).
fn is_scalar_index(st: &FrameState, index: VarId) -> bool {
    matches!(
        st.exact_type(index),
        Some(BuiltinKind::Int | BuiltinKind::Bool)
    )
}

/// Containers whose element kinds are tracked.
fn tracks_elems(kind: BuiltinKind) -> bool {
    matches!(
        kind,
        BuiltinKind::List | BuiltinKind::Tuple | BuiltinKind::Set
    )
}

fn is_mutable(kind: BuiltinKind) -> bool {
    matches!(
        kind,
        BuiltinKind::List | BuiltinKind::Dict | BuiltinKind::Set
    )
}

/// Result kind of a binary operator on two exact builtin operands, when no user code runs
/// and the result is a fresh object (`None`: unknown result or user code may run).
fn binop_result(op: BinOpKind, l: BuiltinKind, r: BuiltinKind) -> Option<BuiltinKind> {
    use BinOpKind::*;
    use BuiltinKind::*;
    let numeric = |k| matches!(k, Int | Float | Bool);
    match op {
        Add => match (l, r) {
            (List, List) => Some(List),
            (Tuple, Tuple) => Some(Tuple),
            (Str, Str) => Some(Str),
            (Bytes, Bytes) => Some(Bytes),
            (a, b) if numeric(a) && numeric(b) => {
                Some(if a == Float || b == Float { Float } else { Int })
            }
            _ => None,
        },
        Sub | Mult | FloorDiv | Mod => match (l, r) {
            (a, b) if numeric(a) && numeric(b) => {
                Some(if a == Float || b == Float { Float } else { Int })
            }
            (Set, Set) if op == Sub => Some(Set),
            _ => None,
        },
        BitOr | BitAnd | BitXor => match (l, r) {
            (Set, Set) => Some(Set),
            (Dict, Dict) if op == BitOr => Some(Dict),
            (Int | Bool, Int | Bool) => Some(Int),
            _ => None,
        },
        _ => None,
    }
}

impl<'a> FnCtx<'a> {
    pub fn new(
        shape: &'a FnShape<'a>,
        kb: &'a EffectiveSoundnessKb,
        builtins: &'a BuiltinScope,
    ) -> Self {
        let f = shape.f;
        let nvars = f.var_names.len();
        let mut uses: Vec<Vec<UseSite>> = vec![Vec::new(); nvars];
        let mut site_base = Vec::with_capacity(f.blocks.len());
        let mut next = 0u32;
        for bb in &f.blocks {
            site_base.push(next);
            next += bb.insts.len() as u32;
            for phi in &bb.phis {
                for &(pred, v) in &phi.incoming {
                    if let Some(u) = uses.get_mut(v.0 as usize) {
                        u.push(UseSite::Phi { block: bb.id, pred });
                    }
                }
            }
            for (i, inst) in bb.insts.iter().enumerate() {
                for op in inst.kind.operands() {
                    if let Some(u) = uses.get_mut(op.0 as usize) {
                        u.push(UseSite::Inst(bb.id, i));
                    }
                }
            }
            if let Some(v) = term_use(&bb.terminator)
                && let Some(u) = uses.get_mut(v.0 as usize)
            {
                u.push(UseSite::Term(bb.id));
            }
        }
        let captured_names: FxHashSet<&str> = f.captured_vars.iter().map(String::as_str).collect();
        let mut captured = vec![false; nvars];
        let mut by_name: FxHashMap<&str, Vec<VarId>> = FxHashMap::default();
        for (i, name) in f.var_names.iter().enumerate() {
            if let Some(n) = name.as_deref() {
                captured[i] = captured_names.contains(n);
                by_name.entry(n).or_default().push(VarId(i as u32));
            }
        }
        let name_groups = by_name.into_values().filter(|g| g.len() > 1).collect();
        Self {
            f,
            shape,
            kb,
            builtins,
            uses,
            site_base,
            captured,
            name_groups,
        }
    }

    /// A nested function captures `v`'s name, so code it runs may rebind it (`nonlocal`).
    pub fn is_captured(&self, v: VarId) -> bool {
        self.captured.get(v.0 as usize).copied().unwrap_or(false)
    }

    pub fn uses(&self, v: VarId) -> &[UseSite] {
        self.uses
            .get(v.0 as usize)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    fn site(&self, bb: BlockId, idx: usize) -> AllocId {
        AllocId(self.site_base[bb.0 as usize] + idx as u32)
    }

    /// Position of the instruction that owns allocation site `a` (the inverse of `site`).
    pub fn site_location(&self, a: AllocId) -> (BlockId, usize) {
        // An empty block shares its base with the next block, so the instruction is in the
        // last block whose base is at most `a` (`site_base[0] == 0`: there is one).
        let bi = self.site_base.partition_point(|&base| base <= a.0) - 1;
        (BlockId(bi as u32), (a.0 - self.site_base[bi]) as usize)
    }

    /// Builtin function called by `Call { callee_var, callee }`, if it is the real builtin.
    pub fn builtin_callee(&self, callee_var: Option<VarId>, callee: &str) -> Option<&'a str> {
        let cv = callee_var?;
        let inst = self.shape.inst(cv)?;
        match &inst.kind {
            InstKind::LoadGlobal {
                name,
                is_lazy_import: false,
                ..
            } if name == callee && !name.contains('.') && self.builtins.resolves(name) => {
                Some(name.as_str())
            }
            _ => None,
        }
    }

    fn entry_state(&self) -> FrameState {
        let mut st = FrameState::default();
        for p in &self.f.params {
            st.define(
                p.var,
                ExactTypeVal::Unknown {
                    reason: "parameter".into(),
                },
                UniqueState::Shared {
                    witness: EscapeWitness::ParameterInput,
                },
                Allocs::new(),
            );
        }
        st
    }

    /// Element kinds a value may have, when known (`Some(EMPTY)`: it never has a value).
    pub fn possible_kinds(&self, st: &FrameState, v: VarId) -> Option<KindMask> {
        self.possible_kinds_depth(st, v, 0)
    }

    fn possible_kinds_depth(&self, st: &FrameState, v: VarId, depth: u32) -> Option<KindMask> {
        if let Some(k) = st.exact_type(v) {
            return Some(KindMask::of(k));
        }
        if depth > 8 {
            return None;
        }
        let inst = self.shape.inst(v)?;
        match &inst.kind {
            InstKind::Copy { src, .. } => self.possible_kinds_depth(st, *src, depth + 1),
            InstKind::IterNext { iter, .. } => self.yielded_kinds(st, *iter),
            InstKind::SubscriptLoad {
                obj,
                index,
                is_slice: false,
                ..
            } if matches!(
                st.exact_type(*obj),
                Some(BuiltinKind::List | BuiltinKind::Tuple)
            ) && is_scalar_index(st, *index) =>
            {
                self.elems_of(st, *obj)
            }
            _ => None,
        }
    }

    /// What `next(it)` may produce. Only an iterator made from an object by `iter()` (a `for`
    /// loop, a comprehension) yields that object's elements: an iterator *retaining* an object
    /// (`map(f, xs)`, a generator expression over `xs`) yields whatever its code produces.
    fn yielded_kinds(&self, st: &FrameState, it: VarId) -> Option<KindMask> {
        // The IR makes every loop's iterator with `MakeIter`, whose one operand is the
        // iterated object.
        let made = self.shape.inst(it)?;
        let iterable =
            matches!(made.kind, InstKind::MakeIter { .. }).then(|| made.kind.operands()[0])?;
        match self.iterable_elems(st, iterable) {
            ElemKinds::Known(m) => Some(m),
            ElemKinds::Unknown => None,
        }
    }

    /// Element kinds of every object `v` may hold (`None` when one of them is unknown).
    pub fn elems_of(&self, st: &FrameState, v: VarId) -> Option<KindMask> {
        if st.points_to_incomplete.contains(&v) {
            return None;
        }
        Self::elems_of_bases(st, &st.allocs_of(v))
    }

    /// Union of the element kinds of `bases` (`None` if any is unknown, or if there is none:
    /// nothing is known of a value without an allocation site).
    fn elems_of_bases(st: &FrameState, bases: &[AllocId]) -> Option<KindMask> {
        let mut mask = None;
        for a in bases {
            match st.alloc_elems.get(a) {
                Some(ElemKinds::Known(m)) => {
                    mask = Some(mask.unwrap_or(KindMask::EMPTY).union(*m));
                }
                _ => return None,
            }
        }
        mask
    }

    fn value_kinds(&self, st: &FrameState, v: VarId) -> ElemKinds {
        self.possible_kinds(st, v)
            .map_or(ElemKinds::Unknown, ElemKinds::Known)
    }

    /// Element kinds of the iterable `v` (for `list(v)`, `x.extend(v)`, ...).
    fn iterable_elems(&self, st: &FrameState, v: VarId) -> ElemKinds {
        match st.exact_type(v) {
            Some(BuiltinKind::List | BuiltinKind::Tuple | BuiltinKind::Set) => self
                .elems_of(st, v)
                .map_or(ElemKinds::Unknown, ElemKinds::Known),
            Some(BuiltinKind::Str) => ElemKinds::Known(KindMask::of(BuiltinKind::Str)),
            Some(BuiltinKind::Bytes) => ElemKinds::Known(KindMask::of(BuiltinKind::Int)),
            _ => {
                // A range object (fresh alloc with known `int` elements), possibly merged with
                // other tracked objects whose elements are known.
                if st.var_type.get(&v).is_some_and(
                    |t| matches!(t, ExactTypeVal::Unknown { reason } if reason == RANGE_REASON),
                ) {
                    return self
                        .elems_of(st, v)
                        .map_or(ElemKinds::Unknown, ElemKinds::Known);
                }
                ElemKinds::Unknown
            }
        }
    }

    /// Iterating `v`, or asking its length, runs no user code: an exact builtin container,
    /// string, or `range` object.
    pub fn iterates_without_user_code(&self, st: &FrameState, v: VarId) -> bool {
        let range = !st.points_to_incomplete.contains(&v)
            && st.var_type.get(&v).is_some_and(
                |t| matches!(t, ExactTypeVal::Unknown { reason } if reason == RANGE_REASON),
            );
        range
            || matches!(
                st.exact_type(v),
                Some(
                    BuiltinKind::List
                        | BuiltinKind::Tuple
                        | BuiltinKind::Set
                        | BuiltinKind::Dict
                        | BuiltinKind::Str
                        | BuiltinKind::Bytes
                )
            )
    }

    /// Neither an exact builtin, a tracked allocation, nor a derived reference of one.
    fn is_untracked(st: &FrameState, v: VarId) -> bool {
        st.exact_type(v).is_none()
            && st.allocs_of(v).is_empty()
            && st.derives_from.get(&v).is_none_or(|d| d.is_empty())
    }

    fn record_mutation(st: &FrameState, v: VarId, eff: &mut InstEffects) {
        allocs_union(&mut eff.mutated, &st.allocs_of(v));
        if let Some(d) = st.derives_from.get(&v) {
            allocs_union(&mut eff.mutated, d);
        }
    }

    fn add_elems(st: &mut FrameState, container: VarId, kinds: ElemKinds) {
        for a in st.allocs_of(container) {
            if let Some(cur) = st.alloc_elems.get_mut(&a) {
                *cur = cur.join(kinds);
            }
        }
    }

    fn after_def(&self, st: &mut FrameState, dst: VarId, span: Span) {
        if self.is_captured(dst) {
            st.mark_escaped(dst, EscapeWitness::CapturedByClosure, span);
        }
    }

    fn fresh(
        &self,
        st: &mut FrameState,
        dst: VarId,
        kind: BuiltinKind,
        site: AllocId,
        span: Span,
        elems: ElemKinds,
    ) {
        st.define(
            dst,
            ExactTypeVal::Exact {
                ty: kind,
                origin: span,
            },
            UniqueState::Unique {
                alloc_ids: SmallVec::from_slice(&[site]),
            },
            SmallVec::from_slice(&[site]),
        );
        let elems = if tracks_elems(kind) {
            elems
        } else {
            ElemKinds::Unknown
        };
        // A site in a loop summarizes every object it creates: while a variable or an iterator
        // still refers to an earlier one, the site's kinds must keep describing that one too.
        let earlier_object_referenced = st
            .alias_sets
            .get(&site)
            .is_some_and(|m| m.iter().any(|&x| x != dst))
            || st.derived_refs.get(&site).is_some_and(|d| !d.is_empty());
        let elems = match st.alloc_elems.get(&site) {
            Some(earlier) if earlier_object_referenced => earlier.join(elems),
            _ => elems,
        };
        st.alloc_elems.insert(site, elems);
    }

    fn opaque(st: &mut FrameState, dst: VarId, reason: &str, span: Span) {
        st.define(
            dst,
            ExactTypeVal::Unknown {
                reason: reason.to_owned(),
            },
            UniqueState::Shared {
                witness: EscapeWitness::ExternalValue(span),
            },
            Allocs::new(),
        );
    }

    fn escape_all(st: &mut FrameState, vars: impl IntoIterator<Item = VarId>, span: Span) {
        for v in vars {
            st.mark_escaped(v, EscapeWitness::PassedToUnknownCall(span), span);
        }
    }

    /// Apply a Layer 1 entry to the arguments of a call, each with its parameter position (a
    /// method's receiver is position 0).
    fn apply_param_effects(
        st: &mut FrameState,
        effects: &[SoundnessParamEffect],
        args: &[(usize, VarId)],
        span: Span,
        eff: &mut InstEffects,
        retained: &mut Vec<VarId>,
    ) {
        for &(i, a) in args {
            let e = effects
                .get(i)
                .or_else(|| effects.last())
                .copied()
                .unwrap_or(SoundnessParamEffect::Escaped);
            match e {
                SoundnessParamEffect::ReadOnly => {}
                SoundnessParamEffect::Mutated => Self::record_mutation(st, a, eff),
                SoundnessParamEffect::RetainedInReturn => retained.push(a),
                SoundnessParamEffect::Escaped => {
                    st.mark_escaped(a, EscapeWitness::StoredInContainerOrAttr(span), span)
                }
            }
        }
    }

    /// Transfer one instruction.
    pub(crate) fn step(
        &self,
        st: &mut FrameState,
        bb: BlockId,
        idx: usize,
        inst: &Inst,
        eff: &mut InstEffects,
    ) {
        let span = inst.span;
        let site = self.site(bb, idx);
        match &inst.kind {
            InstKind::Const { dst, literal } => {
                let kind = match literal {
                    ConstLiteral::Int(_) => Some(BuiltinKind::Int),
                    ConstLiteral::Float(_) => Some(BuiltinKind::Float),
                    ConstLiteral::Str(_) => Some(BuiltinKind::Str),
                    ConstLiteral::Bytes => Some(BuiltinKind::Bytes),
                    ConstLiteral::Bool(_) => Some(BuiltinKind::Bool),
                    _ => None,
                };
                match kind {
                    Some(k) => self.fresh(st, *dst, k, site, span, ElemKinds::Unknown),
                    None => Self::opaque(st, *dst, "constant", span),
                }
                self.after_def(st, *dst, span);
            }
            InstKind::AllocBuiltin { dst, kind, elems } => {
                let mut kinds = ElemKinds::Known(KindMask::EMPTY);
                for &e in elems {
                    kinds = kinds.join(self.value_kinds(st, e));
                }
                for &e in elems {
                    st.mark_escaped(e, EscapeWitness::StoredInContainerOrAttr(span), span);
                }
                self.fresh(st, *dst, *kind, site, span, kinds);
                self.after_def(st, *dst, span);
            }
            InstKind::DictLiteral { dst, entries } => {
                for e in entries {
                    match e {
                        DictEntry::KeyValue { key, value } => {
                            st.mark_escaped(
                                *key,
                                EscapeWitness::StoredInContainerOrAttr(span),
                                span,
                            );
                            st.mark_escaped(
                                *value,
                                EscapeWitness::StoredInContainerOrAttr(span),
                                span,
                            );
                        }
                        DictEntry::Unpack { dict } => {
                            if st.exact_type(*dict) != Some(BuiltinKind::Dict) {
                                // Unknown mapping: `keys()` / `__getitem__` may run user code.
                                Self::escape_all(st, [*dict], span);
                            }
                        }
                    }
                }
                self.fresh(st, *dst, BuiltinKind::Dict, site, span, ElemKinds::Unknown);
                self.after_def(st, *dst, span);
            }
            InstKind::LoadGlobal { dst, .. } => {
                Self::opaque(st, *dst, "global", span);
                self.after_def(st, *dst, span);
            }
            InstKind::LoadDeref { dst, .. } => {
                Self::opaque(st, *dst, "free variable", span);
                self.after_def(st, *dst, span);
            }
            InstKind::LoadName { dst, .. } => {
                Self::opaque(st, *dst, "possibly unbound name", span);
                self.after_def(st, *dst, span);
            }
            InstKind::Import { dst, .. } => {
                Self::opaque(st, *dst, "imported module or name", span);
                self.after_def(st, *dst, span);
            }
            InstKind::CatchException { dst: Some(dst), .. } => {
                Self::opaque(st, *dst, "caught exception", span);
                self.after_def(st, *dst, span);
            }
            InstKind::CatchException { dst: None, .. } => {}
            InstKind::StoreGlobal { value, .. } | InstKind::StoreDeref { value, .. } => {
                st.mark_escaped(*value, EscapeWitness::StoredInContainerOrAttr(span), span);
            }
            InstKind::Copy { dst, src } => {
                let val = st.value(*src);
                st.assign(*dst, val);
                self.after_def(st, *dst, span);
            }
            InstKind::Delete { var, .. } => {
                if let Some(v) = var {
                    st.kill(*v);
                }
            }
            InstKind::BinOp {
                dst,
                op,
                lhs,
                rhs,
                in_place,
            } => self.step_binop(st, *dst, *op, *lhs, *rhs, *in_place, site, span, eff),
            InstKind::UnaryOp { dst, op, operand } => {
                match (op, st.exact_type(*operand)) {
                    (UnaryOpKind::Not, Some(_)) => {
                        self.fresh(st, *dst, BuiltinKind::Bool, site, span, ElemKinds::Unknown)
                    }
                    // `not x` always produces an exact `bool`; only `x.__bool__` / `__len__`
                    // may run user code.
                    (UnaryOpKind::Not, None) => {
                        Self::escape_all(st, [*operand], span);
                        self.fresh(st, *dst, BuiltinKind::Bool, site, span, ElemKinds::Unknown)
                    }
                    (
                        UnaryOpKind::USub | UnaryOpKind::UAdd | UnaryOpKind::Invert,
                        Some(k @ (BuiltinKind::Int | BuiltinKind::Float)),
                    ) => self.fresh(st, *dst, k, site, span, ElemKinds::Unknown),
                    (
                        UnaryOpKind::USub | UnaryOpKind::UAdd | UnaryOpKind::Invert,
                        Some(BuiltinKind::Bool),
                    ) => self.fresh(st, *dst, BuiltinKind::Int, site, span, ElemKinds::Unknown),
                    _ => {
                        Self::escape_all(st, [*operand], span);
                        Self::opaque(st, *dst, "result of a unary operator", span);
                    }
                }
                self.after_def(st, *dst, span);
            }
            InstKind::Compare { dst, op, lhs, rhs } => {
                let (lt, rt) = (st.exact_type(*lhs), st.exact_type(*rhs));
                let plain = |k: Option<BuiltinKind>| {
                    matches!(
                        k,
                        Some(
                            BuiltinKind::Int
                                | BuiltinKind::Float
                                | BuiltinKind::Bool
                                | BuiltinKind::Str
                                | BuiltinKind::Bytes
                        )
                    )
                };
                let bool_result = match op {
                    CompareOpKind::Is | CompareOpKind::IsNot => true,
                    CompareOpKind::In | CompareOpKind::NotIn => {
                        if rt.is_none() {
                            // `rhs.__contains__(lhs)` is user code.
                            Self::escape_all(st, [*lhs, *rhs], span);
                        } else if !plain(lt)
                            && !self
                                .elems_of(st, *rhs)
                                .is_some_and(|m| m.is_subset_of(KindMask::hash_consistent()))
                        {
                            // Elements' `__eq__` may run user code that receives the probe.
                            Self::escape_all(st, [*lhs], span);
                        }
                        // `in` always produces a `bool`.
                        true
                    }
                    _ => {
                        let known = |k: Option<BuiltinKind>| k.is_some();
                        if plain(lt) && plain(rt) {
                            true
                        } else {
                            if !(known(lt) && known(rt)) {
                                Self::escape_all(st, [*lhs, *rhs], span);
                            }
                            false
                        }
                    }
                };
                if bool_result {
                    self.fresh(st, *dst, BuiltinKind::Bool, site, span, ElemKinds::Unknown);
                } else {
                    Self::opaque(st, *dst, "result of a comparison", span);
                }
                self.after_def(st, *dst, span);
            }
            InstKind::AttrLoad { dst, obj, .. } => {
                // A bound method or attribute may keep a reference to `obj`.
                Self::escape_all(st, [*obj], span);
                Self::opaque(st, *dst, "attribute", span);
                self.after_def(st, *dst, span);
            }
            InstKind::AttrStore { obj, value, .. } => {
                st.mark_escaped(*value, EscapeWitness::StoredInContainerOrAttr(span), span);
                Self::escape_all(st, [*obj], span);
            }
            InstKind::AttrDelete { obj, .. } => Self::escape_all(st, [*obj], span),
            InstKind::SubscriptLoad {
                dst,
                obj,
                index,
                is_slice,
                ..
            } => {
                let ot = st.exact_type(*obj);
                match (ot, is_slice) {
                    (
                        Some(
                            k @ (BuiltinKind::List
                            | BuiltinKind::Tuple
                            | BuiltinKind::Str
                            | BuiltinKind::Bytes),
                        ),
                        true,
                    ) => {
                        let elems = self
                            .elems_of(st, *obj)
                            .map_or(ElemKinds::Unknown, ElemKinds::Known);
                        self.fresh(st, *dst, k, site, span, elems);
                    }
                    (Some(BuiltinKind::Str), false) => {
                        self.fresh(st, *dst, BuiltinKind::Str, site, span, ElemKinds::Unknown)
                    }
                    (Some(BuiltinKind::Bytes), false) if is_scalar_index(st, *index) => {
                        self.fresh(st, *dst, BuiltinKind::Int, site, span, ElemKinds::Unknown)
                    }
                    (Some(BuiltinKind::Bytes), false) => {
                        Self::opaque(st, *dst, "item or slice of `bytes`", span)
                    }
                    (
                        Some(k @ (BuiltinKind::List | BuiltinKind::Tuple | BuiltinKind::Dict)),
                        false,
                    ) => {
                        let element = k == BuiltinKind::Dict || is_scalar_index(st, *index);
                        let single = self
                            .elems_of(st, *obj)
                            .filter(|_| element)
                            .and_then(KindMask::single)
                            .filter(|k| !is_mutable(*k));
                        st.define(
                            *dst,
                            match single {
                                Some(k) => ExactTypeVal::Exact {
                                    ty: k,
                                    origin: span,
                                },
                                None => ExactTypeVal::Unknown {
                                    reason: "container element".into(),
                                },
                            },
                            UniqueState::Shared {
                                witness: EscapeWitness::StoredInContainerOrAttr(span),
                            },
                            Allocs::new(),
                        );
                    }
                    _ => {
                        Self::escape_all(st, [*obj, *index], span);
                        Self::opaque(st, *dst, "subscript of an unknown object", span);
                    }
                }
                self.after_def(st, *dst, span);
            }
            InstKind::SubscriptStore {
                obj, index, value, ..
            } => {
                st.mark_escaped(*value, EscapeWitness::StoredInContainerOrAttr(span), span);
                match st.exact_type(*obj) {
                    Some(BuiltinKind::List) => {
                        let kinds = self.value_kinds(st, *value);
                        Self::record_mutation(st, *obj, eff);
                        Self::add_elems(st, *obj, kinds);
                    }
                    Some(BuiltinKind::Dict) => {
                        st.mark_escaped(*index, EscapeWitness::StoredInContainerOrAttr(span), span);
                        Self::record_mutation(st, *obj, eff);
                    }
                    _ => {
                        Self::record_mutation(st, *obj, eff);
                        Self::escape_all(st, [*obj, *index], span);
                    }
                }
            }
            InstKind::SubscriptDelete { obj, index, .. } => {
                Self::record_mutation(st, *obj, eff);
                if !matches!(
                    st.exact_type(*obj),
                    Some(BuiltinKind::List | BuiltinKind::Dict)
                ) {
                    Self::escape_all(st, [*obj, *index], span);
                }
            }
            InstKind::MakeIter { dst, iterable, .. } => {
                if Self::is_untracked(st, *iterable) {
                    // `iter(x)` on an unknown object runs its `__iter__`.
                    Self::escape_all(st, [*iterable], span);
                }
                Self::opaque(st, *dst, "iterator", span);
                st.add_derived(*dst, &[*iterable]);
                self.after_def(st, *dst, span);
            }
            InstKind::IterNext { dst, iter, .. } => {
                if st.derives_from.get(iter).is_none_or(|d| d.is_empty()) {
                    Self::escape_all(st, [*iter], span);
                }
                let single = self
                    .yielded_kinds(st, *iter)
                    .and_then(KindMask::single)
                    .filter(|k| !is_mutable(*k));
                st.define(
                    *dst,
                    match single {
                        Some(k) => ExactTypeVal::Exact {
                            ty: k,
                            origin: span,
                        },
                        None => ExactTypeVal::Unknown {
                            reason: "loop element".into(),
                        },
                    },
                    UniqueState::Shared {
                        witness: EscapeWitness::StoredInContainerOrAttr(span),
                    },
                    Allocs::new(),
                );
                self.after_def(st, *dst, span);
            }
            InstKind::MakeGenExpr {
                dst,
                outer_iter,
                captured,
                ..
            } => {
                for &c in captured {
                    st.mark_escaped(c, EscapeWitness::PassedToUnknownCall(span), span);
                }
                if Self::is_untracked(st, *outer_iter) {
                    Self::escape_all(st, [*outer_iter], span);
                }
                Self::opaque(st, *dst, "generator", span);
                st.add_derived(*dst, &[*outer_iter]);
                self.after_def(st, *dst, span);
            }
            InstKind::MakeClosure { dst, .. } => {
                // The function object keeps every value its creation evaluates: captured
                // variables, and default values when the IR passes them.
                for c in inst.kind.operands() {
                    st.mark_escaped(c, EscapeWitness::CapturedByClosure, span);
                }
                Self::opaque(st, *dst, "function", span);
                self.after_def(st, *dst, span);
            }
            InstKind::Call {
                dst,
                callee_var,
                callee,
                args,
                kwargs,
                ..
            } => self.step_call(st, *dst, *callee_var, callee, args, kwargs, site, span, eff),
            InstKind::MethodCall {
                dst,
                recv,
                method,
                args,
                kwargs,
                ..
            } => self.step_method(st, *dst, *recv, method, args, kwargs, site, span, eff),
            InstKind::Await { dst, value } => {
                Self::escape_all(st, [*value], span);
                Self::opaque(st, *dst, "awaited value", span);
                self.after_def(st, *dst, span);
            }
            InstKind::Yield { dst, value } => {
                if let Some(v) = value {
                    st.mark_escaped(*v, EscapeWitness::StoredInContainerOrAttr(span), span);
                }
                Self::opaque(st, *dst, "value sent into the generator", span);
                self.after_def(st, *dst, span);
            }
            InstKind::YieldFrom { dst, value } => {
                Self::escape_all(st, [*value], span);
                Self::opaque(st, *dst, "result of `yield from`", span);
                self.after_def(st, *dst, span);
            }
            InstKind::EnterWith { dst, mgr, .. } => {
                Self::escape_all(st, [*mgr], span);
                Self::opaque(st, *dst, "context manager value", span);
                self.after_def(st, *dst, span);
            }
            InstKind::ExitWith { mgr, .. } => Self::escape_all(st, [*mgr], span),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn step_binop(
        &self,
        st: &mut FrameState,
        dst: VarId,
        op: BinOpKind,
        lhs: VarId,
        rhs: VarId,
        in_place: bool,
        site: AllocId,
        span: Span,
        eff: &mut InstEffects,
    ) {
        let (lt, rt) = (st.exact_type(lhs), st.exact_type(rhs));
        // `list += iterable` / `set |= set` / `dict |= mapping`: in-place update of `lhs`.
        let in_place_mutation = in_place
            && match (lt, op) {
                (Some(BuiltinKind::List), BinOpKind::Add) => true,
                (Some(BuiltinKind::List), BinOpKind::Mult) => rt == Some(BuiltinKind::Int),
                (
                    Some(BuiltinKind::Set),
                    BinOpKind::BitOr | BinOpKind::BitAnd | BinOpKind::BitXor | BinOpKind::Sub,
                ) => rt == Some(BuiltinKind::Set),
                (Some(BuiltinKind::Dict), BinOpKind::BitOr) => rt == Some(BuiltinKind::Dict),
                _ => false,
            };
        if in_place_mutation {
            if lt == Some(BuiltinKind::List) && op == BinOpKind::Add {
                let kinds = self.iterable_elems(st, rhs);
                if rt.is_none() {
                    // `list.__iadd__` iterates any iterable (user `__iter__` may run).
                    Self::escape_all(st, [rhs], span);
                }
                Self::add_elems(st, lhs, kinds);
            } else if lt == Some(BuiltinKind::Set) {
                let kinds = self.iterable_elems(st, rhs);
                Self::add_elems(st, lhs, kinds);
            }
            Self::record_mutation(st, lhs, eff);
            let val = st.value(lhs);
            st.assign(dst, val);
            self.after_def(st, dst, span);
            return;
        }
        match (lt, rt) {
            (Some(l), Some(r)) if binop_result(op, l, r).is_some() => {
                let kind = binop_result(op, l, r).unwrap_or(BuiltinKind::Int);
                let elems = if tracks_elems(kind) {
                    let mut e = self
                        .elems_of(st, lhs)
                        .map_or(ElemKinds::Unknown, ElemKinds::Known);
                    e = e.join(
                        self.elems_of(st, rhs)
                            .map_or(ElemKinds::Unknown, ElemKinds::Known),
                    );
                    e
                } else {
                    ElemKinds::Unknown
                };
                self.fresh(st, dst, kind, site, span, elems);
            }
            _ => {
                Self::escape_all(st, [lhs, rhs], span);
                Self::opaque(st, dst, "result of a binary operator", span);
            }
        }
        self.after_def(st, dst, span);
    }

    #[allow(clippy::too_many_arguments)]
    fn step_call(
        &self,
        st: &mut FrameState,
        dst: VarId,
        callee_var: Option<VarId>,
        callee: &str,
        args: &[VarId],
        kwargs: &[(Option<String>, VarId)],
        site: AllocId,
        span: Span,
        eff: &mut InstEffects,
    ) {
        // `f(*xs)` passes the elements of `xs`: the Layer 1 positions no longer line up.
        let unpacked = (0..args.len()).any(|i| self.f.is_starred(dst, i));
        let entry = self
            .builtin_callee(callee_var, callee)
            .filter(|_| !unpacked)
            .and_then(|name| {
                self.kb
                    .lookup(&format!("builtins.{name}"))
                    .map(|e| (name, e))
            });
        let Some((name, entry)) = entry else {
            Self::escape_all(
                st,
                callee_var
                    .into_iter()
                    .chain(args.iter().copied())
                    .chain(kwargs.iter().map(|(_, v)| *v)),
                span,
            );
            Self::opaque(st, dst, &format!("result of `{callee}`"), span);
            self.after_def(st, dst, span);
            return;
        };
        for (_, v) in kwargs {
            st.mark_escaped(*v, EscapeWitness::PassedToUnknownCall(span), span);
        }
        let mut retained = Vec::new();
        let indexed: Vec<(usize, VarId)> = args.iter().copied().enumerate().collect();
        Self::apply_param_effects(st, &entry.param_effects, &indexed, span, eff, &mut retained);
        let ret_kind = entry
            .return_exact_type
            .as_deref()
            .and_then(BuiltinKind::from_name);
        if let Some(kind) = ret_kind {
            let elems = match (kind, args.first()) {
                (BuiltinKind::List | BuiltinKind::Tuple | BuiltinKind::Set, Some(&a))
                    if kwargs.is_empty() =>
                {
                    self.iterable_elems(st, a)
                }
                (_, None) => ElemKinds::Known(KindMask::EMPTY),
                _ => ElemKinds::Unknown,
            };
            self.fresh(st, dst, kind, site, span, elems);
        } else if name == "range" {
            st.define(
                dst,
                ExactTypeVal::Unknown {
                    reason: RANGE_REASON.into(),
                },
                UniqueState::Unique {
                    alloc_ids: SmallVec::from_slice(&[site]),
                },
                SmallVec::from_slice(&[site]),
            );
            st.alloc_elems
                .insert(site, ElemKinds::Known(KindMask::of(BuiltinKind::Int)));
        } else {
            Self::opaque(st, dst, &format!("result of `{name}`"), span);
        }
        if !retained.is_empty() {
            st.add_derived(dst, &retained);
        }
        self.after_def(st, dst, span);
    }

    #[allow(clippy::too_many_arguments)]
    fn step_method(
        &self,
        st: &mut FrameState,
        dst: VarId,
        recv: VarId,
        method: &str,
        args: &[VarId],
        kwargs: &[(Option<String>, VarId)],
        site: AllocId,
        span: Span,
        eff: &mut InstEffects,
    ) {
        let unpacked = (0..args.len()).any(|i| self.f.is_starred(dst, i));
        let entry = st
            .exact_type(recv)
            .filter(|_| !unpacked)
            .and_then(|k| self.kb.lookup(&format!("builtins.{}.{method}", k.name())));
        let Some(entry) = entry else {
            Self::escape_all(
                st,
                std::iter::once(recv)
                    .chain(args.iter().copied())
                    .chain(kwargs.iter().map(|(_, v)| *v)),
                span,
            );
            Self::opaque(st, dst, &format!("result of `.{method}()`"), span);
            self.after_def(st, dst, span);
            return;
        };
        for (_, v) in kwargs {
            st.mark_escaped(*v, EscapeWitness::PassedToUnknownCall(span), span);
        }
        // Element kinds added by the known growing methods (before escapes erase nothing:
        // kinds are properties of the values, not of their uniqueness).
        let added = match (method, args) {
            ("append" | "add", [v]) => Some(self.value_kinds(st, *v)),
            ("insert", [_, v]) => Some(self.value_kinds(st, *v)),
            ("extend" | "update", [it]) => Some(self.iterable_elems(st, *it)),
            ("setdefault", _) => None,
            _ => None,
        };
        let mut retained = Vec::new();
        let mut indexed: Vec<(usize, VarId)> = vec![(0, recv)];
        indexed.extend(args.iter().copied().enumerate().map(|(i, a)| (i + 1, a)));
        Self::apply_param_effects(st, &entry.param_effects, &indexed, span, eff, &mut retained);
        if let Some(kinds) = added {
            Self::add_elems(st, recv, kinds);
        }
        match entry
            .return_exact_type
            .as_deref()
            .and_then(BuiltinKind::from_name)
        {
            Some(kind) => {
                let elems = if tracks_elems(kind) {
                    self.elems_of(st, recv)
                        .map_or(ElemKinds::Unknown, ElemKinds::Known)
                } else {
                    ElemKinds::Unknown
                };
                self.fresh(st, dst, kind, site, span, elems);
            }
            None => Self::opaque(st, dst, &format!("result of `.{method}()`"), span),
        }
        if !retained.is_empty() {
            st.add_derived(dst, &retained);
        }
        self.after_def(st, dst, span);
    }

    fn step_terminator(&self, st: &mut FrameState, term: &Terminator, span: Span) {
        match term {
            Terminator::Branch { cond, .. } => {
                if !self.exact_on_every_path(st, *cond, 0) {
                    // `bool(cond)` may run `__bool__` / `__len__`.
                    Self::escape_all(st, [*cond], span);
                }
            }
            Terminator::Return(Some(v)) | Terminator::Raise(Some(v)) => {
                // The object leaves the frame; a handler or `finally` body of this frame may
                // still see a raised one (along the exception edge).
                st.mark_escaped(*v, EscapeWitness::ReturnedOrRaised(span), span);
            }
            _ => {}
        }
    }

    /// Whether `v` holds an exact builtin whatever path reached here: its own type is exact, or
    /// it is a phi (`a and b`, `x if c else y`) whose every incoming value is (SSA values keep
    /// their type, and the state joins each one over the paths that define it).
    fn exact_on_every_path(&self, st: &FrameState, v: VarId, depth: u32) -> bool {
        self.exact_on_every_path_in(st, v, depth, &mut FxHashMap::default())
    }

    /// [`Self::exact_on_every_path`], deciding each value at each depth once: phis may share
    /// inputs, which a walk would reach along every path.
    fn exact_on_every_path_in(
        &self,
        st: &FrameState,
        v: VarId,
        depth: u32,
        decided: &mut FxHashMap<(VarId, u32), bool>,
    ) -> bool {
        if st.exact_type(v).is_some() {
            return true;
        }
        let walk::Def::Phi(b, _) = self.shape.def(v) else {
            return false;
        };
        if let Some(&known) = decided.get(&(v, depth)) {
            return known;
        }
        let exact = depth < 8
            && self
                .f
                .block(b)
                .phis
                .iter()
                .filter(|p| p.dst == v)
                .flat_map(|p| &p.incoming)
                .all(|&(_, x)| x != v && self.exact_on_every_path_in(st, x, depth + 1, decided));
        decided.insert((v, depth), exact);
        exact
    }

    /// Apply the phis of `succ` for the edge from `pred` (parallel copy).
    fn apply_phis(&self, st: &mut FrameState, succ: BlockId, pred: BlockId) {
        let block = self.f.block(succ);
        if block.phis.is_empty() {
            return;
        }
        let vals: Vec<(VarId, VarValue)> = block
            .phis
            .iter()
            .filter_map(|phi| {
                phi.incoming
                    .iter()
                    .find(|(p, _)| *p == pred)
                    .map(|(_, v)| (phi.dst, st.value(*v)))
            })
            .collect();
        for (dst, val) in vals {
            st.assign(dst, val);
            self.after_def(st, dst, Span { start: 0, end: 0 });
        }
    }

    /// Exception-edge widening: every SSA version of a source name present in the state may be
    /// the binding a handler observes.
    fn widen_name_groups(&self, st: &mut FrameState) {
        for group in &self.name_groups {
            let present: Vec<VarId> = group
                .iter()
                .copied()
                .filter(|v| st.var_type.contains_key(v) || st.var_unique.contains_key(v))
                .collect();
            if present.len() < 2 {
                continue;
            }
            let mut joined = st.value(present[0]);
            for &v in &present[1..] {
                let other = st.value(v);
                // `var_type` and `var_unique` always have the same keys, so a present version
                // has both (were one missing, the joined value would be untracked).
                joined.ty = joined.ty.zip(other.ty).map(|(a, b)| join_type(&a, &b));
                joined.unique = joined
                    .unique
                    .zip(other.unique)
                    .map(|(a, b)| join_unique(&a, &b));
                allocs_union(&mut joined.points_to, &other.points_to);
                joined.points_to_incomplete |= other.points_to_incomplete;
                allocs_union(&mut joined.derives_from, &other.derives_from);
            }
            for v in present {
                st.assign(v, joined.clone());
            }
        }
    }

    /// Transfer a whole block. `record` sees each instruction with the state before it.
    pub(crate) fn transfer_block(
        &self,
        bb: BlockId,
        mut st: FrameState,
        mut record: impl FnMut(usize, &FrameState, &InstEffects),
    ) -> (FrameState, Option<FrameState>) {
        let block = self.f.block(bb);
        let mut exc = block.exc_target.map(|_| st.clone());
        for (i, inst) in block.insts.iter().enumerate() {
            let mut eff = InstEffects::default();
            let before = st.clone();
            self.step(&mut st, bb, i, inst, &mut eff);
            record(i, &before, &eff);
            if let Some(e) = exc.as_mut() {
                join_into(e, &st);
            }
        }
        self.step_terminator(&mut st, &block.terminator, term_span(self.f, bb));
        if let Some(e) = exc.as_mut() {
            join_into(e, &st);
            self.widen_name_groups(e);
        }
        (st, exc)
    }

    /// RPO worklist fixpoint.
    pub(crate) fn run(&self) -> Analysis {
        let n = self.f.blocks.len();
        let mut block_in: Vec<Option<FrameState>> = vec![None; n];
        let mut out: Vec<Option<FrameState>> = vec![None; n];
        let mut exc_out: Vec<Option<FrameState>> = vec![None; n];
        let entry = self.entry_state();
        let mut converged = false;
        for _ in 0..MAX_PASSES {
            let mut changed = false;
            for &b in self.shape.rpo() {
                let bi = b.0 as usize;
                let mut new_in: Option<FrameState> = (b.0 == 0).then(|| entry.clone());
                for &p in self.shape.preds(b) {
                    let pi = p.0 as usize;
                    let pb = self.f.block(p);
                    if pb.terminator.successors().contains(&b)
                        && let Some(o) = &out[pi]
                    {
                        let mut s = o.clone();
                        self.apply_phis(&mut s, b, p);
                        match &mut new_in {
                            Some(cur) => join_into(cur, &s),
                            None => new_in = Some(s),
                        }
                    }
                    if pb.exc_target == Some(b)
                        && let Some(o) = &exc_out[pi]
                    {
                        let mut s = o.clone();
                        self.apply_phis(&mut s, b, p);
                        match &mut new_in {
                            Some(cur) => join_into(cur, &s),
                            None => new_in = Some(s),
                        }
                    }
                }
                // Every block in RPO has a predecessor earlier in RPO (its DFS parent).
                let Some(new_in) = new_in else { continue };
                if block_in[bi].as_ref() == Some(&new_in) && out[bi].is_some() {
                    continue;
                }
                let (o, e) = self.transfer_block(b, new_in.clone(), |_, _, _| {});
                block_in[bi] = Some(new_in);
                if out[bi].as_ref() != Some(&o) || exc_out[bi] != e {
                    changed = true;
                }
                out[bi] = Some(o);
                exc_out[bi] = e;
            }
            if !changed {
                converged = true;
                break;
            }
        }
        Analysis {
            block_in,
            converged,
        }
    }

    /// Every reachable block with its entry state (`run` gave every block in RPO one on its
    /// first pass).
    pub(crate) fn entered_blocks<'s>(
        &'s self,
        an: &'s Analysis,
    ) -> impl Iterator<Item = (BlockId, FrameState)> + 's {
        self.shape
            .rpo()
            .iter()
            .filter_map(|&b| Some((b, an.block_in[b.0 as usize].clone()?)))
    }

    /// Replay every reachable block once and collect each instruction's mutations.
    pub(crate) fn mutations(&self, an: &Analysis) -> FxHashMap<(BlockId, usize), Allocs> {
        let mut out = FxHashMap::default();
        for (b, st) in self.entered_blocks(an) {
            self.transfer_block(b, st, |i, _, eff| {
                if !eff.mutated.is_empty() {
                    out.insert((b, i), eff.mutated.clone());
                }
            });
        }
        out
    }

    /// State immediately before instruction `idx` of `bb` (`idx == insts.len()`: before the
    /// terminator).
    pub(crate) fn state_before(
        &self,
        an: &Analysis,
        bb: BlockId,
        idx: usize,
    ) -> Option<FrameState> {
        let mut st = an.block_in[bb.0 as usize].clone()?;
        for (i, inst) in self.f.block(bb).insts.iter().enumerate().take(idx) {
            let mut eff = InstEffects::default();
            self.step(&mut st, bb, i, inst, &mut eff);
        }
        Some(st)
    }
}

/// `var_type` reason of a `range(...)` object (its alloc carries `int` elements).
const RANGE_REASON: &str = "range object";

/// The source position of block `b`'s terminator: its last instruction, the span the IR
/// records for it (a block without instructions), or the function's.
pub(crate) fn term_span(f: &IrFunction, b: BlockId) -> Span {
    let block = f.block(b);
    block
        .insts
        .last()
        .map(|i| i.span)
        .or(block.term_span)
        .unwrap_or(f.span)
}

pub(crate) fn term_use(t: &Terminator) -> Option<VarId> {
    match t {
        Terminator::Branch { cond, .. } => Some(*cond),
        Terminator::Return(Some(v)) | Terminator::Raise(Some(v)) => Some(*v),
        _ => None,
    }
}
