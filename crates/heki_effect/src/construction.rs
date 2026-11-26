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
//!
//! Each escape comes with the tests of the function's inputs that hold where it runs: a store
//! under `if register:` may keep the object only where `register` is true, a condition the
//! answer carries ([`Keep`]) and propagation turns into a fact of the function that each of its
//! calls decides (`MayGiven`).
//!
//! A returned value may also be an element of a parameter (an item, `get`, an iteration step of
//! it) on some paths: the function returns a new object at the calls binding such a parameter to
//! a value with no elements, where those paths raise or never reach their `return`
//! (`elements_of` of [`ReturnsNew::New`]).

use crate::{
    CallTests, FuncKey, InputGuard, Keep, MayGiven, NewArg, NewCond, NewFrom, NewKind, ReturnsNew,
};
use heki_ir::walk::{Answer, Budget, Closure, Def, Facts, FnShape, Join, Or, Sparse, fixpoint};
use heki_ir::{InstKind, VarId};
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
    returns: Vec<NewFrom>,
}

/// What the extractor records for one function.
#[derive(Default)]
pub(crate) struct Recorder {
    allocs: Vec<Alloc>,
    /// The results of calls that hand back the object they are handed (a project function
    /// returning its parameter), by the argument's variable: the same object.
    handed_back: FxHashMap<VarId, VarId>,
    /// Each escape with the tests of the function's inputs that hold where it runs.
    escapes: Vec<(Pos, VarId, Escape, Vec<InputGuard>)>,
    /// The points asked about, by query id.
    queries: Vec<(Pos, VarId)>,
}

/// The escape state before a point: allocation roots and parameters that may be kept, and the
/// roots / parameters handed to each `(callee, parameter)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct State {
    allocs: u64,
    params: u64,
    /// By the id of a store that may keep what it is handed ([`Keep`]): (allocation roots,
    /// parameters).
    handed: Vec<(u64, u64)>,
}

/// What may be kept on some path: the union of the states joined.
impl Join for State {
    fn join(&mut self, other: &Self) -> bool {
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

    fn weight(&self) -> usize {
        1 + self.handed.len()
    }
}

/// The ownership facts of the variables, from the allocations through copies and phis.
struct Roots<'f> {
    /// Allocation roots each variable may hold (any phi input).
    may: FxHashMap<VarId, u64>,
    /// Variables every input of which is an allocation (`may` is exact for them).
    all: FxHashMap<VarId, bool>,
    /// Returned variables every input of which is an allocation or an element of a parameter,
    /// and some an element: the parameters those are elements of. One without an allocation
    /// (absent from `may`) returns nothing at a call binding them to values without elements.
    elements_of: FxHashMap<VarId, u64>,
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
    pub(crate) fn alloc(&mut self, pos: Pos, dst: VarId, kind: NewKind, returns: Vec<NewFrom>) {
        if self.allocs.len() < 64 {
            self.allocs.push(Alloc {
                pos,
                dst,
                kind,
                returns,
            });
        }
    }

    /// `dst` holds the object `arg` holds: the result of a call returning its argument.
    pub(crate) fn handed_back(&mut self, dst: VarId, arg: VarId) {
        self.handed_back.insert(dst, arg);
    }

    /// An escape at `pos`, where the tests `known` of the function's inputs hold.
    pub(crate) fn escape(&mut self, pos: Pos, var: VarId, how: Escape, known: Vec<InputGuard>) {
        self.escapes.push((pos, var, how, known));
    }

    /// Ask about `var` just before the instruction at `pos`; returns the query id.
    pub(crate) fn query(&mut self, pos: Pos, var: VarId) -> usize {
        self.queries.push((pos, var));
        self.queries.len() - 1
    }

    /// Answer every query, and the returns of the function of `shape` (`returns`: whether the
    /// function's result is a new object at every `return` with a value). `element_of` gives the
    /// parameters a value is an element of by its definition (`0`: none); `handed_to` the
    /// callee of a hand-over and what its call tells, where the tests given hold.
    pub(crate) fn resolve(
        self,
        shape: &FnShape<'_>,
        may_params: &FxHashMap<VarId, u64>,
        param_of: &dyn Fn(VarId) -> Option<u8>,
        element_of: &dyn Fn(VarId) -> u64,
        handed_to: &dyn Fn(u32, u16, &[InputGuard]) -> (FuncKey, CallTests),
        returns: bool,
    ) -> (Vec<NewArg>, ReturnsNew) {
        // Without an allocation, only parameters can be asked about, and nothing is returned
        // new.
        if self.queries.is_empty() && (self.allocs.is_empty() || !returns) {
            return (Vec::new(), ReturnsNew::No);
        }
        let f = shape.f;
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
        let asked = (self.queries.iter().map(|(_, v)| *v))
            .chain(returned.iter().map(|(_, v)| *v))
            .chain(self.escapes.iter().map(|(_, v, _, _)| *v));
        let budget = Budget::of_function("construction::Recorder", f);
        let returned_vars: Vec<VarId> = returned.iter().map(|(_, v)| *v).collect();
        let found = self.roots(
            shape,
            &budget,
            asked,
            (&returned_vars, element_of),
            may_params,
            param_of,
        );
        // Past the budget, no object is known to be new, as past the allocations recorded.
        let Answer::Known(roots) = found else {
            return (vec![NewArg::Old; self.queries.len()], ReturnsNew::No);
        };
        let trackable = |v: VarId| roots.may.contains_key(&v) || param_of(v).is_some();
        let any_return_new = !returned.is_empty()
            && (returned.iter()).all(|(_, v)| trackable(*v) || roots.elements_of.contains_key(v));
        if !self.queries.iter().any(|(_, v)| trackable(*v)) && !any_return_new {
            return (vec![NewArg::Old; self.queries.len()], ReturnsNew::No);
        }
        // The ids of the stores that may keep what they are handed: a hand-over, one per callee,
        // parameter, and what its call tells, and a definite store under tests of the
        // function's inputs, one per tests. The answers name only those, so the calls of one
        // helper binding alike share a hand-over.
        let mut handed_ids: FxHashMap<Keep, usize> = FxHashMap::default();
        let mut handed_keys: Vec<Keep> = Vec::new();
        let mut escape_ids: Vec<Option<usize>> = Vec::with_capacity(self.escapes.len());
        for (_, _, how, known) in &self.escapes {
            let key = match how {
                Escape::Definite if known.is_empty() => {
                    escape_ids.push(None);
                    continue;
                }
                Escape::Definite => Keep {
                    by: None,
                    at: CallTests {
                        args: Vec::new(),
                        known: known.clone(),
                    },
                },
                Escape::Handed(s, c, p) => {
                    let (callee, at) = handed_to(*s, *c, known);
                    Keep {
                        by: Some((callee, *p)),
                        at,
                    }
                }
            };
            let next = handed_keys.len();
            let id = *handed_ids.entry(key.clone()).or_insert(next);
            if id == next {
                handed_keys.push(key);
            }
            escape_ids.push(Some(id));
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
        for ((pos, var, _, _), id) in self.escapes.iter().zip(escape_ids) {
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
        // The forward data flow over the blocks the entry reaches, in reverse postorder: the
        // state on entering a block joins what each predecessor may have kept on its way out,
        // through its normal edges, and at any of its points through its exception edge. Kills
        // apply inside a block, so the states only grow.
        let rpo = shape.rpo();
        let node = |b: heki_ir::BlockId| shape.rpo_index(b).map(|i| i as usize);
        let out_of = |p: heki_ir::BlockId, facts: &Facts<State>| {
            let mut state = node(p)
                .and_then(|i| facts.get(i))
                .map_or_else(|| empty.clone(), Clone::clone);
            let mut exc = state.clone();
            for (_, _, action) in of(p.0 as usize) {
                apply(&mut state, action);
                exc.join(&state);
            }
            (state, exc)
        };
        let transfer = |i: usize, facts: &Facts<State>| {
            let b = rpo[i];
            let mut entry = empty.clone();
            for &p in shape.preds(b).iter().filter(|p| node(**p).is_some()) {
                let (out, exc) = out_of(p, facts);
                let pb = f.block(p);
                if pb.terminator.successors().contains(&b) {
                    entry.join(&out);
                }
                if pb.exc_target == Some(b) {
                    entry.join(&exc);
                }
            }
            Some(entry)
        };
        let readers = |i: usize, out: &mut Vec<usize>| {
            out.extend(shape.succs(rpo[i]).filter_map(node));
        };
        // A block is visited once, then once per bit a predecessor's state gains.
        let bits = 2 * 64 * (1 + handed_keys.len());
        let most_preds = (rpo.iter().map(|b| shape.preds(*b).len()).max()).unwrap_or(0);
        let spec = Sparse {
            nodes: rpo.len(),
            readers: &readers,
            max_visits: u32::try_from(1 + most_preds * (1 + bits)).unwrap_or(u32::MAX),
        };
        // Past the budget, no object is known to be new, as past the roots' walk.
        let Answer::Known(block_in) = fixpoint(&budget, spec, transfer) else {
            return (vec![NewArg::Old; self.queries.len()], ReturnsNew::No);
        };
        // Answer the queries with the state before each one (a block the entry does not reach
        // keeps nothing).
        let mut answers = vec![NewArg::Old; self.queries.len() + returned.len()];
        let mut state = empty.clone();
        for (bi, block) in f.blocks.iter().enumerate() {
            let list = of(bi);
            if list.iter().all(|(_, _, a)| !matches!(a, Action::Query(_))) {
                continue;
            }
            state.clone_from(
                node(block.id)
                    .and_then(|i| block_in.get(i))
                    .unwrap_or(&empty),
            );
            for (_, _, action) in list {
                if let Action::Query(i) = action {
                    let (var, of_return) = match self.queries.get(*i) {
                        Some((_, v)) => (*v, false),
                        None => (returned[*i - self.queries.len()].1, true),
                    };
                    answers[*i] = self.answer(&roots, &state, &handed_keys, var, of_return);
                } else {
                    apply(&mut state, action);
                }
            }
        }
        let tail = answers.split_off(self.queries.len());
        let returns_new = || {
            let mut kind = NewKind::Own;
            let mut cond = NewCond::default();
            let mut any = false;
            for (v, a) in returned_vars.iter().zip(tail) {
                // Only elements of parameters: nothing is returned there at such a call.
                if !roots.may.contains_key(v) && roots.elements_of.contains_key(v) {
                    continue;
                }
                let NewArg::New { kind: k, cond: c } = a else {
                    return ReturnsNew::No;
                };
                kind = kind.min(k);
                cond = cond.and(&c);
                any = true;
            }
            let elements_of = (returned_vars.iter())
                .filter_map(|v| roots.elements_of.get(v))
                .fold(0, |mask, m| mask | m);
            if !any {
                return ReturnsNew::No;
            }
            ReturnsNew::New {
                kind,
                cond,
                elements_of,
                kept: MayGiven::default(),
            }
        };
        (answers, returns_new())
    }

    /// The allocation roots each of `asked` may hold, through copies and phis: `may`, the roots
    /// of every value a value reaches through their inputs, and `all`, whether every value on the
    /// way holds allocations only; for the `returned` values, also `elements_of`, by the
    /// parameters `element_of` says a value is an element of. Open past the budget.
    fn roots<'f>(
        &self,
        shape: &FnShape<'_>,
        budget: &Budget,
        asked: impl Iterator<Item = VarId>,
        (returned, element_of): (&[VarId], &dyn Fn(VarId) -> u64),
        may_params: &'f FxHashMap<VarId, u64>,
        param_of: &'f dyn Fn(VarId) -> Option<u8>,
    ) -> Answer<Roots<'f>> {
        let mut roots = Roots {
            may: FxHashMap::default(),
            all: FxHashMap::default(),
            elements_of: FxHashMap::default(),
            may_params,
            param_of,
        };
        let mut own: FxHashMap<VarId, u64> = FxHashMap::default();
        for (r, a) in self.allocs.iter().enumerate() {
            *own.entry(a.dst).or_default() |= 1u64 << r;
        }
        if own.is_empty() {
            return Answer::Known(roots);
        }
        let f = shape.f;
        // A local of a function read where nothing bound it: reading it raises
        // `UnboundLocalError`, so it holds no object (in a module or class body the read falls
        // back to the globals).
        let unbound = |x: VarId| {
            !f.is_module_body
                && !f.is_class_body
                && matches!(
                    shape.inst(x).map(|i| &i.kind),
                    Some(InstKind::LoadName { .. })
                )
        };
        // A copy's source, the argument a call hands back, a phi's inputs other than itself that
        // may hold an object.
        let inputs = |v: VarId, out: &mut Vec<VarId>| match shape.def(v) {
            Def::Inst(b, i) => {
                if let InstKind::Copy { src, .. } = f.block(b).insts[i as usize].kind {
                    out.push(src);
                }
                out.extend(self.handed_back.get(&v));
            }
            Def::Phi(b, i) => out.extend(
                (f.block(b).phis[i as usize].incoming.iter().map(|(_, x)| *x))
                    .filter(|x| *x != v && !unbound(*x)),
            ),
            Def::Param | Def::None => {}
        };
        let held: Closure<VarId, Or> = Closure::new();
        let may = |v: VarId| {
            let own = |u: VarId| Or(own.get(&u).copied().unwrap_or(0));
            held.ask(budget, v, own, inputs).map(|bits| bits.0)
        };
        let lacking: Closure<VarId, bool> = Closure::new();
        for v in asked {
            // A value on the way that holds no allocation.
            let none = |u: VarId| !matches!(may(u), Answer::Known(b) if b != 0);
            let held = may(v).and_then(|bits| {
                if bits == 0 || roots.may.contains_key(&v) {
                    return Answer::Known(None);
                }
                (lacking.ask(budget, v, none, inputs)).map(|lacks| Some((bits, !*lacks)))
            });
            let Answer::Known(held) = held else {
                return Answer::Open;
            };
            if let Some((bits, all)) = held {
                roots.may.insert(v, bits);
                roots.all.insert(v, all);
            }
        }
        // A returned value some inputs of which are elements of parameters, the others
        // allocations.
        let elements: Closure<VarId, Or> = Closure::new();
        let elements_of = |v: VarId| {
            let of = |u: VarId| Or(element_of(u));
            elements.ask(budget, v, of, inputs).map(|m| m.0)
        };
        let lacking_either: Closure<VarId, bool> = Closure::new();
        // A value on the way that holds neither an allocation nor an element.
        let neither = |u: VarId| {
            let holds = |a: Answer<u64>| matches!(a, Answer::Known(b) if b != 0);
            !holds(elements_of(u)) && !holds(may(u))
        };
        for &v in returned {
            if roots.all.get(&v).is_some_and(|all| *all) {
                continue;
            }
            let mixed = elements_of(v).and_then(|mask| {
                let lacks = lacking_either.ask(budget, v, neither, inputs);
                lacks.map(|lacks| (mask != 0 && !*lacks).then_some(mask))
            });
            // Past the budget, the value is known to be no element of a parameter.
            if let Some(mask) = mixed.open_means(None) {
                roots.elements_of.insert(v, mask);
            }
        }
        Answer::Known(roots)
    }

    /// Whether `var` holds, at a point with escape state `state`, a new object or a parameter
    /// that nothing kept, and under which conditions. A returned value (`of_return`) may also
    /// be an element of a parameter on the paths without an allocation
    /// ([`Roots::elements_of`]).
    fn answer(
        &self,
        roots: &Roots<'_>,
        state: &State,
        handed_keys: &[Keep],
        var: VarId,
        of_return: bool,
    ) -> NewArg {
        let (a, p) = roots.masks(var);
        let new = a != 0
            && p == 0
            && (roots.all.get(&var).copied().unwrap_or(false)
                || of_return && roots.elements_of.contains_key(&var));
        let param = if a == 0 { (roots.param_of)(var) } else { None };
        if (!new && param.is_none()) || state.allocs & a != 0 || state.params & p != 0 {
            return NewArg::Old;
        }
        let mut keeps = Vec::new();
        for (i, (ha, hp)) in state.handed.iter().enumerate() {
            if ha & a != 0 || hp & p != 0 {
                keeps.push(handed_keys[i].clone());
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
