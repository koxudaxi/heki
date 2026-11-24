//! The references a function makes to project functions, methods, and classes without calling
//! them (`FileSummary::reference_sites`): the uses handing one on (`references = "calls"`), and
//! the other uses of its value (`references = "reads"`).

use super::{Extractor, Position, names_only};
use crate::CallSite;
use heki_ast::Span;
use heki_ir::walk::{Answer, Budget, Visit, reach};
use heki_ir::{DictEntry, InstKind, IrFunction, ModuleIr, Terminator, VarId};
use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::OnceCell;

impl Extractor<'_> {
    /// The function's references to a project function, method, or class
    /// ([`FileSummary::reference_sites`]): a load of one (a global, a name, an attribute, or a
    /// closure cell, which holds what the enclosing function binds to the name, a local import
    /// included), or the function a `def` or `lambda` makes, whose value reaches, through copies, phis, and the displays holding it, a use
    /// handing it on ([`Usage::HandsOn`]), or with [`Contracts::reference_reads`] another use
    /// ([`Usage::Reads`]), which a load handing it on is not. The value as a callee is a call (its
    /// own site), and so is a callable a library API runs (`Self::handed_callable`); a closure's
    /// captures, a binding of another name, and a deletion use nothing. A site is at the load; a
    /// name an import or the module body binds is read without a load of its own (the bound
    /// value), so its sites are at the uses taking it.
    ///
    /// [`FileSummary::reference_sites`]: crate::FileSummary::reference_sites
    /// [`Contracts::reference_reads`]: crate::Contracts::reference_reads
    pub(super) fn reference_sites(&mut self) -> Vec<CallSite> {
        let reads = self.contracts.is_some_and(|c| c.reference_reads());
        let flow = HandedValues::of(
            (self.f, self.cx.ir()),
            |at| self.callees.get(&at).is_some_and(names_only),
            &self.handed,
            reads,
        );
        // Past the budget every walk answers open and the function has no reference site.
        let budget = Budget::of_function("summary::reference_sites", self.f);
        let reaching = |taking| {
            flow.reaching(&budget, taking)
                .open_means(FxHashSet::default())
        };
        let handed = reaching(Taking::HandedOn);
        let used = if reads {
            reaching(Taking::Used)
        } else {
            FxHashSet::default()
        };
        let mut out = Vec::new();
        for &(at, var, span, imported) in &flow.loads {
            let (is_handed, is_used) = (handed.contains(&var), used.contains(&var));
            if !is_handed && !is_used {
                continue;
            }
            self.pos = at;
            let Some(site) = self.reference_site(var, at.0, span) else {
                continue;
            };
            let (handed_at, read_at) = if imported || flow.bound_by_module(var) {
                let taken = |taking| flow.taken_at(&budget, var, taking).open_means(Vec::new());
                let handed_at = taken(Taking::HandedOn);
                let mut read_at = if is_used {
                    taken(Taking::Used)
                } else {
                    Vec::new()
                };
                read_at.retain(|s| !handed_at.contains(s));
                (handed_at, read_at)
            } else if is_handed {
                (vec![span], Vec::new())
            } else {
                (Vec::new(), vec![span])
            };
            out.extend(handed_at.into_iter().map(|span| CallSite {
                span,
                ..site.clone()
            }));
            out.extend(read_at.into_iter().map(|span| CallSite {
                span,
                is_read: true,
                ..site.clone()
            }));
        }
        if budget.spent() { Vec::new() } else { out }
    }
}

/// How a use takes the object of a value ([`HandedValues::uses`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Usage {
    /// Handed on: an argument of a call that does more than inspect or name it ([`names_only`]),
    /// a stored attribute, item, or global (of a function), a `yield`, a `return`, or a `raise`.
    HandsOn,
    /// Bound to a name by the module body: the name is the object itself (an import, a module
    /// alias), and what a display holds is stored with it.
    BindsName,
    /// Used otherwise (kept only for [`Contracts::reference_reads`]): an argument of a call that
    /// only inspects or names it, the object of an attribute access or a method call, a
    /// subscript, an operand, a tested condition, an iterated, awaited, formatted, or `with`
    /// value, a `**` unpacking, and the values a statement uses without an instruction
    /// ([`ModuleIr::statement_uses`]), outside the annotations the function evaluates
    /// ([`ModuleIr::annotations`]).
    ///
    /// [`Contracts::reference_reads`]: crate::Contracts::reference_reads
    Reads,
}

/// How a display holds a value.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Held {
    /// An element, or a dict display's value.
    Value,
    /// A dict display's key (only for [`Contracts::reference_reads`]): read, never handed on.
    ///
    /// [`Contracts::reference_reads`]: crate::Contracts::reference_reads
    Key,
}

/// What a reference site does with its target's object.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Taking {
    HandedOn,
    /// Handed on, or read.
    Used,
}

impl Taking {
    /// Whether a use so made takes the object of the value it uses.
    fn takes(self, usage: Usage) -> bool {
        match self {
            Self::HandedOn => usage == Usage::HandsOn,
            Self::Used => usage != Usage::BindsName,
        }
    }

    /// Whether a use so made takes what a display it uses holds (a module binding a name to a
    /// display stores what the display holds).
    fn takes_held(self, usage: Usage) -> bool {
        match self {
            Self::HandedOn => usage != Usage::Reads,
            Self::Used => true,
        }
    }

    /// Whether a display taken passes on the values it holds so.
    fn passes(self, held: Held) -> bool {
        self == Self::Used || held == Held::Value
    }
}

/// How the values of a function reach the uses taking their objects
/// ([`Extractor::reference_sites`]).
struct HandedValues {
    /// Where each value comes from: the values it copies, or the phi inputs it merges.
    from: FxHashMap<VarId, Vec<VarId>>,
    /// The values each display holds.
    held: FxHashMap<VarId, Vec<(VarId, Held)>>,
    /// The uses of each value.
    uses: FxHashMap<VarId, Vec<(Span, Usage)>>,
    /// The loads: where, the value, its span, and whether it is an import.
    loads: Vec<(Position, VarId, Span, bool)>,
    /// The uses are kept for reads too ([`Usage::Reads`], [`Held::Key`]).
    reads: bool,
    /// The annotations the function evaluates, in order: an annotation is no read.
    annotations: Vec<Span>,
    /// Built on first use.
    onward: OnceCell<Onward>,
}

/// What each value flows to: a copy or phi (`None`), or a display holding it.
type Onward = FxHashMap<VarId, Vec<(VarId, Option<Held>)>>;

impl HandedValues {
    /// The flow of `f`'s values; `names_only_at` tells the calls that hand nothing on, and
    /// `handed` the callables library APIs run (`(call, argument)`), calls already. Given `f`'s
    /// module, the other uses are kept too, with the values its statements use without an
    /// instruction ([`ModuleIr::statement_uses`]).
    fn of(
        (f, ir): (&IrFunction, &ModuleIr),
        names_only_at: impl Fn(Position) -> bool,
        handed: &FxHashSet<(VarId, VarId)>,
        reads: bool,
    ) -> Self {
        let reads = reads.then_some(ir);
        let mut flow = Self {
            from: FxHashMap::default(),
            held: FxHashMap::default(),
            uses: FxHashMap::default(),
            loads: Vec::new(),
            reads: reads.is_some(),
            annotations: reads.map_or_else(Vec::new, |ir| ir.annotations_of(f.id).collect()),
            onward: OnceCell::new(),
        };
        let binding = if f.is_module_body {
            Usage::BindsName
        } else {
            Usage::HandsOn
        };
        for bb in &f.blocks {
            for phi in &bb.phis {
                let inputs = phi.incoming.iter().map(|(_, v)| *v);
                flow.from.entry(phi.dst).or_default().extend(inputs);
            }
            for (index, inst) in bb.insts.iter().enumerate() {
                let (at, span) = ((bb.id, index), inst.span);
                match &inst.kind {
                    InstKind::LoadGlobal { dst, .. }
                    | InstKind::LoadName { dst, .. }
                    | InstKind::LoadDeref { dst, .. } => flow.loads.push((at, *dst, span, false)),
                    InstKind::AttrLoad { dst, obj, .. } => {
                        flow.loads.push((at, *dst, span, false));
                        flow.add(*obj, span, Usage::Reads);
                    }
                    InstKind::Import { dst, .. } => flow.loads.push((at, *dst, span, true)),
                    InstKind::Copy { dst, src } => flow.from.entry(*dst).or_default().push(*src),
                    InstKind::AllocBuiltin { dst, elems, .. } => {
                        let elems = elems.iter().map(|v| (*v, Held::Value));
                        flow.held.entry(*dst).or_default().extend(elems);
                    }
                    InstKind::DictLiteral { dst, entries } => {
                        for e in entries {
                            match e {
                                DictEntry::KeyValue { key, value } => {
                                    flow.hold(*dst, *value, Held::Value);
                                    flow.hold(*dst, *key, Held::Key);
                                }
                                DictEntry::Unpack { dict } => flow.add(*dict, span, Usage::Reads),
                            }
                        }
                    }
                    InstKind::Call {
                        dst,
                        callee_var: Some(_),
                        args,
                        kwargs,
                        ..
                    }
                    | InstKind::MethodCall {
                        dst, args, kwargs, ..
                    } => {
                        if let InstKind::MethodCall { recv, .. } = &inst.kind {
                            flow.add(*recv, span, Usage::Reads);
                        }
                        let passed = args.iter().chain(kwargs.iter().map(|(_, v)| v));
                        if names_only_at(at) {
                            for v in passed {
                                flow.add(*v, span, Usage::Reads);
                            }
                        } else {
                            for v in passed.filter(|v| !handed.contains(&(*dst, **v))) {
                                flow.add(*v, span, Usage::HandsOn);
                            }
                        }
                    }
                    // A comprehension iterates its outermost iterable; its captures are its
                    // body's.
                    kind if let Some((iterable, _)) = kind.comprehension_operands() => {
                        flow.add(iterable, span, Usage::Reads);
                    }
                    InstKind::AttrStore { obj, value, .. } => {
                        flow.add(*value, span, Usage::HandsOn);
                        flow.add(*obj, span, Usage::Reads);
                    }
                    InstKind::SubscriptStore {
                        obj, index, value, ..
                    } => {
                        flow.add(*value, span, Usage::HandsOn);
                        flow.add(*obj, span, Usage::Reads);
                        flow.add(*index, span, Usage::Reads);
                    }
                    InstKind::Yield {
                        value: Some(value), ..
                    } => flow.add(*value, span, Usage::HandsOn),
                    InstKind::StoreGlobal { value, .. } => flow.add(*value, span, binding),
                    // A `def` or `lambda` makes the function its value refers to: in a module
                    // body, bound to its name as an import is, so read without a load of its own
                    // (`HANDLERS = [run]` after `def run`). A class statement's own uses are its
                    // bases. Its captures (the defaults and class keywords among them are
                    // statement uses) use nothing.
                    InstKind::MakeClosure {
                        dst,
                        func_id: Some(id),
                        ..
                    } if !ir.function(*id).is_class_body => {
                        flow.loads.push((at, *dst, span, f.is_module_body));
                    }
                    // A binding of another name, a deletion.
                    InstKind::StoreDeref { .. }
                    | InstKind::Delete { .. }
                    | InstKind::MakeClosure { .. } => {}
                    other => {
                        for v in other.operands() {
                            flow.add(v, span, Usage::Reads);
                        }
                    }
                }
            }
            // The builder gives every `return` and `raise` of a value its statement's span, and
            // every tested condition its own.
            match (&bb.terminator, bb.term_span) {
                (Terminator::Return(Some(v)) | Terminator::Raise(Some(v)), Some(span)) => {
                    flow.add(*v, span, Usage::HandsOn);
                }
                (Terminator::Branch { cond, .. }, Some(span)) => {
                    flow.add(*cond, span, Usage::Reads);
                }
                _ => {}
            }
        }
        for (value, span) in reads.into_iter().flat_map(|ir| ir.statement_uses_of(f.id)) {
            flow.add(value, span, Usage::Reads);
        }
        flow
    }

    fn add(&mut self, var: VarId, span: Span, usage: Usage) {
        let kept = usage != Usage::Reads || self.reads && !self.in_annotation(span);
        if kept {
            self.uses.entry(var).or_default().push((span, usage));
        }
    }

    /// Whether `span` lies inside an annotation the function evaluates.
    fn in_annotation(&self, span: Span) -> bool {
        let before = self.annotations.partition_point(|a| a.start <= span.start);
        before > 0 && span.end <= self.annotations[before - 1].end
    }

    fn hold(&mut self, display: VarId, var: VarId, held: Held) {
        if held == Held::Key && !self.reads {
            return;
        }
        self.held.entry(display).or_default().push((var, held));
    }

    /// Whether the module body binds a name to `var` itself.
    fn bound_by_module(&self, var: VarId) -> bool {
        self.uses
            .get(&var)
            .is_some_and(|uses| uses.iter().any(|(_, u)| *u == Usage::BindsName))
    }

    /// The values `display` holds that it passes on when `taking` takes it.
    fn passed(&self, display: VarId, taking: Taking) -> impl Iterator<Item = VarId> + '_ {
        let held = self.held.get(&display).into_iter().flatten();
        held.filter(move |(_, h)| taking.passes(*h))
            .map(|(v, _)| *v)
    }

    /// The values whose objects reach a use `taking` takes: through copies and phis, and as
    /// what a display reaching such a use holds. Open past `budget`.
    fn reaching(&self, budget: &Budget, taking: Taking) -> Answer<FxHashSet<VarId>> {
        let used_by = |pred: fn(Taking, Usage) -> bool| {
            self.uses
                .iter()
                .filter(move |(_, uses)| uses.iter().any(|(_, u)| pred(taking, *u)))
                .map(|(v, _)| *v)
        };
        let mut displays = Vec::new();
        let walked = reach(
            budget,
            used_by(Taking::takes_held),
            |v, out| {
                out.extend(self.from.get(&v).into_iter().flatten());
                out.extend(self.passed(v, taking));
            },
            |v| {
                displays.push(v);
                Visit::Expand::<()>
            },
        );
        let mut starts: Vec<VarId> = used_by(Taking::takes).collect();
        starts.extend(displays.iter().flat_map(|d| self.passed(*d, taking)));
        let mut out = FxHashSet::default();
        let reached = reach(
            budget,
            starts,
            |v, next| next.extend(self.from.get(&v).into_iter().flatten()),
            |v| {
                out.insert(v);
                Visit::Expand::<()>
            },
        );
        walked.zip(reached).map(|_| out)
    }

    /// Where `var`'s object is taken as `taking` takes it: the spans of the uses it reaches, in
    /// order. Open past `budget`.
    fn taken_at(&self, budget: &Budget, var: VarId, taking: Taking) -> Answer<Vec<Span>> {
        let onward = self.onward.get_or_init(|| {
            let mut onward = Onward::default();
            for (to, sources) in &self.from {
                for s in sources {
                    onward.entry(*s).or_default().push((*to, None));
                }
            }
            for (display, values) in &self.held {
                for (v, held) in values {
                    onward.entry(*v).or_default().push((*display, Some(*held)));
                }
            }
            onward
        });
        let mut out = Vec::new();
        let walked = reach(
            budget,
            [(var, false)],
            |(v, in_display), next| {
                for &(w, held) in onward.get(&v).into_iter().flatten() {
                    match held {
                        None => next.push((w, in_display)),
                        Some(h) if taking.passes(h) => next.push((w, true)),
                        Some(_) => {}
                    }
                }
            },
            |(v, in_display)| {
                let uses = self.uses.get(&v).into_iter().flatten();
                out.extend(
                    uses.filter(|(_, u)| taking.takes(*u) || in_display && taking.takes_held(*u))
                        .map(|(s, _)| *s),
                );
                Visit::Expand::<()>
            },
        );
        out.sort_unstable();
        out.dedup();
        walked.map(|_| out)
    }
}
