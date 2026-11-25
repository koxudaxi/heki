//! The local part of [`crate::EffectSummary::only_read_params`] and
//! [`crate::ContainerParams`]: walks over the uses of the values a parameter's object may be.
//! A change of the object itself is [`crate::EffectSummary::mutated_param_paths`]' (the
//! `MUT110` judgment); these walks find what else may keep it, change it through a value that
//! may be it, or hand it to code heki does not follow.

use super::{Extractor, Position, ProjectCall};
use crate::ContainerParams;
use crate::readonly::{
    ContainerKind, container_mutator, result_detached_from_container, returns_container_view,
};
use crate::resolver::{Callee, Val, func_qualname, mutable_container};
use heki_ir::walk::{Answer, Budget, Def, FnShape, Use, Visit, reach};
use heki_ir::{
    BinOpKind, BlockId, BuiltinKind, CompareOpKind, DictEntry, FunctionId, InstKind, Terminator,
    UNPACKED, UnaryOpKind, VarId,
};
use heki_kb::SoundnessParamEffect;
use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::Cell;
use std::collections::BTreeMap;

/// The library calls that take no order from a set they iterate: what they give is the same in
/// any order of its elements.
const ORDER_FREE: &[&str] = &[
    "builtins.len",
    "builtins.bool",
    "builtins.set",
    "builtins.frozenset",
    "builtins.sorted",
    "builtins.min",
    "builtins.max",
    "builtins.any",
    "builtins.all",
];

/// The classes whose methods are those of a builtin list, dict, or set: the builtins, their
/// `typing` aliases, and the abstract containers.
const CONTAINER_CLASSES: &[&str] = &[
    "builtins.list",
    "builtins.dict",
    "builtins.set",
    "typing.List",
    "typing.Dict",
    "typing.Set",
    "typing.MutableSequence",
    "typing.MutableMapping",
    "typing.MutableSet",
    "collections.abc.MutableSequence",
    "collections.abc.MutableMapping",
    "collections.abc.MutableSet",
];

/// What one use of a value does with the object the value may be.
enum Handling {
    /// Only reads it: a test, an item read, an iteration, a call Layer 1 lists as reading it.
    Reads,
    /// The value it defines holds the object too: a copy, a phi, a view of it (an iterator, a
    /// `dict` view, a generator over it).
    Holds(VarId),
    /// Hands it to project callees, each binding it to a parameter: their facts decide, when
    /// propagation joins them. A generator or a coroutine the call creates holds it too.
    Bound(Option<VarId>),
    /// Anything else: the object may be changed, kept, or handed to code heki does not follow.
    Open,
}

/// The project calls of the function by position, and the budget of the walks over its values.
struct Walks<'x> {
    calls_at: FxHashMap<Position, &'x ProjectCall>,
    budget: Budget,
}

impl<'a> Extractor<'a> {
    /// [`crate::EffectSummary::only_read_params`] and [`crate::EffectSummary::container_params`]
    /// of the function's own body. A stub's body and one with an A1 hazard (`eval`, `locals()`,
    /// a body too large to model) establish nothing.
    pub(super) fn only_read_facts(&mut self) {
        let f = self.f;
        let walks = Walks {
            calls_at: self.project_calls.iter().map(|c| (c.at, c)).collect(),
            budget: Budget::of_function("summary::only_read", f),
        };
        if crate::is_stub(self.path) || !f.a1_hazards.is_empty() {
            self.summary.container_params = self.container_params(&walks, false);
            return;
        }
        let mut starts: BTreeMap<u8, Vec<VarId>> = BTreeMap::new();
        // A copy `[*p]` and `f(*p)` unpack is iterated, not held ([`Self::handling`]).
        let held = self
            .may_params
            .iter()
            .filter(|(v, _)| self.f.var_name(**v) != Some(UNPACKED));
        for (&v, &mask) in held {
            for k in (0..64u8).filter(|k| mask & (1u64 << k) != 0) {
                starts.entry(k).or_default().push(v);
            }
        }
        let mut only = u64::MAX;
        for (k, mut seeds) in starts {
            seeds.sort_unstable();
            if !self.only_read_from(&walks, k, seeds) {
                only &= !(1u64 << k);
            }
        }
        let containers = self.container_params(&walks, true);
        self.summary.only_read_params = only;
        self.summary.container_params = containers;
    }

    /// Whether no use of `seeds` (the values that may be the object of position `k`), or of
    /// the values holding it, leaves the object open; open past the walk budget.
    fn only_read_from(&self, walks: &Walks<'_>, k: u8, seeds: Vec<VarId>) -> bool {
        let open = Cell::new(false);
        let found = reach(
            &walks.budget,
            seeds,
            |v, next| {
                for &u in self.shape.readers(v) {
                    match self.handling(walks, k, v, u) {
                        Handling::Reads | Handling::Bound(None) => {}
                        Handling::Holds(w) | Handling::Bound(Some(w)) => next.push(w),
                        Handling::Open => open.set(true),
                    }
                }
            },
            |_| match open.get() {
                true => Visit::Stop(()),
                false => Visit::Expand,
            },
        );
        !open.get() && matches!(found, Answer::Known(None))
    }

    /// What the use `u` of `v`, a value that may be the object of position `k`, does with it.
    fn handling(&self, walks: &Walks<'_>, k: u8, v: VarId, u: Use) -> Handling {
        let (bb, i) = match u {
            Use::Phi(bb, i) => return Handling::Holds(self.f.block(bb).phis[i as usize].dst),
            Use::Terminator(bb) => {
                return match self.f.block(bb).terminator {
                    Terminator::Branch { cond, .. } if cond == v => Handling::Reads,
                    _ => Handling::Open,
                };
            }
            Use::Inst(bb, i) => (bb, i as usize),
        };
        let inst = &self.f.block(bb).insts[i];
        match &inst.kind {
            InstKind::Copy { dst, .. } if self.f.var_name(*dst) == Some(UNPACKED) => {
                Handling::Reads
            }
            InstKind::Copy { dst, .. } => Handling::Holds(*dst),
            InstKind::BinOp {
                lhs,
                in_place: true,
                ..
            } if *lhs == v => Handling::Open,
            InstKind::SubscriptDelete { obj, .. } if *obj == v => Handling::Open,
            // An operator, a comparison, or an item access with the object as the key runs a
            // special method of the other operand's class, which gets the object.
            InstKind::BinOp { lhs, rhs, .. } => self.operand_handling(bb, v, *lhs, *rhs),
            InstKind::Compare { op, lhs, rhs, .. } => match op {
                CompareOpKind::Is | CompareOpKind::IsNot => Handling::Reads,
                CompareOpKind::In | CompareOpKind::NotIn if *rhs == v => Handling::Reads,
                _ => self.operand_handling(bb, v, *lhs, *rhs),
            },
            InstKind::SubscriptLoad { obj, index, .. }
            | InstKind::SubscriptDelete { obj, index, .. } => {
                self.operand_handling(bb, v, *obj, *index)
            }
            InstKind::UnaryOp { .. }
            | InstKind::Convert { .. }
            | InstKind::Format { .. }
            | InstKind::BuildString { .. }
            | InstKind::MakeIter { .. }
            | InstKind::IterNext { .. }
            | InstKind::YieldFrom { .. }
            | InstKind::Delete { .. } => Handling::Reads,
            // `{**p}` reads the mapping's items into a new dict.
            InstKind::DictLiteral { entries, .. } => {
                let held = entries.iter().any(
                    |e| matches!(e, DictEntry::KeyValue { key, value } if *key == v || *value == v),
                );
                match held {
                    true => Handling::Open,
                    false => Handling::Reads,
                }
            }
            // A generator expression holds an iterator over its outermost iterable, and the
            // values it captures, which its body reads where it runs.
            InstKind::MakeGenExpr {
                dst,
                func_id,
                captured,
                ..
            } => {
                if captured.contains(&v) && !self.child_reads_parameter(k, v, *func_id) {
                    return Handling::Open;
                }
                Handling::Holds(*dst)
            }
            InstKind::Call { callee_var, .. } if *callee_var == Some(v) => Handling::Open,
            kind => CallOperands::of(kind, (bb, i)).map_or(Handling::Open, |call| {
                self.call_handling(walks, k, v, &call)
            }),
        }
    }

    /// An operation on `a` and `b`, one of them `v`: the other operand, when it may be an
    /// instance of a project class, has its special method run with the object, which may keep
    /// it; a builtin's only reads it.
    fn operand_handling(&self, bb: BlockId, v: VarId, a: VarId, b: VarId) -> Handling {
        let other = if a == v { b } else { a };
        let project = |c: &String| self.cx.project.is_project_class(c);
        match self.val_in(bb, other) {
            Some(Val::Inst(c)) if project(c) => Handling::Open,
            Some(Val::Union(cs)) if cs.iter().any(project) => Handling::Open,
            _ => Handling::Reads,
        }
    }

    /// Whether the generator expression `child` reads `v` as the function's parameter `k`
    /// itself: `v` is the parameter, never rebound, and the expression's captured parameters
    /// (`FunctionDecl::captured_params`) bind it, so the call creating it hands the object to
    /// the expression's own fact of it.
    fn child_reads_parameter(&self, k: u8, v: VarId, child: Option<FunctionId>) -> bool {
        let param = self
            .f
            .params
            .get(usize::from(k))
            .is_some_and(|p| p.var == v);
        let bit = 1u64 << k;
        param
            && child.is_some_and(|id| {
                let key = crate::FuncKey {
                    module: self.key.module.clone(),
                    qualname: func_qualname(self.cx.ir().function(id)).to_owned(),
                };
                self.cx
                    .project
                    .function_decl(&key)
                    .and_then(|d| d.captured_params.as_deref())
                    .is_some_and(|c| {
                        (c.params.iter())
                            .any(|(owner, p)| self.enclosing_param_bit(owner, *p) == bit)
                    })
            })
    }

    /// What the call `call` does with `v`, one of its operands other than the value it calls.
    fn call_handling(
        &self,
        walks: &Walks<'_>,
        k: u8,
        v: VarId,
        call: &CallOperands<'_>,
    ) -> Handling {
        match self.callees.get(&call.at) {
            Some(Callee::Project(keys, _)) => self.project_handling(walks, k, v, call, keys),
            Some(Callee::External(symbol)) => self.library_handling(v, call, symbol),
            _ => Handling::Open,
        }
    }

    /// A project call: `v` bound to a parameter of every callee, on a call whose targets are
    /// every implementation it can run and that constructs no object, hands the object to the
    /// callees' facts; the value itself must be one that may be the object (a view of it is no
    /// argument the callees' facts are of).
    fn project_handling(
        &self,
        walks: &Walks<'_>,
        k: u8,
        v: VarId,
        call: &CallOperands<'_>,
        keys: &[crate::FuncKey],
    ) -> Handling {
        let places = call.places(self.shape, v);
        let carried = self
            .may_params
            .get(&v)
            .is_some_and(|m| m & (1u64 << k) != 0);
        // Each place the call hands `v` over at is one every callee binds to a parameter: the
        // receiver they bind, an argument by position or keyword (one `*` unpacks is iterated,
        // `**` copies a mapping's items).
        let site = walks.calls_at.get(&call.at).filter(|site| {
            let exact = places
                .iter()
                .filter(|p| matches!(p, Place::Position(_) | Place::Keyword))
                .count();
            carried
                && self.sites[site.site].constructs.is_none()
                && site.params.len() == keys.len()
                && !places.contains(&Place::AfterUnpacking)
                && (!places.contains(&Place::Receiver) || site.recv == Some(v))
                && (site.params.iter())
                    .all(|(_, bound)| bound.iter().filter(|(_, a)| *a == v).count() == exact)
                && self.targets_complete_at(call.at)
        });
        let Some(site) = site else {
            return Handling::Open;
        };
        let creates = keys.iter().any(|key| {
            self.cx
                .project
                .function_decl(key)
                .is_some_and(|d| d.is_generator || d.is_async && !self.sites[site.site].is_awaited)
        });
        Handling::Bound(creates.then_some(call.dst))
    }

    /// What Layer 1 lists the library call `symbol` as doing with its argument at `position`
    /// (with no entry, it may keep or change it), for the result `dst` of the call.
    fn layer1(&self, symbol: &str, position: usize, dst: VarId) -> Handling {
        let effect = (self.cx.layer1)
            .and_then(|kb| kb.lookup(symbol))
            .and_then(|e| {
                e.param_effects
                    .get(position)
                    .or_else(|| e.param_effects.last())
            });
        match effect {
            Some(SoundnessParamEffect::ReadOnly) => Handling::Reads,
            Some(SoundnessParamEffect::RetainedInReturn) => Handling::Holds(dst),
            _ => Handling::Open,
        }
    }

    /// A call of a library function or method `symbol`: what Layer 1 lists it as doing with the
    /// argument, or for the receiver of a builtin container's method, what the method does.
    fn library_handling(&self, v: VarId, call: &CallOperands<'_>, symbol: &str) -> Handling {
        let offset = self.library_offset(call);
        let mut held = None;
        for place in call.places(self.shape, v) {
            let handling = match place {
                Place::Receiver => self.receiver_handling(symbol, call.dst),
                Place::Position(j) => self.layer1(symbol, j + offset, call.dst),
                Place::Unpacked | Place::Mapping => Handling::Reads,
                Place::AfterUnpacking | Place::Keyword => Handling::Open,
            };
            match handling {
                Handling::Open => return Handling::Open,
                Handling::Holds(w) => held = Some(w),
                _ => {}
            }
        }
        held.map_or(Handling::Reads, Handling::Holds)
    }

    /// The receiver of a call of the library method `symbol`: a builtin container's method that
    /// does not change it either reads it or gives a view of it; any other method is what Layer
    /// 1 lists for its first parameter.
    fn receiver_handling(&self, symbol: &str, dst: VarId) -> Handling {
        let container_method = symbol
            .rsplit_once('.')
            .filter(|(class, _)| CONTAINER_CLASSES.contains(class))
            .map(|(_, method)| method);
        match container_method {
            Some(m) if container_mutator(m) => Handling::Open,
            Some(m) if returns_container_view(m) => Handling::Holds(dst),
            Some(m) if result_detached_from_container(m) => Handling::Reads,
            _ => self.layer1(symbol, 0, dst),
        }
    }

    /// [`ContainerParams`] of the function's parameters annotated with a mutable container, with
    /// the facts of their uses when the body is `modeled`.
    fn container_params(&self, walks: &Walks<'_>, modeled: bool) -> Option<Box<ContainerParams>> {
        let resolve = |name: &str| self.cx.module.resolve_dotted(name, true);
        let annotated: Vec<(u8, ContainerKind)> = (0..64u8)
            .zip(&self.f.params)
            .filter(|(_, p)| !p.is_vararg && !p.is_kwarg)
            .filter_map(|(i, p)| {
                let kind = mutable_container(p.annotation.as_deref()?, &resolve)?;
                Some((i, kind))
            })
            .collect();
        let mask = annotated.iter().fold(0, |acc, (i, _)| acc | 1u64 << i);
        if mask == 0 || !modeled {
            return (mask != 0).then(|| {
                Box::new(ContainerParams {
                    annotated: mask,
                    ..ContainerParams::default()
                })
            });
        }
        let names: FxHashSet<&str> = annotated
            .iter()
            .map(|(i, _)| self.f.params[usize::from(*i)].name.as_str())
            .collect();
        let mut named: FxHashMap<&str, Vec<VarId>> = FxHashMap::default();
        for (v, name) in self.f.var_names.iter().enumerate() {
            if let Some(name) = name.as_deref().filter(|n| names.contains(n)) {
                let v = VarId(u32::try_from(v).expect("fewer than 2^32 values"));
                named.entry(name).or_default().push(v);
            }
        }
        let deleted: FxHashSet<&str> = self
            .f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|i| match &i.kind {
                InstKind::Delete { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .filter(|n| names.contains(n))
            .collect();
        let mut out = ContainerParams {
            annotated: mask,
            ..ContainerParams::default()
        };
        for (i, kind) in annotated {
            let bit = 1u64 << i;
            let param = &self.f.params[usize::from(i)];
            let bindings = if deleted.contains(param.name.as_str()) {
                None
            } else {
                let others = named
                    .get(param.name.as_str())
                    .map_or(&[][..], Vec::as_slice);
                self.default_bindings(i, kind, others)
            };
            if bindings.is_some() {
                out.bound_once |= bit;
            }
            let itself = bindings.unwrap_or_default();
            if self.abstract_use(walks, i, kind, &itself) {
                out.abstract_use |= bit;
            }
        }
        Some(Box::new(out))
    }

    /// The bindings of parameter `i`'s name besides the parameter itself, `others`, when each
    /// is `if p is None: p = <a new container of its kind>` or a join of such a binding with
    /// the parameter: those values (empty when there is none). `None` for any other binding.
    fn default_bindings(&self, i: u8, kind: ContainerKind, others: &[VarId]) -> Option<Vec<VarId>> {
        let param = self.f.params[usize::from(i)].var;
        // A join takes each of its inputs from the parameter or from another binding of the
        // name, itself checked here.
        let joins_bindings = |bb: BlockId, j: u32| {
            (self.f.block(bb).phis[j as usize].incoming.iter())
                .all(|(_, x)| *x == param || others.contains(x))
        };
        let mut bound = Vec::new();
        for &u in others.iter().filter(|u| **u != param) {
            match self.shape.def(u) {
                // A value the builder numbered and left out (a phi it found to be one value).
                Def::None => {}
                Def::Phi(bb, j) if joins_bindings(bb, j) => bound.push(u),
                Def::Inst(bb, j) if self.is_none_default(param, kind, bb, j as usize) => {
                    bound.push(u);
                }
                _ => return None,
            }
        }
        Some(bound)
    }

    /// Whether the instruction at `(bb, j)` binds the parameter's name to a new container of its
    /// kind, in the block a `p is None` test of the parameter itself branches to.
    fn is_none_default(&self, param: VarId, kind: ContainerKind, bb: BlockId, j: usize) -> bool {
        let InstKind::Copy { src, .. } = &self.f.block(bb).insts[j].kind else {
            return false;
        };
        let new = match self.shape.def(*src) {
            Def::Inst(b, at) => match &self.f.block(b).insts[at as usize].kind {
                InstKind::AllocBuiltin { kind: built, .. } => matches!(
                    (built, kind),
                    (BuiltinKind::List, ContainerKind::List)
                        | (BuiltinKind::Dict, ContainerKind::Dict)
                        | (BuiltinKind::Set, ContainerKind::Set)
                ),
                InstKind::DictLiteral { .. } => kind == ContainerKind::Dict,
                InstKind::Call { .. } => matches!(
                    self.callees.get(&(b, at as usize)),
                    Some(Callee::External(symbol)) if symbol == kind.class()
                ),
                _ => false,
            },
            _ => false,
        };
        new && self.shape.preds(bb).iter().any(|&pred| {
            matches!(
                self.f.block(pred).terminator,
                Terminator::Branch { cond, then_bb, .. }
                    if then_bb == bb && self.tests_none(cond, param)
            )
        })
    }

    /// Whether `cond` is `param is None` (or `None is param`).
    fn tests_none(&self, cond: VarId, param: VarId) -> bool {
        let Some(InstKind::Compare {
            op: CompareOpKind::Is,
            lhs,
            rhs,
            ..
        }) = self.shape.inst(cond).map(|i| &i.kind)
        else {
            return false;
        };
        let none = |x: &VarId| {
            matches!(
                self.shape.inst(*x).map(|i| &i.kind),
                Some(InstKind::Const {
                    literal: heki_ir::ConstLiteral::None,
                    ..
                })
            )
        };
        (*lhs == param && none(rhs)) || (*rhs == param && none(lhs))
    }

    /// Whether every use of parameter `i` (of `kind`), and of the bindings `itself` of its name
    /// that stand for it, is one the read-only type allows, and every value derived from it is
    /// only iterated, unpacked, tested, measured, or handed to a library call Layer 1 lists as
    /// reading it ([`ContainerParams::abstract_use`]); false past the walk budget.
    fn abstract_use(
        &self,
        walks: &Walks<'_>,
        i: u8,
        kind: ContainerKind,
        itself: &[VarId],
    ) -> bool {
        let param = self.f.params[usize::from(i)].var;
        let is_itself = |v: VarId| {
            self.param_alias.get(&v) == Some(&(i, None))
                || itself.contains(&v)
                || itself.contains(&self.shape.copy_root(v))
        };
        let failed = Cell::new(false);
        let seeds = std::iter::once(param)
            .chain(itself.iter().copied())
            .map(|v| (v, false));
        let found = reach(
            &walks.budget,
            seeds,
            |(v, derived), next| {
                for &u in self.shape.readers(v) {
                    match self.abstract_step(walks, kind, v, derived, u) {
                        Some(None) => {}
                        Some(Some(w)) => next.push((w, derived || !is_itself(w))),
                        None => failed.set(true),
                    }
                }
            },
            |_| match failed.get() {
                true => Visit::Stop(()),
                false => Visit::Expand,
            },
        );
        !failed.get() && matches!(found, Answer::Known(None))
    }

    /// What the use `u` of `v` (a value standing for a parameter of `kind`, or `derived` from
    /// it) needs of the read-only type: `Some(None)` when it allows the use, `Some(Some(w))`
    /// when `w` derives from `v` (a phi, a copy, a slice, a set operation), `None` when it does
    /// not allow it.
    fn abstract_step(
        &self,
        walks: &Walks<'_>,
        kind: ContainerKind,
        v: VarId,
        derived: bool,
        u: Use,
    ) -> Option<Option<VarId>> {
        let iterates = kind != ContainerKind::Set;
        let (bb, i) = match u {
            Use::Phi(bb, i) => return Some(Some(self.f.block(bb).phis[i as usize].dst)),
            Use::Terminator(bb) => {
                return matches!(self.f.block(bb).terminator, Terminator::Branch { cond, .. } if cond == v)
                    .then_some(None);
            }
            Use::Inst(bb, i) => (bb, i as usize),
        };
        let inst = &self.f.block(bb).insts[i];
        let allowed = match &inst.kind {
            InstKind::Copy { dst, .. } if self.f.var_name(*dst) == Some(UNPACKED) => iterates,
            InstKind::Copy { dst, .. } => return Some(Some(*dst)),
            InstKind::MakeIter { .. } | InstKind::YieldFrom { .. } => iterates,
            InstKind::MakeGenExpr { outer_iter, .. } => *outer_iter == v && iterates,
            InstKind::IterNext { .. } | InstKind::Delete { .. } => true,
            InstKind::SubscriptLoad {
                obj,
                index,
                is_slice,
                ..
            } if *obj == v => {
                if kind == ContainerKind::Set {
                    false
                } else if crate::consumption::is_unpacking(self.shape, *index, inst.span) {
                    true
                } else if *is_slice && kind == ContainerKind::List {
                    return Some(inst.kind.dst());
                } else {
                    !derived
                }
            }
            InstKind::SubscriptLoad { .. } => true,
            InstKind::Compare { op, .. } => match op {
                CompareOpKind::In
                | CompareOpKind::NotIn
                | CompareOpKind::Is
                | CompareOpKind::IsNot => true,
                CompareOpKind::Eq | CompareOpKind::NotEq => !derived,
                _ => !derived && kind == ContainerKind::Set,
            },
            InstKind::UnaryOp { op, .. } => *op == UnaryOpKind::Not,
            InstKind::BinOp {
                lhs, in_place: true, ..
            } => *lhs != v && iterates,
            InstKind::BinOp { op, dst, .. } => {
                let set_op = matches!(
                    op,
                    BinOpKind::BitOr | BinOpKind::BitAnd | BinOpKind::Sub | BinOpKind::BitXor
                );
                if kind == ContainerKind::Set && set_op {
                    return Some(Some(*dst));
                }
                false
            }
            InstKind::Convert { .. } | InstKind::Format { .. } | InstKind::BuildString { .. } => {
                !derived
            }
            // `{**p}` reads a mapping by its keys.
            InstKind::DictLiteral { entries, .. } => {
                !derived
                    && kind == ContainerKind::Dict
                    && entries
                        .iter()
                        .all(|e| !matches!(e, DictEntry::KeyValue { key, value } if *key == v || *value == v))
            }
            InstKind::Call { callee_var, .. } if *callee_var == Some(v) => false,
            kind_ => CallOperands::of(kind_, (bb, i))
                .is_some_and(|call| self.abstract_call(walks, kind, v, derived, &call)),
        };
        allowed.then_some(None)
    }

    /// Whether the read-only type allows the call `call` with `v` among its operands: a method
    /// of the read-only type on the value itself; an argument a library call Layer 1 lists as
    /// reading (for a set, one that takes no order from it); the value itself handed to a project
    /// call, whose parameter's annotation decides; an iteration by `*` unpacking.
    fn abstract_call(
        &self,
        walks: &Walks<'_>,
        kind: ContainerKind,
        v: VarId,
        derived: bool,
        call: &CallOperands<'_>,
    ) -> bool {
        let callee = self.callees.get(&call.at);
        let sets = kind == ContainerKind::Set;
        let offset = self.library_offset(call);
        let project = walks.calls_at.contains_key(&call.at);
        (call.places(self.shape, v).into_iter()).all(|place| match (place, callee) {
            (Place::Receiver, _) => {
                !derived
                    && call
                        .method
                        .is_some_and(|m| kind.read_only_methods().contains(&m))
            }
            (Place::Unpacked, _) => !sets,
            (Place::Mapping, _) => !derived && kind == ContainerKind::Dict,
            (Place::Position(j), Some(Callee::External(symbol))) => {
                (!sets || ORDER_FREE.contains(&symbol.as_str()))
                    && !matches!(self.layer1(symbol, j + offset, call.dst), Handling::Open)
            }
            (Place::Position(_) | Place::Keyword, Some(Callee::Project(..))) => !derived && project,
            _ => false,
        })
    }

    /// How many leading parameters of a library call no argument binds: one for a method of an
    /// instance, whose receiver Layer 1 counts as its first parameter.
    fn library_offset(&self, call: &CallOperands<'_>) -> usize {
        usize::from(
            call.recv
                .is_some_and(|r| matches!(self.val_in(call.at.0, r), Some(Val::Inst(_)))),
        )
    }
}

/// Where a call hands a value over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    /// As the receiver of a method call.
    Receiver,
    /// As the positional argument at this index.
    Position(usize),
    /// Unpacked with `*`: the call takes its elements.
    Unpacked,
    /// As a positional argument after one `*` unpacks: no known parameter binds it.
    AfterUnpacking,
    /// As a keyword argument.
    Keyword,
    /// Unpacked with `**`: the call takes the mapping's items.
    Mapping,
}

/// The operands of a `Call` or `MethodCall` instruction at `at`, other than the value it calls.
struct CallOperands<'i> {
    at: Position,
    dst: VarId,
    recv: Option<VarId>,
    method: Option<&'i str>,
    args: &'i [VarId],
    kwargs: &'i [(Option<String>, VarId)],
    star_from: Option<u8>,
}

impl<'i> CallOperands<'i> {
    fn of(kind: &'i InstKind, at: Position) -> Option<Self> {
        match kind {
            InstKind::Call {
                dst,
                args,
                kwargs,
                star_from,
                ..
            } => Some(Self {
                at,
                dst: *dst,
                recv: None,
                method: None,
                args,
                kwargs,
                star_from: *star_from,
            }),
            InstKind::MethodCall {
                dst,
                recv,
                method,
                args,
                kwargs,
                star_from,
                ..
            } => Some(Self {
                at,
                dst: *dst,
                recv: Some(*recv),
                method: Some(method),
                args,
                kwargs,
                star_from: *star_from,
            }),
            _ => None,
        }
    }

    /// Every place the call hands `v` over at.
    fn places(&self, shape: &FnShape<'_>, v: VarId) -> Vec<Place> {
        let receiver = (self.recv == Some(v)).then_some(Place::Receiver);
        let positional = self
            .args
            .iter()
            .enumerate()
            .filter(|(_, a)| **a == v)
            .map(|(j, _)| {
                if shape.is_starred(self.dst, j) {
                    Place::Unpacked
                } else if self.star_from.is_some_and(|s| j >= usize::from(s)) {
                    Place::AfterUnpacking
                } else {
                    Place::Position(j)
                }
            });
        let keywords = (self.kwargs.iter().filter(|(_, a)| *a == v)).map(|(name, _)| match name {
            Some(_) => Place::Keyword,
            None => Place::Mapping,
        });
        receiver
            .into_iter()
            .chain(positional)
            .chain(keywords)
            .collect()
    }
}
