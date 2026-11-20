//! The eight Family 3 (`OPT`) rules: IR patterns, Proof Obligations, Profitability, and minimal
//! byte-span [`TextEdit`]s (docs/ANALYSIS_AND_KB_SPEC.md section 3.3).
//!
//! A candidate is produced for every pattern occurrence; [`OptCandidate::is_reportable`]
//! decides whether it becomes a diagnostic (not `Disproved`, profitable, and the container's
//! exact type established — without that there is no evidence of a quadratic cost).
//! Edits are offered for `Safe` candidates only ([`OptCandidate::fix`]).

use crate::edits::{
    ends_statement, header_colon_follows, indentation, is_identifier, is_plain_assign, line_start,
    slice, squash, starts_statement, strip_token, trim_element,
};
use crate::interpreter::{Analysis, BuiltinScope, FnCtx, UseSite};
use crate::liveness::Liveness;
use crate::obligations::{Checker, ObligationKind, ProofObligation, SafetyVerdict};
use crate::{FrameState, KindMask, ObligationStatus, ProfitabilityTier, SafetyTier};
use heki_ast::{LineIndex, Span};
use heki_ir::{
    BinOpKind, BlockId, BuiltinKind, CompareOpKind, DictEntry, FnIndex, Inst, InstKind, IrFunction,
    LoopBound, ModuleIr, NaturalLoop, Terminator, VarId,
};
use heki_kb::{EffectiveSoundnessKb, SoundnessParamEffect};
use rustc_hash::FxHashSet;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;

use BuiltinKind::{Bytes, Dict, List, Set, Str, Tuple};
use ObligationKind as K;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum OptRule {
    Opt101,
    Opt102,
    Opt103,
    Opt110,
    Opt111,
    Opt120,
    Opt121,
    Opt130,
}

impl OptRule {
    pub const ALL: [Self; 8] = [
        Self::Opt101,
        Self::Opt102,
        Self::Opt103,
        Self::Opt110,
        Self::Opt111,
        Self::Opt120,
        Self::Opt121,
        Self::Opt130,
    ];

    pub fn code(self) -> &'static str {
        match self {
            Self::Opt101 => "OPT101",
            Self::Opt102 => "OPT102",
            Self::Opt103 => "OPT103",
            Self::Opt110 => "OPT110",
            Self::Opt111 => "OPT111",
            Self::Opt120 => "OPT120",
            Self::Opt121 => "OPT121",
            Self::Opt130 => "OPT130",
        }
    }

    /// `Unconditional` only for `OPT101` / `OPT102` (ADR-011).
    pub fn profitability(self) -> ProfitabilityTier {
        match self {
            Self::Opt101 | Self::Opt102 => ProfitabilityTier::Unconditional,
            _ => ProfitabilityTier::SizeDependent,
        }
    }
}

/// Minimal byte-span replacement in the analyzed source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextEdit {
    pub span: Span,
    pub replacement: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptCandidate {
    pub rule: OptRule,
    /// Module-relative qualname of the function.
    pub function: String,
    /// The rewritten statement or expression.
    pub span: Span,
    /// The accumulator / container (baseline key target).
    pub target: String,
    pub message: String,
    pub verdict: SafetyVerdict,
    pub profitability: ProfitabilityTier,
    /// Why the rewrite would not pay off here (small constant input, bounded loop).
    pub not_profitable: Option<String>,
    /// What proves the quadratic / linear cost (`None`: nothing does, e.g. `x in param`).
    pub cost_evidence: Option<String>,
    pub edits: Vec<TextEdit>,
}

impl OptCandidate {
    pub fn is_safe(&self) -> bool {
        self.verdict.tier == SafetyTier::Safe
    }

    /// Reported as a diagnostic: not `Disproved`, profitable, and with proven cost (an exact
    /// builtin container), whatever the remaining blockers.
    pub fn is_reportable(&self) -> bool {
        self.verdict.tier != SafetyTier::Disproved
            && self.not_profitable.is_none()
            && self.cost_evidence.is_some()
    }

    /// The edits, when the rewrite is proven safe.
    pub fn fix(&self) -> Option<&[TextEdit]> {
        (self.is_safe() && !self.edits.is_empty()).then_some(self.edits.as_slice())
    }
}

/// Per-run options.
#[derive(Debug, Clone, Default)]
pub struct AnalysisOptions {
    /// Some module of the project rebinds attributes of `builtins` (see
    /// [`module_patches_builtins`]): no builtin name can be trusted (Assumption A1(b)).
    pub project_patches_builtins: bool,
    /// Obligations recorded as established. ONLY for obligation-stripping tests, which show
    /// that each obligation is what rejects its counterexample.
    #[doc(hidden)]
    pub disabled_obligations: Vec<ObligationKind>,
}

/// The module imports `builtins` or touches `__builtins__` (it may patch builtin names).
pub fn module_patches_builtins(ir: &ModuleIr) -> bool {
    ir.functions.iter().any(|f| {
        f.blocks
            .iter()
            .flat_map(|b| &b.insts)
            .any(|i| match &i.kind {
                InstKind::Import { module, name, .. } => {
                    module.as_deref() == Some("builtins")
                        || (module.is_none() && name == "builtins")
                }
                InstKind::LoadGlobal { name, .. } | InstKind::StoreGlobal { name, .. } => {
                    name == "__builtins__"
                }
                _ => false,
            })
    })
}

/// Module-level facts for edits.
struct ModuleCtx<'a> {
    source: &'a str,
    lines: LineIndex,
    builtins: BuiltinScope,
    used_names: FxHashSet<String>,
    /// Per function (by its span): the names it binds itself (parameters, assignments,
    /// imports, handler names, free variables), i.e. every name but the module globals it reads.
    local_names: Vec<(Span, FxHashSet<&'a str>)>,
    /// How to spell `collections.deque` (`None`: no safe way to import it).
    deque: Option<DequeRef>,
}

#[derive(Debug, Clone)]
struct DequeRef {
    spelling: String,
    /// The edit adding `import collections` first, when the module does not bind it
    /// (`crate::imports::collections_import`).
    import: Option<(Span, String)>,
}

impl<'a> ModuleCtx<'a> {
    fn new(ir: &'a ModuleIr, source: &'a str, opts: &AnalysisOptions) -> Self {
        let builtins = BuiltinScope::for_module(ir, opts.project_patches_builtins);
        let mut used_names = FxHashSet::default();
        let mut stores: rustc_hash::FxHashMap<&str, u32> = rustc_hash::FxHashMap::default();
        for f in &ir.functions {
            used_names.extend(f.var_names.iter().flatten().cloned());
            for inst in f.blocks.iter().flat_map(|b| &b.insts) {
                match &inst.kind {
                    InstKind::LoadGlobal { name, .. }
                    | InstKind::LoadDeref { name, .. }
                    | InstKind::LoadName { name, .. }
                    | InstKind::StoreDeref { name, .. } => {
                        used_names.insert(name.clone());
                    }
                    InstKind::StoreGlobal { name, .. } => {
                        used_names.insert(name.clone());
                        *stores.entry(name.as_str()).or_default() += 1;
                    }
                    InstKind::Call { callee, .. } => {
                        used_names.insert(callee.split('.').next().unwrap_or(callee).to_owned());
                    }
                    _ => {}
                }
            }
        }
        let module_fn = ir.function(ir.module_fn);
        let mut deque = None;
        for inst in module_fn.blocks.iter().flat_map(|b| &b.insts) {
            let InstKind::Import {
                dst, module, name, ..
            } = &inst.kind
            else {
                continue;
            };
            let bound = module_fn.var_name(*dst).unwrap_or_default();
            let single = stores.get(bound).copied() == Some(1);
            if module.as_deref() == Some("collections") && name == "deque" && single {
                deque = Some(DequeRef {
                    spelling: bound.to_owned(),
                    import: None,
                });
            } else if module.is_none()
                // `import collections.abc` binds `collections` too.
                && (name == "collections" || name.starts_with("collections."))
                && bound == "collections"
                && single
                && deque.is_none()
            {
                deque = Some(DequeRef {
                    spelling: "collections.deque".into(),
                    import: None,
                });
            }
        }
        if deque.is_none() && !used_names.contains("collections") {
            deque = crate::imports::collections_import(source, module_fn).map(|import| DequeRef {
                spelling: "collections.deque".into(),
                import: Some(import),
            });
        }
        let local_names = ir
            .functions
            .iter()
            .filter(|f| !f.is_module_body && !f.is_class_body)
            .map(|f| {
                let global_reads: FxHashSet<VarId> = f
                    .blocks
                    .iter()
                    .flat_map(|b| &b.insts)
                    .filter(|i| matches!(i.kind, InstKind::LoadGlobal { .. }))
                    .filter_map(|i| i.kind.dst())
                    .collect();
                let names = f
                    .var_names
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !global_reads.contains(&VarId(*i as u32)))
                    .filter_map(|(_, n)| n.as_deref())
                    .collect();
                (f.span, names)
            })
            .collect();
        // An import heki sees may be rebound behind the IR (star import, `exec`, `globals()`),
        // and so may a new one: no spelling of `deque` is trusted then.
        let deque = deque.filter(|_| !builtins.dynamic_globals());
        Self {
            source,
            lines: LineIndex::from_source(source),
            builtins,
            used_names,
            local_names,
            deque,
        }
    }

    /// `f` or a function enclosing it binds `name`, so a reference to `name` added inside `f`
    /// would not reach the module global or the builtin. Enclosing functions are the ones whose
    /// span contains `f`'s (class bodies in between are not scopes `f` sees, and qualnames do
    /// not always name the functions around a class).
    fn binds_locally(&self, f: &IrFunction, name: &str) -> bool {
        self.local_names.iter().any(|(span, names)| {
            let encloses = span.start <= f.span.start && f.span.end <= span.end;
            // A bare annotation (`set: object`) makes the name local without a value the IR
            // records.
            encloses
                && (names.contains(name)
                    || self
                        .text(*span)
                        .is_some_and(|t| annotated_declarations(t, name).next().is_some()))
        })
    }

    fn text(&self, span: Span) -> Option<&'a str> {
        slice(self.source, span.start, span.end)
    }

    /// A name not used anywhere in the module nor `taken` by another edit of the function.
    fn fresh_name(&self, base: &str, taken: &FxHashSet<String>) -> String {
        let base = if is_identifier(base) { base } else { "hoisted" };
        let free = |n: &str| !self.used_names.contains(n) && !taken.contains(n);
        if free(base) {
            return base.to_owned();
        }
        (1u64..)
            .map(|i| format!("{base}_{i}"))
            .find(|n| free(n))
            .expect("a module uses finitely many names")
    }
}

/// Analyze every function of a module for `OPT` candidates (module and class bodies are
/// skipped: their namespaces are observable from outside the frame).
pub fn analyze_module(
    ir: &ModuleIr,
    source: &str,
    kb: &EffectiveSoundnessKb,
    opts: &AnalysisOptions,
) -> Vec<OptCandidate> {
    let m = ModuleCtx::new(ir, source, opts);
    let mut out = Vec::new();
    for f in &ir.functions {
        if f.is_module_body || f.is_class_body {
            continue;
        }
        let cx = FnCtx::new(f, kb, &m.builtins);
        let sites = find_sites(&cx);
        if sites.is_empty() {
            continue;
        }
        let ix = FnIndex::new(f, &ir.constants);
        let an = cx.run();
        let live = Liveness::compute(f);
        let mutations = cx.mutations(&an);
        let ck = Checker::new(
            &cx,
            &an,
            &live,
            &mutations,
            &opts.disabled_obligations,
            &m.lines,
        );
        let rules = Rules {
            m: &m,
            ck: &ck,
            ix: &ix,
            hoisted_names: RefCell::new(FxHashSet::default()),
        };
        for site in &sites {
            if let Some(c) = rules.evaluate(site) {
                out.push(c);
            }
        }
    }
    out.sort_by_key(|a| (a.span.start, a.rule));
    out
}

// ================================================================================ patterns

#[derive(Debug, Clone, Copy)]
enum Membership<'a> {
    /// `e in lst` (`OPT110`).
    Plain,
    /// `e in set(keys)`: the container is rebuilt by the call at `at` (`OPT130`).
    Rebuild {
        at: (BlockId, usize),
        callee: &'a str,
        args: &'a [VarId],
    },
    /// `if v not in seen: seen.append(v)`: the append at `(block, index)` (`OPT111`).
    Dedup(BlockId, usize),
}

#[derive(Debug, Clone, Copy)]
enum FrontForm {
    /// `q.pop(0)`
    PopCall,
    /// `del q[0]`
    DelIndex,
    /// `q = q[1:]`: the `Copy` binding the slice.
    SliceRebind { copy: (BlockId, usize) },
}

#[derive(Debug, Clone, Copy)]
enum Site<'a> {
    /// `x = x + [e, ...]`: `BinOp` at `at`, list display `list`, `Copy` at `copy`.
    ListConcat {
        at: (BlockId, usize),
        acc: VarId,
        list: VarId,
        copy: (BlockId, usize),
    },
    /// `d = {**d, ...}`: `DictLiteral` at `at` defining `dst`.
    DictMerge {
        at: (BlockId, usize),
        acc: VarId,
        dst: VarId,
        entries: &'a [DictEntry],
        copy: (BlockId, usize),
    },
    /// `s = s | rhs`
    SetOr {
        at: (BlockId, usize),
        acc: VarId,
        rhs: VarId,
        copy: (BlockId, usize),
    },
    /// `acc = acc + rhs` / `acc += rhs` on a loop-carried local.
    ImmutableConcat {
        at: (BlockId, usize),
        acc: VarId,
        rhs: VarId,
        copy: Option<(BlockId, usize)>,
    },
    /// `obj.attr += rhs` in a loop.
    AttrConcat {
        at: (BlockId, usize),
        obj: VarId,
        attr: &'a str,
        rhs: VarId,
    },
    Membership {
        at: (BlockId, usize),
        probe: VarId,
        container: VarId,
        kind: Membership<'a>,
    },
    FrontPop {
        at: (BlockId, usize),
        q: VarId,
        form: FrontForm,
    },
    FrontInsert {
        at: (BlockId, usize),
        q: VarId,
    },
}

fn inst_at(f: &IrFunction, at: (BlockId, usize)) -> &Inst {
    &f.block(at.0).insts[at.1]
}

fn innermost_loop(f: &IrFunction, b: BlockId) -> Option<&NaturalLoop> {
    f.innermost_loop(b)
}

/// The single use of `v`, if it is a `Copy` into a variable named `name`.
fn sole_copy_to(cx: &FnCtx<'_>, v: VarId, name: &str) -> Option<(BlockId, usize)> {
    let [UseSite::Inst(b, i)] = cx.uses(v) else {
        return None;
    };
    match &cx.f.block(*b).insts[*i].kind {
        InstKind::Copy { dst, src } if *src == v && cx.f.var_name(*dst) == Some(name) => {
            Some((*b, *i))
        }
        _ => None,
    }
}

/// `v` is the self-accumulating loop-carried phi of a loop containing `b`.
fn loop_carried_phi(f: &IrFunction, v: VarId, b: BlockId) -> bool {
    f.loop_carried.iter().any(|lc| {
        lc.phi_var == v
            && lc.self_accumulating
            && f.loops
                .iter()
                .any(|lp| lp.header == lc.loop_header && lp.body.contains(&b))
    })
}

fn find_sites<'a>(cx: &FnCtx<'a>) -> Vec<Site<'a>> {
    let f = cx.f;
    let mut sites = Vec::new();
    let mut dedup_compares: FxHashSet<(BlockId, usize)> = FxHashSet::default();
    for bb in &f.blocks {
        if !f.is_in_loop(bb.id) {
            continue;
        }
        for (i, inst) in bb.insts.iter().enumerate() {
            let at = (bb.id, i);
            match &inst.kind {
                InstKind::BinOp {
                    dst,
                    op,
                    lhs,
                    rhs,
                    in_place,
                } => {
                    let name = f.var_name(*lhs);
                    let carried = name.is_some() && loop_carried_phi(f, *lhs, bb.id);
                    let list_rhs = cx.def_inst(*rhs).is_some_and(|(_, _, d)| {
                        matches!(d.kind, InstKind::AllocBuiltin { kind: List, .. })
                    });
                    if let (true, Some(name)) = (carried, name) {
                        let copy = sole_copy_to(cx, *dst, name);
                        match (op, in_place, copy) {
                            (BinOpKind::Add, false, Some(copy)) if list_rhs => {
                                sites.push(Site::ListConcat {
                                    at,
                                    acc: *lhs,
                                    list: *rhs,
                                    copy,
                                });
                            }
                            (BinOpKind::BitOr, false, Some(copy)) => sites.push(Site::SetOr {
                                at,
                                acc: *lhs,
                                rhs: *rhs,
                                copy,
                            }),
                            (BinOpKind::Add, false, Some(copy)) => {
                                sites.push(Site::ImmutableConcat {
                                    at,
                                    acc: *lhs,
                                    rhs: *rhs,
                                    copy: Some(copy),
                                })
                            }
                            (BinOpKind::Add, true, _) if f.var_name(*dst) == Some(name) => sites
                                .push(Site::ImmutableConcat {
                                    at,
                                    acc: *lhs,
                                    rhs: *rhs,
                                    copy: None,
                                }),
                            _ => {}
                        }
                    }
                    // `obj.attr += rhs`: AttrLoad -> BinOp(in place) -> AttrStore(same attr).
                    if *op == BinOpKind::Add
                        && *in_place
                        && let Some((_, _, load)) = cx.def_inst(*lhs)
                        && let InstKind::AttrLoad { obj, attr, .. } = &load.kind
                        && bb.insts.iter().skip(i + 1).any(|s| {
                            matches!(&s.kind, InstKind::AttrStore { obj: o, attr: a, value }
                                if o == obj && a == attr && value == dst)
                        })
                    {
                        sites.push(Site::AttrConcat {
                            at,
                            obj: *obj,
                            attr,
                            rhs: *rhs,
                        });
                    }
                }
                InstKind::DictLiteral { dst, entries } if entries.len() >= 2 => {
                    let acc = entries.iter().find_map(|e| match e {
                        DictEntry::Unpack { dict }
                            if f.var_name(*dict).is_some() && loop_carried_phi(f, *dict, bb.id) =>
                        {
                            Some(*dict)
                        }
                        _ => None,
                    });
                    if let Some(acc) = acc
                        && let Some(copy) = sole_copy_to(cx, *dst, f.var_name(acc).unwrap_or(""))
                    {
                        sites.push(Site::DictMerge {
                            at,
                            acc,
                            dst: *dst,
                            entries,
                            copy,
                        });
                    }
                }
                InstKind::Compare {
                    op: op @ (CompareOpKind::In | CompareOpKind::NotIn),
                    lhs,
                    rhs,
                    dst,
                } => {
                    let defined_inside = cx.def_inst(*rhs).filter(|(_, _, d)| {
                        d.span.start >= inst.span.start && d.span.end <= inst.span.end
                    });
                    let kind = match defined_inside {
                        Some((db, di, d)) => match &d.kind {
                            InstKind::Call {
                                callee_var,
                                callee,
                                args,
                                ..
                            } if cx.builtin_callee(*callee_var, callee).is_some_and(|n| {
                                matches!(n, "set" | "list" | "tuple" | "dict" | "frozenset")
                            }) =>
                            {
                                Some(Membership::Rebuild {
                                    at: (db, di),
                                    callee,
                                    args,
                                })
                            }
                            _ => None,
                        },
                        None => {
                            let dedup = (*op == CompareOpKind::NotIn)
                                .then(|| dedup_append(cx, bb.id, *dst, *lhs, *rhs))
                                .flatten();
                            Some(dedup.map_or(Membership::Plain, |(b, i)| {
                                dedup_compares.insert(at);
                                Membership::Dedup(b, i)
                            }))
                        }
                    };
                    if let Some(kind) = kind {
                        sites.push(Site::Membership {
                            at,
                            probe: *lhs,
                            container: *rhs,
                            kind,
                        });
                    }
                }
                InstKind::MethodCall {
                    recv,
                    method,
                    args,
                    kwargs,
                    ..
                } if kwargs.is_empty() => match (method.as_str(), args.as_slice()) {
                    ("pop", [c]) if f.const_int(*c) == Some(0) => sites.push(Site::FrontPop {
                        at,
                        q: *recv,
                        form: FrontForm::PopCall,
                    }),
                    ("insert", [c, _]) if f.const_int(*c) == Some(0) => {
                        sites.push(Site::FrontInsert { at, q: *recv })
                    }
                    _ => {}
                },
                InstKind::SubscriptDelete { obj, index, .. } if f.const_int(*index) == Some(0) => {
                    sites.push(Site::FrontPop {
                        at,
                        q: *obj,
                        form: FrontForm::DelIndex,
                    })
                }
                InstKind::SubscriptLoad {
                    dst,
                    obj,
                    is_slice: true,
                    slice_lower_const: Some(1),
                    slice_upper_const: None,
                    ..
                } => {
                    if let Some(name) = f.var_name(*obj)
                        && loop_carried_phi(f, *obj, bb.id)
                        && let Some(copy) = sole_copy_to(cx, *dst, name)
                    {
                        sites.push(Site::FrontPop {
                            at,
                            q: *obj,
                            form: FrontForm::SliceRebind { copy },
                        });
                    }
                }
                _ => {}
            }
        }
    }
    sites
}

/// `if v not in seen: seen.append(v)`: the compare's result drives the block's branch and the
/// then-block appends `v` to `seen`.
fn dedup_append(
    cx: &FnCtx<'_>,
    b: BlockId,
    cmp: VarId,
    probe: VarId,
    seen: VarId,
) -> Option<(BlockId, usize)> {
    let Terminator::Branch { cond, then_bb, .. } = &cx.f.block(b).terminator else {
        return None;
    };
    if *cond != cmp {
        return None;
    }
    cx.f.block(*then_bb)
        .insts
        .iter()
        .position(|i| {
            matches!(&i.kind, InstKind::MethodCall { recv, method, args, kwargs, .. }
                if *recv == seen && method == "append" && args.as_slice() == [probe] && kwargs.is_empty())
        })
        .map(|i| (*then_bb, i))
}

// ============================================================================ evaluation

struct Rules<'a, 'c> {
    m: &'c ModuleCtx<'a>,
    ck: &'c Checker<'a, 'c>,
    /// The shared size and lifetime facts (`heki_ir::FnIndex`): profitability only.
    ix: &'c FnIndex<'a>,
    /// Names the function's proven hoists bind: two fixes applied in one pass must not bind
    /// the same name.
    hoisted_names: RefCell<FxHashSet<String>>,
}

fn established(evidence: &str) -> ObligationStatus {
    ObligationStatus::Established {
        evidence: evidence.into(),
    }
}

fn blocked(blocker: impl Into<String>) -> ObligationStatus {
    ObligationStatus::Blocked {
        blocker: blocker.into(),
    }
}

fn disproved(counterexample: impl Into<String>) -> ObligationStatus {
    ObligationStatus::Disproved {
        counterexample: counterexample.into(),
    }
}

/// A Layer 1 parameter effect that neither changes nor stores the argument (it is only read,
/// or kept alive by the result).
fn reads_only(effect: &SoundnessParamEffect) -> bool {
    matches!(
        effect,
        SoundnessParamEffect::ReadOnly | SoundnessParamEffect::RetainedInReturn
    )
}

/// Bounded loops do not pay for size-dependent rewrites.
fn loop_bound_note(lp: &NaturalLoop) -> Option<String> {
    match lp.bound {
        LoopBound::ConstSmall(n) => Some(format!("the loop runs at most {n} times")),
        LoopBound::RetryPattern => Some("the loop is a retry loop".into()),
        LoopBound::DaemonPolling | LoopBound::Unbounded => None,
    }
}

impl<'a, 'c> Rules<'a, 'c> {
    /// The 1-based line of a source offset.
    fn line(&self, offset: u32) -> u32 {
        self.m.lines.line(offset)
    }

    fn f(&self) -> &'a IrFunction {
        self.ck.cx.f
    }

    fn cx(&self) -> &'c FnCtx<'a> {
        self.ck.cx
    }

    fn an(&self) -> &'c Analysis {
        self.ck.an
    }

    fn state(&self, at: (BlockId, usize)) -> Option<FrameState> {
        self.cx().state_before(self.an(), at.0, at.1)
    }

    fn var(&self, v: VarId) -> String {
        self.f()
            .var_name(v)
            .filter(|n| !n.starts_with('.'))
            .unwrap_or("<expr>")
            .to_owned()
    }

    fn base(&self) -> Vec<ProofObligation> {
        vec![self.ck.a1_hazard_free(), self.ck.converged()]
    }

    fn a1_blocks(&self) -> bool {
        !self.f().a1_hazards.is_empty() && !self.ck.disabled.contains(&K::A1HazardFree)
    }

    /// Every obligation is established: the candidate's edits will be offered.
    fn proven(&self, obs: &[ProofObligation]) -> bool {
        SafetyVerdict::evaluate(obs.to_vec(), self.a1_blocks()).tier == SafetyTier::Safe
    }

    /// A fresh name for a value a proven hoist binds, distinct from the function's other ones.
    fn hoisted_name(&self, base: &str) -> String {
        let mut taken = self.hoisted_names.borrow_mut();
        let name = self.m.fresh_name(base, &taken);
        taken.insert(name.clone());
        name
    }

    #[allow(clippy::too_many_arguments)]
    fn candidate(
        &self,
        rule: OptRule,
        span: Span,
        target: String,
        message: String,
        obligations: Vec<ProofObligation>,
        not_profitable: Option<String>,
        cost_evidence: Option<String>,
        edits: Vec<TextEdit>,
    ) -> OptCandidate {
        let a1 = self.a1_blocks();
        OptCandidate {
            rule,
            function: self.f().qualname.clone(),
            span,
            target,
            message,
            verdict: SafetyVerdict::evaluate(obligations, a1),
            profitability: rule.profitability(),
            not_profitable,
            cost_evidence,
            edits,
        }
    }

    /// The exact kind every loop-entry value of the loop-carried `phi` has, if one of `allowed`.
    fn init_kind(
        &self,
        st: &FrameState,
        phi: VarId,
        allowed: &[BuiltinKind],
    ) -> Option<BuiltinKind> {
        let lc = self.f().loop_carried.iter().find(|lc| lc.phi_var == phi)?;
        let first = st.exact_type(*lc.init_vars.first()?)?;
        let all_first = lc
            .init_vars
            .iter()
            .all(|v| st.exact_type(*v) == Some(first));
        (allowed.contains(&first) && all_first).then_some(first)
    }

    /// Cost evidence: `v`'s exact type at the site is one of `allowed`.
    fn exact_evidence(&self, st: &FrameState, v: VarId, allowed: &[BuiltinKind]) -> Option<String> {
        st.exact_type(v)
            .filter(|k| allowed.contains(k))
            .map(|k| format!("`{}` is an exact `{}`", self.var(v), k.name()))
    }

    /// Cost evidence of an accumulation: it starts as an exact `allowed` container.
    fn init_evidence(
        &self,
        st: &FrameState,
        phi: VarId,
        allowed: &[BuiltinKind],
    ) -> Option<String> {
        self.init_kind(st, phi, allowed)
            .map(|k| format!("`{}` starts as an exact `{}`", self.var(phi), k.name()))
    }

    fn evaluate(&self, site: &Site<'a>) -> Option<OptCandidate> {
        match *site {
            Site::ListConcat {
                at,
                acc,
                list,
                copy,
            } => self.list_concat(at, acc, list, copy),
            Site::DictMerge {
                at,
                acc,
                dst,
                entries,
                copy,
            } => self.dict_merge(at, acc, dst, entries, copy),
            Site::SetOr { at, acc, rhs, copy } => self.set_or(at, acc, rhs, copy),
            Site::ImmutableConcat { at, acc, rhs, copy } => {
                self.immutable_concat(at, acc, rhs, copy)
            }
            Site::AttrConcat { at, obj, attr, rhs } => self.attr_concat(at, obj, attr, rhs),
            Site::Membership {
                at,
                probe,
                container,
                kind,
            } => match kind {
                Membership::Plain => self.membership(at, probe, container),
                Membership::Rebuild {
                    at: call_at,
                    callee,
                    args,
                } => self.rebuild(at, call_at, container, callee, args),
                Membership::Dedup(b, i) => self.dedup(at, probe, container, (b, i)),
            },
            Site::FrontPop { at, q, form } => self.front(OptRule::Opt120, at, q, Some(form)),
            Site::FrontInsert { at, q } => self.front(OptRule::Opt121, at, q, None),
        }
    }

    /// Instructions whose span lies inside `span` rebind `name` or change `allocs`.
    fn eval_order(
        &self,
        subject: &str,
        span: Span,
        name: &str,
        st: &FrameState,
        acc: VarId,
    ) -> ProofObligation {
        let f = self.f();
        let allocs = st.allocs_of(acc);
        let mut status =
            established("evaluating the right-hand side neither rebinds nor changes it");
        for bb in &f.blocks {
            for (i, inst) in bb.insts.iter().enumerate() {
                if inst.span.start < span.start || inst.span.end > span.end {
                    continue;
                }
                if let Some(d) = inst.kind.dst()
                    && f.var_name(d) == Some(name)
                {
                    status = disproved(format!(
                        "the right-hand side rebinds `{name}` (line {})",
                        self.line(inst.span.start)
                    ));
                    break;
                }
                if self
                    .ck
                    .mutations
                    .get(&(bb.id, i))
                    .is_some_and(|m| m.iter().any(|a| allocs.contains(a)))
                {
                    status = disproved(format!(
                        "the right-hand side changes `{name}` before the original reads it (line {})",
                        self.line(inst.span.start)
                    ));
                    break;
                }
            }
        }
        self.ck.ob(K::EvalOrderPreserved, subject, status)
    }

    /// `[stmt_start, value_end)` of `name = <value>` with `Copy` at `copy`, when it is a plain
    /// single-target assignment statement.
    fn assignment_span(&self, copy: (BlockId, usize), value: Span) -> Option<Span> {
        let target = inst_at(self.f(), copy).span;
        let between = self.m.text(Span {
            start: target.end,
            end: value.start,
        })?;
        (is_plain_assign(between)
            && starts_statement(self.m.source, target.start)
            && ends_statement(self.m.source, value.end))
        .then_some(Span {
            start: target.start,
            end: value.end,
        })
    }

    // ------------------------------------------------------------------------ OPT101

    fn list_concat(
        &self,
        at: (BlockId, usize),
        acc: VarId,
        list: VarId,
        copy: (BlockId, usize),
    ) -> Option<OptCandidate> {
        let f = self.f();
        let st = self.state(at)?;
        let name = self.var(acc);
        let binop = inst_at(f, at);
        let dst = binop.kind.dst()?;
        let (_, _, list_inst) = self.cx().def_inst(list)?;
        let mut obs = self.base();
        obs.push(self.ck.exact_type(&name, &st, acc, &[List]));
        obs.push(
            self.ck
                .ob(K::ExactType, "[...]", established("list display")),
        );
        obs.push(self.ck.unique(&name, &st, acc));
        obs.push(self.ck.exclusive_access(&name, &st, acc, at, &[dst], true));
        obs.push(self.eval_order(&name, list_inst.span, &name, &st, acc));
        obs.push(
            self.ck
                .exception_state_safe(&name, &st, acc, at.0, &name, &[dst]),
        );
        obs.push(self.ck.ob(
            K::ExceptionStateSafe,
            "append",
            established("the elements are evaluated before the list changes; `append` / `extend` of a built list cannot fail"),
        ));
        let stmt = self.assignment_span(copy, binop.span);
        let span = stmt.unwrap_or(binop.span);
        let mut edits = Vec::new();
        if let Some(stmt) = stmt
            && let Some(lhs_text) = self.m.text(Span {
                start: binop.span.start,
                end: list_inst.span.start,
            })
            && squash(lhs_text) == format!("{name}+")
            && list_inst.span.end == binop.span.end
            && let Some(display) = self.m.text(list_inst.span)
        {
            let one_plain_element = matches!(&list_inst.kind,
                InstKind::AllocBuiltin { elems, .. } if matches!(elems.as_slice(), [e] if !f.is_unpacked(*e)));
            let inner = display
                .strip_prefix('[')
                .and_then(|d| d.strip_suffix(']'))
                .map(trim_element);
            let replacement = match inner {
                Some(e) if one_plain_element => format!("{name}.append({e})"),
                _ => format!("{name}.extend({display})"),
            };
            edits.push(TextEdit {
                span: stmt,
                replacement,
            });
        }
        Some(self.candidate(
            OptRule::Opt101,
            span,
            name.clone(),
            format!("`{name} = {name} + [...]` copies the whole list on every iteration (quadratic); append in place with `{name}.append(...)` / `{name}.extend(...)`"),
            obs,
            None,
            self.init_evidence(&st, acc, &[List]),
            edits,
        ))
    }

    // ------------------------------------------------------------------------ OPT102

    fn dict_merge(
        &self,
        at: (BlockId, usize),
        acc: VarId,
        dst: VarId,
        entries: &[DictEntry],
        copy: (BlockId, usize),
    ) -> Option<OptCandidate> {
        let f = self.f();
        let st = self.state(at)?;
        let name = self.var(acc);
        let lit = inst_at(f, at);
        let leading = matches!(entries.first(), Some(DictEntry::Unpack { dict }) if *dict == acc);
        let unpacked_again = entries
            .iter()
            .skip(1)
            .any(|e| matches!(e, DictEntry::Unpack { dict } if *dict == acc));
        let mut obs = self.base();
        obs.push(self.ck.exact_type(&name, &st, acc, &[Dict]));
        // After the display: its own entries may store the accumulator, or an iterator or view
        // of it, in the new dict (`{**d, k: d}`), which updating in place would make cyclic.
        let after = self.state((at.0, at.1 + 1))?;
        obs.push(self.ck.unique(&name, &after, acc));
        obs.push(self.ck.exclusive_access(&name, &st, acc, at, &[dst], true));
        obs.push(self.ck.ob(
            K::DictUnpackLeading,
            &name,
            if !leading {
                disproved(format!(
                    "`**{name}` is not the first entry: earlier entries keep their keys in the original, but `{name}.update(...)` would overwrite them"
                ))
            } else if unpacked_again {
                disproved(format!("`**{name}` is unpacked again later in the same display"))
            } else {
                established("the accumulator is the leading `**` entry")
            },
        ));
        obs.push(self.eval_order(&name, lit.span, &name, &st, acc));
        obs.push(
            self.ck
                .exception_state_safe(&name, &st, acc, at.0, &name, &[dst]),
        );
        let added = &entries[1..];
        obs.push(self.insertion_order(&st, &name, added));
        obs.push(self.partial_update(&name, at.0, matches!(added, [DictEntry::KeyValue { .. }])));
        let stmt = self.assignment_span(copy, lit.span);
        let span = stmt.unwrap_or(lit.span);
        let mut edits = Vec::new();
        if let (Some(stmt), true, false) = (stmt, leading, unpacked_again)
            && let Some(replacement) = self.dict_update_text(&st, &name, entries, lit.span)
        {
            edits.push(TextEdit {
                span: stmt,
                replacement,
            });
        }
        Some(self.candidate(
            OptRule::Opt102,
            span,
            name.clone(),
            format!("`{name} = {{**{name}, ...}}` copies the whole dict on every iteration (quadratic); update it in place"),
            obs,
            None,
            self.init_evidence(&st, acc, &[Dict]),
            edits,
        ))
    }

    /// `d[k] = v` for one simple `k: v` entry, otherwise `d.update({...rest...})`.
    fn dict_update_text(
        &self,
        st: &FrameState,
        name: &str,
        entries: &[DictEntry],
        lit: Span,
    ) -> Option<String> {
        let display = self.m.text(lit)?;
        let inner = display.strip_prefix('{')?.strip_suffix('}')?;
        let rest = strip_token(inner, "**")?;
        let rest = strip_token(rest, name)?;
        // Keep a multi-line display's layout; drop the space after `**d,` on one line. The
        // display has a second entry (`find_sites`), so `rest` is never empty.
        let rest = strip_token(rest, ",")?.trim_start_matches([' ', '\t']);
        let rest_trim = rest.trim();
        if let [_, DictEntry::KeyValue { key, value }] = entries
            && let (Some(k), Some(v)) = (self.simple_text(*key), self.simple_text(*value))
        {
            let squashed = squash(rest_trim);
            let expect = squash(&format!("{k}:{v}"));
            if squashed == expect || squashed == format!("{expect},") {
                return Some(format!("{name}[{k}] = {v}"));
            }
        }
        if let [_, DictEntry::Unpack { dict }] = entries
            && st.exact_type(*dict) == Some(Dict)
            && let Some(e) = rest.trim_start().strip_prefix("**")
        {
            return Some(format!("{name}.update({})", trim_element(e)));
        }
        Some(format!("{name}.update({{{rest}}})"))
    }

    /// `EvalOrderPreserved(update)`: the display inserts each entry into the copy before it
    /// evaluates the next one, and an insertion may run a key's `__eq__` (on a hash collision)
    /// or a mapping's `keys()` / `__getitem__`; `d.update({...})` evaluates every entry first,
    /// then inserts them, comparing the new keys with each other twice. That is the same only
    /// for one entry, for the entries of one exact dict, or for keys that are `int` / `str` /
    /// `bytes` / `bool` / `float` values whose later entries evaluate no code.
    fn insertion_order(&self, st: &FrameState, name: &str, added: &[DictEntry]) -> ProofObligation {
        let status = match added {
            [DictEntry::KeyValue { .. }] => {
                established("one entry: evaluated, then inserted, as in the display")
            }
            [DictEntry::Unpack { dict }] if st.exact_type(*dict) == Some(Dict) => {
                established("the entries of one exact dict are inserted in the same order")
            }
            _ if added.iter().enumerate().all(|(i, e)| match e {
                DictEntry::KeyValue { key, value } => {
                    self.cx()
                        .possible_kinds(st, *key)
                        .is_some_and(|m| m.is_subset_of(KindMask::hash_consistent()))
                        && (i == 0
                            || (self.evaluates_no_code(*key) && self.evaluates_no_code(*value)))
                }
                DictEntry::Unpack { .. } => false,
            }) =>
            {
                established("builtin keys, and the later entries are constants or local names")
            }
            _ => blocked(format!(
                "`{name}.update({{...}})` evaluates every entry before inserting any; the display inserts each entry (running key `__eq__` or a mapping's methods) before it evaluates the next"
            )),
        };
        self.ck.ob(K::EvalOrderPreserved, "update", status)
    }

    /// `ExceptionStateSafe(update)`: an insertion that raises (a key's `__eq__`) leaves the
    /// accumulator updated in place up to that key, where the display left it unchanged; only
    /// a handler that reads it can tell.
    fn partial_update(&self, name: &str, at: BlockId, one_insertion: bool) -> ProofObligation {
        let status = if one_insertion {
            established("one insertion: it happens entirely or not at all")
        } else if self.ck.handler_observed_names(at).contains(name) {
            blocked(format!(
                "an insertion that raises would leave `{name}` partly updated, and an exception handler reads `{name}`"
            ))
        } else {
            established("no exception handler reads the accumulator")
        };
        self.ck.ob(K::ExceptionStateSafe, "update", status)
    }

    /// Evaluating `v` runs no code and cannot raise: a constant, or a local name that is
    /// bound on every path and that no nested function can rebind.
    fn evaluates_no_code(&self, v: VarId) -> bool {
        match self.cx().def_inst(v) {
            Some((_, _, inst)) if matches!(inst.kind, InstKind::Const { .. }) => true,
            _ => self.bound_local_name(v).is_some() && !self.cx().is_captured(v),
        }
    }

    /// A display of one element or entry, without unpacking (`{e}`, `{k: v}`).
    fn single_item_display(&self, v: VarId) -> bool {
        match self.cx().def_inst(v).map(|(_, _, i)| &i.kind) {
            Some(InstKind::AllocBuiltin { elems, .. }) => {
                matches!(elems.as_slice(), [e] if !self.f().is_unpacked(*e))
            }
            Some(InstKind::DictLiteral { entries, .. }) => {
                matches!(entries.as_slice(), [DictEntry::KeyValue { .. }])
            }
            _ => false,
        }
    }

    /// Source text of a side-effect-free, non-raising operand (constant or bound local).
    fn simple_text(&self, v: VarId) -> Option<String> {
        match self.cx().def_inst(v) {
            Some((_, _, inst)) if matches!(inst.kind, InstKind::Const { .. }) => {
                self.m.text(inst.span).map(str::to_owned)
            }
            // Any other value: a local name bound on every path (`definitely_bound` is false
            // for a variable without a definition).
            _ => self.bound_local_name(v),
        }
    }

    fn bound_local_name(&self, v: VarId) -> Option<String> {
        let n = self.f().var_name(v)?;
        (is_identifier(n) && self.ck.definitely_bound(v)).then(|| n.to_owned())
    }

    fn set_or(
        &self,
        at: (BlockId, usize),
        acc: VarId,
        rhs: VarId,
        copy: (BlockId, usize),
    ) -> Option<OptCandidate> {
        let f = self.f();
        let st = self.state(at)?;
        let name = self.var(acc);
        let binop = inst_at(f, at);
        let dst = binop.kind.dst()?;
        let kind = st.exact_type(acc);
        if kind.is_some_and(|k| !matches!(k, Set | Dict)) {
            return None;
        }
        let mut obs = self.base();
        obs.push(self.ck.exact_type(&name, &st, acc, &[Set, Dict]));
        obs.push(
            self.ck
                .exact_type("right operand", &st, rhs, &[kind.unwrap_or(Set)]),
        );
        obs.push(self.ck.unique(&name, &st, acc));
        obs.push(self.ck.exclusive_access(&name, &st, acc, at, &[dst], true));
        obs.push(self.eval_order(&name, binop.span, &name, &st, acc));
        obs.push(
            self.ck
                .exception_state_safe(&name, &st, acc, at.0, &name, &[dst]),
        );
        obs.push(self.partial_update(&name, at.0, self.single_item_display(rhs)));
        let stmt = self.assignment_span(copy, binop.span);
        let span = stmt.unwrap_or(binop.span);
        let mut edits = Vec::new();
        if let Some(stmt) = stmt
            && let Some(text) = self.m.text(binop.span)
            && let Some(rest) = strip_token(text, &name)
            && let Some(rhs_text) = strip_token(rest, "|")
        {
            edits.push(TextEdit {
                span: stmt,
                replacement: format!("{name} |= {}", rhs_text.trim()),
            });
        }
        Some(self.candidate(
            OptRule::Opt102,
            span,
            name.clone(),
            format!("`{name} = {name} | ...` copies the whole container on every iteration (quadratic); update it in place with `{name} |= ...`"),
            obs,
            None,
            self.init_evidence(&st, acc, &[Set, Dict]),
            edits,
        ))
    }

    // ------------------------------------------------------------------------ OPT103

    /// Uses of `name`'s SSA versions inside the loop other than the accumulation itself.
    fn intermediate_read(
        &self,
        name: &str,
        lp: &NaturalLoop,
        allowed: &[(BlockId, usize)],
    ) -> Option<u32> {
        let f = self.f();
        for &b in &lp.body {
            for (i, inst) in f.block(b).insts.iter().enumerate() {
                if allowed.contains(&(b, i)) {
                    continue;
                }
                if inst
                    .kind
                    .operands()
                    .iter()
                    .any(|v| f.var_name(*v) == Some(name))
                {
                    return Some(inst.span.start);
                }
            }
            if let Some(v) = crate::interpreter::term_use(&f.block(b).terminator)
                && f.var_name(v) == Some(name)
            {
                return Some(crate::interpreter::term_span(f, b).start);
            }
        }
        None
    }

    fn immutable_concat(
        &self,
        at: (BlockId, usize),
        acc: VarId,
        rhs: VarId,
        copy: Option<(BlockId, usize)>,
    ) -> Option<OptCandidate> {
        let f = self.f();
        let st = self.state(at)?;
        let entry = self.init_kind(
            &st,
            acc,
            &[
                List,
                Dict,
                Set,
                Tuple,
                Str,
                Bytes,
                BuiltinKind::Int,
                BuiltinKind::Float,
                BuiltinKind::Bool,
            ],
        );
        let kind = st.exact_type(acc).or(entry);
        // Lists are `OPT101` (or already in place); numbers do not grow; CPython optimizes local
        // `str` concatenation in place.
        if kind.is_some_and(|k| !matches!(k, Bytes | Tuple)) {
            return None;
        }
        let lp = innermost_loop(f, at.0)?;
        let name = self.var(acc);
        let binop = inst_at(f, at);
        let dst = binop.kind.dst()?;
        let mut allowed = vec![at];
        allowed.extend(copy);
        let mut obs = self.base();
        obs.push(self.ck.exact_type(&name, &st, acc, &[Bytes, Tuple]));
        obs.push(
            self.ck
                .exact_type("right operand", &st, rhs, &[kind.unwrap_or(Bytes)]),
        );
        obs.push(self.ck.unique(&name, &st, acc));
        obs.push(self.ck.exclusive_access(&name, &st, acc, at, &[dst], true));
        obs.push(self.ck.ob(
            K::NoIntermediateReads,
            &name,
            match self.intermediate_read(&name, lp, &allowed) {
                Some(off) => blocked(format!("`{name}` is read inside the loop (line {}); a buffer would have to be joined there", self.line(off))),
                None => established("only the accumulation reads it inside the loop"),
            },
        ));
        let observed = self.ck.handler_observed_names(at.0);
        obs.push(self.ck.ob(
            K::ExceptionStateSafe,
            &name,
            if observed.contains(name.as_str()) {
                blocked(format!(
                    "an exception handler reads `{name}` before the buffer is joined"
                ))
            } else {
                established("no exception handler reads the accumulator")
            },
        ));
        let span = copy
            .and_then(|c| self.assignment_span(c, binop.span))
            .unwrap_or(binop.span);
        let what = kind.map_or("immutable value", BuiltinKind::name);
        // `while len(acc) < N` with a small constant `N`: no copy is longer than `N`.
        let capped = lp.len_caps.iter().find(|c| c.var == acc).map(|c| {
            format!(
                "the loop's test keeps `len({name})` below {} whatever the input",
                c.limit
            )
        });
        Some(self.candidate(
            OptRule::Opt103,
            span,
            name.clone(),
            format!("`{name}` grows by `{what}` concatenation on every iteration (quadratic); collect the pieces in a list and join them once after the loop"),
            obs,
            capped.or_else(|| loop_bound_note(lp)),
            self.init_evidence(&st, acc, &[Bytes, Tuple]),
            Vec::new(),
        ))
    }

    fn attr_concat(
        &self,
        at: (BlockId, usize),
        obj: VarId,
        attr: &str,
        rhs: VarId,
    ) -> Option<OptCandidate> {
        let f = self.f();
        let st = self.state(at)?;
        let lp = innermost_loop(f, at.0)?;
        let binop = inst_at(f, at);
        let target = format!("{}.{attr}", self.var(obj));
        let mut obs = self.base();
        // The attribute's own type is not tracked; the right operand shows what it holds.
        obs.push(self.ck.exact_type(&target, &st, rhs, &[Str, Bytes, Tuple]));
        obs.push(self.ck.ob(
            K::Unique,
            &target,
            blocked(format!(
                "`{target}` is reachable through `{}`: every reader would need the joined value",
                self.var(obj)
            )),
        ));
        // Only one object's attribute accumulates: the loop element, or an object created in
        // the iteration, gets one piece each.
        let fresh_receiver = (!self.ix.lives_across(lp, obj)).then(|| {
            format!(
                "`{}` is a different object in every iteration: no attribute grows across them",
                self.var(obj)
            )
        });
        Some(self.candidate(
            OptRule::Opt103,
            binop.span,
            target.clone(),
            format!("`{target} += ...` re-copies the attribute on every iteration (quadratic); accumulate in a local list and assign the joined result once"),
            obs,
            fresh_receiver.or_else(|| loop_bound_note(lp)),
            st.exact_type(rhs).filter(|k| matches!(k, Str | Bytes | Tuple)).map(|k| format!("the right operand is an exact `{}`", k.name())),
            Vec::new(),
        ))
    }

    // ------------------------------------------------------------------- hoisting helpers

    /// `SideEffectFree(set)` is blocked when the `set(...)` a hoisting edit writes would not be
    /// the builtin: rebound in the module, or a local name of this function or an enclosing one
    /// (`def f(items, set=...)`).
    fn set_not_builtin(&self) -> Option<ProofObligation> {
        let why = if !self.m.builtins.resolves("set") {
            "`set` is rebound in this module"
        } else if self.m.binds_locally(self.f(), "set") {
            "`set` is a local name of this function or an enclosing one"
        } else {
            return None;
        };
        Some(self.ck.ob(K::SideEffectFree, "set", blocked(why)))
    }

    /// The loop statement's iterable (`for p in <region>:`), whatever blocks it spans: it runs
    /// after a statement hoisted before the loop (`None`: a `while` loop, whose test is part of
    /// the loop).
    fn hoist_region(&self, lp: &NaturalLoop) -> Option<Span> {
        let first = self.f().block(lp.header).insts.first()?;
        let InstKind::IterNext { iter, .. } = first.kind else {
            return None;
        };
        self.cx()
            .def_inst(iter)
            .map(|(_, _, make_iter)| make_iter.span)
    }

    /// Line start and indentation of the `for` statement of `lp`. A comprehension's `for`
    /// clause is not a statement: a generator expression, a lambda, and (before 3.12) a
    /// comprehension are scopes named `<...>`, and an inlined comprehension's iterable is
    /// followed by `if`, `for`, or a closing bracket where a statement's header ends with `:`.
    fn for_stmt(&self, lp: &NaturalLoop) -> Option<(u32, String)> {
        if !is_identifier(&self.f().name) {
            return None;
        }
        let first = self.f().block(lp.header).insts.first()?;
        let InstKind::IterNext { iter, .. } = first.kind else {
            return None;
        };
        let (_, _, make_iter) = self.cx().def_inst(iter)?;
        if !header_colon_follows(self.m.source, make_iter.span.end) {
            return None;
        }
        let ls = line_start(self.m.source, first.span.start);
        let head = slice(self.m.source, ls, first.span.start)?;
        let words: Vec<&str> = head.split_whitespace().collect();
        if words != ["for"] && words != ["async", "for"] {
            return None;
        }
        Some((ls, indentation(self.m.source, ls).to_owned()))
    }

    /// Hoist `<name> = <expr>` before the loop and replace `replace` with `<name>`.
    /// The name is fresh (`base`, or `base_1`, ...): `None` when the loop is no `for`
    /// statement.
    fn hoist_edits(
        &self,
        lp: &NaturalLoop,
        base: &str,
        expr: &str,
        replace: Span,
    ) -> Option<(String, Vec<TextEdit>)> {
        let (ls, indent) = self.for_stmt(lp)?;
        let name = self.hoisted_name(base);
        let edits = vec![
            TextEdit {
                span: Span { start: ls, end: ls },
                replacement: format!("{indent}{name} = {expr}\n"),
            },
            TextEdit {
                span: replace,
                replacement: name.clone(),
            },
        ];
        Some((name, edits))
    }

    /// `e in <name>` with `<name>` as the last token of the compare: its span. A comparison
    /// sharing its span with another one is part of a chain (`i in a in b`): the last token
    /// may be another comparison's operand.
    fn container_name_span(&self, cmp: Span, name: &str) -> Option<Span> {
        let compares_here = self
            .f()
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter(|i| matches!(i.kind, InstKind::Compare { .. }) && i.span == cmp)
            .count();
        if compares_here != 1 {
            return None;
        }
        let text = self.m.text(cmp)?;
        let head = text.strip_suffix(name)?;
        let head = head.trim_end();
        (head.ends_with(" in") || head.ends_with(" not in") || head.ends_with(")in")).then_some(
            Span {
                start: cmp.end - name.len() as u32,
                end: cmp.end,
            },
        )
    }

    /// `v` is a small display for its whole life (`heki_ir::FnIndex::container_bound`): why
    /// a size-dependent rewrite does not pay off.
    fn small_note(&self, name: &str, v: VarId) -> Option<String> {
        self.ix.container_bound(v).map(|n| {
            format!("`{name}` holds at most {n} item(s): a small display that nothing grows")
        })
    }

    // ------------------------------------------------------------------------ OPT110

    fn membership(&self, at: (BlockId, usize), probe: VarId, lst: VarId) -> Option<OptCandidate> {
        let f = self.f();
        let st = self.state(at)?;
        if st
            .exact_type(lst)
            .is_some_and(|k| !matches!(k, List | Tuple))
        {
            // Sets / dicts already hash; `str` / `bytes` membership is a substring test.
            return None;
        }
        let lp = innermost_loop(f, at.0)?;
        let cmp = inst_at(f, at);
        let name = self.var(lst);
        let mut obs = self.base();
        obs.push(self.ck.exact_type(&name, &st, lst, &[List, Tuple]));
        obs.push(
            self.ck
                .value_loop_invariant(&name, &st, lst, lp, self.hoist_region(lp)),
        );
        obs.push(self.ck.zero_trip_safe(&name, &st, lst));
        obs.push(self.ck.hash_consistent_probe(&self.var(probe), &st, probe));
        obs.extend(self.set_not_builtin());
        // A set cannot hold dicts, lists, or sets: the advice has nothing to build.
        let unhashable = KindMask::of(List)
            .union(KindMask::of(Dict))
            .union(KindMask::of(Set));
        let not_profitable = self
            .small_note(&name, lst)
            .or_else(|| {
                self.cx()
                    .elems_of(&st, lst)
                    .filter(|m| *m != KindMask::EMPTY && m.is_subset_of(unhashable))
                    .map(|m| {
                        format!(
                            "its elements are unhashable ({}): no `set` can hold them",
                            crate::obligations::describe_mask(m)
                        )
                    })
            })
            .or_else(|| loop_bound_note(lp));
        let mut edits = Vec::new();
        if self.proven(&obs)
            && f.var_name(lst).is_some_and(is_identifier)
            && let Some(rspan) = self.container_name_span(cmp.span, &name)
            && let Some((_, e)) =
                self.hoist_edits(lp, &format!("{name}_set"), &format!("set({name})"), rspan)
        {
            edits = e;
        }
        Some(self.candidate(
            OptRule::Opt110,
            cmp.span,
            name.clone(),
            format!("`... in {name}` scans the list on every iteration; build `set({name})` once before the loop"),
            obs,
            not_profitable,
            self.exact_evidence(&st, lst, &[List, Tuple]),
            edits,
        ))
    }

    // ------------------------------------------------------------------------ OPT130

    fn rebuild(
        &self,
        at: (BlockId, usize),
        call_at: (BlockId, usize),
        rebuilt: VarId,
        callee: &str,
        args: &[VarId],
    ) -> Option<OptCandidate> {
        let f = self.f();
        let st = self.state(at)?;
        let lp = innermost_loop(f, at.0)?;
        let cmp = inst_at(f, at);
        let call = inst_at(f, call_at);
        let target = format!("{callee}(...)");
        let mut obs = self.base();
        obs.push(
            self.ck
                .exact_type(&target, &st, rebuilt, &[Set, List, Tuple, Dict]),
        );
        // Every instruction of the call expression is a pure builtin step in one block.
        let mut impure: Option<String> = None;
        let mut free: Vec<VarId> = Vec::new();
        let mut inside: FxHashSet<VarId> = FxHashSet::default();
        for bb in &f.blocks {
            for (i, inst) in bb.insts.iter().enumerate() {
                if inst.span.start < call.span.start || inst.span.end > call.span.end {
                    continue;
                }
                if bb.id != call_at.0 {
                    impure.get_or_insert_with(|| "the expression contains control flow".into());
                    continue;
                }
                if let Some(d) = inst.kind.dst() {
                    inside.insert(d);
                }
                let pre = self.state((bb.id, i));
                let outer = (bb.id, i) == call_at;
                if let Some(why) = pre.as_ref().and_then(|p| self.impure_step(p, inst, outer)) {
                    impure.get_or_insert(why);
                }
                free.extend(inst.kind.operands());
            }
        }
        free.retain(|v| !inside.contains(v));
        free.sort_unstable();
        free.dedup();
        obs.push(self.ck.ob(
            K::SideEffectFree,
            &target,
            match &impure {
                Some(why) => blocked(why.clone()),
                None => established("only builtin constructors and reads of loop-invariant values"),
            },
        ));
        let region = self.hoist_region(lp);
        for &v in &free {
            obs.push(
                self.ck
                    .value_loop_invariant(&self.var(v), &st, v, lp, region),
            );
        }
        // A hoisted container is one object shared by every iteration: no use but the test
        // may see it (`(tmp := list(keys))` then `tmp.clear()`).
        obs.push(self.ck.ob(
            K::ValueLoopInvariant,
            &target,
            match self.cx().uses(rebuilt) {
                [UseSite::Inst(b, i)] if (*b, *i) == at => {
                    established("only the membership test reads the built container")
                }
                _ => blocked(
                    "the built container is also bound or used: the hoisted one would be shared by every iteration",
                ),
            },
        ));
        let unpacked = call
            .kind
            .dst()
            .is_some_and(|d| (0..args.len()).any(|i| f.is_starred(d, i)));
        let raises = match (callee, args) {
            _ if unpacked => Some(
                "`*` passes the elements of the argument to the constructor: hoisting could raise or consume an iterator even when the loop runs zero times",
            ),
            ("list" | "tuple", []) | ("set" | "frozenset" | "dict", []) => None,
            // Iterating a builtin container runs no user code (a number is not iterable).
            ("list" | "tuple", [a])
                if matches!(
                    st.exact_type(*a),
                    Some(List | Tuple | Set | Dict | Str | Bytes)
                ) =>
            {
                None
            }
            // Building a set or dict from a dict compares keys with equal hashes: a key's own
            // `__eq__` would run (or raise) even when the loop runs zero times.
            ("set" | "frozenset" | "dict", [a]) if st.exact_type(*a) == Some(Dict) => Some(
                "its keys may run their own `__eq__` while the copy is built, even when the loop runs zero times",
            ),
            ("set" | "frozenset", [a]) if matches!(st.exact_type(*a), Some(Set | Str | Bytes)) => {
                None
            }
            ("set" | "frozenset", [a]) => match self.cx().elems_of(&st, *a) {
                Some(m) if m.is_subset_of(KindMask::hash_consistent()) => None,
                _ => Some(
                    "its elements may be unhashable: hoisting raises `TypeError` even when the loop runs zero times",
                ),
            },
            _ => Some(
                "the argument may not be a builtin container: hoisting could raise even when the loop runs zero times",
            ),
        };
        obs.push(self.ck.ob(
            K::ZeroTripExceptionSafe,
            &target,
            match raises {
                Some(why) => blocked(why),
                None => established("the constructor cannot raise for these arguments"),
            },
        ));
        // These constructors take at most one argument (more always raise, blocked above).
        let not_profitable = match (callee, args.first()) {
            ("tuple", Some(a)) if st.exact_type(*a) == Some(Tuple) => Some(format!(
                "`tuple({})` returns the tuple itself: nothing is rebuilt",
                self.var(*a)
            )),
            (_, Some(a)) => self.small_note(&self.var(*a), *a),
            (_, None) => Some("the container is empty".into()),
        }
        .or_else(|| loop_bound_note(lp));
        let mut edits = Vec::new();
        let base = match args {
            [a] => format!("{}_{callee}", self.var(*a)),
            _ => format!("hoisted_{callee}"),
        };
        if self.proven(&obs)
            && let Some(text) = self.m.text(call.span)
            && let Some((_, e)) = self.hoist_edits(lp, &base, text, call.span)
        {
            edits = e;
        }
        Some(self.candidate(
            OptRule::Opt130,
            cmp.span,
            target,
            format!("`{callee}(...)` rebuilds the same container on every iteration; build it once before the loop"),
            obs,
            not_profitable,
            Some(format!("`{callee}(...)` builds a new container on every iteration")),
            edits,
        ))
    }

    /// Why one instruction of a hoisted expression is not a pure builtin step (`outer`: it is
    /// the rebuilding constructor call itself).
    fn impure_step(&self, st: &FrameState, inst: &Inst, outer: bool) -> Option<String> {
        let cx = self.cx();
        match &inst.kind {
            InstKind::Const { .. } | InstKind::Copy { .. } => None,
            InstKind::LoadGlobal {
                name,
                is_lazy_import: false,
                ..
            } if self.m.builtins.resolves(name) => None,
            InstKind::Call { kwargs, .. } if outer && !kwargs.is_empty() => {
                Some("keyword arguments are not modelled".into())
            }
            InstKind::Call {
                callee_var, callee, ..
            } if outer => match cx
                .builtin_callee(*callee_var, callee)
                .and_then(|n| cx.kb.lookup(&format!("builtins.{n}")))
            {
                Some(e) if e.param_effects.iter().all(reads_only) => None,
                _ => Some(format!("`{callee}(...)` may have side effects")),
            },
            // Even a builtin may raise for its argument (`int("x")`, `range(0.5)`) or run user
            // code through it (`map(repr, objs)`), which hoisting would do although the loop
            // runs zero times.
            InstKind::Call { callee, .. } => Some(format!(
                "`{callee}(...)` inside it may raise or run user code before the loop"
            )),
            // A list or set method that only reads its receiver (`.copy()`, which takes no
            // argument); not a dict's (copying a dict may compare its keys, running their
            // `__eq__`, like `dict(d)`).
            InstKind::MethodCall {
                recv,
                method,
                args,
                kwargs,
                ..
            } if args.is_empty() && kwargs.is_empty() => match st
                .exact_type(*recv)
                .filter(|k| *k != Dict)
                .and_then(|k| cx.kb.lookup(&format!("builtins.{}.{method}", k.name())))
            {
                Some(e) if e.param_effects.iter().all(reads_only) => None,
                _ => Some(format!("`.{method}()` may have side effects")),
            },
            _ => Some(format!(
                "the expression evaluates more than builtin calls (line {})",
                self.line(inst.span.start)
            )),
        }
    }

    // ------------------------------------------------------------------------ OPT111

    fn dedup(
        &self,
        at: (BlockId, usize),
        probe: VarId,
        seen: VarId,
        append: (BlockId, usize),
    ) -> Option<OptCandidate> {
        let f = self.f();
        let st = self.state(at)?;
        if st.exact_type(seen).is_some_and(|k| k != List) {
            return None;
        }
        let lp = innermost_loop(f, at.0)?;
        let cmp = inst_at(f, at);
        let app = inst_at(f, append);
        let name = self.var(seen);
        let region = self.hoist_region(lp);
        let mut obs = self.base();
        obs.push(self.ck.exact_type(&name, &st, seen, &[List]));
        obs.push(self.ck.unique(&name, &st, seen));
        let rebound = self.ck.rebinding(seen, lp, region);
        let allocs = st.allocs_of(seen);
        let other = self.ck.first_mutation_in(&allocs, |b, i| {
            (b, i) != append && self.ck.in_loop_or(lp, region, b, i)
        });
        obs.push(self.ck.ob(
            K::OnlyPairedAppends,
            &name,
            if let Some(wh) = rebound {
                disproved(format!("`{name}` is rebound {wh}"))
            } else if let Some((_, _, s)) = other {
                disproved(format!(
                    "`{name}` is also changed at line {}: the auxiliary set would go stale",
                    self.line(s.start)
                ))
            } else {
                established("the paired append is the only change inside the loop")
            },
        ));
        let mut eaaw = self
            .ck
            .exclusive_access(&name, &st, seen, append, &[], false);
        if let ObligationStatus::Disproved { counterexample } = &eaaw.status {
            eaaw.status = blocked(format!(
                "{counterexample}; it could change the list behind the auxiliary set"
            ));
        }
        obs.push(eaaw);
        obs.push(self.ck.zero_trip_safe(&name, &st, seen));
        obs.push(self.ck.hash_consistent_probe(&self.var(probe), &st, probe));
        obs.extend(self.set_not_builtin());
        let mut edits = Vec::new();
        let probe_name = f.var_name(probe).filter(|n| is_identifier(n));
        if let (Some(pn), Some(rspan)) = (probe_name, self.container_name_span(cmp.span, &name))
            && self.proven(&obs)
            && f.var_name(seen).is_some_and(is_identifier)
            && self.cx().uses(app.kind.dst()?).is_empty()
            && ends_statement(self.m.source, app.span.end)
            && self
                .m
                .text(app.span)
                .is_some_and(|t| squash(t) == format!("{name}.append({pn})"))
            && let Some((h, mut e)) =
                self.hoist_edits(lp, &format!("{name}_set"), &format!("set({name})"), rspan)
        {
            e.push(TextEdit {
                span: Span {
                    start: app.span.end,
                    end: app.span.end,
                },
                replacement: format!("; {h}.add({pn})"),
            });
            edits = e;
        }
        Some(self.candidate(
            OptRule::Opt111,
            cmp.span,
            name.clone(),
            format!("`if ... not in {name}: {name}.append(...)` scans the list on every iteration; keep an auxiliary set next to it"),
            obs,
            loop_bound_note(lp),
            self.exact_evidence(&st, seen, &[List]),
            edits,
        ))
    }

    // ----------------------------------------------------------------- OPT120 / OPT121

    /// Every variable that may refer to `q`'s objects anywhere in the function.
    fn alias_closure(&self, st: &FrameState, q: VarId) -> FxHashSet<VarId> {
        let allocs = st.allocs_of(q);
        let mut set: FxHashSet<VarId> = FxHashSet::default();
        set.insert(q);
        for s in self
            .an()
            .block_in
            .iter()
            .flatten()
            .chain(std::iter::once(st))
        {
            for a in &allocs {
                set.extend(s.alias_sets.get(a).into_iter().flatten().copied());
            }
        }
        // Later copies, phis, and in-place updates propagate the reference.
        loop {
            let mut grew = false;
            for bb in &self.f().blocks {
                for phi in &bb.phis {
                    if phi.incoming.iter().any(|(_, v)| set.contains(v)) {
                        grew |= set.insert(phi.dst);
                    }
                }
                for inst in &bb.insts {
                    match &inst.kind {
                        InstKind::Copy { dst, src } if set.contains(src) => {
                            grew |= set.insert(*dst)
                        }
                        InstKind::BinOp {
                            dst,
                            lhs,
                            in_place: true,
                            ..
                        } if set.contains(lhs) => grew |= set.insert(*dst),
                        _ => {}
                    }
                }
            }
            if !grew {
                break;
            }
        }
        set
    }

    /// Why one use of an alias of `q` has no `deque` equivalent (`None`: it has one).
    fn non_deque_use(
        &self,
        u: VarId,
        site: UseSite,
        rewritten: (BlockId, usize),
    ) -> Option<String> {
        let f = self.f();
        let cx = self.cx();
        let idx_ok = |v: VarId| matches!(f.const_int(v), Some(0 | -1));
        match site {
            UseSite::Phi { .. } => None,
            UseSite::Term(b) => match &f.block(b).terminator {
                Terminator::Branch { .. } => None,
                _ => Some("it is returned or raised (callers would receive a `deque`)".into()),
            },
            UseSite::Inst(b, i) => {
                if (b, i) == rewritten {
                    return None;
                }
                let inst = &f.block(b).insts[i];
                let off = inst.span.start;
                match &inst.kind {
                    InstKind::Copy { .. } | InstKind::MakeIter { .. } => None,
                    InstKind::UnaryOp {
                        op: heki_ir::UnaryOpKind::Not,
                        ..
                    } => None,
                    InstKind::Compare {
                        op: CompareOpKind::In | CompareOpKind::NotIn,
                        rhs,
                        lhs,
                        ..
                    } if *rhs == u && *lhs != u => None,
                    InstKind::MethodCall {
                        recv,
                        method,
                        args,
                        kwargs,
                        ..
                    } if *recv == u && kwargs.is_empty() && !args.contains(&u) => {
                        // `deque.pop()` takes no index: another `q.pop(0)` stays a `TypeError`
                        // unless its own fix is applied, which overlaps this one's.
                        let ok = match (method.as_str(), args.as_slice()) {
                            ("append" | "extend" | "count" | "index" | "remove", [_, ..]) => true,
                            ("clear" | "reverse", []) => true,
                            ("pop", []) => true,
                            ("insert", [c, _]) => f.const_int(*c) == Some(0),
                            _ => false,
                        };
                        (!ok).then(|| {
                            format!(
                                "`.{method}(...)` has no `deque` equivalent here (line {})",
                                self.line(off)
                            )
                        })
                    }
                    InstKind::Call {
                        callee_var,
                        callee,
                        args,
                        kwargs,
                        ..
                    } if kwargs.is_empty() => {
                        let entry = cx
                            .builtin_callee(*callee_var, callee)
                            .and_then(|n| cx.kb.lookup(&format!("builtins.{n}")));
                        let ok = entry.is_some_and(|e| {
                            args.iter()
                                .enumerate()
                                .filter(|(_, a)| **a == u)
                                .all(|(k, _)| {
                                    e.param_effects
                                        .get(k)
                                        .or(e.param_effects.last())
                                        .is_some_and(reads_only)
                                })
                                && matches!(
                                    callee.as_str(),
                                    "len"
                                        | "list"
                                        | "tuple"
                                        | "set"
                                        | "iter"
                                        | "enumerate"
                                        | "reversed"
                                        | "bool"
                                )
                        });
                        (!ok).then(|| {
                            format!("it is passed to `{callee}(...)` (line {})", self.line(off))
                        })
                    }
                    InstKind::SubscriptLoad {
                        obj,
                        index,
                        is_slice: false,
                        ..
                    } if *obj == u && idx_ok(*index) => None,
                    InstKind::SubscriptStore {
                        obj, index, value, ..
                    } if *obj == u && *value != u && idx_ok(*index) => None,
                    InstKind::SubscriptDelete { obj, index, .. } if *obj == u && idx_ok(*index) => {
                        None
                    }
                    InstKind::BinOp {
                        op: BinOpKind::Add,
                        lhs,
                        rhs,
                        in_place: true,
                        ..
                    } if *lhs == u && *rhs != u => None,
                    _ => Some(format!(
                        "this use has no `deque` equivalent (line {})",
                        self.line(off)
                    )),
                }
            }
        }
    }

    /// A derived reference (a `for` loop's iterator, `iter(q)`, ...) of `allocs` that is still
    /// live after one of their changes: a deque's iterators raise `RuntimeError` once it
    /// changes, a list's keep going.
    fn iterated_while_changed(&self, allocs: &[crate::AllocId]) -> Option<String> {
        let f = self.f();
        let mut changes: Vec<(BlockId, usize)> = self
            .ck
            .mutations
            .iter()
            .filter(|(_, m)| m.iter().any(|a| allocs.contains(a)))
            .map(|(&at, _)| at)
            .collect();
        changes.sort_unstable();
        changes.into_iter().find_map(|(b, i)| {
            let st = self.state((b, i))?;
            let live = self.ck.live.live_after(f, b, i);
            let d = allocs.iter()
                .flat_map(|a| st.derived_refs.get(a).into_iter().flatten())
                .find(|d| live.contains(**d))?;
            Some(format!(
                "{} still iterates it when it changes at line {} (a `deque` raises `RuntimeError` there)",
                self.ck.name(*d),
                self.line(f.block(b).insts[i].span.start)
            ))
        })
    }

    /// `q` is a non-empty list at `at`: `at` is in the body of a `while q:` loop that tests this
    /// very `q`, and nothing before `at` in that block changes it.
    fn non_empty_at(&self, at: (BlockId, usize), q: VarId, allocs: &[crate::AllocId]) -> bool {
        let f = self.f();
        let tested = innermost_loop(f, at.0).is_some_and(|lp| {
            matches!(&f.block(lp.header).terminator,
                Terminator::Branch { cond, then_bb, .. } if *cond == q && *then_bb == at.0)
        });
        tested
            && !self
                .ck
                .mutations
                .iter()
                .any(|(&(b, i), m)| b == at.0 && i < at.1 && m.iter().any(|a| allocs.contains(a)))
    }

    fn front(
        &self,
        rule: OptRule,
        at: (BlockId, usize),
        q: VarId,
        form: Option<FrontForm>,
    ) -> Option<OptCandidate> {
        let f = self.f();
        let st = self.state(at)?;
        if st.exact_type(q).is_some_and(|k| k != List) {
            return None;
        }
        let lp = innermost_loop(f, at.0)?;
        let inst = inst_at(f, at);
        let name = self.var(q);
        let mut obs = self.base();
        obs.push(self.ck.exact_type(&name, &st, q, &[List]));
        obs.push(self.ck.unique(&name, &st, q));
        // Every allocation of `q` is a display / `list(...)` outside the loop (rewritten to a
        // deque) or the slice this candidate replaces.
        let allocs = st.allocs_of(q);
        let mut init_sites = Vec::new();
        let mut bad_alloc = None;
        for a in &allocs {
            let loc = self.cx().site_location(*a);
            let ai = inst_at(f, loc);
            let rewritten_slice = matches!(form, Some(FrontForm::SliceRebind { .. })) && loc == at;
            let init = !lp.body.contains(&loc.0)
                && match &ai.kind {
                    InstKind::AllocBuiltin { kind: List, .. } => true,
                    InstKind::Call {
                        callee_var, callee, ..
                    } => self.cx().builtin_callee(*callee_var, callee) == Some("list"),
                    _ => false,
                };
            if init {
                init_sites.push(loc);
            } else if !rewritten_slice {
                bad_alloc = Some(format!(
                    "it is also created at line {}",
                    self.line(ai.span.start)
                ));
            }
        }
        let aliases = self.alias_closure(&st, q);
        let mut bad_use = bad_alloc.clone();
        if bad_use.is_none() {
            'outer: for &u in &aliases {
                for &site in self.cx().uses(u) {
                    let rewritten_copy = match form {
                        Some(FrontForm::SliceRebind { copy }) => {
                            UseSite::Inst(copy.0, copy.1) == site
                        }
                        _ => false,
                    };
                    if rewritten_copy {
                        continue;
                    }
                    if let Some(why) = self.non_deque_use(u, site, at) {
                        bad_use = Some(format!("{} : {why}", self.ck.name(u)));
                        break 'outer;
                    }
                }
            }
        }
        if bad_use.is_none() {
            bad_use = self.iterated_while_changed(&allocs);
        }
        // A type checker must accept the rewritten variable: its `list` annotations become
        // `deque` ones, and one that cannot be rewritten blocks the fix.
        let mut type_edits = Vec::new();
        if bad_use.is_none() {
            match self.declared_type_edits(&init_sites, &aliases) {
                Ok(edits) => type_edits = edits,
                Err(why) => bad_use = Some(why),
            }
        }
        if bad_use.is_none()
            && matches!(form, Some(FrontForm::SliceRebind { .. }))
            && !self.non_empty_at(at, q, &allocs)
        {
            bad_use = Some(format!(
                "`{name}[1:]` of an empty list is `[]`, but `{name}.popleft()` raises `IndexError`: only `while {name}:` right before it proves `{name}` non-empty"
            ));
        }
        obs.push(self.ck.ob(
            K::DequeEquivalentOps,
            &name,
            match &bad_use {
                Some(why) => blocked(why.clone()),
                None if init_sites.is_empty() => {
                    blocked("no list display or `list(...)` to turn into a deque")
                }
                None => established("every use has a `collections.deque` equivalent"),
            },
        ));
        if let Some(FrontForm::SliceRebind { .. }) = form {
            let dst = inst.kind.dst()?;
            obs.push(self.ck.exclusive_access(&name, &st, q, at, &[dst], true));
            obs.push(
                self.ck
                    .exception_state_safe(&name, &st, q, at.0, &name, &[dst]),
            );
        }
        let edits = self
            .front_edits(at, &name, form, &init_sites, type_edits)
            .unwrap_or_default();
        let (msg, span) = match form {
            Some(FrontForm::SliceRebind { copy }) => (
                format!(
                    "`{name} = {name}[1:]` copies the list to drop one element (O(n) per iteration); use `collections.deque` and `{name}.popleft()`"
                ),
                self.assignment_span(copy, inst.span).unwrap_or(inst.span),
            ),
            Some(FrontForm::DelIndex) => (
                format!(
                    "`del {name}[0]` shifts every element (O(n) per iteration); use `collections.deque` and `{name}.popleft()`"
                ),
                inst.span,
            ),
            Some(FrontForm::PopCall) => (
                format!(
                    "`{name}.pop(0)` shifts every element (O(n) per iteration); use `collections.deque` and `{name}.popleft()`"
                ),
                inst.span,
            ),
            None => (
                format!(
                    "`{name}.insert(0, ...)` shifts every element (O(n) per iteration); use `collections.deque` and `{name}.appendleft(...)`"
                ),
                inst.span,
            ),
        };
        let evidence = self.exact_evidence(&st, q, &[List]);
        // A list rebuilt in every iteration pays for its front operations once per build; a
        // small display that the loop only drains never holds more than its items.
        let not_profitable = self
            .small_note(&name, q)
            .or_else(|| {
                (!self.ix.lives_across(lp, q))
                    .then(|| format!("`{name}` is rebuilt in every iteration of the loop"))
            })
            .or_else(|| loop_bound_note(lp));
        Some(self.candidate(
            rule,
            span,
            name.clone(),
            msg,
            obs,
            not_profitable,
            evidence,
            edits,
        ))
    }

    /// The edits turning the annotations of the deque's variables into `deque` ones: the
    /// annotation of an initializing assignment (`q: list[int] = []`) is rewritten; one that is
    /// not `list` / `list[...]` / `List[...]`, or another annotated declaration of a variable
    /// that may hold the deque (a parameter, a bare `q: list[int]`), blocks the fix.
    fn declared_type_edits(
        &self,
        init_sites: &[(BlockId, usize)],
        aliases: &FxHashSet<VarId>,
    ) -> Result<Vec<TextEdit>, String> {
        let f = self.f();
        let spelling = self
            .m
            .deque
            .as_ref()
            .map_or("collections.deque", |d| d.spelling.as_str());
        let mut edits = Vec::new();
        let mut handled: Vec<u32> = Vec::new();
        // Every init site is a display or a `list(...)` call (`front`): it defines a value.
        let inits = init_sites
            .iter()
            .filter_map(|&s| Some((inst_at(f, s), inst_at(f, s).kind.dst()?)));
        for (init, value) in inits {
            // The assignment binding it (`q: list[int] = []`), not a phi of a conditional.
            let binding = self
                .cx()
                .uses(value)
                .iter()
                .find_map(|u| match *u {
                    UseSite::Inst(b, i) => Some(&f.block(b).insts[i]),
                    _ => None,
                })
                .filter(|inst| {
                    matches!(&inst.kind, InstKind::Copy { dst, src }
                        if *src == value && f.var_name(*dst).is_some_and(is_identifier))
                });
            let Some(target) = binding.map(|inst| inst.span) else {
                continue;
            };
            let between = self
                .m
                .text(Span {
                    start: target.end,
                    end: init.span.start,
                })
                .unwrap_or_default();
            let trimmed = between.trim();
            let Some(inner) = trimmed.strip_prefix(':').and_then(|t| t.strip_suffix('=')) else {
                continue;
            };
            let ann = inner.trim();
            let ann_start = target.end + between.find(ann).unwrap_or(0) as u32;
            let Some(rewritten) = deque_annotation(ann, spelling) else {
                return Err(format!(
                    "its annotation `{ann}` (line {}) would not accept a `collections.deque`; rewrite it by hand",
                    self.line(ann_start)
                ));
            };
            handled.push(target.start);
            edits.push(TextEdit {
                span: Span {
                    start: ann_start,
                    end: ann_start + ann.len() as u32,
                },
                replacement: rewritten,
            });
        }
        let mut names: Vec<&str> = aliases
            .iter()
            .filter_map(|v| f.var_name(*v))
            .filter(|n| is_identifier(n))
            .collect();
        names.sort_unstable();
        names.dedup();
        let text = self.m.text(f.span).unwrap_or_default();
        for name in names {
            if let Some(off) = annotated_declarations(text, name)
                .map(|o| f.span.start + o)
                .find(|o| !handled.contains(o))
            {
                return Err(format!(
                    "`{name}` is declared with an annotation at line {}; rewrite it by hand",
                    self.line(off)
                ));
            }
        }
        Ok(edits)
    }

    fn front_edits(
        &self,
        at: (BlockId, usize),
        name: &str,
        form: Option<FrontForm>,
        init_sites: &[(BlockId, usize)],
        type_edits: Vec<TextEdit>,
    ) -> Option<Vec<TextEdit>> {
        let f = self.f();
        let deque = self.m.deque.as_ref()?;
        // The spelling must reach the module's import from inside this function.
        if self
            .m
            .binds_locally(f, deque.spelling.split('.').next().unwrap_or_default())
        {
            return None;
        }
        // A `list(...)` call is an init site only when `list` is the real builtin (`front`).
        let inst = inst_at(f, at);
        let mut edits = Vec::new();
        if let Some((span, text)) = &deque.import {
            edits.push(TextEdit {
                span: *span,
                replacement: text.clone(),
            });
        }
        for &s in init_sites {
            let ii = inst_at(f, s);
            let text = self.m.text(ii.span)?;
            let wrapped = match &ii.kind {
                InstKind::Call { args, .. } if args.is_empty() => format!("{}()", deque.spelling),
                InstKind::Call { .. } => {
                    let inner = text.trim_start().strip_prefix("list")?.trim_start();
                    format!("{}{inner}", deque.spelling)
                }
                _ => format!("{}({text})", deque.spelling),
            };
            edits.push(TextEdit {
                span: ii.span,
                replacement: wrapped,
            });
        }
        let text = self.m.text(inst.span)?;
        let site_edit = match form {
            Some(FrontForm::PopCall) => {
                (squash(text) == format!("{name}.pop(0)")).then(|| TextEdit {
                    span: inst.span,
                    replacement: format!("{name}.popleft()"),
                })?
            }
            Some(FrontForm::DelIndex) => {
                let ls = line_start(self.m.source, inst.span.start);
                let head = slice(self.m.source, ls, inst.span.start)?;
                let del_at = head.rfind("del")? as u32 + ls;
                let between = slice(self.m.source, del_at + 3, inst.span.start)?;
                (squash(text) == format!("{name}[0]")
                    && between.trim().is_empty()
                    && starts_statement(self.m.source, del_at)
                    && ends_statement(self.m.source, inst.span.end))
                .then(|| TextEdit {
                    span: Span {
                        start: del_at,
                        end: inst.span.end,
                    },
                    replacement: format!("{name}.popleft()"),
                })?
            }
            Some(FrontForm::SliceRebind { copy }) => {
                let stmt = self.assignment_span(copy, inst.span)?;
                (squash(text) == format!("{name}[1:]")).then(|| TextEdit {
                    span: stmt,
                    replacement: format!("{name}.popleft()"),
                })?
            }
            None => {
                let rest = squash(text);
                let prefix = format!("{name}.insert(0,");
                if !rest.starts_with(&prefix) {
                    return None;
                }
                let open = text.find('(')?;
                let after_paren = &text[open + 1..text.len().checked_sub(1)?];
                let value = strip_token(strip_token(after_paren, "0")?, ",")?;
                TextEdit {
                    span: inst.span,
                    replacement: format!("{name}.appendleft({})", trim_element(value)),
                }
            }
        };
        edits.push(site_edit);
        edits.extend(type_edits);
        Some(edits)
    }
}

/// `list[T]` / `list` / `List[T]` / `typing.List[T]` (also quoted) spelled with `spelling`
/// (`collections.deque[T]`); `None` for any other annotation (a union, `Sequence[T]`, ...).
fn deque_annotation(ann: &str, spelling: &str) -> Option<String> {
    let t = ann.trim();
    for q in ['"', '\''] {
        if let Some(inner) = t.strip_prefix(q).and_then(|i| i.strip_suffix(q)) {
            return deque_annotation(inner, spelling).map(|d| format!("{q}{d}{q}"));
        }
    }
    let (head, args) = match t.find('[') {
        Some(i) => (t[..i].trim_end(), Some(&t[i..])),
        None => (t, None),
    };
    let is_list = head == "list" || head == "List" || head.ends_with(".List");
    if !is_list {
        return None;
    }
    match args {
        None => Some(spelling.to_owned()),
        Some(a) if a.ends_with(']') && closes_at_end(a) => Some(format!("{spelling}{a}")),
        Some(_) => None,
    }
}

/// `[...]` whose first bracket closes at its last character.
fn closes_at_end(text: &str) -> bool {
    let mut depth = 0i32;
    let close = text.char_indices().find_map(|(i, c)| {
        depth += i32::from(c == '[') - i32::from(c == ']');
        (depth == 0).then_some(i + c.len_utf8())
    });
    close == Some(text.len())
}

/// Offsets (in `text`) of annotated declarations of `name`: `name:` (not `name :=`) where a
/// statement or a parameter can start: after indentation, `;`, `(`, `,`, or `*`.
fn annotated_declarations<'t>(text: &'t str, name: &'t str) -> impl Iterator<Item = u32> + 't {
    text.match_indices(name).filter_map(move |(i, _)| {
        let before = text[..i].trim_end_matches([' ', '\t']);
        let starts = before.is_empty() || before.ends_with(['\n', ';', '(', ',', '*']);
        let word_start = text[..i]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        let after = text[i + name.len()..].trim_start_matches([' ', '\t']);
        (starts && word_start && after.starts_with(':') && !after.starts_with(":="))
            .then_some(i as u32)
    })
}
