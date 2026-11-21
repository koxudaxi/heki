//! `MUT202` return flow (docs/ANALYSIS_AND_KB_SPEC.md section 2.3): which part of a value is
//! shared with the other callers of a read-only producer, how a value derived from it keeps that
//! sharing, and which call results a function may return.
//!
//! The derivations below are the in-function transfer of `MUT202` (`heki_rules` applies them
//! forward, from the producer's call). The extractor applies them backward, from the values a
//! function returns: a function that returns a producer's value, or a part of it, becomes a
//! producer itself. Its summary carries a [`ProtectedReturn`], propagated bottom-up over the call
//! graph, so a wrapper needs no configuration of its own.

use crate::{Callee, SharedChain};
use heki_ir::walk::FnShape;
use heki_ir::{DictEntry, Inst, InstKind, IrFunction, PhiNode, Terminator, VarId};
use heki_kb::{CopyDepth, EffectiveEcosystemKb};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

/// Builtin sequences whose slices are new objects holding the same elements (`xs[:]`,
/// `xs[1:]`): a slice of one is a shallow copy. Other objects' slices may be views (`numpy`).
pub const SLICE_COPYING_TYPES: &[&str] = &[
    "builtins.list",
    "builtins.bytearray",
    "builtins.tuple",
    "builtins.str",
    "builtins.bytes",
];

/// The step from `obj` to what the `SubscriptLoad` `inst` reads from it: an item of target
/// unpacking, a shallow copy for a slice of a builtin sequence (`obj_type`, its inferred class),
/// or an element.
pub fn subscript_step(shape: &FnShape<'_>, inst: &Inst, obj_type: Option<&str>) -> ReturnStep {
    match &inst.kind {
        InstKind::SubscriptLoad { index, .. }
            if crate::consumption::is_unpacking(shape, *index, inst.span) =>
        {
            ReturnStep::TARGET_ITEM
        }
        // A slice copies unless the value's class is known to share its storage (a NumPy
        // view): an unannotated value is far more often a list, and a report needs evidence.
        InstKind::SubscriptLoad { is_slice: true, .. }
            if obj_type.is_none_or(|t| SLICE_COPYING_TYPES.contains(&t)) =>
        {
            ReturnStep::SHALLOW_COPY
        }
        _ => ReturnStep::ELEMENT,
    }
}

/// The values a display that only unpacks others copies into the container it makes: the
/// one `**m` entry of a dict display, or the starred elements of a list, tuple, or set display
/// (`{**m}`, `[*xs, *ys]`). The new container is a shallow copy of each
/// ([`ReturnStep::SHALLOW_COPY`]). A dict display with more entries (`{**m, "k": []}`,
/// `{**m, **fresh}`) is not followed: which of its items are the copied ones depends on keys
/// the shared value does not carry.
pub fn display_copies(f: &IrFunction, kind: &InstKind) -> Vec<VarId> {
    match kind {
        InstKind::DictLiteral { entries, .. } => match entries.as_slice() {
            [DictEntry::Unpack { dict }] => vec![*dict],
            _ => Vec::new(),
        },
        InstKind::AllocBuiltin { elems, .. } if elems.iter().all(|v| f.is_unpacked(*v)) => {
            elems.clone()
        }
        _ => Vec::new(),
    }
}

/// Methods returning (views of) a container's elements.
pub const ELEMENT_ACCESSORS: &[&str] = &["get", "__getitem__", "values", "items"];
/// Methods returning a copy of their receiver.
pub const COPY_METHODS: &[&str] = &["copy", "model_copy", "__copy__", "__deepcopy__"];

/// Which parts of a value the other callers of a read-only producer share.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Shared {
    /// The object itself: changing its attributes or items is a violation.
    pub object: bool,
    /// The values below it (attributes, items, elements), at any depth.
    pub elements: bool,
}

impl Shared {
    /// The result of a producer: the object, and with `deep` the values below it.
    pub fn produced(deep: bool) -> Self {
        Self {
            object: true,
            elements: deep,
        }
    }

    /// Whether any part is shared.
    pub fn any(self) -> bool {
        self.object || self.elements
    }

    /// A value that may be either: what either shares.
    pub fn join(self, other: Self) -> Self {
        Self {
            object: self.object || other.object,
            elements: self.elements || other.elements,
        }
    }
}

/// How a value derived from another keeps that value's sharing: the derived object is shared
/// when the source object is (`from_object`) or when the values below the source are
/// (`from_elements`); the values below stay shared as they were.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ReturnStep {
    pub from_object: bool,
    pub from_elements: bool,
}

impl ReturnStep {
    /// The value itself: a copy of the reference, an iterator over it.
    pub const ITSELF: Self = Self {
        from_object: true,
        from_elements: false,
    };
    /// A value below it: an attribute, an item, an element of an iteration.
    pub const ELEMENT: Self = Self {
        from_object: false,
        from_elements: true,
    };
    /// An item of target unpacking (`a, b = value`, `consumption::is_unpacking`): shared like
    /// the value when the value is, else like an element.
    pub const TARGET_ITEM: Self = Self {
        from_object: true,
        from_elements: true,
    };
    /// A shallow copy: a new object that still holds the shared values below it.
    pub const SHALLOW_COPY: Self = Self {
        from_object: false,
        from_elements: false,
    };

    /// The sharing of the derived value.
    pub fn apply(self, source: Shared) -> Shared {
        Shared {
            object: (self.from_object && source.object) || (self.from_elements && source.elements),
            elements: source.elements,
        }
    }

    /// `self` applied to the result of `first`.
    pub fn after(self, first: Self) -> Self {
        Self {
            from_object: self.from_object && first.from_object,
            from_elements: (self.from_object && first.from_elements) || self.from_elements,
        }
    }

    /// A value derived by either step (paths joining at a phi).
    pub fn join(self, other: Self) -> Self {
        Self {
            from_object: self.from_object || other.from_object,
            from_elements: self.from_elements || other.from_elements,
        }
    }
}

/// How a call's result derives from the operand it reads for `MUT202`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Derivation {
    /// A deep copy: nothing of the operand is shared.
    DeepCopy,
    /// A shallow copy ([`ReturnStep::SHALLOW_COPY`]) or an element view
    /// ([`ReturnStep::ELEMENT`]).
    Step(ReturnStep),
}

/// The derivation of a call's result from its receiver (`method` set: copy methods, element
/// accessors) or from its first argument (`method` `None`), for the resolved `callee`: copy
/// sanitizers of the catalog copy as deeply as their entry says, and copy methods of a receiver
/// that is no project class copy, deeply for `__deepcopy__` and `model_copy(deep=True)`
/// (`deep_kwarg`). A copy method a project class defines copies as deeply as its own body does,
/// which its summary says (`EffectSummary::returns_protected`), not its name. A `dict` built
/// from a mapping and other arguments (`own_items`: `dict(m, k=v)`) holds their values under
/// their keys, so it is not followed, as a dict display with entries of its own is not
/// ([`display_copies`]).
pub fn call_derivation(
    method: Option<&str>,
    callee: &Callee,
    kb: &EffectiveEcosystemKb,
    deep_kwarg: bool,
    own_items: bool,
) -> Option<Derivation> {
    let symbol = match callee {
        Callee::External(s) => s.as_str(),
        _ => "",
    };
    if method.is_none() && own_items && symbol == "builtins.dict" {
        return None;
    }
    let catalogued = kb.lookup(symbol).and_then(|e| e.copy_sanitizer);
    let copies_by_name =
        method.is_some_and(|m| COPY_METHODS.contains(&m)) && !matches!(callee, Callee::Project(..));
    if catalogued.is_some() || copies_by_name {
        let deep_copy = catalogued == Some(CopyDepth::Deep)
            || method == Some("__deepcopy__")
            || method == Some("model_copy") && deep_kwarg;
        return Some(if deep_copy {
            Derivation::DeepCopy
        } else {
            Derivation::Step(ReturnStep::SHALLOW_COPY)
        });
    }
    method
        .filter(|m| ELEMENT_ACCESSORS.contains(m))
        .map(|_| Derivation::Step(ReturnStep::ELEMENT))
}

/// Whether a copy call asks for a deep copy (`model_copy(deep=True)`, `is_true` deciding a
/// keyword's value). Pydantic assigns the values of `update` to the copy by reference after
/// copying, so a copy with `update` (or with a `**` mapping that may hold it) is a shallow one.
pub fn deep_copy_requested(
    kwargs: &[(Option<String>, VarId)],
    is_true: impl Fn(VarId) -> bool,
) -> bool {
    let deep = kwargs
        .iter()
        .any(|(k, v)| k.as_deref() == Some("deep") && is_true(*v));
    deep && kwargs
        .iter()
        .all(|(k, _)| k.as_deref().is_some_and(|k| k != "update"))
}

/// A function that may return (on some path) a value shared with a read-only producer's other
/// callers: a declared `readonly-returns` producer's or an `@lru_cache` function's result, or a
/// part of one, directly or through other such functions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectedReturn {
    /// The declared producer or `@lru_cache` function the value comes from.
    pub producer: String,
    /// What the returned value shares, joined over the paths that return such a value.
    pub shared: Shared,
    /// Calls from this function down to the one that returns the producer's value.
    pub chain: SharedChain,
}

impl ProtectedReturn {
    /// Join the value another path returns: what either shares, named by the shorter chain.
    pub fn join(&mut self, other: Self) {
        let shared = self.shared.join(other.shared);
        if other.chain.len() < self.chain.len() {
            *self = other;
        }
        self.shared = shared;
    }

    /// Record in `slot` that the function returns, through `step`, a value of `producer` that
    /// shares `produced`, reached through `chain` (nothing when the step shares nothing of it).
    pub fn add(
        slot: &mut Option<Self>,
        producer: String,
        produced: Shared,
        step: ReturnStep,
        chain: SharedChain,
    ) {
        let shared = step.apply(produced);
        if !shared.any() {
            return;
        }
        let found = Self {
            producer,
            shared,
            chain,
        };
        match slot {
            Some(r) => r.join(found),
            None => *slot = Some(found),
        }
    }
}

/// The values a function's result may derive from without a deep copy: for each such variable,
/// the step from it to the returned value, joined over the paths that return it. The walk goes
/// back from every `return v` a run can reach (`live`), through copies, phis (along the edges a
/// run can take), attribute and item loads, iterations, and the derivations `derive` reports for
/// an instruction (a call's receiver and first argument); it does not pass a deep copy, and a
/// generator's or coroutine's caller gets a generator or a coroutine, never the returned value,
/// so neither is walked.
pub(crate) fn returned_values<'t>(
    shape: &FnShape<'_>,
    defs: &FxHashMap<VarId, &Inst>,
    live: &crate::ownership::Live,
    derive: impl Fn(&InstKind) -> Vec<(VarId, Derivation)>,
    type_of: impl Fn(VarId) -> Option<&'t str>,
) -> FxHashMap<VarId, ReturnStep> {
    let f = shape.f;
    let mut steps: FxHashMap<VarId, ReturnStep> = FxHashMap::default();
    if !matches!(f.kind, heki_ir::FunctionKind::Sync) {
        return steps;
    }
    let mut work: Vec<(VarId, ReturnStep)> = f
        .blocks
        .iter()
        .filter(|b| live.block(b.id))
        .filter_map(|b| match b.terminator {
            Terminator::Return(Some(v)) => Some((v, ReturnStep::ITSELF)),
            _ => None,
        })
        .collect();
    let mut phis: Option<FxHashMap<VarId, (heki_ir::BlockId, &PhiNode)>> = None;
    while let Some((var, step)) = work.pop() {
        let step = match steps.get(&var) {
            Some(old) if old.join(step) == *old => continue,
            Some(old) => old.join(step),
            None => step,
        };
        steps.insert(var, step);
        let Some(inst) = defs.get(&var) else {
            let phis = phis.get_or_insert_with(|| {
                f.blocks
                    .iter()
                    .flat_map(|b| b.phis.iter().map(move |p| (p.dst, (b.id, p))))
                    .collect()
            });
            if let Some((block, phi)) = phis.get(&var) {
                work.extend(
                    phi.incoming
                        .iter()
                        .filter(|(from, v)| *v != var && live.edge(*from, *block))
                        .map(|(_, v)| (*v, step)),
                );
            }
            continue;
        };
        match &inst.kind {
            InstKind::Copy { src, .. } => work.push((*src, step)),
            InstKind::MakeIter { iterable, .. } => work.push((*iterable, step)),
            InstKind::IterNext { iter, .. } => work.push((*iter, step.after(ReturnStep::ELEMENT))),
            InstKind::AttrLoad { obj, .. } => work.push((*obj, step.after(ReturnStep::ELEMENT))),
            InstKind::SubscriptLoad { obj, .. } => {
                let derived = subscript_step(shape, inst, type_of(*obj));
                work.push((*obj, step.after(derived)));
            }
            InstKind::DictLiteral { .. } | InstKind::AllocBuiltin { .. } => {
                for copied in display_copies(f, &inst.kind) {
                    work.push((copied, step.after(ReturnStep::SHALLOW_COPY)));
                }
            }
            // A call's result derives from its receiver or first argument when it copies or
            // views it; `derive` reports nothing for any other instruction.
            kind => {
                for (operand, derivation) in derive(kind) {
                    if let Derivation::Step(s) = derivation {
                        work.push((operand, step.after(s)));
                    }
                }
            }
        }
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_STEPS: [ReturnStep; 4] = [
        ReturnStep::ITSELF,
        ReturnStep::ELEMENT,
        ReturnStep::TARGET_ITEM,
        ReturnStep::SHALLOW_COPY,
    ];

    fn shared(object: bool, elements: bool) -> Shared {
        Shared { object, elements }
    }

    /// The states a forward `MUT202` value can be in: a shallow producer's result, a deep one's,
    /// and a shallow copy of a deep one (a shared object never has unshared parts below a
    /// shallow copy).
    const STATES: [(bool, bool); 3] = [(true, false), (true, true), (false, true)];

    #[test]
    fn steps_match_the_forward_transfer() {
        for (object, elements) in STATES {
            let s = shared(object, elements);
            // An attribute or item is shared when the values below are.
            assert_eq!(ReturnStep::ELEMENT.apply(s), shared(elements, elements));
            // Unpacking keeps a shared object, else reads an element.
            let unpacked = if object { s } else { shared(true, true) };
            assert_eq!(ReturnStep::TARGET_ITEM.apply(s), unpacked);
            // A shallow copy is new; what it holds stays shared.
            assert_eq!(ReturnStep::SHALLOW_COPY.apply(s), shared(false, elements));
            assert_eq!(ReturnStep::ITSELF.apply(s), s);
        }
        assert!(
            !ReturnStep::SHALLOW_COPY
                .apply(Shared::produced(false))
                .any()
        );
    }

    #[test]
    fn composition_and_join_agree_with_applying_in_turn() {
        for first in ALL_STEPS {
            for second in ALL_STEPS {
                for (object, elements) in STATES {
                    let s = shared(object, elements);
                    assert_eq!(
                        second.after(first).apply(s),
                        second.apply(first.apply(s)),
                        "{first:?} then {second:?} on {s:?}"
                    );
                    assert_eq!(
                        first.join(second).apply(s),
                        first.apply(s).join(second.apply(s)),
                        "{first:?} or {second:?} on {s:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_join_keeps_the_shorter_chain_and_both_sharings() {
        let hop = |callee: &str| {
            std::sync::Arc::new(crate::CallHop {
                caller: crate::FuncKey {
                    module: "m".into(),
                    qualname: "f".into(),
                },
                callee: callee.into(),
                span: heki_ast::Span { start: 0, end: 1 },
                path: "m.py".into(),
            })
        };
        let mut long = ProtectedReturn {
            producer: "m.deep".into(),
            shared: shared(false, true),
            chain: vec![hop("m.a"), hop("m.deep")],
        };
        long.join(ProtectedReturn {
            producer: "m.flat".into(),
            shared: shared(true, false),
            chain: vec![hop("m.flat")],
        });
        assert_eq!(long.producer, "m.flat");
        assert_eq!(long.shared, shared(true, true));
        assert_eq!(long.chain.len(), 1);
    }
}
