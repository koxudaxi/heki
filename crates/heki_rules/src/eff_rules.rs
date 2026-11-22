//! Family 2 execution-effect rules: `EFF101`, `EFF105`, `EFF102`, `EFF103`, `EFF106`,
//! `EFF107`, `EFF109`, `EFF110`, `EFF201` (docs/ANALYSIS_AND_KB_SPEC.md section 2.4).

use crate::mut_rules::{
    CallTargets, ContractTarget, FunctionMatcher, FunctionMatchers, References, SiteId, Subclasses,
    allows_fresh, call_verb, counts_references, group_texts, helper_changes_field,
    initializes_itself, parse_call_targets, parse_read_targets, parse_targets, patterns,
    project_classes, reports_import_probes, site_callee, write_hits,
};
use crate::opt_rules::{Creators, Sizes};
use crate::premises::{self, OpenCause};
use crate::{
    CheckContext, DottedPattern, ModuleUnit, RuleConfigError, SitesByCaller, has_glob_syntax,
    split_last_dot,
};
use heki_ast::Span;
use heki_diagnostics::{Certainty, Diagnostic, OpenPremise, OpenRecord};
use heki_effect::guards::{loop_bit, per_item_bit};
use heki_effect::with_paths;
use heki_effect::{
    AccessKind, BLOCKING_EFFECTS, BoundCallee, BoundaryReport, CallHop, CallSite, CallTarget,
    Construction, EffectSummary, ExternalAccess, Facts, FuncKey, HopGap, MustGap, PerItem, Runner,
    SiteEffect, SiteMust, TypeCtx, WriteKind, anchors_call_site, boundary_reports_in,
};
pub(crate) use heki_ir::ItemOrigins;
use heki_ir::walk::{self, Answer, Budget, FnShape, JoinMap, Or, Sparse, fixpoint};
use heki_ir::{BlockId, Carries, InstKind, IrFunction, ItemOrigin, ItemSource, Must, Progress};
use heki_kb::{EcosystemEntry, EnvironmentAccess, ExecEffectKind, ResultItems, WaitKind};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::{Arc, LazyLock};

/// The rules whose reports are split by the must-facts of execution effects
/// (`heki_effect::EffectSummary::must_exec`): an established report is the exact rule's, the
/// rest its heuristic sibling's.
pub(crate) const MUST_EFFECT_RULES: &[&str] = &["EFF101", "EFF111", "EFF103", "EFF113", "DATA101"];

/// Modules whose own code is an ORM implementation (EFF103 is about ORM *users*).
const ORM_PACKAGES: &[&str] = &["sqlalchemy", "django.db", "sqlmodel"];
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
    relax_tests && is_test_code(&key.qualname, &unit.rel_path)
}

/// Test code as pytest discovers it by default: a `test_*` function (a method of a test class
/// included) and the functions nested in one, and every function of a `test_*.py` or
/// `*_test.py` file or of a file under a `test/` or `tests/` directory.
pub(crate) fn is_test_code(qualname: &str, rel_path: &str) -> bool {
    let mut components = rel_path.split('/').peekable();
    let mut in_tests = false;
    let mut file = "";
    while let Some(c) = components.next() {
        if components.peek().is_some() {
            in_tests |= c == "tests" || c == "test";
        } else {
            file = c;
        }
    }
    let stem = file.strip_suffix(".py").unwrap_or(file);
    in_tests
        || stem.starts_with("test_")
        || stem.ends_with("_test")
        || qualname.split('.').any(|part| part.starts_with("test_"))
}

/// The code of a boundary report: `EFF105` for the file system, `EFF101` when whenever the call
/// returns it has run a wait for input or output (`BoundaryReport::must`), `EFF111` otherwise.
fn boundary_code(r: &BoundaryReport) -> &'static str {
    if r.effect == ExecEffectKind::BlockingFs {
        "EFF105"
    } else if premises::tier("EFF101", r.must.is_some() && waits_for_io(r.effect))
        == Certainty::Exact
    {
        "EFF101"
    } else {
        "EFF111"
    }
}

/// The half of `EFF101`'s "Waits" the effect decides: a wait for input or output, not for
/// another thread's progress.
fn waits_for_io(effect: ExecEffectKind) -> bool {
    effect.wait_kind() == Some(WaitKind::Io)
}

/// The premises of `EFF101`'s claim an `EFF111` report leaves open: the steps of its must-fact
/// that do not hold along its chain (`BoundaryReport::gaps`), and the effect when it is no wait
/// for input or output.
fn eff111_open_premises(cx: &CheckContext<'_>, r: &BoundaryReport) -> Vec<OpenPremise> {
    let mut open = chain_open_premises(cx, &premises::CONDITIONAL_WAIT, &r.chain, &r.gaps);
    if !waits_for_io(r.effect) {
        let last = r.chain.last().map(|h| cx.hop_location(h));
        open.push(premises::WAITS_ON_ANOTHER_THREAD.open_at(last, None));
    }
    debug_assert!(
        !open.is_empty(),
        "an EFF111 report records why it is not EFF101"
    );
    open
}

/// The open premises the gaps of a must-fact along `chain` stand for (each at its hop), with
/// `conditional` the cause of a last call whose effect the catalog makes conditional: "Waits"
/// for `EFF101`, "Sends" for `EFF103`.
fn chain_open_premises(
    cx: &CheckContext<'_>,
    conditional: &'static OpenCause,
    chain: &[CallHop],
    gaps: &[HopGap],
) -> Vec<OpenPremise> {
    gaps.iter()
        .map(|g| {
            let (cause, about): (&'static OpenCause, Option<&FuncKey>) = match &g.gap {
                MustGap::NotReached => (&premises::NEVER_REACHED, None),
                MustGap::NeverReturns => (&premises::NEVER_RETURNS, None),
                MustGap::NotOnEveryReturn => (&premises::NOT_MADE_ON_EVERY_RETURN, None),
                MustGap::IncompleteTargets => (&premises::INCOMPLETE_TARGETS, None),
                MustGap::NotRunAtCall(target) => (&premises::NOT_RUN_AT_THE_CALL, target.as_ref()),
                MustGap::Decorated(target) => (&premises::DECORATED, Some(target)),
                MustGap::DefinedTwice(target) => (&premises::DEFINED_TWICE, Some(target)),
                MustGap::WithoutTheFact(target) => {
                    (&premises::IMPLEMENTATION_WITHOUT_THE_FACT, Some(target))
                }
                MustGap::Conditional => (conditional, None),
            };
            let at = chain.get(g.hop).map(|h| cx.hop_location(h));
            cause.open_at(at, about.map(|k| format!("`{}`", k.fqn())))
        })
        .collect()
}

/// The message of an `EFF101` report: the waiting call the reported call is, the one it always
/// runs through helpers, or the one each implementation runs.
fn always_waits(function: &str, must: &SiteMust) -> String {
    match (must.chain.as_slice(), must.witnesses.as_slice()) {
        ([] | [_], _) => format!(
            "`{function}` runs the waiting call `{}` on the event loop",
            must.target()
        ),
        ([first, ..], [witness]) => format!(
            "`{}` always runs the waiting call `{}` on the event loop ({} down, on every path)",
            first.callee,
            witness.callee,
            match must.chain.len() - 1 {
                1 => "1 call".to_owned(),
                n => format!("{n} calls"),
            }
        ),
        ([first, ..], witnesses) => {
            let method = first.callee.rsplit('.').next().unwrap_or(&first.callee);
            format!(
                "`{method}` always runs a waiting call on the event loop: {}",
                each_witness(witnesses)
            )
        }
    }
}

/// The calls of several implementations' chains with the effect, each named with the function
/// that makes it.
fn each_witness(witnesses: &[Arc<CallHop>]) -> String {
    witnesses
        .iter()
        .map(|w| format!("`{}` (in `{}`)", w.callee, w.caller.qualname))
        .collect::<Vec<_>>()
        .join(" or ")
}

pub(crate) fn eff101_105(cx: &CheckContext<'_>, out: &mut Vec<Diagnostic>) {
    if !["EFF101", "EFF111", "EFF105"]
        .iter()
        .any(|code| cx.selection.is_enabled(code))
    {
        return;
    }
    let relax_tests = cx.config.lint.relax_async_in_tests.unwrap_or(true);
    let mut eff101: Vec<BoundaryReport> = Vec::new();
    let mut eff111: Vec<BoundaryReport> = Vec::new();
    let mut eff105: Vec<BoundaryReport> = Vec::new();
    // The boundaries of the checked files: all of them for `heki check`, the edited one for
    // the LSP engine.
    let reports = boundary_reports_in(
        cx.index,
        cx.units.iter().map(|u| u.input.path.as_path()),
        cx.must_explainer.as_ref(),
    );
    for r in reports {
        let code = boundary_code(&r);
        let unit = cx.unit(&r.path).expect(REPORTS_OF_UNITS);
        let relaxed_here = cx
            .index
            .summary(&r.function)
            .is_some_and(|s| relaxed(s, &r.function, unit, relax_tests));
        if !cx.enabled(code, unit) || relaxed_here {
            continue;
        }
        match code {
            "EFF105" => eff105.push(r),
            "EFF101" => eff101.push(r),
            _ => eff111.push(r),
        }
    }
    for (code, reports) in [("EFF101", eff101), ("EFF111", eff111), ("EFF105", eff105)] {
        for r in &reports {
            let unit = cx.unit(&r.path).expect(REPORTS_OF_UNITS);
            let what = if code == "EFF105" {
                "the blocking filesystem call"
            } else {
                "the blocking call"
            };
            // A call chain starts at a call or a property read; an iteration is named, as the
            // hop shows it as a call of the generator function.
            let iterating = match r.chain.first() {
                Some(h) if r.iterates => format!(" by iterating `{}`", h.callee),
                _ => String::new(),
            };
            // An exact report names, keys, and folds by the call it claims: the end of the chain
            // every implementation runs, not of the shortest chain the heuristic tier found.
            let (message, chain, target) = match &r.must {
                Some(must) if code == "EFF101" => (
                    always_waits(&r.function.qualname, must),
                    must.related(),
                    must.target(),
                ),
                _ => (
                    format!(
                        "`{}` reaches {what} `{}` on the event loop{iterating}",
                        r.function.qualname, r.root_cause
                    ),
                    r.chain.clone(),
                    r.root_cause.clone(),
                ),
            };
            let fold_key = format!("{target}|{}", effect_name(r.effect));
            let open_premises = if cx.open_premises && code == "EFF111" {
                eff111_open_premises(cx, r)
            } else {
                Vec::new()
            };
            if let Some(d) = cx.emit(
                out,
                code,
                unit,
                r.span,
                &r.function.qualname,
                target,
                message,
                Some(unblocking_help(r.effect).to_owned()),
                &chain,
            ) {
                d.fold_key = Some(fold_key);
                d.open_records = open_premises.into_iter().map(OpenRecord::from).collect();
            }
        }
    }
}

/// Async boundaries reaching one blocking root cause are folded into the first beyond this many.
pub const ROOT_CAUSE_GROUP_THRESHOLD: usize = 10;

/// Root-Cause Grouping of output: when more than [`ROOT_CAUSE_GROUP_THRESHOLD`] of `diags`
/// share a rule and a [`Diagnostic::fold_key`], only the first (by path and position) stays,
/// with the count of the others in its message. Run on the reports that are shown, after
/// suppressions and baselines: each report is a site of its own for both.
pub fn fold_root_causes(diags: &mut Vec<Diagnostic>) {
    let mut groups: BTreeMap<(&str, &str), Vec<usize>> = BTreeMap::new();
    for (i, d) in diags.iter().enumerate() {
        if let Some(key) = &d.fold_key {
            groups.entry((&d.rule_code, key)).or_default().push(i);
        }
    }
    let mut folded: FxHashMap<usize, usize> = FxHashMap::default();
    let mut dropped: FxHashSet<usize> = FxHashSet::default();
    for members in groups
        .into_values()
        .filter(|m| m.len() > ROOT_CAUSE_GROUP_THRESHOLD)
    {
        let first = *members
            .iter()
            .min_by_key(|&&i| (&diags[i].path, diags[i].span, &diags[i].qualname))
            .expect("a group has members");
        folded.insert(first, members.len() - 1);
        dropped.extend(members.into_iter().filter(|&i| i != first));
    }
    for (&i, &more) in &folded {
        let d = &mut diags[i];
        // An exact report's group shares the call each of its reports claims.
        d.message = if d.rule_code == "EFF101" {
            format!(
                "{} (+{more} more async call sites that always run `{}`)",
                d.message, d.target
            )
        } else {
            format!(
                "{} (+{more} more async call sites reaching the same blocking root cause)",
                d.message
            )
        };
    }
    *diags = std::mem::take(diags)
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !dropped.contains(i))
        .map(|(_, d)| d)
        .collect();
}

/// What keeps the event loop running instead of a blocking call of effect `e`.
fn unblocking_help(e: ExecEffectKind) -> &'static str {
    match e {
        ExecEffectKind::Sleep => {
            "use `await asyncio.sleep(...)`, or offload it with `asyncio.to_thread`"
        }
        ExecEffectKind::BlockingDb => "offload it with `asyncio.to_thread`, or use an async driver",
        ExecEffectKind::BlockingSubprocess => {
            "offload it with `asyncio.to_thread`, or use `asyncio.create_subprocess_exec`"
        }
        ExecEffectKind::BlockingSync => {
            "offload it with `asyncio.to_thread`, or use the `asyncio` primitive (`asyncio.Lock`, `asyncio.Event`, `asyncio.Queue`)"
        }
        ExecEffectKind::BlockingFs => {
            "offload it with `asyncio.to_thread`, or use an async file API"
        }
        // Network I/O; the other effects never block (`ExecEffectKind::BLOCKING`).
        _ => "offload it with `asyncio.to_thread`, or use an async client",
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

/// The call with the effect, in parentheses, when the call a loop makes reaches it through
/// helpers (a direct call names it already).
fn through_helpers(chain: &[CallHop]) -> String {
    match chain {
        [_, .., last] => format!(" (`{}`)", last.callee),
        _ => String::new(),
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
    let want103 = (cx.enabled("EFF103", unit) || cx.enabled("EFF113", unit))
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
    // The functions where a builtin calls a project callable once per item: their calls outside
    // loops may repeat too.
    let per_item_callers: FxHashSet<&str> = sites
        .all()
        .filter(|s| s.per_item.is_some())
        .map(|s| s.caller.qualname.as_str())
        .collect();
    let tcx = TypeCtx::new(&unit.input, cx.decls, cx.kb);
    let no_types = FxHashMap::default();
    let constants = cx.fixed_constants(unit);
    let creators = Creators::new(cx, &tcx, &no_types, unit, &constants);
    let item_origins: std::cell::OnceCell<ItemOrigins<'_>> = std::cell::OnceCell::new();
    for f in &unit.input.ir.functions {
        if f.is_class_body {
            continue;
        }
        let key = CheckContext::key(unit, f);
        let per_item_calls = per_item_callers.contains(key.qualname.as_str());
        if f.loops.is_empty() && !per_item_calls {
            continue;
        }
        let migration_fn = f.name == "upgrade" || f.name == "downgrade";
        let wanted = |e: &SiteEffect| match e.effect {
            ExecEffectKind::UnpooledClientAlloc => want102,
            ExecEffectKind::DbQuery => want103 && !migration_fn,
            _ => false,
        };
        let sizes = Sizes::new(cx, &tcx, &no_types, FnShape::new(f), unit, &constants);
        let shape = &sizes.ix.shape;
        let mut data_sized: Vec<Option<bool>> = vec![None; f.loops.len()];
        // An iteration shares its span with the call producing the iterable: one report.
        let mut seen: FxHashSet<Span> = FxHashSet::default();
        let mut queries: Vec<LoopQuery<'_>> = Vec::new();
        for bb in &f.blocks {
            let depth = shape.loop_depth(bb.id);
            if depth == 0 && !per_item_calls {
                continue;
            }
            // `for.iter` / `MakeIter` live in the preheader (depth 0), so they never get here.
            // Loops whose size the code fixes (small constant ranges and displays, retries,
            // daemon polling) are relaxed: a call is reported when some loop around it is
            // data-sized (the mask of those loops, as `SiteEffect::once_loops`), and the effect
            // does not run once per object across that loop (lazy initialization). A builtin
            // calling a callable once per item is one more loop, inside the others, as long as
            // the items it iterates.
            let mut data_loops: Option<u64> = None;
            for (index, inst) in bb.insts.iter().enumerate() {
                // Reading a property runs its getter (a call site at the read); iterating a
                // generator runs its body and iterating an object of a catalogued class its
                // `__iter__` (call sites at the iteration). Creating a generator expression
                // iterates its outermost iterable at once: only that catalogued `__iter__` counts
                // there, as the generator expression's body is a function of its own.
                let iteration = matches!(inst.kind, InstKind::MakeGenExpr { .. });
                if !iteration && !anchors_call_site(&inst.kind) {
                    continue;
                }
                let list = sites.at(&key.qualname, inst.span);
                if list.is_empty() || depth == 0 && list.iter().all(|s| s.per_item.is_none()) {
                    continue;
                }
                if !seen.insert(inst.span) {
                    continue;
                }
                let effects: Vec<(SiteEffect, &CallSite)> = list
                    .iter()
                    .filter(|s| !iteration || s.project_callees().is_empty())
                    .flat_map(|s| {
                        cx.index
                            .site_effects(&unit.input.path, s)
                            .into_iter()
                            .map(|e| (e, *s))
                    })
                    .filter(|(e, _)| wanted(e))
                    .collect();
                if effects.is_empty() {
                    continue;
                }
                let around = *data_loops.get_or_insert_with(|| {
                    f.loops
                        .iter()
                        .enumerate()
                        .filter(|(i, lp)| {
                            shape.in_loop(*i, bb.id)
                                && *data_sized[*i]
                                    .get_or_insert_with(|| sizes.data_sized(lp, &creators))
                        })
                        .fold(0, |mask, (_, lp)| mask | loop_bit(lp))
                });
                // The shortest chain of an effect of `kind` that some data-sized loop runs on
                // every iteration, the builtin whose loop over its items does, and the call site
                // the chain starts at.
                let per_iteration = |kind: ExecEffectKind| {
                    effects
                        .iter()
                        .filter(|(e, _)| e.effect == kind)
                        .filter_map(|(e, site)| {
                            let per_item = site.per_item.as_deref();
                            let items = per_item
                                .filter(|p| sizes.items_data_sized(inst, &p.items))
                                .map_or(0, |_| per_item_bit(depth));
                            let repeated = (around | items) & !e.once_loops;
                            let by = per_item.filter(|_| repeated & items != 0);
                            (repeated != 0).then_some((e.chain.as_slice(), Repeats(by), *site))
                        })
                        .min_by_key(|(chain, _, _)| chain.len())
                };
                if let Some((chain, repeats, _)) =
                    per_iteration(ExecEffectKind::UnpooledClientAlloc)
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
                            "`{}` allocates an unpooled client{} {}",
                            chain[0].callee,
                            through_helpers(chain),
                            repeats.every()
                        ),
                        Some(format!(
                            "create one client or session before the {} and reuse it",
                            repeats.ahead_of()
                        )),
                        chain,
                    );
                }
                if let Some((chain, repeats, chain_site)) = per_iteration(ExecEffectKind::DbQuery) {
                    queries.push(LoopQuery {
                        at: (bb.id, index),
                        span: inst.span,
                        chain: chain.to_vec(),
                        chain_site,
                        repeats,
                        sites: list,
                    });
                }
            }
        }
        if queries.is_empty() {
            continue;
        }
        // The split is the same whichever of the two rules runs: `EFF113` reports the rest.
        let origins = item_origins.get_or_init(|| ItemOrigins::new(unit.input.ir));
        let verdicts = established_queries(cx, unit, &tcx, f, origins, &queries);
        for (q, verdict) in queries.iter().zip(verdicts) {
            let root = q.chain.last().map(|h| h.callee.clone()).unwrap_or_default();
            let exact = verdict.exact;
            let tier = premises::tier("EFF103", exact.is_some());
            let reported = match exact.filter(|_| tier == Certainty::Exact) {
                // An exact report names and keys the call it claims.
                Some(e) if cx.enabled("EFF103", unit) => cx.emit(
                    out,
                    "EFF103",
                    unit,
                    q.span,
                    &key.qualname,
                    e.must.target(),
                    e.message(),
                    Some(query_help(&q.repeats)),
                    &e.must.related(),
                ),
                Some(_) => None,
                None if cx.enabled("EFF113", unit) => cx.emit(
                    out,
                    "EFF113",
                    unit,
                    q.span,
                    &key.qualname,
                    root,
                    format!(
                        "`{}` runs a database query{} {}",
                        q.chain[0].callee,
                        through_helpers(&q.chain),
                        q.repeats.every()
                    ),
                    Some(query_help(&q.repeats)),
                    &q.chain,
                ),
                None => None,
            };
            if let Some(d) = reported.filter(|d| d.rule_code == "EFF113") {
                debug_assert!(
                    !cx.open_premises || !verdict.open.is_empty(),
                    "an EFF113 report records why it is not EFF103"
                );
                d.open_records = verdict.open.into_iter().map(OpenRecord::from).collect();
            }
        }
    }
}

/// A call in a loop that may run a database query on every iteration (`EFF103` / `EFF113`).
struct LoopQuery<'s> {
    /// The call's instruction (block, index).
    at: (BlockId, usize),
    span: Span,
    /// The shortest chain to the query.
    chain: Vec<CallHop>,
    /// The call site `chain` starts at.
    chain_site: &'s CallSite,
    repeats: Repeats<'s>,
    /// The call sites at the instruction.
    sites: &'s [&'s CallSite],
}

/// What establishes an `EFF103` report: the origins of the items the loop steps through, and
/// what the call has sent whenever it returns.
struct EstablishedQuery {
    origins: DataOrigins,
    must: SiteMust,
}

/// The decision on one `EFF103` claim ([`established_queries`]): established, or, when the run
/// records them ([`CheckContext::open_premises`]), the premises it leaves open.
struct QueryVerdict {
    exact: Option<EstablishedQuery>,
    open: Vec<OpenPremise>,
}

/// Where the items of a loop over data come from, as a report names them
/// ([`data_origin`]).
pub(crate) struct DataOrigins {
    names: Vec<String>,
    /// Some origin carries only its count (`map(f, ids)`): the loop runs once for each of its
    /// items without stepping through them.
    counts_only: bool,
}

impl DataOrigins {
    /// The origins of every item of `found`, or why the first one that is not the data's is not
    /// (a cause of "Origin").
    pub(crate) fn of(
        cx: &CheckContext<'_>,
        tcx: &TypeCtx<'_>,
        ir: &heki_ir::ModuleIr,
        found: &std::collections::BTreeSet<ItemOrigin>,
    ) -> Result<Self, &'static OpenCause> {
        let names = found
            .iter()
            .map(|o| data_origin(cx, tcx, ir, *o))
            .collect::<Result<Vec<String>, _>>()?;
        let counts_only = found.iter().any(|o| o.carries == Carries::Count);
        (!names.is_empty())
            .then_some(Self { names, counts_only })
            .ok_or(&premises::NO_ITEMS_FOLLOWED)
    }

    /// "every iteration over parameter `ids`", or "every iteration, once for each item of
    /// parameter `ids`" for a count: what a message says repeats.
    pub(crate) fn every_iteration(&self) -> String {
        let named = self.names.join(" and ");
        if self.counts_only {
            format!("every iteration, once for each item of {named}")
        } else {
            format!("every iteration over {named}")
        }
    }

    /// The end of a clause [`Self::every_iteration`] opens in the middle of a sentence.
    pub(crate) fn clause_end(&self) -> &'static str {
        if self.counts_only { "," } else { "" }
    }
}

impl EstablishedQuery {
    fn message(&self) -> String {
        let what = match (self.must.witnesses.as_slice(), self.must.chain.as_slice()) {
            ([witness], [first, _, ..]) => {
                format!("`{}` (through `{}`)", witness.callee, first.callee)
            }
            ([witness], _) => format!("`{}`", witness.callee),
            (witnesses, _) => each_witness(witnesses),
        };
        format!(
            "{}{} sends a database statement: {what}",
            self.origins.every_iteration(),
            self.origins.clause_end()
        )
    }
}

/// Which of `queries` in `f` are established (`EFF103`): the call sends a statement whenever it
/// returns ([`heki_effect::ProjectIndex::site_must`]) and runs on every iteration that completes
/// of a loop around it, exception edges included, whose items the data decides: a parameter or a
/// part of one, or a catalog call's rows, lines, directory entries, or the lines of its text
/// (`heki_kb::EcosystemEntry::items`). A loop whose trip count the code fixes is not one, at any
/// count. When the run records open premises, a claim that is not established says which, from
/// the same steps: those of the must-fact along the reported chain, and those of the loop around
/// the call that leaves the fewest open (the first such).
fn established_queries(
    cx: &CheckContext<'_>,
    unit: &ModuleUnit<'_>,
    tcx: &TypeCtx<'_>,
    f: &IrFunction,
    origins: &ItemOrigins<'_>,
    queries: &[LoopQuery<'_>],
) -> Vec<QueryVerdict> {
    let types = tcx.infer(f);
    let owner = "heki_rules::eff_rules::established_queries";
    let budget = Budget::of_function(owner, f);
    let path = &unit.input.path;
    with_paths(tcx, f, &types, owner, |paths, ix| {
        // The premises of loop `li` for the call at `at` sending with `progress`, told to
        // `steps` ("Every iteration", then "Origin"): the items' origins when every one holds.
        let loop_steps = |li: usize,
                          at: (BlockId, usize),
                          progress: Progress,
                          steps: &mut dyn FnMut(&'static OpenCause) -> ControlFlow<()>|
         -> Option<DataOrigins> {
            let mut open = false;
            if paths.on_every_iteration(li, at, progress) != Answer::Known(Must::Every) {
                open = true;
                if steps(&premises::NOT_EVERY_ITERATION).is_break() {
                    return None;
                }
            }
            if !ix.fixed_trips(&f.loops[li], &mut |_| None).is_open() {
                open = true;
                if steps(&premises::FIXED_BY_THE_CODE).is_break() {
                    return None;
                }
            }
            let Answer::Known(found) = origins.of_loop(&budget, f.id, li) else {
                let _ = steps(&premises::NO_ITEMS_FOLLOWED);
                return None;
            };
            match DataOrigins::of(cx, tcx, unit.input.ir, &found) {
                Ok(origins) => (!open).then_some(origins),
                Err(cause) => {
                    let _ = steps(cause);
                    None
                }
            }
        };
        let loops_around =
            |at: (BlockId, usize)| (0..f.loops.len()).filter(move |&li| ix.shape.in_loop(li, at.0));
        // The open premises of the loop around `q` that leaves the fewest open.
        let loop_open_premises = |q: &LoopQuery<'_>, progress: Progress| {
            let mut fewest: Option<Vec<OpenPremise>> = None;
            for li in loops_around(q.at) {
                let mut open: Vec<OpenPremise> = Vec::new();
                // The loop's first instruction in the source: its header's, or for a `while`
                // loop whose test is a phi, its body's.
                let lp = &f.loops[li];
                let first = std::iter::once(&lp.header)
                    .chain(&lp.body)
                    .filter_map(|b| f.block(*b).insts.first())
                    .min_by_key(|i| i.span.start);
                let at = first
                    .map(|i| cx.related_at(path, i.span, "the loop around the call".to_owned()));
                loop_steps(li, q.at, progress, &mut |cause| {
                    let premise = cause.open_at(at.clone(), None);
                    if !open.contains(&premise) {
                        open.push(premise);
                    }
                    ControlFlow::Continue(())
                });
                if fewest.as_ref().is_none_or(|f| open.len() < f.len()) {
                    fewest = Some(open);
                }
            }
            fewest.unwrap_or_default()
        };
        queries
            .iter()
            .map(|q| {
                if let Some(by) = q.repeats.0 {
                    let open = cx.open_premises.then(|| {
                        let mut open = query_chain_open_premises(cx, path, q);
                        let at = cx.related_at(path, q.span, "the call".to_owned());
                        let builtin = format!("`{}`", by.api);
                        open.push(premises::REPEATED_BY_A_BUILTIN.open_at(Some(at), Some(builtin)));
                        open
                    });
                    return QueryVerdict {
                        exact: None,
                        open: open.unwrap_or_default(),
                    };
                }
                let established = q.sites.iter().find_map(|s| {
                    cx.index
                        .site_must(path, s, ExecEffectKind::DbQuery)
                        .map(|m| (*s, m))
                });
                let Some((site, must)) = established else {
                    let open = cx.open_premises.then(|| {
                        let mut open = query_chain_open_premises(cx, path, q);
                        open.extend(loop_open_premises(q, progress_of(q.chain_site)));
                        open
                    });
                    return QueryVerdict {
                        exact: None,
                        open: open.unwrap_or_default(),
                    };
                };
                let exact = loops_around(q.at).find_map(|li| {
                    let origins =
                        loop_steps(li, q.at, progress_of(site), &mut |_| ControlFlow::Break(()))?;
                    Some(EstablishedQuery {
                        origins,
                        must: must.clone(),
                    })
                });
                let open = (exact.is_none() && cx.open_premises)
                    .then(|| loop_open_premises(q, progress_of(site)));
                QueryVerdict {
                    exact,
                    open: open.unwrap_or_default(),
                }
            })
            .collect()
    })
}

/// How far a call has to get for what it sends to count: a Layer 2 call when it is made (its
/// statement went out also when it raised), a project call when it returned.
fn progress_of(site: &CallSite) -> Progress {
    match site.target {
        CallTarget::Layer2 { .. } => Progress::Called,
        CallTarget::Project(_) => Progress::Returned,
    }
}

/// The premises of `EFF103`'s claim the must-fact of the query `q` leaves open along its chain
/// (`heki_effect::MustExplainer::gaps_along`).
fn query_chain_open_premises(
    cx: &CheckContext<'_>,
    path: &Path,
    q: &LoopQuery<'_>,
) -> Vec<OpenPremise> {
    let gaps = cx
        .must_explainer
        .as_ref()
        .map_or_else(Vec::new, |explainer| {
            explainer.gaps_along(path, q.chain_site, ExecEffectKind::DbQuery, &q.chain)
        });
    chain_open_premises(cx, &premises::CONDITIONAL_STATEMENT, &q.chain, &gaps)
}

/// How a report names `origin` when the data decides the number of its items: a parameter or
/// a part of one, or a catalog call sizing its result (`heki_kb::EcosystemEntry::items`), in
/// the function the origin names (a generator expression's loop reads its creator's iterable).
/// Otherwise why the data does not (a cause of "Origin"): a fixed collection, a method's
/// receiver or a call heki does not take as the data, or a library call the catalog does not
/// size.
pub(crate) fn data_origin(
    cx: &CheckContext<'_>,
    tcx: &TypeCtx<'_>,
    ir: &heki_ir::ModuleIr,
    origin: ItemOrigin,
) -> Result<String, &'static OpenCause> {
    let not_the_data = &premises::ITEMS_NOT_FROM_THE_DATA;
    let f = ir.function(origin.function);
    let param = |p: u8| {
        f.params
            .get(usize::from(p))
            .map(|p| p.name.as_str())
            .ok_or(not_the_data)
    };
    let named = match origin.source {
        ItemSource::Param(p) => format!("parameter `{}`", param(p)?),
        ItemSource::ParamPart(p) => format!("a part of parameter `{}`", param(p)?),
        ItemSource::Call { block, index } => {
            let inst = usize::try_from(index)
                .ok()
                .and_then(|i| f.block(block).insts.get(i))
                .ok_or(not_the_data)?;
            let types = tcx.infer(f);
            let callee = match &inst.kind {
                InstKind::Call { callee_var, .. } => {
                    tcx.resolve_call(callee_var.and_then(|v| types.get(&v)))
                }
                InstKind::MethodCall { recv, method, .. } => {
                    tcx.resolve_method(types.get(recv), method)
                }
                _ => return Err(not_the_data),
            };
            let heki_effect::Callee::External(symbol) = callee else {
                return Err(not_the_data);
            };
            let unsized_call = &premises::LIBRARY_CALL_WITHOUT_ITEMS;
            let entry = cx.kb.lookup(&symbol).ok_or(unsized_call)?;
            let what = match (entry.items.ok_or(unsized_call)?, origin.text) {
                (ResultItems::Rows, false) => "the rows",
                (ResultItems::Lines, false) => "the lines",
                (ResultItems::Entries, false) => "the entries",
                (ResultItems::Text, true) => "the lines of the text",
                _ => return Err(unsized_call),
            };
            // A catalog class's method is named by the library API it is.
            format!("{what} of `{}`", entry.api.as_deref().unwrap_or(&symbol))
        }
        ItemSource::Receiver => return Err(not_the_data),
        ItemSource::Fixed => return Err(&premises::FIXED_BY_THE_CODE),
    };
    Ok(named)
}

/// The advice of an `EFF103` / `EFF113` report, which its message leaves out.
fn query_help(repeats: &Repeats<'_>) -> String {
    format!(
        "batch the queries, or load the data once before the {}",
        repeats.ahead_of()
    )
}

/// What repeats an effect `EFF102` / `EFF103` report: a data-sized loop around the call, or the
/// builtin calling the callee once per item (`sorted(xs, key=f)`).
struct Repeats<'s>(Option<&'s PerItem>);

impl Repeats<'_> {
    fn every(&self) -> String {
        match self.0 {
            Some(p) => format!(
                "for every item `{}` calls it with",
                p.api.strip_prefix("builtins.").unwrap_or(&p.api)
            ),
            None => "on every loop iteration".to_owned(),
        }
    }

    /// What the help moves the work in front of.
    fn ahead_of(&self) -> &'static str {
        match self.0 {
            Some(_) => "call",
            None => "loop",
        }
    }
}

mod fanout_exclusive;
mod held_across_io;
mod lock_retaken;
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

/// `EFF109` (opt-in): a non-reentrant lock taken again while it is held.
pub(crate) fn eff109(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    lock_retaken::run(cx, unit, out);
}

/// `EFF110` (opt-in): an object allowing one operation at a time, used by tasks started
/// together.
pub(crate) fn eff110(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    fanout_exclusive::run(cx, unit, out);
}

/// `forbid` tokens of `[[tool.heki.effect-boundaries]]` and the execution effects each one
/// forbids (`mutates-params`, `writes-targets`, `reads-targets`, `calls`, and
/// `reads-environment` forbid facts of another kind instead).
const FORBID_TOKENS: &[(&str, &[ExecEffectKind])] = &[
    ("blocking", BLOCKING_EFFECTS),
    ("blocking-net", &[ExecEffectKind::BlockingNet]),
    ("blocking-db", &[ExecEffectKind::BlockingDb]),
    ("blocking-fs", &[ExecEffectKind::BlockingFs]),
    ("blocking-subprocess", &[ExecEffectKind::BlockingSubprocess]),
    ("blocking-sync", &[ExecEffectKind::BlockingSync]),
    ("sleep", &[ExecEffectKind::Sleep]),
    ("mutates-params", &[]),
    ("writes-targets", &[]),
    ("reads-targets", &[]),
    ("calls", &[]),
    ("reads-environment", &[]),
];

/// `UnknownForbid::expected`, built from [`FORBID_TOKENS`].
static FORBID_EXPECTED: LazyLock<String> = LazyLock::new(|| {
    let tokens: Vec<String> = FORBID_TOKENS
        .iter()
        .map(|(t, _)| format!("`{t}`"))
        .collect();
    format!("one of {}", tokens.join(", "))
});

/// One parsed `[[tool.heki.effect-boundaries]]` contract.
struct Boundary<'r> {
    name: &'r str,
    scopes: Vec<DottedPattern>,
    effects: BTreeSet<ExecEffectKind>,
    mutates_params: bool,
    writes_targets: bool,
    write_targets: Vec<ContractTarget>,
    /// Writes reported unless `allow_writes` names them (with or without `writes-targets`).
    write_universe: Vec<ContractTarget>,
    allow_writes: Vec<ContractTarget>,
    reads_targets: bool,
    read_targets: Vec<ContractTarget>,
    /// `read-targets` as names of module attributes outside the project, by the same index.
    external_reads: FunctionMatchers,
    calls: bool,
    call_targets: CallTargets,
    /// The references that count for `calls` (`references`).
    references: References,
    allowed_callees: Vec<DottedPattern>,
    note: Option<&'r str>,
    /// `fresh-objects = "allow"`: writes constructing the object they write are allowed.
    allow_fresh: bool,
    reads_environment: bool,
    /// `import-probes = "report"`: `reads-environment` also forbids import probes.
    import_probes: bool,
}

/// Whether a pattern names `key`, its qualname, or a module / class / function containing it.
pub(crate) fn names_function(patterns: &[DottedPattern], key: &FuncKey) -> bool {
    let fqn = key.fqn();
    patterns.iter().any(|p| p.names_within(&fqn, &key.qualname))
}

impl Boundary<'_> {
    /// A function of the boundary's scopes, or a module body a scope names as `<module>`
    /// (`pkg.gen:<module>`): a scope naming its module (`pkg.*`, `pkg.gen:*`) does not cover it.
    fn in_scope(&self, key: &FuncKey) -> bool {
        if key.qualname != "<module>" {
            return names_function(&self.scopes, key);
        }
        let fqn = key.fqn();
        self.scopes
            .iter()
            .any(|p| p.names_module_bodies() && p.names_within(&fqn, &key.qualname))
    }

    /// A Layer 2 symbol (or one of its modules / classes) that `allowed-callees` names.
    fn allows_symbol(&self, symbol: &str) -> bool {
        self.allowed_callees
            .iter()
            .any(|p| p.names_within(symbol, symbol))
    }

    /// Whether `allowed-callees` names everything `site` calls: the method the protocol typing
    /// the receiver declares ([`CallSite::declared`], what the call targets judge the call by),
    /// the class it constructs (or one of its modules / classes), or each of its project
    /// callees.
    fn allows_call(&self, cx: &CheckContext<'_>, site: &CallSite) -> bool {
        if let Some(declared) = site.declared() {
            return names_function(&self.allowed_callees, declared);
        }
        match &site.constructs {
            Some(class) => {
                let qualname = cx.decls.class_qualname(class).unwrap_or(class);
                self.allowed_callees
                    .iter()
                    .any(|p| p.names_within(class, qualname))
            }
            None => site
                .project_callees()
                .iter()
                .all(|bc| names_function(&self.allowed_callees, &bc.callee)),
        }
    }

    /// Which facts of a project callee count at `site`, run where the site's caller runs
    /// `into` ([`heki_effect::ProjectIndex::delivery`]): `allowed-callees` names neither the
    /// callee whose body runs nor the function iterating its result (`SiteRole::Through`). A
    /// function returning a generator is not checked itself: its own body does not run there,
    /// nor does a callee run on a worker thread (`Runner::Worker`).
    fn counts<'s>(
        &self,
        cx: &CheckContext<'_>,
        site: &'s CallSite,
        callee: &FuncKey,
        into: Facts,
    ) -> Option<heki_effect::Delivery<'s>> {
        if site.runner == Runner::Worker {
            return None;
        }
        let s = cx.index.summary(callee)?;
        let d = cx.index.delivery(site, s).filter(|d| d.into == into)?;
        let runs_body = d.from == s.body_facts();
        let allowed = runs_body && names_function(&self.allowed_callees, callee)
            || d.via
                .is_some_and(|p| names_function(&self.allowed_callees, p));
        (!allowed).then_some(d)
    }

    /// Whether the search enters a project callee: its facts count and, when its own body runs,
    /// it is not another function of the scopes (that one reports its own violations).
    fn follows<'s>(
        &self,
        cx: &CheckContext<'_>,
        site: &'s CallSite,
        callee: &FuncKey,
        into: Facts,
    ) -> Option<heki_effect::Delivery<'s>> {
        let d = self.counts(cx, site, callee, into)?;
        let own_body = cx
            .index
            .summary(callee)
            .is_some_and(|s| d.from == s.body_facts());
        (!(own_body && self.in_scope(callee))).then_some(d)
    }

    /// Whether the boundary checks writes: `writes-targets`, or a `write-universe`.
    fn checks_writes(&self) -> bool {
        self.writes_targets || !self.write_universe.is_empty()
    }

    /// Whether `reads-environment` forbids `access`: a call or a read the ecosystem catalog
    /// tags as one of the environment, an import of a module not established to be the
    /// project's, or an import probe with `import-probes = "report"`.
    fn forbids_environment(&self, cx: &CheckContext<'_>, access: &ExternalAccess) -> bool {
        let tagged = |kind| {
            cx.kb
                .lookup(&access.name)
                .and_then(EcosystemEntry::environment_access)
                == Some(kind)
        };
        self.reads_environment
            && match access.kind {
                AccessKind::Call => {
                    tagged(EnvironmentAccess::Call)
                        || tagged(EnvironmentAccess::Import) && !access.imports_project_module
                }
                AccessKind::Read => tagged(EnvironmentAccess::Read),
                AccessKind::ImportProbe => self.import_probes,
            }
    }
}

fn boundaries<'a>(cx: &CheckContext<'a>) -> Result<Vec<Boundary<'a>>, RuleConfigError> {
    let all_classes = project_classes(cx);
    let mut out = Vec::new();
    for b in &cx.config.effect_boundaries {
        let mut effects = BTreeSet::new();
        for token in &b.forbid {
            let Some((_, kinds)) = FORBID_TOKENS.iter().find(|(t, _)| t == token) else {
                return Err(RuleConfigError::UnknownForbid {
                    boundary: b.name.clone(),
                    token: token.clone(),
                    expected: FORBID_EXPECTED.as_str(),
                });
            };
            effects.extend(kinds.iter().copied());
        }
        let forbids = |token: &str| b.forbid.iter().any(|t| t == token);
        for (token, key, targets) in [
            ("writes-targets", "write-targets", &b.write_targets),
            ("reads-targets", "read-targets", &b.read_targets),
            ("calls", "call-targets", &b.call_targets),
        ] {
            if forbids(token) && targets.is_empty() {
                return Err(RuleConfigError::MissingTargets {
                    boundary: b.name.clone(),
                    token,
                    key,
                });
            }
        }
        if !b.allow_writes.is_empty() && b.write_universe.is_empty() {
            return Err(RuleConfigError::AllowWritesWithoutUniverse(b.name.clone()));
        }
        let import_probes =
            reports_import_probes(b.import_probes.as_deref(), "effect-boundaries", &b.name)?;
        if import_probes && !forbids("reads-environment") {
            return Err(RuleConfigError::ImportProbesWithoutEnvironment(
                b.name.clone(),
            ));
        }
        let external_reads = b
            .read_targets
            .iter()
            .map(|t| FunctionMatcher::named(t, "effect-boundaries.read-targets"))
            .collect::<Result<Vec<_>, _>>()?;
        let external_reads =
            FunctionMatchers::new(external_reads, "effect-boundaries.read-targets")?;
        let targets = |texts: &[String], context: &'static str| {
            parse_targets(cx, &all_classes, texts, context, context)
        };
        out.push(Boundary {
            name: &b.name,
            scopes: patterns(&b.scopes, "effect-boundaries.scopes")?,
            effects,
            mutates_params: forbids("mutates-params"),
            writes_targets: forbids("writes-targets"),
            write_targets: targets(&b.write_targets, "effect-boundaries.write-targets")?,
            write_universe: targets(&b.write_universe, "effect-boundaries.write-universe")?,
            allow_writes: targets(&b.allow_writes, "effect-boundaries.allow-writes")?,
            reads_targets: forbids("reads-targets"),
            read_targets: parse_read_targets(
                cx,
                &all_classes,
                &b.read_targets,
                "effect-boundaries.read-targets",
                "effect-boundaries.read-targets",
            )?,
            external_reads,
            calls: forbids("calls"),
            call_targets: parse_call_targets(
                cx,
                &all_classes,
                &b.call_targets,
                "effect-boundaries.call-targets",
                Subclasses::Covered,
            )?,
            references: counts_references(b.references.as_deref(), "effect-boundaries", &b.name)?,
            allowed_callees: patterns(&b.allowed_callees, "effect-boundaries.allowed-callees")?,
            note: b.message.as_deref(),
            allow_fresh: allows_fresh(b.fresh_objects.as_deref(), "effect-boundaries", &b.name)?,
            reads_environment: forbids("reads-environment"),
            import_probes,
        });
    }
    Ok(out)
}

/// `EFF201`: functions of `effect-boundaries` scopes must not reach a forbidden effect,
/// directly or through calls. Each scope function is searched breadth first over the IR-free
/// call graph (so every chain is a shortest one), without entering `allowed-callees`, other
/// scope functions (they report for themselves), or `async def` bodies that do not run at the
/// call.
pub(crate) fn eff201(
    cx: &CheckContext<'_>,
    out: &mut Vec<Diagnostic>,
) -> Result<(), RuleConfigError> {
    if !cx.selection.is_enabled("EFF201") || cx.config.effect_boundaries.is_empty() {
        return Ok(());
    }
    let boundaries = boundaries(cx)?;
    let sites = cx.sites_by_caller();
    let references = if boundaries
        .iter()
        .any(|b| b.calls && b.references != References::Ignored)
    {
        cx.reference_sites_by_caller()
    } else {
        SitesByCaller::default()
    };
    for unit in cx.units {
        let mut done: FxHashSet<(FuncKey, usize)> = FxHashSet::default();
        for f in &unit.input.ir.functions {
            if f.is_class_body {
                continue;
            }
            let key = CheckContext::key(unit, f);
            for (bi, b) in boundaries.iter().enumerate() {
                if b.in_scope(&key) && done.insert((key.clone(), bi)) {
                    let reach = Reach::search(cx, (&sites, &references), b, &key);
                    reach.report(cx, b, unit, f, &key, out);
                }
            }
        }
    }
    Ok(())
}

/// Reached nodes in breadth-first order (`0`: the scope function's body): a function, which of
/// its facts, and the parent node with the hops from it.
type Nodes = Vec<(FuncKey, Facts, Option<(usize, Vec<CallHop>)>)>;

/// Hops from the scope function down to node `i`.
fn chain_to(nodes: &Nodes, mut i: usize) -> Vec<CallHop> {
    let mut chain = Vec::new();
    while let Some((parent, hops)) = &nodes[i].2 {
        chain.extend(hops.iter().rev().cloned());
        i = *parent;
    }
    chain.reverse();
    chain
}

/// The first (shortest) reached violation of one kind, and every distinct site of that kind the
/// scope function reaches (`Diagnostic::sites`).
struct Found {
    /// From the scope function; ends at the sink for an effect and at the target for a call.
    chain: Vec<CallHop>,
    /// The write or read itself (reported there when the scope function holds it).
    span: Span,
    /// The kind of the first site (`direct_attr`, ..., or `calls` / `constructs`).
    kind: &'static str,
    sites: BTreeSet<SiteId>,
}

/// Record `site` under `key`, keeping the first violation `first` builds.
fn note<K: Ord>(
    map: &mut BTreeMap<K, Found>,
    key: K,
    site: SiteId,
    first: impl FnOnce() -> (Vec<CallHop>, Span, &'static str),
) {
    map.entry(key)
        .or_insert_with(|| {
            let (chain, span, kind) = first();
            Found {
                chain,
                span,
                kind,
                sites: BTreeSet::new(),
            }
        })
        .sites
        .insert(site);
}

/// The first environment read of a scope function (`reads-environment`): its kind and name, and
/// every distinct site of every kind.
type EnvironmentFound = Option<(Found, AccessKind, String)>;

/// Record the environment read `site`, keeping the first one `first` builds.
fn note_environment(
    found: &mut EnvironmentFound,
    site: SiteId,
    first: impl FnOnce() -> (Vec<CallHop>, Span, AccessKind, String),
) {
    let (found, _, _) = found.get_or_insert_with(|| {
        let (chain, span, kind, name) = first();
        let found = Found {
            chain,
            span,
            kind: kind.as_str(),
            sites: BTreeSet::new(),
        };
        (found, kind, name)
    });
    found.sites.insert(site);
}

/// The target of a reported `write-universe` write: the universe entry it matches, with a glob
/// attribute part replaced by the written attribute (`DataModel.*` -> `DataModel.fields`), so
/// writes to a subclass instance keep the class the configuration names.
fn undeclared_text(entry: &str, attr: &str) -> String {
    match split_last_dot(entry) {
        Some((class, pattern)) if has_glob_syntax(pattern) => {
            format!("{class}.{attr}")
        }
        _ => entry.to_owned(),
    }
}

/// What a forbidden write is reported as: a `write-targets` entry (by index), or, for a
/// `write-universe` write that `allow-writes` does not name, its `undeclared_text`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum WriteBucket {
    Target(usize),
    Undeclared(String),
}

/// The part of the call graph one scope function reaches. A node is a function's facts of one
/// kind (`Facts`): its body, or what iterating the object it returns runs (a function returning
/// a generator hands out the generator's body, which runs where the object is iterated).
struct Reach<'a> {
    nodes: Nodes,
    /// Followed call edges `(caller node, callee node, binding)`.
    edges: Vec<(usize, usize, &'a BoundCallee)>,
    exec: BTreeMap<ExecEffectKind, Found>,
    writes: BTreeMap<WriteBucket, Found>,
    /// By `read-targets` index.
    reads: BTreeMap<usize, Found>,
    /// By the `call-targets` indices a call matches ([`called_targets`]).
    calls: BTreeMap<Vec<usize>, Found>,
    environment: EnvironmentFound,
}

impl<'a> Reach<'a> {
    /// Search from `start` over the call sites of `sites.0`; `sites.1` holds the reference
    /// sites, which count for `calls` with `references = "calls"`.
    fn search(
        cx: &CheckContext<'_>,
        sites: (&SitesByCaller<'a>, &SitesByCaller<'_>),
        b: &Boundary<'_>,
        start: &FuncKey,
    ) -> Self {
        let (sites, references) = sites;
        let start_facts = cx
            .index
            .summary(start)
            .map_or(Facts::Body, EffectSummary::body_facts);
        let mut reach = Self {
            nodes: vec![(start.clone(), start_facts, None)],
            edges: Vec::new(),
            exec: BTreeMap::new(),
            writes: BTreeMap::new(),
            reads: BTreeMap::new(),
            calls: BTreeMap::new(),
            environment: None,
        };
        let mut index_of: FxHashMap<(FuncKey, Facts), usize> = FxHashMap::default();
        index_of.insert((start.clone(), start_facts), 0);
        let mut i = 0;
        while i < reach.nodes.len() {
            let (func, facts) = (reach.nodes[i].0.clone(), reach.nodes[i].1);
            // The node runs the function's own body, or only what its returned object hands out
            // (the `Return` sites of the object its calls return).
            let runs_body = cx
                .index
                .summary(&func)
                .is_some_and(|s| facts == s.body_facts());
            let into = if runs_body {
                Facts::Body
            } else {
                Facts::Result
            };
            let own: &[(&'a Path, &'a CallSite)] = sites.get(&func).map_or(&[], Vec::as_slice);
            if b.checks_writes() && runs_body {
                reach.collect_writes(cx, sites, b, i, &func, own);
            }
            if b.reads_targets && runs_body {
                reach.collect_reads(cx, b, i, &func);
            }
            if runs_body {
                reach.collect_external(cx, b, i, &func);
            }
            for &(path, site) in own {
                let hop = |caller: &FuncKey, callee: String| CallHop {
                    caller: caller.clone(),
                    callee,
                    span: site.span,
                    path: path.to_path_buf(),
                };
                if b.calls && runs_body && !b.allows_call(cx, site) {
                    reach.note_calls(b, i, &func, (path, site), false);
                }
                match &site.target {
                    CallTarget::Layer2 {
                        symbol, effects, ..
                    } => {
                        if !runs_body || b.allows_symbol(symbol) {
                            continue;
                        }
                        let on_full = cx.index.while_full_effects(site).into_iter().flatten();
                        for e in effects
                            .iter()
                            .chain(on_full)
                            .filter(|e| b.effects.contains(*e))
                        {
                            let nodes = &reach.nodes;
                            note(&mut reach.exec, *e, (func.fqn(), site.span), || {
                                let mut chain = chain_to(nodes, i);
                                chain.push(hop(&func, symbol.clone()));
                                (chain, site.span, "")
                            });
                        }
                    }
                    CallTarget::Project(callees) => {
                        for bc in callees {
                            let Some(d) = b.follows(cx, site, &bc.callee, into) else {
                                continue;
                            };
                            let hops = match d.via {
                                None => vec![hop(&func, bc.callee.fqn())],
                                Some(p) => vec![hop(&func, p.fqn()), hop(p, bc.callee.fqn())],
                            };
                            let node = (bc.callee.clone(), d.from);
                            let j = *index_of.entry(node).or_insert_with(|| {
                                reach
                                    .nodes
                                    .push((bc.callee.clone(), d.from, Some((i, hops))));
                                reach.nodes.len() - 1
                            });
                            reach.edges.push((i, j, bc));
                        }
                    }
                }
            }
            if b.calls && runs_body {
                let handed = references.get(&func).map_or(&[][..], Vec::as_slice);
                let counted = handed
                    .iter()
                    .filter(|(_, site)| b.references.counts(site) && !b.allows_call(cx, site));
                for &entry in counted {
                    reach.note_calls(b, i, &func, entry, true);
                }
            }
            i += 1;
        }
        reach
    }

    /// Note the `call-targets` groups `site` of node `i` (function `func`, in `path`) calls, or
    /// hands on (`reference`).
    fn note_calls(
        &mut self,
        b: &Boundary<'_>,
        i: usize,
        func: &FuncKey,
        (path, site): (&Path, &CallSite),
        reference: bool,
    ) {
        for group in b.call_targets.called(site) {
            let nodes = &self.nodes;
            note(&mut self.calls, group, (func.fqn(), site.span), || {
                let mut chain = chain_to(nodes, i);
                chain.push(CallHop {
                    caller: func.clone(),
                    callee: site_callee(site),
                    span: site.span,
                    path: path.to_path_buf(),
                });
                (chain, site.span, call_verb(site, reference))
            });
        }
    }

    /// Writes node `i` performs: its own writes, and `IndirectHelper` writes created at its own
    /// call sites (a helper whose effects count mutating the field it is handed). Writes
    /// propagated from callees are left to the callees' own nodes. A write is forbidden when it
    /// matches `write-targets` (with `writes-targets`) or `write-universe`, and `allow-writes`
    /// does not name it.
    fn collect_writes(
        &mut self,
        cx: &CheckContext<'_>,
        sites: &SitesByCaller<'_>,
        b: &Boundary<'_>,
        i: usize,
        func: &FuncKey,
        own: &[(&Path, &CallSite)],
    ) {
        debug_assert!(
            cx.index.propagates_typed_writes(),
            "`typed_writes` decides for EFF201"
        );
        let writes = cx
            .index
            .summary(func)
            .into_iter()
            .flat_map(|s| &s.typed_field_write_sites);
        for w in writes {
            let extra = match (w.via_chain.as_slice(), w.kind) {
                ([], _) => None,
                // A helper of the scopes counts too: it cannot see the type of the field it
                // is handed, so the function handing it over reports the write. The helper's
                // summary also holds what its callees change, so the write counts only when the
                // helper changes the argument outside `allowed-callees`.
                ([hop], WriteKind::IndirectHelper)
                    if own.iter().any(|(_, site)| {
                        site.span == hop.span
                            && site.project_callees().iter().any(|bc| {
                                bc.callee.fqn() == hop.callee
                                    && b.counts(cx, site, &bc.callee, Facts::Body).is_some()
                                    && helper_changes_field(cx, sites, bc, w, |site, callee| {
                                        b.counts(cx, site, callee, Facts::Body)
                                    })
                            })
                    }) =>
                {
                    Some(CallHop::clone(hop))
                }
                _ => continue,
            };
            if b.allow_writes.iter().any(|a| a.matches_write(w))
                || b.allow_fresh && w.construction == Construction::Yes
            {
                continue;
            }
            // A class's own initialization (constructing a fresh object) is not a write.
            let own_init =
                |t: &ContractTarget| initializes_itself(cx, func, &t.classes, w.obj_params);
            let mut buckets: Vec<WriteBucket> = Vec::new();
            if b.writes_targets {
                buckets.extend(
                    write_hits(&b.write_targets, w)
                        .into_iter()
                        .filter(|(_, t)| !own_init(t))
                        .map(|(ti, _)| WriteBucket::Target(ti)),
                );
            }
            if buckets.is_empty()
                && let Some((_, t)) = write_hits(&b.write_universe, w)
                    .into_iter()
                    .find(|(_, t)| !own_init(t))
            {
                let text = match &t.key {
                    Some(_) => t.text.clone(),
                    None => undeclared_text(&t.text, &w.attr),
                };
                buckets.push(WriteBucket::Undeclared(text));
            }
            for bucket in buckets {
                let nodes = &self.nodes;
                note(&mut self.writes, bucket, (func.fqn(), w.span), || {
                    let mut chain = chain_to(nodes, i);
                    chain.extend(extra.clone());
                    (chain, w.span, w.kind.as_str())
                });
            }
        }
    }

    /// Reads of `read-targets` node `i` performs (read sites are local facts of every
    /// summary); an initializer reading its own instance is no read.
    fn collect_reads(&mut self, cx: &CheckContext<'_>, b: &Boundary<'_>, i: usize, func: &FuncKey) {
        let reads = cx
            .index
            .summary(func)
            .into_iter()
            .flat_map(|s| &s.typed_field_read_sites)
            .filter(|r| r.counts_for_contracts());
        for r in reads {
            for (ti, _) in b.read_targets.iter().enumerate().filter(|(_, t)| {
                t.matches(&r.class_qualname, &r.attr)
                    && !initializes_itself(cx, func, &t.classes, r.obj_params)
            }) {
                let nodes = &self.nodes;
                note(&mut self.reads, ti, (func.fqn(), r.span), || {
                    (chain_to(nodes, i), r.span, r.kind.as_str())
                });
            }
        }
    }

    /// What node `i` accesses outside the project in its own body that the boundary forbids
    /// (local facts of the index, kept for what some boundary forbids): calls of external
    /// `call-targets` (`calls`), reads of module attributes `read-targets` names
    /// (`reads-targets`), and the environment reads of `reads-environment`. A call
    /// `allowed-callees` names is allowed.
    fn collect_external(
        &mut self,
        cx: &CheckContext<'_>,
        b: &Boundary<'_>,
        i: usize,
        func: &FuncKey,
    ) {
        let accesses = cx.index.file_of(func).into_iter().flat_map(|path| {
            let own = cx.index.files[path].external_accesses.get(func);
            own.into_iter().flatten().map(move |a| (path, a))
        });
        for (path, a) in accesses {
            let call = a.kind == AccessKind::Call;
            if call && b.allows_symbol(&a.name) {
                continue;
            }
            let site: SiteId = (func.fqn(), a.span);
            let nodes = &self.nodes;
            // A call's chain ends with the hop to the function called; node `i` reads or
            // imports itself.
            let chain = || {
                let mut chain = chain_to(nodes, i);
                chain.extend(call.then(|| CallHop {
                    caller: func.clone(),
                    callee: a.name.clone(),
                    span: a.span,
                    path: path.to_path_buf(),
                }));
                chain
            };
            let (calls, reads) = match a.kind {
                AccessKind::Call if b.calls => (
                    b.call_targets.functions.naming_external(&a.name),
                    Vec::new(),
                ),
                AccessKind::Read if b.reads_targets => {
                    (Vec::new(), b.external_reads.naming_external(&a.name))
                }
                _ => (Vec::new(), Vec::new()),
            };
            for t in calls {
                note(&mut self.calls, vec![t], site.clone(), || {
                    (chain(), a.span, a.kind.as_str())
                });
            }
            for t in reads {
                note(&mut self.reads, t, site.clone(), || {
                    (chain(), a.span, a.kind.as_str())
                });
            }
            if b.forbids_environment(cx, a) {
                note_environment(&mut self.environment, site, || {
                    (chain(), a.span, a.kind, a.name.clone())
                });
            }
        }
    }

    /// Parameters of the scope function that a reached function mutates, through followed
    /// edges only (a forward flow of "callee parameter k is scope parameter p, or an object it
    /// holds" bits, and of "callee parameter k is a shallow copy of scope parameter p" bits,
    /// whose changes count only below the copy's top level), and the distinct sites of those
    /// changes (each in the body making it).
    fn mutated_params(&self, cx: &CheckContext<'_>, params: usize) -> (u64, BTreeSet<SiteId>) {
        // Per node, the scope parameters each of its parameters holds: `(any change counts, only
        // changes below the top level count)`.
        type Held = JoinMap<usize, (Or, Or)>;
        let mut into: Vec<Vec<(usize, &BoundCallee)>> = vec![Vec::new(); self.nodes.len()];
        let mut out_of: Vec<Vec<usize>> = vec![Vec::new(); self.nodes.len()];
        for &(from, to, bc) in &self.edges {
            into[to].push((from, bc));
            out_of[from].push(to);
        }
        let readers = |i: usize, out: &mut Vec<usize>| out.extend(&out_of[i]);
        let transfer = |node: usize, known: &walk::Facts<Held>| -> Option<Held> {
            let mut held = Held::new();
            if node == 0 {
                for p in 0..params.min(64) {
                    held.insert(p, (Or(1u64 << p), Or(0)));
                }
            }
            for &(from, bc) in &into[node] {
                let of = |p: u8| {
                    let bits = known.get(from).and_then(|h| h.get(&usize::from(p)));
                    bits.map_or((0, 0), |(full, below)| (full.0, below.0))
                };
                for (k, o) in bc.params.iter().enumerate() {
                    // The parameter itself keeps what counts; a part of it, or of a copy of it,
                    // is below the top level.
                    let mut bits = (0, 0);
                    if let Some((p, part)) = &o.param {
                        let (full, below) = of(*p);
                        bits.0 |= full;
                        if part.is_some() {
                            bits.0 |= below;
                        } else {
                            bits.1 |= below;
                        }
                    }
                    for (p, _) in &o.param_elements {
                        let (full, below) = of(*p);
                        bits.0 |= full | below;
                    }
                    // A copy of it, or of a part of it, shares only what is below its own top
                    // level.
                    if let Some((p, _)) = &o.param_copy {
                        let (full, below) = of(*p);
                        bits.1 |= full | below;
                    }
                    if bits != (0, 0) {
                        held.insert(k, (Or(bits.0), Or(bits.1)));
                    }
                }
            }
            Some(held)
        };
        // A node is visited once, then again only after a node calling it gains a bit of one of
        // its parameters' two masks: at most the bits of every node's parameters.
        let parameters: usize = (0..self.nodes.len())
            .map(|j| {
                let called = into[j].iter().map(|(_, bc)| bc.params.len()).max();
                called.unwrap_or(0).max(if j == 0 { params } else { 0 })
            })
            .sum();
        let spec = Sparse {
            nodes: self.nodes.len(),
            readers: &readers,
            max_visits: u32::try_from(1 + 2 * 64 * parameters).unwrap_or(u32::MAX),
        };
        let budget = Budget::unmetered("eff_rules::Reach::mutated_params");
        // Open: no change of a parameter is known, and none is reported.
        let Answer::Known(found) = fixpoint(&budget, spec, transfer) else {
            return (0, BTreeSet::new());
        };
        let alias = |i: usize, k: u8| {
            let bits = found.get(i).and_then(|h| h.get(&usize::from(k)));
            bits.map_or((0, 0), |(full, below)| (full.0, below.0))
        };
        let mut out = 0;
        let mut sites = BTreeSet::new();
        for (i, (func, facts, _)) in self.nodes.iter().enumerate() {
            let Some(local) = cx
                .index
                .file_of(func)
                .and_then(|p| cx.index.files[p].local_functions.get(func))
                .filter(|s| *facts == s.body_facts())
            else {
                continue;
            };
            for k in (0..64u8).filter(|k| local.mutates_params & (1u64 << k) != 0) {
                let (full, below) = alias(i, k);
                let below = below
                    & if local
                        .changed_param_attrs
                        .get(&k)
                        .is_some_and(|a| !a.is_empty())
                    {
                        u64::MAX
                    } else {
                        0
                    };
                let scope_params = full | below;
                if scope_params != 0 {
                    out |= scope_params;
                    let spans = local.param_change_sites.get(&k).into_iter().flatten();
                    sites.extend(spans.map(|span| (func.fqn(), *span)));
                }
            }
        }
        (out, sites)
    }

    fn report(
        &self,
        cx: &CheckContext<'_>,
        b: &Boundary<'_>,
        unit: &ModuleUnit<'_>,
        f: &IrFunction,
        key: &FuncKey,
        out: &mut Vec<Diagnostic>,
    ) {
        let forbids = |what: String| format!("{what}, which effect boundary `{}` forbids", b.name);
        let help = || b.note.map(str::to_owned);
        let mut emit = |found: &Found, target: String, message: String| {
            let span = found.chain.first().map_or(found.span, |h| h.span);
            if let Some(d) = cx.emit(
                out,
                "EFF201",
                unit,
                span,
                &key.qualname,
                format!("{}:{target}", b.name),
                message,
                help(),
                &found.chain,
            ) {
                d.sites = u32::try_from(found.sites.len()).unwrap_or(u32::MAX);
            }
        };
        for (effect, found) in &self.exec {
            let sink = found.chain.last().map_or("", |h| h.callee.as_str());
            let via = match found.chain.as_slice() {
                [first, _, ..] => format!(" via `{}`", first.callee),
                _ => String::new(),
            };
            let name = effect_name(*effect);
            let message = forbids(format!("`{}` reaches {name} (`{sink}`{via})", key.qualname));
            emit(found, name.to_owned(), message);
        }
        let via_first = |found: &Found| {
            found
                .chain
                .first()
                .map(|h| format!(" via `{}`", h.callee))
                .unwrap_or_default()
        };
        for (bucket, found) in &self.writes {
            let via = via_first(found);
            match bucket {
                WriteBucket::Target(ti) => {
                    let target = &b.write_targets[*ti].text;
                    let message = forbids(format!(
                        "`{}` writes `{target}` ({}{via})",
                        key.qualname, found.kind
                    ));
                    emit(found, target.clone(), message);
                }
                WriteBucket::Undeclared(attr) => {
                    let message = format!(
                        "`{}` writes `{attr}` ({}{via}), which effect boundary `{}` does not allow",
                        key.qualname, found.kind, b.name
                    );
                    emit(found, attr.clone(), message);
                }
            }
        }
        // Reads and calls name their kind in the target: one boundary may forbid writing and
        // reading the same `Class.attr`, and a report of each must stay a report of its own.
        for (ti, found) in &self.reads {
            let target = &b.read_targets[*ti].text;
            let via = via_first(found);
            let message = forbids(format!(
                "`{}` reads `{target}` ({}{via})",
                key.qualname, found.kind
            ));
            emit(found, format!("reads:{target}"), message);
        }
        for (group, found) in &self.calls {
            let (target, called) = group_texts(&b.call_targets, group, None);
            let via = match found.chain.as_slice() {
                [first, _, ..] => format!(" (via `{}`)", first.callee),
                _ => String::new(),
            };
            let message = forbids(format!("`{}` {} {called}{via}", key.qualname, found.kind));
            emit(found, format!("calls:{target}"), message);
        }
        if let Some((found, kind, name)) = &self.environment {
            let what = match kind {
                AccessKind::Call => format!("calls `{name}`"),
                AccessKind::Read => format!("reads `{name}`"),
                AccessKind::ImportProbe => format!("probes the import of `{name}`"),
            };
            // A call's chain ends at the function called.
            let calls_through = match kind {
                AccessKind::Call => found.chain.len() > 1,
                _ => !found.chain.is_empty(),
            };
            let via = if calls_through {
                via_first(found)
            } else {
                String::new()
            };
            let message = forbids(format!(
                "`{}` reads the environment ({what}{via})",
                key.qualname
            ));
            emit(found, "reads-environment".to_owned(), message);
        }
        if b.mutates_params {
            let (bits, sites) = self.mutated_params(cx, f.params.len());
            if bits != 0 {
                let names: Vec<String> = f
                    .params
                    .iter()
                    .take(64)
                    .enumerate()
                    .filter(|(k, _)| bits & (1u64 << k) != 0)
                    .map(|(_, p)| format!("`{}`", p.name))
                    .collect();
                if let Some(d) = cx.emit(
                    out,
                    "EFF201",
                    unit,
                    f.span,
                    &key.qualname,
                    format!("{}:mutates-params", b.name),
                    forbids(format!(
                        "`{}` mutates its parameters ({})",
                        key.qualname,
                        names.join(", ")
                    )),
                    help(),
                    &[],
                ) {
                    d.sites = u32::try_from(sites.len().max(1)).unwrap_or(u32::MAX);
                }
            }
        }
    }
}
