//! The `object()` sentinels of a module, the attributes its classes bind to one, and the classes
//! it registers with `copyreg`: what `copy.deepcopy` does to an instance holding a sentinel
//! ([`super::ProjectDecls::held_sentinels`], [`super::ProjectDecls::copy_of`]).

use super::{
    ModuleDecls, NamedAttr, PartialAttr, dataclass_decorator, decorator_parts,
    is_summarized_function, qualifiers, split_top_level,
};
use crate::ModuleInput;

use crate::types::MAX_NAMED;
use heki_ast::Span;
use heki_ir::walk::FnShape;
use heki_ir::{
    BlockId, FunctionId, GlobalStores, InstKind, IrFunction, LastBindings, ModuleIr, VarId,
};
use heki_semantic::{NameResolution, ScopeKind};
use rustc_hash::FxHashMap;
use std::collections::BTreeSet;

/// [`ModuleDecls::object_sentinels`]: the module body's names stored once, outside a loop, from a
/// call of the builtin `object` with no argument (through copies). Only such an object is one
/// that `copy.deepcopy` replaces: a PEP 661 `sentinel(...)`, like any other value, is none.
pub(super) fn object_sentinels(input: &ModuleInput<'_>, decls: &ModuleDecls) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if input.ir.constants.dynamic || input.ir.has_star_import || !builtin_object(input, decls) {
        return out;
    }
    let module_stores = GlobalStores::of(&input.ir.functions);
    object_calls(input.ir, |name, _| {
        if module_stores.once(name) && !decls.imports.contains_key(name) {
            out.insert(name.to_owned());
        }
    });
    out
}

/// The `object()` calls the module body stores in globals outside a loop, by name, that a call
/// of the builtin `sentinel` (PEP 661) can replace: none when the module binds a global named
/// `sentinel`, which such a call would read instead. The module's
/// [`ModuleDecls::object_sentinels`] are among the names.
pub fn object_sentinel_calls(ir: &ModuleIr) -> FxHashMap<String, Span> {
    let mut out = FxHashMap::default();
    if !GlobalStores::of(&ir.functions).binds("sentinel") {
        object_calls(ir, |name, call| {
            out.insert(name.to_owned(), call);
        });
    }
    out
}

/// Each store, outside a loop of the module body, of the result of a call `object()` in a global:
/// the name and the call (through copies). Whether `object` is the builtin is for the caller.
fn object_calls(ir: &ModuleIr, mut each: impl FnMut(&str, Span)) {
    for f in ir.functions.iter().filter(|f| f.is_module_body) {
        let shape = FnShape::new(f);
        for b in f.blocks.iter().filter(|b| shape.loop_depth(b.id) == 0) {
            for inst in &b.insts {
                let InstKind::StoreGlobal { name, value } = &inst.kind else {
                    continue;
                };
                if let Some(call) = object_call(&shape, *value) {
                    each(name, call);
                }
            }
        }
    }
}

/// Whether the module reads `object` as the builtin: its module scope reads it so, and the module
/// binds no global of that name.
fn builtin_object(input: &ModuleInput<'_>, decls: &ModuleDecls) -> bool {
    let read_as_builtin = input
        .semantic
        .scopes
        .iter()
        .filter(|sc| sc.kind == ScopeKind::Module)
        .flat_map(|sc| &sc.reads)
        .any(|(name, how)| name == "object" && *how == NameResolution::Builtin);
    read_as_builtin
        && !decls.imports.contains_key("object")
        && !decls.classes.contains_key("object")
        && !decls.functions.contains_key("object")
        && !decls.unknown_globals.contains("object")
}

/// The call `object()` whose result `value` is: a call, with no argument, of the global name
/// `object`.
fn object_call(shape: &FnShape<'_>, value: VarId) -> Option<Span> {
    let call = shape.inst(shape.copy_root(value))?;
    let InstKind::Call {
        callee_var: Some(callee),
        args,
        kwargs,
        star_from: None,
        ..
    } = &call.kind
    else {
        return None;
    };
    let of_object = matches!(
        shape.inst(shape.copy_root(*callee)).map(|i| &i.kind),
        Some(InstKind::LoadGlobal { name, .. }) if name == "object"
    );
    (args.is_empty() && kwargs.is_empty() && of_object).then_some(call.span)
}

/// The fqn of the module-level name that may be an `object()` sentinel which `value` holds: one
/// of the module's own sentinels, a name an import binds (and nothing in the module binds again),
/// or an attribute of such a name (`sentinels.MISSING`). Whether it is a sentinel is for its own
/// module to say. A class or function the module binds again (`stores`) is none: what the name
/// holds depends on when it is read.
pub(super) fn named_value(
    shape: &FnShape<'_>,
    value: VarId,
    decls: &ModuleDecls,
    stores: &GlobalStores<'_>,
) -> Option<String> {
    let global = |var: VarId| match shape.inst(shape.copy_root(var)).map(|i| &i.kind) {
        Some(InstKind::LoadGlobal { name, .. }) => Some(name.as_str()),
        _ => None,
    };
    if let Some(name) = global(value) {
        let own = if decls.classes.contains_key(name) || decls.functions.contains_key(name) {
            stores.once(name)
        } else {
            decls.object_sentinels.contains(name) || decls.aliases.contains_key(name)
        };
        if own && !decls.imports.contains_key(name) {
            return Some(format!("{}.{name}", decls.module_name));
        }
        return decls.fixed_import(name).cloned();
    }
    match shape.inst(shape.copy_root(value)).map(|i| &i.kind) {
        Some(InstKind::AttrLoad { obj, attr, .. }) => {
            Some(format!("{}.{attr}", decls.fixed_import(global(*obj)?)?))
        }
        _ => None,
    }
}

/// [`super::ClassDecl::named_attrs`] of the module's classes: what their bodies bind and
/// annotate, and what their methods store on `self` (a name, or a parameter whose default is
/// one), among the names that may be sentinels.
pub(super) fn named_attrs(input: &ModuleInput<'_>, decls: &mut ModuleDecls) {
    let mut found: Vec<(String, String, NamedAttr)> = Vec::new();
    let mut defaults: FxHashMap<FunctionId, Vec<(VarId, String)>> = FxHashMap::default();
    let constants = &input.ir.constants;
    let staticmethod_trusted = !constants.dynamic && !constants.shadowed.contains("staticmethod");
    let stores = GlobalStores::of(&input.ir.functions);
    for body in input.ir.functions.iter().filter(|f| f.is_class_body) {
        class_body_names(body, decls, &stores, staticmethod_trusted, &mut found);
        method_defaults(input, body, decls, &stores, &mut defaults);
    }
    // The methods of the module's classes that run on an instance, with their receiver.
    let methods = input.ir.functions.iter().filter_map(|f| {
        let owner = f.owner_class_qualname.as_ref()?;
        let receiver = f.params.first().filter(|p| !p.is_vararg && !p.kw_only)?;
        let bound = !f.is_class_body
            && is_summarized_function(f)
            && !f.has_decorator(&["staticmethod", "classmethod"]);
        bound.then_some((f, owner, receiver.var))
    });
    for (f, owner, receiver) in methods {
        let shape = FnShape::new(f);
        let defaulted = defaults.get(&f.id).map_or(&[][..], Vec::as_slice);
        let attr_stores = f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|i| match &i.kind {
                InstKind::AttrStore { obj, attr, value } if shape.copy_root(*obj) == receiver => {
                    Some((attr, *value))
                }
                _ => None,
            });
        for (attr, value) in attr_stores {
            let root = shape.copy_root(value);
            let name = named_value(&shape, value, decls, &stores).or_else(|| {
                defaulted
                    .iter()
                    .find_map(|(param, name)| (*param == root).then(|| name.clone()))
            });
            if let Some(name) = name {
                let held = NamedAttr {
                    held: BTreeSet::from([name]),
                    ..NamedAttr::default()
                };
                found.push((owner.clone(), attr.clone(), held));
            }
        }
    }
    // The semantic model declares every class the IR has (one AST).
    for (class, attr, named) in found {
        if let Some(c) = decls.classes.get_mut(&class) {
            c.named_attrs.entry(attr).or_default().join(named);
        }
    }
}

impl NamedAttr {
    /// Add what `other`, found after it for the same attribute, says: the class body's binding
    /// found first stays, and the flags and the names held add up.
    fn join(&mut self, other: NamedAttr) {
        if self.body.is_none() && other.body.is_some() {
            self.body_static = other.body_static;
        }
        self.body = self.body.take().or(other.body);
        if self.body_paths.is_empty() {
            self.body_paths = other.body_paths;
            self.body_defined = other.body_defined;
        }
        self.body_partial = self.body_partial.take().or(other.body_partial);
        self.annotated |= other.annotated;
        self.validates_default |= other.validates_default;
        self.held.extend(other.held);
    }
}

/// The parameters of the methods a class body defines whose default is a name that may be a
/// sentinel (`def __init__(self, default=MISSING)`), by method. A `def` in a class body hands
/// its function object the defaults first, in the order of the parameters that have one (a
/// class body has no cells of its own to hand over before them).
fn method_defaults(
    input: &ModuleInput<'_>,
    body: &IrFunction,
    decls: &ModuleDecls,
    stores: &GlobalStores<'_>,
    out: &mut FxHashMap<FunctionId, Vec<(VarId, String)>>,
) {
    let mut shape = None;
    for inst in body.blocks.iter().flat_map(|b| &b.insts) {
        let InstKind::MakeClosure {
            func_id: Some(id),
            captured,
            ..
        } = &inst.kind
        else {
            continue;
        };
        let method = input.ir.function(*id);
        let defaulted = method
            .params
            .iter()
            .filter(|p| p.has_default && !p.is_vararg && !p.is_kwarg);
        let shape = shape.get_or_insert_with(|| FnShape::new(body));
        let named: Vec<(VarId, String)> = defaulted
            .zip(captured)
            .filter(|(p, _)| p.default.is_none())
            .filter_map(|(p, value)| Some((p.var, named_value(shape, *value, decls, stores)?)))
            .collect();
        if !named.is_empty() {
            out.insert(*id, named);
        }
    }
}

/// How a class body binds a name ([`class_body_names`]).
#[derive(Clone, Copy)]
enum Binding {
    /// A named copy of the value (`name = value`).
    Copy(VarId),
    /// The class's own `def name(...)`.
    Def,
    /// Anything else: an import, an augmented assignment, an `except` clause, a nested class.
    Other,
}

/// The names one class body binds last to a name that may be a sentinel, with the defaults of
/// the fields that the `__init__` a stdlib `@dataclass` generates stores; and the names it binds
/// last to different names, or to its own `def`, on different paths (`def fetch(...)`, then
/// `if flag: fetch = _quiet`).
fn class_body_names(
    body: &IrFunction,
    decls: &ModuleDecls,
    stores: &GlobalStores<'_>,
    staticmethod_trusted: bool,
    found: &mut Vec<(String, String, NamedAttr)>,
) {
    let resolve = |name: &str| decls.resolve_dotted(name, true);
    let shape = FnShape::new(body);
    // Every binding of a name, by block and statement start: a named copy, or a value the
    // statement makes under the name. A read names its value too and binds nothing; `del name`
    // unbinds it.
    let mut bindings: FxHashMap<&str, Vec<(BlockId, u32, Binding)>> = FxHashMap::default();
    let mut deleted: Vec<&str> = Vec::new();
    for bb in &body.blocks {
        for inst in &bb.insts {
            let binding = match &inst.kind {
                InstKind::Delete { name, .. } => {
                    deleted.push(name);
                    continue;
                }
                InstKind::LoadGlobal { .. }
                | InstKind::LoadName { .. }
                | InstKind::LoadDeref { .. } => {
                    continue;
                }
                InstKind::Copy { src, .. } => Binding::Copy(*src),
                InstKind::MakeClosure { .. } => Binding::Def,
                _ => Binding::Other,
            };
            let Some(name) = inst
                .kind
                .dst()
                .and_then(|d| body.var_name(d))
                .filter(|n| !n.starts_with('.'))
            else {
                continue;
            };
            let binding = match binding {
                Binding::Def
                    if !decls
                        .functions
                        .contains_key(&format!("{}.{name}", body.qualname)) =>
                {
                    Binding::Other
                }
                b => b,
            };
            bindings
                .entry(name)
                .or_default()
                .push((bb.id, inst.span.start, binding));
        }
    }
    let dataclass = body
        .decorators
        .iter()
        .find(|d| dataclass_decorator(d, &resolve).is_some());
    // The `__init__` a stdlib `@dataclass` generates stores each field it takes: `init` left on,
    // and no `__init__` of the class's own, which it would keep.
    let generated_init = dataclass.is_some_and(|d| init_left_on(decorator_parts(d).1))
        && !bindings.contains_key("__init__")
        && !decls
            .functions
            .contains_key(&format!("{}.__init__", body.qualname));
    // Per annotated name: whether it declares a field, and whether it asks to validate the default.
    let fields: FxHashMap<&str, (bool, bool)> = body
        .local_annotations
        .iter()
        .filter(|a| !a.target.contains('.'))
        .map(|a| {
            let (q, _) = qualifiers(&a.annotation, &resolve);
            let field = !q.class_var && !q.pseudo_field;
            let validates = a.annotation.contains("validate_default");
            (a.target.as_str(), (field, validates))
        })
        .collect();
    let last = LastBindings::of(&shape);
    for (name, binds) in bindings {
        if deleted.contains(&name) {
            continue;
        }
        let at: Vec<(BlockId, u32)> = binds.iter().map(|&(b, start, _)| (b, start)).collect();
        // A name some paths leave unbound is what the bases give there.
        let (reaching, bound) = last.last_of(&shape, &at);
        if !bound {
            continue;
        }
        let (field, validates_default) = fields.get(name).copied().unwrap_or((false, false));
        let attr = match reaching[..] {
            [i] => {
                let Binding::Copy(value) = binds[i].2 else {
                    continue;
                };
                // `attr: T = field(default=NAME)`: a stdlib dataclass makes `NAME` the class
                // attribute, and `init=False` leaves the field out of `__init__`.
                let (named, init, body_static) = match field_default(&shape, value, &resolve) {
                    Some((default, init)) if dataclass.is_some() => {
                        (named_value(&shape, default, decls, stores), init, false)
                    }
                    Some(_) => continue,
                    None => match static_method_of(&shape, value, staticmethod_trusted) {
                        Some(wrapped) => (named_value(&shape, wrapped, decls, stores), true, true),
                        None => (named_value(&shape, value, decls, stores), true, false),
                    },
                };
                let Some(named) = named else {
                    // `copy = staticmethod(partial(copy_field, deep=False))`.
                    let (wrapped, is_static) =
                        match static_method_of(&shape, value, staticmethod_trusted) {
                            Some(wrapped) => (wrapped, true),
                            None => (value, false),
                        };
                    let Some(partial) = partial_of(&shape, wrapped, decls, stores, is_static)
                    else {
                        continue;
                    };
                    let attr = NamedAttr {
                        body_partial: Some(Box::new(partial)),
                        annotated: field,
                        validates_default,
                        ..NamedAttr::default()
                    };
                    found.push((body.qualname.clone(), name.to_owned(), attr));
                    continue;
                };
                let held = if field && init && generated_init {
                    BTreeSet::from([named.clone()])
                } else {
                    BTreeSet::new()
                };
                NamedAttr {
                    body: Some(named),
                    body_static,
                    held,
                    ..NamedAttr::default()
                }
            }
            _ => {
                let bound = reaching.iter().map(|&i| binds[i].2);
                let Some((paths, defined)) =
                    bound_on_paths(&shape, decls, stores, staticmethod_trusted, bound)
                else {
                    continue;
                };
                NamedAttr {
                    body_paths: paths,
                    body_defined: defined,
                    ..NamedAttr::default()
                }
            }
        };
        let attr = NamedAttr {
            annotated: field,
            validates_default,
            ..attr
        };
        found.push((body.qualname.clone(), name.to_owned(), attr));
    }
}

/// [`NamedAttr::body_paths`] and [`NamedAttr::body_defined`] of the bindings of one name that
/// may each be its last (`bound`): each binds a name, wrapped in `staticmethod` or not, or is
/// the class's own `def`. `None` when one binds anything else, or one name binds it on every
/// path.
fn bound_on_paths(
    shape: &FnShape<'_>,
    decls: &ModuleDecls,
    stores: &GlobalStores<'_>,
    staticmethod_trusted: bool,
    bound: impl Iterator<Item = Binding>,
) -> Option<(Vec<(String, bool)>, bool)> {
    let mut paths: Vec<(String, bool)> = Vec::new();
    let mut defined = false;
    for binding in bound {
        let value = match binding {
            Binding::Def => {
                defined = true;
                continue;
            }
            Binding::Copy(value) => value,
            Binding::Other => return None,
        };
        let (value, is_static) = match static_method_of(shape, value, staticmethod_trusted) {
            Some(wrapped) => (wrapped, true),
            None => (value, false),
        };
        let named = (named_value(shape, value, decls, stores)?, is_static);
        if !paths.contains(&named) {
            paths.push(named);
        }
    }
    let one_name = paths.len() == 1 && !defined;
    (!paths.is_empty() && !one_name && paths.len() + usize::from(defined) <= MAX_NAMED)
        .then_some((paths, defined))
}

/// The value `staticmethod(value)` wraps, when `value` holds such a call of the builtin
/// (`trusted`: the module does not rebind the name).
fn static_method_of(shape: &FnShape<'_>, value: VarId, trusted: bool) -> Option<VarId> {
    match shape.inst(shape.copy_root(value)).map(|i| &i.kind) {
        Some(InstKind::Call {
            callee,
            args,
            kwargs,
            star_from: None,
            ..
        }) if trusted && callee == "staticmethod" && kwargs.is_empty() && args.len() == 1 => {
            Some(args[0])
        }
        _ => None,
    }
}

/// The `functools.partial` object `value` holds, when a call of `functools.partial` (by an
/// import of it, or through `functools`) makes it of a name ([`named_value`]) and keywords only
/// (no positional argument after the callable, no `*` or `**` unpacking): a call through it
/// passes its own arguments first, as a call of the name would.
fn partial_of(
    shape: &FnShape<'_>,
    value: VarId,
    decls: &ModuleDecls,
    stores: &GlobalStores<'_>,
    is_static: bool,
) -> Option<PartialAttr> {
    let (partial, args, kwargs) = match shape.inst(shape.copy_root(value)).map(|i| &i.kind) {
        Some(InstKind::Call {
            callee_var: Some(callee),
            args,
            kwargs,
            star_from: None,
            ..
        }) => (named_value(shape, *callee, decls, stores)?, args, kwargs),
        // `functools.partial(...)` through the module.
        Some(InstKind::MethodCall {
            recv,
            method,
            args,
            kwargs,
            star_from: None,
            ..
        }) => {
            let module = match shape.inst(shape.copy_root(*recv)).map(|i| &i.kind) {
                Some(InstKind::LoadGlobal { name, .. }) => decls.fixed_import(name)?,
                _ => return None,
            };
            (format!("{module}.{method}"), args, kwargs)
        }
        _ => return None,
    };
    let [callable] = args[..] else {
        return None;
    };
    (partial == "functools.partial" && kwargs.iter().all(|(k, _)| k.is_some())).then_some(())?;
    Some(PartialAttr {
        callable: named_value(shape, callable, decls, stores)?,
        keywords: !kwargs.is_empty(),
        is_static,
    })
}

/// `(default, init)` of a field specifier `value` holds (`dataclasses.field(default=NAME)`):
/// the default's value, and whether `init` is left on.
pub(super) fn field_default(
    shape: &FnShape<'_>,
    value: VarId,
    resolve: &impl Fn(&str) -> Option<String>,
) -> Option<(VarId, bool)> {
    let Some(InstKind::Call { callee, kwargs, .. }) =
        shape.inst(shape.copy_root(value)).map(|i| &i.kind)
    else {
        return None;
    };
    if resolve(callee).as_deref() != Some("dataclasses.field") {
        return None;
    }
    let keyword = |key: &str| {
        kwargs
            .iter()
            .find_map(|(k, v)| (k.as_deref() == Some(key)).then_some(*v))
    };
    let init = keyword("init").is_none_or(|v| shape.bool_const(v) == Some(true));
    Some((keyword("default")?, init))
}

/// Whether the arguments of a `@dataclass(...)` decorator leave `init` on: no `init=` other than
/// `True`.
pub(super) fn init_left_on(args: &str) -> bool {
    split_top_level(args, ',')
        .iter()
        .all(|arg| match arg.split_once('=') {
            Some((k, v)) if k.trim() == "init" => v.trim() == "True",
            _ => true,
        })
}

/// [`ModuleDecls::copy_registrations`]: the classes `copyreg.pickle(C, ...)` and
/// `copyreg.dispatch_table[C] = ...` name in any function of the module.
pub(super) fn copy_registrations(input: &ModuleInput<'_>, decls: &ModuleDecls) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for f in &input.ir.functions {
        let mentions = f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .any(|i| match &i.kind {
                InstKind::MethodCall { method, .. } => method == "pickle",
                InstKind::Call { callee, .. } => callee.rsplit('.').next() == Some("pickle"),
                InstKind::AttrLoad { attr: name, .. } | InstKind::LoadGlobal { name, .. } => {
                    name == "dispatch_table"
                }
                _ => false,
            });
        if !mentions {
            continue;
        }
        let shape = FnShape::new(f);
        // A module body reads its own globals as the values it stored.
        let stored_as: FxHashMap<VarId, &str> = f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|i| match &i.kind {
                InstKind::StoreGlobal { name, value } => {
                    Some((shape.copy_root(*value), name.as_str()))
                }
                _ => None,
            })
            .collect();
        let dotted = |var: VarId| dotted_name(&shape, &stored_as, var, decls);
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            let class = match &inst.kind {
                InstKind::MethodCall {
                    recv, method, args, ..
                } if method == "pickle" && dotted(*recv).as_deref() == Some("copyreg") => {
                    args.first()
                }
                InstKind::Call {
                    callee_var: Some(callee),
                    args,
                    ..
                } if dotted(*callee).as_deref() == Some("copyreg.pickle") => args.first(),
                InstKind::SubscriptStore { obj, index, .. }
                    if dotted(*obj).as_deref() == Some("copyreg.dispatch_table") =>
                {
                    Some(index)
                }
                _ => continue,
            };
            out.insert(
                class
                    .and_then(|c| dotted(*c))
                    .unwrap_or_else(|| "*".to_owned()),
            );
        }
    }
    out
}

/// The fqn of the global name or attribute chain `var` holds (`copyreg.pickle`,
/// `models.Node`), resolved through the module's names; `stored_as` names the values the
/// function stores in globals.
fn dotted_name(
    shape: &FnShape<'_>,
    stored_as: &FxHashMap<VarId, &str>,
    var: VarId,
    decls: &ModuleDecls,
) -> Option<String> {
    let mut attrs = Vec::new();
    let mut cur = shape.copy_root(var);
    let head = loop {
        if let Some(name) = stored_as.get(&cur) {
            break *name;
        }
        match shape.inst(cur).map(|i| &i.kind)? {
            InstKind::AttrLoad { obj, attr, .. } => {
                attrs.push(attr.as_str());
                cur = shape.copy_root(*obj);
            }
            InstKind::LoadGlobal { name, .. } => break name.as_str(),
            _ => return None,
        }
    };
    let mut text = head.to_owned();
    for attr in attrs.iter().rev() {
        text.push('.');
        text.push_str(attr);
    }
    decls.resolve_dotted(&text, false)
}
