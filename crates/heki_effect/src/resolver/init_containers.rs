//! The `__init__` of a class that only stores attributes on the instance
//! ([`super::ClassDecl::plain_init`]), and the containers it makes for the instance, which the
//! instance alone holds when the constructor returns. A function constructing such an instance
//! counts what it adds to that container as held by the instance (`fresh-objects`,
//! `crate::construction`).

use super::{
    LocalNames, ModuleDecls, NameBinding, PlainInit, is_summarized_function, self_aliases,
};
use crate::ModuleInput;
use crate::readonly::ContainerKind;
use crate::summary::NAMESPACE_ATTRS;
use heki_ir::walk::{Answer, Budget, FnShape, Use, Visit, reach};
use heki_ir::{BlockId, BuiltinKind, FunctionKind, InstKind, IrFunction, Terminator, VarId};
use rustc_hash::FxHashSet;
use std::collections::BTreeMap;

/// A point of a function: block, instruction index.
type At = (BlockId, usize);

/// By attribute, the point and value of each store of it through the instance.
type Stores<'f> = BTreeMap<&'f str, Vec<(At, VarId)>>;

/// [`super::ClassDecl::plain_init`] of the module's classes, from their own `__init__`: a plain
/// method (no decorator, which may replace it) taking the instance as its first parameter, the
/// only binding of the name in the class body.
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
        // The builtin a global name the module does not bind reads.
        let builtin = |name: &str| {
            let class = format!("builtins.{name}");
            match decls.lookup(name, false, &LocalNames::default()) {
                NameBinding::Unbound => Some(class),
                NameBinding::Object(fqn) => (fqn == class).then_some(fqn),
                NameBinding::Unknown => None,
            }
        };
        let found = plain_init(f, this.var, &builtin);
        if let Some(class) = decls.classes.get_mut(owner) {
            class.plain_init = found;
        }
    }
}

/// The [`PlainInit`] `f` is, with `this` its parameter of the instance: no instruction or
/// terminator uses a value that may hold the instance (`this`, what is read out of it,
/// `super()`, and what any of those flows into) other than as
/// - the object of an attribute store through `this`, of any attribute but `__dict__` and
///   `__class__`, which replace the instance's namespace or its class;
/// - a copy;
/// - `super().__init__(...)`, the call of the builtin `super` without arguments, handed no
///   value that may hold the instance.
///
/// Its containers are the attributes bound to a builtin container `f` makes, that the instance
/// alone holds when `f` returns: the container is a list, dict, or set display, or a call of a
/// builtin container class (`list`, `dict`, `set`, `bytearray`) without arguments, the store is
/// its only use and the only store of the attribute, and every path to a `return` passes the
/// store. Nothing else may reach the instance, so nothing `f` runs may keep the container or
/// rebind the attribute but the stores and the `__init__` it calls, of which
/// [`super::ProjectDecls::constructor_containers`] establishes the same. `None` past the budget.
fn plain_init(
    f: &IrFunction,
    this: VarId,
    builtin: &dyn Fn(&str) -> Option<String>,
) -> Option<Box<PlainInit>> {
    let shape = FnShape::new(f);
    let selfs = self_aliases(f, this);
    let budget = Budget::of_function("resolver::init_containers", f);
    let found = holding_values(&shape, &selfs, &budget).and_then(|holding| {
        let Some((stores, calls_next)) = instance_uses(&shape, &selfs, &holding, builtin) else {
            return Answer::Known(None);
        };
        let containers = Paths::of(&shape, &budget).and_then(|paths| {
            (stores.iter())
                .map(|(attr, stored)| {
                    let [(at, value)] = stored[..] else {
                        return Answer::Known(None);
                    };
                    if !paths.on_every_return(at) {
                        return Answer::Known(None);
                    }
                    let kind = made_container(&shape, &budget, value, builtin);
                    kind.map(|kind| kind.map(|kind| (Box::<str>::from(*attr), kind)))
                })
                .collect::<Answer<Vec<_>>>()
        });
        containers.map(|containers| {
            Some(Box::new(PlainInit {
                stores: stores.keys().map(|a| (*a).into()).collect(),
                calls_next,
                containers: containers.into_iter().flatten().collect(),
            }))
        })
    });
    // Past the budget, the `__init__` is not known to do no more.
    found.open_means(None)
}

/// The attributes `f` stores through the instance `selfs` holds, with each store's point and
/// value, and whether it calls `super().__init__(...)`: `None` when a value that may hold the
/// instance (`holding`) has another use ([`plain_init`]).
fn instance_uses<'f>(
    shape: &FnShape<'f>,
    selfs: &FxHashSet<VarId>,
    holding: &FxHashSet<VarId>,
    builtin: &dyn Fn(&str) -> Option<String>,
) -> Option<(Stores<'f>, bool)> {
    let f = shape.f;
    let mut stores = Stores::new();
    let mut calls_next = false;
    let next_init = |recv: VarId, args: &[VarId], kwargs: &[(Option<String>, VarId)]| {
        let made_by = shape.inst(shape.copy_root(recv)).map(|i| &i.kind);
        made_by
            .and_then(|kind| global_call(shape, kind))
            .is_some_and(|callee| {
                callee == "super" && builtin(callee).as_deref() == Some("builtins.super")
            })
            && !(args.iter().chain(kwargs.iter().map(|(_, v)| v))).any(|v| holding.contains(v))
    };
    for bb in &f.blocks {
        for (i, inst) in bb.insts.iter().enumerate() {
            match &inst.kind {
                InstKind::AttrStore { obj, attr, value } if selfs.contains(obj) => {
                    if NAMESPACE_ATTRS.contains(&attr.as_str()) {
                        return None;
                    }
                    stores.entry(attr).or_default().push(((bb.id, i), *value));
                }
                InstKind::Copy { .. } => {}
                InstKind::MethodCall {
                    recv,
                    method,
                    args,
                    kwargs,
                    is_awaited: false,
                    ..
                } if method == "__init__" && next_init(*recv, args, kwargs) => calls_next = true,
                kind => {
                    if kind.operands().iter().any(|v| holding.contains(v)) {
                        return None;
                    }
                }
            }
        }
    }
    // A branch on the instance runs its `__bool__`; returning or raising it hands it on.
    let in_terminator =
        |v: &VarId| (shape.readers(*v).iter()).any(|u| matches!(u, Use::Terminator(_)));
    (!holding.iter().any(in_terminator)).then_some((stores, calls_next))
}

/// What the paths of a function say about its points, each computed once for all its stores.
struct Paths<'s, 'f> {
    shape: &'s FnShape<'f>,
    /// The nearest block dominating every `return` the entry reaches (`None`: none does).
    returns_under: Option<BlockId>,
    /// The blocks a path reaches a `return` from.
    reach_return: FxHashSet<BlockId>,
}

impl<'s, 'f> Paths<'s, 'f> {
    fn of(shape: &'s FnShape<'f>, budget: &Budget) -> Answer<Self> {
        let f = shape.f;
        let returning: Vec<BlockId> = (f.blocks.iter())
            .filter(|b| matches!(b.terminator, Terminator::Return(_)) && shape.reachable(b.id))
            .map(|b| b.id)
            .collect();
        let dom = shape.dominators();
        let returns_under = (returning.iter().copied()).reduce(|a, b| dom.common(a, b));
        let mut reach_return: FxHashSet<BlockId> = FxHashSet::default();
        let preds = |b: BlockId, out: &mut Vec<BlockId>| out.extend(shape.preds(b));
        let walked = reach(budget, returning, preds, |b| {
            reach_return.insert(b);
            Visit::<()>::Expand
        });
        walked.map(|_| Self {
            shape,
            returns_under,
            reach_return,
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

/// The kind of the new builtin container the stored `value` is, when the store is its only use:
/// a display, or a call without arguments of the builtin class a global name the function does
/// not bind reads, also through copies, each the only use of the value it copies. Open past the
/// budget.
fn made_container(
    shape: &FnShape<'_>,
    budget: &Budget,
    value: VarId,
    builtin: &dyn Fn(&str) -> Option<String>,
) -> Answer<Option<ContainerKind>> {
    let copied = |v: VarId, out: &mut Vec<VarId>| {
        if let Some(InstKind::Copy { src, .. }) = shape.inst(v).map(|i| &i.kind) {
            out.push(*src);
        }
    };
    // The store, or the copy the walk came from, reads each value the walk visits.
    let walked = reach(budget, [value], copied, |v| {
        let made_by = (shape.readers(v).len() == 1)
            .then(|| shape.inst(v))
            .flatten()
            .map(|i| &i.kind);
        match made_by {
            Some(InstKind::Copy { .. }) => Visit::Expand,
            Some(InstKind::AllocBuiltin {
                kind: BuiltinKind::List,
                ..
            }) => Visit::Stop(Some(ContainerKind::List)),
            Some(InstKind::AllocBuiltin {
                kind: BuiltinKind::Set,
                ..
            }) => Visit::Stop(Some(ContainerKind::Set)),
            Some(InstKind::DictLiteral { .. }) => Visit::Stop(Some(ContainerKind::Dict)),
            Some(kind) => Visit::Stop(
                global_call(shape, kind)
                    .and_then(builtin)
                    .and_then(|class| ContainerKind::of_class(&class)),
            ),
            None => Visit::Stop(None),
        }
    });
    walked.map(Option::flatten)
}

/// The name `kind` calls when it is a call without arguments of the global the name reads.
fn global_call<'f>(shape: &FnShape<'f>, kind: &'f InstKind) -> Option<&'f str> {
    match kind {
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
            Some(callee)
        }
        _ => None,
    }
}
