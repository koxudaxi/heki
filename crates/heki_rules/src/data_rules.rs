//! Family 2 data rule: `DATA101` (docs/ANALYSIS_AND_KB_SPEC.md section 2.4).

use crate::eff_rules::shortest;
use crate::mut_rules::CallInst;
use crate::{CheckContext, ModuleUnit};
use heki_diagnostics::Diagnostic;
use heki_effect::{Callee, SiteEffect, TypeCtx, Val};
use heki_ir::{Inst, InstKind, IrFunction, LoopCarriedInfo, NaturalLoop, VarId};
use heki_kb::ExecEffectKind;
use rustc_hash::FxHashMap;

/// `DATA101`: a loop-carried variable rebuilt *from itself* through a `QuadraticRealloc` API
/// (`df = pd.concat([df, part])`): one of its accumulating updates (`LoopCarriedInfo::updates`,
/// the back-edge values computed from the previous value) is such a call, and the previous
/// value reaches it through the data it combines, not only as the `out=` buffer it writes into
/// (`out = np.concatenate(parts, out=out)` reuses the buffer). One-shot concatenations inside a
/// loop, also on a branch that leaves the variable unchanged otherwise, are not updates and are
/// never reported.
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
                let call = CallInst::of(&inst.kind)?;
                if !combines(f, &defs, &call, lc.phi_var) {
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
                    reallocates_argument(cx, &tcx, types, f, &call, lc.phi_var)
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

/// Whether the project call `call` returns a quadratic reallocation of an argument that carries
/// `previous` (as itself, through copies, or in a display): a callee's `reallocates_params`.
fn reallocates_argument(
    cx: &CheckContext<'_>,
    tcx: &TypeCtx<'_>,
    types: &FxHashMap<VarId, Val>,
    f: &IrFunction,
    call: &CallInst<'_>,
    previous: VarId,
) -> bool {
    let Callee::Project(keys, binds_recv) = call.callee(tcx, types) else {
        return false;
    };
    keys.iter().any(|key| {
        cx.index.summary(key).is_some_and(|summary| {
            let decl = cx.decls.function_decl(key).cloned().unwrap_or_default();
            call.bind(&decl, binds_recv).into_iter().any(|(pos, v)| {
                pos < 64
                    && summary.reallocates_params & (1u64 << pos) != 0
                    && carries(f, v, previous)
            })
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

/// Whether `call` reads `previous` through what it combines: its receiver, its positional
/// arguments, or a keyword argument other than the `out=` destination, followed back through the
/// instructions and phis that compute them.
fn combines(
    f: &IrFunction,
    defs: &FxHashMap<VarId, &Inst>,
    call: &CallInst<'_>,
    previous: VarId,
) -> bool {
    let mut work: Vec<VarId> = call
        .recv
        .map(|(recv, _)| recv)
        .into_iter()
        .chain(call.args.iter().copied())
        .chain(destination_free(call.kwargs))
        .collect();
    let phis: FxHashMap<VarId, &heki_ir::PhiNode> = f
        .blocks
        .iter()
        .flat_map(|b| &b.phis)
        .map(|p| (p.dst, p))
        .collect();
    let mut seen: rustc_hash::FxHashSet<VarId> = work.iter().copied().collect();
    while let Some(v) = work.pop() {
        if v == previous {
            return true;
        }
        let next: Vec<VarId> = match (defs.get(&v), phis.get(&v)) {
            (Some(def), _) => def.kind.operands(),
            (None, Some(phi)) => phi.incoming.iter().map(|(_, x)| *x).collect(),
            (None, None) => Vec::new(),
        };
        work.extend(next.into_iter().filter(|x| seen.insert(*x)));
    }
    false
}

/// The keyword arguments of a call but `out=`, the destination buffer it writes into.
fn destination_free(kwargs: &[(Option<String>, VarId)]) -> impl Iterator<Item = VarId> + '_ {
    kwargs
        .iter()
        .filter(|(name, _)| name.as_deref() != Some("out"))
        .map(|(_, v)| *v)
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
