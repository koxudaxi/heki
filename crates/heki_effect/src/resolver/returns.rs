//! What calling a project function gives by what every `return` of it gives
//! ([`super::FunctionDecl::returns`]), and what the items of a call's result are
//! ([`TypeCtx::call_items`]): the one reading of a call's result that typing and the rules'
//! item origins share.
//!
//! The return fact is pass 1 ([`record_return_facts`]): it is inferred against the module's own
//! declarations, completed against themselves (the globals and attributes the module's own code
//! stores), so it stays a function of the module's text and the declarations do not depend on
//! the order in which modules were edited. A value another module gives is of no type there, and
//! a function returning one gets no fact.

use super::{
    Callee, FunctionDecl, ModuleDecls, ProjectDecls, Ty, TypeCtx, Val, complete_decls,
    is_generator_function, is_summarized_function, keeps_body_result, note,
};
use crate::infer::{Called, HandedArgs, Inferred, MAX_HANDED_CALLS};
use crate::partial::Partial;
use crate::{FuncKey, ModuleInput};
use heki_ir::walk::{Answer, Budget, FnShape};
use heki_ir::{
    FunctionKind, Inst, InstKind, IrFunction, ItemOrigins, ItemSource, Terminator, VarId,
};
use heki_kb::{EffectiveEcosystemKb, LiteralValue, ResultItems};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::rc::Rc;

/// What every `return` of a function gives, past what its annotation says
/// ([`super::FunctionDecl::returns`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReturnFact {
    /// The items of the returned value, when the data decides how many there are: the rows,
    /// lines, or entries of a catalog call, or what a project function or an offloaded call
    /// returns of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<ReturnedItems>,
    /// The class of the value every return gives, when it is narrower than the annotation
    /// ([`ProjectDecls::narrower`]: a connection on a file of `-> sqlite3.Connection`, a subclass
    /// of the annotated class): calling the function gives that, as a local or a global
    /// annotated with the class keeps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ty: Option<Ty>,
    /// The module globals the definitions read (fqns), which `ty` is inferred from as their
    /// module binds them: once another module rebinds one, `ty` says nothing
    /// ([`super::FunctionDecl::declared_return`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ty_globals: Vec<String>,
    /// For a method, the attribute of its receiver every return gives (`return self._db`, also
    /// awaited of an `async def`): what a call on an instance gives is the one class the stores
    /// of that attribute bind, which the whole project decides ([`ProjectDecls::held_attr`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_attr: Option<Box<str>>,
}

/// Items whose number the data decides ([`ReturnFact::items`], [`CallItems`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReturnedItems {
    /// What the catalog call they come from gives ([`heki_kb::EcosystemEntry::items`]).
    pub kind: ResultItems,
    /// They are the pieces `str.splitlines` cuts the call's text into.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pieces: bool,
    /// Some of the call's items may be left out (a filter, a condition of a comprehension).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dropped: bool,
    /// Only their count follows the call's: one item for each of its items (`[f(r) for r in
    /// rows]`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub counted: bool,
    /// The library API of the call, as reports name it.
    pub api: String,
}

/// The items of a call's result ([`TypeCtx::call_items`]), and the project function that
/// returns them, when the call is of one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallItems {
    pub items: ReturnedItems,
    pub through: Option<FuncKey>,
}

/// Why a call's result has no items [`TypeCtx::call_items`] follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoItems {
    /// A project call heki does not follow to items, or a call it does not resolve.
    NotFollowed,
    /// A library call the catalog does not size.
    Unsized,
}

/// The most passes deciding a module's return facts ([`ReturnFacts::in_passes`]): a function
/// returning what another returns (`a` returning `b()` returning `c()`) is decided a pass after
/// it, so a function more than this many functions down such a chain gets no fact.
const MAX_RETURN_PASSES: usize = 8;

impl TypeCtx<'_> {
    /// What the items of the result of the call `inst` are, when the data decides how many
    /// there are: those the catalog gives a library call ([`heki_kb::EcosystemEntry::items`]),
    /// those `returned` gives a project function (its [`ReturnFact::items`]), and for a call
    /// handing back what a callable it is handed returns ([`heki_kb::ResultOf`]:
    /// `asyncio.to_thread(f, x)`), those of calling the callable, through at most
    /// [`MAX_HANDED_CALLS`] such calls, as typing follows them. A `functools.partial` object
    /// calls the callable it holds. `types` and `def` are those of the function making the call.
    pub fn call_items<'i>(
        &self,
        types: &FxHashMap<VarId, Val>,
        def: &dyn Fn(VarId) -> Option<&'i Inst>,
        inst: &'i Inst,
        returned: &dyn Fn(&FuncKey) -> Option<ReturnedItems>,
    ) -> Result<CallItems, NoItems> {
        let (mut called, mut passed) = match &inst.kind {
            InstKind::Call {
                callee_var: Some(c),
                args,
                kwargs,
                star_from,
                ..
            } => (Called::Value(*c), HandedArgs::new(args, kwargs, *star_from)),
            InstKind::MethodCall {
                recv,
                method,
                args,
                kwargs,
                star_from,
                ..
            } => (
                Called::Method(*recv, method),
                HandedArgs::new(args, kwargs, *star_from),
            ),
            _ => return Err(NoItems::NotFollowed),
        };
        let mut handed_calls = 0;
        loop {
            if let Called::Value(v) = called
                && let Some(partial) = self.partial_object(types, def, v)
            {
                let (callable, args) = passed
                    .through_partial(partial)
                    .ok_or(NoItems::NotFollowed)?;
                (called, passed) = (Called::Value(callable), args);
            }
            let callee = match called {
                Called::Value(v) => match bound_method(def, v) {
                    Some((recv, method)) => self.resolve_method(types.get(&recv), method),
                    None => self.resolve_call(types.get(&v)),
                },
                Called::Method(recv, method) => self.resolve_method(types.get(&recv), method),
            };
            let symbol = match callee {
                Callee::External(symbol) => symbol,
                Callee::Project(keys, _) => return project_items(&keys, returned),
                Callee::TooMany | Callee::Unresolved => return Err(NoItems::NotFollowed),
            };
            let entry = self.catalog(&symbol).ok_or(NoItems::Unsized)?;
            let Some(of) = entry.result_of else {
                let kind = entry.items.ok_or(NoItems::Unsized)?;
                let api = entry.api.clone().unwrap_or(symbol);
                return Ok(CallItems {
                    items: ReturnedItems::of_call(kind, api),
                    through: None,
                });
            };
            let (callable, handed) = HandedArgs::of(of, passed.passed())
                .filter(|_| handed_calls < MAX_HANDED_CALLS)
                .ok_or(NoItems::NotFollowed)?;
            handed_calls += 1;
            called = Called::Value(callable);
            passed = handed;
        }
    }

    /// [`Self::call_items`] with the return facts of the declarations.
    pub fn call_items_by_declarations<'i>(
        &self,
        types: &FxHashMap<VarId, Val>,
        def: &dyn Fn(VarId) -> Option<&'i Inst>,
        inst: &'i Inst,
    ) -> Result<CallItems, NoItems> {
        let project = self.project;
        self.call_items(types, def, inst, &|key| returned_items(project, key))
    }

    /// The `functools.partial` object this function makes that `v` holds ([`Partial::of`]).
    fn partial_object<'i>(
        &self,
        types: &FxHashMap<VarId, Val>,
        def: &dyn Fn(VarId) -> Option<&'i Inst>,
        v: VarId,
    ) -> Option<Partial> {
        let copied = |v: VarId| {
            let mut cur = v;
            while let Some(InstKind::Copy { src, .. }) = def(cur).map(|i| &i.kind) {
                cur = *src;
            }
            cur
        };
        Partial::of(v, copied, |object| {
            self.partial_parts(&def(object)?.kind, &|v| types.get(&v))
        })
    }

    /// The parts of a `functools.partial(f, ...)` call `kind` ([`crate::partial::parts`]): `f`,
    /// the positional arguments after it up to the first one `*` unpacks, the keywords, and
    /// whether one is unpacked; `val` gives the values the call's operands hold.
    #[allow(clippy::type_complexity)]
    pub(crate) fn partial_parts<'k, 'v>(
        &self,
        kind: &'k InstKind,
        val: &dyn Fn(VarId) -> Option<&'v Val>,
    ) -> Option<(VarId, &'k [VarId], &'k [(Option<String>, VarId)], bool)> {
        let (callee, args, kwargs, star_from) = match kind {
            InstKind::Call {
                callee_var: Some(c),
                args,
                kwargs,
                star_from,
                ..
            } => (self.resolve_call(val(*c)), args, kwargs, star_from),
            InstKind::MethodCall {
                recv,
                method,
                args,
                kwargs,
                star_from,
                ..
            } => (
                self.resolve_method(val(*recv), method),
                args,
                kwargs,
                star_from,
            ),
            _ => return None,
        };
        matches!(callee, Callee::External(s) if s == "functools.partial").then_some(())?;
        crate::partial::parts(args, kwargs, *star_from)
    }

    /// The most items the call `inst` gives when the code fixes how many: the count of a
    /// statement whose text fixes its rows (`LIMIT 3`, an aggregate without `GROUP BY`), the
    /// integer passed where the catalog says the call gives at most that many
    /// ([`heki_kb::EcosystemEntry::items_up_to`]: `fetchmany(3)`), and for a call on a receiver
    /// that a call giving the same kind of items made (`fetchall()` of what `execute(...)`
    /// returns), at most the receiver's. `types` and `def` are those of the function making the
    /// call.
    pub fn fixed_call_items<'i>(
        &self,
        types: &FxHashMap<VarId, Val>,
        def: &dyn Fn(VarId) -> Option<&'i Inst>,
        inst: &'i Inst,
    ) -> Option<u32> {
        let mut fewest: Option<u32> = None;
        let mut at = inst;
        let mut kind: Option<ResultItems> = None;
        // A chain of calls on the result of the one before (`execute(...).fetchall()`).
        for _ in 0..MAX_HANDED_CALLS {
            let (callee, recv, args, kwargs, star_from) = match &at.kind {
                InstKind::Call {
                    callee_var,
                    args,
                    kwargs,
                    star_from,
                    ..
                } => (
                    self.resolve_call(callee_var.and_then(|c| types.get(&c))),
                    None,
                    args,
                    kwargs,
                    *star_from,
                ),
                InstKind::MethodCall {
                    recv,
                    method,
                    args,
                    kwargs,
                    star_from,
                    ..
                } => (
                    self.resolve_method(types.get(recv), method),
                    Some(*recv),
                    args,
                    kwargs,
                    *star_from,
                ),
                _ => break,
            };
            let Callee::External(symbol) = callee else {
                break;
            };
            let Some(entry) = self.catalog(&symbol) else {
                break;
            };
            if entry.items.is_none() || kind.is_some_and(|k| Some(k) != entry.items) {
                break;
            }
            kind = entry.items;
            let up_to = entry.items_up_to.as_ref().and_then(|slot| {
                let v =
                    super::passed_var(args, kwargs, star_from, (slot.arg, slot.param.as_deref()))?;
                match super::literal(def, &|i| self.module_constant(i, &|_| None), v)? {
                    LiteralValue::Int(n) => u32::try_from(n.max(0)).ok(),
                    _ => None,
                }
            });
            let statement = crate::summary::statements::runs_statement(&symbol)
                .then(|| args.first())
                .flatten()
                .and_then(|v| self.str_constant_with(*v, def, types))
                .and_then(|text| crate::summary::statements::fixed_rows(&text));
            for n in [up_to, statement].into_iter().flatten() {
                fewest = Some(fewest.map_or(n, |f| f.min(n)));
            }
            let Some(next) = recv.and_then(def) else {
                break;
            };
            at = next;
        }
        fewest
    }
}

impl TypeCtx<'_> {
    /// What calling `method` on an instance of the project class `class` gives by the return
    /// facts of the definitions the call may run: `found`, the one the MRO finds, and the
    /// override of every project subclass ([`ProjectDecls::method_targets`]), when they are every
    /// definition an instance may run ([`ProjectDecls::instance_targets_complete`]). Each gives
    /// its fact ([`FunctionDecl::inferred_return`], or the class of the attribute it returns,
    /// [`ProjectDecls::held_attr`]) or else its annotation, `Self` bound to `self_ty`; the call
    /// gives what they agree on. A type checker holds every override to the annotation of the
    /// definition it overrides, which `None` leaves the call; the fact of one definition says
    /// nothing of another. Without a fact of `found`, what they agree on is its annotation.
    pub(crate) fn dispatched_return(
        &self,
        class: &str,
        method: &str,
        found: &FunctionDecl,
        self_ty: &Ty,
    ) -> Option<Ty> {
        let fact = found.returns.as_deref()?;
        if fact.self_attr.is_none() && fact.ty.is_none() {
            return None;
        }
        let project = self.project;
        let callee = project.method_targets(class, method, false, self.kb);
        let Callee::Project(keys, _) = &callee else {
            return None;
        };
        let decls: Vec<&FunctionDecl> = (keys.iter())
            .map(|k| project.function_decl(k))
            .collect::<Option<_>>()?;
        if !project.instance_targets_complete(class, method, &callee, self.kb) {
            return None;
        }
        let mut each: Vec<Ty> = Vec::with_capacity(decls.len());
        for d in decls {
            if d.reads_as_property() || d.returned_import.is_some() {
                return None;
            }
            let attr = d.returns.as_deref().and_then(|r| r.self_attr.as_deref());
            let returned = match (attr, d.inferred_return(project)) {
                (Some(attr), _) => project.held_attr(class, attr)?,
                (None, Some(t)) => project.declared_ty(t)?,
                (None, None) => project.declared_ty(d.return_type.as_ref()?)?,
            };
            each.push(d.call_type(returned.bind_self(Some(self_ty))?));
        }
        let first = each.pop()?;
        each.iter().all(|t| *t == first).then_some(first)
    }
}

impl ProjectDecls {
    /// The class of the value reading `attr` on an instance of the project class `class` gives,
    /// when the class's own methods decide it: its Priority 3 type ([`Self::member_type`]: an
    /// annotation, or the join of the stores where `__init__` or `__post_init__` establishes it)
    /// is an instance of one class, and every class whose code may store into such an instance
    /// ([`Self::instance_family`]) stores the attribute only through `self` in its own methods,
    /// never a value that may be `None` ([`super::ClassDecl::unowned_attrs`]). A store from
    /// another module, a store anywhere through a receiver of no class heki knows, which may be
    /// such an instance ([`super::StoresElsewhere::untyped_attrs`]), a store under a name heki
    /// cannot read through such a receiver ([`super::StoresElsewhere::untyped_unnamed`]), and a
    /// getter of the name leave it open. The answer is kept per class and attribute for the life of the indexes, so
    /// the calls of a getter walk the family once.
    pub(crate) fn held_attr(&self, class: &str, attr: &str) -> Option<Ty> {
        self.read(|k| note(&mut k.property_names, attr));
        if self.property_names.contains(attr) || self.attr_stored_untyped(attr) {
            return None;
        }
        let class = self.canonical(class);
        let key = (class.clone(), attr.to_owned());
        self.kept(&self.held_attrs, key, || {
            for c in self.instance_family(&class) {
                let decl = self.class(&c)?;
                let unowned = (decl.unowned_attrs.binary_search_by(|a| (**a).cmp(attr))).is_ok();
                if unowned || self.attr_stored_elsewhere(&c, attr) {
                    return None;
                }
            }
            let t = self.declared_ty(&self.member_type(&class, attr, false)?)?;
            matches!(t, Ty::Inst(_)).then_some(t)
        })
    }
}

/// The items a call of the project functions `keys` gives: those every one returns
/// (`returned`), when they agree.
fn project_items(
    keys: &[FuncKey],
    returned: &dyn Fn(&FuncKey) -> Option<ReturnedItems>,
) -> Result<CallItems, NoItems> {
    let mut each = keys.iter().map(returned);
    let first = each.next().flatten().ok_or(NoItems::NotFollowed)?;
    each.all(|items| items.as_ref() == Some(&first))
        .then(|| CallItems {
            items: first,
            through: (keys.len() == 1).then(|| keys[0].clone()),
        })
        .ok_or(NoItems::NotFollowed)
}

impl ReturnedItems {
    /// The items of a catalog call's result, `api` as reports name the call.
    fn of_call(kind: ResultItems, api: String) -> Self {
        Self {
            kind,
            pieces: false,
            dropped: false,
            counted: false,
            api,
        }
    }

    /// What a value made from these items gives: the pieces `str.splitlines` cuts their text
    /// into (`text`), some of them (`dropped`), and only their count (`counted`). Pieces of
    /// pieces, and a count of items some of which may be left out, are none heki names.
    pub fn read_through(self, text: bool, dropped: bool, counted: bool) -> Option<Self> {
        let dropped = self.dropped || dropped;
        let counted = self.counted || counted;
        (!(self.pieces && text) && !(dropped && counted)).then_some(Self {
            pieces: self.pieces || text,
            dropped,
            counted,
            ..self
        })
    }

    /// What a report calls the items: none for text iterated as it is (its characters) or
    /// pieces of something other than text.
    pub fn named(&self) -> Option<&'static str> {
        match (self.kind, self.pieces) {
            (ResultItems::Rows, false) => Some("the rows"),
            (ResultItems::Lines, false) => Some("the lines"),
            (ResultItems::Entries, false) => Some("the entries"),
            (ResultItems::Text, true) => Some("the lines of the text"),
            _ => None,
        }
    }
}

/// The items `key`'s return fact gives ([`ReturnFact::items`]).
fn returned_items(project: &ProjectDecls, key: &FuncKey) -> Option<ReturnedItems> {
    project.function_decl(key)?.returns.as_ref()?.items.clone()
}

/// The receiver and method name of the bound method `v` holds (`m = obj.method`, through
/// copies).
fn bound_method<'i>(def: &dyn Fn(VarId) -> Option<&'i Inst>, v: VarId) -> Option<(VarId, &'i str)> {
    let mut cur = v;
    loop {
        match &def(cur)?.kind {
            InstKind::Copy { src, .. } => cur = *src,
            InstKind::AttrLoad { obj, attr, .. } => return Some((*obj, attr.as_str())),
            _ => return None,
        }
    }
}

/// [`super::FunctionDecl::returns`] of the module's functions (pass 1): what every `return` of every
/// definition of a name gives, inferred against the module's own declarations completed against
/// themselves. Only the functions some of whose returned items come from calls are inferred.
pub(super) fn record_return_facts(
    input: &ModuleInput<'_>,
    decls: &mut ModuleDecls,
    kb: &EffectiveEcosystemKb,
) {
    let origins = ItemOrigins::new(input.ir);
    let mut candidates: FxHashMap<&str, Vec<Returns>> = FxHashMap::default();
    let mut ruled_out: Vec<&str> = Vec::new();
    for f in input
        .ir
        .functions
        .iter()
        .filter(|f| is_summarized_function(f))
    {
        match returned_origins(&origins, decls, f) {
            Some(returns) => candidates.entry(&f.qualname).or_default().push(returns),
            None => ruled_out.push(&f.qualname),
        }
    }
    for q in ruled_out {
        candidates.remove(q);
    }
    let mut decided: FxHashMap<&str, ReturnFact> = FxHashMap::default();
    // Written with the other facts once they are decided: what a call of a method gives depends
    // on the project's stores and subclasses, which the module's own declarations do not see.
    for (qualname, attr) in returned_self_attrs(input, decls) {
        decided.entry(qualname).or_default().self_attr = Some(attr);
    }
    let typed = typed_candidates(input, decls, kb);
    if candidates.is_empty() && typed.is_empty() {
        write_return_facts(decls, decided);
        return;
    }
    let own = ProjectDecls::new([decls.clone()]);
    let completed = ProjectDecls::new([complete_decls(input, decls.clone(), &own, kb)]);
    let facts = ReturnFacts {
        cx: TypeCtx::new(input, &completed, kb),
        candidates: &candidates,
        inferred: RefCell::default(),
        types: RefCell::default(),
    };
    for (qualname, items) in facts.in_passes() {
        decided.entry(qualname).or_default().items = Some(items);
    }
    for (qualname, definitions) in &typed {
        if let Some(ty) = facts.returned_type(definitions) {
            let fact = decided.entry(qualname).or_default();
            fact.ty = Some(ty);
            fact.ty_globals = facts.globals_read(decls, definitions);
        }
    }
    write_return_facts(decls, decided);
}

fn write_return_facts(decls: &mut ModuleDecls, decided: FxHashMap<&str, ReturnFact>) {
    for (qualname, fact) in decided {
        if let Some(d) = decls.functions.get_mut(qualname) {
            d.returns = Some(Box::new(fact));
        }
    }
}

/// The methods of the module whose every definition returns the same attribute of the instance
/// it is called on ([`ReturnFact::self_attr`]: `return self._db`, through copies, which
/// [`super::handed_back_param`] decides), by qualname.
fn returned_self_attrs<'f>(
    input: &'f ModuleInput<'_>,
    decls: &ModuleDecls,
) -> Vec<(&'f str, Box<str>)> {
    let mut each: FxHashMap<&str, Option<Box<str>>> = FxHashMap::default();
    for f in input
        .ir
        .functions
        .iter()
        .filter(|f| is_summarized_function(f))
    {
        let on_instance = (decls.functions.get(&f.qualname))
            .is_some_and(|d| d.is_method && !d.is_staticmethod && !d.is_classmethod);
        let attr = match on_instance.then(|| super::handed_back_param(f, decls)) {
            Some(Some((0, Some(attr)))) => Some(Box::from(attr)),
            _ => None,
        };
        each.entry(&f.qualname)
            .and_modify(|a| {
                if *a != attr {
                    *a = None;
                }
            })
            .or_insert(attr);
    }
    let mut out: Vec<(&str, Box<str>)> = (each.into_iter())
        .filter_map(|(qualname, attr)| Some((qualname, attr?)))
        .collect();
    out.sort_unstable();
    out
}

/// One definition whose returned values may say more than its annotation: the values, and what
/// they may say.
struct TypedReturns<'f> {
    f: &'f IrFunction,
    values: Vec<VarId>,
    returned: Returned,
}

/// What the values a definition returns may say beyond its annotation ([`typed_candidates`]).
enum Returned {
    /// A class narrower than the annotated one ([`ProjectDecls::narrower`]).
    Narrower(Ty),
    /// The named objects they are (a `def` or `lambda` the function makes, a function or class a
    /// name binds), where the annotation names no class (none, `Any`, `object`, `Callable`).
    Named,
    /// The instance of one class that the calls they are give (a construction of the class, a
    /// call the declarations or the catalog give a class), where the annotation names no class.
    Called,
}

/// The functions of the module whose every definition returns a value on every `return` and
/// whose values may say more than the annotation: one annotated with a class a narrower one may
/// specialize ([`ProjectDecls::specializes`]: a library class some catalog class has the methods
/// of, `sqlite3.Connection` of a connection on a file, or a class of the module that another of
/// its classes names as a base), or one whose annotation names no class and every value of
/// which may be a named object ([`may_return_named`]) or is what a call gives
/// ([`returned_without_class`]).
fn typed_candidates<'f>(
    input: &'f ModuleInput<'_>,
    decls: &ModuleDecls,
    kb: &EffectiveEcosystemKb,
) -> FxHashMap<&'f str, Vec<TypedReturns<'f>>> {
    let specializable = |class: &str| {
        kb.api_classes.values().any(|library| library == class)
            || decls
                .classes
                .values()
                .any(|c| c.bases.iter().any(|b| b == class))
    };
    let mut out: FxHashMap<&str, Vec<TypedReturns<'_>>> = FxHashMap::default();
    let mut ruled_out: Vec<&str> = Vec::new();
    for f in input
        .ir
        .functions
        .iter()
        .filter(|f| is_summarized_function(f))
    {
        let declared = decls
            .functions
            .get(&f.qualname)
            .and_then(|d| d.return_type.as_ref());
        let values = returned_values(decls, f);
        let returned = match (declared, &values) {
            (Some(t), Some(_)) if t.obj().is_some_and(|o| specializable(&o.class)) => {
                Some(Returned::Narrower(t.clone()))
            }
            (None, Some(values)) => returned_without_class(decls, f, values),
            (Some(t), Some(values)) if t.obj().is_some_and(|o| names_no_class(&o.class)) => {
                returned_without_class(decls, f, values)
            }
            _ => None,
        };
        match returned.zip(values) {
            Some((returned, values)) => out.entry(&f.qualname).or_default().push(TypedReturns {
                f,
                values,
                returned,
            }),
            None => ruled_out.push(&f.qualname),
        }
    }
    for q in ruled_out {
        out.remove(q);
    }
    out
}

/// Whether an annotation of `class` says nothing of the object a function returns that a named
/// object would not: `Any`, `object`, or any `Callable`.
fn names_no_class(class: &str) -> bool {
    super::of_any_class(class) || super::is_any_callable(class)
}

/// What the values `f` returns may say where its annotation names no class: the named objects
/// they may be ([`may_return_named`]), or the class of the instance the calls they are give
/// when each is a call or awaits one. A value made another way (a parameter, an attribute, a
/// display) is not inferred: an instance's attribute holds what the whole project stores
/// there, which the module's own declarations do not see.
fn returned_without_class(
    decls: &ModuleDecls,
    f: &IrFunction,
    values: &[VarId],
) -> Option<Returned> {
    let shape = FnShape::new(f);
    if may_return_named(decls, &shape, values) {
        return Some(Returned::Named);
    }
    let made_by = |v: VarId| shape.inst(shape.copy_root(v)).map(|i| &i.kind);
    let is_call = |kind: Option<&InstKind>| {
        matches!(
            kind,
            Some(InstKind::Call { .. } | InstKind::MethodCall { .. })
        )
    };
    values
        .iter()
        .all(|v| match made_by(*v) {
            Some(InstKind::Await { value, .. }) => is_call(made_by(*value)),
            kind => is_call(kind),
        })
        .then_some(Returned::Called)
}

/// Whether every value `f` returns may be a named object, by what makes it: a `def` or `lambda`
/// of `f`, a name its module binds to a definition, an import, or an alias, or a name a scope
/// enclosing it binds. Only such functions are inferred for a [`Returned::Named`] fact.
fn may_return_named(decls: &ModuleDecls, shape: &FnShape<'_>, values: &[VarId]) -> bool {
    let named = |name: &str| {
        decls.functions.contains_key(name)
            || decls.classes.contains_key(name)
            || decls.imports.contains_key(name)
            || decls.aliases.contains_key(name)
            || decls.alias_paths.contains_key(name)
    };
    values
        .iter()
        .all(|v| match shape.inst(shape.copy_root(*v)).map(|i| &i.kind) {
            Some(InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. }) => {
                named(name)
            }
            Some(
                InstKind::LoadDeref { .. }
                | InstKind::MakeClosure {
                    func_id: Some(_), ..
                },
            ) => true,
            _ => false,
        })
}

/// The value of every `return` of `f`, when calling it runs its body and gives what it returns
/// and each `return` gives a value.
fn returned_values(decls: &ModuleDecls, f: &IrFunction) -> Option<Vec<VarId>> {
    if !matches!(f.kind, FunctionKind::Sync | FunctionKind::Async)
        || is_generator_function(f, decls)
        || !keeps_body_result(f, decls)
    {
        return None;
    }
    let values: Vec<VarId> = f
        .blocks
        .iter()
        .filter_map(|bb| match bb.terminator {
            Terminator::Return(value) => Some(value),
            _ => None,
        })
        .collect::<Option<_>>()?;
    (!values.is_empty()).then_some(values)
}

/// The calls the items of the values one definition returns come from.
struct Returns {
    calls: Vec<ReturnedCall>,
}

/// A call the items of a returned value come from ([`ItemSource::Call`]), and how the value
/// reads them ([`heki_ir::ItemOrigin::text`], [`heki_ir::ItemOrigin::carries`]).
struct ReturnedCall {
    function: heki_ir::FunctionId,
    block: heki_ir::BlockId,
    index: u32,
    text: bool,
    dropped: bool,
    counted: bool,
}

/// The values every `return` of `f` gives with their item origins, when each returns a value
/// whose items all come from calls and calling `f` runs its body and gives what it returns.
fn returned_origins(
    origins: &ItemOrigins<'_>,
    decls: &ModuleDecls,
    f: &IrFunction,
) -> Option<Returns> {
    let budget = Budget::of_function("heki_effect::resolver::returns", f);
    let mut calls = Vec::new();
    for value in returned_values(decls, f)? {
        let Answer::Known(found) = origins.of_value(&budget, f.id, value) else {
            return None;
        };
        (!found.is_empty()).then_some(())?;
        for o in found {
            let ItemSource::Call { block, index } = o.source else {
                return None;
            };
            calls.push(ReturnedCall {
                function: o.function,
                block,
                index,
                text: o.text,
                dropped: o.dropped,
                counted: o.carries == heki_ir::Carries::Count,
            });
        }
    }
    (!calls.is_empty()).then_some(Returns { calls })
}

/// The return facts of one module being decided.
struct ReturnFacts<'c, 'f> {
    cx: TypeCtx<'c>,
    candidates: &'c FxHashMap<&'f str, Vec<Returns>>,
    /// The types of each function a returned call is in, and of each typed candidate's
    /// definition, inferred once for both facts.
    inferred: RefCell<FxHashMap<heki_ir::FunctionId, Rc<Inferred>>>,
    /// The values of [`Self::inferred`] the item origins read.
    types: RefCell<FxHashMap<heki_ir::FunctionId, Rc<FxHashMap<VarId, Val>>>>,
}

impl<'f> ReturnFacts<'_, 'f> {
    /// The facts of the candidates, by qualname: each pass decides every candidate from the
    /// facts of the pass before (a function returning what another of the module returns has
    /// its fact one pass after it), until no fact changes or [`MAX_RETURN_PASSES`] passes. A
    /// fact, once decided, stays: a pass only adds facts, so the facts do not depend on the
    /// order of the candidates. Past the bound, a function down a longer chain gets none.
    fn in_passes(&self) -> FxHashMap<&'f str, ReturnedItems> {
        let mut facts: FxHashMap<&'f str, ReturnedItems> = FxHashMap::default();
        for _ in 0..MAX_RETURN_PASSES {
            let next: FxHashMap<&'f str, ReturnedItems> = self
                .candidates
                .iter()
                .filter_map(|(q, definitions)| Some((*q, self.agreed(definitions, &facts)?)))
                .collect();
            if next == facts {
                break;
            }
            facts = next;
        }
        facts
    }

    /// The items all of `definitions` return, when they agree, by the facts `facts` of the
    /// module's functions.
    fn agreed(
        &self,
        definitions: &[Returns],
        facts: &FxHashMap<&str, ReturnedItems>,
    ) -> Option<ReturnedItems> {
        let module = &self.cx.module.module_name;
        let returned = |key: &FuncKey| {
            (key.module == *module)
                .then(|| facts.get(key.qualname.as_str()).cloned())
                .flatten()
        };
        let mut each = definitions
            .iter()
            .flat_map(|d| &d.calls)
            .map(|o| self.origin_items(o, &returned));
        let first = each.next().flatten()?;
        each.all(|items| items.as_ref() == Some(&first))
            .then_some(first)
    }

    /// The type every value of `definitions` returns, past what the annotations say: one class
    /// narrower than the annotation, or the instance of one class where the annotation names
    /// none, that they agree on, or the named objects they are, when each is one
    /// ([`Returned::Named`]: a function returning one of two functions returns one of them).
    fn returned_type(&self, definitions: &[TypedReturns<'_>]) -> Option<Ty> {
        let project = self.cx.project;
        let mut each = definitions.iter().flat_map(|d| {
            let inferred = self.inferred(d.f);
            let declared = match &d.returned {
                Returned::Narrower(t) => project.declared_ty(t),
                Returned::Named | Returned::Called => None,
            };
            d.values
                .iter()
                .map(|v| {
                    let t = inferred.ty(*v)?;
                    match &d.returned {
                        Returned::Narrower(_) => declared
                            .as_ref()
                            .is_some_and(|declared| project.narrower(t, declared, self.cx.kb))
                            .then(|| t.clone()),
                        Returned::Named => (!t.named().is_empty()).then(|| t.clone()),
                        Returned::Called => matches!(t, Ty::Inst(_)).then(|| t.clone()),
                    }
                })
                .collect::<Vec<_>>()
        });
        let first = each.next().flatten()?;
        if !first.named().is_empty() {
            let mut named: Vec<String> = first.named().to_vec();
            for t in each {
                let t = t?;
                // A definition giving an instance where another gives a named object.
                (!t.named().is_empty()).then_some(())?;
                named.extend(t.named().iter().cloned());
            }
            return Ty::one_of(named);
        }
        each.all(|t| t.as_ref() == Some(&first)).then_some(first)
    }

    /// The module globals `definitions` read, by the fqn each name reads ([`ReturnFact::ty_globals`]):
    /// what an import binds, or the module's own global; and the member of a module they load
    /// (`pkg.models.Other` of `models.Other()`), which another module may rebind.
    fn globals_read(&self, decls: &ModuleDecls, definitions: &[TypedReturns<'_>]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for d in definitions {
            let inferred = self.inferred(d.f);
            for i in d.f.blocks.iter().flat_map(|b| &b.insts) {
                match &i.kind {
                    InstKind::LoadGlobal { name, .. } => out.push(decls.global_read(name)),
                    InstKind::MethodCall {
                        recv: owner,
                        method: member,
                        ..
                    }
                    | InstKind::AttrLoad {
                        obj: owner,
                        attr: member,
                        ..
                    } => {
                        if let Some(Ty::Ref(module)) = inferred.ty(*owner) {
                            out.push(format!("{module}.{member}"));
                        }
                    }
                    _ => {}
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// The types of `f`, inferred once.
    fn inferred(&self, f: &IrFunction) -> Rc<Inferred> {
        if let Some(done) = self.inferred.borrow().get(&f.id) {
            return Rc::clone(done);
        }
        let inferred = self.cx.infer_types(f);
        self.inferred
            .borrow_mut()
            .insert(f.id, Rc::clone(&inferred));
        inferred
    }

    /// The items of the returned value whose items come from the call `call`.
    fn origin_items(
        &self,
        call: &ReturnedCall,
        returned: &dyn Fn(&FuncKey) -> Option<ReturnedItems>,
    ) -> Option<ReturnedItems> {
        let f = self.cx.ir().function(call.function);
        let inst = f
            .block(call.block)
            .insts
            .get(usize::try_from(call.index).ok()?)?;
        let types = Rc::clone(
            self.types
                .borrow_mut()
                .entry(f.id)
                .or_insert_with(|| Rc::new(self.inferred(f).vals())),
        );
        let shape = FnShape::new(f);
        let def = |v: VarId| shape.inst(v);
        // A count the code fixes is no data's.
        self.cx
            .fixed_call_items(&types, &def, inst)
            .is_none()
            .then_some(())?;
        let called = self.cx.call_items(&types, &def, inst, returned).ok()?;
        called
            .items
            .read_through(call.text, call.dropped, call.counted)
    }
}
