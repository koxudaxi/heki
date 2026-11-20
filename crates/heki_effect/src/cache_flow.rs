//! `MUT101` facts (docs/ANALYSIS_AND_KB_SPEC.md section 2.3): a method's own writes of `self`
//! attributes, and the control flow from them to the points that invalidate or read a cached
//! property of its instance.
//!
//! They are read from the IR once, when the method is summarized, and kept in
//! [`crate::FileSummary::cache_flows`]: whether a write leaves a cached value behind is then
//! decided from these facts and the summaries alone, also for a method of a file that is not
//! checked (the language server checks one file).

use crate::{CONTAINER_MUTATORS, CallSite, FuncKey, TypeCtx, aliases_of, guards, self_aliases};
use heki_ast::Span;
use heki_ir::{BlockId, CompareOpKind, InstKind, IrFunction, Terminator, VarId};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::cell::OnceCell;

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
    f: &IrFunction,
    key: &FuncKey,
    sites: &[CallSite],
    tcx: &TypeCtx<'_>,
) -> Option<CacheFlow> {
    if f.is_class_body || f.is_module_body || f.owner_class_qualname.is_none() {
        return None;
    }
    let writes = self_attr_writes(f);
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
    let blocks = flow_blocks(f, key, sites, tcx, &mut names);
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
pub fn self_attr_writes(f: &IrFunction) -> Vec<(&str, SelfAttrWrite)> {
    attr_writes(f, &self_vars(f))
}

/// [`self_attr_writes`] of the objects the variables `objs` hold (`self`, a parameter): the
/// writes of `<obj>.<attr>` in `f`'s own body.
pub fn attr_writes<'f>(
    f: &'f IrFunction,
    self_vars: &FxHashSet<VarId>,
) -> Vec<(&'f str, SelfAttrWrite)> {
    // `var -> attr`: the variables holding `self.<attr>`, through copies.
    let mut alias: FxHashMap<VarId, &str> = FxHashMap::default();
    let mut defs: FxHashMap<VarId, &InstKind> = FxHashMap::default();
    for inst in f.blocks.iter().flat_map(|b| &b.insts) {
        if let Some(d) = inst.kind.dst() {
            defs.insert(d, &inst.kind);
        }
        if let InstKind::AttrLoad { dst, obj, attr } = &inst.kind
            && self_vars.contains(obj)
            && attr != "__dict__"
        {
            alias.insert(*dst, attr);
        }
    }
    loop {
        let before = alias.len();
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            if let InstKind::Copy { dst, src } = &inst.kind
                && let Some(&attr) = alias.get(src)
            {
                alias.insert(*dst, attr);
            }
        }
        if alias.len() == before {
            break;
        }
    }
    // The `if k not in self.<attr>:` branch into `bb`.
    let guarded = |bb: BlockId, attr: &str| {
        f.blocks.iter().any(|p| match &p.terminator {
            Terminator::Branch { cond, then_bb, .. } if *then_bb == bb => matches!(
                defs.get(cond),
                Some(InstKind::Compare { op: CompareOpKind::NotIn, rhs, .. })
                    if alias.get(rhs) == Some(&attr)
            ),
            _ => false,
        })
    };
    let lazy = lazy_init_blocks(f, self_vars);
    let mut out = Vec::new();
    for bb in &f.blocks {
        for (index, inst) in bb.insts.iter().enumerate() {
            // `(attr, memo_shaped, rebinds)`.
            let w = match &inst.kind {
                InstKind::AttrStore { obj, attr, .. } | InstKind::AttrDelete { obj, attr }
                    if self_vars.contains(obj) && attr != "__dict__" =>
                {
                    Some((attr.as_str(), false, true))
                }
                InstKind::SubscriptStore { obj, .. } => alias
                    .get(obj)
                    .map(|&attr| (attr, guarded(bb.id, attr), false)),
                InstKind::SubscriptDelete { obj, .. } => {
                    alias.get(obj).map(|&attr| (attr, false, false))
                }
                InstKind::MethodCall { recv, method, .. }
                    if CONTAINER_MUTATORS.contains(&method.as_str()) =>
                {
                    alias
                        .get(recv)
                        .map(|&attr| (attr, method == "setdefault", false))
                }
                _ => None,
            };
            if let Some((attr, memo_shaped, rebinds)) = w {
                out.push((
                    attr,
                    SelfAttrWrite {
                        attr: 0,
                        span: inst.span,
                        block: bb.id,
                        index: u32::try_from(index).unwrap_or(u32::MAX),
                        memo_shaped,
                        rebinds,
                        lazy_init: lazy
                            .iter()
                            .any(|(a, blocks)| *a == attr && blocks[bb.id.0 as usize]),
                    },
                ));
            }
        }
    }
    out.sort_by_key(|(_, w)| w.span);
    out
}

/// The attributes of `self` that `f` initializes lazily, each with the blocks (by id) that only
/// the "unset" outcome of one of its `None` / falsy tests leads to
/// ([`guards::lazy_init_branches`]): every path from the entry to such a block runs through the
/// branch (normal and exception edges).
fn lazy_init_blocks(f: &IrFunction, self_vars: &FxHashSet<VarId>) -> Vec<(String, Vec<bool>)> {
    guards::lazy_init_branches(f)
        .into_iter()
        .filter(|b| self_vars.contains(&b.obj))
        .map(|b| {
            let blocks = avoidable_from_entry(f, b.init).into_iter().map(|a| !a);
            (b.attr, blocks.collect())
        })
        .collect()
}

/// Membership (by block id) of the blocks a path from the entry of `f` reaches without running
/// through `d` (normal and exception edges).
fn avoidable_from_entry(f: &IrFunction, d: BlockId) -> Vec<bool> {
    let mut seen = vec![false; f.blocks.len()];
    let mut stack = vec![BlockId(0)];
    while let Some(cur) = stack.pop() {
        if cur == d || std::mem::replace(&mut seen[cur.0 as usize], true) {
            continue;
        }
        let block = f.block(cur);
        stack.extend(block.terminator.successors());
        stack.extend(block.exc_target);
    }
    seen
}

/// The blocks of `f` with the points that invalidate or read a cached property of its own
/// instance.
fn flow_blocks(
    f: &IrFunction,
    key: &FuncKey,
    sites: &[CallSite],
    tcx: &TypeCtx<'_>,
    names: &mut Names,
) -> Vec<FlowBlock> {
    let self_vars = self_vars(f);
    let dicts = aliases_of(
        f,
        f.blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|i| match &i.kind {
                InstKind::AttrLoad { dst, obj, attr }
                    if attr == "__dict__" && self_vars.contains(obj) =>
                {
                    Some(*dst)
                }
                _ => None,
            }),
    );
    // A key naming a property: a string literal or a `Final` string constant.
    let types = OnceCell::new();
    let key_name = |v: VarId| {
        f.const_str(v)
            .map(str::to_owned)
            .or_else(|| tcx.str_constant_in(f, v, types.get_or_init(|| tcx.infer(f))))
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
