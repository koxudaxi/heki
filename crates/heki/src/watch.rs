//! `heki check --watch`: one full check, then incremental re-checks of changed files through the
//! LSP engine (`heki_lsp::Workspace`: summaries stay resident, IR is rebuilt per changed file,
//! callers and importers are re-analyzed only when a summary or declaration changed).
//!
//! Files are polled (no platform watcher dependency). A change to the configuration, the
//! summary snapshot, or the Ratchet baseline, or a switch of the `src` layout, reloads the whole
//! workspace. A reload that fails, and a path that disappears, are reported while the watch goes
//! on.

use crate::{CheckArgs, OutputFormat};
use anyhow::Result;
use heki_ast::LineIndex;
use heki_diagnostics::{Diagnostic, render, sort_diagnostics};
use heki_effect::detect_src_layout;
use heki_lsp::{CONFIG_FILES, EditOutcome, Selectors, Workspace};
use heki_profile::ProfileData;
use heki_rules::project::{
    FileError, ProjectEnv, ProjectOptions, collect_files, display_path, exclude_set,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

const POLL: Duration = Duration::from_millis(200);

/// Stamps of the Python files of the project (keyed like `ParsedFile::path`).
type FileStamps = BTreeMap<PathBuf, SystemTime>;

/// Stamps of the files whose change reloads the workspace (absent files have none).
type ConfigStamps = BTreeMap<PathBuf, SystemTime>;

/// Test hook: stop after this many re-checks (the watch otherwise runs until interrupted).
const MAX_RUNS_ENV: &str = "HEKI_WATCH_MAX_RUNS";

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn stamps_of(paths: &[PathBuf]) -> ConfigStamps {
    paths
        .iter()
        .filter_map(|p| Some((p.clone(), mtime(p)?)))
        .collect()
}

struct Watcher<'a> {
    args: &'a CheckArgs,
    hot: Option<&'a ProfileData>,
}

/// A loaded workspace with what it was loaded from.
struct Loaded {
    ws: Workspace,
    errors: Vec<FileError>,
    files: FileStamps,
    /// The configuration files, the summary snapshot, and the Ratchet baseline.
    config_files: Vec<PathBuf>,
    config: ConfigStamps,
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

    /// Stamps first (a change during the analysis shows up on the next poll), then the
    /// workspace.
    fn load(&self) -> Result<Loaded> {
        let env = ProjectEnv::load(&self.opts())?;
        let config_files = self.config_files(&env);
        let config = stamps_of(&config_files);
        let files = self.scan(&env)?;
        let selectors = Selectors {
            select: self.args.select.clone(),
            extend_select: self.args.extend_select.clone(),
            ignore: self.args.ignore.clone(),
        };
        let (ws, errors) =
            Workspace::load(&self.opts(), &selectors, self.args.baseline.as_deref())?;
        Ok(Loaded {
            ws,
            errors,
            files,
            config_files,
            config,
        })
    }

    /// The files whose change reloads the workspace: the configuration, the summary snapshot,
    /// and the Ratchet baseline the diagnostics are compared with.
    fn config_files(&self, env: &ProjectEnv) -> Vec<PathBuf> {
        let root = &env.root;
        let mut config: Vec<PathBuf> = CONFIG_FILES.iter().map(|n| root.join(n)).collect();
        config.extend(self.args.config.clone());
        config.extend(
            self.args
                .baseline
                .clone()
                .or_else(|| env.cfg.baseline.as_ref().map(|b| root.join(b))),
        );
        config
    }

    /// The Python files of the project with their stamps. A path of the command line that does
    /// not exist (any more) holds no files: the watch goes on, and its files come back with it.
    fn scan(&self, env: &ProjectEnv) -> Result<FileStamps> {
        let exclude = exclude_set(&env.cfg)?;
        let present: Vec<PathBuf> = self
            .args
            .paths
            .iter()
            .filter(|p| p.exists())
            .cloned()
            .collect();
        Ok(collect_files(&present, &env.root, &exclude)?
            .into_iter()
            .filter_map(|p| Some((display_path(&p), mtime(&p)?)))
            .collect())
    }

    fn print(&self, report: &Report) -> bool {
        let mut all: Vec<Diagnostic> = report.diags.values().flatten().cloned().collect();
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
        for e in report.failure.iter().chain(report.errors.values()) {
            log!(self.args.output_format, "error: {e}");
        }
        if matches!(
            self.args.output_format,
            OutputFormat::Full | OutputFormat::Concise
        ) {
            eprintln!(
                "[heki watch] {} diagnostic(s) in {} file(s); watching for changes.",
                all.len(),
                report.diags.len()
            );
        }
        !all.is_empty()
    }
}

/// The error of each file that could not be read, parsed, or checked when it was last looked at:
/// it is reported after every re-check until the file changes.
type Errors = BTreeMap<PathBuf, String>;

/// What the watch reports after each re-check.
#[derive(Default)]
struct Report {
    diags: BTreeMap<PathBuf, Vec<Diagnostic>>,
    errors: Errors,
    /// Why nothing is checked: the configuration became invalid, or the files could not be
    /// listed. Reported until a change fixes it.
    failure: Option<String>,
}

impl Report {
    /// Everything a freshly loaded workspace reports.
    fn of(ws: &mut Workspace, errors: Vec<FileError>) -> Self {
        let (diags, more) = ws.diagnose_all();
        Self {
            diags,
            errors: errors
                .into_iter()
                .chain(more)
                .map(|e| (e.path, e.message))
                .collect(),
            failure: None,
        }
    }

    fn failed(error: &anyhow::Error) -> Self {
        Self {
            failure: Some(format!("{error:#}")),
            ..Self::default()
        }
    }

    /// Keep the latest result of one re-analyzed file: its diagnostics, or none when it does
    /// not parse or check (the error is reported) or left the project (`None`).
    fn record(&mut self, path: &Path, outcome: Option<EditOutcome>) {
        self.errors.remove(path);
        match outcome {
            Some(EditOutcome {
                diagnostics,
                error: None,
                ..
            }) => {
                self.diags.insert(path.to_path_buf(), diagnostics);
            }
            Some(EditOutcome { error: Some(e), .. }) => {
                self.diags.remove(path);
                self.errors.insert(path.to_path_buf(), e);
            }
            None => {
                self.diags.remove(path);
            }
        }
    }

    /// Apply the files that changed between two polls (`before` -> `now`) to `ws`.
    fn apply(&mut self, ws: &mut Workspace, before: &FileStamps, now: &FileStamps) {
        // Files to re-analyze once the batch is in the index. A file of the batch is analyzed
        // against the index as the files before it left it: what a later one changes for it
        // queues it again.
        let mut affected: BTreeSet<PathBuf> = BTreeSet::new();
        for gone in before.keys().filter(|p| !now.contains_key(*p)) {
            affected.extend(ws.remove(gone));
            self.diags.remove(gone);
            self.errors.remove(gone);
        }
        let changed = now.iter().filter(|(p, t)| before.get(*p) != Some(t));
        for (p, _) in changed {
            match std::fs::read_to_string(p) {
                Ok(text) => {
                    let out = ws.edit(p, &text);
                    affected.remove(p);
                    affected.extend(out.affected.iter().cloned());
                    self.record(p, Some(out));
                }
                // A file that cannot be read is no longer part of the project, as for
                // `heki check`: its facts leave the index, and what they affected is
                // re-analyzed.
                Err(e) => {
                    affected.remove(p);
                    affected.extend(ws.remove(p));
                    self.diags.remove(p);
                    self.errors
                        .insert(p.clone(), format!("{}: {e}", p.display()));
                }
            }
        }
        let read = |p: &Path| std::fs::read_to_string(p).ok();
        for (p, out) in ws.refresh_all(affected, &read) {
            self.record(&p, out);
        }
    }

    fn exit_code(&self, dirty: bool) -> ExitCode {
        if self.failure.is_some() || !self.errors.is_empty() {
            ExitCode::from(2)
        } else if dirty {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        }
    }
}

pub(crate) fn run(args: &CheckArgs, hot: Option<&ProfileData>) -> Result<ExitCode> {
    if args.fix || args.diff || args.update_baseline {
        eprintln!(
            "warning: `--watch` only reports diagnostics; `--fix`, `--diff`, and `--update-baseline` are ignored."
        );
    }
    let w = Watcher { args, hot };
    let max_runs: Option<usize> = std::env::var(MAX_RUNS_ENV)
        .ok()
        .and_then(|v| v.parse().ok());
    // What fails before the first check (a missing path, an invalid configuration) ends the
    // run, as it ends `heki check`; later, the watch reports it and waits for a fix.
    let loaded = w.load()?;
    let (mut files, mut config_files, mut config) =
        (loaded.files, loaded.config_files, loaded.config);
    let mut ws = Some(loaded.ws);
    let mut report = Report::of(ws.as_mut().expect("just loaded"), loaded.errors);
    let mut dirty = w.print(&report);
    let mut runs = 0;
    while max_runs.is_none_or(|m| runs < m) {
        std::thread::sleep(POLL);
        let now_config = stamps_of(&config_files);
        // A package marker appearing or disappearing under `src/` can switch the `src` layout:
        // every module name under `src/` changes with it.
        let relayout = ws
            .as_ref()
            .is_some_and(|ws| detect_src_layout(&ws.env.root) != ws.env.src_layout);
        if now_config != config || relayout {
            match w.load() {
                Ok(loaded) => {
                    (files, config_files, config) =
                        (loaded.files, loaded.config_files, loaded.config);
                    let fresh = ws.insert(loaded.ws);
                    report = Report::of(fresh, loaded.errors);
                }
                Err(e) => {
                    ws = None;
                    config = now_config;
                    report = Report::failed(&e);
                }
            }
        } else if let Some(ws) = ws.as_mut() {
            let now_files = match w.scan(&ws.env) {
                Ok(now_files) => now_files,
                Err(e) => {
                    let failure = Some(format!("{e:#}"));
                    if report.failure != failure {
                        report.failure = failure;
                        runs += 1;
                        dirty = w.print(&report);
                    }
                    continue;
                }
            };
            if now_files == files && report.failure.is_none() {
                continue;
            }
            report.failure = None;
            report.apply(ws, &files, &now_files);
            files = now_files;
        } else {
            continue;
        }
        runs += 1;
        dirty = w.print(&report);
    }
    Ok(report.exit_code(dirty))
}
