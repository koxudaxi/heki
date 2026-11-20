//! `EFF106` (`lock-or-tx-held-across-io`, opt-in): an `await` or blocking network I/O while a
//! type-certain synchronous lock or a DB transaction is held by a `with` block.
//!
//! A manager is held when `TypeCtx` types it as an instance of a Layer 2 class flagged
//! `sync_lock_or_tx` (`threading.Lock`, ...) or when it is the result of a call resolving to a
//! flagged Layer 2 symbol (`sqlalchemy.orm.Session.begin`, `django.db.transaction.atomic`, ...).
//! `async with` is never considered. The held region is a forward must-dataflow over the CFG
//! (exception edges included): held after `EnterWith`, released at the matching `ExitWith`,
//! joined by intersection.

use super::{relaxed, shortest};
use crate::{CheckContext, ModuleUnit};
use heki_ast::Span;
use heki_diagnostics::Diagnostic;
use heki_effect::consumption::resolve_inst;
use heki_effect::{CallHop, CallSite, Callee, SiteEffect, TypeCtx, Val, anchors_call_site};
use heki_ir::{BlockId, Inst, InstKind, IrFunction, VarId};
use heki_kb::ExecEffectKind;
use rustc_hash::FxHashMap;
use std::collections::BTreeMap;

/// Call sites of a module by `(caller qualname, span)` ([`CheckContext::sites_by_span`]).
type SitesBySpan<'a> = FxHashMap<(String, Span), Vec<&'a CallSite>>;

/// What a held manager holds: the flagged symbol.
#[derive(Debug)]
struct Held {
    symbol: String,
    /// A transaction starter's result (`false`: a lock instance).
    transaction: bool,
}

/// Managers certainly held at a program point: `mgr -> span of the acquiring with item`.
type State = BTreeMap<VarId, Span>;

pub(super) fn run(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.enabled("EFF106", unit) {
        return;
    }
    let relax_tests = cx.config.lint.relax_async_in_tests.unwrap_or(true);
    let tcx = TypeCtx::new(&unit.input, cx.decls, cx.kb);
    let mut sites = None;
    for f in unit.input.ir.functions.iter().filter(|f| !f.is_class_body) {
        let enters_sync_with = f.blocks.iter().flat_map(|b| &b.insts).any(|i| {
            matches!(
                i.kind,
                InstKind::EnterWith {
                    is_async: false,
                    ..
                }
            )
        });
        let key = CheckContext::key(unit, f);
        if !enters_sync_with
            || cx
                .index
                .summary(&key)
                .is_some_and(|s| relaxed(s, &key, unit, relax_tests))
        {
            continue;
        }
        let types = tcx.infer(f);
        let held = held_managers(cx, &tcx, f, &types);
        if held.is_empty() {
            continue;
        }
        let sites = sites.get_or_insert_with(|| cx.sites_by_span(unit));
        let reached = held_at_entry(f, &held)
            .into_iter()
            .enumerate()
            .filter_map(|(b, s)| s.map(|s| (BlockId(b as u32), s)));
        for (bb, mut state) in reached {
            for inst in &f.block(bb).insts {
                // The outermost resource held here names the report.
                if let Some((&mgr, &taken)) = state.iter().min_by_key(|(_, sp)| sp.start)
                    && let Some(offense) = offense(cx, unit, sites, &key.qualname, inst)
                {
                    let line = unit.lines.line_col(taken.start).0;
                    let (target, message, chain) =
                        describe(&key.qualname, &held[&mgr], line, offense);
                    cx.emit(
                        out,
                        "EFF106",
                        unit,
                        inst.span,
                        &key.qualname,
                        target,
                        message,
                        &chain,
                    );
                }
                transfer(&inst.kind, &held, inst.span, &mut state);
            }
        }
    }
}

/// What an instruction does while a resource is held.
enum Offense {
    Await,
    /// Blocking network I/O, with the shortest chain from the call to the sink.
    Net(Vec<CallHop>),
}

fn offense(
    cx: &CheckContext<'_>,
    unit: &ModuleUnit<'_>,
    sites: &SitesBySpan<'_>,
    qualname: &str,
    inst: &Inst,
) -> Option<Offense> {
    match &inst.kind {
        // An `async with` held across already suspended at its entry, which this lock region
        // covers too: its exit is not reported again.
        InstKind::ExitWith { .. } => None,
        kind if kind.suspends() => Some(Offense::Await),
        // Reading a property runs its getter (a call site at the read); iterating a generator
        // runs its body (a call site at the iteration).
        kind if anchors_call_site(kind) => {
            // An awaited `async def` runs here too: its `await` is the report.
            let effects: Vec<SiteEffect> = sites
                .get(&(qualname.to_owned(), inst.span))
                .into_iter()
                .flatten()
                .flat_map(|s| cx.index.site_effects(&unit.input.path, s))
                .filter(|e| !e.callee_is_async)
                .collect();
            shortest(&effects, ExecEffectKind::BlockingNet).map(|c| Offense::Net(c.to_vec()))
        }
        _ => None,
    }
}

/// `(target, message, related chain)` of a report in `qualname` while `held` (taken at `line`)
/// is held.
fn describe(
    qualname: &str,
    held: &Held,
    line: u32,
    offense: Offense,
) -> (String, String, Vec<CallHop>) {
    let holding = if held.transaction {
        format!(
            "the database transaction from `{}` (line {line}) is open",
            held.symbol
        )
    } else {
        format!(
            "holding the synchronous `{}` taken at line {line}",
            held.symbol
        )
    };
    match offense {
        Offense::Await => {
            let cost = if held.transaction {
                "the transaction and its pooled connection stay held while the coroutine is suspended"
            } else {
                "other threads block and the event loop can deadlock"
            };
            (
                format!("{}->await", held.symbol),
                format!("`async def {qualname}` awaits while {holding}; {cost}"),
                Vec::new(),
            )
        }
        Offense::Net(chain) => {
            let root = &chain[chain.len() - 1].callee;
            let call = if chain.len() == 1 {
                format!("calls `{root}` (blocking network I/O)")
            } else {
                format!(
                    "calls `{}`, which reaches blocking network I/O `{root}`,",
                    chain[0].callee
                )
            };
            let cost = if held.transaction {
                "the transaction and its pooled connection stay held for the whole request"
            } else {
                "every thread waiting for the lock stalls for the whole request"
            };
            (
                format!("{}->{root}", held.symbol),
                format!("`{qualname}` {call} while {holding}; {cost}"),
                chain,
            )
        }
    }
}

/// Managers of non-async `with` items that hold a flagged lock or transaction.
fn held_managers(
    cx: &CheckContext<'_>,
    tcx: &TypeCtx<'_>,
    f: &IrFunction,
    types: &FxHashMap<VarId, Val>,
) -> FxHashMap<VarId, Held> {
    let flagged = |symbol: &str| cx.kb.lookup(symbol).is_some_and(|e| e.sync_lock_or_tx);
    let mut out = FxHashMap::default();
    for inst in f.blocks.iter().flat_map(|b| &b.insts) {
        let InstKind::EnterWith {
            mgr,
            is_async: false,
            ..
        } = inst.kind
        else {
            continue;
        };
        let lock = match types.get(&mgr) {
            Some(Val::Inst(class)) if flagged(class) => Some((class.clone(), false)),
            _ => None,
        };
        let held = lock.or_else(|| {
            let mut cur = mgr;
            while let Some((_, def)) = f.def_inst(cur) {
                match &def.kind {
                    InstKind::Copy { src, .. } => cur = *src,
                    kind => {
                        return match resolve_inst(tcx, types, kind) {
                            Callee::External(symbol) if flagged(&symbol) => Some((symbol, true)),
                            _ => None,
                        };
                    }
                }
            }
            None
        });
        if let Some((symbol, transaction)) = held {
            out.entry(mgr).or_insert(Held {
                symbol,
                transaction,
            });
        }
    }
    out
}

/// Apply one instruction to the held set.
fn transfer(kind: &InstKind, held: &FxHashMap<VarId, Held>, span: Span, state: &mut State) {
    match kind {
        InstKind::EnterWith { mgr, .. } if held.contains_key(mgr) => {
            state.entry(*mgr).or_insert(span);
        }
        InstKind::ExitWith { mgr, .. } => {
            state.remove(mgr);
        }
        _ => {}
    }
}

/// The managers certainly held on entry to each block (`None`: unreachable), a forward
/// must-dataflow joined by intersection. Along an exception edge only what the block holds at
/// every one of its instructions flows.
fn held_at_entry(f: &IrFunction, held: &FxHashMap<VarId, Held>) -> Vec<Option<State>> {
    let mut ins: Vec<Option<State>> = vec![None; f.blocks.len()];
    ins[0] = Some(State::new());
    let rpo = f.rpo_blocks();
    let meet = |slot: &mut Option<State>, incoming: &State| -> bool {
        let next = match slot {
            Some(cur) => cur
                .iter()
                .filter(|(k, _)| incoming.contains_key(k))
                .map(|(k, v)| (*k, (*v).min(incoming[k])))
                .collect(),
            None => incoming.clone(),
        };
        let changed = slot.as_ref() != Some(&next);
        *slot = Some(next);
        changed
    };
    loop {
        let mut changed = false;
        // Reverse postorder reaches every block from an already visited predecessor first.
        for &b in &rpo {
            let mut state = ins[b.0 as usize].clone().unwrap_or_default();
            let bb = f.block(b);
            let mut throughout = state.clone();
            for inst in &bb.insts {
                transfer(&inst.kind, held, inst.span, &mut state);
                throughout.retain(|k, _| state.contains_key(k));
            }
            for s in bb.terminator.successors() {
                changed |= meet(&mut ins[s.0 as usize], &state);
            }
            if let Some(e) = bb.exc_target {
                changed |= meet(&mut ins[e.0 as usize], &throughout);
            }
        }
        if !changed {
            return ins;
        }
    }
}
