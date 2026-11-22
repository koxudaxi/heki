//! Set iteration order (`EFF112`): where a function takes the order of a set whose elements'
//! hashes change between runs, and where that order goes (docs/ANALYSIS_AND_KB_SPEC.md section
//! 2.4).
//!
//! - **Set order**: a value is a `set` or `frozenset` by construction (a display, a
//!   comprehension, `set()`, `frozenset()`, a set operator with such an operand, a module
//!   constant bound once to one) or by its inferred type (an annotation, a callee's declared
//!   return type).
//! - **Run-dependent hashes**: its elements are `str` or `bytes` (constants, values of that
//!   inferred type, or the keys of a mapping of `str` keys), or tuples of them: `PYTHONHASHSEED`
//!   randomizes their hashes per process, and with them the iteration order.
//! - **Order taken**: iterating it (a loop or a comprehension: each element), `list()`,
//!   `tuple()`, `iter()`, `enumerate()`, `zip()`, `str.join()`, formatting it, `set.pop()`, or a
//!   generator expression over it; a set holding at most one element on every path to that
//!   point has one order and takes none.
//! - **Order flow**: a value keeps the order of what it is built from through copies, phis,
//!   list, tuple, and dict displays, formatting, string concatenation, `str.join`, `list()` and
//!   the other builtins of [`KEEPING_BUILTINS`], `.items()` / `.keys()` / `.values()`, items read
//!   by position from a sequence that may hold two, and the containers that keep the order it is
//!   appended to or stored in (a list, a deque, a mapping, a stream; a set has an order of its
//!   own, and a key read from a mapping before keeps its place when stored again); an element of
//!   the set also passes on to what is computed from it (an attribute, an item looked up by it,
//!   text made from it, a call's result), and text added onto what a loop has built so far holds
//!   the elements in order. `sorted()`, `set()`, `len()`, comparisons, items looked up by key, and calls
//!   heki cannot resolve end it.
//! - **Ends**: a `return`, an argument a catalog output call writes in order
//!   (`EcosystemEntry::writes_in_order`, with its sorting keyword established false) or the
//!   message of a builtin exception it constructs, and an argument of a project call (resolved
//!   through the callee's facts during propagation). One element at a time reaches only an output
//!   that appends (a stream): a raise for each element names the first that fails.
//!
//! Each function keeps, IR-free, the ends each source reaches: the orders it takes, its
//! parameters, and the results of its project calls ([`SetOrderFacts::flows`]). Propagation
//! resolves them against the callees' facts into what the function returns in a set's order
//! ([`SetOrderFacts::returns`], [`SetOrderFacts::returns_params`]) and which parameters reach an
//! output ([`SetOrderFacts::writes_params`]); `EFF112` reads the same resolution
//! ([`Resolver`]).

use crate::resolver::Callee;
use crate::types::{Args, Elem, Ty};
use crate::{CallHop, EffectSummary, FuncKey, SharedChain, TypeCtx, Val};
use heki_ast::Span;
use heki_ir::walk::{
    Answer, Budget, Closure, Facts, FnShape, Join, KeySet, Sparse, Use, Visit, fixpoint, reach,
};
use heki_ir::{BinOpKind, BlockId, BuiltinKind, CompareOpKind, ConstLiteral, Inst, InstKind};
use heki_ir::{IrFunction, Terminator, VarId};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

/// Builtins whose result keeps the order of their arguments.
pub const KEEPING_BUILTINS: &[&str] = &[
    "builtins.list",
    "builtins.tuple",
    "builtins.dict",
    "builtins.str",
    "builtins.repr",
    "builtins.ascii",
    "builtins.format",
    "builtins.iter",
    "builtins.next",
    "builtins.enumerate",
    "builtins.zip",
    "builtins.reversed",
    "builtins.bytes",
    "builtins.bytearray",
];

/// Builtins whose result does not depend on the order of their arguments, or restores one.
const ORDER_FREE_BUILTINS: &[&str] = &[
    "builtins.len",
    "builtins.sorted",
    "builtins.set",
    "builtins.frozenset",
    "builtins.sum",
    "builtins.min",
    "builtins.max",
    "builtins.any",
    "builtins.all",
    "builtins.bool",
    "builtins.hash",
    "builtins.id",
    "builtins.isinstance",
    "builtins.issubclass",
    "builtins.callable",
    "builtins.type",
    "builtins.hasattr",
];

/// Methods whose result keeps the order of their receiver.
const KEEPING_METHODS: &[&str] = &[
    "items",
    "keys",
    "values",
    "copy",
    "encode",
    "decode",
    "strip",
    "lstrip",
    "rstrip",
    "upper",
    "lower",
    "replace",
    "split",
    "splitlines",
];

/// Methods putting their arguments into their receiver in order.
const FILLING_METHODS: &[&str] = &[
    "append",
    "extend",
    "insert",
    "appendleft",
    "update",
    "setdefault",
    "write",
    "writelines",
];

/// The filling methods only a container keeping its insertion order (a list, a deque, a
/// mapping) or a stream has; the others (`insert`, `update`, `setdefault`) also name a set's
/// method or one returning a new value.
const ORDERED_FILLING_METHODS: &[&str] = &["append", "extend", "appendleft", "write", "writelines"];

/// Classes whose instances keep the order items are put into them.
const ORDERED_CONTAINERS: &[&str] = &[
    "builtins.list",
    "builtins.dict",
    "collections.deque",
    "collections.OrderedDict",
    "collections.defaultdict",
    "collections.Counter",
];

/// Mapping classes: an item is read from one by key.
const MAPPINGS: &[&str] = &[
    "builtins.dict",
    "collections.OrderedDict",
    "collections.defaultdict",
    "collections.Counter",
];

/// What a function does with set iteration order ([`EffectSummary::set_order`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetOrderFacts {
    /// The ends each source of the function reaches (local, never propagated).
    pub flows: Vec<OrderFlow>,
    /// The function may return a value in the order of a set: where the order is taken
    /// (propagated through the functions returning it).
    pub returns: Option<Arc<OrderOrigin>>,
    /// The parameters whose order the function may return (propagated).
    pub returns_params: u64,
    /// The parameters whose order may reach an output, with the calls from the function down
    /// to the output call, which is the last hop (propagated).
    pub writes_params: BTreeMap<u8, SharedChain>,
}

impl SetOrderFacts {
    fn is_empty(&self) -> bool {
        self.flows.is_empty()
            && self.returns.is_none()
            && self.returns_params == 0
            && self.writes_params.is_empty()
    }
}

/// The ends one source of a function reaches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderFlow {
    pub source: OrderSource,
    pub ends: Vec<OrderEnd>,
}

/// What a flow starts from.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum OrderSource {
    /// The function takes a set's order here.
    Take(OrderTake),
    /// What the caller passes for this parameter.
    Param(u8),
    /// What the project call at `span` returns, one of `callees` running there.
    Result { span: Span, callees: Vec<FuncKey> },
}

/// Where and how a function takes a set's order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OrderTake {
    pub span: Span,
    pub how: TakeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TakeKind {
    /// A loop or a comprehension over it.
    Iterated,
    /// `list()`, `tuple()`, `iter()`, `enumerate()`, `zip()`, `str.join()` of it.
    Copied,
    /// Formatted into text.
    Formatted,
    /// `set.pop()`.
    Picked,
    /// A generator expression over it.
    Generated,
}

impl TakeKind {
    /// How a report says the order is taken.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Iterated => "iterating a set",
            Self::Copied => "copying a set into a sequence or text",
            Self::Formatted => "formatting a set",
            Self::Picked => "picking an element of a set",
            Self::Generated => "a generator expression over a set",
        }
    }
}

/// Where a flow ends.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum OrderEnd {
    /// The function returns it.
    Return,
    /// The output call at `span` writes it in order (`symbol`: the catalog symbol, or the
    /// builtin exception class whose message it is).
    Output { span: Span, symbol: String },
    /// It is argument `param` of `callee` at the project call at `site`; `each`: one element of
    /// the set at a time (the call runs once per element, so only an output that appends what
    /// each call writes, a stream, writes the set's order).
    Arg {
        site: Span,
        callee: FuncKey,
        param: u8,
        each: bool,
    },
}

/// Where a set's order a value has was taken: in `function`, at `take`, reached from the
/// function holding the fact through the returning calls `via` (none: taken by that function).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OrderOrigin {
    pub function: FuncKey,
    pub take: OrderTake,
    pub via: SharedChain,
}

// ----------------------------------------------------------------------------- local facts

/// A project call of the function: where it is, what it defines, and how it binds each
/// candidate's parameters.
pub(crate) struct OrderCall<'c> {
    pub at: (BlockId, usize),
    pub dst: VarId,
    pub span: Span,
    pub candidates: &'c [(FuncKey, Vec<(u8, VarId)>)],
    /// The receiver the candidates bind to their first parameter.
    pub recv: Option<VarId>,
}

/// What the summary of a function knows of it that its order flow reads.
pub(crate) struct LocalInput<'f, 'c> {
    pub f: &'f IrFunction,
    pub shape: &'c FnShape<'f>,
    pub cx: &'c TypeCtx<'f>,
    pub types: &'c FxHashMap<VarId, Val>,
    /// The types of its values with their arguments.
    pub inferred: &'c crate::infer::Inferred,
    pub callees: &'c FxHashMap<(BlockId, usize), Callee>,
}

/// The kind of order a value carries in the flow: an element of the set (or a value computed
/// from one), or a whole value in the set's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Element,
    Ordered,
}

/// A value of the flow with the kind of order it carries.
type Node = (VarId, Kind);

/// What one use of a value does with its order.
enum Step {
    To(VarId, Kind),
    End(OrderEnd),
}

/// The local order facts of `f` ([`SetOrderFacts::flows`]); `None` when no source reaches an
/// end.
pub(crate) fn local_facts(
    input: LocalInput<'_, '_>,
    calls: &[OrderCall<'_>],
) -> Option<Box<SetOrderFacts>> {
    let LocalInput {
        f,
        shape,
        cx,
        types,
        inferred,
        callees,
    } = input;
    let budget = Budget::of_function("EFF112 order flow", f);
    let mut flow = LocalFlow {
        f,
        shape,
        cx,
        types,
        callees,
        calls: calls.iter().map(|c| (c.at, c)).collect(),
        inferred,
        sets: Closure::new(),
        elements: FxHashSet::default(),
        item_reads: std::cell::OnceCell::new(),
        budget: &budget,
    };
    let mut sources: Vec<(OrderSource, VarId, Kind)> = Vec::new();
    flow.order_takes(&mut sources);
    flow.elements = sources
        .iter()
        .filter(|(_, _, kind)| *kind == Kind::Element)
        .map(|(_, v, _)| *v)
        .collect();
    for (i, p) in f.params.iter().enumerate().take(64) {
        sources.push((OrderSource::Param(i as u8), p.var, Kind::Ordered));
    }
    for c in calls {
        let callees = c.candidates.iter().map(|(k, _)| k.clone()).collect();
        sources.push((
            OrderSource::Result {
                span: c.span,
                callees,
            },
            c.dst,
            Kind::Ordered,
        ));
    }
    let mut ends_of: Vec<OrderEnd> = Vec::new();
    let mut end_ids: FxHashMap<OrderEnd, usize> = FxHashMap::default();
    // The nodes a node passes its order on to, recorded where its own ends are found.
    let next_of: std::cell::RefCell<FxHashMap<Node, Vec<Node>>> = std::cell::RefCell::default();
    let closure: Closure<(VarId, Kind), KeySet<usize>> = Closure::new();
    let mut flows: Vec<OrderFlow> = Vec::new();
    for (source, v, kind) in sources {
        let reached = closure.ask(
            &budget,
            (v, kind),
            |node| {
                let (own, next) = flow.order_node(node, &mut ends_of, &mut end_ids);
                next_of.borrow_mut().insert(node, next);
                own
            },
            |node, out| out.extend(next_of.borrow_mut().remove(&node).into_iter().flatten()),
        );
        // Past the budget nothing is decided: the function keeps no facts (below).
        let reached = reached.open_means(std::rc::Rc::default());
        if reached.is_empty() {
            continue;
        }
        flows.push(OrderFlow {
            source,
            ends: reached.iter().map(|i| ends_of[*i].clone()).collect(),
        });
    }
    let facts = SetOrderFacts {
        flows,
        ..SetOrderFacts::default()
    };
    (!facts.is_empty() && !budget.spent()).then(|| Box::new(facts))
}

/// The tables one function's order flow reads.
struct LocalFlow<'f, 'c> {
    f: &'f IrFunction,
    shape: &'c FnShape<'f>,
    cx: &'c TypeCtx<'f>,
    types: &'c FxHashMap<VarId, Val>,
    callees: &'c FxHashMap<(BlockId, usize), Callee>,
    calls: FxHashMap<(BlockId, usize), &'c OrderCall<'c>>,
    /// The function's inferred types with their arguments.
    inferred: &'c crate::infer::Inferred,
    /// Whether a value is a set, and of text ([`Self::hashes_vary`]).
    sets: Closure<VarId, (bool, bool)>,
    /// The elements the function's loops over sets take.
    elements: FxHashSet<VarId>,
    /// Where the function reads an item, by the mapping's copy root and the key.
    item_reads: std::cell::OnceCell<ItemReads>,
    budget: &'c Budget,
}

/// The places (block, instruction) of the item reads `obj[index]` of a function, by the copy
/// root of `obj` and `index`.
type ItemReads = FxHashMap<(VarId, VarId), Vec<(BlockId, usize)>>;

impl<'f, 'c> LocalFlow<'f, 'c> {
    fn inst_at(&self, at: (BlockId, usize)) -> &'f Inst {
        &self.f.block(at.0).insts[at.1]
    }

    /// The external function the call defining `v` calls.
    fn external_defining(&self, v: VarId) -> Option<&'c str> {
        match self.shape.def(v) {
            heki_ir::walk::Def::Inst(b, i) => self.external_callee((b, i as usize)),
            _ => None,
        }
    }

    fn external_callee(&self, at: (BlockId, usize)) -> Option<&'c str> {
        match self.callees.get(&at) {
            Some(Callee::External(symbol)) => Some(symbol),
            _ => None,
        }
    }

    /// The orders the function takes, each with the value carrying it.
    fn order_takes(&self, out: &mut Vec<(OrderSource, VarId, Kind)>) {
        let f = self.f;
        let mut next_of: FxHashMap<VarId, Vec<VarId>> = FxHashMap::default();
        for bb in &f.blocks {
            for inst in &bb.insts {
                if let InstKind::IterNext { dst, iter, .. } = &inst.kind {
                    next_of.entry(*iter).or_default().push(*dst);
                }
            }
        }
        for bb in &f.blocks {
            for (i, inst) in bb.insts.iter().enumerate() {
                let at = (bb.id, i);
                let take = |how| OrderTake {
                    span: inst.span,
                    how,
                };
                match &inst.kind {
                    // The iterable a comprehension or a generator expression of its own is
                    // handed (`.0`) is taken where it is created.
                    InstKind::MakeIter { dst, iterable, .. }
                        if f.var_name(*iterable) != Some(".0")
                            && self.order_taken(*iterable, bb.id) =>
                    {
                        for e in next_of.get(dst).into_iter().flatten() {
                            out.push((
                                OrderSource::Take(take(TakeKind::Iterated)),
                                *e,
                                Kind::Element,
                            ));
                        }
                    }
                    InstKind::MakeGenExpr {
                        dst, outer_iter, ..
                    } if self.order_taken(*outer_iter, bb.id) => out.push((
                        OrderSource::Take(take(TakeKind::Generated)),
                        *dst,
                        Kind::Ordered,
                    )),
                    InstKind::Format { dst, value, .. } | InstKind::Convert { dst, value, .. }
                        if self.order_taken(*value, bb.id) =>
                    {
                        out.push((
                            OrderSource::Take(take(TakeKind::Formatted)),
                            *dst,
                            Kind::Ordered,
                        ));
                    }
                    InstKind::MethodCall {
                        dst, recv, method, ..
                    } if method == "pop" && self.order_taken(*recv, bb.id) => out.push((
                        OrderSource::Take(take(TakeKind::Picked)),
                        *dst,
                        Kind::Ordered,
                    )),
                    InstKind::MethodCall {
                        dst, method, args, ..
                    } if method == "join"
                        && args.first().is_some_and(|a| self.order_taken(*a, bb.id)) =>
                    {
                        out.push((
                            OrderSource::Take(take(TakeKind::Copied)),
                            *dst,
                            Kind::Ordered,
                        ));
                    }
                    // A list or dict comprehension before Python 3.12 is a function of its own
                    // the creating function hands the iterated set to (`__comp_<id>`).
                    InstKind::Call {
                        dst,
                        callee,
                        callee_var: None,
                        args,
                        ..
                    } if self.ordered_comprehension(callee)
                        && args.first().is_some_and(|a| self.order_taken(*a, bb.id)) =>
                    {
                        out.push((
                            OrderSource::Take(take(TakeKind::Iterated)),
                            *dst,
                            Kind::Ordered,
                        ));
                    }
                    InstKind::Call { dst, args, .. }
                        if self
                            .external_callee(at)
                            .is_some_and(|s| KEEPING_BUILTINS.contains(&s))
                            && args.first().is_some_and(|a| self.order_taken(*a, bb.id)) =>
                    {
                        let how = match self.external_callee(at) {
                            Some(
                                "builtins.iter" | "builtins.next" | "builtins.enumerate"
                                | "builtins.zip" | "builtins.reversed",
                            ) => TakeKind::Iterated,
                            _ => TakeKind::Copied,
                        };
                        out.push((OrderSource::Take(take(how)), *dst, Kind::Ordered));
                    }
                    _ => {}
                }
            }
        }
    }

    /// Whether `callee` names a list or dict comprehension lowered as a function of its own,
    /// whose result keeps the order of what it iterates.
    fn ordered_comprehension(&self, callee: &str) -> bool {
        matches!(
            self.comprehension(callee),
            Some("<listcomp>" | "<dictcomp>")
        )
    }

    /// Whether `callee` names a dict comprehension lowered as a function of its own.
    fn dict_comprehension(&self, callee: &str) -> bool {
        self.comprehension(callee) == Some("<dictcomp>")
    }

    /// The name of the comprehension `callee` names when it is lowered as a function of its
    /// own (`__comp_<id>`, before Python 3.12).
    fn comprehension(&self, callee: &str) -> Option<&'f str> {
        let id = callee.strip_prefix("__comp_")?.parse::<u32>().ok()?;
        Some(self.cx.ir().function(heki_ir::FunctionId(id)).name.as_str())
    }

    /// Whether a use in block `at` takes the order of `v`: a set of run-dependent hashes that
    /// may hold two elements there.
    fn order_taken(&self, v: VarId, at: BlockId) -> bool {
        self.hashes_vary(v) && !self.bounded_to_one(v, at)
    }

    /// Whether `v` is a set whose elements' hashes change between runs: a set (`SET`) of text
    /// (`TEXT`), each part decided where the value is made or joined from the values it is
    /// made of ([`Self::set_sources`]).
    fn hashes_vary(&self, v: VarId) -> bool {
        let (set, text) = self.set_and_text(v);
        set && text
    }

    /// Whether `v` is a set, of any elements.
    fn is_set(&self, v: VarId) -> bool {
        self.set_and_text(v).0
    }

    fn set_and_text(&self, v: VarId) -> (bool, bool) {
        *self
            .sets
            .ask(
                self.budget,
                v,
                |x| self.set_own(x),
                |x, out| self.set_sources(x, out),
            )
            .open_means(std::rc::Rc::default())
    }

    /// Whether `v` itself is a set, and whether its elements are text, as where it is made says:
    /// a display, `set()` / `frozenset()` of text, a set operator, a module constant bound once to
    /// a set of text, a mapping's keys, or its inferred type.
    fn set_own(&self, v: VarId) -> (bool, bool) {
        let Some((_, inst)) = self.shape.defining(v) else {
            return self.typed_set(v);
        };
        match &inst.kind {
            InstKind::Copy { .. } => (false, false),
            InstKind::AllocBuiltin {
                kind: BuiltinKind::Set,
                dst,
                elems,
            } => (true, self.all_text(elems) || self.filled_with_text(*dst)),
            // A set operator makes a set of a set (its operands' facts, joined in) or of a
            // mapping's view; `|` of two mappings makes a mapping.
            InstKind::BinOp {
                op: BinOpKind::BitAnd | BinOpKind::BitOr | BinOpKind::BitXor | BinOpKind::Sub,
                in_place: false,
                lhs,
                rhs,
                ..
            } => (self.is_view(*lhs) || self.is_view(*rhs), false),
            InstKind::Call { args, .. } => match self.external_defining(v) {
                Some("builtins.set" | "builtins.frozenset") => {
                    (true, args.first().is_some_and(|a| self.text_items(*a)))
                }
                _ => self.typed_set(v),
            },
            InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. }
                if self.cx.string_set_global(name) =>
            {
                (true, true)
            }
            InstKind::MethodCall { recv, method, .. } if method == "keys" => {
                (false, self.text_keys(*recv))
            }
            _ => self.typed_set(v),
        }
    }

    /// Whether `v` is a view of a mapping's keys or items, which set operators take as a set.
    fn is_view(&self, v: VarId) -> bool {
        matches!(
            self.shape.inst(self.shape.copy_root(v)).map(|i| &i.kind),
            Some(InstKind::MethodCall { method, .. }) if method == "keys" || method == "items"
        )
    }

    /// The values whose set and text `v` takes on: a copy's source, a phi's inputs, a set
    /// operator's operands, the argument of `set()` / `frozenset()`.
    fn set_sources(&self, v: VarId, out: &mut Vec<VarId>) {
        match self.shape.def(v) {
            heki_ir::walk::Def::Phi(b, i) => out.extend(
                self.f.block(b).phis[i as usize]
                    .incoming
                    .iter()
                    .map(|(_, x)| *x),
            ),
            heki_ir::walk::Def::Inst(b, i) => match &self.inst_at((b, i as usize)).kind {
                InstKind::Copy { src, .. } => out.push(*src),
                InstKind::BinOp {
                    op: BinOpKind::BitAnd | BinOpKind::BitOr | BinOpKind::BitXor | BinOpKind::Sub,
                    lhs,
                    rhs,
                    in_place: false,
                    ..
                } => out.extend([*lhs, *rhs]),
                InstKind::Call { args, .. }
                    if matches!(
                        self.external_callee((b, i as usize)),
                        Some("builtins.set" | "builtins.frozenset")
                    ) =>
                {
                    out.extend(args.first().copied());
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// Every value of `elems`, at least one, is text.
    fn all_text(&self, elems: &[VarId]) -> bool {
        !elems.is_empty() && elems.iter().all(|e| self.holds_text(*e))
    }

    /// `v` is a `str` or `bytes` value: a constant, a formatted string, or of that type.
    fn holds_text(&self, v: VarId) -> bool {
        let root = self.shape.copy_root(v);
        match self.shape.inst(root).map(|i| &i.kind) {
            Some(InstKind::Const {
                literal: ConstLiteral::Str(_) | ConstLiteral::Bytes,
                ..
            })
            | Some(InstKind::BuildString { .. } | InstKind::Format { .. }) => true,
            _ => matches!(
                self.types.get(&root),
                Some(Val::Inst(c)) if c == "builtins.str" || c == "builtins.bytes"
            ),
        }
    }

    /// The set `set_var` (an empty display) is filled with text: each `add` on it or on a copy
    /// of it, at least one, adds text.
    fn filled_with_text(&self, set_var: VarId) -> bool {
        let mut added = false;
        let found_other = reach(
            self.budget,
            [set_var],
            |x, next| {
                for u in self.shape.readers(x) {
                    if let Use::Inst(b, i) = *u
                        && let InstKind::Copy { dst, .. } = &self.inst_at((b, i as usize)).kind
                    {
                        next.push(*dst);
                    }
                }
            },
            |x| {
                for u in self.shape.readers(x) {
                    let Use::Inst(b, i) = *u else {
                        continue;
                    };
                    if let InstKind::MethodCall {
                        recv, method, args, ..
                    } = &self.inst_at((b, i as usize)).kind
                        && *recv == x
                        && method == "add"
                    {
                        if !args.first().is_some_and(|a| self.holds_text(*a)) {
                            return Visit::Stop(());
                        }
                        added = true;
                    }
                }
                Visit::Expand
            },
        );
        added && found_other == Answer::Known(None)
    }

    /// The items of iterable `v` are text: a display of text, or a value whose inferred items
    /// are.
    fn text_items(&self, v: VarId) -> bool {
        let root = self.shape.copy_root(v);
        match self.shape.inst(root).map(|i| &i.kind) {
            Some(InstKind::AllocBuiltin { elems, .. }) if self.all_text(elems) => true,
            _ => self.inferred_ty(root).is_some_and(|t| items_are_text(&t)),
        }
    }

    /// What `v`'s inferred type says: a set or frozenset, and of text.
    fn typed_set(&self, v: VarId) -> (bool, bool) {
        self.inferred_ty(v).map_or((false, false), |t| {
            let set = matches!(&t, Ty::Inst(o) if o.class == "builtins.set" || o.class == "builtins.frozenset");
            (set, items_are_text(&t))
        })
    }

    /// Whether `obj[index]` looks an item up by key: `obj` is a mapping, or the key is text.
    fn looked_up(&self, obj: VarId, index: VarId) -> bool {
        self.is_mapping(obj)
            || self
                .inferred_ty(index)
                .is_some_and(|t| matches!(&t, Ty::Inst(o) if o.class == "builtins.str" || o.class == "builtins.bytes"))
    }

    /// Whether `v` is a mapping: a dict display or comprehension, a mapping class called, or
    /// a value of an inferred mapping type.
    fn is_mapping(&self, v: VarId) -> bool {
        let root = self.shape.copy_root(v);
        match self.shape.inst(root).map(|i| &i.kind) {
            Some(
                InstKind::DictLiteral { .. }
                | InstKind::AllocBuiltin {
                    kind: BuiltinKind::Dict,
                    ..
                },
            ) => true,
            Some(InstKind::Call { callee, .. }) if self.dict_comprehension(callee) => true,
            _ => {
                self.external_defining(root).is_some_and(|s| MAPPINGS.contains(&s))
                    || self.inferred_ty(root).is_some_and(|t| {
                        matches!(&t, Ty::Inst(o) if matches!(o.args, Args::Map(..)) || MAPPINGS.contains(&o.class.as_str()))
                    })
            }
        }
    }

    /// Whether `method` called on `recv` puts its arguments into a container that keeps their
    /// order. A set keeps an order of its own, which a later iteration of it takes anew.
    fn fills_in_order(&self, recv: VarId, method: &str) -> bool {
        let root = self.shape.copy_root(recv);
        !self.is_set(root)
            && (ORDERED_FILLING_METHODS.contains(&method) || self.keeps_insertion_order(root))
    }

    /// Whether `v` is a container keeping its insertion order: a list or dict display or
    /// comprehension, such a class called, or a value of such an inferred type.
    fn keeps_insertion_order(&self, v: VarId) -> bool {
        match self.shape.inst(v).map(|i| &i.kind) {
            Some(
                InstKind::DictLiteral { .. }
                | InstKind::AllocBuiltin {
                    kind: BuiltinKind::List | BuiltinKind::Dict,
                    ..
                },
            ) => true,
            Some(InstKind::Call { callee, .. }) if self.ordered_comprehension(callee) => true,
            _ => self
                .external_defining(v)
                .is_some_and(|s| ORDERED_CONTAINERS.contains(&s))
                || self.inferred_ty(v).is_some_and(
                    |t| matches!(&t, Ty::Inst(o) if ORDERED_CONTAINERS.contains(&o.class.as_str())),
                ),
        }
    }

    /// Whether the function reads `obj[index]` on every path to the instruction at `at`: a read
    /// earlier in its block, or in a block dominating it.
    fn read_before(&self, obj: VarId, index: VarId, at: (BlockId, usize)) -> bool {
        let reads = self.item_reads.get_or_init(|| {
            let mut reads = ItemReads::default();
            for bb in &self.f.blocks {
                for (i, inst) in bb.insts.iter().enumerate() {
                    if let InstKind::SubscriptLoad { obj, index, .. } = &inst.kind {
                        let key = (self.shape.copy_root(*obj), *index);
                        reads.entry(key).or_default().push((bb.id, i));
                    }
                }
            }
            reads
        });
        let doms = self.shape.dominators();
        let key = (self.shape.copy_root(obj), index);
        reads.get(&key).into_iter().flatten().any(|&(b, i)| {
            self.budget.take(1)
                && if b == at.0 {
                    i < at.1
                } else {
                    doms.dominates(b, at.0)
                }
        })
    }

    /// The inferred type of mapping `v` has text keys.
    fn text_keys(&self, v: VarId) -> bool {
        self.inferred_ty(v).is_some_and(|t| {
            matches!(&t, Ty::Inst(o) if matches!(o.args, Args::Map(..))) && items_are_text(&t)
        })
    }

    fn inferred_ty(&self, v: VarId) -> Option<Ty> {
        self.inferred.ty(v).cloned()
    }

    /// Whether `v` holds at most one element on every path to block `at`: a test of its length
    /// (`if len(v) > 1: return`, `v.pop() if len(v) == 1 else None`) on the way, on the side
    /// that bounds it.
    fn bounded_to_one(&self, v: VarId, at: BlockId) -> bool {
        let root = self.shape.copy_root(v);
        let doms = self.shape.dominators();
        let mut b = at;
        while let Some(d) = doms.strict_dominator(b).filter(|_| self.budget.take(1)) {
            if let Terminator::Branch {
                cond,
                then_bb,
                else_bb,
            } = self.f.block(d).terminator
                && let Some(holds) = self.bounding_side(cond, root)
            {
                // The edge into `side` is on every path to `at` when `side` is entered only by it
                // and dominates `at`.
                let side = if holds { then_bb } else { else_bb };
                if self.shape.preds(side) == [d] && doms.dominates(side, at) {
                    return true;
                }
            }
            b = d;
        }
        false
    }

    /// For a branch on `cond`, the side bounding the length of `set` to one (`Some(true)`: the
    /// test holding): `cond` compares `len(set)` with a constant, or it is the value of a
    /// short-circuit `or` (`and`) one of whose operands does so on its failing (holding) side,
    /// which the whole failing (holding) implies of every operand.
    fn bounding_side(&self, cond: VarId, set: VarId) -> Option<bool> {
        if let Some(holds) = self.length_at_most_one(cond, set) {
            return Some(holds);
        }
        let heki_ir::walk::Def::Phi(merge, i) = self.shape.def(cond) else {
            return None;
        };
        let incoming = &self.f.block(merge).phis[i as usize].incoming;
        // Each operand but the last branches to the merge on the side that decides the whole
        // (holding for `or`, failing for `and`); the last jumps there. The other side of the
        // whole is then known of every operand.
        let mut known = None;
        for (pred, x) in incoming {
            let side = match self.f.block(*pred).terminator {
                Terminator::Branch {
                    cond: c,
                    then_bb,
                    else_bb,
                } if c == *x && (then_bb == merge) != (else_bb == merge) => Some(else_bb == merge),
                Terminator::Jump(to) if to == merge => None,
                _ => return None,
            };
            match (known, side) {
                (_, None) => {}
                (None, Some(s)) => known = Some(s),
                (Some(k), Some(s)) if k != s => return None,
                _ => {}
            }
        }
        let known = known?;
        incoming
            .iter()
            .any(|(_, x)| self.length_at_most_one(*x, set) == Some(known))
            .then_some(known)
    }

    /// For a branch on `cond`, a comparison of `len(set)` with a constant: `Some(true)` when the
    /// test holding bounds the length to one, `Some(false)` when its failing does.
    fn length_at_most_one(&self, cond: VarId, set: VarId) -> Option<bool> {
        let InstKind::Compare { op, lhs, rhs, .. } = &self.shape.inst(cond)?.kind else {
            return None;
        };
        let measured = |x: VarId| match self.shape.inst(x).map(|i| &i.kind) {
            Some(InstKind::Call { args, .. })
                if self.external_defining(x) == Some("builtins.len") =>
            {
                args.first()
                    .is_some_and(|a| self.shape.copy_root(*a) == set)
            }
            _ => false,
        };
        let (op, n) = match (measured(*lhs), measured(*rhs)) {
            (true, false) => (*op, self.shape.int_const(*rhs)?),
            (false, true) => (flip(*op), self.shape.int_const(*lhs)?),
            _ => return None,
        };
        // `len op n`: does it holding (or failing) leave at most one element?
        let holds = match op {
            CompareOpKind::Eq => n <= 1,
            CompareOpKind::Lt => n <= 2,
            CompareOpKind::LtE => n <= 1,
            _ => false,
        };
        let fails = match op {
            CompareOpKind::Gt => n <= 1,
            CompareOpKind::GtE => n <= 2,
            CompareOpKind::NotEq => (0..=1).contains(&n),
            _ => false,
        };
        match (holds, fails) {
            (true, _) => Some(true),
            (false, true) => Some(false),
            _ => None,
        }
    }

    /// The ends at node `node` and the nodes its order passes on to.
    fn order_node(
        &self,
        (v, kind): (VarId, Kind),
        ends_of: &mut Vec<OrderEnd>,
        end_ids: &mut FxHashMap<OrderEnd, usize>,
    ) -> (KeySet<usize>, Vec<(VarId, Kind)>) {
        let mut own = KeySet::new();
        let mut next = Vec::new();
        for u in self.shape.readers(v) {
            let mut steps = Vec::new();
            match *u {
                Use::Phi(b, i) => steps.push(Step::To(self.f.block(b).phis[i as usize].dst, kind)),
                Use::Terminator(b) => {
                    if let Terminator::Return(Some(r)) = self.f.block(b).terminator
                        && r == v
                    {
                        steps.push(Step::End(OrderEnd::Return));
                    }
                }
                Use::Inst(b, i) => self.order_uses((b, i as usize), v, kind, &mut steps),
            }
            for step in steps {
                match step {
                    Step::To(w, k) => next.push((w, k)),
                    Step::End(end) => {
                        let id = *end_ids.entry(end.clone()).or_insert_with(|| {
                            ends_of.push(end);
                            ends_of.len() - 1
                        });
                        own.insert(id);
                    }
                }
            }
        }
        (own, next)
    }

    /// What the instruction at `at` does with the order `v` carries.
    fn order_uses(&self, at: (BlockId, usize), v: VarId, kind: Kind, out: &mut Vec<Step>) {
        let inst = self.inst_at(at);
        let derived = |dst: VarId| Step::To(dst, kind);
        let element = kind == Kind::Element;
        match &inst.kind {
            InstKind::Copy { dst, .. } => out.push(derived(*dst)),
            InstKind::AllocBuiltin {
                dst,
                kind: BuiltinKind::List | BuiltinKind::Tuple | BuiltinKind::Dict,
                ..
            }
            | InstKind::DictLiteral { dst, .. } => out.push(Step::To(*dst, Kind::Ordered)),
            InstKind::Format { dst, .. } | InstKind::Convert { dst, .. } => out.push(derived(*dst)),
            InstKind::BuildString { dst, parts } => {
                out.push(self.concatenated(*dst, parts, v, kind));
            }
            InstKind::BinOp {
                dst,
                op: BinOpKind::Add | BinOpKind::Mod,
                lhs,
                rhs,
                ..
            } => out.push(self.concatenated(*dst, &[*lhs, *rhs], v, kind)),
            InstKind::SubscriptLoad {
                dst,
                obj,
                index,
                key_const,
                ..
            } => {
                // An item read by position from an ordered sequence that may hold two items is a
                // pick; an item looked up by key, or by an element, is that key's.
                let picked = *obj == v
                    && key_const.is_none()
                    && !self.looked_up(*obj, *index)
                    && !self.bounded_to_one(*obj, at.0);
                if picked || *obj != v && element {
                    out.push(derived(*dst));
                }
            }
            InstKind::SubscriptStore {
                obj, index, value, ..
            } if *obj != v => {
                // A key read from the mapping before is in it: storing at it again keeps the
                // mapping's order, and a value computed from the element that is the key is that
                // key's alone.
                let own_key = *index == v && *value != v
                    || element && self.elements.contains(&self.shape.copy_root(*index));
                let rewritten = own_key && self.read_before(*obj, *index, at);
                if !rewritten {
                    out.push(Step::To(self.shape.copy_root(*obj), Kind::Ordered));
                }
            }
            InstKind::MethodCall {
                dst,
                recv,
                method,
                args,
                kwargs,
                ..
            } if *recv == v => {
                if KEEPING_METHODS.contains(&method.as_str()) {
                    out.push(Step::To(*dst, Kind::Ordered));
                } else {
                    self.order_call(at, (*dst, args, kwargs), v, kind, out);
                }
            }
            InstKind::MethodCall { recv, method, .. }
                if matches!(method.as_str(), "write" | "writelines")
                    && let Some(stream) = self.written_stream(*recv) =>
            {
                out.push(Step::End(OrderEnd::Output {
                    span: inst.span,
                    symbol: format!("{stream}.{method}"),
                }));
            }
            InstKind::MethodCall { recv, method, .. }
                if FILLING_METHODS.contains(&method.as_str())
                    && !matches!(self.callees.get(&at), Some(Callee::Project(..)))
                    && self.fills_in_order(*recv, method) =>
            {
                out.push(Step::To(self.shape.copy_root(*recv), Kind::Ordered));
            }
            InstKind::MethodCall { dst, method, .. }
                if matches!(method.as_str(), "join" | "format") =>
            {
                out.push(Step::To(*dst, Kind::Ordered));
            }
            InstKind::Call {
                dst, args, kwargs, ..
            }
            | InstKind::MethodCall {
                dst, args, kwargs, ..
            } => self.order_call(at, (*dst, args, kwargs), v, kind, out),
            _ => {}
        }
    }

    /// What text or a sequence `dst` concatenated from `operands`, one of them `v`, carries. Text
    /// made from one element is that element's, unless it is added onto what a loop has built so
    /// far (another operand is a phi, also formatted), which then holds the elements in order.
    fn concatenated(&self, dst: VarId, operands: &[VarId], v: VarId, kind: Kind) -> Step {
        let onto_carried = operands.iter().filter(|&&x| x != v).any(|&x| {
            let x = match self.shape.inst(x).map(|i| &i.kind) {
                Some(InstKind::Format { value, .. } | InstKind::Convert { value, .. }) => *value,
                _ => x,
            };
            matches!(
                self.shape.def(self.shape.copy_root(x)),
                heki_ir::walk::Def::Phi(..)
            )
        });
        let kind = if onto_carried { Kind::Ordered } else { kind };
        Step::To(dst, kind)
    }

    /// The stream `recv` writes to, as a report names it: `sys.stdout` or `sys.stderr`, or a
    /// file the function opens (`open(...)`, `io.open(...)`, `Path.open(...)`, also bound by a
    /// `with`).
    fn written_stream(&self, recv: VarId) -> Option<String> {
        let root = self.shape.copy_root(recv);
        let opened = |v: VarId| {
            matches!(
                self.external_defining(self.shape.copy_root(v)),
                Some("builtins.open" | "io.open" | "pathlib.Path.open")
            )
        };
        match &self.shape.inst(root)?.kind {
            InstKind::AttrLoad { obj, attr, .. }
                if matches!(attr.as_str(), "stdout" | "stderr")
                    && matches!(self.types.get(obj), Some(Val::Ref(m)) if m == "sys") =>
            {
                Some(format!("sys.{attr}"))
            }
            InstKind::EnterWith { mgr, .. } if opened(*mgr) => Some("open()".to_owned()),
            InstKind::Call { .. } | InstKind::MethodCall { .. } if opened(root) => {
                Some("open()".to_owned())
            }
            _ => None,
        }
    }

    /// What the call at `at` does with the order `v` carries as one of its arguments.
    fn order_call(
        &self,
        at: (BlockId, usize),
        (dst, args, kwargs): CallParts<'_>,
        v: VarId,
        kind: Kind,
        out: &mut Vec<Step>,
    ) {
        let span = self.inst_at(at).span;
        if let Some(call) = self.calls.get(&at) {
            for (callee, params) in call.candidates {
                let bound = params
                    .iter()
                    .filter(|(_, a)| *a == v)
                    .map(|(p, _)| *p)
                    .chain((call.recv == Some(v)).then_some(0));
                for param in bound {
                    out.push(Step::End(OrderEnd::Arg {
                        site: call.span,
                        callee: callee.clone(),
                        param,
                        each: kind == Kind::Element,
                    }));
                }
            }
            if kind == Kind::Element {
                out.push(Step::To(dst, Kind::Element));
            }
            return;
        }
        match self.callees.get(&at) {
            Some(Callee::External(symbol)) => {
                if ORDER_FREE_BUILTINS.contains(&symbol.as_str()) {
                    return;
                }
                if KEEPING_BUILTINS.contains(&symbol.as_str()) {
                    out.push(Step::To(dst, kind));
                    return;
                }
                if let Some(output) = self.written_output(span, (dst, args, kwargs), symbol, v) {
                    // One element at a time writes the set's order only where the writes add
                    // up: a stream.
                    if kind == Kind::Ordered || appends(symbol) {
                        out.push(Step::End(output));
                    }
                    // A serializer's text keeps the order too (`json.dumps`).
                    out.push(Step::To(dst, kind));
                    return;
                }
                if kind == Kind::Element {
                    out.push(Step::To(dst, Kind::Element));
                }
            }
            _ if kind == Kind::Element => out.push(Step::To(dst, Kind::Element)),
            _ => {}
        }
    }

    /// The output end of the call at `at` to `symbol` when it writes `v` in order: a catalog
    /// output call whose written argument is `v` and whose sorting keyword is established
    /// false, or a builtin exception constructed with `v` as its message.
    fn written_output(
        &self,
        span: Span,
        (_, args, kwargs): CallParts<'_>,
        symbol: &str,
        v: VarId,
    ) -> Option<OrderEnd> {
        let end = || {
            Some(OrderEnd::Output {
                span,
                symbol: symbol.to_owned(),
            })
        };
        if is_builtin_exception(symbol) {
            return args.contains(&v).then(end).flatten();
        }
        let out = self.cx.catalog(symbol)?.writes_in_order.as_ref()?;
        let written = match (out.arg, &out.param) {
            (None, None) => args.contains(&v),
            (arg, param) => {
                arg.is_some_and(|k| args.get(k as usize) == Some(&v))
                    || param.as_deref().is_some_and(|p| {
                        kwargs
                            .iter()
                            .any(|(name, x)| name.as_deref() == Some(p) && *x == v)
                    })
            }
        };
        if !written {
            return None;
        }
        let sorts = match &out.sorted_by {
            None => false,
            Some(keyword) => match kwargs
                .iter()
                .find(|(name, _)| name.as_deref() == Some(keyword))
            {
                None => out.sorted_by_default,
                Some((_, x)) => self.shape.bool_const(*x) != Some(false),
            },
        };
        (!sorts).then(end).flatten()
    }
}

/// Whether the output `symbol` adds what each call writes to what earlier calls wrote: a stream
/// (`print`, `pprint.pprint`, a stream's `write`). A raise is not one: an exception raised for one
/// element at a time names the first element that fails, which follows the set's order only when
/// two of them fail.
pub fn appends(symbol: &str) -> bool {
    matches!(symbol, "builtins.print" | "pprint.pprint" | "pprint.pp")
        || symbol.ends_with(".write")
        || symbol.ends_with(".writelines")
}

/// `a op b` read as `b op' a`.
fn flip(op: CompareOpKind) -> CompareOpKind {
    match op {
        CompareOpKind::Lt => CompareOpKind::Gt,
        CompareOpKind::LtE => CompareOpKind::GtE,
        CompareOpKind::Gt => CompareOpKind::Lt,
        CompareOpKind::GtE => CompareOpKind::LtE,
        other => other,
    }
}

/// A builtin exception class, whose constructor keeps its message.
fn is_builtin_exception(symbol: &str) -> bool {
    symbol.strip_prefix("builtins.").is_some_and(|name| {
        !name.contains('.') && (name.ends_with("Error") || name.ends_with("Exception"))
    })
}

/// A call's result, positional arguments, and keyword arguments.
type CallParts<'a> = (VarId, &'a [VarId], &'a [(Option<String>, VarId)]);

/// What iterating a value of type `t` yields (a mapping's keys) is text, or tuples of text.
fn items_are_text(t: &Ty) -> bool {
    let item = match t {
        Ty::Inst(crate::types::Obj {
            args: Args::Items(e) | Args::Map(e, _),
            ..
        }) => e.as_ref(),
        _ => return false,
    };
    let text = |e: &Elem| matches!(e, Elem::Of(Ty::Inst(x)) if x.class == "builtins.str" || x.class == "builtins.bytes");
    match item {
        Elem::Of(Ty::Inst(i)) if i.class == "builtins.tuple" => match &i.args {
            Args::Tuple(parts) => !parts.is_empty() && parts.iter().all(text),
            Args::Items(e) => text(e),
            _ => false,
        },
        e => text(e),
    }
}

// ----------------------------------------------------------------------------- resolution

/// Resolves a function's flows against its callees' facts: what each call returns in a set's
/// order, what each parameter reaches.
pub struct Resolver<'i> {
    summary_of: &'i dyn Fn(&FuncKey) -> Option<&'i EffectSummary>,
}

/// What a source of a function carries: a set's order (the first origin, in the order of
/// [`OrderOrigin`]), and the parameters whose order it may be.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Carried {
    pub origin: Option<Arc<OrderOrigin>>,
    pub params: u64,
}

impl Join for Carried {
    /// Join `other` in: whether this grew.
    fn join(&mut self, other: &Self) -> bool {
        let mut grew = other.params & !self.params != 0;
        self.params |= other.params;
        if let Some(o) = &other.origin
            && self.origin.as_ref().is_none_or(|kept| o < kept)
        {
            self.origin = Some(Arc::clone(o));
            grew = true;
        }
        grew
    }
}

impl<'i> Resolver<'i> {
    pub fn new(summary_of: &'i dyn Fn(&FuncKey) -> Option<&'i EffectSummary>) -> Self {
        Self { summary_of }
    }

    fn facts(&self, f: &FuncKey) -> Option<&'i SetOrderFacts> {
        (self.summary_of)(f)?.set_order.as_deref()
    }

    /// What each flow of `facts` (of `function`, in file `path`) carries: its own take, its
    /// parameter, or for a call's result, what a callee returns in a set's order, and what an
    /// argument carries that a callee returns the order of. The last is a fixpoint over the
    /// flows: a flow is visited again only when what it carries grows (its parameters, or an
    /// earlier origin), which bounds the visits.
    pub fn carried(&self, function: &FuncKey, path: &Path, facts: &SetOrderFacts) -> Vec<Carried> {
        let base: Vec<Carried> = facts
            .flows
            .iter()
            .map(|flow| match &flow.source {
                OrderSource::Take(take) => Carried {
                    origin: Some(Arc::new(OrderOrigin {
                        function: function.clone(),
                        take: take.clone(),
                        via: Vec::new(),
                    })),
                    params: 0,
                },
                OrderSource::Param(i) => Carried {
                    origin: None,
                    params: 1u64 << i,
                },
                OrderSource::Result { span, callees } => Carried {
                    origin: callees
                        .iter()
                        .filter_map(|callee| {
                            let origin = self.facts(callee)?.returns.as_ref()?;
                            let hop = Arc::new(CallHop {
                                caller: function.clone(),
                                callee: callee.fqn(),
                                span: *span,
                                path: path.to_path_buf(),
                            });
                            let via = std::iter::once(hop)
                                .chain(origin.via.iter().cloned())
                                .collect();
                            Some(Arc::new(OrderOrigin {
                                via,
                                ..(**origin).clone()
                            }))
                        })
                        .min(),
                    params: 0,
                },
            })
            .collect();
        let result_at: FxHashMap<Span, usize> = facts
            .flows
            .iter()
            .enumerate()
            .filter_map(|(i, flow)| match &flow.source {
                OrderSource::Result { span, .. } => Some((*span, i)),
                _ => None,
            })
            .collect();
        // `feeds[i]`: the flows whose order reaches the result of flow `i`'s call, through a
        // callee returning the parameter they are; `fed[j]`: the results flow `j` feeds.
        let mut feeds: Vec<Vec<(usize, bool)>> = vec![Vec::new(); facts.flows.len()];
        let mut fed: Vec<Vec<usize>> = vec![Vec::new(); facts.flows.len()];
        for (j, flow) in facts.flows.iter().enumerate() {
            for end in &flow.ends {
                let OrderEnd::Arg {
                    site,
                    callee,
                    param,
                    each,
                } = end
                else {
                    continue;
                };
                let returned = self
                    .facts(callee)
                    .is_some_and(|cf| cf.returns_params & (1u64 << param) != 0);
                if let Some(&i) = result_at.get(site).filter(|_| returned) {
                    feeds[i].push((j, *each));
                    fed[j].push(i);
                }
            }
        }
        let budget = Budget::unmetered("EFF112 carried orders");
        let spec = Sparse {
            nodes: facts.flows.len(),
            readers: &|j, out| out.extend(fed[j].iter().copied()),
            // Each visit after the first adds a parameter or an earlier origin.
            max_visits: 128,
        };
        let solved = fixpoint(&budget, spec, |i, solved: &Facts<Carried>| {
            let mut fact = base[i].clone();
            let solved_feeds = feeds[i]
                .iter()
                .filter_map(|&(j, each)| Some((solved.get(j)?, each)));
            for (from, each) in solved_feeds {
                // What a call returns for one element of the set at a time is in no order: only
                // the parameters pass on.
                fact.join(&Carried {
                    origin: from.origin.clone().filter(|_| !each),
                    params: from.params,
                });
            }
            Some(fact)
        });
        // Past the visit limit nothing is decided: no source carries anything.
        let solved = solved.map(Facts::into_vec).open_means(Vec::new());
        (0..base.len())
            .map(|i| solved.get(i).cloned().flatten().unwrap_or_default())
            .collect()
    }

    /// The output chain an end leads to: the output call itself, or a call passing a callee's
    /// parameter that reaches one (the call, then the callee's chain).
    pub fn output_chain(
        &self,
        function: &FuncKey,
        path: &Path,
        end: &OrderEnd,
    ) -> Option<SharedChain> {
        match end {
            OrderEnd::Return => None,
            OrderEnd::Output { span, symbol } => Some(vec![Arc::new(CallHop {
                caller: function.clone(),
                callee: symbol.clone(),
                span: *span,
                path: path.to_path_buf(),
            })]),
            OrderEnd::Arg {
                site,
                callee,
                param,
                each,
            } => {
                let chain = self
                    .facts(callee)?
                    .writes_params
                    .get(param)
                    .filter(|chain| !each || chain.last().is_some_and(|o| appends(&o.callee)))?;
                let hop = Arc::new(CallHop {
                    caller: function.clone(),
                    callee: callee.fqn(),
                    span: *site,
                    path: path.to_path_buf(),
                });
                Some(std::iter::once(hop).chain(chain.iter().cloned()).collect())
            }
        }
    }
}

/// The propagated facts of `s` (the function `function` of file `path`), from its flows and
/// its callees' facts.
pub(crate) fn propagate<'i>(
    summary_of: &'i dyn Fn(&FuncKey) -> Option<&'i EffectSummary>,
    function: &FuncKey,
    path: &Path,
    s: &mut EffectSummary,
) {
    let Some(facts) = s.set_order.as_deref() else {
        return;
    };
    let resolver = Resolver::new(summary_of);
    let carried = resolver.carried(function, path, facts);
    let mut returned = Carried::default();
    let mut writes_params: BTreeMap<u8, SharedChain> = BTreeMap::new();
    for (flow, carries) in facts.flows.iter().zip(&carried) {
        for end in &flow.ends {
            if *end == OrderEnd::Return {
                returned.join(carries);
                continue;
            }
            if carries.params == 0 {
                continue;
            }
            let Some(chain) = resolver.output_chain(function, path, end) else {
                continue;
            };
            for i in (0..64u8).filter(|i| carries.params & (1u64 << i) != 0) {
                if writes_params
                    .get(&i)
                    .is_none_or(|kept| chain.len() < kept.len())
                {
                    writes_params.insert(i, chain.clone());
                }
            }
        }
    }
    let facts = s.set_order.as_deref_mut().expect("checked above");
    facts.returns = returned.origin;
    facts.returns_params = returned.params;
    facts.writes_params = writes_params;
}

/// Whether `v` is a `str` or `bytes` constant.
fn is_text_constant(shape: &FnShape<'_>, v: VarId) -> bool {
    let kind = shape.inst(shape.copy_root(v)).map(|i| &i.kind);
    matches!(
        kind,
        Some(InstKind::Const {
            literal: ConstLiteral::Str(_) | ConstLiteral::Bytes,
            ..
        })
    )
}

/// The module-level names bound once to a set of text constants (a display, or `set()` /
/// `frozenset()` of one), for [`TypeCtx::string_set_global`].
pub(crate) fn string_set_globals(ir: &heki_ir::ModuleIr) -> FxHashSet<String> {
    let mut stores: FxHashMap<&str, u32> = FxHashMap::default();
    for f in &ir.functions {
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            if let InstKind::StoreGlobal { name, .. } = &inst.kind {
                *stores.entry(name).or_default() += 1;
            }
        }
    }
    let module = ir.function(ir.module_fn);
    let shape = FnShape::new(module);
    let text_display = |v: VarId| match shape.inst(shape.copy_root(v)).map(|i| &i.kind) {
        Some(InstKind::AllocBuiltin { elems, .. }) => {
            !elems.is_empty() && elems.iter().all(|e| is_text_constant(&shape, *e))
        }
        _ => false,
    };
    let mut out = FxHashSet::default();
    for inst in module.blocks.iter().flat_map(|b| &b.insts) {
        let InstKind::StoreGlobal { name, value } = &inst.kind else {
            continue;
        };
        if stores.get(name.as_str()) != Some(&1) {
            continue;
        }
        let set = match shape.inst(shape.copy_root(*value)).map(|i| &i.kind) {
            Some(InstKind::AllocBuiltin {
                kind: BuiltinKind::Set,
                ..
            }) => text_display(*value),
            Some(InstKind::Call { callee, args, .. })
                if matches!(callee.as_str(), "frozenset" | "set") =>
            {
                args.first().is_some_and(|a| text_display(*a))
            }
            _ => false,
        };
        if set {
            out.insert(name.clone());
        }
    }
    out
}
