//! Import facts (ADR-022): every place a module imports another, when it runs, and what loading
//! a module loads while it loads. This module is the one home of which module an import site
//! loads ([`ImportGraph::target`]); no rule resolves an import itself.
//!
//! The facts of each file ([`ModuleImports`]) are local and IR-free, kept in
//! [`crate::FileSummary::imports`] with [`crate::OptInFacts::imports`]. The graph is built per
//! check from them and the project declarations, and never kept in the index (ADR-017).

use crate::resolver::{FromImport, ProjectDecls, resolve_relative_module};
use crate::{FileSummary, ModuleInput, environment, is_stub, scc};
use heki_ast::Span;
use heki_ir::walk::{Answer, BitSet, Budget, Closure, FnShape, Visit, reach, reach_breadth_first};
use heki_ir::{BlockId, FunctionId, Inst, InstKind, IrFunction, LastBindings, ModuleIr};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, OnceCell};
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::rc::Rc;

/// When an import site runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Timing {
    /// While its module loads: in the module body or a class body the module body runs, or a
    /// lazy import whose binding a read of the module body may reach ([`lazy_loads`]).
    Body,
    /// When the name it binds is first used (PEP 810), which the module body does not do.
    Lazy,
    /// When the function holding it runs.
    Function,
    /// Never: under `if TYPE_CHECKING:`.
    TypeChecking,
}

impl Timing {
    pub const ALL: [Self; 4] = [Self::Body, Self::Lazy, Self::Function, Self::TypeChecking];

    /// [`Self::as_str`] of each of [`Self::ALL`]: how the configuration names the classes.
    pub const NAMES: [&'static str; 4] = [
        Self::ALL[0].as_str(),
        Self::ALL[1].as_str(),
        Self::ALL[2].as_str(),
        Self::ALL[3].as_str(),
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Body => "body",
            Self::Lazy => "lazy",
            Self::Function => "function",
            Self::TypeChecking => "type-checking",
        }
    }
}

/// How an import site names what it imports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportForm {
    /// `import a.b.c`, also with `as`.
    Import,
    /// `from a.b import name`.
    From(String),
    /// `from a.b import *`.
    Star,
    /// A call importing a module by name (`importlib.import_module`, `__import__`:
    /// `heki_kb::EcosystemEntry::loads_module`).
    ByName,
}

/// A module name an import site gives, made absolute.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Named {
    Module(String),
    /// Some module whose name starts with this text (`pkg.plugins.` of `f"pkg.plugins.{name}"`).
    Under(String),
}

/// One import site of a module (a local fact).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportSite {
    /// The imported name of a statement (`a.b` of `import a.b`, `name` of `from m import
    /// name`), or the call.
    pub span: Span,
    /// The function the site is in: `<module>` for the module body and the class bodies it
    /// runs, as `attribution = "lexical"` names it; a class body in a function is that
    /// function's.
    pub qualname: String,
    pub form: ImportForm,
    /// The modules the site names: the one of a statement (`a.b` of `from a.b import c`), or
    /// every text a by-name call's argument may hold. None for a relative import climbing past
    /// its top-level package, which imports nothing and raises; open when the code does not
    /// fix a name.
    pub modules: Answer<Vec<Named>>,
    pub timing: Answer<Timing>,
    /// An `ImportError` handler takes the failure of the import (the `try` around it, or
    /// around the class statement whose body holds it).
    pub guarded: bool,
    /// The site runs on every path through its function's body to a normal return, outside
    /// loops; never established in a class body.
    pub unconditional: bool,
}

impl ImportSite {
    /// Whether the site imports whenever its module loads: in the body, on every path through
    /// it, and with no `ImportError` handler taking a failure. A site on one branch (`if
    /// sys.platform == "win32": import winreg`) is not one.
    pub fn loads_while_its_module_loads(&self) -> bool {
        self.timing == Answer::Known(Timing::Body) && self.unconditional && !self.guarded
    }

    /// The site with its positions zeroed: what Early Cutoff compares.
    fn unplaced(&self) -> Self {
        Self {
            span: Span { start: 0, end: 0 },
            ..self.clone()
        }
    }
}

/// The import facts of one file ([`crate::FileSummary::imports`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleImports {
    /// In source order.
    pub sites: Vec<ImportSite>,
}

impl ModuleImports {
    /// Whether the two give the same graph, wherever the sites are written (Early Cutoff:
    /// related locations move with the files that show them).
    pub fn same_facts(&self, other: &Self) -> bool {
        self.sites.len() == other.sites.len()
            && (self.sites.iter().zip(&other.sites)).all(|(a, b)| a.unplaced() == b.unplaced())
    }
}

/// A class body a function runs, and whether an `ImportError` handler takes what its class
/// statement raises (in the function, or around the statement of an enclosing class body).
struct ClassBody<'m> {
    body: &'m IrFunction,
    guarded: bool,
}

/// The class bodies `f` runs, nested ones included, each once (a class statement in a `finally`
/// body is made once per way out of its `try`; one guarded copy guards it).
fn class_bodies<'m>(ir: &'m ModuleIr, f: &'m IrFunction) -> Vec<ClassBody<'m>> {
    let mut out: Vec<ClassBody<'m>> = Vec::new();
    let mut index: FxHashMap<u32, usize> = FxHashMap::default();
    // Each node is a function and whether a handler guards the class statements leading to it.
    let next = |(id, outer): (u32, bool), out: &mut Vec<(u32, bool)>| {
        let g = ir.function(FunctionId(id));
        for bb in &g.blocks {
            for inst in &bb.insts {
                if let InstKind::MakeClosure {
                    func_id: Some(body),
                    ..
                } = &inst.kind
                    && ir.function(*body).is_class_body
                {
                    let guarded = outer || environment::guarded_against_import_errors(g, bb.id);
                    out.push((body.0, guarded));
                }
            }
        }
    };
    // Unmetered: a function's class statements are walked once, and the walk is never open.
    reach(
        &Budget::unmetered("import_sites"),
        [(f.id.0, false)],
        next,
        |(id, guarded)| {
            if id != f.id.0 {
                let at = *index.entry(id).or_insert_with(|| {
                    out.push(ClassBody {
                        body: ir.function(FunctionId(id)),
                        guarded: false,
                    });
                    out.len() - 1
                });
                out[at].guarded |= guarded;
            }
            Visit::<()>::Expand
        },
    )
    .open_means(None);
    out
}

/// Where the copies of one statement's import are, and what holds them.
struct Placed {
    /// The function running it (an index into [`Placements::runners`]).
    runner: usize,
    in_class: bool,
    guarded: bool,
    unconditional: bool,
}

/// Where a module's import statements run, from its IR.
struct Placements<'m> {
    /// The functions that are no class bodies: each runs its own body and its class bodies.
    runners: Vec<&'m IrFunction>,
    /// By the span of the imported name: one entry for the copies of a statement in a
    /// `finally` body, guarded or unconditional when one copy is.
    placed: FxHashMap<Span, Placed>,
    /// The reads of names a lazy import binds in the module body and the class bodies it runs.
    lazy_reads: Vec<LazyRead<'m>>,
    /// By the span of the imported name, what the module body's reads load of a lazy import
    /// ([`lazy_loads`]).
    lazy_loads: FxHashMap<Span, LazyLoad>,
    /// The class bodies an `ImportError` handler guards around their class statement.
    guarded_classes: Vec<Span>,
}

impl<'m> Placements<'m> {
    fn of(ir: &'m ModuleIr) -> Self {
        let runners: Vec<&IrFunction> =
            (ir.functions.iter()).filter(|f| !f.is_class_body).collect();
        let mut out = Self {
            runners: Vec::new(),
            placed: FxHashMap::default(),
            lazy_reads: Vec::new(),
            lazy_loads: FxHashMap::default(),
            guarded_classes: Vec::new(),
        };
        for (runner, f) in runners.iter().enumerate() {
            out.place(ir, runner, f);
        }
        if !out.lazy_reads.is_empty() {
            out.lazy_loads = lazy_loads(ir, &out.lazy_reads);
        }
        out.runners = runners;
        out
    }

    /// The imports `f`, the function `runner`, runs in its body and its class bodies.
    fn place(&mut self, ir: &'m ModuleIr, runner: usize, f: &'m IrFunction) {
        // Built for the functions holding an import only.
        let shape: OnceCell<FnShape<'_>> = OnceCell::new();
        let ends: OnceCell<LastBindings> = OnceCell::new();
        let always = |bb: BlockId| {
            let shape = shape.get_or_init(|| FnShape::new(f));
            ends.get_or_init(|| LastBindings::of(shape))
                .always_runs(shape, bb)
        };
        let bodies = class_bodies(ir, f);
        (self.guarded_classes).extend(bodies.iter().filter(|c| c.guarded).map(|c| c.body.span));
        let holders = std::iter::once((f, false, false))
            .chain(bodies.iter().map(|c| (c.body, true, c.guarded)));
        for (g, in_class, outer) in holders {
            for bb in &g.blocks {
                for (index, inst) in bb.insts.iter().enumerate() {
                    match &inst.kind {
                        InstKind::Import { .. } => {
                            let guarded =
                                outer || environment::guarded_against_import_errors(g, bb.id);
                            let unconditional = !in_class && always(bb.id);
                            self.add(inst.span, runner, in_class, guarded, unconditional);
                        }
                        InstKind::LoadGlobal {
                            name,
                            is_lazy_import: true,
                            ..
                        } if f.is_module_body => self.lazy_reads.push(LazyRead {
                            name,
                            at: (!in_class).then_some((bb.id, index)),
                            unconditional: !in_class && always(bb.id),
                            guarded: outer || environment::guarded_against_import_errors(g, bb.id),
                        }),
                        _ => {}
                    }
                }
            }
        }
        if f.is_module_body {
            for &(bb, span) in &ir.star_imports {
                let guarded = environment::guarded_against_import_errors(f, bb);
                self.add(span, runner, false, guarded, always(bb));
            }
        }
    }

    fn add(&mut self, span: Span, runner: usize, in_class: bool, guarded: bool, always: bool) {
        let at = self.placed.entry(span).or_insert(Placed {
            runner,
            in_class,
            guarded: false,
            unconditional: false,
        });
        at.guarded |= guarded;
        at.unconditional |= always;
    }
}

/// A read of a name a lazy import binds, in the module body or a class body it runs.
struct LazyRead<'m> {
    name: &'m str,
    /// The block of the module body and the instruction; `None` in a class body.
    at: Option<(BlockId, usize)>,
    unconditional: bool,
    guarded: bool,
}

/// What the module body's reads load of one lazy import. An entry without `read` comes from a
/// read whose binding heki cannot tell, which may load it.
#[derive(Default)]
struct LazyLoad {
    /// A read its binding may reach: the first such read loads the module there.
    read: bool,
    /// A read only its binding reaches runs on every path through the body.
    unconditional: bool,
    /// A read that may load it is outside every `ImportError` handler.
    unguarded: bool,
}

/// What the module body's `reads` of lazily imported names load (PEP 810): a read loads the
/// module of the lazy import whose binding reaches it, so a name bound twice
/// (`lazy import fractions as dep`, then `lazy import decimal as dep`) loads by the binding a
/// read sees. A read whose binding heki cannot tell (in a class body, of a name the module
/// binds more than once, or past the budget) may load any lazy import of its name.
fn lazy_loads(ir: &ModuleIr, reads: &[LazyRead<'_>]) -> FxHashMap<Span, LazyLoad> {
    let body = ir.function(ir.module_fn);
    let shape = FnShape::new(body);
    let names: FxHashSet<&str> = reads.iter().map(|r| r.name).collect();
    let bindings = module_bindings(ir, &shape, &names);
    let budget = Budget::of_function("lazy_imports", body);
    let reaching = Closure::new();
    let mut out: FxHashMap<Span, LazyLoad> = FxHashMap::default();
    for read in reads {
        let name = &bindings[read.name];
        match name.reaching(&shape, &budget, &reaching, read.at) {
            Answer::Known(reached) => {
                let only = reached.len() == 1;
                for import in reached.into_iter().filter_map(|b| name.import(b)) {
                    let load = out.entry(import).or_default();
                    load.read = true;
                    load.unconditional |= only && read.unconditional;
                    load.unguarded |= !read.guarded;
                }
            }
            Answer::Open => {
                for import in (0..name.all.len()).filter_map(|b| name.import(b)) {
                    out.entry(import).or_default().unguarded |= !read.guarded;
                }
            }
        }
    }
    out
}

/// The bindings of `names` in the module body of `ir`, by name: its stores (`StoreGlobal`: an
/// assignment, a definition, an import) and its star imports.
fn module_bindings<'n>(
    ir: &ModuleIr,
    shape: &FnShape<'_>,
    names: &FxHashSet<&'n str>,
) -> FxHashMap<&'n str, NameBindings> {
    let mut out: FxHashMap<&str, NameBindings> = (names.iter().enumerate())
        .map(|(id, &name)| (name, NameBindings::new(id as u32)))
        .collect();
    let body = shape.f;
    for bb in &body.blocks {
        for (index, inst) in bb.insts.iter().enumerate() {
            if let InstKind::StoreGlobal { name, value } = &inst.kind
                && let Some(bindings) = out.get_mut(name.as_str())
            {
                let binding = match shape.inst(shape.copy_root(*value)) {
                    Some(
                        i @ Inst {
                            kind: InstKind::Import { is_lazy: true, .. },
                            ..
                        },
                    ) => Binding::Lazy(i.span),
                    _ => Binding::Store,
                };
                bindings.add(bb.id, index + 1, binding);
            }
        }
    }
    for &(bb, span) in &ir.star_imports {
        let insts = &body.block(bb).insts;
        let after = (insts.iter())
            .position(|i| i.span.start >= span.end)
            .unwrap_or(insts.len());
        for bindings in out.values_mut() {
            bindings.add(bb, after, Binding::Star);
        }
    }
    for bindings in out.values_mut() {
        for list in bindings.by_block.values_mut() {
            list.sort_unstable();
        }
    }
    out
}

/// A binding of a name in the module body.
#[derive(Clone, Copy)]
enum Binding {
    /// A lazy import, by its imported name.
    Lazy(Span),
    /// Any other store.
    Store,
    /// A star import, which may bind the name or leave the binding before it.
    Star,
}

/// The bindings of one name in the module body.
struct NameBindings {
    /// For the [`Closure`] keys.
    id: u32,
    /// Each binding, by its index.
    all: Vec<Binding>,
    /// Per block, its bindings in order: the first instruction that sees it (the one after a
    /// store, the first after a star import) and the binding's index.
    by_block: FxHashMap<BlockId, Vec<(usize, usize)>>,
}

impl NameBindings {
    fn new(id: u32) -> Self {
        Self {
            id,
            all: Vec::new(),
            by_block: FxHashMap::default(),
        }
    }

    fn add(&mut self, bb: BlockId, from: usize, binding: Binding) {
        (self.by_block.entry(bb).or_default()).push((from, self.all.len()));
        self.all.push(binding);
    }

    /// The lazy import binding `binding` binds.
    fn import(&self, binding: usize) -> Option<Span> {
        match self.all[binding] {
            Binding::Lazy(span) => Some(span),
            Binding::Store | Binding::Star => None,
        }
    }

    /// Into `out`, the bindings of `bb` an instruction from `upto` on may see: back to its last
    /// store before it, star imports after the store included. Whether there is such a store,
    /// which hides what reaches the block.
    fn seen_in(&self, bb: BlockId, upto: usize, out: &mut Vec<usize>) -> bool {
        let here = self.by_block.get(&bb).into_iter().flatten().rev();
        for &(_, binding) in here.filter(|&&(from, _)| from <= upto) {
            out.push(binding);
            if !matches!(self.all[binding], Binding::Star) {
                return true;
            }
        }
        false
    }

    /// The bindings (by index) that may reach a read of the name `at` an instruction of the
    /// module body: those its block sees before it and, past no store, those reaching the
    /// block. Open in a class body (`at` is `None`) unless the name has one binding, and past
    /// the budget.
    fn reaching(
        &self,
        shape: &FnShape<'_>,
        budget: &Budget,
        reaching: &Closure<(u32, BlockId), BitSet>,
        at: Option<(BlockId, usize)>,
    ) -> Answer<Vec<usize>> {
        if self.all.len() == 1 {
            return Answer::Known(vec![0]);
        }
        let Some((bb, index)) = at else {
            return Answer::Open;
        };
        let mut seen = Vec::new();
        if self.seen_in(bb, index, &mut seen) {
            return Answer::Known(seen);
        }
        let entering = reaching.ask(
            budget,
            (self.id, bb),
            |(_, to)| self.entering(shape, to),
            |(_, to), out| {
                out.extend(
                    (shape.preds(to).iter())
                        .filter(|&&from| self.passes_through(shape.f, from, to))
                        .map(|&from| (self.id, from)),
                )
            },
        );
        entering.map(|set| {
            seen.extend(set.iter());
            seen.sort_unstable();
            seen.dedup();
            seen
        })
    }

    /// The bindings the blocks before `to` hand it in one step: those the end of a block
    /// jumping to it sees, and every one of a block whose exceptions `to` handles (the
    /// exception may come after any).
    fn entering(&self, shape: &FnShape<'_>, to: BlockId) -> BitSet {
        let mut out = BitSet::new();
        let mut seen = Vec::new();
        for &from in shape.preds(to) {
            if handles(shape.f, from, to) {
                for &(_, binding) in self.by_block.get(&from).into_iter().flatten() {
                    out.insert(binding);
                }
            }
            if shape.f.block(from).terminator.successors().contains(&to) {
                seen.clear();
                self.seen_in(from, usize::MAX, &mut seen);
                for &binding in &seen {
                    out.insert(binding);
                }
            }
        }
        out
    }

    /// Whether what reaches the start of `from` may reach `to` through it: `from` stores the
    /// name nowhere, or `to` handles its exceptions.
    fn passes_through(&self, f: &IrFunction, from: BlockId, to: BlockId) -> bool {
        let stores = (self.by_block.get(&from).into_iter().flatten())
            .any(|&(_, binding)| !matches!(self.all[binding], Binding::Star));
        !stores || handles(f, from, to)
    }
}

/// Whether `to` handles the exceptions raised in `from`.
fn handles(f: &IrFunction, from: BlockId, to: BlockId) -> bool {
    f.block(from).exc_target == Some(to)
}

/// The import facts of `input`: its import statements, read from the semantic model and placed
/// in the IR, and `by_name`, the by-name calls the summary found. `None` for a typing stub,
/// whose imports never run.
pub(crate) fn module_imports(
    input: &ModuleInput<'_>,
    mut by_name: Vec<ImportSite>,
) -> Option<ModuleImports> {
    if is_stub(&input.path) {
        return None;
    }
    let placements = Placements::of(input.ir);
    let statements = (input.semantic.imports.iter())
        .map(|imp| (imp.span, Statement::Import(imp)))
        .chain((input.semantic.star_imports.iter()).map(|star| (star.span, Statement::Star(star))));
    let mut sites: Vec<ImportSite> = statements
        .filter_map(|(span, statement)| {
            let at = placements.placed.get(&span)?;
            Some(statement_site(input, &placements, span, statement, at))
        })
        .collect();
    for site in &mut by_name {
        site.guarded |= (placements.guarded_classes.iter())
            .any(|c| c.start <= site.span.start && site.span.end <= c.end);
    }
    sites.extend(by_name);
    sites.sort_by_key(|s| s.span.start);
    Some(ModuleImports { sites })
}

/// The site of the import `statement` of `input`, placed `at`.
fn statement_site(
    input: &ModuleInput<'_>,
    placements: &Placements<'_>,
    span: Span,
    statement: Statement<'_>,
    at: &Placed,
) -> ImportSite {
    let runner = placements.runners[at.runner];
    let (level, module, form, in_type_checking) = match statement {
        Statement::Import(imp) if imp.is_from => (
            imp.level,
            imp.module.as_deref(),
            ImportForm::From(imp.imported_name.clone()),
            imp.in_type_checking,
        ),
        Statement::Import(imp) => (
            0,
            Some(imp.imported_name.as_str()),
            ImportForm::Import,
            imp.in_type_checking,
        ),
        Statement::Star(star) => (
            star.level,
            star.module.as_deref(),
            ImportForm::Star,
            star.in_type_checking,
        ),
    };
    let mut unconditional = at.unconditional && !at.in_class;
    let mut guarded = at.guarded;
    let is_lazy = matches!(statement, Statement::Import(imp) if imp.is_lazy);
    let timing = match statement {
        _ if in_type_checking => Answer::Known(Timing::TypeChecking),
        _ if !runner.is_module_body => Answer::Known(Timing::Function),
        Statement::Import(imp) if imp.lazy_open => Answer::Open,
        // A lazy import loads where the module body first reads its binding.
        Statement::Import(_) if is_lazy => match placements.lazy_loads.get(&span) {
            Some(load) if load.read => {
                unconditional = load.unconditional;
                guarded = !load.unguarded;
                Answer::Known(Timing::Body)
            }
            Some(_) => Answer::Open,
            None => Answer::Known(Timing::Lazy),
        },
        _ => Answer::Known(Timing::Body),
    };
    let modules = match resolve_relative_module(
        &input.module_name,
        input.is_package,
        level,
        module,
        input.root_is_package,
    ) {
        // The root package of a project that is one, which heki names no module.
        Some(m) if m.is_empty() => Answer::Open,
        Some(m) => Answer::Known(vec![Named::Module(m)]),
        None => Answer::Known(Vec::new()),
    };
    ImportSite {
        span,
        qualname: crate::resolver::func_qualname(runner).to_owned(),
        form,
        modules,
        timing,
        guarded,
        unconditional,
    }
}

/// An import statement of the semantic model.
#[derive(Clone, Copy)]
enum Statement<'s> {
    Import(&'s heki_semantic::ModuleImport),
    Star(&'s heki_semantic::StarImport),
}

// ------------------------------------------------------------------------------------ graph

/// A module an import site loads: one of the project's, or one outside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ModuleRef {
    Project(u32),
    External(u32),
}

/// What one import site loads ([`ImportGraph::target`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// This module, after its parent packages.
    Module(ModuleRef),
    /// `from a import c` where heki cannot tell whether `c` is the submodule `a.c` (an
    /// external package, or a project package that binds `c` before the submodule is
    /// imported, `ProjectDecls::from_import`): `a` loads either way.
    PackageOrSubmodule {
        package: ModuleRef,
        submodule: String,
    },
    /// A by-name import of one of these.
    AnyOf(Vec<Target>),
    /// Some module whose name starts with this text.
    Under(String),
    /// A module below a project package that the project does not have (a type checker
    /// reports it), or, empty, a relative import climbing past its top-level package: the
    /// import fails.
    Missing(String),
    /// Nothing heki can name: a name the code does not fix, or a module the import may find
    /// inside or outside the project.
    Open,
}

impl ModuleRef {
    /// The id of a project module, `None` for a module outside the project.
    pub fn project(self) -> Option<u32> {
        match self {
            Self::Project(m) => Some(m),
            Self::External(_) => None,
        }
    }
}

impl Target {
    /// The module the site certainly loads: its module, or the package of a `from` import.
    pub fn certain(&self) -> Option<ModuleRef> {
        match self {
            Self::Module(m) | Self::PackageOrSubmodule { package: m, .. } => Some(*m),
            _ => None,
        }
    }
}

/// The import graph of a project: its modules, those outside it that its imports name, and
/// what each import site loads. Built per check from the summaries' import facts and the
/// declarations (`ProjectDecls`, whose reads go into `DeclKeys::modules`).
pub struct ImportGraph<'i> {
    /// The project modules, sorted.
    modules: Vec<String>,
    module_ids: FxHashMap<String, u32>,
    externals: Vec<String>,
    external_ids: FxHashMap<String, u32>,
    /// Per project module, the file defining it.
    paths: Vec<&'i Path>,
    /// Per project module, its sites and what each loads; `None` for a module heki has no
    /// import facts of (a file it could not read).
    sites: Vec<Option<Vec<(&'i ImportSite, Target)>>>,
    /// Per project module, what loading it loads in one step ([`Self::loads`]).
    loads: Vec<Vec<ModuleRef>>,
    /// The nodes and edges, the size the walks over the graph are budgeted by.
    size: usize,
}

impl<'i> ImportGraph<'i> {
    /// The graph of the project `files` summarize, with `decls` their declarations. A module
    /// several files define is the one of its owner (`module_precedence`).
    pub fn build(files: impl IntoIterator<Item = &'i FileSummary>, decls: &ProjectDecls) -> Self {
        Self::resolving(files, decls, |_| true)
    }

    /// [`Self::build`] with the sites of the modules `resolves` selects only, the others left
    /// without import facts: what those modules import, without reading the declarations the
    /// imports of the others name (which the language server records as what a check of one
    /// file depends on). Walks over it see the others as modules that load nothing.
    pub fn resolving(
        files: impl IntoIterator<Item = &'i FileSummary>,
        decls: &ProjectDecls,
        resolves: impl Fn(&str) -> bool,
    ) -> Self {
        let mut files: Vec<&'i FileSummary> = files.into_iter().collect();
        files.sort_by(|a, b| {
            crate::module_precedence(&a.path).cmp(&crate::module_precedence(&b.path))
        });
        // The greatest of the files sharing a module name is its owner.
        let owners: FxHashMap<&str, &'i FileSummary> = files
            .into_iter()
            .map(|f| (f.module_name.as_str(), f))
            .collect();
        let mut modules: Vec<String> = (owners.keys())
            .filter(|m| !m.is_empty())
            .map(|m| (*m).to_owned())
            .collect();
        modules.sort();
        let module_ids = (modules.iter().enumerate())
            .map(|(i, m)| (m.clone(), i as u32))
            .collect();
        let mut graph = Self {
            modules,
            module_ids,
            externals: Vec::new(),
            external_ids: FxHashMap::default(),
            paths: Vec::new(),
            sites: Vec::new(),
            loads: Vec::new(),
            size: 0,
        };
        for i in 0..graph.modules.len() {
            let name = graph.modules[i].as_str();
            let owner = owners[name];
            graph.paths.push(&owner.path);
            let facts = owner.imports().filter(|_| resolves(name));
            let resolved: Option<Vec<(&'i ImportSite, Target)>> = facts.map(|facts| {
                (facts.sites.iter())
                    .map(|site| (site, graph.target(site, decls)))
                    .collect()
            });
            let mut loads: BTreeSet<ModuleRef> = BTreeSet::new();
            let mut later: BTreeSet<ModuleRef> = BTreeSet::new();
            for (site, target) in resolved.iter().flatten() {
                let Some(m) = target.certain() else {
                    continue;
                };
                // The parents of every site's module are named, so that [`Self::parents`]
                // finds them for a site of any timing.
                if site.loads_while_its_module_loads() {
                    graph.with_parents(m, &mut loads);
                } else {
                    graph.with_parents(m, &mut later);
                }
            }
            graph.size += 1 + resolved.as_ref().map_or(0, Vec::len);
            graph.sites.push(resolved);
            graph.loads.push(loads.into_iter().collect());
        }
        graph.size += graph.externals.len();
        graph
    }

    /// What `site` loads: the modules it names resolved against the project, a `from` import
    /// split between a submodule and an attribute by `ProjectDecls::from_import`.
    fn target(&mut self, site: &ImportSite, decls: &ProjectDecls) -> Target {
        let Answer::Known(named) = &site.modules else {
            return Target::Open;
        };
        let mut targets: Vec<Target> = named
            .iter()
            .map(|n| match n {
                Named::Module(m) => self.module_target(m, decls),
                Named::Under(head) => Target::Under(head.clone()),
            })
            .collect();
        match (&site.form, targets.len()) {
            (_, 0) => Target::Missing(String::new()),
            (ImportForm::From(name), 1) => {
                let package = targets.remove(0);
                self.imported_from(package, name, decls)
            }
            (_, 1) => targets.remove(0),
            _ => Target::AnyOf(targets),
        }
    }

    /// `from <package> import name`.
    fn imported_from(&mut self, package: Target, name: &str, decls: &ProjectDecls) -> Target {
        match package {
            Target::Module(ModuleRef::Project(p)) => {
                let package_name = self.modules[p as usize].clone();
                let full = format!("{package_name}.{name}");
                if !decls.holds_module(&full) {
                    return package;
                }
                match decls.from_import(&package_name, name) {
                    FromImport::Submodule => self.module_target(&full, decls),
                    FromImport::Attribute => package,
                    FromImport::Open => Target::PackageOrSubmodule {
                        package: ModuleRef::Project(p),
                        submodule: full,
                    },
                }
            }
            Target::Module(m @ ModuleRef::External(e)) => Target::PackageOrSubmodule {
                package: m,
                submodule: format!("{}.{name}", self.externals[e as usize]),
            },
            other => other,
        }
    }

    /// The module named `name`.
    fn module_target(&mut self, name: &str, decls: &ProjectDecls) -> Target {
        if let Some(&id) = self.module_ids.get(name)
            && decls.holds_module(name)
        {
            return Target::Module(ModuleRef::Project(id));
        }
        // The project has the module, but the import may find another one first.
        if decls.module(name).is_some() {
            return Target::Open;
        }
        if (name.match_indices('.')).any(|(i, _)| decls.module(&name[..i]).is_some()) {
            return Target::Missing(name.to_owned());
        }
        Target::Module(ModuleRef::External(self.external(name)))
    }

    fn external(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.external_ids.get(name) {
            return id;
        }
        let id = self.externals.len() as u32;
        self.externals.push(name.to_owned());
        self.external_ids.insert(name.to_owned(), id);
        id
    }

    /// `m` and the parent packages importing it loads first.
    fn with_parents(&mut self, m: ModuleRef, out: &mut BTreeSet<ModuleRef>) {
        let name = self.name(m).to_owned();
        for (i, _) in name.match_indices('.') {
            let parent = &name[..i];
            match m {
                ModuleRef::Project(_) => {
                    if let Some(&p) = self.module_ids.get(parent) {
                        out.insert(ModuleRef::Project(p));
                    }
                }
                ModuleRef::External(_) => {
                    out.insert(ModuleRef::External(self.external(parent)));
                }
            }
        }
        out.insert(m);
    }

    /// The parent packages importing the module `m` of a site loads first, outermost first:
    /// those of the project's that the project has, and every one outside it.
    pub fn parents(&self, m: ModuleRef) -> Vec<ModuleRef> {
        let name = self.name(m);
        let prefixes = name.match_indices('.').map(|(i, _)| &name[..i]);
        match m {
            ModuleRef::Project(_) => (prefixes.filter_map(|p| self.module_ids.get(p)))
                .map(|&p| ModuleRef::Project(p))
                .collect(),
            ModuleRef::External(_) => (prefixes.filter_map(|p| self.external_ids.get(p)))
                .map(|&p| ModuleRef::External(p))
                .collect(),
        }
    }

    /// Whether `package` is the project module `m` or a package holding it: Python loads it
    /// before `m` runs, so no import in `m` loads it anew.
    pub fn holds(&self, package: ModuleRef, m: u32) -> bool {
        matches!(package, ModuleRef::Project(_))
            && (self.modules[m as usize].strip_prefix(self.name(package)))
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
    }

    /// The project modules site `j` of the project module `m` loads anew, each with the hop
    /// into it: the parent packages of the module the site certainly loads
    /// ([`Target::certain`]) that do not hold `m` ([`Self::holds`]), outermost first, then that
    /// module unless it is `m` itself. A package holding `m` that the site names itself (`from
    /// pkg import name` in `pkg.a`) is a hop: the site reads it while it may still be loading.
    pub fn hops(&self, m: u32, j: usize) -> Vec<ImportHop> {
        let certain = self.sites(m).and_then(|s| s[j].1.certain());
        let Some(t) = certain.and_then(ModuleRef::project) else {
            return Vec::new();
        };
        let hop = |to, parent| ImportHop {
            from: m,
            site: j,
            to,
            parent,
        };
        let parents = (self.parents(ModuleRef::Project(t)).into_iter())
            .filter(|&p| !self.holds(p, m))
            .filter_map(ModuleRef::project)
            .map(|p| hop(p, true));
        parents.chain((t != m).then(|| hop(t, false))).collect()
    }

    /// The groups of project modules importing each other through the sites `counts` takes:
    /// every strongly connected component of two or more modules of their [`Self::hops`], in
    /// the order of their smallest modules, each with a shortest cycle through that module.
    /// Linear in the modules and sites, and unmetered: Tarjan's algorithm takes each hop once,
    /// and the search for a cycle the modules of its own group, which no other group shares.
    pub fn cycle_groups(&self, counts: impl Fn(&ImportSite) -> bool) -> Vec<CycleGroup> {
        let hops: Vec<Vec<ImportHop>> = (0..self.modules.len() as u32)
            .map(|m| self.counted_hops(m, &counts))
            .collect();
        let successors: Vec<Vec<usize>> = (hops.iter())
            .map(|hs| hs.iter().map(|h| h.to as usize).collect())
            .collect();
        let mut groups: Vec<Vec<u32>> = scc::tarjan_scc(self.modules.len(), &successors)
            .into_iter()
            .filter(|component| component.len() > 1)
            .map(|component| {
                let mut members: Vec<u32> = component.into_iter().map(|m| m as u32).collect();
                members.sort_unstable();
                members
            })
            .collect();
        groups.sort_unstable();
        let budget = Budget::unmetered("import_cycles");
        let mut member = vec![false; self.modules.len()];
        groups
            .into_iter()
            .map(|members| {
                members.iter().for_each(|&m| member[m as usize] = true);
                let cycle = shortest_cycle(&budget, &hops, &member, members[0]);
                members.iter().for_each(|&m| member[m as usize] = false);
                CycleGroup { members, cycle }
            })
            .collect()
    }

    /// The hop of module `m` into each module the sites `counts` takes load anew, by the
    /// module it enters, ascending. Of the sites loading one module, the hop is the first in
    /// source order of those that run earliest ([`Timing`]), an import of the module before
    /// a load of it as a parent package: which sites load a module, and when, chooses it, not
    /// the order they are written in.
    fn counted_hops(&self, m: u32, counts: &impl Fn(&ImportSite) -> bool) -> Vec<ImportHop> {
        // A site whose timing is open ranks after every known one.
        type Rank = ((bool, Timing), bool, usize);
        let mut into: BTreeMap<u32, (Rank, ImportHop)> = BTreeMap::new();
        for (j, (site, _)) in self.sites(m).into_iter().flatten().enumerate() {
            if !counts(site) {
                continue;
            }
            let timing = (site.timing.is_open(), site.timing.open_means(Timing::Body));
            for hop in self.hops(m, j) {
                let rank = (timing, hop.parent, j);
                let kept = into.entry(hop.to).or_insert((rank, hop));
                if rank < kept.0 {
                    *kept = (rank, hop);
                }
            }
        }
        into.into_values().map(|(_, hop)| hop).collect()
    }

    /// The name of `m`.
    pub fn name(&self, m: ModuleRef) -> &str {
        match m {
            ModuleRef::Project(i) => &self.modules[i as usize],
            ModuleRef::External(i) => &self.externals[i as usize],
        }
    }

    /// The file defining the project module `m`.
    pub fn path(&self, m: u32) -> &'i Path {
        self.paths[m as usize]
    }

    /// The id of the project module named `name`.
    pub fn module(&self, name: &str) -> Option<u32> {
        self.module_ids.get(name).copied()
    }

    /// The sites of the project module `m` and what each loads (`None`: heki has no import
    /// facts of it).
    pub fn sites(&self, m: u32) -> Option<&[(&'i ImportSite, Target)]> {
        self.sites[m as usize].as_deref()
    }

    /// A budget for the walks over the graph: [`Budget::STEPS_PER_UNIT`] per module and site.
    pub fn budget(&self, owner: &'static str) -> Budget {
        Budget::of_graph(owner, self.size)
    }

    /// What loading the project module `m` loads in one step: the module each of its sites
    /// that certainly runs while it loads loads ([`ImportSite::loads_while_its_module_loads`],
    /// [`Target::certain`]), with the parent packages it loads first. A site on one branch, one
    /// an `ImportError` handler guards, and one whose timing or module heki cannot tell, add
    /// nothing: loading `m` loads at least these.
    pub fn loads(&self, m: u32) -> &[ModuleRef] {
        &self.loads[m as usize]
    }

    /// The modules loading the project module `start` loads while it loads, besides itself,
    /// at least: [`Self::loads`] of it and, through the project's modules, of what they load.
    /// Open once `budget` is spent.
    pub fn loaded_while_loading(&self, budget: &Budget, start: u32) -> Answer<Vec<ModuleRef>> {
        let mut seen: Vec<ModuleRef> = Vec::new();
        let walked = reach(
            budget,
            [ModuleRef::Project(start)],
            |m, out| {
                if let ModuleRef::Project(i) = m {
                    out.extend(self.loads(i));
                }
            },
            |m| {
                seen.push(m);
                Visit::<()>::Expand
            },
        );
        walked.map(|_| {
            seen.retain(|&m| m != ModuleRef::Project(start));
            seen.sort();
            seen
        })
    }

    /// [`ImportTime`] of every project module, each asked in the order of the modules.
    pub fn import_time(&self, budget: &Budget) -> ImportTime {
        let closure: Closure<u32, BitSet> = Closure::new();
        let own = |m: u32| {
            let mut fact = BitSet::new();
            for load in self.loads(m) {
                if let ModuleRef::External(e) = load {
                    fact.insert(*e as usize);
                }
            }
            fact
        };
        let next = |m: u32, out: &mut Vec<u32>| {
            out.extend(self.loads(m).iter().filter_map(|load| match load {
                ModuleRef::Project(p) => Some(*p),
                ModuleRef::External(_) => None,
            }));
        };
        let externals = (0..self.modules.len() as u32)
            .map(|m| closure.ask(budget, m, own, next))
            .collect();
        ImportTime { externals }
    }
}

/// The import-time closure ([`ImportGraph::import_time`]): per project module, the modules
/// outside the project that loading it loads while it loads, at least, through the project's
/// modules ([`ImportGraph::loads`]; `walk::Closure` over the graph, the members of an import
/// cycle sharing one fact).
pub struct ImportTime {
    externals: Vec<Answer<Rc<BitSet>>>,
}

impl ImportTime {
    /// The external modules loading the project module `m` loads, by name; open once the
    /// budget was spent.
    pub fn externals<'g>(&self, graph: &'g ImportGraph<'_>, m: u32) -> Answer<Vec<&'g str>> {
        self.externals[m as usize].as_ref().map(|loaded| {
            let mut names: Vec<&str> = (loaded.iter())
                .map(|e| graph.name(ModuleRef::External(e as u32)))
                .collect();
            names.sort_unstable();
            names
        })
    }
}

/// A step of an import chain: site `site` of the project module `from` (its index in
/// [`ImportGraph::sites`]) loads the project module `to`, the module it imports or, first, a
/// parent package of it (`parent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportHop {
    pub from: u32,
    pub site: usize,
    pub to: u32,
    pub parent: bool,
}

/// A group of project modules importing each other ([`ImportGraph::cycle_groups`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleGroup {
    /// Ascending, which is the order of their names.
    pub members: Vec<u32>,
    /// A shortest cycle from the smallest member back to it: each hop enters the module the
    /// next one leaves, and the last hop, which closes the cycle, enters the first member.
    pub cycle: Vec<ImportHop>,
}

/// A shortest cycle of `hops` from `start` back to it through the modules `member` marks,
/// breadth first, each module's hops taken in the order of the modules they enter: the cycle
/// closes at the first module the search reaches that imports `start`. One step of the
/// unmetered `budget` per module reached.
fn shortest_cycle(
    budget: &Budget,
    hops: &[Vec<ImportHop>],
    member: &[bool],
    start: u32,
) -> Vec<ImportHop> {
    let mut into: FxHashMap<u32, ImportHop> = FxHashMap::default();
    let closing: Cell<Option<ImportHop>> = Cell::new(None);
    let searched = reach_breadth_first::<_, ()>(
        budget,
        [start],
        |m, next| {
            for hop in hops[m as usize].iter().filter(|h| member[h.to as usize]) {
                if hop.to == start {
                    // The search stops at its next module: no other hop closes the cycle.
                    closing.set(Some(*hop));
                } else if let Entry::Vacant(entry) = into.entry(hop.to) {
                    entry.insert(*hop);
                    next.push(hop.to);
                }
            }
        },
        |_| match closing.get() {
            Some(_) => Visit::Stop(()),
            None => Visit::Expand,
        },
    );
    searched.open_means(None);
    let last = closing
        .get()
        .expect("a group of modules has a cycle through each");
    let mut cycle = vec![last];
    let mut at = last.from;
    while at != start {
        let hop = into[&at];
        cycle.push(hop);
        at = hop.from;
    }
    cycle.reverse();
    cycle
}
