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

use heki_diagnostics::Certainty;
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

pub const EVERY_RETURN: Premise = Premise {
    name: "Every return",
    states: "each function on the chain has made the next call on every path on which it returns normally, exception edges into handlers included",
    decided_in: &[
        "heki_ir::facts::paths::Paths::on_every_return",
        "heki_effect::paths::with_paths",
        "heki_effect::callgraph::site_must_chain",
    ],
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

pub const WAITS: Premise = Premise {
    name: "Waits",
    states: "the last call waits for the network, a database, a child process, or a sleep on every call, as the ecosystem catalog describes it (R3), for every argument heki cannot evaluate (R6)",
    decided_in: &[
        "heki_kb::EcosystemEntry::case_for",
        "heki_effect::resolver::arg_fact",
        "heki_effect::summary::Extractor::unconditional",
    ],
};

pub const SENDS: Premise = Premise {
    name: "Sends",
    states: "the last call sends a database statement on every call, as the ecosystem catalog describes it (R3)",
    decided_in: &[
        "heki_kb::EcosystemEntry::emits_in",
        "heki_effect::summary::Extractor::unconditional",
    ],
};

pub const EVERY_ITERATION: Premise = Premise {
    name: "Every iteration",
    states: "the call runs on every iteration of the loop that completes, exception edges into handlers included",
    decided_in: &["heki_ir::facts::paths::Paths::on_every_iteration"],
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
}
