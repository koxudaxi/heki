//! The declared contracts the summaries depend on ([`heki_effect::Contracts`]), built from the
//! configuration: the `readonly-returns` producers (`MUT202`), the `invalidation` contracts
//! (`MUT109`), and whether any ownership contract is declared. `heki check` and the LSP engine hand them to the analysis, so a wrapper of a
//! producer and an invalidator reached through calls are known across files without IR.

use crate::mut_rules::{Allowance, AllowanceKeys, ReadOnlyProducers};
use crate::{DottedPattern, RuleConfigError, RuleSelection};
use heki_effect::{Contracts, TypedWrites};
use heki_env::HekiConfig;
use rustc_hash::FxHashMap;
use std::sync::{Arc, Mutex, PoisonError};

/// The most `invalidation` contracts a configuration may declare (one bit each).
pub const MAX_INVALIDATIONS: usize = 64;

/// One `invalidation` contract's patterns.
#[derive(Debug)]
pub(crate) struct InvalidationNames {
    /// `targets`: the configured text and its `(class, attr)` patterns.
    targets: Vec<(String, DottedPattern, DottedPattern)>,
    invalidators: Vec<DottedPattern>,
    exempt_writers: Vec<DottedPattern>,
}

fn names(patterns: &[DottedPattern], fqn: &str, qualname: &str) -> bool {
    patterns
        .iter()
        .any(|p| p.matches(fqn) || p.matches(qualname))
}

impl InvalidationNames {
    pub(crate) fn names_invalidator(&self, fqn: &str, qualname: &str) -> bool {
        names(&self.invalidators, fqn, qualname)
    }

    pub(crate) fn names_exempt_writer(&self, fqn: &str, qualname: &str) -> bool {
        names(&self.exempt_writers, fqn, qualname)
    }

    /// The first target naming attribute `attr` of an object of class `classes[0]` (`classes`:
    /// the class and its project bases), as configured.
    pub(crate) fn target(&self, classes: &[String], attr: &str) -> Option<&str> {
        self.targets
            .iter()
            .find(|(_, class, a)| a.matches(attr) && classes.iter().any(|name| class.matches(name)))
            .map(|(text, _, _)| text.as_str())
    }
}

/// [`Contracts`] over a configuration's `readonly-returns` and `invalidation` entries.
#[derive(Debug)]
pub(crate) struct ConfigContracts {
    /// [`Contracts::ownership`].
    ownership: bool,
    producers: ReadOnlyProducers,
    pub(crate) invalidations: Vec<InvalidationNames>,
    /// The masks of [`Contracts::invalidator_of`] by name.
    invalidators: NameMasks,
    /// What each `write-owners` contract allows and forbids, in configuration order.
    writers: Vec<Allowance>,
    /// The masks of [`Contracts::writer_contexts`] by name.
    writer_contexts: NameMasks,
}

/// Contract masks memoized by `(fqn, qualname)`: a mask depends on the name alone, and the
/// summaries ask for the callee of every call, which would match the same names against every
/// pattern again.
#[derive(Debug, Default)]
struct NameMasks(Mutex<FxHashMap<String, FxHashMap<String, u64>>>);

impl NameMasks {
    fn get(&self, fqn: &str, qualname: &str, compute: impl FnOnce() -> u64) -> u64 {
        let known = |map: &FxHashMap<String, FxHashMap<String, u64>>| {
            map.get(fqn).and_then(|q| q.get(qualname)).copied()
        };
        if let Some(mask) = known(&self.0.lock().unwrap_or_else(PoisonError::into_inner)) {
            return mask;
        }
        let mask = compute();
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(fqn.to_owned())
            .or_default()
            .insert(qualname.to_owned(), mask);
        mask
    }
}

impl ConfigContracts {
    /// Compile the patterns: a target without a dot is `InvalidTarget`, a bad glob `Glob`, and
    /// more than [`MAX_INVALIDATIONS`] contracts `TooManyInvalidations`.
    pub(crate) fn new(config: &HekiConfig) -> Result<Self, RuleConfigError> {
        if config.invalidation.len() > MAX_INVALIDATIONS {
            return Err(RuleConfigError::TooManyInvalidations(
                config.invalidation.len(),
            ));
        }
        let mut invalidations = Vec::new();
        for rule in &config.invalidation {
            let mut targets = Vec::new();
            for t in &rule.targets {
                let Some((class, attr)) = t.rsplit_once('.') else {
                    return Err(RuleConfigError::InvalidTarget {
                        section: "invalidation",
                        target: t.clone(),
                    });
                };
                targets.push((
                    t.clone(),
                    DottedPattern::new(class, "invalidation.targets")?,
                    DottedPattern::new(attr, "invalidation.targets")?,
                ));
            }
            invalidations.push(InvalidationNames {
                targets,
                invalidators: crate::mut_rules::patterns(
                    &rule.invalidators,
                    "invalidation.invalidators",
                )?,
                exempt_writers: crate::mut_rules::patterns(
                    &rule.exempt_writers,
                    "invalidation.exempt-writers",
                )?,
            });
        }
        let writers = config
            .write_owners
            .iter()
            .map(|rule| {
                Allowance::new(&AllowanceKeys {
                    allowed: (&rule.allowed_writers, "write-owners.allowed-writers"),
                    forbidden: (&rule.forbidden_writers, "write-owners.forbidden-writers"),
                    exempt: (&rule.exempt_methods, "write-owners.exempt-methods"),
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            ownership: declares_ownership(config),
            producers: ReadOnlyProducers::new(config)?,
            invalidations,
            invalidators: NameMasks::default(),
            writers,
            writer_contexts: NameMasks::default(),
        })
    }
}

impl Contracts for ConfigContracts {
    fn ownership(&self) -> bool {
        self.ownership
    }

    fn readonly_producer(&self, fqn: &str) -> Option<bool> {
        self.producers.deep_for(fqn)
    }

    fn invalidations(&self) -> usize {
        self.invalidations.len()
    }

    fn invalidator_of(&self, fqn: &str, qualname: &str) -> u64 {
        self.invalidators.get(fqn, qualname, || {
            mask(&self.invalidations, |c| c.names_invalidator(fqn, qualname))
        })
    }

    fn writer_contexts(&self, fqn: &str, qualname: &str) -> u64 {
        self.writer_contexts.get(fqn, qualname, || {
            let Some((module, _)) = fqn
                .strip_suffix(qualname)
                .and_then(|m| m.strip_suffix('.'))
                .map(|m| (m, ()))
            else {
                return 0;
            };
            let key = heki_effect::FuncKey {
                module: module.to_owned(),
                qualname: qualname.to_owned(),
            };
            self.writers.iter().enumerate().fold(0, |acc, (k, a)| {
                let bit = 2 * (k % 32);
                let allowed = u64::from(a.allows(&key)) << bit;
                let forbidden = u64::from(a.has_forbidden() && a.in_scope(&key)) << (bit + 1);
                acc | allowed | forbidden
            })
        })
    }

    fn exempt_writer(&self, fqn: &str, qualname: &str) -> u64 {
        mask(&self.invalidations, |c| {
            c.names_exempt_writer(fqn, qualname)
        })
    }

    fn target_of(&self, classes: &[String], attr: &str) -> u64 {
        mask(&self.invalidations, |c| c.target(classes, attr).is_some())
    }
}

/// Bit `i` set for every contract `i` that `has` holds for.
fn mask(contracts: &[InvalidationNames], has: impl Fn(&InvalidationNames) -> bool) -> u64 {
    contracts
        .iter()
        .enumerate()
        .filter(|(_, c)| has(c))
        .fold(0, |acc, (i, _)| acc | (1u64 << i))
}

/// The contracts of `config` the summaries depend on (`None` when it declares no producer and no
/// ownership contract, so nothing is computed for them).
pub fn summary_contracts(
    config: &HekiConfig,
) -> Result<Option<Arc<dyn Contracts>>, RuleConfigError> {
    let contracts = ConfigContracts::new(config)?;
    if contracts.producers.is_empty() && !contracts.ownership {
        return Ok(None);
    }
    Ok(Some(Arc::new(contracts)))
}

/// [`Contracts::ownership`] of `config`.
fn declares_ownership(config: &HekiConfig) -> bool {
    !(config.write_owners.is_empty()
        && config.read_owners.is_empty()
        && config.effect_boundaries.is_empty()
        && config.invalidation.is_empty())
}

/// Whether propagation carries typed writes for the rules `selection` runs on `config`: with an
/// ownership contract (the gate of the other contract-only facts; `MUT201` and `EFF201` read
/// the callers' typed write sites, and `EFF201`'s `IndirectHelper` sites depend on the callees'
/// write facts), or with `MUT108`, which reads the callees' write facts. The readers assert it
/// ([`heki_effect::ProjectIndex::propagates_typed_writes`]).
pub fn typed_writes(config: &HekiConfig, selection: &RuleSelection) -> TypedWrites {
    if declares_ownership(config) || selection.is_enabled("MUT108") {
        TypedWrites::Propagated
    } else {
        TypedWrites::Local
    }
}
