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
        _ => match c.verdict.blocker() {
            Some(b) => format!(" [not auto-fixable: {}]", b.describe()),
            None => String::new(),
        },
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
        if !c.is_reportable() || !cx.enabled(c.rule.code(), unit) {
            continue;
        }
        let message = format!("{}{}", c.message, fix_note(&c));
        let before = out.len();
        cx.emit(
            out,
            c.rule.code(),
            unit,
            c.span,
            &c.function,
            c.target.clone(),
            message,
            &[],
        );
        if let Some(d) = out.get_mut(before) {
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
}
