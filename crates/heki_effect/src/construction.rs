//! Objects under construction (`fresh-objects`, docs/configuration.md).
//!
//! A write constructs the object it writes when the object was allocated in the same call
//! chain — by calling its class, by copying, or by a project function returning a new object —
//! or is a parameter of the writing function, and nothing may hold it besides the frame yet: no
//! store, yield, closure, or call that may keep it ran before the write. The extractor records,
//! in source order, the allocations, the escapes (a definite one, or a hand-over to a project
//! callee's parameter, which keeps the object only if that callee does), and the points it asks
//! about (writes, call arguments, returns); [`Recorder::resolve`] runs one forward data flow over
//! the function's blocks (kills at allocations, unions at joins and exception edges) and answers
//! each question with a [`NewArg`]: whether the variable holds a new object (or a parameter) that
//! nothing kept so far, under conditions on the callees, which propagation resolves.
//!
//! Each escape comes with the tests of the function's inputs that hold where it runs: a store
//! under `if register:` may keep the object only where `register` is true, a condition the
//! answer carries ([`Keep`]) and propagation turns into a fact of the function that each of its
//! calls decides (`MayGiven`).
//!
//! An attribute store of a new object into another new object, of a class whose attribute write
//! runs no code, links the two instead of keeping the value ([`Escape::Into`]): objects the
//! function made and stored in one another stay new until one of them is kept. Keeping an object
//! keeps every object it holds through such links, and keeping a value read out of an object
//! (an attribute, an item, an iteration step, what a call over it returns) keeps what that
//! object holds, which may be the objects holding it ([`Links`]). The result of a project
//! function holding itself that way is `ReturnsNew::New::linked`: a value read out of it at a
//! call may be the result.
//!
//! A returned value may also be an element of a parameter (an item, `get`, an iteration step of
//! it) on some paths: the function returns a new object at the calls binding such a parameter to
//! a value with no elements, where those paths raise or never reach their `return`
//! (`elements_of` of [`ReturnsNew::New`]).

use crate::readonly::ContainerKind;
use crate::{
    CallTests, FuncKey, InputGuard, Keep, KeptBy, MayGiven, NewArg, NewCond, NewFrom, NewKind,
    ReturnsNew,
};
use heki_ir::walk::{Answer, Budget, Closure, Def, Facts, FnShape, Join, Or, Sparse, fixpoint};
use heki_ir::{InstKind, VarId};
use rustc_hash::FxHashMap;

/// Position of an instruction: block, index in the block (`insts.len()` for the terminator).
pub(crate) type Pos = (u32, u32);

/// An escape of a variable's object at an instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Escape {
    /// Stored, yielded, captured, or handed to code heki does not follow.
    Definite,
    /// Handed to a project callee's parameter (the call site and callee indexes of the
    /// function, the parameter): kept only if the callee keeps it.
    Handed(u32, u16, u8),
    /// Stored as an attribute of the object this variable holds, by a write that runs no code: a
    /// link when that object is one the function made on every path, kept by what keeps it.
    Into(VarId),
    /// Stored in the builtin container of this kind the variable holds, by a call or an item
    /// store that keeps nothing else (`kids.append(part)`, `kids[k] = part`): a link when that
    /// container is one the constructor of the function's objects made ([`Recorder::resolve`]),
    /// and the store runs no code of the project; otherwise a definite escape. The receiver's
    /// own escape (the variable is the container's) is then nothing.
    Added(VarId, ContainerKind),
}

/// One allocation: a variable defined by a constructor call, a copy, or a project call
/// (`returns`) whose callees must return a new object.
struct Alloc {
    pos: Pos,
    dst: VarId,
    kind: NewKind,
    returns: Vec<NewFrom>,
    /// The project class a constructor call made the object of.
    class: Option<Box<str>>,
    /// For a container the constructor of a root's object made, that root, made and so killed
    /// where that object is, and the attribute.
    of: Option<(usize, Box<str>)>,
}

/// What the extractor records for one function.
#[derive(Default)]
pub(crate) struct Recorder {
    allocs: Vec<Alloc>,
    /// The results of calls that hand back the object they are handed (a project function
    /// returning its parameter), and the targets an unpacking of a display binds
    /// (`a, b = X(), Y()`), by the variable of the object they hold: the same object.
    handed_back: FxHashMap<VarId, VarId>,
    /// Each escape with the tests of the function's inputs that hold where it runs.
    escapes: Vec<(Pos, VarId, Escape, Vec<InputGuard>)>,
    /// The points asked about, by query id.
    queries: Vec<(Pos, VarId)>,
    /// The writes of an attribute (`None`: of any) of the object a variable holds: a store, a
    /// delete, `setattr`, a write through its `__dict__`.
    writes: Vec<(Pos, VarId, Option<Box<str>>)>,
    /// The attribute reads a store into a container names ([`Escape::Added`]): the read, the
    /// object read, and the attribute.
    reads: Vec<(VarId, VarId, Box<str>)>,
    /// The reads [`Recorder::resolve`] decides give a container the constructor of the object
    /// read made: the container roots each gives, and their kind.
    owned: FxHashMap<VarId, (u64, ContainerKind)>,
}

/// The escape state before a point: allocation roots and parameters that may be kept, and the
/// roots / parameters handed to each `(callee, parameter)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct State {
    allocs: u64,
    params: u64,
    /// By the id of a store that may keep what it is handed ([`Keep`]): (allocation roots,
    /// parameters).
    handed: Vec<(u64, u64)>,
}

/// The operands of a value's definition it may hold a part of, pushed to the list given.
pub(crate) type ReadsOf<'a> = dyn Fn(VarId, &mut Vec<VarId>) + 'a;

/// What the call of a hand-over (site, callee index, tests known) tells: the callee and the
/// call's tests.
pub(crate) type HandedTo<'a> = dyn Fn(u32, u16, &[InputGuard]) -> (FuncKey, CallTests) + 'a;

/// The kind of the builtin container an attribute of a new instance of a project class holds,
/// one its constructor made and nothing else holds (`ProjectDecls::constructor_container`).
pub(crate) type MadeContainer<'a> = dyn Fn(&str, &str) -> Option<ContainerKind> + 'a;

/// What may be kept on some path: the union of the states joined.
impl Join for State {
    fn join(&mut self, other: &Self) -> bool {
        let before = (self.allocs, self.params);
        self.allocs |= other.allocs;
        self.params |= other.params;
        let mut grew = before != (self.allocs, self.params);
        for (a, b) in self.handed.iter_mut().zip(&other.handed) {
            let was = *a;
            a.0 |= b.0;
            a.1 |= b.1;
            grew |= was != *a;
        }
        grew
    }

    fn weight(&self) -> usize {
        1 + self.handed.len()
    }
}

/// The ownership facts of the variables, from the allocations through copies and phis.
struct Roots<'f> {
    /// Allocation roots each variable may hold (any phi input).
    may: FxHashMap<VarId, u64>,
    /// Variables every input of which is an allocation (`may` is exact for them).
    all: FxHashMap<VarId, bool>,
    /// Returned variables every input of which is an allocation or an element of a parameter,
    /// and some an element: the parameters those are elements of. One without an allocation
    /// (absent from `may`) returns nothing at a call binding them to values without elements.
    elements_of: FxHashMap<VarId, u64>,
    may_params: &'f FxHashMap<VarId, u64>,
    /// `(i, None)` entries: the variable is parameter `i` itself on every path.
    param_of: &'f dyn Fn(VarId) -> Option<u8>,
}

impl Roots<'_> {
    fn masks(&self, var: VarId) -> (u64, u64) {
        (
            self.may.get(&var).copied().unwrap_or(0),
            self.may_params.get(&var).copied().unwrap_or(0),
        )
    }
}

/// An action of the data flow at one position, applied in this order: queries, escapes, kills.
enum Action {
    Query(usize),
    Escape(Kept),
    /// A link of a value into the objects of the roots `holder`: what keeps one of them keeps
    /// the value from then on, as the escapes `value` would.
    Link {
        holder: u64,
        value: Vec<Kept>,
    },
    Kill(u64),
}

/// Allocation roots and parameters an escape may keep, by the id of the store that may keep them
/// (`None`: kept).
#[derive(Clone, Copy)]
struct Kept {
    allocs: u64,
    params: u64,
    by: Option<usize>,
}

/// The objects of the function's allocation roots that hold one another through links
/// ([`Escape::Into`]), and what each value asked about may have been read out of.
struct Links {
    /// By root, the roots its objects may hold through any number of links.
    held: Vec<u64>,
    /// By value, the roots it may hold or have been read out of, through copies, phis, and the
    /// operands of its definition ([`Recorder::resolve`]'s `reads_of`). Absent: only `may`.
    read: FxHashMap<VarId, u64>,
    /// Roots every value may hold or have been read out of (all of them past the budget).
    everything: u64,
}

impl Links {
    /// What every value may hold of `roots` roots, each holding every one: past the budget.
    fn everything(roots: usize) -> Self {
        let all = u64::MAX.checked_shr(64 - roots as u32).unwrap_or(0);
        Self {
            held: vec![all; roots],
            read: FxHashMap::default(),
            everything: all,
        }
    }

    /// The roots the objects of `roots` may hold.
    fn held_by(&self, roots: u64) -> u64 {
        roots_in(roots).fold(0, |mask, r| mask | self.held[r])
    }

    /// The roots `var` may hold or have been read out of.
    fn read(&self, var: VarId, may: u64) -> u64 {
        self.read.get(&var).copied().unwrap_or(may) | self.everything
    }
}

/// The indexes of the bits set in `mask`.
fn roots_in(mask: u64) -> impl Iterator<Item = usize> {
    (0..64).filter(move |r| mask & (1u64 << r) != 0)
}

/// The stores that may keep what they are handed ([`Keep`]), each with its id.
#[derive(Default)]
struct Keeps {
    ids: FxHashMap<Keep, usize>,
    keys: Vec<Keep>,
}

impl Keeps {
    fn id(&mut self, key: Keep) -> usize {
        let next = self.keys.len();
        let id = *self.ids.entry(key.clone()).or_insert(next);
        if id == next {
            self.keys.push(key);
        }
        id
    }

    /// The id of a store that keeps what it is handed where the tests `known` hold (`None`:
    /// whatever the function is passed).
    fn store(&mut self, known: &[InputGuard]) -> Option<usize> {
        (!known.is_empty()).then(|| {
            self.id(Keep {
                by: KeptBy::Store,
                at: CallTests {
                    args: Vec::new(),
                    known: known.to_vec(),
                },
            })
        })
    }
}

impl Recorder {
    /// An allocation at `pos`; `class`: the project class a constructor call makes an instance
    /// of.
    pub(crate) fn alloc(
        &mut self,
        pos: Pos,
        dst: VarId,
        (kind, class): (NewKind, Option<&str>),
        returns: Vec<NewFrom>,
    ) {
        if self.allocs.len() < 64 {
            self.allocs.push(Alloc {
                pos,
                dst,
                kind,
                returns,
                class: class.map(Into::into),
                of: None,
            });
        }
    }

    /// `dst` holds the object `arg` holds: the result of a call returning its argument, or the
    /// target an unpacking binds to an element of a display.
    pub(crate) fn handed_back(&mut self, dst: VarId, arg: VarId) {
        self.handed_back.insert(dst, arg);
    }

    /// An escape at `pos`, where the tests `known` of the function's inputs hold.
    pub(crate) fn escape(&mut self, pos: Pos, var: VarId, how: Escape, known: Vec<InputGuard>) {
        self.escapes.push((pos, var, how, known));
    }

    /// The read `read` of the attribute `attr` of the object `obj` holds gives a container that
    /// a store names ([`Escape::Added`]).
    pub(crate) fn container_read(&mut self, read: VarId, obj: VarId, attr: &str) {
        self.reads.push((read, obj, attr.into()));
    }

    /// A write of the attribute `attr` (`None`: of any) of the object `obj` holds at `pos`.
    pub(crate) fn write(&mut self, pos: Pos, obj: VarId, attr: Option<&str>) {
        self.writes.push((pos, obj, attr.map(Into::into)));
    }

    /// Make every store into a container ([`Escape::Added`]) that `runs_code` says may hand the
    /// value (at a position) to code of the project a definite escape.
    pub(crate) fn adds_running_code(&mut self, runs_code: impl Fn(Pos, VarId) -> bool) {
        for (pos, var, how, _) in &mut self.escapes {
            if matches!(how, Escape::Added(..)) && runs_code(*pos, *var) {
                *how = Escape::Definite;
            }
        }
    }

    /// Ask about `var` just before the instruction at `pos`; returns the query id.
    pub(crate) fn query(&mut self, pos: Pos, var: VarId) -> usize {
        self.queries.push((pos, var));
        self.queries.len() - 1
    }

    /// Answer every query, and the returns of the function of `shape` (`returns`: whether the
    /// function's result is a new object at every `return` with a value). `element_of` gives the
    /// parameters a value is an element of by its definition (`0`: none); `reads_of` the
    /// operands of a value's definition it may hold a part of (the object of an attribute read,
    /// what a call is handed); `handed_to` the callee of a hand-over and what its call tells,
    /// where the tests given hold; `made` the containers the constructor of a project class
    /// makes for its instance.
    ///
    /// A read of an attribute of an object the function made by calling its class, on every
    /// path, is the container that class's constructor made for it (`Owned`, [`Self::owned`]),
    /// when a store into a container names the read: a root of its own, which the object holds
    /// and which is made and killed where the object is. Rebinding the attribute, or any
    /// attribute, of an object that may be it keeps that root, so that what is stored into what
    /// a later read gives is kept; keeping or handing on the object keeps it with what the
    /// object holds.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn resolve(
        mut self,
        shape: &FnShape<'_>,
        may_params: &FxHashMap<VarId, u64>,
        param_of: &dyn Fn(VarId) -> Option<u8>,
        (element_of, reads_of): (&dyn Fn(VarId) -> u64, &ReadsOf<'_>),
        handed_to: &HandedTo<'_>,
        made: &MadeContainer<'_>,
        returns: bool,
    ) -> (Vec<NewArg>, ReturnsNew) {
        // Without an allocation, only parameters can be asked about, and nothing is returned
        // new.
        if self.queries.is_empty() && (self.allocs.is_empty() || !returns) {
            return (Vec::new(), ReturnsNew::No);
        }
        let f = shape.f;
        let returned: Vec<(Pos, VarId)> = if returns {
            f.blocks
                .iter()
                .filter_map(|b| match b.terminator {
                    heki_ir::Terminator::Return(Some(v)) => {
                        Some(((b.id.0, u32::try_from(b.insts.len()).ok()?), v))
                    }
                    _ => None,
                })
                .collect()
        } else {
            Vec::new()
        };
        let holders = self.escapes.iter().filter_map(|(_, _, how, _)| match how {
            Escape::Into(holder) | Escape::Added(holder, _) => Some(*holder),
            _ => None,
        });
        // The objects whose attributes the containers stored into are read out of.
        let read_objects = self.reads.iter().map(|(_, obj, _)| *obj);
        let asked: Vec<VarId> = (self.queries.iter().map(|(_, v)| *v))
            .chain(returned.iter().map(|(_, v)| *v))
            .chain(self.escapes.iter().map(|(_, v, _, _)| *v))
            .chain(holders)
            .chain(read_objects)
            .collect();
        let budget = Budget::of_function("construction::Recorder", f);
        let returned_vars: Vec<VarId> = returned.iter().map(|(_, v)| *v).collect();
        let roots_of = |rec: &Self, written: &[(Pos, VarId, Option<Box<str>>)]| {
            rec.roots(
                shape,
                &budget,
                (asked.iter().copied()).chain(written.iter().map(|(_, obj, _)| *obj)),
                (&returned_vars, element_of),
                may_params,
                param_of,
            )
        };
        // The writes matter only to the containers decided `Owned`.
        let found = roots_of(&self, &[]).and_then(|roots| {
            if self.decide_owned(&roots, made) {
                roots_of(&self, &self.writes)
            } else {
                self.writes.clear();
                Answer::Known(roots)
            }
        });
        // Past the budget, no object is known to be new, as past the allocations recorded.
        let Answer::Known(roots) = found else {
            return (vec![NewArg::Old; self.queries.len()], ReturnsNew::No);
        };
        let trackable = |v: VarId| roots.may.contains_key(&v) || param_of(v).is_some();
        let any_return_new = !returned.is_empty()
            && (returned.iter()).all(|(_, v)| trackable(*v) || roots.elements_of.contains_key(v));
        if !self.queries.iter().any(|(_, v)| trackable(*v)) && !any_return_new {
            return (vec![NewArg::Old; self.queries.len()], ReturnsNew::No);
        }
        // A link's holder: an object the function made on every path, no parameter.
        let holds = |h: VarId| {
            let (a, p) = roots.masks(h);
            a != 0 && p == 0 && roots.all.get(&h).copied().unwrap_or(false)
        };
        let containers = self.container_roots();
        // A container stored into: one of `kind` the constructor of the function's objects made
        // for them, on every path.
        let owned = |c: VarId, kind: ContainerKind| {
            holds(c)
                && roots.masks(c).0 & !containers == 0
                && self.owned.get(&shape.copy_root(c)).map(|o| o.1) == Some(kind)
        };
        let linking = |var: VarId, how: &Escape| match how {
            Escape::Into(h) => holds(*h),
            Escape::Added(c, kind) => *c != var && owned(*c, *kind),
            _ => false,
        };
        // Past the budget, every value may hold or have been read out of every root.
        let links = (self.links(shape, &budget, &roots, &linking, reads_of))
            .open_means(Links::everything(self.allocs.len()));
        // The ids of the stores that may keep what they are handed: a hand-over, one per callee,
        // parameter, and what its call tells; a definite store under tests of the function's
        // inputs, one per tests; and a store of a value read out of a project function's result,
        // one per function and tests. The answers name only those, so the calls of one helper
        // binding alike share a hand-over.
        let mut keeps = Keeps::default();
        let factories = (self.allocs.iter().enumerate())
            .filter(|(_, a)| !a.returns.is_empty())
            .fold(0u64, |mask, (r, _)| mask | (1u64 << r));
        // What storing `var` where the tests `known` hold keeps: the objects it may be, those
        // they hold, and those a value read out of them may be; read out of a project
        // function's result, the result itself when that function links it to itself
        // (`KeptBy::ReadBack`).
        let stored = |keeps: &mut Keeps, var: VarId, known: &[InputGuard], out: &mut Vec<Kept>| {
            let (a, params) = roots.masks(var);
            let read = links.read(var, a);
            let allocs = a | links.held_by(read);
            if allocs | params != 0 {
                out.push(Kept {
                    allocs,
                    params,
                    by: keeps.store(known),
                });
            }
            self.read_backs(keeps, &links, read & factories & !allocs, known, out);
        };
        let mut actions: Vec<(Pos, u8, Action)> = Vec::with_capacity(
            self.queries.len() + returned.len() + self.escapes.len() + self.allocs.len(),
        );
        for (pos, var, how, known) in &self.escapes {
            let mut out = Vec::new();
            match how {
                // The receiver of a store into a container its objects own keeps nothing.
                Escape::Added(c, kind) if c == var && owned(*c, *kind) => {}
                Escape::Into(holder) | Escape::Added(holder, _) if linking(*var, how) => {
                    // A parameter stored in a new object stays kept (`keep_params`).
                    let params = roots.masks(*var).1;
                    if params != 0 {
                        let by = keeps.store(known);
                        out.push(Kept {
                            allocs: 0,
                            params,
                            by,
                        });
                    }
                    let mut value = Vec::new();
                    stored(&mut keeps, *var, known, &mut value);
                    let holder = roots.masks(*holder).0;
                    // A holder a project call returned is new only where that call returns a
                    // new object: elsewhere the value is as public as the holder.
                    let allocs = value.iter().fold(0, |mask, k| mask | k.allocs);
                    for r in roots_in(holder).filter(|_| allocs != 0) {
                        for from in &self.allocs[r].returns {
                            let by = Some(keeps.id(Keep {
                                by: KeptBy::Holder(from.clone()),
                                at: CallTests {
                                    args: Vec::new(),
                                    known: known.clone(),
                                },
                            }));
                            out.push(Kept {
                                allocs,
                                params: 0,
                                by,
                            });
                        }
                    }
                    actions.push((*pos, 1, Action::Link { holder, value }));
                }
                Escape::Definite | Escape::Into(_) | Escape::Added(..) => {
                    stored(&mut keeps, *var, known, &mut out);
                }
                Escape::Handed(s, c, p) => {
                    let (allocs, params) = roots.masks(*var);
                    if allocs | params != 0 {
                        let (callee, at) = handed_to(*s, *c, known);
                        let by = Some(keeps.id(Keep {
                            by: KeptBy::Callee(callee, *p),
                            at,
                        }));
                        out.push(Kept { allocs, params, by });
                    }
                    // The callee may store anything it reads out of the object, which its
                    // summary does not tell.
                    let read = links.read(*var, allocs);
                    let parts = links.held_by(read);
                    if parts != 0 {
                        let by = keeps.store(known);
                        out.push(Kept {
                            allocs: parts,
                            params: 0,
                            by,
                        });
                    }
                    self.read_backs(
                        &mut keeps,
                        &links,
                        read & factories & !parts,
                        known,
                        &mut out,
                    );
                }
            }
            actions.extend(out.into_iter().map(|k| (*pos, 1, Action::Escape(k))));
        }
        // A write of an attribute of an object that may be one holding a container keeps the
        // container: a later read of the attribute may give another value.
        for (pos, obj, attr) in &self.writes {
            let objects = roots.masks(*obj).0;
            let objects = objects | links.held_by(links.read(*obj, objects));
            let rebound = (self.allocs.iter().enumerate())
                .filter(|(_, a)| {
                    a.of.as_ref().is_some_and(|(r, of)| {
                        objects & (1u64 << r) != 0 && attr.as_ref().is_none_or(|attr| attr == of)
                    })
                })
                .fold(0u64, |mask, (r, _)| mask | (1u64 << r));
            if rebound != 0 {
                let kept = Kept {
                    allocs: rebound,
                    params: 0,
                    by: None,
                };
                actions.push((*pos, 1, Action::Escape(kept)));
            }
        }
        let handed_keys = keeps.keys;
        // Every action in position order (queries, escapes, kills at one position), and the
        // range of each block's actions.
        let n = f.blocks.len();
        for (i, (pos, _)) in self.queries.iter().enumerate() {
            actions.push((*pos, 0, Action::Query(i)));
        }
        for (i, (pos, _)) in returned.iter().enumerate() {
            actions.push((*pos, 0, Action::Query(self.queries.len() + i)));
        }
        for (r, a) in self.allocs.iter().enumerate() {
            actions.push((a.pos, 2, Action::Kill(1u64 << r)));
        }
        actions.sort_by_key(|(pos, phase, _)| (*pos, *phase));
        let mut ranges = vec![(0usize, 0usize); n];
        let mut start = 0;
        while start < actions.len() {
            let block = actions[start].0.0 as usize;
            let mut end = start;
            while end < actions.len() && actions[end].0.0 as usize == block {
                end += 1;
            }
            ranges[block] = (start, end);
            start = end;
        }
        let of = |bi: usize| &actions[ranges[bi].0..ranges[bi].1];
        let empty = State {
            handed: vec![(0, 0); handed_keys.len()],
            ..State::default()
        };
        let keep = |state: &mut State, k: &Kept| match k.by {
            None => {
                state.allocs |= k.allocs;
                state.params |= k.params;
            }
            Some(id) => {
                state.handed[id].0 |= k.allocs;
                state.handed[id].1 |= k.params;
            }
        };
        let apply = |state: &mut State, action: &Action| match action {
            Action::Escape(k) => keep(state, k),
            Action::Link { holder, value } => {
                // Kept with its holder: by every store that may keep one of the holder's
                // objects.
                let allocs = value.iter().fold(0, |mask, k| mask | k.allocs);
                for h in &mut state.handed {
                    if h.0 & holder != 0 {
                        h.0 |= allocs;
                    }
                }
                if state.allocs & holder != 0 {
                    for k in value {
                        keep(state, k);
                    }
                }
            }
            Action::Kill(bit) => {
                state.allocs &= !bit;
                for h in &mut state.handed {
                    h.0 &= !bit;
                }
            }
            Action::Query(_) => {}
        };
        // The forward data flow over the blocks the entry reaches, in reverse postorder: the
        // state on entering a block joins what each predecessor may have kept on its way out,
        // through its normal edges, and at any of its points through its exception edge. Kills
        // apply inside a block, so the states only grow.
        let rpo = shape.rpo();
        let node = |b: heki_ir::BlockId| shape.rpo_index(b).map(|i| i as usize);
        let out_of = |p: heki_ir::BlockId, facts: &Facts<State>| {
            let mut state = node(p)
                .and_then(|i| facts.get(i))
                .map_or_else(|| empty.clone(), Clone::clone);
            let mut exc = state.clone();
            for (_, _, action) in of(p.0 as usize) {
                apply(&mut state, action);
                exc.join(&state);
            }
            (state, exc)
        };
        let transfer = |i: usize, facts: &Facts<State>| {
            let b = rpo[i];
            let mut entry = empty.clone();
            for &p in shape.preds(b).iter().filter(|p| node(**p).is_some()) {
                let (out, exc) = out_of(p, facts);
                let pb = f.block(p);
                if pb.terminator.successors().contains(&b) {
                    entry.join(&out);
                }
                if pb.exc_target == Some(b) {
                    entry.join(&exc);
                }
            }
            Some(entry)
        };
        let readers = |i: usize, out: &mut Vec<usize>| {
            out.extend(shape.succs(rpo[i]).filter_map(node));
        };
        // A block is visited once, then once per bit a predecessor's state gains.
        let bits = 2 * 64 * (1 + handed_keys.len());
        let most_preds = (rpo.iter().map(|b| shape.preds(*b).len()).max()).unwrap_or(0);
        let spec = Sparse {
            nodes: rpo.len(),
            readers: &readers,
            max_visits: u32::try_from(1 + most_preds * (1 + bits)).unwrap_or(u32::MAX),
        };
        // Past the budget, no object is known to be new, as past the roots' walk.
        let Answer::Known(block_in) = fixpoint(&budget, spec, transfer) else {
            return (vec![NewArg::Old; self.queries.len()], ReturnsNew::No);
        };
        // Answer the queries with the state before each one (a block the entry does not reach
        // keeps nothing).
        let mut answers = vec![NewArg::Old; self.queries.len() + returned.len()];
        let mut state = empty.clone();
        for (bi, block) in f.blocks.iter().enumerate() {
            let list = of(bi);
            if list.iter().all(|(_, _, a)| !matches!(a, Action::Query(_))) {
                continue;
            }
            state.clone_from(
                node(block.id)
                    .and_then(|i| block_in.get(i))
                    .unwrap_or(&empty),
            );
            for (_, _, action) in list {
                if let Action::Query(i) = action {
                    let (var, of_return) = match self.queries.get(*i) {
                        Some((_, v)) => (*v, false),
                        None => (returned[*i - self.queries.len()].1, true),
                    };
                    answers[*i] =
                        self.answer(&roots, &state, &handed_keys, var, of_return, containers);
                } else {
                    apply(&mut state, action);
                }
            }
        }
        let tail = answers.split_off(self.queries.len());
        let returns_new = || {
            let mut kind = NewKind::Own;
            let mut cond = NewCond::default();
            let mut any = false;
            let mut linked = false;
            for (v, a) in returned_vars.iter().zip(tail) {
                // Only elements of parameters: nothing is returned there at such a call.
                if !roots.may.contains_key(v) && roots.elements_of.contains_key(v) {
                    continue;
                }
                let NewArg::New { kind: k, cond: c } = a else {
                    return ReturnsNew::No;
                };
                kind = kind.min(k);
                cond = cond.and(&c);
                any = true;
                linked |= roots_in(roots.masks(*v).0).any(|r| links.held[r] & (1u64 << r) != 0);
            }
            let elements_of = (returned_vars.iter())
                .filter_map(|v| roots.elements_of.get(v))
                .fold(0, |mask, m| mask | m);
            if !any {
                return ReturnsNew::No;
            }
            ReturnsNew::New {
                kind,
                cond,
                elements_of,
                kept: MayGiven::default(),
                linked,
            }
        };
        (answers, returns_new())
    }

    /// The allocation roots each allocating variable defines, and the container roots each read
    /// decided `Owned` gives.
    fn own_roots(&self) -> FxHashMap<VarId, u64> {
        let mut own: FxHashMap<VarId, u64> = FxHashMap::default();
        for (r, a) in self
            .allocs
            .iter()
            .enumerate()
            .filter(|(_, a)| a.of.is_none())
        {
            *own.entry(a.dst).or_default() |= 1u64 << r;
        }
        for (read, (bits, _)) in &self.owned {
            *own.entry(*read).or_default() |= bits;
        }
        own
    }

    /// The roots of the containers the constructors of the function's objects made.
    fn container_roots(&self) -> u64 {
        (self.allocs.iter().enumerate())
            .filter(|(_, a)| a.of.is_some())
            .fold(0, |mask, (r, _)| mask | (1u64 << r))
    }

    /// Decide which attribute reads a store into a container names ([`Self::reads`]) give the
    /// container the constructor of the object read made for it ([`Self::owned`]): every root
    /// the object may hold, on every path, is an object the function made by calling a project
    /// class whose constructor makes the attribute such a container (`made`), of one kind. Each
    /// object root and attribute gets one container root, among the 64 roots. Whether any does.
    fn decide_owned(&mut self, roots: &Roots<'_>, made: &MadeContainer<'_>) -> bool {
        let reads = std::mem::take(&mut self.reads);
        let mut made_for: FxHashMap<(usize, &str), usize> = FxHashMap::default();
        for (t, obj, attr) in &reads {
            let (a, p) = roots.masks(*obj);
            if self.owned.contains_key(t)
                || a == 0
                || p != 0
                || !roots.all.get(obj).copied().unwrap_or(false)
            {
                continue;
            }
            let kinds: Option<Vec<ContainerKind>> = roots_in(a)
                .map(|r| {
                    let alloc = &self.allocs[r];
                    (alloc.kind == NewKind::Own)
                        .then_some(())
                        .and_then(|()| made(alloc.class.as_deref()?, attr))
                })
                .collect();
            let kind = (kinds.as_deref())
                .and_then(|ks| ks.first().filter(|k| ks.iter().all(|x| x == *k)))
                .copied();
            let missing = roots_in(a)
                .filter(|r| !made_for.contains_key(&(*r, &**attr)))
                .count();
            let (Some(kind), true) = (kind, self.allocs.len() + missing <= 64) else {
                continue;
            };
            let mut bits = 0u64;
            for r in roots_in(a) {
                let id = *made_for.entry((r, attr)).or_insert_with(|| {
                    let pos = self.allocs[r].pos;
                    self.allocs.push(Alloc {
                        pos,
                        dst: *t,
                        kind: NewKind::Own,
                        returns: Vec::new(),
                        class: None,
                        of: Some((r, attr.clone())),
                    });
                    self.allocs.len() - 1
                });
                bits |= 1u64 << id;
            }
            self.owned.insert(*t, (bits, kind));
        }
        !self.owned.is_empty()
    }

    /// The links between the allocation roots ([`Links`]): each escape `linking` says is a
    /// link ([`Escape::Into`], [`Escape::Added`]) links into the holder's roots the roots its
    /// value may hold or have been read out of (a value read out of an object may be one it
    /// holds), and an object holds the containers its constructor made, closed under links.
    /// Without a link or a root from a project call, nothing is read out of a root that the
    /// escapes do not keep already. Open past the budget.
    fn links(
        &self,
        shape: &FnShape<'_>,
        budget: &Budget,
        roots: &Roots<'_>,
        linking: &dyn Fn(VarId, &Escape) -> bool,
        reads_of: &ReadsOf<'_>,
    ) -> Answer<Links> {
        let mut links = Links {
            held: vec![0; self.allocs.len()],
            read: FxHashMap::default(),
            everything: 0,
        };
        for (r, a) in self.allocs.iter().enumerate() {
            if let Some((holder, _)) = a.of {
                links.held[holder] |= 1u64 << r;
            }
        }
        let any_link = (self.escapes.iter()).any(|(_, var, how, _)| linking(*var, how));
        if !any_link && self.allocs.iter().all(|a| a.returns.is_empty()) {
            return Answer::Known(links);
        }
        let own = self.own_roots();
        let f = shape.f;
        // What `may` walks, and the operands of a definition the value may hold a part of; a
        // read decided `Owned` is its container itself.
        let from = |v: VarId, out: &mut Vec<VarId>| {
            match shape.def(v) {
                Def::Inst(..) if self.owned.contains_key(&v) => {}
                Def::Inst(..) => reads_of(v, out),
                Def::Phi(b, i) => out.extend(
                    (f.block(b).phis[i as usize].incoming.iter().map(|(_, x)| *x))
                        .filter(|x| *x != v),
                ),
                Def::Param | Def::None => {}
            }
            out.extend(self.handed_back.get(&v));
        };
        let read: Closure<VarId, Or> = Closure::new();
        let own = |u: VarId| Or(own.get(&u).copied().unwrap_or(0));
        let found: Answer<Vec<u64>> = (self.escapes.iter())
            .map(|(_, var, _, _)| read.ask(budget, *var, own, from).map(|b| b.0))
            .collect();
        let written: Answer<Vec<u64>> = (self.writes.iter())
            .map(|(_, obj, _)| read.ask(budget, *obj, own, from).map(|b| b.0))
            .collect();
        found.zip(written).map(|(found, written)| {
            for ((_, obj, _), found) in self.writes.iter().zip(written) {
                links.read.insert(*obj, found | roots.masks(*obj).0);
            }
            for ((_, var, how, _), found) in self.escapes.iter().zip(found) {
                let read = found | roots.masks(*var).0;
                links.read.insert(*var, read);
                if let Escape::Into(holder) | Escape::Added(holder, _) = how
                    && linking(*var, how)
                {
                    for r in roots_in(roots.masks(*holder).0) {
                        links.held[r] |= read;
                    }
                }
            }
            // Closed under links: what an object holds holds (Warshall's closure).
            for k in 0..links.held.len() {
                let through = links.held[k];
                for held in &mut links.held {
                    if *held & (1u64 << k) != 0 {
                        *held |= through;
                    }
                }
            }
            links
        })
    }

    /// The escapes of a store, where the tests `known` hold, of a value read out of the roots
    /// `mask` from project calls: each keeps its root's objects when a function of the call
    /// links its result to itself (`KeptBy::ReadBack`).
    fn read_backs(
        &self,
        keeps: &mut Keeps,
        links: &Links,
        mask: u64,
        known: &[InputGuard],
        out: &mut Vec<Kept>,
    ) {
        for r in roots_in(mask) {
            let allocs = (1u64 << r) | links.held[r];
            for from in &self.allocs[r].returns {
                let by = Some(keeps.id(Keep {
                    by: KeptBy::ReadBack(from.callee.clone()),
                    at: CallTests {
                        args: Vec::new(),
                        known: known.to_vec(),
                    },
                }));
                out.push(Kept {
                    allocs,
                    params: 0,
                    by,
                });
            }
        }
    }

    /// The allocation roots each of `asked` may hold, through copies and phis: `may`, the roots
    /// of every value a value reaches through their inputs, and `all`, whether every value on the
    /// way holds allocations only; for the `returned` values, also `elements_of`, by the
    /// parameters `element_of` says a value is an element of. Open past the budget.
    fn roots<'f>(
        &self,
        shape: &FnShape<'_>,
        budget: &Budget,
        asked: impl Iterator<Item = VarId>,
        (returned, element_of): (&[VarId], &dyn Fn(VarId) -> u64),
        may_params: &'f FxHashMap<VarId, u64>,
        param_of: &'f dyn Fn(VarId) -> Option<u8>,
    ) -> Answer<Roots<'f>> {
        let mut roots = Roots {
            may: FxHashMap::default(),
            all: FxHashMap::default(),
            elements_of: FxHashMap::default(),
            may_params,
            param_of,
        };
        let own = self.own_roots();
        if own.is_empty() {
            return Answer::Known(roots);
        }
        let f = shape.f;
        // A local of a function read where nothing bound it: reading it raises
        // `UnboundLocalError`, so it holds no object (in a module or class body the read falls
        // back to the globals).
        let unbound = |x: VarId| {
            !f.is_module_body
                && !f.is_class_body
                && matches!(
                    shape.inst(x).map(|i| &i.kind),
                    Some(InstKind::LoadName { .. })
                )
        };
        // A copy's source, the argument a call hands back, a phi's inputs other than itself that
        // may hold an object.
        let inputs = |v: VarId, out: &mut Vec<VarId>| match shape.def(v) {
            Def::Inst(b, i) => {
                if let InstKind::Copy { src, .. } = f.block(b).insts[i as usize].kind {
                    out.push(src);
                }
                out.extend(self.handed_back.get(&v));
            }
            Def::Phi(b, i) => out.extend(
                (f.block(b).phis[i as usize].incoming.iter().map(|(_, x)| *x))
                    .filter(|x| *x != v && !unbound(*x)),
            ),
            Def::Param | Def::None => {}
        };
        let held: Closure<VarId, Or> = Closure::new();
        let may = |v: VarId| {
            let own = |u: VarId| Or(own.get(&u).copied().unwrap_or(0));
            held.ask(budget, v, own, inputs).map(|bits| bits.0)
        };
        let lacking: Closure<VarId, bool> = Closure::new();
        for v in asked {
            // A value on the way that holds no allocation.
            let none = |u: VarId| !matches!(may(u), Answer::Known(b) if b != 0);
            let held = may(v).and_then(|bits| {
                if bits == 0 || roots.may.contains_key(&v) {
                    return Answer::Known(None);
                }
                (lacking.ask(budget, v, none, inputs)).map(|lacks| Some((bits, !*lacks)))
            });
            let Answer::Known(held) = held else {
                return Answer::Open;
            };
            if let Some((bits, all)) = held {
                roots.may.insert(v, bits);
                roots.all.insert(v, all);
            }
        }
        // A returned value some inputs of which are elements of parameters, the others
        // allocations.
        let elements: Closure<VarId, Or> = Closure::new();
        let elements_of = |v: VarId| {
            let of = |u: VarId| Or(element_of(u));
            elements.ask(budget, v, of, inputs).map(|m| m.0)
        };
        let lacking_either: Closure<VarId, bool> = Closure::new();
        // A value on the way that holds neither an allocation nor an element.
        let neither = |u: VarId| {
            let holds = |a: Answer<u64>| matches!(a, Answer::Known(b) if b != 0);
            !holds(elements_of(u)) && !holds(may(u))
        };
        for &v in returned {
            if roots.all.get(&v).is_some_and(|all| *all) {
                continue;
            }
            let mixed = elements_of(v).and_then(|mask| {
                let lacks = lacking_either.ask(budget, v, neither, inputs);
                lacks.map(|lacks| (mask != 0 && !*lacks).then_some(mask))
            });
            // Past the budget, the value is known to be no element of a parameter.
            if let Some(mask) = mixed.open_means(None) {
                roots.elements_of.insert(v, mask);
            }
        }
        Answer::Known(roots)
    }

    /// Whether `var` holds, at a point with escape state `state`, a new object or a parameter
    /// that nothing kept, and under which conditions. A returned value (`of_return`) may also
    /// be an element of a parameter on the paths without an allocation
    /// ([`Roots::elements_of`]). A container root (`containers`) is no new object the answer
    /// names: it is part of the object holding it.
    fn answer(
        &self,
        roots: &Roots<'_>,
        state: &State,
        handed_keys: &[Keep],
        var: VarId,
        of_return: bool,
        containers: u64,
    ) -> NewArg {
        let (a, p) = roots.masks(var);
        let new = a != 0
            && a & containers == 0
            && p == 0
            && (roots.all.get(&var).copied().unwrap_or(false)
                || of_return && roots.elements_of.contains_key(&var));
        let param = if a == 0 { (roots.param_of)(var) } else { None };
        if (!new && param.is_none()) || state.allocs & a != 0 || state.params & p != 0 {
            return NewArg::Old;
        }
        let mut keeps = Vec::new();
        for (i, (ha, hp)) in state.handed.iter().enumerate() {
            if ha & a != 0 || hp & p != 0 {
                keeps.push(handed_keys[i].clone());
            }
        }
        if let Some(param) = param {
            let cond = NewCond::new(Vec::new(), keeps);
            return NewArg::Param { param, cond };
        }
        let mut kind = NewKind::Own;
        let mut returns = Vec::new();
        for (r, alloc) in self.allocs.iter().enumerate() {
            if a & (1u64 << r) != 0 {
                kind = kind.min(alloc.kind);
                returns.extend(alloc.returns.iter().cloned());
            }
        }
        NewArg::New {
            kind,
            cond: NewCond::new(returns, keeps),
        }
    }
}
