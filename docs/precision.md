# Rule Precision

Every heki rule is **exact** or **heuristic**:

- An exact rule had at least 100 of its reports checked by hand, at most 3 from one package,
  and none was wrong about the code. A wrong report found later is a bug: it is fixed before the
  next release, or the rule becomes heuristic in that release.
- A heuristic rule's report can be wrong about the code. Every output labels its reports
  `heuristic`.

Only the exact rules run by default. A heuristic rule runs when `select` or `extend-select` names
its code or name; with `lint.heuristic = true` (`--heuristic`), the heuristic rules that are not
opt-in also run by default and for their prefixes and `ALL`. A contract rule runs when its
contract is declared, whatever its certainty. See
[Rule selection](configuration.md#rule-selection-toolhekilint).

No rule is exact yet: no rule has had 100 of its reports checked, and every rule measured so far
had reports that were wrong about the code.

## What counts as wrong

A report states what its message says about the code, in the terms its rule page defines. It is
wrong when that statement is false. Whether the code does harm is a separate question, and so is
whether a rewrite of it would be valid:

- A query in a loop over three items has the shape the rule reports; the harm is small.
- A write the author meant, or a memo heki does not recognize, still changes the object.
- A rewrite that a fact heki cannot prove blocks is a question of [fix safety](fixes.md).

Each checked report gets one class:

| Class | Meaning |
| ----- | ------- |
| C (confirmed) | The problem is real and observable from the code. |
| P (plausible) | The report is right, and the code does harm at realistic sizes. |
| B (benign) | The report is right, but the code is deliberate or harmless at realistic sizes. |
| F (false) | The report is wrong about the code. |

"Right about the code" is C, P, and B together; "real harm" is C and P. Real harm is published
for each rule but decides nothing about its certainty.

The terms the rules use:

- **Data-sized**: the code does not fix the size or the number of iterations at 16 or fewer. A
  bound stated anywhere heki reads counts: a literal, a constant, `range(K)`, a slice, a `len`
  test or cap, a counted `while`, or a comprehension over such a value. For a function that only
  the checked files can call, the arguments its callers pass count too. A bound past 16 is
  data-sized.
- **On every iteration**: the statement runs on at least two iterations of some run of the loop.
  It does not when every way on from it leaves the loop, or when a first-iteration flag, a flag
  set once, or a lazy initialization the loop tests guards it.
- **Reaches**: through the calls heki resolves ([How a call is
  resolved](scope.md#how-a-call-is-resolved)), along branches the code does not rule out.

A rule becomes exact only once its page and its message state its claim in these terms.

## How a rule is measured

1. **Corpus**: the 378 packages of Stage 0-A (30.75 million lines), frozen, and packages chosen
   for the rules with few reports, frozen for each release cycle.
2. **Sample**: 100 reports per rule, or all of them when it has fewer: one report per package in
   each round, at most 3 per package, mature and young packages alternating, reports in tests
   marked.
3. **Review**: at least two reviewers per rule, on one rubric. A wrong report needs a minimal
   reproduction on the measured build, and a second reviewer confirms it.
4. **Exact**: no wrong report among 100 or more. No wrong report in 100 puts the rate of wrong
   reports below 3% (one-sided, 95%).
5. **Every release**: a corpus run of the release candidate, a check of every report it adds or
   changes in an exact rule, and a fresh sample of 30 reports per exact rule.

## Measurements

Round 3 sampled up to 50 reports per rule on the Stage 0-A corpus, with the build of 2026-11-21.
Its reviewers also counted as wrong a bound past 16 that the code states and an exclusion a rule
page documents but heki does not recognize; under the terms above those reports are right, so
these figures are lower bounds. For `OPT101` and `OPT102` the table keeps the reviewers' verdicts:
four of their samples loop a fixed 4 times, which is not data-sized.

| Rule | Certainty | Measured | Sample | C / P / B / F | Right about the code | 95% interval | Real harm |
| ---- | --------- | -------- | -----: | ------------- | -------------------: | ------------ | --------: |
| `EFF101` | heuristic | round 3, 2026-11-21 | 50 | 3 / 13 / 25 / 9 | 82% | 69-90% | 32% |
| `EFF102` | heuristic | round 3, 2026-11-21 | 50 | 0 / 2 / 25 / 23 | 54% | 40-67% | 4% |
| `EFF103` | heuristic | round 3, 2026-11-21 | 50 | 0 / 8 / 29 / 13 | 74% | 60-84% | 16% |
| `DATA101` | heuristic | round 3, 2026-11-21 | 28 | 0 / 3 / 17 / 8 | 71% | 53-85% | 11% |
| `MUT107` | heuristic | round 3, 2026-11-21 | 50 | 1 / 1 / 37 / 11 | 78% | 65-87% | 4% |
| `MUT110` | heuristic | round 3, 2026-11-21 | 4 | 0 / 0 / 4 / 0 | 100% | 51-100% | 0% |
| `MUT202` | heuristic | round 3, 2026-11-21 | 20 | 2 / 1 / 11 / 6 | 70% | 48-85% | 15% |
| `OPT101` | heuristic | round 3, 2026-11-21 | 9 | 0 / 0 / 7 / 2 | 78% | 45-94% | 0% |
| `OPT102` | heuristic | round 3, 2026-11-21 | 6 | 0 / 1 / 3 / 2 | 67% | 30-90% | 17% |
| `OPT103` | heuristic | round 3, 2026-11-21 | 50 | 0 / 2 / 44 / 4 | 92% | 81-97% | 4% |
| `OPT110` | heuristic | round 3, 2026-11-21 | 50 | 0 / 3 / 36 / 11 | 78% | 65-87% | 6% |
| `OPT111` | heuristic | round 3, 2026-11-21 | 50 | 0 / 2 / 42 / 6 | 88% | 76-94% | 4% |
| `OPT112` | heuristic | round 3, 2026-11-21 | 50 | 0 / 2 / 43 / 5 | 90% | 79-96% | 4% |
| `OPT113` | heuristic | round 3, 2026-11-21 | 50 | 0 / 11 / 35 / 4 | 92% | 81-97% | 22% |
| `OPT120` | heuristic | round 3, 2026-11-21 | 50 | 0 / 2 / 45 / 3 | 94% | 84-98% | 4% |
| `OPT121` | heuristic | round 3, 2026-11-21 | 50 | 0 / 1 / 41 / 8 | 84% | 71-92% | 2% |
| `OPT130` | heuristic | round 3, 2026-11-21 | 30 | 0 / 2 / 27 / 1 | 97% | 83-99% | 7% |

The other rules (the contract rules and the opt-in rules) have not been measured on this
protocol yet.
