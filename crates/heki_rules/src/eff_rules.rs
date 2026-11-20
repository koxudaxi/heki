//! Family 2 execution-effect rules: `EFF101`, `EFF105`, `EFF102`, `EFF103`, `EFF106`,
//! `EFF107`, `EFF201` (docs/DETAILED_SPEC_M3_M7.md section 3.2).

use crate::mut_rules::{
    ContractTarget, initializes_itself, parse_targets, patterns, project_classes, with_note,
};
use crate::{CheckContext, DottedPattern, ModuleUnit, RuleConfigError, SitesByCaller};
use heki_ast::Span;
use heki_diagnostics::Diagnostic;
use heki_effect::{
    BLOCKING_EFFECTS, BoundCallee, BoundaryReport, CallHop, CallSite, CallTarget, EffectSummary,
    FuncKey, ROOT_CAUSE_GROUP_THRESHOLD, SiteEffect, WriteKind, boundary_reports_in,
    group_by_root_cause,
};
use heki_ir::{InstKind, IrFunction, LoopBound};
use heki_kb::ExecEffectKind;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::LazyLock;

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

/// `EFF106`: `await` / blocking network I/O while a sync lock or DB transaction is held
/// (scaffold).
pub(crate) fn eff106(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    let _ = (cx, unit, out);
}

/// `EFF107`: a locally bound `asyncio.create_task` result that is never retained (scaffold).
pub(crate) fn eff107(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    let _ = (cx, unit, out);
}

/// `forbid` tokens of `[[tool.heki.effect-boundaries]]` and the execution effects each one
/// forbids (`mutates-params` / `writes-targets` forbid mutation facts instead).
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
    allowed_callees: Vec<DottedPattern>,
    note: Option<&'r str>,
}

/// `name` and each of its dotted prefixes (`pkg.mod.f`, then `pkg` and `pkg.mod`).
fn with_prefixes(name: &str) -> impl Iterator<Item = &str> {
    std::iter::once(name).chain(name.match_indices('.').map(|(i, _)| &name[..i]))
}

/// Whether a pattern names `key`, its qualname, or a module / class / function containing it.
fn names_function(patterns: &[DottedPattern], key: &FuncKey) -> bool {
    let fqn = key.fqn();
    patterns.iter().any(|p| {
        with_prefixes(&fqn)
            .chain(with_prefixes(&key.qualname))
            .any(|n| p.matches(n))
    })
}

impl Boundary<'_> {
    /// A function of the boundary's scopes (module bodies are not functions).
    fn in_scope(&self, key: &FuncKey) -> bool {
        key.qualname != "<module>" && names_function(&self.scopes, key)
    }

    /// A Layer 2 symbol (or one of its modules / classes) that `allowed-callees` names.
    fn allows_symbol(&self, symbol: &str) -> bool {
        self.allowed_callees
            .iter()
            .any(|p| with_prefixes(symbol).any(|n| p.matches(n)))
    }

    /// Whether the effects of a project callee count at `site`: its body runs at the call and
    /// `allowed-callees` does not name it.
    fn counts(&self, cx: &CheckContext<'_>, site: &CallSite, callee: &FuncKey) -> bool {
        cx.index
            .summary(callee)
            .is_some_and(|s| site.runs_body_of(s))
            && !names_function(&self.allowed_callees, callee)
    }

    /// Whether the search enters a project callee: its effects count and it is not another
    /// function of the scopes (that one reports its own violations).
    fn follows(&self, cx: &CheckContext<'_>, site: &CallSite, callee: &FuncKey) -> bool {
        self.counts(cx, site, callee) && !self.in_scope(callee)
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
        let writes_targets = b.forbid.iter().any(|t| t == "writes-targets");
        if writes_targets && b.write_targets.is_empty() {
            return Err(RuleConfigError::MissingWriteTargets(b.name.clone()));
        }
        out.push(Boundary {
            name: &b.name,
            scopes: patterns(&b.scopes, "effect-boundaries.scopes")?,
            effects,
            mutates_params: b.forbid.iter().any(|t| t == "mutates-params"),
            writes_targets,
            write_targets: parse_targets(
                cx,
                &all_classes,
                &b.write_targets,
                "effect-boundaries.write-targets",
                "effect-boundaries.write-targets",
            )?,
            allowed_callees: patterns(&b.allowed_callees, "effect-boundaries.allowed-callees")?,
            note: b.message.as_deref(),
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
    for unit in cx.units {
        let mut done: FxHashSet<(FuncKey, usize)> = FxHashSet::default();
        for f in &unit.input.ir.functions {
            if f.is_class_body || f.is_module_body {
                continue;
            }
            let key = CheckContext::key(unit, f);
            for (bi, b) in boundaries.iter().enumerate() {
                if b.in_scope(&key) && done.insert((key.clone(), bi)) {
                    let reach = Reach::search(cx, &sites, b, &key);
                    reach.report(cx, b, unit, f, &key, out);
                }
            }
        }
    }
    Ok(())
}

/// The part of the call graph one scope function reaches.
struct Reach<'a> {
    /// Reached functions in breadth-first order (`0`: the scope function) with the parent
    /// node and the hop from it.
    nodes: Vec<(FuncKey, Option<(usize, CallHop)>)>,
    /// Followed call edges `(caller node, callee node, binding)`.
    edges: Vec<(usize, usize, &'a BoundCallee)>,
    /// Shortest chain to each forbidden execution effect.
    exec: BTreeMap<ExecEffectKind, Vec<CallHop>>,
    /// First (shortest) write of each write target: chain, write span, kind.
    writes: BTreeMap<usize, (Vec<CallHop>, Span, WriteKind)>,
}

impl<'a> Reach<'a> {
    fn search(
        cx: &CheckContext<'_>,
        sites: &SitesByCaller<'a>,
        b: &Boundary<'_>,
        start: &FuncKey,
    ) -> Self {
        let mut reach = Self {
            nodes: vec![(start.clone(), None)],
            edges: Vec::new(),
            exec: BTreeMap::new(),
            writes: BTreeMap::new(),
        };
        let mut index_of: FxHashMap<FuncKey, usize> = FxHashMap::default();
        index_of.insert(start.clone(), 0);
        let mut i = 0;
        while i < reach.nodes.len() {
            let func = reach.nodes[i].0.clone();
            let own: &[(&'a Path, &'a CallSite)] = sites.get(&func).map_or(&[], Vec::as_slice);
            if b.writes_targets {
                reach.collect_writes(cx, b, i, &func, own);
            }
            for &(path, site) in own {
                let hop = |callee: String| CallHop {
                    caller: func.clone(),
                    callee,
                    span: site.span,
                    path: path.to_path_buf(),
                };
                match &site.target {
                    CallTarget::Layer2 {
                        symbol, effects, ..
                    } => {
                        if b.allows_symbol(symbol) {
                            continue;
                        }
                        for e in effects.iter().filter(|e| b.effects.contains(*e)) {
                            if !reach.exec.contains_key(e) {
                                let mut chain = reach.chain_to(i);
                                chain.push(hop(symbol.clone()));
                                reach.exec.insert(*e, chain);
                            }
                        }
                    }
                    CallTarget::Project(callees) => {
                        for bc in callees.iter().filter(|bc| b.follows(cx, site, &bc.callee)) {
                            let j = *index_of.entry(bc.callee.clone()).or_insert_with(|| {
                                reach
                                    .nodes
                                    .push((bc.callee.clone(), Some((i, hop(bc.callee.fqn())))));
                                reach.nodes.len() - 1
                            });
                            reach.edges.push((i, j, bc));
                        }
                    }
                }
            }
            i += 1;
        }
        reach
    }

    /// Hops from the scope function down to node `i`.
    fn chain_to(&self, mut i: usize) -> Vec<CallHop> {
        let mut chain = Vec::new();
        while let Some((parent, hop)) = &self.nodes[i].1 {
            chain.push(hop.clone());
            i = *parent;
        }
        chain.reverse();
        chain
    }

    /// Writes of `write-targets` node `i` performs: its own writes, and `IndirectHelper` writes
    /// created at its own call sites (a helper whose effects count mutating the field it is
    /// handed). Writes propagated from callees are left to the callees' own nodes.
    fn collect_writes(
        &mut self,
        cx: &CheckContext<'_>,
        b: &Boundary<'_>,
        i: usize,
        func: &FuncKey,
        own: &[(&Path, &CallSite)],
    ) {
        let writes = cx
            .index
            .summary(func)
            .into_iter()
            .flat_map(|s| &s.typed_field_write_sites);
        for w in writes {
            let extra = match (w.via_chain.as_slice(), w.kind) {
                ([], _) => None,
                // A helper of the scopes counts too: it cannot see the type of the field it
                // is handed, so the function handing it over reports the write.
                ([hop], WriteKind::IndirectHelper)
                    if own.iter().any(|(_, site)| {
                        site.span == hop.span
                            && site.project_callees().iter().any(|bc| {
                                bc.callee.fqn() == hop.callee && b.counts(cx, site, &bc.callee)
                            })
                    }) =>
                {
                    Some(hop.clone())
                }
                _ => continue,
            };
            // A class's own initialization (constructing a fresh object) is not a write.
            for (ti, _) in b.write_targets.iter().enumerate().filter(|(_, t)| {
                t.matches(&w.class_qualname, &w.attr)
                    && !initializes_itself(cx, func, &t.classes, w.obj_params)
            }) {
                if !self.writes.contains_key(&ti) {
                    let mut chain = self.chain_to(i);
                    chain.extend(extra.clone());
                    self.writes.insert(ti, (chain, w.span, w.kind));
                }
            }
        }
    }

    /// Parameters of the scope function that a reached function mutates, through followed
    /// edges only (a forward flow of "callee parameter k is scope parameter p" bits).
    fn mutated_params(&self, cx: &CheckContext<'_>, params: usize) -> u64 {
        let mut alias: FxHashMap<(usize, usize), u64> = FxHashMap::default();
        for p in 0..params.min(64) {
            alias.insert((0, p), 1u64 << p);
        }
        loop {
            let mut changed = false;
            for &(from, to, bc) in &self.edges {
                for (k, o) in bc.params.iter().enumerate() {
                    let Some((p, _)) = &o.param else {
                        continue;
                    };
                    let bits = alias.get(&(from, usize::from(*p))).copied().unwrap_or(0);
                    let slot = alias.entry((to, k)).or_insert(0);
                    if *slot | bits != *slot {
                        *slot |= bits;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        let mut out = 0;
        for (i, (func, _)) in self.nodes.iter().enumerate() {
            let local = cx
                .index
                .file_of(func)
                .and_then(|p| cx.index.files[p].local_functions.get(func))
                .map_or(0, |s| s.mutates_params);
            for k in (0..64).filter(|k| local & (1u64 << k) != 0) {
                out |= alias.get(&(i, k)).copied().unwrap_or(0);
            }
        }
        out
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
        let forbids = |what: String| {
            with_note(
                format!("{what}, which effect boundary `{}` forbids", b.name),
                b.note,
            )
        };
        for (effect, chain) in &self.exec {
            let sink = chain.last().map_or("", |h| h.callee.as_str());
            let via = match chain.as_slice() {
                [first, _, ..] => format!(" via `{}`", first.callee),
                _ => String::new(),
            };
            cx.emit(
                out,
                "EFF201",
                unit,
                chain[0].span,
                &key.qualname,
                format!("{}:{}", b.name, effect_name(*effect)),
                forbids(format!(
                    "`{}` reaches {} (`{sink}`{via})",
                    key.qualname,
                    effect_name(*effect)
                )),
                chain,
            );
        }
        for (ti, (chain, span, kind)) in &self.writes {
            let target = &b.write_targets[*ti].text;
            let via = chain
                .first()
                .map(|h| format!(" via `{}`", h.callee))
                .unwrap_or_default();
            cx.emit(
                out,
                "EFF201",
                unit,
                chain.first().map_or(*span, |h| h.span),
                &key.qualname,
                format!("{}:{target}", b.name),
                forbids(format!(
                    "`{}` writes `{target}` ({}{via})",
                    key.qualname,
                    kind.as_str()
                )),
                chain,
            );
        }
        if b.mutates_params {
            let bits = self.mutated_params(cx, f.params.len());
            if bits != 0 {
                let names: Vec<String> = f
                    .params
                    .iter()
                    .take(64)
                    .enumerate()
                    .filter(|(k, _)| bits & (1u64 << k) != 0)
                    .map(|(_, p)| format!("`{}`", p.name))
                    .collect();
                cx.emit(
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
                    &[],
                );
            }
        }
    }
}
