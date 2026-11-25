//! The local part of [`crate::EffectSummary::only_read_params`], of
//! [`crate::EffectSummary::read_kinds`], and of [`crate::ContainerParams`]: walks over the uses
//! of the values a parameter's object may be.
//! A change of the object itself is [`crate::EffectSummary::mutated_param_paths`]' (the
//! `MUT110` judgment); these walks find what else may keep it, change it through a value that
//! may be it, or hand it to code heki does not follow.

use super::{Built, Extractor, Position, ProjectCall};
use crate::order_flow::ORDER_FREE_BUILTINS;
use crate::ownership;
use crate::readonly::{
    ContainerFamily, ContainerKind, container_mutator, result_detached_from_container,
    returns_container_view,
};
use crate::resolver::{Callee, Val, func_qualname, mutable_container};
use crate::{ContainerParams, ReadKinds};
use heki_ir::walk::{Answer, Budget, Def, FnShape, Use, Visit, reach};
use heki_ir::{
    BinOpKind, BlockId, BuiltinKind, CompareOpKind, DictEntry, FunctionId, InstKind, Terminator,
    UNPACKED, UnaryOpKind, VarId,
};
use heki_kb::SoundnessParamEffect;
use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::Cell;
use std::collections::BTreeMap;

/// What one use of a value does with the object the value may be, with the kinds of read it
/// makes of it.
enum Handling {
    /// Only reads it: a test, an item read, an iteration, a call Layer 1 lists as reading it.
    Reads(ReadKinds),
    /// The value it defines holds the object too: a copy, a phi, a view of it (an iterator, a
    /// `dict` view, a generator over it).
    Holds(VarId, ReadKinds),
    /// Hands it to project callees, each binding it to a parameter: their facts decide, when
    /// propagation joins them. A generator or a coroutine the call creates holds it too.
    Bound(Option<VarId>, ReadKinds),
    /// Anything else: the object may be changed, kept, or handed to code heki does not follow.
    Open,
}

impl Handling {
    fn holds(w: VarId) -> Self {
        Self::Holds(w, ReadKinds::NONE)
    }
}

/// The project calls of the function by position, what the shape tests of its patterns read of
/// their subjects, and the budget of the walks over its values.
struct Walks<'x> {
    calls_at: FxHashMap<Position, &'x ProjectCall>,
    patterns: FxHashMap<VarId, ReadKinds>,
    budget: Budget,
}

impl<'a> Extractor<'a> {
    /// [`crate::EffectSummary::only_read_params`] and [`crate::EffectSummary::container_params`]
    /// of the function's own body. A stub's body and one with an A1 hazard (`eval`, `locals()`,
    /// a body too large to model) establish nothing.
    pub(super) fn only_read_facts(&mut self) {
        let f = self.f;
        let mut patterns: FxHashMap<VarId, ReadKinds> = FxHashMap::default();
        for &(subject, shape) in &f.pattern_subjects {
            *patterns.entry(subject).or_default() |= ReadKinds::of_pattern(shape);
        }
        let walks = Walks {
            calls_at: self.project_calls.iter().map(|c| (c.at, c)).collect(),
            patterns,
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
            match self.only_read_from(&walks, k, seeds) {
                Some(kinds) => self.summary.add_read_kinds(k, kinds),
                None => only &= !(1u64 << k),
            }
        }
        let containers = self.container_params(&walks, true);
        self.summary.only_read_params = only;
        self.summary.container_params = containers;
    }

    /// The kinds of read the uses of `seeds` (the values that may be the object of position
    /// `k`), and of the values holding it, make of the object; `None` when one leaves it open,
    /// and past the walk budget.
    fn only_read_from(&self, walks: &Walks<'_>, k: u8, seeds: Vec<VarId>) -> Option<ReadKinds> {
        let open = Cell::new(false);
        let kinds = Cell::new(ReadKinds::NONE);
        let found = reach(
            &walks.budget,
            seeds,
            |v, next| {
                let tested = walks.patterns.get(&v).copied().unwrap_or_default();
                kinds.set(kinds.get() | tested);
                for &u in self.shape.readers(v) {
                    let (held, read) = match self.handling(walks, k, v, u) {
                        Handling::Reads(read) | Handling::Bound(None, read) => (None, read),
                        Handling::Holds(w, read) | Handling::Bound(Some(w), read) => {
                            (Some(w), read)
                        }
                        Handling::Open => {
                            open.set(true);
                            continue;
                        }
                    };
                    kinds.set(kinds.get() | read);
                    next.extend(held);
                }
            },
            |_| match open.get() {
                true => Visit::Stop(()),
                false => Visit::Expand,
            },
        );
        (!open.get() && matches!(found, Answer::Known(None))).then(|| kinds.get())
    }

    /// What the use `u` of `v`, a value that may be the object of position `k`, does with it.
    fn handling(&self, walks: &Walks<'_>, k: u8, v: VarId, u: Use) -> Handling {
        let (bb, i) = match u {
            Use::Phi(bb, i) => return Handling::holds(self.f.block(bb).phis[i as usize].dst),
            Use::Terminator(bb) => {
                return match self.f.block(bb).terminator {
                    Terminator::Branch { cond, .. } if cond == v => {
                        Handling::Reads(ReadKinds::SIZE)
                    }
                    _ => Handling::Open,
                };
            }
            Use::Inst(bb, i) => (bb, i as usize),
        };
        let inst = &self.f.block(bb).insts[i];
        match &inst.kind {
            InstKind::Copy { dst, .. } if self.f.var_name(*dst) == Some(UNPACKED) => {
                Handling::Reads(ReadKinds::UNPACKING)
            }
            InstKind::Copy { dst, .. } => Handling::holds(*dst),
            InstKind::BinOp {
                lhs,
                in_place: true,
                ..
            } if *lhs == v => Handling::Open,
            InstKind::SubscriptDelete { obj, .. } if *obj == v => Handling::Open,
            // An operator, a comparison, or an item access with the object as the key runs a
            // special method of the other operand's class, which gets the object.
            InstKind::BinOp { lhs, rhs, .. } => {
                self.operand_handling(bb, v, *lhs, *rhs, ReadKinds::OPERAND)
            }
            InstKind::Compare { op, lhs, rhs, .. } => match op {
                CompareOpKind::Is | CompareOpKind::IsNot => Handling::Reads(ReadKinds::NONE),
                CompareOpKind::In | CompareOpKind::NotIn if *rhs == v => {
                    Handling::Reads(ReadKinds::MEMBERSHIP)
                }
                _ => self.operand_handling(bb, v, *lhs, *rhs, ReadKinds::COMPARISON),
            },
            InstKind::SubscriptLoad { obj, index, .. }
                if *obj == v && crate::consumption::is_unpacking(self.shape, *index, inst.span) =>
            {
                Handling::Reads(ReadKinds::TARGET_UNPACKING)
            }
            InstKind::SubscriptLoad { obj, .. } if *obj == v && self.item_read_changes(bb, v) => {
                Handling::Open
            }
            // A key is hashed and compared, an index converted (`__index__`).
            InstKind::SubscriptLoad { obj, index, .. } if *obj == v => {
                self.key_handling(bb, *index)
            }
            InstKind::SubscriptLoad { obj, index, .. }
            | InstKind::SubscriptDelete { obj, index, .. } => self.operand_handling(
                bb,
                v,
                *obj,
                *index,
                ReadKinds::COMPARISON | ReadKinds::OPERATORS,
            ),
            InstKind::UnaryOp { op, .. } => Handling::Reads(match op {
                UnaryOpKind::Not => ReadKinds::SIZE,
                _ => ReadKinds::OPERATORS,
            }),
            InstKind::Convert { .. } | InstKind::Format { .. } | InstKind::BuildString { .. } => {
                Handling::Reads(ReadKinds::TEXT)
            }
            InstKind::MakeIter { .. } | InstKind::YieldFrom { .. } => {
                Handling::Reads(ReadKinds::ITERATION)
            }
            // An iterator over the object, or a view of it, steps through the builtin's own
            // storage; a `del` of a name reads nothing.
            InstKind::IterNext { .. } | InstKind::Delete { .. } => Handling::Reads(ReadKinds::NONE),
            // `{**p}` reads the mapping's items into a new dict.
            InstKind::DictLiteral { entries, .. } => {
                let held = entries.iter().any(
                    |e| matches!(e, DictEntry::KeyValue { key, value } if *key == v || *value == v),
                );
                match held {
                    true => Handling::Open,
                    false => Handling::Reads(ReadKinds::MAPPING_UNPACK),
                }
            }
            // A generator expression holds an iterator over its outermost iterable, which it
            // makes where it is created, and the values it captures, which its body reads where
            // it runs.
            InstKind::MakeGenExpr {
                dst,
                func_id,
                captured,
                outer_iter,
                ..
            } => {
                if captured.contains(&v) && !self.child_reads_parameter(k, v, *func_id) {
                    return Handling::Open;
                }
                let read = match *outer_iter == v {
                    true => ReadKinds::ITERATION,
                    false => ReadKinds::NONE,
                };
                Handling::Holds(*dst, read)
            }
            InstKind::Call { callee_var, .. } if *callee_var == Some(v) => Handling::Open,
            kind => CallOperands::of(kind, (bb, i)).map_or(Handling::Open, |call| {
                self.call_handling(walks, k, v, &call)
            }),
        }
    }

    /// An operation on `a` and `b`, one of them `v`: the other operand's special method is run
    /// with the object (`other == p` runs `type(other).__eq__(other, p)` first), and may keep or
    /// change it, unless the operand only runs a builtin's ([`Self::builtin_operand`]).
    fn operand_handling(
        &self,
        bb: BlockId,
        v: VarId,
        a: VarId,
        b: VarId,
        read: ReadKinds,
    ) -> Handling {
        let other = if a == v { b } else { a };
        match self.builtin_operand(bb, other) {
            true => Handling::Reads(read),
            false => Handling::Open,
        }
    }

    /// Whether the operand `other` of an operation on the object at `bb` runs a builtin's special
    /// methods, which only read the object: it is built here as a builtin value of its exact
    /// class, or each class it may be is a builtin class no project class derives from (R2). A
    /// value of unknown class, of `object`, or of a library class may run code that keeps it.
    fn builtin_operand(&self, bb: BlockId, other: VarId) -> bool {
        if self.built(self.shape.copy_root(other)) != Built::Unknown {
            return true;
        }
        let builtin = |c: &String| {
            c.starts_with("builtins.")
                && c != "builtins.object"
                && self.cx.project.value_classes(c).is_empty()
        };
        let classes = self.instance_classes(bb, other);
        !classes.is_empty() && classes.iter().all(builtin)
    }

    /// The classes `v` may be an instance of at `bb` ([`Val::instance_classes`]).
    fn instance_classes(&self, bb: BlockId, v: VarId) -> &[String] {
        self.val_in(bb, v).map_or(&[], Val::instance_classes)
    }

    /// The key `index` of an item read of the object: the key's own methods (`__hash__`,
    /// `__eq__`, `__index__`) get the key and the keys the object holds, not the object; a key
    /// that may be an instance of a project class leaves it open.
    fn key_handling(&self, bb: BlockId, index: VarId) -> Handling {
        let project = |c: &String| self.cx.project.is_project_class(c);
        match self.instance_classes(bb, index).iter().any(project) {
            true => Handling::Open,
            false => Handling::Reads(ReadKinds::ITEM),
        }
    }

    /// Whether an item read of `v` may change the object: a class it may be has a base binding
    /// a read of an item that changes it ([`ContainerFamily::changing_read`]: a `defaultdict`'s
    /// `__missing__` inserts the key).
    fn item_read_changes(&self, bb: BlockId, v: VarId) -> bool {
        let item: Vec<&str> = ReadKinds::ITEM.methods().collect();
        (self.instance_classes(bb, v).iter()).any(|class| {
            (self.cx.project.mro_of(class).iter())
                .any(|base| ContainerFamily::changing_read(base, &item).is_some())
        })
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
        if self.hands_back(v, call) {
            return Handling::holds(call.dst);
        }
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
        // The callees' kinds of read join where propagation joins their facts; what the call
        // reads itself is what it unpacks.
        let unpacked = places
            .iter()
            .fold(ReadKinds::NONE, |read, p| read | p.unpacking());
        Handling::Bound(creates.then_some(call.dst), unpacked)
    }

    /// What Layer 1 lists the library call `symbol` as doing with its argument at `position`
    /// (with no entry, it may keep or change it), for the result `dst` of the call; `read` gives
    /// the kinds of read of an argument it reads.
    fn layer1(
        &self,
        symbol: &str,
        position: usize,
        dst: VarId,
        read: impl FnOnce() -> ReadKinds,
    ) -> Handling {
        let effect = (self.cx.layer1)
            .and_then(|kb| kb.lookup(symbol))
            .and_then(|e| {
                e.param_effects
                    .get(position)
                    .or_else(|| e.param_effects.last())
            });
        match effect {
            Some(SoundnessParamEffect::ReadOnly) => Handling::Reads(read()),
            Some(SoundnessParamEffect::RetainedInReturn) => Handling::Holds(dst, read()),
            _ => Handling::Open,
        }
    }

    /// A call of a library function or method `symbol`: what Layer 1 lists it as doing with the
    /// argument, or for the receiver of a builtin container's method, what the method does.
    fn library_handling(&self, v: VarId, call: &CallOperands<'_>, symbol: &str) -> Handling {
        let offset = self.library_offset(call);
        let invoked = |j: usize| {
            u8::try_from(j).map_or(ReadKinds::ALL, |j| {
                ReadKinds::of_library_call(self.cx.kb, symbol, j)
            })
        };
        let mut held = None;
        let mut kinds = ReadKinds::NONE;
        for place in call.places(self.shape, v) {
            let handling = match place {
                Place::Receiver => self.receiver_handling(symbol, call.dst),
                Place::Position(j) => self.layer1(symbol, j + offset, call.dst, || invoked(j)),
                Place::Unpacked | Place::Mapping => Handling::Reads(place.unpacking()),
                Place::AfterUnpacking | Place::Keyword => Handling::Open,
            };
            match handling {
                Handling::Open => return Handling::Open,
                Handling::Holds(w, read) => {
                    held = Some(w);
                    kinds |= read;
                }
                Handling::Reads(read) | Handling::Bound(_, read) => kinds |= read,
            }
        }
        held.map_or(Handling::Reads(kinds), |w| Handling::Holds(w, kinds))
    }

    /// The receiver of a call of the library method `symbol`: a method of a builtin container,
    /// of its `typing` alias, or of its ABCs (read as the builtin's) that does not change it
    /// either reads it or gives a view of it; any other method is what Layer 1 lists for its
    /// first parameter, read in every kind.
    fn receiver_handling(&self, symbol: &str, dst: VarId) -> Handling {
        let container_method = symbol
            .rsplit_once('.')
            .filter(|(class, _)| ContainerFamily::read_as_builtin(class).is_some())
            .map(|(_, method)| method);
        match container_method {
            Some(m) if container_mutator(m) => Handling::Open,
            Some(m) if returns_container_view(m) => {
                Handling::Holds(dst, ReadKinds::of_method(m, true))
            }
            Some(m) if result_detached_from_container(m) => {
                Handling::Reads(ReadKinds::of_method(m, true))
            }
            _ => self.layer1(symbol, 0, dst, || ReadKinds::ALL),
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
            kind_ => match CallOperands::of(kind_, (bb, i)) {
                Some(call) if self.hands_back(v, &call) => return Some(Some(call.dst)),
                Some(call) => self.abstract_call(walks, kind, v, derived, &call),
                None => false,
            },
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
                (!sets || ORDER_FREE_BUILTINS.contains(&symbol.as_str()))
                    && !matches!(
                        self.layer1(symbol, j + offset, call.dst, || ReadKinds::NONE),
                        Handling::Open
                    )
            }
            (Place::Position(_) | Place::Keyword, Some(Callee::Project(..))) => !derived && project,
            _ => false,
        })
    }

    /// Whether `call` is a library call handing `v` back as itself, `typing.cast(T, v)`
    /// ([`ownership::handed_back`]): its result is the object, and the call does nothing else
    /// with it.
    fn hands_back(&self, v: VarId, call: &CallOperands<'_>) -> bool {
        matches!(
            self.callees.get(&call.at),
            Some(Callee::External(symbol))
                if ownership::handed_back(symbol, call.args, call.star_from) == Some(v)
        )
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

impl Place {
    /// What the call reads of a value it unpacks here ([`ReadKinds::UNPACKING`],
    /// [`ReadKinds::MAPPING_UNPACK`]); none where it hands the value over whole.
    fn unpacking(self) -> ReadKinds {
        match self {
            Self::Unpacked => ReadKinds::UNPACKING,
            Self::Mapping => ReadKinds::MAPPING_UNPACK,
            _ => ReadKinds::NONE,
        }
    }
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
