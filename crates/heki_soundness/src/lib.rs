//! Soundness / Alias lane (`Family 3: OPT` — zero miscompilations).
//!
//! Architecture Invariant (ADR-004, ADR-011):
//! - NEVER depends on `heki_effect` or PEP 484 type annotations.
//! - Tracks `Unique` with up to `MAX_JOIN_ALLOC_IDS` (4) joined allocation origins,
//!   direct aliases, and derived references (`MakeIter`, `MakeGenExpr`, Layer 1 `retained_in_return`)
//!   with transitive escape.
//! - Separates Safety (`ProofObligation` / `SafetyVerdict`) orthogonally from
//!   Profitability (`Unconditional` vs `SizeDependent`).
//!
//! Pipeline (docs/DETAILED_SPEC_M3_M7.md section 4): [`interpreter`] runs the RPO worklist
//! abstract interpretation, [`liveness`] the backward liveness (with `exc_target` edges),
//! [`obligations`] evaluates the three-valued Proof Obligations, and [`opt_rules`] matches the
//! eight `OPT` patterns and builds their minimal byte-span [`TextEdit`]s.

mod edits;
mod interpreter;
mod liveness;
mod obligations;
mod opt_rules;

pub use interpreter::BuiltinScope;
pub use obligations::{ObligationKind, ProofObligation, SafetyVerdict};
pub use opt_rules::{
    AnalysisOptions, OptCandidate, OptRule, TextEdit, analyze_module, module_patches_builtins,
};

use heki_ast::Span;
use heki_ir::{BuiltinKind, VarId};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

pub const MAX_JOIN_ALLOC_IDS: usize = 4;

/// Allocation site (one per allocating IR instruction; a site in a loop summarizes every
/// object it creates).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AllocId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExactTypeVal {
    Exact { ty: BuiltinKind, origin: Span },
    Unknown { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EscapeWitness {
    PassedToUnknownCall(Span),
    StoredInContainerOrAttr(Span),
    DerivedRefEscapedAt(Span),
    JoinAllocOverflow,
    ParameterInput,
    /// The variable's name is captured by a nested function, lambda, or generator expression.
    CapturedByClosure,
    /// The value comes from outside the frame (global, attribute, element, unknown call result).
    ExternalValue(Span),
    /// Returned or raised: a `finally` block or handler may still run before the frame ends.
    ReturnedOrRaised(Span),
}

impl EscapeWitness {
    /// Human-readable reason, with the byte offset of the escaping operation when known.
    pub fn describe(&self) -> String {
        match self {
            Self::PassedToUnknownCall(s) => {
                format!("passed to code heki cannot see (offset {})", s.start)
            }
            Self::StoredInContainerOrAttr(s) => {
                format!(
                    "stored in a container, attribute, or global (offset {})",
                    s.start
                )
            }
            Self::DerivedRefEscapedAt(s) => {
                format!("an iterator or view of it escapes (offset {})", s.start)
            }
            Self::JoinAllocOverflow => {
                format!("it may hold objects from more than {MAX_JOIN_ALLOC_IDS} allocation sites")
            }
            Self::ParameterInput => "it is a parameter: the caller may hold a reference".into(),
            Self::CapturedByClosure => "its name is captured by a nested function".into(),
            Self::ExternalValue(s) => format!(
                "it comes from outside the function (global, attribute, element, or call result; offset {})",
                s.start
            ),
            Self::ReturnedOrRaised(s) => format!("it is returned or raised (offset {})", s.start),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UniqueState {
    Unique { alloc_ids: SmallVec<[AllocId; 2]> },
    Shared { witness: EscapeWitness },
}

/// Set of [`BuiltinKind`]s (one bit per kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct KindMask(pub u16);

impl KindMask {
    pub const EMPTY: Self = Self(0);

    fn bit(kind: BuiltinKind) -> u16 {
        1 << match kind {
            BuiltinKind::List => 0,
            BuiltinKind::Dict => 1,
            BuiltinKind::Set => 2,
            BuiltinKind::Tuple => 3,
            BuiltinKind::Str => 4,
            BuiltinKind::Bytes => 5,
            BuiltinKind::Int => 6,
            BuiltinKind::Float => 7,
            BuiltinKind::Bool => 8,
        }
    }

    pub fn of(kind: BuiltinKind) -> Self {
        Self(Self::bit(kind))
    }

    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub fn contains(self, kind: BuiltinKind) -> bool {
        self.0 & Self::bit(kind) != 0
    }

    pub fn is_subset_of(self, other: Self) -> bool {
        self.0 & !other.0 == 0
    }

    /// Kinds whose `==` agrees with `hash()` and whose values are hashable: `int`, `str`,
    /// `bytes`, `bool`, `float` (DETAILED_SPEC section 4.5, `OPT110` / S22).
    pub fn hash_consistent() -> Self {
        [
            BuiltinKind::Int,
            BuiltinKind::Str,
            BuiltinKind::Bytes,
            BuiltinKind::Bool,
            BuiltinKind::Float,
        ]
        .into_iter()
        .fold(Self::EMPTY, |m, k| m.union(Self::of(k)))
    }

    /// The single kind in the set, if there is exactly one.
    pub fn single(self) -> Option<BuiltinKind> {
        BuiltinKind::ALL.into_iter().find(|&k| self == Self::of(k))
    }
}

/// Element kinds of a tracked `list` / `tuple` / `set` allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElemKinds {
    Known(KindMask),
    Unknown,
}

impl ElemKinds {
    pub fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Known(a), Self::Known(b)) => Self::Known(a.union(b)),
            _ => Self::Unknown,
        }
    }
}

/// Abstract interpreter state per program point in `heki_soundness`.
///
/// Invariants kept by the interpreter: `alias_sets[a]` holds every variable whose `points_to`
/// contains `a` (whether `Unique` or `Shared`), `derived_refs[a]` every variable whose
/// `derives_from` contains `a`, and all member lists are sorted and free of duplicates.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FrameState {
    pub var_type: FxHashMap<VarId, ExactTypeVal>,
    pub var_unique: FxHashMap<VarId, UniqueState>,
    pub alias_sets: FxHashMap<AllocId, Vec<VarId>>,
    pub derived_refs: FxHashMap<AllocId, Vec<VarId>>,
    pub derives_from: FxHashMap<VarId, SmallVec<[AllocId; 2]>>,
    /// Allocation sites a variable may refer to. Kept when the variable becomes `Shared`, so a
    /// mutation or escape through a shared alias still reaches every object it may name.
    pub points_to: FxHashMap<VarId, SmallVec<[AllocId; 2]>>,
    /// Element kinds of each allocated `list` / `tuple` / `set` (absent: not allocated on
    /// every path reaching this point).
    pub alloc_elems: FxHashMap<AllocId, ElemKinds>,
}

/// Three-valued Proof Obligation status (ADR-003 / ADR-006).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObligationStatus {
    Established { evidence: String },
    Blocked { blocker: String },
    Disproved { counterexample: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyTier {
    Safe,
    Conditional,
    Blocked,
    Disproved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProfitabilityTier {
    /// Profitable from n = 1 and 0-trip loops (`OPT101`, `OPT102` ONLY).
    Unconditional,
    /// May regress for small inputs or 0-trip loops (`OPT103`, `OPT110`..`OPT130`).
    SizeDependent,
}

/// Join two `UniqueState` values at a CFG merge point (enforcing `MAX_JOIN_ALLOC_IDS = 4`).
pub fn join_unique(lhs: &UniqueState, rhs: &UniqueState) -> UniqueState {
    match (lhs, rhs) {
        (UniqueState::Shared { witness }, _) | (_, UniqueState::Shared { witness }) => {
            UniqueState::Shared {
                witness: witness.clone(),
            }
        }
        (UniqueState::Unique { alloc_ids: a }, UniqueState::Unique { alloc_ids: b }) => {
            let mut merged: SmallVec<[AllocId; 2]> = a.clone();
            for id in b {
                if !merged.contains(id) {
                    merged.push(*id);
                }
            }
            merged.sort_unstable();
            if merged.len() <= MAX_JOIN_ALLOC_IDS {
                UniqueState::Unique { alloc_ids: merged }
            } else {
                UniqueState::Shared {
                    witness: EscapeWitness::JoinAllocOverflow,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod invariant_tests {
    use super::*;

    #[test]
    fn join_unique_downgrades_past_four_allocs() {
        let a = UniqueState::Unique {
            alloc_ids: SmallVec::from_slice(&[AllocId(1), AllocId(2), AllocId(3)]),
        };
        let b = UniqueState::Unique {
            alloc_ids: SmallVec::from_slice(&[AllocId(4), AllocId(5)]),
        };
        assert!(matches!(
            join_unique(&a, &b),
            UniqueState::Shared {
                witness: EscapeWitness::JoinAllocOverflow
            }
        ));
    }

    #[test]
    fn architecture_invariant_soundness_never_depends_on_effect_or_pep484() {
        let cargo_toml = include_str!("../Cargo.toml");
        for line in cargo_toml.lines() {
            let code = line.split('#').next().unwrap_or("");
            assert!(
                !code.contains("heki_effect"),
                "ADR-004 VIOLATION: `heki_soundness` MUST NEVER depend on `heki_effect`"
            );
        }
        // Walk all `.rs` files in `crates/heki_soundness/src/` and ensure no PEP 484 annotation fields are accessed.
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let rs_files = std::fs::read_dir(src_dir)
            .expect("src dir readable")
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|e| e == "rs"));
        let mut checked = 0;
        for path in rs_files {
            let text = std::fs::read_to_string(&path).expect("rs file readable");
            // Strip test modules and `//` comments so docs/comments are not falsely matched.
            let prod = text.split("#[cfg(test)]").next().unwrap_or(&text);
            let offending: Vec<&str> = prod
                .lines()
                .map(|line| line.split("//").next().unwrap_or(""))
                .filter(|code| {
                    [".annotation", ".return_annotation", ".local_annotations"]
                        .iter()
                        .any(|forbidden| code.contains(forbidden))
                })
                .collect();
            let file = path.display();
            assert!(
                offending.is_empty(),
                "ADR-004 VIOLATION in {file}: `heki_soundness` MUST NEVER trust PEP 484 annotations: {offending:?}"
            );
            checked += 1;
        }
        // The walk saw the sources (lib, interpreter, liveness, obligations, opt_rules, edits).
        assert!(checked >= 6, "only {checked} source files checked");
    }
}
