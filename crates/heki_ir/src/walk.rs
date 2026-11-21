//! Bounded walks over graphs of IR ids, and the answers of the questions analyses decide.
//!
//! [`Answer`] is heki's one type for an answer that is established or unknown: a walk that could
//! not decide its question, a capacity the code does not fix, a premise nothing establishes.
//! What an unknown answer means belongs to the consumer, not to the question: a missing trip
//! count means "no rewrite" to an `OPT` rule and "unbounded loop" to `EFF102`. So every use states
//! it, with [`Answer::open_means`] or a `match`, and docs/scope.md ("When heki cannot tell") holds
//! the choices: a rewrite takes an open answer as "no rewrite", an exact report as "no finding",
//! and a heuristic report states its own choice where it reads the answer.

use serde::{Deserialize, Serialize};

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

#[cfg(test)]
mod tests;
