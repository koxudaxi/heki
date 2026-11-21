//! Declarative three-valued Proof Obligations (docs/ANALYSIS_AND_KB_SPEC.md section 3.3).
//!
//! Each obligation is `Established`, `Blocked` (not provable here), or `Disproved` (a concrete
//! counterexample shape exists). [`SafetyVerdict::evaluate`] aggregates them: any `Disproved`
//! wins, then any `Blocked`, otherwise `Safe`. `disabled` obligations are recorded as
//! established; it exists only for the obligation-stripping tests, which prove that each
//! obligation is what stops its counterexample.

use crate::interpreter::{Allocs, Analysis, FnCtx};
use crate::liveness::Liveness;
use crate::{ExactTypeVal, FrameState, KindMask, ObligationStatus, SafetyTier};
use heki_ast::{LineIndex, Span};
use heki_ir::walk::Def;
use heki_ir::{BlockId, BuiltinKind, InstKind, NaturalLoop, VarId};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::fmt;

/// Why nothing is proven of a value the analysis holds no state for.
const UNTRACKED: &str = "its value is not tracked here";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ObligationKind {
    /// No frame-introspection hazard in the function (Assumption A1).
    A1HazardFree,
    /// The abstract interpretation reached a fixpoint.
    AnalysisConverged,
    ExactType,
    Unique,
    ExclusiveAccessAtWrite,
    EvalOrderPreserved,
    ExceptionStateSafe,
    /// `{**d, ...}`: the accumulator is the first entry and is not unpacked again.
    DictUnpackLeading,
    ValueLoopInvariant,
    /// Hoisting `set(...)` cannot raise even when the loop runs zero times.
    ZeroTripExceptionSafe,
    /// The probe of `e in container` compares the same way through `hash` as through `==`.
    HashConsistentProbe,
    /// The accumulator is not read inside the loop besides the accumulation itself.
    NoIntermediateReads,
    /// The list is changed only by the appends the auxiliary set mirrors.
    OnlyPairedAppends,
    /// Every use of the list has a `collections.deque` equivalent.
    DequeEquivalentOps,
    /// The hoisted expression has no side effects.
    SideEffectFree,
}

impl fmt::Display for ObligationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// One evaluated obligation, e.g. `ExclusiveAccessAtWrite(x)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofObligation {
    pub kind: ObligationKind,
    pub subject: String,
    pub status: ObligationStatus,
}

impl ProofObligation {
    pub fn label(&self) -> String {
        if self.subject.is_empty() {
            self.kind.to_string()
        } else {
            format!("{}({})", self.kind, self.subject)
        }
    }

    pub fn is_established(&self) -> bool {
        matches!(self.status, ObligationStatus::Established { .. })
    }

    /// `Kind(subject): reason`.
    pub fn describe(&self) -> String {
        let reason = match &self.status {
            ObligationStatus::Established { evidence } => evidence,
            ObligationStatus::Blocked { blocker } => blocker,
            ObligationStatus::Disproved { counterexample } => counterexample,
        };
        format!("{}: {reason}", self.label())
    }
}

/// Aggregated safety of one rewrite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyVerdict {
    pub tier: SafetyTier,
    pub obligations: Vec<ProofObligation>,
}

impl SafetyVerdict {
    /// Aggregate; `force_blocked` (A1 hazard) yields `Blocked` whatever the obligations say.
    pub fn evaluate(obligations: Vec<ProofObligation>, force_blocked: bool) -> Self {
        let any = |pred: fn(&ObligationStatus) -> bool| obligations.iter().any(|o| pred(&o.status));
        let tier = if force_blocked {
            SafetyTier::Blocked
        } else if any(|s| matches!(s, ObligationStatus::Disproved { .. })) {
            SafetyTier::Disproved
        } else if any(|s| matches!(s, ObligationStatus::Blocked { .. })) {
            SafetyTier::Blocked
        } else {
            SafetyTier::Safe
        };
        Self { tier, obligations }
    }

    pub fn blocker(&self) -> Option<&ProofObligation> {
        self.obligations
            .iter()
            .find(|o| matches!(o.status, ObligationStatus::Blocked { .. }))
    }

    pub fn counterexample(&self) -> Option<&ProofObligation> {
        self.obligations
            .iter()
            .find(|o| matches!(o.status, ObligationStatus::Disproved { .. }))
    }

    pub fn get(&self, kind: ObligationKind) -> Option<&ProofObligation> {
        self.obligations.iter().find(|o| o.kind == kind)
    }

    /// `Unique(x), ExclusiveAccessAtWrite(x), ...` of the established obligations.
    pub fn evidence(&self) -> String {
        self.obligations
            .iter()
            .filter(|o| o.is_established())
            .map(ProofObligation::label)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Obligation evaluation over one analyzed function.
pub(crate) struct Checker<'a, 'c> {
    pub cx: &'c FnCtx<'a>,
    pub an: &'c Analysis,
    pub live: &'c Liveness,
    pub mutations: &'c FxHashMap<(BlockId, usize), Allocs>,
    pub disabled: &'c [ObligationKind],
    /// Lines of the module, for the positions the messages name.
    pub lines: &'c LineIndex,
    handler_names: RefCell<FxHashMap<BlockId, FxHashSet<String>>>,
}

impl<'a, 'c> Checker<'a, 'c> {
    pub fn new(
        cx: &'c FnCtx<'a>,
        an: &'c Analysis,
        live: &'c Liveness,
        mutations: &'c FxHashMap<(BlockId, usize), Allocs>,
        disabled: &'c [ObligationKind],
        lines: &'c LineIndex,
    ) -> Self {
        Self {
            cx,
            an,
            live,
            mutations,
            disabled,
            lines,
            handler_names: RefCell::new(FxHashMap::default()),
        }
    }

    /// The 1-based line of a source offset.
    pub fn line(&self, offset: u32) -> u32 {
        self.lines.line(offset)
    }

    pub fn ob(
        &self,
        kind: ObligationKind,
        subject: &str,
        status: ObligationStatus,
    ) -> ProofObligation {
        let status = if self.disabled.contains(&kind) {
            ObligationStatus::Established {
                evidence: "disabled for an obligation-stripping test".into(),
            }
        } else {
            status
        };
        ProofObligation {
            kind,
            subject: subject.to_owned(),
            status,
        }
    }

    pub fn name(&self, v: VarId) -> String {
        // A starred display element (`[*xs]`) is a copy of the iterable it unpacks.
        if self.cx.f.is_unpacked(v)
            && let Some(i) = self.cx.shape.inst(v)
            && let InstKind::Copy { src, .. } = &i.kind
        {
            return self.name(*src);
        }
        self.cx
            .f
            .var_name(v)
            .filter(|n| !n.starts_with('.'))
            .map_or_else(|| self.describe_temp(v), |n| format!("`{n}`"))
    }

    fn describe_temp(&self, v: VarId) -> String {
        match self.cx.shape.inst(v).map(|i| &i.kind) {
            Some(InstKind::MakeIter { .. }) => "the loop's iterator".into(),
            Some(InstKind::MakeGenExpr { .. }) => "a generator expression".into(),
            Some(InstKind::Call { callee, .. }) => format!("the result of `{callee}(...)`"),
            Some(InstKind::MethodCall { method, .. }) => format!("the result of `.{method}()`"),
            _ => "a temporary".into(),
        }
    }

    pub fn a1_hazard_free(&self) -> ProofObligation {
        let status = match self.cx.f.a1_hazards.first() {
            Some(h) => ObligationStatus::Blocked {
                blocker: format!(
                    "A1 hazard: `{}` (line {}) can read or rebind locals behind the analysis",
                    h.kind,
                    self.line(h.span.start)
                ),
            },
            None => ObligationStatus::Established {
                evidence: "no frame introspection in the function".into(),
            },
        };
        self.ob(ObligationKind::A1HazardFree, "", status)
    }

    pub fn converged(&self) -> ProofObligation {
        let status = if self.an.converged {
            ObligationStatus::Established {
                evidence: "fixpoint reached".into(),
            }
        } else {
            ObligationStatus::Blocked {
                blocker: "the abstract interpretation did not converge".into(),
            }
        };
        self.ob(ObligationKind::AnalysisConverged, "", status)
    }

    pub fn exact_type(
        &self,
        subject: &str,
        st: &FrameState,
        v: VarId,
        allowed: &[BuiltinKind],
    ) -> ProofObligation {
        let untracked = ExactTypeVal::Unknown {
            reason: UNTRACKED.into(),
        };
        let status = match st.var_type.get(&v).unwrap_or(&untracked) {
            ExactTypeVal::Exact { ty, origin } if allowed.contains(ty) => {
                ObligationStatus::Established {
                    evidence: format!(
                        "exact builtin `{}` created at line {}",
                        ty.name(),
                        self.line(origin.start)
                    ),
                }
            }
            ExactTypeVal::Exact { ty, .. } => ObligationStatus::Blocked {
                blocker: format!("it is a `{}`", ty.name()),
            },
            ExactTypeVal::Unknown { reason } => ObligationStatus::Blocked {
                blocker: format!(
                    "its exact type is unknown ({reason}); type annotations are not trusted"
                ),
            },
        };
        self.ob(ObligationKind::ExactType, subject, status)
    }

    pub fn unique(&self, subject: &str, st: &FrameState, v: VarId) -> ProofObligation {
        let witness = st.shared_witness(v);
        let status = if st.is_unique(v) {
            ObligationStatus::Established {
                evidence: "only local, tracked references exist".into(),
            }
        } else {
            let why = witness.map_or(UNTRACKED.to_owned(), |w| {
                format!("it may be shared: {}", w.describe(self.lines))
            });
            ObligationStatus::Blocked { blocker: why }
        };
        self.ob(ObligationKind::Unique, subject, status)
    }

    /// Every alias and derived reference of `v`'s objects is dead right after the write
    /// (`exclude`: variables whose old value the write itself replaces).
    /// `rebinds`: the write replaces `v`'s binding (`x = x + [e]`), so `v` itself must be dead
    /// too; an in-place call (`seen.append(v)`) keeps `v` bound.
    #[allow(clippy::too_many_arguments)]
    pub fn exclusive_access(
        &self,
        subject: &str,
        st: &FrameState,
        v: VarId,
        at: (BlockId, usize),
        exclude: &[VarId],
        rebinds: bool,
    ) -> ProofObligation {
        let live = self.live.live_after(self.cx.f, at.0, at.1);
        let mut status = ObligationStatus::Established {
            evidence: "no other reference to the object is live after the write".into(),
        };
        if rebinds && live.contains(v) {
            status = ObligationStatus::Disproved {
                counterexample: format!(
                    "the old value of {} is read after the write",
                    self.name(v)
                ),
            };
        }
        'outer: for a in st.allocs_of(v) {
            for &m in st.alias_sets.get(&a).map(Vec::as_slice).unwrap_or(&[]) {
                if m != v && !exclude.contains(&m) && live.contains(m) {
                    status = ObligationStatus::Disproved {
                        counterexample: format!(
                            "{} still refers to the same object and is used later",
                            self.name(m)
                        ),
                    };
                    break 'outer;
                }
            }
            for &d in st.derived_refs.get(&a).map(Vec::as_slice).unwrap_or(&[]) {
                if !exclude.contains(&d) && live.contains(d) {
                    status = ObligationStatus::Disproved {
                        counterexample: format!(
                            "{} (an iterator or view of it) is still live and would observe the in-place update",
                            self.name(d)
                        ),
                    };
                    break 'outer;
                }
            }
        }
        self.ob(ObligationKind::ExclusiveAccessAtWrite, subject, status)
    }

    /// Source names read anywhere reachable from `handler` (including after it).
    fn names_read_from(&self, handler: BlockId) -> FxHashSet<String> {
        if let Some(s) = self.handler_names.borrow().get(&handler) {
            return s.clone();
        }
        let f = self.cx.f;
        let mut names = FxHashSet::default();
        let mut seen = FxHashSet::default();
        let mut stack = vec![handler];
        let add = |v: VarId, names: &mut FxHashSet<String>| {
            if let Some(n) = f.var_name(v) {
                names.insert(n.to_owned());
            }
        };
        while let Some(b) = stack.pop() {
            if !seen.insert(b) {
                continue;
            }
            let bb = f.block(b);
            for phi in &bb.phis {
                for &(_, v) in &phi.incoming {
                    add(v, &mut names);
                }
            }
            for inst in &bb.insts {
                for op in inst.kind.operands() {
                    add(op, &mut names);
                }
            }
            if let Some(v) = crate::interpreter::term_use(&bb.terminator) {
                add(v, &mut names);
            }
            stack.extend(bb.terminator.successors());
            stack.extend(bb.exc_target);
        }
        self.handler_names
            .borrow_mut()
            .insert(handler, names.clone());
        names
    }

    /// Blocks reachable from `start` (inclusive), following normal and exception edges.
    pub fn reachable_from(&self, start: BlockId) -> FxHashSet<BlockId> {
        let f = self.cx.f;
        let mut seen = FxHashSet::default();
        let mut stack = vec![start];
        while let Some(b) = stack.pop() {
            if seen.insert(b) {
                let bb = f.block(b);
                stack.extend(bb.terminator.successors());
                stack.extend(bb.exc_target);
            }
        }
        seen
    }

    /// Names a handler may read after an exception raised at or after `from`.
    pub fn handler_observed_names(&self, from: BlockId) -> FxHashSet<String> {
        let mut out = FxHashSet::default();
        for b in self.reachable_from(from) {
            if let Some(h) = self.cx.f.block(b).exc_target {
                out.extend(self.names_read_from(h));
            }
        }
        out
    }

    /// A handler entered after the write cannot observe an older binding of another name that
    /// aliases `v` (the IR's handler phis only see each block's last binding).
    pub fn exception_state_safe(
        &self,
        subject: &str,
        st: &FrameState,
        v: VarId,
        at: BlockId,
        target_name: &str,
        exclude: &[VarId],
    ) -> ProofObligation {
        let observed = self.handler_observed_names(at);
        let mut status = ObligationStatus::Established {
            evidence: "no exception handler can observe another reference to the old object".into(),
        };
        if !observed.is_empty() {
            'outer: for a in st.allocs_of(v) {
                let members = st.alias_sets.get(&a).into_iter().flatten();
                let derived = st.derived_refs.get(&a).into_iter().flatten();
                for &m in members.chain(derived) {
                    if m == v || exclude.contains(&m) {
                        continue;
                    }
                    if let Some(n) = self.cx.f.var_name(m)
                        && n != target_name
                        && observed.contains(n)
                    {
                        status = ObligationStatus::Blocked {
                            blocker: format!(
                                "an exception handler may still see `{n}` bound to the old object"
                            ),
                        };
                        break 'outer;
                    }
                }
            }
        }
        self.ob(ObligationKind::ExceptionStateSafe, subject, status)
    }

    /// The variable is bound on every path to its uses (not a possibly-unbound name).
    pub fn definitely_bound(&self, v: VarId) -> bool {
        let mut seen = FxHashSet::default();
        let mut stack = vec![v];
        while let Some(x) = stack.pop() {
            if !seen.insert(x) {
                continue;
            }
            match self.cx.shape.def(x) {
                Def::Param => {}
                Def::Phi(b, _) => stack.extend(
                    self.cx
                        .f
                        .block(b)
                        .phis
                        .iter()
                        .filter(|p| p.dst == x)
                        .flat_map(|p| p.incoming.iter().map(|&(_, v)| v)),
                ),
                // An instruction's result, or no definition at all (no instruction).
                Def::Inst(..) | Def::None => match self.cx.shape.inst(x).map(|inst| &inst.kind) {
                    Some(
                        InstKind::LoadName { .. }
                        | InstKind::LoadGlobal { .. }
                        | InstKind::LoadDeref { .. },
                    )
                    | None => return false,
                    Some(InstKind::Copy { src, .. }) => stack.push(*src),
                    Some(_) => {}
                },
            }
        }
        true
    }

    /// Where `v` is (re)bound after the point a hoisted statement would run: inside the loop,
    /// or in `region`, the loop statement's iterable (`for p in (lst := ...):`), which runs
    /// after a statement hoisted before the loop.
    pub fn rebinding(
        &self,
        v: VarId,
        lp: &NaturalLoop,
        region: Option<Span>,
    ) -> Option<&'static str> {
        if let Def::Phi(b, _) | Def::Inst(b, _) = self.cx.shape.def(v)
            && self.cx.shape.in_body(lp, b)
        {
            return Some("inside the loop");
        }
        region
            .is_some_and(|r| self.defined_in(v, r, &mut FxHashSet::default()))
            .then_some(
                "in the loop statement's iterable, after the point the hoisted value is built",
            )
    }

    /// `v`'s value is produced inside `region`: by an instruction there, or by a phi merging
    /// such a value (`(lst := a) if c else (lst := b)`).
    fn defined_in(&self, v: VarId, region: Span, seen: &mut FxHashSet<VarId>) -> bool {
        if !seen.insert(v) {
            return false;
        }
        match self.cx.shape.def(v) {
            Def::Inst(b, i) => within(self.cx.f.block(b).insts[i as usize].span, region),
            Def::Phi(b, _) => self
                .cx
                .f
                .block(b)
                .phis
                .iter()
                .filter(|p| p.dst == v)
                .flat_map(|p| &p.incoming)
                .any(|&(_, w)| self.defined_in(w, region, seen)),
            Def::Param | Def::None => false,
        }
    }

    /// `ValueLoopInvariant(v, loop)` (spec section 4.4), checked over the loop and `region` (the
    /// loop statement's iterable, which runs after a statement hoisted before the loop).
    pub fn value_loop_invariant(
        &self,
        subject: &str,
        st: &FrameState,
        v: VarId,
        lp: &NaturalLoop,
        region: Option<Span>,
    ) -> ProofObligation {
        let kind = ObligationKind::ValueLoopInvariant;
        if let Some(wh) = self.rebinding(v, lp, region) {
            return self.ob(
                kind,
                subject,
                ObligationStatus::Disproved {
                    counterexample: format!("{} is rebound {wh}", self.name(v)),
                },
            );
        }
        let ty = st.exact_type(v);
        if matches!(
            ty,
            Some(
                BuiltinKind::Tuple
                    | BuiltinKind::Str
                    | BuiltinKind::Bytes
                    | BuiltinKind::Int
                    | BuiltinKind::Float
                    | BuiltinKind::Bool
            )
        ) {
            return self.ob(
                kind,
                subject,
                ObligationStatus::Established {
                    evidence: "immutable value bound outside the loop".into(),
                },
            );
        }
        let allocs = st.allocs_of(v);
        // A mutation anywhere in the loop or in its iterable (whatever blocks it spans).
        let in_scope = |b: BlockId, i: usize| self.in_loop_or(lp, region, b, i);
        let mut mutated_at = None;
        for (&(b, i), m) in self.mutations {
            if in_scope(b, i) && m.iter().any(|a| allocs.contains(a)) {
                let off = self.cx.f.block(b).insts[i].span.start;
                if mutated_at.is_none_or(|o| off < o) {
                    mutated_at = Some(off);
                }
            }
        }
        if let Some(off) = mutated_at {
            return self.ob(
                kind,
                subject,
                ObligationStatus::Disproved {
                    counterexample: format!(
                        "{} is mutated inside the loop (line {})",
                        self.name(v),
                        self.line(off)
                    ),
                },
            );
        }
        let header_unique = self.an.block_in[lp.header.0 as usize]
            .as_ref()
            .is_some_and(|h| h.is_unique(v));
        let status = if st.is_unique(v) && header_unique && !allocs.is_empty() {
            ObligationStatus::Established {
                evidence: "bound outside the loop, unique, and never mutated in it".into(),
            }
        } else {
            let why = st
                .shared_witness(v)
                .map_or("its contents are not tracked".to_owned(), |w| {
                    w.describe(self.lines)
                });
            ObligationStatus::Blocked {
                blocker: format!("it may change during the loop: {why}"),
            }
        };
        self.ob(kind, subject, status)
    }

    /// Hoisting `set(v)` before the loop cannot raise (spec: S22).
    pub fn zero_trip_safe(&self, subject: &str, st: &FrameState, v: VarId) -> ProofObligation {
        let kind = ObligationKind::ZeroTripExceptionSafe;
        if !self.definitely_bound(v) {
            return self.ob(
                kind,
                subject,
                ObligationStatus::Blocked {
                    blocker: format!(
                        "{} may be unbound before the loop; hoisting would raise `NameError`",
                        self.name(v)
                    ),
                },
            );
        }
        let status = match self.cx.elems_of(st, v) {
            Some(m) if m.is_subset_of(KindMask::hash_consistent()) => ObligationStatus::Established {
                evidence: "every element is an `int` / `str` / `bytes` / `bool` / `float`".into(),
            },
            Some(m) => ObligationStatus::Blocked {
                blocker: format!(
                    "elements may be unhashable ({}): hoisting `set(...)` raises `TypeError` even when the loop runs zero times",
                    describe_mask(m)
                ),
            },
            None => ObligationStatus::Blocked {
                blocker: "element types are unknown: hoisting `set(...)` could raise `TypeError` even when the loop runs zero times".into(),
            },
        };
        self.ob(kind, subject, status)
    }

    /// `e in set_` agrees with `e in list_` for the probe `e`.
    pub fn hash_consistent_probe(
        &self,
        subject: &str,
        st: &FrameState,
        e: VarId,
    ) -> ProofObligation {
        let status = match self.cx.possible_kinds(st, e) {
            Some(m) if m == KindMask::EMPTY => ObligationStatus::Established {
                evidence: "the probe is drawn from an empty literal".into(),
            },
            Some(m) if m.is_subset_of(KindMask::hash_consistent()) => {
                ObligationStatus::Established {
                    evidence: format!("the probe is a builtin {}", describe_mask(m)),
                }
            }
            _ => ObligationStatus::Blocked {
                blocker: format!(
                    "{} may have a custom `__eq__` / `__hash__` or be unhashable",
                    self.name(e)
                ),
            },
        };
        self.ob(ObligationKind::HashConsistentProbe, subject, status)
    }

    /// Instruction `i` of block `b` runs inside `lp` or inside `region`.
    pub fn in_loop_or(&self, lp: &NaturalLoop, region: Option<Span>, b: BlockId, i: usize) -> bool {
        self.cx.shape.in_body(lp, b)
            || region.is_some_and(|r| within(self.cx.f.block(b).insts[i].span, r))
    }

    /// The first mutation of `allocs`, in source order, among the instructions `pred` accepts.
    pub fn first_mutation_in(
        &self,
        allocs: &[crate::AllocId],
        pred: impl Fn(BlockId, usize) -> bool,
    ) -> Option<(BlockId, usize, Span)> {
        let mut best: Option<(BlockId, usize, Span)> = None;
        for (&(b, i), m) in self.mutations {
            if pred(b, i) && m.iter().any(|a| allocs.contains(a)) {
                let span = self.cx.f.block(b).insts[i].span;
                if best.is_none_or(|(_, _, s)| span.start < s.start) {
                    best = Some((b, i, span));
                }
            }
        }
        best
    }
}

fn within(inner: Span, outer: Span) -> bool {
    inner.start >= outer.start && inner.end <= outer.end
}

/// `str / int`: the kinds of a non-empty mask (both callers handle the empty mask, which never
/// holds a value, before describing one).
pub(crate) fn describe_mask(m: KindMask) -> String {
    let names: Vec<&str> = BuiltinKind::ALL
        .into_iter()
        .filter(|k| m.contains(*k))
        .map(BuiltinKind::name)
        .collect();
    names.join(" / ")
}
