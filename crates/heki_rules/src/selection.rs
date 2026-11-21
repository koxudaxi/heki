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

use crate::{CORE_RULES, RuleMeta};
use globset::{Glob, GlobMatcher};
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
}

/// Rules a selector names: a code or slug names its rule, and a prefix or `ALL` the matching
/// rules `admits`.
fn resolve(
    selector: &str,
    admits: &dyn Fn(&RuleMeta) -> bool,
) -> Result<Vec<&'static RuleMeta>, SelectionError> {
    let s = selector.trim();
    if let Some(r) = CORE_RULES
        .iter()
        .find(|r| r.code.eq_ignore_ascii_case(s) || r.slug.eq_ignore_ascii_case(s))
    {
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

/// The enabled rules before `ignore`, with `heuristic` as the value of `lint.heuristic`.
fn enabled_before_ignore(
    lint: &LintConfig,
    cli: &Selectors,
    heuristic: bool,
) -> Result<BTreeSet<&'static str>, SelectionError> {
    let unnamed = |r: &RuleMeta| r.runs_unnamed(heuristic);
    let select = if cli.select.is_empty() {
        lint.select.as_deref()
    } else {
        Some(cli.select.as_slice())
    };
    let mut enabled: BTreeSet<&'static str> = match select {
        None => CORE_RULES
            .iter()
            .filter(|r| unnamed(r))
            .map(|r| r.code)
            .collect(),
        Some(select) => codes(select, &unnamed)?,
    };
    enabled.extend(codes(&lint.extend_select, &unnamed)?);
    enabled.extend(codes(&cli.extend_select, &unnamed)?);
    Ok(enabled)
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
        let heuristic_off = if heuristic {
            BTreeSet::new()
        } else {
            enabled_before_ignore(lint, cli, true)?
                .into_iter()
                .filter(|code| !enabled.contains(code) && !ignored.contains(code))
                .collect()
        };
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
