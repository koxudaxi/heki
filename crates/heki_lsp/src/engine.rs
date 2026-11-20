//! Incremental engine behind `heki server` and `heki check --watch` (ADR-017).
//!
//! Resident state is the configuration, the knowledge bases, per-file metadata, and the
//! `ProjectIndex` (lightweight `FileSummary`s + reverse call graph). IR is built for one file at a
//! time and dropped as soon as its summary and diagnostics are computed.

use globset::GlobSet;
use heki_diagnostics::Diagnostic;
use heki_effect::{
    CallSite, DeclKeys, DeclPasses, FuncKey, ProjectIndex, SiteRole, analyze_project_with_reads,
};
use heki_rules::project::{
    ParsedFile, Project, ProjectEnv, ProjectError, ProjectOptions, exclude_set, parse_source,
    rel_posix, retain_unsuppressed,
};
use heki_rules::{
    CHAIN_RULES, ChainFacts, CheckExtras, RuleSelection, SNAPSHOT_FILE, SelectionError,
    SnapshotError, SummarySnapshot, chain_facts, check_project, family_readers, find_rule,
};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
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

/// The files of a project root whose change reloads the whole workspace: the configuration and
/// the summary snapshot (`MUT103`).
pub const CONFIG_FILES: &[&str] = &["pyproject.toml", "heki.toml", ".heki.toml", SNAPSHOT_FILE];

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

/// Result of re-analyzing one file.
#[derive(Debug, Clone, Default)]
pub struct EditOutcome {
    pub diagnostics: Vec<Diagnostic>,
    /// Other files whose results may change: callers of changed summaries, files whose summary
    /// or check read changed declarations, files of functions whose callers here changed their
    /// calls, the class families of changed methods, the `MUT108` / `MUT110` reports above
    /// functions whose own in-place changes or calls their call chains walk changed, files whose
    /// diagnostics show call sites of this file that moved, and every checked file when the
    /// project starts or stops patching `builtins`.
    pub affected: Vec<PathBuf>,
    /// Why the file has no diagnostics: a syntax error (the file then leaves the index, as a
    /// file `heki check` cannot parse is not part of the project) or an invalid rule option.
    pub error: Option<String>,
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
    /// Declarations of every indexed module (both passes), updated in place by edits.
    decls: DeclPasses,
    /// The other files the latest diagnostics of each file point into (related locations):
    /// when an edit moves call sites of one of them, those diagnostics change.
    shown: FxHashMap<PathBuf, FxHashSet<PathBuf>>,
    /// The files whose latest diagnostics include a report of `heki_rules::CHAIN_RULES`: its
    /// call chain walks the own in-place changes and calls of functions in other files
    /// (`heki_rules::ChainFacts`).
    chain_reports: FxHashSet<PathBuf>,
    /// What the latest check of each file read of the declarations (what its summary read is
    /// in its `FileSummary`).
    check_reads: FxHashMap<PathBuf, DeclKeys>,
    /// The indexed files that may patch `builtins` (they import it or touch `__builtins__`):
    /// while there is one, no module trusts builtin names (Assumption A1(b)).
    patching_builtins: FxHashSet<PathBuf>,
    /// What the configured contracts read across functions (`Workspace::affected_by`).
    reach: ContractReach,
    stats: EngineStats,
}

/// What the configured contracts read across functions besides the summaries of callees.
#[derive(Debug, Clone, Copy, Default)]
struct ContractReach {
    /// An owner contract (`write-owners`, `read-owners`, `call-owners`) or an `invalidation`
    /// contract: whether a function only runs inside allowed callers, or whether every caller
    /// path announces its write, is decided by walking every caller above it, so a change of
    /// the calls of a function can change the reports of everything below it.
    owners: bool,
    /// A contract on calls (`call-owners`, an effect boundary forbidding `calls`): a call of a
    /// target is reported at the callers of the private helper making it, or at a scope function
    /// any number of calls above it, and a call of a function without effects changes no
    /// summary.
    calls: bool,
}

/// Per function of one file: its project call sites without positions, and the cached
/// properties it invalidates (see `Workspace::callee_files`).
type CallerView = BTreeMap<FuncKey, (Vec<CallSite>, BTreeSet<String>)>;

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
        let patching_builtins = project
            .files
            .iter()
            .filter(|f| f.patches_builtins())
            .map(|f| f.path.clone())
            .collect();
        // A configuration error is reported by every check (`check_project`); without valid
        // contracts the summaries carry no contract facts.
        let contracts = heki_rules::summary_contracts(env_cfg).ok().flatten();
        let index = analyze_project_with_reads(&project.inputs(), &project.env.kb, contracts);
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
        let reach = ContractReach {
            owners: !env_cfg.write_owners.is_empty()
                || !env_cfg.read_owners.is_empty()
                || !env_cfg.call_owners.is_empty()
                || !env_cfg.invalidation.is_empty(),
            calls: !env_cfg.call_owners.is_empty()
                || env_cfg
                    .effect_boundaries
                    .iter()
                    .any(|b| b.forbid.iter().any(|t| t == "calls")),
        };
        let Project { env, errors, .. } = project;
        let decls = index.decl_passes();
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
                shown: FxHashMap::default(),
                chain_reports: FxHashSet::default(),
                check_reads: FxHashMap::default(),
                patching_builtins,
                reach,
                stats,
            },
            errors,
        ))
    }

    /// Every indexed function as `(file, qualname)` (summaries keep no definition lines).
    pub fn functions(&self) -> impl Iterator<Item = (&Path, &str)> {
        self.index.files.values().flat_map(|f| {
            f.local_functions
                .keys()
                .map(move |k| (f.path.as_path(), k.qualname.as_str()))
        })
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

    /// Diagnostics of one parsed file against the current index; records which other files
    /// they point into and what the check read of the declarations.
    fn check(&mut self, parsed: &ParsedFile) -> Result<Vec<Diagnostic>, String> {
        let unit = parsed.unit();
        let extras = CheckExtras {
            soundness_kb: &self.env.soundness_kb,
            snapshot: self.snapshot.as_ref(),
            decls: Some(self.decls.completed()),
            project_patches_builtins: Some(!self.patching_builtins.is_empty()),
            text_of: Some(&|p: &Path| heki_ast::read_source(p).ok()),
        };
        let (checked, reads) = self.decls.completed().record(|| {
            check_project(
                std::slice::from_ref(&unit),
                &self.env.cfg,
                &self.env.kb,
                &self.index,
                &self.selection,
                &extras,
            )
            .map_err(|e| e.to_string())
            .map(|mut diags| {
                retain_unsuppressed(&mut diags, [parsed]);
                diags
            })
        });
        let shown = checked
            .iter()
            .flatten()
            .flat_map(|d| &d.related)
            .filter(|r| r.path != parsed.path)
            .map(|r| r.path.clone())
            .collect();
        self.shown.insert(parsed.path.clone(), shown);
        let chains = checked
            .iter()
            .flatten()
            .any(|d| CHAIN_RULES.contains(&d.rule_code.as_str()));
        if chains {
            self.chain_reports.insert(parsed.path.clone());
        } else {
            self.chain_reports.remove(&parsed.path);
        }
        self.check_reads.insert(parsed.path.clone(), reads);
        checked
    }

    /// Diagnostics of `text` as `path` against the current index (the summaries are unchanged).
    pub fn diagnose(&mut self, path: &Path, text: &str) -> Result<Vec<Diagnostic>, String> {
        let parsed = self.parse(path, text)?;
        self.check(&parsed)
    }

    /// Files other than `path` whose latest diagnostics point into `path`.
    fn showing(&self, path: &Path) -> Vec<PathBuf> {
        self.shown
            .iter()
            .filter(|(file, shown)| file.as_path() != path && shown.contains(path))
            .map(|(file, _)| file.clone())
            .collect()
    }

    /// Spans of the call sites indexed for `path` (the positions other files' call chains
    /// show).
    fn call_site_spans(&self, path: &Path) -> Vec<heki_ast::Span> {
        self.index
            .files
            .get(path)
            .map(|f| f.call_sites.iter().map(|s| s.span).collect())
            .unwrap_or_default()
    }

    /// Files whose summary or latest check read declarations in `change` (a re-export, a
    /// class's methods, bases, or attribute types, a property getter, a subclass): their names
    /// may resolve differently now.
    fn reading(&self, change: &DeclKeys) -> Vec<PathBuf> {
        let mut out: FxHashSet<PathBuf> = self
            .index
            .files
            .values()
            .filter(|f| f.decl_reads.intersects(change))
            .map(|f| f.path.clone())
            .collect();
        out.extend(
            self.check_reads
                .iter()
                .filter(|(_, reads)| reads.intersects(change))
                .map(|(path, _)| path.clone()),
        );
        out.into_iter().collect()
    }

    /// What the callees of `path`'s functions see of them: per function, its project call
    /// sites and constructions of project classes (without positions) and the cached
    /// properties it invalidates.
    fn caller_view(&self, path: &Path) -> CallerView {
        let mut out = CallerView::new();
        let Some(file) = self.index.files.get(path) else {
            return out;
        };
        for site in file
            .call_sites
            .iter()
            .filter(|s| !s.project_callees().is_empty() || s.constructs.is_some())
        {
            let mut site = site.clone();
            site.span = heki_ast::Span { start: 0, end: 0 };
            out.entry(site.caller.clone()).or_default().0.push(site);
        }
        for (key, (_, invalidated)) in &mut out {
            *invalidated = self
                .index
                .summary(key)
                .map(|s| s.invalidates_cached.clone())
                .unwrap_or_default();
        }
        out
    }

    /// Files of the functions whose callers changed what they see of them: calls gained, lost,
    /// or bound differently (a function's `MUT201` / `MUT203` reports depend on the contexts
    /// it runs in), or the cached properties a caller invalidates (`MUT101`).
    fn callee_files(&self, before: &CallerView, after: &CallerView) -> Vec<PathBuf> {
        let mut out = FxHashSet::default();
        for caller in before.keys().chain(after.keys()) {
            let (old, new) = (before.get(caller), after.get(caller));
            if old == new {
                continue;
            }
            for (sites, _) in old.into_iter().chain(new) {
                out.extend(
                    sites
                        .iter()
                        .flat_map(CallSite::project_callees)
                        .filter_map(|bc| self.index.file_of(&bc.callee).map(Path::to_path_buf)),
                );
            }
        }
        out.into_iter().collect()
    }

    /// Files defining classes whose `MUT101` / `MUT107` reports read the summaries of the
    /// `changed` methods (class families span modules).
    fn family_files(&self, changed: &[FuncKey]) -> Vec<PathBuf> {
        let classes = family_readers(&self.index, self.decls.completed(), changed);
        self.index
            .files
            .values()
            .filter(|f| {
                f.decls
                    .classes
                    .keys()
                    .any(|q| classes.contains(&format!("{}.{q}", f.module_name)))
            })
            .map(|f| f.path.clone())
            .collect()
    }

    /// Record whether `path` patches `builtins`; when that changes whether the project does,
    /// every checked file's `OPT` diagnostics may change: returns those files.
    fn note_builtins(&mut self, path: &Path, patches: bool) -> Vec<PathBuf> {
        let before = self.patching_builtins.is_empty();
        if patches {
            self.patching_builtins.insert(path.to_path_buf());
        } else {
            self.patching_builtins.remove(path);
        }
        if before == self.patching_builtins.is_empty() {
            return Vec::new();
        }
        self.check_reads.keys().cloned().collect()
    }

    /// Re-summarize one parsed file; returns the update and what changed in the declarations.
    fn summarize(&mut self, parsed: &ParsedFile) -> (heki_effect::IncrementalUpdate, DeclKeys) {
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

    /// Files of the generators handed to the `changed` functions (`SiteRole::Through`): whether
    /// such a call runs a generator depends on the function iterating the parameter it is
    /// handed, and so do the generator's `MUT201` / `MUT203` reports (it hands them only to
    /// callers that run it).
    fn handed_over_files(&self, changed: &[FuncKey]) -> Vec<PathBuf> {
        let mut out = FxHashSet::default();
        for p in changed {
            let sites = self
                .index
                .reverse_call_graph
                .get(p)
                .into_iter()
                .flatten()
                .flat_map(|caller| {
                    self.index
                        .file_of(caller)
                        .and_then(|f| self.index.files.get(f))
                        .into_iter()
                        .flat_map(|file| &file.call_sites)
                        .filter(move |s| &s.caller == caller)
                });
            let handed = |s: &&CallSite| matches!(&s.role, SiteRole::Through { via, .. } if via.iter().any(|(k, _)| k == p));
            for site in sites.filter(handed) {
                out.extend(
                    site.project_callees()
                        .iter()
                        .filter_map(|bc| self.index.file_of(&bc.callee).map(Path::to_path_buf)),
                );
            }
        }
        out.into_iter().collect()
    }

    /// The files an update of the index affects besides the updated file: callers of changed
    /// summaries, readers of changed declarations, callees whose callers changed, generators
    /// handed to changed functions, class families whose members changed, and the files with
    /// `MUT108` / `MUT110` reports above functions whose chain facts (`chains`, before and
    /// after) changed.
    fn affected_by(
        &self,
        update: &heki_effect::IncrementalUpdate,
        change: &DeclKeys,
        callers: (&CallerView, &CallerView),
        chains: (&ChainFacts, &ChainFacts),
    ) -> FxHashSet<PathBuf> {
        let mut affected: FxHashSet<PathBuf> = update
            .affected_callers
            .iter()
            .filter_map(|k: &FuncKey| self.index.file_of(k).map(Path::to_path_buf))
            .collect();
        affected.extend(self.reading(change));
        affected.extend(self.callee_files(callers.0, callers.1));
        affected.extend(self.handed_over_files(&update.changed));
        affected.extend(self.family_files(&update.changed));
        let changed: Vec<&FuncKey> = callers
            .0
            .keys()
            .chain(callers.1.keys())
            .filter(|k| callers.0.get(*k) != callers.1.get(*k))
            .collect();
        if self.reach.owners {
            affected.extend(self.below_files(&changed, callers));
        }
        if self.reach.calls {
            affected.extend(self.above_files(&changed));
        }
        // A `MUT108` / `MUT110` call chain ends at the first function whose own body makes the
        // change and walks the calls of the functions before it: when a function moves the
        // change to or from a function it calls, or changes those calls, the chains of the
        // reports above it change while no summary does (and no report appears or disappears).
        let walked: Vec<&FuncKey> = chains
            .0
            .keys()
            .chain(chains.1.keys())
            .filter(|k| chains.0.get(*k) != chains.1.get(*k))
            .collect();
        if !walked.is_empty() && !self.chain_reports.is_empty() {
            let above = self.above_files(&walked);
            affected.extend(above.into_iter().filter(|f| self.chain_reports.contains(f)));
        }
        affected
    }

    /// Files of every function below the `changed` callers (their callees before and after
    /// the edit, and transitively the callees of those): the allowed-context walk of the owner
    /// contracts and the announcement walk of `invalidation` contracts read the calls of every
    /// caller above a function.
    fn below_files(
        &self,
        changed: &[&FuncKey],
        callers: (&CallerView, &CallerView),
    ) -> FxHashSet<PathBuf> {
        let mut out = FxHashSet::default();
        if changed.is_empty() {
            return out;
        }
        let mut below: FxHashMap<&FuncKey, Vec<&FuncKey>> = FxHashMap::default();
        for site in self.index.files.values().flat_map(|f| &f.call_sites) {
            below
                .entry(&site.caller)
                .or_default()
                .extend(site.project_callees().iter().map(|bc| &bc.callee));
        }
        let mut queue: Vec<&FuncKey> = changed
            .iter()
            .flat_map(|k| [callers.0.get(*k), callers.1.get(*k)])
            .flatten()
            .flat_map(|(sites, _)| sites.iter().flat_map(CallSite::project_callees))
            .map(|bc| &bc.callee)
            .collect();
        let mut seen: FxHashSet<&FuncKey> = queue.iter().copied().collect();
        while let Some(func) = queue.pop() {
            out.extend(self.index.file_of(func).map(Path::to_path_buf));
            for next in below.get(func).into_iter().flatten() {
                if seen.insert(next) {
                    queue.push(next);
                }
            }
        }
        out
    }

    /// Files of every function above the `changed` callers on the reverse call graph: their
    /// calls of `call-owners` targets are reported there when they are private helpers, and
    /// effect boundaries forbidding `calls` reach them from any scope function above.
    fn above_files(&self, changed: &[&FuncKey]) -> FxHashSet<PathBuf> {
        let mut queue: Vec<&FuncKey> = changed.to_vec();
        let mut seen: FxHashSet<&FuncKey> = queue.iter().copied().collect();
        let mut out = FxHashSet::default();
        while let Some(func) = queue.pop() {
            for caller in self
                .index
                .reverse_call_graph
                .get(func)
                .into_iter()
                .flatten()
            {
                if seen.insert(caller) {
                    out.extend(self.index.file_of(caller).map(Path::to_path_buf));
                    queue.push(caller);
                }
            }
        }
        out
    }

    /// Re-analyze an edited file: update its summary (Early Cutoff when no fact changed) and
    /// return its diagnostics plus the other files that need re-analysis.
    pub fn edit(&mut self, path: &Path, text: &str) -> EditOutcome {
        let (outcome, update) = self.analyze(path, text);
        if let Some((summary_changed, decls_changed)) = update {
            self.stats.edited_reanalyses += 1;
            self.stats.last_summary_changed = summary_changed;
            self.stats.last_decls_changed = decls_changed;
        }
        outcome
    }

    /// Re-analyze a file an edit of another file affects (its callees, the declarations it
    /// resolves names through, or the call sites its diagnostics show changed). Counted as a
    /// re-analysis of another file; what it affects in turn is in the outcome too.
    pub fn refresh(&mut self, path: &Path, text: &str) -> EditOutcome {
        let (outcome, update) = self.analyze(path, text);
        if update.is_some() {
            self.stats.dependent_reanalyses += 1;
        }
        outcome
    }

    /// Re-summarize one file and check it. The second value is `(summary changed, declarations
    /// changed)`, or `None` when the file does not parse: it then leaves the index.
    fn analyze(&mut self, path: &Path, text: &str) -> (EditOutcome, Option<(bool, bool)>) {
        let parsed = match self.parse(path, text) {
            Ok(parsed) => parsed,
            Err(e) => {
                let outcome = EditOutcome {
                    diagnostics: Vec::new(),
                    affected: self.remove(path),
                    error: Some(e),
                };
                return (outcome, None);
            }
        };
        let callers_before = self.caller_view(&parsed.path);
        let chains_before = chain_facts(&self.index, &parsed.path);
        let sites_before = self.call_site_spans(&parsed.path);
        let (update, change) = self.summarize(&parsed);
        let callers_after = self.caller_view(&parsed.path);
        let chains_after = chain_facts(&self.index, &parsed.path);
        let mut affected = self.affected_by(
            &update,
            &change,
            (&callers_before, &callers_after),
            (&chains_before, &chains_after),
        );
        affected.extend(self.note_builtins(&parsed.path, parsed.patches_builtins()));
        // Call chains of other files show this file's call sites: when they move, those
        // diagnostics move with them (the summaries copying them were re-propagated).
        if self.call_site_spans(&parsed.path) != sites_before {
            affected.extend(self.showing(&parsed.path));
        }
        affected.remove(&parsed.path);
        let mut affected: Vec<PathBuf> = affected.into_iter().collect();
        affected.sort();
        let (diagnostics, error) = match self.check(&parsed) {
            Ok(diagnostics) => (diagnostics, None),
            Err(e) => (Vec::new(), Some(e)),
        };
        let outcome = EditOutcome {
            diagnostics,
            affected,
            error,
        };
        (outcome, Some((update.summary_changed, !change.is_empty())))
    }

    /// Re-analyze the `queued` files and, in turn, every file those re-analyses affect, until
    /// none is left. `text_of` reads a file (an open document's text, else the disk); a file
    /// it cannot read left the project and is removed (`None` in the result). Returns the
    /// latest outcome of every file re-analyzed.
    ///
    /// It ends: re-analyzing a file whose inputs did not change since its last re-analysis
    /// changes nothing and affects no file, and each file's inputs change finitely often.
    pub fn refresh_all(
        &mut self,
        queued: impl IntoIterator<Item = PathBuf>,
        text_of: &dyn Fn(&Path) -> Option<String>,
    ) -> BTreeMap<PathBuf, Option<EditOutcome>> {
        let mut queue: BTreeSet<PathBuf> = queued.into_iter().collect();
        let mut out = BTreeMap::new();
        while let Some(path) = queue.pop_first() {
            let outcome = match text_of(&path) {
                Some(text) => {
                    let outcome = self.refresh(&path, &text);
                    queue.extend(outcome.affected.iter().cloned());
                    Some(outcome)
                }
                None => {
                    queue.extend(self.remove(&path));
                    None
                }
            };
            out.insert(path, outcome);
        }
        out
    }

    /// Drop a deleted file (or one that no longer parses); returns the files it affects.
    pub fn remove(&mut self, path: &Path) -> Vec<PathBuf> {
        self.shown.remove(path);
        self.chain_reports.remove(path);
        self.check_reads.remove(path);
        let Some(meta) = self.files.remove(path) else {
            return Vec::new();
        };
        let callers_before = self.caller_view(path);
        let chains_before = chain_facts(&self.index, path);
        let update = self.index.remove_file(path);
        let change = match self.index.module_decls(&meta.module_name) {
            Some(remaining) => self.decls.replace(remaining),
            None => self.decls.remove(&meta.module_name),
        };
        self.stats.indexed_files = self.index.files.len();
        let mut affected = self.affected_by(
            &update,
            &change,
            (&callers_before, &CallerView::new()),
            (&chains_before, &ChainFacts::new()),
        );
        affected.extend(self.note_builtins(path, false));
        let mut v: Vec<PathBuf> = affected.into_iter().collect();
        v.sort();
        v
    }

    /// Diagnostics of every indexed file, read from the disk.
    pub fn diagnose_all(&mut self) -> (BTreeMap<PathBuf, Vec<Diagnostic>>, Vec<String>) {
        let mut out = BTreeMap::new();
        let mut errors = Vec::new();
        let paths: Vec<PathBuf> = self.files.keys().cloned().collect();
        for path in &paths {
            let text = match heki_ast::read_source(path) {
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
        let (mut ws, errors) = Workspace::load(&opts, &Selectors::default()).unwrap();
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
