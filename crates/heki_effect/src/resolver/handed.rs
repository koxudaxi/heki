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
use heki_ir::walk::{Answer, Budget, Facts, FnShape, Or, Sparse, Visit, fixpoint, reach};
use heki_ir::{BlockId, InstKind, IrFunction, ModuleIr, VarId};
use rustc_hash::FxHashMap;
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
        let decl =
            (decls.functions.get_mut(qualname)).expect("pass 1 declares every summarized function");
        decl.calls_params |= own.calls;
        decl.awaits_params |= own.awaits;
        let mut handing = decl.handing.take().map(Vec::from).unwrap_or_default();
        handing.extend(own.handing);
        decl.handing = (!handing.is_empty()).then(|| handing.into_boxed_slice());
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
            shape: FnShape::new(f),
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
    shape: FnShape<'a>,
    defs: &'a FxHashMap<VarId, &'a InstKind>,
    phis: &'a FxHashMap<VarId, &'a [(BlockId, VarId)]>,
    cells: &'a crate::infer::ParamCells,
    captured: Option<&'a CapturedParams>,
}

impl Values<'_> {
    /// The parameters (bit `i`; a cell of an enclosing function's parameter at its captured
    /// position) `var` may hold as itself, through copies and one choice: a parameter rebound
    /// before is another value, and `*args` / `**kwargs` hold no one callable.
    fn params(&self, var: VarId) -> u64 {
        let root = self.copied(var);
        match self.phis.get(&root) {
            Some(incoming) => {
                (incoming.iter()).fold(0, |m, (_, v)| m | self.param_bit(self.copied(*v)))
            }
            None => self.param_bit(root),
        }
    }

    /// The parameter `var` is, as a mask: one of the function's own, or a cell of one.
    fn param_bit(&self, var: VarId) -> u64 {
        let own = (self.f.params.iter())
            .position(|p| p.var == var && !p.is_vararg && !p.is_kwarg)
            .filter(|i| *i < 64);
        match (own, self.defs.get(&var)) {
            (Some(i), _) => 1u64 << i,
            (None, Some(InstKind::LoadDeref { name, .. })) => self.captured_bit(name),
            _ => 0,
        }
    }

    /// The value `var` copies, through any number of copies (each value's root is found once per
    /// function).
    fn copied(&self, var: VarId) -> VarId {
        self.shape.copy_root(var)
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
        match self.defs.get(&self.copied(var))? {
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

/// The parameters a function of [`complete`]'s fixpoint calls, and those it awaits.
type Called = (Or, Or);

/// What [`complete`] decides of the module's functions, by qualname: nothing past the budget.
fn decided(
    input: &ModuleInput<'_>,
    decls: &ModuleDecls,
    project: &ProjectDecls,
) -> Vec<(String, (u64, u64))> {
    let mut keys: Vec<(FuncKey, &FunctionDecl)> = (decls.functions.iter())
        .filter(|(_, d)| d.handing.is_some())
        .map(|(qualname, d)| {
            let key = FuncKey {
                module: decls.module_name.clone(),
                qualname: qualname.clone(),
            };
            (key, d.as_ref())
        })
        .collect();
    if keys.is_empty() {
        return Vec::new();
    }
    let roots = keys.len();
    let budget = Budget::of_functions(OWNER, &input.ir.functions);
    let mut index: FxHashMap<FuncKey, usize> = (keys.iter().enumerate())
        .map(|(i, (key, _))| (key.clone(), i))
        .collect();
    // By node: its calls that run a function of the fixpoint, with that node.
    let mut calls: Vec<Vec<(usize, &HandingCall)>> = vec![Vec::new(); roots];
    let expand = |i: usize, out: &mut Vec<usize>| {
        let (caller, decl) = (keys[i].0.clone(), keys[i].1);
        let handing = decl.handing.as_deref().unwrap_or_default();
        // Past the budget the node is not read, and nothing is decided without it.
        if !budget.take(handing.len()) {
            return;
        }
        for h in handing {
            let Some((key, callee)) = run_callee(decls, project, &caller, h).filter(|(_, d)| {
                d.calls_params != 0 || d.awaits_params != 0 || d.handing.is_some()
            }) else {
                continue;
            };
            let j = *index.entry(key.clone()).or_insert_with(|| {
                keys.push((key, callee));
                calls.push(Vec::new());
                keys.len() - 1
            });
            calls[i].push((j, h));
            out.push(j);
        }
    };
    if reach(&budget, 0..roots, expand, |_| Visit::<()>::Expand).is_open() {
        return Vec::new();
    }
    let mut readers: Vec<Vec<usize>> = vec![Vec::new(); keys.len()];
    for (i, node) in calls.iter().enumerate() {
        for &(j, _) in node {
            readers[j].push(i);
        }
    }
    // A fact holds one bit per parameter: a node is read again at most once per bit a callee
    // gains.
    let most_calls = calls.iter().map(Vec::len).max().unwrap_or(0);
    let spec = Sparse {
        nodes: keys.len(),
        readers: &|i, out| out.extend_from_slice(&readers[i]),
        max_visits: u32::try_from(1 + 128 * most_calls).unwrap_or(u32::MAX),
    };
    let own = |j: usize| (keys[j].1.calls_params, keys[j].1.awaits_params);
    let walked = fixpoint(&budget, spec, |i, facts: &Facts<Called>| {
        // Each visit inspects every call of the node, so it is charged them all.
        budget.take(calls[i].len()).then(|| {
            let (mut called, mut awaited) = own(i);
            for &(j, h) in &calls[i] {
                let (c, a) = facts.get(j).map_or(own(j), |(c, a)| (c.0, a.0));
                let up = |mask| handed_up(h, (&keys[i].0.qualname, keys[i].1), keys[j].1, mask);
                called |= up(c);
                awaited |= up(a);
            }
            (Or(called), Or(awaited))
        })
    });
    let Answer::Known(facts) = walked else {
        return Vec::new();
    };
    (keys.iter().enumerate().take(roots))
        .filter_map(|(i, (key, _))| {
            let (c, a) = facts.get(i)?;
            Some((key.qualname.clone(), (c.0, a.0)))
        })
        .collect()
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

/// The parameters of `caller` (its qualname and declaration) that the call `h` binds to the
/// parameters of `callee` in `mask`: an argument that is a parameter of the caller, and a cell
/// the nested callee reads of a parameter of the caller (or of a function enclosing both, at the
/// caller's captured position).
fn handed_up(
    h: &HandingCall,
    (caller, decl): (&str, &FunctionDecl),
    callee: &FunctionDecl,
    mask: u64,
) -> u64 {
    let mut out = 0u64;
    let captured = callee.captured_params.as_deref().filter(|_| h.nested);
    for k in (0..64u8).filter(|k| mask & (1u64 << k) != 0) {
        if let Some(p) = h.call.binds(callee, k) {
            out |= 1u64 << p;
            continue;
        }
        let cell = captured.and_then(|c| c.params.get(usize::from(k.checked_sub(c.first)?)));
        let own = decl.captured_params.as_deref();
        out |= cell.map_or(0, |(owner, i)| {
            if owner == caller {
                1u64 << i
            } else {
                own.map_or(0, |own| own.bit(owner, *i))
            }
        });
    }
    out
}
