//! Backward liveness over the SSA-CFG (docs/ANALYSIS_AND_KB_SPEC.md section 3.3).
//!
//! A handler (`exc_target`) is entered from an instruction of the block that may raise or
//! from its `raise`, so what is live at the handler is live before each of those
//! ([`heki_ir::InstKind::may_raise`]), not after the last one.
//! A phi's incoming value is used at the end of its predecessor, not at the phi's block.

use crate::interpreter::term_use;
use heki_ir::walk::{Answer, Budget, Facts, FnShape, Join, Sparse, fixpoint};
use heki_ir::{BlockId, IrFunction, VarId};

/// Dense bit set over `VarId`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VarSet(Vec<u64>);

impl VarSet {
    pub fn new(nvars: usize) -> Self {
        Self(vec![0; nvars.div_ceil(64)])
    }

    pub fn insert(&mut self, v: VarId) {
        let i = v.0 as usize;
        if let Some(w) = self.0.get_mut(i / 64) {
            *w |= 1 << (i % 64);
        }
    }

    pub fn remove(&mut self, v: VarId) {
        let i = v.0 as usize;
        if let Some(w) = self.0.get_mut(i / 64) {
            *w &= !(1 << (i % 64));
        }
    }

    pub fn contains(&self, v: VarId) -> bool {
        let i = v.0 as usize;
        self.0.get(i / 64).is_some_and(|w| w & (1 << (i % 64)) != 0)
    }

    fn union_with(&mut self, other: &Self) -> bool {
        let mut changed = false;
        for (a, b) in self.0.iter_mut().zip(&other.0) {
            let n = *a | *b;
            changed |= n != *a;
            *a = n;
        }
        changed
    }
}

impl Join for VarSet {
    fn join(&mut self, other: &Self) -> bool {
        self.union_with(other)
    }

    fn weight(&self) -> usize {
        self.0.len().max(1)
    }
}

pub(crate) struct Liveness {
    /// Live at the end of each block, on its normal-flow edges.
    live_out: Vec<VarSet>,
    /// Live where each block may raise into its handler.
    live_at_raise: Vec<VarSet>,
}

impl Liveness {
    /// Open past the budget of the function.
    pub fn compute(shape: &FnShape<'_>) -> Answer<Self> {
        let f = shape.f;
        let nvars = f.var_names.len();
        let n = f.blocks.len();
        // The blocks the entry reaches, in postorder, so that a block is first visited after
        // its successors.
        let order: Vec<BlockId> = shape.rpo().iter().rev().copied().collect();
        let node = |b: BlockId| shape.rpo_index(b).map(|i| order.len() - 1 - i as usize);
        let entering = |live_in: &Facts<VarSet>, b: BlockId, s: BlockId| {
            let mut live = (node(s).and_then(|i| live_in.get(i)).cloned())
                .unwrap_or_else(|| VarSet::new(nvars));
            for phi in &f.block(s).phis {
                for &(p, v) in &phi.incoming {
                    if p == b {
                        live.insert(v);
                    }
                }
            }
            live
        };
        // What is live at the end of `b` on its normal edges, and where it may raise.
        let exits = |live_in: &Facts<VarSet>, b: BlockId| {
            let bb = f.block(b);
            let mut out = VarSet::new(nvars);
            for s in bb.terminator.successors() {
                out.union_with(&entering(live_in, b, s));
            }
            let at_raise = bb
                .exc_target
                .map_or_else(|| VarSet::new(nvars), |e| entering(live_in, b, e));
            (out, at_raise)
        };
        let transfer = |i: usize, live_in: &Facts<VarSet>| {
            let bb = f.block(order[i]);
            let (out, at_raise) = exits(live_in, order[i]);
            let inn = Self::backward(bb, out, &at_raise, bb.insts.len());
            Some(bb.phis.iter().fold(inn, |mut live, phi| {
                live.remove(phi.dst);
                live
            }))
        };
        let readers = |i: usize, out: &mut Vec<usize>| {
            out.extend(shape.preds(order[i]).iter().filter_map(|p| node(*p)));
        };
        // A block is visited once, then once per variable a successor's set gains.
        let spec = Sparse {
            nodes: order.len(),
            readers: &readers,
            max_visits: u32::try_from(1 + 3 * (1 + nvars)).unwrap_or(u32::MAX),
        };
        let budget = Budget::of_function("soundness::Liveness", f);
        fixpoint(&budget, spec, transfer).map(|live_in| {
            let mut live_out = vec![VarSet::new(nvars); n];
            let mut live_at_raise = vec![VarSet::new(nvars); n];
            for &b in &order {
                let (out, at_raise) = exits(&live_in, b);
                live_out[b.0 as usize] = out;
                live_at_raise[b.0 as usize] = at_raise;
            }
            Self {
                live_out,
                live_at_raise,
            }
        })
    }

    /// Walk the `last` instructions of `bb` backward from its end: `live` is what its normal
    /// successors need, `at_raise` what its handler needs where it may raise.
    fn backward(
        bb: &heki_ir::BasicBlock,
        mut live: VarSet,
        at_raise: &VarSet,
        last: usize,
    ) -> VarSet {
        if let Some(v) = term_use(&bb.terminator) {
            live.insert(v);
        }
        if matches!(bb.terminator, heki_ir::Terminator::Raise(_)) {
            live.union_with(at_raise);
        }
        for inst in bb.insts.iter().rev().take(last) {
            if let Some(d) = inst.kind.dst() {
                live.remove(d);
            }
            for op in inst.kind.operands() {
                live.insert(op);
            }
            if inst.kind.may_raise() {
                live.union_with(at_raise);
            }
        }
        live
    }

    /// Variables live immediately after instruction `idx` of block `b`.
    pub fn live_after(&self, f: &IrFunction, b: BlockId, idx: usize) -> VarSet {
        let bi = b.0 as usize;
        let bb = f.block(b);
        // The instructions after `idx` run from the end of the block back to `idx + 1`.
        let after = bb.insts.len().saturating_sub(idx + 1);
        Self::backward(
            bb,
            self.live_out[bi].clone(),
            &self.live_at_raise[bi],
            after,
        )
    }
}
