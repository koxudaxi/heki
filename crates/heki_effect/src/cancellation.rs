//! `EFF118`: where a function's awaits are shielded from the cancellation of the AnyIO or Trio
//! scopes around it, and where an await delivers a cancellation on purpose.
//!
//! - **Shields**: a `with` over a catalog cancel scope (`heki_kb::EcosystemEntry::cancel_scope`)
//!   made in the function with a constant `shield=True` shields what runs inside it. One made
//!   with a shield argument that is not a constant, one whose `shield` attribute the function
//!   sets, and one the function did not make (an attribute, a parameter) may shield or not: the
//!   answer is open. Held as a forward must-dataflow over the `with` regions ([`held`]), so a
//!   point counts as shielded only when every path into it entered a shield.
//! - **Re-delivery**: `lowlevel.checkpoint()` and `checkpoint_if_cancelled()` do no work of
//!   their own; awaited where nothing runs after them they hand the cancellation on to the
//!   caller, which is the point of awaiting them in a cleanup ([`runs_nothing_after`]).

use crate::resolver::{Callee, TypeCtx, Val};
use crate::{ShieldAt, consumption::resolve_inst, held};
use heki_ir::walk::{Answer, Budget, FnShape, Visit, reach};
use heki_ir::{BlockId, ConstLiteral, Inst, InstKind, VarId};
use heki_kb::{ArgSlot, EcosystemEntry};
use rustc_hash::{FxHashMap, FxHashSet};

/// The catalog checkpoints that do no work of their own: awaiting one only lets a cancellation
/// in, which a cleanup does on purpose as its last step.
pub const REDELIVERIES: &[&str] = &[
    "anyio.lowlevel.checkpoint",
    "anyio.lowlevel.checkpoint_if_cancelled",
    "trio.lowlevel.checkpoint",
    "trio.lowlevel.checkpoint_if_cancelled",
];

/// The shield state of each instruction of a function that runs inside a cancel scope
/// ([`Self::at`]).
#[derive(Debug, Default)]
pub struct ShieldPoints {
    at: FxHashMap<(BlockId, usize), ShieldAt>,
}

impl ShieldPoints {
    /// The `with` regions of `f` over catalog cancel scopes, with what each scope shields.
    pub fn of(
        tcx: &TypeCtx<'_>,
        shape: &FnShape<'_>,
        types: &FxHashMap<VarId, Val>,
        live: &crate::ownership::Live,
    ) -> Self {
        let f = shape.f;
        let def = |v: VarId| shape.defining(v).map(|(_, i)| i);
        let sets_shield = f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .any(|i| matches!(&i.kind, InstKind::AttrStore { attr, .. } if attr == "shield"));
        let mut taken: held::Acquisitions<ShieldAt> = held::Acquisitions::default();
        let mut changes: FxHashMap<(BlockId, usize), held::Change> = FxHashMap::default();
        let mut exits: Vec<((BlockId, usize), VarId)> = Vec::new();
        for bb in f.blocks.iter().filter(|b| live.block(b.id)) {
            for (i, inst) in bb.insts.iter().enumerate() {
                match &inst.kind {
                    InstKind::EnterWith { mgr, .. } => {
                        if let Some(shield) = scope_shield(tcx, types, &def, *mgr, sets_shield) {
                            let k = taken.add(inst.span, Some(*mgr), shield);
                            changes.insert((bb.id, i), held::Change::Take(k));
                        }
                    }
                    InstKind::ExitWith { mgr, .. } => exits.push(((bb.id, i), *mgr)),
                    _ => {}
                }
            }
        }
        if taken.is_empty() {
            return Self::default();
        }
        for (at, mgr) in exits {
            let span = f.block(at.0).insts[at.1].span;
            changes.insert(at, held::Change::Give(taken.exited(mgr, span)));
        }
        let entry = held::at_entry(shape, taken.len(), |b, i| changes.get(&(b, i)));
        let mut at = FxHashMap::default();
        for (b, state) in entry.into_iter().enumerate() {
            let Some(mut state) = state else { continue };
            let bb = &f.blocks[b];
            for i in 0..bb.insts.len() {
                let here = state.ones().map(|k| *taken.item(k)).max();
                if let Some(shield) = here {
                    at.insert((bb.id, i), shield);
                }
                state.apply(changes.get(&(bb.id, i)));
            }
        }
        Self { at }
    }

    /// The shield state while instruction `index` of block `bb` runs.
    pub fn at(&self, bb: BlockId, index: usize) -> ShieldAt {
        self.at.get(&(bb, index)).copied().unwrap_or_default()
    }
}

/// What a `with` over `mgr` shields, when `mgr` is a cancel scope: the scope the catalog call
/// defining it makes, or (open) a scope made elsewhere.
fn scope_shield<'i>(
    tcx: &TypeCtx<'_>,
    types: &FxHashMap<VarId, Val>,
    def: &dyn Fn(VarId) -> Option<&'i Inst>,
    mgr: VarId,
    sets_shield: bool,
) -> Option<ShieldAt> {
    let mgr = through_copies(def, mgr);
    if let Some(inst) = def(mgr)
        && let InstKind::Call {
            args,
            kwargs,
            star_from,
            ..
        }
        | InstKind::MethodCall {
            args,
            kwargs,
            star_from,
            ..
        } = &inst.kind
        && let Callee::External(symbol) = resolve_inst(tcx, types, &inst.kind)
        && let Some(slot) = tcx.catalog(&symbol).and_then(|e| e.cancel_scope.as_ref())
    {
        return match passed_shield(def, (args, kwargs, *star_from), slot) {
            Some(true) => Some(ShieldAt::Shielded),
            Some(false) if !sets_shield => None,
            _ => Some(ShieldAt::Open),
        };
    }
    let made_elsewhere = match types.get(&mgr) {
        Some(Val::Inst(class)) => tcx
            .kb
            .lookup(class)
            .is_some_and(|e: &EcosystemEntry| e.cancel_scope.is_some()),
        _ => false,
    };
    made_elsewhere.then_some(ShieldAt::Open)
}

/// The constant a call (its arguments, keywords, and first `*` position) passes at `slot`:
/// nothing passed is the default, `False`. `None` for another value, or when an unpacking may
/// pass it.
fn passed_shield<'i>(
    def: &dyn Fn(VarId) -> Option<&'i Inst>,
    (args, kwargs, star_from): (&[VarId], &[(Option<String>, VarId)], Option<u8>),
    slot: &ArgSlot,
) -> Option<bool> {
    let unpacked = star_from.is_some() || kwargs.iter().any(|(k, _)| k.is_none());
    match crate::resolver::passed_var(args, kwargs, star_from, (slot.arg, slot.param.as_deref())) {
        None if unpacked => None,
        None => Some(false),
        Some(var) => match def(through_copies(def, var)).map(|i| &i.kind) {
            Some(InstKind::Const {
                literal: ConstLiteral::Bool(shield),
                ..
            }) => Some(*shield),
            _ => None,
        },
    }
}

fn through_copies<'i>(def: &dyn Fn(VarId) -> Option<&'i Inst>, mut var: VarId) -> VarId {
    let mut seen = FxHashSet::default();
    while seen.insert(var) {
        match def(var).map(|i| &i.kind) {
            Some(InstKind::Copy { src, .. }) => var = *src,
            _ => break,
        }
    }
    var
}

/// Whether nothing runs after instruction `index` of block `bb` on a normal path: every
/// instruction after it, in its block and in the blocks it leads to, is no call, await, or
/// `with` step, besides the `Await` of the call's own result (a `return` ends the path). A
/// walk past `budget` is open, and the checkpoint then counts.
pub fn runs_nothing_after(shape: &FnShape<'_>, budget: &Budget, bb: BlockId, index: usize) -> bool {
    let f = shape.f;
    let insts = &f.block(bb).insts;
    if insts[index + 1..].iter().any(|i| runs(i, &insts[index])) {
        return false;
    }
    let found = reach(
        budget,
        f.block(bb).terminator.successors(),
        |b, next| next.extend(f.block(b).terminator.successors()),
        |b| {
            if f.block(b).insts.iter().any(|i| runs(i, &insts[index])) {
                Visit::Stop(())
            } else {
                Visit::Expand
            }
        },
    );
    matches!(found, Answer::Known(None))
}

/// Whether `inst`, after the call `call`, runs code: a call, an await other than the call's own
/// (`await f()` is the call and the `Await` of what it returns), a `with` step, a `yield`.
fn runs(inst: &Inst, call: &Inst) -> bool {
    match &inst.kind {
        InstKind::Await { value, .. } => Some(*value) != call.kind.dst(),
        kind => matches!(
            kind,
            InstKind::Call { .. }
                | InstKind::MethodCall { .. }
                | InstKind::EnterWith { .. }
                | InstKind::ExitWith { .. }
                | InstKind::Yield { .. }
        ),
    }
}
