//! `MUT112` ownership (docs/ANALYSIS_AND_KB_SPEC.md section 2.3): whether the object a value
//! holds is one the function makes itself, state that outlives the call, or a parameter, path
//! by path.
//!
//! [`source`] tells what the instruction defining a value makes of it. `heki_rules` applies it
//! forward over a function's values; the extractor applies it backward, from the values a
//! function returns ([`origins_of`]), into [`EffectSummary::return_origins`] and the calls whose
//! results they may be, which propagation resolves with what those calls return: a call's
//! result is classified across functions, through any number of wrappers and nested calls.
//!
//! [`EffectSummary::return_origins`]: crate::EffectSummary::return_origins

use crate::readonly::{self, COPY_METHODS, SLICE_COPYING_TYPES};
use crate::resolver::Callee;
use heki_ast::Span;
use heki_ir::{Inst, InstKind, IrFunction, PhiNode, VarId};
use heki_kb::EffectiveEcosystemKb;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

/// The kinds of object a value may hold, path by path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Origins {
    /// A new object made during the call: a display, a comprehension, a copy, a constructor's
    /// result, an operator's result.
    pub new: bool,
    /// State that outlives the call: an attribute, a global, a closure cell, an element of a
    /// parameter or of such state, or what a memoizing function returns.
    pub held: bool,
    /// Parameters (bit `i`: parameter `i < 64`) it may be, as themselves.
    pub params: u64,
    /// Parameters it may be an element of (an item, an attribute, an iteration step): the
    /// caller's argument holds it.
    pub elements: u64,
}

impl Origins {
    /// What either value may be.
    pub fn join(&mut self, other: Self) {
        self.new |= other.new;
        self.held |= other.held;
        self.params |= other.params;
        self.elements |= other.elements;
    }

    /// What an element of a value of these origins may be: an element of a parameter it may be,
    /// or held state (an element of a new object is of unknown origin).
    pub fn element(self) -> Self {
        Self {
            held: self.held,
            elements: self.params | self.elements,
            ..Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A project call of the same function whose result an object may be (`MUT112`), by the span
/// of the call site: the result itself, or an element of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResultOf {
    pub span: Span,
    pub element: bool,
}

/// What an object may be as the function holding it sees it (`MUT112`): the origins it knows,
/// the project calls whose results it may be, which propagation resolves with what their
/// callees return, and the phis it may be, by their positions in the function's
/// `ReturnOrigins::nodes` (each phi is walked once, for every value that may be it).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LocalOrigins {
    pub known: Origins,
    pub results: Vec<ResultOf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<u32>,
}

impl LocalOrigins {
    pub fn is_empty(&self) -> bool {
        self.known.is_empty() && self.results.is_empty() && self.nodes.is_empty()
    }
}

/// `MUT112`'s facts of one function ([`EffectSummary::return_origins`]): what the object it
/// returns may be, and what each phi its origin walks meet may be.
///
/// [`EffectSummary::return_origins`]: crate::EffectSummary::return_origins
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReturnOrigins {
    /// The returned object: its known origins, path by path (propagation adds what the project
    /// calls in `results` return), the project calls whose results it may be, and the phis it
    /// may be (local).
    pub returned: LocalOrigins,
    /// What each phi the origin walks of the function meet may be, at the positions that
    /// [`LocalOrigins::nodes`] and the arguments' [`crate::ArgOrigin::origins`] name, each walked
    /// once (local).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<LocalOrigins>,
}

/// The facts of a function that returns nothing `MUT112` knows of.
pub(crate) static NO_RETURN_ORIGINS: ReturnOrigins = ReturnOrigins {
    returned: LocalOrigins {
        known: Origins {
            new: false,
            held: false,
            params: 0,
            elements: 0,
        },
        results: Vec::new(),
        nodes: Vec::new(),
    },
    nodes: Vec::new(),
};

impl ReturnOrigins {
    pub fn is_empty(&self) -> bool {
        self.returned.is_empty() && self.nodes.is_empty()
    }
}

/// The phis the origin walks of one function meet ([`origins_of`]), each as itself or as an
/// element: a node walked once, which the walks refer to by its position.
#[derive(Default)]
pub(crate) struct OriginNodes {
    ids: FxHashMap<(VarId, Part), u32>,
    keys: Vec<(VarId, Part)>,
    walked: Vec<LocalOrigins>,
    pending: Vec<u32>,
    /// The values the walks took, each walk counting its own.
    pub(crate) steps: usize,
}

impl OriginNodes {
    fn id(&mut self, key: (VarId, Part)) -> u32 {
        if let Some(&id) = self.ids.get(&key) {
            return id;
        }
        let id = u32::try_from(self.keys.len()).unwrap_or(u32::MAX);
        self.ids.insert(key, id);
        self.keys.push(key);
        self.walked.push(LocalOrigins::default());
        self.pending.push(id);
        id
    }

    /// A node met and not walked yet, with the value and part to walk from.
    pub(crate) fn next_pending(&mut self) -> Option<(u32, (VarId, Part))> {
        let id = self.pending.pop()?;
        Some((id, self.keys[id as usize]))
    }

    pub(crate) fn set(&mut self, id: u32, origins: LocalOrigins) {
        self.walked[id as usize] = origins;
    }

    /// The nodes' origins, by position.
    pub(crate) fn into_origins(self) -> Vec<LocalOrigins> {
        self.walked
    }
}

/// State outliving a call that an instruction reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holder<'i> {
    /// `obj.<attr>`.
    Attr { obj: VarId, attr: &'i str },
    /// `getattr(obj, name)`: the attribute the operand `name` names.
    NamedAttr { obj: VarId, name: VarId },
    /// A module global or a builtin.
    Global(&'i str),
    /// A variable of an enclosing function.
    Cell(&'i str),
}

/// What the instruction defining a value makes of the object it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source<'i> {
    /// A new object.
    New,
    /// A new object holding the elements of the operands: a shallow copy of them
    /// ([`readonly::copy_of`]).
    Copy(&'i [VarId]),
    /// State that outlives the call.
    Held(Holder<'i>),
    /// The operand's object: a copy of the reference, `typing.cast`, an in-place operator.
    Same(VarId),
    /// An element of the operand: an item, `get` / `pop` / `setdefault`, an iteration step.
    Element(VarId),
    /// An element of the first operand, or the second when the key is missing:
    /// `d.get(k, default)`, `d.pop(k, default)`.
    ElementOr(VarId, VarId),
    /// An attribute, or the operand when it is missing: `getattr(obj, name, default)`.
    HeldOr(Holder<'i>, VarId),
    /// A slice of the operand: a new sequence for the builtin sequences (and a value of unknown
    /// class, far more often a list), possibly a view of it for another class (`numpy`).
    Slice(VarId),
    /// What the project callees return: their summaries say, in propagation.
    Project,
    Unknown,
}

/// Calls of these return a new object.
const NEW_OBJECT_CALLS: &[&str] = &[
    "builtins.list",
    "builtins.dict",
    "builtins.set",
    "builtins.frozenset",
    "builtins.tuple",
    "builtins.bytearray",
    "builtins.sorted",
    "builtins.dict.fromkeys",
    "collections.defaultdict",
    "collections.OrderedDict",
    "collections.Counter",
    "collections.deque",
    "collections.ChainMap",
    "copy.copy",
    "copy.deepcopy",
    "copy.replace",
    "dataclasses.replace",
];

/// `typing.cast(T, value)` returns `value`.
pub(crate) const CASTS: &[&str] = &["typing.cast", "typing_extensions.cast"];

/// The argument a call of the library function `symbol` with the positional arguments `args`
/// (unpacked from `star_from` on) hands back as itself: the value of `typing.cast(T, value)`, a
/// type annotation in call form that returns its value unchanged at run time. Every reader of "which object is this value" (the alias facts of
/// changes and retention, [`source`], the references a function hands on) takes it from here; a
/// project function returning its parameter on every path is the declarations' answer
/// ([`crate::FunctionDecl::returns_param`]).
pub(crate) fn handed_back(symbol: &str, args: &[VarId], star_from: Option<u8>) -> Option<VarId> {
    const VALUE: usize = 1;
    // `cast(T, *values)` hands back what the unpacking binds to the value, not the iterable.
    let bound = star_from.is_none_or(|s| usize::from(s) > VALUE);
    (CASTS.contains(&symbol) && bound).then(|| args.get(VALUE).copied())?
}

/// What the instruction `kind` of `f` makes of the object its result holds. A call's resolution
/// is `callee` (`None` for any other instruction); `constructs` says the call constructs a
/// project class. `made_here` tells the values holding an object the function makes itself
/// ([`made_here`]): an attribute of such an object is one of its elements, not state that
/// outlives the call.
pub fn source<'i>(
    f: &IrFunction,
    kind: &'i InstKind,
    callee: Option<&Callee>,
    constructs: bool,
    kb: &EffectiveEcosystemKb,
    made_here: &dyn Fn(VarId) -> bool,
) -> Source<'i> {
    let copy = || {
        let callee = callee.unwrap_or(&Callee::Unresolved);
        readonly::copy_of(f, kind, callee, kb).map(|c| {
            if c.deep {
                Source::New
            } else {
                Source::Copy(c.operands)
            }
        })
    };
    match kind {
        InstKind::AllocBuiltin { .. }
        | InstKind::DictLiteral { .. }
        | InstKind::Convert { .. }
        | InstKind::Format { .. }
        | InstKind::BuildString { .. } => copy().unwrap_or(Source::New),
        InstKind::BinOp {
            lhs,
            in_place: true,
            ..
        } => Source::Same(*lhs),
        InstKind::BinOp { .. } => Source::New,
        InstKind::Copy { src, .. } => Source::Same(*src),
        InstKind::LoadGlobal { name, .. } => Source::Held(Holder::Global(name)),
        InstKind::LoadDeref { name, .. } => Source::Held(Holder::Cell(name)),
        InstKind::AttrLoad { obj, .. } if made_here(*obj) => Source::Element(*obj),
        InstKind::AttrLoad { obj, attr, .. } => Source::Held(Holder::Attr { obj: *obj, attr }),
        InstKind::SubscriptLoad {
            obj,
            is_slice: true,
            ..
        } => Source::Slice(*obj),
        InstKind::SubscriptLoad { obj, .. } => Source::Element(*obj),
        InstKind::MakeIter { iterable, .. } => Source::Same(*iterable),
        InstKind::IterNext { iter, .. } => Source::Element(*iter),
        InstKind::Call { callee: name, .. } if name.starts_with("__comp_") => Source::New,
        InstKind::Call {
            args, star_from, ..
        }
        | InstKind::MethodCall {
            args, star_from, ..
        } => {
            if constructs {
                return Source::New;
            }
            if let Some(Callee::Project(..)) = callee {
                return Source::Project;
            }
            if let Some(copied) = copy() {
                return copied;
            }
            let symbol = match callee {
                Some(Callee::External(s)) => s.as_str(),
                _ => "",
            };
            if NEW_OBJECT_CALLS.contains(&symbol)
                || kb
                    .lookup(symbol)
                    .is_some_and(|e| e.copy_sanitizer.is_some())
            {
                return Source::New;
            }
            if let Some(value) = handed_back(symbol, args, *star_from) {
                return Source::Same(value);
            }
            // `getattr(*spec)` names neither its object nor its default: unknown.
            if symbol == "builtins.getattr"
                && let [obj, name, ..] = args.as_slice()
            {
                let held = Holder::NamedAttr {
                    obj: *obj,
                    name: *name,
                };
                return match (args.get(2), made_here(*obj)) {
                    (Some(default), true) => Source::ElementOr(*obj, *default),
                    (Some(default), false) => Source::HeldOr(held, *default),
                    (None, true) => Source::Element(*obj),
                    (None, false) => Source::Held(held),
                };
            }
            match kind {
                InstKind::MethodCall { recv, method, .. } => method_source(*recv, method, args),
                _ => Source::Unknown,
            }
        }
        _ => Source::Unknown,
    }
}

/// The one value the instruction `kind` of `f` copies, shallowly or deeply, with the call's
/// resolution `callee` ([`readonly::copy_of`]): the receiver of a copy method (`x.copy()`), or
/// the argument of a copying call given it alone (`list(x)`, `copy.deepcopy(x)`).
pub fn copied_operand(
    f: &IrFunction,
    kind: &InstKind,
    callee: Option<&Callee>,
    kb: &EffectiveEcosystemKb,
) -> Option<VarId> {
    let callee = callee.unwrap_or(&Callee::Unresolved);
    let copy = readonly::copy_of(f, kind, callee, kb)?;
    copy.operands
        .first()
        .copied()
        .filter(|_| copy.operands.len() == 1)
}

/// The [`source`] of an instruction, given the values that hold an object the function makes
/// itself ([`made_here`]).
pub type SourceGiven<'i, 'f> = dyn Fn(&'i Inst, &dyn Fn(VarId) -> bool) -> Source<'i> + 'f;

/// Whether the object a variable holds is one the function makes itself: `root`, the value the
/// variable copies through any number of copies, is defined as a new object. Whether an
/// instruction makes a new object never depends on what its operands hold, so `source_of` is
/// asked with none made here, and the question does not go on through the operands
/// (`x = x.child`, thousands deep).
pub fn made_here<'i>(
    root: VarId,
    defs: &FxHashMap<VarId, &'i Inst>,
    source_of: &SourceGiven<'i, '_>,
) -> bool {
    defs.get(&root)
        .is_some_and(|inst| matches!(source_of(inst, &|_| false), Source::New | Source::Copy(_)))
}

/// [`source`] of a method call outside the project, by the method's name.
fn method_source<'i>(recv: VarId, method: &str, args: &[VarId]) -> Source<'i> {
    if COPY_METHODS.contains(&method) {
        return Source::New;
    }
    match (method, args) {
        ("get" | "pop", [_, default, ..]) => Source::ElementOr(recv, *default),
        ("get" | "pop" | "__getitem__" | "setdefault", [_, ..]) => Source::Element(recv),
        _ => Source::Unknown,
    }
}

/// Whether a slice of an object of class `class` (`None`: unknown) is a new sequence.
pub fn slice_copies(class: Option<&str>) -> bool {
    class.is_none_or(|c| SLICE_COPYING_TYPES.contains(&c))
}

/// Methods that keep the object handed to them in their receiver (as an element, or the default
/// of `setdefault`).
pub const STORING_METHODS: &[&str] = &["append", "appendleft", "insert", "add", "setdefault"];

/// Where an instruction stores an object: in another object (a display, an attribute, an item,
/// `setattr`, a container method), in a global, or in a closure cell. The object is part of that
/// state from then on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Place<'i> {
    Object(VarId),
    Global(&'i str),
    Cell(&'i str),
}

/// The values the instruction `kind` stores, and where.
pub fn stores(kind: &InstKind) -> Vec<(VarId, Place<'_>)> {
    match kind {
        InstKind::AttrStore { obj, value, .. } | InstKind::SubscriptStore { obj, value, .. } => {
            vec![(*value, Place::Object(*obj))]
        }
        InstKind::StoreGlobal { name, value } => vec![(*value, Place::Global(name))],
        InstKind::StoreDeref { name, value } => vec![(*value, Place::Cell(name))],
        InstKind::AllocBuiltin { dst, elems, .. } => {
            elems.iter().map(|v| (*v, Place::Object(*dst))).collect()
        }
        InstKind::DictLiteral { dst, entries } => entries
            .iter()
            .filter_map(|e| match e {
                heki_ir::DictEntry::KeyValue { value, .. } => Some((*value, Place::Object(*dst))),
                heki_ir::DictEntry::Unpack { .. } => None,
            })
            .collect(),
        InstKind::MethodCall {
            recv, method, args, ..
        } if STORING_METHODS.contains(&method.as_str()) => args
            .last()
            .map(|v| (*v, Place::Object(*recv)))
            .into_iter()
            .collect(),
        InstKind::Call { callee, args, .. } if callee == "setattr" && args.len() == 3 => {
            vec![(args[2], Place::Object(args[0]))]
        }
        _ => Vec::new(),
    }
}

/// The blocks of a function a run can reach and the edges it can take: a branch on a constant
/// takes one side (`while True:` never leaves at its header), and a block only such an untaken
/// edge leads to never runs. Exception edges are taken.
pub struct Live {
    blocks: Vec<bool>,
    dead: FxHashSet<(heki_ir::BlockId, heki_ir::BlockId)>,
}

impl Live {
    pub fn of(f: &heki_ir::IrFunction, defs: &FxHashMap<VarId, &Inst>) -> Self {
        let dead = dead_edges(f, defs);
        let mut blocks = vec![false; f.blocks.len()];
        let mut stack: Vec<heki_ir::BlockId> = f.blocks.first().map(|b| b.id).into_iter().collect();
        while let Some(b) = stack.pop() {
            let i = b.0 as usize;
            if i >= blocks.len() || blocks[i] {
                continue;
            }
            blocks[i] = true;
            let block = f.block(b);
            stack.extend(
                block
                    .terminator
                    .successors()
                    .into_iter()
                    .filter(|to| !dead.contains(&(b, *to)))
                    .chain(block.exc_target),
            );
        }
        Self { blocks, dead }
    }

    /// Whether a run can reach `b`.
    pub fn block(&self, b: heki_ir::BlockId) -> bool {
        self.blocks[b.0 as usize]
    }

    /// Whether a run can take the edge `from -> to`.
    pub fn edge(&self, from: heki_ir::BlockId, to: heki_ir::BlockId) -> bool {
        self.block(from) && !self.dead.contains(&(from, to))
    }
}

/// The edges of `f` a branch on a constant never takes, by `(from, to)` block.
fn dead_edges(
    f: &heki_ir::IrFunction,
    defs: &FxHashMap<VarId, &Inst>,
) -> FxHashSet<(heki_ir::BlockId, heki_ir::BlockId)> {
    let mut out = FxHashSet::default();
    for b in &f.blocks {
        let heki_ir::Terminator::Branch {
            cond,
            then_bb,
            else_bb,
        } = b.terminator
        else {
            continue;
        };
        if let Some(InstKind::Const {
            literal: heki_ir::ConstLiteral::Bool(taken),
            ..
        }) = defs.get(&cond).map(|i| &i.kind)
        {
            out.insert((b.id, if *taken { else_bb } else { then_bb }));
        }
    }
    out
}

/// What a parameter is to the function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamRole {
    /// Parameter `i`: the caller's object.
    Plain(u8),
    /// `*args` / `**kwargs`: a new tuple or dict holding the caller's objects.
    Packed,
}

/// A value of the backward walk: the object itself, or one of its elements.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Part {
    Object,
    Element,
}

/// The origins of the objects the `roots` hold: a walk back through copies, phis (along the
/// edges a run can take, `live`), casts, in-place operators, iterations, items, and the
/// defaults of `get` / `pop` / `getattr`, which classifies what each value's definition
/// ([`source`] by `source_of`) or parameter (`param_of`) makes of it. An element of a new object
/// is of unknown origin, one of a parameter is the caller's, and one of held state is held; an
/// object the function stores in another (`kept`, through copies and phis) is an element of that
/// one, and one stored in a global or a closure cell is held. A project call's result is one of
/// the `results`, which propagation resolves with what its callees return. A phi other than a
/// root is one of the `nodes` ([`OriginNodes`]), walked once from it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn origins_of<'i>(
    defs: &FxHashMap<VarId, &'i Inst>,
    phis: &FxHashMap<VarId, (heki_ir::BlockId, &PhiNode)>,
    live: &Live,
    roots: &[(VarId, Part)],
    source_of: impl Fn(&'i Inst) -> Source<'i>,
    param_of: impl Fn(VarId) -> Option<ParamRole>,
    slice_copies: impl Fn(VarId) -> bool,
    kept: &FxHashMap<VarId, Vec<Place<'_>>>,
    nodes: &mut OriginNodes,
) -> LocalOrigins {
    let mut out = LocalOrigins::default();
    let mut seen: FxHashSet<(VarId, Part)> = FxHashSet::default();
    let mut work: Vec<(VarId, Part)> = roots.to_vec();
    while let Some((var, part)) = work.pop() {
        if !seen.insert((var, part)) {
            continue;
        }
        nodes.steps += 1;
        if let Some(role) = param_of(var) {
            match (role, part) {
                (ParamRole::Plain(i), Part::Object) if i < 64 => out.known.params |= 1u64 << i,
                (ParamRole::Plain(i), Part::Element) if i < 64 => {
                    out.known.elements |= 1u64 << i;
                }
                (ParamRole::Packed, Part::Object) => out.known.new = true,
                _ => out.known.held = true,
            }
            continue;
        }
        if let Some(into) = kept.get(&var) {
            for place in into {
                match place {
                    Place::Object(c) => work.push((*c, Part::Element)),
                    Place::Global(_) | Place::Cell(_) => out.known.held = true,
                }
            }
            continue;
        }
        if let Some((block, phi)) = phis.get(&var) {
            if roots.contains(&(var, part)) {
                work.extend(
                    phi.incoming
                        .iter()
                        .filter(|(from, _)| live.edge(*from, *block))
                        .map(|(_, v)| (*v, part)),
                );
            } else {
                out.nodes.push(nodes.id((var, part)));
            }
            continue;
        }
        let source = defs.get(&var).map_or(Source::Unknown, |i| source_of(i));
        match source {
            Source::New => out.known.new |= part == Part::Object,
            Source::Copy(copied) => match part {
                Part::Object => out.known.new = true,
                Part::Element => work.extend(copied.iter().map(|v| (*v, Part::Element))),
            },
            Source::Held(_) => out.known.held = true,
            Source::Same(v) => work.push((v, part)),
            Source::Element(v) => work.push((v, Part::Element)),
            Source::ElementOr(v, default) => {
                work.push((v, Part::Element));
                work.push((default, part));
            }
            Source::HeldOr(_, default) => {
                out.known.held = true;
                work.push((default, part));
            }
            Source::Slice(v) if slice_copies(v) => out.known.new |= part == Part::Object,
            Source::Slice(v) => work.push((v, part)),
            Source::Project => out.results.push(ResultOf {
                span: defs[&var].span,
                element: part == Part::Element,
            }),
            Source::Unknown => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_join_keeps_every_kind() {
        let mut a = Origins {
            new: true,
            ..Origins::default()
        };
        a.join(Origins {
            held: true,
            params: 0b10,
            ..Origins::default()
        });
        assert_eq!(
            a,
            Origins {
                new: true,
                held: true,
                params: 0b10,
                elements: 0,
            }
        );
        assert!(!a.is_empty());
        assert!(Origins::default().is_empty());
        // An element of a new object is of unknown origin; of a parameter, the caller's.
        assert_eq!(
            a.element(),
            Origins {
                held: true,
                elements: 0b10,
                ..Origins::default()
            }
        );
    }

    #[test]
    fn methods_outside_the_project_by_name() {
        let (r, k, d) = (VarId(1), VarId(2), VarId(3));
        assert_eq!(method_source(r, "get", &[k, d]), Source::ElementOr(r, d));
        assert_eq!(method_source(r, "pop", &[k, d]), Source::ElementOr(r, d));
        assert_eq!(method_source(r, "get", &[k]), Source::Element(r));
        assert_eq!(method_source(r, "setdefault", &[k, d]), Source::Element(r));
        assert_eq!(method_source(r, "copy", &[]), Source::New);
        assert_eq!(method_source(r, "keys", &[]), Source::Unknown);
        assert!(slice_copies(None));
        assert!(slice_copies(Some("builtins.list")));
        assert!(!slice_copies(Some("numpy.ndarray")));
    }
}
