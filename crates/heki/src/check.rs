//! `heki check`: discover -> parse / IR (parallel) -> effect summaries -> rules -> inline
//! suppressions -> Ratchet baseline -> output. Exit codes: 0 clean, 1 violations (or Ratchet
//! regressions), 2 configuration / I/O / unknown rule / syntax errors.

use crate::{CheckArgs, OutputFormat, SnapshotArgs};
use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSet, GlobSetBuilder};
use heki_ast::LineIndex;
use heki_diagnostics::{
    Diagnostic, InlineIgnores, RatchetBaseline, apply_fixes, baseline_counts, baseline_key, render,
    unified_diff,
};
use heki_effect::{ModuleInput, analyze_project, detect_src_layout, module_name_for_path};
use heki_env::{HekiConfig, PythonVersion, RuntimeProfile};
use heki_ir::ModuleIr;
use heki_kb::{EcosystemEntry, EffectiveEcosystemKb, EffectiveSoundnessKb};
use heki_profile::ProfileData;
use heki_rules::{
    CheckExtras, ModuleUnit, RuleSelection, SNAPSHOT_FILE, SummarySnapshot, check_project,
    find_rule,
};
use heki_semantic::SemanticModel;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

/// Directories skipped during discovery unless `exclude` replaces this list (Ruff-style).
const DEFAULT_EXCLUDE: &[&str] = &[
    ".bzr",
    ".direnv",
    ".eggs",
    ".git",
    ".hg",
    ".mypy_cache",
    ".nox",
    ".pants.d",
    ".ruff_cache",
    ".svn",
    ".tox",
    ".venv",
    "__pypackages__",
    "_build",
    "buck-out",
    "build",
    "dist",
    "node_modules",
    "site-packages",
    "venv",
];

const ROOT_MARKERS: &[&str] = &["heki.toml", ".heki.toml", "pyproject.toml", ".git"];

pub(crate) struct ParsedFile {
    path: PathBuf,
    rel: String,
    module_name: String,
    is_package: bool,
    source: String,
    semantic: SemanticModel,
    ir: ModuleIr,
    comments: Vec<heki_ast::Span>,
}

/// Project-relative POSIX path (falls back to the display path outside the root).
fn rel_posix(path: &Path, root: &Path) -> String {
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let rel = abs.strip_prefix(root).unwrap_or(path);
    let mut parts = Vec::new();
    for c in rel.components() {
        if let Component::Normal(s) = c {
            parts.push(s.to_string_lossy().into_owned());
        }
    }
    parts.join("/")
}

/// Display form of a discovered path (`./pkg/a.py` -> `pkg/a.py`).
fn display_path(path: &Path) -> PathBuf {
    path.strip_prefix(".")
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf())
}

fn find_root(start: &Path) -> PathBuf {
    let start = std::fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    let dir = if start.is_dir() {
        start.clone()
    } else {
        start.parent().map(Path::to_path_buf).unwrap_or_default()
    };
    for d in dir.ancestors() {
        if ROOT_MARKERS.iter().any(|m| d.join(m).exists()) {
            return d.to_path_buf();
        }
    }
    dir
}

fn glob_set(patterns: &[String], what: &str) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(Glob::new(p).with_context(|| format!("invalid {what} glob `{p}`"))?);
        // `generated` also excludes everything below it.
        b.add(Glob::new(&format!("{p}/**")).with_context(|| format!("invalid {what} glob `{p}`"))?);
    }
    Ok(b.build()?)
}

/// Excluded when the project-relative path or any of its components matches.
fn is_excluded(rel: &str, set: &GlobSet) -> bool {
    set.is_match(rel) || rel.split('/').any(|c| set.is_match(c))
}

fn collect_files(paths: &[PathBuf], root: &Path, exclude: &GlobSet) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for p in paths {
        if !p.exists() {
            bail!("path `{}` does not exist", p.display());
        }
        if p.is_file() {
            // Explicitly named files are always checked (Ruff semantics).
            files.push(p.clone());
            continue;
        }
        let mut builder = ignore::WalkBuilder::new(p);
        builder.add_custom_ignore_filename(".hekiignore");
        for entry in builder.build() {
            let entry = entry.with_context(|| format!("failed to walk `{}`", p.display()))?;
            let ep = entry.path();
            if ep.is_file()
                && ep.extension().is_some_and(|e| e == "py" || e == "pyi")
                && !is_excluded(&rel_posix(ep, root), exclude)
            {
                files.push(ep.to_path_buf());
            }
        }
    }
    let mut seen = FxHashSet::default();
    files.retain(|f| seen.insert(std::fs::canonicalize(f).unwrap_or_else(|_| f.clone())));
    files.sort();
    Ok(files)
}

fn parse_file(
    path: &Path,
    root: &Path,
    src_layout: bool,
    profile: RuntimeProfile,
    overrides: Option<&FxHashMap<PathBuf, String>>,
) -> Result<ParsedFile, String> {
    let source = match overrides.and_then(|o| o.get(&display_path(path))) {
        Some(text) => text.clone(),
        None => std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?,
    };
    let parsed = heki_ast::parse_module(&source).map_err(|errs| {
        let index = LineIndex::from_source(&source);
        let first = &errs[0];
        let (line, col) = index.line_col(first.span.start);
        format!(
            "{}:{line}:{col}: syntax error: {} ({} error(s))",
            path.display(),
            first.message,
            errs.len()
        )
    })?;
    let rel = rel_posix(path, root);
    let (module_name, is_package) = module_name_for_path(Path::new(&rel), src_layout)
        .unwrap_or_else(|| {
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            (stem, false)
        });
    let semantic = heki_semantic::analyze_module(&parsed, &source, profile);
    let ir = heki_ir::build_module_ir(&parsed, &source, &semantic, profile);
    Ok(ParsedFile {
        path: display_path(path),
        rel,
        module_name,
        is_package,
        comments: parsed.comment_spans(),
        source,
        semantic,
        ir,
    })
}

/// Where a run looks for files and configuration.
pub(crate) struct ProjectOptions<'a> {
    pub paths: &'a [PathBuf],
    pub config: Option<&'a Path>,
    pub target_version: Option<&'a str>,
    /// In-memory sources replacing files on disk (keyed like `ParsedFile::path`).
    pub overrides: Option<&'a FxHashMap<PathBuf, String>>,
}

/// A discovered, parsed project (IR kept for the one-shot CLI run).
pub(crate) struct Project {
    pub root: PathBuf,
    pub cfg: HekiConfig,
    pub kb: EffectiveEcosystemKb,
    pub soundness_kb: EffectiveSoundnessKb,
    pub files: Vec<ParsedFile>,
    /// Read / syntax errors (exit code 2 after the report).
    pub errors: Vec<String>,
}

fn load_config(opts: &ProjectOptions<'_>) -> Result<(HekiConfig, PathBuf)> {
    let first = opts
        .paths
        .first()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("."));
    let root = find_root(&first);
    if let Some(cfg_path) = opts.config {
        let cfg = HekiConfig::load_from_file(cfg_path)?;
        return Ok((cfg, root));
    }
    Ok((HekiConfig::load_from_dir(&root)?, root))
}

fn profile(target_version: Option<&str>, cfg: &HekiConfig) -> Result<RuntimeProfile> {
    match target_version.or(cfg.target_version.as_deref()) {
        Some(t) => PythonVersion::parse_tag(t)
            .map(RuntimeProfile::from)
            .with_context(|| format!("unknown target version `{t}` (expected py311..py315)")),
        None => Ok(cfg.runtime_profile()),
    }
}

impl Project {
    pub(crate) fn load(opts: &ProjectOptions<'_>) -> Result<Self> {
        let (cfg, root) = load_config(opts)?;
        let runtime = profile(opts.target_version, &cfg)?;
        let overrides = cfg
            .ecosystem
            .iter()
            .map(EcosystemEntry::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let kb = EffectiveEcosystemKb::load_builtin(&overrides)?;
        let soundness_kb = EffectiveSoundnessKb::load_builtin(runtime.python_version)?;
        let mut exclude: Vec<String> = cfg
            .exclude
            .clone()
            .unwrap_or_else(|| DEFAULT_EXCLUDE.iter().map(|s| (*s).to_owned()).collect());
        exclude.extend(cfg.extend_exclude.iter().cloned());
        let exclude = glob_set(&exclude, "exclude")?;
        let paths = if opts.paths.is_empty() {
            vec![PathBuf::from(".")]
        } else {
            opts.paths.to_vec()
        };
        let found = collect_files(&paths, &root, &exclude)?;
        let src_layout = detect_src_layout(&root);
        let results: Vec<Result<ParsedFile, String>> = found
            .par_iter()
            .map(|p| parse_file(p, &root, src_layout, runtime, opts.overrides))
            .collect();
        let mut files = Vec::new();
        let mut errors = Vec::new();
        for r in results {
            match r {
                Ok(p) => files.push(p),
                Err(e) => errors.push(e),
            }
        }
        Ok(Self {
            root,
            cfg,
            kb,
            soundness_kb,
            files,
            errors,
        })
    }

    pub(crate) fn inputs(&self) -> Vec<ModuleInput<'_>> {
        self.files.iter().map(module_input).collect()
    }

    pub(crate) fn units(&self) -> Vec<ModuleUnit<'_>> {
        self.files
            .iter()
            .map(|p| ModuleUnit {
                input: module_input(p),
                source: &p.source,
                rel_path: p.rel.clone(),
                lines: LineIndex::from_source(&p.source),
            })
            .collect()
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
    let cfg = &project.cfg;
    let selection =
        RuleSelection::resolve(&cfg.lint, &args.select, &args.extend_select, &args.ignore)?;
    for key in cfg.lint.severity.keys() {
        if find_rule(key).is_none() {
            bail!("unknown rule `{key}` in lint.severity");
        }
    }
    let snapshot = if selection.is_enabled("MUT103") {
        let path = project.root.join(SNAPSHOT_FILE);
        if path.exists() {
            Some(SummarySnapshot::load(&path)?)
        } else {
            eprintln!(
                "warning: `MUT103` is enabled but `{}` does not exist; run `heki snapshot --write` first",
                path.display()
            );
            None
        }
    } else {
        None
    };
    let inputs = project.inputs();
    let index = analyze_project(&inputs, &project.kb);
    let units = project.units();
    let extras = CheckExtras {
        soundness_kb: &project.soundness_kb,
        snapshot: snapshot.as_ref(),
    };
    let mut diags = check_project(&units, cfg, &project.kb, &index, &selection, &extras)?;
    let ignores: FxHashMap<&Path, InlineIgnores> = project
        .files
        .par_iter()
        .map(|p| {
            (
                p.path.as_path(),
                InlineIgnores::from_comments(&p.source, &p.comments),
            )
        })
        .collect();
    diags.retain(|d| {
        !ignores
            .get(d.path.as_path())
            .is_some_and(|ign| ign.is_suppressed(d.line, &d.rule_code, &d.rule_slug))
    });
    if let Some(hot) = hot {
        for d in diags.iter_mut().filter(|d| d.rule_code.starts_with("OPT")) {
            if let Some(pct) = hot.cum_pct(&d.path, &d.qualname) {
                let note = format!(
                    "profile: `{}` is on the stack in {pct:.1}% of samples",
                    d.qualname
                );
                d.evidence = Some(match d.evidence.take() {
                    Some(e) => format!("{e}; {note}"),
                    None => note,
                });
            }
        }
    }
    drop(units);
    drop(inputs);
    Ok(Analyzed { project, diags })
}

/// Whether `--fix` / `--diff` applies this diagnostic's fix: `safe` + `unconditional` rules
/// (`OPT101` / `OPT102`) always; `size-dependent` ones with `--include-size-dependent` or in a
/// function the profile shows hot.
fn applicable(d: &Diagnostic, args: &CheckArgs, hot: Option<&ProfileData>) -> bool {
    if d.fix.is_none() {
        return false;
    }
    let Some(meta) = find_rule(&d.rule_code) else {
        return false;
    };
    meta.auto_fix_default
        || args.include_size_dependent
        || hot.is_some_and(|h| h.is_hot(&d.path, &d.qualname))
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
    let mut errors = Vec::new();
    for _ in 0..MAX_FIX_PASSES {
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
        for (path, groups) in by_path {
            let Some(file) = a.project.files.iter().find(|f| f.path == path) else {
                continue;
            };
            let (new, applied) = apply_fixes(&file.source, &groups);
            if applied.is_empty() || new == file.source {
                continue;
            }
            pass += applied.len();
            originals
                .entry(file.path.clone())
                .or_insert_with(|| (file.rel.clone(), file.source.clone()));
            overrides.insert(file.path.clone(), new);
        }
        if pass == 0 {
            break;
        }
        total += pass;
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
    if args.watch {
        eprintln!(
            "warning: `--watch` arrives with the incremental server (Milestone M7); checking once."
        );
    }
    let hot = args
        .profile
        .as_deref()
        .map(heki_profile::load)
        .transpose()?;
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
    report(args, hot.as_ref())
}

/// Report the diagnostics (after `--fix`, of the fixed files).
fn report(args: &CheckArgs, hot: Option<&ProfileData>) -> Result<ExitCode> {
    let Analyzed {
        mut project,
        mut diags,
    } = analyze(args, hot, None)?;
    let root = project.root.clone();
    let by_path: FxHashMap<&Path, &ParsedFile> = project
        .files
        .iter()
        .map(|p| (p.path.as_path(), p))
        .collect();
    let key_path = |p: &Path| {
        by_path
            .get(p)
            .map_or_else(|| p.to_string_lossy().replace('\\', "/"), |f| f.rel.clone())
    };
    let baseline_path = args
        .baseline
        .clone()
        .or_else(|| project.cfg.baseline.clone().map(|b| root.join(b)));
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
    let errors = std::mem::take(&mut project.errors);
    finish(&errors, code)
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
    let index = analyze_project(&inputs, &project.kb);
    let units = project.units();
    let snap = SummarySnapshot::build(&units, &index);
    if args.write {
        let path = project.root.join(SNAPSHOT_FILE);
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
fn finish(errors: &[String], code: ExitCode) -> Result<ExitCode> {
    for e in errors {
        eprintln!("error: {e}");
    }
    Ok(if errors.is_empty() {
        code
    } else {
        ExitCode::from(2)
    })
}

fn module_input(p: &ParsedFile) -> ModuleInput<'_> {
    ModuleInput {
        path: p.path.clone(),
        module_name: p.module_name.clone(),
        is_package: p.is_package,
        content_hash: String::new(),
        semantic: &p.semantic,
        ir: &p.ir,
    }
}

fn format_of(f: OutputFormat) -> heki_diagnostics::OutputFormat {
    match f {
        OutputFormat::Full => heki_diagnostics::OutputFormat::Full,
        OutputFormat::Concise => heki_diagnostics::OutputFormat::Concise,
        OutputFormat::Json => heki_diagnostics::OutputFormat::Json,
        OutputFormat::Github => heki_diagnostics::OutputFormat::Github,
        OutputFormat::Sarif => heki_diagnostics::OutputFormat::Sarif,
    }
}
