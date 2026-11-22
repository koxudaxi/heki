//! Incremental engine behind `heki server` and `heki check --watch` (ADR-017).
//!
//! Resident state is the configuration, the knowledge bases, per-file metadata, and the
//! `ProjectIndex` (lightweight `FileSummary`s + reverse call graph). IR is built for one file at a
//! time and dropped as soon as its summary and diagnostics are computed.

use globset::GlobSet;
use heki_diagnostics::{Diagnostic, RatchetBaseline};
use heki_effect::{
    CallSite, DeclKeys, DeclPasses, FileSummary, FuncKey, ProjectIndex, SiteRole, UnreadModule,
    summarize_project_with_reads,
};
pub use heki_rules::Selectors;
use heki_rules::project::{
    Facts, FileError, IndexFacts, ParsedFile, Project, ProjectEnv, ProjectError, ProjectOptions,
    discovers, exclude_set, parse_source, retain_unsuppressed, unread_module,
};
use heki_rules::{
    CHAIN_RULES, ChainFacts, CheckExtras, RuleSelection, SNAPSHOT_FILE, SelectionError,
    SnapshotError, StaleWriteKey, SummarySnapshot, chain_facts, check_project, family_readers,
    find_rule, stale_writes_by_file,
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
    #[error(transparent)]
    Rules(heki_rules::RuleConfigError),
    #[error(transparent)]
    Baseline(Box<heki_diagnostics::BaselineError>),
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
    /// Where the spans of the file that other files' diagnostics can show are in its text
    /// ([`shown_places`]).
    shown_places: Vec<SitePlace>,
}

/// A span, the line and columns of both its ends (`(line, byte column, UTF-16 column)`, as the
/// CLI and the LSP show a related location), and what is there. A span can keep its bytes while
/// its line changes (a comment above it turned into a blank line), and two calls of one length
/// that swap keep every span while each call moves.
type SitePlace = (heki_ast::Span, [(u32, u32, u32); 2], ShownAt);

/// What is at a place other files' diagnostics can show.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ShownAt {
    /// A call site, with its project callees.
    Call(Vec<FuncKey>),
    /// A call of a function outside the project that an effect boundary forbids.
    ExternalCall(String),
    /// A method's own change of the container a `self` attribute holds.
    Change(String),
    /// A function's own in-place change of the object its parameter holds (the line `MUT110`
    /// names).
    ParamChange(u8),
}

/// The places in its text `source` of the spans of `file` that other files' diagnostics can
/// show: its call and reference sites and the calls outside the project an effect boundary
/// forbids (the hops of call chains), the changes its methods make to their own containers (the
/// methods `MUT113` names), and the in-place changes its functions make to their parameters (the
/// lines `MUT110` names).
fn shown_places(file: &heki_effect::FileSummary, source: &str) -> Vec<SitePlace> {
    let lines = heki_ast::LineIndex::from_source(source);
    let place = |offset: u32| {
        let (line, column) = lines.line_col(offset);
        let start = lines.line_start(line).unwrap_or(0) as usize;
        let utf16 = source
            .get(start..offset as usize)
            .map_or(0, |t| t.encode_utf16().count());
        (line, column, u32::try_from(utf16).unwrap_or(u32::MAX))
    };
    let mut changes: Vec<(heki_ast::Span, ShownAt)> = file
        .local_functions
        .values()
        .flat_map(|s| &s.iteration_facts().self_container_change_sites)
        .flat_map(|(attr, spans)| {
            spans
                .iter()
                .map(|span| (*span, ShownAt::Change(attr.clone())))
        })
        .chain(
            file.local_functions
                .values()
                .flat_map(|s| &s.param_inplace_sites)
                .flat_map(|(p, spans)| spans.iter().map(|span| (*span, ShownAt::ParamChange(*p)))),
        )
        .collect();
    changes.sort();
    let calls = file
        .call_sites
        .iter()
        .chain(&file.reference_sites)
        .map(|s| {
            let callees = s.project_callees().iter().map(|bc| bc.callee.clone());
            (s.span, ShownAt::Call(callees.collect()))
        });
    let mut external: Vec<(heki_ast::Span, ShownAt)> = file
        .external_accesses
        .values()
        .flatten()
        .filter(|a| a.kind == heki_effect::AccessKind::Call)
        .map(|a| (a.span, ShownAt::ExternalCall(a.name.clone())))
        .collect();
    external.sort();
    calls
        .chain(external)
        .chain(changes)
        .map(|(span, at)| (span, [place(span.start), place(span.end)], at))
        .collect()
}

/// Result of re-analyzing one file.
#[derive(Debug, Clone, Default)]
pub struct EditOutcome {
    pub diagnostics: Vec<Diagnostic>,
    /// Other files whose results may change: callers of changed summaries, files whose summary
    /// or check read changed declarations, files of functions whose callers here changed their
    /// calls, the class families of changed methods, the `MUT108` / `MUT110` reports above
    /// functions whose own in-place changes or calls their call chains walk changed, files whose
    /// diagnostics show call sites or container changes of this file that moved, and every
    /// checked file when the project starts or stops patching `builtins`.
    pub affected: Vec<PathBuf>,
    /// Why the file has no diagnostics: a syntax error (the file then stays in the index as a
    /// module heki knows nothing about, as a file `heki check` cannot parse does).
    pub error: Option<FileError>,
}

/// A project file as a re-analysis finds it.
#[derive(Debug)]
pub enum Source {
    /// Its text: an open document's, or the file's on the disk.
    Text(String),
    /// A file heki cannot read (its encoding, its permissions): a module heki knows nothing
    /// about, as for `heki check` ([`Workspace::unreadable`]).
    Unreadable(std::io::Error),
    /// No file any more ([`Workspace::remove`]).
    Gone,
}

impl Source {
    /// The file on the disk, decided by the read alone: only a read that finds no file says it
    /// is gone. Asking whether the path exists after a failed read would answer for another
    /// moment, and on Windows a deleted file stays visible until its last handle closes while
    /// every read of it fails.
    pub fn read(path: &Path) -> Self {
        match heki_ast::read_source(path) {
            Ok(text) => Self::Text(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::Gone,
            Err(e) => Self::Unreadable(e),
        }
    }
}

pub struct Workspace {
    pub env: ProjectEnv,
    selection: RuleSelection,
    snapshot: Option<SummarySnapshot>,
    /// The Ratchet baseline the diagnostics are compared with, and its file.
    baseline: Option<(PathBuf, RatchetBaseline)>,
    /// The directories discovery walks (`ProjectOptions::paths`).
    walked: Vec<PathBuf>,
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
    /// The heuristic reports of exact rules the latest check of each file withheld
    /// (`RuleSelection::withhold_heuristic`).
    withheld: FxHashMap<PathBuf, usize>,
    /// What the latest check of each file read of the declarations (what its summary read is
    /// in its `FileSummary`).
    check_reads: CheckReads,
    /// The indexed files that may patch `builtins` (they import it or touch `__builtins__`):
    /// while there is one, no module trusts builtin names (Assumption A1(b)).
    patching_builtins: FxHashSet<PathBuf>,
    /// The writes that leave a `@cached_property` stale (`MUT101`'s) in the methods of each file,
    /// as of the file's latest analysis; kept while `MUT111` is selected: the files reading a
    /// property whose writes change need a new check.
    stale_writes: FxHashMap<PathBuf, BTreeSet<StaleWriteKey>>,
    /// What the configured contracts read across functions (`Workspace::affected_by`).
    reach: ContractReach,
    stats: EngineStats,
}

/// What the latest check of each file read of the declarations ([`DeclKeys`]). A check reads
/// about every module and class of the project, so the keys as strings would grow with the
/// number of files times the number of declarations: every key is interned once, and a file
/// keeps the ids its check read.
#[derive(Debug, Default)]
struct CheckReads {
    /// Key ids, per `DeclKeys` field.
    ids: [FxHashMap<String, u32>; DECL_KEY_KINDS],
    files: FxHashMap<PathBuf, KeySet>,
}

/// The number of fields of [`DeclKeys`].
const DECL_KEY_KINDS: usize = 8;

impl CheckReads {
    /// Every field of `keys` (destructured without `..`: a new kind of key does not compile
    /// until it is kept here too, and the table of ids has one map per field).
    fn fields(keys: &DeclKeys) -> [&BTreeSet<String>; DECL_KEY_KINDS] {
        let DeclKeys {
            modules,
            subclasses,
            property_names,
            returning_names,
            class_facts,
            protocols,
            grown_globals,
            package_paths,
        } = keys;
        [
            modules,
            subclasses,
            property_names,
            returning_names,
            class_facts,
            protocols,
            grown_globals,
            package_paths,
        ]
    }

    fn insert(&mut self, path: PathBuf, keys: &DeclKeys) {
        let mut next = self.ids.iter().map(FxHashMap::len).sum::<usize>() as u32;
        let mut read = Vec::new();
        for (ids, field) in self.ids.iter_mut().zip(Self::fields(keys)) {
            for key in field {
                let id = *ids.entry(key.clone()).or_insert_with(|| {
                    next += 1;
                    next - 1
                });
                read.push(id);
            }
        }
        self.files.insert(path, KeySet::new(read));
    }

    fn remove(&mut self, path: &Path) {
        self.files.remove(path);
    }

    /// Every file with a recorded check.
    fn paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.files.keys()
    }

    /// The files whose latest check read a key of `change` (a key no check read has no id).
    fn reading<'a>(&'a self, change: &DeclKeys) -> impl Iterator<Item = &'a PathBuf> + 'a {
        let changed: Vec<u32> = self
            .ids
            .iter()
            .zip(Self::fields(change))
            .flat_map(|(ids, field)| field.iter().filter_map(|key| ids.get(key).copied()))
            .collect();
        self.files
            .iter()
            .filter(move |(_, read)| changed.iter().any(|&id| read.contains(id)))
            .map(|(path, _)| path)
    }
}

/// A set of key ids: a bitset when that is smaller (a check that read most keys), else the
/// sorted ids.
#[derive(Debug, Clone, PartialEq, Eq)]
enum KeySet {
    Sparse(Box<[u32]>),
    Dense(Box<[u64]>),
}

impl KeySet {
    fn new(mut ids: Vec<u32>) -> Self {
        ids.sort_unstable();
        let words = ids.last().map_or(0, |&max| max as usize / 64 + 1);
        if words * 2 < ids.len() {
            let mut bits = vec![0u64; words];
            for id in ids {
                bits[id as usize / 64] |= 1 << (id % 64);
            }
            Self::Dense(bits.into_boxed_slice())
        } else {
            Self::Sparse(ids.into_boxed_slice())
        }
    }

    fn contains(&self, id: u32) -> bool {
        match self {
            Self::Sparse(ids) => ids.binary_search(&id).is_ok(),
            Self::Dense(bits) => bits
                .get(id as usize / 64)
                .is_some_and(|w| w & (1 << (id % 64)) != 0),
        }
    }
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
    /// An effect boundary: a scope function's report names the ecosystem calls and the
    /// accesses outside the project it reaches any number of calls below (which
    /// `allowed-callees` may allow one by one) and counts them, while the summaries keep the
    /// effects with one chain each and no such access.
    ecosystem_calls: bool,
}

/// Per function of one file: its project call sites without positions, and the cached
/// properties it invalidates (see `Workspace::callee_files`).
type CallerView = BTreeMap<FuncKey, (Vec<CallSite>, BTreeSet<String>)>;

/// Per function of one file: its ecosystem (Layer 2) call sites and its accesses outside the
/// project that effect boundaries forbid, without positions.
type EcosystemView = BTreeMap<FuncKey, (Vec<CallSite>, Vec<heki_effect::ExternalAccess>)>;

impl Workspace {
    /// Index every Python file under `paths` (IR is dropped once the files are summarized, before
    /// propagation). `overrides` replaces the disk text of open documents. `baseline` replaces
    /// the Ratchet baseline of the configuration (`--baseline`). `facts`: [`Facts::Inspect`] for
    /// the language server, [`Facts::Check`] for `heki check --watch`.
    pub fn load(
        opts: &ProjectOptions<'_>,
        selectors: &Selectors,
        baseline: Option<&Path>,
        facts: Facts,
    ) -> Result<(Self, Vec<FileError>), EngineError> {
        let project = Project::load(opts).map_err(EngineError::Project)?;
        let env_cfg = &project.env.cfg;
        let selection =
            RuleSelection::resolve(&env_cfg.lint, selectors).map_err(EngineError::Selection)?;
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
        let baseline_path = baseline
            .map(Path::to_path_buf)
            .or_else(|| env_cfg.baseline.as_ref().map(|b| project.env.root.join(b)));
        let baseline = match baseline_path {
            Some(path) => {
                let recorded =
                    RatchetBaseline::load(&path).map_err(|e| EngineError::Baseline(Box::new(e)))?;
                Some((path, recorded))
            }
            None => None,
        };
        let exclude = exclude_set(env_cfg).map_err(EngineError::Project)?;
        let patching_builtins = project
            .files
            .iter()
            .filter(|f| f.patches_builtins())
            .map(|f| f.path.clone())
            .collect();
        let IndexFacts {
            contracts,
            typed_writes,
            opt_in,
        } = IndexFacts::of(facts, env_cfg, &project.env.kb, &selection)
            .map_err(EngineError::Rules)?;
        let summaries = summarize_project_with_reads(
            &project.inputs(),
            &project.unread,
            &project.env.kb,
            contracts.as_deref(),
            opt_in,
        );
        let unread = project.unread.iter().map(|m| {
            (
                m.path.clone(),
                FileMeta {
                    module_name: m.module_name.clone(),
                    shown_places: Vec::new(),
                },
            )
        });
        let files = project
            .files
            .iter()
            .zip(&summaries)
            .map(|(f, summary)| {
                (
                    f.path.clone(),
                    FileMeta {
                        module_name: f.module_name.clone(),
                        shown_places: shown_places(summary, &f.source),
                    },
                )
            })
            .chain(unread)
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
            ecosystem_calls: !env_cfg.effect_boundaries.is_empty(),
        };
        // Every file's IR, semantic model, and text go before propagation: only the index stays.
        let Project {
            env,
            files: parsed,
            errors,
            ..
        } = project;
        drop(parsed);
        let index = ProjectIndex::build_for(summaries, contracts, typed_writes, opt_in);
        let decls = index.decl_passes();
        // The options of the rules (contract targets and patterns) are read by every check, and
        // an invalid one fails them all: the workspace fails to load, as `heki check` fails
        // before it reports anything.
        let extras = CheckExtras {
            soundness_kb: &env.soundness_kb,
            snapshot: snapshot.as_ref(),
            decls: Some(decls.completed()),
            project_patches_builtins: Some(false),
            text_of: None,
            open_premises: false,
        };
        check_project(&[], &env.cfg, &env.kb, &index, &selection, &extras)
            .map_err(EngineError::Rules)?;
        let stale_writes = if selection.is_enabled("MUT111") {
            stale_writes_by_file(&index, decls.completed(), |_| true)
        } else {
            FxHashMap::default()
        };
        let stats = EngineStats {
            indexed_files: index.files.len(),
            ..EngineStats::default()
        };
        Ok((
            Self {
                env,
                selection,
                snapshot,
                baseline,
                walked: opts.paths.to_vec(),
                exclude,
                files,
                index,
                decls,
                shown: FxHashMap::default(),
                chain_reports: FxHashSet::default(),
                withheld: FxHashMap::default(),
                check_reads: CheckReads::default(),
                patching_builtins,
                stale_writes,
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

    pub fn selection(&self) -> &RuleSelection {
        &self.selection
    }

    /// The heuristic reports of exact rules the latest checks of the files withheld.
    pub fn withheld_heuristic(&self) -> usize {
        self.withheld.values().sum()
    }

    /// The files in the index.
    pub fn indexed(&self) -> impl Iterator<Item = &PathBuf> {
        self.files.keys()
    }

    /// The Ratchet baseline file the diagnostics are compared with: a change of it changes them
    /// all.
    pub fn baseline_path(&self) -> Option<&Path> {
        self.baseline.as_ref().map(|(path, _)| path.as_path())
    }

    /// A Python file the project indexes: one that discovery finds (`collect_files`: ignore
    /// files, hidden directories, and `exclude` leave files out), as `heki check` does.
    pub fn accepts(&self, path: &Path) -> bool {
        self.files.contains_key(path)
            || self
                .walked
                .iter()
                .any(|dir| discovers(dir, &self.env.root, &self.exclude, path))
    }

    fn parse(&self, path: &Path, text: &str) -> Result<ParsedFile, FileError> {
        parse_source(
            path,
            &self.env.root,
            text.to_owned(),
            &self.env.layout,
            self.env.runtime,
        )
    }

    /// Diagnostics of one parsed file against the current index and, as `heki check` reports
    /// them, against the Ratchet baseline; records which other files they point into and what
    /// the check read of the declarations.
    fn check(&mut self, parsed: &ParsedFile) -> Vec<Diagnostic> {
        let unit = parsed.unit();
        let extras = CheckExtras {
            soundness_kb: &self.env.soundness_kb,
            snapshot: self.snapshot.as_ref(),
            decls: Some(self.decls.completed()),
            project_patches_builtins: Some(!self.patching_builtins.is_empty()),
            text_of: Some(&|p: &Path| heki_ast::read_source(p).ok()),
            open_premises: false,
        };
        let (mut diags, reads) = self.decls.completed().record(|| {
            let mut diags = check_project(
                std::slice::from_ref(&unit),
                &self.env.cfg,
                &self.env.kb,
                &self.index,
                &self.selection,
                &extras,
            )
            .expect("a check fails only on invalid rule options, which fail `Workspace::load`");
            retain_unsuppressed(&mut diags, [parsed]);
            diags
        });
        // A withheld report counts: an edit to its call chain can make it exact.
        let chains = diags
            .iter()
            .any(|d| CHAIN_RULES.contains(&d.rule_code.as_str()));
        if chains {
            self.chain_reports.insert(parsed.path.clone());
        } else {
            self.chain_reports.remove(&parsed.path);
        }
        let withheld = self.selection.withhold_heuristic(&mut diags);
        self.withheld.insert(parsed.path.clone(), withheld);
        let shown = diags
            .iter()
            .flat_map(|d| &d.related)
            .filter(|r| r.path != parsed.path)
            .map(|r| r.path.clone())
            .collect();
        self.shown.insert(parsed.path.clone(), shown);
        self.check_reads.insert(parsed.path.clone(), &reads);
        // Baseline keys name the file, so the counts of one file are the counts of its keys.
        if let Some((_, baseline)) = &self.baseline {
            baseline.keep_regressions(&mut diags, &|_| parsed.rel.clone());
        }
        diags
    }

    /// Diagnostics of `text` as `path` against the current index (the summaries are unchanged).
    pub fn diagnose(&mut self, path: &Path, text: &str) -> Result<Vec<Diagnostic>, FileError> {
        let parsed = self.parse(path, text)?;
        Ok(self.check(&parsed))
    }

    /// Files other than `path` whose latest diagnostics point into `path`.
    fn showing(&self, path: &Path) -> Vec<PathBuf> {
        self.shown
            .iter()
            .filter(|(file, shown)| file.as_path() != path && shown.contains(path))
            .map(|(file, _)| file.clone())
            .collect()
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
        out.extend(self.check_reads.reading(change).cloned());
        out.into_iter().collect()
    }

    /// What the callees of `path`'s functions see of them: per function, its project call
    /// sites and constructions of project classes, and its references handing one on (without
    /// positions), and the cached properties it invalidates.
    fn caller_view(&self, path: &Path) -> CallerView {
        let mut out = CallerView::new();
        let Some(file) = self.index.files.get(path) else {
            return out;
        };
        for site in file
            .call_sites
            .iter()
            .filter(|s| !s.project_callees().is_empty() || s.constructs.is_some())
            .chain(&file.reference_sites)
        {
            out.entry(site.caller.clone())
                .or_default()
                .0
                .push(site.unplaced());
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

    /// What the functions of `path` call of the ecosystem and access outside the project
    /// (`EcosystemView`).
    fn ecosystem_view(&self, path: &Path) -> EcosystemView {
        let mut out = EcosystemView::new();
        let file = self.index.files.get(path);
        for site in file
            .into_iter()
            .flat_map(|f| &f.call_sites)
            .filter(|s| matches!(s.target, heki_effect::CallTarget::Layer2 { .. }))
        {
            out.entry(site.caller.clone())
                .or_default()
                .0
                .push(site.unplaced());
        }
        for (func, accesses) in file.into_iter().flat_map(|f| &f.external_accesses) {
            let unplaced = accesses.iter().map(|a| heki_effect::ExternalAccess {
                span: heki_ast::Span { start: 0, end: 0 },
                ..a.clone()
            });
            out.entry(func.clone()).or_default().1.extend(unplaced);
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

    /// Files defining classes whose `MUT101` / `MUT107` / `MUT112` / `MUT113` reports read the
    /// summaries of the `changed` methods (class families span modules), or the calls of
    /// `callees` (`MUT113` follows a generator's object through its callers): those of the
    /// functions whose call sites changed, before and after the change.
    fn family_files(&self, changed: &[FuncKey], callees: &[&FuncKey]) -> Vec<PathBuf> {
        let classes = family_readers(
            &self.index,
            self.decls.completed(),
            changed,
            callees,
            self.selection.is_enabled("MUT112"),
        );
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
        self.check_reads.paths().cloned().collect()
    }

    /// Record the stale writes (`MUT101`'s) in the methods of `path` against the current index;
    /// when they changed, the files reading the cached properties they leave stale (`MUT111`)
    /// may report differently: returns those files.
    fn note_stale_writes(&mut self, path: &Path) -> Vec<PathBuf> {
        if !self.selection.is_enabled("MUT111") {
            return Vec::new();
        }
        let now = stale_writes_by_file(&self.index, self.decls.completed(), |p| p == path)
            .remove(path)
            .unwrap_or_default();
        let before = if now.is_empty() {
            self.stale_writes.remove(path)
        } else {
            self.stale_writes.insert(path.to_path_buf(), now.clone())
        }
        .unwrap_or_default();
        let props: BTreeSet<&str> = before
            .symmetric_difference(&now)
            .map(|w| {
                w.prop
                    .qualname
                    .rsplit('.')
                    .next()
                    .unwrap_or(&w.prop.qualname)
            })
            .collect();
        if props.is_empty() {
            return Vec::new();
        }
        self.index
            .files
            .values()
            .filter(|f| {
                f.local_functions.values().any(|s| {
                    s.typed_field_read_sites
                        .iter()
                        .any(|r| props.contains(r.attr.as_str()))
                })
            })
            .map(|f| f.path.clone())
            .collect()
    }

    /// Re-summarize one parsed file; returns the update, what changed in the declarations, and
    /// whether the places other files' diagnostics can show changed ([`shown_places`]).
    fn summarize(
        &mut self,
        parsed: &ParsedFile,
    ) -> (heki_effect::IncrementalUpdate, DeclKeys, bool) {
        let out =
            self.index
                .reanalyze_module_cached(&parsed.input(), &self.env.kb, &mut self.decls);
        let places = self
            .index
            .files
            .get(&parsed.path)
            .map_or_else(Vec::new, |f| shown_places(f, &parsed.source));
        let before = self.files.insert(
            parsed.path.clone(),
            FileMeta {
                module_name: parsed.module_name.clone(),
                shown_places: places,
            },
        );
        let moved = before.is_none_or(|b| b.shown_places != self.files[&parsed.path].shown_places);
        self.stats.indexed_files = self.index.files.len();
        (out.0, out.1, moved)
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

    /// The files an update of the index affects besides the updated file: the files of changed
    /// summaries and of their callers, readers of changed declarations, callees whose callers changed, generators
    /// handed to changed functions, class families whose members changed, the files with
    /// `MUT108` / `MUT110` reports above functions whose chain facts (`chains`, before and
    /// after) changed, and with an effect boundary the files above functions whose ecosystem
    /// calls (`ecosystem`, before and after) changed.
    fn affected_by(
        &self,
        update: &heki_effect::IncrementalUpdate,
        change: &DeclKeys,
        callers: (&CallerView, &CallerView),
        chains: (&ChainFacts, &ChainFacts),
        ecosystem: (&EcosystemView, &EcosystemView),
    ) -> FxHashSet<PathBuf> {
        // The functions whose facts changed include the other members of a call cycle through
        // the file: their own reports (`MUT103` against the snapshot, ...) read them.
        let mut affected: FxHashSet<PathBuf> = update
            .affected_callers
            .iter()
            .chain(&update.changed)
            .filter_map(|k: &FuncKey| self.index.file_of(k).map(Path::to_path_buf))
            .collect();
        affected.extend(self.reading(change));
        affected.extend(self.callee_files(callers.0, callers.1));
        affected.extend(self.handed_over_files(&update.changed));
        let changed: Vec<&FuncKey> = callers
            .0
            .keys()
            .chain(callers.1.keys())
            .filter(|k| callers.0.get(*k) != callers.1.get(*k))
            .collect();
        let callees: Vec<&FuncKey> = changed
            .iter()
            .flat_map(|k| callers.0.get(*k).into_iter().chain(callers.1.get(*k)))
            .flat_map(|(sites, _)| sites)
            .flat_map(CallSite::project_callees)
            .map(|bc| &bc.callee)
            .collect();
        affected.extend(self.family_files(&update.changed, &callees));
        if self.reach.owners {
            affected.extend(self.below_files(&changed, callers));
        }
        if self.reach.calls {
            affected.extend(self.above_files(&changed));
        }
        if self.reach.ecosystem_calls {
            let calling_differently: Vec<&FuncKey> = ecosystem
                .0
                .keys()
                .chain(ecosystem.1.keys())
                .filter(|k| ecosystem.0.get(*k) != ecosystem.1.get(*k))
                .collect();
            affected.extend(self.above_files(&calling_differently));
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
    /// changed)`, or `None` when the file does not parse: it then stays in the index as a module
    /// heki knows nothing about ([`Self::unreadable`]).
    fn analyze(&mut self, path: &Path, text: &str) -> (EditOutcome, Option<(bool, bool)>) {
        let parsed = match self.parse(path, text) {
            Ok(parsed) => parsed,
            Err(e) => {
                let outcome = EditOutcome {
                    diagnostics: Vec::new(),
                    affected: self.unreadable(path),
                    error: Some(e),
                };
                return (outcome, None);
            }
        };
        let callers_before = self.caller_view(&parsed.path);
        let chains_before = chain_facts(&self.index, &parsed.path);
        let ecosystem_before = self.ecosystem_view(&parsed.path);
        let (update, change, places_moved) = self.summarize(&parsed);
        let callers_after = self.caller_view(&parsed.path);
        let chains_after = chain_facts(&self.index, &parsed.path);
        let ecosystem_after = self.ecosystem_view(&parsed.path);
        let mut affected = self.affected_by(
            &update,
            &change,
            (&callers_before, &callers_after),
            (&chains_before, &chains_after),
            (&ecosystem_before, &ecosystem_after),
        );
        affected.extend(self.note_builtins(&parsed.path, parsed.patches_builtins()));
        affected.extend(self.note_stale_writes(&parsed.path));
        // Call chains of other files show this file's call sites, and `MUT113` reports the
        // container changes of its methods: when they move, those diagnostics move with them
        // (the summaries copying call sites were re-propagated).
        if places_moved {
            affected.extend(self.showing(&parsed.path));
        }
        affected.remove(&parsed.path);
        let mut affected: Vec<PathBuf> = affected.into_iter().collect();
        affected.sort();
        let outcome = EditOutcome {
            diagnostics: self.check(&parsed),
            affected,
            error: None,
        };
        (outcome, Some((update.summary_changed, !change.is_empty())))
    }

    /// Re-analyze the `queued` files and, in turn, every file those re-analyses affect, until
    /// none is left. `source_of` reads a file (an open document's text, else the disk); a file
    /// it cannot read stays a module heki knows nothing about, and a file it finds gone leaves
    /// the project (`None` in the result for both). Returns the latest outcome of every file
    /// re-analyzed.
    ///
    /// It ends: re-analyzing a file whose inputs did not change since its last re-analysis
    /// changes nothing and affects no file, and each file's inputs change finitely often.
    pub fn refresh_all(
        &mut self,
        queued: impl IntoIterator<Item = PathBuf>,
        source_of: &dyn Fn(&Path) -> Source,
    ) -> BTreeMap<PathBuf, Option<EditOutcome>> {
        let mut queue: BTreeSet<PathBuf> = queued.into_iter().collect();
        let mut out = BTreeMap::new();
        while let Some(path) = queue.pop_first() {
            let outcome = match source_of(&path) {
                Source::Text(text) => {
                    let outcome = self.refresh(&path, &text);
                    queue.extend(outcome.affected.iter().cloned());
                    Some(outcome)
                }
                Source::Unreadable(_) => {
                    queue.extend(self.unreadable(&path));
                    None
                }
                Source::Gone => {
                    queue.extend(self.remove(&path));
                    None
                }
            };
            out.insert(path, outcome);
        }
        out
    }

    /// Drop a deleted file; returns the files it affects.
    pub fn remove(&mut self, path: &Path) -> Vec<PathBuf> {
        self.replace_file(path, None)
    }

    /// Keep a file of the project that heki cannot read or parse as a module it knows nothing
    /// about, as `heki check` does: a name under it resolves to nothing, not to the catalog.
    /// Returns the files it affects.
    pub fn unreadable(&mut self, path: &Path) -> Vec<PathBuf> {
        let module = unread_module(path, &self.env.root, &self.env.layout);
        self.replace_file(path, Some(module))
    }

    /// Take the facts of `path` out of the index, leaving the module heki could not read in
    /// their place (`unread`) or nothing; returns the files that affects.
    fn replace_file(&mut self, path: &Path, unread: Option<UnreadModule>) -> Vec<PathBuf> {
        self.shown.remove(path);
        self.chain_reports.remove(path);
        self.withheld.remove(path);
        self.check_reads.remove(path);
        let module_name = match &unread {
            Some(m) => {
                let meta = FileMeta {
                    module_name: m.module_name.clone(),
                    shown_places: Vec::new(),
                };
                self.files.insert(path.to_path_buf(), meta);
                m.module_name.clone()
            }
            None => match self.files.remove(path) {
                Some(meta) => meta.module_name,
                None => return Vec::new(),
            },
        };
        let callers_before = self.caller_view(path);
        let chains_before = chain_facts(&self.index, path);
        let ecosystem_before = self.ecosystem_view(path);
        let update = match &unread {
            Some(m) => self.index.update_file(FileSummary::unread(m)),
            None => self.index.remove_file(path),
        };
        let change = match self.index.module_decls(&module_name) {
            Some(owner) => self.decls.replace(owner),
            None => self.decls.remove(&module_name),
        };
        self.stats.indexed_files = self.index.files.len();
        let mut affected = self.affected_by(
            &update,
            &change,
            (&callers_before, &CallerView::new()),
            (&chains_before, &ChainFacts::new()),
            (&ecosystem_before, &EcosystemView::new()),
        );
        affected.extend(self.note_builtins(path, false));
        affected.extend(self.note_stale_writes(path));
        let mut v: Vec<PathBuf> = affected.into_iter().collect();
        v.sort();
        v
    }

    /// Diagnostics of every indexed file, read from the disk.
    pub fn diagnose_all(&mut self) -> (BTreeMap<PathBuf, Vec<Diagnostic>>, Vec<FileError>) {
        let mut out = BTreeMap::new();
        let mut errors = Vec::new();
        let paths: Vec<PathBuf> = self.files.keys().cloned().collect();
        for path in &paths {
            let diagnosed = heki_ast::read_source(path)
                .map_err(|e| FileError::unreadable(path, &e))
                .and_then(|text| self.diagnose(path, &text));
            match diagnosed {
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
    fn key_sets_hold_exactly_their_ids_in_either_form() {
        let cases: [(&[u32], bool); 5] = [
            (&[], false),
            (&[7], false),
            (&[3, 900, 41], false),
            (&[0, 1, 2, 63, 64, 5], true),
            (&[130, 2, 129, 128, 70, 71, 4], true),
        ];
        for (ids, dense) in cases {
            let set = KeySet::new(ids.to_vec());
            assert_eq!(matches!(set, KeySet::Dense(_)), dense, "{ids:?}");
            for id in 0..1000 {
                assert_eq!(set.contains(id), ids.contains(&id), "{ids:?} {id}");
            }
        }
    }

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
        let (mut ws, errors) =
            Workspace::load(&opts, &Selectors::default(), None, Facts::Check).unwrap();
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
        assert_eq!(
            (&errors[0].path, errors[0].position),
            (&dir.join("b.py"), None)
        );
        assert!(errors[0].message.contains("valid UTF-8"), "{errors:?}");
        assert_eq!(
            (&errors[1].path, errors[1].position),
            (&dir.join("c.py"), Some((1, 7)))
        );
        assert!(
            errors[1].message.starts_with("syntax error: "),
            "{errors:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
