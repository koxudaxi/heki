//! Declared contracts that summaries depend on: the producers of `[[tool.heki.readonly-returns]]`
//! (`MUT202`), the `[[tool.heki.invalidation]]` contracts (`MUT109`), whether any ownership
//! contract is declared, the names whose unresolved reads and calls the strict contracts count
//! (`MUT206`, `MUT207`), whether a call contract counts references as calls (`MUT204`,
//! `EFF201`), and what the effect boundaries forbid outside the project (`EFF201`).
//!
//! The rules layer implements [`Contracts`] from the configuration and hands it to
//! [`crate::analyze_project_with`]; the index keeps it for incremental updates. Facts that depend
//! on a contract are computed into the summaries, so a change of them takes part in Early Cutoff
//! and nothing is computed for contracts a project does not declare. Without a `Contracts`, only
//! the configuration-free half of `MUT202` (`@lru_cache` results) is summarized.

use crate::{FuncKey, ProjectDecls};
use rustc_hash::FxHashSet;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;

/// A class as the contracts match it. A pattern with a module part (`pkg.model:Model`) compares
/// the class's module and its qualname inside it, which the fqn alone does not tell apart
/// (`pkg.sub.Model` is in module `pkg.sub`, or nested in a class `sub` of `pkg`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractClass {
    pub fqn: String,
    /// The qualname of the project class inside its module (`Outer.Inner`), looked up only when
    /// a class pattern has a module part ([`Contracts::names_class_modules`]); `None` otherwise,
    /// and for a class the project does not define.
    pub qualname: Option<String>,
}

impl ContractClass {
    /// The class `fqn` as `contracts` match it.
    pub fn new(contracts: &dyn Contracts, project: &ProjectDecls, fqn: String) -> Self {
        let qualname = if contracts.names_class_modules() {
            project.class_qualname(&fqn).map(str::to_owned)
        } else {
            None
        };
        Self { fqn, qualname }
    }

    /// `class` and then its project bases, as [`Contracts::target_of`] takes them.
    pub fn lineage(contracts: &dyn Contracts, project: &ProjectDecls, class: &str) -> Vec<Self> {
        std::iter::once(class.to_owned())
            .chain(project.ancestors_of(class))
            .map(|fqn| Self::new(contracts, project, fqn))
            .collect()
    }

    /// The fqn and the qualname of attribute `attr` of the class, as
    /// [`Contracts::readonly_producer`] takes them (the fqn twice without the qualname).
    pub fn member(&self, attr: &str) -> (String, String) {
        let fqn = format!("{}.{attr}", self.fqn);
        let qualname = self
            .qualname
            .as_ref()
            .map_or_else(|| fqn.clone(), |q| format!("{q}.{attr}"));
        (fqn, qualname)
    }
}

/// What the declared contracts say about a name, as far as the summaries need it. Masks have
/// one bit per `invalidation` contract, in configuration order (at most 64 contracts).
pub trait Contracts: Debug + Send + Sync {
    /// Whether an ownership contract is declared (`write-owners`, `read-owners`,
    /// `effect-boundaries`, `invalidation`). Only the rules of those contracts read the facts it
    /// turns on: attributes and keys named by `Final` string constants, writes into the entries
    /// of attribute containers, whether a write constructs its object, and the writes heki
    /// cannot tie to a class.
    fn ownership(&self) -> bool;

    /// Whether an `invalidation` target's class or a `readonly-returns` producer has a module
    /// part: only then does a [`ContractClass`] carry its qualname.
    fn names_class_modules(&self) -> bool;

    /// `Some(deep)` when a `readonly-returns` entry names a project function (`fqn` and its
    /// `qualname`), an attribute of a class ([`ContractClass::member`]), or an external symbol
    /// (the symbol twice).
    fn readonly_producer(&self, fqn: &str, qualname: &str) -> Option<bool>;

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
    fn target_of(&self, classes: &[ContractClass], attr: &str) -> u64;

    /// Whether a target of some contract names an attribute of class `classes[0]` or of one of
    /// its bases (`classes` as for [`Contracts::target_of`]).
    fn names_class(&self, classes: &[ContractClass]) -> bool;

    /// Whether a `read-owners` contract with `unresolved = "report"` names attribute `attr`: a
    /// read of it on an object of unknown class is an unresolved access (`MUT206`).
    fn unresolved_read(&self, attr: &str) -> bool;

    /// Whether a `call-owners` contract with `unresolved = "report"` names a function, method,
    /// or class whose last name is `name`: a call of a method so named on an object of unknown
    /// class is an unresolved access (`MUT207`).
    fn unresolved_call(&self, name: &str) -> bool;

    /// Whether a `call-owners` contract, or an effect boundary forbidding `calls`, counts a
    /// reference handing one of its targets on as a call of it (`references = "calls"`): the
    /// summaries then carry `FileSummary::reference_sites`.
    fn references(&self) -> bool;

    /// Whether such a contract also counts a class statement naming one of its targets among
    /// its bases (`references = "calls-and-bases"`): the reference sites then include the bases
    /// of class statements (`CallSite::is_base`).
    fn reference_bases(&self) -> bool;

    /// The parameters the argument lists of call targets name (`DataType(is_dict=True)`): the
    /// summaries then keep the constants each call passes for them (`CallSite::arguments`),
    /// looking up the names of the parameters the call binds.
    fn marked_params(&self) -> &FxHashSet<String>;

    /// Whether an effect boundary forbids calling `symbol`, a function or class outside the
    /// project (a `call-targets` entry names it, or `reads-environment` with the ecosystem
    /// catalog): its calls are kept (`FileSummary::external_accesses`).
    fn external_call(&self, symbol: &str) -> bool;

    /// Whether some effect boundary forbids reading module attributes outside the project:
    /// without one, no read is looked at.
    fn reads_external(&self) -> bool;

    /// Whether an effect boundary forbids reading `fqn`, an attribute of a module outside the
    /// project (`sys.platform`).
    fn external_read(&self, fqn: &str) -> bool;

    /// Whether an effect boundary reports the imports that probe whether a module is installed
    /// (`import-probes = "report"`).
    fn import_probes(&self) -> bool;

    /// Whether an effect boundary checks a module body (a scope naming `<module>`): what the
    /// bodies of its classes take from outside the project is then the module body's
    /// (`FileSummary::external_accesses`).
    fn checks_module_bodies(&self) -> bool;

    /// Whether a `call-owners` contract allows the calls through the receiver of the method
    /// making them (`own-receiver = "allow"`): the call sites then keep the parameters their
    /// receiver may be (`Dispatch::recv_params`).
    fn own_receiver_calls(&self) -> bool;

    /// Whether a `write-owners` contract allows the writes through a value typed with a protocol
    /// it names (`allowed-protocols`): the write sites then tell the writes through a protocol
    /// (`TypedFieldWriteSite::through_protocol`).
    fn write_protocols(&self) -> bool;
}
