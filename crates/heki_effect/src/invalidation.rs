//! `MUT109` facts (docs/ANALYSIS_AND_KB_SPEC.md section 2.3): after a write to a declared
//! `invalidation` target, every path to the function's normal return must reach a call that
//! announces the change.
//!
//! The control flow is read once, when a function is summarized: each point that matters (the
//! entry, every project call, every write to a target) gets a [`SiteFlow`], namely the contracts
//! whose invalidators the function itself calls on every normal path from that point (a backward
//! dataflow over the normal edges, so a different invalidator in each branch counts), and the
//! nearest project call on every such path (from the post-dominator tree). Chaining those calls
//! with the callees' `must_invalidate` bits answers "is this write announced, directly or through
//! calls" during propagation and in the rule, with no IR. Exception edges are not followed and a
//! path that ends in `raise` carries no obligation: only normal returns count. A branch on a
//! constant (`while True:`) takes its one feasible edge, and one on a boolean flag the path sets
//! after the point (`changed = True ... if changed: notify()`) too (see [`Flags`]).

use crate::callgraph::Through;
use crate::{
    CallSite, Delivery, EffectSummary, Facts, FuncKey, ProjectIndex, SharedChain, WriteKind,
};
use heki_ast::Span;
use heki_ir::walk::{self, And, Answer, Budget, FnShape, Sparse, Visit, fixpoint, reach};
use heki_ir::{BlockId, ConstLiteral, InstKind, IrFunction, Terminator, VarId};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::Path;

/// What follows one point of a function on every normal path to a return (`MUT109`).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct SiteFlow {
    /// Contracts (bits) one of whose invalidators the function itself calls on every such path.
    pub invalidated: u64,
    /// The nearest project call on every such path, as an index into the file's `call_sites`;
    /// that call's own flow continues the chain.
    pub next_call: Option<u32>,
}

/// Where an object a function holds may come from, among the objects that existed before the
/// call (`MUT109`): its parameters and what they reach (attributes, items, iteration elements,
/// through copies of the reference), and module or closure state. An object the function
/// constructs, copies, or gets from a call reaches nothing: no fact can describe it yet, or its
/// origin is unknown, so writing it announces nothing a caller has to know.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Reach {
    /// Parameters (bit `i`: parameter `i`, bit 0 a method's `self`) the object is reached from.
    pub params: u64,
    /// Reached from module globals or closure variables.
    pub globals: bool,
}

impl Reach {
    /// Whether the object may have existed before the call.
    pub fn any(self) -> bool {
        self.params != 0 || self.globals
    }

    pub fn join(self, other: Self) -> Self {
        Self {
            params: self.params | other.params,
            globals: self.globals || other.globals,
        }
    }
}

/// One write to a contract target in a function's own body (`MUT109`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowWrite {
    /// Contracts whose targets the written attribute is (the function's exempt ones removed).
    pub contracts: u64,
    /// Fully qualified class of the written object.
    pub class_qualname: String,
    pub attr: String,
    pub kind: WriteKind,
    pub span: Span,
    /// Where the written object comes from.
    pub reach: Reach,
    /// The parameters the written object may be, as itself (`TypedFieldWriteSite::obj_params`).
    pub obj_params: u64,
    pub flow: SiteFlow,
}

/// The `MUT109` facts of one function that propagation reads: the flow from its entry and its
/// own target writes. Kept in `FileSummary::flows` (local facts, no IR); only computed when an
/// `invalidation` contract is configured.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FuncFlow {
    /// The flow from the entry of each definition (a qualname defined twice has two).
    pub entries: Vec<SiteFlow>,
    pub writes: Vec<FlowWrite>,
}

/// A write to a contract target that some normal path of the summary owner leaves unannounced
/// (`MUT109`): the owner's own write, or one a private helper it calls passes up.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct UnmetWrite {
    /// Index of the `invalidation` contract.
    pub contract: u8,
    /// Fully qualified class of the written object.
    pub class_qualname: String,
    pub attr: String,
    /// Kind of the original write.
    pub kind: WriteKind,
    /// The write, or the call of the helper that passes it up, in the owner's file.
    pub span: Span,
    /// The original write, in the file of the function that writes (the owner's own write:
    /// `span`). With the writing function it identifies the violation site
    /// (`Diagnostic::sites`).
    pub origin: Span,
    /// Calls from the owner down to the function that writes (empty: the owner itself).
    pub via_chain: SharedChain,
    /// Where the written object comes from, as the owner sees it.
    pub reach: Reach,
    /// The owner's parameters the written object may be, as itself: `self` (bit 0) in an
    /// initializer is the class's own initialization.
    pub obj_params: u64,
}

/// The points of one function the flow analysis describes, by position (`block`, instruction
/// index).
#[derive(Debug, Default)]
pub(crate) struct Events {
    /// Calls that announce a change themselves: the contracts their callee is an invalidator of.
    pub invalidators: Vec<(BlockId, usize, u64)>,
    /// The function's project call sites, with their index in its site list.
    pub calls: Vec<(BlockId, usize, usize)>,
    /// Writes to contract targets (their `flow` is filled in by [`analyze`]).
    pub writes: Vec<(BlockId, usize, FlowWrite)>,
}

/// The mask of the first `n` contracts (`n` in `1..=64`).
pub(crate) fn contracts_mask(n: usize) -> u64 {
    u64::MAX
        .checked_shr(64 - u32::try_from(n.min(64)).unwrap_or(64))
        .unwrap_or(0)
}

/// The most flag states one point's path search visits before it settles for the
/// flag-insensitive answer.
const MAX_FLAG_STATES: usize = 4096;

/// Branch conditions a function computes from boolean constants: directly (`while True:`,
/// `done = True ... if not done:`), or through flags, phis whose inputs are all boolean
/// constants or flags (`changed = False` before a loop, `changed = True` after a write), tested
/// as `if changed:`, `if not changed:`, `if changed is True:`. Along one path from a point, a
/// flag set after the point is known, so the branch that tests it takes one edge:
/// `changed = True` after a write makes `if changed: notify()` announce it.
struct Flags<'f> {
    consts: &'f FxHashMap<VarId, bool>,
    defs: FxHashMap<VarId, &'f InstKind>,
    /// Flag phis by block.
    phis: Vec<Vec<FlagPhi<'f>>>,
}

/// A flag phi: its variable and its `(predecessor, input)` pairs.
type FlagPhi<'f> = (VarId, &'f [(BlockId, VarId)]);

/// Known flag values along one path, sorted by variable.
type FlagEnv = Vec<(VarId, bool)>;

impl<'f> Flags<'f> {
    /// The conditions of `f`, and whether a branch tests a flag.
    fn of(shape: &FnShape<'f>, consts: &'f FxHashMap<VarId, bool>) -> (Self, bool) {
        let f = shape.f;
        // Without a boolean constant no condition is known and no phi is a flag.
        if consts.is_empty() {
            let phis = vec![Vec::new(); f.blocks.len()];
            let defs = FxHashMap::default();
            return (Self { consts, defs, phis }, false);
        }
        let mut defs = FxHashMap::default();
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            if let Some(d) = inst.kind.dst() {
                defs.insert(d, &inst.kind);
            }
        }
        // A greatest fixpoint: every phi is a flag until an input is neither a constant nor a
        // flag (loops make phis feed each other).
        let mut flags: rustc_hash::FxHashSet<VarId> = f
            .blocks
            .iter()
            .flat_map(|b| &b.phis)
            .map(|p| p.dst)
            .collect();
        loop {
            let before = flags.len();
            for phi in f.blocks.iter().flat_map(|b| &b.phis) {
                let flag_input = |v: &VarId| {
                    let v = shape.copy_root(*v);
                    consts.contains_key(&v) || flags.contains(&v)
                };
                if !phi
                    .incoming
                    .iter()
                    .all(|(_, v)| *v == phi.dst || flag_input(v))
                {
                    flags.remove(&phi.dst);
                }
            }
            if flags.len() == before {
                break;
            }
        }
        let phis: Vec<Vec<FlagPhi<'f>>> = f
            .blocks
            .iter()
            .map(|b| {
                b.phis
                    .iter()
                    .filter(|p| flags.contains(&p.dst))
                    .map(|p| (p.dst, p.incoming.as_slice()))
                    .collect()
            })
            .collect();
        let out = Self { consts, defs, phis };
        // The operands of comparisons may share values (`b = a == a`): each value is decided
        // once.
        let mut decided = FxHashMap::default();
        let tested = f.blocks.iter().any(|b| match b.terminator {
            Terminator::Branch { cond, .. } => out.depends_on_flag(cond, &flags, &mut decided),
            _ => false,
        });
        (out, tested)
    }

    fn depends_on_flag(
        &self,
        v: VarId,
        flags: &rustc_hash::FxHashSet<VarId>,
        decided: &mut FxHashMap<VarId, bool>,
    ) -> bool {
        if let Some(&known) = decided.get(&v) {
            return known;
        }
        let depends = match self.defs.get(&v) {
            _ if flags.contains(&v) => true,
            Some(InstKind::Copy { src, .. }) => self.depends_on_flag(*src, flags, decided),
            Some(InstKind::UnaryOp {
                op: heki_ir::UnaryOpKind::Not,
                operand,
                ..
            }) => self.depends_on_flag(*operand, flags, decided),
            Some(InstKind::Compare { lhs, rhs, .. }) => {
                self.depends_on_flag(*lhs, flags, decided)
                    || self.depends_on_flag(*rhs, flags, decided)
            }
            _ => false,
        };
        decided.insert(v, depends);
        depends
    }

    /// The value of `v` on a path with `env`, if known.
    fn value(&self, v: VarId, env: &FlagEnv) -> Option<bool> {
        self.value_in(v, env, &mut FxHashMap::default())
    }

    /// [`Self::value`], deciding each value once (comparisons may share operands).
    fn value_in(
        &self,
        v: VarId,
        env: &FlagEnv,
        decided: &mut FxHashMap<VarId, Option<bool>>,
    ) -> Option<bool> {
        if let Some(b) = self.consts.get(&v) {
            return Some(*b);
        }
        if let Ok(i) = env.binary_search_by_key(&v, |(k, _)| *k) {
            return Some(env[i].1);
        }
        if let Some(&known) = decided.get(&v) {
            return known;
        }
        let value = match self.defs.get(&v)? {
            InstKind::Copy { src, .. } => self.value_in(*src, env, decided),
            InstKind::UnaryOp {
                op: heki_ir::UnaryOpKind::Not,
                operand,
                ..
            } => self.value_in(*operand, env, decided).map(|b| !b),
            InstKind::Compare {
                op:
                    op @ (heki_ir::CompareOpKind::Is
                    | heki_ir::CompareOpKind::Eq
                    | heki_ir::CompareOpKind::IsNot
                    | heki_ir::CompareOpKind::NotEq),
                lhs,
                rhs,
                ..
            } => self
                .value_in(*lhs, env, decided)
                .zip(self.value_in(*rhs, env, decided))
                .map(|(l, r)| {
                    let positive =
                        matches!(op, heki_ir::CompareOpKind::Is | heki_ir::CompareOpKind::Eq);
                    (l == r) == positive
                }),
            _ => None,
        };
        decided.insert(v, value);
        value
    }

    /// The flags known on entering block `to` from `from`, given those known in `from`.
    fn enter(&self, from: BlockId, to: usize, env: &FlagEnv) -> FlagEnv {
        let mut next: FlagEnv = env
            .iter()
            .copied()
            .filter(|(v, _)| !self.phis[to].iter().any(|(d, _)| d == v))
            .collect();
        for (dst, incoming) in &self.phis[to] {
            if let Some(value) = incoming
                .iter()
                .find(|(b, _)| *b == from)
                .and_then(|(_, v)| self.value(*v, env))
            {
                next.push((*dst, value));
            }
        }
        next.sort_unstable();
        next
    }
}

/// The flows of a function's events: `(entry, flow of each project call by site index, writes)`.
/// `next_call` indices are site-list indices of the function (the caller rebases them onto the
/// file's call sites).
pub(crate) fn analyze(
    shape: &FnShape<'_>,
    mut events: Events,
    contracts: usize,
) -> (SiteFlow, Vec<(usize, SiteFlow)>, Vec<FlowWrite>) {
    let f = shape.f;
    let all = contracts_mask(contracts);
    let n = f.blocks.len();
    let consts: FxHashMap<VarId, bool> = f
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .filter_map(|i| match &i.kind {
            InstKind::Const {
                dst,
                literal: ConstLiteral::Bool(v),
            } => Some((*dst, *v)),
            _ => None,
        })
        .collect();
    let (conds, has_flags) = Flags::of(shape, &consts);
    // Normal successors (a constant branch keeps its feasible edge); `None`: a normal return.
    let mut succ: Vec<Option<Vec<usize>>> = Vec::with_capacity(n);
    for bb in &f.blocks {
        succ.push(match &bb.terminator {
            Terminator::Jump(t) => Some(vec![t.0 as usize]),
            Terminator::Branch {
                cond,
                then_bb,
                else_bb,
            } => Some(match conds.value(*cond, &Vec::new()) {
                Some(true) => vec![then_bb.0 as usize],
                Some(false) => vec![else_bb.0 as usize],
                None => vec![then_bb.0 as usize, else_bb.0 as usize],
            }),
            Terminator::Return(_) => None,
            Terminator::Raise(_) | Terminator::Unreachable => Some(Vec::new()),
        });
    }

    let mut block_inv = vec![0u64; n];
    let mut inv_at: Vec<Vec<(usize, u64)>> = vec![Vec::new(); n];
    for (b, i, mask) in &events.invalidators {
        block_inv[b.0 as usize] |= mask;
        inv_at[b.0 as usize].push((*i, *mask));
    }
    // Each block's invalidators by position, each with the contracts it and every later one
    // announce: the contracts announced after a point are one lookup, whatever the block holds.
    for v in &mut inv_at {
        v.sort_unstable_by_key(|(i, _)| *i);
        let mut later = 0;
        for (_, mask) in v.iter_mut().rev() {
            later |= *mask;
            *mask = later;
        }
    }
    let budget = Budget::of_function("invalidation::analyze", f);
    let solved = covered_in(shape, &succ, &block_inv, all, &budget).and_then(|cov_in| {
        // Post-dominators only find the next call; without calls there is none to find.
        let ipdom = if events.calls.is_empty() {
            Answer::Known(vec![None; n])
        } else {
            post_dominators(&succ, &budget)
        };
        ipdom.map(|ipdom| (cov_in, ipdom))
    });
    // Past the budget no write is known to go unannounced: every point announces every contract.
    let unknown = if solved.is_open() { all } else { 0 };
    let (cov_in, ipdom) = solved.open_means((vec![all; n], vec![None; n]));
    let cov_out = |b: usize| -> u64 {
        match &succ[b] {
            None => 0,
            Some(s) => s.iter().fold(all, |acc, t| acc & cov_in[*t]),
        }
    };

    let mut calls_at: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
    for (b, i, site) in &events.calls {
        calls_at[b.0 as usize].push((*i, *site));
    }
    for v in &mut calls_at {
        v.sort_unstable();
    }
    let inv_after = |b: usize, after: Option<usize>| -> u64 {
        let first = after.map_or(0, |a| inv_at[b].partition_point(|(i, _)| *i <= a));
        inv_at[b].get(first).map_or(0, |(_, later)| *later)
    };
    // The first call on the post-dominator chain from the start of each block, answered once
    // per block (a chain is followed until a block already answered), so the next call of every
    // site is found in time linear in the function.
    let mut next_from: Vec<Option<Option<usize>>> = vec![None; n];
    let mut chain = Vec::new();
    for start in 0..n {
        let mut found = None;
        let mut x = Some(start);
        while let Some(b) = x.filter(|b| *b != n) {
            if let Some(known) = next_from[b] {
                found = known;
                break;
            }
            if let Some(first) = calls_at[b].first() {
                next_from[b] = Some(Some(first.1));
                found = Some(first.1);
                break;
            }
            chain.push(b);
            x = ipdom[b];
        }
        for b in chain.drain(..) {
            next_from[b] = Some(found);
        }
    }
    let searches = Budget::unmetered("invalidation::flag_covered");
    // The flow after instruction `after` of block `b` (`None`: from the block's start).
    let flow_at = |b: usize, after: Option<usize>| -> SiteFlow {
        let mut invalidated = unknown | inv_after(b, after) | cov_out(b);
        if has_flags && invalidated != all {
            let point = (b, after);
            invalidated |= flag_covered(f, &conds, &succ, &inv_after, all, point, &searches);
        }
        let first = after.map_or(0, |a| calls_at[b].partition_point(|(i, _)| *i <= a));
        let next = calls_at[b].get(first).map(|c| c.1).or_else(|| {
            ipdom[b]
                .filter(|x| *x != n)
                .and_then(|x| next_from[x].flatten())
        });
        SiteFlow {
            invalidated,
            next_call: next.map(|s| u32::try_from(s).unwrap_or(u32::MAX)),
        }
    };
    let entry = flow_at(0, None);
    let calls = events
        .calls
        .iter()
        .map(|(b, i, site)| (*site, flow_at(b.0 as usize, Some(*i))))
        .collect();
    for (b, i, w) in &mut events.writes {
        w.flow = flow_at(b.0 as usize, Some(*i));
    }
    let writes = events.writes.into_iter().map(|(_, _, w)| w).collect();
    (entry, calls, writes)
}

/// The contracts covered from the start of each block (`block_inv`: those each block announces
/// itself): a greatest fixpoint over the normal edges `succ`, each block starting from `all`,
/// so that paths that never return (and loops before a return) add no obligation. A block the
/// entry does not reach keeps `all`. Open past the budget.
fn covered_in(
    shape: &FnShape<'_>,
    succ: &[Option<Vec<usize>>],
    block_inv: &[u64],
    all: u64,
    budget: &Budget,
) -> Answer<Vec<u64>> {
    let n = succ.len();
    // The blocks in postorder, so that a block is first visited after its successors.
    let order: Vec<usize> = shape.rpo().iter().rev().map(|b| b.0 as usize).collect();
    let mut node = vec![usize::MAX; n];
    for (i, b) in order.iter().enumerate() {
        node[*b] = i;
    }
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (b, s) in succ.iter().enumerate() {
        for t in s.iter().flatten() {
            preds[*t].push(b);
        }
    }
    let readers = |i: usize, out: &mut Vec<usize>| {
        out.extend(
            preds[order[i]]
                .iter()
                .map(|p| node[*p])
                .filter(|j| *j != usize::MAX),
        );
    };
    let transfer = |i: usize, covered: &walk::Facts<And>| {
        let b = order[i];
        let rest = match &succ[b] {
            None => 0,
            Some(s) => s.iter().fold(all, |acc, t| {
                acc & covered.get(node[*t]).map_or(all, |c| c.0)
            }),
        };
        Some(And(block_inv[b] | rest))
    };
    // A block is visited once, then once per fact a successor gets or bit it loses.
    let spec = Sparse {
        nodes: order.len(),
        readers: &readers,
        max_visits: 1 + 2 * (1 + 64),
    };
    fixpoint(budget, spec, transfer).map(|covered| {
        (0..n)
            .map(|b| covered.get(node[b]).map_or(all, |c| c.0))
            .collect()
    })
}

/// The immediate post-dominator of each block over the normal edges `succ` (`None`: a normal
/// return), towards a virtual exit (index `succ.len()`) after every return; `None` for a block
/// from which no return is reachable. Lengauer and Tarjan's algorithm over the edges reversed,
/// from the exit: the blocks are numbered depth first, then each is visited once in the reverse
/// order, so no pass repeats. A step per block, edge, and link compressed; open past the budget.
fn post_dominators(succ: &[Option<Vec<usize>>], budget: &Budget) -> Answer<Vec<Option<usize>>> {
    const NONE: usize = usize::MAX;
    let n = succ.len();
    let exit = n;
    // Edges reversed: from each block to those with an edge to it, from the exit to every
    // block that returns.
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n + 1];
    for (b, s) in succ.iter().enumerate() {
        match s {
            None => preds[exit].push(b),
            Some(s) => s.iter().for_each(|t| preds[*t].push(b)),
        }
    }
    // Depth-first numbers from the exit, each block's parent in that tree, and the blocks by
    // number.
    let mut number = vec![NONE; n + 1];
    let mut block = Vec::with_capacity(n + 1);
    let mut parent = Vec::with_capacity(n + 1);
    let mut stack = vec![(exit, NONE)];
    while let Some((x, from)) = stack.pop() {
        if number[x] != NONE {
            continue;
        }
        number[x] = block.len();
        block.push(x);
        parent.push(from);
        stack.extend(preds[x].iter().rev().map(|p| (*p, number[x])));
    }
    let count = block.len();
    let mut steps = count + preds.iter().map(Vec::len).sum::<usize>();
    let to_exit = [exit];
    // The numbers of the blocks with an edge into block number `w` (not the exit) on the
    // reversed edges: its successors, the exit for a return.
    let into = |w: usize| {
        let targets: &[usize] = succ[block[w]].as_deref().unwrap_or(&to_exit);
        targets.iter().map(|t| number[*t]).filter(|v| *v != NONE)
    };
    // By number: the semidominator, the ancestor linked in the forest, and the number of
    // smallest semidominator on the compressed path to that ancestor.
    let mut semi: Vec<usize> = (0..count).collect();
    let mut ancestor = vec![NONE; count];
    let mut best: Vec<usize> = (0..count).collect();
    let mut idom = vec![NONE; count];
    let mut same = vec![NONE; count];
    let mut bucket: Vec<Vec<usize>> = vec![Vec::new(); count];
    let mut path = Vec::new();
    // The number of the smallest semidominator on the forest path above `v`, the path
    // compressed on the way.
    let mut lowest = |v: usize, ancestor: &mut [usize], best: &mut [usize], semi: &[usize]| {
        let mut x = v;
        while ancestor[ancestor[x]] != NONE {
            path.push(x);
            x = ancestor[x];
        }
        steps += path.len();
        for u in path.drain(..).rev() {
            let a = ancestor[u];
            if semi[best[a]] < semi[best[u]] {
                best[u] = best[a];
            }
            ancestor[u] = ancestor[a];
        }
        best[v]
    };
    for w in (1..count).rev() {
        let p = parent[w];
        let mut s = p;
        for v in into(w) {
            let candidate = if v <= w {
                v
            } else {
                semi[lowest(v, &mut ancestor, &mut best, &semi)]
            };
            s = s.min(candidate);
        }
        semi[w] = s;
        bucket[s].push(w);
        ancestor[w] = p;
        for v in std::mem::take(&mut bucket[p]) {
            let y = lowest(v, &mut ancestor, &mut best, &semi);
            if semi[y] == semi[v] {
                idom[v] = p;
            } else {
                same[v] = y;
            }
        }
    }
    for w in 1..count {
        if same[w] != NONE {
            idom[w] = idom[same[w]];
        }
    }
    let mut ipdom: Vec<Option<usize>> = vec![None; n + 1];
    ipdom[exit] = Some(exit);
    for w in 1..count {
        ipdom[block[w]] = Some(block[idom[w]]);
    }
    (budget.take(steps).then_some(ipdom)).map_or(Answer::Open, Answer::Known)
}

/// The contracts announced on every feasible path from the point after instruction `after` of
/// block `b` (`point`), with the flags a path sets (see [`Flags`]); `0` (nothing more than the
/// flag-insensitive answer) when the search exceeds [`MAX_FLAG_STATES`].
fn flag_covered(
    f: &IrFunction,
    flags: &Flags<'_>,
    succ: &[Option<Vec<usize>>],
    inv_after: &dyn Fn(usize, Option<usize>) -> u64,
    all: u64,
    (b, after): (usize, Option<usize>),
    budget: &Budget,
) -> u64 {
    // States: the start point, then `(block, flags)` on entering a block; the states each one
    // leads to (`None`: a normal return).
    let mut states: Vec<(usize, FlagEnv)> = vec![(b, Vec::new())];
    let mut index: FxHashMap<(usize, FlagEnv), usize> = FxHashMap::default();
    let mut edges: Vec<Option<Vec<usize>>> = vec![None];
    let next = |s: usize, out: &mut Vec<usize>| {
        let (block, env) = states[s].clone();
        let targets: Option<Vec<usize>> = match &f.blocks[block].terminator {
            Terminator::Branch {
                cond,
                then_bb,
                else_bb,
            } => match flags.value(*cond, &env) {
                Some(true) => Some(vec![then_bb.0 as usize]),
                Some(false) => Some(vec![else_bb.0 as usize]),
                None => succ[block].clone(),
            },
            _ => succ[block].clone(),
        };
        let from = BlockId(u32::try_from(block).unwrap_or(0));
        let mut ids = Vec::new();
        for t in targets.iter().flatten() {
            let state = (*t, flags.enter(from, *t, &env));
            let id = *index.entry(state.clone()).or_insert_with(|| {
                states.push(state);
                edges.push(None);
                states.len() - 1
            });
            ids.push(id);
        }
        out.extend(&ids);
        edges[s] = targets.map(|_| ids);
    };
    let mut visited = 0;
    let searched = reach(budget, [0], next, |_| {
        visited += 1;
        if visited > MAX_FLAG_STATES {
            Visit::Stop(())
        } else {
            Visit::Expand
        }
    });
    if !matches!(searched, Answer::Known(None)) {
        return 0;
    }
    // A greatest fixpoint over the states, as for the blocks; the later states first.
    let last = states.len() - 1;
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); states.len()];
    for (s, out) in edges.iter().enumerate() {
        for t in out.iter().flatten() {
            preds[*t].push(s);
        }
    }
    let readers = |i: usize, out: &mut Vec<usize>| {
        out.extend(preds[last - i].iter().map(|s| last - s));
    };
    let transfer = |i: usize, covered: &walk::Facts<And>| {
        let s = last - i;
        let own = inv_after(states[s].0, if s == 0 { after } else { None });
        let rest = match &edges[s] {
            None => 0,
            Some(out) => out.iter().fold(all, |acc, t| {
                acc & covered.get(last - t).map_or(all, |c| c.0)
            }),
        };
        Some(And(own | rest))
    };
    // A state is visited once, then once per fact a successor gets or bit it loses.
    let spec = Sparse {
        nodes: states.len(),
        readers: &readers,
        max_visits: 1 + 2 * (1 + 64),
    };
    let covered = fixpoint(budget, spec, transfer).map(|c| c.get(last).map_or(0, |c| c.0));
    // Unmetered and within the visit limit, the search is never open.
    covered.open_means(0)
}

/// The contracts announced after points of the project's functions ([`Self::after`]). What the
/// call sites from one on announce does not depend on the point the chain is entered from, so
/// it is kept per site while the summaries it reads stay the same: for the propagation of one
/// function, or one check of the rules.
pub struct CoveredAfter<'i> {
    index: &'i ProjectIndex,
    /// By file and call site: the contracts the calls from the site to the end of its chain
    /// announce.
    from: RefCell<FxHashMap<(&'i Path, u32), u64>>,
    steps: Budget,
}

impl<'i> CoveredAfter<'i> {
    pub fn new(index: &'i ProjectIndex) -> Self {
        Self {
            index,
            from: RefCell::default(),
            steps: Budget::unmetered("invalidation::CoveredAfter"),
        }
    }

    /// The contracts of `mask` announced after a point with `flow` in a function of `path`:
    /// by the function's own invalidator calls on every normal path, or by a project call on
    /// every such path whose callees announce them on every path of their own
    /// (`must_invalidate`): every function the call may run, an abstract method aside. A
    /// different helper in each branch is not combined.
    pub fn after(&self, path: &'i Path, flow: &SiteFlow, mask: u64) -> u64 {
        let later = flow.next_call.map_or(0, |site| self.from(path, site));
        (flow.invalidated | later) & mask
    }

    /// What the calls from `site` on announce. The chain is followed to the end or to a site
    /// already answered, then answered back to `site`.
    fn from(&self, path: &'i Path, site: u32) -> u64 {
        let file = &self.index.files[path];
        let mut chain = Vec::new();
        let mut next = Some(site);
        let mut known = 0;
        // `next_call` follows the post-dominator tree, so the chain is finite; the bound only
        // guards an index from another version of the file.
        for _ in 0..file.call_sites.len() {
            let Some(i) = next.filter(|i| (*i as usize) < file.call_sites.len()) else {
                break;
            };
            if let Some(&from) = self.from.borrow().get(&(path, i)) {
                known = from;
                break;
            }
            self.steps.take(1);
            chain.push(i);
            next = file.call_sites[i as usize].flow.and_then(|f| f.next_call);
        }
        let mut from = self.from.borrow_mut();
        for i in chain.into_iter().rev() {
            known |= self.announced_at(&file.call_sites[i as usize]);
            from.insert((path, i), known);
        }
        known
    }

    /// The contracts every function the call at `site` may run announces on every path. Only a
    /// body that runs at its call is known to run to its end: a generator may be iterated in
    /// part.
    fn announced_at(&self, site: &CallSite) -> u64 {
        let mut announced: Option<u64> = None;
        for bc in site.project_callees() {
            let here = match self.index.summary(&bc.callee) {
                Some(cs) if cs.is_abstract() => continue,
                Some(cs)
                    if body_runs_at(self.index, site, cs)
                        .is_some_and(|d| d.from == Facts::Body) =>
                {
                    cs.must_invalidate
                }
                _ => 0,
            };
            announced = Some(announced.map_or(here, |a| a & here));
        }
        announced.unwrap_or(0)
    }
}

/// Where the body of `callee` (where its `MUT109` writes and announcements are) runs at `site`
/// inside the body of the site's caller ([`ProjectIndex::delivery`]): a plain function, or a
/// coroutine awaited or driven, at its call; a generator where its result is iterated or handed
/// to a function that iterates it. What iterating a plain function's result runs
/// (`EffectSummary::iterated`) carries no `MUT109` facts, and a body that runs where the
/// caller's own result is iterated is not followed.
pub fn body_runs_at<'s>(
    index: &ProjectIndex,
    site: &'s CallSite,
    callee: &EffectSummary,
) -> Option<Delivery<'s>> {
    index
        .delivery(site, callee)
        .filter(|d| d.into == Facts::Body && (d.from == Facts::Body || callee.is_generator))
}

/// A private function (`_helper`, not a dunder) or a synthetic scope (`<lambda>`, `<genexpr>`,
/// `<listcomp>`): neither is an API of its own, so either hands its reports, and its
/// unannounced writes (`MUT109`), to its callers.
pub fn delegates_to_callers(qualname: &str) -> bool {
    let last = qualname.rsplit('.').next().unwrap_or(qualname);
    last.starts_with('<')
        || last.starts_with('_') && !(last.starts_with("__") && last.ends_with("__"))
}

/// Initializers that construct their own `self` (`MUT109` writes to it need no announcement).
pub const INITIALIZERS: &[&str] = &[
    "__init__",
    "__post_init__",
    "__new__",
    "model_post_init",
    "__setstate__",
];

/// Whether `func` is an initializer of its class.
pub fn is_initializer(func: &FuncKey) -> bool {
    INITIALIZERS.contains(&func.qualname.rsplit('.').next().unwrap_or(&func.qualname))
}

/// A class's own initialization: `func` (summary `s`) is an initializer method, and an object
/// that may be its parameters `obj_params` may be its own `self` (bit 0). An object that may also
/// be another one gets the benefit of the doubt. A helper's access reaches the initializer as one
/// of the object it bound to the helper's parameter, so the helpers it passes `self` to
/// initialize it too.
pub fn initializes_own_self(func: &FuncKey, s: &EffectSummary, obj_params: u64) -> bool {
    obj_params & 1 != 0 && is_initializer(func) && s.owner_class_qualname.is_some()
}

fn bits(mask: u64) -> impl Iterator<Item = u8> {
    (0..64u8).filter(move |k| mask & (1u64 << k) != 0)
}

/// Compute `must_invalidate` and `unmet_invalidation` of `func` from its flows and the current
/// summaries of its callees (`sites`: its outgoing call sites as `(file, index)`).
pub(crate) fn propagate_facts(
    index: &ProjectIndex,
    contracts: &dyn crate::Contracts,
    func: &FuncKey,
    sites: &[(std::path::PathBuf, usize)],
    s: &mut EffectSummary,
) {
    let n = contracts.invalidations();
    if n == 0 {
        return;
    }
    let all = contracts_mask(n);
    let path = &index.func_files[func];
    let flows = index.files[path].flows.get(func);
    let covers = CoveredAfter::new(index);
    // Every function summarized with a contract has a flow for each of its definitions.
    s.must_invalidate = flows.map_or(0, |fl| {
        fl.entries
            .iter()
            .fold(all, |acc, e| acc & covers.after(path, e, all))
    });
    let mut unmet: BTreeSet<UnmetWrite> = BTreeSet::new();
    // The function's own writes leave out the contracts it is an exempt writer of already.
    for w in flows.into_iter().flat_map(|fl| &fl.writes) {
        let mask = w.contracts;
        let open = mask & !covers.after(path, &w.flow, mask);
        for k in bits(open) {
            unmet.insert(UnmetWrite {
                contract: k,
                class_qualname: w.class_qualname.clone(),
                attr: w.attr.clone(),
                kind: w.kind,
                span: w.span,
                origin: w.span,
                via_chain: Vec::new(),
                reach: w.reach,
                obj_params: w.obj_params,
            });
        }
    }
    let initializer = is_initializer(func);
    // Matched against the patterns only when a call writes a target.
    let mut exempt = None;
    let mut exempt_of =
        || *exempt.get_or_insert_with(|| contracts.exempt_writer(&func.fqn(), &func.qualname));
    for (site_path, i) in sites {
        let site = &index.files[site_path].call_sites[*i];
        let Some(flow) = site.flow else {
            continue;
        };
        // Targets a callee writes through the arguments, which only this binding ties to a
        // class: the function's own writes, at the call.
        for bc in site.project_callees() {
            let Some(callee) = index.summary(&bc.callee) else {
                continue;
            };
            let Some(d) = body_runs_at(index, site, callee) else {
                continue;
            };
            let facts = callee
                .facts(d.from)
                .expect("the body `body_runs_at` finds is a plain function's or a generator's");
            let lead = crate::callgraph::lead_hops(site, site_path, &bc.callee, d.via);
            for w in crate::callgraph::indirect_writes(index, site, bc, callee, facts, &lead) {
                let Some(targets) = &w.origin.targets else {
                    continue;
                };
                let mask = match w.through {
                    Through::Field => targets.field,
                    Through::Instance => targets
                        .classes
                        .iter()
                        .find(|line| line[0].fqn == w.class)
                        .map_or(0, |line| contracts.target_of(line, w.attr)),
                };
                let reach = w.origin.reach;
                if mask == 0 || builds_written_object(initializer, reach, w.obj_params) {
                    continue;
                }
                let mask = mask & !exempt_of();
                let open = mask & !covers.after(site_path, &flow, mask);
                for k in bits(open) {
                    unmet.insert(UnmetWrite {
                        contract: k,
                        class_qualname: w.class.to_owned(),
                        attr: w.attr.to_owned(),
                        kind: w.kind,
                        span: site.span,
                        origin: site.span,
                        via_chain: w.via_chain(&lead),
                        reach,
                        obj_params: w.obj_params,
                    });
                }
            }
        }
        // Private helpers whose body runs here pass their unannounced writes up.
        let helpers = site
            .project_callees()
            .iter()
            .filter(|bc| delegates_to_callers(&bc.callee.qualname))
            .filter_map(|bc| {
                let callee = index.summary(&bc.callee)?;
                Some((bc, callee, body_runs_at(index, site, callee)?))
            });
        for (bc, callee, d) in helpers {
            for u in &callee.unmet_invalidation {
                let bit = 1u64 << u.contract;
                if exempt_of() & bit != 0
                    || u.via_chain.last().is_some_and(|h| h.callee == func.fqn())
                {
                    continue;
                }
                let reach = reach_across(u.reach, bc);
                let obj_params = bc.caller_params_of(u.obj_params);
                if builds_written_object(initializer, reach, obj_params) {
                    continue;
                }
                if covers.after(site_path, &flow, bit) & bit != 0 {
                    continue;
                }
                let mut via_chain = crate::callgraph::lead_hops(site, site_path, &bc.callee, d.via);
                via_chain.extend(u.via_chain.iter().cloned());
                unmet.insert(UnmetWrite {
                    contract: u.contract,
                    class_qualname: u.class_qualname.clone(),
                    attr: u.attr.clone(),
                    kind: u.kind,
                    span: site.span,
                    origin: u.origin,
                    via_chain,
                    reach,
                    obj_params,
                });
            }
        }
    }
    s.unmet_invalidation = shortest_chains(unmet);
}

/// Whether a function builds the object it writes, so the write announces nothing: an object
/// that did not exist before the call (`reach`: constructed, copied, or from a call), or, in an
/// initializer, its own `self` (`obj_params` bit 0: the object itself, not one reached through
/// its attributes).
pub fn builds_written_object(initializer: bool, reach: Reach, obj_params: u64) -> bool {
    !reach.any() || initializer && obj_params & 1 != 0
}

/// Where an object a callee reaches from its parameters (and module state) comes from in the
/// caller, through the arguments bound at the call.
pub fn reach_across(reach: Reach, bc: &crate::BoundCallee) -> Reach {
    bits(reach.params)
        .filter_map(|k| bc.params.get(usize::from(k)))
        .fold(
            Reach {
                params: 0,
                globals: reach.globals,
            },
            |acc, o| acc.join(o.reach),
        )
}

/// One unmet write per original write, call, and written object: the shortest chain of the
/// ones that reach it (diamonds of private helpers would otherwise multiply them).
fn shortest_chains(set: BTreeSet<UnmetWrite>) -> BTreeSet<UnmetWrite> {
    let key = |w: &UnmetWrite| {
        (
            w.contract,
            w.class_qualname.clone(),
            w.attr.clone(),
            w.kind,
            w.span,
            w.reach,
            w.obj_params,
            w.via_chain.last().map(|h| h.callee.clone()),
            w.origin,
        )
    };
    let mut all: Vec<UnmetWrite> = set.into_iter().collect();
    all.sort_by_key(|w| (key(w), w.via_chain.len()));
    all.dedup_by_key(|w| key(w));
    all.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::post_dominators;
    use heki_ir::walk::{Answer, Budget};

    /// The immediate post-dominators of `succ` from their definition: `d` post-dominates `b`
    /// when every path from `b` to the exit runs through `d`, and the nearest is the one every
    /// other strict post-dominator of `b` post-dominates.
    fn by_definition(succ: &[Option<Vec<usize>>]) -> Vec<Option<usize>> {
        let n = succ.len();
        let exit = n;
        let next = |b: usize| -> Vec<usize> { succ[b].clone().unwrap_or_else(|| vec![exit]) };
        // Whether a path from `b` reaches the exit without running through `avoid`.
        let escapes = |b: usize, avoid: Option<usize>| {
            let mut seen = vec![false; n + 1];
            let mut stack = vec![b];
            while let Some(x) = stack.pop() {
                if Some(x) == avoid || std::mem::replace(&mut seen[x], true) {
                    continue;
                }
                if x == exit {
                    return true;
                }
                stack.extend(next(x));
            }
            false
        };
        let pdoms = |b: usize| -> Vec<usize> {
            let mut out: Vec<usize> = (0..n)
                .filter(|d| *d != b && !escapes(b, Some(*d)))
                .collect();
            out.push(exit);
            out
        };
        (0..=n)
            .map(|b| {
                if b == exit {
                    return Some(exit);
                }
                if !escapes(b, None) {
                    return None;
                }
                let strict = pdoms(b);
                strict.iter().copied().find(|d| {
                    *d == exit && strict.len() == 1
                        || *d != exit && strict.iter().all(|o| o == d || pdoms(*d).contains(o))
                })
            })
            .collect()
    }

    #[test]
    fn post_dominators_are_those_every_path_to_a_return_runs_through() {
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut below = |m: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % m as u64) as usize
        };
        for _ in 0..3_000 {
            let n = 1 + below(10);
            let succ: Vec<Option<Vec<usize>>> = (0..n)
                .map(|_| match below(8) {
                    0 => None,
                    1 => Some(Vec::new()),
                    _ => Some((0..1 + below(2)).map(|_| below(n)).collect()),
                })
                .collect();
            let found = post_dominators(&succ, &Budget::unmetered("test"));
            assert_eq!(found, Answer::Known(by_definition(&succ)), "{succ:?}");
        }
    }

    #[test]
    fn a_deep_chain_of_blocks_is_numbered_without_recursion() {
        let n = 200_000;
        let succ: Vec<Option<Vec<usize>>> = (0..n)
            .map(|i| if i + 1 < n { Some(vec![i + 1]) } else { None })
            .collect();
        let found = post_dominators(&succ, &Budget::unmetered("test"));
        let ends = found.map(|ipdom| (ipdom[0], ipdom[n - 1]));
        assert_eq!(ends, Answer::Known((Some(1), Some(n))));
    }
}
