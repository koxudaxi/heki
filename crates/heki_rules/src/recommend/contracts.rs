//! "Contract candidates": the `write-owners`, `read-owners`, and `effect-boundaries` entries the
//! code already keeps, drawn from the census of the discovery run ([`crate::census`]), never from
//! names, each kept only when a trial of its rules reports nothing on the code as it is, and the
//! kept ones merged into a few blocks.
//!
//! Generation may propose too much; the trial decides. It declares every candidate at once, the
//! ownership contracts with `unresolved = "report"`, and runs their rules as `heki check` would
//! (inline suppressions and `per-file-ignores` apply); each report counts for the contracts it is
//! of ([`Diagnostic::contracts`]). A candidate the rule of its contract reports is rejected, and
//! so is one that hides a report of the configuration's own contracts ([`judge`]); reports of
//! accesses on objects of unknown class make it medium instead.
//!
//! The kept candidates whose entries differ only in their targets (or scopes) become one block,
//! whose reports are theirs: a contract reports each access of its targets that it does not
//! allow, whichever target the access is of. A block may name a class's attributes by a pattern
//! (`module:Class.*`) where the census sees no access the block would not allow; the pattern
//! trial declares each such pattern alone, and the block keeps it only when that reports nothing
//! the targets it stands for did not. The first [`MAX_BLOCKS_SHOWN`] blocks of each kind are
//! shown, and the verification run applies them.

use super::block::set_keys;
use super::{LeftOut, MAX_EXAMPLES, MAX_LISTED, RunLabel};
use crate::DottedPattern;
use crate::census::{Accesses, AttributeUse, Census, ModuleUse};
use crate::contracts::Declared;
use crate::eff_rules::names_function;
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

/// The blocks of each kind the report shows, the first by rank, unless it shows every one
/// (`heki recommend --all`).
pub const MAX_BLOCKS_SHOWN: usize = 3;

/// The sites keeping blocks at `unresolved = "ignore"` the report lists, the first by the
/// candidates they hold back, unless it lists up to [`MAX_LISTED`] (`heki recommend --all`).
pub const MAX_BLOCKERS_SHOWN: usize = 5;

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

/// A block the configuration declares: the candidates the contract trial kept whose entries
/// differ only in their targets (or scopes), as one entry.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ContractAdvice {
    pub kind: ContractKind,
    /// What the block is about, sorted: the attributes (`module:Class.attr`) and class patterns
    /// (`module:Class.*`) its targets name, or the packages and modules its scopes cover.
    pub targets: Vec<String>,
    /// The candidates the block stands for.
    pub candidates: usize,
    /// The entry the block declares, as the keys it sets.
    pub entry: serde_json::Map<String, serde_json::Value>,
    /// The lowest of its candidates'.
    pub confidence: Confidence,
    /// Its candidates' together.
    pub evidence: Evidence,
    /// What the contract trial reported for its candidates, each report once.
    pub trial: Trial,
    /// The class patterns the pattern trial tried in place of a class's targets.
    pub patterns: Vec<PatternAdvice>,
    /// What the confidence does not cover.
    pub caveats: Vec<String>,
}

/// A pattern naming every attribute of a class and its subclasses (`module:Class.*`; for
/// `read-owners`, the private ones, `module:Class._*`), tried in place of the targets of the
/// class a block lists: it also names the attributes the code adds later.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PatternAdvice {
    pub pattern: String,
    /// The targets of the block it stands for, sorted.
    pub replaces: Vec<String>,
    /// Whether the block names it in place of them: the pattern trial reported nothing for it
    /// that they did not.
    pub kept: bool,
    /// What the pattern trial reported for it, declared alone with the block's other keys.
    pub trial: Trial,
}

/// The blocks of one kind past the first [`MAX_BLOCKS_SHOWN`], which the report leaves out
/// (`heki recommend --all` shows them).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct NotShown {
    pub kind: ContractKind,
    pub blocks: usize,
    /// The candidates they stand for.
    pub candidates: usize,
}

/// The accesses heki cannot tie to a class (`MUT205`, `MUT206`) that keep blocks at
/// `unresolved = "ignore"`: each site once, with what it holds back.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ContractBlockers {
    pub count: usize,
    /// The first by the candidates they hold back, then by path and line:
    /// [`MAX_BLOCKERS_SHOWN`], or with `heki recommend --all` up to [`MAX_LISTED`].
    pub items: Vec<ContractBlocker>,
    /// Whether `items` holds fewer than `count`.
    pub truncated: bool,
}

/// One site of [`ContractBlockers`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ContractBlocker {
    pub rule: String,
    /// As `heki check` writes it.
    pub path: String,
    pub line: u32,
    /// The function the access is judged in.
    pub function: String,
    /// The report as `heki check` writes it for the first candidate it holds back.
    pub message: String,
    /// The candidates whose trial reported it, and the blocks they are in, shown or not.
    pub candidates: usize,
    pub blocks: usize,
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

    /// What the entry is about: an ownership entry's `targets`, a boundary's `scopes`.
    fn subjects(&self) -> &[String] {
        match self {
            Self::Write(e) => &e.targets,
            Self::Read(e) => &e.targets,
            Self::Boundary(e) => &e.scopes,
        }
    }

    fn set_subjects(&mut self, subjects: Vec<String>) {
        match self {
            Self::Write(e) => e.targets = subjects,
            Self::Read(e) => e.targets = subjects,
            Self::Boundary(e) => e.scopes = subjects,
        }
    }

    /// The keys the entry sets but its name and subjects: entries with the same ones merge into
    /// one block.
    fn merge_key(&self) -> String {
        let mut keys = self.keys();
        for key in ["name", "targets", "scopes"] {
            keys.remove(key);
        }
        serde_json::Value::Object(keys).to_string()
    }

    /// What an ownership entry allows besides construction: its allowed functions, and whether
    /// it allows the accesses through a method's own receiver. `None` for a boundary.
    fn allowance(&self) -> Option<Allowance> {
        let (allowed, own_receiver) = match self {
            Self::Write(e) => (&e.allowed_writers, &e.own_receiver),
            Self::Read(e) => (&e.allowed_readers, &e.own_receiver),
            Self::Boundary(_) => return None,
        };
        Some(Allowance {
            functions: (allowed.iter())
                .filter_map(|p| DottedPattern::new(p, "allowed").ok())
                .collect(),
            own_receiver: own_receiver.as_deref() == Some("allow"),
        })
    }
}

/// The functions an ownership entry allows to access its targets after construction.
struct Allowance {
    functions: Vec<DottedPattern>,
    /// Every access through the receiver of the method making it (`own-receiver = "allow"`).
    own_receiver: bool,
}

impl Allowance {
    fn allows(&self, access: &Access) -> bool {
        names_function(&self.functions, &access.function)
            || self.own_receiver && access.own_receiver
    }
}

/// What a candidate or a block rests on, in full: [`Evidence`] lists the first of each.
#[derive(Debug, Clone, Default)]
struct Seen {
    functions: BTreeSet<String>,
    modules: BTreeSet<String>,
    audience: BTreeSet<String>,
    in_place: bool,
    unseen: BTreeSet<String>,
}

impl Seen {
    fn evidence(&self) -> Evidence {
        Evidence {
            functions: Listed::of(self.functions.iter().cloned()),
            modules: Listed::of(self.modules.iter().cloned()),
            audience: Listed::of(self.audience.iter().cloned()),
            in_place: self.in_place,
            unseen: Listed::of(self.unseen.iter().cloned()),
        }
    }

    fn add(&mut self, other: &Self) {
        self.functions.extend(other.functions.iter().cloned());
        self.modules.extend(other.modules.iter().cloned());
        self.audience.extend(other.audience.iter().cloned());
        self.in_place |= other.in_place;
        self.unseen.extend(other.unseen.iter().cloned());
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
    seen: Seen,
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

/// A function accessing an attribute of a class's family, as a class pattern's entry would
/// judge it.
#[derive(Debug, Clone)]
struct Access {
    function: FuncKey,
    /// Every access is through the receiver of the method making it.
    own_receiver: bool,
}

/// What a class pattern of a block names: every attribute of a project class and of its
/// subclasses (`module:Class.*`), or for `read-owners` the private ones (`module:Class._*`).
#[derive(Debug, Clone)]
struct ClassFacts {
    /// The class's module and its qualified name there.
    module: String,
    qualname: String,
    /// The qualified names of the class and of its project subclasses, which a report of a
    /// `__dict__` key no target names gives (`Class.attr`).
    family: Vec<String>,
    /// The functions writing an attribute of the family after its object is constructed.
    writers: Vec<Access>,
    /// The functions reading an attribute of the family whose name starts with `_`.
    private_readers: Vec<Access>,
}

impl ClassFacts {
    /// The pattern naming the attributes of the family a `kind` block is about.
    fn pattern(&self, kind: ContractKind) -> String {
        let last = if kind == ContractKind::ReadOwners {
            "_*"
        } else {
            "*"
        };
        format!("{}:{}.{last}", self.module, self.qualname)
    }

    /// The accesses the pattern of a `kind` block judges.
    fn accesses(&self, kind: ContractKind) -> &[Access] {
        if kind == ContractKind::ReadOwners {
            &self.private_readers
        } else {
            &self.writers
        }
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
    /// The classes of the ownership candidates tried, by their fully qualified names.
    classes: FxHashMap<String, ClassFacts>,
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
/// candidate either. An ownership candidate is judged by the accesses of its class's family (the
/// class and its project subclasses), which its target covers.
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
    let census = Census::of(index, &decls);
    let class_modules: FxHashMap<&str, &[&str]> = (census.classes.iter())
        .map(|c| (c.class, c.modules.as_slice()))
        .collect();
    let mut by_class: FxHashMap<&str, Vec<&AttributeUse<'_>>> = FxHashMap::default();
    let mut by_attribute: FxHashMap<(&str, &str), &AttributeUse<'_>> = FxHashMap::default();
    for a in &census.attributes {
        by_class.entry(a.class).or_default().push(a);
        by_attribute.insert((a.class, a.attr), a);
    }
    let mut families: FxHashMap<&str, Vec<String>> = FxHashMap::default();
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
        let family_classes = families
            .entry(class)
            .or_insert_with(|| family_of(&decls, class));
        let family: Vec<&AttributeUse<'_>> = (family_classes.iter())
            .filter_map(|c| by_attribute.get(&(c.as_str(), attr)).copied())
            .collect();
        let site = (&decls, module, qualname);
        if proposes(ContractKind::WriteOwners) && !declared.owns_writes(class, attr) {
            by_kind
                .entry(ContractKind::WriteOwners)
                .or_default()
                .extend(write_owner(site, attribute, &family, tests));
        }
        if proposes(ContractKind::ReadOwners)
            && is_private_attribute(attr)
            && !declared.owns_reads(class, attr)
        {
            by_kind
                .entry(ContractKind::ReadOwners)
                .or_default()
                .extend(read_owner(site, attribute, &family, &class_modules, tests));
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
                if !out.classes.contains_key(class.as_str()) {
                    let family = &families[class.as_str()];
                    let facts = class_facts(&decls, class, family, &by_class);
                    out.classes.insert(class.clone(), facts);
                }
            }
            out.candidates.push(candidate);
        }
    }
    out
}

/// The project class `class` and its project subclasses, by their fully qualified names.
fn family_of(decls: &ProjectDecls, class: &str) -> Vec<String> {
    std::iter::once(class.to_owned())
        .chain(decls.subclasses_of(class))
        .collect()
}

/// What a pattern on `class` (fully qualified; `family` it and its subclasses) names, from the
/// census's attributes `by_class`. A function of test code is never in a block's owner module,
/// so only `own-receiver = "allow"` can allow its accesses.
fn class_facts(
    decls: &ProjectDecls,
    class: &str,
    family: &[String],
    by_class: &FxHashMap<&str, Vec<&AttributeUse<'_>>>,
) -> ClassFacts {
    let qualname = decls.class_qualname(class).unwrap_or(class);
    let uses: Vec<&AttributeUse<'_>> = (family.iter())
        .filter_map(|c| by_class.get(c.as_str()))
        .flatten()
        .copied()
        .collect();
    let writers = (uses.iter())
        .flat_map(|a| &a.writers)
        .filter(|w| !w.fresh)
        .map(|w| &w.accesses);
    let private_readers = (uses.iter())
        .filter(|a| a.attr.starts_with('_'))
        .flat_map(|a| &a.readers);
    ClassFacts {
        module: class[..class.len() - qualname.len() - 1].to_owned(),
        qualname: qualname.to_owned(),
        family: (family.iter())
            .filter_map(|c| decls.class_qualname(c))
            .map(str::to_owned)
            .collect(),
        writers: by_function(writers),
        private_readers: by_function(private_readers),
    }
}

/// `accesses` by function, each through its own receiver only when all of them are.
fn by_function<'a, 'i: 'a>(accesses: impl Iterator<Item = &'a Accesses<'i>>) -> Vec<Access> {
    let mut by_function: BTreeMap<&FuncKey, Access> = BTreeMap::new();
    for a in accesses {
        let access = by_function.entry(a.function).or_insert_with(|| Access {
            function: a.function.clone(),
            own_receiver: true,
        });
        access.own_receiver &= a.own_receiver;
    }
    by_function.into_values().collect()
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

/// Who accesses an attribute's family besides its objects' construction, as an ownership entry
/// allows them.
struct Owners<'i> {
    /// The module whose functions the entry allows, with the qualified names of those of them
    /// accessing the attribute; `None` when it allows none: only construction and, with
    /// `own_receiver`, methods through their own receiver access it.
    module: Option<(&'i str, Vec<&'i str>)>,
    /// A method accessing it only through its own receiver lies outside `module`: the entry
    /// sets `own-receiver = "allow"`.
    own_receiver: bool,
}

impl Owners<'_> {
    /// The entry's allowed functions: the smallest pattern naming those of `module`.
    fn allowed(&self, decls: &ProjectDecls) -> Vec<String> {
        (self.module.iter())
            .map(|(module, qualnames)| {
                let is_class = |q: &str| decls.class_qualname(&format!("{module}.{q}")).is_some();
                allowed_pattern(module, qualnames, is_class)
            })
            .collect()
    }

    fn own_receiver(&self) -> Option<String> {
        self.own_receiver.then(|| "allow".to_owned())
    }
}

/// The owners of `accesses` (each function's accesses of an attribute's family), `home` the
/// module of its class: the one module of the functions not accessing it only through their own
/// receiver, or without such functions `home` when another accesses it there, else the one module
/// of the others. `None` when no entry allows them: such functions in two modules or in test
/// code, which never defines an owner (its access would be a report of the contract), or an
/// owner module a pattern cannot spell.
fn owners_of<'i>(
    accesses: &[&Accesses<'i>],
    home: &'i str,
    tests: &FxHashSet<&Path>,
) -> Option<Owners<'i>> {
    let in_tests = |a: &Accesses<'_>| tests.contains(a.path);
    let direct: Vec<&Accesses<'i>> = (accesses.iter().copied())
        .filter(|a| !a.own_receiver)
        .collect();
    if direct.iter().any(|a| in_tests(a)) {
        return None;
    }
    let program: Vec<&Accesses<'i>> = (accesses.iter().copied())
        .filter(|a| !in_tests(a))
        .collect();
    let one_module = |of: &[&Accesses<'i>]| -> Option<&'i str> {
        let first = of.first()?.function.module.as_str();
        of.iter()
            .all(|a| a.function.module == first)
            .then_some(first)
    };
    let module = if !direct.is_empty() {
        Some(one_module(&direct)?)
    } else if program.iter().any(|a| a.function.module == home) {
        Some(home)
    } else {
        one_module(&program)
    };
    if module.is_some_and(|m| !names_module_literally(m)) {
        return None;
    }
    let own_receiver = (accesses.iter()).any(|a| Some(a.function.module.as_str()) != module);
    let module = module.map(|m| {
        let qualnames = (program.iter())
            .filter(|a| a.function.module == m)
            .map(|a| a.function.qualname.as_str())
            .collect();
        (m, qualnames)
    });
    Some(Owners {
        module,
        own_receiver,
    })
}

/// A `write-owners` candidate for `attribute`, `family` the uses of its name by the class and its
/// project subclasses: written after construction by functions of one module only (methods
/// through their own receiver may be anywhere), or only while its objects are constructed
/// (`allowed-writers = []`), and read in at least [`MIN_AUDIENCE`] other modules.
fn write_owner(
    (decls, module, qualname): ClassSite<'_>,
    attribute: &AttributeUse<'_>,
    family: &[&AttributeUse<'_>],
    tests: &FxHashSet<&Path>,
) -> Option<Candidate> {
    if (family.iter()).all(|a| a.writers.is_empty() && a.initializers.is_empty()) {
        return None;
    }
    // A factory filling the object it constructs writes it while it is constructed: the
    // contracts allow it (`fresh-objects = "allow"`).
    let writers: Vec<&Accesses<'_>> = (family.iter())
        .flat_map(|a| &a.writers)
        .filter(|w| !w.fresh)
        .map(|w| &w.accesses)
        .collect();
    let owners = owners_of(&writers, module, tests)?;
    let owner: &str = owners.module.as_ref().map_or(module, |(m, _)| *m);
    let audience: BTreeSet<&str> = (family.iter())
        .flat_map(|a| &a.readers)
        .map(|r| r.function.module.as_str())
        .filter(|m| *m != owner)
        .collect();
    if audience.len() < MIN_AUDIENCE {
        return None;
    }
    let attr = attribute.attr;
    let target = format!("{module}:{qualname}.{attr}");
    let in_place = (family.iter())
        .flat_map(|a| &a.writers)
        .any(|w| !w.fresh && w.in_place);
    let mut modules: BTreeSet<String> = (writers.iter())
        .map(|w| w.function.module.clone())
        .collect();
    if modules.is_empty() {
        modules.insert(module.to_owned());
    }
    Some(Candidate {
        entry: Entry::Write(WriteOwnerRule {
            targets: vec![target.clone()],
            allowed_writers: owners.allowed(decls),
            own_receiver: owners.own_receiver(),
            ..WriteOwnerRule::default()
        }),
        name: format!("{qualname}.{attr} writers"),
        full_name: format!("{target} writers"),
        seen: Seen {
            functions: writers.iter().map(|w| w.function.fqn()).collect(),
            modules,
            audience: audience.iter().map(|m| (*m).to_owned()).collect(),
            in_place,
            unseen: BTreeSet::new(),
        },
        attribute: Some((attribute.class.to_owned(), attr.to_owned(), Vec::new())),
        members: Vec::new(),
        rank: (!in_place, Reverse(audience.len()), Reverse(0)),
        shown: target,
    })
}

/// A `read-owners` candidate for the private `attribute`, `family` the uses of its name by the
/// class and its project subclasses: read only by functions of its class's module (methods
/// through their own receiver may be anywhere), the class used in at least
/// [`MIN_CLASS_MODULES`] modules.
fn read_owner(
    (decls, module, qualname): ClassSite<'_>,
    attribute: &AttributeUse<'_>,
    family: &[&AttributeUse<'_>],
    class_modules: &FxHashMap<&str, &[&str]>,
    tests: &FxHashSet<&Path>,
) -> Option<Candidate> {
    let readers: Vec<&Accesses<'_>> = family.iter().flat_map(|a| &a.readers).collect();
    if readers.is_empty() {
        return None;
    }
    let owners = owners_of(&readers, module, tests)?;
    if owners.module.as_ref().map(|(m, _)| *m) != Some(module) {
        return None;
    }
    let used_in = class_modules.get(attribute.class).copied().unwrap_or(&[]);
    let audience: Vec<&str> = used_in.iter().copied().filter(|m| *m != module).collect();
    if used_in.len() < MIN_CLASS_MODULES || audience.is_empty() {
        return None;
    }
    let attr = attribute.attr;
    let target = format!("{module}:{qualname}.{attr}");
    Some(Candidate {
        entry: Entry::Read(ReadOwnerRule {
            targets: vec![target.clone()],
            allowed_readers: owners.allowed(decls),
            own_receiver: owners.own_receiver(),
            ..ReadOwnerRule::default()
        }),
        name: format!("{qualname}.{attr} readers"),
        full_name: format!("{target} readers"),
        seen: Seen {
            functions: readers.iter().map(|r| r.function.fqn()).collect(),
            modules: readers.iter().map(|r| r.function.module.clone()).collect(),
            audience: audience.iter().map(|m| (*m).to_owned()).collect(),
            in_place: false,
            unseen: BTreeSet::new(),
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
            let patterns = scope_patterns(scope, &members, holding_tests.contains(scope));
            let functions = defined_functions(&members);
            Candidate {
                entry: Entry::Boundary(EffectBoundaryRule {
                    scopes: patterns,
                    forbid: vec!["blocking".to_owned()],
                    ..EffectBoundaryRule::default()
                }),
                name: format!("{scope} without blocking calls"),
                full_name: format!("{scope} without blocking calls"),
                seen: Seen {
                    functions: functions.iter().map(|f| f.fqn()).collect(),
                    modules: members.iter().map(|m| m.module.to_owned()).collect(),
                    audience: callers.iter().map(|c| c.module.clone()).collect(),
                    in_place: false,
                    unseen: (members.iter())
                        .flat_map(|m| &m.unseen)
                        .map(|f| f.fqn())
                        .collect(),
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

/// A candidate the contract trial kept, with the reports the trial made for it (and whether an
/// inline suppression dropped each).
#[derive(Debug, Clone)]
struct Kept<'d> {
    candidate: Candidate,
    confidence: Confidence,
    reports: Vec<(&'d Diagnostic, bool)>,
    trial: Trial,
}

/// Kept candidates of one kind whose entries differ only in their subjects (targets or scopes)
/// and names, as one entry.
#[derive(Debug, Clone)]
struct Block {
    entry: Entry,
    /// What the text calls each candidate, sorted.
    shown: Vec<String>,
    seen: Seen,
    trial: Trial,
    confidence: Confidence,
    caveats: Vec<String>,
    /// An ownership block's targets by the class they are of (fully qualified), each class
    /// with a pattern to try where the census sees no access of its family the block would not
    /// allow.
    classes: BTreeMap<String, Vec<String>>,
    patterns: Vec<Pattern>,
    /// The order within the kind: the least first.
    rank: (bool, Reverse<usize>, Reverse<usize>),
}

/// A class pattern tried in place of the targets of the class a block lists.
#[derive(Debug, Clone)]
struct Pattern {
    /// The class, fully qualified.
    class: String,
    text: String,
    replaces: Vec<String>,
    /// The qualified names of the class and its project subclasses.
    family: Vec<String>,
    kept: bool,
    trial: Trial,
}

impl Pattern {
    /// Whether the pattern of a `kind` block names the attribute `member` (`Class.attr`, as a
    /// report of a `__dict__` key no target names gives it).
    fn names_member(&self, kind: ContractKind, member: &str) -> bool {
        self.family.iter().any(|class| {
            (member.strip_prefix(class.as_str()))
                .and_then(|rest| rest.strip_prefix('.'))
                .is_some_and(|attr| {
                    !attr.contains('.')
                        && (kind != ContractKind::ReadOwners || attr.starts_with('_'))
                })
        })
    }
}

/// Section 3: the blocks the report shows, the candidates the trial rejected, and what was not
/// tried or not shown.
#[derive(Debug, Default)]
pub(super) struct Section {
    /// By kind, then by rank within it.
    blocks: Vec<Block>,
    pub rejected: Vec<RejectedContract>,
    pub not_tried: Vec<NotTried>,
    pub not_shown: Vec<NotShown>,
    pub blockers: ContractBlockers,
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

/// The `scopes` of a boundary around `members`, the modules at or below `scope`. A package's
/// scope names its own module, which a module part names without its submodules, also when it
/// has no `__init__.py` yet: code added there later is inside the boundary. Below it, the
/// package's every module, or, when test code lives below the package, each member by name.
fn scope_patterns(scope: &str, members: &[&ModuleUse<'_>], holding_tests: bool) -> Vec<String> {
    let mut patterns = vec![format!("{scope}:*")];
    let below = members.iter().filter(|m| m.module != scope);
    if members.len() > 1 || members[0].module != scope {
        if holding_tests {
            patterns.extend(below.map(|m| format!("{}:*", m.module)));
        } else {
            patterns.push(format!("{scope}.*:*"));
        }
    }
    patterns
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

/// Why a candidate or a block of `kind` may not see all it is about, each a reason it is
/// medium: `unresolved` accesses on objects of unknown class its trial reported, `unseen` scope
/// functions reaching code heki does not see, and the Python files below the project root left
/// out, and how many heki could not read or parse (`unanalyzed`).
fn doubts(
    kind: ContractKind,
    unresolved: usize,
    unseen: usize,
    (left_out, unparsed): (&LeftOut, usize),
) -> Vec<String> {
    let mut doubts = Vec::new();
    if unresolved > 0 {
        doubts.push(format!(
            "{} reports {unresolved} access(es) on objects of unknown class: heki cannot tell \
             whether they are of a target, so the entry sets unresolved = \"ignore\"",
            kind.strict_rule().unwrap_or_default()
        ));
    }
    if unseen > 0 {
        doubts.push(format!(
            "{unseen} function(s) of the scopes reach code heki does not see"
        ));
    }
    if left_out.python_files > 0 {
        doubts.push(
            "Python files below the project root were not analyzed: their accesses are not seen"
                .to_owned(),
        );
    } else if left_out.truncated {
        doubts.push(
            "the count of the Python files below the project root stopped before the end: files \
             it did not reach may be left out, and their accesses are not seen"
                .to_owned(),
        );
    }
    if unparsed > 0 {
        doubts.push(format!(
            "{unparsed} Python file(s) heki could not read or parse: their accesses are not seen"
        ));
    }
    doubts
}

/// Whether `trial`, what a run reported for `entry` as the block would declare it, rejects it: a
/// report under the contract's rule, or under the rule of the accesses it cannot tie to a class
/// while the entry reports them.
fn rejects(entry: &Entry, trial: &Trial) -> bool {
    let kind = entry.kind();
    trial.reports_of(Some(kind.rule())) > 0
        || entry.reports_unresolved() && trial.reports_of(kind.strict_rule()) > 0
}

/// Judge every candidate of `generated` by what the contract trial, a run of `cfg` with them
/// all declared ([`trial_config`]), reported (`kept` after inline suppressions, `suppressed`
/// dropped by them), and merge the kept ones into blocks. `configured_now`: the reports of
/// `cfg`'s own contracts on the code as it is ([`configured_reports`]). `unanalyzed`: the Python
/// files below the project root left out, and how many heki could not read or parse. `all`:
/// show every block, not the first [`MAX_BLOCKS_SHOWN`] of each kind.
pub(super) fn judge<'d>(
    generated: Generated,
    cfg: &HekiConfig,
    (kept, suppressed): (&'d [Diagnostic], &'d [Diagnostic]),
    configured_now: &'d [Diagnostic],
    unanalyzed: (&LeftOut, usize),
    shown: &dyn Fn(&Path) -> String,
    all: bool,
) -> Section {
    let Generated {
        candidates,
        not_tried,
        ignored,
        strict_ignored,
        classes,
    } = generated;
    let entries: Vec<&Entry> = candidates.iter().map(|c| &c.entry).collect();
    let mut reports = attribute_reports(cfg, &entries, kept, suppressed);
    // A target lists its `__dict__` key for every contract (`unlisted-dict-keys`), on its class
    // and the subclasses, so a candidate can hide a report the configuration makes now: that
    // report counts as its own.
    for d in hidden_reports(cfg, kept, configured_now) {
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
    let mut kept_ones = Vec::new();
    for (candidate, reports) in candidates.into_iter().zip(reports) {
        let kind = candidate.entry.kind();
        let mut candidate = Kept {
            trial: tally(kind, &reports, shown),
            reports,
            candidate,
            confidence: Confidence::High,
        };
        candidate.weigh(unanalyzed, strict_ignored.contains(&kind));
        if rejects(&candidate.candidate.entry, &candidate.trial) {
            section.rejected.push(RejectedContract {
                kind,
                target: candidate.candidate.shown,
                entry: candidate.candidate.entry.keys(),
                run: RunLabel::ContractTrial,
                trial: candidate.trial,
            });
        } else {
            kept_ones.push(candidate);
        }
    }
    leave_out_covered(&mut kept_ones);
    let (blocks, blockers) = merge(kept_ones, unanalyzed, shown);
    section.blockers = blockers.shown(all);
    let mut shown_of_kind: BTreeMap<ContractKind, usize> = BTreeMap::new();
    for mut block in blocks {
        let kind = block.entry.kind();
        let shown = shown_of_kind.entry(kind).or_default();
        if all || *shown < MAX_BLOCKS_SHOWN {
            *shown += 1;
            block.propose_patterns(&classes);
            section.blocks.push(block);
            continue;
        }
        // The blocks are by kind, so the kind's count is the last one.
        if section.not_shown.last().is_none_or(|n| n.kind != kind) {
            section.not_shown.push(NotShown {
                kind,
                blocks: 0,
                candidates: 0,
            });
        }
        let not_shown = section.not_shown.last_mut().expect("the kind's count");
        not_shown.blocks += 1;
        not_shown.candidates += block.shown.len();
    }
    name_blocks(&mut section.blocks, cfg);
    section
}

/// The reports of `cfg`'s own contracts on the code as it is (`configured_now`) that a run with
/// more contracts declared no longer makes (`kept`): the new ones hide them.
fn hidden_reports<'d>(
    cfg: &HekiConfig,
    kept: &[Diagnostic],
    configured_now: &'d [Diagnostic],
) -> Vec<&'d Diagnostic> {
    let still: FxHashSet<ReportKey<'_>> = (kept.iter())
        .filter(|d| of_configured(cfg, d))
        .map(ReportKey::of)
        .collect();
    (configured_now.iter())
        .filter(|d| !still.contains(&ReportKey::of(d)))
        .collect()
}

/// Leave out an ownership candidate a kept candidate of a base class covers: a target of a class
/// covers its subclasses.
fn leave_out_covered(kept: &mut Vec<Kept<'_>>) {
    let covering: FxHashSet<(ContractKind, String, String)> = (kept.iter())
        .filter_map(|k| {
            let (class, attr, _) = k.candidate.attribute.as_ref()?;
            Some((k.candidate.entry.kind(), class.clone(), attr.clone()))
        })
        .collect();
    kept.retain(|k| {
        let kind = k.candidate.entry.kind();
        !(k.candidate.attribute.as_ref()).is_some_and(|(_, attr, ancestors)| {
            (ancestors.iter()).any(|base| covering.contains(&(kind, base.clone(), attr.clone())))
        })
    });
}

/// The sites keeping candidates at `unresolved = "ignore"`, while they are gathered: per report,
/// the candidates its trial attributed it to and the blocks they are in.
#[derive(Default)]
struct Blockers<'d> {
    by_report: FxHashMap<*const Diagnostic, (&'d Diagnostic, usize, BTreeSet<usize>)>,
    sorted: Vec<(String, u32, &'d Diagnostic, usize, usize)>,
}

impl<'d> Blockers<'d> {
    fn add(&mut self, d: &'d Diagnostic, block: usize) {
        let (_, candidates, blocks) = (self.by_report)
            .entry(std::ptr::from_ref(d))
            .or_insert_with(|| (d, 0, BTreeSet::new()));
        *candidates += 1;
        blocks.insert(block);
    }

    /// Sorted by the candidates each holds back, then by path and line.
    fn sort(&mut self, shown: &dyn Fn(&Path) -> String) {
        self.sorted = (self.by_report.values())
            .map(|(d, candidates, blocks)| (shown(&d.path), d.line, *d, *candidates, blocks.len()))
            .collect();
        self.sorted.sort_by(|a, b| {
            (Reverse(a.3), &a.0, a.1, &a.2.rule_code, a.2.column).cmp(&(
                Reverse(b.3),
                &b.0,
                b.1,
                &b.2.rule_code,
                b.2.column,
            ))
        });
    }

    /// The first [`MAX_BLOCKERS_SHOWN`], or with `all` up to [`MAX_LISTED`].
    fn shown(self, all: bool) -> ContractBlockers {
        let limit = if all { MAX_LISTED } else { MAX_BLOCKERS_SHOWN };
        let count = self.sorted.len();
        ContractBlockers {
            count,
            truncated: count > limit,
            items: (self.sorted.into_iter().take(limit))
                .map(|(path, line, d, candidates, blocks)| ContractBlocker {
                    rule: d.rule_code.clone(),
                    path,
                    line,
                    function: d.qualname.clone(),
                    message: d.message.clone(),
                    candidates,
                    blocks,
                })
                .collect(),
        }
    }
}

/// The kept candidates as blocks, ranked within each kind, and the sites of the accesses on
/// objects of unknown class their trials reported.
fn merge<'d>(
    kept: Vec<Kept<'d>>,
    unanalyzed: (&LeftOut, usize),
    shown: &dyn Fn(&Path) -> String,
) -> (Vec<Block>, Blockers<'d>) {
    let mut groups: BTreeMap<(ContractKind, String), Vec<Kept<'d>>> = BTreeMap::new();
    for k in kept {
        let key = (k.candidate.entry.kind(), k.candidate.entry.merge_key());
        groups.entry(key).or_default().push(k);
    }
    let mut blocks = Vec::new();
    let mut blockers = Blockers::default();
    for ((kind, _), mut members) in groups {
        members.sort_by(|a, b| a.candidate.shown.cmp(&b.candidate.shown));
        let index = blocks.len();
        for member in &members {
            for &(d, dropped) in &member.reports {
                if !dropped && Some(d.rule_code.as_str()) == kind.strict_rule() {
                    blockers.add(d, index);
                }
            }
        }
        blocks.push(Block::of(kind, &members, unanalyzed, shown));
    }
    blockers.sort(shown);
    blocks.sort_by(|a, b| {
        (a.entry.kind(), a.rank, &a.shown).cmp(&(b.entry.kind(), b.rank, &b.shown))
    });
    (blocks, blockers)
}

/// Give each block a name no contract of its kind in `cfg` or another block has
/// ([`Block::names`]).
fn name_blocks(blocks: &mut [Block], cfg: &HekiConfig) {
    let mut taken: FxHashMap<ContractKind, FxHashSet<String>> = FxHashMap::default();
    for block in blocks {
        let kind = block.entry.kind();
        let taken = taken
            .entry(kind)
            .or_insert_with(|| kind.names_in(cfg).into_iter().map(str::to_owned).collect());
        let (name, full_name) = block.names();
        let name = unique_name(taken, &name, &full_name);
        block.entry.set_name(name);
    }
}

impl Kept<'_> {
    /// The confidence of a candidate the trial kept, and its entry's `unresolved`: `"report"`
    /// only when the confidence is high and the configuration does not ignore the rule of such
    /// accesses (`strict_ignored`). A medium one may leave accesses of unknown class unseen (in
    /// the files left out or not parsed), which that rule would report once `heki check` sees
    /// them.
    fn weigh(&mut self, unanalyzed: (&LeftOut, usize), strict_ignored: bool) {
        let kind = self.candidate.entry.kind();
        let unresolved = self.trial.reports_of(kind.strict_rule());
        let unseen = self.candidate.seen.unseen.len();
        if !doubts(kind, unresolved, unseen, unanalyzed).is_empty() {
            self.confidence = Confidence::Medium;
        }
        self.candidate
            .entry
            .set_unresolved(self.confidence == Confidence::High && !strict_ignored);
    }
}

impl Block {
    /// The block of `members`, kept candidates of `kind` with the same entry but its subjects and
    /// name, sorted by what the text calls them.
    fn of(
        kind: ContractKind,
        members: &[Kept<'_>],
        unanalyzed: (&LeftOut, usize),
        shown: &dyn Fn(&Path) -> String,
    ) -> Self {
        let mut entry = members[0].candidate.entry.clone();
        let mut subjects: Vec<String> = Vec::new();
        let mut seen = Seen::default();
        let mut classes: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut reports: Vec<(&Diagnostic, bool)> = Vec::new();
        let mut counted: FxHashSet<*const Diagnostic> = FxHashSet::default();
        for member in members {
            let candidate = &member.candidate;
            for subject in candidate.entry.subjects() {
                if !subjects.contains(subject) {
                    subjects.push(subject.clone());
                }
            }
            seen.add(&candidate.seen);
            if let Some((class, _, _)) = &candidate.attribute {
                (classes.entry(class.clone()).or_default()).push(candidate.shown.clone());
            }
            for &(d, dropped) in &member.reports {
                if counted.insert(std::ptr::from_ref(d)) {
                    reports.push((d, dropped));
                }
            }
        }
        if kind != ContractKind::EffectBoundaries {
            subjects.sort();
        }
        entry.set_subjects(subjects);
        // A report of one access stands for every contract finding it: the block reports it
        // once, and the examples follow the order `heki check` prints them in.
        reports.sort_by(|(a, _), (b, _)| {
            (&a.path, a.line, a.column).cmp(&(&b.path, b.line, b.column))
        });
        let trial = tally(kind, &reports, shown);
        let modules = seen.modules.clone();
        seen.audience.retain(|m| !modules.contains(m));
        let unresolved = trial.reports_of(kind.strict_rule());
        let mut caveats = doubts(kind, unresolved, seen.unseen.len(), unanalyzed);
        let confidence =
            if caveats.is_empty() && members.iter().all(|m| m.confidence == Confidence::High) {
                Confidence::High
            } else {
                Confidence::Medium
            };
        caveats.push(UNSEEN_CAVEAT.to_owned());
        let size = match kind {
            ContractKind::EffectBoundaries => seen.functions.len(),
            _ => members.len(),
        };
        Self {
            rank: (
                !seen.in_place,
                Reverse(seen.audience.len() * size),
                Reverse(size),
            ),
            entry,
            shown: members.iter().map(|m| m.candidate.shown.clone()).collect(),
            seen,
            trial,
            confidence,
            caveats,
            classes,
            patterns: Vec::new(),
        }
    }

    /// The block's name, and the name to fall back on when the configuration has a contract of
    /// that name: after its one target or scope, else after what accesses its targets or where
    /// its scopes are.
    fn names(&self) -> (String, String) {
        let one = |subject: &str, what: &str| {
            let local = subject.split_once(':').map_or(subject, |(_, q)| q);
            (format!("{local} {what}"), format!("{subject} {what}"))
        };
        let merged = match &self.entry {
            Entry::Write(e) if e.targets.len() == 1 => return one(&e.targets[0], "writers"),
            Entry::Read(e) if e.targets.len() == 1 => return one(&e.targets[0], "readers"),
            Entry::Boundary(_) if self.shown.len() == 1 => {
                let name = format!("{} without blocking calls", self.shown[0]);
                return (name.clone(), name);
            }
            Entry::Write(e) => owned_by("written", &e.allowed_writers, e.own_receiver.as_deref()),
            Entry::Read(e) => owned_by("read", &e.allowed_readers, e.own_receiver.as_deref()),
            Entry::Boundary(_) => match common_package(&self.shown) {
                Some(package) => format!("scopes of {package} without blocking calls"),
                None => "scopes without blocking calls".to_owned(),
            },
        };
        (merged.clone(), merged)
    }

    /// Propose a pattern for each class of an ownership block whose family, as the census sees
    /// it, no function accesses that the block does not allow: `classes` holds what each
    /// pattern would name.
    fn propose_patterns(&mut self, classes: &FxHashMap<String, ClassFacts>) {
        let kind = self.entry.kind();
        let Some(allowance) = self.entry.allowance() else {
            return;
        };
        for (class, targets) in &self.classes {
            // Generation describes the class of every candidate it tries.
            let facts = &classes[class];
            if facts.accesses(kind).iter().all(|a| allowance.allows(a)) {
                self.patterns.push(Pattern {
                    class: class.clone(),
                    text: facts.pattern(kind),
                    replaces: targets.clone(),
                    family: facts.family.clone(),
                    kept: false,
                    trial: Trial::of(kind),
                });
            }
        }
    }

    /// The entry with the kept patterns in place of the targets they stand for. A block without
    /// patterns, an effect boundary among them, keeps its subjects: they are no classes.
    fn apply_patterns(&mut self) {
        if self.patterns.is_empty() {
            return;
        }
        let mut subjects: Vec<String> = Vec::new();
        for (class, targets) in &self.classes {
            match self.patterns.iter().find(|p| p.kept && &p.class == class) {
                Some(pattern) => subjects.push(pattern.text.clone()),
                None => subjects.extend(targets.iter().cloned()),
            }
        }
        subjects.sort();
        self.entry.set_subjects(subjects);
    }

    /// What the block is about, as the report lists it: an ownership block's targets, a
    /// boundary's packages and modules.
    fn targets(&self) -> Vec<String> {
        match self.entry.kind() {
            ContractKind::EffectBoundaries => self.shown.clone(),
            _ => self.entry.subjects().to_vec(),
        }
    }
}

/// The name of a merged ownership block: what accesses its targets besides construction.
fn owned_by(access: &str, allowed: &[String], own_receiver: Option<&str>) -> String {
    let mut name = if allowed.is_empty() {
        format!("attributes {access} only while constructed")
    } else {
        format!("attributes {access} by {}", allowed.join(", "))
    };
    if own_receiver == Some("allow") {
        name.push_str(" and through their own receiver");
    }
    name
}

/// The longest dotted prefix `names` share, the whole of a name included.
fn common_package(names: &[String]) -> Option<String> {
    let mut common: Vec<&str> = names.first()?.split('.').collect();
    for name in &names[1..] {
        let shared = (common.iter().zip(name.split('.')))
            .take_while(|(a, b)| **a == *b)
            .count();
        common.truncate(shared);
    }
    (!common.is_empty()).then(|| common.join("."))
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

    /// Each pattern to try, by its block and its place there, with the entry the pattern trial
    /// declares it in: the block's, naming the pattern alone, and named as a block of that one
    /// target would be.
    fn pattern_entries(&self) -> Vec<((usize, usize), Entry)> {
        let mut out = Vec::new();
        for (b, block) in self.blocks.iter().enumerate() {
            for (p, pattern) in block.patterns.iter().enumerate() {
                let mut alone = block.clone();
                alone.entry.set_subjects(vec![pattern.text.clone()]);
                let (name, _) = alone.names();
                alone.entry.set_name(name);
                out.push(((b, p), alone.entry));
            }
        }
        out
    }

    /// `cfg` with every pattern to try declared alone after its own contracts, with
    /// `unresolved = "report"`: the configuration of the pattern trial, or `None` without a
    /// pattern to try.
    pub(super) fn pattern_config(&self, cfg: &HekiConfig) -> Option<HekiConfig> {
        let entries = self.pattern_entries();
        if entries.is_empty() {
            return None;
        }
        let mut trial = cfg.clone();
        for (_, mut entry) in entries {
            entry.set_unresolved(true);
            entry.declare_in(&mut trial);
        }
        Some(trial)
    }

    /// The rules the pattern trial runs: those of the kinds it declares patterns of.
    pub(super) fn pattern_rules(&self) -> Vec<&'static str> {
        let kinds: BTreeSet<ContractKind> = (self.pattern_entries().iter())
            .map(|(_, e)| e.kind())
            .collect();
        kinds
            .into_iter()
            .flat_map(|k| std::iter::once(k.rule()).chain(k.strict_rule()))
            .collect()
    }

    /// Keep each pattern the pattern trial, a run of `cfg` with them declared
    /// ([`Self::pattern_config`]), reported nothing for that the targets it stands for did not:
    /// no report under the contract's rule, none hiding a report of `cfg`'s own contracts
    /// (`configured_now`), and none of the accesses on objects of unknown class when the block
    /// reports them.
    pub(super) fn judge_patterns(
        &mut self,
        cfg: &HekiConfig,
        (kept, suppressed): (&[Diagnostic], &[Diagnostic]),
        configured_now: &[Diagnostic],
        shown: &dyn Fn(&Path) -> String,
    ) {
        let tried = self.pattern_entries();
        let entries: Vec<&Entry> = tried.iter().map(|(_, e)| e).collect();
        let mut reports = attribute_reports(cfg, &entries, kept, suppressed);
        for d in hidden_reports(cfg, kept, configured_now) {
            for (((b, p), entry), reports) in tried.iter().zip(&mut reports) {
                let kind = entry.kind();
                if ContractKind::of_rule(&d.rule_code) == Some(kind)
                    && self.blocks[*b].patterns[*p].names_member(kind, &d.target)
                {
                    reports.push((d, false));
                }
            }
        }
        for (((b, p), _), reports) in tried.iter().zip(reports) {
            let block = &mut self.blocks[*b];
            let trial = tally(block.entry.kind(), &reports, shown);
            let pattern = &mut block.patterns[*p];
            pattern.kept = !rejects(&block.entry, &trial);
            pattern.trial = trial;
        }
        for block in &mut self.blocks {
            block.apply_patterns();
        }
        name_blocks(&mut self.blocks, cfg);
    }

    /// The entries the block declares, in its order.
    fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.blocks.iter().map(|b| &b.entry)
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

    /// Whether the report shows a block.
    pub(super) fn has_candidates(&self) -> bool {
        !self.blocks.is_empty()
    }

    /// The blocks the report shows.
    pub(super) fn advice(&self) -> Vec<ContractAdvice> {
        (self.blocks.iter())
            .map(|b| ContractAdvice {
                kind: b.entry.kind(),
                targets: b.targets(),
                candidates: b.shown.len(),
                entry: b.entry.keys(),
                confidence: b.confidence,
                evidence: b.seen.evidence(),
                trial: b.trial.clone(),
                patterns: (b.patterns.iter())
                    .map(|p| PatternAdvice {
                        pattern: p.text.clone(),
                        replaces: p.replaces.clone(),
                        kept: p.kept,
                        trial: p.trial.clone(),
                    })
                    .collect(),
                caveats: b.caveats.clone(),
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

/// `items` as a line lists them: the first [`MAX_EXAMPLES`], then how many there are.
fn first_of(items: &[String]) -> String {
    if items.len() <= MAX_EXAMPLES {
        return items.join(", ");
    }
    format!(
        "{}, ... ({} in all)",
        items[..MAX_EXAMPLES].join(", "),
        items.len()
    )
}

/// The section as the text prints it, from the report's lists.
pub(super) fn render(
    out: &mut String,
    contracts: &[ContractAdvice],
    (rejected, not_tried, not_shown): (&[RejectedContract], &[NotTried], &[NotShown]),
    blockers: &ContractBlockers,
) {
    out.push_str("\nContract candidates\n");
    if contracts.is_empty() && rejected.is_empty() {
        out.push_str("  None here.\n");
    }
    for c in contracts {
        render_block(out, c);
    }
    if !blockers.items.is_empty() {
        out.push_str(
            "  Accesses heki cannot tie to a class, each keeping the blocks it reaches at \
             unresolved = \"ignore\":\n",
        );
        for b in &blockers.items {
            let _ = writeln!(
                out,
                "    {}:{}  {}  {}: {} candidate(s) in {} block(s)",
                b.path, b.line, b.rule, b.function, b.candidates, b.blocks
            );
        }
        if blockers.truncated {
            let _ = writeln!(
                out,
                "    and {} more site(s)",
                blockers.count - blockers.items.len()
            );
        }
        out.push_str(
            "    An annotation naming the object's class resolves such a site: heki then judges \
             the access by that class.\n",
        );
    }
    for r in rejected {
        let first = first_rejecting(r.kind, &r.entry, &r.trial);
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
    for n in not_shown {
        let _ = writeln!(
            out,
            "  Not shown: {} more {} block(s) of {} candidate(s); heki recommend --all shows \
             every block",
            n.blocks,
            n.kind.key(),
            n.candidates
        );
    }
    if !contracts.is_empty() {
        let _ = writeln!(out, "  For every block, {UNSEEN_CAVEAT}.");
    }
}

/// The first report of `trial` that rejects an `entry` of `kind`, as `: RULE at path:line`: one
/// under the contract's rule, or under the rule of the accesses it cannot tie to a class when the
/// entry reports them.
fn first_rejecting(
    kind: ContractKind,
    entry: &serde_json::Map<String, serde_json::Value>,
    trial: &Trial,
) -> String {
    let reports_unresolved = entry.get("unresolved").and_then(|v| v.as_str()) == Some("report");
    (trial.examples.iter())
        .find(|e| e.rule == kind.rule() || reports_unresolved)
        .map(|e| format!(": {} at {}:{}", e.rule, e.path, e.line))
        .unwrap_or_default()
}

/// One block as the text prints it.
fn render_block(out: &mut String, c: &ContractAdvice) {
    let confidence = match c.confidence {
        Confidence::High => "high",
        Confidence::Medium => "medium",
    };
    let _ = writeln!(
        out,
        "  {}  {}  {confidence}",
        c.kind.key(),
        first_of(&c.targets)
    );
    let _ = writeln!(out, "    {}", evidence_line(c));
    for p in &c.patterns {
        let what = match c.kind {
            ContractKind::ReadOwners => "private attribute",
            _ => "attribute",
        };
        if p.kept {
            let _ = writeln!(
                out,
                "    {} names every {what} of the class and its subclasses, also those added \
                 later, in place of {}",
                p.pattern,
                first_of(&p.replaces)
            );
        } else {
            let first = first_rejecting(c.kind, &c.entry, &p.trial);
            let _ = writeln!(
                out,
                "    {} was tried in place of {} ({}){first}, so the block lists the targets",
                p.pattern,
                first_of(&p.replaces),
                counts(&p.trial)
            );
        }
    }
    let mut trial = format!("trial: {}", counts(&c.trial));
    if let Some(strict) = c.kind.strict_rule() {
        let unresolved = c.trial.reports_of(Some(strict));
        if unresolved > 0 {
            // The sites are listed once, after the blocks.
            trial.push_str(" (sites below), so unresolved = \"ignore\"");
        } else if c.entry.get("unresolved").and_then(|v| v.as_str()) == Some("report") {
            trial.push_str(" (unresolved = \"report\")");
        } else if c.confidence == Confidence::Medium {
            let _ = write!(
                trial,
                " (unresolved = \"ignore\", as for every medium candidate)"
            );
        } else {
            let _ = write!(
                trial,
                " (unresolved = \"ignore\": the configuration ignores {strict})"
            );
        }
    }
    let _ = writeln!(out, "    {trial}");
}

/// What a block rests on, as one line of the text.
fn evidence_line(c: &ContractAdvice) -> String {
    let e = &c.evidence;
    let own_receiver = c.entry.get("own-receiver").and_then(|v| v.as_str()) == Some("allow");
    let allowed = |key: &str| -> Option<String> {
        let first = c.entry.get(key)?.as_array()?.first()?.as_str()?;
        Some(first.split_once(':').map_or(first, |(m, _)| m).to_owned())
    };
    // The functions, named within the owner's module when they all are there.
    let module = (e.modules.count == 1)
        .then(|| e.modules.items.first().map(String::as_str))
        .flatten();
    let named: Vec<String> = (e.functions.items.iter())
        .map(|f| {
            module
                .and_then(|m| f.strip_prefix(m))
                .and_then(|q| q.strip_prefix('.'))
                .unwrap_or(f)
                .to_owned()
        })
        .collect();
    let named = examples(&Listed {
        count: e.functions.count,
        items: named,
        truncated: e.functions.truncated,
    });
    // Where the functions are: one module, or the owner's and methods through their own
    // receiver elsewhere.
    let place = |key: &str| match (module, allowed(key)) {
        (Some(m), _) if !own_receiver => format!("all in {m}"),
        (_, Some(owner)) => format!("outside {owner} only through their own receiver"),
        (_, None) => "only through their own receiver".to_owned(),
    };
    let objects = if c.candidates == 1 { "its" } else { "their" };
    let class = if c.candidates == 1 {
        "the class is"
    } else {
        "their classes are"
    };
    match c.kind {
        ContractKind::WriteOwners if e.functions.count == 0 => format!(
            "written only while {objects} objects are constructed; read in {} other module(s)",
            e.audience.count
        ),
        ContractKind::WriteOwners => format!(
            "written by {} function(s), {} ({named}){}; read in {} other module(s)",
            e.functions.count,
            place("allowed-writers"),
            if e.in_place { ", changed in place" } else { "" },
            e.audience.count
        ),
        ContractKind::ReadOwners => format!(
            "read by {} function(s), {} ({named}); {class} used in {} other module(s)",
            e.functions.count,
            place("allowed-readers"),
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
