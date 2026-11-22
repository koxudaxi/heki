//! Paths through one function, over its normal and exception edges (Advisory and Profitability
//! only, NEVER a Soundness premise): the branches its code decides, whether a point runs
//! whenever the function returns normally, and whether it runs on every iteration of a loop
//! that completes.
//!
//! - **Decided branches** ([`Decided`]): a test of a literal or of a module constant
//!   ([`ModuleConstants::ints`], `strs`, `bools`, `typing.TYPE_CHECKING` included), through
//!   `not`, comparisons of two such values, and the phis of `and` / `or` along the edges that can
//!   run, never takes its other side. The code only that side reaches is dead for every fact.
//! - **Exceptions**: an instruction raises into its block's handler unless the assumptions every
//!   exact claim states rule it out ([`PathReader::raises`]). A handler's type is not matched, so
//!   any handler may take any exception. A `with` body's exception goes on past the `with` (the
//!   IR's view: its `__exit__` may suppress it) unless the reader establishes that no manager
//!   of the statement suppresses ([`PathReader::exit_suppresses`]).
//! - **Must facts** ([`Paths::on_every_return`], [`Paths::on_every_iteration`]): every path that
//!   completes a region passes a point, decided as dominance in a graph of two nodes per block:
//!   its head, before its first raising instruction runs, which holds the block's exception
//!   edge, and its tail, after every raising instruction returned, which holds its normal
//!   edges. The exception edges of the later raising instructions go to the same handler, and
//!   every path to them passes the first, so moving them to the head changes no dominator.
//!   A point "called" is passed on the path where its own call raises: whenever the function
//!   returns normally, the call has been made.

use super::{FnIndex, ModuleConstants};
use crate::walk::{Answer, Budget};
use crate::{
    BlockId, CompareOpKind, ConstLiteral, Inst, InstKind, IrFunction, ModuleIr, NaturalLoop,
    Terminator, UnaryOpKind, VarId,
};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::cell::{OnceCell, RefCell};

#[cfg(test)]
mod tests;

/// How far the call at a point has run on a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Progress {
    /// It was made: an exception the call itself raises still counts.
    Called,
    /// It returned normally.
    Returned,
}

/// Whether every path that completes a region passes a point. The region is the function's
/// normal returns, or the iterations of a loop that reach its back edge (a `continue` or the end
/// of its body); a path that leaves the loop by `break`, `return`, or an exception does not
/// complete an iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Must {
    Every,
    /// Some path completes the region without the point.
    NotEvery,
    /// No path completes the region: the function never returns normally, or every iteration
    /// of the loop leaves it.
    NeverCompletes,
    /// No path reaches the point (also through branches the code decides).
    Dead,
}

// ------------------------------------------------------------------------ decided branches

/// The branches a function's code decides, and the blocks that some path reaches without
/// taking a side a decided branch never takes.
#[derive(Debug, Clone)]
pub struct Decided {
    live: Vec<bool>,
    /// Per block, the successor its decided branch never takes.
    never_taken: Vec<Option<BlockId>>,
}

impl Decided {
    /// Some path from the entry reaches `b`. A block reached only along the back edges of
    /// loops counts as reached: the edges are judged in one pass in reverse postorder, and an
    /// edge from a later block is taken as one that runs.
    pub fn is_live(&self, b: BlockId) -> bool {
        self.live.get(b.0 as usize).copied().unwrap_or(false)
    }

    /// The edge from `from` to `to` (normal or exception one) can run.
    pub fn takes(&self, from: BlockId, to: BlockId) -> bool {
        self.is_live(from) && self.never_taken[from.0 as usize] != Some(to)
    }

    /// The side the branch ending `b` never takes, when its test is decided.
    pub fn never_taken(&self, b: BlockId) -> Option<BlockId> {
        self.never_taken.get(b.0 as usize).copied().flatten()
    }
}

/// A value the code fixes, for the comparisons a decided test may make.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Scalar<'a> {
    None,
    Bool(bool),
    Int(i64),
    Str(&'a str),
}

impl Scalar<'_> {
    fn truth(self) -> bool {
        match self {
            Self::None => false,
            Self::Bool(b) => b,
            Self::Int(n) => n != 0,
            Self::Str(s) => !s.is_empty(),
        }
    }

    /// `a op b`, for operators whose outcome on two such values Python fixes: equality of two
    /// values of one kind (`1 == True` mixes kinds and is left open), and identity of `None`
    /// and the booleans, which are singletons.
    fn compare(op: CompareOpKind, a: Self, b: Self) -> Option<bool> {
        let same_kind = std::mem::discriminant(&a) == std::mem::discriminant(&b);
        let singletons = |x: Self| matches!(x, Self::None | Self::Bool(_));
        match op {
            CompareOpKind::Eq if same_kind => Some(a == b),
            CompareOpKind::NotEq if same_kind => Some(a != b),
            CompareOpKind::Is | CompareOpKind::IsNot if singletons(a) && singletons(b) => {
                Some((a == b) == (op == CompareOpKind::Is))
            }
            // `None` is no other value, and an `int` or `str` is never `None` or a boolean.
            CompareOpKind::Is | CompareOpKind::IsNot if singletons(a) != singletons(b) => {
                Some(op == CompareOpKind::IsNot)
            }
            _ => None,
        }
    }
}

/// The value `v` holds when the code fixes it: a literal, a module constant, or
/// `typing.TYPE_CHECKING` read as an attribute of the module.
pub(super) fn scalar<'a>(ix: &FnIndex<'a>, v: VarId) -> Option<Scalar<'a>> {
    let module: &'a ModuleConstants = ix.module;
    let shape = &ix.shape;
    match &shape.inst(shape.copy_root(v))?.kind {
        InstKind::Const { literal, .. } => match literal {
            ConstLiteral::None => Some(Scalar::None),
            ConstLiteral::Bool(b) => Some(Scalar::Bool(*b)),
            ConstLiteral::Int(n) => Some(Scalar::Int(*n)),
            ConstLiteral::Str(s) => Some(Scalar::Str(s.as_str())),
            _ => None,
        },
        InstKind::LoadGlobal {
            name,
            is_lazy_import: false,
            ..
        } if !module.dynamic => module
            .bools
            .get(name)
            .map(|b| Scalar::Bool(*b))
            .or_else(|| module.ints.get(name).map(|n| Scalar::Int(*n)))
            .or_else(|| module.strs.get(name).map(|s| Scalar::Str(s))),
        InstKind::AttrLoad { obj, attr, .. } if !module.dynamic => {
            match &shape.inst(shape.copy_root(*obj))?.kind {
                InstKind::LoadGlobal {
                    name,
                    is_lazy_import: false,
                    ..
                } => module
                    .bools
                    .get(&format!("{name}.{attr}"))
                    .map(|b| Scalar::Bool(*b)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The truth of a test the code decides, from the truths of the values it reads that the
/// pass in reverse postorder has decided before it.
fn decided_truth(
    ix: &FnIndex<'_>,
    kind: &InstKind,
    truths: &FxHashMap<VarId, bool>,
) -> Option<bool> {
    match kind {
        InstKind::Copy { src, .. } => truths.get(src).copied(),
        InstKind::UnaryOp {
            op: UnaryOpKind::Not,
            operand,
            ..
        } => truths.get(operand).map(|t| !t),
        InstKind::Compare { op, lhs, rhs, .. } => {
            Scalar::compare(*op, scalar(ix, *lhs)?, scalar(ix, *rhs)?)
        }
        InstKind::Const {
            literal: ConstLiteral::Float(text),
            ..
        } => text.parse::<f64>().ok().map(|x| x != 0.0),
        _ => None,
    }
}

impl<'a> FnIndex<'a> {
    /// The branches the code of this function decides ([`Decided`]), computed once in one pass
    /// in reverse postorder: a value's definition comes before its uses there, except a phi's
    /// inputs along back edges, which leave the phi undecided.
    pub fn decided(&self) -> &Decided {
        self.decided.get_or_init(|| {
            let shape = &self.shape;
            let n = self.f.blocks.len();
            let mut live = vec![false; n];
            let mut never_taken: Vec<Option<BlockId>> = vec![None; n];
            let mut judged = vec![false; n];
            let mut truths: FxHashMap<VarId, bool> = FxHashMap::default();
            // The edge from `pred` into `to` can run, as far as the blocks judged so far tell.
            let may_take = |pred: BlockId,
                            to: BlockId,
                            live: &[bool],
                            never_taken: &[Option<BlockId>],
                            judged: &[bool]| {
                let p = pred.0 as usize;
                !judged[p] || (live[p] && never_taken[p] != Some(to))
            };
            for &b in shape.rpo() {
                let bi = b.0 as usize;
                live[bi] = bi == 0
                    || shape
                        .preds(b)
                        .iter()
                        .any(|p| may_take(*p, b, &live, &never_taken, &judged));
                judged[bi] = true;
                if !live[bi] {
                    continue;
                }
                let bb = self.f.block(b);
                for phi in &bb.phis {
                    let mut agreed: Option<bool> = None;
                    let mut decided = true;
                    for &(pred, x) in &phi.incoming {
                        if !may_take(pred, b, &live, &never_taken, &judged) {
                            continue;
                        }
                        match (truths.get(&x), agreed) {
                            (Some(t), None) => agreed = Some(*t),
                            (Some(t), Some(a)) if *t == a => {}
                            _ => decided = false,
                        }
                    }
                    if decided && let Some(t) = agreed {
                        truths.insert(phi.dst, t);
                    }
                }
                for inst in &bb.insts {
                    let Some(dst) = inst.kind.dst() else {
                        continue;
                    };
                    let truth = scalar(self, dst)
                        .map(Scalar::truth)
                        .or_else(|| decided_truth(self, &inst.kind, &truths));
                    if let Some(t) = truth {
                        truths.insert(dst, t);
                    }
                }
                if let Terminator::Branch {
                    cond,
                    then_bb,
                    else_bb,
                } = bb.terminator
                {
                    never_taken[bi] = match truths.get(&cond) {
                        Some(true) => Some(else_bb),
                        Some(false) => Some(then_bb),
                        None => None,
                    };
                }
            }
            Decided { live, never_taken }
        })
    }
}

// --------------------------------------------------------------------------- module bindings

/// The names Python 3.11 binds in `builtins` (every later version binds them too), without the
/// module dunders, sorted.
const BUILTIN_NAMES: &[&str] = &[
    "ArithmeticError",
    "AssertionError",
    "AttributeError",
    "BaseException",
    "BaseExceptionGroup",
    "BlockingIOError",
    "BrokenPipeError",
    "BufferError",
    "BytesWarning",
    "ChildProcessError",
    "ConnectionAbortedError",
    "ConnectionError",
    "ConnectionRefusedError",
    "ConnectionResetError",
    "DeprecationWarning",
    "EOFError",
    "Ellipsis",
    "EncodingWarning",
    "EnvironmentError",
    "Exception",
    "ExceptionGroup",
    "False",
    "FileExistsError",
    "FileNotFoundError",
    "FloatingPointError",
    "FutureWarning",
    "GeneratorExit",
    "IOError",
    "ImportError",
    "ImportWarning",
    "IndentationError",
    "IndexError",
    "InterruptedError",
    "IsADirectoryError",
    "KeyError",
    "KeyboardInterrupt",
    "LookupError",
    "MemoryError",
    "ModuleNotFoundError",
    "NameError",
    "None",
    "NotADirectoryError",
    "NotImplemented",
    "NotImplementedError",
    "OSError",
    "OverflowError",
    "PendingDeprecationWarning",
    "PermissionError",
    "ProcessLookupError",
    "RecursionError",
    "ReferenceError",
    "ResourceWarning",
    "RuntimeError",
    "RuntimeWarning",
    "StopAsyncIteration",
    "StopIteration",
    "SyntaxError",
    "SyntaxWarning",
    "SystemError",
    "SystemExit",
    "TabError",
    "TimeoutError",
    "True",
    "TypeError",
    "UnboundLocalError",
    "UnicodeDecodeError",
    "UnicodeEncodeError",
    "UnicodeError",
    "UnicodeTranslateError",
    "UnicodeWarning",
    "UserWarning",
    "ValueError",
    "Warning",
    "ZeroDivisionError",
    "abs",
    "aiter",
    "all",
    "anext",
    "any",
    "ascii",
    "bin",
    "bool",
    "breakpoint",
    "bytearray",
    "bytes",
    "callable",
    "chr",
    "classmethod",
    "compile",
    "complex",
    "copyright",
    "credits",
    "delattr",
    "dict",
    "dir",
    "divmod",
    "enumerate",
    "eval",
    "exec",
    "exit",
    "filter",
    "float",
    "format",
    "frozenset",
    "getattr",
    "globals",
    "hasattr",
    "hash",
    "help",
    "hex",
    "id",
    "input",
    "int",
    "isinstance",
    "issubclass",
    "iter",
    "len",
    "license",
    "list",
    "locals",
    "map",
    "max",
    "memoryview",
    "min",
    "next",
    "object",
    "oct",
    "open",
    "ord",
    "pow",
    "print",
    "property",
    "quit",
    "range",
    "repr",
    "reversed",
    "round",
    "set",
    "setattr",
    "slice",
    "sorted",
    "staticmethod",
    "str",
    "sum",
    "super",
    "tuple",
    "type",
    "vars",
    "zip",
];

/// What a module's globals hold whenever one of its functions runs, under the assumptions
/// every exact claim states (bindings are static, imports resolve as configured): the names
/// its body binds once, on every path through it, that nothing else binds or deletes; which of
/// them a plain `import` binds, and to what module; and whether its builtins are the real ones.
#[derive(Debug, Clone, Default)]
pub struct ModuleBindings {
    once: FxHashSet<String>,
    /// The module each `import m` (`import m as n`, `import m.sub`) of `once` binds.
    modules: FxHashMap<String, String>,
    /// Names some code of the module binds or deletes as globals.
    bound: FxHashSet<String>,
    /// The module's globals are bound behind the IR (`exec`, `globals()`), or a star import may
    /// rebind any public name: no builtin is trusted.
    untrusted: bool,
    star_import: bool,
}

impl ModuleBindings {
    /// The bindings of `ir`'s module.
    pub fn of(ir: &ModuleIr) -> Self {
        let mut stores: FxHashMap<&str, u32> = FxHashMap::default();
        let mut out = Self {
            untrusted: ir.constants.dynamic || ir.has_star_import,
            star_import: ir.has_star_import,
            ..Self::default()
        };
        for f in &ir.functions {
            for inst in f.blocks.iter().flat_map(|b| &b.insts) {
                match &inst.kind {
                    InstKind::StoreGlobal { name, .. } => {
                        *stores.entry(name).or_default() += 1;
                        out.bound.insert(name.clone());
                    }
                    InstKind::Delete {
                        name, global: true, ..
                    } => {
                        // Deleted: never bound once.
                        *stores.entry(name).or_default() += 2;
                        out.bound.insert(name.clone());
                    }
                    _ => {}
                }
            }
        }
        if ir.constants.dynamic {
            return out;
        }
        let module = ir.function(ir.module_fn);
        let ix = FnIndex::new(module, &ir.constants);
        let budget = Budget::of_function("module bindings", module);
        let paths = Paths::new(&ix, &PathReader::ir(), &budget);
        for bb in &module.blocks {
            for (i, inst) in bb.insts.iter().enumerate() {
                let InstKind::StoreGlobal { name, value } = &inst.kind else {
                    continue;
                };
                // A star import may rebind any public name.
                let public = !name.starts_with('_');
                if stores.get(name.as_str()) != Some(&1)
                    || (out.star_import && public)
                    || paths.on_every_return((bb.id, i), Progress::Called)
                        != Answer::Known(Must::Every)
                {
                    continue;
                }
                if let Some(InstKind::Import {
                    module: None,
                    name: imported,
                    is_lazy: false,
                    ..
                }) = ix.source(*value).map(|i| &i.kind)
                {
                    // `import a.b` binds `a`; `import a.b as n` binds `n` to `a.b`.
                    let first = imported.split('.').next().unwrap_or(imported);
                    let bound_module = if name == first { first } else { imported };
                    out.modules.insert(name.clone(), bound_module.to_owned());
                }
                out.once.insert(name.clone());
            }
        }
        out
    }

    /// Whether the module binds `name` once, on every path through its body, and nothing else
    /// binds or deletes it: whenever one of its functions runs, the name holds what that binding
    /// bound (an import a rewrite can reuse, say).
    pub fn bound_once(&self, name: &str) -> bool {
        self.once.contains(name)
    }

    /// A load of the global `name` finds a binding: one bound once, or a real builtin.
    fn binds(&self, name: &str) -> bool {
        self.once.contains(name)
            || (!self.untrusted
                && !self.bound.contains(name)
                && BUILTIN_NAMES.binary_search(&name).is_ok())
    }
}

// ------------------------------------------------------------------------- the reader's part

/// What the function's reader knows that its IR does not.
pub struct PathReader<'r> {
    /// The bindings of the function's module; `None` takes every instruction the IR marks as
    /// raising ([`InstKind::may_raise`]) as one that raises.
    pub bindings: Option<&'r ModuleBindings>,
    /// The module `module` has a member `attr` (the catalog's word, or the project's code).
    pub module_member: &'r dyn Fn(&str, &str) -> bool,
    /// The value `obj` has the attribute `attr` whenever the code runs, so looking it up does not
    /// raise: a method or attribute of its class, by the catalog or the project's code.
    pub attribute_found: &'r dyn Fn(VarId, &str) -> bool,
    /// Whether the `__exit__` (`__aexit__`) of the context manager `mgr` may suppress an
    /// exception: `Known(false)` when it returns a false value on every path.
    pub exit_suppresses: &'r dyn Fn(VarId) -> Answer<bool>,
}

impl PathReader<'_> {
    /// The reader that knows nothing beyond the IR.
    pub fn ir() -> PathReader<'static> {
        fn no_member(_: &str, _: &str) -> bool {
            false
        }
        fn no_attribute(_: VarId, _: &str) -> bool {
            false
        }
        fn may_suppress(_: VarId) -> Answer<bool> {
            Answer::Open
        }
        PathReader {
            bindings: None,
            module_member: &no_member,
            attribute_found: &no_attribute,
            exit_suppresses: &may_suppress,
        }
    }

    /// The reader that knows the module's bindings, and nothing else beyond the IR.
    pub fn with_bindings(bindings: &ModuleBindings) -> PathReader<'_> {
        PathReader {
            bindings: Some(bindings),
            ..PathReader::ir()
        }
    }

    /// Whether `inst` may raise an exception a handler can take, under the assumptions every
    /// exact claim states: bindings are static, imports resolve as configured, the catalog
    /// describes the library, and no asynchronous or resource-exhaustion exception arises
    /// (`KeyboardInterrupt` and other signals, `MemoryError`, `RecursionError`), since those
    /// can arise at any instruction. So a load of a global the module binds once or of a real
    /// builtin does not raise, nor does an attribute load of a member of a module a plain
    /// `import` binds once, nor a list or tuple display without unpacking. Everything else the
    /// IR marks does: a load that may find the name unbound, an attribute load of any other
    /// object (a lazy initialization may test it with `except AttributeError`), a subscript, a
    /// set or dict display (which hashes), an import, a call, an `await`, an operator.
    pub fn raises(&self, ix: &FnIndex<'_>, inst: &Inst) -> bool {
        if !inst.kind.may_raise() {
            return false;
        }
        let Some(bindings) = self.bindings else {
            return true;
        };
        match &inst.kind {
            InstKind::LoadGlobal {
                name,
                is_lazy_import: false,
                ..
            } => !bindings.binds(name),
            InstKind::AttrLoad { obj, attr, .. } => self.lookup_raises(ix, *obj, attr),
            InstKind::AllocBuiltin {
                dst,
                kind: crate::BuiltinKind::List | crate::BuiltinKind::Tuple,
                elems,
            } => (0..elems.len()).any(|i| ix.f.is_starred(*dst, i)),
            _ => true,
        }
    }
}

impl PathReader<'_> {
    /// Whether looking up the attribute `attr` of `obj` may raise (an attribute load, or the
    /// lookup a method call makes before it calls anything): unless `obj` is a module a plain
    /// `import` binds once and `attr` one of its members, or the reader finds the attribute.
    pub fn lookup_raises(&self, ix: &FnIndex<'_>, obj: VarId, attr: &str) -> bool {
        let Some(bindings) = self.bindings else {
            return true;
        };
        let module = ix.source(obj).and_then(|o| match &o.kind {
            InstKind::LoadGlobal {
                name,
                is_lazy_import: false,
                ..
            } => bindings.modules.get(name),
            _ => None,
        });
        let member = module.is_some_and(|m| (self.module_member)(m, attr));
        !member && !(self.attribute_found)(obj, attr)
    }
}

// --------------------------------------------------------------------------------- the graph

/// Passes over a graph before its dominators count as not decided (two suffice in the
/// reducible graphs Python's statements make: one to find them, one to see nothing changes).
const MAX_PASSES: u32 = 4;

/// A region's graph, two nodes per block in reverse postorder (the head of the `k`-th block is
/// `2k`, its tail `2k + 1`) and a sink last, and the nodes that dominate the sink.
struct Dominance {
    /// The position of each block of the region.
    local: FxHashMap<BlockId, u32>,
    /// Per node, its immediate dominator ([`UNREACHED`] for a node the root does not reach).
    idom: Vec<u32>,
    /// Per node, whether the region's root reaches it.
    reached: Vec<bool>,
    /// Per node, whether every path from the root to the sink passes it.
    on_every: Vec<bool>,
    sink: u32,
}

impl Dominance {
    fn must(&self, block: BlockId, tail: bool) -> Must {
        let Some(k) = self.local.get(&block) else {
            return Must::Dead;
        };
        let node = (2 * k + u32::from(tail)) as usize;
        if !self.reached[node] {
            Must::Dead
        } else if !self.reached[self.sink as usize] {
            Must::NeverCompletes
        } else if self.on_every[node] {
            Must::Every
        } else {
            Must::NotEvery
        }
    }
}

/// Which blocks every path from a function's entry to another block passes, over the graph of
/// its path facts ([`Paths::block_dominators`]).
#[derive(Debug, Clone)]
pub struct BlockDominators {
    local: FxHashMap<BlockId, u32>,
    idom: Vec<u32>,
}

impl BlockDominators {
    /// Whether every path from the entry to block `to` passes block `through`: also when no
    /// path reaches `to` (none avoids `through` either).
    pub fn passes(&self, through: BlockId, to: BlockId) -> bool {
        let head = |b: BlockId| self.local.get(&b).map(|k| 2 * k);
        let Some(mut x) = head(to).filter(|x| self.idom[*x as usize] != UNREACHED) else {
            return true;
        };
        let Some(target) = head(through) else {
            return false;
        };
        // Up the dominator tree, which a node's immediate dominator always climbs: it ends at
        // the root (node 0).
        loop {
            if x == target {
                return true;
            }
            if x == 0 {
                return false;
            }
            x = self.idom[x as usize];
        }
    }
}

/// The region a graph covers.
#[derive(Clone, Copy)]
enum Region<'l> {
    /// The function, to its normal returns.
    Function,
    /// One loop's body (by its index in `f.loops`), from its header to its back edges.
    Loop(usize, &'l NaturalLoop),
}

/// The path facts of one function ([`Paths::on_every_return`], [`Paths::on_every_iteration`]),
/// computed on demand.
pub struct Paths<'p, 'a> {
    ix: &'p FnIndex<'a>,
    /// Per block, the position of its first raising instruction (`u32::MAX`: none).
    first_raise: Vec<u32>,
    /// Per block, where an exception it raises goes in this function (`None`: out of it).
    exception_to: Vec<Option<BlockId>>,
    /// The method calls whose lookup of the method may raise before anything is called.
    lookups_raise: FxHashSet<(BlockId, usize)>,
    /// The budget of the owner reading these facts.
    budget: &'p Budget,
    on_return: OnceCell<Answer<Dominance>>,
    on_iteration: RefCell<FxHashMap<usize, Answer<Dominance>>>,
}

impl<'p, 'a> Paths<'p, 'a> {
    /// The path facts of `ix`'s function, with what `reader` knows, charged to `budget`.
    pub fn new(ix: &'p FnIndex<'a>, reader: &PathReader<'_>, budget: &'p Budget) -> Self {
        let f = ix.f;
        let first_raise = f
            .blocks
            .iter()
            .map(|bb| {
                bb.insts
                    .iter()
                    .position(|i| reader.raises(ix, i))
                    .map_or(u32::MAX, |p| u32::try_from(p).unwrap_or(u32::MAX - 1))
            })
            .collect();
        let exception_to = f
            .blocks
            .iter()
            .map(|bb| exception_target(f, bb.exc_target, reader))
            .collect();
        let lookups_raise = f
            .blocks
            .iter()
            .flat_map(|bb| {
                bb.insts
                    .iter()
                    .enumerate()
                    .map(move |(i, inst)| (bb.id, i, inst))
            })
            .filter(|(_, _, inst)| match &inst.kind {
                InstKind::MethodCall { recv, method, .. } => {
                    reader.lookup_raises(ix, *recv, method)
                }
                _ => false,
            })
            .map(|(b, i, _)| (b, i))
            .collect();
        Self {
            ix,
            first_raise,
            exception_to,
            lookups_raise,
            budget,
            on_return: OnceCell::new(),
            on_iteration: RefCell::new(FxHashMap::default()),
        }
    }

    /// Whether the point `at` (instruction `at.1` of block `at.0`) has run, to `progress`,
    /// whenever the function returns normally.
    pub fn on_every_return(&self, at: (BlockId, usize), progress: Progress) -> Answer<Must> {
        let (block, tail) = self.node_at(at, progress);
        self.on_return
            .get_or_init(|| self.dominance(Region::Function))
            .as_ref()
            .map(|d| d.must(block, tail))
    }

    /// Whether the point `at` inside `f.loops[li]` has run, to `progress`, on every iteration
    /// of the loop that completes. `NeverCompletes` when every iteration leaves the loop: the
    /// point then runs at most once per run of it.
    pub fn on_every_iteration(
        &self,
        li: usize,
        at: (BlockId, usize),
        progress: Progress,
    ) -> Answer<Must> {
        let Some(lp) = self.ix.f.loops.get(li) else {
            return Answer::Open;
        };
        // A loop no path reaches (a decided branch excludes it) has no iteration at all.
        let reached = self
            .on_return
            .get_or_init(|| self.dominance(Region::Function))
            .as_ref()
            .map(|d| d.must(lp.header, false));
        match reached {
            Answer::Known(Must::Dead) | Answer::Open => return reached,
            Answer::Known(_) => {}
        }
        let (block, tail) = self.node_at(at, progress);
        if let Some(known) = self.on_iteration.borrow().get(&li) {
            return known.as_ref().map(|d| d.must(block, tail));
        }
        let dominance = self.dominance(Region::Loop(li, lp));
        let answer = dominance.as_ref().map(|d| d.must(block, tail));
        self.on_iteration.borrow_mut().insert(li, dominance);
        answer
    }

    /// Which blocks every path from the entry to another block passes ([`BlockDominators`]), over
    /// the graph `on_every_return` reads: a branch the code decides is not taken, and a block
    /// raises into its handler only where an instruction of it may raise.
    pub fn block_dominators(&self) -> Answer<BlockDominators> {
        self.on_return
            .get_or_init(|| self.dominance(Region::Function))
            .as_ref()
            .map(|d| BlockDominators {
                local: d.local.clone(),
                idom: d.idom.clone(),
            })
    }

    /// The node that is passed exactly when the point is: the head while no raising
    /// instruction before it (or, for `Returned`, at it) has run. A method call whose lookup of
    /// the method may raise has made no call until the lookup returned: it is called only after
    /// a raising step of its own.
    fn node_at(&self, (block, index): (BlockId, usize), progress: Progress) -> (BlockId, bool) {
        let first = self
            .first_raise
            .get(block.0 as usize)
            .copied()
            .unwrap_or(u32::MAX) as usize;
        let past_its_own_step =
            progress == Progress::Returned || self.lookups_raise.contains(&(block, index));
        let tail = first < index || (first == index && past_its_own_step);
        (block, tail)
    }

    fn dominance(&self, region: Region<'_>) -> Answer<Dominance> {
        let f = self.ix.f;
        let decided = self.ix.decided();
        let shape = &self.ix.shape;
        let (blocks, root): (Vec<BlockId>, BlockId) = match region {
            Region::Function => (shape.rpo().to_vec(), BlockId(0)),
            Region::Loop(li, lp) => {
                let blocks = shape
                    .rpo()
                    .iter()
                    .copied()
                    .filter(|b| shape.in_loop(li, *b))
                    .collect();
                (blocks, lp.header)
            }
        };
        // The root comes first: it dominates the region, and a dominator comes before the
        // blocks it dominates in reverse postorder.
        if blocks.first() != Some(&root) || !self.budget.take(blocks.len()) {
            return Answer::Open;
        }
        let local: FxHashMap<BlockId, u32> = blocks
            .iter()
            .enumerate()
            .map(|(k, b)| (*b, k as u32))
            .collect();
        let sink = 2 * blocks.len() as u32;
        let head = |b: BlockId| local.get(&b).map(|k| 2 * k);
        let mut preds: Vec<SmallVec<[u32; 2]>> = vec![SmallVec::new(); sink as usize + 1];
        for (k, &b) in blocks.iter().enumerate() {
            let (h, t) = (2 * k as u32, 2 * k as u32 + 1);
            preds[t as usize].push(h);
            let to_handler = self.exception_to[b.0 as usize].and_then(head);
            if self.first_raise[b.0 as usize] != u32::MAX
                && let Some(x) = to_handler
            {
                preds[x as usize].push(h);
            }
            let bb = f.block(b);
            match &bb.terminator {
                Terminator::Return(_) => {
                    if matches!(region, Region::Function) {
                        preds[sink as usize].push(t);
                    }
                }
                Terminator::Raise(_) => {
                    if let Some(x) = to_handler {
                        preds[x as usize].push(t);
                    }
                }
                term => {
                    for s in term.successors() {
                        if !decided.takes(b, s) {
                            continue;
                        }
                        let to = match region {
                            Region::Loop(_, lp) if s == lp.header => Some(sink),
                            _ => head(s),
                        };
                        if let Some(x) = to {
                            preds[x as usize].push(t);
                        }
                    }
                }
            }
        }
        let Some(idom) = self.immediate_dominators(&preds) else {
            return Answer::Open;
        };
        let reached: Vec<bool> = idom.iter().map(|d| *d != UNREACHED).collect();
        let mut on_every = vec![false; preds.len()];
        if reached[sink as usize] {
            let mut x = sink;
            loop {
                on_every[x as usize] = true;
                if x == 0 {
                    break;
                }
                x = idom[x as usize];
            }
        }
        Answer::Known(Dominance {
            local,
            idom,
            reached,
            on_every,
            sink,
        })
    }

    /// The immediate dominator of each node of the graph of `preds` from node 0, numbered in
    /// reverse postorder ([`UNREACHED`] for a node 0 does not reach); `None` past the steps
    /// left or the pass limit.
    fn immediate_dominators(&self, preds: &[SmallVec<[u32; 2]>]) -> Option<Vec<u32>> {
        let mut idom = vec![UNREACHED; preds.len()];
        idom[0] = 0;
        let nodes = preds.len();
        let mut reducible = true;
        let mut settled = false;
        for _ in 0..MAX_PASSES {
            let mut changed = false;
            let mut steps = 0usize;
            for x in 1..preds.len() {
                let mut found = UNREACHED;
                // The later predecessors first: along a chain of them the walk up is short.
                for &p in preds[x].iter().rev() {
                    if idom[p as usize] == UNREACHED {
                        continue;
                    }
                    if found == UNREACHED {
                        found = p;
                        continue;
                    }
                    // A dominator comes before what it dominates, so each walk up ends within
                    // the nodes; one that would not (a graph that is not reducible) leaves the
                    // dominators undecided.
                    let (mut a, mut b) = (p, found);
                    let mut walked = 0;
                    while a != b && walked <= nodes {
                        while a > b && walked <= nodes {
                            a = idom[a as usize];
                            walked += 1;
                        }
                        while b > a && walked <= nodes {
                            b = idom[b as usize];
                            walked += 1;
                        }
                    }
                    steps += walked;
                    reducible &= a == b;
                    found = a;
                }
                steps += preds[x].len();
                if idom[x] != found {
                    idom[x] = found;
                    changed = true;
                }
            }
            if !self.budget.take(steps) {
                return None;
            }
            settled = !changed;
            if settled {
                break;
            }
        }
        (settled && reducible).then_some(idom)
    }
}

const UNREACHED: u32 = u32::MAX;

/// Where an exception raised in a block with the handler `target` goes: past a `with` exit
/// whose managers none suppress, to the handler around that `with`.
fn exception_target(
    f: &IrFunction,
    target: Option<BlockId>,
    reader: &PathReader<'_>,
) -> Option<BlockId> {
    let mut to = target?;
    // Each step goes out to an enclosing statement, so there are fewer than the blocks.
    for _ in 0..f.blocks.len() {
        let managers: SmallVec<[VarId; 2]> = f
            .block(to)
            .insts
            .iter()
            .map_while(|i| match i.kind {
                InstKind::ExitWith { mgr, .. } => Some(mgr),
                _ => None,
            })
            .collect();
        let goes_on = !managers.is_empty()
            && managers
                .iter()
                .all(|m| (reader.exit_suppresses)(*m) == Answer::Known(false));
        if !goes_on {
            break;
        }
        to = f.block(to).exc_target?;
    }
    Some(to)
}
