//! The `object()` sentinels of a module, the attributes its classes bind to one, and the classes
//! it registers with `copyreg`: what `copy.deepcopy` does to an instance holding a sentinel
//! ([`super::ProjectDecls::held_sentinels`], [`super::ProjectDecls::deep_copy_of`]).

use super::{
    ModuleDecls, NamedAttr, dataclass_decorator, decorator_parts, is_summarized_function,
    qualifiers, split_top_level,
};
use crate::ModuleInput;

use heki_ir::walk::FnShape;
use heki_ir::{FunctionId, GlobalStores, InstKind, IrFunction, VarId};
use heki_semantic::{NameResolution, ScopeKind};
use rustc_hash::FxHashMap;
use std::collections::BTreeSet;

/// [`ModuleDecls::object_sentinels`]: the module body's names stored once, outside a loop, from a
/// call of the builtin `object` with no argument (through copies).
pub(super) fn object_sentinels(input: &ModuleInput<'_>, decls: &ModuleDecls) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if input.ir.constants.dynamic || input.ir.has_star_import || !builtin_object(input, decls) {
        return out;
    }
    let module_stores = GlobalStores::of(&input.ir.functions);
    for f in input.ir.functions.iter().filter(|f| f.is_module_body) {
        let shape = FnShape::new(f);
        for b in f.blocks.iter().filter(|b| shape.loop_depth(b.id) == 0) {
            for inst in &b.insts {
                let InstKind::StoreGlobal { name, value } = &inst.kind else {
                    continue;
                };
                let once = module_stores.once(name) && !decls.imports.contains_key(name);
                if once && calls_object(&shape, *value) {
                    out.insert(name.clone());
                }
            }
        }
    }
    out
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

/// Whether `value` is the result of `object()`: a call, with no argument, of the global name
/// `object` (the caller has established that it is the builtin).
fn calls_object(shape: &FnShape<'_>, value: VarId) -> bool {
    let Some(InstKind::Call {
        callee_var: Some(callee),
        args,
        kwargs,
        star_from: None,
        ..
    }) = shape.inst(shape.copy_root(value)).map(|i| &i.kind)
    else {
        return false;
    };
    args.is_empty()
        && kwargs.is_empty()
        && matches!(
            shape.inst(shape.copy_root(*callee)).map(|i| &i.kind),
            Some(InstKind::LoadGlobal { name, .. }) if name == "object"
        )
}

/// The fqn of the module-level name that may be an `object()` sentinel which `value` holds: one
/// of the module's own sentinels, a name an import binds (and nothing in the module binds again),
/// or an attribute of such a name (`sentinels.MISSING`). Whether it is a sentinel is for its own
/// module to say.
fn named_value(shape: &FnShape<'_>, value: VarId, decls: &ModuleDecls) -> Option<String> {
    let global = |var: VarId| match shape.inst(shape.copy_root(var)).map(|i| &i.kind) {
        Some(InstKind::LoadGlobal { name, .. }) => Some(name.as_str()),
        _ => None,
    };
    if let Some(name) = global(value) {
        if decls.object_sentinels.contains(name) {
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
    for body in input.ir.functions.iter().filter(|f| f.is_class_body) {
        class_body_names(body, decls, &mut found);
        method_defaults(input, body, decls, &mut defaults);
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
        let stores = f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|i| match &i.kind {
                InstKind::AttrStore { obj, attr, value } if shape.copy_root(*obj) == receiver => {
                    Some((attr, *value))
                }
                _ => None,
            });
        for (attr, value) in stores {
            let root = shape.copy_root(value);
            let name = named_value(&shape, value, decls).or_else(|| {
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
            let entry = c.named_attrs.entry(attr).or_default();
            entry.body = entry.body.take().or(named.body);
            entry.annotated |= named.annotated;
            entry.validates_default |= named.validates_default;
            entry.held.extend(named.held);
        }
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
            .filter_map(|(p, value)| Some((p.var, named_value(shape, *value, decls)?)))
            .collect();
        if !named.is_empty() {
            out.insert(*id, named);
        }
    }
}

/// The names one class body binds once to a name that may be a sentinel, with the defaults of
/// the fields that the `__init__` a stdlib `@dataclass` generates stores.
fn class_body_names(
    body: &IrFunction,
    decls: &ModuleDecls,
    found: &mut Vec<(String, String, NamedAttr)>,
) {
    let resolve = |name: &str| decls.resolve_dotted(name, true);
    let shape = FnShape::new(body);
    // A class body binds a name as a named copy; `del name` unbinds it.
    let mut bindings: FxHashMap<&str, Vec<VarId>> = FxHashMap::default();
    let mut deleted: Vec<&str> = Vec::new();
    for inst in body.blocks.iter().flat_map(|b| &b.insts) {
        match &inst.kind {
            InstKind::Copy { dst, src } => {
                if let Some(name) = body.var_name(*dst).filter(|n| !n.starts_with('.')) {
                    bindings.entry(name).or_default().push(*src);
                }
            }
            InstKind::Delete { name, .. } => deleted.push(name),
            _ => {}
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
    for (name, values) in bindings {
        let [value] = values[..] else { continue };
        if deleted.contains(&name) {
            continue;
        }
        let (field, validates_default) = fields.get(name).copied().unwrap_or((false, false));
        // `attr: T = field(default=NAME)`: a stdlib dataclass makes `NAME` the class attribute,
        // and `init=False` leaves the field out of `__init__`.
        let (named, init) = match field_default(&shape, value, &resolve) {
            Some((default, init)) if dataclass.is_some() => {
                (named_value(&shape, default, decls), init)
            }
            Some(_) => continue,
            None => (named_value(&shape, value, decls), true),
        };
        let Some(named) = named else { continue };
        let held = if field && init && generated_init {
            BTreeSet::from([named.clone()])
        } else {
            BTreeSet::new()
        };
        let attr = NamedAttr {
            body: Some(named),
            annotated: field,
            validates_default,
            held,
        };
        found.push((body.qualname.clone(), name.to_owned(), attr));
    }
}

/// `(default, init)` of a field specifier `value` holds (`dataclasses.field(default=NAME)`):
/// the default's value, and whether `init` is left on.
fn field_default(
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
fn init_left_on(args: &str) -> bool {
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
