# Changelog

All notable changes to heki are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). heki is experimental: until 1.0, any
release can change rules, configuration keys, diagnostics, and output formats.

## [0.2.0] - Unreleased

### Fixed

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
