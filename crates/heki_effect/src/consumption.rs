//! Single-pass iterator consumption (`MUT106`): which SSA values are single-pass iterators,
//! which of them iterate other values lazily (`enumerate(it)` iterates `it` when it is
//! consumed), and where values are consumed. Shared by the local summary
//! (`EffectSummary::consumes_params`) and the rule.
//!
//! Every call is resolved through [`TypeCtx`], so a shadowed `list` or `iter` is never matched.

use crate::resolver::{Callee, TypeCtx, Val};
use heki_ast::Span;
use heki_ir::{BlockId, ConstLiteral, FunctionKind, InstKind, IrFunction, Terminator, VarId};
use rustc_hash::{FxHashMap, FxHashSet};

/// How a single-pass constructor iterates its positional arguments `first..end`
/// (`usize::MAX`: to the last).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Iterates {
    /// Lazily, each to exhaustion, when the result is consumed.
    Lazily,
    /// Lazily until the shortest one ends (`zip`, `map(f, a, b)`): only a sole iterable (or one
    /// iterator passed in every position, `zip(it, it)`) is exhausted;
    /// `zip(range(n), it)` takes `n` items of `it`.
    UntilShortest,
    /// Materialized when the result is built.
    Eagerly,
}

/// Builtin / `itertools` constructors returning a single-pass iterator, with the positional
/// arguments they iterate. `reversed`, `islice`, and `takewhile` iterate nothing to
/// exhaustion; `tee`, `repeat`, `cycle`, and `count` are not single-pass sources at all.
const CONSTRUCTORS: &[(&str, usize, usize, Iterates)] = &[
    ("builtins.iter", 0, 1, Iterates::Lazily),
    ("builtins.enumerate", 0, 1, Iterates::Lazily),
    ("builtins.zip", 0, usize::MAX, Iterates::UntilShortest),
    ("builtins.map", 1, usize::MAX, Iterates::UntilShortest),
    ("builtins.filter", 1, 2, Iterates::Lazily),
    ("builtins.reversed", 0, 0, Iterates::Lazily),
    ("itertools.chain", 0, usize::MAX, Iterates::Lazily),
    ("itertools.chain.from_iterable", 0, 1, Iterates::Lazily),
    ("itertools.islice", 0, 0, Iterates::Lazily),
    ("itertools.starmap", 1, 2, Iterates::Lazily),
    ("itertools.takewhile", 0, 0, Iterates::Lazily),
    ("itertools.dropwhile", 1, 2, Iterates::Lazily),
    ("itertools.filterfalse", 1, 2, Iterates::Lazily),
    ("itertools.compress", 0, 2, Iterates::UntilShortest),
    ("itertools.accumulate", 0, 1, Iterates::Lazily),
    ("itertools.groupby", 0, 1, Iterates::Lazily),
    ("itertools.pairwise", 0, 1, Iterates::Lazily),
    ("itertools.batched", 0, 1, Iterates::Lazily),
    ("itertools.zip_longest", 0, usize::MAX, Iterates::Lazily),
    ("itertools.product", 0, usize::MAX, Iterates::Eagerly),
    ("itertools.permutations", 0, 1, Iterates::Eagerly),
    ("itertools.combinations", 0, 1, Iterates::Eagerly),
    (
        "itertools.combinations_with_replacement",
        0,
        1,
        Iterates::Eagerly,
    ),
];

/// Builtins iterating their first positional argument to exhaustion (`min` / `max` only with a
/// single positional argument: `max(a, b)` compares two values).
const FULL_CONSUMERS: &[&str] = &[
    "builtins.list",
    "builtins.tuple",
    "builtins.set",
    "builtins.frozenset",
    "builtins.dict",
    "builtins.sum",
    "builtins.any",
    "builtins.all",
    "builtins.min",
    "builtins.max",
    "builtins.sorted",
];

/// Annotations naming a single-pass iterator (`Iterable`, `Sequence`, ... may be re-iterable).
const ITERATOR_TYPES: &[&str] = &[
    "typing.Iterator",
    "typing.Generator",
    "typing.AsyncIterator",
    "typing.AsyncGenerator",
    "collections.abc.Iterator",
    "collections.abc.Generator",
    "collections.abc.AsyncIterator",
    "collections.abc.AsyncGenerator",
];

/// The positional arguments of `args` a single-pass constructor iterates to exhaustion, and
/// whether it does so when the result is built. `iter(callable, sentinel)` iterates nothing.
fn constructor_args(symbol: &str, args: &[VarId]) -> Option<(Vec<VarId>, bool)> {
    let &(name, first, end, how) = CONSTRUCTORS.iter().find(|c| c.0 == symbol)?;
    let end = if name == "builtins.iter" && args.len() != 1 {
        0
    } else {
        end.min(args.len())
    };
    let mut iterated = args.get(first..end).unwrap_or_default().to_vec();
    if how == Iterates::UntilShortest && iterated.iter().any(|a| *a != iterated[0]) {
        iterated.clear();
    }
    Some((iterated, how == Iterates::Eagerly))
}

/// The argument a full consumer iterates to exhaustion (`list(it)`, `sum(it)`, ...).
fn consumer_arg(symbol: &str, args: &[VarId]) -> Option<VarId> {
    let single = !matches!(symbol, "builtins.min" | "builtins.max") || args.len() == 1;
    (FULL_CONSUMERS.contains(&symbol) && single)
        .then(|| args.first().copied())
        .flatten()
}

/// One consumption of tracked values by an instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Consumption {
    pub block: BlockId,
    /// Instruction index in `block`.
    pub index: usize,
    pub span: Span,
    /// Roots consumed, wrappers expanded (each root once, outermost first).
    pub roots: Vec<VarId>,
    /// Runs to exhaustion: anything but a `for` loop that can leave its body early.
    pub full: bool,
    /// First program point after the exhaustion: `(block, instruction index)`.
    pub done: (BlockId, usize),
}

/// Tracked values of one function and their consumptions.
#[derive(Debug, Default)]
pub struct IterFlow {
    /// Tracked value -> its root (a root maps to itself): parameters and single-pass sources,
    /// followed through `Copy` and phis whose other inputs share one root.
    pub root_of: FxHashMap<VarId, VarId>,
    /// Lazy wrappers: root -> the values it iterates when it is consumed.
    pub wraps: FxHashMap<VarId, Vec<VarId>>,
    /// Defining position of each instruction-defined root (parameters have none).
    pub def_at: FxHashMap<VarId, (BlockId, usize)>,
    /// Resolved callee of every `Call` / `MethodCall` by position.
    pub callees: FxHashMap<(BlockId, usize), Callee>,
    pub events: Vec<Consumption>,
}

/// The callee of a call instruction resolved through the receiver types (`Unresolved` for any
/// other instruction).
pub fn resolve_inst(tcx: &TypeCtx<'_>, types: &FxHashMap<VarId, Val>, kind: &InstKind) -> Callee {
    match kind {
        InstKind::Call { callee_var, .. } => {
            tcx.resolve_call(callee_var.and_then(|v| types.get(&v)))
        }
        InstKind::MethodCall { recv, method, .. } => tcx.resolve_method(types.get(recv), method),
        _ => Callee::Unresolved,
    }
}

impl IterFlow {
    /// Analyze `f`. Every parameter is a root with `all_params`, else only those annotated as
    /// iterators ([`ITERATOR_TYPES`]).
    pub fn new(
        f: &IrFunction,
        tcx: &TypeCtx<'_>,
        types: &FxHashMap<VarId, Val>,
        all_params: bool,
    ) -> Self {
        let mut flow = Self::default();
        for p in &f.params {
            let annotated = p
                .annotation
                .as_deref()
                .is_some_and(|a| is_iterator_annotation(tcx, a));
            if all_params || annotated {
                flow.root_of.insert(p.var, p.var);
            }
        }
        for bb in &f.blocks {
            for (index, inst) in bb.insts.iter().enumerate() {
                let (dst, args) = match &inst.kind {
                    InstKind::MakeGenExpr {
                        dst, outer_iter, ..
                    } => {
                        flow.add_root(*dst, (bb.id, index), vec![*outer_iter]);
                        continue;
                    }
                    InstKind::Call { dst, args, .. } | InstKind::MethodCall { dst, args, .. } => {
                        (dst, args)
                    }
                    _ => continue,
                };
                let callee = resolve_inst(tcx, types, &inst.kind);
                if let Some(wrapped) = source_wraps(tcx, &callee, args) {
                    flow.add_root(*dst, (bb.id, index), wrapped);
                }
                flow.callees.insert((bb.id, index), callee);
            }
        }
        flow.alias(f);
        // Locals annotated as iterators that are not already tracked.
        let annotated: FxHashSet<&str> = f
            .local_annotations
            .iter()
            .filter(|a| !a.target.contains('.') && is_iterator_annotation(tcx, &a.annotation))
            .map(|a| a.target.as_str())
            .collect();
        for bb in &f.blocks {
            for (index, inst) in bb.insts.iter().enumerate() {
                if let InstKind::Copy { dst, .. } = &inst.kind
                    && !flow.root_of.contains_key(dst)
                    && f.var_name(*dst).is_some_and(|n| annotated.contains(n))
                {
                    flow.add_root(*dst, (bb.id, index), Vec::new());
                }
            }
        }
        flow.alias(f);
        flow.collect_events(f, types);
        flow
    }

    fn add_root(&mut self, var: VarId, at: (BlockId, usize), wrapped: Vec<VarId>) {
        self.root_of.insert(var, var);
        self.def_at.insert(var, at);
        self.wraps.insert(var, wrapped);
    }

    /// `Copy` and agreeing-phi aliases of the roots, to a fixpoint.
    fn alias(&mut self, f: &IrFunction) {
        loop {
            let before = self.root_of.len();
            for bb in &f.blocks {
                for phi in &bb.phis {
                    let mut inputs = phi
                        .incoming
                        .iter()
                        .map(|(_, v)| *v)
                        .filter(|v| *v != phi.dst);
                    let first = inputs.next().and_then(|v| self.root_of.get(&v).copied());
                    if let Some(r) = first
                        && inputs.all(|v| self.root_of.get(&v) == Some(&r))
                    {
                        self.root_of.entry(phi.dst).or_insert(r);
                    }
                }
                for inst in &bb.insts {
                    if let InstKind::Copy { dst, src } = &inst.kind
                        && let Some(&r) = self.root_of.get(src)
                    {
                        self.root_of.entry(*dst).or_insert(r);
                    }
                }
            }
            if self.root_of.len() == before {
                return;
            }
        }
    }

    /// Roots `values` consume: each value's root, then (pre-order) the values its wrappers
    /// iterate. A root consumed twice by one instruction (`zip(it, it)`) counts once.
    pub fn expand(&self, values: &[VarId]) -> Vec<VarId> {
        let mut out = Vec::new();
        let mut stack: Vec<VarId> = values.iter().rev().copied().collect();
        while let Some(v) = stack.pop() {
            if let Some(&r) = self.root_of.get(&v)
                && !out.contains(&r)
            {
                out.push(r);
                stack.extend(self.wraps.get(&r).into_iter().flatten().rev());
            }
        }
        out
    }

    /// Record that the instruction at `(block, index)` iterates `values` to exhaustion.
    pub fn add_full(&mut self, block: BlockId, index: usize, span: Span, values: &[VarId]) {
        self.push(block, index, span, values, true, (block, index + 1));
    }

    fn push(
        &mut self,
        block: BlockId,
        index: usize,
        span: Span,
        values: &[VarId],
        full: bool,
        done: (BlockId, usize),
    ) {
        let roots = self.expand(values);
        if !roots.is_empty() {
            self.events.push(Consumption {
                block,
                index,
                span,
                roots,
                full,
                done,
            });
        }
    }

    fn collect_events(&mut self, f: &IrFunction, types: &FxHashMap<VarId, Val>) {
        for bb in &f.blocks {
            for (index, inst) in bb.insts.iter().enumerate() {
                let consumed: Vec<VarId> = match &inst.kind {
                    InstKind::MakeIter { dst, iterable, .. } => {
                        if let Some((exhausted, full)) = loop_exhaustion(f, *dst) {
                            let done = (exhausted, 0);
                            self.push(bb.id, index, inst.span, &[*iterable], full, done);
                        }
                        continue;
                    }
                    InstKind::YieldFrom { value, .. } => vec![*value],
                    // Non-inlined comprehension (Python 3.11): iterates its first argument.
                    InstKind::Call {
                        callee_var: None,
                        args,
                        ..
                    } => args.first().copied().into_iter().collect(),
                    InstKind::MethodCall {
                        recv, method, args, ..
                    } if method == "join" && args.len() == 1 && is_str(f, types, *recv) => {
                        args.clone()
                    }
                    InstKind::Call { args, .. } | InstKind::MethodCall { args, .. } => {
                        match &self.callees[&(bb.id, index)] {
                            Callee::External(symbol) => match constructor_args(symbol, args) {
                                Some((wrapped, true)) => wrapped,
                                _ => consumer_arg(symbol, args).into_iter().collect(),
                            },
                            _ => continue,
                        }
                    }
                    _ => continue,
                };
                self.add_full(bb.id, index, inst.span, &consumed);
            }
        }
    }
}

/// The values a resolved call's result iterates when it is consumed, when that result is a
/// single-pass source: a single-pass constructor or a project generator function (every CHA
/// candidate must be one).
fn source_wraps(tcx: &TypeCtx<'_>, callee: &Callee, args: &[VarId]) -> Option<Vec<VarId>> {
    match callee {
        Callee::External(symbol) => constructor_args(symbol, args)
            .map(|(wrapped, eager)| if eager { Vec::new() } else { wrapped }),
        Callee::Project(keys, _) => keys
            .iter()
            .all(|k| tcx.project.function_decl(k).is_some_and(|d| d.is_generator))
            .then(Vec::new),
        Callee::TooMany | Callee::Unresolved => None,
    }
}

fn is_iterator_annotation(tcx: &TypeCtx<'_>, annotation: &str) -> bool {
    tcx.module
        .resolve_annotation(annotation)
        .is_some_and(|t| ITERATOR_TYPES.contains(&t.as_str()))
}

/// `var` holds a `str`: a string constant (through copies) or a value typed `str`.
fn is_str(f: &IrFunction, types: &FxHashMap<VarId, Val>, var: VarId) -> bool {
    let mut cur = var;
    while let Some((_, inst)) = f.def_inst(cur) {
        match &inst.kind {
            InstKind::Copy { src, .. } => cur = *src,
            InstKind::Const {
                literal: ConstLiteral::Str(_),
                ..
            } => return true,
            _ => break,
        }
    }
    matches!(types.get(&var), Some(Val::Inst(t)) if t == "builtins.str")
}

/// For the iterator `iter` a `MakeIter` defines: the block entered when its loop is exhausted,
/// and whether the loop always runs to exhaustion (no `break`, `return`, or `raise` inside it).
fn loop_exhaustion(f: &IrFunction, iter: VarId) -> Option<(BlockId, bool)> {
    let lp = f.loops.iter().find(|lp| {
        f.block(lp.header)
            .insts
            .iter()
            .any(|i| matches!(i.kind, InstKind::IterNext { iter: it, .. } if it == iter))
    })?;
    let header = &f.block(lp.header).terminator;
    let exhausted = header
        .successors()
        .into_iter()
        .find(|s| !lp.body.contains(s))?;
    let leaves_early = lp.body.iter().filter(|b| **b != lp.header).any(|b| {
        let t = &f.block(*b).terminator;
        matches!(t, Terminator::Return(_) | Terminator::Raise(_))
            || t.successors().iter().any(|s| !lp.body.contains(s))
    });
    Some((exhausted, !leaves_early))
}

/// Whether every normal-flow path from the entry to a `return` passes through `block`.
fn runs_on_every_return(f: &IrFunction, block: BlockId) -> bool {
    let entry = BlockId(0);
    let mut seen: FxHashSet<BlockId> = FxHashSet::default();
    let mut stack = vec![entry];
    while let Some(b) = stack.pop() {
        if b == block || !seen.insert(b) {
            continue;
        }
        let t = &f.block(b).terminator;
        if matches!(t, Terminator::Return(_)) {
            return false;
        }
        stack.extend(t.successors());
    }
    true
}

/// `EffectSummary::consumes_params` and `unconditional_forwards` of `f` (local facts):
/// parameters a full consumption on every normal path iterates, and parameters passed to
/// project calls only where the call runs on every normal path. A generator function's body
/// does not run at the call: both are empty.
pub(crate) fn param_consumption(
    f: &IrFunction,
    tcx: &TypeCtx<'_>,
    types: &FxHashMap<VarId, Val>,
) -> (u64, u64) {
    if f.params.is_empty()
        || matches!(
            f.kind,
            FunctionKind::Generator | FunctionKind::AsyncGenerator
        )
    {
        return (0, 0);
    }
    let flow = IterFlow::new(f, tcx, types, true);
    let mut must: FxHashMap<BlockId, bool> = FxHashMap::default();
    let mut runs = |b: BlockId| *must.entry(b).or_insert_with(|| runs_on_every_return(f, b));
    let bits = |roots: &[VarId]| {
        let mut out = 0u64;
        for (i, p) in f.params.iter().take(64).enumerate() {
            if roots.contains(&p.var) {
                out |= 1u64 << i;
            }
        }
        out
    };
    let mut consumes = 0;
    for e in flow.events.iter().filter(|e| e.full) {
        if runs(e.block) {
            consumes |= bits(&e.roots);
        }
    }
    let mut conditional = 0;
    for bb in &f.blocks {
        for (index, inst) in bb.insts.iter().enumerate() {
            let project = match &inst.kind {
                // A non-inlined comprehension (Python 3.11) is a project call.
                InstKind::Call {
                    callee_var: None, ..
                } => true,
                _ => matches!(flow.callees.get(&(bb.id, index)), Some(Callee::Project(..))),
            };
            if project && !runs(bb.id) {
                conditional |= bits(&flow.expand(&inst.kind.operands()));
            }
        }
    }
    let params: Vec<VarId> = f.params.iter().map(|p| p.var).collect();
    (consumes, bits(&params) & !conditional)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(symbol, positional argument count, (iterated argument indices, eagerly))`.
    type Case = (&'static str, usize, Option<(&'static [u32], bool)>);

    #[test]
    fn constructors_iterate_their_documented_arguments() {
        let v: Vec<VarId> = (0..4).map(VarId).collect();
        let cases: &[Case] = &[
            ("builtins.iter", 1, Some((&[0], false))),
            ("builtins.iter", 2, Some((&[], false))),
            ("builtins.enumerate", 2, Some((&[0], false))),
            // `zip` / `map` over several iterables stop at the shortest: none is exhausted.
            ("builtins.zip", 3, Some((&[], false))),
            ("builtins.zip", 1, Some((&[0], false))),
            ("builtins.map", 3, Some((&[], false))),
            ("builtins.map", 2, Some((&[1], false))),
            ("builtins.map", 0, Some((&[], false))),
            ("itertools.chain", 3, Some((&[0, 1, 2], false))),
            ("itertools.zip_longest", 2, Some((&[0, 1], false))),
            ("builtins.filter", 2, Some((&[1], false))),
            ("builtins.reversed", 1, Some((&[], false))),
            ("itertools.islice", 3, Some((&[], false))),
            ("itertools.takewhile", 2, Some((&[], false))),
            ("itertools.compress", 2, Some((&[], false))),
            ("itertools.product", 2, Some((&[0, 1], true))),
            ("itertools.combinations", 2, Some((&[0], true))),
            ("itertools.filterfalse", 1, Some((&[], false))),
            ("itertools.tee", 1, None),
            ("itertools.cycle", 1, None),
            ("itertools.repeat", 1, None),
            ("itertools.count", 0, None),
            ("builtins.list", 1, None),
        ];
        for &(symbol, n, expected) in cases {
            let got = constructor_args(symbol, &v[..n]);
            let expected = expected
                .map(|(idx, eager)| (idx.iter().map(|i| VarId(*i)).collect::<Vec<_>>(), eager));
            assert_eq!(got, expected, "{symbol}/{n}");
        }
        // One iterator in every position (`zip(it, it)`, the pairing idiom) is exhausted.
        let it = [VarId(7), VarId(7)];
        assert_eq!(
            constructor_args("builtins.zip", &it),
            Some((it.to_vec(), false))
        );
    }

    #[test]
    fn consumers_take_their_first_argument() {
        let v: Vec<VarId> = (0..2).map(VarId).collect();
        assert_eq!(consumer_arg("builtins.list", &v[..1]), Some(VarId(0)));
        assert_eq!(consumer_arg("builtins.sorted", &v), Some(VarId(0)));
        assert_eq!(consumer_arg("builtins.max", &v[..1]), Some(VarId(0)));
        // `max(a, b)` compares two values; `list()` has nothing to consume.
        assert_eq!(consumer_arg("builtins.max", &v), None);
        assert_eq!(consumer_arg("builtins.min", &v), None);
        assert_eq!(consumer_arg("builtins.list", &[]), None);
        assert_eq!(consumer_arg("builtins.len", &v[..1]), None);
        assert_eq!(consumer_arg("builtins.next", &v[..1]), None);
    }
}
