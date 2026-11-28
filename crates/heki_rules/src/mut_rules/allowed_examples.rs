//! The allowed functions the help of a contract report names (`Diagnostic::allowed_examples`):
//! public project functions and methods whose own body does what the report shows, from a place
//! the contract allows, each with its file. A `write-owners`, `read-owners`, or `call-owners`
//! contract allows the functions its `allowed-*` patterns name, and every function outside its
//! `forbidden-*` patterns; an `invalidation` contract allows the writes of its exempt writers and
//! of the functions that announce every write of the target on every path. The functions come
//! from the index, as every other fact of a report, so the language server's check of one file
//! names the same ones as `heki check`. They carry no line: a line of another file moves with
//! every edit above it, which leaves the facts the help names the same.

use super::call_owners::split_arguments;
use super::{Allowance, AllowanceKeys, Named, Overrides, items_target, patterns};
use crate::{CheckContext, DottedPattern, last_name};
use heki_diagnostics::AllowedExample;
use heki_effect::{EffectSummary, FileSummary, FuncKey, ProjectIndex, SiteRole};
use heki_env::HekiConfig;
use heki_semantic::mangling::unmangled;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;

/// How many functions the help of one report names.
const NAMED: usize = 3;

/// The rules whose reports name allowed functions: an edit of the functions a contract allows
/// can change their help in every other file ([`example_digest`]).
pub const EXAMPLE_RULES: &[&str] = &[
    "MUT208", "MUT201", "MUT203", "MUT204", "MUT205", "MUT206", "MUT207",
];

/// One function found doing what a report shows, with its file.
type Found<'a> = (&'a FuncKey, &'a Path);

/// The functions found for the reports of one key, by target, member, and fully qualified name.
type ByTarget<'a> = BTreeMap<String, BTreeMap<String, BTreeMap<String, Found<'a>>>>;

/// The functions doing what the reports of a `MergedReports` show from places their contracts
/// allow, for the keys, targets, and members the reports name: by key, target, member, and
/// fully qualified name.
pub(crate) struct AllowedExamples<'a, K> {
    wanted: BTreeMap<K, ByTarget<'a>>,
}

impl<'a, K: Ord + Clone> AllowedExamples<'a, K> {
    pub(super) fn wanting<'k>(names: impl Iterator<Item = (&'k K, &'k str, &'k str)>) -> Self
    where
        K: 'k,
    {
        let mut wanted: BTreeMap<K, ByTarget<'a>> = BTreeMap::new();
        for (key, target, named) in names {
            wanted
                .entry(key.clone())
                .or_default()
                .entry(target.to_owned())
                .or_default()
                .entry(named.to_owned())
                .or_default();
        }
        Self { wanted }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.wanted.is_empty()
    }

    /// Record that `func`, defined in `path`, does what a report under `key` and `name`
    /// shows, from a place its contract allows.
    pub(crate) fn add(&mut self, (key, name): (&K, Named<'_>), func: &'a FuncKey, path: &'a Path) {
        let Some(found) = self
            .wanted
            .get_mut(key)
            .and_then(|targets| targets.get_mut(name.target))
            .and_then(|members| members.get_mut(name.named))
        else {
            return;
        };
        found.entry(func.fqn()).or_insert((func, path));
    }

    /// The functions found for the report under `key` naming `target` and `named`: the first
    /// [`NAMED`] by fully qualified name.
    pub(super) fn found_for(&self, key: &K, target: &str, named: &str) -> Vec<Found<'a>> {
        self.wanted
            .get(key)
            .and_then(|targets| targets.get(target))
            .and_then(|members| members.get(named))
            .into_iter()
            .flat_map(BTreeMap::values)
            .take(NAMED)
            .copied()
            .collect()
    }
}

/// The help of a contract report: the contract's `message`, then the functions `found` doing
/// what the report shows from places the contract allows (`who` of `named`), and those functions
/// with their files as the report's `allowed_examples`.
pub(super) fn contract_help(
    note: Option<&str>,
    (who, named): (&str, &str),
    found: &[Found<'_>],
) -> (Option<String>, Vec<AllowedExample>) {
    let examples: Vec<AllowedExample> = found
        .iter()
        .map(|&(func, path)| AllowedExample {
            fqn: func.fqn(),
            path: path.to_path_buf(),
        })
        .collect();
    // By fully qualified name, which names the module: the file's path is written relative to
    // the working directory by `heki check` and absolute by the language server.
    let listed: Vec<String> = examples.iter().map(|e| format!("`{}`", e.fqn)).collect();
    let examples_text =
        (!listed.is_empty()).then(|| format!("{who} of {named}: {}", listed.join(", ")));
    let help = match (note, examples_text) {
        (Some(note), Some(text)) => Some(format!("{note}; {text}")),
        (note, text) => note.map(str::to_owned).or(text),
    };
    (help, examples)
}

/// Whether a report can name `func` as a function doing what it shows: a public function or a
/// method of a public class, which other code may call. Not a scope nested in a function (a
/// lambda, a comprehension, a nested function), a private helper (`_name`), or a special method
/// (`__init__`, `__deepcopy__`), which construction and the protocols of the language call.
fn public_function(func: &FuncKey) -> bool {
    func.qualname
        .split('.')
        .all(|name| !name.starts_with(['_', '<']))
}

/// The functions of the project that `names` accepts as places to access a contract's targets
/// from ([`public_function`], and declared by their module, so no class body), with their
/// defining files and final summaries. The module's own declarations are part of its summary:
/// reading them reads nothing of the project's declarations a later edit could change.
pub(super) fn place_functions<'a>(
    cx: &CheckContext<'a>,
    names: impl Fn(&FuncKey) -> bool,
) -> Vec<(&'a FuncKey, &'a Path, &'a EffectSummary)> {
    cx.index
        .files
        .iter()
        .flat_map(|(path, file)| {
            let declared = |key: &FuncKey| file.decls.functions.contains_key(&key.qualname);
            (file.functions.iter())
                .filter(move |(key, _)| declared(key))
                .map(move |(key, s)| (key, path.as_path(), &**s))
        })
        .filter(|(key, path, _)| {
            cx.index.file_of(key) == Some(path) && public_function(key) && names(key)
        })
        .collect()
}

/// The functions one kind of owner contract accepts as places to access its targets from
/// ([`Allowance::names_place`]), and the last names of its targets: the attribute, or the
/// function, method, or class called.
#[derive(Default)]
struct OwnedNames {
    allowances: Vec<Allowance>,
    names: Vec<DottedPattern>,
    /// A contract has `include-overrides`, whose entries name overrides in classes the
    /// configuration alone does not tell: every function may be a place.
    overrides: bool,
}

impl OwnedNames {
    /// `None` for a pattern that does not compile.
    fn new<'c>(
        contracts: impl Iterator<Item = (AllowanceKeys<'c>, &'c [String], Option<bool>)>,
        context: &'static str,
        name: impl Fn(&'c str) -> &'c str,
    ) -> Option<Self> {
        let mut out = Self::default();
        for (keys, targets, include_overrides) in contracts {
            out.allowances
                .push(Allowance::new(&keys, Overrides::Off).ok()?);
            out.overrides |= include_overrides == Some(true);
            for t in targets {
                out.names.push(DottedPattern::new(name(t), context).ok()?);
            }
        }
        Some(out)
    }

    fn places(&self, func: &FuncKey) -> bool {
        self.overrides || self.allowances.iter().any(|a| a.names_place(func))
    }

    /// Whether `name` may be the name of a target: as written, or a private member mangled
    /// with any class (`_Ledger__rows` of `Ledger.__rows`).
    fn names(&self, name: &str) -> bool {
        let mut written = std::iter::once(name).chain(unmangled(name).map(|(_, written)| written));
        written.any(|n| self.names.iter().any(|p| p.matches(n)))
    }
}

/// What the contracts of a configuration accept as the places their reports' help can name,
/// by name alone: the functions the owner contracts allow, with the names of their targets, and
/// the exempt writers of the `invalidation` contracts. Built from the configuration, for the
/// language server ([`example_digest`]).
pub struct ExampleOwners {
    writers: OwnedNames,
    readers: OwnedNames,
    callers: OwnedNames,
    exempt_writers: Vec<DottedPattern>,
    /// An `invalidation` contract has `include-overrides`: every function may be an exempt
    /// writer ([`OwnedNames::overrides`]).
    exempt_overrides: bool,
    invalidation: bool,
}

impl ExampleOwners {
    /// `None` without an owner or `invalidation` contract, and for patterns that do not compile
    /// (a configuration error every check reports).
    pub fn new(config: &HekiConfig) -> Option<Self> {
        let writers = config.write_owners.iter().map(|r| {
            let keys = AllowanceKeys {
                allowed: (&r.allowed_writers, "write-owners.allowed-writers"),
                forbidden: (&r.forbidden_writers, "write-owners.forbidden-writers"),
                exempt: (&r.exempt_methods, "write-owners.exempt-methods"),
            };
            (keys, r.targets.as_slice(), r.include_overrides)
        });
        let readers = config.read_owners.iter().map(|r| {
            let keys = AllowanceKeys {
                allowed: (&r.allowed_readers, "read-owners.allowed-readers"),
                forbidden: (&r.forbidden_readers, "read-owners.forbidden-readers"),
                exempt: (&r.exempt_methods, "read-owners.exempt-methods"),
            };
            (keys, r.targets.as_slice(), r.include_overrides)
        });
        let callers = config.call_owners.iter().map(|r| {
            let keys = AllowanceKeys {
                allowed: (&r.allowed_callers, "call-owners.allowed-callers"),
                forbidden: (&r.forbidden_callers, "call-owners.forbidden-callers"),
                exempt: (&r.exempt_methods, "call-owners.exempt-methods"),
            };
            (keys, r.targets.as_slice(), r.include_overrides)
        });
        let calls = "call-owners.targets";
        let exempt = config.invalidation.iter().flat_map(|r| &r.exempt_writers);
        let exempt: Vec<String> = exempt.cloned().collect();
        let out = Self {
            // A text that may name items (whether it does depends on the classes) may be the
            // target of any write.
            writers: OwnedNames::new(writers, "write-owners.targets", |t| {
                match items_target(t, |_| true) {
                    Some(_) => "*",
                    None => last_name(t),
                }
            })?,
            readers: OwnedNames::new(readers, "read-owners.targets", last_name)?,
            callers: OwnedNames::new(callers, calls, |t| {
                last_name(split_arguments(t, calls).map_or(t, |(name, _)| name))
            })?,
            exempt_writers: patterns(&exempt, "invalidation.exempt-writers").ok()?,
            exempt_overrides: (config.invalidation.iter())
                .any(|r| r.include_overrides == Some(true)),
            invalidation: !config.invalidation.is_empty(),
        };
        let owners = [&out.writers, &out.readers, &out.callers];
        (out.invalidation || owners.iter().any(|o| !o.allowances.is_empty())).then_some(out)
    }

    fn exempt_writer(&self, func: &FuncKey) -> bool {
        let fqn = func.fqn();
        self.exempt_overrides
            || (self.exempt_writers.iter()).any(|p| p.matches_def(&fqn, &func.qualname))
    }
}

/// The last name of a function, method, or class (`pkg.model.Ledger.post`: `post`).
fn last_of(fqn: &str) -> &str {
    fqn.rsplit('.').next().unwrap_or(fqn)
}

/// What the help of contract reports can name of the public functions of `path`, folded into
/// one number (0 for none): the own writes, reads, and calls of a target's name and the
/// constructions by the functions `owners` accepts, and the `invalidation` target writes of every
/// function with the writes it leaves unannounced. The language server checks the files with
/// reports of [`EXAMPLE_RULES`] again when it changes.
pub fn example_digest(owners: &ExampleOwners, index: &ProjectIndex, path: &Path) -> u64 {
    (index.files.get(path)).map_or(0, |file| digest_of(owners, index, file))
}

fn digest_of(owners: &ExampleOwners, index: &ProjectIndex, file: &FileSummary) -> u64 {
    let path = file.path.as_path();
    // A set: another access of one kind changes nothing a report names.
    let mut facts: BTreeSet<u64> = BTreeSet::new();
    let funcs = file.functions.iter();
    for (key, s) in
        funcs.filter(|(key, _)| index.file_of(key) == Some(path) && public_function(key))
    {
        let (exempt, writes) = (owners.exempt_writer(key), owners.writers.places(key));
        let own_writes = s
            .typed_field_write_sites
            .iter()
            .filter(|w| w.via_chain.is_empty());
        for w in own_writes.filter(|w| exempt || writes && owners.writers.names(&w.attr)) {
            facts.insert(fingerprint((
                key,
                "write",
                &w.class_qualname,
                &w.attr,
                &w.key,
                &w.construction,
            )));
        }
        let reads = owners.readers.places(key);
        for r in s
            .typed_field_read_sites
            .iter()
            .filter(|r| reads && owners.readers.names(&r.attr))
        {
            facts.insert(fingerprint((
                key,
                "read",
                &r.class_qualname,
                &r.attr,
                r.counts_for_contracts(),
            )));
        }
        let calls = owners.callers.places(key);
        for site in file.call_sites.iter().filter(|c| calls && &c.caller == key) {
            let callees = site.project_callees().iter().map(|bc| &bc.callee);
            let callees: Vec<&FuncKey> = callees.chain(site.declared()).collect();
            let mut names = callees.iter().map(|k| last_of(&k.qualname));
            // A subclass of a target class is constructed under a name of its own.
            if site.constructs.is_some() || names.any(|n| owners.callers.names(n)) {
                let call = site.role == SiteRole::Call;
                facts.insert(fingerprint((
                    key,
                    "call",
                    call,
                    &site.constructs,
                    &callees,
                    &site.arguments,
                )));
            }
        }
        let flows = file.flows.get(key).filter(|_| owners.invalidation);
        let writes: Vec<_> = flows.into_iter().flat_map(|f| &f.writes).collect();
        for w in &writes {
            facts.insert(fingerprint((
                key,
                "flow",
                w.contracts,
                &w.class_qualname,
                &w.attr,
            )));
        }
        // A write it leaves unannounced, its own or a helper's, keeps a function from announcing
        // its target.
        for u in s.unmet_invalidation.iter().filter(|_| !writes.is_empty()) {
            facts.insert(fingerprint((
                key,
                "unmet",
                u.contract,
                &u.class_qualname,
                &u.attr,
            )));
        }
    }
    if facts.is_empty() {
        0
    } else {
        fingerprint(&facts)
    }
}

fn fingerprint(value: impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}
