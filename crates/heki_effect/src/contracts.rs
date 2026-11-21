//! Declared contracts that summaries depend on: the producers of `[[tool.heki.readonly-returns]]`
//! (`MUT202`), the `[[tool.heki.invalidation]]` contracts (`MUT109`), whether any ownership
//! contract is declared, and the names whose unresolved reads and calls the strict contracts
//! count (`MUT206`, `MUT207`).
//!
//! The rules layer implements [`Contracts`] from the configuration and hands it to
//! [`crate::analyze_project_with`]; the index keeps it for incremental updates. Facts that depend
//! on a contract are computed into the summaries, so a change of them takes part in Early Cutoff
//! and nothing is computed for contracts a project does not declare. Without a `Contracts`, only
//! the configuration-free half of `MUT202` (`@lru_cache` results) is summarized.

use crate::FuncKey;
use std::fmt::Debug;

/// What the declared contracts say about a name, as far as the summaries need it. Masks have
/// one bit per `invalidation` contract, in configuration order (at most 64 contracts).
pub trait Contracts: Debug + Send + Sync {
    /// Whether an ownership contract is declared (`write-owners`, `read-owners`,
    /// `effect-boundaries`, `invalidation`). Only the rules of those contracts read the facts it
    /// turns on: attributes and keys named by `Final` string constants, writes into the entries
    /// of attribute containers, whether a write constructs its object, and the writes heki
    /// cannot tie to a class.
    fn ownership(&self) -> bool;

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

    /// How the `write-owners` contracts treat a write made inside the function: bit `2k` when
    /// contract `k` allows it (an allowed writer or an exempt method), bit `2k + 1` when the
    /// function is one of its forbidden writers. Contracts past the 32nd share the bits of
    /// contract `k % 32`: their chains are then kept apart less finely.
    fn writer_contexts(&self, func: &FuncKey) -> u64;

    /// The contracts whose targets include attribute `attr` of an object of class `classes[0]`;
    /// `classes` lists that class and then its bases (a target class covers its subclasses).
    fn target_of(&self, classes: &[String], attr: &str) -> u64;

    /// Whether a target of some contract names an attribute of class `classes[0]` or of one of
    /// its bases (`classes` as for [`Contracts::target_of`]).
    fn names_class(&self, classes: &[String]) -> bool;

    /// Whether a `read-owners` contract with `unresolved = "report"` names attribute `attr`: a
    /// read of it on an object of unknown class is an unresolved access (`MUT206`).
    fn unresolved_read(&self, attr: &str) -> bool;

    /// Whether a `call-owners` contract with `unresolved = "report"` names a function, method,
    /// or class whose last name is `name`: a call of a method so named on an object of unknown
    /// class is an unresolved access (`MUT207`).
    fn unresolved_call(&self, name: &str) -> bool;
}
