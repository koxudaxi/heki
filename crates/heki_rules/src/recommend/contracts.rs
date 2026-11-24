//! "Contract candidates": the `write-owners`, `read-owners`, and `effect-boundaries` entries the
//! code already keeps, drawn from the census of the discovery run ([`crate::census`]), never from
//! names, each kept only when a trial of its rules reports nothing on the code as it is.
//!
//! Generation may propose too much; the trial decides. It declares every candidate at once, the
//! ownership contracts with `unresolved = "report"`, and runs their rules as `heki check` would
//! (inline suppressions and `per-file-ignores` apply); each report counts for the contracts it is
//! of ([`Diagnostic::contracts`]). A candidate the rule of its contract reports is rejected, and
//! so is one that hides a report of the configuration's own contracts ([`judge`]); reports of
//! accesses on objects of unknown class make it medium instead. The verification run then
//! applies the whole block.

use super::block::set_keys;
use super::{LeftOut, MAX_EXAMPLES, MAX_LISTED, RunLabel};
use crate::census::{AttributeUse, Census, ModuleUse};
use crate::contracts::Declared;
use crate::names_module_literally;
use crate::selection::RuleSelection;
use heki_diagnostics::Diagnostic;
use heki_effect::{FuncKey, ProjectDecls, ProjectIndex};
use heki_env::{EffectBoundaryRule, HekiConfig, ReadOwnerRule, WriteOwnerRule};
use rustc_hash::{FxHashMap, FxHashSet};
use schemars::JsonSchema;
use serde::Serialize;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

/// The most candidates of one kind a trial declares, the first by rank. A configuration's
/// `write-owners` contracts past the 32nd share bits in
/// [`heki_effect::Contracts::writer_contexts`], which would blur the trial, so the configuration's
/// own `write-owners` contracts count against it.
pub const MAX_CANDIDATES: usize = 32;

/// The least number of modules other than the owner's reading an attribute, for a `write-owners`
/// candidate: the audience that makes declaring its owner worth it.
pub const MIN_AUDIENCE: usize = 2;

/// The least number of modules using a class, for a `read-owners` candidate of a private
/// attribute of it.
pub const MIN_CLASS_MODULES: usize = 2;

/// The least number of functions in the scopes of an `effect-boundaries` candidate.
pub const MIN_SCOPE_FUNCTIONS: usize = 3;

/// What every candidate leaves unseen, whatever its confidence.
const UNSEEN_CAVEAT: &str = "accesses inside code heki cannot follow (unresolved calls, libraries \
                             without a catalog) are not seen";

/// The kinds of contract `heki recommend` proposes, by their key in the configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ContractKind {
    WriteOwners,
    ReadOwners,
    EffectBoundaries,
}

impl ContractKind {
    const ALL: [Self; 3] = [Self::WriteOwners, Self::ReadOwners, Self::EffectBoundaries];

    /// The kind's key in the configuration.
    pub fn key(self) -> &'static str {
        match self {
            Self::WriteOwners => "write-owners",
            Self::ReadOwners => "read-owners",
            Self::EffectBoundaries => "effect-boundaries",
        }
    }

    /// The rule reporting what the contract forbids.
    pub fn rule(self) -> &'static str {
        match self {
            Self::WriteOwners => "MUT201",
            Self::ReadOwners => "MUT203",
            Self::EffectBoundaries => "EFF201",
        }
    }

    /// The rule reporting the accesses the contract cannot tie to a class, with
    /// `unresolved = "report"`.
    pub fn strict_rule(self) -> Option<&'static str> {
        match self {
            Self::WriteOwners => Some("MUT205"),
            Self::ReadOwners => Some("MUT206"),
            Self::EffectBoundaries => None,
        }
    }

    /// The kind whose contracts `code` reports for.
    fn of_rule(code: &str) -> Option<Self> {
        let table = crate::contracts::contract_table_of_rule(code)?;
        Self::ALL.into_iter().find(|k| k.key() == table)
    }

    /// How many contracts of the kind `cfg` declares: the candidates of a trial come after them.
    fn declared_in(self, cfg: &HekiConfig) -> usize {
        match self {
            Self::WriteOwners => cfg.write_owners.len(),
            Self::ReadOwners => cfg.read_owners.len(),
            Self::EffectBoundaries => cfg.effect_boundaries.len(),
        }
    }

    /// The names the contracts of the kind in `cfg` have.
    fn names_in(self, cfg: &HekiConfig) -> Vec<&str> {
        match self {
            Self::WriteOwners => cfg.write_owners.iter().map(|c| c.name.as_str()).collect(),
            Self::ReadOwners => cfg.read_owners.iter().map(|c| c.name.as_str()).collect(),
            Self::EffectBoundaries => (cfg.effect_boundaries.iter())
                .map(|c| c.name.as_str())
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Confidence {
    /// The trial found nothing the contract cannot see: no access it cannot tie to a class (an
    /// effect boundary: no scope function reaching code heki does not see), and every Python file
    /// below the project root was analyzed.
    High,
    /// The trial reports no violation, but heki may not see all the contract is about: accesses
    /// on objects of unknown class, Python files below the project root left out, or scope
    /// functions reaching code heki does not see. The caveats say which. An ownership entry then
    /// sets `unresolved = "ignore"`.
    Medium,
}

/// A list of names, the first [`MAX_LISTED`] of them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Listed {
    pub count: usize,
    /// Sorted.
    pub items: Vec<String>,
    /// Whether `items` holds fewer than `count`.
    pub truncated: bool,
}

impl Listed {
    fn of(items: impl IntoIterator<Item = String>) -> Self {
        let all: BTreeSet<String> = items.into_iter().collect();
        Self {
            count: all.len(),
            truncated: all.len() > MAX_LISTED,
            items: all.into_iter().take(MAX_LISTED).collect(),
        }
    }
}

/// What a candidate rests on, from the discovery run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Evidence {
    /// The functions the entry allows (`write-owners`: those writing the attribute after its
    /// object is constructed; `read-owners`: those reading it) or holds (`effect-boundaries`:
    /// the functions its scopes' modules define, without the comprehensions and lambdas inside
    /// them), fully qualified.
    pub functions: Listed,
    /// The modules of those functions; for an attribute written only while its objects are
    /// constructed, the module of its class.
    pub modules: Listed,
    /// The other modules reading the attribute (`write-owners`), using its class
    /// (`read-owners`), or calling into the scopes (`effect-boundaries`).
    pub audience: Listed,
    /// `write-owners`: a writer changes the attribute's object in place (a container).
    pub in_place: bool,
    /// `effect-boundaries`: the functions of the scopes reaching code heki does not see (a call
    /// it does not resolve, a library name without a catalog entry, an A1 hazard, a body known
    /// only from a stub).
    pub unseen: Listed,
}

/// One report of a trial.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct TrialReport {
    pub rule: String,
    /// As `heki check` writes it.
    pub path: String,
    pub line: u32,
    pub message: String,
}

/// What a run with the contract declared reported for it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Trial {
    /// The reports after inline suppressions, by rule: the contract's rule, and for an ownership
    /// contract the rule of the accesses it cannot tie to a class.
    pub reports: BTreeMap<String, usize>,
    /// The reports inline suppressions drop.
    pub suppressed: usize,
    /// The reports after inline suppressions, the first 50 in the order `heki check` prints
    /// them.
    pub examples: Vec<TrialReport>,
}

impl Trial {
    fn of(kind: ContractKind) -> Self {
        let rules = std::iter::once(kind.rule()).chain(kind.strict_rule());
        Self {
            reports: rules.map(|r| (r.to_owned(), 0)).collect(),
            ..Self::default()
        }
    }

    fn reports_of(&self, rule: Option<&str>) -> usize {
        rule.and_then(|r| self.reports.get(r)).copied().unwrap_or(0)
    }
}

/// A candidate the trial kept: the block declares it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ContractAdvice {
    pub kind: ContractKind,
    /// What the entry is about: the attribute (`module:Class.attr`), or the package or module its
    /// scopes cover.
    pub target: String,
    /// The entry the block declares, as the keys it sets.
    pub entry: serde_json::Map<String, serde_json::Value>,
    pub confidence: Confidence,
    pub evidence: Evidence,
    /// What the contract trial reported for it.
    pub trial: Trial,
    /// What the confidence does not cover.
    pub caveats: Vec<String>,
}

/// A candidate a run with it declared reported for.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RejectedContract {
    pub kind: ContractKind,
    /// What the entry is about, as for a candidate kept.
    pub target: String,
    /// The entry the block would declare.
    pub entry: serde_json::Map<String, serde_json::Value>,
    /// The run that reported it: the contract trial, or the verification run with the whole
    /// block.
    pub run: RunLabel,
    pub trial: Trial,
}

/// The candidates of one kind past the first [`MAX_CANDIDATES`] (for `write-owners`, less the
/// configuration's own), which no trial declared.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct NotTried {
    pub kind: ContractKind,
    /// The candidates the trial declared.
    pub tried: usize,
    /// The candidates past them.
    pub candidates: usize,
}

/// An entry of one kind.
#[derive(Debug, Clone, PartialEq)]
enum Entry {
    Write(WriteOwnerRule),
    Read(ReadOwnerRule),
    Boundary(EffectBoundaryRule),
}

impl Entry {
    fn kind(&self) -> ContractKind {
        match self {
            Self::Write(_) => ContractKind::WriteOwners,
            Self::Read(_) => ContractKind::ReadOwners,
            Self::Boundary(_) => ContractKind::EffectBoundaries,
        }
    }

    fn set_name(&mut self, name: String) {
        match self {
            Self::Write(e) => e.name = name,
            Self::Read(e) => e.name = name,
            Self::Boundary(e) => e.name = name,
        }
    }

    /// `unresolved = "report"` or `"ignore"` (an ownership contract).
    fn set_unresolved(&mut self, report: bool) {
        let mode = Some(if report { "report" } else { "ignore" }.to_owned());
        match self {
            Self::Write(e) => e.unresolved = mode,
            Self::Read(e) => e.unresolved = mode,
            Self::Boundary(_) => {}
        }
    }

    fn reports_unresolved(&self) -> bool {
        let report = |mode: &Option<String>| mode.as_deref() == Some("report");
        match self {
            Self::Write(e) => report(&e.unresolved),
            Self::Read(e) => report(&e.unresolved),
            Self::Boundary(_) => false,
        }
    }

    /// Declared after the contracts `cfg` has of its kind.
    fn declare_in(&self, cfg: &mut HekiConfig) {
        match self {
            Self::Write(e) => cfg.write_owners.push(e.clone()),
            Self::Read(e) => cfg.read_owners.push(e.clone()),
            Self::Boundary(e) => cfg.effect_boundaries.push(e.clone()),
        }
    }

    fn keys(&self) -> serde_json::Map<String, serde_json::Value> {
        match self {
            Self::Write(e) => set_keys(e),
            Self::Read(e) => set_keys(e),
            Self::Boundary(e) => set_keys(e),
        }
    }
}

/// One candidate, before and after its trial.
#[derive(Debug, Clone)]
struct Candidate {
    entry: Entry,
    /// What the text calls it: the target, or the scopes' package or module.
    shown: String,
    /// The name the entry gets, unless the configuration has a contract of that name.
    name: String,
    /// The name with the module part, which the project has once.
    full_name: String,
    evidence: Evidence,
    /// An ownership candidate's class and attribute, and the project classes the class derives
    /// from: a candidate of a base class covers its subclasses.
    attribute: Option<(String, String, Vec<String>)>,
    /// An ownership candidate's `Class.attr` names, as a report of a `__dict__` key no target
    /// names gives them: its class's and each project subclass's, which its target covers.
    members: Vec<String>,
    /// The order within the kind: the least first.
    rank: (bool, Reverse<usize>, Reverse<usize>),
}

impl Candidate {
    /// Whether an ownership candidate's target covers `member` (`Class.attr`, as a report of a
    /// `__dict__` key no target names gives it).
    fn names_member(&self, member: &str) -> bool {
        self.members.iter().any(|m| m == member)
    }
}

/// The candidates of a project, for its trial.
#[derive(Debug, Default)]
pub(super) struct Generated {
    /// Ranked within each kind, the kinds in [`ContractKind`] order.
    candidates: Vec<Candidate>,
    not_tried: Vec<NotTried>,
    /// The kinds whose rule the configuration ignores: no candidate of theirs is proposed.
    ignored: Vec<ContractKind>,
    /// The kinds whose rule of the accesses on objects of unknown class the configuration
    /// ignores: their entries set `unresolved = "ignore"`, as `heki check` keeps that rule off.
    strict_ignored: Vec<ContractKind>,
}

impl Generated {
    pub(super) fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }
}

/// The functions the modules `members` define, without the scopes Python makes inside them.
fn defined_functions<'i>(members: &[&ModuleUse<'i>]) -> Vec<&'i FuncKey> {
    let functions = members.iter().flat_map(|m| &m.functions).copied();
    functions.filter(|f| !is_synthetic(f)).collect()
}

/// Whether `function` is a scope Python makes inside a function (a comprehension, a generator
/// expression, a lambda): the evidence counts the functions the code defines.
fn is_synthetic(function: &FuncKey) -> bool {
    let name = function.qualname.rsplit('.').next().unwrap_or_default();
    name.starts_with('<')
}

/// Whether `attr` is a private attribute: `_name`, not a dunder.
fn is_private_attribute(attr: &str) -> bool {
    attr.starts_with('_') && !(attr.starts_with("__") && attr.ends_with("__"))
}

/// The smallest pattern with a module part naming every function of `module` in `qualnames`:
/// `module:Class.*` when they are all methods of one class `is_class` names (or functions
/// nested in them), else `module:*`. A pattern with a module part never matches a dotted suffix
/// of another project's names.
pub fn allowed_pattern(
    module: &str,
    qualnames: &[&str],
    is_class: impl Fn(&str) -> bool,
) -> String {
    // The dotted names the functions are defined in, without their own name.
    let enclosing: Vec<Vec<&str>> = qualnames
        .iter()
        .map(|q| {
            let mut parts: Vec<&str> = q.split('.').collect();
            parts.pop();
            parts
        })
        .collect();
    let mut common: Vec<&str> = enclosing.first().cloned().unwrap_or_default();
    for parts in enclosing.iter().skip(1) {
        let shared = common.iter().zip(parts).take_while(|(a, b)| a == b).count();
        common.truncate(shared);
    }
    // The innermost class enclosing them all.
    while !common.is_empty() {
        let class = common.join(".");
        if is_class(&class) {
            return format!("{module}:{class}.*");
        }
        common.pop();
    }
    format!("{module}:*")
}

/// The candidates of the project `index` analyzes, the census's, ranked and capped: what `cfg`
/// and the rules `current` ignores leave to propose. `tests`: the files holding test code
/// ([`heki_effect::code_kind::is_test_code`]: a test file, or one defining a test function), which no
/// contract is drawn from and no effect boundary covers: a boundary is about the program's
/// layers. A module whose name a pattern cannot spell ([`names_module_literally`]) gets no
/// candidate either.
pub(super) fn generate(
    index: &ProjectIndex,
    cfg: &HekiConfig,
    declared: &Declared,
    current: &RuleSelection,
    tests: &FxHashSet<&Path>,
) -> Generated {
    let ignored: Vec<ContractKind> = ContractKind::ALL
        .into_iter()
        .filter(|k| current.is_ignored(k.rule()))
        .collect();
    let proposes = |kind| !ignored.contains(&kind);
    let strict_ignored: Vec<ContractKind> = ContractKind::ALL
        .into_iter()
        .filter(|k| proposes(*k) && k.strict_rule().is_some_and(|r| current.is_ignored(r)))
        .collect();
    let decls = index.project_decls();
    let census = Census::of(index);
    let class_modules: FxHashMap<&str, &[&str]> = (census.classes.iter())
        .map(|c| (c.class, c.modules.as_slice()))
        .collect();
    let mut by_kind: BTreeMap<ContractKind, Vec<Candidate>> = BTreeMap::new();
    // A protocol's member is no attribute of one class: the classes implementing it hold it.
    // A contract on test scaffolding (a recorder, a fake) guards nothing of the program.
    let test_modules: FxHashSet<&str> = (census.modules.iter())
        .filter(|m| tests.contains(m.path))
        .map(|m| m.module)
        .collect();
    let attributes = (census.attributes.iter())
        .filter(|a| !decls.protocol_declares(a.class, a.attr))
        .filter_map(|a| Some((a, decls.class_qualname(a.class)?)));
    for (attribute, qualname) in attributes {
        let (class, attr) = (attribute.class, attribute.attr);
        let module = &class[..class.len() - qualname.len() - 1];
        if test_modules.contains(module) || !names_module_literally(module) {
            continue;
        }
        let site = (&decls, module, qualname);
        if proposes(ContractKind::WriteOwners) && !declared.owns_writes(class, attr) {
            by_kind
                .entry(ContractKind::WriteOwners)
                .or_default()
                .extend(write_owner(site, attribute));
        }
        if proposes(ContractKind::ReadOwners)
            && is_private_attribute(attr)
            && !declared.owns_reads(class, attr)
        {
            by_kind
                .entry(ContractKind::ReadOwners)
                .or_default()
                .extend(read_owner(site, attribute, &class_modules));
        }
    }
    if proposes(ContractKind::EffectBoundaries) {
        let modules: Vec<&ModuleUse<'_>> = (census.modules.iter())
            .filter(|m| !tests.contains(m.path))
            .collect();
        by_kind.insert(
            ContractKind::EffectBoundaries,
            effect_boundaries(index, &modules, &test_modules, declared),
        );
    }
    let mut out = Generated {
        ignored,
        strict_ignored,
        ..Generated::default()
    };
    for (kind, mut candidates) in by_kind {
        candidates.sort_by(|a, b| (a.rank, &a.shown).cmp(&(b.rank, &b.shown)));
        let cap = match kind {
            ContractKind::WriteOwners => MAX_CANDIDATES.saturating_sub(cfg.write_owners.len()),
            _ => MAX_CANDIDATES,
        };
        if candidates.len() > cap {
            out.not_tried.push(NotTried {
                kind,
                tried: cap,
                candidates: candidates.len() - cap,
            });
            candidates.truncate(cap);
        }
        let mut taken: FxHashSet<String> = (kind.names_in(cfg).into_iter())
            .map(str::to_owned)
            .collect();
        for mut candidate in candidates {
            let name = unique_name(&mut taken, &candidate.name, &candidate.full_name);
            candidate.entry.set_name(name);
            if let Some((class, attr, ancestors)) = &mut candidate.attribute {
                *ancestors = decls.ancestors_of(class);
                candidate.members = std::iter::once(class.clone())
                    .chain(decls.subclasses_of(class))
                    .filter_map(|c| Some(format!("{}.{attr}", decls.class_qualname(&c)?)))
                    .collect();
            }
            out.candidates.push(candidate);
        }
    }
    out
}

/// `name`, or `full_name` when the configuration or another candidate has a contract of that
/// name, numbered past that.
fn unique_name(taken: &mut FxHashSet<String>, name: &str, full_name: &str) -> String {
    let mut chosen = [name, full_name]
        .into_iter()
        .find(|n| !taken.contains(*n))
        .map(str::to_owned);
    let mut n = 2;
    while chosen.is_none() {
        let numbered = format!("{full_name} ({n})");
        if !taken.contains(&numbered) {
            chosen = Some(numbered);
        }
        n += 1;
    }
    let chosen = chosen.expect("a free name was found");
    taken.insert(chosen.clone());
    chosen
}

/// The project declarations, a class's module, and its qualified name within it.
type ClassSite<'s> = (&'s ProjectDecls, &'s str, &'s str);

/// A `write-owners` candidate for `attribute`: written after construction by functions of one
/// module only, or only while its objects are constructed (`allowed-writers = []`), and read in
/// at least [`MIN_AUDIENCE`] other modules.
fn write_owner(
    (decls, module, qualname): ClassSite<'_>,
    attribute: &AttributeUse<'_>,
) -> Option<Candidate> {
    // A factory filling the object it constructs writes it while it is constructed: the
    // contracts allow it (`fresh-objects = "allow"`).
    let writers: Vec<&FuncKey> = (attribute.writers.iter())
        .filter(|w| !w.fresh)
        .map(|w| w.accesses.function)
        .collect();
    if attribute.writers.is_empty() && attribute.initializers.is_empty() {
        return None;
    }
    let owner = writers.first().map_or(module, |w| w.module.as_str());
    if writers.iter().any(|w| w.module != owner) || !names_module_literally(owner) {
        return None;
    }
    let audience: BTreeSet<&str> = (attribute.readers.iter())
        .map(|r| r.function.module.as_str())
        .filter(|m| *m != owner)
        .collect();
    if audience.len() < MIN_AUDIENCE {
        return None;
    }
    let attr = attribute.attr;
    let target = format!("{module}:{qualname}.{attr}");
    let allowed_writers = if writers.is_empty() {
        Vec::new()
    } else {
        let qualnames: Vec<&str> = writers.iter().map(|w| w.qualname.as_str()).collect();
        let is_class = |q: &str| decls.class_qualname(&format!("{owner}.{q}")).is_some();
        vec![allowed_pattern(owner, &qualnames, is_class)]
    };
    let in_place = (attribute.writers.iter()).any(|w| !w.fresh && w.in_place);
    Some(Candidate {
        entry: Entry::Write(WriteOwnerRule {
            targets: vec![target.clone()],
            allowed_writers,
            ..WriteOwnerRule::default()
        }),
        name: format!("{qualname}.{attr} writers"),
        full_name: format!("{target} writers"),
        evidence: Evidence {
            functions: Listed::of(writers.iter().map(|w| w.fqn())),
            modules: Listed::of([owner.to_owned()]),
            audience: Listed::of(audience.iter().map(|m| (*m).to_owned())),
            in_place,
            unseen: Listed::default(),
        },
        attribute: Some((attribute.class.to_owned(), attr.to_owned(), Vec::new())),
        members: Vec::new(),
        rank: (!in_place, Reverse(audience.len()), Reverse(0)),
        shown: target,
    })
}

/// A `read-owners` candidate for the private `attribute`: read only by functions of its class's
/// module, the class used in at least [`MIN_CLASS_MODULES`] modules.
fn read_owner(
    (decls, module, qualname): ClassSite<'_>,
    attribute: &AttributeUse<'_>,
    class_modules: &FxHashMap<&str, &[&str]>,
) -> Option<Candidate> {
    let readers: Vec<&FuncKey> = attribute.readers.iter().map(|r| r.function).collect();
    if readers.is_empty() || readers.iter().any(|r| r.module != module) {
        return None;
    }
    let used_in = class_modules.get(attribute.class).copied().unwrap_or(&[]);
    let audience: Vec<&str> = used_in.iter().copied().filter(|m| *m != module).collect();
    if used_in.len() < MIN_CLASS_MODULES || audience.is_empty() {
        return None;
    }
    let attr = attribute.attr;
    let target = format!("{module}:{qualname}.{attr}");
    let qualnames: Vec<&str> = readers.iter().map(|r| r.qualname.as_str()).collect();
    let is_class = |q: &str| decls.class_qualname(&format!("{module}.{q}")).is_some();
    Some(Candidate {
        entry: Entry::Read(ReadOwnerRule {
            targets: vec![target.clone()],
            allowed_readers: vec![allowed_pattern(module, &qualnames, is_class)],
            ..ReadOwnerRule::default()
        }),
        name: format!("{qualname}.{attr} readers"),
        full_name: format!("{target} readers"),
        evidence: Evidence {
            functions: Listed::of(readers.iter().map(|r| r.fqn())),
            modules: Listed::of([module.to_owned()]),
            audience: Listed::of(audience.iter().map(|m| (*m).to_owned())),
            in_place: false,
            unseen: Listed::default(),
        },
        attribute: Some((attribute.class.to_owned(), attr.to_owned(), Vec::new())),
        members: Vec::new(),
        rank: (false, Reverse(audience.len()), Reverse(0)),
        shown: target,
    })
}

/// The `effect-boundaries` candidates: each module whose functions reach no blocking effect,
/// a package whose every module does as one scope, with at least [`MIN_SCOPE_FUNCTIONS`]
/// functions; none for the modules a declared boundary forbidding every blocking effect covers,
/// nor for one whose name a pattern cannot spell. `modules` leave out the modules holding test
/// code, `test_modules`.
fn effect_boundaries(
    index: &ProjectIndex,
    modules: &[&ModuleUse<'_>],
    test_modules: &FxHashSet<&str>,
    declared: &Declared,
) -> Vec<Candidate> {
    let qualifies = |m: &ModuleUse<'_>| {
        m.blocking.is_empty()
            && !m.functions.iter().any(|f| declared.bounds(f))
            && names_module_literally(m.module)
    };
    // Per dotted prefix of a module name (the name itself included): the modules at or below
    // it, and how many of them qualify.
    let mut below: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for m in modules {
        let ok = usize::from(qualifies(m));
        for (end, _) in m.module.match_indices('.').chain([(m.module.len(), "")]) {
            let counts = below.entry(&m.module[..end]).or_default();
            counts.0 += 1;
            counts.1 += ok;
        }
    }
    // Each qualifying module goes into the outermost package (two modules or more) whose every
    // module qualifies, or stands alone.
    let mut scopes: BTreeMap<&str, Vec<&ModuleUse<'_>>> = BTreeMap::new();
    for &m in modules.iter().filter(|m| qualifies(m)) {
        let package = (m.module.match_indices('.').map(|(end, _)| end))
            .chain([m.module.len()])
            .map(|end| &m.module[..end])
            .find(|p| matches!(below.get(p), Some(&(all, ok)) if all >= 2 && all == ok));
        scopes
            .entry(package.unwrap_or(m.module))
            .or_default()
            .push(m);
    }
    scopes.retain(|_, members| defined_functions(members).len() >= MIN_SCOPE_FUNCTIONS);
    // The packages holding test code: a wildcard below one would cover the tests too.
    let holding_tests: FxHashSet<&str> = (test_modules.iter())
        .flat_map(|t| t.match_indices('.').map(|(end, _)| &t[..end]))
        .collect();
    // Who calls into each scope from outside it.
    let scope_of: FxHashMap<&FuncKey, usize> = (scopes.values().enumerate())
        .flat_map(|(s, members)| {
            members
                .iter()
                .flat_map(|m| &m.functions)
                .map(move |f| (*f, s))
        })
        .collect();
    let mut callers: Vec<FxHashSet<&FuncKey>> = vec![FxHashSet::default(); scopes.len()];
    for file in index.files.values() {
        for site in &file.call_sites {
            let from = scope_of.get(&site.caller);
            for callee in site.project_callees() {
                if let Some(&s) = scope_of.get(&callee.callee)
                    && from != Some(&s)
                {
                    callers[s].insert(&site.caller);
                }
            }
        }
    }
    scopes
        .into_iter()
        .zip(callers)
        .map(|((scope, members), callers)| {
            let package = members.len() > 1 || members[0].module != scope;
            let mut patterns = Vec::new();
            if package && holding_tests.contains(scope) {
                patterns.extend(members.iter().map(|m| format!("{}:*", m.module)));
            } else {
                if members.iter().any(|m| m.module == scope) {
                    patterns.push(format!("{scope}:*"));
                }
                if package {
                    patterns.push(format!("{scope}.*:*"));
                }
            }
            let functions = defined_functions(&members);
            let audience = Listed::of(callers.iter().map(|c| c.module.clone()));
            Candidate {
                entry: Entry::Boundary(EffectBoundaryRule {
                    scopes: patterns,
                    forbid: vec!["blocking".to_owned()],
                    ..EffectBoundaryRule::default()
                }),
                name: format!("{scope} without blocking calls"),
                full_name: format!("{scope} without blocking calls"),
                evidence: Evidence {
                    functions: Listed::of(functions.iter().map(|f| f.fqn())),
                    modules: Listed::of(members.iter().map(|m| m.module.to_owned())),
                    in_place: false,
                    unseen: Listed::of(members.iter().flat_map(|m| &m.unseen).map(|f| f.fqn())),
                    audience,
                },
                attribute: None,
                members: Vec::new(),
                rank: (
                    false,
                    Reverse(functions.len() * callers.len()),
                    Reverse(functions.len()),
                ),
                shown: scope.to_owned(),
            }
        })
        .collect()
}

/// `cfg` with every candidate declared after its own contracts, the ownership contracts with
/// `unresolved = "report"`: the configuration of the contract trial.
pub(super) fn trial_config(cfg: &HekiConfig, generated: &Generated) -> HekiConfig {
    let mut trial = cfg.clone();
    for candidate in &generated.candidates {
        let mut entry = candidate.entry.clone();
        entry.set_unresolved(true);
        entry.declare_in(&mut trial);
    }
    trial
}

/// The rules the contract trial runs: those of the kinds it declares candidates of.
pub(super) fn trial_rules(generated: &Generated) -> Vec<&'static str> {
    let kinds: BTreeSet<ContractKind> = generated
        .candidates
        .iter()
        .map(|c| c.entry.kind())
        .collect();
    kinds
        .into_iter()
        .flat_map(|k| std::iter::once(k.rule()).chain(k.strict_rule()))
        .collect()
}

/// A candidate the trial kept.
#[derive(Debug, Clone)]
struct Kept {
    candidate: Candidate,
    confidence: Confidence,
    trial: Trial,
    caveats: Vec<String>,
}

/// Section 3: the candidates the trial kept, those it rejected, and what was not tried.
#[derive(Debug, Default)]
pub(super) struct Section {
    kept: Vec<Kept>,
    pub rejected: Vec<RejectedContract>,
    pub not_tried: Vec<NotTried>,
    ignored: Vec<ContractKind>,
    /// The block sets `src`: the candidates, which name modules as the files are named now, wait
    /// for a run with it.
    deferred: bool,
}

/// Where each report a run made belongs: the candidates `entries` (in the order they are
/// declared, after `cfg`'s own contracts of their kind), by the contracts it is of.
fn attribute_reports<'d>(
    cfg: &HekiConfig,
    entries: &[&Entry],
    kept: &'d [Diagnostic],
    suppressed: &'d [Diagnostic],
) -> Vec<Vec<(&'d Diagnostic, bool)>> {
    let mut place: FxHashMap<(ContractKind, usize), usize> = FxHashMap::default();
    let mut next: FxHashMap<ContractKind, usize> = FxHashMap::default();
    for (i, entry) in entries.iter().enumerate() {
        let kind = entry.kind();
        let at = next.entry(kind).or_insert_with(|| kind.declared_in(cfg));
        place.insert((kind, *at), i);
        *at += 1;
    }
    let mut out = vec![Vec::new(); entries.len()];
    let reports = (kept.iter().map(|d| (d, false)))
        .chain(suppressed.iter().map(|d| (d, true)))
        .filter_map(|(d, dropped)| Some((d, dropped, ContractKind::of_rule(&d.rule_code)?)));
    for (d, dropped, kind) in reports {
        for &p in &d.contracts {
            if let Some(&i) = usize::try_from(p).ok().and_then(|p| place.get(&(kind, p))) {
                out[i].push((d, dropped));
            }
        }
    }
    out
}

/// What a block setting `src` changes, which a run on the files as they are named now does not
/// see.
pub(super) const SRC_RENAMES: &str =
    "the block sets src, which renames modules and resolves imports heki does not resolve now";

/// The reports of `cfg`'s own contracts among `diags`: a candidate must leave each reported.
pub(super) fn configured_reports(cfg: &HekiConfig, diags: &[Diagnostic]) -> Vec<Diagnostic> {
    (diags.iter())
        .filter(|d| of_configured(cfg, d))
        .cloned()
        .collect()
}

/// Whether `d` is a report of one of `cfg`'s own contracts, which a trial declares before the
/// candidates.
fn of_configured(cfg: &HekiConfig, d: &Diagnostic) -> bool {
    ContractKind::of_rule(&d.rule_code).is_some_and(|kind| {
        let declared = kind.declared_in(cfg);
        (d.contracts.iter()).any(|&p| usize::try_from(p).is_ok_and(|p| p < declared))
    })
}

/// A report as two runs over the same files both make it.
#[derive(PartialEq, Eq, Hash)]
struct ReportKey<'d> {
    rule: &'d str,
    path: &'d Path,
    span: (u32, u32),
    target: &'d str,
}

impl<'d> ReportKey<'d> {
    fn of(d: &'d Diagnostic) -> Self {
        Self {
            rule: &d.rule_code,
            path: &d.path,
            span: (d.span.start, d.span.end),
            target: &d.target,
        }
    }
}

/// The trial of a candidate of `kind` from the reports a run attributes to it.
fn tally(
    kind: ContractKind,
    reports: &[(&Diagnostic, bool)],
    shown: &dyn Fn(&Path) -> String,
) -> Trial {
    let mut trial = Trial::of(kind);
    for &(d, suppressed) in reports {
        if suppressed {
            trial.suppressed += 1;
            continue;
        }
        *trial.reports.entry(d.rule_code.clone()).or_default() += 1;
        if trial.examples.len() < MAX_LISTED {
            trial.examples.push(TrialReport {
                rule: d.rule_code.clone(),
                path: shown(&d.path),
                line: d.line,
                message: d.message.clone(),
            });
        }
    }
    trial
}

/// Judge every candidate of `generated` by what the contract trial, a run of `cfg` with them
/// all declared ([`trial_config`]), reported (`kept` after inline suppressions, `suppressed`
/// dropped by them). `configured_now`: the reports of `cfg`'s own contracts on the code as it is
/// ([`configured_reports`]). `unanalyzed`: the Python files below the project root left out, and
/// how many heki could not read or parse.
pub(super) fn judge<'d>(
    generated: Generated,
    cfg: &HekiConfig,
    (kept, suppressed): (&'d [Diagnostic], &'d [Diagnostic]),
    configured_now: &'d [Diagnostic],
    unanalyzed: (&LeftOut, usize),
    shown: &dyn Fn(&Path) -> String,
) -> Section {
    let Generated {
        candidates,
        not_tried,
        ignored,
        strict_ignored,
    } = generated;
    let entries: Vec<&Entry> = candidates.iter().map(|c| &c.entry).collect();
    let mut reports = attribute_reports(cfg, &entries, kept, suppressed);
    // A target lists its `__dict__` key for every contract (`unlisted-dict-keys`), on its class
    // and the subclasses, so a candidate can hide a report the configuration makes now: that
    // report counts as its own.
    let still: FxHashSet<ReportKey<'_>> = (kept.iter())
        .filter(|d| of_configured(cfg, d))
        .map(ReportKey::of)
        .collect();
    for d in configured_now
        .iter()
        .filter(|d| !still.contains(&ReportKey::of(d)))
    {
        for (candidate, reports) in candidates.iter().zip(&mut reports) {
            if ContractKind::of_rule(&d.rule_code) == Some(candidate.entry.kind())
                && candidate.names_member(&d.target)
            {
                reports.push((d, false));
            }
        }
    }
    let mut section = Section {
        not_tried,
        ignored,
        ..Section::default()
    };
    for (candidate, reports) in candidates.into_iter().zip(reports) {
        let kind = candidate.entry.kind();
        let mut kept = Kept {
            trial: tally(kind, &reports, shown),
            candidate,
            confidence: Confidence::High,
            caveats: Vec::new(),
        };
        kept.weigh(unanalyzed, strict_ignored.contains(&kind));
        section.kept.push(kept);
    }
    let trials = section.kept.iter().map(|k| k.trial.clone()).collect();
    section.reject(RunLabel::ContractTrial, trials);
    section.leave_out_covered();
    section
}

impl Kept {
    /// The confidence of a candidate the trial kept, and its entry's `unresolved`: `"report"`
    /// only when the confidence is high and the configuration does not ignore the rule of such
    /// accesses (`strict_ignored`). A medium one may leave accesses of unknown class unseen (in
    /// the files left out or not parsed), which that rule would report once `heki check` sees
    /// them.
    fn weigh(&mut self, (left_out, unparsed): (&LeftOut, usize), strict_ignored: bool) {
        let kind = self.candidate.entry.kind();
        let unresolved = self.trial.reports_of(kind.strict_rule());
        if unresolved > 0 {
            self.caveats.push(format!(
                "{} reports {unresolved} access(es) on objects of unknown class: heki cannot tell \
                 whether they are of the target, so the entry sets unresolved = \"ignore\"",
                kind.strict_rule().unwrap_or_default()
            ));
        }
        let unseen = self.candidate.evidence.unseen.count;
        if unseen > 0 {
            self.caveats.push(format!(
                "{unseen} function(s) of the scopes reach code heki does not see"
            ));
        }
        if left_out.python_files > 0 {
            self.caveats.push(
                "Python files below the project root were not analyzed: their accesses are not \
                 seen"
                    .to_owned(),
            );
        } else if left_out.truncated {
            self.caveats.push(
                "the count of the Python files below the project root stopped before the end: \
                 files it did not reach may be left out, and their accesses are not seen"
                    .to_owned(),
            );
        }
        if unparsed > 0 {
            self.caveats.push(format!(
                "{unparsed} Python file(s) heki could not read or parse: their accesses are not \
                 seen"
            ));
        }
        if !self.caveats.is_empty() {
            self.confidence = Confidence::Medium;
        }
        self.candidate
            .entry
            .set_unresolved(self.confidence == Confidence::High && !strict_ignored);
        self.caveats.push(UNSEEN_CAVEAT.to_owned());
    }

    /// Whether `trial`, what a run reported for the entry, rejects the entry as the block would
    /// declare it: a report under the contract's rule, or under the rule of the accesses it
    /// cannot tie to a class while the entry reports them.
    fn reported_by(&self, trial: &Trial) -> bool {
        let entry = &self.candidate.entry;
        let kind = entry.kind();
        trial.reports_of(Some(kind.rule())) > 0
            || entry.reports_unresolved() && trial.reports_of(kind.strict_rule()) > 0
    }
}

impl Section {
    /// The section of a project without candidates to try: what generation left out.
    pub(super) fn untried(generated: Generated) -> Self {
        Self {
            not_tried: generated.not_tried,
            ignored: generated.ignored,
            ..Self::default()
        }
    }

    /// The section when the block sets `src`: a new source root renames modules and resolves
    /// imports the discovery run did not, so the candidates of that run, which name modules as
    /// the files are named now, are not tried.
    pub(super) fn deferred(generated: Generated) -> Self {
        Self {
            deferred: !generated.is_empty() || !generated.not_tried.is_empty(),
            ignored: generated.ignored,
            ..Self::default()
        }
    }

    /// Leave out an ownership candidate a kept candidate of a base class covers: a target of a
    /// class covers its subclasses.
    fn leave_out_covered(&mut self) {
        let kept: FxHashSet<(ContractKind, &str, &str)> = (self.kept.iter())
            .filter_map(|k| {
                let (class, attr, _) = k.candidate.attribute.as_ref()?;
                Some((k.candidate.entry.kind(), class.as_str(), attr.as_str()))
            })
            .collect();
        let covered: Vec<bool> = (self.kept.iter())
            .map(|k| {
                k.candidate
                    .attribute
                    .as_ref()
                    .is_some_and(|(_, attr, ancestors)| {
                        let kind = k.candidate.entry.kind();
                        (ancestors.iter()).any(|base| kept.contains(&(kind, base, attr)))
                    })
            })
            .collect();
        let mut covered = covered.into_iter();
        self.kept.retain(|_| !covered.next().unwrap_or(false));
    }

    /// The entries the block declares, in its order.
    fn entries(&self) -> Vec<&Entry> {
        self.kept.iter().map(|k| &k.candidate.entry).collect()
    }

    /// Declare the block's entries in `cfg`, after its own.
    pub(super) fn declare_in(&self, cfg: &mut HekiConfig) {
        for entry in self.entries() {
            entry.declare_in(cfg);
        }
    }

    /// The rules the block's contracts need that `current` does not run: the rule of each kind
    /// it declares, and the rule of the accesses an ownership contract cannot tie to a class
    /// where one sets `unresolved = "report"`.
    pub(super) fn rules_to_select(&self, current: &RuleSelection) -> Vec<String> {
        self.rules()
            .filter(|r| !current.is_enabled(r))
            .map(str::to_owned)
            .collect()
    }

    /// Every rule the block's contracts report under.
    pub(super) fn rules(&self) -> impl Iterator<Item = &'static str> + '_ {
        let mut rules = BTreeSet::new();
        for entry in self.entries() {
            rules.insert(entry.kind().rule());
            if entry.reports_unresolved() {
                rules.extend(entry.kind().strict_rule());
            }
        }
        rules.into_iter()
    }

    /// The block's entries, as the block writes them.
    pub(super) fn block_entries(
        &self,
    ) -> (
        Vec<WriteOwnerRule>,
        Vec<ReadOwnerRule>,
        Vec<EffectBoundaryRule>,
    ) {
        let (mut writes, mut reads, mut boundaries) = (Vec::new(), Vec::new(), Vec::new());
        for entry in self.entries() {
            match entry {
                Entry::Write(e) => writes.push(e.clone()),
                Entry::Read(e) => reads.push(e.clone()),
                Entry::Boundary(e) => boundaries.push(e.clone()),
            }
        }
        (writes, reads, boundaries)
    }

    /// Move the candidates a run reported for into the rejected ones, under `run`: `trials`
    /// holds what the run reported for each, in order. Whether there was one.
    fn reject(&mut self, run: RunLabel, trials: Vec<Trial>) -> bool {
        let before = self.rejected.len();
        let mut trials = trials.into_iter();
        let mut rejected = Vec::new();
        self.kept.retain(|k| {
            let trial = trials.next().unwrap_or_default();
            let reported = k.reported_by(&trial);
            if reported {
                rejected.push(RejectedContract {
                    kind: k.candidate.entry.kind(),
                    target: k.candidate.shown.clone(),
                    entry: k.candidate.entry.keys(),
                    run,
                    trial,
                });
            }
            !reported
        });
        self.rejected.extend(rejected);
        self.rejected.len() > before
    }

    /// The candidates the block declares, as the report lists them.
    pub(super) fn advice(&self) -> Vec<ContractAdvice> {
        (self.kept.iter())
            .map(|k| ContractAdvice {
                kind: k.candidate.entry.kind(),
                target: k.candidate.shown.clone(),
                entry: k.candidate.entry.keys(),
                confidence: k.confidence,
                evidence: k.candidate.evidence.clone(),
                trial: k.trial.clone(),
                caveats: k.caveats.clone(),
            })
            .collect()
    }

    /// Notes on what the section left out.
    pub(super) fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.deferred {
            notes.push(format!(
                "{SRC_RENAMES}: run heki recommend again with it for contract candidates"
            ));
        }
        for kind in &self.ignored {
            notes.push(format!(
                "the configuration ignores {}, so no {} contract is proposed",
                kind.rule(),
                kind.key()
            ));
        }
        notes
    }
}

/// The first [`MAX_EXAMPLES`] of `listed`, with `...` past them.
fn examples(listed: &Listed) -> String {
    let mut out: Vec<&str> = listed
        .items
        .iter()
        .take(MAX_EXAMPLES)
        .map(String::as_str)
        .collect();
    if listed.count > MAX_EXAMPLES {
        out.push("...");
    }
    out.join(", ")
}

/// A trial's counts as the text prints them: `MUT201 0, MUT205 2; 1 suppressed inline`.
fn counts(trial: &Trial) -> String {
    // The contract's rule first: `MUT201` sorts before `MUT205`.
    let parts: Vec<String> = (trial.reports.iter())
        .map(|(rule, n)| format!("{rule} {n}"))
        .collect();
    let mut out = parts.join(", ");
    if trial.suppressed > 0 {
        let _ = write!(out, "; {} suppressed inline", trial.suppressed);
    }
    out
}

/// The section as the text prints it.
pub(super) fn render(
    out: &mut String,
    contracts: &[ContractAdvice],
    rejected: &[RejectedContract],
    not_tried: &[NotTried],
) {
    out.push_str("\nContract candidates\n");
    if contracts.is_empty() && rejected.is_empty() {
        out.push_str("  None here.\n");
    }
    for c in contracts {
        let confidence = match c.confidence {
            Confidence::High => "high",
            Confidence::Medium => "medium",
        };
        let _ = writeln!(out, "  {}  {}  {confidence}", c.kind.key(), c.target);
        let e = &c.evidence;
        let module = e.modules.items.first().map_or("", String::as_str);
        let local = |fqn: &str| -> String {
            fqn.strip_prefix(module)
                .and_then(|q| q.strip_prefix('.'))
                .unwrap_or(fqn)
                .to_owned()
        };
        let named: Vec<String> = e.functions.items.iter().map(|f| local(f)).collect();
        let named = Listed {
            count: e.functions.count,
            items: named,
            truncated: e.functions.truncated,
        };
        let line = match c.kind {
            ContractKind::WriteOwners if e.functions.count == 0 => format!(
                "written only while its objects are constructed; read in {} other module(s)",
                e.audience.count
            ),
            ContractKind::WriteOwners => format!(
                "written by {} function(s), all in {module} ({}){}; read in {} other module(s)",
                e.functions.count,
                examples(&named),
                if e.in_place { ", changed in place" } else { "" },
                e.audience.count
            ),
            ContractKind::ReadOwners => format!(
                "read by {} function(s), all in {module} ({}); the class is used in {} other \
                 module(s)",
                e.functions.count,
                examples(&named),
                e.audience.count
            ),
            ContractKind::EffectBoundaries => {
                let mut line = format!(
                    "{} function(s) in {} module(s) reach no blocking call heki sees",
                    e.functions.count, e.modules.count
                );
                if e.audience.count > 0 {
                    let _ = write!(line, "; called from {} other module(s)", e.audience.count);
                }
                if e.unseen.count > 0 {
                    let _ = write!(
                        line,
                        "; {} reach code heki does not see ({})",
                        e.unseen.count,
                        examples(&e.unseen)
                    );
                }
                line
            }
        };
        let _ = writeln!(out, "    {line}");
        let mut trial = format!("trial: {}", counts(&c.trial));
        if let Some(strict) = c.kind.strict_rule() {
            let unresolved = c.trial.reports_of(Some(strict));
            if unresolved > 0 {
                let at: Vec<String> = (c.trial.examples.iter())
                    .filter(|r| r.rule == strict)
                    .take(MAX_EXAMPLES)
                    .map(|r| format!("{}:{}", r.path, r.line))
                    .collect();
                let more = if unresolved > MAX_EXAMPLES {
                    ", ..."
                } else {
                    ""
                };
                let _ = write!(
                    trial,
                    " (at {}{more}), so unresolved = \"ignore\"",
                    at.join(", ")
                );
            } else if c.entry.get("unresolved").and_then(|v| v.as_str()) == Some("report") {
                trial.push_str(" (unresolved = \"report\")");
            } else if c.confidence == Confidence::Medium {
                trial.push_str(" (unresolved = \"ignore\", as for every medium candidate)");
            } else {
                let _ = write!(
                    trial,
                    " (unresolved = \"ignore\": the configuration ignores {strict})"
                );
            }
        }
        let _ = writeln!(out, "    {trial}");
    }
    for r in rejected {
        let first = (r.trial.examples.first())
            .map(|e| format!(": {} at {}:{}", e.rule, e.path, e.line))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "  Rejected: {} {} ({}){first}",
            r.kind.key(),
            r.target,
            counts(&r.trial)
        );
    }
    for n in not_tried {
        let _ = writeln!(
            out,
            "  Not tried: {} more {} candidate(s) past the first {}",
            n.candidates,
            n.kind.key(),
            n.tried
        );
    }
    if !contracts.is_empty() {
        let _ = writeln!(out, "  For every candidate, {UNSEEN_CAVEAT}.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_allowed_pattern_is_the_smallest_form_naming_every_function() {
        let classes = ["Store", "Store.Inner", "Other"];
        let is_class = |q: &str| classes.contains(&q);
        for (qualnames, expected) in [
            (&["Store.add", "Store.clear"][..], "app.store:Store.*"),
            // A function nested in a method is the class's too.
            (&["Store.add", "Store.add.fill"], "app.store:Store.*"),
            // The innermost class enclosing them all.
            (
                &["Store.Inner.put", "Store.Inner.take"],
                "app.store:Store.Inner.*",
            ),
            (&["Store.Inner.put", "Store.add"], "app.store:Store.*"),
            // Methods of two classes, a module-level function, the module body.
            (&["Store.add", "Other.add"], "app.store:*"),
            (&["Store.add", "reset"], "app.store:*"),
            (&["reset"], "app.store:*"),
            (&["<module>"], "app.store:*"),
            // A function enclosing another is no class.
            (&["build.fill", "build.empty"], "app.store:*"),
        ] {
            assert_eq!(
                allowed_pattern("app.store", qualnames, is_class),
                expected,
                "{qualnames:?}"
            );
        }
    }
}
