//! Guards on execution effects (docs/ANALYSIS_AND_KB_SPEC.md section 2.2): what makes an
//! effect not happen although the code reaching it runs.
//!
//! - **Input guards**: tests of the function's own inputs that dominate an effect, `if cache:`,
//!   `if x is None:`, `isinstance(x, T)` (also inside `and` / `or` / `not`, early returns, and
//!   conditional expressions), on a parameter or on an attribute of one that the function never
//!   assigns. A call whose argument decides such a test against the effect (a constant, the
//!   literal default of an omitted parameter, a test the caller has already made on the same
//!   input) does not reach the effect; an undecided test on a caller input becomes the
//!   caller's guard.
//! - **Lazy initialization**: an effect inside the branch that only an unset attribute leads to
//!   (`if not self._sock:`, `if self._sock: return`), when every way out of the branch sets the
//!   attribute, runs at most once per object. `EffectGuards::once_per` records where the objects
//!   come from; a loop calling it on objects it does not construct does not run it per
//!   iteration. The recognition of the branch is shared with `MUT107`
//!   ([`lazy_init_branches`]).
//!
//! The local facts are computed here (`LocalGuards`), and one transfer ([`through_call`]) maps
//! a callee's guards across a call site for both the fixpoint and the rules. Anything not
//! recognized keeps the effect.

use crate::resolver::{Val, aliases_of};
use crate::{
    ArgBinding, BoundCallee, CallSite, ConstArg, ConstClass, EffectGuards, GuardSubject, GuardTest,
    InputGuard, SiteRole, Slot,
};
use heki_ir::{
    BlockId, BuiltinKind, CompareOpKind, ConstLiteral, DictEntry, Inst, InstKind, IrFunction,
    NaturalLoop, PhiNode, Terminator, UnaryOpKind, VarId,
};
use heki_kb::ExecEffectKind;
use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::{OnceCell, RefCell};
use std::collections::BTreeMap;

// --------------------------------------------------------------------------- the summary facts

impl EffectGuards {
    /// No guard at all: the effect runs whenever the function does.
    pub fn is_trivial(&self) -> bool {
        self.inputs.is_empty() && self.once_per.is_none()
    }

    /// The guards of two occurrences together: the tests both need, and lazy initialization
    /// only when both are lazy (from the parameters of either).
    fn join(&mut self, other: &Self) {
        self.inputs.retain(|g| other.inputs.contains(g));
        self.once_per = self.once_per.zip(other.once_per).map(|(a, b)| a | b);
    }
}

/// Record one more occurrence of `effect` with guards `occ`; `had`: the function already
/// reaches the effect (an effect without an entry is unguarded).
pub(crate) fn add_occurrence(
    guards: &mut BTreeMap<ExecEffectKind, EffectGuards>,
    effect: ExecEffectKind,
    had: bool,
    occ: &EffectGuards,
) {
    if !had {
        if !occ.is_trivial() {
            guards.insert(effect, occ.clone());
        }
        return;
    }
    if let Some(g) = guards.get_mut(&effect) {
        g.join(occ);
        if g.is_trivial() {
            guards.remove(&effect);
        }
    }
}

impl ConstArg {
    /// The constant a literal is (`None` for a literal of unknown class).
    pub fn of_literal(literal: &ConstLiteral) -> Option<Self> {
        let (class, truthy) = match literal {
            ConstLiteral::None => (ConstClass::None, Some(false)),
            ConstLiteral::Bool(b) => (ConstClass::Bool, Some(*b)),
            ConstLiteral::Int(n) => (ConstClass::Int, Some(*n != 0)),
            ConstLiteral::Float(text) => (
                ConstClass::Float,
                text.parse::<f64>().ok().map(|x| x != 0.0),
            ),
            ConstLiteral::Str(s) => (ConstClass::Str, Some(!s.is_empty())),
            ConstLiteral::Bytes => (ConstClass::Bytes, None),
            ConstLiteral::Ellipsis => (ConstClass::Other, Some(true)),
            ConstLiteral::Other => return None,
        };
        Some(Self { class, truthy })
    }

    /// The outcome of `test` on this value, when the value decides it.
    fn decide(&self, test: &GuardTest) -> Option<bool> {
        match test {
            GuardTest::Truthy => self.truthy,
            GuardTest::IsNone => Some(self.class == ConstClass::None),
            GuardTest::IsInstance(classes) => self.class.is_instance(classes),
        }
    }
}

impl ConstClass {
    /// The builtin classes an instance of this class is an instance of.
    fn mro(self) -> &'static [&'static str] {
        match self {
            Self::None => &["builtins.NoneType", "builtins.object"],
            Self::Bool => &["builtins.bool", "builtins.int", "builtins.object"],
            Self::Int => &["builtins.int", "builtins.object"],
            Self::Float => &["builtins.float", "builtins.object"],
            Self::Str => &["builtins.str", "builtins.object"],
            Self::Bytes => &["builtins.bytes", "builtins.object"],
            Self::List => &["builtins.list", "builtins.object"],
            Self::Tuple => &["builtins.tuple", "builtins.object"],
            Self::Set => &["builtins.set", "builtins.object"],
            Self::Dict => &["builtins.dict", "builtins.object"],
            Self::Other => &[],
        }
    }

    /// `isinstance(value, classes)`: true when a class is in the value's builtin MRO, false
    /// when every class is a builtin type outside it (or `os.PathLike`, which no builtin
    /// implements); otherwise unknown.
    fn is_instance(self, classes: &[String]) -> Option<bool> {
        let mro = self.mro();
        if mro.is_empty() {
            return None;
        }
        if classes.iter().any(|c| mro.contains(&c.as_str())) {
            return Some(true);
        }
        classes
            .iter()
            .all(|c| {
                c == "os.PathLike"
                    || c.strip_prefix("builtins.")
                        .is_some_and(|b| BuiltinKind::from_name(b).is_some())
            })
            .then_some(false)
    }
}

/// What the caller knows of `test` on `subject` from the tests `known` that hold at the call.
fn known_outcome(known: &[InputGuard], subject: &GuardSubject, test: &GuardTest) -> Option<bool> {
    known
        .iter()
        .filter(|k| &k.subject == subject)
        .find_map(|k| match (&k.test, test) {
            (a, b) if a == b => Some(k.holds),
            // `None` is falsy; a truthy value is not `None`.
            (GuardTest::IsNone, GuardTest::Truthy) | (GuardTest::Truthy, GuardTest::IsNone)
                if k.holds =>
            {
                Some(false)
            }
            // An instance of one of `cs` is an instance of any superset of `cs`, and not an
            // instance of `cs` is not an instance of any subset.
            (GuardTest::IsInstance(cs), GuardTest::IsInstance(ds))
                if k.holds && cs.iter().all(|c| ds.contains(c)) =>
            {
                Some(true)
            }
            (GuardTest::IsInstance(cs), GuardTest::IsInstance(ds))
                if !k.holds && ds.iter().all(|d| cs.contains(d)) =>
            {
                Some(false)
            }
            _ => None,
        })
}

/// What a callee's effect becomes in the caller at one call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Through {
    /// Its guards in the caller's terms, per call of the caller: the call's own guards and the
    /// callee's undecided tests of caller inputs.
    pub guards: EffectGuards,
    /// The loops around the call across whose iterations it runs at most once per object, and
    /// the objects stay the same (`SiteEffect::once_loops`).
    pub once_loops: u64,
}

/// Map the guards `callee` of one effect of `bc` across `site`: `None` when an argument or a
/// test the caller has made decides a guard against the effect (it does not run here).
pub(crate) fn through_call(
    site: &CallSite,
    bc: &BoundCallee,
    callee: Option<&EffectGuards>,
) -> Option<Through> {
    let mut guards = EffectGuards {
        inputs: site.guards.iter().cloned().collect(),
        once_per: site.once_per,
    };
    let mut once_loops = site.once_loops;
    let Some(callee) = callee else {
        return Some(Through { guards, once_loops });
    };
    let binding = |p: usize| bc.params.get(p).and_then(|o| o.binding.as_ref());
    for g in &callee.inputs {
        let Some(b) = binding(usize::from(g.subject.param)) else {
            continue;
        };
        if g.subject.attr.is_none()
            && let Some(outcome) = b.constant.and_then(|c| c.decide(&g.test))
        {
            if outcome != g.holds {
                return None;
            }
            continue;
        }
        let subject = match (&b.input, &g.subject.attr) {
            (Some(GuardSubject { param, attr: None }), attr) => GuardSubject {
                param: *param,
                attr: attr.clone(),
            },
            (Some(input), None) => input.clone(),
            _ => continue,
        };
        match known_outcome(&site.guards, &subject, &g.test) {
            Some(outcome) if outcome != g.holds => return None,
            Some(_) => {}
            None => {
                guards.inputs.insert(InputGuard {
                    subject,
                    test: g.test.clone(),
                    holds: g.holds,
                });
            }
        }
    }
    if let Some(roots) = callee.once_per {
        // Per call: the objects come from the arguments, unless one is constructed here.
        // Per iteration of a loop: unless one is constructed inside that loop. Where the
        // callee's facts run elsewhere than at the call (a generator iterated later), the
        // arguments' loops are not those of the facts: no loop is relaxed.
        let mut per_call = Some(0u64);
        let mut stays = if site.role == SiteRole::Call {
            u64::MAX
        } else {
            0
        };
        for p in (0..64).filter(|p| roots & (1u64 << p) != 0) {
            match binding(p) {
                Some(b) if !b.derived_fresh => {
                    per_call = per_call.map(|m| m | b.derived_params);
                    stays &= b.stays_in_loops;
                }
                Some(b) => {
                    per_call = None;
                    stays &= b.stays_in_loops;
                }
                None => {
                    per_call = None;
                    stays = 0;
                }
            }
        }
        guards.once_per = guards.once_per.or(per_call);
        once_loops |= stays;
    }
    Some(Through { guards, once_loops })
}

// ---------------------------------------------------------------------- lazy initialization

/// A `None` / falsy test of `obj.attr` and the block that only its "unset" outcome leads to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LazyInitBranch {
    /// The block ending in the test.
    pub test: BlockId,
    /// The `then` block of `if obj.attr is None:` / `== None` / `if not obj.attr:`, the `else`
    /// block of `is not None` / `!= None` / `if obj.attr:`.
    pub init: BlockId,
    pub obj: VarId,
    pub attr: String,
}

/// The lazy initialization branches of `f` (`MUT107` and the effect guards). Only a block the
/// test alone leads to counts: an `if` without `else` falls through to a block that the other
/// branch reaches too.
pub fn lazy_init_branches(f: &IrFunction) -> Vec<LazyInitBranch> {
    let mut defs: FxHashMap<VarId, &InstKind> = FxHashMap::default();
    for inst in f.blocks.iter().flat_map(|b| &b.insts) {
        if let Some(d) = inst.kind.dst() {
            defs.insert(d, &inst.kind);
        }
    }
    branches(f, &|v| defs.get(v).copied())
}

type DefOf<'d, 'a> = &'d dyn Fn(&VarId) -> Option<&'a InstKind>;

/// [`lazy_init_branches`] with the defining instruction of every variable.
fn branches(f: &IrFunction, def: DefOf<'_, '_>) -> Vec<LazyInitBranch> {
    let loads_attr = |v: &VarId| {
        let mut v = *v;
        while let Some(InstKind::Copy { src, .. }) = def(&v) {
            v = *src;
        }
        match def(&v) {
            Some(InstKind::AttrLoad { obj, attr, .. }) => Some((*obj, attr.clone())),
            _ => None,
        }
    };
    let is_none = |v: &VarId| {
        matches!(
            def(v),
            Some(InstKind::Const {
                literal: ConstLiteral::None,
                ..
            })
        )
    };
    let none_test = |l: &VarId, r: &VarId| {
        if is_none(r) {
            loads_attr(l)
        } else if is_none(l) {
            loads_attr(r)
        } else {
            None
        }
    };
    let mut found = Vec::new();
    for bb in &f.blocks {
        let Terminator::Branch {
            cond,
            then_bb,
            else_bb,
        } = &bb.terminator
        else {
            continue;
        };
        let test = match def(cond) {
            Some(InstKind::Compare {
                op: CompareOpKind::Is | CompareOpKind::Eq,
                lhs,
                rhs,
                ..
            }) => none_test(lhs, rhs).map(|t| (t, *then_bb)),
            Some(InstKind::Compare {
                op: CompareOpKind::IsNot | CompareOpKind::NotEq,
                lhs,
                rhs,
                ..
            }) => none_test(lhs, rhs).map(|t| (t, *else_bb)),
            Some(InstKind::UnaryOp {
                op: UnaryOpKind::Not,
                operand,
                ..
            }) => loads_attr(operand).map(|t| (t, *then_bb)),
            _ => None,
        }
        .or_else(|| loads_attr(cond).map(|t| (t, *else_bb)));
        if let Some(((obj, attr), init)) = test {
            found.push(LazyInitBranch {
                test: bb.id,
                init,
                obj,
                attr,
            });
        }
    }
    if found.is_empty() {
        return found;
    }
    let preds = f.predecessors();
    found.retain(|b| preds[b.init.0 as usize] == [b.test]);
    found
}

/// A lazy initialization region of an effect site: the blocks `init` dominates, when every path
/// from it to a normal return, or back to the test, assigns the attribute.
struct LazyRegion {
    test: BlockId,
    obj: VarId,
    /// Parameters the object comes from; `None` when it may be one the function constructs
    /// (initialized on every call).
    root: Option<u64>,
    /// Membership by block id.
    blocks: Vec<bool>,
}

// ------------------------------------------------------------------------- the local facts

/// Where an object may come from: parameters (bits) and objects the function constructs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Derived {
    params: u64,
    fresh: bool,
}

/// Visit the values an instruction's result comes from, for [`Derived`]: what it reads, calls,
/// or iterates (`obj.attr`, `obj[k]`, `obj.m(a)`, `f(a)`, iterators, `with`, `await`).
fn for_each_source(kind: &InstKind, mut visit: impl FnMut(VarId)) {
    match kind {
        InstKind::Copy { src: v, .. }
        | InstKind::AttrLoad { obj: v, .. }
        | InstKind::SubscriptLoad { obj: v, .. }
        | InstKind::MakeIter { iterable: v, .. }
        | InstKind::IterNext { iter: v, .. }
        | InstKind::EnterWith { mgr: v, .. }
        | InstKind::Await { value: v, .. }
        | InstKind::YieldFrom { value: v, .. } => visit(*v),
        InstKind::Call { .. } | InstKind::MethodCall { .. } => {
            kind.operands().into_iter().for_each(visit);
        }
        _ => {}
    }
}

/// The inputs the guard analysis of one function reads (owned by the summary extractor).
pub(crate) struct GuardInputs<'f, 'a> {
    pub f: &'a IrFunction,
    pub defs: &'f FxHashMap<VarId, &'a Inst>,
    pub uses: &'f FxHashMap<VarId, Vec<&'a InstKind>>,
    pub types: &'f FxHashMap<VarId, Val>,
    /// The classes each `isinstance` check names, by its second argument.
    pub isinstance_classes: &'f FxHashMap<VarId, Vec<String>>,
    /// Parameter `i` itself (`None`) or a path through its attributes.
    pub param_alias: &'f FxHashMap<VarId, (u8, Option<String>)>,
    /// One-level slots of parameters (`p.a`, `p["k"]`).
    pub param_slot_alias: &'f FxHashMap<VarId, (u8, Slot)>,
    /// Attributes the function assigns or deletes on any object.
    pub stored_attrs: &'f [&'a str],
    /// Instances the function constructs (copies and agreeing phis included).
    pub fresh: &'f FxHashSet<VarId>,
    /// The block and phi defining each phi variable (built on first use).
    pub phis: &'f OnceCell<FxHashMap<VarId, (BlockId, &'a PhiNode)>>,
    /// Buffers of the backward walks.
    pub scratch: &'f RefCell<(FxHashSet<VarId>, Vec<VarId>)>,
}

/// The guards of one function's own effect sites and call sites, computed on first use.
#[derive(Default)]
pub(crate) struct LocalGuards {
    facts: OnceCell<LocalFacts>,
    regions: OnceCell<Vec<LazyRegion>>,
}

/// The input tests of a function and the ones holding at the entry of each block (they hold
/// throughout it); `entry` is empty when the function tests no input.
#[derive(Default)]
struct LocalFacts {
    tests: Vec<(GuardSubject, GuardTest)>,
    entry: Vec<Vec<(u32, bool)>>,
}

impl LocalGuards {
    fn regions(&self, cx: &GuardInputs<'_, '_>) -> &[LazyRegion] {
        self.regions.get_or_init(|| {
            let f = cx.f;
            let def = |v: &VarId| cx.def(*v);
            let mut values: FxHashMap<VarId, FxHashSet<VarId>> = FxHashMap::default();
            branches(f, &def)
                .into_iter()
                .filter(|b| initializes(f, &def, &mut values, b))
                .map(|b| {
                    let d = derived(cx, b.obj);
                    LazyRegion {
                        test: b.test,
                        obj: b.obj,
                        root: (!d.fresh).then_some(d.params),
                        blocks: dominated(f, b.init),
                    }
                })
                .collect()
        })
    }

    /// The input guards at the entry of `bb`; they hold throughout it, as no fact is killed.
    pub(crate) fn facts_at(&self, cx: &GuardInputs<'_, '_>, bb: BlockId) -> Vec<InputGuard> {
        let facts = self.facts.get_or_init(|| Facts::run(cx));
        let Some(entry) = facts.entry.get(bb.0 as usize) else {
            return Vec::new();
        };
        entry
            .iter()
            .map(|&(i, holds)| {
                let (subject, test) = facts.tests[i as usize].clone();
                InputGuard {
                    subject,
                    test,
                    holds,
                }
            })
            .collect()
    }

    /// The lazy initialization `bb` runs in (`EffectGuards::once_per`), preferring objects that
    /// come from fewer parameters.
    pub(crate) fn once_per_at(&self, cx: &GuardInputs<'_, '_>, bb: BlockId) -> Option<u64> {
        self.regions(cx)
            .iter()
            .filter(|r| r.blocks[bb.0 as usize])
            .filter_map(|r| r.root)
            .min_by_key(|m| (m.count_ones(), *m))
    }

    /// `CallSite::once_loops` for a site in block `bb`, inside `loops`.
    pub(crate) fn once_loops_at(
        &self,
        cx: &GuardInputs<'_, '_>,
        bb: BlockId,
        loops: &[&NaturalLoop],
    ) -> u64 {
        if loops.is_empty() {
            return 0;
        }
        let mut out = 0;
        for r in self.regions(cx).iter().filter(|r| r.blocks[bb.0 as usize]) {
            for lp in loops {
                if lp.body.contains(&r.test) && !fresh_in_loop(cx, r.obj, lp) {
                    out |= loop_bit(lp);
                }
            }
        }
        out
    }

    /// The guards of an effect site in block `bb`.
    pub(crate) fn occurrence(&self, cx: &GuardInputs<'_, '_>, bb: BlockId) -> EffectGuards {
        EffectGuards {
            inputs: self.facts_at(cx, bb).into_iter().collect(),
            once_per: self.once_per_at(cx, bb),
        }
    }

    /// The binding facts of argument `var` at a call inside `loops`.
    pub(crate) fn binding(
        &self,
        cx: &GuardInputs<'_, '_>,
        var: VarId,
        loops: &[&NaturalLoop],
    ) -> ArgBinding {
        let d = derived(cx, var);
        ArgBinding {
            constant: const_arg(cx, var),
            input: cx.subject(var),
            derived_params: d.params,
            derived_fresh: d.fresh,
            stays_in_loops: loops
                .iter()
                .filter(|lp| !d.fresh || !fresh_in_loop(cx, var, lp))
                .fold(0, |m, lp| m | loop_bit(lp)),
        }
    }
}

impl<'a> GuardInputs<'_, 'a> {
    fn def(&self, v: VarId) -> Option<&'a InstKind> {
        self.defs.get(&v).map(|i| &i.kind)
    }

    fn phi(&self, v: VarId) -> Option<(BlockId, &'a PhiNode)> {
        self.phis
            .get_or_init(|| {
                let mut phis = FxHashMap::default();
                for bb in &self.f.blocks {
                    for phi in &bb.phis {
                        phis.insert(phi.dst, (bb.id, phi));
                    }
                }
                phis
            })
            .get(&v)
            .copied()
    }

    /// The guard subject `v` is: a parameter other than `*args` / `**kwargs`, or an attribute
    /// of one that the function never assigns.
    fn subject(&self, v: VarId) -> Option<GuardSubject> {
        let plain = |i: u8| {
            self.f
                .params
                .get(usize::from(i))
                .is_some_and(|p| !p.is_vararg && !p.is_kwarg)
        };
        if let Some((i, None)) = self.param_alias.get(&v) {
            return plain(*i).then_some(GuardSubject {
                param: *i,
                attr: None,
            });
        }
        match self.param_slot_alias.get(&v)? {
            (i, Slot::Attr(a)) if plain(*i) && !self.stored_attrs.contains(&a.as_str()) => {
                Some(GuardSubject {
                    param: *i,
                    attr: Some(a.clone()),
                })
            }
            _ => None,
        }
    }

    /// The position of parameter variable `v` (below 64).
    fn param_index(&self, v: VarId) -> Option<usize> {
        self.f.params.iter().take(64).position(|p| p.var == v)
    }
}

/// The bit of loop `lp` in a mask of the loops around a site (`CallSite::once_loops`).
pub fn loop_bit(lp: &NaturalLoop) -> u64 {
    1u64 << lp.depth.saturating_sub(1).min(63)
}

/// Whether `var` may come from an object constructed inside `lp` (a constructor call there).
fn fresh_in_loop(cx: &GuardInputs<'_, '_>, var: VarId, lp: &NaturalLoop) -> bool {
    let mut inside: FxHashSet<VarId> = FxHashSet::default();
    for b in &lp.body {
        let block = cx.f.block(*b);
        inside.extend(block.phis.iter().map(|p| p.dst));
        inside.extend(block.insts.iter().filter_map(|i| i.kind.dst()));
    }
    let mut seen = FxHashSet::default();
    let mut stack = vec![var];
    while let Some(v) = stack.pop() {
        if !inside.contains(&v) || !seen.insert(v) {
            continue;
        }
        if let Some((_, phi)) = cx.phi(v) {
            stack.extend(phi.incoming.iter().map(|(_, w)| *w));
            continue;
        }
        let def = &cx.defs[&v].kind;
        if cx.fresh.contains(&v) && matches!(def, InstKind::Call { .. }) {
            return true;
        }
        for_each_source(def, |w| stack.push(w));
    }
    false
}

/// [`Derived`] of `var`: the parameters and constructed instances among the values it may
/// come from (a union over what reaches it backwards, so no fixpoint is needed).
fn derived(cx: &GuardInputs<'_, '_>, var: VarId) -> Derived {
    let mut out = Derived::default();
    if let Some(i) = cx.param_index(var) {
        out.params = 1u64 << i;
        out.fresh = cx.fresh.contains(&var);
        return out;
    }
    let mut scratch = cx.scratch.borrow_mut();
    let (seen, stack) = &mut *scratch;
    seen.clear();
    stack.clear();
    stack.push(var);
    while let Some(v) = stack.pop() {
        if !seen.insert(v) {
            continue;
        }
        if let Some(i) = cx.param_index(v) {
            out.params |= 1u64 << i;
        }
        out.fresh |= cx.fresh.contains(&v);
        match cx.def(v) {
            Some(def) => {
                for_each_source(def, |w| stack.push(w));
            }
            None => {
                if let Some((_, phi)) = cx.phi(v) {
                    stack.extend(phi.incoming.iter().map(|(_, w)| *w));
                }
            }
        }
    }
    out
}

/// Every path from `b.init` to a normal return, or back to the test, assigns `b.obj.attr`
/// (stores in a block end the path; exceptions leave without a return).
fn initializes(
    f: &IrFunction,
    def: DefOf<'_, '_>,
    values: &mut FxHashMap<VarId, FxHashSet<VarId>>,
    b: &LazyInitBranch,
) -> bool {
    // The same object: an alias, or the same attribute of an alias of the same base
    // (`self.conn._sock` tested and assigned through two loads of `self.conn`).
    let base_of = |v: &VarId| match def(v) {
        Some(InstKind::AttrLoad { obj, attr, .. }) => Some((*obj, attr.as_str())),
        _ => None,
    };
    let tested_base = base_of(&b.obj);
    for v in [Some(b.obj), tested_base.map(|(o, _)| o)]
        .into_iter()
        .flatten()
    {
        values.entry(v).or_insert_with(|| same_values(f, v));
    }
    let aliases = &values[&b.obj];
    let same = |v: &VarId| {
        aliases.contains(v)
            || matches!(
                (tested_base, base_of(v)),
                (Some((base, attr)), Some((o, a))) if attr == a && values[&base].contains(&o)
            )
    };
    let mut seen = FxHashSet::default();
    let mut stack = vec![b.init];
    while let Some(cur) = stack.pop() {
        if !seen.insert(cur) {
            continue;
        }
        if cur == b.test {
            return false;
        }
        let block = f.block(cur);
        let stores = block.insts.iter().any(|i| {
            matches!(&i.kind, InstKind::AttrStore { obj, attr, .. } if *attr == b.attr && same(obj))
        });
        if stores {
            continue;
        }
        if matches!(block.terminator, Terminator::Return(_)) {
            return false;
        }
        stack.extend(block.terminator.successors());
        stack.extend(block.exc_target);
    }
    true
}

/// `root`, its copies and phis, and the builder's re-reads of its variable in blocks only an
/// exception reaches (`LoadName` of the same name, as after a `finally` in a handler).
fn same_values(f: &IrFunction, root: VarId) -> FxHashSet<VarId> {
    let name = f.var_name(root);
    let reloads = f
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .filter_map(|i| match &i.kind {
            InstKind::LoadName { dst, name: n } if Some(n.as_str()) == name => Some(*dst),
            _ => None,
        });
    aliases_of(f, std::iter::once(root).chain(reloads))
}

/// Membership (by block id) of the blocks every path from the entry to which runs through `d`
/// (normal and exception edges).
fn dominated(f: &IrFunction, d: BlockId) -> Vec<bool> {
    let reach = |start: BlockId, avoid: Option<BlockId>| {
        let mut seen = vec![false; f.blocks.len()];
        let mut stack = vec![start];
        while let Some(cur) = stack.pop() {
            if Some(cur) == avoid || std::mem::replace(&mut seen[cur.0 as usize], true) {
                continue;
            }
            let block = f.block(cur);
            stack.extend(block.terminator.successors());
            stack.extend(block.exc_target);
        }
        seen
    };
    let around = reach(BlockId(0), Some(d));
    let through = reach(d, None);
    through
        .iter()
        .zip(&around)
        .map(|(t, a)| *t && !*a)
        .collect()
}

/// The constant (through copies) or display `var` is. A list, set, or dict display counts only
/// as the argument alone, so nothing can have filled or emptied it.
fn const_arg(cx: &GuardInputs<'_, '_>, var: VarId) -> Option<ConstArg> {
    let mut v = var;
    while let Some(InstKind::Copy { src, .. }) = cx.def(v) {
        v = *src;
    }
    if let Some(InstKind::Const { literal, .. }) = cx.def(v) {
        return ConstArg::of_literal(literal);
    }
    let alone = || cx.uses.get(&var).is_some_and(|u| u.len() == 1);
    match cx.def(var)? {
        InstKind::AllocBuiltin {
            kind: BuiltinKind::Tuple,
            elems,
            ..
        } => Some(ConstArg {
            class: ConstClass::Tuple,
            truthy: Some(!elems.is_empty()),
        }),
        InstKind::AllocBuiltin { kind, elems, .. } if alone() => {
            let class = match kind {
                BuiltinKind::List => ConstClass::List,
                BuiltinKind::Set => ConstClass::Set,
                _ => return None,
            };
            Some(ConstArg {
                class,
                truthy: Some(!elems.is_empty()),
            })
        }
        InstKind::DictLiteral { entries, .. } if alone() => Some(ConstArg {
            class: ConstClass::Dict,
            truthy: entries
                .iter()
                .any(|e| matches!(e, DictEntry::KeyValue { .. }))
                .then_some(true)
                .or(entries.is_empty().then_some(false)),
        }),
        _ => None,
    }
}

// ------------------------------------------------------------------ the input facts dataflow

fn is_none(cx: &GuardInputs<'_, '_>, v: &VarId) -> bool {
    matches!(
        cx.def(*v),
        Some(InstKind::Const {
            literal: ConstLiteral::None,
            ..
        })
    )
}

/// Facts as a bit set: bit `2 * a` says atom `a` holds, bit `2 * a + 1` that it fails. Atoms
/// are the input tests (`Facts::tests`, first) and the values that tell which edge into an `and`
/// / `or` merge was taken (`Facts::merged`); atoms past [`ATOMS`] are not tracked (fewer facts,
/// never wrong ones), and the last pair marks an impossible state.
type FactSet = u64;

/// How many atoms are tracked; the next pair of bits is [`IMPOSSIBLE`].
const ATOMS: u32 = 31;
/// An infeasible state: both bits of the reserved last atom.
const IMPOSSIBLE: FactSet = 0b11 << (2 * ATOMS);
const HOLDS: FactSet = 0x5555_5555_5555_5555;

/// The fact that atom `a` has truthiness `holds` (nothing for an untracked atom).
fn fact(a: u32, holds: bool) -> FactSet {
    let bit = 2 * a.min(ATOMS) + u32::from(!holds);
    FactSet::from(a < ATOMS) << bit
}

fn contradictory(facts: FactSet) -> bool {
    facts & HOLDS & (facts >> 1) != 0
}

/// What holds on the edges out of one block, by target (`None`: the edge is infeasible).
type OutEdges = Vec<(BlockId, Option<FactSet>)>;

/// A forward must-analysis of the conditions that hold at each block: branch edges add what
/// their condition says (through copies, `not`, and the phis of `and` / `or` / conditional
/// expressions), merges keep what holds on every edge, infeasible edges drop out.
struct Facts<'c, 'f, 'a> {
    cx: &'c GuardInputs<'f, 'a>,
    /// The input tests, interned (their atoms).
    tests: Vec<(GuardSubject, GuardTest)>,
    /// The input test a value is, and the outcome its truthiness means.
    test_of: FxHashMap<VarId, (u32, bool)>,
    /// The values whose truthiness tells which edge into an `and` / `or` merge was taken, with
    /// their atoms (after the tests).
    merged: FxHashMap<VarId, u32>,
    /// What holds on the edges out of each block, by target, once the block is done; `None`:
    /// the edge is infeasible.
    edges: Vec<Option<OutEdges>>,
}

impl<'c, 'f, 'a> Facts<'c, 'f, 'a> {
    /// The input tests of the function and the ones at the entry of every block.
    fn run(cx: &'c GuardInputs<'f, 'a>) -> LocalFacts {
        let mut facts = Self {
            cx,
            tests: Vec::new(),
            test_of: FxHashMap::default(),
            merged: FxHashMap::default(),
            edges: Vec::new(),
        };
        facts.find_tests();
        if facts.tests.is_empty() {
            return LocalFacts::default();
        }
        facts.edges = vec![None; cx.f.blocks.len()];
        let tests = u32::try_from(facts.tests.len())
            .unwrap_or(u32::MAX)
            .min(ATOMS);
        let entry = facts
            .solve()
            .into_iter()
            .map(|set| {
                let set = set.unwrap_or(0);
                (0..tests)
                    .flat_map(|i| [(i, true), (i, false)])
                    .filter(|&(i, holds)| set & fact(i, holds) != 0)
                    .collect()
            })
            .collect();
        LocalFacts {
            tests: facts.tests,
            entry,
        }
    }

    /// Intern the input tests among the values branch conditions are made of.
    fn find_tests(&mut self) {
        let cx = self.cx;
        let mut stack: Vec<VarId> =
            cx.f.blocks
                .iter()
                .filter_map(|b| match b.terminator {
                    Terminator::Branch { cond, .. } => Some(cond),
                    _ => None,
                })
                .collect();
        let mut seen = FxHashSet::default();
        let mut merged = Vec::new();
        while let Some(v) = stack.pop() {
            if !seen.insert(v) {
                continue;
            }
            if let Some((subject, test, polarity)) = self.test(v) {
                let key = (subject, test);
                let i = match self.tests.iter().position(|t| *t == key) {
                    Some(i) => i,
                    None => {
                        self.tests.push(key);
                        self.tests.len() - 1
                    }
                };
                self.test_of
                    .insert(v, (u32::try_from(i).unwrap_or(u32::MAX), polarity));
            }
            match cx.def(v) {
                Some(InstKind::Copy { src, .. })
                | Some(InstKind::UnaryOp {
                    op: UnaryOpKind::Not,
                    operand: src,
                    ..
                }) => stack.push(*src),
                _ => {
                    if let Some((_, phi)) = self.cx.phi(v) {
                        for (_, w) in &phi.incoming {
                            merged.push(*w);
                            stack.push(*w);
                        }
                    }
                }
            }
        }
        let first = u32::try_from(self.tests.len()).unwrap_or(u32::MAX);
        for w in merged {
            let next = first.saturating_add(u32::try_from(self.merged.len()).unwrap_or(u32::MAX));
            self.merged.entry(w).or_insert(next);
        }
    }

    /// The input test `v` is: a subject itself (truthiness), `isinstance(subject, C)`, or
    /// `subject is None` / `is not None` (`==` / `!=`); with the outcome its truthiness means.
    fn test(&self, v: VarId) -> Option<(GuardSubject, GuardTest, bool)> {
        let cx = self.cx;
        if let Some(s) = cx.subject(v) {
            return Some((s, GuardTest::Truthy, true));
        }
        let is_none = |v: &VarId| is_none(cx, v);
        match cx.def(v)? {
            InstKind::Call {
                callee_var: Some(c),
                args,
                kwargs,
                ..
            } if kwargs.is_empty()
                && args.len() == 2
                && matches!(cx.types.get(c), Some(Val::Ref(n)) if n == "builtins.isinstance") =>
            {
                let subject = cx.subject(args[0])?;
                let classes = cx.isinstance_classes.get(&args[1])?.clone();
                Some((subject, GuardTest::IsInstance(classes), true))
            }
            InstKind::Compare { op, lhs, rhs, .. } => {
                let holds = match op {
                    CompareOpKind::Is | CompareOpKind::Eq => true,
                    CompareOpKind::IsNot | CompareOpKind::NotEq => false,
                    _ => return None,
                };
                let side = if is_none(rhs) {
                    lhs
                } else if is_none(lhs) {
                    rhs
                } else {
                    return None;
                };
                let subject = cx.subject(*side)?;
                Some((subject, GuardTest::IsNone, holds))
            }
            _ => None,
        }
    }

    /// What `v` having truthiness `holds` says.
    fn decompose(&self, v: VarId, holds: bool, stack: &mut Vec<VarId>) -> FactSet {
        let mut out: FactSet = 0;
        if let Some(&a) = self.merged.get(&v) {
            out |= fact(a, holds);
        }
        if let Some(&(t, polarity)) = self.test_of.get(&v) {
            out |= fact(t, holds == polarity);
        }
        if stack.contains(&v) {
            return out;
        }
        stack.push(v);
        match self.cx.def(v) {
            Some(InstKind::Copy { src, .. }) => out |= self.decompose(*src, holds, stack),
            Some(InstKind::UnaryOp {
                op: UnaryOpKind::Not,
                operand,
                ..
            }) => out |= self.decompose(*operand, !holds, stack),
            _ => {
                // The phi has the value of the edge taken: what holds on every feasible edge
                // whose incoming value has this truthiness. A loop phi whose back edge is not
                // done yet says nothing more.
                if let Some((bb, phi)) = self.cx.phi(v)
                    && let Some(edges) = phi
                        .incoming
                        .iter()
                        .map(|(pred, _)| self.edge(*pred, bb))
                        .collect::<Option<Vec<_>>>()
                {
                    let mut all: Option<FactSet> = None;
                    for ((_, w), edge) in phi.incoming.iter().zip(edges) {
                        let Some(edge) = edge else {
                            continue;
                        };
                        let e = edge | self.decompose(*w, holds, stack);
                        if contradictory(e) {
                            continue;
                        }
                        all = Some(all.map_or(e, |a| a & e));
                    }
                    // No edge gives this truthiness: impossible.
                    out |= all.unwrap_or(IMPOSSIBLE);
                }
            }
        }
        stack.pop();
        out
    }

    /// What holds on the edge `from -> to`: `None` while `from` is not done, then `Some(None)`
    /// for an infeasible edge.
    fn edge(&self, from: BlockId, to: BlockId) -> Option<Option<FactSet>> {
        let out = self.edges[from.0 as usize].as_ref()?;
        Some(out.iter().find(|(t, _)| *t == to).and_then(|(_, e)| *e))
    }

    /// The facts at the entry of every block (`None`: not reached), in one pass in reverse
    /// postorder. Facts are never killed, so an edge back to a loop header carries at least
    /// what holds at the header: the merges may ignore the edges not done yet.
    fn solve(&mut self) -> Vec<Option<FactSet>> {
        let f = self.cx.f;
        let mut entry: Vec<Option<FactSet>> = vec![None; f.blocks.len()];
        for (i, b) in f.rpo_blocks().into_iter().enumerate() {
            if i == 0 {
                entry[b.0 as usize] = Some(0);
            }
            let Some(cur) = entry[b.0 as usize] else {
                continue;
            };
            let block = f.block(b);
            let mut out: OutEdges = Vec::new();
            match &block.terminator {
                Terminator::Jump(t) => out.push((*t, Some(cur))),
                Terminator::Branch {
                    cond,
                    then_bb,
                    else_bb,
                } => {
                    for (t, holds) in [(*then_bb, true), (*else_bb, false)] {
                        let e = cur | self.decompose(*cond, holds, &mut Vec::new());
                        out.push((t, (!contradictory(e)).then_some(e)));
                    }
                }
                _ => {}
            }
            if let Some(x) = block.exc_target {
                // An exception can leave before the branch: only what held on entry.
                out.retain(|(t, _)| *t != x);
                out.push((x, Some(cur)));
            }
            for (t, e) in &out {
                if let Some(e) = e {
                    let slot = &mut entry[t.0 as usize];
                    *slot = Some(slot.map_or(*e, |acc| acc & e));
                }
            }
            self.edges[b.0 as usize] = Some(out);
        }
        entry
    }
}
