//! Incremental engine behind `heki server` and `heki check --watch` (ADR-017).
//!
//! Resident state is the configuration, the knowledge bases, per-file metadata, and the
//! `ProjectIndex` (lightweight `FileSummary`s + reverse call graph). IR is built for one file at a
//! time and dropped as soon as its summary and diagnostics are computed.

use globset::GlobSet;
use heki_diagnostics::Diagnostic;
use heki_effect::{FuncKey, ProjectDecls, ProjectIndex, analyze_project, is_private_name};
use heki_rules::project::{
    ParsedFile, Project, ProjectEnv, ProjectError, ProjectOptions, exclude_set, parse_source,
    rel_posix, retain_unsuppressed,
};
use heki_rules::{
    CheckExtras, RuleSelection, SNAPSHOT_FILE, SelectionError, SnapshotError, SummarySnapshot,
    check_project, find_rule,
};
use rustc_hash::FxHashSet;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Project(ProjectError),
    #[error(transparent)]
    Selection(SelectionError),
    #[error(transparent)]
    Snapshot(SnapshotError),
    #[error("unknown rule `{0}` in lint.severity")]
    UnknownSeverityRule(String),
}

/// Counters exposed through the `heki/stats` request (Early Cutoff verification).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct EngineStats {
    /// Files in the index.
    pub indexed_files: usize,
    /// Re-analyses of edited / saved files.
    pub edited_reanalyses: usize,
    /// Re-analyses of *other* files (affected callers, declaration dependents).
    pub dependent_reanalyses: usize,
    /// Outcome of the last edit: a final summary changed (`false`: Early Cutoff).
    pub last_summary_changed: bool,
    /// Outcome of the last edit: the module's declarations changed.
    pub last_decls_changed: bool,
}

#[derive(Debug, Clone)]
struct FileMeta {
    module_name: String,
}

/// Result of re-analyzing one edited file.
#[derive(Debug, Clone, Default)]
pub struct EditOutcome {
    pub diagnostics: Vec<Diagnostic>,
    /// Other files whose diagnostics may change (callers of changed summaries and, when the
    /// module's declarations changed, importers that need re-extraction).
    pub affected: Vec<PathBuf>,
}

/// Selection overrides (`--select` / `--extend-select` / `--ignore`).
#[derive(Debug, Clone, Default)]
pub struct Selectors {
    pub select: Vec<String>,
    pub extend_select: Vec<String>,
    pub ignore: Vec<String>,
}

pub struct Workspace {
    pub env: ProjectEnv,
    selection: RuleSelection,
    snapshot: Option<SummarySnapshot>,
    exclude: GlobSet,
    files: BTreeMap<PathBuf, FileMeta>,
    index: ProjectIndex,
    /// Declarations of every indexed module, updated in place by edits.
    decls: ProjectDecls,
    stats: EngineStats,
}

impl Workspace {
    /// Index every Python file under `paths` (IR is dropped once the index is built).
    /// `overrides` replaces the disk text of open documents.
    pub fn load(
        opts: &ProjectOptions<'_>,
        selectors: &Selectors,
    ) -> Result<(Self, Vec<String>), EngineError> {
        let project = Project::load(opts).map_err(EngineError::Project)?;
        let env_cfg = &project.env.cfg;
        let selection = RuleSelection::resolve(
            &env_cfg.lint,
            &selectors.select,
            &selectors.extend_select,
            &selectors.ignore,
        )
        .map_err(EngineError::Selection)?;
        if let Some(k) = env_cfg
            .lint
            .severity
            .keys()
            .find(|k| find_rule(k).is_none())
        {
            return Err(EngineError::UnknownSeverityRule(k.clone()));
        }
        let snapshot_path = project.env.root.join(SNAPSHOT_FILE);
        let snapshot = if selection.is_enabled("MUT103") && snapshot_path.exists() {
            Some(SummarySnapshot::load(&snapshot_path).map_err(EngineError::Snapshot)?)
        } else {
            None
        };
        let exclude = exclude_set(env_cfg).map_err(EngineError::Project)?;
        let index = analyze_project(&project.inputs(), &project.env.kb);
        let files = project
            .files
            .iter()
            .map(|f| {
                (
                    f.path.clone(),
                    FileMeta {
                        module_name: f.module_name.clone(),
                    },
                )
            })
            .collect();
        let Project { env, errors, .. } = project;
        let decls = index.project_decls();
        let stats = EngineStats {
            indexed_files: index.files.len(),
            ..EngineStats::default()
        };
        Ok((
            Self {
                env,
                selection,
                snapshot,
                exclude,
                files,
                index,
                decls,
                stats,
            },
            errors,
        ))
    }

    pub fn stats(&self) -> &EngineStats {
        &self.stats
    }

    /// A Python file the project would index (inside the root, not excluded).
    pub fn accepts(&self, path: &Path) -> bool {
        path.extension().is_some_and(|e| e == "py" || e == "pyi")
            && path.starts_with(&self.env.root)
            && {
                let rel = rel_posix(path, &self.env.root);
                !self.exclude.is_match(&rel) && !rel.split('/').any(|c| self.exclude.is_match(c))
            }
    }

    fn parse(&self, path: &Path, text: &str) -> Result<ParsedFile, String> {
        parse_source(
            path,
            &self.env.root,
            text.to_owned(),
            self.env.src_layout,
            self.env.runtime,
        )
    }

    fn check(&self, parsed: &ParsedFile) -> Result<Vec<Diagnostic>, String> {
        let unit = parsed.unit();
        let extras = CheckExtras {
            soundness_kb: &self.env.soundness_kb,
            snapshot: self.snapshot.as_ref(),
            decls: Some(&self.decls),
        };
        let mut diags = check_project(
            std::slice::from_ref(&unit),
            &self.env.cfg,
            &self.env.kb,
            &self.index,
            &self.selection,
            &extras,
        )
        .map_err(|e| e.to_string())?;
        retain_unsuppressed(&mut diags, [parsed]);
        Ok(diags)
    }

    /// Diagnostics of `text` as `path` against the current index (the index is unchanged).
    pub fn diagnose(&self, path: &Path, text: &str) -> Result<Vec<Diagnostic>, String> {
        let parsed = self.parse(path, text)?;
        self.check(&parsed)
    }

    /// Files importing `module` (or a package containing it): their local summaries resolve
    /// names through its declarations.
    fn dependents_of(&self, module: &str, except: &Path) -> Vec<PathBuf> {
        let refers = |fqn: &str| {
            fqn == module
                || fqn.starts_with(&format!("{module}."))
                || module.starts_with(&format!("{fqn}."))
        };
        let mut out: Vec<PathBuf> = self
            .index
            .files
            .values()
            .filter(|f| f.path != except)
            .filter(|f| {
                f.decls
                    .imports
                    .values()
                    .chain(f.decls.type_checking_imports.values())
                    .any(|fqn| refers(fqn))
            })
            .map(|f| f.path.clone())
            .collect();
        out.sort();
        out
    }

    /// Re-summarize one parsed file; returns the update and whether its declarations changed.
    fn summarize(&mut self, parsed: &ParsedFile) -> (heki_effect::IncrementalUpdate, bool) {
        self.files.insert(
            parsed.path.clone(),
            FileMeta {
                module_name: parsed.module_name.clone(),
            },
        );
        let out =
            self.index
                .reanalyze_module_cached(&parsed.input(), &self.env.kb, &mut self.decls);
        self.stats.indexed_files = self.index.files.len();
        out
    }

    /// Private functions the indexed file `path` calls. A private helper with project callers
    /// hands its `MUT201` / `MUT203` reports to them, so its own file's diagnostics depend on
    /// whether it has callers at all.
    fn private_callees(&self, path: &Path) -> FxHashSet<FuncKey> {
        self.index
            .files
            .get(path)
            .into_iter()
            .flat_map(|f| &f.call_edges)
            .filter(|(_, callee, _)| is_private_name(&callee.qualname))
            .map(|(_, callee, _)| callee.clone())
            .collect()
    }

    /// Files of the private functions an edit started or stopped calling: each may have gained
    /// its first caller or lost its last one.
    fn delegation_changes(
        &self,
        before: &FxHashSet<FuncKey>,
        after: &FxHashSet<FuncKey>,
    ) -> Vec<PathBuf> {
        before
            .symmetric_difference(after)
            .filter_map(|k| self.index.file_of(k).map(Path::to_path_buf))
            .collect()
    }

    /// Re-analyze an edited file: update its summary (Early Cutoff when nothing changed) and
    /// return its diagnostics plus the other files that need re-analysis.
    pub fn edit(&mut self, path: &Path, text: &str) -> Result<EditOutcome, String> {
        let parsed = self.parse(path, text)?;
        let calls_before = self.private_callees(&parsed.path);
        let (update, decls_changed) = self.summarize(&parsed);
        self.stats.edited_reanalyses += 1;
        self.stats.last_summary_changed = update.summary_changed;
        self.stats.last_decls_changed = decls_changed;
        let mut affected: FxHashSet<PathBuf> = update
            .affected_callers
            .iter()
            .filter_map(|k: &FuncKey| self.index.file_of(k).map(Path::to_path_buf))
            .collect();
        if decls_changed {
            affected.extend(self.dependents_of(&parsed.module_name, &parsed.path));
        }
        let calls_after = self.private_callees(&parsed.path);
        affected.extend(self.delegation_changes(&calls_before, &calls_after));
        affected.remove(&parsed.path);
        let mut affected: Vec<PathBuf> = affected.into_iter().collect();
        affected.sort();
        Ok(EditOutcome {
            diagnostics: self.check(&parsed)?,
            affected,
        })
    }

    /// Re-extract a dependent file (its imports' declarations or callees changed) and return
    /// its diagnostics. Counted as a re-analysis of another file.
    pub fn refresh(&mut self, path: &Path, text: &str) -> Result<Vec<Diagnostic>, String> {
        let parsed = self.parse(path, text)?;
        self.summarize(&parsed);
        self.stats.dependent_reanalyses += 1;
        self.check(&parsed)
    }

    /// Drop a deleted file; returns the files whose callees disappeared.
    pub fn remove(&mut self, path: &Path) -> Vec<PathBuf> {
        let Some(meta) = self.files.remove(path) else {
            return Vec::new();
        };
        let calls_before = self.private_callees(path);
        let update = self.index.remove_file(path);
        match self.index.module_decls(&meta.module_name) {
            Some(remaining) => self.decls.replace(remaining),
            None => self.decls.remove(&meta.module_name),
        }
        self.stats.indexed_files = self.index.files.len();
        let mut affected: FxHashSet<PathBuf> = update
            .affected_callers
            .iter()
            .filter_map(|k| self.index.file_of(k).map(Path::to_path_buf))
            .collect();
        affected.extend(self.dependents_of(&meta.module_name, path));
        affected.extend(self.delegation_changes(&calls_before, &FxHashSet::default()));
        let mut v: Vec<PathBuf> = affected.into_iter().collect();
        v.sort();
        v
    }

    /// Diagnostics of every indexed file, read from the disk.
    pub fn diagnose_all(&self) -> (BTreeMap<PathBuf, Vec<Diagnostic>>, Vec<String>) {
        let mut out = BTreeMap::new();
        let mut errors = Vec::new();
        for path in self.files.keys() {
            let text = match std::fs::read_to_string(path) {
                Ok(t) => t,
                Err(e) => {
                    errors.push(format!("{}: {e}", path.display()));
                    continue;
                }
            };
            match self.diagnose(path, &text) {
                Ok(d) => {
                    out.insert(path.clone(), d);
                }
                Err(e) => errors.push(e),
            }
        }
        (out, errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnose_all_reports_indexed_files_that_can_no_longer_be_read() {
        let dir = std::env::temp_dir().join(format!("heki_lsp_engine_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pyproject.toml"), "[project]\nname = \"t\"\n").unwrap();
        std::fs::write(dir.join("a.py"), "x = 1\n").unwrap();
        std::fs::write(dir.join("b.py"), "y = 2\n").unwrap();
        std::fs::write(dir.join("c.py"), "z = 3\n").unwrap();
        let paths = [dir.clone()];
        let opts = ProjectOptions {
            paths: &paths,
            ..ProjectOptions::default()
        };
        let (ws, errors) = Workspace::load(&opts, &Selectors::default()).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        // After indexing (a race a process cannot set up portably), `b.py` stops being UTF-8
        // and `c.py` stops parsing: both errors are reported, `a.py` is still checked.
        std::fs::write(dir.join("b.py"), b"\xff\xfe").unwrap();
        std::fs::write(dir.join("c.py"), "def f(:\n").unwrap();
        let (diags, errors) = ws.diagnose_all();
        assert_eq!(
            diags.keys().cloned().collect::<Vec<_>>(),
            vec![dir.join("a.py")]
        );
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(
            errors[0].starts_with(&format!("{}: ", dir.join("b.py").display()))
                && errors[0].contains("valid UTF-8"),
            "{errors:?}"
        );
        assert!(
            errors[1].starts_with(&format!("{}:1:", dir.join("c.py").display()))
                && errors[1].contains("syntax error"),
            "{errors:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
