//! The objects a function uses that allow one operation at a time (`EFF110`,
//! docs/ANALYSIS_AND_KB_SPEC.md section 2.2).
//!
//! A use is a call of a method the catalog flags `one_at_a_time` on an instance of its class
//! (`await reader.readline()`, `await conn.fetch(q)`, `await ws.recv()`), or an `async for` over
//! one (its class's `__anext__`). `EffectSummary::exclusive_uses` holds the function's own uses
//! and those of the calls that run in its body on its task, in the function's names
//! ([`ObjRef`]), as `EffectSummary::acquires_locks` does for locks. A use while the function
//! certainly holds a lock does not count, and neither do the uses of a call made while it holds
//! one (`CallSite::under_lock`): the lock keeps the tasks to one at a time.

use crate::callgraph;
use crate::locks::ObjRef;
use crate::resolver::{TypeCtx, Val};
use crate::{BoundCallee, CallHop, CallSite, FuncKey, Runner, SharedChain};
use heki_ast::Span;
use heki_ir::{InstKind, VarId};
use heki_kb::OneAtATime;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::path::Path;
use std::sync::Arc;

/// One object a function uses one operation at a time (`EffectSummary::exclusive_uses`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExclusiveUse {
    /// The catalog method (`asyncio.StreamReader.readline`).
    pub method: String,
    /// When a second task's call raises: always, or while the object holds no connection.
    pub kind: OneAtATime,
    /// The calls from the function to the use, then the use itself: a hop from the using
    /// function to the method, at the call.
    pub chain: SharedChain,
}

impl ExclusiveUse {
    /// Copy with every source location zeroed (Early Cutoff).
    pub(crate) fn without_locations(&self) -> Self {
        let mut out = self.clone();
        crate::zero_hop_spans(&mut out.chain);
        out
    }
}

/// The kind two uses of one object count as together: one that always raises covers one that
/// raises while unbound, and an operation that unbinds is one while unbound.
fn stronger(a: OneAtATime, b: OneAtATime) -> OneAtATime {
    if a == OneAtATime::Always || b == OneAtATime::Always {
        OneAtATime::Always
    } else {
        OneAtATime::WhenUnbound
    }
}

/// Add one more use of `obj`: the stronger kind, and the shortest chain.
pub(crate) fn add_use(uses: &mut BTreeMap<ObjRef, ExclusiveUse>, obj: ObjRef, u: ExclusiveUse) {
    let u = ExclusiveUse {
        kind: stronger(u.kind, u.kind),
        ..u
    };
    match uses.entry(obj) {
        Entry::Vacant(e) => {
            e.insert(u);
        }
        Entry::Occupied(mut e) => {
            let cur = e.get_mut();
            cur.kind = stronger(cur.kind, u.kind);
            if u.chain.len() < cur.chain.len() {
                cur.chain = u.chain;
                cur.method = u.method;
            }
        }
    }
}

/// What one use of a callee becomes in the caller at `site` (bound by `bc`): the object in the
/// caller's names, with the call in front of its chain. `None` when the caller holds a lock at
/// the call, the callee runs on a worker thread, or the caller has no name for the object.
pub(crate) fn through_call(
    site: &CallSite,
    bc: &BoundCallee,
    obj: &ObjRef,
    u: &ExclusiveUse,
    lead: &[Arc<CallHop>],
    rewrite: bool,
) -> Option<(ObjRef, ExclusiveUse)> {
    if site.under_lock || site.runner == Runner::Worker {
        return None;
    }
    let mapped = obj.through(bc)?;
    Some((
        mapped,
        ExclusiveUse {
            method: u.method.clone(),
            kind: u.kind,
            chain: callgraph::extend_chain(lead, &u.chain, rewrite),
        },
    ))
}

/// The use `kind` makes, when it calls a one-at-a-time method on (or iterates with `async for`)
/// an instance of a catalog class (`class_of`): the object, the method, and its kind.
pub fn use_step(
    tcx: &TypeCtx<'_>,
    kind: &InstKind,
    class_of: &dyn Fn(VarId) -> Option<String>,
) -> Option<(VarId, String, OneAtATime)> {
    let (obj, method) = match kind {
        InstKind::MethodCall { recv, method, .. } => (*recv, method.as_str()),
        InstKind::MakeIter {
            iterable,
            is_async: true,
            ..
        } => (*iterable, "__anext__"),
        _ => return None,
    };
    let symbol = format!("{}.{method}", class_of(obj)?);
    let one = tcx.kb.lookup(&symbol)?.one_at_a_time?;
    Some((obj, symbol, one))
}

/// The class of `var` when it is an instance of one (`types`: the inferred values).
pub fn instance_class(types: &FxHashMap<VarId, Val>, var: VarId) -> Option<String> {
    match types.get(&var)? {
        Val::Inst(c) => Some(c.clone()),
        _ => None,
    }
}

/// The use [`ExclusiveUse`] of `method` by the function `caller` at `span`.
pub(crate) fn own_use(
    caller: &FuncKey,
    path: &Path,
    method: String,
    kind: OneAtATime,
    span: Span,
) -> ExclusiveUse {
    let hop = CallHop {
        caller: caller.clone(),
        callee: method.clone(),
        span,
        path: path.to_path_buf(),
    };
    ExclusiveUse {
        method,
        kind,
        chain: vec![Arc::new(hop)],
    }
}
