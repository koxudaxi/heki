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
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum ExperimentalKind {
    /// A rule code (`MUT101`).
    Rule,
    /// A command (`heki inspect`), its options and output included.
    Command,
    /// A command and one of its options (`heki check --generate-prompt`).
    CliOption,
    /// A key below `[tool.heki]`, with its tables (`lint.relax-async-in-tests`).
    ConfigKey,
    /// An output format and one of its fields (`json fix`).
    OutputField,
    /// A behavior no single name selects, described in words.
    Behavior,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ExperimentalFeature {
    /// `<kind>.<name>`, stable while the feature is experimental (`rule.MUT101`).
    pub id: &'static str,
    pub kind: ExperimentalKind,
    /// What the feature is: a rule code, a command, an option, a key, or a behavior.
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
        id: "rule.MUT112",
        kind: ExperimentalKind::Rule,
        target: "MUT112",
        since: "0.2.0",
        reason: "Its precision on real code bases is still low: much of what it reports is code \
            that changes a borrowed container on purpose.",
        tracking: None,
    },
    ExperimentalFeature {
        id: "command.inspect",
        kind: ExperimentalKind::Command,
        target: "heki inspect",
        since: "0.2.1",
        reason: "The facts it shows, its purity verdict, and its JSON object can still change as \
            heki's summaries close gaps; the object's `version` changes when a field is removed \
            or changes meaning.",
        tracking: None,
    },
    ExperimentalFeature {
        id: "command.skill",
        kind: ExperimentalKind::Command,
        target: "heki skill",
        since: "0.2.5",
        reason: "Where coding agents read skills, and so where `heki skill install` writes, can \
            change as the agents evolve; the skill's wording changes with heki's commands.",
        tracking: None,
    },
    ExperimentalFeature {
        id: "command.recommend",
        kind: ExperimentalKind::Command,
        target: "heki recommend",
        since: "0.2.2",
        reason: "The sections, their policies, the candidates it infers, and its JSON object can \
            still change as the policies are calibrated; the object's `version` changes when a \
            field is removed or changes meaning.",
        tracking: None,
    },
    ExperimentalFeature {
        id: "rule.MUT113",
        kind: ExperimentalKind::Rule,
        target: "MUT113",
        since: "0.2.0",
        reason: "It has few reports, and most of them are still false.",
        tracking: None,
    },
    ExperimentalFeature {
        id: "rule.EFF109",
        kind: ExperimentalKind::Rule,
        target: "EFF109",
        since: "0.2.1",
        reason: "Its precision is measured on a small sample of reports only.",
        tracking: None,
    },
    ExperimentalFeature {
        id: "rule.EFF110",
        kind: ExperimentalKind::Rule,
        target: "EFF110",
        since: "0.2.1",
        reason: "Its precision is measured on a small sample of reports only.",
        tracking: None,
    },
    ExperimentalFeature {
        id: "rule.EFF112",
        kind: ExperimentalKind::Rule,
        target: "EFF112",
        since: "0.2.4",
        reason: "Its precision is measured on a sample of reports of one corpus only, and its \
            premises are not yet certified.",
        tracking: None,
    },
    ExperimentalFeature {
        id: "rule.EFF114",
        kind: ExperimentalKind::Rule,
        target: "EFF114",
        since: "0.2.4",
        reason: "Its precision is measured on a sample of reports of one corpus only, and its \
            premises are not yet certified.",
        tracking: None,
    },
    ExperimentalFeature {
        id: "rule.EFF115",
        kind: ExperimentalKind::Rule,
        target: "EFF115",
        since: "0.2.5",
        reason: "The development corpus has no report of it, so its precision is not measured, \
            and its premises are not yet certified.",
        tracking: None,
    },
    ExperimentalFeature {
        id: "rule.EFF116",
        kind: ExperimentalKind::Rule,
        target: "EFF116",
        since: "0.2.6",
        reason: "The development corpus has few reports of it, and its premises are not yet \
            certified.",
        tracking: None,
    },
    ExperimentalFeature {
        id: "rule.IMP101",
        kind: ExperimentalKind::Rule,
        target: "IMP101",
        since: "0.2.7",
        reason: "It is new: its reports are heuristic until a review of its reports on real code \
            bases certifies its claim, and which imports count can still change.",
        tracking: None,
    },
    ExperimentalFeature {
        id: "rule.IMP102",
        kind: ExperimentalKind::Rule,
        target: "IMP102",
        since: "0.3.0",
        reason: "It is new: every report rests on established premises only, but its reports are \
            heuristic until a review of its reports on real code bases certifies its claim, and \
            which imports and code stop a run can still change.",
        tracking: None,
    },
    ExperimentalFeature {
        id: "rule.IMP104",
        kind: ExperimentalKind::Rule,
        target: "IMP104",
        since: "0.3.0",
        reason: "It is new: every report rests on established premises only, but its reports are \
            heuristic until a review of its reports on real code bases certifies its claim, and \
            which modules count as package and development code can still change.",
        tracking: None,
    },
    ExperimentalFeature {
        id: "rule.IMP201",
        kind: ExperimentalKind::Rule,
        target: "IMP201",
        since: "0.2.7",
        reason: "It is new: its reports are heuristic until a review of its reports on real code \
            bases certifies its claim.",
        tracking: None,
    },
    ExperimentalFeature {
        id: "rule.IMP202",
        kind: ExperimentalKind::Rule,
        target: "IMP202",
        since: "0.2.7",
        reason: "It is the strict mode of `IMP201`, which is experimental.",
        tracking: None,
    },
    ExperimentalFeature {
        id: "rule.IMP301",
        kind: ExperimentalKind::Rule,
        target: "IMP301",
        since: "0.3.0",
        reason: "It is new: every report rests on established premises only, but its reports are \
            heuristic until a review of its reports on real code bases certifies its claim, and \
            which distributions provide a module can still change.",
        tracking: None,
    },
    ExperimentalFeature {
        id: "rule.IMP302",
        kind: ExperimentalKind::Rule,
        target: "IMP302",
        since: "0.3.0",
        reason: "It is new: every report rests on established premises only, but its reports are \
            heuristic until a review of its reports on real code bases certifies its claim, and \
            which distributions provide a module and which modules are base modules can still \
            change.",
        tracking: None,
    },
    ExperimentalFeature {
        id: "rule.MUT120",
        kind: ExperimentalKind::Rule,
        target: "MUT120",
        since: "0.2.7",
        reason: "It is new: its reports are heuristic until a review of its reports on real code \
            bases certifies its claim.",
        tracking: None,
    },
    ExperimentalFeature {
        id: "cli-option.generate-prompt",
        kind: ExperimentalKind::CliOption,
        target: "heki check --generate-prompt",
        since: "0.2.2",
        reason: "What the prompt holds and how it says it can still change as it is tried with \
            models, and so can its JSON object, whose schema `heki check \
            --output-format-json-schema generate-prompt` prints; the object's `version` changes \
            when a field is removed or changes meaning.",
        tracking: None,
    },
];

/// The entry of an experimental rule, by its code.
pub fn experimental_rule(code: &str) -> Option<&'static ExperimentalFeature> {
    EXPERIMENTAL_FEATURES
        .iter()
        .find(|f| f.kind == ExperimentalKind::Rule && f.target == code)
}
