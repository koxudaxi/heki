//! The contract entries that match nothing in the analyzed files (`lint.unmatched-contract-entries`,
//! `heki recommend`). An entry naming no definition, attribute, method, or `__dict__` key of the
//! project guards nothing, and renaming or removing what it named makes it so without a report.
//! Each entry is judged by the matcher its rule compiles, against the project's functions and
//! classes, the names the classes declare (their own and their project bases'), and the
//! attributes and constant `__dict__` keys the code reads or writes on objects typed with them
//! or on objects heki cannot type, which may be of any class. A class the code gives attributes
//! named at run time may have any attribute: an entry naming none of its known ones is reported
//! only under `"error"`, as one that may name nothing. A name a copy takes from the namespace
//! of an instance of the class family or from the fields it declares, or a deletion removes, is
//! one the class has already.
//! Braces are taken first: an entry whose `{...}` group lists names is judged name by name, and
//! a group with an empty alternative is one name with its extensions. The keys that may name
//! code outside the project by its full name (an effect boundary's read and call targets and
//! allowed callees, the producers of `readonly-returns`, and the module patterns of
//! `import-boundaries`) are judged when an entry's first name is a module of the project.

use crate::mut_rules::{
    Allowance, AllowanceKeys, FunctionMatcher, Overrides, Subclasses, class_set, parse_targets,
    project_classes, split_arguments, split_item,
};
use crate::selection::RuleSelection;
use crate::{
    CheckContext, DottedPattern, GLOB_SYNTAX, NamePart, RuleConfigError, def_spellings,
    has_glob_syntax, last_name, split_last_dot,
};
use heki_effect::{AccessKind, ContractClass, FuncKey, ReadKind, Unresolved};
use heki_env::{EntryPlace, HekiConfig, UnmatchedEntries};
use rustc_hash::{FxHashMap, FxHashSet};
use schemars::JsonSchema;
use serde::Serialize;
use std::borrow::Cow;
use std::collections::hash_map::Entry;
use std::fmt::Write as _;

/// The most contracts, and the most names of a `{...}` group, the message of an entry names.
const MAX_NAMED: usize = 3;

/// A contract entry that matches nothing in the analyzed files, with every contract of its kind
/// holding the same text under the same key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct UnmatchedEntry {
    /// The contracts' kind (`write-owners`).
    pub kind: String,
    /// The key holding the entry (`targets`, `allowed-writers`).
    pub key: String,
    /// The entry as configured.
    pub entry: String,
    /// The contracts holding it, in configuration order: a name, or `#<position>` (from 1) for
    /// a contract without one.
    pub contracts: Vec<String>,
    /// What it names none of, in the words of the message (`function or method`).
    pub names_no: String,
    /// Existing names like the entry, written as the entry is, the closest first (at most
    /// three).
    pub nearest: Vec<String>,
    /// The names a `{...}` group of the entry lists that name nothing, written out
    /// (`Base.c` of `Base.{a,c}`); empty when the entry is one name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternatives: Vec<String>,
    /// The class family gets attributes named at run time, which may include the entry's:
    /// heki cannot tell whether it names one (judged only with
    /// `unmatched-contract-entries = "error"`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub named_at_run_time: bool,
    /// Where the entry is written when no contract table of the main configuration file holds
    /// it: in a file `include` lists, or in a group (`@name`); each place once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub written_at: Vec<EntryPlace>,
}

impl UnmatchedEntry {
    /// The entry in one line, as `heki check` warns about it and `heki recommend` lists it.
    pub fn message(&self) -> String {
        let contracts = listed(&self.contracts, |c| match c.strip_prefix('#') {
            Some(_) => c.to_owned(),
            None => format!("`{c}`"),
        });
        let written_at = match self.written_at.as_slice() {
            [] => String::new(),
            places => {
                let places: Vec<String> = places.iter().map(ToString::to_string).collect();
                format!(" (from {})", places.join(", "))
            }
        };
        let mut out = format!(
            "`{}` entry `{}` of {} {contracts}{written_at} ",
            self.key, self.entry, self.kind
        );
        let (verb, they) = match self.alternatives.as_slice() {
            [] => ("names", "it is one"),
            [_] => ("which names", "it is one"),
            _ => ("which name", "they are ones"),
        };
        if !self.alternatives.is_empty() {
            let _ = write!(
                out,
                "lists {}, ",
                listed(&self.alternatives, |a| format!("`{a}`"))
            );
        }
        let _ = write!(out, "{verb} no {} in the analyzed files", self.names_no);
        if self.named_at_run_time {
            let _ = write!(out, ", unless {they} the code names at run time");
        }
        let quoted: Vec<String> = self.nearest.iter().map(|n| format!("`{n}`")).collect();
        match quoted.as_slice() {
            [] => {}
            [one] => {
                let _ = write!(out, "; {one} exists");
            }
            [first @ .., last] => {
                let _ = write!(out, "; {} or {last} exist", first.join(", "));
            }
        }
        out
    }
}

/// `items` in a message, quoted by `quote`: the first [`MAX_NAMED`], and how many more.
fn listed(items: &[String], quote: impl Fn(&str) -> String) -> String {
    let named: Vec<String> = items.iter().take(MAX_NAMED).map(|i| quote(i)).collect();
    match (named.as_slice(), items.len() - named.len()) {
        ([one], 0) => one.clone(),
        (all, 0) => format!(
            "{} and {}",
            all[..all.len() - 1].join(", "),
            all[all.len() - 1]
        ),
        (all, more) => format!("{}, and {more} more", all.join(", ")),
    }
}

/// How the entries of a key are matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// `Class.attr` or `Class.attr["key"]`: an attribute, method, or `__dict__` key of a class
    /// family.
    Attribute,
    /// A name of a definition.
    Definition(Definition),
    /// A `readonly-returns` producer: a function or method, or an attribute of a class.
    Producer,
    /// An `import-boundaries` module pattern, or a module and a name of it (`pkg.db:Session`).
    Module,
    /// An `ignored-imports` pair of module patterns (`pkg.cli -> pkg.db`).
    ModulePair,
}

/// The keys whose entries may name code outside the project by its full dotted name
/// (`os.getenv`, `sys.platform`, `<stdlib>`): an entry is judged when its first name is a
/// module of the project, which says it names project code.
const JUDGED_BY_FIRST_NAME: [&str; 10] = [
    "effect-boundaries.read-targets",
    "effect-boundaries.call-targets",
    "effect-boundaries.allowed-callees",
    "readonly-returns.producers",
    "import-boundaries.importers",
    "import-boundaries.exempt-importers",
    "import-boundaries.allowed-imports",
    "import-boundaries.forbidden-imports",
    "import-boundaries.relative-imports",
    "import-boundaries.ignored-imports",
];

/// How an entry naming a definition is matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Definition {
    /// A function pattern, as `allowed-*` and `forbidden-*` match it.
    Function,
    /// A function, method, or class, as call targets match it (an argument list included).
    CallTarget,
    /// A method name or pattern, as `exempt-methods` matches it.
    Exempt,
    /// A module, class, or function holding functions, as effect-boundary scopes match it.
    Scope,
    /// A class pattern.
    Class,
}

impl Definition {
    fn names_no(self) -> &'static str {
        match self {
            Self::Function => "function or method",
            Self::CallTarget => "function, method, or class",
            Self::Exempt => "method",
            Self::Scope => "module, class, or function",
            Self::Class => "class",
        }
    }
}

/// One key of one contract whose entries are judged.
struct Key<'c> {
    /// The contract's name, or `#<position>`.
    contract: String,
    /// The contract's position among those of its kind.
    index: usize,
    /// The key as the rules compile it: the contract's kind, a dot, and the key
    /// (`write-owners.targets`).
    context: &'static str,
    entries: &'c [String],
    shape: Shape,
}

impl Key<'_> {
    fn kind(&self) -> &'static str {
        self.context
            .split_once('.')
            .map_or(self.context, |(kind, _)| kind)
    }

    fn key(&self) -> &'static str {
        self.context
            .split_once('.')
            .map_or(self.context, |(_, key)| key)
    }
}

/// Every key of the contracts of `config` whose entries are judged, in configuration order, of
/// the kinds whose rules `selection` runs: a contract whose rules do not run guards nothing
/// either way, and is not validated.
fn judged_keys<'c>(config: &'c HekiConfig, selection: &RuleSelection) -> Vec<Key<'c>> {
    use Definition::{CallTarget, Class, Exempt, Function, Scope};
    use Shape::{Attribute, Definition as Def, Module, ModulePair, Producer};
    fn push<'c>(
        out: &mut Vec<Key<'c>>,
        (index, name): (usize, &str),
        lists: &[(&'static str, &'c [String], Shape)],
    ) {
        let contract = if name.is_empty() {
            format!("#{}", index + 1)
        } else {
            name.to_owned()
        };
        for &(context, entries, shape) in lists {
            out.push(Key {
                contract: contract.clone(),
                index,
                context,
                entries,
                shape,
            });
        }
    }
    // The contracts of a kind, when one of its rules runs.
    fn running<'c, T>(contracts: &'c [T], codes: &[&str], selection: &RuleSelection) -> &'c [T] {
        if codes.iter().any(|c| selection.is_enabled(c)) {
            contracts
        } else {
            &[]
        }
    }
    let mut out = Vec::new();
    let write_owners = running(&config.write_owners, &["MUT201", "MUT205"], selection);
    for (i, r) in write_owners.iter().enumerate() {
        let lists = [
            ("write-owners.targets", &r.targets[..], Attribute),
            (
                "write-owners.allowed-writers",
                &r.allowed_writers[..],
                Def(Function),
            ),
            (
                "write-owners.forbidden-writers",
                &r.forbidden_writers[..],
                Def(Function),
            ),
            (
                "write-owners.exempt-methods",
                &r.exempt_methods[..],
                Def(Exempt),
            ),
            (
                "write-owners.allowed-protocols",
                &r.allowed_protocols[..],
                Def(Class),
            ),
            (
                "write-owners.forbid-cached-property-on",
                &r.forbid_cached_property_on[..],
                Def(Class),
            ),
        ];
        push(&mut out, (i, &r.name), &lists);
    }
    let read_owners = running(&config.read_owners, &["MUT203", "MUT206"], selection);
    for (i, r) in read_owners.iter().enumerate() {
        let lists = [
            ("read-owners.targets", &r.targets[..], Attribute),
            (
                "read-owners.allowed-readers",
                &r.allowed_readers[..],
                Def(Function),
            ),
            (
                "read-owners.forbidden-readers",
                &r.forbidden_readers[..],
                Def(Function),
            ),
            (
                "read-owners.exempt-methods",
                &r.exempt_methods[..],
                Def(Exempt),
            ),
            (
                "read-owners.allowed-protocols",
                &r.allowed_protocols[..],
                Def(Class),
            ),
        ];
        push(&mut out, (i, &r.name), &lists);
    }
    let call_owners = running(&config.call_owners, &["MUT204", "MUT207"], selection);
    for (i, r) in call_owners.iter().enumerate() {
        let lists = [
            ("call-owners.targets", &r.targets[..], Def(CallTarget)),
            (
                "call-owners.allowed-callers",
                &r.allowed_callers[..],
                Def(Function),
            ),
            (
                "call-owners.forbidden-callers",
                &r.forbidden_callers[..],
                Def(Function),
            ),
            (
                "call-owners.exempt-methods",
                &r.exempt_methods[..],
                Def(Exempt),
            ),
            (
                "call-owners.allowed-protocols",
                &r.allowed_protocols[..],
                Def(Class),
            ),
        ];
        push(&mut out, (i, &r.name), &lists);
    }
    let boundaries = running(&config.effect_boundaries, &["EFF201"], selection);
    for (i, r) in boundaries.iter().enumerate() {
        let lists = [
            ("effect-boundaries.scopes", &r.scopes[..], Def(Scope)),
            (
                "effect-boundaries.write-targets",
                &r.write_targets[..],
                Attribute,
            ),
            (
                "effect-boundaries.write-universe",
                &r.write_universe[..],
                Attribute,
            ),
            (
                "effect-boundaries.allow-writes",
                &r.allow_writes[..],
                Attribute,
            ),
            (
                "effect-boundaries.read-targets",
                &r.read_targets[..],
                Attribute,
            ),
            (
                "effect-boundaries.call-targets",
                &r.call_targets[..],
                Def(CallTarget),
            ),
            (
                "effect-boundaries.allowed-callees",
                &r.allowed_callees[..],
                Def(Scope),
            ),
        ];
        push(&mut out, (i, &r.name), &lists);
    }
    let returns = running(&config.readonly_returns, &["MUT202"], selection);
    for (i, r) in returns.iter().enumerate() {
        let lists = [("readonly-returns.producers", &r.producers[..], Producer)];
        push(&mut out, (i, &r.name), &lists);
    }
    let invalidation = running(&config.invalidation, &["MUT208"], selection);
    for (i, r) in invalidation.iter().enumerate() {
        let lists = [
            ("invalidation.targets", &r.targets[..], Attribute),
            (
                "invalidation.invalidators",
                &r.invalidators[..],
                Def(Function),
            ),
            (
                "invalidation.exempt-writers",
                &r.exempt_writers[..],
                Def(Function),
            ),
        ];
        push(&mut out, (i, &r.name), &lists);
    }
    let imports = running(&config.import_boundaries, &["IMP201", "IMP202"], selection);
    for (i, r) in imports.iter().enumerate() {
        let lists = [
            (
                "import-boundaries.importers",
                r.importers.as_deref().unwrap_or_default(),
                Module,
            ),
            (
                "import-boundaries.exempt-importers",
                &r.exempt_importers[..],
                Module,
            ),
            (
                "import-boundaries.allowed-imports",
                &r.allowed_imports[..],
                Module,
            ),
            (
                "import-boundaries.forbidden-imports",
                &r.forbidden_imports[..],
                Module,
            ),
            (
                "import-boundaries.relative-imports",
                &r.relative_imports[..],
                Module,
            ),
            (
                "import-boundaries.ignored-imports",
                &r.ignored_imports[..],
                ModulePair,
            ),
        ];
        push(&mut out, (i, &r.name), &lists);
    }
    out
}

/// What the entries are matched against: the project's functions and classes, and the names
/// the classes have.
struct ProjectNames<'a> {
    functions: Vec<&'a FuncKey>,
    /// The functions by the last part of each spelling of their qualname ([`def_spellings`]:
    /// `Parser.__run` and `Parser._Parser__run`), as the matchers accept either.
    functions_by_last: FxHashMap<Cow<'a, str>, Vec<&'a FuncKey>>,
    /// The functions by each part of their module and of each spelling of their qualname: those
    /// a module, class, or function of that name may hold.
    functions_within: FxHashMap<Cow<'a, str>, Vec<&'a FuncKey>>,
    classes: Vec<ContractClass>,
    /// The classes (by index) by the last part of each spelling of their qualname.
    classes_by_last: FxHashMap<String, Vec<usize>>,
    /// The attributes and constant `__dict__` keys the code writes on objects typed with each
    /// class (fqn), and the attributes and keys it reads through the namespace of one.
    accessed: FxHashMap<&'a str, FxHashSet<&'a str>>,
    /// The constant `__dict__` keys the code reads directly on objects typed with each class
    /// (fqn): a key that exists only where some code writes it, which a read-owners target
    /// matches all the same.
    dict_read: FxHashMap<&'a str, FxHashSet<&'a str>>,
    /// The attributes and constant `__dict__` keys the code writes, and the target names it reads
    /// or calls, on objects heki cannot type: each may be any class's.
    on_unknown: FxHashSet<&'a str>,
    /// The classes (fqn) the code writes attributes named at run time on.
    computed: FxHashSet<&'a str>,
    /// `unmatched-contract-entries = "error"`: an entry naming none of the known names of a
    /// class the code gives attributes named at run time is reported too.
    strict: bool,
    /// The names each class has ([`Self::names_of`]), found once, without and with the keys only
    /// read.
    names: [FxHashMap<String, FxHashSet<&'a str>>; 2],
    /// The top-level modules of the analyzed files.
    project_modules: FxHashSet<&'a str>,
    /// The modules of the analyzed files and the packages holding them, namespace packages
    /// included: what a module pattern may name.
    modules: FxHashSet<&'a str>,
    /// The top-level modules outside the project heki knows the analyzed files to use: those
    /// their module-level imports name.
    imported_outside: FxHashSet<&'a str>,
    /// The symbols outside the project the analyzed files call and read that a contract names
    /// ([`heki_effect::FileSummary::external_accesses`]).
    outside_calls: FxHashSet<&'a str>,
    outside_reads: FxHashSet<&'a str>,
    /// The forms of the functions and of the classes ([`WrittenForms`]) and the last parts of
    /// the functions' qualnames, sorted and each once: what the names like an entry that matches
    /// nothing are found among, built when an entry first needs them.
    function_forms: Option<WrittenForms>,
    class_forms: Option<WrittenForms>,
    method_names: Option<Vec<&'a str>>,
    /// The comparisons finding names like the entries may still take.
    nearest_left: NearestBudget,
}

/// The comparisons that finding the names like the entries that match nothing
/// ([`UnmatchedEntry::nearest`]) may take in a run, per function and class of the project, and
/// at least. Each entry is compared with every name one part away from it, so thousands of
/// entries naming nothing in a module of thousands of functions would compare every pair. An
/// entry past the budget is reported without names.
const NEAREST_COMPARISONS_PER_DEFINITION: usize = 64;
const MIN_NEAREST_COMPARISONS: usize = 4_096;

/// The comparisons left to find names like the entries ([`NEAREST_COMPARISONS_PER_DEFINITION`]).
struct NearestBudget(usize);

impl NearestBudget {
    /// The names `nearest` finds by `comparisons` more, when they fit the budget, which they then
    /// take; else none. Comparisons that do not fit take nothing, so a later entry with fewer
    /// names one part away may still get its names.
    fn offer<T>(&mut self, comparisons: usize, nearest: impl FnOnce() -> Vec<T>) -> Vec<T> {
        let Some(left) = self.0.checked_sub(comparisons) else {
            return Vec::new();
        };
        self.0 = left;
        nearest()
    }
}

/// The forms one kind of definition is written in, but the names heki makes up (`<module>`,
/// `<lambda>`), in the order of the definitions, each with the index of its definition. They
/// are indexed by what holds their last part and by that part, apart for the forms with a
/// module part, so that an entry is compared only with the forms one part away from it
/// ([`nearest_written`]).
struct WrittenForms {
    forms: Vec<(usize, String)>,
    /// Positions in `forms` by whether the form has a module part (1) or not (0), then by what
    /// holds its last part ([`split_holder`]).
    by_holder: [FxHashMap<String, Vec<usize>>; 2],
    /// Positions in `forms` by whether the form has a module part, then by its last part.
    by_last: [FxHashMap<String, Vec<usize>>; 2],
}

impl WrittenForms {
    fn new(definitions: impl Iterator<Item = Vec<String>>) -> Self {
        let mut out = Self {
            forms: Vec::new(),
            by_holder: Default::default(),
            by_last: Default::default(),
        };
        for (i, forms) in definitions.enumerate() {
            for form in forms.into_iter().filter(|f| !f.contains('<')) {
                let with_module = usize::from(form.contains(':'));
                let (holder, last) = split_holder(&form);
                let at = out.forms.len();
                (out.by_holder[with_module].entry(holder.to_owned()))
                    .or_default()
                    .push(at);
                (out.by_last[with_module].entry(last.to_owned()))
                    .or_default()
                    .push(at);
                out.forms.push((i, form));
            }
        }
        out
    }

    /// The positions of the forms written as `entry` is (with a module part or without one)
    /// that have its holder, and of those that have its last part.
    fn one_part_away(&self, entry: &str) -> [&[usize]; 2] {
        fn at<'i>(index: &'i FxHashMap<String, Vec<usize>>, key: &str) -> &'i [usize] {
            index.get(key).map_or(&[], Vec::as_slice)
        }
        let with_module = usize::from(entry.contains(':'));
        let (holder, last) = split_holder(entry);
        [
            at(&self.by_holder[with_module], holder),
            at(&self.by_last[with_module], last),
        ]
    }
}

impl<'a> ProjectNames<'a> {
    fn new(cx: &CheckContext<'a>) -> Self {
        let defined = cx.defined_functions();
        let mut functions_by_last: FxHashMap<Cow<'a, str>, Vec<&FuncKey>> = FxHashMap::default();
        let mut functions_within: FxHashMap<Cow<'a, str>, Vec<&FuncKey>> = FxHashMap::default();
        let mut accessed: FxHashMap<&str, FxHashSet<&str>> = FxHashMap::default();
        let mut dict_read: FxHashMap<&str, FxHashSet<&str>> = FxHashMap::default();
        let (mut on_unknown, mut computed) = (FxHashSet::default(), FxHashSet::default());
        for &(key, s) in &defined {
            on_unknown.extend(s.unresolved_access_sites.iter().map(|a| a.name.as_str()));
            for w in &s.unresolved_write_sites {
                match &w.what {
                    Unresolved::Receiver(attr) => {
                        on_unknown.insert(attr.as_str());
                    }
                    Unresolved::Name(class) => {
                        computed.insert(class.as_str());
                    }
                    // Any attribute of any class: counting it would leave no entry judged.
                    Unresolved::Anything => {}
                    // A name the instance has already: it adds none.
                    Unresolved::Existing(_) => {}
                }
            }
            let mut parts: Vec<Cow<'a, str>> = key.module.split('.').map(Cow::Borrowed).collect();
            let mut lasts: Vec<Cow<'a, str>> = Vec::new();
            for (_, qualname) in def_spellings(&key.fqn(), &key.qualname) {
                let spelled = dotted_parts(qualname);
                lasts.extend(spelled.last().cloned());
                parts.extend(spelled);
            }
            lasts.dedup();
            for last in lasts {
                functions_by_last.entry(last).or_default().push(key);
            }
            parts.sort_unstable();
            parts.dedup();
            for part in parts {
                functions_within.entry(part).or_default().push(key);
            }
            let writes = (s.typed_field_write_sites.iter()).map(|w| (&w.class_qualname, &w.attr));
            let reads =
                (s.typed_field_read_sites.iter()).map(|r| (r.kind, &r.class_qualname, &r.attr));
            for (class, attr) in writes {
                accessed.entry(class).or_default().insert(attr);
            }
            for (kind, class, attr) in reads {
                // A key read through the namespace exists only where some code writes it.
                let seen = if kind == ReadKind::DictDirect {
                    &mut dict_read
                } else {
                    &mut accessed
                };
                seen.entry(class).or_default().insert(attr);
            }
        }
        let classes = project_classes(cx);
        let mut classes_by_last: FxHashMap<String, Vec<usize>> = FxHashMap::default();
        for (i, c) in classes.iter().enumerate() {
            let qualname = c.qualname.as_deref().unwrap_or(&c.fqn);
            let mut lasts: Vec<String> = def_spellings(&c.fqn, qualname)
                .map(|(_, q)| last_part(&q).to_owned())
                .collect();
            lasts.dedup();
            for last in lasts {
                classes_by_last.entry(last).or_default().push(i);
            }
        }
        let (mut outside_calls, mut outside_reads) = (FxHashSet::default(), FxHashSet::default());
        for &(key, _) in &defined {
            for (_, a) in cx.external_accesses(key) {
                match a.kind {
                    AccessKind::Call => outside_calls.insert(a.name.as_str()),
                    AccessKind::Read => outside_reads.insert(a.name.as_str()),
                    AccessKind::ImportProbe => false,
                };
            }
        }
        let top_level = |module: &'a str| module.split('.').next().unwrap_or(module);
        let project_modules: FxHashSet<&str> = (cx.index.files.values())
            .map(|f| top_level(&f.module_name))
            .collect();
        let mut modules: FxHashSet<&str> = FxHashSet::default();
        for module in cx.index.files.values().map(|f| f.module_name.as_str()) {
            modules.insert(module);
            modules.extend(module.match_indices('.').map(|(at, _)| &module[..at]));
        }
        let imported_outside = (cx.index.files.values())
            .flat_map(|f| f.decls.imports.values())
            .map(|fqn| top_level(fqn))
            .filter(|m| !project_modules.contains(m))
            .collect();
        let definitions = defined.len() + classes.len();
        Self {
            project_modules,
            modules,
            imported_outside,
            outside_calls,
            outside_reads,
            functions: defined.into_iter().map(|(key, _)| key).collect(),
            functions_by_last,
            functions_within,
            classes,
            classes_by_last,
            accessed,
            dict_read,
            on_unknown,
            computed,
            strict: cx.config.lint.unmatched_contract_entries == Some(UnmatchedEntries::Error),
            names: Default::default(),
            function_forms: None,
            class_forms: None,
            method_names: None,
            nearest_left: NearestBudget(
                (NEAREST_COMPARISONS_PER_DEFINITION * definitions).max(MIN_NEAREST_COMPARISONS),
            ),
        }
    }

    /// The functions whose names look like `entry`, written as `entry` is ([`nearest_written`]).
    fn nearest_functions(&mut self, entry: &str) -> Vec<String> {
        let functions = &self.functions;
        let written = (self.function_forms).get_or_insert_with(|| {
            WrittenForms::new(functions.iter().map(|k| {
                vec![
                    k.qualname.clone(),
                    k.fqn(),
                    format!("{}:{}", k.module, k.qualname),
                ]
            }))
        });
        nearest_written(entry, written, &mut self.nearest_left)
            .into_iter()
            .map(|(_, n)| n)
            .collect()
    }

    /// The classes (by index in [`Self::classes`]) whose names look like `entry`
    /// ([`nearest_written`]).
    fn nearest_classes(&mut self, entry: &str) -> Vec<(usize, String)> {
        let classes = &self.classes;
        let written = (self.class_forms)
            .get_or_insert_with(|| WrittenForms::new(classes.iter().map(written_forms)));
        nearest_written(entry, written, &mut self.nearest_left)
    }

    /// The method names like `entry`, an exempt method. One with a holder or a glob is matched
    /// as a function pattern ([`Self::nearest_functions`]).
    fn nearest_methods(&mut self, entry: &str) -> Vec<String> {
        if entry.contains(['.', ':']) || has_glob_syntax(entry) {
            return self.nearest_functions(entry);
        }
        // An exempt method without its class matches the last part as written
        // (`Allowance::exempts`), never the spelling its class holds.
        let functions = &self.functions;
        let methods = self.method_names.get_or_insert_with(|| {
            let mut methods: Vec<&str> =
                functions.iter().map(|&k| last_part(&k.qualname)).collect();
            methods.sort_unstable();
            methods.dedup();
            methods
        });
        self.nearest_left.offer(methods.len(), || {
            (heki_env::problem::nearest(entry, methods.iter().copied()).into_iter())
                .map(str::to_owned)
                .collect()
        })
    }

    /// Whether `entry` of the `call-owners` or `read-owners` targets (`context`) names code
    /// outside the project: a symbol outside the project the analyzed files call or read, or any
    /// symbol of a module outside the project, which heki cannot list. An `<external>` entry, or
    /// one whose first name is a module of the standard library or a module the analyzed files
    /// import from outside the project (`sys._getframe`), is a rule about any use of that code,
    /// now or later. An entry whose first name is a module of the project names project code,
    /// also where the analyzed files leave out the module defining it. An entry of the other
    /// keys that may name code outside the project ([`JUDGED_BY_FIRST_NAME`]) names project
    /// code only then.
    fn names_outside(&self, context: &'static str, entry: &str) -> Result<bool, RuleConfigError> {
        let used = match context {
            "call-owners.targets" => &self.outside_calls,
            "read-owners.targets" => &self.outside_reads,
            _ if JUDGED_BY_FIRST_NAME.contains(&context) => {
                let first = entry.split(['.', ':']).next().unwrap_or(entry);
                return Ok(!self.project_modules.contains(first));
            }
            _ => return Ok(false),
        };
        // A call target with an argument list names project functions only.
        let (name, arguments) = split_arguments(entry, context)?;
        let first = name.split('.').next().unwrap_or(name);
        if arguments.is_some() || name.contains(':') || self.project_modules.contains(first) {
            return Ok(false);
        }
        let matcher = FunctionMatcher::named(name, context)?;
        Ok(matcher.names_outside_only()
            || used.iter().any(|s| matcher.names_external(s))
            || name.contains('.')
                && (heki_env::is_stdlib_module(first) || self.imported_outside.contains(first)))
    }

    /// The functions a pattern whose last part is literal may name: those whose qualname ends
    /// in that part, as every name it matches does.
    fn candidates(&self, text: &str) -> &[&'a FuncKey] {
        (self.functions_by_last.get(last_name(text))).map_or(&[], Vec::as_slice)
    }

    /// The classes a pattern whose text is `text` may name, as [`Self::candidates`] does.
    fn candidate_classes(&self, text: &str) -> impl Iterator<Item = &ContractClass> {
        let last = last_name(text);
        let all = has_glob_syntax(last);
        let some = self
            .classes_by_last
            .get(last)
            .map_or(&[][..], Vec::as_slice);
        let picked: Box<dyn Iterator<Item = &ContractClass>> = if all {
            Box::new(self.classes.iter())
        } else {
            Box::new(some.iter().map(|&i| &self.classes[i]))
        };
        picked
    }

    /// Whether the code may give `class` any attribute: it or a project base stores attributes
    /// heki cannot name, or the code writes names computed at run time on objects typed with
    /// one of them (which may be instances of `class`).
    fn has_unnamed(&self, cx: &CheckContext<'a>, class: &str) -> bool {
        std::iter::once(class.to_owned())
            .chain(cx.decls.ancestors_of(class))
            .any(|c| self.computed.contains(c.as_str()) || cx.decls.adds_unnamed_attrs(&c))
    }

    /// The names `class` has: those it and its project bases declare, and those the code
    /// accesses on objects typed with it. A `__dict__` key the code only reads counts when
    /// `with_dict_reads`.
    fn names_of(
        &mut self,
        cx: &CheckContext<'a>,
        class: &str,
        with_dict_reads: bool,
    ) -> &FxHashSet<&'a str> {
        let found = usize::from(with_dict_reads);
        if !self.names[found].contains_key(class) {
            let mut names: FxHashSet<&str> = FxHashSet::default();
            for c in std::iter::once(class.to_owned()).chain(cx.decls.ancestors_of(class)) {
                names.extend(cx.decls.declared_names(&c));
            }
            names.extend(self.accessed.get(class).into_iter().flatten().copied());
            if with_dict_reads {
                names.extend(self.dict_read.get(class).into_iter().flatten().copied());
            }
            self.names[found].insert(class.to_owned(), names);
        }
        &self.names[found][class]
    }
}

/// The last part of a qualname (`save` of `Store.save`).
fn last_part(qualname: &str) -> &str {
    qualname.rsplit('.').next().unwrap_or(qualname)
}

/// The dotted parts of `path`, borrowed as `path` is.
fn dotted_parts(path: Cow<'_, str>) -> Vec<Cow<'_, str>> {
    match path {
        Cow::Borrowed(p) => p.split('.').map(Cow::Borrowed).collect(),
        Cow::Owned(p) => p
            .split('.')
            .map(|part| Cow::Owned(part.to_owned()))
            .collect(),
    }
}

/// The entries of the contracts that match nothing in the analyzed files, one per kind, key,
/// text, and what the names of a `{...}` group that match nothing name none of, in
/// configuration order.
pub(crate) fn unmatched_entries(
    cx: &CheckContext<'_>,
) -> Result<Vec<UnmatchedEntry>, RuleConfigError> {
    let keys = judged_keys(cx.config, cx.selection);
    if keys.iter().all(|k| k.entries.is_empty()) {
        return Ok(Vec::new());
    }
    let mut names = ProjectNames::new(cx);
    let mut found: Vec<UnmatchedEntry> = Vec::new();
    let mut judged: FxHashMap<(&str, &str), Vec<usize>> = FxHashMap::default();
    for key in keys {
        let (kind, name) = (key.kind(), key.key());
        for (position, entry) in key.entries.iter().enumerate() {
            let at = match judged.entry((key.context, entry.as_str())) {
                Entry::Occupied(at) => at.into_mut(),
                Entry::Vacant(at) => {
                    let unmatched =
                        unmatched_names(cx, &mut names, key.shape, (kind, key.context), entry)?;
                    at.insert(record(&mut found, (kind, name), entry, unmatched))
                }
            };
            let place = (cx.config.sources).place(kind, key.index, name, position);
            for &at in at.iter() {
                let found = &mut found[at];
                if found.contracts.last() != Some(&key.contract) {
                    found.contracts.push(key.contract.clone());
                }
                if let Some(place) = place
                    && !found.written_at.contains(place)
                {
                    found.written_at.push(place.clone());
                }
            }
        }
    }
    Ok(found)
}

/// Add the names of `entry` (of `kind` under `key`) that name nothing to `found`, one entry for
/// each thing they name none of; the positions of those entries.
fn record(
    found: &mut Vec<UnmatchedEntry>,
    (kind, key): (&str, &str),
    entry: &str,
    unmatched: Vec<(Option<String>, Missing)>,
) -> Vec<usize> {
    let mut at: Vec<usize> = Vec::new();
    for (alternative, missing) in unmatched {
        let same = at.iter().copied().find(|&i| {
            found[i].names_no == missing.names_no
                && found[i].named_at_run_time == missing.named_at_run_time
        });
        let i = same.unwrap_or_else(|| {
            found.push(UnmatchedEntry {
                kind: kind.to_owned(),
                key: key.to_owned(),
                entry: entry.to_owned(),
                contracts: Vec::new(),
                names_no: missing.names_no,
                nearest: Vec::new(),
                alternatives: Vec::new(),
                named_at_run_time: missing.named_at_run_time,
                written_at: Vec::new(),
            });
            at.push(found.len() - 1);
            found.len() - 1
        });
        let found = &mut found[i];
        found.alternatives.extend(alternative);
        for nearest in missing.nearest {
            if found.nearest.len() < MAX_NAMED && !found.nearest.contains(&nearest) {
                found.nearest.push(nearest);
            }
        }
    }
    at
}

/// The names `entry` stands for that name nothing, each with what it names none of. Braces are
/// taken first ([`heki_env::glob::alternatives`]), in the text the rule matches as a pattern (a
/// call target without its argument list, a `Class.attr` target without its key): a name
/// names something when one of its forms does (`m:f` or `m:f.*` of `m:f{,.*}`). A name is
/// written out (as its first form) only when the entry lists several.
fn unmatched_names<'a>(
    cx: &CheckContext<'a>,
    names: &mut ProjectNames<'a>,
    shape: Shape,
    (section, context): (&'static str, &'static str),
    entry: &str,
) -> Result<Vec<(Option<String>, Missing)>, RuleConfigError> {
    let pattern = match shape {
        Shape::Definition(Definition::CallTarget) => split_arguments(entry, context)?.0,
        Shape::Attribute => split_item(entry).map_or(entry, |(base, _)| base),
        _ => entry,
    };
    let rest = &entry[pattern.len()..];
    // Text the glob syntax rejects is the configuration error the rule reports.
    let listed =
        heki_env::glob::alternatives(pattern).unwrap_or_else(|| vec![vec![pattern.into()]]);
    let several = listed.len() > 1;
    let mut out = Vec::new();
    'names: for forms in listed {
        let mut missing: Option<Missing> = None;
        for form in &forms {
            let written = format!("{form}{rest}");
            let Some(m) = judge(cx, names, shape, (section, context), &written)? else {
                continue 'names;
            };
            match &mut missing {
                Some(first) => first.named_at_run_time |= m.named_at_run_time,
                None => missing = Some(m),
            }
        }
        out.extend(missing.map(|m| (several.then(|| format!("{}{rest}", forms[0])), m)));
    }
    Ok(out)
}

/// What an entry that matches nothing names none of ([`UnmatchedEntry`]).
struct Missing {
    names_no: String,
    nearest: Vec<String>,
    named_at_run_time: bool,
}

impl Missing {
    fn new(names_no: String, nearest: Vec<String>) -> Self {
        Self {
            names_no,
            nearest,
            named_at_run_time: false,
        }
    }
}

/// `None` when `entry` (of `shape`, under `context` of `section`) names something in the
/// analyzed files, else what it names none of and the existing names like it. An entry whose
/// last part is a glob is a rule about every name it may match, now or later: it names
/// something when what holds that part does ([`glob_holder_named`]), which every name it
/// matches is held by.
fn judge<'a>(
    cx: &CheckContext<'a>,
    names: &mut ProjectNames<'a>,
    shape: Shape,
    (section, context): (&'static str, &'static str),
    entry: &str,
) -> Result<Option<Missing>, RuleConfigError> {
    let definition = match shape {
        // Each side is a module pattern, whose first name says whether it is judged.
        Shape::ModulePair => {
            for side in entry.split("->") {
                let missing = judge(cx, names, Shape::Module, (section, context), side.trim())?;
                if missing.is_some() {
                    return Ok(missing);
                }
            }
            return Ok(None);
        }
        _ if names.names_outside(context, entry)? => return Ok(None),
        Shape::Attribute => return judge_attribute(cx, names, (section, context), entry),
        Shape::Producer => return judge_producer(cx, names, (section, context), entry),
        Shape::Module => return judge_module(names, entry, context),
        Shape::Definition(definition) => definition,
    };
    let (name, arguments) = match definition {
        Definition::CallTarget => split_arguments(entry, context)?,
        _ => (entry, None),
    };
    let matched = match glob_holder_named(names, name, context)? {
        Some(held) => held,
        None => names_definition(cx, names, definition, name, context)?,
    };
    if matched {
        let parameters: Vec<String> = arguments.into_iter().flatten().map(|(p, _)| p).collect();
        if parameters.is_empty() || takes_parameters(cx, names, name, &parameters, context)? {
            return Ok(None);
        }
        let quoted: Vec<String> = parameters.iter().map(|p| format!("`{p}`")).collect();
        let names_no = format!("{} taking {}", definition.names_no(), quoted.join(" and "));
        return Ok(Some(Missing::new(names_no, Vec::new())));
    }
    let nearest = match definition {
        Definition::Exempt => names.nearest_methods(name),
        Definition::Class => (names.nearest_classes(name).into_iter())
            .map(|(_, n)| n)
            .collect(),
        _ => names.nearest_functions(name),
    };
    Ok(Some(Missing::new(
        definition.names_no().to_owned(),
        nearest,
    )))
}

/// [`judge`] of a `readonly-returns` producer, which names a function or method as a function
/// pattern does, or an attribute read on an object of a class ([`judge_attribute`]).
fn judge_producer<'a>(
    cx: &CheckContext<'a>,
    names: &mut ProjectNames<'a>,
    (section, context): (&'static str, &'static str),
    entry: &str,
) -> Result<Option<Missing>, RuleConfigError> {
    let function = Shape::Definition(Definition::Function);
    let Some(missing) = judge(cx, names, function, (section, context), entry)? else {
        return Ok(None);
    };
    let mut named_at_run_time = false;
    if split_last_dot(entry).is_some() {
        match judge_attribute(cx, names, (section, context), entry)? {
            None => return Ok(None),
            Some(attribute) => named_at_run_time = attribute.named_at_run_time,
        }
    }
    Ok(Some(Missing {
        names_no: "function, method, or attribute".to_owned(),
        nearest: missing.nearest,
        named_at_run_time,
    }))
}

/// [`judge`] of an `import-boundaries` module pattern: its module part (`pkg.db` of
/// `pkg.db:Session`) must match a module of the analyzed files or a package holding one, as
/// `import` finds a namespace package too.
fn judge_module(
    names: &ProjectNames<'_>,
    entry: &str,
    context: &'static str,
) -> Result<Option<Missing>, RuleConfigError> {
    let module = entry.split_once(':').map_or(entry, |(module, _)| module);
    let part = NamePart::new(module, entry, context)?;
    let named = names.modules.iter().any(|m| part.is_match(m));
    Ok((!named).then(|| Missing::new("module".to_owned(), Vec::new())))
}

/// Whether the matcher the rule of a `definition` entry compiles from `name` matches a project definition: a
/// function or method (by the function patterns, exempt methods, call targets, and scopes),
/// or a class (by every shape but exempt methods).
fn names_definition<'a>(
    cx: &CheckContext<'a>,
    names: &ProjectNames<'a>,
    definition: Definition,
    name: &str,
    context: &'static str,
) -> Result<bool, RuleConfigError> {
    let one = [name.to_owned()];
    let functions = names.candidates(name);
    let classes = || names.candidate_classes(name);
    Ok(match definition {
        Definition::Function => {
            let keys = AllowanceKeys {
                allowed: (&one, context),
                forbidden: (&[], context),
                exempt: (&[], context),
            };
            let a = Allowance::new(&keys, Overrides::Off)?;
            let p = DottedPattern::new(name, context)?;
            functions.iter().any(|k| a.names_allowed(k)) || classes().any(|c| p.matches_class(c))
        }
        Definition::Exempt => {
            let keys = AllowanceKeys {
                allowed: (&[], context),
                forbidden: (&[], context),
                exempt: (&one, context),
            };
            let a = Allowance::new(&keys, Overrides::Off)?;
            functions.iter().any(|k| a.exempts(k))
        }
        Definition::CallTarget => {
            let m = FunctionMatcher::new(cx, &names.classes, name, context)?;
            functions.iter().any(|k| m.matches(k))
                || classes().any(|c| m.names_def(&c.fqn, c.qualname.as_deref().unwrap_or(&c.fqn)))
                || binds_imported_name(cx, &m)
        }
        Definition::Scope => {
            let p = DottedPattern::new(name, context)?;
            (names.functions.iter()).any(|k| p.names_within(&k.fqn(), &k.qualname))
                || classes().any(|c| p.matches_class(c))
        }
        Definition::Class => {
            let p = DottedPattern::new(name, context)?;
            !class_set(cx, &p, &names.classes, Subclasses::Excluded).is_empty()
        }
    })
}

/// Whether a function or class the call target `name` names may be called with every one of
/// `parameters`: a call passing a constant for a parameter nothing it names takes raises
/// `TypeError` (`DataType(is_dict=True)` without an `is_dict` parameter or `**kwargs`), so the
/// target matches no call. A class whose initializer is not a project function, or that
/// declares the name (a dataclass field, a pydantic model's field for `model_construct`), may
/// take it. A target whose last name is a glob, or that names every callee, may name callables
/// heki does not list.
fn takes_parameters(
    cx: &CheckContext<'_>,
    names: &ProjectNames<'_>,
    name: &str,
    parameters: &[String],
    context: &'static str,
) -> Result<bool, RuleConfigError> {
    if has_glob_syntax(last_name(name)) {
        return Ok(true);
    }
    let m = FunctionMatcher::new(cx, &names.classes, name, context)?;
    let function_takes = |k: &FuncKey| {
        cx.decls
            .function_decl(k)
            .is_none_or(|d| d.has_kwargs || parameters.iter().all(|p| d.param_names.contains(p)))
    };
    let class_takes = |class: &str| {
        let lineage: Vec<String> =
            (std::iter::once(class.to_owned()).chain(cx.decls.ancestors_of(class))).collect();
        let declared: FxHashSet<&str> = (lineage.iter())
            .flat_map(|c| cx.decls.declared_names(c))
            .collect();
        parameters.iter().all(|p| declared.contains(p.as_str()))
            || cx
                .decls
                .mro_method(class, "__init__")
                .is_none_or(|init| function_takes(&init))
    };
    let named_class =
        |c: &&ContractClass| m.names_def(&c.fqn, c.qualname.as_deref().unwrap_or(&c.fqn));
    let functions: Vec<&FuncKey> = (names.candidates(name).iter())
        .copied()
        .filter(|k| m.matches(k))
        .collect();
    let classes: Vec<&ContractClass> = names.candidate_classes(name).filter(named_class).collect();
    // A module-qualified target no project function or class defines names an imported or
    // aliased object (`binds_imported_name`), whose signature heki does not have.
    if name.contains(':') && functions.is_empty() && classes.is_empty() {
        return Ok(true);
    }
    Ok(functions.iter().any(|k| function_takes(k)) || classes.iter().any(|c| class_takes(&c.fqn)))
}

/// Whether the module part of `m` names a project module that binds a name `m` matches to an
/// object it imports or aliases (`pkg:Kind` of `from pkg.enums import Kind` in `pkg`): a read of
/// it through the module object is a use of the target ([`heki_effect::CallSite::module_member`]).
fn binds_imported_name(cx: &CheckContext<'_>, m: &FunctionMatcher) -> bool {
    let Some(module) = m.whole().module().filter(|p| p.glob().is_none()) else {
        return false;
    };
    let Some(decls) = cx.decls.module(module.text()) else {
        return false;
    };
    let bound = decls.imports.keys().chain(decls.aliases.keys());
    bound
        .into_iter()
        .any(|name| m.names_def(&format!("{}.{name}", module.text()), name))
}

/// For `name` whose last part is a glob: whether the parts written out before its first glob
/// name a module, class, or function, which every name it matches is held by: what holds the
/// last part (`pkg.store` of `pkg.store.*`, `pkg:Store` of `pkg:Store.*`), or the prefix of a
/// glob holder (`pkg.zz` of `pkg.zz.*:*`, `pkg` of `pkg.*.run_*`). `true` when no part is
/// written out (`*_helper`, `*.run_*`); `None` when the last part is literal.
fn glob_holder_named(
    names: &ProjectNames<'_>,
    name: &str,
    context: &'static str,
) -> Result<Option<bool>, RuleConfigError> {
    if !has_glob_syntax(last_name(name)) {
        return Ok(None);
    }
    let first_glob = name.find(GLOB_SYNTAX).unwrap_or(0);
    let Some((holder, _)) = name[..first_glob].rsplit_once(['.', ':']) else {
        return Ok(Some(true));
    };
    let p = DottedPattern::new(holder, context)?;
    let last = last_name(holder);
    let functions = names
        .functions_within
        .get(last)
        .map_or(&[][..], Vec::as_slice);
    Ok(Some(
        (functions.iter()).any(|k| p.names_within(&k.fqn(), &k.qualname))
            || names.candidate_classes(holder).any(|c| p.matches_class(c)),
    ))
}

/// [`judge`] of a `Class.attr` (or `Class.attr["key"]`) entry: its class part must name a
/// project class, and its attribute part a name of the class family ([`ProjectNames::names_of`]).
/// A glob attribute part needs the class only, and a glob class part what holds it
/// ([`glob_holder_named`]). An entry target is judged by its attribute. A family the code gives
/// attributes named at run time ([`ProjectNames::has_unnamed`]) may have any: the entry names
/// something unless the contracts are strict ([`ProjectNames::strict`]).
fn judge_attribute<'a>(
    cx: &CheckContext<'a>,
    names: &mut ProjectNames<'a>,
    (section, context): (&'static str, &'static str),
    entry: &str,
) -> Result<Option<Missing>, RuleConfigError> {
    // A read target guards the reads of a key, so a key only read exists for it.
    let with_dict_reads = matches!(
        context,
        "read-owners.targets" | "effect-boundaries.read-targets"
    );
    let (base, item) = match split_item(entry) {
        Some((base, _)) => (base, &entry[base.len()..]),
        None => (entry, ""),
    };
    // A target without a dot is the configuration error `parse_targets` reports.
    let (class_part, attr) = split_last_dot(base).unwrap_or(("", base));
    let classes: Vec<ContractClass> = names.candidate_classes(class_part).cloned().collect();
    let parsed = parse_targets(cx, &classes, &[entry.to_owned()], section, context)?;
    let target = &parsed[0];
    let mut family: Vec<&String> = target.classes.iter().collect();
    family.sort();
    if family.is_empty() && glob_holder_named(names, class_part, context)? == Some(true)
        || !family.is_empty() && has_glob_syntax(attr)
    {
        return Ok(None);
    }
    if family.is_empty() {
        let mut nearest = Vec::new();
        for (i, class) in names.nearest_classes(class_part) {
            let fqn = names.classes[i].fqn.clone();
            if names
                .names_of(cx, &fqn, with_dict_reads)
                .iter()
                .any(|n| target.matches_attr(n))
            {
                nearest.push(format!("{class}.{attr}{item}"));
            }
        }
        return Ok(Some(Missing::new("class".to_owned(), nearest)));
    }
    if names.on_unknown.contains(attr) {
        return Ok(None);
    }
    let mut known: Vec<&str> = Vec::new();
    let mut named_at_run_time = false;
    for class in family {
        let has = names.names_of(cx, class, with_dict_reads);
        if has.iter().any(|n| target.matches_attr(n)) {
            return Ok(None);
        }
        known.extend(has.iter().copied());
        named_at_run_time |= names.has_unnamed(cx, class);
    }
    if named_at_run_time && !names.strict {
        return Ok(None);
    }
    known.sort_unstable();
    known.dedup();
    let nearest = names.nearest_left.offer(known.len(), || {
        (heki_env::problem::nearest(attr, known).into_iter())
            .map(|n| format!("{class_part}.{n}{item}"))
            .collect()
    });
    Ok(Some(Missing {
        names_no: format!("attribute, method, or `__dict__` key of `{class_part}`"),
        nearest,
        named_at_run_time,
    }))
}

/// The forms a class may be written in: its qualname, its fqn, and `module:qualname`.
fn written_forms(class: &ContractClass) -> Vec<String> {
    let qualname = class.qualname.as_deref().unwrap_or(&class.fqn);
    let mut out = vec![qualname.to_owned(), class.fqn.clone()];
    if let Some(module) = crate::module_of(&class.fqn, qualname) {
        out.push(format!("{module}:{qualname}"));
    }
    out
}

/// The definitions (by index in `written`) whose names look like `entry`, each once and written
/// as `entry` is (with a module part when it has one, else as a qualname or a fqn), the closest
/// first. One part of the name changed, as a rename or a move changes it: a name in the same
/// module or class whose last part looks like the entry's, or one with the same last part
/// whose holder looks like the entry's (`pkg.models:Model` for `pkg.model:Model`). None for a
/// glob, nor past the budget `left`, which the forms one part away take.
fn nearest_written(
    entry: &str,
    written: &WrittenForms,
    left: &mut NearestBudget,
) -> Vec<(usize, String)> {
    if has_glob_syntax(entry) {
        return Vec::new();
    }
    let [same_holder, same_last] = written.one_part_away(entry);
    left.offer(same_holder.len() + same_last.len(), || {
        nearest_one_part_away(entry, written, [same_holder, same_last])
    })
}

/// [`nearest_written`] among the forms one part away from `entry`, at the positions
/// [`WrittenForms::one_part_away`] gives.
fn nearest_one_part_away(
    entry: &str,
    written: &WrittenForms,
    [same_holder, same_last]: [&[usize]; 2],
) -> Vec<(usize, String)> {
    let (holder, last) = split_holder(entry);
    let form = |at: usize| split_holder(&written.forms[at].1);
    let mut at: Vec<usize> = (same_holder.iter())
        .filter(|&&at| looks_like(last, form(at).1))
        .chain(
            same_last
                .iter()
                .filter(|&&at| looks_like(holder, form(at).0)),
        )
        .copied()
        .collect();
    // In the order of the definitions and their forms: of names as close, the first is offered.
    at.sort_unstable();
    at.dedup();
    let candidates: Vec<(usize, &str)> = (at.into_iter())
        .map(|at| (written.forms[at].0, written.forms[at].1.as_str()))
        .collect();
    let of: FxHashMap<&str, usize> = candidates.iter().map(|&(i, f)| (f, i)).collect();
    let mut out: Vec<(usize, String)> = Vec::new();
    for name in heki_env::problem::nearest(entry, candidates.iter().map(|&(_, f)| f)) {
        let i = of[name];
        if out.iter().all(|(j, _)| *j != i) {
            out.push((i, name.to_owned()));
        }
    }
    out
}

/// A name split at its last dot or colon: what holds its last part (empty for none), and that
/// part.
fn split_holder(name: &str) -> (&str, &str) {
    name.rsplit_once(['.', ':']).unwrap_or(("", name))
}

/// Whether `name` looks like `word`, as [`heki_env::problem::nearest`] judges it.
fn looks_like(word: &str, name: &str) -> bool {
    !heki_env::problem::nearest(word, [name]).is_empty()
}
