# Design Philosophy

heki asks nothing of your code and never runs it. This page says where heki comes from, the six
choices behind it, and what Python leaves open that heki checks.

| Question | Short answer |
| --- | --- |
| Does heki lock me in? | No. Your code imports nothing from it; remove it and the code runs as before ([1](#1-a-linter-not-a-library)). |
| Does it run my code? | No. It reads the source, and never runs, imports, or instruments it ([3](#3-static-only)). |
| How does it share the work with Ruff and type checkers? | Ruff keeps syntax and style, and mypy, Pyright, Pyrefly, and ty keep types. heki takes what needs the call graph: effects, changes to state, and aliases ([6](#6-a-third-layer-gated-by-evidence)). |

## Why heki exists

heki grew out of
[datamodel-code-generator](https://github.com/datamodel-code-generator/datamodel-code-generator),
a tool that generates Python data models from schemas. The project started in 2019, long before
AI coding was common, and kept its quality mainly with end-to-end tests: a test runs the generator
on a schema and compares the code it writes with an expected file. As the project grew, so did the
tests, from 129 expected files at the start of 2021 to more than 21,000 in November 2026.

To keep test time down and make the code more robust, the maintainers added check scripts beside
the tests to guard invariants: which layers may import which, how many escape hatches the source
holds, and the patterns that reviews kept finding. The scripts and the CI grew too. During 2026
the `scripts/` folder went from 6 files to 100, and a scheduled run of the test workflow from 26
jobs and about half an hour of runner time to 203 jobs and about 13 hours. Each new invariant was
one more script to write, test, and keep running.

From that came the idea of heki: instead of one more script for each invariant, one fast, general
tool that checks state ownership, side effects, and architecture walls. datamodel-code-generator
now runs heki in its CI, and heki contracts have replaced its 929-line architecture checker.

Now that coding agents write much of the code, not only people, the same mechanism works as a
guardrail: a wall in `heki.toml` holds for an agent's change as it does for a person's.

## Design principles

### 1. A linter, not a library

heki is a program you run beside your code, not a package your code imports. There are no
decorators, contract types, or wrappers to add, and nothing runs with your program: the `heki`
package installs one executable and no Python dependencies. Remove heki and the code runs as
before; what stays behind is a configuration file and any `# heki: ignore[...]` comments.

A library of decorators would put each check next to the code it guards, but every module would
then import it, in tests and in production, and leaving it would mean editing that code again. A
tool you can drop at any time is cheap to try, and nothing in your code depends on it.

### 2. Standard Python, read as written

heki adds no syntax and asks for no new annotations. It reads what the code already says:

- a `Sequence`, `Mapping`, or `AbstractSet` parameter is one the function only reads (`MUT110`);
- a `frozen` dataclass, a `Final` attribute, or a `ReadOnly` key is not changed in place
  (`MUT108`);
- an `async def` must not wait on a blocking call, even several helpers below (`EFF101`);
- a lock that is not reentrant, held by a `with` block, is not taken again inside it, also
  through a call (`EFF109`);
- what an `lru_cache` or `cache` function returns is shared by every caller (`MUT114`).

Ownership or borrow markers would make some checks easier, but they would be a dialect only heki
understands. Type checkers, editors, and readers already know these types and statements, so the
promise is already written down; heki checks that the code keeps it.

### 3. Static only

heki never runs, imports, or instruments your program, and patches nothing at run time. It needs
no database, no environment variables, and no virtual environment with your dependencies: the
effects of the standard library and of installed packages come from a catalog, not from their
source ([Where heki's knowledge comes from](scope.md#where-hekis-knowledge-comes-from)). That
makes it safe to run in CI on pull requests you do not trust. It is one Rust executable that
needs no Python interpreter, and it checks a 100,000-line project in a few seconds.

Tests see only the paths a run takes, and run-time checks cost time in production. heki follows
every path, including an error branch no test reaches, and adds nothing to the program.

### 4. Walls outside the code

Contracts live in a configuration file, best a `heki.toml` of its own, and today's findings in
`heki-baseline.toml`, not in the `.py` files. A wall written as a decorator or a marker would sit
in files that people and agents edit all day: deleting one line silences it, and `CODEOWNERS` can
protect that line only by protecting the whole file. A wall in its own file can have an owner, so
moving it is a change reviewed on its own
([Architecture rules with agents](coding-agents.md#architecture-rules-with-agents)).

The only heki comment a `.py` file holds is an inline suppression (`# heki: ignore[...]`, or
Ruff's `# noqa`). `heki check --report-suppressed` writes them to a file, so CI can compare a pull
request with its base ([Auditing inline suppressions](adoption.md#auditing-inline-suppressions)).

### 5. Silent when unsure

heki does not report a guess as a fact. A report is exact only when every premise of its claim is
established, and the rules that run by default are exact: every report they made on 378 packages
and on a frozen holdout of 100 more was checked by hand, with no false claim. A report with an
open premise is labeled heuristic, and heuristic rules run only when you ask for them. When heki
cannot tell, it says nothing ([Rule precision](precision.md)).

The rewrites go further. `heki check --fix` uses a separate analysis that does not trust even
type annotations, because a subclass can override any method, and it applies a rewrite only when
it proves that the program behaves the same under CPython's semantics
([What a fix must prove](fixes.md#what-a-fix-must-prove)). A checker that guesses gets ignored or
turned off; one that reports only what it can back can fail a build.

### 6. A third layer, gated by evidence

heki takes only what needs control flow, the call graph, or alias facts. What the syntax of one
file decides belongs to Ruff, and whether values match their types to mypy, Pyright, Pyrefly, and
ty. Where another tool covers part of a case, heki keeps the rest, and its configuration,
suppression comments, and output formats follow Ruff's ([Configuration](configuration.md)).

A new rule is decided by evidence, not by feel. It is counted on a corpus of real projects (378
packages, 30.75 million lines), its reports are judged by hand for real harm, and it joins the
default set only when its census and a run on a frozen holdout show no false claim
([How a rule becomes exact](precision.md#how-a-rule-becomes-exact)). The shapes that failed are
listed with their counts in [Measured and not taken](scope.md#measured-and-not-taken), and the
whole gate is [How a rule gets into heki](scope.md#how-a-rule-gets-into-heki).

## What Python leaves open

Other languages enforce some of what heki checks when the code compiles. In Python nothing does,
so heki reports the code that breaks the expectation.

| What Python leaves open | What other languages enforce | What heki checks |
| --- | --- | --- |
| Any code can change any object it reaches | ownership and borrowing (Rust; [each idea mapped to heki](ownership.md#if-you-know-rust)), `const` / `readonly` | read-only parameters, fields, and views changed through helpers (`MUT110`, `MUT108`, `MUT120`); `write-owners` / `read-owners` contracts (`MUT201`, `MUT203`) |
| A signature does not say what a function does | effect types, `IO` in the type (Haskell) | blocking calls reached from `async def`, and clients and queries in loops (`EFF101`–`EFF103`); `effect-boundaries` (`EFF201`); effect snapshots (`MUT103`) |
| Shared mutable objects hide changes | value semantics, immutability by default | stale cached properties and hashes (`MUT101`, `MUT107`); changed shared return values (`MUT114`, `MUT202`); writes an `invalidation` contract must announce (`MUT208`) |
| Dynamic dispatch blocks optimizations | static types let a compiler prove a rewrite | quadratic patterns (`OPT`), rewritten only when proven under CPython's semantics |

The [Rust table](ownership.md#if-you-know-rust) on State Ownership and Mutation maps each Rust
idea to the closest heki rule and says how Python differs.
