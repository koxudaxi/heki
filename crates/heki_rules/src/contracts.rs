//! The declared contracts the summaries depend on ([`heki_effect::Contracts`]), built from the
//! configuration: the `readonly-returns` producers (`MUT202`), the `invalidation` contracts
//! (`MUT109`), whether any ownership contract is declared, the names the strict
//! `read-owners` and `call-owners` contracts count unresolved accesses of (`MUT206`, `MUT207`),
//! whether a call contract counts references as calls (`MUT204`, `EFF201`), and what the effect
//! boundaries forbid outside the project (`EFF201`).
//! `heki check` and the LSP engine hand them to the analysis, so a wrapper of a producer and an
//! invalidator reached through calls are known across files without IR.

use crate::inspect::ContractRole;
use crate::mut_rules::{
    Allowance, AllowanceKeys, FunctionMatcher, FunctionMatchers, ReadOnlyProducers, References,
    patterns, split_arguments,
};
use crate::{
    CheckContext, DottedPattern, RuleConfigError, RuleSelection, last_name, split_last_dot,
};
use heki_effect::{ContractClass, Contracts, FuncKey, OptInFacts, TypedWrites};
use heki_env::HekiConfig;
use heki_kb::{EffectiveEcosystemKb, EnvironmentAccess};
use rustc_hash::{FxHashMap, FxHashSet};
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
    patterns.iter().any(|p| p.matches_def(fqn, qualname))
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
    pub(crate) fn target(&self, classes: &[ContractClass], attr: &str) -> Option<&str> {
        self.targets
            .iter()
            .find(|(_, class, a)| a.matches(attr) && classes.iter().any(|c| class.matches_class(c)))
            .map(|(text, _, _)| text.as_str())
    }
}

/// [`Contracts`] over a configuration's `readonly-returns` and `invalidation` entries.
#[derive(Debug)]
pub(crate) struct ConfigContracts {
    /// [`Contracts::ownership`].
    ownership: bool,
    /// [`Contracts::names_class_modules`].
    class_modules: bool,
    producers: ReadOnlyProducers,
    pub(crate) invalidations: Vec<InvalidationNames>,
    /// The masks of [`Contracts::invalidator_of`] by name.
    invalidators: NameMasks,
    /// What each `write-owners` contract allows and forbids, in configuration order.
    writers: Vec<Allowance>,
    /// The masks of [`Contracts::writer_contexts`] by name.
    writer_contexts: NameMasks,
    /// The attribute patterns of the `read-owners` targets with `unresolved = "report"`.
    unresolved_reads: Vec<DottedPattern>,
    /// The last names of the `call-owners` targets with `unresolved = "report"`.
    unresolved_calls: Vec<DottedPattern>,
    /// [`Contracts::references`] and [`Contracts::reference_bases`].
    references: References,
    /// [`Contracts::marked_params`].
    marked_params: FxHashSet<String>,
    external: ExternalNames,
    /// [`Contracts::own_receiver_calls`].
    own_receiver_calls: bool,
    /// [`Contracts::write_protocols`].
    write_protocols: bool,
}

/// What the effect boundaries forbid outside the project ([`Contracts::external_call`],
/// [`Contracts::external_read`], [`Contracts::import_probes`]).
#[derive(Debug)]
struct ExternalNames {
    /// The `call-targets` of the boundaries forbidding `calls` (`None`: too many globs).
    calls: Option<FunctionMatchers>,
    /// The `read-targets` of the boundaries forbidding `reads-targets` (`None`: too many globs).
    reads: Option<FunctionMatchers>,
    /// The catalog's environment calls and reads, when a boundary forbids `reads-environment`.
    environment: FxHashMap<String, EnvironmentAccess>,
    probes: bool,
    /// [`Contracts::checks_module_bodies`].
    module_bodies: bool,
}

impl ExternalNames {
    /// A bad pattern is left out: it is a configuration error of `EFF201`, which parses every
    /// boundary before it reports, and only when it runs, as for the other contracts.
    fn new(config: &HekiConfig, kb: &EffectiveEcosystemKb) -> Self {
        let forbidding = |token: &'static str| {
            config
                .effect_boundaries
                .iter()
                .filter(move |b| b.forbid.iter().any(|t| t == token))
        };
        // A call target with an argument list names project functions only.
        let matchers = |texts: Vec<&String>, context: &'static str| {
            let named = texts
                .into_iter()
                .filter(|t| matches!(split_arguments(t, context), Ok((_, None))))
                .filter_map(|t| FunctionMatcher::named(t, context).ok())
                .collect();
            FunctionMatchers::new(named, context).ok()
        };
        let environment = if forbidding("reads-environment").next().is_some() {
            kb.entries
                .values()
                .filter_map(|e| Some((e.symbol.clone(), e.environment_access()?)))
                .collect()
        } else {
            FxHashMap::default()
        };
        Self {
            calls: matchers(
                forbidding("calls").flat_map(|b| &b.call_targets).collect(),
                "effect-boundaries.call-targets",
            ),
            reads: matchers(
                forbidding("reads-targets")
                    .flat_map(|b| &b.read_targets)
                    .collect(),
                "effect-boundaries.read-targets",
            ),
            environment,
            probes: forbidding("reads-environment")
                .any(|b| b.import_probes.as_deref() == Some("report")),
            module_bodies: config.effect_boundaries.iter().any(|b| {
                b.scopes.iter().any(|s| {
                    DottedPattern::new(s, "effect-boundaries.scopes")
                        .is_ok_and(|p| p.names_module_bodies())
                })
            }),
        }
    }

    /// Whether the catalog tags `symbol` as an environment access of kind `access`.
    fn environment(&self, symbol: &str, access: EnvironmentAccess) -> bool {
        self.environment.get(symbol) == Some(&access)
    }
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
    pub(crate) fn new(
        config: &HekiConfig,
        kb: &EffectiveEcosystemKb,
    ) -> Result<Self, RuleConfigError> {
        if config.invalidation.len() > MAX_INVALIDATIONS {
            return Err(RuleConfigError::TooManyInvalidations(
                config.invalidation.len(),
            ));
        }
        let mut invalidations = Vec::new();
        for rule in &config.invalidation {
            let mut targets = Vec::new();
            for t in &rule.targets {
                let Some((class, attr)) = split_last_dot(t) else {
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
        let strict = |mode: &Option<String>| mode.as_deref() == Some("report");
        let reads = config.read_owners.iter().filter(|r| strict(&r.unresolved));
        let calls = config.call_owners.iter().filter(|r| strict(&r.unresolved));
        let producers = ReadOnlyProducers::new(config)?;
        let class_modules = producers.names_modules()
            || invalidations
                .iter()
                .flat_map(|c| &c.targets)
                .any(|(_, class, _)| class.module().is_some());
        Ok(Self {
            ownership: declares_ownership(config),
            class_modules,
            producers,
            invalidations,
            invalidators: NameMasks::default(),
            writers,
            writer_contexts: NameMasks::default(),
            unresolved_reads: last_names(reads.flat_map(|r| &r.targets), "read-owners.targets")?,
            unresolved_calls: call_last_names(calls.flat_map(|r| &r.targets))?,
            references: counts_references(config),
            marked_params: marked_params(config),
            external: ExternalNames::new(config, kb),
            own_receiver_calls: config
                .call_owners
                .iter()
                .any(|r| r.own_receiver.as_deref() == Some("allow")),
            write_protocols: config
                .write_owners
                .iter()
                .any(|r| !r.allowed_protocols.is_empty()),
        })
    }
}

/// [`last_names`] of call targets, whose argument lists name no function.
fn call_last_names<'t>(
    targets: impl Iterator<Item = &'t String>,
) -> Result<Vec<DottedPattern>, RuleConfigError> {
    let context = "call-owners.targets";
    targets
        .map(|t| DottedPattern::new(last_name(split_arguments(t, context)?.0), context))
        .collect()
}

/// [`Contracts::marked_params`] of `config`. A bad argument list is left out: it is a
/// configuration error of the rule of its contract, which parses it before it reports.
fn marked_params(config: &HekiConfig) -> FxHashSet<String> {
    let calls = config.call_owners.iter().flat_map(|r| &r.targets);
    let boundaries = config
        .effect_boundaries
        .iter()
        .filter(|b| b.forbid.iter().any(|t| t == "calls"))
        .flat_map(|b| &b.call_targets);
    calls
        .chain(boundaries)
        .filter_map(|t| split_arguments(t, "call-targets").ok()?.1)
        .flatten()
        .map(|(name, _)| name)
        .collect()
}

/// The patterns of the last dotted names of `targets` (`children` of `pkg.Reference.children`).
fn last_names<'t>(
    targets: impl Iterator<Item = &'t String>,
    context: &'static str,
) -> Result<Vec<DottedPattern>, RuleConfigError> {
    targets
        .map(|t| DottedPattern::new(last_name(t), context))
        .collect()
}

impl Contracts for ConfigContracts {
    fn ownership(&self) -> bool {
        self.ownership
    }

    fn names_class_modules(&self) -> bool {
        self.class_modules
    }

    fn readonly_producer(&self, fqn: &str, qualname: &str) -> Option<bool> {
        self.producers.deep_for(fqn, qualname)
    }

    fn invalidations(&self) -> usize {
        self.invalidations.len()
    }

    fn invalidator_of(&self, fqn: &str, qualname: &str) -> u64 {
        self.invalidators.get(fqn, qualname, || {
            mask(&self.invalidations, |c| c.names_invalidator(fqn, qualname))
        })
    }

    fn writer_contexts(&self, func: &FuncKey) -> u64 {
        self.writer_contexts.get(&func.fqn(), &func.qualname, || {
            self.writers.iter().enumerate().fold(0, |acc, (k, a)| {
                let bit = 2 * (k % 32);
                let allowed = u64::from(a.allows(func)) << bit;
                let forbidden = u64::from(a.has_forbidden() && a.in_scope(func)) << (bit + 1);
                acc | allowed | forbidden
            })
        })
    }

    fn exempt_writer(&self, fqn: &str, qualname: &str) -> u64 {
        mask(&self.invalidations, |c| {
            c.names_exempt_writer(fqn, qualname)
        })
    }

    fn target_of(&self, classes: &[ContractClass], attr: &str) -> u64 {
        mask(&self.invalidations, |c| c.target(classes, attr).is_some())
    }

    fn names_class(&self, classes: &[ContractClass]) -> bool {
        self.invalidations.iter().any(|c| {
            c.targets
                .iter()
                .any(|(_, class, _)| classes.iter().any(|name| class.matches_class(name)))
        })
    }

    fn unresolved_read(&self, attr: &str) -> bool {
        self.unresolved_reads.iter().any(|p| p.matches(attr))
    }

    fn unresolved_call(&self, name: &str) -> bool {
        self.unresolved_calls.iter().any(|p| p.matches(name))
    }

    fn external_call(&self, symbol: &str) -> bool {
        self.external.environment(symbol, EnvironmentAccess::Call)
            || self.external.environment(symbol, EnvironmentAccess::Import)
            || self
                .external
                .calls
                .as_ref()
                .is_some_and(|m| !m.naming_external(symbol).is_empty())
    }

    fn reads_external(&self) -> bool {
        self.external.reads.as_ref().is_some_and(|m| !m.is_empty())
            || self
                .external
                .environment
                .values()
                .any(|a| *a == EnvironmentAccess::Read)
    }

    fn external_read(&self, fqn: &str) -> bool {
        self.external.environment(fqn, EnvironmentAccess::Read)
            || self
                .external
                .reads
                .as_ref()
                .is_some_and(|m| !m.naming_external(fqn).is_empty())
    }

    fn import_probes(&self) -> bool {
        self.external.probes
    }

    fn checks_module_bodies(&self) -> bool {
        self.external.module_bodies
    }

    fn references(&self) -> bool {
        self.references != References::Ignored
    }

    fn reference_bases(&self) -> bool {
        self.references >= References::CallsAndBases
    }

    fn reference_reads(&self) -> bool {
        self.references == References::Reads
    }

    fn marked_params(&self) -> &FxHashSet<String> {
        &self.marked_params
    }

    fn own_receiver_calls(&self) -> bool {
        self.own_receiver_calls
    }

    fn write_protocols(&self) -> bool {
        self.write_protocols
    }
}

/// The most a contract of `config` counts as calls ([`Contracts::references`],
/// [`Contracts::reference_bases`], [`Contracts::reference_reads`]; the values are checked where
/// the rules parse them).
fn counts_references(config: &HekiConfig) -> References {
    let counted = |references: &Option<String>| {
        references
            .as_deref()
            .map_or(References::Ignored, References::named)
    };
    let calls = config.call_owners.iter().map(|r| counted(&r.references));
    let boundaries = config
        .effect_boundaries
        .iter()
        .filter(|b| b.forbid.iter().any(|t| t == "calls"))
        .map(|b| counted(&b.references));
    calls.chain(boundaries).max().unwrap_or(References::Ignored)
}

/// Bit `i` set for every contract `i` that `has` holds for.
fn mask(contracts: &[InvalidationNames], has: impl Fn(&InvalidationNames) -> bool) -> u64 {
    contracts
        .iter()
        .enumerate()
        .filter(|(_, c)| has(c))
        .fold(0, |acc, (i, _)| acc | (1u64 << i))
}

/// The contracts of `config` the summaries depend on (`None` when it declares no producer, no
/// ownership contract, no strict `call-owners` contract, and no call contract counting
/// references, so nothing is computed for them).
pub fn summary_contracts(
    config: &HekiConfig,
    kb: &EffectiveEcosystemKb,
) -> Result<Option<Arc<dyn Contracts>>, RuleConfigError> {
    let contracts = ConfigContracts::new(config, kb)?;
    if contracts.producers.is_empty()
        && !contracts.ownership
        && contracts.unresolved_reads.is_empty()
        && contracts.unresolved_calls.is_empty()
        && contracts.references == References::Ignored
        && contracts.marked_params.is_empty()
    {
        return Ok(None);
    }
    Ok(Some(Arc::new(contracts)))
}

/// [`Contracts::ownership`] of `config`: an ownership contract is declared.
pub fn declares_ownership(config: &HekiConfig) -> bool {
    !(config.write_owners.is_empty()
        && config.read_owners.is_empty()
        && config.effect_boundaries.is_empty()
        && config.invalidation.is_empty())
}

/// Whether propagation carries typed writes for the rules `selection` runs on `config`: with an
/// ownership contract (the gate of the other contract-only facts; `MUT201` and `EFF201` read
/// the callers' typed write sites, and `EFF201`'s `IndirectHelper` sites depend on the callees'
/// write facts), or with `MUT108` or `MUT112`, which read the callees' write facts. The readers
/// assert it ([`heki_effect::ProjectIndex::propagates_typed_writes`]).
pub fn typed_writes(config: &HekiConfig, selection: &RuleSelection) -> TypedWrites {
    if declares_ownership(config)
        || selection.is_enabled("MUT108")
        || selection.is_enabled("MUT112")
    {
        TypedWrites::Propagated
    } else {
        TypedWrites::Local
    }
}

/// The opt-in facts the rules `selection` runs read: `MUT112`'s return origins, `MUT113`'s
/// iteration facts, `EFF109`'s lock facts, `EFF110`'s one-at-a-time uses, and the must-facts of
/// the effect rules whose reports are split by them, each only with its rules. The readers assert
/// it ([`heki_effect::ProjectIndex::opt_in_facts`]).
pub fn opt_in_facts(selection: &RuleSelection) -> OptInFacts {
    OptInFacts {
        return_origins: selection.is_enabled("MUT112"),
        iterations: selection.is_enabled("MUT113"),
        locks: selection.is_enabled("EFF109"),
        set_order: selection.is_enabled("EFF112"),
        exclusive: selection.is_enabled("EFF110"),
        // `EFF114` reads which project functions never return normally
        // (`MustExec::never_returns`).
        must_effects: crate::eff_rules::MUST_EFFECT_RULES
            .iter()
            .chain(&["EFF114"])
            .any(|code| selection.is_enabled(code)),
        call_cycles: selection.is_enabled("EFF114"),
        shared_state: false,
    }
}

/// [`opt_in_facts`] with the facts `heki inspect` reads besides the rules: the shared state
/// functions change, and what they return (`returns.origins`). `heki check` computes them only
/// for the rules it runs.
pub fn inspect_facts(selection: &RuleSelection) -> OptInFacts {
    OptInFacts {
        return_origins: true,
        shared_state: true,
        ..opt_in_facts(selection)
    }
}

/// The roles `key` has in the declared contracts, in configuration order, and in the ecosystem
/// catalog: the matchers the rules use, compiled as they compile them.
pub(crate) fn roles_of(
    cx: &CheckContext<'_>,
    key: &FuncKey,
) -> Result<Vec<ContractRole>, RuleConfigError> {
    let config = cx.config;
    let mut out = Vec::new();
    let mut add = |kind: &'static str, name: &str, roles: Vec<&'static str>| {
        if !roles.is_empty() {
            out.push(ContractRole {
                kind,
                name: name.to_owned(),
                roles,
            });
        }
    };
    let owner_roles = |a: &Allowance, allowed: &'static str, forbidden: &'static str| {
        let mut roles = Vec::new();
        if a.names_allowed(key) {
            roles.push(allowed);
        }
        if a.exempts(key) {
            roles.push("exempt-method");
        }
        if a.has_forbidden() && a.in_scope(key) {
            roles.push(forbidden);
        }
        roles
    };
    for rule in &config.write_owners {
        let a = Allowance::new(&AllowanceKeys {
            allowed: (&rule.allowed_writers, "write-owners.allowed-writers"),
            forbidden: (&rule.forbidden_writers, "write-owners.forbidden-writers"),
            exempt: (&rule.exempt_methods, "write-owners.exempt-methods"),
        })?;
        add(
            "write-owners",
            &rule.name,
            owner_roles(&a, "allowed-writer", "forbidden-writer"),
        );
    }
    for rule in &config.read_owners {
        let a = Allowance::new(&AllowanceKeys {
            allowed: (&rule.allowed_readers, "read-owners.allowed-readers"),
            forbidden: (&rule.forbidden_readers, "read-owners.forbidden-readers"),
            exempt: (&rule.exempt_methods, "read-owners.exempt-methods"),
        })?;
        add(
            "read-owners",
            &rule.name,
            owner_roles(&a, "allowed-reader", "forbidden-reader"),
        );
    }
    let all_classes = crate::mut_rules::project_classes(cx);
    for rule in &config.call_owners {
        let a = Allowance::new(&AllowanceKeys {
            allowed: (&rule.allowed_callers, "call-owners.allowed-callers"),
            forbidden: (&rule.forbidden_callers, "call-owners.forbidden-callers"),
            exempt: (&rule.exempt_methods, "call-owners.exempt-methods"),
        })?;
        let mut roles = Vec::new();
        for t in &rule.targets {
            if FunctionMatcher::new(cx, &all_classes, t, "call-owners.targets")?.matches(key) {
                roles.push("call-target");
                break;
            }
        }
        roles.extend(owner_roles(&a, "allowed-caller", "forbidden-caller"));
        add("call-owners", &rule.name, roles);
    }
    for rule in &config.effect_boundaries {
        let scopes = patterns(&rule.scopes, "effect-boundaries.scopes")?;
        let callees = patterns(&rule.allowed_callees, "effect-boundaries.allowed-callees")?;
        let mut roles = Vec::new();
        if key.qualname != "<module>" && crate::eff_rules::names_function(&scopes, key) {
            roles.push("boundary-scope");
        }
        if crate::eff_rules::names_function(&callees, key) {
            roles.push("allowed-callee");
        }
        add("effect-boundaries", &rule.name, roles);
    }
    let fqn = key.fqn();
    for rule in &config.readonly_returns {
        let producers = patterns(&rule.producers, "readonly-returns.producers")?;
        let roles = if producers.iter().any(|p| p.matches(&fqn)) {
            vec!["read-only-producer"]
        } else {
            Vec::new()
        };
        add("readonly-returns", &rule.name, roles);
    }
    let compiled = ConfigContracts::new(config, cx.kb)?;
    for (rule, names) in config.invalidation.iter().zip(&compiled.invalidations) {
        let mut roles = Vec::new();
        if names.names_invalidator(&fqn, &key.qualname) {
            roles.push("invalidator");
        }
        if names.names_exempt_writer(&fqn, &key.qualname) {
            roles.push("exempt-writer");
        }
        add("invalidation", &rule.name, roles);
    }
    if cx.kb.lookup(&fqn).is_some() {
        add("ecosystem", &fqn, vec!["catalog-entry"]);
    }
    Ok(out)
}
