//! Where iterators are iterated: which SSA values are single-pass iterators, which of them
//! iterate other values lazily (`enumerate(it)` iterates `it` when it is iterated), and where
//! values are iterated. The one table of iterating calls, shared by `MUT106` (a second pass over
//! an exhausted iterator), the local summary (`EffectSummary::consumes_params` /
//! `iterates_params`), and where a generator's body runs (`summary`: the `Iterate`,
//! `Return`, and `Through` call-site roles).
//!
//! An iteration *steps* every value it reaches through lazy wrappers (`zip(range(n), it)` steps
//! `it`) and *exhausts* only some of them (`zip(range(n), it)` takes `n` items of `it`):
//! `MUT106` needs exhaustion, a generator's body runs at any step.
//!
//! Every call is resolved through [`TypeCtx`], so a shadowed `list` or `iter` is never matched.

use crate::resolver::{Callee, TypeCtx, Val};
use heki_ast::Span;
use heki_ir::{BlockId, ConstLiteral, FunctionKind, Inst, InstKind, IrFunction, Terminator, VarId};
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
    /// Lazily, stopping at a count or a condition (`islice`, `takewhile`): stepped, never
    /// exhausted.
    Partially,
    /// Materialized when the result is built.
    Eagerly,
}

/// Builtin / `itertools` / `heapq` constructors returning a single-pass iterator, with the
/// positional arguments they iterate. `reversed` takes a sequence (a generator is not
/// reversible), so it iterates no argument; `tee`, `repeat`, `cycle`, and `count` are not
/// single-pass sources at all.
const CONSTRUCTORS: &[(&str, usize, usize, Iterates)] = &[
    ("builtins.iter", 0, 1, Iterates::Lazily),
    ("builtins.enumerate", 0, 1, Iterates::Lazily),
    ("builtins.zip", 0, usize::MAX, Iterates::UntilShortest),
    ("builtins.map", 1, usize::MAX, Iterates::UntilShortest),
    ("builtins.filter", 1, 2, Iterates::Lazily),
    ("builtins.reversed", 0, 0, Iterates::Lazily),
    ("itertools.chain", 0, usize::MAX, Iterates::Lazily),
    ("itertools.chain.from_iterable", 0, 1, Iterates::Lazily),
    ("itertools.islice", 0, 1, Iterates::Partially),
    ("itertools.starmap", 1, 2, Iterates::Lazily),
    ("itertools.takewhile", 1, 2, Iterates::Partially),
    ("itertools.dropwhile", 1, 2, Iterates::Lazily),
    ("itertools.filterfalse", 1, 2, Iterates::Lazily),
    ("itertools.compress", 0, 2, Iterates::UntilShortest),
    ("itertools.accumulate", 0, 1, Iterates::Lazily),
    ("itertools.groupby", 0, 1, Iterates::Lazily),
    ("itertools.pairwise", 0, 1, Iterates::Lazily),
    ("itertools.batched", 0, 1, Iterates::Lazily),
    ("itertools.zip_longest", 0, usize::MAX, Iterates::Lazily),
    ("heapq.merge", 0, usize::MAX, Iterates::Lazily),
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

/// Which positional arguments a full consumer iterates to exhaustion.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Consumes {
    /// The first (`list(it)`, `xs.extend(it)`).
    First,
    /// The second (`heapq.nlargest(n, it)`, `functools.reduce(f, it)`).
    Second,
    /// Every one (`s.update(a, b)`).
    All,
    /// The first when it is the only one (`max(it)`; `max(a, b)` compares two values).
    Sole,
}

/// Calls iterating an iterable argument to exhaustion when they run: builtins, methods of
/// builtin and `collections` containers (`builtins.list.extend` is `xs.extend(...)` on a
/// value typed `list`), and standard-library reducers. `str.join` / `bytes.join` on a literal
/// are matched by their receiver.
const FULL_CONSUMERS: &[(&str, Consumes)] = &[
    ("builtins.list", Consumes::First),
    ("builtins.tuple", Consumes::First),
    ("builtins.set", Consumes::First),
    ("builtins.frozenset", Consumes::First),
    ("builtins.dict", Consumes::First),
    ("builtins.bytes", Consumes::First),
    ("builtins.bytearray", Consumes::First),
    ("builtins.sum", Consumes::First),
    ("builtins.min", Consumes::Sole),
    ("builtins.max", Consumes::Sole),
    ("builtins.sorted", Consumes::First),
    ("builtins.dict.fromkeys", Consumes::First),
    ("builtins.dict.update", Consumes::First),
    ("builtins.list.extend", Consumes::First),
    ("builtins.bytearray.extend", Consumes::First),
    ("builtins.set.update", Consumes::All),
    ("builtins.set.union", Consumes::All),
    ("builtins.set.intersection", Consumes::All),
    ("builtins.set.difference", Consumes::All),
    ("builtins.str.join", Consumes::First),
    ("builtins.bytes.join", Consumes::First),
    ("collections.deque", Consumes::First),
    ("collections.deque.extend", Consumes::First),
    ("collections.deque.extendleft", Consumes::First),
    ("collections.Counter", Consumes::First),
    ("collections.Counter.update", Consumes::First),
    ("collections.OrderedDict", Consumes::First),
    ("collections.defaultdict", Consumes::Second),
    ("heapq.nlargest", Consumes::Second),
    ("heapq.nsmallest", Consumes::Second),
    ("functools.reduce", Consumes::Second),
    ("math.prod", Consumes::First),
    ("math.fsum", Consumes::First),
];

/// Calls stepping their first argument until an item decides the result (`any` at the first true
/// item, `all` at the first false one): they exhaust it only when no item does.
const STOPPING_CONSUMERS: &[&str] = &["builtins.any", "builtins.all"];

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

/// The positional arguments a single-pass constructor iterates.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Wrapped {
    /// Every argument an iteration of the result steps.
    stepped: Vec<VarId>,
    /// The arguments an exhausting iteration of the result exhausts (a subset of `stepped`).
    exhausted: Vec<VarId>,
    /// Iterated when the result is built (`itertools.product`), not when it is iterated.
    eager: bool,
}

/// What a single-pass constructor iterates of `args`. `iter(callable, sentinel)` iterates
/// nothing.
fn constructor_args(symbol: &str, args: &[VarId]) -> Option<Wrapped> {
    let &(name, first, end, how) = CONSTRUCTORS.iter().find(|c| c.0 == symbol)?;
    let end = if name == "builtins.iter" && args.len() != 1 {
        0
    } else {
        end.min(args.len())
    };
    let stepped = args.get(first..end).unwrap_or_default().to_vec();
    let exhausted = match how {
        Iterates::Partially => Vec::new(),
        Iterates::UntilShortest if stepped.iter().any(|a| *a != stepped[0]) => Vec::new(),
        _ => stepped.clone(),
    };
    Some(Wrapped {
        stepped,
        exhausted,
        eager: how == Iterates::Eagerly,
    })
}

/// The arguments a full consumer iterates to exhaustion (`list(it)`, `sum(it)`, ...).
fn consumer_args(symbol: &str, args: &[VarId]) -> Vec<VarId> {
    let Some(&(_, how)) = FULL_CONSUMERS.iter().find(|c| c.0 == symbol) else {
        return Vec::new();
    };
    match how {
        Consumes::First => args.first().copied().into_iter().collect(),
        Consumes::Second => args.get(1).copied().into_iter().collect(),
        Consumes::All => args.to_vec(),
        Consumes::Sole if args.len() == 1 => args.to_vec(),
        Consumes::Sole => Vec::new(),
    }
}

/// The arguments a call of the external `symbol` iterates when it runs: an eager constructor's
/// (`itertools.product(a, b)`), a full consumer's (`list(it)`, `xs.extend(it)`), or the first
/// one of a consumer that may stop early (`any(it)`: it steps it, without exhausting it).
pub(crate) fn iterated_args(symbol: &str, args: &[VarId]) -> Vec<VarId> {
    if STOPPING_CONSUMERS.contains(&symbol) {
        return args[..args.len().min(1)].to_vec();
    }
    match constructor_args(symbol, args) {
        Some(w) if w.eager => w.stepped,
        _ => consumer_args(symbol, args),
    }
}

/// One iteration of tracked values by an instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Consumption {
    pub block: BlockId,
    /// Instruction index in `block`.
    pub index: usize,
    pub span: Span,
    /// Roots exhausted, wrappers expanded (each root once, outermost first).
    pub roots: Vec<VarId>,
    /// Every root the iteration steps, wrappers expanded (a superset of `roots`).
    pub stepped: Vec<VarId>,
    /// Runs to exhaustion: anything but a `for` loop that can leave its body early.
    pub full: bool,
    /// First program point after the exhaustion: `(block, instruction index)`.
    pub done: (BlockId, usize),
}

/// Which values a flow tracks besides single-pass sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Roots {
    /// `MUT106`: parameters and locals annotated as iterators.
    SinglePass,
    /// The local summary: every parameter (`consumes_params` / `iterates_params`) and every
    /// project call's result (where a returned generator is iterated, returned, or passed on).
    Summary,
}

/// Tracked values of one function and their iterations.
#[derive(Debug, Default)]
pub struct IterFlow {
    /// Tracked value -> its root (a root maps to itself): parameters and single-pass sources,
    /// followed through `Copy` and phis whose other inputs share one root.
    pub root_of: FxHashMap<VarId, VarId>,
    /// Values merged from different roots (`rows() if flag else other()`, through copies): the
    /// roots each may be. Iterating one steps them all; it exhausts none (it is one of them).
    pub may_roots: FxHashMap<VarId, Vec<VarId>>,
    /// Lazy wrappers: root -> the values it exhausts when it is consumed.
    pub wraps: FxHashMap<VarId, Vec<VarId>>,
    /// Lazy wrappers: root -> every value it steps when it is iterated (`wraps` and the values
    /// it may stop early in: `zip(range(n), it)` steps `it`).
    pub steps: FxHashMap<VarId, Vec<VarId>>,
    /// Defining position of each instruction-defined root (parameters have none).
    pub def_at: FxHashMap<VarId, (BlockId, usize)>,
    /// Resolved callee of every `Call` / `MethodCall` by position.
    pub callees: FxHashMap<(BlockId, usize), Callee>,
    pub events: Vec<Consumption>,
    /// Roots known to be iterators: subscripting one is an unpacking (`a, b = it`). A parameter
    /// without an iterator annotation may be a sequence.
    unpackable: FxHashSet<VarId>,
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
    /// Analyze `f`, resolving its calls.
    pub fn new(
        f: &IrFunction,
        tcx: &TypeCtx<'_>,
        types: &FxHashMap<VarId, Val>,
        roots: Roots,
    ) -> Self {
        let mut callees = FxHashMap::default();
        for bb in &f.blocks {
            for (index, inst) in bb.insts.iter().enumerate() {
                if matches!(
                    inst.kind,
                    InstKind::Call { .. } | InstKind::MethodCall { .. }
                ) {
                    callees.insert((bb.id, index), resolve_inst(tcx, types, &inst.kind));
                }
            }
        }
        Self::with_callees(f, tcx, types, roots, callees)
    }

    /// Analyze `f` with the callee of every `Call` / `MethodCall` already resolved.
    pub fn with_callees(
        f: &IrFunction,
        tcx: &TypeCtx<'_>,
        types: &FxHashMap<VarId, Val>,
        roots: Roots,
        callees: FxHashMap<(BlockId, usize), Callee>,
    ) -> Self {
        let mut flow = Self {
            callees,
            ..Self::default()
        };
        for p in &f.params {
            let annotated = p
                .annotation
                .as_deref()
                .is_some_and(|a| is_iterator_annotation(tcx, a));
            if annotated {
                flow.unpackable.insert(p.var);
            }
            if roots == Roots::Summary || annotated {
                flow.root_of.insert(p.var, p.var);
            }
        }
        for bb in &f.blocks {
            for (index, inst) in bb.insts.iter().enumerate() {
                let (dst, args) = match &inst.kind {
                    InstKind::MakeGenExpr {
                        dst, outer_iter, ..
                    } => {
                        let w = vec![*outer_iter];
                        flow.add_root(*dst, (bb.id, index), w.clone(), w);
                        continue;
                    }
                    InstKind::Call { dst, args, .. } | InstKind::MethodCall { dst, args, .. } => {
                        (dst, args)
                    }
                    // A read running property getters is a call of them (`EffectSummary`
                    // flows record it with the getters as its callee).
                    InstKind::AttrLoad { dst, .. } => {
                        if roots == Roots::Summary
                            && matches!(
                                flow.callees.get(&(bb.id, index)),
                                Some(Callee::Project(..))
                            )
                        {
                            flow.add_root(*dst, (bb.id, index), Vec::new(), Vec::new());
                        }
                        continue;
                    }
                    _ => continue,
                };
                let callee = &flow.callees[&(bb.id, index)];
                if let Some(w) = source_wraps(tcx, callee, args) {
                    flow.add_root(*dst, (bb.id, index), w.exhausted, w.stepped);
                } else if roots == Roots::Summary && matches!(callee, Callee::Project(..)) {
                    flow.add_root(*dst, (bb.id, index), Vec::new(), Vec::new());
                }
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
                    flow.add_root(*dst, (bb.id, index), Vec::new(), Vec::new());
                }
            }
        }
        flow.alias(f);
        flow.collect_events(f, types);
        flow
    }

    /// An instruction-defined root: an iterator, so subscripting it unpacks it.
    fn add_root(
        &mut self,
        var: VarId,
        at: (BlockId, usize),
        exhausted: Vec<VarId>,
        stepped: Vec<VarId>,
    ) {
        self.root_of.insert(var, var);
        self.def_at.insert(var, at);
        self.wraps.insert(var, exhausted);
        self.steps.insert(var, stepped);
        self.unpackable.insert(var);
    }

    /// `Copy` and agreeing-phi aliases of the roots, to a fixpoint.
    fn alias(&mut self, f: &IrFunction) {
        loop {
            let before = (
                self.root_of.len(),
                self.may_roots.values().map(Vec::len).sum::<usize>(),
            );
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
                        continue;
                    }
                    let mut may: Vec<VarId> = phi
                        .incoming
                        .iter()
                        .filter(|(_, v)| *v != phi.dst)
                        .flat_map(|(_, v)| self.possible_roots(*v))
                        .collect();
                    may.sort();
                    may.dedup();
                    if !may.is_empty() {
                        self.may_roots.insert(phi.dst, may);
                    }
                }
                for inst in &bb.insts {
                    if let InstKind::Copy { dst, src } = &inst.kind {
                        if let Some(&r) = self.root_of.get(src) {
                            self.root_of.entry(*dst).or_insert(r);
                        } else if let Some(may) = self.may_roots.get(src).cloned() {
                            self.may_roots.insert(*dst, may);
                        }
                    }
                }
            }
            let after = (
                self.root_of.len(),
                self.may_roots.values().map(Vec::len).sum::<usize>(),
            );
            if after == before {
                return;
            }
        }
    }

    /// The roots `v` is or may be.
    fn possible_roots(&self, v: VarId) -> Vec<VarId> {
        match self.root_of.get(&v) {
            Some(r) => vec![*r],
            None => self.may_roots.get(&v).cloned().unwrap_or_default(),
        }
    }

    /// Roots `values` exhaust: each value's root, then (pre-order) the values its wrappers
    /// exhaust. A root consumed twice by one instruction (`zip(it, it)`) counts once.
    pub fn expand(&self, values: &[VarId]) -> Vec<VarId> {
        expand_through(&self.root_of, &self.wraps, values)
    }

    /// Roots an iteration of `values` steps: each value's root (the roots a merged value may
    /// be), then (pre-order) every value its wrappers step.
    pub fn expand_steps(&self, values: &[VarId]) -> Vec<VarId> {
        let values: Vec<VarId> = values
            .iter()
            .flat_map(|v| match self.may_roots.get(v) {
                Some(roots) if !self.root_of.contains_key(v) => roots.clone(),
                _ => vec![*v],
            })
            .collect();
        expand_through(&self.root_of, &self.steps, &values)
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
        let stepped = self.expand_steps(values);
        if !stepped.is_empty() {
            self.events.push(Consumption {
                block,
                index,
                span,
                roots,
                stepped,
                full,
                done,
            });
        }
    }

    fn collect_events(&mut self, f: &IrFunction, types: &FxHashMap<VarId, Val>) {
        // `a, b = it` reads `it[0]` and `it[1]`: one unpacking per root and block.
        let mut unpacked: FxHashSet<(BlockId, VarId)> = FxHashSet::default();
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
                    InstKind::SubscriptLoad { obj, index, .. }
                        if is_unpacking(f, *index, inst.span) =>
                    {
                        match self.root_of.get(obj) {
                            Some(r)
                                if self.unpackable.contains(r) && unpacked.insert((bb.id, *r)) =>
                            {
                                vec![*obj]
                            }
                            _ => continue,
                        }
                    }
                    // Non-inlined comprehension (Python 3.11): iterates its first argument.
                    InstKind::Call {
                        callee_var: None,
                        args,
                        ..
                    } => args.first().copied().into_iter().collect(),
                    InstKind::MethodCall {
                        recv, method, args, ..
                    } if method == "join" && args.len() == 1 && is_text(f, types, *recv) => {
                        args.clone()
                    }
                    InstKind::Call { args, .. } | InstKind::MethodCall { args, .. } => {
                        match &self.callees[&(bb.id, index)] {
                            Callee::External(symbol)
                                if STOPPING_CONSUMERS.contains(&symbol.as_str()) =>
                            {
                                let first = &args[..args.len().min(1)];
                                let done = (bb.id, index + 1);
                                self.push(bb.id, index, inst.span, first, false, done);
                                continue;
                            }
                            Callee::External(symbol) => iterated_args(symbol, args),
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

/// Whether the subscript at `span` with index `index` is one the builder emits for target
/// unpacking (`a, b = it` reads `it[0]` and `it[1]`): a constant integer index lowered at the
/// target element's own location. A subscript written in the source (`it["row"]`, `it[0]`)
/// lowers its index at the index expression.
pub fn is_unpacking(f: &IrFunction, index: VarId, span: Span) -> bool {
    matches!(
        f.def_inst(index),
        Some((_, Inst { kind: InstKind::Const { literal: ConstLiteral::Int(_), .. }, span: at, .. }))
            if *at == span
    )
}

/// Roots `values` reach through `edges` (pre-order, each root once).
fn expand_through(
    root_of: &FxHashMap<VarId, VarId>,
    edges: &FxHashMap<VarId, Vec<VarId>>,
    values: &[VarId],
) -> Vec<VarId> {
    let mut out = Vec::new();
    let mut stack: Vec<VarId> = values.iter().rev().copied().collect();
    while let Some(v) = stack.pop() {
        if let Some(&r) = root_of.get(&v)
            && !out.contains(&r)
        {
            out.push(r);
            stack.extend(edges.get(&r).into_iter().flatten().rev());
        }
    }
    out
}

/// What a resolved call's result iterates when it is iterated, when that result is a
/// single-pass source: a single-pass constructor or a project generator function (every CHA
/// candidate must be one).
fn source_wraps(tcx: &TypeCtx<'_>, callee: &Callee, args: &[VarId]) -> Option<Wrapped> {
    match callee {
        Callee::External(symbol) => constructor_args(symbol, args).map(|w| {
            if w.eager {
                Wrapped {
                    stepped: Vec::new(),
                    exhausted: Vec::new(),
                    eager: true,
                }
            } else {
                w
            }
        }),
        Callee::Project(keys, _) => keys
            .iter()
            .all(|k| tcx.project.function_decl(k).is_some_and(|d| d.is_generator))
            .then(|| Wrapped {
                stepped: Vec::new(),
                exhausted: Vec::new(),
                eager: false,
            }),
        Callee::TooMany | Callee::Unresolved => None,
    }
}

fn is_iterator_annotation(tcx: &TypeCtx<'_>, annotation: &str) -> bool {
    tcx.module
        .resolve_annotation(annotation)
        .is_some_and(|t| ITERATOR_TYPES.contains(&t.as_str()))
}

/// `var` holds a `str` or `bytes`: a constant (through copies) or a value typed so.
fn is_text(f: &IrFunction, types: &FxHashMap<VarId, Val>, var: VarId) -> bool {
    let mut cur = var;
    while let Some((_, inst)) = f.def_inst(cur) {
        match &inst.kind {
            InstKind::Copy { src, .. } => cur = *src,
            InstKind::Const {
                literal: ConstLiteral::Str(_) | ConstLiteral::Bytes,
                ..
            } => return true,
            _ => break,
        }
    }
    matches!(types.get(&var), Some(Val::Inst(t)) if t == "builtins.str" || t == "builtins.bytes")
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

/// How `f` iterates its parameters (local facts; bit `i`: parameter `i < 64`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ParamIteration {
    /// `EffectSummary::consumes_params`: exhausted by a full consumption on every normal path.
    pub consumes: u64,
    /// `EffectSummary::unconditional_forwards`: passed to project calls only where the call
    /// runs on every normal path.
    pub unconditional: u64,
    /// `EffectSummary::iterates_params`: stepped by some iteration on some path.
    pub iterates: u64,
}

/// [`ParamIteration`] of `f` from its [`Roots::Summary`] flow. A generator function's body does
/// not run at the call, so its calls consume nothing; iterating the generator steps what its
/// body iterates (`iterates`).
pub(crate) fn param_iteration(f: &IrFunction, flow: &IterFlow) -> ParamIteration {
    let bits = |roots: &[VarId]| {
        let mut out = 0u64;
        for (i, p) in f.params.iter().take(64).enumerate() {
            if roots.contains(&p.var) {
                out |= 1u64 << i;
            }
        }
        out
    };
    let iterates = flow.events.iter().fold(0, |acc, e| acc | bits(&e.stepped));
    if f.params.is_empty()
        || matches!(
            f.kind,
            FunctionKind::Generator | FunctionKind::AsyncGenerator
        )
    {
        return ParamIteration {
            iterates,
            ..ParamIteration::default()
        };
    }
    let mut must: FxHashMap<BlockId, bool> = FxHashMap::default();
    let mut runs = |b: BlockId| *must.entry(b).or_insert_with(|| runs_on_every_return(f, b));
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
    ParamIteration {
        consumes,
        unconditional: bits(&params) & !conditional,
        iterates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(symbol, positional argument count, (stepped, exhausted, eagerly))`.
    type Case = (
        &'static str,
        usize,
        Option<(&'static [u32], &'static [u32], bool)>,
    );

    #[test]
    fn constructors_iterate_their_documented_arguments() {
        let v: Vec<VarId> = (0..4).map(VarId).collect();
        let cases: &[Case] = &[
            ("builtins.iter", 1, Some((&[0], &[0], false))),
            ("builtins.iter", 2, Some((&[], &[], false))),
            ("builtins.enumerate", 2, Some((&[0], &[0], false))),
            // `zip` / `map` over several iterables stop at the shortest: each is stepped, none
            // is exhausted.
            ("builtins.zip", 3, Some((&[0, 1, 2], &[], false))),
            ("builtins.zip", 1, Some((&[0], &[0], false))),
            ("builtins.map", 3, Some((&[1, 2], &[], false))),
            ("builtins.map", 2, Some((&[1], &[1], false))),
            ("builtins.map", 0, Some((&[], &[], false))),
            ("itertools.chain", 3, Some((&[0, 1, 2], &[0, 1, 2], false))),
            ("itertools.zip_longest", 2, Some((&[0, 1], &[0, 1], false))),
            ("heapq.merge", 2, Some((&[0, 1], &[0, 1], false))),
            ("builtins.filter", 2, Some((&[1], &[1], false))),
            // A generator is not reversible: `reversed` takes sequences.
            ("builtins.reversed", 1, Some((&[], &[], false))),
            // `islice` / `takewhile` stop at a count or a condition.
            ("itertools.islice", 3, Some((&[0], &[], false))),
            ("itertools.takewhile", 2, Some((&[1], &[], false))),
            ("itertools.compress", 2, Some((&[0, 1], &[], false))),
            ("itertools.product", 2, Some((&[0, 1], &[0, 1], true))),
            ("itertools.combinations", 2, Some((&[0], &[0], true))),
            ("itertools.filterfalse", 1, Some((&[], &[], false))),
            ("itertools.tee", 1, None),
            ("itertools.cycle", 1, None),
            ("itertools.repeat", 1, None),
            ("itertools.count", 0, None),
            ("builtins.list", 1, None),
        ];
        let vars = |idx: &[u32]| idx.iter().map(|i| VarId(*i)).collect::<Vec<_>>();
        for &(symbol, n, expected) in cases {
            let got = constructor_args(symbol, &v[..n]);
            let expected = expected.map(|(stepped, exhausted, eager)| Wrapped {
                stepped: vars(stepped),
                exhausted: vars(exhausted),
                eager,
            });
            assert_eq!(got, expected, "{symbol}/{n}");
        }
        // One iterator in every position (`zip(it, it)`, the pairing idiom) is exhausted.
        let it = [VarId(7), VarId(7)];
        assert_eq!(
            constructor_args("builtins.zip", &it).map(|w| w.exhausted),
            Some(it.to_vec())
        );
    }

    #[test]
    fn consumers_take_their_documented_arguments() {
        let v: Vec<VarId> = (0..3).map(VarId).collect();
        let cases: &[(&str, usize, &[u32])] = &[
            ("builtins.list", 1, &[0]),
            ("builtins.sorted", 2, &[0]),
            ("builtins.max", 1, &[0]),
            // `max(a, b)` compares two values; `list()` has nothing to consume.
            ("builtins.max", 2, &[]),
            ("builtins.min", 2, &[]),
            ("builtins.list", 0, &[]),
            ("builtins.list.extend", 1, &[0]),
            ("builtins.set.update", 3, &[0, 1, 2]),
            ("collections.deque", 2, &[0]),
            ("collections.defaultdict", 2, &[1]),
            ("heapq.nlargest", 2, &[1]),
            ("functools.reduce", 3, &[1]),
            ("builtins.len", 1, &[]),
            ("builtins.next", 1, &[]),
            // `any` / `all` stop at the item deciding the result.
            ("builtins.any", 1, &[]),
            ("builtins.all", 1, &[]),
            ("starlette.concurrency.iterate_in_threadpool", 1, &[]),
        ];
        for &(symbol, n, expected) in cases {
            let expected: Vec<VarId> = expected.iter().map(|i| VarId(*i)).collect();
            assert_eq!(consumer_args(symbol, &v[..n]), expected, "{symbol}/{n}");
        }
    }
}
