//! Backward liveness over the SSA-CFG (docs/DETAILED_SPEC_M3_M7.md section 4.3).
//!
//! Successors include `exc_target` edges: a handler can be entered from any point of a
//! protected block, so everything live at the handler is live throughout the block.
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
    nvars: usize,
    live_out: Vec<VarSet>,
}

fn successors(f: &IrFunction, b: BlockId) -> Vec<BlockId> {
    let bb = f.block(b);
    let mut s = bb.terminator.successors();
    if let Some(e) = bb.exc_target
        && !s.contains(&e)
    {
        s.push(e);
    }
    s
}

impl Liveness {
    pub fn compute(f: &IrFunction) -> Self {
        let nvars = f.var_names.len();
        let n = f.blocks.len();
        let mut live_in = vec![VarSet::new(nvars); n];
        let mut live_out = vec![VarSet::new(nvars); n];
        let mut order = f.rpo_blocks();
        order.reverse();
        loop {
            let mut changed = false;
            for &b in &order {
                let bi = b.0 as usize;
                let mut out = VarSet::new(nvars);
                for s in successors(f, b) {
                    out.union_with(&live_in[s.0 as usize]);
                    for phi in &f.block(s).phis {
                        for &(p, v) in &phi.incoming {
                            if p == b {
                                out.insert(v);
                            }
                        }
                    }
                }
                let mut inn = out.clone();
                let bb = f.block(b);
                if let Some(v) = term_use(&bb.terminator) {
                    inn.insert(v);
                }
                for inst in bb.insts.iter().rev() {
                    if let Some(d) = inst.kind.dst() {
                        inn.remove(d);
                    }
                    for op in inst.kind.operands() {
                        inn.insert(op);
                    }
                }
                for phi in &bb.phis {
                    inn.remove(phi.dst);
                }
                if live_out[bi] != out {
                    live_out[bi] = out;
                    changed = true;
                }
                if live_in[bi] != inn {
                    live_in[bi] = inn;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        Self { nvars, live_out }
    }

    /// Variables live immediately after instruction `idx` of block `b`.
    pub fn live_after(&self, f: &IrFunction, b: BlockId, idx: usize) -> VarSet {
        let bb = f.block(b);
        let mut live = self
            .live_out
            .get(b.0 as usize)
            .cloned()
            .unwrap_or_else(|| VarSet::new(self.nvars));
        if let Some(v) = term_use(&bb.terminator) {
            live.insert(v);
        }
        for inst in bb.insts.iter().skip(idx + 1).rev() {
            if let Some(d) = inst.kind.dst() {
                live.remove(d);
            }
            for op in inst.kind.operands() {
                live.insert(op);
            }
        }
        live
    }
}
