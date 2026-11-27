//! `IMP201` import boundaries (`[[tool.heki.import-boundaries]]`): which modules a module may import,
//! and `IMP202`, its strict sibling for the imports whose module heki cannot name; `IMP101`,
//! import cycles ([`cycles`]); `IMP102`, circular imports that fail ([`circular`]); `IMP104`,
//! development code imported by package code ([`development`]); `IMP301` and `IMP302`,
//! distributions declared only for development or in extras, loaded while the package loads
//! ([`dependencies`]). Which module an import loads is decided by `heki_effect::imports` alone.

mod circular;
mod cycles;
mod dependencies;
mod development;

pub use circular::circular_failure_files;
pub(crate) use circular::imp102;
pub(crate) use cycles::imp101;
pub use cycles::import_cycle_files;
pub use dependencies::dependency_report_files;
pub(crate) use dependencies::imp301_imp302;
pub(crate) use development::imp104;

use crate::mut_rules::{contract_places, mode, reports_unresolved};
use crate::patterns::PartIndex;
use crate::{CheckContext, GLOB_CHARACTERS, NamePart, RuleConfigError, RuleSelection};
use heki_diagnostics::{ContractEntry, Diagnostic, RelatedLocation};
use heki_effect::imports::{ImportForm, ImportGraph, ImportSite, ModuleRef, Named, Target, Timing};
use heki_env::{HekiConfig, ImportBoundaryRule};
use heki_ir::walk::{Answer, Budget, Visit, reach_breadth_first};
use rustc_hash::{FxHashMap, FxHashSet};

const SECTION: &str = "import-boundaries";

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
    /// The name a `from` import of this module takes (`n` of `from name import n`), when the
    /// import takes a name of the module and not a submodule.
    taken: Option<&'n str>,
}

impl<'n> Imported<'n> {
    fn of(graph: &'n ImportGraph<'_>, m: ModuleRef) -> Self {
        Self {
            name: graph.name(m),
            project: matches!(m, ModuleRef::Project(_)),
            taken: None,
        }
    }

    /// The module as the import `site` takes it: with the name of a `from` import of it.
    fn at(self, site: &'n ImportSite) -> Self {
        Self {
            taken: taken_name(site, self.name),
            ..self
        }
    }
}

/// The name that `from module import name` takes from `module`, when `site` is that import. Its
/// target is then `module.name`: a name of the module, or a submodule that heki cannot tell from
/// one.
fn taken_name<'s>(site: &'s ImportSite, module: &str) -> Option<&'s str> {
    match (&site.form, &site.modules) {
        (ImportForm::From(name), Answer::Known(named)) => {
            matches!(&named[..], [Named::Module(m)] if m == module).then_some(name.as_str())
        }
        _ => None,
    }
}

/// A module pattern: a whole module name or a glob (`*` spans dots), or a group, or a name of
/// a module (`module:name`, a glob in either part), which only `from module import name` imports.
#[derive(Debug, Clone)]
enum ImportPattern {
    Module(NamePart),
    Group(Group),
    Name { module: NamePart, name: NamePart },
}

impl ImportPattern {
    /// `names`: the list may name a name of a module (every list but `relative-imports`).
    fn new(text: &str, context: &'static str, names: bool) -> Result<Self, RuleConfigError> {
        if let Some((module, name)) = text.split_once(':') {
            if !names || module.is_empty() || name.is_empty() || name.contains(':') {
                return Err(RuleConfigError::ImportName {
                    pattern: text.to_owned(),
                    context,
                });
            }
            return Ok(Self::Name {
                module: NamePart::new(module, text, context)?,
                name: NamePart::new(name, text, context)?,
            });
        }
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
            Self::Name { .. } => self.names(module),
        }
    }

    /// Whether the pattern is a name of a module that the import of `module` takes.
    fn names(&self, module: Imported<'_>) -> bool {
        match self {
            Self::Name { module: part, name } => {
                module.taken.is_some_and(|taken| name.is_match(taken)) && part.is_match(module.name)
            }
            Self::Module(_) | Self::Group(_) => false,
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
            // A by-name import takes no name of a module.
            Self::Name { .. } => false,
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
            Self::Group(_) | Self::Name { .. } => false,
        }
    }
}

/// The text of a pattern before its first character with a glob meaning.
fn literal_prefix(text: &str) -> &str {
    text.find(GLOB_CHARACTERS).map_or(text, |at| &text[..at])
}

/// Whether a pattern is one of the allowed, the forbidden, or the relative imports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Allowed,
    Forbidden,
    Relative,
}

/// The allowed, the forbidden, or the relative imports of a contract.
#[derive(Debug, Default)]
struct Imports {
    patterns: Vec<ImportPattern>,
}

impl Imports {
    fn is_set(&self) -> bool {
        !self.patterns.is_empty()
    }

    /// Whether some pattern is a name of a module.
    fn names(&self) -> bool {
        (self.patterns.iter()).any(|p| matches!(p, ImportPattern::Name { .. }))
    }

    /// Whether every pattern is a name of a module, and there is one.
    fn only_names(&self) -> bool {
        self.is_set() && (self.patterns.iter()).all(|p| matches!(p, ImportPattern::Name { .. }))
    }

    fn groups(&self) -> impl Iterator<Item = Group> + '_ {
        self.patterns.iter().filter_map(|p| match p {
            ImportPattern::Group(group) => Some(*group),
            ImportPattern::Module(_) | ImportPattern::Name { .. } => None,
        })
    }

    /// The place of the first pattern matching the import of `module`, of which `indexed` is
    /// the first module pattern the index of module patterns finds: a group or a name of a
    /// module before it, else `indexed`.
    fn first_matching(&self, module: Imported<'_>, indexed: Option<usize>) -> Option<usize> {
        let group = Group::of(module);
        let before = &self.patterns[..indexed.unwrap_or(self.patterns.len())];
        let other = before.iter().position(|p| match p {
            ImportPattern::Group(g) => *g == group,
            ImportPattern::Name { .. } => p.names(module),
            ImportPattern::Module(_) => false,
        });
        other.or(indexed)
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

/// Why an import breaks a contract, with the entry of the contract that says so: the first
/// `forbidden-imports` pattern the module matches (`Forbidden`), or the `relative-imports` one
/// (`Absolute`), by its list and place there.
#[derive(Debug, Clone, Copy)]
struct Breach {
    why: Why,
    entry: Option<(&'static str, usize)>,
}

impl From<Why> for Breach {
    fn from(why: Why) -> Self {
        Self { why, entry: None }
    }
}

/// A module pattern of `allowed-imports`, `forbidden-imports`, or `relative-imports` that a
/// name matches: its contract, its list, and its place there.
#[derive(Debug, Clone, Copy)]
struct Matched {
    contract: usize,
    side: Side,
    position: usize,
}

/// The place of the first pattern of contract `c` on `side` among `matched`.
fn first_matched(matched: &[Matched], c: usize, side: Side) -> Option<usize> {
    (matched.iter())
        .filter(|m| m.contract == c && m.side == side)
        .map(|m| m.position)
        .min()
}

/// Why an import breaks a contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Why {
    Forbidden,
    NotAllowed,
    /// The contract allows no import: it sets none of `allowed-imports`, `forbidden-imports`,
    /// and `relative-imports`.
    AllowsNone,
    /// The importer names by its full name a module the contract allows only through a relative
    /// import (`relative-imports`).
    Absolute,
}

struct ImportContract<'c> {
    /// How a report names the contract ([`heki_env::contract_label`]).
    label: String,
    note: Option<&'c str>,
    allowed: Imports,
    forbidden: Imports,
    /// The modules allowed only through a relative import.
    relative: Imports,
    type_checking: bool,
    reach: Reach,
    report_unresolved: bool,
}

impl ImportContract<'_> {
    /// Whether the contract lists the modules it allows: every other import breaks it, unless
    /// `forbidden-imports` is set.
    fn lists_allowed(&self) -> bool {
        self.allowed.is_set() || self.relative.is_set()
    }

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
        } else if self.lists_allowed() {
            Why::NotAllowed
        } else {
            Why::AllowsNone
        }
    }

    /// The text of a report naming the `broken` modules, each with why it breaks the contract:
    /// the modules of one reason together, `after` following the first of them.
    fn breaks_text(&self, broken: &[(String, Breach)], after: &str) -> String {
        let mut reasons: Vec<(Vec<String>, Why)> = Vec::new();
        for (name, Breach { why, .. }) in broken {
            match reasons.iter_mut().find(|(_, w)| w == why) {
                Some((names, _)) => names.push(name.clone()),
                None => reasons.push((vec![name.clone()], *why)),
            }
        }
        let texts: Vec<String> = (reasons.iter().enumerate())
            .map(|(i, (names, why))| {
                let after = if i == 0 { after } else { "" };
                format!(
                    "{}{after}, {}",
                    quoted(names, "or"),
                    self.why_text(*why, false)
                )
            })
            .collect();
        texts.join(", or ")
    }

    /// The text of a report saying why the import may break it, for one of `whys`.
    fn may_break_text(&self, whys: &[Why]) -> String {
        let texts: Vec<String> = whys.iter().map(|why| self.why_text(*why, true)).collect();
        texts.join(", or ")
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
            (Why::Absolute, false) => {
                format!("which {label} allows only through a relative import")
            }
            (Why::Absolute, true) => {
                format!("which may be a module {label} allows only through a relative import")
            }
        }
    }
}

/// An `ignored-imports` pair: an importer pattern (indexed) and the imported one.
struct IgnoredPair {
    contract: usize,
    imported: ImportPattern,
}

/// The `import-boundaries` contracts, their patterns indexed together.
struct ImportContracts<'c> {
    contracts: Vec<ImportContract<'c>>,
    /// The `importers` patterns, with their contracts; the contracts without the key judge
    /// every module.
    importers: (PartIndex, Vec<usize>),
    every_module: Vec<usize>,
    exempt: (PartIndex, Vec<usize>),
    /// The module patterns of `allowed-imports`, `forbidden-imports`, and `relative-imports`.
    imports: (PartIndex, Vec<Matched>),
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
    fn new(rules: &'c [ImportBoundaryRule]) -> Result<Self, RuleConfigError> {
        let mut contracts = Vec::new();
        let (mut importers, mut every_module, mut exempt) = (Vec::new(), Vec::new(), Vec::new());
        let (mut imports, mut ignored) = (Vec::new(), Vec::new());
        for (c, rule) in rules.iter().enumerate() {
            match &rule.importers {
                None => every_module.push(c),
                Some(patterns) => {
                    for text in patterns {
                        importers.push((module_part(text, "import-boundaries.importers")?, c));
                    }
                }
            }
            for text in &rule.exempt_importers {
                exempt.push((module_part(text, "import-boundaries.exempt-importers")?, c));
            }
            let mut sides = [Imports::default(), Imports::default(), Imports::default()];
            for (side, texts, context) in [
                (
                    Side::Allowed,
                    &rule.allowed_imports,
                    "import-boundaries.allowed-imports",
                ),
                (
                    Side::Forbidden,
                    &rule.forbidden_imports,
                    "import-boundaries.forbidden-imports",
                ),
                (
                    Side::Relative,
                    &rule.relative_imports,
                    "import-boundaries.relative-imports",
                ),
            ] {
                for text in texts {
                    if side == Side::Relative && text.starts_with('<') {
                        return Err(RuleConfigError::RelativeGroup {
                            pattern: text.clone(),
                            context,
                        });
                    }
                    let pattern = ImportPattern::new(text, context, side != Side::Relative)?;
                    if let ImportPattern::Module(part) = &pattern {
                        let position = sides[side as usize].patterns.len();
                        let matched = Matched {
                            contract: c,
                            side,
                            position,
                        };
                        imports.push((part.clone(), matched));
                    }
                    sides[side as usize].patterns.push(pattern);
                }
            }
            for text in &rule.ignored_imports {
                let context = "import-boundaries.ignored-imports";
                let (importer, imported) = text
                    .split_once("->")
                    .map(|(a, b)| (a.trim(), b.trim()))
                    .filter(|(a, b)| !a.is_empty() && !b.is_empty())
                    .ok_or_else(|| RuleConfigError::ImportPair {
                        pattern: text.clone(),
                        context,
                    })?;
                let imported = ImportPattern::new(imported, context, true)?;
                ignored.push((
                    module_part(importer, context)?,
                    IgnoredPair {
                        contract: c,
                        imported,
                    },
                ));
            }
            let [allowed, forbidden, relative] = sides;
            contracts.push(ImportContract {
                label: heki_env::contract_label(SECTION, c, &rule.name),
                note: rule.message.as_deref(),
                allowed,
                forbidden,
                relative,
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
            importers: indexed(importers, "import-boundaries.importers")?,
            every_module,
            exempt: indexed(exempt, "import-boundaries.exempt-importers")?,
            imports: indexed(imports, "import-boundaries.allowed-imports")?,
            ignored: indexed(ignored, "import-boundaries.ignored-imports")?,
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

    /// The contracts' module patterns that `name` matches.
    fn matched(&self, name: &str) -> Vec<Matched> {
        let (index, owners) = &self.imports;
        let mut ids = Vec::new();
        index.find(name, &mut ids);
        ids.into_iter().map(|i| owners[i]).collect()
    }

    /// Whether contract `c` allows `module` by what it lists as allowed (`allowed-imports`,
    /// `relative-imports`), by the module patterns it `matched`.
    fn allows(&self, c: usize, module: Imported<'_>, matched: &[Matched]) -> bool {
        let contract = &self.contracts[c];
        let allowed = &contract.allowed;
        (allowed.is_set()
            && (allowed.groups().any(|g| g == Group::of(module))
                || first_matched(matched, c, Side::Allowed).is_some()
                || allowed.patterns.iter().any(|p| p.names(module))))
            || first_matched(matched, c, Side::Relative).is_some()
    }

    /// Why importing `module` breaks contract `c`, by the module patterns it `matched`; `None`
    /// when the contract allows it. A module of `relative-imports` is allowed here: how an import
    /// names it is judged apart ([`Self::judge`]).
    fn violates(&self, c: usize, module: Imported<'_>, matched: &[Matched]) -> Option<Breach> {
        let contract = &self.contracts[c];
        let forbidden = &contract.forbidden;
        match (contract.lists_allowed(), forbidden.is_set()) {
            (false, false) => Some(Why::AllowsNone.into()),
            (_, true) => {
                let indexed = first_matched(matched, c, Side::Forbidden);
                let position = forbidden.first_matching(module, indexed)?;
                (!self.allows(c, module, matched)).then_some(Breach {
                    why: Why::Forbidden,
                    entry: Some(("forbidden-imports", position)),
                })
            }
            (true, false) => (!self.allows(c, module, matched)).then_some(Why::NotAllowed.into()),
        }
    }

    /// Why the name that the import of `module` takes breaks contract `c` by a forbidden name
    /// alone (`forbidden-imports = ["pkg.db:_*"]`), unless the contract allows it.
    fn forbids_taken(&self, c: usize, module: Imported<'_>, matched: &[Matched]) -> Option<Breach> {
        let forbidden = &self.contracts[c].forbidden;
        let position = forbidden.patterns.iter().position(|p| p.names(module))?;
        (!self.allows(c, module, matched)).then_some(Breach {
            why: Why::Forbidden,
            entry: Some(("forbidden-imports", position)),
        })
    }

    /// Whether contract `c` accepts the import of `module` by `importer` (`ignored-imports`).
    fn ignores(&self, c: usize, importer: &str, module: Imported<'_>) -> bool {
        self.ignores_by(c, importer, |p| p.matches(module))
    }

    /// Whether an `ignored-imports` pair of contract `c` for `importer` has an imported side
    /// that `accepts`.
    fn ignores_by(
        &self,
        c: usize,
        importer: &str,
        accepts: impl Fn(&ImportPattern) -> bool,
    ) -> bool {
        let (index, pairs) = &self.ignored;
        let mut ids = Vec::new();
        index.find(importer, &mut ids);
        (ids.into_iter().map(|i| &pairs[i])).any(|p| p.contract == c && accepts(&p.imported))
    }

    /// Whether contract `c` names the name that the import of `module` takes, as an allowed
    /// import or as an accepted one of `importer`: the import is the contract's own, whatever
    /// it takes the module to be (a submodule or a name of it).
    fn names_taken(&self, c: usize, importer: &str, module: Imported<'_>) -> bool {
        (self.contracts[c].allowed.patterns.iter()).any(|p| p.names(module))
            || self.ignores_by(c, importer, |p| p.names(module))
    }

    /// Why an import of `module` written in `importer` breaks contract `c`, unless it accepts
    /// it. `absolute`: the judged importer names the module by its full name, which a module of
    /// `relative-imports` must not be. `loading`: whether the contract allows the module is
    /// judged here (a walk judges it where it reaches the module, and here only the forbidden
    /// names the site takes of it).
    fn judge(
        &self,
        c: usize,
        importer: &str,
        module: Imported<'_>,
        absolute: bool,
        loading: bool,
    ) -> Option<Breach> {
        if self.ignores(c, importer, module) {
            return None;
        }
        let matched = self.matched(module.name);
        if let Some(position) = first_matched(&matched, c, Side::Relative) {
            return absolute.then_some(Breach {
                why: Why::Absolute,
                entry: Some(("relative-imports", position)),
            });
        }
        if loading {
            self.violates(c, module, &matched)
        } else {
            self.forbids_taken(c, module, &matched)
        }
    }

    /// Every reason for which importing some module whose name starts with `head` may break
    /// contract `c`, the importer naming it by its full name when `absolute` ([`Self::judge`]):
    /// a name of `relative-imports` written in full, and a module the contract does not allow.
    /// Empty when no such import can.
    fn may_break_under(&self, c: usize, head: &str, absolute: bool) -> Vec<Why> {
        let contract = &self.contracts[c];
        let (allowed, forbidden, relative) =
            (&contract.allowed, &contract.forbidden, &contract.relative);
        let spelled = absolute && (relative.patterns.iter()).any(|p| p.may_match_under(head));
        let loaded = if !contract.lists_allowed() && !forbidden.is_set() {
            Some(Why::AllowsNone)
        } else if (allowed.patterns.iter().chain(&relative.patterns))
            .any(|p| p.matches_all_under(head))
        {
            None
        } else if !forbidden.is_set() {
            Some(Why::NotAllowed)
        } else {
            (forbidden.patterns.iter())
                .any(|p| p.may_match_under(head))
                .then_some(Why::Forbidden)
        };
        spelled
            .then_some(Why::Absolute)
            .into_iter()
            .chain(loaded)
            .collect()
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
    /// A relative import that climbs past the top-level package.
    BeyondTop,
    /// A call that may be `importlib.import_module` or `__import__` ([`ImportForm::MaybeByName`]).
    MaybeByName,
}

impl OpenImport {
    /// The report's target: what a baseline entry counts it by.
    fn target(&self) -> String {
        match self {
            Self::Unknown => "*".to_owned(),
            Self::Under(head) => format!("{head}*"),
            Self::AnyOf(names) => names.join("|"),
            Self::Submodule { submodule, .. } => submodule.clone(),
            Self::BeyondTop | Self::MaybeByName => "*".to_owned(),
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
            Self::BeyondTop => "a module above the top-level package".to_owned(),
            Self::MaybeByName => {
                "a module through a call that may be `importlib.import_module` or `__import__`"
                    .to_owned()
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
    /// breaking the contract for its reason.
    Breaks(Vec<(String, Breach)>),
    /// The site may import a module that breaks the contract, for one of these reasons.
    Open(OpenImport, Vec<Why>),
}

/// One answer of a part of a by-name import: whether the module it names breaks the contract.
enum Part {
    Breaks(String, Breach),
    Fine,
    /// It may break the contract, for one of these reasons.
    Open(Vec<Why>),
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

/// `IMP201` and `IMP202`: the imports of the modules each `import-boundaries` contract judges that
/// it does not allow, and, with `unresolved = "report"`, the imports whose module heki cannot
/// name that it may not allow.
pub(crate) fn imp201_imp202(
    cx: &CheckContext<'_>,
    out: &mut Vec<Diagnostic>,
) -> Result<(), RuleConfigError> {
    if !judges_imports(cx.config, cx.selection) {
        return Ok(());
    }
    let contracts = ImportContracts::new(&cx.config.import_boundaries)?;
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
        let budget = graph.budget("import_boundaries");
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
            let verdict = self.verdict(c, m, site, target, true, false);
            self.emit(c, m, (site, target), &[], verdict);
        }
    }

    /// What contract `c` says about the import `site` of module `m` makes of `target`. `own`: `m`
    /// is the judged importer, so how the site names each module is judged too. `walked`: the
    /// walk of a `reach` contract judges whether the contract allows the module a site certainly
    /// loads where it reaches it, so only the rest of the target is judged for that here.
    fn verdict(
        &self,
        c: usize,
        m: u32,
        site: &ImportSite,
        target: &Target,
        own: bool,
        walked: bool,
    ) -> Verdict {
        let (contracts, graph) = (self.contracts, self.graph);
        let contract = &contracts.contracts[c];
        let importer = graph.name(ModuleRef::Project(m));
        // Whether the judged importer names module `i` of the site by its full name.
        let absolute = |i: usize| own && !site.spelling.relative(i);
        let judge = |module: Imported<'_>, i: usize, loading: bool| {
            contracts.judge(c, importer, module, absolute(i), loading)
        };
        let one = |module: ModuleRef, i: usize, loading: bool| {
            judge(Imported::of(graph, module).at(site), i, loading)
        };
        match target {
            Target::Module(module) => match one(*module, 0, !walked) {
                Some(why) => Verdict::Breaks(vec![(graph.name(*module).to_owned(), why)]),
                None => Verdict::Fine,
            },
            Target::PackageOrSubmodule { package, submodule } => {
                // The name the import takes may be the submodule, which a forbidden name does not
                // name: that break alone is open, not exact.
                let mut whys = Vec::new();
                if let Some(breach) = one(*package, 0, !walked) {
                    if judge(Imported::of(graph, *package), 0, !walked).is_some() {
                        return Verdict::Breaks(vec![(graph.name(*package).to_owned(), breach)]);
                    }
                    whys.push(breach.why);
                }
                let imported = Imported {
                    name: submodule,
                    project: matches!(package, ModuleRef::Project(_)),
                    taken: None,
                };
                // A name the contract names is allowed whether it is a submodule or not.
                if contracts.names_taken(c, importer, Imported::of(graph, *package).at(site)) {
                    return Verdict::Fine;
                }
                if let Some(Breach { why, .. }) = judge(imported, 0, true)
                    && !whys.contains(&why)
                {
                    whys.push(why);
                }
                if whys.is_empty() {
                    return Verdict::Fine;
                }
                Verdict::Open(
                    OpenImport::Submodule {
                        package: graph.name(*package).to_owned(),
                        submodule: submodule.clone(),
                    },
                    whys,
                )
            }
            Target::AnyOf(targets) => {
                let parts: Vec<Part> = (targets.iter().enumerate())
                    .map(|(i, t)| match t {
                        Target::Module(module) => match one(*module, i, true) {
                            Some(why) => Part::Breaks(graph.name(*module).to_owned(), why),
                            None => Part::Fine,
                        },
                        Target::Missing(_) => Part::Fine,
                        Target::Under(head) => {
                            let whys = contracts.may_break_under(c, head, absolute(i));
                            if whys.is_empty() {
                                Part::Fine
                            } else {
                                Part::Open(whys)
                            }
                        }
                        _ => Part::Open(vec![contract.open_why()]),
                    })
                    .collect();
                let broken: Vec<(String, Breach)> = (parts.iter())
                    .filter_map(|p| match p {
                        Part::Breaks(name, breach) => Some((name.clone(), *breach)),
                        _ => None,
                    })
                    .collect();
                if !broken.is_empty() && broken.len() == parts.len() {
                    return Verdict::Breaks(broken);
                }
                // Each reason once, in the order of the parts giving it.
                let mut whys: Vec<Why> = Vec::new();
                for part in &parts {
                    let given = match part {
                        Part::Breaks(_, breach) => std::slice::from_ref(&breach.why),
                        Part::Open(whys) => whys.as_slice(),
                        Part::Fine => &[],
                    };
                    for why in given {
                        if !whys.contains(why) {
                            whys.push(*why);
                        }
                    }
                }
                if whys.is_empty() {
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
                Verdict::Open(OpenImport::AnyOf(names), whys)
            }
            Target::Under(head) => {
                let whys = contracts.may_break_under(c, head, absolute(0));
                if whys.is_empty() {
                    return Verdict::Fine;
                }
                Verdict::Open(OpenImport::Under(head.clone()), whys)
            }
            // A by-name import takes no name, so a forbidden name never breaks; the rest of the
            // contract (a module of `relative-imports` written in full) may.
            Target::Open if site.form.is_by_name() && contract.forbidden.only_names() => {
                let whys = contracts.may_break_under(c, "", absolute(0));
                if whys.is_empty() {
                    return Verdict::Fine;
                }
                Verdict::Open(OpenImport::Unknown, whys)
            }
            Target::Open if site.form == ImportForm::MaybeByName => {
                Verdict::Open(OpenImport::MaybeByName, vec![contract.open_why()])
            }
            Target::Open => Verdict::Open(OpenImport::Unknown, vec![contract.open_why()]),
            // A relative import climbing past its top-level package names no module and raises.
            Target::Missing(name) if name.is_empty() => {
                Verdict::Open(OpenImport::BeyondTop, vec![contract.open_why()])
            }
            Target::Missing(_) => Verdict::Fine,
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
                    let imported = Imported::of(graph, t).at(site);
                    if contracts.ignores(c, from_name, imported)
                        || contracts.names_taken(c, from_name, imported)
                    {
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
            // Only whether the contract allows the module: how the importer's own imports name
            // it is judged at its sites below, and the other steps are not the importer's.
            let Some(breach) = contracts.violates(c, imported, &contracts.matched(imported.name))
            else {
                continue;
            };
            let chain = chain_to(module);
            let site = first_site(&chain).expect("a module reached has a chain to it");
            let verdict = Verdict::Breaks(vec![(imported.name.to_owned(), breach)]);
            self.emit(c, m, site, &chain[1..], verdict);
        }
        // The sites whose module a walk cannot take: judged where the chain to them ends. The
        // importer's own sites are judged also for how they name the modules they import.
        for &module in &reached {
            let ModuleRef::Project(r) = module else {
                continue;
            };
            let own = r == m;
            for (_, site, target) in self.counted(c, r) {
                // A module a step imports is judged where the walk reaches it, unless the
                // contract forbids names: only the site says which name it takes.
                if matches!(target, Target::Module(_)) && !own && !contract.forbidden.names() {
                    continue;
                }
                let mut verdict = self.verdict(c, r, site, target, own, true);
                // A step names the name it takes of a module, as the importer's own sites do.
                if let Verdict::Breaks(broken) = &mut verdict
                    && !own
                    && let [(module, _)] = &mut broken[..]
                    && let Some(taken) = taken_name(site, module)
                {
                    *module = format!("{module}.{taken}");
                }
                let what = match &verdict {
                    Verdict::Breaks(broken) => {
                        let names: Vec<String> = broken.iter().map(|(n, _)| n.clone()).collect();
                        quoted(&names, "or")
                    }
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
        let mut module_key = None;
        // The entry of the first module the report names that an entry breaks the contract by.
        let mut entry = None;
        let (code, target, message) = match verdict {
            Verdict::Fine => return,
            Verdict::Breaks(mut broken) => {
                entry = broken.iter().find_map(|(_, breach)| breach.entry);
                // `from m import n` imports the name `n` of `m`: the report names it.
                if let ([(module, _)], None) = (&mut broken[..], first)
                    && let Some(taken) = taken_name(site, module)
                {
                    module_key = Some(module.clone());
                    *module = format!("{module}.{taken}");
                }
                let names: Vec<String> = broken.iter().map(|(n, _)| n.clone()).collect();
                let message = match first {
                    Some(first) if first != names[0] => format!(
                        "`{}` imports `{first}`{when}, and {}",
                        site.qualname,
                        contract.breaks_text(&broken, " through it")
                    ),
                    _ => format!(
                        "`{}` imports {}",
                        site.qualname,
                        contract.breaks_text(&broken, &when)
                    ),
                };
                ("IMP201", names.join("|"), message)
            }
            Verdict::Open(import, whys) => {
                if !contract.report_unresolved {
                    return;
                }
                let why = contract.may_break_text(&whys);
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
        let entries: Vec<ContractEntry> = (entry.into_iter())
            .map(|(list, position)| ContractEntry::new(c, list, position))
            .collect();
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
            d.contract_entries = entries;
            d.key_module = module_key;
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

/// Whether `IMP201` or `IMP202` judges the `import-boundaries` contracts of `config`: one is
/// declared, and `selection` runs one of the rules.
pub fn judges_imports(config: &HekiConfig, selection: &RuleSelection) -> bool {
    !config.import_boundaries.is_empty()
        && ["IMP201", "IMP202"]
            .iter()
            .any(|code| selection.is_enabled(code))
}

/// Whether the rules `selection` runs judge an `import-boundaries` contract of `config` walking what
/// its importers import (`reach` other than `"statement"`): its reports change with the imports
/// of every module its walks reach. A contract no selected rule judges is not read, so its
/// patterns are not validated either.
pub fn walks_imports(config: &HekiConfig, selection: &RuleSelection) -> bool {
    judges_imports(config, selection)
        && (config.import_boundaries.iter())
            .any(|r| r.reach.as_deref().is_some_and(|r| r != "statement"))
}

/// The modules among `modules`, each with what it stands for (its file), that an
/// `import-boundaries` contract walking imports judges.
pub fn walking_importers<'m, T>(
    config: &HekiConfig,
    modules: impl IntoIterator<Item = (&'m str, T)>,
) -> Result<Vec<T>, RuleConfigError> {
    let contracts = ImportContracts::new(&config.import_boundaries)?;
    let walking = |c: &usize| contracts.contracts[*c].reach != Reach::Statement;
    Ok((modules.into_iter())
        .filter(|(module, _)| contracts.judging(module).iter().any(walking))
        .map(|(_, of)| of)
        .collect())
}
