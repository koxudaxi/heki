//! `MUT112` (opt-in, `conditional-ownership-mutation`): a value that is a new object on one path
//! and borrowed state on another (a parameter, an attribute, a global, a variable of an enclosing
//! function, an element of one, or what a function returns of such state), then changed in
//! place. The change reaches the borrowed object on its paths only: `scopes = given or []`,
//! then `scopes.extend(...)`, changes the caller's list whenever it is not empty.
//!
//! What a value holds comes from `heki_effect::ownership`, applied forward over the function's
//! values: a phi joins its inputs, a project call's result is what its callees' summaries say
//! they return (`EffectSummary::returns_origins`, with the arguments bound to the parameters
//! they return), and a closure's variable of an enclosing function is what that function bound
//! to it. A new object the function stores in other state is that state's on every path. A
//! change is the function's own change of a container (an item stored or deleted, a container
//! method, an in-place operator) or a project callee's change of the container it is handed. A
//! value stored in `self.<attr>` is reported at the store when a method of the class family
//! changes that attribute in place.

use super::readonly_mutation::IN_PLACE;
use super::{
    CallInst, CallParts, ClassMethods, MUTABLE_BUILTINS, facts_run_at, is_memo_name, site_call,
};
use crate::{CheckContext, ModuleUnit};
use heki_ast::Span;
use heki_diagnostics::Diagnostic;
use heki_effect::ownership::{self, Holder, Place, Source};
use heki_effect::{
    CONTAINER_MUTATORS, CallHop, CallSite, Callee, EffectSummary, FuncKey, SiteRole, TypeCtx, Val,
    self_aliases,
};
use heki_ir::{
    BlockId, CompareOpKind, ConstLiteral, FunctionId, Inst, InstKind, IrFunction, Terminator,
    UnaryOpKind, VarId,
};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};

/// The help of a change in the function.
const COPY_FIRST: &str = "copy the borrowed value before changing it (`list(...)`, `dict(...)`, `.copy()`), so that every path changes an object of its own";
/// The help of a value stored in an attribute.
const STORE_A_COPY: &str = "store a copy of the borrowed value (`list(...)`, `dict(...)`, `.copy()`), so that the instance owns what its methods change";

pub(crate) fn mut112(cx: &CheckContext<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.selection.is_enabled("MUT112") {
        return;
    }
    debug_assert!(
        cx.index.propagates_typed_writes(),
        "`typed_writes` decides for MUT112"
    );
    let classes = ClassMethods::new(cx.units, cx.index);
    let found: Vec<Vec<Diagnostic>> = cx
        .units
        .par_iter()
        .filter(|unit| cx.enabled("MUT112", unit))
        .map(|unit| {
            let mut d = Vec::new();
            UnitCheck::new(cx, unit, &classes).run(&mut d);
            d
        })
        .collect();
    out.extend(found.into_iter().flatten());
}

/// What a value may borrow, for the message.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Borrow {
    Param(String),
    /// An attribute of the object `Receiver` names.
    Attr(String, Receiver),
    Global(String),
    Cell(String),
    Element(Box<Borrow>),
    /// What the project function (its qualname) returns.
    Returned(String),
}

impl Borrow {
    fn describe(&self) -> String {
        match self {
            Self::Param(n) => format!("the parameter `{n}`"),
            Self::Attr(a, _) => format!("the attribute `{a}`"),
            Self::Global(g) => format!("the global `{g}`"),
            Self::Cell(c) => format!("the enclosing function's `{c}`"),
            Self::Element(b) => format!("an element of {}", b.describe()),
            Self::Returned(q) => format!("what `{q}` returns"),
        }
    }

    /// An element of what this borrows (an element of an element is one too, so that a loop
    /// stepping into its own elements reaches a fixpoint).
    fn element(&self) -> Self {
        match self {
            Self::Element(_) => self.clone(),
            _ => Self::Element(Box::new(self.clone())),
        }
    }

    /// A memo or cache attribute, or an element of one: filling it is what it is for.
    fn is_memo(&self) -> bool {
        match self {
            Self::Attr(a, _) => is_memo_name(a),
            Self::Element(b) => b.is_memo(),
            _ => false,
        }
    }
}

/// The object an attribute belongs to. Each read of `self`, of a global, or of a variable of an
/// enclosing function is the same object; any other is the value read, through copies.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Receiver {
    MethodSelf,
    Global(String),
    Cell(String),
    Value(FunctionId, VarId),
}

/// A new object a value may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct New {
    /// Where it is made (the first found).
    at: Span,
    /// Every new object it may be starts empty (`[]`, `{}`, `dict()`): removing from it does
    /// nothing.
    empty: bool,
}

/// The objects a value may hold, path by path: the first found of each kind.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
struct Own {
    new: Option<New>,
    /// Borrowed state it may be.
    borrowed: Option<Borrow>,
    /// A parameter it may be when the caller passed one, where the function makes a new object
    /// only for `None` or a sentinel (`if p is None: p = []`): the accumulator idiom fills
    /// what the caller hands it, so this counts only when the value is also stored on `self`.
    defaulted: Option<Borrow>,
    /// What its elements borrow while it is new itself (`*args`, `**kwargs`).
    items: Option<Borrow>,
}

impl Own {
    fn new_at(at: Span, empty: bool) -> Self {
        Self {
            new: Some(New { at, empty }),
            ..Self::default()
        }
    }

    fn borrowing(b: Borrow) -> Self {
        Self {
            borrowed: Some(b),
            ..Self::default()
        }
    }

    /// What either value may hold.
    fn join(&self, other: &Self) -> Self {
        let new = match (self.new, other.new) {
            (Some(a), Some(b)) => Some(New {
                at: a.at,
                empty: a.empty && b.empty,
            }),
            (a, b) => a.or(b),
        };
        Self {
            new,
            borrowed: self.borrowed.clone().or_else(|| other.borrowed.clone()),
            defaulted: self.defaulted.clone().or_else(|| other.defaulted.clone()),
            items: self.items.clone().or_else(|| other.items.clone()),
        }
    }

    /// What an element of the value may hold: what the value borrows, below it.
    fn element(&self) -> Option<Self> {
        let below = self
            .borrowed
            .as_ref()
            .or(self.defaulted.as_ref())
            .or(self.items.as_ref())?;
        Some(Self::borrowing(below.element()))
    }

    /// A new object on one path and borrowed state on another.
    fn is_conditional(&self) -> bool {
        self.new.is_some() && (self.borrowed.is_some() || self.defaulted.is_some())
    }

    /// The value on a path where it is not the object `other` is: without what `other`
    /// borrows.
    fn other_than(&self, other: &Self) -> Self {
        let differs = |b: &Option<Borrow>| {
            b.clone().filter(|b| {
                other.borrowed.as_ref() != Some(b) && other.defaulted.as_ref() != Some(b)
            })
        };
        Self {
            borrowed: differs(&self.borrowed),
            defaulted: differs(&self.defaulted),
            ..self.clone()
        }
    }

    /// What the value borrows, for the message.
    fn borrow(&self) -> &Borrow {
        self.borrowed
            .as_ref()
            .or(self.defaulted.as_ref())
            .expect("a conditional value borrows")
    }
}

/// One module: its call sites by `(caller, span)`, and what each function binds to the
/// variables its closures capture.
struct UnitCheck<'a> {
    cx: &'a CheckContext<'a>,
    unit: &'a ModuleUnit<'a>,
    classes: &'a ClassMethods<'a>,
    tcx: TypeCtx<'a>,
    /// The module's call sites by caller qualname and span.
    sites: FxHashMap<&'a str, FxHashMap<Span, Vec<&'a CallSite>>>,
    /// Per closure, the enclosing function's values of the variables it captures.
    seeds: FxHashMap<FunctionId, FxHashMap<String, Own>>,
}

impl<'a> UnitCheck<'a> {
    fn new(
        cx: &'a CheckContext<'a>,
        unit: &'a ModuleUnit<'a>,
        classes: &'a ClassMethods<'a>,
    ) -> Self {
        let mut sites: FxHashMap<&str, FxHashMap<Span, Vec<&CallSite>>> = FxHashMap::default();
        let file = cx.index.files.get(&unit.input.path);
        for site in file.into_iter().flat_map(|f| &f.call_sites) {
            sites
                .entry(site.caller.qualname.as_str())
                .or_default()
                .entry(site.span)
                .or_default()
                .push(site);
        }
        Self {
            cx,
            unit,
            classes,
            tcx: TypeCtx::new(&unit.input, cx.decls, cx.kb),
            sites,
            seeds: FxHashMap::default(),
        }
    }

    fn run(mut self, out: &mut Vec<Diagnostic>) {
        let functions = &self.unit.input.ir.functions;
        for i in enclosing_first(functions) {
            let f = &functions[i];
            if f.is_class_body {
                continue;
            }
            let mut check = FnCheck::new(&self, f);
            check.propagate();
            check.report(out);
            let captured = check.captured();
            for (child, values) in captured {
                let seeds = self.seeds.entry(child).or_default();
                for (name, own) in values {
                    let joined = seeds
                        .get(&name)
                        .map_or_else(|| own.clone(), |o| o.join(&own));
                    seeds.insert(name, joined);
                }
            }
        }
    }
}

/// The positions of `functions`, every enclosing function before the functions it defines.
fn enclosing_first(functions: &[IrFunction]) -> Vec<usize> {
    let mut parent: FxHashMap<FunctionId, FunctionId> = FxHashMap::default();
    for f in functions {
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            if let InstKind::MakeClosure {
                func_id: Some(child),
                ..
            } = &inst.kind
            {
                parent.insert(*child, f.id);
            }
        }
    }
    // Only the function a definition is nested in makes its function: the walk ends at the
    // module body.
    let depth = |id: FunctionId| {
        let mut d = 0;
        let mut cur = id;
        while let Some(p) = parent.get(&cur) {
            d += 1;
            cur = *p;
        }
        d
    };
    let mut order: Vec<usize> = (0..functions.len()).collect();
    order.sort_by_key(|&i| depth(functions[i].id));
    order
}

/// A change in place of a value: where, the value, and the calls down to a helper making it.
struct Change {
    span: Span,
    var: VarId,
    via: Vec<CallHop>,
    /// The change only removes (`del x[k]`, `x.pop(k)`, `x.clear()`, ...): on an empty new
    /// object it does nothing.
    removes: bool,
}

/// Container methods that only remove.
const REMOVING_METHODS: &[&str] = &[
    "pop",
    "popitem",
    "remove",
    "discard",
    "clear",
    "__delitem__",
];

/// One function: the values of its variables, to a fixpoint.
struct FnCheck<'a, 'u> {
    u: &'u UnitCheck<'a>,
    f: &'a IrFunction,
    key: FuncKey,
    /// The function's call sites by span.
    sites: Option<&'u FxHashMap<Span, Vec<&'a CallSite>>>,
    types: FxHashMap<VarId, Val>,
    defs: FxHashMap<VarId, &'a Inst>,
    /// What the parameters hold when the function starts.
    initial: FxHashMap<VarId, Own>,
    own: FxHashMap<VarId, Own>,
    /// The method's `self` and its aliases.
    self_vars: FxHashSet<VarId>,
    /// The blocks a run can reach and the edges it can take.
    live: ownership::Live,
    preds: Vec<Vec<BlockId>>,
    /// Where the new objects the function stores in another object, a global, or a closure
    /// cell are made, and where they are stored: they are part of that state from then on, on
    /// every path.
    kept: FxHashMap<Span, Vec<Place<'a>>>,
}

impl<'a, 'u> FnCheck<'a, 'u> {
    fn new(u: &'u UnitCheck<'a>, f: &'a IrFunction) -> Self {
        let defs: FxHashMap<VarId, &'a Inst> = f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|i| i.kind.dst().map(|d| (d, i)))
            .collect();
        let is_method = f.owner_class_qualname.is_some() && !f.has_decorator(&["staticmethod"]);
        let receiver = f.params.first().filter(|_| is_method);
        let self_vars = receiver
            .filter(|_| !f.has_decorator(&["classmethod"]))
            .map(|p| self_aliases(f, p.var))
            .unwrap_or_default();
        let tested = identity_tested_params(f, &defs);
        let mut own = FxHashMap::default();
        for p in &f.params {
            if receiver.is_some_and(|r| r.var == p.var) {
                continue;
            }
            if p.is_vararg || p.is_kwarg {
                // A new tuple or dict on every call, holding the caller's objects.
                let items = Some(Borrow::Param(p.name.clone()));
                own.insert(
                    p.var,
                    Own {
                        items,
                        ..Own::new_at(f.span, false)
                    },
                );
            } else if tested.contains(&p.var) {
                let defaulted = Some(Borrow::Param(p.name.clone()));
                own.insert(
                    p.var,
                    Own {
                        defaulted,
                        ..Own::default()
                    },
                );
            } else {
                own.insert(p.var, Own::borrowing(Borrow::Param(p.name.clone())));
            }
        }
        let live = ownership::Live::of(f, &defs);
        let key = CheckContext::key(u.unit, f);
        Self {
            sites: u.sites.get(key.qualname.as_str()),
            key,
            types: u.tcx.infer(f),
            u,
            f,
            defs,
            initial: own.clone(),
            own,
            self_vars,
            live,
            preds: f.predecessors(),
            kept: FxHashMap::default(),
        }
    }

    fn sites_at(&self, span: Span) -> &[&'a CallSite] {
        self.sites
            .and_then(|by_span| by_span.get(&span))
            .map_or(&[], Vec::as_slice)
    }

    /// The parts of a call instruction: a project call as the summaries resolved it, any other
    /// call by the inferred values.
    fn call_of(&self, inst: &'a Inst) -> Option<CallParts<'a>> {
        if let Some(call) = site_call(self.f, self.sites_at(inst.span), inst, false) {
            return Some(call);
        }
        let call = CallInst::of(&inst.kind)?;
        Some(CallParts {
            callee: call.callee(&self.u.tcx, &self.types),
            recv: call.recv,
            args: call.args,
            kwargs: call.kwargs,
            site: None,
        })
    }

    /// The property getter a read of `obj.<attr>` calls.
    fn getter_call(&self, inst: &'a Inst) -> Option<CallParts<'a>> {
        let InstKind::AttrLoad { obj, attr, .. } = &inst.kind else {
            return None;
        };
        let site = self.sites_at(inst.span).iter().find(|s| {
            s.role == SiteRole::Call
                && s.runner == heki_effect::Runner::Caller
                && !s.project_callees().is_empty()
        })?;
        Some(CallParts {
            recv: Some((*obj, attr.as_str())),
            args: &[],
            kwargs: &[],
            callee: Callee::Project(
                site.project_callees()
                    .iter()
                    .map(|bc| bc.callee.clone())
                    .collect(),
                site.binds_recv,
            ),
            site: Some(site),
        })
    }

    fn source(&self, inst: &'a Inst) -> Source<'a> {
        let kb = self.u.cx.kb;
        match &inst.kind {
            InstKind::AttrLoad { .. } if self.getter_call(inst).is_some() => Source::Project,
            InstKind::Call { .. } | InstKind::MethodCall { .. } => {
                let constructs = self
                    .sites_at(inst.span)
                    .iter()
                    .any(|s| s.constructs.is_some());
                let callee = self.call_of(inst).map(|c| c.callee);
                ownership::source(&inst.kind, callee.as_ref(), constructs, kb)
            }
            kind => ownership::source(kind, None, false, kb),
        }
    }

    fn holder(&self, h: Holder<'_>) -> Borrow {
        match h {
            Holder::Attr { obj, attr } => Borrow::Attr(attr.to_owned(), self.receiver(obj)),
            Holder::NamedAttr { obj, name } => {
                let attr = match self.defs.get(&name).map(|i| &i.kind) {
                    Some(InstKind::Const {
                        literal: ConstLiteral::Str(s),
                        ..
                    }) => s.clone(),
                    _ => "?".to_owned(),
                };
                Borrow::Attr(attr, self.receiver(obj))
            }
            Holder::Global(g) => Borrow::Global(g.to_owned()),
            Holder::Cell(c) => Borrow::Cell(c.to_owned()),
        }
    }

    /// The object whose attributes `obj` reads or stores.
    fn receiver(&self, obj: VarId) -> Receiver {
        if self.self_vars.contains(&obj) {
            return Receiver::MethodSelf;
        }
        let root = self.copied_from(obj);
        match self.defs.get(&root).map(|i| &i.kind) {
            Some(InstKind::LoadGlobal { name, .. }) => Receiver::Global(name.clone()),
            Some(InstKind::LoadDeref { name, .. }) => Receiver::Cell(name.clone()),
            _ => Receiver::Value(self.f.id, root),
        }
    }

    /// The value `v` copies, through any number of copies (`v` itself when it is no copy).
    fn copied_from(&self, v: VarId) -> VarId {
        let mut cur = v;
        while let Some(InstKind::Copy { src, .. }) = self.defs.get(&cur).map(|i| &i.kind) {
            cur = *src;
        }
        cur
    }

    /// What a project call's result holds: what its callees return, with the arguments bound
    /// to the parameters they return.
    fn returned(&self, inst: &'a Inst) -> Option<Own> {
        let call = self.getter_call(inst).or_else(|| self.call_of(inst))?;
        let site = call.site?;
        let mut out: Option<Own> = None;
        let mut add = |o: &Own| out = Some(out.as_ref().map_or_else(|| o.clone(), |a| a.join(o)));
        for bc in site.project_callees() {
            let r = self
                .u
                .cx
                .index
                .summary(&bc.callee)
                .map(|s| s.returns_origins)
                .unwrap_or_default();
            if r.new {
                add(&Own::new_at(inst.span, false));
            }
            if r.held {
                add(&Own::borrowing(Borrow::Returned(
                    bc.callee.qualname.clone(),
                )));
            }
        }
        for (_, s, pos, var) in call.bound_arguments(self.u.cx) {
            let Some(o) = self.own.get(&var).filter(|_| pos < 64) else {
                continue;
            };
            let bit = 1u64 << pos;
            if s.returns_origins.params & bit != 0 {
                add(o);
            }
            if s.returns_origins.elements & bit != 0
                && let Some(e) = o.element()
            {
                add(&e);
            }
        }
        out
    }

    /// What the value `inst` defines holds, from its operands': a new object it makes that the
    /// function stores somewhere is part of where it is stored (an element of that object).
    fn transfer(&self, inst: &'a Inst) -> Option<Own> {
        let mut out = self.derive(inst)?;
        if let Some(places) = out.new.and_then(|n| self.kept.get(&n.at)) {
            out.new = None;
            for place in places {
                let part = match place {
                    Place::Object(c) => self.own.get(c).and_then(Own::element),
                    Place::Global(g) => Some(Own::borrowing(self.holder(Holder::Global(g)))),
                    Place::Cell(c) => Some(Own::borrowing(self.holder(Holder::Cell(c)))),
                };
                out = part.map_or(out.clone(), |p| out.join(&p));
            }
        }
        (out != Own::default()).then_some(out)
    }

    fn derive(&self, inst: &'a Inst) -> Option<Own> {
        let own = |v: &VarId| self.own.get(v);
        match self.source(inst) {
            Source::New => Some(Own::new_at(inst.span, starts_empty(&inst.kind))),
            Source::Held(Holder::Cell(name)) => Some(self.captured_value(name)),
            Source::Held(h) => Some(Own::borrowing(self.holder(h))),
            Source::Same(v) => own(&v).cloned(),
            Source::Element(v) => own(&v)?.element(),
            Source::ElementOr(v, default) => {
                join_opt(own(&v).and_then(Own::element), own(&default).cloned())
            }
            Source::HeldOr(h, default) => Some(
                Own::borrowing(self.holder(h)).join(&own(&default).cloned().unwrap_or_default()),
            ),
            Source::Slice(v) => {
                let class = match self.types.get(&v) {
                    Some(Val::Inst(c)) => Some(c.as_str()),
                    _ => None,
                };
                if ownership::slice_copies(class) {
                    Some(Own::new_at(inst.span, false))
                } else {
                    own(&v).cloned()
                }
            }
            Source::Project => self.returned(inst),
            Source::Unknown => None,
        }
    }

    /// A variable of an enclosing function: what that function bound to it where it made the
    /// closure, or state that outlives the closure's calls.
    fn captured_value(&self, name: &str) -> Own {
        self.u
            .seeds
            .get(&self.f.id)
            .and_then(|s| s.get(name))
            .filter(|o| **o != Own::default())
            .cloned()
            .unwrap_or_else(|| Own::borrowing(self.holder(Holder::Cell(name))))
    }

    /// The values of the function's variables ([`Self::fixpoint`]), once more without the new
    /// objects the function stores somewhere ([`Self::kept`]).
    fn propagate(&mut self) {
        self.fixpoint();
        // A stored object the function makes there (through copies): a value joining several
        // new objects at a phi names only the first of them.
        let mut kept: FxHashMap<Span, Vec<Place<'a>>> = FxHashMap::default();
        for inst in self.live_insts() {
            for (v, place) in ownership::stores(&inst.kind) {
                let root = self.copied_from(v);
                if let (Some(def), Some(o)) = (self.defs.get(&root), self.own.get(&root))
                    && o.new.is_some_and(|n| n.at == def.span)
                    && o.borrowed.is_none()
                    && o.defaulted.is_none()
                    && o.items.is_none()
                {
                    kept.entry(def.span).or_default().push(place);
                }
            }
        }
        if !kept.is_empty() {
            self.kept = kept;
            self.own = self.initial.clone();
            self.fixpoint();
        }
    }

    /// The values of the function's variables, to a fixpoint: a phi joins its inputs, and a
    /// value is derived again when an operand's grows. A value only grows (each kind is set
    /// once), so the loop ends.
    fn fixpoint(&mut self) {
        let rpo: Vec<BlockId> = self
            .f
            .rpo_blocks()
            .into_iter()
            .filter(|b| self.live.block(*b))
            .collect();
        loop {
            let mut changed = false;
            for &bb in &rpo {
                let block = self.f.block(bb);
                for phi in &block.phis {
                    let mut joined: Option<Own> = None;
                    for &(from, v) in &phi.incoming {
                        if v == phi.dst || !self.live.edge(from, bb) {
                            continue;
                        }
                        joined = join_opt(joined, self.incoming(from, bb, v));
                    }
                    if let Some(o) = joined {
                        changed |= self.grow(phi.dst, o);
                    }
                }
                for inst in &block.insts {
                    let Some(dst) = inst.kind.dst() else {
                        continue;
                    };
                    if let Some(o) = self.transfer(inst) {
                        changed |= self.grow(dst, o);
                    }
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// The value `v` brings into a phi of `block` from `from`: on the edge where an `is` test of
    /// it against another object fails (`if result is shared: result = shared.copy()`), it is
    /// not that object.
    fn incoming(&self, from: BlockId, block: BlockId, v: VarId) -> Option<Own> {
        let own = self.own.get(&v)?;
        let Some((a, b)) = self.failed_identity(from, block) else {
            return Some(own.clone());
        };
        let root = self.copied_from(v);
        let other = if self.copied_from(a) == root {
            b
        } else if self.copied_from(b) == root {
            a
        } else {
            return Some(own.clone());
        };
        Some(match self.own.get(&other) {
            Some(o) => own.other_than(o),
            None => own.clone(),
        })
    }

    /// The operands of the `is` comparison that is false on the edge `from -> to`: the branch
    /// ends `from`, or an empty block `from` that is the side of the branch that it leads to.
    fn failed_identity(&self, from: BlockId, to: BlockId) -> Option<(VarId, VarId)> {
        let test = |at: BlockId, side: BlockId| {
            let Terminator::Branch {
                cond,
                then_bb,
                else_bb,
            } = self.f.block(at).terminator
            else {
                return None;
            };
            let (mut cur, mut negated) = (cond, false);
            for _ in 0..8 {
                match self.defs.get(&cur).map(|i| &i.kind)? {
                    InstKind::UnaryOp {
                        op: UnaryOpKind::Not,
                        operand,
                        ..
                    } => {
                        negated = !negated;
                        cur = *operand;
                    }
                    InstKind::Copy { src, .. } => cur = *src,
                    InstKind::Compare { op, lhs, rhs, .. } => {
                        let is = match op {
                            CompareOpKind::Is => !negated,
                            CompareOpKind::IsNot => negated,
                            _ => return None,
                        };
                        let fails = if is { else_bb } else { then_bb };
                        return (fails == side).then_some((*lhs, *rhs));
                    }
                    _ => return None,
                }
            }
            None
        };
        test(from, to).or_else(|| {
            let b = self.f.block(from);
            match (self.preds[from.0 as usize].as_slice(), &b.terminator) {
                ([at], Terminator::Jump(next)) if b.insts.is_empty() && *next == to => {
                    test(*at, from)
                }
                _ => None,
            }
        })
    }

    /// Join `o` into the value of `var`; whether it grew.
    fn grow(&mut self, var: VarId, o: Own) -> bool {
        let next = match self.own.get(&var) {
            Some(old) => old.join(&o),
            None => o,
        };
        let grew = self.own.get(&var) != Some(&next);
        if grew {
            self.own.insert(var, next);
        }
        grew
    }

    /// The instructions of the blocks a run can reach.
    fn live_insts(&self) -> impl Iterator<Item = &'a Inst> + '_ {
        self.f
            .blocks
            .iter()
            .filter(|b| self.live.block(b.id))
            .flat_map(|b| &b.insts)
    }

    /// The values the function's closures capture, by closure and variable name.
    fn captured(&self) -> Vec<(FunctionId, Vec<(String, Own)>)> {
        let mut out = Vec::new();
        for inst in self.live_insts() {
            let InstKind::MakeClosure {
                func_id: Some(child),
                captured,
                ..
            } = &inst.kind
            else {
                continue;
            };
            let values: Vec<(String, Own)> = captured
                .iter()
                .filter_map(|v| Some((self.f.var_name(*v)?.to_owned(), self.own.get(v)?.clone())))
                .collect();
            if !values.is_empty() {
                out.push((*child, values));
            }
        }
        out
    }

    /// The function's own in-place changes of a container at `inst` (an item stored or deleted,
    /// a container method, an in-place operator), and the project calls that change the
    /// container they are handed itself (not a value below it).
    fn changes(&self, inst: &'a Inst) -> Vec<Change> {
        let own = |var: VarId, removes: bool| Change {
            span: inst.span,
            var,
            via: Vec::new(),
            removes,
        };
        match &inst.kind {
            InstKind::SubscriptStore { obj, .. } => return vec![own(*obj, false)],
            InstKind::SubscriptDelete { obj, .. } => return vec![own(*obj, true)],
            InstKind::BinOp {
                lhs,
                in_place: true,
                ..
            } => {
                let mutable = matches!(self.types.get(lhs), Some(Val::Inst(t)) if MUTABLE_BUILTINS.contains(&t.as_str()));
                return if mutable {
                    vec![own(*lhs, false)]
                } else {
                    Vec::new()
                };
            }
            _ => {}
        }
        let Some(call) = site_call(self.f, self.sites_at(inst.span), inst, false) else {
            return match &inst.kind {
                InstKind::MethodCall { recv, method, .. }
                    if CONTAINER_MUTATORS.contains(&method.as_str()) =>
                {
                    vec![own(*recv, REMOVING_METHODS.contains(&method.as_str()))]
                }
                _ => Vec::new(),
            };
        };
        let site = call.site.expect("`site_call` names the call site");
        let mut out = Vec::new();
        for (key, callee, pos, var) in call.bound_arguments(self.u.cx) {
            let changes = facts_run_at(self.u.cx.index, site, callee)
                .is_some_and(|facts| changes_itself(facts, pos));
            if changes && self.own.get(&var).is_some_and(Own::is_conditional) {
                out.push(Change {
                    span: inst.span,
                    var,
                    via: vec![CallHop {
                        caller: self.key.clone(),
                        callee: key.fqn(),
                        span: inst.span,
                        path: self.u.unit.input.path.clone(),
                    }],
                    removes: false,
                });
            }
        }
        out
    }

    /// A value stored in `self.<attr>` at `inst`.
    fn self_store(&self, inst: &'a Inst) -> Option<(&'a str, &Own)> {
        match &inst.kind {
            InstKind::AttrStore { obj, attr, value } if self.self_vars.contains(obj) => {
                Some((attr.as_str(), self.own.get(value)?))
            }
            _ => None,
        }
    }

    /// A conditional value stored back where it borrows from, the same attribute of the same
    /// object or an item of the same container (`self.items = self.items or []`,
    /// `d[k] = d.get(k) or []`): that state holds it on every path.
    fn settled(&self, inst: &'a Inst) -> Option<&Own> {
        let (value, place) = match &inst.kind {
            InstKind::AttrStore { obj, attr, value } => {
                (value, self.holder(Holder::Attr { obj: *obj, attr }))
            }
            InstKind::SubscriptStore { obj, value, .. } => {
                (value, self.own.get(obj)?.element()?.borrowed?)
            }
            InstKind::Call { callee, args, .. } if callee == "setattr" && args.len() == 3 => {
                let held = Holder::NamedAttr {
                    obj: args[0],
                    name: args[1],
                };
                (&args[2], self.holder(held))
            }
            _ => return None,
        };
        let o = self.own.get(value)?;
        (o.borrowed.as_ref() == Some(&place)).then_some(o)
    }

    fn report(&self, out: &mut Vec<Diagnostic>) {
        let mut stores: Vec<(Span, &'a str, &Own)> = Vec::new();
        let mut changes: Vec<(Change, &Own)> = Vec::new();
        let mut settled: FxHashSet<&Own> = FxHashSet::default();
        for inst in self.live_insts() {
            if let Some((attr, o)) = self.self_store(inst).filter(|(_, o)| o.is_conditional()) {
                stores.push((inst.span, attr, o));
            }
            settled.extend(self.settled(inst));
            for change in self.changes(inst) {
                if let Some(o) = self.own.get(&change.var).filter(|o| o.is_conditional()) {
                    changes.push((change, o));
                }
            }
        }
        let stored: FxHashSet<&Own> = stores.iter().map(|(_, _, o)| *o).collect();
        // One report per value, at its first change; a settled value or a memo is never one.
        changes.sort_by_key(|(c, _)| (c.span.start, c.span.end));
        let mut reported: FxHashSet<&Own> = settled.clone();
        for (change, o) in changes {
            // Removing from an empty new object does nothing: the paths agree. A conditional
            // expression changed in place (`(a if c else b).append(x)`) picks the container on
            // purpose.
            let picked = self
                .f
                .var_name(change.var)
                .is_some_and(|n| n.starts_with(".ifexpr"));
            if o.borrowed.is_none() && !stored.contains(o)
                || o.borrow().is_memo()
                || change.removes && o.new.is_some_and(|n| n.empty)
                || picked
                || !reported.insert(o)
            {
                continue;
            }
            let what = match self.f.var_name(change.var) {
                Some(n) if !n.starts_with('.') => format!("`{n}`"),
                _ => "a value".to_owned(),
            };
            self.u.cx.emit(
                out,
                "MUT112",
                self.u.unit,
                change.span,
                &self.key.qualname,
                what.trim_matches('`').to_owned(),
                format!(
                    "`{}` changes {what} in place, which is a new object on one path and {} on another",
                    self.key.qualname,
                    o.borrow().describe()
                ),
                Some(COPY_FIRST.to_owned()),
                &change.via,
            );
        }
        stores.sort_by_key(|(span, _, _)| (span.start, span.end));
        for (span, attr, o) in stores {
            if reported.contains(o) || is_memo_name(attr) || o.borrow().is_memo() {
                continue;
            }
            let Some((changer, related)) = self.changer_of(attr) else {
                continue;
            };
            reported.insert(o);
            if let Some(d) = self.u.cx.emit(
                out,
                "MUT112",
                self.u.unit,
                span,
                &self.key.qualname,
                format!("self.{attr}"),
                format!(
                    "`{}` stores in `self.{attr}` a value that is a new object on one path and {} on another, and `{changer}` changes it in place",
                    self.key.qualname,
                    o.borrow().describe()
                ),
                Some(STORE_A_COPY.to_owned()),
                &[],
            ) {
                d.related.extend(related);
            }
        }
    }

    /// The first method of the class family (and where it does it) that changes the object
    /// `self.<attr>` holds in place, directly or through the functions it calls.
    fn changer_of(
        &self,
        attr: &str,
    ) -> Option<(String, Option<heki_diagnostics::RelatedLocation>)> {
        let cx = self.u.cx;
        let class = format!(
            "{}.{}",
            self.u.unit.input.module_name,
            self.f.owner_class_qualname.as_deref()?
        );
        let mut related: FxHashSet<String> = cx.decls.subclasses_of(&class).into_iter().collect();
        related.extend(cx.decls.ancestors_of(&class));
        related.insert(class.clone());
        let in_place = |s: &EffectSummary| {
            s.typed_field_writes.iter().any(|w| {
                w.attr == attr && IN_PLACE.contains(&w.kind) && related.contains(&w.class_qualname)
            })
        };
        let mut family = self.u.classes.family(cx.decls, &class);
        family.sort_by(|a, b| a.key.cmp(&b.key));
        let m = family
            .into_iter()
            .find(|m| cx.index.summary(&m.key).is_some_and(in_place))?;
        let site = cx
            .index
            .files
            .get(m.path)
            .and_then(|f| f.local_functions.get(&m.key))
            .and_then(|s| {
                s.typed_field_write_sites.iter().find(|w| {
                    w.attr == attr
                        && IN_PLACE.contains(&w.kind)
                        && related.contains(&w.class_qualname)
                })
            })
            .map(|w| {
                cx.related_at(
                    m.path,
                    w.span,
                    format!("`{}` changes `self.{attr}` in place", m.key.qualname),
                )
            });
        Some((m.key.qualname.clone(), site))
    }
}

fn join_opt(a: Option<Own>, b: Option<Own>) -> Option<Own> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.join(&b)),
        (a, b) => a.or(b),
    }
}

/// Whether the facts of a callee change the container bound to its parameter `pos` in place
/// (`mutated_param_paths` entry `""`), not only an attribute or the objects below it.
fn changes_itself(facts: &EffectSummary, pos: usize) -> bool {
    u8::try_from(pos)
        .ok()
        .and_then(|p| facts.mutated_param_paths.get(&p))
        .is_some_and(|paths| paths.contains(""))
}

/// The parameters (their variables) that an `is` / `is not` comparison deciding a branch tests
/// against a constant, a global, or an attribute, also through copies and phis: `if out is None:
/// out = []` replaces such a parameter with a new object only when the caller passes none.
fn identity_tested_params(f: &IrFunction, defs: &FxHashMap<VarId, &Inst>) -> FxHashSet<VarId> {
    let sentinel = |v: &VarId| {
        matches!(
            defs.get(v).map(|i| &i.kind),
            Some(InstKind::Const { .. } | InstKind::LoadGlobal { .. } | InstKind::AttrLoad { .. })
        )
    };
    let mut tested: Vec<VarId> = Vec::new();
    for b in &f.blocks {
        let Terminator::Branch { cond, .. } = b.terminator else {
            continue;
        };
        let mut cur = cond;
        for _ in 0..8 {
            match defs.get(&cur).map(|i| &i.kind) {
                Some(InstKind::UnaryOp {
                    op: UnaryOpKind::Not,
                    operand,
                    ..
                }) => cur = *operand,
                Some(InstKind::Copy { src, .. }) => cur = *src,
                Some(InstKind::Compare {
                    op: CompareOpKind::Is | CompareOpKind::IsNot,
                    lhs,
                    rhs,
                    ..
                }) => {
                    if sentinel(rhs) {
                        tested.push(*lhs);
                    }
                    if sentinel(lhs) {
                        tested.push(*rhs);
                    }
                    break;
                }
                _ => break,
            }
        }
    }
    let phis: FxHashMap<VarId, &heki_ir::PhiNode> = f
        .blocks
        .iter()
        .flat_map(|b| &b.phis)
        .map(|p| (p.dst, p))
        .collect();
    let params: FxHashSet<VarId> = f.params.iter().map(|p| p.var).collect();
    let mut seen: FxHashSet<VarId> = FxHashSet::default();
    let mut out = FxHashSet::default();
    while let Some(v) = tested.pop() {
        if !seen.insert(v) {
            continue;
        }
        if params.contains(&v) {
            out.insert(v);
        } else if let Some(phi) = phis.get(&v) {
            tested.extend(phi.incoming.iter().map(|(_, x)| *x));
        } else if let Some(InstKind::Copy { src, .. }) = defs.get(&v).map(|i| &i.kind) {
            tested.push(*src);
        }
    }
    out
}

/// Whether the new object `kind` makes starts empty: an empty display, or a container
/// constructor called without arguments (`defaultdict(list)` with its factory only).
fn starts_empty(kind: &InstKind) -> bool {
    match kind {
        InstKind::AllocBuiltin { elems, .. } => elems.is_empty(),
        InstKind::DictLiteral { entries, .. } => entries.is_empty(),
        InstKind::Call {
            callee,
            args,
            kwargs,
            ..
        } => {
            let name = callee.rsplit('.').next().unwrap_or(callee);
            kwargs.is_empty()
                && match name {
                    "list" | "dict" | "set" | "OrderedDict" | "Counter" | "deque" => {
                        args.is_empty()
                    }
                    "defaultdict" => args.len() <= 1,
                    _ => false,
                }
        }
        _ => false,
    }
}
