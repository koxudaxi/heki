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
    StoredElsewhere, StoredName, StoresElsewhere, Table, TypeCtx, aliases_of, is_any_callable,
    is_summarized_function, kept_in_declarations, note, of_any_class, self_aliases,
};
use crate::ModuleInput;
use crate::infer::Inferred;
use crate::types::{Added, Change, Obj, Ty};

use heki_ir::walk::{
    Answer, Budget, Closure, Def, Facts, FnShape, Join, Sparse, Use, Visit, fixpoint, reach,
};
use heki_ir::{
    BinOpKind, BlockId, BuiltinKind, FunctionId, InPlaceChanges, Inst, InstKind, IrFunction,
    LastBindings, Terminator, VarId,
};
use heki_kb::{EffectiveEcosystemKb, FieldItems};
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

/// The key of the declarations' read log ([`super::DeclKeys::grown_globals`]) an answer of
/// [`ProjectDecls::attr_stored_untyped`] notes: the attribute after a dot, which no fqn starts
/// with.
fn untyped_key(attr: &str) -> String {
    format!(".{attr}")
}

/// The same key for the stores under a name heki cannot read through such a receiver
/// ([`StoresElsewhere::untyped_unnamed`]): no attribute is named `.` after the dot.
const UNTYPED_UNNAMED_KEY: &str = "..";

/// The key an answer of [`ProjectDecls::bound_on_classes`] notes for a store under the constant
/// name `name` through a receiver that may be a class object: the name after a colon, which no
/// fqn starts with.
fn on_classes_key(name: &str) -> String {
    format!(":{name}")
}

/// The same key for the stores under a name built at run time through such a receiver
/// ([`StoredName::Built`]).
const ON_CLASSES_BUILT_KEY: &str = "::";

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
    /// The classes of `any` whose instances may get an attribute they did not have: a store
    /// under a name heki cannot read, not one under a key of an instance's namespace.
    adding: BTreeSet<String>,
    /// The classes whose `__setstate__` restores attributes heki cannot name from its state.
    restoring: BTreeSet<String>,
    /// A store binds a global of the module heki cannot name.
    any_global: bool,
    /// The names a store rebinds to what an in-place operator gives on their own value.
    in_place: BTreeSet<StoreTarget>,
    /// The attributes a store binds otherwise than through `self` in a method of their class,
    /// or to a value that may be `None` ([`ClassDecl::unowned_attrs`]).
    unowned: BTreeSet<StoreTarget>,
    /// What changes in place add to the objects of names the code changes through a load
    /// typed by the name alone (a global read through the module's own object): joined into
    /// their type once it is established.
    added: BTreeMap<StoreTarget, Added>,
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
        for class in &other.adding {
            grew |= self.adding.insert(class.clone());
        }
        for class in &other.restoring {
            grew |= self.restoring.insert(class.clone());
        }
        grew |= self.any_global.join(&other.any_global);
        for target in &other.in_place {
            grew |= self.in_place.insert(target.clone());
        }
        for target in &other.unowned {
            grew |= self.unowned.insert(target.clone());
        }
        for (target, added) in &other.added {
            grew |= self.add(target.clone(), added);
        }
        grew
    }

    fn weight(&self) -> usize {
        1 + self.every.len()
            + self.establishing.len()
            + self.any.len()
            + self.adding.len()
            + self.restoring.len()
            + self.in_place.len()
            + self.unowned.len()
            + self.added.len()
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
        for (target, added) in &other.added {
            if self.add(target.clone(), added) {
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
        // What reading an attribute gives does not depend on whether a store adds one, or on
        // where its stores are.
        self.adding.extend(other.adding.iter().cloned());
        self.unowned.extend(other.unowned.iter().cloned());
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

    /// Join `added` into what changes in place add to `target` ([`Self::added`]); returns
    /// whether it grew.
    fn add(&mut self, target: StoreTarget, added: &Added) -> bool {
        self.added.entry(target).or_default().join(added)
    }

    /// The type of `target` when its establishing stores bind a value of known type and heki
    /// can name every store of it: the join of every store, with what [`Self::added`] adds.
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
        let t = match self.added.get(target) {
            Some(added) => t.with_added(added),
            None => t,
        };
        (!self.in_place.contains(target) || keeps_in_place(&t)).then_some(t)
    }

    /// The types these joins give the module's globals and attributes (`decls` gives the
    /// annotations, which type every store of the attributes they declare).
    fn decide(&self, decls: &ModuleDecls) -> Decided {
        let mut out = Decided {
            any: self.any.clone(),
            adding: self.adding.clone(),
            restoring: self.restoring.difference(&self.any).cloned().collect(),
            ..Decided::default()
        };
        for target in &self.unowned {
            if let StoreTarget::Attr(class, attr) = target {
                let attrs = out.unowned.entry(class.clone()).or_default();
                attrs.insert(attr.clone());
            }
        }
        for target in &self.in_place {
            if let StoreTarget::Attr(class, attr) = target
                && !declares_items(decls, class, attr)
            {
                out.in_place
                    .entry(class.clone())
                    .or_default()
                    .insert(attr.clone());
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
                    if declares_items(decls, class, attr) {
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
    adding: BTreeSet<String>,
    restoring: BTreeSet<String>,
    in_place: BTreeMap<String, BTreeSet<String>>,
    unowned: BTreeMap<String, BTreeSet<String>>,
}

/// Whether the module's stores into `attr` of `class` decide nothing of the attribute's type:
/// the class is not the module's, or an annotation names more than a container class
/// (`list[Handler]`), which types every store. A bare container annotation (`list`) fixes only
/// the class: the stores give the items as they give an unannotated attribute's type.
fn declares_items(decls: &ModuleDecls, class: &str, attr: &str) -> bool {
    decls.classes.get(class).is_none_or(|c| {
        c.attr_types
            .get(attr)
            .is_some_and(|t| !t.is_bare_container())
    })
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
        for qualname in self.adding {
            if let Some(class) = decls.classes.get_mut(&qualname) {
                class.adds_unnamed_attrs = true;
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
        for (qualname, attrs) in self.unowned {
            if let Some(class) = decls.classes.get_mut(&qualname) {
                class.unowned_attrs = attrs.into_iter().map(Box::from).collect();
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
                let exports = super::star_import_module(input, star).and_then(|module| {
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
    changed: Box<[ChangedLoad<'f>]>,
    /// The changes in place of the function, [`Answer::Open`] when heki cannot list them all.
    changes: Answer<InPlaceChanges<'f>>,
    /// The class whose instance the function stores into as `self` (qualname), and the
    /// variables holding it ([`self_of`]).
    selfs: Option<(&'f str, FxHashSet<VarId>)>,
    /// `__init__` or `__post_init__`, whose stores through `self` every instance runs.
    at_init: bool,
    /// The state parameter of a `__setstate__` method.
    state: Option<VarId>,
}

/// A load whose object a function changes in place ([`changed_loads`]): the variable it binds
/// and what it reads.
struct ChangedLoad<'f> {
    dst: VarId,
    of: LoadOf<'f>,
}

/// What a [`ChangedLoad`] reads.
#[derive(Clone, Copy)]
enum LoadOf<'f> {
    /// A name of the module (`HANDLERS[kind] = run`).
    Name(&'f str),
    /// Attribute `attr` of `obj` (`self.items.append(x)`): `(obj, attr)`.
    Attr(VarId, &'f str),
}

/// What the stores of a function bind in other modules, by name.
#[derive(Default)]
struct Foreign {
    globals: BTreeMap<String, ForeignStore>,
    attrs: BTreeMap<String, BTreeMap<String, ForeignStore>>,
    unnamed: BTreeSet<String>,
    untyped: BTreeSet<String>,
    untyped_unnamed: bool,
    on_classes: BTreeSet<StoredName>,
}

/// What the stores of a function do to one name of another module.
#[derive(Default)]
struct ForeignStore {
    /// The join of the values.
    stored: Stored,
    /// A store rebinds the name to what an in-place operator gives on its own value.
    in_place: bool,
    /// What the function's changes in place of the object the name holds add to it
    /// ([`StoringFn::changed`]).
    changed: Option<Added>,
    /// A store binds a value that may be `None`.
    none: bool,
}

impl ForeignStore {
    fn changed_by(&mut self, added: &Added) {
        join_added(&mut self.changed, added);
    }
}

/// Join `added` into what changes in place add to a name (`None`: no change).
fn join_added(changed: &mut Option<Added>, added: &Added) {
    match changed {
        Some(before) => {
            before.join(added);
        }
        None => *changed = Some(added.clone()),
    }
}

impl Foreign {
    fn into_decl(self) -> Option<StoresElsewhere> {
        let stored = |s: ForeignStore| {
            let ty = s.stored.elsewhere();
            (ty.is_some() || s.in_place || s.changed.is_some() || s.none).then_some(
                StoredElsewhere {
                    ty,
                    in_place: s.in_place,
                    changed: s.changed,
                    none: s.none,
                },
            )
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
            untyped_attrs: self.untyped,
            untyped_unnamed: self.untyped_unnamed,
            on_classes: self.on_classes,
        };
        let any = !(out.globals.is_empty()
            && out.attrs.is_empty()
            && out.unnamed.is_empty()
            && out.untyped_attrs.is_empty()
            && !out.untyped_unnamed
            && out.on_classes.is_empty());
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
/// attribute its class annotates has no entry, unless the annotation is a bare container class
/// (`list`), which fixes only the class: its stores give the items as they give an unannotated
/// attribute's type. In a module whose globals `globals()` or `exec` may change, each store of
/// an unannotated global binds a value of unknown type, and so does a store of the module body
/// before a star import, which may rebind the name.
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
        let body = input.ir.function(input.ir.module_fn);
        let body_alone = cx0.infers_alone(body);
        let body_types = cx0.infer_types(body);
        let body_reads = first_reads.consulted.take();
        let body_trace = first_reads.recorder.take();
        let declared = |name: &str| body_types.declared(name).cloned();
        let mut foreign = Foreign::default();
        let mut first: Vec<First> = Vec::with_capacity(fns.len());
        let mut changed: FxHashSet<&str> = FxHashSet::default();
        for s in &fns {
            let (types, alone, earlier) = if s.f.is_module_body {
                first_reads.consulted.replace(body_reads.clone());
                (Rc::clone(&body_types), body_alone, &*body_trace)
            } else {
                let alone = cx0.infers_alone(s.f);
                (cx0.infer_types(s.f), alone, &[][..])
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
        // A module body storing nothing binds no table.
        let body = fns.iter().find(|s| s.f.is_module_body);
        let tables = body.map_or_else(BTreeMap::new, |body| {
            tables(decls, body, &body_types, &unfixed, &changed)
        });
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
/// entry, one of whose values is a function, a class, a `functools.partial` object, or an
/// instance whose fields hold such objects (`crate::types::Args::Fields`), when
/// no function of the module changes the global's object in place (`changed`,
/// [`Classifier::changed_globals`], and the module body's changes of the display it holds) and
/// nothing may rebind the name (`unfixed`).
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
            let display = shape.copy_root(*value);
            let Some(InstKind::DictLiteral { entries, .. }) = shape.inst(display).map(|i| &i.kind)
            else {
                continue;
            };
            // The module body holds the display in a value: an item it stores there, or a
            // method it calls on it, changes it after the store (`HANDLERS["b"] = other`).
            let changed_here = (body.changes.as_ref())
                .map(|c| c.aliased_by.contains_key(&display))
                .open_means(true);
            if entries.len() > TABLE_ENTRIES || changed_here {
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
            let callable = |t: &Option<Ty>| match t {
                Some(Ty::Ref(_) | Ty::OneOf(_) | Ty::Class(_) | Ty::Partial(..)) => true,
                // An instance whose fields hold named objects (`{"a": Spec(run=f)}`).
                Some(t) => t
                    .obj()
                    .is_some_and(|o| matches!(o.args, crate::types::Args::Fields(_))),
                None => false,
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
            let declared = self.decls.classes[qualname].attr_types.get(attr);
            if let Some(t) = declared.filter(|t| !t.is_bare_container()) {
                out.add(Some(Some(t)));
                continue;
            }
            match joins.every.get(&target) {
                Some(Stored::Open) => return None,
                Some(stored) => {
                    out.join(stored);
                }
                None => {}
            }
            if let Some(bare) = declared
                && joins.established(&target).is_none()
            {
                out.add(Some(Some(bare)));
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
            let declared = self.decls.classes[qualname].attr_types.get(attr);
            if let Some(t) = declared.filter(|t| !t.is_bare_container()) {
                return Some(Read::Of(t.clone()));
            }
            let bare = declared.map(|t| Read::Of(t.clone()));
            let target = StoreTarget::Attr(qualname.to_owned(), attr.to_owned());
            if !self.establishable.contains(&target) {
                if bare.is_some() {
                    return bare;
                }
                continue;
            }
            let mut consulted = self.consulted.borrow_mut();
            consulted.push(Key::Name(target.clone()));
            consulted.push(Key::Class(qualname.to_owned()));
            drop(consulted);
            let Some(family) = self.family(class) else {
                return bare;
            };
            let joins = self.joins.borrow();
            let read = match joins.establishing.get(&target) {
                None | Some(Stored::Nothing) => {
                    self.pending.borrow_mut().insert(target);
                    Some(Read::Pending)
                }
                Some(Stored::Open) => None,
                Some(Stored::Of(_)) if joins.any.contains(qualname) => None,
                Some(Stored::Of(_)) => self.join_family(&joins, &family, attr).map(Read::Of),
            };
            return match (declared, read) {
                (Some(d), Some(Read::Of(t))) => Some(Read::Of(d.holding_stored(Some(t)))),
                (Some(_), None) => bare,
                (_, read) => read,
            };
        }
        None
    }
}

fn insts(f: &IrFunction) -> impl Iterator<Item = &Inst> {
    f.blocks.iter().flat_map(|b| &b.insts)
}

/// The loads of `f` of a global or an attribute whose object `f` changes in place itself
/// ([`heki_ir::FnIndex::changes_in_place`]: `HANDLERS[kind] = run`, `self.items.append(x)`,
/// `self.items += more`), in the order of the function's instructions, and the changes
/// ([`Answer::Open`] when heki cannot list them all, and every load may be changed).
fn changed_loads<'f>(
    input: &'f ModuleInput<'_>,
    f: &'f IrFunction,
) -> (Box<[ChangedLoad<'f>]>, Answer<InPlaceChanges<'f>>) {
    let changes = insts(f).any(|inst| {
        matches!(
            inst.kind,
            InstKind::SubscriptStore { .. }
                | InstKind::MethodCall { .. }
                | InstKind::BinOp { in_place: true, .. }
        )
    });
    if !changes {
        return (Box::default(), Answer::Known(InPlaceChanges::default()));
    }
    let changes = heki_ir::FnIndex::new(f, &input.ir.constants).changes_in_place();
    let changed = |dst: &VarId| {
        let aliased = changes.as_ref().map(|c| c.aliased_by.contains_key(dst));
        aliased.open_means(true)
    };
    let loads = insts(f).filter_map(|inst| {
        let (dst, of) = match &inst.kind {
            InstKind::LoadGlobal { dst, name, .. } | InstKind::LoadName { dst, name } => {
                (*dst, LoadOf::Name(name))
            }
            InstKind::AttrLoad { dst, obj, attr } => (*dst, LoadOf::Attr(*obj, attr)),
            _ => return None,
        };
        changed(&dst).then_some(ChangedLoad { dst, of })
    });
    (loads.collect(), changes)
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
        let (changed, changes) = changed_loads(input, f);
        if changed.is_empty() && !insts(f).any(|inst| calls.may_store(&inst.kind)) {
            continue;
        }
        let selfs = is_summarized_function(f).then(|| self_of(cx, f)).flatten();
        let method = selfs.is_some() && f.owner_class_qualname.is_some();
        out.push(StoringFn {
            f,
            shape,
            changed,
            changes,
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

/// The name an attribute operation names ([`key_name`] for one computed at run time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AttrName<'f> {
    /// The one a string constant gives.
    Named(Cow<'f, str>),
    /// One of the string constants of the tuple, list, or set display a loop takes the name
    /// from (`for key in ("a", "b")`, also through a module's tuple constant, `for key in
    /// RECORDS`), or that the keys of the pairs `ns.update(...)` stores are
    /// (`ns.update((key, v) for key in RECORDS)`).
    OneOf(Vec<String>),
    /// An attribute one of these project classes (canonical) gives its instances: a key of
    /// the namespace of an instance (`for key, value in src.__dict__.items()`), which that
    /// instance has already, or a field the class declares (`for name in type(src).model_fields`,
    /// `heki_kb::DeclaredFields`) ([`ProjectDecls::keys_exist_in`]).
    KeyOf(Vec<String>),
    /// One built at run time whose constant text starts with `head` and ends with `tail`, which
    /// do not overlap ([`built_name`]): `f"__attr_{name}"` starts with `__attr_`.
    Built { head: String, tail: String },
    /// One heki cannot read: any name.
    Unknown,
}

/// What a loop over a value takes from it, when heki can name it ([`Iterated::items`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Items {
    /// The string constants of a display.
    Constants(Vec<String>),
    /// The keys of a mapping from attributes of instances of these project classes
    /// ([`AttrName::KeyOf`]): the namespace of an instance, or the fields the class declares.
    Keys(Vec<String>),
    /// `(key, value)` pairs whose keys are the boxed items: the pairs of such a mapping
    /// (`Keys`), or pairs a generator makes of string constants
    /// (`Constants`: `((key, src.__dict__[key]) for key in RECORDS)`).
    Pairs(Box<Items>),
    /// Objects whose `name` is a field one of these project classes declares
    /// (`dataclasses.fields(obj)`).
    Fields(Vec<String>),
}

/// The values a loop iterates, followed through the function of `f`: `def` gives each
/// variable's defining instruction, `types` the inferred values, `constant` the string constant
/// a variable holds.
struct Iterated<'a, 'i> {
    cx: &'a TypeCtx<'a>,
    f: &'a IrFunction,
    def: &'a dyn Fn(VarId) -> Option<&'i Inst>,
    types: &'a FxHashMap<VarId, crate::resolver::Val>,
    constant: &'a dyn Fn(VarId) -> Option<String>,
}

impl<'i> Iterated<'_, 'i> {
    /// The most wrappers (`list(src.__dict__.items())`) followed to the namespace.
    const MAX_WRAPPERS: usize = 4;
    /// The most copies followed to a defining instruction, as [`TypeCtx::str_constant_with`]
    /// follows them.
    const MAX_COPIES: u32 = 16;

    /// The instruction defining what `var` holds, through copies.
    fn def(&self, var: VarId) -> Option<&'i Inst> {
        let mut cur = var;
        for _ in 0..Self::MAX_COPIES {
            let inst = (self.def)(cur)?;
            match &inst.kind {
                InstKind::Copy { src, .. } => cur = *src,
                _ => return Some(inst),
            }
        }
        None
    }

    /// What the loop `item` holds an item of takes ([`Self::items`] of what it iterates), when
    /// `item` holds one.
    fn loop_items(&self, item: VarId) -> Option<Items> {
        if let InstKind::IterNext { iter, .. } = self.def(item)?.kind
            && let InstKind::MakeIter { iterable, .. } = self.def(iter)?.kind
        {
            return self.items(iterable);
        }
        None
    }

    /// Whether the list or set display `display` still holds the items it was made with where a
    /// loop reads it: nothing but copies, loops (a comprehension's too), and the `wrappers` reads
    /// it or a variable that may be it, so no method adds, removes, or replaces an item, and no
    /// other call is handed it (`keys = ["a"]; keys.append(name)` may hold any name).
    fn holds_its_items(&self, display: VarId, wrappers: &'static [&'static str]) -> bool {
        let held = aliases_of(self.f, [display]);
        let phis_hold = self.f.blocks.iter().flat_map(|b| &b.phis).all(|phi| {
            held.contains(&phi.dst) || !phi.incoming.iter().any(|(_, v)| held.contains(v))
        });
        phis_hold
            && insts(self.f).all(|inst| match &inst.kind {
                InstKind::Copy { .. } | InstKind::MakeIter { .. } => true,
                kind if let Some((_, captured)) = kind.comprehension_operands() => {
                    !captured.iter().any(|v| held.contains(v))
                }
                kind => {
                    !kind.operands().iter().any(|v| held.contains(v))
                        || super::plain_call(kind, wrappers, |v| self.types.get(&v)).is_some()
                }
            })
    }

    /// What a loop over `iterable` takes: the items of a display of string constants or of a
    /// module's tuple constant ([`TypeCtx::global_read_str_tuple`]), the keys or the pairs of a
    /// namespace (`vars(src)`, `src.__dict__`), the fields a class declares
    /// ([`Self::declared_fields`]), a copy of a mapping (`.copy()`, `dict(...)`), its `keys()`
    /// or `items()`, a `list`, `tuple`, or `sorted` of one of these, or a generator expression
    /// over one ([`Self::yielded`]). Each wrapper holds one value, so the walk follows one chain
    /// to the display, the namespace, or the fields, then applies the wrappers from the inside
    /// out.
    fn items(&self, iterable: VarId) -> Option<Items> {
        const WRAPPERS: &[&str] = &[
            "builtins.list",
            "builtins.tuple",
            "builtins.sorted",
            "builtins.dict",
        ];
        let wrapped = |wrappers: &[Wrapper], inner: Items| {
            (wrappers.iter().rev()).try_fold(inner, |items, w| w.apply(items))
        };
        let mut wrappers: Vec<Wrapper> = Vec::new();
        let mut cur = iterable;
        for _ in 0..=Self::MAX_WRAPPERS {
            let inst = self.def(cur)?;
            let namespace = super::namespace_load(&inst.kind, |v| self.types.get(&v));
            let inner = match namespace {
                Some((_, obj)) => Some(Items::Keys(self.instances(obj)?)),
                None => self
                    .declared_fields(&inst.kind)
                    .or_else(|| self.tuple_constant(inst)),
            };
            if let Some(inner) = inner {
                return wrapped(&wrappers, inner);
            }
            match &inst.kind {
                InstKind::AllocBuiltin {
                    dst,
                    kind: kind @ (BuiltinKind::Tuple | BuiltinKind::List | BuiltinKind::Set),
                    elems,
                } => {
                    if *kind != BuiltinKind::Tuple && !self.holds_its_items(*dst, WRAPPERS) {
                        return None;
                    }
                    let names = elems.iter().enumerate().map(|(i, e)| {
                        (!self.f.is_starred(*dst, i))
                            .then(|| (self.constant)(*e))
                            .flatten()
                    });
                    let inner = Items::Constants(names.collect::<Option<_>>()?);
                    return wrapped(&wrappers, inner);
                }
                InstKind::MakeGenExpr {
                    func_id: Some(body),
                    outer_iter,
                    ..
                } => match self.yielded(*body)? {
                    Yielded::PairsOf(name) => {
                        let inner = Items::Pairs(Box::new(Items::Constants(vec![name])));
                        return wrapped(&wrappers, inner);
                    }
                    Yielded::Items(wrapper) => {
                        wrappers.push(wrapper);
                        cur = *outer_iter;
                    }
                },
                InstKind::MethodCall {
                    recv,
                    method,
                    args,
                    kwargs,
                    star_from: None,
                    ..
                } if args.is_empty() && kwargs.is_empty() => {
                    wrappers.push(match method.as_str() {
                        "keys" | "copy" => Wrapper::Keys,
                        "items" => Wrapper::Pairs,
                        _ => return None,
                    });
                    cur = *recv;
                }
                kind => {
                    let (wrapper, _, args) =
                        super::plain_call(kind, WRAPPERS, |v| self.types.get(&v))?;
                    // Each wrapper takes one iterable: `list()` iterates nothing.
                    let [arg] = args else { return None };
                    wrappers.push(match wrapper {
                        "builtins.dict" => Wrapper::Dict,
                        _ => Wrapper::Same,
                    });
                    cur = *arg;
                }
            }
        }
        None
    }

    /// The items of the module-level tuple constant `inst` reads, of this module or another
    /// (`RECORDS`, `keys.RECORDS`, `from pkg.keys import RECORDS`).
    fn tuple_constant(&self, inst: &Inst) -> Option<Items> {
        let module_of = |v: VarId| match self.types.get(&v)? {
            crate::resolver::Val::Ref(module) => Some(module.clone()),
            _ => None,
        };
        let read = self.cx.global_read(inst, &module_of)?;
        let texts = self.cx.global_read_str_tuple(read)?;
        Some(Items::Constants(
            texts.into_iter().map(str::to_owned).collect(),
        ))
    }

    /// What the generator expression whose body is the function `body` yields, when heki can
    /// name it from the outermost iterable, which its creator evaluates and passes as `.0`: each
    /// item a loop over `.0` takes (`(key for key in RECORDS if ...)`, [`Wrapper::Same`]), or a
    /// `(key, value)` pair keyed by that item (`((key, src.__dict__[key]) for key in RECORDS)`,
    /// [`Wrapper::Keyed`]), or a pair keyed by a string constant. A key a later `for` clause
    /// takes, or any other value, is none heki names.
    fn yielded(&self, body: FunctionId) -> Option<Yielded> {
        let f = self.cx.ir().function(body);
        let shape = FnShape::new(f);
        // The instruction defining what `v` holds, through copies.
        let source = |v: VarId| shape.inst(shape.copy_root(v));
        // A generator expression yields its element once, and loops over `.0` first.
        let value = insts(f).find_map(|inst| match &inst.kind {
            InstKind::Yield { value, .. } => *value,
            _ => None,
        })?;
        let over_outermost = |iter: VarId| {
            source(iter).is_some_and(|made| {
                matches!(&made.kind, InstKind::MakeIter { iterable, .. }
                    if f.var_name(*iterable) == Some(".0"))
            })
        };
        let item = insts(f).find_map(|inst| match &inst.kind {
            InstKind::IterNext { dst, iter, .. } if over_outermost(*iter) => Some(*dst),
            _ => None,
        });
        let is_item = |v: VarId| Some(shape.copy_root(v)) == item;
        let yielded = source(value)?;
        match &yielded.kind {
            InstKind::AllocBuiltin {
                dst,
                kind: BuiltinKind::Tuple,
                elems,
            } if elems.len() == 2 && !(0..2).any(|i| f.is_starred(*dst, i)) => {
                if is_item(elems[0]) {
                    return Some(Yielded::Items(Wrapper::Keyed));
                }
                let def = |v: VarId| shape.inst(v);
                let types = FxHashMap::default();
                let name = self.cx.str_constant_with(elems[0], &def, &types)?;
                Some(Yielded::PairsOf(name))
            }
            _ => is_item(value).then_some(Yielded::Items(Wrapper::Same)),
        }
    }

    /// The fields of a class that the instruction `kind` reads (`type(obj).model_fields`,
    /// `obj.__dataclass_fields__`, [`ProjectDecls::declared_fields_attr`]) or that a call returns
    /// (`dataclasses.fields(obj)`), as the catalog keeps them (`heki_kb::DeclaredFields`), the
    /// same way for each class the object read or passed may be ([`Self::classes`]).
    fn declared_fields(&self, kind: &InstKind) -> Option<Items> {
        let (project, kb) = (self.cx.project, self.cx.kb);
        let (classes, items) = match kind {
            InstKind::AttrLoad { obj, attr, .. } if kb.keeps_fields_in(attr) => {
                let classes = self.classes(*obj)?;
                let mut items = classes
                    .iter()
                    .map(|c| project.declared_fields_attr(c, attr, kb));
                let first = items.next()??;
                items
                    .all(|i| i == Some(first))
                    .then_some((classes, first))?
            }
            kind => {
                let (callee, _, args) = super::plain_callee(kind, |v| self.types.get(&v))?;
                let items = kb.function_fields(&callee.fqn())?;
                let [obj] = args else { return None };
                (self.classes(*obj)?, items)
            }
        };
        Some(match items {
            FieldItems::Keys => Items::Keys(classes),
            FieldItems::Names => Items::Fields(classes),
        })
    }

    /// The project classes (canonical) of the instance `var` holds: one, or each of a union.
    fn instances(&self, var: VarId) -> Option<Vec<String>> {
        use crate::resolver::Val;
        match self.types.get(&var)? {
            Val::Inst(class) => Some(vec![self.project_class(class)?]),
            Val::Union(classes) => classes.iter().map(|c| self.project_class(c)).collect(),
            _ => None,
        }
    }

    /// The project classes (canonical) of the value `var` holds, an instance or a class object
    /// (named, `cls`, `type(obj)`), or `obj.__class__` when reading it gives the class of the
    /// instance (`ProjectDecls::reads_own_class`).
    fn classes(&self, var: VarId) -> Option<Vec<String>> {
        let project = self.cx.project;
        if let Some(class) = self.types.get(&var).and_then(|v| v.class_object(project)) {
            return Some(vec![self.project_class(class)?]);
        }
        match self.def(var).map(|i| &i.kind) {
            Some(InstKind::AttrLoad { obj, attr, .. }) if attr == "__class__" => {
                let classes = self.instances(*obj)?;
                let kb = self.cx.kb;
                (classes.iter().all(|c| project.reads_own_class(c, kb))).then_some(classes)
            }
            _ => self.instances(var),
        }
    }

    /// The canonical name of the project class `class`, unless it is a protocol, whose values
    /// need not derive from it.
    fn project_class(&self, class: &str) -> Option<String> {
        let project = self.cx.project;
        (project.is_project_class(class) && project.protocol(class).is_none())
            .then(|| project.canonical(class))
    }
}

/// What a generator expression yields, by its outermost iterable ([`Iterated::yielded`]).
enum Yielded {
    /// What the wrapper makes of the items of the iterable.
    Items(Wrapper),
    /// `(key, value)` pairs, each keyed by this string constant.
    PairsOf(String),
}

/// What a call around an iterable gives a loop over its result ([`Iterated::items`]).
#[derive(Debug, Clone, Copy)]
enum Wrapper {
    /// `ns.keys()`, `ns.copy()`: the keys of a namespace or of a mapping of fields.
    Keys,
    /// `ns.items()`: its pairs.
    Pairs,
    /// `dict(...)` of such a mapping or its pairs: its keys.
    Dict,
    /// `list`, `tuple`, `sorted`: the same items.
    Same,
    /// A generator of `(key, value)` pairs keyed by each item: string constants, or the keys of
    /// a mapping.
    Keyed,
}

impl Wrapper {
    fn apply(self, inner: Items) -> Option<Items> {
        match (self, inner) {
            (Self::Keys | Self::Dict, Items::Keys(classes)) => Some(Items::Keys(classes)),
            (Self::Pairs, Items::Keys(classes)) => {
                Some(Items::Pairs(Box::new(Items::Keys(classes))))
            }
            (Self::Dict, Items::Pairs(keys)) => Some(*keys),
            (Self::Same, items) => Some(items),
            (Self::Keyed, keys @ (Items::Constants(_) | Items::Keys(_))) => {
                Some(Items::Pairs(Box::new(keys)))
            }
            _ => None,
        }
    }
}

/// What the key `key` of a store into an object's attributes names (`obj.__dict__[key] = v`,
/// `setattr(obj, key, v)`, `vars(obj).setdefault(key, v)`): the one decision of the names those
/// stores make, for the types of the attributes and for the facts of the ownership contracts.
/// A string constant ([`TypeCtx::str_constant_with`]; `def` gives each variable's defining
/// instruction in the function of `f`, `types` the inferred values) names itself. A key a loop
/// takes from a display of string constants names one of them (a tuple, or a list or set nothing
/// changes, [`Iterated::holds_its_items`]); one it takes from the namespace
/// of an object (`for key in vars(src)`, `src.__dict__.keys()`, the first of each pair of
/// `src.__dict__.items()`, also through a copy, `list`, `tuple`, or `sorted`) names an attribute
/// that object has, and one it takes from the fields a class declares (`type(src).model_fields`
/// and its keys or pairs as for a namespace, the `name` of each of `dataclasses.fields(src)`)
/// names an attribute the class declares. A key built at run time names one with the constant
/// text at its ends ([`built_name`]). Any other key may be any name.
pub(crate) fn key_name<'i>(
    cx: &TypeCtx<'_>,
    f: &IrFunction,
    key: VarId,
    def: &dyn Fn(VarId) -> Option<&'i Inst>,
    types: &FxHashMap<VarId, crate::resolver::Val>,
) -> AttrName<'static> {
    let constant = |v: VarId| cx.str_constant_with(v, def, types);
    if let Some(name) = constant(key) {
        return AttrName::Named(Cow::Owned(name));
    }
    let loops = Iterated {
        cx,
        f,
        def,
        types,
        constant: &constant,
    };
    let items = match loops.def(key).map(|i| &i.kind) {
        Some(InstKind::IterNext { .. }) => loops.loop_items(key),
        // The first of the pair a loop unpacks (`for key, value in ...`).
        Some(InstKind::SubscriptLoad {
            obj,
            index,
            key_const: None,
            is_slice: false,
            ..
        }) if is_int(loops.def(*index), 0) => match loops.loop_items(*obj) {
            Some(Items::Pairs(keys)) => Some(*keys),
            _ => None,
        },
        // The name of the field a loop takes (`for field in dataclasses.fields(obj)`).
        Some(InstKind::AttrLoad { obj, attr, .. }) if attr == "name" => {
            match loops.loop_items(*obj) {
                Some(Items::Fields(classes)) => Some(Items::Keys(classes)),
                _ => None,
            }
        }
        _ => None,
    };
    match items {
        Some(Items::Constants(names)) => AttrName::OneOf(names),
        Some(Items::Keys(classes)) => AttrName::KeyOf(classes),
        Some(Items::Pairs(_) | Items::Fields(_)) | None => built_name(f, key, def, &constant),
    }
}

/// Which end of a string [`built_end`] reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    Head,
    Tail,
}

/// What the string `key` built at run time names ([`AttrName::Built`]): one starting and ending
/// with the constant text [`built_end`] finds at each end, any name when both are empty. A walk
/// that reads all of the text gives it as the head alone, so the two ends never overlap
/// (`"__" + "set" + "attr__"` starts with `__setattr__`). `constant` gives the string constant a
/// variable holds.
fn built_name<'i>(
    f: &IrFunction,
    key: VarId,
    def: &dyn Fn(VarId) -> Option<&'i Inst>,
    constant: &dyn Fn(VarId) -> Option<String>,
) -> AttrName<'static> {
    let (mut head, whole) = built_end(f, key, End::Head, def, constant);
    let mut tail = String::new();
    if !whole {
        match built_end(f, key, End::Tail, def, constant) {
            (text, true) => head = text,
            (text, false) => tail = text,
        }
    }
    match head.is_empty() && tail.is_empty() {
        true => AttrName::Unknown,
        false => AttrName::Built { head, tail },
    }
}

/// The constant text the string `v` starts (`end`) or ends with, through copies, and whether it
/// is all of its text. An f-string has its literal text before its first and after its last
/// replacement field (`IrFunction::str_head`, `IrFunction::str_tail`); `a + b` starts as `a`
/// does, followed by how `b` starts when `a` is a constant, and ends likewise; a `%` or
/// `.format()` of a constant template has the template's text before its first and after its
/// last field ([`template_ends`]). Nothing is known of the ends of any other value, nor past
/// [`BUILT_STEPS`] steps.
fn built_end<'i>(
    f: &IrFunction,
    mut v: VarId,
    end: End,
    def: &dyn Fn(VarId) -> Option<&'i Inst>,
    constant: &dyn Fn(VarId) -> Option<String>,
) -> (String, bool) {
    // The pieces from the end inwards.
    let mut pieces: Vec<String> = Vec::new();
    let joined = |mut pieces: Vec<String>| {
        if end == End::Tail {
            pieces.reverse();
        }
        pieces.concat()
    };
    let mut whole = true;
    for _ in 0..BUILT_STEPS {
        if let Some(text) = constant(v) {
            pieces.push(text);
            return (joined(pieces), whole);
        }
        let Some(inst) = def(v) else { break };
        let template = |template: VarId, format: bool| {
            let (head, tail, whole) = template_ends(&constant(template)?, format);
            Some((if end == End::Head { head } else { tail }, whole))
        };
        let last = match &inst.kind {
            InstKind::Copy { src, .. } => {
                v = *src;
                continue;
            }
            InstKind::BinOp {
                op: BinOpKind::Add,
                lhs,
                rhs,
                in_place: false,
                ..
            } => {
                let (near, far) = match end {
                    End::Head => (*lhs, *rhs),
                    End::Tail => (*rhs, *lhs),
                };
                match constant(near) {
                    Some(text) => {
                        pieces.push(text);
                        v = far;
                    }
                    None => {
                        whole = false;
                        v = near;
                    }
                }
                continue;
            }
            InstKind::BuildString { parts, .. } => Some((
                match end {
                    End::Head => f.str_head(v).unwrap_or_default().to_owned(),
                    End::Tail => f.str_tail(v).to_owned(),
                },
                parts.is_empty(),
            )),
            InstKind::BinOp {
                op: BinOpKind::Mod,
                lhs,
                in_place: false,
                ..
            } => template(*lhs, false),
            InstKind::MethodCall { recv, method, .. } if method == "format" => {
                template(*recv, true)
            }
            _ => None,
        };
        let Some((text, all)) = last else { break };
        pieces.push(text);
        return (joined(pieces), whole && all);
    }
    (joined(pieces), false)
}

/// The methods Python calls with a class as the first argument although no decorator says so:
/// the implicit class methods, and `__new__`.
const CLASS_FIRST_METHODS: &[&str] = &["__init_subclass__", "__class_getitem__", "__new__"];

/// Most steps [`built_end`] takes through copies and `+` operands from one name.
const BUILT_STEPS: usize = 16;

/// The literal text of a `%` template (`format`: false) or of a `str.format()` one before its
/// first and after its last field, and whether it has none. A doubled `%%`, `{{`, or `}}` is the
/// character itself. Formatting with a malformed template (a lone `}`, a `%` ending it) raises,
/// so what such a template gives here is never stored.
fn template_ends(template: &str, format: bool) -> (String, String, bool) {
    let special: &[char] = if format { &['{', '}'] } else { &['%'] };
    let mut chars = template.chars().peekable();
    let (mut head, mut text) = (None, String::new());
    while let Some(c) = chars.next() {
        if !special.contains(&c) || chars.next_if_eq(&c).is_some() || c == '}' {
            text.push(c);
            continue;
        }
        if format {
            // A field may nest fields in its format specification.
            skip_nested(&mut chars, '{', '}');
        } else {
            // A mapping key, flags, width, precision, and a length modifier, then the conversion.
            if chars.next_if_eq(&'(').is_some() {
                skip_nested(&mut chars, '(', ')');
            }
            while chars
                .next_if(|c| c.is_ascii_digit() || "#- +*.hlL".contains(*c))
                .is_some()
            {}
            chars.next();
        }
        head.get_or_insert(std::mem::take(&mut text));
    }
    match head {
        Some(head) => (head, text, false),
        None => (text.clone(), text, true),
    }
}

/// Skip what `chars` holds up to the `close` matching an `open` just read.
fn skip_nested(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, open: char, close: char) {
    let mut depth = 1;
    while depth > 0
        && let Some(c) = chars.next()
    {
        depth = depth + usize::from(c == open) - usize::from(c == close);
    }
}

/// What the keys of `other` name, stored by `ns.update(other)` (as for [`key_name`]): those of
/// the namespace of an object, of its copy, or of its `items()`, name attributes that object
/// has, and those of the fields a class declares attributes the class declares
/// ([`AttrName::KeyOf`]); those of the pairs a generator makes of string constants name these
/// ([`AttrName::OneOf`]); any others may be any names.
pub(crate) fn update_keys<'i>(
    cx: &TypeCtx<'_>,
    f: &IrFunction,
    other: VarId,
    def: &dyn Fn(VarId) -> Option<&'i Inst>,
    types: &FxHashMap<VarId, crate::resolver::Val>,
) -> AttrName<'static> {
    let constant = |v: VarId| cx.str_constant_with(v, def, types);
    let loops = Iterated {
        cx,
        f,
        def,
        types,
        constant: &constant,
    };
    // A mapping gives its keys, pairs the keys they hold; strings are no pairs
    // (`ns.update(["ab"])` stores `a`).
    let keys = match loops.items(other) {
        Some(Items::Pairs(keys)) => Some(*keys),
        Some(keys @ Items::Keys(_)) => Some(keys),
        _ => None,
    };
    match keys {
        Some(Items::Keys(classes)) => AttrName::KeyOf(classes),
        Some(Items::Constants(names)) => AttrName::OneOf(names),
        _ => AttrName::Unknown,
    }
}

/// Whether `inst` is the integer constant `n`.
fn is_int(inst: Option<&Inst>, n: i64) -> bool {
    matches!(
        inst.map(|i| &i.kind),
        Some(InstKind::Const {
            literal: heki_ir::ConstLiteral::Int(k),
            ..
        }) if *k == n
    )
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
/// builtins by their symbol, as call resolution names them); `named` the name a variable
/// holding an attribute name names ([`key_name`]). A method `__setattr__` or `__delattr__`
/// takes the object first when called on a class (`object.__setattr__(obj, name, v)`), as its
/// number of arguments tells.
pub(crate) fn attr_op<'f, 'v>(
    kind: &'f InstKind,
    val: impl Fn(VarId) -> Option<&'v crate::resolver::Val>,
    named: impl Fn(VarId) -> AttrName<'static>,
) -> Option<AttrOp<'f>> {
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
    let replaces = |n: &str| n == "__dict__" || n == "__class__";
    match name {
        AttrName::Named(n) if replaces(&n) => AttrName::Unknown,
        AttrName::OneOf(names) if names.iter().any(|n| replaces(n)) => AttrName::Unknown,
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
/// through it (`named` as for [`attr_op`]).
pub(crate) fn namespace_op<'f>(
    kind: &'f InstKind,
    ns: &FxHashSet<VarId>,
    named: impl Fn(VarId) -> AttrName<'static>,
) -> NamespaceOp<'f> {
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
            let op = attr_op(&inst.kind, |v| self.vals.get(&v), |v| self.key_name(v));
            if let Some(AttrOp::Store { obj, name, .. }) = &op {
                self.on_class(&mut out, *obj, name);
            }
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
                    name: AttrName::OneOf(names),
                    value,
                }) => {
                    for attr in &names {
                        self.named(&mut out, obj, attr, value);
                    }
                }
                Some(AttrOp::Store {
                    obj,
                    name: name @ (AttrName::KeyOf(_) | AttrName::Built { .. } | AttrName::Unknown),
                    value,
                }) => self.unnamed(&mut out, obj, Some(value), &name),
                Some(AttrOp::Namespace { dst, obj }) => namespaces.push((dst, obj)),
                // A deletion binds no value: a read after it raises, or finds what the class
                // holds.
                Some(AttrOp::Delete { .. }) | None => {}
            }
        }
        for (ns, obj) in namespaces {
            self.namespace(&mut out, ns, obj);
        }
        let budget = Budget::of_function("store additions", f);
        let additions: Closure<VarId, Added> = Closure::new();
        for load in &self.s.changed {
            let added = self.s.changes.as_ref().and_then(|changes| {
                additions.ask(
                    &budget,
                    load.dst,
                    |v| self.added_by(changes, v),
                    |v, out| out.extend(changes.aliased_by.get(&v).into_iter().flatten()),
                )
            });
            // Past the budget, or when heki cannot list the changes, they add what heki cannot
            // type.
            let added = added.open_means(Rc::new(Added::unknown()));
            self.changed_in_place(&mut out, load, &added);
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
        (self.s.changed.iter())
            .filter_map(|load| match load.of {
                LoadOf::Name(name) => Some(name),
                LoadOf::Attr(obj, attr) => own_module(obj).then_some(attr),
            })
            .collect()
    }

    /// A change in place of the object the load `load` gives (an item store, a method call
    /// that may add items, an in-place operator): a store, into the name the load reads, of
    /// the loaded type with what the changes add ([`Ty::with_added`]), or for an annotated
    /// global, of its declaration refined by the instance the load gives with what the changes
    /// add ([`Self::declared_store`]). The name keeps its class, and its items only where every
    /// change adds items heki types (`HANDLERS = {"a": run}`, then `HANDLERS[kind] = other`
    /// with `other` of unknown type, keeps no item type). A global read through the module's
    /// own object, which types it by its name alone (`tables.HANDLERS[kind] = other` in a
    /// module importing itself), keeps its declaration, or its type joins what the changes add
    /// once it is established ([`Joins::added`]).
    fn changed_in_place(&self, out: &mut Sink<'_>, load: &ChangedLoad<'_>, added: &Added) {
        // The loaded value's type with what the changes add, when that changes it; another
        // module's names are marked instead, decided by their own module's stores.
        let loaded = self.types.ty(load.dst);
        let with_added = loaded
            .map(|t| (t.with_added(added), t))
            .and_then(|(changed, t)| (changed != *t).then_some(changed));
        let value = with_added.as_ref().map(Some);
        // A load typed by a name says nothing of the object the name holds.
        let changed_object = with_added.as_ref().or(loaded).filter(|t| t.obj().is_some());
        let module = self.cx.module_name();
        match load.of {
            LoadOf::Name(name) => {
                if let Some(fqn) = self.decls.fixed_import(name) {
                    if let Some(foreign) = out.foreign.as_deref_mut() {
                        let fqn = self.cx.project.canonical(fqn);
                        foreign.globals.entry(fqn).or_default().changed_by(added);
                    }
                } else if self.decls.unknown_globals.contains(name) {
                    // An annotation types the items any change adds, as it types every store.
                    let declared = self.declared_store(name, changed_object);
                    if let Some(value) = declared.as_ref().map(Some).or(value) {
                        let target = StoreTarget::Global(name.to_owned());
                        out.own.store(target, false, Some(value));
                    }
                }
            }
            LoadOf::Attr(obj, attr) => {
                for holder in self.holders(obj) {
                    match holder {
                        Holder::Own { class, own_self } => {
                            if let Some(value) = value {
                                let target = StoreTarget::Attr(class.to_owned(), attr.to_owned());
                                out.own
                                    .store(target, own_self && self.s.at_init, Some(value));
                            }
                        }
                        Holder::Module(m) if m == module => {
                            let target = StoreTarget::Global(attr.to_owned());
                            let declared = self.declared_store(attr, changed_object);
                            let named_only = loaded.is_none_or(|t| matches!(t, Ty::Ref(_)));
                            if let Some(value) = declared.as_ref().map(Some).or(value) {
                                out.own.store(target, false, Some(value));
                            } else if named_only {
                                out.own.add(target, added);
                            }
                        }
                        Holder::Module(m) => {
                            if let Some(foreign) = out.foreign.as_deref_mut() {
                                let fqn = format!("{m}.{attr}");
                                foreign.globals.entry(fqn).or_default().changed_by(added);
                            }
                        }
                        Holder::Class(class) => {
                            if let Some(foreign) = out.foreign.as_deref_mut() {
                                let attrs = foreign.attrs.entry(class).or_default();
                                attrs.entry(attr.to_owned()).or_default().changed_by(added);
                            }
                        }
                    }
                }
            }
        }
    }

    /// What the changes of `changes` on the variable `v` itself add to its object
    /// ([`Added::of`]), as this function's types type their operands. A change whose result the
    /// function changes in place again (`d.setdefault(k, []).append(x)`, `xs += ys` and then
    /// `xs.append(x)`) adds items heki does not follow.
    fn added_by(&self, changes: &InPlaceChanges<'_>, v: VarId) -> Added {
        let mut added = Added::default();
        for inst in changes.by_receiver.get(&v).into_iter().flatten() {
            let changed_again = matches!(
                inst.kind,
                InstKind::MethodCall { dst, .. } | InstKind::BinOp { dst, .. }
                    if changes.aliased_by.contains_key(&dst)
            );
            let change = Change::of(&inst.kind).filter(|_| !changed_again);
            added.join(&change.map_or_else(Added::unknown, |(_, c)| Added::of(&c, self.types)));
        }
        added
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
    /// rebind it (`rebinds`); [`Self::declared_store`] when the module annotates it.
    fn global_value(&self, name: &str, value: VarId, rebinds: bool) -> Option<Option<Ty>> {
        let mut out = self.types.stored(value).map(|t| t.cloned());
        if rebinds {
            out = Some(None);
        }
        let known = out.as_ref().and_then(Option::as_ref);
        if let Some(declared) = self.declared_store(name, known) {
            out = Some(Some(declared));
        }
        out
    }

    /// What a store of a value of type `value` (`None`: unknown) binds in the global `name`
    /// the module annotates: its declaration, refined by what the value says beyond it
    /// ([`ProjectDecls::refine`]); `None` for a global it does not annotate.
    fn declared_store(&self, name: &str, value: Option<&Ty>) -> Option<Ty> {
        let declared = (self.declared)(name)?;
        Some(match value {
            Some(t) => self.cx.project.refine(t, &declared, self.cx.kb),
            None => declared,
        })
    }

    /// The name the variable `var` holding an attribute name names ([`key_name`]).
    fn key_name(&self, var: VarId) -> AttrName<'static> {
        self.traced(|def| key_name(self.cx, self.s.f, var, def, &self.vals))
    }

    /// The names the keys of `other` name, stored by `ns.update(other)` ([`update_keys`]).
    fn update_keys(&self, other: VarId) -> AttrName<'static> {
        self.traced(|def| update_keys(self.cx, self.s.f, other, def, &self.vals))
    }

    /// What `trace` tells from the instructions defining the function's variables, for one step
    /// of the budget: past it, any name.
    fn traced(
        &self,
        trace: impl FnOnce(&dyn Fn(VarId) -> Option<&'f Inst>) -> AttrName<'static>,
    ) -> AttrName<'static> {
        if !self.budget.take(1) {
            return AttrName::Unknown;
        }
        trace(&|v| self.s.shape.inst(v))
    }

    /// Note a store under `name` through `obj` when `obj` may be a class object
    /// ([`StoresElsewhere::on_classes`]). A name of unknown value (a parameter, a key of a
    /// namespace or a mapping, a value read from data) is not noted, as R1 takes a special
    /// method to be bound under its own name.
    fn on_class(&self, out: &mut Sink<'_>, obj: VarId, name: &AttrName<'_>) {
        let Some(foreign) = out
            .foreign
            .as_deref_mut()
            .filter(|_| self.may_be_class(obj))
        else {
            return;
        };
        match name {
            AttrName::Named(n) => {
                foreign.on_classes.insert(StoredName::Named(n.to_string()));
            }
            AttrName::OneOf(names) => {
                (foreign.on_classes).extend(names.iter().cloned().map(StoredName::Named));
            }
            AttrName::Built { head, tail } => {
                foreign.on_classes.insert(StoredName::Built {
                    head: head.clone(),
                    tail: tail.clone(),
                });
            }
            AttrName::KeyOf(_) | AttrName::Unknown => {}
        }
    }

    /// Whether the receiver `obj` may be a class object: any value heki does not establish as
    /// an instance. `self` of a method (and `super()` there) is one, except in the methods
    /// Python hands a class first ([`CLASS_FIRST_METHODS`]), and so is a value of a class (what
    /// calling it makes, a parameter or an attribute annotated with it) unless the class may
    /// hold class objects: `type` and `abc.ABCMeta`, `Any`, `object`, and `Callable`
    /// ([`of_any_class`], [`is_any_callable`]), and a project protocol, which a class object
    /// may implement. A project module is none.
    fn may_be_class(&self, obj: VarId) -> bool {
        let selfs = self.s.selfs.as_ref();
        if selfs.is_some_and(|(_, selfs)| selfs.contains(&obj)) {
            // Inference types the first parameter of every method as an instance.
            return CLASS_FIRST_METHODS.contains(&self.s.f.name.as_str());
        }
        let project = self.cx.project;
        let holds_classes = |o: &Obj| {
            matches!(o.class.as_str(), "builtins.type" | "abc.ABCMeta")
                || of_any_class(&o.class)
                || is_any_callable(&o.class)
                || project.protocol(&o.class).is_some()
        };
        match self.types.ty(obj) {
            Some(Ty::Super(_)) => selfs.is_none(),
            Some(Ty::Ref(m)) => project.module(m).is_none(),
            Some(t) if !t.members().is_empty() => t.members().iter().any(holds_classes),
            _ => true,
        }
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
        let stored = StoredValue {
            ty: self.types.stored(value),
            may_be_none: self.may_be_none(value),
        };
        self.store_through(out, obj, attr, stored, global);
    }

    /// Whether `value` may be `None`, which its type leaves out: the `None` constant reaches it
    /// through copies and branches (`Database(url) if url else None`). A value typed by an
    /// annotation allowing `None` is the class it names, as everywhere receivers are typed. Past
    /// the budget it may be.
    fn may_be_none(&self, value: VarId) -> bool {
        let shape = &self.s.shape;
        let f = self.s.f;
        let next = |v: VarId, out: &mut Vec<VarId>| match shape.def(v) {
            Def::Phi(b, i) => {
                let phi = &f.blocks[b.0 as usize].phis[i as usize];
                out.extend(phi.incoming.iter().map(|(_, x)| *x));
            }
            Def::Inst(..) => {
                if let Some(InstKind::Copy { src, .. }) = shape.inst(v).map(|i| &i.kind) {
                    out.push(*src);
                }
            }
            Def::Param | Def::None => {}
        };
        let visit = |v: VarId| match shape.inst(v).map(|i| &i.kind) {
            Some(InstKind::Const {
                literal: heki_ir::ConstLiteral::None,
                ..
            }) => Visit::Stop(()),
            _ => Visit::Expand,
        };
        reach(&self.budget, [value], next, visit) != Answer::Known(None)
    }

    /// A store of a value `value` in the attribute `attr` of what `obj` holds; `global` is
    /// what it binds when `obj` is the module itself. A receiver of no class heki knows may be
    /// an instance of any class ([`StoresElsewhere::untyped_attrs`]).
    fn store_through(
        &self,
        out: &mut Sink<'_>,
        obj: VarId,
        attr: &str,
        value: StoredValue<'_>,
        global: Option<Option<Ty>>,
    ) {
        let module = self.cx.module_name();
        let holders = self.holders(obj);
        if holders.is_empty()
            && let Some(foreign) = out.foreign.as_deref_mut()
        {
            foreign.untyped.insert(attr.to_owned());
        }
        let StoredValue {
            ty: value,
            may_be_none,
        } = value;
        // A store of `None` itself binds no type.
        let may_be_none = may_be_none || value.is_none();
        for holder in holders {
            match holder {
                Holder::Own { class, own_self } => {
                    let target = StoreTarget::Attr(class.to_owned(), attr.to_owned());
                    let establishing = own_self && self.s.at_init;
                    if !own_self || may_be_none {
                        out.own.unowned.insert(target.clone());
                    }
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
                        stored.none |= may_be_none;
                    }
                }
            }
        }
    }

    /// A store of an attribute heki cannot name or pair with its value (`name`:
    /// [`AttrName::KeyOf`], or any other: the keys of the pairs `ns.update(pairs)` stores, whose
    /// values heki does not follow) of what `obj` holds, binding `value` when one value is known
    /// (`__setstate__` restores from its state). One under a key of the namespace of an
    /// instance of the class or a subclass (a copy) adds no attribute the class's instances do
    /// not have ([`ProjectDecls::keys_exist_in`]).
    fn unnamed(&self, out: &mut Sink<'_>, obj: VarId, value: Option<VarId>, name: &AttrName<'_>) {
        let sources = match name {
            AttrName::KeyOf(sources) => Some(sources),
            _ => None,
        };
        let holders = self.holders(obj);
        if holders.is_empty()
            && let Some(foreign) = out.foreign.as_deref_mut()
        {
            foreign.untyped_unnamed = true;
        }
        for holder in holders {
            match holder {
                Holder::Own { class, own_self } => {
                    let restores = own_self && value.is_some_and(|v| self.holds_state(v));
                    if restores {
                        out.own.restoring.insert(class.to_owned());
                        continue;
                    }
                    out.own.any.insert(class.to_owned());
                    let fqn = format!("{}.{class}", self.cx.module_name());
                    let copies = sources.is_some_and(|sources| {
                        (sources.iter()).all(|source| self.cx.project.keys_exist_in(source, &fqn))
                    });
                    if !copies {
                        out.own.adding.insert(class.to_owned());
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
            return self.unnamed(out, obj, None, &AttrName::Unknown);
        }
        let mut named: Vec<(String, VarId)> = Vec::new();
        let mut unnamed: Vec<(Option<VarId>, AttrName<'static>)> = Vec::new();
        let mut sorted: Vec<VarId> = held.iter().copied().collect();
        sorted.sort_unstable();
        for v in &sorted {
            for u in shape.readers(*v) {
                if !self.budget.take(1) {
                    return self.unnamed(out, obj, None, &AttrName::Unknown);
                }
                match *u {
                    Use::Phi(b, i) => {
                        let phi = &f.blocks[b.0 as usize].phis[i as usize];
                        if !phi.incoming.iter().all(|(_, x)| held.contains(x)) {
                            unnamed.push((None, AttrName::Unknown));
                        }
                    }
                    Use::Terminator(b) => {
                        let branch =
                            matches!(f.blocks[b.0 as usize].terminator, Terminator::Branch { .. });
                        if !branch {
                            unnamed.push((None, AttrName::Unknown));
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
        for (value, name) in unnamed {
            self.unnamed(out, obj, value, &name);
        }
    }

    /// What `kind`, which reads a namespace (`held`), stores through it ([`namespace_op`]).
    fn namespace_use(
        &self,
        kind: &InstKind,
        held: &FxHashSet<VarId>,
        named: &mut Vec<(String, VarId)>,
        unnamed: &mut Vec<(Option<VarId>, AttrName<'static>)>,
    ) {
        match namespace_op(kind, held, |v| self.key_name(v)) {
            NamespaceOp::Reads | NamespaceOp::Removes(_) | NamespaceOp::RemovesAll => {}
            NamespaceOp::Stores {
                name: AttrName::Named(attr),
                value,
            } => named.push((attr.into_owned(), value)),
            NamespaceOp::Stores {
                name: AttrName::OneOf(attrs),
                value,
            } => named.extend(attrs.into_iter().map(|attr| (attr, value))),
            NamespaceOp::Stores {
                name: AttrName::KeyOf(sources),
                value,
            } => unnamed.push((Some(value), AttrName::KeyOf(sources))),
            NamespaceOp::Stores { value, .. } => unnamed.push((Some(value), AttrName::Unknown)),
            NamespaceOp::Updates(keywords) => {
                for (keyword, value) in keywords {
                    match keyword {
                        Some(attr) => named.push((attr.clone(), *value)),
                        None => unnamed.push((Some(*value), self.update_keys(*value))),
                    }
                }
            }
            NamespaceOp::StoresAny { value } => {
                let name = value.map_or(AttrName::Unknown, |other| self.update_keys(other));
                unnamed.push((value, name));
            }
            NamespaceOp::HandsOn => unnamed.push((None, AttrName::Unknown)),
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

/// What a store binds ([`Classifier::store_through`]).
struct StoredValue<'t> {
    /// As [`Inferred::stored`] gives it.
    ty: Option<Option<&'t Ty>>,
    /// The value may be `None`, which `ty` leaves out ([`Classifier::may_be_none`]).
    may_be_none: bool,
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
    /// The attributes some module stores through a receiver of no class heki knows
    /// ([`StoresElsewhere::untyped_attrs`]).
    untyped_attrs: FxHashSet<String>,
    /// Some module stores under a name heki cannot read through such a receiver
    /// ([`StoresElsewhere::untyped_unnamed`]).
    untyped_unnamed: bool,
    /// The constant names some module stores under through a receiver that may be a class
    /// object ([`StoresElsewhere::on_classes`]).
    on_classes: FxHashSet<String>,
    /// The `head` and `tail` of the names built at run time some module stores under through
    /// such a receiver ([`StoredName::Built`]).
    on_classes_built: FxHashSet<(String, String)>,
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
        let untyped = (self.untyped_attrs).symmetric_difference(&after.untyped_attrs);
        out.extend(untyped.map(|attr| untyped_key(attr)));
        if self.untyped_unnamed != after.untyped_unnamed {
            out.insert(UNTYPED_UNNAMED_KEY.to_owned());
        }
        let on_classes = (self.on_classes).symmetric_difference(&after.on_classes);
        out.extend(on_classes.map(|name| on_classes_key(name)));
        if self.on_classes_built != after.on_classes_built {
            out.insert(ON_CLASSES_BUILT_KEY.to_owned());
        }
        for module in self.held.keys().chain(after.held.keys()) {
            if self.held.get(module) != after.held.get(module) {
                out.insert(module.clone());
            }
        }
        out
    }
}

impl StoredElsewhere {
    /// Whether every store binds `None`, which adds nothing to the name's type.
    fn binds_none_alone(&self) -> bool {
        self.none && self.ty.is_none() && !self.in_place && self.changed.is_none()
    }

    /// Join what another module's stores bind.
    fn join(&mut self, other: &StoredElsewhere) {
        self.in_place |= other.in_place;
        self.none |= other.none;
        if let Some(added) = &other.changed {
            join_added(&mut self.changed, added);
        }
        self.ty = match (self.ty.take(), &other.ty) {
            (t, None) => t,
            (None, t) => t.clone(),
            (Some(Some(a)), Some(Some(b))) => Some(a.join(b)),
            (Some(_), Some(_)) => Some(None),
        };
    }

    /// The join of `own` and what these stores bind, with what the changes in place add:
    /// `None` when it is unknown or rebinding in place may change it.
    fn joined_with(&self, own: &Ty) -> Option<Ty> {
        let t = match &self.ty {
            Some(other) => own.join(other.as_ref()?)?,
            None => own.clone(),
        };
        let t = match &self.changed {
            Some(added) => t.with_added(added),
            None => t,
        };
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
            out.untyped_attrs
                .extend(stores.untyped_attrs.iter().cloned());
            out.untyped_unnamed |= stores.untyped_unnamed;
            for name in &stores.on_classes {
                match name {
                    StoredName::Named(n) => out.on_classes.insert(n.clone()),
                    StoredName::Built { head, tail } => {
                        out.on_classes_built.insert((head.clone(), tail.clone()))
                    }
                };
            }
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
                    // A store of `None` alone binds nothing a read of the name gives.
                    let binding = attrs.iter().filter(|(_, t)| !t.binds_none_alone());
                    into.attrs.extend(binding.map(|(attr, _)| attr.clone()));
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
            Some(stored) if stored.changed.is_some() && m.declared_items.contains_key(name) => {
                let declared = &m.declared_items[name];
                StoredElsewhere {
                    changed: None,
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

    /// Whether another module stores into `attr` of the instances of the canonical class
    /// `class` through a receiver of the class, by name.
    pub(super) fn attr_stored_elsewhere(&self, class: &str, attr: &str) -> bool {
        self.read(|k| note(&mut k.grown_globals, class));
        (self.elsewhere.attrs.get(class)).is_some_and(|attrs| attrs.contains_key(attr))
    }

    /// Whether some module stores `attr`, by name or under a name heki cannot read, through a
    /// receiver of no class heki knows ([`StoresElsewhere::untyped_attrs`],
    /// [`StoresElsewhere::untyped_unnamed`]). Which instances such a receiver holds is not
    /// decided, so the second leaves every attribute of every class open.
    pub(super) fn attr_stored_untyped(&self, attr: &str) -> bool {
        self.read(|k| note(&mut k.grown_globals, &untyped_key(attr)));
        self.read(|k| note(&mut k.grown_globals, UNTYPED_UNNAMED_KEY));
        self.elsewhere.untyped_attrs.contains(attr) || self.elsewhere.untyped_unnamed
    }

    /// Whether some module may store `name` through a receiver that may be a class object
    /// ([`StoresElsewhere::on_classes`]): under `name` itself, or under a name built at run time
    /// that starts and ends as `name` does, with room for both ends. A store under a name of
    /// unknown value binds none of the names asked about here (`Classifier::on_class`).
    pub(super) fn bound_on_classes(&self, name: &str) -> bool {
        self.read(|k| note(&mut k.grown_globals, &on_classes_key(name)));
        self.read(|k| note(&mut k.grown_globals, ON_CLASSES_BUILT_KEY));
        let built = |(head, tail): &(String, String)| {
            name.len() >= head.len() + tail.len()
                && name.starts_with(head.as_str())
                && name.ends_with(tail.as_str())
        };
        self.elsewhere.on_classes.contains(name)
            || self.elsewhere.on_classes_built.iter().any(built)
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
    /// annotated attribute's declared type; a bare container annotation's class, `list`, is
    /// joined only where `__init__` does not establish the attribute), and what other modules
    /// store into its instances; `None` for none or an unknown one, and when a class's instances
    /// get attributes heki cannot name.
    pub(super) fn join_family(&self, family: &[String], attr: &str) -> Option<Ty> {
        let mut out = Stored::Nothing;
        let mut restoring = false;
        let mut in_place = false;
        let mut changed: Option<Added> = None;
        for (class, decl) in family
            .iter()
            .filter_map(|class| Some((class, self.class(class)?)))
        {
            if self.stores_any_attr(class, decl) {
                return None;
            }
            restoring |= decl.restores_state;
            let declared = decl.attr_types.get(attr);
            if let Some(t) = declared.filter(|t| !t.is_bare_container()) {
                out.add(Some(Some(t)));
                continue;
            }
            in_place |= decl.in_place_attrs.iter().any(|a| &**a == attr);
            let store = self.attr_store(class, decl, attr);
            match store {
                Some(AttrStore::Established(t) | AttrStore::Of(t)) => out.add(Some(Some(t))),
                Some(AttrStore::Open) => return None,
                None => {}
            }
            // The class's instances start with a value its stores do not give unless its
            // `__init__` establishes the attribute (a dataclass's generated `__init__` binds
            // one): any value of the annotation's class.
            if let Some(bare) = declared
                && !matches!(store, Some(AttrStore::Established(_)))
            {
                out.add(Some(Some(bare)));
            }
            if let Some(stored) = self.elsewhere.attrs.get(class).and_then(|a| a.get(attr)) {
                in_place |= stored.in_place;
                if let Some(added) = &stored.changed {
                    join_added(&mut changed, added);
                }
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
        let t = out.type_of().map(|t| match &changed {
            Some(added) => t.with_added(added),
            None => t,
        });
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
