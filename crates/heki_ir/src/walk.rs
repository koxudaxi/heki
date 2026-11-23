//! Bounded walks over graphs of IR ids: values, blocks, functions, and classes.
//!
//! Walks of the analyses go through these engines, so that the rules of `docs/development.md`
//! ("Analysis cost") hold by construction:
//!
//! - [`Memo`] keeps each answer per key, a node and a question: a walk through the inputs of a
//!   phi or both operands of an expression decides each node once.
//! - [`Closure`] joins the facts of every node a node reaches (the members of a cycle share one
//!   fact), and [`fixpoint`] joins the facts of nodes that read each other, visiting a node again
//!   only when a fact it reads grows, up to a visit limit. Neither ever replaces a fact.
//! - [`reach`] walks with a visited set, so it ends on every cycle.
//! - A [`Budget`] per owner (a rule run on one function, the summary of one function) bounds
//!   the steps of all the owner's walks, and the facts they keep.
//! - [`FnShape`] holds the tables of one function that walks read in place of linear scans:
//!   definitions, readers, copy roots, predecessors, reverse postorder, dominators, and loops.
//! - [`SharedMemo`] keeps answers over the project declarations across functions, replaying
//!   what deciding each one read so that the recorded dependencies stay complete.
//!
//! [`Answer`] is heki's one type for an answer that is established or unknown: a walk that could
//! not decide its question (a cycle in a [`Memo`], a [`fixpoint`] past its visit limit, any walk
//! of an owner whose [`Budget`] is spent), a capacity the code does not fix, a premise nothing
//! establishes. What an unknown answer means belongs to the consumer, not to the question: a
//! missing trip count means "no rewrite" to an `OPT` rule and "unbounded loop" to `EFF102`. So
//! every use states it, with [`Answer::open_means`] or a `match`, and docs/scope.md ("When heki
//! cannot tell") holds the choices: a rewrite takes an open answer as "no rewrite", an exact report
//! as "no finding", and a heuristic report states its own choice where it reads the answer.
//!
//! `Memo`, `Closure`, and `Budget` are `!Sync`: they live in their owner for one analysis, never
//! in the shared project index (ADR-017), which must be `Sync`.

mod join;
mod shape;

pub use join::{And, BitSet, Fallible, Join, JoinMap, KeySet, Min, Or};
pub use shape::{Def, FnShape, Use};

use crate::IrFunction;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::hash::Hash;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

// ---------------------------------------------------------------------------------- answers

/// An answer that is established ([`Self::Known`]) or unknown ([`Self::Open`]). It has no
/// default and no conversion to `Option`, so that an open answer is never read as a value by
/// accident.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer<A> {
    Known(A),
    Open,
}

impl<A> Answer<A> {
    /// The answer, `open` standing for an open one: what this consumer acts on when the answer
    /// is unknown.
    pub fn open_means(self, open: A) -> A {
        match self {
            Self::Known(a) => a,
            Self::Open => open,
        }
    }

    pub fn is_open(&self) -> bool {
        matches!(self, Self::Open)
    }

    pub fn as_ref(&self) -> Answer<&A> {
        match self {
            Self::Known(a) => Answer::Known(a),
            Self::Open => Answer::Open,
        }
    }

    pub fn map<B>(self, f: impl FnOnce(A) -> B) -> Answer<B> {
        match self {
            Self::Known(a) => Answer::Known(f(a)),
            Self::Open => Answer::Open,
        }
    }

    /// A further question on the answer, open when either is.
    pub fn and_then<B>(self, f: impl FnOnce(A) -> Answer<B>) -> Answer<B> {
        match self {
            Self::Known(a) => f(a),
            Self::Open => Answer::Open,
        }
    }

    /// Both answers, established only when each is.
    pub fn zip<B>(self, other: Answer<B>) -> Answer<(A, B)> {
        match (self, other) {
            (Self::Known(a), Answer::Known(b)) => Answer::Known((a, b)),
            _ => Answer::Open,
        }
    }

    /// The answer over two cases (two paths, two callees), `join` combining two established
    /// ones: open when either is, since one unknown case leaves the whole unknown.
    pub fn join_with(self, other: Self, join: impl FnOnce(A, A) -> A) -> Self {
        self.zip(other).map(|(a, b)| join(a, b))
    }
}

/// The answers of every item, established only when each is.
impl<A, C: FromIterator<A>> FromIterator<Answer<A>> for Answer<C> {
    fn from_iter<I: IntoIterator<Item = Answer<A>>>(answers: I) -> Self {
        let mut open = false;
        let known = answers
            .into_iter()
            .map_while(|a| match a {
                Answer::Known(a) => Some(a),
                Answer::Open => {
                    open = true;
                    None
                }
            })
            .collect();
        if open { Self::Open } else { Self::Known(known) }
    }
}

// ----------------------------------------------------------------------------------- budget

/// The steps one owner's walks may take for one function. Once a walk needs more, the budget is
/// spent: from then on every walk of the owner answers [`Answer::Open`], and the owner reports
/// nothing for the function.
pub struct Budget {
    owner: &'static str,
    /// `None`: no limit ([`Self::unmetered`]).
    limit: Option<usize>,
    left: Cell<usize>,
    taken: Cell<usize>,
    spent: Cell<bool>,
    /// The function's qualified name and span start, kept only while [`record_steps`] is on.
    function: Option<(String, u32)>,
}

impl Budget {
    /// Steps per IR unit (a block, a phi, an instruction) of the function.
    pub const STEPS_PER_UNIT: usize = 64;
    pub const MIN_STEPS: usize = 4096;

    /// [`Self::STEPS_PER_UNIT`] per IR unit of `f`, at least [`Self::MIN_STEPS`].
    pub fn of_function(owner: &'static str, f: &IrFunction) -> Self {
        let units: usize = f
            .blocks
            .iter()
            .map(|b| 1 + b.phis.len() + b.insts.len())
            .sum();
        Self::new(
            owner,
            Some((units * Self::STEPS_PER_UNIT).max(Self::MIN_STEPS)),
            Some(f),
        )
    }

    /// `per_byte` steps per byte of `f`'s source, clamped to `min..=max`: the budgets some walks
    /// are bounded by in source bytes (the cells of lazy-initialization regions, origin steps,
    /// kept pairs), so that such a walk moved here spends exactly as it did.
    pub fn per_source_byte(
        owner: &'static str,
        f: &IrFunction,
        per_byte: usize,
        min: usize,
        max: usize,
    ) -> Self {
        let bytes = f.span.end.saturating_sub(f.span.start) as usize;
        Self::new(
            owner,
            Some(bytes.saturating_mul(per_byte).clamp(min, max)),
            Some(f),
        )
    }

    /// For walks over the project index or the declarations, which expand each node once per
    /// rule run: their cost is linear in what they walk, and a budget spent in a different order
    /// by `heki check` (every file) and by the language server (one file) would make the two
    /// disagree.
    pub fn unmetered(owner: &'static str) -> Self {
        Self::new(owner, None, None)
    }

    /// [`Self::STEPS_PER_UNIT`] per node and edge of a project graph built per check (the
    /// import graph), at least [`Self::MIN_STEPS`]. Its walks ask in the order of the graph's
    /// nodes, which `heki check` and the language server build alike, so they spend it alike.
    pub fn of_graph(owner: &'static str, units: usize) -> Self {
        Self::new(
            owner,
            Some((units.saturating_mul(Self::STEPS_PER_UNIT)).max(Self::MIN_STEPS)),
            None,
        )
    }

    fn new(owner: &'static str, limit: Option<usize>, f: Option<&IrFunction>) -> Self {
        let function = f
            .filter(|_| RECORDING.load(Ordering::Relaxed))
            .map(|f| (f.qualname.clone(), f.span.start));
        Self {
            owner,
            limit,
            left: Cell::new(limit.unwrap_or(usize::MAX)),
            taken: Cell::new(0),
            spent: Cell::new(false),
            function,
        }
    }

    /// Take `n` steps: `false` once the budget is spent, and from then on.
    pub fn take(&self, n: usize) -> bool {
        if self.spent.get() {
            return false;
        }
        self.taken.set(self.taken.get().saturating_add(n));
        if self.limit.is_none() {
            return true;
        }
        match self.left.get().checked_sub(n) {
            Some(left) => {
                self.left.set(left);
                true
            }
            None => {
                self.spent.set(true);
                false
            }
        }
    }

    /// Spend what is left: a walk that cannot go on (past the nesting guard) leaves the owner
    /// nothing to report from. An unmetered budget is spent too.
    pub fn spend(&self) {
        self.spent.set(true);
    }

    pub fn spent(&self) -> bool {
        self.spent.get()
    }

    /// The steps taken so far, including those of the take that spent the budget.
    pub fn taken(&self) -> usize {
        self.taken.get()
    }
}

impl Drop for Budget {
    fn drop(&mut self) {
        if RECORDING.load(Ordering::Relaxed) {
            let record = StepRecord {
                owner: self.owner,
                function: self.function.as_ref().map(|(name, _)| name.clone()),
                start: self.function.as_ref().map(|(_, start)| *start),
                limit: self.limit,
                taken: self.taken.get(),
                spent: self.spent.get(),
            };
            RECORDS
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(record);
        }
    }
}

/// What [`record_steps`] keeps of one [`Budget`] when it is dropped (`heki check --walk-steps`):
/// the step counts a shape check compares across sizes, without timing noise.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct StepRecord {
    pub owner: &'static str,
    /// The function's qualified name (`None`: an unmetered budget, over a whole rule run).
    pub function: Option<String>,
    /// The start of the function's span.
    pub start: Option<u32>,
    /// `None`: unmetered.
    pub limit: Option<usize>,
    pub taken: usize,
    pub spent: bool,
}

static RECORDING: AtomicBool = AtomicBool::new(false);
static RECORDS: Mutex<Vec<StepRecord>> = Mutex::new(Vec::new());

/// Keep a [`StepRecord`] of every budget created from now on, for [`recorded_steps`].
pub fn record_steps() {
    RECORDING.store(true, Ordering::Relaxed);
}

/// The records kept so far (they are removed), sorted: the order budgets are dropped in depends
/// on the threads.
pub fn recorded_steps() -> Vec<StepRecord> {
    let mut records = std::mem::take(&mut *RECORDS.lock().unwrap_or_else(PoisonError::into_inner));
    records.sort();
    records
}

// --------------------------------------------------------------------------------- nesting

/// How many [`Memo::ask`] calls may be nested on one thread. Deciding an answer can ask others
/// on the native stack, so a chain longer than this would overflow it; past it the walk answers
/// open and its owner's budget is spent.
pub const MAX_NESTED_ASKS: usize = 512;

thread_local! {
    static NESTED_ASKS: Cell<usize> = const { Cell::new(0) };
}

/// One level of [`NESTED_ASKS`], given back when dropped (also when the decision unwinds).
struct Nested;

impl Nested {
    fn enter() -> Option<Self> {
        NESTED_ASKS.with(|n| {
            let depth = n.get();
            (depth < MAX_NESTED_ASKS).then(|| {
                n.set(depth + 1);
                Self
            })
        })
    }
}

impl Drop for Nested {
    fn drop(&mut self) {
        NESTED_ASKS.with(|n| n.set(n.get() - 1));
    }
}

// ------------------------------------------------------------------------------------ memo

enum Slot<A> {
    Deciding,
    Known(A),
}

/// One question over keys, each answer decided once (one step). A key asked again while it is
/// being decided answers open: inside a cycle, the answers then depend on which key was asked
/// first. They are deterministic all the same, and the same in `heki check` and the language
/// server, which both analyze a whole function. A question that joins facts (a union, a may-set)
/// uses [`Closure`] or [`fixpoint`] instead, whose answers do not depend on the order.
///
/// A depth-limited question keeps the remaining depth in its key: a node first reached near the
/// limit must not answer for a caller asking it from the top. An answer is cloned on every ask
/// that finds it: keep a large one behind an `Rc`.
pub struct Memo<K, A> {
    slots: RefCell<FxHashMap<K, Slot<A>>>,
}

impl<K, A> Default for Memo<K, A> {
    fn default() -> Self {
        Self {
            slots: RefCell::default(),
        }
    }
}

impl<K: Clone + Eq + Hash, A: Clone> Memo<K, A> {
    pub fn new() -> Self {
        Self::default()
    }

    /// The answer for `key`, from `decide` the first time. `decide` may ask this memo again.
    pub fn ask(&self, budget: &Budget, key: K, decide: impl FnOnce() -> A) -> Answer<A> {
        if budget.spent() {
            return Answer::Open;
        }
        match self.slots.borrow().get(&key) {
            Some(Slot::Known(a)) => return Answer::Known(a.clone()),
            Some(Slot::Deciding) => return Answer::Open,
            None => {}
        }
        let Some(nested) = Nested::enter() else {
            budget.spend();
            return Answer::Open;
        };
        if !budget.take(1) {
            return Answer::Open;
        }
        self.slots.borrow_mut().insert(key.clone(), Slot::Deciding);
        let answer = decide();
        drop(nested);
        // An answer decided while the budget ran out may rest on open answers taken as facts.
        if budget.spent() {
            return Answer::Open;
        }
        self.slots
            .borrow_mut()
            .insert(key, Slot::Known(answer.clone()));
        Answer::Known(answer)
    }
}

// --------------------------------------------------------------------------------- closure

/// Per node, its own fact joined with the facts of every node it reaches. The members of a cycle
/// share one fact: the one a visited-set walk from any of them finds. Each node is expanded once
/// per `Closure`, however many starts are asked, for one step and the [`Join::weight`] of its
/// own fact. A component's fact joins its members' own facts and, once each, the facts of the
/// components they reach, whose weights it takes too: the work and the memory of the facts are
/// those steps, whatever the shape of the graph.
pub struct Closure<K, S> {
    component_of: RefCell<FxHashMap<K, usize>>,
    facts: RefCell<Vec<Rc<S>>>,
}

impl<K, S> Default for Closure<K, S> {
    fn default() -> Self {
        Self {
            component_of: RefCell::default(),
            facts: RefCell::default(),
        }
    }
}

/// A node of the walk of [`Closure::ask`]: its key, its own fact, and the nodes it reaches in
/// one step. Nodes are numbered in the order the walk finds them, which is their Tarjan index.
struct Expanded<K, S> {
    key: K,
    own: S,
    next: Vec<K>,
    low: usize,
    on_stack: bool,
}

impl<K: Copy + Eq + Hash, S: Join + Default> Closure<K, S> {
    pub fn new() -> Self {
        Self::default()
    }

    /// The fact of `start`: `own` of every node it reaches, `next` listing the nodes one step
    /// from a node. Neither may ask this closure again. The members of a component share one
    /// fact, so it is handed out behind an `Rc`.
    pub fn ask(
        &self,
        budget: &Budget,
        start: K,
        mut own: impl FnMut(K) -> S,
        mut next: impl FnMut(K, &mut Vec<K>),
    ) -> Answer<Rc<S>> {
        if budget.spent() {
            return Answer::Open;
        }
        if let Some(&c) = self.component_of.borrow().get(&start) {
            return Answer::Known(Rc::clone(&self.facts.borrow()[c]));
        }
        // Iterative Tarjan: the frames hold each node of the current path and its next
        // successor; the stack holds the nodes whose component is not complete yet.
        let mut nodes: Vec<Expanded<K, S>> = Vec::new();
        let mut index: FxHashMap<K, usize> = FxHashMap::default();
        let mut stack: Vec<usize> = Vec::new();
        let mut frames: Vec<(usize, usize)> = Vec::new();
        let mut expand = |key: K, nodes: &mut Vec<Expanded<K, S>>| {
            let own = own(key);
            if !budget.take(1 + own.weight()) {
                return None;
            }
            let id = nodes.len();
            let mut successors = Vec::new();
            next(key, &mut successors);
            nodes.push(Expanded {
                key,
                own,
                next: successors,
                low: id,
                on_stack: true,
            });
            Some(id)
        };
        let Some(root) = expand(start, &mut nodes) else {
            return Answer::Open;
        };
        index.insert(start, root);
        stack.push(root);
        frames.push((root, 0));
        while let Some(&mut (v, ref mut at)) = frames.last_mut() {
            if let Some(&w) = nodes[v].next.get(*at) {
                *at += 1;
                if self.component_of.borrow().contains_key(&w) {
                    continue;
                }
                match index.get(&w) {
                    Some(&wi) => {
                        if nodes[wi].on_stack {
                            nodes[v].low = nodes[v].low.min(wi);
                        }
                    }
                    None => {
                        let Some(wi) = expand(w, &mut nodes) else {
                            return Answer::Open;
                        };
                        index.insert(w, wi);
                        stack.push(wi);
                        frames.push((wi, 0));
                    }
                }
                continue;
            }
            frames.pop();
            if let Some(&(parent, _)) = frames.last() {
                nodes[parent].low = nodes[parent].low.min(nodes[v].low);
            }
            if nodes[v].low == v && !self.complete(budget, &mut nodes, &mut stack, v) {
                return Answer::Open;
            }
        }
        let c = self.component_of.borrow()[&start];
        Answer::Known(Rc::clone(&self.facts.borrow()[c]))
    }

    /// Pop the component `root` heads off `stack` and keep its fact: the own facts of its
    /// members and the facts of the components they reach. `false` once the budget is spent.
    fn complete(
        &self,
        budget: &Budget,
        nodes: &mut [Expanded<K, S>],
        stack: &mut Vec<usize>,
        root: usize,
    ) -> bool {
        let at = stack
            .iter()
            .rposition(|&x| x == root)
            .expect("a component's root is on the stack");
        let members = stack.split_off(at);
        let mut component_of = self.component_of.borrow_mut();
        let mut facts = self.facts.borrow_mut();
        let c = facts.len();
        for &m in &members {
            nodes[m].on_stack = false;
            component_of.insert(nodes[m].key, c);
        }
        let mut reached: Vec<usize> = members
            .iter()
            .flat_map(|&m| nodes[m].next.iter().map(|w| component_of[w]))
            .filter(|&cw| cw != c)
            .collect();
        reached.sort_unstable();
        reached.dedup();
        if !budget.take(reached.iter().map(|&cw| facts[cw].weight()).sum()) {
            return false;
        }
        let mut fact = S::default();
        for &m in &members {
            fact.join(&nodes[m].own);
        }
        for &cw in &reached {
            fact.join(&facts[cw]);
        }
        facts.push(Rc::new(fact));
        true
    }
}

// ----------------------------------------------------------------------------------- reach

/// What [`reach`] does at a node it visits.
pub enum Visit<T> {
    /// Go on to the nodes one step from it.
    Expand,
    /// Go on, but not through it.
    Prune,
    /// End the walk with this answer.
    Stop(T),
}

/// Walk from `starts` (visited in their order) over `next`, each node visited once (one step),
/// depth first: the answer of the first [`Visit::Stop`], or `None` when the walk ends without
/// one.
pub fn reach<K: Copy + Eq + Hash, T>(
    budget: &Budget,
    starts: impl IntoIterator<Item = K>,
    mut next: impl FnMut(K, &mut Vec<K>),
    mut visit: impl FnMut(K) -> Visit<T>,
) -> Answer<Option<T>> {
    if budget.spent() {
        return Answer::Open;
    }
    let mut stack: Vec<K> = starts.into_iter().collect();
    stack.reverse();
    let mut seen: FxHashSet<K> = FxHashSet::default();
    let mut successors = Vec::new();
    while let Some(k) = stack.pop() {
        if !seen.insert(k) {
            continue;
        }
        if !budget.take(1) {
            return Answer::Open;
        }
        match visit(k) {
            Visit::Stop(t) => return Answer::Known(Some(t)),
            Visit::Prune => {}
            Visit::Expand => {
                successors.clear();
                next(k, &mut successors);
                stack.extend(successors.iter().rev().filter(|w| !seen.contains(w)));
            }
        }
    }
    Answer::Known(None)
}

// -------------------------------------------------------------------------------- fixpoint

/// The nodes `0..nodes` of a [`fixpoint`]: `readers(i, out)` lists the nodes whose transfer
/// reads the fact of `i`, and `max_visits` bounds the visits of one node (the height of the
/// facts' lattice is enough for a monotone transfer).
pub struct Sparse<'s> {
    pub nodes: usize,
    pub readers: &'s dyn Fn(usize, &mut Vec<usize>),
    pub max_visits: u32,
}

/// The facts of a [`fixpoint`], by node (`None`: no fact).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts<L>(Vec<Option<L>>);

impl<L> Facts<L> {
    pub fn get(&self, i: usize) -> Option<&L> {
        self.0.get(i).and_then(Option::as_ref)
    }

    pub fn into_vec(self) -> Vec<Option<L>> {
        self.0
    }
}

/// The facts of nodes `0..spec.nodes` to a fixpoint. Each node is visited once in order, then
/// again only when a fact it reads grows, the earliest pending node first; `transfer` gives a
/// node's new fact (`None`: none yet), which is joined into the one it has, never put in its
/// place. A visit takes one step and the [`Join::weight`] of the fact it joins. A
/// must-question starts from every fact and only loses facts: its `Join` is the intersection.
/// Past `max_visits` visits of one node, or past the budget, the facts are open, and no finding
/// may come from them.
pub fn fixpoint<L: Join>(
    budget: &Budget,
    spec: Sparse<'_>,
    mut transfer: impl FnMut(usize, &Facts<L>) -> Option<L>,
) -> Answer<Facts<L>> {
    if budget.spent() {
        return Answer::Open;
    }
    let mut facts = Facts((0..spec.nodes).map(|_| None).collect());
    let mut pending: BinaryHeap<Reverse<usize>> = (0..spec.nodes).map(Reverse).collect();
    let mut queued = vec![true; spec.nodes];
    let mut visits = vec![0u32; spec.nodes];
    let mut readers = Vec::new();
    while let Some(Reverse(i)) = pending.pop() {
        queued[i] = false;
        visits[i] += 1;
        if visits[i] > spec.max_visits || !budget.take(1) {
            return Answer::Open;
        }
        let Some(new) = transfer(i, &facts) else {
            continue;
        };
        if !budget.take(new.weight()) {
            return Answer::Open;
        }
        let slot = &mut facts.0[i];
        let grew = match slot {
            Some(old) => old.join(&new),
            None => {
                *slot = Some(new);
                true
            }
        };
        if !grew {
            continue;
        }
        readers.clear();
        (spec.readers)(i, &mut readers);
        for &r in &readers {
            if !std::mem::replace(&mut queued[r], true) {
                pending.push(Reverse(r));
            }
        }
    }
    Answer::Known(facts)
}

// ------------------------------------------------------------------------------ shared memo

/// What an owner of a [`SharedMemo`] records of the declarations its analyses read (the
/// project declarations record `DeclKeys` for the language server's invalidation).
pub trait ReadLog {
    type Reads;

    /// Whether an analysis is recording what it reads.
    fn recording(&self) -> bool;

    /// Run `decide` while recording, apart from what the recording read before (which it keeps
    /// recording afterwards): its answer and what it read.
    fn isolated<A>(&self, decide: impl FnOnce() -> A) -> (A, Arc<Self::Reads>);

    /// Count `reads` as read by the recording analysis.
    fn replay(&self, reads: Arc<Self::Reads>);
}

/// Answers over declarations kept across functions and rule runs, until [`Self::clear`] (with
/// the indexes they are computed from). An answer kept while recording keeps what deciding it
/// read, and every later read of it while recording replays that, so an answer read from here
/// records the same dependencies as one decided again. The only engine shared between threads.
/// Deciding an answer must not ask for itself: there is no cycle detection across threads.
#[derive(Debug)]
pub struct SharedMemo<K, A, R> {
    slots: RwLock<FxHashMap<K, Kept<A, R>>>,
}

#[derive(Debug)]
struct Kept<A, R> {
    answer: A,
    /// `None`: decided while nothing recorded.
    reads: Option<Arc<R>>,
}

impl<K, A, R> Default for SharedMemo<K, A, R> {
    fn default() -> Self {
        Self {
            slots: RwLock::default(),
        }
    }
}

impl<K: Eq + Hash, A: Clone, R> SharedMemo<K, A, R> {
    pub fn new() -> Self {
        Self::default()
    }

    /// The answer for `key`, from `decide` the first time (and again the first time `log`
    /// records, to record what it reads).
    pub fn ask<L: ReadLog<Reads = R>>(&self, log: &L, key: K, decide: impl FnOnce() -> A) -> A {
        let recording = log.recording();
        let kept = self
            .slots
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .filter(|k| !recording || k.reads.is_some())
            .map(|k| (k.answer.clone(), k.reads.clone()));
        if let Some((answer, reads)) = kept {
            if let Some(reads) = reads.filter(|_| recording) {
                log.replay(reads);
            }
            return answer;
        }
        let (answer, reads) = if recording {
            let (answer, reads) = log.isolated(decide);
            log.replay(Arc::clone(&reads));
            (answer, Some(reads))
        } else {
            (decide(), None)
        };
        self.slots
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                key,
                Kept {
                    answer: answer.clone(),
                    reads,
                },
            );
        answer
    }

    pub fn clear(&mut self) {
        self.slots
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

#[cfg(test)]
mod tests;
