# Rule Precision

Every heki report is **exact** or **heuristic**:

- An exact report is built only from facts heki established. Its claim, what its message states
  about the code, is a set of premises (its rule page lists them under "What a report states"),
  and each premise was established: where heki cannot tell, it does not report. A false exact
  report is a bug.
- A heuristic report states the same kind of claim, but some premise may be open: heki reported
  without ruling it out. It can be wrong about the code, and every output labels it `heuristic`.

A rule is exact once it is certified (below), and heuristic until then. An exact rule's reports
are exact, with one exception: an `OPT` rule's report that did not establish a premise of the
rule's exact tier is heuristic, as that premise is open: a rewrite heki could not prove safe, a
loop whose trip count the code fixes, or an `OPT113` join whose rule page premises do not all
hold. Only exact reports are shown by default.

- A heuristic rule runs when `select` or `extend-select` names its code or name. With
  `lint.heuristic = true` (`--heuristic`), the heuristic rules that are not opt-in also run by
  default and for their prefixes and `ALL`.
- The heuristic reports of an exact rule are shown under the same conditions: with
  `lint.heuristic = true`, or when its code or name is selected.
- A contract rule runs when its contract is declared, whatever its certainty.
- A run of `full` or `concise` output that leaves heuristic rules off, or heuristic reports out,
  ends with a note saying how many.

See [Rule selection](configuration.md#rule-selection-toolhekilint). The proven tier of the eight
`OPT` rewrite rules and the exact tier of `OPT113` are exact (certified below); the other rules
are heuristic.

## What a report states

A report states what its message says about the code, in the terms its rule page defines. It is
wrong when that statement is false. Whether the code does harm is a separate question, and so is
whether a rewrite of it would be valid:

- A query in a loop over three items has the shape the rule reports; the harm is small.
- A write the author meant, or a memo heki does not recognize, still changes the object.
- A rewrite that a fact heki cannot prove blocks is a question of [fix safety](fixes.md).

The premises the rules use:

- **Resolves**: a call is to the function shown, through the calls heki resolves ([How a call is
  resolved](scope.md#how-a-call-is-resolved)). Through a method call, every implementation the
  call can reach counts. A receiver typed by a `Protocol` is not resolved for an exact report:
  its implementers are never a complete set.
- **Reaches on every path**: whenever the reported call returns, the effect has run: each
  function on the chain reaches the next call on every path on which it returns. The claim is
  anchored at the reported call, so a branch around that call in its own function does not
  matter.
- **On every iteration**: the statement runs on every iteration of the loop that completes.
  It does not when every way on from it leaves the loop, or when a first-iteration flag, a flag
  set once, or a lazy initialization the loop tests guards it.
- **Steps through an input**: the loop iterates a named input, such as a parameter, an
  attribute of one, the rows of a query, or the lines of a file.
- **Data-sized** (heuristic reports only): the code does not fix the size or the number of
  iterations at 16 or fewer. heki counts a size it cannot bound as data-sized, so this premise
  is open whenever no bound is found. An exact report never claims a size: a bound of 16 or
  fewer only removes reports, and a larger fixed bound is no evidence either way.

An exact report's message states its claim and the witnesses of its premises, nothing else: no
cost estimate such as "(N+1)", and no advice (that goes into the help).

Exact claims hold in every run consistent with seven assumptions
([ARCHITECTURE.md, section 3.4](https://github.com/koxudaxi/heki/blob/main/ARCHITECTURE.md)):

- **R1**: the functions and classes on a call chain are not replaced at run time.
- **R2**: a value annotated `C` is an instance of `C` or of a project subclass. A premise that
  goes through a method call holds only when every implementation in such a subclass has it.
  Rewrites never rely on annotations.
- **R3**: the ecosystem catalog describes the library, an effect with its condition.
- **R4**: imports resolve as configured.
- **R5**: the checked files are the project.
- **R6**: a library call's argument is a value it waits on (a nonzero sleep, a host name to look
  up) unless heki establishes otherwise.
- **R7**: no asynchronous or resource-exhaustion exception (`MemoryError`, `KeyboardInterrupt`, a
  signal) is raised. Under it, loading a local, a module name bound once, a builtin, or a member
  of an imported module, and building a `list` or `tuple` display, do not raise; other attribute
  loads, subscripts, `set` and `dict` displays, calls, awaits, and operators may.

"A branch heki cannot decide can go both ways" is not an assumption: a report that needs it is
heuristic. Neither is a context manager heki does not resolve: it may suppress exceptions, so an
exact report claims nothing that runs in its body after a statement that may raise.

Each checked report gets one class:

| Class | Meaning |
| ----- | ------- |
| C (confirmed) | The problem is real and observable from the code. |
| P (plausible) | The report is right, and the code does harm at realistic sizes. |
| B (benign) | The report is right, but the code is deliberate or harmless at realistic sizes. |
| F (false) | The report is wrong about the code. |

"Right about the code" is C, P, and B together; "real harm" is C and P. Real harm is published
for each rule. It decides only whether an exact rule runs by default: it does when its share of
real harm is worth the line it prints, and is opt-in otherwise.

## How a rule becomes exact

A rule is certified exact when all four hold:

1. **Premise stripping**: for each premise of its claim, the tests hold a counterexample that
   the rule reports once that premise is taken as established, and does not report otherwise.
2. **Census**: every report the rule has on the development corpus (the 378 packages of Stage
   0-A, 30.75 million lines, and the packages added for the rules with few reports) is checked
   by hand by two reviewers. A false claim needs a minimal reproduction, and is fixed as a
   defect of a premise's fact, never by a list of exclusions.
3. **Holdout**: one run on a frozen holdout corpus of 100 packages, never looked at while the
   rule was designed or fixed, finds no false claim outside a violation of R1-R7.
4. **Run-time oracle**: the claimed events (the call that runs, the statement executed on each
   iteration) happen where the code ran, checked with `sys.monitoring` on the differential test
   suite and on datamodel-code-generator.

Every release then runs the corpus with the release candidate and checks every report it adds
or changes in an exact rule. A false claim found later is a bug: it is fixed before the next
release, or the rule becomes heuristic in that release and needs a new certification.

## Certifications

A certified rule has a row here: the release that certified it, and the reports of its census
and of its holdout run with the false claims among them. The census of the proven `OPT` tier
checked by hand every exact report on the Stage 0-A corpus with the build of 2026-11-21 (one
report, whose rewrite needed the set-order obligation, left the tier); the holdout run checked
every exact report of one run on the 100 packages of the frozen holdout corpus. `OPT113`'s
census reports are those of its exact tier with the plain-key premise, all among the reports
the census of the earlier tier checked by hand.

| Rule | Certified | Census: reports / false | Holdout: reports / false |
| ---- | --------- | ----------------------: | -----------------------: |
| `OPT101` | 0.2.0 | 7 / 0 | 2 / 0 |
| `OPT102` | 0.2.0 | 1 / 0 | 0 / 0 |
| `OPT103` | 0.2.0 | 128 / 0 | 188 / 0 |
| `OPT110` | 0.2.0 | 8 / 0 | 0 / 0 |
| `OPT111` | 0.2.0 | 7 / 0 | 7 / 0 |
| `OPT120` | 0.2.0 | 70 / 0 | 33 / 0 |
| `OPT121` | 0.2.0 | 12 / 0 | 5 / 0 |
| `OPT130` | 0.2.0 | 1 / 0 | 0 / 0 |
| `OPT113` | 0.2.0 | 8 / 0 | 6 / 0 |

## Measurements

Round 3 sampled up to 50 reports per rule on the Stage 0-A corpus, with the build of 2026-11-21.
These are measurements of heuristic rules, not certifications. Its reviewers also counted as
wrong a bound past 16 that the code states and an exclusion a rule page documents but heki does
not recognize; under the terms above those reports are right, so these figures are lower
bounds. For `OPT101` and `OPT102` the table keeps the reviewers' verdicts: four of their samples
loop a fixed 4 times, which is not data-sized.

| Rule | Certainty | Measured | Sample | C / P / B / F | Right about the code | 95% interval | Real harm |
| ---- | --------- | -------- | -----: | ------------- | -------------------: | ------------ | --------: |
| `EFF101` | heuristic | round 3, 2026-11-21 | 50 | 3 / 13 / 25 / 9 | 82% | 69-90% | 32% |
| `EFF102` | heuristic | round 3, 2026-11-21 | 50 | 0 / 2 / 25 / 23 | 54% | 40-67% | 4% |
| `EFF103` | heuristic | round 3, 2026-11-21 | 50 | 0 / 8 / 29 / 13 | 74% | 60-84% | 16% |
| `DATA101` | heuristic | round 3, 2026-11-21 | 28 | 0 / 3 / 17 / 8 | 71% | 53-85% | 11% |
| `MUT107` | heuristic | round 3, 2026-11-21 | 50 | 1 / 1 / 37 / 11 | 78% | 65-87% | 4% |
| `MUT110` | heuristic | round 3, 2026-11-21 | 4 | 0 / 0 / 4 / 0 | 100% | 51-100% | 0% |
| `MUT202` | heuristic | round 3, 2026-11-21 | 20 | 2 / 1 / 11 / 6 | 70% | 48-85% | 15% |
| `OPT101` | exact | round 3, 2026-11-21 | 9 | 0 / 0 / 7 / 2 | 78% | 45-94% | 0% |
| `OPT102` | exact | round 3, 2026-11-21 | 6 | 0 / 1 / 3 / 2 | 67% | 30-90% | 17% |
| `OPT103` | exact | round 3, 2026-11-21 | 50 | 0 / 2 / 44 / 4 | 92% | 81-97% | 4% |
| `OPT110` | exact | round 3, 2026-11-21 | 50 | 0 / 3 / 36 / 11 | 78% | 65-87% | 6% |
| `OPT111` | exact | round 3, 2026-11-21 | 50 | 0 / 2 / 42 / 6 | 88% | 76-94% | 4% |
| `OPT112` | heuristic | round 3, 2026-11-21 | 50 | 0 / 2 / 43 / 5 | 90% | 79-96% | 4% |
| `OPT113` | exact | round 3, 2026-11-21 | 50 | 0 / 11 / 35 / 4 | 92% | 81-97% | 22% |
| `OPT120` | exact | round 3, 2026-11-21 | 50 | 0 / 2 / 45 / 3 | 94% | 84-98% | 4% |
| `OPT121` | exact | round 3, 2026-11-21 | 50 | 0 / 1 / 41 / 8 | 84% | 71-92% | 2% |
| `OPT130` | exact | round 3, 2026-11-21 | 30 | 0 / 2 / 27 / 1 | 97% | 83-99% | 7% |

The other rules (the contract rules and the opt-in rules) have not been measured on this
protocol yet.
