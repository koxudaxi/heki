//! The failures of an import that the handlers around it take ([`ImportFailure`],
//! [`super::ImportSite::handled`]): the `except` clauses whose classes may match the failure,
//! of which one whose body may end without raising takes it, and the handlers its exception
//! reaches when no clause takes it or a clause raises again.

use heki_ir::walk::{Budget, FnShape, Visit, reach};
use heki_ir::{BlockId, InstKind, Terminator};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::cell::{OnceCell, RefCell};
use std::collections::BTreeSet;

/// A failure of an import, ordered by what a handler taking it takes: one that takes any import
/// error takes a missing module too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImportFailure {
    /// The module is not there (`ModuleNotFoundError`): not installed, or a module a project
    /// package does not have.
    MissingModule,
    /// Any `ImportError`, also a plain one: a name a module does not bind, as one still loading
    /// in an import cycle has not yet.
    AnyImportError,
}

impl ImportFailure {
    /// The builtin classes an `except` clause names to take the failure: its own class and its
    /// bases.
    fn classes(self) -> &'static [&'static str] {
        match self {
            Self::MissingModule => &[
                "ModuleNotFoundError",
                "ImportError",
                "Exception",
                "BaseException",
            ],
            Self::AnyImportError => &["ImportError", "Exception", "BaseException"],
        }
    }
}

/// What reaches a handler: the import's failure, also raised again by a clause (`raise`), or
/// an exception whose class heki does not tell: one a clause raised in its place (`raise
/// Error(...) from e`), or one a handler nested in the clause raised again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Raised {
    Failure(ImportFailure),
    Other,
}

/// How an `except` clause matches what reaches it, from never to always.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Match {
    Never,
    May,
    Always,
}

/// What an exception does at a handler block or clause test it reaches
/// ([`ImportHandlers::reached`]).
enum Reached {
    /// A handler takes it: the code after the handler runs on.
    Taken,
    /// It goes on to these handler blocks and clause tests.
    PassedOn(Vec<(BlockId, Raised)>),
}

/// What the handlers of one function take of the failures of its imports, each handler walked
/// once.
pub(crate) struct ImportHandlers<'s, 'f> {
    shape: &'s FnShape<'f>,
    /// The builtins the module may rebind.
    shadowed: &'s BTreeSet<String>,
    by_handler: RefCell<FxHashMap<BlockId, Option<ImportFailure>>>,
    /// Built for the functions whose imports a handler may take only.
    body_ends: OnceCell<BodyEnds>,
}

impl<'s, 'f> ImportHandlers<'s, 'f> {
    pub(crate) fn new(shape: &'s FnShape<'f>, shadowed: &'s BTreeSet<String>) -> Self {
        Self {
            shape,
            shadowed,
            by_handler: RefCell::default(),
            body_ends: OnceCell::new(),
        }
    }

    /// The widest failure of an import in block `bb` that a handler takes, so that the code
    /// after the handler runs on; `None` when every failure leaves the function. A handler that
    /// passes a missing module on but takes a plain `ImportError` (`except ModuleNotFoundError:
    /// raise` before `except ImportError:`) counts as taking any import error.
    pub(crate) fn taken_at(&self, bb: BlockId) -> Option<ImportFailure> {
        let handler = self.shape.f.block(bb).exc_target?;
        if let Some(&taken) = self.by_handler.borrow().get(&handler) {
            return taken;
        }
        let taken = [ImportFailure::AnyImportError, ImportFailure::MissingModule]
            .into_iter()
            .find(|&failure| self.takes(handler, failure));
        self.by_handler.borrow_mut().insert(handler, taken);
        taken
    }

    /// Whether `failure`, raised into `handler`, is taken there or by a handler it reaches
    /// ([`Self::reached`]). Unmetered: each handler block and clause test is a step once per
    /// [`Raised`].
    fn takes(&self, handler: BlockId, failure: ImportFailure) -> bool {
        reach(
            &Budget::unmetered("import_handlers"),
            [(handler, Raised::Failure(failure))],
            |(b, raised), out| {
                if let Reached::PassedOn(to) = self.reached(b, raised) {
                    out.extend(to);
                }
            },
            |(b, raised)| match self.reached(b, raised) {
                Reached::Taken => Visit::Stop(()),
                Reached::PassedOn(_) => Visit::Expand,
            },
        )
        .open_means(None)
        .is_some()
    }

    /// What `raised` does at block `b`, a handler block or a clause test: the exit of a `with`
    /// block takes it (its context manager may suppress it); an `except` clause whose classes
    /// may match it takes it when its body may end without raising ([`BodyEnds`]), and one
    /// that certainly matches hides the later clauses; a `finally` body takes it when it may
    /// leave without raising it again. What a clause or a `finally` body raises goes to the
    /// handler around it.
    fn reached(&self, b: BlockId, raised: Raised) -> Reached {
        let f = self.shape.f;
        let block = f.block(b);
        if let Some(InstKind::ExitWith { .. }) = block.insts.first().map(|i| &i.kind) {
            return Reached::Taken;
        }
        let mut to = Vec::new();
        let (body, matched) = if let Terminator::Branch {
            then_bb, else_bb, ..
        } = block.terminator
            && let Some(InstKind::CatchException { exc_types, .. }) =
                f.block(then_bb).insts.first().map(|i| &i.kind)
        {
            let matched = matches(exc_types, raised, self.shadowed);
            if matched != Match::Always {
                to.push((else_bb, raised));
            }
            (then_bb, matched)
        } else {
            // A `finally` body, the end of the clauses (which raises the exception again), or
            // the rest of an `except*` group.
            (b, Match::Always)
        };
        if matched != Match::Never {
            let ends = (self.body_ends).get_or_init(|| BodyEnds::of(self.shape));
            if ends.completes[body.0 as usize] {
                return Reached::Taken;
            }
            to.extend(
                (ends.raises[body.0 as usize].iter())
                    .map(|r| (r.to, if r.same { raised } else { Raised::Other })),
            );
        }
        Reached::PassedOn(to)
    }
}

/// A `raise` that leaves the bodies of the blocks dominating it ([`BodyEnds`]) from one of them
/// on.
#[derive(Debug, Clone, Copy)]
struct RaiseOut {
    /// The handler it raises into.
    to: BlockId,
    /// Whether it raises again the exception the body took: a bare `raise` under no block of
    /// the body but its first that an exception enters. Under such a block it raises what
    /// entered that block (a nested handler's exception), which heki does not tell.
    same: bool,
    /// The first block, in the preorder of the dominator tree, whose body it leaves: the one
    /// after the nearest block dominating both it and its handler.
    leaves_from: u32,
}

/// How the body of each block of a function ends, the body being the blocks the block
/// dominates: it completes when a path through it reaches a block outside it by a normal edge
/// (the code after the `try`, or out of a loop) or returns, and otherwise it ends in the
/// `raise`s whose handlers are outside it; a `raise` into a handler inside it goes on in a `try`
/// of the body. An edge from block `b` to block `s` leaves the bodies of the blocks dominating
/// `b` but not `s`, those after the nearest block dominating both in the preorder of the
/// dominator tree. So one sweep up the tree from its leaves finds every body's end from its own
/// block's edges and its children's bodies, and no body is walked: the bodies of a run of
/// clauses each leading on to the rest of the function hold each other.
struct BodyEnds {
    /// Per block, whether its body completes; a block the entry does not reach dominates
    /// nothing, and its body completes.
    completes: Vec<bool>,
    /// Per block, the `raise`s leaving its body: one per handler and [`RaiseOut::same`].
    raises: Vec<Vec<RaiseOut>>,
}

impl BodyEnds {
    fn of(shape: &FnShape<'_>) -> Self {
        let (f, dominators) = (shape.f, shape.dominators());
        let number = |b: BlockId| {
            dominators
                .number(b)
                .expect("the entry reaches the blocks of the reverse postorder")
        };
        // The nearest block dominating both ends of an edge is its target or the target's
        // immediate dominator, found in one step up from the target.
        let leaves_from = |b: BlockId, to: BlockId| number(dominators.common(to, b)) + 1;
        let entered: FxHashSet<BlockId> = f.blocks.iter().filter_map(|b| b.exc_target).collect();
        let blocks = f.blocks.len();
        // Per block, the first block whose body an edge of the block's body leaves; a `return`
        // leaves every body.
        let mut leaving = vec![u32::MAX; blocks];
        let mut out = Self {
            completes: vec![true; blocks],
            raises: vec![Vec::new(); blocks],
        };
        // The reverse postorder lists a block after the blocks dominating it, so backwards each
        // block comes after the blocks of its body.
        for &b in shape.rpo().iter().rev() {
            let (i, block) = (b.0 as usize, f.block(b));
            for s in block.terminator.successors() {
                leaving[i] = leaving[i].min(leaves_from(b, s));
            }
            match block.terminator {
                Terminator::Return(_) => leaving[i] = 0,
                Terminator::Raise(raised) => {
                    if let Some(to) = block.exc_target {
                        let raise = RaiseOut {
                            to,
                            same: raised.is_none(),
                            leaves_from: leaves_from(b, to),
                        };
                        keep(&mut out.raises[i], raise);
                    }
                }
                _ => {}
            }
            let at = number(b);
            out.completes[i] = leaving[i] <= at;
            out.raises[i].retain(|r| r.leaves_from <= at);
            let Some(parent) = dominators.strict_dominator(b) else {
                continue;
            };
            let p = parent.0 as usize;
            leaving[p] = leaving[p].min(leaving[i]);
            let passed: Vec<RaiseOut> = (out.raises[i].iter())
                .map(|&r| RaiseOut {
                    same: r.same && !entered.contains(&b),
                    ..r
                })
                .collect();
            for raise in passed {
                keep(&mut out.raises[p], raise);
            }
        }
        out
    }
}

/// Adds `raise` to `raises`, once per handler and [`RaiseOut::same`]: of two, the one leaving
/// the larger bodies.
fn keep(raises: &mut Vec<RaiseOut>, raise: RaiseOut) {
    match (raises.iter_mut()).find(|r| (r.to, r.same) == (raise.to, raise.same)) {
        Some(kept) => kept.leaves_from = kept.leaves_from.min(raise.leaves_from),
        None => raises.push(raise),
    }
}

/// How an `except` clause whose types are the source texts `types` (none for a bare
/// `except`) matches `raised`. `shadowed` are the builtins the module may rebind.
fn matches(types: &[String], raised: Raised, shadowed: &BTreeSet<String>) -> Match {
    let Some(text) = types.first() else {
        return Match::Always;
    };
    let failure = match raised {
        Raised::Failure(failure) => Some(failure),
        Raised::Other => None,
    };
    let classes = (text
        .trim()
        .strip_prefix('(')
        .and_then(|t| t.strip_suffix(')')))
    .unwrap_or(text);
    (classes.split(',').map(str::trim).filter(|t| !t.is_empty()))
        .map(|class| class_matches(class, failure, shadowed))
        .max()
        .unwrap_or(Match::May)
}

/// How the class named `class` in an `except` clause matches `failure`, or an exception of
/// a class heki does not tell (`None`). A builtin exception class the module does not rebind
/// matches when it is the failure's class or a base of it, and never otherwise (a subclass
/// does not take its base: `ModuleNotFoundError` passes a plain `ImportError` on). Any
/// other name may match.
fn class_matches(
    class: &str,
    failure: Option<ImportFailure>,
    shadowed: &BTreeSet<String>,
) -> Match {
    let (name, qualified) = match class.strip_prefix("builtins.") {
        Some(name) => (name, true),
        None => (class, false),
    };
    if !heki_ir::is_builtin_name(name) || (!qualified && shadowed.contains(name)) {
        return Match::May;
    }
    let Some(failure) = failure else {
        return if name == "BaseException" {
            Match::Always
        } else {
            Match::May
        };
    };
    if failure.classes().contains(&name) {
        return Match::Always;
    }
    let exception_class = ["Error", "Exit", "Interrupt", "Iteration", "Warning"]
        .iter()
        .any(|end| name.ends_with(end));
    if exception_class {
        Match::Never
    } else {
        Match::May
    }
}

#[cfg(test)]
mod tests {
    use super::{ImportFailure, Match, Raised, matches};
    use std::collections::BTreeSet;

    #[test]
    fn except_clauses_matching_import_failures() {
        let shadowed: BTreeSet<String> = ["KeyError".to_owned()].into();
        use ImportFailure::{AnyImportError as Any, MissingModule as Missing};
        use Match::{Always, May, Never};
        for (types, failure, matched) in [
            ("ImportError", Any, Always),
            ("ImportError", Missing, Always),
            ("ModuleNotFoundError", Any, Never),
            ("ModuleNotFoundError", Missing, Always),
            ("builtins.ModuleNotFoundError", Missing, Always),
            ("(KeyError, ModuleNotFoundError)", Any, May),
            ("(KeyError, ModuleNotFoundError)", Missing, Always),
            ("(ModuleNotFoundError, OSError)", Any, Never),
            (
                "(\n    OSError,\n    ModuleNotFoundError,\n)",
                Missing,
                Always,
            ),
            ("(ValueError, builtins.KeyError)", Any, Never),
            ("(\n    OSError,\n    StopIteration,\n)", Missing, Never),
            ("SystemExit", Any, Never),
            ("Exception", Missing, Always),
            ("BaseException", Any, Always),
            ("(OSError, ImportError)", Missing, Always),
            ("ExceptionGroup", Any, May),
            ("MyError", Missing, May),
            ("errors.TimeoutError", Any, May),
            ("int", Missing, May),
        ] {
            assert_eq!(
                matches(&[types.to_owned()], Raised::Failure(failure), &shadowed),
                matched,
                "{types} {failure:?}"
            );
        }
        for (types, matched) in [
            (&[][..], Always),
            (&["OSError".to_owned()][..], May),
            (&["BaseException".to_owned()][..], Always),
        ] {
            assert_eq!(
                matches(types, Raised::Other, &shadowed),
                matched,
                "{types:?}"
            );
        }
    }
}
