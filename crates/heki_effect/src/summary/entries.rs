//! The entries of the containers project-class attributes hold (`WriteKind::ItemMut`): through
//! item reads, copies, agreeing phis, the closure cells of enclosing functions, and the views
//! and iterators of the entries (`values()`, `items()`, a loop over them), as one fixpoint over
//! the values of a function on its own budget.

use super::{Extractor, ItemOf, ItemStep, ObjAttr, joined_item};
use heki_ir::walk::{Budget, Facts, Join, Sparse, fixpoint};
use heki_ir::{InstKind, PhiNode, VarId};
use rustc_hash::FxHashMap;

/// What a value hands out of the entries of a container a project-class attribute holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryView {
    /// `values()`, or an iterator over it: each step is an entry.
    Values,
    /// `items()`, or an iterator over it: each step is a `(key, entry)` pair.
    Items,
    /// One such pair: its second item is the entry.
    Pair,
}

/// What a value is of the entries of a container a project-class attribute holds. A value
/// with no fact yet is none of these: a phi joins the facts of the inputs that have one, so
/// that entries go round the cycles loops make.
#[derive(Debug, Clone, PartialEq)]
enum EntryFact {
    Entry(ItemOf),
    View(ItemOf, EntryView),
    /// No entry or view heki can name: neither, or different ones on different paths.
    Unnamed,
}

/// The most times an [`EntryFact`] grows: its key once, the objects' parameters and the
/// parameters they are reached from a bit at a time, the other parts of [`ItemOf`] once each,
/// and [`EntryFact::Unnamed`] last.
const ENTRY_FACT_HEIGHT: usize = 2 * 64 + 5;

impl Join for EntryFact {
    fn join(&mut self, other: &Self) -> bool {
        let joined = match (&*self, other) {
            (Self::Unnamed, _) => return false,
            (Self::Entry(a), Self::Entry(b)) => joined_item([Some(a), Some(b)]).map(Self::Entry),
            (Self::View(a, x), Self::View(b, y)) if x == y => {
                joined_item([Some(a), Some(b)]).map(|i| Self::View(i, *x))
            }
            _ => None,
        }
        .unwrap_or(Self::Unnamed);
        let grew = joined != *self;
        *self = joined;
        grew
    }
}

/// How the function makes a value that may be an entry, or hand entries out ([`EntryFact`]).
enum Made<'f> {
    /// A read of a closure cell that the enclosing function binds to entries
    /// ([`super::CellAlias`]).
    Cell(ItemOf),
    /// The object another value is ([`Extractor::same_object`]).
    Same(VarId),
    Phi(&'f PhiNode),
    /// `container[index]`, not a slice. `second`: the index is `1` or `-1`, which is the entry
    /// of a `(key, entry)` pair.
    Item {
        container: VarId,
        key_const: Option<&'f str>,
        index: VarId,
        second: bool,
    },
    /// A method outside the project handing out an element or a view of the elements
    /// ([`Extractor::element_view`]): `get` and `__getitem__` the entry under `key`, `values()`
    /// and `items()` a view.
    Access {
        container: VarId,
        key: Option<VarId>,
        view: Option<EntryView>,
    },
    /// `iter(view)`.
    Iter(VarId),
    /// A step of an iterator.
    Next(VarId),
}

impl Made<'_> {
    /// The values whose facts the fact of this one is made from.
    fn sources(&self, out: &mut Vec<VarId>) {
        match self {
            Made::Cell(_) => {}
            Made::Same(v) | Made::Iter(v) | Made::Next(v) => out.push(*v),
            Made::Phi(phi) => out.extend(phi.incoming.iter().map(|(_, v)| *v)),
            Made::Item { container, .. } | Made::Access { container, .. } => out.push(*container),
        }
    }
}

impl<'a> Extractor<'a> {
    /// [`Self::item_alias`]: the entries of the containers project-class attributes hold,
    /// decided after the other alias maps, which none of them reads. An entry is followed only
    /// with an ownership contract and below a typed attribute (most functions have none); the
    /// closure cells of enclosing functions hand theirs in whatever the contracts. Past the
    /// budget or the visits of one value the entries are unknown: no value is an entry.
    pub(super) fn build_entry_aliases(&mut self) {
        let cells = std::mem::take(&mut self.item_alias);
        let follows = self.ownership && !(self.obj_attr_alias.is_empty() && cells.is_empty());
        if cells.is_empty() && !follows {
            return;
        }
        let made = self.entry_values(cells, follows);
        let index: FxHashMap<VarId, usize> =
            made.iter().enumerate().map(|(i, (v, _))| (*v, i)).collect();
        let mut readers: Vec<Vec<usize>> = vec![Vec::new(); made.len()];
        let mut sources = Vec::new();
        let mut most_sources = 0;
        for (i, (_, m)) in made.iter().enumerate() {
            sources.clear();
            m.sources(&mut sources);
            most_sources = most_sources.max(sources.len());
            for s in &sources {
                if let Some(&j) = index.get(s) {
                    readers[j].push(i);
                }
            }
        }
        let spec = Sparse {
            nodes: made.len(),
            readers: &|i, out| out.extend(&readers[i]),
            // Each visit after the first follows the growth of a fact it reads.
            max_visits: u32::try_from(1 + most_sources * ENTRY_FACT_HEIGHT).unwrap_or(u32::MAX),
        };
        let budget = Budget::of_function("summary::entry_aliases", self.f);
        let transfer = |i: usize, facts: &Facts<EntryFact>| {
            let fact_of = |v: VarId| index.get(&v).map(|j| facts.get(*j));
            self.entry_fact(&made[i].1, fact_of)
        };
        let facts = fixpoint(&budget, spec, transfer).map(Facts::into_vec);
        for ((v, _), fact) in made.iter().zip(facts.open_means(Vec::new())) {
            if let Some(EntryFact::Entry(item)) = fact {
                self.item_alias.insert(*v, item);
            }
        }
    }

    /// The values of the function that may be entries or hand them out, in block order, each
    /// with how it is made: `cells` the reads of closure cells bound to entries, and with
    /// `follows` the item reads, views, and iterators of the containers.
    fn entry_values(
        &mut self,
        mut cells: FxHashMap<VarId, ItemOf>,
        follows: bool,
    ) -> Vec<(VarId, Made<'a>)> {
        let f = self.f;
        let mut made = Vec::new();
        for bb in &f.blocks {
            self.pos = (bb.id, 0);
            for phi in &bb.phis {
                made.push((phi.dst, Made::Phi(phi)));
            }
            for inst in &bb.insts {
                let Some(dst) = inst.kind.dst() else {
                    continue;
                };
                if let Some(item) = cells.remove(&dst) {
                    made.push((dst, Made::Cell(item)));
                } else if let Some(src) = self.same_objects.get(&dst) {
                    made.push((dst, Made::Same(*src)));
                } else if follows && let Some(m) = self.made_entry(&inst.kind) {
                    made.push((dst, m));
                }
            }
        }
        made
    }

    /// How the instruction `kind` makes an entry, a view, or an iterator of entries, of
    /// whatever container it reads.
    fn made_entry(&self, kind: &'a InstKind) -> Option<Made<'a>> {
        match kind {
            InstKind::SubscriptLoad {
                obj,
                index,
                key_const,
                is_slice: false,
                ..
            } => Some(Made::Item {
                container: *obj,
                key_const: key_const.as_deref(),
                index: *index,
                second: matches!(self.shape.int_const(*index), Some(1 | -1)),
            }),
            InstKind::MakeIter { iterable, .. } => Some(Made::Iter(*iterable)),
            InstKind::IterNext { iter, .. } => Some(Made::Next(*iter)),
            InstKind::MethodCall { method, .. } => {
                let Some((container, ItemStep::Items(key))) = self.element_view(kind) else {
                    return None;
                };
                let view = match method.as_str() {
                    "values" => Some(EntryView::Values),
                    "items" => Some(EntryView::Items),
                    _ => None,
                };
                Some(Made::Access {
                    container,
                    key,
                    view,
                })
            }
            _ => None,
        }
    }

    /// The fact of a value made as `made`, from the facts `fact_of` gives the values it reads:
    /// `Some(None)` for a value that has none yet, `None` for one that is no entry or view
    /// (made by nothing [`Self::entry_values`] lists). A phi joins the inputs that have one;
    /// any other value waits for the fact it reads.
    fn entry_fact<'m>(
        &self,
        made: &Made<'_>,
        fact_of: impl Fn(VarId) -> Option<Option<&'m EntryFact>>,
    ) -> Option<EntryFact> {
        let read = |v: VarId| match fact_of(v) {
            Some(fact) => fact.map(Read::Fact),
            None => Some(Read::Unnamed),
        };
        let fact = match made {
            Made::Cell(item) => EntryFact::Entry(item.clone()),
            Made::Same(src) => read(*src)?.cloned(),
            Made::Phi(phi) => {
                let mut joined: Option<EntryFact> = None;
                for (_, v) in phi.incoming.iter().filter(|(_, v)| *v != phi.dst) {
                    let Some(fact) = read(*v).map(Read::cloned) else {
                        continue;
                    };
                    match &mut joined {
                        Some(j) => {
                            j.join(&fact);
                        }
                        None => joined = Some(fact),
                    }
                }
                joined?
            }
            Made::Item {
                container,
                key_const,
                index,
                second,
            } => match read(*container)? {
                Read::Fact(EntryFact::View(item, EntryView::Pair)) if *second => {
                    EntryFact::Entry(item.clone())
                }
                fact => self.entry_below(*container, fact, || self.key_of(*key_const, *index)),
            },
            Made::Access {
                container,
                key,
                view,
            } => {
                let key = || key.and_then(|k| self.key_of(None, k));
                match (self.entry_below(*container, read(*container)?, key), view) {
                    (EntryFact::Entry(item), Some(view)) => EntryFact::View(item, *view),
                    (fact, _) => fact,
                }
            }
            Made::Iter(src) => match read(*src)? {
                Read::Fact(fact @ EntryFact::View(..)) => fact.clone(),
                _ => EntryFact::Unnamed,
            },
            Made::Next(src) => match read(*src)? {
                Read::Fact(EntryFact::View(item, EntryView::Values)) => {
                    EntryFact::Entry(item.clone())
                }
                Read::Fact(EntryFact::View(item, EntryView::Items)) => {
                    EntryFact::View(item.clone(), EntryView::Pair)
                }
                _ => EntryFact::Unnamed,
            },
        };
        Some(fact)
    }

    /// The entry under `key` of `container`, whose fact is `fact`: an entry of an entry is one
    /// of the attribute's container, under its first key.
    fn entry_below(
        &self,
        container: VarId,
        fact: Read<'_>,
        key: impl FnOnce() -> Option<String>,
    ) -> EntryFact {
        match fact {
            Read::Fact(EntryFact::Entry(outer)) => EntryFact::Entry(outer.clone()),
            _ => match self.obj_attr_alias.get(&container) {
                Some(a) => EntryFact::Entry(entry_of_attr(a, key())),
                None => EntryFact::Unnamed,
            },
        }
    }
}

/// The fact a value read has ([`Extractor::entry_fact`]): its own, or none for a value that
/// is no entry or view.
#[derive(Clone, Copy)]
enum Read<'m> {
    Fact(&'m EntryFact),
    Unnamed,
}

impl Read<'_> {
    fn cloned(self) -> EntryFact {
        match self {
            Read::Fact(fact) => fact.clone(),
            Read::Unnamed => EntryFact::Unnamed,
        }
    }
}

/// An entry under `key` of the container the attribute load `a` gives.
fn entry_of_attr(a: &ObjAttr, key: Option<String>) -> ItemOf {
    ItemOf {
        classes: a.classes.clone(),
        attr: a.attr.clone(),
        key,
        obj_params: a.obj_params,
        fresh: a.fresh,
        reach: a.reach,
        holder: a.holder,
    }
}
