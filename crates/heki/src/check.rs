//! `heki check`: discover -> parse / IR (parallel) -> effect summaries -> rules -> inline
//! suppressions -> fixes -> Ratchet baseline -> output. Exit codes: 0 clean, 1 violations (or
//! Ratchet regressions / pending `--diff`), 2 configuration / I/O / unknown rule / syntax errors.

use crate::{CheckArgs, OutputFormat, SnapshotArgs};
use anyhow::{Context, Result, bail};
use heki_diagnostics::{
    Diagnostic, RatchetBaseline, apply_fixes, baseline_counts, baseline_key, render, unified_diff,
};
use heki_effect::analyze_project;
use heki_profile::ProfileData;
use heki_rules::project::{ParsedFile, Project, ProjectOptions, retain_unsuppressed};
use heki_rules::{
    CheckExtras, RuleSelection, SNAPSHOT_FILE, SummarySnapshot, check_project, find_rule,
};
use rustc_hash::{FxHashMap, FxHashSet};
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
) -> Result<Option<SummarySnapshot>> {
    if !selection.is_enabled("MUT103") {
        return Ok(None);
    }
    let path = root.join(SNAPSHOT_FILE);
    if path.exists() {
        return Ok(Some(SummarySnapshot::load(&path)?));
    }
    eprintln!(
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
    let snapshot = snapshot_for(&selection, &project.env.root)?;
    let env = &project.env;
    let inputs = project.inputs();
    let index = analyze_project(&inputs, &env.kb);
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
    rel: String,
    original: String,
    fixed: String,
}

/// Apply fixes in memory until nothing more applies. Returns the number of applied fixes,
/// the changed files, and the read / syntax errors of the last pass.
fn fix_in_memory(
    args: &CheckArgs,
    hot: Option<&ProfileData>,
) -> Result<(usize, Vec<FixedFile>, Vec<String>)> {
    let mut overrides: FxHashMap<PathBuf, String> = FxHashMap::default();
    let mut originals: BTreeMap<PathBuf, (String, String)> = BTreeMap::new();
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
            originals
                .entry(file.path.clone())
                .or_insert_with(|| (file.rel.clone(), file.source.clone()));
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
        .filter_map(|(path, (rel, original))| {
            let fixed = overrides.remove(&path)?;
            (fixed != original).then_some(FixedFile {
                path,
                rel,
                original,
                fixed,
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
            print!("{}", unified_diff(&f.rel, &f.original, &f.fixed));
        }
        eprintln!("Would fix {fixes} issue(s) in {} file(s).", files.len());
        let code = if fixes == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        };
        return finish(&errors, code);
    }
    if args.fix {
        let (fixes, files, _) = fix_in_memory(args, hot.as_ref())?;
        for f in &files {
            std::fs::write(&f.path, &f.fixed)
                .with_context(|| format!("failed to write `{}`", f.path.display()))?;
        }
        eprintln!("Fixed {fixes} issue(s) in {} file(s).", files.len());
    }
    let Analyzed { project, diags } = analyze(args, hot.as_ref(), None)?;
    report(args, &project, diags)
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
    if args.update_baseline {
        let Some(bp) = &baseline_path else {
            bail!(
                "`--update-baseline` needs `--baseline <path>` or `baseline` in the configuration"
            );
        };
        let baseline = RatchetBaseline::from_counts(baseline_counts(&diags, &key_path));
        baseline.save(bp)?;
        eprintln!(
            "Wrote Ratchet baseline {} ({} entries, {} diagnostic(s)).",
            bp.display(),
            baseline.counts.len(),
            diags.len()
        );
        return finish(&project.errors, ExitCode::SUCCESS);
    }
    if let Some(bp) = &baseline_path {
        let baseline = RatchetBaseline::load(bp)?;
        let cmp = baseline.compare(&baseline_counts(&diags, &key_path));
        for (key, allowed, cur) in &cmp.stale_entries {
            eprintln!(
                "note: baseline entry {}::{} {} `{}` can shrink {allowed} -> {cur} (`--update-baseline`)",
                key.path, key.qualname, key.rule_code, key.target
            );
        }
        let regressed: FxHashSet<_> = cmp.regressions.iter().map(|(k, _, _)| k.clone()).collect();
        diags.retain(|d| regressed.contains(&baseline_key(d, key_path(&d.path))));
    }

    let source_line = |p: &Path, line: u32| {
        by_path.get(p).and_then(|f| {
            f.source
                .lines()
                .nth(usize::try_from(line.saturating_sub(1)).unwrap_or(usize::MAX))
                .map(str::to_owned)
        })
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
    finish(&project.errors, code)
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
        snap.save(&path)?;
        eprintln!(
            "Wrote summary snapshot {} ({} function(s)).",
            path.display(),
            snap.functions.len()
        );
    } else {
        print!("{}", snap.to_json());
    }
    finish(&project.errors, ExitCode::SUCCESS)
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

/// Syntax / read errors make the run fail with exit code 2 after the report.
pub(crate) fn finish(errors: &[String], code: ExitCode) -> Result<ExitCode> {
    for e in errors {
        eprintln!("error: {e}");
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
