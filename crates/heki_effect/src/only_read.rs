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
use crate::decorators::{self, ReadsAnnotations};
use crate::readonly::{ContainerFamily, ContainerKind, FamilyLevel};
use crate::resolver::{FamilyOpen, TypeCtx, func_qualname, is_unknown_base, param_annotation};
use crate::{BoundCallee, EffectSummary, ProjectDecls};
use heki_ast::Span;
use heki_ir::{ConstLiteral, InstKind, IrFunction, PatternShape};
use heki_kb::{AnnotationsRead, EffectiveEcosystemKb};
use heki_semantic::{ScopeId, SemanticModel};
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
    /// A read heki has no list of methods for: it may run any method or attribute the class
    /// binds, so the tables of the other kinds do not bound it. The last bit, clear of
    /// [`Self::NAMED`]'s.
    pub const ANY_METHOD: Self = Self(1 << 31);
    /// Every read: a library call whose `[[invokes]]` entry does not say what it runs, a method
    /// no kind names.
    pub const ALL: Self = Self(u32::MAX);
    /// An operand of an operator whose other operand is a builtin: besides the operand's own
    /// operator methods, what the builtin's runs of it. `xs += v` extends a list, as
    /// [`Self::UNPACKING`] does; `d |= v` looks `keys` up on `v` by name (`__getattr__` when the
    /// class has none), then reads `keys()` and the items, or iterates `v` as pairs; `"%s" % v`
    /// converts `v` to text.
    pub const OPERAND: Self = Self(
        Self::OPERATORS.0
            | Self::UNPACKING.0
            | Self::ITEM.0
            | Self::TEXT.0
            | Self::BY_NAME.0
            | Self::ANY_ATTRIBUTE.0
            | Self::KEYS,
    );
    /// `[*v]`, `(*v,)`, `{*v}`, `f(*v)`, `a, *rest = v`: iteration, and the length hint
    /// (`__len__`, else `__length_hint__`) CPython takes before it extends a list or builds a
    /// tuple from `v`. A set display and a starred target take none; they are read alike, which
    /// only leaves out more.
    pub const UNPACKING: Self = Self(Self::ITERATION.0 | Self::SIZE.0);
    /// `a, b = v`: iteration, never an item read.
    pub const TARGET_UNPACKING: Self = Self::ITERATION;
    /// `{**v}`, `f(**v)`: `keys()`, looked up by name, then the item of each key. A `dict` whose
    /// `__iter__` is its own is copied from its storage, which runs neither.
    pub const MAPPING_UNPACK: Self = Self(Self::ITEM.0 | Self::BY_NAME.0 | Self::KEYS);
    /// `case [a, b]`, `case []`: the subject's length, then its iteration, or its items when a
    /// `*_` sits among them.
    pub const SEQUENCE_PATTERN: Self = Self(Self::SIZE.0 | Self::ITERATION.0 | Self::ITEM.0);
    /// `case {"k": v}`: the subject's length and its `get`, looked up by name.
    pub const MAPPING_PATTERN: Self = Self(Self::SIZE.0 | Self::BY_NAME.0 | Self::GET);

    /// The implicit reads, by bit, with what they are and the special methods each may run
    /// (none listed for [`Self::ANY_METHOD`]: [`read_override`] takes every member).
    const IMPLICIT: [(Self, &str, &[&str]); 10] = [
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
        (Self::ANY_METHOD, "any method", &[]),
    ];

    /// The methods of `list`, `dict`, and `set` that only read or give a view, called by name:
    /// bit `9 + i` each, `keys` first ([`Self::KEYS`]), then `get` ([`Self::GET`]).
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
    const GET: u32 = Self::named_bit(1);

    /// What the shape test of a pattern reads of its subject.
    pub(crate) const fn of_pattern(shape: PatternShape) -> Self {
        match shape {
            PatternShape::Sequence => Self::SEQUENCE_PATTERN,
            PatternShape::Mapping => Self::MAPPING_PATTERN,
        }
    }

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
    /// `[[invokes]]` entry names for the argument at `position`, every kind without an entry.
    pub(crate) fn of_library_call(kb: &EffectiveEcosystemKb, symbol: &str, position: u8) -> Self {
        let Some(entry) = kb.invoked(symbol) else {
            return Self::ALL;
        };
        let named = (entry.values.iter()).filter(|v| v.names(Some(position), None));
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

// The named methods' bits stay below `ANY_METHOD`'s.
const _: () = assert!(ReadKinds::named_bit(ReadKinds::NAMED.len()) <= ReadKinds::ANY_METHOD.0);

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
/// the object may be binds the method the read runs, or any member when a read has no list of
/// the methods it runs ([`ReadKinds::ANY_METHOD`]), or a metaclass heki does not know creates
/// it, which may install one; or a project class inherits from a standard-library base a read
/// method that changes the object (`defaultdict`'s `__missing__`). Or the family is open: a value
/// of the read-only type may be of a class heki does not list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOverride {
    Open(FamilyOpen),
    Binds {
        class: String,
        method: &'static str,
    },
    BindsAny {
        class: String,
        member: String,
    },
    Metaclass {
        class: String,
    },
    Inherits {
        class: String,
        base: String,
        method: &'static str,
    },
}

/// The first project class, in [`ProjectDecls::implementers`]' order, whose code the reads
/// `kinds` of an object of the read-only type of the builtin `kind` may run (the annotation the
/// suggestion writes: a `Mapping` may be an instance of a project subclass of `UserDict`): one binding a method the reads run
/// ([`ReadKinds::methods`]), one whose MRO has a base outside the project binding one that
/// changes the object ([`ContainerFamily::changing_read`]), one binding anything when a read
/// may run any method or inherits a standard-library class whose methods are Python code
/// ([`ContainerFamily::runs_python_methods`]) and binds any member, or one created by a
/// metaclass heki does not know. `None` when every read
/// is the builtin's own and the family is closed.
pub fn read_override(
    decls: &ProjectDecls,
    kind: ContainerKind,
    kinds: ReadKinds,
) -> Option<ReadOverride> {
    if kinds == ReadKinds::NONE {
        return None;
    }
    let methods: Vec<&'static str> = kinds.methods().collect();
    let any = kinds.contains(ReadKinds::ANY_METHOD);
    let members = decls.implementers(kind, FamilyLevel::ReadOnly);
    if let Some(open) = &members.open {
        return Some(ReadOverride::Open(open.clone()));
    }
    (members.classes.iter()).find_map(|class| {
        let class = class.clone();
        let d = decls.class_decl(&class)?;
        // The project classes the receiver takes its methods from: the class and every
        // project base in its MRO, mixins that do not derive from the builtin included.
        let mro = decls.mro_of(&class);
        let project: Vec<&crate::ClassDecl> = std::iter::once(d)
            .chain(
                (mro.iter())
                    .filter(|b| **b != class)
                    .filter_map(|b| decls.class_decl(b)),
            )
            .collect();
        if let Some(method) = methods
            .iter()
            .find(|m| project.iter().any(|p| p.members.contains(**m)))
        {
            return Some(ReadOverride::Binds { class, method });
        }
        let inherited = (decls.mro_of(&class).into_iter())
            .find_map(|base| Some((ContainerFamily::changing_read(&base, &methods)?, base)));
        if let Some((method, base)) = inherited {
            return Some(ReadOverride::Inherits {
                class,
                base,
                method,
            });
        }
        let python_based = mro.iter().any(|b| ContainerFamily::runs_python_methods(b));
        let member = project.iter().find_map(|p| p.members.first());
        match member.filter(|_| any || python_based) {
            Some(member) => Some(ReadOverride::BindsAny {
                member: member.clone(),
                class,
            }),
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
    /// Nothing heki knows of: every decorator is one the decorator table knows by its
    /// definition, which leaves annotations unread, or one the catalog lists as reading only
    /// fields; the class has no metaclass but `type` or `abc.ABCMeta`, and no
    /// base outside the project but one of the standard library or one the catalog lists as
    /// reading only fields.
    Unread,
    /// The catalog lists the decorator or base named as reading them.
    Read(Box<str>),
    /// A decorator, metaclass, or base heki does not know, named, may read them.
    Unknown(Box<str>),
}

/// [`AnnotationFacts`] of every function of the module `cx` describes that has a parameter,
/// by qualified name. `semantic` is the module's scopes.
pub(crate) fn annotation_facts(
    cx: &TypeCtx<'_>,
    semantic: &SemanticModel,
) -> BTreeMap<String, AnnotationFacts> {
    let ir = cx.ir();
    let parameters = semantic.parameters();
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
            .map(|p| {
                let scope = p
                    .annotation_span
                    .and_then(|s| parameters.function_at(s.start));
                let resolve = |name: &str| annotation_name(cx, semantic, scope, name);
                param_annotation(p.annotation.as_deref(), p.annotation_span, &resolve)
            })
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

/// What the name `name` (dotted) of an annotation of a parameter of the function scope `scope`
/// resolves to: the module's binding, unless a function or class body around the `def` binds
/// its head where the annotation is evaluated, which a type checker reads it as too; nothing
/// without the scope.
fn annotation_name(
    cx: &TypeCtx<'_>,
    semantic: &SemanticModel,
    scope: Option<ScopeId>,
    name: &str,
) -> Option<String> {
    let head = name.split('.').next().unwrap_or(name);
    if semantic.annotation_binder(scope?, head).is_some() {
        return None;
    }
    cx.module.resolve_dotted(name, true)
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
    let constructor = matches!(f.name.as_str(), "__new__" | "__init__");
    let class_bodies =
        (cx.ir().functions.iter()).filter(|g| g.is_class_body && &g.qualname == owner);
    for body in class_bodies {
        let signature_read = constructor && !has_docstring(body);
        for d in &body.decorators {
            reads = reads.or(decorator_reads(cx, d));
            let documents = |e: &decorators::Decorator| {
                e.annotations == ReadsAnnotations::ConstructorWithoutDocstring
            };
            if signature_read && decorators::is(d, cx.module, documents) {
                let by = cx.module.decorator_symbol(d).unwrap_or_else(|| d.clone());
                reads = reads.or(AnnotationReads::Read(by.as_str().into()));
            }
        }
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

/// Whether the class body `body` starts with a docstring.
fn has_docstring(body: &IrFunction) -> bool {
    let mut insts: Vec<_> = body.blocks.iter().flat_map(|b| &b.insts).collect();
    insts.sort_by_key(|i| i.span.start);
    insts.first().is_some_and(|first| match &first.kind {
        InstKind::Const {
            dst,
            literal: ConstLiteral::Str(_),
        } => insts.iter().all(|i| !i.kind.operands().contains(dst)),
        _ => false,
    })
}

/// What the decorator `text` (its source) reads of what it decorates, besides a constructor's
/// signature ([`annotation_reads`]).
fn decorator_reads(cx: &TypeCtx<'_>, text: &str) -> AnnotationReads {
    if decorators::is(text, cx.module, |d| {
        d.annotations != ReadsAnnotations::Unknown
    }) {
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
