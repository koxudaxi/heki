//! `MUT101`'s exact claim (docs/precision.md): whether a write that leaves a `@cached_property`
//! stale establishes every premise, decided from the index and the declarations alone, so that
//! the language server's one-file check agrees with `heki check`.
//!
//! - **Cached property**: every class the instance may be gets the property from a
//!   `functools.cached_property` getter, the first definition along its MRO.
//! - **Reads on every path**: that getter reads the written attribute on every path on which it
//!   returns normally (`KeptFacts::must_read_self_attrs`).
//! - **Plain write**: the write rebinds or deletes the attribute with a plain store, or changes
//!   in place the builtin container it holds through a method no project subclass replaces.
//! - **Kept to the return**: no point between the write and the method's normal return may
//!   remove the kept value ([`KeptAfterWrites`]).

use super::{Method, invalidates, stale_reads};
use heki_ast::Span;
use heki_effect::{
    AttrKeyPath, CacheEvent, CacheFlow, FlowBlock, FuncKey, ProjectDecls, ProjectIndex,
    SelfAttrWrite,
};
use heki_ir::BlockId;
use heki_ir::walk::{Answer, Budget, Facts, Sparse, fixpoint};
use rustc_hash::FxHashMap;

/// The project callees of a method's call sites, by the call's position in its source.
pub(super) type CallsBySpan<'i> = FxHashMap<Span, Vec<&'i FuncKey>>;

/// [`CallsBySpan`] of `writer`.
pub(super) fn calls_by_span<'i>(index: &'i ProjectIndex, writer: &Method<'_>) -> CallsBySpan<'i> {
    let mut out: CallsBySpan<'i> = FxHashMap::default();
    let sites = index
        .files
        .get(writer.path)
        .into_iter()
        .flat_map(|f| &f.call_sites);
    for site in sites.filter(|s| s.caller == writer.key) {
        let callees = out.entry(site.span).or_default();
        callees.extend(site.project_callees().iter().map(|bc| &bc.callee));
    }
    out
}

/// Whether the write `w` to `self.<attr>`, which the `@cached_property` named `prop` reads,
/// establishes every premise of `MUT101`'s exact claim when `writer` runs it on any instance
/// that runs it; `kept` is the **Kept to the return** fact of its definition.
pub(super) fn established(
    index: &ProjectIndex,
    decls: &ProjectDecls,
    writer: &FuncKey,
    prop: &str,
    attr: &str,
    w: &SelfAttrWrite,
    kept: &KeptAfterWrites,
) -> bool {
    let classes = stale_reads::classes_running(index, decls, writer, prop);
    let read = AttrKeyPath {
        attr: attr.to_owned(),
        const_key: w.key().map(str::to_owned),
    };
    let holders: Vec<&String> = classes
        .iter()
        .filter(|class| may_hold(decls, class, prop))
        .collect();
    !holders.is_empty()
        && kept.kept_to_return(w)
        && !setup_step(index, decls, writer, prop, &holders)
        && holders.iter().all(|class| {
            cached_getter_reads(index, decls, class, prop, &read)
                && plain_write(decls, class, attr, w)
        })
}

/// Whether the writer is a setup step, of which the claim holds only vacuously as a rule: the
/// census found such writes benign. Its reports stay heuristic (**After setup** is open):
///
/// - a finalizer (`__del__`): no read of the instance follows it;
/// - a step of two-phase setup: a method the `__init__` of a holder class calls on the instance,
///   or a property setter it stores through, whose later calls precede the property's first
///   computation in practice, which no fact establishes;
/// - lazy setup through a second cached property: the writer is a cached-property getter that
///   no function of the project invalidates, so it runs once per instance, and the property's
///   getter reads it on every path, so that one run comes when the property is first computed.
fn setup_step(
    index: &ProjectIndex,
    decls: &ProjectDecls,
    writer: &FuncKey,
    prop: &str,
    holders: &[&String],
) -> bool {
    let name = writer
        .qualname
        .rsplit('.')
        .next()
        .unwrap_or(&writer.qualname);
    if name == "__del__" {
        return true;
    }
    // An `__init__` along a holder's MRO (a subclass's calls its base's through `super()`).
    let called_by_init = holders.iter().any(|class| {
        std::iter::once((*class).clone())
            .chain(decls.ancestors_of(class))
            .flat_map(|c| decls.methods_of(&c))
            .filter(|key| key.qualname.ends_with(".__init__"))
            .any(|init| {
                let stores_through = index
                    .summary(&init)
                    .is_some_and(|s| s.mutates_self_attrs.iter().any(|w| w.attr == name));
                stores_through
                    || index.file_of(&init).is_some_and(|path| {
                        index.files[path].call_sites.iter().any(|site| {
                            site.caller == init
                                && site.recv_is_self
                                && site.project_callees().iter().any(|bc| bc.callee == *writer)
                        })
                    })
            })
    });
    if called_by_init {
        return true;
    }
    let lazy_getter = index
        .summary(writer)
        .is_some_and(|s| s.is_cached_property && s.listed_cache().is_some());
    lazy_getter
        && !invalidated_anywhere(index, name)
        && holders.iter().all(|class| {
            decls
                .function_taken(class, prop)
                .and_then(|getter| index.summary(&getter))
                .is_some_and(|s| s.must_read_self_attrs().iter().any(|r| r.attr == name))
        })
}

/// Whether a function of the project invalidates the cached property `name` by its name, on
/// its own instance or on another object.
fn invalidated_anywhere(index: &ProjectIndex, name: &str) -> bool {
    index
        .files
        .values()
        .flat_map(|f| f.local_functions.values())
        .any(|s| invalidates(s, name) || matches!(s.removes_kept(), heki_effect::KeptRemoval::Named(names) if names.contains(name)))
}

/// Whether an instance of `class` may hold a value under the property's name `prop`: a class
/// along its MRO names it (in its body, or as an attribute its methods set), or a base heki does
/// not know may. An instance of any other class holds no value of the property, so the claim
/// says nothing of it; each project subclass that runs the writer is a class of its own.
fn may_hold(decls: &ProjectDecls, class: &str, prop: &str) -> bool {
    let read = decls.attr_read(class, prop);
    read.declared || !read.bases_known
}
/// **Cached property** and **Reads on every path** for instances of `class`: the first
/// definition of `prop` along its MRO is a synchronous getter of a project class that no base
/// heki does not know comes before, decorated by a cached-property decorator the catalog lists
/// as keeping the value in the instance `__dict__` (`KeptFacts::listed_cache`, R3; with one
/// that keeps it in a slot on an instance without a `__dict__`, no project class along the MRO
/// defines `__slots__`), the class has no metaclass heki does not know, and the getter reads
/// `read` on every path on which it returns normally: the attribute with the written key, or as
/// a whole.
fn cached_getter_reads(
    index: &ProjectIndex,
    decls: &ProjectDecls,
    class: &str,
    prop: &str,
    read: &AttrKeyPath,
) -> bool {
    if decls.has_custom_metaclass(class) {
        return false;
    }
    let Some(getter) = decls.function_taken(class, prop) else {
        return false;
    };
    let Some(s) = index.summary(&getter) else {
        return false;
    };
    let Some(listed) = s.listed_cache().filter(|_| s.is_cached_property) else {
        return false;
    };
    if s.is_async || s.is_generator || (listed.in_slot && decls.defines_slots(class)) {
        return false;
    }
    s.must_read_self_attrs().iter().any(|r| {
        r.attr == read.attr
            && (r.const_key.is_none() || read.const_key.is_none() || r.const_key == read.const_key)
    })
}

/// **Plain write** on an instance of `class`: a rebinding or deletion stores into the instance
/// itself (no property, descriptor, `__setattr__`, `__delattr__`, or metaclass heki does not
/// know along the MRO or in a subclass, and every base a project class or a plain one); an
/// in-place change reads the attribute plainly and changes a builtin container it holds by its
/// type through a method or item store that no project subclass of the container replaces.
fn plain_write(decls: &ProjectDecls, class: &str, attr: &str, w: &SelfAttrWrite) -> bool {
    let read = decls.attr_read(class, attr);
    if !read.bases_known || read.custom_metaclass || !read.code_binders.is_empty() {
        return false;
    }
    if w.rebinds {
        return !decls.attr_write_runs_code(class, attr)
            && decls
                .value_classes(class)
                .iter()
                .all(|c| !decls.attr_write_runs_code(c, attr));
    }
    w.change().is_some_and(|change| {
        decls.plain_attr(class, attr)
            && decls.method_not_overridden(change.kind.class(), &change.method, change.by_name)
    })
}

/// **Kept to the return** in one definition of a method, for the value kept under one name:
/// the points that may remove or replace it, and per block whether some path from its start to
/// a normal return passes one. Exception edges into handlers count (any instruction may raise
/// into its block's handler); a path that leaves the method by an exception makes no claim.
pub(super) struct KeptAfterWrites<'f> {
    flow: &'f CacheFlow,
    /// Per block, the instruction indices of the points that may remove the value.
    removals: Vec<Vec<u32>>,
    /// Per block, whether some path from its start to a normal return passes such a point
    /// (`None`: past the walk's bound, so nothing is established).
    may_remove: Option<Vec<bool>>,
    /// Per block, whether some path from its start reaches a normal return.
    returns: Option<Vec<bool>>,
}

impl<'f> KeptAfterWrites<'f> {
    /// The fact for the value kept under `prop` in the instances of the method whose definition
    /// `flow` is and whose call sites are `calls`. The points are `del self.<prop>`, its removal
    /// from `self.__dict__` or a clearing of it, a store of `self.<prop>`, a point that may
    /// remove it in any object ([`CacheEvent::MayRemove`], [`CacheEvent::MayRemoveAny`]), and a
    /// project call any of whose targets removes it, invalidates it, or stores it, in its own
    /// instance or in an object it is handed.
    pub(super) fn of(
        index: &ProjectIndex,
        calls: &CallsBySpan<'_>,
        flow: &'f CacheFlow,
        prop: &str,
    ) -> Self {
        let call_removes = |span: Span| {
            calls.get(&span).into_iter().flatten().any(|callee| {
                index.summary(callee).is_none_or(|c| {
                    c.removes_kept().removes(prop)
                        || invalidates(c, prop)
                        || c.mutates_self_attrs.iter().any(|w| w.attr == prop)
                        || c.mutated_param_paths
                            .values()
                            .any(|paths| paths.contains(prop))
                })
            })
        };
        let mut removals: Vec<Vec<u32>> = vec![Vec::new(); flow.blocks.len()];
        for (b, block) in flow.blocks.iter().enumerate() {
            for &(i, event) in &block.events {
                let removes = match event {
                    CacheEvent::DeleteAttr(name)
                    | CacheEvent::RemoveEntry(name)
                    | CacheEvent::MayRemove(name) => flow.name(name) == prop,
                    CacheEvent::ClearDict | CacheEvent::MayRemoveAny => true,
                    CacheEvent::SelfCall(span) | CacheEvent::Calls(span) => call_removes(span),
                    CacheEvent::LoadAttr(_) => false,
                };
                if removes {
                    removals[b].push(i);
                }
            }
        }
        for w in flow.writes_of(prop) {
            removals[w.block.0 as usize].push(w.index);
        }
        let preds = continued_from(flow);
        let returns = backward(flow, &preds, |b| flow.blocks[b].returns);
        let may_remove = returns
            .as_ref()
            .and_then(|returns| backward(flow, &preds, |b| !removals[b].is_empty() && returns[b]));
        Self {
            flow,
            removals,
            may_remove,
            returns,
        }
    }

    /// Whether no path from just after the write `w` to a normal return of the method passes a
    /// point that may remove the value.
    pub(super) fn kept_to_return(&self, w: &SelfAttrWrite) -> bool {
        let (Some(may_remove), Some(returns)) = (&self.may_remove, &self.returns) else {
            return false;
        };
        let b = w.block.0 as usize;
        let removed_here = returns[b] && self.removals[b].iter().any(|i| *i > w.index);
        !removed_here && !onward(&self.flow.blocks[b]).any(|s| may_remove[s.0 as usize])
    }
}

/// The blocks a path continues to from `block`: its normal successors and its handler.
fn onward(block: &FlowBlock) -> impl Iterator<Item = BlockId> + '_ {
    block.succs.iter().copied().chain(block.exc_target)
}

/// Per block, the blocks a path continues from to it ([`onward`]).
fn continued_from(flow: &CacheFlow) -> Vec<Vec<usize>> {
    let mut preds = vec![Vec::new(); flow.blocks.len()];
    for (b, block) in flow.blocks.iter().enumerate() {
        for s in onward(block) {
            if let Some(p) = preds.get_mut(s.0 as usize) {
                p.push(b);
            }
        }
    }
    preds
}

/// Per block, whether some path from its start reaches a block where `holds` (over normal and
/// exception edges): a backward reach to a fixpoint, each block visited once, then once more
/// for each of the blocks it continues to whose fact turns true.
fn backward(
    flow: &CacheFlow,
    preds: &[Vec<usize>],
    holds: impl Fn(usize) -> bool,
) -> Option<Vec<bool>> {
    let readers = |b: usize, out: &mut Vec<usize>| out.extend(&preds[b]);
    let out_degree = flow
        .blocks
        .iter()
        .map(|b| onward(b).count())
        .max()
        .unwrap_or(0);
    let budget = Budget::unmetered("heki_rules::mut_rules::kept_values");
    let facts = fixpoint(
        &budget,
        Sparse {
            nodes: flow.blocks.len(),
            readers: &readers,
            max_visits: u32::try_from(out_degree + 1).unwrap_or(u32::MAX),
        },
        |b, facts: &Facts<bool>| {
            let on =
                holds(b) || onward(&flow.blocks[b]).any(|s| facts.get(s.0 as usize) == Some(&true));
            on.then_some(true)
        },
    );
    match facts {
        Answer::Known(facts) => Some(
            facts
                .into_vec()
                .into_iter()
                .map(|f| f == Some(true))
                .collect(),
        ),
        Answer::Open => None,
    }
}
