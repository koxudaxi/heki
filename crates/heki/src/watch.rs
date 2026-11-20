//! `heki check --watch`: one full check, then incremental re-checks of changed files through the
//! LSP engine (`heki_lsp::Workspace`: summaries stay resident, IR is rebuilt per changed file,
//! callers and importers are re-analyzed only when a summary or declaration changed).
//!
//! Files are polled (no platform watcher dependency). A change to the configuration or to the
//! summary snapshot reloads the whole workspace.

use crate::{CheckArgs, OutputFormat};
use anyhow::Result;
use heki_ast::LineIndex;
use heki_diagnostics::{Diagnostic, render, sort_diagnostics};
use heki_lsp::{CONFIG_FILES, EditOutcome, Selectors, Workspace};
use heki_profile::ProfileData;
use heki_rules::project::{ProjectEnv, ProjectOptions, collect_files, display_path, exclude_set};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

const POLL: Duration = Duration::from_millis(200);

/// Python files and configuration files with their modification times.
type Stamps = (BTreeMap<PathBuf, SystemTime>, Vec<SystemTime>);

/// Test hook: stop after this many re-checks (the watch otherwise runs until interrupted).
const MAX_RUNS_ENV: &str = "HEKI_WATCH_MAX_RUNS";

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

struct Watcher<'a> {
    args: &'a CheckArgs,
    hot: Option<&'a ProfileData>,
}

impl Watcher<'_> {
    fn opts(&self) -> ProjectOptions<'_> {
        ProjectOptions {
            paths: &self.args.paths,
            config: self.args.config.as_deref(),
            target_version: self.args.target_version.as_deref(),
            overrides: None,
        }
    }

    /// Modification times first (a change during the analysis shows up on the next poll),
    /// then the workspace.
    fn load(&self) -> Result<(Workspace, Vec<String>, Stamps)> {
        let stamps = self.scan(&ProjectEnv::load(&self.opts())?)?;
        let selectors = Selectors {
            select: self.args.select.clone(),
            extend_select: self.args.extend_select.clone(),
            ignore: self.args.ignore.clone(),
        };
        let (ws, errors) = Workspace::load(&self.opts(), &selectors)?;
        Ok((ws, errors, stamps))
    }

    /// Python files with their modification times, plus the configuration files.
    /// (`paths` is never empty: the CLI defaults it to `.`.)
    fn scan(&self, env: &ProjectEnv) -> Result<Stamps> {
        let ws_env = env;
        let exclude = exclude_set(&ws_env.cfg)?;
        let files = collect_files(&self.args.paths, &ws_env.root, &exclude)?
            .into_iter()
            .filter_map(|p| Some((display_path(&p), mtime(&p)?)))
            .collect();
        let root = &ws_env.root;
        let mut config: Vec<PathBuf> = CONFIG_FILES.iter().map(|n| root.join(n)).collect();
        config.extend(self.args.config.clone());
        let stamps = config.iter().filter_map(|p| mtime(p)).collect();
        Ok((files, stamps))
    }

    fn print(&self, diags: &BTreeMap<PathBuf, Vec<Diagnostic>>, errors: &[String]) -> bool {
        let mut all: Vec<Diagnostic> = diags.values().flatten().cloned().collect();
        crate::check::annotate_hot(&mut all, self.hot);
        // The paths `heki check` prints.
        for d in &mut all {
            d.path = crate::check::shown_path(&d.path);
            for r in &mut d.related {
                r.path = crate::check::shown_path(&r.path);
            }
        }
        sort_diagnostics(&mut all);
        // As `heki check` prints them: across every file.
        heki_rules::fold_root_causes(&mut all);
        let source_line = |p: &Path, line: u32| {
            let text = std::fs::read_to_string(p).ok()?;
            LineIndex::from_source(&text)
                .line_text(&text, line)
                .map(str::to_owned)
        };
        print!(
            "{}",
            render(
                &all,
                crate::check::format_of(self.args.output_format),
                &source_line
            )
        );
        for e in errors {
            log!(self.args.output_format, "error: {e}");
        }
        if matches!(
            self.args.output_format,
            OutputFormat::Full | OutputFormat::Concise
        ) {
            eprintln!(
                "[heki watch] {} diagnostic(s) in {} file(s); watching for changes.",
                all.len(),
                diags.len()
            );
        }
        !all.is_empty()
    }
}

/// Keep the latest result of one re-analyzed file: its diagnostics, or none when it does not
/// parse or check (the error is reported) or left the project (`None`).
fn record(
    diags: &mut BTreeMap<PathBuf, Vec<Diagnostic>>,
    errors: &mut Vec<String>,
    path: &Path,
    outcome: Option<EditOutcome>,
) {
    match outcome {
        Some(EditOutcome {
            diagnostics,
            error: None,
            ..
        }) => {
            diags.insert(path.to_path_buf(), diagnostics);
        }
        Some(EditOutcome { error: Some(e), .. }) => {
            diags.remove(path);
            errors.push(e);
        }
        None => {
            diags.remove(path);
        }
    }
}

pub(crate) fn run(args: &CheckArgs, hot: Option<&ProfileData>) -> Result<ExitCode> {
    if args.fix || args.diff || args.baseline.is_some() || args.update_baseline {
        eprintln!(
            "warning: `--watch` only reports diagnostics; `--fix`, `--diff`, `--baseline`, and `--update-baseline` are ignored."
        );
    }
    let w = Watcher { args, hot };
    let max_runs: Option<usize> = std::env::var(MAX_RUNS_ENV)
        .ok()
        .and_then(|v| v.parse().ok());
    let (mut ws, mut errors, (mut files, mut config)) = w.load()?;
    let (mut diags, more) = ws.diagnose_all();
    errors.extend(more);
    let mut dirty = w.print(&diags, &errors);
    let mut runs = 0;
    while max_runs.is_none_or(|m| runs < m) {
        std::thread::sleep(POLL);
        let (now_files, now_config) = w.scan(&ws.env)?;
        if now_config != config {
            let (fresh, errs, stamps) = w.load()?;
            ws = fresh;
            let (d, more) = ws.diagnose_all();
            diags = d;
            errors = errs;
            errors.extend(more);
            (files, config) = stamps;
            runs += 1;
            dirty = w.print(&diags, &errors);
            continue;
        } else if now_files != files {
            errors.clear();
            let mut affected: Vec<PathBuf> = Vec::new();
            for gone in files.keys().filter(|p| !now_files.contains_key(*p)) {
                affected.extend(ws.remove(gone));
                diags.remove(gone);
            }
            let changed: Vec<&PathBuf> = now_files
                .iter()
                .filter(|(p, t)| files.get(*p) != Some(t))
                .map(|(p, _)| p)
                .collect();
            for p in &changed {
                match std::fs::read_to_string(p) {
                    Ok(text) => {
                        let out = ws.edit(p, &text);
                        affected.extend(out.affected.iter().cloned());
                        record(&mut diags, &mut errors, p, Some(out));
                    }
                    Err(e) => {
                        diags.remove(*p);
                        errors.push(format!("{}: {e}", p.display()));
                    }
                }
            }
            affected.retain(|p| !changed.contains(&p));
            let read = |p: &Path| std::fs::read_to_string(p).ok();
            for (p, out) in ws.refresh_all(affected, &read) {
                record(&mut diags, &mut errors, &p, out);
            }
        } else {
            continue;
        }
        files = now_files;
        config = now_config;
        runs += 1;
        dirty = w.print(&diags, &errors);
    }
    Ok(if !errors.is_empty() {
        ExitCode::from(2)
    } else if dirty {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}
