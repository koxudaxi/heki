//! The one import whose removal takes away a finding of the import graph: a site on every cycle
//! of a group ([`ImportGraph::cycle_cuts`]), or on every chain from some start modules to a
//! module ([`path_cuts`]). A rule names such a site in its help only once one of these searches
//! has shown that the graph without the site's hops keeps no cycle in the group, or no chain to
//! the module. Each is one walk over the graph without the sites of the cycle or the chain.

use super::{ImportGraph, ImportHop, ImportSite};
use crate::scc;
use heki_ir::walk::{Answer, Budget, Closure, Join, KeySet};
use rustc_hash::{FxHashMap, FxHashSet};
use std::hash::Hash;
use std::rc::Rc;

/// A step of a chain: from a node, at one of its sites, into the next node.
pub type ChainStep<K> = (K, usize, K);

/// The furthest node of a chain a node reaches, by its place on the chain.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Furthest(Option<usize>);

impl Join for Furthest {
    fn join(&mut self, other: &Self) -> bool {
        let grew = other.0 > self.0;
        if grew {
            self.0 = other.0;
        }
        grew
    }
}

/// The steps of `chain` whose site carries every chain of `steps` from `starts` to the node
/// `chain` ends at, by index into `chain`, ascending: once that site's steps are gone, no start
/// reaches the node, nor any node of `chain` after the step. `chain` leaves one of `starts`, and
/// `steps(node, out)` lists each step from a node with the site it is at.
///
/// Without the site of step `i`, the starts still reach the nodes of `chain` up to the one
/// step `i` leaves, and the nodes the steps before it enter. Each of those reaches, over the
/// steps at no site of `chain`, a furthest node of `chain`; step `i` is a cut when none is past
/// the node it leaves (from a node past it, `chain` goes on to its end). One [`Closure`] over the
/// steps at no site of `chain` joins those furthest places; open once `budget` is spent.
pub fn path_cuts<K: Copy + Eq + Hash>(
    budget: &Budget,
    starts: impl IntoIterator<Item = K>,
    chain: &[ChainStep<K>],
    steps: impl Fn(K, &mut Vec<(usize, K)>),
) -> Answer<Vec<usize>> {
    let place: FxHashMap<K, usize> = (chain.first().map(|&(k, _, _)| k).into_iter())
        .chain(chain.iter().map(|&(_, _, to)| to))
        .enumerate()
        .map(|(i, k)| (k, i))
        .collect();
    let on_chain: FxHashSet<(K, usize)> = chain.iter().map(|&(k, site, _)| (k, site)).collect();
    let listed = |k: K| {
        let mut out = Vec::new();
        steps(k, &mut out);
        out
    };
    let closure: Closure<K, Furthest> = Closure::new();
    // The furthest place `reached` and what `k` reaches.
    let join = |reached: Answer<Furthest>, k: K| {
        let furthest = closure.ask(
            budget,
            k,
            |k| Furthest(place.get(&k).copied()),
            |k, out| {
                out.extend(
                    (listed(k).into_iter())
                        .filter(|&(site, _)| !on_chain.contains(&(k, site)))
                        .map(|(_, to)| to),
                );
            },
        );
        reached.join_with(furthest.map(|f| *f), |mut a, b| {
            a.join(&b);
            a
        })
    };
    let mut reached = starts
        .into_iter()
        .fold(Answer::Known(Furthest::default()), &join);
    let mut cuts = Vec::new();
    for (i, &(from, _, _)) in chain.iter().enumerate() {
        // The nodes the step before this one enters.
        let entered =
            (i.checked_sub(1).map(|b| chain[b]).into_iter()).flat_map(|(before, site, _)| {
                (listed(before).into_iter())
                    .filter(move |&(s, _)| s == site)
                    .map(|(_, to)| to)
            });
        reached = std::iter::once(from).chain(entered).fold(reached, &join);
        if reached == Answer::Known(Furthest(Some(i))) {
            cuts.push(i);
        }
    }
    reached.map(|_| cuts)
}

impl ImportGraph<'_> {
    /// The hops of `cycle`, a cycle through the group `members` (ascending) of the sites `counts`
    /// takes, whose site is on every cycle of the group: once that site's hops are gone, no
    /// modules of the group import each other in a cycle. By index into `cycle`, ascending; open
    /// once `budget` is spent.
    ///
    /// The other hops of the group, those of no site of `cycle`, must form no cycle, which would
    /// keep every site. Then every cycle of the group takes hops of `cycle` with paths of the
    /// other hops between them. Hop `k` leads, through its site and the other hops, to the
    /// modules some hops of `cycle` leave; with `l` the one of them nearest behind `k` (`k`
    /// itself, then `k - 1`, and on around), the hops from `l` to `k` close a cycle that keeps
    /// every other site. A cut is a hop each of these ranges holds: no cycle is left without it.
    /// One walk over the other hops, a [`Closure`] of the hops of `cycle` each module leads to.
    pub fn cycle_cuts(
        &self,
        members: &[u32],
        cycle: &[ImportHop],
        counts: impl Fn(&ImportSite) -> bool,
        budget: &Budget,
    ) -> Answer<Vec<usize>> {
        let member = |m: u32| members.binary_search(&m).ok();
        let on_cycle: FxHashSet<(u32, usize)> = cycle.iter().map(|h| (h.from, h.site)).collect();
        let into_group = |m: u32, j: usize, out: &mut Vec<u32>| {
            out.extend(
                (self.hops(m, j).into_iter())
                    .map(|hop| hop.to)
                    .filter(|&w| member(w).is_some()),
            );
        };
        let others = |m: u32, out: &mut Vec<u32>| {
            for (j, (site, _)) in self.sites(m).into_iter().flatten().enumerate() {
                if counts(site) && !on_cycle.contains(&(m, j)) {
                    into_group(m, j, out);
                }
            }
        };
        // Linear in the sites and hops of the group, and unmetered, like the search for groups.
        let successors: Vec<Vec<usize>> = (members.iter())
            .map(|&m| {
                let mut next = Vec::new();
                others(m, &mut next);
                next.into_iter().filter_map(member).collect()
            })
            .collect();
        if (scc::tarjan_scc(members.len(), &successors).iter()).any(|c| c.len() > 1) {
            return Answer::Known(Vec::new());
        }
        let leaving: FxHashMap<u32, usize> = (cycle.iter().enumerate())
            .map(|(k, hop)| (hop.from, k))
            .collect();
        let leaves = |m: u32| {
            let mut leaves = KeySet::new();
            if let Some(&l) = leaving.get(&m) {
                leaves.insert(l);
            }
            leaves
        };
        let closure: Closure<u32, KeySet<usize>> = Closure::new();
        let n = cycle.len();
        let behind: Answer<Vec<usize>> = (cycle.iter().enumerate())
            .map(|(k, hop)| {
                let mut heads = Vec::new();
                into_group(hop.from, hop.site, &mut heads);
                let led: Answer<Vec<Rc<KeySet<usize>>>> = (heads.into_iter())
                    .map(|head| closure.ask(budget, head, &leaves, &others))
                    .collect();
                // The next hop leaves the module this one enters: `n - 1` behind it.
                led.map(|led| {
                    (led.iter().flat_map(|l| l.iter()))
                        .map(|&l| (k + n - l) % n)
                        .fold(n - 1, usize::min)
                })
            })
            .collect();
        behind.map(|behind| {
            // Per hop, how many ranges hold it, as differences from the hop before.
            let mut held = vec![0i64; n + 1];
            for (k, behind) in behind.into_iter().enumerate() {
                let from = (k + n - behind) % n;
                held[from] += 1;
                held[k + 1] -= 1;
                if from > k {
                    held[n] -= 1;
                    held[0] += 1;
                }
            }
            let mut holding = 0;
            let mut cuts = Vec::new();
            for (i, change) in held[..n].iter().enumerate() {
                holding += change;
                if holding == n as i64 {
                    cuts.push(i);
                }
            }
            cuts
        })
    }
}
