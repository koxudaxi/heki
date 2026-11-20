//! Family 2 data rule: `DATA101` (docs/ANALYSIS_AND_KB_SPEC.md section 2.4).

use crate::eff_rules::shortest;
use crate::{CheckContext, ModuleUnit};
use heki_diagnostics::Diagnostic;
use heki_effect::SiteEffect;
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
        for (lc, _) in accumulating_loops(f) {
            // The first update (in block order) that is a quadratic reallocation.
            let hit = lc.updates.iter().find_map(|v| {
                let inst = defs.get(v)?;
                if !matches!(
                    inst.kind,
                    InstKind::Call { .. } | InstKind::MethodCall { .. }
                ) || !combines(f, &defs, inst, lc.phi_var)
                {
                    return None;
                }
                let effects: Vec<SiteEffect> = sites
                    .get(&(key.qualname.clone(), inst.span))
                    .into_iter()
                    .flatten()
                    .flat_map(|s| cx.index.site_effects(&unit.input.path, s))
                    .collect();
                shortest(&effects, ExecEffectKind::QuadraticRealloc)
                    .map(|chain| (inst.span, chain.to_vec()))
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

/// Whether the call `inst` reads `previous` through what it combines: its receiver, its
/// positional arguments, or a keyword argument other than the `out=` destination, followed back
/// through the instructions and phis that compute them.
fn combines(f: &IrFunction, defs: &FxHashMap<VarId, &Inst>, inst: &Inst, previous: VarId) -> bool {
    let mut work: Vec<VarId> = match &inst.kind {
        InstKind::Call { args, kwargs, .. } => args
            .iter()
            .copied()
            .chain(destination_free(kwargs))
            .collect(),
        InstKind::MethodCall {
            recv, args, kwargs, ..
        } => std::iter::once(*recv)
            .chain(args.iter().copied())
            .chain(destination_free(kwargs))
            .collect(),
        _ => return false,
    };
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
