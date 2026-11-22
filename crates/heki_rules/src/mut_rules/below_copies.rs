//! `MUT115` (`shallow-copy-nested-mutation`): an in-place change of an object below a shallow
//! copy the function made of state that outlives the call. The copy holds the same object as
//! the copied value, so the change is seen through the original too.
//!
//! The copies and what they share are `heki_effect::readonly`'s, the table `MUT202` reads
//! ([`readonly::call_derivation`], [`readonly::display_copies`], [`readonly::subscript_step`]);
//! [`Mut202Fn`] holds the function's values and calls. A report is exact when every premise of
//! the rule page holds ([`Mut202Fn::exact_below_copy`]), heuristic otherwise.

use super::{CallParts, MUTABLE_BUILTINS, Mut202Fn, attr_writes, site_call};
use crate::CallHop;
use heki_diagnostics::Diagnostic;
use heki_effect::readonly::{self, ContainerKind, Derivation, ReturnStep};
use heki_effect::{Callee, Val};
use heki_ir::walk::{
    Answer, Budget, Closure, Def, Facts, Join, KeySet, Sparse, Use, Visit, fixpoint, reach,
};
use heki_ir::{BlockId, DictEntry, Inst, InstKind, Terminator, VarId};
use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::{OnceCell, RefCell};
use std::collections::BTreeSet;
use std::rc::Rc;

/// The builtin functions that only read the values they are handed (and build new ones from
/// them): a copy handed to one keeps the objects it holds.
const READING_BUILTINS: &[&str] = &[
    "builtins.len",
    "builtins.isinstance",
    "builtins.bool",
    "builtins.str",
    "builtins.repr",
    "builtins.print",
    "builtins.sorted",
    "builtins.list",
    "builtins.tuple",
    "builtins.set",
    "builtins.frozenset",
    "builtins.dict",
    "builtins.iter",
    "builtins.enumerate",
    "builtins.zip",
    "builtins.reversed",
    "builtins.any",
    "builtins.all",
    "builtins.sum",
    "builtins.min",
    "builtins.max",
    "builtins.id",
    "builtins.type",
    "builtins.hash",
    "builtins.format",
];

/// The methods of a builtin container that only read it.
const READING_METHODS: &[&str] = &[
    "get",
    "keys",
    "values",
    "items",
    "copy",
    "count",
    "index",
    "__len__",
    "__contains__",
];

/// How a copy is made, as far as its exact claim cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyKind {
    /// A builtin container's copy (`x.copy()`, `dict(x)`, `{**x}`, `[*x]`, `x[:]`): the new
    /// container is the builtin `made`, and it reads the copied container through `reads`
    /// (looked up `by_name` for `x.copy()`, called implicitly otherwise).
    Container {
        made: Option<ContainerKind>,
        reads: &'static [&'static str],
        by_name: bool,
    },
    /// `copy.copy(x)` of an instance.
    CopyCopy,
    /// `model_copy()` of a pydantic model ([`heki_kb`] entry `pydantic.BaseModel.model_copy`).
    ModelCopy,
    /// Any other shallow copy (a catalog copy sanitizer, `copy.replace`,
    /// `dataclasses.replace`, a copy method of a library class): heuristic only.
    Other,
}

/// A shallow copy the function makes of the value `copied`.
struct CopySite<'a> {
    inst: &'a Inst,
    dst: VarId,
    copied: VarId,
    kind: CopyKind,
    /// How the message names it (`` `dict(...)` ``, `` `.copy()` ``).
    text: &'static str,
    /// The attributes the copy is given values of its own when it is made (`replace(x, f=v)`,
    /// `model_copy(update={"f": v})`); `None` when heki cannot read their names (`**kw`, an
    /// `update` mapping that is no dict display with constant keys).
    given: Option<BTreeSet<String>>,
    /// Every keyword of the call is one heki reads (a `deep=` it cannot evaluate may copy
    /// deeply).
    keywords_known: bool,
}

/// One level of a copy: what the first load from the copy reads.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Level {
    /// An item under the constant key (`None`: a key heki cannot read, an index).
    Key(Option<String>),
    Attr(String),
}

/// A step of [`Mut202Fn::copies_below`]: the object a value holds is below a copy, or the value
/// is a copy (or below one), whose parts are then below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Step {
    Below,
    Holder,
}

/// What a value's object is to the copies of the function on every path to its use
/// ([`Mut202Fn::held`]): one of the copies, or an object below one, read from it by the load
/// `first` (the level the copy holds it at) and then by plain loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    Copy(usize),
    Below { site: usize, first: VarId },
}

/// A class decision `MUT115` asks ([`Mut202Fn::class_answer`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ClassQuestion {
    /// `ProjectDecls::method_not_overridden` (looked up by name or not).
    NotOverridden(bool),
    /// No copy hook (`copy.copy`).
    CopyHooks,
    /// No `__copy__` or `model_copy` (pydantic's `model_copy`).
    ModelCopyHooks,
    /// `ProjectDecls::attr_read_finds_stored`.
    Read,
    /// `ProjectDecls::attr_store_on_instance`.
    Store,
}

/// The class decisions asked so far in one module's check, each decided once: the declarations
/// it reads are recorded with the first ask, which the check of the module makes.
#[derive(Default)]
pub(super) struct ClassAnswers {
    answers: RefCell<FxHashMap<(ClassQuestion, String, String), bool>>,
}

/// The facts of `MUT115` for one function, built on the first need.
pub(super) struct BelowCopies<'a> {
    sites: Vec<CopySite<'a>>,
    by_dst: FxHashMap<VarId, usize>,
    /// `site -> (any path, every path)`: the copied value may be, and is, state that outlives
    /// the call.
    borrowed: Vec<(bool, bool)>,
    may: Closure<(VarId, Step), KeySet<usize>>,
    /// What each value holds of the copies on every path ([`Mut202Fn::held_facts`]).
    held: OnceCell<FxHashMap<VarId, MustHeld>>,
    /// The uses of each copy root ([`Mut202Fn::copy_uses`]) and the stores into it
    /// ([`Mut202Fn::stores_into`]), built on the first need.
    uses: RefCell<FxHashMap<VarId, Rc<CopyUses>>>,
    stores: RefCell<FxHashMap<VarId, Rc<Stores>>>,
    budget: Budget,
}

/// The uses of a copy root and of the copies of the reference to it, each with the value it
/// uses.
type CopyUses = Vec<(VarId, Use)>;

/// The stores into one object, by level and block: their indexes in the block.
type Stores = FxHashMap<(Level, BlockId), Vec<u32>>;

/// [`Held`] as the fact of a must-question: established, or contradicted on some path.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MustHeld {
    Known(Held),
    Conflict,
}

impl Join for MustHeld {
    fn join(&mut self, other: &Self) -> bool {
        if self == other || *self == Self::Conflict {
            return false;
        }
        *self = Self::Conflict;
        true
    }
}

impl<'a> Mut202Fn<'a> {
    /// The answer to `question` about `class` (and the attribute or method `name`), decided once
    /// per module's check.
    fn class_answer(
        &self,
        question: ClassQuestion,
        class: &str,
        name: &str,
        decide: impl FnOnce() -> bool,
    ) -> bool {
        let key = (question, class.to_owned(), name.to_owned());
        if let Some(&known) = self.classes.answers.borrow().get(&key) {
            return known;
        }
        let answer = decide();
        self.classes.answers.borrow_mut().insert(key, answer);
        answer
    }

    /// [`heki_effect::ProjectDecls::method_not_overridden`], once per module's check.
    fn not_overridden(&self, class: &str, method: &str, by_name: bool) -> bool {
        self.class_answer(ClassQuestion::NotOverridden(by_name), class, method, || {
            self.cx.decls.method_not_overridden(class, method, by_name)
        })
    }

    /// `MUT115`'s reports of the function.
    pub(super) fn below_copies(&self, out: &mut Vec<Diagnostic>) {
        if !self.cx.enabled("MUT115", self.unit) {
            return;
        }
        let facts = self.copy_facts();
        if facts.sites.is_empty() {
            return;
        }
        let memo_fills = self.below_memo_fills(&facts);
        let mut reported: FxHashSet<heki_ast::Span> = FxHashSet::default();
        for bb in &self.f.blocks {
            for (index, inst) in bb.insts.iter().enumerate() {
                let at = (bb.id, u32::try_from(index).unwrap_or(u32::MAX));
                if memo_fills.contains(&at) || reported.contains(&inst.span) {
                    continue;
                }
                if let Some((site, chain)) = self.below_change(&facts, inst) {
                    reported.insert(inst.span);
                    self.report_below(out, &facts, inst, site, chain);
                }
            }
        }
    }

    /// The copy facts of the function: its shallow copies of values that may outlive the call.
    fn copy_facts(&self) -> BelowCopies<'a> {
        let mut sites = Vec::new();
        for inst in self.f.blocks.iter().flat_map(|b| &b.insts) {
            if let Some(site) = self.copy_site(inst) {
                sites.push(site);
            }
        }
        let mut facts = BelowCopies {
            by_dst: sites.iter().enumerate().map(|(i, s)| (s.dst, i)).collect(),
            sites,
            borrowed: Vec::new(),
            may: Closure::new(),
            held: OnceCell::new(),
            uses: RefCell::default(),
            stores: RefCell::default(),
            budget: Budget::of_function("MUT115 copies", self.f),
        };
        facts.borrowed = (facts.sites.iter())
            .map(|s| self.borrowed(s.copied, &facts.budget))
            .collect();
        facts
    }

    /// The shallow copy `inst` makes, if any ([`readonly`]'s table, as `MUT202` follows it).
    fn copy_site(&self, inst: &'a Inst) -> Option<CopySite<'a>> {
        let dst = inst.kind.dst()?;
        let site = |copied, kind, text, given| CopySite {
            inst,
            dst,
            copied,
            kind,
            text,
            given,
            keywords_known: true,
        };
        match &inst.kind {
            InstKind::SubscriptLoad { obj, .. } => {
                let class = self.class_of(*obj);
                let step = readonly::subscript_step(&self.ix.shape, inst, class);
                (step == ReturnStep::SHALLOW_COPY).then(|| {
                    let kind = CopyKind::Container {
                        made: Some(ContainerKind::List),
                        reads: &["__getitem__"],
                        by_name: false,
                    };
                    site(*obj, kind, "a slice", Some(BTreeSet::new()))
                })
            }
            InstKind::DictLiteral { .. } | InstKind::AllocBuiltin { .. } => {
                let [copied] = readonly::display_copies(self.f, &inst.kind) else {
                    return None;
                };
                let (made, text) = match inst.kind {
                    InstKind::DictLiteral { .. } => (Some(ContainerKind::Dict), "`{**...}`"),
                    _ => (Some(ContainerKind::List), "`[*...]`"),
                };
                let reads: &[&str] = &["__iter__", "keys", "__getitem__"];
                let kind = CopyKind::Container {
                    made: made.filter(|_| {
                        matches!(inst.kind, InstKind::DictLiteral { .. })
                            || is_list_display(&inst.kind)
                    }),
                    reads,
                    by_name: false,
                };
                Some(site(*copied, kind, text, Some(BTreeSet::new())))
            }
            _ => self.call_copy(inst, dst),
        }
    }

    /// The shallow copy the call `inst` makes of its receiver or of its first argument.
    fn call_copy(&self, inst: &'a Inst, dst: VarId) -> Option<CopySite<'a>> {
        let call = self.call_of(inst)?;
        let deep = readonly::deep_copy_requested(call.kwargs, |v| {
            self.ix.shape.bool_const(v) == Some(true)
        });
        let own_items = call.args.len() > 1 || !call.kwargs.is_empty();
        let shallow = |method: Option<&str>| {
            readonly::call_derivation(method, &call.callee, self.cx.kb, deep, own_items)
                == Some(Derivation::Step(ReturnStep::SHALLOW_COPY))
        };
        let symbol = match &call.callee {
            Callee::External(s) => s.as_str(),
            _ => "",
        };
        // A function of a module (`copy.copy(x)`) is a method call on the module.
        let on_module = |recv: VarId| matches!(self.types.get(&recv), Some(Val::Ref(_)));
        let (copied, kind, text) = match call.recv {
            Some((recv, method)) if !on_module(recv) && shallow(Some(method)) => {
                let kind = if method == "model_copy" {
                    if symbol == "pydantic.BaseModel.model_copy" {
                        CopyKind::ModelCopy
                    } else {
                        CopyKind::Other
                    }
                } else if method == "copy" && call.args.is_empty() && call.kwargs.is_empty() {
                    CopyKind::Container {
                        made: None,
                        reads: &["copy"],
                        by_name: true,
                    }
                } else {
                    CopyKind::Other
                };
                let text = if method == "model_copy" {
                    "`model_copy()`"
                } else {
                    "`.copy()`"
                };
                (recv, kind, text)
            }
            _ if shallow(None) => {
                let copied = *call.args.first()?;
                let alone = call.args.len() == 1 && call.kwargs.is_empty();
                let container = |made, text| {
                    let kind = CopyKind::Container {
                        made: Some(made),
                        reads: &["__iter__", "keys", "__getitem__"],
                        by_name: false,
                    };
                    (kind, text)
                };
                let (kind, text) = match symbol {
                    "builtins.dict" if alone => container(ContainerKind::Dict, "`dict(...)`"),
                    "builtins.list" if alone => container(ContainerKind::List, "`list(...)`"),
                    "builtins.set" if alone => container(ContainerKind::Set, "`set(...)`"),
                    "copy.copy" if alone => (CopyKind::CopyCopy, "`copy.copy`"),
                    "copy.replace" => (CopyKind::Other, "`copy.replace`"),
                    "dataclasses.replace" => (CopyKind::Other, "`dataclasses.replace`"),
                    _ => (CopyKind::Other, "a shallow copy"),
                };
                (copied, kind, text)
            }
            _ => return None,
        };
        let given = self.given_attrs(&call, kind);
        let keywords_known = call.kwargs.iter().all(|(name, value)| {
            name.as_deref() != Some("deep") || self.ix.shape.bool_const(*value) == Some(false)
        });
        Some(CopySite {
            inst,
            dst,
            copied,
            kind,
            text,
            given,
            keywords_known,
        })
    }

    /// [`CopySite::given`] of a copy call: the keywords of `replace(x, f=v)`, the constant keys
    /// of `model_copy(update={...})`.
    fn given_attrs(&self, call: &CallParts<'_>, kind: CopyKind) -> Option<BTreeSet<String>> {
        let mut given = BTreeSet::new();
        for (name, value) in call.kwargs {
            match (kind, name.as_deref()) {
                (_, None) => return None,
                (CopyKind::ModelCopy, Some("deep")) => {}
                (CopyKind::ModelCopy, Some("update")) => {
                    let Some(InstKind::DictLiteral { entries, .. }) =
                        self.ix.shape.inst(*value).map(|i| &i.kind)
                    else {
                        return None;
                    };
                    for entry in entries {
                        let DictEntry::KeyValue { key, .. } = entry else {
                            return None;
                        };
                        given.insert(self.ix.shape.str_const(*key)?.to_owned());
                    }
                }
                (_, Some(name)) => {
                    given.insert(name.to_owned());
                }
            }
        }
        Some(given)
    }

    /// The class of a value's inferred type, when it is one class.
    fn class_of(&self, v: VarId) -> Option<&str> {
        match self.types.get(&v) {
            Some(Val::Inst(class)) => Some(class.as_str()),
            _ => None,
        }
    }

    /// Whether the value `v` may be, and is on every path, state that outlives the call: a
    /// parameter, a module global, a variable of an enclosing function, or what a plain
    /// attribute or item load reads from such state. A walk back over the definitions; past the
    /// budget, only "may be" stands.
    fn borrowed(&self, v: VarId, budget: &Budget) -> (bool, bool) {
        let shape = &self.ix.shape;
        let mut any = false;
        let mut every = true;
        let next = |w: VarId, out: &mut Vec<VarId>| match shape.def(w) {
            Def::Phi(b, i) => out.extend(
                (self.f.block(b).phis[i as usize].incoming.iter())
                    .filter(|(_, x)| *x != w)
                    .map(|(_, x)| *x),
            ),
            Def::Inst(..) => out.extend(shape.inst(w).and_then(|i| self.borrowed_from(&i.kind))),
            Def::Param | Def::None => {}
        };
        let visit = |w: VarId| {
            let (outlives, plain) = match shape.def(w) {
                Def::Param => (true, true),
                Def::Phi(..) => (false, true),
                Def::None => (false, false),
                Def::Inst(..) => match shape.inst(w) {
                    Some(inst) => self.outlives_by(inst),
                    None => (false, false),
                },
            };
            any |= outlives;
            every &= plain;
            heki_ir::walk::Visit::<()>::Expand
        };
        let complete = !heki_ir::walk::reach(budget, [v], next, visit).is_open();
        (any, any && every && complete)
    }

    /// The value whose part the instruction `kind` reads, for [`Self::borrowed`]: the object of
    /// an attribute or item load or of `get`, or the operand of a copy of the reference.
    fn borrowed_from(&self, kind: &InstKind) -> Option<VarId> {
        match kind {
            InstKind::Copy { src, .. } => Some(*src),
            InstKind::AttrLoad { obj, .. }
            | InstKind::SubscriptLoad {
                obj,
                is_slice: false,
                ..
            } => Some(*obj),
            InstKind::MethodCall { recv, method, .. } if method == "get" => Some(*recv),
            _ => None,
        }
    }

    /// What the instruction `inst` makes of the value it defines, for [`Self::borrowed`]:
    /// whether it is state that outlives the call itself (a global, a variable of an enclosing
    /// function), and whether its object is established to be what the value it reads holds (a
    /// copy of the reference, a plain load) or such state. What a call returns is neither: a
    /// summary states what a function returns only when `MUT112` runs.
    fn outlives_by(&self, inst: &Inst) -> (bool, bool) {
        match &inst.kind {
            InstKind::LoadGlobal { .. } | InstKind::LoadDeref { .. } => (true, true),
            InstKind::Copy { .. } => (false, true),
            InstKind::AttrLoad { obj, attr, .. } => (false, self.plain_read(*obj, attr)),
            InstKind::SubscriptLoad {
                obj,
                is_slice: false,
                ..
            } => (false, self.plain_item(*obj)),
            InstKind::MethodCall {
                recv, method, args, ..
            } if method == "get" => (false, args.len() == 1 && self.plain_get(*recv)),
            _ => (false, false),
        }
    }

    /// Whether reading `attr` on the object `obj` holds finds what the instance stores there
    /// (an instance of a project class, or of a pydantic model, by its inferred class: R2).
    fn plain_read(&self, obj: VarId, attr: &str) -> bool {
        attr != "__dict__"
            && self.class_of(obj).is_some_and(|class| {
                self.class_answer(ClassQuestion::Read, class, attr, || {
                    self.cx
                        .decls
                        .attr_read_finds_stored(class, attr, |base| self.catalog_stores(base))
                })
            })
    }

    /// Whether an item load from `obj` reads an item the container holds: a builtin list or
    /// dict whose `__getitem__` no project subclass replaces (R2).
    fn plain_item(&self, obj: VarId) -> bool {
        self.class_of(obj).is_some_and(|class| {
            matches!(class, "builtins.list" | "builtins.dict")
                && self.not_overridden(class, "__getitem__", false)
        })
    }

    /// Whether `obj.get(k)` reads an item a builtin dict holds.
    fn plain_get(&self, obj: VarId) -> bool {
        self.class_of(obj) == Some("builtins.dict")
            && self.not_overridden("builtins.dict", "get", true)
    }

    /// Whether the catalog says the class `base` outside the project stores an attribute on
    /// the instance itself (`stores_on_instance` on its `__setattr__`, R3).
    fn catalog_stores(&self, base: &str) -> bool {
        self.cx
            .kb
            .lookup(&format!("{base}.__setattr__"))
            .is_some_and(|e| e.stores_on_instance)
    }

    /// The copies whose parts the object `v` holds may be (`Step::Below`), or that `v` may be
    /// or be below (`Step::Holder`): through copies, phis, in-place operators, plain or other
    /// loads, element accessors, the steps of iterating a part (not a copy: iterating a copy
    /// is how a container is changed while looping over it), and copies of copies. A load of
    /// a level the copy was given a value of its own, or that a store into the copy
    /// dominating the load rebinds, reads no part of it. Empty past the budget.
    fn copies_below(&self, facts: &BelowCopies<'a>, v: VarId, step: Step) -> KeySet<usize> {
        let shape = &self.ix.shape;
        let own = |(w, step): (VarId, Step)| -> KeySet<usize> {
            match (step, facts.by_dst.get(&w)) {
                (Step::Holder, Some(&site)) if facts.borrowed[site].0 => {
                    std::iter::once(site).collect()
                }
                _ => KeySet::new(),
            }
        };
        let next = |(w, step): (VarId, Step), out: &mut Vec<(VarId, Step)>| match shape.def(w) {
            Def::Phi(b, i) => out.extend(
                (self.f.block(b).phis[i as usize].incoming.iter())
                    .filter(|(_, x)| *x != w)
                    .map(|(_, x)| (*x, step)),
            ),
            Def::Inst(b, i) => {
                let inst = &self.f.block(b).insts[i as usize];
                if step == Step::Holder {
                    out.push((w, Step::Below));
                    // A copy of a copy, or of a part of one, holds those parts too.
                    if let Some(&site) = facts.by_dst.get(&w) {
                        out.push((facts.sites[site].copied, Step::Holder));
                    }
                }
                match &inst.kind {
                    InstKind::Copy { src, .. } => out.push((*src, step)),
                    InstKind::BinOp {
                        lhs,
                        in_place: true,
                        ..
                    } => out.push((*lhs, step)),
                    _ if step == Step::Holder => {}
                    InstKind::IterNext { iter, .. } => {
                        if let Some(InstKind::MakeIter { iterable, .. }) =
                            shape.inst(*iter).map(|i| &i.kind)
                            && !self.is_copy(facts, *iterable)
                        {
                            out.push((*iterable, Step::Holder));
                        }
                    }
                    kind => {
                        if let Some((container, level)) = self.load_of(kind)
                            && !self.rebound_before(facts, container, &level, (b, i))
                        {
                            out.push((container, Step::Holder));
                        }
                    }
                }
            }
            Def::Param | Def::None => {}
        };
        match facts.may.ask(&facts.budget, (v, step), own, next) {
            Answer::Known(sites) => (*sites).clone(),
            Answer::Open => KeySet::new(),
        }
    }

    /// Whether the value `v` is one of the copies (through copies and phis of it alone).
    fn is_copy(&self, facts: &BelowCopies<'a>, v: VarId) -> bool {
        facts.by_dst.contains_key(&self.ix.shape.copy_root(v))
    }

    /// The container a load reads from, and the level it reads: an item (`c["k"]`, `c[i]`),
    /// an attribute (not `__dict__`, the instance's own storage), or an element accessor
    /// (`c.get("k")`, `c.pop("k")`, `c.setdefault("k", d)`).
    fn load_of(&self, kind: &InstKind) -> Option<(VarId, Level)> {
        let shape = &self.ix.shape;
        match kind {
            InstKind::SubscriptLoad {
                obj,
                key_const,
                is_slice: false,
                ..
            } => Some((*obj, Level::Key(key_const.clone()))),
            InstKind::AttrLoad { obj, attr, .. } if attr != "__dict__" => {
                Some((*obj, Level::Attr(attr.clone())))
            }
            InstKind::MethodCall {
                recv, method, args, ..
            } if matches!(
                method.as_str(),
                "get" | "pop" | "setdefault" | "__getitem__"
            ) =>
            {
                let key = args
                    .first()
                    .and_then(|a| shape.str_const(*a))
                    .map(str::to_owned);
                Some((*recv, Level::Key(key)))
            }
            _ => None,
        }
    }

    /// Whether a store into the copy `container` holds (or a copy of the reference to it)
    /// rebinds `level` on every path to the load at `at`: a store of that very level dominates
    /// the load. A load of a level the copy call names (`replace(x, f=v)`,
    /// `model_copy(update={"f": v})`) reads none of the copied value's parts either.
    fn rebound_before(
        &self,
        facts: &BelowCopies<'a>,
        container: VarId,
        level: &Level,
        at: (BlockId, u32),
    ) -> bool {
        let root = self.ix.shape.copy_root(container);
        if let (Some(&site), Level::Attr(attr)) = (facts.by_dst.get(&root), level)
            && facts.sites[site]
                .given
                .as_ref()
                .is_some_and(|given| given.contains(attr))
        {
            return true;
        }
        // A store under a key heki cannot read may store another one.
        if *level == Level::Key(None) {
            return false;
        }
        let stores = self.stores_into(facts, root);
        let dominators = self.ix.shape.dominators();
        let same = stores.keys().filter(|(l, _)| l == level);
        // One step per store a load is checked against.
        let checked = same.clone().count();
        facts.budget.take(checked)
            && same.into_iter().any(|key @ (_, b)| {
                if *b == at.0 {
                    stores[key].iter().any(|i| *i < at.1)
                } else {
                    dominators.dominates(*b, at.0)
                }
            })
    }

    /// The stores into the object `root` holds (through copies of the reference to it), by
    /// level and block, with their indexes in the block: item stores and deletes, attribute
    /// stores and deletes. The store of an augmented assignment (`c["k"] += [x]`) puts back the
    /// object an in-place operator returned of what the level held: a builtin container's
    /// operator returns the container itself, so it rebinds nothing.
    fn stores_into(&self, facts: &BelowCopies<'a>, root: VarId) -> Rc<Stores> {
        if let Some(stores) = facts.stores.borrow().get(&root) {
            return Rc::clone(stores);
        }
        let mut stores = Stores::default();
        for (_, u) in self.copy_uses(facts, root).iter() {
            let Use::Inst(b, i) = *u else { continue };
            let kind = &self.f.block(b).insts[i as usize].kind;
            let level = match kind {
                InstKind::SubscriptStore { value, .. } | InstKind::AttrStore { value, .. }
                    if self.stores_back(root, kind, *value) =>
                {
                    continue;
                }
                InstKind::SubscriptStore { key_const, .. }
                | InstKind::SubscriptDelete { key_const, .. } => Level::Key(key_const.clone()),
                InstKind::AttrStore { attr, .. } | InstKind::AttrDelete { attr, .. } => {
                    Level::Attr(attr.clone())
                }
                _ => continue,
            };
            if store_target(kind).is_some_and(|obj| self.ix.shape.copy_root(obj) == root) {
                stores.entry((level, b)).or_default().push(i);
            }
        }
        let stores = Rc::new(stores);
        facts.stores.borrow_mut().insert(root, Rc::clone(&stores));
        stores
    }

    /// Whether the store `kind` into `root` puts back `value`, the result of an in-place
    /// operator on a builtin container loaded from the same level of `root`.
    fn stores_back(&self, root: VarId, kind: &InstKind, value: VarId) -> bool {
        let shape = &self.ix.shape;
        let Some(InstKind::BinOp {
            lhs,
            in_place: true,
            ..
        }) = shape.inst(value).map(|i| &i.kind)
        else {
            return false;
        };
        let stored = match kind {
            InstKind::SubscriptStore { obj, key_const, .. } => {
                (*obj, Level::Key(key_const.clone()))
            }
            InstKind::AttrStore { obj, attr, .. } => (*obj, Level::Attr(attr.clone())),
            _ => return false,
        };
        let loaded = shape.inst(*lhs).and_then(|i| self.load_of(&i.kind));
        let builtin = self
            .class_of(*lhs)
            .is_some_and(|t| MUTABLE_BUILTINS.contains(&t));
        builtin
            && stored.1 != Level::Key(None)
            && shape.copy_root(stored.0) == root
            && loaded.is_some_and(|(obj, level)| shape.copy_root(obj) == root && level == stored.1)
    }

    /// The uses of `root` and of the copies of the reference to it, each with the value it
    /// uses.
    fn copy_uses(&self, facts: &BelowCopies<'a>, root: VarId) -> Rc<CopyUses> {
        if let Some(uses) = facts.uses.borrow().get(&root) {
            return Rc::clone(uses);
        }
        let shape = &self.ix.shape;
        let mut uses = Vec::new();
        let next = |v: VarId, out: &mut Vec<VarId>| {
            for u in shape.readers(v) {
                if let Use::Inst(b, i) = *u
                    && let InstKind::Copy { dst, .. } = &self.f.block(b).insts[i as usize].kind
                {
                    out.push(*dst);
                }
            }
        };
        let visit = |v: VarId| {
            uses.extend(shape.readers(v).iter().map(|u| (v, *u)));
            Visit::<()>::Expand
        };
        // Past the budget the uses are incomplete: an unknown use keeps the claim open.
        if reach(&facts.budget, [root], next, visit).is_open() {
            uses.push((root, Use::Phi(BlockId(0), u32::MAX)));
        }
        let uses = Rc::new(uses);
        facts.uses.borrow_mut().insert(root, Rc::clone(&uses));
        uses
    }

    /// The change `inst` makes below one of the copies, if any: the copy site, and the call to
    /// the helper making it (empty: `inst` makes it itself).
    fn below_change(
        &self,
        facts: &BelowCopies<'a>,
        inst: &'a Inst,
    ) -> Option<(usize, Vec<CallHop>)> {
        let changed = match &inst.kind {
            InstKind::AttrStore { obj, .. }
            | InstKind::AttrDelete { obj, .. }
            | InstKind::SubscriptStore { obj, .. }
            | InstKind::SubscriptDelete { obj, .. } => Some(*obj),
            InstKind::MethodCall { recv, method, .. } if heki_effect::container_mutator(method) => {
                Some(*recv)
            }
            InstKind::BinOp {
                lhs,
                in_place: true,
                ..
            } => Some(*lhs).filter(|lhs| {
                self.class_of(*lhs)
                    .is_some_and(|t| MUTABLE_BUILTINS.contains(&t))
            }),
            _ => None,
        };
        if let Some(v) = changed {
            let sites = self.copies_below(facts, v, Step::Below);
            return sites.iter().next().map(|s| (*s, Vec::new()));
        }
        // Handing a part of a copy to a helper that changes that parameter, or the copy
        // itself to one that changes what its parameter holds.
        let shape = &self.ix.shape;
        let call = site_call(&|v| shape.inst(v), self.function_sites(), inst, false)?;
        for (key, callee, pos, var) in call.bound_arguments(self.cx) {
            let p = u8::try_from(pos).ok().filter(|p| *p < 64)?;
            let facts_run = self.facts_run_by_call(inst.span, key, callee);
            let below = self.copies_below(facts, var, Step::Below);
            let site = if facts_run.iter().any(|f| f.changes_param_state(p, None)) {
                below.iter().next().copied()
            } else {
                None
            };
            let site = site.or_else(|| {
                let changes_below = facts_run.iter().any(|f| {
                    f.changed_param_attrs
                        .get(&p)
                        .is_some_and(|attrs| f.changes_param_state(p, Some(attrs)))
                });
                let copy = facts.by_dst.get(&shape.copy_root(var)).copied();
                copy.filter(|s| changes_below && facts.borrowed[*s].0)
            });
            if let Some(site) = site {
                let hop = CallHop {
                    caller: self.key.clone(),
                    callee: key.fqn(),
                    span: inst.span,
                    path: self.unit.input.path.clone(),
                };
                return Some((site, vec![hop]));
            }
        }
        None
    }

    /// The writes of attributes of objects below the copies that fill a memo or initialize it
    /// lazily, which leave what the object computes as it was (as for `MUT202`).
    fn below_memo_fills(&self, facts: &BelowCopies<'a>) -> FxHashSet<(BlockId, u32)> {
        let objects: FxHashMap<VarId, u32> = (self.f.blocks.iter())
            .flat_map(|b| &b.insts)
            .filter_map(|inst| inst.kind.dst())
            .filter(|v| !self.copies_below(facts, *v, Step::Below).is_empty())
            .map(|v| (v, self.ix.shape.copy_root(v).0))
            .collect();
        if objects.is_empty() {
            return FxHashSet::default();
        }
        attr_writes(&self.ix.shape, &objects)
            .into_iter()
            .filter(|(attr, w)| w.fills_memo(attr) || w.lazy_init)
            .map(|(_, w)| (w.block, w.index))
            .collect()
    }

    fn report_below(
        &self,
        out: &mut Vec<Diagnostic>,
        facts: &BelowCopies<'a>,
        inst: &'a Inst,
        site: usize,
        chain: Vec<CallHop>,
    ) {
        let copy = &facts.sites[site];
        let exact = chain.is_empty() && self.exact_below_copy(facts, inst, site);
        let line = self.unit.lines.line_col(copy.inst.span.start).0;
        let original = self.value_text(copy.copied);
        let shares = if exact { "shares" } else { "may share" };
        let message = format!(
            "`{}` changes in place an object that its shallow copy of {original} ({}, line {line}) {shares} with {original}",
            self.key.qualname, copy.text,
        );
        let mut related = vec![CallHop {
            caller: self.key.clone(),
            callee: copy.text.trim_matches('`').to_owned(),
            span: copy.inst.span,
            path: self.unit.input.path.clone(),
        }];
        related.extend(chain);
        let target = self
            .f
            .var_name(copy.dst)
            .map_or_else(|| original.trim_matches('`').to_owned(), str::to_owned);
        let Some(d) = self.cx.emit(
            out,
            "MUT115",
            self.unit,
            inst.span,
            &self.key.qualname,
            target,
            message,
            Some("copy the nested object too before changing it (`copy.deepcopy(...)`, or a copy of that part)".to_owned()),
            &related,
        ) else {
            return;
        };
        let tier = if exact {
            heki_diagnostics::Certainty::Exact
        } else {
            heki_diagnostics::Certainty::Heuristic
        };
        d.certainty = tier.at_most(d.certainty);
    }

    /// Whether the change `inst` makes below the copy `site` states `MUT115`'s exact claim:
    /// every premise of the rule page holds.
    fn exact_below_copy(&self, facts: &BelowCopies<'a>, inst: &Inst, site: usize) -> bool {
        let Some(changed) = changed_value(&inst.kind) else {
            return false;
        };
        let Some(Held::Below { site: held, first }) = self.held(facts, changed) else {
            return false;
        };
        held == site
            && facts.borrowed[site].1
            && self.copies_shallowly(&facts.sites[site])
            && self.still_shared(facts, site, first)
            && self.changes_in_place(&inst.kind, changed)
    }

    /// Shallow copy: the copy shares what is below its top level with the copied value, by
    /// its class (R2) and the methods the copy runs.
    fn copies_shallowly(&self, copy: &CopySite<'_>) -> bool {
        let decls = self.cx.decls;
        let Some(class) = self.class_of(copy.copied).filter(|_| copy.keywords_known) else {
            return false;
        };
        match copy.kind {
            CopyKind::Container { reads, by_name, .. } => {
                BUILTIN_COPIED.contains(&class)
                    && reads.iter().all(|m| self.not_overridden(class, m, by_name))
            }
            CopyKind::CopyCopy => {
                decls.is_project_class(class)
                    && self.class_answer(ClassQuestion::CopyHooks, class, "", || {
                        decls.binds_none_of(class, COPY_HOOKS, |_| false)
                    })
            }
            CopyKind::ModelCopy => {
                decls.is_project_class(class)
                    && self.class_answer(ClassQuestion::ModelCopyHooks, class, "", || {
                        decls.binds_none_of(class, &["__copy__", "model_copy"], |base| {
                            base == "pydantic.BaseModel"
                        })
                    })
            }
            CopyKind::Other => false,
        }
    }

    /// What the value `v` holds on every path to its use, of the copies
    /// ([`Self::held_facts`]).
    fn held(&self, facts: &BelowCopies<'a>, v: VarId) -> Option<Held> {
        let held = facts.held.get_or_init(|| self.held_facts(facts));
        match held.get(&v) {
            Some(MustHeld::Known(h)) => Some(*h),
            _ => None,
        }
    }

    /// What each value holds on every path, of the copies: a copy (through copies of the
    /// reference and phis whose inputs agree), or an object below one, read by plain loads (an
    /// item of a builtin list or dict, a plain attribute of an instance) of a level the copy was
    /// not given a value of its own for, nor rebound before. An optimistic must-question over
    /// the function's values ([`fixpoint`]): a value starts with no fact and only loses one.
    /// Past the budget no value holds anything.
    fn held_facts(&self, facts: &BelowCopies<'a>) -> FxHashMap<VarId, MustHeld> {
        let shape = &self.ix.shape;
        let f = self.f;
        let vars: Vec<VarId> = (f.blocks.iter())
            .flat_map(|b| {
                (b.phis.iter().map(|p| p.dst)).chain(b.insts.iter().filter_map(|i| i.kind.dst()))
            })
            .collect();
        let index: FxHashMap<VarId, usize> =
            vars.iter().enumerate().map(|(i, v)| (*v, i)).collect();
        let readers = |i: usize, out: &mut Vec<usize>| {
            for u in shape.readers(vars[i]) {
                let dst = match *u {
                    Use::Inst(b, k) => f.block(b).insts[k as usize].kind.dst(),
                    Use::Phi(b, k) => Some(f.block(b).phis[k as usize].dst),
                    Use::Terminator(_) => None,
                };
                out.extend(dst.and_then(|d| index.get(&d).copied()));
            }
        };
        let spec = Sparse {
            nodes: vars.len(),
            readers: &readers,
            max_visits: 4,
        };
        let transfer = |i: usize, known: &Facts<MustHeld>| -> Option<MustHeld> {
            let v = vars[i];
            let of = |w: VarId| index.get(&w).and_then(|j| known.get(*j)).cloned();
            if let Some(&site) = facts.by_dst.get(&v) {
                return Some(MustHeld::Known(Held::Copy(site)));
            }
            match shape.def(v) {
                Def::Phi(b, k) => {
                    let phi = &f.block(b).phis[k as usize];
                    let mut joined: Option<MustHeld> = None;
                    for (_, input) in phi.incoming.iter().filter(|(_, x)| *x != v) {
                        if let Some(fact) = of(*input) {
                            match &mut joined {
                                Some(j) => {
                                    j.join(&fact);
                                }
                                None => joined = Some(fact),
                            }
                        }
                    }
                    joined
                }
                Def::Inst(b, k) => {
                    let kind = &f.block(b).insts[k as usize].kind;
                    if let InstKind::Copy { src, .. } = kind {
                        return of(*src);
                    }
                    Some(
                        self.loaded_held(facts, kind, v, (b, k), of)
                            .unwrap_or(MustHeld::Conflict),
                    )
                }
                Def::Param | Def::None => Some(MustHeld::Conflict),
            }
        };
        match fixpoint(&facts.budget, spec, transfer) {
            Answer::Known(found) => (found.into_vec().into_iter().enumerate())
                .filter_map(|(i, fact)| Some((vars[i], fact?)))
                .collect(),
            Answer::Open => FxHashMap::default(),
        }
    }

    /// What the load `kind` (defining `v` at `at`) holds of the copies, given what the
    /// container it reads holds (`of`): `None` when it is no plain load of a copy or of an
    /// object below one, or not yet known.
    fn loaded_held(
        &self,
        facts: &BelowCopies<'a>,
        kind: &InstKind,
        v: VarId,
        at: (BlockId, u32),
        of: impl Fn(VarId) -> Option<MustHeld>,
    ) -> Option<MustHeld> {
        let (container, level) = self.load_of(kind)?;
        // `pop` and `setdefault` may change what the container holds.
        if let InstKind::MethodCall { method, args, .. } = kind
            && (method != "get" || args.len() != 1)
        {
            return None;
        }
        if self.rebound_before(facts, container, &level, at) {
            return None;
        }
        let held = match of(container)? {
            MustHeld::Known(held) => held,
            MustHeld::Conflict => return Some(MustHeld::Conflict),
        };
        let plain = match held {
            Held::Copy(site) => {
                return Some(
                    if self.first_load_plain(&facts.sites[site], container, &level) {
                        MustHeld::Known(Held::Below { site, first: v })
                    } else {
                        MustHeld::Conflict
                    },
                );
            }
            Held::Below { .. } => match (&level, kind) {
                (Level::Attr(attr), _) => self.plain_read(container, attr),
                (Level::Key(_), InstKind::MethodCall { .. }) => self.plain_get(container),
                (Level::Key(_), _) => self.plain_item(container),
            },
        };
        Some(if plain {
            MustHeld::Known(held)
        } else {
            MustHeld::Conflict
        })
    }

    /// Whether the first load from the copy, of `level`, reads what the copy holds there: an
    /// item of a builtin list or dict copy, or a plain attribute of an instance copy.
    fn first_load_plain(&self, copy: &CopySite<'_>, container: VarId, level: &Level) -> bool {
        match (copy.kind, level) {
            (CopyKind::Container { made, .. }, Level::Key(_)) => {
                let made = made.or_else(|| match self.class_of(copy.copied) {
                    Some("builtins.dict") => Some(ContainerKind::Dict),
                    Some("builtins.list") => Some(ContainerKind::List),
                    _ => None,
                });
                matches!(made, Some(ContainerKind::Dict | ContainerKind::List))
            }
            (CopyKind::CopyCopy | CopyKind::ModelCopy, Level::Attr(attr)) => {
                self.class_of(copy.copied).is_some_and(|class| {
                    self.class_answer(ClassQuestion::Read, class, attr, || {
                        self.cx
                            .decls
                            .attr_read_finds_stored(class, attr, |base| self.catalog_stores(base))
                    })
                }) && self
                    .class_of(container)
                    .is_none_or(|c| Some(c) == self.class_of(copy.copied))
            }
            _ => false,
        }
    }

    /// Still shared: nothing gives the copy's `level` (the one its load `first` reads) a
    /// value of its own anywhere in the function. No store or delete of that level into the
    /// copy, no `update=` or keyword of the copy call naming it, no method changing a copied
    /// container's items, and the copy reaches no other code: its uses are plain loads,
    /// methods and builtins that only read it, iterations, and returns.
    fn still_shared(&self, facts: &BelowCopies<'a>, site: usize, first: VarId) -> bool {
        let shape = &self.ix.shape;
        let copy = &facts.sites[site];
        let Some(load) = shape.inst(first) else {
            return false;
        };
        let Some((_, level)) = self.load_of(&load.kind) else {
            return false;
        };
        if let Level::Attr(attr) = &level
            && copy.given.as_ref().is_none_or(|g| g.contains(attr))
        {
            return false;
        }
        let replaces = |stored: &Level| match (stored, &level) {
            (Level::Key(None), Level::Key(_)) | (Level::Key(Some(_)), Level::Key(None)) => true,
            (Level::Key(Some(k)), Level::Key(Some(l))) => k == l,
            (Level::Attr(a), Level::Attr(b)) => a == b,
            _ => false,
        };
        if self
            .stores_into(facts, copy.dst)
            .keys()
            .any(|(stored, _)| replaces(stored))
        {
            return false;
        }
        self.copy_uses(facts, copy.dst)
            .iter()
            .all(|(v, u)| match *u {
                Use::Inst(b, i) => {
                    self.copy_use_reads(&self.f.block(b).insts[i as usize].kind, *v, copy.kind)
                }
                Use::Terminator(b) => match self.f.block(b).terminator {
                    Terminator::Return(_) => true,
                    Terminator::Branch { .. } => matches!(copy.kind, CopyKind::Container { .. }),
                    _ => false,
                },
                Use::Phi(..) => false,
            })
    }

    /// Whether the instruction `kind`, a use of the copy `v`, rebinds nothing the copy holds
    /// beyond the stores [`Self::stores_into`] lists, and hands the copy to no other code.
    fn copy_use_reads(&self, kind: &InstKind, v: VarId, copied: CopyKind) -> bool {
        // An instance's operators, iteration, items, and formatting may run the project's code.
        if !matches!(copied, CopyKind::Container { .. }) {
            return match kind {
                InstKind::Copy { .. } => true,
                InstKind::AttrLoad { obj, attr, .. } => *obj == v && self.plain_read(v, attr),
                InstKind::AttrStore { obj, value, .. } => *obj == v && *value != v,
                InstKind::AttrDelete { obj, .. } => *obj == v,
                _ => false,
            };
        }
        match kind {
            InstKind::Copy { .. }
            | InstKind::MakeIter { .. }
            | InstKind::Format { .. }
            | InstKind::BuildString { .. } => true,
            InstKind::SubscriptLoad { obj, .. } => *obj == v,
            InstKind::AttrLoad { obj, attr, .. } => *obj == v && attr != "__dict__",
            InstKind::AttrStore { obj, value, .. } => *obj == v && *value != v,
            InstKind::AttrDelete { obj, .. } => *obj == v,
            InstKind::SubscriptStore {
                obj, value, index, ..
            } => *obj == v && *value != v && *index != v,
            InstKind::SubscriptDelete { obj, index, .. } => *obj == v && *index != v,
            InstKind::BinOp { in_place, lhs, .. } => !(*in_place && *lhs == v),
            InstKind::Compare { .. } | InstKind::UnaryOp { .. } => true,
            InstKind::MethodCall {
                recv,
                method,
                args,
                kwargs,
                ..
            } => {
                let handed = args.contains(&v) || kwargs.iter().any(|(_, a)| *a == v);
                *recv == v && !handed && READING_METHODS.contains(&method.as_str())
            }
            InstKind::Call {
                callee_var, kwargs, ..
            } => {
                kwargs.iter().all(|(name, a)| *a != v || name.is_some())
                    && matches!(
                        self.tcx.resolve_call(callee_var.and_then(|c| self.types.get(&c))),
                        Callee::External(s) if READING_BUILTINS.contains(&s.as_str())
                    )
            }
            _ => false,
        }
    }

    /// Changed below the copy: the instruction `kind` changes the object `changed` holds in
    /// place: an item store or delete, a changing method, or an in-place operator on a builtin
    /// list, dict, or set that no project subclass replaces (R2), or a plain attribute store or
    /// delete on an instance of a project class ([`heki_effect::ProjectDecls::attr_store_on_instance`]).
    fn changes_in_place(&self, kind: &InstKind, changed: VarId) -> bool {
        let decls = self.cx.decls;
        let Some(class) = self.class_of(changed) else {
            return false;
        };
        let container = ContainerKind::ALL.into_iter().find(|k| k.class() == class);
        match (kind, container) {
            (InstKind::SubscriptStore { .. }, Some(k)) => {
                k.takes_item_stores() && self.not_overridden(class, "__setitem__", false)
            }
            (InstKind::SubscriptDelete { .. }, Some(k)) => {
                k.takes_item_stores() && self.not_overridden(class, "__delitem__", false)
            }
            (InstKind::MethodCall { method, .. }, Some(k)) => {
                k.mutator(method) && self.not_overridden(class, method, true)
            }
            (InstKind::BinOp { op, .. }, Some(k)) => {
                k.in_place_op(*op) && self.not_overridden(class, op.in_place_method(), false)
            }
            (InstKind::AttrStore { attr, .. } | InstKind::AttrDelete { attr, .. }, None) => {
                decls.is_project_class(class)
                    && self.class_answer(ClassQuestion::Store, class, attr, || {
                        decls.attr_store_on_instance(class, attr, |base| self.catalog_stores(base))
                    })
            }
            _ => false,
        }
    }

    /// How a message names the value `v`: a parameter or a variable by its name, an attribute
    /// of one (`self.options`), a global by its name.
    fn value_text(&self, v: VarId) -> String {
        let shape = &self.ix.shape;
        let mut parts = Vec::new();
        let mut at = v;
        for _ in 0..4 {
            match shape.inst(at).map(|i| &i.kind) {
                Some(InstKind::AttrLoad { obj, attr, .. }) => {
                    parts.push(attr.clone());
                    at = *obj;
                }
                Some(InstKind::Copy { src, .. }) => at = *src,
                Some(InstKind::LoadGlobal { name, .. } | InstKind::LoadDeref { name, .. }) => {
                    parts.push(name.clone());
                    break;
                }
                _ => {
                    match self.f.var_name(at) {
                        Some(name) => parts.push(name.to_owned()),
                        None if parts.is_empty() => return "the copied value".to_owned(),
                        None => parts.push("...".to_owned()),
                    }
                    break;
                }
            }
        }
        parts.reverse();
        format!("`{}`", parts.join("."))
    }
}

/// The builtin containers whose copies read the items they hold.
const BUILTIN_COPIED: &[&str] = &[
    "builtins.dict",
    "builtins.list",
    "builtins.set",
    "builtins.tuple",
];

/// The members by which a class decides what `copy.copy` of its instances makes.
const COPY_HOOKS: &[&str] = &[
    "__copy__",
    "__reduce__",
    "__reduce_ex__",
    "__getstate__",
    "__setstate__",
];

/// The object a store or delete instruction stores into.
fn store_target(kind: &InstKind) -> Option<VarId> {
    match kind {
        InstKind::SubscriptStore { obj, .. }
        | InstKind::SubscriptDelete { obj, .. }
        | InstKind::AttrStore { obj, .. }
        | InstKind::AttrDelete { obj, .. } => Some(*obj),
        _ => None,
    }
}

/// The value an in-place change instruction changes.
fn changed_value(kind: &InstKind) -> Option<VarId> {
    match kind {
        InstKind::AttrStore { obj, .. }
        | InstKind::AttrDelete { obj, .. }
        | InstKind::SubscriptStore { obj, .. }
        | InstKind::SubscriptDelete { obj, .. } => Some(*obj),
        InstKind::MethodCall { recv, .. } => Some(*recv),
        InstKind::BinOp {
            lhs,
            in_place: true,
            ..
        } => Some(*lhs),
        _ => None,
    }
}

/// Whether a builtin display is a list display (not a tuple or set display).
fn is_list_display(kind: &InstKind) -> bool {
    matches!(
        kind,
        InstKind::AllocBuiltin {
            kind: heki_ir::BuiltinKind::List,
            ..
        }
    )
}
