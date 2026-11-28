//! Functions that call a value they are handed: `each(xs, visit)` calling `visit(x)`, directly,
//! in a function nested in it, or by handing it on (`run_all(xs, visit)` calling
//! `each(xs, visit)`). Pass 1 records each function's own calls of its parameters and the calls
//! that hand them on ([`record`]); completing a module decides [`FunctionDecl::calls_params`] and
//! [`FunctionDecl::awaits_params`] through those calls, across any number of modules
//! ([`complete`]). A call binding such a parameter to a known function runs that function there
//! (the summaries' sites of handed callables).

use super::forwarded::{self, ForwardingCall};
use super::{CapturedParams, FunctionDecl, ModuleDecls, ProjectDecls};
use super::{func_qualname, is_summarized_function};
use crate::{FuncKey, ModuleInput};
use heki_ir::walk::Budget;
use heki_ir::{BlockId, InstKind, IrFunction, ModuleIr, VarId};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

/// The owner of the budget of a module's [`complete`].
const OWNER: &str = "heki_effect::resolver::handed";

/// A call that may hand a parameter of the calling function on ([`FunctionDecl::handing`]): of a
/// project function by name, with the parameters its arguments are, or of a function nested in
/// the caller, which reads the caller's parameters through its closure cells too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandingCall {
    /// The callee named as a global of the module, or by its qualname when `nested`.
    pub call: ForwardingCall,
    /// The callee is a function nested in the caller, called there or handed to a builtin that
    /// calls it once per item (`map(each, xs)`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub nested: bool,
    /// The call is awaited: an `async def` callee runs its body there.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub awaited: bool,
}

/// Record, for each function of the module, the parameters it calls and awaits itself and the
/// calls that may hand them on.
pub(super) fn record(
    ir: &ModuleIr,
    decls: &mut ModuleDecls,
    captured: &FxHashMap<&str, CapturedParams>,
) {
    let cells = crate::infer::ParamCells::of(ir);
    let mut found: Vec<(&str, Own)> = Vec::new();
    for f in ir.functions.iter().filter(|f| is_summarized_function(f)) {
        let own = Own::of(ir, f, decls, &cells, captured.get(f.qualname.as_str()));
        if own.calls != 0 || own.awaits != 0 || !own.handing.is_empty() {
            found.push((&f.qualname, own));
        }
    }
    // The definitions of one qualname share a declaration: it calls what any of them calls.
    for (qualname, own) in found {
        if let Some(decl) = decls.functions.get_mut(qualname) {
            decl.calls_params |= own.calls;
            decl.awaits_params |= own.awaits;
            let mut handing = decl.handing.take().map(Vec::from).unwrap_or_default();
            handing.extend(own.handing);
            decl.handing = (!handing.is_empty()).then(|| handing.into_boxed_slice());
        }
    }
}

/// What one definition does itself with its parameters ([`record`]).
struct Own {
    calls: u64,
    awaits: u64,
    handing: Vec<HandingCall>,
}

impl Own {
    fn of(
        ir: &ModuleIr,
        f: &IrFunction,
        decls: &ModuleDecls,
        cells: &crate::infer::ParamCells,
        captured: Option<&CapturedParams>,
    ) -> Self {
        let defs: FxHashMap<VarId, &InstKind> = (f.blocks.iter().flat_map(|b| &b.insts))
            .filter_map(|i| Some((i.kind.dst()?, &i.kind)))
            .collect();
        let phis: FxHashMap<VarId, &[(BlockId, VarId)]> = (f.blocks.iter().flat_map(|b| &b.phis))
            .map(|p| (p.dst, p.incoming.as_slice()))
            .collect();
        let values = Values {
            ir,
            f,
            defs: &defs,
            phis: &phis,
            cells,
            captured,
        };
        let mut own = Self {
            calls: 0,
            awaits: 0,
            handing: Vec::new(),
        };
        for kind in f.blocks.iter().flat_map(|b| &b.insts).map(|i| &i.kind) {
            let InstKind::Call {
                callee_var: Some(called),
                args,
                kwargs,
                is_awaited,
                star_from,
                ..
            } = kind
            else {
                continue;
            };
            let params = values.params(*called);
            own.calls |= params;
            if *is_awaited {
                own.awaits |= params;
            }
            let mut param = |v: VarId| one_param(values.params(v));
            let handing = match values.nested(*called) {
                Some(qualname) => Some((
                    nested_call(qualname, args, kwargs, *star_from, &mut param),
                    true,
                )),
                None => ForwardingCall::of(kind, &defs, decls, &mut param).map(|c| (c, false)),
            };
            own.handing
                .extend(handing.map(|(call, nested)| HandingCall {
                    call,
                    nested,
                    awaited: *is_awaited,
                }));
            // A builtin calling a nested function once per item calls it here.
            let builtin = match defs.get(called) {
                Some(InstKind::LoadGlobal { name, .. })
                    if !decls.imports.contains_key(name) && !decls.binds(name) =>
                {
                    Some(format!("builtins.{name}"))
                }
                _ => None,
            };
            let per_item = builtin.and_then(|symbol| {
                crate::consumption::per_item_callable(&symbol, args, kwargs, *star_from)
            });
            if let Some(qualname) = per_item.and_then(|p| values.nested(p.callable)) {
                own.handing.push(HandingCall {
                    call: nested_call(qualname, &[], &[], None, &mut param),
                    nested: true,
                    awaited: false,
                });
            }
        }
        own
    }
}

/// A call of the nested function `qualname`, with the parameters its arguments are.
fn nested_call(
    qualname: &str,
    args: &[VarId],
    kwargs: &[(Option<String>, VarId)],
    star_from: Option<u8>,
    param: &mut dyn FnMut(VarId) -> Option<u8>,
) -> ForwardingCall {
    // A keyword binds its parameter by name whatever `*` unpacks.
    let before_star = star_from.map_or(args.len(), |s| args.len().min(usize::from(s)));
    ForwardingCall {
        callee: qualname.into(),
        positional: args[..before_star].iter().map(|a| param(*a)).collect(),
        keywords: (kwargs.iter())
            .filter_map(|(name, v)| Some((Box::from(name.as_deref()?), param(*v)?)))
            .collect(),
    }
}

/// The parameter a mask of [`Values::params`] names, when it names one.
fn one_param(mask: u64) -> Option<u8> {
    (mask.count_ones() == 1).then(|| u8::try_from(mask.trailing_zeros()).ok())?
}

/// The values of one function, as [`Own::of`] reads them.
struct Values<'a> {
    ir: &'a ModuleIr,
    f: &'a IrFunction,
    defs: &'a FxHashMap<VarId, &'a InstKind>,
    phis: &'a FxHashMap<VarId, &'a [(BlockId, VarId)]>,
    cells: &'a crate::infer::ParamCells,
    captured: Option<&'a CapturedParams>,
}

impl Values<'_> {
    /// The parameters (bit `i`; a cell of an enclosing function's parameter at its captured
    /// position) `var` may hold as itself, through copies and choices: a parameter rebound
    /// before is another value, and `*args` / `**kwargs` hold no one callable.
    fn params(&self, var: VarId) -> u64 {
        let mut mask = 0;
        let mut seen: FxHashSet<VarId> = FxHashSet::default();
        let mut stack = vec![var];
        while let Some(v) = stack.pop() {
            if !seen.insert(v) {
                continue;
            }
            if let Some(i) = (self.f.params.iter())
                .position(|p| p.var == v && !p.is_vararg && !p.is_kwarg)
                .filter(|i| *i < 64)
            {
                mask |= 1u64 << i;
                continue;
            }
            match self.defs.get(&v) {
                Some(InstKind::Copy { src, .. }) => stack.push(*src),
                Some(InstKind::LoadDeref { name, .. }) => mask |= self.captured_bit(name),
                _ => {}
            }
            if let Some(incoming) = self.phis.get(&v) {
                stack.extend(incoming.iter().map(|(_, v)| *v));
            }
        }
        mask
    }

    /// The captured position of the enclosing function's parameter the cell `name` holds.
    fn captured_bit(&self, name: &str) -> u64 {
        let cell = self.captured.zip(self.cells.param(self.ir, self.f, name));
        cell.map_or(0, |(captured, (owner, i))| {
            captured.bit(func_qualname(owner), i)
        })
    }

    /// The qualname of the function nested in this one that `var` holds, through copies.
    fn nested(&self, var: VarId) -> Option<&str> {
        let mut cur = var;
        // Copies in dead code can close a cycle: at most one step per instruction.
        for _ in 0..self.defs.len() {
            match self.defs.get(&cur) {
                Some(InstKind::Copy { src, .. }) => cur = *src,
                _ => break,
            }
        }
        match self.defs.get(&cur)? {
            InstKind::MakeClosure {
                func_id: Some(id), ..
            } => {
                let child = self.ir.function(*id);
                is_summarized_function(child).then(|| func_qualname(child))
            }
            _ => None,
        }
    }
}

/// [`FunctionDecl::calls_params`] and [`FunctionDecl::awaits_params`] of the module's functions
/// that hand parameters on ([`FunctionDecl::handing`]), against pass 1 `project`: a callee whose
/// body runs at the call (no generator; an `async def` only where the call is awaited) calls the
/// caller's parameter the call binds to one it calls. A union fixpoint from the own facts, along
/// the calls through any number of functions and modules; each evaluation of a function is charged
/// its calls, and past the module's budget the facts stay what they are.
pub(super) fn complete(input: &ModuleInput<'_>, decls: &mut ModuleDecls, project: &ProjectDecls) {
    let decided = decided(input, decls, project);
    for (qualname, (calls, awaits)) in decided {
        if let Some(decl) = decls.functions.get_mut(&qualname) {
            decl.calls_params = calls;
            decl.awaits_params = awaits;
        }
    }
}

/// A function of [`complete`]'s fixpoint.
struct Node<'d> {
    key: FuncKey,
    decl: &'d FunctionDecl,
    /// Its calls that run a function of the fixpoint, with that node.
    callees: Vec<(usize, &'d HandingCall)>,
    callers: Vec<usize>,
    calls: u64,
    awaits: u64,
}

fn decided(
    input: &ModuleInput<'_>,
    decls: &ModuleDecls,
    project: &ProjectDecls,
) -> Vec<(String, (u64, u64))> {
    let mut nodes: Vec<Node<'_>> = Vec::new();
    let mut index: FxHashMap<FuncKey, usize> = FxHashMap::default();
    for (qualname, decl) in &decls.functions {
        if decl.handing.is_some() {
            let key = FuncKey {
                module: decls.module_name.clone(),
                qualname: qualname.clone(),
            };
            index.insert(key.clone(), nodes.len());
            nodes.push(Node::new(key, decl));
        }
    }
    if nodes.is_empty() {
        return Vec::new();
    }
    let budget = Budget::of_functions(OWNER, &input.ir.functions);
    // The functions the handing calls run, breadth first.
    let mut i = 0;
    while i < nodes.len() {
        let handing = nodes[i].decl.handing.as_deref().unwrap_or_default();
        if !budget.take(handing.len()) {
            break;
        }
        for h in handing {
            let Some((key, callee)) = run_callee(decls, project, &nodes[i].key, h) else {
                continue;
            };
            if callee.calls_params == 0 && callee.awaits_params == 0 && callee.handing.is_none() {
                continue;
            }
            let j = *index.entry(key.clone()).or_insert_with(|| {
                nodes.push(Node::new(key, callee));
                nodes.len() - 1
            });
            nodes[i].callees.push((j, h));
            nodes[j].callers.push(i);
        }
        i += 1;
    }
    let mut queue: Vec<usize> = (0..nodes.len()).rev().collect();
    let mut queued: Vec<bool> = vec![true; nodes.len()];
    while let Some(i) = queue.pop() {
        queued[i] = false;
        if !budget.take(nodes[i].callees.len()) {
            break;
        }
        let (mut calls, mut awaits) = (nodes[i].calls, nodes[i].awaits);
        for &(j, h) in &nodes[i].callees {
            let (caller, callee) = (&nodes[i], &nodes[j]);
            calls |= handed_up(h, caller, callee, callee.calls);
            awaits |= handed_up(h, caller, callee, callee.awaits);
        }
        if (calls, awaits) != (nodes[i].calls, nodes[i].awaits) {
            nodes[i].calls = calls;
            nodes[i].awaits = awaits;
            for &p in &nodes[i].callers {
                if !queued[p] {
                    queued[p] = true;
                    queue.push(p);
                }
            }
        }
    }
    (nodes.into_iter())
        .filter(|n| n.key.module == decls.module_name)
        .map(|n| (n.key.qualname, (n.calls, n.awaits)))
        .collect()
}

impl<'d> Node<'d> {
    fn new(key: FuncKey, decl: &'d FunctionDecl) -> Self {
        Self {
            key,
            decl,
            callees: Vec::new(),
            callers: Vec::new(),
            calls: decl.calls_params,
            awaits: decl.awaits_params,
        }
    }
}

/// The function `h`, a call in `caller`, runs the body of: a nested function of the caller or a
/// project function by name, neither a generator nor an `async def` the call does not await.
fn run_callee<'d>(
    decls: &'d ModuleDecls,
    project: &'d ProjectDecls,
    caller: &FuncKey,
    h: &HandingCall,
) -> Option<(FuncKey, &'d FunctionDecl)> {
    let (key, decl) = if h.nested {
        let key = FuncKey {
            module: caller.module.clone(),
            qualname: h.call.callee.to_string(),
        };
        let decl = forwarded::decl_of(decls, project, &key)?;
        (key, decl)
    } else {
        let (key, decl) = forwarded::callee(project, &h.call)?;
        let decl = forwarded::decl_of(decls, project, &key).unwrap_or(decl);
        (key, decl)
    };
    (!decl.is_generator && (!decl.is_async || h.awaited)).then_some((key, decl))
}

/// The parameters of `caller` that the call `h` binds to the parameters of `callee` in `mask`:
/// an argument that is a parameter of the caller, and a cell the nested callee reads of a
/// parameter of the caller (or of a function enclosing both, at the caller's captured position).
fn handed_up(h: &HandingCall, caller: &Node<'_>, callee: &Node<'_>, mask: u64) -> u64 {
    let mut out = 0u64;
    let captured = callee.decl.captured_params.as_deref().filter(|_| h.nested);
    for k in (0..64u8).filter(|k| mask & (1u64 << k) != 0) {
        if let Some(p) = h.call.binds(callee.decl, k) {
            out |= 1u64 << p;
            continue;
        }
        let Some(c) = captured else { continue };
        let Some((owner, i)) = k
            .checked_sub(c.first)
            .and_then(|j| c.params.get(usize::from(j)))
        else {
            continue;
        };
        let own = caller.decl.captured_params.as_deref();
        out |= if *owner == caller.key.qualname {
            1u64 << i
        } else {
            own.map_or(0, |own| own.bit(owner, *i))
        };
    }
    out
}
