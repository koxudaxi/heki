//! Declared contracts that summaries depend on: the producers of `[[tool.heki.readonly-returns]]`
//! (`MUT202`) and the `[[tool.heki.invalidation]]` contracts (`MUT109`).
//!
//! The rules layer implements [`Contracts`] from the configuration and hands it to
//! [`crate::analyze_project_with`]; the index keeps it for incremental updates. Facts that depend
//! on a contract are computed into the summaries, so a change of them takes part in Early Cutoff
//! and nothing is computed for contracts a project does not declare. Without a `Contracts`, only
//! the configuration-free half of `MUT202` (`@lru_cache` results) is summarized.

use std::fmt::Debug;

/// What the declared contracts say about a name, as far as the summaries need it. Masks have
/// one bit per `invalidation` contract, in configuration order (at most 64 contracts).
pub trait Contracts: Debug + Send + Sync {
    /// `Some(deep)` when a `readonly-returns` entry names `fqn`: a project function, an external
    /// symbol, or an attribute (`pkg.mod.Class.attr`).
    fn readonly_producer(&self, fqn: &str) -> Option<bool>;

    /// The number of `invalidation` contracts.
    fn invalidations(&self) -> usize;

    /// The contracts that name the function (`fqn`, and `qualname` for a project function) an
    /// invalidator: calling it announces a change of their targets.
    fn invalidator_of(&self, fqn: &str, qualname: &str) -> u64;

    /// The contracts that name the function an exempt writer: its writes need no announcement.
    fn exempt_writer(&self, fqn: &str, qualname: &str) -> u64;

    /// The contracts whose targets include attribute `attr` of an object of class `classes[0]`;
    /// `classes` lists that class and then its bases (a target class covers its subclasses).
    fn target_of(&self, classes: &[String], attr: &str) -> u64;
}
