# Changelog

All notable changes to heki are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). heki is experimental: until 1.0, any
release can change rules, configuration keys, diagnostics, and output formats.

## [0.2.0] - Unreleased

### Added

- `MUT206` (`unresolved-guarded-read`) and `MUT207` (`unresolved-guarded-call`): `read-owners`
  and `call-owners` contracts take `unresolved = "report"`, as `write-owners` does, and then
  report the reads of an attribute and the calls of a method named like a target on an object of
  unknown class (an unannotated parameter, a loop variable, an untyped call result, `Any`,
  `object`, or a protocol whose implementations heki cannot list), unless every caller binds an
  instance of a project class outside the targets' classes (for a call target naming no class,
  `reset`, one whose MRO finds no `reset` the target names).
- Contract keys and dynamic names follow module-level string constants bound once in their
  module, and class constants declared `ClassVar` or `Final` and bound once, read on the class,
  an instance, or `cls`; a constant without `Final` stops being one when another module rebinds
  or deletes it (`keys.KEY = "x"`, `Keys.KEY = "x"`), and a class constant when its own module
  stores it on the class or an instance outside the class body, or when a subclass's MRO finds
  another definition. A `Final` field of a dataclass is no constant on an instance.
- `MUT112` (`conditional-ownership-mutation`, opt-in, experimental) reports an in-place change of
  a container that is a new object on one path and borrowed state on another
  (`items = given or []`, `d.get(k, {})`, a callee returning a memo or its argument), made
  directly, through a helper, or in a closure, and a store of such a value in `self.<attr>` that a
  method of the class family changes in place.
- `MUT113` (`iterated-container-mutation`, opt-in, experimental) reports a dict, set, or deque
  of `self` changed in place while a method iterates it: by another method of the class while a
  generator is suspended inside the iteration, or by a function the loop's body calls; the next
  step raises `RuntimeError`.

### Changed

- The `OPT` rules default to severity `warning` instead of `error`: they report slower code,
  not wrong code. The exit code does not change (a warning still makes `heki check` exit 1),
  only the output label and the SARIF level. `[tool.heki.lint.severity]` still overrides it.
- `EFF102` and `EFF103` also report the function a builtin calls once per item: the function
  handed to `map`, `filter`, `itertools.starmap`, `itertools.filterfalse`, and
  `functools.reduce`, and the `key` of `sorted`, `list.sort`, `min`, `max`, and
  `itertools.groupby` (`sorted(users, key=lambda u: score(conn, u))`), when the items are not a
  fixed number. Its body runs at the call for every rule, so `EFF101` sees its blocking calls
  too. A `functools.partial` handed to such a builtin is followed to the function it calls.

### Fixed

- `EFF102` and `EFF103` no longer count a `functools.cached_property` getter as running on
  every read: it runs once per instance, like a lazy initialization of `self`, also when
  `cached_property` comes from a module that re-exports it (`kombu.utils.cached_property`). A
  loop reading it on an instance the loop does not construct is not reported.
- `EFF102` and `EFF103` no longer report a client or query created only while a local variable
  the loop carries is unset (`if conn is None: conn = connect()`), only while a first-iteration
  flag holds (`if first: ...; first = False`), or only while a module global is unset
  (`global _session`).
- `EFF102` and `EFF103` no longer report a client or query that every way on from leaves the
  loop (followed by `return`, `break`, or `raise`): it runs once per run of the loop.
- `EFF101` no longer reports `result()` or `exception()` of a `concurrent.futures.Future` known
  to be done: under `if f.done():`, or once `await asyncio.wrap_future(f)` has returned.
- `EFF107` (`unretained-async-task`, opt-in) no longer reports a task whose bound method is
  passed to a call or stored, as in `self.addCleanup(task.cancel)` or
  `loop.call_later(1, task.cancel)`: the bound method holds the task.
- `EFF107` no longer reports a call that makes no new task because its argument is known not
  to be a coroutine: the result of a plain `def` or a generator function, such as a method
  returning a future (`asyncio.ensure_future(event.wait())`), or a future from
  `loop.run_in_executor`, `loop.create_future`, `asyncio.gather`, `asyncio.wrap_future`, or an
  annotation. The `coro=` and `coro_or_future=` keyword forms are checked the same way.
- Calling an `async def` gives a coroutine: a method called on the result without `await` no
  longer resolves on the function's return annotation, so the rules stop following it there.
  `await` and `async with` still give the annotated type, and awaiting a future gives what it
  holds, not the future.
- A value typed with a project `Protocol` and handed to a helper that does not type its
  parameter is an instance of the protocol and of its implementations: the helper's writes are
  `MUT201` writes of those classes, and `MUT205` reports a write to a protocol whose
  implementations heki cannot list. `MUT109` matches such a write against the targets of each
  class once.
- A call target naming no class (`reset`) gives `MUT204` the initialization of the classes
  defining the methods it names: `self.reset()` in their `__init__` is no report.
- `MUT205` uses the class an `isinstance` test around a write narrows its object to.
- A list or set comprehension that collects the results of a call no longer binds each result to
  the call's first parameter: `[load(graph, raw) for raw in items]`, with `load` returning a
  `readonly-returns` value, is no `MUT202` report.
- A `return` no run reaches (after a `while True:` loop that no `break` leaves, or under
  `if False:`) is not what the function returns: a wrapper returning a cached or
  `readonly-returns` value only there is no producer, and changing its result is no `MUT202`
  report.
- `MUT112` no longer counts an attribute of an object the function makes itself
  (`visitor = Visitor()`, then `visitor.errors`) as borrowed state, nor a value that a flag
  parameter copies (`if not in_place: x = x.copy()`).
- `MUT113` no longer counts the changes a function makes to a container it has just bound to the
  attribute (`self.xref = {}`, then `self.xref[k] = v`), nor a store or a `defaultdict` read under
  a key the container holds (past `if self.items.get(k) is None: raise KeyError(k)`, inside
  `if k in self.items:` at any depth).
- Sizes the code states are no longer taken as data-sized (`EFF102`, `EFF103`, `DATA101`, and
  the `OPT` rules): a builtin constructor of a fixed iterable (`list(range(7))`,
  `tuple(".;:")`), a string constant, a comprehension over fixed iterables or a new list filled
  a fixed number of times (`[c for c in LABELS if c in seen]`, `ids.append(...)` for each of a
  few cards), a slice with constant bounds (`cases[:3]`), and an iterable a length test on every
  path to the loop caps (`if not 1 <= len(cards) <= 6: raise ...`, `assert len(xs) <= 3`). A
  module body's loops and comprehensions over imported constants, and generator expressions
  and comprehensions before 3.12 over constants of other modules, get the same sizes as the
  code of a function.
- A module-level tuple or string stays fixed when other code puts it in a list or passes it to
  a function (an `EFF103` report on a loop of `CREATE TABLE` statements).
- `DATA101` no longer reports an accumulation in a loop whose trip count the code fixes
  (`for is_p_wave in [False, True]`, `range(4)`).
- Polls and retries are no longer taken for loops over data (`EFF102`, `EFF103`, and the
  size-dependent rules): a loop that sleeps or waits with a timeout on every path back to its
  test (`while not job_done(): time.sleep(5)`, `for _ in range(30): ... sleep(1)`), a `while`
  loop over a counter the code bounds (`i = 1` ... `while i < 6:` ... `i += 1`), a loop until a
  stop event is set (`while not stop_event.is_set():`), and a retry over a tuple target naming
  the attempt (`for attempt, proxy in enumerate(proxies):`).
- `EFF103` no longer reports DDL built at run time or taken from constants: an f-string, `%`,
  or `.format()` starting with `CREATE` / `ALTER` / `DROP` / `PRAGMA`, SQLAlchemy's `text()` of
  one, and the statements of a module's schema constants (a tuple of statements, the pieces of
  `SCHEMA.split(";")` or `SCHEMA.splitlines()`).
- `EFF101` no longer reports `socket.getaddrinfo` with the `AI_NUMERICHOST` flag, which parses
  a numeric address and never asks a resolver.

## [0.1.2] - 2026-11-21

### Fixed

- The source distribution carries the pages, changelog, and documentation generator its tests
  read, so `cargo test --workspace` passes in the unpacked archive (the 0.1.1 release stopped
  there before publishing to PyPI).

## [0.1.1] - 2026-11-21

### Changed

- `MUT111` and `heki check --profile` are experimental: `MUT111` reports the readers of the stale
  writes `MUT101` finds, so it shares `MUT101`'s precision, and `--profile` reads the output of
  CPython 3.15's sampling profiler, whose format can still change. The new
  [Experimental Features](https://heki.koxudaxi.dev/experimental/) page lists every experimental
  feature, and `heki rule` says when a rule is experimental.
- Small displays and loop bounds now go up to 16 items (8 before), so loops whose fixed trip
  count is 9 to 16 and containers of 9 to 16 items no longer get size-dependent reports
  (`OPT103`, `OPT110`, `OPT111`, `OPT112`, `OPT113`, `OPT120`, `OPT121`, `OPT130`, `EFF102`,
  `EFF103`).

### Fixed

- The errors of files that cannot be read or parsed (and of files that changed on disk during
  `--fix`) print the paths diagnostics print: relative to the working directory, without a
  leading `./` (`error: pkg/broken.py:1:7: syntax error: ...`, was `./pkg/broken.py`), also
  in `--watch`, `heki snapshot`, and `heki assumptions`. The hazards `heki assumptions` lists
  print those paths too, also for a directory given as an absolute path.
- On Windows, `json` output, errors, and `heki assumptions` write paths with `/` separators,
  as the other output formats do (`pkg/a.py`, was `pkg\a.py`), and the paths heki resolves
  (the project root and the files under it that messages name, such as a configured baseline,
  and the language server's paths in its log) no longer start with `\\?\`.
- On Windows, a checkout keeps the repository's LF line breaks (`.gitattributes`), so a heki
  built there prints the rule documentation (`heki rule`) as written, without CR characters.
- The language server, when the client names no workspace folder, indexes the working directory
  in the form it compares document paths in; on Windows a document could otherwise be missed
  as part of the project.
- `OPT113` (`nested-loop-equijoin`) no longer reports a join when the code fixes the size of
  either loop, such as a module tuple of operation names or `range(10)`: the cost then grows
  with the other side only. Its help for the grouped index now says to read each outer key with
  `.get(key, [])`, so that a key with no element keeps its empty list.

## [0.1.0] - 2026-11-21

The first public preview.

### Added

- `heki check` checks a project, or the files and directories given, with Ruff-style rule
  selection (`select`, `extend-select`, `ignore`, `per-file-ignores`), suppression comments
  (`# heki: ignore[CODE]`, and `# noqa: CODE` for compatibility), output formats (`full`,
  `concise`, `json`, `github`, `sarif`; paths relative to the working directory, as in Ruff),
  and exit codes `0` / `1` / `2`. The target Python version (`py311` to `py315`) comes from
  `requires-python` or `--target-version`, and `--watch` re-checks on every change.
- 31 rules in four families:
  - Mutation and state: `MUT107`, `MUT109`, `MUT110`, `MUT201`, `MUT202`, `MUT203`, `MUT204`,
    `MUT205`, and the opt-in `MUT103`, `MUT106`, `MUT108`, and `MUT111`, and `MUT101`
    (experimental: its precision outside a few code bases is still low, and it may change).
  - Execution effects: `EFF101`, `EFF102`, `EFF103`, `EFF201`, and the opt-in `EFF105`,
    `EFF106`, and `EFF107`.
  - Data pipelines: `DATA101`.
  - Complexity rewrites: `OPT101`, `OPT102`, `OPT103`, `OPT110`, `OPT111`, `OPT112`, `OPT113`,
    `OPT120`, `OPT121`, and `OPT130`.
- `MUT111` (`cached-property-stale-read`, opt-in) reports each function that reads a
  `@cached_property` that a method can leave stale (a `MUT101` finding), with the writers as
  related locations; under a Ratchet baseline, a new reader of a stale value is one new
  diagnostic.
- Proven-safe rewrites: `heki check --fix` applies the unconditional `OPT101` / `OPT102` fixes,
  `--include-size-dependent` adds the size-dependent ones in every function, `--profile` alone
  applies them in the hot functions of a sampling profile (CPython 3.15's profiler or py-spy),
  and `--diff` previews them. A fix that turns a list into a `collections.deque` also rewrites its `list` annotation
  and adds the import in sorted order to the standard-library imports.
- Contracts in `[tool.heki]`: `write-owners` (`MUT201`), `read-owners` (`MUT203`),
  `call-owners` (`MUT204`), `readonly-returns` (`MUT202`, followed through the functions that
  return a producer's result), `invalidation` (`MUT109`), and `effect-boundaries` (`EFF201`, with
  `reads-targets`, `calls`, and `allow-writes` / `write-universe`); `forbidden-writers` /
  `forbidden-readers` / `forbidden-callers` restrict an owner contract to a layer. Targets can
  name container entries (`Class.attr["key"]`) and use module-level `Final` string constants as
  keys; `fresh-objects = "allow" | "report"` decides whether a write that constructs its object
  counts (allowed by default), and `unresolved = "report"` makes a `write-owners` contract count
  the writes heki cannot tie to a class (`MUT205`).
- A catalog of library behavior (blocking network, database, and file calls, their async
  counterparts, offloading helpers, locks and transactions) for the standard library, requests,
  httpx, urllib3, aiohttp, SQLAlchemy, SQLModel, psycopg, psycopg2, PyMySQL, Django, redis,
  pymongo, boto3, pandas, numpy, the offloading helpers of anyio, trio, Starlette, and FastAPI,
  and asgiref (`sync_to_async`), asyncer (`asyncify`), and Django Channels
  (`database_sync_to_async`), also followed as decorators; extensible with
  `[[tool.heki.ecosystem]]`.
- Generator functions are followed where their generators are iterated (`for`, comprehensions,
  unpacking, `yield from`, consuming calls, lazy wrappers, project functions iterating them),
  generators returned by helpers included; a generator handed to a framework
  (`StreamingResponse(rows())`) is not reported.
- Receiver typing for the call graph: container item types (`list[Model]`), return annotations
  along the MRO, `Self`, copies, `functools.partial`, `typing.cast`, unions of up to three
  classes, `isinstance` narrowing, and the structural implementations of project `Protocol`s.
- Effect guards: a branch the caller's arguments switch off is not followed
  (`load(name, cache=False)`), and a client created by a lazy initialization counts once per
  object, not once per loop iteration.
- Ratchet baselines (`--baseline`, `--update-baseline`), keyed by file, function, rule, and
  target instead of line numbers, that count violation sites (`version = 2`); older files keep
  counting reports until `--update-baseline`.
- Every diagnostic is one sentence saying what happens, and what to do about it is a separate
  help text: a `= help:` line in the `full` output, a `help` field in JSON (`null` when a rule
  has no advice for the report), `properties.help` in SARIF, and the last related-information
  entry in the language server; `concise` and `github` print the message only. A contract's
  `message` is the help of its reports.
- `heki snapshot` saves the effect summaries of public functions for `MUT103`.
- `heki server` (alias `heki lsp`), an incremental language server: call chains as related
  information, quick fixes for the proven-safe rewrites, and suppression quick fixes.
- `heki rule` (alias `heki explain`) explains a rule; `heki assumptions` lists the functions the
  rewrite proofs cannot see through (`eval`, `exec`, `locals()`, frame introspection, and
  functions too large to model).
- A size budget per function bounds heki's memory on any input: a function whose model would
  pass it is left unmodeled (no rewrite in it, its body one call heki cannot resolve), and a
  function whose rewrite proof would be too large gets no `OPT` finding.
- Wheels for Linux (x86_64 and aarch64 with glibc, x86_64 with musl), macOS (arm64 and x86_64),
  and Windows (x86_64), and a source distribution.

[0.2.0]: https://github.com/koxudaxi/heki/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/koxudaxi/heki/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/koxudaxi/heki/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/koxudaxi/heki/releases/tag/v0.1.0
