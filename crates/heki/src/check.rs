//! `heki check`: discover -> parse / IR (parallel) -> effect summaries -> rules -> inline
//! suppressions -> fixes -> Ratchet baseline -> output. Exit codes: 0 nothing that fails the run,
//! 1 a report at the `fail-on` severity (an error by default; with a baseline, only regressions
//! are reported), a baseline update refused, or pending `--diff` changes, 2 configuration / I/O /
//! unknown rule / syntax errors.

use crate::failure;
use crate::{AnalysisArgs, CheckArgs, ColorChoice, OutputFormat, SnapshotArgs};
use anyhow::Result;
use heki_ast::LineIndex;
use heki_diagnostics::{
    BASELINE_VERSION, BaselineKey, BaselineVersion, CountUnit, Diagnostic, FileFailure,
    RatchetBaseline, Regression, StaleEntry, Stylesheet, Tally, TextEdit, baseline_counts,
    fix_python_source, github_escape, path_text, render, unified_diff,
};
use heki_effect::ProjectIndex;
use heki_env::{Docs, LintConfig, Problem, Severity, UnmatchedEntries};
use heki_rules::project::{
    CommandPaths, Facts, FileError, IndexFacts, Indexed, ParsedFile, Project, ProjectEnv,
    ProjectOptions, RulesRun, baseline_key, canonical, imports_left_out, paths_leave_out,
    rel_posix,
};
use heki_rules::{
    RuleSelection, SNAPSHOT_FILE, Selectors, SummarySnapshot, find_rule, fixes_unconditionally,
    fold_root_causes, recommend,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeMap;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The selection options of the command line.
pub(crate) fn selectors(args: &AnalysisArgs) -> Selectors {
    Selectors {
        select: args.select.clone(),
        extend_select: args.extend_select.clone(),
        ignore: args.ignore.clone(),
        heuristic: args.heuristic.value(),
        size_dependent_rewrites: false,
    }
}

/// The selection options of `heki check`: [`selectors`] and `--include-size-dependent`.
pub(crate) fn check_selectors(args: &CheckArgs) -> Selectors {
    Selectors {
        size_dependent_rewrites: args.include_size_dependent,
        ..selectors(&args.analysis)
    }
}

/// Rule selection of a run (the command line's `selectors` over the config).
pub(crate) fn selection(selectors: &Selectors, project: &Project) -> Result<RuleSelection> {
    Ok(RuleSelection::resolve(&project.env.cfg.lint, selectors)?)
}

/// `--fail-on` over `lint.fail-on`: the least severity whose reports make the run fail.
pub(crate) fn fail_on(args: &CheckArgs, lint: &LintConfig) -> Severity {
    args.fail_on
        .map_or_else(|| lint.fail_on_severity(), Severity::from)
}

/// Whether one of the reports `diags` makes the run fail (exit code 1) at `fail_on`
/// ([`Diagnostic::fails_run`]).
pub(crate) fn any_failing(diags: &[Diagnostic], fail_on: Severity) -> bool {
    diags.iter().any(|d| d.fails_run(fail_on))
}

/// The saved summary snapshot when `MUT103` is enabled (`None` with a warning if missing).
fn snapshot_for(
    selection: &RuleSelection,
    root: &Path,
    format: OutputFormat,
) -> Result<Option<SummarySnapshot>> {
    if !selection.is_enabled("MUT103") {
        return Ok(None);
    }
    let path = root.join(SNAPSHOT_FILE);
    if path.exists() {
        return Ok(Some(SummarySnapshot::load(&path)?));
    }
    log!(
        format,
        "warning: `MUT103` is enabled but `{}` does not exist; run `heki snapshot --write` first",
        path.display()
    );
    Ok(None)
}

/// The project at `paths` (with `overrides` in place of files' text), with the rule selection of
/// `selectors` and the saved summary snapshot `heki check` uses for `analysis`. `format` is the
/// output format the run's warnings follow.
pub(crate) fn load(
    paths: &[PathBuf],
    analysis: &AnalysisArgs,
    selectors: &Selectors,
    format: OutputFormat,
    overrides: Option<&FxHashMap<PathBuf, String>>,
) -> Result<(Project, RuleSelection, Option<SummarySnapshot>)> {
    let project = Project::load(&ProjectOptions {
        paths,
        config: analysis.config.as_deref(),
        working_dir: None,
        target_version: analysis.target_version.as_deref(),
        overrides,
        removed: None,
    })?;
    let selection = selection(selectors, &project)?;
    let snapshot = snapshot_for(&selection, &project.env.root, format)?;
    Ok((project, selection, snapshot))
}

/// [`load`] with the index `heki check` builds for `analysis`, with `facts`.
pub(crate) fn load_and_index(
    paths: &[PathBuf],
    analysis: &AnalysisArgs,
    selectors: &Selectors,
    format: OutputFormat,
    overrides: Option<&FxHashMap<PathBuf, String>>,
    facts: Facts,
) -> Result<Indexed> {
    let (project, selection, snapshot) = load(paths, analysis, selectors, format, overrides)?;
    Ok(Indexed::build(project, selection, snapshot, facts)?)
}

/// One analysis pass: the project, its index, and its diagnostics after inline suppressions.
struct Analyzed {
    project: Project,
    index: ProjectIndex,
    diags: Vec<Diagnostic>,
    /// The heuristic rules that did not run only because `lint.heuristic` is off.
    heuristic_off: usize,
    /// The heuristic reports of exact rules withheld because `lint.heuristic` is off.
    withheld: Vec<Diagnostic>,
    selection: RuleSelection,
    /// Whether the run compared with a summary snapshot.
    snapshot: bool,
    /// The files of `--touched`, when it is given.
    touched: Option<Touched>,
}

impl Analyzed {
    fn rules_run(&self) -> RulesRun<'_> {
        RulesRun {
            selection: &self.selection,
            snapshot: self.snapshot,
            lint: &self.project.env.cfg.lint,
        }
    }
}

fn analyze(
    args: &CheckArgs,
    paths: &CommandPaths,
    overrides: Option<&FxHashMap<PathBuf, String>>,
) -> Result<Analyzed> {
    let indexed = load_and_index(
        &paths.analyzed,
        &args.analysis,
        &check_selectors(args),
        args.output_format,
        overrides,
        Facts::Check,
    )?;
    let (diags, withheld) = indexed.diagnose()?;
    let Indexed {
        project,
        index,
        selection,
        snapshot,
    } = indexed;
    let touched = Touched::of(&args.touched, &paths.files, &project);
    Ok(Analyzed {
        project,
        index,
        diags,
        heuristic_off: selection.heuristic_off().len(),
        withheld,
        selection,
        snapshot: snapshot.is_some(),
        touched,
    })
}

/// Whether `--fix` / `--diff` applies this diagnostic's fix: `safe` + `unconditional` rules
/// (`OPT101` / `OPT102`) and reports on suppression comments always; `size-dependent` ones with
/// `--include-size-dependent`, and unsafe ones (`MUT118`) with `--unsafe-fixes`.
fn applicable(d: &Diagnostic, args: &CheckArgs) -> bool {
    let rule = find_rule(&d.rule_code);
    d.fix.is_some()
        && (fixes_unconditionally(d)
            || rule.is_some_and(|r| r.size_dependent_fix) && args.include_size_dependent
            || rule.is_some_and(|r| r.unsafe_fix) && args.unsafe_fixes)
}

/// Fix passes before `--fix` / `--diff` stop (each pass re-proves every candidate on the text
/// the previous pass produced; a pass that applies nothing ends the loop).
const MAX_FIX_PASSES: usize = 10;

/// One file changed by `--fix` / `--diff`.
struct FixedFile {
    path: PathBuf,
    original: String,
    fixed: String,
    /// The fixes applied to it.
    fixes: usize,
}

/// How `heki check` prints the path of a checked file, in every output format, in `--diff`
/// headers, and in its errors (as Ruff does): relative to the working directory when it is
/// inside it (also through a symbolic link), otherwise as given.
pub(crate) fn shown_path(p: &Path) -> PathBuf {
    if p.is_relative() {
        return p.to_path_buf();
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    p.strip_prefix(&cwd)
        .ok()
        .map(Path::to_path_buf)
        .or_else(|| {
            let abs = canonical(p).ok()?;
            let rel = abs.strip_prefix(canonical(&cwd).ok()?).ok()?;
            Some(rel.to_path_buf())
        })
        .unwrap_or_else(|| p.to_path_buf())
}

/// [`shown_path`] as reports write it.
pub(crate) fn shown_text(p: &Path) -> String {
    path_text(&shown_path(p))
}

/// [`shown_text`] of a directory: `.` for the working directory itself.
pub(crate) fn shown_dir(p: &Path) -> String {
    match shown_text(p) {
        shown if shown.is_empty() => ".".to_owned(),
        shown => shown,
    }
}

/// File errors as `heki check` prints them: with the paths it prints ([`shown_text`]).
pub(crate) fn shown_errors<'a>(errors: impl IntoIterator<Item = &'a FileError>) -> Vec<String> {
    errors
        .into_iter()
        .map(|e| e.report(&shown_text(&e.path)))
        .collect()
}

/// File errors as SARIF output carries them, with the paths `heki check` prints.
pub(crate) fn shown_failures<'a>(
    errors: impl IntoIterator<Item = &'a FileError>,
) -> Vec<FileFailure> {
    errors
        .into_iter()
        .map(|e| FileFailure {
            path: shown_path(&e.path),
            position: e.position,
            message: e.message.clone(),
        })
        .collect()
}

/// The files `--touched` names, and the files PATHS name in a project ([`CommandPaths`]), among
/// the files the run analyzed. A run with them shows and fails on only the reports that touch
/// one of them (in the file, or with a hop of the call chain, `related`, in it), and rewrites
/// only them. The analysis still covers the whole of what it analyzes, since a report in a file
/// the change left alone can come from a callee it changed.
pub(crate) struct Touched {
    /// Keyed like [`ParsedFile::path`], as reports name files.
    files: FxHashSet<PathBuf>,
    /// The baseline paths of `files`.
    baseline_paths: FxHashSet<String>,
    /// The paths `--touched` gives that name no analyzed file.
    unknown: Vec<PathBuf>,
    /// The files PATHS give that are not analyzed.
    unknown_files: Vec<PathBuf>,
}

impl Touched {
    /// The files `touched` (`--touched`) and `files` (of PATHS) name in `project`, or `None`
    /// when both are empty. Paths are compared as the file system resolves them, so `./a.py`, an
    /// absolute path, and a link name the same file.
    pub(crate) fn of(touched: &[PathBuf], files: &[PathBuf], project: &Project) -> Option<Self> {
        if touched.is_empty() && files.is_empty() {
            return None;
        }
        let wanted: FxHashSet<PathBuf> = touched
            .iter()
            .chain(files)
            .filter_map(|p| canonical(p).ok())
            .collect();
        let root = &project.env.root;
        // A file that could not be parsed is touched too: its error is the report.
        let analyzed = project
            .files
            .iter()
            .map(|f| (&f.path, f.rel.clone()))
            .chain(
                project
                    .errors
                    .iter()
                    .map(|e| (&e.path, rel_posix(&e.path, root))),
            );
        let mut found = FxHashSet::default();
        let (mut analyzed_files, mut baseline_paths) = (FxHashSet::default(), FxHashSet::default());
        for (path, rel) in analyzed {
            let Some(resolved) = canonical(path).ok().filter(|p| wanted.contains(p)) else {
                continue;
            };
            found.insert(resolved);
            analyzed_files.insert(path.clone());
            baseline_paths.insert(rel);
        }
        let not_found = |given: &[PathBuf]| -> Vec<PathBuf> {
            given
                .iter()
                .filter(|p| canonical(p).map_or(true, |p| !found.contains(&p)))
                .cloned()
                .collect()
        };
        Some(Self {
            unknown: not_found(touched),
            unknown_files: not_found(files),
            files: analyzed_files,
            baseline_paths,
        })
    }

    /// How many analyzed files are touched.
    pub(crate) fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Whether `d` is in a touched file or has a hop of its call chain in one.
    fn touches(&self, d: &Diagnostic) -> bool {
        self.files.contains(&d.path) || d.related.iter().any(|r| self.files.contains(&r.path))
    }

    /// Whether a run with `touched` shows `d`.
    pub(crate) fn shows(touched: Option<&Self>, d: &Diagnostic) -> bool {
        touched.is_none_or(|t| t.touches(d))
    }

    /// The file errors of `errors` a run with `touched` reports and fails on, as it does its
    /// reports: only those of a touched file. A note names the others once: a run over given
    /// files says with its exit code whether those files could be analyzed, and another file
    /// that cannot be read only leaves out the reports that need its code.
    pub(crate) fn errors<'e>(
        touched: Option<&Self>,
        errors: &'e [FileError],
        format: OutputFormat,
    ) -> Vec<&'e FileError> {
        let (shown, others): (Vec<&FileError>, Vec<&FileError>) =
            (errors.iter()).partition(|e| touched.is_none_or(|t| t.files.contains(&e.path)));
        if !others.is_empty() {
            let mut paths: Vec<String> = (others.iter().take(recommend::MAX_EXAMPLES))
                .map(|e| format!("`{}`", shown_text(&e.path)))
                .collect();
            if others.len() > recommend::MAX_EXAMPLES {
                paths.push("...".to_owned());
            }
            log!(
                format,
                "note: {} file(s) the run does not report on could not be analyzed: {}; reports that need their code are missing: name a file to see its error, or exclude it with `extend-exclude`",
                others.len(),
                paths.join(", ")
            );
        }
        shown
    }

    /// Whether `--fix` and `--diff` in a run with `touched` apply the fix of `d`, a report it
    /// shows: only in a touched file.
    fn rewrites(touched: Option<&Self>, d: &Diagnostic) -> bool {
        touched.is_none_or(|t| t.files.contains(&d.path))
    }

    /// How many of `diags` a run with `touched` shows.
    pub(crate) fn shown(touched: Option<&Self>, diags: &[Diagnostic]) -> usize {
        diags.iter().filter(|d| Self::shows(touched, d)).count()
    }

    /// Drop the reports of `diags` that touch no touched file.
    pub(crate) fn keep(&self, diags: &mut Vec<Diagnostic>) {
        diags.retain(|d| self.touches(d));
    }

    /// The touched files as reports print them, in order.
    pub(crate) fn shown_files(&self) -> Vec<String> {
        let mut shown: Vec<String> = self.files.iter().map(|p| shown_text(p)).collect();
        shown.sort_unstable();
        shown
    }

    /// The baseline entries a run with these files judges, of the reports `diags` before the
    /// comparison.
    fn baseline_view(
        &self,
        diags: &[Diagnostic],
        key_path: &dyn Fn(&Path) -> String,
        version: BaselineVersion,
    ) -> BaselineView<'_> {
        BaselineView {
            touched: self,
            keys: diags
                .iter()
                .filter(|d| self.touches(d))
                .map(|d| heki_diagnostics::baseline_key(d, key_path(&d.path), version))
                .collect(),
        }
    }

    /// Warn once about the given paths that name no analyzed file.
    pub(crate) fn warn(&self, format: OutputFormat) {
        let listed = |paths: &[PathBuf]| -> Vec<String> {
            paths
                .iter()
                .map(|p| format!("`{}`", path_text(p)))
                .collect()
        };
        if !self.unknown_files.is_empty() {
            let paths = listed(&self.unknown_files);
            log!(
                format,
                "warning: PATHS name {} file(s) that are not among the analyzed files (not a Python file of the project or below the directories given, or excluded): {}",
                paths.len(),
                paths.join(", ")
            );
        }
        if !self.unknown.is_empty() {
            let paths = listed(&self.unknown);
            log!(
                format,
                "warning: `--touched` names {} path(s) that are not among the analyzed files (missing, not a Python file under PATHS, or excluded): {}",
                paths.len(),
                paths.join(", ")
            );
        }
    }
}

/// Warn about the files PATHS name that `project` does not analyze, for a command that reports
/// on everything it analyzes (`heki recommend`, `heki inspect`, `heki snapshot`): a file names
/// its project, and with directories, it is analyzed only below them.
pub(crate) fn warn_files_not_analyzed(
    paths: &CommandPaths,
    project: &Project,
    format: OutputFormat,
) {
    if let Some(named) = Touched::of(&[], &paths.files, project) {
        named.warn(format);
    }
}

/// The baseline entries a `--touched` run judges: an entry of a touched file, and an entry with a
/// report that touches one. The reports of the other entries are not shown, so a grown or
/// stale count there is not this run's to report.
struct BaselineView<'t> {
    touched: &'t Touched,
    keys: FxHashSet<BaselineKey>,
}

impl BaselineView<'_> {
    /// Whether the run judges the entry or report `key`; a group entry also when it holds a
    /// shown group, one of `within`.
    fn shows(&self, key: &BaselineKey, within: &[BaselineKey]) -> bool {
        self.keys.contains(key)
            || self.touched.baseline_paths.contains(&key.path)
            || within.iter().any(|g| self.keys.contains(g))
    }
}

/// What `--fix` / `--diff` would do.
struct FixRun {
    fixes: usize,
    files: Vec<FixedFile>,
    /// Read / syntax errors of the last pass.
    errors: Vec<FileError>,
    /// Files whose fixes would not parse, left unchanged.
    refused: Vec<String>,
    /// The files of `--touched` in the last pass.
    touched: Option<Touched>,
    /// The fixes of shown reports in files that are not touched, left unapplied in the last
    /// pass.
    not_touched: usize,
}

/// Apply fixes in memory until nothing more applies.
fn fix_in_memory(args: &CheckArgs, paths: &CommandPaths) -> Result<FixRun> {
    let mut overrides: FxHashMap<PathBuf, String> = FxHashMap::default();
    let mut originals: BTreeMap<PathBuf, String> = BTreeMap::new();
    let mut applied_to: FxHashMap<PathBuf, usize> = FxHashMap::default();
    let mut total = 0;
    let mut errors;
    let mut touched;
    let mut not_touched;
    let mut passes = 0;
    let mut refused: Vec<String> = Vec::new();
    loop {
        let a = analyze(args, paths, Some(&overrides))?;
        errors = a.project.errors;
        touched = a.touched;
        let by_path;
        (by_path, not_touched) =
            fixes_to_apply(&a.diags, touched.as_ref(), &|d| applicable(d, args));
        let mut pass = 0;
        for file in &a.project.files {
            let Some(groups) = by_path.get(file.path.as_path()) else {
                continue;
            };
            let fixed = fix_python_source(&shown_text(&file.path), &file.source, groups);
            pass += fixed.applied.len();
            *applied_to.entry(file.path.clone()).or_default() += fixed.applied.len();
            refused.extend(fixed.refused);
            originals
                .entry(file.path.clone())
                .or_insert_with(|| file.source.clone());
            overrides.insert(file.path.clone(), fixed.text);
        }
        total += pass;
        passes += 1;
        if pass == 0 || passes == MAX_FIX_PASSES {
            break;
        }
    }
    let files = originals
        .into_iter()
        .filter_map(|(path, original)| {
            let fixed = overrides.remove(&path)?;
            let fixes = applied_to.get(&path).copied().unwrap_or_default();
            (fixed != original).then_some(FixedFile {
                path,
                original,
                fixed,
                fixes,
            })
        })
        .collect();
    // A file whose fixes break it is refused again in every later pass.
    refused.sort_unstable();
    refused.dedup();
    Ok(FixRun {
        fixes: total,
        files,
        errors,
        refused,
        touched,
        not_touched,
    })
}

/// The fixes of `diags` that `--fix` and `--diff` apply in a run with `touched`, by file, and how
/// many fixes of shown reports in files that are not touched they leave: a run over touched files
/// rewrites only them, since a pre-commit hook must never change a file the commit left alone.
/// `applicable` says which fixes the options apply ([`applicable`]).
fn fixes_to_apply<'d>(
    diags: &'d [Diagnostic],
    touched: Option<&Touched>,
    applicable: &dyn Fn(&Diagnostic) -> bool,
) -> (BTreeMap<&'d Path, Vec<&'d [TextEdit]>>, usize) {
    let mut by_path: BTreeMap<&Path, Vec<&[TextEdit]>> = BTreeMap::new();
    let mut not_touched = 0;
    let shown = diags
        .iter()
        .filter(|d| applicable(d) && Touched::shows(touched, d));
    for d in shown {
        let Some(edits) = &d.fix else { continue };
        if Touched::rewrites(touched, d) {
            by_path.entry(d.path.as_path()).or_default().push(edits);
        } else {
            not_touched += 1;
        }
    }
    (by_path, not_touched)
}

/// The line after the summary of `--fix` and `--diff` that says how many fixes a run over
/// touched files left in the other files, empty when it left none.
fn not_touched_note(not_touched: usize) -> String {
    if not_touched == 0 {
        return String::new();
    }
    format!(
        "note: {not_touched} fix(es) in files outside the touched ones not applied; name the files to fix them\n"
    )
}

pub(crate) fn run(args: &CheckArgs) -> Result<ExitCode> {
    if let Some(target) = args.output_format_json_schema {
        print!("{}", crate::prompt::json_schema(target));
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(question) = &args.generate_prompt {
        return crate::prompt::run(args, question);
    }
    recording_steps(args.walk_steps.as_deref(), || run_check(args))
}

/// `run()`, with the steps of every walk written to `path` when one is given
/// (`--walk-steps`).
pub(crate) fn recording_steps(
    path: Option<&Path>,
    run: impl FnOnce() -> Result<ExitCode>,
) -> Result<ExitCode> {
    let Some(path) = path else {
        return run();
    };
    heki_ir::walk::record_steps();
    let code = run()?;
    let steps = serde_json::to_string_pretty(&heki_ir::walk::recorded_steps())?;
    std::fs::write(path, steps + "\n").map_err(|e| failure::unwritable(path, &e))?;
    Ok(code)
}

fn run_check(args: &CheckArgs) -> Result<ExitCode> {
    let styles = stdout_styles(args.color);
    let paths = command_paths(args)?;
    if args.watch {
        return crate::watch::run(args, &styles);
    }
    if args.diff {
        let run = fix_in_memory(args, &paths)?;
        if let Some(touched) = &run.touched {
            touched.warn(args.output_format);
        }
        for f in &run.files {
            print!(
                "{}",
                unified_diff(&shown_text(&f.path), &f.original, &f.fixed, &styles)
            );
        }
        eprintln!(
            "Would fix {} issue(s) in {} file(s).",
            run.fixes,
            run.files.len()
        );
        eprint!("{}", not_touched_note(run.not_touched));
        let code = if run.fixes == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        };
        let shown = Touched::errors(run.touched.as_ref(), &run.errors, args.output_format);
        let mut errors = failure::of_files(shown);
        errors.extend(run.refused.iter().map(Problem::new));
        return finish(args.output_format, &errors, code);
    }
    let mut refused = Vec::new();
    let mut conflicts = Vec::new();
    if args.fix {
        let run = fix_in_memory(args, &paths)?;
        let (fixes, written);
        (fixes, written, conflicts) = write_fixed(&run.files)?;
        eprintln!("Fixed {fixes} issue(s) in {written} file(s).");
        eprint!("{}", not_touched_note(run.not_touched));
        refused = run.refused;
    }
    let mut analyzed = analyze(args, &paths, None)?;
    analyzed.project.errors.extend(conflicts);
    let diags = std::mem::take(&mut analyzed.diags);
    let code = report(args, &analyzed, diags, &styles)?;
    let Analyzed { project, index, .. } = analyzed;
    // The process ends here: freeing the index and the parsed files one allocation at a time
    // would only cost time.
    std::mem::forget(index);
    std::mem::forget(project);
    let refused: Vec<Problem> = refused.iter().map(Problem::new).collect();
    finish(args.output_format, &refused, code)
}

/// What the PATHS of `args` name ([`CommandPaths`]). A file names the files to report on, which
/// neither a baseline update nor a watch can take: a baseline records every report of what the
/// run covers, and `--watch` shows every report of what it analyzes.
pub(crate) fn command_paths(args: &CheckArgs) -> Result<CommandPaths> {
    let paths = CommandPaths::of(&args.paths);
    let Some(file) = paths.files.first() else {
        return Ok(paths);
    };
    let file = path_text(file);
    if args.update_baseline {
        return Err(Problem::new(format!(
            "`--update-baseline` records the reports of directories, and `{file}` is a file"
        ))
        .help("record the project with `heki check --update-baseline`, or the entries of one directory with `heki check <dir> --update-baseline`: the entries of the other files stay as they are")
        .docs(Docs::Baselines)
        .into());
    }
    if args.watch {
        return Err(Problem::new(format!(
            "`--watch` watches directories, and `{file}` is a file"
        ))
        .help("watch the project with `heki check --watch`, or run `heki server` in the editor")
        .docs(Docs::CliCheck)
        .into());
    }
    Ok(paths)
}

/// Write the fixed files whose disk text is still the text the fixes were proven on. A file
/// that changed meanwhile (an editor saved it during the analysis) is left as it is: writing
/// the fixed text would replace that save. Returns the fixes and files written and an error
/// for each file left.
fn write_fixed(files: &[FixedFile]) -> Result<(usize, usize, Vec<FileError>)> {
    let (mut fixes, mut written, mut conflicts) = (0, 0, Vec::new());
    for f in files {
        if heki_ast::read_source(&f.path).ok().as_ref() != Some(&f.original) {
            conflicts.push(FileError::changed_while_fixing(&f.path));
            continue;
        }
        heki_ast::write_source(&f.path, &f.fixed).map_err(|e| failure::unwritable(&f.path, &e))?;
        fixes += f.fixes;
        written += 1;
    }
    Ok((fixes, written, conflicts))
}

/// The Ratchet baseline a run compares its reports with: `--baseline`, else the `baseline` of
/// the configuration, relative to the project root.
pub(crate) fn baseline_path(analysis: &AnalysisArgs, env: &ProjectEnv) -> Option<PathBuf> {
    analysis
        .baseline
        .clone()
        .or_else(|| env.cfg.baseline.as_ref().map(|b| env.root.join(b)))
}

/// Every path of `diags` as the output formats print it ([`shown_path`]).
pub(crate) fn show_paths(diags: &mut [Diagnostic]) {
    for d in diags {
        d.path = shown_path(&d.path);
        for r in &mut d.related {
            r.path = shown_path(&r.path);
        }
    }
}

/// The targets of `keys` for a note: "`a, b` and `c, d`".
fn targets(keys: &[BaselineKey]) -> String {
    let mut quoted: Vec<String> = keys.iter().map(|k| format!("`{}`", k.target)).collect();
    let last = quoted.pop().unwrap_or_default();
    if quoted.is_empty() {
        last
    } else {
        format!("{} and {last}", quoted.join(", "))
    }
}

/// An entry's allowance, with `noun` after its count, and what a run found, as notes write them:
/// with the exact sites (`1 site(s) (0 exact)`) when they differ and the counts alone do not
/// tell them.
fn tallies(allowed: Tally, found: Tally, noun: &str) -> (String, String) {
    let tiers = allowed.exact() != found.exact() && (allowed.heuristic > 0 || found.heuristic > 0);
    let shown = |t: Tally, noun: &str| {
        let count = [t.count.to_string(), noun.to_owned()].join(" ");
        let count = count.trim_end();
        if tiers {
            format!("{count} ({} exact)", t.exact())
        } else {
            count.to_owned()
        }
    };
    (shown(allowed, noun), shown(found, ""))
}

/// The note on a regression: the counts of its key, or what a new group joins.
fn regression_note(regression: &Regression, unit: CountUnit) -> String {
    let Regression {
        key,
        allowed,
        found,
        joins,
    } = regression;
    if !key.is_group() {
        let (allowed, found) = tallies(*allowed, *found, unit.noun());
        return format!("note: baseline entry {key} allows {allowed}, found {found}");
    }
    let outside = key.members_outside(joins).join(", ");
    let how = match &joins[..] {
        [] => "no baseline entry holds any of its modules".to_owned(),
        [_] => format!("`{outside}` joined the baseline group {}", targets(joins)),
        _ if outside.is_empty() => format!("it joins the baseline groups {}", targets(joins)),
        _ => format!(
            "it joins the baseline groups {} with `{outside}`",
            targets(joins)
        ),
    };
    format!("note: {key} is a new group: {how}")
}

/// The note of a run compared with a baseline written before [`BASELINE_VERSION`]: what its
/// entries allow that the current format does not, until `--update-baseline` rewrites it.
fn outdated_baseline_note(path: &Path, version: BaselineVersion, fmt: OutputFormat) {
    let path = path.display();
    match version {
        BaselineVersion::V1 => log!(
            fmt,
            "note: baseline {path} counts diagnostics, not violation sites (written before version {}); `--update-baseline` rewrites it to count sites",
            BaselineVersion::V2.number()
        ),
        BaselineVersion::V2 => log!(
            fmt,
            "note: baseline {path} does not record which sites were heuristic (written before version {BASELINE_VERSION}), so a recorded heuristic report that is exact now is no regression; `--update-baseline` rewrites it to record them"
        ),
        BaselineVersion::V3 => {}
    }
}

/// Baseline handling, rendering, summary, and exit code of the reports `diags` of `analyzed`.
fn report(
    args: &CheckArgs,
    analyzed: &Analyzed,
    mut diags: Vec<Diagnostic>,
    styles: &Stylesheet,
) -> Result<ExitCode> {
    let project = &analyzed.project;
    let key_path = baseline_key(&project.files);
    let baseline_path = baseline_path(&args.analysis, &project.env);
    let fmt = args.output_format;
    let touched = analyzed.touched.as_ref();
    if let Some(touched) = touched {
        touched.warn(fmt);
    }
    if args.update_baseline && baseline_path.is_none() {
        return Err(Problem::new("`--update-baseline` needs a baseline file")
            .help("name it with `--baseline <path>`, or set `baseline` in the configuration")
            .docs(Docs::Baselines)
            .into());
    }
    // `--update-baseline` records the current findings into a missing baseline, and otherwise
    // only lets it shrink: it compares like a check first.
    let recorded = match &baseline_path {
        Some(bp) if !args.update_baseline || bp.exists() => {
            Some(RatchetBaseline::load(bp, heki_rules::reports_groups)?)
        }
        _ => None,
    };
    let current = baseline_counts(&diags, &key_path, BaselineVersion::V3);
    let found = diags.len();
    let mut regressions = 0;
    // The update the run was asked for did not happen: it fails whatever the reports' severity.
    let mut refused_update = false;
    // A baseline entry matched no report (`lint.report-unused-suppressions`): an error.
    let mut unused_entries = false;
    // A run over part of the project knows nothing of the other files' reports: it leaves their
    // entries as they are.
    let covered = project.covers_entry();
    if let (Some(bp), Some(baseline)) = (&baseline_path, &recorded) {
        // An entry counts every report of its key, so the baseline compares all of them; a
        // `--touched` run judges only the entries in view. Every report is on a file the run
        // covers. An entry is judged by a run covering its file: one of a group, of no one
        // file, by a run over the whole project, whose root its empty path names.
        let in_view = touched.map(|t| t.baseline_view(&diags, &key_path, baseline.version));
        let shown = |key: &BaselineKey, within: &[BaselineKey]| {
            in_view.as_ref().is_none_or(|v| v.shows(key, within))
        };
        let unit = baseline.version.unit();
        if !args.update_baseline {
            outdated_baseline_note(bp, baseline.version, fmt);
        }
        // Legacy entries compare in their own unit, before `--update-baseline` rewrites them.
        let cmp = baseline.keep_regressions(&mut diags, &key_path);
        if !args.update_baseline {
            let run = analyzed.rules_run();
            let checked = project.files.iter().map(|f| f.rel.as_str()).collect();
            let withheld = baseline_counts(&analyzed.withheld, &key_path, baseline.version);
            // A report withheld because `lint.heuristic` is off still matches its entry.
            let judged =
                (cmp.stale_entries.iter()).filter(|e| covered(&e.key) && shown(&e.key, &e.within));
            for StaleEntry {
                key,
                allowed,
                found,
                within,
            } in judged
            {
                let (files, _) = project.entry_files(key);
                let matched = (*found + key.found_in(&withheld)).count;
                if run.unused_entry(key, matched, &files, &checked) {
                    log!(
                        fmt,
                        "error: baseline entry {key} matched no report (`--update-baseline` removes it)"
                    );
                    unused_entries = true;
                    continue;
                }
                if within.is_empty() {
                    let (allowed, found) = tallies(*allowed, *found, "");
                    log!(
                        fmt,
                        "note: baseline entry {key} can shrink {allowed} -> {found} (`--update-baseline`)"
                    );
                } else {
                    log!(
                        fmt,
                        "note: baseline entry {key} can shrink to {} (`--update-baseline`)",
                        targets(within)
                    );
                }
            }
        }
        for regression in cmp.regressions.iter().filter(|r| shown(&r.key, &[])) {
            log!(fmt, "{}", regression_note(regression, unit));
        }
        regressions = cmp.regressions.len();
    }
    if let Some(bp) = baseline_path.as_ref().filter(|_| args.update_baseline) {
        if !project.errors.is_empty() {
            failure::print(fmt, &not_written("baseline", bp, project.errors.len()));
            return finish(fmt, &failure::of_files(&project.errors), ExitCode::SUCCESS);
        }
        // A run decides the entries whose files it covers and keeps the others. A group it
        // reports that a kept entry holds is that entry's: the entry may hold modules the run
        // does not see, so it stays and the group is not counted beside it.
        let kept: Vec<(&BaselineKey, &Tally)> = recorded
            .iter()
            .flat_map(|b| &b.counts)
            .filter(|(key, _)| !covered(key))
            .collect();
        // Kept entries of version 2 stay as they are: an entry that records no heuristic site
        // allows as many exact ones as it had sites, as their version did.
        let legacy_kept = !kept.is_empty()
            && recorded
                .as_ref()
                .is_some_and(|b| b.version.unit() == CountUnit::Diagnostics);
        if regressions > 0 {
            let refused = Problem::new(format!(
                "baseline `{}` not updated: {regressions} entry(ies) grew",
                bp.display()
            ))
            .help("a Ratchet baseline only shrinks: fix the regressions reported above, or delete the baseline to record the current findings")
            .docs(Docs::Baselines);
            failure::print(fmt, &refused);
            refused_update = true;
        } else if legacy_kept {
            // Its other entries count diagnostics, and site counts beside them would mix units.
            let refused = Problem::new(format!(
                "baseline `{}` not updated: it counts diagnostics (written before version {}), and this run covers part of the project",
                bp.display(),
                BaselineVersion::V2.number()
            ))
            .help("update it once from the project root with `heki check --update-baseline`, which rewrites it to count sites")
            .docs(Docs::Baselines);
            failure::print(fmt, &refused);
            refused_update = true;
        } else {
            let mut counts = current;
            // A run records every group it reports, and the entries of the files it covers.
            counts.retain(|key, _| {
                if key.is_group() {
                    return !kept.iter().any(|(entry, _)| entry.holds(key));
                }
                covered(key)
            });
            counts.extend(kept.into_iter().map(|(key, count)| (key.clone(), *count)));
            let baseline = RatchetBaseline::from_counts(counts);
            baseline.save(bp)?;
            log!(
                fmt,
                "Wrote Ratchet baseline {} ({} entries, {found} diagnostic(s)).",
                bp.display(),
                baseline.counts.len(),
            );
            return finish(fmt, &failure::of_files(&project.errors), ExitCode::SUCCESS);
        }
    }

    let unmatched_entries = report_unmatched_entries(args, analyzed)?;
    note_imports_left_out(project, fmt)?;
    let errors = Touched::errors(touched, &project.errors, fmt);
    if let Some(touched) = touched {
        touched.keep(&mut diags);
    }
    // Suppressions and the baseline saw every report; only output folds them, among the ones
    // it shows.
    fold_root_causes(&mut diags);
    // Every format prints the paths `--diff` prints.
    show_paths(&mut diags);
    let by_shown: FxHashMap<PathBuf, (&ParsedFile, LineIndex)> = project
        .files
        .iter()
        .map(|p| (shown_path(&p.path), (p, LineIndex::from_source(&p.source))))
        .collect();
    // A file that did not parse is read again for the line of its syntax error.
    let source_line = |p: &Path, line: u32| match by_shown.get(p) {
        Some((f, lines)) => lines.line_text(&f.source, line).map(str::to_owned),
        None => heki_diagnostics::file_line(p, line),
    };
    let failures = shown_failures(errors.iter().copied());
    print!(
        "{}",
        render(
            &diags,
            &failures,
            format_of(args.output_format),
            styles,
            &source_line
        )
    );
    if matches!(
        args.output_format,
        OutputFormat::Full | OutputFormat::Concise
    ) {
        summary(
            &diags,
            errors.len(),
            refused_update || unused_entries || unmatched_entries,
            project.files.len(),
            baseline_path.is_some(),
            touched.map(Touched::file_count),
        );
        not_failing_hint(
            &diags,
            fail_on(args, &project.env.cfg.lint),
            baseline_path.is_some(),
        );
        heuristic_hint(
            analyzed.heuristic_off,
            Touched::shown(touched, &analyzed.withheld),
        );
        // A file given as a path selects what is shown, as `--touched` does.
        if diags.is_empty()
            && errors.is_empty()
            && touched.is_none()
            && baseline_path.is_none()
            && !project.env.cfg.declares_contracts()
        {
            eprintln!("{}", heki_rules::RECOMMEND_HINT);
        }
    }
    if matches!(args.output_format, OutputFormat::Full) {
        rule_pages(&diags);
    }
    let code = if refused_update
        || unused_entries
        || unmatched_entries
        || any_failing(&diags, fail_on(args, &project.env.cfg.lint))
    {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    };
    finish(args.output_format, &failure::of_files(errors), code)
}

/// Print the contract entries that match nothing in the analyzed files, as
/// `lint.unmatched-contract-entries` says; whether they make the run fail (as errors).
fn report_unmatched_entries(args: &CheckArgs, analyzed: &Analyzed) -> Result<bool> {
    let env = &analyzed.project.env;
    let configured = env.cfg.lint.unmatched_contract_entries;
    let severity = match configured.unwrap_or_default() {
        UnmatchedEntries::Ignore => return Ok(false),
        UnmatchedEntries::Warning => Severity::Warning,
        UnmatchedEntries::Error => Severity::Error,
    };
    // Without the key, only a run over the whole project judges: an entry naming code in a file
    // the paths leave out would look unmatched.
    if configured.is_none() {
        let limit = recommend::MAX_CENSUS_ENTRIES;
        let (left_out, _) = paths_leave_out(&analyzed.project, 1, limit)?;
        if !left_out.is_empty() {
            return Ok(false);
        }
    }
    let entries = heki_rules::unmatched_entries(env, &analyzed.index, &analyzed.selection)?;
    for entry in &entries {
        log!(
            args.output_format,
            "{}: {}",
            severity.word(),
            entry.message()
        );
    }
    Ok(severity == Severity::Error && !entries.is_empty())
}

/// Note the project modules the analyzed files import and the paths of the run leave out
/// ([`imports_left_out`]): reports that need their code are missing, and an exact report may
/// not hold for the project.
fn note_imports_left_out(project: &Project, format: OutputFormat) -> Result<()> {
    let left_out = imports_left_out(project, recommend::MAX_CENSUS_ENTRIES)?;
    if left_out.is_empty() {
        return Ok(());
    }
    let root = &project.env.root;
    let mut modules: Vec<String> = left_out
        .iter()
        .take(recommend::MAX_EXAMPLES)
        .map(|m| format!("`{}` ({})", m.module, rel_posix(&m.path, root)))
        .collect();
    if left_out.len() > recommend::MAX_EXAMPLES {
        modules.push("...".to_owned());
    }
    log!(
        format,
        "note: the analyzed files import {} project module(s) that PATHS leave out: {}; reports that need their code are missing: check the project root `{}`, with `--touched` for the files to report on",
        left_out.len(),
        modules.join(", "),
        shown_root(root)
    );
    Ok(())
}

/// [`shown_dir`] of the project root, written `..` for each level the working directory is
/// below it.
fn shown_root(root: &Path) -> String {
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|d| canonical(&d).ok());
    let depth = cwd
        .as_deref()
        .and_then(|cwd| cwd.strip_prefix(root).ok())
        .map(|below| below.components().count());
    match depth {
        Some(depth) if depth > 0 => vec![".."; depth].join("/"),
        _ => shown_dir(root),
    }
}

/// `heki snapshot [PATHS...] [--write]`: print or save the normalized summaries (`MUT103`).
pub(crate) fn run_snapshot(args: &SnapshotArgs) -> Result<ExitCode> {
    // `heki snapshot summary --write` (ARCHITECTURE.md spelling) is accepted as an alias.
    let paths: Vec<PathBuf> = args
        .paths
        .iter()
        .enumerate()
        .filter(|(i, p)| !(*i == 0 && p.as_os_str() == "summary" && !p.exists()))
        .map(|(_, p)| p.clone())
        .collect();
    let paths = CommandPaths::of(&paths);
    let project = Project::load(&ProjectOptions {
        paths: &paths.analyzed,
        config: args.config.as_deref(),
        working_dir: None,
        target_version: args.target_version.as_deref(),
        overrides: None,
        removed: None,
    })?;
    warn_files_not_analyzed(&paths, &project, OutputFormat::Full);
    let index = project.index(IndexFacts::snapshot());
    let units = project.units();
    let snap = SummarySnapshot::build(&units, &index);
    if args.write {
        let path = project.env.root.join(SNAPSHOT_FILE);
        if !project.errors.is_empty() {
            failure::print(
                OutputFormat::Full,
                &not_written("summary snapshot", &path, project.errors.len()),
            );
            return finish(
                OutputFormat::Full,
                &failure::of_files(&project.errors),
                ExitCode::SUCCESS,
            );
        }
        snap.save(&path)?;
        eprintln!(
            "Wrote summary snapshot {} ({} function(s)).",
            path.display(),
            snap.functions.len()
        );
    } else {
        print!("{}", snap.to_json());
    }
    finish(
        OutputFormat::Full,
        &failure::of_files(&project.errors),
        ExitCode::SUCCESS,
    )
}

/// Why a run that could not analyze every file keeps the `what` at `path` as it is: written
/// from the other files, it would drop the entries of the failed ones, and a later run would
/// compare them against nothing.
fn not_written(what: &str, path: &Path, failed: usize) -> Problem {
    let docs = if what == "baseline" {
        Docs::Baselines
    } else {
        Docs::Snapshots
    };
    Problem::new(format!(
        "{what} `{}` not written: {failed} file(s) could not be analyzed",
        path.display()
    ))
    .help("fix the files listed below, or exclude them with `extend-exclude`, then run again")
    .docs(docs)
}

/// The last line of a text run, with the number of file errors it reports and whether an error
/// it printed above fails it (a contract entry that matches nothing, a baseline entry that
/// matches no report, a refused baseline update); `touched` is the number of analyzed files
/// `--touched` names. Only a run that passes says that all checks passed.
fn summary(
    diags: &[Diagnostic],
    errors: usize,
    failed: bool,
    files: usize,
    baseline: bool,
    touched: Option<usize>,
) {
    let what = if baseline {
        "Ratchet regression(s)"
    } else {
        "diagnostic(s)"
    };
    let found = diags.len();
    let scope = match touched {
        None => format!("in {files} file(s)"),
        Some(touched) => format!("for {touched} touched file(s) ({files} file(s) analyzed)"),
    };
    let line = match (found, errors, failed, touched) {
        (0, 0, false, None) => format!("All checks passed ({files} file(s))."),
        (0, 0, false, Some(_)) => format!("All checks passed {scope}."),
        (0, 0, true, _) => format!("Found 0 {what} {scope}; the errors above fail the run."),
        (found, 0, _, _) => format!("Found {found} {what} {scope}."),
        (found, errors, _, _) => {
            format!("Found {found} {what} {scope}; {errors} file(s) could not be analyzed.")
        }
    };
    eprintln!("{line}");
}

/// After the summary of the full text format: where each reported rule is explained.
pub(crate) fn rule_pages(diags: &[Diagnostic]) {
    let rules: BTreeMap<&str, &str> = diags
        .iter()
        .map(|d| (d.rule_code.as_str(), d.rule_slug.as_str()))
        .collect();
    for (code, slug) in rules {
        let page = heki_env::problem::rule_page(slug);
        eprintln!("{code} ({slug}): `heki rule {code}`, {page}");
    }
}

/// After the summary of a text format: the reports it shows that do not fail the run at `fail_on`
/// ([`heki_rules::not_failing_note`]); `baseline` when the reports shown are Ratchet regressions.
fn not_failing_hint(diags: &[Diagnostic], fail_on: Severity, baseline: bool) {
    let passing = diags.iter().filter(|d| !d.fails_run(fail_on)).count();
    if fail_on == Severity::Error && passing > 0 {
        let which = if baseline {
            "regression(s)"
        } else {
            "report(s) shown now"
        };
        eprintln!("{}", heki_rules::not_failing_note(passing, which));
    }
}

/// After the summary of a text format: what `lint.heuristic` would add ([`heki_rules::heuristic_hint`]).
pub(crate) fn heuristic_hint(rules_off: usize, reports_withheld: usize) {
    if let Some(note) = heki_rules::heuristic_hint(rules_off, reports_withheld) {
        eprintln!("{note}");
    }
}

/// Write one line to stderr. GitHub Actions reads every line of a job's log for workflow
/// commands (`::error file=...::`): in `github` output CR and LF are escaped as in annotations,
/// so text from the project (a file name, a baseline entry, an error message) never starts a
/// line of its own.
pub(crate) fn log_line(format: OutputFormat, line: &str) {
    if matches!(format, OutputFormat::Github) {
        eprintln!("{}", github_escape(line));
    } else {
        eprintln!("{line}");
    }
}

/// Syntax / read errors make the run fail with exit code 2 after the report.
pub(crate) fn finish(format: OutputFormat, errors: &[Problem], code: ExitCode) -> Result<ExitCode> {
    for e in errors {
        failure::print(format, e);
    }
    Ok(if errors.is_empty() {
        code
    } else {
        ExitCode::from(2)
    })
}

/// The styles of what `heki check` prints to stdout, colored when Ruff colors: `--color always`
/// and `never` decide alone. Otherwise `FORCE_COLOR` (not empty) and `CLICOLOR_FORCE` (not `0`)
/// turn color on, then `NO_COLOR` (set to anything) and `CLICOLOR=0` turn it off, and stdout is
/// colored when it is a terminal.
pub(crate) fn stdout_styles(choice: ColorChoice) -> Stylesheet {
    let var = std::env::var_os;
    let colored = match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            var("FORCE_COLOR").is_some_and(|v| !v.is_empty())
                || var("CLICOLOR_FORCE").is_some_and(|v| v != "0")
                || (var("NO_COLOR").is_none()
                    && var("CLICOLOR").is_none_or(|v| v != "0")
                    && std::io::stdout().is_terminal())
        }
    };
    if !colored {
        return Stylesheet::plain();
    }
    // A Windows console shows escape codes as text until virtual terminal processing is on. As
    // in Ruff, a stdout that is not a console (color forced into a file or a pipe) is colored
    // although it cannot be switched on.
    let _ = anstyle_query::windows::enable_ansi_colors();
    Stylesheet::styled()
}

pub(crate) fn format_of(f: OutputFormat) -> heki_diagnostics::OutputFormat {
    match f {
        OutputFormat::Full => heki_diagnostics::OutputFormat::Full,
        OutputFormat::Concise => heki_diagnostics::OutputFormat::Concise,
        OutputFormat::Json => heki_diagnostics::OutputFormat::Json,
        OutputFormat::Github => heki_diagnostics::OutputFormat::Github,
        OutputFormat::Sarif => heki_diagnostics::OutputFormat::Sarif,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A report at `path` with a fix, and a hop of its call chain at each of `related`.
    fn fixable(path: &str, related: &[&str]) -> Diagnostic {
        let span = heki_ast::Span { start: 0, end: 1 };
        Diagnostic {
            path: PathBuf::from(path),
            line: 1,
            column: 1,
            span,
            qualname: "f".to_owned(),
            rule_code: "OPT101".to_owned(),
            rule_slug: "list-quadratic-concat".to_owned(),
            target: "x".to_owned(),
            severity: Severity::Warning,
            certainty: heki_diagnostics::Certainty::Exact,
            message: String::new(),
            help: None,
            evidence: None,
            blocker: None,
            related: related
                .iter()
                .map(|p| heki_diagnostics::RelatedLocation {
                    path: PathBuf::from(p),
                    line: 1,
                    column: 1,
                    span,
                    message: String::new(),
                })
                .collect(),
            fix: Some(vec![TextEdit {
                span,
                replacement: "y".to_owned(),
            }]),
            sites: 1,
            fold_key: None,
            open_records: Vec::new(),
            contracts: Vec::new(),
            allowed_examples: Vec::new(),
            group: false,
            key_target: None,
        }
    }

    #[test]
    fn a_run_over_touched_files_fixes_only_them() {
        // No rule's fixable report has a hop in another file yet, so the binary cannot show one.
        let diags = [
            fixable("a.py", &[]),
            fixable("caller.py", &["a.py"]),
            fixable("other.py", &[]),
        ];
        let touched = Touched {
            files: FxHashSet::from_iter([PathBuf::from("a.py")]),
            baseline_paths: FxHashSet::default(),
            unknown: Vec::new(),
            unknown_files: Vec::new(),
        };
        let all = |_: &Diagnostic| true;
        let (by_path, not_touched) = fixes_to_apply(&diags, Some(&touched), &all);
        assert_eq!(
            by_path.keys().copied().collect::<Vec<_>>(),
            [Path::new("a.py")]
        );
        assert_eq!(not_touched, 1);
        let (by_path, not_touched) = fixes_to_apply(&diags, None, &all);
        assert_eq!(by_path.len(), 3);
        assert_eq!(not_touched, 0);
        let (by_path, _) = fixes_to_apply(&diags, None, &|d: &Diagnostic| d.related.is_empty());
        assert_eq!(by_path.len(), 2);
        assert_eq!(
            not_touched_note(1),
            "note: 1 fix(es) in files outside the touched ones not applied; name the files to fix them\n"
        );
        assert_eq!(not_touched_note(0), "");
    }

    #[test]
    fn fixes_are_not_written_over_a_save_made_while_fixing() {
        let dir = std::env::temp_dir().join(format!("heki_write_fixed_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (kept, fixed) = (dir.join("kept.py"), dir.join("fixed.py"));
        let file = |path: &Path, original: &str| FixedFile {
            path: path.to_path_buf(),
            original: original.to_owned(),
            fixed: "x.append(i)\n".to_owned(),
            fixes: 1,
        };
        // After the fixes were proven on `x = x + [i]`, an editor saved `kept.py` (a race a
        // process cannot set up portably).
        std::fs::write(&kept, "x = x + [i]  # saved\n").unwrap();
        std::fs::write(&fixed, "x = x + [i]\n").unwrap();
        let files = [file(&kept, "x = x + [i]\n"), file(&fixed, "x = x + [i]\n")];
        let (fixes, written, conflicts) = write_fixed(&files).unwrap();
        assert_eq!((fixes, written), (1, 1));
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert_eq!(conflicts[0].path, kept);
        let problem = heki_env::Explain::explain(&conflicts[0]);
        assert!(
            problem.message.starts_with("changed on disk"),
            "{problem:?}"
        );
        assert_eq!(problem.help, ["run `heki check --fix` again"]);
        assert_eq!(
            std::fs::read_to_string(&kept).unwrap(),
            "x = x + [i]  # saved\n"
        );
        assert_eq!(std::fs::read_to_string(&fixed).unwrap(), "x.append(i)\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
