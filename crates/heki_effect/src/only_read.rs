//! The parameters a function only reads ([`EffectSummary::only_read_params`]), and what its own
//! body does with the parameters annotated with a mutable container ([`ContainerParams`]): the
//! facts a read-only annotation (`Sequence`, `Mapping`, `AbstractSet`) of such a parameter needs.
//!
//! One judgment has two readers: whether a function changes the object itself comes from
//! [`EffectSummary::mutated_param_paths`] alone, the fact `MUT110` reports, so a parameter only
//! read here is never one `MUT110` would report changed. The rest (stores, captures, returns,
//! the calls it is handed to) is this module's: the extractor finds it in the function's own
//! body, and propagation joins what the project callees do with it.

use crate::callgraph::bound_params;
use crate::readonly::ContainerKind;
use crate::resolver::{TypeCtx, func_qualname, is_unknown_base, param_annotation};
use crate::{BoundCallee, EffectSummary, decorators};
use heki_ast::Span;
use heki_ir::IrFunction;
use heki_kb::AnnotationsRead;
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
    if let Some(body) = (cx.ir().functions.iter()).find(|g| g.is_class_body && &g.qualname == owner)
    {
        for d in &body.decorators {
            reads = reads.or(decorator_reads(cx, d));
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
