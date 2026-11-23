//! Import facts (ADR-022): every place a module imports another, when it runs, and what loading
//! a module loads while it loads. This module is the one home of which module an import site
//! loads ([`ImportGraph::target`]); no rule resolves an import itself.
//!
//! The facts of each file ([`ModuleImports`]) are local and IR-free, kept in
//! [`crate::FileSummary::imports`] with [`crate::OptInFacts::imports`]. The graph is built per
//! check from them and the project declarations, and never kept in the index (ADR-017).

use crate::resolver::{FromImport, ProjectDecls, resolve_relative_module};
use crate::{FileSummary, ModuleInput, environment, is_stub};
use heki_ast::Span;
use heki_ir::walk::{Answer, BitSet, Budget, Closure, FnShape, Visit, reach};
use heki_ir::{BlockId, FunctionId, InstKind, IrFunction, LastBindings, ModuleIr};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::cell::OnceCell;
use std::collections::BTreeSet;
use std::rc::Rc;

/// When an import site runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Timing {
    /// While its module loads: in the module body or a class body the module body runs, or a
    /// lazy import whose name the module body reads.
    Body,
    /// When the name it binds is first used (PEP 810), which the module body does not do.
    Lazy,
    /// When the function holding it runs.
    Function,
    /// Never: under `if TYPE_CHECKING:`.
    TypeChecking,
}

impl Timing {
    pub fn as_str(self) -> &'static str {
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
                match index.get(&id) {
                    Some(&at) => out[at].guarded |= guarded,
                    None => {
                        index.insert(id, out.len());
                        let body = ir.function(FunctionId(id));
                        out.push(ClassBody { body, guarded });
                    }
                }
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
    /// The names the module body reads that a lazy import binds, which the read loads there,
    /// and whether one such read runs on every path through the body.
    lazy_reads: FxHashMap<&'m str, bool>,
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
            lazy_reads: FxHashMap::default(),
            guarded_classes: Vec::new(),
        };
        for (runner, f) in runners.iter().enumerate() {
            out.place(ir, runner, f);
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
                for inst in &bb.insts {
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
                        } if f.is_module_body => {
                            let unconditional = !in_class && always(bb.id);
                            *self.lazy_reads.entry(name.as_str()).or_default() |= unconditional;
                        }
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
    let timing = match statement {
        _ if in_type_checking => Answer::Known(Timing::TypeChecking),
        _ if !runner.is_module_body => Answer::Known(Timing::Function),
        Statement::Import(imp) if imp.lazy_open => Answer::Open,
        // A lazy import loads where the module body first reads its name.
        Statement::Import(imp) if imp.is_lazy => {
            match placements.lazy_reads.get(imp.bound_name.as_str()) {
                Some(&read_on_every_path) => {
                    unconditional = read_on_every_path;
                    Answer::Known(Timing::Body)
                }
                None => Answer::Known(Timing::Lazy),
            }
        }
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
        guarded: at.guarded,
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
            sites: Vec::new(),
            loads: Vec::new(),
            size: 0,
        };
        for i in 0..graph.modules.len() {
            let facts = owners[graph.modules[i].as_str()].imports();
            let resolved: Option<Vec<(&'i ImportSite, Target)>> = facts.map(|facts| {
                (facts.sites.iter())
                    .map(|site| (site, graph.target(site, decls)))
                    .collect()
            });
            let mut loads: BTreeSet<ModuleRef> = BTreeSet::new();
            for (site, target) in resolved.iter().flatten() {
                if site.timing != Answer::Known(Timing::Body) || site.guarded {
                    continue;
                }
                if let Some(m) = target.certain() {
                    graph.with_parents(m, &mut loads);
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

    /// The name of `m`.
    pub fn name(&self, m: ModuleRef) -> &str {
        match m {
            ModuleRef::Project(i) => &self.modules[i as usize],
            ModuleRef::External(i) => &self.externals[i as usize],
        }
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

    /// What loading the project module `m` loads in one step: the module each of its `body`
    /// sites loads ([`Target::certain`]), wherever the site is written (a claim from it says
    /// where the import is, never that a branch is taken), with the parent packages it loads
    /// first. A site an `ImportError` handler guards, and one whose timing or module heki
    /// cannot tell, adds nothing: loading `m` loads at least these.
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
