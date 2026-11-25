//! The module-level names an annotation reads as the type they name, though they are no class
//! ([`ModuleDecls::type_names`]: type aliases and bound type variables), and the text of the type
//! expression a value spells ([`type_expression_text`]), which reads as the same text in an
//! annotation does: the value of an alias, the bound of a type variable, and the type
//! `typing.cast` names.

use super::{ModuleDecls, unquote};
use crate::ModuleInput;
use crate::types::Ty;
use heki_ir::walk::{Answer, Budget, FnShape, Visit, reach};
use heki_ir::{BinOpKind, BuiltinKind, ConstLiteral, GlobalStores, InstKind, VarId};
use rustc_hash::FxHashMap;

/// The most parts (names, attributes, subscripts, operands, constants) of a type expression
/// [`type_expression_text`] spells: more than any type heki keeps has, a union of at most three
/// classes with the types of their items.
const MAX_TYPE_EXPRESSION_PARTS: usize = 64;

/// [`ModuleDecls::type_names`] of the module, into `decls`: names the module body binds once,
/// in a module where no star import, `exec`, or `globals()` may bind them again. The aliases
/// come first, in the order the body binds them: an alias's value may name an alias bound
/// before it, and no type variable, which stands there for any type, not for its bound. A type
/// variable's bound or constraints may name an alias.
pub(super) fn type_names(input: &ModuleInput<'_>, decls: &mut ModuleDecls) {
    if input.ir.constants.dynamic || input.ir.has_star_import {
        return;
    }
    let body = input.ir.function(input.ir.module_fn);
    let stores = GlobalStores::of(&input.ir.functions);
    let shape = FnShape::new(body);
    let annotations: FxHashMap<&str, &str> = body
        .local_annotations
        .iter()
        .map(|a| (a.target.as_str(), a.annotation.as_str()))
        .collect();
    let bound_once = || {
        body.blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|inst| match &inst.kind {
                InstKind::StoreGlobal { name, value } if stores.once(name) => Some((name, *value)),
                _ => None,
            })
    };
    for (name, value) in bound_once() {
        let annotation = annotations.get(name.as_str()).copied();
        let ty = alias_value(input, &shape, decls, annotation, value)
            .and_then(|text| decls.resolve_annotation_ty(&text));
        if let Some(ty) = ty {
            decls.type_names.insert(name.clone(), ty);
        }
    }
    for (name, value) in bound_once() {
        if let Some(ty) = type_var_type(&shape, decls, value) {
            decls.type_names.insert(name.clone(), ty);
        }
    }
}

/// The text of the type the module-level store of `value` (annotated `annotation`) makes its
/// name an alias of, as a type checker reads it: the value of a store annotated `TypeAlias` (the
/// text of a string included), of a `type` statement, or of a store of a subscript or a union
/// without an annotation (`Names = frozenset[str]`, `Operation = Added | Removed`). `None` for
/// any other store: one annotated otherwise binds a value of its annotation.
fn alias_value(
    input: &ModuleInput<'_>,
    shape: &FnShape<'_>,
    decls: &ModuleDecls,
    annotation: Option<&str>,
    value: VarId,
) -> Option<String> {
    if let Some(annotation) = annotation {
        let alias = decls
            .resolve_dotted(unquote(annotation), true)
            .is_some_and(|a| {
                matches!(
                    a.as_str(),
                    "typing.TypeAlias" | "typing_extensions.TypeAlias"
                )
            });
        return alias.then(|| type_expression_text(shape, value)).flatten();
    }
    let root = shape.copy_root(value);
    match &shape.inst(root)?.kind {
        InstKind::MakeClosure { func_id: None, .. } => {
            input.ir.type_value(shape.f.id, root).map(str::to_owned)
        }
        InstKind::SubscriptLoad {
            is_slice: false, ..
        }
        | InstKind::BinOp {
            op: BinOpKind::BitOr,
            in_place: false,
            ..
        } => type_expression_text(shape, root),
        _ => None,
    }
}

/// The type an annotation naming the module-level variable bound to `value` gives, when `value`
/// is a `TypeVar(...)` call (`typing` or `typing_extensions`) whose `bound=` or constraints the
/// module's names resolve to classes: the bound, or the union of the constraints. A bound or a
/// constraint is a type expression ([`type_expression_text`]: `bound="ContextView"`,
/// `bound=views.ContextView`); anything else leaves the variable unbounded.
fn type_var_type(shape: &FnShape<'_>, decls: &ModuleDecls, value: VarId) -> Option<Ty> {
    let text = |var: VarId| type_expression_text(shape, var);
    let (callee, args, kwargs) = match &shape.inst(shape.copy_root(value))?.kind {
        InstKind::Call {
            callee_var: Some(c),
            args,
            kwargs,
            star_from: None,
            ..
        } => (text(*c)?, args, kwargs),
        InstKind::MethodCall {
            recv,
            method,
            args,
            kwargs,
            star_from: None,
            ..
        } => (format!("{}.{method}", text(*recv)?), args, kwargs),
        _ => return None,
    };
    let is_type_var = decls
        .resolve_dotted(&callee, false)
        .is_some_and(|c| matches!(c.as_str(), "typing.TypeVar" | "typing_extensions.TypeVar"));
    if !is_type_var {
        return None;
    }
    let bound = kwargs
        .iter()
        .find(|(k, _)| k.as_deref() == Some("bound"))
        .map(|(_, v)| *v);
    match (bound, args.get(1..).unwrap_or_default()) {
        (Some(bound), []) => decls.resolve_annotation_ty(&text(bound)?),
        (None, constraints) if !constraints.is_empty() => {
            let texts: Vec<String> = constraints
                .iter()
                .map(|c| text(*c))
                .collect::<Option<_>>()?;
            let parts: Vec<&str> = texts.iter().map(String::as_str).collect();
            decls.union_annotation(&parts)
        }
        _ => None,
    }
}

/// The text of the type expression `var` spells, as an annotation writes it: the text of a
/// string, or an expression of names of module-level objects (any name of the module body, a
/// global a function loads), attributes of them, subscripts (`dict[str, Model]`), unions (`Model
/// | None`), `None`, `...`, strings (forward references), and lists (`Callable[[Model], None]`).
/// `None` for any other value (a local, a parameter, a call's result, a value of several paths)
/// and past [`MAX_TYPE_EXPRESSION_PARTS`] parts, counted as the text writes them: a value read by
/// two parts (`kind | kind`) is written twice, so a chain of such unions doubles its text with
/// every link while it adds one value.
pub(crate) fn type_expression_text(shape: &FnShape<'_>, var: VarId) -> Option<String> {
    let budget = Budget::unmetered("heki_effect::resolver::type_names::type_expression_text");
    let mut parts: Vec<VarId> = Vec::new();
    let other = reach(
        &budget,
        [var],
        |v, next| {
            if let Some(part) = TypePart::of(shape, v) {
                part.operands(next);
            }
        },
        |v| {
            parts.push(v);
            match TypePart::of(shape, v) {
                Some(_) if parts.len() <= MAX_TYPE_EXPRESSION_PARTS => Visit::Expand,
                _ => Visit::Stop(()),
            }
        },
    );
    if other != Answer::Known(None) {
        return None;
    }
    // The builder numbers values in the order it defines them, and defines the operands of an
    // expression before the expression: in that order each part's own parts are spelled first,
    // also one that several parts read (a name of the module body).
    parts.sort_unstable();
    let mut texts: FxHashMap<VarId, String> = FxHashMap::default();
    let mut written: FxHashMap<VarId, usize> = FxHashMap::default();
    let mut operands = Vec::new();
    for v in parts {
        let part = TypePart::of(shape, v)?;
        operands.clear();
        part.operands(&mut operands);
        let size = 1 + operands
            .iter()
            .map(|o| written.get(o).copied())
            .sum::<Option<usize>>()?;
        if size > MAX_TYPE_EXPRESSION_PARTS {
            return None;
        }
        written.insert(v, size);
        let text = part.text(&texts, v == var)?;
        texts.insert(v, text);
    }
    texts.remove(&var)
}

/// One part of a type expression ([`type_expression_text`]).
enum TypePart<'a> {
    /// A name of a module-level object.
    Name(&'a str),
    Str(&'a str),
    /// `None` or `...`.
    Constant(&'static str),
    Copy(VarId),
    Attr(VarId, &'a str),
    Subscript(VarId, VarId),
    Union(VarId, VarId),
    /// The arguments of a subscript (`str, Model` of `dict[str, Model]`).
    Tuple(&'a [VarId]),
    /// The parameters of a `Callable` (`[Model]` of `Callable[[Model], None]`).
    List(&'a [VarId]),
}

impl<'a> TypePart<'a> {
    /// The part `v` is, if it is one.
    fn of(shape: &FnShape<'a>, v: VarId) -> Option<Self> {
        let f = shape.f;
        let inst = shape.inst(v);
        let name = match inst.map(|i| &i.kind) {
            Some(InstKind::LoadGlobal { name, .. }) => Some(name.as_str()),
            // Every load and every assignment of a name of the module body binds a variable the
            // IR names after it.
            _ if f.is_module_body => f.var_name(v).filter(|n| !n.starts_with('.')),
            _ => None,
        };
        if let Some(name) = name {
            return Some(Self::Name(name));
        }
        let unpacks = |elems: &[VarId]| elems.iter().any(|e| f.is_unpacked(*e));
        Some(match &inst?.kind {
            InstKind::Copy { src, .. } => Self::Copy(*src),
            InstKind::Const { literal, .. } => match literal {
                ConstLiteral::Str(s) => Self::Str(s),
                ConstLiteral::None => Self::Constant("None"),
                ConstLiteral::Ellipsis => Self::Constant("..."),
                _ => return None,
            },
            InstKind::AttrLoad { obj, attr, .. } => Self::Attr(*obj, attr),
            InstKind::SubscriptLoad {
                obj,
                index,
                is_slice: false,
                ..
            } => Self::Subscript(*obj, *index),
            InstKind::BinOp {
                op: BinOpKind::BitOr,
                lhs,
                rhs,
                in_place: false,
                ..
            } => Self::Union(*lhs, *rhs),
            InstKind::AllocBuiltin {
                kind: BuiltinKind::Tuple,
                elems,
                ..
            } if !unpacks(elems) => Self::Tuple(elems),
            InstKind::AllocBuiltin {
                kind: BuiltinKind::List,
                elems,
                ..
            } if !unpacks(elems) => Self::List(elems),
            _ => return None,
        })
    }

    fn operands(&self, next: &mut Vec<VarId>) {
        match self {
            Self::Copy(v) | Self::Attr(v, _) => next.push(*v),
            Self::Subscript(a, b) | Self::Union(a, b) => next.extend([*a, *b]),
            Self::Tuple(elems) | Self::List(elems) => next.extend(elems.iter().copied()),
            Self::Name(_) | Self::Str(_) | Self::Constant(_) => {}
        }
    }

    /// The part's text, from the texts of its operands (`spelled`); `whole` for the expression
    /// itself, whose string is the text of an annotation.
    fn text(&self, spelled: &FxHashMap<VarId, String>, whole: bool) -> Option<String> {
        let of = |v: &VarId| spelled.get(v).map(String::as_str);
        let joined = |elems: &[VarId]| {
            elems
                .iter()
                .map(of)
                .collect::<Option<Vec<&str>>>()
                .map(|texts| texts.join(", "))
        };
        Some(match self {
            Self::Name(name) => (*name).to_owned(),
            Self::Str(text) if whole => (*text).to_owned(),
            // A forward reference inside an expression is quoted, as an annotation writes it.
            Self::Str(text) => format!("\"{text}\""),
            Self::Constant(text) => (*text).to_owned(),
            Self::Copy(v) => of(v)?.to_owned(),
            Self::Attr(obj, attr) => format!("{}.{attr}", of(obj)?),
            Self::Subscript(obj, index) => format!("{}[{}]", of(obj)?, of(index)?),
            Self::Union(lhs, rhs) => format!("{} | {}", of(lhs)?, of(rhs)?),
            // `tuple[()]` subscripts the empty tuple.
            Self::Tuple([]) => "()".to_owned(),
            Self::Tuple(elems) => joined(elems)?,
            Self::List(elems) => format!("[{}]", joined(elems)?),
        })
    }
}
