//! `EFF118` (`unshielded-cleanup-checkpoint`, heuristic): a cleanup that awaits a call reaching
//! an AnyIO or Trio cancellation checkpoint with no shield on the way. In a cancelled scope the
//! checkpoint raises the cancellation again, and the rest of the cleanup does not run.
//!
//! - **Cleanups**: a `finally` body, an `except` clause that is the first of its `try` to take
//!   the cancellation (a bare `except`, `BaseException`, `asyncio.CancelledError`,
//!   `trio.Cancelled`, `anyio.get_cancelled_exc_class()`, or a tuple holding one), and the body
//!   of an `__aexit__`. A clause after one whose classes heki cannot tell is left out.
//! - **Checkpoint**: the awaited call reaches a catalogued checkpoint
//!   (`ExecEffectKind::Checkpoint`) through awaited calls, with the shortest chain. A shield on
//!   the way (`CancelScope(shield=True)`, `move_on_after(..., shield=True)`), or a scope that
//!   may shield, stops it ([`heki_effect::cancellation`]); so does a branch the call's
//!   constant arguments never take (the input guards).
//! - **Callers**: when every call of the function runs it shielded, its cleanup is too. A call
//!   that only creates its coroutine (handed to a wrapper, a task, `asyncio.shield`) runs it
//!   where heki does not follow: open, and no report when no call is unshielded.

use crate::eff_rules::recursion_handlers::{
    Clause, class_named, clause_chain, is_star_clause, type_expression, type_expression_inst,
};
use crate::{CheckContext, ModuleUnit, SitesBySpan};
use heki_ast::Span;
use heki_diagnostics::Diagnostic;
use heki_effect::{
    CallHop, CallSite, CallsByCaller, FuncKey, ProjectDecls, Runner, ShieldAt, SiteRole, TypeCtx,
    code_kind,
};
use heki_ir::walk::FnShape;
use heki_ir::{BlockId, BuiltinKind, FunctionKind, InstKind, IrFunction, VarId};
use heki_kb::ExecEffectKind;
use rustc_hash::FxHashSet;

/// The classes, besides `BaseException`, whose `except` takes the cancellation of an AnyIO or
/// Trio scope.
const CANCELLATION_CLASSES: &[&str] = &[
    "asyncio.CancelledError",
    "asyncio.exceptions.CancelledError",
    "trio.Cancelled",
];

/// The call whose result is the class of the cancellation of the running AnyIO backend.
const CANCELLED_CLASS_CALL: &str = "anyio.get_cancelled_exc_class";

/// Where a cleanup runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cleanup {
    Finally,
    Handler,
    Aexit,
}

impl Cleanup {
    fn phrase(self) -> &'static str {
        match self {
            Self::Finally => "a `finally` body",
            Self::Handler => "an `except` clause taking the cancellation",
            Self::Aexit => "`__aexit__`",
        }
    }
}

/// Whether an `except` clause takes the cancellation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Takes {
    Yes,
    No,
    Unknown,
}

pub(super) fn run(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.enabled("EFF118", unit) {
        return;
    }
    let tcx = TypeCtx::new(&unit.input, cx.decls, cx.kb);
    let mut sites: Option<SitesBySpan<'_>> = None;
    let mut calls = cx.index.calls_by_caller();
    for f in unit.input.ir.functions.iter() {
        if f.is_class_body || f.is_module_body {
            continue;
        }
        let key = CheckContext::key(unit, f);
        if code_kind::is_test_code(&key.qualname, &unit.rel_path) {
            continue;
        }
        let cleanups = cleanups_of(f, unit.source, &tcx, cx.decls);
        if cleanups.is_empty() {
            continue;
        }
        let sites = sites.get_or_insert_with(|| cx.sites_by_span(unit));
        let mut own: Vec<&CallSite> = sites
            .of(&key.qualname)
            .all()
            .filter(|s| awaited_unshielded(s))
            .collect();
        own.sort_by_key(|s| (s.span.start, s.span.end));
        let mut callers: Option<ShieldAt> = None;
        let mut reported: FxHashSet<Span> = FxHashSet::default();
        for site in own {
            let Some(cleanup) = cleanups
                .iter()
                .find(|(c, _)| c.start <= site.span.start && site.span.end <= c.end)
                .map(|(_, kind)| *kind)
            else {
                continue;
            };
            if reported.contains(&site.span) {
                continue;
            }
            let Some(reach) = checkpoint_reach(cx, unit, site) else {
                continue;
            };
            if *callers.get_or_insert_with(|| callers_shield(cx, &mut calls, &key))
                != ShieldAt::Unshielded
            {
                break;
            }
            reported.insert(site.span);
            report(cx, unit, &key, site.span, cleanup, &reach, out);
        }
    }
}

/// An awaited call on the caller's own task with no scope that may shield it around.
fn awaited_unshielded(site: &CallSite) -> bool {
    site.role == SiteRole::Call
        && site.is_awaited
        && site.runner == Runner::Caller
        && site.shield.is_unshielded()
}

/// The checkpoint a call reaches, with the shortest chain of calls down to it: the callee of
/// the call, then each hop to the checkpoint.
struct Reach {
    callee: String,
    checkpoint: String,
    chain: Vec<CallHop>,
}

fn checkpoint_reach(
    cx: &CheckContext<'_>,
    unit: &ModuleUnit<'_>,
    site: &CallSite,
) -> Option<Reach> {
    let chain = (cx.index.site_effects(&unit.input.path, site).into_iter())
        .filter(|e| e.effect == ExecEffectKind::Checkpoint)
        .map(|e| e.chain)
        .min_by_key(Vec::len)?;
    Some(Reach {
        callee: chain.first()?.callee.clone(),
        checkpoint: chain.last()?.callee.clone(),
        chain,
    })
}

/// How the calls of `key` run it: shielded when every one of them is, open when none is
/// unshielded and one only creates its coroutine or sits in a scope that may shield.
/// Unshielded when one is, or when heki finds no call (the function is an entry point).
fn callers_shield<'a>(
    cx: &CheckContext<'a>,
    calls: &mut CallsByCaller<'a>,
    key: &FuncKey,
) -> ShieldAt {
    // The function has call sites in the index: it is summarized. Each function asks once, so
    // reading every call of it stays linear in the calls of the project.
    let found = cx.index.callers_within(calls, key, 1, usize::MAX);
    let mut answer: Option<ShieldAt> = None;
    for at in &found.sites {
        let runs = at.sites.iter().filter_map(|s| match s.role {
            SiteRole::Call if s.is_awaited || s.drives_coroutine_manually => Some(s.shield),
            SiteRole::Call => Some(ShieldAt::Open),
            _ => None,
        });
        if let Some(here) = runs.min() {
            answer = Some(answer.map_or(here, |a| a.min(here)));
        }
    }
    answer.unwrap_or(ShieldAt::Unshielded)
}

/// The cleanups of `f` with the spans of their statements.
fn cleanups_of(
    f: &IrFunction,
    source: &str,
    tcx: &TypeCtx<'_>,
    decls: &ProjectDecls,
) -> Vec<(Span, Cleanup)> {
    if !matches!(f.kind, FunctionKind::Async | FunctionKind::AsyncGenerator) {
        return Vec::new();
    }
    if f.name == "__aexit__" && f.owner_class.is_some() {
        return vec![(f.span, Cleanup::Aexit)];
    }
    let mut out: Vec<(Span, Cleanup)> = (f.finally_bodies.iter())
        .map(|s| (*s, Cleanup::Finally))
        .collect();
    let has_handler = f.blocks.iter().any(|b| {
        matches!(
            b.insts.first().map(|i| &i.kind),
            Some(InstKind::CatchException { .. })
        )
    });
    if !has_handler {
        return out;
    }
    let shape = FnShape::new(f);
    let landing: FxHashSet<BlockId> = f.blocks.iter().filter_map(|b| b.exc_target).collect();
    for dispatch in landing {
        let (clauses, _) = clause_chain(f, dispatch);
        for clause in &clauses {
            let takes = if is_star_clause(source, clause.span) {
                Takes::Unknown
            } else {
                takes_cancellation(&shape, tcx, decls, clause)
            };
            match takes {
                Takes::Yes => {
                    out.push((clause.span, Cleanup::Handler));
                    break;
                }
                Takes::Unknown => break,
                Takes::No => {}
            }
        }
    }
    out
}

fn takes_cancellation(
    shape: &FnShape<'_>,
    tcx: &TypeCtx<'_>,
    decls: &ProjectDecls,
    clause: &Clause<'_>,
) -> Takes {
    if clause.types.is_empty() {
        return Takes::Yes;
    }
    // A name the clause reads without an instruction (a parameter) is of no class heki knows.
    let Some(root) = type_expression(shape.f, clause.test) else {
        return Takes::Unknown;
    };
    let classes: Vec<VarId> = match type_expression_inst(shape.f, clause.test) {
        Some(InstKind::AllocBuiltin {
            kind: BuiltinKind::Tuple,
            elems,
            ..
        }) => elems.clone(),
        _ => vec![root],
    };
    let named = |v: VarId| class_named(shape, tcx, decls, clause.test, v);
    let mut takes = Takes::No;
    for class in classes {
        // `except anyio.get_cancelled_exc_class():` names the class the call returns.
        let called = match shape.defining(class).map(|(_, i)| &i.kind) {
            Some(InstKind::Call {
                callee_var: Some(callee),
                ..
            }) => named(*callee),
            Some(InstKind::MethodCall { recv, method, .. }) => {
                named(*recv).map(|module| format!("{module}.{method}"))
            }
            _ => None,
        };
        if called.as_deref() == Some(CANCELLED_CLASS_CALL) {
            return Takes::Yes;
        }
        match named(class).as_deref() {
            Some("builtins.BaseException") => return Takes::Yes,
            Some(fqn) if CANCELLATION_CLASSES.contains(&fqn) => return Takes::Yes,
            // A builtin, a class of the project, or a library class that is no cancellation
            // (`OSError`, `httpx.HTTPError`): each is an `Exception`, which the cancellation is
            // not.
            Some(_) => {}
            None => takes = Takes::Unknown,
        }
    }
    takes
}

fn report(
    cx: &CheckContext<'_>,
    unit: &ModuleUnit<'_>,
    key: &FuncKey,
    span: Span,
    cleanup: Cleanup,
    reach: &Reach,
    out: &mut Vec<Diagnostic>,
) {
    let Reach {
        callee,
        checkpoint,
        chain,
    } = reach;
    let (awaited, through) = if chain.len() == 1 {
        (
            format!("the cancellation checkpoint `{checkpoint}`"),
            String::new(),
        )
    } else {
        (
            format!("`{callee}`"),
            format!(
                ", which reaches the cancellation checkpoint `{checkpoint}` {} calls down",
                chain.len()
            ),
        )
    };
    let message = format!(
        "`{}` awaits {awaited} in {}{through} with no shield on the way: in a cancelled scope \
         the await raises the cancellation and the rest of the cleanup does not run",
        key.qualname,
        cleanup.phrase(),
    );
    let scope = if checkpoint.starts_with("trio.") {
        "trio.CancelScope(shield=True)"
    } else {
        "anyio.CancelScope(shield=True)"
    };
    let help = format!("run the cleanup inside `with {scope}:`");
    cx.emit(
        out,
        "EFF118",
        unit,
        span,
        &key.qualname,
        format!("{callee}->{checkpoint}"),
        message,
        Some(help),
        chain,
    );
}
