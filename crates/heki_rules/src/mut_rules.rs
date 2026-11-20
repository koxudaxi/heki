//! Family 1 rules: `MUT101`, `MUT106`, `MUT107`, `MUT108`, `MUT109`, `MUT110`, `MUT112`,
//! `MUT113`, `MUT201`, `MUT202`, `MUT203`, `MUT204` (docs/ANALYSIS_AND_KB_SPEC.md section 2.3).

use crate::{
    CheckContext, DottedPattern, ModuleUnit, RuleConfigError, SitesByCaller, has_glob_syntax,
};
use heki_ast::Span;
use heki_diagnostics::Diagnostic;
use heki_effect::readonly::{self, Derivation};
use heki_effect::{
    AttrKeyPath, BoundCallee, CONTAINER_MUTATORS, CacheEvent, CacheFlow, CallHop, CallSite, Callee,
    Construction, Delivery, EffectSummary, Facts, FlowBlock, FuncKey, INITIALIZERS, ProjectDecls,
    ProjectIndex, ReturnStep, SelfAttrWrite, Shared, SiteRole, TypeCtx, TypedFieldWriteSite, Val,
    WriteKind, attr_writes, self_attr_writes,
};
use heki_env::HekiConfig;
use heki_ir::{BlockId, FnIndex, Inst, InstKind, IrFunction, VarId};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

// ==================================================================================== MUT101

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
            for (&attr, read_keys) in &reads {
                let writes: Vec<&AttrKeyPath> = local
                    .mutates_self_attrs
                    .iter()
                    .filter(|w| w.attr == attr)
                    .collect();
                if writes.is_empty() {
                    continue;
                }
                let first = flows.iter().zip(&invalidations).find_map(|(flow, points)| {
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
                    own.first().map(|w| w.span)
                });
                let Some(span) = first else {
                    continue;
                };
                // Filter 3: disjoint constant dict keys.
                if disjoint_keys(
                    read_keys.iter().copied(),
                    writes.iter().map(|w| &w.const_key),
                ) {
                    continue;
                }
                found(StaleWrite {
                    prop: p,
                    writer: m,
                    attr,
                    span,
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
                    cx.emit(
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
                );
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
                };
                if invalidates {
                    out.invalidations.insert((b, index));
                } else if reads {
                    out.reads.insert((b, index));
                }
            }
        }
        out
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
        let point_after = |b: BlockId, after: Option<u32>| {
            self.invalidations
                .iter()
                .any(|(pb, pi)| *pb == b && after.is_none_or(|a| *pi > a))
        };
        let leaves_covered = |b: &FlowBlock, covered: &[bool]| {
            !b.returns && b.succs.iter().all(|s| covered[s.0 as usize])
        };
        // Covered from each block's start: a greatest fixpoint from `true`.
        let mut covered = vec![true; flow.blocks.len()];
        loop {
            let mut changed = false;
            for (i, b) in flow.blocks.iter().enumerate().rev() {
                let id = BlockId(u32::try_from(i).unwrap_or(u32::MAX));
                let v = point_after(id, None) || leaves_covered(b, &covered);
                if v != covered[i] {
                    covered[i] = v;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        point_after(block, Some(index)) || leaves_covered(&flow.blocks[block.0 as usize], &covered)
    }

    /// Every path from the entry to the instruction passes an invalidation with no read of the
    /// property after it: the cached value is gone when the write happens. A block an exception
    /// enters may hold any value, and a block no path enters keeps its first value.
    fn absent_before(&self, flow: &CacheFlow, block: BlockId, index: u32) -> bool {
        let events_of = |b: BlockId| flow.blocks[b.0 as usize].events.iter().map(|(i, _)| *i);
        let through = |b: BlockId, mut absent: bool, upto: Option<u32>| {
            for i in events_of(b).filter(|i| upto.is_none_or(|u| *i < u)) {
                if self.invalidations.contains(&(b, i)) {
                    absent = true;
                } else if self.reads.contains(&(b, i)) {
                    absent = false;
                }
            }
            absent
        };
        let n = flow.blocks.len();
        let mut preds: Vec<Vec<BlockId>> = vec![Vec::new(); n];
        let mut handlers = vec![false; n];
        for (i, b) in flow.blocks.iter().enumerate() {
            for s in &b.succs {
                preds[s.0 as usize].push(BlockId(u32::try_from(i).unwrap_or(u32::MAX)));
            }
            if let Some(h) = b.exc_target {
                handlers[h.0 as usize] = true;
            }
        }
        let reachable = reachable_blocks(flow);
        // Absent at each block's start: a greatest fixpoint from `true` (the entry: present).
        let mut absent_in = vec![true; n];
        absent_in[0] = false;
        loop {
            let mut changed = false;
            for i in (0..n).filter(|i| reachable[*i]) {
                let v = i != 0
                    && !handlers[i]
                    && preds[i]
                        .iter()
                        .all(|p| through(*p, absent_in[p.0 as usize], None));
                if v != absent_in[i] {
                    absent_in[i] = v;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        through(block, absent_in[block.0 as usize], Some(index))
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
/// while it is suspended: a changed function, and one whose `calling` sites changed, is read for
/// the families of the generators whose objects its calls may receive, also through the functions
/// handing them on). With `in_place_changes` (`MUT112` selected), every changed method is read for
/// its own family: a store in `self.<attr>` is reported when a method of the family changes the
/// attribute in place. A family's classes include the ancestors whose methods it inherits. A
/// changed writer is otherwise reported in its own file only.
pub fn family_readers(
    index: &ProjectIndex,
    decls: &ProjectDecls,
    changed: &[FuncKey],
    calling: &[&FuncKey],
    in_place_changes: bool,
) -> BTreeSet<String> {
    let has_reported_reads = |class: &String| {
        decls.methods_of(class).iter().any(|k| {
            k.qualname.ends_with(".__hash__")
                || index.summary(k).is_some_and(|s| s.is_cached_property)
        })
    };
    let has_suspending_generator = |class: &String| {
        decls
            .methods_of(class)
            .iter()
            .any(|k| iterated_mutation::suspends(index, k))
    };
    let methods = changed.iter().filter_map(|key| {
        let (owner, name) = key.qualname.rsplit_once('.')?;
        let class = format!("{}.{owner}", key.module);
        decls.is_project_class(&class).then_some((key, class, name))
    });
    let mut roots = BTreeSet::new();
    for key in changed.iter().chain(calling.iter().copied()) {
        for generator in iterated_mutation::generators_reached(index, key) {
            if let Some((owner, _)) = generator.qualname.rsplit_once('.') {
                roots.insert(format!("{}.{owner}", generator.module));
            }
        }
    }
    for (key, class, name) in methods {
        if in_place_changes
            || name == "__hash__"
            || index.summary(key).is_none_or(|s| s.is_cached_property)
        {
            roots.insert(class.clone());
        }
        // A method that overrides an inherited one (or stops doing so) changes which of the
        // ancestors' methods the class family inherits.
        let overrides = decls.ancestors_of(&class).iter().any(|a| {
            decls
                .methods_of(a)
                .iter()
                .any(|k| k.qualname.rsplit('.').next() == Some(name))
        });
        if overrides {
            roots.insert(class.clone());
        }
        let suspending = decls
            .ancestors_of(&class)
            .iter()
            .chain(&decls.subclasses_of(&class))
            .chain([&class])
            .any(has_suspending_generator);
        if suspending {
            roots.insert(class.clone());
        }
        // Initialization, and the private helpers it alone calls, write nothing a cached
        // property or a hash reads.
        roots.extend(
            decls
                .ancestors_of(&class)
                .into_iter()
                .chain(decls.subclasses_of(&class))
                .chain([class])
                .filter(has_reported_reads),
        );
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

mod stale_reads;

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
fn split_item(text: &str) -> Option<(&str, &str)> {
    let quote = match text.as_bytes() {
        [.., q @ (b'"' | b'\''), b']'] => char::from(*q),
        _ => return None,
    };
    let open = format!("[{quote}");
    let start = text.rfind(&open)?;
    let key = text.get(start + 2..text.len() - 2)?;
    (!key.contains(quote)).then_some((&text[..start], key))
}

/// Every project class (fqn), sorted.
pub(crate) fn project_classes(cx: &CheckContext<'_>) -> Vec<String> {
    let mut all: Vec<String> = cx
        .index
        .files
        .values()
        .flat_map(|f| {
            f.decls
                .classes
                .keys()
                .map(move |q| format!("{}.{q}", f.module_name))
        })
        .collect();
    all.sort();
    all
}

fn class_set(cx: &CheckContext<'_>, pattern: &DottedPattern, all: &[String]) -> FxHashSet<String> {
    let mut out = FxHashSet::default();
    for c in all.iter().filter(|c| pattern.matches(c)) {
        out.insert(c.clone());
        out.extend(cx.decls.subclasses_of(c));
    }
    out
}

/// Parse `Class.attr` (and `Class.attr["key"]`) targets: a target without a dot is
/// `InvalidTarget { section }`, a bad glob `Glob { context }`.
pub(crate) fn parse_targets(
    cx: &CheckContext<'_>,
    all: &[String],
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
        let Some((class, attr)) = base.rsplit_once('.') else {
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
            classes: class_set(cx, &class, all),
        });
    }
    Ok(out)
}

/// [`parse_targets`] for reads: an item target is `ItemTarget { section }` (a read site is an
/// attribute read).
pub(crate) fn parse_read_targets(
    cx: &CheckContext<'_>,
    all: &[String],
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

/// The functions a contract lets touch its targets: `allowed-*` patterns (matched against the
/// fqn and the qualname) and `exempt-methods` (a plain name matches the last qualname
/// component; an entry with a dot or a glob is a pattern like `allowed-*`). `forbidden-*`
/// patterns (matched like `allowed-*`) narrow where the contract applies: when set, only
/// accesses by them are reported, and the allowed functions stay exceptions inside them.
#[derive(Debug, Default)]
pub(crate) struct Allowance {
    allowed: Vec<DottedPattern>,
    exempt_names: FxHashSet<String>,
    forbidden: Vec<DottedPattern>,
    /// Functions the contract guards (`call-owners` targets): they are part of the guarded
    /// operation, whose calls are checked where they are made, so they may use each other.
    targets: Vec<FunctionMatcher>,
}

/// Project functions a `call-owners` / `call-targets` entry names: the whole text by fqn or
/// qualname (dotted suffixes, globs), and, for `Class.method`, the overrides of the method in
/// the class family the class part names.
#[derive(Debug)]
pub(crate) struct FunctionMatcher {
    whole: DottedPattern,
    method: Option<(FxHashSet<String>, DottedPattern)>,
}

impl FunctionMatcher {
    pub(crate) fn new(
        cx: &CheckContext<'_>,
        all_classes: &[String],
        text: &str,
        context: &'static str,
    ) -> Result<Self, RuleConfigError> {
        let mut method = None;
        if let Some((class, name)) = text.rsplit_once('.') {
            let owners = class_set(cx, &DottedPattern::new(class, context)?, all_classes);
            if !owners.is_empty() {
                method = Some((owners, DottedPattern::new(name, context)?));
            }
        }
        Ok(Self {
            whole: DottedPattern::new(text, context)?,
            method,
        })
    }

    pub(crate) fn matches(&self, key: &FuncKey) -> bool {
        self.whole.matches(&key.fqn())
            || self.whole.matches(&key.qualname)
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

    /// Every project class the whole text names, plus their subclasses.
    pub(crate) fn classes(
        &self,
        cx: &CheckContext<'_>,
        all_classes: &[String],
    ) -> FxHashSet<String> {
        class_set(cx, &self.whole, all_classes)
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
            forbidden: patterns(keys.forbidden.0, keys.forbidden.1)?,
            targets: Vec::new(),
        };
        for e in keys.exempt.0 {
            if e.contains('.') || has_glob_syntax(e) {
                out.allowed.push(DottedPattern::new(e, keys.exempt.1)?);
            } else {
                out.exempt_names.insert(e.clone());
            }
        }
        Ok(out)
    }

    pub(crate) fn allows(&self, key: &FuncKey) -> bool {
        let name = key.qualname.rsplit('.').next().unwrap_or(&key.qualname);
        if self.exempt_names.contains(name) {
            return true;
        }
        let fqn = key.fqn();
        self.allowed
            .iter()
            .any(|p| p.matches(&fqn) || p.matches(&key.qualname))
            || self.targets.iter().any(|t| t.matches(key))
    }

    /// The same allowance, with the guarded functions `targets` allowed too.
    pub(crate) fn with_targets(self, targets: Vec<FunctionMatcher>) -> Self {
        Self { targets, ..self }
    }

    /// Whether the contract has `forbidden-*` patterns.
    pub(crate) fn has_forbidden(&self) -> bool {
        !self.forbidden.is_empty()
    }

    /// Whether an access by `key` falls under the contract: every function without
    /// `forbidden-*`, else the functions they name.
    pub(crate) fn in_scope(&self, key: &FuncKey) -> bool {
        if self.forbidden.is_empty() {
            return true;
        }
        let fqn = key.fqn();
        self.forbidden
            .iter()
            .any(|p| p.matches(&fqn) || p.matches(&key.qualname))
    }

    /// `outside the <owners> of `name``, or `inside the forbidden <who> of `name`` for a contract
    /// with `forbidden-*`.
    pub(crate) fn where_text(&self, owners: &str, forbidden: &str, name: &str) -> String {
        if self.has_forbidden() {
            format!("inside the {forbidden} of `{name}`")
        } else {
            format!("outside the {owners} of `{name}`")
        }
    }
}

/// A private helper delegates responsibility to its project callers: the call sites that run
/// its own body (`EffectSummary::body_facts`) and so receive its writes and reads. A private
/// `async def` only scheduled (never awaited) or a private generator whose generators all
/// escape keeps its reports. So does a private function that only private functions of its own
/// recursion call (`_walk` calling itself, exposed as a callback): going up through private
/// callers never reaches a function that reports, so the access would be reported nowhere.
fn delegates(cx: &CheckContext<'_>, sites: &SitesByCaller<'_>, key: &FuncKey) -> bool {
    if !is_private(&key.qualname) {
        return false;
    }
    let callers_running = |func: &FuncKey| {
        cx.index
            .summary(func)
            .map_or_else(Vec::new, |s| callers_of(cx, sites, func, s.body_facts()))
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
/// their callers are not known exactly.
fn runs_inside_allowed(
    cx: &CheckContext<'_>,
    sites: &SitesByCaller<'_>,
    allowance: &Allowance,
    family: &FxHashSet<String>,
    func: &FuncKey,
    obj: u64,
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
    while let Some((callee, facts, obj)) = queue.pop_front() {
        let callers = callers_of(cx, sites, &callee, facts);
        if callers.is_empty() {
            // A function without project callers, outside every allowed context.
            return false;
        }
        for c in callers {
            let caller_obj = c.bc.caller_params_of(obj);
            // A function returning the object does not run what iterating it runs.
            if c.into == Facts::Result {
                if visited.insert((c.caller.clone(), Facts::Result, caller_obj)) {
                    queue.push_back((c.caller.clone(), Facts::Result, caller_obj));
                }
                continue;
            }
            if allowance.allows(c.caller)
                || c.via.is_some_and(|p| allowance.allows(p))
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
/// positional arguments after it, keyword arguments by name.
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
    bound.extend(args.iter().enumerate().map(|(i, v)| (i + offset, *v)));
    for (name, v) in kwargs {
        if let Some(pos) = name
            .as_ref()
            .and_then(|n| decl.param_names.iter().position(|p| p == n))
        {
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

/// The reports of a contract rule, merged by `(reporting function, span, target)`: the first
/// report of a key gives the message, the help, and the related chain, and every distinct violation site
/// behind the key counts (`Diagnostic::sites`), so a second write inside a private helper that is
/// reported at its caller is visible to the Ratchet baseline.
#[derive(Default)]
pub(crate) struct MergedReports<'u> {
    reports: BTreeMap<(FuncKey, Span, String), MergedReport<'u>>,
}

struct MergedReport<'u> {
    unit: &'u ModuleUnit<'u>,
    message: String,
    help: Option<String>,
    chain: Vec<CallHop>,
    sites: BTreeSet<SiteId>,
}

impl<'u> MergedReports<'u> {
    /// Add a report of `func` at `span` for `target`, standing for `site`, with the contract's
    /// `message` as its help (`note`); `message` and `chain` are only built for the first
    /// report of the key.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add(
        &mut self,
        unit: &'u ModuleUnit<'u>,
        func: &FuncKey,
        span: Span,
        target: &str,
        site: SiteId,
        note: Option<&str>,
        message: impl FnOnce() -> String,
        chain: impl FnOnce() -> Vec<CallHop>,
    ) {
        self.reports
            .entry((func.clone(), span, target.to_owned()))
            .or_insert_with(|| MergedReport {
                unit,
                message: message(),
                help: note.map(str::to_owned),
                chain: chain(),
                sites: BTreeSet::new(),
            })
            .sites
            .insert(site);
    }

    pub(crate) fn emit(self, cx: &CheckContext<'_>, code: &'static str, out: &mut Vec<Diagnostic>) {
        for ((func, span, target), r) in self.reports {
            let sites = u32::try_from(r.sites.len()).unwrap_or(u32::MAX);
            if let Some(d) = cx.emit(
                out,
                code,
                r.unit,
                span,
                &func.qualname,
                target,
                r.message,
                r.help,
                &r.chain,
            ) {
                d.sites = sites;
            }
        }
    }
}

/// `unresolved` values; the first is the default (writes heki cannot tie to a class are not
/// reported).
const UNRESOLVED: &[&str] = &["ignore", "report"];

/// `fresh-objects` values; the first is the default (writes that construct an object are no
/// violation).
const FRESH_OBJECTS: &[&str] = &["allow", "report"];

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
        })
}

struct OwnerRule<'r> {
    name: &'r str,
    targets: Vec<ContractTarget>,
    allowance: Allowance,
    forbid_cached: FxHashSet<String>,
    note: Option<&'r str>,
    /// `fresh-objects = "allow"`: writes constructing the object they write are allowed.
    allow_fresh: bool,
    /// `unresolved = "report"`: writes heki cannot tie to a class are reported (`MUT205`).
    report_unresolved: bool,
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
    for rule in &cx.config.write_owners {
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
        })?;
        let mut forbid_cached = FxHashSet::default();
        for p in &rule.forbid_cached_property_on {
            let p = DottedPattern::new(p, "write-owners.forbid-cached-property-on")?;
            forbid_cached.extend(class_set(cx, &p, &all_classes));
        }
        rules.push(OwnerRule {
            name: &rule.name,
            targets,
            allowance,
            forbid_cached,
            note: rule.message.as_deref(),
            allow_fresh: allows_fresh(rule.fresh_objects.as_deref(), "write-owners", &rule.name)?,
            report_unresolved: reports_unresolved(
                rule.unresolved.as_deref(),
                "write-owners",
                &rule.name,
            )?,
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
    for path in paths {
        let file = &cx.index.files[path];
        let Some(unit) = cx.unit(path) else {
            continue;
        };
        let mut funcs: Vec<_> = file.functions.iter().collect();
        funcs.sort_by(|a, b| a.0.cmp(b.0));
        for (key, s) in funcs {
            for site in &s.typed_field_write_sites {
                for rule in &rules {
                    // Constructing an object is no write of its owners' state.
                    if rule.allow_fresh && site.construction == Construction::Yes {
                        continue;
                    }
                    for (_, t) in write_hits(&rule.targets, site) {
                        let mut chain: Vec<FuncKey> = vec![key.clone()];
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
                        let kept_below = chain[1..=origin]
                            .iter()
                            .any(|k| is_private(&k.qualname) && !delegates(cx, sites, k));
                        if kept_below
                            || delegates(cx, sites, key)
                            || initializes_itself(cx, key, &t.classes, site.obj_params)
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
                        if runs_inside_allowed(
                            cx,
                            sites,
                            &rule.allowance,
                            &t.classes,
                            key,
                            site.obj_params,
                        ) {
                            continue;
                        }
                        let span = site.via_chain.first().map_or(site.span, |h| h.span);
                        let writer = site
                            .via_chain
                            .last()
                            .map_or_else(|| key.fqn(), |h| h.callee.clone());
                        reports.add(
                            unit,
                            key,
                            span,
                            &t.text,
                            (writer, site.span),
                            rule.note,
                            || {
                                let via = site
                                    .via_chain
                                    .last()
                                    .map(|h| format!(" via `{}`", h.callee))
                                    .unwrap_or_default();
                                format!(
                                    "`{}` writes `{}` ({}){via} {}",
                                    key.qualname,
                                    t.text,
                                    site.kind.as_str(),
                                    rule.allowance.where_text(
                                        "write owners",
                                        "forbidden writers",
                                        rule.name
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
            for rule in rules.iter().filter(|r| r.forbid_cached.contains(&class)) {
                cx.emit(
                    out,
                    "MUT201",
                    unit,
                    f.span,
                    &f.qualname,
                    format!("{owner}.{}", f.name),
                    format!(
                        "`@cached_property` `{}` is defined on `{owner}`, which `{}` forbids caching on",
                        f.name, rule.name
                    ),
                    rule.note.map(str::to_owned),
                    &[],
                );
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

    pub(crate) fn deep_for(&self, fqn: &str) -> Option<bool> {
        let mut found = None;
        for (p, deep) in &self.rules {
            if p.matches(fqn) {
                found = Some(found.unwrap_or(false) || *deep);
            }
        }
        found
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rules.is_empty()
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
                    || producers.deep_for(&k.fqn()).is_some() && property(k, s)
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
}

impl Ro {
    fn new(producer: String, shared: Shared, via: Vec<CallHop>) -> Option<Self> {
        shared.any().then_some(Self {
            producer,
            deep: shared.elements,
            shallow_copy: !shared.object,
            via,
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
        }
    }

    /// The origin of a value derived from this one by `step`.
    fn derive(&self, step: ReturnStep) -> Option<Self> {
        Self::new(
            self.producer.clone(),
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
    sites: &[&'i CallSite],
    inst: &'i Inst,
    worker: bool,
) -> Option<CallParts<'i>> {
    let call = CallInst::of(&inst.kind)?;
    let runner = if worker {
        heki_effect::Runner::Worker
    } else {
        heki_effect::Runner::Caller
    };
    let site = sites.iter().find(|s| {
        s.span == inst.span
            && s.role == SiteRole::Call
            && s.runner == runner
            && !s.project_callees().is_empty()
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

/// The value each of `vars` copies, through any number of copies (itself when it is no copy),
/// each value resolved once. Copies in dead code can close a cycle, which ends a walk.
fn copy_origins(f: &IrFunction, vars: impl Iterator<Item = VarId>) -> FxHashMap<VarId, VarId> {
    let sources: FxHashMap<VarId, VarId> = f
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .filter_map(|i| match i.kind {
            InstKind::Copy { dst, src } => Some((dst, src)),
            _ => None,
        })
        .collect();
    let mut origin: FxHashMap<VarId, VarId> = FxHashMap::default();
    for var in vars {
        let mut path: FxHashSet<VarId> = FxHashSet::default();
        let mut cur = var;
        let root = loop {
            if let Some(&known) = origin.get(&cur) {
                break known;
            }
            match sources.get(&cur) {
                Some(&src) if path.insert(cur) => cur = src,
                _ => break cur,
            }
        };
        origin.insert(var, root);
        origin.extend(path.into_iter().map(|v| (v, root)));
    }
    origin
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
    if !cx.enabled("MUT202", unit) {
        return;
    }
    let tcx = TypeCtx::new(&unit.input, cx.decls, cx.kb);
    for f in &unit.input.ir.functions {
        if f.is_class_body {
            continue;
        }
        Mut202Fn::new(cx, unit, &tcx, returns, f).run(out);
    }
}

struct Mut202Fn<'a> {
    cx: &'a CheckContext<'a>,
    unit: &'a ModuleUnit<'a>,
    tcx: &'a TypeCtx<'a>,
    returns: &'a ReadOnlyReturns,
    f: &'a IrFunction,
    /// The defining instruction of each of `f`'s variables, looked up per call operand.
    ix: FnIndex<'a>,
    key: FuncKey,
    types: FxHashMap<VarId, Val>,
    ro: FxHashMap<VarId, Ro>,
    /// The function's call sites in the index, every role (built on the first need).
    sites: std::cell::OnceCell<Vec<&'a CallSite>>,
}

impl<'a> Mut202Fn<'a> {
    fn new(
        cx: &'a CheckContext<'a>,
        unit: &'a ModuleUnit<'a>,
        tcx: &'a TypeCtx<'a>,
        returns: &'a ReadOnlyReturns,
        f: &'a IrFunction,
    ) -> Self {
        Self {
            cx,
            unit,
            tcx,
            returns,
            f,
            ix: FnIndex::new(f, &unit.input.ir.constants),
            key: CheckContext::key(unit, f),
            types: tcx.infer(f),
            ro: FxHashMap::default(),
            sites: std::cell::OnceCell::new(),
        }
    }

    /// The function's call sites in the index, every role.
    fn function_sites(&self) -> &[&'a CallSite] {
        self.sites.get_or_init(|| {
            self.cx
                .index
                .files
                .get(&self.unit.input.path)
                .into_iter()
                .flat_map(|f| &f.call_sites)
                .filter(|s| s.caller == self.key)
                .collect()
        })
    }

    /// The facts of `callee` that the call at `span` (a site of it) runs in this function: at
    /// the call, and where the object it returns is iterated ([`facts_run_at`]).
    fn facts_run_by_call(
        &self,
        span: Span,
        key: &FuncKey,
        callee: &'a EffectSummary,
    ) -> Vec<&'a EffectSummary> {
        let sites = self.function_sites();
        let calls_key = |s: &&&CallSite| s.project_callees().iter().any(|bc| bc.callee == *key);
        let call = sites
            .iter()
            .filter(calls_key)
            .find(|s| s.role == SiteRole::Call && s.span == span);
        // The other sites of the object the call returns copy its target.
        call.into_iter()
            .flat_map(|call| sites.iter().filter(move |s| s.target == call.target))
            .filter(calls_key)
            .filter_map(|s| facts_run_at(self.cx.index, s, callee))
            .collect()
    }

    /// The parts of a call instruction (`None` for any other instruction).
    fn call_of<'i>(&self, inst: &'i Inst) -> Option<CallParts<'i>>
    where
        'a: 'i,
    {
        // A project call as the summaries resolved it; any other call by the inferred values.
        if let Some(call) = site_call(&|v| self.ix.def_inst(v), self.function_sites(), inst, false)
        {
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
        let direct =
            |producer: String, deep: bool| Ro::new(producer, Shared::produced(deep), vec![]);
        match callee {
            Callee::Project(keys, _) => keys.iter().find_map(|k| {
                let fqn = k.fqn();
                let summary = self.cx.index.summary(k);
                if summary.is_some_and(|s| s.is_lru_cache) {
                    return direct(fqn, true);
                }
                if let Some(d) = self.returns.producers.deep_for(&fqn) {
                    return direct(fqn, d);
                }
                let r = summary?.returns_protected.as_ref()?;
                let mut via = vec![CallHop {
                    caller: self.key.clone(),
                    callee: fqn,
                    span,
                    path: self.unit.input.path.clone(),
                }];
                via.extend(heki_effect::owned_chain(&r.chain));
                Ro::new(r.producer.clone(), r.shared, via)
            }),
            Callee::External(sym) => self
                .returns
                .producers
                .deep_for(sym)
                .and_then(|d| direct(sym.clone(), d)),
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
                let step = readonly::subscript_step(self.f, inst, class);
                self.ro.get(obj)?.derive(step)
            }
            InstKind::AttrLoad { obj, attr, .. } => {
                if let Some(r) = self.ro.get(obj) {
                    return r.derive(ReturnStep::ELEMENT);
                }
                // `readonly-returns` may name a property: `obj.schema`.
                if let Some(Val::Inst(class)) = self.types.get(obj) {
                    let fqn = format!("{class}.{attr}");
                    if let Some(deep) = self.returns.producers.deep_for(&fqn) {
                        return Ro::new(fqn, Shared::produced(deep), vec![]);
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
                    self.f.const_bool(v) == Some(true)
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
        let rpo = self.f.rpo_blocks();
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
        self.propagate();
        if self.ro.is_empty() {
            return;
        }
        // Filling a memo of a shared object, or initializing one of its attributes lazily,
        // leaves what it computes as it was (as for the helpers, `memo_param_paths`). Each
        // object is read on its own: a test of one object's attribute says nothing of another's.
        let origins = copy_origins(self.f, self.ro.keys().copied());
        let objects: FxHashMap<VarId, u32> = self
            .ro
            .iter()
            .filter(|(_, r)| !r.shallow_copy)
            .map(|(v, _)| (*v, origins[v].0))
            .collect();
        let memo_fills: FxHashSet<(BlockId, u32)> = attr_writes(self.f, &objects)
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
                if let Some(r) = hit
                    && reported.insert(inst.span)
                {
                    // A value from a wrapper: the wrapper called here, then the hops down to the
                    // producer, then the hop to the mutating helper.
                    let through = r
                        .via
                        .first()
                        .map(|h| format!(" through `{}`", h.callee))
                        .unwrap_or_default();
                    let mut related = r.via.clone();
                    related.extend(chain);
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
                }
            }
        }
    }

    fn violation(&self, inst: &Inst) -> (Option<Ro>, Vec<CallHop>) {
        let direct = match &inst.kind {
            InstKind::AttrStore { obj, .. }
            | InstKind::AttrDelete { obj, .. }
            | InstKind::SubscriptStore { obj, .. }
            | InstKind::SubscriptDelete { obj, .. } => self.guarded(*obj),
            InstKind::MethodCall { recv, method, .. }
                if CONTAINER_MUTATORS.contains(&method.as_str()) =>
            {
                self.guarded(*recv)
            }
            InstKind::BinOp {
                lhs, in_place: true, ..
            } => self.guarded(*lhs).filter(|_| {
                matches!(self.types.get(lhs), Some(Val::Inst(t)) if MUTABLE_BUILTINS.contains(&t.as_str()))
            }),
            _ => None,
        };
        if direct.is_some() {
            return (direct.cloned(), Vec::new());
        }
        // Passing a read-only value to a project function that mutates that parameter, also
        // through an offloading API that calls it on a worker thread.
        let def = |v| self.ix.def_inst(v);
        let Some(call) = site_call(&def, self.function_sites(), inst, false)
            .or_else(|| site_call(&def, self.function_sites(), inst, true))
        else {
            return (None, Vec::new());
        };
        for (key, callee, pos, var) in call.bound_arguments(self.cx) {
            let Some(r) = self.ro.get(&var) else {
                continue;
            };
            // Summaries track parameters 0..64 (`mutated_param_paths` keys are `u8`).
            // A shallow copy shares only the objects below its top level: rebinding its
            // attributes changes the copy alone. Filling a memo of the shared object, or
            // initializing one of its attributes lazily, leaves what it computes as it was.
            let p = u8::try_from(pos).ok().filter(|p| *p < 64);
            let mutates = self
                .facts_run_by_call(inst.span, key, callee)
                .iter()
                .any(|facts| {
                    p.is_some_and(|p| {
                        if r.shallow_copy {
                            facts
                                .changed_param_attrs
                                .get(&p)
                                .is_some_and(|attrs| facts.changes_param_state(p, Some(attrs)))
                        } else {
                            facts.changes_param_state(p, None)
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
                return (Some(r.clone()), vec![hop]);
            }
        }
        (None, Vec::new())
    }
}

// ==================================================================================== MUT107

/// `MUT107`: a method of the class (or of a subclass) mutates an attribute `__hash__` reads
/// after initialization, so instances already stored in a `set` or as `dict` keys can no longer
/// be found. Reported at the method's first own write of the attribute, once per method and
/// attribute; the filters are `MUT101`'s (two-phase initialization, memo dictionaries,
/// disjoint constant keys) plus lazy initialization and `__hash__`'s own cache.
pub(crate) fn mut107(cx: &CheckContext<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.selection.is_enabled("MUT107") {
        return;
    }
    let mut seen = FxHashSet::default();
    for_each_class_family(cx, |methods, family, init_called| {
        let hashes: Vec<_> = methods
            .iter()
            .filter(|h| h.name() == "__hash__" && !writes_may_run_descriptors(cx.decls, h))
            .filter_map(|h| Some((h, cx.index.summary(&h.key)?)))
            .collect();
        if hashes.is_empty() {
            return;
        }
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
            for (m, Checked { unit, f }, local) in writers {
                for (attr, read_keys) in &reads {
                    let writes: Vec<&AttrKeyPath> = local
                        .mutates_self_attrs
                        .iter()
                        .filter(|w| w.attr == *attr)
                        .collect();
                    // Filter 4: stored hashes (`self._hash`, which a deserialization hook
                    // recomputes), disjoint constant keys.
                    if writes.is_empty()
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
                    let first = self_attr_writes(f).into_iter().find(|(a, w)| {
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
                    cx.emit(
                        out,
                        "MUT107",
                        unit,
                        write.span,
                        &m.key.qualname,
                        format!("{class_qualname}.{attr}->__hash__"),
                        format!(
                            "`{}` mutates `self.{attr}`, which `{}` reads, so instances already stored in a `set` or used as `dict` keys can no longer be found",
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
    name: &'r str,
    targets: Vec<ContractTarget>,
    allowance: Allowance,
    note: Option<&'r str>,
    /// `unresolved = "report"`: reads heki cannot tie to a class are reported (`MUT206`).
    report_unresolved: bool,
}

/// Where one access (a read, a call) is reported.
pub(crate) struct DelegatedReport {
    pub func: FuncKey,
    pub span: Span,
    /// Hops from `func` down to the accessing function (empty: `func` accesses itself).
    pub chain: Vec<CallHop>,
}

/// `MUT203`: reads of `read-owners` targets outside their allowed readers. Read sites are
/// local facts; a read in a private helper with project callers is reported at the call sites
/// of its outermost non-private callers, found by walking the reverse call graph.
pub(crate) fn mut203(
    cx: &CheckContext<'_>,
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
    for rule in &cx.config.read_owners {
        rules.push(ReadOwnerRule {
            name: &rule.name,
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
            })?,
            note: rule.message.as_deref(),
            report_unresolved: reports_unresolved(
                rule.unresolved.as_deref(),
                "read-owners",
                &rule.name,
            )?,
        });
    }
    unresolved::mut206(cx, &rules, out);
    if !reads {
        return Ok(());
    }
    let sites = cx.sites_by_caller();
    let mut reports = MergedReports::default();
    for (key, s) in cx.defined_functions() {
        for read in &s.typed_field_read_sites {
            for rule in &rules {
                for t in rule
                    .targets
                    .iter()
                    .filter(|t| t.matches(&read.class_qualname, &read.attr))
                {
                    let found = delegated_reports(
                        cx,
                        &sites,
                        &rule.allowance,
                        &t.classes,
                        key,
                        read.obj_params,
                        read.span,
                    );
                    for r in found {
                        let Some(unit) = cx.index.file_of(&r.func).and_then(|p| cx.unit(p)) else {
                            continue;
                        };
                        reports.add(
                            unit,
                            &r.func,
                            r.span,
                            &t.text,
                            (key.fqn(), read.span),
                            rule.note,
                            || {
                                let via = if r.chain.is_empty() {
                                    String::new()
                                } else {
                                    format!(" via `{}`", key.fqn())
                                };
                                format!(
                                    "`{}` reads `{}` ({}){via} {}",
                                    r.func.qualname,
                                    t.text,
                                    read.kind.as_str(),
                                    rule.allowance.where_text(
                                        "read owners",
                                        "forbidden readers",
                                        rule.name
                                    )
                                )
                            },
                            || r.chain.clone(),
                        );
                    }
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
    /// Parameters of `func` the accessed object may be.
    obj: u64,
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
pub(crate) fn delegated_reports(
    cx: &CheckContext<'_>,
    sites: &SitesByCaller<'_>,
    allowance: &Allowance,
    family: &FxHashSet<String>,
    func: &FuncKey,
    obj: u64,
    span: Span,
) -> Vec<DelegatedReport> {
    if allowance.allows(func) {
        return Vec::new();
    }
    let in_scope = allowance.in_scope(func);
    if !delegates(cx, sites, func) {
        if !in_scope
            || initializes_itself(cx, func, family, obj)
            || runs_inside_allowed(cx, sites, allowance, family, func, obj)
        {
            return Vec::new();
        }
        return vec![DelegatedReport {
            func: func.clone(),
            span,
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
        obj,
        in_scope,
        down: None,
    }];
    let mut visited: FxHashSet<(FuncKey, Facts, u64, bool)> = FxHashSet::default();
    visited.insert((func.clone(), facts, obj, in_scope));
    let mut out = Vec::new();
    let mut i = 0;
    while i < states.len() {
        let (callee, facts, obj, below_in_scope) = (
            states[i].func.clone(),
            states[i].facts,
            states[i].obj,
            states[i].in_scope,
        );
        for c in callers_of(cx, sites, &callee, facts) {
            let caller_obj = c.bc.caller_params_of(obj);
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
                let state = (c.caller.clone(), Facts::Result, caller_obj, below_in_scope);
                if visited.insert(state) {
                    states.push(WalkState {
                        func: c.caller.clone(),
                        facts: Facts::Result,
                        obj: caller_obj,
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
            if delegates(cx, sites, c.caller) {
                if visited.insert((c.caller.clone(), c.into, caller_obj, in_scope)) {
                    states.push(WalkState {
                        func: c.caller.clone(),
                        facts: c.into,
                        obj: caller_obj,
                        in_scope,
                        down: Some((i, hops)),
                    });
                }
            } else if in_scope
                && !initializes_itself(cx, c.caller, family, caller_obj)
                && !runs_inside_allowed(cx, sites, allowance, family, c.caller, caller_obj)
            {
                let mut chain = hops;
                let mut j = i;
                while let Some((below, h)) = &states[j].down {
                    chain.extend(h.iter().cloned());
                    j = *below;
                }
                out.push(DelegatedReport {
                    func: c.caller.clone(),
                    span: c.site.span,
                    chain,
                });
            }
        }
        i += 1;
    }
    out
}

// ==================================================================================== MUT204

mod call_owners;

pub(crate) use call_owners::{
    CallTargetPattern, called_targets, group_texts, mut204, parse_call_targets, site_callee,
};
