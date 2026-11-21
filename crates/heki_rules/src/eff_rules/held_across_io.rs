//! `EFF106` (`lock-or-tx-held-across-io`, opt-in): an `await` or blocking network I/O while a
//! type-certain synchronous lock or a DB transaction is held by a `with` block.
//!
//! A manager is held when `TypeCtx` types it as an instance of a Layer 2 class flagged
//! `sync_lock_or_tx` (`threading.Lock`, ...) or when it is the result of a call resolving to a
//! flagged Layer 2 symbol (`sqlalchemy.orm.Session.begin`, `django.db.transaction.atomic`, ...).
//! `async with` is never considered. The held region is a forward must-dataflow over the CFG
//! (exception edges included): each acquisition is held after its `EnterWith` and released at
//! the `ExitWith` of its own `with` statement, joined by intersection. A re-entrant lock entered
//! again by a nested `with` stays held when the inner block exits.

use super::{relaxed, shortest};
use crate::{CheckContext, ModuleUnit};
use heki_ast::Span;
use heki_diagnostics::Diagnostic;
use heki_effect::consumption::resolve_inst;
use heki_effect::held::{self, Acquisitions, Change};
use heki_effect::{CallHop, CallSite, Callee, SiteEffect, TypeCtx, Val, anchors_call_site};
use heki_ir::walk::FnShape;
use heki_ir::{BlockId, Inst, InstKind, IrFunction, VarId};
use heki_kb::ExecEffectKind;
use rustc_hash::FxHashMap;

/// Call sites of a module by `(caller qualname, span)` ([`CheckContext::sites_by_span`]).
type SitesBySpan<'a> = FxHashMap<(String, Span), Vec<&'a CallSite>>;

/// What a held manager holds: the flagged symbol.
#[derive(Debug)]
struct Held {
    symbol: String,
    /// A transaction starter's result (`false`: a lock instance).
    transaction: bool,
}

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
        let shape = FnShape::new(f);
        let held = held_managers(&tcx, &shape, &types);
        if held.is_empty() {
            continue;
        }
        let sites = sites.get_or_insert_with(|| cx.sites_by_span(unit));
        let (taken, changes) = acquisitions(f, &held);
        let reached = held::at_entry(&shape, taken.len(), |b, i| changes.get(&(b, i)))
            .into_iter()
            .enumerate()
            .filter_map(|(b, s)| s.map(|s| (BlockId(b as u32), s)));
        for (bb, mut state) in reached {
            for (i, inst) in f.block(bb).insts.iter().enumerate() {
                // The outermost resource held here names the report.
                if let Some(k) = state.ones().min_by_key(|k| taken.span(*k).start)
                    && let Some(offense) = offense(cx, unit, sites, &key.qualname, inst)
                {
                    let mgr = *taken.item(k);
                    let line = unit.lines.line_col(taken.span(k).start).0;
                    let Report {
                        target,
                        message,
                        help,
                        chain,
                    } = describe(&key.qualname, &held[&mgr], line, offense);
                    cx.emit(
                        out,
                        "EFF106",
                        unit,
                        inst.span,
                        &key.qualname,
                        target,
                        message,
                        Some(help.to_owned()),
                        &chain,
                    );
                }
                state.apply(changes.get(&(bb, i)));
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

struct Report {
    target: String,
    message: String,
    help: &'static str,
    chain: Vec<CallHop>,
}

/// The report of `offense` in `qualname` while `held` (taken at `line`) is held.
fn describe(qualname: &str, held: &Held, line: u32, offense: Offense) -> Report {
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
        Offense::Await => Report {
            target: format!("{}->await", held.symbol),
            message: format!("`async def {qualname}` awaits while {holding}"),
            help: if held.transaction {
                "move the `await` before or after the transaction"
            } else {
                "move the `await` out of the `with` block and hold the lock only for the update"
            },
            chain: Vec::new(),
        },
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
            Report {
                target: format!("{}->{root}", held.symbol),
                message: format!("`{qualname}` {call} while {holding}"),
                help: if held.transaction {
                    "make the request before or after the transaction"
                } else {
                    "make the request before taking the lock and hold it only for the update"
                },
                chain,
            }
        }
    }
}

/// Managers of non-async `with` items that hold a flagged lock or transaction.
fn held_managers(
    tcx: &TypeCtx<'_>,
    shape: &FnShape<'_>,
    types: &FxHashMap<VarId, Val>,
) -> FxHashMap<VarId, Held> {
    let f = shape.f;
    let flagged = |symbol: &str| tcx.catalog(symbol).is_some_and(|e| e.sync_lock_or_tx);
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
            let def = shape.inst(shape.copy_root(mgr))?;
            match resolve_inst(tcx, types, &def.kind) {
                Callee::External(symbol) if flagged(&symbol) => Some((symbol, true)),
                _ => None,
            }
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

/// The acquisitions of `held` managers by `with` items (each holding its manager), and what
/// each instruction does to them: an `EnterWith` takes one, and an `ExitWith`, which spans its
/// `with` statement, gives back those of its manager by the items inside that statement.
fn acquisitions(
    f: &IrFunction,
    held: &FxHashMap<VarId, Held>,
) -> (Acquisitions<VarId>, FxHashMap<(BlockId, usize), Change>) {
    let mut taken = Acquisitions::default();
    let mut changes = FxHashMap::default();
    for bb in &f.blocks {
        for (i, inst) in bb.insts.iter().enumerate() {
            if let InstKind::EnterWith { mgr, .. } = inst.kind
                && held.contains_key(&mgr)
            {
                let k = taken.add(inst.span, Some(mgr), mgr);
                changes.insert((bb.id, i), Change::Take(k));
            }
        }
    }
    for bb in &f.blocks {
        for (i, inst) in bb.insts.iter().enumerate() {
            if let InstKind::ExitWith { mgr, .. } = inst.kind {
                changes.insert((bb.id, i), Change::Give(taken.exited(mgr, inst.span)));
            }
        }
    }
    (taken, changes)
}
