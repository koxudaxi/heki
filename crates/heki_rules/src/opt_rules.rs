//! Family 3 (`OPT101`–`OPT130`): `heki_soundness` candidates turned into diagnostics
//! (docs/ANALYSIS_AND_KB_SPEC.md section 3.3, docs/fixes.md).
//!
//! Reported: candidates that are not `Disproved`, are profitable, and have proven cost
//! (`OptCandidate::is_reportable`). A diagnostic carries `fix` only when the rewrite is `Safe`;
//! whether `--fix` applies it by default is the rule's `auto_fix_default` (`OPT101` / `OPT102`).
//! A report whose rewrite is not `Safe` has a premise it did not establish, so it is heuristic
//! whatever its rule's certainty (ADR-018: the proven reports are the rule's exact tier), and so
//! is a size-dependent one in a loop whose trip count the code fixes past 16
//! (`OptCandidate::in_exact_tier`).
//!
//! `OPT112` / `OPT113` are Advisory-lane reports without a rewrite: they use the Advisory
//! receiver types (`heki_effect::TypeCtx`, PEP 484 annotations included), which the Soundness
//! lane never trusts.

use crate::mut_rules::{CallInst, site_call};
use crate::{CheckContext, ModuleUnit};
use heki_ast::Span;
use heki_diagnostics::{Diagnostic, TextEdit};
use heki_effect::{CallSite, Items, ProjectDecls, TypeCtx, Val, container_mutator};
use heki_ir::walk::{self, Answer, Budget, Def, FnShape, Use};
use heki_ir::{
    BlockId, BuiltinKind, CompareOpKind, ConstLiteral, FnIndex, FunctionId, Inst, InstKind,
    IrFunction, ItemOrigins, ItemSource, LoopBound, ModuleConstants, NaturalLoop, PhiNode,
    SMALL_MAX, SmallDisplay, Terminator, UnaryOpKind, VarId,
};
use heki_soundness::{
    AnalysisOptions, OptCandidate, ProfitabilityTier, SafetyTier, analyze_module,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::RefCell;

/// Suffix of the help saying what `--fix` does with one candidate.
fn fix_note(c: &OptCandidate) -> String {
    match (c.verdict.tier, c.fix().is_some(), c.profitability) {
        (SafetyTier::Safe, true, ProfitabilityTier::Unconditional) => {
            " [safe fix: `heki check --fix`]".into()
        }
        (SafetyTier::Safe, true, ProfitabilityTier::SizeDependent) => {
            " [proven-safe suggestion: `heki check --fix --include-size-dependent`]".into()
        }
        (SafetyTier::Safe, false, _) => " [proven safe; rewrite it by hand]".into(),
        // Reported candidates that are not safe are `Blocked`, which names its obligation.
        _ => c
            .verdict
            .blocker()
            .map(|b| format!(" [not auto-fixable: {}]", b.describe()))
            .unwrap_or_default(),
    }
}

pub(crate) fn opt(
    cx: &CheckContext<'_>,
    unit: &ModuleUnit<'_>,
    opts: &AnalysisOptions,
    out: &mut Vec<Diagnostic>,
) {
    let any_enabled = heki_soundness::OptRule::ALL
        .iter()
        .any(|r| cx.enabled(r.code(), unit));
    if !any_enabled {
        return;
    }
    for c in analyze_module(unit.input.ir, unit.source, cx.soundness, opts) {
        if !c.is_reportable() {
            continue;
        }
        let help = format!("{}{}", c.help, fix_note(&c));
        let Some(d) = cx.emit(
            out,
            c.rule.code(),
            unit,
            c.span,
            &c.function,
            c.target.clone(),
            c.message.clone(),
            Some(help),
            &[],
        ) else {
            // This `OPT` rule is not selected for the file.
            continue;
        };
        let tier = crate::premises::tier(c.rule.code(), c.in_exact_tier());
        d.certainty = tier.at_most(d.certainty);
        d.evidence = c.cost_evidence.clone().map(|cost| match c.verdict.tier {
            SafetyTier::Safe => format!("{cost}; established: {}", c.verdict.evidence()),
            _ => cost,
        });
        d.blocker = c.verdict.blocker().map(|b| b.describe());
        d.fix = c.fix().map(|edits| {
            edits
                .iter()
                .map(|e| TextEdit {
                    span: e.span,
                    replacement: e.replacement.clone(),
                })
                .collect()
        });
    }
}

// ============================================================================= loop sizes

/// The Advisory lane's view of loop and container sizes: the shared IR facts
/// (`heki_ir::FnIndex`: displays of this function, this module's constants, loop trip counts),
/// extended to the small displays of other modules' globals and of instance attributes, which
/// only the declarations know (`ProjectDecls::small_global` / `small_attr`, read through the
/// recorded declarations so that the language server re-checks a reader when they change).
pub(crate) struct Sizes<'a, 'c> {
    cx: &'c CheckContext<'a>,
    tcx: &'c TypeCtx<'a>,
    /// Receiver types for attribute loads (empty: attributes are not resolved).
    types: &'c FxHashMap<VarId, Val>,
    pub ix: FnIndex<'c>,
    /// Some display the module's own code fixes may grow in another module
    /// (`CheckContext::fixed_constants`): the IR's fixed trip counts are decided again.
    withheld: bool,
}

impl<'a, 'c> Sizes<'a, 'c> {
    /// `constants`: the module's constants the project leaves fixed
    /// (`CheckContext::fixed_constants`).
    pub(crate) fn new(
        cx: &'c CheckContext<'a>,
        tcx: &'c TypeCtx<'a>,
        types: &'c FxHashMap<VarId, Val>,
        shape: FnShape<'c>,
        unit: &'c ModuleUnit<'_>,
        constants: &'c ModuleConstants,
    ) -> Self {
        Self::of(cx, tcx, types, shape, constants, withholds(unit, constants))
    }

    fn of(
        cx: &'c CheckContext<'a>,
        tcx: &'c TypeCtx<'a>,
        types: &'c FxHashMap<VarId, Val>,
        shape: FnShape<'c>,
        constants: &'c ModuleConstants,
        withheld: bool,
    ) -> Self {
        let ix = FnIndex::with_shape(shape, constants);
        Self {
            cx,
            tcx,
            types,
            ix: if withheld { ix.redeciding_trips() } else { ix },
            withheld,
        }
    }

    /// A data-sized loop: its trip count is not fixed by the code (`LoopBound::Unbounded`, the
    /// IR's verdict, or a fixed count from a module display another module may grow), and it
    /// does not iterate a small display of another module or an attribute (or a builtin
    /// wrapper of one: `enumerate(PREFIXES)`), here or where the function creating this one
    /// evaluates the iterable it passes (a generator expression's, asked through `creators`).
    pub(crate) fn data_sized(&self, lp: &NaturalLoop, creators: &Creators<'_, '_>) -> bool {
        let decided_again = match lp.bound {
            LoopBound::Unbounded => true,
            LoopBound::ConstSmall(_) => self.withheld,
            LoopBound::RetryPattern | LoopBound::DaemonPolling => false,
        };
        decided_again
            && self
                .ix
                .fixed_trip(lp, &mut |inst| self.leaf_len(inst), SMALL_MAX)
                .is_none()
            && self.passed_trip(lp, creators).is_none()
    }

    /// How many items a load of a global or an attribute gives, when the declarations fix it
    /// and this function does not grow it.
    fn leaf_len(&self, inst: &Inst) -> Option<u32> {
        let d = self.leaf_display(inst)?;
        self.ix.load_unchanged(inst.kind.dst()?).then_some(d.len)
    }

    /// The trip count of a loop over the iterable this function receives from the one creating
    /// it (`.0` of a generator expression, or of a comprehension before 3.12), as the creator
    /// sees that iterable.
    fn passed_trip(&self, lp: &NaturalLoop, creators: &Creators<'_, '_>) -> Option<u32> {
        let f = self.ix.f;
        let (creator, iterable) = f.passed_from?;
        let it = self.ix.loop_iterable(lp)?;
        (f.var_name(it) == Some(".0")).then_some(())?;
        creators.trip(creator, iterable)
    }

    /// The loop of a builtin calling a callable once per item (`map(f, xs)`, `sorted(xs,
    /// key=f)`) at `call` is data-sized: no iterable it takes the items from yields a number
    /// of items the code fixes, as for a `for` loop's iterable (`max(a, b, key=f)` compares
    /// two values).
    pub(crate) fn items_data_sized(&self, call: &Inst, items: &Items) -> bool {
        CallInst::of(&call.kind).is_some_and(|call| match items {
            Items::Args(positions) => {
                !positions.is_empty()
                    && positions.iter().all(|p| {
                        call.args
                            .get(usize::from(*p))
                            .is_some_and(|it| !self.fixed_items(*it))
                    })
            }
            Items::Receiver => call.recv.is_some_and(|(it, _)| !self.fixed_items(it)),
            Items::Unpacked => true,
        })
    }

    /// Iterating `it` yields a number of items the code fixes (`FnIndex::iter_bound`): also a
    /// small display of another module or an attribute, or a builtin wrapper of one.
    fn fixed_items(&self, it: VarId) -> bool {
        self.ix
            .iter_bound(it, &mut |inst| self.leaf_len(inst))
            .is_some()
    }

    /// `var` is a small display of `kinds` for its whole life: built in this function or bound
    /// to a constant of this module (`FnIndex::container_bound`), or a small global of another
    /// module or attribute display (`Self::leaf_display`) that this function does not grow.
    pub(crate) fn small_container(&self, var: VarId, kinds: &[BuiltinKind]) -> bool {
        if self.ix.container_bound(var).is_some() {
            return true;
        }
        let v = self.ix.shape.copy_root(var);
        self.ix
            .shape
            .inst(v)
            .and_then(|inst| self.leaf_display(inst))
            .is_some_and(|d| d.is_of(kinds) && self.ix.load_unchanged(v))
    }

    /// What an imported name loaded into `var` refers to (`consts` of `from pkg import
    /// consts`), without type inference.
    fn imported(&self, var: VarId) -> Option<Val> {
        let inst = self.ix.shape.inst(var)?;
        Some(Val::Ref(self.cx.decls.canonical(self.import_of(inst)?)))
    }

    /// The fqn an imported name `inst` loads names, as this module's imports bind it: a load
    /// of a name nothing else in the module binds ([`ModuleDecls::fixed_import`]), or in a
    /// module body a copy of its `Import`.
    fn import_of(&self, inst: &Inst) -> Option<&str> {
        match &inst.kind {
            InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. } => {
                self.tcx.module.fixed_import(name).map(String::as_str)
            }
            InstKind::Import { .. } => self.tcx.import_target(inst.span),
            _ => None,
        }
    }

    /// The small display a load of a global or an attribute gives, by the declarations: a
    /// module global (`NAME`, an imported `NAME`, `module.NAME`) or an instance attribute its
    /// class family only ever stores such displays into (`self.preferred = [",", ";"]`, a
    /// class attribute).
    fn leaf_display(&self, inst: &Inst) -> Option<SmallDisplay> {
        let decls = self.cx.decls;
        match &inst.kind {
            InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. } => {
                let fqn = match self.tcx.module.imports.get(name) {
                    Some(_) => self.import_of(inst)?.to_owned(),
                    None => format!("{}.{name}", self.tcx.module_name()),
                };
                decls.small_global(&fqn).cloned()
            }
            InstKind::Import { .. } => decls.small_global(self.import_of(inst)?).cloned(),
            InstKind::AttrLoad { obj, attr, .. } => {
                match self.types.get(obj).cloned().or_else(|| self.imported(*obj)) {
                    // A class attribute read through the class (`cls.order`).
                    Some(Val::Ref(class) | Val::Class(class)) if decls.is_project_class(&class) => {
                        decls.small_attr(&class, attr)
                    }
                    Some(Val::Ref(module)) => {
                        decls.small_global(&format!("{module}.{attr}")).cloned()
                    }
                    Some(Val::Inst(class)) => decls.small_attr(&class, attr),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

/// `constants` (`CheckContext::fixed_constants`) lack a display of the module's own constants,
/// which another module may grow.
fn withholds(unit: &ModuleUnit<'_>, constants: &ModuleConstants) -> bool {
    constants.displays.len() != unit.input.ir.constants.displays.len()
}

/// The functions of a module that create child scopes, as one rule run over the module sees
/// the iterables they pass ([`Sizes::passed_trip`]): a view of each creator, built once, and
/// the trip count of a loop over each iterable, however many child scopes receive it.
pub(crate) struct Creators<'a, 'c> {
    cx: &'c CheckContext<'a>,
    tcx: &'c TypeCtx<'a>,
    /// The creators' attribute loads are not resolved.
    untyped: &'c FxHashMap<VarId, Val>,
    constants: &'c ModuleConstants,
    withheld: bool,
    views: RefCell<FxHashMap<FunctionId, Sizes<'a, 'c>>>,
    trips: RefCell<FxHashMap<(FunctionId, VarId), Option<u32>>>,
}

impl<'a, 'c> Creators<'a, 'c> {
    /// `constants`: the module's constants the project leaves fixed
    /// (`CheckContext::fixed_constants`); `untyped`: an empty map.
    pub(crate) fn new(
        cx: &'c CheckContext<'a>,
        tcx: &'c TypeCtx<'a>,
        untyped: &'c FxHashMap<VarId, Val>,
        unit: &ModuleUnit<'_>,
        constants: &'c ModuleConstants,
    ) -> Self {
        Self {
            cx,
            tcx,
            untyped,
            constants,
            withheld: withholds(unit, constants),
            views: RefCell::default(),
            trips: RefCell::default(),
        }
    }

    /// The trip count of a loop over `iterable`, a value of the function `creator`, as a view of
    /// `creator` that has answered nothing else gives it (`FnIndex::iter_bound_alone`).
    fn trip(&self, creator: FunctionId, iterable: VarId) -> Option<u32> {
        if let Some(known) = self.trips.borrow().get(&(creator, iterable)) {
            return *known;
        }
        let mut views = self.views.borrow_mut();
        let outer: &Sizes<'a, 'c> = views.entry(creator).or_insert_with(|| {
            Sizes::of(
                self.cx,
                self.tcx,
                self.untyped,
                FnShape::new(self.tcx.ir().function(creator)),
                self.constants,
                self.withheld,
            )
        });
        let trip = outer
            .ix
            .iter_bound_alone(iterable, &mut |inst| outer.leaf_len(inst));
        self.trips.borrow_mut().insert((creator, iterable), trip);
        trip
    }
}

// ==================================================================================== OPT112

/// `list` methods that scan the whole list.
const LINEAR_LIST_METHODS: &[&str] = &["index", "remove", "count"];

/// The small displays a `list` receiver may be.
const LIST: &[BuiltinKind] = &[BuiltinKind::List];

/// `OPT112`: `list.index()` / `list.remove()` / `list.count()` run again on every iteration of
/// a data-sized loop, on a list that outlives the iteration (Advisory lane).
pub(crate) fn opt112(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.enabled("OPT112", unit) {
        return;
    }
    let tcx = TypeCtx::new(&unit.input, cx.decls, cx.kb);
    let no_types = FxHashMap::default();
    let constants = cx.fixed_constants(unit);
    let creators = Creators::new(cx, &tcx, &no_types, unit, &constants);
    for f in &unit.input.ir.functions {
        let shape = FnShape::new(f);
        // `index(x, start)` scans from a position, usually one that advances (a linear walk).
        let calls: Vec<(BlockId, Span, VarId, &str)> = f
            .blocks
            .iter()
            .filter(|b| shape.loop_depth(b.id) > 0)
            .flat_map(|b| {
                b.insts.iter().filter_map(move |i| match &i.kind {
                    InstKind::MethodCall {
                        recv, method, args, ..
                    } if LINEAR_LIST_METHODS.contains(&method.as_str()) && args.len() == 1 => {
                        Some((b.id, i.span, *recv, method.as_str()))
                    }
                    _ => None,
                })
            })
            .collect();
        if f.is_class_body || calls.is_empty() {
            continue;
        }
        let types = tcx.infer(f);
        let sizes = Sizes::new(cx, &tcx, &types, shape, unit, &constants);
        let defs = Defs::new(&sizes.ix.shape);
        for (bb, span, recv, method) in calls {
            // Only a receiver that is certainly a builtin `list` (`set.remove`, `str.index`, a
            // project class's `remove`, or an unknown type are not linear scans of a list).
            let is_list = matches!(types.get(&recv), Some(Val::Inst(t)) if t == "builtins.list");
            // A data-sized loop (not a small constant range or display, a retry, or daemon
            // polling) runs the call again on the same list.
            let hot: Vec<&NaturalLoop> = f
                .loops
                .iter()
                .filter(|lp| {
                    defs.shape.in_body(lp, bb)
                        && runs_again(defs.shape, lp, bb)
                        && sizes.ix.lives_across(lp, recv)
                        && sizes.data_sized(lp, &creators)
                })
                .collect();
            if !is_list || hot.is_empty() || sizes.small_container(recv, LIST) {
                continue;
            }
            let call = slice(unit.source, span);
            let target = defs.receiver_name(recv).unwrap_or_else(|| {
                call.split_once(&format!(".{method}("))
                    .map_or(call, |(head, _)| head)
                    .to_owned()
            });
            // The advice holds for what the loop does with the list: removals can wait for
            // the end of the loop only when it reads the list nowhere else, and an index or a
            // count built before the loop stays right only while the loop does not change it.
            let use_of = ListUse::new(&defs, &hot, recv);
            let changed = use_of.changed() || use_of.handed_to_mutator(cx, unit, f);
            let advice = match method {
                "index" if changed => {
                    "the loop changes the list, so keep a `dict` (value → index) up to date with each change".into()
                }
                "index" => "build a `dict` (value → index) or a `set` once before the loop".into(),
                "remove" if use_of.read_besides_removals() => {
                    "the loop reads the list after each removal, so keep its values in a `set` or a `dict` (`dict.fromkeys(...)` keeps their order) and delete them from it".into()
                }
                "remove" => {
                    "collect the values to drop in a `set` and rebuild the list once with a comprehension".into()
                }
                _ if changed => format!(
                    "the loop changes the list, so keep a `collections.Counter({target})` up to date with each change"
                ),
                _ => format!("count once before the loop with `collections.Counter({target})`"),
            };
            cx.emit(
                out,
                "OPT112",
                unit,
                span,
                heki_effect::func_qualname(f),
                target.clone(),
                format!("`{target}.{method}()` scans the list each time it runs in the loop"),
                Some(advice),
                &[],
            );
        }
    }
}

/// What the loops running an `OPT112` call again do with its list (the receiver `list`).
struct ListUse<'d, 'a> {
    defs: &'d Defs<'a>,
    /// The blocks of those loops.
    blocks: FxHashSet<BlockId>,
    /// Their iterables.
    iterables: Vec<VarId>,
    /// Copies and phis of the value the receiver copies.
    alias: FxHashSet<VarId>,
    /// The receiver's source form (`items`, `self.items`): loads of the same name or path.
    name: Option<String>,
}

impl<'d, 'a> ListUse<'d, 'a> {
    fn new(defs: &'d Defs<'a>, hot: &[&NaturalLoop], list: VarId) -> Self {
        let root = defs.shape.copy_root(list);
        Self {
            defs,
            blocks: hot.iter().flat_map(|lp| lp.body.iter().copied()).collect(),
            iterables: hot
                .iter()
                .filter_map(|lp| Some(defs.iterables.get(&defs.next_of(lp)?.1)?.0))
                .collect(),
            alias: forward_aliases(defs.f, &[root]),
            name: defs.receiver_name(list),
        }
    }

    /// Whether `var` holds the list.
    fn is_list(&self, var: VarId) -> bool {
        self.alias.contains(&var)
            || self.name.is_some() && self.defs.receiver_name(var) == self.name
    }

    fn insts(&self) -> impl Iterator<Item = &'a Inst> + '_ {
        self.blocks
            .iter()
            .flat_map(|b| &self.defs.f.block(*b).insts)
    }

    /// Whether the loops change the list in place: a mutating method, an item assignment or
    /// deletion, an augmented assignment.
    fn changed(&self) -> bool {
        self.insts().any(|i| match &i.kind {
            InstKind::MethodCall { recv, method, .. } => {
                self.is_list(*recv) && container_mutator(method)
            }
            InstKind::SubscriptStore { obj, .. } | InstKind::SubscriptDelete { obj, .. } => {
                self.is_list(*obj)
            }
            InstKind::BinOp {
                lhs,
                in_place: true,
                ..
            } => self.is_list(*lhs),
            _ => false,
        })
    }

    /// Whether the loops hand the list to a project function that changes that parameter in
    /// place (its summary's `mutates_params`), itself or through further calls: the callees
    /// the call site of `f` records ([`site_call`]).
    fn handed_to_mutator(
        &self,
        cx: &CheckContext<'_>,
        unit: &ModuleUnit<'_>,
        f: &IrFunction,
    ) -> bool {
        let key = CheckContext::key(unit, f);
        let sites: Vec<&CallSite> = cx
            .index
            .files
            .get(&unit.input.path)
            .into_iter()
            .flat_map(|file| &file.call_sites)
            .filter(|s| s.caller == key)
            .collect();
        let def = |v| self.defs.insts.get(&v).map(|(_, i)| *i);
        self.insts().any(|i| {
            let call = site_call(&def, &sites, i, false);
            call.iter()
                .flat_map(|c| c.bound_arguments(cx))
                .any(|(_, summary, pos, v)| {
                    pos < 64 && summary.mutates_params & (1u64 << pos) != 0 && self.is_list(v)
                })
        })
    }

    /// Whether the loops read the list other than by removing values from it: they iterate it,
    /// test it, pass it on, or read its items. A membership test of a value they remove
    /// (`if x in items: items.remove(x)`) only guards the removal.
    fn read_besides_removals(&self) -> bool {
        let removal = |kind: &InstKind| match kind {
            InstKind::MethodCall {
                recv, method, args, ..
            } if method == "remove" && self.is_list(*recv) => args.first().copied(),
            _ => None,
        };
        let removed: FxHashSet<VarId> = self.insts().filter_map(|i| removal(&i.kind)).collect();
        if self.iterables.iter().any(|v| self.is_list(*v)) {
            return true;
        }
        let reads = self.insts().any(|i| {
            let allowed = match &i.kind {
                InstKind::Copy { .. } => true,
                InstKind::Compare {
                    op: CompareOpKind::In | CompareOpKind::NotIn,
                    lhs,
                    ..
                } => removed.contains(lhs),
                kind => removal(kind).is_some(),
            };
            let operands = i.kind.operands();
            match &i.kind {
                // The receiver of a removal; its value must not be the list.
                _ if allowed && removal(&i.kind).is_some() => {
                    operands[1..].iter().any(|v| self.is_list(*v))
                }
                _ if allowed => false,
                _ => operands.iter().any(|v| self.is_list(*v)),
            }
        });
        reads
            || self.blocks.iter().any(|b| {
                matches!(self.defs.f.block(*b).terminator,
                    Terminator::Branch { cond: v, .. }
                    | Terminator::Return(Some(v))
                    | Terminator::Raise(Some(v)) if self.is_list(v))
            })
    }
}

/// Whether block `bb` of loop `lp` can run again in the same run of the loop: the loop header
/// is reachable from its normal successors without leaving the loop. A block that returns,
/// raises, or `break`s on every path runs once.
fn runs_again(shape: &FnShape<'_>, lp: &NaturalLoop, bb: BlockId) -> bool {
    let f = shape.f;
    let mut seen = FxHashSet::default();
    let mut stack = f.block(bb).terminator.successors();
    while let Some(b) = stack.pop() {
        if b == lp.header {
            return true;
        }
        if shape.in_body(lp, b) && seen.insert(b) {
            stack.extend(f.block(b).terminator.successors());
        }
    }
    false
}

/// `roots` plus every variable that may hold the same value: `Copy` results and phis with
/// such an input.
fn forward_aliases(f: &IrFunction, roots: &[VarId]) -> FxHashSet<VarId> {
    let mut alias: FxHashSet<VarId> = roots.iter().copied().collect();
    loop {
        let before = alias.len();
        for bb in &f.blocks {
            for phi in &bb.phis {
                if phi.incoming.iter().any(|(_, v)| alias.contains(v)) {
                    alias.insert(phi.dst);
                }
            }
            for inst in &bb.insts {
                if let InstKind::Copy { dst, src } = &inst.kind
                    && alias.contains(src)
                {
                    alias.insert(*dst);
                }
            }
        }
        if alias.len() == before {
            return alias;
        }
    }
}

/// Source text of `span`.
fn slice(source: &str, span: Span) -> &str {
    source
        .get(span.start as usize..span.end as usize)
        .unwrap_or_default()
}

/// `text` on one line, runs of whitespace collapsed.
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Defining instruction or phi of every variable of one function, with its block.
struct Defs<'a> {
    f: &'a IrFunction,
    shape: &'a FnShape<'a>,
    insts: FxHashMap<VarId, (BlockId, &'a Inst)>,
    phis: FxHashMap<VarId, (BlockId, &'a PhiNode)>,
    /// `MakeIter` results: the iterable and its span.
    iterables: FxHashMap<VarId, (VarId, Span)>,
}

impl<'a> Defs<'a> {
    fn new(shape: &'a FnShape<'a>) -> Self {
        let f = shape.f;
        let mut insts = FxHashMap::default();
        let mut phis = FxHashMap::default();
        let mut iterables = FxHashMap::default();
        for bb in &f.blocks {
            for phi in &bb.phis {
                phis.insert(phi.dst, (bb.id, phi));
            }
            for inst in &bb.insts {
                if let InstKind::MakeIter { dst, iterable, .. } = &inst.kind {
                    iterables.insert(*dst, (*iterable, inst.span));
                }
                if let Some(d) = inst.kind.dst() {
                    insts.insert(d, (bb.id, inst));
                }
            }
        }
        Self {
            f,
            shape,
            insts,
            phis,
            iterables,
        }
    }

    fn kind(&self, var: VarId) -> Option<&'a InstKind> {
        self.insts.get(&var).map(|(_, i)| &i.kind)
    }

    /// Source form of a receiver: `items`, `self.items`, `config.names`.
    fn receiver_name(&self, var: VarId) -> Option<String> {
        match self.kind(var) {
            Some(InstKind::AttrLoad { obj, attr, .. }) => {
                Some(format!("{}.{attr}", self.receiver_name(*obj)?))
            }
            _ => self
                .f
                .var_name(var)
                .filter(|n| !n.starts_with('.'))
                .map(str::to_owned),
        }
    }

    // ------------------------------------------------------------------------------ OPT113

    /// The `for` loop `lp`, `f.loops[index]` (its header advances an iterator); `None` for a
    /// `while` loop.
    fn for_loop(&self, index: usize, lp: &'a NaturalLoop) -> Option<ForLoop<'a>> {
        let (elem, iter) = self.next_of(lp)?;
        let &(iterable, iter_span) = self.iterables.get(&iter)?;
        Some(ForLoop {
            lp,
            index,
            iter_span,
            iterable,
            element: self.derived(elem),
        })
    }

    /// The `IterNext` of the `for` loop `lp`: its element and iterator.
    fn next_of(&self, lp: &NaturalLoop) -> Option<(VarId, VarId)> {
        self.f
            .block(lp.header)
            .insts
            .iter()
            .find_map(|i| match &i.kind {
                InstKind::IterNext { dst, iter, .. } => Some((*dst, *iter)),
                _ => None,
            })
    }

    /// `elem` and the values derived from it: copies, unpacked components and constant-key
    /// subscripts (`a[0]`, `a["k"]`), attribute loads (`a.k`, `a.x.y`).
    fn derived(&self, elem: VarId) -> FxHashSet<VarId> {
        let mut set = FxHashSet::default();
        set.insert(elem);
        loop {
            let before = set.len();
            for inst in self.f.blocks.iter().flat_map(|b| &b.insts) {
                let derived = match &inst.kind {
                    InstKind::Copy { dst, src: obj } | InstKind::AttrLoad { dst, obj, .. } => {
                        set.contains(obj).then_some(*dst)
                    }
                    InstKind::SubscriptLoad {
                        dst, obj, index, ..
                    } => (set.contains(obj)
                        && matches!(
                            self.kind(*index),
                            Some(InstKind::Const {
                                literal: ConstLiteral::Int(_) | ConstLiteral::Str(_),
                                ..
                            })
                        ))
                    .then_some(*dst),
                    _ => None,
                };
                set.extend(derived);
            }
            if set.len() == before {
                return set;
            }
        }
    }

    /// Whether `var` (the iterable of the loop headed by `inner`) is computed, inside `lp`,
    /// from the element of a loop around `inner` (`lp` or one nested in it): `for b in
    /// a.children`, `for b in index[a.k]`, `for b in f(a)`, `for b in xs[i + 1:]` traverse the
    /// outer element instead of joining with an independent collection.
    fn reads_elements(&self, lp: &NaturalLoop, inner: BlockId, var: VarId) -> bool {
        // A slice or a comprehension is a value without operands: the instructions inside its
        // span count as its operands.
        let values: Vec<(Span, VarId)> = lp
            .body
            .iter()
            .flat_map(|b| &self.f.block(*b).insts)
            .filter_map(|i| Some((i.span, i.kind.dst()?)))
            .collect();
        let mut seen = FxHashSet::default();
        let mut stack = vec![var];
        while let Some(v) = stack.pop() {
            if !seen.insert(v) {
                continue;
            }
            if let Some((bb, phi)) = self.phis.get(&v)
                && self.shape.in_body(lp, *bb)
            {
                stack.extend(phi.incoming.iter().map(|(_, x)| *x));
            } else if let Some((bb, inst)) = self.insts.get(&v)
                && self.shape.in_body(lp, *bb)
            {
                let element = matches!(inst.kind, InstKind::IterNext { .. })
                    && self
                        .f
                        .loops
                        .iter()
                        .any(|l| l.header == *bb && self.shape.in_body(l, inner));
                if element {
                    return true;
                }
                stack.extend(inst.kind.operands());
                stack.extend(
                    values
                        .iter()
                        .filter(|(s, _)| inst.span.start <= s.start && s.end <= inst.span.end)
                        .map(|(_, d)| *d),
                );
            }
        }
        false
    }

    /// Whether the comparison `cmp` selects the pairs a join keeps, in the inner loop `inner`:
    /// through `Copy` and `and` it decides a branch; it is never negated, never one alternative
    /// of an `or`, never picks a value for every pair (`x if a == b else y`, an `else` assigning
    /// what the `then` assigns), its true side never just continues the loop (`if a == b:
    /// continue` skips the self-pairs of an all-pairs loop), its false side does nothing of its
    /// own ([`Self::idle_on_mismatch`]), and it is no `elif` after an `if` handling other pairs
    /// ([`Self::after_other_work`]).
    fn selects_pairs(&self, cmp: VarId, inner: &NaturalLoop) -> bool {
        let alias = forward_aliases(self.f, &[cmp]);
        let negated = self.f.blocks.iter().flat_map(|b| &b.insts).any(|i| {
            matches!(&i.kind, InstKind::UnaryOp { op: UnaryOpKind::Not, operand, .. } if alias.contains(operand))
        });
        // An `or` merge: an operand branches into it when true.
        let alternative = self.f.blocks.iter().any(|m| {
            m.phis.iter().any(|p| {
                p.incoming.iter().any(|(_, v)| alias.contains(v))
                    && p.incoming.iter().any(|(from, v)| {
                        matches!(self.f.block(*from).terminator, Terminator::Branch { cond, then_bb, .. } if cond == *v && then_bb == m.id)
                    })
            })
        });
        let branches: Vec<(BlockId, BlockId, BlockId)> = self
            .f
            .blocks
            .iter()
            .filter_map(|b| match b.terminator {
                Terminator::Branch {
                    cond,
                    then_bb,
                    else_bb,
                } if alias.contains(&cond) => Some((b.id, then_bb, else_bb)),
                _ => None,
            })
            .collect();
        // Both sides compute a value: they jump to one merge (not the loop header) whose phi
        // takes a value from the false side.
        let picks_value = branches.iter().any(|&(_, t, e)| {
            matches!((&self.f.block(t).terminator, &self.f.block(e).terminator),
                (Terminator::Jump(mt), Terminator::Jump(m)) if mt == m
                    && self.f.loops.iter().all(|lp| lp.header != *m)
                    && self.f.block(*m).phis.iter().any(|p| p.incoming.iter().any(|(from, _)| *from == e)))
        });
        !negated
            && !alternative
            && !picks_value
            && !branches.is_empty()
            && branches.iter().all(|&(from, then_bb, else_bb)| {
                !self.continues(from, then_bb)
                    && self.idle_on_mismatch(inner, then_bb, else_bb)
                    && !self.after_other_work(from)
            })
    }

    /// Whether a match of the comparison `cmp` leaves the inner loop `inner` (`break`,
    /// `return`, `raise`): only the first matching element of the inner collection is used.
    fn stops_at_first_match(&self, cmp: VarId, inner: &NaturalLoop) -> bool {
        let alias = forward_aliases(self.f, &[cmp]);
        let matches: Vec<BlockId> = self
            .f
            .blocks
            .iter()
            .filter_map(|b| match b.terminator {
                Terminator::Branch { cond, then_bb, .. } if alias.contains(&cond) => Some(then_bb),
                _ => None,
            })
            .collect();
        !matches.is_empty()
            && matches.iter().all(|&then_bb| {
                then_bb != inner.header
                    && !(self.shape.in_body(inner, then_bb)
                        && runs_again(self.shape, inner, then_bb))
            })
    }

    /// Whether block `from` runs only on the false side of another branch whose true side does
    /// work: an `elif` after an `if` that handles other pairs (`if a.x == b.k: ... elif a.y ==
    /// b.k: ...` matches on two keys, so one index skips pairs the other test handles).
    fn after_other_work(&self, from: BlockId) -> bool {
        let [p] = self.shape.preds(from) else {
            return false;
        };
        matches!(self.f.block(*p).terminator, Terminator::Branch { then_bb, else_bb, .. }
            if else_bb == from && then_bb != from && !self.continues(*p, then_bb))
    }

    /// Whether a pair whose keys differ does nothing of its own: the false edge `else_bb` of a
    /// branch on the comparison reaches, through blocks without instructions, either the next
    /// pair of the inner loop `inner` or a block of the loop the true edge `then_bb` reaches too
    /// (what every pair runs after the `if`; the merge of an `and` is one). An `else` with work,
    /// an `assert` raising on a mismatch, and statements after a `break` the match takes all
    /// need every pair: an index of the inner collection would skip them.
    fn idle_on_mismatch(&self, inner: &NaturalLoop, then_bb: BlockId, else_bb: BlockId) -> bool {
        // The blocks of the inner loop a matching pair runs, without going round the loop.
        let mut matched = FxHashSet::default();
        let mut stack = vec![then_bb];
        while let Some(b) = stack.pop() {
            if b != inner.header && self.shape.in_body(inner, b) && matched.insert(b) {
                stack.extend(self.f.block(b).terminator.successors());
            }
        }
        let mut seen = FxHashSet::default();
        let mut b = else_bb;
        loop {
            if b == inner.header || matched.contains(&b) {
                return true;
            }
            let block = self.f.block(b);
            if !self.shape.in_body(inner, b) || !seen.insert(b) || !block.insts.is_empty() {
                return false;
            }
            let Terminator::Jump(next) = block.terminator else {
                return false;
            };
            b = next;
        }
    }

    /// Whether block `to`, entered from block `from`, only continues a loop containing `from`.
    fn continues(&self, from: BlockId, to: BlockId) -> bool {
        let b = self.f.block(to);
        b.insts.is_empty()
            && b.phis.is_empty()
            && matches!(b.terminator, Terminator::Jump(h)
                if self.f.loops.iter().any(|lp| lp.header == h && self.shape.in_body(lp, from)))
    }

    /// The mapping the inner loop steps through by its keys, when the inner key is that key:
    /// `for k, v in d.items()` with the key `k`, or `for k in d.keys()` (`d`, from the
    /// iterable's text `inner_iter`).
    fn keyed_mapping<'t>(
        &self,
        inner: &ForLoop<'_>,
        key: VarId,
        inner_iter: &'t str,
    ) -> Option<&'t str> {
        let (element, _) = self.next_of(inner.lp)?;
        let (method, mapping) = [".items()", ".keys()"]
            .into_iter()
            .find_map(|m| Some((m, inner_iter.strip_suffix(m)?)))?;
        let mut key = key;
        while let Some(InstKind::Copy { src, .. }) = self.kind(key) {
            key = *src;
        }
        let first = |index: VarId| {
            matches!(
                self.kind(index),
                Some(InstKind::Const {
                    literal: ConstLiteral::Int(0),
                    ..
                })
            )
        };
        let is_key = match (method, self.kind(key)) {
            (".keys()", _) => self.whole_element(key),
            (_, Some(InstKind::SubscriptLoad { obj, index, .. })) => {
                *obj == element && first(*index)
            }
            _ => false,
        };
        is_key.then_some(mapping)
    }

    /// Whether `var` is a loop element itself (through `Copy`), not a part of it.
    fn whole_element(&self, var: VarId) -> bool {
        matches!(
            self.kind(self.shape.copy_root(var)),
            Some(InstKind::IterNext { .. })
        )
    }

    /// Whether the key `var` is known to be hashable: its inferred type, or every class of an
    /// inferred union, is a builtin whose instances hash by value (`str`, `int`, `float`,
    /// `bool`, `bytes`, `complex`, `frozenset`, `None`). A `tuple` hashes only when its items
    /// do, and a project class may set `__hash__ = None`.
    fn hashable(&self, var: VarId, types: &FxHashMap<VarId, Val>) -> bool {
        const HASHABLE: &[&str] = &[
            "builtins.str",
            "builtins.int",
            "builtins.float",
            "builtins.bool",
            "builtins.bytes",
            "builtins.complex",
            "builtins.frozenset",
            "builtins.NoneType",
        ];
        known_instance(types, var, HASHABLE)
    }

    /// The unhashable builtin type (`list`, `dict`, `set`, `bytearray`) the key `var` is known
    /// to have: its inferred type, or every class of an inferred union (the first one names
    /// it). A loop element over displays has the type of their elements.
    fn unhashable(&self, var: VarId, types: &FxHashMap<VarId, Val>) -> Option<&'static str> {
        const UNHASHABLE: &[&str] = &["list", "dict", "set", "bytearray"];
        let unhashable = |class: &str| {
            let name = class.strip_prefix("builtins.")?;
            UNHASHABLE.iter().find(|u| **u == name).copied()
        };
        match types.get(&var)? {
            Val::Inst(class) => unhashable(class),
            Val::Union(classes) => classes
                .iter()
                .map(|c| unhashable(c))
                .collect::<Option<Vec<_>>>()?
                .first()
                .copied(),
            Val::Ref(_) | Val::Class(_) | Val::Super(_) => None,
        }
    }

    /// Source form of a loop-element value: `order.customer_id`, `row["id"]`, `pair[0]`, `k`.
    fn element_text(&self, source: &str, var: VarId) -> String {
        let named = self.f.var_name(var).filter(|n| !n.starts_with('.'));
        let (_, inst) = self.insts[&var];
        match (named, &inst.kind) {
            (Some(n), _) => n.to_owned(),
            (None, InstKind::AttrLoad { obj, attr, .. }) => {
                format!("{}.{attr}", self.element_text(source, *obj))
            }
            // A constant subscript, not an unpacked component (whose index spans the target).
            (None, InstKind::SubscriptLoad { obj, index, .. })
                if self.insts[index].1.span != inst.span =>
            {
                format!(
                    "{}[{}]",
                    self.element_text(source, *obj),
                    slice(source, self.insts[index].1.span)
                )
            }
            _ => squash(slice(source, inst.span)),
        }
    }
}

// ==================================================================================== OPT113

/// One data-sized `for` loop of a function.
struct ForLoop<'a> {
    lp: &'a NaturalLoop,
    /// Its index in the function's loops.
    index: usize,
    /// The iterable expression.
    iter_span: Span,
    iterable: VarId,
    /// The loop element and the values derived from it.
    element: FxHashSet<VarId>,
}

/// The comparison reported for one inner loop.
struct Join<'l, 'a> {
    span: Span,
    outer: &'l ForLoop<'a>,
    lhs: VarId,
    rhs: VarId,
    /// The side derived from the inner loop's element.
    inner_side: VarId,
    /// The comparison's result.
    cmp: VarId,
}

/// `OPT113`: a nested-loop equi-join — `for a in A: for b in B: if a.k == b.k:` costs
/// O(n·m); indexing `B` by the key once costs O(n + m) (Advisory lane). One report per inner
/// loop, at its first matching comparison.
pub(crate) fn opt113(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.enabled("OPT113", unit) {
        return;
    }
    let tcx = TypeCtx::new(&unit.input, cx.decls, cx.kb);
    let no_types = FxHashMap::default();
    let constants = cx.fixed_constants(unit);
    let creators = Creators::new(cx, &tcx, &no_types, unit, &constants);
    // Asked only for a reported join.
    let origins = ItemOrigins::new(unit.input.ir);
    for f in &unit.input.ir.functions {
        if f.is_class_body || f.loops.len() < 2 {
            continue;
        }
        let sizes = Sizes::new(cx, &tcx, &no_types, FnShape::new(f), unit, &constants);
        let defs = Defs::new(&sizes.ix.shape);
        // Inferred only for a function with a report, and the premises' walks spend one budget.
        let mut types: Option<FxHashMap<VarId, Val>> = None;
        let mut budget: Option<Budget> = None;
        // Both loops are data-sized `for` loops (inlined comprehensions included).
        let fors: Vec<ForLoop<'_>> = f
            .loops
            .iter()
            .enumerate()
            .filter(|(_, lp)| sizes.data_sized(lp, &creators))
            .filter_map(|(i, lp)| defs.for_loop(i, lp))
            .collect();
        for inner in &fors {
            let mut hit: Option<Join<'_, '_>> = None;
            let outers = fors.iter().filter(|o| {
                o.lp.header != inner.lp.header
                    && defs.shape.in_body(o.lp, inner.lp.header)
                    && !defs.reads_elements(o.lp, inner.lp.header, inner.iterable)
            });
            for outer in outers {
                for inst in inner.lp.body.iter().flat_map(|b| &f.block(*b).insts) {
                    let InstKind::Compare {
                        dst,
                        op: CompareOpKind::Eq,
                        lhs,
                        rhs,
                    } = &inst.kind
                    else {
                        continue;
                    };
                    let inner_side = if outer.element.contains(lhs) && inner.element.contains(rhs) {
                        *rhs
                    } else if inner.element.contains(lhs) && outer.element.contains(rhs) {
                        *lhs
                    } else {
                        continue;
                    };
                    if hit.as_ref().is_none_or(|h| inst.span.start < h.span.start)
                        && defs.selects_pairs(*dst, inner.lp)
                    {
                        hit = Some(Join {
                            span: inst.span,
                            outer,
                            lhs: *lhs,
                            rhs: *rhs,
                            inner_side,
                            cmp: *dst,
                        });
                    }
                }
            }
            if let Some(j) = hit {
                let lhs = defs.element_text(unit.source, j.lhs);
                let rhs = defs.element_text(unit.source, j.rhs);
                let outer_iter = squash(slice(unit.source, j.outer.iter_span));
                let inner_iter = squash(slice(unit.source, inner.iter_span));
                let types = types.get_or_insert_with(|| tcx.infer(f));
                // A `set` or `dict` cannot hold an unhashable key.
                let unhashable = [
                    j.inner_side,
                    if j.inner_side == j.lhs { j.rhs } else { j.lhs },
                ]
                .into_iter()
                .find_map(|k| Some((k, defs.unhashable(k, types)?)));
                let premises = JoinPremises {
                    defs: &defs,
                    sizes: &sizes,
                    types,
                    decls: cx.decls,
                    origins: &origins,
                    budget: budget.get_or_insert_with(|| Budget::of_function("OPT113", f)),
                };
                let same_collection = premises.same_collection_each_time(inner, j.outer)
                    && premises.keys_unchanged(j.outer, j.inner_side);
                let exact = same_collection
                    && premises.steps_through_inputs(j.outer)
                    && premises.steps_through_inputs(inner)
                    && premises.trips_open(j.outer)
                    && premises.trips_open(inner)
                    && premises.idle_on_mismatch(inner, j.outer, j.cmp)
                    && premises.steps_run_no_code(inner);
                // The advice names a `set` or a `dict` only for keys known to be hashable, and
                // an index built before the outer loop only for an inner collection that is the
                // same on every outer iteration.
                let mut conditions = Vec::new();
                if !same_collection {
                    conditions.push(format!(
                        "`{inner_iter}` is the same collection, with the same keys, on every iteration of the outer loop"
                    ));
                }
                // The index holds the inner keys.
                if unhashable.is_none() && !defs.hashable(j.inner_side, types) {
                    conditions.push("the keys are hashable".to_owned());
                }
                let advice = if let Some((k, kind)) = unhashable {
                    let key = defs.element_text(unit.source, k);
                    format!(
                        "`{key}` is an unhashable `{kind}`, so index `{inner_iter}` by a hashable form of it (a `tuple` or `frozenset` of its items) before the outer loop"
                    )
                } else if !defs.stops_at_first_match(j.cmp, inner.lp) {
                    // Every match is used: an index must keep each key's elements, duplicates
                    // included, and an outer key that matches nothing still gets its empty
                    // list (a lookup with a default, not a loop over the index).
                    let empty_groups = "then read each outer key with `.get(key, [])` so that a key with no element keeps its empty list";
                    if defs.whole_element(j.inner_side) {
                        format!(
                            "group the elements of `{inner_iter}` by value in a `dict` (value → list of equal elements) before the outer loop, {empty_groups}"
                        )
                    } else {
                        let key = defs.element_text(unit.source, j.inner_side);
                        format!(
                            "index `{inner_iter}` in a `dict` from `{key}` to the list of elements with that key before the outer loop, {empty_groups}"
                        )
                    }
                } else if defs.whole_element(j.inner_side) {
                    format!("build a `set` of `{inner_iter}` before the outer loop")
                } else {
                    let key = defs.element_text(unit.source, j.inner_side);
                    format!("index `{inner_iter}` by `{key}` in a `dict` before the outer loop")
                };
                let help = if let Some(mapping) =
                    defs.keyed_mapping(inner, j.inner_side, &inner_iter)
                {
                    // Its keys are hashable, and a lookup reads the mapping as it is then.
                    format!(
                        "`{mapping}` is already keyed by `{}`: look each outer key up in it (`{mapping}.get(...)`) instead of looping over it",
                        defs.element_text(unit.source, j.inner_side)
                    )
                } else if conditions.is_empty() {
                    advice
                } else {
                    format!("if {}, {advice}", conditions.join(" and "))
                };
                // An exact report states its claim: what a pair that does not match runs.
                let message = if exact {
                    format!(
                        "for each item of `{outer_iter}`, the loop over `{inner_iter}` compares `{lhs} == {rhs}` and does nothing else for an item that does not match"
                    )
                } else {
                    format!(
                        "nested loops join `{outer_iter}` and `{inner_iter}` on `{lhs} == {rhs}`"
                    )
                };
                let d = cx
                    .emit(
                        out,
                        "OPT113",
                        unit,
                        j.span,
                        heki_effect::func_qualname(f),
                        format!("{lhs}=={rhs}"),
                        message,
                        Some(help),
                        &[],
                    )
                    .expect("the rule runs on the unit: it returned at its start otherwise");
                d.certainty = crate::premises::tier("OPT113", exact).at_most(d.certainty);
                if exact {
                    d.evidence = Some(format!(
                        "established: both loops step through inputs, a pair that does not match runs only the comparison, and `{inner_iter}` is the same collection, with the same keys, on every outer iteration"
                    ));
                }
            }
        }
    }
}

/// Collection methods that only read their receiver.
const READING_METHODS: &[&str] = &[
    "items",
    "keys",
    "values",
    "get",
    "index",
    "count",
    "copy",
    "split",
    "rsplit",
    "splitlines",
    "strip",
    "lower",
    "upper",
    "startswith",
    "endswith",
];

/// Whether `v` is known to be an instance of one of `classes`: its inferred type, or every class
/// of an inferred union.
fn known_instance(types: &FxHashMap<VarId, Val>, v: VarId, classes: &[&str]) -> bool {
    match types.get(&v) {
        Some(Val::Inst(class)) => classes.contains(&class.as_str()),
        Some(Val::Union(members)) => members.iter().all(|c| classes.contains(&c.as_str())),
        _ => false,
    }
}

/// Methods whose result steps through their receiver's items or text without running code of
/// the project, and the receivers they do it for: the views of a `dict`, the pieces of a `str`.
const VIEWS: &[(&str, &[&str])] = &[
    ("items", &["builtins.dict"]),
    ("keys", &["builtins.dict"]),
    ("values", &["builtins.dict"]),
    ("split", &["builtins.str", "builtins.bytes"]),
    ("rsplit", &["builtins.str", "builtins.bytes"]),
    ("splitlines", &["builtins.str", "builtins.bytes"]),
];

/// The builtin containers whose iteration runs no code of the project.
const CONTAINERS: &[&str] = &[
    "builtins.list",
    "builtins.tuple",
    "builtins.set",
    "builtins.frozenset",
    "builtins.dict",
    "builtins.str",
    "builtins.bytes",
    "builtins.range",
];

/// The premises of an `OPT113` report's exact tier (ADR-018) besides the comparison of keys,
/// each established or not; a walk that runs out of its budget establishes nothing.
struct JoinPremises<'p, 'a, 'c> {
    defs: &'p Defs<'p>,
    sizes: &'p Sizes<'a, 'c>,
    /// The function's inferred types, annotations included (assumption R2).
    types: &'p FxHashMap<VarId, Val>,
    decls: &'p ProjectDecls,
    origins: &'p ItemOrigins<'a>,
    budget: &'p Budget,
}

impl JoinPremises<'_, '_, '_> {
    fn builtin_type(&self, v: VarId, classes: &[&str]) -> bool {
        known_instance(self.types, v, classes)
    }

    /// Whether loading `attr` from `obj` reads a plain attribute of a project class heki knows
    /// `obj` to be an instance of (every class of a union), running no code of the project
    /// (`ProjectDecls::plain_attr`). An object of unknown class may have a property of that
    /// name.
    fn plain_key_attr(&self, obj: VarId, attr: &str) -> bool {
        match self.types.get(&obj) {
            Some(Val::Inst(class)) => self.decls.plain_attr(class, attr),
            Some(Val::Union(classes)) => classes.iter().all(|c| self.decls.plain_attr(c, attr)),
            _ => false,
        }
    }

    /// Whether the inner loop's builtin step makes tuples: it steps through `items()` of a
    /// `dict`, `enumerate(...)`, or `zip(...)`.
    fn steps_through_tuples(&self, inner: &ForLoop<'_>) -> bool {
        let shape = &self.sizes.ix.shape;
        match shape.inst(shape.copy_root(inner.iterable)).map(|i| &i.kind) {
            Some(InstKind::MethodCall { recv, method, .. }) => {
                method == "items" && self.builtin_type(*recv, &["builtins.dict"])
            }
            Some(InstKind::Call {
                callee_var, callee, ..
            }) => matches!(
                self.sizes.ix.builtin(*callee_var, callee),
                Some("enumerate" | "zip")
            ),
            _ => false,
        }
    }

    /// Whether no project class derived from one of `classes` overrides `method`, looked up
    /// `by_name` or called implicitly ([`ProjectDecls::method_not_overridden`]).
    fn not_overridden(&self, classes: &[&str], method: &str, by_name: bool) -> bool {
        classes
            .iter()
            .all(|class| self.decls.method_not_overridden(class, method, by_name))
    }

    /// Whether iterating `v` runs no code of the project: its type is a builtin container, and
    /// no project subclass of one defines its own `__iter__`.
    fn iterates_as_builtin(&self, v: VarId) -> bool {
        self.builtin_type(v, CONTAINERS) && self.not_overridden(CONTAINERS, "__iter__", false)
    }

    /// Whether running `inst` may run code of the project that reaches a value `hit` accepts: a
    /// call (but an inert builtin, or a method of a builtin container, which only stores or
    /// reads its arguments) that receives one or a closure capturing one, a call of such a
    /// closure, a method of one that is not a reading one, an in-place operator on one, the making
    /// of a closure that captures one, or a `yield` or `await`, after which other code may run.
    fn runs_code_reaching(&self, inst: &Inst, hit: &dyn Fn(VarId) -> bool) -> bool {
        let shape = &self.sizes.ix.shape;
        let captures = |v: VarId| {
            matches!(
                shape.inst(shape.copy_root(v)).map(|i| &i.kind),
                Some(InstKind::MakeClosure { captured, .. }) if captured.iter().any(|c| hit(*c))
            )
        };
        let handed = |args: &[VarId], kwargs: &[(Option<String>, VarId)]| {
            args.iter()
                .chain(kwargs.iter().map(|(_, v)| v))
                .any(|a| hit(*a) || captures(*a))
        };
        match &inst.kind {
            InstKind::Call {
                callee_var,
                callee,
                args,
                kwargs,
                ..
            } => {
                callee_var.is_some_and(captures)
                    || (!self.inert_builtin(*callee_var, callee, args, kwargs)
                        && handed(args, kwargs))
            }
            InstKind::MethodCall {
                recv,
                method,
                args,
                kwargs,
                ..
            } => {
                (hit(*recv) && !READING_METHODS.contains(&method.as_str()))
                    || (!self.builtin_type(*recv, CONTAINERS) && handed(args, kwargs))
            }
            InstKind::MakeClosure { captured, .. } => captured.iter().any(|c| hit(*c)),
            // `rows += [...]` extends a list in place.
            InstKind::BinOp {
                lhs,
                in_place: true,
                ..
            } => hit(*lhs),
            InstKind::Yield { .. } | InstKind::YieldFrom { .. } | InstKind::Await { .. } => true,
            _ => false,
        }
    }

    /// Whether a call is to a builtin that changes no argument and runs no code of the project: no function handed to it (a `key=`, any keyword, `map` / `filter`'s function,
    /// a `lambda` or a generator expression as an argument).
    fn inert_builtin(
        &self,
        callee_var: Option<VarId>,
        callee: &str,
        args: &[VarId],
        kwargs: &[(Option<String>, VarId)],
    ) -> bool {
        let shape = &self.sizes.ix.shape;
        let builtin = self.sizes.ix.builtin(callee_var, callee);
        builtin.is_some_and(|name| name != "map" && name != "filter")
            && kwargs.is_empty()
            && !args.iter().any(|a| {
                matches!(
                    shape.inst(*a).map(|i| &i.kind),
                    Some(InstKind::MakeClosure { .. } | InstKind::MakeGenExpr { .. })
                )
            })
    }

    /// The loop steps through items of the function's inputs: every source of its items is a
    /// parameter, an attribute or an item of one (a call result needs the catalog to say what
    /// it yields, and a method's receiver, a module constant, or a display is not an input).
    fn steps_through_inputs(&self, lp: &ForLoop<'_>) -> bool {
        // Open (a budget spent) establishes nothing.
        self.origins
            .of_loop(self.budget, self.defs.f.id, lp.index)
            .map(|origins| {
                !origins.is_empty()
                    && origins.iter().all(|o| {
                        matches!(o.source, ItemSource::Param(_) | ItemSource::ParamPart(_))
                    })
            })
            .open_means(false)
    }

    /// The code fixes no trip count of the loop: a fixed count past 16 is no evidence either
    /// way, so the exact tier leaves it out (a smaller one is not reported at all).
    fn trips_open(&self, lp: &ForLoop<'_>) -> bool {
        self.sizes
            .ix
            .fixed_trips(lp.lp, &mut |inst| self.sizes.leaf_len(inst))
            .is_open()
    }

    /// A pair whose keys differ runs nothing but the comparison: from the inner loop's header,
    /// every block a pair runs until the next pair, taking the false side of the comparison's
    /// branch, holds only the loads of the two items' keys and the comparison, and no other
    /// branch (an earlier `and` operand, an `if` around the test), `return`, `raise`, or way
    /// out of the inner loop.
    fn idle_on_mismatch(&self, inner: &ForLoop<'_>, outer: &ForLoop<'_>, cmp: VarId) -> bool {
        let f = self.defs.f;
        let shape = &self.sizes.ix.shape;
        let alias = forward_aliases(f, &[cmp]);
        let header = inner.lp.header;
        let key = |v: &VarId| inner.element.contains(v) || outer.element.contains(v);
        // A constant read only as the index of a key (`row["id"]`) runs no code either.
        let index_of_key = |c: &VarId| {
            shape.readers(*c).iter().all(|u| {
                matches!(*u, Use::Inst(b, i) if matches!(
                    &f.block(b).insts[i as usize].kind,
                    InstKind::SubscriptLoad { dst, index, .. } if index == c && key(dst)
                ))
            })
        };
        // A key load runs no code of the project: a plain attribute of an item whose class heki
        // knows, an item of a builtin container, or a component of the tuple the inner loop's
        // builtin step makes (`for k, v in d.items()`).
        let tuple_item = self
            .steps_through_tuples(inner)
            .then(|| self.defs.next_of(inner.lp).map(|(e, _)| e))
            .flatten();
        let key_work = |inst: &Inst| match &inst.kind {
            InstKind::IterNext { .. } => true,
            InstKind::Const { dst, .. } => index_of_key(dst),
            InstKind::Copy { dst, .. } => key(dst),
            InstKind::AttrLoad { dst, obj, attr } => key(dst) && self.plain_key_attr(*obj, attr),
            InstKind::SubscriptLoad { dst, obj, .. } => {
                key(dst)
                    && (self.builtin_type(*obj, CONTAINERS)
                        || tuple_item == Some(shape.copy_root(*obj)))
            }
            InstKind::Compare { dst, .. } => *dst == cmp,
            _ => false,
        };
        let found = walk::reach(
            self.budget,
            [header],
            |b, out| {
                match f.block(b).terminator {
                    Terminator::Branch { cond, else_bb, .. } if alias.contains(&cond) => {
                        out.push(else_bb);
                    }
                    ref t => out.extend(t.successors()),
                }
                // The next pair starts at the header again; its exit ends the loop.
                out.retain(|s| *s != header && (b != header || shape.in_loop(inner.index, *s)));
            },
            |b| {
                let block = f.block(b);
                let other_branch = b != header
                    && matches!(block.terminator, Terminator::Branch { cond, .. } if !alias.contains(&cond));
                let leaves = matches!(
                    block.terminator,
                    Terminator::Return(_) | Terminator::Raise(_) | Terminator::Unreachable
                );
                if !shape.in_loop(inner.index, b)
                    || other_branch
                    || leaves
                    || !block.insts.iter().all(key_work)
                {
                    walk::Visit::Stop(())
                } else {
                    walk::Visit::Expand
                }
            },
        );
        matches!(found, Answer::Known(None))
    }

    /// Advancing the inner loop runs no code of the project: its iterable is a builtin
    /// container by its type (an annotation counts, assumption R2), a display or a
    /// comprehension's result, a builtin that builds a container (`sorted`, `list`, `range`,
    /// ...), a view of a `dict` or the pieces of a `str`, or `enumerate` / `zip` / `reversed` /
    /// `iter` of such iterables, also through a phi. A value of unknown type (it may be a
    /// generator), a `map` or `filter` call, or any other call may run code for every pair.
    fn steps_run_no_code(&self, inner: &ForLoop<'_>) -> bool {
        let f = self.defs.f;
        let shape = &self.sizes.ix.shape;
        let ix = &self.sizes.ix;
        fn lazy<'i>(ix: &FnIndex<'_>, shape: &FnShape<'_>, inst: &'i Inst) -> Option<&'i [VarId]> {
            match &inst.kind {
                InstKind::Call {
                    dst,
                    callee_var,
                    callee,
                    args,
                    ..
                } if matches!(
                    ix.builtin(*callee_var, callee),
                    Some("enumerate" | "zip" | "reversed" | "iter")
                ) =>
                {
                    (0..args.len())
                        .all(|i| !shape.is_starred(*dst, i))
                        .then_some(args.as_slice())
                }
                _ => None,
            }
        }
        let found = walk::reach(
            self.budget,
            [inner.iterable],
            |v, out| {
                let v = shape.copy_root(v);
                if let Def::Phi(b, i) = shape.def(v) {
                    out.extend(f.block(b).phis[i as usize].incoming.iter().map(|(_, x)| *x));
                } else if let Some(args) = shape.inst(v).and_then(|i| lazy(ix, shape, i)) {
                    out.extend(args.iter().copied());
                }
            },
            |v| {
                let v = shape.copy_root(v);
                let inst = shape.inst(v);
                // A phi through its inputs, a lazy builtin through its arguments.
                if matches!(shape.def(v), Def::Phi(..))
                    || inst.is_some_and(|i| lazy(ix, shape, i).is_some())
                {
                    return walk::Visit::Expand;
                }
                let builds = inst.map_or_else(
                    || self.iterates_as_builtin(v),
                    |inst| match &inst.kind {
                        InstKind::AllocBuiltin { .. } | InstKind::DictLiteral { .. } => true,
                        InstKind::Call {
                            callee_var, callee, ..
                        } => matches!(
                            ix.builtin(*callee_var, callee),
                            Some(
                                "sorted"
                                    | "list"
                                    | "tuple"
                                    | "set"
                                    | "frozenset"
                                    | "dict"
                                    | "range"
                            )
                        ),
                        InstKind::MethodCall { recv, method, .. } => {
                            VIEWS.iter().any(|(m, classes)| {
                                m == method
                                    && self.builtin_type(*recv, classes)
                                    && self.not_overridden(classes, m, true)
                            })
                        }
                        _ => self.iterates_as_builtin(v),
                    },
                );
                if builds {
                    walk::Visit::Prune
                } else {
                    walk::Visit::Stop(())
                }
            },
        );
        matches!(found, Answer::Known(None))
    }

    /// The outer loop leaves the compared key of every inner item as it is: it stores into or
    /// deletes no attribute or constant item that the key `key` loads from the inner item
    /// (`b.k = ...` for the key `b.k`), and changes neither the key's object nor an object the key
    /// is loaded from with a method that is not a reading one.
    fn keys_unchanged(&self, outer: &ForLoop<'_>, key: VarId) -> bool {
        let f = self.defs.f;
        let shape = &self.sizes.ix.shape;
        let in_outer = |b: BlockId| shape.in_loop(outer.index, b);
        // Each load on the way from the inner item to the key: the object it reads (through
        // copies), and the attribute or constant item it takes (`None`: a computed item).
        let mut loads: Vec<(VarId, Option<&str>)> = Vec::new();
        let walked = walk::reach(
            self.budget,
            [key],
            |v, out| {
                if let Some(InstKind::AttrLoad { obj, .. } | InstKind::SubscriptLoad { obj, .. }) =
                    shape.inst(shape.copy_root(v)).map(|i| &i.kind)
                {
                    out.push(*obj);
                }
            },
            |v| {
                match shape.inst(shape.copy_root(v)).map(|i| &i.kind) {
                    Some(InstKind::AttrLoad { obj, attr, .. }) => {
                        loads.push((shape.copy_root(*obj), Some(attr.as_str())));
                    }
                    Some(InstKind::SubscriptLoad { obj, key_const, .. }) => {
                        loads.push((shape.copy_root(*obj), key_const.as_deref()));
                    }
                    _ => {}
                }
                walk::Visit::<()>::Expand
            },
        );
        let key = shape.copy_root(key);
        // A store, a deletion, or a changing method of the key's object or of an object the key
        // is loaded from, at what the key loads (`None`: anything).
        let on_the_way = |v: VarId| {
            let v = shape.copy_root(v);
            v == key || loads.iter().any(|(read, _)| *read == v)
        };
        let rekeys = |inst: &Inst| {
            if self.runs_code_reaching(inst, &on_the_way) {
                return true;
            }
            let (obj, what) = match &inst.kind {
                InstKind::AttrStore { obj, attr, .. } | InstKind::AttrDelete { obj, attr } => {
                    (*obj, Some(attr.as_str()))
                }
                InstKind::SubscriptStore { obj, index, .. }
                | InstKind::SubscriptDelete { obj, index, .. } => (*obj, shape.str_const(*index)),
                _ => return false,
            };
            let obj = shape.copy_root(obj);
            obj == key
                || loads.iter().any(|(read, step)| {
                    *read == obj && (what.is_none() || step.is_none() || what == *step)
                })
        };
        !walked.is_open()
            && !f
                .blocks
                .iter()
                .filter(|b| in_outer(b.id))
                .any(|b| b.insts.iter().any(rekeys))
    }

    /// The inner loop steps through the same collection on every iteration of the outer one:
    /// its iterable is bound before the outer loop, or computed inside it only from constants,
    /// module names, and values bound before it (through copies, attribute and item loads,
    /// operators, displays, read-only builtins, and reading methods), and nothing in the outer
    /// loop changes one of those values through its names: no changing method, store, or
    /// deletion, no call that receives it, and no `yield` or `await`, after which other code
    /// may run.
    fn same_collection_each_time(&self, inner: &ForLoop<'_>, outer: &ForLoop<'_>) -> bool {
        let f = self.defs.f;
        let shape = &self.sizes.ix.shape;
        let in_outer = |b: BlockId| shape.in_loop(outer.index, b);
        // A string built in the loop is not followed: an f-string's IR drops its operands.
        let pure_step = |inst: &Inst| match &inst.kind {
            InstKind::Copy { .. }
            | InstKind::AttrLoad { .. }
            | InstKind::SubscriptLoad { .. }
            | InstKind::BinOp { .. } => true,
            InstKind::AllocBuiltin { kind, .. } => *kind != BuiltinKind::Str,
            InstKind::Call {
                callee_var,
                callee,
                args,
                kwargs,
                ..
            } => self.inert_builtin(*callee_var, callee, args, kwargs),
            InstKind::MethodCall { method, .. } => READING_METHODS.contains(&method.as_str()),
            _ => false,
        };
        let mut read = Vec::new();
        let found = walk::reach(
            self.budget,
            [inner.iterable],
            |v, out| {
                if let Some((b, inst)) = shape.defining(v)
                    && in_outer(b)
                {
                    out.extend(inst.kind.operands());
                }
            },
            |v| {
                read.push(v);
                match shape.def(v) {
                    Def::Param => walk::Visit::Prune,
                    Def::Phi(b, _) if !in_outer(b) => walk::Visit::Prune,
                    Def::Inst(b, _) if !in_outer(b) => walk::Visit::Prune,
                    Def::Inst(..) => match shape.inst(v) {
                        Some(inst)
                            if matches!(
                                inst.kind,
                                InstKind::Const { .. } | InstKind::LoadGlobal { .. }
                            ) =>
                        {
                            walk::Visit::Prune
                        }
                        Some(inst) if pure_step(inst) => walk::Visit::Expand,
                        _ => walk::Visit::Stop(()),
                    },
                    Def::Phi(..) | Def::None => walk::Visit::Stop(()),
                }
            },
        );
        if !matches!(found, Answer::Known(None)) {
            return false;
        }
        // The names of those values: every value they copy, or a phi of theirs takes (`rows =
        // ys` before the loop makes `ys` a name of `rows`), and every copy of those.
        let mut sources = Vec::new();
        let traced = walk::reach(
            self.budget,
            read,
            |v, out| match shape.def(v) {
                Def::Phi(b, i) => {
                    out.extend(f.block(b).phis[i as usize].incoming.iter().map(|(_, x)| *x));
                }
                _ => out.extend(shape.inst(v).and_then(|i| match i.kind {
                    InstKind::Copy { src, .. } => Some(src),
                    _ => None,
                })),
            },
            |v| {
                sources.push(v);
                walk::Visit::<()>::Expand
            },
        );
        // A walk past its budget leaves names out, so it establishes nothing.
        let complete = !traced.is_open();
        let names = forward_aliases(f, &sources);
        let named = |v: &VarId| names.contains(v);
        let changes = |inst: &Inst| match &inst.kind {
            InstKind::AttrStore { obj, .. }
            | InstKind::AttrDelete { obj, .. }
            | InstKind::SubscriptStore { obj, .. }
            | InstKind::SubscriptDelete { obj, .. } => named(obj),
            _ => self.runs_code_reaching(inst, &|v| named(&v)),
        };
        complete
            && !f
                .blocks
                .iter()
                .filter(|b| in_outer(b.id))
                .any(|b| b.insts.iter().any(changes))
    }
}
