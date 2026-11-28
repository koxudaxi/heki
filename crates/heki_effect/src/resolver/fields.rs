//! The fields the constructor of a `typing.NamedTuple` or of a `@dataclass(frozen=True)` binds
//! ([`super::ClassDecl::init_fields`], [`ProjectDecls::constructor_fields`]), and what an
//! instance such a call makes holds in them ([`crate::types::Args::Fields`]): the argument of
//! each field's position or keyword, or its default. Storing a field of such an instance raises
//! (`AttributeError` on a named tuple, `FrozenInstanceError` on a frozen dataclass), so it holds
//! that value while it lives, unless code stores it through `object.__setattr__`, which the
//! stores of its class show ([`ProjectDecls::field_holds_given`]).

use super::sentinels::{field_default, init_left_on, named_value};
use super::{
    CHA_CANDIDATE_LIMIT, FunctionDecl, ModuleDecls, NAMED_TUPLE_BASES, PLAIN_BASES, ProjectDecls,
    ReadOnlyKind, TypeCtx, dataclass_decorator, decorator_parts, keyword_bool, note, qualifiers,
    unquote,
};
use crate::ModuleInput;
use crate::infer::Inferred;
use crate::readonly::ContainerFamily;
use crate::types::Ty;
use heki_ir::walk::FnShape;
use heki_ir::{ConstLiteral, GlobalStores, InstKind, IrFunction, VarId};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

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
    init_params(input, decls);
}

/// [`super::ClassDecl::init_params`] of the module's classes: each store through `self` in a
/// method of the class, by attribute; an attribute counts when its one such store is in
/// `__init__` and binds a parameter's value as passed.
fn init_params(input: &ModuleInput<'_>, decls: &mut ModuleDecls) {
    let mut found: BTreeMap<&str, BTreeMap<&str, Option<u8>>> = BTreeMap::new();
    let methods = (input.ir.functions.iter())
        .filter(|f| !f.is_class_body && !f.is_module_body)
        .filter(|f| !f.has_decorator(&["staticmethod", "classmethod"]));
    for f in methods {
        let (Some(class), Some(receiver)) = (f.owner_class_qualname.as_deref(), f.params.first())
        else {
            continue;
        };
        let shape = FnShape::new(f);
        let receivers = super::self_aliases(f, receiver.var);
        let stores = found.entry(class).or_default();
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            let InstKind::AttrStore { obj, attr, value } = &inst.kind else {
                continue;
            };
            if !receivers.contains(obj) {
                continue;
            }
            let root = shape.copy_root(*value);
            let param = (f.name == "__init__")
                .then(|| (f.params.iter().skip(1)).position(|p| p.var == root))
                .flatten()
                .filter(|&i| !f.params[i + 1].is_vararg && !f.params[i + 1].is_kwarg);
            let at = param.and_then(|i| u8::try_from(i + 1).ok());
            match stores.entry(attr.as_str()) {
                Entry::Vacant(e) => {
                    e.insert(at);
                }
                Entry::Occupied(mut e) => {
                    e.insert(None);
                }
            }
        }
    }
    for (class, stores) in found {
        let params: Box<[(Box<str>, u8)]> = (stores.into_iter())
            .filter_map(|(attr, at)| Some((Box::from(attr), at?)))
            .collect();
        if let Some(c) = decls.classes.get_mut(class)
            && !params.is_empty()
        {
            c.init_params = params;
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

/// What a module's calls bind to the fields of the instances they make
/// ([`super::ModuleDecls::field_values`]), by the class the call makes (or may make a subclass
/// of), then the field. A copy of an object of unknown class, or a call of a callee of unknown
/// value, binds under [`ANY_CLASS`] the fields it names, or [`ANY_FIELD`] when it unpacks them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldValues {
    /// Of a read-only container field: whether a value bound there may be changed in place.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bound: BTreeMap<String, BTreeMap<Box<str>, Changeable>>,
    /// Of an attribute of a private class that its constructor binds to an argument
    /// ([`HeldParam`]): the objects bound there.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub held: BTreeMap<String, BTreeMap<Box<str>, BoundObjects>>,
}

/// The objects the calls of the project bind to one attribute, as far as calling it runs them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoundObjects {
    /// The named objects (functions, classes) by fqn; `None`, which a call cannot run, adds
    /// none.
    Named(BTreeSet<Box<str>>),
    /// A value of no named object, or one heki does not see.
    Open,
}

impl BoundObjects {
    fn join(&mut self, other: BoundObjects) {
        match (&mut *self, other) {
            (BoundObjects::Open, _) => {}
            (_, BoundObjects::Open) => *self = BoundObjects::Open,
            (BoundObjects::Named(names), BoundObjects::Named(more)) => names.extend(more),
        }
    }
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

    fn hold(&mut self, class: &str, attr: &str, value: BoundObjects) {
        let attrs = self.held.entry(class.to_owned()).or_default();
        match attrs.entry(attr.into()) {
            Entry::Vacant(e) => {
                e.insert(value);
            }
            Entry::Occupied(mut e) => e.get_mut().join(value),
        }
    }

    fn is_empty(&self) -> bool {
        self.bound.is_empty() && self.held.is_empty()
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
    let mut tracked: FxHashMap<String, Tracked> = FxHashMap::default();
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
    (!out.is_empty()).then(|| Box::new(out))
}

/// Whether `f` may make an instance [`field_values`] records: it calls a name the module binds to
/// a class with tracked fields ([`tracked_fields`]) or to a copy the catalog lists
/// (`dataclasses.replace`), a name the module does not bind (a parameter such as `cls`, a local
/// alias, a local import), or a `_replace` method.
fn may_make_instances(
    f: &IrFunction,
    cx: &TypeCtx<'_>,
    tracked: &mut FxHashMap<String, Tracked>,
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
            // `C.__init__(obj, ...)` binds what a construction of `C` binds.
            InstKind::MethodCall { recv, method, .. } => {
                matches!(method.as_str(), "_replace" | "__init__")
                    || may_name_instances(&defs, *recv, method, cx, tracked)
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
    tracked: &mut FxHashMap<String, Tracked>,
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

/// What [`field_values`] records of the calls making instances of one project class.
#[derive(Default)]
struct Tracked {
    /// The read-only container fields ([`ProjectDecls::readonly_attr`]: of a frozen class, or
    /// declared `Final` / `ReadOnly`) the generated constructor binds.
    readonly: Vec<Box<str>>,
    /// The parameters binding attributes of a private class ([`held_params`]).
    held: Vec<HeldParam>,
}

impl Tracked {
    fn is_empty(&self) -> bool {
        self.readonly.is_empty() && self.held.is_empty()
    }
}

/// [`Tracked`] of the project class `class`, kept by class.
fn tracked_fields<'t>(
    project: &ProjectDecls,
    class: &str,
    tracked: &'t mut FxHashMap<String, Tracked>,
) -> &'t Tracked {
    tracked.entry(class.to_owned()).or_insert_with(|| {
        let readonly = (project.constructor_fields(class).unwrap_or_default().iter())
            .filter(|f| {
                project.readonly_attr(class, &f.name).is_some_and(|ro| {
                    ro.holds_container
                        && matches!(ro.kind, ReadOnlyKind::Frozen | ReadOnlyKind::Declared)
                })
            })
            .map(|f| f.name.clone())
            .collect();
        Tracked {
            readonly,
            held: held_params(project, class),
        }
    })
}

/// A parameter of the constructor of a private project class that binds an attribute of the
/// new instance to the argument as passed: a field of a generated constructor, or a parameter
/// `__init__` stores ([`super::ClassDecl::init_params`]).
struct HeldParam {
    attr: Box<str>,
    /// The keyword binding it, if a keyword can.
    keyword: Option<Box<str>>,
    /// The position binding it, the receiver not counted, if a position can.
    position: Option<usize>,
    /// What the attribute holds when the call binds nothing to the parameter.
    unbound: BoundObjects,
}

/// The [`HeldParam`]s of the private project class `class`.
fn held_params(project: &ProjectDecls, class: &str) -> Vec<HeldParam> {
    if !is_private_class(class) {
        return Vec::new();
    }
    if let Some(fields) = project.constructor_fields(class) {
        let positional = fields.iter().filter(|f| !f.kw_only).count();
        return (fields.iter().enumerate())
            .map(|(i, f)| HeldParam {
                attr: f.name.clone(),
                keyword: Some(f.name.clone()),
                position: (i < positional).then_some(i),
                unbound: unbound_field(project, &f.default),
            })
            .collect();
    }
    let Some((_, decl, init)) = project.constructor_init(class) else {
        return Vec::new();
    };
    (decl.init_params.iter())
        .filter_map(|(attr, at)| {
            let i = usize::from(*at);
            let name = init.param_names.get(i)?;
            let positional = i < usize::from(init.positional_params);
            let required = if positional {
                i < usize::from(init.required_positional)
            } else {
                let bit = i - usize::from(init.positional_params);
                bit < 8 && init.required_keyword_only >> bit & 1 == 1
            };
            Some(HeldParam {
                attr: attr.clone(),
                keyword: (i >= usize::from(init.positional_only)).then(|| name.as_str().into()),
                position: positional.then(|| i - 1),
                // A required parameter left unbound raises `TypeError`; a default may be any
                // value.
                unbound: if required {
                    BoundObjects::Named(BTreeSet::new())
                } else {
                    BoundObjects::Open
                },
            })
        })
        .collect()
}

/// What a field defaulting to `default` holds when a call binds nothing to it.
fn unbound_field(project: &ProjectDecls, default: &FieldDefault) -> BoundObjects {
    match default {
        // The call raises `TypeError`, or the field holds `None`.
        FieldDefault::Required | FieldDefault::None => BoundObjects::Named(BTreeSet::new()),
        FieldDefault::Named(name) => {
            BoundObjects::Named(BTreeSet::from([project.object_named(name).into()]))
        }
        FieldDefault::Value | FieldDefault::Other => BoundObjects::Open,
    }
}

/// Whether code outside the checked files cannot name the class `class` (R5): its name, or a
/// module along its dotted name, is private.
pub(crate) fn is_private_class(class: &str) -> bool {
    class.split('.').any(crate::callgraph::is_private_name)
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
        tracked: &mut FxHashMap<String, Tracked>,
        out: &mut FieldValues,
    ) {
        let project = self.cx.project;
        if let Target::Named(fqn) = &target
            && let Some(class) = fqn.strip_suffix(".__init__")
            && project.is_project_class(class)
        {
            // `C.__init__(obj, ...)` stores into an object no construction of `C` makes here.
            for p in &tracked_fields(project, class, tracked).held {
                out.hold(class, &p.attr, BoundObjects::Open);
            }
            return;
        }
        match target {
            Target::Named(fqn) | Target::Class(fqn) if project.is_project_class(&fqn) => {
                let fields = tracked_fields(project, &fqn, tracked);
                self.constructed(&fqn, &fields.readonly, args, kwargs, star_from, out);
                self.held_bound(&fqn, &fields.held, args, kwargs, star_from, out);
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
            out.hold(ANY_CLASS, ANY_FIELD, BoundObjects::Open);
        }
        for (k, v) in kwargs {
            let Some(field) = k else { continue };
            if self.changeable(*v) == Changeable::Maybe {
                out.bind(ANY_CLASS, field, Changeable::Maybe);
            }
            out.hold(ANY_CLASS, field, self.held(*v));
        }
    }

    /// What the value `v` holds for a call of it: the named objects its type gives, or nothing
    /// heki can name.
    fn held(&self, v: VarId) -> BoundObjects {
        let names: Vec<&String> = match self.types.ty(v) {
            Some(Ty::Ref(fqn)) => vec![fqn],
            Some(Ty::OneOf(fqns)) => fqns.iter().collect(),
            _ => return BoundObjects::Open,
        };
        let project = self.cx.project;
        BoundObjects::Named(
            names
                .into_iter()
                .map(|f| project.canonical(f).into())
                .collect(),
        )
    }

    /// A call of the private class `class` (or of a subclass, through a class object), whose
    /// constructor binds the attributes `params` name to its arguments.
    fn held_bound(
        &self,
        class: &str,
        params: &[HeldParam],
        args: &[VarId],
        kwargs: &[(Option<String>, VarId)],
        star_from: Option<u8>,
        out: &mut FieldValues,
    ) {
        let unpacked = star_from.is_some() || kwargs.iter().any(|(k, _)| k.is_none());
        for p in params {
            let by_keyword = (p.keyword.as_deref()).and_then(|name| {
                (kwargs.iter()).find_map(|(k, v)| (k.as_deref() == Some(name)).then_some(*v))
            });
            let at = p.position.and_then(|i| args.get(i));
            let held = match (by_keyword, at) {
                _ if unpacked => BoundObjects::Open,
                (Some(v), None) | (None, Some(&v)) => self.held(v),
                (None, None) => p.unbound.clone(),
                // Bound twice: the call raises `TypeError`.
                (Some(_), Some(_)) => continue,
            };
            out.hold(class, &p.attr, held);
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
        tracked: &mut FxHashMap<String, Tracked>,
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
                out.hold(ANY_CLASS, ANY_FIELD, BoundObjects::Open);
            }
            for (k, v) in r.kwargs {
                let Some(field) = k else { continue };
                if self.changeable(*v) == Changeable::Maybe {
                    out.bind(ANY_CLASS, field, Changeable::Maybe);
                }
                out.hold(ANY_CLASS, field, self.held(*v));
            }
            return;
        };
        for o in original.members() {
            let class = self.cx.project.canonical(&o.class);
            let fields = tracked_fields(self.cx.project, &class, tracked);
            for p in fields.held.iter().filter(|p| p.keyword.is_some()) {
                let named = (r.kwargs.iter())
                    .find_map(|(k, v)| (k.as_deref() == p.keyword.as_deref()).then_some(*v));
                match named {
                    _ if unpacked => out.hold(&class, &p.attr, BoundObjects::Open),
                    Some(v) => out.hold(&class, &p.attr, self.held(v)),
                    // The copy holds the original's value.
                    None => {}
                }
            }
            for field in &fields.readonly {
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

    /// [`Self::field_values`] and [`Self::held_values`]: what every module binds to each
    /// read-only container field, and to each attribute of a private class its constructor
    /// binds.
    pub(super) fn joined_field_values(&self) -> (JoinedBound, JoinedHeld) {
        let mut out: JoinedBound = FxHashMap::default();
        let mut held: JoinedHeld = FxHashMap::default();
        for v in self
            .modules
            .values()
            .filter_map(|m| m.field_values.as_deref())
        {
            for (class, fields) in &v.bound {
                for (field, value) in fields {
                    let key = (self.canonical(class), field.to_string());
                    let joined = out.entry(key).or_insert(*value);
                    *joined = (*joined).max(*value);
                }
            }
            for (class, attrs) in &v.held {
                for (attr, value) in attrs {
                    let key = (self.canonical(class), attr.to_string());
                    match held.entry(key) {
                        std::collections::hash_map::Entry::Vacant(e) => {
                            e.insert(value.clone());
                        }
                        std::collections::hash_map::Entry::Occupied(mut e) => {
                            e.get_mut().join(value.clone());
                        }
                    }
                }
            }
        }
        (out, held)
    }

    /// What the attribute `attr` of an instance of the project class `class` holds where the
    /// call constructing the instance is not in view (a parameter, `self`): the join of what
    /// every construction of the project binds there ([`FieldValues::held`]), which are all of
    /// them when the class is private under R5 and each class of its family binds the attribute
    /// from its constructor and nowhere else ([`Self::binds_from_constructor`]). Copies with
    /// the field replaced and calls through a class object of a base class join in, as does
    /// what a callee of unknown value is passed under the attribute's name. `None` when a value
    /// is no named object or heki does not see it, or no construction binds one; too many past
    /// [`CHA_CANDIDATE_LIMIT`] objects.
    pub(crate) fn constructed_value(&self, class: &str, attr: &str) -> Option<ConstructedValue> {
        let class = self.canonical(class);
        if !is_private_class(&class) {
            return None;
        }
        let key = (class.clone(), attr.to_owned());
        self.kept(&self.constructed_values, key, || {
            let mut family = self.subclasses_of(&class);
            family.push(class.clone());
            if !family.iter().all(|c| self.binds_from_constructor(c, attr)) {
                return None;
            }
            let bases = self.ancestors_of(&class);
            let mut named: BTreeSet<Box<str>> = BTreeSet::new();
            let mut made = false;
            let classes = family.iter().chain(&bases).map(|c| (c.as_str(), attr));
            let unknown = [(ANY_CLASS, attr), (ANY_CLASS, ANY_FIELD)];
            for (i, (c, a)) in classes.chain(unknown).enumerate() {
                self.read(|k| note(&mut k.class_facts, &held_key(c, a)));
                match self.held_values.get(&(c.to_owned(), a.to_owned())) {
                    Some(BoundObjects::Open) => return None,
                    Some(BoundObjects::Named(names)) => {
                        made |= i < family.len() + bases.len();
                        named.extend(names.iter().cloned());
                    }
                    None => {}
                }
            }
            if !made || named.is_empty() {
                return None;
            }
            if named.len() > CHA_CANDIDATE_LIMIT {
                return Some(ConstructedValue::TooMany);
            }
            let mut names: Vec<String> = named.into_iter().map(String::from).collect();
            let value = match names.len() {
                1 => Ty::Ref(names.pop()?),
                _ => Ty::OneOf(names),
            };
            Some(ConstructedValue::Of(value))
        })
    }

    /// Whether an instance of exactly the class `class` holds in `attr` what its construction
    /// bound: a field of a generated constructor that keeps it ([`Self::field_holds_given`]),
    /// or an attribute the `__init__` it runs stores a parameter in
    /// ([`super::ClassDecl::init_params`]) with no other store ([`Self::only_init_stores`]).
    fn binds_from_constructor(&self, class: &str, attr: &str) -> bool {
        if self.field_holds_given(class, attr) {
            return true;
        }
        let Some((owner, decl, _)) = self.constructor_init(class) else {
            return false;
        };
        let stored = decl.init_params.iter().any(|(a, _)| &**a == attr);
        stored && self.only_init_stores(class, &owner, attr)
    }

    /// The project class whose `__init__` constructing an instance of exactly `class` runs, its
    /// declaration, and that method: the first along the MRO defining one, past no `__new__`,
    /// metaclass, or base outside the project (`object` among them), which may make the
    /// instance otherwise.
    pub(super) fn constructor_init(
        &self,
        class: &str,
    ) -> Option<(String, &super::ClassDecl, &FunctionDecl)> {
        let makes = |d: &super::ClassDecl| d.custom_metaclass || d.members.contains("__new__");
        let mro = self.mro(class);
        let c = mro.iter().find(|c| {
            self.read(|k| note(&mut k.class_facts, c));
            (self.class(c)).is_none_or(|d| makes(d) || d.members.contains("__init__"))
        })?;
        let d = self.class(c).filter(|d| !makes(d))?;
        let key = self.own_method(c, "__init__")?;
        Some((c.clone(), d, self.function_decl(&key)?))
    }

    /// Whether the only store of `attr` into an instance of exactly `class` is the one the
    /// `__init__` of `owner` makes through `self`: no property or attribute hook of the name;
    /// no class whose code may store into the instance ([`Self::instance_family`]) defines a
    /// method of the name or binds it to a name in its body, stores it otherwise than through
    /// `self` in its own methods
    /// ([`super::ClassDecl::unowned_attrs`]), or, past `owner`, stores or annotates it at all;
    /// no other module stores it through a receiver of such a class; and no store of it, or of
    /// a name heki cannot read, goes through a receiver of no class heki knows.
    fn only_init_stores(&self, class: &str, owner: &str, attr: &str) -> bool {
        self.read(|k| note(&mut k.property_names, attr));
        if self.property_names.contains(attr)
            || self.attr_stored_untyped(attr)
            || self.attr_read(class, attr).runs_hook(false)
        {
            return false;
        }
        self.instance_family(class).iter().all(|c| {
            self.read(|k| note(&mut k.class_facts, c));
            // A base outside the project may store the attribute itself.
            self.class(c).is_some_and(|d| {
                let unowned = (d.unowned_attrs.binary_search_by(|a| (**a).cmp(attr))).is_ok();
                let stored_here = c != owner
                    && (self.attr_store(c, d, attr).is_some() || d.attr_types.contains_key(attr));
                !unowned
                    && !stored_here
                    && self.own_method(c, attr).is_none()
                    && self.class_aliases(c, attr).is_none()
                    && !self.attr_stored_elsewhere(c, attr)
                    && !self.stores_any_attr(c, d)
                    && !self.rebound_elsewhere(&format!("{c}.{attr}"))
            })
        })
    }
}

/// [`ProjectDecls::field_values`], by class and field.
pub(super) type JoinedBound = FxHashMap<(String, String), Changeable>;

/// [`ProjectDecls::held_values`], by class and attribute.
pub(super) type JoinedHeld = FxHashMap<(String, String), BoundObjects>;

/// What [`ProjectDecls::constructed_value`] finds.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ConstructedValue {
    /// The named objects every construction binds: one name, or [`Ty::OneOf`].
    Of(Ty),
    /// More objects than a call through the attribute is resolved to.
    TooMany,
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

/// The key an answer of [`ProjectDecls::constructed_value`] notes for what the modules'
/// constructions bind to the attribute `attr` of the class `class`: after a `=`, which no fqn
/// starts with.
pub(super) fn held_key(class: &str, attr: &str) -> String {
    format!("={class}.{attr}")
}
