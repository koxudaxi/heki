//! Family 2 execution-effect rules: `EFF101`, `EFF105`, `EFF102`, `EFF103`, `EFF106`,
//! `EFF107`, `EFF201` (docs/ANALYSIS_AND_KB_SPEC.md section 2.4).

use crate::mut_rules::{
    CallTargetPattern, ContractTarget, SiteId, allows_fresh, initializes_itself,
    parse_call_targets, parse_read_targets, parse_targets, patterns, project_classes, site_callee,
    with_note, write_hits,
};
use crate::opt_rules::Sizes;
use crate::{CheckContext, DottedPattern, ModuleUnit, RuleConfigError, SitesByCaller};
use heki_ast::Span;
use heki_diagnostics::Diagnostic;
use heki_effect::guards::loop_bit;
use heki_effect::{
    BLOCKING_EFFECTS, BoundCallee, BoundaryReport, CallHop, CallSite, CallTarget, Construction,
    EffectSummary, Facts, FuncKey, ROOT_CAUSE_GROUP_THRESHOLD, Runner, SiteEffect, TypeCtx,
    WriteKind, anchors_call_site, boundary_reports_in, group_by_root_cause,
};
use heki_ir::{InstKind, IrFunction, VarId};
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
            let verb = if r.iterates { "iterates" } else { "calls" };
            let mut message = format!(
                "`async def {}` {verb} `{first}`, which reaches {what} `{}` ({}) on the event loop; offload it (`asyncio.to_thread`) or use an async API",
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
    let tcx = TypeCtx::new(&unit.input, cx.decls, cx.kb);
    let no_types = FxHashMap::default();
    for f in &unit.input.ir.functions {
        if f.is_class_body || f.loops.is_empty() {
            continue;
        }
        let key = CheckContext::key(unit, f);
        let migration_fn = f.name == "upgrade" || f.name == "downgrade";
        // Built when a block of a loop has an effect to report.
        let mut sizes: Option<Sizes<'_, '_>> = None;
        let mut data_sized: Vec<Option<bool>> = vec![None; f.loops.len()];
        // An iteration shares its span with the call producing the iterable: one report.
        let mut seen: FxHashSet<Span> = FxHashSet::default();
        for bb in &f.blocks {
            if f.block_loop_depth(bb.id) == 0 {
                continue;
            }
            // `for.iter` / `MakeIter` live in the preheader (depth 0), so they never get here.
            // Loops whose size the code fixes (small constant ranges and displays, retries,
            // daemon polling) are relaxed: a call is reported when some loop around it is
            // data-sized (the mask of those loops, as `SiteEffect::once_loops`), and the effect
            // does not run once per object across that loop (lazy initialization).
            let mut data_loops = || {
                let sizes = sizes.get_or_insert_with(|| Sizes::new(cx, &tcx, &no_types, f, unit));
                f.loops
                    .iter()
                    .enumerate()
                    .filter(|(i, lp)| {
                        lp.body.contains(&bb.id)
                            && *data_sized[*i].get_or_insert_with(|| sizes.data_sized(lp))
                    })
                    .fold(0, |mask, (_, lp)| mask | loop_bit(lp))
            };
            let mut checked: Option<u64> = None;
            for inst in &bb.insts {
                // Reading a property runs its getter (a call site at the read); iterating a
                // generator runs its body and iterating an object of a catalogued class its
                // `__iter__` (call sites at the iteration). Creating a generator expression
                // iterates its outermost iterable at once: only that catalogued `__iter__` counts
                // there, as the generator expression's body is a function of its own.
                let iteration = matches!(inst.kind, InstKind::MakeGenExpr { .. });
                if !iteration && !anchors_call_site(&inst.kind) {
                    continue;
                }
                let args: &[VarId] = match &inst.kind {
                    InstKind::Call { args, .. } | InstKind::MethodCall { args, .. } => args,
                    _ => &[],
                };
                let Some(list) = sites.get(&(key.qualname.clone(), inst.span)) else {
                    continue;
                };
                if !seen.insert(inst.span) {
                    continue;
                }
                let effects: Vec<SiteEffect> = list
                    .iter()
                    .filter(|s| !iteration || s.project_callees().is_empty())
                    .flat_map(|s| cx.index.site_effects(&unit.input.path, s))
                    .collect();
                let mut hot = || *checked.get_or_insert_with(&mut data_loops);
                // The shortest chain of an effect of `kind` that some data-sized loop runs on
                // every iteration.
                let mut per_iteration = |kind: ExecEffectKind| {
                    if !effects.iter().any(|e| e.effect == kind) {
                        return None;
                    }
                    let data = hot();
                    effects
                        .iter()
                        .filter(|e| e.effect == kind && data & !e.once_loops != 0)
                        .map(|e| e.chain.as_slice())
                        .min_by_key(|c| c.len())
                };
                if want102 && let Some(chain) = per_iteration(ExecEffectKind::UnpooledClientAlloc) {
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
                    && let Some(chain) = per_iteration(ExecEffectKind::DbQuery)
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

/// `forbid` tokens of `[[tool.heki.effect-boundaries]]` and the execution effects each one
/// forbids (`mutates-params`, `writes-targets`, `reads-targets`, and `calls` forbid facts of
/// another kind instead).
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
    calls: bool,
    call_targets: Vec<CallTargetPattern>,
    allowed_callees: Vec<DottedPattern>,
    note: Option<&'r str>,
    /// `fresh-objects = "allow"`: writes constructing the object they write are allowed.
    allow_fresh: bool,
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

    /// A Layer 2 symbol or a project class (or one of its modules / classes) that
    /// `allowed-callees` names.
    fn allows_symbol(&self, symbol: &str) -> bool {
        self.allowed_callees
            .iter()
            .any(|p| with_prefixes(symbol).any(|n| p.matches(n)))
    }

    /// Whether `allowed-callees` names everything `site` calls: the class it constructs, or
    /// each of its project callees.
    fn allows_call(&self, site: &CallSite) -> bool {
        match &site.constructs {
            Some(class) => self.allows_symbol(class),
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
            calls: forbids("calls"),
            call_targets: parse_call_targets(
                cx,
                &all_classes,
                &b.call_targets,
                "effect-boundaries.call-targets",
            )?,
            allowed_callees: patterns(&b.allowed_callees, "effect-boundaries.allowed-callees")?,
            note: b.message.as_deref(),
            allow_fresh: allows_fresh(b.fresh_objects.as_deref(), "effect-boundaries", &b.name)?,
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

/// The target of a reported `write-universe` write: the universe entry it matches, with a glob
/// attribute part replaced by the written attribute (`DataModel.*` -> `DataModel.fields`), so
/// writes to a subclass instance keep the class the configuration names.
fn undeclared_text(entry: &str, attr: &str) -> String {
    match entry.rsplit_once('.') {
        Some((class, pattern)) if pattern.contains(['*', '?', '[', '{']) => {
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
    /// By `call-targets` index.
    calls: BTreeMap<usize, Found>,
}

impl<'a> Reach<'a> {
    fn search(
        cx: &CheckContext<'_>,
        sites: &SitesByCaller<'a>,
        b: &Boundary<'_>,
        start: &FuncKey,
    ) -> Self {
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
                reach.collect_writes(cx, b, i, &func, own);
            }
            if b.reads_targets && runs_body {
                reach.collect_reads(cx, b, i, &func);
            }
            for &(path, site) in own {
                let hop = |caller: &FuncKey, callee: String| CallHop {
                    caller: caller.clone(),
                    callee,
                    span: site.span,
                    path: path.to_path_buf(),
                };
                if b.calls && runs_body && !b.allows_call(site) {
                    for (ti, _) in b
                        .call_targets
                        .iter()
                        .enumerate()
                        .filter(|(_, t)| t.hit(site))
                    {
                        let nodes = &reach.nodes;
                        note(&mut reach.calls, ti, (func.fqn(), site.span), || {
                            let mut chain = chain_to(nodes, i);
                            chain.push(hop(&func, site_callee(site)));
                            let kind = if site.constructs.is_some() {
                                "constructs"
                            } else {
                                "calls"
                            };
                            (chain, site.span, kind)
                        });
                    }
                }
                match &site.target {
                    CallTarget::Layer2 {
                        symbol, effects, ..
                    } => {
                        if !runs_body || b.allows_symbol(symbol) {
                            continue;
                        }
                        for e in effects.iter().filter(|e| b.effects.contains(*e)) {
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
            i += 1;
        }
        reach
    }

    /// Writes node `i` performs: its own writes, and `IndirectHelper` writes created at its own
    /// call sites (a helper whose effects count mutating the field it is handed). Writes
    /// propagated from callees are left to the callees' own nodes. A write is forbidden when it
    /// matches `write-targets` (with `writes-targets`) or `write-universe`, and `allow-writes`
    /// does not name it.
    fn collect_writes(
        &mut self,
        cx: &CheckContext<'_>,
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
                // is handed, so the function handing it over reports the write.
                ([hop], WriteKind::IndirectHelper)
                    if own.iter().any(|(_, site)| {
                        site.span == hop.span
                            && site.project_callees().iter().any(|bc| {
                                bc.callee.fqn() == hop.callee
                                    && b.counts(cx, site, &bc.callee, Facts::Body).is_some()
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
            .flat_map(|s| &s.typed_field_read_sites);
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
        for (i, (func, facts, _)) in self.nodes.iter().enumerate() {
            let local = cx
                .index
                .file_of(func)
                .and_then(|p| cx.index.files[p].local_functions.get(func))
                .filter(|s| *facts == s.body_facts())
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
                    let message = with_note(
                        format!(
                            "`{}` writes `{attr}` ({}{via}), which effect boundary `{}` does not allow",
                            key.qualname, found.kind, b.name
                        ),
                        b.note,
                    );
                    emit(found, attr.clone(), message);
                }
            }
        }
        for (ti, found) in &self.reads {
            let target = &b.read_targets[*ti].text;
            let via = via_first(found);
            let message = forbids(format!(
                "`{}` reads `{target}` ({}{via})",
                key.qualname, found.kind
            ));
            emit(found, target.clone(), message);
        }
        for (ti, found) in &self.calls {
            let target = &b.call_targets[*ti].text;
            let via = match found.chain.as_slice() {
                [first, _, ..] => format!(" (via `{}`)", first.callee),
                _ => String::new(),
            };
            let message = forbids(format!("`{}` {} `{target}`{via}", key.qualname, found.kind));
            emit(found, target.clone(), message);
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
