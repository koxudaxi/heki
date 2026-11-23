//! `MUT119`: an in-place change of a builtin container applied to a new object that nothing else
//! holds and the code never names (`list(xs).sort()`, `env = os.environ.copy().update(...)`,
//! `task.environment["K"] = v` through a getter that returns a new dict): the change is lost when
//! the statement ends.
//!
//! The changed object is an unnamed value that nothing reads but the change and an item load of
//! the same statement (`settings()[k] += 1`), so no later code can read it and no flow analysis is
//! needed. Where it comes from is [`heki_effect::fresh::source`]: the builtin itself, a copy, or
//! project functions whose final facts say every return is a new container of one kind
//! ([`heki_effect::ProjectIndex::fresh_return`]).

use crate::{CheckContext, ModuleUnit};
use heki_diagnostics::{Certainty, Diagnostic};
use heki_effect::fresh::{FreshReturn, Source, source};
use heki_effect::readonly::ContainerKind;
use heki_effect::{Callee, TypeCtx, Val};
use heki_ir::walk::{FnShape, Use};
use heki_ir::{BlockId, Inst, InstKind, VarId};
use rustc_hash::FxHashMap;

/// A change applied to an unnamed value.
struct Change<'f> {
    inst: &'f Inst,
    /// The changed object.
    object: VarId,
    /// The changing method; `None` for an item store or delete.
    method: Option<&'f str>,
}

/// What the changed object is.
struct Changed {
    kind: ContainerKind,
    /// The function whose result it is (a property's getter).
    returned_by: Option<String>,
    copy: bool,
    exact: bool,
}

/// `MUT119`'s reports in the functions of `unit`.
pub(crate) fn mut119(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.enabled("MUT119", unit) {
        return;
    }
    let tcx = TypeCtx::new(&unit.input, cx.decls, cx.kb);
    for f in &unit.input.ir.functions {
        let shape = FnShape::new(f);
        let changes = changes(&shape);
        if changes.is_empty() {
            continue;
        }
        let types = tcx.infer(f);
        let qualname = CheckContext::key(unit, f).qualname;
        for change in changes {
            let Some(changed) = changed(cx, &tcx, &shape, &types, &change) else {
                continue;
            };
            let Some(bound) = lost(&shape, &change) else {
                continue;
            };
            report(cx, unit, &qualname, &change, &changed, bound, out);
        }
    }
}

/// The changes of `shape`'s function applied to an unnamed value that a display, a call, a
/// property read, or a slice makes, read by nothing else but an item load.
fn changes<'f>(shape: &FnShape<'f>) -> Vec<Change<'f>> {
    let f = shape.f;
    let mut out = Vec::new();
    for b in &f.blocks {
        for (i, inst) in b.insts.iter().enumerate() {
            let (object, method) = match &inst.kind {
                InstKind::MethodCall { recv, method, .. }
                    if ContainerKind::EVERY.iter().any(|k| k.mutator(method)) =>
                {
                    (*recv, Some(method.as_str()))
                }
                InstKind::SubscriptStore { obj, .. } | InstKind::SubscriptDelete { obj, .. } => {
                    (*obj, None)
                }
                _ => continue,
            };
            // The change's own expression makes the object: a comprehension's appends, which
            // build it, lie inside its span instead.
            let made = shape.inst(object).is_some_and(|d| {
                d.span.start >= inst.span.start
                    && d.span.end <= inst.span.end
                    && matches!(
                        d.kind,
                        InstKind::AllocBuiltin { .. }
                            | InstKind::DictLiteral { .. }
                            | InstKind::Call { .. }
                            | InstKind::MethodCall { .. }
                            | InstKind::AttrLoad { .. }
                            | InstKind::SubscriptLoad { is_slice: true, .. }
                    )
            });
            let at = Use::Inst(b.id, u32::try_from(i).unwrap_or(u32::MAX));
            if made
                && f.var_name(object).is_none()
                && shape
                    .readers(object)
                    .iter()
                    .all(|u| *u == at || item_load(shape, *u, object))
            {
                out.push(Change {
                    inst,
                    object,
                    method,
                });
            }
        }
    }
    out
}

/// Whether the use `u` loads an item of `object` (`d[k]` of `d[k] += 1`).
fn item_load(shape: &FnShape<'_>, u: Use, object: VarId) -> bool {
    let kind = |b: BlockId, i: u32| &shape.f.block(b).insts[i as usize].kind;
    matches!(u, Use::Inst(b, i) if matches!(
        kind(b, i),
        InstKind::SubscriptLoad { obj, index, .. } if *obj == object && *index != object
    ))
}

/// What the change's object is, when it is a new builtin container of a kind the change
/// changes: built, copied, or returned by project functions whose every return is one.
fn changed(
    cx: &CheckContext<'_>,
    tcx: &TypeCtx<'_>,
    shape: &FnShape<'_>,
    types: &FxHashMap<VarId, Val>,
    change: &Change<'_>,
) -> Option<Changed> {
    let made = &shape.inst(change.object)?.kind;
    let class_of = |v: VarId| match types.get(&v) {
        Some(Val::Inst(class)) => Some(class.as_str()),
        _ => None,
    };
    // What a call or a property read resolves to, and whether that is every implementation it
    // can run.
    let (callee, complete) = match made {
        InstKind::AttrLoad { obj, attr, .. } => {
            let callee = tcx.resolve_attr_load(types.get(obj), attr);
            let complete = matches!(&callee, Some(Callee::Project(keys, _)) if tcx.getter_targets_complete(types.get(obj), attr, keys));
            (callee, complete)
        }
        InstKind::MethodCall { recv, method, .. } => {
            let callee = tcx.resolve_method(types.get(recv), method);
            let complete = tcx.method_targets_complete(types.get(recv), method, &callee);
            (Some(callee), complete)
        }
        InstKind::Call { callee_var, .. } => {
            let value = callee_var.and_then(|v| types.get(&v));
            let callee = tcx.resolve_call(value);
            let complete = tcx.call_targets_complete(value, &callee);
            (Some(callee), complete)
        }
        _ => (None, false),
    };
    let (by_name, method) = match change.method {
        Some(m) => (true, m),
        None if matches!(change.inst.kind, InstKind::SubscriptStore { .. }) => {
            (false, "__setitem__")
        }
        None => (false, "__delitem__"),
    };
    let decls = cx.decls;
    let mut changed = match source(
        made,
        callee.as_ref(),
        types.get(&change.object),
        class_of,
        cx.kb,
    )? {
        Source::Built(kind) => Changed {
            kind,
            returned_by: None,
            copy: false,
            exact: true,
        },
        // Under R2 the copied value may be of a project subclass that replaces the copy or the
        // change.
        Source::Copied(kind) => {
            let copy_kept = match made {
                InstKind::MethodCall { recv, method, .. } => {
                    class_of(*recv).is_none_or(|c| decls.method_not_overridden(c, method, true))
                }
                InstKind::SubscriptLoad { obj, .. } => class_of(*obj)
                    .is_none_or(|c| decls.method_not_overridden(c, "__getitem__", false)),
                _ => true,
            };
            Changed {
                kind,
                returned_by: None,
                copy: true,
                exact: copy_kept && decls.method_not_overridden(kind.class(), method, by_name),
            }
        }
        Source::Calls(keys) => {
            let mut fact = FreshReturn {
                kind: None,
                built: complete,
                calls: Vec::new(),
            };
            if !keys
                .iter()
                .all(|k| fact.join_callee(cx.index.fresh_return(k)))
            {
                return None;
            }
            Changed {
                kind: fact.kind?,
                returned_by: Some(keys[0].qualname.clone()),
                copy: false,
                exact: fact.built,
            }
        }
    };
    let changes = match change.method {
        Some(m) => changed.kind.mutator(m),
        None => changed.kind.takes_item_stores(),
    };
    // An augmented item assignment (`d[k] += v`) runs the item's own in-place operator first,
    // which may change an item other code holds.
    changed.exact &= !matches!(
        change.inst.kind,
        InstKind::SubscriptStore { value, .. }
            if matches!(shape.inst(value).map(|i| &i.kind), Some(InstKind::BinOp { in_place: true, .. }))
    );
    changes.then_some(changed)
}

/// Whether the change is lost: its result is unused, or it is `None` and only bound to names or
/// stored (`Some(true)`). An item store or delete has no result.
fn lost(shape: &FnShape<'_>, change: &Change<'_>) -> Option<bool> {
    let (InstKind::MethodCall { dst, .. }, Some(method)) = (&change.inst.kind, change.method)
    else {
        return Some(false);
    };
    let readers = shape.readers(*dst);
    if readers.is_empty() {
        return Some(false);
    }
    (!ContainerKind::returns_value(method) && readers.iter().all(|u| binds(shape, *u, *dst)))
        .then_some(true)
}

/// Whether the use `u` only binds `v` to a name or stores it (`x = v`, `obj.a = v`, `m[k] = v`).
fn binds(shape: &FnShape<'_>, u: Use, v: VarId) -> bool {
    let f = shape.f;
    let Use::Inst(b, i) = u else {
        return false;
    };
    match &f.block(b).insts[i as usize].kind {
        InstKind::Copy { dst, .. } => f.var_name(*dst).is_some(),
        InstKind::StoreGlobal { .. } | InstKind::StoreDeref { .. } => true,
        InstKind::AttrStore { obj, value, .. } => *value == v && *obj != v,
        InstKind::SubscriptStore {
            obj, index, value, ..
        } => *value == v && *obj != v && *index != v,
        _ => false,
    }
}

fn report(
    cx: &CheckContext<'_>,
    unit: &ModuleUnit<'_>,
    qualname: &str,
    change: &Change<'_>,
    changed: &Changed,
    bound: bool,
    out: &mut Vec<Diagnostic>,
) {
    let kind = changed.kind.as_str();
    let (what, member) = match (change.method, &change.inst.kind) {
        (Some(m), _) => (format!("`{m}`"), m),
        (None, InstKind::SubscriptStore { .. }) => {
            ("the item assignment".to_owned(), "__setitem__")
        }
        (None, _) => ("the item deletion".to_owned(), "__delitem__"),
    };
    let object = match (&changed.returned_by, changed.copy) {
        (Some(function), _) => format!("the new `{kind}` that `{function}` returns"),
        (None, true) => format!("a new copy (a `{kind}`)"),
        (None, false) => format!("a new `{kind}`"),
    };
    let stored = if bound {
        format!(", and the assignment stores the `None` that {what} returns")
    } else {
        String::new()
    };
    let d = cx
        .emit(
            out,
            "MUT119",
            unit,
            change.inst.span,
            qualname,
            format!("{kind}.{member}"),
            format!(
                "{what} changes {object}, which nothing else holds and no name keeps: the change is lost when the statement ends{stored}"
            ),
            Some(format!(
                "bind the new `{kind}` to a name and change it there, or build the changed value in one expression (`sorted(...)`, `{{**a, **b}}`, `[*a, *b]`)"
            )),
            &[],
        )
        .expect("the rule runs on the unit");
    let tier = if changed.exact {
        Certainty::Exact
    } else {
        Certainty::Heuristic
    };
    d.certainty = tier.at_most(d.certainty);
}
