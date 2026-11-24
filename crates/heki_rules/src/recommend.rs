//! `heki recommend` (experimental): advice on the configuration of one project, computed from
//! the analysis `heki check` runs on it (docs/recommend.md, ADR-020).
//!
//! The discovery run analyzes the project once with every rule selected, as `heki check` would
//! with that selection: inline suppressions and `per-file-ignores` apply. What the current
//! configuration shows is the part of that run's reports whose rules it selects, which is what
//! `heki check` prints, since a rule's reports do not depend on which other rules are selected.
//! The contract trial declares every contract candidate at once; the verification run is
//! `heki check` with the whole block applied, and gives the counts the block promises. Each run
//! builds its index after the previous one is dropped, from the files parsed once.
//! The report is the same for the same input, configuration, and heki version: every list is
//! sorted, and every cap is a count.

pub mod blind_spots;
pub mod block;
pub mod contracts;
pub mod premises;
pub mod rules;

use crate::project::{
    Facts, Indexed, ParsedFile, Project, ProjectError, paths_leave_out, regressed,
};
use crate::selection::{RuleSelection, SelectionError, Selectors};
use crate::{CORE_RULES, Certainty, RuleConfigError, SummarySnapshot, fold_root_causes};
use heki_diagnostics::{Diagnostic, RatchetBaseline};
use heki_env::{ConfigLayout, HekiConfig, LintConfig};
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

/// Where to report what only heki can change: a fact only its shipped catalog can hold.
pub const ISSUES_URL: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/issues");

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

impl heki_env::Explain for RecommendError {
    fn explain(&self) -> heki_env::Problem {
        match self {
            Self::Selection(e) => e.explain(),
            Self::Rules(e) => e.explain(),
            Self::Project(e) => e.explain(),
        }
    }
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
    /// The paths given on the command line.
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
    /// What keeps the heuristic reports of exact claims heuristic.
    pub open_premises: premises::OpenPremises,
    /// What heki cannot see: no count of reports applies there.
    pub blind_spots: blind_spots::BlindSpotsSection,
    /// Every rule the configuration does not run in full: the rules it leaves off, and the exact
    /// rules whose heuristic reports it withholds. The block's rules come first.
    pub rules: Vec<rules::RuleAdvice>,
    /// The contracts the code keeps, which the block declares: each kept by the contract trial,
    /// then applied with the whole block by the verification run. By kind, then by rank within
    /// it.
    pub contracts: Vec<contracts::ContractAdvice>,
    /// The candidates a run with them declared reported for: not in the block.
    pub rejected_contracts: Vec<contracts::RejectedContract>,
    /// The candidates past the cap of their kind, which no trial declared.
    pub contracts_not_tried: Vec<contracts::NotTried>,
    /// The entries of the contracts configured now that match nothing in the analyzed files, one
    /// per kind, key, and text, in configuration order.
    pub unmatched_entries: Vec<crate::unmatched::UnmatchedEntry>,
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
    /// Whether the count stopped before the end (after 50 files, or after the first 10,000
    /// entries below the root) or could not read an entry.
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Run {
    pub label: RunLabel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RunLabel {
    /// Every rule selected, on the configuration as it is.
    Discovery,
    /// The source-root trial: the module names of the files with directories added to `src`,
    /// and the imports they resolve (no analysis).
    SourceRoots,
    /// The configuration with a catalog entry that waits and sends a query for each library
    /// name tried, and the rules those effects make report.
    HypotheticalCatalog,
    /// The configuration with every contract candidate declared, the contract rules selected.
    ContractTrial,
    /// The configuration with the whole block applied, the block's rules and contracts selected.
    Verification,
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
    let (left_out, left_out_paths) = left_out(&project, shown)?;
    let functions = project.files.iter().map(|f| f.ir.functions.len()).sum();
    let files = project.files.len();
    let selection = discovery_selection(&env.cfg.lint, &current)?;
    let mut runs = vec![Run {
        label: RunLabel::Discovery,
    }];
    let discovery = Indexed::build(project, selection, snapshot, Facts::Recommend)?;
    // The heuristic reports of exact claims record what they leave open, and why.
    let (diags, _) = discovery.diagnose_units(&discovery.project.units(), true)?;
    let files_analyzed = &discovery.project.files;
    let open_premises = premises::open_premises(&diags, shown);
    let discovered = blind_spots::Discovered::of(&discovery.index, files_analyzed, shown);
    let mut rules = rules::advise(&rules::Inputs {
        current: &current,
        baseline: baseline.as_ref(),
        files: files_analyzed,
        discovery: &diags,
    });
    let reports_now = reports_now(&current, baseline.as_ref(), files_analyzed, &diags);
    let configured_now = contracts::configured_reports(&discovery.project.env.cfg, &diags);
    drop(diags);
    let env = &discovery.project.env;
    let declared = crate::declared_contracts(env, &discovery.index, &current)?;
    let unmatched_entries = crate::unmatched_entries(env, &discovery.index, &current)?;
    // Test code as `relax-async-in-tests` finds it: a test file, or a `test_*` function in any
    // file (a test class's method in Django's `tests.py`, for one).
    let tests: FxHashSet<&Path> = (discovery.project.files.iter())
        .filter(|f| {
            let qualnames = f.ir.functions.iter().map(|g| g.qualname.as_str());
            std::iter::once("")
                .chain(qualnames)
                .any(|q| heki_effect::code_kind::is_test_code(q, &f.rel))
        })
        .map(|f| f.path.as_path())
        .collect();
    let generated = contracts::generate(&discovery.index, &env.cfg, &declared, &current, &tests);
    drop(tests);
    // One index at a time: the trials build their own. (A partial move would keep it until the
    // end of the function.)
    let Indexed { project, index, .. } = discovery;
    drop(index);
    let left_out_paths = (!left_out.truncated).then_some(&left_out_paths[..]);
    let roots = blind_spots::source_roots(&project, left_out_paths, baseline.as_ref(), shown);
    if !roots.candidates.is_empty() {
        runs.push(Run {
            label: RunLabel::SourceRoots,
        });
    }
    let tried = discovered.to_try(&roots);
    let (mut project, trial) = if tried.is_empty() {
        (project, blind_spots::Trial::default())
    } else {
        runs.push(Run {
            label: RunLabel::HypotheticalCatalog,
        });
        blind_spots::hypothetical_catalog(project, tried)?
    };
    let src = roots.src.clone();
    let blind_spots = discovered.finish(&trial, roots, &project.files);
    let user = project.env.cfg.clone();
    // The candidates name modules as the files are named now, from an analysis without the
    // imports a new source root resolves: with `src` in the block, a run after it proposes them.
    let section = if src.is_some() {
        contracts::Section::deferred(generated)
    } else if generated.is_empty() {
        contracts::Section::untried(generated)
    } else {
        let trial = contracts::trial_config(&user, &generated);
        let rules_run = contracts::trial_rules(&generated);
        let (analyzed, kept, suppressed) = analyze(project, trial, &rules_run)?;
        project = analyzed;
        runs.push(Run {
            label: RunLabel::ContractTrial,
        });
        contracts::judge(
            generated,
            &user,
            (&kept, &suppressed),
            &configured_now,
            (&left_out, project.errors.len()),
            shown,
        )
    };
    // The verification run: `heki check` with the block's rules and contracts applied, which
    // gives the counts the block promises. One run suffices: discovery ran every rule, so it has
    // every fact this run has, and the contracts come from its census, so a kept contract never
    // reports here. With `src` in the block, a run on the files as they are named now would not
    // count the reports through the imports the new source roots resolve.
    let verification = additions(&rules, &section, &current);
    let unverified = src.is_some() && verification != block::Additions::default();
    let verified = src.is_none() && verification != block::Additions::default();
    if verified {
        let mut applied = user.clone();
        (applied.lint.extend_select).extend(verification.extend_select.iter().cloned());
        section.declare_in(&mut applied);
        let mut rules_run: Vec<&str> = verification
            .extend_select
            .iter()
            .map(String::as_str)
            .collect();
        for rule in section.rules() {
            rules_run.push(rule);
        }
        let (analyzed, kept, _) = analyze(project, applied, &rules_run)?;
        project = analyzed;
        rules::recount(
            &mut rules,
            &rules::Inputs {
                current: &current,
                baseline: baseline.as_ref(),
                files: &project.files,
                discovery: &kept,
            },
        );
    }
    if verified {
        runs.push(Run {
            label: RunLabel::Verification,
        });
    }
    let additions = block::Additions {
        src,
        ..additions(&rules, &section, &current)
    };
    let block = block::render(&additions, &user, layout);
    let mut notes = Vec::new();
    if left_out.python_files > 0 || left_out.truncated {
        notes.push(left_out_note(&left_out));
    }
    notes.extend(blind_spots::notes(&blind_spots, &trial));
    notes.extend(section.notes());
    if unverified {
        notes.push(format!(
            "{}: the counts of the block's rules are of the files as they are named now; run \
             heki recommend again with it for the counts under it",
            contracts::SRC_RENAMES
        ));
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
        runs,
        reports_now,
        open_premises,
        blind_spots,
        rules,
        contracts: section.advice(),
        rejected_contracts: section.rejected,
        contracts_not_tried: section.not_tried,
        unmatched_entries,
        notes,
        config: block.config,
        config_toml: block.toml,
        content: String::new(),
    };
    report.content = render_text(&report);
    Ok(report)
}

/// The selection of the discovery run: every rule by its code ([`selection_of`]). `MUT103`
/// runs only when the current selection runs it: it compares with a snapshot the user saves.
fn discovery_selection(
    lint: &LintConfig,
    current: &RuleSelection,
) -> Result<RuleSelection, SelectionError> {
    let every_rule = CORE_RULES
        .iter()
        .filter(|r| r.code != "MUT103" || current.is_enabled(r.code))
        .map(|r| r.code.to_owned())
        .collect();
    selection_of(lint, every_rule)
}

/// The selection of the rules `codes` by their codes, so that no heuristic report of theirs is
/// withheld, with the configuration's `per-file-ignores`.
fn selection_of(lint: &LintConfig, codes: Vec<String>) -> Result<RuleSelection, SelectionError> {
    let lint = LintConfig {
        select: Some(Vec::new()),
        extend_select: Vec::new(),
        ignore: Vec::new(),
        ..lint.clone()
    };
    let selectors = Selectors {
        select: codes,
        extend_select: Vec::new(),
        ignore: Vec::new(),
        heuristic: Some(true),
    };
    RuleSelection::resolve(&lint, &selectors)
}

/// `project` analyzed as `heki check` would with the configuration `cfg` in place of its own
/// ([`Project::set_config`]: the contracts, the rules, and the catalog may differ), running the
/// rules `codes` with the configuration's `per-file-ignores`: the project with its configuration
/// back, and the reports inline suppressions keep and drop.
fn analyze(
    mut project: Project,
    cfg: HekiConfig,
    codes: &[&str],
) -> Result<(Project, Vec<Diagnostic>, Vec<Diagnostic>), RecommendError> {
    let own = project.set_config(cfg)?;
    let codes = codes.iter().map(|c| (*c).to_owned()).collect();
    let selection = selection_of(&project.env.cfg.lint, codes)?;
    let indexed = Indexed::build(project, selection, None, Facts::Check)?;
    let (kept, suppressed) = indexed.diagnose_with_suppressed()?;
    let Indexed {
        mut project, index, ..
    } = indexed;
    drop(index);
    project.set_config(own)?;
    Ok((project, kept, suppressed))
}

/// What the block adds: the rules section 2 enables, the contracts section 3 keeps, and the
/// contract rules those need that the current selection does not run.
fn additions(
    rules: &[rules::RuleAdvice],
    section: &contracts::Section,
    current: &RuleSelection,
) -> block::Additions {
    let mut extend_select: Vec<String> = rules
        .iter()
        .filter(|r| r.in_block)
        .map(|r| r.code.clone())
        .chain(section.rules_to_select(current))
        .collect();
    extend_select.sort();
    extend_select.dedup();
    let (write_owners, read_owners, effect_boundaries) = section.block_entries();
    block::Additions {
        extend_select,
        write_owners,
        read_owners,
        effect_boundaries,
        ..block::Additions::default()
    }
}

/// The discovery run's reports that `current` shows, counted as `heki check` prints them.
fn reports_now(
    current: &RuleSelection,
    baseline: Option<&RatchetBaseline>,
    files: &[ParsedFile],
    diags: &[Diagnostic],
) -> ReportsNow {
    let mut now: Vec<Diagnostic> = diags
        .iter()
        .filter(|d| current.is_enabled(&d.rule_code))
        .cloned()
        .collect();
    let withheld_heuristic = current.withhold_heuristic(&mut now);
    let baseline = baseline.map(|baseline| {
        let regressed = regressed(baseline, &now, files);
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
/// `exclude`) and the paths analyzed leave out, as the output writes them, then as paths. Paths
/// that hold the root leave nothing out.
fn left_out(
    project: &Project,
    shown: &dyn Fn(&Path) -> String,
) -> Result<(LeftOut, Vec<PathBuf>), ProjectError> {
    let (found, complete) = paths_leave_out(project, MAX_LISTED, MAX_CENSUS_ENTRIES)?;
    let left_out = LeftOut {
        python_files: found.len(),
        files: found.iter().map(|p| shown(p)).collect(),
        truncated: !complete,
    };
    Ok((left_out, found))
}

fn left_out_note(left_out: &LeftOut) -> String {
    if left_out.python_files == 0 {
        return format!(
            "the count of the Python files below the project root stopped before the end (after \
             the first {MAX_CENSUS_ENTRIES} entries, or at an entry it could not read): heki \
             takes the analyzed files as the whole project, so what it says about callers, \
             readers, and writers holds for those files only."
        );
    }
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
    premises::render(&mut out, &report.open_premises);
    blind_spots::render(&mut out, &report.blind_spots);
    rules::render(&mut out, &report.rules, now.baseline.is_some());
    contracts::render(
        &mut out,
        &report.contracts,
        &report.rejected_contracts,
        &report.contracts_not_tried,
    );
    if !report.unmatched_entries.is_empty() {
        out.push_str("\nContract entries that match nothing\n");
        for entry in &report.unmatched_entries {
            let _ = writeln!(out, "  - {}", entry.message());
        }
    }
    if !report.notes.is_empty() {
        out.push_str("\nNotes\n");
        for note in &report.notes {
            let _ = writeln!(out, "  - {note}");
        }
    }
    let file = report.config_file.as_deref().unwrap_or(NEW_CONFIG_FILE);
    if report.config_toml.is_empty() {
        let _ = writeln!(out, "\n{file}: nothing to add");
    } else {
        let _ = writeln!(out, "\n{file}: keys to add or change");
        for line in report.config_toml.lines() {
            if line.is_empty() {
                out.push('\n');
            } else {
                let _ = writeln!(out, "  {line}");
            }
        }
    }
    out
}
