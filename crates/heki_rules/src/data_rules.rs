//! Family 2 data rule: `DATA101` (docs/ANALYSIS_AND_KB_SPEC.md section 2.4).

use crate::eff_rules::{DataOrigins, shortest};
use crate::mut_rules::{CallInst, site_call};
use crate::opt_rules::Sizes;
use crate::{CheckContext, ModuleUnit};
use heki_diagnostics::{Certainty, Diagnostic};
use heki_effect::{CallSite, SiteEffect, TypeCtx, with_paths};
use heki_ir::walk::{Answer, Budget, Def, Visit, reach};
use heki_ir::{
    BinOpKind, BuiltinKind, ConstLiteral, Inst, InstKind, IrFunction, LoopCarriedInfo, Must,
    NaturalLoop, Progress, UnaryOpKind, VarId,
};
use heki_kb::ExecEffectKind;
use rustc_hash::{FxHashMap, FxHashSet};

/// `DATA101`: a loop-carried variable rebuilt *from itself* through a `QuadraticRealloc` API
/// (`df = pd.concat([df, part])`): one of its accumulating updates (`LoopCarriedInfo::updates`,
/// the back-edge values computed from the previous value) is such a call, and the previous
/// value reaches it through the data it combines, not only as the `out=` buffer it writes into
/// (`out = np.concatenate(parts, out=out)` reuses the buffer) or through arithmetic on it
/// (`x = np.concatenate((x - d, [a]))` combines a new array). One-shot concatenations inside a
/// loop, also on a branch that leaves the variable unchanged otherwise, are not updates and are
/// never reported, and neither is a loop whose trip count the code fixes (`Sizes::data_sized`).
pub(crate) fn data101(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.enabled("DATA101", unit) {
        return;
    }
    let sites = cx.sites_by_span(unit);
    let tcx = TypeCtx::new(&unit.input, cx.decls, cx.kb);
    let no_types = FxHashMap::default();
    let constants = cx.fixed_constants(unit);
    for f in &unit.input.ir.functions {
        if f.is_class_body || f.loop_carried.iter().all(|lc| !lc.self_accumulating) {
            continue;
        }
        let key = CheckContext::key(unit, f);
        let sizes = Sizes::new(cx, &tcx, &no_types, f, unit, &constants);
        let mut defs: FxHashMap<VarId, &Inst> = FxHashMap::default();
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            if let Some(d) = inst.kind.dst() {
                defs.insert(d, inst);
            }
        }
        let starred = first_starred(f);
        for (lc, lp) in accumulating_loops(f)
            .into_iter()
            .filter(|(_, lp)| sizes.data_sized(lp))
        {
            // The first update (in block order) that is a quadratic reallocation of the previous
            // value: the reallocating call itself, or a project call returning one of the
            // argument that carries it (`acc = grow(acc)`, not `acc = step(acc)` where `step`
            // reallocates something else and returns `acc`).
            let hit = lc.updates.iter().find_map(|v| {
                // An awaited call's value is the call's (`acc = await to_thread(grow, acc)`).
                let inst = match &defs.get(v)?.kind {
                    InstKind::Await { value, .. } => defs.get(value)?,
                    _ => defs.get(v)?,
                };
                let call = CallInst::of(&inst.kind)?;
                if !combines(f, &defs, &starred, &call, lc.phi_var) {
                    return None;
                }
                let at: &[&CallSite] = sites
                    .get(&(key.qualname.clone(), inst.span))
                    .map_or(&[], Vec::as_slice);
                let effects: Vec<SiteEffect> = at
                    .iter()
                    .flat_map(|s| cx.index.site_effects(&unit.input.path, s))
                    .collect();
                let chain = shortest(&effects, ExecEffectKind::QuadraticRealloc)?;
                let reallocates =
                    chain.len() == 1 || reallocates_argument(cx, at, f, &defs, inst, lc.phi_var);
                reallocates.then(|| (inst.span, chain.to_vec(), *v, at))
            });
            let Some((span, chain, update, at)) = hit else {
                continue;
            };
            let established = established_update(cx, unit, &tcx, f, lc, lp, update, at);
            // Loop-carried values are header phis, which the IR builder only creates for
            // named locals (`read_local_recursive`).
            let var = lc
                .var_name
                .clone()
                .expect("loop-carried values are named locals");
            let root = chain.last().map(|h| h.callee.clone()).unwrap_or_default();
            let every = match &established {
                Some(origins) => format!("on {}", origins.every_iteration()),
                None => "on every loop iteration".to_owned(),
            };
            if let Some(d) = cx.emit(
                out,
                "DATA101",
                unit,
                span,
                &key.qualname,
                var.clone(),
                format!("`{var}` is rebuilt from itself with `{root}` {every}"),
                Some("collect the parts in a list and combine them once after the loop".to_owned()),
                &chain,
            ) {
                let premises = if established.is_some() {
                    Certainty::Exact
                } else {
                    Certainty::Heuristic
                };
                d.certainty = premises.at_most(d.certainty);
            }
        }
    }
}

/// The origins that establish a `DATA101` report of the accumulating update `update` of `lc` in
/// `lp`, named as `EFF103` names them: the update copies the whole previous value whenever it
/// returns (`heki_effect::ProjectIndex::site_must` of its call sites `at`), returns on every
/// iteration of the loop that completes, and is the value every path around the back edge
/// carries (no path resets the variable); the loop steps through items the data decides, and
/// its trip count is not one the code fixes. `None` when a premise is not established.
#[allow(clippy::too_many_arguments)]
fn established_update(
    cx: &CheckContext<'_>,
    unit: &ModuleUnit<'_>,
    tcx: &TypeCtx<'_>,
    f: &IrFunction,
    lc: &LoopCarriedInfo,
    lp: &NaturalLoop,
    update: VarId,
    at: &[&CallSite],
) -> Option<DataOrigins> {
    at.iter().find_map(|s| {
        cx.index
            .site_must(&unit.input.path, s, ExecEffectKind::QuadraticRealloc)
    })?;
    let li = f.loops.iter().position(|l| l.header == lp.header)?;
    let types = tcx.infer(f);
    let owner = "heki_rules::data_rules::established_update";
    with_paths(tcx, f, &types, owner, |paths, ix| {
        let shape = ix.shape();
        // An awaited call's value is the `await`'s (`acc = await to_thread(grow, acc)`).
        let call = match shape.inst(update).map(|i| &i.kind) {
            Some(InstKind::Await { value, .. }) => *value,
            _ => update,
        };
        let Def::Inst(block, index) = shape.def(call) else {
            return None;
        };
        let at = (block, index as usize);
        if paths.on_every_iteration(li, at, Progress::Returned) != Answer::Known(Must::Every)
            || !ix.fixed_trips(lp, &mut |_| None).is_open()
        {
            return None;
        }
        let budget = Budget::of_function(owner, f);
        // Every value the back edge carries is the update, through copies and phis.
        let reset = reach(
            &budget,
            lc.back_vars.iter().copied(),
            |v, next| match shape.def(v) {
                Def::Phi(b, i) => next.extend(
                    f.block(b).phis[i as usize]
                        .incoming
                        .iter()
                        .map(|(_, input)| *input),
                ),
                Def::Inst(..) => {
                    if let Some(InstKind::Copy { src, .. }) = shape.inst(v).map(|i| &i.kind) {
                        next.push(*src);
                    }
                }
                Def::Param | Def::None => {}
            },
            |v| {
                if v == update {
                    return Visit::Prune;
                }
                match shape.def(v) {
                    Def::Phi(..) => Visit::Expand,
                    Def::Inst(..)
                        if matches!(
                            shape.inst(v).map(|i| &i.kind),
                            Some(InstKind::Copy { .. })
                        ) =>
                    {
                        Visit::Expand
                    }
                    _ => Visit::Stop(()),
                }
            },
        );
        if reset != Answer::Known(None) {
            return None;
        }
        let origins = crate::eff_rules::ItemOrigins::new(unit.input.ir);
        let Answer::Known(found) = origins.of_loop(&budget, f.id, li) else {
            return None;
        };
        DataOrigins::of(cx, tcx, unit.input.ir, &found)
    })
}

/// Whether the project call the call instruction `inst` makes (its call site among `sites`:
/// on the caller's thread, or on a worker an offloading API runs it on) returns a quadratic
/// reallocation of an argument that carries `previous` (as itself, through copies, or in a
/// display): a callee's `reallocates_params`.
fn reallocates_argument<'i>(
    cx: &CheckContext<'_>,
    sites: &[&'i CallSite],
    f: &IrFunction,
    defs: &FxHashMap<VarId, &'i Inst>,
    inst: &'i Inst,
    previous: VarId,
) -> bool {
    let def = |v| defs.get(&v).copied();
    let call = site_call(&def, sites, inst, false).or_else(|| site_call(&def, sites, inst, true));
    call.iter()
        .flat_map(|c| c.bound_arguments(cx))
        .any(|(_, summary, pos, v)| {
            pos < 64 && summary.reallocates_params & (1u64 << pos) != 0 && carries(f, v, previous)
        })
}

/// Whether `v` is `previous`, a copy of it, or a display holding one.
fn carries(f: &IrFunction, v: VarId, previous: VarId) -> bool {
    let mut stack = vec![v];
    let mut seen = Vec::new();
    while let Some(v) = stack.pop() {
        if v == previous {
            return true;
        }
        if seen.contains(&v) {
            continue;
        }
        seen.push(v);
        match f.def_inst(v).map(|(_, i)| &i.kind) {
            Some(InstKind::Copy { src, .. }) => stack.push(*src),
            Some(InstKind::AllocBuiltin { elems, .. }) => stack.extend(elems.iter().copied()),
            _ => {}
        }
    }
    false
}

/// Whether `call` reads `previous` through what it combines: its receiver, its positional
/// arguments, or a keyword argument other than the `out=` destination, followed back through the
/// phis and the operands each instruction's result may hold ([`held_operands`]).
fn combines(
    f: &IrFunction,
    defs: &FxHashMap<VarId, &Inst>,
    starred: &FxHashMap<VarId, usize>,
    call: &CallInst<'_>,
    previous: VarId,
) -> bool {
    let mut work: Vec<VarId> = call
        .recv
        .map(|(recv, _)| recv)
        .into_iter()
        .chain(call.args.iter().copied())
        .chain(destination_free(call.kwargs))
        .collect();
    let phis: FxHashMap<VarId, &heki_ir::PhiNode> = f
        .blocks
        .iter()
        .flat_map(|b| &b.phis)
        .map(|p| (p.dst, p))
        .collect();
    let mut derivations = Derivations::new(defs, starred, previous);
    let mut seen: FxHashSet<VarId> = work.iter().copied().collect();
    while let Some(v) = work.pop() {
        if v == previous {
            return true;
        }
        let next: Vec<VarId> = match (defs.get(&v), phis.get(&v)) {
            (Some(def), _) => held_operands(&def.kind, &mut derivations),
            (None, Some(phi)) => phi.incoming.iter().map(|(_, x)| *x).collect(),
            (None, None) => Vec::new(),
        };
        work.extend(next.into_iter().filter(|x| seen.insert(*x)));
    }
    false
}

/// The operands an instruction's result may hold. An operator holds its operands only as a
/// builtin container operation: sequence concatenation and repetition (`+`, `*`) and dict
/// union (`|`); the other operators never hold an array or a frame (sets and dict keys hold
/// only hashable values). With an operand computed entry by entry from the previous value
/// ([`FromPrevious::Entrywise`]) it holds neither: an array's or a frame's operator takes
/// precedence over a list's or a dict's and works on each entry, so `x - np.outer(u, v)` and
/// `w * x[1:] + c` are new arrays whose every entry changed. A loop that rebuilds its value
/// that way already does work in proportion to the value's size on every iteration, and there
/// are no unchanged parts to collect and combine once. An item of such a value is a number or
/// an array, so it is not held either, though a list it repeats is (`parts * q[0]`).
fn held_operands(kind: &InstKind, derivations: &mut Derivations<'_>) -> Vec<VarId> {
    match *kind {
        InstKind::BinOp {
            op: BinOpKind::Add | BinOpKind::Mult | BinOpKind::BitOr,
            lhs,
            rhs,
            ..
        } => {
            let operands = [(lhs, derivations.of(lhs)), (rhs, derivations.of(rhs))];
            if operands.iter().any(|(_, p)| *p == FromPrevious::Entrywise) {
                return Vec::new();
            }
            operands
                .into_iter()
                .filter(|(_, p)| *p != FromPrevious::Item)
                .map(|(v, _)| v)
                .collect()
        }
        InstKind::BinOp { .. } | InstKind::UnaryOp { .. } | InstKind::Compare { .. } => Vec::new(),
        ref other => other.operands(),
    }
}

/// How a value is computed from the loop's previous value, which is the result of a reallocating
/// call: an array or a frame of at least one dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FromPrevious {
    /// The previous value, a slice of one of these, or an operator result with one as an operand
    /// (but `@`, which makes a number of two vectors, and `not`): an array or a frame computed
    /// entry by entry.
    Entrywise,
    /// An item of an [`FromPrevious::Entrywise`] value or of another item (`q[0]`).
    Item,
    /// Any other value, which may hold the previous value.
    Other,
}

/// The [`FromPrevious`] of each value of one function, also through copies and unpacking
/// (`w, v = q[0], q[1:]` takes `v` from the display). Each answer is kept per value.
struct Derivations<'a> {
    defs: &'a FxHashMap<VarId, &'a Inst>,
    /// The first operand written with `*` of each display ([`first_starred`]).
    starred: &'a FxHashMap<VarId, usize>,
    previous: VarId,
    known: FxHashMap<VarId, FromPrevious>,
}

impl<'a> Derivations<'a> {
    fn new(
        defs: &'a FxHashMap<VarId, &'a Inst>,
        starred: &'a FxHashMap<VarId, usize>,
        previous: VarId,
    ) -> Self {
        Self {
            defs,
            starred,
            previous,
            known: FxHashMap::default(),
        }
    }

    /// How `v` is computed from the previous value. A value whose inputs lead back to itself
    /// (copies around dead code) takes its answer from the others.
    fn of(&mut self, v: VarId) -> FromPrevious {
        let mut visiting: FxHashSet<VarId> = FxHashSet::default();
        let mut stack = vec![(v, false)];
        while let Some((x, expanded)) = stack.pop() {
            if self.known.contains_key(&x) {
                continue;
            }
            if expanded {
                let answer = self.derive(x);
                self.known.insert(x, answer);
            } else {
                // Its inputs go above it, and a value being visited is not pushed again, so `x`
                // is known before any other entry of it comes off the stack.
                visiting.insert(x);
                stack.push((x, true));
                let inputs = self.inputs(x);
                stack.extend(
                    inputs
                        .into_iter()
                        .filter(|i| !self.known.contains_key(i) && !visiting.contains(i))
                        .map(|i| (i, false)),
                );
            }
        }
        self.known[&v]
    }

    /// The values [`Self::derive`] reads for `v`.
    fn inputs(&self, v: VarId) -> Vec<VarId> {
        match self.defs.get(&v).map(|inst| &inst.kind) {
            Some(InstKind::Copy { src, .. }) => vec![*src],
            Some(InstKind::SubscriptLoad {
                obj,
                index,
                is_slice,
                ..
            }) => vec![self.unpacked_item(*obj, *index, *is_slice).unwrap_or(*obj)],
            Some(InstKind::BinOp { op, lhs, rhs, .. }) if *op != BinOpKind::MatMult => {
                vec![*lhs, *rhs]
            }
            Some(InstKind::UnaryOp { op, operand, .. }) if *op != UnaryOpKind::Not => {
                vec![*operand]
            }
            _ => Vec::new(),
        }
    }

    /// How `v` is computed from the previous value, once its [`Self::inputs`] are known.
    fn derive(&self, v: VarId) -> FromPrevious {
        use FromPrevious::{Entrywise, Item, Other};
        if v == self.previous {
            return Entrywise;
        }
        let of = |x: &VarId| self.known.get(x).copied().unwrap_or(Other);
        match self.defs.get(&v).map(|inst| &inst.kind) {
            Some(InstKind::Copy { src, .. }) => of(src),
            Some(InstKind::SubscriptLoad {
                obj,
                index,
                is_slice,
                ..
            }) => match self.unpacked_item(*obj, *index, *is_slice) {
                Some(item) => of(&item),
                None => match (of(obj), is_slice) {
                    (Entrywise, true) => Entrywise,
                    (Entrywise | Item, _) => Item,
                    (Other, _) => Other,
                },
            },
            Some(InstKind::BinOp { op, lhs, rhs, .. })
                if *op != BinOpKind::MatMult && (of(lhs) == Entrywise || of(rhs) == Entrywise) =>
            {
                Entrywise
            }
            Some(InstKind::UnaryOp { op, operand, .. })
                if *op != UnaryOpKind::Not && of(operand) == Entrywise =>
            {
                Entrywise
            }
            _ => Other,
        }
    }

    /// The item an indexing at a constant position takes from a tuple or list display with no
    /// `*` up to that position.
    fn unpacked_item(&self, display: VarId, index: VarId, is_slice: bool) -> Option<VarId> {
        let kind = |v: VarId| self.defs.get(&v).map(|inst| &inst.kind);
        let (
            false,
            Some(InstKind::AllocBuiltin {
                kind: BuiltinKind::Tuple | BuiltinKind::List,
                elems,
                dst,
            }),
            Some(InstKind::Const {
                literal: ConstLiteral::Int(i),
                ..
            }),
        ) = (is_slice, kind(display), kind(index))
        else {
            return None;
        };
        let i = usize::try_from(*i).ok()?;
        let item = *elems.get(i)?;
        self.starred
            .get(dst)
            .is_none_or(|&first| first > i)
            .then_some(item)
    }
}

/// The first operand written with `*` (`IrFunction::starred`) of each display or call of `f`.
fn first_starred(f: &IrFunction) -> FxHashMap<VarId, usize> {
    let mut first: FxHashMap<VarId, usize> = FxHashMap::default();
    for &(dst, i) in &f.starred {
        let i = i as usize;
        first
            .entry(dst)
            .and_modify(|k| *k = (*k).min(i))
            .or_insert(i);
    }
    first
}

/// The keyword arguments of a call but `out=`, the destination buffer it writes into.
fn destination_free(kwargs: &[(Option<String>, VarId)]) -> impl Iterator<Item = VarId> + '_ {
    kwargs
        .iter()
        .filter(|(name, _)| name.as_deref() != Some("out"))
        .map(|(_, v)| *v)
}

/// Self-accumulating loop-carried values of `f` with their natural loop. `loop_carried`
/// entries name loops of the same function; one without its loop (an IR inconsistency) is
/// skipped instead of being matched against another loop.
fn accumulating_loops(f: &IrFunction) -> Vec<(&LoopCarriedInfo, &NaturalLoop)> {
    let mut out = Vec::new();
    for lc in f.loop_carried.iter().filter(|lc| lc.self_accumulating) {
        let Some(lp) = f.loops.iter().find(|l| l.header == lc.loop_header) else {
            continue;
        };
        out.push((lc, lp));
    }
    out
}
