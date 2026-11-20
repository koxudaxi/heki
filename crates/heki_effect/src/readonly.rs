//! `MUT202` return flow (docs/ANALYSIS_AND_KB_SPEC.md section 2.3): which part of a value is
//! shared with the other callers of a read-only producer, how a value derived from it keeps that
//! sharing, and which call results a function may return.
//!
//! The derivations below are the in-function transfer of `MUT202` (`heki_rules` applies them
//! forward, from the producer's call). The extractor applies them backward, from the values a
//! function returns: a function that returns a producer's value, or a part of it, becomes a
//! producer itself. Its summary carries a [`ProtectedReturn`], propagated bottom-up over the call
//! graph, so a wrapper needs no configuration of its own.

use crate::CallHop;
use crate::resolver::Val;
use heki_ir::{Inst, InstKind, IrFunction, PhiNode, Terminator, VarId};
use heki_kb::EffectiveEcosystemKb;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

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
/// accessors) or from its first argument (`method` `None`), for the resolved external `symbol`
/// (empty for any other callee): copy sanitizers of the catalog and copy methods copy, deeply for
/// `deepcopy`, `__deepcopy__`, and `model_copy(deep=True)` (`deep_kwarg`).
pub fn call_derivation(
    method: Option<&str>,
    symbol: &str,
    kb: &EffectiveEcosystemKb,
    deep_kwarg: bool,
) -> Option<Derivation> {
    let sanitizer = kb.lookup(symbol).is_some_and(|e| e.copy_sanitizer)
        || method.is_some_and(|m| COPY_METHODS.contains(&m));
    if sanitizer {
        let deep_copy = symbol.ends_with("deepcopy")
            || method == Some("__deepcopy__")
            || (method == Some("model_copy") || symbol.ends_with(".model_copy")) && deep_kwarg;
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
    pub chain: Vec<CallHop>,
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
        chain: Vec<CallHop>,
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

/// Values a slice of which is a new value holding the same items: the built-in sequences.
const COPYING_SLICES: &[&str] = &[
    "builtins.list",
    "builtins.tuple",
    "builtins.str",
    "builtins.bytes",
    "builtins.bytearray",
];

/// The step from the subscripted value to the result of the `SubscriptLoad` `inst`: an item of
/// target unpacking keeps the value ([`crate::consumption::is_unpacking`]); a slice is a
/// shallow copy (`xs[:]`) unless the value is known to be of another class than the built-in
/// sequences (a library's or the project's slice may be a view); any other subscript reads an
/// element.
pub fn subscript_step(f: &IrFunction, inst: &Inst, subscripted: Option<&Val>) -> ReturnStep {
    let InstKind::SubscriptLoad {
        index, is_slice, ..
    } = &inst.kind
    else {
        return ReturnStep::ELEMENT;
    };
    if crate::consumption::is_unpacking(f, *index, inst.span) {
        return ReturnStep::TARGET_ITEM;
    }
    let copies = match subscripted {
        None => true,
        Some(Val::Inst(class)) => COPYING_SLICES.contains(&class.as_str()),
        Some(_) => false,
    };
    if *is_slice && copies {
        ReturnStep::SHALLOW_COPY
    } else {
        ReturnStep::ELEMENT
    }
}

/// The values a function's result may derive from without a deep copy: for each such variable,
/// the step from it to the returned value, joined over the paths that return it. The walk goes
/// back from every `return v` through copies, phis, attribute and item loads, iterations, and
/// the derivations `derive` reports for an instruction (a call's receiver and first argument);
/// it does not pass a deep copy, and a generator's or coroutine's caller gets a generator or a
/// coroutine, never the returned value, so neither is walked.
pub(crate) fn returned_values(
    f: &IrFunction,
    defs: &FxHashMap<VarId, &Inst>,
    types: &FxHashMap<VarId, Val>,
    derive: impl Fn(&InstKind) -> Vec<(VarId, Derivation)>,
) -> FxHashMap<VarId, ReturnStep> {
    let mut steps: FxHashMap<VarId, ReturnStep> = FxHashMap::default();
    if !matches!(f.kind, heki_ir::FunctionKind::Sync) {
        return steps;
    }
    let mut work: Vec<(VarId, ReturnStep)> = f
        .blocks
        .iter()
        .filter_map(|b| match b.terminator {
            Terminator::Return(Some(v)) => Some((v, ReturnStep::ITSELF)),
            _ => None,
        })
        .collect();
    let mut phis: Option<FxHashMap<VarId, &PhiNode>> = None;
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
                    .flat_map(|b| &b.phis)
                    .map(|p| (p.dst, p))
                    .collect()
            });
            if let Some(phi) = phis.get(&var) {
                work.extend(
                    phi.incoming
                        .iter()
                        .filter(|(_, v)| *v != var)
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
                let derived = subscript_step(f, inst, types.get(obj));
                work.push((*obj, step.after(derived)));
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
        let hop = |callee: &str| CallHop {
            caller: crate::FuncKey {
                module: "m".into(),
                qualname: "f".into(),
            },
            callee: callee.into(),
            span: heki_ast::Span { start: 0, end: 1 },
            path: "m.py".into(),
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
