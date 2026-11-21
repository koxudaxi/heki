//! Facts that only grow, for [`super::Closure`] and [`super::fixpoint`].

use std::collections::BTreeMap;

/// A fact that only grows: `join` takes in `other` and returns whether `self` grew. `weight` is
/// what keeping the fact costs, taken from the budget when it is kept, so the memory of kept
/// facts is bounded by the budget too.
pub trait Join: Clone + PartialEq {
    fn join(&mut self, other: &Self) -> bool;

    fn weight(&self) -> usize {
        1
    }
}

/// `true` once either is.
impl Join for bool {
    fn join(&mut self, other: &Self) -> bool {
        let grew = *other && !*self;
        *self |= *other;
        grew
    }
}

/// Both parts, each joined with its own.
impl<A: Join, B: Join> Join for (A, B) {
    fn join(&mut self, other: &Self) -> bool {
        let a = self.0.join(&other.0);
        let b = self.1.join(&other.1);
        a || b
    }

    fn weight(&self) -> usize {
        self.0.weight() + self.1.weight()
    }
}

/// The smallest wins (the tightest of several caps).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Min<T>(pub T);

impl<T: Ord + Clone> Join for Min<T> {
    fn join(&mut self, other: &Self) -> bool {
        let grew = other.0 < self.0;
        if grew {
            self.0 = other.0.clone();
        }
        grew
    }
}

/// A bit mask (parameters, loops), the union of the masks joined.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Or(pub u64);

impl Join for Or {
    fn join(&mut self, other: &Self) -> bool {
        let grew = other.0 & !self.0 != 0;
        self.0 |= other.0;
        grew
    }
}

/// A set of keys, sorted without duplicates: a result listing a key twice cannot be built.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeySet<K>(Vec<K>);

impl<K> Default for KeySet<K> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<K: Ord + Clone> KeySet<K> {
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` when `key` was not in the set.
    pub fn insert(&mut self, key: K) -> bool {
        match self.0.binary_search(&key) {
            Ok(_) => false,
            Err(at) => {
                self.0.insert(at, key);
                true
            }
        }
    }

    pub fn contains(&self, key: &K) -> bool {
        self.0.binary_search(key).is_ok()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_slice(&self) -> &[K] {
        &self.0
    }

    pub fn iter(&self) -> std::slice::Iter<'_, K> {
        self.0.iter()
    }
}

impl<K: Ord> FromIterator<K> for KeySet<K> {
    fn from_iter<I: IntoIterator<Item = K>>(keys: I) -> Self {
        let mut keys: Vec<K> = keys.into_iter().collect();
        keys.sort();
        keys.dedup();
        Self(keys)
    }
}

impl<K: Ord + Clone> Join for KeySet<K> {
    fn join(&mut self, other: &Self) -> bool {
        let (mine, theirs) = (&self.0, &other.0);
        let mut merged = Vec::with_capacity(mine.len() + theirs.len());
        let (mut i, mut j) = (0, 0);
        while i < mine.len() && j < theirs.len() {
            match mine[i].cmp(&theirs[j]) {
                std::cmp::Ordering::Less => {
                    merged.push(mine[i].clone());
                    i += 1;
                }
                std::cmp::Ordering::Greater => {
                    merged.push(theirs[j].clone());
                    j += 1;
                }
                std::cmp::Ordering::Equal => {
                    merged.push(mine[i].clone());
                    i += 1;
                    j += 1;
                }
            }
        }
        merged.extend_from_slice(&mine[i..]);
        merged.extend_from_slice(&theirs[j..]);
        let grew = merged.len() > mine.len();
        self.0 = merged;
        grew
    }

    fn weight(&self) -> usize {
        self.0.len().max(1)
    }
}

/// One entry per key, the values of a key joined: an ordered result with no duplicate key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinMap<K, V>(BTreeMap<K, V>);

impl<K, V> Default for JoinMap<K, V> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

impl<K: Ord + Clone, V: Join> JoinMap<K, V> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Join `value` into the entry of `key`: whether the map grew.
    pub fn insert(&mut self, key: K, value: V) -> bool {
        match self.0.get_mut(&key) {
            Some(v) => v.join(&value),
            None => {
                self.0.insert(key, value);
                true
            }
        }
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        self.0.get(key)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> std::collections::btree_map::Iter<'_, K, V> {
        self.0.iter()
    }
}

impl<K: Ord + Clone, V: Join> Join for JoinMap<K, V> {
    fn join(&mut self, other: &Self) -> bool {
        let mut grew = false;
        for (k, v) in &other.0 {
            grew |= self.insert(k.clone(), v.clone());
        }
        grew
    }

    fn weight(&self) -> usize {
        self.0.values().map(Join::weight).sum::<usize>().max(1)
    }
}

/// A dense set of small ids (interned classes, block or value indexes).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct BitSet(Vec<u64>);

impl BitSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` when `i` was not in the set.
    pub fn insert(&mut self, i: usize) -> bool {
        let (word, bit) = (i / 64, 1u64 << (i % 64));
        if self.0.len() <= word {
            self.0.resize(word + 1, 0);
        }
        let grew = self.0[word] & bit == 0;
        self.0[word] |= bit;
        grew
    }

    pub fn contains(&self, i: usize) -> bool {
        self.0
            .get(i / 64)
            .is_some_and(|w| w & (1u64 << (i % 64)) != 0)
    }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|w| *w == 0)
    }

    /// The members in increasing order.
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.0.iter().enumerate().flat_map(|(word, &bits)| {
            (0..64)
                .filter(move |bit| bits & (1u64 << bit) != 0)
                .map(move |bit| word * 64 + bit)
        })
    }
}

impl Join for BitSet {
    fn join(&mut self, other: &Self) -> bool {
        if self.0.len() < other.0.len() {
            self.0.resize(other.0.len(), 0);
        }
        let mut grew = false;
        for (mine, theirs) in self.0.iter_mut().zip(&other.0) {
            grew |= theirs & !*mine != 0;
            *mine |= theirs;
        }
        grew
    }

    fn weight(&self) -> usize {
        self.0.len().max(1)
    }
}

/// A fact that can fail, failure absorbing every other fact: the origins of a value, which are
/// not a display once one of them is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fallible<T>(pub Option<T>);

impl<T: Default> Default for Fallible<T> {
    fn default() -> Self {
        Self(Some(T::default()))
    }
}

impl<T: Join> Fallible<T> {
    pub fn failed() -> Self {
        Self(None)
    }
}

impl<T: Join> Join for Fallible<T> {
    fn join(&mut self, other: &Self) -> bool {
        match (&mut self.0, &other.0) {
            (None, _) => false,
            (Some(_), None) => {
                self.0 = None;
                true
            }
            (Some(mine), Some(theirs)) => mine.join(theirs),
        }
    }

    fn weight(&self) -> usize {
        self.0.as_ref().map_or(1, Join::weight)
    }
}
