//! The contracts with `fail = true`: their reports fail the run whatever their tier and
//! severity ([`Diagnostic::fails_run`]).

use heki_diagnostics::Diagnostic;
use heki_env::HekiConfig;

use crate::DottedPattern;

/// Mark the reports of the contracts with `fail = true`. Run once the reports of a project are
/// final: a report standing for several contracts fails the run when one of them says so.
pub(crate) fn mark_failing_reports(config: &HekiConfig, diags: &mut [Diagnostic]) {
    let failing = |fail: &Option<bool>| fail.unwrap_or(false);
    // `fail` per contract of each kind, by place in the kind's array; nothing to do without one.
    let places = |fails: Vec<bool>| fails.contains(&true).then_some(fails);
    let write = places(
        config
            .write_owners
            .iter()
            .map(|r| failing(&r.fail))
            .collect(),
    );
    let read = places(
        config
            .read_owners
            .iter()
            .map(|r| failing(&r.fail))
            .collect(),
    );
    let call = places(
        config
            .call_owners
            .iter()
            .map(|r| failing(&r.fail))
            .collect(),
    );
    let effect = places(
        config
            .effect_boundaries
            .iter()
            .map(|r| failing(&r.fail))
            .collect(),
    );
    let invalidation = places(
        config
            .invalidation
            .iter()
            .map(|r| failing(&r.fail))
            .collect(),
    );
    let import = places(
        config
            .import_boundaries
            .iter()
            .map(|r| failing(&r.fail))
            .collect(),
    );
    let producers: Vec<DottedPattern> = config
        .readonly_returns
        .iter()
        .filter(|r| failing(&r.fail))
        .flat_map(|r| &r.producers)
        .filter_map(|p| DottedPattern::new(p, "readonly-returns.producers").ok())
        .collect();
    for d in diags {
        let kind = match d.rule_code.as_str() {
            "MUT201" | "MUT205" => &write,
            "MUT203" | "MUT206" => &read,
            "MUT204" | "MUT207" => &call,
            "EFF201" => &effect,
            "MUT208" => &invalidation,
            "IMP201" | "IMP202" => &import,
            "MUT202" => {
                // The report names its producer, not its contract.
                d.fails_run_by_contract = producers.iter().any(|p| names_producer(p, &d.target));
                continue;
            }
            _ => continue,
        };
        if let Some(fails) = kind {
            d.fails_run_by_contract = d
                .contracts
                .iter()
                .any(|&c| fails.get(c as usize).copied().unwrap_or(false));
        }
    }
}

/// Whether `pattern` names the producer a report gives by its fully qualified name (a project
/// function or an attribute) or its symbol (an external one): a pattern with a module part
/// judges the name split at each of its dots.
fn names_producer(pattern: &DottedPattern, producer: &str) -> bool {
    pattern.matches_def(producer, producer)
        || producer
            .match_indices('.')
            .any(|(dot, _)| pattern.matches_def(producer, &producer[dot + 1..]))
}
