//! Local (one function) effect extraction (docs/ANALYSIS_AND_KB_SPEC.md section 2.2).
//!
//! Produces the function's local `EffectSummary` plus the `CallSite`s the interprocedural
//! fixpoint (`callgraph`) maps callee facts through. No IR survives this module.

use crate::resolver::{Callee, DeclKeys, ModuleDecls, TypeCtx, Val, func_qualname, self_aliases};
use crate::{
    ArgOrigin, AttrKeyPath, BoundCallee, CallHop, CallSite, CallTarget, EffectSummary, FileSummary,
    FuncKey, ModuleInput, ProjectDecls, ReadKind, Slot, TypedFieldReadSite, TypedFieldWriteSite,
    WriteKind,
};
use heki_ast::Span;
use heki_ir::{BlockId, ConstLiteral, FunctionId, FunctionKind, InstKind, IrFunction, VarId};
use heki_kb::{EffectiveEcosystemKb, ExecEffectKind};
use rustc_hash::{FxHashMap, FxHashSet};
use std::path::Path;

/// Methods that mutate their receiver in place (section 2.4; shared by MUT101/201/202).
pub const CONTAINER_MUTATORS: &[&str] = &[
    "append",
    "extend",
    "insert",
    "pop",
    "remove",
    "clear",
    "sort",
    "reverse",
    "add",
    "discard",
    "update",
    "popitem",
    "setdefault",
    "difference_update",
    "intersection_update",
    "symmetric_difference_update",
    "__setitem__",
    "__delitem__",
];

/// Execution effects `EFF101` treats as blocking the event loop.
pub const BLOCKING_EFFECTS: &[ExecEffectKind] = &[
    ExecEffectKind::BlockingNet,
    ExecEffectKind::BlockingDb,
    ExecEffectKind::BlockingSubprocess,
    ExecEffectKind::BlockingSync,
    ExecEffectKind::Sleep,
    ExecEffectKind::BlockingFs,
];

/// Summarize every function of one module (local facts only; see `ProjectIndex::build`).
/// Summarize one module against the project declarations. `own` is the module's own completed
/// declarations with what completing them read; with `track`, what the summary reads of
/// `decls` is recorded too (`FileSummary::decl_reads`).
pub(crate) fn summarize_module(
    input: &ModuleInput<'_>,
    decls: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
    own: (ModuleDecls, DeclKeys),
    track: bool,
) -> FileSummary {
    let (own_decls, completion_reads) = own;
    let (mut file, reads) = if track {
        decls.record(|| extract_module(input, decls, kb))
    } else {
        (extract_module(input, decls, kb), DeclKeys::default())
    };
    file.decls = own_decls;
    file.decl_reads = reads;
    file.decl_reads.extend(completion_reads);
    file
}

fn extract_module(
    input: &ModuleInput<'_>,
    decls: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
) -> FileSummary {
    let cx = TypeCtx::new(input, decls, kb);
    let mut file = FileSummary {
        path: input.path.clone(),
        module_name: input.module_name.clone(),
        content_hash: input.content_hash.clone(),
        ..FileSummary::default()
    };
    for f in &input.ir.functions {
        if f.is_class_body {
            continue;
        }
        let key = FuncKey {
            module: input.module_name.clone(),
            qualname: func_qualname(f).to_owned(),
        };
        let (summary, sites) = Extractor::new(f, &key, &cx, &input.path).run();
        for site in &sites {
            if let CallTarget::Project(callees) = &site.target {
                for c in callees {
                    file.call_edges
                        .push((key.clone(), c.callee.clone(), site.span));
                }
            }
        }
        file.call_sites.extend(sites);
        // Same qualname defined twice (conditional definitions): keep the union of facts.
        match file.local_functions.get_mut(&key) {
            Some(existing) => merge_duplicate(existing, summary),
            None => {
                file.local_functions.insert(key, summary);
            }
        }
    }
    file
}

fn merge_duplicate(into: &mut EffectSummary, other: EffectSummary) {
    into.direct_exec_effects.extend(other.direct_exec_effects);
    into.transitive_exec_effects
        .extend(other.transitive_exec_effects);
    for (k, chain) in other.exec_chains {
        into.exec_chains.entry(k).or_insert(chain);
    }
    into.mutates_params |= other.mutates_params;
    into.escapes_params |= other.escapes_params;
    into.consumes_params |= other.consumes_params;
    // A must-fact: propagate only through parameters both definitions forward unconditionally.
    into.unconditional_forwards &= other.unconditional_forwards;
    for (i, paths) in other.mutated_param_paths {
        into.mutated_param_paths.entry(i).or_default().extend(paths);
    }
    for (i, slots) in other.inplace_param_slots {
        into.inplace_param_slots.entry(i).or_default().extend(slots);
    }
    into.reads_self_attrs.extend(other.reads_self_attrs);
    into.mutates_self_attrs.extend(other.mutates_self_attrs);
    into.invalidates_cached.extend(other.invalidates_cached);
    into.typed_field_write_sites
        .extend(other.typed_field_write_sites);
    into.typed_field_read_sites
        .extend(other.typed_field_read_sites);
    into.private_calls.extend(other.private_calls);
    into.offloads_blocking_params |= other.offloads_blocking_params;
    into.unknown_calls |= other.unknown_calls;
    into.refresh_typed_field_writes();
}

/// Who owns an object being written: the method's `self`, a parameter path, or a typed object.
#[derive(Debug, Clone, Default)]
struct Owner {
    is_self: bool,
    param: Option<(u8, Option<String>)>,
    typed_class: Option<String>,
    /// Parameters the object may be, as itself.
    may_params: u64,
    /// An instance the function constructs (`TypedFieldWriteSite::fresh`).
    fresh: bool,
}

/// `obj.<attr>` with project-class `obj` (or `obj["key"]` with a `TypedDict` `obj`).
#[derive(Debug, Clone)]
struct ObjAttr {
    class: String,
    /// The attribute (the key of a `TypedDict`).
    attr: String,
    /// The variable is the load itself (not a copy / phi of it).
    direct: bool,
    /// The parameters `obj` may be (as itself).
    obj_params: u64,
    /// `obj` is an instance the function constructs (`TypedFieldWriteSite::fresh`).
    fresh: bool,
}

struct Extractor<'a> {
    f: &'a IrFunction,
    key: &'a FuncKey,
    cx: &'a TypeCtx<'a>,
    path: &'a Path,
    types: FxHashMap<VarId, Val>,
    /// Defining instruction of each variable.
    defs: FxHashMap<VarId, &'a InstKind>,
    /// Instructions using each variable as an operand.
    uses: FxHashMap<VarId, Vec<&'a InstKind>>,
    self_vars: FxHashSet<VarId>,
    param_alias: FxHashMap<VarId, (u8, Option<String>)>,
    /// One-level slots of parameters: `p.a` / `p["k"]` of parameter `p` itself, through copies
    /// and agreeing phis (`EffectSummary::inplace_param_slots`).
    param_slot_alias: FxHashMap<VarId, (u8, Slot)>,
    /// `self.<attr>` aliases: `var -> attr`.
    self_attr_alias: FxHashMap<VarId, String>,
    /// `obj.<attr>` with project-class `obj`.
    obj_attr_alias: FxHashMap<VarId, ObjAttr>,
    /// `x.__dict__` -> owner of `x`.
    dict_of: FxHashMap<VarId, Owner>,
    /// Parameters (bit `i`: parameter `i < 64`) each variable may be, through copies and phis
    /// (any input): the may-alias counterpart of `param_alias`'s `(i, None)` entries.
    may_params: FxHashMap<VarId, u64>,
    /// Instances the function constructs by calling a project class (`Model(...)`,
    /// `cls(...)`), through copies and phis whose every input is one (`a = A(x) if c else
    /// A(y)`): `TypedFieldWriteSite::fresh`.
    fresh: FxHashSet<VarId>,
    summary: EffectSummary,
    sites: Vec<CallSite>,
}

impl<'a> Extractor<'a> {
    fn new(f: &'a IrFunction, key: &'a FuncKey, cx: &'a TypeCtx<'a>, path: &'a Path) -> Self {
        let types = cx.infer(f);
        let mut defs = FxHashMap::default();
        let mut uses: FxHashMap<VarId, Vec<&InstKind>> = FxHashMap::default();
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            if let Some(d) = inst.kind.dst() {
                defs.insert(d, &inst.kind);
            }
            for op in inst.kind.operands() {
                uses.entry(op).or_default().push(&inst.kind);
            }
        }
        let is_method =
            f.owner_class_qualname.is_some() && !f.has_decorator(&["staticmethod", "classmethod"]);
        let self_vars = match (is_method, f.params.first()) {
            (true, Some(p)) => self_aliases(f, p.var),
            _ => FxHashSet::default(),
        };
        let summary = EffectSummary {
            is_async: matches!(f.kind, FunctionKind::Async | FunctionKind::AsyncGenerator),
            is_cached_property: f.has_decorator(&["cached_property"]),
            is_lru_cache: f.has_decorator(&["lru_cache", "cache"]),
            owner_class_qualname: f.owner_class_qualname.clone(),
            decorators: f.decorators.clone(),
            ..EffectSummary::default()
        };
        let mut ex = Self {
            f,
            key,
            cx,
            path,
            types,
            defs,
            uses,
            self_vars,
            param_alias: FxHashMap::default(),
            param_slot_alias: FxHashMap::default(),
            self_attr_alias: FxHashMap::default(),
            obj_attr_alias: FxHashMap::default(),
            dict_of: FxHashMap::default(),
            may_params: FxHashMap::default(),
            fresh: FxHashSet::default(),
            summary,
            sites: Vec::new(),
        };
        ex.build_may_params();
        ex.build_fresh();
        ex.build_aliases();
        ex
    }

    /// [`Self::may_params`]: a monotone fixpoint over copies and phis.
    fn build_may_params(&mut self) {
        for (i, p) in self.f.params.iter().enumerate().take(64) {
            *self.may_params.entry(p.var).or_default() |= 1u64 << i;
        }
        loop {
            let mut changed = false;
            for bb in &self.f.blocks {
                for phi in &bb.phis {
                    let bits = phi.incoming.iter().fold(0, |acc, (_, v)| {
                        acc | self.may_params.get(v).copied().unwrap_or(0)
                    });
                    changed |= or_bits(&mut self.may_params, phi.dst, bits);
                }
                for inst in &bb.insts {
                    if let InstKind::Copy { dst, src } = &inst.kind {
                        let bits = self.may_params.get(src).copied().unwrap_or(0);
                        changed |= or_bits(&mut self.may_params, *dst, bits);
                    }
                }
            }
            if !changed {
                return;
            }
        }
    }

    fn project_class(&self, var: VarId) -> Option<String> {
        match self.types.get(&var)? {
            Val::Inst(c) if self.cx.project.is_project_class(c) => Some(c.clone()),
            _ => None,
        }
    }

    /// Alias maps through `AttrLoad`, `Copy`, and agreeing phis (iterated to a fixpoint).
    fn build_aliases(&mut self) {
        for (i, p) in self.f.params.iter().enumerate() {
            if let Ok(i) = u8::try_from(i)
                && i < 64
            {
                self.param_alias.insert(p.var, (i, None));
            }
        }
        loop {
            let before = (
                self.param_alias.len(),
                self.param_slot_alias.len(),
                self.self_attr_alias.len(),
                self.obj_attr_alias.len(),
                self.dict_of.len(),
            );
            for bb in &self.f.blocks {
                for phi in &bb.phis {
                    let inputs: Vec<VarId> = phi
                        .incoming
                        .iter()
                        .map(|(_, v)| *v)
                        .filter(|v| *v != phi.dst)
                        .collect();
                    merge_phi(&mut self.param_alias, phi.dst, &inputs, |a| a.clone());
                    merge_phi(&mut self.param_slot_alias, phi.dst, &inputs, |a| a.clone());
                    merge_phi(&mut self.self_attr_alias, phi.dst, &inputs, |a| a.clone());
                    merge_obj_attr_phi(&mut self.obj_attr_alias, phi.dst, &inputs);
                }
                for inst in &bb.insts {
                    match &inst.kind {
                        InstKind::Copy { dst, src } => {
                            if let Some(a) = self.param_alias.get(src).cloned() {
                                self.param_alias.insert(*dst, a);
                            }
                            if let Some(a) = self.param_slot_alias.get(src).cloned() {
                                self.param_slot_alias.insert(*dst, a);
                            }
                            if let Some(a) = self.self_attr_alias.get(src).cloned() {
                                self.self_attr_alias.insert(*dst, a);
                            }
                            if let Some(a) = self.obj_attr_alias.get(src).cloned() {
                                let copy = ObjAttr { direct: false, ..a };
                                self.obj_attr_alias.insert(*dst, copy);
                            }
                            if let Some(o) = self.dict_of.get(src).cloned() {
                                self.dict_of.insert(*dst, o);
                            }
                        }
                        InstKind::AttrLoad { dst, obj, attr } if attr == "__dict__" => {
                            let owner = self.owner_of(*obj);
                            self.dict_of.insert(*dst, owner);
                        }
                        InstKind::AttrLoad { dst, obj, attr } => {
                            if let Some((i, path)) = self.param_alias.get(obj).cloned() {
                                if path.is_none() {
                                    self.param_slot_alias
                                        .insert(*dst, (i, Slot::Attr(attr.clone())));
                                }
                                self.param_alias
                                    .insert(*dst, (i, Some(path.unwrap_or_else(|| attr.clone()))));
                            }
                            if self.self_vars.contains(obj) {
                                self.self_attr_alias.insert(*dst, attr.clone());
                            }
                            if let Some(class) = self.project_class(*obj) {
                                let load = self.obj_attr(class, *obj, attr);
                                self.obj_attr_alias.insert(*dst, load);
                            }
                        }
                        // `p["key"]` of a parameter, `td["key"]` of a `TypedDict`: a key of a
                        // typed dict is its field.
                        InstKind::SubscriptLoad {
                            dst,
                            obj,
                            key_const: Some(key),
                            ..
                        } => {
                            if let Some((i, None)) = self.param_alias.get(obj) {
                                self.param_slot_alias
                                    .insert(*dst, (*i, Slot::Key(key.clone())));
                            }
                            if let Some(class) = self.project_class(*obj)
                                && self.cx.project.is_typed_dict(&class)
                            {
                                let load = self.obj_attr(class, *obj, key);
                                self.obj_attr_alias.insert(*dst, load);
                            }
                        }
                        _ => {}
                    }
                }
            }
            let after = (
                self.param_alias.len(),
                self.param_slot_alias.len(),
                self.self_attr_alias.len(),
                self.obj_attr_alias.len(),
                self.dict_of.len(),
            );
            if after == before {
                break;
            }
        }
    }

    /// The load `obj.<attr>` (`obj["<attr>"]`) of the project-class object `obj`.
    fn obj_attr(&self, class: String, obj: VarId, attr: &str) -> ObjAttr {
        ObjAttr {
            class,
            attr: attr.to_owned(),
            direct: true,
            obj_params: self.may_params.get(&obj).copied().unwrap_or(0),
            fresh: self.is_fresh(obj),
        }
    }

    /// [`Self::fresh`]: the results of calling a project class, then a monotone fixpoint over
    /// copies and phis.
    fn build_fresh(&mut self) {
        for inst in self.f.blocks.iter().flat_map(|b| &b.insts) {
            if let InstKind::Call {
                dst,
                callee_var: Some(c),
                ..
            } = &inst.kind
                && matches!(self.types.get(c), Some(Val::Ref(fqn)) if self.cx.project.is_project_class(fqn))
            {
                self.fresh.insert(*dst);
            }
        }
        loop {
            let before = self.fresh.len();
            for bb in &self.f.blocks {
                for phi in &bb.phis {
                    let mut inputs = phi
                        .incoming
                        .iter()
                        .map(|(_, v)| *v)
                        .filter(|v| *v != phi.dst);
                    let first = inputs.next();
                    if first.is_some_and(|v| self.fresh.contains(&v))
                        && inputs.all(|v| self.fresh.contains(&v))
                    {
                        self.fresh.insert(phi.dst);
                    }
                }
                for inst in &bb.insts {
                    if let InstKind::Copy { dst, src } = &inst.kind
                        && self.fresh.contains(src)
                    {
                        self.fresh.insert(*dst);
                    }
                }
            }
            if self.fresh.len() == before {
                return;
            }
        }
    }

    /// Whether `var` is an instance the function constructs (`Self::fresh`).
    fn is_fresh(&self, var: VarId) -> bool {
        self.fresh.contains(&var)
    }

    fn owner_of(&self, var: VarId) -> Owner {
        Owner {
            is_self: self.self_vars.contains(&var),
            param: self.param_alias.get(&var).cloned(),
            typed_class: self.project_class(var),
            may_params: self.may_params.get(&var).copied().unwrap_or(0),
            fresh: self.is_fresh(var),
        }
    }

    fn origin(&self, var: VarId) -> ArgOrigin {
        let field = self.obj_attr_alias.get(&var);
        ArgOrigin {
            param: self.param_alias.get(&var).cloned(),
            param_slot: self.param_slot_alias.get(&var).cloned(),
            self_attr: self.self_attr_alias.get(&var).cloned(),
            typed_field: field.map(|a| (a.class.clone(), a.attr.clone())),
            typed_instance: self.project_class(var),
            may_params: self.may_params.get(&var).copied().unwrap_or(0),
            fresh: self.is_fresh(var) || field.is_some_and(|a| a.fresh),
        }
    }

    /// String constant behind `var` (through `Copy`).
    fn const_str(&self, var: VarId) -> Option<String> {
        let mut cur = var;
        for _ in 0..16 {
            match self.defs.get(&cur)? {
                InstKind::Const {
                    literal: ConstLiteral::Str(s),
                    ..
                } => return Some(s.clone()),
                InstKind::Copy { src, .. } => cur = *src,
                _ => return None,
            }
        }
        None
    }

    fn run(mut self) -> (EffectSummary, Vec<CallSite>) {
        let f = self.f;
        (
            self.summary.consumes_params,
            self.summary.unconditional_forwards,
        ) = crate::consumption::param_consumption(f, self.cx, &self.types);
        for bb in &f.blocks {
            for inst in &bb.insts {
                self.visit(bb.id, &inst.kind, inst.span);
            }
        }
        self.summary.refresh_typed_field_writes();
        (self.summary, self.sites)
    }

    fn visit(&mut self, bb: BlockId, kind: &'a InstKind, span: Span) {
        match kind {
            InstKind::AttrLoad { dst, obj, attr } if attr != "__dict__" => {
                if self.self_vars.contains(obj) {
                    self.record_self_read(*dst, attr);
                }
                let owner = self.owner_of(*obj);
                self.typed_read(owner, attr, ReadKind::DirectAttr, span);
                self.property_read(bb, *dst, *obj, attr, span);
            }
            InstKind::SubscriptLoad {
                obj,
                key_const: Some(k),
                ..
            } => {
                // `obj.__dict__["attr"]`.
                if let Some(owner) = self.dict_of.get(obj).cloned() {
                    self.typed_read(owner, k, ReadKind::DictDirect, span);
                }
            }
            InstKind::AttrStore { obj, attr, value } => {
                self.write_attr(*obj, attr, WriteKind::DirectAttr, span);
                self.note_escape(*value);
            }
            InstKind::AttrDelete { obj, attr } => {
                if self.self_vars.contains(obj) {
                    self.summary.invalidates_cached.insert(attr.clone());
                }
                self.write_attr(*obj, attr, WriteKind::AttrDelete, span);
            }
            InstKind::SubscriptStore {
                obj,
                key_const,
                value,
                ..
            } => {
                self.subscript_write(*obj, key_const.as_deref(), false, span);
                self.note_escape(*value);
            }
            InstKind::SubscriptDelete { obj, key_const, .. } => {
                self.subscript_write(*obj, key_const.as_deref(), true, span);
            }
            InstKind::MethodCall {
                dst,
                recv,
                method,
                args,
                kwargs,
                is_awaited,
            } => {
                if crate::callgraph::is_private_name(method) {
                    self.summary.private_calls.insert(method.clone());
                }
                if CONTAINER_MUTATORS.contains(&method.as_str()) {
                    self.container_method(*recv, method, args, span);
                    for a in args {
                        if matches!(method.as_str(), "append" | "insert" | "add" | "setdefault") {
                            self.note_escape(*a);
                        }
                    }
                }
                // `obj.__dict__.get("attr"[, default])`.
                if method == "get"
                    && let Some(owner) = self.dict_of.get(recv).cloned()
                    && let Some(k) = args.first().and_then(|a| self.const_str(*a))
                {
                    self.typed_read(owner, &k, ReadKind::DictDirect, span);
                }
                let callee = self.cx.resolve_method(self.types.get(recv), method);
                let (recv_origin, recv_is_self) = self.receiver(*recv);
                self.call(
                    bb,
                    *dst,
                    callee,
                    Some(recv_origin),
                    args,
                    kwargs,
                    *is_awaited,
                    recv_is_self,
                    span,
                );
            }
            InstKind::Call {
                dst,
                callee_var,
                callee,
                args,
                kwargs,
                is_awaited,
            } => {
                let target = match callee_var {
                    Some(v) => self.cx.resolve_call(self.types.get(v)),
                    // Non-inlined comprehension (Python 3.11, `__comp_<function id>`) runs
                    // immediately.
                    None => callee
                        .strip_prefix("__comp_")
                        .and_then(|n| n.parse::<u32>().ok())
                        .map_or(Callee::Unresolved, |id| self.child_callee(FunctionId(id))),
                };
                self.call(
                    bb,
                    *dst,
                    target,
                    None,
                    args,
                    kwargs,
                    *is_awaited,
                    false,
                    span,
                );
            }
            // A generator expression's body runs where it is consumed — almost always
            // immediately in the creating frame (`sum(f(x) for x in xs)`).
            InstKind::MakeGenExpr {
                dst,
                func_id: Some(id),
                ..
            } => {
                let target = self.child_callee(*id);
                self.call(bb, *dst, target, None, &[], &[], false, false, span);
            }
            _ => {}
        }
    }

    /// The origin of a receiver bound to a method's first parameter, and whether it is the
    /// calling method's own `self`: `super()` binds the calling method's `self` (parameter 0).
    fn receiver(&self, recv: VarId) -> (ArgOrigin, bool) {
        let mut origin = self.origin(recv);
        let is_super = matches!(self.types.get(&recv), Some(Val::Super(_)));
        if is_super {
            origin.param = Some((0, None));
            origin.may_params = 1;
        }
        (origin, is_super || self.self_vars.contains(&recv))
    }

    /// Reading `obj.attr` runs the property getters it resolves to: a project call binding the
    /// receiver, at the read, awaited when the read value is awaited directly (an `async`
    /// getter returns a coroutine). The read's own facts (`reads_self_attrs`, typed read
    /// sites) stay as they are.
    fn property_read(&mut self, bb: BlockId, dst: VarId, obj: VarId, attr: &str, span: Span) {
        let Some(callee) = self.cx.resolve_attr_load(self.types.get(&obj), attr) else {
            return;
        };
        let (recv, recv_is_self) = self.receiver(obj);
        let is_awaited = self.uses.get(&dst).is_some_and(|uses| {
            uses.iter()
                .any(|u| matches!(u, InstKind::Await { value, .. } if *value == dst))
        });
        self.call(
            bb,
            dst,
            callee,
            Some(recv),
            &[],
            &[],
            is_awaited,
            recv_is_self,
            span,
        );
    }

    fn child_callee(&self, id: FunctionId) -> Callee {
        let child = self.cx.ir().function(id);
        Callee::Project(
            vec![FuncKey {
                module: self.key.module.clone(),
                qualname: func_qualname(child).to_owned(),
            }],
            false,
        )
    }

    fn record_self_read(&mut self, dst: VarId, attr: &str) {
        let uses = self.uses.get(&dst).map(Vec::as_slice).unwrap_or(&[]);
        let keys: Vec<Option<String>> = uses
            .iter()
            .map(|u| match u {
                InstKind::SubscriptLoad {
                    obj,
                    key_const: Some(k),
                    ..
                } if *obj == dst => Some(k.clone()),
                _ => None,
            })
            .collect();
        if !keys.is_empty() && keys.iter().all(Option::is_some) {
            for k in keys {
                self.summary.reads_self_attrs.insert(AttrKeyPath {
                    attr: attr.to_owned(),
                    const_key: k,
                });
            }
        } else {
            self.summary.reads_self_attrs.insert(AttrKeyPath {
                attr: attr.to_owned(),
                const_key: None,
            });
        }
    }

    fn note_escape(&mut self, value: VarId) {
        if let Some((i, None)) = self.param_alias.get(&value) {
            self.summary.escapes_params |= 1u64 << i;
        }
    }

    fn mutate_param(&mut self, i: u8, path: String) {
        self.summary.mutates_params |= 1u64 << i;
        self.summary
            .mutated_param_paths
            .entry(i)
            .or_default()
            .insert(path);
    }

    #[allow(clippy::too_many_arguments)]
    fn typed_site(
        &mut self,
        class: String,
        attr: &str,
        kind: WriteKind,
        span: Span,
        obj_params: u64,
        fresh: bool,
    ) {
        self.summary
            .typed_field_write_sites
            .insert(TypedFieldWriteSite {
                class_qualname: class,
                attr: attr.to_owned(),
                kind,
                span,
                via_chain: Vec::new(),
                obj_params,
                fresh,
            });
    }

    /// Read of attribute `attr` on the object `owner`, when it has a project class.
    fn typed_read(&mut self, owner: Owner, attr: &str, kind: ReadKind, span: Span) {
        if let Some(class) = owner.typed_class {
            self.summary
                .typed_field_read_sites
                .insert(TypedFieldReadSite {
                    class_qualname: class,
                    attr: attr.to_owned(),
                    kind,
                    obj_params: owner.may_params,
                    span,
                });
        }
    }

    /// Write of attribute `attr` on the object `owner` (AttrStore / AttrDelete / setattr / __dict__).
    fn write_owner_attr(&mut self, owner: Owner, attr: &str, kind: WriteKind, span: Span) {
        if owner.is_self {
            self.summary.mutates_self_attrs.insert(AttrKeyPath {
                attr: attr.to_owned(),
                const_key: None,
            });
        }
        match owner.param {
            Some((i, None)) => self.mutate_param(i, attr.to_owned()),
            Some((i, Some(path))) => self.mutate_param(i, path),
            None => {}
        }
        if let Some(class) = owner.typed_class {
            self.typed_site(class, attr, kind, span, owner.may_params, owner.fresh);
        }
    }

    fn write_attr(&mut self, obj: VarId, attr: &str, kind: WriteKind, span: Span) {
        let owner = self.owner_of(obj);
        self.write_owner_attr(owner, attr, kind, span);
    }

    /// In-place mutation of the container held in `var` (mutator method or subscript write).
    fn container_mut(&mut self, var: VarId, const_key: Option<&str>, span: Span) {
        if let Some(attr) = self.self_attr_alias.get(&var).cloned() {
            self.summary.mutates_self_attrs.insert(AttrKeyPath {
                attr,
                const_key: const_key.map(str::to_owned),
            });
        }
        match self.param_alias.get(&var).cloned() {
            Some((i, None)) => self.mutate_param(i, String::new()),
            Some((i, Some(path))) => self.mutate_param(i, path),
            None => {}
        }
        if let Some((i, slot)) = self.param_slot_alias.get(&var).cloned() {
            self.summary
                .inplace_param_slots
                .entry(i)
                .or_default()
                .insert(slot);
        }
        if let Some(a) = self.obj_attr_alias.get(&var).cloned() {
            let kind = if a.direct {
                WriteKind::ContainerMut
            } else {
                WriteKind::AliasContainerMut
            };
            self.typed_site(a.class, &a.attr, kind, span, a.obj_params, a.fresh);
        }
    }

    fn subscript_write(&mut self, obj: VarId, key: Option<&str>, is_delete: bool, span: Span) {
        if let Some(owner) = self.dict_of.get(&obj).cloned() {
            // `x.__dict__["a"] = v` / `del self.__dict__["prop"]`.
            if let Some(k) = key {
                if is_delete && owner.is_self {
                    self.summary.invalidates_cached.insert(k.to_owned());
                }
                self.write_owner_attr(owner, k, WriteKind::DictDirect, span);
            }
            return;
        }
        self.container_mut(obj, key, span);
    }

    fn container_method(&mut self, recv: VarId, method: &str, args: &[VarId], span: Span) {
        if let Some(owner) = self.dict_of.get(&recv).cloned() {
            match method {
                "clear" if owner.is_self => {
                    self.summary.invalidates_cached.insert("*".to_owned());
                }
                "pop" | "__delitem__" | "setdefault" | "__setitem__" => {
                    if let Some(k) = args.first().and_then(|a| self.const_str(*a)) {
                        if owner.is_self && matches!(method, "pop" | "__delitem__") {
                            self.summary.invalidates_cached.insert(k.clone());
                        }
                        self.write_owner_attr(owner, &k, WriteKind::DictDirect, span);
                    }
                }
                _ => {}
            }
            return;
        }
        self.container_mut(recv, None, span);
    }

    fn hop(&self, callee: String, span: Span) -> CallHop {
        CallHop {
            caller: self.key.clone(),
            callee,
            span,
            path: self.path.to_path_buf(),
        }
    }

    /// `drives_coroutine_manually`: the call result is `.send(...)`-driven or handed to a
    /// `run_sync`-style driver. Copies of an SSA variable form a tree (every variable has one
    /// definition), so the walk visits each copy once.
    fn drives_manually(&self, dst: VarId) -> bool {
        let mut vars = vec![dst];
        while let Some(v) = vars.pop() {
            for u in self.uses.get(&v).into_iter().flatten() {
                match u {
                    InstKind::Copy { dst, .. } => vars.push(*dst),
                    InstKind::MethodCall { recv, method, .. } if *recv == v && method == "send" => {
                        return true;
                    }
                    InstKind::Call { callee, args, .. } if args.contains(&v) => {
                        if callee
                            .rsplit('.')
                            .next()
                            .is_some_and(|n| n.ends_with("run_sync"))
                        {
                            return true;
                        }
                    }
                    InstKind::MethodCall { method, args, .. }
                        if args.contains(&v) && method.ends_with("run_sync") =>
                    {
                        return true;
                    }
                    _ => {}
                }
            }
        }
        false
    }

    #[allow(clippy::too_many_arguments)]
    fn call(
        &mut self,
        bb: BlockId,
        dst: VarId,
        callee: Callee,
        recv: Option<ArgOrigin>,
        args: &[VarId],
        kwargs: &[(Option<String>, VarId)],
        is_awaited: bool,
        recv_is_self: bool,
        span: Span,
    ) {
        let loop_depth = self.f.block_loop_depth(bb);
        match callee {
            Callee::External(symbol) => {
                self.external_call(symbol, args, is_awaited, loop_depth, span);
            }
            Callee::Project(keys, binds_recv) => {
                let mut callees = Vec::new();
                let mut any_async = false;
                for key in keys {
                    let decl = self
                        .cx
                        .project
                        .function_decl(&key)
                        .cloned()
                        .unwrap_or_default();
                    any_async |= decl.is_async;
                    let mut params: Vec<ArgOrigin> = Vec::new();
                    if binds_recv && !decl.is_staticmethod {
                        // Constructor calls bind a fresh object (`recv == None`).
                        params.push(if decl.is_classmethod {
                            ArgOrigin::default()
                        } else {
                            recv.clone().unwrap_or_default()
                        });
                    }
                    params.extend(args.iter().map(|a| self.origin(*a)));
                    for (name, v) in kwargs {
                        let Some(name) = name else { continue };
                        if let Some(pos) = decl.param_names.iter().position(|p| p == name) {
                            if params.len() <= pos {
                                params.resize(pos + 1, ArgOrigin::default());
                            }
                            params[pos] = self.origin(*v);
                        }
                    }
                    while params.last().is_some_and(ArgOrigin::is_empty) {
                        params.pop();
                    }
                    callees.push(BoundCallee {
                        callee: key,
                        params,
                    });
                }
                let drives = any_async && !is_awaited && self.drives_manually(dst);
                self.sites.push(CallSite {
                    caller: self.key.clone(),
                    target: CallTarget::Project(callees),
                    is_awaited,
                    span,
                    loop_depth,
                    recv_is_self,
                    drives_coroutine_manually: drives,
                });
            }
            Callee::TooMany | Callee::Unresolved => {
                self.summary.unknown_calls = true;
            }
        }
    }

    fn external_call(
        &mut self,
        symbol: String,
        args: &[VarId],
        is_awaited: bool,
        loop_depth: u16,
        span: Span,
    ) {
        if symbol == "builtins.setattr" || symbol == "builtins.object.__setattr__" {
            if let (Some(obj), Some(attr)) =
                (args.first(), args.get(1).and_then(|a| self.const_str(*a)))
            {
                self.write_attr(*obj, &attr, WriteKind::SetAttr, span);
            }
            return;
        }
        if symbol == "builtins.getattr" || symbol == "builtins.object.__getattribute__" {
            if let (Some(obj), Some(attr)) =
                (args.first(), args.get(1).and_then(|a| self.const_str(*a)))
            {
                let owner = self.owner_of(*obj);
                self.typed_read(owner, &attr, ReadKind::GetAttr, span);
            }
            return;
        }
        let Some(entry) = self.cx.kb.lookup(&symbol) else {
            return;
        };
        if entry.excluded {
            return;
        }
        // Stage 0-A filter: an awaited call never blocks the loop itself; loop-sensitive
        // effects (`DbQuery`, `UnpooledClientAlloc`, ...) are kept for EFF102 / EFF103.
        let effects: std::collections::BTreeSet<ExecEffectKind> = entry
            .effects
            .iter()
            .copied()
            .filter(|e| !(is_awaited && BLOCKING_EFFECTS.contains(e)))
            .collect();
        if let Some(k) = entry.offloads_blocking_arg
            && let Some((p, None)) = args
                .get(usize::from(k))
                .and_then(|a| self.param_alias.get(a))
        {
            self.summary.offloads_blocking_params |= 1u64 << p;
        }
        if effects.is_empty() && entry.offloads_blocking_arg.is_none() {
            return;
        }
        for e in &effects {
            self.summary.direct_exec_effects.insert(*e);
            let chain = vec![self.hop(symbol.clone(), span)];
            self.summary.exec_chains.insert(*e, chain);
        }
        self.sites.push(CallSite {
            caller: self.key.clone(),
            target: CallTarget::Layer2 {
                symbol,
                effects,
                offloads_blocking_arg: entry.offloads_blocking_arg,
                async_safe: entry.async_safe,
            },
            is_awaited,
            span,
            loop_depth,
            recv_is_self: false,
            drives_coroutine_manually: false,
        });
    }
}

/// Give `dst` the alias shared by every aliased phi input (if they all agree).
fn merge_phi<T: Clone + PartialEq>(
    map: &mut FxHashMap<VarId, T>,
    dst: VarId,
    inputs: &[VarId],
    conv: impl Fn(&T) -> T,
) {
    if map.contains_key(&dst) {
        return;
    }
    let mut found: Option<T> = None;
    for v in inputs {
        match (map.get(v), &found) {
            (Some(a), None) => found = Some(conv(a)),
            (Some(a), Some(f)) if conv(a) != *f => return,
            (None, _) => return,
            _ => {}
        }
    }
    if let Some(a) = found {
        map.insert(dst, a);
    }
}

/// [`merge_phi`] for `obj.<attr>` aliases: the inputs must name one class attribute; the
/// objects' parameters are united (the merged object may be any of them). Inputs are inserted
/// with their final parameters (`may_params` is complete before the alias pass), so the first
/// insertion is final too.
fn merge_obj_attr_phi(map: &mut FxHashMap<VarId, ObjAttr>, dst: VarId, inputs: &[VarId]) {
    if map.contains_key(&dst) {
        return;
    }
    let mut found: Option<ObjAttr> = None;
    for v in inputs {
        let Some(a) = map.get(v) else {
            return;
        };
        match &mut found {
            None => {
                found = Some(ObjAttr {
                    direct: false,
                    ..a.clone()
                });
            }
            Some(f) => {
                if f.class != a.class || f.attr != a.attr {
                    return;
                }
                f.obj_params |= a.obj_params;
                f.fresh &= a.fresh;
            }
        }
    }
    if let Some(a) = found {
        map.insert(dst, a);
    }
}

/// `map[var] |= bits`; whether it grew.
fn or_bits(map: &mut FxHashMap<VarId, u64>, var: VarId, bits: u64) -> bool {
    if bits == 0 {
        return false;
    }
    let slot = map.entry(var).or_default();
    let grown = *slot | bits != *slot;
    *slot |= bits;
    grown
}
