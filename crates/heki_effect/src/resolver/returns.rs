//! What calling a project function gives by what every `return` of it gives
//! ([`FunctionDecl::returns`]), and what the items of a call's result are
//! ([`TypeCtx::call_items`]): the one reading of a call's result that typing and the rules'
//! item origins share.
//!
//! The return fact is pass 1 ([`record_return_facts`]): it is inferred against the module's own
//! declarations, completed against themselves (the globals and attributes the module's own code
//! stores), so it stays a function of the module's text and the declarations do not depend on
//! the order in which modules were edited. A value another module gives is of no type there, and
//! a function returning one gets no fact.

use super::{
    Callee, FunctionDecl, ModuleDecls, ProjectDecls, TypeCtx, Val, complete_decls,
    is_generator_function, is_summarized_function, keeps_body_result,
};
use crate::infer::{Called, HandedArgs, MAX_HANDED_CALLS};
use crate::partial::Partial;
use crate::{FuncKey, ModuleInput};
use heki_ir::walk::{Answer, Budget, FnShape};
use heki_ir::{
    FunctionKind, Inst, InstKind, IrFunction, ItemOrigin, ItemOrigins, ItemSource, Terminator,
    VarId,
};
use heki_kb::{EffectiveEcosystemKb, LiteralValue, ResultItems};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;

/// What every `return` of a function gives, past what its annotation says
/// ([`FunctionDecl::returns`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReturnFact {
    /// The items of the returned value, when the data decides how many there are: the rows,
    /// lines, or entries of a catalog call, or what a project function or an offloaded call
    /// returns of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<ReturnedItems>,
}

/// Items whose number the data decides ([`ReturnFact::items`], [`CallItems`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReturnedItems {
    /// What the catalog call they come from gives ([`heki_kb::EcosystemEntry::items`]).
    pub kind: ResultItems,
    /// They are the pieces `str.splitlines` cuts the call's text into.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pieces: bool,
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

/// The most project functions one return fact follows down (`a` returning `b()` returning
/// `c()`): past them a function gets no fact.
const MAX_RETURN_DEPTH: u32 = 16;

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
        for _ in 0..=MAX_HANDED_CALLS {
            if let Called::Value(v) = called
                && let Some(held) = self.partial_held(types, def, v)
            {
                called = Called::Value(held);
            }
            let callee = match called {
                Called::Value(v) => match bound_method(def, v) {
                    Some((recv, method)) => self.resolve_method(types.get(&recv), method),
                    None => self.resolve_call(types.get(&v)),
                },
                Called::Method(recv, method) => self.resolve_method(types.get(&recv), method),
            };
            match callee {
                Callee::External(symbol) => {
                    let entry = self.catalog(&symbol).ok_or(NoItems::Unsized)?;
                    if let Some(of) = entry.result_of {
                        let (callable, handed) =
                            HandedArgs::of(of, passed.passed()).ok_or(NoItems::NotFollowed)?;
                        called = Called::Value(callable);
                        passed = handed;
                        continue;
                    }
                    let kind = entry.items.ok_or(NoItems::Unsized)?;
                    return Ok(CallItems {
                        items: ReturnedItems {
                            kind,
                            pieces: false,
                            counted: false,
                            api: entry.api.clone().unwrap_or(symbol),
                        },
                        through: None,
                    });
                }
                Callee::Project(keys, _) => {
                    let mut found: Option<ReturnedItems> = None;
                    for key in &keys {
                        let items = returned(key).ok_or(NoItems::NotFollowed)?;
                        if found.as_ref().is_some_and(|f| *f != items) {
                            return Err(NoItems::NotFollowed);
                        }
                        found = Some(items);
                    }
                    let items = found.ok_or(NoItems::NotFollowed)?;
                    return Ok(CallItems {
                        items,
                        through: (keys.len() == 1).then(|| keys[0].clone()),
                    });
                }
                Callee::TooMany | Callee::Unresolved => return Err(NoItems::NotFollowed),
            }
        }
        Err(NoItems::NotFollowed)
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

    /// The callable calling `v` calls when it holds a `functools.partial` object this function
    /// makes ([`Partial::of`]).
    fn partial_held<'i>(
        &self,
        types: &FxHashMap<VarId, Val>,
        def: &dyn Fn(VarId) -> Option<&'i Inst>,
        v: VarId,
    ) -> Option<VarId> {
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
        .map(|p| p.callable)
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
            } => (self.resolve_method(val(*recv), method), args, kwargs, star_from),
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
            let Some(next) = recv.and_then(|r| def(r)) else {
                break;
            };
            at = next;
        }
        fewest
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

/// [`FunctionDecl::returns`] of the module's functions (pass 1): what every `return` of every
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
    for f in input.ir.functions.iter().filter(|f| is_summarized_function(f)) {
        match returned_origins(&origins, decls, f) {
            Some(returns) => candidates.entry(&f.qualname).or_default().push(returns),
            None => ruled_out.push(&f.qualname),
        }
    }
    for q in ruled_out {
        candidates.remove(q);
    }
    if candidates.is_empty() {
        return;
    }
    let own = ProjectDecls::new([decls.clone()]);
    let completed = ProjectDecls::new([complete_decls(input, decls.clone(), &own, kb)]);
    let facts = ReturnFacts {
        cx: TypeCtx::new(input, &completed, kb),
        candidates: &candidates,
        decided: RefCell::default(),
    };
    let decided: Vec<(&str, ReturnedItems)> = candidates
        .keys()
        .filter_map(|q| Some((*q, facts.items_of(q, 0)?)))
        .collect();
    for (qualname, items) in decided {
        if let Some(d) = decls.functions.get_mut(qualname) {
            d.returns = Some(Box::new(ReturnFact { items: Some(items) }));
        }
    }
}

/// Where the items of each value one definition returns come from.
struct Returns {
    values: Vec<std::collections::BTreeSet<ItemOrigin>>,
}

/// The values every `return` of `f` gives with their item origins, when each returns a value
/// whose items all come from calls and calling `f` runs its body and gives what it returns.
fn returned_origins(
    origins: &ItemOrigins<'_>,
    decls: &ModuleDecls,
    f: &IrFunction,
) -> Option<Returns> {
    if !matches!(f.kind, FunctionKind::Sync | FunctionKind::Async)
        || is_generator_function(f)
        || !keeps_body_result(f, decls)
    {
        return None;
    }
    let budget = Budget::of_function("heki_effect::resolver::returns", f);
    let mut values = Vec::new();
    for bb in &f.blocks {
        let Terminator::Return(value) = bb.terminator else {
            continue;
        };
        let Answer::Known(found) = origins.of_value(&budget, f.id, value?) else {
            return None;
        };
        let from_calls = !found.is_empty()
            && found
                .iter()
                .all(|o| matches!(o.source, ItemSource::Call { .. }));
        if !from_calls {
            return None;
        }
        values.push(found);
    }
    (!values.is_empty()).then_some(Returns { values })
}

/// The return facts of one module being decided, each name once.
struct ReturnFacts<'c, 'f> {
    cx: TypeCtx<'c>,
    candidates: &'c FxHashMap<&'f str, Vec<Returns>>,
    /// `None` while a name is being decided: a call back into it (recursion) gives no fact.
    decided: RefCell<FxHashMap<String, Option<Option<ReturnedItems>>>>,
}

impl ReturnFacts<'_, '_> {
    /// The items every return of every definition of `qualname` gives, `depth` functions down.
    fn items_of(&self, qualname: &str, depth: u32) -> Option<ReturnedItems> {
        if let Some(decided) = self.decided.borrow().get(qualname) {
            return decided.clone().flatten();
        }
        let definitions = self.candidates.get(qualname)?;
        if depth > MAX_RETURN_DEPTH {
            return None;
        }
        self.decided.borrow_mut().insert(qualname.to_owned(), None);
        let items = self.agreed(definitions, depth);
        self.decided
            .borrow_mut()
            .insert(qualname.to_owned(), Some(items.clone()));
        items
    }

    /// The items all of `definitions` return, when they agree.
    fn agreed(&self, definitions: &[Returns], depth: u32) -> Option<ReturnedItems> {
        let module = &self.cx.module.module_name;
        let returned = |key: &FuncKey| {
            (key.module == *module)
                .then(|| self.items_of(&key.qualname, depth + 1))
                .flatten()
        };
        let mut found: Option<ReturnedItems> = None;
        for d in definitions {
            for origins in &d.values {
                for o in origins {
                    let items = self.origin_items(o, &returned)?;
                    if found.as_ref().is_some_and(|f| *f != items) {
                        return None;
                    }
                    found = Some(items);
                }
            }
        }
        found
    }

    /// The items of the returned value whose items come from the call at `origin`.
    fn origin_items(
        &self,
        origin: &ItemOrigin,
        returned: &dyn Fn(&FuncKey) -> Option<ReturnedItems>,
    ) -> Option<ReturnedItems> {
        let ItemSource::Call { block, index } = origin.source else {
            return None;
        };
        let f = self.cx.ir().function(origin.function);
        let inst = f.block(block).insts.get(usize::try_from(index).ok()?)?;
        let types = self.cx.infer(f);
        let shape = FnShape::new(f);
        let def = |v: VarId| shape.inst(v);
        if self.cx.fixed_call_items(&types, &def, inst).is_some() {
            return None;
        }
        let called = self.cx.call_items(&types, &def, inst, returned).ok()?.items;
        let pieces = match (called.kind, called.pieces, origin.text) {
            (ResultItems::Text, false, true) => true,
            (_, pieces, false) => pieces,
            _ => return None,
        };
        Some(ReturnedItems {
            pieces,
            counted: called.counted || origin.carries == heki_ir::Carries::Count,
            ..called
        })
    }
}

impl FunctionDecl {
    /// The items calling it gives by its return fact ([`ReturnFact::items`]).
    pub fn returned_items(&self) -> Option<&ReturnedItems> {
        self.returns.as_ref()?.items.as_ref()
    }
}
