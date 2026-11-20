//! Guards on execution effects (docs/ANALYSIS_AND_KB_SPEC.md section 2.2): what makes an
//! effect not happen although the code reaching it runs.
//!
//! - **Input guards**: tests of the function's own inputs that dominate an effect, `if cache:`,
//!   `if x is None:`, `isinstance(x, T)`, `hasattr(x, "write")` (also inside `and` / `or` /
//!   `not`, early returns, and conditional expressions), on a parameter or on an attribute of
//!   one that the function never assigns. A test of a value a phi merges (`buf = StringIO()`
//!   when `buf is None`, then `isinstance(buf, str)`) is read along each edge into the phi:
//!   the value that edge brings decides it, or is an input whose test it is, and the edge's
//!   own facts come with it. A call whose argument decides such a test against the effect (a
//!   constant, a value whose exact class the caller knows, the literal default of an omitted
//!   parameter, a test the caller has already made on the same input) does not reach the
//!   effect; an undecided test on a caller input becomes the caller's guard.
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
            GuardTest::Truthy => self.truthy.or_else(|| self.class.decide(test)),
            _ => self.class.decide(test),
        }
    }
}

/// Classes besides the builtin types whose relation to every [`ConstClass`] is fixed: an
/// instance of one of those is an instance of one of these exactly when its MRO names it.
/// `os.PathLike` is implemented by none of them, and the `io` ABCs register only the `io`
/// implementations.
const DECIDED_CLASSES: &[&str] = &[
    "io.BufferedIOBase",
    "io.BufferedRWPair",
    "io.BufferedRandom",
    "io.BufferedReader",
    "io.BufferedWriter",
    "io.BytesIO",
    "io.FileIO",
    "io.IOBase",
    "io.RawIOBase",
    "io.StringIO",
    "io.TextIOBase",
    "io.TextIOWrapper",
    "os.PathLike",
];

/// The attributes of the file protocol, for which `hasattr` on a value of a [`ConstClass`] is
/// decided: the buffers have every one but `__fspath__`, the builtin values none but
/// `__iter__` of the strings and containers, in every Python version.
const FILE_PROTOCOL: &[&str] = &[
    "__enter__",
    "__exit__",
    "__fspath__",
    "__iter__",
    "__next__",
    "close",
    "fileno",
    "flush",
    "getvalue",
    "isatty",
    "read",
    "readable",
    "readline",
    "readlines",
    "seek",
    "seekable",
    "tell",
    "truncate",
    "writable",
    "write",
    "writelines",
];

impl ConstClass {
    /// The class of a display or comprehension of `kind` (an f-string, `Str`, has none: a single
    /// replacement field is the result of `__format__`, which may be a `str` subclass).
    fn of_builtin(kind: BuiltinKind) -> Option<Self> {
        match kind {
            BuiltinKind::List => Some(Self::List),
            BuiltinKind::Tuple => Some(Self::Tuple),
            BuiltinKind::Set => Some(Self::Set),
            BuiltinKind::Dict => Some(Self::Dict),
            _ => None,
        }
    }

    /// The buffer class a call of `module.name` constructs.
    fn of_buffer(module: &str, name: &str) -> Option<Self> {
        match (module, name) {
            ("io" | "_io", "StringIO") => Some(Self::StringIo),
            ("io" | "_io", "BytesIO") => Some(Self::BytesIo),
            _ => None,
        }
    }

    /// The outcome of `test` on every value of this class: truthiness only for the buffers,
    /// which define neither `__bool__` nor `__len__`.
    fn decide(self, test: &GuardTest) -> Option<bool> {
        match test {
            GuardTest::Truthy => matches!(self, Self::StringIo | Self::BytesIo).then_some(true),
            GuardTest::IsNone => Some(self == Self::None),
            GuardTest::IsInstance(classes) => self.is_instance(classes),
            GuardTest::HasAttr(name) => self.has_attr(name),
        }
    }

    /// The classes an instance of this class is an instance of, among the builtin types and
    /// [`DECIDED_CLASSES`].
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
            Self::StringIo => &[
                "io.StringIO",
                "io.TextIOBase",
                "io.IOBase",
                "builtins.object",
            ],
            Self::BytesIo => &[
                "io.BytesIO",
                "io.BufferedIOBase",
                "io.IOBase",
                "builtins.object",
            ],
            Self::Other => &[],
        }
    }

    /// `isinstance(value, classes)`: true when a class is in the value's MRO, false when every
    /// class is a builtin type or one of [`DECIDED_CLASSES`] outside it; otherwise unknown.
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
                DECIDED_CLASSES.contains(&c.as_str())
                    || c.strip_prefix("builtins.")
                        .is_some_and(|b| BuiltinKind::from_name(b).is_some())
            })
            .then_some(false)
    }

    /// `hasattr(value, name)` for a [`FILE_PROTOCOL`] name. A buffer can gain attributes of its
    /// own (it has a `__dict__`), so only the ones its class has are decided; a builtin value
    /// has only its class's.
    fn has_attr(self, name: &str) -> Option<bool> {
        if !FILE_PROTOCOL.contains(&name) {
            return None;
        }
        match self {
            Self::StringIo | Self::BytesIo => (name != "__fspath__").then_some(true),
            Self::Str | Self::Bytes | Self::List | Self::Tuple | Self::Set | Self::Dict => {
                Some(name == "__iter__")
            }
            Self::None | Self::Bool | Self::Int | Self::Float => Some(false),
            Self::Other => None,
        }
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
    classes: OnceCell<Classes>,
}

/// The input tests of a function and the ones holding at the entry of each block (they hold
/// throughout it); `entry` is empty when the function tests no input.
#[derive(Default)]
struct LocalFacts {
    tests: Vec<(GuardSubject, GuardTest)>,
    entry: Vec<Vec<(u32, bool)>>,
}

impl LocalGuards {
    fn classes(&self, cx: &GuardInputs<'_, '_>) -> &Classes {
        self.classes.get_or_init(|| known_classes(cx))
    }

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
        let facts = self.facts.get_or_init(|| Facts::run(cx, self.classes(cx)));
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
            constant: const_arg(cx, self.classes(cx), var),
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

    /// The string literal `v` is.
    fn const_str(&self, v: VarId) -> Option<String> {
        match self.def(v)? {
            InstKind::Const {
                literal: ConstLiteral::Str(s),
                ..
            } => Some(s.clone()),
            _ => None,
        }
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

/// What the caller knows of the argument `var`: the constant it is (through copies), or its
/// exact class among `classes`. A list, set, or dict display has a truthiness only as the
/// argument alone, so nothing can have filled or emptied it.
fn const_arg(cx: &GuardInputs<'_, '_>, classes: &Classes, var: VarId) -> Option<ConstArg> {
    let mut v = var;
    while let Some(InstKind::Copy { src, .. }) = cx.def(v) {
        v = *src;
    }
    if let Some(InstKind::Const { literal, .. }) = cx.def(v) {
        return ConstArg::of_literal(literal);
    }
    let class = *classes.get(&var)?;
    let alone = || cx.uses.get(&var).is_some_and(|u| u.len() == 1);
    let truthy = match cx.def(var) {
        Some(InstKind::AllocBuiltin {
            kind: BuiltinKind::Tuple,
            elems,
            ..
        }) => Some(!elems.is_empty()),
        Some(InstKind::AllocBuiltin { elems, .. }) if alone() => Some(!elems.is_empty()),
        Some(InstKind::DictLiteral { entries, .. }) if alone() => entries
            .iter()
            .any(|e| matches!(e, DictEntry::KeyValue { .. }))
            .then_some(true)
            .or(entries.is_empty().then_some(false)),
        _ => None,
    };
    Some(ConstArg { class, truthy })
}

/// The exact classes of a function's values ([`known_classes`]).
type Classes = FxHashMap<VarId, ConstClass>;

/// What [`known_classes`] knows of a value so far.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ClassState {
    /// No value reaches it yet (a phi or copy not reached by the fixpoint).
    Unset,
    Exact(ConstClass),
    /// Values of different or unknown classes reach it.
    Mixed,
}

impl ClassState {
    fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unset, s) | (s, Self::Unset) => s,
            (Self::Exact(a), Self::Exact(b)) if a == b => self,
            _ => Self::Mixed,
        }
    }
}

/// The passes [`known_classes`] takes at most; past them only the definitions' own classes are
/// known, none of a copy or phi.
const CLASS_PASSES: usize = 8;

/// The exact class of every value of the function that has one: the class every value it may be
/// (through copies and phis) has, each a constant, a list, tuple, set, or dict display or
/// comprehension, or a buffer the function constructs ([`ConstClass::of_buffer`]). A monotone
/// fixpoint over copies and phis in reverse postorder: a value only goes from unset to a class
/// to mixed.
fn known_classes(cx: &GuardInputs<'_, '_>) -> Classes {
    let f = cx.f;
    let mut state: FxHashMap<VarId, ClassState> = FxHashMap::default();
    let mut merges: Vec<(VarId, Vec<VarId>)> = Vec::new();
    let order = f.rpo_blocks();
    for b in &order {
        let block = f.block(*b);
        for phi in &block.phis {
            let inputs = phi.incoming.iter().map(|(_, w)| *w);
            merges.push((phi.dst, inputs.filter(|w| *w != phi.dst).collect()));
        }
        for inst in &block.insts {
            match &inst.kind {
                InstKind::Copy { dst, src } => merges.push((*dst, vec![*src])),
                kind => {
                    if let Some(dst) = kind.dst() {
                        let class = leaf_class(cx, dst, kind);
                        state.insert(dst, class.map_or(ClassState::Mixed, ClassState::Exact));
                    }
                }
            }
        }
    }
    for (dst, _) in &merges {
        state.insert(*dst, ClassState::Unset);
    }
    // A value neither a merge nor defined in a reached block (a parameter) is of unknown class.
    let of = |state: &FxHashMap<VarId, ClassState>, v: &VarId| {
        state.get(v).copied().unwrap_or(ClassState::Mixed)
    };
    let mut settled = false;
    for _ in 0..CLASS_PASSES {
        let mut changed = false;
        for (dst, inputs) in &merges {
            let joined = inputs
                .iter()
                .fold(ClassState::Unset, |acc, w| acc.join(of(&state, w)));
            let old = of(&state, dst);
            let new = old.join(joined);
            if new != old {
                state.insert(*dst, new);
                changed = true;
            }
        }
        if !changed {
            settled = true;
            break;
        }
    }
    if !settled {
        for (dst, _) in &merges {
            state.insert(*dst, ClassState::Mixed);
        }
    }
    state
        .into_iter()
        .filter_map(|(v, s)| match s {
            ClassState::Exact(c) => Some((v, c)),
            ClassState::Unset | ClassState::Mixed => None,
        })
        .collect()
}

/// The class of the value `kind` defines (`v`), when it is exact.
fn leaf_class(cx: &GuardInputs<'_, '_>, v: VarId, kind: &InstKind) -> Option<ConstClass> {
    match kind {
        InstKind::Const { literal, .. } => Some(ConstArg::of_literal(literal)?.class),
        InstKind::AllocBuiltin { kind, .. } => ConstClass::of_builtin(*kind),
        InstKind::DictLiteral { .. } => Some(ConstClass::Dict),
        kind => constructed_class(cx, v, kind),
    }
}

/// The class of the object the call `kind` (defining `v`) constructs, when it is exact: an
/// `io.StringIO` / `io.BytesIO` buffer, or the container a comprehension frame builds and
/// returns (Python 3.11, `__comp_<id>(...)`, the only call without a callee value).
fn constructed_class(cx: &GuardInputs<'_, '_>, v: VarId, kind: &InstKind) -> Option<ConstClass> {
    if let InstKind::Call {
        callee_var: Some(c),
        ..
    } = kind
        && let Some(Val::Ref(fqn)) = cx.types.get(c)
        && let Some((module, name)) = fqn.rsplit_once('.')
    {
        return ConstClass::of_buffer(module, name);
    }
    if let InstKind::MethodCall { recv, method, .. } = kind
        && let Some(Val::Ref(module)) = cx.types.get(recv)
    {
        return ConstClass::of_buffer(module, method);
    }
    if let InstKind::Call {
        callee_var: None, ..
    } = kind
        && let Some(Val::Inst(class)) = cx.types.get(&v)
    {
        let kind = BuiltinKind::from_name(class.strip_prefix("builtins.")?)?;
        return ConstClass::of_builtin(kind);
    }
    None
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

/// The variables a [`Facts::decompose`] is inside, and what it found for each variable and
/// truthiness whose result does not depend on the path to it (while one branch is decided).
#[derive(Default)]
struct Decomposing {
    stack: Vec<VarId>,
    done: FxHashMap<(VarId, bool), FactSet>,
}

/// A test of a value a phi merges (`isinstance(buf, str)` after `buf = StringIO()` on one
/// branch only): the value along each edge into the phi decides it, or is an input whose test
/// is an atom, or tells nothing.
struct PhiTest {
    /// The block of the phi.
    block: BlockId,
    /// The outcome of the test its truthiness means.
    polarity: bool,
    /// What the test is along the edge from each predecessor of the phi's block.
    edges: Vec<(BlockId, EdgeOutcome)>,
}

/// [`PhiTest::edges`]: what the test of the value one edge brings into the phi is.
#[derive(Clone, Copy)]
enum EdgeOutcome {
    /// The value's class decides it.
    Decided(bool),
    /// The value is an input, and this atom is its test.
    Atom(u32),
    Unknown,
}

/// A forward must-analysis of the conditions that hold at each block: branch edges add what
/// their condition says (through copies, `not`, the phis of `and` / `or` / conditional
/// expressions, and the edges into the phi of a tested value), merges keep what holds on every
/// edge, infeasible edges drop out.
struct Facts<'c, 'f, 'a> {
    cx: &'c GuardInputs<'f, 'a>,
    /// The exact classes of the function's values.
    classes: &'c Classes,
    /// The input tests, interned (their atoms).
    tests: Vec<(GuardSubject, GuardTest)>,
    /// The input test a value is, and the outcome its truthiness means.
    test_of: FxHashMap<VarId, (u32, bool)>,
    /// The tests of values a phi merges, by the value that is the test.
    phi_tests: FxHashMap<VarId, PhiTest>,
    /// The values whose truthiness tells which edge into an `and` / `or` merge was taken, with
    /// their atoms (after the tests).
    merged: FxHashMap<VarId, u32>,
    /// What holds on the edges out of each block, by target, once the block is done; `None`:
    /// the edge is infeasible.
    edges: Vec<Option<OutEdges>>,
}

impl<'c, 'f, 'a> Facts<'c, 'f, 'a> {
    /// The input tests of the function and the ones at the entry of every block.
    fn run(cx: &'c GuardInputs<'f, 'a>, classes: &'c Classes) -> LocalFacts {
        let mut facts = Self {
            cx,
            classes,
            tests: Vec::new(),
            test_of: FxHashMap::default(),
            phi_tests: FxHashMap::default(),
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
                let atom = self.intern(subject, test);
                self.test_of.insert(v, (atom, polarity));
            } else if let Some(test) = self.phi_test(v) {
                self.phi_tests.insert(v, test);
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

    /// The atom of the input test `(subject, test)`.
    fn intern(&mut self, subject: GuardSubject, test: GuardTest) -> u32 {
        let key = (subject, test);
        let i = match self.tests.iter().position(|t| *t == key) {
            Some(i) => i,
            None => {
                self.tests.push(key);
                self.tests.len() - 1
            }
        };
        u32::try_from(i).unwrap_or(u32::MAX)
    }

    /// The input test `v` is: a subject itself (truthiness), or a [`Self::tested`] test of one;
    /// with the outcome its truthiness means.
    fn test(&self, v: VarId) -> Option<(GuardSubject, GuardTest, bool)> {
        let cx = self.cx;
        if let Some(s) = cx.subject(v) {
            return Some((s, GuardTest::Truthy, true));
        }
        let (value, test, polarity) = self.tested(v)?;
        Some((cx.subject(value)?, test, polarity))
    }

    /// The test `v` makes of a value: `isinstance(x, C)`, `hasattr(x, "name")`, or `x is None`
    /// / `is not None` (`==` / `!=`); with the outcome its truthiness means.
    fn tested(&self, v: VarId) -> Option<(VarId, GuardTest, bool)> {
        let cx = self.cx;
        let is_none = |v: &VarId| is_none(cx, v);
        match cx.def(v)? {
            InstKind::Call {
                callee_var: Some(c),
                args,
                kwargs,
                ..
            } if kwargs.is_empty() && args.len() == 2 => {
                let test = match cx.types.get(c) {
                    Some(Val::Ref(n)) if n == "builtins.isinstance" => {
                        GuardTest::IsInstance(cx.isinstance_classes.get(&args[1])?.clone())
                    }
                    Some(Val::Ref(n)) if n == "builtins.hasattr" => {
                        GuardTest::HasAttr(cx.const_str(args[1])?)
                    }
                    _ => return None,
                };
                Some((args[0], test, true))
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
                Some((*side, GuardTest::IsNone, holds))
            }
            _ => None,
        }
    }

    /// The [`PhiTest`] `v` is: a [`Self::tested`] test of a phi (through copies).
    fn phi_test(&mut self, v: VarId) -> Option<PhiTest> {
        let cx = self.cx;
        let (mut value, test, polarity) = self.tested(v)?;
        while let Some(InstKind::Copy { src, .. }) = cx.def(value) {
            value = *src;
        }
        let (block, phi) = cx.phi(value)?;
        let mut edges = Vec::with_capacity(phi.incoming.len());
        for (pred, w) in &phi.incoming {
            let along = match cx.subject(*w) {
                Some(s) => EdgeOutcome::Atom(self.intern(s, test.clone())),
                None => match self.classes.get(w).and_then(|c| c.decide(&test)) {
                    Some(outcome) => EdgeOutcome::Decided(outcome),
                    None => EdgeOutcome::Unknown,
                },
            };
            edges.push((*pred, along));
        }
        Some(PhiTest {
            block,
            polarity,
            edges,
        })
    }

    /// What `test` having outcome `outcome` says: what holds on every feasible edge into its
    /// phi along which the value can have that outcome. The phi's value is the one of the edge
    /// taken, and what held on that edge still holds, as no fact is killed. Nothing while an
    /// edge is not done (a loop's back edge).
    fn along_phi_edges(&self, test: &PhiTest, outcome: bool) -> FactSet {
        let Some(edges) = test
            .edges
            .iter()
            .map(|(pred, _)| self.edge(*pred, test.block))
            .collect::<Option<Vec<_>>>()
        else {
            return 0;
        };
        let mut all: Option<FactSet> = None;
        for (edge, (_, along)) in edges.into_iter().zip(&test.edges) {
            // An infeasible edge brings no value.
            let e = edge.unwrap_or(IMPOSSIBLE)
                | match *along {
                    EdgeOutcome::Decided(o) if o != outcome => IMPOSSIBLE,
                    EdgeOutcome::Atom(a) => fact(a, outcome),
                    EdgeOutcome::Decided(_) | EdgeOutcome::Unknown => 0,
                };
            if contradictory(e) {
                continue;
            }
            all = Some(all.map_or(e, |a| a & e));
        }
        all.unwrap_or(IMPOSSIBLE)
    }

    /// What `v` having truthiness `holds` says.
    fn decompose(&self, v: VarId, holds: bool, walk: &mut Decomposing) -> FactSet {
        self.decompose_at(v, holds, walk).0
    }

    /// [`Self::decompose`], with the lowest position in the walk's stack where it cut a cycle:
    /// a result that cut none below `v` is the same on every path to `v`, and is kept (a
    /// condition reaches the same phis through many paths: `c = c or not c`).
    fn decompose_at(&self, v: VarId, holds: bool, walk: &mut Decomposing) -> (FactSet, usize) {
        let mut out: FactSet = 0;
        if let Some(&a) = self.merged.get(&v) {
            out |= fact(a, holds);
        }
        if let Some(&(t, polarity)) = self.test_of.get(&v) {
            out |= fact(t, holds == polarity);
        }
        if let Some(at) = walk.stack.iter().position(|w| *w == v) {
            return (out, at);
        }
        if let Some(&known) = walk.done.get(&(v, holds)) {
            return (known, usize::MAX);
        }
        let depth = walk.stack.len();
        walk.stack.push(v);
        let mut low = usize::MAX;
        let mut sub = |w: VarId, h: bool, walk: &mut Decomposing| {
            let (found, at) = self.decompose_at(w, h, walk);
            low = low.min(at);
            found
        };
        match self.cx.def(v) {
            Some(InstKind::Copy { src, .. }) => out |= sub(*src, holds, walk),
            Some(InstKind::UnaryOp {
                op: UnaryOpKind::Not,
                operand,
                ..
            }) => out |= sub(*operand, !holds, walk),
            Some(_) if self.phi_tests.contains_key(&v) => {
                let test = &self.phi_tests[&v];
                out |= self.along_phi_edges(test, holds == test.polarity);
            }
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
                        let e = edge | sub(*w, holds, walk);
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
        walk.stack.pop();
        if low >= depth {
            walk.done.insert((v, holds), out);
            low = usize::MAX;
        }
        (out, low)
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
                    let mut walk = Decomposing::default();
                    for (t, holds) in [(*then_bb, true), (*else_bb, false)] {
                        let e = cur | self.decompose(*cond, holds, &mut walk);
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
