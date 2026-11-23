//! The types of module globals and instance attributes (Priority 3), from every store the
//! project's code makes to each: a name the module body (a global) or `__init__` /
//! `__post_init__` through `self` (an attribute) binds to a value of known type has the join of
//! what every store of it binds ([`ModuleDecls::global_types`], [`ClassDecl::attr_stores`],
//! [`ProjectDecls::member_type`]).
//!
//! A module decides the types of its own globals and of its classes' attributes ([`record`]).
//! What another module stores into them through a receiver whose type the inference knows is
//! joined where the type is read ([`ModuleDecls::stores_elsewhere`], [`Elsewhere`]).

use super::{
    AttrStore, ClassDecl, ModuleDecls, ProjectDecls, StoreDependency, StoreFallbacks, StoreTarget,
    StoredElsewhere, StoresElsewhere, Table, TypeCtx, aliases_of, is_summarized_function,
    kept_in_declarations, note, self_aliases,
};
use crate::ModuleInput;
use crate::infer::Inferred;
use crate::types::Ty;

use heki_ir::walk::{
    Answer, Budget, Def, Facts, FnShape, Join, Sparse, Use, Visit, fixpoint, reach,
};
use heki_ir::{BlockId, BuiltinKind, Inst, InstKind, IrFunction, LastBindings, Terminator, VarId};
use heki_kb::EffectiveEcosystemKb;
use rustc_hash::{FxHashMap, FxHashSet};
use std::borrow::{Borrow, Cow};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;

/// What reading a global or an attribute gives while [`record`] decides its type.
#[derive(Clone, PartialEq)]
pub(crate) enum Read {
    /// No store has bound a value yet.
    Pending,
    /// A value of this type.
    Of(Ty),
}

/// The reads of the names [`record`] decides, which [`TypeCtx`] hands to it.
pub(crate) trait StoreReads {
    /// Reading the module's global `name`: `None` for what the declarations give.
    fn global(&self, name: &str) -> Option<Read>;
    /// Reading `attr` on an instance of the class `class` (past `class` itself with
    /// `skip_self`): `None` for what the declarations give.
    fn member(&self, class: &str, attr: &str, skip_self: bool) -> Option<Read>;
}

/// A read of [`StoreReads`].
#[derive(Clone, PartialEq, Eq)]
enum Query {
    Global(String),
    Member(String, String, bool),
}

/// A [`Query`] by reference, which the reads of one inference are looked up by.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum QueryOf<'q> {
    Global(&'q str),
    Member(&'q str, &'q str, bool),
}

trait AsQuery {
    fn as_query(&self) -> QueryOf<'_>;
}

impl AsQuery for Query {
    fn as_query(&self) -> QueryOf<'_> {
        match self {
            Query::Global(name) => QueryOf::Global(name),
            Query::Member(class, attr, skip_self) => QueryOf::Member(class, attr, *skip_self),
        }
    }
}

impl AsQuery for QueryOf<'_> {
    fn as_query(&self) -> QueryOf<'_> {
        *self
    }
}

impl<'q> Borrow<dyn AsQuery + 'q> for Query {
    fn borrow(&self) -> &(dyn AsQuery + 'q) {
        self
    }
}

impl Hash for dyn AsQuery + '_ {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_query().hash(state);
    }
}

impl Hash for Query {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_query().hash(state);
    }
}

impl PartialEq for dyn AsQuery + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.as_query() == other.as_query()
    }
}

impl Eq for dyn AsQuery + '_ {}

/// The reads one inference of a function made through [`StoreReads`], each with what it gave,
/// in the order it first made them. The inference depends on nothing else that [`record`]
/// changes: inferred again, it makes the same reads while each gives what it gave, so when
/// every one does, it infers the same types.
type Trace = Box<[(Query, Option<Read>)]>;

/// The reads one inference makes ([`Trace`]). A read made again in the same inference gives
/// what it gave the first time: the stores joined so far change only between inferences.
#[derive(Default)]
struct Recorder(RefCell<FxHashMap<Query, (usize, Option<Read>)>>);

impl Recorder {
    fn read(&self, query: QueryOf<'_>, answer: impl FnOnce() -> Option<Read>) -> Option<Read> {
        if let Some((_, known)) = self.0.borrow().get(&query as &dyn AsQuery) {
            return known.clone();
        }
        let answer = answer();
        let owned = match query {
            QueryOf::Global(name) => Query::Global(name.to_owned()),
            QueryOf::Member(class, attr, skip_self) => {
                Query::Member(class.to_owned(), attr.to_owned(), skip_self)
            }
        };
        let mut reads = self.0.borrow_mut();
        let order = reads.len();
        reads.insert(owned, (order, answer.clone()));
        answer
    }

    /// The reads made since the last call.
    fn take(&self) -> Trace {
        let mut reads: Vec<_> = self.0.take().into_iter().collect();
        reads.sort_unstable_by_key(|(_, (order, _))| *order);
        reads.into_iter().map(|(q, (_, a))| (q, a)).collect()
    }
}

/// The `DeclKeys::class_facts` key of whether project code calls `__setstate__` itself.
pub(super) const SETSTATE_KEY: &str = "__setstate__";

/// Most visits of one storing function in the fixpoint of [`record`]: the first finds the
/// values of the stores reading nothing being decided, each next one a value a read gave since.
const VISITS: u32 = 4;

/// The join of the types of the values the stores to one name bind.
#[derive(Debug, Clone, Default, PartialEq)]
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

    /// The type the declarations keep.
    fn type_of(&self) -> Option<Ty> {
        match self {
            Stored::Of(t) if kept_in_declarations(t) => Some(t.clone()),
            _ => None,
        }
    }

    /// What a store of another module records: `None` for an unknown type.
    fn elsewhere(self) -> Option<Option<Ty>> {
        match self {
            Stored::Nothing => None,
            Stored::Of(t) => Some(Some(t)),
            Stored::Open => Some(None),
        }
    }
}

impl Join for Stored {
    fn join(&mut self, other: &Self) -> bool {
        let before = self.clone();
        match other {
            Stored::Nothing => {}
            Stored::Of(t) => self.add(Some(Some(t))),
            Stored::Open => self.add(Some(None)),
        }
        *self != before
    }
}

/// The classes whose in-place operators (`+=`, `|=`) give the object they change or a new one of
/// its class: rebinding a name holding one to what the operator gives on its own value binds
/// nothing of another class.
const KEEP_CLASS_IN_PLACE: &[&str] = &[
    "builtins.list",
    "builtins.set",
    "builtins.dict",
    "builtins.bytearray",
    "collections.deque",
    "builtins.str",
    "builtins.bytes",
];

/// Whether `t` is a type of [`KEEP_CLASS_IN_PLACE`]: a name holding it keeps it through the
/// stores rebinding it in place.
fn keeps_in_place(t: &Ty) -> bool {
    let members = t.members();
    !members.is_empty()
        && members
            .iter()
            .all(|o| KEEP_CLASS_IN_PLACE.contains(&o.class.as_str()))
}

/// What reading a name being decided depends on: the name, whether a class (by qualname)
/// stores attributes heki cannot name, or whether the module stores globals heki cannot name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Name(StoreTarget),
    Class(String),
    Globals,
}

/// What a function of the first round read of the globals and attributes: their names, which
/// tell whether it reads one being decided.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Named {
    Global(String),
    Attr(String),
}

/// The joins of the stores of a module's code (of one function, or of all), by what they store
/// into.
#[derive(Debug, Clone, Default, PartialEq)]
struct Joins {
    every: BTreeMap<StoreTarget, Stored>,
    /// The stores that establish a type: the module body's of a global, `__init__`'s or
    /// `__post_init__`'s through `self` of an attribute.
    establishing: BTreeMap<StoreTarget, Stored>,
    /// The classes, by qualname, whose instances get attributes heki cannot name.
    any: BTreeSet<String>,
    /// The classes whose `__setstate__` restores attributes heki cannot name from its state.
    restoring: BTreeSet<String>,
    /// A store binds a global of the module heki cannot name.
    any_global: bool,
    /// The names a store rebinds to what an in-place operator gives on their own value.
    in_place: BTreeSet<StoreTarget>,
}

impl Join for Joins {
    fn join(&mut self, other: &Self) -> bool {
        let mut grew = false;
        for (target, s) in &other.every {
            grew |= self.every.entry(target.clone()).or_default().join(s);
        }
        for (target, s) in &other.establishing {
            grew |= self.establishing.entry(target.clone()).or_default().join(s);
        }
        for class in &other.any {
            grew |= self.any.insert(class.clone());
        }
        for class in &other.restoring {
            grew |= self.restoring.insert(class.clone());
        }
        grew |= self.any_global.join(&other.any_global);
        for target in &other.in_place {
            grew |= self.in_place.insert(target.clone());
        }
        grew
    }

    fn weight(&self) -> usize {
        1 + self.every.len()
            + self.establishing.len()
            + self.any.len()
            + self.restoring.len()
            + self.in_place.len()
    }
}

impl Joins {
    /// Join `other` in; returns the keys whose facts grew.
    fn join_keys(&mut self, other: &Self) -> Vec<Key> {
        let mut out = Vec::new();
        for (target, s) in &other.every {
            if self.every.entry(target.clone()).or_default().join(s) {
                out.push(Key::Name(target.clone()));
            }
        }
        for (target, s) in &other.establishing {
            if self.establishing.entry(target.clone()).or_default().join(s) {
                out.push(Key::Name(target.clone()));
            }
        }
        for target in &other.in_place {
            if self.in_place.insert(target.clone()) {
                out.push(Key::Name(target.clone()));
            }
        }
        // A class gaining either changes what reading its attributes gives: `any` leaves them
        // unknown, `restoring` holds only while the restores do.
        for class in &other.any {
            if self.any.insert(class.clone()) {
                out.push(Key::Class(class.clone()));
            }
        }
        for class in &other.restoring {
            if self.restoring.insert(class.clone()) {
                out.push(Key::Class(class.clone()));
            }
        }
        if other.any_global && !self.any_global {
            self.any_global = true;
            out.push(Key::Globals);
        }
        out
    }

    fn store(&mut self, target: StoreTarget, establishing: bool, value: Option<Option<&Ty>>) {
        if establishing {
            self.establishing
                .entry(target.clone())
                .or_default()
                .add(value);
        }
        self.every.entry(target).or_default().add(value);
    }

    /// The type of `target` when its establishing stores bind a value of known type and heki
    /// can name every store of it: the join of every store.
    fn established(&self, target: &StoreTarget) -> Option<Ty> {
        let named = match target {
            StoreTarget::Global(_) => !self.any_global,
            StoreTarget::Attr(class, _) => !self.any.contains(class),
        };
        let at_start = matches!(self.establishing.get(target), Some(Stored::Of(_)));
        if !(named && at_start) {
            return None;
        }
        let t = self.every.get(target)?.type_of()?;
        (!self.in_place.contains(target) || keeps_in_place(&t)).then_some(t)
    }

    /// The types these joins give the module's globals and attributes (`decls` gives the
    /// annotations, which type every store of the attributes they declare).
    fn decide(&self, decls: &ModuleDecls) -> Decided {
        let mut out = Decided {
            any: self.any.clone(),
            restoring: self.restoring.difference(&self.any).cloned().collect(),
            ..Decided::default()
        };
        for target in &self.in_place {
            if let StoreTarget::Attr(class, attr) = target {
                let declared = decls
                    .classes
                    .get(class)
                    .is_none_or(|c| c.attr_types.contains_key(attr));
                if !declared {
                    out.in_place
                        .entry(class.clone())
                        .or_default()
                        .insert(attr.clone());
                }
            }
        }
        for (target, joined) in &self.every {
            let established = self.established(target);
            match target {
                StoreTarget::Global(name) => {
                    if let Some(t) = established {
                        out.globals.insert(name.clone(), t);
                    }
                }
                StoreTarget::Attr(class, attr) => {
                    let declared = decls
                        .classes
                        .get(class)
                        .is_none_or(|c| c.attr_types.contains_key(attr));
                    if declared {
                        continue;
                    }
                    let store = match (established, joined) {
                        (_, Stored::Nothing) => continue,
                        (Some(t), _) => AttrStore::Established(t),
                        (None, Stored::Of(t)) => AttrStore::Of(t.clone()),
                        (None, Stored::Open) => AttrStore::Open,
                    };
                    out.attrs
                        .entry(class.clone())
                        .or_default()
                        .insert(attr.clone(), store);
                }
            }
        }
        out
    }
}

/// The types of a module's globals and of its classes' attributes, as [`Joins::decide`] gives
/// them.
#[derive(Debug, Default, PartialEq)]
struct Decided {
    globals: BTreeMap<String, Ty>,
    attrs: BTreeMap<String, BTreeMap<String, AttrStore>>,
    any: BTreeSet<String>,
    restoring: BTreeSet<String>,
    in_place: BTreeMap<String, BTreeSet<String>>,
}

impl Decided {
    fn write(self, decls: &mut ModuleDecls) {
        decls.global_types.extend(self.globals);
        for (qualname, attrs) in self.attrs {
            // The semantic model declares every class the IR has (one AST).
            for class in decls.classes.get_mut(&qualname).into_iter() {
                class.attr_stores = attrs
                    .iter()
                    .map(|(attr, store)| (Box::from(attr.as_str()), store.clone()))
                    .collect();
            }
        }
        for qualname in self.any {
            if let Some(class) = decls.classes.get_mut(&qualname) {
                class.stores_any_attr = true;
            }
        }
        for qualname in self.restoring {
            if let Some(class) = decls.classes.get_mut(&qualname) {
                class.restores_state = true;
            }
        }
        for (qualname, attrs) in self.in_place {
            if let Some(class) = decls.classes.get_mut(&qualname) {
                class.in_place_attrs = attrs.into_iter().map(Box::from).collect();
            }
        }
    }

    /// What `fallback` gives where it differs from these types (`None`: nowhere).
    fn fallbacks(&self, fallback: &Decided) -> Option<StoreFallbacks> {
        let mut out = StoreFallbacks::default();
        for name in self.globals.keys().chain(fallback.globals.keys()) {
            let t = fallback.globals.get(name);
            if self.globals.get(name) != t {
                out.globals.insert(name.clone(), t.cloned());
            }
        }
        let classes: BTreeSet<&String> = self.attrs.keys().chain(fallback.attrs.keys()).collect();
        let none = BTreeMap::new();
        for class in classes {
            let (own, other) = (
                self.attrs.get(class).unwrap_or(&none),
                fallback.attrs.get(class).unwrap_or(&none),
            );
            for attr in own.keys().chain(other.keys()) {
                if own.get(attr) == other.get(attr) {
                    continue;
                }
                // The fallback joins every store the types found, so it has an entry wherever
                // they have one.
                let store = other.get(attr).cloned().unwrap_or(AttrStore::Open);
                out.attrs
                    .entry(class.clone())
                    .or_default()
                    .insert(attr.clone(), store);
            }
        }
        // A class these types only restore stores any attribute in the fallback when the
        // fallback has it among `any`.
        let any = fallback.any.iter().filter(|c| !self.any.contains(*c));
        let restoring = fallback
            .restoring
            .iter()
            .filter(|c| !self.any.contains(*c) && !self.restoring.contains(*c));
        out.any_attr = any.chain(restoring).cloned().collect();
        let differs = !(out.globals.is_empty() && out.attrs.is_empty() && out.any_attr.is_empty());
        differs.then_some(out)
    }
}

/// What may bind a module's globals behind the stores the IR shows.
struct Unfixed<'p> {
    /// `globals()` or `exec` may bind any global at any time
    /// ([`heki_ir::ModuleConstants::dynamic`]).
    dynamic: bool,
    /// The end of each star import that runs (not one under `if TYPE_CHECKING:`), and the
    /// names its module exports when they are known ([`ModuleDecls::exports`]): a name the
    /// module body stores may hold anything after it, unless a store binds the name after it
    /// for good ([`LastBindings::overrides`]).
    star_imports: Vec<(u32, Option<&'p [String]>)>,
    /// Where the module body binds a name for good (`None`: the body stores nothing).
    last: Option<LastBindings>,
}

impl<'p> Unfixed<'p> {
    fn of(input: &ModuleInput<'_>, project: &'p ProjectDecls, fns: &[StoringFn<'_>]) -> Self {
        let star_imports: Vec<(u32, Option<&[String]>)> = input
            .semantic
            .star_imports
            .iter()
            .filter(|star| !star.in_type_checking)
            .map(|star| {
                let exports = super::resolve_relative_module(
                    &input.module_name,
                    input.is_package,
                    star.level,
                    star.module.as_deref(),
                    input.root_is_package,
                )
                .and_then(|module| {
                    let all = format!("{module}.__all__");
                    let fixed = !project.grown_elsewhere(&all);
                    project
                        .module(&module)?
                        .exports
                        .as_deref()
                        .filter(|_| fixed)
                });
                (star.span.end, exports)
            })
            .collect();
        let last = fns
            .iter()
            .find(|s| s.f.is_module_body)
            .filter(|_| !star_imports.is_empty())
            .map(|body| LastBindings::of(&body.shape));
        Self {
            dynamic: input.ir.constants.dynamic,
            star_imports,
            last,
        }
    }

    /// Whether the global `name` that `store` in `block` of `s` stores may then be bound to
    /// anything.
    fn rebinds(&self, s: &StoringFn<'_>, block: BlockId, store: &Inst, name: &str) -> bool {
        let overridden = |end: u32| {
            self.last
                .is_some_and(|last| last.overrides(&s.shape, block, store, end))
        };
        let star_rebinds = |(end, exports): &(u32, Option<&[String]>)| {
            exports.is_none_or(|e| e.iter().any(|n| n == name)) && !overridden(*end)
        };
        self.dynamic || (s.f.is_module_body && self.star_imports.iter().any(star_rebinds))
    }
}

/// A function of the module that may store into a global or into an attribute of an instance.
struct StoringFn<'f> {
    f: &'f IrFunction,
    shape: FnShape<'f>,
    /// The loads of a global or an attribute whose object the function changes in place
    /// ([`changed_loads`]).
    changed: Box<[(BlockId, usize)]>,
    /// The class whose instance the function stores into as `self` (qualname), and the
    /// variables holding it ([`self_of`]).
    selfs: Option<(&'f str, FxHashSet<VarId>)>,
    /// `__init__` or `__post_init__`, whose stores through `self` every instance runs.
    at_init: bool,
    /// The state parameter of a `__setstate__` method.
    state: Option<VarId>,
}

/// What the stores of a function bind in other modules, by name.
#[derive(Default)]
struct Foreign {
    globals: BTreeMap<String, ForeignStore>,
    attrs: BTreeMap<String, BTreeMap<String, ForeignStore>>,
    unnamed: BTreeSet<String>,
}

/// What the stores of a function do to one name of another module.
#[derive(Default)]
struct ForeignStore {
    /// The join of the values.
    stored: Stored,
    /// A store rebinds the name to what an in-place operator gives on its own value.
    in_place: bool,
    /// The function changes the object the name holds in place ([`StoringFn::changed`]).
    changed: bool,
}

impl Foreign {
    fn into_decl(self) -> Option<StoresElsewhere> {
        let stored = |s: ForeignStore| {
            let ty = s.stored.elsewhere();
            (ty.is_some() || s.in_place || s.changed).then_some(StoredElsewhere {
                ty,
                in_place: s.in_place,
                changed: s.changed,
            })
        };
        let out = StoresElsewhere {
            globals: self
                .globals
                .into_iter()
                .filter_map(|(fqn, s)| Some((fqn, stored(s)?)))
                .collect(),
            attrs: self
                .attrs
                .into_iter()
                .map(|(class, attrs)| {
                    let attrs = attrs
                        .into_iter()
                        .filter_map(|(attr, s)| Some((attr, stored(s)?)));
                    (class, attrs.collect::<BTreeMap<_, _>>())
                })
                .filter(|(_, attrs)| !attrs.is_empty())
                .collect(),
            unnamed: self.unnamed,
        };
        let any = !(out.globals.is_empty() && out.attrs.is_empty() && out.unnamed.is_empty());
        any.then_some(out)
    }
}

/// The types of the module's globals and of its classes' instance attributes, from every store
/// of the module's code ([`ModuleDecls::global_types`], [`ClassDecl::attr_stores`],
/// [`ClassDecl::stores_any_attr`]), and what its code stores into other modules
/// ([`ModuleDecls::stores_elsewhere`]).
///
/// A store stores into what its receiver holds: `self` in a method (also a copy of it, and the
/// closure cell of a function nested in the method), or a value whose type the inference knows
/// (an instance of a project class, a project module; `cls()` in a class method, a parameter
/// `obj: Helper`, `import cfg` in a function). The stores are `x = v` after `global x`,
/// `obj.x = v`, `setattr(obj, "x", v)`, `object.__setattr__(obj, "x", v)` and
/// `super().__setattr__("x", v)` (also through an alias, the name a string constant of the
/// module may give), and those through the namespace `obj.__dict__` or `vars(obj)`
/// (`ns["x"] = v`, `ns.setdefault("x", v)`); one under a name heki cannot read, one replacing
/// the namespace or the class (`obj.__dict__ = state`), and one handing the namespace on stores
/// any attribute, except in `__setstate__` from its state ([`ClassDecl::restores_state`]).
/// A global the module body binds to a value of known type, and an attribute `__init__` or
/// `__post_init__` binds to one through `self`, have the join of every store as their type; a
/// store of a value of unknown type leaves none. An annotation types every store of the name it
/// declares: an annotated global's stores hold its declaration or a narrower value, and an
/// attribute its class annotates has no entry. In a module whose globals `globals()` or `exec`
/// may change, each store of an unannotated global binds a value of unknown type, and so does a
/// store of the module body before a star import, which may rebind the name.
///
/// A store reading a name being decided (`self.items = self.items[1:]`,
/// `self.active = self.client`) binds a value whose type depends on the one being decided. The
/// types are a least fixpoint ([`heki_ir::walk::fixpoint`]): every such read starts with no value,
/// then gives what the stores bound so far give, as the final declarations will (the join over
/// the class's family, [`ProjectDecls::member_type`]), and each function is visited again only
/// when a name it reads grew, up to [`VISITS`] times, and inferred again only when a read it
/// made gives something else ([`decide`]). Reads that the final declarations answer from
/// another module give what pass 1 gives: an attribute of a class whose family has a member in
/// another module, and a class of another module along the MRO. Past the visit limit or the
/// budget, or when a read assumed a value the stores never bound, the types are those of reads
/// giving what pass 1 gives (the first round). What other modules store into the module's globals or
/// into its classes' instances is not known here: where the types differ from the first round's
/// joined with them, the latter are kept ([`ModuleDecls::store_fallbacks`]), and hold once
/// another module stores into a name their stores read ([`StoreFallbacks::dependencies`]).
///
/// The stores into other modules are those through receivers as the decided types type them:
/// each visit of the fixpoint joins what its function stores into other modules
/// (`self.base.client = v`, where only the stores of `base` type it), and when the first
/// round's types hold, the functions of the fixpoint are classified once more under them. A
/// receiver is typed by this module's stores alone, not by what other modules store into it.
pub(super) fn record(
    input: &ModuleInput<'_>,
    project: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
    decls: &mut ModuleDecls,
) {
    let cx = TypeCtx::new(input, project, kb);
    record_getstates(input, &cx, decls);
    let fns = storing_functions(input, &cx);
    if fns.is_empty() {
        return;
    }
    let unfixed = Unfixed::of(input, project, &fns);
    let (decided, fallbacks, foreign, tables) = {
        let decls: &ModuleDecls = decls;
        let first_reads = FirstReads::default();
        let cx0 = TypeCtx::new(input, project, kb).reading_stores(&first_reads);
        // The annotations, which type every store of the globals they declare.
        let body = input.ir.functions.iter().find(|f| f.is_module_body);
        let body_alone = body.is_some_and(|f| cx0.infers_alone(f));
        let body_types = body.map(|f| cx0.infer_types(f));
        let body_reads = first_reads.consulted.take();
        let body_trace = first_reads.recorder.take();
        let declared = |name: &str| body_types.as_ref().and_then(|t| t.declared(name).cloned());
        let mut foreign = Foreign::default();
        let mut first: Vec<First> = Vec::with_capacity(fns.len());
        let mut changed: FxHashSet<&str> = FxHashSet::default();
        for s in &fns {
            let (types, alone, earlier) = match &body_types {
                Some(types) if s.f.is_module_body => {
                    first_reads.consulted.replace(body_reads.clone());
                    (Rc::clone(types), body_alone, &*body_trace)
                }
                _ => {
                    let alone = cx0.infers_alone(s.f);
                    (cx0.infer_types(s.f), alone, &[][..])
                }
            };
            let mut own = Joins::default();
            let classifier = Classifier::new(&cx0, decls, s, &types, &declared, &unfixed);
            classifier.classify(&mut own, Some(&mut foreign));
            changed.extend(classifier.changed_globals());
            let named = first_reads.consulted.take().into_iter().collect();
            let trace = first_reads.recorder.take();
            let trace = alone.then(|| earlier.iter().cloned().chain(trace).collect());
            first.push(First { own, named, trace });
        }
        let (decided, fallbacks) = decide(
            input,
            project,
            kb,
            decls,
            &fns,
            first,
            &declared,
            &unfixed,
            &mut foreign,
        );
        let tables = match (&body_types, fns.iter().find(|s| s.f.is_module_body)) {
            (Some(types), Some(body)) => tables(decls, body, types, &unfixed, &changed),
            _ => BTreeMap::new(),
        };
        let declaring: BTreeMap<String, Ty> = decided
            .globals
            .keys()
            .filter_map(|name| {
                let d = declared(name)?;
                (d.without_items() != d).then(|| (name.clone(), d))
            })
            .collect();
        (decided, fallbacks, foreign.into_decl(), (tables, declaring))
    };
    decided.write(decls);
    (decls.tables, decls.declared_items) = tables;
    decls.store_fallbacks = fallbacks.map(Box::new);
    decls.stores_elsewhere = foreign.map(Box::new);
}

/// Most entries a table keeps ([`ModuleDecls::tables`]).
const TABLE_ENTRIES: usize = 256;

/// [`ModuleDecls::tables`]: the module body's stores of a global it binds once (`body`, typed
/// by `types`), outside a loop, to a dict display with string-literal keys and no `**`
/// entry, one of whose values is a function, a class, or a `functools.partial` object, when
/// no function of the module changes the global's object in place (`changed`,
/// [`Classifier::changed_globals`]) and nothing may rebind the name (`unfixed`).
fn tables(
    decls: &ModuleDecls,
    body: &StoringFn<'_>,
    types: &Inferred,
    unfixed: &Unfixed<'_>,
    changed: &FxHashSet<&str>,
) -> BTreeMap<String, Table> {
    let mut out = BTreeMap::new();
    let stores = heki_ir::GlobalStores::of(std::slice::from_ref(body.f));
    let shape = &body.shape;
    for bb in body.f.blocks.iter().filter(|b| shape.loop_depth(b.id) == 0) {
        for inst in &bb.insts {
            let InstKind::StoreGlobal { name, value } = &inst.kind else {
                continue;
            };
            if !stores.once(name)
                || changed.contains(name.as_str())
                || !decls.unknown_globals.contains(name)
                || unfixed.rebinds(body, bb.id, inst, name)
            {
                continue;
            }
            let Some(InstKind::DictLiteral { entries, .. }) =
                shape.inst(shape.copy_root(*value)).map(|i| &i.kind)
            else {
                continue;
            };
            if entries.len() > TABLE_ENTRIES {
                continue;
            }
            let mut table: Vec<(Box<str>, Option<Ty>)> = Vec::with_capacity(entries.len());
            let mut keyed = true;
            for entry in entries {
                let heki_ir::DictEntry::KeyValue { key, value } = entry else {
                    keyed = false;
                    break;
                };
                let Some(key) = shape.str_const(*key) else {
                    keyed = false;
                    break;
                };
                // A later entry of the same key replaces the earlier one.
                table.retain(|(k, _)| **k != *key);
                table.push((Box::from(key), types.ty(*value).cloned()));
            }
            let callable = |t: &Option<Ty>| {
                matches!(
                    t,
                    Some(Ty::Ref(_) | Ty::OneOf(_) | Ty::Class(_) | Ty::Partial(_))
                )
            };
            if keyed && table.iter().any(|(_, t)| callable(t)) {
                out.insert(name.clone(), table.into_boxed_slice());
            }
        }
    }
    out
}

/// What the first round gives for one storing function.
struct First {
    own: Joins,
    /// The names being decided that its inference read.
    named: FxHashSet<Named>,
    /// The reads its inference made ([`Trace`]), when they are all it made: `None` when it ran
    /// another function's inference, which an earlier one may have run and kept.
    trace: Option<Trace>,
}

/// The types the module's stores give, from their first round (`first`), and the first round's
/// where they differ. What the functions reading names being decided store into other modules
/// joins `foreign`, through receivers typed as the decided types type them.
///
/// A function is inferred again only when a read it made gives something else: a visit of the
/// fixpoint first asks the reads of the function's last inference (the first round's, for the
/// first visit), and when each gives what it gave there, the function stores what it stored
/// then, and it stored the same into other modules.
#[allow(clippy::too_many_arguments)]
fn decide(
    input: &ModuleInput<'_>,
    project: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
    decls: &ModuleDecls,
    fns: &[StoringFn<'_>],
    first: Vec<First>,
    declared: &dyn Fn(&str) -> Option<Ty>,
    unfixed: &Unfixed<'_>,
    foreign: &mut Foreign,
) -> (Decided, Option<StoreFallbacks>) {
    let mut round0 = Joins::default();
    for f in &first {
        round0.join(&f.own);
    }
    let targets: FxHashSet<Named> = round0
        .every
        .keys()
        .chain(&round0.in_place)
        .map(|t| match t {
            StoreTarget::Global(name) => Named::Global(name.clone()),
            StoreTarget::Attr(_, attr) => Named::Attr(attr.clone()),
        })
        .collect();
    // The functions reading a name being decided are inferred again; the others' stores bind
    // what they bound.
    let nodes: Vec<usize> = (0..fns.len())
        .filter(|&i| !first[i].named.is_disjoint(&targets))
        .collect();
    if nodes.is_empty() {
        return (round0.decide(decls), None);
    }
    let mut fixed = Joins::default();
    let mut in_fixpoint = vec![false; fns.len()];
    for &i in &nodes {
        in_fixpoint[i] = true;
    }
    for (f, _) in first.iter().zip(&in_fixpoint).filter(|(_, node)| !**node) {
        fixed.join(&f.own);
    }
    let establishable: FxHashSet<StoreTarget> = round0
        .establishing
        .iter()
        .filter(|(_, s)| !matches!(s, Stored::Nothing))
        .map(|(t, _)| t.clone())
        .collect();
    let deciding = Deciding {
        project,
        module: &input.module_name,
        decls,
        establishable,
        joins: RefCell::new(fixed),
        families: RefCell::default(),
        distinct_families: RefCell::default(),
        consulted: RefCell::default(),
        pending: RefCell::default(),
        recorder: Recorder::default(),
    };
    let cx = TypeCtx::new(input, project, kb).reading_stores(&deciding);
    let readers_of: RefCell<FxHashMap<Key, FxHashSet<usize>>> = RefCell::default();
    let consulted_of: RefCell<Vec<FxHashSet<Key>>> =
        RefCell::new(vec![FxHashSet::default(); nodes.len()]);
    let written: RefCell<Vec<Vec<Key>>> = RefCell::new(vec![Vec::new(); nodes.len()]);
    let readers = |i: usize, out: &mut Vec<usize>| {
        let readers_of = readers_of.borrow();
        for key in &written.borrow()[i] {
            out.extend(readers_of.get(key).into_iter().flatten().copied());
        }
    };
    let spec = Sparse {
        nodes: nodes.len(),
        readers: &readers,
        max_visits: VISITS,
    };
    let budget = Budget::of_functions("store types", nodes.iter().map(|&i| fns[i].f));
    // The last inference of each function of the fixpoint: its reads and what it stored.
    let mut last: Vec<Option<(Trace, Joins)>> = nodes
        .iter()
        .map(|&i| Some((first[i].trace.clone()?, first[i].own.clone())))
        .collect();
    // Each visit also joins what the function stores into other modules: a receiver a read of
    // a name being decided types (`self.base.client = v`) is typed only here.
    let solved = fixpoint(&budget, spec, |i, _: &Facts<Joins>| {
        let own = match &last[i] {
            Some((trace, own)) if budget.take(trace.len()) && deciding.replays(trace) => {
                own.clone()
            }
            _ => {
                // The inference makes again the reads a replay stopping part way made.
                deciding.consulted.borrow_mut().clear();
                let own = infer_stores(&cx, decls, &fns[nodes[i]], declared, unfixed, foreign);
                last[i] = Some((deciding.recorder.take(), own.clone()));
                own
            }
        };
        written.borrow_mut()[i] = deciding.joins.borrow_mut().join_keys(&own);
        let mut readers_of = readers_of.borrow_mut();
        for key in deciding.consulted.take() {
            readers_of.entry(key.clone()).or_default().insert(i);
            consulted_of.borrow_mut()[i].insert(key);
        }
        Some(own)
    });
    // A read took a name for one no store binds yet, and no store ever bound it: what the read
    // gives is what the declarations give, which the fixpoint did not assume.
    let unbound = {
        let joins = deciding.joins.borrow();
        let pending = deciding.pending.borrow();
        pending
            .iter()
            .any(|t| matches!(joins.establishing.get(t), None | Some(Stored::Nothing)))
    };
    let slots = match solved {
        Answer::Known(slots) if !unbound => slots,
        _ => {
            // The first round's types hold: the visits may not have typed the receivers as
            // they do, so the stores into other modules are classified once more under them,
            // past the functions an inference with the same reads classified already.
            deciding.joins.replace(round0.clone());
            for (&i, last) in nodes.iter().zip(&last) {
                let inferred = [first[i].trace.as_deref(), last.as_ref().map(|(t, _)| &**t)];
                if !inferred
                    .into_iter()
                    .flatten()
                    .any(|trace| deciding.replays(trace))
                {
                    infer_stores(&cx, decls, &fns[i], declared, unfixed, foreign);
                    deciding.recorder.take();
                }
            }
            return (round0.decide(decls), None);
        }
    };
    drop(cx);
    let joins = deciding.joins.into_inner();
    let decided = joins.decide(decls);
    let mut fallback = joins;
    for &i in &nodes {
        fallback.join(&first[i].own);
    }
    let fallbacks = decided
        .fallbacks(&fallback.decide(decls))
        .map(|f| StoreFallbacks {
            dependencies: dependencies(&slots, consulted_of.into_inner()),
            ..f
        });
    (decided, fallbacks)
}

/// What `s` stores, inferred under what the reads of `cx` give now; what it stores into other
/// modules joins `foreign`.
fn infer_stores(
    cx: &TypeCtx<'_>,
    decls: &ModuleDecls,
    s: &StoringFn<'_>,
    declared: &dyn Fn(&str) -> Option<Ty>,
    unfixed: &Unfixed<'_>,
    foreign: &mut Foreign,
) -> Joins {
    cx.forget_inferred();
    let types = cx.infer_types(s.f);
    let mut own = Joins::default();
    Classifier::new(cx, decls, s, &types, declared, unfixed).classify(&mut own, Some(foreign));
    own
}

/// What each function of the fixpoint stores into (its fact, `slots`) and the names being
/// decided it read (`consulted`), for those that do both.
fn dependencies(slots: &Facts<Joins>, consulted: Vec<FxHashSet<Key>>) -> Box<[StoreDependency]> {
    let dependency = |(i, read): (usize, FxHashSet<Key>)| {
        let stores: Box<[StoreTarget]> = slots.get(i)?.every.keys().cloned().collect();
        let mut reads: Vec<StoreTarget> = read
            .into_iter()
            .filter_map(|k| match k {
                Key::Name(t) => Some(t),
                Key::Class(_) | Key::Globals => None,
            })
            .collect();
        reads.sort_unstable();
        let both = !(stores.is_empty() || reads.is_empty());
        both.then(|| StoreDependency {
            stores,
            reads: reads.into(),
        })
    };
    consulted
        .into_iter()
        .enumerate()
        .filter_map(dependency)
        .collect()
}

/// The reads of the first round, each giving what the declarations give: which names being
/// decided each function reads.
#[derive(Default)]
struct FirstReads {
    consulted: RefCell<Vec<Named>>,
    recorder: Recorder,
}

impl StoreReads for FirstReads {
    fn global(&self, name: &str) -> Option<Read> {
        self.recorder.read(QueryOf::Global(name), || {
            self.consulted
                .borrow_mut()
                .push(Named::Global(name.to_owned()));
            None
        })
    }

    fn member(&self, class: &str, attr: &str, skip_self: bool) -> Option<Read> {
        let query = QueryOf::Member(class, attr, skip_self);
        self.recorder.read(query, || {
            self.consulted
                .borrow_mut()
                .push(Named::Attr(attr.to_owned()));
            None
        })
    }
}

/// The reads of the fixpoint of [`decide`]: the types the stores joined so far give.
struct Deciding<'d> {
    /// Pass 1.
    project: &'d ProjectDecls,
    module: &'d str,
    /// The module's declarations, with their annotations.
    decls: &'d ModuleDecls,
    /// The names whose establishing stores bind a value in the first round.
    establishable: FxHashSet<StoreTarget>,
    joins: RefCell<Joins>,
    /// The canonical classes of the family of a class ([`ProjectDecls::instance_family`]),
    /// `None` when a member is a class of another module.
    families: RefCell<FxHashMap<String, Option<Rc<[String]>>>>,
    /// One copy of each family: the classes of a chain of subclasses share theirs.
    distinct_families: RefCell<FxHashSet<Rc<[String]>>>,
    /// What the function being inferred read of the names being decided.
    consulted: RefCell<Vec<Key>>,
    /// The names a read took for ones no store binds yet.
    pending: RefCell<FxHashSet<StoreTarget>>,
    /// The reads of the inference running.
    recorder: Recorder,
}

impl Deciding<'_> {
    /// Whether each read of `trace` gives what it gave, asked in its order up to the first that
    /// does not: an inference running now makes those reads too.
    fn replays(&self, trace: &[(Query, Option<Read>)]) -> bool {
        trace.iter().all(|(query, gave)| {
            let gives = match query {
                Query::Global(name) => self.global_now(name),
                Query::Member(class, attr, skip_self) => self.member_now(class, attr, *skip_self),
            };
            gives == *gave
        })
    }

    /// The qualname of the canonical class `class` when it is a class of the module.
    fn own_qualname<'c>(&self, class: &'c str) -> Option<&'c str> {
        let qualname = class.strip_prefix(self.module)?.strip_prefix('.')?;
        self.decls
            .classes
            .contains_key(qualname)
            .then_some(qualname)
    }

    fn family(&self, class: &str) -> Option<Rc<[String]>> {
        if let Some(known) = self.families.borrow().get(class) {
            return known.clone();
        }
        let family = self.project.instance_family(class);
        let own = family
            .iter()
            .all(|c| self.own_qualname(c).is_some())
            .then(|| {
                let family: Rc<[String]> = family.into();
                let mut distinct = self.distinct_families.borrow_mut();
                if let Some(known) = distinct.get(&family) {
                    return Rc::clone(known);
                }
                distinct.insert(Rc::clone(&family));
                family
            });
        self.families
            .borrow_mut()
            .insert(class.to_owned(), own.clone());
        own
    }

    /// What reading `attr` gives on an instance of a class of the family `family` (canonical
    /// fqns, all of this module): the join of what each stores in it, as
    /// [`ProjectDecls::join_family`] gives it.
    fn join_family(&self, joins: &Joins, family: &[String], attr: &str) -> Option<Ty> {
        let mut out = Stored::Nothing;
        let mut restoring = false;
        let mut in_place = false;
        for class in family {
            let qualname = self.own_qualname(class)?;
            let target = StoreTarget::Attr(qualname.to_owned(), attr.to_owned());
            let mut consulted = self.consulted.borrow_mut();
            consulted.push(Key::Class(qualname.to_owned()));
            consulted.push(Key::Name(target.clone()));
            drop(consulted);
            if joins.any.contains(qualname) {
                return None;
            }
            restoring |= joins.restoring.contains(qualname);
            in_place |= joins.in_place.contains(&target);
            match self.decls.classes[qualname].attr_types.get(attr) {
                Some(t) => out.add(Some(Some(t))),
                None => match joins
                    .every
                    .get(&StoreTarget::Attr(qualname.to_owned(), attr.to_owned()))
                {
                    Some(Stored::Open) => return None,
                    Some(stored) => {
                        out.join(stored);
                    }
                    None => {}
                },
            }
        }
        if restoring
            && !self
                .project
                .restores_hold(family, |key| self.getstate_of_self(key))
        {
            return None;
        }
        out.type_of().filter(|t| !in_place || keeps_in_place(t))
    }

    /// Whether the module's `__getstate__` `key` builds its state from the instance alone.
    fn getstate_of_self(&self, key: &crate::FuncKey) -> bool {
        key.module == self.module
            && self
                .decls
                .functions
                .get(&key.qualname)
                .is_some_and(|f| f.state_of_self)
    }
}

impl StoreReads for Deciding<'_> {
    fn global(&self, name: &str) -> Option<Read> {
        self.recorder
            .read(QueryOf::Global(name), || self.global_now(name))
    }

    fn member(&self, class: &str, attr: &str, skip_self: bool) -> Option<Read> {
        let query = QueryOf::Member(class, attr, skip_self);
        self.recorder
            .read(query, || self.member_now(class, attr, skip_self))
    }
}

impl Deciding<'_> {
    fn global_now(&self, name: &str) -> Option<Read> {
        let target = StoreTarget::Global(name.to_owned());
        if !self.establishable.contains(&target) {
            return None;
        }
        let mut consulted = self.consulted.borrow_mut();
        consulted.push(Key::Name(target.clone()));
        consulted.push(Key::Globals);
        drop(consulted);
        let joins = self.joins.borrow();
        match joins.establishing.get(&target) {
            None | Some(Stored::Nothing) => {
                self.pending.borrow_mut().insert(target);
                Some(Read::Pending)
            }
            Some(Stored::Open) => None,
            Some(Stored::Of(_)) => joins.established(&target).map(Read::Of),
        }
    }

    fn member_now(&self, class: &str, attr: &str, skip_self: bool) -> Option<Read> {
        let project = self.project;
        // As `ProjectDecls::member_type` walks it, past what another module decides.
        for c in project.mro(class).iter().skip(usize::from(skip_self)) {
            let Some(qualname) = self.own_qualname(c) else {
                if project.is_project_class(c) {
                    return None;
                }
                continue;
            };
            if project.property_at(c, attr) {
                return None;
            }
            if let Some(t) = self.decls.classes[qualname].attr_types.get(attr) {
                return Some(Read::Of(t.clone()));
            }
            let target = StoreTarget::Attr(qualname.to_owned(), attr.to_owned());
            if !self.establishable.contains(&target) {
                continue;
            }
            let mut consulted = self.consulted.borrow_mut();
            consulted.push(Key::Name(target.clone()));
            consulted.push(Key::Class(qualname.to_owned()));
            drop(consulted);
            let family = self.family(class)?;
            let joins = self.joins.borrow();
            return match joins.establishing.get(&target) {
                None | Some(Stored::Nothing) => {
                    self.pending.borrow_mut().insert(target);
                    Some(Read::Pending)
                }
                Some(Stored::Open) => None,
                Some(Stored::Of(_)) if joins.any.contains(qualname) => None,
                Some(Stored::Of(_)) => self.join_family(&joins, &family, attr).map(Read::Of),
            };
        }
        None
    }
}

fn insts(f: &IrFunction) -> impl Iterator<Item = &Inst> {
    f.blocks.iter().flat_map(|b| &b.insts)
}

/// The loads of `f` of a global or an attribute whose object `f` changes in place itself
/// ([`heki_ir::FnIndex::changed_vars`]: `HANDLERS[kind] = run`, `self.items.append(x)`), by
/// their place in the function's instructions.
fn changed_loads(input: &ModuleInput<'_>, f: &IrFunction) -> Box<[(BlockId, usize)]> {
    let changes = insts(f).any(|inst| {
        matches!(
            inst.kind,
            InstKind::SubscriptStore { .. } | InstKind::MethodCall { .. }
        )
    });
    if !changes {
        return Box::default();
    }
    let changed = heki_ir::FnIndex::new(f, &input.ir.constants).changed_vars();
    let mut out = Vec::new();
    for b in &f.blocks {
        for (i, inst) in b.insts.iter().enumerate() {
            let load = matches!(
                inst.kind,
                InstKind::LoadGlobal { .. } | InstKind::LoadName { .. } | InstKind::AttrLoad { .. }
            );
            if load && inst.kind.dst().is_some_and(|d| changed.contains(&d)) {
                out.push((b.id, i));
            }
        }
    }
    out.into_boxed_slice()
}

/// The functions of the module that may store into a global or into an attribute of an
/// instance (before their types are inferred): those with a global or attribute store, a load
/// of a namespace, or a call of `__setattr__` or of what `cx` may type as `setattr`,
/// `object.__setattr__`, or `vars`; and those changing in place what a load of a global or an
/// attribute gives ([`StoringFn::changed`]).
fn storing_functions<'f>(input: &'f ModuleInput<'_>, cx: &TypeCtx<'f>) -> Vec<StoringFn<'f>> {
    // Whether a module-level name may hold one of `STORE_BUILTINS`, by name.
    let named: RefCell<FxHashMap<&str, bool>> = RefCell::default();
    let names_builtin = |name: &'f str| {
        *named.borrow_mut().entry(name).or_insert_with(
            || matches!(cx.global(name), Ty::Ref(s) if STORE_BUILTINS.contains(&s.as_str())),
        )
    };
    let mut out = Vec::new();
    for f in &input.ir.functions {
        let shape = FnShape::new(f);
        let budget = Budget::of_function("store targets", f);
        let calls = Calls {
            cx,
            shape: &shape,
            budget: &budget,
            names_builtin: &names_builtin,
        };
        let changed = changed_loads(input, f);
        if changed.is_empty() && !insts(f).any(|inst| calls.may_store(&inst.kind)) {
            continue;
        }
        let selfs = is_summarized_function(f).then(|| self_of(cx, f)).flatten();
        let method = selfs.is_some() && f.owner_class_qualname.is_some();
        out.push(StoringFn {
            f,
            shape,
            changed,
            at_init: method && (f.name == "__init__" || f.name == "__post_init__"),
            state: (method && f.name == "__setstate__")
                .then(|| f.params.get(1).map(|p| p.var))
                .flatten(),
            selfs,
        });
    }
    // The module body first: its annotations type the stores of the globals they declare.
    out.sort_by_key(|s| !s.f.is_module_body);
    out
}

/// What [`storing_functions`] reads of the calls of a function.
struct Calls<'c, 'f> {
    cx: &'c TypeCtx<'c>,
    shape: &'c FnShape<'f>,
    budget: &'c Budget,
    /// Whether the module-level name may hold one of [`STORE_BUILTINS`].
    names_builtin: &'c dyn Fn(&'f str) -> bool,
}

impl<'f> Calls<'_, 'f> {
    /// Whether `kind` may store into a global or an attribute (what [`Classifier::classify`]
    /// reads, before the types are inferred).
    fn may_store(&self, kind: &InstKind) -> bool {
        match kind {
            InstKind::StoreGlobal { .. } | InstKind::AttrStore { .. } => true,
            InstKind::AttrLoad { attr, .. } => attr == "__dict__",
            // `builtins.setattr(obj, name, v)` calls the builtin through its module.
            InstKind::MethodCall { method, .. } => is_store_builtin_member(method),
            InstKind::Call {
                callee_var: Some(c),
                ..
            } => self.may_name_store_builtin(*c),
            _ => false,
        }
    }

    /// Whether the inference may type `callee` as one of [`STORE_BUILTINS`]: a name or an
    /// import the module's declarations resolve to one, an attribute of that name, a closure
    /// cell, or a copy or phi of one.
    fn may_name_store_builtin(&self, callee: VarId) -> bool {
        let shape = self.shape;
        let f: &'f IrFunction = shape.f;
        // Only phis and copies are expanded.
        let next = |v: VarId, out: &mut Vec<VarId>| match shape.def(v) {
            Def::Phi(b, i) => out.extend(
                f.blocks[b.0 as usize].phis[i as usize]
                    .incoming
                    .iter()
                    .map(|(_, x)| *x),
            ),
            _ => out.push(shape.copy_root(v)),
        };
        let visit = |v: VarId| {
            let found = match (shape.def(v), shape.inst(v)) {
                (Def::Phi(..), _) => return Visit::Expand,
                (_, Some(inst)) => match &inst.kind {
                    InstKind::Copy { .. } => return Visit::Expand,
                    InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. } => {
                        (self.names_builtin)(name)
                    }
                    InstKind::Import { .. } => matches!(
                        self.cx.import_type(inst.span),
                        Some(Ty::Ref(s)) if STORE_BUILTINS.contains(&s.as_str())
                    ),
                    InstKind::AttrLoad { attr, .. } => is_store_builtin_member(attr),
                    InstKind::LoadDeref { .. } => true,
                    _ => false,
                },
                (_, None) => false,
            };
            if found { Visit::Stop(()) } else { Visit::Prune }
        };
        !matches!(
            reach(self.budget, [callee], next, visit),
            Answer::Known(None)
        )
    }
}

/// The builtins a call of which may store an attribute or load a namespace.
const STORE_BUILTINS: &[&str] = &[
    "builtins.setattr",
    "builtins.object.__setattr__",
    "builtins.vars",
];

/// Whether an attribute or a method named `name` may be one of [`STORE_BUILTINS`] read from
/// the module or class holding it (`builtins.setattr`, `object.__setattr__`).
fn is_store_builtin_member(name: &str) -> bool {
    STORE_BUILTINS
        .iter()
        .any(|b| b.rsplit_once('.').is_some_and(|(_, member)| member == name))
}

/// The class whose instance `f` stores into as `self`, and the variables holding it: a method's
/// first parameter, or in a scope nested in methods the closure cell of the first parameter of
/// the innermost one whose `self` it reads ([`TypeCtx::captured_param`]), each with their copies.
fn self_of<'f>(cx: &TypeCtx<'f>, f: &'f IrFunction) -> Option<(&'f str, FxHashSet<VarId>)> {
    if f.owner_class_qualname.is_some() {
        return Some((self_class(f)?, self_aliases(f, f.params.first()?.var)));
    }
    let reads: Vec<(&IrFunction, VarId)> = insts(f)
        .filter_map(|inst| match &inst.kind {
            InstKind::LoadDeref { dst, name } => match cx.captured_param(f, name)? {
                (method, 0) if self_class(method).is_some() => Some((method, *dst)),
                _ => None,
            },
            _ => None,
        })
        .collect();
    // The methods enclosing `f` enclose each other: the innermost has the longest qualname.
    let (method, _) = *reads.iter().max_by_key(|(m, _)| m.qualname.len())?;
    let cells = reads
        .iter()
        .filter(|(m, _)| m.id == method.id)
        .map(|(_, v)| *v);
    Some((self_class(method)?, aliases_of(f, cells)))
}

/// The class of a method called with its instance as the first parameter (no `staticmethod`
/// or `classmethod`).
fn self_class(f: &IrFunction) -> Option<&str> {
    let summarized_method = is_summarized_function(f)
        && !f.params.is_empty()
        && !f.has_decorator(&["staticmethod", "classmethod"]);
    f.owner_class_qualname
        .as_deref()
        .filter(|_| summarized_method)
}

/// What a receiver holds, for the stores through it.
enum Holder<'h> {
    /// An instance of a class of the module, by qualname: the method's `self`, or another.
    Own { class: &'h str, own_self: bool },
    /// An instance of a class of another module, by fqn.
    Class(String),
    /// A project module, by name.
    Module(String),
}

/// The name an attribute operation names: one a string constant gives, or one heki cannot
/// read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AttrName<'f> {
    Named(Cow<'f, str>),
    Unknown,
}

/// An operation on the attributes of an object, however it is spelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AttrOp<'f> {
    /// A store of `value` in the attribute `name` of `obj`: `obj.attr = v`,
    /// `setattr(obj, name, v)`, `object.__setattr__(obj, name, v)`, `obj.__setattr__(name, v)`
    /// (`super().__setattr__(name, v)`), also through an alias of the builtin or its module
    /// (`builtins.setattr(obj, name, v)`). Replacing the namespace or the class
    /// (`obj.__dict__ = state`, `obj.__class__ = C`, or either through `setattr`) stores every
    /// attribute (`AttrName::Unknown`).
    Store {
        obj: VarId,
        name: AttrName<'f>,
        value: VarId,
    },
    /// A deletion: `del obj.attr`, `delattr(obj, name)`, `object.__delattr__(obj, name)`,
    /// `obj.__delattr__(name)`.
    Delete { obj: VarId, name: AttrName<'f> },
    /// A load of the namespace of `obj` into `dst` ([`super::namespace_load`]).
    Namespace { dst: VarId, obj: VarId },
}

/// What `kind` does to the attributes of an object: the one recognition of the stores and
/// deletions of attributes through every spelling. `val` gives what a variable holds (the
/// builtins by their symbol, as call resolution names them); `name` the string constant a
/// variable holds ([`TypeCtx::str_constant_with`]). A method `__setattr__` or `__delattr__`
/// takes the object first when called on a class (`object.__setattr__(obj, name, v)`), as its
/// number of arguments tells.
pub(crate) fn attr_op<'f, 'v>(
    kind: &'f InstKind,
    val: impl Fn(VarId) -> Option<&'v crate::resolver::Val>,
    name: impl Fn(VarId) -> Option<String>,
) -> Option<AttrOp<'f>> {
    let named = |v: VarId| name(v).map_or(AttrName::Unknown, |n| AttrName::Named(Cow::Owned(n)));
    let stored = |v: VarId| replacing_all(named(v));
    const ATTR_BUILTINS: &[&str] = &[
        "builtins.setattr",
        "builtins.object.__setattr__",
        "builtins.delattr",
        "builtins.object.__delattr__",
    ];
    match super::plain_call(kind, ATTR_BUILTINS, &val) {
        Some(("builtins.setattr" | "builtins.object.__setattr__", _, [obj, n, value])) => {
            return Some(AttrOp::Store {
                obj: *obj,
                name: stored(*n),
                value: *value,
            });
        }
        Some(("builtins.delattr" | "builtins.object.__delattr__", _, [obj, n])) => {
            return Some(AttrOp::Delete {
                obj: *obj,
                name: named(*n),
            });
        }
        _ => {}
    }
    match kind {
        InstKind::AttrStore { obj, attr, value } => Some(AttrOp::Store {
            obj: *obj,
            name: replacing_all(AttrName::Named(Cow::Borrowed(attr.as_str()))),
            value: *value,
        }),
        InstKind::AttrDelete { obj, attr } => Some(AttrOp::Delete {
            obj: *obj,
            name: AttrName::Named(Cow::Borrowed(attr.as_str())),
        }),
        InstKind::MethodCall {
            recv,
            method,
            args,
            kwargs,
            star_from: None,
            ..
        } if kwargs.is_empty() => match (method.as_str(), args.as_slice()) {
            ("__setattr__", [n, value]) => Some(AttrOp::Store {
                obj: *recv,
                name: stored(*n),
                value: *value,
            }),
            ("__setattr__", [obj, n, value]) => Some(AttrOp::Store {
                obj: *obj,
                name: stored(*n),
                value: *value,
            }),
            ("__delattr__", [n]) => Some(AttrOp::Delete {
                obj: *recv,
                name: named(*n),
            }),
            ("__delattr__", [obj, n]) => Some(AttrOp::Delete {
                obj: *obj,
                name: named(*n),
            }),
            _ => namespace_op_load(kind, &val),
        },
        _ => namespace_op_load(kind, &val),
    }
}

/// The name a store binds: one replacing the namespace or the class (`__dict__`, `__class__`)
/// stores every attribute, whichever spelling stores it.
fn replacing_all(name: AttrName<'_>) -> AttrName<'_> {
    match name {
        AttrName::Named(n) if n == "__dict__" || n == "__class__" => AttrName::Unknown,
        name => name,
    }
}

fn namespace_op_load<'f, 'v>(
    kind: &InstKind,
    val: &impl Fn(VarId) -> Option<&'v crate::resolver::Val>,
) -> Option<AttrOp<'f>> {
    let (dst, obj) = super::namespace_load(kind, val)?;
    Some(AttrOp::Namespace { dst, obj })
}

/// What an operation reading a namespace does to the attributes it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NamespaceOp<'f> {
    /// Reads, formats, or compares it.
    Reads,
    /// Stores `value` under a key: `ns[key] = v`, `ns.setdefault(key, v)`, `ns.__setitem__`.
    Stores { name: AttrName<'f>, value: VarId },
    /// Stores the keywords of `ns.update(a=v)` (`None`: a `**` unpacking).
    Updates(&'f [(Option<String>, VarId)]),
    /// Removes the attribute under a key: `del ns[key]`, `ns.pop(key)`.
    Removes(AttrName<'f>),
    /// Removes every attribute: `ns.clear()`, `ns.popitem()`.
    RemovesAll,
    /// Stores attributes heki cannot name, the items of `value` when one is known
    /// (`ns.update(other)`).
    StoresAny { value: Option<VarId> },
    /// Hands the namespace on (passed, stored, operated on): anything may happen to it.
    HandsOn,
}

/// The methods of a namespace that store nothing in it.
const NAMESPACE_READS: &[&str] = &[
    "get",
    "keys",
    "values",
    "items",
    "copy",
    "__contains__",
    "__getitem__",
    "__len__",
    "__iter__",
];

/// What the instruction `kind`, which reads a namespace held by the variables `ns`, does
/// through it (`name` as for [`attr_op`]).
pub(crate) fn namespace_op<'f>(
    kind: &'f InstKind,
    ns: &FxHashSet<VarId>,
    name: impl Fn(VarId) -> Option<String>,
) -> NamespaceOp<'f> {
    let named = |v: VarId| name(v).map_or(AttrName::Unknown, |n| AttrName::Named(Cow::Owned(n)));
    match kind {
        InstKind::Copy { .. }
        | InstKind::Compare { .. }
        | InstKind::Convert { .. }
        | InstKind::Format { .. }
        | InstKind::BuildString { .. } => NamespaceOp::Reads,
        InstKind::SubscriptLoad { obj, index, .. } if ns.contains(obj) && !ns.contains(index) => {
            NamespaceOp::Reads
        }
        InstKind::SubscriptDelete { obj, index, .. } if ns.contains(obj) && !ns.contains(index) => {
            NamespaceOp::Removes(named(*index))
        }
        InstKind::SubscriptStore {
            obj, index, value, ..
        } if ns.contains(obj) && !ns.contains(value) && !ns.contains(index) => {
            NamespaceOp::Stores {
                name: named(*index),
                value: *value,
            }
        }
        InstKind::MethodCall {
            recv,
            method,
            args,
            kwargs,
            ..
        } if ns.contains(recv)
            && !args.iter().any(|a| ns.contains(a))
            && !kwargs.iter().any(|(_, v)| ns.contains(v)) =>
        {
            match (method.as_str(), args.as_slice(), kwargs.is_empty()) {
                ("setdefault" | "__setitem__", [key, value], true) => NamespaceOp::Stores {
                    name: named(*key),
                    value: *value,
                },
                // A default of `None`, on which every call raises.
                ("setdefault", [_], true) => NamespaceOp::Reads,
                ("update", [], false) => NamespaceOp::Updates(kwargs),
                ("update", [other], true) => NamespaceOp::StoresAny {
                    value: Some(*other),
                },
                ("pop" | "__delitem__", [key, ..], true) => NamespaceOp::Removes(named(*key)),
                ("clear" | "popitem", [], true) => NamespaceOp::RemovesAll,
                (m, _, true) if NAMESPACE_READS.contains(&m) => NamespaceOp::Reads,
                _ => NamespaceOp::StoresAny { value: None },
            }
        }
        _ => NamespaceOp::HandsOn,
    }
}

/// The stores of one function, with its inferred types.
struct Classifier<'c, 'f> {
    cx: &'c TypeCtx<'c>,
    decls: &'c ModuleDecls,
    s: &'c StoringFn<'f>,
    types: &'c Inferred,
    /// The inferred values: the callees, and the string constants naming attributes.
    vals: FxHashMap<VarId, crate::resolver::Val>,
    declared: &'c dyn Fn(&str) -> Option<Ty>,
    unfixed: &'c Unfixed<'c>,
    budget: Budget,
}

impl<'c, 'f> Classifier<'c, 'f> {
    fn new(
        cx: &'c TypeCtx<'c>,
        decls: &'c ModuleDecls,
        s: &'c StoringFn<'f>,
        types: &'c Inferred,
        declared: &'c dyn Fn(&str) -> Option<Ty>,
        unfixed: &'c Unfixed<'c>,
    ) -> Self {
        Self {
            cx,
            decls,
            s,
            types,
            vals: types.vals(),
            declared,
            unfixed,
            budget: Budget::of_function("store targets", s.f),
        }
    }

    /// Join the function's stores into the module's (`own`) and, when given, into what the
    /// module stores into other modules (`foreign`).
    fn classify(&self, own: &mut Joins, foreign: Option<&mut Foreign>) {
        let f = self.s.f;
        let mut out = Sink { own, foreign };
        let mut namespaces: Vec<(VarId, VarId)> = Vec::new();
        let blocks = f.blocks.iter();
        for (block, inst) in blocks.flat_map(|b| b.insts.iter().map(move |inst| (b.id, inst))) {
            if let InstKind::StoreGlobal { name, value } = &inst.kind {
                let target = StoreTarget::Global(name.clone());
                let loads_it = |k: &InstKind| matches!(k, InstKind::LoadGlobal { name: n, .. } | InstKind::LoadName { name: n, .. } if n == name);
                if self.in_place(*value, loads_it) {
                    out.own.in_place.insert(target);
                    continue;
                }
                let rebinds = self.unfixed.rebinds(self.s, block, inst, name);
                let value = self.global_value(name, *value, rebinds);
                let value = value.as_ref().map(Option::as_ref);
                out.own.store(target, f.is_module_body, value);
                continue;
            }
            let op = attr_op(&inst.kind, |v| self.vals.get(&v), |v| self.name_of(v));
            match op {
                Some(AttrOp::Store {
                    obj,
                    name: AttrName::Named(attr),
                    value,
                }) => {
                    let shape = &self.s.shape;
                    let loads_it = |k: &InstKind| matches!(k, InstKind::AttrLoad { obj: o, attr: a, .. } if *a == attr && shape.copy_root(*o) == shape.copy_root(obj));
                    if self.in_place(value, loads_it) {
                        self.named_in_place(&mut out, obj, &attr);
                    } else {
                        self.named(&mut out, obj, &attr, value);
                    }
                }
                Some(AttrOp::Store {
                    obj,
                    name: AttrName::Unknown,
                    value,
                }) => self.unnamed(&mut out, obj, Some(value)),
                Some(AttrOp::Namespace { dst, obj }) => namespaces.push((dst, obj)),
                // A deletion binds no value: a read after it raises, or finds what the class
                // holds.
                Some(AttrOp::Delete { .. }) | None => {}
            }
        }
        for (ns, obj) in namespaces {
            self.namespace(&mut out, ns, obj);
        }
        for &(block, i) in &self.s.changed {
            self.changed_in_place(&mut out, &f.block(block).insts[i]);
        }
    }

    /// The module's globals whose object the function changes in place
    /// ([`StoringFn::changed`]), by any spelling of the receiver: the name
    /// (`HANDLERS[kind] = run`), or an attribute of the module's own object, which the module
    /// imports (`tables.HANDLERS[kind] = run`).
    fn changed_globals(&self) -> Vec<&'f str> {
        let module = self.cx.module_name();
        let own_module = |obj: VarId| {
            (self.holders(obj).iter()).any(|h| matches!(h, Holder::Module(m) if m == module))
        };
        let loads = self.s.changed.iter();
        let loads = loads.map(|&(block, i)| &self.s.f.block(block).insts[i].kind);
        loads
            .filter_map(|load| match load {
                InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. } => {
                    Some(name.as_str())
                }
                InstKind::AttrLoad { obj, attr, .. } if own_module(*obj) => Some(attr.as_str()),
                _ => None,
            })
            .collect()
    }

    /// A change in place of the object the load `load` gives (an item store, a method call
    /// that may add items): a store, into the name the load reads, of the
    /// object's class with its items unknown, or of an annotated global's declaration. The name
    /// keeps its class and no items the code may add or replace (`HANDLERS = {"a": run}`, then
    /// `HANDLERS[kind] = other`). A global read through the module's own object, which types it
    /// by its name alone (`tables.HANDLERS[kind] = other` in a module importing itself), keeps
    /// its declaration or no type.
    fn changed_in_place(&self, out: &mut Sink<'_>, load: &Inst) {
        // The loaded value's class with its items unknown, when it has items; another module's
        // names are marked instead, decided by their own module's stores.
        let loaded = load.kind.dst().and_then(|d| self.types.ty(d));
        let without_items = loaded
            .map(|t| (t.without_items(), t))
            .and_then(|(changed, t)| (changed != *t).then_some(changed));
        let value = without_items.as_ref().map(Some);
        let module = self.cx.module_name();
        match &load.kind {
            InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. } => {
                if let Some(fqn) = self.decls.fixed_import(name) {
                    if let Some(foreign) = out.foreign.as_deref_mut() {
                        let fqn = self.cx.project.canonical(fqn);
                        foreign.globals.entry(fqn).or_default().changed = true;
                    }
                } else if self.decls.unknown_globals.contains(name) {
                    // An annotation types the items any change adds, as it types every store.
                    let declared = (self.declared)(name);
                    if let Some(value) = declared.as_ref().map(Some).or(value) {
                        let target = StoreTarget::Global(name.clone());
                        out.own.store(target, false, Some(value));
                    }
                }
            }
            InstKind::AttrLoad { obj, attr, .. } => {
                for holder in self.holders(*obj) {
                    match holder {
                        Holder::Own { class, own_self } => {
                            if let Some(value) = value {
                                let target = StoreTarget::Attr(class.to_owned(), attr.to_owned());
                                out.own
                                    .store(target, own_self && self.s.at_init, Some(value));
                            }
                        }
                        Holder::Module(m) if m == module => {
                            let declared = (self.declared)(attr);
                            let named_only = loaded.is_none_or(|t| matches!(t, Ty::Ref(_)));
                            let known = declared.as_ref().map(Some).or(value);
                            let value = known.or(named_only.then_some(None));
                            if let Some(value) = value {
                                let target = StoreTarget::Global(attr.clone());
                                out.own.store(target, false, Some(value));
                            }
                        }
                        Holder::Module(m) => {
                            if let Some(foreign) = out.foreign.as_deref_mut() {
                                let fqn = format!("{m}.{attr}");
                                foreign.globals.entry(fqn).or_default().changed = true;
                            }
                        }
                        Holder::Class(class) => {
                            if let Some(foreign) = out.foreign.as_deref_mut() {
                                let attrs = foreign.attrs.entry(class).or_default();
                                attrs.entry(attr.clone()).or_default().changed = true;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Whether `value` is what an in-place operator gives on the value a load `loads_it`
    /// accepts reads (`x += v`, `obj.attr |= v`).
    fn in_place(&self, value: VarId, loads_it: impl Fn(&InstKind) -> bool) -> bool {
        let shape = &self.s.shape;
        let Some(InstKind::BinOp {
            in_place: true,
            lhs,
            ..
        }) = shape.inst(shape.copy_root(value)).map(|i| &i.kind)
        else {
            return false;
        };
        shape
            .inst(shape.copy_root(*lhs))
            .is_some_and(|i| loads_it(&i.kind))
    }

    /// A store rebinding the attribute `attr` of what `obj` holds to what an in-place operator
    /// gives on its own value.
    fn named_in_place(&self, out: &mut Sink<'_>, obj: VarId, attr: &str) {
        let module = self.cx.module_name();
        for holder in self.holders(obj) {
            match holder {
                Holder::Own { class, .. } => {
                    let target = StoreTarget::Attr(class.to_owned(), attr.to_owned());
                    out.own.in_place.insert(target);
                }
                Holder::Module(m) if m == module => {
                    out.own
                        .in_place
                        .insert(StoreTarget::Global(attr.to_owned()));
                }
                Holder::Module(m) => {
                    if let Some(foreign) = out.foreign.as_deref_mut() {
                        foreign
                            .globals
                            .entry(format!("{m}.{attr}"))
                            .or_default()
                            .in_place = true;
                    }
                }
                Holder::Class(class) => {
                    if let Some(foreign) = out.foreign.as_deref_mut() {
                        let attrs = foreign.attrs.entry(class).or_default();
                        attrs.entry(attr.to_owned()).or_default().in_place = true;
                    }
                }
            }
        }
    }

    /// What a store of `value` in the global `name` binds: of unknown type when the module may
    /// rebind it (`rebinds`); its declaration, refined by what the value says beyond it
    /// ([`ProjectDecls::refine`]), when the module annotates it.
    fn global_value(&self, name: &str, value: VarId, rebinds: bool) -> Option<Option<Ty>> {
        let mut out = self.types.stored(value).map(|t| t.cloned());
        if rebinds {
            out = Some(None);
        }
        if let Some(d) = (self.declared)(name) {
            out = Some(Some(match out {
                Some(Some(t)) => self.cx.project.refine(&t, &d),
                _ => d,
            }));
        }
        out
    }

    /// The string constant `var` holds ([`TypeCtx::str_constant_in`]: a literal, a string
    /// constant of a module or a class).
    fn name_of(&self, var: VarId) -> Option<String> {
        if !self.budget.take(1) {
            return None;
        }
        self.cx.str_constant_in(&self.s.shape, var, &self.vals)
    }

    /// What the receiver `obj` holds.
    fn holders(&self, obj: VarId) -> Vec<Holder<'c>> {
        if let Some((class, selfs)) = &self.s.selfs
            && selfs.contains(&obj)
        {
            return vec![Holder::Own {
                class,
                own_self: true,
            }];
        }
        match self.types.ty(obj) {
            Some(Ty::Inst(o)) => self.instance(&o.class).into_iter().collect(),
            Some(Ty::Union(os)) => os.iter().filter_map(|o| self.instance(&o.class)).collect(),
            // `super()` in a method stands for its `self`.
            Some(Ty::Super(_)) => self
                .s
                .selfs
                .as_ref()
                .map(|(class, _)| Holder::Own {
                    class,
                    own_self: true,
                })
                .into_iter()
                .collect(),
            Some(Ty::Ref(m)) if self.cx.project.module(m).is_some() => {
                vec![Holder::Module(self.cx.project.canonical(m))]
            }
            _ => Vec::new(),
        }
    }

    /// The holder of an instance of the class `class`: a project class that is no protocol,
    /// whose implementations need not be its subclasses.
    fn instance(&self, class: &str) -> Option<Holder<'c>> {
        let project = self.cx.project;
        if !project.is_project_class(class) || project.protocol(class).is_some() {
            return None;
        }
        let class = project.canonical(class);
        let own = class
            .strip_prefix(self.cx.module_name())
            .and_then(|rest| rest.strip_prefix('.'))
            .and_then(|q| self.decls.classes.get_key_value(q));
        Some(match own {
            Some((qualname, _)) => Holder::Own {
                class: qualname,
                own_self: false,
            },
            None => Holder::Class(class),
        })
    }

    /// A store of `value` in the attribute `attr` of what `obj` holds.
    fn named(&self, out: &mut Sink<'_>, obj: VarId, attr: &str, value: VarId) {
        let global = self.global_value(attr, value, self.unfixed.dynamic);
        self.store_through(out, obj, attr, self.types.stored(value), global);
    }

    /// A store of a value of type `value` in the attribute `attr` of what `obj` holds;
    /// `global` is what it binds when `obj` is the module itself.
    fn store_through(
        &self,
        out: &mut Sink<'_>,
        obj: VarId,
        attr: &str,
        value: Option<Option<&Ty>>,
        global: Option<Option<Ty>>,
    ) {
        let module = self.cx.module_name();
        for holder in self.holders(obj) {
            match holder {
                Holder::Own { class, own_self } => {
                    let target = StoreTarget::Attr(class.to_owned(), attr.to_owned());
                    let establishing = own_self && self.s.at_init;
                    out.own.store(target, establishing, value);
                }
                Holder::Module(m) if m == module => {
                    out.own.store(
                        StoreTarget::Global(attr.to_owned()),
                        false,
                        global.as_ref().map(Option::as_ref),
                    );
                }
                Holder::Module(m) => {
                    if let Some(foreign) = out.foreign.as_deref_mut() {
                        let stored = foreign.globals.entry(format!("{m}.{attr}")).or_default();
                        stored.stored.add(value);
                    }
                }
                Holder::Class(class) => {
                    if let Some(foreign) = out.foreign.as_deref_mut() {
                        let stored = foreign
                            .attrs
                            .entry(class)
                            .or_default()
                            .entry(attr.to_owned())
                            .or_default();
                        stored.stored.add(value);
                    }
                }
            }
        }
    }

    /// A store of an attribute heki cannot name of what `obj` holds, binding `value` when one
    /// value is known (`__setstate__` restores from its state).
    fn unnamed(&self, out: &mut Sink<'_>, obj: VarId, value: Option<VarId>) {
        for holder in self.holders(obj) {
            match holder {
                Holder::Own { class, own_self } => {
                    let restores = own_self && value.is_some_and(|v| self.holds_state(v));
                    if restores {
                        out.own.restoring.insert(class.to_owned());
                    } else {
                        out.own.any.insert(class.to_owned());
                    }
                }
                Holder::Module(m) if m == self.cx.module_name() => out.own.any_global = true,
                Holder::Module(fqn) | Holder::Class(fqn) => {
                    if let Some(foreign) = out.foreign.as_deref_mut() {
                        foreign.unnamed.insert(fqn);
                    }
                }
            }
        }
    }

    /// The stores through the namespace `ns` of what `obj` holds (`obj.__dict__`, `vars(obj)`,
    /// and their copies) of the attributes a string constant names (`ns["attr"] = v`,
    /// `ns.setdefault("attr", v)`, `ns.update(attr=v)`); one under another key,
    /// `ns.update(other)`, and a namespace written otherwise or handed on (passed, returned,
    /// merged) store any attribute. Reads, deletions, formatting, and comparisons store nothing.
    fn namespace(&self, out: &mut Sink<'_>, ns: VarId, obj: VarId) {
        let shape = &self.s.shape;
        let f = self.s.f;
        let mut held: FxHashSet<VarId> = FxHashSet::default();
        let next = |v: VarId, out: &mut Vec<VarId>| {
            for u in shape.readers(v) {
                match *u {
                    Use::Phi(b, i) => out.push(f.blocks[b.0 as usize].phis[i as usize].dst),
                    Use::Inst(b, i) => {
                        let kind = &f.blocks[b.0 as usize].insts[i as usize].kind;
                        if let InstKind::Copy { dst, .. } = kind {
                            out.push(*dst);
                        }
                    }
                    Use::Terminator(_) => {}
                }
            }
        };
        let complete = reach(&self.budget, [ns], next, |v| {
            held.insert(v);
            Visit::<()>::Expand
        });
        if complete.is_open() {
            return self.unnamed(out, obj, None);
        }
        let mut named: Vec<(String, VarId)> = Vec::new();
        let mut unnamed: Vec<Option<VarId>> = Vec::new();
        let mut sorted: Vec<VarId> = held.iter().copied().collect();
        sorted.sort_unstable();
        for v in &sorted {
            for u in shape.readers(*v) {
                if !self.budget.take(1) {
                    return self.unnamed(out, obj, None);
                }
                match *u {
                    Use::Phi(b, i) => {
                        let phi = &f.blocks[b.0 as usize].phis[i as usize];
                        if !phi.incoming.iter().all(|(_, x)| held.contains(x)) {
                            unnamed.push(None);
                        }
                    }
                    Use::Terminator(b) => {
                        let branch =
                            matches!(f.blocks[b.0 as usize].terminator, Terminator::Branch { .. });
                        if !branch {
                            unnamed.push(None);
                        }
                    }
                    Use::Inst(b, i) => {
                        let kind = &f.blocks[b.0 as usize].insts[i as usize].kind;
                        self.namespace_use(kind, &held, &mut named, &mut unnamed);
                    }
                }
            }
        }
        for (attr, value) in named {
            self.named(out, obj, &attr, value);
        }
        for value in unnamed {
            self.unnamed(out, obj, value);
        }
    }

    /// What `kind`, which reads a namespace (`held`), stores through it ([`namespace_op`]).
    fn namespace_use(
        &self,
        kind: &InstKind,
        held: &FxHashSet<VarId>,
        named: &mut Vec<(String, VarId)>,
        unnamed: &mut Vec<Option<VarId>>,
    ) {
        match namespace_op(kind, held, |v| self.name_of(v)) {
            NamespaceOp::Reads | NamespaceOp::Removes(_) | NamespaceOp::RemovesAll => {}
            NamespaceOp::Stores {
                name: AttrName::Named(attr),
                value,
            } => named.push((attr.into_owned(), value)),
            NamespaceOp::Stores {
                name: AttrName::Unknown,
                value,
            } => unnamed.push(Some(value)),
            NamespaceOp::Updates(keywords) => {
                for (keyword, value) in keywords {
                    match keyword {
                        Some(attr) => named.push((attr.clone(), *value)),
                        None => unnamed.push(Some(*value)),
                    }
                }
            }
            NamespaceOp::StoresAny { value } => unnamed.push(value),
            NamespaceOp::HandsOn => unnamed.push(None),
        }
    }

    /// Whether `v` holds the state `__setstate__` receives, or part of it: through copies,
    /// iteration, items, `dict(state)`, `state.pop(key)`, and the reads of [`NAMESPACE_READS`]
    /// (`state.items()`).
    fn holds_state(&self, v: VarId) -> bool {
        let shape = &self.s.shape;
        let f = self.s.f;
        let next = |x: VarId, out: &mut Vec<VarId>| match shape.def(x) {
            Def::Phi(b, i) => out.extend(
                f.blocks[b.0 as usize].phis[i as usize]
                    .incoming
                    .iter()
                    .map(|(_, y)| *y),
            ),
            _ => out.extend(shape.inst(x).and_then(|i| state_operand(&i.kind))),
        };
        let visit = |x: VarId| {
            if Some(x) == self.s.state {
                return Visit::Prune;
            }
            let kind = shape.inst(x).map(|i| &i.kind);
            let reads_state = match kind {
                Some(InstKind::MethodCall { method, .. }) => {
                    NAMESPACE_READS.contains(&method.as_str()) || method == "pop"
                }
                Some(InstKind::Call {
                    callee_var: Some(c),
                    args,
                    ..
                }) => {
                    args.len() == 1
                        && matches!(self.types.ty(*c), Some(Ty::Ref(d)) if d == "builtins.dict")
                }
                kind => kind.is_some_and(|k| state_operand(k).is_some()),
            };
            if reads_state || matches!(shape.def(x), Def::Phi(..)) {
                Visit::Expand
            } else {
                Visit::Stop(())
            }
        };
        self.s.state.is_some() && reach(&self.budget, [v], next, visit) == Answer::Known(None)
    }
}

/// The operand a value [`Classifier::holds_state`] follows comes from: what is copied,
/// iterated, or subscripted, the receiver of a method, the argument of a call.
fn state_operand(kind: &InstKind) -> Option<VarId> {
    match kind {
        InstKind::Copy { src: from, .. }
        | InstKind::IterNext { iter: from, .. }
        | InstKind::MakeIter { iterable: from, .. }
        | InstKind::SubscriptLoad { obj: from, .. }
        | InstKind::MethodCall { recv: from, .. } => Some(*from),
        InstKind::Call { args, .. } => args.first().copied(),
        _ => None,
    }
}

/// Where the stores of a function go.
struct Sink<'s> {
    own: &'s mut Joins,
    foreign: Option<&'s mut Foreign>,
}

/// [`ModuleDecls::exports`]: the module's `__all__`, when the module body binds it once, on
/// every path that finishes it ([`LastBindings::always_runs`]: where it stays unbound, a star
/// import binds every public name), to a tuple of string literals
/// ([`heki_ir::ModuleConstants::str_tuples`]), or to a list of them nothing changes: the module
/// body uses the list only to bind `__all__` ([`binds_only`]), and no function of the module
/// grows it ([`heki_ir::ModuleConstants::displays`]).
pub(super) fn exported_names(input: &ModuleInput<'_>) -> Option<Box<[String]>> {
    let constants = &input.ir.constants;
    let tuple = constants.str_tuples.get("__all__");
    if tuple.is_none() && !constants.displays.contains_key("__all__") {
        return None;
    }
    let body = input.ir.functions.iter().find(|f| f.is_module_body)?;
    let shape = FnShape::new(body);
    let (block, value) = body.blocks.iter().find_map(|b| {
        b.insts.iter().find_map(|inst| match &inst.kind {
            InstKind::StoreGlobal { name, value } if name == "__all__" => Some((b.id, *value)),
            _ => None,
        })
    })?;
    if !LastBindings::of(&shape).always_runs(&shape, block) {
        return None;
    }
    if let Some(names) = tuple {
        return Some(names.iter().map(|n| n.to_string()).collect());
    }
    let list = shape.copy_root(value);
    let Some(InstKind::AllocBuiltin {
        kind: BuiltinKind::List,
        elems,
        ..
    }) = shape.inst(list).map(|i| &i.kind)
    else {
        return None;
    };
    if !binds_only(body, list, "__all__") {
        return None;
    }
    elems
        .iter()
        .map(|e| shape.str_const(shape.copy_root(*e)).map(str::to_owned))
        .collect()
}

/// Whether the module body `body` uses the list `list` only to bind the global `name`: no
/// instruction but a copy and the store reads it or a copy of it (`__all__.append(n)` changes
/// what the binding holds).
fn binds_only(body: &IrFunction, list: VarId, name: &str) -> bool {
    let held = aliases_of(body, [list]);
    insts(body).all(|inst| match &inst.kind {
        InstKind::Copy { .. } => true,
        InstKind::StoreGlobal { name: n, .. } if n == name => true,
        kind => !kind.operands().iter().any(|v| held.contains(v)),
    })
}

/// Whether the module may call `__setstate__` itself, outside a `__setstate__` handing its own
/// state on (`super().__setstate__(state)`): [`ModuleDecls::calls_setstate`]. A call, a read of
/// the method (`restore = obj.__setstate__`), and its name as a string
/// (`getattr(obj, "__setstate__")`) all count: the method read may be called with anything.
pub(super) fn calls_setstate(input: &ModuleInput<'_>) -> bool {
    let names_it = |kind: &InstKind| match kind {
        InstKind::MethodCall { method: name, .. } | InstKind::AttrLoad { attr: name, .. } => {
            name == "__setstate__"
        }
        InstKind::Const {
            literal: heki_ir::ConstLiteral::Str(s),
            ..
        } => s == "__setstate__",
        _ => false,
    };
    input
        .ir
        .functions
        .iter()
        .filter(|f| f.name != "__setstate__")
        .flat_map(insts)
        .any(|inst| names_it(&inst.kind))
}

/// [`super::FunctionDecl::state_of_self`] of the module's `__getstate__` methods. The methods
/// defined under one name (on both branches of an `if`, or defined again) share a declaration:
/// it holds only when every one of them does.
fn record_getstates(input: &ModuleInput<'_>, cx: &TypeCtx<'_>, decls: &mut ModuleDecls) {
    let mut of_self: BTreeMap<&str, bool> = BTreeMap::new();
    for f in &input.ir.functions {
        if f.name == "__getstate__" && f.owner_class_qualname.is_some() {
            *of_self.entry(&f.qualname).or_insert(true) &= state_of_self(cx, f);
        }
    }
    for (qualname, holds) in of_self {
        if let Some(decl) = decls.functions.get_mut(qualname) {
            decl.state_of_self = holds;
        }
    }
}

/// Whether the `__getstate__` method `f` returns only states built from the instance's own
/// attributes: its namespace (`self.__dict__`, `vars(self)`, a copy of either), or a dict it
/// fills only with them (`{a: getattr(self, a, None) for a in self.__attrs__}`), each value
/// read from the instance or a constant.
fn state_of_self(cx: &TypeCtx<'_>, f: &IrFunction) -> bool {
    f.params
        .first()
        .is_some_and(|this| state_built_from(cx, f, this.var))
}

/// [`state_of_self`] with the instance in `this`.
fn state_built_from(cx: &TypeCtx<'_>, f: &IrFunction, this: VarId) -> bool {
    let selfs = self_aliases(f, this);
    let types = cx.infer_types(f);
    let shape = FnShape::new(f);
    let builtin = |v: VarId, name: &str| matches!(types.ty(v), Some(Ty::Ref(s)) if s == name);
    // A value read from the instance, or a constant.
    let own_value = |v: VarId| match shape.inst(shape.copy_root(v)).map(|i| &i.kind) {
        Some(InstKind::AttrLoad { obj, .. }) => selfs.contains(obj),
        Some(InstKind::Const { .. }) => true,
        Some(InstKind::Call {
            callee_var: Some(c),
            args,
            ..
        }) => builtin(*c, "builtins.getattr") && args.first().is_some_and(|a| selfs.contains(a)),
        _ => false,
    };
    let namespace = |v: VarId| match shape.inst(v).map(|i| &i.kind) {
        Some(InstKind::AttrLoad { obj, attr, .. }) => attr == "__dict__" && selfs.contains(obj),
        Some(InstKind::Call {
            callee_var: Some(c),
            args,
            ..
        }) => builtin(*c, "builtins.vars") && args.len() == 1 && selfs.contains(&args[0]),
        _ => false,
    };
    let state = |v: VarId| {
        let root = shape.copy_root(v);
        if namespace(root) {
            return true;
        }
        match shape.inst(root).map(|i| &i.kind) {
            Some(InstKind::MethodCall {
                recv, method, args, ..
            }) => method == "copy" && args.is_empty() && namespace(shape.copy_root(*recv)),
            Some(InstKind::DictLiteral { entries, .. }) => {
                entries.iter().all(|e| match e {
                    heki_ir::DictEntry::KeyValue { value, .. } => own_value(*value),
                    heki_ir::DictEntry::Unpack { .. } => false,
                }) && filled_by_self(&shape, root, &own_value)
            }
            Some(InstKind::AllocBuiltin {
                kind: BuiltinKind::Dict,
                elems,
                ..
            }) => elems.is_empty() && filled_by_self(&shape, root, &own_value),
            _ => false,
        }
    };
    let mut returns = f.blocks.iter().filter_map(|b| match b.terminator {
        Terminator::Return(v) => Some(v),
        _ => None,
    });
    returns.all(|v| v.is_some_and(state))
}

/// Whether the dict `dict` is only filled with values `own_value` accepts (`d[k] = v`), copied,
/// and returned.
fn filled_by_self(shape: &FnShape<'_>, dict: VarId, own_value: &dyn Fn(VarId) -> bool) -> bool {
    let f = shape.f;
    let budget = Budget::of_function("store targets", f);
    let next = |v: VarId, out: &mut Vec<VarId>| {
        for u in shape.readers(v) {
            if let Use::Inst(b, i) = *u
                && let InstKind::Copy { dst, .. } = &f.blocks[b.0 as usize].insts[i as usize].kind
            {
                out.push(*dst);
            }
        }
    };
    let visit = |v: VarId| {
        let returned =
            |b: BlockId| matches!(f.blocks[b.0 as usize].terminator, Terminator::Return(_));
        let only_filled = shape.readers(v).iter().all(|u| match *u {
            Use::Inst(b, i) => match &f.blocks[b.0 as usize].insts[i as usize].kind {
                InstKind::Copy { .. } => true,
                InstKind::SubscriptStore { obj, value, .. } => *obj == v && own_value(*value),
                _ => false,
            },
            u => matches!(u, Use::Terminator(b) if returned(b)),
        });
        if only_filled {
            Visit::Expand
        } else {
            Visit::Stop(())
        }
    };
    reach(&budget, [dict], next, visit) == Answer::Known(None)
}

/// What project modules store into the globals of other modules and into the instances of
/// their classes ([`ModuleDecls::stores_elsewhere`]), joined by what they store into, and the
/// names whose [`ModuleDecls::store_fallbacks`] hold, by module, in each module they store into.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Elsewhere {
    globals: FxHashMap<String, StoredElsewhere>,
    attrs: FxHashMap<String, FxHashMap<String, StoredElsewhere>>,
    unnamed: FxHashSet<String>,
    held: FxHashMap<String, FxHashSet<StoreTarget>>,
}

/// What other modules store into a module: its globals and the attributes of its classes, by
/// name, or ones heki cannot name.
#[derive(Default)]
struct Touched {
    globals: FxHashSet<String>,
    attrs: FxHashSet<String>,
    any_global: bool,
    any_attr: bool,
}

impl Touched {
    /// The names of the module whose `fallbacks` hold: those a function of its fixpoint stores
    /// when it reads a name other modules store into, or one of these names in turn.
    fn held(&self, fallbacks: &StoreFallbacks) -> FxHashSet<StoreTarget> {
        let dependencies = &fallbacks.dependencies;
        let touched = |read: &StoreTarget| match read {
            StoreTarget::Global(name) => self.any_global || self.globals.contains(name),
            StoreTarget::Attr(_, attr) => self.any_attr || self.attrs.contains(attr),
        };
        let mut readers: FxHashMap<&StoreTarget, Vec<usize>> = FxHashMap::default();
        for (i, d) in dependencies.iter().enumerate() {
            for read in &d.reads {
                readers.entry(read).or_default().push(i);
            }
        }
        let starts =
            (0..dependencies.len()).filter(|&i| dependencies[i].reads.iter().any(&touched));
        let next = |i: usize, out: &mut Vec<usize>| {
            for stored in &dependencies[i].stores {
                out.extend(readers.get(stored).into_iter().flatten().copied());
            }
        };
        let mut reached = vec![false; dependencies.len()];
        let budget = Budget::unmetered("store types");
        let walked = reach(&budget, starts, next, |i| {
            reached[i] = true;
            Visit::<()>::Expand
        });
        // Past the budget, every fallback holds.
        let every = walked.is_open();
        dependencies
            .iter()
            .zip(reached)
            .filter(|&(_, reached)| every || reached)
            .flat_map(|(d, _)| d.stores.iter().cloned())
            .collect()
    }
}

impl Elsewhere {
    /// The `DeclKeys::grown_globals` keys whose answers differ in `after`: the globals, the
    /// classes, and the modules.
    pub(super) fn changes(&self, after: &Self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for fqn in self.globals.keys().chain(after.globals.keys()) {
            if self.globals.get(fqn) != after.globals.get(fqn) {
                out.insert(fqn.clone());
            }
        }
        for class in self.attrs.keys().chain(after.attrs.keys()) {
            if self.attrs.get(class) != after.attrs.get(class) {
                out.insert(class.clone());
            }
        }
        out.extend(self.unnamed.symmetric_difference(&after.unnamed).cloned());
        for module in self.held.keys().chain(after.held.keys()) {
            if self.held.get(module) != after.held.get(module) {
                out.insert(module.clone());
            }
        }
        out
    }
}

impl StoredElsewhere {
    /// Join what another module's stores bind.
    fn join(&mut self, other: &StoredElsewhere) {
        self.in_place |= other.in_place;
        self.changed |= other.changed;
        self.ty = match (self.ty.take(), &other.ty) {
            (t, None) => t,
            (None, t) => t.clone(),
            (Some(Some(a)), Some(Some(b))) => Some(a.join(b)),
            (Some(_), Some(_)) => Some(None),
        };
    }

    /// The join of `own` and what these stores bind: `None` when it is unknown or rebinding in
    /// place may change it.
    fn joined_with(&self, own: &Ty) -> Option<Ty> {
        let t = match &self.ty {
            Some(other) => own.join(other.as_ref()?)?,
            None => own.clone(),
        };
        let t = if self.changed { t.without_items() } else { t };
        Some(t).filter(|t| kept_in_declarations(t) && (!self.in_place || keeps_in_place(t)))
    }
}

impl ProjectDecls {
    /// [`Self::elsewhere`].
    pub(super) fn stores_elsewhere(&self) -> Elsewhere {
        let mut out = Elsewhere::default();
        let mut touched: FxHashMap<String, Touched> = FxHashMap::default();
        // The module `fqn` is in (or names), and the rest of it, when that is not `m`.
        let owner = |fqn: &str, m: &ModuleDecls| {
            let (owner, rest) = self.split_module(fqn)?;
            (owner.module_name != m.module_name).then(|| (owner.module_name.clone(), rest.len()))
        };
        for m in self.modules.values() {
            let Some(stores) = m.stores_elsewhere.as_deref() else {
                continue;
            };
            for (fqn, t) in &stores.globals {
                let fqn = self.canonical(fqn);
                let Some((module, rest)) = owner(&fqn, m) else {
                    continue;
                };
                let name = fqn[fqn.len() - rest..].to_owned();
                touched.entry(module).or_default().globals.insert(name);
                out.globals.entry(fqn).or_default().join(t);
            }
            for (class, attrs) in &stores.attrs {
                let class = self.canonical(class);
                for (module, _) in owner(&class, m).into_iter() {
                    let into = touched.entry(module).or_default();
                    into.attrs.extend(attrs.keys().cloned());
                    let joins = out.attrs.entry(class.clone()).or_default();
                    for (attr, t) in attrs {
                        joins.entry(attr.clone()).or_default().join(t);
                    }
                }
            }
            for fqn in &stores.unnamed {
                let fqn = self.canonical(fqn);
                for (module, rest) in owner(&fqn, m).into_iter() {
                    let into = touched.entry(module).or_default();
                    if rest == 0 {
                        into.any_global = true;
                    } else {
                        into.any_attr = true;
                    }
                    out.unnamed.insert(fqn.clone());
                }
            }
        }
        out.held = touched
            .into_iter()
            .filter_map(|(module, into)| {
                let fallbacks = self.module(&module)?.store_fallbacks.as_deref()?;
                Some((module.clone(), into.held(fallbacks)))
            })
            .collect();
        out
    }

    /// The fallbacks of the module `m` and the names whose fallbacks hold, where another module
    /// stores into it.
    fn fallbacks_of<'m>(
        &self,
        m: &'m ModuleDecls,
    ) -> Option<(&'m StoreFallbacks, &FxHashSet<StoreTarget>)> {
        self.read(|k| note(&mut k.grown_globals, &m.module_name));
        let held = self.elsewhere.held.get(&m.module_name)?;
        Some((m.store_fallbacks.as_deref()?, held))
    }

    /// The type of the module global `name` of `m` ([`ModuleDecls::global_types`]), joined with
    /// what other project modules store into it ([`Self::elsewhere`]).
    pub(super) fn global_type(&self, m: &ModuleDecls, name: &str) -> Option<Ty> {
        let fqn = self.canonical(&format!("{}.{name}", m.module_name));
        self.read(|k| note(&mut k.grown_globals, &fqn));
        let fallback = self.fallbacks_of(m).and_then(|(fallbacks, held)| {
            let fallback = fallbacks.globals.get(name)?;
            held.contains(&StoreTarget::Global(name.to_owned()))
                .then_some(fallback)
        });
        let own = match fallback {
            Some(fallback) => fallback.as_ref(),
            None => m.global_types.get(name),
        }?;
        if self.elsewhere.unnamed.contains(&m.module_name) {
            return None;
        }
        match self.elsewhere.globals.get(&fqn) {
            None => Some(own.clone()),
            // Another module's change in place leaves an annotated global its declaration.
            Some(stored) if stored.changed && m.declared_items.contains_key(name) => {
                let declared = &m.declared_items[name];
                StoredElsewhere {
                    changed: false,
                    ..stored.clone()
                }
                .joined_with(declared)
            }
            Some(stored) => stored.joined_with(own),
        }
    }

    /// The entries of the table the global `fqn` holds ([`ModuleDecls::tables`]), through
    /// re-exports, unless another module stores into the name or changes its object in place
    /// (`ModuleDecls::stores_elsewhere`), or rebinds it.
    pub(crate) fn table(&self, fqn: &str) -> Option<&[(Box<str>, Option<Ty>)]> {
        let fqn = self.canonical_value(fqn)?;
        let (m, name) = self.split_module(&fqn)?;
        let entries = m.tables.get(name)?;
        let touched = self.global_stored_elsewhere(m, &fqn) || self.rebound_elsewhere(&fqn);
        (!touched).then_some(&**entries)
    }

    /// Whether another project module stores into the global `fqn` of `m`, by name in any
    /// spelling (`lib.f = g`, `setattr(lib, "f", g)`) or under a name heki cannot read
    /// (`setattr(lib, name, g)`).
    pub(super) fn global_stored_elsewhere(&self, m: &ModuleDecls, fqn: &str) -> bool {
        self.read(|k| note(&mut k.grown_globals, fqn));
        self.read(|k| note(&mut k.grown_globals, &m.module_name));
        self.elsewhere.globals.contains_key(fqn) || self.elsewhere.unnamed.contains(&m.module_name)
    }

    /// What the module's code stores in `attr` of the instances of the canonical class `class`
    /// (`decl`), with its fallback when another module stores into the class's module.
    pub(super) fn attr_store<'s>(
        &'s self,
        class: &str,
        decl: &'s ClassDecl,
        attr: &str,
    ) -> Option<&'s AttrStore> {
        let fallback = self.split_module(class).and_then(|(m, qualname)| {
            let (fallbacks, held) = self.fallbacks_of(m)?;
            let fallback = fallbacks.attrs.get(qualname)?.get(attr)?;
            let target = StoreTarget::Attr(qualname.to_owned(), attr.to_owned());
            held.contains(&target).then_some(fallback)
        });
        fallback.or_else(|| decl.stored(attr))
    }

    /// Whether the code of the module of the canonical class `class` (`decl`) stores attributes
    /// heki cannot name in its instances, or another module's does.
    pub(super) fn stores_any_attr(&self, class: &str, decl: &ClassDecl) -> bool {
        self.read(|k| note(&mut k.grown_globals, class));
        decl.stores_any_attr
            || self.elsewhere.unnamed.contains(class)
            || self.split_module(class).is_some_and(|(m, qualname)| {
                self.fallbacks_of(m)
                    .is_some_and(|(f, _)| f.any_attr.contains(qualname))
            })
    }

    /// The join of what the code of the project stores in `attr` of the instances of the
    /// classes of `family` (canonical, [`Self::instance_family`]): each class's stores (an
    /// annotated attribute's declared type), and what other modules store into its instances;
    /// `None` for none or an unknown one, and when a class's instances get attributes heki
    /// cannot name.
    pub(super) fn join_family(&self, family: &[String], attr: &str) -> Option<Ty> {
        let mut out = Stored::Nothing;
        let mut restoring = false;
        let mut in_place = false;
        let mut changed = false;
        for (class, decl) in family
            .iter()
            .filter_map(|class| Some((class, self.class(class)?)))
        {
            if self.stores_any_attr(class, decl) {
                return None;
            }
            restoring |= decl.restores_state;
            if let Some(t) = decl.attr_types.get(attr) {
                out.add(Some(Some(t)));
                continue;
            }
            in_place |= decl.in_place_attrs.iter().any(|a| &**a == attr);
            match self.attr_store(class, decl, attr) {
                Some(AttrStore::Established(t) | AttrStore::Of(t)) => out.add(Some(Some(t))),
                Some(AttrStore::Open) => return None,
                None => {}
            }
            if let Some(stored) = self.elsewhere.attrs.get(class).and_then(|a| a.get(attr)) {
                in_place |= stored.in_place;
                changed |= stored.changed;
                if let Some(t) = &stored.ty {
                    out.add(Some(t.as_ref()));
                }
            }
        }
        let of_self =
            |key: &crate::FuncKey| self.function_decl(key).is_some_and(|d| d.state_of_self);
        if restoring && !self.restores_hold(family, of_self) {
            return None;
        }
        let t = out
            .type_of()
            .map(|t| if changed { t.without_items() } else { t });
        t.filter(|t| !in_place || keeps_in_place(t))
    }

    /// Whether what `__setstate__` restores in an instance of the classes of `family` is what
    /// one held: pickle or copy took the state (no project code calls `__setstate__` itself),
    /// through no `__reduce__` or `__reduce_ex__` of the family, and no `__getstate__` of it
    /// building the state from anything but the instance's own attributes (`of_self`).
    pub(super) fn restores_hold(
        &self,
        family: &[String],
        of_self: impl Fn(&crate::FuncKey) -> bool,
    ) -> bool {
        self.read(|k| note(&mut k.class_facts, SETSTATE_KEY));
        if self.setstate_called {
            return false;
        }
        family.iter().all(|class| {
            let reduces = ["__reduce__", "__reduce_ex__"]
                .iter()
                .any(|m| self.own_method(class, m).is_some());
            !reduces
                && self
                    .own_method(class, "__getstate__")
                    .is_none_or(|key| of_self(&key))
        })
    }

    /// Whether the definition of `attr` that the project class `class` has itself is a
    /// property's getter (`ProjectDecls::member_type` reads its declared type).
    pub(super) fn property_at(&self, class: &str, attr: &str) -> bool {
        self.read(|k| note(&mut k.property_names, attr));
        self.property_names.contains(attr) && self.own_method(class, attr).is_some()
    }
}
