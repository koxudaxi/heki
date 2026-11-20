//! `heki_soundness` gates (ARCHITECTURE.md section 10):
//! 1. the seven counterexamples are `Blocked` / `Disproved` (never `Safe`);
//! 2. obligation stripping: disabling one obligation lets exactly its counterexample through;
//! 3. the `OPT101` / `OPT102` positives are `Safe` + `Unconditional` with byte-exact edits
//!    (their CPython equivalence is checked end to end by `crates/heki/tests/cli_fix.rs`).

use super::*;
use ObligationKind as K;
use heki_ast::parse_module;
use heki_env::{PythonVersion, RuntimeProfile};
use heki_kb::EffectiveSoundnessKb;

const COUNTEREXAMPLES: &str = include_str!("../../../tests/difftest/counterexamples.py");
const REWRITES: &str = include_str!("../../../tests/difftest/opt_rewrites.py");

fn ir_of(src: &str) -> heki_ir::ModuleIr {
    let parsed = parse_module(src).expect("valid python");
    let profile = RuntimeProfile::from(PythonVersion::PY315);
    let semantic = heki_semantic::analyze_module(&parsed, src, profile);
    heki_ir::build_module_ir(&parsed, src, &semantic, profile)
}

fn analyze_with(src: &str, disabled: &[ObligationKind]) -> Vec<OptCandidate> {
    let kb = EffectiveSoundnessKb::load_builtin(PythonVersion::PY315).unwrap();
    let ir = ir_of(src);
    analyze_module(
        &ir,
        src,
        &kb,
        &AnalysisOptions {
            project_patches_builtins: false,
            disabled_obligations: disabled.to_vec(),
        },
    )
}

fn analyze(src: &str) -> Vec<OptCandidate> {
    analyze_with(src, &[])
}

fn of<'a>(cands: &'a [OptCandidate], function: &str) -> Vec<&'a OptCandidate> {
    cands.iter().filter(|c| c.function == function).collect()
}

fn status(c: &OptCandidate, kind: ObligationKind) -> &ObligationStatus {
    &c.verdict
        .get(kind)
        .unwrap_or_else(|| panic!("{} has no {kind}: {:#?}", c.function, c.verdict))
        .status
}

fn is_disproved(s: &ObligationStatus) -> bool {
    matches!(s, ObligationStatus::Disproved { .. })
}

fn is_blocked(s: &ObligationStatus) -> bool {
    matches!(s, ObligationStatus::Blocked { .. })
}

/// Apply non-overlapping edits back to front.
fn apply(src: &str, edits: &[TextEdit]) -> String {
    let mut edits = edits.to_vec();
    edits.sort_by_key(|e| std::cmp::Reverse((e.span.start, e.span.end)));
    let mut out = src.to_owned();
    for e in edits {
        out.replace_range(e.span.start as usize..e.span.end as usize, &e.replacement);
    }
    out
}

fn function_text<'a>(src: &'a str, name: &str) -> &'a str {
    let start = src.find(&format!("def {name}(")).unwrap();
    let rest = &src[start..];
    let end = rest[1..]
        .find("\ndef ")
        .or_else(|| rest[1..].find("\nclass "))
        .map_or(rest.len(), |e| e + 1);
    &rest[..end]
}

// ------------------------------------------------------------------ gate 1: counterexamples

#[test]
fn counterexamples_py_yields_no_safe_candidate() {
    let cands = analyze(COUNTEREXAMPLES);
    let safe: Vec<_> = cands.iter().filter(|c| c.is_safe()).collect();
    assert!(safe.is_empty(), "{safe:#?}");
    // The counterexamples whose original shape already sits in a loop are candidates there.
    let expect = [
        (
            "test_s23_for_iter_mutation_infinite_loop",
            OptRule::Opt101,
            K::ExclusiveAccessAtWrite,
        ),
        (
            "test_s26_dict_keys_view_runtime_error",
            OptRule::Opt102,
            K::ExclusiveAccessAtWrite,
        ),
        (
            "test_dcg_right_side_dict_unpack_precedence",
            OptRule::Opt102,
            K::DictUnpackLeading,
        ),
        (
            "test_s27_loop_target_mutated_inside_loop",
            OptRule::Opt110,
            K::ValueLoopInvariant,
        ),
    ];
    for (f, rule, kind) in expect {
        let cs = of(&cands, f);
        let c = cs
            .iter()
            .find(|c| c.rule == rule)
            .unwrap_or_else(|| panic!("{f}: {cs:#?}"));
        assert_eq!(
            c.verdict.tier,
            SafetyTier::Disproved,
            "{f}: {:#?}",
            c.verdict
        );
        assert!(is_disproved(status(c, kind)), "{f}: {:#?}", c.verdict);
    }
    let s22 = of(&cands, "test_s22_zero_trip_loop_unhashable_exception");
    assert!(!s22.is_empty());
    assert!(s22.iter().all(|c| c.verdict.tier == SafetyTier::Blocked
        && is_blocked(status(c, K::ZeroTripExceptionSafe))));
}

/// The seven counterexamples (S23, S24, S26, dcg `{**other, **d}`, S27, S9b, S22) in the shape
/// of the rule they attack.
fn rule_shaped() -> [(&'static str, OptRule, SafetyTier, ObligationKind); 7] {
    [
        (
            "cx_s23",
            OptRule::Opt101,
            SafetyTier::Disproved,
            K::ExclusiveAccessAtWrite,
        ),
        (
            "cx_s24",
            OptRule::Opt101,
            SafetyTier::Disproved,
            K::ExclusiveAccessAtWrite,
        ),
        (
            "cx_s26",
            OptRule::Opt102,
            SafetyTier::Disproved,
            K::ExclusiveAccessAtWrite,
        ),
        (
            "cx_dcg",
            OptRule::Opt102,
            SafetyTier::Disproved,
            K::DictUnpackLeading,
        ),
        (
            "cx_s27",
            OptRule::Opt110,
            SafetyTier::Disproved,
            K::ValueLoopInvariant,
        ),
        ("cx_s9b", OptRule::Opt110, SafetyTier::Blocked, K::ExactType),
        (
            "cx_s22",
            OptRule::Opt110,
            SafetyTier::Blocked,
            K::ZeroTripExceptionSafe,
        ),
    ]
}

#[test]
fn seven_counterexamples_are_blocked_or_disproved() {
    let cands = analyze(REWRITES);
    for (f, rule, tier, kind) in rule_shaped() {
        let cs = of(&cands, f);
        assert!(!cs.is_empty(), "{f}: no candidate");
        for c in &cs {
            assert_eq!(c.rule, rule, "{f}");
            assert_eq!(c.verdict.tier, tier, "{f}: {:#?}", c.verdict);
            let s = status(c, kind);
            assert!(
                if tier == SafetyTier::Disproved {
                    is_disproved(s)
                } else {
                    is_blocked(s)
                },
                "{f}: {kind} {s:?}"
            );
            assert!(c.fix().is_none(), "{f}");
        }
    }
    // S9b: the `list[int]` annotation is never trusted.
    let s9b = of(&cands, "cx_s9b")[0];
    let ObligationStatus::Blocked { blocker } = status(s9b, K::ExactType) else {
        unreachable!()
    };
    assert!(blocker.contains("WeirdList"), "{blocker}");
    // Neither the further unsafe shapes nor the naive rewrites get a safe candidate.
    for c in &cands {
        if c.function.starts_with("neg_") || c.function.starts_with("naive_") {
            assert!(!c.is_safe(), "{c:#?}");
        }
    }
}

// --------------------------------------------------------------- gate 2: obligation stripping

#[test]
fn stripping_each_obligation_lets_its_counterexample_through() {
    let cases: [(ObligationKind, &[&str]); 4] = [
        (K::ExclusiveAccessAtWrite, &["cx_s23", "cx_s24", "cx_s26"]),
        (K::ValueLoopInvariant, &["cx_s27"]),
        (K::DictUnpackLeading, &["cx_dcg"]),
        (K::ZeroTripExceptionSafe, &["cx_s22"]),
    ];
    let baseline = analyze(REWRITES);
    for (kind, functions) in cases {
        let stripped = analyze_with(REWRITES, &[kind]);
        for f in functions {
            assert!(of(&baseline, f).iter().all(|c| !c.is_safe()), "{f}");
            // The baseline fails on exactly this obligation: its counterexample when one is
            // known (`Disproved`), otherwise its first blocker.
            for c in of(&baseline, f) {
                let failing = c
                    .verdict
                    .counterexample()
                    .or_else(|| c.verdict.blocker())
                    .unwrap_or_else(|| panic!("{f}: {c:#?}"));
                assert_eq!(failing.kind, kind, "{f}: {}", failing.describe());
                assert!(
                    failing.describe().starts_with(&format!("{kind}(")),
                    "{}",
                    failing.describe()
                );
            }
            let cs = of(&stripped, f);
            assert!(!cs.is_empty(), "{f}");
            assert!(
                cs.iter().all(|c| c.is_safe()),
                "{kind} stripped, {f} must pass: {cs:#?}"
            );
            // Stripped, the obligation is recorded as established by the stripping itself.
            for c in &cs {
                assert!(c.verdict.counterexample().is_none(), "{f}");
                let ob = c.verdict.get(kind).unwrap();
                assert!(
                    ob.describe()
                        .ends_with(": disabled for an obligation-stripping test"),
                    "{}",
                    ob.describe()
                );
            }
        }
        // Only the counterexamples that this obligation guards pass.
        for (f, ..) in rule_shaped() {
            if !functions.contains(&f) {
                assert!(of(&stripped, f).iter().all(|c| !c.is_safe()), "{kind}: {f}");
            }
        }
    }
}

// ------------------------------------------------------------------- gate 3: positives

#[test]
fn opt101_opt102_positives_are_safe_unconditional_with_exact_edits() {
    let cands = analyze(REWRITES);
    let expect = [
        ("pos_append", OptRule::Opt101, "x = x + [i]", "x.append(i)"),
        (
            "pos_extend_pairs",
            OptRule::Opt101,
            "out = out + [i, i * 2]",
            "out.extend([i, i * 2])",
        ),
        (
            "pos_conditional",
            OptRule::Opt101,
            "acc = acc + [i]",
            "acc.append(i)",
        ),
        (
            "pos_nested",
            OptRule::Opt101,
            "flat = flat + [v]",
            "flat.append(v)",
        ),
        (
            "pos_fresh_per_outer",
            OptRule::Opt101,
            "cur = cur + [v * 10]",
            "cur.append(v * 10)",
        ),
        ("pos_while", OptRule::Opt101, "x = x + [i]", "x.append(i)"),
        (
            "pos_list_call",
            OptRule::Opt101,
            "x = x + [i]",
            "x.append(i)",
        ),
        (
            "pos_try_handler",
            OptRule::Opt101,
            "x = x + [10 // i]",
            "x.append(10 // i)",
        ),
        (
            "pos_generator",
            OptRule::Opt101,
            "x = x + [str(i)]",
            "x.append(str(i))",
        ),
        (
            "pos_dict_kv",
            OptRule::Opt102,
            "d = {**d, k: v}",
            "d[k] = v",
        ),
        (
            "pos_dict_multi",
            OptRule::Opt102,
            "d = {**d, \"key\": k, \"value\": v}",
            "d.update({\"key\": k, \"value\": v})",
        ),
        (
            "pos_dict_merge",
            OptRule::Opt102,
            "merged = {**merged, **part}",
            "merged.update(part)",
        ),
        (
            "pos_dict_effects",
            OptRule::Opt102,
            "d = {**d, key_of(i): val_of(i)}",
            "d.update({key_of(i): val_of(i)})",
        ),
        (
            "pos_set_union",
            OptRule::Opt102,
            "seen = seen | {g}",
            "seen |= {g}",
        ),
    ];
    for (f, rule, before, after) in expect {
        let cs = of(&cands, f);
        let c = cs
            .iter()
            .find(|c| c.rule == rule && c.is_safe())
            .unwrap_or_else(|| panic!("{f}: {cs:#?}"));
        assert_eq!(c.profitability, ProfitabilityTier::Unconditional, "{f}");
        assert!(c.is_reportable(), "{f}");
        let edits = c.fix().unwrap_or_else(|| panic!("{f}: no fix"));
        assert_eq!(edits.len(), 1, "{f}");
        assert_eq!(
            &REWRITES[edits[0].span.start as usize..edits[0].span.end as usize],
            before,
            "{f}"
        );
        assert_eq!(edits[0].replacement, after, "{f}");
        // Only the accumulating statements of a positive are candidates.
        assert!(cs.iter().all(|c| c.is_safe()), "{f}: {cs:#?}");
    }
}

#[test]
fn size_dependent_positives_are_safe_with_hoisting_edits() {
    let cands = analyze(REWRITES);
    let expect = [
        (
            "sd_membership",
            OptRule::Opt110,
            "allowed_set = set(allowed)",
            "if p in allowed_set:",
        ),
        (
            "sd_rebuild",
            OptRule::Opt130,
            "keys_set = set(keys)",
            "if p in keys_set:",
        ),
        (
            "sd_dedup",
            OptRule::Opt111,
            "seen_set = set(seen)",
            "seen.append(w); seen_set.add(w)",
        ),
        (
            "sd_queue",
            OptRule::Opt120,
            "q = collections.deque([0])",
            "cur = q.popleft()",
        ),
        (
            "sd_front_insert",
            OptRule::Opt121,
            "q = collections.deque([])",
            "q.appendleft(i)",
        ),
    ];
    for (f, rule, line_a, line_b) in expect {
        let cs = of(&cands, f);
        let c = cs
            .iter()
            .find(|c| c.rule == rule)
            .unwrap_or_else(|| panic!("{f}: {cs:#?}"));
        assert!(c.is_safe(), "{f}: {:#?}", c.verdict);
        assert_eq!(c.profitability, ProfitabilityTier::SizeDependent);
        assert!(c.is_reportable(), "{f}: {:?}", c.not_profitable);
        let fixed = apply(REWRITES, c.fix().unwrap());
        let body = function_text(&fixed, f);
        assert!(
            body.contains(line_a) && body.contains(line_b),
            "{f}:\n{body}"
        );
        assert!(parse_module(&fixed).is_ok(), "{f}");
    }
}

// --------------------------------------------------------------------------- edge cases

#[test]
fn a1_hazard_blocks_every_candidate() {
    let src = "def f(items):\n    x = []\n    for i in items:\n        x = x + [i]\n    eval('1')\n    return x\n";
    let cands = analyze(src);
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].verdict.tier, SafetyTier::Blocked);
    assert!(is_blocked(status(&cands[0], K::A1HazardFree)));
    assert!(cands[0].fix().is_none());
}

#[test]
fn shadowed_builtins_are_not_trusted() {
    let base = "def f(items):\n    x = list(items)\n    for i in items:\n        x = x + [i]\n    return x\n";
    assert!(analyze(base)[0].is_safe());
    for prefix in [
        "list = tuple\n",
        "from helpers import *\n",
        "import builtins\n",
        "def g():\n    global list\n    list = 1\n",
    ] {
        let src = format!("{prefix}{base}");
        let c = &analyze(&src)[0];
        assert!(!c.is_safe(), "{prefix}");
        assert!(is_blocked(status(c, K::ExactType)), "{prefix}");
    }
}

#[test]
fn aliases_escapes_and_closures_block_opt101() {
    let cases = [
        // A live alias of the initial list.
        (
            "    snap = x\n",
            "    return snap\n",
            K::ExclusiveAccessAtWrite,
            true,
        ),
        // An escape inside the loop.
        ("", "", K::Unique, false),
    ];
    let (pre, post, kind, disproved) = &cases[0];
    let src =
        format!("def f(items):\n    x = []\n{pre}    for i in items:\n        x = x + [i]\n{post}");
    let c = &analyze(&src)[0];
    assert_eq!(c.verdict.tier, SafetyTier::Disproved);
    assert_eq!(is_disproved(status(c, *kind)), *disproved);

    let escaped = "def f(items, sink):\n    x = []\n    for i in items:\n        x = x + [i]\n        sink(x)\n    return x\n";
    let c = &analyze(escaped)[0];
    assert!(is_blocked(status(c, K::Unique)), "{:#?}", c.verdict);

    let closure = "def f(items):\n    x = []\n    g = lambda: x\n    for i in items:\n        x = x + [i]\n    return g\n";
    let c = &analyze(closure)[0];
    assert!(is_blocked(status(c, K::Unique)), "{:#?}", c.verdict);

    let param =
        "def f(acc, items):\n    for i in items:\n        acc = acc + [i]\n    return acc\n";
    let c = &analyze(param)[0];
    assert!(!c.is_safe() && !c.is_reportable(), "{:#?}", c.verdict);

    let conditional_alias = "def f(items, c):\n    x = []\n    other = []\n    y = x if c else other\n    for i in items:\n        x = x + [i]\n    return y\n";
    let c = &analyze(conditional_alias)[0];
    assert!(
        is_disproved(status(c, K::ExclusiveAccessAtWrite)),
        "{:#?}",
        c.verdict
    );
}

#[test]
fn exception_handlers_and_finally_are_conservative() {
    // A stale binding of `z` is visible to the handler if `g()` raises.
    let stale = "def f(items, g):\n    x = []\n    z = None\n    try:\n        for i in items:\n            z = x\n            x = x + [i]\n            g()\n            z = None\n    except ValueError:\n        return z\n    return x\n";
    let c = &analyze(stale)[0];
    assert!(!c.is_safe(), "{:#?}", c.verdict);
    assert!(
        is_blocked(status(c, K::ExceptionStateSafe))
            || is_disproved(status(c, K::ExclusiveAccessAtWrite)),
        "{:#?}",
        c.verdict
    );
    // `return x` inside `try` / `finally`: the finally block runs after the return value is taken.
    let finally = "def f():\n    x = []\n    try:\n        return x\n    finally:\n        for i in range(3):\n            x = x + [i]\n";
    let c = &analyze(finally)[0];
    assert!(!c.is_safe(), "{:#?}", c.verdict);
    // A handler that only reads the accumulator itself is fine.
    let fine = "def f(items):\n    x = []\n    try:\n        for i in items:\n            x = x + [10 // i]\n    except ZeroDivisionError:\n        return x\n    return x\n";
    assert!(
        analyze(fine)[0].is_safe(),
        "{:#?}",
        analyze(fine)[0].verdict
    );
}

#[test]
fn eval_order_and_statement_shapes() {
    let walrus = "def f(items):\n    x = []\n    for i in items:\n        x = x + [(x := [i])]\n    return x\n";
    let c = &analyze(walrus)[0];
    assert!(!c.is_safe(), "{:#?}", c.verdict);
    // Chained / parenthesized / annotated assignments are not rewritten textually.
    for stmt in ["y = x = x + [i]", "x = (x + [i])", "x: list = x + [i]"] {
        let src = format!(
            "def f(items):\n    x = []\n    for i in items:\n        {stmt}\n    return x\n"
        );
        for c in analyze(&src) {
            assert!(c.fix().is_none(), "{stmt}: {c:#?}");
        }
    }
    // One-line bodies and trailing comments keep their layout.
    let src = "def f(items):\n    x = []\n    for i in items: x = x + [i]  # grow\n    return x\n";
    let fixed = apply(src, analyze(src)[0].fix().unwrap());
    assert!(
        fixed.contains("for i in items: x.append(i)  # grow"),
        "{fixed}"
    );
    // A starred element becomes `extend`.
    let src =
        "def f(items):\n    x = []\n    for i in items:\n        x = x + [*i]\n    return x\n";
    let fixed = apply(src, analyze(src)[0].fix().unwrap());
    assert!(fixed.contains("x.extend([*i])"), "{fixed}");
}

#[test]
fn opt102_forms() {
    let exact = "def f(items):\n    d = {}\n    extra = {'a': 1}\n    for i in items:\n        d = {**d, **extra}\n    return d\n";
    let fixed = apply(exact, analyze(exact)[0].fix().unwrap());
    assert!(fixed.contains("d.update(extra)"), "{fixed}");
    let unknown_rhs = "def f(items, other):\n    s = set()\n    for i in items:\n        s = s | other\n    return s\n";
    let c = &analyze(unknown_rhs)[0];
    assert!(!c.is_safe() && c.is_reportable(), "{:#?}", c.verdict);
    let reunpack = "def f(items):\n    d = {}\n    for k in items:\n        d = {**d, k: 1, **d}\n    return d\n";
    let c = &analyze(reunpack)[0];
    assert!(is_disproved(status(c, K::DictUnpackLeading)));
    let dict_or =
        "def f(items):\n    d = {}\n    for k in items:\n        d = d | {k: 1}\n    return d\n";
    let fixed = apply(dict_or, analyze(dict_or)[0].fix().unwrap());
    assert!(fixed.contains("d |= {k: 1}"), "{fixed}");
}

#[test]
fn opt103_reports_immutable_accumulation_without_edits() {
    let src = "def f(chunks):\n    acc = b''\n    for c in chunks:\n        acc = acc + c\n    return acc\n";
    let c = &analyze(src)[0];
    assert_eq!(c.rule, OptRule::Opt103);
    assert!(c.is_reportable(), "{:#?}", c.verdict);
    assert!(c.edits.is_empty());
    // Local `str` accumulation is optimized by CPython; numbers do not grow.
    for init in ["''", "0"] {
        let src = format!(
            "def f(chunks):\n    acc = {init}\n    for c in chunks:\n        acc = acc + c\n    return acc\n"
        );
        assert!(analyze(&src).is_empty(), "{init}");
    }
    // Reading the accumulator inside the loop blocks the buffer rewrite.
    let src = "def f(chunks, log):\n    acc = ()\n    for c in chunks:\n        acc += (c,)\n        log(len(acc))\n    return acc\n";
    let c = &analyze(src)[0];
    assert!(!c.is_safe());
    // `self.s += ...` is reported when the class assigns the attribute a string literal: the
    // right operand alone does not show it (`list += str` extends a list in place).
    let method = "    def f(self, xs):\n        for x in xs:\n            self.s += f'{x},'\n";
    let src = format!("class C:\n    def __init__(self):\n        self.s = ''\n\n{method}");
    let c = &analyze(&src)[0];
    assert_eq!((c.rule, c.target.as_str()), (OptRule::Opt103, "self.s"));
    assert!(c.is_reportable() && !c.is_safe(), "{:#?}", c.verdict);
    let c = &analyze(&format!("class C:\n{method}"))[0];
    assert!(!c.is_reportable(), "{c:#?}");
}

#[test]
fn opt110_profitability_and_probe() {
    // Small constant displays are not worth hoisting.
    let small = "def f(items):\n    allowed = [1, 2, 3]\n    out = []\n    for i in range(items):\n        if i in allowed:\n            out.append(i)\n    return out\n";
    let c = &analyze(small)[0];
    assert!(c.is_safe() && !c.is_reportable(), "{c:#?}");
    // Unknown container types are not reported (nothing shows a linear scan).
    let param =
        "def f(items, allowed):\n    for i in items:\n        if i in allowed:\n            pass\n";
    let c = &analyze(param)[0];
    assert!(!c.is_reportable());
    // An unknown probe could define `__eq__` without a matching `__hash__`.
    let probe = "def f(items):\n    allowed = list(range(20))\n    out = []\n    for i in items:\n        if i in allowed:\n            out.append(i)\n    return out\n";
    let c = &analyze(probe)[0];
    assert!(
        is_blocked(status(c, K::HashConsistentProbe)) && c.is_reportable(),
        "{c:#?}"
    );
    // Sets and strings are not candidates.
    let set = "def f(items):\n    s = set(items)\n    for i in items:\n        if i in s:\n            pass\n";
    assert!(analyze(set).is_empty());
}

#[test]
fn opt120_needs_deque_equivalent_uses_and_an_import_point() {
    let returned = "import os\n\ndef f(n):\n    q = [0]\n    while q and len(q) < n:\n        q.append(q.pop(0) + 1)\n    return q\n";
    let c = &analyze(returned)[0];
    assert!(
        is_blocked(status(c, K::DequeEquivalentOps)),
        "{:#?}",
        c.verdict
    );
    let sorted = "def f(n):\n    q = [3, 1]\n    while q:\n        q.sort()\n        q.pop(0)\n";
    let c = &analyze(sorted)[0];
    assert!(is_blocked(status(c, K::DequeEquivalentOps)));
    // Without `collections` imported, `import collections` goes before the first import.
    let src = "\"\"\"Doc.\"\"\"\nfrom __future__ import annotations\n\nimport os\n\ndef f(n):\n    q = list(range(n))\n    out = []\n    while q:\n        out.append(q.pop(0))\n    return out\n";
    let c = &analyze(src)[0];
    assert!(c.is_safe(), "{:#?}", c.verdict);
    let fixed = apply(src, c.fix().unwrap());
    assert!(
        fixed.contains("from __future__ import annotations\n\nimport collections\nimport os\n"),
        "{fixed}"
    );
    assert!(
        fixed.contains("q = collections.deque(range(n))")
            && fixed.contains("out.append(q.popleft())"),
        "{fixed}"
    );
    // `from collections import deque` is reused; `del q[0]` and `q = q[1:]` become popleft.
    let src = "from collections import deque\n\ndef f(n):\n    q = [1, 2, 3, 4, 5, 6, 7, 8, 9]\n    total = 0\n    while q:\n        total += q[0]\n        del q[0]\n    return total\n";
    let fixed = apply(src, analyze(src)[0].fix().unwrap());
    assert!(
        fixed.contains("q = deque([1, 2, 3, 4, 5, 6, 7, 8, 9])")
            && fixed.contains("        q.popleft()\n"),
        "{fixed}"
    );
    let src = "import collections\n\ndef f(n):\n    q = [1, 2, 3]\n    total = 0\n    while q:\n        total += q[0]\n        q = q[1:]\n    return total\n";
    let c = &analyze(src)[0];
    assert!(c.is_safe(), "{:#?}", c.verdict);
    let fixed = apply(src, c.fix().unwrap());
    assert!(
        fixed.contains("q = collections.deque([1, 2, 3])")
            && fixed.contains("        q.popleft()\n"),
        "{fixed}"
    );
}

#[test]
fn analysis_converges_on_irreducible_shapes() {
    // Deeply nested loops with many allocation sites still reach a fixpoint.
    let mut src = String::from("def f(items):\n    x = []\n");
    for d in 0..6 {
        src.push_str(&format!("{}for i{d} in items:\n", "    ".repeat(d + 1)));
    }
    src.push_str(&format!("{}x = x + [i5]\n    return x\n", "    ".repeat(7)));
    let c = &analyze(&src)[0];
    assert!(
        c.verdict
            .get(K::AnalysisConverged)
            .unwrap()
            .is_established()
    );
    assert!(c.is_safe(), "{:#?}", c.verdict);
}

// ------------------------------------------------------------------------ invariant guards

#[test]
fn a_live_old_value_of_the_accumulator_disproves_exclusive_access() {
    // Python source cannot express it (the write renames `x`), so the IR is changed by hand:
    // the loop body reads the old `x` again right after `x = x + [i]` (`x is x`).
    let src = "def f(items):\n    x = []\n    for i in items:\n        x = x + [i]\n    return x\n";
    let kb = EffectiveSoundnessKb::load_builtin(PythonVersion::PY315).unwrap();
    let opts = AnalysisOptions::default();
    assert!(analyze_module(&ir_of(src), src, &kb, &opts)[0].is_safe());
    let mut ir = ir_of(src);
    let f = ir.functions.iter_mut().find(|f| f.name == "f").unwrap();
    let (b, i, old) = f
        .blocks
        .iter()
        .enumerate()
        .find_map(|(b, bb)| {
            bb.insts
                .iter()
                .position(|inst| matches!(inst.kind, heki_ir::InstKind::BinOp { .. }))
                .map(|i| {
                    let heki_ir::InstKind::BinOp { lhs, .. } = bb.insts[i].kind else {
                        unreachable!()
                    };
                    (b, i, lhs)
                })
        })
        .unwrap();
    let dst = VarId(f.var_names.len() as u32);
    f.var_names.push(None);
    f.blocks[b].insts.insert(
        i + 2,
        heki_ir::Inst {
            kind: heki_ir::InstKind::Compare {
                dst,
                op: heki_ir::CompareOpKind::Is,
                lhs: old,
                rhs: old,
            },
            span: Span { start: 0, end: 0 },
        },
    );
    let c = &analyze_module(&ir, src, &kb, &opts)[0];
    assert_eq!(c.verdict.tier, SafetyTier::Disproved, "{:#?}", c.verdict);
    assert_eq!(
        status(c, K::ExclusiveAccessAtWrite),
        &ObligationStatus::Disproved {
            counterexample: "the old value of `x` is read after the write".into()
        }
    );
}
