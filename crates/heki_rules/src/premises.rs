//! The premises of the exact claims (docs/precision.md): one row per premise, saying what it
//! states and the one place heki decides it, and the premises each exact rule's claim rests on.
//!
//! - Each rule page lists its claim's premises under "What a report states" by these names;
//!   `scripts/build_docs.py` checks the two lists agree.
//! - `heki rule --output-format json` lists them with each rule.
//! - The run-time oracle (`tests/oracle`) enumerates them: it generates programs that keep every
//!   premise of a claim but one.
//! - A rule's exact or heuristic tier comes from [`tier`], which takes the rule's premises from
//!   this table.
//! - A heuristic report of a rule with a row can record which premises it left open and why
//!   (`heki_diagnostics::Diagnostic::open_premises`), each with an [`OpenCause`] from the closed
//!   list kept next to its premise below ([`CAUSES`]). The causes come from the steps of the
//!   decision that sets the report's tier, so they cannot disagree with it. A run records them
//!   only when it asks (`CheckExtras::open_premises`); no output format writes them yet. The
//!   list grows with what the homes tell apart: a cause stands for the cases its facts cannot
//!   separate yet, and says so, rather than guessing one of them. The rules recording causes so
//!   far are `EFF101` (with its heuristic side `EFF111`) and `EFF103` (with `EFF113`).

use heki_diagnostics::{Certainty, OpenPremise, RelatedLocation};
use serde::Serialize;

/// One premise of an exact claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Premise {
    /// The name the rule pages give it, unique in the table.
    pub name: &'static str,
    /// What it states, as the rule pages state it.
    pub states: &'static str,
    /// Where heki decides it (`crate::module::item`): the one home every rule reading the
    /// premise goes through. Several entries are steps of one decision, in order.
    pub decided_in: &'static [&'static str],
}

/// Why a premise of a report is open: one of the closed list kept next to the premise
/// ([`CAUSES`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenCause {
    /// Unique within its premise, kebab-case.
    pub id: &'static str,
    pub premise: &'static Premise,
    /// What it states, as the rule pages and the reports state it.
    pub states: &'static str,
    pub closer: Closer,
}

/// What closes an open cause, for every case the cause stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Closer {
    /// An annotation the user writes; heki trusts it under R2, so it must be true.
    Annotation,
    /// A change of the code's shape.
    Code,
    /// A fact only heki's shipped catalog can hold (no `[[tool.heki.ecosystem]]` key sets it).
    HekiCatalog,
    /// Nothing the user can apply to every case: heki cannot establish the premise today, the
    /// code states the opposite, or the cause stands for cases with different closers that the
    /// facts do not tell apart yet.
    None,
}

impl OpenCause {
    /// The record of this cause on a report: where the premise is open, and what the statement
    /// leaves out.
    pub fn open_at(
        &'static self,
        at: Option<RelatedLocation>,
        detail: Option<String>,
    ) -> OpenPremise {
        OpenPremise {
            premise: self.premise.name,
            cause: self.id,
            at,
            detail,
        }
    }
}

pub const EVERY_RETURN: Premise = Premise {
    name: "Every return",
    states: "each function on the chain has made the next call on every path on which it returns normally, exception edges into handlers included",
    decided_in: &[
        "heki_ir::facts::paths::Paths::on_every_return",
        "heki_effect::paths::with_paths",
        "heki_effect::callgraph::site_must_chain",
    ],
};

pub const NOT_MADE_ON_EVERY_RETURN: OpenCause = OpenCause {
    id: "not-made-on-every-return",
    premise: &EVERY_RETURN,
    states: "a function on the chain returns normally on a path on which it has not made the next call: a branch, an early return, a loop that may run no time, a handler that returns or goes on, or a `with` whose manager may suppress the exception",
    closer: Closer::Code,
};

pub const NEVER_REACHED: OpenCause = OpenCause {
    id: "never-reached",
    premise: &EVERY_RETURN,
    states: "no path reaches the reported call: it is behind a branch the code decides against, and heki claims nothing about a call that never runs",
    closer: Closer::None,
};

pub const NEVER_RETURNS: OpenCause = OpenCause {
    id: "never-returns",
    premise: &EVERY_RETURN,
    states: "the call never returns normally: every target it can run is abstract or always raises, and heki claims nothing about a return that never happens",
    closer: Closer::None,
};

pub const EVERY_IMPLEMENTATION: Premise = Premise {
    name: "Every implementation",
    states: "each call on the chain has as its targets every implementation it can run (R2, R5), each with the fact",
    decided_in: &[
        "heki_effect::summary::Extractor::targets_complete",
        "heki_effect::resolver::TypeCtx::method_targets_complete",
        "heki_effect::callgraph::site_must_chain",
    ],
};

pub const INCOMPLETE_TARGETS: OpenCause = OpenCause {
    id: "incomplete-targets",
    premise: &EVERY_IMPLEMENTATION,
    states: "the call's targets may not be every implementation it can run: a receiver typed by a protocol (R5), a base class heki knows nothing of that may define the method, more than 64 classes to dispatch over, or a member of a union that does not resolve",
    closer: Closer::None,
};

pub const IMPLEMENTATION_WITHOUT_THE_FACT: OpenCause = OpenCause {
    id: "implementation-without-the-fact",
    premise: &EVERY_IMPLEMENTATION,
    states: "an implementation the call can run has not been established to make the call whenever it returns",
    closer: Closer::Code,
};

pub const NOT_RUN_AT_THE_CALL: OpenCause = OpenCause {
    id: "not-run-at-the-call",
    premise: &EVERY_IMPLEMENTATION,
    states: "the call does not run a synchronous body there and then: a target is an `async def` or a generator, an API runs the callable, or the call iterates or hands on what a call returns",
    closer: Closer::None,
};

pub const DECORATED: OpenCause = OpenCause {
    id: "decorated",
    premise: &EVERY_IMPLEMENTATION,
    states: "a decorator of a target may not run its body on every call (only `staticmethod`, `classmethod`, `property`, `abstractmethod`, and `override` are known to)",
    closer: Closer::None,
};

pub const DEFINED_TWICE: OpenCause = OpenCause {
    id: "defined-twice",
    premise: &EVERY_IMPLEMENTATION,
    states: "the name of a target is bound to more than one definition",
    closer: Closer::Code,
};

pub const WAITS: Premise = Premise {
    name: "Waits",
    states: "the last call waits for the network, a database, a child process, or a sleep on every call, as the ecosystem catalog describes it (R3), for every argument heki cannot evaluate (R6)",
    decided_in: &[
        "heki_kb::EcosystemEntry::case_for",
        "heki_effect::resolver::arg_fact",
        "heki_effect::summary::Extractor::unconditional",
    ],
};

pub const WAITS_ON_ANOTHER_THREAD: OpenCause = OpenCause {
    id: "waits-on-another-thread",
    premise: &WAITS,
    states: "the last call waits for another thread (a lock, an event, a queue, a thread, a future), which may have done its part already",
    closer: Closer::None,
};

pub const CONDITIONAL_WAIT: OpenCause = OpenCause {
    id: "conditional-wait",
    premise: &WAITS,
    states: "the catalog does not establish that the last call waits on every call: an argument that may come from the caller can select a case that returns at once, or it waits only in some state (a SQLite connection that may be in memory, a database call that may be answered without the database)",
    closer: Closer::None,
};

pub const SENDS: Premise = Premise {
    name: "Sends",
    states: "the last call sends a database statement on every call, as the ecosystem catalog describes it (R3)",
    decided_in: &[
        "heki_kb::EcosystemEntry::emits_in",
        "heki_effect::summary::Extractor::unconditional",
    ],
};

pub const CONDITIONAL_STATEMENT: OpenCause = OpenCause {
    id: "conditional-statement",
    premise: &SENDS,
    states: "the catalog does not establish that the last call sends a statement on every call: it sends one only in some session state, or an argument that may come from the caller can select a case that does not",
    closer: Closer::None,
};

pub const EVERY_ITERATION: Premise = Premise {
    name: "Every iteration",
    states: "the call runs on every iteration of the loop that completes, exception edges into handlers included",
    decided_in: &["heki_ir::facts::paths::Paths::on_every_iteration"],
};

pub const NOT_EVERY_ITERATION: OpenCause = OpenCause {
    id: "not-every-iteration",
    premise: &EVERY_ITERATION,
    states: "the call is not established to run on every iteration of the loop that completes: a `continue`, a branch, or a handler that goes on skips it, or no iteration completes",
    closer: Closer::Code,
};

pub const REPEATED_BY_A_BUILTIN: OpenCause = OpenCause {
    id: "repeated-by-a-builtin",
    premise: &EVERY_ITERATION,
    states: "a builtin repeats the call once per item it is handed (`map(f, xs)`, `sorted(xs, key=f)`), and heki establishes iterations only of the function's own loops",
    closer: Closer::None,
};

pub const ORIGIN: Premise = Premise {
    name: "Origin",
    states: "the loop steps through items the data decides (a parameter, the rows of a query, the lines of a file, the entries of a directory), and the code does not fix their number",
    decided_in: &[
        "heki_ir::facts::origin::ItemOrigins::of_loop",
        "heki_rules::eff_rules::data_origin",
        "heki_ir::facts::FnIndex::fixed_trips",
    ],
};

pub const FIXED_BY_THE_CODE: OpenCause = OpenCause {
    id: "fixed-by-the-code",
    premise: &ORIGIN,
    states: "the code fixes the loop's items or bounds their number: a literal, a display, a module constant, `range` of constants, a slice, a length test, or a counter",
    closer: Closer::None,
};

pub const ITEMS_NOT_FROM_THE_DATA: OpenCause = OpenCause {
    id: "items-not-from-the-data",
    premise: &ORIGIN,
    states: "the loop's items come from something heki does not take as the data: a method's receiver, a project function's result, or a call heki does not resolve",
    closer: Closer::None,
};

pub const LIBRARY_CALL_WITHOUT_ITEMS: OpenCause = OpenCause {
    id: "library-call-without-items",
    premise: &ORIGIN,
    states: "the loop's items are a library call's result whose catalog entry does not say that the data decides their number (`items`)",
    closer: Closer::HekiCatalog,
};

pub const NO_ITEMS_FOLLOWED: OpenCause = OpenCause {
    id: "no-items-followed",
    premise: &ORIGIN,
    states: "heki follows no items the loop steps through: a `while` loop, or an iterable the walk does not decide",
    closer: Closer::None,
};

pub const LOOP_CARRIED_ON_EVERY_PATH: Premise = Premise {
    name: "Loop-carried on every path",
    states: "every path around the back edge carries the new value, computed from the previous one",
    decided_in: &[
        "heki_rules::data_rules::carries_only",
        "heki_rules::data_rules::holds_previous",
    ],
};

pub const REALLOCATES: Premise = Premise {
    name: "Reallocates",
    states: "the call copies all of its inputs into a new value on every call, as the ecosystem catalog describes it",
    decided_in: &["heki_effect::ProjectIndex::site_must"],
};

pub const READ_ONLY_PARAMETER: Premise = Premise {
    name: "Read-only parameter",
    states: "the parameter is annotated `Sequence`, `Mapping`, or `ReadOnly`",
    decided_in: &["heki_effect::resolver::param_access"],
};

pub const PASSED_AS_ITSELF: Premise = Premise {
    name: "Passed as itself",
    states: "on every path to the call, the argument is the parameter or a local alias of it",
    decided_in: &["heki_effect::summary::merge_phi"],
};

pub const CHANGED_AT_THE_LINE: Premise = Premise {
    name: "Changed at the line",
    states: "the named line changes in place the object bound to the parameter of the function it is in, a builtin container whose changing methods no project subclass replaces",
    decided_in: &[
        "heki_effect::summary::Extractor::container_mut",
        "heki_effect::resolver::ProjectDecls::method_not_overridden",
    ],
};

pub const CACHED: Premise = Premise {
    name: "Cached",
    states: "the decorator is `functools.cache`, or `functools.lru_cache` that keeps results",
    decided_in: &["heki_effect::readonly::cache_decorator"],
};

pub const KEYED: Premise = Premise {
    name: "Keyed",
    states: "the cached function takes a parameter besides `self` / `cls`",
    decided_in: &["heki_effect::readonly::cached_result"],
};

pub const CONTAINER_KIND: Premise = Premise {
    name: "Container kind",
    states: "every return a run can reach hands out a builtin container of the named kind, by its return annotation (R2) or by construction",
    decided_in: &[
        "heki_effect::readonly::annotated_kind",
        "heki_effect::readonly::constructed_kind",
    ],
};

pub const SAME_OBJECT: Premise = Premise {
    name: "Same object",
    states: "the changed object is the cached call's result on every path, with no copy",
    decided_in: &["heki_rules::mut_rules::Mut202Fn::must_results"],
};

pub const CHANGED_IN_PLACE: Premise = Premise {
    name: "Changed in place",
    states: "the line is an in-place change of a container of that kind, which no project subclass replaces",
    decided_in: &[
        "heki_effect::readonly::ContainerKind::mutators",
        "heki_effect::resolver::ProjectDecls::method_not_overridden",
    ],
};

pub const BUILTIN_TYPE_BY_CONSTRUCTION: Premise = Premise {
    name: "Builtin type by construction",
    states: "the value is of the builtin type the rule names by its construction; annotations are never trusted",
    decided_in: &["heki_soundness::interpreter::FrameState::exact_type"],
};

pub const LOOP_CARRIED: Premise = Premise {
    name: "Loop-carried",
    states: "the result is the value the next iteration starts from",
    decided_in: &["heki_soundness::opt_rules::loop_carried_phi"],
};

pub const IN_A_LOOP: Premise = Premise {
    name: "In a loop",
    states: "the statement is in the body of a loop",
    decided_in: &["heki_soundness::opt_rules::find_sites"],
};

pub const REWRITE_PROVEN_SAFE: Premise = Premise {
    name: "Rewrite proven safe",
    states: "every proof obligation of the rewrite is established",
    decided_in: &["heki_soundness::obligations::SafetyVerdict::evaluate"],
};

pub const EQUALITY_OF_KEYS: Premise = Premise {
    name: "Equality of keys",
    states: "the comparison is `==` between keys of the two items, and each key the inner loop loads runs no code of the project",
    decided_in: &[
        "heki_rules::opt_rules::opt113",
        "heki_effect::resolver::ProjectDecls::plain_attr",
    ],
};

pub const STEPS_THROUGH_INPUTS: Premise = Premise {
    name: "Steps through inputs",
    states: "each loop steps through items of the function's inputs",
    decided_in: &["heki_rules::opt_rules::JoinPremises::steps_through_inputs"],
};

pub const IDLE_ON_A_MISMATCH: Premise = Premise {
    name: "Idle on a mismatch",
    states: "an inner item whose key differs runs only the loads of the two keys and the comparison",
    decided_in: &["heki_rules::opt_rules::JoinPremises::idle_on_mismatch"],
};

pub const SAME_COLLECTION_SAME_KEYS: Premise = Premise {
    name: "Same collection, same keys",
    states: "the inner loop steps through the same collection, with the same keys, on every iteration of the outer loop",
    decided_in: &[
        "heki_rules::opt_rules::JoinPremises::same_collection_each_time",
        "heki_rules::opt_rules::JoinPremises::keys_unchanged",
    ],
};

/// Every premise of the table, once.
pub const ALL: &[&Premise] = &[
    &EVERY_RETURN,
    &EVERY_IMPLEMENTATION,
    &WAITS,
    &SENDS,
    &EVERY_ITERATION,
    &ORIGIN,
    &LOOP_CARRIED_ON_EVERY_PATH,
    &REALLOCATES,
    &READ_ONLY_PARAMETER,
    &PASSED_AS_ITSELF,
    &CHANGED_AT_THE_LINE,
    &CACHED,
    &KEYED,
    &CONTAINER_KIND,
    &SAME_OBJECT,
    &CHANGED_IN_PLACE,
    &BUILTIN_TYPE_BY_CONSTRUCTION,
    &LOOP_CARRIED,
    &IN_A_LOOP,
    &REWRITE_PROVEN_SAFE,
    &EQUALITY_OF_KEYS,
    &STEPS_THROUGH_INPUTS,
    &IDLE_ON_A_MISMATCH,
    &SAME_COLLECTION_SAME_KEYS,
];

/// Every open cause, once, in the order of their premises in [`ALL`].
pub const CAUSES: &[&OpenCause] = &[
    &NOT_MADE_ON_EVERY_RETURN,
    &NEVER_REACHED,
    &NEVER_RETURNS,
    &INCOMPLETE_TARGETS,
    &IMPLEMENTATION_WITHOUT_THE_FACT,
    &NOT_RUN_AT_THE_CALL,
    &DECORATED,
    &DEFINED_TWICE,
    &WAITS_ON_ANOTHER_THREAD,
    &CONDITIONAL_WAIT,
    &CONDITIONAL_STATEMENT,
    &NOT_EVERY_ITERATION,
    &REPEATED_BY_A_BUILTIN,
    &FIXED_BY_THE_CODE,
    &ITEMS_NOT_FROM_THE_DATA,
    &LIBRARY_CALL_WITHOUT_ITEMS,
    &NO_ITEMS_FOLLOWED,
];

/// The cause `id` of the premise named `premise`.
pub fn cause(premise: &str, id: &str) -> Option<&'static OpenCause> {
    CAUSES
        .iter()
        .copied()
        .find(|c| c.premise.name == premise && c.id == id)
}

/// The exact rule whose claim the heuristic rule `code` reports with a premise open (ADR-018
/// decision 9: one claim, one code), whose premise row its reports' open premises belong to.
pub fn exact_side_of(code: &str) -> Option<&'static str> {
    match code {
        "EFF111" => Some("EFF101"),
        "EFF113" => Some("EFF103"),
        _ => None,
    }
}

const OPT_ACCUMULATION: &[&Premise] = &[
    &BUILTIN_TYPE_BY_CONSTRUCTION,
    &LOOP_CARRIED,
    &IN_A_LOOP,
    &REWRITE_PROVEN_SAFE,
];
const OPT_SCAN: &[&Premise] = &[
    &BUILTIN_TYPE_BY_CONSTRUCTION,
    &IN_A_LOOP,
    &REWRITE_PROVEN_SAFE,
];

/// The premises of the exact claim of the rule `code`, in the order its page lists them; empty
/// for a rule without an exact claim.
pub fn of_rule(code: &str) -> &'static [&'static Premise] {
    match code {
        "EFF101" => &[&EVERY_RETURN, &EVERY_IMPLEMENTATION, &WAITS],
        "EFF103" => &[
            &ORIGIN,
            &EVERY_ITERATION,
            &EVERY_RETURN,
            &EVERY_IMPLEMENTATION,
            &SENDS,
        ],
        "DATA101" => &[
            &LOOP_CARRIED_ON_EVERY_PATH,
            &EVERY_ITERATION,
            &ORIGIN,
            &REALLOCATES,
        ],
        "MUT110" => &[
            &READ_ONLY_PARAMETER,
            &PASSED_AS_ITSELF,
            &CHANGED_AT_THE_LINE,
        ],
        "MUT114" => &[
            &CACHED,
            &KEYED,
            &CONTAINER_KIND,
            &SAME_OBJECT,
            &CHANGED_IN_PLACE,
        ],
        "OPT101" | "OPT102" | "OPT103" => OPT_ACCUMULATION,
        "OPT110" | "OPT111" | "OPT120" | "OPT121" | "OPT130" => OPT_SCAN,
        "OPT113" => &[
            &EQUALITY_OF_KEYS,
            &STEPS_THROUGH_INPUTS,
            &IDLE_ON_A_MISMATCH,
            &SAME_COLLECTION_SAME_KEYS,
        ],
        _ => &[],
    }
}

/// The tier of one report of the exact rule `code`: exact when every premise of its claim
/// ([`of_rule`]) is established, heuristic otherwise. A rule's report never claims more than
/// the table lists for it.
pub fn tier(code: &str, established: bool) -> Certainty {
    debug_assert!(
        !of_rule(code).is_empty(),
        "{code} decides an exact tier but has no premises in the table"
    );
    if established {
        Certainty::Exact
    } else {
        Certainty::Heuristic
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CORE_RULES;

    #[test]
    fn every_exact_rule_has_premises_and_every_premise_a_rule() {
        for rule in CORE_RULES {
            let exact = rule.certainty == Certainty::Exact;
            assert_eq!(!of_rule(rule.code).is_empty(), exact, "{}", rule.code);
        }
        for premise in ALL {
            assert!(
                CORE_RULES
                    .iter()
                    .any(|r| of_rule(r.code).iter().any(|p| p.name == premise.name)),
                "{} belongs to no rule",
                premise.name
            );
        }
    }

    #[test]
    fn premise_names_are_unique() {
        for (i, a) in ALL.iter().enumerate() {
            for b in &ALL[i + 1..] {
                assert_ne!(a.name, b.name);
            }
        }
    }

    #[test]
    fn every_cause_belongs_to_a_premise_of_the_table_once_and_is_kebab_case() {
        for (i, c) in CAUSES.iter().enumerate() {
            assert!(
                ALL.contains(&c.premise),
                "{} belongs to no premise of the table",
                c.id
            );
            assert!(
                !c.id.is_empty()
                    && !c.id.starts_with('-')
                    && !c.id.ends_with('-')
                    && !c.id.contains("--")
                    && c.id.chars().all(|ch| ch.is_ascii_lowercase() || ch == '-'),
                "{} is not kebab-case",
                c.id
            );
            for d in &CAUSES[i + 1..] {
                assert!(
                    c.premise.name != d.premise.name || c.id != d.id,
                    "{}: {} twice",
                    c.premise.name,
                    c.id
                );
            }
            assert_eq!(cause(c.premise.name, c.id), Some(*c));
        }
    }

    #[test]
    fn the_causes_follow_the_order_of_their_premises() {
        let position = |c: &OpenCause| ALL.iter().position(|p| *p == c.premise);
        assert!(CAUSES.windows(2).all(|w| position(w[0]) <= position(w[1])));
    }

    #[test]
    fn a_heuristic_side_reports_the_claim_of_an_exact_rule() {
        for rule in CORE_RULES {
            if let Some(exact) = exact_side_of(rule.code) {
                assert_eq!(rule.certainty, Certainty::Heuristic, "{}", rule.code);
                assert!(!of_rule(exact).is_empty(), "{exact}");
            }
        }
        assert_eq!(exact_side_of("EFF111"), Some("EFF101"));
        assert_eq!(exact_side_of("EFF113"), Some("EFF103"));
    }
}
