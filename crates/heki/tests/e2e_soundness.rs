//! `heki_soundness` end to end (Milestone M8): the abstract interpreter's transfer functions and
//! every `OPT` rule path, through the real `heki check` binary.
//!
//! Each fixture under `tests/difftest/soundness_e2e_*.py` is checked twice:
//! 1. its `OPT` diagnostics (`--output-format json`) must be exactly the table below, function by
//!    function: the rule, and either the proven-safe edits or the first blocker;
//! 2. CPython must print the same transcript before and after `--fix --include-size-dependent`
//!    (and after the default `--fix`), and a second run finds nothing left to fix.

mod common;

use common::{Project, repo_file, run_python};
use serde_json::Value;

/// What one `OPT` diagnostic must say.
#[derive(Debug, Clone, Copy)]
enum V {
    /// Proven safe; the fix's replacement texts, in source order.
    Fix(&'static [&'static str]),
    /// Proven safe, but no automatic edit (`[proven safe; rewrite it by hand]`).
    Hand,
    /// Not proven: the first blocker (`Kind(subject): reason`) starts with this text.
    Blocked(&'static str),
}

/// Expected `OPT` diagnostics of one function (by qualname), in source order.
type Expect = (&'static str, &'static [(&'static str, V)]);

fn opt_json(p: &Project, args: &[&str]) -> Vec<Value> {
    // The Soundness lane's rules: `OPT112` / `OPT113` are Advisory-lane reports (no rewrite,
    // tested in `e2e_v1x_default_rules.rs`) that some fixtures here also trigger.
    let mut all = vec![
        "--select",
        "OPT",
        "--ignore",
        "OPT112,OPT113",
        "--output-format",
        "json",
    ];
    all.extend_from_slice(args);
    let (code, stdout, stderr) = p.check(&all);
    assert!(code == 0 || code == 1, "exit {code}: {stderr}");
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"))
}

fn replacements(d: &Value) -> Vec<String> {
    d["fix"]
        .as_array()
        .map(|edits| {
            edits
                .iter()
                .map(|e| e["replacement"].as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// The diagnostics of `diags` match `expect` exactly (functions without an entry have none).
fn assert_verdicts(diags: &[Value], expect: &[Expect]) {
    for d in diags {
        let q = d["qualname"].as_str().unwrap();
        assert!(
            expect.iter().any(|(f, _)| *f == q),
            "unexpected diagnostic in `{q}`: {d:#}"
        );
    }
    for (function, want) in expect {
        let got: Vec<&Value> = diags
            .iter()
            .filter(|d| d["qualname"] == *function)
            .collect();
        let rules: Vec<&str> = got
            .iter()
            .map(|d| d["rule_code"].as_str().unwrap())
            .collect();
        let want_rules: Vec<&str> = want.iter().map(|(r, _)| *r).collect();
        assert_eq!(rules, want_rules, "{function}: {got:#?}");
        for (d, (_, v)) in got.iter().zip(want.iter()) {
            let blocker = d["blocker"].as_str();
            match v {
                V::Fix(reps) => {
                    assert_eq!(blocker, None, "{function}: {d:#}");
                    assert_eq!(replacements(d), *reps, "{function}: {d:#}");
                }
                V::Hand => {
                    assert_eq!(blocker, None, "{function}: {d:#}");
                    assert!(d["fix"].is_null(), "{function}: {d:#}");
                    assert!(
                        d["help"]
                            .as_str()
                            .unwrap()
                            .ends_with("[proven safe; rewrite it by hand]"),
                        "{function}: {d:#}"
                    );
                }
                V::Blocked(prefix) => {
                    assert!(d["fix"].is_null(), "{function}: {d:#}");
                    let b = blocker.unwrap_or_else(|| panic!("{function}: no blocker: {d:#}"));
                    assert!(b.starts_with(prefix), "{function}: {b}");
                }
            }
        }
    }
}

/// Verdicts, then CPython equivalence of the default and the size-dependent `--fix`.
fn check_fixture(name: &str, fixture: &str, expect: &[Expect]) {
    let p = Project::new(name);
    let original = repo_file(fixture);
    p.write("cases.py", &original);
    assert_verdicts(&opt_json(&p, &[]), expect);
    let before = run_python(&p.dir, "cases.py");
    for args in [&["--fix"][..], &["--fix", "--include-size-dependent"][..]] {
        p.write("cases.py", &original);
        let (_, _, stderr) = p.check(&[&["--select", "OPT"][..], args].concat());
        assert!(stderr.contains("Fixed "), "{args:?}: {stderr}");
        let fixed = p.read("cases.py");
        assert_eq!(
            before,
            run_python(&p.dir, "cases.py"),
            "{args:?} changed the output:\n{fixed}"
        );
        // Every applicable proven-safe edit was applied; nothing is left for another run.
        let size_dependent = args.len() == 2;
        let left = opt_json(&p, &["--include-size-dependent"]);
        assert!(
            left.iter().all(|d| d["fix"].is_null()
                || (!size_dependent
                    && !matches!(d["rule_code"].as_str(), Some("OPT101" | "OPT102")))),
            "{args:?}: {left:#?}"
        );
    }
}

const ONE: &str = "x.append(i)";

// ======================================================================== interpreter

const INTERP: &[Expect] = &[
    ("scaled", &[("OPT101", V::Fix(&["x.append(i * factor)"]))]),
    (
        "unary_mix",
        &[(
            "OPT101",
            V::Fix(&["x.extend([-i, ~i, not i, -flag, +f, -f])"]),
        )],
    ),
    ("unary_unknown", &[("OPT101", V::Fix(&["x.append(-p)"]))]),
    (
        "binops",
        &[(
            "OPT101",
            V::Fix(&["x.extend([t, s, b, diff, bits, merged])"]),
        )],
    ),
    (
        "binops_unknown_results",
        &[(
            "OPT101",
            V::Fix(&["out.extend([a, [i] * 2, c, i / 2, i ** 2, i << 1])"]),
        )],
    ),
    ("compares", &[("OPT101", V::Fix(&["x.append(v)"]))]),
    (
        "vowels_seen",
        &[(
            "OPT110",
            V::Fix(&["    vowels_set = set(vowels)\n", "vowels_set"]),
        )],
    ),
    (
        "byte_codes",
        &[(
            "OPT110",
            V::Fix(&["    codes_set = set(codes)\n", "codes_set"]),
        )],
    ),
    (
        "mixed_tuple_probe",
        &[(
            "OPT110",
            V::Fix(&["    allowed_set = set(allowed)\n", "allowed_set"]),
        )],
    ),
    (
        "slices_and_items",
        &[(
            "OPT101",
            V::Fix(&[
                "x.extend([nums[1:], pair[0:1], text[:2], data[1:], nums[0], pair[1], table[\"k\"]])",
            ]),
        )],
    ),
    (
        "unknown_subscripts",
        &[("OPT101", V::Fix(&["x.append(first)"]))],
    ),
    (
        "subscript_writes",
        &[(
            "OPT110",
            V::Fix(&["    allowed_set = set(allowed)\n", "allowed_set"]),
        )],
    ),
    ("Tally.collect", &[("OPT101", V::Fix(&["out.append(i)"]))]),
    (
        "bound_method_alias",
        &[(
            "OPT101",
            V::Blocked("Unique(acc): it may be shared: passed to code heki cannot see"),
        )],
    ),
    (
        "free_variables.<locals>.collect",
        &[(
            "OPT101",
            V::Blocked(
                "Unique(out): it may be shared: stored in a container, attribute, or global",
            ),
        )],
    ),
    (
        "stored_in_global",
        &[(
            "OPT101",
            V::Blocked("Unique(x): it may be shared: stored in a container, attribute, or global"),
        )],
    ),
    (
        "scaled_by_global",
        &[("OPT101", V::Fix(&["x.append(math.floor(i * SCALE))"]))],
    ),
    ("deleted_alias", &[("OPT101", V::Fix(&[ONE]))]),
    (
        "maybe_unbound",
        &[("OPT101", V::Fix(&["x.extend([i, seed])"]))],
    ),
    (
        "long_copy_chain",
        &[(
            "OPT110",
            V::Fix(&["    allowed_set = set(allowed)\n", "allowed_set"]),
        )],
    ),
    (
        "long_copy_chain_mixed",
        &[(
            "OPT110",
            V::Blocked("HashConsistentProbe(b10): `b10` may have a custom `__eq__`"),
        )],
    ),
    (
        "inner_loop_over_list",
        &[("OPT101", V::Fix(&["x.append(v * i)"]))],
    ),
    (
        "escaped_iterator",
        &[(
            "OPT101",
            V::Blocked("Unique(x): it may be shared: an iterator or view of it escapes"),
        )],
    ),
    ("generator_after_loop", &[("OPT101", V::Fix(&[ONE]))]),
    ("generator_over_param", &[("OPT101", V::Fix(&[ONE]))]),
    // S24 through a generator expression: `Disproved`, not reported.
    ("generator_before_loop", &[]),
    ("delegating", &[("OPT101", V::Fix(&[ONE]))]),
    ("awaiting", &[("OPT101", V::Fix(&["x.append(v)"]))]),
    ("within_context", &[("OPT101", V::Fix(&[ONE]))]),
    ("within_async_context", &[("OPT101", V::Fix(&[ONE]))]),
    (
        "known_calls",
        &[(
            "OPT101",
            V::Fix(&["x.extend([float(i), bool(i), int(\"10\", base=2), dict(k=i)])"]),
        )],
    ),
    (
        "methods",
        &[(
            "OPT101",
            V::Fix(&["x.extend([c, c.index(1), {1}.copy(), {\"a\": i}.copy()])"]),
        )],
    ),
    (
        "unknown_method_on_accumulator",
        &[(
            "OPT101",
            V::Blocked("Unique(x): it may be shared: passed to code heki cannot see"),
        )],
    ),
    (
        "in_place_updates",
        &[(
            "OPT110",
            V::Fix(&["    allowed_set = set(allowed)\n", "allowed_set"]),
        )],
    ),
    ("handler_names", &[("OPT101", V::Fix(&["x.append(v)"]))]),
    (
        "two_candidates_one_try",
        &[
            ("OPT101", V::Fix(&[ONE])),
            ("OPT101", V::Fix(&["y.append(i * 2)"])),
        ],
    ),
    (
        "branchy_try",
        &[
            ("OPT101", V::Fix(&[ONE])),
            ("OPT101", V::Fix(&["x.append(-i)"])),
        ],
    ),
    // The `finally` loop runs after `return x` took the list: appending would change the
    // returned list (disproved, not reported).
    ("returned_in_try", &[]),
    (
        "raised_accumulator",
        &[(
            "OPT101",
            V::Blocked("Unique(x): it may be shared: it is returned or raised"),
        )],
    ),
    (
        // `check(i)` may raise after the append with `z` still the old list, which the
        // handler returns: appending would change it (disproved, not reported).
        "stale_handler_alias",
        &[],
    ),
    (
        "many_sites",
        &[(
            "OPT101",
            V::Blocked(
                "Unique(x): it may be shared: it may hold objects from more than 4 allocation sites",
            ),
        )],
    ),
    ("consumed_before_the_loop", &[("OPT101", V::Fix(&[ONE]))]),
    (
        "slice_object_subscripts",
        &[("OPT101", V::Fix(&["x.extend([data[s], nums[s]])"]))],
    ),
];

#[test]
fn interpreter_constructs() {
    check_fixture(
        "sound_interp",
        "tests/difftest/soundness_e2e_interp.py",
        INTERP,
    );
}

// ======================================================================== accumulations

const HAND101: &[(&str, V)] = &[("OPT101", V::Hand)];

const ACCUMULATE: &[Expect] = &[
    ("trailing_comma", &[("OPT101", V::Fix(&[ONE]))]),
    ("starred", &[("OPT101", V::Fix(&["x.extend([*row])"]))]),
    ("one_line_body", &[("OPT101", V::Fix(&[ONE]))]),
    ("parenthesized", HAND101),
    ("parenthesized_operand", HAND101),
    ("annotated", HAND101),
    ("through_a_temporary", &[]),
    // `Disproved` (`EvalOrderPreserved`): never reported.
    ("walrus_rebinds", &[]),
    ("rhs_changes_list", &[]),
    (
        "rhs_changes_other",
        &[("OPT101", V::Fix(&["x.append(other.pop())"]))],
    ),
    // The read after `del x` is of an unbound name, not of the accumulator.
    ("deleted_accumulator", &[]),
    (
        "merge_exact_dict",
        &[("OPT102", V::Fix(&["d.update(extra)"]))],
    ),
    (
        "merge_unknown_mapping",
        &[(
            "OPT102",
            V::Blocked("EvalOrderPreserved(update): `d.update({...})` evaluates every entry"),
        )],
    ),
    (
        "merge_constant_keys",
        &[("OPT102", V::Fix(&["d.update({\"lo\": i, \"hi\": j})"]))],
    ),
    (
        "key_not_simple_text",
        &[("OPT102", V::Fix(&["d.update({(k): 1})"]))],
    ),
    ("constant_key", &[("OPT102", V::Fix(&["d[None] = v"]))]),
    ("parameter_key", &[("OPT102", V::Fix(&["d[key] = v"]))]),
    ("conditional_key", &[("OPT102", V::Fix(&["d[k] = v"]))]),
    ("dict_or", &[("OPT102", V::Fix(&["d |= {k: 1}"]))]),
    (
        "set_or_list",
        &[(
            "OPT102",
            V::Blocked("ExactType(s): its exact type is unknown (result of a binary operator)"),
        )],
    ),
    ("set_or_parenthesized", &[("OPT102", V::Hand)]),
    ("int_flags", &[]),
    ("int_or", &[]),
    ("merge_other_first", &[]),
    ("merge_twice", &[]),
    ("bytes_literal_pieces", &[("OPT103", V::Hand)]),
    (
        "bytes_unknown_pieces",
        &[(
            "OPT103",
            V::Blocked("ExactType(acc): its exact type is unknown (result of a binary operator)"),
        )],
    ),
    ("tuple_in_place", &[("OPT103", V::Hand)]),
    (
        "bytes_plus_str",
        &[(
            "OPT103",
            V::Blocked("ExactType(acc): its exact type is unknown (result of a binary operator)"),
        )],
    ),
    ("str_accumulation", &[]),
    ("number_accumulation", &[]),
    (
        "read_in_loop",
        &[(
            "OPT103",
            V::Blocked("NoIntermediateReads(acc): `acc` is read inside the loop"),
        )],
    ),
    (
        "tested_in_loop",
        &[(
            "OPT103",
            V::Blocked("NoIntermediateReads(acc): `acc` is read inside the loop"),
        )],
    ),
    (
        "handler_reads",
        &[(
            "OPT103",
            V::Blocked(
                "ExceptionStateSafe(acc): an exception handler reads `acc` before the buffer is joined",
            ),
        )],
    ),
    // Bounded loops do not pay for the buffer rewrite.
    ("few_iterations", &[]),
    ("retry", &[]),
    (
        "Report.render",
        &[(
            "OPT103",
            V::Blocked("Unique(self.s): `self.s` is reachable through `self`"),
        )],
    ),
    // The loop's test caps the accumulator; each item's attribute gets one piece.
    ("capped_by_test", &[]),
    ("element_attributes", &[]),
    (
        "comment_after_element",
        &[(
            "OPT101",
            V::Fix(&["x.append(i  # keep this comment\n                 )"]),
        )],
    ),
    (
        "comment_before_starred",
        &[(
            "OPT101",
            V::Fix(&["x.extend([  # spread\n            *row])"]),
        )],
    ),
    (
        "merge_with_comment",
        &[(
            "OPT102",
            V::Fix(&["d.update(extra  # keep this comment\n             )"]),
        )],
    ),
    // `list += tuple` extends in place; an attribute that holds a `str` is copied.
    ("Collector.collect", &[]),
    (
        "local_holder",
        &[(
            "OPT103",
            V::Blocked("Unique(holder.text): `holder.text` is reachable through `holder`"),
        )],
    ),
    (
        "set_or_built",
        &[("OPT102", V::Fix(&["s |= set([g, -g])"]))],
    ),
    (
        "Encoder.encode",
        &[
            (
                "OPT103",
                V::Blocked("Unique(self.data): `self.data` is reachable through `self`"),
            ),
            (
                "OPT103",
                V::Blocked("Unique(self.parts): `self.parts` is reachable through `self`"),
            ),
        ],
    ),
];

#[test]
fn accumulation_rules() {
    check_fixture(
        "sound_accumulate",
        "tests/difftest/soundness_e2e_accumulate.py",
        ACCUMULATE,
    );
}

// ======================================================================== hoisting

const ALLOWED_SET: V = V::Fix(&["    allowed_set = set(allowed)\n", "allowed_set"]);

const HOIST: &[Expect] = &[
    ("tuple_container", &[("OPT110", ALLOWED_SET)]),
    // No `for` statement to hoist before.
    ("while_loop", &[("OPT110", V::Hand)]),
    ("comprehension", &[("OPT110", V::Hand)]),
    ("split_for_header", &[("OPT110", V::Hand)]),
    // `Disproved` (`ValueLoopInvariant`): never reported.
    ("rebound_in_loop", &[]),
    ("changed_twice_in_loop", &[]),
    ("changed_in_for_header", &[]),
    ("changed_before_and_after", &[("OPT110", ALLOWED_SET)]),
    // No exact container type: nothing shows a linear scan.
    ("parameter_container", &[]),
    ("literal_container", &[]),
    ("string_container", &[]),
    (
        "unknown_elements",
        &[(
            "OPT110",
            V::Blocked("ZeroTripExceptionSafe(allowed): element types are unknown"),
        )],
    ),
    // The path where `allowed` is unbound returns first: code after the `return` flows
    // nowhere, and the hoisted set is always built from a bound name.
    (
        "unbound_in_dead_code",
        &[(
            "OPT110",
            V::Fix(&["    allowed_set = set(allowed)\n", "allowed_set"]),
        )],
    ),
    (
        "call_probe",
        &[(
            "OPT110",
            V::Blocked("HashConsistentProbe(<expr>): the result of `str(...)` may have a custom"),
        )],
    ),
    (
        "method_probe",
        &[(
            "OPT110",
            V::Blocked("HashConsistentProbe(<expr>): the result of `.lower()` may have a custom"),
        )],
    ),
    (
        "generator_probe",
        &[(
            "OPT110",
            V::Blocked("HashConsistentProbe(<expr>): a generator expression may have a custom"),
        )],
    ),
    (
        "attribute_probe",
        &[(
            "OPT110",
            V::Blocked("HashConsistentProbe(<expr>): a temporary may have a custom"),
        )],
    ),
    (
        "mixed_probe",
        &[(
            "OPT110",
            V::Blocked("HashConsistentProbe(p): `p` may have a custom"),
        )],
    ),
    ("empty_probe", &[("OPT110", ALLOWED_SET)]),
    // The value of a deleted name is not tracked: no evidence, not reported.
    ("deleted_container", &[]),
    (
        "stored_flag",
        &[(
            "OPT110",
            V::Fix(&["    seen_set = set(seen)\n", "seen_set"]),
        )],
    ),
    ("write_through_iterator", &[]),
    (
        "rebound_in_outer_loop",
        &[(
            "OPT110",
            V::Fix(&["        allowed_set = set(allowed)\n", "allowed_set"]),
        )],
    ),
    (
        "flag_tested_elsewhere",
        &[(
            "OPT110",
            V::Fix(&["    seen_set = set(seen)\n", "seen_set"]),
        )],
    ),
    // `Disproved` (`OnlyPairedAppends`).
    ("dedup_rebound", &[]),
    ("dedup_other_changes", &[]),
    (
        "dedup_live_alias",
        &[(
            "OPT111",
            V::Blocked(
                "ExclusiveAccessAtWrite(seen): `alias` still refers to the same object and is used later; it could change the list behind the auxiliary set",
            ),
        )],
    ),
    ("dedup_parameter", &[]),
    ("dedup_result_used", &[("OPT111", V::Hand)]),
    ("dedup_while", &[("OPT111", V::Hand)]),
    ("dedup_few", &[]),
    ("dedup_set_append", &[]),
    ("dedup_in_for_header", &[]),
    ("rebuild_list", &[("OPT130", V::Hand)]),
    (
        "rebuild_tuple_of_str",
        &[(
            "OPT130",
            V::Fix(&["    letters_tuple = tuple(letters)\n", "letters_tuple"]),
        )],
    ),
    (
        "rebuild_set_of_set",
        &[(
            "OPT130",
            V::Fix(&["    keys_set = set(keys)\n", "keys_set"]),
        )],
    ),
    (
        "rebuild_set_of_copy",
        &[(
            "OPT130",
            V::Fix(&["    hoisted = set(keys.copy())\n", "hoisted"]),
        )],
    ),
    // Empty or small containers do not pay for hoisting.
    ("rebuild_empty", &[]),
    ("rebuild_small", &[]),
    ("rebuild_keywords", &[]),
    (
        "rebuild_display",
        &[(
            "OPT130",
            V::Blocked(
                "SideEffectFree(set(...)): the expression evaluates more than builtin calls",
            ),
        )],
    ),
    (
        "rebuild_frozenset",
        &[(
            "OPT130",
            V::Blocked(
                "ExactType(frozenset(...)): its exact type is unknown (result of `frozenset`)",
            ),
        )],
    ),
    // The conditional's blocks count as rebound inside the loop: `Disproved`.
    ("rebuild_choice", &[]),
    (
        "rebuild_unknown_call",
        &[(
            "OPT130",
            V::Blocked(
                "SideEffectFree(set(...)): `load(...)` inside it may raise or run user code before the loop",
            ),
        )],
    ),
    // `stack.pop()` changes `stack` on every iteration: `Disproved`.
    ("rebuild_popped", &[]),
    (
        "rebuild_dict_copy",
        &[(
            "OPT130",
            V::Blocked("SideEffectFree(list(...)): `.copy()` may have side effects"),
        )],
    ),
    (
        "rebuild_unknown_elements",
        &[(
            "OPT130",
            V::Blocked("ZeroTripExceptionSafe(set(...)): its elements may be unhashable"),
        )],
    ),
    (
        "rebuild_parameter",
        &[(
            "OPT130",
            V::Blocked(
                "ValueLoopInvariant(keys): it may change during the loop: it is a parameter: the caller may hold a reference",
            ),
        )],
    ),
    ("rebuild_rebound_key", &[]),
    (
        "rebuild_dict",
        &[(
            "OPT130",
            V::Blocked(
                "ZeroTripExceptionSafe(dict(...)): its keys may run their own `__eq__` while the copy is built",
            ),
        )],
    ),
    (
        "rebuild_number",
        &[(
            "OPT130",
            V::Blocked(
                "ZeroTripExceptionSafe(list(...)): the argument may not be a builtin container",
            ),
        )],
    ),
    // A small display of `bytes`; elements no `set` can hold.
    ("bytes_display", &[]),
    ("unhashable_elements", &[]),
    (
        "probes_from_a_list",
        &[(
            "OPT110",
            V::Fix(&["    allowed_set = set(allowed)\n", "allowed_set"]),
        )],
    ),
    (
        "probes_from_either_list",
        &[(
            "OPT110",
            V::Fix(&["    allowed_set = set(allowed)\n", "allowed_set"]),
        )],
    ),
    (
        "probes_measured_first",
        &[(
            "OPT110",
            V::Fix(&["    allowed_set = set(allowed)\n", "allowed_set"]),
        )],
    ),
    (
        "probe_by_index",
        &[(
            "OPT110",
            V::Fix(&["    allowed_set = set(allowed)\n", "allowed_set"]),
        )],
    ),
    // A bounded loop, or a constructor that returns its argument: nothing to gain.
    ("lookup_in_a_bounded_loop", &[]),
    ("rebuild_in_a_bounded_loop", &[]),
    ("rebuild_tuple_of_a_tuple", &[]),
    // A comparison chain shares one span: no edit can name the right operand.
    // Each link of the chain is a comparison of its own: the first one's `allowed` is the
    // second one's operand too (no automatic edit), and the second probes with a list.
    (
        "chained_membership",
        &[
            ("OPT110", V::Hand),
            (
                "OPT110",
                V::Blocked("HashConsistentProbe(allowed): `allowed` may have a custom"),
            ),
        ],
    ),
    // A comprehension's or generator expression's `for` clause is no statement to hoist before.
    (
        "comprehension_clause_on_its_own_line",
        &[("OPT110", V::Hand)],
    ),
    (
        "generator_clause_on_its_own_line.<locals>.<genexpr>",
        &[("OPT130", V::Hand)],
    ),
    (
        "two_hoists_in_nested_loops",
        &[
            (
                "OPT130",
                V::Fix(&["    hoisted = set(\"abcdefghijklmnopq\")\n", "hoisted"]),
            ),
            (
                "OPT130",
                V::Fix(&[
                    "        hoisted_1 = set(\"jklmnopqrstuvwxyz\")\n",
                    "hoisted_1",
                ]),
            ),
        ],
    ),
];

#[test]
fn hoisting_rules() {
    check_fixture(
        "sound_hoist",
        "tests/difftest/soundness_e2e_hoist.py",
        HOIST,
    );
}

// ======================================================================== deques

const NOT_EMPTY: &str = "DequeEquivalentOps(q): `q[1:]` of an empty list is `[]`, but `q.popleft()` raises `IndexError`: only `while q:` right before it proves `q` non-empty";

const DEQUE: &[Expect] = &[
    (
        "drain_del",
        &[("OPT120", V::Fix(&["deque(list(items))", "q.popleft()"]))],
    ),
    (
        "drain_slice",
        &[("OPT120", V::Fix(&["deque(list(items))", "q.popleft()"]))],
    ),
    (
        "drain_slice_after_change",
        &[("OPT120", V::Blocked(NOT_EMPTY))],
    ),
    ("drain_slice_for", &[("OPT120", V::Blocked(NOT_EMPTY))]),
    (
        "fill_front",
        &[("OPT121", V::Fix(&["deque()", "q.appendleft(i)"]))],
    ),
    // The call text is not `q.insert(0, ...)` / `q.pop(0)`: rewritten by hand.
    ("fill_front_parenthesized", &[("OPT121", V::Hand)]),
    ("pop_parenthesized", &[("OPT120", V::Hand)]),
    (
        "copied_queue",
        &[(
            "OPT120",
            V::Blocked("DequeEquivalentOps(q): it is also created at line 107"),
        )],
    ),
    ("parameter_queue", &[]),
    (
        "drain_then_alias",
        &[("OPT120", V::Fix(&["deque(list(items))", "q.popleft()"]))],
    ),
    // The statement text is not `del q[0]` / `q = q[1:]`: rewritten by hand.
    ("drain_del_parenthesized", &[("OPT120", V::Hand)]),
    ("drain_slice_parenthesized", &[("OPT120", V::Hand)]),
    (
        "deque_uses",
        &[("OPT120", V::Fix(&["deque([0])", "q.popleft()"]))],
    ),
    (
        "unknown_list_methods",
        &[(
            "OPT120",
            V::Blocked("Unique(q): it may be shared: passed to code heki cannot see"),
        )],
    ),
    (
        "deque_more_uses",
        &[("OPT121", V::Fix(&["deque([5, 4, 3])", "q.appendleft(i)"]))],
    ),
    (
        "alias_after_site",
        &[("OPT120", V::Fix(&["deque([1, 2, 3])", "q.popleft()"]))],
    ),
    (
        "formatted_items",
        &[("OPT120", V::Fix(&["deque(list(items))", "q.popleft()"]))],
    ),
    (
        "middle_index",
        &[(
            "OPT120",
            V::Blocked("DequeEquivalentOps(q): `q` : this use has no `deque` equivalent"),
        )],
    ),
    (
        "sorted_in_loop",
        &[(
            "OPT120",
            V::Blocked("DequeEquivalentOps(q): `q` : this use has no `deque` equivalent"),
        )],
    ),
    (
        "passed_to_zip",
        &[(
            "OPT120",
            V::Blocked("DequeEquivalentOps(q): `q` : it is passed to `zip(...)`"),
        )],
    ),
    (
        "self_extend",
        &[(
            "OPT120",
            V::Blocked("DequeEquivalentOps(q): `q` : this use has no `deque` equivalent"),
        )],
    ),
    (
        "compared",
        &[(
            "OPT120",
            V::Blocked("DequeEquivalentOps(q): `q` : this use has no `deque` equivalent"),
        )],
    ),
    (
        "pop_middle",
        &[(
            "OPT120",
            V::Blocked("DequeEquivalentOps(q): `q` : `.pop(...)` has no `deque` equivalent here"),
        )],
    ),
    // The queue's text: formatted, converted, or printed by code heki cannot see.
    (
        "formatted_queue",
        &[(
            "OPT120",
            V::Blocked("DequeEquivalentOps(q): `q` : it is formatted (line"),
        )],
    ),
    (
        "stringified_queue",
        &[(
            "OPT120",
            V::Blocked("DequeEquivalentOps(q): `q` : it is passed to `str(...)`"),
        )],
    ),
    (
        "printed_queue",
        &[(
            "OPT120",
            V::Blocked("Unique(q): it may be shared: passed to code heki cannot see"),
        )],
    ),
    // Both fixes wrap the same display: the second waits for another pass, where `q` is no
    // longer a list (`deque.insert(0, v)` stays correct).
    (
        "front_insert_twice",
        &[
            ("OPT121", V::Fix(&["deque([])", "q.appendleft(i)"])),
            ("OPT121", V::Fix(&["deque([])", "q.appendleft(-i)"])),
        ],
    ),
    ("dict_pop", &[]),
    ("dict_del", &[]),
    ("bounded", &[]),
    // A small display the loop only drains, a list rebuilt in every iteration: not profitable.
    ("drained_small", &[]),
    ("rebuilt_per_iteration", &[]),
    (
        "drain_and_test",
        &[("OPT120", V::Fix(&["deque(list(items))", "q.popleft()"]))],
    ),
    (
        "annotated_queue",
        &[(
            "OPT120",
            V::Fix(&["deque(list(items))", "q.popleft()", "deque[int]"]),
        )],
    ),
    (
        "fill_front_with_comment",
        &[(
            "OPT121",
            V::Fix(&[
                "deque()",
                "q.appendleft(i  # keep this comment\n                 )",
            ]),
        )],
    ),
    (
        "drain_then_iterate",
        &[("OPT120", V::Fix(&["deque(range(n))", "q.popleft()"]))],
    ),
    (
        "drain_then_sum_reversed",
        &[(
            "OPT120",
            V::Blocked("DequeEquivalentOps(q): the result of `reversed(...)` (line"),
        )],
    ),
    ("unpacked_queue", &[("OPT120", V::Blocked(UNPACKED_QUEUE))]),
    ("matched_queue", &[("OPT120", V::Blocked(UNPACKED_QUEUE))]),
];

const UNPACKED_QUEUE: &str = "DequeEquivalentOps(q): the list is made by unpacking";

#[test]
fn deque_rules() {
    check_fixture(
        "sound_deque",
        "tests/difftest/soundness_e2e_deque.py",
        DEQUE,
    );
}

// ============================================================ counterexamples found in M8

const PARTLY_UPDATED: &str = "ExceptionStateSafe(update): an insertion that raises would leave";
const REGROUPED: &str = "EvalOrderPreserved(update): ";

const COUNTEREXAMPLES: &[Expect] = &[
    // Default `--fix` (`OPT102`): the display stores the accumulator in the new dict.
    (
        "cx_dict_stores_itself",
        &[(
            "OPT102",
            V::Blocked("Unique(d): it may be shared: stored in a container, attribute, or global"),
        )],
    ),
    (
        "cx_dict_stores_its_iterator",
        &[(
            "OPT102",
            V::Blocked("Unique(d): it may be shared: an iterator or view of it escapes"),
        )],
    ),
    // The `for` iterable rebinds or changes the container after the hoisting point:
    // `Disproved`, not reported.
    ("cx_rebound_in_iterable", &[]),
    ("cx_changed_in_iterable", &[]),
    ("cx_rebuilt_from_rebound", &[]),
    ("cx_dedup_rebound_in_iterable", &[]),
    (
        "cx_nested_builtin_raises",
        &[(
            "OPT130",
            V::Blocked(
                "SideEffectFree(set(...)): `int(...)` inside it may raise or run user code before the loop",
            ),
        )],
    ),
    (
        "cx_nested_builtin_runs_user_code",
        &[(
            "OPT130",
            V::Blocked(
                "SideEffectFree(tuple(...)): `map(...)` inside it may raise or run user code before the loop",
            ),
        )],
    ),
    (
        "old_nested_builtin_runs_user_code",
        &[(
            "OPT110",
            V::Blocked("ZeroTripExceptionSafe(hoisted): element types are unknown"),
        )],
    ),
    (
        "cx_list_of_number",
        &[(
            "OPT130",
            V::Blocked(
                "ZeroTripExceptionSafe(list(...)): the argument may not be a builtin container",
            ),
        )],
    ),
    (
        "old_list_of_number",
        &[(
            "OPT110",
            V::Blocked("ZeroTripExceptionSafe(five_list): element types are unknown"),
        )],
    ),
    (
        "cx_set_of_dict_keys",
        &[(
            "OPT130",
            V::Blocked(
                "ZeroTripExceptionSafe(set(...)): its keys may run their own `__eq__` while the copy is built",
            ),
        )],
    ),
    (
        "cx_local_set",
        &[(
            "OPT110",
            V::Blocked(
                "SideEffectFree(set): `set` is a local name of this function or an enclosing one",
            ),
        )],
    ),
    (
        "cx_local_set_dedup",
        &[(
            "OPT111",
            V::Blocked(
                "SideEffectFree(set): `set` is a local name of this function or an enclosing one",
            ),
        )],
    ),
    // Proven safe, but `collections.deque` would name the parameter: no automatic edit.
    ("cx_local_collections", &[("OPT120", V::Hand)]),
    // The same through a class: the enclosing function is found by its span, not its qualname.
    (
        "cx_class_in_function_collections.<locals>.LocalCx.drain",
        &[("OPT120", V::Hand)],
    ),
    (
        "cx_two_pops",
        &[
            (
                "OPT120",
                V::Blocked(
                    "DequeEquivalentOps(q): `q` : `.pop(...)` has no `deque` equivalent here",
                ),
            ),
            (
                "OPT120",
                V::Blocked(
                    "DequeEquivalentOps(q): `q` : `.pop(...)` has no `deque` equivalent here",
                ),
            ),
        ],
    ),
    (
        "cx_iterated_while_popped",
        &[(
            "OPT120",
            V::Blocked(
                "DequeEquivalentOps(q): the loop's iterator still iterates it when it changes",
            ),
        )],
    ),
    ("cx_slice_of_empty", &[("OPT120", V::Blocked(NOT_EMPTY))]),
    // `pop`'s default and `map`'s function stay live in the result: `Disproved`.
    ("cx_kept_by_pop_default", &[]),
    (
        "cx_compared_by_remove",
        &[(
            "OPT101",
            V::Blocked(
                "Unique(acc): it may be shared: stored in a container, attribute, or global",
            ),
        )],
    ),
    ("cx_function_kept_by_map", &[]),
    (
        "cx_map_over_ints",
        &[(
            "OPT110",
            V::Blocked("HashConsistentProbe(p): `p` may have a custom `__eq__`"),
        )],
    ),
    (
        "cx_generator_over_ints",
        &[(
            "OPT110",
            V::Blocked("HashConsistentProbe(p): `p` may have a custom `__eq__`"),
        )],
    ),
    (
        "cx_ints_or_parameter",
        &[(
            "OPT110",
            V::Blocked("HashConsistentProbe(p): `p` may have a custom `__eq__`"),
        )],
    ),
    (
        "cx_range_or_parameter",
        &[(
            "OPT110",
            V::Blocked("HashConsistentProbe(p): `p` may have a custom `__eq__`"),
        )],
    ),
    (
        "cx_tuple_or_element",
        &[(
            "OPT130",
            V::Blocked("ZeroTripExceptionSafe(set(...)): its elements may be unhashable"),
        )],
    ),
    (
        "cx_elements_after_escape",
        &[(
            "OPT110",
            V::Blocked("HashConsistentProbe(p): `p` may have a custom `__eq__`"),
        )],
    ),
    (
        "cx_list_from_earlier_iteration",
        &[(
            "OPT110",
            V::Blocked("HashConsistentProbe(p): `p` may have a custom `__eq__`"),
        )],
    ),
    (
        "cx_subscript_with_slice_object",
        &[(
            "OPT110",
            V::Blocked("HashConsistentProbe(p): `p` may have a custom `__eq__`"),
        )],
    ),
    (
        "cx_partly_updated_in_handler",
        &[("OPT102", V::Blocked(PARTLY_UPDATED))],
    ),
    (
        "cx_set_partly_updated_in_handler",
        &[("OPT102", V::Blocked(PARTLY_UPDATED))],
    ),
    (
        "cx_later_entry_before_collision",
        &[("OPT102", V::Blocked(REGROUPED))],
    ),
    (
        "cx_entry_reads_what_eq_changes",
        &[("OPT102", V::Blocked(REGROUPED))],
    ),
    (
        "cx_rebuilt_container_changed",
        &[(
            "OPT130",
            V::Blocked("ValueLoopInvariant(list(...)): the built container is also bound or used"),
        )],
    ),
    // `list(*args)`: the call's Layer 1 positions do not apply, so nothing is known of it.
    (
        "cx_rebuild_with_unpacked_argument",
        &[(
            "OPT130",
            V::Blocked("ExactType(list(...)): its exact type is unknown (result of `list`)"),
        )],
    ),
    (
        "cx_rebuild_copy_with_argument",
        &[(
            "OPT130",
            V::Blocked(
                "SideEffectFree(set(...)): the expression evaluates more than builtin calls",
            ),
        )],
    ),
    (
        "cx_set_declared_without_value",
        &[(
            "OPT110",
            V::Blocked(
                "SideEffectFree(set): `set` is a local name of this function or an enclosing one",
            ),
        )],
    ),
    (
        "cx_deque_keyword_arguments",
        &[(
            "OPT121",
            V::Blocked("DequeEquivalentOps(q): `list(...)` with `*` / `**` arguments"),
        )],
    ),
    (
        "cx_extended_by_its_own_iterator",
        &[(
            "OPT120",
            V::Blocked(
                "DequeEquivalentOps(q): the result of `reversed(...)` still iterates it when it changes",
            ),
        )],
    ),
    (
        "cx_iterator_type_observed",
        &[(
            "OPT120",
            V::Blocked("DequeEquivalentOps(q): the result of `iter(...)` (line"),
        )],
    ),
    // `q[1::2]` is no front pop: not a candidate.
    ("cx_stepped_slice", &[]),
    (
        "cx_iterator_returned",
        &[(
            "OPT120",
            V::Blocked("DequeEquivalentOps(q): the result of `iter(...)` (line"),
        )],
    ),
];

#[test]
fn counterexamples_found_in_m8_stay_unrewritten() {
    check_fixture(
        "sound_cx",
        "tests/difftest/soundness_e2e_counterexamples.py",
        COUNTEREXAMPLES,
    );
    let p = Project::new("sound_cx_unchanged");
    let original = repo_file("tests/difftest/soundness_e2e_counterexamples.py");
    p.write("cases.py", &original);
    let (_, _, stderr) = p.check(&["--select", "OPT", "--fix", "--include-size-dependent"]);
    assert!(stderr.contains("Fixed 0 issue(s)"), "{stderr}");
    assert_eq!(p.read("cases.py"), original);
    assert!(run_python(&p.dir, "cases.py").contains("old rewrites diverge: 41/41"));
}

// ======================================================================== module-level facts

/// One project's verdicts and, when `runs`, its first file's CPython output before and after
/// `--fix --include-size-dependent`.
fn check_module(name: &str, files: &[(&str, &str)], expect: &[Expect], runs: bool) -> Project {
    let p = Project::new(name);
    for (path, text) in files {
        p.write(path, text);
    }
    assert_verdicts(&opt_json(&p, &[]), expect);
    if runs {
        let before = run_python(&p.dir, files[0].0);
        let (_, _, stderr) = p.check(&["--select", "OPT", "--fix", "--include-size-dependent"]);
        assert!(stderr.contains("Fixed "), "{stderr}");
        assert_eq!(
            before,
            run_python(&p.dir, files[0].0),
            "{}",
            p.read(files[0].0)
        );
    }
    p
}

const DRAIN: &str = "def drain(n):\n    q = [1, 2, 3, *range(n)]\n    out = []\n    while q:\n        out.append(q.pop(0))\n    return out[:n]\n";

const MEMBERSHIP: &str = "def member(n):\n    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17]\n    return [p for p in [3, 6, 9, 12, 15, 18, 21, 24, 27, 30, 33, 36, 39, 42, 45, 48, 51] if p in allowed]\n\n\ndef dedup(n):\n    seen = []\n    for v in [1, 2, 2, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 5, 6, 6]:\n        if v not in seen:\n            seen.append(v)\n    return seen\n\n\ndef drain_list(n):\n    q = list([1, 2, 3])\n    out = []\n    while q:\n        out.append(q.pop(0))\n    return out\n";

const SET_REBOUND_110: &[(&str, V)] = &[(
    "OPT110",
    V::Blocked("SideEffectFree(set): `set` is rebound in this module"),
)];

const SET_REBOUND_111: &[(&str, V)] = &[(
    "OPT111",
    V::Blocked("SideEffectFree(set): `set` is rebound in this module"),
)];

#[test]
fn dynamic_module_namespaces_distrust_builtins_and_imports() {
    let expect: &[Expect] = &[
        ("member", SET_REBOUND_110),
        ("dedup", SET_REBOUND_111),
        // `list` is not trusted either: `list([...])` is not a list heki knows.
        ("drain_list", &[]),
        // The list is still proven safe, but `collections.deque` / `deque` may name something
        // else by the time the function runs.
        ("drain", &[("OPT120", V::Hand)]),
    ];
    // A module-level `exec` (an A1 hazard of the module body), a star import, `globals()`.
    for (i, head) in [
        "import collections\n\nexec(\"collections = None\")\n",
        "from collections import deque\nfrom helpers import *\n",
        "import collections\n\n\ndef rebind():\n    globals()[\"collections\"] = None\n",
    ]
    .into_iter()
    .enumerate()
    {
        let src = format!(
            "{head}\n\n{MEMBERSHIP}\n\n{DRAIN}\n\nprint(member(3), dedup(3), drain_list(3), drain(3))\n"
        );
        check_module(
            &format!("sound_dynamic_{i}"),
            &[("cases.py", &src), ("helpers.py", "deque = list\n")],
            expect,
            false,
        );
    }
}

#[test]
fn a_module_importing_builtins_shadows_them_project_wide() {
    let expect: &[Expect] = &[
        ("member", SET_REBOUND_110),
        ("dedup", SET_REBOUND_111),
        ("drain_list", &[]),
    ];
    check_module(
        "sound_patch_builtins",
        &[
            ("cases.py", MEMBERSHIP),
            (
                "patcher.py",
                "import builtins\n\nbuiltins.set = frozenset\n",
            ),
        ],
        expect,
        false,
    );
}

#[test]
fn deque_spellings_and_import_insertion() {
    // `import collections` is reused; a module without it gets one before its first import.
    for (i, head) in [
        "import collections\n",
        "\"\"\"Doc.\"\"\"\nfrom __future__ import annotations\n\nimport os\n",
    ]
    .into_iter()
    .enumerate()
    {
        let src = format!("{head}\n\n{DRAIN}\n\nprint(drain(3))\n");
        let inserted: &[&str] = &[
            "import collections\n",
            "collections.deque([1, 2, 3, *range(n)])",
            "q.popleft()",
        ];
        let reps = if i == 0 { &inserted[1..] } else { inserted };
        let p = Project::new(&format!("sound_spelling_{i}"));
        p.write("cases.py", &src);
        let diags = opt_json(&p, &[]);
        assert_eq!(diags.len(), 1, "{diags:#?}");
        assert_eq!(diags[0]["rule_code"], "OPT120", "{diags:#?}");
        assert_eq!(replacements(&diags[0]), reps, "{diags:#?}");
        let before = run_python(&p.dir, "cases.py");
        p.check(&["--select", "OPT", "--fix", "--include-size-dependent"]);
        assert_eq!(
            before,
            run_python(&p.dir, "cases.py"),
            "{}",
            p.read("cases.py")
        );
    }
    // Module code before the first import would run before an import inserted next to it, so
    // the import goes above that code, as in a module without imports (with the blank lines an
    // import block needs before a `def`).
    const TOP: &[&str] = &[
        "import collections\n\n\n",
        "collections.deque([1, 2, 3, *range(n)])",
        "q.popleft()",
    ];
    let early = format!("{DRAIN}\n\nFIRST = drain(3)\n\nimport os\n\nprint(FIRST, os.sep)\n");
    check_module(
        "sound_early_call",
        &[("cases.py", &early)],
        &[("drain", &[("OPT120", V::Fix(TOP))])],
        true,
    );
    check_module(
        "sound_no_import",
        &[("cases.py", &format!("{DRAIN}\n\nprint(drain(3))\n"))],
        &[("drain", &[("OPT120", V::Fix(TOP))])],
        true,
    );
}

#[test]
fn deque_import_reuse_and_placement() {
    const INSERTED: &[&str] = &[
        "import collections\n",
        "collections.deque([1, 2, 3, *range(n)])",
        "q.popleft()",
    ];
    // (module head, the fix's replacements, whether CPython can run the module)
    let cases: &[(&str, &[&str], bool)] = &[
        // An unconditional absolute import is reused.
        (
            "from collections import deque\n",
            &["deque([1, 2, 3, *range(n)])", "q.popleft()"],
            true,
        ),
        // A name bound only for type checkers is not there at run time.
        (
            "from typing import TYPE_CHECKING\n\nif TYPE_CHECKING:\n    from collections import deque\n",
            INSERTED,
            true,
        ),
        // `from .collections import deque` is another module's `deque`.
        (
            "from .collections import deque\n",
            &[
                "import collections\n\n",
                "collections.deque([1, 2, 3, *range(n)])",
                "q.popleft()",
            ],
            false,
        ),
        // `__future__` imports stay first, the shebang and the encoding line too.
        (
            "from __future__ import annotations\nfrom math import pi\n",
            INSERTED,
            true,
        ),
        ("#!/usr/bin/env python3\nimport sys\n", INSERTED, true),
        ("# -*- coding: utf-8 -*-\nimport os\n", INSERTED, true),
        // A non-ASCII name next to an import heki plans around.
        ("import os as caf\u{e9}\n", INSERTED, true),
    ];
    for (i, (head, reps, runs)) in cases.iter().enumerate() {
        let src = format!("{head}\n\n{DRAIN}\n\nprint(drain(3))\n");
        let p = Project::new(&format!("sound_import_{i}"));
        p.write("cases.py", &src);
        let diags = opt_json(&p, &[]);
        assert_eq!(diags.len(), 1, "{head}: {diags:#?}");
        assert_eq!(replacements(&diags[0]), *reps, "{head}: {diags:#?}");
        if *runs {
            let before = run_python(&p.dir, "cases.py");
            p.check(&["--select", "OPT", "--fix", "--include-size-dependent"]);
            let fixed = p.read("cases.py");
            assert_eq!(before, run_python(&p.dir, "cases.py"), "{fixed}");
            let first = head.lines().next().unwrap();
            let must_stay_first = first.starts_with('#') || first.starts_with("from __future__");
            assert!(!must_stay_first || fixed.starts_with(first), "{fixed}");
        }
    }
}

#[test]
fn frame_introspection_blocks_the_function() {
    let src = "def f(items):\n    x = []\n    for i in items:\n        x = x + [i]\n    eval('1')\n    return x\n";
    check_module(
        "sound_a1",
        &[("cases.py", src)],
        &[(
            "f",
            &[(
                "OPT101",
                V::Blocked("A1HazardFree: A1 hazard: `eval` (line 5) can read or rebind locals"),
            )],
        )],
        false,
    );
}

/// `def shift(items)` with a shift register of `stages` variables: a list created in the loop
/// reaches `v1` only after one fixpoint pass per stage.
fn shift_register(stages: usize) -> String {
    let mut src = String::from("def shift(items):\n    x = []\n");
    for k in 1..=stages {
        src.push_str(&format!("    v{k} = None\n"));
    }
    src.push_str("    for i in items:\n");
    for k in 1..stages {
        src.push_str(&format!("        v{k} = v{}\n", k + 1));
    }
    src.push_str(&format!(
        "        v{stages} = [i]\n        x = x + [i]\n    return x, v1\n\n\nprint(shift(range(3)))\n"
    ));
    src
}

#[test]
fn a_fixpoint_not_reached_in_64_passes_blocks_every_candidate() {
    check_module(
        "sound_no_fixpoint",
        &[("cases.py", &shift_register(70))],
        &[(
            "shift",
            &[(
                "OPT101",
                V::Blocked("AnalysisConverged: the abstract interpretation did not converge"),
            )],
        )],
        false,
    );
    // A shorter register converges, and the rewrite is proven safe.
    check_module(
        "sound_fixpoint",
        &[("cases.py", &shift_register(20))],
        &[("shift", &[("OPT101", V::Fix(&[ONE]))])],
        true,
    );
}

// ================================================ frames, bindings, and paths the IR models

const STORED: &str = "Unique(xs): it may be shared: stored in a container, attribute, or global";

const FORMATTED: &str = "DequeEquivalentOps(q): `q` : it is formatted (line";

const IR_COUNTEREXAMPLES: &[Expect] = &[
    // A replacement field may rebind a builtin name or store the accumulator.
    ("cx_walrus_in_fstring", &[]),
    ("cx_nested_format_spec", &[("OPT101", V::Blocked(STORED))]),
    // A replacement field, also of a format specification, formats the list: a deque's text
    // differs.
    ("cx_formatted_queue", &[("OPT120", V::Blocked(FORMATTED))]),
    (
        "cx_queue_in_format_spec",
        &[("OPT120", V::Blocked(FORMATTED))],
    ),
    (
        "cx_queue_in_template_spec",
        &[("OPT120", V::Blocked(FORMATTED))],
    ),
    // A definition's header, a handler's type, and a target's subscript bind in the function:
    // `list` is a local name there, not the builtin.
    ("cx_walrus_in_default", &[]),
    ("cx_walrus_in_lambda_default", &[]),
    ("cx_walrus_in_decorator", &[]),
    ("cx_walrus_in_class_base", &[]),
    ("cx_walrus_in_handler_type", &[]),
    ("cx_walrus_in_subscript_target", &[]),
    // An assignment expression in a generator expression rebinds the function's cell.
    ("cx_walrus_in_generator_expression", CAPTURED),
    ("cx_walrus_in_nested_generator_expressions", CAPTURED),
    // Only a scope nested in the child reads the cell.
    ("cx_capture_through_function", CAPTURED),
    ("cx_capture_through_class", CAPTURED),
    ("cx_capture_through_generator_expression", CAPTURED),
    // A function keeps its defaults and annotations (evaluated eagerly before 3.14, closed
    // over by `__annotate__` from 3.14), a type parameter's bound or a type alias's value
    // closes over the names it reads, and a class hands its keywords to `__init_subclass__`.
    ("cx_default_keeps_value", CAPTURED),
    ("cx_keyword_only_default_keeps_value", CAPTURED),
    ("cx_lambda_default_keeps_value", CAPTURED),
    ("cx_return_annotation_keeps_value", CAPTURED),
    ("cx_parameter_annotation_keeps_value", CAPTURED),
    ("cx_class_annotation_keeps_value", CAPTURED),
    ("cx_type_parameter_bound_keeps_value", CAPTURED),
    ("cx_type_alias_keeps_value", CAPTURED),
    ("cx_class_keyword_reaches_user_code", CAPTURED),
    // `3 < 2` stops the chain before the assignment expression runs.
    ("cx_chained_comparison_short_circuits", &[]),
    // The middle `keys` is also the second comparison's operand: no edit may replace it.
    (
        "cx_membership_in_chained_comparison",
        &[
            ("OPT110", V::Hand),
            (
                "OPT110",
                V::Blocked("HashConsistentProbe(keys): `keys` may have a custom"),
            ),
        ],
    ),
    (
        "old_membership_in_chained_comparison",
        &[("OPT110", V::Hand)],
    ),
    // A capture binds an item of the subject, which may be the caller's list.
    ("cx_sequence_pattern_capture", &[]),
    ("cx_as_pattern_capture", &[]),
    ("cx_mapping_pattern_capture", &[]),
    // `echo[acc:]` hands `slice(acc, None, None)` to `__getitem__`; a template keeps `acc`.
    (
        "cx_slice_bound_reaches_getitem",
        &[(
            "OPT101",
            V::Blocked(
                "Unique(acc): it may be shared: stored in a container, attribute, or global",
            ),
        )],
    ),
    (
        "cx_template_keeps_value",
        &[(
            "OPT101",
            V::Blocked(
                "Unique(acc): it may be shared: stored in a container, attribute, or global",
            ),
        )],
    ),
    // `locals` / `eval` reached through an alias or a call of `map` run in this frame.
    (
        "cx_locals_through_alias",
        &[("OPT101", V::Blocked("A1HazardFree: A1 hazard: `locals`"))],
    ),
    (
        "cx_eval_called_by_map",
        &[("OPT101", V::Blocked("A1HazardFree: A1 hazard: `eval`"))],
    ),
    // `keys` may be unbound where the hoisted statement would run.
    ("cx_deleted_on_one_branch", &[]),
];

const CAPTURED: &[(&str, V)] = &[(
    "OPT101",
    V::Blocked("Unique(acc): it may be shared: its name is captured by a nested function"),
)];

#[test]
fn ir_counterexamples_stay_unrewritten() {
    check_fixture_unchanged(
        "sound_ir_cx",
        "tests/difftest/ir_counterexamples.py",
        IR_COUNTEREXAMPLES,
    );
}

/// The originals of a counterexample fixture: the verdicts of `expect`, no edit under any
/// `--fix`, and CPython confirms that each old rewrite diverged.
fn check_fixture_unchanged(name: &str, fixture: &str, expect: &[Expect]) {
    let p = Project::new(name);
    let original = repo_file(fixture);
    p.write("cases.py", &original);
    // Before 3.14 annotations are evaluated eagerly, from 3.14 lazily: both keep the object.
    for version in ["py313", "py315"] {
        let args = ["--include-size-dependent", "--target-version", version];
        assert_verdicts(&opt_json(&p, &args), expect);
    }
    let (_, _, stderr) = p.check(&["--select", "OPT", "--fix", "--include-size-dependent"]);
    assert!(stderr.contains("Fixed 0 issue(s)"), "{stderr}");
    assert_eq!(p.read("cases.py"), original);
    let transcript = run_python(&p.dir, "cases.py");
    let total = transcript.lines().count() - 1;
    assert!(
        transcript.contains(&format!("old rewrites diverge: {total}/{total}")),
        "{transcript}"
    );
}

#[test]
fn string_annotations_keep_nothing() {
    // Under `from __future__ import annotations` the function keeps the annotation's text.
    let src = "from __future__ import annotations\n\n\ndef tagged(items):\n    acc = []\n    def tag() -> acc:\n        pass\n    old = tag.__annotations__[\"return\"]\n    for x in items:\n        acc = acc + [x]\n    return old, acc\n\n\nprint(tagged([1]))\n";
    check_module(
        "sound_string_annotations",
        &[("cases.py", src)],
        &[("tagged", &[("OPT101", V::Fix(&["acc.append(x)"]))])],
        true,
    );
}

#[test]
fn the_last_operand_of_a_chained_comparison_is_its_own() {
    // `i in keys == keys` would become `i in keys_set == keys`, which changes the second
    // comparison; the last operand of `0 < i in keys` belongs to one comparison only.
    let src = "def shared(n):\n    keys = list(range(17))\n    hit = None\n    for i in range(n):\n        hit = i in keys == keys\n    return hit\n\n\ndef last(n):\n    keys = list(range(17))\n    hit = None\n    for i in range(n):\n        hit = 0 < i in keys\n    return hit\n\n\nprint(shared(1), last(2))\n";
    check_module(
        "sound_chain_operands",
        &[("cases.py", src)],
        &[
            ("shared", &[("OPT110", V::Hand)]),
            (
                "last",
                &[(
                    "OPT110",
                    V::Fix(&["    keys_set = set(keys)\n", "keys_set"]),
                )],
            ),
        ],
        true,
    );
}

#[test]
fn a_finally_body_is_rewritten_only_if_every_way_through_it_is_safe() {
    // The `finally` loop runs after the early `return` with `acc` the caller's list, and after
    // normal completion with a fresh one: rewriting it is unsafe. Without an alias on any path
    // it is safe.
    let src = "def finally_after_return(n, early, shared):\n    acc = []\n    try:\n        if early:\n            acc = shared\n            return None\n    finally:\n        for x in range(n):\n            acc = acc + [x]\n    return len(acc)\n\n\ndef finally_all_safe(n, early):\n    acc = []\n    try:\n        if early:\n            return None\n    finally:\n        for x in range(n):\n            acc = acc + [x]\n    return len(acc)\n\n\ns = []\nfinally_after_return(1, True, s)\nprint(s, finally_all_safe(2, True), finally_all_safe(2, False))\n";
    check_module(
        "sound_finally_copies",
        &[("cases.py", src)],
        &[(
            "finally_all_safe",
            &[("OPT101", V::Fix(&["acc.append(x)"]))],
        )],
        true,
    );
}
