//! The fields the constructor of a `typing.NamedTuple` or of a `@dataclass(frozen=True)` binds
//! ([`super::ClassDecl::init_fields`], [`ProjectDecls::constructor_fields`]), and what an
//! instance such a call makes holds in them ([`crate::types::Args::Fields`]): the argument of
//! each field's position or keyword, or its default. Storing a field of such an instance raises
//! (`AttributeError` on a named tuple, `FrozenInstanceError` on a frozen dataclass), so it holds
//! that value while it lives, unless code stores it through `object.__setattr__`, which the
//! stores of its class show ([`ProjectDecls::field_holds_given`]).

use super::sentinels::{field_default, init_left_on, named_value};
use super::{
    ModuleDecls, NAMED_TUPLE_BASES, PLAIN_BASES, ProjectDecls, ReadOnlyKind, TypeCtx,
    dataclass_decorator, decorator_parts, keyword_bool, note, qualifiers, unquote,
};
use crate::ModuleInput;
use crate::infer::Inferred;
use crate::readonly::ContainerFamily;
use crate::types::Ty;
use heki_ir::walk::FnShape;
use heki_ir::{ConstLiteral, GlobalStores, InstKind, IrFunction, VarId};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The parameters the generated constructor of a class takes for its own fields, in order
/// ([`super::ClassDecl::init_fields`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitFields {
    /// The class is a `typing.NamedTuple`; otherwise a frozen stdlib dataclass, whose
    /// constructor also takes the fields of its dataclass bases first.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub named_tuple: bool,
    pub fields: Box<[InitField]>,
}

/// One field a generated constructor binds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitField {
    pub name: Box<str>,
    /// Only a keyword binds it (`kw_only=True` on the class or the field, or after `KW_ONLY`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub kw_only: bool,
    pub default: FieldDefault,
}

/// What a field holds when the call binds nothing to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FieldDefault {
    /// It has no default: a call leaving it unbound raises `TypeError`.
    Required,
    /// `None`.
    None,
    /// The module-level name the class body binds it to, by the fqn its module gives it
    /// (`run: Callable[[], None] = noop`, `field(default=noop)`).
    Named(Box<str>),
    /// A literal, a display, or what a default factory makes: a value of no named object, and
    /// no descriptor the class would hold.
    Value,
    /// Any other value, which the class of a dataclass holds and which may be a descriptor that
    /// reading the field runs.
    Other,
}

/// [`super::ClassDecl::init_fields`] of the module's classes: a `NamedTuple` class, and a
/// `@dataclass(frozen=True)` that leaves `init` on, each defining none of the methods that
/// would make or change an instance otherwise (`__init__`, `__new__`, `__post_init__`,
/// `__setattr__`) or hook its reads (`__getattr__`, `__getattribute__`).
pub(super) fn init_fields(input: &ModuleInput<'_>, decls: &mut ModuleDecls) {
    let stores = GlobalStores::of(&input.ir.functions);
    let found: Vec<(String, InitFields)> = input
        .ir
        .functions
        .iter()
        .filter(|f| f.is_class_body)
        .filter_map(|body| Some((body.qualname.clone(), class_fields(body, decls, &stores)?)))
        .collect();
    for (qualname, fields) in found {
        if let Some(c) = decls.classes.get_mut(&qualname) {
            c.init_fields = Some(Box::new(fields));
        }
    }
}

/// The methods whose definition in the class makes its constructor bind something other than
/// its fields, or its instances hold something else in them.
const CONSTRUCTION_HOOKS: &[&str] = &[
    "__init__",
    "__new__",
    "__post_init__",
    "__setattr__",
    "__getattr__",
    "__getattribute__",
];

/// The fields of the class whose body is `body`, when a generated constructor binds them
/// ([`init_fields`]).
fn class_fields(
    body: &IrFunction,
    decls: &ModuleDecls,
    stores: &GlobalStores<'_>,
) -> Option<InitFields> {
    let resolve = |name: &str| decls.resolve_dotted(name, true);
    let class = decls.classes.get(&body.qualname)?;
    let named_tuple = class
        .bases
        .iter()
        .any(|b| NAMED_TUPLE_BASES.contains(&b.as_str()));
    let decorator = body
        .decorators
        .iter()
        .find(|d| dataclass_decorator(d, &resolve).is_some());
    let mut kw_only = false;
    match (named_tuple, decorator) {
        (true, None) => {}
        (false, Some(d)) => {
            let args = decorator_parts(d).1;
            if dataclass_decorator(d, &resolve) != Some(true) || !init_left_on(args) {
                return None;
            }
            kw_only = keyword_flag(args, "kw_only")?;
        }
        _ => return None,
    }
    if CONSTRUCTION_HOOKS
        .iter()
        .any(|m| class.members.contains(*m))
    {
        return None;
    }
    let shape = FnShape::new(body);
    let mut bound: FxHashMap<&str, Vec<VarId>> = FxHashMap::default();
    for inst in body.blocks.iter().flat_map(|b| &b.insts) {
        if let Some(name) = inst.kind.dst().and_then(|d| body.var_name(d)) {
            let value = match &inst.kind {
                InstKind::Copy { src, .. } => Some(*src),
                _ => None,
            };
            bound.entry(name).or_default().extend(value);
        }
    }
    let mut fields: Vec<InitField> = Vec::new();
    for ann in &body.local_annotations {
        if ann.target.contains('.') {
            continue;
        }
        let (q, _) = qualifiers(&ann.annotation, &resolve);
        if q.class_var {
            continue;
        }
        if q.pseudo_field {
            // The fields after `_: KW_ONLY` take keywords only; an `InitVar` is a parameter
            // that is no field.
            let text = unquote(&ann.annotation).trim();
            let sentinel = resolve(text).as_deref() == Some("dataclasses.KW_ONLY");
            if named_tuple || !sentinel {
                return None;
            }
            kw_only = true;
            continue;
        }
        if fields.iter().any(|f| *f.name == *ann.target) {
            return None;
        }
        let values = bound
            .get(ann.target.as_str())
            .map_or(&[][..], Vec::as_slice);
        let (default, field_kw_only) = match values {
            [] if bound.contains_key(ann.target.as_str()) => return None,
            [] => (FieldDefault::Required, false),
            [value] => field_value(&shape, *value, decls, stores, !named_tuple)?,
            _ => return None,
        };
        fields.push(InitField {
            name: ann.target.as_str().into(),
            kw_only: kw_only || field_kw_only,
            default,
        });
    }
    Some(InitFields {
        named_tuple,
        fields: fields.into_boxed_slice(),
    })
}

/// The literal flag the decorator arguments `args` pass as `name` (false when they pass none);
/// `None` for one that is no literal.
fn keyword_flag(args: &str, name: &str) -> Option<bool> {
    let passed = super::split_top_level(args, ',')
        .iter()
        .any(|arg| arg.split_once('=').is_some_and(|(k, _)| k.trim() == name));
    match keyword_bool(args, name) {
        Some(flag) => Some(flag),
        None => (!passed).then_some(false),
    }
}

/// The default of a field whose class body binds it to `value`, and whether a field
/// specifier makes it keyword-only (`field(kw_only=True)`). `None` for a field the constructor
/// does not take (`field(init=False)`) or a specifier heki does not read.
fn field_value(
    shape: &FnShape<'_>,
    value: VarId,
    decls: &ModuleDecls,
    stores: &GlobalStores<'_>,
    dataclass: bool,
) -> Option<(FieldDefault, bool)> {
    let resolve = |name: &str| decls.resolve_dotted(name, true);
    let root = shape.inst(shape.copy_root(value)).map(|i| &i.kind);
    if let Some(InstKind::Call {
        callee,
        args,
        kwargs,
        star_from,
        ..
    }) = root
        && resolve(callee).as_deref() == Some("dataclasses.field")
    {
        if !dataclass || !args.is_empty() || star_from.is_some() {
            return None;
        }
        let keyword = |key: &str| {
            kwargs
                .iter()
                .find_map(|(k, v)| (k.as_deref() == Some(key)).then_some(*v))
        };
        let flag = |key: &str| keyword(key).map(|v| shape.bool_const(v));
        let init_left_on = flag("init") != Some(Some(false));
        let (default, init) = match field_default(shape, value, &resolve) {
            Some((default, init)) => (default_of(shape, default, decls, stores), init),
            None if keyword("default_factory").is_some() => (FieldDefault::Value, init_left_on),
            None => (FieldDefault::Required, init_left_on),
        };
        let kw_only = match flag("kw_only") {
            None => false,
            Some(Some(flag)) => flag,
            Some(None) => return None,
        };
        let known =
            kwargs.iter().all(|(k, _)| k.is_some()) && flag("init").is_none_or(|f| f.is_some());
        return (init && known).then_some((default, kw_only));
    }
    Some((default_of(shape, value, decls, stores), false))
}

/// What a field defaulting to `value` holds: `None`, a module-level name, a literal or a
/// display, or another value.
fn default_of(
    shape: &FnShape<'_>,
    value: VarId,
    decls: &ModuleDecls,
    stores: &GlobalStores<'_>,
) -> FieldDefault {
    match shape.inst(shape.copy_root(value)).map(|i| &i.kind) {
        Some(InstKind::Const {
            literal: ConstLiteral::None,
            ..
        }) => return FieldDefault::None,
        Some(
            InstKind::Const { .. } | InstKind::AllocBuiltin { .. } | InstKind::DictLiteral { .. },
        ) => {
            return FieldDefault::Value;
        }
        _ => {}
    }
    match named_value(shape, value, decls, stores) {
        Some(name) => FieldDefault::Named(name.into()),
        None => FieldDefault::Other,
    }
}

impl ProjectDecls {
    /// The fields calling exactly the class `class` binds, in the order its positions bind
    /// them, the keyword-only ones among them: a `NamedTuple` class's own, or those of a frozen
    /// dataclass and its dataclass bases, a base's first and a field the class declares again
    /// keeping the base's place ([`super::ClassDecl::init_fields`]). `None` when another
    /// constructor runs: a base that is no such class or plain base, or a metaclass heki does
    /// not know.
    pub(crate) fn constructor_fields(&self, class: &str) -> Option<Vec<&InitField>> {
        let class = self.canonical(class);
        self.read(|k| note(&mut k.class_facts, &class));
        let decl = self.class(&class)?;
        let own = decl.init_fields.as_deref()?;
        if decl.custom_metaclass {
            return None;
        }
        if own.named_tuple {
            let plain = decl
                .bases
                .iter()
                .all(|b| PLAIN_BASES.contains(&self.canonical(b).as_str()));
            return plain.then(|| own.fields.iter().collect());
        }
        let mut out: Vec<&InitField> = Vec::new();
        for c in self.mro(&class).iter().rev() {
            if PLAIN_BASES.contains(&c.as_str()) && !NAMED_TUPLE_BASES.contains(&c.as_str()) {
                continue;
            }
            self.read(|k| note(&mut k.class_facts, c));
            let d = self.class(c)?;
            let fields = d.init_fields.as_deref().filter(|f| !f.named_tuple)?;
            if d.custom_metaclass {
                return None;
            }
            for f in &fields.fields {
                match out.iter_mut().find(|o| o.name == f.name) {
                    Some(o) => *o = f,
                    None => out.push(f),
                }
            }
        }
        let (positional, keyword): (Vec<&InitField>, Vec<&InitField>) =
            out.into_iter().partition(|f| !f.kw_only);
        Some(positional.into_iter().chain(keyword).collect())
    }

    /// Whether an instance of exactly the class `class` holds in its field `attr` what the call
    /// constructing it bound ([`Self::constructor_fields`]): the field is one; along the MRO,
    /// every class binding the name binds it as a field of its own, to a default that is no
    /// descriptor reading it would run (a named tuple's fields are its items whatever their
    /// defaults); no code of the project stores it on an instance of such a class (through
    /// `object.__setattr__`, which a plain store would not get past) or stores attributes heki
    /// cannot name there; no module binds it on a class; and reading it runs no hook.
    pub(crate) fn field_holds_given(&self, class: &str, attr: &str) -> bool {
        let class = self.canonical(class);
        let is_field = self
            .constructor_fields(&class)
            .is_some_and(|fields| fields.iter().any(|f| &*f.name == attr));
        if !is_field || self.attr_read(&class, attr).runs_hook(false) {
            return false;
        }
        self.mro(&class).iter().all(|c| {
            self.read(|k| note(&mut k.class_facts, c));
            let Some(d) = self.class(c) else {
                return true;
            };
            let binding_is_field = || {
                d.init_fields.as_deref().is_some_and(|own| {
                    own.fields.iter().any(|f| {
                        &*f.name == attr
                            && (own.named_tuple || self.default_runs_nothing(&f.default))
                    })
                })
            };
            (!d.members.contains(attr) || !d.properties.contains(attr) && binding_is_field())
                && !d.rebound_attrs.contains(attr)
                && self.attr_store(c, d, attr).is_none()
                && !self.stores_any_attr(c, d)
                && !self.rebound_elsewhere(&format!("{c}.{attr}"))
        })
    }

    /// Whether a dataclass whose field defaults to `default` holds no descriptor of the field
    /// that reading it on an instance would run: none, `None`, a literal, a display, a default
    /// factory's result, or a project function or class, which no instance's own value is
    /// read through.
    fn default_runs_nothing(&self, default: &FieldDefault) -> bool {
        match default {
            FieldDefault::Required | FieldDefault::None | FieldDefault::Value => true,
            FieldDefault::Named(name) => matches!(
                self.symbol(&self.object_named(name)),
                super::Symbol::Function(..) | super::Symbol::Class(_)
            ),
            FieldDefault::Other => false,
        }
    }
}

/// What a module's calls bind to the read-only container fields of the instances they make
/// ([`super::ModuleDecls::field_values`]): by the class the call makes (or may make a subclass
/// of), then the field, whether a value bound there may be changed in place. A copy of an
/// object of unknown class binds under [`ANY_CLASS`] the fields it may change, by name, or
/// [`ANY_FIELD`] when it unpacks them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldValues {
    pub bound: BTreeMap<String, BTreeMap<Box<str>, Changeable>>,
}

/// Whether the values a module binds to one field may be changed in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Changeable {
    /// Every one is an instance of a class rejecting every change of its container family
    /// ([`ContainerFamily::rejects_changes`]).
    No,
    /// One is a mutable container, a value of a class heki does not know, or a default.
    Maybe,
}

impl FieldValues {
    fn bind(&mut self, class: &str, field: &str, value: Changeable) {
        let fields = self.bound.entry(class.to_owned()).or_default();
        let joined = fields.entry(field.into()).or_insert(value);
        *joined = (*joined).max(value);
    }
}

/// [`super::ModuleDecls::field_values`] of a module: what each call making an instance of a
/// project class whose generated constructor binds read-only container fields
/// ([`ProjectDecls::constructor_fields`], [`ProjectDecls::readonly_attr`]) binds to them, by the
/// class's own call (`Segment(...)`), a class object that may be a subclass (`cls(...)`), or a
/// copy with fields replaced (`dataclasses.replace(segment, meta=...)`,
/// `segment._replace(meta=...)`). A call that may bind fields heki does not see (`*` or `**`
/// unpacking, a replacement by position) binds a value that may change to each of them.
///
/// Only a function that may make such a call is inferred ([`may_make_instances`]).
pub(super) fn field_values(input: &ModuleInput<'_>, cx: &TypeCtx<'_>) -> Option<Box<FieldValues>> {
    let mut out = FieldValues::default();
    let mut tracked: FxHashMap<String, Vec<Box<str>>> = FxHashMap::default();
    for f in &input.ir.functions {
        if !may_make_instances(f, cx, &mut tracked) {
            continue;
        }
        let types = cx.infer_types(f);
        let values = Values { cx, types: &types };
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            match &inst.kind {
                InstKind::Call {
                    callee_var,
                    args,
                    kwargs,
                    star_from,
                    ..
                } => {
                    let target = match callee_var.and_then(|c| types.ty(c)) {
                        Some(Ty::Ref(fqn)) => Target::Named(fqn.clone()),
                        Some(Ty::Class(class)) => Target::Class(class.clone()),
                        Some(_) => Target::Other,
                        None => Target::Unknown,
                    };
                    values.call(target, args, kwargs, *star_from, &mut tracked, &mut out);
                }
                InstKind::MethodCall {
                    recv,
                    method,
                    args,
                    kwargs,
                    star_from,
                    ..
                } if method == "_replace" => {
                    let replaced = Replaced {
                        original: *recv,
                        by_position: !args.is_empty() || star_from.is_some(),
                        kwargs,
                    };
                    values.replaced(&replaced, &mut tracked, &mut out);
                }
                InstKind::MethodCall {
                    recv,
                    method,
                    args,
                    kwargs,
                    star_from,
                    ..
                } => {
                    // `module.Class(...)` and `module.replace(...)` read as a method of the
                    // module object.
                    let target = match types.ty(*recv) {
                        Some(Ty::Ref(module)) => {
                            Target::Named(cx.project.canonical(&format!("{module}.{method}")))
                        }
                        Some(_) => Target::Other,
                        None => Target::Unknown,
                    };
                    values.call(target, args, kwargs, *star_from, &mut tracked, &mut out);
                }
                _ => {}
            }
        }
    }
    (!out.bound.is_empty()).then(|| Box::new(out))
}

/// Whether `f` may make an instance [`field_values`] records: it calls a name the module binds to
/// a class with tracked fields ([`tracked_fields`]) or to a copy the catalog lists
/// (`dataclasses.replace`), a name the module does not bind (a parameter such as `cls`, a local
/// alias, a local import), or a `_replace` method.
fn may_make_instances(
    f: &IrFunction,
    cx: &TypeCtx<'_>,
    tracked: &mut FxHashMap<String, Vec<Box<str>>>,
) -> bool {
    let defs: FxHashMap<VarId, &InstKind> = (f.blocks.iter().flat_map(|b| &b.insts))
        .filter_map(|i| Some((i.kind.dst()?, &i.kind)))
        .collect();
    f.blocks
        .iter()
        .flat_map(|b| &b.insts)
        .any(|inst| match &inst.kind {
            InstKind::Call { callee, .. } => match cx.module.resolve_dotted(callee, false) {
                None => true,
                Some(fqn) => {
                    let fqn = cx.project.canonical(&fqn);
                    !tracked_fields(cx.project, &fqn, tracked).is_empty()
                        || cx.catalog(&fqn).is_some_and(|e| e.copy_sanitizer.is_some())
                }
            },
            InstKind::MethodCall { recv, method, .. } => {
                method == "_replace" || may_name_instances(&defs, *recv, method, cx, tracked)
            }
            _ => false,
        })
}

/// Whether `recv.method(...)` may call a class with tracked fields or a copy the catalog lists:
/// the dotted name it reads resolves to one, or a name the module does not bind; a receiver
/// that no instruction of the function defines (a parameter) may be anything.
fn may_name_instances(
    defs: &FxHashMap<VarId, &InstKind>,
    recv: VarId,
    method: &str,
    cx: &TypeCtx<'_>,
    tracked: &mut FxHashMap<String, Vec<Box<str>>>,
) -> bool {
    let Some(text) = dotted_text(defs, recv) else {
        return !defs.contains_key(&recv);
    };
    match cx.module.resolve_dotted(&format!("{text}.{method}"), false) {
        None => true,
        Some(fqn) => {
            let fqn = cx.project.canonical(&fqn);
            !tracked_fields(cx.project, &fqn, tracked).is_empty()
                || cx.catalog(&fqn).is_some_and(|e| e.copy_sanitizer.is_some())
        }
    }
}

/// The dotted name a chain of attribute reads of a name spells (`pkg.model`).
fn dotted_text(defs: &FxHashMap<VarId, &InstKind>, v: VarId) -> Option<String> {
    let mut attrs: Vec<&str> = Vec::new();
    let mut at = v;
    // Each step reads the object an earlier definition made, so the chain ends.
    loop {
        match defs.get(&at)? {
            InstKind::LoadGlobal { name, .. }
            | InstKind::LoadName { name, .. }
            | InstKind::LoadDeref { name, .. } => {
                attrs.push(name);
                attrs.reverse();
                return Some(attrs.join("."));
            }
            InstKind::AttrLoad { obj, attr, .. } => {
                attrs.push(attr);
                at = *obj;
            }
            _ => return None,
        }
    }
}

/// The read-only container fields ([`ProjectDecls::readonly_attr`]: of a frozen class, or
/// declared `Final` / `ReadOnly`) the generated constructor of the project class `class` binds,
/// kept by class.
fn tracked_fields<'t>(
    project: &ProjectDecls,
    class: &str,
    tracked: &'t mut FxHashMap<String, Vec<Box<str>>>,
) -> &'t [Box<str>] {
    tracked.entry(class.to_owned()).or_insert_with(|| {
        let Some(fields) = project.constructor_fields(class) else {
            return Vec::new();
        };
        fields
            .iter()
            .filter(|f| {
                project.readonly_attr(class, &f.name).is_some_and(|ro| {
                    ro.holds_container
                        && matches!(ro.kind, ReadOnlyKind::Frozen | ReadOnlyKind::Declared)
                })
            })
            .map(|f| f.name.clone())
            .collect()
    })
}

/// A copy of `original` with the fields `kwargs` names replaced, or (`by_position`) fields heki
/// cannot name.
struct Replaced<'i> {
    original: VarId,
    by_position: bool,
    kwargs: &'i [(Option<String>, VarId)],
}

/// The values one function's calls bind, by its inferred types.
struct Values<'v, 'a> {
    cx: &'v TypeCtx<'a>,
    types: &'v Inferred,
}

/// What a call's callee is, by the types of the function.
enum Target {
    /// A named object: a class, a function, a module attribute.
    Named(String),
    /// A class object that may be a subclass (`cls`).
    Class(String),
    /// Something heki types, that is no class object or named object.
    Other,
    /// Nothing heki knows: it may be a class of the project.
    Unknown,
}

impl Values<'_, '_> {
    /// What a call binds to the tracked fields, by its callee.
    fn call(
        &self,
        target: Target,
        args: &[VarId],
        kwargs: &[(Option<String>, VarId)],
        star_from: Option<u8>,
        tracked: &mut FxHashMap<String, Vec<Box<str>>>,
        out: &mut FieldValues,
    ) {
        match target {
            Target::Named(fqn) | Target::Class(fqn) if self.cx.project.is_project_class(&fqn) => {
                let fields = tracked_fields(self.cx.project, &fqn, tracked);
                self.constructed(&fqn, fields, args, kwargs, star_from, out);
            }
            Target::Named(fqn)
                if !args.is_empty()
                    && self
                        .cx
                        .catalog(&fqn)
                        .is_some_and(|e| e.copy_sanitizer.is_some()) =>
            {
                let replaced = Replaced {
                    original: args[0],
                    by_position: args.len() > 1 || star_from.is_some(),
                    kwargs,
                };
                self.replaced(&replaced, tracked, out);
            }
            Target::Unknown => self.unreadable(kwargs, star_from, out),
            _ => {}
        }
    }

    /// A call of a callee heki does not know, which may make an instance of any class: what it
    /// binds by keyword may change the field of that name, and a `*` or `**` unpacking, any
    /// field. Positions given to a callee heki cannot name at all are not read: that is a
    /// callback or a function far more often than a class.
    fn unreadable(
        &self,
        kwargs: &[(Option<String>, VarId)],
        star_from: Option<u8>,
        out: &mut FieldValues,
    ) {
        let unpacked = star_from.is_some() || kwargs.iter().any(|(k, _)| k.is_none());
        if unpacked {
            out.bind(ANY_CLASS, ANY_FIELD, Changeable::Maybe);
        }
        for (k, v) in kwargs {
            if let Some(field) = k
                && self.changeable(*v) == Changeable::Maybe
            {
                out.bind(ANY_CLASS, field, Changeable::Maybe);
            }
        }
    }

    /// Whether the value `v` may be changed in place.
    fn changeable(&self, v: VarId) -> Changeable {
        let fixed = match self.types.ty(v) {
            Some(t @ (Ty::Inst(_) | Ty::Union(_))) => {
                (t.members().iter()).all(|o| ContainerFamily::rejects_changes(&o.class))
            }
            _ => false,
        };
        if fixed {
            Changeable::No
        } else {
            Changeable::Maybe
        }
    }

    /// A call of the class `class` (or of a subclass, through a class object), whose generated
    /// constructor binds the read-only container fields `fields`.
    fn constructed(
        &self,
        class: &str,
        fields: &[Box<str>],
        args: &[VarId],
        kwargs: &[(Option<String>, VarId)],
        star_from: Option<u8>,
        out: &mut FieldValues,
    ) {
        if fields.is_empty() {
            return;
        }
        let order = self
            .cx
            .project
            .constructor_fields(class)
            .unwrap_or_default();
        let positional: Vec<&str> = order
            .iter()
            .filter(|f| !f.kw_only)
            .map(|f| &*f.name)
            .collect();
        let unpacked = star_from.is_some() || kwargs.iter().any(|(k, _)| k.is_none());
        for field in fields {
            let by_keyword = kwargs
                .iter()
                .find_map(|(k, v)| (k.as_deref() == Some(&**field)).then_some(*v));
            let at = positional.iter().position(|p| *p == &**field);
            let value = match (by_keyword, at.and_then(|i| args.get(i))) {
                _ if unpacked => Changeable::Maybe,
                (Some(v), None) | (None, Some(&v)) => self.changeable(v),
                // A default, or a call raising `TypeError`.
                _ => Changeable::Maybe,
            };
            out.bind(class, field, value);
        }
    }

    /// A copy of an instance of a project class with tracked fields, which makes an instance of
    /// the original's class holding the replaced fields' new values.
    fn replaced(
        &self,
        r: &Replaced<'_>,
        tracked: &mut FxHashMap<String, Vec<Box<str>>>,
        out: &mut FieldValues,
    ) {
        let unpacked = r.by_position || r.kwargs.iter().any(|(k, _)| k.is_none());
        let Some(original) = self
            .types
            .ty(r.original)
            .filter(|t| !t.members().is_empty())
        else {
            // A copy of an object of no class heki knows may be one of any class: what it
            // binds may change the field of that name of any class.
            if unpacked {
                out.bind(ANY_CLASS, ANY_FIELD, Changeable::Maybe);
            }
            for (k, v) in r.kwargs {
                if let Some(field) = k
                    && self.changeable(*v) == Changeable::Maybe
                {
                    out.bind(ANY_CLASS, field, Changeable::Maybe);
                }
            }
            return;
        };
        for o in original.members() {
            let class = self.cx.project.canonical(&o.class);
            for field in tracked_fields(self.cx.project, &class, tracked) {
                let named = r
                    .kwargs
                    .iter()
                    .find_map(|(k, v)| (k.as_deref() == Some(&**field)).then_some(*v));
                let value = match named {
                    _ if unpacked => Changeable::Maybe,
                    Some(v) => self.changeable(v),
                    // The copy holds the original's value.
                    None => continue,
                };
                out.bind(&class, field, value);
            }
        }
    }
}

impl ProjectDecls {
    /// Whether the field `attr` of an instance of the project class `class` holds an object
    /// that rejects every in-place change (`MUT108`): `class` and each of its subclasses hold in
    /// it what their constructions bind ([`Self::field_holds_given`]), some module makes an
    /// instance of one of them, and every value any module binds there
    /// ([`ModuleDecls::field_values`], also through a copy or a class object of a base class)
    /// rejects the changes ([`Changeable::No`]).
    pub fn field_rejects_changes(&self, class: &str, attr: &str) -> bool {
        let class = self.canonical(class);
        let mut family = self.subclasses_of(&class);
        family.push(class.clone());
        if !family.iter().all(|c| self.field_holds_given(c, attr)) {
            return false;
        }
        let bound = |c: &str| {
            self.read(|k| note(&mut k.class_facts, &field_key(c, attr)));
            self.field_values
                .get(&(c.to_owned(), attr.to_owned()))
                .copied()
        };
        let mut made = false;
        for c in &family {
            match bound(c) {
                Some(Changeable::Maybe) => return false,
                Some(Changeable::No) => made = true,
                None => {}
            }
        }
        let bases = self.ancestors_of(&class);
        let any = || {
            self.read(|k| note(&mut k.class_facts, &field_key(ANY_CLASS, attr)));
            self.read(|k| note(&mut k.class_facts, &field_key(ANY_CLASS, ANY_FIELD)));
            let by = |field: &str| {
                (self.field_values).contains_key(&(ANY_CLASS.to_owned(), field.to_owned()))
            };
            by(attr) || by(ANY_FIELD)
        };
        made && bases.iter().all(|c| bound(c) != Some(Changeable::Maybe)) && !any()
    }

    /// [`Self::field_values`]: what every module binds to each read-only container field.
    pub(super) fn joined_field_values(&self) -> FxHashMap<(String, String), Changeable> {
        let mut out: FxHashMap<(String, String), Changeable> = FxHashMap::default();
        for m in self.modules.values() {
            for (class, fields) in m.field_values.iter().flat_map(|v| &v.bound) {
                for (field, value) in fields {
                    let key = (self.canonical(class), field.to_string());
                    let joined = out.entry(key).or_insert(*value);
                    *joined = (*joined).max(*value);
                }
            }
        }
        out
    }
}

/// The class a copy of an object of unknown class binds fields of ([`FieldValues::bound`]): any.
const ANY_CLASS: &str = "*";

/// The field such a copy binds when it unpacks a mapping of them or passes positions: any.
const ANY_FIELD: &str = "*";

/// The key of the declarations' read log ([`super::DeclKeys::class_facts`]) an answer of
/// [`ProjectDecls::field_rejects_changes`] notes for what the modules bind to the field `attr`
/// of the class `class`.
pub(super) fn field_key(class: &str, attr: &str) -> String {
    format!("{class}.{attr}")
}
