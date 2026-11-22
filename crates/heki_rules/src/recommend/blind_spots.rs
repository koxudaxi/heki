//! "What heki cannot see": where heki is silent, so no count of reports applies. The calls of
//! library names the ecosystem catalog has no entry for, with what `heki check` would report if
//! each waited or sent a database query (the hypothetical-catalog run); the calls heki does not
//! resolve, with why; the directories that, made source roots, resolve imports that name no
//! project module now (the source-root trial); and the functions with an A1 hazard.
//!
//! A library entry never enters the block: heki cannot verify what a library call does (ADR-020,
//! rule 3). A source root enters it only after its trial.

use super::{ISSUES_URL, MAX_EXAMPLES, MAX_LISTED, RecommendError, printed, selection_of};
use crate::blind_spots::{BlindSite, BlindSpots, CallCounts};
use crate::project::{Facts, Indexed, ParsedFile, Project, module_of, rel_posix};
use heki_ast::LineIndex;
use heki_diagnostics::RatchetBaseline;
use heki_effect::{ModuleLayout, ProjectIndex, module_name_for_path, resolve_relative_module};
use heki_env::{EcosystemOverride, HekiConfig};
use heki_semantic::ModuleImport;
use rustc_hash::{FxHashMap, FxHashSet};
use schemars::JsonSchema;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

/// The most library names the hypothetical-catalog run gives an entry: those with the most
/// reach.
pub const MAX_TRIED_SYMBOLS: usize = 64;

/// The effects the hypothetical-catalog run gives each name: a wait on the network (`EFF101`,
/// `EFF111`) and a database query (`EFF103`, `EFF113`).
pub const HYPOTHETICAL_EFFECTS: [&str; 2] = ["blocking_net", "db_query"];

/// The rules the hypothetical-catalog run selects.
const HYPOTHETICAL_RULES: [&str; 4] = ["EFF101", "EFF111", "EFF103", "EFF113"];

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
    /// What `heki check` would report if the call waited on the network, from the
    /// hypothetical-catalog run (`null`: the name was not tried).
    pub if_waits: Option<IfWaits>,
    /// What `heki check` would report if the call sent a database query (`null`: not tried).
    pub if_queries: Option<IfQueries>,
    /// The first 50 calls.
    pub examples: Vec<Site>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct IfWaits {
    #[serde(rename = "EFF101")]
    pub eff101: usize,
    #[serde(rename = "EFF111")]
    pub eff111: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct IfQueries {
    #[serde(rename = "EFF103")]
    pub eff103: usize,
    #[serde(rename = "EFF113")]
    pub eff113: usize,
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
    found: BTreeMap<String, (IfWaits, IfQueries)>,
    /// A report named several of the names tried: it counts for the first.
    several: bool,
}

/// The hypothetical-catalog run: `project` analyzed with an entry of [`HYPOTHETICAL_EFFECTS`]
/// for each of `tried` added to its configuration, with the rules those effects make report and
/// the configuration's `per-file-ignores`, as `heki check` would. Each report counts for the name
/// tried that its chain ends at (the first by name when it names several). Returns the project
/// with its configuration as it was.
pub(super) fn hypothetical_catalog(
    mut project: Project,
    tried: Vec<String>,
) -> Result<(Project, Trial), RecommendError> {
    let mut cfg = project.env.cfg.clone();
    cfg.ecosystem
        .extend(tried.iter().map(|symbol| EcosystemOverride {
            symbol: symbol.clone(),
            effects: Some(HYPOTHETICAL_EFFECTS.map(str::to_owned).to_vec()),
            ..EcosystemOverride::default()
        }));
    let saved = project.set_config(cfg)?;
    let selection = selection_of(&saved.lint, HYPOTHETICAL_RULES.map(str::to_owned).to_vec())?;
    let run = Indexed::build(project, selection, None, Facts::Check)?;
    let (diags, _) = run.diagnose()?;
    let Indexed {
        mut project, index, ..
    } = run;
    drop(index);
    project.set_config(saved)?;
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
        match d.rule_code.as_str() {
            "EFF101" => waits.eff101 += 1,
            "EFF111" => waits.eff111 += 1,
            "EFF103" => queries.eff103 += 1,
            "EFF113" => queries.eff113 += 1,
            _ => {}
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
    let waits = s.if_waits.unwrap_or_default();
    let queries = s.if_queries.unwrap_or_default();
    waits.eff101 + waits.eff111 + queries.eff103 + queries.eff113
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
        !self.below.is_empty() && resolves(name, &self.below)
    }
}

/// Whether some dotted prefix of `name` (itself included) is a module of `modules`: a project
/// module heki resolves the name into.
fn resolves(name: &str, modules: &FxHashSet<String>) -> bool {
    let mut end = name.len();
    loop {
        if modules.contains(&name[..end]) {
            return true;
        }
        match name[..end].rfind('.') {
            Some(dot) => end = dot,
            None => return false,
        }
    }
}

/// The module names of a project's files under one module layout.
struct Names {
    /// Of each file, in the order of `Project::files`: its module and whether it is a package.
    files: Vec<(String, bool)>,
    /// Every module name, those of the files heki could not read included.
    all: FxHashSet<String>,
    root_is_package: bool,
}

impl Names {
    fn under(project: &Project, layout: &ModuleLayout) -> Self {
        let files: Vec<(String, bool)> = project
            .files
            .iter()
            .map(|f| module_of(&f.rel, &f.path, layout))
            .collect();
        let root = &project.env.root;
        let unread = project.unread.iter().map(|u| {
            let rel = rel_posix(&u.path, root);
            module_of(&rel, &u.path, layout).0
        });
        let all = files.iter().map(|(m, _)| m.clone()).chain(unread).collect();
        Self {
            files,
            all,
            root_is_package: layout.root_is_package,
        }
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
            self.root_is_package,
        )?;
        Some(if base.is_empty() {
            imp.imported_name.clone()
        } else {
            format!("{base}.{}", imp.imported_name)
        })
    }

    /// For each module name more than one file of one kind (implementation or stub) defines,
    /// two of those files.
    fn shared(&self, project: &Project) -> BTreeMap<(String, bool), (usize, usize)> {
        let mut first: FxHashMap<(&str, bool), usize> = FxHashMap::default();
        let mut shared = BTreeMap::new();
        for (i, (module, _)) in self.files.iter().enumerate() {
            let key = (
                module.as_str(),
                heki_effect::is_stub(&project.files[i].path),
            );
            match first.get(&key) {
                Some(&j) => {
                    shared.entry((module.clone(), key.1)).or_insert((j, i));
                }
                None => {
                    first.insert(key, i);
                }
            }
        }
        shared
    }
}

/// One import of a file that names no project module now.
struct Unresolved {
    file: usize,
    line: u32,
    name: String,
}

/// The source-root trial (no analysis: it computes the module names `heki check` would give the
/// files with `src` set, and resolves the imports by them). A candidate is a directory below the
/// project root, not a source root, below which the dotted path of an import that names no
/// project module now is a file (`backend/config/env.py` for `import config.env`). An import of
/// the standard library or of a package the catalog covers is no candidate's, and neither is one
/// whose path is below several directories none of which holds the importing file. The candidates
/// resolving the most imports are tried first, each with the ones already taken: one enters the
/// block when its imports resolve, no two files of one kind then share a module name, every
/// import that resolves now still does, and no module whose name changes is named by a contract
/// of the configuration or by an entry of `baseline`.
pub(super) fn source_roots(
    project: &Project,
    baseline: Option<&RatchetBaseline>,
    shown: &dyn Fn(&Path) -> String,
) -> SourceRoots {
    let env = &project.env;
    let current = Names::under(project, &env.layout);
    let roots = &env.layout.source_roots;
    // The directories below which a dotted path is a file of the project: each pair once, and
    // per dotted path how many directories and the first.
    let mut pairs: FxHashSet<(String, String)> = FxHashSet::default();
    let mut below: FxHashMap<String, (usize, String)> = FxHashMap::default();
    for f in &project.files {
        let parts: Vec<&str> = f.rel.split('/').collect();
        for i in 1..parts.len() {
            let dir = parts[..i].join("/");
            if roots.contains(&dir) {
                continue;
            }
            let Some((dotted, _)) = module_name_for_path(Path::new(&parts[i..].join("/"))) else {
                continue;
            };
            if pairs.insert((dotted.clone(), dir.clone())) {
                below.entry(dotted).or_insert((0, dir)).0 += 1;
            }
        }
    }
    let mut out = SourceRoots::default();
    let mut by_dir: BTreeMap<String, Vec<Unresolved>> = BTreeMap::new();
    let mut resolved_now: Vec<(usize, usize)> = Vec::new();
    for (i, f) in project.files.iter().enumerate() {
        let mut lines = None;
        for (k, imp) in f.semantic.imports.iter().enumerate() {
            let Some(name) = current.imported(i, imp) else {
                continue;
            };
            if resolves(&name, &current.all) {
                resolved_now.push((i, k));
                continue;
            }
            let top = name.split('.').next().unwrap_or(&name);
            if imp.level > 0 || heki_env::is_stdlib_module(top) || env.kb.is_opaque_to_layer3(top) {
                continue;
            }
            // The longest dotted prefix that is a file below some directory.
            let mut end = name.len();
            let found = loop {
                if let Some(found) = below.get(&name[..end]) {
                    break Some(found);
                }
                match name[..end].rfind('.') {
                    Some(dot) => end = dot,
                    None => break None,
                }
            };
            let Some((count, first)) = found else {
                continue;
            };
            let dotted = &name[..end];
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
                let lines = lines.get_or_insert_with(|| LineIndex::from_source(&f.source));
                by_dir.entry(dir).or_default().push(Unresolved {
                    file: i,
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
    let named = names_in_configuration(&env.cfg, baseline);
    let mut accepted: Vec<String> = Vec::new();
    out.total = ranked.len();
    for (n, (dir, imports)) in ranked.into_iter().take(MAX_LISTED).enumerate() {
        let examples = imports
            .iter()
            .take(MAX_LISTED)
            .map(|u| ImportAt {
                path: shown(&project.files[u.file].path),
                line: u.line,
                name: u.name.clone(),
            })
            .collect();
        let mut candidate = SourceRoot {
            directory: dir.clone(),
            imports: imports.len(),
            examples,
            in_block: false,
            reason: String::new(),
        };
        if n >= MAX_SOURCE_ROOT_TRIALS {
            candidate.reason = format!(
                "not tried: only the first {MAX_SOURCE_ROOT_TRIALS} directories by imports are"
            );
            out.candidates.push(candidate);
            continue;
        }
        let mut with: Vec<String> = roots.iter().chain(&accepted).cloned().collect();
        with.push(dir.clone());
        let layout = ModuleLayout {
            source_roots: with.clone(),
            root_is_package: false,
        };
        let trial = Names::under(project, &layout);
        match refusal(
            project,
            &current,
            &trial,
            &imports,
            &resolved_now,
            &named,
            shown,
        ) {
            Some(reason) => candidate.reason = reason,
            None => {
                candidate.in_block = true;
                candidate.reason = "its imports resolve, and no import, contract, or baseline \
                    entry loses its module"
                    .to_owned();
                accepted.push(dir);
                out.src = Some(with);
            }
        }
        out.candidates.push(candidate);
    }
    out
}

/// The dotted names the configuration's contracts and `baseline`'s entries use, with what names
/// them: a module whose name changes must not be among them.
fn names_in_configuration<'c>(
    cfg: &'c HekiConfig,
    baseline: Option<&'c RatchetBaseline>,
) -> Vec<(String, &'static str)> {
    fn strings(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::String(s) => out.push(s.clone()),
            serde_json::Value::Array(items) => items.iter().for_each(|v| strings(v, out)),
            serde_json::Value::Object(map) => map.values().for_each(|v| strings(v, out)),
            _ => {}
        }
    }
    let mut contracts = Vec::new();
    for value in [
        serde_json::to_value(&cfg.write_owners),
        serde_json::to_value(&cfg.read_owners),
        serde_json::to_value(&cfg.call_owners),
        serde_json::to_value(&cfg.effect_boundaries),
        serde_json::to_value(&cfg.readonly_returns),
        serde_json::to_value(&cfg.invalidation),
    ]
    .into_iter()
    .flatten()
    {
        strings(&value, &mut contracts);
    }
    let mut out: Vec<(String, &'static str)> = contracts
        .into_iter()
        .map(|s| (s, "a contract of the configuration"))
        .collect();
    for key in baseline.iter().flat_map(|b| b.counts.keys()) {
        out.push((key.target.clone(), "the baseline"));
        out.push((key.qualname.clone(), "the baseline"));
    }
    out
}

/// Whether `text` names the dotted name `name` as a whole: not as part of a longer name.
fn names_module(text: &str, name: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(name).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + name.len()..].chars().next();
        !before.is_some_and(ident) && !after.is_some_and(ident)
    })
}

/// Why the layout `trial` stays out of the block, or `None` when it may enter it.
fn refusal(
    project: &Project,
    current: &Names,
    trial: &Names,
    imports: &[Unresolved],
    resolved_now: &[(usize, usize)],
    named: &[(String, &'static str)],
    shown: &dyn Fn(&Path) -> String,
) -> Option<String> {
    if let Some(u) = imports.iter().find(|u| !resolves(&u.name, &trial.all)) {
        return Some(format!("`{}` would still name no project module", u.name));
    }
    let before = current.shared(project);
    if let Some(((module, _), (a, b))) = trial
        .shared(project)
        .into_iter()
        .find(|(k, _)| !before.contains_key(k))
    {
        return Some(format!(
            "{} and {} would both define module `{module}`",
            shown(&project.files[a].path),
            shown(&project.files[b].path)
        ));
    }
    for &(i, k) in resolved_now {
        let imp = &project.files[i].semantic.imports[k];
        if !trial
            .imported(i, imp)
            .is_some_and(|n| resolves(&n, &trial.all))
        {
            let name = current.imported(i, imp).unwrap_or_default();
            return Some(format!(
                "`{name}` in {}, which names a project module now, would not",
                shown(&project.files[i].path)
            ));
        }
    }
    // A module whose name changes loses the part its new name drops (`backend` of
    // `backend.config.env`, now `config.env`): a name that spells it no longer names it.
    let mut dropped: BTreeSet<&str> = BTreeSet::new();
    for ((old, _), (new, _)) in current.files.iter().zip(&trial.files) {
        if old != new {
            let kept = old
                .strip_suffix(new.as_str())
                .and_then(|p| p.strip_suffix('.'));
            dropped.insert(kept.unwrap_or(old));
        }
    }
    for module in dropped {
        if let Some((_, what)) = named.iter().find(|(text, _)| names_module(text, module)) {
            return Some(format!(
                "{what} names `{module}`, whose modules would be named otherwise"
            ));
        }
    }
    None
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

/// What a trial counted for a name, as the text says it.
fn hypothetical(counts: &[(&str, usize)], tried: bool) -> String {
    if !tried {
        return "not tried".to_owned();
    }
    let found: Vec<String> = counts
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(code, n)| format!("{code} {n}"))
        .collect();
    if found.is_empty() {
        "-".to_owned()
    } else {
        found.join(", ")
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
            let w = s.if_waits.unwrap_or_default();
            let q = s.if_queries.unwrap_or_default();
            (
                hypothetical(
                    &[("EFF101", w.eff101), ("EFF111", w.eff111)],
                    s.if_waits.is_some(),
                ),
                hypothetical(
                    &[("EFF103", q.eff103), ("EFF113", q.eff113)],
                    s.if_queries.is_some(),
                ),
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
    fn a_name_is_named_only_as_a_whole() {
        for (text, name, named) in [
            ("backend.config.env.load", "backend", true),
            ("backend.config.*:*", "backend", true),
            ("app.backend:Store.items", "backend", true),
            ("backend", "backend", true),
            ("backends.x", "backend", false),
            ("my_backend.x", "backend", false),
            ("x.backend_y", "backend", false),
            ("config.env", "backend", false),
            ("pkg.backend.config", "pkg.backend", true),
        ] {
            assert_eq!(names_module(text, name), named, "{text} {name}");
        }
    }

    #[test]
    fn a_name_resolves_into_a_module_it_is_or_is_below() {
        let modules: FxHashSet<String> = ["config.env", "app"]
            .map(str::to_owned)
            .into_iter()
            .collect();
        for (name, resolved) in [
            ("config.env", true),
            ("config.env.load", true),
            ("config", false),
            ("config.envs", false),
            ("app.models.Order", true),
            ("application", false),
        ] {
            assert_eq!(resolves(name, &modules), resolved, "{name}");
        }
    }
}
