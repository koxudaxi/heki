//! `heki check --watch`: one full check, then incremental re-checks of changed files through the
//! LSP engine (`heki_lsp::Workspace`: summaries stay resident, IR is rebuilt per changed file,
//! callers and importers are re-analyzed only when a summary or declaration changed).
//!
//! Files are polled (no platform watcher dependency). A change to the configuration, the
//! summary snapshot, or the Ratchet baseline, or a switch of the module layout, reloads the whole
//! workspace. A reload that fails, and a path that disappears, are reported while the watch goes
//! on.

use crate::{CheckArgs, OutputFormat};
use anyhow::Result;
use heki_ast::LineIndex;
use heki_diagnostics::{Diagnostic, render, sort_diagnostics};
use heki_effect::ModuleLayout;
use heki_lsp::{CONFIG_FILES, EditOutcome, Workspace};
use heki_profile::{ProfileData, ProfiledFunction};
use heki_rules::project::{
    FileError, ProjectEnv, ProjectOptions, collect_files, display_path, exclude_set,
};
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

const POLL: Duration = Duration::from_millis(200);

/// After a poll finds changed files, the watch polls again this long later, until a poll finds
/// no further change (at most `SETTLE_ROUNDS` times): files saved together, or written in
/// steps, are re-checked once, in their final state.
const SETTLE: Duration = Duration::from_millis(100);
const SETTLE_ROUNDS: usize = 10;

/// Files whose timestamps are younger than this at a poll may still change without changing
/// them (file systems with coarse timestamps): their content is compared too (`Stamp`).
const RACY: Duration = Duration::from_secs(2);

/// What a poll compares of a file: its size and modification time and, on Unix, its inode and
/// status-change time, which every write and every replacement update, also one that restores
/// the modification time (as file synchronization tools do).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Signature {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    status_changed: (i64, i64),
    #[cfg(unix)]
    inode: (u64, u64),
}

impl Signature {
    fn of(meta: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            len: meta.len(),
            modified: meta.modified().ok(),
            #[cfg(unix)]
            status_changed: (meta.ctime(), meta.ctime_nsec()),
            #[cfg(unix)]
            inode: (meta.dev(), meta.ino()),
        }
    }

    /// The latest time the file is stamped with.
    fn latest(&self) -> Option<SystemTime> {
        #[cfg(unix)]
        {
            let (secs, nanos) = self.status_changed;
            let status = u64::try_from(secs).ok().map(|s| {
                SystemTime::UNIX_EPOCH + Duration::new(s, u32::try_from(nanos).unwrap_or_default())
            });
            self.modified.max(status)
        }
        #[cfg(not(unix))]
        self.modified
    }
}

/// A file as one poll saw it. A file stamped within `RACY` of the poll could change again
/// inside the resolution of its timestamps, so its content is hashed, at that poll and at the
/// next one (when such a change has to show).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    signature: Signature,
    content: Option<u64>,
    racy: bool,
}

impl Stamp {
    /// `path` as a poll at `now` sees it (`None`: it cannot be looked at); `earlier`: as the
    /// previous poll saw it.
    fn take(path: &Path, earlier: Option<&Stamp>, now: SystemTime) -> Option<Self> {
        let signature = Signature::of(&std::fs::metadata(path).ok()?);
        let racy = signature
            .latest()
            .is_none_or(|t| now.duration_since(t).map_or(true, |age| age < RACY));
        let content = (racy || earlier.is_some_and(|e| e.racy))
            .then(|| std::fs::read(path).ok())
            .flatten()
            .map(|bytes| {
                let mut hasher = std::hash::DefaultHasher::new();
                bytes.hash(&mut hasher);
                hasher.finish()
            });
        Some(Self {
            signature,
            content,
            racy,
        })
    }

    /// The file is as it was at `earlier`: the same signature and, where both polls read it,
    /// the same content.
    fn same_as(&self, earlier: &Self) -> bool {
        self.signature == earlier.signature
            && !matches!((self.content, earlier.content), (Some(a), Some(b)) if a != b)
    }
}

/// Stamps of files: the Python files of the project (keyed like `ParsedFile::path`), or those
/// whose change reloads the workspace (absent files have none).
type Stamps = BTreeMap<PathBuf, Stamp>;

/// Whether no file of `earlier` changed, appeared, or disappeared in `now`.
fn unchanged(earlier: &Stamps, now: &Stamps) -> bool {
    earlier.len() == now.len()
        && now
            .iter()
            .all(|(p, s)| earlier.get(p).is_some_and(|e| s.same_as(e)))
}

/// Test hook: stop after this many re-checks (the watch otherwise runs until interrupted).
const MAX_RUNS_ENV: &str = "HEKI_WATCH_MAX_RUNS";

/// Stamps of `paths` at a poll; `earlier`: as the previous poll saw them.
fn stamps_of<'p>(paths: impl IntoIterator<Item = &'p PathBuf>, earlier: &Stamps) -> Stamps {
    let now = SystemTime::now();
    paths
        .into_iter()
        .filter_map(|p| Some((p.clone(), Stamp::take(p, earlier.get(p), now)?)))
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
    files: Stamps,
    /// The configuration files, the summary snapshot, and the Ratchet baseline.
    config_files: Vec<PathBuf>,
    config: Stamps,
}

impl Watcher<'_> {
    fn opts(&self) -> ProjectOptions<'_> {
        ProjectOptions {
            paths: &self.args.paths,
            config: self.args.analysis.config.as_deref(),
            target_version: self.args.analysis.target_version.as_deref(),
            overrides: None,
        }
    }

    /// Stamps first (a change during the analysis shows up on the next poll), then the
    /// workspace.
    fn load(&self, earlier: (&Stamps, &Stamps)) -> Result<Loaded> {
        let env = ProjectEnv::load(&self.opts())?;
        let config_files = self.config_files(&env);
        let config = stamps_of(&config_files, earlier.1);
        let files = self.scan(&env, earlier.0)?;
        let (ws, errors) = Workspace::load(
            &self.opts(),
            &crate::check::selectors(&self.args.analysis),
            self.args.analysis.baseline.as_deref(),
        )?;
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
        config.extend(self.args.analysis.config.clone());
        config.extend(
            self.args
                .analysis
                .baseline
                .clone()
                .or_else(|| env.cfg.baseline.as_ref().map(|b| root.join(b))),
        );
        config
    }

    /// The Python files of the project with their stamps. A path of the command line that does
    /// not exist (any more) holds no files: the watch goes on, and its files come back with it.
    fn scan(&self, env: &ProjectEnv, earlier: &Stamps) -> Result<Stamps> {
        let exclude = exclude_set(&env.cfg)?;
        let present: Vec<PathBuf> = self
            .args
            .paths
            .iter()
            .filter(|p| p.exists())
            .cloned()
            .collect();
        let files: Vec<PathBuf> = collect_files(&present, &env.root, &exclude)?
            .iter()
            .map(|p| display_path(p))
            .collect();
        Ok(stamps_of(&files, earlier))
    }

    fn print(&self, report: &Report, ws: Option<&Workspace>) -> bool {
        let mut all: Vec<Diagnostic> = report.diags.values().flatten().cloned().collect();
        // The index keeps no definition lines: a frame is told apart by its names only.
        let hot = self.hot.zip(ws).map(|(profile, ws)| {
            profile.attribute(ws.functions().map(|(path, qualname)| ProfiledFunction {
                path,
                qualname,
                lines: None,
            }))
        });
        crate::check::annotate_hot(&mut all, hot.as_ref());
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
            let text = heki_ast::read_source(p).ok()?;
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
        let errors = crate::check::shown_errors(report.errors.values());
        for e in report.failure.iter().chain(&errors) {
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
            let (rules_off, withheld) = ws.map_or((0, 0), |ws| {
                (
                    ws.selection().heuristic_off().len(),
                    ws.withheld_heuristic(),
                )
            });
            crate::check::heuristic_hint(rules_off, withheld);
        }
        !all.is_empty()
    }
}

/// The error of each file that could not be read, parsed, or checked when it was last looked at:
/// it is reported after every re-check until the file changes.
type Errors = BTreeMap<PathBuf, FileError>;

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
                .map(|e| (e.path.clone(), e))
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
    fn apply(&mut self, ws: &mut Workspace, before: &Stamps, now: &Stamps) {
        // Files to re-analyze once the batch is in the index. A file of the batch is analyzed
        // against the index as the files before it left it: what a later one changes for it
        // queues it again.
        let mut affected: BTreeSet<PathBuf> = BTreeSet::new();
        for gone in before.keys().filter(|p| !now.contains_key(*p)) {
            affected.extend(ws.remove(gone));
            self.diags.remove(gone);
            self.errors.remove(gone);
        }
        let changed = now
            .iter()
            .filter(|(p, s)| before.get(*p).is_none_or(|b| !s.same_as(b)));
        for (p, _) in changed {
            match heki_ast::read_source(p) {
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
                    self.errors.insert(p.clone(), FileError::unreadable(p, &e));
                }
            }
        }
        let read = |p: &Path| heki_ast::read_source(p).ok();
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
    let loaded = w.load((&Stamps::new(), &Stamps::new()))?;
    let (mut files, mut config_files, mut config) =
        (loaded.files, loaded.config_files, loaded.config);
    let mut ws = Some(loaded.ws);
    let mut report = Report::of(ws.as_mut().expect("just loaded"), loaded.errors);
    let mut dirty = w.print(&report, ws.as_ref());
    let mut runs = 0;
    while max_runs.is_none_or(|m| runs < m) {
        std::thread::sleep(POLL);
        let now_config = stamps_of(&config_files, &config);
        let reconfigured = !unchanged(&config, &now_config);
        // The racy files' content hashes of this poll are what the next one compares.
        config = now_config;
        // `src/__init__.py` or a root `__init__.py` appearing or disappearing switches the module
        // layout (`ModuleLayout`): every module name under `src/`, or every relative import that
        // climbs to the root, changes with it.
        let relayout = ws
            .as_ref()
            .is_some_and(|ws| ModuleLayout::detect(&ws.env.root) != ws.env.layout);
        if reconfigured || relayout {
            match w.load((&files, &config)) {
                Ok(loaded) => {
                    (files, config_files, config) =
                        (loaded.files, loaded.config_files, loaded.config);
                    let fresh = ws.insert(loaded.ws);
                    report = Report::of(fresh, loaded.errors);
                }
                Err(e) => {
                    ws = None;
                    report = Report::failed(&e);
                }
            }
        } else if let Some(ws) = ws.as_mut() {
            let now_files = match w.scan(&ws.env, &files) {
                Ok(now_files) => now_files,
                Err(e) => {
                    let failure = Some(format!("{e:#}"));
                    if report.failure != failure {
                        report.failure = failure;
                        runs += 1;
                        dirty = w.print(&report, Some(&*ws));
                    }
                    continue;
                }
            };
            if unchanged(&files, &now_files) && report.failure.is_none() {
                // The racy files' content hashes of this poll are what the next one compares.
                files = now_files;
                continue;
            }
            let mut now_files = now_files;
            for _ in 0..SETTLE_ROUNDS {
                std::thread::sleep(SETTLE);
                match w.scan(&ws.env, &now_files) {
                    Ok(settled) if !unchanged(&now_files, &settled) => now_files = settled,
                    _ => break,
                }
            }
            report.failure = None;
            report.apply(ws, &files, &now_files);
            files = now_files;
        } else {
            continue;
        }
        runs += 1;
        dirty = w.print(&report, ws.as_ref());
    }
    Ok(report.exit_code(dirty))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_compare_content_while_timestamps_are_racy() {
        let dir = std::env::temp_dir().join(format!("heki_watch_stamps_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.py");
        std::fs::write(&path, "x = 1\n").unwrap();
        let now = SystemTime::now();
        let first = Stamp::take(&path, None, now).unwrap();
        assert!(first.racy && first.content.is_some(), "{first:?}");
        // Long after, the file is no longer racy, but the poll after a racy one still reads it.
        let later = now + RACY * 2;
        let second = Stamp::take(&path, Some(&first), later).unwrap();
        assert!(!second.racy && second.content.is_some(), "{second:?}");
        assert!(second.same_as(&first));
        let third = Stamp::take(&path, Some(&second), later).unwrap();
        assert_eq!(third.content, None);
        assert!(third.same_as(&second));
        // A change the signature misses (a timestamp too coarse to move) shows in the content.
        let mut rewritten = first.clone();
        rewritten.content = Some(first.content.unwrap() ^ 1);
        assert!(!rewritten.same_as(&first));
        assert!(Stamp::take(&dir.join("missing.py"), None, now).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
