//! `EFF109` (`lock-reacquired-while-held`, opt-in): a non-reentrant lock taken again while it is
//! held, by the function itself or by the calls it makes on its own thread or task.
//!
//! A lock is held from a `with` / `async with` over it to the `ExitWith` of that statement, and
//! from an awaited or blocking `acquire()` to a `release()`, certainly (on every path,
//! [`held::at_entry`]), and while nothing binds its name again: the holder itself, or a function
//! it calls (`heki_effect::ProjectIndex::site_rebinds`). The acquisitions a call makes come from
//! the callees' summaries (`LockFacts::acquires`, [`heki_effect::ProjectIndex::site_locks`]), in
//! the caller's names: the same `self.<attr>`, module global, or closure variable on both sides.
//!
//! An acquisition behind a test of the lock owner's state that the holder, or what it calls,
//! changes, or of a module global the holder sets, in the acquiring function or in one on the
//! way to it, is not reported: such code keeps a flag of its own to know whether it holds the
//! lock (`if self._starting: return`, `if _LIB is not None:
//! return _LIB`), which heki does not evaluate. Neither is a lock some function of the module
//! releases without holding it: another thread gives that lock back as a signal (a handoff),
//! so taking it again waits for that thread, by design.

use crate::{CheckContext, ModuleUnit};
use heki_ast::Span;
use heki_diagnostics::{Diagnostic, RelatedLocation};
use heki_effect::held;
use heki_effect::locks::{self, LockSteps, ObjRef, SiteLock, Taken};
use heki_effect::ownership;
use heki_effect::{CallSite, EffectSummary, FuncKey, TypeCtx, anchors_call_site};

/// Call sites of a module by `(caller qualname, span)` (`CheckContext::sites_by_span`).
type SitesBySpan<'s> = FxHashMap<(String, Span), Vec<&'s CallSite>>;
use heki_ir::walk::{Answer, FnShape};
use heki_ir::{BlockId, Inst, InstKind, IrFunction, VarId};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeSet;

/// A function's lock steps, in the blocks a run reaches (`live`).
struct Steps<'f> {
    shape: FnShape<'f>,
    live: ownership::Live,
    locks: LockSteps,
}

/// A lock as every function of a module can name it ([`handoff_key`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ModuleLock {
    /// A module global or a closure variable.
    Obj(ObjRef),
    /// An attribute of `self` in the methods of a class.
    Attr { class: String, attr: String },
}

/// How every function of the module names `lock` of `f`: a global or a closure variable by
/// itself, an attribute of `self` by its class; `None` for an attribute of another parameter.
fn handoff_key(f: &IrFunction, lock: &ObjRef) -> Option<ModuleLock> {
    match lock {
        ObjRef::Global(_) | ObjRef::Cell { .. } => Some(ModuleLock::Obj(lock.clone())),
        ObjRef::Param {
            param: 0,
            attr: Some(attr),
        } => Some(ModuleLock::Attr {
            class: f.owner_class_qualname.clone()?,
            attr: attr.clone(),
        }),
        ObjRef::Param { .. } => None,
    }
}

/// The locks some function of the module releases where it does not hold them: a signal
/// between threads.
fn handoffs(
    cx: &CheckContext<'_>,
    tcx: &TypeCtx<'_>,
    unit: &ModuleUnit<'_>,
    sites: &SitesBySpan<'_>,
) -> FxHashSet<ModuleLock> {
    let mut out = FxHashSet::default();
    for f in unit.input.ir.functions.iter().filter(|f| !f.is_class_body) {
        let releases =
            f.blocks.iter().flat_map(|b| &b.insts).any(
                |i| matches!(&i.kind, InstKind::MethodCall { method, .. } if method == "release"),
            );
        if !releases {
            continue;
        }
        let key = CheckContext::key(unit, f);
        let steps = lock_steps(cx, tcx, sites, f, &key);
        let entry = held::at_entry(&steps.shape, steps.locks.taken.len(), |b, i| {
            steps.locks.changes.get(&(b, i))
        });
        let reached = entry
            .into_iter()
            .enumerate()
            .filter_map(|(b, s)| Some((BlockId(b as u32), s?)));
        for (bb, mut state) in reached {
            for i in 0..f.block(bb).insts.len() {
                if let Some(lock) = steps.locks.releases.get(&(bb, i))
                    && !state
                        .ones()
                        .any(|k| steps.locks.taken.item(k).lock == *lock)
                {
                    out.extend(handoff_key(f, lock));
                }
                state.apply(steps.locks.changes.get(&(bb, i)));
            }
        }
    }
    out
}

pub(super) fn run(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.enabled("EFF109", unit) {
        return;
    }
    debug_assert!(
        cx.index.opt_in_facts().locks,
        "`opt_in_facts` decides for EFF109"
    );
    let tcx = TypeCtx::new(&unit.input, cx.decls, cx.kb);
    let mut sites = None;
    let mut signals = None;
    for f in unit.input.ir.functions.iter().filter(|f| !f.is_class_body) {
        // Only a function taking a lock itself can hold one.
        let takes = f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .any(|i| match &i.kind {
                InstKind::EnterWith { .. } => true,
                InstKind::MethodCall { method, .. } => method == "acquire",
                _ => false,
            });
        if !takes {
            continue;
        }
        let key = CheckContext::key(unit, f);
        let sites = sites.get_or_insert_with(|| cx.sites_by_span(unit));
        let steps = lock_steps(cx, &tcx, sites, f, &key);
        if steps.locks.taken.is_empty() {
            continue;
        }
        let signals = signals.get_or_insert_with(|| handoffs(cx, &tcx, unit, sites));
        let summary = cx.index.summary(&key);
        // The globals the function sets (`global _LIB; _LIB = lib`).
        let sets: FxHashSet<String> = f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|i| match &i.kind {
                InstKind::StoreGlobal { name, .. } if !f.is_module_body => {
                    Some(tcx.global_fqn(name))
                }
                _ => None,
            })
            .collect();
        let state_of = HolderState {
            summary,
            sets: &sets,
        };
        let entry = held::at_entry(&steps.shape, steps.locks.taken.len(), |b, i| {
            steps.locks.changes.get(&(b, i))
        });
        let mut reported: FxHashSet<(Span, ObjRef)> = FxHashSet::default();
        for (b, state) in entry.into_iter().enumerate() {
            let Some(mut state) = state else {
                continue;
            };
            let bb = BlockId(b as u32);
            if !steps.live.block(bb) {
                continue;
            }
            for (i, inst) in f.block(bb).insts.iter().enumerate() {
                if !state.is_empty() {
                    let own = steps.locks.takes.get(&(bb, i));
                    for (lock, chain) in retakes(cx, unit, sites, &key, &state_of, own, inst) {
                        if handoff_key(f, &lock).is_some_and(|k| signals.contains(&k)) {
                            continue;
                        }
                        // The outermost acquisition of the lock names the report.
                        let Some(k) = state
                            .ones()
                            .filter(|k| steps.locks.taken.item(*k).lock == lock)
                            .min_by_key(|k| steps.locks.taken.span(*k).start)
                        else {
                            continue;
                        };
                        if reported.insert((inst.span, lock.clone())) {
                            let held = (steps.locks.taken.span(k), steps.locks.taken.item(k));
                            report(cx, unit, f, &key, inst.span, held, chain, out);
                        }
                    }
                }
                state.apply(steps.locks.changes.get(&(bb, i)));
            }
        }
    }
}

/// The lock acquisitions of `f` and what each instruction does to them, the names its calls
/// may bind among them.
fn lock_steps<'f>(
    cx: &CheckContext<'_>,
    tcx: &TypeCtx<'_>,
    sites: &SitesBySpan<'_>,
    f: &'f IrFunction,
    key: &FuncKey,
) -> Steps<'f> {
    let defs: FxHashMap<VarId, &Inst> = f
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .filter_map(|i| Some((i.kind.dst()?, i)))
        .collect();
    let def = |v: VarId| defs.get(&v).copied();
    let types = tcx.infer(f);
    let live = ownership::Live::of(f, &defs);
    let rebound = |inst: &Inst| -> Vec<ObjRef> {
        let own = locks::rebound_name(tcx, f, key, &types, &def, &inst.kind);
        let mut names: Vec<ObjRef> = own.into_iter().collect();
        if anchors_call_site(&inst.kind) {
            let here = sites.get(&(key.qualname.clone(), inst.span));
            names.extend((here.into_iter().flatten()).flat_map(|site| cx.index.site_rebinds(site)));
        }
        names
    };
    let locks = locks::lock_steps(tcx, f, key, &types, &def, &live, &rebound);
    Steps {
        shape: FnShape::new(f),
        live,
        locks,
    }
}

/// The locks `inst` takes, with the calls to where it takes each (empty for its own
/// acquisition), while `state`'s locks may be held: its own acquisition, and those its call
/// sites' callees make.
#[allow(clippy::too_many_arguments)]
fn retakes(
    cx: &CheckContext<'_>,
    unit: &ModuleUnit<'_>,
    sites: &SitesBySpan<'_>,
    key: &FuncKey,
    holder: &HolderState<'_>,
    own: Option<&ObjRef>,
    inst: &Inst,
) -> Vec<(ObjRef, Vec<heki_effect::CallHop>)> {
    let mut out = Vec::new();
    if let Some(lock) = own {
        out.push((lock.clone(), Vec::new()));
    }
    if anchors_call_site(&inst.kind) {
        for site in sites
            .get(&(key.qualname.clone(), inst.span))
            .into_iter()
            .flatten()
        {
            for found in cx.index.site_locks(&unit.input.path, site) {
                if !holder.guards(cx, &found) {
                    out.push((found.lock, found.chain));
                }
            }
        }
    }
    out
}

/// What the holder changes: its facts (`summary`) and the globals it sets.
struct HolderState<'s> {
    summary: Option<&'s EffectSummary>,
    sets: &'s FxHashSet<String>,
}

impl HolderState<'_> {
    /// Whether an acquisition is behind a test of state the holder changes: a module global it
    /// sets, or, for a lock a parameter holds, an attribute of that object it (or what it
    /// calls) changes; or behind more tests than heki keeps.
    fn guards(&self, cx: &CheckContext<'_>, found: &SiteLock) -> bool {
        // Open: every test may be one of state the holder changes, so no finding.
        let Answer::Known(tests) = &found.tests else {
            return true;
        };
        if tests.globals.iter().any(|g| self.sets.contains(g)) {
            return true;
        }
        let (
            ObjRef::Param {
                param,
                attr: Some(_),
            },
            Some(s),
        ) = (&found.lock, self.summary)
        else {
            return false;
        };
        let mut tested: BTreeSet<&str> = tests.attrs.iter().map(String::as_str).collect();
        let callees = tests.calls.iter().filter_map(|k| cx.index.summary(k));
        tested.extend(callees.flat_map(|c| c.reads_self_attrs.iter().map(|r| r.attr.as_str())));
        let writes_param = s
            .mutated_param_paths
            .get(param)
            .is_some_and(|paths| paths.iter().any(|a| tested.contains(a.as_str())));
        let writes_self = *param == 0
            && s.owner_class_qualname.is_some()
            && s.mutates_self_attrs
                .iter()
                .any(|w| tested.contains(w.attr.as_str()));
        writes_param || writes_self
    }
}

#[allow(clippy::too_many_arguments)]
fn report(
    cx: &CheckContext<'_>,
    unit: &ModuleUnit<'_>,
    f: &IrFunction,
    key: &FuncKey,
    span: Span,
    (taken, held): (Span, &Taken),
    chain: Vec<heki_effect::CallHop>,
    out: &mut Vec<Diagnostic>,
) {
    let name = held.lock.name_in(f);
    let line = unit.lines.line_col(taken.start).0;
    let class = &held.class;
    let (message, target, help) = match chain.first() {
        None => (
            format!(
                "`{}` takes `{name}` again while it holds it (taken at line {line}); `{class}` is \
                 not reentrant, so this waits forever",
                key.qualname
            ),
            format!("{name}->again"),
            "take the lock once: drop the inner acquisition",
        ),
        Some(first) => (
            format!(
                "`{}` calls `{}`, which takes `{name}` while `{}` holds it (taken at line \
                 {line}); `{class}` is not reentrant, so this waits forever",
                key.qualname, first.callee, key.qualname
            ),
            format!("{name}->{}", first.callee),
            "call a variant that expects the lock to be held, or make the call after releasing \
             the lock",
        ),
    };
    let d = cx
        .emit(
            out,
            "EFF109",
            unit,
            span,
            &key.qualname,
            target,
            message,
            Some(help.to_owned()),
            &[],
        )
        .expect("`run` checked that EFF109 runs on the unit");
    let mut related: Vec<RelatedLocation> = vec![cx.related_at(
        &unit.input.path,
        taken,
        format!("`{}` takes `{name}` here", key.qualname),
    )];
    if let Some((last, calls)) = chain.split_last() {
        for hop in calls {
            related.push(cx.related_at(
                &hop.path,
                hop.span,
                format!("`{}` calls `{}`", hop.caller.qualname, hop.callee),
            ));
        }
        related.push(cx.related_at(
            &last.path,
            last.span,
            format!("`{}` takes the lock again here", last.caller.qualname),
        ));
    }
    d.related = related;
}
