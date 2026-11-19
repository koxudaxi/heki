//! Family 2 data rule: `DATA101` (docs/DETAILED_SPEC_M3_M7.md section 3.2).

use crate::eff_rules::shortest;
use crate::{CheckContext, ModuleUnit};
use heki_diagnostics::Diagnostic;
use heki_effect::SiteEffect;
use heki_ir::{BlockId, Inst, InstKind, VarId};
use heki_kb::ExecEffectKind;
use rustc_hash::{FxHashMap, FxHashSet};

/// `DATA101`: a loop-carried variable rebuilt *from itself* (`self_accumulating`) through a
/// `QuadraticRealloc` API (`df = pd.concat([df, part])`). One-shot concatenations inside a loop
/// are not self-accumulating and are never reported.
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
        for lc in f.loop_carried.iter().filter(|lc| lc.self_accumulating) {
            let Some(lp) = f.loops.iter().find(|l| l.header == lc.loop_header) else {
                continue;
            };
            let in_loop: FxHashSet<BlockId> = lp.body.iter().copied().chain([lp.header]).collect();
            let phis: FxHashMap<VarId, Vec<VarId>> = f
                .blocks
                .iter()
                .filter(|b| in_loop.contains(&b.id))
                .flat_map(|b| &b.phis)
                .map(|p| (p.dst, p.incoming.iter().map(|(_, v)| *v).collect()))
                .collect();
            // Walk back from the back-edge values to the calls that produced them.
            let mut stack: Vec<VarId> = lc.back_vars.clone();
            let mut visited = FxHashSet::default();
            let mut hit = None;
            while let Some(v) = stack.pop() {
                if v == lc.phi_var || !visited.insert(v) {
                    continue;
                }
                if let Some(incoming) = phis.get(&v) {
                    stack.extend(incoming);
                    continue;
                }
                let Some(inst) = defs.get(&v) else {
                    continue;
                };
                match &inst.kind {
                    InstKind::Copy { src, .. } => stack.push(*src),
                    InstKind::Call { .. } | InstKind::MethodCall { .. } => {
                        let effects: Vec<SiteEffect> = sites
                            .get(&(key.qualname.clone(), inst.span))
                            .into_iter()
                            .flatten()
                            .flat_map(|s| cx.index.site_effects(&unit.input.path, s))
                            .collect();
                        if let Some(chain) = shortest(&effects, ExecEffectKind::QuadraticRealloc) {
                            hit = Some((inst.span, chain.to_vec()));
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let Some((span, chain)) = hit else {
                continue;
            };
            let var = lc.var_name.clone().unwrap_or_else(|| "<value>".into());
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
