//! The keywords a call passes by name, and the constants it passes for the parameters an
//! argument list names (`CallSite::arguments`): written, or the entries of a mapping the function
//! builds and unpacks with `**` (a dict display, a merge of displays, `dict(...)`, `a | b`, or
//! one of several on different paths), and the field values a library API builds its receiver's
//! class from (`EcosystemEntry::builds_receiver`).

use super::{Bindings, Call, Extractor};
use crate::resolver::{self, Callee, FunctionDecl, Val};
use crate::{ArgValue, ArgumentValues};
use heki_ir::walk::{Answer, Def, Facts, Join, Sparse, Use, Visit, fixpoint, reach};
use heki_ir::{BinOpKind, DictEntry, InstKind, VarId};
use heki_kb::FieldValues;
use rustc_hash::{FxHashMap, FxHashSet};
use std::borrow::Cow;
use std::cell::RefCell;

/// The most ways a mapping may be built that its entries are followed through: a phi of
/// displays is as many ways as its inputs, and merging two mappings is one way per pair of their
/// ways. Past it, the mapping may hold anything.
const MAPPING_WAYS: usize = 8;

/// The keywords a call passes by name on one way the mappings it unpacks may be built
/// ([`Extractor::keyword_ways`]).
#[derive(Clone)]
pub(super) struct Keywords<'c> {
    /// Each keyword written, and each entry of a `**` operand the function builds.
    pub(super) named: Vec<(Cow<'c, str>, VarId)>,
    /// Another `**` operand may pass any keyword.
    pub(super) open: bool,
    /// Two of `named` share a name: the call raises `TypeError`.
    pub(super) repeated: bool,
}

/// One way a mapping may be built: its entries, sorted by key, and whether it may hold other
/// keys too (a mapping heki does not follow was merged into it before them).
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Way {
    entries: Vec<(String, VarId)>,
    open: bool,
}

impl Way {
    /// Bind `key` to `value`, over an entry of the key.
    fn set(&mut self, key: &str, value: VarId) {
        match self.entries.binary_search_by(|(k, _)| k.as_str().cmp(key)) {
            Ok(i) => self.entries[i].1 = value,
            Err(i) => self.entries.insert(i, (key.to_owned(), value)),
        }
    }

    /// Merge in a mapping that may hold any key: it may replace every entry.
    fn forget(&mut self) {
        self.entries.clear();
        self.open = true;
    }

    /// This way with the way `later` merged into it (`{**self, **later}`): the entries of
    /// `later` replace those of their keys, and the keys heki does not know that `later` may
    /// hold may replace every entry.
    fn merged(&self, later: &Way) -> Way {
        if later.open {
            return later.clone();
        }
        let mut out = self.clone();
        for (k, v) in &later.entries {
            out.set(k, *v);
        }
        out
    }
}

/// The ways a mapping value may be built ([`Extractor::mapping`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Ways {
    /// A dict the function builds, by each way it may be built (sorted, at most
    /// [`MAPPING_WAYS`]).
    Built(Vec<Way>),
    /// A value heki does not build as a mapping, or one of more ways than [`MAPPING_WAYS`].
    Any,
}

impl Ways {
    /// `ways`, sorted and without repeats.
    fn built(mut ways: Vec<Way>) -> Self {
        ways.sort_unstable();
        ways.dedup();
        Self::Built(ways)
    }
}

/// A mapping may be built any way one of its sources may: the union of the ways, any value
/// once one source is.
impl Join for Ways {
    fn join(&mut self, other: &Self) -> bool {
        let (Self::Built(mine), Self::Built(theirs)) = (&mut *self, other) else {
            let grew = *self != Self::Any;
            *self = Self::Any;
            return grew;
        };
        let before = mine.len();
        for way in theirs {
            if let Err(i) = mine.binary_search(way) {
                mine.insert(i, way.clone());
            }
        }
        if mine.len() > MAPPING_WAYS {
            *self = Self::Any;
            return true;
        }
        mine.len() != before
    }

    fn weight(&self) -> usize {
        match self {
            Self::Built(ways) => ways.iter().map(|w| 1 + w.entries.len()).sum(),
            Self::Any => 1,
        }
    }
}

/// How the function builds a mapping value ([`Extractor::mapping`]).
enum Node {
    /// A mapping decided before, with its ways.
    Known(Ways),
    /// From parts, in order: a dict display, a `dict(...)` call, a `|` merge.
    Parts(Vec<Part>),
    /// A phi: the value of one of its inputs (each a copy root).
    Either(Vec<VarId>),
    /// Any other value, or a mapping something may change: it may hold anything.
    Leaf,
}

/// A part of a mapping built from parts ([`Node::Parts`]).
enum Part {
    /// An entry with a string constant key.
    Entry(String, VarId),
    /// The entries of another mapping (a copy root).
    Merge(VarId),
    /// An entry whose key heki cannot tell: it may replace every entry before it.
    Unknown,
}

impl Node {
    /// Add to `out` the mappings this one is built from.
    fn sources(&self, out: &mut Vec<VarId>) {
        match self {
            Self::Parts(parts) => out.extend(parts.iter().filter_map(|p| match p {
                Part::Merge(v) => Some(*v),
                Part::Entry(..) | Part::Unknown => None,
            })),
            Self::Either(inputs) => out.extend(inputs),
            Self::Known(_) | Self::Leaf => {}
        }
    }
}

/// [`CallSite::arguments`] from what each way a call may run passes for the marked parameters
/// (`None`: one where the call cannot run): the constants every way agrees on.
pub(super) fn agreed_marks(
    each: impl IntoIterator<Item = Option<Vec<(String, ArgValue)>>>,
) -> Option<Box<ArgumentValues>> {
    let mut each = each.into_iter().map(|marks| {
        let mut marks = marks?;
        marks.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        Some(marks)
    });
    let mut values = each.next()??;
    for other in each {
        let other = other?;
        values.retain(|(name, value)| {
            other
                .binary_search_by(|(n, _)| n.as_str().cmp(name))
                .is_ok_and(|i| &other[i].1 == value)
        });
    }
    (!values.is_empty()).then(|| Box::new(ArgumentValues(values)))
}

impl<'a> Extractor<'a> {
    /// [`Contracts::marked_params`], when `call` may pass them: a builtin calling the callees
    /// per item passes them no constant of the caller.
    pub(super) fn marked_params(&self, call: &Call<'_>) -> Option<&'a FxHashSet<String>> {
        let marked = self.contracts?.marked_params();
        (!marked.is_empty() && call.per_item.is_none()).then_some(marked)
    }

    /// [`CallSite::arguments`] of a class without a project `__init__` (a dataclass, a pydantic
    /// model): only a keyword names a parameter there.
    pub(super) fn keyword_marks(&self, call: &Call<'_>) -> Option<Box<ArgumentValues>> {
        let marked = self.marked_params(call)?;
        agreed_marks(self.keyword_ways(call).into_iter().map(|passed| {
            if passed.repeated {
                return None;
            }
            let values = passed
                .named
                .iter()
                .filter(|(name, _)| marked.contains(name.as_ref()))
                .filter_map(|(name, v)| Some((name.to_string(), self.arg_value(*v)?)));
            Some(values.collect())
        }))
    }

    /// [`CallSite::arguments`] of a call building an instance of its receiver's class from
    /// `fields` ([`Self::built_receiver`]): the keywords, or the entries of the mapping passed
    /// there, as every way it may be built gives them.
    pub(super) fn field_marks(
        &self,
        call: &Call<'_>,
        fields: &FieldValues,
    ) -> Option<Box<ArgumentValues>> {
        if fields.keywords {
            return self.keyword_marks(call);
        }
        let marked = self.marked_params(call)?;
        let Ways::Built(ways) = self.mapping(self.field_mapping(call, fields)?) else {
            return None;
        };
        agreed_marks(ways.iter().map(|way| {
            let values = way
                .entries
                .iter()
                .filter(|(name, _)| marked.contains(name))
                .filter_map(|(name, v)| Some((name.clone(), self.arg_value(*v)?)));
            Some(values.collect())
        }))
    }

    /// The project class whose instance a call of the library API `symbol` builds without its
    /// `__init__`, and where the fields' values come from (`EcosystemEntry::builds_receiver`):
    /// the class of the receiver, a class object or an instance. A call building one only from a
    /// dict builds none from a value not established to be one.
    pub(super) fn built_receiver(
        &self,
        symbol: &str,
        call: &Call<'_>,
    ) -> Option<(String, &'a FieldValues)> {
        let fields = self.cx.catalog(symbol)?.builds_receiver.as_ref()?;
        let class = match self.val(call.recv_var?)? {
            Val::Ref(class) | Val::Class(class) | Val::Inst(class) => class,
            Val::Super(_) | Val::Union(_) => return None,
        };
        if !self.cx.project.is_project_class(class) {
            return None;
        }
        if fields.only_from_dict {
            let mapping = self.field_mapping(call, fields)?;
            let dict = matches!(self.mapping(mapping), Ways::Built(_))
                || matches!(self.val(mapping), Some(Val::Inst(c)) if c == "builtins.dict");
            if !dict {
                return None;
            }
        }
        Some((class.clone(), fields))
    }

    /// The mapping `call` passes where `fields` takes the fields' values from.
    fn field_mapping(&self, call: &Call<'_>, fields: &FieldValues) -> Option<VarId> {
        let at = (fields.arg, fields.param.as_deref());
        resolver::passed_var(call.args, call.kwargs, call.star_from, at)
    }

    /// The constants `call` passes to `decl` (whose first positional argument binds parameter
    /// `offset`) for its parameters an argument list names (`marked`), as `bindings` binds them,
    /// with the name of a keyword landing in `**kwargs` as a parameter of its own. `None` when
    /// the call raises `TypeError` instead of running `decl` ([`Bindings::raises`]).
    pub(super) fn callee_marks(
        &self,
        call: &Call<'_>,
        decl: &FunctionDecl,
        offset: usize,
        bindings: &Bindings<'_>,
        marked: &FxHashSet<String>,
    ) -> Option<Vec<(String, ArgValue)>> {
        if bindings.raises(decl, offset) {
            return None;
        }
        let name_of = |p: usize| decl.param_names.get(p).filter(|n| marked.contains(*n));
        let mut out = Vec::new();
        for (name, v, p) in &bindings.keywords {
            // In `**kwargs`, the keyword passes nothing to a parameter of its name (a
            // positional-only one, or `*args` / `**kwargs` itself).
            if !marked.contains(name.as_ref())
                || (p.is_none() && decl.param_names.iter().any(|n| n == name))
            {
                continue;
            }
            out.extend(self.arg_value(*v).map(|value| (name.to_string(), value)));
        }
        for &(slot, a) in &bindings.slots {
            if let Some(name) = decl.positional_param(slot).and_then(name_of) {
                out.extend(self.arg_value(a).map(|value| (name.clone(), value)));
            }
        }
        for (p, c) in bindings.defaults(call, decl, offset) {
            if let Some(name) = name_of(p) {
                out.extend(ArgValue::decided(c).map(|value| (name.clone(), value)));
            }
        }
        Some(out)
    }

    /// How `call` binds the parameters of `decl`, whose first positional argument binds
    /// parameter `offset` (a bound receiver binds those before it).
    pub(super) fn bindings<'c>(
        &self,
        call: &Call<'c>,
        decl: &FunctionDecl,
        offset: usize,
    ) -> Bindings<'c> {
        self.bind(call, decl, offset, self.passed_keywords(call))
    }

    /// [`Self::bindings`] with the keywords `passed`.
    pub(super) fn bind<'c>(
        &self,
        call: &Call<'c>,
        decl: &FunctionDecl,
        offset: usize,
        passed: Keywords<'c>,
    ) -> Bindings<'c> {
        let keywords = passed.named.into_iter().map(|(name, v)| {
            let p = decl.keyword_param(&name);
            (name, v, p)
        });
        Bindings {
            slots: self.positional_slots(call, offset),
            keywords: keywords.collect(),
            unpacks: call.star_from.is_some() || passed.open,
            repeated: passed.repeated,
        }
    }

    /// The keywords `call` passes by name, whatever way the mappings it unpacks are built: the
    /// way when there is one, else those written and any other.
    pub(super) fn passed_keywords<'c>(&self, call: &Call<'c>) -> Keywords<'c> {
        let mut ways = self.keyword_ways(call);
        if ways.len() == 1 {
            return ways.swap_remove(0);
        }
        let written = call.kwargs.iter().filter_map(|(name, v)| {
            let name = name.as_deref()?;
            Some((Cow::Borrowed(name), *v))
        });
        Keywords {
            named: written.collect(),
            open: true,
            repeated: false,
        }
    }

    /// The keywords `call` passes by name on each way the mappings it unpacks with `**` may be
    /// built ([`Self::mapping`]), which Python passes as written keywords: each one written, and
    /// the entries of the mapping on that way; a mapping heki does not build, or one that would
    /// make more than [`MAPPING_WAYS`] ways, may pass any keyword. Every call binds each entry of
    /// the mappings it unpacks, which the source does not repeat: the entries are taken from the
    /// function's budget, so that a wide display unpacked by many calls costs work linear in the
    /// function, and past the budget a mapping may pass any keyword.
    pub(super) fn keyword_ways<'c>(&self, call: &Call<'c>) -> Vec<Keywords<'c>> {
        let mut ways = vec![Keywords {
            named: Vec::new(),
            open: false,
            repeated: false,
        }];
        let mut unpacked = false;
        for (name, v) in call.kwargs {
            if let Some(name) = name {
                for way in &mut ways {
                    way.named.push((Cow::Borrowed(name.as_str()), *v));
                }
                continue;
            }
            let built = match self.mapping(*v) {
                Ways::Built(built) if ways.len() * built.len() <= MAPPING_WAYS => built,
                Ways::Built(_) | Ways::Any => Vec::new(),
            };
            let bound: usize = built.iter().map(|w| w.entries.len()).sum::<usize>() * ways.len();
            if built.is_empty() || !self.walk_budget.take(bound) {
                for way in &mut ways {
                    way.open = true;
                }
                continue;
            }
            unpacked = true;
            ways = ways
                .iter()
                .flat_map(|way| {
                    built.iter().map(move |mapping| {
                        let mut out = way.clone();
                        let entries = mapping.entries.iter();
                        out.named
                            .extend(entries.map(|(k, v)| (Cow::Owned(k.clone()), *v)));
                        out.open |= mapping.open;
                        out
                    })
                })
                .collect();
        }
        // Written keywords never share a name; a mapping unpacked may repeat one.
        if unpacked {
            for way in &mut ways {
                let mut seen = FxHashSet::default();
                way.repeated = !way.named.iter().all(|(n, _)| seen.insert(n.as_ref()));
            }
        }
        ways
    }

    /// The ways the mapping `var` holds through copies may be built, when it is a dict the
    /// function builds and nothing changes ([`Self::only_unpacked`]): a dict display with string
    /// constant keys ([`Self::resolved_str`]; a later entry of a key replaces an earlier one),
    /// one merging other mappings (`{**a, "k": v}`, `dict(a, k=v)`, `a | b`: the entries of a
    /// later one replace those of an earlier one, and one heki does not build may replace any),
    /// or a phi of such mappings (each input's ways).
    ///
    /// One walk over the mappings it is built from, then a fixpoint over them, so that a mapping
    /// a loop merges into itself is built again with what each pass adds. The answer of every
    /// mapping the fixpoint decides is kept, and a later walk stops at one: a chain of merges
    /// asked link by link takes one step per link. Past the budget, a mapping may hold anything.
    pub(super) fn mapping(&self, var: VarId) -> Ways {
        let root = self.shape.copy_root(var);
        if let Some(known) = self.mappings.borrow().get(&root) {
            return known.clone();
        }
        self.decide_mapping(root)
    }

    /// [`Self::mapping`] of the copy root `root`, decided with the mappings it is built from,
    /// and kept.
    fn decide_mapping(&self, root: VarId) -> Ways {
        let nodes: RefCell<Vec<(VarId, Node)>> = RefCell::new(Vec::new());
        let index: RefCell<FxHashMap<VarId, usize>> = RefCell::default();
        let next = |v: VarId, out: &mut Vec<VarId>| {
            let i = index.borrow()[&v];
            nodes.borrow()[i].1.sources(out);
        };
        let visit = |v: VarId| {
            let known = self.mappings.borrow().get(&v).cloned();
            let node = match known {
                Some(ways) => Node::Known(ways),
                None => self.mapping_node(v),
            };
            let mut nodes = nodes.borrow_mut();
            index.borrow_mut().insert(v, nodes.len());
            nodes.push((v, node));
            Visit::<()>::Expand
        };
        let walked = reach(&self.walk_budget, [root], next, visit);
        // Past the budget, the mappings it is built from are not all found.
        let decided =
            walked.and_then(|_| self.mapping_ways(nodes.into_inner(), index.into_inner()));
        decided.map(|kept| kept[0].clone()).open_means(Ways::Any)
    }

    /// The ways of the mappings `nodes` (the first asked, then the mappings each is built from,
    /// by `index`), each kept: a mapping built only from itself, which no path builds, holds
    /// nothing it could pass.
    fn mapping_ways(
        &self,
        nodes: Vec<(VarId, Node)>,
        index: FxHashMap<VarId, usize>,
    ) -> Answer<Vec<Ways>> {
        let mut readers: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
        let mut sources = Vec::new();
        for (i, (_, node)) in nodes.iter().enumerate() {
            sources.clear();
            node.sources(&mut sources);
            for s in &sources {
                readers[index[s]].push(i);
            }
        }
        let spec = Sparse {
            nodes: nodes.len(),
            readers: &|i, out| out.extend(&readers[i]),
            // A mapping's ways only grow, to at most `MAPPING_WAYS` and then any.
            max_visits: (MAPPING_WAYS + 2) as u32,
        };
        let transfer = |i: usize, facts: &Facts<Ways>| -> Option<Ways> {
            let fact_of = |v: &VarId| facts.get(index[v]);
            match &nodes[i].1 {
                Node::Known(ways) => Some(ways.clone()),
                Node::Leaf => Some(Ways::Any),
                Node::Either(inputs) => {
                    let mut known = inputs.iter().filter_map(fact_of);
                    let mut out = known.next()?.clone();
                    for fact in known {
                        out.join(fact);
                    }
                    Some(out)
                }
                Node::Parts(parts) => built_from(parts, fact_of),
            }
        };
        fixpoint(&self.walk_budget, spec, transfer).map(|facts| {
            let ways: Vec<Ways> = facts
                .into_vec()
                .into_iter()
                .map(|fact| fact.unwrap_or(Ways::Any))
                .collect();
            let mut kept = self.mappings.borrow_mut();
            for ((v, _), fact) in nodes.iter().zip(&ways) {
                kept.entry(*v).or_insert_with(|| fact.clone());
            }
            ways
        })
    }

    /// How the function builds the copy root `root` as a mapping, when it does and nothing
    /// changes it.
    fn mapping_node(&self, root: VarId) -> Node {
        match self.shape.def(root) {
            Def::Phi(b, i) => {
                let phi = &self.f.block(b).phis[i as usize];
                Node::Either(
                    phi.incoming
                        .iter()
                        .map(|(_, v)| self.shape.copy_root(*v))
                        .collect(),
                )
            }
            Def::Inst(..) => {
                let parts = self
                    .shape
                    .inst(root)
                    .and_then(|i| self.mapping_parts(&i.kind));
                match parts {
                    Some(parts) if self.only_unpacked(root) => Node::Parts(parts),
                    _ => Node::Leaf,
                }
            }
            Def::Param | Def::None => Node::Leaf,
        }
    }

    /// The parts of the mapping the instruction `kind` builds: a dict display, `dict(...)` of at
    /// most one mapping and keywords, or `a | b` of two mappings the function builds.
    fn mapping_parts(&self, kind: &InstKind) -> Option<Vec<Part>> {
        let merge = |v: &VarId| Part::Merge(self.shape.copy_root(*v));
        match kind {
            InstKind::DictLiteral { entries, .. } => Some(
                entries
                    .iter()
                    .map(|entry| match entry {
                        DictEntry::KeyValue { key, value } => match self.resolved_str(*key) {
                            Some(key) => Part::Entry(key, *value),
                            None => Part::Unknown,
                        },
                        DictEntry::Unpack { dict } => merge(dict),
                    })
                    .collect(),
            ),
            InstKind::BinOp {
                op: BinOpKind::BitOr,
                lhs,
                rhs,
                in_place: false,
                ..
            } if self.builds_mapping(*lhs) && self.builds_mapping(*rhs) => {
                Some(vec![merge(lhs), merge(rhs)])
            }
            _ => {
                let (args, kwargs) = self.dict_call(kind)?;
                let keywords = kwargs.iter().map(|(name, v)| match name {
                    Some(name) => Part::Entry(name.clone(), *v),
                    None => merge(v),
                });
                Some(args.iter().map(merge).chain(keywords).collect())
            }
        }
    }

    /// Whether the copy root of `v` is a mapping the function builds, or a phi that may be one
    /// ([`Self::mapping_parts`]), whatever its entries.
    fn builds_mapping(&self, v: VarId) -> bool {
        let root = self.shape.copy_root(v);
        match self.shape.inst(root).map(|i| &i.kind) {
            Some(InstKind::DictLiteral { .. }) => true,
            Some(InstKind::BinOp {
                op: BinOpKind::BitOr,
                in_place: false,
                ..
            }) => true,
            Some(kind) => self.dict_call(kind).is_some(),
            None => matches!(self.shape.def(root), Def::Phi(..)),
        }
    }

    /// The positional arguments and keywords of a call of `dict` with at most one positional
    /// argument and no `*` unpacking.
    #[allow(clippy::type_complexity)]
    fn dict_call<'k>(
        &self,
        kind: &'k InstKind,
    ) -> Option<(&'k [VarId], &'k [(Option<String>, VarId)])> {
        let InstKind::Call {
            callee_var: Some(c),
            args,
            kwargs,
            star_from: None,
            ..
        } = kind
        else {
            return None;
        };
        let dict = matches!(self.cx.resolve_call(self.types.get(c)),
            Callee::External(s) if s == "builtins.dict");
        (dict && args.len() <= 1).then_some((args.as_slice(), kwargs.as_slice()))
    }

    /// The argument a call reads as a mapping without keeping it: the one of `dict(m)`, and the
    /// field values of a library API building its receiver's class from a mapping
    /// (`model_validate(m)`, `model_copy(update=m)`).
    fn read_mapping(&self, kind: &InstKind) -> Option<VarId> {
        if let Some((args, _)) = self.dict_call(kind) {
            return args.first().copied();
        }
        let InstKind::MethodCall {
            recv,
            method,
            args,
            kwargs,
            star_from,
            ..
        } = kind
        else {
            return None;
        };
        let Callee::External(symbol) = self.cx.resolve_method(self.types.get(recv), method) else {
            return None;
        };
        let fields = self.cx.catalog(&symbol)?.builds_receiver.as_ref()?;
        let at = (fields.arg, fields.param.as_deref());
        (!fields.keywords)
            .then(|| resolver::passed_var(args, kwargs, *star_from, at))
            .flatten()
    }

    /// Whether no use of `root` can change the object it makes: each value holding it (`root`,
    /// its copies, and the phis merging them) is only copied, merged into another mapping (a
    /// `**` entry of a display, `dict(...)`, an operand of `|`), unpacked with `**` into a call,
    /// read as a mapping by a call building a class's instance from it, tested, returned, or
    /// raised.
    pub(super) fn only_unpacked(&self, root: VarId) -> bool {
        let shape = self.shape;
        let f = self.f;
        let keeps_entries = |v: VarId, u: &Use| match *u {
            Use::Inst(b, i) => match &f.block(b).insts[i as usize].kind {
                InstKind::Copy { .. }
                | InstKind::BinOp {
                    op: BinOpKind::BitOr,
                    in_place: false,
                    ..
                } => true,
                InstKind::DictLiteral { entries, .. } => entries.iter().all(|e| match e {
                    DictEntry::KeyValue { key, value } => *key != v && *value != v,
                    DictEntry::Unpack { .. } => true,
                }),
                kind @ (InstKind::Call { kwargs, .. } | InstKind::MethodCall { kwargs, .. }) => {
                    let unpacking = kwargs.iter().filter(|(k, x)| k.is_none() && *x == v);
                    let read = usize::from(self.read_mapping(kind) == Some(v));
                    kind.operands().iter().filter(|x| **x == v).count() == unpacking.count() + read
                }
                _ => false,
            },
            Use::Phi(..) | Use::Terminator(_) => true,
        };
        let holders = |v: VarId, out: &mut Vec<VarId>| {
            out.extend(shape.readers(v).iter().filter_map(|u| match *u {
                Use::Inst(b, i) => match &f.block(b).insts[i as usize].kind {
                    InstKind::Copy { dst, .. } => Some(*dst),
                    _ => None,
                },
                Use::Phi(b, i) => Some(f.block(b).phis[i as usize].dst),
                Use::Terminator(_) => None,
            }));
        };
        let walked = reach(&self.walk_budget, [root], holders, |v| {
            match shape.readers(v).iter().all(|u| keeps_entries(v, u)) {
                true => Visit::Expand,
                false => Visit::Stop(()),
            }
        });
        walked == Answer::Known(None)
    }
}

/// The ways a mapping built from `parts` may be built, from the ways of the mappings it merges
/// (`fact_of`; `None` until each has some).
fn built_from<'f>(parts: &[Part], fact_of: impl Fn(&VarId) -> Option<&'f Ways>) -> Option<Ways> {
    let mut ways = vec![Way::default()];
    for part in parts {
        let merged = match part {
            Part::Entry(key, value) => {
                ways.iter_mut().for_each(|w| w.set(key, *value));
                continue;
            }
            Part::Unknown => {
                ways.iter_mut().for_each(Way::forget);
                continue;
            }
            Part::Merge(v) => fact_of(v)?,
        };
        match merged {
            Ways::Built(merged) if ways.len() * merged.len() <= MAPPING_WAYS => {
                ways = ways
                    .iter()
                    .flat_map(|w| merged.iter().map(move |later| w.merged(later)))
                    .collect();
            }
            Ways::Built(_) => return Some(Ways::Any),
            Ways::Any => ways.iter_mut().for_each(Way::forget),
        }
    }
    Some(Ways::built(ways))
}
