//! Objects under construction (`fresh-objects`, docs/configuration.md).
//!
//! A write constructs the object it writes when the object was allocated in the same call
//! chain — by calling its class, by copying, or by a project function returning a new object —
//! or is a parameter of the writing function, and nothing may hold it besides the frame yet: no
//! store, yield, closure, or call that may keep it ran before the write. The extractor records,
//! in source order, the allocations, the escapes (a definite one, or a hand-over to a project
//! callee's parameter, which keeps the object only if that callee does), and the points it asks
//! about (writes, call arguments, returns); [`Recorder::resolve`] runs one forward data flow over
//! the function's blocks (kills at allocations, unions at joins and exception edges) and answers
//! each question with a [`NewArg`]: whether the variable holds a new object (or a parameter) that
//! nothing kept so far, under conditions on the callees, which propagation resolves.

use crate::{FuncKey, NewArg, NewCond, NewKind};
use heki_ir::{InstKind, IrFunction, VarId};
use rustc_hash::FxHashMap;

/// Position of an instruction: block, index in the block (`insts.len()` for the terminator).
pub(crate) type Pos = (u32, u32);

/// An escape of a variable's object at an instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Escape {
    /// Stored, yielded, captured, or handed to code heki does not follow.
    Definite,
    /// Handed to a project callee's parameter (the call site and callee indexes of the
    /// function, the parameter): kept only if the callee keeps it.
    Handed(u32, u16, u8),
}

/// One allocation: a variable defined by a constructor call, a copy, or a project call
/// (`returns`) whose callees must return a new object.
struct Alloc {
    pos: Pos,
    dst: VarId,
    kind: NewKind,
    returns: Vec<FuncKey>,
}

/// What the extractor records for one function.
#[derive(Default)]
pub(crate) struct Recorder {
    allocs: Vec<Alloc>,
    escapes: Vec<(Pos, VarId, Escape)>,
    /// The points asked about, by query id.
    queries: Vec<(Pos, VarId)>,
}

/// The escape state before a point: allocation roots and parameters that may be kept, and the
/// roots / parameters handed to each `(callee, parameter)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct State {
    allocs: u64,
    params: u64,
    /// By hand-over id: (allocation roots, parameters).
    handed: Vec<(u64, u64)>,
}

impl State {
    fn join(&mut self, other: &Self) {
        self.absorb(other);
    }

    /// `self |= other`; whether it grew.
    fn absorb(&mut self, other: &Self) -> bool {
        let before = (self.allocs, self.params);
        self.allocs |= other.allocs;
        self.params |= other.params;
        let mut grew = before != (self.allocs, self.params);
        for (a, b) in self.handed.iter_mut().zip(&other.handed) {
            let was = *a;
            a.0 |= b.0;
            a.1 |= b.1;
            grew |= was != *a;
        }
        grew
    }
}

/// The ownership facts of the variables, from the allocations through copies and phis.
struct Roots<'f> {
    /// Allocation roots each variable may hold (any phi input).
    may: FxHashMap<VarId, u64>,
    /// Variables every input of which is an allocation (`may` is exact for them).
    all: FxHashMap<VarId, bool>,
    may_params: &'f FxHashMap<VarId, u64>,
    /// `(i, None)` entries: the variable is parameter `i` itself on every path.
    param_of: &'f dyn Fn(VarId) -> Option<u8>,
}

impl Roots<'_> {
    fn masks(&self, var: VarId) -> (u64, u64) {
        (
            self.may.get(&var).copied().unwrap_or(0),
            self.may_params.get(&var).copied().unwrap_or(0),
        )
    }
}

/// An action of the data flow at one position, applied in this order: queries, escapes, kills.
enum Action {
    Query(usize),
    Escape(VarId, Option<usize>),
    Kill(u64),
}

impl Recorder {
    pub(crate) fn alloc(&mut self, pos: Pos, dst: VarId, kind: NewKind, returns: Vec<FuncKey>) {
        if self.allocs.len() < 64 {
            self.allocs.push(Alloc {
                pos,
                dst,
                kind,
                returns,
            });
        }
    }

    pub(crate) fn escape(&mut self, pos: Pos, var: VarId, how: Escape) {
        self.escapes.push((pos, var, how));
    }

    /// Ask about `var` just before the instruction at `pos`; returns the query id.
    pub(crate) fn query(&mut self, pos: Pos, var: VarId) -> usize {
        self.queries.push((pos, var));
        self.queries.len() - 1
    }

    /// Answer every query, and the returns of `f` (`returns`: whether the function's result is
    /// a new object at every `return` with a value).
    pub(crate) fn resolve(
        self,
        f: &IrFunction,
        may_params: &FxHashMap<VarId, u64>,
        param_of: &dyn Fn(VarId) -> Option<u8>,
        callee_of: &dyn Fn(u32, u16) -> FuncKey,
        returns: bool,
    ) -> (Vec<NewArg>, Option<NewArg>) {
        // Without an allocation, only parameters can be asked about, and nothing is returned
        // new.
        if self.queries.is_empty() && (self.allocs.is_empty() || !returns) {
            return (Vec::new(), None);
        }
        let roots = self.roots(f, may_params, param_of);
        let trackable = |v: VarId| roots.may.contains_key(&v) || param_of(v).is_some();
        let returned: Vec<(Pos, VarId)> = if returns {
            f.blocks
                .iter()
                .filter_map(|b| match b.terminator {
                    heki_ir::Terminator::Return(Some(v)) => {
                        Some(((b.id.0, u32::try_from(b.insts.len()).ok()?), v))
                    }
                    _ => None,
                })
                .collect()
        } else {
            Vec::new()
        };
        let any_return_new = !returned.is_empty() && returned.iter().all(|(_, v)| trackable(*v));
        if !self.queries.iter().any(|(_, v)| trackable(*v)) && !any_return_new {
            return (vec![NewArg::Old; self.queries.len()], None);
        }
        // Hand-over ids.
        let mut handed_ids: FxHashMap<(u32, u16, u8), usize> = FxHashMap::default();
        let mut handed_keys: Vec<(u32, u16, u8)> = Vec::new();
        for (_, _, how) in &self.escapes {
            if let Escape::Handed(s, c, p) = how
                && !handed_ids.contains_key(&(*s, *c, *p))
            {
                handed_ids.insert((*s, *c, *p), handed_keys.len());
                handed_keys.push((*s, *c, *p));
            }
        }
        // Every action in position order (queries, escapes, kills at one position), and the
        // range of each block's actions.
        let n = f.blocks.len();
        let mut actions: Vec<(Pos, u8, Action)> = Vec::with_capacity(
            self.queries.len() + returned.len() + self.escapes.len() + self.allocs.len(),
        );
        for (i, (pos, _)) in self.queries.iter().enumerate() {
            actions.push((*pos, 0, Action::Query(i)));
        }
        for (i, (pos, _)) in returned.iter().enumerate() {
            actions.push((*pos, 0, Action::Query(self.queries.len() + i)));
        }
        for (pos, var, how) in &self.escapes {
            let id = match how {
                Escape::Definite => None,
                Escape::Handed(s, c, p) => Some(handed_ids[&(*s, *c, *p)]),
            };
            actions.push((*pos, 1, Action::Escape(*var, id)));
        }
        for (r, a) in self.allocs.iter().enumerate() {
            actions.push((a.pos, 2, Action::Kill(1u64 << r)));
        }
        actions.sort_by_key(|(pos, phase, _)| (*pos, *phase));
        let mut ranges = vec![(0usize, 0usize); n];
        let mut start = 0;
        while start < actions.len() {
            let block = actions[start].0.0 as usize;
            let mut end = start;
            while end < actions.len() && actions[end].0.0 as usize == block {
                end += 1;
            }
            ranges[block] = (start, end);
            start = end;
        }
        let of = |bi: usize| &actions[ranges[bi].0..ranges[bi].1];
        let empty = State {
            handed: vec![(0, 0); handed_keys.len()],
            ..State::default()
        };
        let apply = |state: &mut State, action: &Action| match action {
            Action::Escape(var, id) => {
                let (a, p) = roots.masks(*var);
                match id {
                    None => {
                        state.allocs |= a;
                        state.params |= p;
                    }
                    Some(id) => {
                        state.handed[*id].0 |= a;
                        state.handed[*id].1 |= p;
                    }
                }
            }
            Action::Kill(bit) => {
                state.allocs &= !bit;
                for h in &mut state.handed {
                    h.0 &= !bit;
                }
            }
            Action::Query(_) => {}
        };
        // The forward data flow to a fixpoint (states only grow: kills apply inside a block).
        // The two buffers are reused, so the iterations allocate nothing.
        let mut block_in: Vec<State> = vec![empty.clone(); n];
        let (mut state, mut exc) = (empty.clone(), empty);
        let rpo = f.rpo_blocks();
        loop {
            let mut changed = false;
            for &b in &rpo {
                let bi = b.0 as usize;
                state.clone_from(&block_in[bi]);
                exc.clone_from(&state);
                for (_, _, action) in of(bi) {
                    apply(&mut state, action);
                    exc.join(&state);
                }
                let block = f.block(b);
                for succ in block.terminator.successors() {
                    changed |= block_in[succ.0 as usize].absorb(&state);
                }
                if let Some(t) = block.exc_target {
                    changed |= block_in[t.0 as usize].absorb(&exc);
                }
            }
            if !changed {
                break;
            }
        }
        // Answer the queries with the state before each one.
        let mut answers = vec![NewArg::Old; self.queries.len() + returned.len()];
        for (bi, entry) in block_in.iter().enumerate() {
            let list = of(bi);
            if list.iter().all(|(_, _, a)| !matches!(a, Action::Query(_))) {
                continue;
            }
            state.clone_from(entry);
            for (_, _, action) in list {
                if let Action::Query(i) = action {
                    let var = match self.queries.get(*i) {
                        Some((_, v)) => *v,
                        None => returned[*i - self.queries.len()].1,
                    };
                    answers[*i] = self.answer(&roots, &state, &handed_keys, callee_of, var);
                } else {
                    apply(&mut state, action);
                }
            }
        }
        let tail = answers.split_off(self.queries.len());
        let returns_new = (!tail.is_empty())
            .then(|| {
                let mut kind = NewKind::Own;
                let mut cond = NewCond::default();
                for a in tail {
                    let NewArg::New { kind: k, cond: c } = a else {
                        return None;
                    };
                    kind = kind.min(k);
                    cond = cond.and(&c);
                }
                Some(NewArg::New { kind, cond })
            })
            .flatten();
        (answers, returns_new)
    }

    /// The allocation roots of every variable, through copies and phis (a monotone fixpoint).
    fn roots<'f>(
        &self,
        f: &IrFunction,
        may_params: &'f FxHashMap<VarId, u64>,
        param_of: &'f dyn Fn(VarId) -> Option<u8>,
    ) -> Roots<'f> {
        let mut may: FxHashMap<VarId, u64> = FxHashMap::default();
        for (r, a) in self.allocs.iter().enumerate() {
            *may.entry(a.dst).or_default() |= 1u64 << r;
        }
        if may.is_empty() {
            return Roots {
                may,
                all: FxHashMap::default(),
                may_params,
                param_of,
            };
        }
        // Copies and phis (phi inputs other than the phi itself).
        let mut copies: Vec<(VarId, VarId)> = Vec::new();
        let mut phis: Vec<&heki_ir::PhiNode> = Vec::new();
        for bb in &f.blocks {
            phis.extend(&bb.phis);
            for inst in &bb.insts {
                if let InstKind::Copy { dst, src } = &inst.kind {
                    copies.push((*dst, *src));
                }
            }
        }
        let inputs = |phi: &heki_ir::PhiNode| {
            let dst = phi.dst;
            phi.incoming
                .iter()
                .map(|(_, v)| *v)
                .filter(move |v| *v != dst)
                .collect::<Vec<_>>()
        };
        let phi_inputs: Vec<(VarId, Vec<VarId>)> =
            phis.iter().map(|p| (p.dst, inputs(p))).collect();
        let flows = || {
            copies
                .iter()
                .map(|(d, s)| (*d, std::slice::from_ref(s)))
                .chain(phi_inputs.iter().map(|(d, i)| (*d, i.as_slice())))
        };
        // `may`: a union over every input, to the least fixpoint.
        loop {
            let mut changed = false;
            for (dst, inputs) in flows() {
                let bits = inputs
                    .iter()
                    .fold(0, |acc, v| acc | may.get(v).copied().unwrap_or(0));
                if bits != 0 {
                    let slot = may.entry(dst).or_default();
                    if *slot | bits != *slot {
                        *slot |= bits;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        // `all`: every input holds allocations only, to the greatest fixpoint.
        let mut all: FxHashMap<VarId, bool> = may.keys().map(|v| (*v, true)).collect();
        loop {
            let mut changed = false;
            for (dst, inputs) in flows() {
                if all.get(&dst) == Some(&true)
                    && !inputs.iter().all(|v| all.get(v).copied().unwrap_or(false))
                {
                    all.insert(dst, false);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        Roots {
            may,
            all,
            may_params,
            param_of,
        }
    }

    /// Whether `var` holds, at a point with escape state `state`, a new object or a parameter
    /// that nothing kept, and under which conditions.
    fn answer(
        &self,
        roots: &Roots<'_>,
        state: &State,
        handed_keys: &[(u32, u16, u8)],
        callee_of: &dyn Fn(u32, u16) -> FuncKey,
        var: VarId,
    ) -> NewArg {
        let (a, p) = roots.masks(var);
        let new = a != 0 && p == 0 && roots.all.get(&var).copied().unwrap_or(false);
        let param = if a == 0 { (roots.param_of)(var) } else { None };
        if (!new && param.is_none()) || state.allocs & a != 0 || state.params & p != 0 {
            return NewArg::Old;
        }
        let mut keeps = Vec::new();
        for (i, (ha, hp)) in state.handed.iter().enumerate() {
            if ha & a != 0 || hp & p != 0 {
                let (s, c, k) = handed_keys[i];
                keeps.push((callee_of(s, c), k));
            }
        }
        if let Some(param) = param {
            let cond = NewCond::new(Vec::new(), keeps);
            return NewArg::Param { param, cond };
        }
        let mut kind = NewKind::Own;
        let mut returns = Vec::new();
        for (r, alloc) in self.allocs.iter().enumerate() {
            if a & (1u64 << r) != 0 {
                kind = kind.min(alloc.kind);
                returns.extend(alloc.returns.iter().cloned());
            }
        }
        NewArg::New {
            kind,
            cond: NewCond::new(returns, keeps),
        }
    }
}
