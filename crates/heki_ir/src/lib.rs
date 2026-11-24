//! Shared SSA-CFG Intermediate Representation for both `heki_soundness` and `heki_effect`.
//!
//! Key invariants (see `docs/ANALYSIS_AND_KB_SPEC.md` Section 1):
//! - Left-to-right explicit evaluation order and explicit `exc_target` exception edges.
//! - `for` / `async for` headers (`for.iter` + `MakeIter`) are lowered OUTSIDE the loop body
//!   (depth `d`), while `IterNext` and body statements live at depth `d + 1`.
//! - Nested classes have distinct `owner_class: Option<ClassId>` (never merged into outer class).
//! - Every instruction preserves its original AST `Span` for minimal `TextEdit` generation.
//! - `A1Hazard` sites are recorded per function so `heki_soundness` can skip only that function
//!   while `heki_effect` continues normally.

mod builder;
mod dominators;
mod facts;
mod special_methods;
pub mod walk;

pub use builder::build_module_ir;
pub use dominators::{Dominated, DominatorTree, immediate_dominators};
pub use facts::{
    Carries, Decided, FnIndex, GlobalStores, ItemOrigin, ItemOrigins, ItemSource, LEN_CAP_MAX,
    LastBindings, ModuleBindings, ModuleConstants, Must, PathReader, Paths, Progress, SMALL_MAX,
    SmallDisplay, Trips, comprehension_id, is_builtin_name,
};
pub use special_methods::SpecialMethods;

use heki_ast::Span;
use heki_semantic::{ClassId, ScopeId};
use rustc_hash::FxHashSet;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FunctionId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BlockId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct VarId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionKind {
    Sync,
    Async,
    Generator,
    AsyncGenerator,
}

/// Loop bound classification used ONLY by Profitability and Advisory noise suppression
/// (NEVER used as a Soundness premise).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopBound {
    /// The loop runs at most this many times whatever the data: its iterable is fixed by the
    /// code (a display of at most [`SMALL_MAX`] items, a name or module constant bound to one
    /// that nothing grows, `range(...)` of constants, or `enumerate` / `zip` / `reversed` /
    /// `sorted` / `.items()` ... of one).
    ConstSmall(u32),
    RetryPattern,
    DaemonPolling,
    Unbounded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NaturalLoop {
    pub header: BlockId,
    pub preheader: Option<BlockId>,
    pub body: Vec<BlockId>,
    pub exits: Vec<BlockId>,
    pub back_edges: Vec<BlockId>,
    pub bound: LoopBound,
    pub depth: u16,
    /// Values the loop's own test caps: `while len(acc) < N` (also as one `and` operand) with
    /// a constant `N` of at most [`LEN_CAP_MAX`] (Profitability only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub len_caps: Vec<LenCap>,
}

/// `len(var)` is below `limit` whenever the loop's body starts an iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LenCap {
    /// The SSA value the loop test measures (the header phi of a loop-carried accumulator).
    pub var: VarId,
    pub limit: u32,
}

/// Loop-carried SSA `PhiNode` metadata (used by `DATA101` and `OPT101`–`OPT103`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopCarriedInfo {
    pub loop_header: BlockId,
    pub phi_var: VarId,
    pub var_name: Option<String>,
    pub init_vars: Vec<VarId>,
    pub back_vars: Vec<VarId>,
    /// True when some back-edge value is computed from `phi_var` inside the loop (`x = x +
    /// [e]`, `d = {**d, k: v}`, `df = pd.concat([df, chunk])`), i.e. `updates` is not empty.
    /// A path that only passes the previous value on (a branch leaving the variable unchanged)
    /// is not an update.
    pub self_accumulating: bool,
    /// The instruction results that reach the back edge through copies and phis only and are
    /// computed from `phi_var` (the accumulating updates), in block order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub updates: Vec<VarId>,
}

/// The `A1Hazard` kind of a function whose IR would pass its size budget: its body is one call
/// of an unknown callable with every parameter, so nothing in it is modeled.
pub const UNMODELED: &str = "unmodeled";

/// A1 assumption hazard (disables `heki_soundness` on this function only; `heki_effect` continues).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct A1Hazard {
    pub kind: String,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParamInfo {
    pub name: String,
    pub var: VarId,
    pub has_default: bool,
    /// The default when it is a literal (`None`, `False`, `0`, `""`, ...): what the callee
    /// binds when the caller passes nothing. The default's value, evaluated where the function
    /// is defined, is an operand of the `MakeClosure` that builds the function object.
    pub default: Option<ConstLiteral>,
    /// The default is a list, dict, or set display: one container, built where the function is
    /// defined, that every call binding the default shares (a cache kept across calls).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub container_default: bool,
    /// Keyword-only (after `*` or `*args`): never bound by position.
    pub kw_only: bool,
    /// Positional-only (before `/`): never bound by keyword.
    pub pos_only: bool,
    pub is_vararg: bool,
    pub is_kwarg: bool,
    /// PEP 484 annotation text (Advisory lane ONLY; `heki_soundness` MUST ignore this).
    pub annotation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhiNode {
    pub dst: VarId,
    pub incoming: Vec<(BlockId, VarId)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinKind {
    List,
    Dict,
    Set,
    Tuple,
    Str,
    Bytes,
    Int,
    Float,
    Bool,
}

impl BuiltinKind {
    /// Every kind, in declaration order.
    pub const ALL: [Self; 9] = [
        Self::List,
        Self::Dict,
        Self::Set,
        Self::Tuple,
        Self::Str,
        Self::Bytes,
        Self::Int,
        Self::Float,
        Self::Bool,
    ];

    /// The builtin type's name (`list`, `dict`, ...).
    pub fn name(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Dict => "dict",
            Self::Set => "set",
            Self::Tuple => "tuple",
            Self::Str => "str",
            Self::Bytes => "bytes",
            Self::Int => "int",
            Self::Float => "float",
            Self::Bool => "bool",
        }
    }

    /// The kind a builtin type's name denotes (the inverse of [`Self::name`]).
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstLiteral {
    None,
    Bool(bool),
    Int(i64),
    Float(String),
    Str(String),
    Bytes,
    Ellipsis,
    Other,
}

impl ConstLiteral {
    /// The builtin type of the literal's value (`None` for `None`, `...`, and a literal heki
    /// does not model).
    pub fn builtin_kind(&self) -> Option<BuiltinKind> {
        match self {
            Self::Int(_) => Some(BuiltinKind::Int),
            Self::Float(_) => Some(BuiltinKind::Float),
            Self::Str(_) => Some(BuiltinKind::Str),
            Self::Bytes => Some(BuiltinKind::Bytes),
            Self::Bool(_) => Some(BuiltinKind::Bool),
            Self::None | Self::Ellipsis | Self::Other => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BinOpKind {
    Add,
    Sub,
    Mult,
    Div,
    FloorDiv,
    Mod,
    Pow,
    BitOr,
    BitAnd,
    BitXor,
    LShift,
    RShift,
    MatMult,
}

impl BinOpKind {
    /// The method an augmented assignment with this operator calls (`x += y` calls
    /// `x.__iadd__(y)`).
    pub fn in_place_method(self) -> &'static str {
        const METHODS: [&str; 13] = [
            "__iadd__",
            "__isub__",
            "__imul__",
            "__itruediv__",
            "__ifloordiv__",
            "__imod__",
            "__ipow__",
            "__ior__",
            "__iand__",
            "__ixor__",
            "__ilshift__",
            "__irshift__",
            "__imatmul__",
        ];
        METHODS[self as usize]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnaryOpKind {
    Not,
    Invert,
    UAdd,
    USub,
}

/// The conversion of a replacement field: `!s` calls `str()`, `!r` `repr()`, `!a` `ascii()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Conversion {
    Str,
    Repr,
    Ascii,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompareOpKind {
    Eq,
    NotEq,
    Lt,
    LtE,
    Gt,
    GtE,
    Is,
    IsNot,
    In,
    NotIn,
}

/// Ordered dictionary literal item, preserving `{**d, k: v}` vs `{**other, **d}` order for `OPT102`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DictEntry {
    KeyValue { key: VarId, value: VarId },
    Unpack { dict: VarId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstKind {
    Const {
        dst: VarId,
        literal: ConstLiteral,
    },
    /// Literal or built-in container allocation (`list`/`set`/`tuple`/`dict` empty/etc.).
    /// `elems` tracks stored child values (which become `Shared` in `heki_soundness`).
    AllocBuiltin {
        dst: VarId,
        kind: BuiltinKind,
        elems: Vec<VarId>,
    },
    /// Ordered dict literal preserving unpack positions (`{**d, ...}` vs `{**other, **d}`).
    DictLiteral {
        dst: VarId,
        entries: Vec<DictEntry>,
    },
    LoadGlobal {
        dst: VarId,
        name: String,
        is_lazy_import: bool,
    },
    LoadDeref {
        dst: VarId,
        name: String,
    },
    LoadName {
        dst: VarId,
        name: String,
    },
    StoreGlobal {
        name: String,
        value: VarId,
    },
    StoreDeref {
        name: String,
        value: VarId,
    },
    Copy {
        dst: VarId,
        src: VarId,
    },
    /// `del name`; the name is possibly unbound afterwards (a `LoadName` defines it).
    Delete {
        var: Option<VarId>,
        name: String,
        /// The name is a module global (`del` in the module body or after `global name`).
        #[serde(default)]
        global: bool,
        /// The name is an enclosing function's variable (`del` after `nonlocal name`).
        #[serde(default)]
        nonlocal: bool,
    },
    BinOp {
        dst: VarId,
        op: BinOpKind,
        lhs: VarId,
        rhs: VarId,
        /// True when originating from `AugAssign` (`+=`, `|=`, etc. -> `__iadd__` / `__ior__`).
        in_place: bool,
    },
    UnaryOp {
        dst: VarId,
        op: UnaryOpKind,
        operand: VarId,
    },
    Compare {
        dst: VarId,
        op: CompareOpKind,
        lhs: VarId,
        rhs: VarId,
    },
    /// The conversion of a replacement field (`!s`, `!r`, `!a`): `str(value)`, `repr(value)`,
    /// or `ascii(value)`, which run the value's `__str__` / `__repr__`, code of its class unless
    /// that is a builtin. The result is a `str`, of a subclass if `__str__` / `__repr__` returns
    /// one.
    Convert {
        dst: VarId,
        value: VarId,
        conversion: Conversion,
    },
    /// A replacement field of an f-string or of a format specification, after its conversion:
    /// `format(value, spec)`, with `""` for a missing `spec`, which runs the value's `__format__`,
    /// code of its class unless that is a builtin. The result is a `str`, of a subclass if
    /// `__format__` returns one.
    Format {
        dst: VarId,
        value: VarId,
        spec: Option<VarId>,
    },
    /// An f-string other than one replacement field alone: a new `str` of its literal text and
    /// its formatted fields (`parts`, each a `Format`'s result), in order. It copies the text of
    /// its parts and keeps none of them.
    BuildString {
        dst: VarId,
        parts: Vec<VarId>,
    },
    AttrLoad {
        dst: VarId,
        obj: VarId,
        attr: String,
    },
    AttrStore {
        obj: VarId,
        attr: String,
        value: VarId,
    },
    AttrDelete {
        obj: VarId,
        attr: String,
    },
    SubscriptLoad {
        dst: VarId,
        obj: VarId,
        index: VarId,
        key_const: Option<String>,
        is_slice: bool,
        slice_lower_const: Option<i64>,
        slice_upper_const: Option<i64>,
    },
    SubscriptStore {
        obj: VarId,
        index: VarId,
        key_const: Option<String>,
        value: VarId,
    },
    SubscriptDelete {
        obj: VarId,
        index: VarId,
        key_const: Option<String>,
    },
    MakeIter {
        dst: VarId,
        iterable: VarId,
        is_async: bool,
    },
    IterNext {
        dst: VarId,
        iter: VarId,
        is_async: bool,
    },
    /// Generator expression construction: `outer_iter` is evaluated immediately and retained
    /// as a derived reference (`derived_refs` in `heki_soundness`).
    MakeGenExpr {
        dst: VarId,
        func_id: Option<FunctionId>,
        outer_iter: VarId,
        captured: Vec<VarId>,
    },
    /// A function, lambda, or class object, or a lazily evaluated scope without a lowered
    /// body (`func_id: None`: a `type` alias). `captured` is every value the object keeps or
    /// hands to code that runs later: its closure cells, defaults (the literal ones are also
    /// the parameters' `ParamInfo::default`), eagerly evaluated annotations, and a class's
    /// bases and keywords.
    MakeClosure {
        dst: VarId,
        func_id: Option<FunctionId>,
        captured: Vec<VarId>,
    },
    Call {
        dst: VarId,
        callee_var: Option<VarId>,
        callee: String,
        args: Vec<VarId>,
        kwargs: Vec<(Option<String>, VarId)>,
        is_awaited: bool,
        /// Index of the first positional argument unpacked with `*` (`f(a, *rest)`): from it on,
        /// the parameter an argument binds is not known. `**` unpacking is a `None` keyword.
        star_from: Option<u8>,
    },
    MethodCall {
        dst: VarId,
        recv: VarId,
        method: String,
        args: Vec<VarId>,
        kwargs: Vec<(Option<String>, VarId)>,
        is_awaited: bool,
        /// As for `Call`.
        star_from: Option<u8>,
    },
    Await {
        dst: VarId,
        value: VarId,
    },
    Yield {
        dst: VarId,
        value: Option<VarId>,
    },
    YieldFrom {
        dst: VarId,
        value: VarId,
    },
    EnterWith {
        dst: VarId,
        mgr: VarId,
        is_async: bool,
    },
    ExitWith {
        mgr: VarId,
        is_async: bool,
    },
    CatchException {
        dst: Option<VarId>,
        exc_types: Vec<String>,
    },
    Import {
        dst: VarId,
        module: Option<String>,
        name: String,
        is_lazy: bool,
        /// A relative import (`from .typing import X`, `from . import json`): `module` and
        /// `name` are named from the importing package, not from the import path.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        relative: bool,
    },
}

impl InstKind {
    /// Return the SSA variable defined by this instruction, if any.
    pub fn dst(&self) -> Option<VarId> {
        match self {
            Self::Const { dst, .. }
            | Self::AllocBuiltin { dst, .. }
            | Self::DictLiteral { dst, .. }
            | Self::LoadGlobal { dst, .. }
            | Self::LoadDeref { dst, .. }
            | Self::LoadName { dst, .. }
            | Self::Copy { dst, .. }
            | Self::BinOp { dst, .. }
            | Self::UnaryOp { dst, .. }
            | Self::Compare { dst, .. }
            | Self::Convert { dst, .. }
            | Self::Format { dst, .. }
            | Self::BuildString { dst, .. }
            | Self::AttrLoad { dst, .. }
            | Self::SubscriptLoad { dst, .. }
            | Self::MakeIter { dst, .. }
            | Self::IterNext { dst, .. }
            | Self::MakeGenExpr { dst, .. }
            | Self::MakeClosure { dst, .. }
            | Self::Call { dst, .. }
            | Self::MethodCall { dst, .. }
            | Self::Await { dst, .. }
            | Self::Yield { dst, .. }
            | Self::YieldFrom { dst, .. }
            | Self::EnterWith { dst, .. }
            | Self::Import { dst, .. } => Some(*dst),
            Self::CatchException { dst, .. } => *dst,
            Self::StoreGlobal { .. }
            | Self::StoreDeref { .. }
            | Self::Delete { .. }
            | Self::AttrStore { .. }
            | Self::AttrDelete { .. }
            | Self::SubscriptStore { .. }
            | Self::SubscriptDelete { .. }
            | Self::ExitWith { .. } => None,
        }
    }

    /// Whether the instruction may raise an exception: everything but constants, copies, and
    /// stores of names (a class statement's `MakeClosure` runs the metaclass and
    /// `__init_subclass__`). A block has its `exc_target` edge only from such an instruction
    /// or a `raise`, and a name is never rebound after one in the same block, so a handler
    /// sees the bindings of the point that raised.
    pub fn may_raise(&self) -> bool {
        !matches!(
            self,
            Self::Const { .. }
                | Self::Copy { .. }
                | Self::StoreGlobal { .. }
                | Self::StoreDeref { .. }
                | Self::CatchException { .. }
        )
    }

    /// Whether the instruction suspends a coroutine until what it awaits completes: an `await`,
    /// and the implicit awaits of `async for` (`__anext__`) and `async with` (`__aenter__`,
    /// `__aexit__`). Other tasks run meanwhile.
    pub fn suspends(&self) -> bool {
        matches!(
            self,
            Self::Await { .. }
                | Self::IterNext { is_async: true, .. }
                | Self::EnterWith { is_async: true, .. }
                | Self::ExitWith { is_async: true, .. }
        )
    }

    /// Collect all SSA operand `VarId`s read by this instruction.
    pub fn operands(&self) -> Vec<VarId> {
        match self {
            Self::Const { .. }
            | Self::LoadGlobal { .. }
            | Self::LoadDeref { .. }
            | Self::LoadName { .. }
            | Self::CatchException { .. }
            | Self::Import { .. } => Vec::new(),
            Self::AllocBuiltin { elems, .. } => elems.clone(),
            Self::BuildString { parts, .. } => parts.clone(),
            Self::Format { value, spec, .. } => std::iter::once(*value).chain(*spec).collect(),
            Self::DictLiteral { entries, .. } => {
                let mut out = Vec::new();
                for e in entries {
                    match e {
                        DictEntry::KeyValue { key, value } => {
                            out.push(*key);
                            out.push(*value);
                        }
                        DictEntry::Unpack { dict } => out.push(*dict),
                    }
                }
                out
            }
            Self::StoreGlobal { value, .. }
            | Self::StoreDeref { value, .. }
            | Self::Copy { src: value, .. }
            | Self::UnaryOp { operand: value, .. }
            | Self::Convert { value, .. }
            | Self::AttrLoad { obj: value, .. }
            | Self::AttrDelete { obj: value, .. }
            | Self::MakeIter {
                iterable: value, ..
            }
            | Self::IterNext { iter: value, .. }
            | Self::Await { value, .. }
            | Self::YieldFrom { value, .. }
            | Self::EnterWith { mgr: value, .. }
            | Self::ExitWith { mgr: value, .. } => vec![*value],
            Self::Delete { var, .. } => var.iter().copied().collect(),
            Self::Yield { value, .. } => value.iter().copied().collect(),
            Self::BinOp { lhs, rhs, .. } | Self::Compare { lhs, rhs, .. } => vec![*lhs, *rhs],
            Self::AttrStore { obj, value, .. } => vec![*obj, *value],
            Self::SubscriptLoad { obj, index, .. } | Self::SubscriptDelete { obj, index, .. } => {
                vec![*obj, *index]
            }
            Self::SubscriptStore {
                obj, index, value, ..
            } => vec![*obj, *index, *value],
            Self::MakeGenExpr {
                outer_iter,
                captured,
                ..
            } => {
                let mut out = Vec::with_capacity(1 + captured.len());
                out.push(*outer_iter);
                out.extend_from_slice(captured);
                out
            }
            Self::MakeClosure { captured, .. } => captured.clone(),
            Self::Call {
                callee_var,
                args,
                kwargs,
                ..
            } => {
                let mut out = Vec::new();
                if let Some(cv) = callee_var {
                    out.push(*cv);
                }
                out.extend_from_slice(args);
                for (_, v) in kwargs {
                    out.push(*v);
                }
                out
            }
            Self::MethodCall {
                recv, args, kwargs, ..
            } => {
                let mut out = Vec::with_capacity(1 + args.len() + kwargs.len());
                out.push(*recv);
                out.extend_from_slice(args);
                for (_, v) in kwargs {
                    out.push(*v);
                }
                out
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inst {
    pub kind: InstKind,
    pub span: Span,
}

/// Name of the copy a starred element of a list, tuple, or set display (`*xs` in `[a, *xs]`)
/// is lowered to: the display holds the elements of `xs`, not `xs` itself
/// ([`IrFunction::is_unpacked`]).
pub const UNPACKED: &str = ".unpacked";

/// The name an inlined comprehension (PEP 709) gives its iteration variable `name` in the
/// enclosing frame, apart from the frame's own `name`: when a scope nested in the comprehension
/// reads it, the frame keeps it as a cell ([`IrFunction::captured_vars`]) and the values bound
/// to it carry the name ([`IrFunction::var_names`]).
pub(crate) fn comprehension_target(comprehension: u32, name: &str) -> String {
    format!(".comp{comprehension}_{name}")
}

/// The source name of an inlined comprehension's iteration variable named by
/// [`comprehension_target`] (`x` of `.comp3_x`); `None` for any other name.
pub fn comprehension_target_source(name: &str) -> Option<&str> {
    let (comprehension, source) = name.strip_prefix(".comp")?.split_once('_')?;
    (!comprehension.is_empty() && comprehension.bytes().all(|b| b.is_ascii_digit()))
        .then_some(source)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Terminator {
    Jump(BlockId),
    Branch {
        cond: VarId,
        then_bb: BlockId,
        else_bb: BlockId,
    },
    Return(Option<VarId>),
    Raise(Option<VarId>),
    Unreachable,
}

impl Terminator {
    /// Normal-flow successors (`exc_target` edges are separate). The builder always gives a
    /// `Branch` two distinct targets.
    pub fn successors(&self) -> Vec<BlockId> {
        match self {
            Self::Jump(t) => vec![*t],
            Self::Branch {
                then_bb, else_bb, ..
            } => vec![*then_bb, *else_bb],
            Self::Return(_) | Self::Raise(_) | Self::Unreachable => Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BasicBlock {
    pub id: BlockId,
    pub phis: Vec<PhiNode>,
    pub insts: Vec<Inst>,
    pub terminator: Terminator,
    pub exc_target: Option<BlockId>,
    /// Source span of a `Branch` condition or a `return` / `raise` statement, for blocks whose
    /// terminator uses a value (a block without instructions has no other position).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term_span: Option<Span>,
}

/// Type annotation recorded for local/attribute assignments (Advisory lane ONLY).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalAnnotation {
    pub target: String,
    pub annotation: String,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrFunction {
    pub id: FunctionId,
    pub name: String,
    pub qualname: String,
    pub scope_id: Option<ScopeId>,
    pub owner_class: Option<ClassId>,
    pub owner_class_qualname: Option<String>,
    pub kind: FunctionKind,
    pub params: Vec<ParamInfo>,
    /// PEP 484 return annotation (Advisory lane ONLY; `heki_soundness` MUST ignore this).
    pub return_annotation: Option<String>,
    pub decorators: Vec<String>,
    /// PEP 484 local / `self.attr` annotations (Advisory lane ONLY).
    pub local_annotations: Vec<LocalAnnotation>,
    /// Source variable name indexed by `VarId.0` (`None` for temporaries).
    pub var_names: Vec<Option<String>>,
    /// Free variables captured by nested functions, lambdas, or generator expressions.
    pub captured_vars: Vec<String>,
    /// Operands written with `*` (iterable unpacking): the `dst` of the display or call and
    /// the operand's index in its `elems` / `args` (`[*xs]` is not a one-element list, and
    /// `f(*xs)` passes the elements of `xs`, not `xs`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub starred: Vec<(VarId, u32)>,
    /// The text each f-string starts with (`heki_ast::fstring_head`): the `dst` of the string
    /// it builds and `ALTER TABLE ` of `f"ALTER TABLE {name}"`, in the order of the variables.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub str_heads: Vec<(VarId, String)>,
    pub blocks: Vec<BasicBlock>,
    pub loops: Vec<NaturalLoop>,
    pub loop_carried: Vec<LoopCarriedInfo>,
    pub a1_hazards: Vec<A1Hazard>,
    pub span: Span,
    pub is_module_body: bool,
    pub is_class_body: bool,
    /// A generator expression, or a comprehension before 3.12, receives its outermost iterable
    /// as `.0` from the function creating it: that function and the iterable there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passed_from: Option<(FunctionId, VarId)>,
    /// The scope whose code makes this one: the function, class body, or module body holding
    /// its `def`, `class`, `lambda`, or comprehension (`None` for the module body). The
    /// functions around a scope are not always those its qualname names: a definition under a
    /// `global` declaration of its name is named as a module-level one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enclosing: Option<FunctionId>,
}

impl IrFunction {
    pub fn block(&self, id: BlockId) -> &BasicBlock {
        &self.blocks[id.0 as usize]
    }

    pub fn var_name(&self, var: VarId) -> Option<&str> {
        self.var_names
            .get(var.0 as usize)
            .and_then(|o| o.as_deref())
    }

    /// Whether `var` is a starred display element (the copy named [`UNPACKED`]): the display
    /// unpacks the iterable it copies instead of holding it.
    pub fn is_unpacked(&self, var: VarId) -> bool {
        self.var_name(var) == Some(UNPACKED)
    }

    /// Loop nesting depth of `id` (`0` if outside all loops).
    pub fn block_loop_depth(&self, id: BlockId) -> u16 {
        self.loops
            .iter()
            .filter(|lp| lp.body.contains(&id))
            .map(|lp| lp.depth)
            .max()
            .unwrap_or(0)
    }

    /// True if `id` executes inside at least one natural loop body.
    pub fn is_in_loop(&self, id: BlockId) -> bool {
        self.block_loop_depth(id) > 0
    }

    /// Return the innermost `NaturalLoop` containing `id`, if any.
    pub fn innermost_loop(&self, id: BlockId) -> Option<&NaturalLoop> {
        self.loops
            .iter()
            .filter(|lp| lp.body.contains(&id))
            .max_by_key(|lp| lp.depth)
    }

    /// Look up the defining instruction of an SSA variable, if defined by an instruction (not a param/phi).
    pub fn def_inst(&self, var: VarId) -> Option<(BlockId, &Inst)> {
        for bb in &self.blocks {
            for inst in &bb.insts {
                if inst.kind.dst() == Some(var) {
                    return Some((bb.id, inst));
                }
            }
        }
        None
    }

    pub fn const_str(&self, var: VarId) -> Option<&str> {
        match &self.def_inst(var)?.1.kind {
            InstKind::Const {
                literal: ConstLiteral::Str(s),
                ..
            } => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn const_int(&self, var: VarId) -> Option<i64> {
        match &self.def_inst(var)?.1.kind {
            InstKind::Const {
                literal: ConstLiteral::Int(n),
                ..
            } => Some(*n),
            _ => None,
        }
    }

    pub fn const_bool(&self, var: VarId) -> Option<bool> {
        match &self.def_inst(var)?.1.kind {
            InstKind::Const {
                literal: ConstLiteral::Bool(b),
                ..
            } => Some(*b),
            _ => None,
        }
    }

    /// Whether `var` is the middle operand of a chained comparison (`a < var < c`): two
    /// comparisons read the one evaluation of its expression, so its source text is an operand
    /// of both.
    pub fn is_shared_chain_operand(&self, var: VarId) -> bool {
        let links: Vec<(Span, VarId, VarId)> = self
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|i| match i.kind {
                InstKind::Compare { lhs, rhs, .. } if lhs == var || rhs == var => {
                    Some((i.span, lhs, rhs))
                }
                _ => None,
            })
            .collect();
        links.iter().any(|&(left, _, rhs)| {
            rhs == var
                && links.iter().any(|&(right, lhs, _)| {
                    lhs == var && right.start < left.end && left.start < right.end
                })
        })
    }

    /// Whether operand `index` of the display or call defining `dst` is written with `*`.
    pub fn is_starred(&self, dst: VarId, index: usize) -> bool {
        self.starred
            .iter()
            .any(|&(d, i)| d == dst && i as usize == index)
    }

    /// The text the f-string `var` holds starts with ([`Self::str_heads`]).
    pub fn str_head(&self, var: VarId) -> Option<&str> {
        let i = self
            .str_heads
            .binary_search_by_key(&var, |(v, _)| *v)
            .ok()?;
        Some(self.str_heads[i].1.as_str())
    }

    /// Whether the function's body is left unmodeled ([`UNMODELED`]): the scopes it creates are
    /// lowered, but nothing in the body creates them.
    pub fn is_unmodeled(&self) -> bool {
        self.a1_hazards.iter().any(|h| h.kind == UNMODELED)
    }

    /// Check whether any decorator ends with one of the provided names (e.g. `cached_property`).
    pub fn has_decorator(&self, names: &[&str]) -> bool {
        self.decorators.iter().any(|d| {
            let base = d.split('(').next().unwrap_or(d).trim();
            names
                .iter()
                .any(|n| base == *n || base.ends_with(&format!(".{n}")))
        })
    }

    /// Compute predecessors for every block (including `exc_target` edges).
    pub fn predecessors(&self) -> Vec<Vec<BlockId>> {
        let mut preds = vec![Vec::new(); self.blocks.len()];
        for bb in &self.blocks {
            for succ in bb.terminator.successors() {
                if !preds[succ.0 as usize].contains(&bb.id) {
                    preds[succ.0 as usize].push(bb.id);
                }
            }
            if let Some(exc) = bb.exc_target
                && !preds[exc.0 as usize].contains(&bb.id)
            {
                preds[exc.0 as usize].push(bb.id);
            }
        }
        preds
    }

    /// Reverse Postorder (RPO) over reachable blocks starting from `BlockId(0)`.
    /// Consumed by `heki_soundness`'s RPO Worklist abstract interpreter.
    pub fn rpo_blocks(&self) -> Vec<BlockId> {
        let mut visited = FxHashSet::default();
        let mut postorder = Vec::with_capacity(self.blocks.len());
        // Start from the entry block (every built function has one).
        let mut stack: Vec<(BlockId, bool)> = self
            .blocks
            .first()
            .map(|entry| (entry.id, false))
            .into_iter()
            .collect();

        while let Some((bb_id, expanded)) = stack.pop() {
            if expanded {
                postorder.push(bb_id);
                continue;
            }
            if !visited.insert(bb_id) {
                continue;
            }
            stack.push((bb_id, true));
            let bb = &self.blocks[bb_id.0 as usize];
            if let Some(exc) = bb.exc_target
                && !visited.contains(&exc)
            {
                stack.push((exc, false));
            }
            let succs = bb.terminator.successors();
            for succ in succs.into_iter().rev() {
                if !visited.contains(&succ) {
                    stack.push((succ, false));
                }
            }
        }
        postorder.reverse();
        postorder
    }
}

/// A value a class statement passes as a base ([`ModuleIr::class_base_values`]).
#[derive(Debug, Clone, Copy)]
pub struct ClassBase<'a> {
    /// The block of the statement.
    pub block: BlockId,
    /// The statement's `MakeClosure`.
    pub statement: &'a Inst,
    /// The class object the statement makes (the `dst` of its `MakeClosure`).
    pub class: VarId,
    pub value: VarId,
}

/// Complete IR for one Python source module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleIr {
    pub functions: Vec<IrFunction>,
    pub module_fn: FunctionId,
    pub has_future_annotations: bool,
    /// The module contains `from m import *`: any module-level name (including a builtin name)
    /// may be rebound by it (`heki_soundness` builtin-shadowing protection, Assumption A1(b)).
    #[serde(default)]
    pub has_star_import: bool,
    /// Module-level names the code fixes (integer constants and small displays), for the
    /// size facts of [`FnIndex`] and the declarations other modules read.
    #[serde(default)]
    pub constants: ModuleConstants,
    /// The bases of the class statements, by function (sorted): the function, the `dst` of a
    /// class's `MakeClosure` and the index among its operands (its `captured`) of a value the
    /// statement passes as a base, in the order of the bases. A starred base (`*bases`) passes
    /// the items of its value and is not listed. Kept by module, not by function: few functions
    /// hold a class statement.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub class_bases: Vec<(FunctionId, VarId, u32)>,
    /// The values a statement uses that no instruction reads, by function (sorted), with the
    /// span of the expression giving each: a decorator, which the statement applies to what it
    /// decorates; a default of a function or lambda and a keyword of a class statement, which
    /// the object made keeps (`MakeClosure` holds them among its captures and annotations
    /// without telling them apart); the class of a class pattern and the types of an `except`
    /// clause, which the statement tests a value against; and the cause of `raise ... from`.
    /// Kept by module, as the class bases are: few functions hold such a statement.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub statement_uses: Vec<(FunctionId, VarId, Span)>,
    /// The annotations a function evaluates when it runs, by function and position (sorted):
    /// the span of each annotation expression of a definition it makes, and of each annotated
    /// name of a module or class body, evaluated eagerly (before 3.14, without `from __future__
    /// import annotations`). The instructions inside one evaluate an annotation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub annotations: Vec<(FunctionId, Span)>,
}

impl ModuleIr {
    pub fn function(&self, id: FunctionId) -> &IrFunction {
        &self.functions[id.0 as usize]
    }

    /// The functions around `f`, nearest first ([`IrFunction::enclosing`]): those whose
    /// variables the scopes nested in them may capture, so neither class bodies nor the module
    /// body.
    pub fn enclosing_functions<'m>(
        &'m self,
        f: &'m IrFunction,
    ) -> impl Iterator<Item = &'m IrFunction> + 'm {
        std::iter::successors(Some(f), |g| Some(self.function(g.enclosing?)))
            .skip(1)
            .filter(|g| !g.is_class_body && !g.is_module_body)
    }

    /// The values the class statements of `shape`'s function pass as bases
    /// ([`Self::class_bases`]), statement by statement and in the order of each one's bases.
    pub fn class_base_values<'s, 'a: 's>(
        &'s self,
        shape: &'s walk::FnShape<'a>,
    ) -> impl Iterator<Item = ClassBase<'a>> + 's {
        let id = shape.f.id;
        let start = self.class_bases.partition_point(|b| b.0 < id);
        self.class_bases[start..]
            .iter()
            .take_while(move |b| b.0 == id)
            .filter_map(|&(_, class, index)| {
                let (block, statement) = shape.defining(class)?;
                Some(ClassBase {
                    block,
                    statement,
                    class,
                    value: *statement.kind.operands().get(index as usize)?,
                })
            })
    }

    /// The statement uses of function `id` ([`Self::statement_uses`]).
    pub fn statement_uses_of(&self, id: FunctionId) -> impl Iterator<Item = (VarId, Span)> + '_ {
        let start = self.statement_uses.partition_point(|u| u.0 < id);
        self.statement_uses[start..]
            .iter()
            .take_while(move |u| u.0 == id)
            .map(|&(_, value, span)| (value, span))
    }

    /// The spans of the annotations function `id` evaluates ([`Self::annotations`]), in order.
    pub fn annotations_of(&self, id: FunctionId) -> impl Iterator<Item = Span> + '_ {
        let start = self.annotations.partition_point(|a| a.0 < id);
        self.annotations[start..]
            .iter()
            .take_while(move |a| a.0 == id)
            .map(|&(_, span)| span)
    }

    pub fn all_a1_hazards(&self) -> Vec<(&IrFunction, &A1Hazard)> {
        let mut out = Vec::new();
        for f in &self.functions {
            for h in &f.a1_hazards {
                out.push((f, h));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use heki_ast::parse_module;
    use heki_env::{PythonVersion, RuntimeProfile};
    use heki_semantic::analyze_module;

    fn build(src: &str) -> ModuleIr {
        let parsed = parse_module(src).expect("valid python");
        let profile = RuntimeProfile::from(PythonVersion::PY315);
        let semantic = analyze_module(&parsed, src, profile);
        build_module_ir(&parsed, src, &semantic, profile)
    }

    #[test]
    fn for_iter_is_placed_outside_loop_body() {
        let src = r#"
def fetch(factory):
    out = []
    for item in factory():
        out.append(item)
    return out
"#;
        let ir = build(src);
        let f = ir.functions.iter().find(|f| f.name == "fetch").unwrap();
        assert_eq!(f.loops.len(), 1);
        let lp = &f.loops[0];

        // `factory()` Call and `MakeIter` MUST be outside loop body (depth 0).
        let mut saw_factory_call = false;
        let mut saw_make_iter = false;
        let mut saw_iter_next = false;
        let mut saw_append = false;
        for bb in &f.blocks {
            for inst in &bb.insts {
                match &inst.kind {
                    InstKind::Call { callee, .. } if callee == "factory" => {
                        saw_factory_call = true;
                        assert_eq!(f.block_loop_depth(bb.id), 0);
                        assert!(!lp.body.contains(&bb.id));
                    }
                    InstKind::MakeIter { .. } => {
                        saw_make_iter = true;
                        assert_eq!(f.block_loop_depth(bb.id), 0);
                        assert!(!lp.body.contains(&bb.id));
                    }
                    InstKind::IterNext { .. } => {
                        saw_iter_next = true;
                        assert_eq!(f.block_loop_depth(bb.id), 1);
                        assert!(lp.body.contains(&bb.id));
                    }
                    InstKind::MethodCall { method, .. } if method == "append" => {
                        saw_append = true;
                        assert_eq!(f.block_loop_depth(bb.id), 1);
                    }
                    _ => {}
                }
            }
        }
        assert!(saw_factory_call && saw_make_iter && saw_iter_next && saw_append);
    }

    #[test]
    fn loop_carried_distinguishes_self_accumulating_vs_oneshot_concat() {
        let src = r#"
import pandas as pd

def bad_accum(chunks):
    df = pd.DataFrame()
    for c in chunks:
        df = pd.concat([df, c])
    return df

def good_oneshot(pairs):
    out = []
    for a, b in pairs:
        combined = pd.concat([a, b])
        out.append(combined)
    return out
"#;
        let ir = build(src);
        let bad = ir.functions.iter().find(|f| f.name == "bad_accum").unwrap();
        assert!(
            bad.loop_carried
                .iter()
                .any(|lc| lc.var_name.as_deref() == Some("df") && lc.self_accumulating),
            "expected `df` to be loop-carried self-accumulating: {:?}",
            bad.loop_carried
        );

        let good = ir
            .functions
            .iter()
            .find(|f| f.name == "good_oneshot")
            .unwrap();
        assert!(
            good.loop_carried.is_empty(),
            "one-shot `combined = pd.concat([a, b])` must NOT be loop-carried: {:?}",
            good.loop_carried
        );
    }

    #[test]
    fn dict_unpack_preserves_order_for_opt102() {
        let src = r#"
def left_accum(items):
    d = {}
    for k, v in items:
        d = {**d, k: v}
    return d

def right_accum(items, other):
    d = {}
    for _ in items:
        d = {**other, **d}
    return d
"#;
        let ir = build(src);
        let left = ir
            .functions
            .iter()
            .find(|f| f.name == "left_accum")
            .unwrap();
        let left_dict = left
            .blocks
            .iter()
            .flat_map(|b| b.insts.iter())
            .find_map(|i| match &i.kind {
                InstKind::DictLiteral { entries, .. } if !entries.is_empty() => Some(entries),
                _ => None,
            })
            .unwrap();
        assert!(matches!(left_dict[0], DictEntry::Unpack { .. }));
        assert!(matches!(left_dict[1], DictEntry::KeyValue { .. }));

        let right = ir
            .functions
            .iter()
            .find(|f| f.name == "right_accum")
            .unwrap();
        let right_dict = right
            .blocks
            .iter()
            .flat_map(|b| b.insts.iter())
            .find_map(|i| match &i.kind {
                InstKind::DictLiteral { entries, .. } if !entries.is_empty() => Some(entries),
                _ => None,
            })
            .unwrap();
        assert!(
            matches!(
                right_dict.as_slice(),
                [DictEntry::Unpack { dict: first }, DictEntry::Unpack { dict: second }]
                    if right.var_name(*first) == Some("other") && right.var_name(*second) == Some("d")
            ),
            "expected `{{**other, **d}}`, got {right_dict:?}"
        );
    }

    #[test]
    fn nested_classes_have_distinct_owner_class_and_inner_fn_has_none() {
        let src = r#"
class Outer:
    def outer_method(self):
        def local_helper():
            return 1
        return local_helper()

    class Inner:
        def __init__(self):
            self.x = 1
"#;
        let ir = build(src);
        let outer_m = ir
            .functions
            .iter()
            .find(|f| f.qualname == "Outer.outer_method")
            .unwrap();
        let inner_init = ir
            .functions
            .iter()
            .find(|f| f.qualname == "Outer.Inner.__init__")
            .unwrap();
        let helper = ir
            .functions
            .iter()
            .find(|f| f.name == "local_helper")
            .unwrap();

        assert!(outer_m.owner_class.is_some());
        assert!(inner_init.owner_class.is_some());
        assert_ne!(outer_m.owner_class, inner_init.owner_class);
        assert_eq!(outer_m.owner_class_qualname.as_deref(), Some("Outer"));
        assert_eq!(
            inner_init.owner_class_qualname.as_deref(),
            Some("Outer.Inner")
        );
        assert_eq!(helper.owner_class, None);
    }

    #[test]
    fn tracks_is_awaited_exception_edges_const_keys_and_a1_hazards() {
        let src = r#"
import sys

async def run(client, d):
    try:
        a = await client.get("https://example.com")
        b = client.post("https://example.com")
        v = d["outtmpl"]
        eval("1 + 1")
        f = sys._getframe(0)
        _ = f.f_locals
    except RuntimeError as err:
        return err
    return (a, b, v)
"#;
        let ir = build(src);
        let f = ir.functions.iter().find(|f| f.name == "run").unwrap();
        assert_eq!(f.kind, FunctionKind::Async);

        // Verify is_awaited true vs false
        let mut awaited_get = false;
        let mut sync_post = false;
        let mut saw_key_const = false;
        let mut has_exc_edge = false;
        for bb in &f.blocks {
            if bb.exc_target.is_some() {
                has_exc_edge = true;
            }
            for inst in &bb.insts {
                match &inst.kind {
                    InstKind::MethodCall {
                        method, is_awaited, ..
                    } if method == "get" => awaited_get = *is_awaited,
                    InstKind::MethodCall {
                        method, is_awaited, ..
                    } if method == "post" => sync_post = !*is_awaited,
                    InstKind::SubscriptLoad {
                        key_const: Some(k), ..
                    } if k == "outtmpl" => saw_key_const = true,
                    _ => {}
                }
            }
        }
        assert!(awaited_get);
        assert!(sync_post);
        assert!(saw_key_const);
        assert!(has_exc_edge);
        assert!(
            f.a1_hazards.len() >= 3,
            "expected eval, sys._getframe, f_locals hazards: {:?}",
            f.a1_hazards
        );
        assert!(!f.rpo_blocks().is_empty());
    }

    #[test]
    fn classifies_loop_bounds_const_small_retry_daemon_and_unbounded() {
        let src = r#"
import time

def small():
    for i in range(4):
        pass

def retry(fn):
    for attempt in range(3):
        try:
            return fn()
        except Exception:
            time.sleep(1)

class Worker:
    def serve(self):
        while self._running:
            time.sleep(0.1)

def general(items):
    for x in items:
        pass
"#;
        let ir = build(src);
        let small = ir.functions.iter().find(|f| f.name == "small").unwrap();
        assert_eq!(small.loops[0].bound, LoopBound::ConstSmall(4));

        let retry = ir.functions.iter().find(|f| f.name == "retry").unwrap();
        assert_eq!(retry.loops[0].bound, LoopBound::RetryPattern);

        let serve = ir.functions.iter().find(|f| f.name == "serve").unwrap();
        assert_eq!(serve.loops[0].bound, LoopBound::DaemonPolling);

        let general = ir.functions.iter().find(|f| f.name == "general").unwrap();
        assert_eq!(general.loops[0].bound, LoopBound::Unbounded);
    }

    #[test]
    fn aug_assign_emits_load_binop_inplace_and_store() {
        let src = r#"
def inc(xs, obj):
    x = 1
    x += 2
    obj.count += 1
    xs[0] += 3
    return x
"#;
        let ir = build(src);
        let f = ir.functions.iter().find(|f| f.name == "inc").unwrap();
        let inplace_bins: Vec<_> = f
            .blocks
            .iter()
            .flat_map(|b| b.insts.iter())
            .filter(|i| matches!(i.kind, InstKind::BinOp { in_place: true, .. }))
            .collect();
        assert_eq!(inplace_bins.len(), 3);
    }

    #[test]
    fn genexpr_emits_make_gen_expr_with_outer_iter_and_child_function() {
        let src = r#"
def make(xs):
    g = (x * 2 for x in xs)
    return g
"#;
        let ir = build(src);
        let f = ir.functions.iter().find(|f| f.name == "make").unwrap();
        let has_genexpr = f.blocks.iter().flat_map(|b| b.insts.iter()).any(|i| {
            matches!(i.kind, InstKind::MakeGenExpr { outer_iter, .. } if f.var_name(outer_iter) == Some("xs"))
        });
        assert!(has_genexpr);
        assert!(
            ir.functions
                .iter()
                .any(|cf| cf.kind == FunctionKind::Generator && cf.name == "<genexpr>")
        );
    }

    #[test]
    fn tracks_closure_captures_and_pep709_comp_lexical_isolation() {
        let src = r#"
def outer(xs):
    acc = []
    def inner():
        return acc
    x = 10
    ys = [x * 2 for x in xs]
    return (inner, x, ys)
"#;
        let ir = build(src);
        let outer = ir.functions.iter().find(|f| f.name == "outer").unwrap();
        assert!(
            outer.captured_vars.iter().any(|c| c == "acc"),
            "expected `acc` in captured_vars: {:?}",
            outer.captured_vars
        );
        // Inlined PEP 709 listcomp creates a depth-1 loop while keeping outer `x` isolated.
        assert_eq!(outer.loops.len(), 1);
        assert!(
            outer.loop_carried.is_empty(),
            "outer `x` must remain lexically isolated from comprehension target `x`: {:?}",
            outer.loop_carried
        );
    }

    #[test]
    fn records_star_imports_for_builtin_shadowing() {
        assert!(build("from os.path import *\n").has_star_import);
        assert!(!build("from os.path import join\nimport os\n").has_star_import);
    }

    /// A name bound to another string or tuple constant, and a tuple item naming a string
    /// constant, share its text: the tables keep one copy of each text the module writes.
    #[test]
    fn aliased_text_constants_share_one_text() {
        let src =
            "S0 = \"CREATE TABLE t (x)\"\nS1 = S0\nS2 = S1\nT0 = (S0, \"DROP TABLE t\")\nT1 = T0\n";
        let constants = build(src).constants;
        let s0 = &constants.strs["S0"];
        for alias in ["S1", "S2"] {
            assert!(
                std::sync::Arc::ptr_eq(s0, &constants.strs[alias]),
                "{alias}"
            );
        }
        let t0 = &constants.str_tuples["T0"];
        assert!(std::sync::Arc::ptr_eq(t0, &constants.str_tuples["T1"]));
        assert!(std::sync::Arc::ptr_eq(s0, &t0[0]));
        assert_eq!(&*t0[1], "DROP TABLE t");
    }
}
