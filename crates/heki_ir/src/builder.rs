use crate::facts::{LoopHint, SMALL_MAX};
use crate::{
    A1Hazard, BasicBlock, BinOpKind, BlockId, BuiltinKind, CompareOpKind, ConstLiteral, Conversion,
    DictEntry, FunctionId, FunctionKind, Inst, InstKind, IrFunction, LocalAnnotation, LoopBound,
    LoopCarriedInfo, ModuleIr, NaturalLoop, ParamInfo, PhiNode, Terminator, UnaryOpKind, VarId,
};
use heki_ast::raw_ast::{self as ast, Expr, Pattern, Stmt};
use heki_ast::{ParsedModule, Ranged, Span};
use heki_env::RuntimeProfile;
use heki_semantic::{ClassId, SemanticModel};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::VecDeque;

/// The attribute a positional class-pattern argument reads (`case Point(x)`), named by the
/// class's `__match_args__`, which the IR does not resolve: its `AttrLoad` names the slot,
/// `__match_args__[i]`.
const MATCH_ARGS: &str = "__match_args__";

/// The units of work and memory a function's lowering may take (a block, an instruction, a
/// phi, a name's value in a block, a block a name's lookup passes): a multiple of its source
/// size, within bounds, far above what real code takes (under one unit per byte). A `finally`
/// body is lowered once per way out of its `try`, so copies of nested `finally` bodies
/// multiply; past the budget the function is left unmodeled ([`crate::UNMODELED`]) instead of
/// growing.
const UNITS_PER_SOURCE_BYTE: usize = 4;
const MIN_UNITS: usize = 4096;
const MAX_UNITS: usize = 50_000;

fn unit_budget(span: Span) -> usize {
    let source_bytes = span.end.saturating_sub(span.start) as usize;
    (source_bytes * UNITS_PER_SOURCE_BYTE + MIN_UNITS).min(MAX_UNITS)
}

/// Build SSA-CFG IR for every executable body in `parsed` (module top-level, class bodies,
/// functions/methods, lambdas, generator expressions, and non-inlined comprehensions).
pub fn build_module_ir(
    parsed: &ParsedModule,
    source: &str,
    semantic: &SemanticModel,
    profile: RuntimeProfile,
) -> ModuleIr {
    let mut class_by_span = FxHashMap::default();
    for ci in &semantic.classes {
        class_by_span.insert(ci.span, (ci.id, ci.qualname.clone()));
    }
    let mut lazy_import_names = FxHashSet::default();
    for imp in &semantic.imports {
        if imp.is_lazy {
            lazy_import_names.insert(imp.bound_name.clone());
        }
    }

    let mut introspection_imports = FxHashMap::default();
    let mut module_aliases = FxHashMap::default();
    let module_scope = semantic.scopes.first().map(|s| s.id);
    for imp in semantic
        .imports
        .iter()
        .filter(|i| Some(i.scope) == module_scope)
    {
        match (&imp.module, imp.is_from) {
            (Some(module), true) => {
                if let Some(kind) =
                    introspection_kind(&format!("{module}.{}", imp.imported_name), true)
                {
                    introspection_imports.insert(imp.bound_name.clone(), kind);
                }
            }
            (None, false) if imp.imported_name.split('.').next() != Some(&imp.bound_name) => {
                module_aliases.insert(imp.bound_name.clone(), imp.imported_name.clone());
            }
            _ => {}
        }
    }

    let mut ctx = ModuleLowerer {
        source,
        profile,
        future_annotations: semantic.has_future_annotations,
        introspection_imports,
        module_aliases,
        class_by_span,
        lazy_import_names,
        functions: Vec::new(),
        class_bases: Vec::new(),
        free_names: Vec::new(),
        lowered_at: FxHashMap::default(),
        loop_hints: Vec::new(),
        next_comp_id: 0,
        has_star_import: false,
        last_star_import: None,
    };

    let full_span = Span {
        start: 0,
        end: source.len() as u32,
    };
    let module_fn = ctx.lower_function_body(FunctionSpec {
        name: "<module>".to_string(),
        qualname: "<module>".to_string(),
        owner_class: None,
        owner_class_qualname: None,
        is_async: false,
        params: None,
        return_annotation: None,
        decorators: Vec::new(),
        span: full_span,
        is_module_body: true,
        is_class_body: false,
        outer_locals: FxHashSet::default(),
        walrus_target: WalrusTarget::OwnScope,
        walrus_globals: FxHashSet::default(),
        body: FunctionBody::Suite(parsed.suite()),
    });

    let mut functions = ctx.functions;
    record_module_introspection_aliases(&mut functions, module_fn, &ctx.introspection_imports);
    let constants = crate::facts::refine_loops(
        &mut functions,
        module_fn,
        &ctx.loop_hints,
        ctx.last_star_import,
    );
    for f in &mut functions {
        release_spare_capacity(f);
    }
    functions.shrink_to_fit();
    let mut class_bases = ctx.class_bases;
    // Nested functions finish before the functions holding them.
    class_bases.sort_unstable_by_key(|&(function, _, _)| function);
    class_bases.shrink_to_fit();
    ModuleIr {
        functions,
        module_fn,
        has_future_annotations: semantic.has_future_annotations,
        has_star_import: ctx.has_star_import,
        constants,
        class_bases,
    }
}

/// Module globals bound to a frame-introspection function (`snapshot = locals` at module
/// level): a function reading one may call it in its own frame (Assumption A1(a)). The names
/// an import binds (`imported`) were recorded where they are read.
fn record_module_introspection_aliases(
    functions: &mut [IrFunction],
    module_fn: FunctionId,
    imported: &FxHashMap<String, &'static str>,
) {
    let module = &functions[module_fn.0 as usize];
    let hazard_at: FxHashMap<Span, &str> = module
        .a1_hazards
        .iter()
        .map(|h| (h.span, h.kind.as_str()))
        .collect();
    let defs: FxHashMap<VarId, &Inst> = module
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .filter_map(|i| Some((i.kind.dst()?, i)))
        .collect();
    let mut aliases: FxHashMap<String, String> = FxHashMap::default();
    for inst in module.blocks.iter().flat_map(|b| &b.insts) {
        let InstKind::StoreGlobal { name, value } = &inst.kind else {
            continue;
        };
        if imported.contains_key(name) {
            continue;
        }
        let mut v = *value;
        while let Some(def) = defs.get(&v) {
            match &def.kind {
                InstKind::Copy { src, .. } => v = *src,
                InstKind::Import {
                    module: Some(m),
                    name: imported,
                    ..
                } => {
                    if let Some(kind) = introspection_kind(&format!("{m}.{imported}"), true) {
                        aliases.insert(name.clone(), kind.to_owned());
                    }
                    break;
                }
                _ => {
                    if let Some(kind) = hazard_at.get(&def.span) {
                        aliases.insert(name.clone(), (*kind).to_owned());
                    }
                    break;
                }
            }
        }
    }
    if aliases.is_empty() {
        return;
    }
    for f in functions.iter_mut().filter(|f| f.id != module_fn) {
        let reads: Vec<A1Hazard> = f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|i| match &i.kind {
                InstKind::LoadGlobal { name, .. } => aliases.get(name).map(|kind| A1Hazard {
                    kind: kind.clone(),
                    span: i.span,
                }),
                _ => None,
            })
            .collect();
        f.a1_hazards.extend(reads);
    }
}

/// Building by pushes leaves spare capacity in a function's containers, and a module's IR stays
/// resident until its file is checked.
fn release_spare_capacity(f: &mut IrFunction) {
    f.params.shrink_to_fit();
    f.decorators.shrink_to_fit();
    f.local_annotations.shrink_to_fit();
    f.var_names.shrink_to_fit();
    f.captured_vars.shrink_to_fit();
    f.starred.shrink_to_fit();
    f.str_heads.shrink_to_fit();
    for bb in &mut f.blocks {
        bb.insts.shrink_to_fit();
        bb.phis.shrink_to_fit();
        for phi in &mut bb.phis {
            phi.incoming.shrink_to_fit();
        }
    }
    f.blocks.shrink_to_fit();
    for lp in &mut f.loops {
        lp.body.shrink_to_fit();
        lp.exits.shrink_to_fit();
        lp.back_edges.shrink_to_fit();
        lp.len_caps.shrink_to_fit();
    }
    f.loops.shrink_to_fit();
    for lc in &mut f.loop_carried {
        lc.updates.shrink_to_fit();
    }
    f.loop_carried.shrink_to_fit();
    f.a1_hazards.shrink_to_fit();
}

enum FunctionBody<'a> {
    Suite(&'a [Stmt]),
    Expr(&'a Expr),
    GenExpr(&'a ast::ExprGenerator),
    Comp {
        generators: &'a [ast::Comprehension],
        elt: &'a Expr,
        key: Option<&'a Expr>,
        kind: CompKind,
    },
}

/// Accumulator of a list / set / dict comprehension (generator expressions yield instead).
#[derive(Clone, Copy)]
enum CompKind {
    List,
    Set,
    Dict,
}

impl CompKind {
    fn builtin(self) -> BuiltinKind {
        match self {
            Self::List => BuiltinKind::List,
            Self::Set => BuiltinKind::Set,
            Self::Dict => BuiltinKind::Dict,
        }
    }
}

struct FunctionSpec<'a> {
    name: String,
    qualname: String,
    owner_class: Option<ClassId>,
    owner_class_qualname: Option<String>,
    is_async: bool,
    params: Option<&'a ast::Parameters>,
    return_annotation: Option<String>,
    decorators: Vec<String>,
    span: Span,
    is_module_body: bool,
    is_class_body: bool,
    outer_locals: FxHashSet<String>,
    walrus_target: WalrusTarget,
    /// The names the function an assignment expression binds in declares `global`.
    walrus_globals: FxHashSet<String>,
    body: FunctionBody<'a>,
}

struct ModuleLowerer<'a> {
    source: &'a str,
    profile: RuntimeProfile,
    /// `from __future__ import annotations`: annotations stay strings.
    future_annotations: bool,
    /// Module globals bound by importing a frame-introspection function (`from sys import
    /// _getframe as frame`), with its `A1Hazard` kind.
    introspection_imports: FxHashMap<String, &'static str>,
    /// Modules the module body imports under another name (`import inspect as ins`): bound
    /// name -> module.
    module_aliases: FxHashMap<String, String>,
    class_by_span: FxHashMap<Span, (ClassId, String)>,
    lazy_import_names: FxHashSet<String>,
    functions: Vec<IrFunction>,
    /// The class bases of every lowered function (`ModuleIr::class_bases`).
    class_bases: Vec<(FunctionId, VarId, u32)>,
    /// The free variables of every lowered function, by `FunctionId` (sorted).
    free_names: Vec<Vec<String>>,
    /// The function lowered from each definition, by its span.
    lowered_at: FxHashMap<Span, FunctionId>,
    /// Syntax-level facts of every function's loops (in `IrFunction::loops` order), for the
    /// loop-bound pass that runs once the whole module is lowered.
    loop_hints: Vec<(FunctionId, Vec<LoopHint>)>,
    next_comp_id: u32,
    has_star_import: bool,
    /// The end of the last `from m import *` (module constants bound before it may be rebound).
    last_star_import: Option<u32>,
}

impl<'a> ModuleLowerer<'a> {
    /// `dotted` with an import alias as its first name replaced by the module it names.
    fn resolve_module_alias(&self, dotted: &str) -> String {
        let (head, rest) = dotted.split_once('.').unwrap_or((dotted, ""));
        match self.module_aliases.get(head) {
            Some(module) if !rest.is_empty() => format!("{module}.{rest}"),
            _ => dotted.to_owned(),
        }
    }

    fn slice_text(&self, span: Span) -> String {
        let start = span.start as usize;
        let end = span.end as usize;
        self.source
            .get(start..end)
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }

    fn alloc_function_slot(&mut self, placeholder: IrFunction) -> FunctionId {
        let id = FunctionId(self.functions.len() as u32);
        let mut f = placeholder;
        f.id = id;
        self.functions.push(f);
        self.free_names.push(Vec::new());
        id
    }

    fn lower_function_body(&mut self, spec: FunctionSpec<'a>) -> FunctionId {
        // A `finally` body is lowered once per way out of its `try`; a definition in it is
        // still one function.
        if let Some(&id) = self.lowered_at.get(&spec.span) {
            return id;
        }
        let mut scope_info = ScopeBindings::default();
        if let Some(params) = spec.params {
            collect_params_into(params, &mut scope_info.locals);
        }
        match spec.body {
            FunctionBody::Suite(stmts) => collect_suite_bindings(stmts, &mut scope_info),
            FunctionBody::Expr(expr) => collect_expr_bindings(expr, &mut scope_info),
            FunctionBody::GenExpr(ge) => {
                scope_info.has_yield = true;
                collect_comprehension_bindings(&ge.generators, &[&ge.elt], &mut scope_info);
            }
            FunctionBody::Comp {
                generators,
                elt,
                key,
                ..
            } => {
                let elts: Vec<&Expr> = key.into_iter().chain([elt]).collect();
                collect_comprehension_bindings(generators, &elts, &mut scope_info);
            }
        }
        // PEP 572: an assignment expression in a comprehension's own scope binds the name in
        // the nearest enclosing function (a cell) or module scope.
        let walrus_targets = std::mem::take(&mut scope_info.assignment_expression_targets);
        match spec.walrus_target {
            WalrusTarget::OwnScope => {}
            WalrusTarget::EnclosingFunction => {
                // ... unless that function declares the name `global`.
                for name in walrus_targets {
                    if spec.walrus_globals.contains(&name) {
                        scope_info.globals.insert(name);
                    } else {
                        scope_info.nonlocals.insert(name);
                    }
                }
            }
            WalrusTarget::Module => scope_info.globals.extend(walrus_targets),
        }
        // `global` and `nonlocal` remove names from true function locals.
        for g in &scope_info.globals {
            scope_info.locals.remove(g);
        }
        for nl in &scope_info.nonlocals {
            scope_info.locals.remove(nl);
        }

        let kind = match (spec.is_async, scope_info.has_yield) {
            (false, false) => FunctionKind::Sync,
            (true, false) => FunctionKind::Async,
            (false, true) => FunctionKind::Generator,
            (true, true) => FunctionKind::AsyncGenerator,
        };

        // Reserve slot up-front so nested functions get deterministic IDs after parent.
        let placeholder = IrFunction {
            id: FunctionId(0),
            name: spec.name.clone(),
            qualname: spec.qualname.clone(),
            scope_id: None,
            owner_class: spec.owner_class,
            owner_class_qualname: spec.owner_class_qualname.clone(),
            kind,
            params: Vec::new(),
            return_annotation: spec.return_annotation.clone(),
            decorators: spec.decorators.clone(),
            local_annotations: Vec::new(),
            var_names: Vec::new(),
            captured_vars: Vec::new(),
            starred: Vec::new(),
            str_heads: Vec::new(),
            blocks: Vec::new(),
            loops: Vec::new(),
            loop_carried: Vec::new(),
            a1_hazards: Vec::new(),
            span: spec.span,
            is_module_body: spec.is_module_body,
            is_class_body: spec.is_class_body,
            passed_from: None,
        };
        let fn_id = self.alloc_function_slot(placeholder);
        // The module body is no definition, and its span (the whole source) is also the span of
        // a definition or an expression that spans the whole file.
        if !spec.is_module_body {
            self.lowered_at.insert(spec.span, fn_id);
        }

        let mut builder = FnBuilder::new(
            self,
            fn_id,
            spec.name,
            spec.qualname,
            spec.owner_class,
            spec.owner_class_qualname,
            kind,
            spec.return_annotation,
            spec.decorators,
            spec.span,
            spec.is_module_body,
            spec.is_class_body,
            scope_info,
            &spec.outer_locals,
            spec.walrus_target,
            spec.walrus_globals,
        );

        if let Some(params) = spec.params {
            builder.bind_parameters(params);
        }

        match spec.body {
            FunctionBody::Suite(stmts) => {
                builder.lower_suite(stmts);
                builder.terminate(Terminator::Return(None));
            }
            FunctionBody::Expr(expr) => {
                let val = builder.lower_expr(expr);
                builder.terminate_at(Terminator::Return(Some(val)), Span::from(expr.range()));
            }
            FunctionBody::GenExpr(ge) => {
                let outer_param = builder.fresh_var(Some(".0".to_string()));
                builder.write_local(".0", builder.cur_bb, outer_param);
                builder.lower_comprehension_generators(
                    &ge.generators,
                    Some(outer_param),
                    false,
                    &|b| {
                        let dst = b.fresh_var(None);
                        // `(*xs for xs in rows)` yields from each `xs` (PEP 798).
                        let kind = match ge.elt.as_ref() {
                            Expr::Starred(st) => InstKind::YieldFrom {
                                dst,
                                value: b.lower_expr(&st.value),
                            },
                            elt => InstKind::Yield {
                                dst,
                                value: Some(b.lower_expr(elt)),
                            },
                        };
                        b.emit(kind, Span::from(ge.elt.range()));
                    },
                );
                builder.terminate(Terminator::Return(None));
            }
            FunctionBody::Comp {
                generators,
                elt,
                key,
                kind,
            } => {
                let outer_param = builder.fresh_var(Some(".0".to_string()));
                builder.write_local(".0", builder.cur_bb, outer_param);
                let acc = builder.fresh_var(Some(".comp_acc".to_string()));
                builder.emit(
                    InstKind::AllocBuiltin {
                        dst: acc,
                        kind: kind.builtin(),
                        elems: Vec::new(),
                    },
                    spec.span,
                );
                builder.lower_comprehension_generators(
                    generators,
                    Some(outer_param),
                    false,
                    &|b| {
                        b.emit_comp_element(kind, acc, elt, key);
                    },
                );
                builder.terminate_at(Terminator::Return(Some(acc)), spec.span);
            }
        }

        let (ir_fn, bases) = builder.finish();
        self.functions[fn_id.0 as usize] = ir_fn;
        self.class_bases.extend(
            bases
                .into_iter()
                .map(|(class, index)| (fn_id, class, index)),
        );
        fn_id
    }
}

#[derive(Default)]
struct ScopeBindings {
    locals: FxHashSet<String>,
    globals: FxHashSet<String>,
    nonlocals: FxHashSet<String>,
    /// Names bound by `:=`, which a comprehension's own scope does not keep.
    assignment_expression_targets: FxHashSet<String>,
    has_yield: bool,
}

/// The scope an assignment expression (`:=`) of a scope binds its name in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WalrusTarget {
    OwnScope,
    /// A comprehension scope nested (at any depth of comprehensions) in a function.
    EnclosingFunction,
    /// A comprehension scope nested in the module body.
    Module,
}

/// A statement a `return`, `break`, or `continue` leaves on its way out of the code around it.
#[derive(Clone)]
enum Unwind<'a> {
    /// A loop's body: `break` and `continue` stop here.
    Loop,
    /// A `with` statement's managers, exited innermost first.
    With {
        managers: Vec<VarId>,
        is_async: bool,
        span: Span,
        /// The exception targets outside the statement.
        exc_depth: usize,
    },
    /// A `try` statement's `finally` body, which runs on every way out.
    Finally { body: &'a [Stmt], exc_depth: usize },
}

/// A step of [`FnBuilder::resolve_ssa`].
enum SsaTask {
    /// Find the name's value at the end of the block.
    Read(BlockId),
    /// The value just found is also the name's value at the end of this block.
    Define(BlockId),
    /// The values at the ends of `preds` are found: complete the block's phi.
    Phi {
        bb: BlockId,
        phi_var: VarId,
        preds: Vec<BlockId>,
        /// Record the result as the name's value in `bb` (a phi created by a read).
        define: bool,
    },
}

struct ActiveLoop {
    header: BlockId,
    preheader: Option<BlockId>,
    exit: BlockId,
    depth: u16,
    bound: LoopBound,
    body_blocks: Vec<BlockId>,
    back_edges: Vec<BlockId>,
    hint: LoopHint,
}

struct FnBuilder<'m, 'a> {
    module: &'m mut ModuleLowerer<'a>,
    id: FunctionId,
    name: String,
    qualname: String,
    owner_class: Option<ClassId>,
    owner_class_qualname: Option<String>,
    kind: FunctionKind,
    params: Vec<ParamInfo>,
    return_annotation: Option<String>,
    decorators: Vec<String>,
    local_annotations: Vec<LocalAnnotation>,
    span: Span,
    is_module_body: bool,
    is_class_body: bool,

    scope: ScopeBindings,
    /// Union of outer function locals (to recognize closure captures).
    outer_locals: FxHashSet<String>,
    walrus_target: WalrusTarget,
    walrus_globals: FxHashSet<String>,
    /// Variables in THIS function captured by nested functions/lambdas/genexprs.
    captured_by_children: FxHashSet<String>,
    /// Free variables read by THIS function from outer scopes.
    free_reads: FxHashSet<String>,

    blocks: Vec<BasicBlock>,
    terminated: Vec<bool>,
    /// The block holds an instruction that may raise into its exception target.
    may_raise: Vec<bool>,
    preds: Vec<Vec<BlockId>>,
    sealed: Vec<bool>,
    /// A path from the entry reaches the block through the edges added so far. Lowering adds
    /// the edges entering a block before the ones leaving it, except a loop's back edges, which
    /// come from blocks reached only through its header: marking a block when an edge from a
    /// marked one enters it finds every block a path reaches.
    reachable: Vec<bool>,
    cur_bb: BlockId,

    var_names: Vec<Option<String>>,
    current_def: FxHashMap<(BlockId, String), VarId>,
    incomplete_phis: FxHashMap<BlockId, Vec<(String, VarId)>>,
    var_replacements: FxHashMap<VarId, VarId>,

    exc_target_stack: Vec<BlockId>,
    /// The statements around the current point that a jump out of them runs, innermost last.
    unwind: Vec<Unwind<'a>>,
    loop_stack: Vec<ActiveLoop>,
    finished_loops: Vec<NaturalLoop>,
    /// `finished_loops[i]`'s syntax-level facts.
    finished_hints: Vec<LoopHint>,
    a1_hazards: Vec<A1Hazard>,
    /// `*x` operands of displays and calls (`IrFunction::starred`).
    starred: Vec<(VarId, u32)>,
    /// The class bases of the function (`ModuleIr::class_bases`).
    class_bases: Vec<(VarId, u32)>,
    /// `IrFunction::str_heads`.
    str_heads: Vec<(VarId, String)>,
    /// Local names bound by importing a frame-introspection function, with its kind.
    introspection_locals: FxHashMap<String, &'static str>,

    /// Lexical isolation map for PEP 709 inlined comprehension targets: `raw_name -> isolated_ssa_name`.
    comp_target_renames: Vec<FxHashMap<String, String>>,

    /// IR units left of the function's budget ([`unit_budget`]).
    units_left: usize,
    /// The budget ran out: no further statement is lowered, and `finish` replaces the body.
    unmodeled: bool,
}

impl<'m, 'a> FnBuilder<'m, 'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        module: &'m mut ModuleLowerer<'a>,
        id: FunctionId,
        name: String,
        qualname: String,
        owner_class: Option<ClassId>,
        owner_class_qualname: Option<String>,
        kind: FunctionKind,
        return_annotation: Option<String>,
        decorators: Vec<String>,
        span: Span,
        is_module_body: bool,
        is_class_body: bool,
        scope: ScopeBindings,
        outer_locals: &FxHashSet<String>,
        walrus_target: WalrusTarget,
        walrus_globals: FxHashSet<String>,
    ) -> Self {
        let entry = BasicBlock {
            id: BlockId(0),
            phis: Vec::new(),
            insts: Vec::new(),
            terminator: Terminator::Unreachable,
            exc_target: None,
            term_span: None,
        };
        Self {
            module,
            id,
            name,
            qualname,
            owner_class,
            owner_class_qualname,
            kind,
            params: Vec::new(),
            return_annotation,
            decorators,
            local_annotations: Vec::new(),
            span,
            is_module_body,
            is_class_body,
            scope,
            outer_locals: outer_locals.clone(),
            walrus_target,
            walrus_globals,
            captured_by_children: FxHashSet::default(),
            free_reads: FxHashSet::default(),
            blocks: vec![entry],
            terminated: vec![false],
            may_raise: vec![false],
            preds: vec![Vec::new()],
            sealed: vec![true], // entry block has no predecessors
            reachable: vec![true],
            cur_bb: BlockId(0),
            var_names: Vec::new(),
            current_def: FxHashMap::default(),
            incomplete_phis: FxHashMap::default(),
            var_replacements: FxHashMap::default(),
            exc_target_stack: Vec::new(),
            unwind: Vec::new(),
            loop_stack: Vec::new(),
            finished_loops: Vec::new(),
            finished_hints: Vec::new(),
            a1_hazards: Vec::new(),
            starred: Vec::new(),
            class_bases: Vec::new(),
            str_heads: Vec::new(),
            introspection_locals: FxHashMap::default(),
            comp_target_renames: Vec::new(),
            units_left: unit_budget(span),
            unmodeled: false,
        }
    }

    /// Take one IR unit of the budget.
    fn spend_unit(&mut self) {
        match self.units_left.checked_sub(1) {
            Some(left) => self.units_left = left,
            None => self.unmodeled = true,
        }
    }

    fn fresh_var(&mut self, name: Option<String>) -> VarId {
        let id = VarId(self.var_names.len() as u32);
        self.var_names.push(name);
        id
    }

    fn new_block(&mut self, sealed: bool) -> BlockId {
        self.spend_unit();
        let id = BlockId(self.blocks.len() as u32);
        // A block gets its exception edge with its first instruction that may raise.
        self.blocks.push(BasicBlock {
            id,
            phis: Vec::new(),
            insts: Vec::new(),
            terminator: Terminator::Unreachable,
            exc_target: None,
            term_span: None,
        });
        self.terminated.push(false);
        self.may_raise.push(false);
        self.preds.push(Vec::new());
        self.sealed.push(sealed);
        self.reachable.push(false);
        // Any block allocated while inside active loops belongs to those loop bodies.
        for lp in &mut self.loop_stack {
            lp.body_blocks.push(id);
        }
        id
    }

    fn add_edge(&mut self, from: BlockId, to: BlockId) {
        let p = &mut self.preds[to.0 as usize];
        if !p.contains(&from) {
            p.push(from);
        }
        if self.reachable[from.0 as usize] {
            self.reachable[to.0 as usize] = true;
        }
    }

    fn is_terminated(&self, bb: BlockId) -> bool {
        self.terminated[bb.0 as usize]
    }

    /// No path reaches `bb`: all its predecessors are known, and there are none.
    fn is_dead(&self, bb: BlockId) -> bool {
        bb != BlockId(0) && self.sealed[bb.0 as usize] && self.preds[bb.0 as usize].is_empty()
    }

    fn switch_to(&mut self, bb: BlockId) {
        self.cur_bb = bb;
    }

    fn ensure_open_block(&mut self) {
        if self.is_terminated(self.cur_bb) {
            let dead = self.new_block(true);
            self.cur_bb = dead;
        }
    }

    fn emit(&mut self, kind: InstKind, span: Span) {
        self.spend_unit();
        self.ensure_open_block();
        let bb = self.cur_bb;
        if kind.may_raise() {
            self.raise_edge(bb);
        }
        self.blocks[bb.0 as usize].insts.push(Inst { kind, span });
    }

    /// `bb` may raise into the innermost exception target: the edge to it.
    fn raise_edge(&mut self, bb: BlockId) {
        if self.is_dead(bb) {
            return;
        }
        if let Some(exc) = self.exc_target_stack.last().copied() {
            self.blocks[bb.0 as usize].exc_target = Some(exc);
            self.add_edge(bb, exc);
            self.may_raise[bb.0 as usize] = true;
        }
    }

    /// Before a new binding of a source name inside a `try` or `with` body: an exception
    /// raised earlier in the block must reach the handler with the old binding, so the
    /// binding starts a new block (the handler's phi merges the binding of every block).
    fn begin_binding(&mut self) {
        let bb = self.cur_bb;
        if !self.exc_target_stack.is_empty()
            && self.may_raise[bb.0 as usize]
            && !self.is_terminated(bb)
        {
            let next = self.new_block(true);
            self.terminate(Terminator::Jump(next));
            self.switch_to(next);
        }
    }

    fn emit_const(&mut self, literal: ConstLiteral, span: Span) -> VarId {
        let dst = self.fresh_var(None);
        self.emit(InstKind::Const { dst, literal }, span);
        dst
    }

    /// Set the current block's terminator. A block keeps its first terminator: after `return`,
    /// `raise`, `break` or `continue` the fall-through edges of the enclosing construct are
    /// dropped here (the code that follows is lowered into a fresh dead block).
    fn terminate(&mut self, term: Terminator) {
        let bb = self.cur_bb;
        if self.terminated[bb.0 as usize] {
            return;
        }
        // Code no path reaches (after a `return`, or after a `try` every path leaves) flows
        // nowhere: its jumps would give live blocks a predecessor with unbound values.
        let term = if self.is_dead(bb) {
            Terminator::Unreachable
        } else {
            term
        };
        if matches!(term, Terminator::Raise(_)) {
            self.raise_edge(bb);
        }
        for succ in term.successors() {
            self.add_edge(bb, succ);
        }
        self.blocks[bb.0 as usize].terminator = term;
        self.terminated[bb.0 as usize] = true;
    }

    /// [`Self::terminate`] with the source span of the terminator's value: the `Branch`
    /// condition or the `return` / `raise` statement.
    fn terminate_at(&mut self, term: Terminator, span: Span) {
        let bb = self.cur_bb;
        if !self.terminated[bb.0 as usize] {
            self.blocks[bb.0 as usize].term_span = Some(span);
        }
        self.terminate(term);
    }

    /// Record the `*x` operands of the display or call defining `dst`.
    fn record_starred(&mut self, dst: VarId, operands: &[Expr]) {
        for (i, e) in operands.iter().enumerate() {
            if matches!(e, Expr::Starred(_)) {
                self.starred.push((dst, i as u32));
            }
        }
    }

    // -------------------------------------------------------------------------
    // Braun et al. (2013) SSA construction
    // -------------------------------------------------------------------------
    fn resolve_comp_rename(&self, raw: &str) -> Option<String> {
        for map in self.comp_target_renames.iter().rev() {
            if let Some(mapped) = map.get(raw) {
                return Some(mapped.clone());
            }
        }
        None
    }

    fn is_true_local(&self, name: &str) -> bool {
        if self.is_module_body || self.is_class_body {
            // In module/class bodies, treat locally assigned names as SSA locals too,
            // except explicit `global`/`nonlocal`.
            !self.scope.globals.contains(name)
                && !self.scope.nonlocals.contains(name)
                && self.scope.locals.contains(name)
        } else {
            self.scope.locals.contains(name)
        }
    }

    fn write_local(&mut self, name: &str, bb: BlockId, val: VarId) {
        if self
            .current_def
            .insert((bb, name.to_string()), val)
            .is_none()
        {
            self.spend_unit();
        }
    }

    fn write_var(&mut self, raw_name: &str, val: VarId, span: Span) {
        if let Some(renamed) = self.resolve_comp_rename(raw_name) {
            self.write_local(&renamed, self.cur_bb, val);
            return;
        }
        if self.scope.globals.contains(raw_name) {
            self.emit(
                InstKind::StoreGlobal {
                    name: raw_name.to_string(),
                    value: val,
                },
                span,
            );
            return;
        }
        if self.scope.nonlocals.contains(raw_name) {
            self.free_reads.insert(raw_name.to_string());
            self.emit(
                InstKind::StoreDeref {
                    name: raw_name.to_string(),
                    value: val,
                },
                span,
            );
            return;
        }
        self.begin_binding();
        if self.is_module_body {
            self.emit(
                InstKind::StoreGlobal {
                    name: raw_name.to_string(),
                    value: val,
                },
                span,
            );
        }
        // Create a named SSA copy if `val` currently has no name or a different name,
        // so `var_names[v]` stays faithful for diagnostics and `LoopCarried`.
        let named = if self.var_names[val.0 as usize].as_deref() == Some(raw_name) {
            val
        } else {
            let dst = self.fresh_var(Some(raw_name.to_string()));
            self.emit(InstKind::Copy { dst, src: val }, span);
            dst
        };
        self.write_local(raw_name, self.cur_bb, named);
    }

    fn read_var(&mut self, raw_name: &str, span: Span) -> VarId {
        if let Some(renamed) = self.resolve_comp_rename(raw_name) {
            return self.read_local_ssa(&renamed, self.cur_bb, span);
        }
        if self.scope.globals.contains(raw_name) {
            let dst = self.fresh_var(Some(raw_name.to_string()));
            let is_lazy = self.module.lazy_import_names.contains(raw_name);
            self.emit(
                InstKind::LoadGlobal {
                    dst,
                    name: raw_name.to_string(),
                    is_lazy_import: is_lazy,
                },
                span,
            );
            return dst;
        }
        if self.scope.nonlocals.contains(raw_name) {
            self.free_reads.insert(raw_name.to_string());
            let dst = self.fresh_var(Some(raw_name.to_string()));
            self.emit(
                InstKind::LoadDeref {
                    dst,
                    name: raw_name.to_string(),
                },
                span,
            );
            return dst;
        }
        if self.is_true_local(raw_name) {
            // A module-level read of a lazily imported name (PEP 810) may import the module
            // there, and raise: it loads the global.
            if self.is_module_body && self.module.lazy_import_names.contains(raw_name) {
                let dst = self.fresh_var(Some(raw_name.to_string()));
                self.emit(
                    InstKind::LoadGlobal {
                        dst,
                        name: raw_name.to_string(),
                        is_lazy_import: true,
                    },
                    span,
                );
                return dst;
            }
            return self.read_local_ssa(raw_name, self.cur_bb, span);
        }
        if self.outer_locals.contains(raw_name) {
            self.free_reads.insert(raw_name.to_string());
            let dst = self.fresh_var(Some(raw_name.to_string()));
            self.emit(
                InstKind::LoadDeref {
                    dst,
                    name: raw_name.to_string(),
                },
                span,
            );
            return dst;
        }
        let dst = self.fresh_var(Some(raw_name.to_string()));
        let is_lazy = self.module.lazy_import_names.contains(raw_name);
        self.emit(
            InstKind::LoadGlobal {
                dst,
                name: raw_name.to_string(),
                is_lazy_import: is_lazy,
            },
            span,
        );
        dst
    }

    fn read_local_ssa(&mut self, name: &str, bb: BlockId, span: Span) -> VarId {
        if let Some(&val) = self.current_def.get(&(bb, name.to_string())) {
            return self.canonical_var(val);
        }
        self.resolve_ssa(name, span, vec![SsaTask::Read(bb)])
    }

    /// Braun et al.'s recursive lookup of `name` through the predecessors, run on an explicit
    /// stack: a function of many sequential branches would otherwise recurse once per block.
    /// Every `Read` leaves one value on the value stack; the result is the last one.
    fn resolve_ssa(&mut self, name: &str, span: Span, mut tasks: Vec<SsaTask>) -> VarId {
        let mut values: Vec<VarId> = Vec::new();
        // Blocks with one predecessor the lookup has passed. One reached again is on a cycle,
        // and gets a phi like a join, which ends the walk: a cycle of such blocks alone has no
        // join, and no path enters it (dead code such as a loop after `return`).
        let mut passed: FxHashSet<BlockId> = FxHashSet::default();
        while let Some(task) = tasks.pop() {
            match task {
                SsaTask::Read(bb) => {
                    if let Some(&val) = self.current_def.get(&(bb, name.to_string())) {
                        values.push(self.canonical_var(val));
                        continue;
                    }
                    // Every block the walk passes takes a unit: past the budget the body is
                    // replaced, and any value ends the walk.
                    self.spend_unit();
                    if self.unmodeled {
                        return self.fresh_var(Some(name.to_string()));
                    }
                    if !self.sealed[bb.0 as usize] {
                        let phi_var = self.fresh_var(Some(name.to_string()));
                        self.incomplete_phis
                            .entry(bb)
                            .or_default()
                            .push((name.to_string(), phi_var));
                        self.write_local(name, bb, phi_var);
                        values.push(phi_var);
                    } else {
                        let preds = self.preds[bb.0 as usize].clone();
                        match preds.as_slice() {
                            [] => {
                                let dst = self.fresh_var(Some(name.to_string()));
                                self.blocks[bb.0 as usize].insts.push(Inst {
                                    kind: InstKind::LoadName {
                                        dst,
                                        name: name.to_string(),
                                    },
                                    span,
                                });
                                self.write_local(name, bb, dst);
                                values.push(dst);
                            }
                            [pred] if passed.insert(bb) => {
                                tasks.push(SsaTask::Define(bb));
                                tasks.push(SsaTask::Read(*pred));
                            }
                            _ => {
                                let phi_var = self.fresh_var(Some(name.to_string()));
                                // Break cycles before reading the predecessors.
                                self.write_local(name, bb, phi_var);
                                tasks.push(SsaTask::Phi {
                                    bb,
                                    phi_var,
                                    preds: preds.clone(),
                                    define: true,
                                });
                                tasks.extend(preds.iter().rev().map(|p| SsaTask::Read(*p)));
                            }
                        }
                    }
                }
                SsaTask::Define(bb) => {
                    let val = *values.last().expect("a read left its value");
                    self.write_local(name, bb, val);
                }
                SsaTask::Phi {
                    bb,
                    phi_var,
                    preds,
                    define,
                } => {
                    let operands = values.split_off(values.len() - preds.len());
                    let incoming: Vec<(BlockId, VarId)> = preds.into_iter().zip(operands).collect();
                    let val = self.finish_phi(bb, phi_var, incoming);
                    if define {
                        self.write_local(name, bb, val);
                    }
                    values.push(val);
                }
            }
        }
        values.pop().expect("the first read left its value")
    }

    /// Complete `phi_var` of `bb` from its operands: the one other value when every operand is
    /// that value or the phi itself (a trivial phi), else the phi.
    fn finish_phi(
        &mut self,
        bb: BlockId,
        phi_var: VarId,
        incoming: Vec<(BlockId, VarId)>,
    ) -> VarId {
        let mut same: Option<VarId> = None;
        let mut trivial = true;
        for &(_, raw_v) in &incoming {
            let v = self.canonical_var(raw_v);
            if v == phi_var || same == Some(v) {
                continue;
            }
            if same.is_none() {
                same = Some(v);
            } else {
                trivial = false;
                break;
            }
        }
        // In a block a path from the entry reaches, the one other value comes in on every path
        // and is defined before the block. A loop no path enters has only its back edges, whose
        // one other value can be computed from the phi (`d = d.x` in a loop after `return`):
        // taking it would define a value from itself, a cycle of definitions without a phi. The
        // phi stays there, with no value from outside.
        if trivial
            && let Some(s) = same
            && s != phi_var
            && self.reachable[bb.0 as usize]
        {
            self.var_replacements.insert(phi_var, s);
            return s;
        }

        self.spend_unit();
        self.blocks[bb.0 as usize].phis.push(PhiNode {
            dst: phi_var,
            incoming,
        });
        phi_var
    }

    fn seal_block(&mut self, bb: BlockId) {
        if self.sealed[bb.0 as usize] {
            return;
        }
        self.sealed[bb.0 as usize] = true;
        if let Some(pending) = self.incomplete_phis.remove(&bb) {
            let preds = self.preds[bb.0 as usize].clone();
            for (name, phi_var) in pending {
                let mut tasks = vec![SsaTask::Phi {
                    bb,
                    phi_var,
                    preds: preds.clone(),
                    define: false,
                }];
                tasks.extend(preds.iter().rev().map(|p| SsaTask::Read(*p)));
                self.resolve_ssa(&name, self.span, tasks);
            }
        }
    }

    fn canonical_var(&self, v: VarId) -> VarId {
        resolve_replacement(&self.var_replacements, v)
    }

    // -------------------------------------------------------------------------
    // Parameters & Statements
    // -------------------------------------------------------------------------
    fn bind_parameters(&mut self, params: &ast::Parameters) {
        let positional = params.posonlyargs.len() + params.args.len();
        for (i, p) in params
            .posonlyargs
            .iter()
            .chain(params.args.iter())
            .chain(params.kwonlyargs.iter())
            .enumerate()
        {
            let name = p.parameter.name.to_string();
            let var = self.fresh_var(Some(name.clone()));
            let annotation = p
                .parameter
                .annotation
                .as_ref()
                .map(|a| self.module.slice_text(Span::from(a.range())));
            self.write_local(&name, self.cur_bb, var);
            self.params.push(ParamInfo {
                name,
                var,
                has_default: p.default.is_some(),
                default: p.default.as_deref().and_then(literal_default),
                kw_only: i >= positional,
                is_vararg: false,
                is_kwarg: false,
                annotation,
            });
        }
        if let Some(v) = &params.vararg {
            let name = v.name.to_string();
            let var = self.fresh_var(Some(name.clone()));
            let annotation = v
                .annotation
                .as_ref()
                .map(|a| self.module.slice_text(Span::from(a.range())));
            self.write_local(&name, self.cur_bb, var);
            self.params.push(ParamInfo {
                name,
                var,
                has_default: false,
                default: None,
                kw_only: false,
                is_vararg: true,
                is_kwarg: false,
                annotation,
            });
        }
        if let Some(k) = &params.kwarg {
            let name = k.name.to_string();
            let var = self.fresh_var(Some(name.clone()));
            let annotation = k
                .annotation
                .as_ref()
                .map(|a| self.module.slice_text(Span::from(a.range())));
            self.write_local(&name, self.cur_bb, var);
            self.params.push(ParamInfo {
                name,
                var,
                has_default: false,
                default: None,
                kw_only: false,
                is_vararg: false,
                is_kwarg: true,
                annotation,
            });
        }
    }

    fn lower_suite(&mut self, stmts: &'a [Stmt]) {
        for stmt in stmts {
            self.lower_stmt(stmt);
        }
    }

    fn lower_stmt(&mut self, stmt: &'a Stmt) {
        if self.unmodeled {
            return;
        }
        self.ensure_open_block();
        match stmt {
            Stmt::FunctionDef(func) => self.lower_function_def(func),
            Stmt::ClassDef(cls) => self.lower_class_def(cls),
            Stmt::Return(r) => {
                let val = r.value.as_ref().map(|e| self.lower_expr(e));
                self.leave(false, |b| {
                    b.terminate_at(Terminator::Return(val), Span::from(r.range));
                });
            }
            Stmt::Delete(d) => {
                for target in &d.targets {
                    self.lower_delete_target(target);
                }
            }
            // The alias evaluates its value (and its type parameters' bounds) lazily, in a scope
            // that reads the names it uses from this frame: an unlowered function closing
            // over them.
            Stmt::TypeAlias(ta) => {
                let span = Span::from(ta.range);
                let mut names = self.capture_type_param_reads(ta.type_params.as_deref());
                names.extend(self.capture_free_names(names_loaded(&[&ta.value])));
                let captured = self.read_captured_vars(&names, span);
                let dst = self.fresh_var(None);
                self.emit(
                    InstKind::MakeClosure {
                        dst,
                        func_id: None,
                        captured,
                    },
                    span,
                );
                self.lower_assign_target(&ta.name, dst, span);
            }
            Stmt::Assign(a) => {
                let span = Span::from(a.range);
                let val = self.lower_expr(&a.value);
                for target in &a.targets {
                    self.lower_assign_target(target, val, span);
                }
            }
            Stmt::AugAssign(aa) => self.lower_aug_assign(aa),
            Stmt::AnnAssign(aa) => {
                let span = Span::from(aa.range);
                let ann_text = self.module.slice_text(Span::from(aa.annotation.range()));
                if let Some(target_text) = expr_dotted_name(&aa.target) {
                    self.local_annotations.push(LocalAnnotation {
                        target: target_text,
                        annotation: ann_text,
                        span,
                    });
                }
                if let Some(val_expr) = &aa.value {
                    let val = self.lower_expr(val_expr);
                    self.lower_assign_target(&aa.target, val, span);
                }
                // A function never evaluates its local variables' annotations; a module or
                // class body stores simple names' annotations in its `__annotations__`.
                if aa.simple && (self.is_module_body || self.is_class_body) {
                    let mut retained = Vec::new();
                    let lazily_read = self.lower_annotations(&[&aa.annotation], &mut retained);
                    self.capture_free_names(lazily_read);
                }
            }
            Stmt::For(f) => self.lower_for(f),
            Stmt::While(w) => self.lower_while(w),
            Stmt::If(if_stmt) => self.lower_if(if_stmt),
            Stmt::With(w) => self.lower_with(w),
            Stmt::Match(m) => self.lower_match(m),
            Stmt::Raise(r) => {
                let exc = r.exc.as_ref().map(|e| self.lower_expr(e));
                if let Some(cause) = &r.cause {
                    let _ = self.lower_expr(cause);
                }
                self.terminate_at(Terminator::Raise(exc), Span::from(r.range));
            }
            Stmt::Try(t) => self.lower_try(t),
            Stmt::Assert(a) => {
                let cond = self.lower_expr(&a.test);
                let ok_bb = self.new_block(true);
                let fail_bb = self.new_block(true);
                self.terminate_at(
                    Terminator::Branch {
                        cond,
                        then_bb: ok_bb,
                        else_bb: fail_bb,
                    },
                    Span::from(a.test.range()),
                );
                self.switch_to(fail_bb);
                let msg = a.msg.as_ref().map(|m| self.lower_expr(m));
                self.terminate_at(Terminator::Raise(msg), Span::from(a.range));
                self.switch_to(ok_bb);
            }
            Stmt::Import(imp) => {
                for alias in &imp.names {
                    let span = Span::from(alias.range);
                    let bound = alias
                        .asname
                        .as_ref()
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| alias.name.split('.').next().unwrap_or("").to_string());
                    let dst = self.fresh_var(Some(bound.clone()));
                    self.emit(
                        InstKind::Import {
                            dst,
                            module: None,
                            name: alias.name.to_string(),
                            is_lazy: imp.is_lazy,
                        },
                        span,
                    );
                    self.write_var(&bound, dst, span);
                }
            }
            Stmt::ImportFrom(imp) => {
                let module = imp.module.as_ref().map(|m| m.to_string());
                for alias in &imp.names {
                    if alias.name.as_str() == "*" {
                        self.module.has_star_import = true;
                        let end = Span::from(imp.range).end;
                        self.module.last_star_import =
                            Some(self.module.last_star_import.map_or(end, |e| e.max(end)));
                        continue;
                    }
                    let span = Span::from(alias.range);
                    let bound = alias
                        .asname
                        .as_ref()
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| alias.name.to_string());
                    if !self.is_module_body
                        && let Some(kind) = module
                            .as_ref()
                            .and_then(|m| introspection_kind(&format!("{m}.{}", alias.name), true))
                    {
                        self.introspection_locals.insert(bound.clone(), kind);
                    }
                    let dst = self.fresh_var(Some(bound.clone()));
                    self.emit(
                        InstKind::Import {
                            dst,
                            module: module.clone(),
                            name: alias.name.to_string(),
                            is_lazy: imp.is_lazy,
                        },
                        span,
                    );
                    self.write_var(&bound, dst, span);
                }
            }
            Stmt::Expr(e) => {
                let _ = self.lower_expr(&e.value);
            }
            Stmt::Break(_) => {
                if let Some(exit_bb) = self.loop_stack.last().map(|l| l.exit) {
                    self.leave(true, |b| b.terminate(Terminator::Jump(exit_bb)));
                }
            }
            Stmt::Continue(_) => {
                if let Some(header_bb) = self.loop_stack.last().map(|l| l.header) {
                    self.leave(true, |b| {
                        let cur = b.cur_bb;
                        if !b.is_dead(cur)
                            && let Some(top) = b.loop_stack.last_mut()
                        {
                            top.back_edges.push(cur);
                        }
                        b.terminate(Terminator::Jump(header_bb));
                    });
                }
            }
            Stmt::Global(_) | Stmt::Nonlocal(_) | Stmt::Pass(_) | Stmt::IpyEscapeCommand(_) => {}
        }
    }

    /// Jump out of the statements around the current point (to the innermost loop for
    /// `break` / `continue`, else out of all of them): exit each `with` and run each
    /// `finally` body on the way, innermost first, then `jump`. Each `finally` body is lowered
    /// again here, as CPython compiles it once per way out of its `try`.
    fn leave(&mut self, to_loop: bool, jump: impl FnOnce(&mut Self)) {
        let saved_exc = self.exc_target_stack.clone();
        let saved_unwind = self.unwind.clone();
        while let Some(frame) = self.unwind.pop() {
            match frame {
                Unwind::Loop if to_loop => break,
                Unwind::Loop => {}
                Unwind::With {
                    managers,
                    is_async,
                    span,
                    exc_depth,
                } => {
                    self.enter_unwind_block(exc_depth);
                    for mgr in managers.into_iter().rev() {
                        self.emit(InstKind::ExitWith { mgr, is_async }, span);
                    }
                }
                Unwind::Finally { body, exc_depth } => {
                    self.enter_unwind_block(exc_depth);
                    self.lower_suite(body);
                }
            }
        }
        jump(self);
        self.exc_target_stack = saved_exc;
        self.unwind = saved_unwind;
    }

    /// Continue in a new block whose exceptions go to the targets outside a statement being
    /// left.
    fn enter_unwind_block(&mut self, exc_depth: usize) {
        self.exc_target_stack.truncate(exc_depth);
        self.ensure_open_block();
        let next = self.new_block(true);
        self.terminate(Terminator::Jump(next));
        self.switch_to(next);
    }

    fn lower_function_def(&mut self, func: &'a ast::StmtFunctionDef) {
        let span = Span::from(func.range);
        let mut decorators = Vec::new();
        for dec in &func.decorator_list {
            decorators.push(self.module.slice_text(Span::from(dec.expression.range())));
            let _ = self.lower_expr(&dec.expression);
        }
        // The function object keeps its defaults (`__defaults__`, `__kwdefaults__`) and its
        // eagerly evaluated annotations (`__annotations__`).
        let mut retained: Vec<VarId> = parameter_defaults(&func.parameters)
            .map(|d| self.lower_expr(d))
            .collect();
        let annotations = function_annotations(&func.parameters, func.returns.as_deref());
        let lazily_read = self.lower_annotations(&annotations, &mut retained);

        let (owner_class, owner_class_qualname) = if self.is_class_body {
            (self.owner_class, self.owner_class_qualname.clone())
        } else {
            (None, None)
        };
        let qualname = if self.is_module_body {
            func.name.to_string()
        } else if self.is_class_body {
            format!("{}.{}", self.qualname, func.name)
        } else {
            format!("{}.<locals>.{}", self.qualname, func.name)
        };
        let return_annotation = func
            .returns
            .as_ref()
            .map(|r| self.module.slice_text(Span::from(r.range())));

        let child_outer = self.child_outer_locals();

        let child_id = self.module.lower_function_body(FunctionSpec {
            name: func.name.to_string(),
            qualname,
            owner_class,
            owner_class_qualname,
            is_async: func.is_async,
            params: Some(&func.parameters),
            return_annotation,
            decorators,
            span,
            is_module_body: false,
            is_class_body: false,
            outer_locals: child_outer,
            walrus_target: WalrusTarget::OwnScope,
            walrus_globals: FxHashSet::default(),
            body: FunctionBody::Suite(&func.body),
        });

        let mut captured_names = self.collect_child_captures(child_id);
        captured_names.extend(self.capture_free_names(lazily_read));
        captured_names.extend(self.capture_type_param_reads(func.type_params.as_deref()));
        let mut captured = self.read_captured_vars(&captured_names, span);
        captured.extend(retained);
        let dst = self.fresh_var(Some(func.name.to_string()));
        self.emit(
            InstKind::MakeClosure {
                dst,
                func_id: Some(child_id),
                captured,
            },
            span,
        );
        self.write_var(func.name.as_str(), dst, span);
    }

    /// Evaluate annotations the way the definition does: eagerly before 3.14 (their values
    /// pushed to `retained`, the dict that holds them), not at all under
    /// `from __future__ import annotations`, and from 3.14 in a lazily run `__annotate__`
    /// scope, whose free names are returned.
    fn lower_annotations(
        &mut self,
        annotations: &[&'a Expr],
        retained: &mut Vec<VarId>,
    ) -> Vec<String> {
        if self.module.future_annotations {
            Vec::new()
        } else if self.module.profile.pep649_deferred_annotations {
            names_loaded(annotations)
        } else {
            retained.extend(annotations.iter().map(|a| self.lower_expr(a)));
            Vec::new()
        }
    }

    /// The names the lazily evaluated scopes of PEP 695 type parameters read (bounds,
    /// constraints, defaults), captured like a nested function's free names.
    fn capture_type_param_reads(&mut self, params: Option<&'a ast::TypeParams>) -> Vec<String> {
        let exprs: Vec<&Expr> = params
            .into_iter()
            .flat_map(|tp| &tp.type_params)
            .flat_map(|p| match p {
                ast::TypeParam::TypeVar(tv) => vec![tv.bound.as_deref(), tv.default.as_deref()],
                ast::TypeParam::ParamSpec(ps) => vec![ps.default.as_deref()],
                ast::TypeParam::TypeVarTuple(tvt) => vec![tvt.default.as_deref()],
            })
            .flatten()
            .collect();
        self.capture_free_names(names_loaded(&exprs))
    }

    fn lower_class_def(&mut self, cls: &'a ast::StmtClassDef) {
        let span = Span::from(cls.range);
        let mut decorators = Vec::new();
        for dec in &cls.decorator_list {
            decorators.push(self.module.slice_text(Span::from(dec.expression.range())));
            let _ = self.lower_expr(&dec.expression);
        }
        // The class keeps its bases and hands its keywords to the metaclass and to
        // `__init_subclass__`.
        let mut retained = Vec::new();
        if let Some(args) = &cls.arguments {
            for arg in &args.args {
                retained.push(self.lower_expr(arg));
            }
            for kw in &args.keywords {
                retained.push(self.lower_expr(&kw.value));
            }
        }

        // `heki_semantic` records every `class` statement of the same parse, so its `ClassId`
        // and qualname are always there.
        let (class_id, class_qualname) = self.module.class_by_span[&span].clone();

        let child_outer = self.child_outer_locals();

        let child_id = self.module.lower_function_body(FunctionSpec {
            name: cls.name.to_string(),
            qualname: class_qualname.clone(),
            owner_class: Some(class_id),
            owner_class_qualname: Some(class_qualname),
            is_async: false,
            params: None,
            return_annotation: None,
            decorators,
            span,
            is_module_body: false,
            is_class_body: true,
            outer_locals: child_outer,
            walrus_target: WalrusTarget::OwnScope,
            walrus_globals: FxHashSet::default(),
            body: FunctionBody::Suite(&cls.body),
        });

        let mut captured_names = self.collect_child_captures(child_id);
        captured_names.extend(self.capture_type_param_reads(cls.type_params.as_deref()));
        let mut captured = self.read_captured_vars(&captured_names, span);
        let first_base = captured.len();
        captured.extend(retained);
        let dst = self.fresh_var(Some(cls.name.to_string()));
        if let Some(args) = &cls.arguments {
            for (i, arg) in args.args.iter().enumerate() {
                if !matches!(arg, Expr::Starred(_)) {
                    self.class_bases.push((dst, (first_base + i) as u32));
                }
            }
        }
        self.emit(
            InstKind::MakeClosure {
                dst,
                func_id: Some(child_id),
                captured,
            },
            span,
        );
        self.write_var(cls.name.as_str(), dst, span);
    }

    /// The names a scope nested here may read from enclosing function scopes: the enclosing
    /// functions' locals, this function's own, and the iteration variables of the inlined
    /// comprehensions being lowered (PEP 709 keeps those as cells of this frame).
    fn child_outer_locals(&self) -> FxHashSet<String> {
        let mut out = self.outer_locals.clone();
        if !self.is_module_body && !self.is_class_body {
            out.extend(self.scope.locals.iter().cloned());
        }
        for renames in &self.comp_target_renames {
            out.extend(renames.keys().cloned());
        }
        out
    }

    /// Where an assignment expression of a comprehension scope nested here binds its name.
    /// The names the function an assignment expression of a comprehension scope nested here
    /// binds in declares `global`.
    fn comprehension_walrus_globals(&self) -> FxHashSet<String> {
        match self.walrus_target {
            WalrusTarget::OwnScope => self.scope.globals.clone(),
            _ => self.walrus_globals.clone(),
        }
    }

    fn comprehension_walrus_target(&self) -> WalrusTarget {
        match self.walrus_target {
            WalrusTarget::OwnScope if self.is_module_body => WalrusTarget::Module,
            WalrusTarget::OwnScope => WalrusTarget::EnclosingFunction,
            nested => nested,
        }
    }

    /// [`Self::capture_free_names`] of the child's free variables (those it forwards to its own
    /// nested scopes included).
    fn collect_child_captures(&mut self, child_id: FunctionId) -> Vec<String> {
        let names = self.module.free_names[child_id.0 as usize].clone();
        self.capture_free_names(names)
    }

    /// The names a nested scope reads from enclosing function scopes that are cells of this
    /// frame, by their SSA names; the others are free here too.
    fn capture_free_names(&mut self, names: Vec<String>) -> Vec<String> {
        let mut own_captures = Vec::new();
        for name in names {
            if let Some(cell) = self.resolve_comp_rename(&name) {
                self.captured_by_children.insert(cell.clone());
                own_captures.push(cell);
            } else if self.is_module_body || self.is_class_body {
                // Module names are globals, and a class body's names are not visible to the
                // scopes nested in it.
                if self.outer_locals.contains(&name) {
                    self.free_reads.insert(name);
                }
            } else if self.scope.locals.contains(&name) {
                self.captured_by_children.insert(name.clone());
                own_captures.push(name);
            } else if self.outer_locals.contains(&name) {
                self.free_reads.insert(name);
            }
        }
        own_captures
    }

    fn read_captured_vars(&mut self, names: &[String], span: Span) -> Vec<VarId> {
        names
            .iter()
            .filter_map(|name| {
                if self.current_def.keys().any(|(_, n)| n == name) {
                    Some(self.read_local_ssa(name, self.cur_bb, span))
                } else {
                    None
                }
            })
            .collect()
    }

    fn lower_aug_assign(&mut self, aa: &'a ast::StmtAugAssign) {
        let span = Span::from(aa.range);
        let op = map_binop(aa.op);
        match aa.target.as_ref() {
            Expr::Name(n) => {
                let lhs = self.read_var(n.id.as_str(), Span::from(n.range));
                let rhs = self.lower_expr(&aa.value);
                let dst = self.fresh_var(Some(n.id.to_string()));
                self.emit(
                    InstKind::BinOp {
                        dst,
                        op,
                        lhs,
                        rhs,
                        in_place: true,
                    },
                    span,
                );
                self.write_var(n.id.as_str(), dst, span);
            }
            Expr::Attribute(attr) => {
                let obj = self.lower_expr(&attr.value);
                let attr_name = attr.attr.to_string();
                self.record_a1_attr_if_hazard(&attr_name, Span::from(attr.range));
                let lhs = self.fresh_var(None);
                self.emit(
                    InstKind::AttrLoad {
                        dst: lhs,
                        obj,
                        attr: attr_name.clone(),
                    },
                    Span::from(attr.range),
                );
                let rhs = self.lower_expr(&aa.value);
                let res = self.fresh_var(None);
                self.emit(
                    InstKind::BinOp {
                        dst: res,
                        op,
                        lhs,
                        rhs,
                        in_place: true,
                    },
                    span,
                );
                self.emit(
                    InstKind::AttrStore {
                        obj,
                        attr: attr_name,
                        value: res,
                    },
                    span,
                );
            }
            Expr::Subscript(sub) => {
                let obj = self.lower_expr(&sub.value);
                let (index, key_const, is_slice, slice_lower_const, slice_upper_const) =
                    self.lower_subscript_index(&sub.slice);
                let lhs = self.fresh_var(None);
                self.emit(
                    InstKind::SubscriptLoad {
                        dst: lhs,
                        obj,
                        index,
                        key_const: key_const.clone(),
                        is_slice,
                        slice_lower_const,
                        slice_upper_const,
                    },
                    Span::from(sub.range),
                );
                let rhs = self.lower_expr(&aa.value);
                let res = self.fresh_var(None);
                self.emit(
                    InstKind::BinOp {
                        dst: res,
                        op,
                        lhs,
                        rhs,
                        in_place: true,
                    },
                    span,
                );
                self.emit(
                    InstKind::SubscriptStore {
                        obj,
                        index,
                        key_const,
                        value: res,
                    },
                    span,
                );
            }
            // The parser rejects every other target (`InvalidAugmentedAssignmentTarget`).
            _ => {}
        }
    }

    fn lower_assign_target(&mut self, target: &'a Expr, value: VarId, span: Span) {
        match target {
            Expr::Name(n) => {
                self.write_var(n.id.as_str(), value, Span::from(n.range));
            }
            Expr::Attribute(attr) => {
                let obj = self.lower_expr(&attr.value);
                let attr_name = attr.attr.to_string();
                self.record_a1_attr_if_hazard(&attr_name, Span::from(attr.range));
                self.emit(
                    InstKind::AttrStore {
                        obj,
                        attr: attr_name,
                        value,
                    },
                    span,
                );
            }
            Expr::Subscript(sub) => {
                let obj = self.lower_expr(&sub.value);
                let (index, key_const, _, _, _) = self.lower_subscript_index(&sub.slice);
                self.emit(
                    InstKind::SubscriptStore {
                        obj,
                        index,
                        key_const,
                        value,
                    },
                    span,
                );
            }
            Expr::Tuple(ast::ExprTuple { elts, .. }) | Expr::List(ast::ExprList { elts, .. }) => {
                self.lower_unpacking(elts, value);
            }
            // The parser rejects every other target (`InvalidAssignmentTarget`).
            _ => {}
        }
    }

    /// `del name`: the name is possibly unbound until it is bound again.
    fn lower_name_delete(&mut self, name: &str, span: Span) {
        let prev = self
            .current_def
            .get(&(self.cur_bb, name.to_owned()))
            .copied();
        let global = self.is_module_body || self.scope.globals.contains(name);
        let nonlocal = self.scope.nonlocals.contains(name);
        if nonlocal {
            self.free_reads.insert(name.to_owned());
        }
        self.emit(
            InstKind::Delete {
                var: prev,
                name: name.to_owned(),
                global,
                nonlocal,
            },
            span,
        );
        // Reading it now raises (in a module or class body it falls back to the builtins).
        if !self.scope.globals.contains(name) && !self.scope.nonlocals.contains(name) {
            self.begin_binding();
            let unbound = self.fresh_var(Some(name.to_owned()));
            self.emit(
                InstKind::LoadName {
                    dst: unbound,
                    name: name.to_owned(),
                },
                span,
            );
            self.write_local(name, self.cur_bb, unbound);
        }
    }

    /// `a, *rest, b = value`: every item is read (and their number checked) before any target
    /// is written, so `xs[1], x = xs` binds `x` to the old `xs[1]`. `*rest` binds a new list;
    /// the items after it are counted from the end.
    fn lower_unpacking(&mut self, elts: &'a [Expr], value: VarId) {
        let star = elts.iter().position(|e| matches!(e, Expr::Starred(_)));
        let items: Vec<VarId> = elts
            .iter()
            .enumerate()
            .map(|(i, elt)| {
                let span = Span::from(elt.range());
                if matches!(elt, Expr::Starred(_)) {
                    return self.unpacked_list(value, span);
                }
                let index = match star {
                    Some(s) if i > s => -((elts.len() - i) as i64),
                    _ => i as i64,
                };
                // Index and item share the target's span: the unpacking of an assignment
                // (`heki_effect::consumption::is_unpacking`).
                let index_var = self.emit_const(ConstLiteral::Int(index), span);
                let item = self.fresh_var(None);
                self.emit(
                    InstKind::SubscriptLoad {
                        dst: item,
                        obj: value,
                        index: index_var,
                        key_const: None,
                        is_slice: false,
                        slice_lower_const: None,
                        slice_upper_const: None,
                    },
                    span,
                );
                item
            })
            .collect();
        for (elt, item) in elts.iter().zip(items) {
            let target = match elt {
                Expr::Starred(s) => &s.value,
                other => other,
            };
            self.lower_assign_target(target, item, Span::from(elt.range()));
        }
    }

    fn lower_delete_target(&mut self, target: &'a Expr) {
        let span = Span::from(target.range());
        match target {
            Expr::Name(n) => self.lower_name_delete(n.id.as_str(), span),
            Expr::Attribute(attr) => {
                let obj = self.lower_expr(&attr.value);
                let attr_name = attr.attr.to_string();
                self.record_a1_attr_if_hazard(&attr_name, Span::from(attr.range));
                self.emit(
                    InstKind::AttrDelete {
                        obj,
                        attr: attr_name,
                    },
                    span,
                );
            }
            Expr::Subscript(sub) => {
                let obj = self.lower_expr(&sub.value);
                let (index, key_const, _, _, _) = self.lower_subscript_index(&sub.slice);
                self.emit(
                    InstKind::SubscriptDelete {
                        obj,
                        index,
                        key_const,
                    },
                    span,
                );
            }
            Expr::Tuple(t) => {
                for e in &t.elts {
                    self.lower_delete_target(e);
                }
            }
            Expr::List(l) => {
                for e in &l.elts {
                    self.lower_delete_target(e);
                }
            }
            // The parser rejects every other target (`InvalidDeleteTarget`).
            _ => {}
        }
    }

    // -------------------------------------------------------------------------
    // Loops (`for` / `async for` / `while`) — CRITICAL `for.iter` separation
    // -------------------------------------------------------------------------
    fn lower_for(&mut self, f: &'a ast::StmtFor) {
        let iter_span = Span::from(f.iter.range());
        // INVARIANT (Stage 0-A): `for.iter` and `MakeIter` live in `preheader` (depth `d`),
        // STRICTLY OUTSIDE the loop body (depth `d + 1`).
        let iterable = self.lower_expr(&f.iter);
        let iter_var = self.fresh_var(None);
        self.emit(
            InstKind::MakeIter {
                dst: iter_var,
                iterable,
                is_async: f.is_async,
            },
            iter_span,
        );
        let preheader = self.cur_bb;

        let depth = (self.loop_stack.len() as u16) + 1;
        let (bound, retry_body) = classify_for_bound(f);

        // Allocate loop header (unsealed until back-edges are wired) and exit blocks.
        let header_bb = self.new_block(false);
        let exit_bb = self.new_block(false);
        let orelse_bb = if f.orelse.is_empty() {
            exit_bb
        } else {
            self.new_block(false)
        };

        self.terminate(Terminator::Jump(header_bb));

        // Push active loop AFTER allocating preheader, exit, and orelse so NONE of them
        // are accidentally marked as part of `loop.body`!
        self.loop_stack.push(ActiveLoop {
            header: header_bb,
            preheader: Some(preheader),
            exit: exit_bb,
            depth,
            bound,
            body_blocks: vec![header_bb],
            back_edges: Vec::new(),
            hint: LoopHint {
                retry_body,
                repeats: !reads_target(&f.body, &f.target),
                ..LoopHint::default()
            },
        });

        // Header block (depth d + 1): IterNext -> Branch(body, orelse/exit)
        self.switch_to(header_bb);
        let next_val = self.fresh_var(None);
        let target_span = Span::from(f.target.range());
        self.emit(
            InstKind::IterNext {
                dst: next_val,
                iter: iter_var,
                is_async: f.is_async,
            },
            target_span,
        );
        let body_bb = self.new_block(true);
        self.terminate_at(
            Terminator::Branch {
                cond: next_val,
                then_bb: body_bb,
                else_bb: orelse_bb,
            },
            target_span,
        );

        // Body (depth d + 1)
        self.switch_to(body_bb);
        self.lower_assign_target(&f.target, next_val, Span::from(f.target.range()));
        self.unwind.push(Unwind::Loop);
        self.lower_suite(&f.body);
        self.unwind.pop();
        if !self.is_terminated(self.cur_bb) {
            let tail = self.cur_bb;
            if !self.is_dead(tail)
                && let Some(top) = self.loop_stack.last_mut()
            {
                top.back_edges.push(tail);
            }
            self.terminate(Terminator::Jump(header_bb));
        }

        // Pop loop BEFORE lowering `orelse` (which runs at outer depth `d`).
        let finished = self.loop_stack.pop().unwrap();
        self.seal_block(header_bb);

        if !f.orelse.is_empty() {
            self.seal_block(orelse_bb);
            self.switch_to(orelse_bb);
            self.lower_suite(&f.orelse);
            self.terminate(Terminator::Jump(exit_bb));
        }

        self.seal_block(exit_bb);
        self.switch_to(exit_bb);
        self.finish_loop(finished);
    }

    /// Record a lowered loop (its natural loop and its syntax-level hints).
    fn finish_loop(&mut self, finished: ActiveLoop) {
        self.finished_loops.push(NaturalLoop {
            header: finished.header,
            preheader: finished.preheader,
            body: finished.body_blocks,
            exits: vec![finished.exit],
            back_edges: finished.back_edges,
            bound: finished.bound,
            depth: finished.depth,
            len_caps: Vec::new(),
        });
        self.finished_hints.push(finished.hint);
    }

    fn lower_while(&mut self, w: &'a ast::StmtWhile) {
        let preheader = self.cur_bb;
        let depth = (self.loop_stack.len() as u16) + 1;
        let bound = classify_while_bound(w);

        let header_bb = self.new_block(false);
        let exit_bb = self.new_block(false);
        let orelse_bb = if w.orelse.is_empty() {
            exit_bb
        } else {
            self.new_block(false)
        };

        self.terminate(Terminator::Jump(header_bb));

        self.loop_stack.push(ActiveLoop {
            header: header_bb,
            preheader: Some(preheader),
            exit: exit_bb,
            depth,
            bound,
            body_blocks: vec![header_bb],
            back_edges: Vec::new(),
            hint: LoopHint {
                cap_tests: len_cap_tests(&w.test),
                repeats: true,
                forever: matches!(w.test.as_ref(), Expr::BooleanLiteral(b) if b.value),
                ..LoopHint::default()
            },
        });

        self.switch_to(header_bb);
        let cond = self.lower_expr(&w.test);
        let body_bb = self.new_block(true);
        self.terminate_at(
            Terminator::Branch {
                cond,
                then_bb: body_bb,
                else_bb: orelse_bb,
            },
            Span::from(w.test.range()),
        );

        self.switch_to(body_bb);
        self.unwind.push(Unwind::Loop);
        self.lower_suite(&w.body);
        self.unwind.pop();
        if !self.is_terminated(self.cur_bb) {
            let tail = self.cur_bb;
            if !self.is_dead(tail)
                && let Some(top) = self.loop_stack.last_mut()
            {
                top.back_edges.push(tail);
            }
            self.terminate(Terminator::Jump(header_bb));
        }

        let finished = self.loop_stack.pop().unwrap();
        self.seal_block(header_bb);

        if !w.orelse.is_empty() {
            self.seal_block(orelse_bb);
            self.switch_to(orelse_bb);
            self.lower_suite(&w.orelse);
            self.terminate(Terminator::Jump(exit_bb));
        }

        self.seal_block(exit_bb);
        self.switch_to(exit_bb);
        self.finish_loop(finished);
    }

    // -------------------------------------------------------------------------
    // Control Flow (`if`, `with`, `try`, `match`)
    // -------------------------------------------------------------------------
    fn lower_if(&mut self, if_stmt: &'a ast::StmtIf) {
        let merge_bb = self.new_block(false);

        let cond = self.lower_expr(&if_stmt.test);
        let then_bb = self.new_block(true);
        let mut next_else_bb = if if_stmt.elif_else_clauses.is_empty() {
            merge_bb
        } else {
            self.new_block(true)
        };
        self.terminate_at(
            Terminator::Branch {
                cond,
                then_bb,
                else_bb: next_else_bb,
            },
            Span::from(if_stmt.test.range()),
        );

        self.switch_to(then_bb);
        self.lower_suite(&if_stmt.body);
        self.terminate(Terminator::Jump(merge_bb));

        for (idx, clause) in if_stmt.elif_else_clauses.iter().enumerate() {
            let is_last = idx + 1 == if_stmt.elif_else_clauses.len();
            self.switch_to(next_else_bb);
            if let Some(test) = &clause.test {
                let c = self.lower_expr(test);
                let c_then = self.new_block(true);
                let c_else = if is_last {
                    merge_bb
                } else {
                    self.new_block(true)
                };
                self.terminate_at(
                    Terminator::Branch {
                        cond: c,
                        then_bb: c_then,
                        else_bb: c_else,
                    },
                    Span::from(test.range()),
                );
                self.switch_to(c_then);
                self.lower_suite(&clause.body);
                self.terminate(Terminator::Jump(merge_bb));
                next_else_bb = c_else;
            } else {
                self.lower_suite(&clause.body);
                self.terminate(Terminator::Jump(merge_bb));
            }
        }

        self.seal_block(merge_bb);
        self.switch_to(merge_bb);
    }

    fn lower_with(&mut self, w: &'a ast::StmtWith) {
        let span = Span::from(w.range);
        let mut entered = Vec::with_capacity(w.items.len());
        for item in &w.items {
            let item_span = Span::from(item.context_expr.range());
            let mgr = self.lower_expr(&item.context_expr);
            let res = self.fresh_var(None);
            self.emit(
                InstKind::EnterWith {
                    dst: res,
                    mgr,
                    is_async: w.is_async,
                },
                item_span,
            );
            if let Some(opt) = &item.optional_vars {
                self.lower_assign_target(opt, res, Span::from(opt.range()));
            }
            entered.push(mgr);
        }

        let exit_bb = self.new_block(false);
        self.unwind.push(Unwind::With {
            managers: entered.clone(),
            is_async: w.is_async,
            span,
            exc_depth: self.exc_target_stack.len(),
        });
        self.exc_target_stack.push(exit_bb);
        let body_bb = self.new_block(true);
        self.terminate(Terminator::Jump(body_bb));

        self.switch_to(body_bb);
        self.lower_suite(&w.body);
        self.terminate(Terminator::Jump(exit_bb));

        self.exc_target_stack.pop();
        self.unwind.pop();
        self.seal_block(exit_bb);
        self.switch_to(exit_bb);
        for mgr in entered.into_iter().rev() {
            self.emit(
                InstKind::ExitWith {
                    mgr,
                    is_async: w.is_async,
                },
                span,
            );
        }
    }

    fn lower_try(&mut self, t: &'a ast::StmtTry) {
        let has_handlers = !t.handlers.is_empty();
        let has_finally = !t.finalbody.is_empty();
        // The `finally` body runs on every way out of the `try`: after normal completion
        // (`normal_finally_bb`), before an exception goes on (`raising_finally_bb`, which
        // re-raises), and before a `return` / `break` / `continue` (inlined by `leave`).
        let raising_finally_bb = has_finally.then(|| self.new_block(false));
        let normal_finally_bb = has_finally.then(|| self.new_block(false));
        let dispatch_bb = has_handlers.then(|| self.new_block(false));
        let after_bb = self.new_block(false);
        let normal_next = normal_finally_bb.unwrap_or(after_bb);
        if has_finally {
            self.unwind.push(Unwind::Finally {
                body: &t.finalbody,
                exc_depth: self.exc_target_stack.len(),
            });
        }

        let body_exc_target = dispatch_bb.or(raising_finally_bb);
        if let Some(target) = body_exc_target {
            self.exc_target_stack.push(target);
        }
        let try_bb = self.new_block(true);
        self.terminate(Terminator::Jump(try_bb));
        self.switch_to(try_bb);
        self.lower_suite(&t.body);
        if body_exc_target.is_some() {
            self.exc_target_stack.pop();
        }

        if !self.is_terminated(self.cur_bb) {
            if !t.orelse.is_empty() {
                if let Some(fin) = raising_finally_bb {
                    self.exc_target_stack.push(fin);
                }
                let orelse_bb = self.new_block(true);
                self.terminate(Terminator::Jump(orelse_bb));
                self.switch_to(orelse_bb);
                self.lower_suite(&t.orelse);
                if raising_finally_bb.is_some() {
                    self.exc_target_stack.pop();
                }
            }
            self.terminate(Terminator::Jump(normal_next));
        }

        if let Some(disp) = dispatch_bb {
            self.seal_block(disp);
            if let Some(fin) = raising_finally_bb {
                self.exc_target_stack.push(fin);
            }
            self.lower_handlers(t, disp, normal_next);
            if raising_finally_bb.is_some() {
                self.exc_target_stack.pop();
            }
        }
        if has_finally {
            self.unwind.pop();
        }

        if let Some(fin) = raising_finally_bb {
            self.seal_block(fin);
            self.switch_to(fin);
            self.lower_suite(&t.finalbody);
            self.terminate(Terminator::Raise(None));
        }
        if let Some(fin) = normal_finally_bb {
            self.seal_block(fin);
            self.switch_to(fin);
            self.lower_suite(&t.finalbody);
            self.terminate(Terminator::Jump(after_bb));
        }

        self.seal_block(after_bb);
        self.switch_to(after_bb);
    }

    /// The `except` clauses from the dispatch block `disp`: each tests whether it matches
    /// (an opaque test, except a bare `except:`, which always does); a handler that
    /// completes continues at `normal_next`. Every `except*` clause may run on part of the
    /// exception group, so each continues to the next clause's test. An exception no clause
    /// takes (all or, for `except*`, part of the group) is raised again.
    fn lower_handlers(&mut self, t: &'a ast::StmtTry, disp: BlockId, normal_next: BlockId) {
        let mut cur = disp;
        for h in &t.handlers {
            let ast::ExceptHandler::ExceptHandler(eh) = h;
            self.switch_to(cur);
            let mut exc_types = Vec::new();
            let span = Span::from(eh.range);
            let cond = match &eh.type_ {
                Some(ty) => {
                    exc_types.push(self.module.slice_text(Span::from(ty.range())));
                    let _ = self.lower_expr(ty);
                    self.emit_const(ConstLiteral::Other, span)
                }
                None => self.emit_const(ConstLiteral::Bool(true), span),
            };
            let handler_bb = self.new_block(true);
            let next_bb = self.new_block(false);
            self.terminate_at(
                Terminator::Branch {
                    cond,
                    then_bb: handler_bb,
                    else_bb: next_bb,
                },
                span,
            );

            self.switch_to(handler_bb);
            let exc_var = eh
                .name
                .as_ref()
                .map(|n| self.fresh_var(Some(n.to_string())));
            self.emit(
                InstKind::CatchException {
                    dst: exc_var,
                    exc_types,
                },
                span,
            );
            if let (Some(n), Some(v)) = (&eh.name, exc_var) {
                self.write_var(n.as_str(), v, Span::from(n.range));
            }
            self.lower_suite(&eh.body);
            // `except E as e:` ends with `del e`.
            if let Some(n) = &eh.name
                && !self.is_terminated(self.cur_bb)
            {
                self.lower_name_delete(n.as_str(), Span::from(n.range));
            }
            self.terminate(Terminator::Jump(if t.is_star {
                next_bb
            } else {
                normal_next
            }));
            self.seal_block(next_bb);
            cur = next_bb;
        }
        self.switch_to(cur);
        if t.is_star {
            let rest = self.emit_const(ConstLiteral::Other, Span::from(t.range));
            let raise_bb = self.new_block(true);
            self.terminate(Terminator::Branch {
                cond: rest,
                then_bb: normal_next,
                else_bb: raise_bb,
            });
            self.switch_to(raise_bb);
        }
        self.terminate(Terminator::Raise(None));
    }

    fn lower_match(&mut self, m: &'a ast::StmtMatch) {
        let subject = self.lower_expr(&m.subject);
        let merge_bb = self.new_block(false);

        for (idx, case) in m.cases.iter().enumerate() {
            let is_last = idx + 1 == m.cases.len();
            let next_bb = if is_last {
                merge_bb
            } else {
                self.new_block(false)
            };
            let mut captures = Vec::new();
            self.lower_pattern(&case.pattern, subject, next_bb, &mut captures);
            for (name, value, span) in captures {
                self.write_var(name, value, span);
            }
            if let Some(guard) = &case.guard {
                let g = self.lower_expr(guard);
                self.branch_on_test(g, next_bb, Span::from(guard.range()));
            }
            self.lower_suite(&case.body);
            self.terminate(Terminator::Jump(merge_bb));
            if !is_last {
                self.seal_block(next_bb);
                self.switch_to(next_bb);
            }
        }

        self.seal_block(merge_bb);
        self.switch_to(merge_bb);
    }

    /// Continue in a new block when `cond` holds; otherwise go to `fail`.
    fn branch_on_test(&mut self, cond: VarId, fail: BlockId, span: Span) {
        let ok_bb = self.new_block(true);
        self.terminate_at(
            Terminator::Branch {
                cond,
                then_bb: ok_bb,
                else_bb: fail,
            },
            span,
        );
        self.switch_to(ok_bb);
    }

    /// A test of the subject's shape (its type, length, or keys) the IR does not model: an
    /// opaque value, so both outcomes stay possible.
    fn structural_test(&mut self, fail: BlockId, span: Span) {
        let cond = self.emit_const(ConstLiteral::Other, span);
        self.branch_on_test(cond, fail, span);
    }

    /// Lower the tests of `pat` against `subject` (PEP 634), going to `fail` when one fails,
    /// and collect the values its capture patterns bind. The bindings are written only once
    /// the whole pattern matched: a failed alternative binds nothing.
    fn lower_pattern(
        &mut self,
        pat: &'a Pattern,
        subject: VarId,
        fail: BlockId,
        captures: &mut Vec<(&'a str, VarId, Span)>,
    ) {
        let span = Span::from(pat.range());
        match pat {
            Pattern::MatchValue(mv) => {
                let value = self.lower_expr(&mv.value);
                let cond = self.fresh_var(None);
                self.emit(
                    InstKind::Compare {
                        dst: cond,
                        op: CompareOpKind::Eq,
                        lhs: subject,
                        rhs: value,
                    },
                    span,
                );
                self.branch_on_test(cond, fail, span);
            }
            Pattern::MatchSingleton(ms) => {
                let literal = match ms.value {
                    ast::Singleton::None => ConstLiteral::None,
                    ast::Singleton::True => ConstLiteral::Bool(true),
                    ast::Singleton::False => ConstLiteral::Bool(false),
                };
                let value = self.emit_const(literal, span);
                let cond = self.fresh_var(None);
                self.emit(
                    InstKind::Compare {
                        dst: cond,
                        op: CompareOpKind::Is,
                        lhs: subject,
                        rhs: value,
                    },
                    span,
                );
                self.branch_on_test(cond, fail, span);
            }
            Pattern::MatchSequence(ms) => {
                self.structural_test(fail, span);
                let star = ms
                    .patterns
                    .iter()
                    .position(|p| matches!(p, Pattern::MatchStar(_)));
                let len = ms.patterns.len();
                for (i, sub) in ms.patterns.iter().enumerate() {
                    if let Pattern::MatchStar(st) = sub {
                        // `*rest` is a new list of the items between the fixed positions.
                        let rest = self.unpacked_list(subject, Span::from(st.range));
                        self.lower_pattern(sub, rest, fail, captures);
                        continue;
                    }
                    // Items after the star are counted from the end.
                    let index = match star {
                        Some(s) if i > s => -((len - i) as i64),
                        _ => i as i64,
                    };
                    let item = self.pattern_item(subject, index, span, Span::from(sub.range()));
                    self.lower_pattern(sub, item, fail, captures);
                }
            }
            Pattern::MatchMapping(mm) => {
                self.structural_test(fail, span);
                for (key, sub) in mm.keys.iter().zip(&mm.patterns) {
                    let index = self.lower_expr(key);
                    let value = self.fresh_var(None);
                    self.emit(
                        InstKind::SubscriptLoad {
                            dst: value,
                            obj: subject,
                            index,
                            key_const: extract_const_str(key),
                            is_slice: false,
                            slice_lower_const: None,
                            slice_upper_const: None,
                        },
                        Span::from(sub.range()),
                    );
                    self.lower_pattern(sub, value, fail, captures);
                }
                if let Some(rest) = &mm.rest {
                    // `**rest` is a new dict of the other items.
                    let dst = self.fresh_var(None);
                    self.emit(
                        InstKind::DictLiteral {
                            dst,
                            entries: vec![DictEntry::Unpack { dict: subject }],
                        },
                        Span::from(rest.range),
                    );
                    captures.push((rest.as_str(), dst, Span::from(rest.range)));
                }
            }
            Pattern::MatchClass(mc) => {
                let _ = self.lower_expr(&mc.cls);
                self.structural_test(fail, span);
                // A positional sub-pattern reads the attribute `__match_args__` names, which
                // the IR does not know.
                let positional = mc
                    .arguments
                    .patterns
                    .iter()
                    .enumerate()
                    .map(|(i, p)| (format!("{MATCH_ARGS}[{i}]"), p));
                let keywords = mc
                    .arguments
                    .keywords
                    .iter()
                    .map(|kw| (kw.attr.to_string(), &kw.pattern));
                for (attr, sub) in positional.chain(keywords).collect::<Vec<_>>() {
                    let value = self.fresh_var(None);
                    self.emit(
                        InstKind::AttrLoad {
                            dst: value,
                            obj: subject,
                            attr,
                        },
                        Span::from(sub.range()),
                    );
                    self.lower_pattern(sub, value, fail, captures);
                }
            }
            // Only inside a sequence pattern, which passes the new list of the items it takes.
            Pattern::MatchStar(ms) => {
                if let Some(name) = &ms.name {
                    captures.push((name.as_str(), subject, Span::from(name.range)));
                }
            }
            Pattern::MatchAs(ma) => {
                if let Some(p) = &ma.pattern {
                    self.lower_pattern(p, subject, fail, captures);
                }
                if let Some(name) = &ma.name {
                    captures.push((name.as_str(), subject, Span::from(name.range)));
                }
            }
            Pattern::MatchOr(mo) => self.lower_or_pattern(mo, subject, fail, captures),
        }
    }

    /// `p1 | p2 | ...`: the first alternative that matches binds the names (every alternative
    /// binds the same ones).
    fn lower_or_pattern(
        &mut self,
        mo: &'a ast::PatternMatchOr,
        subject: VarId,
        fail: BlockId,
        captures: &mut Vec<(&'a str, VarId, Span)>,
    ) {
        let join_bb = self.new_block(false);
        let prefix = format!(".or_{}", self.var_names.len());
        let mut bound: Vec<(&'a str, Span)> = Vec::new();
        for (idx, alt) in mo.patterns.iter().enumerate() {
            let alt_fail = if idx + 1 == mo.patterns.len() {
                fail
            } else {
                self.new_block(false)
            };
            let mut alt_captures = Vec::new();
            self.lower_pattern(alt, subject, alt_fail, &mut alt_captures);
            for (name, value, span) in alt_captures {
                self.write_local(&format!("{prefix}_{name}"), self.cur_bb, value);
                if !bound.iter().any(|(n, _)| *n == name) {
                    bound.push((name, span));
                }
            }
            self.terminate(Terminator::Jump(join_bb));
            if alt_fail != fail {
                self.seal_block(alt_fail);
                self.switch_to(alt_fail);
            }
        }
        self.seal_block(join_bb);
        self.switch_to(join_bb);
        for (name, span) in bound {
            let value = self.read_local_ssa(&format!("{prefix}_{name}"), join_bb, span);
            captures.push((name, value, span));
        }
    }

    /// Item `index` of a sequence pattern's subject (negative: counted from the end).
    fn pattern_item(&mut self, subject: VarId, index: i64, pattern: Span, item: Span) -> VarId {
        // The index belongs to the pattern, not to the item: not the target unpacking of an
        // assignment (`heki_effect::consumption::is_unpacking`), which consumes an iterator.
        let index_var = self.emit_const(ConstLiteral::Int(index), pattern);
        let dst = self.fresh_var(None);
        self.emit(
            InstKind::SubscriptLoad {
                dst,
                obj: subject,
                index: index_var,
                key_const: None,
                is_slice: false,
                slice_lower_const: None,
                slice_upper_const: None,
            },
            item,
        );
        dst
    }

    /// A new list holding items of `source` (`[*source]`): what a starred target or pattern
    /// binds.
    fn unpacked_list(&mut self, source: VarId, span: Span) -> VarId {
        let elem = self.fresh_var(Some(crate::UNPACKED.to_owned()));
        self.emit(
            InstKind::Copy {
                dst: elem,
                src: source,
            },
            span,
        );
        let dst = self.fresh_var(None);
        self.starred.push((dst, 0));
        self.emit(
            InstKind::AllocBuiltin {
                dst,
                kind: BuiltinKind::List,
                elems: vec![elem],
            },
            span,
        );
        dst
    }

    // -------------------------------------------------------------------------
    // Expressions (strict left-to-right evaluation order)
    // -------------------------------------------------------------------------

    /// The elements of a list, tuple, or set display. A starred element (`*xs`) is a copy of
    /// `xs` named [`crate::UNPACKED`]: the display holds the elements of `xs`, not `xs` itself.
    fn lower_display_elems(&mut self, elts: &'a [Expr]) -> Vec<VarId> {
        elts.iter()
            .map(|e| match e {
                Expr::Starred(s) => {
                    let src = self.lower_expr(&s.value);
                    let dst = self.fresh_var(Some(crate::UNPACKED.to_owned()));
                    self.emit(InstKind::Copy { dst, src }, Span::from(s.range));
                    dst
                }
                _ => self.lower_expr(e),
            })
            .collect()
    }

    fn lower_expr(&mut self, expr: &'a Expr) -> VarId {
        let span = Span::from(expr.range());
        match expr {
            Expr::Name(n) => {
                let name = n.id.as_str();
                let global_name = self.resolves_to_global(name);
                self.record_a1_reference(name, global_name, span);
                self.read_var(name, span)
            }
            Expr::Named(named) => {
                let val = self.lower_expr(&named.value);
                // PEP 572: walrus writes into the enclosing scope, bypassing comprehension target
                // isolation. The target is always a name (`InvalidNamedAssignmentTarget`).
                if let Expr::Name(n) = named.target.as_ref() {
                    let saved = std::mem::take(&mut self.comp_target_renames);
                    self.write_var(n.id.as_str(), val, Span::from(n.range));
                    self.comp_target_renames = saved;
                }
                val
            }
            Expr::NumberLiteral(num) => {
                let lit = match &num.value {
                    ast::Number::Int(i) => i
                        .as_i64()
                        .map(ConstLiteral::Int)
                        .unwrap_or(ConstLiteral::Other),
                    ast::Number::Float(f) => ConstLiteral::Float(f.to_string()),
                    ast::Number::Complex { .. } => ConstLiteral::Other,
                };
                self.emit_const(lit, span)
            }
            Expr::StringLiteral(s) => {
                self.emit_const(ConstLiteral::Str(s.value.to_str().to_string()), span)
            }
            Expr::BytesLiteral(_) => self.emit_const(ConstLiteral::Bytes, span),
            Expr::BooleanLiteral(b) => self.emit_const(ConstLiteral::Bool(b.value), span),
            // IPython escapes only parse in notebooks; heki parses `.py` sources.
            Expr::NoneLiteral(_) | Expr::IpyEscapeCommand(_) => {
                self.emit_const(ConstLiteral::None, span)
            }
            Expr::EllipsisLiteral(_) => self.emit_const(ConstLiteral::Ellipsis, span),
            Expr::List(l) => {
                let elems = self.lower_display_elems(&l.elts);
                let dst = self.fresh_var(None);
                self.record_starred(dst, &l.elts);
                self.emit(
                    InstKind::AllocBuiltin {
                        dst,
                        kind: BuiltinKind::List,
                        elems,
                    },
                    span,
                );
                dst
            }
            Expr::Tuple(t) => {
                let elems = self.lower_display_elems(&t.elts);
                let dst = self.fresh_var(None);
                self.record_starred(dst, &t.elts);
                self.emit(
                    InstKind::AllocBuiltin {
                        dst,
                        kind: BuiltinKind::Tuple,
                        elems,
                    },
                    span,
                );
                dst
            }
            Expr::Set(s) => {
                let elems = self.lower_display_elems(&s.elts);
                let dst = self.fresh_var(None);
                self.record_starred(dst, &s.elts);
                self.emit(
                    InstKind::AllocBuiltin {
                        dst,
                        kind: BuiltinKind::Set,
                        elems,
                    },
                    span,
                );
                dst
            }
            Expr::Dict(d) => {
                let mut entries = Vec::with_capacity(d.items.len());
                for item in &d.items {
                    if let Some(k_expr) = &item.key {
                        let key = self.lower_expr(k_expr);
                        let value = self.lower_expr(&item.value);
                        entries.push(DictEntry::KeyValue { key, value });
                    } else {
                        let dict = self.lower_expr(&item.value);
                        entries.push(DictEntry::Unpack { dict });
                    }
                }
                let dst = self.fresh_var(None);
                self.emit(InstKind::DictLiteral { dst, entries }, span);
                dst
            }
            Expr::BinOp(b) => {
                let lhs = self.lower_expr(&b.left);
                let rhs = self.lower_expr(&b.right);
                let dst = self.fresh_var(None);
                self.emit(
                    InstKind::BinOp {
                        dst,
                        op: map_binop(b.op),
                        lhs,
                        rhs,
                        in_place: false,
                    },
                    span,
                );
                dst
            }
            Expr::UnaryOp(u) => {
                let operand = self.lower_expr(&u.operand);
                // Fold unary negative integer constant (`-1`) so `q[-1]` is recognized directly.
                if matches!(u.op, ast::UnaryOp::USub)
                    && let Expr::NumberLiteral(num) = u.operand.as_ref()
                    && let ast::Number::Int(i) = &num.value
                    && let Some(n) = i.as_i64().and_then(|v| v.checked_neg())
                {
                    return self.emit_const(ConstLiteral::Int(n), span);
                }
                let dst = self.fresh_var(None);
                self.emit(
                    InstKind::UnaryOp {
                        dst,
                        op: map_unaryop(u.op),
                        operand,
                    },
                    span,
                );
                dst
            }
            Expr::BoolOp(b) => {
                let temp_name = format!(".boolop_{}", self.var_names.len());
                let merge_bb = self.new_block(false);
                let is_and = matches!(b.op, ast::BoolOp::And);

                for (idx, val_expr) in b.values.iter().enumerate() {
                    let v = self.lower_expr(val_expr);
                    self.write_local(&temp_name, self.cur_bb, v);
                    if idx + 1 < b.values.len() {
                        let next_bb = self.new_block(true);
                        let (then_bb, else_bb) = if is_and {
                            (next_bb, merge_bb)
                        } else {
                            (merge_bb, next_bb)
                        };
                        self.terminate_at(
                            Terminator::Branch {
                                cond: v,
                                then_bb,
                                else_bb,
                            },
                            Span::from(val_expr.range()),
                        );
                        self.switch_to(next_bb);
                    } else {
                        self.terminate(Terminator::Jump(merge_bb));
                    }
                }
                self.seal_block(merge_bb);
                self.switch_to(merge_bb);
                self.read_local_ssa(&temp_name, merge_bb, span)
            }
            Expr::Compare(c) => self.lower_compare(c),
            Expr::If(if_expr) => {
                let temp_name = format!(".ifexpr_{}", self.var_names.len());
                let cond = self.lower_expr(&if_expr.test);
                let then_bb = self.new_block(true);
                let else_bb = self.new_block(true);
                let merge_bb = self.new_block(false);
                self.terminate_at(
                    Terminator::Branch {
                        cond,
                        then_bb,
                        else_bb,
                    },
                    Span::from(if_expr.test.range()),
                );

                self.switch_to(then_bb);
                let tval = self.lower_expr(&if_expr.body);
                self.write_local(&temp_name, self.cur_bb, tval);
                self.terminate(Terminator::Jump(merge_bb));

                self.switch_to(else_bb);
                let eval = self.lower_expr(&if_expr.orelse);
                self.write_local(&temp_name, self.cur_bb, eval);
                self.terminate(Terminator::Jump(merge_bb));

                self.seal_block(merge_bb);
                self.switch_to(merge_bb);
                self.read_local_ssa(&temp_name, merge_bb, span)
            }
            Expr::Attribute(attr) => {
                let obj = self.lower_expr(&attr.value);
                let attr_name = attr.attr.to_string();
                self.record_a1_attr_if_hazard(&attr_name, span);
                if let Some(dotted) = expr_dotted_name(expr) {
                    self.record_a1_reference(&dotted, false, span);
                }
                let dst = self.fresh_var(None);
                self.emit(
                    InstKind::AttrLoad {
                        dst,
                        obj,
                        attr: attr_name,
                    },
                    span,
                );
                dst
            }
            Expr::Subscript(sub) => {
                let obj = self.lower_expr(&sub.value);
                let (index, key_const, is_slice, slice_lower_const, slice_upper_const) =
                    self.lower_subscript_index(&sub.slice);
                let dst = self.fresh_var(None);
                self.emit(
                    InstKind::SubscriptLoad {
                        dst,
                        obj,
                        index,
                        key_const,
                        is_slice,
                        slice_lower_const,
                        slice_upper_const,
                    },
                    span,
                );
                dst
            }
            // `a[l:u:s]` passes `slice(l, u, s)` to `__getitem__`, which may keep it: the slice
            // is the tuple of its three bounds (`None` where one is left out).
            Expr::Slice(sl) => {
                let elems: Vec<VarId> = [&sl.lower, &sl.upper, &sl.step]
                    .into_iter()
                    .map(|bound| match bound {
                        Some(b) => self.lower_expr(b),
                        None => self.emit_const(ConstLiteral::None, span),
                    })
                    .collect();
                let dst = self.fresh_var(None);
                self.emit(
                    InstKind::AllocBuiltin {
                        dst,
                        kind: BuiltinKind::Tuple,
                        elems,
                    },
                    span,
                );
                dst
            }
            Expr::Starred(s) => self.lower_expr(&s.value),
            Expr::Await(a) => {
                // Propagate `is_awaited = true` directly onto `await f(...)` / `await obj.m(...)`.
                let val = if let Expr::Call(call) = a.value.as_ref() {
                    self.lower_call(call, true)
                } else {
                    self.lower_expr(&a.value)
                };
                let dst = self.fresh_var(None);
                self.emit(InstKind::Await { dst, value: val }, span);
                dst
            }
            Expr::Yield(y) => {
                let value = y.value.as_ref().map(|v| self.lower_expr(v));
                let dst = self.fresh_var(None);
                self.emit(InstKind::Yield { dst, value }, span);
                dst
            }
            Expr::YieldFrom(yf) => {
                let value = self.lower_expr(&yf.value);
                let dst = self.fresh_var(None);
                self.emit(InstKind::YieldFrom { dst, value }, span);
                dst
            }
            Expr::Call(call) => self.lower_call(call, false),
            Expr::Lambda(lam) => {
                // The function object keeps its defaults.
                let retained: Vec<VarId> = lam
                    .parameters
                    .iter()
                    .flat_map(|params| parameter_defaults(params))
                    .map(|d| self.lower_expr(d))
                    .collect();
                let child_outer = self.child_outer_locals();
                let qualname = format!("{}.<locals>.<lambda>", self.qualname);
                let child_id = self.module.lower_function_body(FunctionSpec {
                    name: "<lambda>".to_string(),
                    qualname,
                    owner_class: None,
                    owner_class_qualname: None,
                    is_async: false,
                    params: lam.parameters.as_deref(),
                    return_annotation: None,
                    decorators: Vec::new(),
                    span,
                    is_module_body: false,
                    is_class_body: false,
                    outer_locals: child_outer,
                    walrus_target: WalrusTarget::OwnScope,
                    walrus_globals: FxHashSet::default(),
                    body: FunctionBody::Expr(&lam.body),
                });
                let captured_names = self.collect_child_captures(child_id);
                let mut captured = self.read_captured_vars(&captured_names, span);
                captured.extend(retained);
                let dst = self.fresh_var(None);
                self.emit(
                    InstKind::MakeClosure {
                        dst,
                        func_id: Some(child_id),
                        captured,
                    },
                    span,
                );
                dst
            }
            Expr::Generator(ge) => {
                // Outermost `iter` is evaluated eagerly in the enclosing frame! (The grammar
                // requires at least one `for` clause.)
                let outer_iter = self.lower_expr(&ge.generators[0].iter);

                let child_outer = self.child_outer_locals();
                let qualname = format!("{}.<locals>.<genexpr>", self.qualname);
                let child_id = self.module.lower_function_body(FunctionSpec {
                    name: "<genexpr>".to_string(),
                    qualname,
                    owner_class: None,
                    owner_class_qualname: None,
                    is_async: ge.generators.iter().any(|g| g.is_async),
                    params: None,
                    return_annotation: None,
                    decorators: Vec::new(),
                    span,
                    is_module_body: false,
                    is_class_body: false,
                    outer_locals: child_outer,
                    walrus_target: self.comprehension_walrus_target(),
                    walrus_globals: self.comprehension_walrus_globals(),
                    body: FunctionBody::GenExpr(ge),
                });
                let captured_names = self.collect_child_captures(child_id);
                let captured = self.read_captured_vars(&captured_names, span);
                let dst = self.fresh_var(None);
                self.emit(
                    InstKind::MakeGenExpr {
                        dst,
                        func_id: Some(child_id),
                        outer_iter,
                        captured,
                    },
                    span,
                );
                dst
            }
            Expr::ListComp(lc) => self.lower_comprehension(
                "<listcomp>",
                CompKind::List,
                &lc.generators,
                &lc.elt,
                None,
                span,
            ),
            Expr::SetComp(sc) => self.lower_comprehension(
                "<setcomp>",
                CompKind::Set,
                &sc.generators,
                &sc.elt,
                None,
                span,
            ),
            Expr::DictComp(dc) => self.lower_comprehension(
                "<dictcomp>",
                CompKind::Dict,
                &dc.generators,
                &dc.value,
                dc.key.as_deref(),
                span,
            ),
            Expr::FString(fs) => {
                let mut parts = Vec::new();
                let mut has_text = false;
                for part in fs.value.iter() {
                    match part {
                        ast::FStringPartRef::Literal(lit) => has_text |= !lit.value.is_empty(),
                        ast::FStringPartRef::FString(f) => {
                            has_text |= self.lower_fields(&f.elements, &mut parts);
                        }
                    }
                }
                let dst = self.joined(parts, has_text, span);
                if let Some(head) = heki_ast::fstring_head(expr) {
                    self.str_heads.push((dst, head));
                }
                dst
            }
            // A template keeps the values of its replacement fields (`Template.values`, a tuple)
            // unconverted and unformatted, and is otherwise opaque; a format specification's
            // fields are formatted into its text.
            Expr::TString(ts) => {
                let mut values = Vec::new();
                for field in ts.value.iter().flat_map(|t| t.elements.interpolations()) {
                    values.push(self.lower_expr(&field.expression));
                    if let Some(spec) = &field.format_spec {
                        let _ = self.lower_format_spec(spec);
                    }
                }
                let held = self.fresh_var(None);
                self.emit(
                    InstKind::AllocBuiltin {
                        dst: held,
                        kind: BuiltinKind::Tuple,
                        elems: values,
                    },
                    span,
                );
                self.emit_const(ConstLiteral::Other, span)
            }
        }
    }

    /// Lower the replacement fields of `elements` in order, pushing each formatted field onto
    /// `parts`; whether the elements write any literal text (a field's `x=` included).
    fn lower_fields(
        &mut self,
        elements: &'a ast::InterpolatedStringElements,
        parts: &mut Vec<VarId>,
    ) -> bool {
        let mut has_text = false;
        for element in elements {
            match element {
                ast::InterpolatedStringElement::Literal(_) => has_text = true,
                ast::InterpolatedStringElement::Interpolation(field) => {
                    has_text |= field.debug_text.is_some();
                    parts.push(self.lower_field(field));
                }
            }
        }
        has_text
    }

    /// One replacement field: its value, converted, then formatted with its specification,
    /// which is evaluated after the conversion.
    fn lower_field(&mut self, field: &'a ast::InterpolatedElement) -> VarId {
        let span = Span::from(field.range);
        let mut value = self.lower_expr(&field.expression);
        let conversion = match field.conversion {
            ast::ConversionFlag::Str => Some(Conversion::Str),
            ast::ConversionFlag::Repr => Some(Conversion::Repr),
            ast::ConversionFlag::Ascii => Some(Conversion::Ascii),
            // `{x=}` shows `repr(x)` unless the field has a conversion or a specification.
            ast::ConversionFlag::None
                if field.debug_text.is_some() && field.format_spec.is_none() =>
            {
                Some(Conversion::Repr)
            }
            ast::ConversionFlag::None => None,
        };
        if let Some(conversion) = conversion {
            let dst = self.fresh_var(None);
            self.emit(
                InstKind::Convert {
                    dst,
                    value,
                    conversion,
                },
                span,
            );
            value = dst;
        }
        let spec = field
            .format_spec
            .as_ref()
            .map(|spec| self.lower_format_spec(spec));
        let dst = self.fresh_var(None);
        self.emit(InstKind::Format { dst, value, spec }, span);
        dst
    }

    /// The text of a format specification: a constant, or an f-string of its own.
    fn lower_format_spec(&mut self, spec: &'a ast::InterpolatedStringFormatSpec) -> VarId {
        let span = Span::from(spec.range);
        let mut parts = Vec::new();
        let has_text = self.lower_fields(&spec.elements, &mut parts);
        if parts.is_empty() {
            let text: String = spec.elements.literals().map(|l| &*l.value).collect();
            return self.emit_const(ConstLiteral::Str(text), span);
        }
        self.joined(parts, has_text, span)
    }

    /// The `str` of an f-string's literal text and formatted fields: a field alone is its own
    /// result (which `__format__` may return as a subclass), anything else a new `str`.
    fn joined(&mut self, parts: Vec<VarId>, has_text: bool, span: Span) -> VarId {
        if let ([field], false) = (parts.as_slice(), has_text) {
            return *field;
        }
        let dst = self.fresh_var(None);
        self.emit(InstKind::BuildString { dst, parts }, span);
        dst
    }

    /// `a < b < c` is `a < b and b < c` with `b` evaluated once: each comparison but the last
    /// branches to the result on a false value before the next operand runs. Each comparison
    /// spans its own two operands (the first and the last also the expression's parentheses).
    fn lower_compare(&mut self, c: &'a ast::ExprCompare) -> VarId {
        let whole = Span::from(c.range);
        if let ([op], [left, right]) = (&*c.ops, &*c.operands) {
            let lhs = self.lower_expr(left);
            let rhs = self.lower_expr(right);
            let dst = self.fresh_var(None);
            self.emit(
                InstKind::Compare {
                    dst,
                    op: map_cmpop(*op),
                    lhs,
                    rhs,
                },
                whole,
            );
            return dst;
        }
        let last = c.ops.len() - 1;
        let result_name = format!(".compare_{}", self.var_names.len());
        let merge_bb = self.new_block(false);
        let mut lhs = self.lower_expr(&c.operands[0]);
        for (i, (op, rhs_expr)) in c.ops.iter().zip(&c.operands[1..]).enumerate() {
            let rhs = self.lower_expr(rhs_expr);
            let link = Span {
                start: if i == 0 {
                    whole.start
                } else {
                    Span::from(c.operands[i].range()).start
                },
                end: if i == last {
                    whole.end
                } else {
                    Span::from(rhs_expr.range()).end
                },
            };
            let dst = self.fresh_var(None);
            self.emit(
                InstKind::Compare {
                    dst,
                    op: map_cmpop(*op),
                    lhs,
                    rhs,
                },
                link,
            );
            self.write_local(&result_name, self.cur_bb, dst);
            if i == last {
                self.terminate(Terminator::Jump(merge_bb));
            } else {
                let next_bb = self.new_block(true);
                self.terminate_at(
                    Terminator::Branch {
                        cond: dst,
                        then_bb: next_bb,
                        else_bb: merge_bb,
                    },
                    link,
                );
                self.switch_to(next_bb);
                lhs = rhs;
            }
        }
        self.seal_block(merge_bb);
        self.switch_to(merge_bb);
        self.read_local_ssa(&result_name, merge_bb, whole)
    }

    fn lower_call(&mut self, call: &'a ast::ExprCall, is_awaited: bool) -> VarId {
        let span = Span::from(call.range());
        let callee_dotted = expr_dotted_name(&call.func)
            .unwrap_or_else(|| self.module.slice_text(Span::from(call.func.range())));
        let star_from = call
            .arguments
            .args
            .iter()
            .position(|a| matches!(a, Expr::Starred(_)))
            .map(|i| u8::try_from(i).unwrap_or(u8::MAX));

        if let Expr::Attribute(attr) = call.func.as_ref() {
            // Check A1 hazards for dotted calls like `sys._getframe`, `gc.get_objects`, `inspect.currentframe`.
            self.record_a1_call_if_hazard(
                &callee_dotted,
                call.arguments.args.len(),
                call.arguments.keywords.len(),
                span,
            );
            let recv = self.lower_expr(&attr.value);
            let args: Vec<VarId> = call
                .arguments
                .args
                .iter()
                .map(|a| self.lower_expr(a))
                .collect();
            let kwargs: Vec<(Option<String>, VarId)> = call
                .arguments
                .keywords
                .iter()
                .map(|kw| {
                    (
                        kw.arg.as_ref().map(|id| id.to_string()),
                        self.lower_expr(&kw.value),
                    )
                })
                .collect();
            let dst = self.fresh_var(None);
            self.record_starred(dst, &call.arguments.args);
            self.emit(
                InstKind::MethodCall {
                    dst,
                    recv,
                    method: attr.attr.to_string(),
                    args,
                    kwargs,
                    is_awaited,
                    star_from,
                },
                span,
            );
            dst
        } else {
            self.record_a1_call_if_hazard(
                &callee_dotted,
                call.arguments.args.len(),
                call.arguments.keywords.len(),
                span,
            );
            // The call itself is the hazard (above), not the read of its callee.
            let callee_var = Some(match call.func.as_ref() {
                Expr::Name(n) => self.read_var(n.id.as_str(), Span::from(n.range)),
                other => self.lower_expr(other),
            });
            let args: Vec<VarId> = call
                .arguments
                .args
                .iter()
                .map(|a| self.lower_expr(a))
                .collect();
            let kwargs: Vec<(Option<String>, VarId)> = call
                .arguments
                .keywords
                .iter()
                .map(|kw| {
                    (
                        kw.arg.as_ref().map(|id| id.to_string()),
                        self.lower_expr(&kw.value),
                    )
                })
                .collect();
            let dst = self.fresh_var(None);
            self.record_starred(dst, &call.arguments.args);
            self.emit(
                InstKind::Call {
                    dst,
                    callee_var,
                    callee: callee_dotted,
                    args,
                    kwargs,
                    is_awaited,
                    star_from,
                },
                span,
            );
            dst
        }
    }

    /// One element of a list / set / dict comprehension into its accumulator `acc`:
    /// `acc.append(elt)`, `acc.add(elt)`, or `acc[key] = elt`; an unpacking element (PEP 798,
    /// `[*xs for xs in rows]`, `{**m for m in maps}`) is `acc.extend(xs)` / `acc.update(...)`.
    fn emit_comp_element(
        &mut self,
        kind: CompKind,
        acc: VarId,
        elt: &'a Expr,
        key: Option<&'a Expr>,
    ) {
        let elt_span = Span::from(elt.range());
        let (method, args) = match (kind, elt, key) {
            (CompKind::List | CompKind::Set, Expr::Starred(st), _) => {
                let method = match kind {
                    CompKind::List => "extend",
                    _ => "update",
                };
                (method, vec![self.lower_expr(&st.value)])
            }
            (CompKind::List, _, _) => ("append", vec![self.lower_expr(elt)]),
            (CompKind::Set, _, _) => ("add", vec![self.lower_expr(elt)]),
            (CompKind::Dict, _, None) => ("update", vec![self.lower_expr(elt)]),
            (CompKind::Dict, _, Some(key)) => {
                let index = self.lower_expr(key);
                let value = self.lower_expr(elt);
                self.emit(
                    InstKind::SubscriptStore {
                        obj: acc,
                        index,
                        key_const: extract_const_str(key),
                        value,
                    },
                    elt_span,
                );
                return;
            }
        };
        let dst = self.fresh_var(None);
        self.emit(
            InstKind::MethodCall {
                dst,
                recv: acc,
                method: method.to_string(),
                args,
                kwargs: Vec::new(),
                is_awaited: false,
                star_from: None,
            },
            elt_span,
        );
    }

    fn lower_comprehension(
        &mut self,
        comp_name: &str,
        kind: CompKind,
        generators: &'a [ast::Comprehension],
        elt: &'a Expr,
        key: Option<&'a Expr>,
        span: Span,
    ) -> VarId {
        if self.module.profile.pep709_inlined_comprehensions {
            let acc = self.fresh_var(None);
            self.emit(
                InstKind::AllocBuiltin {
                    dst: acc,
                    kind: kind.builtin(),
                    elems: Vec::new(),
                },
                span,
            );
            self.lower_comprehension_generators(generators, None, true, &|b| {
                b.emit_comp_element(kind, acc, elt, key);
            });
            acc
        } else {
            // Python 3.11: non-inlined comprehension frame with eager outer iter (the grammar
            // requires at least one `for` clause).
            let outer_iter = self.lower_expr(&generators[0].iter);
            let child_outer = self.child_outer_locals();
            let qualname = format!("{}.<locals>.{comp_name}", self.qualname);
            let child_id = self.module.lower_function_body(FunctionSpec {
                name: comp_name.to_string(),
                qualname,
                owner_class: None,
                owner_class_qualname: None,
                is_async: generators.iter().any(|g| g.is_async),
                params: None,
                return_annotation: None,
                decorators: Vec::new(),
                span,
                is_module_body: false,
                is_class_body: false,
                outer_locals: child_outer,
                walrus_target: self.comprehension_walrus_target(),
                walrus_globals: self.comprehension_walrus_globals(),
                body: FunctionBody::Comp {
                    generators,
                    elt,
                    key,
                    kind,
                },
            });
            let captured_names = self.collect_child_captures(child_id);
            let mut args = vec![outer_iter];
            args.extend(self.read_captured_vars(&captured_names, span));
            let dst = self.fresh_var(None);
            self.emit(
                InstKind::Call {
                    dst,
                    callee_var: None,
                    callee: format!("__comp_{}", child_id.0),
                    args,
                    kwargs: Vec::new(),
                    is_awaited: false,
                    star_from: None,
                },
                span,
            );
            dst
        }
    }

    /// Lower the `for` / `if` clauses of a comprehension as nested loops around `emit_leaf`.
    /// The leaf is a trait object, so one instance serves generator expressions and inlined /
    /// non-inlined comprehensions alike.
    fn lower_comprehension_generators(
        &mut self,
        generators: &'a [ast::Comprehension],
        first_iter_override: Option<VarId>,
        isolate_targets: bool,
        emit_leaf: &dyn Fn(&mut Self),
    ) {
        if generators.is_empty() {
            emit_leaf(self);
            return;
        }
        let g = &generators[0];
        let iter_span = Span::from(g.iter.range());
        let iterable = first_iter_override.unwrap_or_else(|| self.lower_expr(&g.iter));
        let iter_var = self.fresh_var(None);
        self.emit(
            InstKind::MakeIter {
                dst: iter_var,
                iterable,
                is_async: g.is_async,
            },
            iter_span,
        );
        let preheader = self.cur_bb;
        let depth = (self.loop_stack.len() as u16) + 1;
        let bound = classify_iter_expr_bound(&g.iter);

        let header_bb = self.new_block(false);
        let exit_bb = self.new_block(false);
        self.terminate(Terminator::Jump(header_bb));

        self.loop_stack.push(ActiveLoop {
            header: header_bb,
            preheader: Some(preheader),
            exit: exit_bb,
            depth,
            bound,
            body_blocks: vec![header_bb],
            back_edges: Vec::new(),
            hint: LoopHint::default(),
        });

        self.switch_to(header_bb);
        let next_val = self.fresh_var(None);
        let target_span = Span::from(g.target.range());
        self.emit(
            InstKind::IterNext {
                dst: next_val,
                iter: iter_var,
                is_async: g.is_async,
            },
            target_span,
        );
        let body_bb = self.new_block(true);
        self.terminate_at(
            Terminator::Branch {
                cond: next_val,
                then_bb: body_bb,
                else_bb: exit_bb,
            },
            target_span,
        );

        self.switch_to(body_bb);

        if isolate_targets {
            let comp_id = self.module.next_comp_id;
            self.module.next_comp_id += 1;
            let mut raw_targets = FxHashSet::default();
            collect_target_names(&g.target, &mut raw_targets);
            let mut rename_map = FxHashMap::default();
            for name in raw_targets {
                let isolated = crate::comprehension_target(comp_id, &name);
                rename_map.insert(name, isolated);
            }
            self.comp_target_renames.push(rename_map);
        }

        self.lower_assign_target(&g.target, next_val, Span::from(g.target.range()));
        for cond_expr in &g.ifs {
            let cond = self.lower_expr(cond_expr);
            let pass_bb = self.new_block(true);
            let skip_bb = self.new_block(true);
            self.terminate_at(
                Terminator::Branch {
                    cond,
                    then_bb: pass_bb,
                    else_bb: skip_bb,
                },
                Span::from(cond_expr.range()),
            );
            self.switch_to(skip_bb);
            let cur = self.cur_bb;
            if !self.is_dead(cur)
                && let Some(top) = self.loop_stack.last_mut()
            {
                top.back_edges.push(cur);
            }
            self.terminate(Terminator::Jump(header_bb));
            self.switch_to(pass_bb);
        }

        self.lower_comprehension_generators(&generators[1..], None, isolate_targets, emit_leaf);

        if isolate_targets {
            self.comp_target_renames.pop();
        }

        // Expressions never terminate their block, so the element always loops back.
        let tail = self.cur_bb;
        if !self.is_dead(tail)
            && let Some(top) = self.loop_stack.last_mut()
        {
            top.back_edges.push(tail);
        }
        self.terminate(Terminator::Jump(header_bb));

        let finished = self.loop_stack.pop().unwrap();
        self.seal_block(header_bb);
        self.seal_block(exit_bb);
        self.switch_to(exit_bb);
        self.finish_loop(finished);
    }

    fn lower_subscript_index(
        &mut self,
        slice_expr: &'a Expr,
    ) -> (VarId, Option<String>, bool, Option<i64>, Option<i64>) {
        if let Expr::Slice(sl) = slice_expr {
            let lower_const = sl.lower.as_deref().and_then(extract_const_int);
            let upper_const = sl.upper.as_deref().and_then(extract_const_int);
            let idx = self.lower_expr(slice_expr);
            (idx, None, true, lower_const, upper_const)
        } else {
            let key_const = extract_const_str(slice_expr);
            let idx = self.lower_expr(slice_expr);
            (idx, key_const, false, None, None)
        }
    }

    // -------------------------------------------------------------------------
    // A1 Hazard Detection
    // -------------------------------------------------------------------------
    /// A call of a frame-introspection function, by the callee's spelling.
    fn record_a1_call_if_hazard(
        &mut self,
        callee: &str,
        arg_count: usize,
        kwarg_count: usize,
        span: Span,
    ) {
        let kind = self.introspection_binding(callee).or_else(|| {
            introspection_kind(
                &self.module.resolve_module_alias(callee),
                arg_count == 0 && kwarg_count == 0,
            )
        });
        self.push_a1_hazard(kind, span);
    }

    /// The `A1Hazard` kind of the introspection function an import bound `name` to, in the
    /// scope a read of it here resolves to.
    fn introspection_binding(&self, name: &str) -> Option<&'static str> {
        if self.is_module_body || self.resolves_to_global(name) {
            self.module.introspection_imports.get(name).copied()
        } else {
            self.introspection_locals.get(name).copied()
        }
    }

    /// A reference to a frame-introspection function that is not a call of it (`snapshot =
    /// locals`, `map(eval, codes)`): whatever calls it later runs it in this frame or reads
    /// this frame's state. `global_name` is a name that resolves to a global or builtin.
    fn record_a1_reference(&mut self, spelling: &str, global_name: bool, span: Span) {
        let kind = self.introspection_binding(spelling).or_else(|| {
            (global_name || spelling.contains('.'))
                .then(|| introspection_kind(&self.module.resolve_module_alias(spelling), true))
                .flatten()
        });
        self.push_a1_hazard(kind, span);
    }

    fn push_a1_hazard(&mut self, kind: Option<&str>, span: Span) {
        if let Some(kind) = kind {
            self.a1_hazards.push(A1Hazard {
                kind: kind.to_string(),
                span,
            });
        }
    }

    /// Whether a read of `name` here resolves to a module global or a builtin.
    fn resolves_to_global(&self, name: &str) -> bool {
        self.resolve_comp_rename(name).is_none()
            && (self.scope.globals.contains(name)
                || (!self.scope.nonlocals.contains(name)
                    && !self.is_true_local(name)
                    && !self.outer_locals.contains(name)))
    }

    fn record_a1_attr_if_hazard(&mut self, attr: &str, span: Span) {
        if matches!(
            attr,
            "f_locals" | "f_globals" | "gi_frame" | "cr_frame" | "ag_frame" | "__subclasses__"
        ) {
            self.a1_hazards.push(A1Hazard {
                kind: format!(".{attr}"),
                span,
            });
        }
    }

    /// The function's IR passed its budget: its body becomes one call of an unknown callable
    /// with every parameter (whose result a function returns), and an A1 hazard says that
    /// nothing in it is modeled.
    fn replace_body_with_opaque_call(&mut self) {
        let callee = self.fresh_var(None);
        let result = self.fresh_var(None);
        let insts = vec![
            Inst {
                kind: InstKind::Const {
                    dst: callee,
                    literal: ConstLiteral::Other,
                },
                span: self.span,
            },
            Inst {
                kind: InstKind::Call {
                    dst: result,
                    callee_var: Some(callee),
                    callee: String::new(),
                    args: self.params.iter().map(|p| p.var).collect(),
                    kwargs: Vec::new(),
                    is_awaited: false,
                    star_from: None,
                },
                span: self.span,
            },
        ];
        let returns_value = !self.is_module_body && !self.is_class_body;
        self.blocks = vec![BasicBlock {
            id: BlockId(0),
            phis: Vec::new(),
            insts,
            terminator: Terminator::Return(returns_value.then_some(result)),
            exc_target: None,
            term_span: None,
        }];
        self.terminated = vec![true];
        self.may_raise = vec![false];
        self.preds = vec![Vec::new()];
        self.sealed = vec![true];
        self.reachable = vec![true];
        self.incomplete_phis.clear();
        self.var_replacements.clear();
        self.finished_loops.clear();
        self.finished_hints.clear();
        self.starred.clear();
        self.class_bases.clear();
        self.str_heads.clear();
        self.a1_hazards.push(A1Hazard {
            kind: crate::UNMODELED.to_owned(),
            span: self.span,
        });
    }

    // -------------------------------------------------------------------------
    // Finalization: seal remaining blocks, apply trivial-Phi replacements,
    // and compute `LoopCarriedInfo` for every natural loop.
    // -------------------------------------------------------------------------
    /// The function, and its class bases (`ModuleIr::class_bases`).
    fn finish(mut self) -> (IrFunction, Vec<(VarId, u32)>) {
        if self.unmodeled {
            self.replace_body_with_opaque_call();
        }
        for idx in 0..self.blocks.len() {
            self.seal_block(BlockId(idx as u32));
        }

        // Apply canonical variable replacements across all blocks.
        if !self.var_replacements.is_empty() {
            let map = self.var_replacements.clone();
            let resolve = |v: VarId| resolve_replacement(&map, v);
            for bb in &mut self.blocks {
                for phi in &mut bb.phis {
                    for (_, v) in &mut phi.incoming {
                        *v = resolve(*v);
                    }
                }
                for inst in &mut bb.insts {
                    rewrite_inst_vars(&mut inst.kind, resolve);
                }
                match &mut bb.terminator {
                    Terminator::Branch { cond, .. } => *cond = resolve(*cond),
                    Terminator::Return(Some(v)) | Terminator::Raise(Some(v)) => *v = resolve(*v),
                    _ => {}
                }
            }
        }

        let loop_carried =
            compute_loop_carried(&self.blocks, &self.finished_loops, &self.var_names);

        // A scope nested in an inlined comprehension reads its iteration variable through a
        // cell of this frame: the values bound to the variable carry the cell's name, as the
        // bindings of other names do.
        let comprehension_bindings: Vec<(VarId, String)> = self
            .current_def
            .iter()
            .filter(|((_, name), _)| {
                self.captured_by_children.contains(name)
                    && crate::comprehension_target_source(name).is_some()
            })
            .map(|((_, name), v)| (self.canonical_var(*v), name.clone()))
            .collect();
        for (var, name) in comprehension_bindings {
            let slot = &mut self.var_names[var.0 as usize];
            if slot.is_none() {
                *slot = Some(name);
            }
        }

        let mut captured_vars: Vec<String> = self.captured_by_children.into_iter().collect();
        captured_vars.sort();
        let mut free_names: Vec<String> = self.free_reads.into_iter().collect();
        free_names.sort();
        self.module.free_names[self.id.0 as usize] = free_names;
        self.module
            .loop_hints
            .push((self.id, std::mem::take(&mut self.finished_hints)));

        let class_bases = std::mem::take(&mut self.class_bases);
        let f = IrFunction {
            id: self.id,
            name: self.name,
            qualname: self.qualname,
            scope_id: None,
            owner_class: self.owner_class,
            owner_class_qualname: self.owner_class_qualname,
            kind: self.kind,
            params: self.params,
            return_annotation: self.return_annotation,
            decorators: self.decorators,
            local_annotations: self.local_annotations,
            var_names: self.var_names,
            captured_vars,
            starred: self.starred,
            str_heads: self.str_heads,
            blocks: self.blocks,
            loops: self.finished_loops,
            loop_carried,
            a1_hazards: self.a1_hazards,
            span: self.span,
            is_module_body: self.is_module_body,
            is_class_body: self.is_class_body,
            passed_from: None,
        };
        (f, class_bases)
    }
}

// =============================================================================
// Loop-Carried Phi Analysis (`DATA101`, `OPT101`, `OPT102`, `OPT103`)
// =============================================================================
fn compute_loop_carried(
    blocks: &[BasicBlock],
    loops: &[NaturalLoop],
    var_names: &[Option<String>],
) -> Vec<LoopCarriedInfo> {
    let mut out = Vec::new();
    for lp in loops {
        let body_set: FxHashSet<BlockId> = lp.body.iter().copied().collect();
        // Definitions inside the loop: copies and phis pass a value on, every other instruction
        // computes a new one from its operands (header phis are where values enter).
        let mut passes: FxHashMap<VarId, Vec<VarId>> = FxHashMap::default();
        let mut computes: FxHashMap<VarId, Vec<VarId>> = FxHashMap::default();
        let mut order: Vec<VarId> = Vec::new();
        for &bb_id in &lp.body {
            let bb = &blocks[bb_id.0 as usize];
            for phi in &bb.phis {
                if bb_id != lp.header {
                    let deps: Vec<VarId> = phi.incoming.iter().map(|(_, v)| *v).collect();
                    passes.insert(phi.dst, deps);
                }
            }
            for inst in &bb.insts {
                match &inst.kind {
                    InstKind::Copy { dst, src } => {
                        passes.insert(*dst, vec![*src]);
                    }
                    kind => {
                        if let Some(dst) = kind.dst() {
                            computes.insert(dst, kind.operands());
                            order.push(dst);
                        }
                    }
                }
            }
        }

        let mut deps: FxHashMap<VarId, Vec<VarId>> = passes.clone();
        deps.extend(computes.iter().map(|(v, ops)| (*v, ops.clone())));

        let header = &blocks[lp.header.0 as usize];
        for phi in &header.phis {
            let mut init_vars = Vec::new();
            let mut back_vars = Vec::new();
            for &(pred, v) in &phi.incoming {
                if body_set.contains(&pred) {
                    if v != phi.dst && !back_vars.contains(&v) {
                        back_vars.push(v);
                    }
                } else if !init_vars.contains(&v) {
                    init_vars.push(v);
                }
            }
            if back_vars.is_empty() {
                continue;
            }
            let updates =
                accumulating_updates(&back_vars, phi.dst, &passes, &computes, &deps, &order);
            let var_name = var_names.get(phi.dst.0 as usize).and_then(|o| o.clone());
            out.push(LoopCarriedInfo {
                loop_header: lp.header,
                phi_var: phi.dst,
                var_name,
                init_vars,
                back_vars,
                self_accumulating: !updates.is_empty(),
                updates,
            });
        }
    }
    out
}

/// The computed values that reach the back edge (`back_vars`) through copies and phis only and
/// depend on `phi` (in `order`, the block order of their definitions). A path that only passes
/// `phi` itself on (a branch leaving the variable unchanged) contributes nothing.
fn accumulating_updates(
    back_vars: &[VarId],
    phi: VarId,
    passes: &FxHashMap<VarId, Vec<VarId>>,
    computes: &FxHashMap<VarId, Vec<VarId>>,
    deps: &FxHashMap<VarId, Vec<VarId>>,
    order: &[VarId],
) -> Vec<VarId> {
    // The values the back edge may carry: through copies and phis to computed values.
    let mut sources: FxHashSet<VarId> = FxHashSet::default();
    let mut seen: FxHashSet<VarId> = back_vars.iter().copied().collect();
    let mut queue: VecDeque<VarId> = back_vars.iter().copied().collect();
    while let Some(v) = queue.pop_front() {
        if let Some(from) = passes.get(&v) {
            for &x in from {
                if seen.insert(x) {
                    queue.push_back(x);
                }
            }
        } else if computes.contains_key(&v) {
            sources.insert(v);
        }
    }
    order
        .iter()
        .copied()
        .filter(|v| sources.contains(v))
        .filter(|v| reaches_var(&computes[v], phi, deps))
        .collect()
}

/// Whether `target` is one of `starts` or reachable from them through `deps` (breadth-first).
fn reaches_var(starts: &[VarId], target: VarId, deps: &FxHashMap<VarId, Vec<VarId>>) -> bool {
    let mut visited: FxHashSet<VarId> = starts.iter().copied().collect();
    let mut queue: VecDeque<VarId> = starts.iter().copied().collect();
    while let Some(cur) = queue.pop_front() {
        if cur == target {
            return true;
        }
        for &n in deps.get(&cur).into_iter().flatten() {
            if visited.insert(n) {
                queue.push_back(n);
            }
        }
    }
    false
}

/// Follow trivial-phi replacements (`phi -> value`) to the surviving SSA value. The chains are
/// acyclic by construction (a phi is only replaced by a value that is not itself replaced at
/// that point); the step bound only guards against a corrupted map.
fn resolve_replacement(map: &FxHashMap<VarId, VarId>, mut v: VarId) -> VarId {
    let mut steps = 0usize;
    while let Some(&next) = map.get(&v) {
        if next == v || steps > 64 {
            break;
        }
        v = next;
        steps += 1;
    }
    v
}

// =============================================================================
// Loop Bound Classification (Advisory & Profitability ONLY)
// =============================================================================
/// The bound of a `for` loop from its syntax, and whether its body is a retry body (a fixed
/// trip count found later makes it a retry loop too, `facts::refine_loops`).
fn classify_for_bound(f: &ast::StmtFor) -> (LoopBound, bool) {
    let iter_bound = classify_iter_expr_bound(&f.iter);
    let retry_body = is_retry_body(&f.body);
    if retry_body
        && (matches!(iter_bound, LoopBound::ConstSmall(_)) || target_looks_like_retry(&f.target))
    {
        return (LoopBound::RetryPattern, retry_body);
    }
    (iter_bound, retry_body)
}

/// A literal display of at most [`SMALL_MAX`] items without `*` unpacking.
fn fixed_display(elts: &[Expr]) -> Option<LoopBound> {
    let len = u32::try_from(elts.len()).ok().filter(|n| *n <= SMALL_MAX)?;
    (!elts.iter().any(|e| matches!(e, Expr::Starred(_)))).then_some(LoopBound::ConstSmall(len))
}

/// The bound a `for` iterable's syntax fixes: a small display. Everything a name decides
/// (`range(...)`, which a local or a module may rebind, a name bound to a display, a builtin
/// wrapper) is left to [`crate::facts::refine_loops`], which resolves the names.
fn classify_iter_expr_bound(iter: &Expr) -> LoopBound {
    match iter {
        Expr::Tuple(t) => fixed_display(&t.elts).unwrap_or(LoopBound::Unbounded),
        Expr::List(l) => fixed_display(&l.elts).unwrap_or(LoopBound::Unbounded),
        Expr::Set(s) => fixed_display(&s.elts).unwrap_or(LoopBound::Unbounded),
        _ => LoopBound::Unbounded,
    }
}

fn classify_while_bound(w: &ast::StmtWhile) -> LoopBound {
    let (flag, looks_like_flag): (&Expr, fn(&str) -> bool) = match w.test.as_ref() {
        Expr::UnaryOp(u) if matches!(u.op, ast::UnaryOp::Not) => (&u.operand, looks_like_stop_flag),
        test => (test, looks_like_daemon_flag),
    };
    let flag = tested_event(flag).unwrap_or(flag);
    let named = match flag {
        Expr::Attribute(a) => looks_like_flag(a.attr.as_str()),
        Expr::Name(n) => looks_like_flag(n.id.as_str()),
        _ => false,
    };
    if named && !drains(&w.body, flag) {
        return LoopBound::DaemonPolling;
    }
    if is_retry_body(&w.body) && has_sleep_call(&w.body) {
        if matches!(w.test.as_ref(), Expr::BooleanLiteral(b) if b.value) {
            return LoopBound::DaemonPolling;
        }
        return LoopBound::RetryPattern;
    }
    LoopBound::Unbounded
}

/// The event a loop test reads as its flag: `stop` of `stop.is_set()`, or of `stop.wait(1)`,
/// which waits for the event and then tests it.
fn tested_event(test: &Expr) -> Option<&Expr> {
    match test {
        Expr::Call(c) => match c.func.as_ref() {
            Expr::Attribute(a) if matches!(a.attr.as_str(), "is_set" | "wait") => Some(&a.value),
            _ => None,
        },
        _ => None,
    }
}

/// Whether `body` reads a name the `for` target binds (also in a scope nested in it). A target
/// that binds an attribute or an item counts as read: its reads are not followed.
fn reads_target(body: &[Stmt], target: &Expr) -> bool {
    use heki_ast::raw_ast::visitor::{self, Visitor};

    struct Reads<'t> {
        bound: Vec<&'t str>,
        found: bool,
    }
    impl<'e> Visitor<'e> for Reads<'_> {
        fn visit_expr(&mut self, expr: &'e Expr) {
            if let Expr::Name(n) = expr
                && n.ctx == ast::ExprContext::Load
                && self.bound.contains(&n.id.as_str())
            {
                self.found = true;
            }
            visitor::walk_expr(self, expr);
        }
    }

    let mut bound = Vec::new();
    if !target_names(target, &mut bound) {
        return true;
    }
    let mut v = Reads {
        bound,
        found: false,
    };
    v.visit_body(body);
    v.found
}

/// The names a `for` target binds (`i` of `for i, (a, *b) in ...` and the others), and whether
/// it binds nothing else (no attribute or item).
fn target_names<'t>(target: &'t Expr, out: &mut Vec<&'t str>) -> bool {
    match target {
        Expr::Name(n) => {
            out.push(n.id.as_str());
            true
        }
        Expr::Tuple(ast::ExprTuple { elts, .. }) | Expr::List(ast::ExprList { elts, .. }) => {
            elts.iter().all(|e| target_names(e, out))
        }
        Expr::Starred(s) => target_names(&s.value, out),
        _ => false,
    }
}

/// Whether `body` takes items out of the collection `tested` names (`while queue: item =
/// queue.pop()`): a loop over its items, however its name reads, not a flag loop.
fn drains(body: &[Stmt], tested: &Expr) -> bool {
    use heki_ast::raw_ast::visitor::{self, Visitor};

    struct Drains<'t> {
        tested: Option<String>,
        found: bool,
        _expr: std::marker::PhantomData<&'t ()>,
    }
    impl Drains<'_> {
        fn is_tested(&self, e: &Expr) -> bool {
            self.tested.is_some() && expr_dotted_name(e) == self.tested
        }
    }
    impl<'e> Visitor<'e> for Drains<'_> {
        fn visit_stmt(&mut self, stmt: &'e Stmt) {
            if let Stmt::For(f) = stmt
                && self.is_tested(&f.iter)
            {
                self.found = true;
            }
            visitor::walk_stmt(self, stmt);
        }
        fn visit_expr(&mut self, expr: &'e Expr) {
            match expr {
                Expr::Subscript(sub) if self.is_tested(&sub.value) => self.found = true,
                Expr::Call(call) => {
                    if let Expr::Attribute(m) = call.func.as_ref()
                        && self.is_tested(&m.value)
                        && matches!(
                            m.attr.as_str(),
                            "pop" | "popleft" | "popitem" | "remove" | "clear" | "get_nowait"
                        )
                    {
                        self.found = true;
                    }
                }
                _ => {}
            }
            visitor::walk_expr(self, expr);
        }
    }

    let mut v = Drains {
        tested: expr_dotted_name(tested),
        found: false,
        _expr: std::marker::PhantomData,
    };
    v.visit_body(body);
    v.found
}

/// The single comparisons with a `len(...)` operand a `while` test requires (the test itself
/// or one operand of a top-level `and`): `facts` decides which of them cap a length.
fn len_cap_tests(test: &Expr) -> Vec<Span> {
    let conjuncts: Vec<&Expr> = match test {
        Expr::BoolOp(b) if matches!(b.op, ast::BoolOp::And) => b.values.iter().collect(),
        other => vec![other],
    };
    let is_len = |e: &Expr| {
        matches!(e, Expr::Call(c) if matches!(c.func.as_ref(), Expr::Name(n) if n.id.as_str() == "len")
            && c.arguments.args.len() == 1
            && c.arguments.keywords.is_empty())
    };
    conjuncts
        .into_iter()
        .filter_map(|e| match e {
            Expr::Compare(c)
                if c.ops.len() == 1 && (is_len(&c.operands[0]) || is_len(&c.operands[1])) =>
            {
                Some(Span::from(c.range))
            }
            _ => None,
        })
        .collect()
}

/// The words of an identifier, lowercase (`_isRunning` -> `is`, `running`).
fn identifier_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if (c == '_' || (c.is_ascii_uppercase() && prev_lower)) && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        if c != '_' {
            cur.push(c.to_ascii_lowercase());
        }
        prev_lower = c.is_ascii_lowercase();
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

fn looks_like_daemon_flag(name: &str) -> bool {
    identifier_words(name)
        .iter()
        .any(|w| matches!(w.as_str(), "running" | "active" | "alive"))
}

fn looks_like_stop_flag(name: &str) -> bool {
    identifier_words(name).iter().any(|w| {
        matches!(
            w.as_str(),
            "stop" | "stopped" | "stopping" | "closed" | "closing" | "shutdown"
        )
    })
}

/// A target that names an attempt: `attempt`, `retry_count`, or one name of a tuple target
/// (`for attempt, proxy in enumerate(proxies)`).
fn target_looks_like_retry(target: &Expr) -> bool {
    let mut names = Vec::new();
    target_names(target, &mut names);
    names.iter().any(|n| {
        let lower = n.to_ascii_lowercase();
        lower.contains("retry") || lower.contains("retries") || lower.contains("attempt")
    })
}

fn is_retry_body(body: &[Stmt]) -> bool {
    body.iter().any(|stmt| {
        if let Stmt::Try(t) = stmt {
            let exits_on_success =
                suite_has_return_or_break(&t.body) || suite_has_return_or_break(&t.orelse);
            let sleeps_in_handler = t.handlers.iter().any(|h| {
                let ast::ExceptHandler::ExceptHandler(eh) = h;
                has_sleep_call(&eh.body)
            });
            exits_on_success || sleeps_in_handler
        } else {
            false
        }
    })
}

fn suite_has_return_or_break(stmts: &[Stmt]) -> bool {
    for s in stmts {
        match s {
            Stmt::Return(_) | Stmt::Break(_) => return true,
            Stmt::If(i) => {
                if suite_has_return_or_break(&i.body)
                    || i.elif_else_clauses
                        .iter()
                        .any(|c| suite_has_return_or_break(&c.body))
                {
                    return true;
                }
            }
            Stmt::With(w) if suite_has_return_or_break(&w.body) => {
                return true;
            }
            _ => {}
        }
    }
    false
}

fn has_sleep_call(stmts: &[Stmt]) -> bool {
    for s in stmts {
        match s {
            Stmt::Expr(e) => {
                if expr_has_sleep(&e.value) {
                    return true;
                }
            }
            Stmt::If(i) => {
                if has_sleep_call(&i.body)
                    || i.elif_else_clauses.iter().any(|c| has_sleep_call(&c.body))
                {
                    return true;
                }
            }
            Stmt::Try(t)
                if (has_sleep_call(&t.body)
                    || t.handlers.iter().any(|h| {
                        let ast::ExceptHandler::ExceptHandler(eh) = h;
                        has_sleep_call(&eh.body)
                    })) =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

fn expr_has_sleep(expr: &Expr) -> bool {
    match expr {
        Expr::Await(a) => expr_has_sleep(&a.value),
        Expr::Call(c) => {
            if let Some(d) = expr_dotted_name(&c.func) {
                d == "sleep" || d.ends_with(".sleep")
            } else {
                false
            }
        }
        _ => false,
    }
}

// =============================================================================
// Single-Scope Pre-Pass Binding Collector
// =============================================================================
fn collect_params_into(params: &ast::Parameters, locals: &mut FxHashSet<String>) {
    for p in params
        .posonlyargs
        .iter()
        .chain(params.args.iter())
        .chain(params.kwonlyargs.iter())
    {
        locals.insert(p.parameter.name.to_string());
    }
    if let Some(v) = &params.vararg {
        locals.insert(v.name.to_string());
    }
    if let Some(k) = &params.kwarg {
        locals.insert(k.name.to_string());
    }
}

/// The default values of a parameter list, in the order the definition evaluates them
/// (positional ones, then keyword-only ones).
fn parameter_defaults(params: &ast::Parameters) -> impl Iterator<Item = &Expr> {
    params
        .posonlyargs
        .iter()
        .chain(&params.args)
        .chain(&params.kwonlyargs)
        .filter_map(|p| p.default.as_deref())
}

/// A function's annotations in the order the definition evaluates them: the parameters'
/// (positional, `*args`, keyword-only, `**kwargs`), then the return annotation.
fn function_annotations<'e>(
    params: &'e ast::Parameters,
    returns: Option<&'e Expr>,
) -> Vec<&'e Expr> {
    let with_default = |p: &'e ast::ParameterWithDefault| p.parameter.annotation.as_deref();
    let plain =
        |p: &'e Option<Box<ast::Parameter>>| p.as_deref().and_then(|p| p.annotation.as_deref());
    params
        .posonlyargs
        .iter()
        .chain(&params.args)
        .filter_map(with_default)
        .chain(plain(&params.vararg))
        .chain(params.kwonlyargs.iter().filter_map(with_default))
        .chain(plain(&params.kwarg))
        .chain(returns)
        .collect()
}

/// The names an expression loads, also inside the scopes nested in it: what a lazily evaluated
/// scope built from it (an `__annotate__` function, a type parameter's bound, a type alias's
/// value) reads from the frame it is defined in.
fn names_loaded(exprs: &[&Expr]) -> Vec<String> {
    use heki_ast::raw_ast::visitor::{self, Visitor};

    struct Loads(Vec<String>);
    impl<'e> Visitor<'e> for Loads {
        fn visit_expr(&mut self, expr: &'e Expr) {
            if let Expr::Name(n) = expr
                && n.ctx == ast::ExprContext::Load
                && !self.0.iter().any(|seen| seen == n.id.as_str())
            {
                self.0.push(n.id.to_string());
            }
            visitor::walk_expr(self, expr);
        }
    }

    let mut loads = Loads(Vec::new());
    for e in exprs {
        loads.visit_expr(e);
    }
    loads.0
}

fn collect_suite_bindings(stmts: &[Stmt], out: &mut ScopeBindings) {
    for stmt in stmts {
        match stmt {
            // Decorators, defaults, and class arguments run in this scope at the definition.
            Stmt::FunctionDef(f) => {
                for dec in &f.decorator_list {
                    collect_expr_bindings(&dec.expression, out);
                }
                for default in parameter_defaults(&f.parameters) {
                    collect_expr_bindings(default, out);
                }
                out.locals.insert(f.name.to_string());
            }
            Stmt::ClassDef(c) => {
                for dec in &c.decorator_list {
                    collect_expr_bindings(&dec.expression, out);
                }
                if let Some(args) = &c.arguments {
                    for arg in &args.args {
                        collect_expr_bindings(arg, out);
                    }
                    for kw in &args.keywords {
                        collect_expr_bindings(&kw.value, out);
                    }
                }
                out.locals.insert(c.name.to_string());
            }
            Stmt::TypeAlias(ta) => collect_target_bindings(&ta.name, out),
            Stmt::Assign(a) => {
                for t in &a.targets {
                    collect_target_bindings(t, out);
                }
                collect_expr_bindings(&a.value, out);
            }
            Stmt::AugAssign(aa) => {
                collect_target_bindings(&aa.target, out);
                collect_expr_bindings(&aa.value, out);
            }
            Stmt::AnnAssign(aa) => {
                if aa.simple || aa.value.is_some() {
                    collect_target_bindings(&aa.target, out);
                }
                if let Some(v) = &aa.value {
                    collect_expr_bindings(v, out);
                }
            }
            Stmt::Delete(d) => {
                for t in &d.targets {
                    collect_target_bindings(t, out);
                }
            }
            Stmt::For(f) => {
                collect_expr_bindings(&f.iter, out);
                collect_target_bindings(&f.target, out);
                collect_suite_bindings(&f.body, out);
                collect_suite_bindings(&f.orelse, out);
            }
            Stmt::While(w) => {
                collect_expr_bindings(&w.test, out);
                collect_suite_bindings(&w.body, out);
                collect_suite_bindings(&w.orelse, out);
            }
            Stmt::If(i) => {
                collect_expr_bindings(&i.test, out);
                collect_suite_bindings(&i.body, out);
                for c in &i.elif_else_clauses {
                    if let Some(t) = &c.test {
                        collect_expr_bindings(t, out);
                    }
                    collect_suite_bindings(&c.body, out);
                }
            }
            Stmt::With(w) => {
                for item in &w.items {
                    collect_expr_bindings(&item.context_expr, out);
                    if let Some(opt) = &item.optional_vars {
                        collect_target_bindings(opt, out);
                    }
                }
                collect_suite_bindings(&w.body, out);
            }
            Stmt::Try(t) => {
                collect_suite_bindings(&t.body, out);
                for h in &t.handlers {
                    let ast::ExceptHandler::ExceptHandler(eh) = h;
                    if let Some(ty) = &eh.type_ {
                        collect_expr_bindings(ty, out);
                    }
                    if let Some(name) = &eh.name {
                        out.locals.insert(name.to_string());
                    }
                    collect_suite_bindings(&eh.body, out);
                }
                collect_suite_bindings(&t.orelse, out);
                collect_suite_bindings(&t.finalbody, out);
            }
            Stmt::Match(m) => {
                collect_expr_bindings(&m.subject, out);
                for case in &m.cases {
                    collect_pattern_bindings(&case.pattern, &mut out.locals);
                    if let Some(g) = &case.guard {
                        collect_expr_bindings(g, out);
                    }
                    collect_suite_bindings(&case.body, out);
                }
            }
            Stmt::Import(imp) => {
                for alias in &imp.names {
                    let bound = alias
                        .asname
                        .as_ref()
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| alias.name.split('.').next().unwrap_or("").to_string());
                    out.locals.insert(bound);
                }
            }
            Stmt::ImportFrom(imp) => {
                for alias in &imp.names {
                    if alias.name.as_str() == "*" {
                        continue;
                    }
                    let bound = alias
                        .asname
                        .as_ref()
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| alias.name.to_string());
                    out.locals.insert(bound);
                }
            }
            Stmt::Global(g) => {
                for n in &g.names {
                    out.globals.insert(n.to_string());
                }
            }
            Stmt::Nonlocal(nl) => {
                for n in &nl.names {
                    out.nonlocals.insert(n.to_string());
                }
            }
            Stmt::Return(r) => {
                if let Some(v) = &r.value {
                    collect_expr_bindings(v, out);
                }
            }
            Stmt::Raise(r) => {
                if let Some(e) = &r.exc {
                    collect_expr_bindings(e, out);
                }
                if let Some(c) = &r.cause {
                    collect_expr_bindings(c, out);
                }
            }
            Stmt::Assert(a) => {
                collect_expr_bindings(&a.test, out);
                if let Some(m) = &a.msg {
                    collect_expr_bindings(m, out);
                }
            }
            Stmt::Expr(e) => collect_expr_bindings(&e.value, out),
            Stmt::Pass(_) | Stmt::Break(_) | Stmt::Continue(_) | Stmt::IpyEscapeCommand(_) => {}
        }
    }
}

/// The names an assignment target binds, and the bindings of the expressions it evaluates
/// (`d[(k := key)] = v` binds `k`).
fn collect_target_bindings(expr: &Expr, out: &mut ScopeBindings) {
    match expr {
        Expr::Name(n) => {
            out.locals.insert(n.id.to_string());
        }
        Expr::Tuple(ast::ExprTuple { elts, .. }) | Expr::List(ast::ExprList { elts, .. }) => {
            for e in elts {
                collect_target_bindings(e, out);
            }
        }
        Expr::Starred(s) => collect_target_bindings(&s.value, out),
        other => collect_expr_bindings(other, out),
    }
}

/// The names an assignment target binds, without the expressions it evaluates.
fn collect_target_names(expr: &Expr, names: &mut FxHashSet<String>) {
    match expr {
        Expr::Name(n) => {
            names.insert(n.id.to_string());
        }
        Expr::Tuple(ast::ExprTuple { elts, .. }) | Expr::List(ast::ExprList { elts, .. }) => {
            for e in elts {
                collect_target_names(e, names);
            }
        }
        Expr::Starred(s) => collect_target_names(&s.value, names),
        _ => {}
    }
}

fn collect_pattern_bindings(pat: &Pattern, locals: &mut FxHashSet<String>) {
    match pat {
        Pattern::MatchSequence(ms) => {
            for p in &ms.patterns {
                collect_pattern_bindings(p, locals);
            }
        }
        Pattern::MatchMapping(mm) => {
            for p in &mm.patterns {
                collect_pattern_bindings(p, locals);
            }
            if let Some(r) = &mm.rest {
                locals.insert(r.to_string());
            }
        }
        Pattern::MatchClass(mc) => {
            for p in &mc.arguments.patterns {
                collect_pattern_bindings(p, locals);
            }
            for kw in &mc.arguments.keywords {
                collect_pattern_bindings(&kw.pattern, locals);
            }
        }
        Pattern::MatchStar(ms) => {
            if let Some(n) = &ms.name {
                locals.insert(n.to_string());
            }
        }
        Pattern::MatchAs(ma) => {
            if let Some(p) = &ma.pattern {
                collect_pattern_bindings(p, locals);
            }
            if let Some(n) = &ma.name {
                locals.insert(n.to_string());
            }
        }
        Pattern::MatchOr(mo) => {
            for p in &mo.patterns {
                collect_pattern_bindings(p, locals);
            }
        }
        Pattern::MatchValue(_) | Pattern::MatchSingleton(_) => {}
    }
}

/// The bindings of the parts of a comprehension that run in its own scope: every `for` clause
/// but the first one's iterable (which runs in the enclosing scope), the conditions, and the
/// elements.
fn collect_comprehension_bindings(
    generators: &[ast::Comprehension],
    elts: &[&Expr],
    out: &mut ScopeBindings,
) {
    for (idx, g) in generators.iter().enumerate() {
        if idx > 0 {
            collect_expr_bindings(&g.iter, out);
        }
        collect_target_bindings(&g.target, out);
        for cond in &g.ifs {
            collect_expr_bindings(cond, out);
        }
    }
    for elt in elts {
        collect_expr_bindings(elt, out);
    }
}

fn collect_expr_bindings(expr: &Expr, out: &mut ScopeBindings) {
    match expr {
        Expr::Named(n) => {
            collect_target_bindings(&n.target, out);
            collect_target_names(&n.target, &mut out.assignment_expression_targets);
            collect_expr_bindings(&n.value, out);
        }
        Expr::Yield(y) => {
            out.has_yield = true;
            if let Some(v) = &y.value {
                collect_expr_bindings(v, out);
            }
        }
        Expr::YieldFrom(yf) => {
            out.has_yield = true;
            collect_expr_bindings(&yf.value, out);
        }
        // A lambda's body is a scope of its own; its defaults run here.
        Expr::Lambda(lam) => {
            if let Some(params) = &lam.parameters {
                for default in parameter_defaults(params) {
                    collect_expr_bindings(default, out);
                }
            }
        }
        // A generator expression's outermost iterable runs here, and an assignment expression
        // anywhere else in it binds here too (PEP 572); its other names are its own.
        Expr::Generator(ge) => {
            collect_expr_bindings(&ge.generators[0].iter, out);
            let mut own = ScopeBindings::default();
            collect_comprehension_bindings(&ge.generators, &[&ge.elt], &mut own);
            for name in own.assignment_expression_targets {
                out.locals.insert(name.clone());
                out.assignment_expression_targets.insert(name);
            }
        }
        // Inlined into this scope (PEP 709) apart from their iteration variables, which stay
        // isolated; before 3.12 an assignment expression in one binds here all the same.
        Expr::ListComp(lc) => collect_inlined_comprehension(&lc.generators, &[&lc.elt], out),
        Expr::SetComp(sc) => collect_inlined_comprehension(&sc.generators, &[&sc.elt], out),
        Expr::DictComp(dc) => {
            let elts: Vec<&Expr> = dc.key.as_deref().into_iter().chain([&*dc.value]).collect();
            collect_inlined_comprehension(&dc.generators, &elts, out);
        }
        Expr::BoolOp(b) => {
            for v in &b.values {
                collect_expr_bindings(v, out);
            }
        }
        Expr::BinOp(b) => {
            collect_expr_bindings(&b.left, out);
            collect_expr_bindings(&b.right, out);
        }
        Expr::UnaryOp(u) => collect_expr_bindings(&u.operand, out),
        Expr::If(i) => {
            collect_expr_bindings(&i.test, out);
            collect_expr_bindings(&i.body, out);
            collect_expr_bindings(&i.orelse, out);
        }
        Expr::Dict(d) => {
            for item in &d.items {
                if let Some(k) = &item.key {
                    collect_expr_bindings(k, out);
                }
                collect_expr_bindings(&item.value, out);
            }
        }
        Expr::Set(ast::ExprSet { elts, .. })
        | Expr::List(ast::ExprList { elts, .. })
        | Expr::Tuple(ast::ExprTuple { elts, .. }) => {
            for e in elts {
                collect_expr_bindings(e, out);
            }
        }
        Expr::Await(a) => collect_expr_bindings(&a.value, out),
        Expr::Compare(c) => {
            for operand in &c.operands {
                collect_expr_bindings(operand, out);
            }
        }
        Expr::Call(c) => {
            collect_expr_bindings(&c.func, out);
            for a in &c.arguments.args {
                collect_expr_bindings(a, out);
            }
            for kw in &c.arguments.keywords {
                collect_expr_bindings(&kw.value, out);
            }
        }
        Expr::Attribute(a) => collect_expr_bindings(&a.value, out),
        Expr::Subscript(s) => {
            collect_expr_bindings(&s.value, out);
            collect_expr_bindings(&s.slice, out);
        }
        Expr::Starred(s) => collect_expr_bindings(&s.value, out),
        Expr::FString(_) | Expr::TString(_) => {
            for field in heki_ast::interpolated_expressions(expr) {
                collect_expr_bindings(field, out);
            }
        }
        Expr::Slice(s) => {
            for part in [&s.lower, &s.upper, &s.step].into_iter().flatten() {
                collect_expr_bindings(part, out);
            }
        }
        Expr::Name(_)
        | Expr::StringLiteral(_)
        | Expr::BytesLiteral(_)
        | Expr::NumberLiteral(_)
        | Expr::BooleanLiteral(_)
        | Expr::NoneLiteral(_)
        | Expr::EllipsisLiteral(_)
        | Expr::IpyEscapeCommand(_) => {}
    }
}

/// The bindings of a list, set, or dict comprehension lowered into this scope: everything but
/// its iteration variables (isolated per PEP 709, a scope of their own before 3.12).
fn collect_inlined_comprehension(
    generators: &[ast::Comprehension],
    elts: &[&Expr],
    out: &mut ScopeBindings,
) {
    collect_expr_bindings(&generators[0].iter, out);
    let mut own = ScopeBindings::default();
    collect_comprehension_bindings(generators, elts, &mut own);
    let mut targets = FxHashSet::default();
    for g in generators {
        collect_target_names(&g.target, &mut targets);
    }
    own.locals.retain(|n| !targets.contains(n));
    out.locals.extend(own.locals);
    out.globals.extend(own.globals);
    out.nonlocals.extend(own.nonlocals);
    out.assignment_expression_targets
        .extend(own.assignment_expression_targets);
    out.has_yield |= own.has_yield;
}

// =============================================================================
// Helpers
// =============================================================================
/// The `A1Hazard` kind of a frame-introspection function (Assumption A1(a)) by its spelling: a
/// builtin's name, or `module.attr`. `vars` reads the caller's locals only without an argument.
fn introspection_kind(spelling: &str, without_args: bool) -> Option<&'static str> {
    match spelling {
        "eval" | "builtins.eval" => Some("eval"),
        "exec" | "builtins.exec" => Some("exec"),
        "locals" | "builtins.locals" => Some("locals"),
        "vars" | "builtins.vars" if without_args => Some("vars()"),
        "sys._getframe" | "_getframe" => Some("sys._getframe"),
        "inspect.currentframe" => Some("inspect.currentframe"),
        "gc.get_objects" => Some("gc.get_objects"),
        "gc.get_referrers" => Some("gc.get_referrers"),
        "ctypes.pythonapi" => Some("ctypes.pythonapi"),
        _ if spelling.ends_with("._getframe") => Some("sys._getframe"),
        _ if spelling.ends_with(".currentframe") => Some("inspect.currentframe"),
        _ if spelling.ends_with(".get_objects") && spelling.contains("gc.") => {
            Some("gc.get_objects")
        }
        _ => None,
    }
}

fn expr_dotted_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Name(n) => Some(n.id.to_string()),
        Expr::Attribute(a) => Some(format!("{}.{}", expr_dotted_name(&a.value)?, a.attr)),
        _ => None,
    }
}

fn extract_const_str(expr: &Expr) -> Option<String> {
    match expr {
        Expr::StringLiteral(s) => Some(s.value.to_str().to_string()),
        _ => None,
    }
}

/// The literal a parameter default is (`None`, `False`, `-1`, `0.5`, `""`, `b""`).
fn literal_default(expr: &Expr) -> Option<ConstLiteral> {
    match expr {
        Expr::NoneLiteral(_) => Some(ConstLiteral::None),
        Expr::BooleanLiteral(b) => Some(ConstLiteral::Bool(b.value)),
        Expr::StringLiteral(s) => Some(ConstLiteral::Str(s.value.to_str().to_string())),
        Expr::BytesLiteral(_) => Some(ConstLiteral::Bytes),
        Expr::NumberLiteral(ast::ExprNumberLiteral {
            value: ast::Number::Float(f),
            ..
        }) => Some(ConstLiteral::Float(f.to_string())),
        _ => extract_const_int(expr).map(ConstLiteral::Int),
    }
}

fn extract_const_int(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::NumberLiteral(n) => match &n.value {
            ast::Number::Int(i) => i.as_i64(),
            _ => None,
        },
        Expr::UnaryOp(u) if matches!(u.op, ast::UnaryOp::USub) => {
            extract_const_int(&u.operand).and_then(|v| v.checked_neg())
        }
        Expr::UnaryOp(u) if matches!(u.op, ast::UnaryOp::UAdd) => extract_const_int(&u.operand),
        _ => None,
    }
}

fn map_binop(op: ast::Operator) -> BinOpKind {
    match op {
        ast::Operator::Add => BinOpKind::Add,
        ast::Operator::Sub => BinOpKind::Sub,
        ast::Operator::Mult => BinOpKind::Mult,
        ast::Operator::Div => BinOpKind::Div,
        ast::Operator::FloorDiv => BinOpKind::FloorDiv,
        ast::Operator::Mod => BinOpKind::Mod,
        ast::Operator::Pow => BinOpKind::Pow,
        ast::Operator::BitOr => BinOpKind::BitOr,
        ast::Operator::BitAnd => BinOpKind::BitAnd,
        ast::Operator::BitXor => BinOpKind::BitXor,
        ast::Operator::LShift => BinOpKind::LShift,
        ast::Operator::RShift => BinOpKind::RShift,
        ast::Operator::MatMult => BinOpKind::MatMult,
    }
}

fn map_unaryop(op: ast::UnaryOp) -> UnaryOpKind {
    match op {
        ast::UnaryOp::Not => UnaryOpKind::Not,
        ast::UnaryOp::Invert => UnaryOpKind::Invert,
        ast::UnaryOp::UAdd => UnaryOpKind::UAdd,
        ast::UnaryOp::USub => UnaryOpKind::USub,
    }
}

fn map_cmpop(op: ast::CmpOp) -> CompareOpKind {
    match op {
        ast::CmpOp::Eq => CompareOpKind::Eq,
        ast::CmpOp::NotEq => CompareOpKind::NotEq,
        ast::CmpOp::Lt => CompareOpKind::Lt,
        ast::CmpOp::LtE => CompareOpKind::LtE,
        ast::CmpOp::Gt => CompareOpKind::Gt,
        ast::CmpOp::GtE => CompareOpKind::GtE,
        ast::CmpOp::Is => CompareOpKind::Is,
        ast::CmpOp::IsNot => CompareOpKind::IsNot,
        ast::CmpOp::In => CompareOpKind::In,
        ast::CmpOp::NotIn => CompareOpKind::NotIn,
    }
}

fn rewrite_inst_vars(kind: &mut InstKind, resolve: impl Fn(VarId) -> VarId) {
    match kind {
        InstKind::AllocBuiltin { elems: parts, .. } | InstKind::BuildString { parts, .. } => {
            for e in parts {
                *e = resolve(*e);
            }
        }
        InstKind::Format { value, spec, .. } => {
            *value = resolve(*value);
            if let Some(s) = spec {
                *s = resolve(*s);
            }
        }
        InstKind::Convert { value, .. } => *value = resolve(*value),
        InstKind::DictLiteral { entries, .. } => {
            for e in entries {
                match e {
                    DictEntry::KeyValue { key, value } => {
                        *key = resolve(*key);
                        *value = resolve(*value);
                    }
                    DictEntry::Unpack { dict } => {
                        *dict = resolve(*dict);
                    }
                }
            }
        }
        InstKind::StoreGlobal { value, .. }
        | InstKind::StoreDeref { value, .. }
        | InstKind::Copy { src: value, .. }
        | InstKind::UnaryOp { operand: value, .. }
        | InstKind::AttrLoad { obj: value, .. }
        | InstKind::AttrDelete { obj: value, .. }
        | InstKind::MakeIter {
            iterable: value, ..
        }
        | InstKind::IterNext { iter: value, .. }
        | InstKind::Await { value, .. }
        | InstKind::YieldFrom { value, .. }
        | InstKind::EnterWith { mgr: value, .. }
        | InstKind::ExitWith { mgr: value, .. } => {
            *value = resolve(*value);
        }
        InstKind::Delete { var: Some(v), .. } | InstKind::Yield { value: Some(v), .. } => {
            *v = resolve(*v);
        }
        InstKind::BinOp { lhs, rhs, .. } | InstKind::Compare { lhs, rhs, .. } => {
            *lhs = resolve(*lhs);
            *rhs = resolve(*rhs);
        }
        InstKind::AttrStore { obj, value, .. } => {
            *obj = resolve(*obj);
            *value = resolve(*value);
        }
        InstKind::SubscriptLoad { obj, index, .. }
        | InstKind::SubscriptDelete { obj, index, .. } => {
            *obj = resolve(*obj);
            *index = resolve(*index);
        }
        InstKind::SubscriptStore {
            obj, index, value, ..
        } => {
            *obj = resolve(*obj);
            *index = resolve(*index);
            *value = resolve(*value);
        }
        InstKind::MakeGenExpr {
            outer_iter,
            captured,
            ..
        } => {
            *outer_iter = resolve(*outer_iter);
            for c in captured {
                *c = resolve(*c);
            }
        }
        InstKind::MakeClosure { captured, .. } => {
            for c in captured {
                *c = resolve(*c);
            }
        }
        InstKind::Call {
            callee_var,
            args,
            kwargs,
            ..
        } => {
            if let Some(cv) = callee_var {
                *cv = resolve(*cv);
            }
            for a in args {
                *a = resolve(*a);
            }
            for (_, v) in kwargs {
                *v = resolve(*v);
            }
        }
        InstKind::MethodCall {
            recv, args, kwargs, ..
        } => {
            *recv = resolve(*recv);
            for a in args {
                *a = resolve(*a);
            }
            for (_, v) in kwargs {
                *v = resolve(*v);
            }
        }
        InstKind::Const { .. }
        | InstKind::LoadGlobal { .. }
        | InstKind::LoadDeref { .. }
        | InstKind::LoadName { .. }
        | InstKind::Delete { var: None, .. }
        | InstKind::Yield { value: None, .. }
        | InstKind::CatchException { .. }
        | InstKind::Import { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Invariant guard: replacement chains are acyclic by construction, but a corrupted map
    /// (a self-loop or a cycle) must still terminate instead of hanging the build.
    #[test]
    fn resolve_replacement_terminates_on_corrupted_maps() {
        let self_loop: FxHashMap<VarId, VarId> = [(VarId(1), VarId(1))].into_iter().collect();
        assert_eq!(resolve_replacement(&self_loop, VarId(1)), VarId(1));

        let cycle: FxHashMap<VarId, VarId> = [(VarId(1), VarId(2)), (VarId(2), VarId(1))]
            .into_iter()
            .collect();
        let end = resolve_replacement(&cycle, VarId(1));
        assert!(end == VarId(1) || end == VarId(2), "{end:?}");
    }
}
