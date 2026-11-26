//! Registration contexts: the argument a registration hands a callback, carried to the handler
//! the callback selects by name (ADR-024). A registration stores what a factory returns in a
//! slot of the rows of a table; the table is kept by an attribute, a row is picked from it and
//! its slot called; the callback builds an object of the class it captured and calls a method
//! of it through an adapter that orders the arguments by the method's signature; the method
//! looks a handler up with `getattr(self, <name>)` and calls it. Each handler that lookup can
//! find gets the condition "the name equals the handler's name", and the parameters bound to the
//! registering object get its class: one context per registration, never a fact of every call
//! of the handler.
//!
//! Each function's part is extracted while its IR exists ([`DispatchFacts`], kept in
//! `EffectSummary::dispatch`); [`contexts`] joins them over the index. The join interprets the
//! small programs of the functions on the way, one budget per registration.

use std::sync::Arc;

use heki_ast::Span;
use heki_ir::walk::{Answer, Budget, FnShape};
use heki_ir::{
    BuiltinKind, CompareOpKind, ConstLiteral, FunctionKind, InstKind, IrFunction, ModuleIr,
    Terminator, UnaryOpKind, VarId,
};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

use crate::resolver::{ProjectDecls, TypeCtx};
use crate::{EffectSummary, FuncKey, ProjectIndex};

/// Most instructions a function's [`Program`] keeps: longer functions are not followed.
const MAX_PROGRAM: usize = 128;
/// Most nested calls the interpretation of one registration follows.
const MAX_DEPTH: usize = 16;
/// Most handlers one lookup by name may find: a class with more is not followed.
const MAX_CANDIDATES: usize = 8;

/// What a function contributes to registration contexts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchFacts {
    /// Its body, when it is short enough to be followed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<Program>,
    /// What it stores in the rows of a list it returns.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<Row>,
    /// The calls of a class it hands the table a method of `self` returns.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hands: Vec<Hand>,
    /// `self.<attr> = <param>` (or `<param> or []`) in an `__init__`: `(attr, param)`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keeps: Vec<(String, u8)>,
    /// Its calls of a slot of the row a helper picks from `self.<attr>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub row_calls: Vec<RowCall>,
    /// What it returns of the row it picks from a parameter.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub picks: Vec<Pick>,
}

impl DispatchFacts {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The facts with every position zeroed (Early Cutoff compares facts, not positions).
    pub fn without_locations(&self) -> Self {
        let mut out = self.clone();
        let none = Span { start: 0, end: 0 };
        out.rows.iter_mut().for_each(|r| r.site = none);
        out.row_calls.iter_mut().for_each(|c| c.site = none);
        for op in out
            .program
            .iter_mut()
            .flat_map(|p| &mut p.blocks)
            .flat_map(|b| &mut b.ops)
        {
            if let Op::Call(call) = op {
                call.site = none;
            }
        }
        out
    }
}

/// A module-level object a name is bound to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Named {
    Function(FuncKey),
    Class(String),
    /// A module, a builtin, a library object, or a global of the project, by fqn.
    Other(String),
}

/// A value a registration passes to its factory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Leaf {
    Param(u8),
    Named(Named),
    Str(String),
    None,
    Other,
}

/// A row of a table: the slot holds what calling `callee` with `args` returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub slot: u8,
    pub callee: Named,
    pub args: Vec<Leaf>,
    pub kwargs: Vec<(String, Leaf)>,
    /// The factory call.
    pub site: Span,
}

/// Where a call of a class binds what a method of `self` returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArgAt {
    Position(u8),
    Keyword(String),
}

/// `t = self.<method>()` handed to `<class>(..., t, ...)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hand {
    pub method: String,
    pub class: String,
    pub arg: ArgAt,
}

/// `x, f = <helper>(..., self.<attr>, ...)` then `f(<args>)`: the call of `slot` of the row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowCall {
    pub attr: String,
    pub helper: FuncKey,
    /// The position of `self.<attr>` among the helper's arguments.
    pub helper_arg: u8,
    pub slot: u8,
    /// The names of the values the call passes, by position and by keyword.
    pub args: Vec<String>,
    pub kwargs: Vec<(String, String)>,
    pub awaited: bool,
    pub site: Span,
}

/// The function returns, in `slot`, field `field` of the row it picks from parameter `param`
/// (every other return gives `None`s).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pick {
    pub param: u8,
    pub field: u8,
    pub slot: u8,
}

/// A function's body as the join follows it: its blocks over its SSA values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Program {
    pub is_async: bool,
    pub params: Vec<Param>,
    /// Its decorators, innermost first (`None`: an expression other than a name).
    pub decorators: Vec<Option<Named>>,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    pub var: u32,
    pub kind: ParamKind,
    /// The default: `Some(Lit::Other)` for one that is no literal.
    pub default: Option<Lit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParamKind {
    PositionalOnly,
    Positional,
    Star,
    KeywordOnly,
    DoubleStar,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lit {
    None,
    Bool(bool),
    Int(i64),
    Str(String),
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    /// `(value, [(predecessor, input)])`.
    pub joins: Vec<(u32, Vec<(u32, u32)>)>,
    pub ops: Vec<Op>,
    pub end: End,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum End {
    Jump(u32),
    Branch(u32, u32, u32),
    /// The next item of `iter` in `item`, then `body`; `done` once it is exhausted.
    Next {
        iter: u32,
        item: u32,
        body: u32,
        done: u32,
    },
    Return(Option<u32>),
    Raise,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    Const(u32, Lit),
    Global(u32, Named),
    Cell(u32, String),
    Copy(u32, u32),
    Tuple(u32, Vec<u32>),
    List(u32, Vec<u32>),
    Attr(u32, u32, String),
    SetAttr(u32, String),
    Item(u32, u32, u32),
    /// `dst = lhs is rhs` (`true`: `is not`).
    Is(u32, u32, u32, bool),
    /// `dst = lhs in rhs` (`true`: `not in`).
    In(u32, u32, u32, bool),
    Not(u32, u32),
    /// A nested function with its cells; `true` when the function binds a captured name again.
    Closure(u32, FuncKey, Vec<(String, u32)>, bool),
    Call(Box<CallOp>),
    Await(u32, u32),
    Iter(u32, u32),
    /// A string the function builds (an f-string), whose value is not followed.
    Text(u32),
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallOp {
    pub dst: u32,
    /// The value called, or `(receiver, method)`.
    pub callee: u32,
    pub method: Option<String>,
    /// Each argument, `true` when it is starred.
    pub args: Vec<(u32, bool)>,
    /// `None`: `**mapping`.
    pub kwargs: Vec<(Option<String>, u32)>,
    pub site: Span,
}

// ------------------------------------------------------------------------------- extraction

/// The facts of `f`, a function of the module `cx` types (`None` when it has none).
pub(crate) fn extract(
    cx: &TypeCtx<'_>,
    ir: &ModuleIr,
    f: &IrFunction,
) -> Option<Arc<DispatchFacts>> {
    let shape = FnShape::new(f);
    let facts = DispatchFacts {
        program: program(cx, ir, f),
        rows: rows(cx, ir, f, &shape),
        hands: hands(cx, f, &shape),
        keeps: keeps(f, &shape),
        row_calls: row_calls(cx, f, &shape),
        picks: picks(f, &shape),
    };
    (!facts.is_empty()).then(|| Arc::new(facts))
}

/// The object the global `name` of the module is bound to.
fn named(cx: &TypeCtx<'_>, name: &str) -> Named {
    let fqn = if cx.module.binds(name) {
        cx.project.canonical(&cx.module.global_read(name))
    } else {
        format!("builtins.{name}")
    };
    classify(cx.project, fqn)
}

/// A project class or function by its fqn, or another object.
fn classify(decls: &ProjectDecls, fqn: String) -> Named {
    if decls.is_project_class(&fqn) {
        return Named::Class(fqn);
    }
    let function = (fqn.match_indices('.').map(|(i, _)| i))
        .map(|i| FuncKey {
            module: fqn[..i].to_owned(),
            qualname: fqn[i + 1..].to_owned(),
        })
        .find(|k| decls.function_decl(k).is_some());
    function.map_or(Named::Other(fqn), Named::Function)
}

fn param_of(f: &IrFunction, v: VarId) -> Option<u8> {
    let i = f.params.iter().position(|p| p.var == v)?;
    u8::try_from(i).ok()
}

fn lit(c: &ConstLiteral) -> Lit {
    match c {
        ConstLiteral::None => Lit::None,
        ConstLiteral::Bool(b) => Lit::Bool(*b),
        ConstLiteral::Int(i) => Lit::Int(*i),
        ConstLiteral::Str(s) => Lit::Str(s.clone()),
        _ => Lit::Other,
    }
}

fn leaf(cx: &TypeCtx<'_>, f: &IrFunction, shape: &FnShape<'_>, v: VarId) -> Leaf {
    let v = shape.copy_root(v);
    if let Some(i) = param_of(f, v) {
        return Leaf::Param(i);
    }
    match shape.inst(v).map(|i| &i.kind) {
        Some(InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. }) => {
            Leaf::Named(named(cx, name))
        }
        Some(InstKind::Const { literal, .. }) => match lit(literal) {
            Lit::Str(s) => Leaf::Str(s),
            Lit::None => Leaf::None,
            _ => Leaf::Other,
        },
        _ => Leaf::Other,
    }
}

/// The call of a module-level callable `v` is, with plain arguments.
fn factory_call(cx: &TypeCtx<'_>, f: &IrFunction, shape: &FnShape<'_>, v: VarId) -> Option<Row> {
    let inst = shape.inst(shape.copy_root(v))?;
    let InstKind::Call {
        callee_var: Some(c),
        args,
        kwargs,
        star_from: None,
        ..
    } = &inst.kind
    else {
        return None;
    };
    let Leaf::Named(callee) = leaf(cx, f, shape, *c) else {
        return None;
    };
    let args: Vec<Leaf> = args.iter().map(|a| leaf(cx, f, shape, *a)).collect();
    let kwargs: Vec<(String, Leaf)> = (kwargs.iter())
        .map(|(k, a)| Some((k.clone()?, leaf(cx, f, shape, *a))))
        .collect::<Option<_>>()?;
    // Only a call handing the function's own receiver on registers it.
    let hands_self =
        (args.iter().chain(kwargs.iter().map(|(_, l)| l))).any(|l| *l == Leaf::Param(0));
    hands_self.then_some(Row {
        slot: 0,
        callee,
        args,
        kwargs,
        site: inst.span,
    })
}

/// The slots of the tuple display `v` is: `(slot, value)`.
fn tuple_items<'s>(shape: &FnShape<'s>, v: VarId) -> Option<&'s [VarId]> {
    match &shape.inst(shape.copy_root(v))?.kind {
        InstKind::AllocBuiltin {
            kind: BuiltinKind::Tuple,
            elems,
            ..
        } => Some(elems),
        _ => None,
    }
}

/// The rows a list `f` builds and returns holds: each list display `[]` whose every use
/// appends a tuple display to it (directly, or through a nested function appending a tuple of
/// its parameters), copies it, iterates it, or returns it, and which `f` returns, itself or as
/// a list of tuples keeping some of its slots.
fn rows(cx: &TypeCtx<'_>, ir: &ModuleIr, f: &IrFunction, shape: &FnShape<'_>) -> Vec<Row> {
    let mut out = Vec::new();
    for inst in f.blocks.iter().flat_map(|b| &b.insts) {
        if let InstKind::AllocBuiltin {
            dst,
            kind: BuiltinKind::List,
            elems,
        } = &inst.kind
            && elems.is_empty()
            && let Some(rows) = table_rows(cx, ir, f, shape, *dst)
        {
            out.extend(rows);
        }
    }
    out
}

fn table_rows(
    cx: &TypeCtx<'_>,
    ir: &ModuleIr,
    f: &IrFunction,
    shape: &FnShape<'_>,
    table: VarId,
) -> Option<Vec<Row>> {
    let names = |v: VarId| shape.copy_root(v) == table;
    let mut stored: Vec<(u8, VarId)> = Vec::new();
    let mut iterated = Vec::new();
    let mut returned = false;
    // A comprehension before 3.12 is a function of its own, called with the table.
    let mut comprehension = None;
    let vars = (0..f.var_names.len())
        .map(|i| VarId(i as u32))
        .filter(|v| names(*v));
    for v in vars {
        for u in shape.readers(v) {
            let kind = match u {
                heki_ir::walk::Use::Inst(b, i) => &f.blocks[b.0 as usize].insts[*i as usize].kind,
                heki_ir::walk::Use::Terminator(b) => {
                    returned |= matches!(f.blocks[b.0 as usize].terminator, Terminator::Return(_));
                    continue;
                }
                heki_ir::walk::Use::Phi(..) => return None,
            };
            match kind {
                InstKind::Copy { .. } => {}
                InstKind::MakeIter { dst, .. } => iterated.push(*dst),
                InstKind::Call {
                    dst,
                    callee_var,
                    callee,
                    args,
                    ..
                } if args.as_slice() == [v] => {
                    let id = heki_ir::comprehension_id(*callee_var, callee)?;
                    comprehension = Some((ir.function(heki_ir::FunctionId(id)), *dst));
                }
                InstKind::MethodCall {
                    recv,
                    method,
                    args,
                    kwargs,
                    ..
                } if *recv == v && method == "append" && kwargs.is_empty() && args.len() == 1 => {
                    let items = tuple_items(shape, args[0])?;
                    stored.extend(items.iter().enumerate().map(|(s, x)| (s as u8, *x)));
                }
                InstKind::MakeClosure {
                    dst,
                    func_id: Some(id),
                    ..
                } => stored.extend(appended(f, shape, ir.function(*id), *dst, v)?),
                _ => return None,
            }
        }
    }
    let kept = kept_slots(f, shape, table, &iterated, returned, comprehension)?;
    let rows = stored.into_iter().filter(|(s, _)| kept.contains(s));
    Some(
        rows.filter_map(|(slot, v)| {
            Some(Row {
                slot,
                ..factory_call(cx, f, shape, v)?
            })
        })
        .collect(),
    )
}

/// The values `f` hands, by slot, to the nested function `n` (made at `closure`) that appends
/// a tuple of its parameters to the list in its cell `cell`: each call of it in `f`.
fn appended(
    f: &IrFunction,
    shape: &FnShape<'_>,
    n: &IrFunction,
    closure: VarId,
    cell: VarId,
) -> Option<Vec<(u8, VarId)>> {
    let name = f.var_names.get(cell.0 as usize)?.as_deref()?;
    let insts: Vec<&InstKind> = n
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .map(|i| &i.kind)
        .collect();
    let plain = !n.params.iter().any(|p| p.is_vararg || p.is_kwarg);
    let elems = match insts.as_slice() {
        [
            InstKind::LoadDeref {
                dst: list,
                name: read,
            },
            InstKind::AllocBuiltin {
                dst: tuple,
                kind: BuiltinKind::Tuple,
                elems,
            },
            InstKind::MethodCall {
                recv, method, args, ..
            },
        ] if plain && read == name && recv == list && method == "append" && args == &[*tuple] => {
            elems
        }
        _ => return None,
    };
    let slots: Vec<Option<u8>> = (n.params.iter())
        .map(|p| elems.iter().position(|e| *e == p.var).map(|s| s as u8))
        .collect();
    let mut out = Vec::new();
    for v in (0..f.var_names.len()).map(|i| VarId(i as u32)) {
        if shape.copy_root(v) != closure {
            continue;
        }
        for u in shape.readers(v) {
            // Each call of the nested function (it may also be handed on, which adds rows the
            // model does not follow).
            if let heki_ir::walk::Use::Inst(b, i) = u
                && let InstKind::Call {
                    callee_var: Some(c),
                    args,
                    kwargs,
                    star_from: None,
                    ..
                } = &f.blocks[b.0 as usize].insts[*i as usize].kind
                && *c == v
            {
                stored_by(n, &slots, args, kwargs, &mut out);
            }
        }
    }
    Some(out)
}

/// The values a call with `args` and `kwargs` hands the appender `n`, by the slot each fills.
fn stored_by(
    n: &IrFunction,
    slots: &[Option<u8>],
    args: &[VarId],
    kwargs: &[(Option<String>, VarId)],
    out: &mut Vec<(u8, VarId)>,
) {
    let by_name = kwargs.iter().filter_map(|(k, a)| {
        let j = n.params.iter().position(|p| Some(&p.name) == k.as_ref())?;
        Some((j, *a))
    });
    for (j, a) in args.iter().copied().enumerate().chain(by_name) {
        if let Some(Some(s)) = slots.get(j) {
            out.push((*s, a));
        }
    }
}

/// The returned values of `f`, through copies.
fn returned_values(f: &IrFunction, shape: &FnShape<'_>) -> Vec<VarId> {
    (f.blocks.iter())
        .filter_map(|b| match b.terminator {
            Terminator::Return(Some(v)) => Some(shape.copy_root(v)),
            _ => None,
        })
        .collect()
}

/// The slots of the table's rows that what `f` returns keeps: every slot when it returns the
/// table, or those a list of tuples built from the table's rows copies (in `f`, or in the
/// comprehension function `f` calls with the table).
fn kept_slots(
    f: &IrFunction,
    shape: &FnShape<'_>,
    table: VarId,
    iterated: &[VarId],
    returned: bool,
    comprehension: Option<(&IrFunction, VarId)>,
) -> Option<Vec<u8>> {
    let values = returned_values(f, shape);
    let value = *values.first().filter(|_| values.len() == 1)?;
    if returned && value == table {
        return Some((0..=u8::MAX).collect());
    }
    match comprehension {
        Some((n, call)) if call == value => {
            let ns = FnShape::new(n);
            // The function reads the iterable it is called with as `.0`.
            let first = (0u32..)
                .zip(&n.var_names)
                .find(|(_, x)| x.as_deref() == Some(".0"))?
                .0;
            let first = VarId(first);
            let iterated: Vec<VarId> = (n.blocks.iter().flat_map(|b| &b.insts))
                .filter_map(|i| match &i.kind {
                    InstKind::MakeIter { dst, iterable, .. }
                        if ns.copy_root(*iterable) == first =>
                    {
                        Some(*dst)
                    }
                    _ => None,
                })
                .collect();
            let list = *returned_values(n, &ns).first()?;
            copied_slots(n, &ns, list, &iterated)
        }
        _ => copied_slots(f, shape, value, iterated),
    }
}

/// The slots of the rows of the iterations `iterated` that the tuples appended to the list
/// `value` copy: one append of a tuple whose slot `k` is item `k` of a row.
fn copied_slots(
    f: &IrFunction,
    shape: &FnShape<'_>,
    value: VarId,
    iterated: &[VarId],
) -> Option<Vec<u8>> {
    let appends: Vec<&[VarId]> = (shape.readers(value).iter())
        .filter_map(|u| {
            if let heki_ir::walk::Use::Inst(b, i) = u
                && let InstKind::MethodCall {
                    recv, method, args, ..
                } = &f.blocks[b.0 as usize].insts[*i as usize].kind
                && *recv == value
                && method == "append"
            {
                return Some(args.as_slice());
            }
            None
        })
        .collect();
    let item = appends
        .first()
        .filter(|a| appends.len() == 1 && a.len() == 1)?[0];
    let row_item = |e: VarId| -> Option<u8> {
        if let InstKind::SubscriptLoad { obj, index, .. } = &shape.inst(shape.copy_root(e))?.kind
            && let InstKind::IterNext { iter, .. } = &shape.inst(*obj)?.kind
            && iterated.contains(iter)
        {
            return u8::try_from(shape.int_const(*index)?).ok();
        }
        None
    };
    let items = tuple_items(shape, item)?;
    Some(
        (items.iter().enumerate())
            .filter(|(s, e)| row_item(**e) == Some(*s as u8))
            .map(|(s, _)| s as u8)
            .collect(),
    )
}

/// `t = self.<m>()` passed to a call of a project class.
fn hands(cx: &TypeCtx<'_>, f: &IrFunction, shape: &FnShape<'_>) -> Vec<Hand> {
    let Some(this) = f.params.first().map(|p| p.var) else {
        return Vec::new();
    };
    let table = |v: VarId| match &shape.inst(shape.copy_root(v))?.kind {
        InstKind::MethodCall {
            recv,
            method,
            args,
            kwargs,
            ..
        } if *recv == this && args.is_empty() && kwargs.is_empty() => Some(method.clone()),
        _ => None,
    };
    let mut out = Vec::new();
    for inst in f.blocks.iter().flat_map(|b| &b.insts) {
        let InstKind::Call {
            callee_var: Some(c),
            args,
            kwargs,
            star_from: None,
            ..
        } = &inst.kind
        else {
            continue;
        };
        let Leaf::Named(Named::Class(class)) = leaf(cx, f, shape, *c) else {
            continue;
        };
        let at = (args.iter().enumerate()).map(|(i, a)| (ArgAt::Position(i as u8), *a));
        let by_name = kwargs
            .iter()
            .filter_map(|(k, a)| Some((ArgAt::Keyword(k.clone()?), *a)));
        for (arg, v) in at.chain(by_name) {
            if let Some(method) = table(v) {
                out.push(Hand {
                    method,
                    class: class.clone(),
                    arg,
                });
            }
        }
    }
    out
}

/// `self.<attr> = <param>` and `self.<attr> = <param> or []` in an `__init__`.
fn keeps(f: &IrFunction, shape: &FnShape<'_>) -> Vec<(String, u8)> {
    let this = f.params.first().map(|p| p.var);
    if f.name != "__init__" {
        return Vec::new();
    }
    let param = |v: VarId| -> Option<u8> {
        let v = shape.copy_root(v);
        if let Some(i) = param_of(f, v) {
            return Some(i);
        }
        // `p or []`: the table holding the registered row is never empty.
        let join = f.blocks.iter().flat_map(|b| &b.phis).find(|p| p.dst == v)?;
        let mut params = join
            .incoming
            .iter()
            .filter_map(|(_, x)| param_of(f, shape.copy_root(*x)));
        let empty = join.incoming.iter().any(|(_, x)| {
            matches!(&shape.inst(shape.copy_root(*x)).map(|i| &i.kind),
                Some(InstKind::AllocBuiltin { kind: BuiltinKind::List, elems, .. }) if elems.is_empty())
        });
        let p = params.next()?;
        (empty && join.incoming.len() == 2).then_some(p)
    };
    (f.blocks.iter().flat_map(|b| &b.insts))
        .filter_map(|i| match &i.kind {
            InstKind::AttrStore { obj, attr, value } if Some(*obj) == this => {
                Some((attr.clone(), param(*value)?))
            }
            _ => None,
        })
        .collect()
}

/// The calls of a slot of the tuple a helper returns from `self.<attr>`.
fn row_calls(cx: &TypeCtx<'_>, f: &IrFunction, shape: &FnShape<'_>) -> Vec<RowCall> {
    let Some(this) = f.params.first().map(|p| p.var) else {
        return Vec::new();
    };
    // The name of an argument, or of the value it copies, or its position.
    let named = |v: VarId| f.var_names.get(v.0 as usize).cloned().flatten();
    let name = |v: &VarId| {
        let root = shape.copy_root(*v);
        named(*v)
            .or_else(|| named(root))
            .unwrap_or_else(|| format!("#{}", v.0))
    };
    let mut out = Vec::new();
    for inst in f.blocks.iter().flat_map(|b| &b.insts) {
        let InstKind::Call {
            callee_var: Some(c),
            args,
            kwargs,
            star_from: None,
            is_awaited,
            ..
        } = &inst.kind
        else {
            continue;
        };
        let Some(InstKind::SubscriptLoad { obj, index, .. }) =
            shape.inst(shape.copy_root(*c)).map(|i| &i.kind)
        else {
            continue;
        };
        let Some(InstKind::Call {
            callee_var: Some(h),
            args: helper_args,
            ..
        }) = shape.inst(*obj).map(|i| &i.kind)
        else {
            continue;
        };
        let Leaf::Named(Named::Function(helper)) = leaf(cx, f, shape, *h) else {
            continue;
        };
        let held = helper_args.iter().enumerate().find_map(|(i, a)| {
            match &shape.inst(shape.copy_root(*a))?.kind {
                InstKind::AttrLoad { obj, attr, .. } if *obj == this => {
                    Some((i as u8, attr.clone()))
                }
                _ => None,
            }
        });
        let named: Option<Vec<(String, String)>> = kwargs
            .iter()
            .map(|(k, v)| Some((k.clone()?, name(v))))
            .collect();
        let (Some((helper_arg, attr)), Some(slot), Some(kwargs)) =
            (held, shape.int_const(*index), named)
        else {
            continue;
        };
        out.push(RowCall {
            attr,
            helper,
            helper_arg,
            slot: u8::try_from(slot).unwrap_or(u8::MAX),
            args: args.iter().map(name).collect(),
            kwargs,
            awaited: *is_awaited,
            site: inst.span,
        });
    }
    out
}

/// Every return of `f` gives either `None`s or a tuple whose slots hold fields of the row it
/// iterates a parameter for: the `(field, slot)` pairs every such return keeps.
fn picks(f: &IrFunction, shape: &FnShape<'_>) -> Vec<Pick> {
    let mut kept: Option<Vec<Pick>> = None;
    for b in &f.blocks {
        let Terminator::Return(Some(v)) = b.terminator else {
            continue;
        };
        let Some(items) = tuple_items(shape, v) else {
            return Vec::new();
        };
        let none = |e: &VarId| {
            matches!(
                &shape.inst(shape.copy_root(*e)).map(|i| &i.kind),
                Some(InstKind::Const {
                    literal: ConstLiteral::None,
                    ..
                })
            )
        };
        if items.iter().all(none) {
            continue;
        }
        let field = |slot: usize, e: VarId| -> Option<Pick> {
            if let InstKind::SubscriptLoad { obj, index, .. } =
                &shape.inst(shape.copy_root(e))?.kind
                && let InstKind::IterNext { iter, .. } = &shape.inst(*obj)?.kind
                && let InstKind::MakeIter { iterable, .. } = &shape.inst(*iter)?.kind
            {
                return Some(Pick {
                    param: param_of(f, shape.copy_root(*iterable))?,
                    field: u8::try_from(shape.int_const(*index)?).ok()?,
                    slot: slot as u8,
                });
            }
            None
        };
        let here: Vec<Pick> = items
            .iter()
            .enumerate()
            .filter_map(|(s, e)| field(s, *e))
            .collect();
        kept = Some(match kept {
            None => here,
            Some(k) => k.into_iter().filter(|p| here.contains(p)).collect(),
        });
    }
    kept.unwrap_or_default()
}

/// The program of `f`, when it has at most [`MAX_PROGRAM`] instructions.
fn program(cx: &TypeCtx<'_>, ir: &ModuleIr, f: &IrFunction) -> Option<Program> {
    if f.blocks.iter().map(|b| b.insts.len()).sum::<usize>() > MAX_PROGRAM || f.is_module_body {
        return None;
    }
    let params = f.params.iter().map(|p| Param {
        name: p.name.clone(),
        var: p.var.0,
        kind: match () {
            () if p.is_vararg => ParamKind::Star,
            () if p.is_kwarg => ParamKind::DoubleStar,
            () if p.kw_only => ParamKind::KeywordOnly,
            () if p.pos_only => ParamKind::PositionalOnly,
            () => ParamKind::Positional,
        },
        default: p
            .has_default
            .then(|| p.default.as_ref().map_or(Lit::Other, lit)),
    });
    let decorators = f.decorators.iter().map(|d| {
        let plain = d
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.');
        plain.then(|| {
            let (head, rest) = d
                .split_once('.')
                .map_or((d.as_str(), None), |(h, r)| (h, Some(r)));
            match (named(cx, head), rest) {
                (Named::Other(m), Some(rest)) => classify(cx.project, format!("{m}.{rest}")),
                (n, _) => n,
            }
        })
    });
    let starred: FxHashSet<(VarId, u32)> = f.starred.iter().copied().collect();
    let names_bound = |name: &str| {
        f.var_names
            .iter()
            .filter(|n| n.as_deref() == Some(name))
            .count()
    };
    let op = |inst: &heki_ir::Inst| -> Op {
        match &inst.kind {
            InstKind::Const { dst, literal } => Op::Const(dst.0, lit(literal)),
            InstKind::LoadGlobal { dst, name, .. } | InstKind::LoadName { dst, name } => {
                Op::Global(dst.0, named(cx, name))
            }
            InstKind::LoadDeref { dst, name } => Op::Cell(dst.0, name.clone()),
            InstKind::Copy { dst, src } => Op::Copy(dst.0, src.0),
            InstKind::AllocBuiltin { dst, kind, elems } => {
                let elems = elems.iter().map(|e| e.0).collect();
                match kind {
                    BuiltinKind::Tuple => Op::Tuple(dst.0, elems),
                    BuiltinKind::List => Op::List(dst.0, elems),
                    _ => Op::Other,
                }
            }
            InstKind::AttrLoad { dst, obj, attr } => Op::Attr(dst.0, obj.0, attr.clone()),
            InstKind::AttrStore { obj, attr, .. } => Op::SetAttr(obj.0, attr.clone()),
            InstKind::SubscriptLoad {
                dst, obj, index, ..
            } => Op::Item(dst.0, obj.0, index.0),
            InstKind::Compare { dst, op, lhs, rhs } => match op {
                CompareOpKind::Is | CompareOpKind::IsNot => {
                    Op::Is(dst.0, lhs.0, rhs.0, *op == CompareOpKind::IsNot)
                }
                CompareOpKind::In | CompareOpKind::NotIn => {
                    Op::In(dst.0, lhs.0, rhs.0, *op == CompareOpKind::NotIn)
                }
                _ => Op::Other,
            },
            InstKind::UnaryOp {
                dst,
                op: UnaryOpKind::Not,
                operand,
            } => Op::Not(dst.0, operand.0),
            InstKind::MakeClosure {
                dst,
                func_id: Some(id),
                captured,
            } => {
                let n = ir.function(*id);
                let cells: Vec<(String, u32)> = (captured.iter())
                    .filter_map(|v| Some((f.var_names.get(v.0 as usize)?.clone()?, v.0)))
                    .collect();
                let rebound = cells.iter().any(|(name, _)| names_bound(name) > 1);
                let key = FuncKey {
                    module: cx.module.module_name.clone(),
                    qualname: crate::resolver::func_qualname(n).to_owned(),
                };
                Op::Closure(dst.0, key, cells, rebound)
            }
            InstKind::Call {
                dst,
                callee_var: Some(c),
                args,
                kwargs,
                ..
            } => call_op(*dst, c.0, None, args, kwargs, &starred, inst.span),
            InstKind::MethodCall {
                dst,
                recv,
                method,
                args,
                kwargs,
                ..
            } => call_op(
                *dst,
                recv.0,
                Some(method),
                args,
                kwargs,
                &starred,
                inst.span,
            ),
            InstKind::Await { dst, value } => Op::Await(dst.0, value.0),
            InstKind::MakeIter { dst, iterable, .. } => Op::Iter(dst.0, iterable.0),
            InstKind::Format { dst, .. }
            | InstKind::BuildString { dst, .. }
            | InstKind::Convert { dst, .. } => Op::Text(dst.0),
            _ => Op::Other,
        }
    };
    let blocks = f.blocks.iter().map(|b| {
        let mut ops: Vec<Op> = b.insts.iter().map(op).collect();
        let end = match b.terminator {
            Terminator::Jump(t) => End::Jump(t.0),
            Terminator::Branch {
                cond,
                then_bb,
                else_bb,
            } => match b.insts.last().map(|i| &i.kind) {
                Some(InstKind::IterNext { dst, iter, .. }) if *dst == cond => {
                    ops.pop();
                    End::Next {
                        iter: iter.0,
                        item: dst.0,
                        body: then_bb.0,
                        done: else_bb.0,
                    }
                }
                _ => End::Branch(cond.0, then_bb.0, else_bb.0),
            },
            Terminator::Return(v) => End::Return(v.map(|v| v.0)),
            Terminator::Raise(_) | Terminator::Unreachable => End::Raise,
        };
        Block {
            joins: (b.phis.iter())
                .map(|p| {
                    (
                        p.dst.0,
                        p.incoming.iter().map(|(b, v)| (b.0, v.0)).collect(),
                    )
                })
                .collect(),
            ops,
            end,
        }
    });
    Some(Program {
        is_async: f.kind == FunctionKind::Async,
        params: params.collect(),
        decorators: decorators.collect(),
        blocks: blocks.collect(),
    })
}

fn call_op(
    dst: VarId,
    callee: u32,
    method: Option<&String>,
    args: &[VarId],
    kwargs: &[(Option<String>, VarId)],
    starred: &FxHashSet<(VarId, u32)>,
    site: Span,
) -> Op {
    Op::Call(Box::new(CallOp {
        dst: dst.0,
        callee,
        method: method.cloned(),
        args: (args.iter().enumerate())
            .map(|(i, a)| (a.0, starred.contains(&(dst, i as u32))))
            .collect(),
        kwargs: kwargs.iter().map(|(k, v)| (k.clone(), v.0)).collect(),
        site,
    }))
}

// ------------------------------------------------------------------------------------- join

/// A handler a registration reaches, with the condition its name meets there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    pub handler: FuncKey,
    /// The handler's parameter bound to the registering object, and that object's class.
    pub param: u8,
    pub class: String,
    /// The function registering, and its factory call.
    pub registered_in: FuncKey,
    pub registration: Span,
    /// The call of the table's row.
    pub called_in: FuncKey,
    pub called_at: Span,
    /// The lookup by name and the call of what it found.
    pub selected_in: FuncKey,
    pub selected_at: Span,
    pub invoked_at: Span,
    /// The name looked up (the expression it is computed from, or the constant).
    pub selector: String,
    pub equals: String,
    /// The name is not a constant: other names may select other members, or none.
    pub others_open: bool,
}

/// One function's facts, for the join.
struct Entry<'i> {
    key: &'i FuncKey,
    facts: &'i DispatchFacts,
    local: &'i EffectSummary,
}

/// Every registration context of the project.
pub fn contexts(index: &ProjectIndex, decls: &ProjectDecls) -> Vec<Context> {
    let mut entries: Vec<Entry<'_>> = (index.files.values())
        .flat_map(|f| &f.local_functions)
        .filter_map(|(key, local)| {
            Some(Entry {
                key,
                facts: local.dispatch.as_deref()?,
                local,
            })
        })
        .collect();
    entries.sort_by(|a, b| a.key.cmp(b.key));
    let by_fqn: FxHashMap<String, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.key.fqn(), i))
        .collect();
    let units: usize = (entries.iter())
        .filter_map(|e| e.facts.program.as_ref())
        .flat_map(|p| &p.blocks)
        .map(|b| 1 + b.ops.len())
        .sum();
    let mut out = Vec::new();
    for e in &entries {
        for row in &e.facts.rows {
            let found = follow(&entries, &by_fqn, decls, e, row, units);
            // A registration the model cannot follow to the end gives no context.
            out.extend(found.open_means(Vec::new()));
        }
    }
    out
}

/// The methods of `class` (canonical) named `name`, as a project function key.
fn method_key(decls: &ProjectDecls, class: &str, name: &str) -> Option<FuncKey> {
    decls.function_taken(class, name)
}

/// The class `key` is a method of (`None` for a function).
fn owner_of(key: &FuncKey) -> Option<String> {
    let (class, _) = key.qualname.rsplit_once('.')?;
    (!class.contains("<locals>")).then(|| format!("{}.{class}", key.module))
}

fn follow(
    entries: &[Entry<'_>],
    by_fqn: &FxHashMap<String, usize>,
    decls: &ProjectDecls,
    e: &Entry<'_>,
    row: &Row,
    units: usize,
) -> Answer<Vec<Context>> {
    let Some(owner) = owner_of(e.key) else {
        return Answer::Open;
    };
    let Some(flow) = consumer(entries, by_fqn, decls, e.key, &owner, row.slot) else {
        return Answer::Open;
    };
    let mut run = Run {
        entries,
        by_fqn,
        decls,
        budget: Budget::of_graph("registration contexts", units),
        stack: Vec::new(),
        lists: Vec::new(),
        cells: Vec::new(),
        found: Vec::new(),
    };
    let value = |l: &Leaf| match l {
        Leaf::Param(0) => V::Object(owner.clone(), true),
        Leaf::Named(n) => V::of(n),
        Leaf::Str(s) => V::Str(s.clone()),
        Leaf::None => V::None,
        Leaf::Param(_) | Leaf::Other => V::Unknown,
    };
    let args = row.args.iter().map(value).collect();
    let kwargs = row
        .kwargs
        .iter()
        .map(|(k, l)| (k.clone(), value(l)))
        .collect();
    let callback = run.call_value(V::of(&row.callee), args, kwargs);
    let called = callback.and_then(|cb| {
        // Only a nested function the factory makes is the registered callback.
        let made = matches!(&cb, V::Fn(f) if f.cells.is_some());
        let sym = |n: &String| V::Sym(n.clone());
        let args = flow.call.args.iter().map(sym).collect();
        let kwargs = flow
            .call
            .kwargs
            .iter()
            .map(|(k, n)| (k.clone(), sym(n)))
            .collect();
        let result = run.call_value(made.then_some(cb).ok_or(Open)?, args, kwargs)?;
        if flow.call.awaited {
            run.await_value(result)
        } else {
            Ok(result)
        }
    });
    if called.is_err() {
        return Answer::Open;
    }
    let mut out = Vec::new();
    for t in run.found {
        for (param, class) in t.params {
            out.push(Context {
                handler: t.handler.clone(),
                param,
                class,
                registered_in: e.key.clone(),
                registration: row.site,
                called_in: flow.reader.clone(),
                called_at: flow.call.site,
                selected_in: t.selected_in.clone(),
                selected_at: t.selected_at,
                invoked_at: t.invoked_at,
                selector: t.selector.clone(),
                equals: t.member.clone(),
                others_open: t.others_open,
            });
        }
    }
    Answer::Known(out)
}

struct Flow<'i> {
    reader: FuncKey,
    call: &'i RowCall,
}

/// The one call of the registered slot: a method of the owner hands the table `registrar`
/// returns to a class whose `__init__` keeps it in an attribute that nothing else of the class
/// writes, and a method of that class calls the slot of the row a helper picks from it.
fn consumer<'i>(
    entries: &'i [Entry<'i>],
    by_fqn: &FxHashMap<String, usize>,
    decls: &ProjectDecls,
    registrar: &FuncKey,
    owner: &str,
    slot: u8,
) -> Option<Flow<'i>> {
    let method = registrar.qualname.rsplit('.').next()?;
    let entry = |k: &FuncKey| by_fqn.get(&k.fqn()).map(|i| &entries[*i]);
    let mut flows = Vec::new();
    let handed = (entries.iter())
        .filter(|s| owner_of(s.key).as_deref() == Some(owner))
        .flat_map(|s| &s.facts.hands)
        .filter(|h| h.method == method);
    for hand in handed {
        let Some(init) = method_key(decls, &hand.class, "__init__") else {
            continue;
        };
        let names = decls
            .function_decl(&init)
            .map(|d| d.param_names.clone())
            .unwrap_or_default();
        let param = match &hand.arg {
            ArgAt::Position(i) => i + 1,
            ArgAt::Keyword(k) => names
                .iter()
                .position(|n| n == k)
                .map_or(u8::MAX, |i| i as u8),
        };
        let kept = entry(&init).into_iter().flat_map(|i| &i.facts.keeps);
        for (attr, _) in kept.filter(|(_, p)| *p == param) {
            let methods = decls.methods_of(&hand.class);
            let writers = (methods.iter())
                .filter_map(&entry)
                .filter(|m| m.local.mutates_self_attrs.iter().any(|a| a.attr == *attr))
                .count();
            if writers != 1 {
                continue;
            }
            for reader in methods.iter().filter_map(&entry) {
                for call in reader.facts.row_calls.iter().filter(|c| c.attr == *attr) {
                    let picked = entry(&call.helper).into_iter().flat_map(|h| &h.facts.picks);
                    let picked = picked.filter(|p| {
                        p.param == call.helper_arg && p.field == slot && p.slot == call.slot
                    });
                    if picked.count() > 0 {
                        flows.push(Flow {
                            reader: reader.key.clone(),
                            call,
                        });
                    }
                }
            }
        }
    }
    match flows.len() {
        1 => flows.pop(),
        _ => None,
    }
}

// ------------------------------------------------------------------------------ interpreter

/// A path the model does not follow.
#[derive(Debug)]
struct Open;

type Res<T> = Result<T, Open>;

const NO_SITE: Span = Span { start: 0, end: 0 };

/// A project function, a nested function with its cells, or a method bound to its receiver.
#[derive(Debug, Clone, PartialEq)]
struct FnV {
    fqn: String,
    /// The nested function's cells, in [`Run::cells`].
    cells: Option<usize>,
    receiver: Option<V>,
}

/// `getattr(<instance of class>, <name>)`: the selector text, and the name when it is constant.
#[derive(Debug, Clone, PartialEq)]
struct LookupV {
    class: String,
    receiver: V,
    selector: String,
    fixed: Option<String>,
    at: Span,
}

/// The coroutine a call of an `async def` returns, with its arguments and the call site.
#[derive(Debug, Clone, PartialEq)]
struct PendingV {
    coroutine: Coroutine,
    args: Vec<V>,
    kwargs: Vec<(String, V)>,
    site: Span,
}

/// What awaiting a coroutine does: run a function, or call what a lookup selects.
#[derive(Debug, Clone, PartialEq)]
enum Coroutine {
    Run(FnV),
    Select(LookupV),
}

#[derive(Debug, Clone, PartialEq)]
enum V {
    None,
    Bool(bool),
    Int(i64),
    Str(String),
    /// A value the model does not follow (a string built at run time, an unknown default).
    Unknown,
    /// A module-level object outside the project's functions and classes, by fqn.
    Named(String),
    Class(String),
    Fn(Box<FnV>),
    /// An instance of a project class; `true` for the registering object.
    Object(String, bool),
    /// A value the call of the row passes, and the attributes and methods read on it.
    Sym(String),
    Tuple(Vec<V>),
    List(usize),
    Dict(Vec<(String, V)>),
    Signature(Vec<String>),
    Lookup(Box<LookupV>),
    Pending(Box<PendingV>),
}

impl V {
    fn of(n: &Named) -> Self {
        match n {
            Named::Function(k) => Self::func(k.fqn()),
            Named::Class(c) => Self::Class(c.clone()),
            Named::Other(o) => Self::Named(o.clone()),
        }
    }

    fn func(fqn: String) -> Self {
        Self::Fn(Box::new(FnV {
            fqn,
            cells: None,
            receiver: None,
        }))
    }

    fn of_lit(l: &Lit) -> Self {
        match l {
            Lit::None => Self::None,
            Lit::Bool(b) => Self::Bool(*b),
            Lit::Int(i) => Self::Int(*i),
            Lit::Str(s) => Self::Str(s.clone()),
            Lit::Other => Self::Unknown,
        }
    }
}

/// A handler a lookup found, with the parameters its call binds to the registering object.
struct Found {
    handler: FuncKey,
    member: String,
    params: Vec<(u8, String)>,
    selector: String,
    others_open: bool,
    selected_in: FuncKey,
    selected_at: Span,
    invoked_at: Span,
}

struct Run<'r, 'i> {
    entries: &'r [Entry<'i>],
    by_fqn: &'r FxHashMap<String, usize>,
    decls: &'r ProjectDecls,
    budget: Budget,
    /// The functions being run, outermost first.
    stack: Vec<String>,
    lists: Vec<Vec<V>>,
    cells: Vec<Vec<(String, V)>>,
    found: Vec<Found>,
}

/// `params` bound to `args` and `kwargs` as a call binds them (defaults included).
fn bind(params: &[Param], args: Vec<V>, mut kwargs: Vec<(String, V)>) -> Res<Vec<V>> {
    let mut args = args.into_iter();
    let mut out = Vec::with_capacity(params.len());
    for p in params {
        let named = kwargs.iter().position(|(k, _)| *k == p.name);
        let by_keyword = p.kind == ParamKind::Positional || p.kind == ParamKind::KeywordOnly;
        let v = match p.kind {
            ParamKind::Star => Some(V::Tuple(args.by_ref().collect())),
            ParamKind::DoubleStar => Some(V::Dict(std::mem::take(&mut kwargs))),
            ParamKind::KeywordOnly => named.map(|i| kwargs.remove(i).1),
            ParamKind::PositionalOnly | ParamKind::Positional => match args.next() {
                // A keyword for a parameter a position already binds raises `TypeError`.
                Some(v) => Some(named.filter(|_| by_keyword).map_or(Ok(v), |_| Err(Open))?),
                None => named.filter(|_| by_keyword).map(|i| kwargs.remove(i).1),
            },
        };
        out.push(
            v.or_else(|| p.default.as_ref().map(V::of_lit))
                .ok_or(Open)?,
        );
    }
    let all_bound = args.next().is_none() && kwargs.is_empty();
    all_bound.then_some(out).ok_or(Open)
}

/// The parameters of the handler `d` the arguments bind to the registering object, when they
/// bind (the handler is followed no further).
fn bind_handler(
    d: &crate::resolver::FunctionDecl,
    args: &[V],
    kwargs: &[(String, V)],
) -> Option<Vec<(u8, String)>> {
    let mut bound: Vec<Option<&V>> = vec![None; d.param_names.len()];
    let positional = usize::from(d.positional_params);
    let mut binds = args.len() <= positional || d.has_varargs;
    for (slot, a) in bound.iter_mut().zip(args).take(positional) {
        *slot = Some(a);
    }
    for (k, v) in kwargs {
        match d.keyword_param(k) {
            Some(i) => binds &= bound[i].replace(v).is_none(),
            None => binds &= d.has_kwargs,
        }
    }
    let required_kw = |i: usize| {
        let at = i.wrapping_sub(positional);
        i >= positional && at < 8 && d.required_keyword_only & (1 << at) != 0
    };
    let missing = (bound.iter().enumerate())
        .any(|(i, v)| v.is_none() && (i < usize::from(d.required_positional) || required_kw(i)));
    let owner = |(i, v): (usize, &Option<&V>)| match v {
        Some(V::Object(class, true)) => Some((i as u8, class.clone())),
        _ => None,
    };
    (binds && !missing).then(|| bound.iter().enumerate().filter_map(owner).collect())
}

fn truthy(v: &V) -> Res<bool> {
    match v {
        V::Bool(b) => Ok(*b),
        V::None => Ok(false),
        _ => Err(Open),
    }
}

/// Whether the module global `fqn` holds a list (what its stores bind).
fn global_list(decls: &ProjectDecls, fqn: &str) -> bool {
    let (module, name) = fqn.rsplit_once('.').unwrap_or_default();
    let ty = decls.module(module).and_then(|m| m.global_types.get(name));
    matches!(ty, Some(crate::types::Ty::Inst(o)) if o.class == "builtins.list")
}

impl<'i> Run<'_, 'i> {
    fn step(&self) -> Res<()> {
        self.budget.take(1).then_some(()).ok_or(Open)
    }

    fn program_of(&self, fqn: &str) -> Res<&'i Program> {
        let i = *self.by_fqn.get(fqn).ok_or(Open)?;
        self.entries[i].facts.program.as_ref().ok_or(Open)
    }

    /// The method of a project class an instance's attribute `name` reads, when nothing but its
    /// definitions decides what the read gives.
    fn method_of(&self, class: &str, name: &str) -> Res<String> {
        let hooked = ["__getattr__", "__getattribute__"]
            .iter()
            .any(|h| method_key(self.decls, class, h).is_some());
        let key = method_key(self.decls, class, name)
            .filter(|_| !hooked)
            .ok_or(Open)?;
        let d = self.decls.function_decl(&key).ok_or(Open)?;
        let plain =
            d.called_as_defined && !d.is_staticmethod && !d.is_classmethod && !d.is_property;
        plain.then(|| key.fqn()).ok_or(Open)
    }

    fn attribute(&self, v: V, attr: &str) -> Res<V> {
        if let V::Object(class, _) = &v {
            let fqn = self.method_of(class, attr)?;
            return Ok(V::Fn(Box::new(FnV {
                fqn,
                cells: None,
                receiver: Some(v),
            })));
        }
        Ok(match v {
            V::Sym(s) => V::Sym(format!("{s}.{attr}")),
            V::Named(m) => V::of(&classify(self.decls, format!("{m}.{attr}"))),
            V::Signature(names) if attr == "parameters" => V::Signature(names),
            // Function metadata (`fn.__name__`): a value the model does not follow.
            V::Fn(f) if f.receiver.is_none() => V::Unknown,
            _ => return Err(Open),
        })
    }

    /// The parameter names `inspect.signature(f)` lists, and whether `f` is an `async def`.
    fn signature_of(&self, f: &V) -> Res<(Vec<String>, bool)> {
        let V::Fn(f) = f else { return Err(Open) };
        let p = self.program_of(&f.fqn)?;
        let skip = usize::from(f.receiver.is_some());
        let names = p.params.iter().skip(skip).map(|p| p.name.clone()).collect();
        Ok((names, p.is_async))
    }

    /// A call of a builtin or library function the model knows.
    fn builtin(&self, name: &str, args: Vec<V>, site: Span) -> Res<V> {
        let lookup = |class: &String, selector, fixed| {
            V::Lookup(Box::new(LookupV {
                class: class.clone(),
                receiver: args[0].clone(),
                selector,
                fixed,
                at: site,
            }))
        };
        let function = |v: &V| matches!(v, V::Fn(f) if f.receiver.is_none());
        Ok(match (name, args.as_slice()) {
            ("builtins.getattr", [V::Object(class, _), V::Sym(s)]) => {
                lookup(class, s.clone(), None)
            }
            ("builtins.getattr", [V::Object(class, _), V::Str(s)]) => {
                lookup(class, format!("{s:?}"), Some(s.clone()))
            }
            ("builtins.isinstance", [v, V::Named(t)])
                if matches!(v, V::Class(_)) || function(v) =>
            {
                let class = matches!(v, V::Class(_));
                match t.as_str() {
                    "builtins.type" => V::Bool(class),
                    "types.FunctionType" => V::Bool(!class),
                    _ => return Err(Open),
                }
            }
            ("inspect.signature", [f]) => V::Signature(self.signature_of(f)?.0),
            ("inspect.iscoroutinefunction", [f]) => V::Bool(self.signature_of(f)?.1),
            _ => return Err(Open),
        })
    }

    fn construct(&self, class: &str, args: &[V], kwargs: &[(String, V)]) -> Res<V> {
        let custom = ["__init__", "__new__"]
            .iter()
            .any(|m| method_key(self.decls, class, m).is_some());
        let plain = args.is_empty()
            && kwargs.is_empty()
            && !custom
            && !self.decls.has_custom_metaclass(class);
        plain
            .then(|| V::Object(class.to_owned(), false))
            .ok_or(Open)
    }

    /// The handlers the lookup may find on an instance of a project class, called with `args`
    /// and awaited: each `async def` method along the MRO (at most [`MAX_CANDIDATES`]) the name
    /// may be and the arguments bind to.
    fn select(
        &mut self,
        l: LookupV,
        site: Span,
        args: Vec<V>,
        kwargs: Vec<(String, V)>,
    ) -> Res<()> {
        let mut seen = FxHashSet::default();
        let mut members = Vec::new();
        for c in self.decls.mro_of(&l.class) {
            if c == "builtins.object" {
                continue;
            }
            if !self.decls.is_project_class(&c) {
                return Err(Open);
            }
            for key in self.decls.methods_of(&c) {
                let name = key
                    .qualname
                    .rsplit('.')
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                if seen.insert(name.clone())
                    && self.method_of(&l.class, &name).ok() == Some(key.fqn())
                {
                    members.push((name, key));
                }
            }
        }
        if members.len() > MAX_CANDIDATES {
            return Err(Open);
        }
        let here = self
            .stack
            .last()
            .and_then(|f| self.by_fqn.get(f))
            .ok_or(Open)?;
        let selected_in = self.entries[*here].key.clone();
        let mut bound_args = vec![l.receiver.clone()];
        bound_args.extend(args);
        for (member, key) in members {
            let d = self.decls.function_decl(&key).ok_or(Open)?;
            if l.fixed.as_ref().is_some_and(|f| *f != member) || !d.is_async {
                continue;
            }
            let Some(params) = bind_handler(d, &bound_args, &kwargs) else {
                continue;
            };
            self.found.push(Found {
                handler: key,
                member,
                params,
                selector: l.selector.clone(),
                others_open: l.fixed.is_none(),
                selected_in: selected_in.clone(),
                selected_at: l.at,
                invoked_at: site,
            });
        }
        Ok(())
    }

    /// Call `callee`: a call of an `async def` gives the coroutine, run when it is awaited.
    fn call_value(&mut self, callee: V, args: Vec<V>, kwargs: Vec<(String, V)>) -> Res<V> {
        self.invoke_value(callee, args, kwargs, NO_SITE)
    }

    fn invoke_value(
        &mut self,
        callee: V,
        args: Vec<V>,
        kwargs: Vec<(String, V)>,
        site: Span,
    ) -> Res<V> {
        self.step()?;
        let pending = |coroutine, args, kwargs| {
            V::Pending(Box::new(PendingV {
                coroutine,
                args,
                kwargs,
                site,
            }))
        };
        match callee {
            V::Fn(f) if self.program_of(&f.fqn)?.is_async => {
                Ok(pending(Coroutine::Run(*f), args, kwargs))
            }
            V::Fn(f) => self.enter_body(*f, args, kwargs),
            V::Lookup(l) => Ok(pending(Coroutine::Select(*l), args, kwargs)),
            V::Class(c) => self.construct(&c, &args, &kwargs),
            V::Named(n) if kwargs.is_empty() => self.builtin(&n, args, site),
            _ => Err(Open),
        }
    }

    /// Await `value`: run the coroutine (a handler looked up is selected, not run).
    fn await_value(&mut self, value: V) -> Res<V> {
        let V::Pending(p) = value else {
            return Err(Open);
        };
        match p.coroutine {
            Coroutine::Select(l) => self
                .select(l, p.site, p.args, p.kwargs)
                .map(|()| V::Unknown),
            Coroutine::Run(f) => self.enter_body(f, p.args, p.kwargs),
        }
    }

    /// Run the body of a project function, a nested function, or a bound method.
    fn enter_body(&mut self, f: FnV, mut args: Vec<V>, kwargs: Vec<(String, V)>) -> Res<V> {
        let p = self.program_of(&f.fqn)?;
        if self.stack.contains(&f.fqn) || self.stack.len() >= MAX_DEPTH {
            self.budget.spend();
            return Err(Open);
        }
        let module_level = f.cells.is_none() && f.receiver.is_none();
        if !module_level && !p.decorators.is_empty() {
            return Err(Open);
        }
        // A decorator the project defines must give the function back: the name is bound to
        // what the innermost one returns, then each next one.
        let itself = V::func(f.fqn.clone());
        for d in &p.decorators {
            let decorator = V::of(d.as_ref().ok_or(Open)?);
            let result = self.invoke_value(decorator, vec![itself.clone()], Vec::new(), NO_SITE)?;
            (result == itself).then_some(()).ok_or(Open)?;
        }
        args.splice(0..0, f.receiver);
        let values = bind(&p.params, args, kwargs)?;
        self.stack.push(f.fqn);
        let out = self.run_body(p, values, f.cells);
        self.stack.pop();
        out
    }

    fn run_body(&mut self, p: &Program, values: Vec<V>, cells: Option<usize>) -> Res<V> {
        let mut vals: FxHashMap<u32, V> = (p.params.iter().map(|p| p.var)).zip(values).collect();
        let mut iters: FxHashMap<u32, (Vec<V>, usize)> = FxHashMap::default();
        let (mut at, mut from) = (0u32, u32::MAX);
        loop {
            self.step()?;
            let b = p.blocks.get(at as usize).ok_or(Open)?;
            let joined: Vec<(u32, V)> = (b.joins.iter())
                .map(|(dst, inputs)| {
                    let input = inputs.iter().find(|(pred, _)| *pred == from).ok_or(Open)?;
                    Ok((*dst, vals.get(&input.1).cloned().ok_or(Open)?))
                })
                .collect::<Res<_>>()?;
            vals.extend(joined);
            for op in &b.ops {
                self.step()?;
                self.exec_op(op, &mut vals, &mut iters, cells)?;
            }
            from = at;
            at = match &b.end {
                End::Jump(t) => *t,
                End::Branch(c, t, e) => {
                    if truthy(vals.get(c).ok_or(Open)?)? {
                        *t
                    } else {
                        *e
                    }
                }
                End::Next {
                    iter,
                    item,
                    body,
                    done,
                } => {
                    let (items, pos) = iters.get_mut(iter).ok_or(Open)?;
                    *pos += 1;
                    match items.get(*pos - 1).cloned() {
                        Some(v) => {
                            vals.insert(*item, v);
                            *body
                        }
                        None => *done,
                    }
                }
                End::Return(v) => {
                    return Ok(v.and_then(|v| vals.get(&v).cloned()).unwrap_or(V::None));
                }
                End::Raise => return Err(Open),
            };
        }
    }

    fn exec_op(
        &mut self,
        op: &Op,
        vals: &mut FxHashMap<u32, V>,
        iters: &mut FxHashMap<u32, (Vec<V>, usize)>,
        cells: Option<usize>,
    ) -> Res<()> {
        let get = |v: &u32| vals.get(v).cloned().ok_or(Open);
        let (dst, value) = match op {
            Op::Const(d, l) => (*d, V::of_lit(l)),
            Op::Global(d, n) => (*d, V::of(n)),
            Op::Cell(d, name) => {
                let cell = cells.and_then(|c| self.cells[c].iter().find(|(n, _)| n == name));
                (*d, cell.map(|(_, v)| v.clone()).ok_or(Open)?)
            }
            Op::Copy(d, s) => (*d, get(s)?),
            Op::Tuple(d, es) => (*d, V::Tuple(es.iter().map(get).collect::<Res<_>>()?)),
            Op::List(d, es) => {
                self.lists.push(es.iter().map(get).collect::<Res<_>>()?);
                (*d, V::List(self.lists.len() - 1))
            }
            Op::Attr(d, o, a) => (*d, self.attribute(get(o)?, a)?),
            // A store on a function object is metadata: what calling it runs stays.
            Op::SetAttr(o, a) => {
                let metadata =
                    matches!(get(o)?, V::Fn(f) if f.receiver.is_none()) && !a.starts_with("__");
                return metadata.then_some(()).ok_or(Open);
            }
            Op::Item(d, o, i) => {
                let item = match (get(o)?, get(i)?) {
                    (V::Dict(es), V::Str(k)) => {
                        es.into_iter().find(|(n, _)| *n == k).map(|(_, v)| v)
                    }
                    _ => None,
                };
                (*d, item.ok_or(Open)?)
            }
            Op::Is(d, l, r, not) => {
                let (l, r) = (get(l)?, get(r)?);
                // Only a test against `None` of a value the model knows is decided.
                let known = |v: &V| !matches!(v, V::Sym(_) | V::Unknown);
                let decided = (l == V::None || r == V::None) && known(&l) && known(&r);
                (*d, V::Bool(decided.then(|| (l == r) != *not).ok_or(Open)?))
            }
            Op::In(d, l, r, not) => {
                let found = match (get(l)?, get(r)?) {
                    (V::Str(k), V::Dict(es)) => Some(es.iter().any(|(n, _)| *n == k)),
                    _ => None,
                };
                (*d, V::Bool(found.ok_or(Open)? != *not))
            }
            Op::Not(d, x) => (*d, V::Bool(!truthy(&get(x)?)?)),
            Op::Closure(d, key, captured, rebound) => {
                // A cell the enclosing function binds again may hold another value when called.
                (!rebound).then_some(()).ok_or(Open)?;
                let cells = (captured.iter())
                    .map(|(n, v)| Ok((n.clone(), get(v)?)))
                    .collect::<Res<_>>()?;
                self.cells.push(cells);
                let f = FnV {
                    fqn: key.fqn(),
                    cells: Some(self.cells.len() - 1),
                    receiver: None,
                };
                (*d, V::Fn(Box::new(f)))
            }
            Op::Call(c) => {
                let mut args = Vec::new();
                for (a, starred) in &c.args {
                    match (get(a)?, starred) {
                        (V::List(l), true) => args.extend(self.lists[l].clone()),
                        (_, true) => return Err(Open),
                        (v, false) => args.push(v),
                    }
                }
                let kwargs = (c.kwargs.iter())
                    .map(|(k, v)| Ok((k.clone().ok_or(Open)?, get(v)?)))
                    .collect::<Res<Vec<_>>>()?;
                let target = get(&c.callee)?;
                let result = match &c.method {
                    None => self.invoke_value(target, args, kwargs, c.site)?,
                    Some(m) => self.method_call(target, m, args, kwargs, c.site)?,
                };
                (c.dst, result)
            }
            Op::Await(d, x) => (*d, self.await_value(get(x)?)?),
            Op::Iter(d, x) => {
                let items = match get(x)? {
                    V::Tuple(t) => t,
                    V::List(l) => self.lists[l].clone(),
                    _ => return Err(Open),
                };
                iters.insert(*d, (items, 0));
                (*d, V::Unknown)
            }
            Op::Text(d) => (*d, V::Unknown),
            Op::Other => return Err(Open),
        };
        vals.insert(dst, value);
        Ok(())
    }

    /// `recv.<m>(...)`: the methods of the builtin values the model follows, or a call of the
    /// attribute.
    fn method_call(
        &mut self,
        recv: V,
        m: &str,
        args: Vec<V>,
        kwargs: Vec<(String, V)>,
        site: Span,
    ) -> Res<V> {
        let plain = kwargs.is_empty();
        Ok(match (recv, m, args.as_slice()) {
            (V::Sym(s), "lower" | "upper", []) if plain => V::Sym(format!("{s}.{m}()")),
            (V::List(l), "append", [x]) if plain => {
                self.lists[l].push(x.clone());
                V::None
            }
            // A list the module keeps: nothing the model follows reads it.
            (V::Named(g), "append", [_]) if plain && global_list(self.decls, &g) => V::None,
            (V::Signature(names), "keys", []) if plain => {
                self.lists.push(names.into_iter().map(V::Str).collect());
                V::List(self.lists.len() - 1)
            }
            (recv, m, _) => {
                let callee = self.attribute(recv, m)?;
                self.invoke_value(callee, args, kwargs, site)?
            }
        })
    }
}
