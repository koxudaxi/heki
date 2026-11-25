//! `MUT202` return flow (docs/ANALYSIS_AND_KB_SPEC.md section 2.3): which part of a value is
//! shared with the other callers of a read-only producer, how a value derived from it keeps that
//! sharing, and which call results a function may return.
//!
//! The derivations below are the in-function transfer of `MUT202` (`heki_rules` applies them
//! forward, from the producer's call). The extractor applies them backward, from the values a
//! function returns: a function that returns a producer's value, or a part of it, becomes a
//! producer itself. Its summary carries a [`ProtectedReturn`], propagated bottom-up over the call
//! graph, so a wrapper needs no configuration of its own.

use crate::{Callee, ModuleDecls, SharedChain};
use heki_ir::walk::{Budget, FnShape, Visit, reach};
use heki_ir::{DictEntry, Inst, InstKind, IrFunction, PhiNode, Terminator, VarId};
use heki_kb::{CopyDepth, EffectiveEcosystemKb};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

/// Builtin sequences whose slices are new objects holding the same elements (`xs[:]`,
/// `xs[1:]`): a slice of one is a shallow copy. Other objects' slices may be views (`numpy`).
pub const SLICE_COPYING_TYPES: &[&str] = &[
    "builtins.list",
    "builtins.bytearray",
    "builtins.tuple",
    "builtins.str",
    "builtins.bytes",
];

/// The step from `obj` to what the `SubscriptLoad` `inst` reads from it: an item of target
/// unpacking, a shallow copy for a slice of a builtin sequence (`obj_type`, its inferred class),
/// or an element.
pub fn subscript_step(shape: &FnShape<'_>, inst: &Inst, obj_type: Option<&str>) -> ReturnStep {
    match &inst.kind {
        InstKind::SubscriptLoad { index, .. }
            if crate::consumption::is_unpacking(shape, *index, inst.span) =>
        {
            ReturnStep::TARGET_ITEM
        }
        // A slice copies unless the value's class is known to share its storage (a NumPy
        // view): an unannotated value is far more often a list, and a report needs evidence.
        InstKind::SubscriptLoad { is_slice: true, .. }
            if obj_type.is_none_or(|t| SLICE_COPYING_TYPES.contains(&t)) =>
        {
            ReturnStep::SHALLOW_COPY
        }
        _ => ReturnStep::ELEMENT,
    }
}

/// The values a display that only unpacks others copies into the container it makes: the
/// one `**m` entry of a dict display, or the starred elements of a list, tuple, or set display
/// (`{**m}`, `[*xs, *ys]`). The new container is a shallow copy of each
/// ([`ReturnStep::SHALLOW_COPY`]). A dict display with more entries (`{**m, "k": []}`,
/// `{**m, **fresh}`) is not followed: which of its items are the copied ones depends on keys
/// the shared value does not carry.
pub fn display_copies<'i>(f: &IrFunction, kind: &'i InstKind) -> &'i [VarId] {
    match kind {
        InstKind::DictLiteral { entries, .. } => match entries.as_slice() {
            [DictEntry::Unpack { dict }] => std::slice::from_ref(dict),
            _ => &[],
        },
        InstKind::AllocBuiltin { elems, .. } if elems.iter().all(|v| f.is_unpacked(*v)) => elems,
        _ => &[],
    }
}

/// A copy an instruction makes of the values it is given alone ([`copy_of`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyOf<'i> {
    /// The copied values: the receiver, the one argument, or the unpacked values of a display.
    pub operands: &'i [VarId],
    /// It copies what they hold too.
    pub deep: bool,
}

/// The copy the instruction `kind` (a call resolved as `callee`) makes of the values it is
/// given alone, by the one table of copies: a display that only unpacks them
/// ([`display_copies`]: `{**m}`, `[*xs]`), or a call copying its receiver or its one argument
/// ([`call_derivation`]: `x.copy()`, `dict(x)`, `copy.copy(x)`, `copy.deepcopy(x)`). A copy
/// given other arguments (`dict(m, k=v)`, `replace(x, f=v)`, `model_copy(update=...)`) holds
/// values of its own besides the copied ones, so it is no copy here.
pub fn copy_of<'i>(
    f: &IrFunction,
    kind: &'i InstKind,
    callee: &Callee,
    kb: &EffectiveEcosystemKb,
) -> Option<CopyOf<'i>> {
    let (operands, method) = match kind {
        InstKind::DictLiteral { .. } | InstKind::AllocBuiltin { .. } => {
            let operands = display_copies(f, kind);
            return (!operands.is_empty()).then_some(CopyOf {
                operands,
                deep: false,
            });
        }
        InstKind::MethodCall {
            recv,
            method,
            args,
            kwargs,
            ..
        } if args.is_empty() && kwargs.is_empty() => {
            (std::slice::from_ref(recv), Some(method.as_str()))
        }
        // A function of a module (`copy.copy(x)`) is a method call on the module.
        InstKind::Call {
            args,
            kwargs,
            star_from: None,
            ..
        }
        | InstKind::MethodCall {
            args,
            kwargs,
            star_from: None,
            ..
        } if args.len() == 1 && kwargs.is_empty() => (args.as_slice(), None),
        _ => return None,
    };
    match call_derivation(method, callee, kb, false, false)? {
        Derivation::DeepCopy => Some(CopyOf {
            operands,
            deep: true,
        }),
        Derivation::Step(ReturnStep::SHALLOW_COPY) => Some(CopyOf {
            operands,
            deep: false,
        }),
        Derivation::Step(_) => None,
    }
}

/// Methods returning (views of) a container's elements.
pub const ELEMENT_ACCESSORS: &[&str] = &["get", "__getitem__", "values", "items"];
/// Methods returning a copy of their receiver.
pub const COPY_METHODS: &[&str] = &["copy", "model_copy", "__copy__", "__deepcopy__"];

/// Which parts of a value the other callers of a read-only producer share.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Shared {
    /// The object itself: changing its attributes or items is a violation.
    pub object: bool,
    /// The values below it (attributes, items, elements), at any depth.
    pub elements: bool,
}

impl Shared {
    /// The result of a producer: the object, and with `deep` the values below it.
    pub fn produced(deep: bool) -> Self {
        Self {
            object: true,
            elements: deep,
        }
    }

    /// Whether any part is shared.
    pub fn any(self) -> bool {
        self.object || self.elements
    }

    /// A value that may be either: what either shares.
    pub fn join(self, other: Self) -> Self {
        Self {
            object: self.object || other.object,
            elements: self.elements || other.elements,
        }
    }
}

/// How a value derived from another keeps that value's sharing: the derived object is shared
/// when the source object is (`from_object`) or when the values below the source are
/// (`from_elements`); the values below stay shared as they were.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ReturnStep {
    pub from_object: bool,
    pub from_elements: bool,
}

impl ReturnStep {
    /// The value itself: a copy of the reference, an iterator over it.
    pub const ITSELF: Self = Self {
        from_object: true,
        from_elements: false,
    };
    /// A value below it: an attribute, an item, an element of an iteration.
    pub const ELEMENT: Self = Self {
        from_object: false,
        from_elements: true,
    };
    /// An item of target unpacking (`a, b = value`, `consumption::is_unpacking`): shared like
    /// the value when the value is, else like an element.
    pub const TARGET_ITEM: Self = Self {
        from_object: true,
        from_elements: true,
    };
    /// A shallow copy: a new object that still holds the shared values below it.
    pub const SHALLOW_COPY: Self = Self {
        from_object: false,
        from_elements: false,
    };

    /// The sharing of the derived value.
    pub fn apply(self, source: Shared) -> Shared {
        Shared {
            object: (self.from_object && source.object) || (self.from_elements && source.elements),
            elements: source.elements,
        }
    }

    /// `self` applied to the result of `first`.
    pub fn after(self, first: Self) -> Self {
        Self {
            from_object: self.from_object && first.from_object,
            from_elements: (self.from_object && first.from_elements) || self.from_elements,
        }
    }

    /// A value derived by either step (paths joining at a phi).
    pub fn join(self, other: Self) -> Self {
        Self {
            from_object: self.from_object || other.from_object,
            from_elements: self.from_elements || other.from_elements,
        }
    }
}

/// How a call's result derives from the operand it reads for `MUT202`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Derivation {
    /// A deep copy: nothing of the operand is shared.
    DeepCopy,
    /// A shallow copy ([`ReturnStep::SHALLOW_COPY`]) or an element view
    /// ([`ReturnStep::ELEMENT`]).
    Step(ReturnStep),
}

/// The derivation of a call's result from its receiver (`method` set: copy methods, element
/// accessors) or from its first argument (`method` `None`), for the resolved `callee`: copy
/// sanitizers of the catalog copy as deeply as their entry says, and copy methods of a receiver
/// that is no project class copy, deeply for `__deepcopy__` and `model_copy(deep=True)`
/// (`deep_kwarg`). A copy method a project class defines copies as deeply as its own body does,
/// which its summary says (`EffectSummary::returns_protected`), not its name. A `dict` built
/// from a mapping and other arguments (`own_items`: `dict(m, k=v)`) holds their values under
/// their keys, so it is not followed, as a dict display with entries of its own is not
/// ([`display_copies`]).
pub fn call_derivation(
    method: Option<&str>,
    callee: &Callee,
    kb: &EffectiveEcosystemKb,
    deep_kwarg: bool,
    own_items: bool,
) -> Option<Derivation> {
    let symbol = match callee {
        Callee::External(s) => s.as_str(),
        _ => "",
    };
    if method.is_none() && own_items && symbol == "builtins.dict" {
        return None;
    }
    let catalogued = kb.lookup(symbol).and_then(|e| e.copy_sanitizer);
    let copies_by_name =
        method.is_some_and(|m| COPY_METHODS.contains(&m)) && !matches!(callee, Callee::Project(..));
    if catalogued.is_some() || copies_by_name {
        let deep_copy = catalogued == Some(CopyDepth::Deep)
            || method == Some("__deepcopy__")
            || method == Some("model_copy") && deep_kwarg;
        return Some(if deep_copy {
            Derivation::DeepCopy
        } else {
            Derivation::Step(ReturnStep::SHALLOW_COPY)
        });
    }
    method
        .filter(|m| ELEMENT_ACCESSORS.contains(m))
        .map(|_| Derivation::Step(ReturnStep::ELEMENT))
}

/// Whether a copy call asks for a deep copy (`model_copy(deep=True)`, `is_true` deciding a
/// keyword's value). Pydantic assigns the values of `update` to the copy by reference after
/// copying, so a copy with `update` (or with a `**` mapping that may hold it) is a shallow one.
pub fn deep_copy_requested(
    kwargs: &[(Option<String>, VarId)],
    is_true: impl Fn(VarId) -> bool,
) -> bool {
    let deep = kwargs
        .iter()
        .any(|(k, v)| k.as_deref() == Some("deep") && is_true(*v));
    deep && kwargs
        .iter()
        .all(|(k, _)| k.as_deref().is_some_and(|k| k != "update"))
}

/// A builtin container whose methods change it in place: the one a cached function hands out on
/// every live return (`MUT114`, which knows `list`, `dict`, and `set`), or the new object a
/// change is applied to (`MUT119`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerKind {
    List,
    Dict,
    Set,
    Bytearray,
    Deque,
}

impl ContainerKind {
    /// The builtin class.
    pub fn class(self) -> &'static str {
        match self {
            Self::List => "builtins.list",
            Self::Dict => "builtins.dict",
            Self::Set => "builtins.set",
            Self::Bytearray => "builtins.bytearray",
            Self::Deque => "collections.deque",
        }
    }

    /// The kind whose class is `class`, among every kind.
    pub fn of_class(class: &str) -> Option<Self> {
        Self::EVERY.into_iter().find(|k| k.class() == class)
    }

    /// One bit per kind, for sets of kinds.
    pub fn bit(self) -> u8 {
        1 << self as u8
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Dict => "dict",
            Self::Set => "set",
            Self::Bytearray => "bytearray",
            Self::Deque => "deque",
        }
    }

    /// The kinds a receiver of unknown kind is read as ([`container_mutator`]) and a cached
    /// result can be.
    pub const ALL: [Self; 3] = [Self::List, Self::Dict, Self::Set];

    pub const EVERY: [Self; 5] = [
        Self::List,
        Self::Dict,
        Self::Set,
        Self::Bytearray,
        Self::Deque,
    ];

    /// The methods that change a container of this kind in place: the one table of the builtin
    /// containers' changing methods ([`container_mutator`] reads it too).
    pub fn mutators(self) -> &'static [&'static str] {
        match self {
            Self::List => &[
                "append", "extend", "insert", "pop", "remove", "clear", "sort", "reverse",
            ],
            Self::Dict => &["update", "pop", "popitem", "clear", "setdefault"],
            Self::Set => &[
                "add",
                "discard",
                "remove",
                "pop",
                "clear",
                "update",
                "difference_update",
                "intersection_update",
                "symmetric_difference_update",
            ],
            Self::Bytearray => &[
                "append", "extend", "insert", "pop", "remove", "clear", "reverse",
            ],
            Self::Deque => &[
                "append",
                "appendleft",
                "extend",
                "extendleft",
                "insert",
                "pop",
                "popleft",
                "remove",
                "clear",
                "reverse",
                "rotate",
            ],
        }
    }

    /// The methods of the read-only type of a container of this kind (`Sequence` for the
    /// sequences, `Mapping`, `AbstractSet`): a value annotated with that type has only these.
    pub fn read_only_methods(self) -> &'static [&'static str] {
        match self {
            Self::List | Self::Bytearray | Self::Deque => &[
                "index",
                "count",
                "__getitem__",
                "__len__",
                "__contains__",
                "__iter__",
                "__reversed__",
            ],
            Self::Dict => &[
                "get",
                "keys",
                "items",
                "values",
                "__getitem__",
                "__len__",
                "__contains__",
                "__iter__",
                "__eq__",
                "__ne__",
            ],
            Self::Set => &[
                "isdisjoint",
                "__contains__",
                "__len__",
                "__iter__",
                "__le__",
                "__lt__",
                "__ge__",
                "__gt__",
                "__eq__",
                "__ne__",
                "__and__",
                "__or__",
                "__sub__",
                "__xor__",
            ],
        }
    }

    /// Whether the changing method `method` returns a value the call is for (`pop`), not
    /// `None`.
    pub fn returns_value(method: &str) -> bool {
        matches!(method, "pop" | "popitem" | "popleft" | "setdefault")
    }

    /// Whether `method` changes a container of this kind in place.
    pub fn mutator(self, method: &str) -> bool {
        self.mutators().contains(&method)
    }

    /// Whether an item store or delete (`x[k] = v`, `del x[k]`) changes a container of this kind.
    pub fn takes_item_stores(self) -> bool {
        !matches!(self, Self::Set)
    }

    /// Whether the augmented assignment `x <op>= y` changes a container of this kind in place.
    pub fn in_place_op(self, op: heki_ir::BinOpKind) -> bool {
        use heki_ir::BinOpKind::{Add, BitAnd, BitOr, BitXor, Mult, Sub};
        match self {
            Self::List | Self::Bytearray | Self::Deque => matches!(op, Add | Mult),
            Self::Dict => matches!(op, BitOr),
            Self::Set => matches!(op, BitOr | BitAnd | Sub | BitXor),
        }
    }
}

/// Whether `method` changes a builtin list, dict, or set in place when the receiver's kind is
/// not known: a changing method of some kind ([`ContainerKind::mutators`]), or the item store or
/// delete an assignment calls (`__setitem__`, `__delitem__`).
pub fn container_mutator(method: &str) -> bool {
    ContainerKind::ALL.iter().any(|k| k.mutator(method))
        || matches!(method, "__setitem__" | "__delitem__")
}

/// The methods of the builtin containers, besides the changing ones, whose result holds none of
/// their receiver: a count, a test, an item, text, `None`, or a new object. What `keys`,
/// `values`, `items`, `__iter__`, and `__reversed__` return holds the container (a view or an
/// iterator), and so does an in-place operator's result (`__iadd__` returns it).
const DETACHED_RESULT_METHODS: &[&str] = &[
    "__contains__",
    "__copy__",
    "__delitem__",
    "__getitem__",
    "__len__",
    "__setitem__",
    "copy",
    "count",
    "decode",
    "difference",
    "endswith",
    "find",
    "fromkeys",
    "get",
    "hex",
    "index",
    "intersection",
    "isdisjoint",
    "issubset",
    "issuperset",
    "rfind",
    "rindex",
    "startswith",
    "symmetric_difference",
    "union",
];

/// Whether what `method` returns holds none of its receiver, on every builtin container kind that
/// has the method: a changing method ([`ContainerKind::mutators`], which returns `None` or an
/// item) or one of [`DETACHED_RESULT_METHODS`].
pub fn result_detached_from_container(method: &str) -> bool {
    ContainerKind::EVERY.iter().any(|k| k.mutator(method))
        || DETACHED_RESULT_METHODS.contains(&method)
}

/// Whether what `method` returns, on every builtin container kind that has the method, is a view
/// of its receiver or an iterator over it, which holds nothing else.
pub fn returns_container_view(method: &str) -> bool {
    matches!(
        method,
        "__iter__" | "__reversed__" | "items" | "keys" | "values"
    )
}

/// What a class of a container family is to the family's builtin ([`ContainerFamily`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FamilyRole {
    /// The builtin itself, or its `typing` alias (`list`, `typing.List`).
    Builtin,
    /// A standard-library class deriving from the builtin (`collections.OrderedDict` is a
    /// `dict`): under R2 a value of the builtin may be one, or an instance of a project class
    /// deriving from one.
    Derived,
    /// The read-only ABC of the family (`Sequence`, `Mapping`, `collections.abc.Set`, and the
    /// `typing` spellings).
    ReadOnly,
    /// The mutable ABC of the family (`MutableSequence`, `MutableMapping`, `MutableSet`).
    Mutable,
}

/// One class of the container families of `list`, `dict`, and `set`, by the fqn an annotation
/// or a base resolves to: the one table the readers of these classes share.
#[derive(Debug, Clone, Copy)]
pub struct ContainerFamily {
    pub class: &'static str,
    pub kind: ContainerKind,
    pub role: FamilyRole,
}

impl ContainerFamily {
    pub const TABLE: &[Self] = {
        use ContainerKind::{Dict, List, Set};
        use FamilyRole::{Builtin, Derived, Mutable, ReadOnly};
        &[
            ContainerFamily {
                class: "builtins.list",
                kind: List,
                role: Builtin,
            },
            ContainerFamily {
                class: "builtins.dict",
                kind: Dict,
                role: Builtin,
            },
            ContainerFamily {
                class: "builtins.set",
                kind: Set,
                role: Builtin,
            },
            ContainerFamily {
                class: "typing.List",
                kind: List,
                role: Builtin,
            },
            ContainerFamily {
                class: "typing.Dict",
                kind: Dict,
                role: Builtin,
            },
            ContainerFamily {
                class: "typing.Set",
                kind: Set,
                role: Builtin,
            },
            ContainerFamily {
                class: "collections.OrderedDict",
                kind: Dict,
                role: Derived,
            },
            ContainerFamily {
                class: "collections.defaultdict",
                kind: Dict,
                role: Derived,
            },
            ContainerFamily {
                class: "collections.Counter",
                kind: Dict,
                role: Derived,
            },
            ContainerFamily {
                class: "typing.OrderedDict",
                kind: Dict,
                role: Derived,
            },
            ContainerFamily {
                class: "typing.DefaultDict",
                kind: Dict,
                role: Derived,
            },
            ContainerFamily {
                class: "typing.Counter",
                kind: Dict,
                role: Derived,
            },
            ContainerFamily {
                class: "typing.Sequence",
                kind: List,
                role: ReadOnly,
            },
            ContainerFamily {
                class: "typing.Mapping",
                kind: Dict,
                role: ReadOnly,
            },
            ContainerFamily {
                class: "typing.AbstractSet",
                kind: Set,
                role: ReadOnly,
            },
            ContainerFamily {
                class: "collections.abc.Sequence",
                kind: List,
                role: ReadOnly,
            },
            ContainerFamily {
                class: "collections.abc.Mapping",
                kind: Dict,
                role: ReadOnly,
            },
            ContainerFamily {
                class: "collections.abc.Set",
                kind: Set,
                role: ReadOnly,
            },
            ContainerFamily {
                class: "typing.MutableSequence",
                kind: List,
                role: Mutable,
            },
            ContainerFamily {
                class: "typing.MutableMapping",
                kind: Dict,
                role: Mutable,
            },
            ContainerFamily {
                class: "typing.MutableSet",
                kind: Set,
                role: Mutable,
            },
            ContainerFamily {
                class: "collections.abc.MutableSequence",
                kind: List,
                role: Mutable,
            },
            ContainerFamily {
                class: "collections.abc.MutableMapping",
                kind: Dict,
                role: Mutable,
            },
            ContainerFamily {
                class: "collections.abc.MutableSet",
                kind: Set,
                role: Mutable,
            },
        ]
    };

    /// The row of the resolved class `class`.
    pub fn of(class: &str) -> Option<&'static Self> {
        Self::TABLE.iter().find(|r| r.class == class)
    }

    /// The role of `class` in its family, if it has one.
    pub fn role_of(class: &str) -> Option<FamilyRole> {
        Self::of(class).map(|r| r.role)
    }

    /// The classes of `kind`'s family with one of `roles`.
    pub fn classes(
        kind: ContainerKind,
        roles: &'static [FamilyRole],
    ) -> impl Iterator<Item = &'static str> {
        (Self::TABLE.iter())
            .filter(move |r| r.kind == kind && roles.contains(&r.role))
            .map(|r| r.class)
    }

    /// The read methods ([`crate::ReadKinds::methods`]) a standard-library class of the
    /// families binds whose code changes the object: a `defaultdict`'s `__missing__`, which an
    /// item read of a missing key runs, inserts the key.
    const CHANGING_READS: &[(&str, &str)] = &[
        ("collections.defaultdict", "__missing__"),
        ("typing.DefaultDict", "__missing__"),
    ];

    /// The method of `methods` that `class` binds and whose code changes the object
    /// ([`Self::CHANGING_READS`]).
    pub fn changing_read(class: &str, methods: &[&str]) -> Option<&'static str> {
        (Self::CHANGING_READS.iter())
            .find(|(c, m)| *c == class && methods.contains(m))
            .map(|(_, m)| *m)
    }

    /// The builtin whose methods a method called on a value of `class` is read as: the builtin's
    /// own, its alias's, and its ABCs' (a `Mapping`'s `get` is a `dict`'s). A derived class may
    /// add or change methods (`Counter.update` adds counts), so it has none.
    pub fn read_as_builtin(class: &str) -> Option<ContainerKind> {
        Self::of(class)
            .filter(|r| r.role != FamilyRole::Derived)
            .map(|r| r.kind)
    }
}

/// A function whose results a `functools` cache keeps and hands to every later call with the
/// same arguments (`MUT114`'s producer): decorated with `functools.cache`, or with
/// `functools.lru_cache` whose `maxsize` is left out, `None`, or a literal number above 0,
/// resolved through the module's imports, and otherwise only with `staticmethod` / `classmethod`; it takes a parameter besides
/// a method's `self` / `cls`, so the cache keys its results; and every return a run can reach
/// hands out a builtin container of one kind, established by its return annotation (R2) or by
/// construction (a display, a comprehension, `list(...)` / `dict(...)` / `set(...)` /
/// `sorted(...)`, `str.split`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedResult {
    /// `"lru_cache"` or `"cache"`.
    pub decorator: CacheDecorator,
    pub kind: ContainerKind,
    /// Every returned value is built in the function as the builtin itself (a display, a
    /// comprehension, `list(...)`), not only named by the return annotation, which under R2
    /// admits a project subclass.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub built: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheDecorator {
    LruCache,
    Cache,
}

impl CacheDecorator {
    pub fn fqn(self) -> &'static str {
        match self {
            Self::LruCache => "functools.lru_cache",
            Self::Cache => "functools.cache",
        }
    }
}

/// The function's [`CachedResult`], when it is one. `module` describes the function's module,
/// and `type_of` is the class of a value's inferred type.
pub(crate) fn cached_result<'t>(
    f: &IrFunction,
    module: &ModuleDecls,
    defs: &FxHashMap<VarId, &Inst>,
    live: &crate::ownership::Live,
    type_of: impl Fn(VarId) -> Option<&'t str>,
) -> Option<CachedResult> {
    if !matches!(f.kind, heki_ir::FunctionKind::Sync)
        || crate::resolver::is_generator_function(f, module)
    {
        return None;
    }
    let decorator = cache_decorator(&f.decorators, module)?;
    let resolve = |text: &str| module.resolve_dotted(text, false);
    let receiver = f.owner_class_qualname.is_some() && !f.has_decorator(&["staticmethod"]);
    if f.params.len() <= usize::from(receiver) {
        return None;
    }
    let mut returned = Vec::new();
    for b in f.blocks.iter().filter(|b| live.block(b.id)) {
        match b.terminator {
            Terminator::Return(Some(v)) => returned.push(v),
            Terminator::Return(None) => return None,
            _ => {}
        }
    }
    if returned.is_empty() {
        return None;
    }
    let built = constructed_kind(f, defs, live, &returned, &resolve, type_of);
    let annotated = || {
        f.return_annotation
            .as_deref()
            .and_then(|a| annotated_kind(a, &resolve))
    };
    let kind = built.or_else(annotated)?;
    Some(CachedResult {
        decorator,
        kind,
        built: built.is_some(),
    })
}

/// The container kind a return annotation names as a whole (`dict[str, Any]`, `typing.List`),
/// not inside a union or `Optional`.
fn annotated_kind(text: &str, resolve: &impl Fn(&str) -> Option<String>) -> Option<ContainerKind> {
    let text = text.trim().trim_matches(|c| c == '"' || c == '\'');
    // The head names the whole annotation only when no `|` joins it to another at the top
    // level (`list[int] | dict[str, int]`).
    let mut depth = 0i32;
    let union = text.chars().any(|c| {
        depth += i32::from(c == '[') - i32::from(c == ']');
        c == '|' && depth == 0
    });
    if union {
        return None;
    }
    let head = text.split_once('[').map_or(text, |(h, _)| h).trim();
    match resolve(head)?.as_str() {
        "builtins.list" | "typing.List" | "typing_extensions.List" => Some(ContainerKind::List),
        "builtins.dict" | "typing.Dict" | "typing_extensions.Dict" => Some(ContainerKind::Dict),
        "builtins.set" | "typing.Set" | "typing_extensions.Set" => Some(ContainerKind::Set),
        _ => None,
    }
}

/// The one `functools` cache among the decorators of a function of `module`, when every other
/// one keeps the cache's results ([`crate::decorators::Decorator::beside_cache`]) and the cache
/// keeps results (no `maxsize` of the literal `0`).
fn cache_decorator(decorators: &[String], module: &ModuleDecls) -> Option<CacheDecorator> {
    let mut found = None;
    for d in decorators {
        let args = match d.split_once('(') {
            Some((_, rest)) => Some(rest.trim_end().strip_suffix(')')?),
            None => None,
        };
        let decorator = match module.decorator_symbol(d).as_deref() {
            Some("functools.lru_cache") => CacheDecorator::LruCache,
            Some("functools.cache") if args.is_none() => CacheDecorator::Cache,
            _ if crate::decorators::is(d, module, |k| k.beside_cache) => continue,
            _ => return None,
        };
        // The cache keeps results when `maxsize` is left out (128), `None`, or a literal number
        // above 0; any other expression may be 0.
        let maxsize = args
            .into_iter()
            .flat_map(|a| a.split(','))
            .map(str::trim)
            .enumerate();
        let keeps = maxsize
            .filter_map(|(i, part)| {
                part.strip_prefix("maxsize")
                    .map_or((i == 0 && !part.contains('=')).then_some(part), |rest| {
                        rest.trim_start().strip_prefix('=').map(str::trim)
                    })
            })
            .all(|v| v == "None" || v.parse::<u64>().is_ok_and(|n| n > 0));
        if !keeps || found.replace(decorator).is_some() {
            return None;
        }
    }
    found
}

/// The container kind every value in `returned` is built as, through copies and phis (along
/// the edges a run can take).
fn constructed_kind<'t>(
    f: &IrFunction,
    defs: &FxHashMap<VarId, &Inst>,
    live: &crate::ownership::Live,
    returned: &[VarId],
    resolve: &impl Fn(&str) -> Option<String>,
    type_of: impl Fn(VarId) -> Option<&'t str>,
) -> Option<ContainerKind> {
    let phis: FxHashMap<VarId, (heki_ir::BlockId, &PhiNode)> = f
        .blocks
        .iter()
        .flat_map(|b| b.phis.iter().map(move |p| (p.dst, (b.id, p))))
        .collect();
    // `list` / `dict` / `set` / `sorted` the module does not rebind.
    let builtin = |name: &str| resolve(name).is_none_or(|t| t == format!("builtins.{name}"));
    // The container an instruction builds, if any.
    let leaf_kind = |inst: &InstKind| match inst {
        InstKind::AllocBuiltin { kind, .. } => match kind {
            heki_ir::BuiltinKind::List => Some(ContainerKind::List),
            heki_ir::BuiltinKind::Dict => Some(ContainerKind::Dict),
            heki_ir::BuiltinKind::Set => Some(ContainerKind::Set),
            _ => None,
        },
        InstKind::DictLiteral { .. } => Some(ContainerKind::Dict),
        InstKind::Call { callee, .. } if builtin(callee) => match callee.as_str() {
            "list" | "sorted" => Some(ContainerKind::List),
            "dict" => Some(ContainerKind::Dict),
            "set" => Some(ContainerKind::Set),
            _ => None,
        },
        InstKind::MethodCall { recv, method, .. }
            if matches!(method.as_str(), "split" | "rsplit" | "splitlines")
                && matches!(type_of(*recv), Some("builtins.str" | "builtins.bytes")) =>
        {
            Some(ContainerKind::List)
        }
        _ => None,
    };
    // Through copies (their operand) and phis (the inputs along edges a run can take).
    let next = |var: VarId, out: &mut Vec<VarId>| {
        out.extend(defs.get(&var).into_iter().flat_map(|i| i.kind.operands()));
        out.extend(phis.get(&var).into_iter().flat_map(|(block, phi)| {
            phi.incoming
                .iter()
                .filter(move |(from, v)| *v != var && live.edge(*from, *block))
                .map(|(_, v)| *v)
        }));
    };
    let mut kind = None;
    let visit = |var: VarId| {
        let built = match defs.get(&var).map(|i| &i.kind) {
            Some(InstKind::Copy { .. }) => return Visit::Expand,
            None if phis.contains_key(&var) => return Visit::Expand,
            Some(inst) => leaf_kind(inst),
            None => None,
        };
        match built {
            Some(b) if kind.replace(b).is_none_or(|k| k == b) => Visit::Prune,
            _ => Visit::Stop(()),
        }
    };
    let budget = Budget::of_function("MUT114 cached container", f);
    let complete = reach(&budget, returned.iter().copied(), next, visit);
    complete.open_means(Some(())).map_or(kind, |()| None)
}

/// A function that may return (on some path) a value shared with a read-only producer's other
/// callers: a declared `readonly-returns` producer's or an `@lru_cache` function's result, or a
/// part of one, directly or through other such functions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectedReturn {
    /// The declared producer or `@lru_cache` function the value comes from.
    pub producer: String,
    /// What the returned value shares, joined over the paths that return such a value.
    pub shared: Shared,
    /// Calls from this function down to the one that returns the producer's value.
    pub chain: SharedChain,
    /// `producer` is declared in `readonly-returns` (`MUT202`), not an `@lru_cache` function
    /// (`MUT114`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub declared: bool,
}

impl ProtectedReturn {
    /// Join the value another path returns: what either shares, named by the shorter chain.
    pub fn join(&mut self, other: Self) {
        let shared = self.shared.join(other.shared);
        if other.chain.len() < self.chain.len() {
            *self = other;
        }
        self.shared = shared;
    }

    /// Record in `slot` that the function returns, through `step`, a value of `producer` that
    /// shares `produced`, reached through `chain` (nothing when the step shares nothing of it).
    pub fn add(
        slot: &mut Option<Self>,
        producer: String,
        declared: bool,
        produced: Shared,
        step: ReturnStep,
        chain: SharedChain,
    ) {
        let shared = step.apply(produced);
        if !shared.any() {
            return;
        }
        let found = Self {
            producer,
            shared,
            chain,
            declared,
        };
        match slot {
            Some(r) => r.join(found),
            None => *slot = Some(found),
        }
    }
}

/// The values a function's result may derive from without a deep copy: for each such variable,
/// the step from it to the returned value, joined over the paths that return it. The walk goes
/// back from every `return v` a run can reach (`live`), through copies, phis (along the edges a
/// run can take), attribute and item loads, iterations, and the derivations `derive` reports for
/// an instruction (a call's receiver and first argument); it does not pass a deep copy, and a
/// generator's or coroutine's caller gets a generator or a coroutine, never the returned value,
/// so neither is walked.
pub(crate) fn returned_values<'t>(
    shape: &FnShape<'_>,
    defs: &FxHashMap<VarId, &Inst>,
    live: &crate::ownership::Live,
    derive: impl Fn(&InstKind) -> Vec<(VarId, Derivation)>,
    type_of: impl Fn(VarId) -> Option<&'t str>,
) -> FxHashMap<VarId, ReturnStep> {
    let f = shape.f;
    let mut steps: FxHashMap<VarId, ReturnStep> = FxHashMap::default();
    if !matches!(f.kind, heki_ir::FunctionKind::Sync) {
        return steps;
    }
    let mut work: Vec<(VarId, ReturnStep)> = f
        .blocks
        .iter()
        .filter(|b| live.block(b.id))
        .filter_map(|b| match b.terminator {
            Terminator::Return(Some(v)) => Some((v, ReturnStep::ITSELF)),
            _ => None,
        })
        .collect();
    let mut phis: Option<FxHashMap<VarId, (heki_ir::BlockId, &PhiNode)>> = None;
    while let Some((var, step)) = work.pop() {
        let step = match steps.get(&var) {
            Some(old) if old.join(step) == *old => continue,
            Some(old) => old.join(step),
            None => step,
        };
        steps.insert(var, step);
        let Some(inst) = defs.get(&var) else {
            let phis = phis.get_or_insert_with(|| {
                f.blocks
                    .iter()
                    .flat_map(|b| b.phis.iter().map(move |p| (p.dst, (b.id, p))))
                    .collect()
            });
            if let Some((block, phi)) = phis.get(&var) {
                work.extend(
                    phi.incoming
                        .iter()
                        .filter(|(from, v)| *v != var && live.edge(*from, *block))
                        .map(|(_, v)| (*v, step)),
                );
            }
            continue;
        };
        match &inst.kind {
            InstKind::Copy { src, .. } => work.push((*src, step)),
            InstKind::MakeIter { iterable, .. } => work.push((*iterable, step)),
            InstKind::IterNext { iter, .. } => work.push((*iter, step.after(ReturnStep::ELEMENT))),
            InstKind::AttrLoad { obj, .. } => work.push((*obj, step.after(ReturnStep::ELEMENT))),
            InstKind::SubscriptLoad { obj, .. } => {
                let derived = subscript_step(shape, inst, type_of(*obj));
                work.push((*obj, step.after(derived)));
            }
            InstKind::DictLiteral { .. } | InstKind::AllocBuiltin { .. } => {
                for copied in display_copies(f, &inst.kind) {
                    work.push((*copied, step.after(ReturnStep::SHALLOW_COPY)));
                }
            }
            // A call's result derives from its receiver or first argument when it copies or
            // views it; `derive` reports nothing for any other instruction.
            kind => {
                for (operand, derivation) in derive(kind) {
                    if let Derivation::Step(s) = derivation {
                        work.push((operand, step.after(s)));
                    }
                }
            }
        }
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_STEPS: [ReturnStep; 4] = [
        ReturnStep::ITSELF,
        ReturnStep::ELEMENT,
        ReturnStep::TARGET_ITEM,
        ReturnStep::SHALLOW_COPY,
    ];

    fn shared(object: bool, elements: bool) -> Shared {
        Shared { object, elements }
    }

    /// The states a forward `MUT202` value can be in: a shallow producer's result, a deep one's,
    /// and a shallow copy of a deep one (a shared object never has unshared parts below a
    /// shallow copy).
    const STATES: [(bool, bool); 3] = [(true, false), (true, true), (false, true)];

    #[test]
    fn steps_match_the_forward_transfer() {
        for (object, elements) in STATES {
            let s = shared(object, elements);
            // An attribute or item is shared when the values below are.
            assert_eq!(ReturnStep::ELEMENT.apply(s), shared(elements, elements));
            // Unpacking keeps a shared object, else reads an element.
            let unpacked = if object { s } else { shared(true, true) };
            assert_eq!(ReturnStep::TARGET_ITEM.apply(s), unpacked);
            // A shallow copy is new; what it holds stays shared.
            assert_eq!(ReturnStep::SHALLOW_COPY.apply(s), shared(false, elements));
            assert_eq!(ReturnStep::ITSELF.apply(s), s);
        }
        assert!(
            !ReturnStep::SHALLOW_COPY
                .apply(Shared::produced(false))
                .any()
        );
    }

    #[test]
    fn composition_and_join_agree_with_applying_in_turn() {
        for first in ALL_STEPS {
            for second in ALL_STEPS {
                for (object, elements) in STATES {
                    let s = shared(object, elements);
                    assert_eq!(
                        second.after(first).apply(s),
                        second.apply(first.apply(s)),
                        "{first:?} then {second:?} on {s:?}"
                    );
                    assert_eq!(
                        first.join(second).apply(s),
                        first.apply(s).join(second.apply(s)),
                        "{first:?} or {second:?} on {s:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_join_keeps_the_shorter_chain_and_both_sharings() {
        let hop = |callee: &str| {
            std::sync::Arc::new(crate::CallHop {
                caller: crate::FuncKey {
                    module: "m".into(),
                    qualname: "f".into(),
                },
                callee: callee.into(),
                span: heki_ast::Span { start: 0, end: 1 },
                path: "m.py".into(),
            })
        };
        let mut long = ProtectedReturn {
            producer: "m.deep".into(),
            shared: shared(false, true),
            chain: vec![hop("m.a"), hop("m.deep")],
            declared: true,
        };
        long.join(ProtectedReturn {
            producer: "m.flat".into(),
            shared: shared(true, false),
            chain: vec![hop("m.flat")],
            declared: false,
        });
        assert_eq!(long.producer, "m.flat");
        assert!(!long.declared);
        assert_eq!(long.shared, shared(true, true));
        assert_eq!(long.chain.len(), 1);
    }
}
