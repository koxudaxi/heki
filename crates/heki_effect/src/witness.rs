//! The chain a report shows for an execution effect, its witness ([`EffectSummary::exec_chains`],
//! [`EffectSummary::witness_ways`]): among the ways a function reaches the effect, one the
//! call's arguments do not rule out, then one on the normal path before one through cleanup
//! code, then the shortest, the first found among equals. Every way stays a path the code has;
//! the choice never decides whether the effect arrives.

use crate::guards::{NeedAtCall, need_at_call};
use crate::{
    BoundCallee, CallHop, CallSite, EffectSummary, GuardTest, InputGuard, PROVIDED_CHAIN_LIMIT,
    SharedChain, WITNESS_LIMIT, WitnessWay, WitnessWays,
};
use heki_kb::ExecEffectKind;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The tests among `guards` a way is behind: `isinstance` tests of parameters themselves.
pub(crate) fn needs_of<'g>(guards: impl IntoIterator<Item = &'g InputGuard>) -> Vec<InputGuard> {
    let mut out: Vec<InputGuard> = guards
        .into_iter()
        .filter(|g| g.subject.attr.is_none() && matches!(g.test, GuardTest::IsInstance(_)))
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The order of ways whose tests a call leaves open: the normal path first, then the shorter.
fn rank(cleanup: bool, chain: &[Arc<CallHop>]) -> (bool, usize) {
    (cleanup, chain.len())
}

/// Whether way `a` leaves no room for way `b`: wherever `b` is taken `a` is too (its tests are
/// among `b`'s), and its chain is no worse. Of two equal ways the one kept is `a` unless
/// `b_wins_ties`.
fn dominates(a: &WitnessWay, b: &WitnessWay, b_wins_ties: bool) -> bool {
    let (ra, rb) = (rank(a.cleanup, &a.chain), rank(b.cleanup, &b.chain));
    a.needs.iter().all(|n| b.needs.contains(n))
        && (ra < rb || (ra == rb && (!b_wins_ties || a.needs.len() < b.needs.len())))
}

/// The ways to `e` of a summary with the kept chains `chains` and the ways `ways`.
fn ways_in(
    chains: &BTreeMap<ExecEffectKind, SharedChain>,
    ways: Option<&WitnessWays>,
    e: ExecEffectKind,
) -> Vec<WitnessWay> {
    if let Some(ways) = ways.and_then(|w| w.get(&e)) {
        return ways.clone();
    }
    (chains.get(&e).filter(|c| !c.is_empty()))
        .map(|chain| WitnessWay {
            needs: Vec::new(),
            cleanup: false,
            chain: chain.clone(),
        })
        .into_iter()
        .collect()
}

impl EffectSummary {
    /// The ways to `e`: those of [`Self::witness_ways`], or the kept chain alone.
    fn ways_to(&self, e: ExecEffectKind) -> Vec<WitnessWay> {
        ways_in(&self.exec_chains, self.witness_ways.as_deref(), e)
    }

    /// Record a way to `e` behind the tests `needs`, through cleanup code when `cleanup`, and
    /// keep [`Self::exec_chains`] the best way. `later_wins_ties`: a way equal to a kept one
    /// replaces it (a function's own calls of the catalog show the last one found).
    pub(crate) fn add_way(
        &mut self,
        e: ExecEffectKind,
        needs: Vec<InputGuard>,
        cleanup: bool,
        chain: SharedChain,
        later_wins_ties: bool,
    ) {
        // Each way keeps a chain of its own: down a long chain of helpers handing a parameter
        // on, the tests are dropped and the ways fold into one.
        let needs = if chain.len() > PROVIDED_CHAIN_LIMIT {
            Vec::new()
        } else {
            needs
        };
        let new = WitnessWay {
            needs,
            cleanup,
            chain,
        };
        if self
            .witness_ways
            .as_ref()
            .is_none_or(|w| !w.contains_key(&e))
        {
            // The kept chain is the one way, behind no test and on the normal path.
            let plain = new.needs.is_empty() && !new.cleanup;
            let m = new.chain.len();
            let beats = (self.exec_chains.get(&e).filter(|c| !c.is_empty())).is_none_or(|kept| {
                !new.cleanup
                    && (m < kept.len()
                        || (m == kept.len() && later_wins_ties && new.needs.is_empty()))
            });
            if !beats {
                return;
            }
            if plain {
                self.exec_chains.insert(e, new.chain);
                return;
            }
        }
        let mut ways = self.ways_to(e);
        if !ways.iter().any(|w| dominates(w, &new, later_wins_ties)) {
            ways.retain(|w| !dominates(&new, w, false));
            ways.push(new);
        }
        while ways.len() > WITNESS_LIMIT {
            let worst = (ways.iter().enumerate())
                .filter(|(_, w)| !w.needs.is_empty())
                .max_by_key(|(_, w)| (rank(w.cleanup, &w.chain), w.needs.len()))
                .map(|(i, _)| i)
                .expect("past the limit some way has tests: one way has none");
            ways.remove(worst);
        }
        let best = (ways.iter())
            .min_by_key(|w| rank(w.cleanup, &w.chain))
            .expect("a way was just kept");
        self.exec_chains.insert(e, best.chain.clone());
        let plain = matches!(ways.as_slice(), [w] if w.needs.is_empty() && !w.cleanup);
        if plain {
            self.forget_ways(e);
        } else {
            self.witness_ways
                .get_or_insert_with(Box::default)
                .insert(e, ways);
        }
    }

    /// Record every way of another summary (another definition, or a body run with this one)
    /// to each effect, from its kept chains and ways.
    pub(crate) fn add_ways_of(
        &mut self,
        chains: &BTreeMap<ExecEffectKind, SharedChain>,
        ways: Option<&WitnessWays>,
    ) {
        for &e in chains.keys() {
            for w in ways_in(chains, ways, e) {
                self.add_way(e, w.needs, w.cleanup, w.chain, false);
            }
        }
    }

    /// Drop the ways to `e`: another arrival replaces them.
    pub(crate) fn forget_ways(&mut self, e: ExecEffectKind) {
        if let Some(w) = &mut self.witness_ways {
            w.remove(&e);
            if w.is_empty() {
                self.witness_ways = None;
            }
        }
    }
}

/// A callee's way to an effect as a call takes it ([`ways_at_call`]).
pub(crate) struct WayAtCall<'f> {
    /// The tests of the caller's parameters it is behind.
    pub needs: Vec<InputGuard>,
    pub cleanup: bool,
    /// The callee's chain.
    pub tail: &'f [Arc<CallHop>],
}

/// The ways of `facts` (a callee's) to `e` the call at `site` binding `bc` can take, each with
/// its tests in the caller's terms and the call's own tests, and through cleanup code when the
/// way or the call is. A way an argument decides a test against is left out; when every way
/// is, the kept chain stands (a bound dropped the way the call takes). `tail` is the chain the
/// effect arrives by ([`crate::callgraph::arrival`]).
pub(crate) fn ways_at_call<'f>(
    facts: &'f EffectSummary,
    e: ExecEffectKind,
    site: &CallSite,
    bc: &BoundCallee,
    tail: &'f [Arc<CallHop>],
) -> Vec<WayAtCall<'f>> {
    let own = needs_of(&site.guards);
    let through_subclasses = (facts.provided_effects.as_ref()).is_some_and(|p| p.contains_key(&e));
    let ways = match facts.witness_ways.as_ref().and_then(|w| w.get(&e)) {
        Some(ways) if !through_subclasses => ways,
        _ => {
            return vec![WayAtCall {
                needs: own,
                cleanup: site.cleanup,
                tail,
            }];
        }
    };
    let mut out = Vec::new();
    'ways: for w in ways {
        let mut needs = own.clone();
        for n in &w.needs {
            match need_at_call(n, site, bc) {
                NeedAtCall::Fails => continue 'ways,
                NeedAtCall::Caller(t) => needs.push(t),
                NeedAtCall::Open => {}
            }
        }
        needs.sort();
        needs.dedup();
        out.push(WayAtCall {
            needs,
            cleanup: w.cleanup || site.cleanup,
            tail: &w.chain,
        });
    }
    if out.is_empty() {
        out.push(WayAtCall {
            needs: own,
            cleanup: site.cleanup,
            tail,
        });
    }
    out
}

/// The chain of `facts` (a callee's) to `e` the call at `site` binding `bc` shows: the best
/// way its arguments do not rule out ([`ways_at_call`]), else the kept chain.
pub(crate) fn chosen<'f>(
    facts: &'f EffectSummary,
    e: ExecEffectKind,
    site: &CallSite,
    bc: &BoundCallee,
) -> &'f [Arc<CallHop>] {
    let kept = facts.exec_chains.get(&e).map_or(&[][..], Vec::as_slice);
    if facts
        .witness_ways
        .as_ref()
        .is_none_or(|w| !w.contains_key(&e))
    {
        return kept;
    }
    ways_at_call(facts, e, site, bc, kept)
        .into_iter()
        .min_by_key(|w| rank(w.cleanup, w.tail))
        .map_or(kept, |w| w.tail)
}
