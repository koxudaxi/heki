//! Family 2 data rule: `DATA101` (docs/ANALYSIS_AND_KB_SPEC.md section 2.4).

use crate::eff_rules::shortest;
use crate::mut_rules::bind_arguments;
use crate::{CheckContext, ModuleUnit};
use heki_diagnostics::Diagnostic;
use heki_effect::{Callee, SiteEffect, TypeCtx, Val};
use heki_ir::{Inst, InstKind, IrFunction, LoopCarriedInfo, NaturalLoop, VarId};
use heki_kb::ExecEffectKind;
use rustc_hash::FxHashMap;

/// `DATA101`: a loop-carried variable rebuilt *from itself* through a `QuadraticRealloc` API
/// (`df = pd.concat([df, part])`): one of its accumulating updates (`LoopCarriedInfo::updates`,
/// the back-edge values computed from the previous value) is such a call. One-shot
/// concatenations inside a loop, also on a branch that leaves the variable unchanged otherwise,
/// are not updates and are never reported.
pub(crate) fn data101(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.enabled("DATA101", unit) {
        return;
    }
    let sites = cx.sites_by_span(unit);
    let tcx = TypeCtx::new(&unit.input, cx.decls, cx.kb);
    for f in &unit.input.ir.functions {
        if f.is_class_body || f.loop_carried.iter().all(|lc| !lc.self_accumulating) {
            continue;
        }
        let key = CheckContext::key(unit, f);
        let mut defs: FxHashMap<VarId, &Inst> = FxHashMap::default();
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            if let Some(d) = inst.kind.dst() {
                defs.insert(d, inst);
            }
        }
        // Inferred only for an update through a project call.
        let mut types: Option<FxHashMap<VarId, Val>> = None;
        for (lc, _) in accumulating_loops(f) {
            // The first update (in block order) that is a quadratic reallocation of the previous
            // value: the reallocating call itself, or a project call returning one of the
            // argument that carries it (`acc = grow(acc)`, not `acc = step(acc)` where `step`
            // reallocates something else and returns `acc`).
            let hit = lc.updates.iter().find_map(|v| {
                let inst = defs.get(v)?;
                if !matches!(
                    inst.kind,
                    InstKind::Call { .. } | InstKind::MethodCall { .. }
                ) {
                    return None;
                }
                let effects: Vec<SiteEffect> = sites
                    .get(&(key.qualname.clone(), inst.span))
                    .into_iter()
                    .flatten()
                    .flat_map(|s| cx.index.site_effects(&unit.input.path, s))
                    .collect();
                let chain = shortest(&effects, ExecEffectKind::QuadraticRealloc)?;
                let reallocates = chain.len() == 1 || {
                    let types = types.get_or_insert_with(|| tcx.infer(f));
                    reallocates_argument(cx, &tcx, types, f, inst, lc.phi_var)
                };
                reallocates.then(|| (inst.span, chain.to_vec()))
            });
            let Some((span, chain)) = hit else {
                continue;
            };
            // Loop-carried values are header phis, which the IR builder only creates for
            // named locals (`read_local_recursive`).
            let var = lc
                .var_name
                .clone()
                .expect("loop-carried values are named locals");
            let root = chain.last().map(|h| h.callee.clone()).unwrap_or_default();
            cx.emit(
                out,
                "DATA101",
                unit,
                span,
                &key.qualname,
                var.clone(),
                format!(
                    "`{var}` is rebuilt from itself with `{root}` on every loop iteration (quadratic copying); collect the parts in a list and combine them once after the loop"
                ),
                &chain,
            );
        }
    }
}

/// Whether the project call `inst` returns a quadratic reallocation of an argument that carries
/// `previous` (as itself, through copies, or in a display): a callee's `reallocates_params`.
fn reallocates_argument(
    cx: &CheckContext<'_>,
    tcx: &TypeCtx<'_>,
    types: &FxHashMap<VarId, Val>,
    f: &IrFunction,
    inst: &Inst,
    previous: VarId,
) -> bool {
    let (callee, recv, args, kwargs) = match &inst.kind {
        InstKind::Call {
            callee_var,
            args,
            kwargs,
            ..
        } => (
            tcx.resolve_call(callee_var.and_then(|v| types.get(&v))),
            None,
            args,
            kwargs,
        ),
        InstKind::MethodCall {
            recv,
            method,
            args,
            kwargs,
            ..
        } => (
            tcx.resolve_method(types.get(recv), method),
            Some(*recv),
            args,
            kwargs,
        ),
        _ => return false,
    };
    let Callee::Project(keys, binds_recv) = callee else {
        return false;
    };
    keys.iter().any(|key| {
        let Some(summary) = cx.index.summary(key) else {
            return false;
        };
        let decl = cx.decls.function_decl(key).cloned().unwrap_or_default();
        bind_arguments(&decl, binds_recv, recv, args, kwargs)
            .into_iter()
            .any(|(pos, v)| {
                pos < 64
                    && summary.reallocates_params & (1u64 << pos) != 0
                    && carries(f, v, previous)
            })
    })
}

/// Whether `v` is `previous`, a copy of it, or a display holding one.
fn carries(f: &IrFunction, v: VarId, previous: VarId) -> bool {
    let mut stack = vec![v];
    let mut seen = Vec::new();
    while let Some(v) = stack.pop() {
        if v == previous {
            return true;
        }
        if seen.contains(&v) {
            continue;
        }
        seen.push(v);
        match f.def_inst(v).map(|(_, i)| &i.kind) {
            Some(InstKind::Copy { src, .. }) => stack.push(*src),
            Some(InstKind::AllocBuiltin { elems, .. }) => stack.extend(elems.iter().copied()),
            _ => {}
        }
    }
    false
}

/// Self-accumulating loop-carried values of `f` with their natural loop. `loop_carried`
/// entries name loops of the same function; one without its loop (an IR inconsistency) is
/// skipped instead of being matched against another loop.
fn accumulating_loops(f: &IrFunction) -> Vec<(&LoopCarriedInfo, &NaturalLoop)> {
    let mut out = Vec::new();
    for lc in f.loop_carried.iter().filter(|lc| lc.self_accumulating) {
        let Some(lp) = f.loops.iter().find(|l| l.header == lc.loop_header) else {
            continue;
        };
        out.push((lc, lp));
    }
    out
}
