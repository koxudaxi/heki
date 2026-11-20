//! Family 3 (`OPT101`–`OPT130`): `heki_soundness` candidates turned into diagnostics
//! (docs/DETAILED_SPEC_M3_M7.md sections 4.5–4.6).
//!
//! Reported: candidates that are not `Disproved`, are profitable, and have proven cost
//! (`OptCandidate::is_reportable`). A diagnostic carries `fix` only when the rewrite is `Safe`;
//! whether `--fix` applies it by default is the rule's `auto_fix_default` (`OPT101` / `OPT102`).

use crate::{CheckContext, ModuleUnit};
use heki_diagnostics::{Diagnostic, TextEdit};
use heki_soundness::{
    AnalysisOptions, OptCandidate, ProfitabilityTier, SafetyTier, analyze_module,
};

/// Suffix explaining what `--fix` does with one candidate.
fn fix_note(c: &OptCandidate) -> String {
    match (c.verdict.tier, c.fix().is_some(), c.profitability) {
        (SafetyTier::Safe, true, ProfitabilityTier::Unconditional) => {
            " [safe fix: `heki check --fix`]".into()
        }
        (SafetyTier::Safe, true, ProfitabilityTier::SizeDependent) => {
            " [proven-safe suggestion: `heki check --fix --include-size-dependent`]".into()
        }
        (SafetyTier::Safe, false, _) => " [proven safe; rewrite it by hand]".into(),
        // Reported candidates that are not safe are `Blocked`, which names its obligation.
        _ => c
            .verdict
            .blocker()
            .map(|b| format!(" [not auto-fixable: {}]", b.describe()))
            .unwrap_or_default(),
    }
}

pub(crate) fn opt(
    cx: &CheckContext<'_>,
    unit: &ModuleUnit<'_>,
    opts: &AnalysisOptions,
    out: &mut Vec<Diagnostic>,
) {
    let any_enabled = heki_soundness::OptRule::ALL
        .iter()
        .any(|r| cx.enabled(r.code(), unit));
    if !any_enabled {
        return;
    }
    for c in analyze_module(unit.input.ir, unit.source, cx.soundness, opts) {
        if !c.is_reportable() {
            continue;
        }
        let message = format!("{}{}", c.message, fix_note(&c));
        let Some(d) = cx.emit(
            out,
            c.rule.code(),
            unit,
            c.span,
            &c.function,
            c.target.clone(),
            message,
            &[],
        ) else {
            // This `OPT` rule is not selected for the file.
            continue;
        };
        d.evidence = c.cost_evidence.clone().map(|cost| match c.verdict.tier {
            SafetyTier::Safe => format!("{cost}; established: {}", c.verdict.evidence()),
            _ => cost,
        });
        d.blocker = c.verdict.blocker().map(|b| b.describe());
        d.fix = c.fix().map(|edits| {
            edits
                .iter()
                .map(|e| TextEdit {
                    span: e.span,
                    replacement: e.replacement.clone(),
                })
                .collect()
        });
    }
}

/// `OPT112`: `list.index` / `list.remove` / `list.count` inside a loop (Advisory lane,
/// scaffold).
pub(crate) fn opt112(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    let _ = (cx, unit, out);
}

/// `OPT113`: nested-loop equi-joins (Advisory lane, scaffold).
pub(crate) fn opt113(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    let _ = (cx, unit, out);
}
