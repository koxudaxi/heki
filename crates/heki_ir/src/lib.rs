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

pub use builder::build_module_ir;

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
}

/// Loop-carried SSA `PhiNode` metadata (used by `DATA101` and `OPT101`–`OPT103`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopCarriedInfo {
    pub loop_header: BlockId,
    pub phi_var: VarId,
    pub var_name: Option<String>,
    pub init_vars: Vec<VarId>,
    pub back_vars: Vec<VarId>,
    /// True when at least one back-edge update depends transitively on `phi_var`
    /// inside the loop (e.g. `x = x + [e]`, `d = {**d, k: v}`, `df = pd.concat([df, chunk])`).
    pub self_accumulating: bool,
}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnaryOpKind {
    Not,
    Invert,
    UAdd,
    USub,
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
    Delete {
        var: Option<VarId>,
        name: String,
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
    /// Nested function or lambda capturing outer variables.
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
    },
    MethodCall {
        dst: VarId,
        recv: VarId,
        method: String,
        args: Vec<VarId>,
        kwargs: Vec<(Option<String>, VarId)>,
        is_awaited: bool,
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
    pub fn successors(&self) -> Vec<BlockId> {
        match self {
            Self::Jump(t) => vec![*t],
            Self::Branch {
                then_bb, else_bb, ..
            } => {
                if then_bb == else_bb {
                    vec![*then_bb]
                } else {
                    vec![*then_bb, *else_bb]
                }
            }
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
    pub blocks: Vec<BasicBlock>,
    pub loops: Vec<NaturalLoop>,
    pub loop_carried: Vec<LoopCarriedInfo>,
    pub a1_hazards: Vec<A1Hazard>,
    pub span: Span,
    pub is_module_body: bool,
    pub is_class_body: bool,
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
        if self.blocks.is_empty() {
            return Vec::new();
        }
        let mut visited = FxHashSet::default();
        let mut postorder = Vec::with_capacity(self.blocks.len());
        let mut stack: Vec<(BlockId, bool)> = vec![(BlockId(0), false)];

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
}

impl ModuleIr {
    pub fn function(&self, id: FunctionId) -> &IrFunction {
        &self.functions[id.0 as usize]
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
            !good
                .loop_carried
                .iter()
                .any(|lc| lc.var_name.as_deref() == Some("combined") && lc.self_accumulating),
            "one-shot `combined = pd.concat([a, b])` must NOT be self-accumulating: {:?}",
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
        assert_eq!(right_dict.len(), 2);
        match (&right_dict[0], &right_dict[1]) {
            (DictEntry::Unpack { dict: first }, DictEntry::Unpack { dict: second }) => {
                assert_eq!(right.var_name(*first), Some("other"));
                assert_eq!(right.var_name(*second), Some("d"));
            }
            other => panic!("expected two unpacks, got {other:?}"),
        }
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
            !outer
                .loop_carried
                .iter()
                .any(|lc| lc.var_name.as_deref() == Some("x")),
            "outer `x` must remain lexically isolated from comprehension target `x`"
        );
    }

    #[test]
    fn records_star_imports_for_builtin_shadowing() {
        assert!(build("from os.path import *\n").has_star_import);
        assert!(!build("from os.path import join\nimport os\n").has_star_import);
    }
}
