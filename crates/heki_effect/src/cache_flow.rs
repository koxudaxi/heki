//! `MUT101` facts (docs/ANALYSIS_AND_KB_SPEC.md section 2.3): a method's own writes of `self`
//! attributes, and the control flow from them to the points that invalidate or read a cached
//! property of its instance.
//!
//! They are read from the IR once, when the method is summarized, and kept in
//! [`crate::FileSummary::cache_flows`]: whether a write leaves a cached value behind is then
//! decided from these facts and the summaries alone, also for a method of a file that is not
//! checked (the language server checks one file).

use crate::resolver::namespace_load;
use crate::{CallSite, FuncKey, TypeCtx, Val, aliases_of, container_mutator, guards, self_aliases};
use heki_ast::Span;
use heki_ir::walk::FnShape;
use heki_ir::{BlockId, CompareOpKind, InstKind, IrFunction, Terminator, VarId};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

/// The `MUT101` facts of one definition of a method that writes a `self` attribute itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheFlow {
    /// The attribute and property names the writes and events refer to, by index.
    pub names: Vec<String>,
    /// The method's own writes of `self.<attr>`, in source order.
    pub writes: Vec<SelfAttrWrite>,
    /// Every block of the method, by `BlockId`.
    pub blocks: Vec<FlowBlock>,
}

/// A write of `self.<attr>` in a method's own body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfAttrWrite {
    /// The attribute, an index into [`CacheFlow::names`].
    pub attr: u32,
    pub span: Span,
    pub block: BlockId,
    /// The write's instruction index in `block`.
    pub index: u32,
    /// `setdefault`, or a subscript store guarded by `if k not in self.<attr>:`.
    pub memo_shaped: bool,
    /// `self.<attr> = ...` / `del self.<attr>`: the attribute is rebound, not changed in place.
    pub rebinds: bool,
    /// The write runs only where a `None` / falsy test of `self.<attr>` found it unset
    /// (`if self.<attr> is None:`, `if not self.<attr>:`): a lazy initialization.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lazy_init: bool,
}

impl SelfAttrWrite {
    /// A write that fills a memo rather than changing what was computed from the object: a
    /// memo-shaped write (`setdefault`, a store under `if k not in <obj>.<attr>:`), or an
    /// in-place change of an attribute named as a memo or cache ([`is_memo_name`]). Rebinding
    /// such an attribute is no memo operation (`self.cache_size = n`).
    pub fn fills_memo(&self, attr: &str) -> bool {
        self.memo_shaped || !self.rebinds && is_memo_name(attr)
    }
}

/// Memo / cache dictionaries by name: a word of the attribute's name is `cache...` or `memo...`
/// (`self._cache`, `self.memo`, `self._resultCache`, `self.memoized`), not `memory`.
pub fn is_memo_name(attr: &str) -> bool {
    let mut words: Vec<String> = Vec::new();
    for part in attr.split('_') {
        let mut word = String::new();
        for c in part.chars() {
            if c.is_ascii_uppercase() && !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            word.push(c.to_ascii_lowercase());
        }
        words.push(word);
    }
    words
        .iter()
        .any(|w| w.starts_with("cache") || w.starts_with("memo") && !w.starts_with("memor"))
}

/// One block of a method: its normal control flow and the points that invalidate or read a
/// cached property, by instruction index.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowBlock {
    pub succs: Vec<BlockId>,
    /// The block ends in `return`.
    pub returns: bool,
    /// Where an exception raised in the block continues.
    pub exc_target: Option<BlockId>,
    pub events: Vec<(u32, CacheEvent)>,
}

/// What one instruction does to the cached properties of the method's own instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheEvent {
    /// `del self.<name>` (a name index).
    DeleteAttr(u32),
    /// Reading `self.<name>`: a cached property named so computes its value again.
    LoadAttr(u32),
    /// Removing the entry of a constant key from `self.__dict__` (`del self.__dict__[k]`,
    /// `.pop(k)`, `.__delitem__(k)`), with the key as a literal or a `Final` string constant.
    RemoveEntry(u32),
    /// `self.__dict__.clear()`.
    ClearDict,
    /// A call whose call site has a `self` receiver: what it does is in its callees' summaries.
    SelfCall(Span),
}

impl CacheFlow {
    pub fn name(&self, index: u32) -> &str {
        &self.names[index as usize]
    }

    /// The method's own writes of `self.<attr>`, in source order.
    pub fn writes_of<'s>(&'s self, attr: &'s str) -> impl Iterator<Item = &'s SelfAttrWrite> {
        self.writes
            .iter()
            .filter(move |w| self.name(w.attr) == attr)
    }

    /// The attributes the method initializes lazily (a write of
    /// [`SelfAttrWrite::lazy_init`]).
    pub fn lazily_initialized(&self) -> impl Iterator<Item = &str> {
        self.writes
            .iter()
            .filter(|w| w.lazy_init)
            .map(|w| self.name(w.attr))
    }
}

/// The `MUT101` facts of `f` (`None` for a function that is no method or writes no `self`
/// attribute itself). `sites` are the function's call sites.
pub(crate) fn cache_flow(
    shape: &FnShape<'_>,
    key: &FuncKey,
    sites: &[CallSite],
    tcx: &TypeCtx<'_>,
    types: &FxHashMap<VarId, Val>,
) -> Option<CacheFlow> {
    let f = shape.f;
    if f.is_class_body || f.is_module_body || f.owner_class_qualname.is_none() {
        return None;
    }
    let writes = self_attr_writes(shape);
    if writes.is_empty() {
        return None;
    }
    let mut names = Names::default();
    let writes = writes
        .into_iter()
        .map(|(attr, w)| SelfAttrWrite {
            attr: names.index(attr),
            ..w
        })
        .collect();
    let blocks = flow_blocks(shape, key, sites, tcx, types, &mut names);
    Some(CacheFlow {
        names: names.list,
        writes,
        blocks,
    })
}

#[derive(Default)]
struct Names {
    list: Vec<String>,
    index: FxHashMap<String, u32>,
}

impl Names {
    fn index(&mut self, name: &str) -> u32 {
        if let Some(&i) = self.index.get(name) {
            return i;
        }
        let i = u32::try_from(self.list.len()).unwrap_or(u32::MAX);
        self.list.push(name.to_owned());
        self.index.insert(name.to_owned(), i);
        i
    }
}

/// The variables of `f` that are its `self` (its first parameter, through copies).
fn self_vars(f: &IrFunction) -> FxHashSet<VarId> {
    f.params
        .first()
        .map(|p| self_aliases(f, p.var))
        .unwrap_or_default()
}

/// Writes of `self.<attr>` in `f`'s own body with their attribute, in source order (none
/// without a `self` parameter): rebinding or deleting it, and changing the container it holds
/// in place (also through local copies of `self.<attr>`). `self.__dict__` itself is no
/// attribute a property reads.
pub fn self_attr_writes<'f>(shape: &FnShape<'f>) -> Vec<(&'f str, SelfAttrWrite)> {
    let objects = self_vars(shape.f).into_iter().map(|v| (v, 0)).collect();
    attr_writes(shape, &objects)
}

/// An attribute of one of the objects [`attr_writes`] reads: the object's number and the name.
type Held<'f> = (u32, &'f str);

/// [`self_attr_writes`] of several objects at once (`self`, parameters, shared values): the
/// writes of `<obj>.<attr>` in `f`'s own body, where `objects` gives each variable holding one
/// of them the object's number. Each object is read on its own: a test of one object's attribute
/// says nothing of another's.
pub fn attr_writes<'f>(
    shape: &FnShape<'f>,
    objects: &FxHashMap<VarId, u32>,
) -> Vec<(&'f str, SelfAttrWrite)> {
    let f = shape.f;
    // `var -> (object, attr)`: the variables holding `<obj>.<attr>`, through copies.
    let mut alias: FxHashMap<VarId, (u32, &str)> = FxHashMap::default();
    let mut defs: FxHashMap<VarId, &InstKind> = FxHashMap::default();
    for inst in f.blocks.iter().flat_map(|b| &b.insts) {
        if let Some(d) = inst.kind.dst() {
            defs.insert(d, &inst.kind);
        }
        if let InstKind::AttrLoad { dst, obj, attr } = &inst.kind
            && let Some(&o) = objects.get(obj)
            && attr != "__dict__"
        {
            alias.insert(*dst, (o, attr));
        }
    }
    loop {
        let before = alias.len();
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            if let InstKind::Copy { dst, src } = &inst.kind
                && let Some(&a) = alias.get(src)
            {
                alias.insert(*dst, a);
            }
        }
        if alias.len() == before {
            break;
        }
    }
    // The `if k not in <obj>.<attr>:` branches, by the block they lead into, with the key tested.
    let mut membership: FxHashMap<BlockId, Vec<(Held<'_>, VarId)>> = FxHashMap::default();
    for p in &f.blocks {
        if let Terminator::Branch { cond, then_bb, .. } = &p.terminator
            && let Some(InstKind::Compare {
                op: CompareOpKind::NotIn,
                lhs,
                rhs,
                ..
            }) = defs.get(cond)
            && let Some(&held) = alias.get(rhs)
        {
            membership.entry(*then_bb).or_default().push((held, *lhs));
        }
    }
    // The store fills the key the branch found missing: the same value, or an equal constant.
    let literal = |v: &VarId| match defs.get(v) {
        Some(InstKind::Const { literal, .. }) => Some(literal),
        _ => None,
    };
    let same_key = |tested: &VarId, index: &VarId| {
        tested == index || literal(tested).is_some_and(|t| literal(index) == Some(t))
    };
    let guarded = |bb: BlockId, held: (u32, &str), index: &VarId| {
        membership
            .get(&bb)
            .is_some_and(|tests| tests.iter().any(|(h, k)| *h == held && same_key(k, index)))
    };
    // `(block, index, (object, attr), memo_shaped, rebinds)` of each write.
    let mut found = Vec::new();
    for bb in &f.blocks {
        for (index, inst) in bb.insts.iter().enumerate() {
            let w = match &inst.kind {
                InstKind::AttrStore { obj, attr, .. } | InstKind::AttrDelete { obj, attr }
                    if attr != "__dict__" =>
                {
                    objects.get(obj).map(|&o| ((o, attr.as_str()), false, true))
                }
                InstKind::SubscriptStore { obj, index, .. } => alias
                    .get(obj)
                    .map(|&held| (held, guarded(bb.id, held, index), false)),
                InstKind::SubscriptDelete { obj, .. } => {
                    alias.get(obj).map(|&held| (held, false, false))
                }
                InstKind::MethodCall { recv, method, .. } if container_mutator(method) => alias
                    .get(recv)
                    .map(|&held| (held, method == "setdefault", false)),
                _ => None,
            };
            if let Some((held, memo_shaped, rebinds)) = w {
                found.push((bb.id, index, inst.span, held, memo_shaped, rebinds));
            }
        }
    }
    if found.is_empty() {
        return Vec::new();
    }
    let mut written: FxHashMap<&str, FxHashSet<u32>> = FxHashMap::default();
    for (_, _, _, (o, attr), _, _) in &found {
        written.entry(attr).or_default().insert(*o);
    }
    let lazy = lazy_init_blocks(shape, objects, &written);
    let mut out: Vec<(&str, SelfAttrWrite)> = found
        .into_iter()
        .map(|(block, index, span, held, memo_shaped, rebinds)| {
            let write = SelfAttrWrite {
                attr: 0,
                span,
                block,
                index: u32::try_from(index).unwrap_or(u32::MAX),
                memo_shaped,
                rebinds,
                lazy_init: lazy
                    .iter()
                    .any(|(h, blocks)| *h == held && blocks[block.0 as usize]),
            };
            (held.1, write)
        })
        .collect();
    out.sort_by_key(|(_, w)| w.span);
    out
}

/// The attributes of the objects `objects` holds that `f` initializes lazily, among the
/// `written` ones, each with the blocks (by id) that only the "unset" outcome of one of its
/// `None` / falsy tests leads to ([`guards::lazy_init_branches`], [`guards::dominated`]): every
/// path from the entry to such a block runs through the branch (normal and exception edges).
/// The walks share the function's budget ([`guards::Cells`]); past it none is kept.
fn lazy_init_blocks<'f>(
    shape: &FnShape<'_>,
    objects: &FxHashMap<VarId, u32>,
    written: &FxHashMap<&'f str, FxHashSet<u32>>,
) -> Vec<((u32, &'f str), Vec<bool>)> {
    let mut cells = guards::Cells::of(shape.f);
    let mut found = Vec::new();
    let branches = guards::lazy_init_branches(shape)
        .into_iter()
        .filter_map(|b| {
            let o = *objects.get(&b.obj)?;
            let (attr, by) = written.get_key_value(b.attr.as_str())?;
            by.contains(&o).then_some(((o, *attr), b.init))
        });
    for (held, init) in branches {
        found.push((held, guards::dominated(shape, init, &mut cells)));
        if cells.spent() {
            return Vec::new();
        }
    }
    found
}

/// The blocks of `f` (whose variables have `types`) with the points that invalidate or read a
/// cached property of its own instance.
fn flow_blocks(
    shape: &FnShape<'_>,
    key: &FuncKey,
    sites: &[CallSite],
    tcx: &TypeCtx<'_>,
    types: &FxHashMap<VarId, Val>,
    names: &mut Names,
) -> Vec<FlowBlock> {
    let f = shape.f;
    let self_vars = self_vars(f);
    let dicts = aliases_of(
        f,
        f.blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|i| namespace_load(&i.kind, |v| types.get(&v)))
            .filter(|(_, obj)| self_vars.contains(obj))
            .map(|(dst, _)| dst),
    );
    // A key naming a property: a string literal or a `Final` string constant.
    let key_name = |v: VarId| {
        shape
            .str_const(v)
            .map(str::to_owned)
            .or_else(|| tcx.str_constant_in(shape, v, types))
    };
    let self_calls: FxHashSet<Span> = sites
        .iter()
        .filter(|s| s.recv_is_self && &s.caller == key)
        .map(|s| s.span)
        .collect();
    let mut blocks = vec![FlowBlock::default(); f.blocks.len()];
    for bb in &f.blocks {
        let mut events = Vec::new();
        for (index, inst) in bb.insts.iter().enumerate() {
            let event = match &inst.kind {
                InstKind::AttrDelete { obj, attr } if self_vars.contains(obj) => {
                    Some(CacheEvent::DeleteAttr(names.index(attr)))
                }
                InstKind::AttrLoad { obj, attr, .. }
                    if self_vars.contains(obj) && attr != "__dict__" =>
                {
                    Some(CacheEvent::LoadAttr(names.index(attr)))
                }
                InstKind::SubscriptDelete { obj, index, .. } if dicts.contains(obj) => {
                    key_name(*index).map(|k| CacheEvent::RemoveEntry(names.index(&k)))
                }
                InstKind::MethodCall {
                    recv, method, args, ..
                } if dicts.contains(recv) => match method.as_str() {
                    "clear" => Some(CacheEvent::ClearDict),
                    "pop" | "__delitem__" => args
                        .first()
                        .and_then(|a| key_name(*a))
                        .map(|k| CacheEvent::RemoveEntry(names.index(&k))),
                    _ => None,
                },
                InstKind::MethodCall { .. } | InstKind::Call { .. }
                    if self_calls.contains(&inst.span) =>
                {
                    Some(CacheEvent::SelfCall(inst.span))
                }
                _ => None,
            };
            if let Some(event) = event {
                events.push((u32::try_from(index).unwrap_or(u32::MAX), event));
            }
        }
        blocks[bb.id.0 as usize] = FlowBlock {
            succs: bb.terminator.successors(),
            returns: matches!(bb.terminator, Terminator::Return(_)),
            exc_target: bb.exc_target,
            events,
        };
    }
    blocks
}
