//! "What heki cannot see": where heki is silent, so no count of reports applies. The calls of
//! library names the ecosystem catalog has no entry for, with what `heki check` would report if
//! each waited or sent a database query (the hypothetical-catalog run); the calls heki does not
//! resolve, with why; the directories that, made source roots, resolve imports that name no
//! project module now (the source-root trial); and the functions with an A1 hazard.
//!
//! A library entry never enters the block: heki cannot verify what a library call does (ADR-020,
//! rule 3). A source root enters it only after its trial.

use super::{ISSUES_URL, MAX_EXAMPLES, MAX_LISTED, RecommendError, analyze, printed};
use crate::DottedPattern;
use crate::blind_spots::{BlindSite, BlindSpots, CallCounts};
use crate::contracts::{ConfiguredName, configured_names};
use crate::project::{ParsedFile, Project, display_path, module_of, parse_source, rel_posix};
use heki_ast::LineIndex;
use heki_diagnostics::{Certainty, RatchetBaseline};
use heki_effect::{
    ModuleLayout, ProjectIndex, is_source_dir, is_stub, module_name_for_path, module_precedence,
    module_prefix, resolve_relative_module,
};
use heki_env::EcosystemOverride;
use heki_semantic::ModuleImport;
use rustc_hash::{FxHashMap, FxHashSet};
use schemars::JsonSchema;
use serde::Serialize;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The most library names the hypothetical-catalog run gives an entry: those with the most
/// reach.
pub const MAX_TRIED_SYMBOLS: usize = 64;

/// The effects the hypothetical-catalog run gives each name: a wait on the network (`EFF101`)
/// and a database query (`EFF103`).
pub const HYPOTHETICAL_EFFECTS: [&str; 2] = ["blocking_net", "db_query"];

/// The rules the hypothetical-catalog run selects, by name, so that it prints their heuristic
/// reports too.
const HYPOTHETICAL_RULES: [&str; 2] = ["EFF101", "EFF103"];

/// The most directories the source-root trial tries, those resolving the most imports first.
pub const MAX_SOURCE_ROOT_TRIALS: usize = 8;

/// The most rows a table of the text prints.
pub const MAX_TEXT_ROWS: usize = 10;

/// What heki cannot see.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct BlindSpotsSection {
    /// The calls of names of packages the ecosystem catalog does not cover, by package, the
    /// package with the most reach first: at most 50.
    pub uncatalogued: Vec<PackageCalls>,
    /// Names of packages the catalog covers that it has no entry for, which `heki check` would
    /// report if they waited or sent a query: at most 50.
    pub catalog_may_miss: Vec<SymbolCalls>,
    /// The calls heki resolves to no function, by function and reason, the most reach first: at
    /// most 50.
    pub unresolved: Vec<UnresolvedCalls>,
    /// Directories that resolve imports naming no project module once they are source roots,
    /// the most imports first: at most 50.
    pub source_roots: Vec<SourceRoot>,
    pub a1_hazards: A1Hazards,
    pub totals: Totals,
    /// Whether a list holds fewer entries than there are.
    pub truncated: bool,
}

/// The calls of the names of one package.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PackageCalls {
    pub package: String,
    #[serde(flatten)]
    pub counts: Counts,
    /// The names called, the one with the most reach first.
    pub symbols: Vec<SymbolCalls>,
}

/// How many calls there are, and what they may hold up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Counts {
    pub calls: usize,
    /// The calls that are awaited: whatever they run, they do not hold up the event loop.
    pub awaited: usize,
    /// The calls, not awaited, in a function on an event loop (an `async def`, or a function
    /// one runs on its thread): one that waits holds the loop up.
    pub on_event_loop: usize,
    /// The calls inside a loop of their function, or in a function below a loop.
    pub in_loops: usize,
}

impl From<CallCounts> for Counts {
    fn from(c: CallCounts) -> Self {
        Self {
            calls: c.calls,
            awaited: c.awaited,
            on_event_loop: c.on_event_loop,
            in_loops: c.in_loops,
        }
    }
}

impl Counts {
    /// The order of reach: the calls that may hold up an event loop, then those in loops, then
    /// all, the most first.
    fn reach(&self) -> impl Ord {
        (
            std::cmp::Reverse(self.on_event_loop),
            std::cmp::Reverse(self.in_loops),
            std::cmp::Reverse(self.calls),
        )
    }
}

/// The calls of one library name.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct SymbolCalls {
    /// Spelled as an entry of `[[tool.heki.ecosystem]]` needs it: entries are looked up by this
    /// exact name, without inheritance or aliases.
    pub symbol: String,
    #[serde(flatten)]
    pub counts: Counts,
    /// The `EFF101` reports `heki check` would print if the call waited on the network, from
    /// the hypothetical-catalog run (`null`: the name was not tried).
    pub if_waits: Option<TrialReports>,
    /// The `EFF103` reports `heki check` would print if the call sent a database query (`null`:
    /// not tried).
    pub if_queries: Option<TrialReports>,
    /// The first 50 calls.
    pub examples: Vec<Site>,
}

/// The reports of one rule the hypothetical-catalog run printed, by certainty.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct TrialReports {
    pub exact: usize,
    pub heuristic: usize,
}

impl TrialReports {
    fn total(self) -> usize {
        self.exact + self.heuristic
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Site {
    pub path: String,
    pub line: u32,
    pub column: u32,
    /// The function making the call.
    pub function: String,
}

/// The calls one function makes that heki resolves to no function, for one reason.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct UnresolvedCalls {
    pub function: String,
    pub path: String,
    /// Why: `unannotated-parameter`, `unresolved-annotation`, `any-value`, `callable-value`,
    /// `unbound-name`, `unlisted-protocol`, `call-result`, `attribute`, `computed-name`,
    /// `too-many-implementations`, `unknown-member`, or `untyped-value`.
    pub reason: String,
    /// The parameter, for a reason about one.
    pub parameter: Option<String>,
    /// The methods and names called, each once, sorted.
    pub names: Vec<String>,
    #[serde(flatten)]
    pub counts: Counts,
    /// The first 50 calls.
    pub examples: Vec<Site>,
}

/// A directory that, made a source root, resolves imports that name no project module now.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct SourceRoot {
    /// Relative to the project root.
    pub directory: String,
    /// The imports of its files that name no project module now.
    pub imports: usize,
    /// The first 50 of them.
    pub examples: Vec<ImportAt>,
    /// Whether the block adds it to `src`.
    pub in_block: bool,
    /// Why it is in the block or not.
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ImportAt {
    pub path: String,
    pub line: u32,
    /// The module or name the import names.
    pub name: String,
}

/// The functions where assumption A1 does not hold (`heki assumptions` lists them).
#[derive(Debug, Clone, Copy, Default, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct A1Hazards {
    /// The functions with an A1 hazard: the Soundness lane proves nothing in them.
    pub functions: usize,
    /// Of those, the functions too large to model at all.
    pub unmodeled: usize,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Totals {
    /// The calls of names of packages the catalog does not cover.
    pub uncatalogued: Counts,
    pub uncatalogued_packages: usize,
    pub uncatalogued_symbols: usize,
    /// The calls of names of packages the catalog covers, without an entry.
    pub registered_calls: usize,
    pub unresolved: Counts,
    /// The unresolved calls by reason.
    pub unresolved_by_reason: BTreeMap<String, usize>,
    /// The functions making a call heki resolves to no function.
    pub unresolved_functions: usize,
    /// The groups of those calls by function and reason.
    pub unresolved_groups: usize,
    /// The directories of `source_roots`, those past the first 50 included.
    pub source_root_candidates: usize,
    /// The calls of names below a directory of `source_roots`: calls of the project's files,
    /// which the library names leave out.
    pub below_source_roots: usize,
    /// The library names the hypothetical-catalog run gave an entry.
    pub tried: usize,
    /// The library names it did not: past the first 64 by reach.
    pub not_tried: usize,
}

/// The blind calls of the discovery run's index, as the section lists them, before the trials.
pub(super) struct Discovered {
    /// Every library name without an entry, registered package or not, by package then name.
    symbols: Vec<(bool, SymbolCalls)>,
    pub unresolved: Vec<UnresolvedCalls>,
    pub totals: Totals,
}

impl Discovered {
    /// The blind calls `index` records ([`crate::recommend_facts`]) in `files`, with paths
    /// written by `shown`.
    pub fn of(index: &ProjectIndex, files: &[ParsedFile], shown: &dyn Fn(&Path) -> String) -> Self {
        let lines: FxHashMap<&Path, LineIndex> = files
            .iter()
            .map(|f| (f.path.as_path(), LineIndex::from_source(&f.source)))
            .collect();
        let site = |s: &BlindSite<'_>| {
            let (line, column) = lines
                .get(s.path)
                .map_or((0, 0), |l| l.line_col(s.span.start));
            Site {
                path: shown(s.path),
                line,
                column,
                function: s.function.fqn(),
            }
        };
        let examples = |sites: &[BlindSite<'_>]| sites.iter().take(MAX_LISTED).map(site).collect();
        let spots = BlindSpots::of(index);
        let t = spots.totals();
        let mut symbols = Vec::new();
        for package in &spots.uncatalogued {
            for s in &package.symbols {
                symbols.push((
                    package.registered,
                    SymbolCalls {
                        symbol: s.symbol.to_owned(),
                        counts: s.counts.into(),
                        if_waits: None,
                        if_queries: None,
                        examples: examples(&s.sites),
                    },
                ));
            }
        }
        let mut unresolved: Vec<UnresolvedCalls> = spots
            .unresolved
            .iter()
            .map(|u| UnresolvedCalls {
                function: u.function.fqn(),
                path: shown(u.path),
                reason: u.reason.as_str().to_owned(),
                parameter: u.reason.parameter().map(str::to_owned),
                names: u.names.iter().map(|n| (*n).to_owned()).collect(),
                counts: u.counts.into(),
                examples: examples(&u.sites),
            })
            .collect();
        unresolved.sort_by(|a, b| {
            (a.counts.reach(), &a.function, &a.reason).cmp(&(
                b.counts.reach(),
                &b.function,
                &b.reason,
            ))
        });
        let unresolved_total = unresolved.len();
        unresolved.truncate(MAX_LISTED);
        let totals = Totals {
            uncatalogued: Counts::default(),
            uncatalogued_packages: 0,
            uncatalogued_symbols: 0,
            registered_calls: t.registered_calls,
            unresolved: t.unresolved.into(),
            unresolved_by_reason: t
                .unresolved_by_reason
                .into_iter()
                .map(|(reason, calls)| (reason.to_owned(), calls))
                .collect(),
            unresolved_functions: spots
                .unresolved
                .iter()
                .map(|u| u.function)
                .collect::<BTreeSet<_>>()
                .len(),
            unresolved_groups: unresolved_total,
            source_root_candidates: 0,
            below_source_roots: 0,
            tried: 0,
            not_tried: 0,
        };
        Self {
            symbols,
            unresolved,
            totals,
        }
    }

    /// The names the hypothetical-catalog run tries: the first [`MAX_TRIED_SYMBOLS`] by reach,
    /// leaving out the names of the project's files below a directory of `roots`.
    pub fn to_try(&self, roots: &SourceRoots) -> Vec<String> {
        let mut ranked: Vec<&SymbolCalls> = self
            .symbols
            .iter()
            .map(|(_, s)| s)
            .filter(|s| !roots.names_project_file(&s.symbol))
            .collect();
        ranked.sort_by(|a, b| (a.counts.reach(), &a.symbol).cmp(&(b.counts.reach(), &b.symbol)));
        ranked
            .into_iter()
            .take(MAX_TRIED_SYMBOLS)
            .map(|s| s.symbol.clone())
            .collect()
    }
}

/// What the hypothetical-catalog run gives each name it tried.
#[derive(Debug, Default)]
pub(super) struct Trial {
    tried: BTreeSet<String>,
    /// The `EFF101` and the `EFF103` reports of each name.
    found: BTreeMap<String, (TrialReports, TrialReports)>,
    /// A report named several of the names tried: it counts for the first.
    several: bool,
}

/// The hypothetical-catalog run: `project` analyzed ([`analyze`]) with an entry of
/// [`HYPOTHETICAL_EFFECTS`] for each of `tried` added to its configuration, running the rules
/// those effects make report. Each report counts for the name tried that its chain ends at (the
/// first by name when it names several). Returns the project with its configuration as it was.
pub(super) fn hypothetical_catalog(
    project: Project,
    tried: Vec<String>,
) -> Result<(Project, Trial), RecommendError> {
    let mut cfg = project.env.cfg.clone();
    cfg.ecosystem
        .extend(tried.iter().map(|symbol| EcosystemOverride {
            symbol: symbol.clone(),
            effects: Some(HYPOTHETICAL_EFFECTS.map(str::to_owned).to_vec()),
            ..EcosystemOverride::default()
        }));
    let (project, diags, _) = analyze(project, cfg, &HYPOTHETICAL_RULES)?;
    let mut trial = Trial {
        tried: tried.into_iter().collect(),
        ..Trial::default()
    };
    for d in printed(diags) {
        let mut named = d
            .target
            .split(", ")
            .filter(|name| trial.tried.contains(*name));
        let Some(first) = named.next() else {
            continue;
        };
        trial.several |= named.next().is_some();
        let (waits, queries) = trial.found.entry(first.to_owned()).or_default();
        // The run selects these two rules only.
        let rule = if d.rule_code == "EFF101" {
            waits
        } else {
            queries
        };
        match d.certainty {
            Certainty::Exact => rule.exact += 1,
            Certainty::Heuristic => rule.heuristic += 1,
        }
    }
    Ok((project, trial))
}

impl Discovered {
    /// The section, with what `trial` found for the names it tried, the source roots `roots`
    /// tried, and the functions with an A1 hazard in `files`. The calls of names of the
    /// project's files below a directory of `roots` are left out of the library names: heki
    /// takes them for a library's only because the directory is not a source root.
    pub fn finish(
        self,
        trial: &Trial,
        roots: SourceRoots,
        files: &[ParsedFile],
    ) -> BlindSpotsSection {
        let mut totals = self.totals;
        let mut registered = Vec::new();
        let mut packages: BTreeMap<String, Vec<SymbolCalls>> = BTreeMap::new();
        for (in_registered, mut s) in self.symbols {
            if roots.names_project_file(&s.symbol) {
                totals.below_source_roots += s.counts.calls;
                continue;
            }
            if trial.tried.contains(&s.symbol) {
                let found = trial.found.get(&s.symbol).copied().unwrap_or_default();
                (s.if_waits, s.if_queries) = (Some(found.0), Some(found.1));
                totals.tried += 1;
            } else {
                totals.not_tried += 1;
            }
            if in_registered {
                registered.push(s);
            } else {
                let package = s.symbol.split('.').next().unwrap_or(&s.symbol).to_owned();
                packages.entry(package).or_default().push(s);
            }
        }
        totals.source_root_candidates = roots.total;
        let mut truncated = totals.unresolved_groups > self.unresolved.len()
            || roots.total > roots.candidates.len();
        let mut uncatalogued: Vec<PackageCalls> = packages
            .into_iter()
            .map(|(package, mut symbols)| {
                symbols.sort_by(|a, b| {
                    (a.counts.reach(), &a.symbol).cmp(&(b.counts.reach(), &b.symbol))
                });
                let mut counts = Counts::default();
                for s in &symbols {
                    counts.calls += s.counts.calls;
                    counts.awaited += s.counts.awaited;
                    counts.on_event_loop += s.counts.on_event_loop;
                    counts.in_loops += s.counts.in_loops;
                }
                totals.uncatalogued_symbols += symbols.len();
                truncated |= symbols.len() > MAX_LISTED;
                symbols.truncate(MAX_LISTED);
                PackageCalls {
                    package,
                    counts,
                    symbols,
                }
            })
            .collect();
        for p in &uncatalogued {
            totals.uncatalogued.calls += p.counts.calls;
            totals.uncatalogued.awaited += p.counts.awaited;
            totals.uncatalogued.on_event_loop += p.counts.on_event_loop;
            totals.uncatalogued.in_loops += p.counts.in_loops;
        }
        totals.uncatalogued_packages = uncatalogued.len();
        uncatalogued
            .sort_by(|a, b| (a.counts.reach(), &a.package).cmp(&(b.counts.reach(), &b.package)));
        truncated |= uncatalogued.len() > MAX_LISTED;
        uncatalogued.truncate(MAX_LISTED);
        // Only the names heki would report through: the catalog covers the package.
        let mut catalog_may_miss: Vec<SymbolCalls> = registered
            .into_iter()
            .filter(|s| hypothetical_reports(s) > 0)
            .collect();
        catalog_may_miss.sort_by(|a, b| {
            (
                std::cmp::Reverse(hypothetical_reports(a)),
                a.counts.reach(),
                &a.symbol,
            )
                .cmp(&(
                    std::cmp::Reverse(hypothetical_reports(b)),
                    b.counts.reach(),
                    &b.symbol,
                ))
        });
        truncated |= catalog_may_miss.len() > MAX_LISTED;
        catalog_may_miss.truncate(MAX_LISTED);
        let mut a1_hazards = A1Hazards::default();
        for f in files.iter().flat_map(|f| &f.ir.functions) {
            if !f.a1_hazards.is_empty() {
                a1_hazards.functions += 1;
                a1_hazards.unmodeled += usize::from(f.is_unmodeled());
            }
        }
        BlindSpotsSection {
            uncatalogued,
            catalog_may_miss,
            unresolved: self.unresolved,
            source_roots: roots.candidates,
            a1_hazards,
            totals,
            truncated,
        }
    }
}

/// The reports `heki check` would print through the name if it waited or sent a query.
fn hypothetical_reports(s: &SymbolCalls) -> usize {
    s.if_waits.unwrap_or_default().total() + s.if_queries.unwrap_or_default().total()
}

/// The notes the section adds to the report's.
pub(super) fn notes(section: &BlindSpotsSection, trial: &Trial) -> Vec<String> {
    let mut notes = Vec::new();
    if section.totals.tried > 0 {
        notes.push(format!(
            "\"if it waits\" and \"if it sends a query\" come from a trial run that gives each \
             library name tried a catalog entry with `effects = [\"{}\", \"{}\"]`: what `heki \
             check` would print if the call waited on the network or sent a database query. heki \
             cannot see what these calls do, so no entry for them is in the block; add one in \
             `[[tool.heki.ecosystem]]` if you know.",
            HYPOTHETICAL_EFFECTS[0], HYPOTHETICAL_EFFECTS[1]
        ));
    }
    if section.totals.not_tried > 0 {
        notes.push(format!(
            "Only the {} library names with the most reach were tried; {} more were not.",
            section.totals.tried, section.totals.not_tried
        ));
    }
    if section.totals.below_source_roots > 0 {
        notes.push(format!(
            "{} call(s) of names of the project's files below a directory under \"source root\" \
             are not counted as library calls: heki takes them for a library's only because the \
             directory is not a source root.",
            section.totals.below_source_roots
        ));
    }
    if trial.several {
        notes.push(
            "A report naming several of the library names tried counts for the first by name."
                .to_owned(),
        );
    }
    notes
}

// ------------------------------------------------------------------------------ source roots

/// What the source-root trial found: each candidate directory, and the source roots the block
/// sets.
#[derive(Debug, Default)]
pub(super) struct SourceRoots {
    /// The first 50 candidates.
    pub candidates: Vec<SourceRoot>,
    /// How many there are.
    total: usize,
    /// `src` as the block sets it: the source roots now, then the directories added.
    pub src: Option<Vec<String>>,
    /// The dotted paths of the project's files below the candidate directories that imports
    /// name (`config.env` of `backend/config/env.py`).
    below: FxHashSet<String>,
}

impl SourceRoots {
    /// Whether the dotted name `name` is under a dotted path of a project file below a
    /// candidate directory.
    pub fn names_project_file(&self, name: &str) -> bool {
        !self.below.is_empty() && module_prefix(name, |p| self.below.contains(p)).is_some()
    }
}

/// A Python file below the project root, as the source-root trial reads it.
struct TreeFile<'p> {
    path: Cow<'p, Path>,
    /// Relative to the project root, POSIX.
    rel: Cow<'p, str>,
    read: Read<'p>,
}

enum Read<'p> {
    Analyzed(&'p ParsedFile),
    /// A file the paths given leave out: its imports, and the qualnames of its definitions.
    LeftOut(Vec<ModuleImport>, Vec<String>),
    /// A file heki cannot read or parse: only the module it defines counts.
    Unread,
}

impl TreeFile<'_> {
    fn imports(&self) -> &[ModuleImport] {
        match &self.read {
            Read::Analyzed(f) => &f.semantic.imports,
            Read::LeftOut(imports, _) => imports,
            Read::Unread => &[],
        }
    }

    /// The qualnames of its functions (its body, `<module>`, included) and classes.
    fn definitions(&self) -> Vec<&str> {
        match &self.read {
            Read::Analyzed(f) => definitions(f).collect(),
            Read::LeftOut(_, names) => names.iter().map(String::as_str).collect(),
            Read::Unread => Vec::new(),
        }
    }
}

fn definitions(f: &ParsedFile) -> impl Iterator<Item = &str> {
    (f.ir.functions.iter().map(|g| g.qualname.as_str()))
        .chain(f.semantic.classes.iter().map(|c| c.qualname.as_str()))
}

/// The Python files below the project root that `heki check` on the root reads: those analyzed,
/// in the order of `Project::files` (so that an index of one is its index there), those
/// `left_out` names, read here, then those heki cannot read or parse.
fn python_files<'p>(project: &'p Project, left_out: &[PathBuf]) -> Vec<TreeFile<'p>> {
    let env = &project.env;
    let mut tree: Vec<TreeFile<'p>> = (project.files.iter())
        .map(|f| TreeFile {
            path: Cow::Borrowed(&f.path),
            rel: Cow::Borrowed(&f.rel),
            read: Read::Analyzed(f),
        })
        .collect();
    let mut unread: Vec<TreeFile<'p>> = (project.unread.iter())
        .map(|u| TreeFile {
            path: Cow::Borrowed(&u.path),
            rel: Cow::Owned(rel_posix(&u.path, &env.root)),
            read: Read::Unread,
        })
        .collect();
    for path in left_out {
        let parsed = heki_ast::read_source(path).ok().and_then(|source| {
            parse_source(path, &env.root, source, &env.layout, env.runtime).ok()
        });
        match parsed {
            Some(f) => {
                let names = definitions(&f).map(str::to_owned).collect();
                tree.push(TreeFile {
                    path: Cow::Owned(f.path),
                    rel: Cow::Owned(f.rel),
                    read: Read::LeftOut(f.semantic.imports, names),
                });
            }
            None => unread.push(TreeFile {
                path: Cow::Owned(display_path(path)),
                rel: Cow::Owned(rel_posix(path, &env.root)),
                read: Read::Unread,
            }),
        }
    }
    tree.extend(unread);
    tree
}

/// The module names of the files of a tree under one module layout, as heki's index gives them.
struct Names {
    /// Of each file: its module and whether it is a package.
    files: Vec<(String, bool)>,
    /// Of each module name, the file heki's index takes it from ([`module_precedence`]).
    owners: FxHashMap<String, usize>,
    /// The files of one kind (implementations or stubs) that define one module, of each module
    /// more than one such file defines.
    shared: BTreeSet<Vec<usize>>,
    layout: ModuleLayout,
}

impl Names {
    fn under(tree: &[TreeFile<'_>], layout: &ModuleLayout) -> Self {
        let files: Vec<(String, bool)> = (tree.iter())
            .map(|f| module_of(&f.rel, &f.path, layout))
            .collect();
        let mut owners: FxHashMap<String, usize> = FxHashMap::default();
        let mut kinds: FxHashMap<(&str, bool), Vec<usize>> = FxHashMap::default();
        for (i, (module, _)) in files.iter().enumerate() {
            let path: &Path = &tree[i].path;
            owners
                .entry(module.clone())
                .and_modify(|o| {
                    if module_precedence(path) > module_precedence(&tree[*o].path) {
                        *o = i;
                    }
                })
                .or_insert(i);
            kinds
                .entry((module.as_str(), is_stub(path)))
                .or_default()
                .push(i);
        }
        let shared = kinds.into_values().filter(|f| f.len() > 1).collect();
        Self {
            files,
            owners,
            shared,
            layout: layout.clone(),
        }
    }

    /// The source root holding file `i`, `None` for the project root.
    fn root_of(&self, tree: &[TreeFile<'_>], i: usize) -> Option<&str> {
        self.layout.source_root_of(Path::new(tree[i].rel.as_ref()))
    }

    /// What the dotted name `name` names under this layout: the file heki's index resolves it
    /// into (the longest module it is or is below, [`module_prefix`]), and the rest of the name,
    /// which names something in that module (`.load` of `config.env.load`). Two layouts that
    /// give a name the same target let heki follow it to the same definition.
    fn target<'n>(&self, name: &'n str) -> Option<Target<'n>> {
        let module = module_prefix(name, |p| self.owners.contains_key(p))?;
        Some(Target {
            file: self.owners[module],
            rest: &name[module.len()..],
        })
    }

    /// The dotted name the import `imp` of file `i` names: the module of `import a.b`, the
    /// name `from` imports below its module, the module of a relative import resolved from the
    /// file's own (`None` when it climbs past the top).
    fn imported(&self, i: usize, imp: &ModuleImport) -> Option<String> {
        if !imp.is_from {
            return Some(imp.imported_name.clone());
        }
        let (module, is_package) = &self.files[i];
        let base = resolve_relative_module(
            module,
            *is_package,
            imp.level,
            imp.module.as_deref(),
            self.layout.root_is_package,
        )?;
        Some(if base.is_empty() {
            imp.imported_name.clone()
        } else {
            format!("{base}.{}", imp.imported_name)
        })
    }
}

/// What a dotted name names under a module layout ([`Names::target`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Target<'n> {
    file: usize,
    rest: &'n str,
}

impl Target<'_> {
    /// As a refusal says it: the file, and the name in it when the target is not the module.
    fn describe(t: Option<Self>, tree: &[TreeFile<'_>], shown: &dyn Fn(&Path) -> String) -> String {
        let Some(t) = t else {
            return "no project module".to_owned();
        };
        let path = shown(&tree[t.file].path);
        match t.rest.strip_prefix('.') {
            Some(rest) => format!("`{rest}` of {path}"),
            None => path,
        }
    }
}

/// One import of a file that names no project module now.
struct Unresolved {
    file: usize,
    /// Its index among the file's imports.
    import: usize,
    line: u32,
    name: String,
}

/// The source-root trial (no analysis: it computes the module names `heki check` would give the
/// files with `src` set, and resolves the imports by them as heki's index does). A candidate is
/// a directory below the project root, neither a source root nor a regular package, below which
/// the dotted path of an import that names no project module now is a file
/// (`backend/config/env.py` for `import config.env`). An import of the standard library or of a
/// package the catalog covers is no candidate's, and neither is one whose path is below several
/// directories none of which holds the importing file. The candidates resolving the most imports
/// are tried first, each with the ones already taken, over every Python file below the project
/// root (those `left_out` names, the files the paths given leave out, included; when it is
/// `None`, there are more than heki reads, and none is tried): one enters the block when its
/// imports resolve into its own files, no two files of one kind define one module that did not
/// before, every import that resolves now and every import of a candidate taken still names the
/// same module of the same file, and the configuration's contracts and `baseline`'s entries name
/// what they name now, by the matchers the rules use. The block is then judged whole: a
/// candidate under which an import no candidate taken was taken for would name a file below
/// another root than the importing file's stays out, and the others are tried again.
pub(super) fn source_roots(
    project: &Project,
    left_out: Option<&[PathBuf]>,
    baseline: Option<&RatchetBaseline>,
    shown: &dyn Fn(&Path) -> String,
) -> SourceRoots {
    let env = &project.env;
    let tree = python_files(project, left_out.unwrap_or_default());
    let current = Names::under(&tree, &env.layout);
    let roots = &env.layout.source_roots;
    // The directories below which a dotted path is a file of the project: each pair once, and
    // per dotted path how many directories and the first. A regular package is no source root
    // (as one, its `__init__.py` would be a module named `__init__`), as heki's detection
    // decides.
    let mut source_dirs: FxHashMap<String, bool> = FxHashMap::default();
    let mut pairs: FxHashSet<(String, String)> = FxHashSet::default();
    let mut below: FxHashMap<String, (usize, String)> = FxHashMap::default();
    for f in &project.files {
        let parts: Vec<&str> = f.rel.split('/').collect();
        for i in 1..parts.len() {
            let dir = parts[..i].join("/");
            let is_source_dir = !roots.contains(&dir)
                && *source_dirs
                    .entry(dir.clone())
                    .or_insert_with(|| is_source_dir(&env.root.join(&dir), &Path::is_file));
            let dotted = is_source_dir
                .then(|| module_name_for_path(Path::new(&parts[i..].join("/"))))
                .flatten();
            if let Some((dotted, _)) = dotted
                && pairs.insert((dotted.clone(), dir.clone()))
            {
                below.entry(dotted).or_insert((0, dir)).0 += 1;
            }
        }
    }
    let mut out = SourceRoots::default();
    let mut by_dir: BTreeMap<String, Vec<Unresolved>> = BTreeMap::new();
    let mut resolved_now: Vec<(usize, usize)> = Vec::new();
    for (i, f) in tree.iter().enumerate() {
        let mut lines = None;
        for (k, imp) in f.imports().iter().enumerate() {
            let Some(name) = current.imported(i, imp) else {
                continue;
            };
            if current.target(&name).is_some() {
                resolved_now.push((i, k));
                continue;
            }
            // Only the files analyzed make candidates; the others are checked.
            let Read::Analyzed(parsed) = &f.read else {
                continue;
            };
            let top = name.split('.').next().unwrap_or(&name);
            if imp.level > 0 || heki_env::is_stdlib_module(top) || env.kb.is_opaque_to_layer3(top) {
                continue;
            }
            let Some(dotted) = module_prefix(&name, |p| below.contains_key(p)) else {
                continue;
            };
            let (count, first) = &below[dotted];
            out.below.insert(dotted.to_owned());
            // Below several directories, the import is of the one holding the importing file
            // (its own application's), the deepest; one that holds none is no candidate's.
            let dir = if *count == 1 {
                Some(first.clone())
            } else {
                let parts: Vec<&str> = f.rel.split('/').collect();
                (1..parts.len())
                    .rev()
                    .map(|n| parts[..n].join("/"))
                    .find(|dir| pairs.contains(&(dotted.to_owned(), dir.clone())))
            };
            if let Some(dir) = dir {
                let lines = lines.get_or_insert_with(|| LineIndex::from_source(&parsed.source));
                by_dir.entry(dir).or_default().push(Unresolved {
                    file: i,
                    import: k,
                    line: lines.line(imp.span.start),
                    name: name.clone(),
                });
            }
        }
    }
    let mut ranked: Vec<(String, Vec<Unresolved>)> = by_dir.into_iter().collect();
    ranked.sort_by(|a, b| {
        (std::cmp::Reverse(a.1.len()), &a.0).cmp(&(std::cmp::Reverse(b.1.len()), &b.0))
    });
    out.total = ranked.len();
    ranked.truncate(MAX_LISTED);
    let trial = SourceRootTrial {
        project,
        tree: &tree,
        current: &current,
        ranked: &ranked,
        resolved_now: &resolved_now,
        configured: &configured_names(&env.cfg),
        baseline,
        shown,
    };
    // The block, judged whole: a candidate taken that makes an import no candidate taken was
    // taken for resolve into a file below another root than the importing file's is left out,
    // and the others are tried again without it.
    let block = if left_out.is_none() {
        Block::unverifiable(ranked.len())
    } else {
        let mut kept_out: BTreeMap<usize, String> = BTreeMap::new();
        loop {
            let block = trial.take(&kept_out);
            match block.by_chance(&trial) {
                Some((n, reason)) => {
                    kept_out.insert(n, reason);
                }
                None => break block,
            }
        }
    };
    for (n, (dir, imports)) in ranked.iter().enumerate() {
        let examples = imports
            .iter()
            .take(MAX_LISTED)
            .map(|u| ImportAt {
                path: shown(&project.files[u.file].path),
                line: u.line,
                name: u.name.clone(),
            })
            .collect();
        let verdict = block.verdicts.get(n).cloned().unwrap_or_else(|| {
            Some(format!(
                "not tried: only the first {MAX_SOURCE_ROOT_TRIALS} directories by imports are"
            ))
        });
        out.candidates.push(SourceRoot {
            directory: dir.clone(),
            imports: imports.len(),
            examples,
            in_block: verdict.is_none(),
            reason: verdict.unwrap_or_else(|| {
                "its imports resolve, and no import, contract, or baseline entry loses its \
                 module"
                    .to_owned()
            }),
        });
    }
    out.src = block.src;
    out
}

/// What the source-root trial reads: the project and every Python file below its root, their
/// module names now, the candidates by rank with their imports, the imports that resolve now,
/// and the names the configuration and the baseline use.
struct SourceRootTrial<'t> {
    project: &'t Project,
    tree: &'t [TreeFile<'t>],
    current: &'t Names,
    ranked: &'t [(String, Vec<Unresolved>)],
    resolved_now: &'t [(usize, usize)],
    configured: &'t [ConfiguredName<'t>],
    baseline: Option<&'t RatchetBaseline>,
    shown: &'t dyn Fn(&Path) -> String,
}

/// The candidates one pass of the trial takes.
struct Block {
    /// Of each candidate tried, by rank: `None` when taken, else why not.
    verdicts: Vec<Option<String>>,
    /// `src` with the candidates taken, `None` when none is.
    src: Option<Vec<String>>,
    /// The module names with the candidates taken, `None` when none is.
    names: Option<Names>,
    /// The imports the block leaves naming what they name: those that resolve now, then those
    /// of each candidate taken (by file, then index among its imports).
    kept: FxHashSet<(usize, usize)>,
}

impl SourceRootTrial<'_> {
    /// The first [`MAX_SOURCE_ROOT_TRIALS`] candidates tried in rank order, each with the ones
    /// already taken, but for those `kept_out` gives a reason for.
    fn take(&self, kept_out: &BTreeMap<usize, String>) -> Block {
        let roots = &self.project.env.layout.source_roots;
        let mut kept: Vec<(usize, usize)> = self.resolved_now.to_vec();
        let mut block = Block {
            verdicts: Vec::new(),
            src: None,
            names: None,
            kept: FxHashSet::default(),
        };
        let mut accepted: Vec<&str> = Vec::new();
        for (n, (dir, imports)) in self.ranked.iter().enumerate() {
            if n >= MAX_SOURCE_ROOT_TRIALS {
                break;
            }
            if let Some(reason) = kept_out.get(&n) {
                block.verdicts.push(Some(reason.clone()));
                continue;
            }
            let mut with: Vec<String> = roots.clone();
            with.extend(accepted.iter().map(|d| (*d).to_owned()));
            with.push(dir.clone());
            // A configured `src` replaces the detection, the workspace members' packages
            // included ([`ModuleLayout::detect`]): the block's layout names those files below
            // its roots, and the imports resolved now that it would change keep it out.
            let layout = ModuleLayout {
                source_roots: with.clone(),
                member_packages: Vec::new(),
                root_is_package: false,
            };
            let next = Names::under(self.tree, &layout);
            let prev = block.names.as_ref().unwrap_or(self.current);
            let reason = self.refusal(prev, &next, dir, imports, &kept);
            if reason.is_none() {
                accepted.push(dir);
                block.src = Some(with);
                kept.extend(imports.iter().map(|u| (u.file, u.import)));
                block.names = Some(next);
            }
            block.verdicts.push(reason);
        }
        block.kept = kept.into_iter().collect();
        block
    }

    /// Why the layout `next`, that of `prev` with `dir` a source root too, stays out of the
    /// block, or `None` when it may enter it. `imports` are the imports `dir` is tried for, and
    /// under `prev` each import of `kept` names what it must keep naming.
    fn refusal(
        &self,
        prev: &Names,
        next: &Names,
        dir: &str,
        imports: &[Unresolved],
        kept: &[(usize, usize)],
    ) -> Option<String> {
        let (tree, shown) = (self.tree, self.shown);
        // Each import the directory is tried for names one of its files, the one whose path
        // below it the import spells.
        for u in imports {
            let Some(t) = next.target(&u.name) else {
                return Some(format!("`{}` would still name no project module", u.name));
            };
            if next.root_of(tree, t.file) != Some(dir) {
                return Some(format!(
                    "`{}` in {} would name {}, not a file below `{dir}`",
                    u.name,
                    shown(&tree[u.file].path),
                    Target::describe(Some(t), tree, shown)
                ));
            }
        }
        // heki takes a module from one file (an implementation before its stub): files of one
        // kind sharing a name leave the others' definitions out.
        if let Some(files) = (next.shared.iter()).find(|f| !self.current.shared.contains(*f)) {
            let mut paths: Vec<String> = files.iter().map(|&i| shown(&tree[i].path)).collect();
            let last = paths.pop().unwrap_or_default();
            return Some(format!(
                "{} and {last} would each define module `{}`",
                paths.join(", "),
                next.files[files[0]].0
            ));
        }
        // The block as a whole keeps what an import that resolves now names, and what the imports
        // a candidate was taken for name: a later, deeper source root may rename those files. A
        // prefix still being a module is not enough (`backend` of `backend.lib.config.env` once
        // `backend/lib` is a source root): heki would follow the name into another file.
        let moved = kept.iter().find_map(|&(i, k)| {
            let imp = &tree[i].imports()[k];
            let name = prev.imported(i, imp)?;
            let moved = next.imported(i, imp);
            let was = prev.target(&name);
            let now = moved.as_deref().and_then(|n| next.target(n));
            (now != was).then(|| {
                format!(
                    "`{name}` in {} would name {}, not {}",
                    shown(&tree[i].path),
                    Target::describe(now, tree, shown),
                    Target::describe(was, tree, shown)
                )
            })
        });
        moved
            .or_else(|| self.contract_change(prev, next))
            .or_else(|| self.baseline_change(next))
    }

    /// The first name a contract of the configuration gives definitions by whose rule would name
    /// a definition otherwise once its file is named as `next` names it: the rules' matchers
    /// ([`DottedPattern`]) judge its name now and its name then, for each file `prev` names
    /// otherwise, the whole name before what holds its last part.
    fn contract_change(&self, prev: &Names, next: &Names) -> Option<String> {
        if self.configured.is_empty() {
            return None;
        }
        let renamed: Vec<(&str, &str, Vec<&str>)> = (self.tree.iter().enumerate())
            .filter(|&(i, _)| prev.files[i].0 != next.files[i].0)
            .map(|(i, f)| {
                let (now, then) = (&self.current.files[i].0, &next.files[i].0);
                (now.as_str(), then.as_str(), f.definitions())
            })
            .collect();
        let named = |p: &DottedPattern, module: &str, q: &str| {
            let fqn = format!("{module}.{q}");
            (p.matches_def(&fqn, q), p.names_within(&fqn, q))
        };
        for c in self.configured {
            for p in &c.patterns {
                for (now, then, definitions) in &renamed {
                    let Some((q, was)) = (definitions.iter())
                        .map(|q| (*q, named(p, now, q)))
                        .find(|(q, was)| *was != named(p, then, q))
                    else {
                        continue;
                    };
                    let will = named(p, then, q);
                    let named_now = if was.0 == will.0 { was.1 } else { was.0 };
                    let describe = |module: &str| match q {
                        "<module>" => format!("module `{module}`"),
                        _ => format!("`{module}.{q}`"),
                    };
                    let (from, to) = (describe(now), describe(then));
                    let whose = format!(
                        "`{}` ({}) of the {} contract `{}`",
                        c.text, c.key, c.kind, c.contract
                    );
                    return Some(if named_now {
                        format!("{whose} names {from}, and would not name it as {to}")
                    } else {
                        format!("{whose} would name {from} as {to}, which it does not name now")
                    });
                }
            }
        }
        None
    }

    /// The first dotted name in an entry of the baseline that names a module `next` names
    /// otherwise: heki would write the key of the report with the new name, which the entry
    /// would not match.
    fn baseline_change(&self, next: &Names) -> Option<String> {
        let current = self.current;
        let dotted = |c: char| c.is_alphanumeric() || c == '_' || c == '.';
        (self.baseline?.counts.keys())
            .flat_map(|key| [&key.target, &key.qualname])
            .flat_map(|text| text.split(|c| !dotted(c)).map(|run| run.trim_matches('.')))
            .find_map(|run| {
                let t = current.target(run)?;
                let renamed = &next.files[t.file].0;
                (*renamed != current.files[t.file].0).then(|| {
                    format!(
                        "the baseline names `{run}`, which heki would write `{renamed}{}`",
                        t.rest
                    )
                })
            })
    }
}

impl Block {
    /// The verdicts when the paths given leave out Python files heki does not read: no
    /// candidate is tried, as heki cannot check what the block changes for them.
    fn unverifiable(candidates: usize) -> Self {
        let reason = format!(
            "not tried: the paths given leave out more Python files below the project root \
             than heki reads ({MAX_LISTED}), and a source root may change what those import"
        );
        Self {
            verdicts: vec![Some(reason); candidates.min(MAX_SOURCE_ROOT_TRIALS)],
            src: None,
            names: None,
            kept: FxHashSet::default(),
        }
    }

    /// The first import, by file then position, that the block makes resolve although no
    /// candidate taken was taken for it, into a file below a source root other than the
    /// importing file's own: the rank of the candidate that root is, and why it stays out. Such
    /// an import is of a candidate not taken, or of none (below several directories, a name of
    /// the standard library); nothing shows the file it would name is the one meant.
    fn by_chance(&self, trial: &SourceRootTrial<'_>) -> Option<(usize, String)> {
        let names = self.names.as_ref()?;
        let tree = trial.tree;
        tree.iter().enumerate().find_map(|(i, f)| {
            let own = names.root_of(tree, i);
            (f.imports().iter().enumerate())
                .filter(|(k, _)| !self.kept.contains(&(i, *k)))
                .find_map(|(_, imp)| {
                    let name = names.imported(i, imp)?;
                    let target = names.target(&name)?;
                    let other = (names.root_of(tree, target.file)).filter(|r| Some(*r) != own)?;
                    let n = trial.ranked.iter().position(|(dir, _)| dir == other)?;
                    let importer = (trial.shown)(&f.path);
                    Some((
                        n,
                        format!(
                            "`{name}` in {importer} would name {} below source root `{other}`, \
                             not the one of {importer}: heki cannot tell it is the file meant",
                            Target::describe(Some(target), tree, trial.shown)
                        ),
                    ))
                })
        })
    }
}

// ------------------------------------------------------------------------------ text

/// Why heki resolves no function for a call, as the text says it.
fn why(u: &UnresolvedCalls) -> String {
    let parameter = u.parameter.as_deref().unwrap_or_default();
    let why = match u.reason.as_str() {
        "unannotated-parameter" => format!("parameter `{parameter}` has no annotation"),
        "unresolved-annotation" => {
            format!("the annotation of parameter `{parameter}` names no class heki knows")
        }
        "any-value" => "a value annotated `Any` or `object`".to_owned(),
        "callable-value" => "a value annotated `Callable`".to_owned(),
        "unbound-name" => "a name nothing heki sees binds".to_owned(),
        "unlisted-protocol" => "a protocol whose implementations heki cannot list".to_owned(),
        "call-result" => "a call's result heki gives no class".to_owned(),
        "attribute" => "an attribute heki gives no class".to_owned(),
        "computed-name" => "a method named at run time (`getattr`)".to_owned(),
        "too-many-implementations" => "more implementations than heki follows".to_owned(),
        "unknown-member" => {
            "a member heki does not find: a base it knows nothing of, or a module it could not \
             read"
                .to_owned()
        }
        _ => "a value heki gives no class".to_owned(),
    };
    if u.names.is_empty() {
        why
    } else {
        format!("{why} ({})", u.names.join(", "))
    }
}

/// What a trial counted for a name under the rule `code` (`None`: not tried), as the text says
/// it.
fn hypothetical(code: &str, counts: Option<TrialReports>) -> String {
    let Some(counts) = counts else {
        return "not tried".to_owned();
    };
    let found: Vec<String> = [("exact", counts.exact), ("heuristic", counts.heuristic)]
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .map(|(tier, n)| format!("{n} {tier}"))
        .collect();
    if found.is_empty() {
        "-".to_owned()
    } else {
        format!("{code} {}", found.join(", "))
    }
}

/// A table of library names, as the text prints it.
fn names_table(out: &mut String, symbols: &[&SymbolCalls], total: usize) {
    let width = symbols
        .iter()
        .map(|s| s.symbol.len())
        .max()
        .unwrap_or_default()
        .max("name".len());
    let rows: Vec<(String, String)> = symbols
        .iter()
        .map(|s| {
            (
                hypothetical("EFF101", s.if_waits),
                hypothetical("EFF103", s.if_queries),
            )
        })
        .collect();
    let waits_width = rows
        .iter()
        .map(|(w, _)| w.len())
        .max()
        .unwrap_or_default()
        .max("if it waits".len());
    let _ = writeln!(
        out,
        "    {:<width$}  calls  async  loops  {:<waits_width$}  if it sends a query",
        "name", "if it waits"
    );
    for (s, (waits, queries)) in symbols.iter().zip(rows) {
        let c = s.counts;
        let _ = writeln!(
            out,
            "    {:<width$}  {:>5}  {:>5}  {:>5}  {waits:<waits_width$}  {queries}",
            s.symbol, c.calls, c.on_event_loop, c.in_loops
        );
    }
    if total > symbols.len() {
        let _ = writeln!(out, "    and {} more", total - symbols.len());
    }
}

/// The section as the text prints it.
pub(super) fn render(out: &mut String, section: &BlindSpotsSection) {
    out.push_str("\nWhat heki cannot see (no report either way)\n");
    let t = &section.totals;
    let mut empty = true;
    if t.uncatalogued_symbols > 0 {
        empty = false;
        let _ = writeln!(
            out,
            "  calls of packages heki's catalog does not cover: {} call(s) of {} name(s) in {} \
             package(s)",
            t.uncatalogued.calls, t.uncatalogued_symbols, t.uncatalogued_packages
        );
        let mut symbols: Vec<&SymbolCalls> = section
            .uncatalogued
            .iter()
            .flat_map(|p| &p.symbols)
            .collect();
        symbols.sort_by(|a, b| (a.counts.reach(), &a.symbol).cmp(&(b.counts.reach(), &b.symbol)));
        symbols.truncate(MAX_TEXT_ROWS);
        names_table(out, &symbols, t.uncatalogued_symbols);
    }
    if !section.catalog_may_miss.is_empty() {
        empty = false;
        let _ = writeln!(
            out,
            "  names heki's catalog lacks in packages it covers, which heki would report through \
             if they waited or sent a query (if they do, report them at {ISSUES_URL})"
        );
        let symbols: Vec<&SymbolCalls> = section
            .catalog_may_miss
            .iter()
            .take(MAX_TEXT_ROWS)
            .collect();
        names_table(out, &symbols, section.catalog_may_miss.len());
    }
    if t.unresolved.calls > 0 {
        empty = false;
        let _ = writeln!(
            out,
            "  calls heki does not resolve: {} call(s) in {} function(s)",
            t.unresolved.calls, t.unresolved_functions
        );
        let rows: Vec<&UnresolvedCalls> = section.unresolved.iter().take(MAX_TEXT_ROWS).collect();
        let width = rows
            .iter()
            .map(|u| u.function.len())
            .max()
            .unwrap_or_default()
            .max("function".len());
        let _ = writeln!(out, "    {:<width$}  calls  async  loops  why", "function");
        for u in &rows {
            let c = u.counts;
            let _ = writeln!(
                out,
                "    {:<width$}  {:>5}  {:>5}  {:>5}  {}",
                u.function,
                c.calls,
                c.on_event_loop,
                c.in_loops,
                why(u)
            );
        }
        if t.unresolved_groups > rows.len() {
            let _ = writeln!(out, "    and {} more", t.unresolved_groups - rows.len());
        }
    }
    for root in section.source_roots.iter().take(MAX_TEXT_ROWS) {
        empty = false;
        let mut names: Vec<&str> = root.examples.iter().map(|e| e.name.as_str()).collect();
        names.dedup();
        let more = if names.len() > MAX_EXAMPLES {
            ", ..."
        } else {
            ""
        };
        names.truncate(MAX_EXAMPLES);
        let verdict = if root.in_block {
            "in the block".to_owned()
        } else {
            format!("stays out: {}", root.reason)
        };
        let _ = writeln!(
            out,
            "  source root `{}`: {} import(s) name its files ({}{more}): {verdict}",
            root.directory,
            root.imports,
            names.join(", ")
        );
    }
    let shown_roots = section.source_roots.len().min(MAX_TEXT_ROWS);
    if t.source_root_candidates > shown_roots {
        let _ = writeln!(
            out,
            "  and {} more source-root candidate(s)",
            t.source_root_candidates - shown_roots
        );
    }
    let a1 = section.a1_hazards;
    if a1.functions > 0 {
        empty = false;
        let _ = writeln!(
            out,
            "  {} function(s) with an A1 hazard, {} of them too large to model: the Soundness lane \
             proves nothing in them (`heki assumptions` lists them)",
            a1.functions, a1.unmodeled
        );
    }
    if empty {
        out.push_str(
            "  Nothing: every call resolves, every library name called has a catalog entry, and \
             every function is modeled.\n",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_text_says_why_a_call_is_not_resolved() {
        for (reason, parameter, names, said) in [
            (
                "unannotated-parameter",
                Some("client"),
                vec!["get", "post"],
                "parameter `client` has no annotation (get, post)",
            ),
            (
                "unresolved-annotation",
                Some("x"),
                vec![],
                "the annotation of parameter `x` names no class heki knows",
            ),
            (
                "any-value",
                None,
                vec![],
                "a value annotated `Any` or `object`",
            ),
            (
                "callable-value",
                None,
                vec![],
                "a value annotated `Callable`",
            ),
            (
                "unbound-name",
                None,
                vec![],
                "a name nothing heki sees binds",
            ),
            (
                "unlisted-protocol",
                None,
                vec![],
                "a protocol whose implementations heki cannot list",
            ),
            (
                "call-result",
                None,
                vec![],
                "a call's result heki gives no class",
            ),
            (
                "attribute",
                None,
                vec![],
                "an attribute heki gives no class",
            ),
            (
                "computed-name",
                None,
                vec![],
                "a method named at run time (`getattr`)",
            ),
            (
                "too-many-implementations",
                None,
                vec![],
                "more implementations than heki follows",
            ),
            (
                "unknown-member",
                None,
                vec!["m"],
                "a member heki does not find: a base it knows nothing of, or a module it could \
                 not read (m)",
            ),
            ("untyped-value", None, vec![], "a value heki gives no class"),
        ] {
            let calls = UnresolvedCalls {
                function: "f".into(),
                path: "a.py".into(),
                reason: reason.into(),
                parameter: parameter.map(str::to_owned),
                names: names.into_iter().map(str::to_owned).collect(),
                counts: Counts::default(),
                examples: Vec::new(),
            };
            assert_eq!(why(&calls), said, "{reason}");
        }
    }

    #[test]
    fn a_name_not_tried_is_told_apart_from_one_that_reports_nothing() {
        let counts = |exact, heuristic| Some(TrialReports { exact, heuristic });
        for (code, counts, said) in [
            ("EFF101", counts(2, 1), "EFF101 2 exact, 1 heuristic"),
            ("EFF101", counts(0, 3), "EFF101 3 heuristic"),
            ("EFF103", counts(0, 0), "-"),
            ("EFF103", None, "not tried"),
        ] {
            assert_eq!(hypothetical(code, counts), said);
        }
    }
}
