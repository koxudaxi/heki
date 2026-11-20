use crate::facts::LoopHint;
use crate::{
    A1Hazard, BasicBlock, BinOpKind, BlockId, BuiltinKind, CompareOpKind, ConstLiteral, DictEntry,
    FunctionId, FunctionKind, Inst, InstKind, IrFunction, LocalAnnotation, LoopBound,
    LoopCarriedInfo, ModuleIr, NaturalLoop, ParamInfo, PhiNode, Terminator, UnaryOpKind, VarId,
};
use heki_ast::raw_ast::{self as ast, Expr, Pattern, Stmt};
use heki_ast::{ParsedModule, Ranged, Span};
use heki_env::RuntimeProfile;
use heki_semantic::{ClassId, SemanticModel};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::VecDeque;

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

    let mut ctx = ModuleLowerer {
        source,
        profile,
        class_by_span,
        lazy_import_names,
        functions: Vec::new(),
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
        body: FunctionBody::Suite(parsed.suite()),
    });

    let mut functions = ctx.functions;
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
    ModuleIr {
        functions,
        module_fn,
        has_future_annotations: semantic.has_future_annotations,
        has_star_import: ctx.has_star_import,
        constants,
    }
}

/// Give back the spare capacity that building by pushes leaves in a function's containers: a
/// module's IR stays resident until its file is checked, and about a third of its instruction,
/// block, and variable-name arrays was spare capacity.
fn release_spare_capacity(f: &mut IrFunction) {
    f.params.shrink_to_fit();
    f.decorators.shrink_to_fit();
    f.local_annotations.shrink_to_fit();
    f.var_names.shrink_to_fit();
    f.captured_vars.shrink_to_fit();
    f.starred.shrink_to_fit();
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
    body: FunctionBody<'a>,
}

struct ModuleLowerer<'a> {
    source: &'a str,
    profile: RuntimeProfile,
    class_by_span: FxHashMap<Span, (ClassId, String)>,
    lazy_import_names: FxHashSet<String>,
    functions: Vec<IrFunction>,
    /// Syntax-level facts of every function's loops (in `IrFunction::loops` order), for the
    /// loop-bound pass that runs once the whole module is lowered.
    loop_hints: Vec<(FunctionId, Vec<LoopHint>)>,
    next_comp_id: u32,
    has_star_import: bool,
    /// The end of the last `from m import *` (module constants bound before it may be rebound).
    last_star_import: Option<u32>,
}

impl<'a> ModuleLowerer<'a> {
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
        id
    }

    fn lower_function_body(&mut self, spec: FunctionSpec<'a>) -> FunctionId {
        let mut scope_info = ScopeBindings::default();
        if let Some(params) = spec.params {
            collect_params_into(params, &mut scope_info.locals);
        }
        match spec.body {
            FunctionBody::Suite(stmts) => collect_suite_bindings(stmts, &mut scope_info),
            FunctionBody::Expr(expr) => collect_expr_bindings(expr, &mut scope_info),
            FunctionBody::GenExpr(ge) => {
                scope_info.has_yield = true;
                for (idx, g) in ge.generators.iter().enumerate() {
                    if idx > 0 {
                        collect_expr_bindings(&g.iter, &mut scope_info);
                    }
                    collect_target_bindings(&g.target, &mut scope_info.locals);
                    for cond in &g.ifs {
                        collect_expr_bindings(cond, &mut scope_info);
                    }
                }
                collect_expr_bindings(&ge.elt, &mut scope_info);
            }
            FunctionBody::Comp {
                generators,
                elt,
                key,
                ..
            } => {
                for (idx, g) in generators.iter().enumerate() {
                    if idx > 0 {
                        collect_expr_bindings(&g.iter, &mut scope_info);
                    }
                    collect_target_bindings(&g.target, &mut scope_info.locals);
                    for cond in &g.ifs {
                        collect_expr_bindings(cond, &mut scope_info);
                    }
                }
                if let Some(k) = key {
                    collect_expr_bindings(k, &mut scope_info);
                }
                collect_expr_bindings(elt, &mut scope_info);
            }
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
            blocks: Vec::new(),
            loops: Vec::new(),
            loop_carried: Vec::new(),
            a1_hazards: Vec::new(),
            span: spec.span,
            is_module_body: spec.is_module_body,
            is_class_body: spec.is_class_body,
        };
        let fn_id = self.alloc_function_slot(placeholder);

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
                        let v = b.lower_expr(&ge.elt);
                        let dst = b.fresh_var(None);
                        b.emit(
                            InstKind::Yield {
                                dst,
                                value: Some(v),
                            },
                            Span::from(ge.elt.range()),
                        );
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
                        b.emit_comp_element(kind, acc, elt, key, spec.span);
                    },
                );
                builder.terminate_at(Terminator::Return(Some(acc)), spec.span);
            }
        }

        let ir_fn = builder.finish();
        self.functions[fn_id.0 as usize] = ir_fn;
        fn_id
    }
}

#[derive(Default)]
struct ScopeBindings {
    locals: FxHashSet<String>,
    globals: FxHashSet<String>,
    nonlocals: FxHashSet<String>,
    has_yield: bool,
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
    /// Variables in THIS function captured by nested functions/lambdas/genexprs.
    captured_by_children: FxHashSet<String>,
    /// Free variables read by THIS function from outer scopes.
    free_reads: FxHashSet<String>,

    blocks: Vec<BasicBlock>,
    terminated: Vec<bool>,
    preds: Vec<Vec<BlockId>>,
    sealed: Vec<bool>,
    cur_bb: BlockId,

    var_names: Vec<Option<String>>,
    current_def: FxHashMap<(BlockId, String), VarId>,
    incomplete_phis: FxHashMap<BlockId, Vec<(String, VarId)>>,
    var_replacements: FxHashMap<VarId, VarId>,

    exc_target_stack: Vec<BlockId>,
    loop_stack: Vec<ActiveLoop>,
    finished_loops: Vec<NaturalLoop>,
    /// `finished_loops[i]`'s syntax-level facts.
    finished_hints: Vec<LoopHint>,
    a1_hazards: Vec<A1Hazard>,
    /// `*x` operands of displays and calls (`IrFunction::starred`).
    starred: Vec<(VarId, u32)>,

    /// Lexical isolation map for PEP 709 inlined comprehension targets: `raw_name -> isolated_ssa_name`.
    comp_target_renames: Vec<FxHashMap<String, String>>,
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
            captured_by_children: FxHashSet::default(),
            free_reads: FxHashSet::default(),
            blocks: vec![entry],
            terminated: vec![false],
            preds: vec![Vec::new()],
            sealed: vec![true], // entry block has no predecessors
            cur_bb: BlockId(0),
            var_names: Vec::new(),
            current_def: FxHashMap::default(),
            incomplete_phis: FxHashMap::default(),
            var_replacements: FxHashMap::default(),
            exc_target_stack: Vec::new(),
            loop_stack: Vec::new(),
            finished_loops: Vec::new(),
            finished_hints: Vec::new(),
            a1_hazards: Vec::new(),
            starred: Vec::new(),
            comp_target_renames: Vec::new(),
        }
    }

    fn fresh_var(&mut self, name: Option<String>) -> VarId {
        let id = VarId(self.var_names.len() as u32);
        self.var_names.push(name);
        id
    }

    fn new_block(&mut self, sealed: bool) -> BlockId {
        let id = BlockId(self.blocks.len() as u32);
        let exc_target = self.exc_target_stack.last().copied();
        self.blocks.push(BasicBlock {
            id,
            phis: Vec::new(),
            insts: Vec::new(),
            terminator: Terminator::Unreachable,
            exc_target,
            term_span: None,
        });
        self.terminated.push(false);
        self.preds.push(Vec::new());
        self.sealed.push(sealed);
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
    }

    fn is_terminated(&self, bb: BlockId) -> bool {
        self.terminated[bb.0 as usize]
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
        self.ensure_open_block();
        let bb = self.cur_bb;
        if let Some(exc) = self.exc_target_stack.last().copied() {
            self.blocks[bb.0 as usize].exc_target = Some(exc);
            self.add_edge(bb, exc);
        }
        self.blocks[bb.0 as usize].insts.push(Inst { kind, span });
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
        self.current_def.insert((bb, name.to_string()), val);
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
        self.read_local_recursive(name, bb, span)
    }

    fn read_local_recursive(&mut self, name: &str, bb: BlockId, span: Span) -> VarId {
        let val = if !self.sealed[bb.0 as usize] {
            let phi_var = self.fresh_var(Some(name.to_string()));
            self.incomplete_phis
                .entry(bb)
                .or_default()
                .push((name.to_string(), phi_var));
            phi_var
        } else {
            let preds = self.preds[bb.0 as usize].clone();
            if preds.is_empty() {
                let dst = self.fresh_var(Some(name.to_string()));
                self.blocks[bb.0 as usize].insts.push(Inst {
                    kind: InstKind::LoadName {
                        dst,
                        name: name.to_string(),
                    },
                    span,
                });
                dst
            } else if preds.len() == 1 {
                self.read_local_ssa(name, preds[0], span)
            } else {
                let phi_var = self.fresh_var(Some(name.to_string()));
                // Break cycles before recursing into predecessors.
                self.write_local(name, bb, phi_var);
                self.add_phi_operands(bb, name, phi_var, span)
            }
        };
        self.write_local(name, bb, val);
        val
    }

    fn add_phi_operands(&mut self, bb: BlockId, name: &str, phi_var: VarId, span: Span) -> VarId {
        let preds = self.preds[bb.0 as usize].clone();
        let mut incoming = Vec::with_capacity(preds.len());
        for p in preds {
            let v = self.read_local_ssa(name, p, span);
            incoming.push((p, v));
        }

        // Check triviality: all operands are either `phi_var` itself or one single `same` value.
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
        if trivial
            && let Some(s) = same
            && s != phi_var
        {
            self.var_replacements.insert(phi_var, s);
            return s;
        }

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
            for (name, phi_var) in pending {
                self.add_phi_operands(bb, &name, phi_var, self.span);
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
        self.ensure_open_block();
        match stmt {
            Stmt::FunctionDef(func) => self.lower_function_def(func),
            Stmt::ClassDef(cls) => self.lower_class_def(cls),
            Stmt::Return(r) => {
                let val = r.value.as_ref().map(|e| self.lower_expr(e));
                self.terminate_at(Terminator::Return(val), Span::from(r.range));
            }
            Stmt::Delete(d) => {
                for target in &d.targets {
                    self.lower_delete_target(target);
                }
            }
            Stmt::TypeAlias(ta) => {
                let span = Span::from(ta.range);
                let v = self.emit_const(ConstLiteral::Other, span);
                self.lower_assign_target(&ta.name, v, span);
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
                    self.terminate(Terminator::Jump(exit_bb));
                }
            }
            Stmt::Continue(_) => {
                if let Some(header_bb) = self.loop_stack.last().map(|l| l.header) {
                    let cur = self.cur_bb;
                    if let Some(top) = self.loop_stack.last_mut() {
                        top.back_edges.push(cur);
                    }
                    self.terminate(Terminator::Jump(header_bb));
                }
            }
            Stmt::Global(_) | Stmt::Nonlocal(_) | Stmt::Pass(_) | Stmt::IpyEscapeCommand(_) => {}
        }
    }

    fn lower_function_def(&mut self, func: &'a ast::StmtFunctionDef) {
        let span = Span::from(func.range);
        let mut decorators = Vec::new();
        for dec in &func.decorator_list {
            decorators.push(self.module.slice_text(Span::from(dec.expression.range())));
            let _ = self.lower_expr(&dec.expression);
        }
        // Default argument expressions execute in the enclosing scope at def time.
        for p in func
            .parameters
            .posonlyargs
            .iter()
            .chain(func.parameters.args.iter())
            .chain(func.parameters.kwonlyargs.iter())
        {
            if let Some(def) = &p.default {
                let _ = self.lower_expr(def);
            }
        }

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

        let mut child_outer = self.outer_locals.clone();
        if !self.is_module_body && !self.is_class_body {
            child_outer.extend(self.scope.locals.iter().cloned());
        }

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
            body: FunctionBody::Suite(&func.body),
        });

        let captured_names = self.collect_child_captures(child_id);
        let captured_vars = self.read_captured_vars(&captured_names, span);
        let dst = self.fresh_var(Some(func.name.to_string()));
        self.emit(
            InstKind::MakeClosure {
                dst,
                func_id: Some(child_id),
                captured: captured_vars,
            },
            span,
        );
        self.write_var(func.name.as_str(), dst, span);
    }

    fn lower_class_def(&mut self, cls: &'a ast::StmtClassDef) {
        let span = Span::from(cls.range);
        let mut decorators = Vec::new();
        for dec in &cls.decorator_list {
            decorators.push(self.module.slice_text(Span::from(dec.expression.range())));
            let _ = self.lower_expr(&dec.expression);
        }
        if let Some(args) = &cls.arguments {
            for arg in &args.args {
                let _ = self.lower_expr(arg);
            }
            for kw in &args.keywords {
                let _ = self.lower_expr(&kw.value);
            }
        }

        // `heki_semantic` records every `class` statement of the same parse, so its `ClassId`
        // and qualname are always there.
        let (class_id, class_qualname) = self.module.class_by_span[&span].clone();

        let mut child_outer = self.outer_locals.clone();
        if !self.is_module_body && !self.is_class_body {
            child_outer.extend(self.scope.locals.iter().cloned());
        }

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
            body: FunctionBody::Suite(&cls.body),
        });

        let captured_names = self.collect_child_captures(child_id);
        let captured_vars = self.read_captured_vars(&captured_names, span);
        let dst = self.fresh_var(Some(cls.name.to_string()));
        self.emit(
            InstKind::MakeClosure {
                dst,
                func_id: Some(child_id),
                captured: captured_vars,
            },
            span,
        );
        self.write_var(cls.name.as_str(), dst, span);
    }

    fn collect_child_captures(&mut self, child_id: FunctionId) -> Vec<String> {
        // Child's `captured_vars` contains names captured by grandchildren; we also check
        // LoadDeref / StoreDeref instructions in the child function directly.
        let mut names = Vec::new();
        let child = &self.module.functions[child_id.0 as usize];
        for bb in &child.blocks {
            for inst in &bb.insts {
                match &inst.kind {
                    InstKind::LoadDeref { name, .. } | InstKind::StoreDeref { name, .. }
                        if !names.contains(name) =>
                    {
                        names.push(name.clone());
                    }
                    _ => {}
                }
            }
        }
        let mut own_captures = Vec::new();
        for name in names {
            if self.scope.locals.contains(&name) {
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
            Expr::Tuple(t) => {
                for (i, elt) in t.elts.iter().enumerate() {
                    let idx_var =
                        self.emit_const(ConstLiteral::Int(i as i64), Span::from(elt.range()));
                    let elem = self.fresh_var(None);
                    self.emit(
                        InstKind::SubscriptLoad {
                            dst: elem,
                            obj: value,
                            index: idx_var,
                            key_const: None,
                            is_slice: false,
                            slice_lower_const: None,
                            slice_upper_const: None,
                        },
                        Span::from(elt.range()),
                    );
                    self.lower_assign_target(elt, elem, Span::from(elt.range()));
                }
            }
            Expr::List(l) => {
                for (i, elt) in l.elts.iter().enumerate() {
                    let idx_var =
                        self.emit_const(ConstLiteral::Int(i as i64), Span::from(elt.range()));
                    let elem = self.fresh_var(None);
                    self.emit(
                        InstKind::SubscriptLoad {
                            dst: elem,
                            obj: value,
                            index: idx_var,
                            key_const: None,
                            is_slice: false,
                            slice_lower_const: None,
                            slice_upper_const: None,
                        },
                        Span::from(elt.range()),
                    );
                    self.lower_assign_target(elt, elem, Span::from(elt.range()));
                }
            }
            Expr::Starred(s) => {
                self.lower_assign_target(&s.value, value, Span::from(s.range));
            }
            // The parser rejects every other target (`InvalidAssignmentTarget`).
            _ => {}
        }
    }

    fn lower_delete_target(&mut self, target: &'a Expr) {
        let span = Span::from(target.range());
        match target {
            Expr::Name(n) => {
                let prev = self
                    .current_def
                    .get(&(self.cur_bb, n.id.to_string()))
                    .copied();
                self.emit(
                    InstKind::Delete {
                        var: prev,
                        name: n.id.to_string(),
                    },
                    span,
                );
            }
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
                cap_tests: Vec::new(),
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
        self.lower_suite(&f.body);
        if !self.is_terminated(self.cur_bb) {
            let tail = self.cur_bb;
            if let Some(top) = self.loop_stack.last_mut() {
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
                retry_body: false,
                cap_tests: len_cap_tests(&w.test),
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
        self.lower_suite(&w.body);
        if !self.is_terminated(self.cur_bb) {
            let tail = self.cur_bb;
            if let Some(top) = self.loop_stack.last_mut() {
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
        self.exc_target_stack.push(exit_bb);
        let body_bb = self.new_block(true);
        self.terminate(Terminator::Jump(body_bb));

        self.switch_to(body_bb);
        self.lower_suite(&w.body);
        self.terminate(Terminator::Jump(exit_bb));

        self.exc_target_stack.pop();
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

        let dispatch_bb = if has_handlers {
            Some(self.new_block(false))
        } else {
            None
        };
        let finally_bb = if has_finally {
            Some(self.new_block(false))
        } else {
            None
        };
        let after_bb = self.new_block(false);

        let body_exc_target = dispatch_bb.or(finally_bb);
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

        let normal_next = finally_bb.unwrap_or(after_bb);
        if !self.is_terminated(self.cur_bb) {
            if !t.orelse.is_empty() {
                if let Some(fin) = finally_bb {
                    self.exc_target_stack.push(fin);
                }
                let orelse_bb = self.new_block(true);
                self.terminate(Terminator::Jump(orelse_bb));
                self.switch_to(orelse_bb);
                self.lower_suite(&t.orelse);
                if finally_bb.is_some() {
                    self.exc_target_stack.pop();
                }
            }
            self.terminate(Terminator::Jump(normal_next));
        }

        if let Some(disp) = dispatch_bb {
            self.seal_block(disp);
            if let Some(fin) = finally_bb {
                self.exc_target_stack.push(fin);
            }
            let mut cur_disp = disp;
            for (idx, h) in t.handlers.iter().enumerate() {
                let ast::ExceptHandler::ExceptHandler(eh) = h;
                self.switch_to(cur_disp);
                let mut exc_types = Vec::new();
                if let Some(ty) = &eh.type_ {
                    exc_types.push(self.module.slice_text(Span::from(ty.range())));
                    let _ = self.lower_expr(ty);
                }
                let handler_bb = self.new_block(true);
                let fallthrough_bb = if idx + 1 < t.handlers.len() {
                    self.new_block(true)
                } else {
                    finally_bb.unwrap_or(after_bb)
                };
                let cond = self.emit_const(ConstLiteral::Bool(true), Span::from(eh.range));
                self.terminate_at(
                    Terminator::Branch {
                        cond,
                        then_bb: handler_bb,
                        else_bb: fallthrough_bb,
                    },
                    Span::from(eh.range),
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
                    Span::from(eh.range),
                );
                if let (Some(n), Some(v)) = (&eh.name, exc_var) {
                    self.write_var(n.as_str(), v, Span::from(n.range));
                }
                self.lower_suite(&eh.body);
                self.terminate(Terminator::Jump(normal_next));
                cur_disp = fallthrough_bb;
            }
            if finally_bb.is_some() {
                self.exc_target_stack.pop();
            }
        }

        if let Some(fin) = finally_bb {
            self.seal_block(fin);
            self.switch_to(fin);
            self.lower_suite(&t.finalbody);
            self.terminate(Terminator::Jump(after_bb));
        }

        self.seal_block(after_bb);
        self.switch_to(after_bb);
    }

    fn lower_match(&mut self, m: &'a ast::StmtMatch) {
        let subject = self.lower_expr(&m.subject);
        let merge_bb = self.new_block(false);

        for (idx, case) in m.cases.iter().enumerate() {
            let is_last = idx + 1 == m.cases.len();
            let case_bb = self.new_block(true);
            let next_bb = if is_last {
                merge_bb
            } else {
                self.new_block(true)
            };
            let pattern_span = Span::from(case.pattern.range());
            let cond = self.emit_const(ConstLiteral::Bool(true), pattern_span);
            self.terminate_at(
                Terminator::Branch {
                    cond,
                    then_bb: case_bb,
                    else_bb: next_bb,
                },
                pattern_span,
            );
            self.switch_to(case_bb);
            self.lower_pattern(&case.pattern, subject);
            if let Some(guard) = &case.guard {
                let g = self.lower_expr(guard);
                let body_bb = self.new_block(true);
                self.terminate_at(
                    Terminator::Branch {
                        cond: g,
                        then_bb: body_bb,
                        else_bb: next_bb,
                    },
                    Span::from(guard.range()),
                );
                self.switch_to(body_bb);
            }
            self.lower_suite(&case.body);
            self.terminate(Terminator::Jump(merge_bb));
            if !is_last {
                self.switch_to(next_bb);
            }
        }

        self.seal_block(merge_bb);
        self.switch_to(merge_bb);
    }

    fn lower_pattern(&mut self, pat: &'a Pattern, subject: VarId) {
        match pat {
            Pattern::MatchValue(mv) => {
                let _ = self.lower_expr(&mv.value);
            }
            Pattern::MatchSingleton(_) => {}
            Pattern::MatchSequence(ms) => {
                for p in &ms.patterns {
                    self.lower_pattern(p, subject);
                }
            }
            Pattern::MatchMapping(mm) => {
                for k in &mm.keys {
                    let _ = self.lower_expr(k);
                }
                for p in &mm.patterns {
                    self.lower_pattern(p, subject);
                }
                if let Some(rest) = &mm.rest {
                    self.write_var(rest.as_str(), subject, Span::from(rest.range));
                }
            }
            Pattern::MatchClass(mc) => {
                let _ = self.lower_expr(&mc.cls);
                for p in &mc.arguments.patterns {
                    self.lower_pattern(p, subject);
                }
                for kw in &mc.arguments.keywords {
                    self.lower_pattern(&kw.pattern, subject);
                }
            }
            Pattern::MatchStar(ms) => {
                if let Some(name) = &ms.name {
                    self.write_var(name.as_str(), subject, Span::from(name.range));
                }
            }
            Pattern::MatchAs(ma) => {
                if let Some(p) = &ma.pattern {
                    self.lower_pattern(p, subject);
                }
                if let Some(name) = &ma.name {
                    self.write_var(name.as_str(), subject, Span::from(name.range));
                }
            }
            Pattern::MatchOr(mo) => {
                for p in &mo.patterns {
                    self.lower_pattern(p, subject);
                }
            }
        }
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
            Expr::Name(n) => self.read_var(n.id.as_str(), span),
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
            Expr::Compare(c) => {
                // Chained comparisons left-to-right (`operands[0] op[0] operands[1] ...`); the
                // parser always produces at least two operands.
                let mut lhs = self.lower_expr(&c.operands[0]);
                let mut last_res = lhs;
                for (op, rhs_expr) in c.ops.iter().zip(&c.operands[1..]) {
                    let rhs = self.lower_expr(rhs_expr);
                    let dst = self.fresh_var(None);
                    self.emit(
                        InstKind::Compare {
                            dst,
                            op: map_cmpop(*op),
                            lhs,
                            rhs,
                        },
                        span,
                    );
                    lhs = rhs;
                    last_res = dst;
                }
                last_res
            }
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
            Expr::Slice(sl) => {
                if let Some(l) = &sl.lower {
                    let _ = self.lower_expr(l);
                }
                if let Some(u) = &sl.upper {
                    let _ = self.lower_expr(u);
                }
                if let Some(st) = &sl.step {
                    let _ = self.lower_expr(st);
                }
                self.emit_const(ConstLiteral::Other, span)
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
                if let Some(params) = &lam.parameters {
                    for p in params
                        .posonlyargs
                        .iter()
                        .chain(params.args.iter())
                        .chain(params.kwonlyargs.iter())
                    {
                        if let Some(def) = &p.default {
                            let _ = self.lower_expr(def);
                        }
                    }
                }
                let mut child_outer = self.outer_locals.clone();
                if !self.is_module_body && !self.is_class_body {
                    child_outer.extend(self.scope.locals.iter().cloned());
                }
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
                    body: FunctionBody::Expr(&lam.body),
                });
                let captured_names = self.collect_child_captures(child_id);
                let captured = self.read_captured_vars(&captured_names, span);
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

                let mut child_outer = self.outer_locals.clone();
                if !self.is_module_body && !self.is_class_body {
                    child_outer.extend(self.scope.locals.iter().cloned());
                }
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
                for part in fs.value.iter() {
                    if let ast::FStringPartRef::FString(fstr) = part {
                        for elem in &fstr.elements {
                            if let ast::InterpolatedStringElement::Interpolation(interp) = elem {
                                let _ = self.lower_expr(&interp.expression);
                                if let Some(spec) = &interp.format_spec {
                                    for se in &spec.elements {
                                        if let ast::InterpolatedStringElement::Interpolation(si) =
                                            se
                                        {
                                            let _ = self.lower_expr(&si.expression);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                let dst = self.fresh_var(None);
                self.emit(
                    InstKind::AllocBuiltin {
                        dst,
                        kind: BuiltinKind::Str,
                        elems: Vec::new(),
                    },
                    span,
                );
                dst
            }
            Expr::TString(ts) => {
                for part in ts.value.iter() {
                    for elem in &part.elements {
                        if let ast::InterpolatedStringElement::Interpolation(interp) = elem {
                            let _ = self.lower_expr(&interp.expression);
                            if let Some(spec) = &interp.format_spec {
                                for se in &spec.elements {
                                    if let ast::InterpolatedStringElement::Interpolation(si) = se {
                                        let _ = self.lower_expr(&si.expression);
                                    }
                                }
                            }
                        }
                    }
                }
                self.emit_const(ConstLiteral::Other, span)
            }
        }
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
            let callee_var = Some(self.lower_expr(&call.func));
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
    /// `acc.append(elt)`, `acc.add(elt)`, or `acc[key] = elt` (`{**d for d in ds}` has no key).
    fn emit_comp_element(
        &mut self,
        kind: CompKind,
        acc: VarId,
        elt: &'a Expr,
        key: Option<&'a Expr>,
        span: Span,
    ) {
        let elt_span = Span::from(elt.range());
        match kind {
            CompKind::List | CompKind::Set => {
                let v = self.lower_expr(elt);
                let dst = self.fresh_var(None);
                let method = match kind {
                    CompKind::List => "append",
                    _ => "add",
                };
                self.emit(
                    InstKind::MethodCall {
                        dst,
                        recv: acc,
                        method: method.to_string(),
                        args: vec![v],
                        kwargs: Vec::new(),
                        is_awaited: false,
                        star_from: None,
                    },
                    elt_span,
                );
            }
            CompKind::Dict => {
                let k = key
                    .map(|ke| self.lower_expr(ke))
                    .unwrap_or_else(|| self.emit_const(ConstLiteral::None, span));
                let key_const = key.and_then(extract_const_str);
                let v = self.lower_expr(elt);
                self.emit(
                    InstKind::SubscriptStore {
                        obj: acc,
                        index: k,
                        key_const,
                        value: v,
                    },
                    elt_span,
                );
            }
        }
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
                b.emit_comp_element(kind, acc, elt, key, span);
            });
            acc
        } else {
            // Python 3.11: non-inlined comprehension frame with eager outer iter (the grammar
            // requires at least one `for` clause).
            let outer_iter = self.lower_expr(&generators[0].iter);
            let mut child_outer = self.outer_locals.clone();
            if !self.is_module_body && !self.is_class_body {
                child_outer.extend(self.scope.locals.iter().cloned());
            }
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
            collect_target_bindings(&g.target, &mut raw_targets);
            let mut rename_map = FxHashMap::default();
            for name in raw_targets {
                rename_map.insert(name.clone(), format!(".comp{comp_id}_{name}"));
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
            if let Some(top) = self.loop_stack.last_mut() {
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
        if let Some(top) = self.loop_stack.last_mut() {
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
    fn record_a1_call_if_hazard(
        &mut self,
        callee: &str,
        arg_count: usize,
        kwarg_count: usize,
        span: Span,
    ) {
        let hazard = match callee {
            "eval" | "builtins.eval" => Some("eval"),
            "exec" | "builtins.exec" => Some("exec"),
            "locals" | "builtins.locals" => Some("locals"),
            "vars" | "builtins.vars" if arg_count == 0 && kwarg_count == 0 => Some("vars()"),
            "sys._getframe" | "_getframe" => Some("sys._getframe"),
            "inspect.currentframe" => Some("inspect.currentframe"),
            "gc.get_objects" => Some("gc.get_objects"),
            "gc.get_referrers" => Some("gc.get_referrers"),
            "ctypes.pythonapi" => Some("ctypes.pythonapi"),
            _ if callee.ends_with("._getframe") => Some("sys._getframe"),
            _ if callee.ends_with(".currentframe") => Some("inspect.currentframe"),
            _ if callee.ends_with(".get_objects") && callee.contains("gc.") => {
                Some("gc.get_objects")
            }
            _ => None,
        };
        if let Some(kind) = hazard {
            self.a1_hazards.push(A1Hazard {
                kind: kind.to_string(),
                span,
            });
        }
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

    // -------------------------------------------------------------------------
    // Finalization: seal remaining blocks, apply trivial-Phi replacements,
    // and compute `LoopCarriedInfo` for every natural loop.
    // -------------------------------------------------------------------------
    fn finish(mut self) -> IrFunction {
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

        let mut captured_vars: Vec<String> = self.captured_by_children.into_iter().collect();
        captured_vars.sort();
        self.module
            .loop_hints
            .push((self.id, std::mem::take(&mut self.finished_hints)));

        IrFunction {
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
            blocks: self.blocks,
            loops: self.finished_loops,
            loop_carried,
            a1_hazards: self.a1_hazards,
            span: self.span,
            is_module_body: self.is_module_body,
            is_class_body: self.is_class_body,
        }
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

/// A literal display of at most 8 items without `*` unpacking.
fn fixed_display(elts: &[Expr]) -> Option<LoopBound> {
    (elts.len() <= 8 && !elts.iter().any(|e| matches!(e, Expr::Starred(_))))
        .then_some(LoopBound::ConstSmall(elts.len() as u32))
}

fn classify_iter_expr_bound(iter: &Expr) -> LoopBound {
    match iter {
        Expr::Tuple(t) => fixed_display(&t.elts).unwrap_or(LoopBound::Unbounded),
        Expr::List(l) => fixed_display(&l.elts).unwrap_or(LoopBound::Unbounded),
        Expr::Set(s) => fixed_display(&s.elts).unwrap_or(LoopBound::Unbounded),
        Expr::Call(c) => {
            if matches!(c.func.as_ref(), Expr::Name(n) if n.id.as_str() == "range") {
                if c.arguments.args.len() == 1 {
                    if let Some(k) = extract_const_int(&c.arguments.args[0])
                        && (0..=8).contains(&k)
                    {
                        return LoopBound::ConstSmall(k as u32);
                    }
                } else if c.arguments.args.len() == 2
                    && let (Some(lo), Some(hi)) = (
                        extract_const_int(&c.arguments.args[0]),
                        extract_const_int(&c.arguments.args[1]),
                    )
                {
                    let span = hi.saturating_sub(lo);
                    if (0..=8).contains(&span) {
                        return LoopBound::ConstSmall(span as u32);
                    }
                }
            }
            LoopBound::Unbounded
        }
        _ => LoopBound::Unbounded,
    }
}

fn classify_while_bound(w: &ast::StmtWhile) -> LoopBound {
    let is_flag_loop = match w.test.as_ref() {
        Expr::Attribute(a) => looks_like_daemon_flag(a.attr.as_str()),
        Expr::Name(n) => looks_like_daemon_flag(n.id.as_str()),
        Expr::UnaryOp(u) if matches!(u.op, ast::UnaryOp::Not) => match u.operand.as_ref() {
            Expr::Attribute(a) => looks_like_stop_flag(a.attr.as_str()),
            Expr::Name(n) => looks_like_stop_flag(n.id.as_str()),
            _ => false,
        },
        _ => false,
    };
    if is_flag_loop {
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

fn looks_like_daemon_flag(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("running") || lower.contains("active") || lower.contains("alive")
}

fn looks_like_stop_flag(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("stop") || lower.contains("closed") || lower.contains("shutdown")
}

fn target_looks_like_retry(target: &Expr) -> bool {
    if let Expr::Name(n) = target {
        let lower = n.id.to_ascii_lowercase();
        lower.contains("retry") || lower.contains("retries") || lower.contains("attempt")
    } else {
        false
    }
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

fn collect_suite_bindings(stmts: &[Stmt], out: &mut ScopeBindings) {
    for stmt in stmts {
        match stmt {
            Stmt::FunctionDef(f) => {
                out.locals.insert(f.name.to_string());
            }
            Stmt::ClassDef(c) => {
                out.locals.insert(c.name.to_string());
            }
            Stmt::TypeAlias(ta) => collect_target_bindings(&ta.name, &mut out.locals),
            Stmt::Assign(a) => {
                for t in &a.targets {
                    collect_target_bindings(t, &mut out.locals);
                }
                collect_expr_bindings(&a.value, out);
            }
            Stmt::AugAssign(aa) => {
                collect_target_bindings(&aa.target, &mut out.locals);
                collect_expr_bindings(&aa.value, out);
            }
            Stmt::AnnAssign(aa) => {
                if aa.simple || aa.value.is_some() {
                    collect_target_bindings(&aa.target, &mut out.locals);
                }
                if let Some(v) = &aa.value {
                    collect_expr_bindings(v, out);
                }
            }
            Stmt::Delete(d) => {
                for t in &d.targets {
                    collect_target_bindings(t, &mut out.locals);
                }
            }
            Stmt::For(f) => {
                collect_expr_bindings(&f.iter, out);
                collect_target_bindings(&f.target, &mut out.locals);
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
                        collect_target_bindings(opt, &mut out.locals);
                    }
                }
                collect_suite_bindings(&w.body, out);
            }
            Stmt::Try(t) => {
                collect_suite_bindings(&t.body, out);
                for h in &t.handlers {
                    let ast::ExceptHandler::ExceptHandler(eh) = h;
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

fn collect_target_bindings(expr: &Expr, locals: &mut FxHashSet<String>) {
    match expr {
        Expr::Name(n) => {
            locals.insert(n.id.to_string());
        }
        Expr::Tuple(t) => {
            for e in &t.elts {
                collect_target_bindings(e, locals);
            }
        }
        Expr::List(l) => {
            for e in &l.elts {
                collect_target_bindings(e, locals);
            }
        }
        Expr::Starred(s) => collect_target_bindings(&s.value, locals),
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

fn collect_expr_bindings(expr: &Expr, out: &mut ScopeBindings) {
    match expr {
        Expr::Named(n) => {
            collect_target_bindings(&n.target, &mut out.locals);
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
        Expr::Lambda(_) | Expr::Generator(_) => {
            // Do NOT cross nested function / generator expression boundaries.
        }
        Expr::ListComp(lc) => {
            for g in &lc.generators {
                collect_expr_bindings(&g.iter, out);
                for c in &g.ifs {
                    collect_expr_bindings(c, out);
                }
            }
            collect_expr_bindings(&lc.elt, out);
        }
        Expr::SetComp(sc) => {
            for g in &sc.generators {
                collect_expr_bindings(&g.iter, out);
                for c in &g.ifs {
                    collect_expr_bindings(c, out);
                }
            }
            collect_expr_bindings(&sc.elt, out);
        }
        Expr::DictComp(dc) => {
            for g in &dc.generators {
                collect_expr_bindings(&g.iter, out);
                for c in &g.ifs {
                    collect_expr_bindings(c, out);
                }
            }
            if let Some(k) = &dc.key {
                collect_expr_bindings(k, out);
            }
            collect_expr_bindings(&dc.value, out);
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
        Expr::Set(s) => {
            for e in &s.elts {
                collect_expr_bindings(e, out);
            }
        }
        Expr::List(l) => {
            for e in &l.elts {
                collect_expr_bindings(e, out);
            }
        }
        Expr::Tuple(t) => {
            for e in &t.elts {
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
        Expr::Slice(s) => {
            if let Some(l) = &s.lower {
                collect_expr_bindings(l, out);
            }
            if let Some(u) = &s.upper {
                collect_expr_bindings(u, out);
            }
            if let Some(st) = &s.step {
                collect_expr_bindings(st, out);
            }
        }
        _ => {}
    }
}

// =============================================================================
// Helpers
// =============================================================================
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
        InstKind::AllocBuiltin { elems, .. } => {
            for e in elems {
                *e = resolve(*e);
            }
        }
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
