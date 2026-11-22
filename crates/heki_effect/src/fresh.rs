//! `MUT119`'s facts: where a new builtin container comes from, and the functions every return of
//! which is one that nothing else holds.
//!
//! [`source`] names what makes the object an instruction defines: the builtin itself (a display,
//! a comprehension, a builtin constructor), a copy the catalog describes or a slice of a builtin
//! sequence, or a call of project functions (a property read calls its getter). A function's
//! [`FreshReturn`] says that every value it returns comes from one of them, with one container
//! kind, and that nothing the function does may keep it: walking back from the returned values
//! through copies and phis to where each is made, then forward from there over every use. An
//! object stored, captured, yielded, raised, handed to a call that may keep it, or read through
//! an attribute (a bound method holds its object) may be held elsewhere. The calls' own facts
//! come from propagation, which resolves the functions a local fact names.

use crate::decorators::keeps_returned_value;
use crate::ownership::Live;
use crate::readonly::ContainerKind;
use crate::resolver::is_generator_function;
use crate::{Callee, FuncKey, Val};
use heki_ir::walk::{Budget, Def, FnShape, Use, Visit, reach};
use heki_ir::{BlockId, BuiltinKind, FunctionKind, InstKind, Terminator, VarId};
use heki_kb::EffectiveEcosystemKb;
use serde::{Deserialize, Serialize};

/// What every value a function returns is ([`crate::EffectSummary::fresh_return`]): a builtin
/// container of one kind that the call made and that nothing else holds when it returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshReturn {
    /// `None` only in a local summary whose returned values all come from `calls`.
    pub kind: Option<ContainerKind>,
    /// Every returned container is built as the builtin class itself. A copy's class comes from
    /// the copied value's inferred type, which under R2 may be a project subclass's.
    pub built: bool,
    /// The project functions whose results the function returns, each of which must return a
    /// new container of the same kind (local: propagation resolves them, and a final summary
    /// names none).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<FuncKey>,
}

impl FreshReturn {
    /// Add one source of a returned value; `false` when its kind differs from the others'.
    fn add(&mut self, source: Source<'_>) -> bool {
        let kind = match source {
            Source::Calls(keys) => {
                self.calls.extend(keys.iter().cloned());
                return true;
            }
            Source::Built(kind) => kind,
            Source::Copied(kind) => {
                self.built = false;
                kind
            }
        };
        self.joins(kind)
    }

    /// Whether `kind` is the kind of the containers so far (setting it when there is none yet).
    pub fn joins(&mut self, kind: ContainerKind) -> bool {
        *self.kind.get_or_insert(kind) == kind
    }

    /// Join the final fact of a project function whose result is the container (`None` when it
    /// has none); `false` when there is none or its kind differs.
    pub fn join_callee(&mut self, callee: Option<&Self>) -> bool {
        let Some(callee) = callee else {
            return false;
        };
        self.built &= callee.built;
        callee.kind.is_some_and(|kind| self.joins(kind))
    }
}

/// Where a new builtin container an instruction defines comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source<'k> {
    /// Built as the builtin class itself: a display, a comprehension, or a call of the class or
    /// of `sorted`.
    Built(ContainerKind),
    /// A copy the catalog describes (`x.copy()`, `copy.copy(x)`, `os.environ.copy()`) or a
    /// slice of a list or `bytearray`, of the kind its inferred type names.
    Copied(ContainerKind),
    /// What these project functions return (the getters of a property read).
    Calls(&'k [FuncKey]),
}

/// The builtin calls that make a container of their own kind.
const CONSTRUCTORS: &[(&str, ContainerKind)] = &[
    ("builtins.list", ContainerKind::List),
    ("builtins.sorted", ContainerKind::List),
    ("builtins.dict", ContainerKind::Dict),
    ("builtins.set", ContainerKind::Set),
    ("builtins.bytearray", ContainerKind::Bytearray),
    ("collections.deque", ContainerKind::Deque),
];

/// The source of the object the instruction `inst` defines: `callee` is what a call (or a
/// property read) resolves to, `result` the defined value's inferred value, and `class_of` the
/// inferred class of an operand.
pub fn source<'k, 't>(
    inst: &InstKind,
    callee: Option<&'k Callee>,
    result: Option<&Val>,
    class_of: impl Fn(VarId) -> Option<&'t str>,
    kb: &EffectiveEcosystemKb,
) -> Option<Source<'k>> {
    let result_kind = || match result {
        Some(Val::Inst(class)) => ContainerKind::of_class(class),
        _ => None,
    };
    match (inst, callee) {
        (InstKind::AllocBuiltin { kind, .. }, _) => match kind {
            BuiltinKind::List => Some(Source::Built(ContainerKind::List)),
            BuiltinKind::Dict => Some(Source::Built(ContainerKind::Dict)),
            BuiltinKind::Set => Some(Source::Built(ContainerKind::Set)),
            _ => None,
        },
        (InstKind::DictLiteral { .. }, _) => Some(Source::Built(ContainerKind::Dict)),
        (
            InstKind::SubscriptLoad {
                obj,
                is_slice: true,
                ..
            },
            _,
        ) => class_of(*obj)
            .and_then(ContainerKind::of_class)
            .filter(|k| matches!(k, ContainerKind::List | ContainerKind::Bytearray))
            .map(Source::Copied),
        // A call of a class returns the new instance, not what its `__init__` returns, which is
        // `None` in a program that runs (a value makes the construction raise `TypeError`).
        (
            InstKind::Call { .. } | InstKind::MethodCall { .. } | InstKind::AttrLoad { .. },
            Some(Callee::Project(keys, _)),
        ) if !keys.is_empty() => (!keys.iter().any(is_init)).then_some(Source::Calls(keys)),
        (InstKind::Call { .. } | InstKind::MethodCall { .. }, Some(Callee::External(symbol))) => {
            if let Some((_, kind)) = CONSTRUCTORS.iter().find(|(s, _)| s == symbol) {
                return Some(Source::Built(*kind));
            }
            kb.lookup(symbol)
                .and_then(|e| e.copy_sanitizer)
                .and_then(|_| result_kind())
                .map(Source::Copied)
        }
        _ => None,
    }
}

fn is_init(k: &FuncKey) -> bool {
    k.qualname.rsplit('.').next() == Some("__init__")
}

/// The function's local [`FreshReturn`], when every value it returns is a new builtin container
/// that nothing in it may keep. `source_of` is [`source`] of the instruction defining a value,
/// and `inspects(block, index)` says whether the call there only inspects its arguments.
pub(crate) fn fresh_return<'k>(
    shape: &FnShape<'_>,
    live: &Live,
    source_of: impl Fn(VarId) -> Option<Source<'k>>,
    inspects: impl Fn(BlockId, u32) -> bool,
) -> Option<FreshReturn> {
    let f = shape.f;
    if f.kind != FunctionKind::Sync
        || f.is_module_body
        || f.is_class_body
        || is_generator_function(f)
        || !keeps_returned_value(&f.decorators)
        || !f.a1_hazards.is_empty()
    {
        return None;
    }
    let mut returned = Vec::new();
    for b in f.blocks.iter().filter(|b| live.block(b.id)) {
        match b.terminator {
            Terminator::Return(Some(v)) => returned.push(v),
            Terminator::Return(None) => return None,
            _ => {}
        }
    }
    let budget = Budget::of_function("MUT119 fresh return", f);
    let mut fact = FreshReturn {
        kind: None,
        built: true,
        calls: Vec::new(),
    };
    let mut made = Vec::new();
    let passes = |v: VarId| match shape.def(v) {
        Def::Phi(..) => true,
        _ => matches!(shape.inst(v).map(|i| &i.kind), Some(InstKind::Copy { .. })),
    };
    // Back through copies and phis (along the edges a run can take) to where each value is made.
    let back = reach(
        &budget,
        returned,
        |v, out| match shape.def(v) {
            Def::Phi(b, i) => out.extend(
                f.block(b).phis[i as usize]
                    .incoming
                    .iter()
                    .filter(|(from, _)| live.edge(*from, b))
                    .map(|(_, x)| *x),
            ),
            _ => out.extend(shape.inst(v).iter().flat_map(|i| i.kind.operands())),
        },
        |v| {
            if passes(v) {
                return Visit::Expand;
            }
            match source_of(v) {
                Some(s) if fact.add(s) => {
                    made.push(v);
                    Visit::Prune
                }
                _ => Visit::Stop(()),
            }
        },
    );
    if back.open_means(Some(())).is_some() || fact.kind.is_none() && fact.calls.is_empty() {
        return None;
    }
    // Forward from where they are made, through copies, phis, and in-place operators (which
    // return their left operand): every use must keep nothing.
    let held = reach(
        &budget,
        made,
        |v, out| {
            for u in shape.readers(v) {
                match *u {
                    Use::Phi(b, i) => out.push(f.block(b).phis[i as usize].dst),
                    Use::Inst(b, i) => match &f.block(b).insts[i as usize].kind {
                        InstKind::Copy { dst, .. } => out.push(*dst),
                        InstKind::BinOp {
                            dst,
                            in_place: true,
                            ..
                        } => out.push(*dst),
                        _ => {}
                    },
                    Use::Terminator(_) => {}
                }
            }
        },
        |v| {
            if shape
                .readers(v)
                .iter()
                .all(|u| keeps_nothing(shape, *u, v, &inspects))
            {
                Visit::Expand
            } else {
                Visit::Stop(())
            }
        },
    );
    if held.open_means(Some(())).is_some() {
        return None;
    }
    fact.calls.sort();
    fact.calls.dedup();
    // The local summary keeps the list for the whole run.
    fact.calls.shrink_to_fit();
    Some(fact)
}

/// Whether the use `u` of `v` keeps the object `v` holds nowhere but in the values the forward
/// walk follows.
fn keeps_nothing(
    shape: &FnShape<'_>,
    u: Use,
    v: VarId,
    inspects: &impl Fn(BlockId, u32) -> bool,
) -> bool {
    let f = shape.f;
    let (b, i) = match u {
        Use::Phi(..) => return true,
        Use::Terminator(b) => return !matches!(f.block(b).terminator, Terminator::Raise(_)),
        Use::Inst(b, i) => (b, i),
    };
    // `v` is the operand `at` and none of `others`.
    let only =
        |at: &VarId, mut others: std::slice::Iter<'_, VarId>| *at == v && others.all(|o| *o != v);
    let passes = |kwargs: &[(Option<String>, VarId)]| kwargs.iter().all(|(_, a)| *a != v);
    match &f.block(b).insts[i as usize].kind {
        InstKind::Copy { .. }
        | InstKind::Compare { .. }
        | InstKind::UnaryOp { .. }
        | InstKind::Convert { .. }
        | InstKind::Format { .. }
        | InstKind::MakeIter { .. }
        | InstKind::SubscriptDelete { .. }
        | InstKind::Delete { .. } => true,
        // A builtin container's own methods keep none of their receiver.
        InstKind::MethodCall {
            recv, args, kwargs, ..
        } => only(recv, args.iter()) && passes(kwargs),
        InstKind::Call {
            callee_var, kwargs, ..
        } => *callee_var != Some(v) && passes(kwargs) && inspects(b, i),
        InstKind::SubscriptLoad { obj, index, .. } => only(obj, std::slice::from_ref(index).iter()),
        InstKind::SubscriptStore {
            obj, index, value, ..
        } => only(obj, [*index, *value].iter()),
        InstKind::BinOp {
            lhs,
            rhs,
            in_place: true,
            ..
        } => only(lhs, std::slice::from_ref(rhs).iter()),
        _ => false,
    }
}
