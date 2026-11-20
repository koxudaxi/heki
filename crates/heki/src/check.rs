//! `heki check`: discover -> parse / IR (parallel) -> effect summaries -> rules -> inline
//! suppressions -> fixes -> Ratchet baseline -> output. Exit codes: 0 clean, 1 violations (or
//! Ratchet regressions / pending `--diff`), 2 configuration / I/O / unknown rule / syntax errors.

use crate::{CheckArgs, OutputFormat, SnapshotArgs};
use anyhow::{Context, Result, bail};
use heki_ast::LineIndex;
use heki_diagnostics::{
    BASELINE_VERSION, CountUnit, Diagnostic, RatchetBaseline, apply_fixes, baseline_counts,
    github_escape, path_text, render, unified_diff,
};
use heki_effect::{analyze_project, analyze_project_with};
use heki_profile::ProfileData;
use heki_rules::project::{FileError, ParsedFile, Project, ProjectOptions, retain_unsuppressed};
use heki_rules::{
    CheckExtras, RuleSelection, SNAPSHOT_FILE, SummarySnapshot, check_project, find_rule,
    fold_root_causes,
};
use rustc_hash::FxHashMap;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Rule selection of a run (`--select` / `--extend-select` / `--ignore` over the config), with
/// `lint.severity` keys validated.
pub(crate) fn selection(args: &CheckArgs, project: &Project) -> Result<RuleSelection> {
    let cfg = &project.env.cfg;
    let selection =
        RuleSelection::resolve(&cfg.lint, &args.select, &args.extend_select, &args.ignore)?;
    for key in cfg.lint.severity.keys() {
        if find_rule(key).is_none() {
            bail!("unknown rule `{key}` in lint.severity");
        }
    }
    Ok(selection)
}

/// The saved summary snapshot when `MUT103` is enabled (`None` with a warning if missing).
pub(crate) fn snapshot_for(
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

/// Append `profile: ...` evidence to `OPT` diagnostics of profiled functions.
pub(crate) fn annotate_hot(diags: &mut [Diagnostic], hot: Option<&ProfileData>) {
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

/// One analysis pass: the project and its diagnostics after inline suppressions.
struct Analyzed {
    project: Project,
    diags: Vec<Diagnostic>,
}

fn analyze(
    args: &CheckArgs,
    hot: Option<&ProfileData>,
    overrides: Option<&FxHashMap<PathBuf, String>>,
) -> Result<Analyzed> {
    let project = Project::load(&ProjectOptions {
        paths: &args.paths,
        config: args.config.as_deref(),
        target_version: args.target_version.as_deref(),
        overrides,
    })?;
    let selection = selection(args, &project)?;
    let snapshot = snapshot_for(&selection, &project.env.root, args.output_format)?;
    let env = &project.env;
    let inputs = project.inputs();
    let contracts = heki_rules::summary_contracts(&env.cfg)?;
    let index = analyze_project_with(&inputs, &env.kb, contracts);
    let units = project.units();
    let extras = CheckExtras {
        soundness_kb: &env.soundness_kb,
        snapshot: snapshot.as_ref(),
        decls: None,
        project_patches_builtins: None,
        text_of: None,
    };
    let mut diags = check_project(&units, &env.cfg, &env.kb, &index, &selection, &extras)?;
    drop(units);
    drop(inputs);
    retain_unsuppressed(&mut diags, &project.files);
    annotate_hot(&mut diags, hot);
    Ok(Analyzed { project, diags })
}

/// Whether `--fix` / `--diff` applies this diagnostic's fix: `safe` + `unconditional` rules
/// (`OPT101` / `OPT102`) always; `size-dependent` ones with `--include-size-dependent` or in a
/// function the profile shows hot.
fn applicable(d: &Diagnostic, args: &CheckArgs, hot: Option<&ProfileData>) -> bool {
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

/// How `heki check` prints a path, in every output format and in `--diff` headers (as Ruff
/// does): relative to the working directory when it is inside it (also through a symbolic
/// link), otherwise as given.
pub(crate) fn shown_path(p: &Path) -> PathBuf {
    if p.is_relative() {
        return p.to_path_buf();
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    p.strip_prefix(&cwd)
        .ok()
        .map(Path::to_path_buf)
        .or_else(|| {
            let abs = std::fs::canonicalize(p).ok()?;
            let rel = abs.strip_prefix(std::fs::canonicalize(&cwd).ok()?).ok()?;
            Some(rel.to_path_buf())
        })
        .unwrap_or_else(|| p.to_path_buf())
}

/// [`shown_path`] as reports write it.
fn shown_text(p: &Path) -> String {
    path_text(&shown_path(p))
}

/// Apply fixes in memory until nothing more applies. Returns the number of applied fixes,
/// the changed files, and the read / syntax errors of the last pass.
fn fix_in_memory(
    args: &CheckArgs,
    hot: Option<&ProfileData>,
) -> Result<(usize, Vec<FixedFile>, Vec<FileError>)> {
    let mut overrides: FxHashMap<PathBuf, String> = FxHashMap::default();
    let mut originals: BTreeMap<PathBuf, String> = BTreeMap::new();
    let mut applied_to: FxHashMap<PathBuf, usize> = FxHashMap::default();
    let mut total = 0;
    let mut errors;
    let mut passes = 0;
    loop {
        let a = analyze(args, hot, Some(&overrides))?;
        errors = a.project.errors;
        let mut by_path: BTreeMap<&Path, Vec<&[heki_diagnostics::TextEdit]>> = BTreeMap::new();
        for d in &a.diags {
            if applicable(d, args, hot)
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
            let (new, applied) = apply_fixes(&file.source, groups);
            pass += applied.len();
            *applied_to.entry(file.path.clone()).or_default() += applied.len();
            originals
                .entry(file.path.clone())
                .or_insert_with(|| file.source.clone());
            overrides.insert(file.path.clone(), new);
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
    Ok((total, files, errors))
}

pub(crate) fn run(args: &CheckArgs) -> Result<ExitCode> {
    let hot = args
        .profile
        .as_deref()
        .map(heki_profile::load)
        .transpose()?;
    if args.watch {
        return crate::watch::run(args, hot.as_ref());
    }
    if args.diff {
        let (fixes, files, errors) = fix_in_memory(args, hot.as_ref())?;
        for f in &files {
            print!(
                "{}",
                unified_diff(&shown_text(&f.path), &f.original, &f.fixed)
            );
        }
        eprintln!("Would fix {fixes} issue(s) in {} file(s).", files.len());
        let code = if fixes == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        };
        return finish(args.output_format, &errors, code);
    }
    let mut conflicts = Vec::new();
    if args.fix {
        let (_, files, _) = fix_in_memory(args, hot.as_ref())?;
        let (fixes, written);
        (fixes, written, conflicts) = write_fixed(&files)?;
        eprintln!("Fixed {fixes} issue(s) in {written} file(s).");
    }
    let Analyzed { mut project, diags } = analyze(args, hot.as_ref(), None)?;
    project.errors.extend(conflicts);
    report(args, &project, diags)
}

/// Write the fixed files whose disk text is still the text the fixes were proven on. A file
/// that changed meanwhile (an editor saved it during the analysis) is left as it is: writing
/// the fixed text would replace that save. Returns the fixes and files written and an error
/// for each file left.
fn write_fixed(files: &[FixedFile]) -> Result<(usize, usize, Vec<FileError>)> {
    let (mut fixes, mut written, mut conflicts) = (0, 0, Vec::new());
    for f in files {
        if std::fs::read_to_string(&f.path).ok().as_ref() != Some(&f.original) {
            conflicts.push(FileError {
                path: f.path.clone(),
                message: format!(
                    "{}: changed on disk while heki was fixing it; its fixes were not written (run `heki check --fix` again)",
                    f.path.display()
                ),
            });
            continue;
        }
        std::fs::write(&f.path, &f.fixed)
            .with_context(|| format!("failed to write `{}`", f.path.display()))?;
        fixes += f.fixes;
        written += 1;
    }
    Ok((fixes, written, conflicts))
}

/// Baseline handling, rendering, summary, and exit code of one analyzed run.
pub(crate) fn report(
    args: &CheckArgs,
    project: &Project,
    mut diags: Vec<Diagnostic>,
) -> Result<ExitCode> {
    let root = &project.env.root;
    let by_path: FxHashMap<&Path, &ParsedFile> = project
        .files
        .iter()
        .map(|p| (p.path.as_path(), p))
        .collect();
    // Every diagnostic is emitted for one of the project's files (`CheckContext::emit` takes
    // the path of the unit it reports on), so its key path is that file's relative path.
    let key_path = |p: &Path| {
        by_path
            .get(p)
            .map(|f| f.rel.clone())
            .expect("diagnostics are emitted for the project's files")
    };
    let baseline_path = args
        .baseline
        .clone()
        .or_else(|| project.env.cfg.baseline.clone().map(|b| root.join(b)));
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
            return finish(fmt, &project.errors, ExitCode::SUCCESS);
        }
        if regressions > 0 {
            log!(
                fmt,
                "error: baseline {} not updated: {regressions} entry(ies) grew, and a Ratchet baseline only shrinks (delete it to record the current findings)",
                bp.display()
            );
        } else {
            let baseline = RatchetBaseline::from_counts(current);
            baseline.save(bp)?;
            log!(
                fmt,
                "Wrote Ratchet baseline {} ({} entries, {found} diagnostic(s)).",
                bp.display(),
                baseline.counts.len(),
            );
            return finish(fmt, &project.errors, ExitCode::SUCCESS);
        }
    }

    // Suppressions and the baseline saw every report; only output folds them.
    fold_root_causes(&mut diags);
    // Every format prints the paths `--diff` prints.
    for d in &mut diags {
        d.path = shown_path(&d.path);
        for r in &mut d.related {
            r.path = shown_path(&r.path);
        }
    }
    let by_shown: FxHashMap<PathBuf, (&ParsedFile, LineIndex)> = project
        .files
        .iter()
        .map(|p| (shown_path(&p.path), (p, LineIndex::from_source(&p.source))))
        .collect();
    let source_line = |p: &Path, line: u32| {
        by_shown
            .get(p)
            .and_then(|(f, lines)| lines.line_text(&f.source, line))
            .map(str::to_owned)
    };
    print!(
        "{}",
        render(&diags, format_of(args.output_format), &source_line)
    );
    if matches!(
        args.output_format,
        OutputFormat::Full | OutputFormat::Concise
    ) {
        summary(&diags, project.files.len(), baseline_path.is_some());
    }
    let code = if diags.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    };
    finish(args.output_format, &project.errors, code)
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
        target_version: args.target_version.as_deref(),
        overrides: None,
    })?;
    let inputs = project.inputs();
    let index = analyze_project(&inputs, &project.env.kb);
    let units = project.units();
    let snap = SummarySnapshot::build(&units, &index);
    if args.write {
        let path = project.env.root.join(SNAPSHOT_FILE);
        if !project.errors.is_empty() {
            eprintln!(
                "{}",
                not_written("summary snapshot", &path, project.errors.len())
            );
            return finish(OutputFormat::Full, &project.errors, ExitCode::SUCCESS);
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
    finish(OutputFormat::Full, &project.errors, ExitCode::SUCCESS)
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
pub(crate) fn finish(
    format: OutputFormat,
    errors: &[impl std::fmt::Display],
    code: ExitCode,
) -> Result<ExitCode> {
    for e in errors {
        log!(format, "error: {e}");
    }
    Ok(if errors.is_empty() {
        code
    } else {
        ExitCode::from(2)
    })
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
        assert!(
            conflicts[0]
                .message
                .starts_with(&format!("{}: changed on disk", kept.display())),
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
