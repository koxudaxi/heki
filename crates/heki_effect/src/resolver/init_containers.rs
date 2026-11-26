//! The containers a class's own `__init__` makes for the instance
//! ([`super::ClassDecl::init_containers`]): an attribute it binds to a new builtin container,
//! which the instance alone holds when the constructor returns. A function constructing such an
//! instance counts what it adds to that container as held by the instance (`fresh-objects`,
//! `crate::construction`).

use super::{ModuleDecls, is_summarized_function, self_aliases};
use crate::ModuleInput;
use crate::readonly::ContainerKind;
use heki_ir::walk::{Answer, Budget, Def, FnShape, Use, Visit, reach};
use heki_ir::{BlockId, BuiltinKind, FunctionKind, InstKind, IrFunction, Terminator, VarId};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeMap;

/// A point of a function: block, instruction index.
type At = (BlockId, usize);

/// [`super::ClassDecl::init_containers`] of the module's classes, from their own `__init__`:
/// a plain method (no decorator, which may replace it) taking the instance as its first
/// parameter, the only binding of the name in the class body.
pub(super) fn init_containers(input: &ModuleInput<'_>, decls: &mut ModuleDecls) {
    let functions = &input.ir.functions;
    // The class bodies binding `__init__` more than once (a second `def`, an assignment).
    let rebinding: FxHashSet<&str> = (functions.iter().filter(|f| f.is_class_body))
        .filter(|body| {
            let binds = |i: &&heki_ir::Inst| {
                i.kind.dst().and_then(|d| body.var_name(d)) == Some("__init__")
            };
            body.blocks
                .iter()
                .flat_map(|b| &b.insts)
                .filter(binds)
                .count()
                > 1
        })
        .map(|body| body.qualname.as_str())
        .collect();
    for f in functions {
        let (Some(owner), Some(this)) = (&f.owner_class_qualname, f.params.first()) else {
            continue;
        };
        if f.name != "__init__"
            || f.qualname != format!("{owner}.__init__")
            || rebinding.contains(owner.as_str())
            || f.kind != FunctionKind::Sync
            || !is_summarized_function(f)
            || !f.decorators.is_empty()
            || this.is_vararg
            || this.is_kwarg
            || this.kw_only
        {
            continue;
        }
        // The builtin container class a global name the module does not bind reads.
        let builtin = |name: &str| {
            let class = format!("builtins.{name}");
            (decls.resolve_dotted(name, false).as_deref() == Some(class.as_str()))
                .then(|| ContainerKind::of_class(&class))
                .flatten()
        };
        let found = made_containers(f, this.var, &builtin);
        if let Some(class) = decls.classes.get_mut(owner) {
            class.init_containers = found;
        }
    }
}

/// The attributes `f` binds, through its parameter `this`, to a builtin container it makes, and
/// that the instance alone holds when `f` returns:
/// - the container is a list, dict, or set display, or a call of a builtin container class
///   (`list`, `dict`, `set`, `bytearray`) without arguments, and the store is its only use;
/// - the store is the only one of the attribute through `this`, `f` never reads or deletes the
///   attribute through it, and every path to a `return` passes the store;
/// - from the store on, no code may reach the instance: no instruction other than a store of
///   another attribute through `this` uses a value that may hold the instance (`this`, what is
///   read out of it, `super()`, and what any of those flows into), so nothing `f` calls may
///   rebind the attribute or keep the container.
fn made_containers(
    f: &IrFunction,
    this: VarId,
    builtin: &dyn Fn(&str) -> Option<ContainerKind>,
) -> Box<[(Box<str>, ContainerKind)]> {
    let shape = FnShape::new(f);
    let selfs = self_aliases(f, this);
    // Past the budget, no attribute is known to hold one.
    let budget = Budget::of_function("resolver::init_containers", f);
    let found = holding_values(&shape, &selfs, &budget).and_then(|holding| {
        let mut stores: BTreeMap<&str, Vec<(At, VarId)>> = BTreeMap::new();
        let mut used: FxHashSet<&str> = FxHashSet::default();
        // By block, the last point that uses a value that may hold the instance, other than as
        // the object of an attribute store.
        let mut exposed: FxHashMap<BlockId, usize> = FxHashMap::default();
        for bb in &f.blocks {
            for (i, inst) in bb.insts.iter().enumerate() {
                let at = (bb.id, i);
                let exposes = match &inst.kind {
                    InstKind::AttrStore { obj, attr, value } if selfs.contains(obj) => {
                        stores.entry(attr).or_default().push((at, *value));
                        holding.contains(value)
                    }
                    InstKind::AttrDelete { obj, attr } | InstKind::AttrLoad { obj, attr, .. }
                        if selfs.contains(obj) =>
                    {
                        used.insert(attr);
                        true
                    }
                    InstKind::Copy { .. } => false,
                    kind => {
                        kind.operands().iter().any(|v| holding.contains(v))
                            || matches!(kind, InstKind::Call { callee, .. } if callee == "super")
                    }
                };
                if exposes {
                    exposed.insert(bb.id, i);
                }
            }
        }
        Paths::of(&shape, &exposed, &budget).map(|paths| {
            let mut out = Vec::new();
            for (attr, stored) in stores {
                let [(at, value)] = stored[..] else {
                    continue;
                };
                let Some(kind) = made_container(&shape, value, at, builtin) else {
                    continue;
                };
                if !used.contains(attr) && paths.on_every_return(at) && !paths.exposed_after(at) {
                    out.push((attr.into(), kind));
                }
            }
            out.into_boxed_slice()
        })
    });
    found.open_means(Box::default())
}

/// What the paths of a function say about its points, each computed once for all its stores.
struct Paths<'s, 'f> {
    shape: &'s FnShape<'f>,
    /// The nearest block dominating every `return` the entry reaches (`None`: none does).
    returns_under: Option<BlockId>,
    /// The blocks a path reaches a `return` from.
    reach_return: FxHashSet<BlockId>,
    /// By block, the last point exposing the instance in it.
    exposed: &'s FxHashMap<BlockId, usize>,
    /// The blocks a path of one edge or more leads from to a block exposing the instance.
    lead_to_exposed: FxHashSet<BlockId>,
}

impl<'s, 'f> Paths<'s, 'f> {
    fn of(
        shape: &'s FnShape<'f>,
        exposed: &'s FxHashMap<BlockId, usize>,
        budget: &Budget,
    ) -> Answer<Self> {
        let f = shape.f;
        let returning: Vec<BlockId> = (f.blocks.iter())
            .filter(|b| matches!(b.terminator, Terminator::Return(_)) && shape.reachable(b.id))
            .map(|b| b.id)
            .collect();
        let dom = shape.dominators();
        let returns_under = (returning.iter().copied()).reduce(|a, b| dom.common(a, b));
        let reach_return = backward(shape, budget, returning.iter().copied(), true);
        let lead_to_exposed = backward(shape, budget, exposed.keys().copied(), false);
        (reach_return.zip(lead_to_exposed)).map(|(reach_return, lead_to_exposed)| Self {
            shape,
            returns_under,
            reach_return,
            exposed,
            lead_to_exposed,
        })
    }

    /// Whether every path from the entry to a `return` runs the instruction at `at`: its block
    /// dominates every `return`, and no `return` is reached through the block's exception
    /// target, which an instruction before `at` may raise into.
    fn on_every_return(&self, at: At) -> bool {
        let dom = self.shape.dominators();
        self.returns_under.is_some_and(|r| dom.dominates(at.0, r))
            && (self.shape.f.block(at.0).exc_target).is_none_or(|h| !self.reach_return.contains(&h))
    }

    /// Whether a point exposing the instance may run after the instruction at `at`: later in its
    /// block, or in a block a path from it reaches (its own again through a loop).
    fn exposed_after(&self, at: At) -> bool {
        self.exposed.get(&at.0).is_some_and(|last| *last > at.1)
            || self.lead_to_exposed.contains(&at.0)
    }
}

/// The blocks with a path to one of `to` over the edges of `shape`: of no edge (`to` itself
/// included) when `inclusive`, else of one edge or more. Open past the budget.
fn backward(
    shape: &FnShape<'_>,
    budget: &Budget,
    to: impl Iterator<Item = BlockId>,
    inclusive: bool,
) -> Answer<FxHashSet<BlockId>> {
    let starts: Vec<BlockId> = if inclusive {
        to.collect()
    } else {
        to.flat_map(|b| shape.preds(b).iter().copied()).collect()
    };
    let mut seen: FxHashSet<BlockId> = FxHashSet::default();
    let preds = |b: BlockId, out: &mut Vec<BlockId>| out.extend(shape.preds(b));
    let walked = reach(budget, starts, preds, |b| {
        seen.insert(b);
        Visit::<()>::Expand
    });
    walked.map(|_| seen)
}

/// The values that may hold the instance `selfs` holds: those, `super()`, and the result of any
/// instruction or phi reading one of them. Open past the budget.
fn holding_values(
    shape: &FnShape<'_>,
    selfs: &FxHashSet<VarId>,
    budget: &Budget,
) -> Answer<FxHashSet<VarId>> {
    let f = shape.f;
    let supers = (f.blocks.iter().flat_map(|b| &b.insts)).filter_map(|i| match &i.kind {
        InstKind::Call { dst, callee, .. } if callee == "super" => Some(*dst),
        _ => None,
    });
    let starts: Vec<VarId> = selfs.iter().copied().chain(supers).collect();
    let readers = |v: VarId, out: &mut Vec<VarId>| {
        out.extend(shape.readers(v).iter().filter_map(|u| match *u {
            Use::Inst(b, i) => f.block(b).insts[i as usize].kind.dst(),
            Use::Phi(b, i) => Some(f.block(b).phis[i as usize].dst),
            Use::Terminator(_) => None,
        }));
    };
    let mut holding: FxHashSet<VarId> = FxHashSet::default();
    let walked = reach(budget, starts, readers, |v| {
        holding.insert(v);
        Visit::<()>::Expand
    });
    walked.map(|_| holding)
}

/// The kind of the new builtin container `value`, stored at `at`, holds, when that store is its
/// only use: a display, or a call without arguments of the builtin class a global name the
/// function does not bind reads.
fn made_container(
    shape: &FnShape<'_>,
    value: VarId,
    at: At,
    builtin: &dyn Fn(&str) -> Option<ContainerKind>,
) -> Option<ContainerKind> {
    let mut v = value;
    let mut reader = Use::Inst(at.0, u32::try_from(at.1).ok()?);
    loop {
        if shape.readers(v) != [reader] {
            return None;
        }
        let (Def::Inst(b, i), Some(inst)) = (shape.def(v), shape.inst(v)) else {
            return None;
        };
        match &inst.kind {
            InstKind::Copy { src, .. } => {
                reader = Use::Inst(b, i);
                v = *src;
            }
            InstKind::AllocBuiltin {
                kind: BuiltinKind::List,
                ..
            } => return Some(ContainerKind::List),
            InstKind::AllocBuiltin {
                kind: BuiltinKind::Set,
                ..
            } => return Some(ContainerKind::Set),
            InstKind::DictLiteral { .. } => return Some(ContainerKind::Dict),
            InstKind::Call {
                callee,
                callee_var: Some(called),
                args,
                kwargs,
                star_from: None,
                ..
            } if args.is_empty()
                && kwargs.is_empty()
                && matches!(
                    shape.inst(*called).map(|i| &i.kind),
                    Some(InstKind::LoadGlobal { name, .. }) if name == callee
                ) =>
            {
                return builtin(callee);
            }
            _ => return None,
        }
    }
}
