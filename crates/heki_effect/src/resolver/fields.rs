//! The fields the constructor of a `typing.NamedTuple` or of a `@dataclass(frozen=True)` binds
//! ([`super::ClassDecl::init_fields`], [`ProjectDecls::constructor_fields`]), and what an
//! instance such a call makes holds in them ([`crate::types::Args::Fields`]): the argument of
//! each field's position or keyword, or its default. Storing a field of such an instance raises
//! (`AttributeError` on a named tuple, `FrozenInstanceError` on a frozen dataclass), so it holds
//! that value while it lives, unless code stores it through `object.__setattr__`, which the
//! stores of its class show ([`ProjectDecls::field_holds_given`]).

use super::sentinels::{field_default, init_left_on, named_value};
use super::{
    ModuleDecls, NAMED_TUPLE_BASES, PLAIN_BASES, ProjectDecls, dataclass_decorator,
    decorator_parts, keyword_bool, note, qualifiers, unquote,
};
use crate::ModuleInput;
use heki_ir::walk::FnShape;
use heki_ir::{ConstLiteral, GlobalStores, InstKind, IrFunction, VarId};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

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
