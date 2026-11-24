//! `IMP201` import ownership (`[[tool.heki.import-owners]]`): which modules a module may import,
//! and `IMP202`, its strict sibling for the imports whose module heki cannot name; `IMP101`,
//! import cycles ([`cycles`]). Which module an import loads is decided by `heki_effect::imports`
//! alone.

mod cycles;

pub(crate) use cycles::imp101;
pub use cycles::import_cycle_files;

use crate::mut_rules::{contract_places, mode, reports_unresolved};
use crate::patterns::PartIndex;
use crate::{CheckContext, GLOB_CHARACTERS, NamePart, RuleConfigError};
use heki_diagnostics::{Diagnostic, RelatedLocation};
use heki_effect::imports::{ImportForm, ImportGraph, ImportSite, ModuleRef, Target, Timing};
use heki_env::{HekiConfig, ImportOwnerRule};
use heki_ir::walk::{Answer, Budget, Visit, reach_breadth_first};
use rustc_hash::{FxHashMap, FxHashSet};

const SECTION: &str = "import-owners";

/// `reach` values; the first is the default.
const REACH: &[&str] = &["statement", "import-time", "all"];

/// `type-checking` values; the first is the default (imports under `TYPE_CHECKING` never run).
const TYPE_CHECKING: &[&str] = &["ignore", "report"];

/// The groups a module pattern can name instead of modules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Stdlib,
    Project,
    ThirdParty,
}

impl Group {
    const NAMES: [(&'static str, Self); 3] = [
        ("<stdlib>", Self::Stdlib),
        ("<project>", Self::Project),
        ("<third-party>", Self::ThirdParty),
    ];

    fn of(module: Imported<'_>) -> Self {
        if module.project {
            Self::Project
        } else if heki_env::is_stdlib_module(module.name) {
            Self::Stdlib
        } else {
            Self::ThirdParty
        }
    }
}

/// A module an import loads, as a contract judges it.
#[derive(Debug, Clone, Copy)]
struct Imported<'n> {
    name: &'n str,
    /// One of the project's modules (`<project>`).
    project: bool,
}

impl<'n> Imported<'n> {
    fn of(graph: &'n ImportGraph<'_>, m: ModuleRef) -> Self {
        Self {
            name: graph.name(m),
            project: matches!(m, ModuleRef::Project(_)),
        }
    }
}

/// A module pattern: a whole module name or a glob (`*` spans dots), or a group.
#[derive(Debug, Clone)]
enum ImportPattern {
    Module(NamePart),
    Group(Group),
}

impl ImportPattern {
    fn new(text: &str, context: &'static str) -> Result<Self, RuleConfigError> {
        if text.starts_with('<') {
            return (Group::NAMES.iter())
                .find(|(name, _)| *name == text)
                .map(|&(_, group)| Self::Group(group))
                .ok_or_else(|| RuleConfigError::ImportGroup {
                    pattern: text.to_owned(),
                    context,
                });
        }
        Ok(Self::Module(NamePart::new(text, text, context)?))
    }

    fn matches(&self, module: Imported<'_>) -> bool {
        match self {
            Self::Module(part) => part.is_match(module.name),
            Self::Group(group) => Group::of(module) == *group,
        }
    }

    /// Whether some module whose name starts with `head` may match.
    fn may_match_under(&self, head: &str) -> bool {
        match self {
            Self::Module(part) => match part.glob() {
                None => part.text().len() > head.len() && part.text().starts_with(head),
                Some(_) => {
                    let literal = literal_prefix(part.text());
                    literal.starts_with(head) || head.starts_with(literal)
                }
            },
            Self::Group(_) => true,
        }
    }

    /// Whether every module whose name starts with `head` matches: a glob ending in its only
    /// `*` after a prefix of `head` (`pkg.plugins.*`, `pkg.*`, `*`).
    fn matches_all_under(&self, head: &str) -> bool {
        match self {
            Self::Module(part) => {
                let literal = literal_prefix(part.text());
                part.text().len() == literal.len() + 1
                    && part.text().ends_with('*')
                    && head.starts_with(literal)
            }
            Self::Group(_) => false,
        }
    }
}

/// The text of a pattern before its first character with a glob meaning.
fn literal_prefix(text: &str) -> &str {
    text.find(GLOB_CHARACTERS).map_or(text, |at| &text[..at])
}

/// Whether a pattern is one of the allowed or of the forbidden imports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Allowed,
    Forbidden,
}

/// The allowed or the forbidden imports of a contract.
#[derive(Debug, Default)]
struct Imports {
    patterns: Vec<ImportPattern>,
}

impl Imports {
    fn is_set(&self) -> bool {
        !self.patterns.is_empty()
    }

    fn groups(&self) -> impl Iterator<Item = Group> + '_ {
        self.patterns.iter().filter_map(|p| match p {
            ImportPattern::Group(group) => Some(*group),
            ImportPattern::Module(_) => None,
        })
    }
}

/// What a contract walks from an importer (`reach`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// Each import site of the importer.
    Statement,
    /// What loading the importer loads while it loads.
    ImportTime,
    /// What any chain of imports that run leads to.
    All,
}

/// Why an import breaks a contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Why {
    Forbidden,
    NotAllowed,
    /// The contract allows no import: it sets neither `allowed-imports` nor `forbidden-imports`.
    AllowsNone,
}

struct ImportContract<'c> {
    /// How a report names the contract ([`heki_env::contract_label`]).
    label: String,
    note: Option<&'c str>,
    allowed: Imports,
    forbidden: Imports,
    type_checking: bool,
    reach: Reach,
    report_unresolved: bool,
}

impl ImportContract<'_> {
    /// Whether an import site of `timing` counts: every class that runs (and `type-checking`
    /// with the key); with `reach = "import-time"`, only what runs while the module loads.
    fn counts(&self, timing: Answer<Timing>) -> bool {
        match (self.reach, timing) {
            (Reach::ImportTime, timing) => timing == Answer::Known(Timing::Body),
            (_, Answer::Known(Timing::TypeChecking)) => self.type_checking,
            _ => true,
        }
    }

    /// Why an import a site may make, of a module some of whose names heki cannot tell, may
    /// break the contract.
    fn open_why(&self) -> Why {
        if self.forbidden.is_set() {
            Why::Forbidden
        } else if self.allowed.is_set() {
            Why::NotAllowed
        } else {
            Why::AllowsNone
        }
    }

    /// The text of a report saying why the import breaks it.
    fn why_text(&self, why: Why, may: bool) -> String {
        let label = &self.label;
        match (why, may) {
            (Why::Forbidden, false) => format!("a forbidden import of {label}"),
            (Why::Forbidden, true) => format!("which may be a forbidden import of {label}"),
            (Why::NotAllowed, false) => format!("not among the allowed imports of {label}"),
            (Why::NotAllowed, true) => {
                format!("which may be outside the allowed imports of {label}")
            }
            (Why::AllowsNone, _) => format!("and {label} allows no imports"),
        }
    }
}

/// An `ignored-imports` pair: an importer pattern (indexed) and the imported one.
struct IgnoredPair {
    contract: usize,
    imported: ImportPattern,
}

/// The `import-owners` contracts, their patterns indexed together.
struct ImportContracts<'c> {
    contracts: Vec<ImportContract<'c>>,
    /// The `importers` patterns, with their contracts; the contracts without the key judge
    /// every module.
    importers: (PartIndex, Vec<usize>),
    every_module: Vec<usize>,
    exempt: (PartIndex, Vec<usize>),
    /// The module patterns of `allowed-imports` and `forbidden-imports`.
    imports: (PartIndex, Vec<(usize, Side)>),
    /// The importer sides of `ignored-imports`, each with its pair.
    ignored: (PartIndex, Vec<IgnoredPair>),
}

/// Module patterns of the contracts, parsed and indexed with what each belongs to.
fn indexed<T>(
    entries: Vec<(NamePart, T)>,
    context: &'static str,
) -> Result<(PartIndex, Vec<T>), RuleConfigError> {
    let (parts, owners): (Vec<NamePart>, Vec<T>) = entries.into_iter().unzip();
    Ok((PartIndex::new(parts.iter().enumerate(), context)?, owners))
}

fn module_part(text: &str, context: &'static str) -> Result<NamePart, RuleConfigError> {
    NamePart::new(text, text, context)
}

impl<'c> ImportContracts<'c> {
    fn new(rules: &'c [ImportOwnerRule]) -> Result<Self, RuleConfigError> {
        let mut contracts = Vec::new();
        let (mut importers, mut every_module, mut exempt) = (Vec::new(), Vec::new(), Vec::new());
        let (mut imports, mut ignored) = (Vec::new(), Vec::new());
        for (c, rule) in rules.iter().enumerate() {
            match &rule.importers {
                None => every_module.push(c),
                Some(patterns) => {
                    for text in patterns {
                        importers.push((module_part(text, "import-owners.importers")?, c));
                    }
                }
            }
            for text in &rule.exempt_importers {
                exempt.push((module_part(text, "import-owners.exempt-importers")?, c));
            }
            let mut sides = [Imports::default(), Imports::default()];
            for (side, texts, context) in [
                (
                    Side::Allowed,
                    &rule.allowed_imports,
                    "import-owners.allowed-imports",
                ),
                (
                    Side::Forbidden,
                    &rule.forbidden_imports,
                    "import-owners.forbidden-imports",
                ),
            ] {
                for text in texts {
                    let pattern = ImportPattern::new(text, context)?;
                    if let ImportPattern::Module(part) = &pattern {
                        imports.push((part.clone(), (c, side)));
                    }
                    sides[side as usize].patterns.push(pattern);
                }
            }
            for text in &rule.ignored_imports {
                let context = "import-owners.ignored-imports";
                let (importer, imported) = text
                    .split_once("->")
                    .map(|(a, b)| (a.trim(), b.trim()))
                    .filter(|(a, b)| !a.is_empty() && !b.is_empty())
                    .ok_or_else(|| RuleConfigError::ImportPair {
                        pattern: text.clone(),
                        context,
                    })?;
                let imported = ImportPattern::new(imported, context)?;
                ignored.push((
                    module_part(importer, context)?,
                    IgnoredPair {
                        contract: c,
                        imported,
                    },
                ));
            }
            let [allowed, forbidden] = sides;
            contracts.push(ImportContract {
                label: heki_env::contract_label(SECTION, c, &rule.name),
                note: rule.message.as_deref(),
                allowed,
                forbidden,
                type_checking: mode(
                    rule.type_checking.as_deref(),
                    TYPE_CHECKING,
                    "`ignore` or `report`",
                    SECTION,
                    &rule.name,
                    "type-checking",
                )? == "report",
                reach: match mode(
                    rule.reach.as_deref(),
                    REACH,
                    "`statement`, `import-time`, or `all`",
                    SECTION,
                    &rule.name,
                    "reach",
                )? {
                    "statement" => Reach::Statement,
                    "import-time" => Reach::ImportTime,
                    _ => Reach::All,
                },
                report_unresolved: reports_unresolved(
                    rule.unresolved.as_deref(),
                    SECTION,
                    &rule.name,
                )?,
            });
        }
        Ok(Self {
            contracts,
            importers: indexed(importers, "import-owners.importers")?,
            every_module,
            exempt: indexed(exempt, "import-owners.exempt-importers")?,
            imports: indexed(imports, "import-owners.allowed-imports")?,
            ignored: indexed(ignored, "import-owners.ignored-imports")?,
        })
    }

    /// The contracts judging the module `importer`, ascending.
    fn judging(&self, importer: &str) -> Vec<usize> {
        let found = |(index, owners): &(PartIndex, Vec<usize>)| {
            let mut ids = Vec::new();
            index.find(importer, &mut ids);
            ids.into_iter().map(|i| owners[i]).collect::<Vec<usize>>()
        };
        let exempt = found(&self.exempt);
        let mut out: Vec<usize> = (found(&self.importers).into_iter())
            .chain(self.every_module.iter().copied())
            .filter(|c| !exempt.contains(c))
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The contracts' module patterns that `name` matches, with the side of each.
    fn matched(&self, name: &str) -> Vec<(usize, Side)> {
        let (index, owners) = &self.imports;
        let mut ids = Vec::new();
        index.find(name, &mut ids);
        ids.into_iter().map(|i| owners[i]).collect()
    }

    /// Why importing `module` breaks contract `c`, by the module patterns it `matched`; `None`
    /// when it does not.
    fn violates(&self, c: usize, module: Imported<'_>, matched: &[(usize, Side)]) -> Option<Why> {
        let contract = &self.contracts[c];
        let group = Group::of(module);
        let named = |imports: &Imports, side: Side| {
            imports.is_set()
                && (imports.groups().any(|g| g == group) || matched.contains(&(c, side)))
        };
        let allowed = named(&contract.allowed, Side::Allowed);
        match (contract.allowed.is_set(), contract.forbidden.is_set()) {
            (false, false) => Some(Why::AllowsNone),
            (_, true) => {
                (named(&contract.forbidden, Side::Forbidden) && !allowed).then_some(Why::Forbidden)
            }
            (true, false) => (!allowed).then_some(Why::NotAllowed),
        }
    }

    /// Whether contract `c` accepts the import of `module` by `importer` (`ignored-imports`).
    fn ignores(&self, c: usize, importer: &str, module: Imported<'_>) -> bool {
        let (index, pairs) = &self.ignored;
        let mut ids = Vec::new();
        index.find(importer, &mut ids);
        (ids.into_iter().map(|i| &pairs[i])).any(|p| p.contract == c && p.imported.matches(module))
    }

    /// Why importing `module` from `importer` breaks contract `c`, unless it accepts it.
    fn judge(&self, c: usize, importer: &str, module: Imported<'_>) -> Option<Why> {
        if self.ignores(c, importer, module) {
            return None;
        }
        self.violates(c, module, &self.matched(module.name))
    }

    /// Whether importing some module whose name starts with `head` may break contract `c`.
    fn may_break_under(&self, c: usize, head: &str) -> bool {
        let contract = &self.contracts[c];
        let (allowed, forbidden) = (&contract.allowed, &contract.forbidden);
        if !allowed.is_set() && !forbidden.is_set() {
            return true;
        }
        if (allowed.patterns.iter()).any(|p| p.matches_all_under(head)) {
            return false;
        }
        !forbidden.is_set() || (forbidden.patterns.iter()).any(|p| p.may_match_under(head))
    }
}

/// What a site whose module heki cannot name may import.
#[derive(Debug, Clone)]
enum OpenImport {
    /// A name the code does not fix.
    Unknown,
    /// Some module whose name starts with this text.
    Under(String),
    /// One of these names.
    AnyOf(Vec<String>),
    /// `from package import name`: the submodule, or only the package.
    Submodule { package: String, submodule: String },
}

impl OpenImport {
    /// The report's target: what a baseline entry counts it by.
    fn target(&self) -> String {
        match self {
            Self::Unknown => "*".to_owned(),
            Self::Under(head) => format!("{head}*"),
            Self::AnyOf(names) => names.join("|"),
            Self::Submodule { submodule, .. } => submodule.clone(),
        }
    }

    fn text(&self) -> String {
        match self {
            Self::Unknown => "a module heki cannot name".to_owned(),
            Self::Under(head) => format!("a module whose name starts with `{head}`"),
            Self::AnyOf(names) => quoted(names, "or"),
            Self::Submodule { package, submodule } => {
                format!("`{submodule}`, or only `{package}`")
            }
        }
    }
}

/// `` `a`, `b` <word> `c` ``.
fn quoted(names: &[String], word: &str) -> String {
    let quoted: Vec<String> = names.iter().map(|n| format!("`{n}`")).collect();
    match quoted.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} {word} {last}", rest.join(", ")),
        _ => quoted.concat(),
    }
}

/// What a contract says about one import site.
enum Verdict {
    Fine,
    /// The site imports these modules (several for a by-name import of one of them), each
    /// breaking the contract.
    Breaks(Vec<String>, Why),
    /// The site may import a module that breaks the contract.
    Open(OpenImport, Why),
}

/// One answer of a part of a by-name import: whether the module it names breaks the contract.
enum Part {
    Breaks(String, Why),
    Fine,
    Open,
}

/// The step of a walk into a module: from the module and site importing it, which loads the
/// module itself or, first, its parent package.
#[derive(Debug, Clone, Copy)]
struct Hop {
    from: u32,
    site: usize,
    parent: bool,
}

/// A related location of a report: a step of its chain, into the module it names, or the site
/// at the end of the chain whose module heki cannot name.
#[derive(Debug, Clone)]
enum Related<'s> {
    Hop(Hop, ModuleRef),
    /// A site of a module, and what it may import.
    Site(u32, &'s ImportSite, String),
}

/// The reports of one check: the contracts, the graph, and the diagnostics they make.
struct Run<'r, 'c, 'g> {
    cx: &'r CheckContext<'c>,
    contracts: &'r ImportContracts<'r>,
    graph: &'r ImportGraph<'g>,
    out: &'r mut Vec<Diagnostic>,
}

/// `IMP201` and `IMP202`: the imports of the modules each `import-owners` contract judges that
/// it does not allow, and, with `unresolved = "report"`, the imports whose module heki cannot
/// name that it may not allow.
pub(crate) fn imp201_imp202(
    cx: &CheckContext<'_>,
    out: &mut Vec<Diagnostic>,
) -> Result<(), RuleConfigError> {
    if cx.config.import_owners.is_empty()
        || !cx.selection.is_enabled("IMP201") && !cx.selection.is_enabled("IMP202")
    {
        return Ok(());
    }
    let contracts = ImportContracts::new(&cx.config.import_owners)?;
    let walks = (contracts.contracts.iter()).any(|c| c.reach != Reach::Statement);
    let checked: FxHashSet<&str> = (cx.units.iter())
        .map(|u| u.input.module_name.as_str())
        .collect();
    // A walk reads the imports of every module it reaches; the statements of a check, only
    // those of the checked modules.
    let graph = ImportGraph::resolving(cx.index.files.values(), cx.decls, |m| {
        walks || checked.contains(m)
    });
    // The checked modules, ascending: a module several files define is checked in its owner.
    let mut ids: Vec<u32> = (cx.units.iter())
        .filter_map(|u| {
            let m = graph.module(&u.input.module_name)?;
            (graph.path(m) == u.input.path.as_path()).then_some(m)
        })
        .collect();
    ids.sort_unstable();
    let Some(&last) = ids.last() else {
        return Ok(());
    };
    let mut run = Run {
        cx,
        contracts: &contracts,
        graph: &graph,
        out,
    };
    for &m in &ids {
        for c in contracts.judging(graph.name(ModuleRef::Project(m))) {
            if contracts.contracts[c].reach == Reach::Statement {
                run.statements(c, m);
            }
        }
    }
    for (c, contract) in contracts.contracts.iter().enumerate() {
        if contract.reach == Reach::Statement {
            continue;
        }
        // The importers walk in the order of the modules and share one budget, as `heki check`
        // and the language server both ask them: a walk after the last checked module changes
        // no answer before it.
        let budget = graph.budget("import_owners");
        for m in 0..=last {
            if contracts
                .judging(graph.name(ModuleRef::Project(m)))
                .contains(&c)
            {
                run.walk(c, m, &budget, ids.binary_search(&m).is_ok());
            }
        }
    }
    Ok(())
}

impl<'r, 'g> Run<'r, '_, 'g> {
    /// The sites of module `m` that contract `c` counts.
    fn counted(&self, c: usize, m: u32) -> Vec<(usize, &'g ImportSite, &'r Target)> {
        let contract = &self.contracts.contracts[c];
        let graph: &'r ImportGraph<'g> = self.graph;
        (graph.sites(m).into_iter().flatten().enumerate())
            .filter(|(_, (site, _))| contract.counts(site.timing))
            .map(|(j, (site, target))| (j, *site, target))
            .collect()
    }

    /// Contract `c` on the statements of module `m`: each import site it counts.
    fn statements(&mut self, c: usize, m: u32) {
        for (_, site, target) in self.counted(c, m) {
            let verdict = self.verdict(c, m, target, false);
            self.emit(c, m, (site, target), &[], verdict);
        }
    }

    /// What contract `c` says about an import of `target` by module `m`. `walked`: the walk of
    /// a `reach` contract judges the module a site certainly loads where it reaches it, so only
    /// the rest of the target is judged here.
    fn verdict(&self, c: usize, m: u32, target: &Target, walked: bool) -> Verdict {
        let (contracts, graph) = (self.contracts, self.graph);
        let importer = graph.name(ModuleRef::Project(m));
        let one = |module: ModuleRef| contracts.judge(c, importer, Imported::of(graph, module));
        let open = |import| Verdict::Open(import, contracts.contracts[c].open_why());
        match target {
            Target::Module(module) if !walked => match one(*module) {
                Some(why) => Verdict::Breaks(vec![graph.name(*module).to_owned()], why),
                None => Verdict::Fine,
            },
            Target::PackageOrSubmodule { package, submodule } => {
                if !walked && let Some(why) = one(*package) {
                    return Verdict::Breaks(vec![graph.name(*package).to_owned()], why);
                }
                let imported = Imported {
                    name: submodule,
                    project: matches!(package, ModuleRef::Project(_)),
                };
                if contracts.judge(c, importer, imported).is_none() {
                    return Verdict::Fine;
                }
                open(OpenImport::Submodule {
                    package: graph.name(*package).to_owned(),
                    submodule: submodule.clone(),
                })
            }
            Target::AnyOf(targets) => {
                let parts: Vec<Part> = (targets.iter())
                    .map(|t| match t {
                        Target::Module(module) => match one(*module) {
                            Some(why) => Part::Breaks(graph.name(*module).to_owned(), why),
                            None => Part::Fine,
                        },
                        Target::Missing(_) => Part::Fine,
                        Target::Under(head) if !contracts.may_break_under(c, head) => Part::Fine,
                        _ => Part::Open,
                    })
                    .collect();
                let broken: Vec<(String, Why)> = (parts.iter())
                    .filter_map(|p| match p {
                        Part::Breaks(name, why) => Some((name.clone(), *why)),
                        _ => None,
                    })
                    .collect();
                if !broken.is_empty() && broken.len() == parts.len() {
                    let why = broken[0].1;
                    return Verdict::Breaks(broken.into_iter().map(|(n, _)| n).collect(), why);
                }
                if broken.is_empty() && !parts.iter().any(|p| matches!(p, Part::Open)) {
                    return Verdict::Fine;
                }
                let names = (targets.iter())
                    .map(|t| match t {
                        Target::Under(head) => format!("{head}*"),
                        Target::Missing(name) => name.clone(),
                        t => t
                            .certain()
                            .map_or_else(|| "*".to_owned(), |m| graph.name(m).to_owned()),
                    })
                    .collect();
                open(OpenImport::AnyOf(names))
            }
            Target::Under(head) if contracts.may_break_under(c, head) => {
                open(OpenImport::Under(head.clone()))
            }
            Target::Open => open(OpenImport::Unknown),
            Target::Module(_) | Target::Under(_) | Target::Missing(_) => Verdict::Fine,
        }
    }

    /// The walk of contract `c` from the importer `m`: every module a counted import of `m`
    /// leads to through the project's modules, each by a shortest chain. `report`: `m` is
    /// checked, so its findings are reported.
    fn walk(&mut self, c: usize, m: u32, budget: &Budget, report: bool) {
        let (contracts, graph) = (self.contracts, self.graph);
        let contract = &contracts.contracts[c];
        // The step into each module from the first module listing it: the nearest one.
        let mut into: FxHashMap<ModuleRef, Hop> = FxHashMap::default();
        let mut reached: Vec<ModuleRef> = Vec::new();
        let walked = reach_breadth_first::<_, ()>(
            budget,
            [ModuleRef::Project(m)],
            |k, next| {
                let ModuleRef::Project(from) = k else {
                    return;
                };
                let from_name = graph.name(k);
                for (j, (site, target)) in graph.sites(from).into_iter().flatten().enumerate() {
                    let Some(t) = target.certain().filter(|_| contract.counts(site.timing)) else {
                        continue;
                    };
                    if contracts.ignores(c, from_name, Imported::of(graph, t)) {
                        continue;
                    }
                    for w in graph.parents(t).into_iter().chain([t]) {
                        if graph.holds(w, m) {
                            continue;
                        }
                        into.entry(w).or_insert(Hop {
                            from,
                            site: j,
                            parent: w != t,
                        });
                        next.push(w);
                    }
                }
            },
            |k| {
                reached.push(k);
                Visit::Expand
            },
        );
        if walked.is_open() || !report {
            return;
        }
        let chain_to = |module: ModuleRef| {
            let mut hops = Vec::new();
            let mut at = module;
            while at != ModuleRef::Project(m) {
                let hop = into[&at];
                hops.push(Related::Hop(hop, at));
                at = ModuleRef::Project(hop.from);
            }
            hops.reverse();
            hops
        };
        // A chain starts at a site of the importer.
        let first_site = |chain: &[Related<'_>]| match chain.first() {
            Some(Related::Hop(hop, _)) => graph.sites(m).map(|s| (s[hop.site].0, &s[hop.site].1)),
            _ => None,
        };
        for &module in &reached[1..] {
            let imported = Imported::of(graph, module);
            let Some(why) = contracts.violates(c, imported, &contracts.matched(imported.name))
            else {
                continue;
            };
            let chain = chain_to(module);
            let site = first_site(&chain).expect("a module reached has a chain to it");
            let verdict = Verdict::Breaks(vec![imported.name.to_owned()], why);
            self.emit(c, m, site, &chain[1..], verdict);
        }
        // The sites whose module a walk cannot take: judged where the chain to them ends.
        for &module in &reached {
            let ModuleRef::Project(r) = module else {
                continue;
            };
            for (_, site, target) in self.counted(c, r) {
                if matches!(target, Target::Module(_)) {
                    continue;
                }
                let verdict = self.verdict(c, r, target, true);
                let what = match &verdict {
                    Verdict::Breaks(names, _) => quoted(names, "or"),
                    Verdict::Open(import, _) => import.text(),
                    Verdict::Fine => continue,
                };
                let mut chain = chain_to(module);
                match first_site(&chain) {
                    Some(first) => {
                        chain.push(Related::Site(r, site, what));
                        self.emit(c, m, first, &chain[1..], verdict);
                    }
                    None => self.emit(c, m, (site, target), &[], verdict),
                }
            }
        }
    }

    /// Report `verdict` of contract `c` at `site` of the importer `m`, with the rest of the
    /// chain from it as related locations: `IMP201` when the import breaks the contract,
    /// `IMP202` when it may.
    fn emit(
        &mut self,
        c: usize,
        m: u32,
        (site, target): (&ImportSite, &Target),
        chain: &[Related<'_>],
        verdict: Verdict,
    ) {
        let contract = &self.contracts.contracts[c];
        let graph = self.graph;
        let importer = graph.name(ModuleRef::Project(m));
        let unit = (self.cx.unit(graph.path(m))).expect("reports are of checked modules");
        let when = when_text(site, Some(importer));
        // A report at the start of a chain names the module the site loads, then what it
        // reaches through it.
        let first = match chain {
            [] => None,
            _ => target.certain().map(|t| graph.name(t)),
        };
        let (code, target, message) = match verdict {
            Verdict::Fine => return,
            Verdict::Breaks(names, why) => {
                let named = quoted(&names, "or");

                let why = contract.why_text(why, false);
                let message = match first {
                    Some(first) if first != names[0] => {
                        format!(
                            "`{}` imports `{first}`{when}, and {named} through it, {why}",
                            site.qualname
                        )
                    }
                    _ => format!("`{}` imports {named}{when}, {why}", site.qualname),
                };
                ("IMP201", names.join("|"), message)
            }
            Verdict::Open(import, why) => {
                if !contract.report_unresolved {
                    return;
                }
                let why = contract.why_text(why, true);
                let message = match first {
                    Some(first) => format!(
                        "`{}` imports `{first}`{when}, and through it {}, {why}",
                        site.qualname,
                        import.text()
                    ),
                    None => format!("`{}` imports {}{when}, {why}", site.qualname, import.text()),
                };
                ("IMP202", import.target(), message)
            }
        };
        let related: Vec<RelatedLocation> = chain.iter().map(|step| self.related(step)).collect();
        let help = contract.note.map(str::to_owned);
        let (span, qualname) = (site.span, site.qualname.as_str());
        let cx = self.cx;
        if let Some(d) = cx.emit(
            self.out,
            code,
            unit,
            span,
            qualname,
            target,
            message,
            help,
            &[],
        ) {
            d.related = related;
            d.contracts = contract_places([c]);
        }
    }

    /// Where a step of a chain is, and what it does.
    fn related(&self, step: &Related<'_>) -> RelatedLocation {
        let graph = self.graph;
        let (from, site, message) = match *step {
            Related::Hop(hop, to) => {
                let (site, message) = hop_text(graph, hop.from, hop.site, to, hop.parent);
                (hop.from, site, message)
            }
            Related::Site(r, site, ref what) => {
                let from = graph.name(ModuleRef::Project(r));
                let message = format!("`{from}` imports {what}{}", when_text(site, None));
                (r, site, message)
            }
        };
        self.cx.related_at(graph.path(from), site.span, message)
    }
}

/// The site `j` of the project module `from` and what its step of an import chain into `to`
/// does, as a related location says it: it imports `to`, or a module whose package `to` is,
/// which it loads first (`parent`).
fn hop_text<'g>(
    graph: &ImportGraph<'g>,
    from: u32,
    j: usize,
    to: ModuleRef,
    parent: bool,
) -> (&'g ImportSite, String) {
    let (site, target) = &graph.sites(from).expect("a chain steps from sites")[j];
    let from = graph.name(ModuleRef::Project(from));
    let when = when_text(site, None);
    let to_name = graph.name(to);
    let message = match target.certain().filter(|_| parent) {
        Some(t) => format!(
            "`{from}` imports `{}`{when}, which loads its package `{to_name}` first",
            graph.name(t)
        ),
        None => format!("`{from}` imports `{to_name}`{when}"),
    };
    (site, message)
}

/// How the import at `site` is made, and when it runs: in the module body of `module` when it
/// is named (`None`: of the module the related location is in).
fn when_text(site: &ImportSite, module: Option<&str>) -> String {
    let by_name = if site.form == ImportForm::ByName {
        " by name"
    } else {
        ""
    };
    let when = match (site.timing, module) {
        (Answer::Known(Timing::Body), Some(module)) => format!(" while `{module}` loads"),
        (Answer::Known(Timing::Body), None) => " while it loads".to_owned(),
        (Answer::Known(Timing::Lazy), _) => " at its first use".to_owned(),
        (Answer::Known(Timing::Function), Some(_)) => " when it runs".to_owned(),
        (Answer::Known(Timing::Function), None) => format!(" in `{}`", site.qualname),
        (Answer::Known(Timing::TypeChecking), _) => " under `TYPE_CHECKING`".to_owned(),
        (Answer::Open, _) => String::new(),
    };
    format!("{by_name}{when}")
}

/// Whether `config` declares an `import-owners` contract walking what its importers import
/// (`reach` other than `"statement"`): its reports change with the imports of every module its
/// walks reach.
pub fn walks_imports(config: &HekiConfig) -> bool {
    (config.import_owners.iter()).any(|r| r.reach.as_deref().is_some_and(|r| r != "statement"))
}

/// The modules among `modules`, each with what it stands for (its file), that an
/// `import-owners` contract walking imports judges.
pub fn walking_importers<'m, T>(
    config: &HekiConfig,
    modules: impl IntoIterator<Item = (&'m str, T)>,
) -> Result<Vec<T>, RuleConfigError> {
    let contracts = ImportContracts::new(&config.import_owners)?;
    let walking = |c: &usize| contracts.contracts[*c].reach != Reach::Statement;
    Ok((modules.into_iter())
        .filter(|(module, _)| contracts.judging(module).iter().any(walking))
        .map(|(_, of)| of)
        .collect())
}
