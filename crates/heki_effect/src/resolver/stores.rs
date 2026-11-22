//! The types of module globals and instance attributes (Priority 3), from every store the
//! module's code makes to each: a type read from one store holds only if every other store binds
//! a value of the same type ([`ModuleDecls::global_types`], [`ClassDecl::attr_stores`],
//! [`super::ProjectDecls::member_type`]).

use super::{
    Assumed, AttrStore, ClassDecl, ModuleDecls, ProjectDecls, TypeCtx, aliases_of,
    is_summarized_function, kept_in_declarations, self_aliases,
};
use crate::ModuleInput;
use crate::infer::Inferred;
use crate::types::Ty;

use heki_ir::{ConstLiteral, Inst, InstKind, IrFunction, Terminator, VarId};
use heki_kb::EffectiveEcosystemKb;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeMap;

/// The join of the types of the values the stores to one name bind.
#[derive(Debug, Clone, Default)]
enum Stored {
    /// No store binds a value (none does, or each binds `None`).
    #[default]
    Nothing,
    /// Every value is of this type.
    Of(Ty),
    /// A value of unknown type, or values no tracked type covers.
    Open,
}

impl Stored {
    /// Add a store binding a value of type `t` ([`Inferred::stored`]).
    fn add(&mut self, t: Option<Option<&Ty>>) {
        *self = match (std::mem::take(self), t) {
            (s, None) => s,
            (Stored::Open, _) | (_, Some(None)) => Stored::Open,
            (Stored::Nothing, Some(Some(t))) => Stored::Of(t.clone()),
            (Stored::Of(a), Some(Some(t))) => a.join(t).map_or(Stored::Open, Stored::Of),
        };
    }

    fn into_type(self) -> Option<Ty> {
        match self {
            Stored::Of(t) if kept_in_declarations(&t) => Some(t),
            _ => None,
        }
    }
}

/// Most rounds [`record`] takes: the first without assumptions, then until what the stores bind
/// under the assumed types is what was assumed.
const ROUNDS: usize = 4;

/// What may bind a module's globals behind the stores the IR shows.
#[derive(Clone, Copy)]
struct Unfixed {
    /// `globals()` or `exec` may bind any global at any time
    /// ([`heki_ir::ModuleConstants::dynamic`]).
    dynamic: bool,
    /// A star import binds the names its module exports: those the module body stores before
    /// it may hold anything after.
    last_star_import: Option<u32>,
}

impl Unfixed {
    /// Whether the global `store` of `f` stores may then be bound to anything.
    fn rebinds(self, f: &IrFunction, store: &Inst) -> bool {
        self.dynamic
            || (f.is_module_body && self.last_star_import.is_some_and(|s| store.span.start < s))
    }
}

/// What a store stores into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Target<'f> {
    Global(&'f str),
    /// An attribute of the instances of a class, by its qualname.
    Attr(&'f str, &'f str),
}

/// A function storing into globals or into the attributes of `self`.
struct StoringFn<'f> {
    f: &'f IrFunction,
    /// The class whose instance the function stores into as `self`, and the variables holding
    /// it ([`self_of`]).
    selfs: Option<(&'f str, FxHashSet<VarId>)>,
    /// `__init__` or `__post_init__`, whose stores every instance runs.
    at_init: bool,
    /// The stores, with the type of each value ([`Inferred::stored`]).
    stores: Vec<(Target<'f>, Option<Option<Ty>>)>,
    /// The classes it stores an attribute of that heki cannot name in.
    any: bool,
    /// A store of a value of unknown type in the first round, in a function reading a name the
    /// next rounds assume a type of: what it binds may depend on the types assumed.
    dependent: bool,
}

/// The joins of every store to each name, in one round.
#[derive(Default)]
struct Joins<'f> {
    every: BTreeMap<Target<'f>, Stored>,
    /// The values of known type only.
    known: BTreeMap<Target<'f>, Stored>,
    /// The stores that establish a type: the module body's of a global, `__init__`'s of an
    /// attribute.
    establishing: FxHashMap<Target<'f>, Stored>,
    /// The classes whose methods store attributes heki cannot name.
    any: FxHashSet<&'f str>,
}

impl<'f> Joins<'f> {
    fn of(fns: &[StoringFn<'f>]) -> Self {
        let mut out = Joins::default();
        for s in fns {
            for (target, value) in &s.stores {
                let value = value.as_ref().map(Option::as_ref);
                out.every.entry(*target).or_default().add(value);
                if let Some(Some(t)) = value {
                    out.known.entry(*target).or_default().add(Some(Some(t)));
                }
                let establishing = match target {
                    Target::Global(_) => s.f.is_module_body,
                    Target::Attr(..) => s.at_init,
                };
                if establishing {
                    out.establishing.entry(*target).or_default().add(value);
                }
            }
            if s.any
                && let Some((class, _)) = &s.selfs
            {
                out.any.insert(*class);
            }
        }
        out
    }

    /// Whether the stores establish a type of `target`, which then is `joined`.
    fn established(&self, target: &Target<'_>, joined: &Stored) -> Option<Ty> {
        let at_start = matches!(self.establishing.get(target), Some(Stored::Of(_)));
        let named = match target {
            Target::Global(_) => true,
            Target::Attr(class, _) => !self.any.contains(class),
        };
        (at_start && named)
            .then(|| joined.clone().into_type())
            .flatten()
    }

    /// The types these stores establish, every value counted (`every`) or those of known type
    /// only (`known`, what the next round assumes).
    fn assumed(&self, joins: &BTreeMap<Target<'f>, Stored>) -> Assumed {
        let mut out = Assumed::default();
        for (target, joined) in joins {
            let Some(t) = self.established(target, joined) else {
                continue;
            };
            match target {
                Target::Global(name) => out.globals.insert((*name).to_owned(), t),
                Target::Attr(class, attr) => out
                    .attrs
                    .insert(((*class).to_owned(), (*attr).to_owned()), t),
            };
        }
        out
    }

    fn write(self, decls: &mut ModuleDecls) {
        let mut attrs: FxHashMap<&str, Vec<(Box<str>, AttrStore)>> = FxHashMap::default();
        for (target, joined) in &self.every {
            let established = self.established(target, joined);
            match *target {
                Target::Global(name) => {
                    if let Some(t) = established {
                        decls.global_types.insert(name.to_owned(), t);
                    }
                }
                Target::Attr(class, attr) => {
                    // An annotation types every store of the attribute.
                    let declared = decls
                        .classes
                        .get(class)
                        .is_some_and(|c| c.attr_types.contains_key(attr));
                    if declared {
                        continue;
                    }
                    let store = match (established, joined) {
                        (_, Stored::Nothing) => continue,
                        (Some(t), _) => AttrStore::Established(t),
                        (None, Stored::Of(t)) => AttrStore::Of(t.clone()),
                        (None, Stored::Open) => AttrStore::Open,
                    };
                    // `every` is sorted by class, then attribute.
                    attrs
                        .entry(class)
                        .or_default()
                        .push((Box::from(attr), store));
                }
            }
        }
        for (owner, stores) in attrs {
            // The semantic model declares every class the IR has (one AST).
            if let Some(class) = decls.classes.get_mut(owner) {
                class.attr_stores = stores.into_boxed_slice();
            }
        }
        for owner in self.any {
            if let Some(class) = decls.classes.get_mut(owner) {
                class.stores_any_attr = true;
            }
        }
    }
}

/// The types of the module's globals and of its classes' instance attributes, from every store
/// of the module ([`ModuleDecls::global_types`], [`ClassDecl::attr_stores`],
/// [`ClassDecl::stores_any_attr`]).
///
/// A global's stores are the module body's and those of every function after `global name`
/// (another module's join where it is read: [`ModuleDecls::rebound_global_types`]); an
/// attribute's are those the methods of its class make through `self` (also a copy of it, and
/// the closure cell of a function nested in the method): `self.attr = v`,
/// `setattr(self, "attr", v)`, `object.__setattr__(self, "attr", v)`, `self.__dict__["attr"] = v`.
/// A global the module body binds to a value of known type, and an attribute `__init__` or
/// `__post_init__` binds to one, have the join of every store as their type; a store of a value
/// of unknown type leaves none. An annotation types every store of the name it declares: an
/// annotated global's stores hold its declaration or a narrower value, and an attribute its
/// class annotates has no entry. In a module whose globals `globals()` or `exec` may change,
/// each store of an unannotated global binds a value of unknown type, and so does a store of the
/// module body before a star import, which may rebind the name.
///
/// A store reading the name itself (`self.items = self.items[1:]`, `self.count += 1`) binds a
/// value whose type depends on the one being decided: the first round reads such names as of
/// unknown type, the next ones as of the type the values of known type establish, until the
/// stores bind what was assumed; without that, the first round's types hold.
pub(super) fn record(
    input: &ModuleInput<'_>,
    project: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
    decls: &mut ModuleDecls,
) {
    decls.rebound_global_types = rebound_global_types(input, project, kb, decls);
    let mut fns = storing_functions(input);
    if fns.is_empty() {
        return;
    }
    // The annotations, which type every store of the names they declare.
    let body = input.ir.functions.iter().find(|f| f.is_module_body);
    let body_types = body.map(|f| TypeCtx::new(input, project, kb).infer_types(f));
    let declared = |name: &str| body_types.as_ref().and_then(|t| t.declared(name).cloned());
    let mut facts = Assumed::default();
    for ann in body.iter().flat_map(|f| &f.local_annotations) {
        if let Some(t) = declared(&ann.target) {
            facts.globals.insert(ann.target.clone(), t);
        }
    }
    for (qualname, class) in &decls.classes {
        for (attr, t) in &class.attr_types {
            facts
                .attrs
                .insert((qualname.clone(), attr.clone()), t.clone());
        }
    }
    let unfixed = Unfixed {
        dynamic: input.ir.constants.dynamic,
        last_star_import: input.ir.last_star_import,
    };
    // The first round assumes nothing beyond the annotations.
    let cx = TypeCtx::new(input, project, kb).assuming(facts.clone());
    for s in &mut fns {
        s.infer(&cx, project, &declared, unfixed);
    }
    let first = Joins::of(&fns);
    // The next ones read each name with the type the stores of known type establish, and infer
    // again the functions storing a value of unknown type that read one of them.
    let mut assumed = facts.over(first.assumed(&first.known));
    for s in &mut fns {
        s.dependent = s.stores.iter().any(|(_, v)| matches!(v, Some(None))) && s.reads(&assumed);
    }
    if !fns.iter().any(|s| s.dependent) {
        return first.write(decls);
    }
    for _ in 1..ROUNDS {
        let cx = TypeCtx::new(input, project, kb).assuming(assumed.clone());
        for s in fns.iter_mut().filter(|s| s.dependent) {
            s.infer(&cx, project, &declared, unfixed);
        }
        let joins = Joins::of(&fns);
        let established = facts.over(joins.assumed(&joins.every));
        if established == assumed {
            return joins.write(decls);
        }
        assumed = established;
    }
    first.write(decls);
}

/// [`ModuleDecls::rebound_global_types`]: the stores of the module's functions into the globals
/// of other project modules (`cfg.client = ...`; `ModuleDecls::rebound_globals` names them).
fn rebound_global_types(
    input: &ModuleInput<'_>,
    project: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
    decls: &ModuleDecls,
) -> BTreeMap<String, Option<Ty>> {
    let mut out: BTreeMap<String, Stored> = BTreeMap::new();
    let names: FxHashSet<&str> = decls
        .rebound_globals
        .iter()
        .filter_map(|fqn| fqn.rsplit_once('.').map(|(_, name)| name))
        .collect();
    if names.is_empty() {
        return BTreeMap::new();
    }
    let cx = TypeCtx::new(input, project, kb);
    let stores = |f: &'_ IrFunction| {
        insts(f).any(|inst| matches!(&inst.kind, InstKind::AttrStore { attr, .. } if names.contains(attr.as_str())))
    };
    for f in input.ir.functions.iter().filter(|f| stores(f)) {
        let types = cx.infer_types(f);
        for inst in insts(f) {
            let InstKind::AttrStore { obj, attr, value } = &inst.kind else {
                continue;
            };
            if let Some(Ty::Ref(module)) = types.ty(*obj)
                && *module != input.module_name
                && project.module(module).is_some()
            {
                let target = format!("{module}.{attr}");
                out.entry(target).or_default().add(types.stored(*value));
            }
        }
    }
    out.into_iter()
        .filter_map(|(fqn, stored)| match stored {
            Stored::Nothing => None,
            Stored::Of(t) => Some((fqn, Some(t))),
            Stored::Open => Some((fqn, None)),
        })
        .collect()
}

/// The functions of the module storing into a global or an attribute of `self` (before their
/// types are inferred).
fn storing_functions<'f>(input: &'f ModuleInput<'_>) -> Vec<StoringFn<'f>> {
    // Each method's class and the name of its `self`, for the functions nested in it.
    let methods: FxHashMap<&str, (&str, &str)> = input
        .ir
        .functions
        .iter()
        .filter(|f| is_summarized_function(f) && !f.has_decorator(&["staticmethod", "classmethod"]))
        .filter_map(|f| {
            let owner = f.owner_class_qualname.as_deref()?;
            Some((
                f.qualname.as_str(),
                (owner, f.params.first()?.name.as_str()),
            ))
        })
        .collect();
    let mut out = Vec::new();
    for f in &input.ir.functions {
        let selfs = is_summarized_function(f)
            .then(|| self_of(f, &methods))
            .flatten()
            .filter(|(_, selfs)| insts(f).any(|inst| touches(&inst.kind, selfs)));
        let globals = insts(f).any(|inst| matches!(inst.kind, InstKind::StoreGlobal { .. }));
        if selfs.is_none() && !globals {
            continue;
        }
        out.push(StoringFn {
            f,
            at_init: f.owner_class_qualname.is_some()
                && (f.name == "__init__" || f.name == "__post_init__"),
            selfs,
            stores: Vec::new(),
            any: false,
            dependent: false,
        });
    }
    out
}

impl<'f> StoringFn<'f> {
    /// Whether the function reads a global or an attribute of `self` that `assumed` types.
    fn reads(&self, assumed: &Assumed) -> bool {
        insts(self.f).any(|inst| match &inst.kind {
            InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. } => {
                assumed.globals.contains_key(name)
            }
            InstKind::AttrLoad { obj, attr, .. } => {
                self.selfs.as_ref().is_some_and(|(class, selfs)| {
                    selfs.contains(obj)
                        && assumed
                            .attrs
                            .contains_key(&((*class).to_owned(), attr.clone()))
                })
            }
            _ => false,
        })
    }

    /// The stores of the function, with the types `cx` infers for their values (`declared`
    /// gives the annotation of a global, which types every store of it).
    fn infer(
        &mut self,
        cx: &TypeCtx<'_>,
        project: &ProjectDecls,
        declared: &dyn Fn(&str) -> Option<Ty>,
        unfixed: Unfixed,
    ) {
        let f = self.f;
        let types = cx.infer_types(f);
        let mut stores: Vec<(Target<'f>, VarId)> = Vec::new();
        // The globals stored that the module may bind to anything later.
        let mut rebindable: FxHashSet<&str> = FxHashSet::default();
        let mut any = false;
        for inst in insts(f) {
            if let InstKind::StoreGlobal { name, value } = &inst.kind {
                stores.push((Target::Global(name), *value));
                if unfixed.rebinds(f, inst) {
                    rebindable.insert(name);
                }
                continue;
            }
            let Some((class, selfs)) = &self.selfs else {
                continue;
            };
            match self_store(f, &inst.kind, selfs, &types) {
                Some(SelfStore::Attr(attr, value)) => {
                    stores.push((Target::Attr(class, attr), value))
                }
                Some(SelfStore::Namespace(ns)) => match namespace_stores(f, ns) {
                    Some(named) => {
                        stores.extend(named.into_iter().map(|(a, v)| (Target::Attr(class, a), v)));
                    }
                    None => any = true,
                },
                Some(SelfStore::Any) => any = true,
                None => {}
            }
        }
        self.any = any;
        self.stores = stores
            .into_iter()
            .map(|(target, value)| {
                let mut value = types.stored(value).map(|t| t.cloned());
                if let Target::Global(name) = target {
                    if rebindable.contains(name) {
                        value = Some(None);
                    }
                    if let Some(d) = declared(name) {
                        value = Some(Some(match value {
                            Some(Some(t)) if project.narrower(&t, &d) => t,
                            _ => d,
                        }));
                    }
                }
                (target, value)
            })
            .collect();
    }
}

fn insts(f: &IrFunction) -> impl Iterator<Item = &Inst> {
    f.blocks.iter().flat_map(|b| &b.insts)
}

/// The class whose instance `f` stores into as `self`, and the variables holding it: a method's
/// first parameter, or in a function nested in a method the closure cell of that parameter
/// (`f` binding no name of its own like it), each with their copies.
fn self_of<'f>(
    f: &'f IrFunction,
    methods: &FxHashMap<&str, (&'f str, &'f str)>,
) -> Option<(&'f str, FxHashSet<VarId>)> {
    if f.owner_class_qualname.is_some() {
        let (owner, _) = methods.get(f.qualname.as_str())?;
        return Some((owner, self_aliases(f, f.params.first()?.var)));
    }
    let mut enclosing = f.qualname.as_str();
    let (owner, name) = loop {
        enclosing = enclosing.rsplit_once(".<locals>.")?.0;
        if let Some(found) = methods.get(enclosing) {
            break *found;
        }
    };
    if f.params.iter().any(|p| p.name == name) {
        return None;
    }
    let cells = insts(f).filter_map(|inst| match &inst.kind {
        InstKind::LoadDeref { dst, name: n } if n == name => Some(*dst),
        _ => None,
    });
    Some((owner, aliases_of(f, cells)))
}

/// Whether `kind` may store an attribute of `self` (`selfs`): the instructions [`self_store`]
/// reads, before the function's types are inferred.
fn touches(kind: &InstKind, selfs: &FxHashSet<VarId>) -> bool {
    match kind {
        InstKind::AttrStore { obj, .. } => selfs.contains(obj),
        InstKind::AttrLoad { obj, attr, .. } => attr == "__dict__" && selfs.contains(obj),
        // `setattr(self, name, value)`, `vars(self)`, as spelled.
        InstKind::Call { callee, args, .. } => {
            matches!((callee.as_str(), args.len()), ("setattr", 3) | ("vars", 1))
                && selfs.contains(&args[0])
        }
        InstKind::MethodCall {
            recv, method, args, ..
        } => {
            method == "__setattr__"
                && (selfs.contains(recv) || args.first().is_some_and(|a| selfs.contains(a)))
        }
        _ => false,
    }
}

/// A store to an attribute of `self`.
enum SelfStore<'f> {
    /// Of the named attribute, binding the value of the variable.
    Attr(&'f str, VarId),
    /// Through the instance's namespace, loaded into the variable.
    Namespace(VarId),
    /// Of an attribute heki cannot name.
    Any,
}

/// The store to an attribute of `self` (`selfs`) that `kind` makes, with `f`'s inferred `types`.
fn self_store<'f>(
    f: &'f IrFunction,
    kind: &'f InstKind,
    selfs: &FxHashSet<VarId>,
    types: &Inferred,
) -> Option<SelfStore<'f>> {
    let builtin = |v: VarId, name: &str| matches!(types.ty(v), Some(Ty::Ref(b)) if b.strip_prefix("builtins.") == Some(name));
    let named = |name: VarId, value: VarId| match literal_str(f, name) {
        Some(attr) => SelfStore::Attr(attr, value),
        None => SelfStore::Any,
    };
    match kind {
        InstKind::AttrStore { obj, attr, value } if selfs.contains(obj) => {
            Some(SelfStore::Attr(attr, *value))
        }
        // `setattr(self, name, value)`.
        InstKind::Call {
            callee_var: Some(c),
            args,
            star_from: None,
            ..
        } if args.len() == 3 && selfs.contains(&args[0]) && builtin(*c, "setattr") => {
            Some(named(args[1], args[2]))
        }
        // `vars(self)`, the instance's namespace.
        InstKind::Call {
            dst,
            callee_var: Some(c),
            args,
            ..
        } if args.len() == 1 && selfs.contains(&args[0]) && builtin(*c, "vars") => {
            Some(SelfStore::Namespace(*dst))
        }
        InstKind::AttrLoad { dst, obj, attr } if attr == "__dict__" && selfs.contains(obj) => {
            Some(SelfStore::Namespace(*dst))
        }
        // `self.__setattr__(name, value)`, `object.__setattr__(self, name, value)`.
        InstKind::MethodCall {
            recv, method, args, ..
        } if method == "__setattr__" => match args.as_slice() {
            [name, value] if selfs.contains(recv) => Some(named(*name, *value)),
            [obj, name, value] if selfs.contains(obj) => Some(named(*name, *value)),
            _ => None,
        },
        _ => None,
    }
}

/// The string literal `var` holds, through copies.
fn literal_str(f: &IrFunction, var: VarId) -> Option<&str> {
    let mut cur = var;
    for _ in 0..8 {
        match &insts(f).find(|inst| inst.kind.dst() == Some(cur))?.kind {
            InstKind::Const {
                literal: ConstLiteral::Str(s),
                ..
            } => return Some(s),
            InstKind::Copy { src, .. } => cur = *src,
            _ => return None,
        }
    }
    None
}

/// The methods of an instance's namespace that store nothing in it.
const NAMESPACE_READS: &[&str] = &[
    "get",
    "keys",
    "values",
    "items",
    "copy",
    "pop",
    "__contains__",
];

/// The stores through the namespace `ns` (`self.__dict__`, `vars(self)`, and their copies) of
/// the attributes a string literal names (`ns["attr"] = v`, `ns.setdefault("attr", v)`), or
/// `None` when it may store another one: it is written otherwise, or handed on (passed,
/// returned, merged). Reads, deletions, formatting, and comparisons store nothing.
fn namespace_stores(f: &IrFunction, ns: VarId) -> Option<Vec<(&str, VarId)>> {
    let held = aliases_of(f, [ns]);
    let mut out = Vec::new();
    for b in &f.blocks {
        let merged = b.phis.iter().any(|phi| {
            !held.contains(&phi.dst) && phi.incoming.iter().any(|(_, v)| held.contains(v))
        });
        let handed = matches!(b.terminator, Terminator::Return(Some(v)) | Terminator::Raise(Some(v)) if held.contains(&v));
        if merged || handed {
            return None;
        }
        for inst in &b.insts {
            if !inst.kind.operands().iter().any(|v| held.contains(v)) {
                continue;
            }
            match &inst.kind {
                InstKind::Copy { .. }
                | InstKind::Compare { .. }
                | InstKind::Convert { .. }
                | InstKind::Format { .. }
                | InstKind::BuildString { .. } => {}
                InstKind::SubscriptLoad { obj, index, .. }
                | InstKind::SubscriptDelete { obj, index, .. }
                    if held.contains(obj) && !held.contains(index) => {}
                InstKind::SubscriptStore {
                    obj, index, value, ..
                } if held.contains(obj) && !held.contains(value) => {
                    out.push((literal_str(f, *index)?, *value));
                }
                InstKind::MethodCall {
                    recv,
                    method,
                    args,
                    kwargs,
                    ..
                } if held.contains(recv)
                    && kwargs.is_empty()
                    && !args.iter().any(|a| held.contains(a)) =>
                {
                    match (method.as_str(), args.as_slice()) {
                        ("setdefault", [key, value]) => out.push((literal_str(f, *key)?, *value)),
                        // A default of `None`, on which every call raises.
                        ("setdefault", [_]) => {}
                        (m, _) if NAMESPACE_READS.contains(&m) => {}
                        _ => return None,
                    }
                }
                _ => return None,
            }
        }
    }
    Some(out)
}

/// The join of what the classes whose stores an instance may receive store in `attr`
/// ([`ClassDecl::attr_stores`]; an annotated attribute's declared type; any type for a class
/// storing attributes heki cannot name), `None` for none or an unknown one.
pub(super) fn join_family<'d>(
    family: impl IntoIterator<Item = &'d ClassDecl>,
    attr: &str,
) -> Option<Ty> {
    let mut out = Stored::Nothing;
    for decl in family {
        if decl.stores_any_attr {
            return None;
        }
        match decl.stored(attr) {
            Some(AttrStore::Established(t) | AttrStore::Of(t)) => out.add(Some(Some(t))),
            Some(AttrStore::Open) => return None,
            None => out.add(decl.attr_types.get(attr).map(Some)),
        }
    }
    out.into_type()
}
