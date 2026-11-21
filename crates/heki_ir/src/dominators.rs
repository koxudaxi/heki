//! Dominance in a function's control flow, over its normal and exception edges: the immediate
//! dominators, and the dominator tree numbered in preorder so that the blocks a block dominates
//! are one range of numbers.

use crate::{BlockId, IrFunction};

/// The immediate dominator of each block the entry reaches over the edges `takes` admits
/// (`None` for the other blocks, the entry for itself), from one pass in reverse postorder. One
/// pass is exact: in the reducible graphs Python's statements make, an edge to an earlier block
/// goes back to a loop header that dominates the edge's source, and leaving such an edge out
/// changes no dominator.
pub fn immediate_dominators(
    f: &IrFunction,
    preds: &[Vec<BlockId>],
    takes: impl Fn(BlockId, BlockId) -> bool,
) -> Vec<Option<BlockId>> {
    let rpo = f.rpo_blocks();
    let mut order = vec![usize::MAX; f.blocks.len()];
    for (i, b) in rpo.iter().enumerate() {
        order[b.0 as usize] = i;
    }
    let mut idom: Vec<Option<BlockId>> = vec![None; f.blocks.len()];
    idom[rpo[0].0 as usize] = Some(rpo[0]);
    let up = |b: BlockId, idom: &[Option<BlockId>]| {
        idom[b.0 as usize].expect("a block before in reverse postorder has a dominator")
    };
    for &b in &rpo[1..] {
        let mut found: Option<BlockId> = None;
        for &p in &preds[b.0 as usize] {
            if idom[p.0 as usize].is_none() || !takes(p, b) {
                continue;
            }
            found = Some(match found {
                None => p,
                Some(mut d) => {
                    let mut q = p;
                    while d != q {
                        while order[d.0 as usize] > order[q.0 as usize] {
                            d = up(d, &idom);
                        }
                        while order[q.0 as usize] > order[d.0 as usize] {
                            q = up(q, &idom);
                        }
                    }
                    d
                }
            });
        }
        idom[b.0 as usize] = found;
    }
    idom
}

/// The dominator tree of a function over all its edges, normal and exception ones, numbered
/// in preorder: the blocks a block dominates are the numbers from its own to the end of its
/// subtree. A block the entry does not reach dominates nothing and has no dominator.
pub struct DominatorTree {
    idom: Vec<Option<BlockId>>,
    /// Per block, its number and the end of its subtree (exclusive).
    subtree: Vec<Option<(u32, u32)>>,
}

/// The blocks a set of blocks dominates, as the sorted, disjoint ranges of their numbers in a
/// [`DominatorTree`].
pub struct Dominated(Vec<(u32, u32)>);

impl DominatorTree {
    pub fn of(f: &IrFunction) -> Self {
        let idom = immediate_dominators(f, &f.predecessors(), |_, _| true);
        let mut children = vec![Vec::new(); f.blocks.len()];
        for (b, d) in idom.iter().enumerate() {
            if let Some(d) = d.filter(|d| d.0 as usize != b) {
                children[d.0 as usize].push(BlockId(b as u32));
            }
        }
        let mut subtree = vec![None; f.blocks.len()];
        let mut next = 0u32;
        // (block, whether its subtree is numbered)
        let mut stack = vec![(f.blocks[0].id, false)];
        while let Some((b, done)) = stack.pop() {
            let slot = &mut subtree[b.0 as usize];
            if done {
                *slot = slot.map(|(start, _)| (start, next));
                continue;
            }
            *slot = Some((next, next));
            next += 1;
            stack.push((b, true));
            stack.extend(children[b.0 as usize].iter().map(|c| (*c, false)));
        }
        Self { idom, subtree }
    }

    /// Whether every path from the entry to `b` runs through `d` (`b` itself included): `false`
    /// when the entry does not reach one of them.
    pub fn dominates(&self, d: BlockId, b: BlockId) -> bool {
        match (self.subtree[d.0 as usize], self.subtree[b.0 as usize]) {
            (Some((start, end)), Some((at, _))) => start <= at && at < end,
            _ => false,
        }
    }

    /// The nearest block other than `b` that dominates it.
    pub fn strict_dominator(&self, b: BlockId) -> Option<BlockId> {
        self.idom[b.0 as usize].filter(|d| *d != b)
    }

    /// The blocks that one of `blocks` dominates (each of `blocks` included).
    pub fn dominated_by(&self, blocks: impl IntoIterator<Item = BlockId>) -> Dominated {
        let mut ranges: Vec<(u32, u32)> = blocks
            .into_iter()
            .filter_map(|b| self.subtree[b.0 as usize])
            .collect();
        ranges.sort_unstable();
        // Two subtrees are disjoint or one holds the other: keep the outermost ones.
        let mut outer: Vec<(u32, u32)> = Vec::with_capacity(ranges.len());
        for range in ranges {
            if outer.last().is_none_or(|last| range.0 >= last.1) {
                outer.push(range);
            }
        }
        Dominated(outer)
    }

    /// Whether `b` is among the blocks of `set`.
    pub fn contains(&self, set: &Dominated, b: BlockId) -> bool {
        self.subtree[b.0 as usize].is_some_and(|(at, _)| {
            let i = set.0.partition_point(|range| range.0 <= at);
            i > 0 && at < set.0[i - 1].1
        })
    }
}
