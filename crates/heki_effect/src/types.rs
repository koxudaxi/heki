//! Value types of the Advisory lane's receiver typing: classes with the arguments of known
//! generic containers (`list[Model]`, `dict[str, Model]`, `tuple[Model, int]`,
//! `Iterator[Model]`), so that the items a value yields when it is iterated, subscripted, or
//! unpacked have a class too. Types are hints for finding call targets and the classes of
//! accessed attributes; they never reach the Soundness lane (ADR-004).

use serde::{Deserialize, Serialize};

/// Deepest nesting of tracked type arguments (`dict[str, list[Model]]` nests 2 deep); deeper
/// arguments are dropped, so loops that build nested containers stay finite.
pub(crate) const MAX_DEPTH: usize = 4;

/// Most classes a union keeps (`Model | Field | Node`); a value of more classes has no tracked
/// type.
pub(crate) const MAX_UNION: usize = 3;

/// What the Advisory lane knows about a value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Ty {
    /// A named object: a module, class, function, or external symbol (by fqn).
    Ref(String),
    /// `super()` inside a method of the class `fqn`.
    Super(String),
    /// An instance of one class.
    Inst(Obj),
    /// `typing.Self` in a declaration: the type of the receiver.
    SelfType,
    /// A `functools.partial(f, ...)` object: calling it calls `f`.
    Partial(Box<Ty>),
    /// An instance of one of two or three classes (sorted by class, one entry per class).
    Union(Vec<Obj>),
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
            Ty::Ref(_) | Ty::Super(_) | Ty::SelfType | Ty::Partial(_) | Ty::Union(_) => None,
        }
    }

    /// The instances a value may be: one for an instance, two or three for a union.
    pub(crate) fn members(&self) -> &[Obj] {
        match self {
            Ty::Inst(o) => std::slice::from_ref(o),
            Ty::Union(os) => os,
            Ty::Ref(_) | Ty::Super(_) | Ty::SelfType | Ty::Partial(_) => &[],
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
            0 => None,
            1 => out.pop().map(Ty::Inst),
            n if n <= MAX_UNION => Some(Ty::Union(out)),
            _ => None,
        }
    }

    /// Maps every instance the value may be (a union member by member).
    pub(crate) fn map_members(&self, f: impl Fn(&Obj) -> Option<Obj>) -> Option<Ty> {
        match self {
            Ty::Inst(o) => f(o).map(Ty::Inst),
            Ty::Union(os) => Ty::of_members(os.iter().map(f).collect::<Option<Vec<_>>>()?),
            Ty::Ref(_) | Ty::Super(_) | Ty::SelfType | Ty::Partial(_) => Some(self.clone()),
        }
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
            Ty::Partial(t) => 1 + t.depth(),
            Ty::Ref(_) | Ty::Super(_) | Ty::SelfType => 1,
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
        }
    }

    /// The items iterating an instance with these arguments yields.
    pub(crate) fn item(&self) -> Elem {
        match self {
            Args::None => Elem::Unknown,
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
            _ => Args::None,
        }
    }

    fn depth(&self) -> usize {
        match self {
            Args::None => 0,
            Args::Items(e) => e.depth(),
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
                Args::items(Elem::Of(model))
            )),
            Some(Ty::inst("builtins.dict"))
        );
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
