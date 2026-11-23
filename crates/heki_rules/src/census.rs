//! The census contract candidates are drawn from (`heki recommend`): per project class
//! attribute, the functions writing it outside the initialization of their own instance, the
//! initializers writing it, and the functions reading it; per project class, the modules using
//! it; per project module, its functions, those reaching a blocking effect, and those reaching
//! code heki does not see ([`ProjectIndex::reaching_blind_spots`]).
//!
//! The sites are each function's own typed read and write sites, which every local summary keeps
//! for every project class whatever the contracts, so the census is a pass over the index and
//! computes no fact of its own. It is keyed by the class heki types the accessed object with,
//! not the class defining the attribute: a write on an instance of a subclass is the subclass's.
//! Without an ownership contract the summaries keep no write into an entry of an attribute's
//! container (`obj.attr["k"].append(v)`) and do not answer whether a write constructs its
//! object; a contract trial, which declares the contracts, sees those.

use heki_ast::Span;
use heki_effect::{BLOCKING_EFFECTS, EffectSummary, FuncKey, INITIALIZERS, ProjectIndex};
use rustc_hash::FxHashSet;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

/// One function's accesses of one attribute: where the first is, and how many there are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accesses<'i> {
    pub function: &'i FuncKey,
    pub path: &'i Path,
    /// The first access in source order.
    pub first: Span,
    pub sites: usize,
}

/// One function's writes of one attribute outside the initialization of its own instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Writes<'i> {
    pub accesses: Accesses<'i>,
    /// Some write rebinds or deletes the attribute (`obj.attr = v`, `setattr`, `__dict__`).
    pub rebinds: bool,
    /// Some write changes the attribute's object in place (`obj.attr.append(v)`, also through a
    /// local alias).
    pub in_place: bool,
    /// Every write is of an object the function constructs itself and fills before anyone else
    /// holds it (a factory).
    pub fresh: bool,
}

/// What the project does with one attribute of one class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeUse<'i> {
    /// The class heki types the accessed objects with, fully qualified.
    pub class: &'i str,
    pub attr: &'i str,
    /// The functions writing it outside the initialization of their own instance, sorted.
    pub writers: Vec<Writes<'i>>,
    /// The initializers (`__init__`, `__post_init__`, ...) of the class writing it on their own
    /// instance, sorted.
    pub initializers: Vec<Accesses<'i>>,
    /// The functions reading it, sorted. A read through a protocol declaring the attribute is
    /// the protocol's, as the contracts judge it.
    pub readers: Vec<Accesses<'i>>,
}

/// The modules using one project class: constructing it, or reading or writing an attribute of
/// an instance of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClassUse<'i> {
    pub class: &'i str,
    /// Sorted.
    pub modules: Vec<&'i str>,
}

/// The functions of one project module, and what they reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleUse<'i> {
    pub module: &'i str,
    pub path: &'i Path,
    /// The module's functions and methods, its body left out, sorted.
    pub functions: Vec<&'i FuncKey>,
    /// The functions reaching a blocking effect (`effect-boundaries`' `blocking`), sorted.
    pub blocking: Vec<&'i FuncKey>,
    /// The functions reaching code heki does not see, sorted.
    pub unseen: Vec<&'i FuncKey>,
}

/// The census of a project, each list sorted by its keys (the modules by name).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Census<'i> {
    pub attributes: Vec<AttributeUse<'i>>,
    pub classes: Vec<ClassUse<'i>>,
    pub modules: Vec<ModuleUse<'i>>,
}

/// The totals of a [`Census`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct CensusTotals {
    pub classes: usize,
    pub attributes: usize,
    pub write_sites: usize,
    pub initializer_sites: usize,
    pub read_sites: usize,
    pub modules: usize,
    pub functions: usize,
}

/// The accesses of one attribute while they are gathered, by function.
#[derive(Default)]
struct Gathered<'i> {
    writers: BTreeMap<&'i FuncKey, Writes<'i>>,
    initializers: BTreeMap<&'i FuncKey, Accesses<'i>>,
    readers: BTreeMap<&'i FuncKey, Accesses<'i>>,
}

impl<'i> Census<'i> {
    /// The census of `index`, whose blind spots (`heki_rules::recommend_facts`) count for
    /// [`ModuleUse::unseen`] when it has them.
    pub fn of(index: &'i ProjectIndex) -> Self {
        let mut attributes: BTreeMap<(&str, &str), Gathered<'_>> = BTreeMap::new();
        let mut classes: BTreeMap<&str, FxHashSet<&str>> = BTreeMap::new();
        let mut modules: BTreeMap<&str, ModuleUse<'_>> = BTreeMap::new();
        let blocking: FxHashSet<_> = BLOCKING_EFFECTS.iter().collect();
        let unseen = index.reaching_blind_spots();
        let mut files: Vec<_> = index.files.values().collect();
        // A module's implementation before its stub: the module's entry shows the first.
        files.sort_by_key(|f| (&f.module_name, heki_effect::is_stub(&f.path), &f.path));
        for file in files {
            let path = file.path.as_path();
            let module = file.module_name.as_str();
            // A module defined twice (a stub and its implementation) counts its functions once,
            // in the file defining them.
            let mut functions: Vec<_> = (file.local_functions.iter())
                .filter(|(f, _)| index.file_of(f) == Some(path))
                .collect();
            functions.sort_by(|a, b| a.0.cmp(b.0));
            // A file heki could not read defines no function, not even its body.
            if functions.is_empty() {
                continue;
            }
            // A module defined by an implementation and its stub is one entry; each function
            // counts in the file defining it.
            let census = modules.entry(module).or_insert(ModuleUse {
                module,
                path,
                functions: Vec::new(),
                blocking: Vec::new(),
                unseen: Vec::new(),
            });
            for &(function, local) in &functions {
                gather(&mut attributes, &mut classes, module, path, function, local);
                if function.qualname == "<module>" {
                    continue;
                }
                census.functions.push(function);
                let reaches_blocking = index.summary(function).is_some_and(|s| {
                    (s.direct_exec_effects.iter())
                        .chain(&s.transitive_exec_effects)
                        .any(|e| blocking.contains(e))
                });
                if reaches_blocking {
                    census.blocking.push(function);
                }
                if unseen.contains(function) {
                    census.unseen.push(function);
                }
            }
            for site in &file.call_sites {
                if let Some(class) = &site.constructs {
                    classes.entry(class).or_default().insert(module);
                }
            }
        }
        Self {
            attributes: attributes
                .into_iter()
                .map(|((class, attr), g)| AttributeUse {
                    class,
                    attr,
                    writers: g.writers.into_values().collect(),
                    initializers: g.initializers.into_values().collect(),
                    readers: g.readers.into_values().collect(),
                })
                .collect(),
            classes: classes
                .into_iter()
                .map(|(class, modules)| {
                    let mut modules: Vec<_> = modules.into_iter().collect();
                    modules.sort_unstable();
                    ClassUse { class, modules }
                })
                .collect(),
            modules: (modules.into_values())
                .map(|mut m| {
                    m.functions.sort_unstable();
                    m.blocking.sort_unstable();
                    m.unseen.sort_unstable();
                    m
                })
                .collect(),
        }
    }

    /// The counts over the whole project.
    pub fn totals(&self) -> CensusTotals {
        let sites = |a: &[Accesses<'_>]| a.iter().map(|a| a.sites).sum::<usize>();
        CensusTotals {
            classes: self.classes.len(),
            attributes: self.attributes.len(),
            write_sites: (self.attributes.iter())
                .flat_map(|a| &a.writers)
                .map(|w| w.accesses.sites)
                .sum(),
            initializer_sites: self.attributes.iter().map(|a| sites(&a.initializers)).sum(),
            read_sites: self.attributes.iter().map(|a| sites(&a.readers)).sum(),
            modules: self.modules.len(),
            functions: self.modules.iter().map(|m| m.functions.len()).sum(),
        }
    }
}

/// Add the typed accesses of `function` (in `module`, file `path`; `local` its own summary).
fn gather<'i>(
    attributes: &mut BTreeMap<(&'i str, &'i str), Gathered<'i>>,
    classes: &mut BTreeMap<&'i str, FxHashSet<&'i str>>,
    module: &'i str,
    path: &'i Path,
    function: &'i FuncKey,
    local: &'i EffectSummary,
) {
    let access = |span| Accesses {
        function,
        path,
        first: span,
        sites: 1,
    };
    let add = |into: &mut Accesses<'i>, span: Span| {
        into.first = into.first.min(span);
        into.sites += 1;
    };
    for site in &local.typed_field_write_sites {
        let class = site.class_qualname.as_str();
        classes.entry(class).or_default().insert(module);
        let gathered = attributes.entry((class, site.attr.as_str())).or_default();
        if initializes_own_instance(function, local, class, site.obj_params) {
            match gathered.initializers.get_mut(function) {
                Some(a) => add(a, site.span),
                None => {
                    gathered.initializers.insert(function, access(site.span));
                }
            }
            continue;
        }
        // A local summary holds no `IndirectHelper` site: propagation makes those.
        let rebinds = site.kind.rebinds();
        let in_place = !rebinds;
        match gathered.writers.get_mut(function) {
            Some(w) => {
                add(&mut w.accesses, site.span);
                w.rebinds |= rebinds;
                w.in_place |= in_place;
                w.fresh &= site.fresh;
            }
            None => {
                gathered.writers.insert(
                    function,
                    Writes {
                        accesses: access(site.span),
                        rebinds,
                        in_place,
                        fresh: site.fresh,
                    },
                );
            }
        }
    }
    for site in local
        .typed_field_read_sites
        .iter()
        .filter(|s| s.counts_for_contracts())
    {
        let class = site.class_qualname.as_str();
        classes.entry(class).or_default().insert(module);
        let gathered = attributes.entry((class, site.attr.as_str())).or_default();
        match gathered.readers.get_mut(function) {
            Some(a) => add(a, site.span),
            None => {
                gathered.readers.insert(function, access(site.span));
            }
        }
    }
}

/// Whether a write by `function` (its own summary `local`) of an object that may be its
/// parameters `obj_params`, an instance of `class`, initializes its own instance: an initializer
/// of `class` writing its `self`, which the ownership contracts allow anyway.
fn initializes_own_instance(
    function: &FuncKey,
    local: &EffectSummary,
    class: &str,
    obj_params: u64,
) -> bool {
    let name = function
        .qualname
        .rsplit('.')
        .next()
        .unwrap_or(&function.qualname);
    obj_params & 1 != 0
        && INITIALIZERS.contains(&name)
        && local.owner_class_qualname.as_deref().is_some_and(|owner| {
            class
                .strip_prefix(function.module.as_str())
                .and_then(|rest| rest.strip_prefix('.'))
                == Some(owner)
        })
}
