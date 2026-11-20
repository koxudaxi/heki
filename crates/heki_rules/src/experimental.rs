//! The experimental features: what can still change or go away in any release.
//!
//! `heki rule <CODE>`, the rule pages, and `docs/experimental.md` (written by
//! `scripts/build_docs.py`) read this registry; the generator also fails when a hand-written page
//! disagrees with it.

use serde::Serialize;

/// What being experimental means, as `heki rule` and the documentation say it.
pub const EXPERIMENTAL_POLICY: &str = "Experimental features can still change in behavior or \
    precision, or be removed, in any release, and each change is listed in the release notes. An \
    opt-in rule that is not experimental is stable and only off by default.";

/// How an entry names its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExperimentalKind {
    /// A rule code (`MUT101`).
    Rule,
    /// A command and one of its options (`heki check --profile`).
    CliOption,
    /// A key below `[tool.heki]`, with its tables (`lint.relax-async-in-tests`).
    ConfigKey,
    /// An output format and one of its fields (`json fix`).
    OutputField,
    /// A behavior no single name selects, described in words.
    Behavior,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ExperimentalFeature {
    /// `<kind>.<name>`, stable while the feature is experimental (`rule.MUT101`).
    pub id: &'static str,
    pub kind: ExperimentalKind,
    pub target: &'static str,
    /// The release the feature is experimental in first.
    pub since: &'static str,
    /// Why the feature is experimental, as whole sentences.
    pub reason: &'static str,
    /// An issue or discussion that tracks what keeps the feature experimental.
    pub tracking: Option<&'static str>,
}

/// An entry leaves the registry once its feature is stable.
pub const EXPERIMENTAL_FEATURES: &[ExperimentalFeature] = &[
    ExperimentalFeature {
        id: "rule.MUT101",
        kind: ExperimentalKind::Rule,
        target: "MUT101",
        since: "0.1.0",
        reason: "Its precision outside a few code bases is still low.",
        tracking: None,
    },
    ExperimentalFeature {
        id: "rule.MUT111",
        kind: ExperimentalKind::Rule,
        target: "MUT111",
        since: "0.1.1",
        reason: "It reports the readers of the stale writes `MUT101` finds, so it shares \
            `MUT101`'s precision.",
        tracking: None,
    },
    ExperimentalFeature {
        id: "cli-option.profile",
        kind: ExperimentalKind::CliOption,
        target: "heki check --profile",
        since: "0.1.1",
        reason: "It reads the output of CPython 3.15's sampling profiler, and that format can \
            still change.",
        tracking: None,
    },
];

/// The entry of an experimental rule, by its code.
pub fn experimental_rule(code: &str) -> Option<&'static ExperimentalFeature> {
    EXPERIMENTAL_FEATURES
        .iter()
        .find(|f| f.kind == ExperimentalKind::Rule && f.target == code)
}
