//! Family 2 execution-effect rules: `EFF101`, `EFF105`, `EFF102`, `EFF103`, `EFF106`,
//! `EFF107`, `EFF201` (docs/DETAILED_SPEC_M3_M7.md section 3.2).

use crate::{CheckContext, ModuleUnit, RuleConfigError};
use heki_diagnostics::Diagnostic;
use heki_effect::{
    BoundaryReport, CallHop, EffectSummary, FuncKey, ROOT_CAUSE_GROUP_THRESHOLD, SiteEffect,
    boundary_reports_in, group_by_root_cause,
};
use heki_ir::{InstKind, LoopBound};
use heki_kb::ExecEffectKind;

/// Modules whose own code is an ORM implementation (EFF103 is about ORM *users*).
const ORM_PACKAGES: &[&str] = &["sqlalchemy", "django.db", "sqlmodel"];
const DDL_PREFIXES: &[&str] = &["CREATE", "ALTER", "DROP", "PRAGMA"];
/// `boundary_reports_in` only reports call sites of the files it is given.
const REPORTS_OF_UNITS: &str = "boundary reports come from the checked files";

fn has_decorator(s: &EffectSummary, name: &str) -> bool {
    s.decorators.iter().any(|d| {
        let base = d.split('(').next().unwrap_or(d).trim();
        base == name || base.ends_with(&format!(".{name}"))
    })
}

/// Default context relaxations: CLI commands (`click` / `typer`), ASGI lifespans, and tests.
fn relaxed(s: &EffectSummary, key: &FuncKey, unit: &ModuleUnit<'_>, relax_tests: bool) -> bool {
    let name = key.qualname.rsplit('.').next().unwrap_or(&key.qualname);
    if has_decorator(s, "command") {
        return true;
    }
    if has_decorator(s, "asynccontextmanager") && name.contains("lifespan") {
        return true;
    }
    relax_tests && (name.starts_with("test_") || unit.rel_path.split('/').any(|c| c == "tests"))
}

pub(crate) fn eff101_105(cx: &CheckContext<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.selection.is_enabled("EFF101") && !cx.selection.is_enabled("EFF105") {
        return;
    }
    let relax_tests = cx.config.lint.relax_async_in_tests.unwrap_or(true);
    let mut eff101: Vec<BoundaryReport> = Vec::new();
    let mut eff105: Vec<BoundaryReport> = Vec::new();
    // The boundaries of the checked files: all of them for `heki check`, the edited one for
    // the LSP engine.
    let reports = boundary_reports_in(cx.index, cx.units.iter().map(|u| u.input.path.as_path()));
    for r in reports {
        let code = if r.effect == ExecEffectKind::BlockingFs {
            "EFF105"
        } else {
            "EFF101"
        };
        let unit = cx.unit(&r.path).expect(REPORTS_OF_UNITS);
        let relaxed_here = cx
            .index
            .summary(&r.function)
            .is_some_and(|s| relaxed(s, &r.function, unit, relax_tests));
        if !cx.enabled(code, unit) || relaxed_here {
            continue;
        }
        if code == "EFF105" {
            eff105.push(r);
        } else {
            eff101.push(r);
        }
    }
    for (code, reports) in [("EFF101", eff101), ("EFF105", eff105)] {
        for g in group_by_root_cause(&reports, ROOT_CAUSE_GROUP_THRESHOLD) {
            let r = &g.representative;
            let unit = cx.unit(&r.path).expect(REPORTS_OF_UNITS);
            let first = r
                .chain
                .first()
                .map_or(r.root_cause.as_str(), |h| h.callee.as_str());
            let what = if code == "EFF105" {
                "blocking filesystem I/O"
            } else {
                "a blocking call"
            };
            let mut message = format!(
                "`async def {}` calls `{first}`, which reaches {what} `{}` ({}) on the event loop; offload it (`asyncio.to_thread`) or use an async API",
                r.function.qualname,
                r.root_cause,
                effect_name(r.effect)
            );
            if let Some(suffix) = g.message_suffix() {
                message.push(' ');
                message.push_str(&suffix);
            }
            cx.emit(
                out,
                code,
                unit,
                r.span,
                &r.function.qualname,
                r.root_cause.clone(),
                message,
                &r.chain,
            );
        }
    }
}

pub(crate) fn effect_name(e: ExecEffectKind) -> &'static str {
    match e {
        ExecEffectKind::BlockingNet => "blocking_net",
        ExecEffectKind::BlockingDb => "blocking_db",
        ExecEffectKind::BlockingSubprocess => "blocking_subprocess",
        ExecEffectKind::BlockingSync => "blocking_sync",
        ExecEffectKind::Sleep => "sleep",
        ExecEffectKind::BlockingFs => "blocking_fs",
        ExecEffectKind::UnpooledClientAlloc => "unpooled_client_alloc",
        ExecEffectKind::DbQuery => "db_query",
        ExecEffectKind::QuadraticRealloc => "quadratic_realloc",
    }
}

/// Shortest chain among `effects` carrying `kind`.
pub(crate) fn shortest(effects: &[SiteEffect], kind: ExecEffectKind) -> Option<&[CallHop]> {
    effects
        .iter()
        .filter(|e| e.effect == kind)
        .map(|e| e.chain.as_slice())
        .min_by_key(|c| c.len())
}

pub(crate) fn eff102_103(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    let want102 = cx.enabled("EFF102", unit);
    let want103 = cx.enabled("EFF103", unit)
        && !ORM_PACKAGES.iter().any(|p| {
            unit.input.module_name == *p || unit.input.module_name.starts_with(&format!("{p}."))
        })
        && !unit
            .rel_path
            .split('/')
            .any(|c| c == "migrations" || c == "alembic");
    if !want102 && !want103 {
        return;
    }
    let sites = cx.sites_by_span(unit);
    for f in &unit.input.ir.functions {
        if f.is_class_body {
            continue;
        }
        let key = CheckContext::key(unit, f);
        let migration_fn = f.name == "upgrade" || f.name == "downgrade";
        for bb in &f.blocks {
            if f.block_loop_depth(bb.id) == 0 {
                continue;
            }
            // `for.iter` / `MakeIter` live in the preheader (depth 0), so they never get here.
            // Bounded loops (small constant ranges, retries, daemon polling) are relaxed.
            if f.innermost_loop(bb.id).map(|lp| lp.bound) != Some(LoopBound::Unbounded) {
                continue;
            }
            for inst in &bb.insts {
                let args = match &inst.kind {
                    InstKind::Call { args, .. } | InstKind::MethodCall { args, .. } => args,
                    _ => continue,
                };
                let Some(list) = sites.get(&(key.qualname.clone(), inst.span)) else {
                    continue;
                };
                let effects: Vec<SiteEffect> = list
                    .iter()
                    .flat_map(|s| cx.index.site_effects(&unit.input.path, s))
                    .collect();
                if want102
                    && let Some(chain) = shortest(&effects, ExecEffectKind::UnpooledClientAlloc)
                {
                    let root = chain.last().map(|h| h.callee.clone()).unwrap_or_default();
                    cx.emit(
                            out,
                            "EFF102",
                            unit,
                            inst.span,
                            &key.qualname,
                            root.clone(),
                            format!(
                                "`{}` allocates an unpooled client (`{root}`) on every loop iteration; create one client / session before the loop and reuse it",
                                chain[0].callee
                            ),
                            chain,
                        );
                }
                // Compared as bytes: the prefixes are ASCII, and slicing the constant could
                // split a multi-byte character.
                let ddl = args.first().and_then(|a| f.const_str(*a)).is_some_and(|q| {
                    let q = q.trim_start().as_bytes();
                    DDL_PREFIXES.iter().any(|p| {
                        q.get(..p.len())
                            .is_some_and(|head| head.eq_ignore_ascii_case(p.as_bytes()))
                    })
                });
                if want103
                    && !migration_fn
                    && !ddl
                    && let Some(chain) = shortest(&effects, ExecEffectKind::DbQuery)
                {
                    let root = chain.last().map(|h| h.callee.clone()).unwrap_or_default();
                    cx.emit(
                            out,
                            "EFF103",
                            unit,
                            inst.span,
                            &key.qualname,
                            root.clone(),
                            format!(
                                "`{}` runs a database query (`{root}`) on every loop iteration (N+1); batch it or load the data once before the loop",
                                chain[0].callee
                            ),
                            chain,
                        );
                }
            }
        }
    }
}

mod held_across_io;
mod unretained_task;

/// `EFF106` (opt-in): `await` / blocking network I/O while a sync lock or DB transaction is
/// held.
pub(crate) fn eff106(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    held_across_io::run(cx, unit, out);
}

/// `EFF107` (opt-in): a locally bound `asyncio.create_task` result that is never retained.
pub(crate) fn eff107(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    unretained_task::run(cx, unit, out);
}

/// `EFF201`: forbidden effects reached from `effect-boundaries` scopes (scaffold).
pub(crate) fn eff201(
    cx: &CheckContext<'_>,
    out: &mut Vec<Diagnostic>,
) -> Result<(), RuleConfigError> {
    let _ = (cx, out);
    Ok(())
}
