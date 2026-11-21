//! Receiver typing of one function (Advisory lane): a forward fixpoint over the SSA-CFG in
//! reverse postorder, repeated until no variable's type changes.
//!
//! Every variable starts unset: no value reaches it yet. A value that never arrives (a local read
//! before any assignment on that path, `None`, an item of an empty display) leaves what is
//! computed from it unset. Displays, the items added to a local container, and phis skip unset
//! values, so a variable rebound in a loop (`node = node.parent`) or assigned on some paths only
//! (a walrus in `and` / `or`) has the type of the values that reach it. A type only grows (the
//! new one is joined with the old), and classes and nesting depth are bounded, so the fixpoint
//! ends; a function without loops needs one pass.
//!
//! Types come from parameter, local, attribute, and return annotations (with the arguments of
//! generic containers), constructors, and imports; they flow through copies, attributes, calls,
//! and the items of containers: iteration, subscripts, unpacking, `dict` views and `get` /
//! `pop`, the builtins that wrap an iterable (`enumerate`, `zip`, `reversed`, `sorted`, `iter`,
//! `next`, `list`, `tuple`, `set`, `frozenset`, `dict`, `filter`, `itertools.chain`), displays,
//! and the items added to a local container (comprehensions build theirs that way). A
//! comprehension or generator expression with a scope of its own iterates the value its
//! enclosing function hands it, so the enclosing function is inferred first.

use crate::resolver::{self, Callee, ProjectDecls, TypeCtx, Val};
use crate::types::{Args, Elem, Obj, Ty};
use heki_ast::Span;
use heki_ir::{
    BlockId, BuiltinKind, ConstLiteral, DictEntry, FunctionId, FunctionKind, Inst, InstKind,
    IrFunction, PhiNode, Terminator, UnaryOpKind, VarId,
};
use heki_kb::ArgFact;
use rustc_hash::FxHashMap;
use std::cell::{OnceCell, RefCell};
use std::rc::Rc;

/// The arguments a call passes, as written (for the catalog's argument cases).
#[derive(Clone, Copy, Default)]
struct Passed<'c> {
    args: &'c [VarId],
    kwargs: &'c [(Option<String>, VarId)],
    star_from: Option<u8>,
}

/// What the fixpoint knows about a variable.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// The function builds comprehension or generator-expression scopes of its own.
    encloses: bool,
    /// The types `isinstance` checks give variables in the blocks they guard, where they differ
    /// from the variable's own.
    narrowed: FxHashMap<(BlockId, VarId), Ty>,
}

impl Inferred {
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

/// Inference state shared by the functions of one module: comprehension and
/// generator-expression scopes need the types of the function that builds them.
#[derive(Default)]
pub(crate) struct Scopes {
    /// Child scope -> the function building it and its variable of the iterated value.
    parents: OnceCell<FxHashMap<FunctionId, (FunctionId, VarId)>>,
    /// The types of functions that build child scopes.
    enclosing: RefCell<FxHashMap<FunctionId, Rc<Inferred>>>,
    /// What a child scope produces when it iterates a value of a given type.
    outputs: RefCell<FxHashMap<(FunctionId, Option<Ty>), Elem>>,
}

/// The function id of a non-inlined comprehension call (Python 3.11).
fn comprehension_id(callee: &str) -> FunctionId {
    FunctionId(
        callee
            .strip_prefix("__comp_")
            .and_then(|n| n.parse().ok())
            .expect("the builder names non-inlined comprehension calls `__comp_<id>`"),
    )
}

impl TypeCtx<'_> {
    /// The types of `f`'s variables.
    pub(crate) fn infer_types(&self, f: &IrFunction) -> Rc<Inferred> {
        if let Some(done) = self.scopes.enclosing.borrow().get(&f.id) {
            return Rc::clone(done);
        }
        let seed = self.seed_of(f);
        let inferred = Rc::new(Engine::new(self, f, seed).run());
        if inferred.encloses {
            self.scopes
                .enclosing
                .borrow_mut()
                .insert(f.id, Rc::clone(&inferred));
        }
        inferred
    }

    /// For a child scope, the type of the value its enclosing function hands it (`None` inside
    /// when unknown); `None` for other functions.
    fn seed_of(&self, f: &IrFunction) -> Option<Option<Ty>> {
        if !matches!(
            f.name.as_str(),
            "<genexpr>" | "<listcomp>" | "<setcomp>" | "<dictcomp>"
        ) {
            return None;
        }
        let parents = self.scopes.parents.get_or_init(|| {
            let mut out = FxHashMap::default();
            for g in &self.ir().functions {
                for inst in g.blocks.iter().flat_map(|b| &b.insts) {
                    match &inst.kind {
                        InstKind::MakeGenExpr {
                            func_id: Some(id),
                            outer_iter,
                            ..
                        } => {
                            out.insert(*id, (g.id, *outer_iter));
                        }
                        InstKind::Call {
                            callee_var: None,
                            callee,
                            args,
                            ..
                        } => {
                            out.insert(comprehension_id(callee), (g.id, args[0]));
                        }
                        _ => {}
                    }
                }
            }
            out
        });
        let &(parent, var) = parents.get(&f.id)?;
        Some(
            self.infer_types(self.ir().function(parent))
                .ty(var)
                .cloned(),
        )
    }

    /// What child scope `id` produces when it iterates a value of type `seed`: the items a
    /// generator expression yields, or the container a comprehension returns.
    fn child_output(&self, id: FunctionId, seed: Option<Ty>) -> Elem {
        let key = (id, seed);
        if let Some(done) = self.scopes.outputs.borrow().get(&key) {
            return done.clone();
        }
        let child = self.ir().function(id);
        let inferred = Engine::new(self, child, Some(key.1.clone())).run();
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
    /// The source of each `Copy`.
    copies: FxHashMap<VarId, VarId>,
    /// Container displays built in the function (`[]`, `{}`, `set()` is a call), by kind.
    displays: FxHashMap<VarId, BuiltinKind>,
    /// The items (or keys) and values added to each display after it is built.
    added: FxHashMap<VarId, (Elem, Elem)>,
    encloses: bool,
    changed: bool,
    /// A pass added items to a display.
    added_changed: bool,
    /// `isinstance` narrowing (only for functions that call a function named `isinstance`).
    narrowing: Option<Narrowing<'e>>,
    /// What `isinstance` checks guarding the current block give its variables.
    cur: FxHashMap<VarId, Ty>,
    /// [`Self::implied`] of each condition, truth, block of the narrowed types it starts from,
    /// and depth, while one block is entered: a condition reaches the same phi through many
    /// paths (a loop header's phi is an input of itself on each `continue`).
    implied_memo: RefCell<FxHashMap<(VarId, bool, BlockId, u8), Narrowed>>,
}

/// The CFG facts `isinstance` narrowing needs, and the narrowed types at each block's entry
/// (`None` until a pass reaches the block).
struct Narrowing<'e> {
    preds: Vec<Vec<BlockId>>,
    defs: FxHashMap<VarId, &'e InstKind>,
    /// Each phi with its block.
    phis: FxHashMap<VarId, (BlockId, &'e PhiNode)>,
    entry: Vec<Option<FxHashMap<VarId, Ty>>>,
}

impl<'e, 'a> Engine<'e, 'a> {
    fn new(cx: &'e TypeCtx<'a>, f: &'e IrFunction, seed: Option<Option<Ty>>) -> Self {
        let mut engine = Self {
            cx,
            f,
            owner: cx.owner_fqn(f),
            self_ty: cx.owner_fqn(f).map(Ty::inst),
            reads_globals: f.is_module_body || f.is_class_body,
            facts: vec![Fact::Unset; f.var_names.len()],
            names: FxHashMap::default(),
            declared: FxHashMap::default(),
            ints: FxHashMap::default(),
            strs: FxHashMap::default(),
            unpacked: FxHashMap::default(),
            copies: FxHashMap::default(),
            displays: FxHashMap::default(),
            added: FxHashMap::default(),
            encloses: false,
            changed: false,
            added_changed: false,
            narrowing: None,
            cur: FxHashMap::default(),
            implied_memo: RefCell::default(),
        };
        engine.seed_params();
        if let Some(seed) = seed {
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
                preds: f.predecessors(),
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
                InstKind::Copy { dst, src } => {
                    self.copies.insert(*dst, *src);
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
                } => self.encloses = true,
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
        let rpo = self.f.rpo_blocks();
        // Without loops every operand is computed before its users in reverse postorder: one
        // pass is final unless it added items to a display already computed.
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
            for &bb in &rpo {
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
            if !self.changed || acyclic && !self.added_changed {
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
            encloses: self.encloses,
            narrowed,
        }
    }

    /// The fact of `var` (see [`Self::need`]).
    fn fact(&self, var: VarId) -> Fact {
        match self.need(var) {
            Ok(Some(t)) => Fact::Known(t.clone()),
            Ok(None) => Fact::Unknown,
            Err(Pending) => Fact::Unset,
        }
    }

    /// The type of `var` for a transfer that needs it. A declared local has its declaration, as
    /// for a type checker, unless what it holds is narrower: an instance of a subclass
    /// (`x: Base = Sub()`), or anything for a declaration of `Any` / `object`.
    fn need(&self, var: VarId) -> Result<Option<&Ty>, Pending> {
        if let Some(t) = self.cur.get(&var) {
            return Ok(Some(t));
        }
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
        for &p in &n.preds[bb.0 as usize] {
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
        let targets = self.cx.project.method_targets(&o.class, name, false);
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
                // A value no instruction defines is a parameter or a phi: either may come from
                // a parameter.
                let no_phi = |_: VarId, _: &mut Vec<VarId>| false;
                let undefined = |d: VarId| self.f.def_inst(d).is_none();
                resolver::arg_fact(
                    |d| self.f.def_inst(d).map(|(_, i)| i),
                    class,
                    |n| match self.need(n).ok().flatten()? {
                        Ty::Ref(fqn) => Some(fqn.clone()),
                        _ => None,
                    },
                    (&no_phi, &undefined),
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
            InstKind::LoadGlobal { name, .. } => Some(cx.global(name)),
            // An unassigned local of a function holds no value on this path.
            InstKind::LoadName { name, .. } if self.reads_globals => Some(cx.global(name)),
            InstKind::LoadName { .. } => return Err(Pending),
            InstKind::Import { .. } => cx.import_type(inst.span),
            InstKind::Copy { src, .. } => self.need(*src)?.cloned(),
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
                match cx.child_output(*id, seed) {
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
                return item_ty(cx.child_output(comprehension_id(callee), seed));
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

    /// Items added to a display after it is built: `append` / `insert` / `extend` of a list,
    /// `add` / `update` of a set, `xs[i] = v`, and `d[k] = v` / `setdefault` / `update` of a
    /// dict (also through copies of the variable holding it).
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

    /// The display a variable holds (through copies), with its kind.
    fn display_of(&self, mut var: VarId) -> Option<(VarId, BuiltinKind)> {
        loop {
            if let Some(kind) = self.displays.get(&var) {
                return Some((var, *kind));
            }
            var = *self.copies.get(&var)?;
        }
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
            // A copy (`copy.copy`, `copy.deepcopy`, `dataclasses.replace`) has its original's
            // type.
            _ if !args.is_empty()
                && self
                    .cx
                    .kb
                    .lookup(fqn)
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
            // Calling what a property returns is calling its value, not the getter.
            let decl = cx
                .project
                .function_decl(&key)
                .filter(|d| d.property_type.is_none())?;
            return decl
                .return_type
                .as_ref()
                .and_then(|t| cx.project.declared_ty(t))
                .and_then(|t| t.bind_self(Some(&self_ty)))
                .map(|t| decl.call_type(t));
        }
        match cx.project.method_targets(class, method, skip_self) {
            Callee::External(symbol)
                if cx
                    .kb
                    .lookup(&symbol)
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
