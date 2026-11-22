//! `heki recommend` (experimental): advice on the configuration of one project, computed from
//! the analysis `heki check` runs on it (docs/recommend.md, ADR-020).
//!
//! The discovery run analyzes the project once with every rule selected, as `heki check` would
//! with that selection: inline suppressions and `per-file-ignores` apply. What the current
//! configuration shows is the part of that run's reports whose rules it selects, which is what
//! `heki check` prints, since a rule's reports do not depend on which other rules are selected.
//! The report is the same for the same input, configuration, and heki version: every list is
//! sorted, and every cap is a count.

pub mod block;

use crate::project::{
    Facts, Indexed, Project, ProjectError, canonical, exclude_set, files_left_out, regressed,
    rel_posix,
};
use crate::selection::{RuleSelection, SelectionError, Selectors};
use crate::{CORE_RULES, Certainty, RuleConfigError, SummarySnapshot, fold_root_causes};
use heki_diagnostics::{Diagnostic, RatchetBaseline};
use heki_env::{ConfigLayout, LintConfig};
use rustc_hash::FxHashSet;
use schemars::JsonSchema;
use serde::Serialize;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// The `version` of the JSON object: it changes when a field is removed or changes meaning;
/// fields are only added within a version.
pub const RECOMMEND_VERSION: u32 = 1;

/// Where the JSON Schema of the object is published (`docs/schemas/recommend-1.json`).
pub const SCHEMA_URL: &str = "https://heki.koxudaxi.dev/schemas/recommend-1.json";

/// The most entries a list of the JSON object holds; the list says when it holds fewer than
/// there are.
pub const MAX_LISTED: usize = 50;

/// The most examples a line of the text names.
pub const MAX_EXAMPLES: usize = 3;

/// The most entries below the project root the count of the Python files the paths leave out
/// looks at: past them, it says how many it found so far.
pub const MAX_CENSUS_ENTRIES: usize = 10_000;

/// The file the configuration goes into when the project has none.
const NEW_CONFIG_FILE: &str = "pyproject.toml";

#[derive(Debug, Error)]
pub enum RecommendError {
    #[error(transparent)]
    Selection(#[from] SelectionError),
    #[error(transparent)]
    Rules(#[from] RuleConfigError),
    #[error(transparent)]
    Project(#[from] ProjectError),
}

/// What `heki recommend` analyzes, and how it writes paths.
pub struct Request<'a> {
    pub project: Project,
    /// The project root as the output writes it (`.` for the working directory).
    pub root: String,
    /// The rule selection the advice is relative to: the command line over the configuration.
    pub current: RuleSelection,
    /// The summary snapshot `MUT103` compares with, when the current selection runs it.
    pub snapshot: Option<SummarySnapshot>,
    /// The Ratchet baseline the current configuration compares with.
    pub baseline: Option<RatchetBaseline>,
    /// The paths the project was loaded from.
    pub paths: &'a [PathBuf],
    /// A path as the output writes it: relative to the working directory when inside it, as
    /// `heki check` writes paths.
    pub shown: &'a dyn Fn(&Path) -> String,
    /// The files that could not be read or parsed, as `heki check` reports them.
    pub errors: Vec<String>,
}

/// The JSON object `heki recommend --output-format json` prints.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(title = "heki recommend", deny_unknown_fields)]
pub struct RecommendReport {
    /// The URL of this schema.
    #[serde(rename = "$schema")]
    pub schema: String,
    pub kind: ReportKind,
    /// The version of the object's fields: it changes when a field is removed or changes
    /// meaning; fields are only added within a version.
    #[schemars(range(min = 1, max = 1))]
    pub version: u32,
    /// The version of heki that wrote the report.
    pub heki: String,
    /// The project root, as the output writes paths (`.` for the working directory).
    pub root: String,
    /// The configuration file the run read, or `null` when the project has none.
    pub config_file: Option<String>,
    /// The table the configuration's keys are under in that file: `tool.heki` in a
    /// `pyproject.toml`, empty for a file that holds them at its top level (`heki.toml`).
    pub table_prefix: String,
    pub analyzed: Analyzed,
    /// The analyses the report was computed from, in the order they ran.
    pub runs: Vec<Run>,
    pub reports_now: ReportsNow,
    /// What the reader should know to weigh the advice.
    pub notes: Vec<String>,
    /// The keys `config_toml` adds or changes, as the configuration's keys and values.
    pub config: serde_json::Map<String, serde_json::Value>,
    /// The keys to add or change, as TOML for `config_file` (empty when nothing is added).
    pub config_toml: String,
    /// The report as the text output prints it.
    pub content: String,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
pub enum ReportKind {
    #[serde(rename = "heki-recommend")]
    HekiRecommend,
}

/// The files the report was computed from.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Analyzed {
    /// The files analyzed.
    pub files: usize,
    /// The functions in them, module bodies included.
    pub functions: usize,
    /// The files that could not be read or parsed, as `heki check` reports them.
    pub errors: Vec<String>,
    /// The paths given on the command line.
    pub paths: Vec<String>,
    pub left_out: LeftOut,
}

/// The Python files below the project root that the paths given leave out: heki takes the
/// files it analyzed as the whole project.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct LeftOut {
    /// How many were found (when `truncated`, a lower bound).
    pub python_files: usize,
    /// The files found, in the order of a walk of the root sorted by name.
    pub files: Vec<String>,
    /// Whether the count stopped before the end: after 50 files, or after the first 10,000
    /// entries below the root.
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Run {
    pub label: RunLabel,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RunLabel {
    /// Every rule selected, on the configuration as it is.
    Discovery,
}

/// What the current configuration shows, as `heki check` prints it: reports that share a root
/// cause are folded into one.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ReportsNow {
    /// The reports `heki check` prints without a baseline.
    pub shown: usize,
    pub exact: usize,
    pub heuristic: usize,
    /// The heuristic reports of exact rules that `lint.heuristic = false` leaves out.
    pub withheld_heuristic: usize,
    /// The heuristic rules that are off only because `lint.heuristic` is false.
    pub heuristic_rules_off: usize,
    /// With a baseline: the reports it allows, and the regressions `heki check` prints.
    pub baseline: Option<BaselineSplit>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct BaselineSplit {
    pub allowed: usize,
    pub regressions: usize,
}

/// The JSON Schema of [`RecommendReport`], as `docs/schemas/recommend-1.json` publishes it.
pub fn json_schema() -> serde_json::Value {
    let mut schema = schemars::schema_for!(RecommendReport);
    schema.insert("$id".to_owned(), SCHEMA_URL.into());
    schema.to_value()
}

/// The report on `request`'s project.
pub fn recommend(request: Request<'_>) -> Result<RecommendReport, RecommendError> {
    let Request {
        project,
        root,
        current,
        snapshot,
        baseline,
        paths,
        shown,
        errors,
    } = request;
    let env = &project.env;
    // Without a configuration file, the keys go into a new `pyproject.toml`.
    let layout = env
        .config_file
        .as_deref()
        .map_or(ConfigLayout::PyProject, ConfigLayout::of);
    let config_file = env.config_file.as_deref().map(shown);
    let left_out = left_out(&project, paths, shown)?;
    let functions = project.files.iter().map(|f| f.ir.functions.len()).sum();
    let files = project.files.len();
    let block = block::render(&block::Additions::default(), &env.cfg, layout);
    let selection = discovery_selection(&env.cfg.lint, &current)?;
    let discovery = Indexed::build(project, selection, snapshot, Facts::Check)?;
    let (diags, _) = discovery.diagnose()?;
    let reports_now = reports_now(&current, baseline.as_ref(), &discovery, diags);
    let mut notes = Vec::new();
    if left_out.python_files > 0 {
        notes.push(left_out_note(&left_out));
    }
    let mut report = RecommendReport {
        schema: SCHEMA_URL.to_owned(),
        kind: ReportKind::HekiRecommend,
        version: RECOMMEND_VERSION,
        heki: env!("CARGO_PKG_VERSION").to_owned(),
        root,
        table_prefix: layout.table_prefix().unwrap_or_default().to_owned(),
        config_file,
        analyzed: Analyzed {
            files,
            functions,
            errors,
            paths: paths.iter().map(|p| shown(p)).collect(),
            left_out,
        },
        runs: vec![Run {
            label: RunLabel::Discovery,
        }],
        reports_now,
        notes,
        config: block.config,
        config_toml: block.toml,
        content: String::new(),
    };
    report.content = render_text(&report);
    Ok(report)
}

/// The selection of the discovery run: every rule by its code, so that no heuristic report is
/// withheld, with the configuration's `per-file-ignores`. `MUT103` runs only when the current
/// selection runs it: it compares with a snapshot the user saves.
fn discovery_selection(
    lint: &LintConfig,
    current: &RuleSelection,
) -> Result<RuleSelection, SelectionError> {
    let lint = LintConfig {
        select: Some(Vec::new()),
        extend_select: Vec::new(),
        ignore: Vec::new(),
        ..lint.clone()
    };
    let every_rule = CORE_RULES
        .iter()
        .filter(|r| r.code != "MUT103" || current.is_enabled(r.code))
        .map(|r| r.code.to_owned())
        .collect();
    let selectors = Selectors {
        select: every_rule,
        extend_select: Vec::new(),
        ignore: Vec::new(),
        heuristic: Some(true),
    };
    RuleSelection::resolve(&lint, &selectors)
}

/// The discovery run's reports that `current` shows, counted as `heki check` prints them.
fn reports_now(
    current: &RuleSelection,
    baseline: Option<&RatchetBaseline>,
    discovery: &Indexed,
    diags: Vec<Diagnostic>,
) -> ReportsNow {
    let mut now: Vec<Diagnostic> = diags
        .into_iter()
        .filter(|d| current.is_enabled(&d.rule_code))
        .collect();
    let withheld_heuristic = current.withhold_heuristic(&mut now);
    let baseline = baseline.map(|baseline| {
        let regressed = regressed(baseline, &now, &discovery.project.files);
        let (regressions, allowed): (Vec<_>, Vec<_>) = now
            .iter()
            .cloned()
            .zip(regressed)
            .partition(|(_, regressed)| *regressed);
        let printed = |split: Vec<(Diagnostic, bool)>| printed(split.into_iter().map(|(d, _)| d));
        BaselineSplit {
            allowed: printed(allowed).len(),
            regressions: printed(regressions).len(),
        }
    });
    let shown = printed(now);
    let exact = shown
        .iter()
        .filter(|d| d.certainty == Certainty::Exact)
        .count();
    ReportsNow {
        shown: shown.len(),
        exact,
        heuristic: shown.len() - exact,
        withheld_heuristic,
        heuristic_rules_off: current.heuristic_off().len(),
        baseline,
    }
}

/// `diags` as `heki check` prints them: reports sharing a root cause folded into one.
fn printed(diags: impl IntoIterator<Item = Diagnostic>) -> Vec<Diagnostic> {
    let mut diags: Vec<Diagnostic> = diags.into_iter().collect();
    fold_root_causes(&mut diags);
    diags
}

/// The Python files that discovery from the project root finds (with the same ignore files and
/// `exclude`) and the paths given leave out, as the output writes them. Paths that hold the
/// root leave nothing out.
fn left_out(
    project: &Project,
    paths: &[PathBuf],
    shown: &dyn Fn(&Path) -> String,
) -> Result<LeftOut, ProjectError> {
    let env = &project.env;
    let holds_root = paths
        .iter()
        .any(|p| canonical(p).is_ok_and(|p| env.root.starts_with(p)));
    if holds_root {
        return Ok(LeftOut {
            python_files: 0,
            files: Vec::new(),
            truncated: false,
        });
    }
    let analyzed: FxHashSet<String> = project
        .files
        .iter()
        .map(|f| f.rel.clone())
        .chain(project.errors.iter().map(|e| rel_posix(&e.path, &env.root)))
        .collect();
    let exclude = exclude_set(&env.cfg)?;
    let (found, complete) = files_left_out(
        &env.root,
        &exclude,
        &|rel| analyzed.contains(rel),
        MAX_LISTED,
        MAX_CENSUS_ENTRIES,
    )?;
    Ok(LeftOut {
        python_files: found.len(),
        files: found.iter().map(|p| shown(p)).collect(),
        truncated: !complete,
    })
}

fn left_out_note(left_out: &LeftOut) -> String {
    let mut examples = left_out.files[..left_out.files.len().min(MAX_EXAMPLES)].join(", ");
    if left_out.python_files > MAX_EXAMPLES || left_out.truncated {
        examples.push_str(", ...");
    }
    let at_least = if left_out.truncated { "at least " } else { "" };
    format!(
        "{at_least}{} Python file(s) below the project root were not analyzed ({examples}): heki \
         takes the analyzed files as the whole project, so what it says about callers, readers, \
         and writers holds for those files only.",
        left_out.python_files
    )
}

/// The report as the text output prints it.
pub fn render_text(report: &RecommendReport) -> String {
    let mut out = String::new();
    let analyzed = &report.analyzed;
    let _ = writeln!(
        out,
        "heki recommend (experimental) on {}: {} file(s), {} function(s)\n",
        report.root, analyzed.files, analyzed.functions
    );
    let now = &report.reports_now;
    let _ = writeln!(
        out,
        "Reports now  {} shown ({} exact, {} heuristic), {} heuristic withheld, {} heuristic \
         rule(s) off",
        now.shown, now.exact, now.heuristic, now.withheld_heuristic, now.heuristic_rules_off
    );
    if let Some(split) = &now.baseline {
        let _ = writeln!(
            out,
            "  with the baseline: {} allowed, {} regression(s)",
            split.allowed, split.regressions
        );
    }
    if !report.notes.is_empty() {
        out.push_str("\nNotes\n");
        for note in &report.notes {
            let _ = writeln!(out, "  - {note}");
        }
    }
    let file = report.config_file.as_deref().unwrap_or(NEW_CONFIG_FILE);
    let _ = writeln!(out, "\n{file}: nothing to add");
    out
}
