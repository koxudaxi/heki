//! `EFF114` (`recursion-error-as-value`, opt-in): an `except` clause that takes the
//! `RecursionError` a call cycle reached from its `try` body can raise, and that no path through
//! completes by raising, so the function goes on with a value that depends on the depth of the
//! stack instead of the error.
//!
//! - **Call cycle**: a call in the `try` body runs, through calls whose bodies run on its stack,
//!   a function on a call cycle (`EffectSummary::call_cycle`). A call heki cannot resolve is not
//!   followed.
//! - **Catches RecursionError**: the clause is the first of its `try` whose classes include
//!   `RecursionError` (`RecursionError`, `RuntimeError`, `Exception`, `BaseException`, a bare
//!   `except`, or a tuple holding one, as the builtins the module does not rebind); an earlier
//!   clause of classes heki cannot tell may take it first, and stops the report. `except*`
//!   clauses are left out.
//! - **Completes normally**: every path through the clause's body ends in a fall-through, a
//!   `return`, a `break`, or a `continue`, none in a `raise` or a call that ends the process.
//!   Instructions in the body that may raise are not counted (the premise heki leaves open).
//!
//! A clause that reports the error (to `stderr`, through `logging` with the exception, or
//! `traceback`) and returns an exit status (an integer constant on every `return`) is a CLI
//! boundary: the claim holds there, and the code is deliberate.

use crate::{CheckContext, ModuleUnit};
use heki_ast::Span;
use heki_diagnostics::Diagnostic;
use heki_effect::consumption::resolve_inst;
use heki_effect::{CallCycle, CallSite, Callee, Facts, FuncKey, Runner, SiteRole, TypeCtx, Val};
use heki_ir::walk::{Answer, Budget, FnShape, Visit, reach};
use heki_ir::{BlockId, BuiltinKind, ConstLiteral, Inst, InstKind, IrFunction, Terminator, VarId};
use rustc_hash::{FxHashMap, FxHashSet};
use std::rc::Rc;

/// The builtin classes a `RecursionError` is an instance of.
const RECURSION_ERROR_CLASSES: &[&str] = &[
    "builtins.RecursionError",
    "builtins.RuntimeError",
    "builtins.Exception",
    "builtins.BaseException",
];

/// Calls that never return normally: they end the process.
const EXITS: &[&str] = &[
    "sys.exit",
    "os._exit",
    "os.abort",
    "builtins.exit",
    "builtins.quit",
];

/// The methods of `logging.Logger` that log a message.
const LOG_LEVELS: &[&str] = &[
    "debug",
    "info",
    "warning",
    "warn",
    "error",
    "exception",
    "critical",
    "fatal",
    "log",
];

/// Calls that report the exception being handled: its traceback, printed to `stderr`.
const TRACEBACK_PRINTS: &[&str] = &["traceback.print_exc", "traceback.print_exception"];

/// Whether an `except` clause takes a `RecursionError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Takes {
    Yes,
    No,
    /// Its classes are not all known (a name bound to a tuple, a class of unknown value).
    Unknown,
}

/// One `except` clause of a `try`: the block testing its classes, the block its body starts in
/// (with the `CatchException`), the source text of its classes (none for a bare `except`), the
/// variable it binds the exception to, and its span.
#[derive(Debug, Clone, Copy)]
struct Clause<'f> {
    test: BlockId,
    body: BlockId,
    types: &'f [String],
    caught: Option<VarId>,
    span: Span,
}

impl<'f> Clause<'f> {
    /// The clause tested in block `test` whose body starts in block `body`, when `body` starts
    /// by binding an exception.
    fn at(f: &'f IrFunction, test: BlockId, body: BlockId) -> Option<Self> {
        let inst = f.block(body).insts.first()?;
        match &inst.kind {
            InstKind::CatchException { dst, exc_types } => Some(Self {
                test,
                body,
                types: exc_types,
                caught: *dst,
                span: inst.span,
            }),
            _ => None,
        }
    }
}

/// What a `RecursionError` reaching a `try`'s dispatch block does there.
#[derive(Debug, Clone, Copy)]
enum Dispatch {
    /// The clause at this index takes it.
    TakenBy(usize),
    /// No clause takes it: the block ending the dispatch raises it again.
    PassedOn(BlockId),
    /// A clause may take it or not (unknown classes, `except*`).
    Undecided,
}

pub(super) fn run(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.enabled("EFF114", unit) {
        return;
    }
    let tcx = TypeCtx::new(&unit.input, cx.decls, cx.kb);
    let mut sites: Option<SitesOfFunctions<'_>> = None;
    let mut chains = CycleChains::default();
    for f in unit.input.ir.functions.iter().filter(|f| !f.is_class_body) {
        let has_handler = f.blocks.iter().any(|b| {
            matches!(
                b.insts.first().map(|i| &i.kind),
                Some(InstKind::CatchException { .. })
            )
        });
        if !has_handler {
            continue;
        }
        let sites = sites.get_or_insert_with(|| sites_of_functions(cx, unit));
        let key = CheckContext::key(unit, f);
        let Some(own_sites) = sites.get(key.qualname.as_str()) else {
            continue;
        };
        let reaches_cycle = own_sites
            .values()
            .flatten()
            .any(|site| cycle_at(cx, site).is_some());
        if !reaches_cycle {
            continue;
        }
        let handlers = Handlers::of(f, unit.source, &tcx);
        if !handlers
            .dispatches
            .values()
            .any(|d| matches!(d, Dispatch::TakenBy(_)))
        {
            continue;
        }
        let types = tcx.infer(f);
        let shape = FnShape::new(f);
        let budget = Budget::of_function("EFF114", f);
        let exits = |inst: &Inst| {
            ends_process(&tcx, &types, &inst.kind)
                || own_sites
                    .get(&inst.span)
                    .is_some_and(|at| at.iter().any(|site| never_returns(cx, site)))
        };
        for (dispatch, body_blocks) in handlers.try_bodies(f) {
            let Some(Dispatch::TakenBy(k)) = handlers.dispatches.get(&dispatch).copied() else {
                continue;
            };
            let Some(witness) = cycle_in_try_body(cx, f, &body_blocks, own_sites) else {
                continue;
            };
            let clause = handlers.clauses[&dispatch][k];
            let Answer::Known(Some(body)) =
                completes_normally(f, &shape, &budget, clause.body, &exits)
            else {
                continue;
            };
            if is_cli_boundary(f, &shape, &tcx, &types, &body, clause.caught) {
                continue;
            }
            let finding = Finding {
                chain: chains.of(cx, &witness),
                clause,
                witness,
            };
            report(cx, unit, f, &key, &finding, out);
        }
    }
}

/// The call sites of a module's functions, by caller qualname, then by span.
type SitesOfFunctions<'a> = FxHashMap<&'a str, FxHashMap<Span, Vec<&'a CallSite>>>;

fn sites_of_functions<'a>(cx: &CheckContext<'a>, unit: &ModuleUnit<'_>) -> SitesOfFunctions<'a> {
    let mut out = SitesOfFunctions::default();
    let file = cx.index.files.get(&unit.input.path);
    for site in file.into_iter().flat_map(|f| &f.call_sites) {
        out.entry(site.caller.qualname.as_str())
            .or_default()
            .entry(site.span)
            .or_default()
            .push(site);
    }
    out
}

/// The `except` clauses of a function's `try` statements, by dispatch block (the block an
/// exception raised in a `try` body goes to first), and what each dispatch does with a
/// `RecursionError`.
struct Handlers<'f> {
    clauses: FxHashMap<BlockId, Vec<Clause<'f>>>,
    dispatches: FxHashMap<BlockId, Dispatch>,
}

impl<'f> Handlers<'f> {
    fn of(f: &'f IrFunction, source: &str, tcx: &TypeCtx<'_>) -> Self {
        let landing: FxHashSet<BlockId> = f.blocks.iter().filter_map(|b| b.exc_target).collect();
        let mut out = Self {
            clauses: FxHashMap::default(),
            dispatches: FxHashMap::default(),
        };
        for d in landing {
            let (clauses, end) = clause_chain(f, d);
            if clauses.is_empty() {
                continue;
            }
            let mut dispatch = end.map_or(Dispatch::Undecided, Dispatch::PassedOn);
            for (k, clause) in clauses.iter().enumerate() {
                let takes = if is_star_clause(source, clause.span) {
                    Takes::Unknown
                } else {
                    takes_recursion_error(f, tcx, clause)
                };
                match takes {
                    Takes::Yes => {
                        dispatch = Dispatch::TakenBy(k);
                        break;
                    }
                    Takes::Unknown => {
                        dispatch = Dispatch::Undecided;
                        break;
                    }
                    Takes::No => {}
                }
            }
            out.clauses.insert(d, clauses);
            out.dispatches.insert(d, dispatch);
        }
        out
    }

    /// The blocks of each `try` body whose `RecursionError` reaches the dispatch block that
    /// takes it ([`Self::taking_dispatch`]), by dispatch block, in block order.
    fn try_bodies(&self, f: &IrFunction) -> Vec<(BlockId, Vec<BlockId>)> {
        let mut taking: FxHashMap<BlockId, Option<BlockId>> = FxHashMap::default();
        let mut bodies: FxHashMap<BlockId, Vec<BlockId>> = FxHashMap::default();
        for b in &f.blocks {
            let Some(landing) = b.exc_target else {
                continue;
            };
            let dispatch = *taking
                .entry(landing)
                .or_insert_with(|| self.taking_dispatch(f, landing));
            if let Some(d) = dispatch {
                bodies.entry(d).or_default().push(b.id);
            }
        }
        let mut out: Vec<(BlockId, Vec<BlockId>)> = bodies.into_iter().collect();
        out.sort_unstable_by_key(|(d, _)| *d);
        out
    }

    /// The dispatch block where a `RecursionError` raised into `landing` (a block's exception
    /// target) is taken or may be: past the exits of `with` statements (the context managers
    /// are taken to let it through) and past `try` statements none of whose clauses takes it.
    /// `None` when it reaches no such block heki follows (a `finally` body, which raises it
    /// again after running).
    fn taking_dispatch(&self, f: &IrFunction, landing: BlockId) -> Option<BlockId> {
        let mut at = landing;
        // Each step goes out to an enclosing statement, so there are fewer than the blocks.
        for _ in 0..f.blocks.len() {
            let exits_with = || {
                matches!(
                    f.block(at).insts.first().map(|i| &i.kind),
                    Some(InstKind::ExitWith { .. })
                )
            };
            at = match self.dispatches.get(&at) {
                Some(Dispatch::TakenBy(_) | Dispatch::Undecided) => break,
                Some(Dispatch::PassedOn(end)) => f.block(*end).exc_target?,
                None if exits_with() => f.block(at).exc_target?,
                None => return None,
            };
        }
        Some(at)
    }
}

/// The clauses a dispatch block tests in turn, and the block that raises the exception again
/// when none takes it (`None` when the chain ends otherwise: an `except*` group's rest).
fn clause_chain(f: &IrFunction, dispatch: BlockId) -> (Vec<Clause<'_>>, Option<BlockId>) {
    let mut clauses = Vec::new();
    let mut test = dispatch;
    // Each clause's test block comes after the previous one's, so the chain has fewer links
    // than the function has blocks.
    for _ in 0..f.blocks.len() {
        match f.block(test).terminator {
            Terminator::Branch {
                then_bb, else_bb, ..
            } => {
                let Some(clause) = Clause::at(f, test, then_bb) else {
                    break;
                };
                clauses.push(clause);
                test = else_bb;
            }
            Terminator::Raise(None) if !clauses.is_empty() => return (clauses, Some(test)),
            _ => break,
        }
    }
    (clauses, None)
}

/// Whether the clause of span `span` is an `except*` clause, by its source text.
fn is_star_clause(source: &str, span: Span) -> bool {
    source
        .get(span.start as usize..span.end as usize)
        .and_then(|text| text.strip_prefix("except"))
        .is_some_and(|rest| rest.trim_start().starts_with('*'))
}

/// Whether the clause takes a `RecursionError`: a bare `except`, or classes the module's names
/// give (a tuple display of them included).
fn takes_recursion_error(f: &IrFunction, tcx: &TypeCtx<'_>, clause: &Clause<'_>) -> Takes {
    if clause.types.is_empty() {
        return Takes::Yes;
    }
    let Some(root) = type_expression(f, clause.test) else {
        return Takes::Unknown;
    };
    let classes: Vec<VarId> = match type_expression_inst(f, clause.test) {
        Some(InstKind::AllocBuiltin {
            kind: BuiltinKind::Tuple,
            elems,
            ..
        }) => elems.clone(),
        _ => vec![root],
    };
    let mut takes = Takes::No;
    for class in classes {
        match class_named(f, tcx, clause.test, class) {
            Some(fqn) if RECURSION_ERROR_CLASSES.contains(&fqn.as_str()) => return Takes::Yes,
            Some(_) => {}
            None => takes = Takes::Unknown,
        }
    }
    takes
}

/// The fqn of the class the value `v` of a clause's class expression (computed in block
/// `test`) names: a module-level name, or an attribute of one naming a module
/// (`builtins.RuntimeError`); `None` for anything else (a local variable, a call).
fn class_named(f: &IrFunction, tcx: &TypeCtx<'_>, test: BlockId, v: VarId) -> Option<String> {
    let defined = |v: VarId| {
        f.block(test)
            .insts
            .iter()
            .find(|i| i.kind.dst() == Some(v))
            .map(|i| &i.kind)
    };
    let global = |kind: Option<&InstKind>| match kind? {
        InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. } => {
            tcx.named_object(name)
        }
        _ => None,
    };
    match defined(v)? {
        InstKind::AttrLoad { obj, attr, .. } => Some(format!("{}.{attr}", global(defined(*obj))?)),
        kind => global(Some(kind)),
    }
}

/// The instruction computing a clause's classes: the last one before the constant the clause's
/// test branches on (the test itself is not modeled).
fn type_expression_inst(f: &IrFunction, test: BlockId) -> Option<&InstKind> {
    let insts = &f.block(test).insts;
    let (_, before) = insts.split_last()?;
    before.last().map(|i| &i.kind)
}

/// The value of a clause's classes ([`type_expression_inst`]).
fn type_expression(f: &IrFunction, test: BlockId) -> Option<VarId> {
    type_expression_inst(f, test)?.dst()
}

/// The span of a clause's `except` line: from `except` to the end of its classes.
fn header_span(f: &IrFunction, clause: &Clause<'_>) -> Span {
    let start = clause.span.start;
    let insts = &f.block(clause.test).insts;
    let end = insts[..insts.len().saturating_sub(1)]
        .iter()
        .map(|i| i.span.end)
        .filter(|end| *end > start)
        .max()
        .unwrap_or(start + "except".len() as u32);
    Span { start, end }
}

/// A call in the `try` body that runs a call cycle: the site, the callee, and its fact.
#[derive(Clone, Copy)]
struct Witness<'a> {
    site: &'a CallSite,
    callee: &'a FuncKey,
    cycle: CallCycle,
}

/// The call of a `try` body (`body_blocks`, [`Handlers::try_bodies`]) that runs a function on a
/// call cycle on the function's stack: the first, in block order, of a function on the cycle,
/// else the first of one reaching it.
fn cycle_in_try_body<'a>(
    cx: &CheckContext<'a>,
    f: &IrFunction,
    body_blocks: &[BlockId],
    sites: &FxHashMap<Span, Vec<&'a CallSite>>,
) -> Option<Witness<'a>> {
    body_blocks
        .iter()
        .flat_map(|b| &f.block(*b).insts)
        .flat_map(|inst| sites.get(&inst.span).into_iter().flatten())
        .filter_map(|site| cycle_at(cx, site))
        .reduce(|first, w| {
            if first.cycle != CallCycle::On && w.cycle == CallCycle::On {
                w
            } else {
                first
            }
        })
}

/// A project callee of `site` whose facts that run during the call reach a call cycle: one on
/// the cycle first.
fn cycle_at<'a>(cx: &CheckContext<'a>, site: &'a CallSite) -> Option<Witness<'a>> {
    if site.runner == Runner::Worker {
        return None;
    }
    site.project_callees()
        .iter()
        .filter_map(|bc| {
            let callee = cx.index.summary(&bc.callee)?;
            let delivery = cx.index.delivery(site, callee)?;
            if delivery.into != Facts::Body {
                return None;
            }
            let cycle = callee.facts(delivery.from)?.call_cycle;
            (cycle != CallCycle::None).then_some(Witness {
                site,
                callee: &bc.callee,
                cycle,
            })
        })
        .max_by_key(|w| w.cycle)
}

/// The chains from the functions the reported calls run down to a function on a call cycle,
/// for one module's check: the call sites of each file a chain enters, indexed by caller once,
/// and each callee's chain once.
#[derive(Default)]
struct CycleChains<'a> {
    sites: FxHashMap<&'a std::path::Path, FxHashMap<&'a FuncKey, Vec<&'a CallSite>>>,
    chains: FxHashMap<&'a FuncKey, Rc<[Witness<'a>]>>,
}

impl<'a> CycleChains<'a> {
    /// The calls from the function a witness calls down to a function on its call cycle, in
    /// call order (none when the callee is on it): each call runs, on the caller's stack, a
    /// callee reaching the cycle.
    fn of(&mut self, cx: &CheckContext<'a>, witness: &Witness<'a>) -> Rc<[Witness<'a>]> {
        if witness.cycle == CallCycle::On {
            return Rc::from(Vec::new());
        }
        if let Some(chain) = self.chains.get(witness.callee) {
            return Rc::clone(chain);
        }
        let budget = Budget::unmetered("EFF114 cycle chains");
        let mut reached_by: FxHashMap<&'a FuncKey, Witness<'a>> = FxHashMap::default();
        let sites = &mut self.sites;
        let found = reach(
            &budget,
            [witness.callee],
            |k: &'a FuncKey, next| {
                let own = cx.index.file_of(k).into_iter().flat_map(|path| {
                    let by_caller = sites.entry(path).or_insert_with(|| {
                        let mut by_caller: FxHashMap<&'a FuncKey, Vec<&'a CallSite>> =
                            FxHashMap::default();
                        for site in &cx.index.files[path].call_sites {
                            by_caller.entry(&site.caller).or_default().push(site);
                        }
                        by_caller
                    });
                    by_caller.get(k).cloned().unwrap_or_default()
                });
                for w in own.filter_map(|site| cycle_at(cx, site)) {
                    if w.callee != witness.callee && !reached_by.contains_key(w.callee) {
                        next.push(w.callee);
                        reached_by.insert(w.callee, w);
                    }
                }
            },
            |k| {
                let on = cx
                    .index
                    .summary(k)
                    .is_some_and(|s| s.call_cycle == CallCycle::On);
                if on { Visit::Stop(k) } else { Visit::Expand }
            },
        );
        let mut chain = Vec::new();
        if let Answer::Known(Some(member)) = found {
            let mut at = member;
            while let Some(w) = reached_by.remove(at) {
                at = &w.site.caller;
                chain.push(w);
            }
            chain.reverse();
        }
        let chain: Rc<[Witness<'a>]> = Rc::from(chain);
        self.chains.insert(witness.callee, Rc::clone(&chain));
        chain
    }
}

/// Whether the call at `site` never returns normally: each of its project callees, at least one,
/// returns on no path (`MustExec::never_returns`: it always raises, or never ends).
fn never_returns(cx: &CheckContext<'_>, site: &CallSite) -> bool {
    site.role == SiteRole::Call
        && !site.project_callees().is_empty()
        && site.project_callees().iter().all(|bc| {
            cx.index
                .summary(&bc.callee)
                .and_then(|s| s.must_exec.as_deref())
                .is_some_and(|m| m.never_returns)
        })
}

/// Whether the call `kind` ends the process ([`EXITS`]).
fn ends_process(tcx: &TypeCtx<'_>, types: &FxHashMap<VarId, Val>, kind: &InstKind) -> bool {
    match resolve_inst(tcx, types, kind) {
        Callee::External(symbol) => EXITS.contains(&symbol.as_str()),
        _ => false,
    }
}

/// The blocks of the clause body starting in `start`, when every path through it completes
/// normally: the blocks `start` dominates and reaches by normal edges, none of which raises or
/// calls one of [`EXITS`]. A path leaving them (to the code after the `try`, through
/// `break`, `continue`, or the end of the body) or returning completes. `Known(None)` when a
/// path raises; `Open` when the walk spends its budget.
fn completes_normally(
    f: &IrFunction,
    shape: &FnShape<'_>,
    budget: &Budget,
    start: BlockId,
    exits: &dyn Fn(&Inst) -> bool,
) -> Answer<Option<Vec<BlockId>>> {
    let dominators = shape.dominators();
    let mut body = Vec::new();
    let raises = reach(
        budget,
        [start],
        |b, next| next.extend(f.block(b).terminator.successors()),
        |b| {
            if !dominators.dominates(start, b) {
                return Visit::Prune;
            }
            body.push(b);
            let block = f.block(b);
            if block.insts.iter().any(exits) || matches!(block.terminator, Terminator::Raise(_)) {
                Visit::Stop(())
            } else {
                Visit::Expand
            }
        },
    );
    raises.map(|raised| raised.is_none().then_some(body))
}

/// Whether the clause body (`body`, binding the exception to `caught`) is a CLI boundary: it
/// reports the error and returns an integer constant, an exit status, on every `return` (at
/// least one).
fn is_cli_boundary(
    f: &IrFunction,
    shape: &FnShape<'_>,
    tcx: &TypeCtx<'_>,
    types: &FxHashMap<VarId, Val>,
    body: &[BlockId],
    caught: Option<VarId>,
) -> bool {
    let mut returns = body
        .iter()
        .filter_map(|b| match f.block(*b).terminator {
            Terminator::Return(value) => Some(value),
            _ => None,
        })
        .peekable();
    let exit_status = |value: Option<VarId>| {
        value.is_some_and(|v| {
            matches!(
                shape.inst(v).map(|i| &i.kind),
                Some(InstKind::Const {
                    literal: ConstLiteral::Int(_),
                    ..
                })
            )
        })
    };
    if returns.peek().is_none() || !returns.all(exit_status) {
        return false;
    }
    body.iter()
        .flat_map(|b| &f.block(*b).insts)
        .any(|inst| reports_error(shape, tcx, types, &inst.kind, caught))
}

/// Whether the call `kind` reports the error being handled: writes to `sys.stderr` (`print` with
/// `file=sys.stderr`, `sys.stderr.write`), prints the traceback, or logs through `logging` with
/// the exception (`exception()`, `exc_info=`, or the caught exception, or text formatted from
/// it, as an argument).
fn reports_error(
    shape: &FnShape<'_>,
    tcx: &TypeCtx<'_>,
    types: &FxHashMap<VarId, Val>,
    kind: &InstKind,
    caught: Option<VarId>,
) -> bool {
    let is_stderr = |v: VarId| match shape.inst(v).map(|i| &i.kind) {
        Some(InstKind::AttrLoad { obj, attr, .. }) => {
            attr == "stderr" && matches!(types.get(obj), Some(Val::Ref(m)) if m == "sys")
        }
        _ => false,
    };
    let (args, kwargs) = match kind {
        InstKind::MethodCall { recv, method, .. }
            if is_stderr(*recv) && matches!(method.as_str(), "write" | "writelines") =>
        {
            return true;
        }
        InstKind::Call { args, kwargs, .. } | InstKind::MethodCall { args, kwargs, .. } => {
            (args, kwargs)
        }
        _ => return false,
    };
    let level = match (resolve_inst(tcx, types, kind), kind) {
        (Callee::External(symbol), _) if symbol == "builtins.print" => {
            return kwargs
                .iter()
                .any(|(name, v)| name.as_deref() == Some("file") && is_stderr(*v));
        }
        (Callee::External(symbol), _) if TRACEBACK_PRINTS.contains(&symbol.as_str()) => {
            return true;
        }
        (Callee::External(symbol), _) => {
            match symbol
                .strip_prefix("logging.Logger.")
                .or_else(|| symbol.strip_prefix("logging."))
            {
                Some(level) => level.to_owned(),
                None => return false,
            }
        }
        // A logger of a class heki does not know (`logging.getLogger` has no return type in
        // the catalog): a method named as a logging level.
        (Callee::Unresolved, InstKind::MethodCall { method, .. })
            if LOG_LEVELS.contains(&method.as_str()) =>
        {
            method.clone()
        }
        _ => return false,
    };
    // The exception, or a replacement field formatting it (`f"{error}"`, `f"{error!r}"`).
    let field_of_caught = |v: VarId| {
        let mut v = shape.copy_root(v);
        // A field is a `Format`, of a `Convert` with a conversion: at most two steps.
        for _ in 0..2 {
            match shape.inst(v).map(|i| &i.kind) {
                Some(InstKind::Format { value, .. } | InstKind::Convert { value, .. }) => {
                    v = shape.copy_root(*value);
                }
                _ => break,
            }
        }
        caught == Some(v)
    };
    let from_caught = |v: VarId| {
        field_of_caught(v)
            || matches!(
                shape.inst(shape.copy_root(v)).map(|i| &i.kind),
                Some(InstKind::BuildString { parts, .. }) if parts.iter().any(|p| field_of_caught(*p))
            )
    };
    level == "exception"
        || kwargs
            .iter()
            .any(|(name, _)| name.as_deref() == Some("exc_info"))
        || args
            .iter()
            .chain(kwargs.iter().map(|(_, v)| v))
            .any(|v| from_caught(*v))
}

/// A clause to report: the clause, the call in its `try` body running a call cycle, and the
/// calls from that call's callee down to a function on the cycle.
struct Finding<'f, 'a> {
    clause: Clause<'f>,
    witness: Witness<'a>,
    chain: Rc<[Witness<'a>]>,
}

fn report(
    cx: &CheckContext<'_>,
    unit: &ModuleUnit<'_>,
    f: &IrFunction,
    key: &FuncKey,
    finding: &Finding<'_, '_>,
    out: &mut Vec<Diagnostic>,
) {
    let Finding {
        clause,
        witness,
        chain,
    } = finding;
    let (handler, classes) = match clause.types {
        [types] => (format!("`except {types}`"), types.as_str()),
        _ => ("the bare `except`".to_owned(), "bare"),
    };
    let own_cycle = cx
        .index
        .summary(key)
        .is_some_and(|s| s.call_cycle == CallCycle::On);
    let nested_in_own = |callee: &FuncKey| {
        callee.module == key.module
            && (callee.qualname == key.qualname
                || callee
                    .qualname
                    .strip_prefix(key.qualname.as_str())
                    .is_some_and(|rest| rest.starts_with(".<locals>.")))
    };
    let function = key.qualname.as_str();
    let member = chain.last().map_or(witness.callee, |w| w.callee);
    let recursion = if own_cycle && nested_in_own(witness.callee) {
        format!("the recursion of `{function}`")
    } else if witness.cycle == CallCycle::On {
        format!("the recursion of `{}`", member.qualname)
    } else if chain.is_empty() {
        // The cycle is reached through what iterating the callee's result runs.
        format!("a recursion below `{}`", witness.callee.qualname)
    } else {
        format!(
            "the recursion of `{}` below `{}`",
            member.qualname, witness.callee.qualname
        )
    };
    let called = match witness.callee.qualname.rsplit('.').next() {
        Some("<genexpr>") => "a generator expression".to_owned(),
        Some("<lambda>") => "a lambda".to_owned(),
        Some(scope) if scope.starts_with('<') => "a comprehension".to_owned(),
        _ => format!("`{}`", witness.callee.qualname),
    };
    let related = cx.related_at(
        &unit.input.path,
        witness.site.span,
        format!("`{function}` runs {called} in the `try` body"),
    );
    let d = cx
        .emit(
        out,
        "EFF114",
        unit,
        header_span(f, clause),
        function,
        format!("{classes}->{}", witness.callee.qualname),
        format!(
            "{handler} takes the `RecursionError` {recursion} can raise, and no path through it raises, so `{function}` goes on with a value instead of the error"
        ),
        Some(
            "let `RecursionError` through (`except RecursionError: raise` before this clause), or catch only the errors the `try` body is expected to raise".to_owned(),
        ),
        &[],
    )
        .expect("`run` checked that EFF114 runs on the unit");
    d.related.push(related);
    for w in chain.iter() {
        let path = cx.index.file_of(&w.site.caller).unwrap_or(&unit.input.path);
        d.related.push(cx.related_at(
            path,
            w.site.span,
            format!("`{}` calls `{}`", w.site.caller.qualname, w.callee.qualname),
        ));
    }
}
