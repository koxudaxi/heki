# Fixes and Safety

`heki check --fix` rewrites an `OPT` finding only when heki proves that the rewrite keeps what
the program does. This page shows the fix commands, what each finding offers, and what a fix
must prove.

## Fix commands

```console
$ heki check --diff                              # preview (exit code 1 when something would change)
$ heki check --fix                               # apply safe, unconditional fixes
$ heki check --fix --include-size-dependent      # also the proven-safe size-dependent rewrites
$ heki check --fix --profile profile.txt         # size-dependent rewrites in hot functions only
```

- `--diff` takes the same flags as `--fix` and writes nothing.
- `--include-size-dependent` applies the size-dependent rewrites in every function, with or
  without `--profile`.
- `--profile` alone applies them only in the functions a [profile](#profiles) shows hot.

## What each finding offers

Every `OPT` rule finds a quadratic or linear-per-iteration pattern that has a faster equivalent.
Two independent questions decide whether heki offers the rewrite:

| Question | Possible answers |
| -------- | ---------------- |
| **Safety:** does the rewrite keep the program's behavior? | every proof obligation is `Established`, `Blocked` (cannot be proven here), or `Disproved` (a concrete counterexample shape exists) |
| **Profitability:** is the rewrite faster for every input? | unconditional, or size-dependent (it can be slower for tiny inputs or for loops that run zero times) |

### Safety

| Obligations | What you get |
| ----------- | ------------ |
| All established | A diagnostic with a fix. A statement whose shape heki does not rewrite (a parenthesized `x = (x + [i])`, an annotated `x: list = x + [i]`) gets a diagnostic saying the rewrite is proven safe, to be made by hand. |
| One blocked | A diagnostic naming the first blocking obligation, and no fix. |
| One disproved | Nothing: the pattern only looks quadratic, and rewriting it would be a bug. |

### Profitability

| Profitability | Rules | Applied by |
| ------------- | ----- | ---------- |
| Unconditional (faster from the first iteration) | `OPT101`, `OPT102` | `heki check --fix` |
| Size-dependent | `OPT110`, `OPT111`, `OPT120`, `OPT121`, `OPT130` | `heki check --fix --include-size-dependent`, or `--fix --profile <dump>` in hot functions |
| Size-dependent, no rewrite | `OPT103`, `OPT112`, `OPT113` | none: suggestion only |

### When the size is fixed

A size-dependent rule reports nothing when the code fixes the size:

- the container holds at most 16 items and nothing grows it past that or lets it escape. It
  is a display without `*` / `**` unpacking, a string constant, a builtin constructor of an
  iterable the code fixes (`list(range(7))`, `tuple(".;:")`, `sorted(NAMES)`, `list()`), a
  slice with constant bounds (`cases[:3]`, `paths[-2:]`), or a comprehension over fixed
  iterables (`[c for c in LABELS if c in wanted]`). Shrinking it (`expected.pop(0)` on
  `expected = [b"a", b""]`) and read-only builtins are fine, and so is filling a new list a
  fixed number of times (`ids.append(card["id"])` in a loop over at most 6 cards). A tuple or
  a string stays fixed whatever other code does with it, unless other code rebinds its name
  (`cfg.STATEMENTS = tuple(rows)`). This holds also through local names,
  module constants, and other modules' module-level displays and class attributes, read in a
  function or in a module body;
- the loop runs a fixed number of times: over such a container, a `range` of constants with at
  most 16 steps, or an iterable a test on every path to the loop caps (`if not 1 <= len(cards)
  <= 6: raise ValueError(...)`, `assert len(xs) <= 3`, also after a loop that shrinks it, when
  nothing on the way to the loop grows it or hands it to other code);
- the list is rebuilt in every iteration;
- the loop's own test caps the accumulator: `while len(buf) < 10`.

These facts decide profitability only, never safety. A size known only at run time does not
count: a list whose size depends on the input is reported even when it always holds three items.

## What a fix must prove

The analysis behind the `OPT` rules is separate from the one behind the other rules, and
deliberately conservative:

| Obligation | Why | Example |
| ---------- | --- | ------- |
| No type annotation is trusted | a subclass can override `__contains__` or `__add__` | `x: list[int] = WeirdList(...)` is not a list for heki |
| The object is created in the function and never escapes | heki counts anything it cannot see as shared: parameters, globals, attributes, values passed to unknown functions, names captured by closures | `sink(rows)` inside the loop blocks `rows.append(...)` |
| Access is exclusive: no other variable, iterator, or view still refers to the old object after the statement | an in-place update would change what they see | `for v in x: x = x + [v]` ends, but `x.append(v)` would loop forever, so heki reports nothing |
| Evaluation order and exceptions are kept | a rewrite must not call anything earlier or later | `{**d, f(): g()}` becomes `d.update({f(): g()})`, not `d[f()] = g()`, which would call `g` first |
| Builtin names are the builtins | the module can rebind them | trusted only while nothing rebinds them: an assignment, a `global` store, `from m import *`, `globals()`, or a module that patches `builtins` (a `.pyi` stub never runs, so its `import builtins` does not count) |
| Locals change only through the code | `eval`, `exec`, `locals()`, `sys._getframe`, and frame attributes can change them behind any analysis | candidates in such functions are always blocked; `heki assumptions` lists these places in a project |

The scope analysis is checked against CPython's `symtable`, and adversarial programs that break a
naive rewrite are run on CPython: see [Development](development.md#checks-against-cpython).

## How `--fix` applies fixes

1. It applies the fixes that do not overlap.
2. It analyzes the result again and repeats until nothing changes, at most ten passes: a fix that
   becomes provable after another one is applied too.
3. It reports what is left.

| Case | What happens |
| ---- | ------------ |
| A suppressed diagnostic (`# heki: ignore[...]`) | It is never fixed. |
| A file that changed on disk meanwhile (an editor saved it) | It is not written: the run keeps that save, reports an error, and exits with code 2. Run `--fix` again. |
| Fixes of a pass that would leave a file that does not parse | None of them is applied to that file, and the run exits with code 2 (a heki bug to report). |
| `\n`, `\r\n`, or lone `\r` line breaks | Fixes work the same, and the lines they insert use the file's own line break. |

`--diff` prints the paths every other output format prints (relative to the working directory),
so `patch -p1` applies it where heki ran.

## Fixes keep type checkers and isort satisfied

A `collections.deque` rewrite (`OPT120`, `OPT121`) also rewrites the variable's annotation and
adds the import:

```console
$ heki check --diff --include-size-dependent jobs.py
--- a/jobs.py
+++ b/jobs.py
@@ -1,11 +1,12 @@
 """Job queue."""
 
+import collections
 import os
 
 
 def drain(jobs: list[int]) -> int:
-    queue: list[int] = list(jobs)
+    queue: collections.deque[int] = collections.deque(list(jobs))
     total = 0
     while queue:
-        total += queue.pop(0)
+        total += queue.popleft()
     return total
Would fix 1 issue(s) in 1 file(s).
```

- An annotation the fix cannot rewrite (`Sequence[int]`, a union), or another annotated
  declaration of the variable, blocks the fix.
- `import collections` goes into the module's block of standard-library imports, in the order
  that block follows: straight imports first, or sorted by module as with
  `force-sort-within-sections`.
- Without such a block, it starts one before the other imports. In a module without imports, it
  follows the docstring and `__future__` imports, with the blank lines isort expects.
- It never goes above a shebang, an encoding declaration, or a `from __future__` import, and only
  imports run before it, so the rewritten function cannot run before `collections` is bound.
- An existing `from collections import deque` or `import collections` is reused only when it runs
  unconditionally (not under `if TYPE_CHECKING:`) and names the standard library's module (not
  `from .collections import deque`).

## Profiles

`--profile` reads a sampling profile you recorded offline. It is experimental; see [experimental.md](experimental.md).

| Profiler | Command |
| -------- | ------- |
| CPython 3.15's sampling profiler | `python -m profiling.sampling run --collapsed -o stacks.txt app.py`, or `--gecko -o profile.json` |
| py-spy | `py-spy record --format raw -o stacks.txt -- python app.py` |

- A function is hot when it is on the stack in at least 1 % of the samples.
- The profile promotes size-dependent fixes in hot functions and adds the sample share to the
  diagnostics' evidence. It never makes an unsafe rewrite safe.

A frame counts for a function when all of these hold:

- its name names the function on a dotted boundary: `run` or `A.run` for `A.run`, never `BA.run`;
- its file is the function's file, as a basename, a relative path, or an absolute path;
- its line, when the profiler recorded one, lies inside the function.

A frame that fits several functions counts for none: a plain `run` without a line in a file with
two `run` methods, or a `models.py` that two packages have. `--watch` matches frames by their
names only.
