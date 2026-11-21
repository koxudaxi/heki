//! The tables of one function that walks read in place of scans over its blocks.

use crate::{
    BlockId, ConstLiteral, DominatorTree, Inst, InstKind, IrFunction, NaturalLoop, Terminator,
    VarId,
};
use std::cell::OnceCell;

/// Where a value is defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Def {
    /// Nothing in the function defines it.
    None,
    Param,
    /// The instruction at this index of the block.
    Inst(BlockId, u32),
    /// The phi at this index of the block.
    Phi(BlockId, u32),
}

/// Where a value is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Use {
    Inst(BlockId, u32),
    Phi(BlockId, u32),
    /// The block's terminator: the condition of a branch, or the value returned or raised.
    Terminator(BlockId),
}

/// A function's tables, each built on its first use in time linear in the function, so that
/// every read is constant or proportional to its answer. One owner (a rule run on the function,
/// the summary of the function) builds one and hands it to all its walks. The names differ from
/// the linear helpers of [`IrFunction`] they replace, which the architecture test finds by name.
pub struct FnShape<'a> {
    pub f: &'a IrFunction,
    defs: OnceCell<Vec<Def>>,
    readers: OnceCell<Rows<Use>>,
    copy_roots: OnceCell<Vec<VarId>>,
    preds: OnceCell<Rows<BlockId>>,
    rpo: OnceCell<Rpo>,
    dominators: OnceCell<DominatorTree>,
    loops: OnceCell<Loops>,
    starred: OnceCell<Vec<(VarId, u32)>>,
}

/// Rows of items by dense index, in one allocation.
struct Rows<T> {
    starts: Vec<u32>,
    items: Vec<T>,
}

impl<T> Rows<T> {
    fn row(&self, i: usize) -> &[T] {
        match (self.starts.get(i), self.starts.get(i + 1)) {
            (Some(&a), Some(&b)) => &self.items[a as usize..b as usize],
            _ => &[],
        }
    }

    /// The rows of `(row, item)` pairs, which `pairs` lists by row and then in their order.
    fn from_sorted(rows: usize, pairs: Vec<(u32, T)>) -> Self {
        let mut starts = vec![0u32; rows + 1];
        for (row, _) in &pairs {
            starts[*row as usize + 1] += 1;
        }
        for i in 0..rows {
            starts[i + 1] += starts[i];
        }
        Self {
            starts,
            items: pairs.into_iter().map(|(_, item)| item).collect(),
        }
    }
}

struct Rpo {
    order: Vec<BlockId>,
    /// Position of each block in `order` (`u32::MAX`: no path from the entry reaches it).
    index: Vec<u32>,
}

struct Loops {
    depth: Vec<u16>,
    /// Index into `f.loops` (`u32::MAX`: in no loop).
    innermost: Vec<u32>,
    /// Each loop's body, sorted.
    bodies: Vec<Vec<BlockId>>,
}

impl<'a> FnShape<'a> {
    pub fn new(f: &'a IrFunction) -> Self {
        Self {
            f,
            defs: OnceCell::new(),
            readers: OnceCell::new(),
            copy_roots: OnceCell::new(),
            preds: OnceCell::new(),
            rpo: OnceCell::new(),
            dominators: OnceCell::new(),
            loops: OnceCell::new(),
            starred: OnceCell::new(),
        }
    }

    // ---------------------------------------------------------------------------- values

    /// Where `v` is defined. The builder defines each value once; were one defined more than
    /// once, the first instruction in block order would win, as in [`IrFunction::def_inst`],
    /// then the first phi, then the parameter.
    pub fn def(&self, v: VarId) -> Def {
        let defs = self.defs.get_or_init(|| {
            let mut defs = vec![Def::None; self.f.var_names.len()];
            let mut set = |v: VarId, d: Def| {
                if let Some(slot @ Def::None) = defs.get_mut(v.0 as usize) {
                    *slot = d;
                }
            };
            for bb in &self.f.blocks {
                for (i, inst) in bb.insts.iter().enumerate() {
                    if let Some(d) = inst.kind.dst() {
                        set(d, Def::Inst(bb.id, i as u32));
                    }
                }
            }
            for bb in &self.f.blocks {
                for (i, phi) in bb.phis.iter().enumerate() {
                    set(phi.dst, Def::Phi(bb.id, i as u32));
                }
            }
            for p in &self.f.params {
                set(p.var, Def::Param);
            }
            defs
        });
        defs.get(v.0 as usize).copied().unwrap_or(Def::None)
    }

    /// The instruction defining `v`, with its block ([`IrFunction::def_inst`]).
    pub fn defining(&self, v: VarId) -> Option<(BlockId, &'a Inst)> {
        match self.def(v) {
            Def::Inst(b, i) => Some((b, &self.f.block(b).insts[i as usize])),
            _ => None,
        }
    }

    /// The instruction defining `v` (none for a parameter or a phi).
    pub fn inst(&self, v: VarId) -> Option<&'a Inst> {
        self.defining(v).map(|(_, inst)| inst)
    }

    /// The string constant `v` is ([`IrFunction::const_str`]).
    pub fn str_const(&self, v: VarId) -> Option<&'a str> {
        match self.literal(v)? {
            ConstLiteral::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// The integer constant `v` is ([`IrFunction::const_int`]).
    pub fn int_const(&self, v: VarId) -> Option<i64> {
        match self.literal(v)? {
            ConstLiteral::Int(n) => Some(*n),
            _ => None,
        }
    }

    /// The boolean constant `v` is ([`IrFunction::const_bool`]).
    pub fn bool_const(&self, v: VarId) -> Option<bool> {
        match self.literal(v)? {
            ConstLiteral::Bool(b) => Some(*b),
            _ => None,
        }
    }

    fn literal(&self, v: VarId) -> Option<&'a ConstLiteral> {
        match &self.inst(v)?.kind {
            InstKind::Const { literal, .. } => Some(literal),
            _ => None,
        }
    }

    /// The phis, instructions, and terminators reading `v`, each once, in block order (a
    /// block's phis, then its instructions, then its terminator).
    pub fn readers(&self, v: VarId) -> &[Use] {
        self.readers
            .get_or_init(|| {
                let mut pairs: Vec<(u32, Use)> = Vec::new();
                let mut read = Vec::new();
                for bb in &self.f.blocks {
                    for (i, phi) in bb.phis.iter().enumerate() {
                        read.clear();
                        read.extend(phi.incoming.iter().map(|(_, x)| *x));
                        each_once(&mut read, &mut pairs, Use::Phi(bb.id, i as u32));
                    }
                    for (i, inst) in bb.insts.iter().enumerate() {
                        read.clear();
                        read.extend(inst.kind.operands());
                        each_once(&mut read, &mut pairs, Use::Inst(bb.id, i as u32));
                    }
                    if let Terminator::Branch { cond: x, .. }
                    | Terminator::Return(Some(x))
                    | Terminator::Raise(Some(x)) = bb.terminator
                    {
                        pairs.push((x.0, Use::Terminator(bb.id)));
                    }
                }
                // A stable sort keeps each value's readers in block order.
                pairs.sort_by_key(|(v, _)| *v);
                Rows::from_sorted(self.f.var_names.len(), pairs)
            })
            .row(v.0 as usize)
    }

    /// The value a chain of copies from `v` ends at (`v` itself when no copy defines it). The
    /// builder closes no cycle of copies (a loop no path enters keeps its header phi); were one
    /// closed, a chain into it would end at its lowest value, whichever value it starts from.
    pub fn copy_root(&self, v: VarId) -> VarId {
        let roots = self.copy_roots.get_or_init(|| {
            const UNKNOWN: u32 = u32::MAX;
            const ON_CHAIN: u32 = u32::MAX - 1;
            let n = self.f.var_names.len();
            let mut roots = vec![UNKNOWN; n];
            let mut chain: Vec<usize> = Vec::new();
            for start in 0..n {
                let mut cur = start;
                let root = loop {
                    match roots[cur] {
                        UNKNOWN => {}
                        ON_CHAIN => {
                            // The chain closes a cycle at `cur`: the values from it on.
                            let cycle = chain.iter().skip_while(|&&c| c != cur);
                            break cycle.fold(cur, |low, &c| low.min(c)) as u32;
                        }
                        known => break known,
                    }
                    match self.inst(VarId(cur as u32)).map(|i| &i.kind) {
                        Some(InstKind::Copy { src, .. }) => {
                            roots[cur] = ON_CHAIN;
                            chain.push(cur);
                            cur = src.0 as usize;
                        }
                        _ => {
                            roots[cur] = cur as u32;
                            break cur as u32;
                        }
                    }
                };
                for c in chain.drain(..) {
                    roots[c] = root;
                }
            }
            roots.into_iter().map(VarId).collect::<Vec<_>>()
        });
        roots.get(v.0 as usize).copied().unwrap_or(v)
    }

    /// Whether operand `index` of the display or call defining `dst` is written with `*`
    /// ([`IrFunction::is_starred`]).
    pub fn is_starred(&self, dst: VarId, index: usize) -> bool {
        u32::try_from(index)
            .is_ok_and(|index| self.starred_table().binary_search(&(dst, index)).is_ok())
    }

    /// The lowest index of an operand written with `*` in the display or call defining `dst`.
    pub fn first_starred(&self, dst: VarId) -> Option<u32> {
        let table = self.starred_table();
        let at = table.partition_point(|&(d, _)| d < dst);
        table.get(at).filter(|(d, _)| *d == dst).map(|(_, i)| *i)
    }

    fn starred_table(&self) -> &[(VarId, u32)] {
        self.starred.get_or_init(|| {
            let mut table = self.f.starred.clone();
            table.sort();
            table
        })
    }

    // ---------------------------------------------------------------------------- blocks

    /// The blocks `b` has an edge to: its terminator's successors, then its exception target.
    pub fn succs(&self, b: BlockId) -> impl Iterator<Item = BlockId> + use<'a> {
        let bb = self.f.block(b);
        let (then, other) = match bb.terminator {
            Terminator::Jump(to) => (Some(to), None),
            Terminator::Branch {
                then_bb, else_bb, ..
            } => (Some(then_bb), Some(else_bb)),
            Terminator::Return(_) | Terminator::Raise(_) | Terminator::Unreachable => (None, None),
        };
        then.into_iter().chain(other).chain(bb.exc_target)
    }

    /// The blocks with an edge (normal or exception) to `b`, each once, in block order
    /// ([`IrFunction::predecessors`]).
    pub fn preds(&self, b: BlockId) -> &[BlockId] {
        self.preds
            .get_or_init(|| {
                let mut pairs: Vec<(u32, BlockId)> = Vec::new();
                for bb in &self.f.blocks {
                    pairs.extend(self.succs(bb.id).map(|s| (s.0, bb.id)));
                }
                pairs.sort();
                pairs.dedup();
                Rows::from_sorted(self.f.blocks.len(), pairs)
            })
            .row(b.0 as usize)
    }

    /// The blocks a path from the entry reaches, in reverse postorder
    /// ([`IrFunction::rpo_blocks`]).
    pub fn rpo(&self) -> &[BlockId] {
        &self.rpo_table().order
    }

    /// The position of `b` in [`Self::rpo`] (`None`: no path from the entry reaches it).
    pub fn rpo_index(&self, b: BlockId) -> Option<u32> {
        let index = *self.rpo_table().index.get(b.0 as usize)?;
        (index != u32::MAX).then_some(index)
    }

    /// Whether a path from the entry reaches `b`.
    pub fn reachable(&self, b: BlockId) -> bool {
        self.rpo_index(b).is_some()
    }

    fn rpo_table(&self) -> &Rpo {
        self.rpo.get_or_init(|| {
            let order = self.f.rpo_blocks();
            let mut index = vec![u32::MAX; self.f.blocks.len()];
            for (i, b) in order.iter().enumerate() {
                index[b.0 as usize] = i as u32;
            }
            Rpo { order, index }
        })
    }

    /// The function's dominator tree over its normal and exception edges. It answers for the
    /// blocks a path from the entry reaches; what a walk takes for the others is its own
    /// statement.
    pub fn dominators(&self) -> &DominatorTree {
        self.dominators.get_or_init(|| DominatorTree::of(self.f))
    }

    // ----------------------------------------------------------------------------- loops

    /// Whether `b` is in the body of loop `li` (an index into `f.loops`).
    pub fn in_loop(&self, li: usize, b: BlockId) -> bool {
        self.loops_table()
            .bodies
            .get(li)
            .is_some_and(|body| body.binary_search(&b).is_ok())
    }

    /// The deepest loop whose body holds `b` (the last listed of equally deep ones), as an
    /// index into `f.loops` ([`IrFunction::innermost_loop`]).
    pub fn innermost_index(&self, b: BlockId) -> Option<usize> {
        let li = *self.loops_table().innermost.get(b.0 as usize)?;
        (li != u32::MAX).then_some(li as usize)
    }

    /// [`Self::innermost_index`], the loop itself.
    pub fn innermost(&self, b: BlockId) -> Option<&'a NaturalLoop> {
        self.innermost_index(b).map(|li| &self.f.loops[li])
    }

    /// The deepest loop depth among the loops whose body holds `b`, `0` in none
    /// ([`IrFunction::block_loop_depth`]).
    pub fn loop_depth(&self, b: BlockId) -> u16 {
        self.loops_table()
            .depth
            .get(b.0 as usize)
            .copied()
            .unwrap_or(0)
    }

    fn loops_table(&self) -> &Loops {
        self.loops.get_or_init(|| {
            let blocks = self.f.blocks.len();
            let mut depth = vec![0u16; blocks];
            let mut innermost = vec![u32::MAX; blocks];
            let mut bodies = Vec::with_capacity(self.f.loops.len());
            for (li, lp) in self.f.loops.iter().enumerate() {
                for &b in &lp.body {
                    let at = &mut innermost[b.0 as usize];
                    // `max_by_key` keeps the last of equal maxima.
                    if *at == u32::MAX || lp.depth >= self.f.loops[*at as usize].depth {
                        *at = li as u32;
                    }
                    depth[b.0 as usize] = depth[b.0 as usize].max(lp.depth);
                }
                let mut body = lp.body.clone();
                body.sort();
                bodies.push(body);
            }
            Loops {
                depth,
                innermost,
                bodies,
            }
        })
    }
}

/// Push `(value, at)` once per distinct value of `read`.
fn each_once(read: &mut Vec<VarId>, pairs: &mut Vec<(u32, Use)>, at: Use) {
    read.sort_unstable();
    read.dedup();
    pairs.extend(read.iter().map(|x| (x.0, at)));
}
