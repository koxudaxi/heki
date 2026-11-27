//! What the contracts of a report add to it once the reports of a project are final: whether it
//! fails the run whatever its tier and severity ([`Diagnostic::fails_run`]: a contract with
//! `fail = true`, or a rule `lint.fail` names), and where the configuration defines its
//! contracts ([`Diagnostic::contract_locations`]).

use crate::selection::RuleSelection;
use heki_diagnostics::{ContractLocation, Diagnostic};
use heki_env::{DefinedAt, HekiConfig};

/// The kind of the contracts the reports of `code` are of ([`Diagnostic::contracts`]).
fn contract_kind(code: &str) -> Option<&'static str> {
    Some(match code {
        "MUT201" | "MUT205" => "write-owners",
        "MUT203" | "MUT206" => "read-owners",
        "MUT204" | "MUT207" => "call-owners",
        "EFF201" => "effect-boundaries",
        "MUT208" => "invalidation",
        "IMP201" | "IMP202" => "import-boundaries",
        "MUT202" => "readonly-returns",
        _ => return None,
    })
}

/// `fail` of each contract of `kind`, by place in the kind's array.
fn fails_of(config: &HekiConfig, kind: &str) -> Vec<bool> {
    let fails = |fail: &Option<bool>| fail.unwrap_or(false);
    match kind {
        "write-owners" => config.write_owners.iter().map(|r| fails(&r.fail)).collect(),
        "read-owners" => config.read_owners.iter().map(|r| fails(&r.fail)).collect(),
        "call-owners" => config.call_owners.iter().map(|r| fails(&r.fail)).collect(),
        "effect-boundaries" => (config.effect_boundaries.iter())
            .map(|r| fails(&r.fail))
            .collect(),
        "invalidation" => config.invalidation.iter().map(|r| fails(&r.fail)).collect(),
        "import-boundaries" => (config.import_boundaries.iter())
            .map(|r| fails(&r.fail))
            .collect(),
        _ => (config.readonly_returns.iter())
            .map(|r| fails(&r.fail))
            .collect(),
    }
}

/// `name` of the contract at `index` of `kind`.
fn name_of<'c>(config: &'c HekiConfig, kind: &str, index: usize) -> Option<&'c str> {
    let name = match kind {
        "write-owners" => &config.write_owners.get(index)?.name,
        "read-owners" => &config.read_owners.get(index)?.name,
        "call-owners" => &config.call_owners.get(index)?.name,
        "effect-boundaries" => &config.effect_boundaries.get(index)?.name,
        "invalidation" => &config.invalidation.get(index)?.name,
        "import-boundaries" => &config.import_boundaries.get(index)?.name,
        _ => &config.readonly_returns.get(index)?.name,
    };
    Some(name)
}

/// Mark the reports that fail the run by the configuration, and give each contract report the
/// places of its contracts. Run once the reports of a project are final: a report standing for
/// several contracts fails the run when one of them says so, and points at each.
pub(crate) fn add_contract_facts(
    config: &HekiConfig,
    selection: &RuleSelection,
    diags: &mut [Diagnostic],
) {
    let kinds = HekiConfig::CONTRACT_KEYS;
    // `fail` per contract of each kind, by place in the kind's array; none without one.
    let fails: Vec<Option<Vec<bool>>> = (kinds.iter())
        .map(|kind| Some(fails_of(config, kind)).filter(|f| f.contains(&true)))
        .collect();
    for d in diags {
        d.fails_run_by_config = selection.fails_run(&d.rule_code);
        let Some(kind) = contract_kind(&d.rule_code) else {
            continue;
        };
        let k = kinds
            .iter()
            .position(|c| *c == kind)
            .expect("a contract kind");
        if let Some(fails) = &fails[k] {
            d.fails_run_by_config |= d
                .contracts
                .iter()
                .any(|&c| fails.get(c as usize).copied().unwrap_or(false));
        }
        d.contract_locations = contract_locations(config, kind, d);
    }
}

/// Where the configuration defines the contracts of `d`, of `kind`: each contract, then the
/// entry of its lists whose text is the report's target, when one is.
fn contract_locations(config: &HekiConfig, kind: &str, d: &Diagnostic) -> Vec<ContractLocation> {
    let sources = &config.sources;
    let mut out = Vec::new();
    for &c in &d.contracts {
        let c = c as usize;
        let Some(table) = sources.contract(kind, c) else {
            continue;
        };
        let name = name_of(config, kind, c).unwrap_or_default();
        let label = match heki_env::contract_label(kind, c, name) {
            named if !name.is_empty() => format!("{kind} contract {named}"),
            unnamed => unnamed,
        };
        out.push(location(table, label));
        if let Some((key, at)) = sources.entry_named(kind, c, &d.target) {
            let group = (at.place.group.as_ref())
                .map_or_else(String::new, |g| format!(" from group `{g}`"));
            let message = format!("`{key}` entry `{}`{group}", d.target);
            out.push(location(at, message));
        }
    }
    out
}

fn location(at: &DefinedAt, message: String) -> ContractLocation {
    ContractLocation {
        path: at.path.clone(),
        line: at.place.line,
        column: at.column,
        message,
    }
}
