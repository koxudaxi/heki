//! Rule selection (docs/configuration.md, Ruff precedence).
//!
//! - Default: every `CORE_RULES` entry with `default_enabled`.
//! - `select` (CLI over config) replaces the default set; `extend-select` adds; `ignore` and
//!   `per-file-ignores` remove.
//! - A selector is a code (`EFF105`), a slug (`async-blocking-fs`), or a code prefix
//!   (`EFF`, `MUT2`, `ALL`). A prefix only enables default-on rules: opt-in rules such as
//!   `EFF105` need their exact code or slug. `ignore` prefixes cover every matching rule.

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

#[derive(Debug, Clone)]
pub struct RuleSelection {
    enabled: BTreeSet<&'static str>,
    per_file: Vec<(GlobMatcher, BTreeSet<&'static str>)>,
}

/// Rules a selector names. `for_enable` restricts prefixes to default-on rules.
fn resolve(selector: &str, for_enable: bool) -> Result<Vec<&'static RuleMeta>, SelectionError> {
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
    Ok(matching
        .into_iter()
        .filter(|r| !for_enable || r.default_enabled)
        .collect())
}

fn codes(selectors: &[String], for_enable: bool) -> Result<BTreeSet<&'static str>, SelectionError> {
    let mut out = BTreeSet::new();
    for s in selectors {
        out.extend(resolve(s, for_enable)?.into_iter().map(|r| r.code));
    }
    Ok(out)
}

impl RuleSelection {
    /// Resolve the effective rule set from `[tool.heki.lint]` and CLI flags.
    pub fn resolve(
        lint: &LintConfig,
        cli_select: &[String],
        cli_extend_select: &[String],
        cli_ignore: &[String],
    ) -> Result<Self, SelectionError> {
        let select = if !cli_select.is_empty() {
            cli_select
        } else {
            &lint.select
        };
        let mut enabled: BTreeSet<&'static str> = if select.is_empty() {
            CORE_RULES
                .iter()
                .filter(|r| r.default_enabled)
                .map(|r| r.code)
                .collect()
        } else {
            codes(select, true)?
        };
        enabled.extend(codes(&lint.extend_select, true)?);
        enabled.extend(codes(cli_extend_select, true)?);
        for code in codes(&lint.ignore, false)?
            .into_iter()
            .chain(codes(cli_ignore, false)?)
        {
            enabled.remove(code);
        }
        let mut per_file = Vec::new();
        for (pattern, rules) in &lint.per_file_ignores {
            let glob = Glob::new(pattern)
                .map_err(|cause| SelectionError::Glob {
                    pattern: pattern.clone(),
                    cause,
                })?
                .compile_matcher();
            per_file.push((glob, codes(rules, false)?));
        }
        Ok(Self { enabled, per_file })
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn sel(lint: &LintConfig, select: &[&str], extend: &[&str], ignore: &[&str]) -> RuleSelection {
        let v = |xs: &[&str]| xs.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        RuleSelection::resolve(lint, &v(select), &v(extend), &v(ignore)).unwrap()
    }

    #[test]
    fn defaults_prefixes_opt_in_and_ignores() {
        let lint = LintConfig::default();
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
    fn cli_select_overrides_config_select() {
        let lint = LintConfig {
            select: vec!["MUT".into()],
            extend_select: vec!["EFF105".into()],
            ..LintConfig::default()
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
            ..LintConfig::default()
        };
        let s = sel(&lint, &[], &[], &[]);
        assert!(!s.is_enabled_for("EFF101", "tests/test_api.py"));
        assert!(s.is_enabled_for("EFF101", "src/pkg/api.py"));
        assert!(s.is_enabled_for("MUT201", "tests/test_api.py"));

        let err = RuleSelection::resolve(&LintConfig::default(), &["NOPE1".into()], &[], &[]);
        assert!(matches!(err, Err(SelectionError::UnknownRule(r)) if r == "NOPE1"));
        assert!(
            RuleSelection::resolve(&LintConfig::default(), &[], &[], &[String::new()]).is_err()
        );
    }
}
