//! Rule selection (docs/configuration.md, Ruff precedence).
//!
//! - Default: every `CORE_RULES` entry in the default selection
//!   ([`RuleMeta::runs_by_default`]): the exact `Default` rules and the `Contract` rules, and with
//!   `lint.heuristic` the heuristic `Default` rules and the `Selectable` rules too (every rule
//!   that is not opt-in, Ruff's `preview` and the earlier default set).
//! - `select` (CLI over config) replaces the default set (an empty `select` in the
//!   configuration selects no rule); `extend-select` adds; `ignore` and
//!   `per-file-ignores` remove.
//! - A selector is a code (`EFF105`), a slug (`async-blocking-fs`), or a code prefix
//!   (`EFF`, `MUT2`, `ALL`). A prefix enables the rules [`RuleMeta::runs_for_prefix`] admits:
//!   those of the default selection and the `Selectable` ones; opt-in rules such as `EFF105`, and
//!   heuristic rules without `lint.heuristic`, need their exact code or slug. `ignore` prefixes
//!   cover every matching rule.
//! - An exact rule whose single reports can be heuristic (an `OPT` report whose rewrite is not
//!   proven) shows those reports only where a heuristic rule would run: with `lint.heuristic`,
//!   when its code or slug is selected, or for a contract rule.

use crate::{CORE_RULES, Certainty, RuleMeta, find_rule};
use globset::{Glob, GlobMatcher};
use heki_diagnostics::Diagnostic;
use heki_env::LintConfig;
use std::collections::BTreeSet;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SelectionError {
    #[error("unknown rule selector `{0}`")]
    UnknownRule(String),
    #[error("invalid per-file-ignores glob `{pattern}`: {cause}")]
    Glob {
        pattern: String,
        cause: globset::Error,
    },
}

/// The selection options of the command line, over the configuration (`--select`,
/// `--extend-select`, `--ignore`, `--heuristic` / `--no-heuristic`).
#[derive(Debug, Clone, Default)]
pub struct Selectors {
    pub select: Vec<String>,
    pub extend_select: Vec<String>,
    pub ignore: Vec<String>,
    /// `None`: `lint.heuristic` decides.
    pub heuristic: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct RuleSelection {
    enabled: BTreeSet<&'static str>,
    per_file: Vec<(GlobMatcher, BTreeSet<&'static str>)>,
    /// The rules `lint.heuristic` would add to `enabled`.
    heuristic_off: BTreeSet<&'static str>,
    /// The enabled rules whose heuristic reports are withheld: exact rules that run without
    /// being named while `lint.heuristic` is off.
    exact_reports_only: BTreeSet<&'static str>,
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
        return Err(SelectionError::UnknownRule(selector.to_owned()));
    }
    Ok(matching.into_iter().filter(|r| admits(r)).collect())
}

fn codes(
    selectors: &[String],
    admits: &dyn Fn(&RuleMeta) -> bool,
) -> Result<BTreeSet<&'static str>, SelectionError> {
    let mut out = BTreeSet::new();
    for s in selectors {
        out.extend(resolve(s, admits)?.into_iter().map(|r| r.code));
    }
    Ok(out)
}

/// Every rule a prefix of `ignore` or `per-file-ignores` matches.
fn any_rule(_: &RuleMeta) -> bool {
    true
}

/// The effective `select` (the command line's over the configuration's; `None`: the default
/// set) and the `extend-select` lists.
fn selecting<'s>(
    lint: &'s LintConfig,
    cli: &'s Selectors,
) -> (Option<&'s [String]>, [&'s [String]; 2]) {
    let select = if cli.select.is_empty() {
        lint.select.as_deref()
    } else {
        Some(cli.select.as_slice())
    };
    (select, [&lint.extend_select, &cli.extend_select])
}

/// The enabled rules before `ignore`, with `heuristic` as the value of `lint.heuristic`.
fn enabled_before_ignore(
    lint: &LintConfig,
    cli: &Selectors,
    heuristic: bool,
) -> Result<BTreeSet<&'static str>, SelectionError> {
    let for_prefix = |r: &RuleMeta| r.runs_for_prefix(heuristic);
    let (select, extend) = selecting(lint, cli);
    let mut enabled: BTreeSet<&'static str> = match select {
        None => CORE_RULES
            .iter()
            .filter(|r| r.runs_by_default(heuristic))
            .map(|r| r.code)
            .collect(),
        Some(select) => codes(select, &for_prefix)?,
    };
    for list in extend {
        enabled.extend(codes(list, &for_prefix)?);
    }
    Ok(enabled)
}

/// The rules `select` / `extend-select` name by their code or slug.
fn named_rules(lint: &LintConfig, cli: &Selectors) -> BTreeSet<&'static str> {
    let (select, extend) = selecting(lint, cli);
    select
        .into_iter()
        .chain(extend)
        .flatten()
        .filter_map(|s| named(s).map(|r| r.code))
        .collect()
}

impl RuleSelection {
    /// Resolve the effective rule set from `[tool.heki.lint]` and the command line.
    pub fn resolve(lint: &LintConfig, cli: &Selectors) -> Result<Self, SelectionError> {
        let heuristic = cli.heuristic.or(lint.heuristic).unwrap_or(false);
        let mut enabled = enabled_before_ignore(lint, cli, heuristic)?;
        let ignored = codes(&lint.ignore, &any_rule)?
            .into_iter()
            .chain(codes(&cli.ignore, &any_rule)?)
            .collect::<BTreeSet<_>>();
        enabled.retain(|code| !ignored.contains(code));
        // The rules `lint.heuristic` would add that are heuristic: it also adds the exact
        // `Selectable` rules to the default selection, which the note does not count.
        let heuristic_off = if heuristic {
            BTreeSet::new()
        } else {
            enabled_before_ignore(lint, cli, true)?
                .into_iter()
                .filter(|code| !enabled.contains(code) && !ignored.contains(code))
                .filter(|code| find_rule(code).is_some_and(|r| r.certainty == Certainty::Heuristic))
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
            let glob = Glob::new(pattern)
                .map_err(|cause| SelectionError::Glob {
                    pattern: pattern.clone(),
                    cause,
                })?
                .compile_matcher();
            per_file.push((glob, codes(rules, &any_rule)?));
        }
        Ok(Self {
            enabled,
            per_file,
            heuristic_off,
            exact_reports_only,
        })
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

    /// Remove the heuristic reports of the rules that show only their exact ones, and return
    /// how many were removed. Callers apply it after inline suppressions and before baselines,
    /// fixes, and folding, so that those see what is shown and the count is what
    /// `lint.heuristic` would add.
    pub fn withhold_heuristic(&self, diags: &mut Vec<Diagnostic>) -> usize {
        let before = diags.len();
        diags.retain(|d| {
            d.certainty == Certainty::Exact
                || !self.exact_reports_only.contains(d.rule_code.as_str())
        });
        before - diags.len()
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
    fn a_selectable_rule_runs_for_its_prefix_or_name_but_never_by_default() {
        let selectable = |certainty| {
            let rule = RuleMeta {
                certainty,
                activation: Activation::Selectable,
                ..*find_rule("OPT110").unwrap()
            };
            let by_prefix = [rule.runs_for_prefix(false), rule.runs_for_prefix(true)];
            let by_default = [rule.runs_by_default(false), rule.runs_by_default(true)];
            (by_prefix, by_default)
        };
        // Exact, a prefix or `ALL` selects it; heuristic, it waits for `lint.heuristic` as a
        // `Default` rule does. Only `lint.heuristic`, which runs every rule that is not opt-in,
        // puts it in the default selection.
        assert_eq!(selectable(Certainty::Exact), ([true, true], [false, true]));
        assert_eq!(
            selectable(Certainty::Heuristic),
            ([false, true], [false, true])
        );
        // The catalog's selectable rules: on by name, off by default, and for `ALL` and their
        // prefixes with `lint.heuristic`.
        let s = CORE_RULES
            .iter()
            .find(|r| r.activation == Activation::Selectable)
            .expect("a selectable rule");
        let plain = LintConfig::default();
        let default = sel(&plain, &[], &[], &[]);
        assert!(!default.is_enabled(s.code));
        // Off by default, it counts as a heuristic rule left off only while it is heuristic.
        assert_eq!(
            default.heuristic_off().contains(s.code),
            s.certainty == Certainty::Heuristic
        );
        assert!(sel(&plain, &[], &[s.code], &[]).is_enabled(s.code));
        for prefix in [&s.code[..3], "ALL"] {
            assert!(
                sel(&with_heuristic(), &[], &[prefix], &[]).is_enabled(s.code),
                "{prefix}"
            );
            assert_eq!(
                sel(&plain, &[], &[prefix], &[]).is_enabled(s.code),
                s.certainty == Certainty::Exact,
                "{prefix}"
            );
        }
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
            fix: None,
            sites: 1,
            fold_key: None,
        };
        let selection = RuleSelection {
            enabled: BTreeSet::from(["OPT110", "OPT111"]),
            per_file: Vec::new(),
            heuristic_off: BTreeSet::new(),
            exact_reports_only: BTreeSet::from(["OPT110"]),
        };
        let mut diags = vec![
            report("OPT110", Certainty::Exact),
            report("OPT110", Certainty::Heuristic),
            report("OPT111", Certainty::Heuristic),
        ];
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
        assert!(matches!(err, Err(SelectionError::UnknownRule(r)) if r == "NOPE1"));
        let empty = Selectors {
            ignore: vec![String::new()],
            ..Selectors::default()
        };
        assert!(RuleSelection::resolve(&LintConfig::default(), &empty).is_err());
    }
}
