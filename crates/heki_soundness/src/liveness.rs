//! Backward liveness over the SSA-CFG (docs/ANALYSIS_AND_KB_SPEC.md section 3.3).
//!
//! A handler (`exc_target`) is entered from an instruction of the block that may raise or
//! from its `raise`, so what is live at the handler is live before each of those
//! ([`heki_ir::InstKind::may_raise`]), not after the last one.
//! A phi's incoming value is used at the end of its predecessor, not at the phi's block.

use crate::interpreter::term_use;
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

pub(crate) struct Liveness {
    /// Live at the end of each block, on its normal-flow edges.
    live_out: Vec<VarSet>,
    /// Live where each block may raise into its handler.
    live_at_raise: Vec<VarSet>,
}

impl Liveness {
    pub fn compute(f: &IrFunction) -> Self {
        let nvars = f.var_names.len();
        let n = f.blocks.len();
        let mut live_in = vec![VarSet::new(nvars); n];
        let mut live_out = vec![VarSet::new(nvars); n];
        let mut live_at_raise = vec![VarSet::new(nvars); n];
        let mut order = f.rpo_blocks();
        order.reverse();
        let entering = |live_in: &[VarSet], b: BlockId, s: BlockId| {
            let mut live = live_in[s.0 as usize].clone();
            for phi in &f.block(s).phis {
                for &(p, v) in &phi.incoming {
                    if p == b {
                        live.insert(v);
                    }
                }
            }
            live
        };
        loop {
            let mut changed = false;
            for &b in &order {
                let bi = b.0 as usize;
                let bb = f.block(b);
                let mut out = VarSet::new(nvars);
                for s in bb.terminator.successors() {
                    out.union_with(&entering(&live_in, b, s));
                }
                let at_raise = bb
                    .exc_target
                    .map_or_else(|| VarSet::new(nvars), |e| entering(&live_in, b, e));
                let inn = Self::backward(bb, out.clone(), &at_raise, bb.insts.len());
                let inn = bb.phis.iter().fold(inn, |mut live, phi| {
                    live.remove(phi.dst);
                    live
                });
                for (slot, new) in [
                    (&mut live_out[bi], out),
                    (&mut live_at_raise[bi], at_raise),
                    (&mut live_in[bi], inn),
                ] {
                    if *slot != new {
                        *slot = new;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        Self {
            live_out,
            live_at_raise,
        }
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
