//! The failures of an import that the handlers around it take ([`ImportFailure`],
//! [`super::ImportSite::handled`]): the `except` clauses whose classes may match the failure,
//! of which one whose body may end without raising takes it, and the handlers its exception
//! reaches when no clause takes it or a clause raises again.

use heki_ir::walk::{Budget, FnShape, Visit, reach};
use heki_ir::{BlockId, InstKind, Terminator};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

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
/// an exception a clause raised in its place (`raise Error(...) from e`), whose class heki does
/// not tell.
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

/// How a handler's body ends ([`ImportHandlers::body_end`]).
#[derive(Debug, Default)]
struct BodyEnd {
    /// A path through it reaches the code after it, leaves a loop, or returns.
    completes: bool,
    /// Where the exceptions it raises on its other paths go, each with whether it is the one it
    /// took (`raise`).
    raises: Vec<(BlockId, bool)>,
}

/// What the handlers of one function take of the failures of its imports, each handler walked
/// once.
pub(crate) struct ImportHandlers<'s, 'f> {
    shape: &'s FnShape<'f>,
    /// The builtins the module may rebind.
    shadowed: &'s BTreeSet<String>,
    by_handler: RefCell<FxHashMap<BlockId, Option<ImportFailure>>>,
    bodies: RefCell<FxHashMap<BlockId, Rc<BodyEnd>>>,
}

impl<'s, 'f> ImportHandlers<'s, 'f> {
    pub(crate) fn new(shape: &'s FnShape<'f>, shadowed: &'s BTreeSet<String>) -> Self {
        Self {
            shape,
            shadowed,
            by_handler: RefCell::default(),
            bodies: RefCell::default(),
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
    /// may match it takes it when its body may end without raising ([`Self::body_end`]), and
    /// one that certainly matches hides the later clauses; a `finally` body takes it when it may
    /// leave without raising it again. What a clause or a `finally` body raises goes to the
    /// handler around it.
    fn reached(&self, b: BlockId, raised: Raised) -> Reached {
        let f = self.shape.f;
        let block = f.block(b);
        if let Some(InstKind::ExitWith { .. }) = block.insts.first().map(|i| &i.kind) {
            return Reached::Taken;
        }
        let clause = match block.terminator {
            Terminator::Branch {
                then_bb, else_bb, ..
            } => match f.block(then_bb).insts.first().map(|i| &i.kind) {
                Some(InstKind::CatchException { exc_types, .. }) => {
                    Some((then_bb, else_bb, matches(exc_types, raised, self.shadowed)))
                }
                _ => None,
            },
            _ => None,
        };
        let mut to = Vec::new();
        let (body, matched) = match clause {
            Some((then_bb, else_bb, matched)) => {
                if matched != Match::Always {
                    to.push((else_bb, raised));
                }
                (then_bb, matched)
            }
            // A `finally` body, the end of the clauses (which raises the exception again),
            // or the rest of an `except*` group.
            None => (b, Match::Always),
        };
        if matched != Match::Never {
            let end = self.body_end(body);
            if end.completes {
                return Reached::Taken;
            }
            to.extend(
                (end.raises.iter())
                    .map(|&(at, same)| (at, if same { raised } else { Raised::Other })),
            );
        }
        Reached::PassedOn(to)
    }

    /// How the handler body starting in block `start` ends: its blocks are the ones `start`
    /// dominates, and a path leaving them by a normal edge (to the code after the `try`, or out
    /// of a loop) or returning completes it. A `raise` whose exception target is outside them
    /// sends its exception to that handler; one inside them goes on in a `try` of the body.
    /// Unmetered: one step per block of the body, each body walked once.
    fn body_end(&self, start: BlockId) -> Rc<BodyEnd> {
        if let Some(end) = self.bodies.borrow().get(&start) {
            return Rc::clone(end);
        }
        let (f, dominators) = (self.shape.f, self.shape.dominators());
        let inside = |b: BlockId| dominators.dominates(start, b);
        let mut end = BodyEnd::default();
        let next = |b: BlockId, out: &mut Vec<BlockId>| {
            let block = f.block(b);
            out.extend(block.terminator.successors());
            out.extend(block.exc_target.filter(|&h| inside(h)));
        };
        let completes = reach(
            &Budget::unmetered("import_handler_bodies"),
            [start],
            next,
            |b| {
                if !inside(b) {
                    return Visit::Stop(());
                }
                let block = f.block(b);
                match block.terminator {
                    Terminator::Return(_) => return Visit::Stop(()),
                    Terminator::Raise(raised) => {
                        if let Some(to) = block.exc_target.filter(|&h| !inside(h)) {
                            end.raises.push((to, raised.is_none()));
                        }
                    }
                    _ => {}
                }
                Visit::Expand
            },
        )
        .open_means(None)
        .is_some();
        end.completes = completes;
        let end = Rc::new(end);
        self.bodies.borrow_mut().insert(start, Rc::clone(&end));
        end
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
