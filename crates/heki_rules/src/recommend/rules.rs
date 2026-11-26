//! "Rules worth enabling here": every rule the current selection leaves off, and every exact
//! rule whose heuristic reports it withholds, with the reports it would add on this project,
//! the published measurement of how often its reports are right, and whether the block enables
//! it ([`RULE_POLICY`]).

use super::{Analyzed, BaselineSplit, printed};
use crate::experimental::experimental_rule;
use crate::precision::{self, PublishedMeasurement};
use crate::project::{ParsedFile, regressed};
use crate::selection::RuleSelection;
use crate::{Activation, CORE_RULES, Certainty, Consequence, RuleMeta};
use heki_diagnostics::{Diagnostic, RatchetBaseline};
use schemars::JsonSchema;
use serde::Serialize;
use std::fmt::Write as _;

/// How `heki recommend` treats a rule the current selection does not run in full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RulePolicy {
    /// The block enables it when it reports here, it is not experimental, and no published
    /// measurement shows real harm below [`MIN_REAL_HARM_PERCENT`]: it reports bugs that can be
    /// critical, whose exact reports stop a run (ADR-019).
    Enable,
    /// Listed with its reports, never enabled by the block: it reports slower code, a bug that
    /// is rarely harmful, or a bug whose harm is not assessed yet, worth reading but not worth
    /// failing a run for.
    List,
    /// `MUT103` compares with a summary snapshot the user saves: listed, never counted.
    Snapshot,
    /// A contract rule: declaring its contract selects it, so contract candidates cover it.
    Contract,
}

/// The reason of a rule the configuration ignores: `ignore` wins over `extend-select`, so naming
/// it shows none of its reports.
const IGNORED: &str = "the configuration ignores it";

/// The least share of real harm, in percent, a published measurement of a rule the block
/// enables may show.
pub const MIN_REAL_HARM_PERCENT: u32 = 10;

/// Every rule, with how `heki recommend` treats it.
pub const RULE_POLICY: &[(&str, RulePolicy)] = &[
    ("MUT101", RulePolicy::Enable),
    ("MUT103", RulePolicy::Snapshot),
    ("MUT106", RulePolicy::Enable),
    ("MUT107", RulePolicy::Enable),
    ("MUT108", RulePolicy::Enable),
    ("MUT110", RulePolicy::List),
    ("MUT111", RulePolicy::Enable),
    ("MUT112", RulePolicy::Enable),
    ("MUT113", RulePolicy::Enable),
    ("MUT114", RulePolicy::Enable),
    ("MUT115", RulePolicy::Enable),
    ("MUT118", RulePolicy::Enable),
    ("MUT119", RulePolicy::Enable),
    ("MUT120", RulePolicy::Enable),
    ("MUT201", RulePolicy::Contract),
    ("MUT202", RulePolicy::Contract),
    ("MUT203", RulePolicy::Contract),
    ("MUT204", RulePolicy::Contract),
    ("MUT205", RulePolicy::Contract),
    ("MUT206", RulePolicy::Contract),
    ("MUT207", RulePolicy::Contract),
    ("MUT208", RulePolicy::Contract),
    ("IMP101", RulePolicy::List),
    ("IMP102", RulePolicy::Enable),
    ("IMP104", RulePolicy::Enable),
    ("IMP201", RulePolicy::Contract),
    ("IMP202", RulePolicy::Contract),
    ("IMP301", RulePolicy::Enable),
    ("IMP302", RulePolicy::Enable),
    ("EFF101", RulePolicy::Enable),
    ("EFF112", RulePolicy::List),
    ("EFF102", RulePolicy::List),
    ("EFF103", RulePolicy::List),
    ("EFF114", RulePolicy::List),
    ("EFF115", RulePolicy::List),
    ("EFF116", RulePolicy::List),
    ("EFF117", RulePolicy::List),
    ("EFF105", RulePolicy::List),
    ("EFF106", RulePolicy::Enable),
    ("EFF107", RulePolicy::Enable),
    ("EFF109", RulePolicy::Enable),
    ("EFF110", RulePolicy::Enable),
    ("EFF201", RulePolicy::Contract),
    ("DATA101", RulePolicy::List),
    ("OPT101", RulePolicy::List),
    ("OPT102", RulePolicy::List),
    ("OPT103", RulePolicy::List),
    ("OPT110", RulePolicy::List),
    ("OPT111", RulePolicy::List),
    ("OPT112", RulePolicy::List),
    ("OPT113", RulePolicy::List),
    ("OPT120", RulePolicy::List),
    ("OPT121", RulePolicy::List),
    ("OPT130", RulePolicy::List),
];

/// The policy of the rule `code`.
pub fn policy(code: &str) -> RulePolicy {
    RULE_POLICY
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, p)| *p)
        .expect("every rule has a policy")
}

/// One rule the current selection does not run in full.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RuleAdvice {
    pub code: String,
    pub slug: String,
    #[schemars(extend("enum" = ["exact", "heuristic"]))]
    pub certainty: &'static str,
    pub activation: Activation,
    pub experimental: bool,
    pub consequence: Consequence,
    /// Whether the current selection runs the rule: then only its heuristic reports, which it
    /// withholds, are counted.
    pub enabled_now: bool,
    /// The reports naming the rule in `extend-select` would add to what `heki check` prints
    /// (`null` for a rule that is not counted).
    pub reports: Option<usize>,
    pub exact: usize,
    pub heuristic: usize,
    /// With a baseline: the added reports it allows, and the regressions.
    pub baseline: Option<BaselineSplit>,
    /// The latest published measurement of how often the rule's reports are right.
    pub measurement: Option<PublishedMeasurement>,
    /// Whether the block enables the rule.
    pub in_block: bool,
    /// Why the rule is in the block or not.
    pub reason: String,
    pub caveats: Vec<String>,
}

/// What the section is computed from.
pub(super) struct Inputs<'a> {
    pub current: &'a RuleSelection,
    pub baseline: Option<&'a RatchetBaseline>,
    pub files: &'a [ParsedFile],
    /// The discovery run's reports, after inline suppressions.
    pub discovery: &'a [Diagnostic],
}

/// The advice on every rule the current selection does not run in full, the block's rules
/// first, then by reports and code.
pub(super) fn advise(inputs: &Inputs<'_>) -> Vec<RuleAdvice> {
    let mut out: Vec<RuleAdvice> = CORE_RULES
        .iter()
        .filter_map(|rule| advice(inputs, rule))
        .collect();
    order(&mut out);
    out
}

/// The block's rules first, then by reports and code.
fn order(rules: &mut [RuleAdvice]) {
    rules.sort_by(|a, b| {
        (
            !a.in_block,
            a.experimental,
            std::cmp::Reverse(a.reports),
            &a.code,
        )
            .cmp(&(
                !b.in_block,
                b.experimental,
                std::cmp::Reverse(b.reports),
                &b.code,
            ))
    });
}

/// The counts of the rules the block enables, from `inputs`, the reports of the verification
/// run: `heki check` with the block applied.
pub(super) fn recount(rules: &mut [RuleAdvice], inputs: &Inputs<'_>) {
    for advice in rules.iter_mut().filter(|r| r.in_block) {
        let meta = crate::find_rule(&advice.code).expect("advice is on a registered rule");
        if let Some(verified) = self::advice(inputs, meta) {
            advice.reports = verified.reports;
            advice.exact = verified.exact;
            advice.heuristic = verified.heuristic;
            advice.baseline = verified.baseline;
        }
    }
    order(rules);
}

fn advice(inputs: &Inputs<'_>, rule: &'static RuleMeta) -> Option<RuleAdvice> {
    let current = inputs.current;
    let enabled_now = current.is_enabled(rule.code);
    let policy = policy(rule.code);
    if policy == RulePolicy::Contract || (enabled_now && !current.withholds_heuristic(rule.code)) {
        return None;
    }
    let of_rule = |d: &&Diagnostic| d.rule_code == rule.code;
    let mut advice = RuleAdvice {
        code: rule.code.to_owned(),
        slug: rule.slug.to_owned(),
        certainty: rule.certainty.word(),
        activation: rule.activation,
        experimental: experimental_rule(rule.code).is_some(),
        consequence: rule.consequence,
        enabled_now,
        reports: None,
        exact: 0,
        heuristic: 0,
        baseline: None,
        measurement: precision::measurement(rule.code).map(|m| m.published()),
        in_block: false,
        reason: String::new(),
        caveats: Vec::new(),
    };
    if policy == RulePolicy::Snapshot {
        advice.reason = "it compares with a saved summary snapshot: run `heki snapshot --write`, \
            then select it"
            .to_owned();
        return Some(advice);
    }
    // What naming the rule adds: all its reports, or the heuristic ones it withholds now.
    let (shown, added): (Vec<&Diagnostic>, Vec<&Diagnostic>) = inputs
        .discovery
        .iter()
        .filter(of_rule)
        .partition(|d| enabled_now && d.certainty == Certainty::Exact);
    if enabled_now && added.is_empty() {
        return None;
    }
    let added_printed = printed(added.iter().map(|d| (*d).clone()));
    advice.exact = added_printed
        .iter()
        .filter(|d| d.certainty == Certainty::Exact)
        .count();
    advice.heuristic = added_printed.len() - advice.exact;
    advice.reports = Some(added_printed.len());
    advice.baseline = inputs
        .baseline
        .map(|baseline| split(baseline, inputs.files, &shown, &added));
    (advice.in_block, advice.reason) = decide(current, rule, policy, &advice);
    if advice.in_block && advice.measurement.is_none() {
        advice.caveats.push("not measured".to_owned());
    }
    Some(advice)
}

/// The allowed reports and the regressions among `added`, the rule's reports naming it adds to
/// `shown`, its reports shown now: a baseline counts a rule's reports of one function and
/// target together.
fn split(
    baseline: &RatchetBaseline,
    files: &[ParsedFile],
    shown: &[&Diagnostic],
    added: &[&Diagnostic],
) -> BaselineSplit {
    let all: Vec<Diagnostic> = shown.iter().chain(added).map(|d| (*d).clone()).collect();
    let flags = regressed(baseline, &all, files);
    let (regressions, allowed): (Vec<_>, Vec<_>) = all
        .into_iter()
        .zip(flags)
        .skip(shown.len())
        .partition(|(_, regressed)| *regressed);
    BaselineSplit {
        allowed: printed(allowed.into_iter().map(|(d, _)| d)).len(),
        regressions: printed(regressions.into_iter().map(|(d, _)| d)).len(),
    }
}

/// Whether the block enables the rule, and why.
fn decide(
    current: &RuleSelection,
    rule: &RuleMeta,
    policy: RulePolicy,
    advice: &RuleAdvice,
) -> (bool, String) {
    let out = |reason: &str| (false, reason.to_owned());
    if current.is_ignored(rule.code) {
        return out(IGNORED);
    }
    if advice.experimental {
        return out("experimental");
    }
    if advice.reports == Some(0) {
        return out("no reports here");
    }
    if policy != RulePolicy::Enable {
        return match rule.consequence {
            Consequence::Slowdown => out("slower code, not a bug"),
            Consequence::Bug if policy == RulePolicy::List => out("listed for review"),
            Consequence::MinorBug | Consequence::Bug => out("a bug that is rarely harmful"),
        };
    }
    let harmless = precision::measurement(rule.code)
        .filter(|m| m.real_harm_below(MIN_REAL_HARM_PERCENT))
        .is_some();
    if harmless {
        return out(&format!("real harm below {MIN_REAL_HARM_PERCENT}%"));
    }
    (true, "a bug, reported here".to_owned())
}

/// The `heki check` command that shows the reports the section counts, and how many it adds.
pub(super) struct ShownByCheck {
    pub command: String,
    pub reports: usize,
    pub exact: usize,
    pub heuristic: usize,
    /// With a baseline: the regressions among them, which a run with the baseline prints.
    pub regressions: usize,
}

/// The command naming in `extend-select` every rule with reports here, in the order the section
/// lists them, but those the configuration ignores; `None` when there is none. A rule's reports do
/// not depend on the other rules selected, so its count holds among the others.
pub(super) fn shown_by_check(rules: &[RuleAdvice], analyzed: &Analyzed) -> Option<ShownByCheck> {
    let mut named: Vec<&RuleAdvice> = (rules.iter())
        .filter(|r| r.reports > Some(0) && r.reason != IGNORED)
        .collect();
    named.sort_by_key(|r| r.experimental);
    if named.is_empty() {
        return None;
    }
    let codes: Vec<&str> = named.iter().map(|r| r.code.as_str()).collect();
    let mut command = "heki check".to_owned();
    if analyzed.paths != ["."] {
        for word in &analyzed.paths {
            command.push(' ');
            command.push_str(&super::command_word(word));
        }
    }
    for word in &analyzed.check_options {
        command.push(' ');
        command.push_str(&super::command_word(word));
    }
    command.push_str(" --extend-select ");
    command.push_str(&codes.join(","));
    Some(ShownByCheck {
        command,
        reports: named.iter().map(|r| r.reports.unwrap_or_default()).sum(),
        exact: named.iter().map(|r| r.exact).sum(),
        heuristic: named.iter().map(|r| r.heuristic).sum(),
        regressions: (named.iter())
            .filter_map(|r| r.baseline.as_ref())
            .map(|b| b.regressions)
            .sum(),
    })
}

/// The measurement as the text prints it.
fn measured(measurement: Option<&PublishedMeasurement>) -> (String, String) {
    match measurement {
        Some(m) => (
            format!(
                "{}% ({}-{}%), n={}, {}",
                m.right_percent, m.interval_percent[0], m.interval_percent[1], m.sample, m.date
            ),
            format!("{}%", m.real_harm_percent),
        ),
        None => ("not measured".to_owned(), "-".to_owned()),
    }
}

/// The section as the text prints it. `analyzed` names the paths and options of the command showing the
/// reports counted names. The heuristic rules without reports are named with `text.all`, and the
/// rules not counted (`MUT103`) with `text.all` or when its snapshot is saved.
pub(super) fn render(
    out: &mut String,
    rules: &[RuleAdvice],
    baseline: bool,
    analyzed: &Analyzed,
    text: super::TextOptions,
) {
    out.push_str("\nRules worth enabling here\n");
    let regressions = if baseline { "  regressions" } else { "" };
    let _ = writeln!(
        out,
        "  rule    reports  exact{regressions}  consequence  right about the code (published)  \
         real harm  block"
    );
    let counted: Vec<&RuleAdvice> = rules.iter().filter(|r| r.reports > Some(0)).collect();
    let row = |out: &mut String, r: &RuleAdvice| {
        let (right, harm) = measured(r.measurement.as_ref());
        let exact = if r.certainty == Certainty::Heuristic.word() {
            "-".to_owned()
        } else {
            r.exact.to_string()
        };
        let regressions = match &r.baseline {
            Some(split) if baseline => format!("  {:>11}", split.regressions),
            _ => String::new(),
        };
        let consequence = match r.consequence {
            Consequence::Bug => "bug",
            Consequence::MinorBug => "minor bug",
            Consequence::Slowdown => "slowdown",
        };
        let mut block = if r.in_block {
            "yes".to_owned()
        } else {
            format!("no: {}", r.reason)
        };
        if r.enabled_now {
            block.push_str(" (on now: naming it adds its heuristic reports)");
        }
        let reports = r.reports.unwrap_or_default();
        let _ = writeln!(
            out,
            "  {:<6}  {reports:>7}  {exact:>5}{regressions}  {consequence:<11}  {right:<32}  \
             {harm:<9}  {block}",
            r.code
        );
    };
    for r in counted.iter().filter(|r| !r.experimental) {
        row(out, r);
    }
    if counted.iter().any(|r| r.experimental) {
        out.push_str("  Optional (experimental, never in the block)\n");
        for r in counted.iter().filter(|r| r.experimental) {
            row(out, r);
        }
    }
    if let Some(shown) = shown_by_check(rules, analyzed) {
        let regressions = if baseline {
            format!(", {} regression(s) among them", shown.regressions)
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "  see the {} report(s){regressions}: {}",
            shown.reports, shown.command
        );
    }
    quiet_rules(out, rules, text.all);
    if text.all || text.snapshot_saved {
        for r in rules.iter().filter(|r| r.reports.is_none()) {
            let _ = writeln!(out, "  {}: {}.", r.code, r.reason);
        }
    }
}

/// The rules without reports here, on one line: the exact ones by code, the experimental ones
/// apart, and the heuristic ones counted, or with `all` named. Every one is off: a rule that
/// runs and withholds nothing is not listed.
fn quiet_rules(out: &mut String, rules: &[RuleAdvice], all: bool) {
    let mut quiet: Vec<&RuleAdvice> = rules.iter().filter(|r| r.reports == Some(0)).collect();
    quiet.sort_unstable_by(|a, b| a.code.cmp(&b.code));
    let exact = |r: &&&RuleAdvice| r.certainty == Certainty::Exact.word();
    let codes = |rules: Vec<&&RuleAdvice>| -> String {
        let codes: Vec<&str> = rules.iter().map(|r| r.code.as_str()).collect();
        codes.join(", ")
    };
    let mut parts = vec![format!("{} rule(s)", quiet.len())];
    let (stable, experimental): (Vec<_>, Vec<_>) =
        quiet.iter().filter(exact).partition(|r| !r.experimental);
    if !stable.is_empty() {
        parts.push(format!("exact: {}", codes(stable)));
    }
    if !experimental.is_empty() {
        parts.push(format!("experimental and exact: {}", codes(experimental)));
    }
    let heuristic: Vec<&&RuleAdvice> = quiet.iter().filter(|r| !exact(r)).collect();
    if all {
        parts.push(format!("heuristic: {}", codes(heuristic)));
    } else {
        parts.push(format!(
            "heki recommend --all names the {} heuristic one(s)",
            heuristic.len()
        ));
    }
    if !quiet.is_empty() {
        let _ = writeln!(out, "  No reports here: {}", parts.join("; "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rule_has_one_policy_that_its_metadata_allows() {
        for rule in CORE_RULES {
            let entries = RULE_POLICY.iter().filter(|(c, _)| *c == rule.code).count();
            assert_eq!(entries, 1, "{}: one entry in RULE_POLICY", rule.code);
            match policy(rule.code) {
                // Only a bug is worth enabling for (ADR-019).
                RulePolicy::Enable => {
                    assert_eq!(rule.consequence, Consequence::Bug, "{}", rule.code)
                }
                RulePolicy::Contract => {
                    assert_eq!(rule.activation, Activation::Contract, "{}", rule.code);
                }
                RulePolicy::Snapshot => assert_eq!(rule.code, "MUT103"),
                RulePolicy::List => {}
            }
            if rule.activation == Activation::Contract {
                assert_eq!(policy(rule.code), RulePolicy::Contract, "{}", rule.code);
            }
        }
        assert_eq!(RULE_POLICY.len(), CORE_RULES.len());
    }
}
