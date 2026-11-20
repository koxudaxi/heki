//! `MUT106` (`iterator-double-consumption`, opt-in): a single-pass iterator or generator (an
//! SSA value that is not rebound) consumed a second time; the second pass sees no items.
//!
//! The first consumption must run to exhaustion (a `for` loop leaving early is the partial-pass
//! idiom), and the second must be reachable from where the first finished along normal control
//! flow without passing the source's definition again (exclusive branches never pair). A source
//! defined outside a loop and consumed to exhaustion inside it is exhausted on the next
//! iteration.

use crate::{CheckContext, ModuleUnit};
use heki_ast::Span;
use heki_diagnostics::Diagnostic;
use heki_effect::consumption::{IterFlow, Roots};
use heki_effect::{CallSite, Callee, EffectSummary, Facts, SiteRole, TypeCtx};
use heki_ir::{BlockId, InstKind, IrFunction, LoopBound, NaturalLoop, VarId};
use rustc_hash::{FxHashMap, FxHashSet};

/// Call sites of a module by `(caller qualname, span)` ([`CheckContext::sites_by_span`]).
type SitesBySpan<'a> = FxHashMap<(String, Span), Vec<&'a CallSite>>;

pub(super) fn run(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.enabled("MUT106", unit) {
        return;
    }
    let tcx = TypeCtx::new(&unit.input, cx.decls, cx.kb);
    let sites = cx.sites_by_span(unit);
    for f in unit.input.ir.functions.iter().filter(|f| !f.is_class_body) {
        let types = tcx.infer(f);
        let qualname = CheckContext::key(unit, f).qualname;
        // The project callees the call sites record (`site_call`).
        let recorded = |inst: &heki_ir::Inst| {
            let at = sites
                .get(&(qualname.clone(), inst.span))
                .map_or(&[][..], Vec::as_slice);
            Some(super::site_call(f, at, inst, false)?.callee)
        };
        let mut flow = IterFlow::new(f, &tcx, &types, Roots::SinglePass, &recorded);
        if flow.root_of.is_empty() {
            continue;
        }
        add_callee_consumption(cx, &sites, &qualname, f, &mut flow);
        report(cx, unit, f, &flow, out);
    }
}

/// Whether the body of the project callee `key` runs at the call at `span` of `qualname`, by its
/// call site ([`heki_effect::ProjectIndex::delivery`]): an `async def` that is neither awaited
/// nor driven, a generator function, and a function an offloading decorator runs on another
/// thread do not run there.
fn body_runs<'a>(
    cx: &CheckContext<'a>,
    sites: &SitesBySpan<'a>,
    qualname: &str,
    span: Span,
    key: &heki_effect::FuncKey,
) -> Option<&'a EffectSummary> {
    let summary = cx.index.summary(key)?;
    sites
        .get(&(qualname.to_owned(), span))?
        .iter()
        .filter(|s| s.role == SiteRole::Call)
        .filter(|s| s.project_callees().iter().any(|bc| bc.callee == *key))
        .any(|s| {
            cx.index
                .delivery(s, summary)
                .is_some_and(|d| d.from == Facts::Body)
        })
        .then_some(summary)
}

/// Calls of project functions that iterate a parameter to exhaustion consume the argument (every
/// CHA candidate must, and its body must run at the call).
fn add_callee_consumption(
    cx: &CheckContext<'_>,
    sites: &SitesBySpan<'_>,
    qualname: &str,
    f: &IrFunction,
    flow: &mut IterFlow,
) {
    let mut found = Vec::new();
    for bb in &f.blocks {
        for (index, inst) in bb.insts.iter().enumerate() {
            let (recv, args, kwargs, star_from) = match &inst.kind {
                InstKind::MethodCall {
                    recv,
                    args,
                    kwargs,
                    star_from,
                    ..
                } => (Some(*recv), args, kwargs, star_from),
                InstKind::Call {
                    args,
                    kwargs,
                    star_from,
                    ..
                } => (None, args, kwargs, star_from),
                _ => continue,
            };
            // Past a `*` argument, the parameter an argument binds is not known.
            let positional =
                &args[..star_from.map_or(args.len(), |s| usize::from(s).min(args.len()))];
            let Some(Callee::Project(keys, binds_recv)) = flow.callees.get(&(bb.id, index)) else {
                continue;
            };
            let mut consumed: Option<FxHashSet<VarId>> = None;
            for key in keys {
                // An abstract method never runs: an override does.
                if cx.index.summary(key).is_some_and(|s| s.is_abstract()) {
                    continue;
                }
                let summary = body_runs(cx, sites, qualname, inst.span, key);
                let decl = cx.decls.function_decl(key).cloned().unwrap_or_default();
                let mask = summary.map_or(0, |s| s.consumes_params);
                let offset = usize::from(*binds_recv && !decl.is_staticmethod);
                let mut bound: Vec<(usize, VarId)> = Vec::new();
                if offset == 1 && !decl.is_classmethod {
                    bound.extend(recv.map(|r| (0, r)));
                }
                bound.extend(positional.iter().enumerate().map(|(i, v)| (i + offset, *v)));
                for (name, v) in kwargs {
                    let pos = name
                        .as_ref()
                        .and_then(|n| decl.param_names.iter().position(|p| p == n));
                    bound.extend(pos.map(|p| (p, *v)));
                }
                let here: FxHashSet<VarId> = bound
                    .into_iter()
                    .filter(|(p, _)| *p < 64 && mask & (1u64 << p) != 0)
                    .map(|(_, v)| v)
                    .collect();
                consumed = Some(match consumed {
                    Some(prev) => prev.intersection(&here).copied().collect(),
                    None => here,
                });
            }
            let mut values: Vec<VarId> = consumed.into_iter().flatten().collect();
            values.sort();
            found.push((bb.id, index, inst.span, values));
        }
    }
    for (block, index, span, values) in found {
        flow.add_full(block, index, span, &values);
    }
}

/// Why a consumption sees an exhausted iterator.
enum Prior {
    /// Consumed to exhaustion before, at this span.
    Before(Span),
    /// Consumed to exhaustion by the previous iteration of this loop (header block).
    Loop(BlockId),
}

fn report(
    cx: &CheckContext<'_>,
    unit: &ModuleUnit<'_>,
    f: &IrFunction,
    flow: &IterFlow,
    out: &mut Vec<Diagnostic>,
) {
    let qualname = CheckContext::key(unit, f).qualname;
    for (j, c2) in flow.events.iter().enumerate() {
        let doubled: Vec<(VarId, Prior)> = c2
            .roots
            .iter()
            .filter_map(|&s| prior(f, flow, j, s).map(|p| (s, p)))
            .collect();
        for (s, why) in &doubled {
            // `list(e)` twice with `e = enumerate(rows)`: report `e`, not also `rows`.
            let inner = doubled.iter().any(|(o, _)| {
                o != s
                    && flow
                        .wraps
                        .get(o)
                        .is_some_and(|w| flow.expand(w).contains(s))
            });
            let Some(name) = name_of(f, flow, *s).filter(|_| !inner) else {
                continue;
            };
            let line = |span: Span| unit.lines.line_col(span.start).0;
            let how = match why {
                Prior::Before(span) => format!("already consumed at line {}", line(*span)),
                // The header evaluates the loop's target or condition on the loop's own line;
                // `while flag:` over a local evaluates nothing there.
                Prior::Loop(header) => match f.block(*header).insts.first() {
                    Some(i) => format!(
                        "consumed by the previous iteration of the loop at line {}",
                        line(i.span)
                    ),
                    None => "consumed by the previous iteration of its loop".to_owned(),
                },
            };
            cx.emit(
                out,
                "MUT106",
                unit,
                c2.span,
                &qualname,
                name.clone(),
                format!("`{name}` is a single-pass iterator {how}, so this pass sees no items"),
                Some(
                    "build a `list` once to iterate it more than once, or create a new iterator for each pass"
                        .to_owned(),
                ),
                &[],
            );
        }
    }
}

/// The earlier exhaustion of root `s` that consumption `j` follows, if any.
fn prior(f: &IrFunction, flow: &IterFlow, j: usize, s: VarId) -> Option<Prior> {
    let c2 = &flow.events[j];
    let def_block = flow.def_at.get(&s).map(|(b, _)| *b);
    let before = flow
        .events
        .iter()
        .enumerate()
        .filter(|(i, c1)| *i != j && c1.full && c1.roots.contains(&s))
        .filter(|(_, c1)| reaches(f, c1.done, (c2.block, c2.index), def_block))
        .map(|(_, c1)| c1.span)
        .min();
    if let Some(span) = before {
        return Some(Prior::Before(span));
    }
    if !c2.full {
        return None;
    }
    // The innermost loop that repeats the consumption on the same object.
    f.loops
        .iter()
        .filter(|lp| lp.body.contains(&c2.block))
        .filter(|lp| def_block.is_none_or(|d| !lp.body.contains(&d)))
        .filter(|lp| !matches!(lp.bound, LoopBound::ConstSmall(0 | 1)))
        .filter(|lp| !iterates(f, flow, lp, s))
        .filter(|lp| back_to_header(f, lp, c2.done))
        .max_by_key(|lp| lp.depth)
        .map(|lp| Prior::Loop(lp.header))
}

/// The loop's own iterable consumes `s`: consuming the rest inside it ends the loop.
fn iterates(f: &IrFunction, flow: &IterFlow, lp: &NaturalLoop, s: VarId) -> bool {
    let consumes_s = |iter: VarId| {
        f.def_inst(iter).is_some_and(|(_, d)| {
            matches!(&d.kind, InstKind::MakeIter { iterable, .. } if flow.expand(&[*iterable]).contains(&s))
        })
    };
    f.block(lp.header)
        .insts
        .iter()
        .any(|i| matches!(i.kind, InstKind::IterNext { iter, .. } if consumes_s(iter)))
}

/// Normal-flow successors of `b`, without the back edges of loops that run at most once.
fn successors(f: &IrFunction, b: BlockId) -> Vec<BlockId> {
    let once = |s: &BlockId| {
        f.loops.iter().any(|lp| {
            lp.header == *s
                && lp.body.contains(&b)
                && matches!(lp.bound, LoopBound::ConstSmall(0 | 1))
        })
    };
    let mut out = f.block(b).terminator.successors();
    out.retain(|s| !once(s));
    out
}

/// Whether the program point `to` is reachable from the point `from` without entering `kill`
/// (the source's defining block: going around it rebinds a fresh object).
fn reaches(
    f: &IrFunction,
    from: (BlockId, usize),
    to: (BlockId, usize),
    kill: Option<BlockId>,
) -> bool {
    if from.0 == to.0 && from.1 <= to.1 {
        return true;
    }
    let mut seen: FxHashSet<BlockId> = FxHashSet::default();
    let mut stack = successors(f, from.0);
    while let Some(b) = stack.pop() {
        if Some(b) == kill || !seen.insert(b) {
            continue;
        }
        if b == to.0 {
            return true;
        }
        stack.extend(successors(f, b));
    }
    false
}

/// Whether the loop's header is reachable from `from` without leaving the loop.
fn back_to_header(f: &IrFunction, lp: &NaturalLoop, from: (BlockId, usize)) -> bool {
    let mut seen: FxHashSet<BlockId> = FxHashSet::default();
    let mut stack = successors(f, from.0);
    while let Some(b) = stack.pop() {
        if b == lp.header {
            return true;
        }
        if lp.body.contains(&b) && seen.insert(b) {
            stack.extend(successors(f, b));
        }
    }
    false
}

/// The source's variable name: its own (a parameter, an annotated local) or the first name it
/// is bound to. Unnamed temporaries are never reported.
fn name_of(f: &IrFunction, flow: &IterFlow, s: VarId) -> Option<String> {
    let proper = |v: VarId| f.var_name(v).filter(|n| !n.starts_with('.'));
    if let Some(n) = proper(s) {
        return Some(n.to_owned());
    }
    f.blocks
        .iter()
        .flat_map(|b| &b.insts)
        .filter_map(|i| match i.kind {
            InstKind::Copy { dst, .. } if flow.root_of.get(&dst) == Some(&s) => {
                proper(dst).map(|n| (i.span, n))
            }
            _ => None,
        })
        .min_by_key(|(span, _)| span.start)
        .map(|(_, n)| n.to_owned())
}
