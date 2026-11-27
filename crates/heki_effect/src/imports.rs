//! Import facts (ADR-022): every place a module imports another, when it runs, and what loading
//! a module loads while it loads. This module is the one home of which module an import site
//! loads ([`ImportGraph::target`]); no rule resolves an import itself.
//!
//! The facts of each file ([`ModuleImports`]) are local and IR-free, kept in
//! [`crate::FileSummary::imports`] with [`crate::OptInFacts::imports`]. The graph is built per
//! check from them and the project declarations, and never kept in the index (ADR-017).

mod circular;
mod cuts;
mod distributions;
mod handlers;
mod loading;

pub use circular::CircularFailure;
pub use cuts::{ChainStep, path_cuts};
pub use distributions::{DeclaredOnly, Distributions, Only};
pub use handlers::ImportFailure;
pub(crate) use handlers::ImportHandlers;
use loading::LoadReads;

use crate::resolver::{FromImport, ProjectDecls, resolve_relative_module};
use crate::{CallSite, FileSummary, ModuleInput, is_stub, scc};
use heki_ast::Span;
use heki_ir::walk::{Answer, BitSet, Budget, Closure, FnShape, Visit, reach, reach_breadth_first};
use heki_ir::{
    BasicBlock, BlockId, BuiltinKind, CompareOpKind, FunctionId, Inst, InstKind, IrFunction,
    LastBindings, ModuleIr, Terminator,
};
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
    /// A call that may be one importing a module by name: `loader.import_module(name)` on an
    /// object of unknown class, or a call of a name that may be bound to `importlib.import_module`
    /// or `__import__` on some path. Its modules are open; it adds no edge to the import graph.
    MaybeByName,
}

impl ImportForm {
    /// Whether the site is a call that imports a module by name, or may be one.
    pub fn is_by_name(&self) -> bool {
        matches!(self, Self::ByName | Self::MaybeByName)
    }
}

/// How an import site writes the names of the modules it imports.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Spelling {
    /// By the full name: `import a.b`, `from a import b`, `importlib.import_module("a.b")`, and
    /// a relative name anchored at a package the code spells (`import_module(".b", "a")`).
    #[default]
    Absolute,
    /// Relative to the site's own module, which stays right in a copy of its package under
    /// another name: `from . import b`, `from ..a import b`, and a relative name anchored at the
    /// module's own `__package__` or `__name__` (`import_module(".b", __package__)`).
    Relative,
    /// A by-name call giving both kinds of names: per module of [`ImportSite::modules`],
    /// whether only relative names give it.
    Mixed(Vec<bool>),
}

impl Spelling {
    /// The spelling of a by-name call, from whether only relative names give each of its
    /// modules.
    pub(crate) fn of_each(relative: Vec<bool>) -> Self {
        if !relative.contains(&true) {
            Self::Absolute
        } else if !relative.contains(&false) {
            Self::Relative
        } else {
            Self::Mixed(relative)
        }
    }

    /// Whether the site names module `i` of its [`ImportSite::modules`] only relatively.
    pub fn relative(&self, i: usize) -> bool {
        match self {
            Self::Absolute => false,
            Self::Relative => true,
            Self::Mixed(each) => each[i],
        }
    }
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
    /// How the site writes the names of `modules`; `Absolute` while they are open.
    #[serde(default)]
    pub spelling: Spelling,
    pub timing: Answer<Timing>,
    /// The widest failure of the import that a handler takes, so that the code after the
    /// handler runs on ([`ImportHandlers::taken_at`]): around the import, around the class
    /// statement whose body holds it, or, for a lazy import, around every read that may load
    /// it. `None` when every failure leaves the function.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handled: Option<ImportFailure>,
    /// The site runs on every path through its function's body to a normal return, outside
    /// loops; never established in a class body.
    pub unconditional: bool,
    /// A lazy import (PEP 810): it loads where a read of its name first runs, not where it is
    /// written.
    #[serde(default)]
    pub lazy: bool,
    /// PEP 810 allows `lazy` on the statement ([`heki_semantic::ModuleImport::lazy_allowed`]);
    /// never on a star import or a call.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lazy_allowed: bool,
    /// Code that runs while the module loads may read what the statement binds
    /// ([`loading::LoadReads`]): moved into a function, the import leaves that read unbound,
    /// and made `lazy`, it loads at that read, still while the module loads. So may any code
    /// read the class attribute an import in a class body binds, which heki does not follow.
    /// Established only for an `import` or `from` statement the module body runs, not lazy.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_while_loading: bool,
    /// The name the statement binds in its module, for one whose binding code of the module
    /// reads only after the module has loaded and other modules see once it has: an `import`
    /// or `from` statement the module body runs, not lazy, not [`Self::read_while_loading`],
    /// and not rebound by a later statement that always runs (`from time import sleep`, then
    /// `def sleep(n): ...`), after which the name no longer holds the import. Another module
    /// may read it ([`ImportGraph::read_elsewhere`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binds: Option<String>,
}

impl ImportSite {
    /// Whether a handler takes `failure` of the import ([`Self::handled`]).
    pub fn handles(&self, failure: ImportFailure) -> bool {
        self.handled >= Some(failure)
    }

    /// Whether the site imports whenever its module loads: in the body, on every path through
    /// it, and with no handler taking a failure of it. A site on one branch (`if sys.platform
    /// == "win32": import winreg`) is not one.
    pub fn loads_while_its_module_loads(&self) -> bool {
        self.timing == Answer::Known(Timing::Body) && self.unconditional && self.handled.is_none()
    }

    /// Whether the site imports where it is written whenever its module loads, and its
    /// `failure` leaves the module: in the module body, on every path through it, not lazy, and
    /// no handler takes that failure. A rule asks for the failure it claims: a handler of only
    /// `ModuleNotFoundError` takes a missing module and passes a plain `ImportError` on.
    pub fn always_imports_leaving(&self, failure: ImportFailure) -> bool {
        self.timing == Answer::Known(Timing::Body)
            && self.unconditional
            && !self.lazy
            && !self.handles(failure)
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
///
/// What the module body does between its imports is told by the index of a site in
/// [`Self::sites`]: "before site `k`" means before site `k` and every later one starts, which
/// stays the same wherever the code moves as long as the order does.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleImports {
    /// In source order.
    pub sites: Vec<ImportSite>,
    /// The module globals the module may bind before one of its sites, each with the first
    /// site it may be bound before: a store of the module body (an assignment, a definition,
    /// an import), or a store in a function or a class body under `global`, which may run at
    /// any time (site 0). A name bound only after every site is not listed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bound_before: BTreeMap<String, u32>,
    /// The first site before which any name of the module may be bound: by a star import of the
    /// module body, or, from the start, by `exec`, a change through `globals()`, or `vars()` or
    /// `locals()` in the module body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub any_bound_before: Option<u32>,
    /// The first site before which the module body may run code other than its imports and its
    /// stores of names ([`may_run_code`]): a call, a class statement, an operator, an attribute
    /// or item access; `sites.len()` when it does only after every site, `None` when it never
    /// does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_before: Option<u32>,
    /// What the module reads of the namespaces of the modules its statements import, by dotted
    /// name (`m.Name`; `m.*` for every public name; `*` alone for any public name of any
    /// module), each with the earliest time it does ([`loading::LoadReads::names_read`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub names_read: BTreeMap<String, ReadTime>,
}

/// When a module reads a name of another module's namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReadTime {
    /// While the reading module loads.
    Loading,
    /// Only after it has: in a function its loading does not run.
    Later,
}

impl ModuleImports {
    /// Whether the two give the same graph, wherever the sites are written (Early Cutoff:
    /// related locations move with the files that show them).
    pub fn same_facts(&self, other: &Self) -> bool {
        self.sites.len() == other.sites.len()
            && (self.sites.iter().zip(&other.sites)).all(|(a, b)| a.unplaced() == b.unplaced())
            && self.bound_before == other.bound_before
            && self.any_bound_before == other.any_bound_before
            && self.code_before == other.code_before
            && self.names_read == other.names_read
    }

    /// Whether the module may have bound `name` before its site `k` runs: it binds the name
    /// there, binds `__getattr__`, which `from` imports fall back to, or may bind any name.
    pub fn may_bind_before(&self, name: &str, k: usize) -> bool {
        let before = |first: Option<u32>| first.is_some_and(|first| first as usize <= k);
        before(self.bound_before.get(name).copied())
            || before(self.bound_before.get("__getattr__").copied())
            || before(self.any_bound_before)
    }

    /// Whether the module body may run code before its site `k` ([`Self::code_before`]).
    pub fn runs_code_before(&self, k: usize) -> bool {
        self.code_before.is_some_and(|first| first as usize <= k)
    }
}

/// The widest failure of a statement's import the handlers around it take
/// ([`ImportSite::handled`]). Handlers around the statement and around what runs it (a class
/// statement) take what either takes: the wider one.
type Handled = Option<ImportFailure>;

/// A class body a function runs, and the failure of its imports the handlers around its class
/// statement take (in the function, or around the statement of an enclosing class body).
struct ClassBody<'m> {
    body: &'m IrFunction,
    handled: Handled,
}

/// The class bodies `f` runs, nested ones included, each once (a class statement in a `finally`
/// body is made once per way out of its `try`; one handled copy handles it).
fn class_bodies<'m>(ir: &'m ModuleIr, f: &'m IrFunction) -> Vec<ClassBody<'m>> {
    let mut out: Vec<ClassBody<'m>> = Vec::new();
    let mut index: FxHashMap<u32, usize> = FxHashMap::default();
    // Each node is a function and the failure the handlers around the class statements leading
    // to it take.
    let next = |(id, outer): (u32, Handled), out: &mut Vec<(u32, Handled)>| {
        let g = ir.function(FunctionId(id));
        let shape = FnShape::new(g);
        let handlers = ImportHandlers::new(&shape, &ir.constants.shadowed);
        for bb in &g.blocks {
            for inst in &bb.insts {
                if let InstKind::MakeClosure {
                    func_id: Some(body),
                    ..
                } = &inst.kind
                    && ir.function(*body).is_class_body
                {
                    out.push((body.0, outer.max(handlers.taken_at(bb.id))));
                }
            }
        }
    };
    // Unmetered: a function's class statements are walked once per failure they take, and the
    // walk is never open.
    reach(
        &Budget::unmetered("import_sites"),
        [(f.id.0, None)],
        next,
        |(id, handled)| {
            if id != f.id.0 {
                let at = *index.entry(id).or_insert_with(|| {
                    out.push(ClassBody {
                        body: ir.function(FunctionId(id)),
                        handled: None,
                    });
                    out.len() - 1
                });
                out[at].handled = out[at].handled.max(handled);
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
    handled: Handled,
    unconditional: bool,
}

/// Where a module's import statements run, from its IR.
struct Placements<'m> {
    /// The functions that are no class bodies: each runs its own body and its class bodies.
    runners: Vec<&'m IrFunction>,
    /// By the span of the imported name: one entry for the copies of a statement in a
    /// `finally` body, handled as the copy whose handlers take most, and unconditional when one
    /// copy is.
    placed: FxHashMap<Span, Placed>,
    /// The reads of names a lazy import binds in the module body and the class bodies it runs.
    lazy_reads: Vec<LazyRead<'m>>,
    /// By the span of the imported name, what the module body's reads load of a lazy import
    /// ([`lazy_loads`]).
    lazy_loads: FxHashMap<Span, LazyLoad>,
    /// The class bodies with a handler around their class statement that takes a failure of
    /// their imports.
    handled_classes: Vec<(Span, Handled)>,
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
            handled_classes: Vec::new(),
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
        let shape = FnShape::new(f);
        // Built for the functions holding an import only.
        let ends: OnceCell<LastBindings> = OnceCell::new();
        let always = |bb: BlockId| {
            ends.get_or_init(|| LastBindings::of(&shape))
                .always_runs(&shape, bb)
        };
        let shadowed = &ir.constants.shadowed;
        let own = ImportHandlers::new(&shape, shadowed);
        let bodies = class_bodies(ir, f);
        (self.handled_classes).extend(
            (bodies.iter())
                .filter(|c| c.handled.is_some())
                .map(|c| (c.body.span, c.handled)),
        );
        let holders = std::iter::once((f, false, None))
            .chain(bodies.iter().map(|c| (c.body, true, c.handled)));
        for (g, in_class, outer) in holders {
            let class_shape;
            let class_handlers;
            let handlers = if in_class {
                class_shape = FnShape::new(g);
                class_handlers = ImportHandlers::new(&class_shape, shadowed);
                &class_handlers
            } else {
                &own
            };
            for bb in &g.blocks {
                for (index, inst) in bb.insts.iter().enumerate() {
                    match &inst.kind {
                        InstKind::Import { .. } => {
                            let handled = outer.max(handlers.taken_at(bb.id));
                            let unconditional = !in_class && always(bb.id);
                            self.add(inst.span, runner, in_class, handled, unconditional);
                        }
                        InstKind::LoadGlobal {
                            name,
                            is_lazy_import: true,
                            ..
                        } if f.is_module_body => self.lazy_reads.push(LazyRead {
                            name,
                            at: (!in_class).then_some((bb.id, index)),
                            unconditional: !in_class && always(bb.id),
                            handled: outer.max(handlers.taken_at(bb.id)),
                        }),
                        _ => {}
                    }
                }
            }
        }
        if f.is_module_body {
            for &(bb, span) in &ir.star_imports {
                self.add(span, runner, false, own.taken_at(bb), always(bb));
            }
        }
    }

    fn add(&mut self, span: Span, runner: usize, in_class: bool, handled: Handled, always: bool) {
        let at = self.placed.entry(span).or_insert(Placed {
            runner,
            in_class,
            handled: None,
            unconditional: false,
        });
        at.handled = at.handled.max(handled);
        at.unconditional |= always;
    }
}

/// A read of a name a lazy import binds, in the module body or a class body it runs.
struct LazyRead<'m> {
    name: &'m str,
    /// The block of the module body and the instruction; `None` in a class body.
    at: Option<(BlockId, usize)>,
    unconditional: bool,
    handled: Handled,
}

/// What the module body's reads load of one lazy import. An entry without `read` comes from a
/// read whose binding heki cannot tell, which may load it.
struct LazyLoad {
    /// A read its binding may reach: the first such read loads the module there.
    read: bool,
    /// A read only its binding reaches runs on every path through the body.
    unconditional: bool,
    /// The failure the handlers around every read that may load it take: the narrowest of
    /// theirs, since any of them may be the read that loads it.
    handled: Handled,
}

impl Default for LazyLoad {
    /// Before any read: each read that may load it narrows `handled` to what its own handlers
    /// take.
    fn default() -> Self {
        Self {
            read: false,
            unconditional: false,
            handled: Some(ImportFailure::AnyImportError),
        }
    }
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
                    load.handled = load.handled.min(read.handled);
                }
            }
            Answer::Open => {
                for import in (0..name.all.len()).filter_map(|b| name.import(b)) {
                    let load = out.entry(import).or_default();
                    load.handled = load.handled.min(read.handled);
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
/// in the IR, and `by_name`, the by-name calls the summary found among the file's `calls`.
/// `None` for a typing stub, whose imports never run.
pub(crate) fn module_imports(
    input: &ModuleInput<'_>,
    mut by_name: Vec<ImportSite>,
    calls: &[CallSite],
) -> Option<ModuleImports> {
    if is_stub(&input.path) {
        return None;
    }
    let placements = Placements::of(input.ir);
    let statements = (input.semantic.imports.iter())
        .map(|imp| (imp.span, Statement::Import(imp)))
        .chain((input.semantic.star_imports.iter()).map(|star| (star.span, Statement::Star(star))));
    let reads = OnceCell::new();
    let reads = || reads.get_or_init(|| LoadReads::of(input, calls));
    let placed: Vec<(Statement<'_>, ImportSite)> = statements
        .filter_map(|(span, statement)| {
            let at = placements.placed.get(&span)?;
            let mut site = statement_site(input, &placements, span, statement, at);
            if let Statement::Import(imp) = statement
                && site.timing == Answer::Known(Timing::Body)
                && !site.lazy
            {
                site.read_while_loading = reads().reads(imp);
                site.binds = (!site.read_while_loading && imp.rebound_at.is_none())
                    .then(|| imp.bound_name.clone());
            }
            Some((statement, site))
        })
        .collect();
    let names_read = if placed.is_empty() {
        BTreeMap::new()
    } else {
        let statements: Vec<(Statement<'_>, &ImportSite)> = placed
            .iter()
            .map(|(statement, site)| (*statement, site))
            .collect();
        reads().names_read(input.ir, &statements)
    };
    let mut sites: Vec<ImportSite> = placed.into_iter().map(|(_, site)| site).collect();
    for site in &mut by_name {
        for (class, handled) in &placements.handled_classes {
            if class.start <= site.span.start && site.span.end <= class.end {
                site.handled = site.handled.max(*handled);
            }
        }
    }
    sites.extend(by_name);
    sites.sort_by_key(|s| s.span.start);
    let order = BodyOrder::of(input.ir, &sites);
    Some(ModuleImports {
        bound_before: order.bound_before,
        any_bound_before: order.any_bound_before,
        code_before: order.code_before,
        sites,
        names_read,
    })
}

/// When the module body binds its names and runs code, by the first site each happens before
/// ([`ModuleImports::bound_before`], [`ModuleImports::any_bound_before`],
/// [`ModuleImports::code_before`]).
struct BodyOrder {
    bound_before: BTreeMap<String, u32>,
    any_bound_before: Option<u32>,
    code_before: Option<u32>,
}

impl BodyOrder {
    fn of(ir: &ModuleIr, sites: &[ImportSite]) -> Self {
        // The first site starting after `offset`: what happens at `offset` happens before it.
        let first_after = |offset: u32| sites.partition_point(|s| s.span.start <= offset) as u32;
        let body = ir.function(ir.module_fn);
        let mut bound_before: BTreeMap<String, u32> = BTreeMap::new();
        for f in &ir.functions {
            for inst in f.blocks.iter().flat_map(|b| &b.insts) {
                let InstKind::StoreGlobal { name, .. } = &inst.kind else {
                    continue;
                };
                // A function or a class body binding the name under `global` may run at any
                // time: called from code that runs before any site.
                let first = if f.id == body.id {
                    first_after(inst.span.start)
                } else {
                    0
                };
                if (first as usize) < sites.len() {
                    let at = bound_before.entry(name.clone()).or_insert(first);
                    *at = (*at).min(first);
                }
            }
        }
        let shape = FnShape::new(body);
        let reads_namespace = |kind: &InstKind| {
            matches!(kind, InstKind::Call { callee, args, kwargs, .. }
                if args.is_empty() && kwargs.is_empty() && matches!(callee.as_str(), "vars" | "locals"))
        };
        let dynamic = ir.constants.dynamic
            || (body.blocks.iter().flat_map(|b| &b.insts)).any(|i| reads_namespace(&i.kind));
        let star = (ir.star_imports.iter()).map(|(_, span)| first_after(span.start));
        let any_bound_before = (dynamic.then_some(0)).into_iter().chain(star).min();
        let code = (body.blocks.iter()).flat_map(|b| {
            let insts = (b.insts.iter())
                .filter(|i| may_run_code(ir, &i.kind))
                .map(|i| i.span.start);
            insts.chain(
                tests_truth(&shape, b)
                    .then(|| b.term_span.map(|s| s.start))
                    .flatten(),
            )
        });
        Self {
            bound_before,
            any_bound_before,
            code_before: code.min().map(first_after),
        }
    }
}

/// Whether the module body's instruction `kind` may run code other than its imports and its
/// stores of names: a call, a class statement (which runs the metaclass and
/// `__init_subclass__`), an operator, an attribute or item access, iteration, formatting, a
/// `with` statement, or a non-empty `dict` or `set` display (which hashes its items). Loads of
/// names, constants, `list` and `tuple` displays, and making a function run none (R7).
fn may_run_code(ir: &ModuleIr, kind: &InstKind) -> bool {
    match kind {
        InstKind::Const { .. }
        | InstKind::Copy { .. }
        | InstKind::LoadGlobal { .. }
        | InstKind::LoadDeref { .. }
        | InstKind::LoadName { .. }
        | InstKind::StoreGlobal { .. }
        | InstKind::StoreDeref { .. }
        | InstKind::Delete { .. }
        | InstKind::CatchException { .. }
        | InstKind::Import { .. } => false,
        InstKind::MakeClosure { func_id, .. } => {
            func_id.is_some_and(|id| ir.function(id).is_class_body)
        }
        InstKind::AllocBuiltin { kind, elems, .. } => {
            matches!(kind, BuiltinKind::Set | BuiltinKind::Dict) && !elems.is_empty()
        }
        InstKind::DictLiteral { entries, .. } => !entries.is_empty(),
        InstKind::Compare { op, .. } => !matches!(op, CompareOpKind::Is | CompareOpKind::IsNot),
        _ => true,
    }
}

/// Whether block `b` ends by testing the truth of a value that may run code (its `__bool__` or
/// `__len__`), or by raising a value (which may make an instance of the class raised): a
/// constant, an identity test, and `TYPE_CHECKING` (read as a global, or as the value its import
/// binds in the module body) run none.
fn tests_truth(shape: &FnShape<'_>, b: &BasicBlock) -> bool {
    match &b.terminator {
        Terminator::Branch { cond, .. } => {
            !matches!(
                shape.inst(shape.copy_root(*cond)).map(|i| &i.kind),
                Some(
                    InstKind::Const { .. }
                        | InstKind::Compare {
                            op: CompareOpKind::Is | CompareOpKind::IsNot,
                            ..
                        }
                )
            ) && !matches!(
                shape.inst(shape.copy_root(*cond)).map(|i| &i.kind),
                Some(
                    InstKind::LoadGlobal { name, .. }
                        | InstKind::LoadName { name, .. }
                        | InstKind::Import { name, .. }
                ) if name == "TYPE_CHECKING"
            )
        }
        Terminator::Raise(Some(_)) => true,
        _ => false,
    }
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
    let mut handled = at.handled;
    let is_lazy = matches!(statement, Statement::Import(imp) if imp.is_lazy);
    let lazy_allowed = matches!(statement, Statement::Import(imp) if imp.lazy_allowed);
    let timing = match statement {
        _ if in_type_checking => Answer::Known(Timing::TypeChecking),
        _ if !runner.is_module_body => Answer::Known(Timing::Function),
        Statement::Import(imp) if imp.lazy_open => Answer::Open,
        // A lazy import loads where the module body first reads its binding.
        Statement::Import(_) if is_lazy => match placements.lazy_loads.get(&span) {
            Some(load) if load.read => {
                unconditional = load.unconditional;
                handled = load.handled;
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
        spelling: if level > 0 {
            Spelling::Relative
        } else {
            Spelling::Absolute
        },
        timing,
        handled,
        unconditional,
        lazy: is_lazy,
        lazy_allowed,
        read_while_loading: false,
        binds: None,
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

/// What the project modules read of each other ([`ImportGraph::read_elsewhere`]).
#[derive(Default)]
struct ReadIndex<'i> {
    /// Per project module and name of it (`*`: every public name), the earliest time another
    /// module reads it.
    names: FxHashMap<(u32, &'i str), ReadTime>,
    /// The modules that may read any public name of any module, each with the earliest time.
    anything: Vec<(u32, ReadTime)>,
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

    /// The failure the site raises whenever it runs: of a module a project package does not
    /// have, `ModuleNotFoundError`; of a relative import climbing past its top-level package,
    /// a plain `ImportError`.
    pub fn failure(&self) -> Option<ImportFailure> {
        match self {
            Self::Missing(name) if name.is_empty() => Some(ImportFailure::AnyImportError),
            Self::Missing(_) => Some(ImportFailure::MissingModule),
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
    /// Per project module, its import facts, with its sites.
    facts: Vec<Option<&'i ModuleImports>>,
    /// Per project module, what loading it loads in one step ([`Self::loads`]).
    loads: Vec<Vec<ModuleRef>>,
    /// The nodes and edges, the size the walks over the graph are budgeted by.
    size: usize,
    /// What the project modules read of each other ([`Self::read_elsewhere`]); made at the
    /// first question.
    reads: OnceCell<ReadIndex<'i>>,
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
            facts: Vec::new(),
            loads: Vec::new(),
            size: 0,
            reads: OnceCell::new(),
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
            graph.facts.push(facts);
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
                let held = decls.holds_module(&full);
                let either = || Target::PackageOrSubmodule {
                    package: ModuleRef::Project(p),
                    submodule: full.clone(),
                };
                match decls.from_import(&package_name, name) {
                    FromImport::Submodule if held => self.module_target(&full, decls),
                    // A name the package binds nowhere is the submodule, which the project's
                    // files lack: Python finds it elsewhere (a compiled extension module) or
                    // not at all.
                    FromImport::Submodule => either(),
                    // The package binds the name to an object of the submodule after importing
                    // it (`from .parse import parse`): until its body has, the import loads the
                    // submodule and takes it, as it does while the package is still loading.
                    FromImport::Open if held => either(),
                    FromImport::Attribute => either(),
                    FromImport::Open => package,
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
    /// module unless it is `m` itself. A package holding `m` is a hop only when the site imports
    /// it, or a name it binds ([`Target::Module`]: `from pkg import name` in `pkg.a`), and so
    /// reads it while it may still be loading.
    pub fn hops(&self, m: u32, j: usize) -> Vec<ImportHop> {
        let target = &self.sites(m).expect("hops are of a module's sites")[j].1;
        let Some(t) = target.certain().and_then(ModuleRef::project) else {
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
        let steps_in = !self.holds(ModuleRef::Project(t), m) || matches!(target, Target::Module(_));
        parents
            .chain((t != m && steps_in).then(|| hop(t, false)))
            .collect()
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

    /// The names the project's modules and packages, namespace packages included, have in a
    /// directory that is no regular package (the project root, a source root, `scripts/`,
    /// `setuptools/_vendor/`, a namespace package's directory): the top-level names a `sys.path`
    /// entry naming that directory makes importable. Under the source roots they are the
    /// project's top-level modules; elsewhere they may be copies of modules from outside the
    /// project that the code puts on `sys.path`.
    pub fn loose_names(&self) -> FxHashSet<&str> {
        let mut names = FxHashSet::default();
        for module in &self.modules {
            let mut holder = None;
            for (i, part) in module.split('.').enumerate() {
                // The directory holding `part`: the root, or a prefix of the name that is no
                // module of the project (a directory without `__init__.py`).
                if holder.is_none_or(|prefix: &str| !self.module_ids.contains_key(prefix)) {
                    names.insert(part);
                }
                let end = (module.match_indices('.').nth(i)).map_or(module.len(), |(at, _)| at);
                holder = Some(&module[..end]);
            }
        }
        names
    }

    /// How many project modules there are: their ids are `0..` it, in the order of their names.
    pub fn module_count(&self) -> u32 {
        self.modules.len() as u32
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

    /// The import facts of the project module `m`, whose sites [`Self::sites`] resolves.
    pub fn facts(&self, m: u32) -> Option<&'i ModuleImports> {
        self.facts[m as usize]
    }

    /// A budget for the walks over the graph: [`Budget::STEPS_PER_UNIT`] per module and site.
    pub fn budget(&self, owner: &'static str) -> Budget {
        Budget::of_graph(owner, self.size)
    }

    /// The earliest time a project module other than `m` reads the name `name` of `m`: by a
    /// `from` import, a star import if the name is public, or an attribute read off what an
    /// import binds ([`ModuleImports::names_read`]). Code of `m` itself reads its globals by
    /// name ([`ImportSite::read_while_loading`]).
    pub fn read_elsewhere(&self, m: u32, name: &str) -> Option<ReadTime> {
        let reads = self.reads.get_or_init(|| {
            let mut reads = ReadIndex::default();
            for (reader, facts) in self.facts.iter().enumerate() {
                let reader = reader as u32;
                let read = (facts.iter()).flat_map(|f| &f.names_read);
                for (dotted, &time) in read {
                    let Some((module, name)) = dotted.rsplit_once('.') else {
                        reads.anything.push((reader, time));
                        continue;
                    };
                    let Some(&of) = self.module_ids.get(module) else {
                        continue;
                    };
                    if of != reader {
                        let earliest = reads.names.entry((of, name)).or_insert(time);
                        *earliest = (*earliest).min(time);
                    }
                }
            }
            reads
        });
        let of = |name: &str| reads.names.get(&(m, name)).copied();
        let public = !name.starts_with('_');
        let anything = (reads.anything.iter()).filter(|&&(reader, _)| reader != m && public);
        (of(name).into_iter())
            .chain(of("*").filter(|_| public))
            .chain(anything.map(|&(_, time)| time))
            .min()
    }

    /// What loading the project module `m` loads in one step: the module each of its sites
    /// that certainly runs while it loads loads ([`ImportSite::loads_while_its_module_loads`],
    /// [`Target::certain`]), with the parent packages it loads first. A site on one branch, one
    /// whose failure a handler takes, and one whose timing or module heki cannot tell, add
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

    /// The project modules that loading any of the project modules `starts` loads while it
    /// loads, through imports a missing module below them fails along with their module
    /// ([`ImportSite::always_imports_leaving`] a missing module) of the modules `enters` takes:
    /// breadth first,
    /// so that each module is reached by a shortest chain of [`Self::hops`] from the start
    /// nearest to it. One walk from all the starts, linear in the modules and sites and
    /// unmetered: each module is visited once.
    pub fn loaded_from(
        &self,
        starts: impl IntoIterator<Item = u32>,
        enters: impl Fn(u32) -> bool,
    ) -> LoadedFrom {
        let starts: Vec<u32> = starts.into_iter().collect();
        let is_start: FxHashSet<u32> = starts.iter().copied().collect();
        let mut into: FxHashMap<u32, ImportHop> = FxHashMap::default();
        let mut order: Vec<u32> = Vec::new();
        reach_breadth_first::<_, ()>(
            &Budget::unmetered("import_dependencies"),
            starts,
            |m, next| {
                for hop in self.loading_hops(m, &enters) {
                    if is_start.contains(&hop.to) {
                        continue;
                    }
                    if let Entry::Vacant(entry) = into.entry(hop.to) {
                        entry.insert(hop);
                        next.push(hop.to);
                    }
                }
            },
            |m| {
                order.push(m);
                Visit::Expand
            },
        )
        .open_means(None);
        LoadedFrom { order, into }
    }

    /// The hops [`Self::loaded_from`] takes from the project module `m`: of its imports a missing
    /// module below them fails along with `m`, into the modules `enters` takes.
    fn loading_hops(&self, m: u32, enters: &impl Fn(u32) -> bool) -> Vec<ImportHop> {
        let sites = self.sites(m).into_iter().flatten().enumerate();
        (sites.filter(|(_, (site, _))| site.always_imports_leaving(ImportFailure::MissingModule)))
            .flat_map(|(j, _)| self.hops(m, j))
            .filter(|hop| enters(hop.to))
            .collect()
    }

    /// The hops of `chain`, a chain [`Self::loaded_from`] finds from one of `starts` (with the
    /// same `enters`), whose site carries every chain from `starts` to the module it ends at
    /// ([`cuts::path_cuts`]), by index into `chain`.
    pub fn loaded_from_cuts(
        &self,
        budget: &Budget,
        starts: impl IntoIterator<Item = u32>,
        enters: impl Fn(u32) -> bool,
        chain: &[ImportHop],
    ) -> Answer<Vec<usize>> {
        let steps: Vec<cuts::ChainStep<u32>> = chain
            .iter()
            .map(|hop| (hop.from, hop.site, hop.to))
            .collect();
        cuts::path_cuts(budget, starts, &steps, |m, out| {
            out.extend((self.loading_hops(m, &enters).into_iter()).map(|hop| (hop.site, hop.to)));
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

/// What loading some project modules loads while they load ([`ImportGraph::loaded_from`]).
pub struct LoadedFrom {
    /// The starts, then the modules reached, in the order of their distance from the starts.
    pub order: Vec<u32>,
    /// The hop into each module reached that is no start.
    into: FxHashMap<u32, ImportHop>,
}

impl LoadedFrom {
    /// The hops from the start nearest to the module `m` reached into it, in order: none for a
    /// start.
    pub fn chain(&self, m: u32) -> Vec<ImportHop> {
        let mut hops = Vec::new();
        let mut at = m;
        while let Some(&hop) = self.into.get(&at) {
            hops.push(hop);
            at = hop.from;
        }
        hops.reverse();
        hops
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
