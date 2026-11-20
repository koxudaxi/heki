//! Family 3 (`OPT101`–`OPT130`): `heki_soundness` candidates turned into diagnostics
//! (docs/ANALYSIS_AND_KB_SPEC.md section 3.3, docs/fixes.md).
//!
//! Reported: candidates that are not `Disproved`, are profitable, and have proven cost
//! (`OptCandidate::is_reportable`). A diagnostic carries `fix` only when the rewrite is `Safe`;
//! whether `--fix` applies it by default is the rule's `auto_fix_default` (`OPT101` / `OPT102`).
//!
//! `OPT112` / `OPT113` are Advisory-lane reports without a rewrite: they use the Advisory
//! receiver types (`heki_effect::TypeCtx`, PEP 484 annotations included), which the Soundness
//! lane never trusts.

use crate::{CheckContext, ModuleUnit};
use heki_ast::Span;
use heki_diagnostics::{Diagnostic, TextEdit};
use heki_effect::{TypeCtx, Val};
use heki_ir::{
    BlockId, BuiltinKind, CompareOpKind, ConstLiteral, FnIndex, Inst, InstKind, IrFunction,
    LoopBound, NaturalLoop, PhiNode, SmallDisplay, Terminator, UnaryOpKind, VarId,
};
use heki_soundness::{
    AnalysisOptions, OptCandidate, ProfitabilityTier, SafetyTier, analyze_module,
};
use rustc_hash::{FxHashMap, FxHashSet};

/// Suffix explaining what `--fix` does with one candidate.
fn fix_note(c: &OptCandidate) -> String {
    match (c.verdict.tier, c.fix().is_some(), c.profitability) {
        (SafetyTier::Safe, true, ProfitabilityTier::Unconditional) => {
            " [safe fix: `heki check --fix`]".into()
        }
        (SafetyTier::Safe, true, ProfitabilityTier::SizeDependent) => {
            " [proven-safe suggestion: `heki check --fix --include-size-dependent`]".into()
        }
        (SafetyTier::Safe, false, _) => " [proven safe; rewrite it by hand]".into(),
        // Reported candidates that are not safe are `Blocked`, which names its obligation.
        _ => c
            .verdict
            .blocker()
            .map(|b| format!(" [not auto-fixable: {}]", b.describe()))
            .unwrap_or_default(),
    }
}

pub(crate) fn opt(
    cx: &CheckContext<'_>,
    unit: &ModuleUnit<'_>,
    opts: &AnalysisOptions,
    out: &mut Vec<Diagnostic>,
) {
    let any_enabled = heki_soundness::OptRule::ALL
        .iter()
        .any(|r| cx.enabled(r.code(), unit));
    if !any_enabled {
        return;
    }
    for c in analyze_module(unit.input.ir, unit.source, cx.soundness, opts) {
        if !c.is_reportable() {
            continue;
        }
        let message = format!("{}{}", c.message, fix_note(&c));
        let Some(d) = cx.emit(
            out,
            c.rule.code(),
            unit,
            c.span,
            &c.function,
            c.target.clone(),
            message,
            &[],
        ) else {
            // This `OPT` rule is not selected for the file.
            continue;
        };
        d.evidence = c.cost_evidence.clone().map(|cost| match c.verdict.tier {
            SafetyTier::Safe => format!("{cost}; established: {}", c.verdict.evidence()),
            _ => cost,
        });
        d.blocker = c.verdict.blocker().map(|b| b.describe());
        d.fix = c.fix().map(|edits| {
            edits
                .iter()
                .map(|e| TextEdit {
                    span: e.span,
                    replacement: e.replacement.clone(),
                })
                .collect()
        });
    }
}

// ============================================================================= loop sizes

/// The Advisory lane's view of loop and container sizes: the shared IR facts
/// (`heki_ir::FnIndex`: displays of this function, this module's constants, loop trip counts),
/// extended to the small displays of other modules' globals and of instance attributes, which
/// only the declarations know (`ProjectDecls::small_global` / `small_attr`, read through the
/// recorded declarations so that the language server re-checks a reader when they change).
pub(crate) struct Sizes<'a, 'c> {
    cx: &'c CheckContext<'a>,
    tcx: &'c TypeCtx<'a>,
    /// Receiver types for attribute loads (empty: attributes are not resolved).
    types: &'c FxHashMap<VarId, Val>,
    pub ix: FnIndex<'c>,
}

impl<'a, 'c> Sizes<'a, 'c> {
    pub(crate) fn new(
        cx: &'c CheckContext<'a>,
        tcx: &'c TypeCtx<'a>,
        types: &'c FxHashMap<VarId, Val>,
        f: &'c IrFunction,
        unit: &'c ModuleUnit<'_>,
    ) -> Self {
        Self {
            cx,
            tcx,
            types,
            ix: FnIndex::new(f, &unit.input.ir.constants),
        }
    }

    /// A data-sized loop: its trip count is not fixed by the code (`LoopBound::Unbounded`, the
    /// IR's verdict), and it does not iterate a small display of another module or an
    /// attribute (or a builtin wrapper of one: `enumerate(PREFIXES)`).
    pub(crate) fn data_sized(&self, lp: &NaturalLoop) -> bool {
        lp.bound == LoopBound::Unbounded
            && !self.ix.loop_iterable(lp).is_some_and(|it| {
                self.ix
                    .iter_bound(it, &mut |inst| {
                        let d = self.leaf_display(inst)?;
                        self.ix.load_unchanged(inst.kind.dst()?).then_some(d.len)
                    })
                    .is_some()
            })
    }

    /// `var` is a small display of `kinds` for its whole life: built in this function or bound
    /// to a constant of this module (`FnIndex::container_bound`), or a small global of another
    /// module or attribute display (`Self::leaf_display`) that this function does not grow.
    pub(crate) fn small_container(&self, var: VarId, kinds: &[BuiltinKind]) -> bool {
        if self.ix.container_bound(var).is_some() {
            return true;
        }
        let mut v = var;
        for _ in 0..16 {
            let Some(inst) = self.ix.def_inst(v) else {
                return false;
            };
            if let InstKind::Copy { src, .. } = &inst.kind {
                v = *src;
                continue;
            }
            return self
                .leaf_display(inst)
                .is_some_and(|d| d.is_of(kinds) && self.ix.load_unchanged(v));
        }
        false
    }

    /// The small display a load of a global or an attribute gives, by the declarations: a
    /// module global (`NAME`, an imported `NAME`, `module.NAME`) or an instance attribute its
    /// class family only ever stores such displays into (`self.preferred = [",", ";"]`, a
    /// class attribute).
    fn leaf_display(&self, inst: &Inst) -> Option<SmallDisplay> {
        let decls = self.cx.decls;
        match &inst.kind {
            InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. } => {
                let fqn = self.tcx.module.imports.get(name).map_or_else(
                    || format!("{}.{name}", self.tcx.module_name()),
                    |t| decls.canonical(t),
                );
                decls.small_global(&fqn).cloned()
            }
            InstKind::AttrLoad { obj, attr, .. } => match self.types.get(obj)? {
                Val::Ref(module) => decls.small_global(&format!("{module}.{attr}")).cloned(),
                Val::Inst(class) => decls.small_attr(class, attr),
                _ => None,
            },
            _ => None,
        }
    }
}

// ==================================================================================== OPT112

/// `list` methods that scan the whole list.
const LINEAR_LIST_METHODS: &[&str] = &["index", "remove", "count"];

/// The small displays a `list` receiver may be.
const LIST: &[BuiltinKind] = &[BuiltinKind::List];

/// `OPT112`: `list.index()` / `list.remove()` / `list.count()` run again on every iteration of
/// a data-sized loop, on a list that outlives the iteration (Advisory lane).
pub(crate) fn opt112(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.enabled("OPT112", unit) {
        return;
    }
    let tcx = TypeCtx::new(&unit.input, cx.decls, cx.kb);
    for f in &unit.input.ir.functions {
        // `index(x, start)` scans from a position, usually one that advances (a linear walk).
        let calls: Vec<(BlockId, Span, VarId, &str)> = f
            .blocks
            .iter()
            .filter(|b| f.is_in_loop(b.id))
            .flat_map(|b| {
                b.insts.iter().filter_map(move |i| match &i.kind {
                    InstKind::MethodCall {
                        recv, method, args, ..
                    } if LINEAR_LIST_METHODS.contains(&method.as_str()) && args.len() == 1 => {
                        Some((b.id, i.span, *recv, method.as_str()))
                    }
                    _ => None,
                })
            })
            .collect();
        if f.is_class_body || calls.is_empty() {
            continue;
        }
        let types = tcx.infer(f);
        let defs = Defs::new(f);
        let sizes = Sizes::new(cx, &tcx, &types, f, unit);
        for (bb, span, recv, method) in calls {
            // Only a receiver that is certainly a builtin `list` (`set.remove`, `str.index`, a
            // project class's `remove`, or an unknown type are not linear scans of a list).
            let is_list = matches!(types.get(&recv), Some(Val::Inst(t)) if t == "builtins.list");
            // A data-sized loop (not a small constant range or display, a retry, or daemon
            // polling) runs the call again on the same list.
            let hot = f.loops.iter().any(|lp| {
                lp.body.contains(&bb)
                    && runs_again(f, lp, bb)
                    && sizes.ix.lives_across(lp, recv)
                    && sizes.data_sized(lp)
            });
            if !is_list || !hot || sizes.small_container(recv, LIST) {
                continue;
            }
            let call = slice(unit.source, span);
            let target = defs.receiver_name(recv).unwrap_or_else(|| {
                call.split_once(&format!(".{method}("))
                    .map_or(call, |(head, _)| head)
                    .to_owned()
            });
            let advice = match method {
                "index" => "build a `dict` (value → index) or a `set` once before the loop".into(),
                "remove" => {
                    "collect the values to drop in a `set` and rebuild the list once with a comprehension".into()
                }
                _ => format!("count once before the loop with `collections.Counter({target})`"),
            };
            cx.emit(
                out,
                "OPT112",
                unit,
                span,
                heki_effect::func_qualname(f),
                target.clone(),
                format!(
                    "`{target}.{method}()` scans the whole list on every iteration of the loop (O(n²)); {advice}"
                ),
                &[],
            );
        }
    }
}

/// Whether block `bb` of loop `lp` can run again in the same run of the loop: the loop header
/// is reachable from its normal successors without leaving the loop. A block that returns,
/// raises, or `break`s on every path runs once.
fn runs_again(f: &IrFunction, lp: &NaturalLoop, bb: BlockId) -> bool {
    let mut seen = FxHashSet::default();
    let mut stack = f.block(bb).terminator.successors();
    while let Some(b) = stack.pop() {
        if b == lp.header {
            return true;
        }
        if lp.body.contains(&b) && seen.insert(b) {
            stack.extend(f.block(b).terminator.successors());
        }
    }
    false
}

/// `roots` plus every variable that may hold the same value: `Copy` results and phis with
/// such an input.
fn forward_aliases(f: &IrFunction, roots: &[VarId]) -> FxHashSet<VarId> {
    let mut alias: FxHashSet<VarId> = roots.iter().copied().collect();
    loop {
        let before = alias.len();
        for bb in &f.blocks {
            for phi in &bb.phis {
                if phi.incoming.iter().any(|(_, v)| alias.contains(v)) {
                    alias.insert(phi.dst);
                }
            }
            for inst in &bb.insts {
                if let InstKind::Copy { dst, src } = &inst.kind
                    && alias.contains(src)
                {
                    alias.insert(*dst);
                }
            }
        }
        if alias.len() == before {
            return alias;
        }
    }
}

/// Source text of `span`.
fn slice(source: &str, span: Span) -> &str {
    source
        .get(span.start as usize..span.end as usize)
        .unwrap_or_default()
}

/// `text` on one line, runs of whitespace collapsed.
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Defining instruction or phi of every variable of one function, with its block.
struct Defs<'a> {
    f: &'a IrFunction,
    insts: FxHashMap<VarId, (BlockId, &'a Inst)>,
    phis: FxHashMap<VarId, (BlockId, &'a PhiNode)>,
    /// `MakeIter` results: the iterable and its span.
    iterables: FxHashMap<VarId, (VarId, Span)>,
}

impl<'a> Defs<'a> {
    fn new(f: &'a IrFunction) -> Self {
        let mut insts = FxHashMap::default();
        let mut phis = FxHashMap::default();
        let mut iterables = FxHashMap::default();
        for bb in &f.blocks {
            for phi in &bb.phis {
                phis.insert(phi.dst, (bb.id, phi));
            }
            for inst in &bb.insts {
                if let InstKind::MakeIter { dst, iterable, .. } = &inst.kind {
                    iterables.insert(*dst, (*iterable, inst.span));
                }
                if let Some(d) = inst.kind.dst() {
                    insts.insert(d, (bb.id, inst));
                }
            }
        }
        Self {
            f,
            insts,
            phis,
            iterables,
        }
    }

    fn kind(&self, var: VarId) -> Option<&'a InstKind> {
        self.insts.get(&var).map(|(_, i)| &i.kind)
    }

    /// Source form of a receiver: `items`, `self.items`, `config.names`.
    fn receiver_name(&self, var: VarId) -> Option<String> {
        match self.kind(var) {
            Some(InstKind::AttrLoad { obj, attr, .. }) => {
                Some(format!("{}.{attr}", self.receiver_name(*obj)?))
            }
            _ => self
                .f
                .var_name(var)
                .filter(|n| !n.starts_with('.'))
                .map(str::to_owned),
        }
    }

    // ------------------------------------------------------------------------------ OPT113

    /// The `for` loop `lp` (its header advances an iterator); `None` for a `while` loop.
    fn for_loop(&self, lp: &'a NaturalLoop) -> Option<ForLoop<'a>> {
        let (elem, iter) = self.next_of(lp)?;
        let &(iterable, iter_span) = self.iterables.get(&iter)?;
        Some(ForLoop {
            lp,
            iter_span,
            iterable,
            element: self.derived(elem),
        })
    }

    /// The `IterNext` of the `for` loop `lp`: its element and iterator.
    fn next_of(&self, lp: &NaturalLoop) -> Option<(VarId, VarId)> {
        self.f
            .block(lp.header)
            .insts
            .iter()
            .find_map(|i| match &i.kind {
                InstKind::IterNext { dst, iter, .. } => Some((*dst, *iter)),
                _ => None,
            })
    }

    /// `elem` and the values derived from it: copies, unpacked components and constant-key
    /// subscripts (`a[0]`, `a["k"]`), attribute loads (`a.k`, `a.x.y`).
    fn derived(&self, elem: VarId) -> FxHashSet<VarId> {
        let mut set = FxHashSet::default();
        set.insert(elem);
        loop {
            let before = set.len();
            for inst in self.f.blocks.iter().flat_map(|b| &b.insts) {
                let derived = match &inst.kind {
                    InstKind::Copy { dst, src: obj } | InstKind::AttrLoad { dst, obj, .. } => {
                        set.contains(obj).then_some(*dst)
                    }
                    InstKind::SubscriptLoad {
                        dst, obj, index, ..
                    } => (set.contains(obj)
                        && matches!(
                            self.kind(*index),
                            Some(InstKind::Const {
                                literal: ConstLiteral::Int(_) | ConstLiteral::Str(_),
                                ..
                            })
                        ))
                    .then_some(*dst),
                    _ => None,
                };
                set.extend(derived);
            }
            if set.len() == before {
                return set;
            }
        }
    }

    /// Whether `var` (the iterable of the loop headed by `inner`) is computed, inside `lp`,
    /// from the element of a loop around `inner` (`lp` or one nested in it): `for b in
    /// a.children`, `for b in index[a.k]`, `for b in f(a)`, `for b in xs[i + 1:]` traverse the
    /// outer element instead of joining with an independent collection.
    fn reads_elements(&self, lp: &NaturalLoop, inner: BlockId, var: VarId) -> bool {
        // A slice, an f-string, or a comprehension is a value without operands: the
        // instructions inside its span count as its operands.
        let values: Vec<(Span, VarId)> = lp
            .body
            .iter()
            .flat_map(|b| &self.f.block(*b).insts)
            .filter_map(|i| Some((i.span, i.kind.dst()?)))
            .collect();
        let mut seen = FxHashSet::default();
        let mut stack = vec![var];
        while let Some(v) = stack.pop() {
            if !seen.insert(v) {
                continue;
            }
            if let Some((bb, phi)) = self.phis.get(&v)
                && lp.body.contains(bb)
            {
                stack.extend(phi.incoming.iter().map(|(_, x)| *x));
            } else if let Some((bb, inst)) = self.insts.get(&v)
                && lp.body.contains(bb)
            {
                let element = matches!(inst.kind, InstKind::IterNext { .. })
                    && self
                        .f
                        .loops
                        .iter()
                        .any(|l| l.header == *bb && l.body.contains(&inner));
                if element {
                    return true;
                }
                stack.extend(inst.kind.operands());
                stack.extend(
                    values
                        .iter()
                        .filter(|(s, _)| inst.span.start <= s.start && s.end <= inst.span.end)
                        .map(|(_, d)| *d),
                );
            }
        }
        false
    }

    /// Whether the comparison `cmp` selects the pairs a join keeps: through `Copy` and `and` it
    /// decides a branch; it is never negated, never one alternative of an `or`, never picks a
    /// value for every pair (`x if a == b else y`, an `else` assigning what the `then`
    /// assigns), its true side never just continues the loop (`if a == b: continue` skips
    /// the self-pairs of an all-pairs loop), and its false side does nothing: an `else` /
    /// `elif` body, an `assert`'s failure, a `break` or a `return` there runs for the pairs an
    /// index would skip (`if i == j: ... else: ...` and `assert a == b` check every pair).
    fn selects_pairs(&self, cmp: VarId) -> bool {
        let alias = forward_aliases(self.f, &[cmp]);
        let negated = self.f.blocks.iter().flat_map(|b| &b.insts).any(|i| {
            matches!(&i.kind, InstKind::UnaryOp { op: UnaryOpKind::Not, operand, .. } if alias.contains(operand))
        });
        // An `or` merge: an operand branches into it when true.
        let alternative = self.f.blocks.iter().any(|m| {
            m.phis.iter().any(|p| {
                p.incoming.iter().any(|(_, v)| alias.contains(v))
                    && p.incoming.iter().any(|(from, v)| {
                        matches!(self.f.block(*from).terminator, Terminator::Branch { cond, then_bb, .. } if cond == *v && then_bb == m.id)
                    })
            })
        });
        let branches: Vec<(BlockId, BlockId, BlockId)> = self
            .f
            .blocks
            .iter()
            .filter_map(|b| match b.terminator {
                Terminator::Branch {
                    cond,
                    then_bb,
                    else_bb,
                } if alias.contains(&cond) => Some((b.id, then_bb, else_bb)),
                _ => None,
            })
            .collect();
        // Both sides compute a value: they jump to one merge (not the loop header) whose phi
        // takes a value from the false side.
        let picks_value = branches.iter().any(|&(_, t, e)| {
            matches!((&self.f.block(t).terminator, &self.f.block(e).terminator),
                (Terminator::Jump(mt), Terminator::Jump(m)) if mt == m
                    && self.f.loops.iter().all(|lp| lp.header != *m)
                    && self.f.block(*m).phis.iter().any(|p| p.incoming.iter().any(|(from, _)| *from == e)))
        });
        let preds = self.f.predecessors();
        !negated
            && !alternative
            && !picks_value
            && !branches.is_empty()
            && branches.iter().all(|&(from, then_bb, else_bb)| {
                !self.continues(from, then_bb) && !self.false_side_works(&preds, else_bb)
            })
    }

    /// Whether the false side of a branch, block `to`, does something: it is entered from the
    /// branch only (no `if` without `else` merges there) and runs code, raises, returns, or
    /// leaves the loop, rather than going straight on (`else: pass`, `else: continue`).
    fn false_side_works(&self, preds: &[Vec<BlockId>], to: BlockId) -> bool {
        if preds[to.0 as usize].len() != 1 {
            return false;
        }
        let b = self.f.block(to);
        let goes_on = b.insts.is_empty()
            && b.phis.is_empty()
            && matches!(b.terminator, Terminator::Jump(t)
                if !self.f.loops.iter().any(|lp| lp.exits.contains(&t)));
        !goes_on
    }

    /// Whether block `to`, entered from block `from`, only continues a loop containing `from`.
    fn continues(&self, from: BlockId, to: BlockId) -> bool {
        let b = self.f.block(to);
        b.insts.is_empty()
            && b.phis.is_empty()
            && matches!(b.terminator, Terminator::Jump(h)
                if self.f.loops.iter().any(|lp| lp.header == h && lp.body.contains(&from)))
    }

    /// Whether `var` is a loop element itself (through `Copy`), not a part of it.
    fn whole_element(&self, mut var: VarId) -> bool {
        while let Some(InstKind::Copy { src, .. }) = self.kind(var) {
            var = *src;
        }
        matches!(self.kind(var), Some(InstKind::IterNext { .. }))
    }

    /// Source form of a loop-element value: `order.customer_id`, `row["id"]`, `pair[0]`, `k`.
    fn element_text(&self, source: &str, var: VarId) -> String {
        let named = self.f.var_name(var).filter(|n| !n.starts_with('.'));
        let (_, inst) = self.insts[&var];
        match (named, &inst.kind) {
            (Some(n), _) => n.to_owned(),
            (None, InstKind::AttrLoad { obj, attr, .. }) => {
                format!("{}.{attr}", self.element_text(source, *obj))
            }
            // A constant subscript, not an unpacked component (whose index spans the target).
            (None, InstKind::SubscriptLoad { obj, index, .. })
                if self.insts[index].1.span != inst.span =>
            {
                format!(
                    "{}[{}]",
                    self.element_text(source, *obj),
                    slice(source, self.insts[index].1.span)
                )
            }
            _ => squash(slice(source, inst.span)),
        }
    }
}

// ==================================================================================== OPT113

/// One data-sized `for` loop of a function.
struct ForLoop<'a> {
    lp: &'a NaturalLoop,
    /// The iterable expression.
    iter_span: Span,
    iterable: VarId,
    /// The loop element and the values derived from it.
    element: FxHashSet<VarId>,
}

/// The comparison reported for one inner loop.
struct Join<'l, 'a> {
    span: Span,
    outer: &'l ForLoop<'a>,
    lhs: VarId,
    rhs: VarId,
    /// The side derived from the inner loop's element.
    inner_side: VarId,
}

/// `OPT113`: a nested-loop equi-join — `for a in A: for b in B: if a.k == b.k:` costs
/// O(n·m); indexing `B` by the key once costs O(n + m) (Advisory lane). One report per inner
/// loop, at its first matching comparison.
pub(crate) fn opt113(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.enabled("OPT113", unit) {
        return;
    }
    let tcx = TypeCtx::new(&unit.input, cx.decls, cx.kb);
    let no_types = FxHashMap::default();
    for f in &unit.input.ir.functions {
        if f.is_class_body || f.loops.len() < 2 {
            continue;
        }
        let defs = Defs::new(f);
        let sizes = Sizes::new(cx, &tcx, &no_types, f, unit);
        // Both loops are data-sized `for` loops (inlined comprehensions included).
        let fors: Vec<ForLoop<'_>> = f
            .loops
            .iter()
            .filter(|lp| sizes.data_sized(lp))
            .filter_map(|lp| defs.for_loop(lp))
            .collect();
        for inner in &fors {
            let mut hit: Option<Join<'_, '_>> = None;
            let outers = fors.iter().filter(|o| {
                o.lp.header != inner.lp.header
                    && o.lp.body.contains(&inner.lp.header)
                    && !defs.reads_elements(o.lp, inner.lp.header, inner.iterable)
            });
            for outer in outers {
                for inst in inner.lp.body.iter().flat_map(|b| &f.block(*b).insts) {
                    let InstKind::Compare {
                        dst,
                        op: CompareOpKind::Eq,
                        lhs,
                        rhs,
                    } = &inst.kind
                    else {
                        continue;
                    };
                    let inner_side = if outer.element.contains(lhs) && inner.element.contains(rhs) {
                        *rhs
                    } else if inner.element.contains(lhs) && outer.element.contains(rhs) {
                        *lhs
                    } else {
                        continue;
                    };
                    if hit.as_ref().is_none_or(|h| inst.span.start < h.span.start)
                        && defs.selects_pairs(*dst)
                    {
                        hit = Some(Join {
                            span: inst.span,
                            outer,
                            lhs: *lhs,
                            rhs: *rhs,
                            inner_side,
                        });
                    }
                }
            }
            if let Some(j) = hit {
                let lhs = defs.element_text(unit.source, j.lhs);
                let rhs = defs.element_text(unit.source, j.rhs);
                let outer_iter = squash(slice(unit.source, j.outer.iter_span));
                let inner_iter = squash(slice(unit.source, inner.iter_span));
                let index = if defs.whole_element(j.inner_side) {
                    format!("build a `set` of `{inner_iter}` before the outer loop")
                } else {
                    let key = defs.element_text(unit.source, j.inner_side);
                    format!("index `{inner_iter}` by `{key}` in a `dict` before the outer loop")
                };
                cx.emit(
                    out,
                    "OPT113",
                    unit,
                    j.span,
                    heki_effect::func_qualname(f),
                    format!("{lhs}=={rhs}"),
                    format!(
                        "nested loops join `{outer_iter}` and `{inner_iter}` on `{lhs} == {rhs}` (O(n·m)); {index}"
                    ),
                    &[],
                );
            }
        }
    }
}
