//! `heki recommend`'s suggestions (ADR-023): the standard types the code already keeps. A
//! suggestion is a claim with premises, as an exact report is, and the annotation edits that
//! write the claim; the edits that are valid only together form one group, applied whole or not
//! at all. It is no report: no rule code, severity, suppression, baseline entry, or exit code,
//! and `heki check` never prints one.
//!
//! The kind `readonly-parameter`: a parameter annotated with a builtin `list`, `dict`, or `set`
//! that the function, and every function it hands it to, only reads, annotated `Sequence`,
//! `Mapping`, or `AbstractSet` instead. Its premises are
//! [`crate::premises::READONLY_PARAMETER`]: when the claim (the first three) is open nothing is
//! listed; when it holds and a premise of the edit is open the group is heuristic, listed with
//! `--all`; a group whose edits change heki's own reports or facts is withheld (the annotation
//! trial).
//!
//! [`suggest`] finds every group from the index, the declarations, and the files' sources (for
//! the edits and the trial); [`Section`] is what the JSON object shows of them.

use crate::premises::{Premise, READONLY_PARAMETER};
use crate::project::{Indexed, parse_source};
use crate::recommend::{MAX_LISTED, RecommendError};
use crate::selection::RuleSelection;
use heki_ast::{LineIndex, Span};
use heki_diagnostics::Diagnostic;
use heki_effect::readonly::ContainerKind;
use heki_effect::{
    AnnotationFacts, AnnotationReads, CallSite, DeclPasses, EffectSummary, FuncKey,
    LiteralAnnotation, ModuleDecls, ParamAnnotation, ProjectDecls,
};
use heki_ir::InstKind;
use heki_soundness::imports::{NewImport, import_edit};
use rustc_hash::{FxHashMap, FxHashSet};
use schemars::JsonSchema;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The kind of [`suggest`]'s groups.
pub const READONLY_PARAMETER_KIND: &str = "readonly-parameter";

/// The methods whose overrides the type checkers do not compare with the base's.
const CONSTRUCTORS: &[&str] = &["__init__", "__new__", "__init_subclass__", "__post_init__"];

/// The configuration of a type checker in a project: the table of a `pyproject.toml`, or its
/// own file (assumption T3).
const TYPE_CHECKERS: &[(&str, &str, &str)] = &[
    ("ty", "pyproject.toml", "[tool.ty"),
    ("ty", "ty.toml", ""),
    ("mypy", "pyproject.toml", "[tool.mypy"),
    ("mypy", "mypy.ini", ""),
    ("mypy", ".mypy.ini", ""),
    ("mypy", "setup.cfg", "[mypy"),
    ("pyright", "pyproject.toml", "[tool.pyright"),
    ("pyright", "pyrightconfig.json", ""),
    ("pyrefly", "pyproject.toml", "[tool.pyrefly"),
    ("pyrefly", "pyrefly.toml", ""),
];

/// Whether a suggestion's claim and edit are established.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SuggestionCertainty {
    /// Every premise is established.
    Exact,
    /// The claim is established; a premise of the edit is open, or a label holds it back.
    Heuristic,
}

/// The `suggestions` object: the standard types the code already keeps.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Section {
    /// The groups per kind, after the annotation trial.
    pub counts: Vec<KindCount>,
    pub type_checker: TypeChecker,
    /// The exact groups by rank, the first 50 (`--all`: every one, and the heuristic groups
    /// after them).
    pub groups: Vec<Group>,
    /// Whether `groups` holds fewer than there are.
    pub truncated: bool,
    /// The groups the annotation trial withheld, by what changed.
    pub withheld: Vec<Withheld>,
    /// The parameters no group holds, by why.
    pub left_out: LeftOut,
    /// Per premise, the parameters it leaves out and those it keeps heuristic.
    pub by_premise: Vec<PremiseCount>,
    /// Where the project reads annotations at run time (`get_type_hints`, `inspect.signature`,
    /// `__annotations__`) outside test code: a function such code finds by scanning a module or
    /// a class is not known to heki (assumption T1).
    pub reader_sites: ReaderSites,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct KindCount {
    pub kind: String,
    pub exact: usize,
    pub heuristic: usize,
}

/// The type checker heki found configured (assumption T3: half of each guard is the type
/// checker's).
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct TypeChecker {
    /// `ty`, `mypy`, `pyright`, or `pyrefly`; `null` when the project configures none.
    pub found: Option<String>,
    /// The file configuring it.
    pub config: Option<String>,
}

/// One group: edits valid only together, applied whole or not at all.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Group {
    /// Kind, module, qualified name, and target of its first item: no line number, as a
    /// baseline key.
    pub id: String,
    pub kind: String,
    pub certainty: SuggestionCertainty,
    pub message: String,
    pub items: Vec<Item>,
    /// Byte-span edits, each with its file, applied together.
    pub edits: Vec<Edit>,
    /// The premises of the claim and the edit, each established or not.
    pub premises: Vec<PremiseState>,
    /// The premises left open, each with its cause and place.
    pub open: Vec<OpenPremise>,
    pub related: Vec<Related>,
    /// What reports a later change of the type it writes.
    pub guards: Vec<Guard>,
    pub leverage: Leverage,
    /// `public` (no underscore along the dotted name), `list[str]` (a `Sequence[str]` also
    /// admits a plain `str`), `family` (an override family is in the group).
    pub labels: Vec<String>,
}

/// One annotation a group changes.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Item {
    pub path: String,
    pub line: u32,
    pub column: u32,
    /// The byte span of the annotation's head, the text the edit replaces.
    pub span: [u32; 2],
    pub qualname: String,
    /// The parameter.
    pub target: String,
    pub current: String,
    pub proposed: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Edit {
    pub path: String,
    pub span: [u32; 2],
    pub replacement: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PremiseState {
    pub name: String,
    pub established: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct OpenPremise {
    pub premise: String,
    pub cause: String,
    pub path: String,
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Related {
    pub path: String,
    pub line: u32,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Guard {
    /// `type-checker` or `heki`.
    pub by: String,
    /// The rule, for heki.
    pub rule: Option<String>,
    /// The rule's certainty, for heki.
    pub certainty: Option<String>,
    /// Whether the configuration runs the rule, for heki.
    pub runs: Option<bool>,
    pub catches: String,
}

/// What ranks a group: the calls of its functions.
#[derive(Debug, Clone, Copy, Default, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Leverage {
    /// The project calls of the group's functions binding the parameter.
    pub call_sites: usize,
    /// Those passing state that outlives the call there (an attribute).
    pub state_arguments: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Withheld {
    /// `report-would-change`, `fact-would-change`, or `edit-does-not-parse`.
    pub reason: String,
    pub groups: usize,
    /// The first 50 groups, each with what changed.
    pub first: Vec<WithheldGroup>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct WithheldGroup {
    pub id: String,
    pub changed: String,
}

/// Parameters with a literal container annotation that no group holds, without a premise of
/// their own: test code, generated files, and functions a stub declares.
#[derive(Debug, Clone, Copy, Default, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct LeftOut {
    pub test_code: usize,
    pub generated_files: usize,
    pub stubs: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PremiseCount {
    pub premise: String,
    /// The parameters whose group is not listed because of it (its first premise that is not
    /// established; for the claim's, nothing is listed).
    pub not_listed: usize,
    /// The parameters in heuristic groups that leave it open.
    pub heuristic: usize,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ReaderSites {
    pub sites: usize,
    /// The files, by sites, the first 50.
    pub files: Vec<ReaderFile>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ReaderFile {
    pub path: String,
    pub sites: usize,
}

/// Every group [`suggest`] found, ranked, with what the section counts.
#[derive(Debug, Clone, Default)]
pub struct Suggestions {
    /// The exact groups by rank, then the heuristic ones by rank.
    pub groups: Vec<Group>,
    pub withheld: Vec<Withheld>,
    pub left_out: LeftOut,
    pub by_premise: Vec<PremiseCount>,
    pub type_checker: TypeChecker,
    pub reader_sites: ReaderSites,
    /// Whether an annotation trial ran.
    pub tried: bool,
}

impl Suggestions {
    /// The JSON section: the exact groups, the first [`MAX_LISTED`] of them, or with `all` every
    /// group.
    pub fn section(&self, all: bool) -> Section {
        let exact = self
            .groups
            .iter()
            .filter(|g| g.certainty == SuggestionCertainty::Exact)
            .count();
        let heuristic = self.groups.len() - exact;
        let groups: Vec<Group> = if all {
            self.groups.clone()
        } else {
            self.groups
                .iter()
                .take(exact.min(MAX_LISTED))
                .cloned()
                .collect()
        };
        Section {
            counts: vec![KindCount {
                kind: READONLY_PARAMETER_KIND.to_owned(),
                exact,
                heuristic,
            }],
            type_checker: self.type_checker.clone(),
            truncated: groups.len() < self.groups.len(),
            groups,
            withheld: self.withheld.clone(),
            left_out: self.left_out,
            by_premise: self.by_premise.clone(),
            reader_sites: self.reader_sites.clone(),
        }
    }
}

/// A parameter annotated with a literal container that no premise left out before grouping.
#[derive(Debug, Clone)]
struct Candidate {
    key: FuncKey,
    /// The file the index has it in.
    path: PathBuf,
    param: u8,
    name: String,
    lit: LiteralAnnotation,
}

/// Why a premise is not established for a candidate.
#[derive(Debug, Clone)]
struct Unmet {
    premise: &'static Premise,
    /// Nothing is listed (the claim is open, or the premise is refuted); else the group is
    /// heuristic.
    drops: bool,
    cause: String,
    at: Option<(PathBuf, u32)>,
}

/// Inputs every step reads.
struct Cx<'a> {
    index: &'a heki_effect::ProjectIndex,
    kb: &'a heki_kb::EffectiveEcosystemKb,
    decls: ProjectDecls,
    files: FxHashMap<&'a Path, &'a crate::project::ParsedFile>,
    shown: &'a dyn Fn(&Path) -> String,
    lines: std::cell::RefCell<FxHashMap<PathBuf, LineIndex>>,
}

impl Cx<'_> {
    fn facts(&self, key: &FuncKey) -> Option<&AnnotationFacts> {
        let path = self.index.file_of(key)?;
        let tables = self.index.files.get(path)?.opt_in_tables.as_deref()?;
        tables.annotation_facts.get(&key.qualname)
    }

    fn module(&self, key: &FuncKey) -> Option<std::sync::Arc<ModuleDecls>> {
        self.index.module_decls(&key.module)
    }

    fn line_col(&self, path: &Path, offset: u32) -> (u32, u32) {
        let mut lines = self.lines.borrow_mut();
        let index = lines.entry(path.to_path_buf()).or_insert_with(|| {
            let source = self.files.get(path).map_or("", |f| f.source.as_str());
            LineIndex::from_source(source)
        });
        index.line_col(offset)
    }

    fn line(&self, path: &Path, offset: u32) -> u32 {
        self.line_col(path, offset).0
    }

    fn param_name(&self, key: &FuncKey, param: u8) -> Option<String> {
        let decls = self.module(key)?;
        let f = decls.functions.get(&key.qualname)?;
        f.param_names.get(usize::from(param)).cloned()
    }
}

/// Every `readonly-parameter` group of the project `indexed` analyzed (the discovery run: every
/// rule selected, the facts of [`crate::recommend_facts`]), ranked, after the annotation trial,
/// which analyzes the files of the groups again in the index and leaves it as it was.
/// `current` is the selection the guards' `runs` is relative to; `reports` are the discovery
/// run's reports.
pub fn suggest(
    indexed: &mut Indexed,
    current: &RuleSelection,
    reports: &[Diagnostic],
    shown: &dyn Fn(&Path) -> String,
) -> Result<Suggestions, RecommendError> {
    let mut found = {
        let cx = Cx {
            index: &indexed.index,
            kb: &indexed.project.env.kb,
            decls: indexed.index.project_decls(),
            files: (indexed.project.files.iter())
                .map(|f| (f.path.as_path(), f))
                .collect(),
            shown,
            lines: std::cell::RefCell::default(),
        };
        let mut found = groups(&cx, current);
        found.type_checker = type_checker(&indexed.project.env.root, shown);
        found.reader_sites = reader_sites(&cx);
        found
    };
    let mut by_file: FxHashMap<&Path, Vec<Diagnostic>> = FxHashMap::default();
    for d in reports {
        by_file.entry(d.path.as_path()).or_default().push(d.clone());
    }
    trial_all(indexed, &mut found, &by_file)?;
    Ok(found.finish())
}

/// The groups found before the trial, with their file edits for the trial.
struct Found {
    groups: Vec<Pending>,
    left_out: LeftOut,
    /// Per premise: (not listed, heuristic) parameters.
    by_premise: BTreeMap<usize, (usize, usize)>,
    type_checker: TypeChecker,
    reader_sites: ReaderSites,
    withheld: BTreeMap<&'static str, Vec<WithheldGroup>>,
    tried: bool,
}

/// The head edits of a file, each a span and its replacement.
type Heads = Vec<(Span, String)>;

/// A file's head edits and the import statements they need.
type FileEdits = (Heads, Vec<String>);

/// A group, and per file the edits the trial applies.
struct Pending {
    group: Group,
    /// The file, its head edits, and the import statements it needs.
    files: BTreeMap<PathBuf, FileEdits>,
    rank: (std::cmp::Reverse<usize>, std::cmp::Reverse<usize>, usize),
    withheld: Option<(&'static str, String)>,
}

impl Found {
    fn finish(self) -> Suggestions {
        let mut kept: Vec<Pending> = Vec::new();
        let mut withheld = self.withheld;
        for p in self.groups {
            match p.withheld {
                Some((reason, changed)) => {
                    withheld.entry(reason).or_default().push(WithheldGroup {
                        id: p.group.id,
                        changed,
                    })
                }
                None => kept.push(p),
            }
        }
        kept.sort_by(|a, b| {
            (
                a.group.certainty,
                a.rank,
                &a.group.items[0].path,
                a.group.items[0].line,
                &a.group.id,
            )
                .cmp(&(
                    b.group.certainty,
                    b.rank,
                    &b.group.items[0].path,
                    b.group.items[0].line,
                    &b.group.id,
                ))
        });
        let names = |i: usize| READONLY_PARAMETER[i].name.to_owned();
        Suggestions {
            groups: kept.into_iter().map(|p| p.group).collect(),
            withheld: withheld
                .into_iter()
                .map(|(reason, mut groups)| {
                    groups.sort_by(|a, b| a.id.cmp(&b.id));
                    let count = groups.len();
                    groups.truncate(MAX_LISTED);
                    Withheld {
                        reason: reason.to_owned(),
                        groups: count,
                        first: groups,
                    }
                })
                .collect(),
            left_out: self.left_out,
            by_premise: (self.by_premise.into_iter())
                .map(|(i, (not_listed, heuristic))| PremiseCount {
                    premise: names(i),
                    not_listed,
                    heuristic,
                })
                .collect(),
            type_checker: self.type_checker,
            reader_sites: self.reader_sites,
            tried: self.tried,
        }
    }
}

fn premise(name: &str) -> &'static Premise {
    READONLY_PARAMETER
        .iter()
        .find(|p| p.name == name)
        .expect("a premise of the table")
}

fn premise_index(p: &Premise) -> usize {
    READONLY_PARAMETER
        .iter()
        .position(|q| q.name == p.name)
        .expect("a premise of the table")
}

/// The candidates, their premises, and the groups they form.
fn groups(cx: &Cx<'_>, current: &RuleSelection) -> Found {
    let mut left_out = LeftOut::default();
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut paths: Vec<&PathBuf> = cx.index.files.keys().collect();
    paths.sort();
    for path in paths {
        let file = &cx.index.files[path];
        let Some(tables) = file.opt_in_tables.as_deref() else {
            continue;
        };
        if heki_effect::is_stub(path) || tables.annotation_facts.is_empty() {
            continue;
        }
        let parsed = cx.files.get(path.as_path());
        let rel = parsed.map_or("", |p| p.rel.as_str());
        let generated = parsed.is_some_and(|p| says_generated(&p.source));
        for (qualname, facts) in &tables.annotation_facts {
            for (i, p) in facts.params.iter().enumerate().take(64) {
                let ParamAnnotation::Literal(lit) = p else {
                    continue;
                };
                let key = FuncKey {
                    module: file.module_name.clone(),
                    qualname: qualname.clone(),
                };
                let param = u8::try_from(i).expect("at most 64 positions");
                if heki_effect::code_kind::is_test_code(qualname, rel) {
                    left_out.test_code += 1;
                } else if generated {
                    left_out.generated_files += 1;
                } else if stub_declares(cx.index, &key) {
                    left_out.stubs += 1;
                } else {
                    let name = cx.param_name(&key, param).unwrap_or_default();
                    candidates.push(Candidate {
                        key,
                        path: path.clone(),
                        param,
                        name,
                        lit: (**lit).clone(),
                    });
                }
            }
        }
    }
    let at: FxHashMap<(&FuncKey, u8), usize> = (candidates.iter().enumerate())
        .map(|(i, c)| ((&c.key, c.param), i))
        .collect();
    let mut union = Union::new(candidates.len());
    let mut unmet: Vec<Vec<Unmet>> = vec![Vec::new(); candidates.len()];
    let mut related: Vec<Vec<Related>> = vec![Vec::new(); candidates.len()];
    let mut family = vec![false; candidates.len()];
    let values = value_uses(cx);
    let calls = Calls::of(cx.index);
    for (i, c) in candidates.iter().enumerate() {
        let summary = cx.index.summary(&c.key);
        let facts = cx.facts(&c.key);
        unmet[i].extend(claim(c, summary));
        unmet[i].extend(local_premises(cx, c, summary, facts, &values));
        let (edges, open, rel) = callees_take_it(cx, c, &at, &candidates);
        for j in edges {
            union.join(i, j);
        }
        unmet[i].extend(open);
        related[i].extend(rel);
        let (edges, open) = whole_family(cx, c, facts, &at, &candidates);
        family[i] = !edges.is_empty();
        for j in edges {
            union.join(i, j);
        }
        unmet[i].extend(open);
    }
    let mut members: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..candidates.len() {
        members.entry(union.find(i)).or_default().push(i);
    }
    let mut by_premise: BTreeMap<usize, (usize, usize)> = BTreeMap::new();
    let mut pending = Vec::new();
    for items in members.into_values() {
        let first_drop = items
            .iter()
            .flat_map(|&i| &unmet[i])
            .filter(|u| u.drops)
            .min_by_key(|u| premise_index(u.premise));
        if let Some(u) = first_drop {
            by_premise.entry(premise_index(u.premise)).or_default().0 += items.len();
            continue;
        }
        let mut open_premises: FxHashSet<usize> = FxHashSet::default();
        for u in items.iter().flat_map(|&i| &unmet[i]) {
            open_premises.insert(premise_index(u.premise));
        }
        for p in &open_premises {
            by_premise.entry(*p).or_default().1 += items.len();
        }
        pending.push(group(
            cx,
            current,
            &candidates,
            &items,
            &unmet,
            &related,
            &family,
            &calls,
        ));
    }
    Found {
        groups: pending,
        left_out,
        by_premise,
        type_checker: TypeChecker::default(),
        reader_sites: ReaderSites::default(),
        withheld: BTreeMap::new(),
        tried: false,
    }
}

/// The claim's premises: only read here, and through every call (`MUT110`'s facts, one
/// judgment with two readers).
fn claim(c: &Candidate, summary: Option<&EffectSummary>) -> Vec<Unmet> {
    let unmet = |name, cause: &str| Unmet {
        premise: premise(name),
        drops: true,
        cause: cause.to_owned(),
        at: None,
    };
    let Some(s) = summary else {
        return vec![unmet(
            "Only read here",
            "heki has no summary of the function",
        )];
    };
    let bit = 1u64 << c.param;
    let changed = s
        .mutated_param_paths
        .get(&c.param)
        .is_some_and(|paths| paths.contains(""));
    if changed {
        return vec![unmet("Only read here", "the function changes it in place")];
    }
    if s.only_read_params & bit == 0 {
        return vec![unmet(
            "Does not leave",
            "it is kept, or handed to a call that may change or keep it",
        )];
    }
    Vec::new()
}

/// The premises decided by the function's own facts: abstract use, name bound once, whole
/// family's local part, not replaced, annotations stay unread, a name for the type.
fn local_premises(
    cx: &Cx<'_>,
    c: &Candidate,
    summary: Option<&EffectSummary>,
    facts: Option<&AnnotationFacts>,
    values: &FxHashMap<FuncKey, (PathBuf, u32)>,
) -> Vec<Unmet> {
    let mut out = Vec::new();
    let mut push = |name, drops, cause: String, at: Option<(PathBuf, u32)>| {
        out.push(Unmet {
            premise: premise(name),
            drops,
            cause,
            at,
        });
    };
    let bit = 1u64 << c.param;
    let container = summary.and_then(|s| s.container_params.as_deref());
    if container.is_none_or(|p| p.abstract_use & bit == 0) {
        push(
            "Abstract use",
            false,
            "an operation or a derived value that the read-only type does not allow".to_owned(),
            None,
        );
    }
    if container.is_none_or(|p| p.bound_once & bit == 0) {
        push(
            "Name bound once",
            false,
            "the name is bound again".to_owned(),
            None,
        );
    }
    let class = owner_class(cx, &c.key);
    if let Some(class) = &class
        && cx.decls.class_decl(class).is_some_and(|d| d.is_protocol)
    {
        push(
            "Whole family",
            true,
            "a member of a Protocol class".to_owned(),
            None,
        );
    }
    if let Some(f) = facts {
        if f.overloaded {
            push(
                "Whole family",
                true,
                "the function has @overload signatures".to_owned(),
                None,
            );
        }
        if f.definitions > 1 {
            push(
                "Whole family",
                false,
                "the name is defined more than once".to_owned(),
                None,
            );
        }
        match &f.reads {
            AnnotationReads::Unread => {}
            AnnotationReads::Read(by) => push(
                "Annotations stay unread",
                true,
                format!("`{by}` reads the annotations at run time"),
                None,
            ),
            AnnotationReads::Unknown(by) => push(
                "Annotations stay unread",
                false,
                format!("`{by}` may read the annotations at run time"),
                None,
            ),
        }
    }
    if let Some((path, span)) = values.get(&c.key) {
        push(
            "Annotations stay unread",
            false,
            "the function is used as a value".to_owned(),
            Some((path.clone(), *span)),
        );
    }
    if cx.decls.call_rebound(&c.key.fqn()) {
        push(
            "Not replaced",
            false,
            "the function may be replaced".to_owned(),
            None,
        );
    }
    if let Some(module) = cx.module(&c.key) {
        let future = cx
            .files
            .get(c.path.as_path())
            .is_some_and(|f| f.semantic.has_future_annotations);
        if let Naming::Taken(other) = type_name(&module, future, &c.lit, c.lit.kind) {
            push(
                "A name for the type",
                false,
                format!("`{}` names {other} here", simple_name(c.lit.kind)),
                None,
            );
        }
    }
    out
}

/// The class `key` is a method of, by its fully qualified name.
fn owner_class(cx: &Cx<'_>, key: &FuncKey) -> Option<String> {
    let (class, _) = key.qualname.rsplit_once('.')?;
    let fqn = format!("{}.{class}", key.module);
    cx.decls.class_decl(&fqn).is_some().then_some(fqn)
}

/// Premise 7, "Callees take it": the project parameters the candidate is handed to, each
/// unannotated, `Any`, `object`, an abstract type the read-only type is, or a candidate of the
/// same kind, which joins the group. Returns the candidates joined, the premise's open causes,
/// and where it is handed on.
fn callees_take_it(
    cx: &Cx<'_>,
    c: &Candidate,
    at: &FxHashMap<(&FuncKey, u8), usize>,
    candidates: &[Candidate],
) -> (Vec<usize>, Vec<Unmet>, Vec<Related>) {
    let (mut joined, mut open, mut related) = (Vec::new(), Vec::new(), Vec::new());
    let Some(file) = cx.index.files.get(&c.path) else {
        return (joined, open, related);
    };
    let bit = 1u64 << c.param;
    for site in file.call_sites.iter().filter(|s| s.caller == c.key) {
        for bc in site.project_callees() {
            for (k, arg) in bc.params.iter().enumerate().take(64) {
                if arg.may_params & bit == 0 {
                    continue;
                }
                let k = u8::try_from(k).expect("at most 64 positions");
                let line = cx.line(&c.path, site.span.start);
                related.push(Related {
                    path: (cx.shown)(&c.path),
                    line,
                    message: format!(
                        "`{}` hands `{}` to `{}`",
                        c.key.qualname,
                        c.name,
                        bc.callee.fqn()
                    ),
                });
                let annotation = cx
                    .facts(&bc.callee)
                    .and_then(|f| f.params.get(usize::from(k)))
                    .unwrap_or(&ParamAnnotation::Any);
                let takes = match annotation {
                    ParamAnnotation::Any => true,
                    ParamAnnotation::Admits(kinds) => kinds & c.lit.kind.bit() != 0,
                    ParamAnnotation::Literal(l) if l.kind == c.lit.kind => {
                        match at.get(&(&bc.callee, k)) {
                            Some(&j) => {
                                joined.push(j);
                                true
                            }
                            None => false,
                        }
                    }
                    ParamAnnotation::Literal(_) | ParamAnnotation::Other => false,
                };
                if !takes {
                    let name = cx.param_name(&bc.callee, k).unwrap_or_default();
                    let is_candidate = at.contains_key(&(&bc.callee, k));
                    let why = match annotation {
                        ParamAnnotation::Literal(l) if !is_candidate => format!(
                            "`{}` takes it as `{name}: {}`, which is not suggested",
                            bc.callee.fqn(),
                            l.text
                        ),
                        ParamAnnotation::Literal(l) => {
                            format!("`{}` takes it as `{name}: {}`", bc.callee.fqn(), l.text)
                        }
                        _ => format!(
                            "`{}` takes it as `{name}` with an annotation the read-only type does not satisfy",
                            bc.callee.fqn()
                        ),
                    };
                    open.push(Unmet {
                        premise: premise("Callees take it"),
                        drops: false,
                        cause: why,
                        at: Some((c.path.clone(), site.span.start)),
                    });
                }
            }
        }
    }
    let _ = candidates;
    (joined, open, related)
}

/// Premise 8, "Whole family", for a method: the same parameter of every definition of its
/// override family (the project classes above and below it defining the name, and theirs, to a
/// fixed point) is a candidate of the same kind, which joins the group, or already admits the
/// read-only type; the classes have no base outside the project but `object`, unless the
/// method is a constructor; a body that states nothing needs a family member that does.
fn whole_family(
    cx: &Cx<'_>,
    c: &Candidate,
    facts: Option<&AnnotationFacts>,
    at: &FxHashMap<(&FuncKey, u8), usize>,
    candidates: &[Candidate],
) -> (Vec<usize>, Vec<Unmet>) {
    let (mut joined, mut open) = (Vec::new(), Vec::new());
    let unmet = |drops, cause: String| Unmet {
        premise: premise("Whole family"),
        drops,
        cause,
        at: None,
    };
    let stub = facts.is_some_and(|f| f.stub_body);
    let Some(class) = owner_class(cx, &c.key) else {
        if stub {
            open.push(unmet(true, "the body states nothing of its own".to_owned()));
        }
        return (joined, open);
    };
    let method = c.key.qualname.rsplit('.').next().unwrap_or_default();
    let members = family(cx, &class, method);
    let mut stated = !stub;
    for (member_class, member) in &members {
        if !CONSTRUCTORS.contains(&method) {
            for base in cx.decls.mro_of(member_class) {
                if !cx.decls.is_project_class(&base) && base != "builtins.object" {
                    open.push(unmet(
                        false,
                        format!("`{base}`, a base outside the project, may declare `{method}`"),
                    ));
                }
            }
        }
        if member == &c.key {
            continue;
        }
        let member_facts = cx.facts(member);
        stated |= member_facts.is_some_and(|f| !f.stub_body);
        let name = cx.param_name(member, c.param);
        if name.as_deref() != Some(c.name.as_str()) {
            open.push(unmet(
                false,
                format!(
                    "the parameter is `{}` in `{}`",
                    name.unwrap_or_default(),
                    member.fqn()
                ),
            ));
            continue;
        }
        let annotation = member_facts
            .and_then(|f| f.params.get(usize::from(c.param)))
            .unwrap_or(&ParamAnnotation::Any);
        let fits = match annotation {
            ParamAnnotation::Any => true,
            ParamAnnotation::Admits(kinds) => kinds & c.lit.kind.bit() != 0,
            ParamAnnotation::Literal(l) if l.kind == c.lit.kind => match at.get(&(member, c.param))
            {
                Some(&j) => {
                    joined.push(j);
                    true
                }
                None => false,
            },
            ParamAnnotation::Literal(_) | ParamAnnotation::Other => false,
        };
        if !fits {
            open.push(unmet(
                false,
                format!(
                    "`{}` of its override family cannot take the read-only type",
                    member.fqn()
                ),
            ));
        }
    }
    if !stated {
        open.push(unmet(
            true,
            "no definition of its family states a body of its own".to_owned(),
        ));
    }
    open.sort_by(|a, b| a.cause.cmp(&b.cause));
    open.dedup_by(|a, b| a.cause == b.cause);
    let _ = candidates;
    (joined, open)
}

/// The override family of `class.method`: each project class along the hierarchy above or below
/// it that defines `method`, and the method, to a fixed point.
fn family(cx: &Cx<'_>, class: &str, method: &str) -> Vec<(String, FuncKey)> {
    let defines = |c: &str| {
        cx.decls
            .methods_of(c)
            .into_iter()
            .find(|k| k.qualname.rsplit('.').next() == Some(method))
    };
    let mut seen: BTreeMap<String, FuncKey> = BTreeMap::new();
    let mut todo = vec![class.to_owned()];
    let mut visited: FxHashSet<String> = FxHashSet::default();
    while let Some(c) = todo.pop() {
        if !visited.insert(c.clone()) {
            continue;
        }
        if let Some(k) = defines(&c) {
            seen.insert(c.clone(), k);
        }
        for related in cx
            .decls
            .ancestors_of(&c)
            .into_iter()
            .chain(cx.decls.subclasses_of(&c))
        {
            if !visited.contains(&related) && defines(&related).is_some() {
                todo.push(related);
            }
        }
    }
    seen.into_iter().collect()
}

/// The project functions used as a value (a reference handing one on), each with its first
/// place.
fn value_uses(cx: &Cx<'_>) -> FxHashMap<FuncKey, (PathBuf, u32)> {
    let mut out: FxHashMap<FuncKey, (PathBuf, u32)> = FxHashMap::default();
    let mut paths: Vec<&PathBuf> = cx.index.files.keys().collect();
    paths.sort();
    for path in paths {
        for site in &cx.index.files[path].reference_sites {
            for bc in site.project_callees() {
                out.entry(bc.callee.clone())
                    .or_insert_with(|| (path.clone(), site.span.start));
            }
        }
    }
    out
}

/// The project calls binding each function's parameters, for the ranking.
struct Calls<'a> {
    by_callee: FxHashMap<&'a FuncKey, Vec<(&'a CallSite, usize)>>,
}

impl<'a> Calls<'a> {
    fn of(index: &'a heki_effect::ProjectIndex) -> Self {
        let mut by_callee: FxHashMap<&FuncKey, Vec<(&CallSite, usize)>> = FxHashMap::default();
        for file in index.files.values() {
            for site in &file.call_sites {
                for (b, bc) in site.project_callees().iter().enumerate() {
                    by_callee.entry(&bc.callee).or_default().push((site, b));
                }
            }
        }
        Self { by_callee }
    }

    /// The calls of `key` binding its parameter `param`, and those passing an attribute there.
    fn leverage(&self, key: &FuncKey, param: u8) -> (usize, usize) {
        let mut out = (0, 0);
        for (site, b) in self.by_callee.get(key).into_iter().flatten() {
            let Some(arg) = site.project_callees()[*b].params.get(usize::from(param)) else {
                continue;
            };
            out.0 += 1;
            out.1 += usize::from(arg.self_attr.is_some() || arg.typed_field.is_some());
        }
        out
    }
}

/// The group of `items` (candidate indices) with its edits.
#[allow(clippy::too_many_arguments)]
fn group(
    cx: &Cx<'_>,
    current: &RuleSelection,
    candidates: &[Candidate],
    items: &[usize],
    unmet: &[Vec<Unmet>],
    related: &[Vec<Related>],
    family: &[bool],
    calls: &Calls<'_>,
) -> Pending {
    let mut members: Vec<&Candidate> = items.iter().map(|&i| &candidates[i]).collect();
    members.sort_by(|a, b| (&a.path, a.lit.head.start).cmp(&(&b.path, b.lit.head.start)));
    let first = members[0];
    let mut out_items = Vec::new();
    let mut edits = Vec::new();
    let mut files: BTreeMap<PathBuf, FileEdits> = BTreeMap::new();
    let mut open: Vec<OpenPremise> = Vec::new();
    let (mut call_sites, mut state_arguments) = (0, 0);
    for c in &members {
        let module = cx.module(&c.key);
        let parsed = cx.files.get(c.path.as_path());
        let future = parsed.is_some_and(|f| f.semantic.has_future_annotations);
        let naming = module
            .as_deref()
            .map_or(Naming::Taken("nothing heki knows".to_owned()), |m| {
                type_name(m, future, &c.lit, c.lit.kind)
            });
        let name = naming.name(c.lit.kind);
        let head_at = (c.lit.head.start - c.lit.span.start) as usize;
        let head_end = (c.lit.head.end - c.lit.span.start) as usize;
        let proposed = format!(
            "{}{name}{}",
            &c.lit.text[..head_at],
            &c.lit.text[head_end..]
        );
        let (line, column) = cx.line_col(&c.path, c.lit.head.start);
        let path = (cx.shown)(&c.path);
        out_items.push(Item {
            path: path.clone(),
            line,
            column,
            span: [c.lit.head.start, c.lit.head.end],
            qualname: c.key.qualname.clone(),
            target: c.name.clone(),
            current: c.lit.text.to_string(),
            proposed,
        });
        edits.push(Edit {
            path: path.clone(),
            span: [c.lit.head.start, c.lit.head.end],
            replacement: name.clone(),
        });
        let entry = files.entry(c.path.clone()).or_default();
        entry.0.push((c.lit.head, name));
        if let Naming::Import { statement } = &naming
            && !entry.1.contains(statement)
        {
            entry.1.push(statement.clone());
            let placed = parsed.and_then(|p| {
                let module_fn = p.ir.functions.iter().find(|f| f.is_module_body)?;
                let import = NewImport {
                    module: "collections.abc",
                    is_from: true,
                    statement,
                };
                import_edit(&p.source, module_fn, &import)
            });
            match placed {
                Some((span, text)) => edits.push(Edit {
                    path: path.clone(),
                    span: [span.start, span.end],
                    replacement: text,
                }),
                None => open.push(OpenPremise {
                    premise: "A name for the type".to_owned(),
                    cause: format!("no place in the module's leading imports for `{statement}`"),
                    path: path.clone(),
                    line: 1,
                }),
            }
        }
        let (calls_here, state) = calls.leverage(&c.key, c.param);
        call_sites += calls_here;
        state_arguments += state;
    }
    let mut open_names: FxHashSet<&str> = FxHashSet::default();
    for &i in items {
        let c = &candidates[i];
        for u in &unmet[i] {
            open_names.insert(u.premise.name);
            let (path, offset) = u.at.clone().unwrap_or((c.path.clone(), c.lit.head.start));
            open.push(OpenPremise {
                premise: u.premise.name.to_owned(),
                cause: u.cause.clone(),
                path: (cx.shown)(&path),
                line: cx.line(&path, offset),
            });
        }
    }
    for o in &open {
        open_names.insert(premise(&o.premise).name);
    }
    open.sort();
    open.dedup();
    let mut related_out: Vec<Related> = items.iter().flat_map(|&i| related[i].clone()).collect();
    related_out.sort();
    related_out.dedup();
    edits.sort();
    edits.dedup();
    let mut labels = Vec::new();
    let public = members
        .iter()
        .any(|c| c.key.fqn().split('.').all(|part| !part.starts_with('_')));
    if public {
        labels.push("public".to_owned());
    }
    let str_items = members.iter().any(|c| c.lit.str_items);
    if str_items {
        labels.push("list[str]".to_owned());
    }
    if items.iter().any(|&i| family[i]) {
        labels.push("family".to_owned());
    }
    let certainty = if open.is_empty() && !str_items {
        SuggestionCertainty::Exact
    } else {
        SuggestionCertainty::Heuristic
    };
    let others = members.len() - 1;
    let message = if others == 0 {
        format!("`{}` is only read in `{}`", first.name, first.key.qualname)
    } else {
        format!(
            "`{}` is only read in `{}` and in the {others} other function(s) of its group",
            first.name, first.key.qualname
        )
    };
    let premises = READONLY_PARAMETER
        .iter()
        .map(|p| PremiseState {
            name: p.name.to_owned(),
            established: !open_names.contains(p.name),
        })
        .collect();
    let readonly = readonly_name(first.lit.kind);
    let concrete = first.lit.kind.as_str();
    let guards = vec![
        Guard {
            by: "type-checker".to_owned(),
            rule: None,
            certainty: None,
            runs: None,
            catches: format!(
                "a change of `{}` in the function, and handing it to a `{concrete}` parameter",
                first.name
            ),
        },
        Guard {
            by: "heki".to_owned(),
            rule: Some("MUT110".to_owned()),
            certainty: Some("exact".to_owned()),
            runs: Some(current.is_enabled("MUT110")),
            catches: format!(
                "handing the `{readonly}` to a function without annotations that changes it"
            ),
        },
    ];
    Pending {
        rank: (
            std::cmp::Reverse(state_arguments),
            std::cmp::Reverse(call_sites),
            members.len(),
        ),
        group: Group {
            id: format!(
                "{READONLY_PARAMETER_KIND}:{}:{}:{}",
                first.key.module, first.key.qualname, first.name
            ),
            kind: READONLY_PARAMETER_KIND.to_owned(),
            certainty,
            message,
            items: out_items,
            edits,
            premises,
            open,
            related: related_out,
            guards,
            leverage: Leverage {
                call_sites,
                state_arguments,
            },
            labels,
        },
        files,
        withheld: None,
    }
}

/// The read-only type of a container kind, as the edit names it without a binding of its own.
fn simple_name(kind: ContainerKind) -> &'static str {
    match kind {
        ContainerKind::Dict => "Mapping",
        ContainerKind::Set => "AbstractSet",
        _ => "Sequence",
    }
}

/// The read-only type's name in `typing` (`typing.AbstractSet` is `collections.abc.Set`).
fn readonly_name(kind: ContainerKind) -> &'static str {
    simple_name(kind)
}

/// How an edit names the read-only type in a module.
enum Naming {
    /// A name the module binds to it already, or the dotted name through a `typing` module it
    /// imports (`t.Sequence` beside `t.List`).
    Bound(String),
    /// The simple name, which the import `statement` binds.
    Import { statement: String },
    /// The simple name is bound to something else (what it names).
    Taken(String),
}

impl Naming {
    fn name(&self, kind: ContainerKind) -> String {
        match self {
            Self::Bound(name) => name.clone(),
            Self::Import { .. } | Self::Taken(_) => simple_name(kind).to_owned(),
        }
    }
}

/// Premise 13, "A name for the type" (B.4 of the design): an existing binding of the read-only
/// type in the module is reused (one under `if TYPE_CHECKING:` too when the module's
/// annotations are not evaluated at run time); otherwise the edit imports it from
/// `collections.abc`, unless the name is bound to something else there.
fn type_name(
    decls: &ModuleDecls,
    future_annotations: bool,
    lit: &LiteralAnnotation,
    kind: ContainerKind,
) -> Naming {
    let simple = simple_name(kind);
    let targets: [&str; 2] = match kind {
        ContainerKind::Dict => ["collections.abc.Mapping", "typing.Mapping"],
        ContainerKind::Set => ["collections.abc.Set", "typing.AbstractSet"],
        _ => ["collections.abc.Sequence", "typing.Sequence"],
    };
    let head = &lit.text
        [(lit.head.start - lit.span.start) as usize..(lit.head.end - lit.span.start) as usize];
    if let Some((prefix, _)) = head.rsplit_once('.')
        && (decls.imports.get(prefix).map(String::as_str) == Some("typing")
            || decls.aliases.get(prefix).map(String::as_str) == Some("typing"))
    {
        let name = if kind == ContainerKind::Set {
            "AbstractSet"
        } else {
            simple
        };
        return Naming::Bound(format!("{prefix}.{name}"));
    }
    let bound_in = |table: &BTreeMap<String, String>| {
        let mut names: Vec<&String> = table
            .iter()
            .filter(|(_, fqn)| targets.contains(&fqn.as_str()))
            .map(|(name, _)| name)
            .collect();
        names.sort_by_key(|n| (n.as_str() != simple, n.len(), (*n).clone()));
        names.first().map(|n| (*n).clone())
    };
    if let Some(name) = bound_in(&decls.imports) {
        return Naming::Bound(name);
    }
    if future_annotations && let Some(name) = bound_in(&decls.type_checking_imports) {
        return Naming::Bound(name);
    }
    if let Some(other) = decls
        .imports
        .get(simple)
        .or_else(|| decls.type_checking_imports.get(simple))
    {
        return Naming::Taken(format!("`{other}`"));
    }
    if decls.binds(simple) {
        return Naming::Taken("another object".to_owned());
    }
    let statement = if kind == ContainerKind::Set {
        "from collections.abc import Set as AbstractSet".to_owned()
    } else {
        format!("from collections.abc import {simple}")
    };
    Naming::Import { statement }
}

/// Whether a stub (`.pyi`) of the project declares the function: the type checker reads the
/// stub, not the edited source.
fn stub_declares(index: &heki_effect::ProjectIndex, key: &FuncKey) -> bool {
    index.files.values().any(|f| {
        f.module_name == key.module
            && heki_effect::is_stub(&f.path)
            && f.decls.functions.contains_key(&key.qualname)
    })
}

/// Premise 12's file part: whether a leading comment of the file says it is generated (a line
/// with `generated` in any case, or `DO NOT EDIT`): the next generation would drop the edit.
fn says_generated(source: &str) -> bool {
    source
        .lines()
        .map(str::trim)
        .take_while(|l| l.is_empty() || l.starts_with('#'))
        .any(|l| l.to_ascii_lowercase().contains("generated") || l.contains("DO NOT EDIT"))
}

/// The type checker configured in the project at `root`, if any (assumption T3).
fn type_checker(root: &Path, shown: &dyn Fn(&Path) -> String) -> TypeChecker {
    for (name, file, table) in TYPE_CHECKERS {
        let path = root.join(file);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if table.is_empty() || text.lines().any(|l| l.trim_start().starts_with(table)) {
            return TypeChecker {
                found: Some((*name).to_owned()),
                config: Some(shown(&path)),
            };
        }
    }
    TypeChecker::default()
}

/// Where the project's code outside tests reads annotations at run time: calls of the functions
/// the catalog lists as reading them, and reads of `__annotations__`.
fn reader_sites(cx: &Cx<'_>) -> ReaderSites {
    let mut files: Vec<ReaderFile> = Vec::new();
    let mut total = 0;
    let mut parsed: Vec<&&crate::project::ParsedFile> = cx.files.values().collect();
    parsed.sort_by(|a, b| a.path.cmp(&b.path));
    for p in parsed {
        let Some(decls) = cx.index.module_decls(&p.module_name) else {
            continue;
        };
        let mut sites = 0;
        for f in &p.ir.functions {
            if heki_effect::code_kind::is_test_code(&f.qualname, &p.rel) {
                continue;
            }
            for inst in f.blocks.iter().flat_map(|b| &b.insts) {
                sites += usize::from(match &inst.kind {
                    InstKind::Call { callee, .. } => decls
                        .decorator_symbol(callee)
                        .is_some_and(|s| cx.kb.reads_passed_annotations(&s)),
                    InstKind::AttrLoad { attr, .. } => attr == "__annotations__",
                    _ => false,
                });
            }
        }
        if sites > 0 {
            total += sites;
            files.push(ReaderFile {
                path: (cx.shown)(&p.path),
                sites,
            });
        }
    }
    files.sort_by(|a, b| b.sites.cmp(&a.sites).then_with(|| a.path.cmp(&b.path)));
    let truncated = files.len() > MAX_LISTED;
    files.truncate(MAX_LISTED);
    ReaderSites {
        sites: total,
        files,
        truncated,
    }
}

/// Union-find over the candidates.
struct Union(Vec<usize>);

impl Union {
    fn new(n: usize) -> Self {
        Self((0..n).collect())
    }

    fn find(&mut self, i: usize) -> usize {
        let mut root = i;
        while self.0[root] != root {
            root = self.0[root];
        }
        let mut at = i;
        while self.0[at] != root {
            let next = self.0[at];
            self.0[at] = root;
            at = next;
        }
        root
    }

    fn join(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.0[a.max(b)] = a.min(b);
        }
    }
}

/// Premise 11, "No fact lost": the files of the groups analyzed again with the edits applied in
/// memory and compared with the analysis as it is. First the facts, file by file, from each
/// edited module's own summaries against the declarations as they are (equal local summaries
/// propagate to equal ones): a file's groups are tried together, and alone when together they
/// change a fact. Then the reports, for every file at once: the files of the groups left are
/// re-analyzed with their edits in the index, as the language server re-analyzes a file after
/// an edit, checked with every rule the discovery run selects, and restored; a group with an
/// edit in a file whose reports change is withheld. `reports` are the discovery run's, by file.
fn trial_all(
    indexed: &mut Indexed,
    found: &mut Found,
    reports: &FxHashMap<&Path, Vec<Diagnostic>>,
) -> Result<(), RecommendError> {
    let mut by_file: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
    for (g, p) in found.groups.iter().enumerate() {
        for path in p.files.keys() {
            by_file.entry(path.clone()).or_default().push(g);
        }
    }
    if by_file.is_empty() {
        return Ok(());
    }
    found.tried = true;
    let mut passes = indexed.index.decl_passes();
    for (path, groups) in &by_file {
        let (heads, imports) = edits_in(&found.groups, groups, path);
        let Some(changed) = facts_trial(indexed, &passes, path, &heads, &imports) else {
            continue;
        };
        if groups.len() == 1 {
            found.groups[groups[0]].withheld = Some(changed);
            continue;
        }
        for &g in groups {
            let (heads, imports) = edits_in(&found.groups, &[g], path);
            if let Some(changed) = facts_trial(indexed, &passes, path, &heads, &imports) {
                found.groups[g].withheld = Some(changed);
            }
        }
    }
    let mut edited: Vec<(usize, Heads, crate::project::ParsedFile)> = Vec::new();
    for (path, groups) in &by_file {
        let kept: Vec<usize> = (groups.iter().copied())
            .filter(|&g| found.groups[g].withheld.is_none())
            .collect();
        let Some(at) = indexed.project.files.iter().position(|f| &f.path == path) else {
            continue;
        };
        if kept.is_empty() {
            continue;
        }
        let (heads, imports) = edits_in(&found.groups, &kept, path);
        if let Ok(parsed) = edited_file(indexed, at, &heads, &imports) {
            edited.push((at, sorted(heads), parsed));
        }
    }
    if edited.is_empty() {
        return Ok(());
    }
    {
        let Indexed { project, index, .. } = &mut *indexed;
        let env = &project.env;
        for (_, _, parsed) in &edited {
            index.reanalyze_module_cached(
                &parsed.input(),
                &env.kb,
                Some(&env.soundness_kb),
                &mut passes,
            );
        }
    }
    let units: Vec<_> = edited.iter().map(|(_, _, p)| p.unit()).collect();
    let checked = indexed.diagnose_units(&units, false);
    drop(units);
    {
        let Indexed { project, index, .. } = &mut *indexed;
        let env = &project.env;
        for (at, _, _) in &edited {
            let input = project.files[*at].input();
            index.reanalyze_module_cached(&input, &env.kb, Some(&env.soundness_kb), &mut passes);
        }
    }
    let (mut after, withheld) = checked?;
    after.extend(withheld);
    let none = Vec::new();
    for (at, heads, _) in &edited {
        let path = indexed.project.files[*at].path.as_path();
        let map = |offset: u32| original_offset(heads, offset);
        let now: Vec<Diagnostic> = (reports.get(path).unwrap_or(&none).iter().cloned())
            .map(|d| placed_in_original(d, path, &|o| o))
            .collect();
        let then: Vec<Diagnostic> = (after.iter().filter(|d| d.path == path).cloned())
            .map(|d| placed_in_original(d, path, &map))
            .collect();
        let changed = match then.iter().find(|d| !now.contains(d)) {
            Some(d) => Some(format!(
                "`{}` at line {} would be reported",
                d.rule_code, d.line
            )),
            None => (now.iter().find(|d| !then.contains(d))).map(|d| {
                format!(
                    "`{}` at line {} would no longer be reported",
                    d.rule_code, d.line
                )
            }),
        };
        let Some(changed) = changed else {
            continue;
        };
        for &g in &by_file[path] {
            let group = &mut found.groups[g];
            if group.withheld.is_none() {
                group.withheld = Some(("report-would-change", changed.clone()));
            }
        }
    }
    Ok(())
}

/// The head edits and the import statements of the groups `gs` in the file `path`.
fn edits_in(groups: &[Pending], gs: &[usize], path: &Path) -> (Vec<(Span, String)>, Vec<String>) {
    let mut heads = Vec::new();
    let mut imports: Vec<String> = Vec::new();
    for &g in gs {
        let (h, i) = &groups[g].files[path];
        heads.extend(h.iter().cloned());
        for s in i {
            if !imports.contains(s) {
                imports.push(s.clone());
            }
        }
    }
    (heads, imports)
}

fn sorted(mut heads: Vec<(Span, String)>) -> Vec<(Span, String)> {
    heads.sort_by_key(|(span, _)| span.start);
    heads
}

/// The project's file at `at` with the head edits `heads` and the import statements `imports`
/// applied, parsed.
///
/// The imports go at the end of the file, not where the edit puts them: a report's message may
/// name a line (`at line 12`), and an import above would move every line below it. Placed
/// there, a module-level import binds the name for the annotations as at the top.
fn edited_file(
    indexed: &Indexed,
    at: usize,
    heads: &[(Span, String)],
    imports: &[String],
) -> Result<crate::project::ParsedFile, crate::project::FileError> {
    let original = &indexed.project.files[at];
    let mut edited = String::with_capacity(original.source.len() + 64);
    let mut from = 0usize;
    for (span, text) in &sorted(heads.to_vec()) {
        edited.push_str(&original.source[from..span.start as usize]);
        edited.push_str(text);
        from = span.end as usize;
    }
    edited.push_str(&original.source[from..]);
    let br = heki_ast::line_break(&original.source);
    if !edited.is_empty() && !edited.ends_with(['\n', '\r']) {
        edited.push_str(br);
    }
    for statement in imports {
        edited.push_str(statement);
        edited.push_str(br);
    }
    let env = &indexed.project.env;
    parse_source(&original.path, &env.root, edited, &env.layout, env.runtime)
}

/// The facts part of the trial of one file: `None` when the edited module's own summaries
/// equal those of the module now, else the reason and what changed.
fn facts_trial(
    indexed: &Indexed,
    passes: &DeclPasses,
    path: &Path,
    heads: &[(Span, String)],
    imports: &[String],
) -> Option<(&'static str, String)> {
    let at = indexed.project.files.iter().position(|f| f.path == path)?;
    let Ok(parsed) = edited_file(indexed, at, heads, imports) else {
        return Some((
            "edit-does-not-parse",
            "the edited file does not parse".to_owned(),
        ));
    };
    let env = &indexed.project.env;
    let edited =
        (indexed.index).summarize_edit(&parsed.input(), &env.kb, Some(&env.soundness_kb), passes);
    let now = indexed.index.files.get(path).map(|f| &f.local_functions);
    let heads = sorted(heads.to_vec());
    let map = |offset: u32| original_offset(&heads, offset);
    changed_facts(now, &edited.local_functions, &map).map(|c| ("fact-would-change", c))
}

/// What differs between the local summaries `now` and `edited` of a file's functions, as
/// Early Cutoff compares them, apart from the field that restates the parameters'
/// annotations (`container_params`): the first function and the fields that differ. The spans
/// some facts keep are compared at their places in the original file (`map`).
fn changed_facts(
    now: Option<&FxHashMap<FuncKey, std::sync::Arc<EffectSummary>>>,
    edited: &FxHashMap<FuncKey, std::sync::Arc<EffectSummary>>,
    map: &dyn Fn(u32) -> u32,
) -> Option<String> {
    let comparable = |s: &EffectSummary, map: &dyn Fn(u32) -> u32| {
        let mut s = s.without_locations();
        s.container_params = None;
        let mut value = serde_json::to_value(&s).unwrap_or_default();
        map_spans(&mut value, map);
        value
    };
    let empty = FxHashMap::default();
    let now = now.unwrap_or(&empty);
    let mut keys: Vec<&FuncKey> = now.keys().chain(edited.keys()).collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        let a = now.get(key).map(|s| comparable(s, &|o| o));
        let b = edited.get(key).map(|s| comparable(s, map));
        if a == b {
            continue;
        }
        let fields = match (&a, &b) {
            (Some(serde_json::Value::Object(a)), Some(serde_json::Value::Object(b))) => {
                let mut names: Vec<&String> = (a.keys().chain(b.keys()))
                    .filter(|k| a.get(*k) != b.get(*k))
                    .collect();
                names.sort();
                names.dedup();
                names
                    .iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
            _ => "its summary".to_owned(),
        };
        return Some(format!(
            "the facts of `{}` would change: {fields}",
            key.qualname
        ));
    }
    None
}

/// `value` with every span in it (an object of `start` and `end` alone) mapped through `map`.
fn map_spans(value: &mut serde_json::Value, map: &dyn Fn(u32) -> u32) {
    match value {
        serde_json::Value::Object(o) => {
            let span = o.len() == 2
                && ["start", "end"]
                    .iter()
                    .all(|k| o.get(*k).is_some_and(serde_json::Value::is_u64));
            if span {
                for k in ["start", "end"] {
                    let at = o[k]
                        .as_u64()
                        .and_then(|v| u32::try_from(v).ok())
                        .unwrap_or(0);
                    o[k] = map(at).into();
                }
                return;
            }
            o.values_mut().for_each(|v| map_spans(v, map));
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(|v| map_spans(v, map)),
        _ => {}
    }
}

/// The offset in the original file of `offset` in the file with `heads` applied (sorted): a
/// position inside a replaced head is the head's start.
fn original_offset(heads: &[(Span, String)], offset: u32) -> u32 {
    let mut delta: i64 = 0;
    for (span, text) in heads {
        let start = i64::from(span.start) + delta;
        let end = start + text.len() as i64;
        let at = i64::from(offset);
        if at < start {
            break;
        }
        if at < end {
            return span.start;
        }
        delta += text.len() as i64 - i64::from(span.end - span.start);
    }
    u32::try_from(i64::from(offset) - delta).unwrap_or(0)
}

/// `d` with its places in `path` at their offsets in the original file (`map`), and no columns:
/// an edit before a place on its line moves the column, and the span says the rest. The lines
/// do not move: the heads hold no line break, and the imports come last.
fn placed_in_original(mut d: Diagnostic, path: &Path, map: &dyn Fn(u32) -> u32) -> Diagnostic {
    // The discovery run records what keeps its heuristic reports heuristic; the trial does not.
    d.open_records.clear();
    let span = |s: Span| Span {
        start: map(s.start),
        end: map(s.end),
    };
    if d.path == path {
        d.span = span(d.span);
        d.column = 0;
        for edit in d.fix.iter_mut().flatten() {
            edit.span = span(edit.span);
        }
    }
    for r in &mut d.related {
        if r.path == path {
            r.span = span(r.span);
            r.column = 0;
        }
    }
    d
}
