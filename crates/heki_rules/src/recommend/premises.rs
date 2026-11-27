//! "What keeps reports heuristic": the heuristic reports of exact claims on the project, counted
//! by the premise each leaves open and why ([`crate::premises::OpenCause`]), or by the condition
//! of the exact tier that is no premise ([`crate::premises::ConditionCause`]), with what closes
//! the cause.
//!
//! The causes come from the reports themselves (`Diagnostic::open_records`, recorded by the
//! discovery run), never from the recommender: a report of a rule that records none is counted
//! as "not recorded", not given a cause.

use super::{MAX_EXAMPLES, MAX_LISTED, printed};
use crate::premises::{self, Closer};
use heki_diagnostics::{Certainty, Diagnostic, OpenRecord, RelatedLocation};
use schemars::JsonSchema;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

/// The heuristic reports of exact claims, by what keeps them heuristic.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct OpenPremises {
    /// The heuristic reports of exact claims, as `heki check` prints them with every rule
    /// selected: the heuristic reports of the exact rules.
    pub reports: usize,
    /// Those reports by the code they are reported under, the most first.
    pub by_rule: Vec<RuleReports>,
    /// The reports that record no cause, by code: they are counted under no row of `by_cause`.
    pub not_recorded: Vec<RuleReports>,
    /// One row per premise or condition left open and cause, the row that is the only open one
    /// of the most reports first, then the row of the most reports.
    pub by_cause: Vec<CauseRow>,
    /// The sets of more than one cause that reports leave open together, the commonest first:
    /// at most 50.
    pub combinations: Vec<Combination>,
    /// Whether `combinations` holds fewer sets than there are.
    pub truncated: bool,
}

/// The reports under one code.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RuleReports {
    pub rule: String,
    pub reports: usize,
}

/// What a report leaves open: a premise of its claim, which may then be false, or a condition
/// of the exact tier that is no premise, under which the claim holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum OpenKind {
    Premise,
    Condition,
}

/// The reports that leave one premise or condition open for one cause.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct CauseRow {
    pub kind: OpenKind,
    /// The premise's name in the premise table (docs/precision.md), or the condition's.
    pub name: String,
    /// The cause, unique within its premise or condition.
    pub cause: String,
    /// What the cause states.
    pub states: String,
    /// What closes the cause: `annotation` (an annotation the user writes, which must be
    /// true), `code` (a change of the code's shape), `heki-catalog` (a fact only heki's
    /// shipped catalog can hold), or `none` (nothing the user can apply to every case).
    pub closer: Closer,
    /// The reports that leave it open.
    pub reports: usize,
    /// The reports it is the only open premise or condition of.
    pub only_open: usize,
    /// The reports by the exact rule whose claim they report.
    pub rules: Vec<CauseRule>,
    /// The first 50 reports, in report order.
    pub examples: Vec<ReportAt>,
}

/// The reports of one rule's claim that leave a cause open.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct CauseRule {
    /// The exact rule whose claim the reports state.
    pub rule: String,
    pub reports: usize,
    pub only_open: usize,
}

/// One report, with where it leaves the cause open.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ReportAt {
    pub path: String,
    pub line: u32,
    pub column: u32,
    pub rule: String,
    /// Where the premise or condition is open: the call of a chain's hop, or a loop.
    pub at: Option<Location>,
    /// What the cause's statement leaves out: the function, value, or class it is about.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Location {
    pub path: String,
    pub line: u32,
    pub column: u32,
}

/// Causes that reports leave open together.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Combination {
    pub open: Vec<OpenRef>,
    pub reports: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct OpenRef {
    pub kind: OpenKind,
    pub name: String,
    pub cause: String,
}

/// One cause a report leaves open, as the table keeps it: its kind, the name of its premise or
/// condition, and its id.
type CauseKey = (OpenKind, &'static str, &'static str);

/// Whether `d` is a heuristic report of an exact claim, and of which rule's claim.
pub(super) fn claim_of(d: &Diagnostic) -> Option<&str> {
    (d.certainty == Certainty::Heuristic && !premises::of_rule(&d.rule_code).is_empty())
        .then_some(&d.rule_code)
}

/// The causes `d` leaves open, each once, in the order it records them.
fn causes_of(d: &Diagnostic) -> Vec<(CauseKey, &OpenRecord)> {
    let mut out: Vec<(CauseKey, &OpenRecord)> = Vec::new();
    for record in &d.open_records {
        let key = match record {
            OpenRecord::Premise(p) => (OpenKind::Premise, p.premise, p.cause),
            OpenRecord::Condition(c) => (OpenKind::Condition, c.condition, c.cause),
        };
        if !out.iter().any(|(k, _)| *k == key) {
            out.push((key, record));
        }
    }
    out
}

/// What the table says of a cause: its statement and closer, and where its row sorts among
/// rows of equal counts (the order of the premise table).
fn described(key: CauseKey) -> (&'static str, Closer, usize) {
    const RECORDED: &str = "every record names a cause of the premise table";
    match key.0 {
        OpenKind::Premise => {
            let cause = premises::cause(key.1, key.2).expect(RECORDED);
            let order = premises::CAUSES.iter().position(|c| *c == cause);
            (cause.states, cause.closer, order.expect(RECORDED))
        }
        OpenKind::Condition => {
            let cause = premises::condition_cause(key.1, key.2).expect(RECORDED);
            let order = premises::CONDITION_CAUSES.iter().position(|c| *c == cause);
            (
                cause.states,
                cause.closer,
                premises::CAUSES.len() + order.expect(RECORDED),
            )
        }
    }
}

/// The section, from `reports`, the discovery run's reports (with their open records), and with
/// paths written by `shown`.
pub(super) fn open_premises(
    reports: &[Diagnostic],
    shown: &dyn Fn(&Path) -> String,
) -> OpenPremises {
    let heuristic = printed(reports.iter().filter(|d| claim_of(d).is_some()).cloned());
    aggregate(&heuristic, shown)
}

/// [`open_premises`] of `heuristic`, the heuristic reports of exact claims as `heki check`
/// prints them.
fn aggregate(heuristic: &[Diagnostic], shown: &dyn Fn(&Path) -> String) -> OpenPremises {
    let location = |at: &RelatedLocation| Location {
        path: shown(&at.path),
        line: at.line,
        column: at.column,
    };
    let mut by_rule: BTreeMap<&str, usize> = BTreeMap::new();
    let mut not_recorded: BTreeMap<&str, usize> = BTreeMap::new();
    let mut rows: BTreeMap<CauseKey, CauseRow> = BTreeMap::new();
    let mut combinations: BTreeMap<Vec<CauseKey>, usize> = BTreeMap::new();
    for d in heuristic {
        *by_rule.entry(&d.rule_code).or_default() += 1;
        let causes = causes_of(d);
        if causes.is_empty() {
            *not_recorded.entry(&d.rule_code).or_default() += 1;
            continue;
        }
        let only = causes.len() == 1;
        if !only {
            let mut keys: Vec<CauseKey> = causes.iter().map(|(k, _)| *k).collect();
            keys.sort_unstable();
            *combinations.entry(keys).or_default() += 1;
        }
        let claim = claim_of(d).expect("a heuristic report of an exact claim");
        for (key, record) in causes {
            let row = rows.entry(key).or_insert_with(|| {
                let (states, closer, _) = described(key);
                CauseRow {
                    kind: key.0,
                    name: key.1.to_owned(),
                    cause: key.2.to_owned(),
                    states: states.to_owned(),
                    closer,
                    reports: 0,
                    only_open: 0,
                    rules: Vec::new(),
                    examples: Vec::new(),
                }
            });
            row.reports += 1;
            row.only_open += usize::from(only);
            match row.rules.iter_mut().find(|r| r.rule == claim) {
                Some(r) => {
                    r.reports += 1;
                    r.only_open += usize::from(only);
                }
                None => row.rules.push(CauseRule {
                    rule: claim.to_owned(),
                    reports: 1,
                    only_open: usize::from(only),
                }),
            }
            let (at, detail) = match record {
                OpenRecord::Premise(p) => (&p.at, &p.detail),
                OpenRecord::Condition(c) => (&c.at, &c.detail),
            };
            row.examples.push(ReportAt {
                path: shown(&d.path),
                line: d.line,
                column: d.column,
                rule: d.rule_code.clone(),
                at: at.as_ref().map(location),
                detail: detail.clone(),
            });
        }
    }
    let mut by_cause: Vec<(CauseKey, CauseRow)> = rows.into_iter().collect();
    by_cause.sort_by_key(|(key, row)| {
        (
            std::cmp::Reverse(row.only_open),
            std::cmp::Reverse(row.reports),
            described(*key).2,
        )
    });
    for (_, row) in &mut by_cause {
        row.examples.truncate(MAX_LISTED);
        row.rules.sort_by(|a, b| a.rule.cmp(&b.rule));
    }
    let mut combinations: Vec<(Vec<CauseKey>, usize)> = combinations.into_iter().collect();
    combinations.sort_by_key(|(_, reports)| std::cmp::Reverse(*reports));
    let truncated = combinations.len() > MAX_LISTED;
    let counted = |counts: BTreeMap<&str, usize>| {
        let mut out: Vec<RuleReports> = counts
            .into_iter()
            .map(|(rule, reports)| RuleReports {
                rule: rule.to_owned(),
                reports,
            })
            .collect();
        out.sort_by_key(|r| std::cmp::Reverse(r.reports));
        out
    };
    OpenPremises {
        reports: heuristic.len(),
        by_rule: counted(by_rule),
        not_recorded: counted(not_recorded),
        by_cause: by_cause.into_iter().map(|(_, row)| row).collect(),
        combinations: combinations
            .into_iter()
            .take(MAX_LISTED)
            .map(|(keys, reports)| Combination {
                open: keys
                    .into_iter()
                    .map(|(kind, name, cause)| OpenRef {
                        kind,
                        name: name.to_owned(),
                        cause: cause.to_owned(),
                    })
                    .collect(),
                reports,
            })
            .collect(),
        truncated,
    }
}

/// The closer as the text prints it.
fn closer_text(closer: Closer) -> &'static str {
    match closer {
        Closer::Annotation => "annotation",
        Closer::Code => "code",
        Closer::HekiCatalog => "heki's catalog",
        Closer::None => "none",
    }
}

/// The section as the text prints it.
pub(super) fn render(out: &mut String, section: &OpenPremises) {
    out.push_str("\nWhat keeps reports heuristic\n");
    if section.reports == 0 {
        out.push_str("  No heuristic reports of exact claims here.\n");
        return;
    }
    let per_rule = |rules: &[RuleReports]| {
        rules
            .iter()
            .map(|r| format!("{} {}", r.rule, r.reports))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let _ = writeln!(
        out,
        "  {} heuristic report(s) of exact claims: {}",
        section.reports,
        per_rule(&section.by_rule)
    );
    let label = |row: &CauseRow| match row.kind {
        OpenKind::Premise => format!("{}: {}", row.name, row.cause),
        OpenKind::Condition => format!("{} (condition): {}", row.name, row.cause),
    };
    let width = section
        .by_cause
        .iter()
        .map(|row| label(row).len())
        .max()
        .unwrap_or_default()
        .max("premise: cause".len());
    if !section.by_cause.is_empty() {
        let _ = writeln!(
            out,
            "  {:<width$}  reports  only open  closed by",
            "premise: cause"
        );
    }
    for row in &section.by_cause {
        let _ = writeln!(
            out,
            "  {:<width$}  {:>7}  {:>9}  {}",
            label(row),
            row.reports,
            row.only_open,
            closer_text(row.closer)
        );
        let _ = writeln!(out, "    {}", row.states);
        let examples: Vec<String> = row
            .examples
            .iter()
            .take(MAX_EXAMPLES)
            .map(|e| format!("{}:{} {}", e.path, e.line, e.rule))
            .collect();
        let more = if row.reports > MAX_EXAMPLES {
            ", ..."
        } else {
            ""
        };
        let _ = writeln!(out, "    e.g. {}{more}", examples.join(", "));
    }
    if !section.not_recorded.is_empty() {
        let _ = writeln!(
            out,
            "  causes not recorded yet: {}",
            per_rule(&section.not_recorded)
        );
    }
    if !section.by_cause.is_empty() {
        out.push_str(
            "  Nothing here enters the block: an annotation or a change of the code is an edit \
             of the program.\n",
        );
    }
    if section
        .by_cause
        .iter()
        .any(|row| row.closer == Closer::HekiCatalog)
    {
        let _ = writeln!(
            out,
            "  heki's catalog takes no configuration for the causes it closes: report a case at \
             {}.",
            super::ISSUES_URL
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use heki_diagnostics::{OpenCondition, OpenPremise};
    use std::path::PathBuf;

    fn report(rule: &str, line: u32, open: &[(&'static str, &'static str)]) -> Diagnostic {
        Diagnostic {
            path: PathBuf::from("app.py"),
            line,
            column: 1,
            span: heki_ast::Span { start: 0, end: 1 },
            qualname: "f".into(),
            rule_code: rule.into(),
            rule_slug: String::new(),
            target: String::new(),
            severity: heki_env::Severity::Warning,
            certainty: Certainty::Heuristic,
            message: String::new(),
            help: None,
            evidence: None,
            blocker: None,
            related: Vec::new(),
            contract_locations: Vec::new(),
            fix: None,
            sites: 1,
            fold_key: None,
            open_records: open
                .iter()
                .map(
                    |(name, cause)| match premises::condition_cause(name, cause) {
                        Some(c) => OpenRecord::Condition(OpenCondition {
                            axis: c.condition.axis,
                            condition: name,
                            cause,
                            at: None,
                            detail: None,
                        }),
                        None => OpenRecord::Premise(OpenPremise {
                            premise: name,
                            cause,
                            at: None,
                            detail: None,
                        }),
                    },
                )
                .collect(),
            contracts: Vec::new(),
            allowed_examples: Vec::new(),
            group: false,
            key_target: None,
            key_module: None,
            fails_run_by_config: false,
        }
    }

    /// `(name, cause, reports, only open)` of each row, in order.
    fn rows(section: &OpenPremises) -> Vec<(String, String, usize, usize)> {
        section
            .by_cause
            .iter()
            .map(|r| (r.name.clone(), r.cause.clone(), r.reports, r.only_open))
            .collect()
    }

    const RETURN: (&str, &str) = ("Every return", "not-made-on-every-return");
    const TARGETS: (&str, &str) = ("Every implementation", "incomplete-targets");
    const UNIQUE: (&str, &str) = ("Rewrite proven safe", "unique");
    const TRIPS: (&str, &str) = ("Trip count open", "trip-count-fixed");

    #[test]
    fn a_cause_is_the_only_open_one_of_the_reports_that_leave_nothing_else_open() {
        for (reports, expected, combinations) in [
            // One cause each: every report counts it as its only open one.
            (
                vec![
                    report("EFF101", 1, &[RETURN]),
                    report("EFF101", 2, &[RETURN]),
                ],
                vec![("Every return", "not-made-on-every-return", 2, 2)],
                0,
            ),
            // Two causes on one report: each counts it, neither as its only open one; the
            // row of more reports leaving it alone open comes first.
            (
                vec![
                    report("EFF101", 1, &[RETURN, TARGETS]),
                    report("EFF101", 2, &[TARGETS]),
                ],
                vec![
                    ("Every implementation", "incomplete-targets", 2, 1),
                    ("Every return", "not-made-on-every-return", 1, 0),
                ],
                1,
            ),
            // A cause recorded twice on one report counts once, and stays its only open one.
            (
                vec![report("OPT110", 1, &[UNIQUE, UNIQUE])],
                vec![("Rewrite proven safe", "unique", 1, 1)],
                0,
            ),
            // A condition counts as an open record: the premise is not the only one open.
            (
                vec![
                    report("OPT101", 1, &[UNIQUE, TRIPS]),
                    report("OPT101", 2, &[TRIPS]),
                ],
                vec![
                    ("Trip count open", "trip-count-fixed", 2, 1),
                    ("Rewrite proven safe", "unique", 1, 0),
                ],
                1,
            ),
            // Equal counts follow the order of the premise table.
            (
                vec![
                    report("OPT110", 1, &[UNIQUE]),
                    report("EFF101", 2, &[RETURN]),
                ],
                vec![
                    ("Every return", "not-made-on-every-return", 1, 1),
                    ("Rewrite proven safe", "unique", 1, 1),
                ],
                0,
            ),
            // A report recording no cause is counted under none.
            (vec![report("MUT114", 1, &[])], vec![], 0),
        ] {
            let section = aggregate(&reports, &|p| p.display().to_string());
            let expected: Vec<(String, String, usize, usize)> = expected
                .into_iter()
                .map(|(n, c, r, o)| (n.to_owned(), c.to_owned(), r, o))
                .collect();
            assert_eq!(rows(&section), expected);
            assert_eq!(section.reports, reports.len());
            assert_eq!(section.combinations.len(), combinations);
            let recorded = reports
                .iter()
                .filter(|d| !d.open_records.is_empty())
                .count();
            let not_recorded: usize = section.not_recorded.iter().map(|r| r.reports).sum();
            assert_eq!(recorded + not_recorded, reports.len());
        }
    }

    #[test]
    fn reports_without_a_recorded_cause_are_said_so() {
        let mut reports = vec![report("MUT114", 1, &[])];
        reports.extend((2..=5).map(|line| report("OPT110", line, &[UNIQUE])));
        let section = aggregate(&reports, &|p| p.display().to_string());
        let mut out = String::new();
        render(&mut out, &section);
        assert!(
            out.contains("\n  causes not recorded yet: MUT114 1\n"),
            "{out}"
        );
        assert!(
            out.contains("\n    e.g. app.py:2 OPT110, app.py:3 OPT110, app.py:4 OPT110, ...\n"),
            "{out}"
        );
    }

    #[test]
    fn only_the_heuristic_reports_of_exact_rules_are_claims() {
        let section = aggregate(&[report("EFF101", 1, &[RETURN])], &|p| {
            p.display().to_string()
        });
        assert_eq!(section.by_cause[0].rules[0].rule, "EFF101");
        let mut exact_tier = report("OPT110", 1, &[]);
        assert_eq!(claim_of(&exact_tier), Some("OPT110"));
        exact_tier.certainty = Certainty::Exact;
        assert_eq!(claim_of(&exact_tier), None, "heuristic reports only");
        assert_eq!(claim_of(&report("EFF106", 1, &[])), None, "no exact claim");
    }
}
