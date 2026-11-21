//! What is certainly held at each point of a function (`EFF106`, `EFF109`): a forward
//! must-dataflow over the CFG, exception edges included, of the acquisitions an analysis
//! tracks.
//!
//! An acquisition is named by the span of the instruction taking it, so the copies of a
//! `finally` body (built once per way out of its `try`) take the same one. The state of a block
//! is a bit per acquisition of the function, so it grows with the acquisitions, not with what
//! they hold.

use heki_ast::Span;
use heki_ir::walk::FnShape;
use heki_ir::{BlockId, VarId};
use rustc_hash::FxHashMap;

/// The acquisitions of one function, by index, with what each holds.
pub struct Acquisitions<T> {
    spans: Vec<Span>,
    items: Vec<T>,
    /// The managers of the `with` items taking each (empty for an `acquire()`).
    mgrs: Vec<Vec<VarId>>,
    by_span: FxHashMap<Span, usize>,
}

impl<T> Default for Acquisitions<T> {
    fn default() -> Self {
        Self {
            spans: Vec::new(),
            items: Vec::new(),
            mgrs: Vec::new(),
            by_span: FxHashMap::default(),
        }
    }
}

impl<T> Acquisitions<T> {
    /// The acquisition at `span` (the first `item` given for it), taken by a `with` over `mgr`
    /// or by an `acquire()` (`None`).
    pub fn add(&mut self, span: Span, mgr: Option<VarId>, item: T) -> usize {
        let k = *self.by_span.entry(span).or_insert_with(|| {
            self.spans.push(span);
            self.items.push(item);
            self.mgrs.push(Vec::new());
            self.spans.len() - 1
        });
        if let Some(m) = mgr
            && !self.mgrs[k].contains(&m)
        {
            self.mgrs[k].push(m);
        }
        k
    }

    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    pub fn span(&self, k: usize) -> Span {
        self.spans[k]
    }

    pub fn item(&self, k: usize) -> &T {
        &self.items[k]
    }

    /// The acquisitions an `ExitWith` of `mgr` spanning its `with` statement (`span`) gives
    /// back: those of `mgr` by the items inside that statement.
    pub fn exited(&self, mgr: VarId, span: Span) -> Vec<usize> {
        (0..self.len())
            .filter(|&k| {
                let taken = self.spans[k];
                self.mgrs[k].contains(&mgr) && span.start <= taken.start && taken.end <= span.end
            })
            .collect()
    }

    /// The acquisitions whose item `released` names: a `release()` gives back the lock,
    /// whether an `acquire()` or a `with` took it.
    pub fn released(&self, released: impl Fn(&T) -> bool) -> Vec<usize> {
        (0..self.len())
            .filter(|&k| released(&self.items[k]))
            .collect()
    }
}

/// What one instruction does to the acquisitions held.
pub enum Change {
    Take(usize),
    Give(Vec<usize>),
}

/// The acquisitions held, a bit each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldSet(Vec<u64>);

impl HeldSet {
    fn empty(n: usize) -> Self {
        Self(vec![0; n.div_ceil(64)])
    }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|w| *w == 0)
    }

    /// The indices held, in order.
    pub fn ones(&self) -> impl Iterator<Item = usize> + '_ {
        self.0.iter().enumerate().flat_map(|(i, w)| {
            (0..64)
                .filter(move |b| w & (1u64 << b) != 0)
                .map(move |b| i * 64 + b)
        })
    }

    pub fn apply(&mut self, change: Option<&Change>) {
        match change {
            Some(Change::Take(k)) => self.0[k / 64] |= 1u64 << (k % 64),
            Some(Change::Give(ks)) => {
                for k in ks {
                    self.0[k / 64] &= !(1u64 << (k % 64));
                }
            }
            None => {}
        }
    }

    /// Keep only what `other` holds too; whether anything changed.
    fn meet(&mut self, other: &Self) -> bool {
        let mut changed = false;
        for (w, o) in self.0.iter_mut().zip(&other.0) {
            let next = *w & o;
            changed |= next != *w;
            *w = next;
        }
        changed
    }
}

/// The acquisitions certainly held on entry to each block (`None`: unreachable), of `n`
/// acquisitions changed as `change` says for instruction `index` of block `bb`, joined by
/// intersection. Along an exception edge only what the block holds at every one of its
/// instructions flows.
pub fn at_entry<'c>(
    shape: &FnShape<'_>,
    n: usize,
    change: impl Fn(BlockId, usize) -> Option<&'c Change>,
) -> Vec<Option<HeldSet>> {
    let f = shape.f;
    let mut ins: Vec<Option<HeldSet>> = vec![None; f.blocks.len()];
    ins[0] = Some(HeldSet::empty(n));
    let meet = |slot: &mut Option<HeldSet>, incoming: &HeldSet| -> bool {
        match slot {
            Some(cur) => cur.meet(incoming),
            None => {
                *slot = Some(incoming.clone());
                true
            }
        }
    };
    loop {
        let mut changed = false;
        // Reverse postorder reaches every block from an already visited predecessor first.
        for &b in shape.rpo() {
            // Reverse postorder visits a predecessor of every block but the first before it.
            let mut state = ins[b.0 as usize]
                .clone()
                .unwrap_or_else(|| HeldSet::empty(n));
            let bb = f.block(b);
            let mut throughout = state.clone();
            for i in 0..bb.insts.len() {
                state.apply(change(b, i));
                throughout.meet(&state);
            }
            for s in bb.terminator.successors() {
                changed |= meet(&mut ins[s.0 as usize], &state);
            }
            if let Some(e) = bb.exc_target {
                changed |= meet(&mut ins[e.0 as usize], &throughout);
            }
        }
        if !changed {
            return ins;
        }
    }
}
