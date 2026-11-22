//! `heki check`: discover -> parse / IR (parallel) -> effect summaries -> rules -> inline
//! suppressions -> fixes -> Ratchet baseline -> output. Exit codes: 0 nothing that fails the run,
//! 1 a report at the `fail-on` severity (an error by default; with a baseline, only regressions
//! are reported), a baseline update refused, or pending `--diff` changes, 2 configuration / I/O /
//! unknown rule / syntax errors.

use crate::{AnalysisArgs, CheckArgs, ColorChoice, OutputFormat, SnapshotArgs};
use anyhow::{Context, Result, bail};
use heki_ast::LineIndex;
use heki_diagnostics::{
    BASELINE_VERSION, CountUnit, Diagnostic, FileFailure, RatchetBaseline, Stylesheet,
    baseline_counts, fix_python_source, github_escape, path_text, render, unified_diff,
};
use heki_effect::{OptInFacts, ProjectIndex, TypedWrites, analyze_project_for, func_qualname};
use heki_env::{LintConfig, Severity};
use heki_profile::{HotFunctions, ProfileData, ProfiledFunction};
use heki_rules::project::{
    FileError, ParsedFile, Project, ProjectEnv, ProjectOptions, canonical, retain_unsuppressed,
};
use heki_rules::{
    CheckExtras, ModuleUnit, RuleSelection, SNAPSHOT_FILE, Selectors, SummarySnapshot,
    check_project, find_rule, fold_root_causes,
};
use rustc_hash::FxHashMap;
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
    }
}

/// Rule selection of a run (the command line over the config), with `lint.severity` keys
/// validated.
fn selection(args: &AnalysisArgs, project: &Project) -> Result<RuleSelection> {
    let cfg = &project.env.cfg;
    let selection = RuleSelection::resolve(&cfg.lint, &selectors(args))?;
    for key in cfg.lint.severity.keys() {
        if find_rule(key).is_none() {
            bail!("unknown rule `{key}` in lint.severity");
        }
    }
    Ok(selection)
}

/// `--fail-on` over `lint.fail-on`: the least severity whose reports make the run fail.
pub(crate) fn fail_on(args: &CheckArgs, lint: &LintConfig) -> Severity {
    args.fail_on
        .map(Severity::from)
        .or(lint.fail_on)
        .unwrap_or(Severity::Error)
}

/// Whether one of the reports `diags` makes the run fail (exit code 1) at `fail_on`.
pub(crate) fn any_failing(diags: &[Diagnostic], fail_on: Severity) -> bool {
    diags
        .iter()
        .any(|d| d.severity == Severity::Error || fail_on == Severity::Warning)
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

/// Every function of the project with the lines of its definition, for attributing profile
/// frames.
fn profiled_functions(project: &Project) -> Vec<ProfiledFunction<'_>> {
    let mut out = Vec::new();
    for file in &project.files {
        let lines = LineIndex::from_source(&file.source);
        out.extend(file.ir.functions.iter().map(|f| ProfiledFunction {
            path: &file.path,
            qualname: func_qualname(f),
            lines: Some(lines.line(f.span.start)..=lines.line(f.span.end)),
        }));
    }
    out
}

/// Append `profile: ...` evidence to `OPT` diagnostics of profiled functions.
pub(crate) fn annotate_hot(diags: &mut [Diagnostic], hot: Option<&HotFunctions>) {
    let Some(hot) = hot else {
        return;
    };
    for d in diags.iter_mut().filter(|d| d.rule_code.starts_with("OPT")) {
        if let Some(pct) = hot.cum_pct(&d.path, &d.qualname) {
            let note = format!(
                "profile: `{}` is on the stack in {pct:.1}% of samples",
                d.qualname
            );
            let parts: Vec<String> = d.evidence.take().into_iter().chain([note]).collect();
            d.evidence = Some(parts.join("; "));
        }
    }
}

/// A loaded project with the analysis `heki check` runs on it: the rule selection, the saved
/// summary snapshot `MUT103` compares with, and the index built with the configuration's
/// contracts and the selection's typed-write mode.
pub(crate) struct Indexed {
    pub project: Project,
    pub selection: RuleSelection,
    pub snapshot: Option<SummarySnapshot>,
    pub index: ProjectIndex,
}

/// Load the project at `paths` (with `overrides` in place of files' text) and build the index
/// `heki check` builds for `analysis`. `format` is the output format the run's warnings follow.
/// `inspect`: the index also holds the facts `heki inspect` reads besides the rules
/// ([`heki_rules::inspect_facts`]), which no rule reads.
pub(crate) fn load_and_index(
    paths: &[PathBuf],
    analysis: &AnalysisArgs,
    format: OutputFormat,
    overrides: Option<&FxHashMap<PathBuf, String>>,
    inspect: bool,
) -> Result<Indexed> {
    let project = Project::load(&ProjectOptions {
        paths,
        config: analysis.config.as_deref(),
        working_dir: None,
        target_version: analysis.target_version.as_deref(),
        overrides,
        removed: None,
    })?;
    let selection = selection(analysis, &project)?;
    let snapshot = snapshot_for(&selection, &project.env.root, format)?;
    let env = &project.env;
    let contracts = heki_rules::summary_contracts(&env.cfg, &env.kb)?;
    let typed_writes = heki_rules::typed_writes(&env.cfg, &selection);
    let opt_in = if inspect {
        heki_rules::inspect_facts(&selection)
    } else {
        heki_rules::opt_in_facts(&selection)
    };
    let inputs = project.inputs();
    let index = analyze_project_for(
        &inputs,
        &project.unread,
        &env.kb,
        contracts,
        typed_writes,
        opt_in,
    );
    drop(inputs);
    Ok(Indexed {
        project,
        selection,
        snapshot,
        index,
    })
}

impl Indexed {
    /// The diagnostics of every enabled rule in the project's files, after inline suppressions
    /// and without the heuristic reports the selection withholds, and how many those are.
    pub(crate) fn diagnose(&self) -> Result<(Vec<Diagnostic>, usize)> {
        self.diagnose_units(&self.project.units())
    }

    /// [`Self::diagnose`] in `units` (some or all of the project's files): the rules read the
    /// other files through the index only.
    pub(crate) fn diagnose_units(
        &self,
        units: &[ModuleUnit<'_>],
    ) -> Result<(Vec<Diagnostic>, usize)> {
        let env = &self.project.env;
        let decls = self.index.project_decls();
        let sources: FxHashMap<&Path, &str> = self
            .project
            .files
            .iter()
            .map(|f| (f.path.as_path(), f.source.as_str()))
            .collect();
        let text_of = |p: &Path| sources.get(p).map(|t| (*t).to_owned());
        let extras = CheckExtras {
            soundness_kb: &env.soundness_kb,
            snapshot: self.snapshot.as_ref(),
            decls: Some(&decls),
            project_patches_builtins: Some(
                self.project.files.iter().any(ParsedFile::patches_builtins),
            ),
            text_of: Some(&text_of),
        };
        let mut diags = check_project(
            units,
            &env.cfg,
            &env.kb,
            &self.index,
            &self.selection,
            &extras,
        )?;
        retain_unsuppressed(&mut diags, &self.project.files);
        let withheld = self.selection.withhold_heuristic(&mut diags);
        Ok((diags, withheld))
    }
}

/// One analysis pass: the project, its index, its diagnostics after inline suppressions, and
/// the shares of its functions in the profile.
struct Analyzed {
    project: Project,
    index: ProjectIndex,
    diags: Vec<Diagnostic>,
    hot: Option<HotFunctions>,
    /// The heuristic rules that did not run only because `lint.heuristic` is off.
    heuristic_off: usize,
    /// The heuristic reports of exact rules withheld because `lint.heuristic` is off.
    withheld: usize,
}

fn analyze(
    args: &CheckArgs,
    profile: Option<&ProfileData>,
    overrides: Option<&FxHashMap<PathBuf, String>>,
) -> Result<Analyzed> {
    let indexed = load_and_index(
        &args.paths,
        &args.analysis,
        args.output_format,
        overrides,
        false,
    )?;
    let (mut diags, withheld) = indexed.diagnose()?;
    let Indexed {
        project,
        index,
        selection,
        ..
    } = indexed;
    let hot = profile.map(|p| p.attribute(profiled_functions(&project)));
    annotate_hot(&mut diags, hot.as_ref());
    Ok(Analyzed {
        project,
        index,
        diags,
        hot,
        heuristic_off: selection.heuristic_off().len(),
        withheld,
    })
}

/// Whether `--fix` / `--diff` applies this diagnostic's fix: `safe` + `unconditional` rules
/// (`OPT101` / `OPT102`) always; `size-dependent` ones with `--include-size-dependent` or in a
/// function the profile shows hot.
fn applicable(d: &Diagnostic, args: &CheckArgs, hot: Option<&HotFunctions>) -> bool {
    d.fix.is_some()
        && find_rule(&d.rule_code).is_some_and(|meta| {
            meta.auto_fix_default
                || args.include_size_dependent
                || hot.is_some_and(|h| h.is_hot(&d.path, &d.qualname))
        })
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

/// What `--fix` / `--diff` would do.
struct FixRun {
    fixes: usize,
    files: Vec<FixedFile>,
    /// Read / syntax errors of the last pass.
    errors: Vec<FileError>,
    /// Files whose fixes would not parse, left unchanged.
    refused: Vec<String>,
}

/// Apply fixes in memory until nothing more applies.
fn fix_in_memory(args: &CheckArgs, hot: Option<&ProfileData>) -> Result<FixRun> {
    let mut overrides: FxHashMap<PathBuf, String> = FxHashMap::default();
    let mut originals: BTreeMap<PathBuf, String> = BTreeMap::new();
    let mut applied_to: FxHashMap<PathBuf, usize> = FxHashMap::default();
    let mut total = 0;
    let mut errors;
    let mut passes = 0;
    let mut refused: Vec<String> = Vec::new();
    loop {
        let a = analyze(args, hot, Some(&overrides))?;
        errors = a.project.errors;
        let mut by_path: BTreeMap<&Path, Vec<&[heki_diagnostics::TextEdit]>> = BTreeMap::new();
        for d in &a.diags {
            if applicable(d, args, a.hot.as_ref())
                && let Some(edits) = &d.fix
            {
                by_path.entry(d.path.as_path()).or_default().push(edits);
            }
        }
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
    })
}

pub(crate) fn run(args: &CheckArgs) -> Result<ExitCode> {
    if let Some(target) = args.output_format_json_schema {
        print!("{}", crate::prompt::json_schema(target));
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(question) = &args.generate_prompt {
        return crate::prompt::run(args, question);
    }
    let Some(path) = &args.walk_steps else {
        return run_check(args);
    };
    heki_ir::walk::record_steps();
    let code = run_check(args)?;
    let steps = serde_json::to_string_pretty(&heki_ir::walk::recorded_steps())?;
    std::fs::write(path, steps + "\n")
        .with_context(|| format!("cannot write `{}`", path.display()))?;
    Ok(code)
}

fn run_check(args: &CheckArgs) -> Result<ExitCode> {
    let hot = args
        .profile
        .as_deref()
        .map(heki_profile::load)
        .transpose()?;
    let styles = stdout_styles(args.color);
    if args.watch {
        return crate::watch::run(args, hot.as_ref(), &styles);
    }
    if args.diff {
        let run = fix_in_memory(args, hot.as_ref())?;
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
        let code = if run.fixes == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        };
        let mut errors = shown_errors(&run.errors);
        errors.extend(run.refused);
        return finish(args.output_format, &errors, code);
    }
    let mut refused = Vec::new();
    let mut conflicts = Vec::new();
    if args.fix {
        let run = fix_in_memory(args, hot.as_ref())?;
        let (fixes, written);
        (fixes, written, conflicts) = write_fixed(&run.files)?;
        eprintln!("Fixed {fixes} issue(s) in {written} file(s).");
        refused = run.refused;
    }
    let Analyzed {
        mut project,
        index,
        diags,
        heuristic_off,
        withheld,
        ..
    } = analyze(args, hot.as_ref(), None)?;
    project.errors.extend(conflicts);
    let code = report(args, &project, diags, heuristic_off, withheld, &styles)?;
    // The process ends here: freeing the index and the parsed files one allocation at a time
    // would only cost time.
    std::mem::forget(index);
    std::mem::forget(project);
    finish(args.output_format, &refused, code)
}

/// Write the fixed files whose disk text is still the text the fixes were proven on. A file
/// that changed meanwhile (an editor saved it during the analysis) is left as it is: writing
/// the fixed text would replace that save. Returns the fixes and files written and an error
/// for each file left.
fn write_fixed(files: &[FixedFile]) -> Result<(usize, usize, Vec<FileError>)> {
    let (mut fixes, mut written, mut conflicts) = (0, 0, Vec::new());
    for f in files {
        if heki_ast::read_source(&f.path).ok().as_ref() != Some(&f.original) {
            conflicts.push(FileError {
                path: f.path.clone(),
                position: None,
                message: "changed on disk while heki was fixing it; its fixes were not written (run `heki check --fix` again)".to_owned(),
            });
            continue;
        }
        heki_ast::write_source(&f.path, &f.fixed)
            .with_context(|| format!("failed to write `{}`", shown_text(&f.path)))?;
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

/// The path a baseline entry names for a file of `project`: relative to the project root.
pub(crate) fn baseline_key(project: &Project) -> impl Fn(&Path) -> String + '_ {
    let rel: FxHashMap<&Path, &str> = project
        .files
        .iter()
        .map(|f| (f.path.as_path(), f.rel.as_str()))
        .collect();
    move |p: &Path| rel.get(p).map_or_else(|| path_text(p), |r| (*r).to_owned())
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

/// Baseline handling, rendering, summary, and exit code of one analyzed run, whose selection
/// left `heuristic_off` rules off and withheld `withheld` reports for being heuristic.
pub(crate) fn report(
    args: &CheckArgs,
    project: &Project,
    mut diags: Vec<Diagnostic>,
    heuristic_off: usize,
    withheld: usize,
    styles: &Stylesheet,
) -> Result<ExitCode> {
    let key_path = baseline_key(project);
    let baseline_path = baseline_path(&args.analysis, &project.env);
    let fmt = args.output_format;
    if args.update_baseline && baseline_path.is_none() {
        bail!("`--update-baseline` needs `--baseline <path>` or `baseline` in the configuration");
    }
    // `--update-baseline` records the current findings into a missing baseline, and otherwise
    // only lets it shrink: it compares like a check first.
    let recorded = match &baseline_path {
        Some(bp) if !args.update_baseline || bp.exists() => Some(RatchetBaseline::load(bp)?),
        _ => None,
    };
    let current = baseline_counts(&diags, &key_path, CountUnit::Sites);
    let found = diags.len();
    let mut regressions = 0;
    // The update the run was asked for did not happen: it fails whatever the reports' severity.
    let mut refused_update = false;
    if let (Some(bp), Some(baseline)) = (&baseline_path, &recorded) {
        let unit = baseline.unit;
        if unit == CountUnit::Diagnostics && !args.update_baseline {
            log!(
                fmt,
                "note: baseline {} counts diagnostics, not violation sites (written before version {BASELINE_VERSION}); `--update-baseline` rewrites it to count sites",
                bp.display()
            );
        }
        // Legacy entries compare in their own unit, before `--update-baseline` rewrites them.
        let cmp = baseline.keep_regressions(&mut diags, &key_path);
        if !args.update_baseline {
            for (key, allowed, cur) in &cmp.stale_entries {
                log!(
                    fmt,
                    "note: baseline entry {}::{} {} `{}` can shrink {allowed} -> {cur} (`--update-baseline`)",
                    key.path,
                    key.qualname,
                    key.rule_code,
                    key.target
                );
            }
        }
        for (key, allowed, cur) in &cmp.regressions {
            log!(
                fmt,
                "note: baseline entry {}::{} {} `{}` allows {allowed} {}, found {cur}",
                key.path,
                key.qualname,
                key.rule_code,
                key.target,
                unit.noun()
            );
        }
        regressions = cmp.regressions.len();
    }
    if let Some(bp) = baseline_path.as_ref().filter(|_| args.update_baseline) {
        if !project.errors.is_empty() {
            log!(fmt, "{}", not_written("baseline", bp, project.errors.len()));
            return finish(fmt, &shown_errors(&project.errors), ExitCode::SUCCESS);
        }
        if regressions > 0 {
            log!(
                fmt,
                "error: baseline {} not updated: {regressions} entry(ies) grew, and a Ratchet baseline only shrinks (delete it to record the current findings)",
                bp.display()
            );
            refused_update = true;
        } else {
            let baseline = RatchetBaseline::from_counts(current);
            baseline.save(bp)?;
            log!(
                fmt,
                "Wrote Ratchet baseline {} ({} entries, {found} diagnostic(s)).",
                bp.display(),
                baseline.counts.len(),
            );
            return finish(fmt, &shown_errors(&project.errors), ExitCode::SUCCESS);
        }
    }

    // Suppressions and the baseline saw every report; only output folds them.
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
        None => {
            let text = heki_ast::read_source(p).ok()?;
            LineIndex::from_source(&text)
                .line_text(&text, line)
                .map(str::to_owned)
        }
    };
    let failures = shown_failures(&project.errors);
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
        summary(&diags, project.files.len(), baseline_path.is_some());
        heuristic_hint(heuristic_off, withheld);
    }
    let code = if refused_update || any_failing(&diags, fail_on(args, &project.env.cfg.lint)) {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    };
    finish(args.output_format, &shown_errors(&project.errors), code)
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
    let project = Project::load(&ProjectOptions {
        paths: &paths,
        config: args.config.as_deref(),
        working_dir: None,
        target_version: args.target_version.as_deref(),
        overrides: None,
        removed: None,
    })?;
    let inputs = project.inputs();
    let index = analyze_project_for(
        &inputs,
        &project.unread,
        &project.env.kb,
        None,
        TypedWrites::Propagated,
        OptInFacts::ALL,
    );
    let units = project.units();
    let snap = SummarySnapshot::build(&units, &index);
    if args.write {
        let path = project.env.root.join(SNAPSHOT_FILE);
        if !project.errors.is_empty() {
            eprintln!(
                "{}",
                not_written("summary snapshot", &path, project.errors.len())
            );
            return finish(
                OutputFormat::Full,
                &shown_errors(&project.errors),
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
        &shown_errors(&project.errors),
        ExitCode::SUCCESS,
    )
}

/// Why a run that could not analyze every file keeps the `what` at `path` as it is: written
/// from the other files, it would drop the entries of the failed ones, and a later run would
/// compare them against nothing.
fn not_written(what: &str, path: &Path, failed: usize) -> String {
    format!(
        "error: {what} {} not written: {failed} file(s) could not be analyzed",
        path.display()
    )
}

fn summary(diags: &[Diagnostic], files: usize, baseline: bool) {
    let what = if baseline {
        "Ratchet regression(s)"
    } else {
        "diagnostic(s)"
    };
    if diags.is_empty() {
        eprintln!("All checks passed ({files} file(s)).");
    } else {
        eprintln!("Found {} {what} in {files} file(s).", diags.len());
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
pub(crate) fn finish(format: OutputFormat, errors: &[String], code: ExitCode) -> Result<ExitCode> {
    for e in errors {
        log!(format, "error: {e}");
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
        assert!(
            conflicts[0].message.starts_with("changed on disk"),
            "{conflicts:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&kept).unwrap(),
            "x = x + [i]  # saved\n"
        );
        assert_eq!(std::fs::read_to_string(&fixed).unwrap(), "x.append(i)\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
