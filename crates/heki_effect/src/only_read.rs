//! The parameters a function only reads ([`EffectSummary::only_read_params`]), and what its own
//! body does with the parameters annotated with a mutable container ([`ContainerParams`]): the
//! facts a read-only annotation (`Sequence`, `Mapping`, `AbstractSet`) of such a parameter needs.
//!
//! One judgment has two readers: whether a function changes the object itself comes from
//! [`EffectSummary::mutated_param_paths`] alone, the fact `MUT110` reports, so a parameter only
//! read here is never one `MUT110` would report changed. The rest (stores, captures, returns,
//! the calls it is handed to) is this module's: the extractor finds it in the function's own
//! body, and propagation joins what the project callees do with it.

use crate::callgraph::bound_params;
use crate::{BoundCallee, EffectSummary};
use serde::{Deserialize, Serialize};

/// The parameters (bit `i`: parameter `i < 64`) annotated with a builtin list, dict, or set, or
/// with a mutable abstract container (`resolver::mutable_container`), and what the function's
/// own body does with each: the local facts a caller's file reads without the function's IR.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerParams {
    /// The annotated parameters.
    pub annotated: u64,
    /// Those whose every use in the body the read-only type allows: its own methods, item reads,
    /// iteration, membership tests, `len`, comparisons, and handing it to a call; and whose
    /// derived values (a slice of a list, a set operation of a set, a join with another value
    /// such as `p or []`) are only iterated, unpacked, tested, measured, or handed to a library
    /// call Layer 1 lists as reading them. A set is iterated nowhere but by a call that takes no
    /// order from it (`len`, `sorted`, `frozenset`, ...).
    pub abstract_use: u64,
    /// Those whose name is bound once: no assignment or `del` of it, except
    /// `if p is None: p = <a new container of its kind>`.
    pub bound_once: u64,
}

impl EffectSummary {
    /// The positions whose object the function only reads, itself and through every function
    /// it hands it to: [`Self::only_read_params`] where [`Self::mutated_param_paths`] has no
    /// change of the object itself.
    pub fn only_read_mask(&self) -> u64 {
        let changed = self
            .mutated_param_paths
            .iter()
            .filter(|(j, paths)| **j < 64 && paths.contains(""))
            .fold(0, |acc, (j, _)| acc | 1u64 << j);
        self.only_read_params & !changed
    }

    /// The parameters annotated with a mutable container that the function only reads
    /// ([`Self::only_read_mask`] of [`ContainerParams::annotated`]).
    pub fn only_read_containers(&self) -> u64 {
        self.container_params
            .as_deref()
            .map_or(0, |c| c.annotated & self.only_read_mask())
    }
}

/// The caller's positions a call hands to `callee` where it does not only read them: those
/// bound to its positions outside [`EffectSummary::only_read_mask`], every bound one when the
/// callee has no summary.
pub(crate) fn not_only_read_through(callee: Option<&EffectSummary>, bc: &BoundCallee) -> u64 {
    let open = callee.map_or(u64::MAX, |c| !c.only_read_mask());
    bound_params(open, bc)
}
