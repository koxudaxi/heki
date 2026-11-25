//! The parameters a function only reads ([`EffectSummary::only_read_params`]), and what its own
//! body does with the parameters annotated with a mutable container ([`ContainerParams`]): the
//! facts a read-only annotation (`Sequence`, `Mapping`, `AbstractSet`) of such a parameter needs.
//!
//! One judgment has two readers: whether a function changes the object itself comes from
//! [`EffectSummary::mutated_param_paths`] alone, the fact `MUT110` reports, so a parameter only
//! read here is never one `MUT110` would report changed. The rest (stores, captures, returns,
//! the calls it is handed to) is this module's: the extractor finds it in the function's own
//! body, and propagation joins what the project callees do with it. So are the kinds of read
//! that reach the object ([`ReadKinds`]), which say what a project subclass of the builtin must
//! not bind for the reads to be the builtin's own (R2, [`read_override`]).

use crate::callgraph::bound_params;
use crate::readonly::ContainerKind;
use crate::resolver::{TypeCtx, func_qualname, is_unknown_base, param_annotation};
use crate::{BoundCallee, EffectSummary, ProjectDecls, decorators};
use heki_ast::Span;
use heki_ir::IrFunction;
use heki_kb::{AnnotationsRead, EffectiveEcosystemKb};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The parameters (bit `i`: parameter `i < 64`) annotated with a builtin list, dict, or set, or
/// with a mutable abstract container (`resolver::mutable_container`), and what the function's
/// own body does with each: the local facts a caller's file reads without the function's IR.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerParams {
    /// The annotated parameters.
    pub annotated: u64,
    /// Those whose every use in the body the read-only type allows: its own methods, item reads,
    /// iteration, membership tests, `len`, comparisons, and handing it to a call; and whose
    /// derived values (a slice of a list, a set operation of a set, a join with another value
    /// such as `p or []`) are only iterated, unpacked, tested, measured, or handed to a library
    /// call Layer 1 lists as reading them. A set is iterated nowhere but by a builtin `EFF112`
    /// counts as taking no order from it (`order_flow::ORDER_FREE_BUILTINS`: `len`, `sorted`,
    /// `frozenset`, ...), so the annotation loses no fact of `EFF112`'s.
    pub abstract_use: u64,
    /// Those whose name is bound once: no assignment or `del` of it, except
    /// `if p is None: p = <a new container of its kind>`.
    pub bound_once: u64,
}

impl EffectSummary {
    /// The positions whose object the function only reads, itself and through every function
    /// it hands it to: [`Self::only_read_params`] where [`Self::mutated_param_paths`] has no
    /// change of the object itself.
    pub fn only_read_mask(&self) -> u64 {
        let changed = self
            .mutated_param_paths
            .iter()
            .filter(|(j, paths)| **j < 64 && paths.contains(""))
            .fold(0, |acc, (j, _)| acc | 1u64 << j);
        self.only_read_params & !changed
    }

    /// The parameters annotated with a mutable container that the function only reads
    /// ([`Self::only_read_mask`] of [`ContainerParams::annotated`]).
    pub fn only_read_containers(&self) -> u64 {
        self.container_params
            .as_deref()
            .map_or(0, |c| c.annotated & self.only_read_mask())
    }
}

/// The caller's positions a call hands to `callee` where it does not only read them: those
/// bound to its positions outside [`EffectSummary::only_read_mask`], every bound one when the
/// callee has no summary.
pub(crate) fn not_only_read_through(callee: Option<&EffectSummary>, bc: &BoundCallee) -> u64 {
    let open = callee.map_or(u64::MAX, |c| !c.only_read_mask());
    bound_params(open, bc)
}

/// Add to `s` the kinds of read `callee` makes of the objects the call `bc` hands it: those of
/// each of its positions go to every position of the caller the object there may be.
pub(crate) fn read_kinds_through(s: &mut EffectSummary, callee: &EffectSummary, bc: &BoundCallee) {
    for (&j, &kinds) in callee.read_kinds.iter().flat_map(|k| k.iter()) {
        let caller = bound_params(1u64 << j, bc);
        for k in (0..64u8).filter(|k| caller & (1u64 << k) != 0) {
            s.add_read_kinds(k, kinds);
        }
    }
}

impl EffectSummary {
    /// The kinds of read that reach the object of position `k` ([`Self::read_kinds`]).
    pub fn read_kinds_of(&self, k: u8) -> ReadKinds {
        (self.read_kinds.as_deref())
            .and_then(|m| m.get(&k).copied())
            .unwrap_or_default()
    }

    /// Add `kinds` to those of position `k`.
    pub(crate) fn add_read_kinds(&mut self, k: u8, kinds: ReadKinds) {
        if kinds != ReadKinds::NONE {
            let all = self.read_kinds.get_or_insert_with(Box::default);
            *all.entry(k).or_default() |= kinds;
        }
    }

    /// Add every kind of read of `other` to this summary's, position by position.
    pub(crate) fn join_read_kinds(&mut self, other: &Self) {
        for (&k, &kinds) in other.read_kinds.iter().flat_map(|m| m.iter()) {
            self.add_read_kinds(k, kinds);
        }
    }
}

/// The kinds of read that reach a parameter's object ([`EffectSummary::read_kinds`]), by the
/// methods each may run on the object's class: what a class the object may be under R2 has to
/// bind for a read to run code of its own. The implicit reads first, then the methods called by
/// name ([`ReadKinds::NAMED`]), one bit each. It holds no class: a read of a `Mapping` is the
/// read of a `dict`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReadKinds(u32);

impl ReadKinds {
    pub const NONE: Self = Self(0);
    pub const ITEM: Self = Self(1);
    pub const ITERATION: Self = Self(1 << 1);
    pub const SIZE: Self = Self(1 << 2);
    pub const MEMBERSHIP: Self = Self(1 << 3);
    pub const COMPARISON: Self = Self(1 << 4);
    pub const TEXT: Self = Self(1 << 5);
    pub const OPERATORS: Self = Self(1 << 6);
    /// A method looked up by name, which runs `__getattribute__` first.
    pub const BY_NAME: Self = Self(1 << 7);
    /// An attribute a library call names, which may be one the class lacks.
    pub const ANY_ATTRIBUTE: Self = Self(1 << 8);
    /// Every read: a library call whose `[[invokes]]` entry does not say what it runs.
    pub const ALL: Self = Self(u32::MAX);
    /// An operand of an operator whose other operand is a builtin: besides the operand's own
    /// operator methods, what the builtin's runs of it (`xs += v` iterates `v`, `d |= v` reads
    /// its `keys()` and items unless `v` iterates as a `dict` does).
    pub const OPERAND: Self =
        Self(Self::OPERATORS.0 | Self::ITERATION.0 | Self::ITEM.0 | Self::BY_NAME.0 | Self::KEYS);
    /// `{**v}`, `f(**v)`: the mapping's `keys()` and items, or its iteration as a `dict`.
    pub const MAPPING_UNPACK: Self =
        Self(Self::ITERATION.0 | Self::ITEM.0 | Self::BY_NAME.0 | Self::KEYS);

    /// The implicit reads, by bit, with what they are and the special methods each may run.
    const IMPLICIT: [(Self, &str, &[&str]); 9] = [
        (Self::ITEM, "item access", &["__getitem__", "__missing__"]),
        (
            Self::ITERATION,
            "iteration",
            &["__iter__", "__reversed__", "__next__"],
        ),
        (
            Self::SIZE,
            "size and truth",
            &["__len__", "__bool__", "__length_hint__"],
        ),
        (Self::MEMBERSHIP, "membership", &["__contains__"]),
        (
            Self::COMPARISON,
            "comparison",
            &[
                "__eq__", "__ne__", "__lt__", "__le__", "__gt__", "__ge__", "__hash__",
            ],
        ),
        (
            Self::TEXT,
            "conversion to text",
            &["__repr__", "__str__", "__format__"],
        ),
        (Self::OPERATORS, "operators", OPERATOR_METHODS),
        (
            Self::BY_NAME,
            "a method by name",
            &["__getattribute__", "__class__"],
        ),
        (Self::ANY_ATTRIBUTE, "any attribute", &["__getattr__"]),
    ];

    /// The methods of `list`, `dict`, and `set` that only read or give a view, called by name:
    /// bit `9 + i` each, `keys` first ([`Self::KEYS`]).
    pub const NAMED: [&str; 16] = [
        "keys",
        "get",
        "items",
        "values",
        "index",
        "count",
        "copy",
        "__copy__",
        "isdisjoint",
        "issubset",
        "issuperset",
        "union",
        "intersection",
        "difference",
        "symmetric_difference",
        "fromkeys",
    ];

    const fn named_bit(i: usize) -> u32 {
        1 << (9 + i)
    }

    const KEYS: u32 = Self::named_bit(0);

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The kinds of read a call of `method` on the object makes, looked up by name (`m.get(k)`,
    /// `m.__getitem__(k)`) or as a special method of its class: [`Self::ALL`] for a method no
    /// kind names.
    pub fn of_method(method: &str, by_name: bool) -> Self {
        let named = Self::NAMED.iter().position(|m| *m == method);
        let implicit = Self::IMPLICIT
            .iter()
            .find(|(_, _, ms)| ms.contains(&method));
        let kinds = match (named, implicit) {
            (Some(i), _) => Self(Self::named_bit(i)),
            (None, Some((kind, ..))) => *kind,
            (None, None) => return Self::ALL,
        };
        if by_name {
            kinds | Self::BY_NAME
        } else {
            kinds
        }
    }

    /// The kinds of read of an argument the library call `symbol` only reads: what its
    /// `[[invokes]]` entry names for the argument at `position` (`None`: the receiver), every
    /// kind without an entry.
    pub(crate) fn of_library_call(
        kb: &EffectiveEcosystemKb,
        symbol: &str,
        position: Option<u8>,
    ) -> Self {
        let Some(entry) = kb.invoked(symbol) else {
            return Self::ALL;
        };
        let named = (entry.values.iter()).filter(|v| match position {
            Some(j) => v.names(Some(j), None),
            None => v.receiver,
        });
        // `"*"` (an attribute the call is handed the name of) and calling the value are no kind:
        // every one.
        let mut kinds = Self::NONE;
        for value in named {
            let called = value.calls.then_some("__call__");
            for m in value.methods.iter().map(String::as_str).chain(called) {
                kinds |= Self::of_method(m, false);
            }
        }
        kinds
    }

    /// The methods these reads may run on the object's class: `__getattribute__` among them
    /// when one is looked up by name ([`Self::BY_NAME`]).
    pub fn methods(self) -> impl Iterator<Item = &'static str> {
        let implicit = (Self::IMPLICIT.iter())
            .filter(move |(kind, ..)| self.contains(*kind))
            .flat_map(|(_, _, ms)| ms.iter().copied());
        let named = (Self::NAMED.iter().enumerate())
            .filter(move |(i, _)| self.0 & Self::named_bit(*i) != 0)
            .map(|(_, m)| *m);
        implicit.chain(named)
    }

    /// What the reads are, for `heki inspect`: the implicit kinds, then the methods called by
    /// name.
    pub fn names(self) -> Vec<&'static str> {
        let implicit = (Self::IMPLICIT.iter())
            .filter(|(kind, ..)| self.contains(*kind))
            .map(|(_, name, _)| *name);
        let named = (Self::NAMED.iter().enumerate())
            .filter(|(i, _)| self.0 & Self::named_bit(*i) != 0)
            .map(|(_, m)| *m);
        implicit.chain(named).collect()
    }
}

impl std::ops::BitOr for ReadKinds {
    type Output = Self;
    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl std::ops::BitOrAssign for ReadKinds {
    fn bitor_assign(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

/// The operator methods an operator runs on an operand's class, the reflected ones included,
/// `__index__`, which an index or a slice bound runs, and the numeric conversions.
const OPERATOR_METHODS: &[&str] = &[
    "__add__",
    "__radd__",
    "__sub__",
    "__rsub__",
    "__mul__",
    "__rmul__",
    "__matmul__",
    "__rmatmul__",
    "__truediv__",
    "__rtruediv__",
    "__floordiv__",
    "__rfloordiv__",
    "__mod__",
    "__rmod__",
    "__divmod__",
    "__rdivmod__",
    "__pow__",
    "__rpow__",
    "__lshift__",
    "__rlshift__",
    "__rshift__",
    "__rrshift__",
    "__and__",
    "__rand__",
    "__or__",
    "__ror__",
    "__xor__",
    "__rxor__",
    "__neg__",
    "__pos__",
    "__invert__",
    "__abs__",
    "__index__",
    "__int__",
    "__float__",
    "__round__",
    "__trunc__",
];

/// A project class whose code a read of a builtin container's object may run (R2): a class
/// the object may be binds the method the read runs, or a metaclass heki does not know creates
/// it, which may install one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOverride {
    Binds { class: String, method: &'static str },
    Metaclass { class: String },
}

/// The first project class, in [`ProjectDecls::implementers`]' order, whose code the reads
/// `kinds` of an object of the builtin `kind` may run: one binding a method the reads run
/// ([`ReadKinds::methods`]), or created by a metaclass heki does not know. `None` when every
/// read is the builtin's own.
pub fn read_override(
    decls: &ProjectDecls,
    kind: ContainerKind,
    kinds: ReadKinds,
) -> Option<ReadOverride> {
    if kinds == ReadKinds::NONE {
        return None;
    }
    let methods: Vec<&'static str> = kinds.methods().collect();
    decls
        .implementers(kind.class())
        .into_iter()
        .find_map(|class| {
            let d = decls.class_decl(&class)?;
            match methods.iter().find(|m| d.members.contains(**m)) {
                Some(method) => Some(ReadOverride::Binds { class, method }),
                None => d
                    .custom_metaclass
                    .then_some(ReadOverride::Metaclass { class }),
            }
        })
}

/// What a read-only annotation of a function's parameters needs besides its summary: what each
/// parameter's annotation says, what may read the annotations at run time, and the shape of
/// its definitions. A side table of [`crate::FileSummary::annotation_facts`], computed while
/// [`crate::OptInFacts::only_read`] is on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnotationFacts {
    /// By position, the named parameters (the receiver included; not `*args` or `**kwargs`).
    pub params: Vec<ParamAnnotation>,
    pub reads: AnnotationReads,
    /// The body states nothing of its own: a docstring, `pass`, `...`, or a lone
    /// `raise NotImplementedError` (an interface a subclass fills in).
    pub stub_body: bool,
    /// A definition of the name is decorated `@overload`.
    pub overloaded: bool,
    /// How many `def` statements define the name (a property's setter, a conditional
    /// definition).
    pub definitions: u8,
}

/// What a parameter annotation says to a read-only annotation of the parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParamAnnotation {
    /// None, `Any`, or `object`: every argument.
    Any,
    /// An abstract type every read-only container of the kinds in the set
    /// ([`ContainerKind::bit`]) is an instance of: `Iterable`, `Collection`, `Container`,
    /// `Sized` for each; `Sequence` and `Reversible` for a list, `Mapping` for a dict,
    /// `AbstractSet` for a set.
    Admits(u8),
    /// A literal container ([`LiteralAnnotation`]).
    Literal(Box<LiteralAnnotation>),
    /// Any other annotation: a `Mutable*` ABC, one written through `Annotated[...]`, an alias, a
    /// class, a union of several types, a name heki does not resolve.
    Other,
}

/// A parameter annotated with a builtin `list`, `dict`, or `set`, or its `typing` form, after
/// `Optional`, `| None`, and quotes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiteralAnnotation {
    pub kind: ContainerKind,
    /// The annotation as written.
    pub text: Box<str>,
    /// Where it is in the file.
    pub span: Span,
    /// Where its head is in the file (`List` of `Optional[List[int]]`, `typing.List` of
    /// `typing.List[int]`): the only text the read-only type replaces.
    pub head: Span,
    /// The type arguments are `str` alone: a `Sequence[str]` also admits a plain `str`.
    pub str_items: bool,
}

/// Whether something may read the annotations of a function at run time: its decorators and,
/// for a method, its class's decorators, metaclass, and bases outside the project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnnotationReads {
    /// Nothing heki knows of: every decorator is in the decorator table, which leaves
    /// annotations unread; the class has no metaclass but `type` or `abc.ABCMeta`, and no
    /// base outside the project but one of the standard library or one the catalog lists as
    /// reading only fields.
    Unread,
    /// The catalog lists the decorator or base named as reading them.
    Read(Box<str>),
    /// A decorator, metaclass, or base heki does not know, named, may read them.
    Unknown(Box<str>),
}

/// [`AnnotationFacts`] of every function of the module `cx` describes that has a parameter,
/// by qualified name.
pub(crate) fn annotation_facts(cx: &TypeCtx<'_>) -> BTreeMap<String, AnnotationFacts> {
    let ir = cx.ir();
    let resolve = |name: &str| cx.module.resolve_dotted(name, true);
    let mut out: BTreeMap<String, AnnotationFacts> = BTreeMap::new();
    for f in &ir.functions {
        if f.is_module_body || f.is_class_body || f.name.starts_with('<') || f.params.is_empty() {
            continue;
        }
        let overload = f.has_decorator(&["overload"]);
        let params = f
            .params
            .iter()
            .filter(|p| !p.is_vararg && !p.is_kwarg)
            .map(|p| param_annotation(p.annotation.as_deref(), p.annotation_span, &resolve))
            .collect();
        let facts = AnnotationFacts {
            params,
            reads: annotation_reads(cx, f),
            stub_body: f.stub_body,
            overloaded: overload,
            definitions: u8::from(!overload),
        };
        match out.get_mut(func_qualname(f)) {
            // The implementation after its overloads, a property's setter, a conditional
            // definition: the facts of the last definition, with what any definition reads.
            Some(seen) => {
                let reads = std::mem::replace(&mut seen.reads, AnnotationReads::Unread);
                *seen = AnnotationFacts {
                    reads: reads.or(facts.reads),
                    overloaded: seen.overloaded || facts.overloaded,
                    definitions: seen.definitions.saturating_add(facts.definitions),
                    stub_body: seen.stub_body && facts.stub_body,
                    ..facts
                };
            }
            None => {
                out.insert(func_qualname(f).to_owned(), facts);
            }
        }
    }
    out
}

impl AnnotationReads {
    /// What the catalog says `by` reads: a reader of the functions' annotations reads them, one
    /// of the fields only leaves them unread.
    fn of_catalog(read: AnnotationsRead, by: &str) -> Self {
        match read {
            AnnotationsRead::Functions => Self::Read(by.into()),
            AnnotationsRead::Fields => Self::Unread,
        }
    }

    /// What reads the annotations when both `self` and `other` may: a known reader first.
    fn or(self, other: Self) -> Self {
        match (self, other) {
            (read @ Self::Read(_), _) | (_, read @ Self::Read(_)) => read,
            (unknown @ Self::Unknown(_), _) | (_, unknown @ Self::Unknown(_)) => unknown,
            _ => Self::Unread,
        }
    }
}

/// What may read the annotations of `f` at run time ([`AnnotationReads`]).
fn annotation_reads(cx: &TypeCtx<'_>, f: &IrFunction) -> AnnotationReads {
    let mut reads = AnnotationReads::Unread;
    for d in &f.decorators {
        reads = reads.or(decorator_reads(cx, d));
    }
    let Some(owner) = &f.owner_class_qualname else {
        return reads;
    };
    let class_decorators = (cx.ir().functions.iter())
        .filter(|g| g.is_class_body && &g.qualname == owner)
        .flat_map(|g| &g.decorators);
    for d in class_decorators {
        reads = reads.or(decorator_reads(cx, d));
    }
    let class = format!("{}.{owner}", cx.module.module_name);
    for c in cx.project.mro_of(&class) {
        let Some(decl) = cx.project.class_decl(&c) else {
            reads = reads.or(base_reads(cx, &c));
            continue;
        };
        if let Some(meta) = decl.metaclass.as_deref()
            && !matches!(meta, "builtins.type" | "abc.ABCMeta")
        {
            reads = reads.or(AnnotationReads::Unknown(meta.into()));
        }
    }
    reads
}

/// What the decorator `text` (its source) reads of what it decorates.
fn decorator_reads(cx: &TypeCtx<'_>, text: &str) -> AnnotationReads {
    if decorators::is(text, cx.module, |d| d.leaves_annotations_unread) {
        return AnnotationReads::Unread;
    }
    let symbol = cx.module.decorator_symbol(text);
    let read = (symbol.as_deref()).and_then(|s| cx.kb.decorator_reads_annotations(s));
    let by = symbol.unwrap_or_else(|| text.to_owned());
    read.map_or(AnnotationReads::Unknown(by.as_str().into()), |r| {
        AnnotationReads::of_catalog(r, &by)
    })
}

/// What the base `base` outside the project reads of the methods of a class deriving from it:
/// a class of the standard library none, unless the catalog lists it.
fn base_reads(cx: &TypeCtx<'_>, base: &str) -> AnnotationReads {
    match cx.kb.base_reads_annotations(base) {
        Some(read) => AnnotationReads::of_catalog(read, base),
        None if is_unknown_base(base) => AnnotationReads::Unknown(base.into()),
        None => {
            let module = base.rsplit_once('.').map_or(base, |(m, _)| m);
            if module == "builtins" || heki_env::is_stdlib_module(module) {
                AnnotationReads::Unread
            } else {
                AnnotationReads::Unknown(base.into())
            }
        }
    }
}
