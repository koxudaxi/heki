//! The contract entries that match nothing in the analyzed files (`lint.unmatched-contract-entries`,
//! `heki recommend`). An entry naming no definition, attribute, method, or `__dict__` key of the
//! project guards nothing, and renaming or removing what it named makes it so without a report.
//! Each entry is judged by the matcher its rule compiles, against the project's functions and
//! classes, the names the classes declare (their own and their project bases'), and the
//! attributes and constant `__dict__` keys the code reads or writes on objects typed with them
//! or on objects heki cannot type, which may be of any class. A class the code gives attributes
//! named at run time may have any attribute.
//! Only the keys whose entries name project code alone are judged: the read and call targets and
//! the allowed callees of an effect boundary, and the producers of `readonly-returns`, may name
//! symbols outside the project.

use crate::mut_rules::{
    Allowance, AllowanceKeys, FunctionMatcher, Subclasses, class_set, parse_targets,
    project_classes, split_arguments, split_item,
};
use crate::{
    CheckContext, DottedPattern, RuleConfigError, has_glob_syntax, last_name, split_last_dot,
};
use heki_effect::{ContractClass, FuncKey, Unresolved};
use heki_env::HekiConfig;
use rustc_hash::{FxHashMap, FxHashSet};
use schemars::JsonSchema;
use serde::Serialize;
use std::fmt::Write as _;

/// The most contracts the message of an entry several contracts hold names.
const MAX_NAMED_CONTRACTS: usize = 3;

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
}

impl UnmatchedEntry {
    /// The entry in one line, as `heki check` warns about it and `heki recommend` lists it.
    pub fn message(&self) -> String {
        let named: Vec<String> = (self.contracts.iter())
            .take(MAX_NAMED_CONTRACTS)
            .map(|c| match c.strip_prefix('#') {
                Some(_) => c.clone(),
                None => format!("`{c}`"),
            })
            .collect();
        let more = self.contracts.len() - named.len();
        let contracts = match (named.as_slice(), more) {
            ([one], 0) => one.clone(),
            (all, 0) => format!(
                "{} and {}",
                all[..all.len() - 1].join(", "),
                all[all.len() - 1]
            ),
            (all, more) => format!("{}, and {more} more", all.join(", ")),
        };
        let mut out = format!(
            "`{}` entry `{}` of {} {contracts} names no {} in the analyzed files",
            self.key, self.entry, self.kind, self.names_no
        );
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

/// How the entries of a key are matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// `Class.attr` or `Class.attr["key"]`: an attribute, method, or `__dict__` key of a class
    /// family.
    Attribute,
    /// A name of a definition.
    Definition(Definition),
}

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

/// Every key of the contracts of `config` whose entries name project code alone, in
/// configuration order.
fn judged_keys(config: &HekiConfig) -> Vec<Key<'_>> {
    use Definition::{CallTarget, Class, Exempt, Function, Scope};
    use Shape::{Attribute, Definition as Def};
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
                context,
                entries,
                shape,
            });
        }
    }
    let mut out = Vec::new();
    for (i, r) in config.write_owners.iter().enumerate() {
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
    for (i, r) in config.read_owners.iter().enumerate() {
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
    for (i, r) in config.call_owners.iter().enumerate() {
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
    for (i, r) in config.effect_boundaries.iter().enumerate() {
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
        ];
        push(&mut out, (i, &r.name), &lists);
    }
    for (i, r) in config.invalidation.iter().enumerate() {
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
    out
}

/// What the entries are matched against: the project's functions and classes, and the names
/// the classes have.
struct ProjectNames<'a> {
    functions: Vec<&'a FuncKey>,
    /// The functions by the last part of their qualname.
    functions_by_last: FxHashMap<&'a str, Vec<&'a FuncKey>>,
    /// The functions by each part of their module and qualname: those a module, class, or
    /// function of that name may hold.
    functions_within: FxHashMap<&'a str, Vec<&'a FuncKey>>,
    classes: Vec<ContractClass>,
    /// The classes (by index) by the last part of their qualname.
    classes_by_last: FxHashMap<String, Vec<usize>>,
    /// The attributes and constant `__dict__` keys the code reads or writes on objects typed with
    /// each class (fqn).
    accessed: FxHashMap<&'a str, FxHashSet<&'a str>>,
    /// The attributes and constant `__dict__` keys the code writes, and the target names it reads
    /// or calls, on objects heki cannot type: each may be any class's.
    on_unknown: FxHashSet<&'a str>,
    /// The classes (fqn) the code writes attributes named at run time on.
    computed: FxHashSet<&'a str>,
    /// The names each class has ([`Self::names_of`]), found once.
    names: FxHashMap<String, FxHashSet<&'a str>>,
}

impl<'a> ProjectNames<'a> {
    fn new(cx: &CheckContext<'a>) -> Self {
        let defined = cx.defined_functions();
        let mut functions_by_last: FxHashMap<&str, Vec<&FuncKey>> = FxHashMap::default();
        let mut functions_within: FxHashMap<&str, Vec<&FuncKey>> = FxHashMap::default();
        let mut accessed: FxHashMap<&str, FxHashSet<&str>> = FxHashMap::default();
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
                }
            }
            functions_by_last
                .entry(last_part(&key.qualname))
                .or_default()
                .push(key);
            let mut parts: Vec<&str> = key
                .module
                .split('.')
                .chain(key.qualname.split('.'))
                .collect();
            parts.sort_unstable();
            parts.dedup();
            for part in parts {
                functions_within.entry(part).or_default().push(key);
            }
            let writes = (s.typed_field_write_sites.iter()).map(|w| (&w.class_qualname, &w.attr));
            let reads = (s.typed_field_read_sites.iter()).map(|r| (&r.class_qualname, &r.attr));
            for (class, attr) in writes.chain(reads) {
                accessed.entry(class).or_default().insert(attr);
            }
        }
        let classes = project_classes(cx);
        let mut classes_by_last: FxHashMap<String, Vec<usize>> = FxHashMap::default();
        for (i, c) in classes.iter().enumerate() {
            let qualname = c.qualname.as_deref().unwrap_or(&c.fqn);
            classes_by_last
                .entry(last_part(qualname).to_owned())
                .or_default()
                .push(i);
        }
        Self {
            functions: defined.into_iter().map(|(key, _)| key).collect(),
            functions_by_last,
            functions_within,
            classes,
            classes_by_last,
            accessed,
            on_unknown,
            computed,
            names: FxHashMap::default(),
        }
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
            .any(|c| self.computed.contains(c.as_str()) || cx.decls.stores_unnamed_attrs(&c))
    }

    /// The names `class` has: those it and its project bases declare, and those the code
    /// accesses on objects typed with it.
    fn names_of(&mut self, cx: &CheckContext<'a>, class: &str) -> &FxHashSet<&'a str> {
        if !self.names.contains_key(class) {
            let mut names: FxHashSet<&str> = FxHashSet::default();
            for c in std::iter::once(class.to_owned()).chain(cx.decls.ancestors_of(class)) {
                names.extend(cx.decls.declared_names(&c));
            }
            names.extend(self.accessed.get(class).into_iter().flatten().copied());
            self.names.insert(class.to_owned(), names);
        }
        &self.names[class]
    }
}

/// The last part of a qualname (`save` of `Store.save`).
fn last_part(qualname: &str) -> &str {
    qualname.rsplit('.').next().unwrap_or(qualname)
}

/// The entries of the contracts that match nothing in the analyzed files, one per kind, key,
/// and text, in configuration order.
pub(crate) fn unmatched_entries(
    cx: &CheckContext<'_>,
) -> Result<Vec<UnmatchedEntry>, RuleConfigError> {
    let keys = judged_keys(cx.config);
    if keys.iter().all(|k| k.entries.is_empty()) {
        return Ok(Vec::new());
    }
    let mut names = ProjectNames::new(cx);
    let mut found: Vec<UnmatchedEntry> = Vec::new();
    let mut judged: FxHashMap<(&str, &str), Option<usize>> = FxHashMap::default();
    for key in keys {
        let (kind, name) = (key.kind(), key.key());
        for entry in key.entries {
            let at = match judged.get(&(key.context, entry.as_str())) {
                Some(at) => *at,
                None => {
                    let unmatched = judge(cx, &mut names, key.shape, (kind, key.context), entry)?;
                    let at = unmatched.map(|(names_no, nearest)| {
                        found.push(UnmatchedEntry {
                            kind: kind.to_owned(),
                            key: name.to_owned(),
                            entry: entry.clone(),
                            contracts: Vec::new(),
                            names_no,
                            nearest,
                        });
                        found.len() - 1
                    });
                    judged.insert((key.context, entry.as_str()), at);
                    at
                }
            };
            if let Some(at) = at
                && found[at].contracts.last() != Some(&key.contract)
            {
                found[at].contracts.push(key.contract.clone());
            }
        }
    }
    Ok(found)
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
) -> Result<Option<(String, Vec<String>)>, RuleConfigError> {
    let definition = match shape {
        Shape::Attribute => return judge_attribute(cx, names, (section, context), entry),
        Shape::Definition(definition) => definition,
    };
    let name = match definition {
        Definition::CallTarget => split_arguments(entry, context)?.0,
        _ => entry,
    };
    let matched = match glob_holder_named(names, name, context)? {
        Some(held) => held,
        None => names_definition(cx, names, definition, name, context)?,
    };
    if matched {
        return Ok(None);
    }
    let nearest = match definition {
        Definition::Exempt => nearest_methods(names, name),
        Definition::Class => (nearest_classes(names, name).into_iter())
            .map(|(_, n)| n)
            .collect(),
        _ => nearest_functions(names, name),
    };
    Ok(Some((definition.names_no().to_owned(), nearest)))
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
            let a = Allowance::new(&AllowanceKeys {
                allowed: (&one, context),
                forbidden: (&[], context),
                exempt: (&[], context),
            })?;
            let p = DottedPattern::new(name, context)?;
            functions.iter().any(|k| a.names_allowed(k)) || classes().any(|c| p.matches_class(c))
        }
        Definition::Exempt => {
            let a = Allowance::new(&AllowanceKeys {
                allowed: (&[], context),
                forbidden: (&[], context),
                exempt: (&one, context),
            })?;
            functions.iter().any(|k| a.exempts(k))
        }
        Definition::CallTarget => {
            let m = FunctionMatcher::new(cx, &names.classes, name, context)?;
            functions.iter().any(|k| m.matches(k))
                || classes().any(|c| m.names_def(&c.fqn, c.qualname.as_deref().unwrap_or(&c.fqn)))
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

/// For `name` whose last part is a glob: whether what holds that part names a module, class, or
/// function (`pkg.store` of `pkg.store.*`, `pkg:Store` of `pkg:Store.*`), or `true` when no
/// literal holder is left to judge (`*_helper`, `pkg.*.run_*`). `None` when the last part is
/// literal.
fn glob_holder_named(
    names: &ProjectNames<'_>,
    name: &str,
    context: &'static str,
) -> Result<Option<bool>, RuleConfigError> {
    if !has_glob_syntax(last_name(name)) {
        return Ok(None);
    }
    let Some((holder, _)) = name.rsplit_once(['.', ':']) else {
        return Ok(Some(true));
    };
    if has_glob_syntax(holder) {
        return Ok(Some(true));
    }
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
/// ([`glob_holder_named`]). An entry target is judged by its attribute.
fn judge_attribute<'a>(
    cx: &CheckContext<'a>,
    names: &mut ProjectNames<'a>,
    (section, context): (&'static str, &'static str),
    entry: &str,
) -> Result<Option<(String, Vec<String>)>, RuleConfigError> {
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
        for (i, class) in nearest_classes(names, class_part) {
            let fqn = names.classes[i].fqn.clone();
            if names
                .names_of(cx, &fqn)
                .iter()
                .any(|n| target.matches_attr(n))
            {
                nearest.push(format!("{class}.{attr}{item}"));
            }
        }
        return Ok(Some(("class".to_owned(), nearest)));
    }
    if names.on_unknown.contains(attr) {
        return Ok(None);
    }
    let mut known: Vec<&str> = Vec::new();
    for class in family {
        if names.has_unnamed(cx, class) {
            return Ok(None);
        }
        let has = names.names_of(cx, class);
        if has.iter().any(|n| target.matches_attr(n)) {
            return Ok(None);
        }
        known.extend(has.iter().copied());
    }
    known.sort_unstable();
    known.dedup();
    let nearest = (heki_env::problem::nearest(attr, known).into_iter())
        .map(|n| format!("{class_part}.{n}{item}"))
        .collect();
    Ok(Some((
        format!("attribute, method, or `__dict__` key of `{class_part}`"),
        nearest,
    )))
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

/// The definitions (by index in `forms`) whose names look like `entry`, each once and written
/// as `entry` is (with a module part when it has one, else as a qualname or a fqn), the closest
/// first. One part of the name changed, as a rename or a move changes it: a name in the same
/// module or class whose last part looks like the entry's, or one with the same last part
/// whose holder looks like the entry's (`pkg.models:Model` for `pkg.model:Model`). None for a
/// glob, nor a name heki makes up (`<module>`, `<lambda>`).
fn nearest_written(entry: &str, forms: impl Iterator<Item = Vec<String>>) -> Vec<(usize, String)> {
    if has_glob_syntax(entry) {
        return Vec::new();
    }
    let with_module = entry.contains(':');
    let (holder, last) = split_holder(entry);
    let one_part_changed = |f: &str| {
        let (h, l) = split_holder(f);
        h == holder && looks_like(last, l) || l == last && looks_like(holder, h)
    };
    let candidates: Vec<(usize, String)> = (forms.enumerate())
        .flat_map(|(i, f)| f.into_iter().map(move |f| (i, f)))
        .filter(|(_, f)| f.contains(':') == with_module && !f.contains('<') && one_part_changed(f))
        .collect();
    let of: FxHashMap<&str, usize> = candidates.iter().map(|(i, f)| (f.as_str(), *i)).collect();
    let mut out: Vec<(usize, String)> = Vec::new();
    for name in heki_env::problem::nearest(entry, candidates.iter().map(|(_, f)| f.as_str())) {
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

fn nearest_functions(names: &ProjectNames<'_>, entry: &str) -> Vec<String> {
    let forms = names.functions.iter().map(|k| {
        vec![
            k.qualname.clone(),
            k.fqn(),
            format!("{}:{}", k.module, k.qualname),
        ]
    });
    nearest_written(entry, forms)
        .into_iter()
        .map(|(_, n)| n)
        .collect()
}

fn nearest_classes(names: &ProjectNames<'_>, entry: &str) -> Vec<(usize, String)> {
    nearest_written(entry, names.classes.iter().map(written_forms))
}

fn nearest_methods(names: &ProjectNames<'_>, entry: &str) -> Vec<String> {
    if entry.contains(['.', ':']) || has_glob_syntax(entry) {
        return nearest_functions(names, entry);
    }
    let mut methods: Vec<&str> = names.functions_by_last.keys().copied().collect();
    methods.sort_unstable();
    heki_env::problem::nearest(entry, methods)
        .into_iter()
        .map(str::to_owned)
        .collect()
}
