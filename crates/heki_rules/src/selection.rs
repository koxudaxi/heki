//! Rule selection (docs/configuration.md, Ruff precedence).
//!
//! - Default: every `CORE_RULES` entry that runs without being named
//!   ([`RuleMeta::runs_unnamed`]): the exact `Default` rules and the `Contract` rules, and with
//!   `lint.heuristic` the heuristic `Default` rules too (Ruff's `preview`).
//! - `select` (CLI over config) replaces the default set (an empty `select` in the
//!   configuration selects no rule); `extend-select` adds; `ignore` and
//!   `per-file-ignores` remove.
//! - A selector is a code (`EFF105`), a slug (`async-blocking-fs`), or a code prefix
//!   (`EFF`, `MUT2`, `ALL`). A prefix enables only the rules that run without being named: opt-in
//!   rules such as `EFF105`, and heuristic rules without `lint.heuristic`, need their exact code
//!   or slug. `ignore` prefixes cover every matching rule.
//! - An exact rule whose single reports can be heuristic (an `OPT` report whose rewrite is not
//!   proven) shows those reports only where a heuristic rule would run: with `lint.heuristic`,
//!   when its code or slug is selected, or for a contract rule.
//! - `--include-size-dependent` selects the rules whose size-dependent rewrites it applies, as if
//!   `extend-select` named them.

use crate::{CORE_RULES, Certainty, RuleMeta, find_rule};
use heki_diagnostics::Diagnostic;
use heki_env::glob::AnyGlob;
use heki_env::{Docs, Explain, LintConfig, Problem, Setting};
use std::collections::BTreeSet;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SelectionError {
    /// A selector that names no rule, in the list `origin` (`--select`, `lint.ignore`,
    /// `lint.per-file-ignores`, `lint.severity`).
    #[error("unknown rule selector `{selector}` in `{origin}`")]
    UnknownRule {
        selector: String,
        origin: &'static str,
    },
    #[error("invalid glob `{pattern}` in `lint.per-file-ignores`: {cause}")]
    Glob {
        pattern: String,
        cause: heki_env::glob::Error,
    },
}

impl Explain for SelectionError {
    fn explain(&self) -> Problem {
        let problem = Problem::new(self.to_string()).docs(Docs::Lint);
        match self {
            Self::UnknownRule { selector, origin } => {
                let setting = match *origin {
                    "lint.severity" => Some(Setting {
                        table: "lint.severity".to_owned(),
                        key: Some(selector.clone()),
                        ..Setting::default()
                    }),
                    "lint.per-file-ignores" => Some(Setting {
                        table: "lint.per-file-ignores".to_owned(),
                        value: Some(selector.clone()),
                        ..Setting::default()
                    }),
                    lint if lint.starts_with("lint.") => {
                        Some(Setting::of_context(lint, Some(selector)))
                    }
                    _ => None,
                };
                let problem = unknown_rule(problem, selector, true);
                match setting {
                    Some(setting) => problem.setting(setting),
                    None => problem,
                }
            }
            Self::Glob { pattern, .. } => problem
                .help("a key of `per-file-ignores` is a glob of project-relative paths, as `\"tests/**\"`")
                .setting(Setting {
                    table: "lint.per-file-ignores".to_owned(),
                    key: Some(pattern.clone()),
                    ..Setting::default()
                }),
        }
    }
}

/// `problem` about `selector`, a word that names no rule, with the rules it might have meant;
/// `prefixes`: whether a prefix (`EFF`, `ALL`) would do. A selector with digits is a misspelled
/// code, so only codes are offered for it.
pub fn unknown_rule(problem: Problem, selector: &str, prefixes: bool) -> Problem {
    let code_like = selector.chars().any(|c| c.is_ascii_digit());
    let families: BTreeSet<&str> = CORE_RULES
        .iter()
        .map(|r| r.code.trim_end_matches(|c: char| c.is_ascii_digit()))
        .chain(["ALL"])
        .filter(|_| prefixes && !code_like)
        .collect();
    let names = CORE_RULES
        .iter()
        .flat_map(|r| [r.code, r.slug])
        .chain(families.iter().copied());
    let mut candidates: Vec<String> = Vec::new();
    let word = selector.trim().to_lowercase();
    // `nearest` is as loose as `clap`; a rule name is offered only for a small typo (`NOPE1` is
    // not a misspelled `OPT101`): one edit for a prefix, two for a code or a slug.
    let close = |name: &&str| {
        let allowed = if name.len() > 3 { 2 } else { 1 };
        strsim::damerau_levenshtein(&word, &name.to_lowercase()) <= allowed
    };
    for name in heki_env::problem::nearest(&word, names)
        .into_iter()
        .filter(close)
    {
        let code = find_rule(name).map_or(name, |r| r.code);
        if !candidates.iter().any(|c| c == code) {
            candidates.push(code.to_owned());
        }
    }
    let mut problem = problem;
    if let Some(first) = candidates.first() {
        let named = find_rule(first).map_or_else(
            || format!("`{first}`"),
            |r| format!("`{}` ({})", r.code, r.slug),
        );
        problem = problem.help(format!("did you mean {named}?"));
    }
    let what = if prefixes {
        "a selector is a rule code (`EFF101`), a rule name (`async-blocking-call`), or a code prefix (`EFF`, `ALL`)"
    } else {
        "a rule is named by its code (`EFF101`) or its name (`async-blocking-call`)"
    };
    problem.candidates = candidates;
    problem
        .help(what)
        .help("`heki rule --all` lists every rule, with its code and name")
}

/// The selection options of the command line, over the configuration (`--select`,
/// `--extend-select`, `--ignore`, `--heuristic` / `--no-heuristic`, `--include-size-dependent`).
#[derive(Debug, Clone, Default)]
pub struct Selectors {
    pub select: Vec<String>,
    pub extend_select: Vec<String>,
    pub ignore: Vec<String>,
    /// `None`: `lint.heuristic` decides.
    pub heuristic: Option<bool>,
    /// `--include-size-dependent`: the rules with a size-dependent rewrite
    /// ([`RuleMeta::size_dependent_fix`]) run as if `extend-select` named them, since applying
    /// their rewrites is what the option asks for.
    pub size_dependent_rewrites: bool,
}

#[derive(Debug, Clone)]
pub struct RuleSelection {
    enabled: BTreeSet<&'static str>,
    per_file: Vec<(AnyGlob, BTreeSet<&'static str>)>,
    /// The rules `lint.heuristic` would add to `enabled`.
    heuristic_off: BTreeSet<&'static str>,
    /// The enabled rules whose heuristic reports are withheld: exact rules that run without
    /// being named while `lint.heuristic` is off.
    exact_reports_only: BTreeSet<&'static str>,
    /// `lint.heuristic`, the command line's over the configuration's.
    heuristic: bool,
    /// The rules `ignore` names (by code, slug, or prefix).
    ignored: BTreeSet<&'static str>,
    /// The rules `lint.fail` names (by code, slug, or prefix): their reports fail the run.
    fail: BTreeSet<&'static str>,
}

/// The selectors a run applies: the command line's `select` over the configuration's (`None`:
/// the default rules), and the `extend-select` and `ignore` lists of both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveSelectors {
    pub select: Option<Vec<String>>,
    pub extend_select: Vec<String>,
    pub ignore: Vec<String>,
}

impl EffectiveSelectors {
    pub fn of(lint: &LintConfig, cli: &Selectors) -> Self {
        let (select, [config, cli_extend]) = selecting(lint, cli);
        Self {
            select: select.map(|(s, _)| s.to_vec()),
            extend_select: [config.0, cli_extend.0].concat(),
            ignore: [lint.ignore.as_slice(), &cli.ignore].concat(),
        }
    }
}

/// The rule a code or slug names.
fn named(selector: &str) -> Option<&'static RuleMeta> {
    let s = selector.trim();
    CORE_RULES
        .iter()
        .find(|r| r.code.eq_ignore_ascii_case(s) || r.slug.eq_ignore_ascii_case(s))
}

/// Rules a selector names: a code or slug names its rule, and a prefix or `ALL` the matching
/// rules `admits`.
fn resolve(
    selector: &str,
    admits: &dyn Fn(&RuleMeta) -> bool,
    origin: &'static str,
) -> Result<Vec<&'static RuleMeta>, SelectionError> {
    let s = selector.trim();
    if let Some(r) = named(s) {
        return Ok(vec![r]);
    }
    let all = s.eq_ignore_ascii_case("ALL");
    let matching: Vec<&RuleMeta> = CORE_RULES
        .iter()
        .filter(|r| all || r.code.len() > s.len() && r.code[..s.len()].eq_ignore_ascii_case(s))
        .collect();
    if s.is_empty() || matching.is_empty() {
        return Err(SelectionError::UnknownRule {
            selector: selector.to_owned(),
            origin,
        });
    }
    Ok(matching.into_iter().filter(|r| admits(r)).collect())
}

/// The rules the selectors of the list `origin` name.
fn codes(
    (selectors, origin): SelectorList<'_>,
    admits: &dyn Fn(&RuleMeta) -> bool,
) -> Result<BTreeSet<&'static str>, SelectionError> {
    let mut out = BTreeSet::new();
    for s in selectors {
        let rules = resolve(s, admits, origin)?;
        out.extend(rules.into_iter().map(|r| r.code));
    }
    Ok(out)
}

/// A list of selectors and where it is written (`--select`, `lint.select`).
type SelectorList<'s> = (&'s [String], &'static str);

/// Every rule a prefix of `ignore` or `per-file-ignores` matches.
fn any_rule(_: &RuleMeta) -> bool {
    true
}

/// The effective `select` (the command line's over the configuration's; `None`: the default
/// set) and the `extend-select` lists.
fn selecting<'s>(
    lint: &'s LintConfig,
    cli: &'s Selectors,
) -> (Option<SelectorList<'s>>, [SelectorList<'s>; 2]) {
    let select = if cli.select.is_empty() {
        lint.select.as_deref().map(|s| (s, "lint.select"))
    } else {
        Some((cli.select.as_slice(), "--select"))
    };
    (
        select,
        [
            (&lint.extend_select, "lint.extend-select"),
            (&cli.extend_select, "--extend-select"),
        ],
    )
}

/// The enabled rules before `ignore`, with `heuristic` as the value of `lint.heuristic`.
fn enabled_before_ignore(
    lint: &LintConfig,
    cli: &Selectors,
    heuristic: bool,
) -> Result<BTreeSet<&'static str>, SelectionError> {
    let unnamed = |r: &RuleMeta| r.runs_unnamed(heuristic);
    let (select, extend) = selecting(lint, cli);
    let mut enabled: BTreeSet<&'static str> = match select {
        None => CORE_RULES
            .iter()
            .filter(|r| unnamed(r))
            .map(|r| r.code)
            .collect(),
        Some(select) => codes(select, &unnamed)?,
    };
    for list in extend {
        enabled.extend(codes(list, &unnamed)?);
    }
    enabled.extend(size_dependent_rules(cli));
    Ok(enabled)
}

/// The rules `select` / `extend-select` name by their code or slug, and those
/// `--include-size-dependent` names.
fn named_rules(lint: &LintConfig, cli: &Selectors) -> BTreeSet<&'static str> {
    let (select, extend) = selecting(lint, cli);
    select
        .into_iter()
        .chain(extend)
        .flat_map(|(list, _)| list)
        .filter_map(|s| named(s).map(|r| r.code))
        .chain(size_dependent_rules(cli))
        .collect()
}

/// The rules `--include-size-dependent` selects ([`Selectors::size_dependent_rewrites`]).
fn size_dependent_rules(cli: &Selectors) -> impl Iterator<Item = &'static str> {
    CORE_RULES
        .iter()
        .filter(move |r| cli.size_dependent_rewrites && r.size_dependent_fix)
        .map(|r| r.code)
}

impl RuleSelection {
    /// Resolve the effective rule set from `[tool.heki.lint]` and the command line. Every
    /// selector, and every rule `lint.severity` names, must name a rule.
    pub fn resolve(lint: &LintConfig, cli: &Selectors) -> Result<Self, SelectionError> {
        let heuristic = cli.heuristic.or(lint.heuristic).unwrap_or(false);
        let mut enabled = enabled_before_ignore(lint, cli, heuristic)?;
        let ignored = codes((&lint.ignore, "lint.ignore"), &any_rule)?
            .into_iter()
            .chain(codes((&cli.ignore, "--ignore"), &any_rule)?)
            .collect::<BTreeSet<_>>();
        enabled.retain(|code| !ignored.contains(code));
        let heuristic_off = if heuristic {
            BTreeSet::new()
        } else {
            enabled_before_ignore(lint, cli, true)?
                .into_iter()
                .filter(|code| !enabled.contains(code) && !ignored.contains(code))
                .collect()
        };
        let named = named_rules(lint, cli);
        let exact_reports_only = enabled
            .iter()
            .copied()
            .filter(|code| {
                find_rule(code)
                    .is_some_and(|r| !r.shows_heuristic_reports(named.contains(code), heuristic))
            })
            .collect();
        let mut per_file = Vec::new();
        for (pattern, rules) in &lint.per_file_ignores {
            let glob = AnyGlob::new(pattern).map_err(|cause| SelectionError::Glob {
                pattern: pattern.clone(),
                cause,
            })?;
            per_file.push((glob, codes((rules, "lint.per-file-ignores"), &any_rule)?));
        }
        if let Some(key) = lint.severity.keys().find(|k| find_rule(k).is_none()) {
            return Err(SelectionError::UnknownRule {
                selector: key.clone(),
                origin: "lint.severity",
            });
        }
        let fail = codes((&lint.fail, "lint.fail"), &any_rule)?;
        Ok(Self {
            enabled,
            per_file,
            heuristic_off,
            exact_reports_only,
            heuristic,
            ignored,
            fail,
        })
    }

    /// Whether the heuristic rules run as the exact ones do (`lint.heuristic`).
    pub fn heuristic(&self) -> bool {
        self.heuristic
    }

    /// Whether `lint.fail` names the rule `code`.
    pub fn fails_run(&self, code: &str) -> bool {
        self.fail.contains(code)
    }

    pub fn is_enabled(&self, code: &str) -> bool {
        self.enabled.contains(code)
    }

    /// `is_enabled` minus `per-file-ignores` matching the project-relative POSIX `rel_path`.
    pub fn is_enabled_for(&self, code: &str, rel_path: &str) -> bool {
        self.is_enabled(code)
            && !self
                .per_file
                .iter()
                .any(|(g, rules)| rules.contains(code) && g.is_match(rel_path))
    }

    /// The heuristic rules that are off only because `lint.heuristic` is: the selectors would
    /// enable them with it, and `ignore` does not name them.
    pub fn heuristic_off(&self) -> &BTreeSet<&'static str> {
        &self.heuristic_off
    }

    /// Whether `ignore` (of the configuration or the command line) names the rule `code`.
    pub fn is_ignored(&self, code: &str) -> bool {
        self.ignored.contains(code)
    }

    /// Whether the enabled rule `code` shows only its exact reports ([`Self::withhold_heuristic`]).
    pub fn withholds_heuristic(&self, code: &str) -> bool {
        self.exact_reports_only.contains(code)
    }

    /// Remove the heuristic reports of the rules that show only their exact ones, and return
    /// how many were removed. Callers apply it after inline suppressions and before baselines,
    /// fixes, and folding, so that those see what is shown and the count is what
    /// `lint.heuristic` would add.
    pub fn withhold_heuristic(&self, diags: &mut Vec<Diagnostic>) -> usize {
        self.take_heuristic(diags).len()
    }

    /// [`Self::withhold_heuristic`], returning the reports it removed.
    pub fn take_heuristic(&self, diags: &mut Vec<Diagnostic>) -> Vec<Diagnostic> {
        let (withheld, shown) = std::mem::take(diags)
            .into_iter()
            .partition(|d| self.withholds(d));
        *diags = shown;
        withheld
    }

    /// Whether the selection withholds the report `d`: a heuristic report of an exact rule.
    pub fn withholds(&self, d: &Diagnostic) -> bool {
        d.certainty != Certainty::Exact && self.exact_reports_only.contains(d.rule_code.as_str())
    }
}

/// The note a text output ends with when a run leaves out what `lint.heuristic` would add:
/// `rules_off` rules that did not run, and `reports_withheld` heuristic reports of exact rules
/// that did (as Ruff says how to apply the fixes it holds back).
pub fn heuristic_hint(rules_off: usize, reports_withheld: usize) -> Option<String> {
    let what = match (rules_off, reports_withheld) {
        (0, 0) => return None,
        (rules, 0) => format!("{rules} heuristic rule(s) are off"),
        (0, reports) => format!("{reports} heuristic report(s) are not shown"),
        (rules, reports) => {
            format!(
                "{rules} heuristic rule(s) are off and {reports} heuristic report(s) are not shown"
            )
        }
    };
    Some(format!(
        "note: {what}; enable them with `--heuristic` or `lint.heuristic = true`"
    ))
}

/// The note for the `count` reports a run shows (`which` names them) that `heki check` does not
/// fail on at `fail-on = "error"`: warnings, and heuristic reports, which count as warnings.
/// `heki check` ends a text run with it, and `heki recommend` lists it among its notes.
pub fn not_failing_note(count: usize, which: &str) -> String {
    format!(
        "heki check does not fail on {count} {which}: warnings, and heuristic reports, which can \
         be wrong about the code and count as warnings; `fail-on = \"warning\"` in the lint \
         settings makes them fail it."
    )
}

/// The line a text run ends with when it shows nothing on a whole project without contracts: what
/// to do next is `heki recommend`'s question (ADR-020).
pub const RECOMMEND_HINT: &str = "hint: `heki recommend` (experimental) lists the parameter \
    types and contracts the code already keeps, the rules that fit this project, and what heki \
    could not see";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Activation, Certainty};
    use std::collections::BTreeMap;

    fn sel(lint: &LintConfig, select: &[&str], extend: &[&str], ignore: &[&str]) -> RuleSelection {
        let v = |xs: &[&str]| xs.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let cli = Selectors {
            select: v(select),
            extend_select: v(extend),
            ignore: v(ignore),
            heuristic: None,
            size_dependent_rewrites: false,
        };
        RuleSelection::resolve(lint, &cli).unwrap()
    }

    fn with_heuristic() -> LintConfig {
        LintConfig {
            heuristic: Some(true),
            ..LintConfig::default()
        }
    }

    #[test]
    fn defaults_prefixes_opt_in_and_ignores() {
        let lint = with_heuristic();
        let d = sel(&lint, &[], &[], &[]);
        assert!(d.is_enabled("EFF101") && d.is_enabled("MUT201") && d.is_enabled("DATA101"));
        assert!(!d.is_enabled("EFF105") && !d.is_enabled("MUT103"));

        // A prefix never turns on opt-in rules; an exact code or slug does.
        let eff = sel(&lint, &["EFF"], &[], &[]);
        assert!(eff.is_enabled("EFF101") && !eff.is_enabled("EFF105") && !eff.is_enabled("MUT201"));
        assert!(sel(&lint, &[], &["EFF105"], &[]).is_enabled("EFF105"));
        assert!(sel(&lint, &["async-blocking-fs"], &[], &[]).is_enabled("EFF105"));

        let ign = sel(&lint, &[], &[], &["MUT", "loop-db-query"]);
        assert!(!ign.is_enabled("MUT101") && !ign.is_enabled("EFF103") && ign.is_enabled("EFF101"));
        assert!(ign.is_ignored("MUT101") && ign.is_ignored("EFF103") && !ign.is_ignored("EFF101"));
    }

    #[test]
    fn heuristic_rules_run_by_name_or_with_lint_heuristic() {
        let plain = LintConfig::default();
        // With `lint.heuristic`, the default set is every rule that is not opt-in.
        let all = sel(&with_heuristic(), &[], &[], &[]);
        for r in CORE_RULES {
            assert_eq!(
                all.is_enabled(r.code),
                r.activation != Activation::OptIn,
                "{}",
                r.code
            );
        }
        assert!(all.heuristic_off().is_empty());
        let heuristic = CORE_RULES
            .iter()
            .find(|r| r.activation == Activation::Default && r.certainty == Certainty::Heuristic)
            .expect("a heuristic rule that is on by default once exact");
        let h = heuristic.code;
        // Without it, a heuristic rule is off by default and for a prefix or `ALL`; a contract
        // rule still runs.
        let default = sel(&plain, &[], &[], &[]);
        assert!(!default.is_enabled(h) && default.is_enabled("MUT201"));
        assert!(default.heuristic_off().contains(h));
        for prefix in [&h[..3], "ALL"] {
            let s = sel(&plain, &[prefix], &[], &[]);
            assert!(
                !s.is_enabled(h) && s.heuristic_off().contains(h),
                "{prefix}"
            );
        }
        assert!(sel(&plain, &["MUT"], &[], &[]).is_enabled("MUT201"));
        // Its code or name selects it whatever `lint.heuristic` says.
        for named in [h, heuristic.slug] {
            let s = sel(&plain, &[], &[named], &[]);
            assert!(s.is_enabled(h) && !s.heuristic_off().contains(h), "{named}");
        }
        // An ignored rule is not counted as off for being heuristic.
        assert!(!sel(&plain, &[], &[], &[h]).heuristic_off().contains(h));
        // The command line wins over the configuration, both ways.
        let cli = |heuristic| Selectors {
            heuristic: Some(heuristic),
            ..Selectors::default()
        };
        assert!(
            RuleSelection::resolve(&plain, &cli(true))
                .unwrap()
                .is_enabled(h)
        );
        let off = RuleSelection::resolve(&with_heuristic(), &cli(false)).unwrap();
        assert!(!off.is_enabled(h) && off.heuristic_off().contains(h));
    }

    #[test]
    fn heuristic_reports_of_exact_rules_are_withheld_and_counted() {
        let report = |code: &str, certainty| Diagnostic {
            path: "a.py".into(),
            line: 1,
            column: 1,
            span: heki_ast::Span { start: 0, end: 1 },
            qualname: "f".into(),
            rule_code: code.into(),
            rule_slug: String::new(),
            target: String::new(),
            severity: heki_env::Severity::Warning,
            certainty,
            message: String::new(),
            help: None,
            evidence: None,
            blocker: None,
            related: Vec::new(),
            contract_locations: Vec::new(),
            fix: None,
            sites: 1,
            fold_key: None,
            open_records: Vec::new(),
            contracts: Vec::new(),
            contract_entries: Vec::new(),
            allowed_examples: Vec::new(),
            group: false,
            key_target: None,
            key_module: None,
            fails_run_by_config: false,
        };
        let selection = RuleSelection {
            enabled: BTreeSet::from(["OPT110", "OPT111"]),
            per_file: Vec::new(),
            heuristic_off: BTreeSet::new(),
            exact_reports_only: BTreeSet::from(["OPT110"]),
            heuristic: false,
            ignored: BTreeSet::new(),
            fail: BTreeSet::new(),
        };
        let mut diags = vec![
            report("OPT110", Certainty::Exact),
            report("OPT110", Certainty::Heuristic),
            report("OPT111", Certainty::Heuristic),
        ];
        assert!(
            selection.withholds_heuristic("OPT110") && !selection.withholds_heuristic("OPT111")
        );
        assert_eq!(selection.withhold_heuristic(&mut diags), 1);
        let left: Vec<_> = diags
            .iter()
            .map(|d| (d.rule_code.as_str(), d.certainty))
            .collect();
        assert_eq!(
            left,
            [
                ("OPT110", Certainty::Exact),
                ("OPT111", Certainty::Heuristic)
            ]
        );
    }

    #[test]
    fn the_hint_names_rules_off_and_reports_withheld() {
        let enable = "; enable them with `--heuristic` or `lint.heuristic = true`";
        assert_eq!(heuristic_hint(0, 0), None);
        for (rules, reports, what) in [
            (3, 0, "3 heuristic rule(s) are off"),
            (0, 2, "2 heuristic report(s) are not shown"),
            (
                3,
                2,
                "3 heuristic rule(s) are off and 2 heuristic report(s) are not shown",
            ),
        ] {
            assert_eq!(
                heuristic_hint(rules, reports).unwrap(),
                format!("note: {what}{enable}")
            );
        }
    }

    #[test]
    fn cli_select_overrides_config_select() {
        let lint = LintConfig {
            select: Some(vec!["MUT".into()]),
            extend_select: vec!["EFF105".into()],
            ..with_heuristic()
        };
        let cfg_only = sel(&lint, &[], &[], &[]);
        assert!(cfg_only.is_enabled("MUT201") && !cfg_only.is_enabled("EFF101"));
        assert!(cfg_only.is_enabled("EFF105"));
        let cli = sel(&lint, &["DATA"], &[], &[]);
        assert!(cli.is_enabled("DATA101") && !cli.is_enabled("MUT201"));
    }

    #[test]
    fn per_file_ignores_and_unknown_rules() {
        let lint = LintConfig {
            per_file_ignores: BTreeMap::from([("tests/**".into(), vec!["EFF".into()])]),
            ..with_heuristic()
        };
        let s = sel(&lint, &[], &[], &[]);
        assert!(!s.is_enabled_for("EFF101", "tests/test_api.py"));
        assert!(s.is_enabled_for("EFF101", "src/pkg/api.py"));
        assert!(s.is_enabled_for("MUT201", "tests/test_api.py"));

        let unknown = Selectors {
            select: vec!["NOPE1".into()],
            ..Selectors::default()
        };
        let err = RuleSelection::resolve(&LintConfig::default(), &unknown);
        assert!(
            matches!(err, Err(SelectionError::UnknownRule { selector, origin: "--select" }) if selector == "NOPE1")
        );
        let empty = Selectors {
            ignore: vec![String::new()],
            ..Selectors::default()
        };
        assert!(RuleSelection::resolve(&LintConfig::default(), &empty).is_err());
    }
}
