//! Where iterators are iterated: which SSA values are single-pass iterators, which of them
//! iterate other values lazily (`enumerate(it)` iterates `it` when it is iterated), and where
//! values are iterated. The one table of iterating calls, shared by `MUT106` (a second pass over
//! an exhausted iterator), the local summary (`EffectSummary::consumes_params` /
//! `iterates_params`), where a generator's body runs (`summary`: the `Iterate`,
//! `Return`, and `Through` call-site roles), and which callable a builtin calls once per item
//! (`map(f, xs)`, `sorted(xs, key=f)`: `Runner::PerItem`).
//!
//! An iteration *steps* every value it reaches through lazy wrappers (`zip(range(n), it)` steps
//! `it`) and *exhausts* only some of them (`zip(range(n), it)` takes `n` items of `it`):
//! `MUT106` needs exhaustion, a generator's body runs at any step.
//!
//! Every call is resolved through [`TypeCtx`], so a shadowed `list` or `iter` is never matched.

use crate::Items;
use crate::resolver::{Callee, TypeCtx, Val};
use heki_ast::Span;
use heki_ir::walk::{Budget, Facts, FnShape, Min, Sparse, Use, Visit, fixpoint, reach};
use heki_ir::{
    BlockId, ConstLiteral, DominatorTree, FunctionKind, Inst, InstKind, IrFunction, Terminator,
    VarId,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::RefCell;

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

/// The positional arguments a single-pass constructor iterates (as variables, or as positions).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Wrapped<T = VarId> {
    /// Every argument an iteration of the result steps.
    stepped: Vec<T>,
    /// The arguments an exhausting iteration of the result exhausts (a subset of `stepped`).
    exhausted: Vec<T>,
    /// Iterated when the result is built (`itertools.product`), not when it is iterated.
    eager: bool,
}

/// What a single-pass constructor iterates of `args`. `iter(callable, sentinel)` iterates
/// nothing.
fn constructor_args<T: Copy + PartialEq>(symbol: &str, args: &[T]) -> Option<Wrapped<T>> {
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
fn consumer_args<T: Copy>(symbol: &str, args: &[T]) -> Vec<T> {
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

/// Where a builtin of [`PER_ITEM_CALLERS`] is handed the callable it calls once per item.
#[derive(Clone, Copy)]
enum CallableArg {
    /// The positional argument at this index (`map(f, xs)`).
    At(usize),
    /// The keyword `key` (`sorted(xs, key=f)`), or the positional argument at this index when
    /// the call passes one (`itertools.groupby(xs, f)`).
    Key(Option<usize>),
}

/// How a builtin of [`PER_ITEM_CALLERS`] passes an item to the callable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ItemArgs {
    /// One argument per iterable, walked together (`map(f, xs, ys)` calls `f(x, y)`).
    OnePerIterable,
    /// The elements of the item (`itertools.starmap(f, pairs)` calls `f(*pair)`).
    Elements,
    /// After the value accumulated so far (`functools.reduce(f, xs)` calls `f(acc, x)`).
    AfterAccumulator,
}

/// Builtins calling a callable argument once per item of what they iterate: the positional
/// arguments [`CONSTRUCTORS`] and [`FULL_CONSUMERS`] give for them, and the list itself for
/// `list.sort`. The lazy ones (`map`, `filter`, ...) call it where their result is consumed,
/// which is almost always where they are written, as for a generator expression.
const PER_ITEM_CALLERS: &[(&str, CallableArg, ItemArgs)] = &[
    ("builtins.map", CallableArg::At(0), ItemArgs::OnePerIterable),
    (
        "builtins.filter",
        CallableArg::At(0),
        ItemArgs::OnePerIterable,
    ),
    ("itertools.starmap", CallableArg::At(0), ItemArgs::Elements),
    (
        "itertools.filterfalse",
        CallableArg::At(0),
        ItemArgs::OnePerIterable,
    ),
    (
        "functools.reduce",
        CallableArg::At(0),
        ItemArgs::AfterAccumulator,
    ),
    (
        "builtins.sorted",
        CallableArg::Key(None),
        ItemArgs::OnePerIterable,
    ),
    (
        "builtins.min",
        CallableArg::Key(None),
        ItemArgs::OnePerIterable,
    ),
    (
        "builtins.max",
        CallableArg::Key(None),
        ItemArgs::OnePerIterable,
    ),
    (
        "builtins.list.sort",
        CallableArg::Key(None),
        ItemArgs::OnePerIterable,
    ),
    (
        "itertools.groupby",
        CallableArg::Key(Some(1)),
        ItemArgs::OnePerIterable,
    ),
];

/// A callable a builtin calls once per item ([`per_item_callable`]).
pub(crate) struct PerItemCall {
    pub(crate) callable: VarId,
    pub(crate) items: Items,
    pub(crate) passed: ItemArgs,
}

/// The callable a call of the external `symbol` calls once per item, and where it takes the
/// items from. `None` when `symbol` is none of [`PER_ITEM_CALLERS`], or when the call does not
/// pass the callable where the builtin takes it (`*` unpacking before it, `**` without `key`).
pub(crate) fn per_item_callable(
    symbol: &str,
    args: &[VarId],
    kwargs: &[(Option<String>, VarId)],
    star_from: Option<u8>,
) -> Option<PerItemCall> {
    let &(_, at, passed) = PER_ITEM_CALLERS.iter().find(|c| c.0 == symbol)?;
    // From `star_from` on, an argument may be any number of values.
    let known = star_from.map_or(args.len(), |s| usize::from(s).min(args.len()));
    let callable = match at {
        CallableArg::Key(Some(i)) if i < args.len() => *args[..known].get(i)?,
        CallableArg::At(i) => *args[..known].get(i)?,
        CallableArg::Key(_) => kwargs
            .iter()
            .find(|(k, _)| k.as_deref() == Some("key"))
            .map(|(_, v)| *v)?,
    };
    let items = if symbol == "builtins.list.sort" {
        Items::Receiver
    } else {
        items_of(args, star_from, |positions| {
            match constructor_args(symbol, positions) {
                Some(w) => w.stepped,
                None => consumer_args(symbol, positions),
            }
        })
    };
    Some(PerItemCall {
        callable,
        items,
        passed,
    })
}

/// What a worker calls `callable` with once per item of the call's positional arguments from
/// `first` on, one item of each per call, as `map` does (`Executor.map(fn, *iterables)`,
/// `heki_kb::EcosystemEntry::offloads_per_item`).
pub(crate) fn per_item_of_args_from(
    callable: VarId,
    first: usize,
    args: &[VarId],
    star_from: Option<u8>,
) -> PerItemCall {
    PerItemCall {
        callable,
        items: items_of(args, star_from, |positions| {
            positions.iter().skip(first).copied().collect()
        }),
        passed: ItemArgs::OnePerIterable,
    }
}

/// The items of the iterables `iterables` picks among the call's positional arguments: hidden
/// when `*` unpacking makes an argument any number of values.
fn items_of(
    args: &[VarId],
    star_from: Option<u8>,
    iterables: impl FnOnce(&[u8]) -> Vec<u8>,
) -> Items {
    if star_from.is_some_and(|s| usize::from(s) < args.len()) {
        return Items::Unpacked;
    }
    let positions: Vec<u8> = (0..args.len())
        .map_while(|i| u8::try_from(i).ok())
        .collect();
    Items::Args(iterables(&positions))
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
pub struct IterFlow {
    /// Tracked value -> its root (a root maps to itself): parameters and single-pass sources,
    /// followed through `Copy` and phis whose other inputs share one root.
    pub root_of: FxHashMap<VarId, VarId>,
    /// The copies and phis of the function, and the one defining each value.
    links: Vec<AliasLink>,
    link_of: FxHashMap<VarId, usize>,
    /// Values merged from different roots (`rows() if flag else other()`, through copies): the
    /// roots each may be ([`Self::may_roots`]), decided for the values asked about.
    merged: RefCell<FxHashMap<VarId, Vec<VarId>>>,
    /// The steps of the flow's walks.
    budget: Budget,
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
    /// Analyze `f`, resolving its calls: `recorded` gives the callee a call's site in the
    /// index records (the summaries resolved it with the values `isinstance` tests narrow),
    /// and any other call is resolved with `types`.
    pub fn new(
        shape: &FnShape<'_>,
        tcx: &TypeCtx<'_>,
        types: &FxHashMap<VarId, Val>,
        roots: Roots,
        recorded: &dyn Fn(&Inst) -> Option<Callee>,
    ) -> Self {
        let f = shape.f;
        let mut callees = FxHashMap::default();
        for bb in &f.blocks {
            for (index, inst) in bb.insts.iter().enumerate() {
                if matches!(
                    inst.kind,
                    InstKind::Call { .. } | InstKind::MethodCall { .. }
                ) {
                    let callee =
                        recorded(inst).unwrap_or_else(|| resolve_inst(tcx, types, &inst.kind));
                    callees.insert((bb.id, index), callee);
                }
            }
        }
        Self::with_callees(shape, tcx, types, roots, callees)
    }

    /// Analyze `f` with the callee of every `Call` / `MethodCall` already resolved.
    pub fn with_callees(
        shape: &FnShape<'_>,
        tcx: &TypeCtx<'_>,
        types: &FxHashMap<VarId, Val>,
        roots: Roots,
        callees: FxHashMap<(BlockId, usize), Callee>,
    ) -> Self {
        let f = shape.f;
        let links = alias_links(f);
        let link_of = links.iter().enumerate().map(|(i, l)| (l.dst, i)).collect();
        let mut flow = Self {
            root_of: FxHashMap::default(),
            links,
            link_of,
            merged: RefCell::default(),
            budget: Budget::of_function("consumption::IterFlow", f),
            wraps: FxHashMap::default(),
            steps: FxHashMap::default(),
            def_at: FxHashMap::default(),
            callees,
            events: Vec::new(),
            unpackable: FxHashSet::default(),
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
        flow.alias(shape);
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
        flow.alias(shape);
        flow.collect_events(shape, types);
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

    /// The `Copy` and phi aliases of the roots: a value has one root when it copies a value that
    /// has one, or is a phi whose other inputs all have the same one. The least such facts, a
    /// value visited again only when an input gains its root. Past the budget, a value is
    /// tracked only where it is a root itself, so fewer iterations are seen, and no rule
    /// reports from the ones missed.
    fn alias(&mut self, shape: &FnShape<'_>) {
        let f = shape.f;
        let (links, link_of) = (&self.links, &self.link_of);
        let readers = |i: usize, out: &mut Vec<usize>| {
            for u in shape.readers(links[i].dst) {
                let dst = match *u {
                    Use::Inst(b, k) => f.block(b).insts[k as usize].kind.dst(),
                    Use::Phi(b, k) => Some(f.block(b).phis[k as usize].dst),
                    Use::Terminator(_) => None,
                };
                out.extend(dst.and_then(|d| link_of.get(&d).copied()));
            }
        };
        // A link is visited once, then once per input gaining its root.
        let max_inputs = links.iter().map(|l| l.inputs.len()).max().unwrap_or(0);
        let spec = Sparse {
            nodes: links.len(),
            readers: &readers,
            max_visits: u32::try_from(max_inputs + 1).unwrap_or(u32::MAX),
        };
        let roots = &self.root_of;
        let transfer = |i: usize, known: &Facts<Min<VarId>>| -> Option<Min<VarId>> {
            let root = |v: VarId| {
                roots.get(&v).copied().or_else(|| {
                    let j = link_of.get(&v)?;
                    known.get(*j).map(|r| r.0)
                })
            };
            let link = &links[i];
            if let Some(&own) = roots.get(&link.dst) {
                return Some(Min(own));
            }
            let mut inputs = link.inputs.iter().map(|v| root(*v));
            let first = inputs.next()??;
            inputs.all(|r| r == Some(first)).then_some(Min(first))
        };
        let found = fixpoint(&self.budget, spec, transfer).map(Facts::into_vec);
        // Past the budget no link has a root but its own.
        let found = found.open_means(Vec::new());
        for (link, root) in self.links.iter().zip(found) {
            if let Some(Min(r)) = root {
                self.root_of.entry(link.dst).or_insert(r);
            }
        }
    }

    /// The roots `v` may be when it has no root of its own: those of the values with one that
    /// reach it through copies and phis without one (`rows() if flag else other()`). Iterating
    /// it steps them all; it exhausts none (it is one of them). Past the budget, none.
    ///
    /// The answer of each value asked is kept, and a walk joins the answer of a value asked
    /// before instead of walking on through it: asked in their order, the links of a chain take
    /// one step each and the weight of the answers they keep and join. Only the values asked about have one: a
    /// merge of many roots asked at its end takes one walk, not a set per merge on the way.
    pub fn may_roots(&self, v: VarId) -> Vec<VarId> {
        if self.root_of.contains_key(&v) {
            return Vec::new();
        }
        if let Some(known) = self.merged.borrow().get(&v) {
            return known.clone();
        }
        let mut found = Vec::new();
        let inputs = |u: VarId, out: &mut Vec<VarId>| {
            if let Some(&i) = self.link_of.get(&u) {
                out.extend(&self.links[i].inputs);
            }
        };
        let walked = reach(&self.budget, [v], inputs, |u| {
            if let Some(&r) = self.root_of.get(&u) {
                found.push(r);
                return Visit::<()>::Prune;
            }
            match self.merged.borrow().get(&u) {
                Some(known) => {
                    // Each query joining a kept answer copies it, so its weight is taken again;
                    // past the budget the answer is none.
                    if self.budget.take(known.len()) {
                        found.extend(known);
                    }
                    Visit::Prune
                }
                None => Visit::Expand,
            }
        });
        found.sort();
        found.dedup();
        // The answer is kept: its weight is taken too.
        if walked.is_open() || !self.budget.take(found.len()) {
            found.clear();
        }
        self.merged.borrow_mut().insert(v, found.clone());
        found
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
            .flat_map(|v| match self.may_roots(*v) {
                roots if roots.is_empty() => vec![*v],
                roots => roots,
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

    fn collect_events(&mut self, shape: &FnShape<'_>, types: &FxHashMap<VarId, Val>) {
        let f = shape.f;
        // `a, b = it` reads `it[0]` and `it[1]`: one unpacking per root and block.
        let mut unpacked: FxHashSet<(BlockId, VarId)> = FxHashSet::default();
        for bb in &f.blocks {
            for (index, inst) in bb.insts.iter().enumerate() {
                let consumed: Vec<VarId> = match &inst.kind {
                    InstKind::MakeIter { dst, iterable, .. } => {
                        if let Some((exhausted, full)) = loop_exhaustion(shape, *dst) {
                            let done = (exhausted, 0);
                            self.push(bb.id, index, inst.span, &[*iterable], full, done);
                        }
                        continue;
                    }
                    InstKind::YieldFrom { value, .. } => vec![*value],
                    InstKind::SubscriptLoad { obj, index, .. }
                        if is_unpacking(shape, *index, inst.span) =>
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
                    } if method == "join" && args.len() == 1 && is_text(types, *recv) => {
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
pub fn is_unpacking(shape: &FnShape<'_>, index: VarId, span: Span) -> bool {
    matches!(
        shape.inst(index),
        Some(Inst { kind: InstKind::Const { literal: ConstLiteral::Int(_), .. }, span: at, .. })
            if *at == span
    )
}

/// A value that aliases others: a `Copy` of one, or a phi of its inputs (itself left out).
struct AliasLink {
    dst: VarId,
    inputs: Vec<VarId>,
}

/// The copies and phis of `f`, in block order.
fn alias_links(f: &IrFunction) -> Vec<AliasLink> {
    let mut links = Vec::new();
    for bb in &f.blocks {
        links.extend(bb.phis.iter().map(|phi| {
            AliasLink {
                dst: phi.dst,
                inputs: (phi.incoming.iter().map(|(_, v)| *v))
                    .filter(|v| *v != phi.dst)
                    .collect(),
            }
        }));
        links.extend(bb.insts.iter().filter_map(|inst| match inst.kind {
            InstKind::Copy { dst, src } => Some(AliasLink {
                dst,
                inputs: vec![src],
            }),
            _ => None,
        }));
    }
    links
}

/// Roots `values` reach through `edges` (pre-order, each root once).
fn expand_through(
    root_of: &FxHashMap<VarId, VarId>,
    edges: &FxHashMap<VarId, Vec<VarId>>,
    values: &[VarId],
) -> Vec<VarId> {
    let mut out = Vec::new();
    let mut seen: FxHashSet<VarId> = FxHashSet::default();
    let mut stack: Vec<VarId> = values.iter().rev().copied().collect();
    while let Some(v) = stack.pop() {
        if let Some(&r) = root_of.get(&v)
            && seen.insert(r)
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
        .resolve_annotation_ty(annotation)
        .and_then(|t| tcx.project.declared_ty(&t))
        .is_some_and(|t| {
            t.obj()
                .is_some_and(|o| ITERATOR_TYPES.contains(&o.class.as_str()))
        })
}

/// `var` holds a `str` or `bytes`.
fn is_text(types: &FxHashMap<VarId, Val>, var: VarId) -> bool {
    matches!(types.get(&var), Some(Val::Inst(t, _)) if t == "builtins.str" || t == "builtins.bytes")
}

/// For the iterator `iter` a `MakeIter` defines: the block entered when its loop is exhausted,
/// and whether the loop always runs to exhaustion (no `break`, `return`, or `raise` inside it).
fn loop_exhaustion(shape: &FnShape<'_>, iter: VarId) -> Option<(BlockId, bool)> {
    let f = shape.f;
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
        .find(|s| !shape.in_body(lp, *s))?;
    let leaves_early = lp.body.iter().filter(|b| **b != lp.header).any(|b| {
        let t = &f.block(*b).terminator;
        matches!(t, Terminator::Return(_) | Terminator::Raise(_))
            || t.successors().iter().any(|s| !shape.in_body(lp, *s))
    });
    Some((exhausted, !leaves_early))
}

/// Whether every normal-flow path from the entry to a `return` passes through a block: the
/// blocks dominating, over the normal edges, the nearest common dominator of the returns the
/// entry reaches, and every block when it reaches none.
fn on_every_return(shape: &FnShape<'_>) -> impl Fn(BlockId) -> bool {
    let f = shape.f;
    let tree = DominatorTree::over(shape, |p, b| {
        f.block(p).terminator.successors().contains(&b)
    });
    let returns = (f.blocks.iter())
        .filter(|b| matches!(b.terminator, Terminator::Return(_)) && tree.number(b.id).is_some())
        .map(|b| b.id);
    // Each step starts where the previous one ended, so the fold climbs the tree once.
    let common = returns.reduce(|c, r| tree.common(c, r));
    move |b| common.is_none_or(|c| tree.dominates(b, c))
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
pub(crate) fn param_iteration(shape: &FnShape<'_>, flow: &IterFlow) -> ParamIteration {
    let f = shape.f;
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
    let runs = on_every_return(shape);
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
