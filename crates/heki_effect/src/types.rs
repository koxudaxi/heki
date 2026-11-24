//! Value types of the Advisory lane's receiver typing: classes with the arguments of known
//! generic containers (`list[Model]`, `dict[str, Model]`, `tuple[Model, int]`,
//! `Iterator[Model]`), so that the items a value yields when it is iterated, subscripted, or
//! unpacked have a class too. Types are hints for finding call targets and the classes of
//! accessed attributes; they never reach the Soundness lane (ADR-004).

use heki_ir::walk::Join;
use heki_ir::{BinOpKind, BuiltinKind, InstKind, VarId};
use serde::{Deserialize, Serialize};

/// Deepest nesting of tracked type arguments (`dict[str, list[Model]]` nests 2 deep); deeper
/// arguments are dropped, so loops that build nested containers stay finite.
pub(crate) const MAX_DEPTH: usize = 4;

/// Most classes a union keeps (`Model | Field | Node`); a value of more classes has no tracked
/// type.
pub(crate) const MAX_UNION: usize = 3;

/// Most named objects a [`Ty::OneOf`] keeps: the targets a call of the value may run, as many as
/// a call through a class hierarchy follows (`resolver::CHA_CANDIDATE_LIMIT`).
pub(crate) const MAX_NAMED: usize = 8;

/// What the Advisory lane knows about a value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Ty {
    /// A named object: a module, class, function, or external symbol (by fqn).
    Ref(String),
    /// A class object that is the class `fqn` or one of its subclasses (`cls` in a class
    /// method): what it constructs and calls dispatches along the class hierarchy.
    Class(String),
    /// `super()` inside a method of the class `fqn`.
    Super(String),
    /// An instance of one class.
    Inst(Obj),
    /// `typing.Self` in a declaration: the type of the receiver.
    SelfType,
    /// A `functools.partial(f, ...)` object: calling it calls `f`.
    Partial(Box<Ty>),
    /// The coroutine a call of an `async def` returns: awaiting it gives the inner type, what
    /// the function's declaration says it returns.
    Coroutine(Box<Ty>),
    /// An instance of one of two or three classes (sorted by class, one entry per class).
    Union(Vec<Obj>),
    /// One of two to [`MAX_NAMED`] named objects, by fqn (sorted): what several paths bind, or
    /// an item of a table of functions (`HANDLERS[kind]`). Calling it calls one of them.
    OneOf(Vec<String>),
}

/// An instance of a class, with the type arguments of a known generic container.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Obj {
    pub class: String,
    pub args: Args,
}

/// The tracked type arguments of an instance.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Args {
    /// None tracked.
    #[default]
    None,
    /// The items iterating the value yields (a sequence, set, iterator, generator, view, or
    /// `tuple[T, ...]`): also what an integer subscript of a sequence returns.
    Items(Box<Elem>),
    /// A mapping: iterating it yields keys, a subscript returns a value.
    Map(Box<Elem>, Box<Elem>),
    /// A tuple of known length, by position.
    Tuple(Vec<Elem>),
    /// A future (`FUTURE_CLASSES`): what awaiting it, or its `result()`, gives.
    Result(Box<Elem>),
}

/// The type of the items in one slot of a container.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Elem {
    /// No item can be there (an empty display).
    Empty,
    Of(Ty),
    Unknown,
}

/// What the arguments of a generic container class mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Generic {
    /// `C[T]`: iterating yields `T`.
    Items,
    /// `C[K, V]`: a mapping.
    Map,
    /// `ItemsView[K, V]`: iterating yields `tuple[K, V]`.
    Pairs,
    /// `Counter[K]`: a mapping from `K` to `int`.
    Counter,
    /// `tuple[A, B]` / `tuple[T, ...]`.
    Tuple,
}

/// Generic containers by the fqn an annotation resolves to (`typing.List` and the other
/// aliases of builtins resolve to the builtin).
const GENERICS: &[(&str, Generic)] = &[
    ("builtins.list", Generic::Items),
    ("builtins.set", Generic::Items),
    ("builtins.frozenset", Generic::Items),
    ("builtins.dict", Generic::Map),
    ("builtins.tuple", Generic::Tuple),
    ("collections.deque", Generic::Items),
    ("collections.defaultdict", Generic::Map),
    ("collections.OrderedDict", Generic::Map),
    ("collections.ChainMap", Generic::Map),
    ("collections.Counter", Generic::Counter),
    ("typing.Sequence", Generic::Items),
    ("typing.MutableSequence", Generic::Items),
    ("typing.Iterable", Generic::Items),
    ("typing.Iterator", Generic::Items),
    ("typing.Collection", Generic::Items),
    ("typing.Reversible", Generic::Items),
    ("typing.AbstractSet", Generic::Items),
    ("typing.MutableSet", Generic::Items),
    ("typing.KeysView", Generic::Items),
    ("typing.ValuesView", Generic::Items),
    ("typing.ItemsView", Generic::Pairs),
    ("typing.AsyncIterable", Generic::Items),
    ("typing.AsyncIterator", Generic::Items),
    ("typing.Generator", Generic::Items),
    ("typing.AsyncGenerator", Generic::Items),
    ("typing.Deque", Generic::Items),
    ("typing.Mapping", Generic::Map),
    ("typing.MutableMapping", Generic::Map),
    ("typing.ChainMap", Generic::Map),
    ("typing.Counter", Generic::Counter),
    ("collections.abc.Sequence", Generic::Items),
    ("collections.abc.MutableSequence", Generic::Items),
    ("collections.abc.Iterable", Generic::Items),
    ("collections.abc.Iterator", Generic::Items),
    ("collections.abc.Collection", Generic::Items),
    ("collections.abc.Reversible", Generic::Items),
    ("collections.abc.Set", Generic::Items),
    ("collections.abc.MutableSet", Generic::Items),
    ("collections.abc.KeysView", Generic::Items),
    ("collections.abc.ValuesView", Generic::Items),
    ("collections.abc.ItemsView", Generic::Pairs),
    ("collections.abc.AsyncIterable", Generic::Items),
    ("collections.abc.AsyncIterator", Generic::Items),
    ("collections.abc.Generator", Generic::Items),
    ("collections.abc.AsyncGenerator", Generic::Items),
    ("collections.abc.Mapping", Generic::Map),
    ("collections.abc.MutableMapping", Generic::Map),
];

/// The generic container `class` is, if any.
pub(crate) fn generic(class: &str) -> Option<Generic> {
    GENERICS.iter().find(|(c, _)| *c == class).map(|(_, g)| *g)
}

/// Augmented assignments changing a container in place, by the container's class: `xs += ys`
/// extends a list, `s |= t` updates a set (`__iadd__`, `__ior__`, ...).
const IN_PLACE_OPERATORS: &[(&str, &[BinOpKind])] = &[
    ("builtins.list", &[BinOpKind::Add, BinOpKind::Mult]),
    ("builtins.bytearray", &[BinOpKind::Add, BinOpKind::Mult]),
    (
        "builtins.set",
        &[
            BinOpKind::BitOr,
            BinOpKind::BitAnd,
            BinOpKind::Sub,
            BinOpKind::BitXor,
        ],
    ),
    ("builtins.dict", &[BinOpKind::BitOr]),
    ("collections.deque", &[BinOpKind::Add, BinOpKind::Mult]),
    ("collections.OrderedDict", &[BinOpKind::BitOr]),
    ("collections.defaultdict", &[BinOpKind::BitOr]),
    (
        "collections.Counter",
        &[
            BinOpKind::Add,
            BinOpKind::Sub,
            BinOpKind::BitOr,
            BinOpKind::BitAnd,
        ],
    ),
];

/// Whether `x op= y` changes the container `x`, an instance of `class`, in place, and so gives
/// the container itself.
pub(crate) fn changes_in_place(class: &str, op: BinOpKind) -> bool {
    IN_PLACE_OPERATORS
        .iter()
        .any(|(c, ops)| *c == class && ops.contains(&op))
}

/// The `typing` generic an unresolved annotation name means (`Iterable` in a string
/// annotation that never imports it), as `typing.<name>`.
pub(crate) fn typing_generic(name: &str) -> Option<String> {
    let fqn = format!("typing.{name}");
    generic(&fqn).map(|_| fqn)
}

impl Ty {
    /// An instance of `class` without tracked arguments.
    pub fn inst(class: impl Into<String>) -> Self {
        Ty::Inst(Obj {
            class: class.into(),
            args: Args::None,
        })
    }

    /// An instance of `class` with `args`, which are dropped when they nest too deep.
    pub(crate) fn container(class: &str, args: Args) -> Self {
        let args = if args.depth() > MAX_DEPTH {
            Args::None
        } else {
            args
        };
        Ty::Inst(Obj {
            class: class.to_owned(),
            args,
        })
    }

    /// The instance a value is (`None` for named objects, `super()`, partial objects, and
    /// unions).
    pub fn obj(&self) -> Option<&Obj> {
        match self {
            Ty::Inst(o) => Some(o),
            Ty::Ref(_)
            | Ty::Class(_)
            | Ty::Super(_)
            | Ty::SelfType
            | Ty::Partial(_)
            | Ty::Coroutine(_)
            | Ty::Union(_)
            | Ty::OneOf(_) => None,
        }
    }

    /// The instances a value may be: one for an instance, two or three for a union.
    pub(crate) fn members(&self) -> &[Obj] {
        match self {
            Ty::Inst(o) => std::slice::from_ref(o),
            Ty::Union(os) => os,
            Ty::Ref(_)
            | Ty::Class(_)
            | Ty::Super(_)
            | Ty::SelfType
            | Ty::Partial(_)
            | Ty::Coroutine(_)
            | Ty::OneOf(_) => &[],
        }
    }

    /// The type of a value that is one of `objs` (joined by class): an instance, a union of at
    /// most [`MAX_UNION`] classes, or `None` for none or more.
    pub(crate) fn of_members(objs: impl IntoIterator<Item = Obj>) -> Option<Ty> {
        let mut out: Vec<Obj> = Vec::new();
        for o in objs {
            match out.binary_search_by(|x| x.class.cmp(&o.class)) {
                Ok(i) => out[i].args = out[i].args.join(&o.args),
                Err(i) => out.insert(i, o),
            }
        }
        match out.len() {
            1 => out.pop().map(Ty::Inst),
            n if (2..=MAX_UNION).contains(&n) => Some(Ty::Union(out)),
            _ => None,
        }
    }

    /// Maps every instance the value may be (a union member by member).
    pub(crate) fn map_members(&self, f: impl Fn(&Obj) -> Option<Obj>) -> Option<Ty> {
        match self {
            Ty::Inst(o) => f(o).map(Ty::Inst),
            Ty::Union(os) => Ty::of_members(os.iter().map(f).collect::<Option<Vec<_>>>()?),
            Ty::Ref(_)
            | Ty::Class(_)
            | Ty::Super(_)
            | Ty::SelfType
            | Ty::Partial(_)
            | Ty::Coroutine(_)
            | Ty::OneOf(_) => Some(self.clone()),
        }
    }

    /// The type with the items of every instance unknown: what a container holds once code
    /// heki does not follow item by item adds to it.
    pub(crate) fn without_items(&self) -> Ty {
        self.map_members(|o| {
            Some(Obj {
                class: o.class.clone(),
                args: Args::None,
            })
        })
        .unwrap_or_else(|| self.clone())
    }

    /// The type once changes in place have added `added` to the value: each list, deque, set,
    /// or dict joins the items, or the keys and values, of its kind; any other instance keeps no
    /// items.
    pub(crate) fn with_added(&self, added: &Added) -> Ty {
        self.map_members(|o| {
            let args = match (Container::of_class(&o.class), &o.args) {
                (Some(Container::Sequence), Args::Items(e)) => Args::items(e.join(&added.sequence)),
                (Some(Container::Set), Args::Items(e)) => Args::items(e.join(&added.set)),
                (Some(Container::Map), Args::Map(k, v)) => {
                    Args::map(k.join(&added.keys), v.join(&added.values))
                }
                _ => Args::None,
            };
            let args = if args.depth() > MAX_DEPTH {
                Args::None
            } else {
                args
            };
            Some(Obj {
                class: o.class.clone(),
                args,
            })
        })
        .unwrap_or_else(|| self.clone())
    }

    /// The declared type with `Self` bound to the receiver type `recv`, or `None` when it
    /// mentions `Self` without a receiver; an item mentioning it is then unknown.
    pub(crate) fn bind_self(&self, recv: Option<&Ty>) -> Option<Ty> {
        match self {
            Ty::SelfType => recv.cloned(),
            _ => self.map_members(|o| {
                Some(Obj {
                    class: o.class.clone(),
                    args: o.args.map_elems(|t| t.bind_self(recv)),
                })
            }),
        }
    }

    /// The least type covering both: the instances of both (one class with joined arguments,
    /// or a union of a few), or `None` when no tracked type does.
    pub fn join(&self, other: &Ty) -> Option<Ty> {
        if self == other {
            return Some(self.clone());
        }
        match (self, other) {
            (Ty::Inst(a), Ty::Inst(b)) if a.class == b.class => Some(Ty::Inst(Obj {
                class: a.class.clone(),
                args: a.args.join(&b.args),
            })),
            (Ty::Ref(a), Ty::Class(b)) | (Ty::Class(b), Ty::Ref(a)) if a == b => {
                Some(Ty::Class(b.clone()))
            }
            (Ty::Ref(_) | Ty::OneOf(_), Ty::Ref(_) | Ty::OneOf(_)) => {
                Ty::one_of(self.named().iter().chain(other.named()).cloned())
            }
            // Either coroutine may be awaited: the result is either declared result.
            (Ty::Coroutine(a), Ty::Coroutine(b)) => a.join(b).map(|t| Ty::Coroutine(Box::new(t))),
            // Catalog awaitables carry their awaited result: join by it (EFF107 only misses).
            (Ty::Coroutine(a), b) | (b, Ty::Coroutine(a)) => a.join(b),
            _ => match (self.members(), other.members()) {
                (a, b) if !a.is_empty() && !b.is_empty() => {
                    Ty::of_members(a.iter().chain(b).cloned())
                }
                _ => None,
            },
        }
    }

    fn depth(&self) -> usize {
        match self {
            Ty::Inst(o) => 1 + o.args.depth(),
            Ty::Union(os) => 1 + os.iter().map(|o| o.args.depth()).max().unwrap_or(0),
            Ty::Partial(t) | Ty::Coroutine(t) => 1 + t.depth(),
            Ty::Ref(_) | Ty::Class(_) | Ty::Super(_) | Ty::SelfType | Ty::OneOf(_) => 1,
        }
    }

    /// The named objects a value is one of: one for a named object, those of a [`Ty::OneOf`].
    pub(crate) fn named(&self) -> &[String] {
        match self {
            Ty::Ref(fqn) => std::slice::from_ref(fqn),
            Ty::OneOf(fqns) => fqns,
            _ => &[],
        }
    }

    /// The value that is one of the named objects `fqns`: one of them, a [`Ty::OneOf`] of at
    /// most [`MAX_NAMED`], or `None` for none or more.
    pub(crate) fn one_of(fqns: impl IntoIterator<Item = String>) -> Option<Ty> {
        let mut out: Vec<String> = fqns.into_iter().collect();
        out.sort();
        out.dedup();
        match out.len() {
            1 => out.pop().map(Ty::Ref),
            n if (2..=MAX_NAMED).contains(&n) => Some(Ty::OneOf(out)),
            _ => None,
        }
    }
}

impl Args {
    /// `Items(item)`, without tracking an unknown item.
    pub(crate) fn items(item: Elem) -> Self {
        match item {
            Elem::Unknown => Args::None,
            item => Args::Items(Box::new(item)),
        }
    }

    /// `Map(key, value)`, without tracking a mapping of unknown keys to unknown values.
    pub(crate) fn map(key: Elem, value: Elem) -> Self {
        match (key, value) {
            (Elem::Unknown, Elem::Unknown) => Args::None,
            (k, v) => Args::Map(Box::new(k), Box::new(v)),
        }
    }

    /// `Result(result)`, without tracking an unknown result.
    pub(crate) fn result(result: Elem) -> Self {
        match result {
            Elem::Unknown => Args::None,
            result => Args::Result(Box::new(result)),
        }
    }

    /// The arguments with `f` applied to every tracked type (an item `f` gives none for is
    /// unknown).
    pub(crate) fn map_elems(&self, f: impl Fn(&Ty) -> Option<Ty>) -> Args {
        let elem = |e: &Elem| match e {
            Elem::Of(t) => Elem::of(f(t)),
            Elem::Empty | Elem::Unknown => e.clone(),
        };
        match self {
            Args::None => Args::None,
            Args::Items(e) => Args::Items(Box::new(elem(e))),
            Args::Map(k, v) => Args::Map(Box::new(elem(k)), Box::new(elem(v))),
            Args::Tuple(es) => Args::Tuple(es.iter().map(elem).collect()),
            Args::Result(e) => Args::Result(Box::new(elem(e))),
        }
    }

    /// The items iterating an instance with these arguments yields (none tracked for a future,
    /// which yields itself while it waits).
    pub(crate) fn item(&self) -> Elem {
        match self {
            Args::None | Args::Result(_) => Elem::Unknown,
            Args::Items(e) | Args::Map(e, _) => (**e).clone(),
            Args::Tuple(es) => es.iter().fold(Elem::Empty, |acc, e| acc.join(e)),
        }
    }

    fn join(&self, other: &Args) -> Args {
        match (self, other) {
            (Args::Map(k1, v1), Args::Map(k2, v2)) => {
                Args::Map(Box::new(k1.join(k2)), Box::new(v1.join(v2)))
            }
            (Args::Tuple(a), Args::Tuple(b)) if a.len() == b.len() => {
                Args::Tuple(a.iter().zip(b).map(|(x, y)| x.join(y)).collect())
            }
            (Args::Items(_) | Args::Tuple(_), Args::Items(_) | Args::Tuple(_)) => {
                Args::items(self.item().join(&other.item()))
            }
            (Args::Result(a), Args::Result(b)) => Args::result(a.join(b)),
            _ => Args::None,
        }
    }

    fn depth(&self) -> usize {
        match self {
            Args::None => 0,
            Args::Items(e) | Args::Result(e) => e.depth(),
            Args::Map(k, v) => k.depth().max(v.depth()),
            Args::Tuple(es) => es.iter().map(Elem::depth).max().unwrap_or(0),
        }
    }
}

impl Elem {
    /// A tracked type, or unknown items.
    pub(crate) fn of(ty: Option<Ty>) -> Self {
        ty.map_or(Elem::Unknown, Elem::Of)
    }

    pub(crate) fn join(&self, other: &Elem) -> Elem {
        match (self, other) {
            (Elem::Empty, e) | (e, Elem::Empty) => e.clone(),
            (Elem::Of(a), Elem::Of(b)) => Elem::of(a.join(b)),
            _ => Elem::Unknown,
        }
    }

    fn depth(&self) -> usize {
        match self {
            Elem::Of(t) => t.depth(),
            Elem::Empty | Elem::Unknown => 0,
        }
    }
}

/// The kinds of builtin container whose changes in place heki types item by item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Container {
    /// A list or a deque.
    Sequence,
    Set,
    /// A dict, also an ordered one or one with a default.
    Map,
}

/// A change in place of an object (`heki_ir::FnIndex::changes_in_place`).
pub(crate) enum Change<'i> {
    /// `obj.method(...)`; `starred` when an argument is unpacked (`*xs`, `**kw`).
    Method {
        method: &'i str,
        args: &'i [VarId],
        kwargs: &'i [(Option<String>, VarId)],
        starred: bool,
    },
    /// An in-place operator, with its right operand (`obj += xs`).
    Operator(BinOpKind, VarId),
    /// `obj[index] = value`.
    ItemStore { index: VarId, value: VarId },
}

impl<'i> Change<'i> {
    /// The change `kind` makes, with the variable holding the object it changes.
    pub(crate) fn of(kind: &'i InstKind) -> Option<(VarId, Self)> {
        Some(match kind {
            InstKind::MethodCall {
                recv,
                method,
                args,
                kwargs,
                star_from,
                ..
            } => {
                let starred = star_from.is_some() || kwargs.iter().any(|(k, _)| k.is_none());
                let change = Change::Method {
                    method,
                    args,
                    kwargs,
                    starred,
                };
                (*recv, change)
            }
            InstKind::BinOp {
                op,
                lhs,
                rhs,
                in_place: true,
                ..
            } => (*lhs, Change::Operator(*op, *rhs)),
            InstKind::SubscriptStore {
                obj, index, value, ..
            } => (
                *obj,
                Change::ItemStore {
                    index: *index,
                    value: *value,
                },
            ),
            _ => return None,
        })
    }
}

/// The types of the operands of a [`Change`], as the code typing it knows them.
pub(crate) trait Operands {
    /// The type of the value `v` holds, as an item.
    fn operand_item(&self, v: VarId) -> Elem;

    /// The items iterating the value `v` holds yields.
    fn operand_items(&self, v: VarId) -> Elem {
        match self.operand_item(v) {
            Elem::Of(Ty::Inst(o)) if o.args != Args::None => o.args.item(),
            Elem::Empty => Elem::Empty,
            _ => Elem::Unknown,
        }
    }

    /// The keys and values of the mapping `v` holds.
    fn operand_entries(&self, v: VarId) -> (Elem, Elem) {
        match self.operand_item(v) {
            Elem::Of(Ty::Inst(Obj {
                args: Args::Map(k, v),
                ..
            })) => (*k, *v),
            Elem::Empty => (Elem::Empty, Elem::Empty),
            _ => (Elem::Unknown, Elem::Unknown),
        }
    }
}

impl Container {
    /// The kind of an instance of `class`, when it is one.
    pub(crate) fn of_class(class: &str) -> Option<Self> {
        match class {
            "builtins.list" | "collections.deque" => Some(Self::Sequence),
            "builtins.set" => Some(Self::Set),
            "builtins.dict" | "collections.OrderedDict" | "collections.defaultdict" => {
                Some(Self::Map)
            }
            _ => None,
        }
    }

    /// The kind of a display of `kind` (`[...]`, `{...}`), when it is one.
    pub(crate) fn of_display(kind: BuiltinKind) -> Option<Self> {
        match kind {
            BuiltinKind::List => Some(Self::Sequence),
            BuiltinKind::Set => Some(Self::Set),
            BuiltinKind::Dict => Some(Self::Map),
            _ => None,
        }
    }

    /// What `change` adds to a container of this kind (`of` types its operands): the items of a
    /// sequence or a set (and `Empty`), or the keys and values of a mapping. An unpacked
    /// argument adds what heki cannot type. `None` when the kind has no such method or operator,
    /// or the call passes arguments the method does not take: the change raises, unless a
    /// subclass defines it.
    pub(crate) fn added(self, change: &Change<'_>, of: &impl Operands) -> Option<(Elem, Elem)> {
        use Container::{Map, Sequence, Set};
        let items = |e: Elem| Some((e, Elem::Empty));
        match *change {
            Change::Method {
                method,
                args,
                kwargs,
                starred,
            } => {
                let methods: &[&str] = match self {
                    Sequence => &["append", "appendleft", "extend", "extendleft", "insert"],
                    Set => &["add", "update", "symmetric_difference_update"],
                    Map => &["setdefault", "update"],
                };
                if !methods.contains(&method) {
                    return None;
                }
                if starred {
                    return Some((Elem::Unknown, Elem::Unknown));
                }
                match (self, method, args) {
                    (Map, "update", [] | [_]) => {
                        let (mut keys, mut values) = args
                            .first()
                            .map_or((Elem::Empty, Elem::Empty), |m| of.operand_entries(*m));
                        for (_, v) in kwargs {
                            keys = keys.join(&Elem::Of(Ty::inst("builtins.str")));
                            values = values.join(&of.operand_item(*v));
                        }
                        Some((keys, values))
                    }
                    _ if !kwargs.is_empty() => None,
                    (Sequence, "append" | "appendleft", [v])
                    | (Sequence, "insert", [_, v])
                    | (Set, "add", [v]) => items(of.operand_item(*v)),
                    (Sequence, "extend" | "extendleft", [xs])
                    | (Set, "symmetric_difference_update", [xs]) => items(of.operand_items(*xs)),
                    (Set, "update", xss) => items(
                        xss.iter()
                            .fold(Elem::Empty, |acc, xs| acc.join(&of.operand_items(*xs))),
                    ),
                    (Map, "setdefault", [k, v]) => Some((of.operand_item(*k), of.operand_item(*v))),
                    // The value is `None`, which types nothing.
                    (Map, "setdefault", [k]) => Some((of.operand_item(*k), Elem::Empty)),
                    _ => None,
                }
            }
            Change::Operator(op, rhs) => match (self, op) {
                (Sequence, BinOpKind::Add) | (Set, BinOpKind::BitOr | BinOpKind::BitXor) => {
                    items(of.operand_items(rhs))
                }
                (Sequence, BinOpKind::Mult) | (Set, BinOpKind::BitAnd | BinOpKind::Sub) => {
                    Some((Elem::Empty, Elem::Empty))
                }
                (Map, BinOpKind::BitOr) => Some(of.operand_entries(rhs)),
                _ => None,
            },
            Change::ItemStore { index, value } => match self {
                // A list takes an integer or a slice (a tuple of its bounds, or `slice(...)`),
                // which stores the items of the value.
                Sequence => {
                    let sliced = matches!(
                        of.operand_item(index),
                        Elem::Of(Ty::Inst(o)) if matches!(o.class.as_str(), "builtins.tuple" | "builtins.slice")
                    );
                    items(if sliced {
                        of.operand_items(value)
                    } else {
                        of.operand_item(value)
                    })
                }
                Map => Some((of.operand_item(index), of.operand_item(value))),
                Set => None,
            },
        }
    }
}

/// What changes in place add to an object, for each kind of container it may be
/// ([`Container::added`], `Unknown` where the kind has no such change).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Added {
    pub sequence: Elem,
    pub set: Elem,
    pub keys: Elem,
    pub values: Elem,
}

impl Added {
    /// What changes heki cannot type add.
    pub(crate) fn unknown() -> Self {
        Self {
            sequence: Elem::Unknown,
            set: Elem::Unknown,
            keys: Elem::Unknown,
            values: Elem::Unknown,
        }
    }

    /// What `change` adds, to whichever kind of container its object is.
    pub(crate) fn of(change: &Change<'_>, of: &impl Operands) -> Self {
        let unknown = (Elem::Unknown, Elem::Unknown);
        let added = |kind: Container| kind.added(change, of).unwrap_or(unknown.clone());
        let (keys, values) = added(Container::Map);
        Self {
            sequence: added(Container::Sequence).0,
            set: added(Container::Set).0,
            keys,
            values,
        }
    }
}

/// What no change adds.
impl Default for Added {
    fn default() -> Self {
        Self {
            sequence: Elem::Empty,
            set: Elem::Empty,
            keys: Elem::Empty,
            values: Elem::Empty,
        }
    }
}

impl Join for Added {
    fn join(&mut self, other: &Self) -> bool {
        let joined = Added {
            sequence: self.sequence.join(&other.sequence),
            set: self.set.join(&other.set),
            keys: self.keys.join(&other.keys),
            values: self.values.join(&other.values),
        };
        let grew = joined != *self;
        *self = joined;
        grew
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(item: Elem) -> Ty {
        Ty::container("builtins.list", Args::items(item))
    }

    #[test]
    fn joins_keep_shared_classes_and_merge_container_arguments() {
        let model = Ty::inst("m.Model");
        let field = Ty::inst("m.Field");
        assert_eq!(model.join(&model), Some(model.clone()));
        // Instances of a few classes are a union, sorted by class; of more, nothing.
        let both = model.join(&field).unwrap();
        assert_eq!(
            both.members()
                .iter()
                .map(|o| o.class.as_str())
                .collect::<Vec<_>>(),
            ["m.Field", "m.Model"]
        );
        assert_eq!(field.join(&model), Some(both.clone()));
        assert_eq!(both.join(&model), Some(both.clone()));
        let three = both.join(&Ty::inst("m.Node")).unwrap();
        assert_eq!(three.members().len(), 3);
        assert_eq!(three.join(&Ty::inst("m.Tree")), None);
        assert_eq!(both.obj(), None);
        assert_eq!(
            Ty::Ref("m.f".into()).join(&Ty::Ref("m.f".into())),
            Some(Ty::Ref("m.f".into()))
        );
        assert_eq!(Ty::Ref("m.f".into()).join(&model), None);
        // Coroutines join what awaiting them gives.
        let coroutine = |t: &Ty| Ty::Coroutine(Box::new(t.clone()));
        assert_eq!(
            coroutine(&model).join(&coroutine(&field)),
            Some(coroutine(&both))
        );
        assert_eq!(
            coroutine(&three).join(&coroutine(&Ty::inst("m.Tree"))),
            None
        );
        // A coroutine and another value join by the coroutine's result.
        assert_eq!(coroutine(&model).join(&model), Some(model.clone()));
        assert_eq!(field.join(&coroutine(&model)), Some(both.clone()));
        // An empty display has no items: it takes the other side's items.
        let models = list(Elem::Of(model.clone()));
        assert_eq!(list(Elem::Empty).join(&models), Some(models.clone()));
        assert_eq!(
            models.join(&list(Elem::Of(field.clone()))),
            Some(list(Elem::Of(both)))
        );
        // Mappings join keys and values; tuples join by position, or become item lists.
        let map = |v: &Ty| {
            Ty::container(
                "builtins.dict",
                Args::Map(Box::new(Elem::Unknown), Box::new(Elem::Of(v.clone()))),
            )
        };
        assert_eq!(map(&model).join(&map(&model)), Some(map(&model)));
        let pair = |a: &Ty, b: &Ty| {
            Ty::container(
                "builtins.tuple",
                Args::Tuple(vec![Elem::Of(a.clone()), Elem::Of(b.clone())]),
            )
        };
        assert_eq!(
            pair(&model, &field).join(&pair(&model, &field)),
            Some(pair(&model, &field))
        );
        let single = Ty::container("builtins.tuple", Args::Tuple(vec![Elem::Of(model.clone())]));
        assert_eq!(
            single.join(&pair(&model, &model)),
            Some(Ty::container(
                "builtins.tuple",
                Args::items(Elem::Of(model.clone()))
            ))
        );
        assert_eq!(
            map(&model).join(&Ty::container(
                "builtins.dict",
                Args::items(Elem::Of(model.clone()))
            )),
            Some(Ty::inst("builtins.dict"))
        );
        // Futures join their results; one whose result is not tracked, or not known, has none.
        let future = |result: Elem| Ty::container("asyncio.Future", Args::result(result));
        assert_eq!(
            future(Elem::Of(model.clone())).join(&future(Elem::Of(field.clone()))),
            Some(future(Elem::Of(model.join(&field).unwrap())))
        );
        assert_eq!(
            future(Elem::Empty).join(&future(Elem::Of(model.clone()))),
            Some(future(Elem::Of(model.clone())))
        );
        assert_eq!(future(Elem::Unknown), Ty::inst("asyncio.Future"));
        assert_eq!(
            future(Elem::Of(model.clone())).join(&Ty::inst("asyncio.Future")),
            Some(Ty::inst("asyncio.Future"))
        );
        assert_eq!(
            future(Elem::Of(Ty::SelfType)).bind_self(Some(&model)),
            Some(future(Elem::Of(model)))
        );
    }

    #[test]
    fn named_objects_keep_through_member_maps() {
        let f = Ty::Ref("m.f".into());
        assert_eq!(f.bind_self(None), Some(f.clone()));
        assert_eq!(f.map_members(|_| None), Some(f.clone()));
        assert!(f.members().is_empty());
        assert_eq!(Ty::SelfType.bind_self(None), None);
        assert_eq!(Ty::of_members(Vec::new()), None);
        // A union whose member maps to nothing is nothing.
        let both = Ty::inst("m.A").join(&Ty::inst("m.B")).unwrap();
        assert_eq!(both.map_members(|_| None), None);
        assert_eq!(both.depth(), 1);
        assert_eq!(f.depth(), 1);
        assert_eq!(Ty::Partial(Box::new(f.clone())).depth(), 2);
        assert_eq!(list(Elem::Of(f.clone())).depth(), 2);
        // A value is one of the named objects it names: one, those of a choice, none for an
        // instance. One name is that object; two to eight are a choice; none or more, nothing.
        let names = |n: usize| (0..n).map(|i| format!("m.f{i}"));
        let choice = Ty::one_of(names(2).rev()).unwrap();
        assert_eq!(choice, Ty::OneOf(names(2).collect()));
        assert_eq!(f.named(), ["m.f"]);
        assert_eq!(choice.named(), ["m.f0", "m.f1"]);
        assert!(Ty::inst("m.A").named().is_empty());
        assert_eq!(Ty::one_of(["m.f".to_owned(), "m.f".to_owned()]), Some(f));
        assert_eq!(Ty::one_of(names(0)), None);
        assert_eq!(Ty::one_of(names(MAX_NAMED + 1)), None);
    }

    #[test]
    fn nesting_beyond_the_depth_limit_drops_the_arguments() {
        let mut ty = Ty::inst("m.Model");
        for _ in 0..MAX_DEPTH {
            ty = list(Elem::Of(ty));
        }
        assert!(matches!(&ty, Ty::Inst(o) if o.args != Args::None));
        assert_eq!(list(Elem::Of(ty)), Ty::inst("builtins.list"));
        assert_eq!(
            Ty::container("builtins.tuple", Args::Tuple(Vec::new())).obj(),
            Some(&Obj {
                class: "builtins.tuple".into(),
                args: Args::Tuple(Vec::new())
            })
        );
        assert_eq!(Ty::Super("m.C".into()).obj(), None);
        assert!(generic("typing.Mapping").is_some());
        assert_eq!(
            typing_generic("Iterable").as_deref(),
            Some("typing.Iterable")
        );
        assert_eq!(typing_generic("Model"), None);
    }
}
