//! Receiver typing of one function (Advisory lane): a forward fixpoint over the SSA-CFG in
//! reverse postorder, repeated until no variable's type changes.
//!
//! Every variable starts unset: no value reaches it yet. A value that never arrives (a local read
//! before any assignment on that path, `None`, an item of an empty display) leaves what is
//! computed from it unset. Displays, the items added to a local container, and phis skip unset
//! values, so a variable rebound in a loop (`node = node.parent`) or assigned on some paths only
//! (a walrus in `and` / `or`) has the type of the values that reach it. A type only grows (the
//! new one is joined with the old), and classes and nesting depth are bounded, so the fixpoint
//! ends; a function without loops needs one pass, or two when a child scope it builds reads a
//! closure cell the function binds later.
//!
//! Types come from parameter, local, attribute, and return annotations (with the arguments of
//! generic containers), constructors, imports, and `type(obj)`; they flow through copies, the
//! augmented assignments that change a built-in container in place (`xs += ys` gives `xs`),
//! attributes, calls, and the items of containers: iteration, subscripts, unpacking, `dict`
//! views and `get` / `pop`, the builtins that wrap an iterable (`enumerate`, `zip`,
//! `reversed`, `sorted`, `iter`, `next`, `list`, `tuple`, `set`, `frozenset`, `dict`, `filter`,
//! `itertools.chain`), displays, and the items added to a local container (comprehensions build
//! theirs that way). A comprehension or generator expression with a scope of its own iterates
//! the value its enclosing function hands it, and a nested scope reads the closure cells of the
//! functions enclosing it, so those functions are inferred first.

use crate::resolver::{self, Callee, ProjectDecls, TypeCtx, Val};
use crate::types::{Args, Elem, Obj, Ty};
use heki_ast::Span;
use heki_ir::walk::FnShape;
use heki_ir::{
    BinOpKind, BlockId, BuiltinKind, ConstLiteral, DictEntry, FunctionId, FunctionKind, Inst,
    InstKind, IrFunction, ModuleIr, PhiNode, Terminator, UnaryOpKind, VarId,
};
use heki_kb::ArgFact;
use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

/// The arguments a call passes, as written (for the catalog's argument cases).
#[derive(Clone, Copy, Default)]
struct Passed<'c> {
    args: &'c [VarId],
    kwargs: &'c [(Option<String>, VarId)],
    star_from: Option<u8>,
}

/// What the fixpoint knows about a variable.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Fact {
    /// No value reaches it (yet).
    Unset,
    Known(Ty),
    Unknown,
}

impl Fact {
    fn join(&self, other: &Fact) -> Fact {
        match (self, other) {
            (Fact::Unset, f) | (f, Fact::Unset) => f.clone(),
            (Fact::Known(a), Fact::Known(b)) => a.join(b).map_or(Fact::Unknown, Fact::Known),
            _ => Fact::Unknown,
        }
    }

    /// The fact as the item of a container (an unset value adds no item).
    fn elem(&self) -> Elem {
        match self {
            Fact::Unset => Elem::Empty,
            Fact::Known(t) => Elem::Of(t.clone()),
            Fact::Unknown => Elem::Unknown,
        }
    }

    /// The fact as a transfer's result.
    fn as_step(&self) -> Step {
        match self {
            Fact::Unset => Err(Pending),
            Fact::Known(t) => Ok(Some(t.clone())),
            Fact::Unknown => Ok(None),
        }
    }
}

/// The classes `isinstance`'s second argument names when it has type `t`: a class, or a tuple
/// of classes (sorted, deduplicated), each named canonically.
fn isinstance_classes(project: &ProjectDecls, t: &Ty) -> Option<Vec<String>> {
    let class = |t: &Ty| match t {
        Ty::Ref(c) => project
            .declared_ty(&Ty::inst(c.as_str()))
            .and_then(|t| t.obj().map(|o| o.class.clone())),
        _ => None,
    };
    let mut out = match t {
        Ty::Inst(Obj {
            args: Args::Tuple(es),
            ..
        }) => es
            .iter()
            .map(|e| match e {
                Elem::Of(t) => class(t),
                Elem::Empty | Elem::Unknown => None,
            })
            .collect::<Option<Vec<_>>>()?,
        t => vec![class(t)?],
    };
    out.sort();
    out.dedup();
    Some(out)
}

/// The types of one function's variables.
pub(crate) struct Inferred {
    facts: Vec<Fact>,
    /// The declared types of annotated local names (`x: Model = ...`).
    declared: FxHashMap<String, Ty>,
    /// Scopes nested in the function read its types: it builds comprehension or
    /// generator-expression scopes of its own, or binds closure cells.
    nested_read: bool,
    /// The closure cells the function binds, by name: every value bound to each.
    cells: FxHashMap<String, Fact>,
    /// The types `isinstance` checks give variables in the blocks they guard, where they differ
    /// from the variable's own.
    narrowed: FxHashMap<(BlockId, VarId), Ty>,
}

impl Inferred {
    /// The fact of the closure cell `name` the function binds.
    fn cell_fact(&self, name: &str) -> Fact {
        self.cells[name].clone()
    }

    /// The declared type of the local name `name`.
    pub(crate) fn declared(&self, name: &str) -> Option<&Ty> {
        self.declared.get(name)
    }

    pub(crate) fn ty(&self, var: VarId) -> Option<&Ty> {
        match &self.facts[var.0 as usize] {
            Fact::Known(t) => Some(t),
            Fact::Unset | Fact::Unknown => None,
        }
    }

    /// The values consumers see of the variables `isinstance` checks narrow, by block.
    pub(crate) fn narrowed_vals(&self) -> FxHashMap<(BlockId, VarId), Val> {
        self.narrowed
            .iter()
            .filter_map(|(k, t)| Some((*k, t.val()?)))
            .collect()
    }

    /// The classes the second argument of each `isinstance(x, C)` call of `f` names, by that
    /// argument ([`isinstance_classes`]).
    pub(crate) fn isinstance_args(
        &self,
        f: &IrFunction,
        project: &ProjectDecls,
    ) -> FxHashMap<VarId, Vec<String>> {
        let mut out = FxHashMap::default();
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            if let InstKind::Call {
                callee_var: Some(c),
                args,
                ..
            } = &inst.kind
                && args.len() >= 2
                && matches!(self.ty(*c), Some(Ty::Ref(n)) if n == "builtins.isinstance")
                && let Some(classes) = self
                    .ty(args[1])
                    .and_then(|t| isinstance_classes(project, t))
            {
                out.insert(args[1], classes);
            }
        }
        out
    }

    /// The values consumers see, by variable.
    pub(crate) fn vals(&self) -> FxHashMap<VarId, Val> {
        (0u32..)
            .zip(&self.facts)
            .filter_map(|(i, f)| match f {
                Fact::Known(t) => Some((VarId(i), t.val()?)),
                Fact::Unset | Fact::Unknown => None,
            })
            .collect()
    }
}

/// Inference state shared by the functions of one module: a scope nested in a function needs
/// the types of the functions enclosing it (the value a comprehension or generator expression
/// iterates, and the closure cells it reads).
#[derive(Default)]
pub(crate) struct Scopes {
    nesting: OnceCell<Nesting>,
    /// The types of functions whose types nested scopes read.
    enclosing: RefCell<FxHashMap<FunctionId, Rc<Inferred>>>,
    /// What a child scope produces in each run.
    outputs: RefCell<FxHashMap<ChildRun, Elem>>,
}

/// A run of a child scope: the scope, the type of the value it iterates, and the facts of the
/// cells it reads.
type ChildRun = (FunctionId, Option<Ty>, CellFacts);

/// The closure cell of a function, by the function and the cell's name there.
pub(crate) type CellKey = (FunctionId, String);

/// The closure cells a scope can read, by the name it reads each with.
type Cells = BTreeMap<String, CellKey>;

/// The facts of the cells a scope's inference reads, by name (sorted).
type CellFacts = Vec<(String, Fact)>;

/// Where the closure cells of the module's nested scopes are bound.
struct Nesting {
    /// The cells each scope nested in a function can read.
    visible: FxHashMap<FunctionId, Rc<Cells>>,
    /// The names each function's inference reads through cells (sorted): its own reads, and
    /// those of the child scopes it runs (comprehensions and generator expressions with a scope
    /// of their own) that are not its own cells.
    reads: FxHashMap<FunctionId, Vec<String>>,
    /// Cells a nested scope may rebind (`nonlocal`, or an assignment expression in a
    /// comprehension scope before 3.12): they hold more than what the function binding them
    /// binds.
    rebound: FxHashSet<CellKey>,
    /// The cells their function binds only as one of its parameters (`i < 64`), and that
    /// parameter ([`cell_bindings`]).
    param_cells: FxHashMap<CellKey, u8>,
}

/// How a function creates a nested scope.
struct Creation<'f> {
    creator: &'f IrFunction,
    /// The cells of inlined comprehensions' iteration variables of the creator that the scope
    /// reads: the values it captures name them.
    comprehension_cells: Vec<&'f str>,
}

impl Nesting {
    fn of(ir: &ModuleIr) -> Self {
        let mut out = Nesting {
            visible: FxHashMap::default(),
            reads: FxHashMap::default(),
            rebound: FxHashSet::default(),
            param_cells: FxHashMap::default(),
        };
        // No function keeps a cell: no scope reads or rebinds one.
        if ir.functions.iter().all(|f| f.captured_vars.is_empty()) {
            return out;
        }
        let mut created: FxHashMap<FunctionId, Creation<'_>> = FxHashMap::default();
        let mut runs: FxHashMap<FunctionId, Vec<FunctionId>> = FxHashMap::default();
        for g in &ir.functions {
            for inst in g.blocks.iter().flat_map(|b| &b.insts) {
                let (child, captured, runs_it) = match &inst.kind {
                    InstKind::MakeGenExpr {
                        func_id: Some(id),
                        captured,
                        ..
                    } => (*id, &captured[..], true),
                    InstKind::Call {
                        callee_var: None,
                        callee,
                        args,
                        ..
                    } => (comprehension_id(callee), &args[1..], true),
                    InstKind::MakeClosure {
                        func_id: Some(id),
                        captured,
                        ..
                    } => (*id, &captured[..], false),
                    _ => continue,
                };
                let comprehension_cells = captured
                    .iter()
                    .filter_map(|v| g.var_name(*v))
                    .filter(|n| {
                        heki_ir::comprehension_target_source(n).is_some()
                            && g.captured_vars.iter().any(|c| c == n)
                    })
                    .collect();
                created.insert(
                    child,
                    Creation {
                        creator: g,
                        comprehension_cells,
                    },
                );
                if runs_it {
                    runs.entry(g.id).or_default().push(child);
                }
            }
        }
        let none = Rc::new(Cells::new());
        // The cells the scopes each function creates outside its inlined comprehensions can
        // read, shared by them.
        let mut for_nested: FxHashMap<FunctionId, Rc<Cells>> = FxHashMap::default();
        let mut own_reads: FxHashMap<FunctionId, BTreeSet<String>> = FxHashMap::default();
        // The builder numbers a function before the scopes nested in it, so the cells of a
        // scope's creator are known when the scope is reached.
        for f in &ir.functions {
            let cells = match created.get(&f.id) {
                Some(c) => {
                    let p = c.creator;
                    let shared = for_nested
                        .entry(p.id)
                        .or_insert_with(|| nested_in(p, out.visible.get(&p.id).unwrap_or(&none)));
                    with_comprehension_cells(shared, p.id, &c.comprehension_cells)
                }
                None => Rc::clone(&none),
            };
            // Nothing in an unmodeled body creates the scopes nested in it, and a scope's cells
            // come from its creator, so their stores are tied to no cell below: any cell they can
            // see may be rebound.
            if f.is_unmodeled() {
                out.rebound.extend(nested_in(f, &cells).values().cloned());
            }
            for inst in f.blocks.iter().flat_map(|b| &b.insts) {
                match &inst.kind {
                    InstKind::LoadDeref { name, .. } => {
                        own_reads.entry(f.id).or_default().insert(name.clone());
                    }
                    // A store never rebinds a comprehension's iteration variable.
                    InstKind::StoreDeref { name, .. } => {
                        if let Some(cell) = cells.get(name) {
                            out.rebound.insert(cell.clone());
                        }
                    }
                    _ => {}
                }
            }
            if !cells.is_empty() {
                out.visible.insert(f.id, cells);
            }
            for (name, vars) in cell_bindings(f) {
                if let [var] = vars[..]
                    && let Some(i) = f.params.iter().take(64).position(|p| p.var == var)
                {
                    let i = u8::try_from(i).expect("at most 64 parameters");
                    out.param_cells.insert((f.id, name.to_owned()), i);
                }
            }
        }
        // The child scopes a function runs come after it.
        for f in ir.functions.iter().rev() {
            let mut names = own_reads.remove(&f.id).unwrap_or_default();
            for c in runs.get(&f.id).into_iter().flatten() {
                let visible = out.visible.get(c);
                for name in out.reads.get(c).into_iter().flatten() {
                    let bound_here = visible
                        .and_then(|v| v.get(name))
                        .is_some_and(|(owner, _)| *owner == f.id);
                    if !bound_here {
                        names.insert(name.clone());
                    }
                }
            }
            if !names.is_empty() {
                out.reads.insert(f.id, names.into_iter().collect());
            }
        }
        out
    }

    /// The cells the inference of `f` reads, by the name `f` reads each with.
    fn read_by(&self, f: FunctionId) -> impl Iterator<Item = (&String, &CellKey)> {
        let visible = self.visible.get(&f);
        self.reads
            .get(&f)
            .into_iter()
            .flatten()
            .filter_map(move |name| Some((name, visible?.get(name)?)))
    }

    /// `cell`, when its values are those the function binding it binds.
    fn bound_only<'c>(&self, cell: &'c CellKey) -> Option<&'c CellKey> {
        (!self.rebound.contains(cell)).then_some(cell)
    }

    /// The function enclosing `f` that binds the variable `name` `f` reads from it, and its
    /// parameter the variable is on every path: the function binds the name only as that
    /// parameter, and no nested scope rebinds it.
    fn captured_param<'m>(
        &self,
        ir: &'m ModuleIr,
        f: &IrFunction,
        name: &str,
    ) -> Option<(&'m IrFunction, u8)> {
        let cell = self.bound_only(self.visible.get(&f.id)?.get(name)?)?;
        Some((ir.function(cell.0), *self.param_cells.get(cell)?))
    }
}

/// The parameters of enclosing functions that the nested scopes of the module read through
/// closure cells ([`resolver::CapturedParams`]), by the scope's qualname. A parameter of a function
/// that one scope nested in it reads is a parameter of every scope nested in it, so a scope can
/// hand it to the scope it calls (a generator expression calling a nested function reading
/// `self`). The scopes sharing a qualname (sibling lambdas, conditional definitions) share one
/// summary, so they share the positions too.
pub(crate) fn captured_params(ir: &ModuleIr) -> FxHashMap<&str, resolver::CapturedParams> {
    let nesting = Nesting::of(ir);
    if nesting.visible.is_empty() {
        return FxHashMap::default();
    }
    // The parameters of each function that the scopes nested in it read, by its qualname.
    let mut read: FxHashMap<&str, BTreeSet<u8>> = FxHashMap::default();
    for f in ir
        .functions
        .iter()
        .filter(|f| nesting.visible.contains_key(&f.id))
    {
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            if let InstKind::LoadDeref { name, .. } = &inst.kind
                && let Some((owner, i)) = nesting.captured_param(ir, f, name)
            {
                read.entry(resolver::func_qualname(owner))
                    .or_default()
                    .insert(i);
            }
        }
    }
    if read.is_empty() {
        return FxHashMap::default();
    }
    // The parameters of enclosing functions each scope has, and the most parameters of its own
    // that a scope of its qualname takes.
    let mut captured: FxHashMap<&str, BTreeSet<(&str, u8)>> = FxHashMap::default();
    let mut own: FxHashMap<&str, usize> = FxHashMap::default();
    for f in &ir.functions {
        let qualname = f.qualname.as_str();
        let enclosing = qualname
            .match_indices(".<locals>.")
            .filter_map(|(at, _)| read.get_key_value(&qualname[..at]));
        let mut params = enclosing
            .flat_map(|(owner, read)| read.iter().map(|i| (*owner, *i)))
            .peekable();
        if params.peek().is_none() {
            continue;
        }
        captured.entry(qualname).or_default().extend(params);
        let most = own.entry(qualname).or_default();
        *most = (*most).max(f.params.len());
    }
    captured
        .into_iter()
        .filter_map(|(qualname, params)| {
            let first = u8::try_from(own[qualname]).ok().filter(|f| *f < 64)?;
            let params = params
                .into_iter()
                .take(64 - usize::from(first))
                .map(|(owner, i)| (owner.to_owned(), i))
                .collect();
            Some((qualname, resolver::CapturedParams { first, params }))
        })
        .collect()
}

/// The cells the scopes `parent` creates outside its inlined comprehensions can read: those
/// `parent` reads, then its own (the locals of a function they capture).
fn nested_in(parent: &IrFunction, inherited: &Rc<Cells>) -> Rc<Cells> {
    let mut own = parent
        .captured_vars
        .iter()
        .filter(|n| heki_ir::comprehension_target_source(n).is_none())
        .peekable();
    if own.peek().is_none() {
        return Rc::clone(inherited);
    }
    let mut cells = (**inherited).clone();
    for name in own {
        cells.insert(name.clone(), (parent.id, name.clone()));
    }
    Rc::new(cells)
}

/// `shared` with the iteration variables of `parent`'s inlined comprehensions that a scope
/// created in them reads (`comprehension_cells`, by their cell names) in place of the names
/// they shadow.
fn with_comprehension_cells(
    shared: &Rc<Cells>,
    parent: FunctionId,
    comprehension_cells: &[&str],
) -> Rc<Cells> {
    if comprehension_cells.is_empty() {
        return Rc::clone(shared);
    }
    let mut cells = (**shared).clone();
    for cell in comprehension_cells {
        if let Some(source) = heki_ir::comprehension_target_source(cell) {
            cells.insert(source.to_owned(), (parent, (*cell).to_owned()));
        }
    }
    Rc::new(cells)
}

/// The function binding a closure's variable (fully qualified), and the class of the instance it
/// binds to it, when that is one class ([`TypeCtx::captured`]).
type Captured = (String, Option<String>);

/// The variables binding each closure cell of `f`, by the cell's name. A cell holds every value
/// the function binds to it: its parameter's, and those of the instructions naming it (an
/// unassigned read, `LoadName`, binds none). A read of the cell in a nested scope is a phi of
/// them all.
pub(crate) fn cell_bindings(f: &IrFunction) -> FxHashMap<&str, Vec<VarId>> {
    let mut cell_vars: FxHashMap<&str, Vec<VarId>> = f
        .captured_vars
        .iter()
        .map(|name| (name.as_str(), Vec::new()))
        .collect();
    if cell_vars.is_empty() {
        return cell_vars;
    }
    for p in &f.params {
        if let Some(vars) = cell_vars.get_mut(p.name.as_str()) {
            vars.push(p.var);
        }
    }
    for inst in f.blocks.iter().flat_map(|b| &b.insts) {
        if let Some(d) = inst.kind.dst()
            && !matches!(inst.kind, InstKind::LoadName { .. })
            && let Some(vars) = f.var_name(d).and_then(|n| cell_vars.get_mut(n))
        {
            vars.push(d);
        }
    }
    cell_vars
}

/// The function id of a non-inlined comprehension call (Python 3.11).
/// Whether `x op= y` on a display of `kind` changes it in place.
fn display_changes_in_place(kind: BuiltinKind, op: BinOpKind) -> bool {
    crate::types::changes_in_place(&format!("builtins.{}", kind.name()), op)
}

fn comprehension_id(callee: &str) -> FunctionId {
    FunctionId(
        callee
            .strip_prefix("__comp_")
            .and_then(|n| n.parse().ok())
            .expect("the builder names non-inlined comprehension calls `__comp_<id>`"),
    )
}

/// What a nested scope receives from the functions enclosing it.
struct Seed {
    /// The type of the value a comprehension or generator expression with a scope of its own
    /// iterates (`None` inside when unknown).
    iterated: Option<Option<Ty>>,
    /// The facts of the closure cells its inference reads, by the name it reads each with: every
    /// value the function binding a cell binds to it, a cell read being a phi of them all.
    cells: FxHashMap<String, Fact>,
}

impl TypeCtx<'_> {
    /// The types of `f`'s variables.
    pub(crate) fn infer_types(&self, f: &IrFunction) -> Rc<Inferred> {
        if let Some(done) = self.scopes.enclosing.borrow().get(&f.id) {
            return Rc::clone(done);
        }
        let seed = self.seed_of(f);
        let shape = FnShape::new(f);
        let inferred = Rc::new(Engine::new(self, &shape, seed).run());
        if inferred.nested_read {
            self.scopes
                .enclosing
                .borrow_mut()
                .insert(f.id, Rc::clone(&inferred));
        }
        inferred
    }

    fn nesting(&self) -> &Nesting {
        self.scopes.nesting.get_or_init(|| Nesting::of(self.ir()))
    }

    /// What the functions enclosing `f` hand it: the type of the value a child scope iterates,
    /// and the facts of the cells a nested scope reads, from their final types.
    fn seed_of(&self, f: &IrFunction) -> Seed {
        let iterated = f.passed_from.map(|(creator, var)| {
            self.infer_types(self.ir().function(creator))
                .ty(var)
                .cloned()
        });
        let nesting = self.nesting();
        let cells = nesting
            .read_by(f.id)
            .map(|(name, captured)| {
                let fact = nesting
                    .bound_only(captured)
                    .map_or(Fact::Unknown, |(owner, bound)| {
                        self.infer_types(self.ir().function(*owner))
                            .cell_fact(bound)
                    });
                (name.clone(), fact)
            })
            .collect();
        Seed { iterated, cells }
    }

    /// The cell the variable `name` that scope `f` reads from an enclosing function is, when the
    /// values that function binds to it are all it holds: no nested scope rebinds it
    /// (`nonlocal`).
    pub(crate) fn bound_cell(&self, f: &IrFunction, name: &str) -> Option<&CellKey> {
        let nesting = self.nesting();
        nesting.bound_only(nesting.visible.get(&f.id)?.get(name)?)
    }

    /// The function enclosing scope `f` whose parameter the variable `name` `f` reads from it is
    /// on every path, and that parameter.
    pub(crate) fn captured_param(&self, f: &IrFunction, name: &str) -> Option<(&IrFunction, u8)> {
        self.nesting().captured_param(self.ir(), f, name)
    }

    /// What the variable `name` that scope `f` reads from an enclosing function is: the
    /// function binding it (fully qualified), and the class of the instance every value that
    /// function binds to it is, when that is one class. `None` when no function enclosing `f`
    /// binds it. The values nested scopes store through `nonlocal` are not among them
    /// ([`Nesting::rebound`]): a caller that needs them reads the rebindings itself.
    pub fn captured(&self, f: &IrFunction, name: &str) -> Option<Captured> {
        let (owner, cell) = self.nesting().visible.get(&f.id)?.get(name)?;
        let g = self.ir().function(*owner);
        let class = match self.infer_types(g).cell_fact(cell) {
            Fact::Known(t) => match t.val() {
                Some(Val::Inst(c)) => Some(c),
                _ => None,
            },
            Fact::Unset | Fact::Unknown => None,
        };
        Some((
            format!("{}.{}", self.module_name(), resolver::func_qualname(g)),
            class,
        ))
    }

    /// What child scope `id` produces when it iterates a value of type `seed` and reads cells
    /// of facts `cells`: the items a generator expression yields, or the container a
    /// comprehension returns.
    fn child_output(&self, id: FunctionId, seed: Option<Ty>, cells: CellFacts) -> Elem {
        let key = (id, seed, cells);
        if let Some(done) = self.scopes.outputs.borrow().get(&key) {
            return done.clone();
        }
        let child = self.ir().function(id);
        let seed = Seed {
            iterated: Some(key.1.clone()),
            cells: key.2.iter().cloned().collect(),
        };
        let shape = FnShape::new(child);
        let inferred = Engine::new(self, &shape, seed).run();
        let produced = |v: VarId| inferred.facts[v.0 as usize].elem();
        let out = if matches!(
            child.kind,
            FunctionKind::Generator | FunctionKind::AsyncGenerator
        ) {
            let yields = child.blocks.iter().flat_map(|b| &b.insts);
            yields
                .filter_map(|i| match i.kind {
                    InstKind::Yield { value: Some(v), .. } => Some(produced(v)),
                    _ => None,
                })
                .fold(Elem::Empty, |acc, e| acc.join(&e))
        } else {
            let returns = child.blocks.iter().filter_map(|b| match b.terminator {
                Terminator::Return(Some(v)) => Some(produced(v)),
                _ => None,
            });
            returns.fold(Elem::Empty, |acc, e| acc.join(&e))
        };
        self.scopes.outputs.borrow_mut().insert(key, out.clone());
        out
    }
}

/// A transfer cannot run yet: an operand it needs is unset.
struct Pending;

type Step = Result<Option<Ty>, Pending>;

/// The fact a transfer's operand has.
fn to_fact(operand: Result<Option<&Ty>, Pending>) -> Fact {
    match operand {
        Ok(Some(t)) => Fact::Known(t.clone()),
        Ok(None) => Fact::Unknown,
        Err(Pending) => Fact::Unset,
    }
}

/// The type of an item, or `Pending` when no item can be there.
fn item_ty(e: Elem) -> Step {
    match e {
        Elem::Empty => Err(Pending),
        Elem::Of(t) => Ok(Some(t)),
        Elem::Unknown => Ok(None),
    }
}

/// The narrowed types both maps give (joined).
fn intersect(a: FxHashMap<VarId, Ty>, b: &FxHashMap<VarId, Ty>) -> FxHashMap<VarId, Ty> {
    a.into_iter()
        .filter_map(|(v, t)| Some((v, t.join(b.get(&v)?)?)))
        .collect()
}

/// What awaiting a value of type `t` gives: a coroutine's declared result, nothing tracked for
/// a future or a task (the result it holds), and otherwise the value's own type, since the
/// catalog types an external awaitable by what awaiting it gives
/// (`psycopg.AsyncConnection.connect`).
fn awaited(t: &Ty) -> Option<Ty> {
    match t {
        Ty::Coroutine(result) => Some((**result).clone()),
        Ty::Inst(o) if heki_kb::FUTURE_CLASSES.contains(&o.class.as_str()) => None,
        other => Some(other.clone()),
    }
}

/// The join of every type, `None` when one is unknown or none covers them all.
fn join_all(types: impl IntoIterator<Item = Option<Ty>>) -> Option<Ty> {
    let mut out: Option<Ty> = None;
    for t in types {
        let t = t?;
        out = Some(match out {
            None => t,
            Some(acc) => acc.join(&t)?,
        });
    }
    out
}

/// `tuple[a, b]`.
fn pair(a: Elem, b: Elem) -> Elem {
    Elem::Of(Ty::container("builtins.tuple", Args::Tuple(vec![a, b])))
}

/// The types `isinstance` checks give variables.
type Narrowed = FxHashMap<VarId, Ty>;

/// The passes the inference takes at most; the corpora need seven at most.
const MAX_PASSES: usize = 24;

struct Engine<'e, 'a> {
    cx: &'e TypeCtx<'a>,
    f: &'e IrFunction,
    shape: &'e FnShape<'e>,
    owner: Option<String>,
    /// The instance `Self` stands for in a method's annotations.
    self_ty: Option<Ty>,
    /// Module and class bodies read unassigned names from the globals and builtins.
    reads_globals: bool,
    facts: Vec<Fact>,
    /// The declared types of annotated local names, and of the variables named like them.
    names: FxHashMap<String, Ty>,
    declared: FxHashMap<VarId, Ty>,
    /// Integer constants, with the span of their literal.
    ints: FxHashMap<VarId, (i64, Span)>,
    /// String constants (the annotations `typing.cast` takes as text).
    strs: FxHashMap<VarId, String>,
    /// How many targets the unpacking assignments of each variable have (`a, b = pair`).
    unpacked: FxHashMap<VarId, usize>,
    /// Container displays built in the function (`[]`, `{}`, `set()` is a call), by kind.
    displays: FxHashMap<VarId, BuiltinKind>,
    /// The items (or keys) and values added to each display after it is built.
    added: FxHashMap<VarId, (Elem, Elem)>,
    /// The facts of the closure cells of enclosing functions the function can read, by name.
    cells: FxHashMap<String, Fact>,
    /// The closure cells the function binds, by name.
    own_cells: FxHashMap<String, OwnCell>,
    /// The position of each variable binding one of those cells (the cell it is named after)
    /// among the cell's variables.
    bound_at: FxHashMap<VarId, usize>,
    nested_read: bool,
    changed: bool,
    /// A pass added items to a display.
    added_changed: bool,
    /// A pass handed a child scope one of the function's cells before every binding of it was
    /// computed.
    cells_deferred: Cell<bool>,
    /// `isinstance` narrowing (only for functions that call a function named `isinstance`).
    narrowing: Option<Narrowing<'e>>,
    /// What `isinstance` checks guarding the current block give its variables.
    cur: FxHashMap<VarId, Ty>,
    /// [`Self::implied`] of each condition, truth, block of the narrowed types it starts from,
    /// and depth, while one block is entered: a condition reaches the same phi through many
    /// paths (a loop header's phi is an input of itself on each `continue`).
    implied_memo: RefCell<FxHashMap<(VarId, bool, BlockId, u8), Narrowed>>,
}

/// A closure cell the function binds.
struct OwnCell {
    /// The variables the function binds it to: its parameters and the instructions naming it.
    vars: Vec<VarId>,
    /// Their facts as bound, not as the `isinstance` checks of a block narrow them.
    bound: BoundFacts,
}

/// Facts in a fixed order, joined pairwise up a balanced tree: a new fact for one of them updates
/// the join in a step per level. A cell bound many times and read by a child scope created
/// after each binding is then read without folding every binding again.
struct BoundFacts {
    /// The facts from `width` on, padded with unset ones; below `width`, the join of each node's
    /// two children, the root at 1.
    nodes: Vec<Fact>,
    width: usize,
    /// How many of the facts are unset.
    unset: usize,
}

impl BoundFacts {
    fn new(facts: Vec<Fact>) -> Self {
        let width = facts.len().next_power_of_two();
        let unset = facts.iter().filter(|f| **f == Fact::Unset).count();
        let mut nodes = vec![Fact::Unset; width];
        nodes.extend(facts);
        nodes.resize(2 * width, Fact::Unset);
        for node in (1..width).rev() {
            nodes[node] = nodes[2 * node].join(&nodes[2 * node + 1]);
        }
        Self {
            nodes,
            width,
            unset,
        }
    }

    /// Replaces fact `i` with `fact`, which is set: a computed value never becomes unset again.
    fn set(&mut self, i: usize, fact: Fact) {
        let mut node = self.width + i;
        if self.nodes[node] == Fact::Unset {
            self.unset -= 1;
        }
        self.nodes[node] = fact;
        while node > 1 {
            node /= 2;
            self.nodes[node] = self.nodes[2 * node].join(&self.nodes[2 * node + 1]);
        }
    }

    fn joined(&self) -> &Fact {
        &self.nodes[1]
    }
}

/// The CFG facts `isinstance` narrowing needs, and the narrowed types at each block's entry
/// (`None` until a pass reaches the block).
struct Narrowing<'e> {
    defs: FxHashMap<VarId, &'e InstKind>,
    /// Each phi with its block.
    phis: FxHashMap<VarId, (BlockId, &'e PhiNode)>,
    entry: Vec<Option<FxHashMap<VarId, Ty>>>,
}

impl<'e, 'a> Engine<'e, 'a> {
    fn new(cx: &'e TypeCtx<'a>, shape: &'e FnShape<'e>, seed: Seed) -> Self {
        let f = shape.f;
        let mut engine = Self {
            cx,
            f,
            shape,
            owner: cx.owner_fqn(f),
            self_ty: cx.owner_fqn(f).map(Ty::inst),
            reads_globals: f.is_module_body || f.is_class_body,
            facts: vec![Fact::Unset; f.var_names.len()],
            names: FxHashMap::default(),
            declared: FxHashMap::default(),
            ints: FxHashMap::default(),
            strs: FxHashMap::default(),
            unpacked: FxHashMap::default(),
            displays: FxHashMap::default(),
            added: FxHashMap::default(),
            cells: seed.cells,
            own_cells: FxHashMap::default(),
            bound_at: FxHashMap::default(),
            nested_read: false,
            changed: false,
            added_changed: false,
            cells_deferred: Cell::new(false),
            narrowing: None,
            cur: FxHashMap::default(),
            implied_memo: RefCell::default(),
        };
        engine.seed_params();
        if let Some(seed) = seed.iterated {
            // The iterated value a child scope receives.
            let outer = (0u32..)
                .zip(&f.var_names)
                .find(|(_, n)| n.as_deref() == Some(".0"))
                .map(|(i, _)| VarId(i))
                .expect("child scopes read their iterated value from `.0`");
            engine.facts[outer.0 as usize] = seed.map_or(Fact::Unknown, Fact::Known);
        }
        engine.collect();
        engine
    }

    fn seed_params(&mut self) {
        let (cx, f) = (self.cx, self.f);
        let is_static = f.has_decorator(&["staticmethod"]);
        let is_classmethod = f.has_decorator(&["classmethod"]);
        for (i, p) in f.params.iter().enumerate() {
            let fact = match &self.owner {
                Some(owner) if i == 0 && !is_static => Fact::Known(if is_classmethod {
                    Ty::Class(owner.clone())
                } else {
                    Ty::inst(owner.as_str())
                }),
                _ => match p
                    .annotation
                    .as_deref()
                    .and_then(|a| cx.module.resolve_annotation_ty(a))
                    .and_then(|t| cx.project.declared_ty(&t))
                    .and_then(|t| t.bind_self(self.self_ty.as_ref()))
                {
                    // `*args: T` is a tuple of `T`, `**kwargs: T` a dict of them.
                    Some(t) => Fact::Known(match t {
                        t if p.is_vararg => {
                            Ty::container("builtins.tuple", Args::items(Elem::Of(t)))
                        }
                        t if p.is_kwarg => Ty::container(
                            "builtins.dict",
                            Args::Map(
                                Box::new(Elem::Of(Ty::inst("builtins.str"))),
                                Box::new(Elem::Of(t)),
                            ),
                        ),
                        t => t,
                    }),
                    None => Fact::Unknown,
                },
            };
            self.facts[p.var.0 as usize] = fact;
        }
    }

    /// The facts the transfers look up: declared locals, constants, unpacking targets,
    /// copies, and displays.
    fn collect(&mut self) {
        let (cx, f) = (self.cx, self.f);
        let insts = || f.blocks.iter().flat_map(|b| &b.insts);
        if insts().any(|i| {
            matches!(&i.kind, InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. }
                if name == "isinstance")
        }) {
            let mut defs = FxHashMap::default();
            for i in insts() {
                if let Some(d) = i.kind.dst() {
                    defs.insert(d, &i.kind);
                }
            }
            let phis = f
                .blocks
                .iter()
                .flat_map(|b| b.phis.iter().map(move |p| (p.dst, (b.id, p))))
                .collect();
            self.narrowing = Some(Narrowing {
                defs,
                phis,
                entry: vec![None; f.blocks.len()],
            });
        }
        for ann in f
            .local_annotations
            .iter()
            .filter(|a| !a.target.contains('.'))
        {
            if let Some(t) = cx
                .module
                .resolve_annotation_ty(&ann.annotation)
                .and_then(|t| cx.project.declared_ty(&t))
                .and_then(|t| t.bind_self(self.self_ty.as_ref()))
            {
                self.names.insert(ann.target.clone(), t);
            }
        }
        if !self.names.is_empty() {
            for (i, name) in (0u32..).zip(&f.var_names) {
                let var = VarId(i);
                if let Some(t) = name.as_deref().and_then(|n| self.names.get(n))
                    && !f.params.iter().any(|p| p.var == var)
                {
                    self.declared.insert(var, t.clone());
                }
            }
        }
        let cell_vars = cell_bindings(f);
        if !cell_vars.is_empty() {
            self.nested_read = true;
        }
        for (name, vars) in cell_vars {
            self.bound_at
                .extend(vars.iter().enumerate().map(|(i, v)| (*v, i)));
            let facts = vars.iter().map(|v| self.bound_fact(*v)).collect();
            let bound = BoundFacts::new(facts);
            self.own_cells
                .insert(name.to_owned(), OwnCell { vars, bound });
        }
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            match &inst.kind {
                InstKind::Const {
                    dst,
                    literal: ConstLiteral::Int(n),
                } => {
                    self.ints.insert(*dst, (*n, inst.span));
                }
                InstKind::Const {
                    dst,
                    literal: ConstLiteral::Str(text),
                } => {
                    self.strs.insert(*dst, text.clone());
                }
                InstKind::AllocBuiltin { dst, kind, .. } => {
                    self.displays.insert(*dst, *kind);
                }
                InstKind::DictLiteral { dst, .. } => {
                    self.displays.insert(*dst, BuiltinKind::Dict);
                }
                InstKind::MakeGenExpr { .. }
                | InstKind::Call {
                    callee_var: None, ..
                } => self.nested_read = true,
                _ => {}
            }
        }
        // An unpacking target loads its position with a constant spanning the target itself.
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            if let InstKind::SubscriptLoad { obj, index, .. } = &inst.kind
                && let Some(&(n, at)) = self.ints.get(index)
                && at == inst.span
            {
                let len = usize::try_from(n).map_or(0, |n| n + 1);
                let slot = self.unpacked.entry(*obj).or_default();
                *slot = (*slot).max(len);
            }
        }
    }

    fn run(mut self) -> Inferred {
        let shape = self.shape;
        // Without loops every operand is computed before its users in reverse postorder: one
        // pass is final unless it added items to a display already computed, or handed a child
        // scope a cell bound later in the function.
        let acyclic = self.f.loops.is_empty();
        for pass in 1.. {
            if pass > MAX_PASSES {
                // A type a loop passes back through a chain of assignments takes a pass per
                // link: past the bound the types found so far are not final, so none is kept.
                self.facts.fill(Fact::Unknown);
                self.narrowing = None;
                break;
            }
            self.changed = false;
            self.added_changed = false;
            self.cells_deferred.set(false);
            for &bb in shape.rpo() {
                let block = self.f.block(bb);
                self.enter(bb);
                for phi in &block.phis {
                    // A phi joins the inputs that have a value: one that never gets a value (an
                    // unassigned local, `None`) adds none, and a loop's back edge adds its value
                    // on the next pass.
                    let fact = phi
                        .incoming
                        .iter()
                        .filter(|(_, v)| *v != phi.dst)
                        .fold(Fact::Unset, |acc, (_, v)| acc.join(&self.fact(*v)));
                    self.update(phi.dst, fact);
                }
                for inst in &block.insts {
                    self.add_contents(inst);
                    if let Some(dst) = inst.kind.dst() {
                        let fact = match self.step(inst) {
                            Ok(Some(t)) => Fact::Known(t),
                            Ok(None) => Fact::Unknown,
                            Err(Pending) => Fact::Unset,
                        };
                        self.update(dst, fact);
                    }
                }
            }
            if !self.changed || acyclic && !self.added_changed && !self.cells_deferred.get() {
                break;
            }
        }
        self.cur.clear();
        // The facts as transfers see them: a declared local has its declaration.
        let declared: Vec<(VarId, Fact)> =
            self.declared.keys().map(|v| (*v, self.fact(*v))).collect();
        let mut facts = std::mem::take(&mut self.facts);
        for (v, fact) in declared {
            facts[v.0 as usize] = fact;
        }
        let cells = self
            .own_cells
            .iter()
            .map(|(name, cell)| {
                let fact = cell
                    .vars
                    .iter()
                    .fold(Fact::Unset, |acc, v| acc.join(&facts[v.0 as usize]));
                (name.clone(), fact)
            })
            .collect();
        let mut narrowed = FxHashMap::default();
        if let Some(n) = &self.narrowing {
            for (b, entry) in (0u32..).zip(&n.entry) {
                for (v, t) in entry.iter().flatten() {
                    if facts[v.0 as usize] != Fact::Known(t.clone()) {
                        narrowed.insert((BlockId(b), *v), t.clone());
                    }
                }
            }
        }
        Inferred {
            facts,
            declared: self.names,
            nested_read: self.nested_read,
            cells,
            narrowed,
        }
    }

    /// The fact of `var` (see [`Self::need`]).
    fn fact(&self, var: VarId) -> Fact {
        to_fact(self.need(var))
    }

    /// The type of `var` for a transfer that needs it. A declared local has its declaration, as
    /// for a type checker, unless what it holds is narrower: an instance of a subclass
    /// (`x: Base = Sub()`), or anything for a declaration of `Any` / `object`.
    fn need(&self, var: VarId) -> Result<Option<&Ty>, Pending> {
        if let Some(t) = self.cur.get(&var) {
            return Ok(Some(t));
        }
        self.unnarrowed(var)
    }

    /// The facts of the cells the inference of child scope `id` reads, as the function hands
    /// them to it now: its own from the values bound to them so far, the others as it received
    /// them.
    fn child_cells(&self, id: FunctionId) -> CellFacts {
        let nesting = self.cx.nesting();
        nesting
            .read_by(id)
            .map(|(name, captured)| {
                let fact = match nesting.bound_only(captured) {
                    Some((owner, bound)) if *owner == self.f.id => self.own_cell(bound),
                    Some(_) => self.cells[name].clone(),
                    None => Fact::Unknown,
                };
                (name.clone(), fact)
            })
            .collect()
    }

    /// The fact of the cell `name` the function binds, from the values bound to it so far: a
    /// binding not computed yet adds none, and asks for another pass.
    fn own_cell(&self, name: &str) -> Fact {
        let bound = &self.own_cells[name].bound;
        if bound.unset > 0 {
            self.cells_deferred.set(true);
        }
        bound.joined().clone()
    }

    /// The fact of `var` as bound to the cell it is named after, not as the `isinstance` checks
    /// of the current block narrow it.
    fn bound_fact(&self, var: VarId) -> Fact {
        to_fact(self.unnarrowed(var))
    }

    /// Hands the new fact of `var` to the cell it binds, if it binds one.
    fn rebind(&mut self, var: VarId) {
        let Some(&i) = self.bound_at.get(&var) else {
            return;
        };
        let fact = self.bound_fact(var);
        let cell = self.f.var_name(var).and_then(|n| self.own_cells.get_mut(n));
        cell.expect("a variable binds the cell it is named after")
            .bound
            .set(i, fact);
    }

    /// What `type(value)` returns for a value of type `t`: the class object of the instance's
    /// class ([`ProjectDecls::class_object`]); unknown for a union. (`value.__class__` is not
    /// this: a class or a subclass may rebind it, as proxies do.)
    fn class_of(&self, t: &Ty) -> Option<Ty> {
        self.cx.project.class_object(&t.obj()?.class)
    }

    /// [`Self::need`] without the `isinstance` checks guarding the current block.
    fn unnarrowed(&self, var: VarId) -> Result<Option<&Ty>, Pending> {
        let inferred = &self.facts[var.0 as usize];
        match (inferred, self.declared.get(&var)) {
            (Fact::Known(t), Some(d)) if !self.cx.project.narrower(t, d) => Ok(Some(d)),
            (Fact::Known(t), _) => Ok(Some(t)),
            (_, Some(d)) => Ok(Some(d)),
            (Fact::Unset, None) => Err(Pending),
            (Fact::Unknown, None) => Ok(None),
        }
    }

    /// Start block `bb`: the narrowed types at its entry are those every reached predecessor
    /// hands it (a predecessor's own, and what its branch condition implies on this edge).
    fn enter(&mut self, bb: BlockId) {
        // The predecessors' narrowed types come from their entries, not the last block's.
        self.cur.clear();
        self.implied_memo.get_mut().clear();
        let Some(n) = &self.narrowing else {
            return;
        };
        let mut entry: Option<FxHashMap<VarId, Ty>> = None;
        for &p in self.shape.preds(bb) {
            let Some(at) = &n.entry[p.0 as usize] else {
                continue;
            };
            let mut edge = at.clone();
            if let Terminator::Branch { cond, then_bb, .. } = &self.f.block(p).terminator {
                edge.extend(self.implied(n, *cond, *then_bb == bb, p, 0));
            }
            entry = Some(match entry {
                None => edge,
                Some(acc) => intersect(acc, &edge),
            });
        }
        let entry = entry.unwrap_or_default();
        self.cur.clone_from(&entry);
        if let Some(n) = &mut self.narrowing {
            n.entry[bb.0 as usize] = Some(entry);
        }
    }

    /// The narrowed types that `cond` being `truth` implies, where the narrowed types at the
    /// entry of `from` hold: `isinstance(x, C)` (with `not`, and through the values `and` /
    /// `or` join).
    fn implied(
        &self,
        n: &Narrowing<'_>,
        cond: VarId,
        truth: bool,
        from: BlockId,
        depth: u8,
    ) -> FxHashMap<VarId, Ty> {
        let key = (cond, truth, from, depth);
        if let Some(known) = self.implied_memo.borrow().get(&key) {
            return known.clone();
        }
        let out = self.implied_once(n, cond, truth, from, depth);
        self.implied_memo.borrow_mut().insert(key, out.clone());
        out
    }

    fn implied_once(
        &self,
        n: &Narrowing<'_>,
        cond: VarId,
        truth: bool,
        from: BlockId,
        depth: u8,
    ) -> FxHashMap<VarId, Ty> {
        let mut out = FxHashMap::default();
        if depth > 8 {
            return out;
        }
        if let Some(&(block, phi)) = n.phis.get(&cond) {
            // `a and b` / `a or b`: the inputs that can be `truth` here, each with the narrowed
            // types of its path.
            let mut joined: Option<FxHashMap<VarId, Ty>> = None;
            for (p, v) in &phi.incoming {
                let Some(path) = &n.entry[p.0 as usize] else {
                    return out;
                };
                if let Terminator::Branch {
                    cond: c,
                    then_bb,
                    else_bb,
                } = &self.f.block(*p).terminator
                    && c == v
                {
                    let target = if truth { else_bb } else { then_bb };
                    if *target == block {
                        continue;
                    }
                }
                let mut facts = path.clone();
                facts.extend(self.implied(n, *v, truth, *p, depth + 1));
                joined = Some(match joined {
                    None => facts,
                    Some(acc) => intersect(acc, &facts),
                });
            }
            return joined.unwrap_or_default();
        }
        match n.defs.get(&cond) {
            Some(InstKind::UnaryOp {
                op: UnaryOpKind::Not,
                operand,
                ..
            }) => return self.implied(n, *operand, !truth, from, depth + 1),
            Some(InstKind::Copy { src, .. }) => {
                return self.implied(n, *src, truth, from, depth + 1);
            }
            Some(InstKind::Call {
                callee_var: Some(c),
                args,
                ..
            }) if args.len() >= 2
                && matches!(self.need(*c), Ok(Some(Ty::Ref(f))) if f == "builtins.isinstance") =>
            {
                let x = args[0];
                let current = n.entry[from.0 as usize]
                    .as_ref()
                    .and_then(|at| at.get(&x))
                    .or_else(|| self.need(x).ok().flatten());
                if let Some(classes) = self
                    .need(args[1])
                    .ok()
                    .flatten()
                    .and_then(|t| isinstance_classes(self.cx.project, t))
                    && let Some(t) = self.narrow(current, &classes, truth)
                {
                    out.insert(x, t);
                }
            }
            _ => {}
        }
        out
    }

    /// The type of a value of type `t` once `isinstance(value, classes)` is `truth`: true keeps
    /// the instances of those classes among what it may be (or is an instance of them); false
    /// drops them from a union.
    fn narrow(&self, t: Option<&Ty>, classes: &[String], truth: bool) -> Option<Ty> {
        let project = self.cx.project;
        let is_a = |c: &str| classes.iter().any(|k| c == k || project.is_subclass(c, k));
        let members = t.map_or(&[][..], Ty::members);
        if truth {
            let kept: Vec<Obj> = members.iter().filter(|o| is_a(&o.class)).cloned().collect();
            if kept.is_empty() {
                Ty::of_members(classes.iter().map(|c| Obj {
                    class: c.clone(),
                    args: Args::None,
                }))
            } else {
                Ty::of_members(kept)
            }
        } else {
            let kept: Vec<Obj> = members
                .iter()
                .filter(|o| !is_a(&o.class))
                .cloned()
                .collect();
            (!kept.is_empty() && kept.len() < members.len())
                .then(|| Ty::of_members(kept))
                .flatten()
        }
    }

    fn update(&mut self, var: VarId, fact: Fact) {
        let slot = &mut self.facts[var.0 as usize];
        if *slot == fact {
            return;
        }
        let joined = slot.join(&fact);
        if joined != *slot {
            *slot = joined;
            self.changed = true;
            self.rebind(var);
        }
    }

    /// The items iterating a value of type `ty` yields: the arguments of a known container, or
    /// what the `__iter__` of a class returns.
    fn items(&self, ty: Option<&Ty>) -> Elem {
        let Some(o) = ty.and_then(Ty::obj) else {
            return Elem::Unknown;
        };
        if o.args != Args::None {
            return o.args.item();
        }
        self.dunder(o, "__iter__")
            .and_then(|t| t.obj().map(|r| r.args.item()))
            .unwrap_or(Elem::Unknown)
    }

    /// [`Self::items`] of a variable (an unset one has none yet).
    fn items_of(&self, var: VarId) -> Elem {
        self.need(var).map_or(Elem::Empty, |t| self.items(t))
    }

    /// What calling special method `name` on an instance returns. The syntax calling it (`x[k]`,
    /// `for v in x`) hands no argument facts to the catalog's argument cases.
    fn dunder(&self, o: &Obj, name: &str) -> Option<Ty> {
        let targets = self
            .cx
            .project
            .method_targets(&o.class, name, false, self.cx.kb);
        self.cx.callee_result(&targets, |_, _| None)
    }

    /// What a call passes at position `index` or by `keyword`, for the catalog's argument cases
    /// (`resolver::passed_arg`): the argument's constant and its class.
    fn passed(
        &self,
        call: Passed<'_>,
        index: Option<u8>,
        keyword: Option<&str>,
    ) -> Option<ArgFact> {
        let class = |v: VarId| {
            self.need(v)
                .ok()
                .flatten()
                .and_then(Ty::obj)
                .map(|o| o.class.clone())
        };
        resolver::passed_arg(
            call.args,
            call.kwargs,
            call.star_from,
            (index, keyword),
            |v| {
                let name_of = |n| match self.need(n).ok().flatten()? {
                    Ty::Ref(fqn) => Some(fqn.clone()),
                    _ => None,
                };
                resolver::arg_fact(
                    |d| self.shape.inst(d),
                    class,
                    name_of,
                    &|inst| self.cx.module_constant(inst, &name_of),
                    v,
                )
            },
        )
    }

    fn step(&self, inst: &Inst) -> Step {
        let cx = self.cx;
        Ok(match &inst.kind {
            InstKind::Const {
                literal: ConstLiteral::None,
                ..
            } => return Err(Pending),
            InstKind::AllocBuiltin { dst, kind, elems } => Some(self.display(*dst, *kind, elems)),
            InstKind::DictLiteral { dst, entries } => Some(self.dict_display(*dst, entries)),
            InstKind::Convert { dst, .. }
            | InstKind::Format { dst, .. }
            | InstKind::BuildString { dst, .. } => Some(self.display(*dst, BuiltinKind::Str, &[])),
            InstKind::LoadGlobal { name, .. } => Some(cx.global(name)),
            // An unassigned local of a function holds no value on this path.
            InstKind::LoadName { name, .. } if self.reads_globals => Some(cx.global(name)),
            InstKind::LoadName { .. } => return Err(Pending),
            InstKind::LoadDeref { name, .. } => {
                return self.cells.get(name).map_or(Ok(None), Fact::as_step);
            }
            InstKind::Import { .. } => cx.import_type(inst.span),
            InstKind::Copy { src, .. } => self.need(*src)?.cloned(),
            // An augmented assignment changing a built-in container in place gives the
            // container: the display it is, with what the function adds to it, or the type the
            // container has.
            InstKind::BinOp {
                op,
                lhs,
                in_place: true,
                ..
            } => match self.display_of(*lhs) {
                Some((root, kind)) if display_changes_in_place(kind, *op) => {
                    self.need(root)?.cloned()
                }
                Some(_) => None,
                None => match self.need(*lhs)? {
                    Some(t @ Ty::Inst(o)) if crate::types::changes_in_place(&o.class, *op) => {
                        Some(t.clone())
                    }
                    _ => None,
                },
            },
            InstKind::Await { value, .. } => self.need(*value)?.and_then(awaited),
            // `with m as c` binds `m.__enter__()` (`async with`: `await m.__aenter__()`): what
            // its definition declares it returns; without a declaration, the manager itself, as
            // clients, sessions, and files return.
            InstKind::EnterWith { mgr, is_async, .. } => {
                let manager = self.need(*mgr)?;
                let enter = if *is_async { "__aenter__" } else { "__enter__" };
                match manager.and_then(|m| self.member_call(m, enter, Passed::default())) {
                    Some(entered) if *is_async => awaited(&entered),
                    Some(entered) => Some(entered),
                    None => manager.cloned(),
                }
            }
            InstKind::AttrLoad { obj, attr, .. } => match self.need(*obj)? {
                Some(Ty::Ref(fqn) | Ty::Class(fqn)) => Some(cx.named(&format!("{fqn}.{attr}"))),
                Some(recv) => self.member(recv, attr),
                None => None,
            },
            InstKind::SubscriptLoad {
                obj,
                index,
                is_slice,
                ..
            } => return self.subscript(inst.span, *obj, *index, *is_slice),
            InstKind::MakeIter { iterable, .. } => match self.items(self.need(*iterable)?) {
                Elem::Unknown => None,
                item => Some(Ty::container("typing.Iterator", Args::items(item))),
            },
            InstKind::IterNext { iter, .. } => return item_ty(self.items(self.need(*iter)?)),
            InstKind::MakeGenExpr {
                func_id: Some(id),
                outer_iter,
                ..
            } => {
                let seed = self.need(*outer_iter)?.cloned();
                match cx.child_output(*id, seed, self.child_cells(*id)) {
                    Elem::Unknown => None,
                    item => Some(Ty::container("typing.Generator", Args::items(item))),
                }
            }
            InstKind::MakeClosure {
                func_id: Some(id), ..
            } => {
                let f = cx.ir().function(*id);
                Some(Ty::Ref(format!("{}.{}", cx.module_name(), f.qualname)))
            }
            InstKind::Call {
                callee_var: Some(c),
                args,
                kwargs,
                star_from,
                ..
            } => {
                let call = Passed {
                    args,
                    kwargs,
                    star_from: *star_from,
                };
                match self.need(*c)? {
                    Some(Ty::Ref(fqn)) if fqn == "builtins.super" && args.is_empty() => {
                        self.owner.clone().map(Ty::Super)
                    }
                    Some(Ty::Ref(fqn) | Ty::Class(fqn)) => return self.call_named(fqn, call),
                    Some(callee) => self.called(callee, call),
                    None => None,
                }
            }
            // A non-inlined comprehension (Python 3.11) returns the container it builds from the
            // value it iterates.
            InstKind::Call {
                callee_var: None,
                callee,
                args,
                ..
            } => {
                let seed = self.need(args[0])?.cloned();
                let id = comprehension_id(callee);
                return item_ty(cx.child_output(id, seed, self.child_cells(id)));
            }
            InstKind::MethodCall {
                recv,
                method,
                args,
                kwargs,
                star_from,
                ..
            } => {
                let call = Passed {
                    args,
                    kwargs,
                    star_from: *star_from,
                };
                return self.method_call(*recv, method, call);
            }
            _ => None,
        })
    }

    /// The type of the new object `dst` of builtin `kind`: a display or a comprehension of
    /// `elems` and of what the function adds to it, or the `str` of an f-string.
    fn display(&self, dst: VarId, kind: BuiltinKind, elems: &[VarId]) -> Ty {
        let class = format!("builtins.{}", kind.name());
        // A starred element (`*xs`, lowered to a copy of `xs`) adds its items.
        let starred = elems.iter().any(|v| self.f.is_unpacked(*v));
        let item = || {
            elems.iter().fold(Elem::Empty, |acc, v| {
                let e = if self.f.is_unpacked(*v) {
                    self.items_of(*v)
                } else {
                    self.fact(*v).elem()
                };
                acc.join(&e)
            })
        };
        let (added, values) = self
            .added
            .get(&dst)
            .cloned()
            .unwrap_or((Elem::Empty, Elem::Empty));
        match kind {
            BuiltinKind::List | BuiltinKind::Set => {
                Ty::container(&class, Args::items(item().join(&added)))
            }
            BuiltinKind::Tuple if !starred => Ty::container(
                &class,
                Args::Tuple(elems.iter().map(|v| self.fact(*v).elem()).collect()),
            ),
            BuiltinKind::Tuple => Ty::container(&class, Args::items(item())),
            BuiltinKind::Dict => Ty::container(&class, Args::map(added, values)),
            _ => Ty::inst(class),
        }
    }

    fn dict_display(&self, dst: VarId, entries: &[DictEntry]) -> Ty {
        let (mut keys, mut values) = self
            .added
            .get(&dst)
            .cloned()
            .unwrap_or((Elem::Empty, Elem::Empty));
        for e in entries {
            let (k, v) = match e {
                DictEntry::KeyValue { key, value } => {
                    (self.fact(*key).elem(), self.fact(*value).elem())
                }
                // `{**other}` adds the keys and values of a mapping.
                DictEntry::Unpack { dict } => self.mapping(*dict),
            };
            keys = keys.join(&k);
            values = values.join(&v);
        }
        Ty::container("builtins.dict", Args::map(keys, values))
    }

    /// The keys and values of the mapping in `var`.
    fn mapping(&self, var: VarId) -> (Elem, Elem) {
        match self.need(var) {
            Err(Pending) => (Elem::Empty, Elem::Empty),
            Ok(Some(Ty::Inst(Obj {
                args: Args::Map(k, v),
                ..
            }))) => ((**k).clone(), (**v).clone()),
            Ok(_) => (Elem::Unknown, Elem::Unknown),
        }
    }

    /// Items added to a display after it is built: `append` / `insert` / `extend` / `+=` of a
    /// list, `add` / `update` / `|=` / `^=` of a set, `xs[i] = v`, and `d[k] = v` /
    /// `setdefault` / `update` / `|=` of a dict (also through copies of the variable holding it,
    /// and through what an in-place operator gives).
    fn add_contents(&mut self, inst: &Inst) {
        let (target, item, value) = match &inst.kind {
            InstKind::MethodCall {
                recv, method, args, ..
            } => {
                let Some((root, kind)) = self.display_of(*recv) else {
                    return;
                };
                match (kind, method.as_str(), args.as_slice()) {
                    (BuiltinKind::List, "append", [v])
                    | (BuiltinKind::List, "insert", [_, v])
                    | (BuiltinKind::Set, "add", [v]) => (root, self.fact(*v).elem(), Elem::Empty),
                    (BuiltinKind::List, "extend", [xs]) | (BuiltinKind::Set, "update", [xs]) => {
                        (root, self.items_of(*xs), Elem::Empty)
                    }
                    (BuiltinKind::Dict, "setdefault", [k, v]) => {
                        (root, self.fact(*k).elem(), self.fact(*v).elem())
                    }
                    (BuiltinKind::Dict, "update", [m]) => {
                        let (k, v) = self.mapping(*m);
                        (root, k, v)
                    }
                    _ => return,
                }
            }
            InstKind::BinOp {
                op,
                lhs,
                rhs,
                in_place: true,
                ..
            } => match (self.display_of(*lhs), op) {
                (Some((root, BuiltinKind::List)), BinOpKind::Add)
                | (Some((root, BuiltinKind::Set)), BinOpKind::BitOr | BinOpKind::BitXor) => {
                    (root, self.items_of(*rhs), Elem::Empty)
                }
                (Some((root, BuiltinKind::Dict)), BinOpKind::BitOr) => {
                    let (k, v) = self.mapping(*rhs);
                    (root, k, v)
                }
                _ => return,
            },
            InstKind::SubscriptStore {
                obj, index, value, ..
            } => match self.display_of(*obj) {
                Some((root, BuiltinKind::Dict)) => {
                    (root, self.fact(*index).elem(), self.fact(*value).elem())
                }
                Some((root, BuiltinKind::List)) => (root, self.fact(*value).elem(), Elem::Empty),
                _ => return,
            },
            _ => return,
        };
        let slot = self
            .added
            .entry(target)
            .or_insert((Elem::Empty, Elem::Empty));
        let joined = (slot.0.join(&item), slot.1.join(&value));
        if joined != *slot {
            *slot = joined;
            self.changed = true;
            self.added_changed = true;
        }
    }

    /// The display a variable holds (through copies, and the in-place operators that give it
    /// back), with its kind.
    fn display_of(&self, var: VarId) -> Option<(VarId, BuiltinKind)> {
        let mut root = self.shape.copy_root(var);
        for _ in 0..16 {
            if let Some(kind) = self.displays.get(&root) {
                return Some((root, *kind));
            }
            match self.shape.inst(root).map(|i| &i.kind) {
                Some(InstKind::BinOp {
                    op,
                    lhs,
                    in_place: true,
                    ..
                }) => {
                    let lhs = self.shape.copy_root(*lhs);
                    match self.displays.get(&lhs) {
                        Some(kind) if !display_changes_in_place(*kind, *op) => return None,
                        _ => root = lhs,
                    }
                }
                _ => return None,
            }
        }
        None
    }

    fn subscript(&self, span: Span, obj: VarId, index: VarId, is_slice: bool) -> Step {
        let Some(o) = self.need(obj)?.and_then(Ty::obj) else {
            return Ok(None);
        };
        let item = match (&o.args, is_slice) {
            (Args::None, false) => return Ok(self.dunder(o, "__getitem__")),
            (Args::None, true) => return Ok(None),
            (Args::Map(_, v), _) => (**v).clone(),
            // A slice keeps the container type (a tuple's length is lost).
            (Args::Items(_), true) => return Ok(Some(Ty::Inst(o.clone()))),
            (Args::Tuple(_), true) => {
                return Ok(Some(Ty::container(
                    "builtins.tuple",
                    Args::items(o.args.item()),
                )));
            }
            (Args::Items(e), false) => (**e).clone(),
            (Args::Tuple(es), false) => self.position(span, obj, index, es),
        };
        item_ty(item)
    }

    /// The item at constant position `index` of a tuple, or any of its items. An unpacking
    /// target takes its position only when the targets match the tuple's length: a starred
    /// target (`first, *rest = ...`) would shift the ones after it.
    fn position(&self, span: Span, obj: VarId, index: VarId, es: &[Elem]) -> Elem {
        let any = || es.iter().fold(Elem::Empty, |acc, e| acc.join(e));
        let Some(&(n, at)) = self.ints.get(&index) else {
            return any();
        };
        if at == span && self.unpacked.get(&obj) != Some(&es.len()) {
            return any();
        }
        let len = i64::try_from(es.len()).unwrap_or(i64::MAX);
        let pos = if n < 0 { len + n } else { n };
        usize::try_from(pos)
            .ok()
            .and_then(|p| es.get(p))
            .cloned()
            .unwrap_or_else(any)
    }

    /// The result of calling the named object `fqn`: the builtins that wrap an iterable keep
    /// its items, anything else returns what [`TypeCtx::call_result`] says. A container they
    /// build is one whatever it wraps, so an unset argument adds no items yet (as in a display):
    /// a loop rebinding `items = list(items)` keeps its type.
    fn call_named(&self, fqn: &str, call: Passed<'_>) -> Step {
        let args = call.args;
        let items = |i: usize| args.get(i).map_or(Elem::Empty, |a| self.items_of(*a));
        let iterator = |class: &str, item: Elem| Ty::container(class, Args::items(item));
        Ok(Some(match fqn {
            "builtins.enumerate" => {
                iterator(fqn, pair(Elem::Of(Ty::inst("builtins.int")), items(0)))
            }
            "builtins.zip" => {
                let columns = (0..args.len()).map(items).collect();
                iterator(
                    fqn,
                    Elem::Of(Ty::container("builtins.tuple", Args::Tuple(columns))),
                )
            }
            "builtins.reversed" | "builtins.iter" if args.len() == 1 => iterator(fqn, items(0)),
            "builtins.filter" => iterator(fqn, items(1)),
            "itertools.chain" => {
                let item = (0..args.len()).fold(Elem::Empty, |acc, i| acc.join(&items(i)));
                iterator(fqn, item)
            }
            "itertools.chain.from_iterable" => {
                let inner = match items(0) {
                    Elem::Of(t) => self.items(Some(&t)),
                    other => other,
                };
                iterator(fqn, inner)
            }
            // `object.__new__(cls)` allocates an instance of `cls` without initializing it.
            "builtins.object.__new__" if !args.is_empty() => match self.need(args[0])? {
                Some(Ty::Ref(class) | Ty::Class(class)) => Ty::inst(class.as_str()),
                _ => return Ok(None),
            },
            "builtins.type" if args.len() == 1 && call.star_from.is_none() => {
                return Ok(self.need(args[0])?.and_then(|t| self.class_of(t)));
            }
            "builtins.sorted" => Ty::container("builtins.list", Args::items(items(0))),
            "builtins.list" | "builtins.set" | "builtins.frozenset" => {
                Ty::container(fqn, Args::items(items(0)))
            }
            "builtins.tuple" if args.is_empty() => Ty::container(fqn, Args::Tuple(Vec::new())),
            "builtins.tuple" => Ty::container(fqn, Args::items(items(0))),
            "builtins.dict" => {
                let args = match args.first() {
                    None => Args::map(Elem::Empty, Elem::Empty),
                    Some(a) => match self.need(*a).map(|t| t.and_then(Ty::obj).map(|o| &o.args)) {
                        Err(Pending) => Args::map(Elem::Empty, Elem::Empty),
                        Ok(Some(Args::Map(k, v))) => Args::Map(k.clone(), v.clone()),
                        // `dict(pairs)` of `(key, value)` tuples.
                        Ok(Some(other)) => match other.item() {
                            Elem::Of(Ty::Inst(Obj {
                                args: Args::Tuple(kv),
                                ..
                            })) if kv.len() == 2 => Args::map(kv[0].clone(), kv[1].clone()),
                            _ => Args::None,
                        },
                        Ok(None) => Args::None,
                    },
                };
                Ty::container(fqn, args)
            }
            // `next(it, default)`: a `None` default adds nothing.
            "builtins.next" if !args.is_empty() => {
                let default = args.get(1).map_or(Elem::Empty, |d| self.fact(*d).elem());
                return item_ty(items(0).join(&default));
            }
            "functools.partial" if !args.is_empty() => {
                return Ok(self
                    .need(args[0])?
                    .map(|t| Ty::Partial(Box::new(t.clone()))));
            }
            "typing.cast" | "typing_extensions.cast" if !args.is_empty() => {
                return Ok(self.type_form(args[0]));
            }
            // A library class that copies its argument (`collections.deque(xs)`) makes an
            // instance of itself.
            "collections.deque" | "collections.OrderedDict" | "builtins.bytearray" => {
                return Ok(self.cx.call_result(fqn, |i, k| self.passed(call, i, k)));
            }
            // A copy (`copy.copy`, `copy.deepcopy`, `dataclasses.replace`) has its original's
            // type.
            _ if !args.is_empty()
                && self
                    .cx
                    .catalog(fqn)
                    .is_some_and(|e| e.copy_sanitizer.is_some()) =>
            {
                return Ok(self.need(args[0])?.cloned());
            }
            _ => return Ok(self.cx.call_result(fqn, |i, k| self.passed(call, i, k))),
        }))
    }

    fn method_call(&self, recv: VarId, method: &str, call: Passed<'_>) -> Step {
        let args = call.args;
        let cx = self.cx;
        let r = self.need(recv)?;
        if let Some(Ty::Ref(fqn)) = r
            && !cx.project.is_project_class(fqn)
        {
            return self.call_named(&cx.project.canonical(&format!("{fqn}.{method}")), call);
        }
        // `Model.__new__(Model)`, `super().__new__(cls)`: what `object.__new__` allocates.
        if method == "__new__"
            && let Some(Ty::Ref(class) | Ty::Class(class) | Ty::Super(class)) = r
            && cx
                .project
                .new_is_object_new(class, matches!(r, Some(Ty::Super(_))), cx.kb)
        {
            return self.call_named("builtins.object.__new__", call);
        }
        if let Some(o) = r.and_then(Ty::obj)
            && let Some(item) = self.container_method(o, method, args)
        {
            return item_ty(item);
        }
        Ok(r.and_then(|recv| self.member_call(recv, method, call)))
    }

    /// The class the static type of a member of `recv` comes from, whether the lookup starts
    /// past it (`super()`), and the instance `Self` stands for there.
    fn static_receiver<'t>(&self, recv: &'t Ty) -> Option<(&'t str, bool, Ty)> {
        match recv {
            Ty::Inst(o) => Some((&o.class, false, recv.clone())),
            Ty::Super(c) => Some((c, true, Ty::inst(c.as_str()))),
            Ty::Ref(c) | Ty::Class(c) if self.cx.project.is_project_class(c) => {
                Some((c, false, Ty::inst(c.as_str())))
            }
            Ty::Ref(_)
            | Ty::Class(_)
            | Ty::SelfType
            | Ty::Partial(_)
            | Ty::Coroutine(_)
            | Ty::Union(_) => None,
        }
    }

    /// The declared type of reading `attr` on a value of type `recv`: a property getter's
    /// return annotation or the attribute's annotation along its MRO (`Self` bound to it).
    fn member(&self, recv: &Ty, attr: &str) -> Option<Ty> {
        if let Ty::Union(os) = recv {
            return join_all(os.iter().map(|o| self.member(&Ty::Inst(o.clone()), attr)));
        }
        let cx = self.cx;
        let (class, skip_self, self_ty) = self.static_receiver(recv)?;
        cx.project
            .member_type(class, attr, skip_self)
            .and_then(|t| cx.project.declared_ty(t))
            .and_then(|t| t.bind_self(Some(&self_ty)))
    }

    /// What calling method `method` on a value of type `recv` returns: the return annotation of
    /// the definition the receiver's MRO finds first (`Self` bound to the receiver), as for a
    /// type checker; past the project classes, what the ecosystem knowledge base says of the
    /// method of the first external base: a copy (`model_copy`, `list.copy`) has its receiver's
    /// type.
    fn member_call(&self, recv: &Ty, method: &str, call: Passed<'_>) -> Option<Ty> {
        if let Ty::Union(os) = recv {
            return join_all(
                os.iter()
                    .map(|o| self.member_call(&Ty::Inst(o.clone()), method, call)),
            );
        }
        let cx = self.cx;
        let (class, skip_self, self_ty) = self.static_receiver(recv)?;
        if let Some(key) = cx.project.mro_definition(class, method, skip_self) {
            let decl = cx.project.function_decl(&key)?;
            // Calling what a property returns is calling its value, not the getter; a class
            // object holds the property object itself, which raises when called.
            if decl.reads_as_property() {
                let on_instance = matches!(recv, Ty::Inst(_) | Ty::Super(_));
                let value = on_instance.then(|| self.member(recv, method)).flatten();
                return value.and_then(|v| self.called(&v, call));
            }
            return decl
                .return_type
                .as_ref()
                .and_then(|t| cx.project.declared_ty(t))
                .and_then(|t| t.bind_self(Some(&self_ty)))
                .map(|t| decl.call_type(t));
        }
        match cx.project.method_targets(class, method, skip_self, cx.kb) {
            Callee::External(symbol)
                if cx
                    .catalog(&symbol)
                    .is_some_and(|e| e.copy_sanitizer.is_some()) =>
            {
                Some(self_ty)
            }
            callee => cx.callee_result(&callee, |i, k| self.passed(call, i, k)),
        }
    }

    /// What calling a value of type `callee` returns: a partial object calls what it wraps, an
    /// instance its `__call__`.
    fn called(&self, callee: &Ty, call: Passed<'_>) -> Option<Ty> {
        match callee {
            Ty::Ref(fqn) | Ty::Class(fqn) => {
                self.cx.call_result(fqn, |i, k| self.passed(call, i, k))
            }
            // What the partial object binds shifts the arguments: they are not followed.
            Ty::Partial(inner) => self.called(inner, Passed::default()),
            other => self.member_call(other, "__call__", call),
        }
    }

    /// The type the type expression in `var` names (`typing.cast`'s first argument): a string
    /// annotation, or a class object.
    fn type_form(&self, var: VarId) -> Option<Ty> {
        let cx = self.cx;
        match self.strs.get(&var) {
            Some(text) => cx
                .module
                .resolve_annotation_ty(text)
                .and_then(|t| cx.project.declared_ty(&t))
                .and_then(|t| t.bind_self(self.self_ty.as_ref())),
            None => match self.fact(var) {
                Fact::Known(Ty::Ref(class) | Ty::Class(class)) => Some(Ty::inst(class)),
                _ => None,
            },
        }
    }

    /// What a method of a container with known arguments returns: the views and values of a
    /// mapping, and the item `pop` removes.
    fn container_method(&self, o: &Obj, method: &str, args: &[VarId]) -> Option<Elem> {
        let view = |class: &str, item: Elem| Elem::Of(Ty::container(class, Args::items(item)));
        // A `None` default adds nothing.
        let or_default = |v: &Elem| match args.get(1) {
            Some(d) => v.join(&self.fact(*d).elem()),
            None => v.clone(),
        };
        Some(match (&o.args, method) {
            (Args::Map(k, v), "items") => {
                view("typing.ItemsView", pair((**k).clone(), (**v).clone()))
            }
            (Args::Map(_, v), "values") => view("typing.ValuesView", (**v).clone()),
            (Args::Map(k, _), "keys") => view("typing.KeysView", (**k).clone()),
            (Args::Map(_, v), "get" | "pop" | "setdefault") => or_default(v),
            (Args::Map(k, v), "popitem") => pair((**k).clone(), (**v).clone()),
            (Args::Items(e), "pop") => (**e).clone(),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests;
