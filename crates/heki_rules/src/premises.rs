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
//!   (`heki_diagnostics::Diagnostic::open_records`), each with an [`OpenCause`] from the closed
//!   list kept next to its premise below ([`CAUSES`]). The causes come from the steps of the
//!   decision that sets the report's tier, so they cannot disagree with it. A run records them
//!   only when it asks (`CheckExtras::open_premises`): `heki recommend` counts the reports by
//!   them (`crate::recommend::premises`); no output format of `heki check` writes them. The
//!   list grows with what the homes tell apart: a cause stands for the cases its facts cannot
//!   separate yet, and says so, rather than guessing one of them. The rules recording causes so
//!   far are `EFF101`, `EFF103`, the eight `OPT` rewrites (a cause of "Rewrite proven safe" per
//!   proof obligation, [`OBLIGATION_CAUSES`]), `OPT113`, `DATA101`, `MUT110`, and `MUT114`.
//! - The exact tier of a size-dependent `OPT` rewrite and of `OPT113` also asks for a
//!   [`Condition`] that is no premise: "Trip count open" (Safety × Profitability, ADR-007 and
//!   ADR-011: the claim holds over a loop the code counts, the change does not pay off there). A
//!   report heuristic only for it records the condition with its cause ([`ConditionCause`]) in
//!   the same list, so every heuristic report names why it is not exact, while the premise rows,
//!   the claims, and the oracle's strips stay as they are.

use heki_diagnostics::{Axis, Certainty, OpenCondition, OpenPremise, OpenRecord, RelatedLocation};
use heki_soundness::ObligationKind;
use schemars::JsonSchema;
use serde::Serialize;
use std::ops::ControlFlow;

/// One premise of an exact claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
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

/// The steps of one decision that do not hold, told to whoever asks: a decision stops at the
/// first (`ControlFlow::Break`), so it spends nothing on the steps after it, and an explanation
/// goes on through the others.
pub(crate) struct Told<'s, S> {
    steps: &'s mut dyn FnMut(S) -> ControlFlow<()>,
    /// A step did not hold.
    pub open: bool,
    /// The last step told stops the decision.
    pub stopped: bool,
}

impl<'s, S> Told<'s, S> {
    pub fn new(steps: &'s mut dyn FnMut(S) -> ControlFlow<()>) -> Self {
        Self {
            steps,
            open: false,
            stopped: false,
        }
    }

    /// Tells `step`; true when the decision stops at it.
    pub fn step(&mut self, step: S) -> bool {
        self.open = true;
        self.stopped = (self.steps)(step).is_break();
        self.stopped
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
    states: "each call on the chain has as its targets every implementation it can run (R1, R2, R5), each with the fact",
    decided_in: &[
        "heki_effect::summary::Extractor::targets_complete",
        "heki_effect::resolver::TypeCtx::inst_targets_complete",
        "heki_effect::resolver::ProjectDecls::lookup_rebound",
        "heki_effect::callgraph::site_must_chain",
    ],
};

pub const INCOMPLETE_TARGETS: OpenCause = OpenCause {
    id: "incomplete-targets",
    premise: &EVERY_IMPLEMENTATION,
    states: "the call's targets may not be every implementation it can run: a receiver typed by a protocol (R5), a base class heki knows nothing of that may define the method, a base another module binds only under `if TYPE_CHECKING:`, more than 64 classes to dispatch over, a member of a union that does not resolve, or a name the project binds to another object (R1: a store or `del` of the function, the class, or the method in its module or another, also through `setattr`, its `__code__`, or a class's `__bases__`)",
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
    states: "a decorator of a target may not run its body on every call (only `staticmethod`, `classmethod`, `property`, `abstractmethod`, `override`, and `final` of the builtins, `abc`, `typing`, and `typing_extensions` are known to)",
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
    states: "the last call waits for the network, a database, a child process, or a sleep on every call, as the ecosystem catalog describes it (R3), for every argument heki cannot evaluate (R6); a call in a helper whose case or receiver state depends on the helper's parameter, by the value each call of the helper passes there",
    decided_in: &[
        "heki_kb::EcosystemEntry::case_for",
        "heki_effect::resolver::arg_fact",
        "heki_effect::summary::Extractor::unconditional",
        "heki_effect::summary::Extractor::given_waits",
        "heki_effect::guards::given",
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
    states: "the loop's items come from something heki does not take as the data: a method's receiver, a project function's result whose returns heki does not follow to such items, or a call heki does not resolve",
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

pub const PREVIOUS_VALUE_NOT_HELD: OpenCause = OpenCause {
    id: "previous-value-not-held",
    premise: &LOOP_CARRIED_ON_EVERY_PATH,
    states: "the data the reallocating call combines is not established to hold the previous value itself on every path: a value only some paths make it, or a value computed from it",
    closer: Closer::Code,
};

pub const RESET_ON_A_PATH: OpenCause = OpenCause {
    id: "reset-on-a-path",
    premise: &LOOP_CARRIED_ON_EVERY_PATH,
    states: "a path around the back edge is not established to carry the update: it resets the variable or binds it to another value",
    closer: Closer::Code,
};

pub const REALLOCATES: Premise = Premise {
    name: "Reallocates",
    states: "the call copies all of its inputs into a new value on every call, as the ecosystem catalog describes it",
    decided_in: &["heki_effect::ProjectIndex::site_must"],
};

pub const NOT_THE_CATALOG_CALL: OpenCause = OpenCause {
    id: "not-the-catalog-call",
    premise: &REALLOCATES,
    states: "the update is not established to be a catalog call that copies all of its inputs on every call: it is a project function's result, which may return another value on some path (heki establishes the reallocation only of the catalog call itself at the update), or a catalog call whose targets or effect heki does not establish there",
    closer: Closer::None,
};

pub const READ_ONLY_PARAMETER: Premise = Premise {
    name: "Read-only parameter",
    states: "the parameter is annotated `Sequence`, `Mapping`, `AbstractSet`, or `ReadOnly`",
    decided_in: &["heki_effect::resolver::param_access"],
};

pub const PASSED_AS_ITSELF: Premise = Premise {
    name: "Passed as itself",
    states: "on every path to the call or the change, the value is the parameter, a local alias of it, or what a call hands back of it as itself (`typing.cast`, a project function returning its parameter on every path)",
    decided_in: &[
        "heki_effect::summary::merge_phi",
        "heki_effect::summary::Extractor::build_same_objects",
    ],
};

pub const CHANGED_AT_THE_LINE: Premise = Premise {
    name: "Changed at the line",
    states: "the named line changes in place the object bound to the parameter of the function it is in, a builtin container whose changing methods no project subclass replaces",
    decided_in: &[
        "heki_effect::summary::Extractor::container_mut",
        "heki_effect::resolver::ProjectDecls::method_not_overridden",
    ],
};

pub const NO_CHANGING_BODY: OpenCause = OpenCause {
    id: "no-changing-body",
    premise: &CHANGED_AT_THE_LINE,
    states: "no function the calls shown reach changes the object in its own body: the change is made where a generator that another function returns is iterated",
    closer: Closer::None,
};

pub const NOT_A_BUILTIN_CONTAINER_CHANGE: OpenCause = OpenCause {
    id: "not-a-builtin-container-change",
    premise: &CHANGED_AT_THE_LINE,
    states: "the function the calls end at changes the object, but not where heki establishes it a builtin container whose changing method no project subclass replaces: it may be a value of another class (a `Sequence` whose `append` returns a new one), or a project subclass overrides the method",
    closer: Closer::None,
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

/// The cause of "Cached", "Keyed", and "Container kind" for a cached function whose summary
/// does not establish them ([`heki_effect::CachedResult`] keeps no reason).
const CACHED_RESULT_STATES: &str = "the cached function's summary does not establish that it is a synchronous function a `functools` cache keeps the results of, that takes a parameter besides `self` / `cls`, and that hands out a builtin container of one kind on every return, or heki does not find the function; the summary keeps no reason, so heki cannot tell which of the three fails, and each is open";

pub const NOT_ESTABLISHED_CACHED: OpenCause = OpenCause {
    id: "cached-result-not-established",
    premise: &CACHED,
    states: CACHED_RESULT_STATES,
    closer: Closer::None,
};

pub const NOT_ESTABLISHED_KEYED: OpenCause = OpenCause {
    id: "cached-result-not-established",
    premise: &KEYED,
    states: CACHED_RESULT_STATES,
    closer: Closer::None,
};

pub const NOT_ESTABLISHED_CONTAINER_KIND: OpenCause = OpenCause {
    id: "cached-result-not-established",
    premise: &CONTAINER_KIND,
    states: CACHED_RESULT_STATES,
    closer: Closer::None,
};

/// The causes a cached function's summary without a [`heki_effect::CachedResult`] leaves open,
/// one for each premise that fact stands for.
pub const CACHED_RESULT_NOT_ESTABLISHED: [&OpenCause; 3] = [
    &NOT_ESTABLISHED_CACHED,
    &NOT_ESTABLISHED_KEYED,
    &NOT_ESTABLISHED_CONTAINER_KIND,
];

pub const SAME_OBJECT: Premise = Premise {
    name: "Same object",
    states: "the changed object is the cached call's result on every path, with no copy",
    decided_in: &["heki_rules::mut_rules::Mut202Fn::must_results"],
};

pub const NOT_THE_RESULT_ON_EVERY_PATH: OpenCause = OpenCause {
    id: "not-the-result-on-every-path",
    premise: &SAME_OBJECT,
    states: "the changed object is not established to be the result of a direct call of the cached function on every path: another value on some path, a value derived from it (an item, an attribute, the contents of a copy), or a call that resolves to more than one function",
    closer: Closer::Code,
};

pub const RETURNED_THROUGH_A_FUNCTION: OpenCause = OpenCause {
    id: "returned-through-a-function",
    premise: &SAME_OBJECT,
    states: "the value comes from a project function that returns the cached function's result, which heki does not establish to be that result itself, with no copy, on every path",
    closer: Closer::None,
};

pub const CHANGED_IN_PLACE: Premise = Premise {
    name: "Changed in place",
    states: "the line is an in-place change of a container of that kind, which no project subclass replaces",
    decided_in: &[
        "heki_effect::readonly::ContainerKind::mutators",
        "heki_effect::resolver::ProjectDecls::method_not_overridden",
    ],
};

pub const CHANGED_IN_A_CALLED_FUNCTION: OpenCause = OpenCause {
    id: "changed-in-a-called-function",
    premise: &CHANGED_IN_PLACE,
    states: "the line hands the value to a project function that changes it, where the claim names an in-place change at the line itself",
    closer: Closer::None,
};

pub const NOT_A_CHANGE_OF_THE_KIND: OpenCause = OpenCause {
    id: "not-a-change-of-the-kind",
    premise: &CHANGED_IN_PLACE,
    states: "the line is no in-place change heki knows for a container of the cached result's kind: an attribute store or deletion, or a method or operator that does not change that kind",
    closer: Closer::None,
};

pub const REPLACED_BY_A_SUBCLASS: OpenCause = OpenCause {
    id: "replaced-by-a-subclass",
    premise: &CHANGED_IN_PLACE,
    states: "the cached function's result is of the container kind only by its return annotation, which under R2 admits a project subclass, and one overrides the changing method or operator, may install it through a metaclass, or defines `__getattribute__`",
    closer: Closer::Code,
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

pub const FRAME_INTROSPECTION: OpenCause = OpenCause {
    id: "frame-introspection",
    premise: &REWRITE_PROVEN_SAFE,
    states: "the function introspects or rebinds its frame (`locals()`, `exec`, `sys._getframe`, ...), which can read or rebind its locals behind the analysis (assumption A1)",
    closer: Closer::Code,
};

pub const ANALYSIS_NOT_CONVERGED: OpenCause = OpenCause {
    id: "analysis-not-converged",
    premise: &REWRITE_PROVEN_SAFE,
    states: "the abstract interpretation of the function did not reach a fixpoint",
    closer: Closer::None,
};

pub const EXACT_TYPE: OpenCause = OpenCause {
    id: "exact-type",
    premise: &REWRITE_PROVEN_SAFE,
    states: "the exact builtin type a value of the statement must have is not established by its construction at the statement: it is another type, or unknown there (annotations are never trusted)",
    closer: Closer::Code,
};

pub const UNIQUE: OpenCause = OpenCause {
    id: "unique",
    premise: &REWRITE_PROVEN_SAFE,
    states: "the value may be shared: a reference to it heki does not track may exist (a parameter, an attribute, a container, a call that keeps it), which would see the change in place",
    closer: Closer::Code,
};

pub const EXCLUSIVE_ACCESS_AT_WRITE: OpenCause = OpenCause {
    id: "exclusive-access-at-write",
    premise: &REWRITE_PROVEN_SAFE,
    states: "another reference to the object, or the old binding, may be used after the write",
    closer: Closer::Code,
};

pub const EVAL_ORDER_PRESERVED: OpenCause = OpenCause {
    id: "eval-order-preserved",
    premise: &REWRITE_PROVEN_SAFE,
    states: "the rewrite may evaluate the statement's parts in another order: the right-hand side changes the value before the original reads it, or an update evaluates every entry before inserting any",
    closer: Closer::Code,
};

pub const EXCEPTION_STATE_SAFE: OpenCause = OpenCause {
    id: "exception-state-safe",
    premise: &REWRITE_PROVEN_SAFE,
    states: "an exception handler may see another state after the rewrite: a value partly updated in place, a name still bound to the old object, or an accumulator not joined yet",
    closer: Closer::Code,
};

pub const DICT_UNPACK_LEADING: OpenCause = OpenCause {
    id: "dict-unpack-leading",
    premise: &REWRITE_PROVEN_SAFE,
    states: "the accumulator is not the leading `**` entry of the display, or is unpacked again",
    closer: Closer::Code,
};

pub const SET_ORDER_UNOBSERVED: OpenCause = OpenCause {
    id: "set-order-unobserved",
    premise: &REWRITE_PROVEN_SAFE,
    states: "something may observe the iteration order of the set the rewrite grows in place, or the value may be a set",
    closer: Closer::Code,
};

pub const VALUE_LOOP_INVARIANT: OpenCause = OpenCause {
    id: "value-loop-invariant",
    premise: &REWRITE_PROVEN_SAFE,
    states: "the value the rewrite builds once before the loop may change during the loop",
    closer: Closer::Code,
};

pub const ZERO_TRIP_EXCEPTION_SAFE: OpenCause = OpenCause {
    id: "zero-trip-exception-safe",
    premise: &REWRITE_PROVEN_SAFE,
    states: "the `set(...)` the rewrite builds before the loop may raise even when the loop runs no time: an element that may be unhashable, or a name that may be unbound there",
    closer: Closer::Code,
};

pub const HASH_CONSISTENT_PROBE: OpenCause = OpenCause {
    id: "hash-consistent-probe",
    premise: &REWRITE_PROVEN_SAFE,
    states: "the value looked up may compare otherwise through `hash` than through `==`: a custom `__eq__` / `__hash__`, or an unhashable value",
    closer: Closer::Code,
};

pub const NO_INTERMEDIATE_READS: OpenCause = OpenCause {
    id: "no-intermediate-reads",
    premise: &REWRITE_PROVEN_SAFE,
    states: "the accumulator is read inside the loop besides the accumulation",
    closer: Closer::Code,
};

pub const ONLY_PAIRED_APPENDS: OpenCause = OpenCause {
    id: "only-paired-appends",
    premise: &REWRITE_PROVEN_SAFE,
    states: "the list changes otherwise than by the appends a set kept beside it would mirror",
    closer: Closer::Code,
};

pub const DEQUE_EQUIVALENT_OPS: OpenCause = OpenCause {
    id: "deque-equivalent-ops",
    premise: &REWRITE_PROVEN_SAFE,
    states: "a use of the list has no `collections.deque` equivalent, or no construction of it can become a deque",
    closer: Closer::Code,
};

pub const SIDE_EFFECT_FREE: OpenCause = OpenCause {
    id: "side-effect-free",
    premise: &REWRITE_PROVEN_SAFE,
    states: "what the rewrite evaluates before the loop may have side effects, or `set` does not name the builtin there",
    closer: Closer::Code,
};

/// The cause of "Rewrite proven safe" each proof obligation stands for when it is not
/// established, one row per obligation kind in the order of its declaration.
pub const OBLIGATION_CAUSES: &[(ObligationKind, &OpenCause)] = &[
    (ObligationKind::A1HazardFree, &FRAME_INTROSPECTION),
    (ObligationKind::AnalysisConverged, &ANALYSIS_NOT_CONVERGED),
    (ObligationKind::ExactType, &EXACT_TYPE),
    (ObligationKind::Unique, &UNIQUE),
    (
        ObligationKind::ExclusiveAccessAtWrite,
        &EXCLUSIVE_ACCESS_AT_WRITE,
    ),
    (ObligationKind::EvalOrderPreserved, &EVAL_ORDER_PRESERVED),
    (ObligationKind::ExceptionStateSafe, &EXCEPTION_STATE_SAFE),
    (ObligationKind::DictUnpackLeading, &DICT_UNPACK_LEADING),
    (ObligationKind::SetOrderUnobserved, &SET_ORDER_UNOBSERVED),
    (ObligationKind::ValueLoopInvariant, &VALUE_LOOP_INVARIANT),
    (
        ObligationKind::ZeroTripExceptionSafe,
        &ZERO_TRIP_EXCEPTION_SAFE,
    ),
    (ObligationKind::HashConsistentProbe, &HASH_CONSISTENT_PROBE),
    (ObligationKind::NoIntermediateReads, &NO_INTERMEDIATE_READS),
    (ObligationKind::OnlyPairedAppends, &ONLY_PAIRED_APPENDS),
    (ObligationKind::DequeEquivalentOps, &DEQUE_EQUIVALENT_OPS),
    (ObligationKind::SideEffectFree, &SIDE_EFFECT_FREE),
];

/// The cause of "Rewrite proven safe" an obligation of `kind` that is not established stands
/// for ([`OBLIGATION_CAUSES`]).
pub fn obligation_cause(kind: ObligationKind) -> &'static OpenCause {
    OBLIGATION_CAUSES
        .iter()
        .find_map(|(k, cause)| (*k == kind).then_some(*cause))
        .expect("every obligation kind has a row")
}

pub const EQUALITY_OF_KEYS: Premise = Premise {
    name: "Equality of keys",
    states: "the comparison is `==` between keys of the two items, and each key the inner loop loads runs no code of the project",
    decided_in: &[
        "heki_rules::opt_rules::opt113",
        "heki_effect::resolver::ProjectDecls::plain_attr",
    ],
};

pub const ITEM_TYPE_UNKNOWN: OpenCause = OpenCause {
    id: "item-type-unknown",
    premise: &EQUALITY_OF_KEYS,
    states: "a key the inner loop loads is an attribute or an item of an item whose type heki does not know, so the load may run a property or a method",
    closer: Closer::Annotation,
};

pub const KEY_LOAD_MAY_RUN_CODE: OpenCause = OpenCause {
    id: "key-load-may-run-code",
    premise: &EQUALITY_OF_KEYS,
    states: "a key the inner loop loads from an item of a known type may run code of the project: no plain attribute of that name (a property, a method, another descriptor, `__getattr__` or `__getattribute__` along the class's MRO or in a project subclass, a base heki does not know, or a metaclass), or a subscript of a value that is no builtin container",
    closer: Closer::None,
};

pub const STEPS_THROUGH_INPUTS: Premise = Premise {
    name: "Steps through inputs",
    states: "each loop steps through items of the function's inputs",
    decided_in: &["heki_rules::opt_rules::JoinPremises::steps_through_inputs"],
};

pub const INPUT_ITEMS_FIXED_BY_THE_CODE: OpenCause = OpenCause {
    id: "fixed-by-the-code",
    premise: &STEPS_THROUGH_INPUTS,
    states: "a loop steps through items the code fixes: a module constant or a display",
    closer: Closer::None,
};

pub const ITEMS_NOT_FROM_THE_INPUTS: OpenCause = OpenCause {
    id: "items-not-from-the-inputs",
    premise: &STEPS_THROUGH_INPUTS,
    states: "a loop's items are not established to come from the function's inputs: they come from a call's result or a method's receiver (`self.rows`), or heki follows none",
    closer: Closer::None,
};

pub const IDLE_ON_A_MISMATCH: Premise = Premise {
    name: "Idle on a mismatch",
    states: "an inner item whose key differs runs only the loads of the two keys and the comparison",
    decided_in: &["heki_rules::opt_rules::JoinPremises::idle_on_mismatch"],
};

pub const WORK_ON_A_MISMATCH: OpenCause = OpenCause {
    id: "work-on-a-mismatch",
    premise: &IDLE_ON_A_MISMATCH,
    states: "an inner item whose key differs is not established to run only the key loads and the comparison: a statement before the test or after the `if`, an `else` or `elif`, an earlier `and` operand, or a way out of the inner loop on that path",
    closer: Closer::Code,
};

pub const ITERABLE_TYPE_UNKNOWN: OpenCause = OpenCause {
    id: "iterable-type-unknown",
    premise: &IDLE_ON_A_MISMATCH,
    states: "the inner loop's iterable is a value whose type heki does not know, which may be a generator that runs code for each item",
    closer: Closer::Annotation,
};

pub const ADVANCING_RUNS_CODE: OpenCause = OpenCause {
    id: "advancing-runs-code",
    premise: &IDLE_ON_A_MISMATCH,
    states: "advancing the inner loop may run code of the project: its iterable is a call's result (a generator, `map`, `filter`, a project function), or a container whose class or a project subclass of a builtin container defines its own `__iter__`",
    closer: Closer::None,
};

pub const SAME_COLLECTION_SAME_KEYS: Premise = Premise {
    name: "Same collection, same keys",
    states: "the inner loop steps through the same collection, with the same keys, on every iteration of the outer loop",
    decided_in: &[
        "heki_rules::opt_rules::JoinPremises::same_collection_each_time",
        "heki_rules::opt_rules::JoinPremises::keys_unchanged",
    ],
};

pub const CACHED_PROPERTY: Premise = Premise {
    name: "Cached property",
    states: "every class the instance may be (R2, R5) that names the property gets it from a synchronous getter, the first definition along its MRO with no base heki does not know before it, of a cached-property decorator the ecosystem catalog lists as keeping the value in the instance `__dict__` (`[[cached_properties]]`: `functools`, Django, Werkzeug, propcache, kombu; R3; for one that keeps it in a slot on an instance without a `__dict__`, no class along the MRO defines `__slots__`), and none has a metaclass other than `type` or `abc.ABCMeta`; an instance of a class that names nothing so holds no value of it",
    decided_in: &[
        "heki_rules::mut_rules::stale_reads::classes_running",
        "heki_rules::mut_rules::kept_values::may_hold",
        "heki_rules::mut_rules::kept_values::cached_getter_reads",
        "heki_effect::resolver::ProjectDecls::function_taken",
        "heki_effect::resolver::ProjectDecls::defines_slots",
        "heki_effect::resolver::ProjectDecls::has_custom_metaclass",
        "heki_kb::EffectiveEcosystemKb::cached_property",
    ],
};

pub const READS_ON_EVERY_PATH: Premise = Premise {
    name: "Reads on every path",
    states: "the getter reads the written attribute, with the written constant key or as a whole, on every path on which it returns normally, itself or through `self` methods and properties it calls on every such path, each of whose implementations runs its body at every call and has the fact",
    decided_in: &[
        "heki_effect::summary::Extractor::must_self_reads",
        "heki_effect::callgraph::must_reads_through_self_calls",
    ],
};

pub const PLAIN_WRITE: Premise = Premise {
    name: "Plain write",
    states: "the write rebinds or deletes the attribute with a plain store (no property, descriptor, `__setattr__`, `__delattr__`, or metaclass in a class the instance may be, none of those hooks bound on one after its body, and every base a project class or a plain one), or changes in place the builtin list, dict, or set the attribute holds by its type through a changing method or an item store that no project subclass replaces",
    decided_in: &[
        "heki_rules::mut_rules::kept_values::plain_write",
        "heki_effect::cache_flow::in_place_change",
        "heki_effect::resolver::ProjectDecls::method_not_overridden",
    ],
};

pub const KEPT_TO_THE_RETURN: Premise = Premise {
    name: "Kept to the return",
    states: "no point from the write to the method's normal return may remove or replace the kept value, the write's own instruction (iterating its argument, hashing a key) and exception edges into handlers included: no store or deletion of the property's name, of an item of a `__dict__` under it, or of a `__dict__` or `__class__`, through any reference but one established as an object that keeps no value under the name (a value built in the method, an instance of classes none of which binds a property getter of the name); no `await` or `yield`; no call whose targets may remove or replace it, every call on those paths being a project call whose targets are every implementation it can run or a call of a library the catalog covers handed no callable, no instance of a project class, and no value of unknown type (R3); and no code of the project an instruction runs implicitly: a special method of an operand's class (an operator, a comparison, `in`, a conversion or format, a subscript, an unpacking, a call's `*` or `**` argument, a hashed key, a truth test, `with`), established as `object`'s, a builtin type's (with what it runs on the items, a stored key compared under an equal hash), or a library class's handed no instance of a project class (R3) on every class the operand may be (R2), where a value of a nominal container ABC (`Sequence`, `Mapping`, `collections.abc.Set`, their mutable ABCs) is one of its family's builtins or a project class implementing it (deriving, through project classes, from the ABC or a class of its family, or registered with one) unless a `register` heki cannot resolve or a `__subclasshook__` opens the family, and any other abstract class, a protocol, `object`, and a value heki does not class establish none, and a comparison or a conversion gives a builtin value only when those methods are; an attribute access (`__getattr__`, `__setattr__`, a property heki does not resolve, another descriptor); a raise of a value that may be a class; a class statement or an import of a project module",
    decided_in: &[
        "heki_effect::summary::Extractor::kept_value_points",
        "heki_effect::summary::Extractor::established_values",
        "heki_ir::IrFunction::special_methods",
        "heki_effect::invokes::special_methods_run_project_code",
        "heki_effect::ProjectDecls::implementers",
        "heki_effect::invokes::may_keep_under",
        "heki_effect::invokes::attribute_access_runs_project_code",
        "heki_effect::cache_flow::cache_flow",
        "heki_rules::mut_rules::kept_values::KeptAfterWrites::kept_to_return",
    ],
};

pub const READS_THE_PROPERTY: Premise = Premise {
    name: "Reads the property",
    states: "the function's own body reads the property on the object (`obj.p`, a constant `getattr`, an item of `obj.__dict__`)",
    decided_in: &["heki_rules::mut_rules::stale_reads::CachedReads::of"],
};

pub const RUNS_THE_WRITER: Premise = Premise {
    name: "Runs the writer",
    states: "every class the object may be (R2) gets the property from a `@cached_property` getter and runs a write of it whose `MUT101` report is exact: the first definition of the writing method along its MRO, or one the definitions before it reach through `super()` without invalidating the property",
    decided_in: &[
        "heki_rules::mut_rules::stale_reads::CachedReads::exact_read",
        "heki_rules::mut_rules::stale_reads::classes_running",
    ],
};

pub const COLLECTION_MAY_CHANGE: OpenCause = OpenCause {
    id: "collection-may-change",
    premise: &SAME_COLLECTION_SAME_KEYS,
    states: "the inner loop is not established to step through the same collection on every outer iteration: its iterable is computed in the outer loop from other than values bound before it, or the outer loop changes one of those values (a changing method, a store, `del`, a call that receives it, a closure, `yield`, or `await`)",
    closer: Closer::Code,
};

pub const KEYS_MAY_CHANGE: OpenCause = OpenCause {
    id: "keys-may-change",
    premise: &SAME_COLLECTION_SAME_KEYS,
    states: "the outer loop may change the compared key of an inner item: a store into or deletion of what the key loads, or a call or closure that reaches the item or the key's object",
    closer: Closer::Code,
};

pub const NEW_OBJECT: Premise = Premise {
    name: "New object",
    states: "the object is built as the builtin itself where it is changed, copied there by a copy the catalog describes from a value of a builtin container type no project subclass copies otherwise (R2), or returned by project functions whose every return is such an object that they keep nowhere, and the call or property read resolves to every function it can run",
    decided_in: &[
        "heki_effect::fresh::source",
        "heki_effect::fresh::FreshReturn::join_callee",
    ],
};

pub const UNNAMED: Premise = Premise {
    name: "Unnamed",
    states: "the object is only the value of the changed expression; nothing reads it but the change and, for an augmented item assignment, the item load it starts with",
    decided_in: &["heki_rules::mut_rules::lost_changes::changes"],
};

/// Every premise of the table, once.
/// The premises of a `readonly-parameter` suggestion of `heki recommend`
/// ([`crate::recommend::types`]), in order: the claim is the first four ("the function only
/// reads the container bound to the parameter, and so does every function it hands it to, with
/// the builtin's own methods"), the rest are the edit's. A suggestion is no report, so these rows are not in [`ALL`].
pub const READONLY_PARAMETER: &[&Premise] = &[
    &Premise {
        name: "Only read here",
        states: "the function changes neither the parameter nor a local alias of it in place: no mutator method, item store or deletion, augmented assignment, or bound mutator",
        decided_in: &["heki_effect::EffectSummary::mutated_param_paths"],
    },
    &Premise {
        name: "Does not leave",
        states: "the object is not stored (an attribute, an item, a global, a closure cell, a display), captured by a nested function, yielded, or returned",
        decided_in: &["heki_effect::EffectSummary::only_read_params"],
    },
    &Premise {
        name: "Every call resolves and only reads",
        states: "each call it is handed to resolves to project functions whose parameter only reads it in every implementation, or to a library call Layer 1 lists as only reading that argument",
        decided_in: &["heki_effect::EffectSummary::only_read_params"],
    },
    &Premise {
        name: "The builtin's own reads",
        states: "no project class a value of the read-only type the suggestion writes may be (R2 over the family: one deriving, through project classes, from the builtin, from a standard-library class of its family such as OrderedDict, UserDict, or ChainMap, or from the ABC, or registered with one; a register heki cannot resolve or a __subclasshook__ admits any class) binds a method the reads of the object here and in its callees run: item access with __missing__, iteration (and the length hint of [*v], f(*v), and xs += v), size and truth, membership, comparison, conversion to text, operators, keys() and the items of **v, the length a sequence or mapping pattern tests and a mapping pattern's get, the methods called by name, and what the [[invokes]] entry of a library call reading it names; a read with no such list (a library call without an entry, a method the builtin lacks) may run any member of the class; none is created by a metaclass heki does not know, and none defines __getattribute__ where a method is called by name; a class outside the project is taken to run what the builtin runs",
        decided_in: &[
            "heki_effect::EffectSummary::read_kinds",
            "heki_effect::ProjectDecls::implementers",
            "heki_effect::read_override",
        ],
    },
    &Premise {
        name: "Literal container annotation",
        states: "the annotation is a builtin list, dict, or set, or its typing form, after Optional, | None, and quotes; not Annotated[...], a Mutable* ABC, or an alias",
        decided_in: &["heki_effect::resolver::param_annotation"],
    },
    &Premise {
        name: "Abstract use",
        states: "every use of the parameter exists on the read-only type, and a value derived from it is only iterated, unpacked, tested, measured, or handed to a call that only reads it; a set gives no iteration order",
        decided_in: &["heki_effect::ContainerParams::abstract_use"],
    },
    &Premise {
        name: "Name bound once",
        states: "the parameter name is not bound again, except `if p is None: p = <a new container of its kind>`",
        decided_in: &["heki_effect::ContainerParams::bound_once"],
    },
    &Premise {
        name: "Callees take it",
        states: "every project parameter it is handed to is unannotated, Any, object, or an abstract type the read-only type is, or is in the same group",
        decided_in: &["crate::recommend::types::callees_take_it"],
    },
    &Premise {
        name: "Whole family",
        states: "not a Protocol member, no @overload, one definition with a body that does something; the same parameter of every definition of its override family is in the group, and every base of their classes is a project class or object",
        decided_in: &["crate::recommend::types::whole_family"],
    },
    &Premise {
        name: "Not replaced",
        states: "nothing rebinds the function or replaces its code (R1)",
        decided_in: &["heki_effect::ProjectDecls::call_rebound"],
    },
    &Premise {
        name: "Annotations stay unread",
        states: "every decorator is in the decorator table, which leaves annotations unread (a dataclass without a docstring reads its constructor's signature); the class has no metaclass but type or abc.ABCMeta and no base outside the project but one of the standard library or one the catalog lists as reading only fields; the function is used only as the callee of calls (T1)",
        decided_in: &[
            "heki_effect::only_read::annotation_facts",
            "heki_effect::FileSummary::reference_sites",
        ],
    },
    &Premise {
        name: "No fact lost",
        states: "the file analyzed again with the groups' edits gives the same local facts and the same reports, every rule selected, and so do the modules an import-boundaries contract walking imports judges; a heuristic group's reports are not compared",
        decided_in: &[
            "crate::recommend::types::trial_all",
            "heki_effect::FileSummary::edit_changes",
        ],
    },
    &Premise {
        name: "A file to edit",
        states: "the file does not say it is generated (a leading comment with generated or DO NOT EDIT), and no stub declares the function",
        decided_in: &["crate::recommend::types::file_to_edit"],
    },
    &Premise {
        name: "A name for the type",
        states: "the module binds the read-only type under a name before the annotation is evaluated (an import above the def, anywhere with postponed annotations), or the name the edit imports it as is free there; no name is written that a scope the annotation is read in before the module (a method's class body, an enclosing function) or a star import of the module binds",
        decided_in: &[
            "crate::recommend::types::type_name",
            "crate::recommend::types::annotation_binder",
        ],
    },
];

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
    &NEW_OBJECT,
    &UNNAMED,
    &CACHED_PROPERTY,
    &READS_ON_EVERY_PATH,
    &PLAIN_WRITE,
    &KEPT_TO_THE_RETURN,
    &READS_THE_PROPERTY,
    &RUNS_THE_WRITER,
];

const MUT101_CLAIM: &[&Premise] = &[
    &CACHED_PROPERTY,
    &READS_ON_EVERY_PATH,
    &EVERY_IMPLEMENTATION,
    &PLAIN_WRITE,
    &KEPT_TO_THE_RETURN,
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
    &PREVIOUS_VALUE_NOT_HELD,
    &RESET_ON_A_PATH,
    &NOT_THE_CATALOG_CALL,
    &NO_CHANGING_BODY,
    &NOT_A_BUILTIN_CONTAINER_CHANGE,
    &NOT_ESTABLISHED_CACHED,
    &NOT_ESTABLISHED_KEYED,
    &NOT_ESTABLISHED_CONTAINER_KIND,
    &NOT_THE_RESULT_ON_EVERY_PATH,
    &RETURNED_THROUGH_A_FUNCTION,
    &CHANGED_IN_A_CALLED_FUNCTION,
    &NOT_A_CHANGE_OF_THE_KIND,
    &REPLACED_BY_A_SUBCLASS,
    &FRAME_INTROSPECTION,
    &ANALYSIS_NOT_CONVERGED,
    &EXACT_TYPE,
    &UNIQUE,
    &EXCLUSIVE_ACCESS_AT_WRITE,
    &EVAL_ORDER_PRESERVED,
    &EXCEPTION_STATE_SAFE,
    &DICT_UNPACK_LEADING,
    &SET_ORDER_UNOBSERVED,
    &VALUE_LOOP_INVARIANT,
    &ZERO_TRIP_EXCEPTION_SAFE,
    &HASH_CONSISTENT_PROBE,
    &NO_INTERMEDIATE_READS,
    &ONLY_PAIRED_APPENDS,
    &DEQUE_EQUIVALENT_OPS,
    &SIDE_EFFECT_FREE,
    &ITEM_TYPE_UNKNOWN,
    &KEY_LOAD_MAY_RUN_CODE,
    &INPUT_ITEMS_FIXED_BY_THE_CODE,
    &ITEMS_NOT_FROM_THE_INPUTS,
    &WORK_ON_A_MISMATCH,
    &ITERABLE_TYPE_UNKNOWN,
    &ADVANCING_RUNS_CODE,
    &COLLECTION_MAY_CHANGE,
    &KEYS_MAY_CHANGE,
];

/// The cause `id` of the premise named `premise`.
pub fn cause(premise: &str, id: &str) -> Option<&'static OpenCause> {
    CAUSES
        .iter()
        .copied()
        .find(|c| c.premise.name == premise && c.id == id)
}

/// A condition of an exact rule's exact tier that is no premise of its claim: the claim is true
/// whatever it says (the rule pages say so under "Not part of the claim"), and a report is exact
/// only when it holds because the change then pays off (Safety × Profitability, ADR-007,
/// ADR-011). The premise rows, the claims, and the oracle's strips do not list it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Condition {
    pub name: &'static str,
    pub axis: Axis,
    pub states: &'static str,
    /// Where heki decides it, as [`Premise::decided_in`].
    pub decided_in: &'static [&'static str],
}

pub const TRIP_COUNT_OPEN: Condition = Condition {
    name: "Trip count open",
    axis: Axis::Profitability,
    states: "the data decides the number of times the loop runs, so the cost grows with it: the loop steps through items the data decides, as \"Origin\" establishes them (a parameter, the rows of a query, the lines of a file, the entries of a directory), and the code does not fix or cap their number",
    decided_in: &[
        "heki_ir::facts::FnIndex::fixed_trips",
        "heki_rules::eff_rules::DataOrigins::of_loop_items",
    ],
};

/// Why a report left a [`Condition`] open, from a closed list kept next to the condition, as
/// [`OpenCause`] is for a premise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ConditionCause {
    /// Unique within its condition, kebab-case.
    pub id: &'static str,
    pub condition: &'static Condition,
    pub states: &'static str,
    pub closer: Closer,
}

pub const TRIP_COUNT_FIXED: ConditionCause = ConditionCause {
    id: "trip-count-fixed",
    condition: &TRIP_COUNT_OPEN,
    states: "the code fixes or caps the loop's trip count (`range` of constants, a display, a slice, a length test, a counter): the change pays off only for a loop the data sizes",
    closer: Closer::None,
};

pub const TRIPS_FIXED_BY_THE_CODE: ConditionCause = ConditionCause {
    id: FIXED_BY_THE_CODE.id,
    condition: &TRIP_COUNT_OPEN,
    states: "some of the loop's items are ones the code fixes (a literal, a display, a module constant, `range` of constants), although it does not fix their number",
    closer: FIXED_BY_THE_CODE.closer,
};

pub const TRIPS_NOT_FROM_THE_DATA: ConditionCause = ConditionCause {
    id: ITEMS_NOT_FROM_THE_DATA.id,
    condition: &TRIP_COUNT_OPEN,
    states: "the loop's items come from something heki does not take as the data: a method's receiver (`self.layers`), a project function's result whose returns heki does not follow to such items, or a call heki does not resolve, so the program's structure may decide their number",
    closer: ITEMS_NOT_FROM_THE_DATA.closer,
};

pub const TRIPS_FROM_A_CALL_WITHOUT_ITEMS: ConditionCause = ConditionCause {
    id: LIBRARY_CALL_WITHOUT_ITEMS.id,
    condition: &TRIP_COUNT_OPEN,
    states: "the loop's items are a library call's result whose catalog entry does not say that the data decides their number (`items`)",
    closer: LIBRARY_CALL_WITHOUT_ITEMS.closer,
};

pub const TRIPS_WITHOUT_ITEMS_FOLLOWED: ConditionCause = ConditionCause {
    id: NO_ITEMS_FOLLOWED.id,
    condition: &TRIP_COUNT_OPEN,
    states: "heki follows no items the loop steps through: a `while` loop, `range` of a number that is no constant or length, or an iterable the walk does not decide",
    closer: NO_ITEMS_FOLLOWED.closer,
};

/// The causes of "Trip count open" for a loop whose items "Origin" does not establish as the
/// data's: one for each cause of "Origin", with its id and closer ([`trip_count_cause`]).
const TRIP_COUNT_ORIGIN_CAUSES: &[&ConditionCause] = &[
    &TRIPS_FIXED_BY_THE_CODE,
    &TRIPS_NOT_FROM_THE_DATA,
    &TRIPS_FROM_A_CALL_WITHOUT_ITEMS,
    &TRIPS_WITHOUT_ITEMS_FOLLOWED,
];

/// Every cause of a condition, once.
pub const CONDITION_CAUSES: &[&ConditionCause] = &[
    &TRIP_COUNT_FIXED,
    &TRIPS_FIXED_BY_THE_CODE,
    &TRIPS_NOT_FROM_THE_DATA,
    &TRIPS_FROM_A_CALL_WITHOUT_ITEMS,
    &TRIPS_WITHOUT_ITEMS_FOLLOWED,
];

/// The cause of "Trip count open" for a loop whose items are not the data's, `origin` being the
/// cause of "Origin" that says why ([`TRIP_COUNT_ORIGIN_CAUSES`]).
pub fn trip_count_cause(origin: &OpenCause) -> &'static ConditionCause {
    TRIP_COUNT_ORIGIN_CAUSES
        .iter()
        .copied()
        .find(|c| c.id == origin.id)
        .unwrap_or(&TRIPS_NOT_FROM_THE_DATA)
}

impl ConditionCause {
    /// The record of this cause on a report: where the condition is open, and what the
    /// statement leaves out.
    pub fn open_at(
        &'static self,
        at: Option<RelatedLocation>,
        detail: Option<String>,
    ) -> OpenRecord {
        OpenRecord::Condition(OpenCondition {
            axis: self.condition.axis,
            condition: self.condition.name,
            cause: self.id,
            at,
            detail,
        })
    }
}

/// The cause `id` of the condition named `condition`.
pub fn condition_cause(condition: &str, id: &str) -> Option<&'static ConditionCause> {
    CONDITION_CAUSES
        .iter()
        .copied()
        .find(|c| c.condition.name == condition && c.id == id)
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
        "MUT101" => MUT101_CLAIM,
        "MUT111" => &[
            &CACHED_PROPERTY,
            &READS_ON_EVERY_PATH,
            &EVERY_IMPLEMENTATION,
            &PLAIN_WRITE,
            &KEPT_TO_THE_RETURN,
            &READS_THE_PROPERTY,
            &RUNS_THE_WRITER,
        ],
        "MUT114" => &[
            &CACHED,
            &KEYED,
            &CONTAINER_KIND,
            &SAME_OBJECT,
            &EVERY_IMPLEMENTATION,
            &CHANGED_IN_PLACE,
        ],
        "MUT119" => &[&NEW_OBJECT, &UNNAMED, &CHANGED_IN_PLACE],
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
    fn a_condition_is_no_premise_and_its_causes_are_unique_kebab_case() {
        let keys: std::collections::BTreeSet<(&str, &str)> = CONDITION_CAUSES
            .iter()
            .map(|c| (c.condition.name, c.id))
            .collect();
        assert_eq!(keys.len(), CONDITION_CAUSES.len(), "a cause twice");
        for c in CONDITION_CAUSES {
            assert!(
                ALL.iter().all(|p| p.name != c.condition.name),
                "{} is a premise row",
                c.condition.name
            );
            assert!(
                c.id.chars().all(|ch| ch.is_ascii_lowercase() || ch == '-')
                    && !c.id.starts_with('-')
                    && !c.id.ends_with('-'),
                "{} is not kebab-case",
                c.id
            );
            assert_eq!(condition_cause(c.condition.name, c.id), Some(*c));
        }
    }

    #[test]
    fn each_cause_of_origin_has_one_of_trip_count_open_with_its_closer() {
        let origin: Vec<&OpenCause> = CAUSES
            .iter()
            .copied()
            .filter(|c| *c.premise == ORIGIN)
            .collect();
        assert_eq!(origin.len(), TRIP_COUNT_ORIGIN_CAUSES.len());
        for o in origin {
            let c = trip_count_cause(o);
            assert_eq!((c.id, c.closer), (o.id, o.closer), "{}", o.id);
            assert_eq!(*c.condition, TRIP_COUNT_OPEN);
            assert!(CONDITION_CAUSES.contains(&c), "{}", o.id);
        }
    }

    #[test]
    fn the_causes_follow_the_order_of_their_premises() {
        let position = |c: &OpenCause| ALL.iter().position(|p| *p == c.premise);
        assert!(CAUSES.windows(2).all(|w| position(w[0]) <= position(w[1])));
    }

    #[test]
    fn every_obligation_kind_has_one_cause_of_the_rewrite_premise() {
        use ObligationKind as K;
        // Exhaustive: a new obligation kind does not compile here until it has its row.
        let row = |kind: ObligationKind| match kind {
            K::A1HazardFree => 0,
            K::AnalysisConverged => 1,
            K::ExactType => 2,
            K::Unique => 3,
            K::ExclusiveAccessAtWrite => 4,
            K::EvalOrderPreserved => 5,
            K::ExceptionStateSafe => 6,
            K::DictUnpackLeading => 7,
            K::SetOrderUnobserved => 8,
            K::ValueLoopInvariant => 9,
            K::ZeroTripExceptionSafe => 10,
            K::HashConsistentProbe => 11,
            K::NoIntermediateReads => 12,
            K::OnlyPairedAppends => 13,
            K::DequeEquivalentOps => 14,
            K::SideEffectFree => 15,
        };
        assert_eq!(OBLIGATION_CAUSES.len(), 16);
        for (i, (kind, cause)) in OBLIGATION_CAUSES.iter().enumerate() {
            assert_eq!(row(*kind), i, "{kind}");
            assert_eq!(cause.premise, &REWRITE_PROVEN_SAFE, "{kind}");
            assert_eq!(obligation_cause(*kind), *cause);
            assert!(CAUSES.contains(cause), "{kind}");
        }
    }
}
