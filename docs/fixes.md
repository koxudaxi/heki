# Fixes and Safety

Every `OPT` rule detects a quadratic or linear-per-iteration pattern and knows a faster
equivalent. Whether the rewrite is **offered** depends on two independent questions:

1. **Safety** — does the rewrite keep the program's behavior? heki answers with a set of proof
   obligations, each `Established`, `Blocked` (cannot be proven here), or `Disproved` (a
   concrete counterexample shape exists).
2. **Profitability** — is the rewrite faster for every input, or only for large ones?

| Safety | What you get |
| ------ | ------------ |
| Safe (every obligation established) | A diagnostic with a fix; for a statement whose shape heki does not rewrite (a parenthesized `x = (x + [i])`, an annotated `x: list = x + [i]`), a diagnostic that says the rewrite is proven safe, to be made by hand. |
| Blocked | A diagnostic naming the first blocking obligation, no fix. |
| Disproved | Nothing: the pattern only looks like a quadratic; rewriting it would be a bug. |

| Profitability | Rules | Applied by |
| ------------- | ----- | ---------- |
| Unconditional (faster from the first iteration) | `OPT101`, `OPT102` | `heki check --fix` |
| Size-dependent (can be slower for tiny inputs or loops that run zero times) | `OPT110`, `OPT111`, `OPT120`, `OPT121`, `OPT130` | `heki check --fix --include-size-dependent`, or `--fix --profile <dump>` for hot functions |
| — | `OPT103` | suggestion only |

A size-dependent candidate is not reported at all when the code fixes its size: the container
is a display of at most 8 items that nothing grows (it may shrink: `expected.pop(0)` on
`expected = [b"a", b""]`), the loop runs a fixed number of times (a small display or `range` of
constants), the list is rebuilt in every iteration, or the loop's own test caps the accumulator
(`while len(buf) < 10`). These facts decide profitability only, never safety.

## What the analysis proves

The analysis behind the `OPT` rules is separate from the one behind the other rules and is
deliberately conservative:

- It **never trusts type annotations**: `x: list[int] = WeirdList(...)` is not a list for heki,
  because a subclass can override `__contains__` or `__add__`.
- Anything it cannot see is shared: parameters, globals, attributes, values passed to unknown
  functions, names captured by closures. A rewrite that mutates in place needs the object to be
  created in the function and never to escape.
- In-place updates need **exclusive access**: no other variable, iterator, or view may still
  refer to the old object after the statement. `for v in x: x = x + [v]` terminates, but
  `x.append(v)` would loop forever, so heki reports nothing there.
- The evaluation order and exception behavior must be preserved: `{**d, f(): g()}` becomes
  `d.update({f(): g()})`, not `d[f()] = g()`, which would call `g` first.
- Builtin names are trusted only while they are not rebound in the module (an assignment, a
  `global` store, `from m import *`, `globals()`, or a module that patches `builtins`; a `.pyi`
  stub never runs, so its `import builtins` does not count).
- Functions using `eval`, `exec`, `locals()`, `sys._getframe`, or frame attributes can change
  locals behind any analysis: their candidates are always blocked. `heki assumptions` lists such
  places in a project.

## Fix commands

```console
$ heki check --diff                              # preview (exit code 1 when something would change)
$ heki check --fix                               # apply safe, unconditional fixes
$ heki check --fix --include-size-dependent      # also the proven-safe size-dependent rewrites
$ heki check --fix --profile profile.txt         # size-dependent rewrites in hot functions only
```

`--include-size-dependent` applies the proven-safe size-dependent rewrites in every function,
with or without `--profile`; `--profile` on its own applies them only in the functions the
profile shows hot.

`--fix` applies non-overlapping fixes, re-analyzes the result, and repeats until nothing changes
(at most ten passes), so a fix that becomes provable after another one is applied too. Then it
reports what is left. A file that changed on disk meanwhile (an editor saved it) is not written:
the run reports it as an error (exit code 2) and keeps that save; run `--fix` again. Suppressed
diagnostics (`# heki: ignore[...]`) are never fixed. When the fixes of a pass would leave a file
that does not parse, none of them is applied to that file and the run exits with code 2 (a heki
bug to report). `--diff`
prints the paths every other output format prints (relative to the working directory), so
`patch -p1` applies it where heki ran.

Fixes work the same in files with `\n`, `\r\n`, or lone `\r` line breaks, and the lines they insert
use the file's own line break.

A fix keeps the file acceptable to a type checker and to isort:

- A `collections.deque` rewrite (`OPT120`, `OPT121`) also rewrites the variable's annotation
  (`q: list[int] = []` becomes `q: collections.deque[int] = collections.deque([])`); an
  annotation it cannot rewrite (`Sequence[int]`, a union) or another annotated declaration of
  the variable blocks the fix.
- `import collections` goes into the module's block of standard-library imports, in the order
  that block follows (straight imports first, or sorted by module as with
  `force-sort-within-sections`); without such a block it starts one before the other imports,
  and in a module without imports it follows the docstring and `__future__` imports, with the
  blank lines isort expects. Only imports run before it, so the rewritten function cannot run
  before `collections` is bound. It never goes above a shebang, an encoding declaration, or a
  `from __future__` import. An existing `from collections import deque` or `import
  collections` is reused only when it runs unconditionally (not under `if TYPE_CHECKING:`)
  and names the standard library's module (not `from .collections import deque`).

## Profiles

`--profile` reads a sampling profile taken offline:

- CPython 3.15's sampling profiler (`python -m profiling.sampling run --collapsed -o
  stacks.txt app.py`, or `--gecko -o profile.json`);
- py-spy (`py-spy record --format raw -o stacks.txt -- python app.py`).

A function on the stack in at least 1 % of the samples is hot. A frame counts for a function when
its name names the function on a dotted boundary (`run` or `A.run` for `A.run`, never for
`BA.run`), its file is the function's file (a basename, a relative, or an absolute path), and its
line, when the profiler recorded one, lies inside the function. A frame that fits several
functions (a plain `run` without a line in a file with two `run` methods, a `models.py` two
packages have) counts for none. `--watch` matches frames by their names only. The profile is used to promote
size-dependent fixes in hot functions and adds the sample share to the diagnostics' evidence; it
never makes an unsafe rewrite safe.
