//! What heki cannot see in a project, counted from the calls the index records it is blind to
//! ([`heki_effect::BlindCall`], extracted with [`crate::recommend_facts`]): the calls of library
//! names the ecosystem catalog has no entry for, by package and name, and the calls heki does not
//! resolve, by function and reason. Each count also says how many of the calls are awaited, how
//! many may hold up an event loop, and how many run in or below a loop
//! ([`ProjectIndex::blind_spot_reach`]).

use heki_ast::Span;
use heki_effect::{Blind, BlindCall, BlindSpotReach, FuncKey, ProjectIndex, UnresolvedReason};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Where one blind call is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlindSite<'i> {
    pub path: &'i Path,
    pub function: &'i FuncKey,
    pub span: Span,
}

/// How many calls there are, and what they may hold up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct CallCounts {
    pub calls: usize,
    /// The calls that are awaited: whatever they run, they do not hold up the event loop that
    /// awaits them.
    pub awaited: usize,
    /// The calls, not awaited, in a function on an event loop
    /// ([`BlindSpotReach::on_event_loop`]): one that waits holds the loop up.
    pub on_event_loop: usize,
    /// The calls inside a loop of their function, or in a function below a loop
    /// ([`BlindSpotReach::below_loop`]).
    pub in_loops: usize,
}

impl CallCounts {
    fn add(&mut self, other: Self) {
        self.calls += other.calls;
        self.awaited += other.awaited;
        self.on_event_loop += other.on_event_loop;
        self.in_loops += other.in_loops;
    }
}

/// The calls of one library name the catalog has no entry for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncataloguedSymbol<'i> {
    /// Spelled as an ecosystem entry needs it.
    pub symbol: &'i str,
    pub counts: CallCounts,
    pub sites: Vec<BlindSite<'i>>,
}

/// The calls of the names of one package the catalog has no entry for, by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncataloguedPackage<'i> {
    pub package: &'i str,
    /// The catalog lists the package: it lacks entries for these names only.
    pub registered: bool,
    pub counts: CallCounts,
    pub symbols: Vec<UncataloguedSymbol<'i>>,
}

/// The calls one function makes that heki does not resolve, for one reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedCalls<'i> {
    pub function: &'i FuncKey,
    pub path: &'i Path,
    pub reason: &'i UnresolvedReason,
    /// The methods and names called, each once, sorted.
    pub names: Vec<&'i str>,
    pub counts: CallCounts,
    pub sites: Vec<BlindSite<'i>>,
}

/// Per project: the calls into packages without catalog entries, by package, then name; the
/// calls heki does not resolve, by file, function, and reason.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlindSpots<'i> {
    pub uncatalogued: Vec<UncataloguedPackage<'i>>,
    pub unresolved: Vec<UnresolvedCalls<'i>>,
}

/// The totals of [`BlindSpots`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlindSpotTotals {
    pub uncatalogued: CallCounts,
    pub uncatalogued_packages: usize,
    pub uncatalogued_symbols: usize,
    /// The calls of names of packages the catalog lists.
    pub registered_calls: usize,
    pub unresolved: CallCounts,
    /// The unresolved calls by reason ([`UnresolvedReason::as_str`]).
    pub unresolved_by_reason: BTreeMap<&'static str, usize>,
    /// The functions making a blind call.
    pub functions: usize,
}

impl<'i> BlindSpots<'i> {
    /// The blind calls of every function of `index`, in file, function, and source order.
    pub fn of(index: &'i ProjectIndex) -> Self {
        let reach = index.blind_spot_reach();
        let mut files: Vec<_> = index.files.values().collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let mut packages: BTreeMap<&str, (bool, BTreeMap<&str, UncataloguedSymbol<'_>>)> =
            BTreeMap::new();
        let mut unresolved: Vec<UnresolvedCalls<'_>> = Vec::new();
        for file in files {
            let mut functions: Vec<_> = file.blind_calls().collect();
            functions.sort_by(|a, b| a.0.cmp(b.0));
            for (function, calls) in functions {
                let mut by_reason: BTreeMap<&UnresolvedReason, UnresolvedCalls<'_>> =
                    BTreeMap::new();
                for call in calls {
                    let site = BlindSite {
                        path: &file.path,
                        function,
                        span: call.span,
                    };
                    let counts = counts_of(call, function, &reach);
                    match &call.blind {
                        Blind::Uncatalogued { symbol, registered } => {
                            let package = symbol.split('.').next().unwrap_or(symbol);
                            let entry = packages
                                .entry(package)
                                .or_insert((*registered, BTreeMap::new()));
                            let calls = entry.1.entry(symbol).or_insert(UncataloguedSymbol {
                                symbol,
                                counts: CallCounts::default(),
                                sites: Vec::new(),
                            });
                            calls.counts.add(counts);
                            calls.sites.push(site);
                        }
                        Blind::Unresolved { name, reason } => {
                            let group = by_reason.entry(reason).or_insert(UnresolvedCalls {
                                function,
                                path: &file.path,
                                reason,
                                names: Vec::new(),
                                counts: CallCounts::default(),
                                sites: Vec::new(),
                            });
                            group.names.extend(name.as_deref());
                            group.counts.add(counts);
                            group.sites.push(site);
                        }
                    }
                }
                for mut group in by_reason.into_values() {
                    group.names.sort_unstable();
                    group.names.dedup();
                    unresolved.push(group);
                }
            }
        }
        let uncatalogued = packages
            .into_iter()
            .map(|(package, (registered, symbols))| {
                let symbols: Vec<_> = symbols.into_values().collect();
                let mut counts = CallCounts::default();
                for s in &symbols {
                    counts.add(s.counts);
                }
                UncataloguedPackage {
                    package,
                    registered,
                    counts,
                    symbols,
                }
            })
            .collect();
        Self {
            uncatalogued,
            unresolved,
        }
    }

    /// The counts over the whole project.
    pub fn totals(&self) -> BlindSpotTotals {
        let mut totals = BlindSpotTotals::default();
        let mut functions: Vec<&FuncKey> = Vec::new();
        for package in &self.uncatalogued {
            totals.uncatalogued.add(package.counts);
            totals.uncatalogued_packages += 1;
            totals.uncatalogued_symbols += package.symbols.len();
            if package.registered {
                totals.registered_calls += package.counts.calls;
            }
            let sites = package.symbols.iter().flat_map(|s| &s.sites);
            functions.extend(sites.map(|s| s.function));
        }
        for group in &self.unresolved {
            totals.unresolved.add(group.counts);
            *totals
                .unresolved_by_reason
                .entry(group.reason.as_str())
                .or_default() += group.counts.calls;
            functions.push(group.function);
        }
        functions.sort_unstable();
        functions.dedup();
        totals.functions = functions.len();
        totals
    }
}

/// What the call `call` of `function` counts for.
fn counts_of(call: &BlindCall, function: &FuncKey, reach: &BlindSpotReach) -> CallCounts {
    CallCounts {
        calls: 1,
        awaited: usize::from(call.is_awaited),
        on_event_loop: usize::from(!call.is_awaited && reach.on_event_loop.contains(function)),
        in_loops: usize::from(call.loop_depth > 0 || reach.below_loop.contains(function)),
    }
}
