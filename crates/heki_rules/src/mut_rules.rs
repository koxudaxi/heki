//! Family 1 rules: `MUT101`, `MUT106`, `MUT107`, `MUT108`, `MUT109`, `MUT110`, `MUT112`,
//! `MUT113`, `MUT119`, `MUT201`, `MUT202`, `MUT203`, `MUT204` (docs/ANALYSIS_AND_KB_SPEC.md
//! section 2.3).

use crate::premises::{self, OpenCause, Told};
use crate::{
    CheckContext, DottedPattern, FnSites, ModuleUnit, RuleConfigError, SitesByCaller, SitesBySpan,
    has_glob_syntax, split_last_dot,
};
use heki_ast::Span;
use heki_diagnostics::{Certainty, Diagnostic, OpenPremise, OpenRecord};
use heki_effect::readonly::{self, Derivation};
use heki_effect::{
    AttrKeyPath, BoundCallee, CacheEvent, CacheFlow, CallHop, CallSite, CallTarget, Callee,
    Construction, ContractClass, Delivery, EffectSummary, Facts, FuncKey, INITIALIZERS,
    ProjectDecls, ProjectIndex, ReturnStep, SelfAttrWrite, Shared, SiteRole, TypeCtx,
    TypedFieldWriteSite, Val, WriteKind, attr_writes, container_mutator, self_attr_writes,
};
use heki_env::HekiConfig;
use heki_ir::walk::{Answer, Budget, FnShape, Visit, reach};
use heki_ir::{BlockId, FnIndex, Inst, InstKind, IrFunction, VarId};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

// ==================================================================================== MUT101

/// The rules whose exact claims read the kept-value facts
/// ([`heki_effect::OptInFacts::kept_values`]).
pub(crate) const KEPT_VALUE_RULES: &[&str] = &["MUT101", "MUT111"];

/// Filter 1: initialization / two-phase initialization methods.
const INIT_METHODS: &[&str] = &[
    "__init__",
    "__post_init__",
    "model_post_init",
    "__setstate__",
    "__enter__",
    "__new__",
    "setup",
    "setUp",
    "postinit",
    "_open",
    "_setup",
];

/// A method of a project class. Its summaries and `MUT101` facts are in the index; a method of
/// a checked file also carries its IR, one per definition (only checked methods are reported).
struct Method<'a> {
    key: FuncKey,
    /// The file defining it.
    path: &'a Path,
    checked: Option<Checked<'a>>,
}

/// A method of a checked file: its unit and IR.
#[derive(Clone, Copy)]
struct Checked<'a> {
    unit: &'a ModuleUnit<'a>,
    f: &'a IrFunction,
}

impl Method<'_> {
    /// The method's own name (the last component of its qualname).
    fn name(&self) -> &str {
        self.key
            .qualname
            .rsplit('.')
            .next()
            .unwrap_or(&self.key.qualname)
    }

    /// The fqn of the class defining the method.
    fn class(&self) -> String {
        let owner = self.key.qualname.rsplit_once('.').map_or("", |(c, _)| c);
        format!("{}.{owner}", self.key.module)
    }
}

/// The instances `m` runs on are of a class with a metaclass other than `type` / `abc.ABCMeta`
/// ([`ProjectDecls::has_custom_metaclass`]): writing their attributes may run a descriptor the
/// metaclass installed (one that invalidates a cached property, say), which heki does not see,
/// so the write rules (`MUT101`, `MUT107`, and through them `MUT111`) stay quiet about them.
fn writes_may_run_descriptors(decls: &ProjectDecls, m: &Method<'_>) -> bool {
    decls.has_custom_metaclass(&m.class())
}

/// A private function or method name (`_name`, not a dunder), or a synthetic scope
/// (`<listcomp>`, `<genexpr>`, `<lambda>`, ...): neither is an API of its own, so either hands
/// its reports to its callers.
fn is_private(name: &str) -> bool {
    heki_effect::delegates_to_callers(name)
}

/// The methods of every project class, keyed by owner class fqn (nested classes stay
/// separate): from the IR of the checked files, and from the summaries of the other indexed
/// files.
struct ClassMethods<'a> {
    by_class: BTreeMap<String, Vec<Method<'a>>>,
}

impl<'a> ClassMethods<'a> {
    fn new(units: &'a [ModuleUnit<'a>], index: &'a ProjectIndex) -> Self {
        let mut by_class: BTreeMap<String, Vec<Method<'a>>> = BTreeMap::new();
        for unit in units {
            for f in &unit.input.ir.functions {
                if f.is_class_body || f.is_module_body {
                    continue;
                }
                let Some(owner) = &f.owner_class_qualname else {
                    continue;
                };
                by_class
                    .entry(format!("{}.{owner}", unit.input.module_name))
                    .or_default()
                    .push(Method {
                        key: CheckContext::key(unit, f),
                        path: &unit.input.path,
                        checked: Some(Checked { unit, f }),
                    });
            }
        }
        let checked: FxHashSet<&Path> = units.iter().map(|u| u.input.path.as_path()).collect();
        let mut others: Vec<(&FuncKey, &Path, &str)> = Vec::new();
        for (path, file) in &index.files {
            if checked.contains(path.as_path()) {
                continue;
            }
            others.extend(file.local_functions.iter().filter_map(|(key, s)| {
                let owner = s.owner_class_qualname.as_deref()?;
                (index.file_of(key) == Some(path.as_path())).then_some((key, path.as_path(), owner))
            }));
        }
        others.sort();
        for (key, path, owner) in others {
            by_class
                .entry(format!("{}.{owner}", key.module))
                .or_default()
                .push(Method {
                    key: key.clone(),
                    path,
                    checked: None,
                });
        }
        Self { by_class }
    }

    /// The family of `class`: its own methods, those of every project subclass, and the methods
    /// it inherits from its project ancestors that no class before them in the MRO overrides
    /// (they run on its instances too). A family spans modules: what a checked method is
    /// reported for does not depend on which other files are checked with it (the language
    /// server checks one).
    fn family(&self, decls: &ProjectDecls, class: &str) -> Vec<&Method<'a>> {
        let methods = self.by_class.get(class).into_iter().flatten();
        let mut family: Vec<&Method<'a>> = methods.clone().collect();
        for sub in decls.subclasses_of(class) {
            family.extend(self.by_class.get(&sub).into_iter().flatten());
        }
        let mut defined: FxHashSet<&str> = methods.map(Method::name).collect();
        for ancestor in decls.ancestors_of(class) {
            let inherited = self.by_class.get(&ancestor).into_iter().flatten();
            family.extend(inherited.clone().filter(|m| !defined.contains(m.name())));
            defined.extend(inherited.map(Method::name));
        }
        family
    }
}

/// Visit every project class whose family has checked methods with its own methods, its
/// family ([`ClassMethods::family`]), and the private methods its family's `__init__`s call on
/// `self`.
fn for_each_class_family<'a>(
    cx: &CheckContext<'a>,
    mut visit: impl FnMut(&[Method<'a>], &[&Method<'a>], &FxHashSet<FuncKey>),
) {
    let classes = ClassMethods::new(cx.units, cx.index);
    for (class, methods) in &classes.by_class {
        let family = classes.family(cx.decls, class);
        if family.iter().all(|m| m.checked.is_none()) {
            continue;
        }
        let init_called = private_methods_called_from_init(cx.index, &family);
        visit(methods, &family, &init_called);
    }
}

/// The method's own (local) summary: one write is one report, where it happens, so the
/// writes merged up from the helpers it calls do not count.
fn local_summary<'a>(index: &'a ProjectIndex, m: &Method<'_>) -> Option<&'a EffectSummary> {
    index
        .files
        .get(m.path)
        .and_then(|f| f.local_functions.get(&m.key))
        .map(|s| &**s)
}

/// Disjoint constant dict keys: every read and every write names a constant key, and no key
/// is both read and written (`self.params["outtmpl"]` vs `self.params["proxy"]`).
fn disjoint_keys<'k>(
    reads: impl IntoIterator<Item = &'k Option<String>>,
    writes: impl IntoIterator<Item = &'k Option<String>>,
) -> bool {
    let k_read: Option<BTreeSet<&String>> = reads.into_iter().map(Option::as_ref).collect();
    let k_write: Option<BTreeSet<&String>> = writes.into_iter().map(Option::as_ref).collect();
    matches!((k_read, k_write), (Some(kr), Some(kw)) if kr.is_disjoint(&kw))
}

/// A write that leaves a `@cached_property` stale: `writer` changes `self.<attr>`, which the
/// cached property `prop` reads, at `span` (its first such write), without invalidating it.
struct StaleWrite<'m, 'a> {
    prop: &'m Method<'a>,
    writer: &'m Method<'a>,
    attr: &'a str,
    span: Span,
    /// The write establishes every premise of `MUT101`'s exact claim ([`kept_values`]).
    exact: bool,
}

/// The writes of `family` that leave a `@cached_property` of the class (`methods`) stale, in
/// the order `MUT101` reports them, among the family members `writers` admits. Decided from the
/// index alone ([`heki_effect::CacheFlow`]), so a writer in a file that is not checked counts
/// too.
fn stale_writes<'m, 'a>(
    index: &'a ProjectIndex,
    decls: &ProjectDecls,
    methods: &'m [Method<'a>],
    family: &[&'m Method<'a>],
    init_called: &FxHashSet<FuncKey>,
    writers: impl Fn(&Method<'_>) -> bool,
    mut found: impl FnMut(StaleWrite<'m, 'a>),
) {
    for p in methods {
        let Some(ps) = index.summary(&p.key) else {
            continue;
        };
        if !ps.is_cached_property || writes_may_run_descriptors(decls, p) {
            continue;
        }
        let p_name = p.name();
        let mut reads: BTreeMap<&str, Vec<&Option<String>>> = BTreeMap::new();
        for r in &ps.reads_self_attrs {
            if r.attr != p_name {
                reads.entry(&r.attr).or_default().push(&r.const_key);
            }
        }
        // A checked file has one method per definition, and the facts of a method cover all
        // of its definitions in the file.
        let mut visited: FxHashSet<(&Path, &FuncKey)> = FxHashSet::default();
        for &m in family {
            if !writers(m) || !visited.insert((m.path, &m.key)) {
                continue;
            }
            if m.key == p.key
                || filter1_init(m, init_called)
                || writes_may_run_descriptors(decls, m)
            {
                continue;
            }
            let Some(ms) = index.summary(&m.key) else {
                continue;
            };
            let flows = index
                .files
                .get(m.path)
                .and_then(|f| f.cache_flows.get(&m.key))
                .map_or(&[][..], Vec::as_slice);
            // Filter 4: the method invalidates the cache after the write on every path.
            let invalidations: Vec<Option<CacheEvents>> = flows
                .iter()
                .map(|flow| {
                    invalidates(ms, p_name).then(|| CacheEvents::of(index, m, flow, p_name))
                })
                .collect();
            let Some(local) = local_summary(index, m) else {
                continue;
            };
            // A private helper whose every caller invalidates the property is covered.
            let covered_by_callers = is_private(m.name())
                && index.reverse_call_graph.get(&m.key).is_some_and(|callers| {
                    !callers.is_empty()
                        && callers
                            .iter()
                            .all(|c| index.summary(c).is_some_and(|cs| invalidates(cs, p_name)))
                });
            if covered_by_callers {
                continue;
            }
            let receiver = receiver_class(decls, m, p);
            let calls = std::cell::OnceCell::new();
            let mut kept: Vec<Option<kept_values::KeptAfterWrites<'_>>> =
                flows.iter().map(|_| None).collect();
            for (&attr, read_keys) in &reads {
                let writes: Vec<&AttrKeyPath> = local
                    .mutates_self_attrs
                    .iter()
                    .filter(|w| w.attr == attr)
                    .collect();
                // The write is a plain store.
                if writes.is_empty() || decls.attr_write_runs_code(&receiver, attr) {
                    continue;
                }
                let first = flows.iter().zip(&invalidations).enumerate().find_map(
                    |(fi, (flow, points))| {
                        // Filter 2: memo / cache dictionaries.
                        let mut own: Vec<&SelfAttrWrite> = flow
                            .writes_of(attr)
                            .filter(|w| !w.fills_memo(attr))
                            .collect();
                        // Computing the cached value initialized the attribute (the property reads
                        // it only where it is initialized lazily), so a lazy initialization cannot
                        // find it unset after.
                        if ps.lazily_read_self_attrs.contains(attr) {
                            own.retain(|w| !w.lazy_init);
                        }
                        // The cached value holds the attribute's object itself: it sees the
                        // changes made in place, and only rebinding the attribute leaves it behind.
                        if ps.returns_self_attrs.contains(attr) {
                            own.retain(|w| w.rebinds);
                        }
                        if let Some(points) = points {
                            own.retain(|w| !points.cover(flow, w.block, w.index));
                        }
                        own.first().map(|w| (fi, *w))
                    },
                );
                let Some((fi, write)) = first else {
                    continue;
                };
                // Filter 3: disjoint constant dict keys.
                if disjoint_keys(
                    read_keys.iter().copied(),
                    writes.iter().map(|w| &w.const_key),
                ) {
                    continue;
                }
                let kept = kept[fi].get_or_insert_with(|| {
                    let calls = calls.get_or_init(|| kept_values::calls_by_span(index, m));
                    kept_values::KeptAfterWrites::of(index, calls, &flows[fi], p_name)
                });
                let exact =
                    kept_values::established(index, decls, &m.key, p_name, attr, write, kept);
                found(StaleWrite {
                    prop: p,
                    writer: m,
                    attr,
                    span: write.span,
                    exact,
                });
            }
        }
    }
}

/// `MUT101` (writes that leave a `@cached_property` stale) and `MUT111` (opt-in: the functions
/// reading such a property), from one analysis of the class families.
pub(crate) fn mut101_mut111(cx: &CheckContext<'_>, out: &mut Vec<Diagnostic>) {
    let report_writes = cx.selection.is_enabled("MUT101");
    let report_reads = cx.selection.is_enabled("MUT111");
    if !report_writes && !report_reads {
        return;
    }
    let classes = ClassMethods::new(cx.units, cx.index);
    let reads = if report_reads {
        stale_reads::CachedReads::of(cx)
    } else {
        stale_reads::CachedReads::default()
    };
    let read_classes = reads.property_classes();
    let mut seen = FxHashSet::default();
    let mut stale: FxHashMap<&FuncKey, Vec<StaleWrite<'_, '_>>> = FxHashMap::default();
    for (class, methods) in &classes.by_class {
        let family = classes.family(cx.decls, class);
        let reported = report_writes && family.iter().any(|m| m.checked.is_some());
        let read = read_classes.contains(class);
        if !reported && !read {
            continue;
        }
        let init_called = private_methods_called_from_init(cx.index, &family);
        let writers = |m: &Method<'_>| read || m.checked.is_some();
        stale_writes(
            cx.index,
            cx.decls,
            methods,
            &family,
            &init_called,
            writers,
            |w| {
                // An overriding `@cached_property` shares the instance `__dict__` slot: one report
                // per property name.
                if let Some(Checked { unit, .. }) = w.writer.checked
                    && reported
                    && seen.insert((w.writer.key.clone(), w.attr, w.prop.name().to_owned()))
                {
                    let p_name = w.prop.name();
                    let class_qualname = cx
                        .index
                        .summary(&w.prop.key)
                        .and_then(|ps| ps.owner_class_qualname.as_deref())
                        .unwrap_or_default();
                    if let Some(d) = cx.emit(
                    out,
                    "MUT101",
                    unit,
                    w.span,
                    &w.writer.key.qualname,
                    format!("{class_qualname}.{}->{p_name}", w.attr),
                    format!(
                        "`{}` mutates `self.{}`, read by `@cached_property` `{p_name}`, without invalidating it",
                        w.writer.key.qualname, w.attr
                    ),
                    Some(format!(
                        "invalidate it after the change with `del self.{p_name}`, or `self.__dict__.pop(\"{p_name}\", None)` when it may not be computed yet"
                    )),
                    &[],
                ) {
                        d.certainty = crate::premises::tier("MUT101", w.exact).at_most(d.certainty);
                    }
                }
                if read {
                    stale.entry(&w.prop.key).or_default().push(w);
                }
            },
        );
    }
    if report_reads {
        reads.report(cx, &stale, out);
    }
}

/// A write `MUT101` finds, whatever is selected: the cached property, the writer, the attribute,
/// the write, and the project classes whose instances run the writer (`MUT111` reports the
/// readers of their instances).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StaleWriteKey {
    pub prop: FuncKey,
    pub writer: FuncKey,
    pub attr: String,
    pub span: Span,
    pub running: BTreeSet<String>,
    /// The write's `MUT101` report is exact (a reader's `MUT111` tier reads it).
    pub exact: bool,
}

/// The writes `MUT101` finds in the methods of the files `wanted` admits, by file, whatever is
/// selected. The language server re-checks the readers of a property (`MUT111`) when the writes
/// of a file change.
pub fn stale_writes_by_file(
    index: &ProjectIndex,
    decls: &ProjectDecls,
    wanted: impl Fn(&Path) -> bool,
) -> FxHashMap<PathBuf, BTreeSet<StaleWriteKey>> {
    let classes = ClassMethods::new(&[], index);
    let mut out: FxHashMap<PathBuf, BTreeSet<StaleWriteKey>> = FxHashMap::default();
    for (class, methods) in &classes.by_class {
        let family = classes.family(decls, class);
        if !family.iter().any(|m| wanted(m.path)) {
            continue;
        }
        let init_called = private_methods_called_from_init(index, &family);
        let writers = |m: &Method<'_>| wanted(m.path);
        stale_writes(index, decls, methods, &family, &init_called, writers, |w| {
            out.entry(w.writer.path.to_path_buf())
                .or_default()
                .insert(StaleWriteKey {
                    prop: w.prop.key.clone(),
                    writer: w.writer.key.clone(),
                    attr: w.attr.to_owned(),
                    span: w.span,
                    running: stale_reads::classes_running(
                        index,
                        decls,
                        &w.writer.key,
                        w.prop.name(),
                    ),
                    exact: w.exact,
                });
        });
    }
    out
}

/// Whether a function's summary says that it invalidates the cached property `prop` somewhere
/// (`del self.prop`, `self.__dict__.pop("prop")`, `self.__dict__.clear()`, also through its
/// `self` calls).
fn invalidates(s: &EffectSummary, prop: &str) -> bool {
    s.invalidates_cached.contains(prop) || s.invalidates_cached.contains("*")
}

/// Where a method invalidates the cached property `prop` of its own instance, and where it reads
/// it (computing the value again): instructions as `(block, index)`.
struct CacheEvents {
    /// `del self.prop`, removing `"prop"` from `self.__dict__` (`pop`, `del`, `clear`), and calls
    /// on `self` of methods whose summary invalidates it.
    invalidations: FxHashSet<(BlockId, u32)>,
    /// `self.prop`, and calls on `self` of methods whose summary reads it.
    reads: FxHashSet<(BlockId, u32)>,
    /// Per block, whether every normal path from its start to a return passes an invalidation.
    covered: Vec<bool>,
    /// Per block, whether every path from the entry to its start passes an invalidation with no
    /// read of the property after it.
    absent: Vec<bool>,
}

/// The last of a block's events that invalidates the property or reads it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LastEvent {
    Invalidates,
    Reads,
}

impl CacheEvents {
    fn of(index: &ProjectIndex, m: &Method<'_>, flow: &CacheFlow, prop: &str) -> Self {
        let file = index.files.get(m.path);
        // What the `self` calls at `span` do to the property: `(invalidate, read)`.
        let self_call = |span: Span| {
            let callees = file
                .into_iter()
                .flat_map(|file| &file.call_sites)
                .filter(|site| site.span == span && site.caller == m.key && site.recv_is_self)
                .flat_map(|site| site.project_callees())
                .filter_map(|bc| index.summary(&bc.callee));
            callees.fold((false, false), |(inv, read), s| {
                (
                    inv || invalidates(s, prop),
                    read || s.reads_self_attrs.iter().any(|r| r.attr == prop),
                )
            })
        };
        let mut out = Self {
            invalidations: FxHashSet::default(),
            reads: FxHashSet::default(),
            covered: Vec::new(),
            absent: Vec::new(),
        };
        for (b, block) in flow.blocks.iter().enumerate() {
            let b = BlockId(u32::try_from(b).unwrap_or(u32::MAX));
            for &(index, event) in &block.events {
                let (invalidates, reads) = match event {
                    CacheEvent::DeleteAttr(name) | CacheEvent::RemoveEntry(name) => {
                        (flow.name(name) == prop, false)
                    }
                    CacheEvent::LoadAttr(name) => (false, flow.name(name) == prop),
                    CacheEvent::ClearDict => (true, false),
                    CacheEvent::SelfCall(span) => self_call(span),
                    // Points that may remove the value are no invalidation the code states.
                    CacheEvent::MayRemove(_) | CacheEvent::MayRemoveAny | CacheEvent::Calls(_) => {
                        (false, false)
                    }
                };
                if invalidates {
                    out.invalidations.insert((b, index));
                } else if reads {
                    out.reads.insert((b, index));
                }
            }
        }
        let budget = Budget::unmetered("mut_rules::CacheEvents");
        // Open: every write is covered, and none is reported.
        let n = flow.blocks.len();
        out.covered = out.covered(flow, &budget).open_means(vec![true; n]);
        out.absent = out.absent(flow, &budget).open_means(vec![true; n]);
        out
    }

    /// The last event of `b` (before instruction `upto`) that invalidates the property or reads
    /// it.
    fn last_event(&self, flow: &CacheFlow, b: BlockId, upto: Option<u32>) -> Option<LastEvent> {
        let events = flow.blocks[b.0 as usize].events.iter().map(|(i, _)| *i);
        (events.filter(|i| upto.is_none_or(|u| *i < u)))
            .filter_map(|i| {
                if self.invalidations.contains(&(b, i)) {
                    Some(LastEvent::Invalidates)
                } else {
                    self.reads.contains(&(b, i)).then_some(LastEvent::Reads)
                }
            })
            .next_back()
    }

    /// [`Self::covered`] of every block. A block is not covered when it invalidates nothing and
    /// returns, or has a successor not covered: the blocks a return reaches backward through
    /// blocks that invalidate nothing.
    fn covered(&self, flow: &CacheFlow, budget: &Budget) -> Answer<Vec<bool>> {
        let n = flow.blocks.len();
        let block = |i: usize| BlockId(u32::try_from(i).unwrap_or(u32::MAX));
        let invalidates: Vec<bool> = (0..n)
            .map(|i| {
                flow.blocks[i]
                    .events
                    .iter()
                    .any(|(k, _)| self.invalidations.contains(&(block(i), *k)))
            })
            .collect();
        let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (i, b) in flow.blocks.iter().enumerate() {
            for s in &b.succs {
                preds[s.0 as usize].push(i);
            }
        }
        let mut covered = vec![true; n];
        let returns = (0..n).filter(|i| flow.blocks[*i].returns && !invalidates[*i]);
        let back = |i: usize, out: &mut Vec<usize>| {
            out.extend(preds[i].iter().filter(|p| !invalidates[**p]));
        };
        reach(budget, returns, back, |i| {
            covered[i] = false;
            Visit::<()>::Expand
        })
        .map(|_| covered)
    }

    /// [`Self::absent`] of every block. The value is present at the start of the entry, of a
    /// handler (an exception may come from anywhere), and of a successor of a block reading it
    /// last, and present on through blocks that neither invalidate nor read it; a block no path
    /// enters keeps it absent.
    fn absent(&self, flow: &CacheFlow, budget: &Budget) -> Answer<Vec<bool>> {
        let n = flow.blocks.len();
        let block = |i: usize| BlockId(u32::try_from(i).unwrap_or(u32::MAX));
        let reachable = reachable_blocks(flow);
        let last: Vec<Option<LastEvent>> = (0..n)
            .map(|i| self.last_event(flow, block(i), None))
            .collect();
        let mut starts: Vec<usize> = vec![0];
        for (i, b) in flow.blocks.iter().enumerate() {
            starts.extend(b.exc_target.map(|h| h.0 as usize));
            if last[i] == Some(LastEvent::Reads) {
                starts.extend(b.succs.iter().map(|s| s.0 as usize));
            }
        }
        starts.retain(|i| reachable.get(*i).copied().unwrap_or(false));
        let mut absent = vec![true; n];
        let on = |i: usize, out: &mut Vec<usize>| {
            if last[i].is_none() {
                out.extend(flow.blocks[i].succs.iter().map(|s| s.0 as usize));
            }
        };
        reach(budget, starts, on, |i| {
            absent[i] = false;
            Visit::<()>::Expand
        })
        .map(|_| absent)
    }

    /// Whether the write at instruction `index` of `block` leaves no stale value behind on any
    /// normal path: the property is invalidated on every path after it, or its value is gone
    /// on every path to it (invalidated, and not read again since).
    fn cover(&self, flow: &CacheFlow, block: BlockId, index: u32) -> bool {
        self.invalidated_after(flow, block, index) || self.absent_before(flow, block, index)
    }

    /// Every normal path from just after the instruction to a return passes an invalidation.
    /// Raising paths carry no obligation (as for `MUT109`), and a loop that never returns adds
    /// none.
    fn invalidated_after(&self, flow: &CacheFlow, block: BlockId, index: u32) -> bool {
        let b = &flow.blocks[block.0 as usize];
        let later = b.events.iter().map(|(i, _)| *i).filter(|i| *i > index);
        later
            .into_iter()
            .any(|i| self.invalidations.contains(&(block, i)))
            || (!b.returns && b.succs.iter().all(|s| self.covered[s.0 as usize]))
    }

    /// Every path from the entry to the instruction passes an invalidation with no read of the
    /// property after it: the cached value is gone when the write happens. A block an exception
    /// enters may hold any value, and a block no path enters keeps its first value.
    fn absent_before(&self, flow: &CacheFlow, block: BlockId, index: u32) -> bool {
        match self.last_event(flow, block, Some(index)) {
            Some(last) => last == LastEvent::Invalidates,
            None => self.absent[block.0 as usize],
        }
    }
}

/// The blocks of a method a path from its entry enters, through normal and exception edges.
fn reachable_blocks(flow: &CacheFlow) -> Vec<bool> {
    let mut reachable = vec![false; flow.blocks.len()];
    let mut stack = vec![BlockId(0)];
    while let Some(b) = stack.pop() {
        let i = b.0 as usize;
        if i >= reachable.len() || reachable[i] {
            continue;
        }
        reachable[i] = true;
        let block = &flow.blocks[i];
        stack.extend(block.succs.iter().copied().chain(block.exc_target));
    }
    reachable
}

fn filter1_init(m: &Method<'_>, init_called: &FxHashSet<FuncKey>) -> bool {
    let name = m.name();
    INIT_METHODS.contains(&name)
        || ["_init_", "_configure_", "setup_", "_setup_"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
        || (is_private(name) && init_called.contains(&m.key))
}

/// The project classes whose methods' `MUT101` / `MUT107` / `MUT112` / `MUT113` reports read
/// the summaries of the methods `changed` (a class family spans modules, see
/// [`ClassMethods::family`]): a changed `@cached_property` or `__hash__` (or a removed method) is
/// read for its own family, any changed method for the families of its ancestors and subclasses
/// that have either (the calls of every method decide which private methods only initialization
/// runs), a method overriding an inherited one for its own family, and any method for its own
/// family when a class of it has a generator suspending inside an iteration of its own containers
/// (`MUT113` reports the generator with the methods changing them, and with what its callers run
/// while it is suspended: the callees of a changed function, and the `callees` a function whose
/// call sites changed had before or has after the change, are read for the families of the
/// generators whose objects their calls may hand on, also through the functions handing them on in
/// turn). With `in_place_changes` (`MUT112` selected), every changed method is read for its own
/// family: a store in `self.<attr>` is reported when a method of the family changes the attribute
/// in place. A family's classes include the ancestors whose methods it inherits. A changed writer
/// is otherwise reported in its own file only.
pub fn family_readers(
    index: &ProjectIndex,
    decls: &ProjectDecls,
    changed: &[FuncKey],
    callees: &[&FuncKey],
    in_place_changes: bool,
) -> BTreeSet<String> {
    let has_reported_reads = |class: &String| {
        decls.methods_of(class).iter().any(|k| {
            k.qualname.ends_with(".__hash__")
                || index.summary(k).is_some_and(|s| s.is_cached_property)
        })
    };
    // Each class's methods are read once, whatever number of its methods changed.
    let mut suspending: FxHashMap<String, bool> = FxHashMap::default();
    let mut has_suspending_generator = |class: &String| {
        *suspending.entry(class.clone()).or_insert_with(|| {
            decls
                .methods_of(class)
                .iter()
                .any(|k| iterated_mutation::suspends(index, k))
        })
    };
    // Per changed class: the names of the methods its ancestors define, whether a class of its
    // family has a suspending generator, and the classes of its family with reported reads.
    let mut families: FxHashMap<String, (FxHashSet<String>, bool, Vec<String>)> =
        FxHashMap::default();
    let methods = changed.iter().filter_map(|key| {
        let (owner, name) = key.qualname.rsplit_once('.')?;
        let class = format!("{}.{owner}", key.module);
        decls.is_project_class(&class).then_some((key, class, name))
    });
    let mut roots = BTreeSet::new();
    for generator in iterated_mutation::generators_reached(index, changed, callees) {
        if let Some((owner, _)) = generator.qualname.rsplit_once('.') {
            roots.insert(format!("{}.{owner}", generator.module));
        }
    }
    for (key, class, name) in methods {
        if in_place_changes
            || name == "__hash__"
            || index.summary(key).is_none_or(|s| s.is_cached_property)
        {
            roots.insert(class.clone());
        }
        let (inherited, suspending, reported) =
            families.entry(class.clone()).or_insert_with(|| {
                let ancestors = decls.ancestors_of(&class);
                let inherited = ancestors
                    .iter()
                    .flat_map(|a| decls.methods_of(a))
                    .filter_map(|k| k.qualname.rsplit('.').next().map(str::to_owned))
                    .collect();
                let family: Vec<String> = ancestors
                    .into_iter()
                    .chain(decls.subclasses_of(&class))
                    .chain([class.clone()])
                    .collect();
                let suspending = family.iter().any(&mut has_suspending_generator);
                let reported = family.into_iter().filter(has_reported_reads).collect();
                (inherited, suspending, reported)
            });
        // A method that overrides an inherited one (or stops doing so) changes which of the
        // ancestors' methods the class family inherits.
        if inherited.contains(name) || *suspending {
            roots.insert(class);
        }
        // Initialization, and the private helpers it alone calls, write nothing a cached
        // property or a hash reads.
        roots.extend(reported.iter().cloned());
    }
    let mut out = BTreeSet::new();
    for root in roots {
        out.extend(decls.subclasses_of(&root));
        // The methods a family inherits are reported in their ancestors' files.
        for class in std::iter::once(root.clone()).chain(decls.subclasses_of(&root)) {
            out.extend(decls.ancestors_of(&class));
        }
        out.insert(root);
    }
    out
}

/// Private methods `__init__` of the class family calls on `self` directly, and the private
/// methods only initialization runs through them: each project caller of such a method is an
/// `__init__` of the family or another one (`__init__` -> `self._load()` -> `self._decode()`).
fn private_methods_called_from_init(
    index: &ProjectIndex,
    family: &[&Method<'_>],
) -> FxHashSet<FuncKey> {
    let inits: Vec<&Method<'_>> = family
        .iter()
        .copied()
        .filter(|m| m.name() == "__init__")
        .collect();
    let self_callees = |path: &Path, caller: &FuncKey| -> Vec<FuncKey> {
        index
            .files
            .get(path)
            .into_iter()
            .flat_map(|f| &f.call_sites)
            .filter(|s| s.caller == *caller && s.recv_is_self)
            .flat_map(|s| s.project_callees())
            .filter(|c| is_private(&c.callee.qualname))
            .map(|c| c.callee.clone())
            .collect()
    };
    let mut out: FxHashSet<FuncKey> = inits
        .iter()
        .flat_map(|init| self_callees(init.path, &init.key))
        .collect();
    let mut only_init: FxHashSet<FuncKey> = inits.iter().map(|m| m.key.clone()).collect();
    let mut work: Vec<(&Path, FuncKey)> = inits.iter().map(|m| (m.path, m.key.clone())).collect();
    while let Some((path, caller)) = work.pop() {
        for callee in self_callees(path, &caller) {
            let runs_only_there = !only_init.contains(&callee)
                && index
                    .reverse_call_graph
                    .get(&callee)
                    .is_some_and(|callers| callers.iter().all(|k| only_init.contains(k)));
            if runs_only_there && let Some(p) = index.file_of(&callee) {
                only_init.insert(callee.clone());
                out.insert(callee.clone());
                work.push((p, callee));
            }
        }
    }
    out
}

mod kept_values;
mod lost_changes;
mod sentinel_copies;
mod stale_reads;

pub use kept_values::{kept_removals_in, lazy_setup_files};
pub(crate) use lost_changes::mut119;
pub(crate) use sentinel_copies::mut118;
pub use sentinel_copies::{SentinelTestKey, deep_copy_files, sentinel_tests_in};

// ==================================================================================== MUT201

/// A `Class.attr` contract target (`write-owners`, `read-owners`, `effect-boundaries`), or
/// an entry of the container an attribute holds (`Class.attr["key"]`, write targets only).
pub(crate) struct ContractTarget {
    /// The configured text (diagnostic target and baseline key).
    pub text: String,
    attr: DottedPattern,
    /// The entry key of an item target.
    pub key: Option<String>,
    /// Project classes (fqn) the class pattern names, plus all their subclasses: the
    /// target's class family.
    pub classes: FxHashSet<String>,
}

impl ContractTarget {
    pub fn matches(&self, class: &str, attr: &str) -> bool {
        self.classes.contains(class) && self.attr.matches(attr)
    }

    /// Whether the attribute pattern matches `attr`, whatever the class.
    pub fn matches_attr(&self, attr: &str) -> bool {
        self.attr.matches(attr)
    }

    /// Whether `site` writes the target: its class and attribute, and for an item target the
    /// entry with its key (a write of the whole container, or with a computed key, writes the
    /// attribute only).
    pub fn matches_write(&self, site: &TypedFieldWriteSite) -> bool {
        self.matches(&site.class_qualname, &site.attr)
            && self
                .key
                .as_ref()
                .is_none_or(|k| site.key.as_ref() == Some(k))
    }
}

/// The targets of one contract `site` writes (by index): the item targets naming the entry it
/// changes when there are any, else the attribute targets, so a write of one entry is reported
/// once, under its most specific target.
pub(crate) fn write_hits<'t>(
    targets: &'t [ContractTarget],
    site: &TypedFieldWriteSite,
) -> Vec<(usize, &'t ContractTarget)> {
    let hits: Vec<(usize, &ContractTarget)> = targets
        .iter()
        .enumerate()
        .filter(|(_, t)| t.matches_write(site))
        .collect();
    if hits.iter().any(|(_, t)| t.key.is_some()) {
        hits.into_iter().filter(|(_, t)| t.key.is_some()).collect()
    } else {
        hits
    }
}

/// `Class.attr["key"]` / `Class.attr['key']` -> (`Class.attr`, `key`).
pub(crate) fn split_item(text: &str) -> Option<(&str, &str)> {
    let quote = match text.as_bytes() {
        [.., q @ (b'"' | b'\''), b']'] => char::from(*q),
        _ => return None,
    };
    let open = format!("[{quote}");
    let start = text.rfind(&open)?;
    let key = text.get(start + 2..text.len() - 2)?;
    (!key.contains(quote)).then_some((&text[..start], key))
}

/// Every project class, with its qualname, sorted by fqn.
pub(crate) fn project_classes(cx: &CheckContext<'_>) -> Vec<ContractClass> {
    let mut all: Vec<ContractClass> = cx
        .index
        .files
        .values()
        .flat_map(|f| {
            f.decls.classes.keys().map(move |q| ContractClass {
                fqn: format!("{}.{q}", f.module_name),
                qualname: Some(q.clone()),
            })
        })
        .collect();
    all.sort_by(|a, b| a.fqn.cmp(&b.fqn));
    all
}

/// The project classes (fqn) of `all` that `pattern` names, plus all their subclasses when
/// `subclasses` covers them.
fn class_set(
    cx: &CheckContext<'_>,
    pattern: &DottedPattern,
    all: &[ContractClass],
    subclasses: Subclasses,
) -> FxHashSet<String> {
    let mut out = FxHashSet::default();
    for c in all.iter().filter(|c| pattern.matches_class(c)) {
        out.insert(c.fqn.clone());
        if subclasses == Subclasses::Covered {
            out.extend(cx.decls.subclasses_of(&c.fqn));
        }
    }
    out
}

/// Parse `Class.attr` (and `Class.attr["key"]`) targets: a target without a dot is
/// `InvalidTarget { section }`, a bad glob `Glob { context }`.
pub(crate) fn parse_targets(
    cx: &CheckContext<'_>,
    all: &[ContractClass],
    targets: &[String],
    section: &'static str,
    context: &'static str,
) -> Result<Vec<ContractTarget>, RuleConfigError> {
    let mut out = Vec::new();
    for t in targets {
        let (base, key) = match split_item(t) {
            Some((base, key)) => (base, Some(key.to_owned())),
            None => (t.as_str(), None),
        };
        let Some((class, attr)) = split_last_dot(base) else {
            return Err(RuleConfigError::InvalidTarget {
                section,
                target: t.clone(),
            });
        };
        let class = DottedPattern::new(class, context)?;
        out.push(ContractTarget {
            text: t.clone(),
            attr: DottedPattern::new(attr, context)?,
            key,
            classes: class_set(cx, &class, all, Subclasses::Covered),
        });
    }
    Ok(out)
}

/// [`parse_targets`] for reads: an item target is `ItemTarget { section }` (a read site is an
/// attribute read).
pub(crate) fn parse_read_targets(
    cx: &CheckContext<'_>,
    all: &[ContractClass],
    targets: &[String],
    section: &'static str,
    context: &'static str,
) -> Result<Vec<ContractTarget>, RuleConfigError> {
    let out = parse_targets(cx, all, targets, section, context)?;
    if let Some(t) = out.iter().find(|t| t.key.is_some()) {
        return Err(RuleConfigError::ItemTarget {
            section,
            target: t.text.clone(),
        });
    }
    Ok(out)
}

pub(crate) fn patterns(
    texts: &[String],
    context: &'static str,
) -> Result<Vec<DottedPattern>, RuleConfigError> {
    texts
        .iter()
        .map(|p| DottedPattern::new(p, context))
        .collect()
}

/// Whether a pattern names `key` by its fully qualified name or its qualname.
fn names(patterns: &[DottedPattern], key: &FuncKey) -> bool {
    let fqn = key.fqn();
    patterns.iter().any(|p| p.matches_def(&fqn, &key.qualname))
}

/// The functions a contract lets touch its targets: `allowed-*` patterns (matched against the
/// fqn and the qualname) and `exempt-methods` (a plain name matches the last qualname
/// component; an entry with a dot or a glob is a pattern like `allowed-*`). `forbidden-*`
/// patterns (matched like `allowed-*`) narrow where the contract applies: when set, only
/// accesses by them are reported, and the allowed functions stay exceptions inside them.
#[derive(Debug, Default)]
pub(crate) struct Allowance {
    allowed: Vec<DottedPattern>,
    exempt_names: FxHashSet<String>,
    /// `exempt-methods` entries with a dot or a glob.
    exempt_patterns: Vec<DottedPattern>,
    forbidden: Vec<DottedPattern>,
    /// Functions the contract guards (`call-owners` targets): they are part of the guarded
    /// operation, whose calls are checked where they are made, so they may use each other.
    targets: Option<std::sync::Arc<FunctionMatchers>>,
    /// Which function these patterns are matched against for an access.
    attribution: Attribution,
    /// `own-receiver = "allow"`: an access through the receiver of the method judging it is
    /// allowed ([`Self::owns`]).
    own_receiver: bool,
    /// The project protocols `allowed-protocols` names, with their subclasses unless
    /// `protocol-subclasses = false` ([`Self::allows_protocol`]).
    protocols: FxHashSet<String>,
    /// With `unresolved = "report"`: the functions some code hands on as values
    /// ([`CheckContext::escaping_functions`]), which have callers heki does not see. Each judges
    /// its accesses itself, as if it had no callers ([`delegated_reports`]).
    escaping: Option<std::sync::Arc<FxHashSet<FuncKey>>>,
}

/// Project functions a `call-owners` / `call-targets` entry names: the whole text by fqn or
/// qualname (dotted suffixes, globs), and, for `Class.method`, the overrides of the method in
/// the class family the class part names. The last segment of the text names the function,
/// method, or class: a glob there matches within that name only (`pkg.*._*` names the private
/// members of everything in `pkg`, not every function of a private module), as the attribute of
/// a `Class.attr` target does. A text with a module part (`pkg:_*`) is anchored at its module
/// already: its qualname part is matched whole.
#[derive(Debug, Clone)]
pub(crate) struct FunctionMatcher {
    whole: DottedPattern,
    /// The text split at its last dot when the last segment is a glob and the text has no module
    /// part: what holds the member (none for a text without a dot), and the member's name.
    member: Option<(Option<DottedPattern>, DottedPattern)>,
    method: Option<(FxHashSet<String>, DottedPattern)>,
    /// The text names nothing outside the project ([`Self::names_external`]): the target had an
    /// argument list, which heki binds only for project functions.
    project_only: bool,
}

impl FunctionMatcher {
    pub(crate) fn new(
        cx: &CheckContext<'_>,
        all_classes: &[ContractClass],
        text: &str,
        context: &'static str,
    ) -> Result<Self, RuleConfigError> {
        let mut out = Self::named(text, context)?;
        if let Some((holder, last)) = split_last_dot(text) {
            let holder = DottedPattern::new(holder, context)?;
            let owners = class_set(cx, &holder, all_classes, Subclasses::Covered);
            if !owners.is_empty() {
                out.method = Some((owners, DottedPattern::new(last, context)?));
            }
        }
        Ok(out)
    }

    /// The matcher of `text` without the methods of a class family: what names a symbol
    /// outside the project ([`Self::names_external`]).
    pub(crate) fn named(text: &str, context: &'static str) -> Result<Self, RuleConfigError> {
        // The whole text first, so that a bad glob is reported as configured.
        let whole = DottedPattern::new(text, context)?;
        let (holder, last) = match split_last_dot(text) {
            Some((holder, last)) => (Some(DottedPattern::new(holder, context)?), last),
            None => (None, text),
        };
        let member = if whole.module().is_none() && has_glob_syntax(last) {
            Some((holder, DottedPattern::new(last, context)?))
        } else {
            None
        };
        Ok(Self {
            whole,
            member,
            method: None,
            project_only: false,
        })
    }

    /// The same matcher, naming project functions and classes only.
    pub(crate) fn project_only(self) -> Self {
        Self {
            project_only: true,
            ..self
        }
    }

    pub(crate) fn whole(&self) -> &DottedPattern {
        &self.whole
    }

    /// Whether the whole text names a project function or class (`fqn` is `module.qualname`).
    /// A glob in the last segment matches the last name only, and the rest of the text what
    /// holds it: `pkg.api_*` names `pkg.api_commit`, not `pkg.api_helpers.api_reset`.
    pub(crate) fn names_def(&self, fqn: &str, qualname: &str) -> bool {
        let Some((holder, member)) = &self.member else {
            return self.whole.matches_def(fqn, qualname);
        };
        [fqn, qualname]
            .into_iter()
            .any(|name| match (holder, name.rsplit_once('.')) {
                (None, split) => member.matches(split.map_or(name, |(_, last)| last)),
                (Some(holder), Some((held_by, last))) => {
                    member.matches(last) && holder.matches(held_by)
                }
                (Some(_), None) => false,
            })
    }

    /// Whether the text names `symbol`, a function, class, or module attribute outside the
    /// project, by its full dotted name: as [`Self::names_def`], but never by a dotted suffix
    /// ([`DottedPattern::matches_whole`]), and only when its first name is not a glob. An entry
    /// naming project functions by a suffix (`reset`, `*.reset`) or by a module part
    /// (`pkg:reset`) names nothing outside the project.
    pub(crate) fn names_external(&self, symbol: &str) -> bool {
        let text = self.whole.name().text();
        !self.project_only
            && !has_glob_syntax(text.split('.').next().unwrap_or(text))
            && self.member.as_ref().map_or_else(
                || self.whole.matches_whole(symbol),
                |(holder, member)| {
                    symbol.rsplit_once('.').is_some_and(|(held_by, last)| {
                        member.matches_whole(last)
                            && holder.as_ref().is_some_and(|h| h.matches_whole(held_by))
                    })
                },
            )
    }

    pub(crate) fn matches(&self, key: &FuncKey) -> bool {
        self.names_def(&key.fqn(), &key.qualname)
            || self.method.as_ref().is_some_and(|(family, name)| {
                key.qualname.rsplit_once('.').is_some_and(|(owner, last)| {
                    name.matches(last) && family.contains(&format!("{}.{owner}", key.module))
                })
            })
    }

    /// The class family of `Class.method` (empty for a function).
    pub(crate) fn method_family(&self) -> Option<&FxHashSet<String>> {
        self.method.as_ref().map(|(family, _)| family)
    }
}

/// The `allowed-*`, `forbidden-*`, and `exempt-methods` keys of one owner contract with their
/// configuration contexts (`write-owners.allowed-writers`, ...).
pub(crate) struct AllowanceKeys<'c> {
    pub allowed: (&'c [String], &'static str),
    pub forbidden: (&'c [String], &'static str),
    pub exempt: (&'c [String], &'static str),
}

impl Allowance {
    pub(crate) fn new(keys: &AllowanceKeys<'_>) -> Result<Self, RuleConfigError> {
        let mut out = Self {
            allowed: patterns(keys.allowed.0, keys.allowed.1)?,
            exempt_names: FxHashSet::default(),
            exempt_patterns: Vec::new(),
            forbidden: patterns(keys.forbidden.0, keys.forbidden.1)?,
            targets: None,
            attribution: Attribution::CallGraph,
            own_receiver: false,
            protocols: FxHashSet::default(),
            escaping: None,
        };
        for e in keys.exempt.0 {
            if e.contains(['.', ':']) || has_glob_syntax(e) {
                out.exempt_patterns
                    .push(DottedPattern::new(e, keys.exempt.1)?);
            } else {
                out.exempt_names.insert(e.clone());
            }
        }
        Ok(out)
    }

    pub(crate) fn allows(&self, key: &FuncKey) -> bool {
        self.exempts(key)
            || self.names_allowed(key)
            || self.targets.as_ref().is_some_and(|t| t.any(key))
    }

    /// Whether `exempt-methods` names `key`.
    pub(crate) fn exempts(&self, key: &FuncKey) -> bool {
        let name = key.qualname.rsplit('.').next().unwrap_or(&key.qualname);
        self.exempt_names.contains(name) || names(&self.exempt_patterns, key)
    }

    /// Whether an `allowed-*` pattern names `key`.
    pub(crate) fn names_allowed(&self, key: &FuncKey) -> bool {
        names(&self.allowed, key)
    }

    /// The same allowance, with the guarded functions `targets` allowed too.
    pub(crate) fn with_targets(self, targets: std::sync::Arc<FunctionMatchers>) -> Self {
        Self {
            targets: Some(targets),
            ..self
        }
    }

    /// The same allowance, judging accesses as `attribution` says.
    pub(crate) fn with_attribution(self, attribution: Attribution) -> Self {
        Self {
            attribution,
            ..self
        }
    }

    /// The same allowance, allowing the accesses through values typed with `protocols`.
    pub(crate) fn with_protocols(self, protocols: FxHashSet<String>) -> Self {
        Self { protocols, ..self }
    }

    /// The same allowance, judging the accesses of the functions some code hands on as values
    /// (`escaping`, with `unresolved = "report"`) in those functions.
    pub(crate) fn with_escaping(
        self,
        escaping: Option<std::sync::Arc<FxHashSet<FuncKey>>>,
    ) -> Self {
        Self { escaping, ..self }
    }

    /// Whether `func` judges its accesses itself because code hands it on as a value
    /// ([`Self::with_escaping`]).
    fn escapes(&self, func: &FuncKey) -> bool {
        self.escaping.as_ref().is_some_and(|e| e.contains(func))
    }

    /// Whether `allowed-protocols` names a project class.
    pub(crate) fn names_protocols(&self) -> bool {
        !self.protocols.is_empty()
    }

    /// Whether an access of `member` through a value typed with `protocol` is allowed
    /// (`allowed-protocols`): the key names the protocol, and the protocol declares the member,
    /// itself or through its protocol bases. The access is allowed wherever it is judged: the
    /// type the code uses the value through is a property of the access, not of its function.
    pub(crate) fn allows_protocol(
        &self,
        decls: &ProjectDecls,
        protocol: &str,
        member: &str,
    ) -> bool {
        self.protocols.contains(protocol) && decls.protocol_declares(protocol, member)
    }

    /// The same allowance, allowing the accesses through a method's own receiver or not.
    pub(crate) fn with_own_receiver(self, own_receiver: bool) -> Self {
        Self {
            own_receiver,
            ..self
        }
    }

    pub(crate) fn is_lexical(&self) -> bool {
        self.attribution == Attribution::Lexical
    }

    /// Whether `func`, the function an access is judged in, accesses the object as its own: a
    /// class's own initialization of `family` ([`initializes_itself`]) when the object may be its
    /// parameters `obj`, or, with `own-receiver = "allow"`, an access through the receiver of the
    /// method `func` (`self`, `cls`, `super()`) when the object the member is looked up on may be
    /// its parameters `looked_up`. For a read or a write both are the accessed object; for a call,
    /// `obj` is what the callee binds to its first parameter and `looked_up` the receiver, which
    /// a class method or a static method does not bind. An object that may be the receiver gets
    /// the benefit of the doubt, as for initialization.
    pub(crate) fn owns(
        &self,
        cx: &CheckContext<'_>,
        func: &FuncKey,
        family: &FxHashSet<String>,
        (obj, looked_up): (u64, u64),
    ) -> bool {
        initializes_itself(cx, func, family, obj)
            || self.own_receiver
                && looked_up & 1 != 0
                && cx.decls.function_decl(func).is_some_and(binds_receiver)
    }

    /// Whether the functions `key` runs access inside it ([`runs_inside_allowed`]): `key` is
    /// allowed or exempt, and the code of the functions it runs is not judged by itself alone
    /// (`attribution = "call-graph"`).
    pub(crate) fn allows_callees(&self, key: &FuncKey) -> bool {
        !self.is_lexical() && self.allows(key)
    }

    /// The function an access made in `func` is judged in: `func`, or with `attribution =
    /// "lexical"` the function whose code holds it. A lambda, comprehension, or generator
    /// expression has no name a pattern could give, so its code belongs to the function it is
    /// written in, and a class body's to the function running the class statement (the module
    /// body at module level).
    pub(crate) fn judged<'k>(
        &self,
        decls: &ProjectDecls,
        func: &'k FuncKey,
    ) -> std::borrow::Cow<'k, FuncKey> {
        if !self.is_lexical() {
            return std::borrow::Cow::Borrowed(func);
        }
        let mut qualname = func.qualname.as_str();
        loop {
            let (holder, last) = match qualname.rsplit_once('.') {
                Some((holder, last)) => (Some(holder), last),
                None => (None, qualname),
            };
            let synthetic = last.starts_with('<') && qualname != "<module>";
            if !synthetic && !decls.is_project_class(&format!("{}.{qualname}", func.module)) {
                break;
            }
            qualname = holder.map_or("<module>", |h| h.strip_suffix(".<locals>").unwrap_or(h));
        }
        if qualname == func.qualname {
            return std::borrow::Cow::Borrowed(func);
        }
        std::borrow::Cow::Owned(FuncKey {
            module: func.module.clone(),
            qualname: qualname.to_owned(),
        })
    }

    /// [`Self::judged`] for an access in `func` to an object that may be its parameters `obj`
    /// (one mask, or several), with the parameters of the judged function the object may be: the
    /// parameters of a scope a function holds are none of the function's, while the function's
    /// own parameters the scope reads through closure cells are
    /// ([`heki_effect::CapturedParams`]).
    pub(crate) fn judged_access<'k, M: ParamMasks>(
        &self,
        decls: &ProjectDecls,
        func: &'k FuncKey,
        obj: M,
    ) -> (std::borrow::Cow<'k, FuncKey>, M) {
        let judged = self.judged(decls, func);
        if *judged == *func {
            return (judged, obj);
        }
        let captured = decls
            .function_decl(func)
            .and_then(|d| d.captured_params.as_deref());
        let obj = obj.map(|mask| captured.map_or(0, |c| c.owner_params(&judged.qualname, mask)));
        (judged, obj)
    }

    /// Whether the contract has `forbidden-*` patterns.
    pub(crate) fn has_forbidden(&self) -> bool {
        !self.forbidden.is_empty()
    }

    /// Whether an access by `key` falls under the contract: every function without
    /// `forbidden-*`, else the functions they name.
    pub(crate) fn in_scope(&self, key: &FuncKey) -> bool {
        self.forbidden.is_empty() || names(&self.forbidden, key)
    }

    /// `outside the <owners> of <label>`, or `inside the forbidden <who> of <label>` for a
    /// contract with `forbidden-*` (`label`: [`heki_env::contract_label`]).
    pub(crate) fn where_text(&self, owners: &str, forbidden: &str, label: &str) -> String {
        if self.has_forbidden() {
            format!("inside the {forbidden} of {label}")
        } else {
            format!("outside the {owners} of {label}")
        }
    }
}

/// A private helper delegates responsibility to its project callers: the call sites that run
/// its own body (`EffectSummary::body_facts`) and so receive its writes and reads. A private
/// `async def` only scheduled (never awaited) or a private generator whose generators all
/// escape keeps its reports. So does a private function that only private functions of its own
/// recursion call (`_walk` calling itself, exposed as a callback): going up through private
/// callers never reaches a function that reports, so the access would be reported nowhere.
fn delegates(
    cx: &CheckContext<'_>,
    sites: &SitesByCaller<'_>,
    key: &FuncKey,
    handover: Handover,
) -> bool {
    if !is_private(&key.qualname) {
        return false;
    }
    let callers_running = |func: &FuncKey| {
        cx.index.summary(func).map_or_else(Vec::new, |s| {
            let mut callers = callers_of(cx, sites, func, s.body_facts());
            callers.retain(|c| handover.hands_over(c));
            callers
        })
    };
    let mut queue = vec![key.clone()];
    let mut seen: FxHashSet<FuncKey> = FxHashSet::from_iter([key.clone()]);
    while let Some(func) = queue.pop() {
        for c in callers_running(&func) {
            let leaves_the_recursion = !is_private(&c.caller.qualname)
                || c.into != Facts::Body
                || callers_running(c.caller).is_empty();
            if leaves_the_recursion {
                return true;
            }
            if seen.insert(c.caller.clone()) {
                queue.push(c.caller.clone());
            }
        }
    }
    false
}

/// Which call sites hand the accesses of a private callee to its callers ([`delegates`]).
#[derive(Debug, Clone, Copy)]
enum Handover {
    /// Every call running the callee's body.
    Every,
    /// Every call but one through a protocol that declares the method
    /// ([`heki_effect::CallSite::declared`]): the read and call contracts judge such a call by
    /// the declaration, so what an implementation does inside stays with the implementation.
    ByImplementation,
}

impl Handover {
    fn hands_over(self, c: &Caller<'_>) -> bool {
        match self {
            Self::Every => true,
            Self::ByImplementation => c.site.declared().is_none(),
        }
    }
}

/// Which callers of the function [`runs_inside_allowed_through`] starts from it follows: all,
/// or only those keeping the accesses with the function (`!Handover::hands_over`).
#[derive(Debug, Clone, Copy)]
enum FirstCallers {
    All,
    KeptBy(Handover),
}

impl FirstCallers {
    fn follows(self, c: &Caller<'_>) -> bool {
        match self {
            Self::All => true,
            Self::KeptBy(handover) => !handover.hands_over(c),
        }
    }
}

/// One project call site that runs the facts `facts` of a function: the caller, the site and
/// the binding of the function there, and which of the caller's facts receive them (its body,
/// or what iterating the object it returns runs).
struct Caller<'a> {
    caller: &'a FuncKey,
    path: &'a Path,
    site: &'a CallSite,
    bc: &'a BoundCallee,
    into: Facts,
    /// The project function that iterates the function's result there (`SiteRole::Through`).
    via: Option<&'a FuncKey>,
}

/// The call sites running `facts` of `callee` ([`heki_effect::ProjectIndex::delivery`]), by
/// caller and in source order. A generator caller's body is its iteration (`Facts::Result`).
fn callers_of<'a>(
    cx: &CheckContext<'a>,
    sites: &SitesByCaller<'a>,
    callee: &FuncKey,
    facts: Facts,
) -> Vec<Caller<'a>> {
    let summary = cx.index.summary(callee);
    let mut callers: Vec<&FuncKey> = cx
        .index
        .reverse_call_graph
        .get(callee)
        .into_iter()
        .flatten()
        .collect();
    callers.sort();
    let mut out = Vec::new();
    for caller in callers {
        let caller_body = cx
            .index
            .summary(caller)
            .map_or(Facts::Body, EffectSummary::body_facts);
        for &(path, site) in sites.get(caller).into_iter().flatten() {
            for bc in site
                .project_callees()
                .iter()
                .filter(|bc| bc.callee == *callee)
            {
                let Some(d) = summary
                    .and_then(|s| cx.index.delivery(site, s))
                    .filter(|d| d.from == facts)
                else {
                    continue;
                };
                let into = match d.into {
                    Facts::Body => caller_body,
                    Facts::Result => Facts::Result,
                };
                out.push(Caller {
                    caller: &site.caller,
                    path,
                    site,
                    bc,
                    into,
                    via: d.via,
                });
            }
        }
    }
    out
}

/// One mask of the parameters an object may be, or several ([`Allowance::judged_access`]).
pub(crate) trait ParamMasks {
    /// Each mask through `f`.
    fn map(self, f: impl Fn(u64) -> u64) -> Self;
}

impl ParamMasks for u64 {
    fn map(self, f: impl Fn(u64) -> u64) -> Self {
        f(self)
    }
}

impl ParamMasks for (u64, u64) {
    fn map(self, f: impl Fn(u64) -> u64) -> Self {
        (f(self.0), f(self.1))
    }
}

/// Whether a function binds a receiver to its first parameter: a method (an instance method's
/// `self`, a class method's `cls`) that is no static method.
pub(crate) fn binds_receiver(decl: &heki_effect::FunctionDecl) -> bool {
    decl.is_method && !decl.is_staticmethod
}

/// A class's own initialization: `key` is an initializer (`INITIALIZERS`) of a class of the
/// target's `family`, and the accessed object may be its own instance (`obj_params` bit 0,
/// `self`): only an object that is provably not `self` (another parameter, a fresh instance,
/// an attribute) stays reported.
pub(crate) fn initializes_itself(
    cx: &CheckContext<'_>,
    key: &FuncKey,
    family: &FxHashSet<String>,
    obj_params: u64,
) -> bool {
    let name = key.qualname.rsplit('.').next().unwrap_or(&key.qualname);
    obj_params & 1 != 0
        && INITIALIZERS.contains(&name)
        && cx
            .index
            .summary(key)
            .and_then(|s| s.owner_class_qualname.as_deref())
            .is_some_and(|owner| family.contains(&format!("{}.{owner}", key.module)))
}

/// Whether `func` (about to be reported for an access to an object that may be its parameters
/// `obj`) only runs inside allowed contexts: it has project callers, and every path up the
/// reverse call graph reaches an allowed or exempt function, or an initializer of the target
/// family on its own `self`, before a function without callers. Such a function is part of the
/// owner's operations, like a hook that a store operation runs. The walk goes breadth first over
/// `(function, facts, object parameters)` states along the call sites that run those facts; a
/// function returning a generator is no context of the generator's body (it does not run it), so
/// the walk goes on to where the object it returns is iterated. Paths that only cycle add no
/// allowed context, so a function reached through cycles alone keeps its report. Initializers
/// end the walk negatively: constructor calls also bind the initializers of subclasses (CHA), so
/// their callers are not known exactly. Judged lexically, an allowed function allows its own
/// code only ([`Allowance::allows_callees`]), while a class's own initialization still covers
/// the helpers it passes `self` to: only such initializers are contexts, and a path no longer
/// handing on the object cannot reach one.
fn runs_inside_allowed(
    cx: &CheckContext<'_>,
    sites: &SitesByCaller<'_>,
    allowance: &Allowance,
    family: &FxHashSet<String>,
    func: &FuncKey,
    obj: u64,
) -> bool {
    runs_inside_allowed_through(cx, sites, allowance, family, func, obj, FirstCallers::All)
}

/// The answers of [`judged_inside_allowed`] for the accesses of one contract to one family of
/// target classes, by the accessing function and the parameters its object may be: the
/// accesses of one function to one object share an answer, and each would walk the same callers.
pub(crate) type InsideAllowed<'k> = FxHashMap<(&'k FuncKey, u64), bool>;

/// [`runs_inside_allowed`] for an access in `func` to an object that may be its parameters
/// `obj`, in the function the contract judges it in ([`Allowance::judged_access`]).
fn judged_inside_allowed<'k>(
    cx: &CheckContext<'_>,
    sites: &SitesByCaller<'_>,
    allowance: &Allowance,
    family: &FxHashSet<String>,
    (func, obj): (&'k FuncKey, u64),
    inside: &mut InsideAllowed<'k>,
) -> bool {
    *inside.entry((func, obj)).or_insert_with(|| {
        let (judged, obj) = allowance.judged_access(cx.decls, func, obj);
        runs_inside_allowed(cx, sites, allowance, family, &judged, obj)
    })
}

/// [`runs_inside_allowed`] along the paths through the callers `first` of `func`.
fn runs_inside_allowed_through(
    cx: &CheckContext<'_>,
    sites: &SitesByCaller<'_>,
    allowance: &Allowance,
    family: &FxHashSet<String>,
    func: &FuncKey,
    obj: u64,
    first: FirstCallers,
) -> bool {
    let is_initializer =
        |k: &FuncKey| INITIALIZERS.contains(&k.qualname.rsplit('.').next().unwrap_or(&k.qualname));
    if is_initializer(func) {
        return false;
    }
    let facts = cx
        .index
        .summary(func)
        .map_or(Facts::Body, EffectSummary::body_facts);
    let mut queue = std::collections::VecDeque::from([(func.clone(), facts, obj)]);
    let mut visited: FxHashSet<(FuncKey, Facts, u64)> = FxHashSet::default();
    visited.insert((func.clone(), facts, obj));
    let mut allowed_contexts = false;
    let mut first = Some(first);
    while let Some((callee, facts, obj)) = queue.pop_front() {
        let mut callers = callers_of(cx, sites, &callee, facts);
        if let Some(first) = first.take() {
            callers.retain(|c| first.follows(c));
        }
        if callers.is_empty() {
            // A function without project callers, outside every allowed context.
            return false;
        }
        for c in callers {
            let caller_obj = c.bc.caller_params_of(obj);
            if allowance.is_lexical() && caller_obj == 0 {
                return false;
            }
            // A function returning the object does not run what iterating it runs.
            if c.into == Facts::Result {
                if visited.insert((c.caller.clone(), Facts::Result, caller_obj)) {
                    queue.push_back((c.caller.clone(), Facts::Result, caller_obj));
                }
                continue;
            }
            if allowance.allows_callees(c.caller)
                || c.via.is_some_and(|p| allowance.allows_callees(p))
                || initializes_itself(cx, c.caller, family, caller_obj)
            {
                allowed_contexts = true;
            } else if is_initializer(c.caller) {
                return false;
            } else if visited.insert((c.caller.clone(), c.into, caller_obj)) {
                queue.push_back((c.caller.clone(), c.into, caller_obj));
            }
        }
    }
    allowed_contexts
}

/// Whether the helper `bc` calls changes the typed field `w` (an `IndirectHelper` write) that
/// its caller hands it, as the field's value or as the instance holding it, itself or through
/// the callees it hands the argument to. `delivers` says what of a callee runs at a call
/// (`None`: nothing that counts, such as a callee the contract allows). The helper's summary
/// also holds what its callees change, so a change made only inside allowed callees is no write
/// of the helper's caller.
pub(crate) fn helper_changes_field<'s>(
    cx: &CheckContext<'_>,
    sites: &SitesByCaller<'s>,
    bc: &BoundCallee,
    w: &TypedFieldWriteSite,
    delivers: impl Fn(&'s CallSite, &FuncKey) -> Option<Delivery<'s>>,
) -> bool {
    bc.params.iter().enumerate().any(|(k, o)| {
        let want = if o
            .typed_field
            .as_ref()
            .is_some_and(|(class, attr)| *class == w.class_qualname && *attr == w.attr)
        {
            ParamChange::Itself
        } else if o.instance_classes().any(|c| *c == w.class_qualname) {
            ParamChange::Attr(&w.attr)
        } else {
            return false;
        };
        changes_param(cx, sites, &bc.callee, k, want, &delivers)
    })
}

/// Whether the helper call behind the `IndirectHelper` write `w` (its last hop) changes the
/// field outside the functions `allowed` names. A call that is no longer found counts.
fn indirect_write_outside(
    cx: &CheckContext<'_>,
    sites: &SitesByCaller<'_>,
    w: &TypedFieldWriteSite,
    allowed: impl Fn(&FuncKey) -> bool,
) -> bool {
    let hop = w
        .via_chain
        .last()
        .expect("the chain of an `IndirectHelper` write ends at the helper call");
    let helpers: Vec<&BoundCallee> = sites
        .get(&hop.caller)
        .into_iter()
        .flatten()
        .filter(|(_, s)| s.span == hop.span)
        .flat_map(|(_, s)| s.project_callees())
        .filter(|bc| bc.callee.fqn() == hop.callee)
        .collect();
    helpers.is_empty()
        || helpers.into_iter().any(|bc| {
            helper_changes_field(cx, sites, bc, w, |site, callee| {
                if allowed(callee) {
                    return None;
                }
                let summary = cx.index.summary(callee)?;
                cx.index
                    .delivery(site, summary)
                    .filter(|d| d.into == Facts::Body)
            })
        })
}

/// What change of a parameter counts: an in-place change of the value itself (what a handed
/// field's `IndirectHelper` write is), any in-place change of it or below it, or a change of its
/// attribute (rebinding it, or changing its value in place).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ParamChange<'a> {
    Itself,
    Any,
    Attr(&'a str),
}

/// Whether `func` changes its parameter `k` as `want` says, in its own body or through the
/// callees it hands the parameter to. A callee whose facts arrive other than as its body (what
/// iterating a plain function's result runs) counts as changing it: that is not followed.
fn changes_param<'s>(
    cx: &CheckContext<'_>,
    sites: &SitesByCaller<'s>,
    func: &FuncKey,
    k: usize,
    want: ParamChange<'_>,
    delivers: &impl Fn(&'s CallSite, &FuncKey) -> Option<Delivery<'s>>,
) -> bool {
    let mut stack = vec![(func.clone(), k, want)];
    let mut seen: FxHashSet<(FuncKey, usize, ParamChange<'_>)> = FxHashSet::default();
    while let Some((func, k, want)) = stack.pop() {
        if !seen.insert((func.clone(), k, want)) {
            continue;
        }
        let local = cx
            .index
            .file_of(&func)
            .and_then(|p| cx.index.files[p].local_functions.get(&func));
        let paths = u8::try_from(k)
            .ok()
            .and_then(|k| local?.mutated_param_paths.get(&k));
        let changed_here = match want {
            ParamChange::Itself => paths.is_some_and(|p| p.contains("")),
            ParamChange::Any => {
                k < 64 && local.is_some_and(|s| s.mutates_params & (1u64 << k) != 0)
            }
            ParamChange::Attr(attr) => paths.is_some_and(|p| p.contains(attr)),
        };
        if changed_here {
            return true;
        }
        for (_, site) in sites.get(&func).into_iter().flatten() {
            for callee in site.project_callees() {
                let handed: Vec<(usize, ParamChange<'_>)> = callee
                    .params
                    .iter()
                    .enumerate()
                    .filter_map(|(j, o)| match (&o.param, want) {
                        (Some((p, None)), _) if usize::from(*p) == k => Some((j, want)),
                        // The callee is handed a part of the parameter: a change there is below
                        // the value, or of another attribute.
                        (Some((_, Some(_))), ParamChange::Itself) => None,
                        (Some((p, Some(attr))), ParamChange::Attr(a))
                            if usize::from(*p) == k && a != attr =>
                        {
                            None
                        }
                        (Some((p, Some(_))), _) if usize::from(*p) == k => {
                            Some((j, ParamChange::Any))
                        }
                        _ => None,
                    })
                    .collect();
                if handed.is_empty() {
                    continue;
                }
                let (Some(summary), Some(d)) = (
                    cx.index.summary(&callee.callee),
                    delivers(site, &callee.callee),
                ) else {
                    continue;
                };
                if d.from != summary.body_facts() {
                    return true;
                }
                stack.extend(
                    handed
                        .into_iter()
                        .map(|(j, next)| (callee.callee.clone(), j, next)),
                );
            }
        }
    }
    false
}

/// One call instruction: what it calls (a value, or a method of its receiver) and its arguments.
pub(crate) struct CallInst<'i> {
    callee_var: Option<VarId>,
    /// `recv.method(...)`: the receiver and the method name.
    pub(crate) recv: Option<(VarId, &'i str)>,
    pub(crate) args: &'i [VarId],
    pub(crate) kwargs: &'i [(Option<String>, VarId)],
}

impl<'i> CallInst<'i> {
    /// `None` for an instruction that is no call.
    pub(crate) fn of(kind: &'i InstKind) -> Option<Self> {
        match kind {
            InstKind::Call {
                callee_var,
                args,
                kwargs,
                ..
            } => Some(Self {
                callee_var: *callee_var,
                recv: None,
                args,
                kwargs,
            }),
            InstKind::MethodCall {
                recv,
                method,
                args,
                kwargs,
                ..
            } => Some(Self {
                callee_var: None,
                recv: Some((*recv, method)),
                args,
                kwargs,
            }),
            _ => None,
        }
    }

    /// The functions the call runs, with the values `types` infers.
    pub(crate) fn callee(&self, tcx: &TypeCtx<'_>, types: &FxHashMap<VarId, Val>) -> Callee {
        match self.recv {
            Some((recv, method)) => tcx.resolve_method(types.get(&recv), method),
            None => tcx.resolve_call(self.callee_var.and_then(|v| types.get(&v))),
        }
    }
}

/// The arguments of a call of the project function `decl` by the callee's parameter positions:
/// a bound method's receiver at 0 (not for a class method's class or a static method),
/// positional and keyword arguments by the parameter they bind
/// ([`heki_effect::FunctionDecl::positional_param`],
/// [`heki_effect::FunctionDecl::keyword_param`]).
pub(crate) fn bind_arguments(
    decl: &heki_effect::FunctionDecl,
    binds_recv: bool,
    recv: Option<VarId>,
    args: &[VarId],
    kwargs: &[(Option<String>, VarId)],
) -> Vec<(usize, VarId)> {
    let offset = usize::from(binds_recv && !decl.is_staticmethod);
    let mut bound: Vec<(usize, VarId)> = Vec::new();
    if offset == 1
        && !decl.is_classmethod
        && let Some(r) = recv
    {
        bound.push((0, r));
    }
    let positions = args.iter().enumerate();
    bound.extend(positions.filter_map(|(i, v)| Some((decl.positional_param(i + offset)?, *v))));
    for (name, v) in kwargs {
        if let Some(pos) = name.as_ref().and_then(|n| decl.keyword_param(n)) {
            bound.push((pos, *v));
        }
    }
    bound
}

/// The facts of `callee` that run in the body of `site`'s caller
/// ([`heki_effect::ProjectIndex::delivery`]): its body where the call runs it, or what iterating
/// the object it returns runs where that object is iterated or handed to a function iterating
/// it. A generator function's body runs there, never at its call; `None` where nothing of it
/// runs in the caller (a generator never iterated, a coroutine never awaited, an object the
/// caller returns).
pub(crate) fn facts_run_at<'s>(
    index: &ProjectIndex,
    site: &CallSite,
    callee: &'s EffectSummary,
) -> Option<&'s EffectSummary> {
    let d = index.delivery(site, callee)?;
    (d.into == Facts::Body)
        .then(|| callee.facts(d.from))
        .flatten()
}

/// One violation site: the function holding the write, read, or call (fqn) and its span there.
pub(crate) type SiteId = (String, Span);

/// The reports of a contract rule, merged by `(reporting function, span, K, target)`: the first
/// report of a key gives the message, the help, and the related chain, and every distinct
/// violation site behind the key counts (`Diagnostic::sites`), so a second write inside a
/// private helper that is reported at its caller is visible to the Ratchet baseline. The
/// message names the member the first report matched ([`matched_name`]) and lists the other
/// members of a glob target the key stands for.
///
/// `K` groups the targets whose reports of the same accesses are one report: the contract for
/// `MUT201`, `MUT203`, and `MUT204`, where an access matching several targets of one contract
/// is reported once, under the target configured first, naming the others; the target itself
/// for the rules that name one target per report.
pub(crate) struct MergedReports<'u, K = String> {
    reports: BTreeMap<(FuncKey, Span, K, String), MergedReport<'u>>,
    /// The sites of each group of accesses reported at the same places ([`AccessGroups`]).
    groups: Vec<BTreeSet<SiteId>>,
}

impl<K> Default for MergedReports<'_, K> {
    fn default() -> Self {
        Self {
            reports: BTreeMap::new(),
            groups: Vec::new(),
        }
    }
}

/// What one report of [`MergedReports`] names: the configured `target` text, the member it
/// matched as the message quotes it (`named`, [`matched_name`] in backticks), the target's
/// place in its contract's configuration (`rank`), which decides the target that names the
/// report of an access matching several, and the contract's place in the configuration's array
/// of its kind (`contract`, [`Diagnostic::contracts`]).
#[derive(Clone, Copy)]
pub(crate) struct Named<'a> {
    pub target: &'a str,
    pub named: &'a str,
    pub rank: usize,
    pub contract: usize,
}

/// A group of sites of [`MergedReports`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SiteGroup(usize);

/// What a report stands for: one violation site, or every site of a group.
pub(crate) enum Sites {
    One(SiteId),
    Group(SiteGroup),
}

impl From<SiteId> for Sites {
    fn from(site: SiteId) -> Self {
        Self::One(site)
    }
}

/// The name a report gives the target an access matched: the configured text, or, for a glob
/// target, the member the access reaches (`pkg.types.DataType._flag` for `pkg.*._*`).
pub(crate) fn matched_name(text: &str, member: impl FnOnce() -> String) -> String {
    if names_members(text) {
        member()
    } else {
        text.to_owned()
    }
}

/// Whether a report of the target `text` names the members it matched ([`matched_name`]): it
/// is a glob (the key of an entry target, `Class.attr["key"]`, is no glob).
pub(crate) fn names_members(text: &str) -> bool {
    has_glob_syntax(split_item(text).map_or(text, |(base, _)| base))
}

struct MergedReport<'u> {
    unit: &'u ModuleUnit<'u>,
    message: String,
    /// The target's place in its contract's configuration ([`Named::rank`]).
    rank: usize,
    /// The member the message names (quoted), and the other members behind the key.
    named: String,
    also: BTreeSet<String>,
    help: Option<String>,
    chain: Vec<CallHop>,
    sites: BTreeSet<SiteId>,
    groups: BTreeSet<SiteGroup>,
    /// The contracts reporting the accesses ([`Named::contract`]).
    contracts: BTreeSet<usize>,
}

impl<'u, K: Ord> MergedReports<'u, K> {
    /// A new, empty group of sites.
    pub(crate) fn group(&mut self) -> SiteGroup {
        self.groups.push(BTreeSet::new());
        SiteGroup(self.groups.len() - 1)
    }

    /// Add `site` to `group`: every report standing for the group stands for it too.
    pub(crate) fn add_to_group(&mut self, group: SiteGroup, site: SiteId) {
        self.groups[group.0].insert(site);
    }

    /// Add a report of `func` at `span` under `key` for `name` (the target and the member it
    /// matched), standing for `sites`, with the contract's `message` as its help (`note`);
    /// `message` and `chain` are only built for the first report of the key and target.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add(
        &mut self,
        unit: &'u ModuleUnit<'u>,
        func: &FuncKey,
        span: Span,
        (key, name): (K, Named<'_>),
        sites: impl Into<Sites>,
        note: Option<&str>,
        message: impl FnOnce() -> String,
        chain: impl FnOnce() -> Vec<CallHop>,
    ) {
        let report = self
            .reports
            .entry((func.clone(), span, key, name.target.to_owned()))
            .or_insert_with(|| MergedReport {
                unit,
                message: message(),
                rank: name.rank,
                named: name.named.to_owned(),
                also: BTreeSet::new(),
                help: note.map(str::to_owned),
                chain: chain(),
                sites: BTreeSet::new(),
                groups: BTreeSet::new(),
                contracts: BTreeSet::new(),
            });
        if report.named != name.named {
            report.also.insert(name.named.to_owned());
        }
        report.contracts.insert(name.contract);
        match sites.into() {
            Sites::One(site) => {
                report.sites.insert(site);
            }
            Sites::Group(group) => {
                report.groups.insert(group);
            }
        }
    }

    pub(crate) fn emit(self, cx: &CheckContext<'_>, code: &'static str, out: &mut Vec<Diagnostic>) {
        // Groups of equal sites are one group: the groups of two targets of one access hold the
        // same sites.
        let mut first_of: FxHashMap<&BTreeSet<SiteId>, usize> = FxHashMap::default();
        let canonical: Vec<usize> = (0..self.groups.len())
            .map(|g| *first_of.entry(&self.groups[g]).or_insert(g))
            .collect();
        // Reports standing for the same sites and groups (the call sites of a function, each
        // running the accesses of the same groups) count their union once.
        type Union<'r> = (&'r BTreeSet<SiteId>, BTreeSet<usize>);
        let mut unions: FxHashMap<Union<'_>, usize> = FxHashMap::default();
        // The reports of one place and `K` standing for the same accesses, by the first of them
        // in key order: they are emitted as the report of the target configured first.
        let mut merged: Vec<(usize, Vec<usize>)> = Vec::new();
        // The keys of one place and `K` come one after another.
        let mut place = None;
        let mut accesses: FxHashMap<Union<'_>, usize> = FxHashMap::default();
        let reports: Vec<_> = self.reports.iter().collect();
        for (i, ((func, span, key, _), r)) in reports.iter().enumerate() {
            if place != Some((func, *span, key)) {
                place = Some((func, *span, key));
                accesses.clear();
            }
            let groups: BTreeSet<usize> = r.groups.iter().map(|g| canonical[g.0]).collect();
            let union = (&r.sites, groups);
            let distinct = *unions.entry(union.clone()).or_insert_with(|| {
                let mut groups = union.1.iter();
                match (groups.next(), groups.next()) {
                    (None, _) => r.sites.len(),
                    (Some(&g), None) if r.sites.is_empty() => self.groups[g].len(),
                    _ => {
                        let grouped = union.1.iter().flat_map(|&g| &self.groups[g]);
                        let all: BTreeSet<&SiteId> = r.sites.iter().chain(grouped).collect();
                        all.len()
                    }
                }
            });
            match accesses.entry(union) {
                std::collections::hash_map::Entry::Occupied(e) => merged[*e.get()].1.push(i),
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(merged.len());
                    merged.push((distinct, vec![i]));
                }
            }
        }
        for (distinct, members) in merged {
            let sites = u32::try_from(distinct).unwrap_or(u32::MAX);
            // The target configured first names the report; the others' names, with their
            // members, follow its own members.
            let lead = *members
                .iter()
                .min_by_key(|&&i| reports[i].1.rank)
                .unwrap_or(&members[0]);
            let ((func, span, _, target), r) = reports[lead];
            let mut also: Vec<(usize, &str)> =
                r.also.iter().map(|m| (r.rank, m.as_str())).collect();
            for &i in members.iter().filter(|&&i| i != lead) {
                let other = reports[i].1;
                also.push((other.rank, other.named.as_str()));
                also.extend(other.also.iter().map(|m| (other.rank, m.as_str())));
            }
            also.sort_unstable();
            let mut seen: FxHashSet<&str> = FxHashSet::from_iter([r.named.as_str()]);
            let also: Vec<&str> = also
                .into_iter()
                .map(|(_, m)| m)
                .filter(|m| seen.insert(m))
                .collect();
            let mut message = r.message.clone();
            if !also.is_empty() {
                message.push_str(&format!(" (also {})", also.join(", ")));
            }
            let contracts: BTreeSet<usize> = members
                .iter()
                .flat_map(|&i| &reports[i].1.contracts)
                .copied()
                .collect();
            if let Some(d) = cx.emit(
                out,
                code,
                r.unit,
                *span,
                &func.qualname,
                target.clone(),
                message,
                r.help.clone(),
                &r.chain,
            ) {
                d.sites = sites;
                d.contracts = contract_places(contracts);
            }
        }
    }
}

/// [`Diagnostic::contracts`] of the contracts at `places` in their kind's configuration.
pub(crate) fn contract_places(places: impl IntoIterator<Item = usize>) -> Vec<u32> {
    places
        .into_iter()
        .map(|p| u32::try_from(p).unwrap_or(u32::MAX))
        .collect()
}

/// `unresolved` values; the first is the default (writes heki cannot tie to a class are not
/// reported).
const UNRESOLVED: &[&str] = &["ignore", "report"];

/// `fresh-objects` values; the first is the default (writes that construct an object are no
/// violation).
const FRESH_OBJECTS: &[&str] = &["allow", "report"];
/// `unlisted-dict-keys` values; the first is the default (a `__dict__` key no target names is
/// no target's write).
const UNLISTED_DICT_KEYS: &[&str] = &["ignore", "report"];
/// The values of `import-probes`, the default first.
const IMPORT_PROBES: &[&str] = &["allow", "report"];

/// The functions some code hands on as values ([`CheckContext::escaping_functions`]), for a
/// contract reporting what it cannot tie (`strict`), computed at most once per check
/// (`known`).
pub(crate) fn escaping_for(
    cx: &CheckContext<'_>,
    strict: bool,
    known: &std::cell::OnceCell<std::sync::Arc<FxHashSet<FuncKey>>>,
) -> Option<std::sync::Arc<FxHashSet<FuncKey>>> {
    strict.then(|| std::sync::Arc::clone(known.get_or_init(|| cx.escaping_functions().into())))
}

/// Whether a contract reports the accesses heki cannot tie to a class (`unresolved`).
pub(crate) fn reports_unresolved(
    value: Option<&str>,
    section: &'static str,
    contract: &str,
) -> Result<bool, RuleConfigError> {
    Ok(mode(
        value,
        UNRESOLVED,
        "`ignore` or `report`",
        section,
        contract,
        "unresolved",
    )? == "report")
}

/// Whether a contract allows the writes that construct the object they write (`fresh-objects`).
pub(crate) fn allows_fresh(
    value: Option<&str>,
    section: &'static str,
    contract: &str,
) -> Result<bool, RuleConfigError> {
    Ok(mode(
        value,
        FRESH_OBJECTS,
        "`allow` or `report`",
        section,
        contract,
        "fresh-objects",
    )? == "allow")
}

/// `references` values; the first is the default (a reference handing a target on is no call).
const REFERENCES: &[&str] = &["ignore", "calls", "calls-and-bases", "reads"];

/// What a call contract counts as a call of a target besides calling it (`references`), each
/// value counting more than the one before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum References {
    /// Nothing.
    Ignored,
    /// A reference handing it on.
    Calls,
    /// A reference handing it on, or a class statement naming it among its bases.
    CallsAndBases,
    /// Any use of its value: a reference handing it on, a class statement naming it among its
    /// bases, or a read using it for anything else.
    Reads,
}

impl References {
    /// What the `references` value `value` counts (a value outside [`REFERENCES`] counts
    /// nothing; it is checked where the rules parse it).
    pub(crate) fn named(value: &str) -> Self {
        match value {
            "calls" => Self::Calls,
            "calls-and-bases" => Self::CallsAndBases,
            "reads" => Self::Reads,
            _ => Self::Ignored,
        }
    }

    /// Whether the reference site `site` counts as a call.
    pub(crate) fn counts(self, site: &CallSite) -> bool {
        match self {
            Self::Ignored => false,
            Self::Calls => !site.is_base && !site.is_read,
            Self::CallsAndBases => !site.is_read,
            Self::Reads => true,
        }
    }
}

/// What a call contract counts as a call besides the calls themselves (`references`).
pub(crate) fn counts_references(
    value: Option<&str>,
    section: &'static str,
    contract: &str,
) -> Result<References, RuleConfigError> {
    let value = mode(
        value,
        REFERENCES,
        "`ignore`, `calls`, `calls-and-bases`, or `reads`",
        section,
        contract,
        "references",
    )?;
    Ok(References::named(value))
}

/// `attribution` values; the first is the default (the call graph decides which function an
/// access is judged in).
const ATTRIBUTION: &[&str] = &["call-graph", "lexical"];

/// Which function an owner contract judges an access in (`attribution`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Attribution {
    /// A private helper hands its accesses to its callers, and a function that only runs inside
    /// allowed functions is allowed.
    #[default]
    CallGraph,
    /// The function whose code holds the access ([`Allowance::judged`]); a helper a class's own
    /// initialization passes `self` to is still part of that initialization.
    Lexical,
}

/// What the sites of one write share: each class's site of a write through a protocol has them
/// ([`TypedFieldWriteSite::through_protocol`]).
type WriteOf<'s> = (&'s str, WriteKind, Span, &'s [std::sync::Arc<CallHop>], u64);

fn write_of(site: &TypedFieldWriteSite) -> WriteOf<'_> {
    (
        &site.attr,
        site.kind,
        site.span,
        &site.via_chain,
        site.obj_params,
    )
}

/// The protocol each write through a protocol-typed value among `sites` (one function's) goes
/// through: the class of the write's site that is a protocol declaring the attribute, whose write
/// the sites of the implementations' attributes share.
fn protocols_written_through<'s>(
    cx: &CheckContext<'_>,
    sites: &'s BTreeSet<TypedFieldWriteSite>,
) -> FxHashMap<WriteOf<'s>, &'s str> {
    sites
        .iter()
        .filter(|s| s.through_protocol && cx.decls.protocol_declares(&s.class_qualname, &s.attr))
        .map(|s| (write_of(s), s.class_qualname.as_str()))
        .collect()
}

/// The project classes the `allowed-protocols` patterns of a contract name, with their subclasses
/// unless `protocol-subclasses = false` (a class that is no protocol allows nothing,
/// [`Allowance::allows_protocol`]).
pub(crate) fn named_protocols(
    cx: &CheckContext<'_>,
    all: &[ContractClass],
    (patterns, subclasses): (&[String], Option<bool>),
    context: &'static str,
) -> Result<FxHashSet<String>, RuleConfigError> {
    let subclasses = Subclasses::of(subclasses);
    let mut out = FxHashSet::default();
    for p in patterns {
        out.extend(class_set(
            cx,
            &DottedPattern::new(p, context)?,
            all,
            subclasses,
        ));
    }
    Ok(out)
}

/// `own-receiver` values; the first is the default.
const OWN_RECEIVER: &[&str] = &["report", "allow"];

/// Whether an owner contract allows the accesses through the receiver of the method judging
/// them (`own-receiver = "allow"`).
pub(crate) fn allows_own_receiver(
    value: Option<&str>,
    section: &'static str,
    contract: &str,
) -> Result<bool, RuleConfigError> {
    Ok(mode(
        value,
        OWN_RECEIVER,
        "`report` or `allow`",
        section,
        contract,
        "own-receiver",
    )? == "allow")
}

/// The `attribution` of an owner contract.
pub(crate) fn attribution(
    value: Option<&str>,
    section: &'static str,
    contract: &str,
) -> Result<Attribution, RuleConfigError> {
    let value = mode(
        value,
        ATTRIBUTION,
        "`call-graph` or `lexical`",
        section,
        contract,
        "attribution",
    )?;
    Ok(if value == "lexical" {
        Attribution::Lexical
    } else {
        Attribution::CallGraph
    })
}

/// Whether an effect boundary's `reads-environment` forbids import probes (`import-probes`).
pub(crate) fn reports_import_probes(
    value: Option<&str>,
    section: &'static str,
    contract: &str,
) -> Result<bool, RuleConfigError> {
    Ok(mode(
        value,
        IMPORT_PROBES,
        "`allow` or `report`",
        section,
        contract,
        "import-probes",
    )? == "report")
}

/// The value of a contract's mode key, one of `values` (the first when unset).
fn mode(
    value: Option<&str>,
    values: &'static [&'static str],
    expected: &'static str,
    section: &'static str,
    contract: &str,
    key: &'static str,
) -> Result<&'static str, RuleConfigError> {
    let Some(value) = value else {
        return Ok(values[0]);
    };
    values
        .iter()
        .find(|v| **v == value)
        .copied()
        .ok_or_else(|| RuleConfigError::UnknownMode {
            section,
            contract: contract.to_owned(),
            key,
            value: value.to_owned(),
            expected,
            values,
        })
}

struct OwnerRule<'r> {
    /// How a report names the contract ([`heki_env::contract_label`]).
    label: String,
    targets: Vec<ContractTarget>,
    allowance: Allowance,
    forbid_cached: FxHashSet<String>,
    note: Option<&'r str>,
    /// `fresh-objects = "allow"`: writes constructing the object they write are allowed.
    allow_fresh: bool,
    /// `unresolved = "report"`: writes heki cannot tie to a class are reported (`MUT205`).
    report_unresolved: bool,
    /// `unlisted-dict-keys = "report"`: the `__dict__` keys of the targets' classes that no
    /// target names are reported too.
    report_unlisted_keys: bool,
}

impl OwnerRule<'_> {
    /// What `site` writes of the contract: the targets it hits, or, with `unlisted-dict-keys =
    /// "report"`, a key written through the namespace of an object of a class some targets cover
    /// (a `dict_direct` write) that no `write-owners` target names (`named`), once for each
    /// family of the contract's targets covering the class. Each comes with the index of a target
    /// whose family it is.
    fn written<'s>(
        &'s self,
        site: &TypedFieldWriteSite,
        named: &NamedDictKeys<'_>,
    ) -> Vec<(usize, Written<'s>)> {
        let hits = write_hits(&self.targets, site);
        if !hits.is_empty()
            || !self.report_unlisted_keys
            || site.kind != WriteKind::DictDirect
            || named.names(&site.class_qualname, &site.attr)
        {
            return hits
                .into_iter()
                .map(|(k, t)| (k, Written::Target(t)))
                .collect();
        }
        let mut families: Vec<(usize, &FxHashSet<String>)> = Vec::new();
        for (k, t) in self.targets.iter().enumerate() {
            if t.classes.contains(&site.class_qualname)
                && !families.iter().any(|(_, f)| *f == &t.classes)
            {
                families.push((k, &t.classes));
            }
        }
        families
            .into_iter()
            .map(|(k, f)| (k, Written::UnlistedKey(f)))
            .collect()
    }
}

/// The `__dict__` keys the `write-owners` contracts name, by class (`unlisted-dict-keys`): a
/// target names the keys its attribute pattern matches (an entry target its container) on every
/// class of its family, whichever contract declares it. A key one contract guards is a listed
/// key of the class for every other contract, whose own allowance does not cover it.
struct NamedDictKeys<'r> {
    /// The targets covering each class (fqn); empty when no contract reports unlisted keys.
    by_class: FxHashMap<&'r str, Vec<&'r ContractTarget>>,
}

impl<'r> NamedDictKeys<'r> {
    fn new(rules: &'r [OwnerRule<'_>]) -> Self {
        let mut by_class: FxHashMap<&str, Vec<&ContractTarget>> = FxHashMap::default();
        if rules.iter().any(|r| r.report_unlisted_keys) {
            for target in rules.iter().flat_map(|r| &r.targets) {
                for class in &target.classes {
                    by_class.entry(class).or_default().push(target);
                }
            }
        }
        Self { by_class }
    }

    /// Whether a target covering `class` names the `__dict__` key `key`.
    fn names(&self, class: &str, key: &str) -> bool {
        self.by_class
            .get(class)
            .is_some_and(|targets| targets.iter().any(|t| t.matches_attr(key)))
    }
}

/// What a write site writes of a `write-owners` contract ([`OwnerRule::written`]).
enum Written<'r> {
    Target(&'r ContractTarget),
    /// A `__dict__` key no target names, on an object of this target family's class.
    UnlistedKey(&'r FxHashSet<String>),
}

pub(crate) fn mut201(
    cx: &CheckContext<'_>,
    out: &mut Vec<Diagnostic>,
) -> Result<(), RuleConfigError> {
    let (writes, unresolved) = (
        cx.selection.is_enabled("MUT201"),
        cx.selection.is_enabled("MUT205"),
    );
    if !writes && !unresolved || cx.config.write_owners.is_empty() {
        return Ok(());
    }
    debug_assert!(
        cx.index.propagates_typed_writes(),
        "`typed_writes` decides for MUT201"
    );
    let all_classes = project_classes(cx);
    let mut rules = Vec::new();
    let escaping = std::cell::OnceCell::new();
    for (index, rule) in cx.config.write_owners.iter().enumerate() {
        let report_unresolved =
            reports_unresolved(rule.unresolved.as_deref(), "write-owners", &rule.name)?;
        let targets = parse_targets(
            cx,
            &all_classes,
            &rule.targets,
            "write-owners",
            "write-owners.targets",
        )?;
        let allowance = Allowance::new(&AllowanceKeys {
            allowed: (&rule.allowed_writers, "write-owners.allowed-writers"),
            forbidden: (&rule.forbidden_writers, "write-owners.forbidden-writers"),
            exempt: (&rule.exempt_methods, "write-owners.exempt-methods"),
        })?
        .with_attribution(attribution(
            rule.attribution.as_deref(),
            "write-owners",
            &rule.name,
        )?)
        .with_own_receiver(allows_own_receiver(
            rule.own_receiver.as_deref(),
            "write-owners",
            &rule.name,
        )?)
        .with_protocols(named_protocols(
            cx,
            &all_classes,
            (&rule.allowed_protocols, rule.protocol_subclasses),
            "write-owners.allowed-protocols",
        )?)
        .with_escaping(escaping_for(cx, report_unresolved, &escaping));
        let mut forbid_cached = FxHashSet::default();
        for p in &rule.forbid_cached_property_on {
            let p = DottedPattern::new(p, "write-owners.forbid-cached-property-on")?;
            forbid_cached.extend(class_set(cx, &p, &all_classes, Subclasses::Covered));
        }
        rules.push(OwnerRule {
            label: heki_env::contract_label("write-owners", index, &rule.name),
            targets,
            allowance,
            forbid_cached,
            note: rule.message.as_deref(),
            allow_fresh: allows_fresh(rule.fresh_objects.as_deref(), "write-owners", &rule.name)?,
            report_unresolved,
            report_unlisted_keys: mode(
                rule.unlisted_dict_keys.as_deref(),
                UNLISTED_DICT_KEYS,
                "`ignore` or `report`",
                "write-owners",
                &rule.name,
                "unlisted-dict-keys",
            )? == "report",
        });
    }

    unresolved::mut205(cx, &rules, out);
    if !writes {
        return Ok(());
    }

    let mut keys_by_fqn: FxHashMap<String, FuncKey> = FxHashMap::default();
    for file in cx.index.files.values() {
        for k in file.local_functions.keys() {
            keys_by_fqn.insert(k.fqn(), k.clone());
        }
    }

    let mut paths: Vec<_> = cx.index.files.keys().collect();
    paths.sort();
    let mut reports = MergedReports::default();
    let mut sites = None;
    let mut inside: Vec<Vec<InsideAllowed<'_>>> = rules
        .iter()
        .map(|r| r.targets.iter().map(|_| InsideAllowed::default()).collect())
        .collect();
    let names_protocols = rules.iter().any(|r| r.allowance.names_protocols());
    let named_keys = NamedDictKeys::new(&rules);
    for path in paths {
        let file = &cx.index.files[path];
        let Some(unit) = cx.unit(path) else {
            continue;
        };
        let mut funcs: Vec<_> = file.functions.iter().collect();
        funcs.sort_by(|a, b| a.0.cmp(b.0));
        for (key, s) in funcs {
            let protocols = if names_protocols {
                protocols_written_through(cx, &s.typed_field_write_sites)
            } else {
                FxHashMap::default()
            };
            for site in &s.typed_field_write_sites {
                for (r, rule) in rules.iter().enumerate() {
                    // Constructing an object is no write of its owners' state.
                    if rule.allow_fresh && site.construction == Construction::Yes
                        || site.through_protocol
                            && protocols.get(&write_of(site)).is_some_and(|p| {
                                rule.allowance.allows_protocol(cx.decls, p, &site.attr)
                            })
                    {
                        continue;
                    }
                    let lexical = rule.allowance.is_lexical();
                    // A write a callee hands up is that callee's own, except the change of a
                    // typed object handed to a helper that never sees its class.
                    let own = site.via_chain.is_empty()
                        || site.kind == WriteKind::IndirectHelper && site.via_chain.len() == 1;
                    if lexical && !own {
                        continue;
                    }
                    let (judged, obj_params) =
                        rule.allowance.judged_access(cx.decls, key, site.obj_params);
                    for (ti, written) in rule.written(site, &named_keys) {
                        let family = match written {
                            Written::Target(t) => &t.classes,
                            Written::UnlistedKey(family) => family,
                        };
                        let mut chain: Vec<FuncKey> = vec![judged.clone().into_owned()];
                        chain.extend(
                            site.via_chain
                                .iter()
                                .filter_map(|h| keys_by_fqn.get(&h.callee).cloned()),
                        );
                        // Allowed writers and exempt methods anywhere on the chain; with
                        // `forbidden-writers`, a forbidden function must be on it.
                        if chain.iter().any(|k| rule.allowance.allows(k))
                            || !chain.iter().any(|k| rule.allowance.in_scope(k))
                        {
                            continue;
                        }
                        let origin = if site.kind == WriteKind::IndirectHelper && chain.len() >= 2 {
                            chain.len() - 2
                        } else {
                            chain.len() - 1
                        };
                        // Report at the outermost function: a write site only reaches callers
                        // through private helpers (and the functions iterating a generator they
                        // are handed, which run its writes without holding them). A private
                        // helper on the way that keeps its accesses (only its own recursion
                        // calls it, see `delegates`) reports the write itself, and a delegating
                        // function leaves it to its callers.
                        // No initializer name is private, so on a reported chain only the
                        // outermost function can be the initializer of the written object.
                        let sites = sites.get_or_insert_with(|| cx.sites_by_caller());
                        // A function handed on as a value keeps its accesses, as a function
                        // without callers does (`Allowance::escapes`).
                        let keeps = |k: &FuncKey| {
                            rule.allowance.escapes(k) || !delegates(cx, sites, k, Handover::Every)
                        };
                        let kept_below = || {
                            chain[1..=origin]
                                .iter()
                                .any(|k| is_private(&k.qualname) && keeps(k))
                        };
                        if !lexical && (kept_below() || !keeps(key))
                            || rule
                                .allowance
                                .owns(cx, &judged, family, (obj_params, obj_params))
                        {
                            continue;
                        }
                        // A helper handed the field that changes it only inside allowed
                        // writers or exempt methods.
                        if site.kind == WriteKind::IndirectHelper
                            && !indirect_write_outside(cx, sites, site, |callee| {
                                rule.allowance.allows(callee)
                            })
                        {
                            continue;
                        }
                        // A function that only runs inside the owner's operations (every caller
                        // path starts in an allowed context) writes inside them.
                        if !rule.allowance.escapes(key)
                            && judged_inside_allowed(
                                cx,
                                sites,
                                &rule.allowance,
                                family,
                                (key, site.obj_params),
                                &mut inside[r][ti],
                            )
                        {
                            continue;
                        }
                        let span = site.via_chain.first().map_or(site.span, |h| h.span);
                        let writer = site
                            .via_chain
                            .last()
                            .map_or_else(|| key.fqn(), |h| h.callee.clone());
                        let member = format!("{}.{}", site.class_qualname, site.attr);
                        // The report's target: the configured one, or the unlisted key as a
                        // target naming it would (`Class.key`).
                        let (text, named, unlisted) = match written {
                            Written::Target(t) => {
                                let named = matched_name(&t.text, || {
                                    let entry = t.key.as_ref().map(|k| format!("[\"{k}\"]"));
                                    format!("{member}{}", entry.unwrap_or_default())
                                });
                                (t.text.clone(), named, "")
                            }
                            Written::UnlistedKey(_) => {
                                let class = &site.class_qualname;
                                let qualname = cx.decls.class_qualname(class).unwrap_or(class);
                                let text = format!("{qualname}.{}", site.attr);
                                (text, member, ", a `__dict__` key no target names,")
                            }
                        };
                        let quoted = format!("`{named}`");
                        let name = Named {
                            target: &text,
                            named: &quoted,
                            rank: ti,
                            contract: r,
                        };
                        reports.add(
                            unit,
                            &judged,
                            span,
                            (r, name),
                            (writer, site.span),
                            rule.note,
                            || {
                                let via = site
                                    .via_chain
                                    .last()
                                    .map(|h| format!(" via `{}`", h.callee))
                                    .unwrap_or_default();
                                format!(
                                    "`{}` writes `{named}` ({}){via}{unlisted} {}",
                                    judged.qualname,
                                    site.kind.describe(),
                                    rule.allowance.where_text(
                                        "write owners",
                                        "forbidden writers",
                                        &rule.label
                                    )
                                )
                            },
                            || heki_effect::owned_chain(&site.via_chain),
                        );
                    }
                }
            }
        }
    }
    reports.emit(cx, "MUT201", out);

    // `forbid-cached-property-on`: a `@cached_property` defined on a guarded class.
    for unit in cx.units {
        for f in &unit.input.ir.functions {
            let Some(owner) = &f.owner_class_qualname else {
                continue;
            };
            let cached = cx
                .index
                .summary(&CheckContext::key(unit, f))
                .is_some_and(|s| s.is_cached_property);
            if f.is_class_body || !cached {
                continue;
            }
            let class = format!("{}.{owner}", unit.input.module_name);
            let forbidding = rules.iter().enumerate();
            for (r, rule) in forbidding.filter(|(_, r)| r.forbid_cached.contains(&class)) {
                if let Some(d) = cx.emit(
                    out,
                    "MUT201",
                    unit,
                    f.span,
                    &f.qualname,
                    format!("{owner}.{}", f.name),
                    format!(
                        "`@cached_property` `{}` is defined on `{owner}`, which {} forbids caching on",
                        f.name, rule.label
                    ),
                    rule.note.map(str::to_owned),
                    &[],
                ) {
                    d.contracts = contract_places([r]);
                }
            }
        }
    }
    Ok(())
}

// ==================================================================================== MUT205

mod unresolved;

// ==================================================================================== MUT202

/// `readonly-returns` producers from the configuration.
#[derive(Debug)]
pub(crate) struct ReadOnlyProducers {
    rules: Vec<(DottedPattern, bool)>,
}

impl ReadOnlyProducers {
    pub fn new(config: &HekiConfig) -> Result<Self, RuleConfigError> {
        let mut rules = Vec::new();
        for r in &config.readonly_returns {
            for p in &r.producers {
                rules.push((DottedPattern::new(p, "readonly-returns.producers")?, r.deep));
            }
        }
        Ok(Self { rules })
    }

    /// Whether a producer names a project function (`fqn` and its `qualname`), an attribute of a
    /// class ([`ContractClass::member`]), or an external symbol (the symbol twice), and then
    /// whether one of those naming it is deep.
    pub(crate) fn deep_for(&self, fqn: &str, qualname: &str) -> Option<bool> {
        let mut found = None;
        for (p, deep) in &self.rules {
            if p.matches_def(fqn, qualname) {
                found = Some(found.unwrap_or(false) || *deep);
            }
        }
        found
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Whether a producer has a module part.
    pub(crate) fn names_modules(&self) -> bool {
        self.rules.iter().any(|(p, _)| p.module().is_some())
    }
}

/// What `MUT202` reads besides the function it checks: the declared producers, and the names of
/// the functions whose call yields a protected value as a property getter could (one returning
/// a protected value, an `@lru_cache` function, or a declared property). Reading a property is a
/// call of its getter; only these names are resolved. The names come from the summaries, so a
/// check reads no other module's declarations for them.
pub(crate) struct ReadOnlyReturns {
    producers: ReadOnlyProducers,
    getters: FxHashSet<String>,
}

impl ReadOnlyReturns {
    pub fn new(cx: &CheckContext<'_>) -> Result<Self, RuleConfigError> {
        let producers = ReadOnlyProducers::new(cx.config)?;
        let property = |k: &FuncKey, s: &EffectSummary| {
            cx.decls.module(&k.module).is_some_and(|m| {
                s.decorators
                    .iter()
                    .any(|d| m.decorator_is(d, &["property", "cached_property"]))
            })
        };
        let getters = cx
            .index
            .files
            .values()
            .flat_map(|f| &f.functions)
            .filter(|(k, s)| {
                s.returns_protected.is_some()
                    || s.is_lru_cache
                    || producers.deep_for(&k.fqn(), &k.qualname).is_some() && property(k, s)
            })
            .map(|(k, _)| {
                k.qualname
                    .rsplit('.')
                    .next()
                    .unwrap_or(&k.qualname)
                    .to_owned()
            })
            .collect();
        Ok(Self { producers, getters })
    }
}

/// Read-only origin of a value.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ro {
    producer: String,
    deep: bool,
    /// A shallow copy of a deep read-only value: its own top level may change, its
    /// elements / attributes are still shared.
    shallow_copy: bool,
    /// How the function got the value from the producer: the call of a function that returns
    /// it (a wrapper), then the wrapper's calls down to the producer (empty: a direct call).
    via: Vec<CallHop>,
    /// `producer` is declared in `readonly-returns` (`MUT202`), not an `@lru_cache` function
    /// (`MUT114`).
    declared: bool,
}

impl Ro {
    fn new(producer: String, declared: bool, shared: Shared, via: Vec<CallHop>) -> Option<Self> {
        shared.any().then_some(Self {
            producer,
            deep: shared.elements,
            shallow_copy: !shared.object,
            via,
            declared,
        })
    }

    fn shared(&self) -> Shared {
        Shared {
            object: !self.shallow_copy,
            elements: self.deep,
        }
    }

    /// The origin of a value that is this one on some paths and `other` on the others: what
    /// either shares, named after the one that shares the object itself.
    fn join(&self, other: &Self) -> Self {
        let shared = self.shared().join(other.shared());
        let named = if !self.shared().object && other.shared().object {
            other
        } else {
            self
        };
        Self {
            producer: named.producer.clone(),
            deep: shared.elements,
            shallow_copy: !shared.object,
            via: named.via.clone(),
            declared: named.declared,
        }
    }

    /// The origin of a value derived from this one by `step`.
    fn derive(&self, step: ReturnStep) -> Option<Self> {
        Self::new(
            self.producer.clone(),
            self.declared,
            step.apply(self.shared()),
            self.via.clone(),
        )
    }
}

/// One call instruction with its resolved callee.
pub(crate) struct CallParts<'i> {
    /// `recv.method(...)`: the receiver and the method name.
    pub(crate) recv: Option<(VarId, &'i str)>,
    pub(crate) args: &'i [VarId],
    pub(crate) kwargs: &'i [(Option<String>, VarId)],
    pub(crate) callee: Callee,
    /// The call site recording the project call ([`site_call`]).
    pub(crate) site: Option<&'i CallSite>,
}

impl CallParts<'_> {
    /// `(callee, its summary, parameter position, argument)` for each argument the call binds
    /// to a project callee the index summarizes ([`bind_arguments`]).
    pub(crate) fn bound_arguments<'s, 'c>(
        &'s self,
        cx: &'s CheckContext<'c>,
    ) -> impl Iterator<Item = (&'s FuncKey, &'c EffectSummary, usize, VarId)> + 's {
        let binds_recv = self.site.is_some_and(|s| s.binds_recv);
        let callees = self.site.into_iter().flat_map(|s| s.project_callees());
        callees.flat_map(move |bc| {
            let key = &bc.callee;
            let summary = cx.index.summary(key);
            let decl = cx.decls.function_decl(key).cloned().unwrap_or_default();
            let bound = bind_arguments(
                &decl,
                binds_recv,
                self.recv.map(|(v, _)| v),
                self.args,
                self.kwargs,
            );
            summary.into_iter().flat_map(move |s| {
                bound
                    .clone()
                    .into_iter()
                    .map(move |(pos, v)| (key, s, pos, v))
            })
        })
    }
}

/// The project call the call instruction `inst` of a function makes as its call site records
/// it (among the function's `sites`; `def` gives each variable's defining instruction): the
/// callees the summaries resolved, with the values the `isinstance` tests around it narrow,
/// which a resolution with the function's inferred values would not see. `worker`: instead
/// the callable an offloading API runs on a worker thread (`asyncio.to_thread(grow, acc)` runs
/// `grow(acc)`), which takes the positional arguments after it, keyword arguments left
/// unbound.
pub(crate) fn site_call<'i>(
    def: &dyn Fn(VarId) -> Option<&'i Inst>,
    sites: FnSites<'_, 'i>,
    inst: &'i Inst,
    worker: bool,
) -> Option<CallParts<'i>> {
    let call = CallInst::of(&inst.kind)?;
    let runner = if worker {
        heki_effect::Runner::Worker
    } else {
        heki_effect::Runner::Caller
    };
    let site = sites.at(inst.span).iter().find(|s| {
        s.role == SiteRole::Call && s.runner == runner && !s.project_callees().is_empty()
    })?;
    if wraps_call_at_own_span(def, inst, &call) {
        return None;
    }
    let callee = Callee::Project(
        site.project_callees()
            .iter()
            .map(|bc| bc.callee.clone())
            .collect(),
        site.binds_recv,
    );
    if !worker {
        return Some(CallParts {
            recv: call.recv,
            args: call.args,
            kwargs: call.kwargs,
            callee,
            site: Some(site),
        });
    }
    let k = usize::from(site.handed_callable?);
    let callable = *call.args.get(k)?;
    let recv = match def(callable).map(|i| &i.kind) {
        Some(InstKind::AttrLoad { obj, attr, .. }) if site.binds_recv => {
            Some((*obj, attr.as_str()))
        }
        _ => None,
    };
    Some(CallParts {
        recv,
        args: &call.args[k + 1..],
        kwargs: &[],
        callee,
        site: Some(site),
    })
}

/// Whether the call instruction `inst` consumes the result of another call instruction with
/// the same span. Only an instruction the IR builder synthesizes around a source expression
/// shares its span (a comprehension's `acc.append(elt)` carries the span of `elt`); when that
/// expression is a call, the call site at the span is the expression's, not the synthesized
/// instruction's.
fn wraps_call_at_own_span<'i>(
    def: &dyn Fn(VarId) -> Option<&'i Inst>,
    inst: &Inst,
    call: &CallInst<'_>,
) -> bool {
    call.recv
        .map(|(recv, _)| recv)
        .into_iter()
        .chain(call.args.iter().copied())
        .chain(call.kwargs.iter().map(|(_, v)| *v))
        .any(|v| def(v).is_some_and(|d| d.span == inst.span && CallInst::of(&d.kind).is_some()))
}

const MUTABLE_BUILTINS: &[&str] = &[
    "builtins.list",
    "builtins.dict",
    "builtins.set",
    "builtins.bytearray",
];

pub(crate) fn mut202(
    cx: &CheckContext<'_>,
    unit: &ModuleUnit<'_>,
    returns: &ReadOnlyReturns,
    out: &mut Vec<Diagnostic>,
) {
    if !["MUT202", "MUT114", "MUT115"]
        .iter()
        .any(|code| cx.enabled(code, unit))
    {
        return;
    }
    let tcx = TypeCtx::new(&unit.input, cx.decls, cx.kb);
    let sites = cx.sites_by_span(unit);
    let classes = below_copies::ClassAnswers::default();
    for f in &unit.input.ir.functions {
        if f.is_class_body {
            continue;
        }
        Mut202Fn::new(cx, unit, &tcx, returns, &sites, &classes, f).run(out);
    }
}

struct Mut202Fn<'a> {
    cx: &'a CheckContext<'a>,
    unit: &'a ModuleUnit<'a>,
    tcx: &'a TypeCtx<'a>,
    returns: &'a ReadOnlyReturns,
    /// `MUT115`'s class decisions asked so far in the module's check.
    classes: &'a below_copies::ClassAnswers,
    f: &'a IrFunction,
    /// The facts and tables of `f`: the defining instruction of each variable, looked up per
    /// call operand.
    ix: FnIndex<'a>,
    key: FuncKey,
    types: FxHashMap<VarId, Val>,
    ro: FxHashMap<VarId, Ro>,
    /// The function's call sites in the index, every role.
    sites: FnSites<'a, 'a>,
    /// The distinct facts of each project callee that the sites of one call run in the function
    /// ([`Self::facts_run_by_call`], built on the first need).
    facts_by_call: std::cell::OnceCell<FactsByCall<'a>>,
    /// The cached function whose result each variable is on every path ([`Self::must_results`],
    /// built on the first need).
    cached: std::cell::OnceCell<FxHashMap<VarId, MustResult<'a>>>,
    /// The function's tables of definitions and block order (built on the first need).
    shape: std::cell::OnceCell<FnShape<'a>>,
    /// The project functions the function calls, by their dotted names (built on the first
    /// explanation of a `MUT114` report, [`Self::cached_function`]).
    callees_by_fqn: std::cell::OnceCell<FxHashMap<String, &'a FuncKey>>,
}

/// Per call ([`CallSite::of_call`]) and project callee, the distinct facts of the callee that the
/// call's site and the sites of the object it returns run in the calling function
/// ([`facts_run_at`]).
type FactsByCall<'a> =
    FxHashMap<(Span, &'a CallTarget), FxHashMap<&'a FuncKey, Vec<&'a EffectSummary>>>;

/// The passes of [`Mut202Fn::must_results`] before it gives up.
const MUST_RESULT_PASSES: usize = 64;

/// The cached function whose result a variable is on every path (`MUT114`'s identity premise).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MustResult<'k> {
    /// No value reaches it yet.
    Unset,
    /// On every path, the result of a direct call of this cached function.
    Of(&'k FuncKey),
    /// Something else on some path.
    Other,
}

impl MustResult<'_> {
    fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unset, x) | (x, Self::Unset) => x,
            (Self::Of(a), Self::Of(b)) if a == b => self,
            _ => Self::Other,
        }
    }
}

/// How an instruction changes its operand in place, for `MUT114`'s claim.
enum Change<'i> {
    /// `x[k] = v`, `del x[k]`.
    Item,
    /// `x.append(...)`.
    Method(&'i str),
    /// `x += y`.
    InPlace(heki_ir::BinOpKind),
}

impl<'a> Mut202Fn<'a> {
    fn new(
        cx: &'a CheckContext<'a>,
        unit: &'a ModuleUnit<'a>,
        tcx: &'a TypeCtx<'a>,
        returns: &'a ReadOnlyReturns,
        sites: &'a SitesBySpan<'a>,
        classes: &'a below_copies::ClassAnswers,
        f: &'a IrFunction,
    ) -> Self {
        let key = CheckContext::key(unit, f);
        Self {
            cx,
            unit,
            tcx,
            returns,
            classes,
            f,
            ix: FnIndex::new(f, &unit.input.ir.constants),
            sites: sites.of(&key.qualname),
            key,
            types: tcx.infer(f),
            ro: FxHashMap::default(),
            facts_by_call: std::cell::OnceCell::new(),
            cached: std::cell::OnceCell::new(),
            shape: std::cell::OnceCell::new(),
            callees_by_fqn: std::cell::OnceCell::new(),
        }
    }

    fn shape(&self) -> &FnShape<'a> {
        self.shape.get_or_init(|| FnShape::new(self.f))
    }

    /// The project function whose result each variable is on every path: a direct call of one
    /// project function, through copies, and through phis whose every input is a result of the
    /// same function (`MUT114`'s claim also needs the function's
    /// [`heki_effect::CachedResult`], which "Same object" leaves to the cached function's own
    /// premises). An optimistic fixpoint: a variable
    /// starts unset and only moves down (unset, then one function, then other), so a loop
    /// passing the value around keeps it. In reverse postorder each pass carries the values
    /// across one more back edge, so the passes follow the nesting of loops, which Python limits
    /// to 20 blocks; past [`MUST_RESULT_PASSES`] no variable is known.
    fn must_results(&self) -> &FxHashMap<VarId, MustResult<'a>> {
        self.cached.get_or_init(|| {
            let f = self.f;
            let defined: FxHashSet<VarId> = f
                .blocks
                .iter()
                .flat_map(|b| {
                    b.phis
                        .iter()
                        .map(|p| p.dst)
                        .chain(b.insts.iter().filter_map(|i| i.kind.dst()))
                })
                .collect();
            // A variable no instruction or phi defines (a parameter) is no cached result.
            let mut state: FxHashMap<VarId, MustResult<'a>> = defined
                .into_iter()
                .map(|v| (v, MustResult::Unset))
                .collect();
            let get = |state: &FxHashMap<VarId, MustResult<'a>>, v: VarId| {
                state.get(&v).copied().unwrap_or(MustResult::Other)
            };
            let rpo = self.shape().rpo();
            let converged = (0..MUST_RESULT_PASSES).any(|_| {
                let mut changed = false;
                for &bb in rpo {
                    let block = f.block(bb);
                    for phi in &block.phis {
                        let joined = phi
                            .incoming
                            .iter()
                            .filter(|(_, v)| *v != phi.dst)
                            .fold(MustResult::Unset, |acc, (_, v)| acc.join(get(&state, *v)));
                        changed |= state.insert(phi.dst, joined) != Some(joined);
                    }
                    for inst in &block.insts {
                        let Some(dst) = inst.kind.dst() else {
                            continue;
                        };
                        let value = match &inst.kind {
                            InstKind::Copy { src, .. } => get(&state, *src),
                            _ => self
                                .single_callee(inst)
                                .map_or(MustResult::Other, MustResult::Of),
                        };
                        changed |= state.insert(dst, value) != Some(value);
                    }
                }
                !changed
            });
            // Past the limit no variable is known.
            state.retain(|_, _| converged);
            state
        })
    }

    /// The one project function the call instruction `inst` runs.
    fn single_callee(&self, inst: &'a Inst) -> Option<&'a FuncKey> {
        let shape = self.shape();
        let call = site_call(&|v| shape.inst(v), self.sites, inst, false)?;
        let callees = call.site?.project_callees();
        let bc = callees.first().filter(|_| callees.len() == 1)?;
        self.cx.index.summary(&bc.callee).map(|_| &bc.callee)
    }

    /// `MUT114`'s exact claim at `inst`, which changes `changed`, a value from the cached
    /// function that `r` names, through the calls `chain` (empty: at `inst` itself): it changes
    /// in place the list, dict, or set that a cached function returned, the same object on every
    /// path. Each premise that does not hold is told to `steps` with its cause and what the cause
    /// is about; a decision stops at the first, an explanation goes on. The cached function and
    /// its result when every premise holds.
    fn cached_change_steps(
        &self,
        inst: &'a Inst,
        changed: VarId,
        r: &Ro,
        chain: &[CallHop],
        steps: &mut dyn FnMut((&'static OpenCause, Option<String>)) -> ControlFlow<()>,
    ) -> Option<(&'a FuncKey, heki_effect::CachedResult)> {
        let mut told = Told::new(steps);
        let named = |key: &FuncKey| Some(format!("`{}`", key.fqn()));
        // "Same object": the result of a direct call of one function on every path.
        let result_of = match self.must_results().get(&changed).copied() {
            Some(MustResult::Of(key)) if r.via.is_empty() => Some(key),
            _ => None,
        };
        if let Some(wrapper) = r.via.first() {
            let about = Some(format!("`{}`", wrapper.callee));
            if told.step((&premises::RETURNED_THROUGH_A_FUNCTION, about)) {
                return None;
            }
        } else if result_of.is_none() && told.step((&premises::NOT_THE_RESULT_ON_EVERY_PATH, None))
        {
            return None;
        }
        // "Cached", "Keyed", and "Container kind": one fact of the cached function's summary.
        let cached_fn = result_of.or_else(|| self.cached_function(r));
        let cached = cached_fn.and_then(|key| self.cx.index.summary(key)?.cached_result);
        if cached.is_none() {
            for cause in premises::CACHED_RESULT_NOT_ESTABLISHED {
                if told.step((cause, cached_fn.and_then(named))) {
                    return None;
                }
            }
        }
        // "Changed in place": at the line itself, a change of a container of that kind.
        if let Some(hop) = chain.first() {
            let about = Some(format!("`{}`", hop.callee));
            told.step((&premises::CHANGED_IN_A_CALLED_FUNCTION, about));
            return None;
        }
        let change = match &inst.kind {
            InstKind::SubscriptStore { .. } => Some((Change::Item, "__setitem__")),
            InstKind::SubscriptDelete { .. } => Some((Change::Item, "__delitem__")),
            InstKind::MethodCall { method, .. } => Some((Change::Method(method), method.as_str())),
            InstKind::BinOp {
                op, in_place: true, ..
            } => Some((Change::InPlace(*op), op.in_place_method())),
            _ => None,
        };
        let Some((change, method)) = change else {
            told.step((&premises::NOT_A_CHANGE_OF_THE_KIND, None));
            return None;
        };
        let cached = cached?;
        let by_name = matches!(change, Change::Method(_));
        let changes = match change {
            Change::Item => cached.kind.takes_item_stores(),
            Change::Method(m) => cached.kind.mutator(m),
            Change::InPlace(op) => cached.kind.in_place_op(op),
        };
        if !changes && told.step((&premises::NOT_A_CHANGE_OF_THE_KIND, None)) {
            return None;
        }
        // Under R2 a result the return annotation names may be an instance of a project subclass
        // of the builtin, whose own `method` runs instead; one built in the function (a display,
        // `list(...)`) is the builtin itself.
        let builtin = cached.built
            || self
                .cx
                .decls
                .method_not_overridden(cached.kind.class(), method, by_name);
        if !builtin {
            told.step((&premises::REPLACED_BY_A_SUBCLASS, cached_fn.and_then(named)));
        }
        (!told.open).then_some((cached_fn?, cached))
    }

    /// The cached function `r` names, when the value's identity is not established: the callee
    /// of the last call through a wrapper, or of a call of this function.
    fn cached_function(&self, r: &Ro) -> Option<&'a FuncKey> {
        let Some(hop) = r.via.last() else {
            let callees = self.callees_by_fqn.get_or_init(|| {
                self.sites
                    .all()
                    .flat_map(CallSite::project_callees)
                    .map(|bc| (bc.callee.fqn(), &bc.callee))
                    .collect()
            });
            return callees.get(&r.producer).copied();
        };
        let site = self.cx.must_explainer.as_ref()?.site_of(hop)?;
        site.project_callees()
            .iter()
            .map(|bc| &bc.callee)
            .find(|key| key.fqn() == r.producer)
    }

    /// The distinct facts of the project callee `key` that the call at `span` (a site of it)
    /// runs in this function: at the call, and where the object it returns is iterated
    /// ([`facts_run_at`]).
    fn facts_run_by_call(&self, span: Span, key: &FuncKey) -> &[&'a EffectSummary] {
        let call = self.sites.at(span).iter().find(|s| {
            s.role == SiteRole::Call && s.project_callees().iter().any(|bc| bc.callee == *key)
        });
        call.and_then(|call| self.facts_by_call().get(&call.of_call())?.get(key))
            .map_or(&[], Vec::as_slice)
    }

    /// The facts the function's sites run, per call they stand for, gathered for all calls in
    /// one pass over the sites.
    fn facts_by_call(&self) -> &FactsByCall<'a> {
        self.facts_by_call.get_or_init(|| {
            let index = self.cx.index;
            let mut out = FactsByCall::default();
            for site in self.sites.all() {
                for bc in site.project_callees() {
                    let run = index
                        .summary(&bc.callee)
                        .and_then(|callee| facts_run_at(index, site, callee));
                    let facts = out
                        .entry(site.of_call())
                        .or_default()
                        .entry(&bc.callee)
                        .or_default();
                    if let Some(run) =
                        run.filter(|run| !facts.iter().any(|f| std::ptr::eq(*f, *run)))
                    {
                        facts.push(run);
                    }
                }
            }
            out
        })
    }

    /// The parts of a call instruction (`None` for any other instruction).
    fn call_of<'i>(&self, inst: &'i Inst) -> Option<CallParts<'i>>
    where
        'a: 'i,
    {
        // A project call as the summaries resolved it; any other call by the inferred values.
        if let Some(call) = site_call(&|v| self.ix.shape.inst(v), self.sites, inst, false) {
            return Some(call);
        }
        let call = CallInst::of(&inst.kind)?;
        Some(CallParts {
            callee: call.callee(self.tcx, &self.types),
            recv: call.recv,
            args: call.args,
            kwargs: call.kwargs,
            site: None,
        })
    }

    /// The read-only origin of a call's (or a property read's) result: a declared producer, an
    /// `@lru_cache` function, or a project function that returns one's value (its summary's
    /// `returns_protected`), reached through the call at `span`.
    fn producer(&self, callee: &Callee, span: Span) -> Option<Ro> {
        let direct = |producer: String, declared: bool, deep: bool| {
            Ro::new(producer, declared, Shared::produced(deep), vec![])
        };
        match callee {
            Callee::Project(keys, _) => keys.iter().find_map(|k| {
                let fqn = k.fqn();
                let summary = self.cx.index.summary(k);
                if summary.is_some_and(|s| s.is_lru_cache) {
                    return direct(fqn, false, true);
                }
                if let Some(d) = self.returns.producers.deep_for(&fqn, &k.qualname) {
                    return direct(fqn, true, d);
                }
                let r = summary?.returns_protected.as_ref()?;
                let mut via = vec![CallHop {
                    caller: self.key.clone(),
                    callee: fqn,
                    span,
                    path: self.unit.input.path.clone(),
                }];
                via.extend(heki_effect::owned_chain(&r.chain));
                Ro::new(r.producer.clone(), r.declared, r.shared, via)
            }),
            Callee::External(sym) => self
                .returns
                .producers
                .deep_for(sym, sym)
                .and_then(|d| direct(sym.clone(), true, d)),
            Callee::TooMany | Callee::Unresolved => None,
        }
    }

    /// Value produced by one instruction from read-only operands (the derivations of
    /// `heki_effect::readonly`, which the return flow applies backward).
    fn transfer(&self, inst: &Inst) -> Option<Ro> {
        match &inst.kind {
            InstKind::Copy { src, .. } => self.ro.get(src).cloned(),
            InstKind::SubscriptLoad { obj, .. } => {
                let class = match self.types.get(obj) {
                    Some(Val::Inst(class)) => Some(class.as_str()),
                    _ => None,
                };
                let step = readonly::subscript_step(&self.ix.shape, inst, class);
                self.ro.get(obj)?.derive(step)
            }
            InstKind::AttrLoad { obj, attr, .. } => {
                if let Some(r) = self.ro.get(obj) {
                    return r.derive(ReturnStep::ELEMENT);
                }
                // `readonly-returns` may name a property: `obj.schema`.
                if let Some(Val::Inst(class)) = self.types.get(obj) {
                    let class = ContractClass {
                        fqn: class.clone(),
                        qualname: self.cx.decls.class_qualname(class).map(str::to_owned),
                    };
                    let (fqn, qualname) = class.member(attr);
                    if let Some(deep) = self.returns.producers.deep_for(&fqn, &qualname) {
                        return Ro::new(fqn, true, Shared::produced(deep), vec![]);
                    }
                }
                // A property getter that returns a read-only producer's value.
                if !self.returns.getters.contains(attr.as_str()) {
                    return None;
                }
                let getter = self.tcx.resolve_attr_load(self.types.get(obj), attr)?;
                self.producer(&getter, inst.span)
            }
            InstKind::MakeIter { iterable, .. } => self.ro.get(iterable).cloned(),
            InstKind::IterNext { iter, .. } => self.ro.get(iter)?.derive(ReturnStep::ELEMENT),
            InstKind::DictLiteral { .. } | InstKind::AllocBuiltin { .. } => {
                readonly::display_copies(self.f, &inst.kind)
                    .iter()
                    .filter_map(|v| self.ro.get(v)?.derive(ReturnStep::SHALLOW_COPY))
                    .reduce(|a, b| a.join(&b))
            }
            _ => {
                let call = self.call_of(inst)?;
                if let Some(r) = self.producer(&call.callee, inst.span) {
                    return Some(r);
                }
                let (operand, method) = match call.recv {
                    Some((recv, method)) if self.ro.contains_key(&recv) => {
                        (Some(recv), Some(method))
                    }
                    _ => (
                        call.args
                            .first()
                            .copied()
                            .filter(|a| self.ro.contains_key(a)),
                        None,
                    ),
                };
                let r = self.ro.get(&operand?)?;
                let deep = readonly::deep_copy_requested(call.kwargs, |v| {
                    self.ix.shape.bool_const(v) == Some(true)
                });
                let own_items = call.args.len() > 1 || !call.kwargs.is_empty();
                match readonly::call_derivation(method, &call.callee, self.cx.kb, deep, own_items)?
                {
                    Derivation::DeepCopy => None,
                    Derivation::Step(step) => r.derive(step),
                }
            }
        }
    }

    /// The read-only origins of the function's values, to a fixpoint: a phi joins the origins
    /// of every incoming value (a shallow copy on one branch and the shared object on the other
    /// is the shared object), and a value is derived again when its operand's origin grows. An
    /// origin only grows (`Ro::join`), so the loop ends.
    fn propagate(&mut self) {
        // A copy: the passes change `self`.
        let rpo = self.ix.shape.rpo().to_vec();
        loop {
            let mut changed = false;
            for &bb in &rpo {
                let block = self.f.block(bb);
                for phi in &block.phis {
                    let joined = phi
                        .incoming
                        .iter()
                        .filter_map(|(_, v)| self.ro.get(v))
                        .fold(None::<Ro>, |acc, r| {
                            Some(acc.map_or_else(|| r.clone(), |a| a.join(r)))
                        });
                    if let Some(r) = joined {
                        changed |= self.grow(phi.dst, r);
                    }
                }
                for inst in &block.insts {
                    let Some(dst) = inst.kind.dst() else {
                        continue;
                    };
                    if let Some(r) = self.transfer(inst) {
                        changed |= self.grow(dst, r);
                    }
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// Join `r` into the origin of `var`; whether it grew.
    fn grow(&mut self, var: VarId, r: Ro) -> bool {
        let next = match self.ro.get(&var) {
            Some(old) => old.join(&r),
            None => r,
        };
        let grew = self.ro.get(&var) != Some(&next);
        if grew {
            self.ro.insert(var, next);
        }
        grew
    }

    /// Read-only origin of `var` when mutating its own top level is a violation.
    fn guarded(&self, var: VarId) -> Option<&Ro> {
        self.ro.get(&var).filter(|r| !r.shallow_copy)
    }

    fn run(mut self, out: &mut Vec<Diagnostic>) {
        self.below_copies(out);
        if !self.cx.enabled("MUT202", self.unit) && !self.cx.enabled("MUT114", self.unit) {
            return;
        }
        self.propagate();
        if self.ro.is_empty() {
            return;
        }
        // Filling a memo of a shared object, or initializing one of its attributes lazily,
        // leaves what it computes as it was (as for the helpers, `memo_param_paths`). Each
        // object is read on its own: a test of one object's attribute says nothing of another's.
        let objects: FxHashMap<VarId, u32> = self
            .ro
            .iter()
            .filter(|(_, r)| !r.shallow_copy)
            .map(|(v, _)| (*v, self.ix.shape.copy_root(*v).0))
            .collect();
        let memo_fills: FxHashSet<(BlockId, u32)> = attr_writes(&self.ix.shape, &objects)
            .into_iter()
            .filter(|(attr, w)| w.fills_memo(attr) || w.lazy_init)
            .map(|(_, w)| (w.block, w.index))
            .collect();
        let mut reported: BTreeSet<Span> = BTreeSet::new();
        for bb in &self.f.blocks {
            for (index, inst) in bb.insts.iter().enumerate() {
                let at = (bb.id, u32::try_from(index).unwrap_or(u32::MAX));
                if memo_fills.contains(&at) {
                    continue;
                }
                let (hit, chain) = self.violation(inst);
                if let Some((r, changed)) = hit
                    && reported.insert(inst.span)
                {
                    self.report(out, inst, changed, &r, chain);
                }
            }
        }
    }

    /// Report the change `inst` makes to `changed`, the shared value `r` (through the helper
    /// calls `chain`): under `MUT202` for a declared producer, else under `MUT114`, exact when
    /// its narrowed claim holds ([`Self::cached_change_steps`]).
    fn report(
        &self,
        out: &mut Vec<Diagnostic>,
        inst: &'a Inst,
        changed: VarId,
        r: &Ro,
        chain: Vec<CallHop>,
    ) {
        // A value from a wrapper: the wrapper called here, then the hops down to the producer,
        // then the hop to the mutating helper.
        let through = r
            .via
            .first()
            .map(|h| format!(" through `{}`", h.callee))
            .unwrap_or_default();
        let mut related = r.via.clone();
        related.extend(chain.iter().cloned());
        if r.declared {
            self.cx.emit(
                out,
                "MUT202",
                self.unit,
                inst.span,
                &self.key.qualname,
                r.producer.clone(),
                format!(
                    "`{}` mutates the shared object returned by read-only producer `{}`{through}",
                    self.key.qualname, r.producer
                ),
                Some("change a copy (`copy.deepcopy(...)`) instead".to_owned()),
                &related,
            );
            return;
        }
        let exact =
            self.cached_change_steps(inst, changed, r, &chain, &mut |_| ControlFlow::Break(()));
        let (message, help) = match exact {
            Some((key, cached)) => (
                format!(
                    "`{}` changes in place the {kind} that `{}` returned; `{}` is cached by `{}`",
                    self.key.qualname,
                    key.qualname,
                    key.qualname,
                    cached.decorator.fqn(),
                    kind = cached.kind.as_str(),
                ),
                format!("change a copy (`{}(...)`) instead", cached.kind.as_str()),
            ),
            None => (
                format!(
                    "`{}` mutates the shared object returned by cached function `{}`{through}",
                    self.key.qualname, r.producer
                ),
                "change a copy (`copy.deepcopy(...)`) instead".to_owned(),
            ),
        };
        let Some(d) = self.cx.emit(
            out,
            "MUT114",
            self.unit,
            inst.span,
            &self.key.qualname,
            exact.map_or_else(|| r.producer.clone(), |(key, _)| key.fqn()),
            message,
            Some(help),
            &related,
        ) else {
            return;
        };
        let tier = premises::tier("MUT114", exact.is_some());
        d.certainty = tier.at_most(d.certainty);
        if self.cx.open_premises && tier == Certainty::Heuristic {
            let mut open: Vec<OpenPremise> = Vec::new();
            self.cached_change_steps(inst, changed, r, &chain, &mut |(cause, about)| {
                open.push(cause.open_at(None, about));
                ControlFlow::Continue(())
            });
            debug_assert!(!open.is_empty(), "a heuristic MUT114 report records why");
            d.open_records = open.into_iter().map(OpenRecord::from).collect();
        }
    }

    /// The shared value `inst` changes, and the value it holds it in, with the call that hands
    /// it to a project function changing it (empty: `inst` changes it itself).
    fn violation(&self, inst: &Inst) -> (Option<(Ro, VarId)>, Vec<CallHop>) {
        let changed = match &inst.kind {
            InstKind::AttrStore { obj, .. }
            | InstKind::AttrDelete { obj, .. }
            | InstKind::SubscriptStore { obj, .. }
            | InstKind::SubscriptDelete { obj, .. } => Some(*obj),
            InstKind::MethodCall { recv, method, .. } if container_mutator(method) => Some(*recv),
            InstKind::BinOp {
                lhs,
                in_place: true,
                ..
            } if matches!(self.types.get(lhs), Some(Val::Inst(t)) if MUTABLE_BUILTINS.contains(&t.as_str())) => {
                Some(*lhs)
            }
            _ => None,
        };
        if let Some((v, r)) = changed.and_then(|v| Some((v, self.guarded(v)?))) {
            return (Some((r.clone(), v)), Vec::new());
        }
        // Passing a read-only value to a project function that mutates that parameter, also
        // through an offloading API that calls it on a worker thread.
        let def = |v| self.ix.shape.inst(v);
        let Some(call) = site_call(&def, self.sites, inst, false)
            .or_else(|| site_call(&def, self.sites, inst, true))
        else {
            return (None, Vec::new());
        };
        for (key, _, pos, var) in call.bound_arguments(self.cx) {
            let Some(r) = self.ro.get(&var) else {
                continue;
            };
            // Summaries track parameters 0..64 (`mutated_param_paths` keys are `u8`).
            // A shallow copy shares only the objects below its top level: rebinding its
            // attributes changes the copy alone; a producer that is not deep shares only the
            // object itself, not what it holds as items. Filling a memo of the shared object,
            // or initializing one of its attributes lazily, leaves what it computes as it was.
            let p = u8::try_from(pos).ok().filter(|p| *p < 64);
            let mutates = self.facts_run_by_call(inst.span, key).iter().any(|facts| {
                p.is_some_and(|p| {
                    if r.shallow_copy {
                        facts
                            .changed_param_attrs
                            .get(&p)
                            .is_some_and(|attrs| facts.changes_param_state(p, Some(attrs)))
                    } else if r.deep {
                        facts.changes_param_state(p, None)
                    } else {
                        facts.changes_param_object(p)
                    }
                })
            });
            if mutates {
                let hop = CallHop {
                    caller: self.key.clone(),
                    callee: key.fqn(),
                    span: inst.span,
                    path: self.unit.input.path.clone(),
                };
                return (Some((r.clone(), var)), vec![hop]);
            }
        }
        (None, Vec::new())
    }
}

// ==================================================================================== MUT115

mod below_copies;

// ==================================================================================== MUT107

/// `MUT107`: a method of the class (or of a subclass) other than the initializers rebinds or
/// changes in place, with a plain store, an attribute that the `__hash__` of its instances
/// reads. Reported at the method's first own write of the attribute, once per method and
/// attribute; the filters are `MUT101`'s (two-phase initialization, memo dictionaries,
/// disjoint constant keys) plus lazy initialization and `__hash__`'s own cache.
pub(crate) fn mut107(cx: &CheckContext<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.selection.is_enabled("MUT107") {
        return;
    }
    let mut seen = FxHashSet::default();
    let mut sites: Option<SitesByCaller<'_>> = None;
    for_each_class_family(cx, |methods, family, init_called| {
        let hashes: Vec<_> = methods
            .iter()
            .filter(|h| h.name() == "__hash__" && !writes_may_run_descriptors(cx.decls, h))
            .filter_map(|h| Some((h, cx.index.summary(&h.key)?)))
            .collect();
        if hashes.is_empty() {
            return;
        }
        let sites = sites.get_or_insert_with(|| cx.sites_by_caller());
        let init_helpers = private_methods_named_in_initializers(cx, family);
        for (h, hs) in hashes {
            // Filter 5: what the hash computation writes itself (`self._hash = ...`, also
            // through its `self.helper()` calls) is its cache; a write elsewhere resets it.
            let cache: BTreeSet<&str> = hs
                .mutates_self_attrs
                .iter()
                .map(|a| a.attr.as_str())
                .collect();
            let mut reads: BTreeMap<&str, Vec<&Option<String>>> = BTreeMap::new();
            for r in &hs.reads_self_attrs {
                if !cache.contains(r.attr.as_str()) {
                    reads.entry(&r.attr).or_default().push(&r.const_key);
                }
            }
            // `__init_subclass__` / `__class_getitem__` are implicit class methods: they set
            // class attributes, not those of an instance.
            // Only checked methods are reported.
            let writers = family
                .iter()
                .filter_map(|m| Some((m, m.checked?)))
                .filter(|(m, _)| {
                    !IMPLICIT_CLASS_METHODS.contains(&m.name())
                        && m.name() != "__hash__"
                        && !filter1_init(m, init_called)
                        && !init_helpers.contains(&m.key)
                        && !writes_may_run_descriptors(cx.decls, m)
                        && !hashes_otherwise(cx.decls, m, h)
                })
                .filter_map(|(m, checked)| Some((m, checked, local_summary(cx.index, m)?)));
            // What the hash reads on the instances of each writer's class, decided once per
            // class.
            // What the hash reads on the instances of each writer's class, decided once per
            // class.
            let mut reads_on: FxHashMap<String, FxHashSet<&str>> = FxHashMap::default();
            for (m, Checked { unit, f }, local) in writers {
                let shape = FnShape::new(f);
                let receiver = receiver_class(cx.decls, m, h);
                for (attr, read_keys) in &reads {
                    let writes: Vec<&AttrKeyPath> = local
                        .mutates_self_attrs
                        .iter()
                        .filter(|w| w.attr == *attr)
                        .collect();
                    if writes.is_empty() {
                        continue;
                    }
                    let read_on_receiver = reads_on
                        .entry(receiver.clone())
                        .or_insert_with(|| self_reads_on(cx, sites, &h.key, &receiver));
                    // Filter 4: stored hashes (`self._hash`, which a deserialization hook
                    // recomputes), disjoint constant keys. The write is a plain store, and
                    // the hash reads the attribute on the writer's instances.
                    if !read_on_receiver.contains(*attr)
                        || cx.decls.attr_write_runs_code(&receiver, attr)
                        || is_stored_hash(attr)
                        || disjoint_keys(
                            read_keys.iter().copied(),
                            writes.iter().map(|w| &w.const_key),
                        )
                    {
                        continue;
                    }
                    // Filters 2 and 3: lazy initialization, memo dictionaries. Filter 6: an
                    // in-place change of a value the hash hands whole to `hash()` changes only
                    // the value's own hash, which its own class answers for.
                    let hashed_whole = hs
                        .hashed_whole_self_attrs
                        .as_deref()
                        .is_some_and(|h| h.contains(*attr));
                    let first = self_attr_writes(&shape).into_iter().find(|(a, w)| {
                        *a == *attr
                            && !w.fills_memo(attr)
                            && !w.lazy_init
                            && (w.rebinds || !hashed_whole)
                    });
                    let Some((_, write)) = first else {
                        continue;
                    };
                    if !seen.insert((m.key.clone(), attr.to_string())) {
                        continue;
                    }
                    let class_qualname = hs.owner_class_qualname.as_deref().unwrap_or_default();
                    let change = if write.rebinds {
                        "rebinds"
                    } else {
                        "changes in place"
                    };
                    cx.emit(
                        out,
                        "MUT107",
                        unit,
                        write.span,
                        &m.key.qualname,
                        format!("{class_qualname}.{attr}->__hash__"),
                        format!(
                            "`{}` {change} `self.{attr}` outside initialization, and `{}` reads it",
                            m.key.qualname, h.key.qualname
                        ),
                        Some(
                            "hash only attributes that never change, and return a changed copy instead (`dataclasses.replace`)"
                                .to_owned(),
                        ),
                        &[],
                    );
                }
            }
        }
    });
}

/// The class of the instances both `writer` and `reader` run on: the writer's class when it
/// is the reader's class or a subclass of it, else the reader's class (the writer is inherited
/// from an ancestor).
fn receiver_class(decls: &ProjectDecls, writer: &Method<'_>, reader: &Method<'_>) -> String {
    let (class, reader_class) = (writer.class(), reader.class());
    if class == reader_class || decls.ancestors_of(&class).contains(&reader_class) {
        class
    } else {
        reader_class
    }
}

/// The `self` attributes `method` reads when it runs on an instance of `receiver`: its own
/// reads, and those of the `self` methods and properties it calls, each call running the
/// definition `receiver`'s MRO finds. The summary of `method` joins the reads of every
/// definition a `self` call may run, those overriding it in other subclasses too. Each method
/// is visited once, and only methods of the classes along `receiver`'s MRO are; the walk is
/// over the project index, so its budget is unmetered (`heki check` and the language server
/// visit the same methods).
fn self_reads_on<'s, 'k>(
    cx: &CheckContext<'s>,
    sites: &SitesByCaller<'k>,
    method: &'k FuncKey,
    receiver: &str,
) -> FxHashSet<&'s str> {
    let mro: Vec<String> = std::iter::once(receiver.to_owned())
        .chain(cx.decls.ancestors_of(receiver))
        .collect();
    let mro_position = |k: &FuncKey| {
        let class = format!("{}.{}", k.module, k.qualname.rsplit_once('.')?.0);
        mro.iter().position(|c| *c == class)
    };
    let next = |k: &'k FuncKey, out: &mut Vec<&'k FuncKey>| {
        let self_calls = sites.get(k).into_iter().flatten();
        out.extend(
            self_calls
                .filter(|(_, s)| s.recv_is_self)
                .filter_map(|(_, site)| {
                    let runs = site
                        .project_callees()
                        .iter()
                        .filter_map(|bc| Some((mro_position(&bc.callee)?, &bc.callee)))
                        .min_by_key(|(at, _)| *at);
                    runs.map(|(_, callee)| callee)
                }),
        );
    };
    let mut reads = FxHashSet::default();
    let visit = |k: &'k FuncKey| {
        let local = cx
            .index
            .file_of(k)
            .and_then(|p| cx.index.files.get(p))
            .and_then(|f| f.local_functions.get(k));
        reads.extend(
            local
                .into_iter()
                .flat_map(|l| l.reads_self_attrs.iter().map(|r| r.attr.as_str())),
        );
        Visit::<()>::Expand
    };
    let budget = Budget::unmetered("MUT107 hash reads");
    reach(&budget, [method], next, visit).open_means(None);
    reads
}

/// Dunder methods Python makes class methods without a decorator.
const IMPLICIT_CLASS_METHODS: &[&str] = &["__init_subclass__", "__class_getitem__"];

/// The instances `writer` runs on never hash with `hash`, a `__hash__` of an ancestor of the
/// writer's class: a class between them defines its own `__hash__`, or `__eq__` alone, which
/// leaves its instances unhashable.
fn hashes_otherwise(decls: &ProjectDecls, writer: &Method<'_>, hash: &Method<'_>) -> bool {
    let (class, hash_class) = (writer.class(), hash.class());
    decls.ancestors_of(&class).contains(&hash_class)
        && decls.hash_owner(&class).is_some_and(|c| c != hash_class)
}

/// A stored hash value (`self._hash`, `self._hashcode`, `self.hash_value`): the hash itself, not
/// a field it is computed from.
fn is_stored_hash(attr: &str) -> bool {
    matches!(
        attr.trim_start_matches('_').to_ascii_lowercase().as_str(),
        "hash" | "hashcode" | "hash_code" | "hashvalue" | "hash_value"
    )
}

/// Filter 1 (`MUT107`) also covers the private methods the family's initialization methods
/// call by name on any receiver: `__new__` sets up the instance it allocates
/// (`self = object.__new__(cls)`, then `self.__setstate(state)`), which is no call on its own
/// `self`. The names come from the summaries (`EffectSummary::private_calls`): an initializer
/// of the family may be in another file.
fn private_methods_named_in_initializers<'m>(
    cx: &CheckContext<'_>,
    family: &[&'m Method<'_>],
) -> FxHashSet<&'m FuncKey> {
    let called: FxHashSet<&str> = family
        .iter()
        .filter(|m| INIT_METHODS.contains(&m.name()))
        .filter_map(|m| cx.index.summary(&m.key))
        .flat_map(|s| s.private_calls.iter().map(String::as_str))
        .collect();
    family
        .iter()
        .filter(|m| called.contains(m.name()))
        .map(|m| &m.key)
        .collect()
}

// ============================================================================ MUT108 / MUT110

mod readonly_mutation;

pub use readonly_mutation::{CHAIN_RULES, ChainFacts, chain_facts};

/// `MUT108` (in-place changes of frozen dataclass / `NamedTuple` / `dataclass_transform` fields,
/// `Final` / `ReadOnly` attributes, and `ReadOnly` `TypedDict` keys) and `MUT110` (`Sequence` /
/// `Mapping` parameters handed to functions that change them), through one analysis.
pub(crate) fn mut108_mut110(cx: &CheckContext<'_>, out: &mut Vec<Diagnostic>) {
    readonly_mutation::run(cx, out);
}

// ==================================================================================== MUT112

mod conditional_ownership;

pub(crate) use conditional_ownership::mut112;

// ==================================================================================== MUT109

mod invalidation;

/// `MUT109`: writes to declared `invalidation` targets that some normal path leaves
/// unannounced (no call reaches one of the contract's invalidators).
pub(crate) fn mut109(
    cx: &CheckContext<'_>,
    out: &mut Vec<Diagnostic>,
) -> Result<(), RuleConfigError> {
    invalidation::run(cx, out)
}

// ==================================================================================== MUT113

mod iterated_mutation;

/// `MUT113` (opt-in): a dict, set, or deque of the instance changed while a method iterates it,
/// by another method while a generator is suspended inside the loop, or by a function the loop's
/// body calls.
pub(crate) fn mut113(cx: &CheckContext<'_>, out: &mut Vec<Diagnostic>) {
    iterated_mutation::run(cx, out);
}

// ==================================================================================== MUT106

mod iter_consumption;

/// `MUT106` (opt-in): a single-pass iterator or generator consumed a second time.
pub(crate) fn mut106(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    iter_consumption::run(cx, unit, out);
}

// ==================================================================================== MUT203

struct ReadOwnerRule<'r> {
    /// How a report names the contract ([`heki_env::contract_label`]).
    label: String,
    targets: Vec<ContractTarget>,
    allowance: Allowance,
    note: Option<&'r str>,
    /// `unresolved = "report"`: reads heki cannot tie to a class are reported (`MUT206`).
    report_unresolved: bool,
}

/// Where one access (a read, a call) is reported.
pub(crate) struct DelegatedReport {
    pub func: FuncKey,
    /// Hops from `func` down to the accessing function (empty: `func` accesses itself).
    pub chain: Vec<CallHop>,
}

impl DelegatedReport {
    /// The span of the report of the access at `access`: the access itself, or the call of the
    /// first hop.
    pub(crate) fn span(&self, access: Span) -> Span {
        self.chain.first().map_or(access, |hop| hop.span)
    }
}

/// The reports of the accesses of one function, by groups of accesses that [`delegated_reports`]
/// reports at the same places (the same object parameters, allowance, and family, and one target
/// and member named): the walk runs once per group, and a report at a call site stands for the
/// sites of every access of its group, so the work grows with the accesses plus the reports, not
/// with their product. The generator expressions of a function share one qualname, and so one
/// key whose every call site runs the accesses of all of them.
pub(crate) struct AccessGroups<K> {
    groups: FxHashMap<K, AccessGroup>,
}

struct AccessGroup {
    sites: SiteGroup,
    /// The reports the walk finds at the access itself, where every access of the group is
    /// reported.
    at_access: Vec<DelegatedReport>,
}

impl<K: Eq + std::hash::Hash> AccessGroups<K> {
    pub(crate) fn new() -> Self {
        Self {
            groups: FxHashMap::default(),
        }
    }

    /// Report the access at `site` of the group `key`, whose reports `walk` finds for its first
    /// access: `add` adds a report found there, standing for `sites`.
    pub(crate) fn report<'u, R: Ord>(
        &mut self,
        reports: &mut MergedReports<'u, R>,
        key: K,
        site: SiteId,
        walk: impl FnOnce() -> Vec<DelegatedReport>,
        mut add: impl FnMut(&mut MergedReports<'u, R>, &DelegatedReport, Sites),
    ) {
        match self.groups.entry(key) {
            Entry::Occupied(group) => {
                let group = group.get();
                reports.add_to_group(group.sites, site.clone());
                for r in &group.at_access {
                    add(reports, r, Sites::One(site.clone()));
                }
            }
            Entry::Vacant(group) => {
                let sites = reports.group();
                reports.add_to_group(sites, site.clone());
                let found = walk();
                // In the walk's order: the first report of a key gives its message and chain.
                for r in &found {
                    let stands_for = if r.chain.is_empty() {
                        Sites::One(site.clone())
                    } else {
                        Sites::Group(sites)
                    };
                    add(reports, r, stands_for);
                }
                let at_access = found.into_iter().filter(|r| r.chain.is_empty()).collect();
                group.insert(AccessGroup { sites, at_access });
            }
        }
    }
}

/// `MUT203`: reads of `read-owners` targets outside their allowed readers. Read sites are
/// local facts; a read in a private helper with project callers is reported at the call sites
/// of its outermost non-private callers, found by walking the reverse call graph.
pub(crate) fn mut203<'c>(
    cx: &CheckContext<'c>,
    out: &mut Vec<Diagnostic>,
) -> Result<(), RuleConfigError> {
    let (reads, unresolved) = (
        cx.selection.is_enabled("MUT203"),
        cx.selection.is_enabled("MUT206"),
    );
    if !reads && !unresolved || cx.config.read_owners.is_empty() {
        return Ok(());
    }
    let all_classes = project_classes(cx);
    let mut rules = Vec::new();
    let escaping = std::cell::OnceCell::new();
    for (index, rule) in cx.config.read_owners.iter().enumerate() {
        let report_unresolved =
            reports_unresolved(rule.unresolved.as_deref(), "read-owners", &rule.name)?;
        rules.push(ReadOwnerRule {
            label: heki_env::contract_label("read-owners", index, &rule.name),
            targets: parse_read_targets(
                cx,
                &all_classes,
                &rule.targets,
                "read-owners",
                "read-owners.targets",
            )?,
            allowance: Allowance::new(&AllowanceKeys {
                allowed: (&rule.allowed_readers, "read-owners.allowed-readers"),
                forbidden: (&rule.forbidden_readers, "read-owners.forbidden-readers"),
                exempt: (&rule.exempt_methods, "read-owners.exempt-methods"),
            })?
            .with_attribution(attribution(
                rule.attribution.as_deref(),
                "read-owners",
                &rule.name,
            )?)
            .with_own_receiver(allows_own_receiver(
                rule.own_receiver.as_deref(),
                "read-owners",
                &rule.name,
            )?)
            .with_protocols(named_protocols(
                cx,
                &all_classes,
                (&rule.allowed_protocols, rule.protocol_subclasses),
                "read-owners.allowed-protocols",
            )?)
            .with_escaping(escaping_for(cx, report_unresolved, &escaping)),
            note: rule.message.as_deref(),
            report_unresolved,
        });
    }
    unresolved::mut206(cx, &rules, out);
    if !reads {
        return Ok(());
    }
    let sites = cx.sites_by_caller();
    let mut reports = MergedReports::default();
    let mut inside: Vec<Vec<InsideAllowed<'_>>> = rules
        .iter()
        .map(|r| r.targets.iter().map(|_| InsideAllowed::default()).collect())
        .collect();
    for (key, s) in cx.defined_functions() {
        let reader = key.fqn();
        let mut groups = AccessGroups::new();
        for read in s
            .typed_field_read_sites
            .iter()
            .filter(|r| r.counts_for_contracts())
        {
            for (contract, (rule, inside)) in rules.iter().zip(&mut inside).enumerate() {
                if rule
                    .allowance
                    .allows_protocol(cx.decls, &read.class_qualname, &read.attr)
                {
                    continue;
                }
                for (k, (t, inside)) in rule
                    .targets
                    .iter()
                    .zip(inside.iter_mut())
                    .enumerate()
                    .filter(|(_, (t, _))| t.matches(&read.class_qualname, &read.attr))
                {
                    let named =
                        matched_name(&t.text, || format!("{}.{}", read.class_qualname, read.attr));
                    let walk = || {
                        delegated_reports(
                            cx,
                            &sites,
                            (&rule.allowance, &t.classes),
                            (key, read.obj_params, read.obj_params),
                            inside,
                        )
                    };
                    let add = |reports: &mut MergedReports<'c, usize>,
                               r: &DelegatedReport,
                               sites| {
                        let Some(unit) = cx.index.file_of(&r.func).and_then(|p| cx.unit(p)) else {
                            return;
                        };
                        let quoted = format!("`{named}`");
                        let name = Named {
                            target: &t.text,
                            named: &quoted,
                            rank: k,
                            contract,
                        };
                        reports.add(
                            unit,
                            &r.func,
                            r.span(read.span),
                            (contract, name),
                            sites,
                            rule.note,
                            || {
                                let via = if r.chain.is_empty() {
                                    String::new()
                                } else {
                                    format!(" via `{reader}`")
                                };
                                format!(
                                    "`{}` reads `{named}` ({}){via} {}",
                                    r.func.qualname,
                                    read.kind.describe(),
                                    rule.allowance.where_text(
                                        "read owners",
                                        "forbidden readers",
                                        &rule.label
                                    )
                                )
                            },
                            || r.chain.clone(),
                        );
                    };
                    let group = (read.obj_params, contract, k, named.clone());
                    groups.report(&mut reports, group, (reader.clone(), read.span), walk, add);
                }
            }
        }
    }
    reports.emit(cx, "MUT203", out);
    Ok(())
}

/// One state of the walk from an access up the reverse call graph.
struct WalkState {
    func: FuncKey,
    /// Which facts of `func` run the access: its body, or what iterating its result runs.
    facts: Facts,
    /// `func` runs the access itself: it is the accessing function, or it calls the function
    /// below (not only returns its result).
    runs_access: bool,
    /// Parameters of `func` the accessed object may be, and those the object the member is
    /// looked up on may be ([`Allowance::owns`]).
    objs: (u64, u64),
    /// A function from the accessing one up to `func` that runs the access falls under
    /// `forbidden-*` (`Allowance::in_scope`).
    in_scope: bool,
    /// The state below (towards the accessing function) and the hops from `func` down to it.
    down: Option<(usize, Vec<CallHop>)>,
}

/// Where an access in `func` (at `span`, to an object that may be its parameters `obj`) is
/// reported: at the access itself, or — for a private helper with project callers running it —
/// at every call site of each outermost non-private caller, found breadth first over
/// `(function, facts, object parameters)` states (cycle-safe) along the call sites that run
/// those facts. A function returning a generator that accesses does not run the access: the
/// walk goes on to where the object it returns is iterated. Allowed functions end a path; an
/// initializer accessing its own instance and a function that only runs inside allowed
/// contexts (MUT201's rules) are not reported. With `forbidden-*`, a report needs a forbidden
/// function running the access between the reporting function and the access (both included).
/// With `attribution = "lexical"`, the access is judged in the function holding it alone, where
/// only a class's own initialization covers what it runs. `looked_up` are the parameters of
/// `func` the object the member is looked up on may be ([`Allowance::owns`]), followed up the
/// walk as `obj` is.
pub(crate) fn delegated_reports<'k>(
    cx: &CheckContext<'_>,
    sites: &SitesByCaller<'_>,
    (allowance, family): (&Allowance, &FxHashSet<String>),
    (func, obj, looked_up): (&'k FuncKey, u64, u64),
    inside: &mut InsideAllowed<'k>,
) -> Vec<DelegatedReport> {
    if allowance.is_lexical() {
        let (judged, (judged_obj, judged_looked_up)) =
            allowance.judged_access(cx.decls, func, (obj, looked_up));
        if allowance.allows(&judged)
            || !allowance.in_scope(&judged)
            || allowance.owns(cx, &judged, family, (judged_obj, judged_looked_up))
            || judged_inside_allowed(cx, sites, allowance, family, (func, obj), inside)
        {
            return Vec::new();
        }
        return vec![DelegatedReport {
            func: judged.into_owned(),
            chain: Vec::new(),
        }];
    }
    if allowance.allows(func) {
        return Vec::new();
    }
    let in_scope = allowance.in_scope(func);
    let reports_itself = |func: &FuncKey, objs: (u64, u64), in_scope: bool, first: FirstCallers| {
        in_scope
            && !allowance.owns(cx, func, family, objs)
            && !runs_inside_allowed_through(cx, sites, allowance, family, func, objs.0, first)
    };
    // A function handed on as a value has callers heki does not see: it keeps the accesses it
    // runs, as a function without callers does.
    let escapes = allowance.escapes(func);
    if escapes || !delegates(cx, sites, func, Handover::ByImplementation) {
        if !in_scope
            || allowance.owns(cx, func, family, (obj, looked_up))
            || !escapes && judged_inside_allowed(cx, sites, allowance, family, (func, obj), inside)
        {
            return Vec::new();
        }
        return vec![DelegatedReport {
            func: func.clone(),
            chain: Vec::new(),
        }];
    }
    let facts = cx
        .index
        .summary(func)
        .map_or(Facts::Body, EffectSummary::body_facts);
    let mut states = vec![WalkState {
        func: func.clone(),
        facts,
        runs_access: true,
        objs: (obj, looked_up),
        in_scope,
        down: None,
    }];
    let mut visited: FxHashSet<(FuncKey, Facts, (u64, u64), bool)> = FxHashSet::default();
    visited.insert((func.clone(), facts, (obj, looked_up), in_scope));
    // The report at state `j` of the walk: at the access itself, or at `func`'s call site of
    // the state below, with the hops down to the access.
    let report_at = |states: &[WalkState], j: usize, func: &FuncKey, mut chain: Vec<CallHop>| {
        let mut j = j;
        while let Some((below, h)) = &states[j].down {
            chain.extend(h.iter().cloned());
            j = *below;
        }
        DelegatedReport {
            func: func.clone(),
            chain,
        }
    };
    // What a caller's call sites hand over or keep is decided once per caller, whichever of its
    // sites the walk reaches (a function calling its generator expressions at many sites).
    let mut delegating: FxHashMap<&FuncKey, bool> = FxHashMap::default();
    let mut reporting: FxHashMap<(&FuncKey, (u64, u64), bool), bool> = FxHashMap::default();
    let mut out = Vec::new();
    let mut i = 0;
    while i < states.len() {
        let (callee, facts, objs, below_in_scope) = (
            states[i].func.clone(),
            states[i].facts,
            states[i].objs,
            states[i].in_scope,
        );
        let (handing_over, keeping): (Vec<Caller<'_>>, Vec<Caller<'_>>) =
            callers_of(cx, sites, &callee, facts)
                .into_iter()
                .partition(|c| Handover::ByImplementation.hands_over(c));

        // Callers through a protocol declaring the method leave the accesses with the function
        // running them, also when its other callers take them over.
        if states[i].runs_access
            && !keeping.is_empty()
            && reports_itself(
                &callee,
                objs,
                below_in_scope,
                FirstCallers::KeptBy(Handover::ByImplementation),
            )
        {
            out.push(report_at(&states, i, &callee, Vec::new()));
        }
        for c in handing_over {
            let caller_objs = (c.bc.caller_params_of(objs.0), c.bc.caller_params_of(objs.1));
            let hop = |from: &FuncKey, to: &FuncKey| CallHop {
                caller: from.clone(),
                callee: to.fqn(),
                span: c.site.span,
                path: c.path.to_path_buf(),
            };
            let hops = match c.via {
                None => vec![hop(c.caller, &callee)],
                Some(p) => vec![hop(c.caller, p), hop(p, &callee)],
            };
            if c.into == Facts::Result {
                // The function returning the object does not run the access.
                let state = (c.caller.clone(), Facts::Result, caller_objs, below_in_scope);
                if visited.insert(state) {
                    states.push(WalkState {
                        func: c.caller.clone(),
                        facts: Facts::Result,
                        runs_access: false,
                        objs: caller_objs,
                        in_scope: below_in_scope,
                        down: Some((i, hops)),
                    });
                }
                continue;
            }
            if allowance.allows(c.caller) || c.via.is_some_and(|p| allowance.allows(p)) {
                continue;
            }
            let in_scope = below_in_scope
                || allowance.in_scope(c.caller)
                || c.via.is_some_and(|p| allowance.in_scope(p));
            let escapes = allowance.escapes(c.caller);
            let delegating = !escapes
                && *delegating
                    .entry(c.caller)
                    .or_insert_with(|| delegates(cx, sites, c.caller, Handover::ByImplementation));
            if escapes {
                if in_scope && !allowance.owns(cx, c.caller, family, caller_objs) {
                    out.push(report_at(&states, i, c.caller, hops));
                }
            } else if delegating {
                if visited.insert((c.caller.clone(), c.into, caller_objs, in_scope)) {
                    states.push(WalkState {
                        func: c.caller.clone(),
                        facts: c.into,
                        runs_access: true,
                        objs: caller_objs,
                        in_scope,
                        down: Some((i, hops)),
                    });
                }
            } else if *reporting
                .entry((c.caller, caller_objs, in_scope))
                .or_insert_with(|| {
                    reports_itself(c.caller, caller_objs, in_scope, FirstCallers::All)
                })
            {
                out.push(report_at(&states, i, c.caller, hops));
            }
        }
        i += 1;
    }
    out
}

// ==================================================================================== MUT204

mod call_owners;

pub(crate) use call_owners::{
    CallTargets, FunctionMatchers, Subclasses, call_verb, group_texts, mut204, parse_call_targets,
    site_callee, split_arguments,
};
