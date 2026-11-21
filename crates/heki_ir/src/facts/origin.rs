//! Where the items a loop steps through come from (Advisory and Profitability only, NEVER a
//! Soundness premise), over the same builtin wrappers as [`FnIndex::iter_bound`].
//!
//! - The items pass through the wrappers that keep them, some of them, or tuples holding them:
//!   `list`, `tuple`, `set`, `sorted`, `reversed`, `iter`, `filter`, `enumerate`, `zip`, slices,
//!   dict views, and a comprehension, generator expression, or list the function fills only by
//!   appending that produces the item of a loop (or a component of it, as unpacking gives).
//! - Through `map`, the element expression of a comprehension, and `range(len(x))` only the
//!   count carries ([`Carries::Count`]); an item of what carries only a count has no source.
//! - The pieces `str.split` / `str.splitlines` (also after `strip`-like methods) cut are pieces of
//!   their receiver's text ([`ItemOrigin::text`]): `CONST.split(";")` is fixed, and the reader
//!   checks that a call whose text they cut returns text.
//! - A source is a parameter, an attribute or item of one, a method's receiver, a collection the
//!   code fixes, or the result of a call, which the reader resolves: a source is a catalog target,
//!   never a name. An item of a call's result, a global that is no constant, and anything else
//!   have none the walk can name, and the answer is `Open`.
//! - A phi takes the sources of its inputs along the edges that can run ([`super::Decided`]); an
//!   empty display (`xs or []`) adds no source.
//!
//! The walk is a [`Closure`] over questions `(function, value, steps)`: the steps are what the
//! way from the loop to the value applies to the value's sources (an item of it, a count of it,
//! pieces of its text), so each source is decided once per way of reading it.

use super::paths::{Scalar, scalar};
use super::{FnIndex, ItemStep, comprehension_id, is_immutable_kind};
use crate::walk::{Answer, Budget, Closure, Def, Fallible, KeySet, Use, Visit, reach};
use crate::{
    BinOpKind, BlockId, BuiltinKind, DictEntry, FunctionId, InstKind, ModuleIr, Terminator, VarId,
};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

#[cfg(test)]
mod tests;

/// Where the items of a collection come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemSource {
    /// A parameter other than a method's receiver (`*args` and `**kwargs` included), by its
    /// position.
    Param(u8),
    /// An attribute or an item of such a parameter, at any depth (`req.ids`, `payload["rows"]`,
    /// an item of a loop over the parameter).
    ParamPart(u8),
    /// A method's receiver (`self`, `cls`), or an attribute or item of it (`self.rows`).
    Receiver,
    /// The result of the call (or of the `__enter__` of the `with` statement) at instruction
    /// `index` of `block` of [`ItemOrigin::function`], which the reader resolves.
    Call { block: BlockId, index: u32 },
    /// A collection the code fixes: a literal, a display without unpacking that nothing grows,
    /// a module constant, `range` of constants.
    Fixed,
}

/// What a collection takes from its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Carries {
    /// The source's items, some of them, or tuples holding them.
    Items,
    /// One item per item of the source (that passes a filter): the count derives from it, the
    /// items do not.
    Count,
}

/// One source of a collection's items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ItemOrigin {
    /// The function whose parameter, receiver, or instruction the source is: the walk crosses
    /// functions (a generator expression's loop reads the iterable its creator passes).
    pub function: FunctionId,
    pub source: ItemSource,
    pub carries: Carries,
    /// The items are pieces of the source's text (`str.split`, `str.splitlines`, also after
    /// `strip`-like methods), which holds only when the source is text.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub text: bool,
}

/// Methods of `str` (and `bytes`) that cut their receiver's text into pieces.
const TEXT_PIECES: &[&str] = &["split", "rsplit", "splitlines"];

/// Methods of `str` (and `bytes`) whose result is their receiver's text, changed in place.
const TEXT_METHODS: &[&str] = &[
    "casefold",
    "decode",
    "lower",
    "lstrip",
    "removeprefix",
    "removesuffix",
    "rstrip",
    "strip",
    "upper",
];

/// What the way from a loop to a value applies to the value's sources, in the order the walk
/// meets the steps (from the collection towards its sources).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
struct Steps {
    /// How many times an item (attribute, subscript, loop item) of the value is taken.
    parts: u8,
    flags: u8,
}

/// The most items of items the walk follows; a deeper item has no source it names.
const MAX_PARTS: u8 = 3;

impl Steps {
    /// Pieces of the value's text are cut.
    const PIECES: u8 = 1;
    /// Only a count of the value's items carries.
    const COUNT: u8 = 2;
    /// The value's text is changed in place.
    const TEXT: u8 = 4;

    fn has(self, flag: u8) -> bool {
        self.flags & flag != 0
    }

    fn with(self, flag: u8) -> Self {
        Self {
            flags: self.flags | flag,
            ..self
        }
    }

    /// The steps with one more item taken.
    fn part(self) -> Option<Self> {
        (self.parts < MAX_PARTS).then_some(Self {
            parts: self.parts + 1,
            ..self
        })
    }

    /// The steps that reach an element of a display or a value appended once, with the item
    /// that is that element taken: what is left to take is items of it.
    fn unpart(self) -> Self {
        Self {
            parts: self.parts.saturating_sub(1),
            ..self
        }
    }

    /// The steps with a count taken first: an item or a piece of what carries only a count has
    /// no source the walk can name.
    fn counted(self) -> Option<Self> {
        (self.parts == 0 && !self.has(Self::PIECES)).then(|| self.with(Self::COUNT))
    }

    /// What these steps make of `source`, of `function`: `None` when an item of a call's
    /// result is taken.
    fn apply(self, function: FunctionId, source: ItemSource) -> Option<ItemOrigin> {
        let taken = self.parts > 0 || self.has(Self::PIECES);
        let source = match source {
            ItemSource::Param(p) | ItemSource::ParamPart(p) if taken => ItemSource::ParamPart(p),
            ItemSource::Call { .. } if self.parts > 0 => return None,
            other => other,
        };
        Some(ItemOrigin {
            function,
            source,
            carries: if self.has(Self::COUNT) {
                Carries::Count
            } else {
                Carries::Items
            },
            text: source != ItemSource::Fixed && self.has(Self::TEXT | Self::PIECES),
        })
    }
}

/// A question of the walk: the sources of `value` in `function`, read through `steps`.
type Key = (FunctionId, VarId, Steps);
type Fact = Fallible<KeySet<ItemOrigin>>;

/// The item origins of one module's collections, computed on demand; each question is decided
/// once, whichever loop asks it.
pub struct ItemOrigins<'m> {
    ir: &'m ModuleIr,
    indexes: RefCell<FxHashMap<FunctionId, Rc<FnIndex<'m>>>>,
    /// Per function, the values whose object something in it may grow
    /// ([`FnIndex::grown_vars`]), decided once for all its questions.
    grown: RefCell<FxHashMap<FunctionId, Rc<FxHashSet<VarId>>>>,
    closure: Closure<Key, Fact>,
}

impl<'m> ItemOrigins<'m> {
    pub fn new(ir: &'m ModuleIr) -> Self {
        Self {
            ir,
            indexes: RefCell::new(FxHashMap::default()),
            grown: RefCell::new(FxHashMap::default()),
            closure: Closure::new(),
        }
    }

    /// Where the items the `for` loop (or comprehension clause) `f.loops[li]` steps through
    /// come from, charged to `budget`; `Open` for a `while` loop, which steps through nothing.
    pub fn of_loop(
        &self,
        budget: &Budget,
        f: FunctionId,
        li: usize,
    ) -> Answer<BTreeSet<ItemOrigin>> {
        let ix = self.index(f);
        let Some(iterable) = ix.f.loops.get(li).and_then(|lp| ix.loop_iterable(lp)) else {
            return Answer::Open;
        };
        // `own` decides a question's successors with its own fact; `next` hands them on.
        let pending: RefCell<Vec<Key>> = RefCell::new(Vec::new());
        let own = |key: Key| {
            let (fact, next) = self.decide_origin(budget, key);
            *pending.borrow_mut() = next;
            fact
        };
        let next = |_: Key, out: &mut Vec<Key>| out.append(&mut pending.borrow_mut());
        self.closure
            .ask(budget, (f, iterable, Steps::default()), own, next)
            .and_then(|fact| match &fact.0 {
                Some(origins) => Answer::Known(origins.iter().copied().collect()),
                None => Answer::Open,
            })
    }

    fn index(&self, f: FunctionId) -> Rc<FnIndex<'m>> {
        Rc::clone(
            self.indexes
                .borrow_mut()
                .entry(f)
                .or_insert_with(|| Rc::new(FnIndex::new(self.ir.function(f), &self.ir.constants))),
        )
    }

    /// Whether something in `ix`'s function may grow the object `v` holds, or let other code
    /// grow it: its items are then no longer only those the walk sees.
    fn may_grow(&self, ix: &FnIndex<'m>, v: VarId) -> bool {
        let grown = Rc::clone(
            self.grown
                .borrow_mut()
                .entry(ix.f.id)
                .or_insert_with(|| Rc::new(ix.grown_vars())),
        );
        grown.contains(&v)
    }

    /// The fact a question holds by itself, and the questions it rests on.
    fn decide_origin(&self, budget: &Budget, (f, v, steps): Key) -> (Fact, Vec<Key>) {
        let ix = self.index(f);
        let mut walk = Decision {
            f,
            steps,
            fact: Fact::default(),
            next: Vec::new(),
        };
        match ix.shape.def(v) {
            Def::Param | Def::None => Self::of_param(&ix, v, &mut walk),
            Def::Phi(b, i) => {
                let decided = ix.decided();
                for &(pred, x) in &ix.f.block(b).phis[i as usize].incoming {
                    if decided.takes(pred, b) {
                        walk.ask(x, steps);
                    }
                }
            }
            Def::Inst(b, i) => self.of_instruction(budget, &ix, (b, i as usize), &mut walk),
        }
        (walk.fact, walk.next)
    }

    /// A parameter, or the `.0` through which a generator expression or comprehension function
    /// receives its outermost iterable from the function creating it.
    fn of_param(ix: &FnIndex<'m>, v: VarId, walk: &mut Decision) {
        let func = ix.f;
        let passed = func.passed_from.filter(|_| func.var_name(v) == Some(".0"));
        if let Some((creator, iterable)) = passed {
            return walk.ask_in(creator, iterable, walk.steps);
        }
        let position = func.params.iter().position(|p| p.var == v);
        let receiver = position == Some(0)
            && func.owner_class.is_some()
            && !func.has_decorator(&["staticmethod"]);
        let source = position.and_then(|p| u8::try_from(p).ok()).map(|p| {
            if receiver {
                ItemSource::Receiver
            } else {
                ItemSource::Param(p)
            }
        });
        walk.add_source(source);
    }

    fn of_instruction(
        &self,
        budget: &Budget,
        ix: &FnIndex<'m>,
        (b, i): (BlockId, usize),
        walk: &mut Decision,
    ) {
        let inst = &ix.f.block(b).insts[i];
        let steps = walk.steps;
        match &inst.kind {
            InstKind::Copy { src, .. }
            | InstKind::Await { value: src, .. }
            | InstKind::MakeIter { iterable: src, .. } => walk.ask(*src, steps),
            InstKind::Const { .. } => walk.add_source(Some(ItemSource::Fixed)),
            InstKind::AllocBuiltin { dst, kind, elems } => {
                self.of_display(budget, ix, *dst, *kind, elems, walk);
            }
            InstKind::DictLiteral { dst, entries } => {
                if self.may_grow(ix, *dst) {
                    return walk.fail();
                }
                for e in entries {
                    match e {
                        DictEntry::KeyValue { key, .. } => walk.element(*key),
                        DictEntry::Unpack { dict } => walk.ask(*dict, steps),
                    }
                }
            }
            // A `LoadName` reads a name a `del` may have unbound: never a constant.
            InstKind::LoadName { .. } => walk.fail(),
            InstKind::LoadGlobal {
                name,
                is_lazy_import: false,
                ..
            } => {
                let m = &self.ir.constants;
                // The items of a string, or of a tuple of strings, are fixed too; those of another
                // display are values the walk does not follow.
                let constant = !m.dynamic
                    && (m.ints.contains_key(name)
                        || m.strs.contains_key(name)
                        || m.str_tuples.contains_key(name)
                        || (m.displays.contains_key(name) && steps.parts == 0)
                        || m.bools.contains_key(name));
                if constant {
                    walk.add_source(Some(ItemSource::Fixed));
                } else {
                    walk.fail();
                }
            }
            InstKind::AttrLoad { obj, .. }
            | InstKind::SubscriptLoad {
                obj,
                is_slice: false,
                ..
            }
            | InstKind::IterNext { iter: obj, .. } => match steps.part() {
                Some(steps) => walk.ask(*obj, steps),
                None => walk.fail(),
            },
            InstKind::SubscriptLoad {
                obj,
                is_slice: true,
                ..
            } => walk.ask(*obj, steps),
            InstKind::Call { .. } | InstKind::MethodCall { .. } => self.of_call(ix, (b, i), walk),
            InstKind::EnterWith { .. } => walk.call_at(b, i),
            InstKind::MakeGenExpr {
                func_id: Some(g), ..
            } => self.of_generator(*g, walk),
            InstKind::BinOp {
                op: BinOpKind::Add,
                lhs,
                rhs,
                in_place: false,
                ..
            } => {
                walk.ask(*lhs, steps);
                walk.ask(*rhs, steps);
            }
            _ => walk.fail(),
        }
    }

    /// A list, tuple, set, or dict display: its starred operands' sources and each other operand
    /// as an element ([`Decision::element`]), when nothing grows it; else what the function
    /// appends to it.
    fn of_display(
        &self,
        budget: &Budget,
        ix: &FnIndex<'m>,
        dst: VarId,
        kind: BuiltinKind,
        elems: &[VarId],
        walk: &mut Decision,
    ) {
        for (k, e) in elems.iter().enumerate() {
            if ix.shape.is_starred(dst, k) {
                walk.ask(*e, walk.steps);
            } else {
                walk.element(*e);
            }
        }
        if is_immutable_kind(kind) || !self.may_grow(ix, dst) {
            return;
        }
        self.of_appends(budget, ix, dst, walk);
    }

    /// What a list or set `root` holds when the function only appends to it (`append`, `add`):
    /// each appended value as [`Decision::produces`] reads it. Any other use of it but copying,
    /// iterating, testing, and returning it may add items the walk does not see, and an append
    /// in a loop over the list itself adds as many items as it finds there.
    fn of_appends(&self, budget: &Budget, ix: &FnIndex<'m>, root: VarId, walk: &mut Decision) {
        let shape = &ix.shape;
        let aliases: RefCell<Vec<VarId>> = RefCell::new(Vec::new());
        let appends: RefCell<Vec<(BlockId, VarId)>> = RefCell::new(Vec::new());
        let alias_of = |u: &Use| match *u {
            Use::Phi(b, i) => Some(ix.f.block(b).phis[i as usize].dst),
            Use::Inst(b, i) => match &ix.f.block(b).insts[i as usize].kind {
                InstKind::Copy { dst, .. } => Some(*dst),
                _ => None,
            },
            Use::Terminator(_) => None,
        };
        let found = reach(
            budget,
            [root],
            |a, out| out.extend(shape.readers(a).iter().filter_map(alias_of)),
            |a| {
                aliases.borrow_mut().push(a);
                for u in shape.readers(a) {
                    let allowed = match *u {
                        Use::Phi(..) => true,
                        Use::Terminator(b) => matches!(
                            ix.f.block(b).terminator,
                            Terminator::Branch { .. } | Terminator::Return(_)
                        ),
                        Use::Inst(b, i) => match &ix.f.block(b).insts[i as usize].kind {
                            InstKind::Copy { .. } | InstKind::MakeIter { .. } => true,
                            InstKind::MethodCall {
                                recv,
                                method,
                                args,
                                kwargs,
                                star_from: None,
                                ..
                            } if *recv == a
                                && matches!(method.as_str(), "append" | "add")
                                && kwargs.is_empty()
                                && args.len() == 1 =>
                            {
                                appends.borrow_mut().push((b, args[0]));
                                true
                            }
                            _ => false,
                        },
                    };
                    if !allowed {
                        return Visit::Stop(());
                    }
                }
                Visit::Expand
            },
        );
        if !matches!(found, Answer::Known(None)) {
            return walk.fail();
        }
        let aliases = aliases.into_inner();
        for (b, value) in appends.into_inner() {
            walk.produces(ix, b, value, &aliases);
        }
    }

    /// What iterating the generator expression `g` yields: what each `yield` produces.
    fn of_generator(&self, g: FunctionId, walk: &mut Decision) {
        let gx = self.index(g);
        let mut inner = Decision {
            f: g,
            ..walk.clone_empty()
        };
        // A generator expression yields its element on each step; `yield` cannot appear in it.
        for bb in &gx.f.blocks {
            for inst in &bb.insts {
                if let InstKind::Yield {
                    value: Some(value), ..
                } = inst.kind
                {
                    inner.produces(&gx, bb.id, value, &[]);
                }
            }
        }
        walk.absorb(inner);
    }

    fn of_call(&self, ix: &FnIndex<'m>, (b, i): (BlockId, usize), walk: &mut Decision) {
        let inst = &ix.f.block(b).insts[i];
        let steps = walk.steps;
        if let Some((step, built)) = ix.item_step(inst) {
            // A new container the function grows holds more than its input's items.
            if let (Some(_), Some(dst)) = (built, inst.kind.dst())
                && self.may_grow(ix, dst)
            {
                return walk.call_at(b, i);
            }
            return match step {
                ItemStep::Same(x) | ItemStep::Kept(x) => walk.ask(x, steps),
                ItemStep::Mapped(x) => walk.ask_counted(x),
                ItemStep::Zip(args) if !args.is_empty() => {
                    for a in args {
                        walk.ask(a, steps);
                    }
                }
                ItemStep::Zip(_) => walk.fail(),
                ItemStep::Range(args) => Self::of_range(ix, &args, walk),
            };
        }
        let comprehension = match &inst.kind {
            InstKind::Call {
                callee_var, callee, ..
            } => comprehension_id(*callee_var, callee),
            _ => None,
        };
        if let Some(id) = comprehension {
            // A comprehension function (before 3.12) returns the list it fills.
            let g = FunctionId(id);
            for bb in self
                .ir
                .functions
                .get(id as usize)
                .into_iter()
                .flat_map(|f| &f.blocks)
            {
                if let Terminator::Return(Some(r)) = bb.terminator {
                    walk.ask_in(g, r, steps);
                }
            }
            return;
        }
        match &inst.kind {
            InstKind::MethodCall { recv, method, .. } if TEXT_PIECES.contains(&method.as_str()) => {
                walk.ask(*recv, steps.with(Steps::PIECES));
            }
            InstKind::MethodCall { recv, method, .. }
                if TEXT_METHODS.contains(&method.as_str()) =>
            {
                walk.ask(*recv, steps.with(Steps::TEXT));
            }
            _ => walk.call_at(b, i),
        }
    }

    /// `range(...)`: fixed for constant bounds; one item per item of `x` for `range(len(x))`
    /// and `range(k, len(x))` (step 1) with a constant `k`.
    fn of_range(ix: &FnIndex<'m>, args: &[VarId], walk: &mut Decision) {
        let int = |v: VarId| matches!(scalar(ix, v), Some(Scalar::Int(_)));
        let one = |v: VarId| scalar(ix, v) == Some(Scalar::Int(1));
        if args.iter().all(|a| int(*a)) {
            return walk.add_source(Some(ItemSource::Fixed));
        }
        let measured = match args {
            [stop] => ix.len_argument(*stop),
            [start, stop] if int(*start) => ix.len_argument(*stop),
            [start, stop, step] if int(*start) && one(*step) => ix.len_argument(*stop),
            _ => None,
        };
        match measured {
            Some(x) => walk.ask_counted(x),
            None => walk.fail(),
        }
    }
}

/// The fact of one question being decided, and the questions it rests on.
struct Decision {
    f: FunctionId,
    steps: Steps,
    fact: Fact,
    next: Vec<Key>,
}

impl Decision {
    fn clone_empty(&self) -> Self {
        Self {
            f: self.f,
            steps: self.steps,
            fact: Fact::default(),
            next: Vec::new(),
        }
    }

    fn absorb(&mut self, other: Self) {
        use crate::walk::Join;
        self.fact.join(&other.fact);
        self.next.extend(other.next);
    }

    fn fail(&mut self) {
        self.fact = Fact::failed();
    }

    /// Add `source` read through the steps; `None`, or a source the steps cannot read (an item
    /// of a call's result), has none the walk can name.
    fn add_source(&mut self, source: Option<ItemSource>) {
        let Some(origin) = source.and_then(|s| self.steps.apply(self.f, s)) else {
            return self.fail();
        };
        if let Some(origins) = &mut self.fact.0 {
            origins.insert(origin);
        }
    }

    /// One element `e` of a collection: a fixed item of it, and when an item of the collection
    /// is taken, `e` itself, whose items are what is left to take.
    fn element(&mut self, e: VarId) {
        if self.steps.parts == 0 {
            self.add_source(Some(ItemSource::Fixed));
        } else {
            self.ask(e, self.steps.unpart());
        }
    }

    fn call_at(&mut self, b: BlockId, i: usize) {
        let call = u32::try_from(i)
            .ok()
            .map(|index| ItemSource::Call { block: b, index });
        self.add_source(call);
    }

    fn ask(&mut self, v: VarId, steps: Steps) {
        self.next.push((self.f, v, steps));
    }

    fn ask_in(&mut self, f: FunctionId, v: VarId, steps: Steps) {
        self.next.push((f, v, steps));
    }

    fn ask_counted(&mut self, v: VarId) {
        match self.steps.counted() {
            Some(steps) => self.ask(v, steps),
            None => self.fail(),
        }
    }

    /// What producing `value` in block `b` of `ix`'s function (an append, a `yield`) adds: the
    /// item of the innermost loop around `b` (or a component of it) adds that loop's items; any
    /// other value one item per iteration of it; a value outside every loop one element. A loop
    /// over one of `aliases` (the collection being filled) feeds itself.
    fn produces(&mut self, ix: &FnIndex<'_>, b: BlockId, value: VarId, aliases: &[VarId]) {
        let shape = &ix.shape;
        let Some(lp) = shape.innermost(b) else {
            let f = std::mem::replace(&mut self.f, ix.f.id);
            self.element(value);
            self.f = f;
            return;
        };
        let (Some(iterable), Some(item)) = (
            ix.loop_iterable(lp),
            ix.f.block(lp.header)
                .insts
                .first()
                .and_then(|i| i.kind.dst()),
        ) else {
            return self.fail();
        };
        if aliases.contains(&shape.copy_root(iterable)) {
            return self.fail();
        }
        let value = shape.copy_root(value);
        let component = match shape.inst(value).map(|i| &i.kind) {
            Some(InstKind::SubscriptLoad {
                obj,
                index,
                is_slice: false,
                ..
            }) if matches!(scalar(ix, *index), Some(Scalar::Int(_))) => Some(shape.copy_root(*obj)),
            _ => None,
        };
        if value == item || component == Some(item) {
            self.ask_in(ix.f.id, iterable, self.steps);
        } else {
            let f = std::mem::replace(&mut self.f, ix.f.id);
            self.ask_counted(iterable);
            self.f = f;
        }
    }
}
