//! What a load of a constant key of a builtin container names: the values the function stored at
//! that key on the paths reaching the load, as a local variable names what was bound to it.
//!
//! A slot is a key of one container (its copy root): a string, or an integer that is not negative.
//! Only containers whose inferred class is a builtin
//! mapping or list are followed: there an item store and an item load name the same object, which
//! an array, a data frame, `os.environ`, or a class with its own `__setitem__` does not promise.
//! A data flow over the function's blocks carries, per slot, the stores that may reach a point,
//! and whether the container's earlier contents may (what it held before the function stored
//! anything, or what a change the flow does not follow put there).
//!
//! A slot is opened (its earlier contents again, no store) by what may change it unseen: a store
//! with another key, a deletion, a changing method, an in-place operator, an attribute read of the
//! container (a bound method holds it), or a call it is handed to. A container that others may
//! hold, because the function did not make it, because it is a phi's input, or because it was
//! stored, captured, yielded, or handed to a call, is also opened by every call, `await`, `yield`
//! and `with`, and its slot by a store with the same key or an unknown one through any other
//! container.

use super::Live;
use crate::readonly::container_mutator;
use heki_ir::walk::{BitSet, Budget, Facts, FnShape, Join, Sparse, fixpoint};
use heki_ir::{BlockId, DictEntry, Inst, InstKind, IrFunction, VarId};
use rustc_hash::{FxHashMap, FxHashSet};

/// The classes whose item load names the object the item store stored.
const SLOT_CLASSES: &[&str] = &[
    "builtins.dict",
    "builtins.list",
    "collections.defaultdict",
    "collections.OrderedDict",
    "collections.Counter",
];

/// The slots one function follows at most: one bit of [`Contents::escaped`] per container.
const MAX_SLOTS: usize = 64;

/// The values one slot follows at most, one word of its stores: a key the function stores more
/// values at is not followed, so that a block's fact is a word per slot.
const MAX_VALUES_PER_SLOT: usize = 64;

/// The stores one load may read at most: a load that more may reach reads the container's item,
/// so that the values the loads of a function name stay proportional to its loads.
const MAX_STORES_PER_READ: usize = 16;

/// What a load of a slot reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlotRead {
    /// The values stored at the slot on the paths that reach the load.
    pub stored: Vec<VarId>,
    /// Some path reaches the load with the container's earlier contents: what the load's own
    /// source says.
    pub earlier: bool,
}

/// The loads of a function that read a slot some store reaches ([`Slots::in_function`]).
#[derive(Debug, Default)]
pub struct Slots {
    reads: FxHashMap<VarId, SlotRead>,
}

impl Slots {
    /// What the load defining `load` reads, when some store reaches it.
    pub fn read(&self, load: VarId) -> Option<&SlotRead> {
        self.reads.get(&load)
    }

    /// The slot reads of `f`. `class_of_value` is the inferred class of a value,
    /// `made_by_function` whether the object it holds is one the function makes itself. Past the
    /// function's budget, no load reads a slot.
    pub fn in_function<'t>(
        f: &IrFunction,
        shape: &FnShape<'_>,
        live: &Live,
        class_of_value: &dyn Fn(VarId) -> Option<&'t str>,
        made_by_function: &dyn Fn(VarId) -> bool,
    ) -> Self {
        let Some(layout) = Layout::scan(f, shape, live, class_of_value, made_by_function) else {
            return Self::default();
        };
        let flow = Flow {
            f,
            shape,
            live,
            layout: &layout,
        };
        let nodes: Vec<BlockId> = shape
            .rpo()
            .iter()
            .copied()
            .filter(|b| live.block(*b))
            .collect();
        let node_of: FxHashMap<BlockId, usize> =
            nodes.iter().enumerate().map(|(i, b)| (*b, i)).collect();
        let readers = |i: usize, out: &mut Vec<usize>| {
            let b = nodes[i];
            out.extend(
                shape
                    .succs(b)
                    .filter(|to| live.edge(b, *to))
                    .filter_map(|to| node_of.get(&to).copied()),
            );
        };
        // Each visit after the first adds a store, the earlier contents of a slot, or an escaped
        // container, at the end of the block or at some point in it.
        let values: usize = layout.values.iter().map(Vec::len).sum();
        let height = values + layout.slot_container.len() + layout.containers.len();
        let spec = Sparse {
            nodes: nodes.len(),
            readers: &readers,
            max_visits: u32::try_from(1 + 2 * height).unwrap_or(u32::MAX),
        };
        let budget = Budget::of_function("ownership::Slots", f);
        let found = fixpoint(&budget, spec, |i, facts| {
            let entry = flow.entry_of(nodes[i], &|p| facts.get(*node_of.get(&p)?))?;
            Some(flow.through_block(nodes[i], entry, &mut None))
        });
        // Open, no block has a fact, and no load reads a slot.
        let facts = found.map(Facts::into_vec).open_means(Vec::new());
        let fact_of = |p: BlockId| facts.get(*node_of.get(&p)?)?.as_ref();
        let mut reads = FxHashMap::default();
        for b in nodes.iter().filter(|b| fact_of(**b).is_some()) {
            if let Some(entry) = flow.entry_of(*b, &fact_of) {
                flow.through_block(*b, entry, &mut Some(&mut reads));
            }
        }
        Self { reads }
    }
}

/// A key of a container: a string or an integer that is not negative. `True` equals the key `1`:
/// a store with it opens every key of the container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Key<'f> {
    Str(&'f str),
    Int(i64),
}

/// The key the value `index` is, when it is one.
fn slot_key<'f>(shape: &FnShape<'f>, index: VarId) -> Option<Key<'f>> {
    let index = shape.copy_root(index);
    if let Some(s) = shape.str_const(index) {
        return Some(Key::Str(s));
    }
    // A negative index counts from the end of a list, so it may name any other.
    let int = shape.int_const(index)?;
    (int >= 0).then_some(Key::Int(int))
}

/// The slots followed in one function: those both stored and loaded.
struct Layout<'f> {
    /// The containers with a slot, by copy root.
    containers: FxHashMap<VarId, usize>,
    /// Per container, whether others may hold it from its definition on: the function did not
    /// make it, or it is a phi's input.
    shared: Vec<bool>,
    /// Per container, its slots.
    slots_of: Vec<Vec<usize>>,
    slots: FxHashMap<(usize, Key<'f>), usize>,
    /// Per slot, its container.
    slot_container: Vec<usize>,
    /// The values stored at a slot, by `(slot, value)`: [`Contents`] names them by their
    /// position among the slot's values.
    value_ids: FxHashMap<(usize, VarId), usize>,
    values: Vec<Vec<VarId>>,
}

impl<'f> Layout<'f> {
    fn scan<'t>(
        f: &'f IrFunction,
        shape: &FnShape<'f>,
        live: &Live,
        class_of_value: &dyn Fn(VarId) -> Option<&'t str>,
        made_by_function: &dyn Fn(VarId) -> bool,
    ) -> Option<Self> {
        let followed = |obj: VarId| {
            let root = shape.copy_root(obj);
            class_of_value(root)
                .is_some_and(|c| SLOT_CLASSES.contains(&c))
                .then_some(root)
        };
        let mut stored: Vec<(VarId, Key<'f>, VarId)> = Vec::new();
        let mut loaded: FxHashSet<(VarId, Key<'f>)> = FxHashSet::default();
        let insts = f
            .blocks
            .iter()
            .filter(|b| live.block(b.id))
            .flat_map(|b| &b.insts);
        for inst in insts {
            match &inst.kind {
                InstKind::SubscriptStore {
                    obj,
                    index,
                    value,
                    is_slice: false,
                    ..
                } => {
                    if let (Some(root), Some(key)) = (followed(*obj), slot_key(shape, *index)) {
                        stored.push((root, key, *value));
                    }
                }
                InstKind::DictLiteral { dst, entries } => {
                    let root = followed(*dst);
                    stored.extend(entries.iter().filter_map(|e| match e {
                        DictEntry::KeyValue { key, value } => {
                            Some((root?, slot_key(shape, *key)?, *value))
                        }
                        DictEntry::Unpack { .. } => None,
                    }));
                }
                kind => loaded.extend(
                    slot_load(kind)
                        .and_then(|(obj, index)| followed(obj).zip(slot_key(shape, index))),
                ),
            }
        }
        let mut layout = Self {
            containers: FxHashMap::default(),
            shared: Vec::new(),
            slots_of: Vec::new(),
            slots: FxHashMap::default(),
            slot_container: Vec::new(),
            value_ids: FxHashMap::default(),
            values: Vec::new(),
        };
        let mut values_at: FxHashMap<(VarId, Key<'f>), FxHashSet<VarId>> = FxHashMap::default();
        for &(root, key, value) in &stored {
            values_at.entry((root, key)).or_default().insert(value);
        }
        for &(root, key, _) in &stored {
            if layout.slot_container.len() == MAX_SLOTS
                || !loaded.contains(&(root, key))
                || values_at[&(root, key)].len() > MAX_VALUES_PER_SLOT
            {
                continue;
            }
            let next = layout.containers.len();
            let c = *layout.containers.entry(root).or_insert(next);
            if c == next {
                layout.shared.push(!made_by_function(root));
                layout.slots_of.push(Vec::new());
            }
            if !layout.slots.contains_key(&(c, key)) {
                let s = layout.slot_container.len();
                layout.slots.insert((c, key), s);
                layout.slot_container.push(c);
                layout.slots_of[c].push(s);
                layout.values.push(Vec::new());
            }
        }
        for (root, key, value) in stored {
            let slot = layout
                .containers
                .get(&root)
                .and_then(|c| layout.slots.get(&(*c, key)).copied());
            if let Some(s) = slot {
                layout.value_id(s, value);
            }
        }
        if layout.slots.is_empty() {
            return None;
        }
        for phi in f
            .blocks
            .iter()
            .filter(|b| live.block(b.id))
            .flat_map(|b| &b.phis)
        {
            for (_, v) in &phi.incoming {
                if let Some(&c) = layout.containers.get(&shape.copy_root(*v)) {
                    layout.shared[c] = true;
                }
            }
        }
        Some(layout)
    }

    fn container_of(&self, shape: &FnShape<'_>, v: VarId) -> Option<usize> {
        self.containers.get(&shape.copy_root(v)).copied()
    }

    fn value_id(&mut self, slot: usize, value: VarId) {
        let next = self.values[slot].len();
        if *self.value_ids.entry((slot, value)).or_insert(next) == next {
            self.values[slot].push(value);
        }
    }
}

/// The container and the index of a load of an item: `obj[index]`, or `obj.get(index[,
/// default])`.
fn slot_load(kind: &InstKind) -> Option<(VarId, VarId)> {
    match kind {
        InstKind::SubscriptLoad {
            obj,
            index,
            is_slice: false,
            ..
        } => Some((*obj, *index)),
        InstKind::MethodCall {
            recv, method, args, ..
        } if method == "get" && (1..=2).contains(&args.len()) => Some((*recv, args[0])),
        _ => None,
    }
}

/// One slot at a point: the stores that may reach it, and whether the container's earlier
/// contents may.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Slot {
    stores: BitSet,
    earlier: bool,
}

impl Slot {
    fn earlier() -> Self {
        Self {
            stores: BitSet::new(),
            earlier: true,
        }
    }
}

/// The slots of a function at a point, and the containers others may hold there (bit `i`:
/// container `i`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Contents {
    slots: Vec<Slot>,
    escaped: u64,
}

impl Join for Contents {
    /// Facts of one function have one entry per slot, but a block without an exception edge
    /// carries none on it.
    fn join(&mut self, other: &Self) -> bool {
        let mut grew = other.escaped & !self.escaped != 0;
        self.escaped |= other.escaped;
        for (mine, theirs) in self.slots.iter_mut().zip(&other.slots) {
            grew |= mine.stores.join(&theirs.stores);
            grew |= mine.earlier.join(&theirs.earlier);
        }
        grew
    }

    fn weight(&self) -> usize {
        self.slots.iter().map(|s| s.stores.weight()).sum::<usize>() + 1
    }
}

/// A block's fact: its slots at its end, and joined over every point in it (what its exception
/// edge carries).
type BlockContents = (Contents, Contents);

struct Flow<'f, 's> {
    f: &'f IrFunction,
    shape: &'s FnShape<'f>,
    live: &'s Live,
    layout: &'s Layout<'f>,
}

impl<'f> Flow<'f, '_> {
    /// The slots where `b` starts: the function's entry, joined with what each predecessor that
    /// has a fact carries on its edge to `b` (`None`: none has one yet).
    fn entry_of<'x>(
        &self,
        b: BlockId,
        fact_of: &dyn Fn(BlockId) -> Option<&'x BlockContents>,
    ) -> Option<Contents> {
        let mut entry: Option<Contents> = None;
        let mut add = |c: &Contents| match &mut entry {
            Some(e) => {
                e.join(c);
            }
            None => entry = Some(c.clone()),
        };
        if self.f.blocks.first().is_some_and(|first| first.id == b) {
            add(&self.start_contents());
        }
        for &p in self
            .shape
            .preds(b)
            .iter()
            .filter(|p| self.live.edge(**p, b))
        {
            let Some((end, raised)) = fact_of(p) else {
                continue;
            };
            let pred = self.f.block(p);
            if pred.terminator.successors().contains(&b) {
                add(end);
            }
            if pred.exc_target == Some(b) {
                add(raised);
            }
        }
        entry
    }

    /// The function's entry: every slot holds its earlier contents, and the containers the
    /// function did not make are held by others.
    fn start_contents(&self) -> Contents {
        let layout = self.layout;
        let escaped = (0..layout.containers.len())
            .filter(|c| layout.shared[*c])
            .fold(0u64, |m, c| m | 1u64 << c);
        Contents {
            slots: vec![Slot::earlier(); layout.slot_container.len()],
            escaped,
        }
    }

    /// The facts of `b` from its entry; with `reads`, what each of its slot loads reads.
    fn through_block(
        &self,
        b: BlockId,
        mut at: Contents,
        reads: &mut Option<&mut FxHashMap<VarId, SlotRead>>,
    ) -> BlockContents {
        let block = self.f.block(b);
        for phi in &block.phis {
            self.define_container(&mut at, phi.dst);
        }
        if block.exc_target.is_none() {
            for inst in &block.insts {
                self.visit_inst(inst, &mut at, reads);
            }
            return (at, Contents::default());
        }
        let mut raised = at.clone();
        for inst in &block.insts {
            let before = at.clone();
            self.visit_inst(inst, &mut at, reads);
            if at != before {
                raised.join(&at);
            }
        }
        (at, raised)
    }

    /// Record what `inst` reads of a slot, then apply it.
    fn visit_inst(
        &self,
        inst: &Inst,
        at: &mut Contents,
        reads: &mut Option<&mut FxHashMap<VarId, SlotRead>>,
    ) {
        if let Some(reads) = reads.as_deref_mut() {
            self.record_read(inst, at, reads);
        }
        self.apply_inst(inst, at);
    }

    /// Record what the load `inst` reads, when it loads a slot some store reaches.
    fn record_read(&self, inst: &Inst, at: &Contents, reads: &mut FxHashMap<VarId, SlotRead>) {
        let (Some((obj, index)), Some(dst)) = (slot_load(&inst.kind), inst.kind.dst()) else {
            return;
        };
        let Some(s) = self.slot_of_load(obj, index) else {
            return;
        };
        let slot = &at.slots[s];
        let stored: Vec<VarId> = slot
            .stores
            .iter()
            .take(MAX_STORES_PER_READ + 1)
            .map(|i| self.layout.values[s][i])
            .collect();
        if stored.is_empty() || stored.len() > MAX_STORES_PER_READ {
            return;
        }
        reads.insert(
            dst,
            SlotRead {
                stored,
                earlier: slot.earlier,
            },
        );
    }

    fn slot_of_load(&self, obj: VarId, index: VarId) -> Option<usize> {
        let c = self.layout.container_of(self.shape, obj)?;
        let key = slot_key(self.shape, index)?;
        self.layout.slots.get(&(c, key)).copied()
    }

    /// Apply `inst` to the slots `at`.
    fn apply_inst(&self, inst: &Inst, at: &mut Contents) {
        let layout = self.layout;
        let container = |v: VarId| layout.container_of(self.shape, v);
        match &inst.kind {
            InstKind::SubscriptStore {
                obj,
                index,
                value,
                is_slice,
                ..
            } => {
                self.store_at_key(at, *obj, (!is_slice).then_some(*index), *value);
                self.mark_escaped(at, *value);
            }
            InstKind::SubscriptDelete { obj, index, .. } => match container(*obj) {
                // Deleting an item of a list moves the items after it.
                Some(c) => self.open_container(at, c),
                None => self.open_key_through(at, slot_key(self.shape, *index)),
            },
            InstKind::DictLiteral { dst, entries } => {
                self.define_container(at, *dst);
                for e in entries {
                    match e {
                        DictEntry::KeyValue { key, value } => {
                            self.store_at_key(at, *dst, Some(*key), *value);
                            self.mark_escaped(at, *value);
                        }
                        DictEntry::Unpack { .. } => {
                            if let Some(c) = container(*dst) {
                                self.open_container(at, c);
                            }
                        }
                    }
                }
            }
            InstKind::MethodCall {
                dst,
                recv,
                method,
                args,
                kwargs,
                ..
            } => {
                match container(*recv) {
                    Some(c) if container_mutator(method) => self.open_container(at, c),
                    // A builtin container's other methods run no code of the program.
                    Some(_) => {}
                    None => self.open_escaped(at),
                }
                for v in args.iter().chain(kwargs.iter().map(|(_, v)| v)) {
                    self.hand_container(at, *v);
                }
                self.define_container(at, *dst);
            }
            InstKind::Call {
                dst,
                callee_var,
                args,
                kwargs,
                ..
            } => {
                self.open_escaped(at);
                let handed = args.iter().chain(kwargs.iter().map(|(_, v)| v));
                for v in handed.chain(callee_var.iter()) {
                    self.hand_container(at, *v);
                }
                self.define_container(at, *dst);
            }
            InstKind::BinOp {
                dst,
                lhs,
                in_place: true,
                ..
            } => {
                if let Some(c) = container(*lhs) {
                    self.open_container(at, c);
                }
                self.define_container(at, *dst);
            }
            InstKind::Await { .. }
            | InstKind::Yield { .. }
            | InstKind::YieldFrom { .. }
            | InstKind::EnterWith { .. }
            | InstKind::ExitWith { .. } => {
                self.open_escaped(at);
                for v in inst.kind.operands() {
                    self.hand_container(at, v);
                }
                if let Some(dst) = inst.kind.dst() {
                    self.define_container(at, dst);
                }
            }
            InstKind::AttrStore { value, .. }
            | InstKind::StoreGlobal { value, .. }
            | InstKind::StoreDeref { value, .. } => self.mark_escaped(at, *value),
            InstKind::AllocBuiltin { dst, elems, .. } => {
                for v in elems {
                    self.mark_escaped(at, *v);
                }
                self.define_container(at, *dst);
            }
            InstKind::Const { .. }
            | InstKind::Copy { .. }
            | InstKind::Compare { .. }
            | InstKind::UnaryOp { .. }
            | InstKind::Convert { .. }
            | InstKind::Format { .. }
            | InstKind::BuildString { .. }
            | InstKind::MakeIter { .. }
            | InstKind::IterNext { .. }
            | InstKind::BinOp { .. }
            | InstKind::SubscriptLoad { .. } => {
                if let Some(dst) = inst.kind.dst() {
                    self.define_container(at, dst);
                }
            }
            // An attribute read of a container is a bound method holding it; a capture or anything
            // else hands it on.
            kind => {
                for v in kind.operands() {
                    self.hand_container(at, v);
                }
                if let Some(dst) = kind.dst() {
                    self.define_container(at, dst);
                }
            }
        }
    }

    /// A store of `value` at `index` of `obj` (`None`: an index that names no one key).
    fn store_at_key(&self, at: &mut Contents, obj: VarId, index: Option<VarId>, value: VarId) {
        let key = index.and_then(|i| slot_key(self.shape, i));
        let Some(c) = self.layout.container_of(self.shape, obj) else {
            self.open_key_through(at, key);
            return;
        };
        match key {
            Some(key) => {
                if let Some(&s) = self.layout.slots.get(&(c, key)) {
                    let id = self
                        .layout
                        .value_ids
                        .get(&(s, value))
                        .copied()
                        .expect("every store of a followed slot has its value");
                    let mut stores = BitSet::new();
                    stores.insert(id);
                    at.slots[s] = Slot {
                        stores,
                        earlier: false,
                    };
                }
            }
            None => self.open_container(at, c),
        }
    }

    /// A store or deletion through a container that is not followed: it may be one others hold.
    fn open_key_through(&self, at: &mut Contents, key: Option<Key<'_>>) {
        for c in self.escaped_containers(at) {
            match key {
                Some(key) => {
                    if let Some(&s) = self.layout.slots.get(&(c, key)) {
                        at.slots[s] = Slot::earlier();
                    }
                }
                None => self.open_container(at, c),
            }
        }
    }

    /// `v` handed to code that may keep or change it.
    fn hand_container(&self, at: &mut Contents, v: VarId) {
        if let Some(c) = self.layout.container_of(self.shape, v) {
            self.open_container(at, c);
            at.escaped |= 1u64 << c;
        }
    }

    /// `v` stored where others may read it.
    fn mark_escaped(&self, at: &mut Contents, v: VarId) {
        if let Some(c) = self.layout.container_of(self.shape, v) {
            at.escaped |= 1u64 << c;
        }
    }

    /// A container defined anew by `dst`: its slots hold nothing stored yet.
    fn define_container(&self, at: &mut Contents, dst: VarId) {
        if let Some(&c) = self.layout.containers.get(&dst) {
            self.open_container(at, c);
            at.escaped &= !(1u64 << c);
            if self.layout.shared[c] {
                at.escaped |= 1u64 << c;
            }
        }
    }

    fn open_container(&self, at: &mut Contents, c: usize) {
        for &s in &self.layout.slots_of[c] {
            at.slots[s] = Slot::earlier();
        }
    }

    fn open_escaped(&self, at: &mut Contents) {
        for c in self.escaped_containers(at) {
            self.open_container(at, c);
        }
    }

    fn escaped_containers(&self, at: &Contents) -> Vec<usize> {
        (0..self.layout.containers.len())
            .filter(|c| at.escaped & (1u64 << c) != 0)
            .collect()
    }
}
