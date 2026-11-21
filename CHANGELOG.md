# Changelog

All notable changes to heki are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). heki is experimental: until 1.0, any
release can change rules, configuration keys, diagnostics, and output formats.

## [0.2.0] - Unreleased

### Breaking changes

- Module names change for projects with a `python/` directory or a directory named after the
  project: they follow the source roots ty detects. Besides `src/`, a `python/` directory (the
  maturin layout) and a directory named after the project that holds one of the same name
  (`acme/acme/`) are source roots when they are not packages, so `python/acme/io.py` is
  `acme.io`, not `python.acme.io`, and the imports of `acme` resolve. The names in messages,
  in contract patterns, and in baseline `target`s change with them, and the default rules can
  report calls through the imports that now resolve. `src = ["src"]` restores the earlier names
  (`src = []` for a project without a `src/` layout).
- The project root of a project without a configuration file changes: it is the current
  directory when that holds the checked paths (the language server: its workspace folder), as
  Ruff and ty root it, instead of the top of the enclosing Git working tree, which stays the
  root when heki runs above it. Outside a Git working tree, `heki check pkg` is rooted in the
  current directory, not in `pkg`. Module names, the `path`s of a `--baseline` file, and the
  place of `.heki-summary-snapshot.json` move with the root; a configuration file keeps the
  root where it is.
- Warnings no longer make `heki check` fail: a run that reports only `warning` diagnostics
  (slower code, by default) prints them and exits 0; an `error` (a bug) still exits 1. The new
  `[tool.heki.lint] fail-on = "warning"` and `heki check --fail-on warning` make warnings fail
  the run again (`--fail-on error` overrides the configuration the other way), in every output
  format and in `--watch`. With a baseline, the same threshold applies to the Ratchet
  regressions; a refused `--update-baseline` and pending `--diff` changes still exit 1.

### Added

- `src` (`[tool.heki]`) lists the source roots, as Ruff's `src` and ty's `environment.root`
  do: directories relative to the project root whose files are named relative to them, so a
  subdirectory application (`src = ["backend"]`, for `backend/config/env.py` imported as
  `config.env`) and a `lib/` layout (`src = ["lib"]`) resolve their imports. It replaces the
  detected source roots.
- The JSON output gives every diagnostic its `certainty`, `"exact"` or `"heuristic"`. SARIF
  gives every rule descriptor `properties.precision` (`very-high` when all its reports are
  exact, `medium` otherwise) and the tag `heuristic` to a heuristic descriptor and to every
  heuristic result, read by GitHub code scanning. The language server sets `data.certainty` on
  every diagnostic and links its code to the rule's page (`codeDescription`). `heki rule --output-format json` gives each
  rule's `certainty`, `activation` (`default`, `opt-in`, or `contract`), `heuristic_note`, and
  `default_enabled` (whether it runs with no configuration).
- `[tool.heki.lint] heuristic`, `heki check --heuristic` / `--no-heuristic`, and
  `heki server --heuristic` / `--no-heuristic`: see the first entry under Changed. A `full` or
  `concise` run that leaves heuristic rules off, or heuristic reports out, says how many, and
  how to show them.
- Every rule page says what a report states: the premises of its claim, and which of them a
  heuristic report may not have established. `docs/precision.md` defines exact and heuristic
  reports, the assumptions R1-R7 exact claims rest on, how a rule is certified, and what each
  rule measured.
- `EFF111` (`async-possibly-blocking-call`) and `EFF113` (`loop-possible-db-query`), heuristic: the
  reports `EFF101` and `EFF103` made before, where the new exact claims are not established. See the
  first entry under Changed.
- `sqlite3.connect` of an in-memory database (`":memory:"`, `file::memory:`, `mode=memory`) does not
  wait, and neither do the statements on that connection. A connection waits only where heki
  establishes that it was opened on a file (a literal or a module constant without an in-memory
  form, or a path built from such parts and `__file__`); any other, one opened from a parameter, an
  f-string, or an environment lookup, or only named by an annotation, may be in memory, so its waits
  are not established (`EFF111`; its statements still count for `EFF103`). Reports name such
  connections' methods by the library's API (`sqlite3.Connection.execute`). A pragma that only reads
  or sets an option of the connection (`PRAGMA busy_timeout = 5000`) does not wait.
- Test code, which `lint.relax-async-in-tests` leaves out of `EFF101`, `EFF111`, `EFF105`, and
  `EFF106`, is what pytest discovers: `test_*` functions and the functions nested in them,
  `test_*.py` and `*_test.py` files at any depth, and files under `test/` or `tests/`.
- The ecosystem catalog says when a call waits. A blocking effect waits for input or output or for
  another thread (`blocking_sync`), which can have done its part already. Every wait entry lists the
  arguments that end it at once: `time.sleep(0)`, `Popen.wait(timeout=0)`, a numeric host or
  `localhost` for `socket.getaddrinfo` and `socket.gethostbyname`, a `data:` URL for `urlopen`; no
  rule reports those calls as waiting. Database entries carry `emits` (`always`,
  `on-identity-map-miss` for `Session.get`, `if-pending` for `Session.flush` / `commit` and a
  driver's `commit()` / `rollback()`, `unless-cached` for Django's `QuerySet.count()` / `exists()` /
  `first()` / `last()`, `unless-connected` for a pool checkout), and context managers
  `exit_suppresses` (`false` for files, locks, HTTP sessions, and database connections; `true` for
  `contextlib.suppress`), and calls whose results the environment sizes `items` (`rows`, `lines`,
  `entries`, `text`). New argument tests: `one_of` and `numeric_address`.
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
- SARIF output lists every file heki could not read or parse as an `error` notification of the
  run (`invocations[0].toolExecutionNotifications`, with its location), and then sets
  `executionSuccessful` to `false`; the results of the other files are kept. JSON output is
  unchanged: the errors stay on stderr, and the exit code is still `2`.
- `call-owners` takes `subclasses = false`: a class target then means constructing exactly the
  class, so raising a subclass of a guarded exception class is no `MUT204` report. `Class.method`
  targets keep covering overrides.
- Contract names take a module part, `module:qualname`, which names definitions of one module
  without its submodules: `pkg:*` names what `pkg/__init__.py` defines, where `pkg.*` also covers
  `pkg.sub.user.sub_user`. The module part matches the whole module name, exactly or as a glob
  (`pkg.*:run`), and works in every key that names project functions or classes, the class of
  `Class.attr` targets included (`pkg.model:DataModel.fields`).
- Effect boundaries check what a scope reads of the environment it runs in. `call-targets` name
  functions and classes outside the project by their full dotted name (`os.getenv`, `platform.*`),
  and `read-targets` module attributes (`sys.platform`, `os.environ`, also read through
  `from sys import platform`); a dotted suffix or a leading glob still names project functions only.
  The `forbid` token `reads-environment` forbids the calls and reads the ecosystem catalog tags with
  the new `environment` key (`os.getenv`, `platform.system`, `importlib.util.find_spec`,
  `importlib.metadata.version`, `os.environ`, `sys.version_info`, ...), in one report per scope
  function, and with `import-probes = "report"` the imports inside a `try` whose `except` clause
  names `ImportError` or `ModuleNotFoundError`.
- `call-owners` and effect boundaries (for `calls`) take `references = "calls"`: a reference handing
  a target on to be called later counts as a call of it, reported as "`f` hands on
  `pkg.core.decide`": passed to a call (`partial(decide)`, `field(default_factory=Rule)`), stored,
  returned, yielded, raised, or held by such a display, also through an import or a module alias.
  Uses that only name a class (`isinstance`, `except`, a base class, `cast`, a typing subscript, an
  annotation, a comparison) are no reference.

### Changed

- `DATA101` marks a report exact when every premise is established (the update returns on every
  iteration that completes, every path around the back edge carries it, the loop steps through a
  parameter, the rows of a query, the lines of a file, or directory entries, and its trip count is
  not fixed) and heuristic otherwise; its message names the loop's origin and drops "(quadratic
  copying)".
- `EFF101` and `EFF103` state exact claims, each premise established: `EFF101` that whenever the
  reported call returns, a call that waits for the network, a database, a child process, or a sleep
  has run on the event loop's thread, through functions that each make the next call on every path
  on which they return (exception edges included), with every implementation of each call followed
  (protocol receivers are not); `EFF103` that every iteration of a loop over a parameter, the rows
  of a query, the lines of a file, or the entries of a directory sends a database statement. Their
  messages state the claim and its witnesses only: a call whose implementations reach different
  statements names each with the function that makes it, and relates each where it is, and "N+1" and
  "batch the queries" are gone. Everything else they reported is `EFF111` / `EFF113`. Waits on
  another thread (`Thread.join()`, `Event.wait()`, locks, `Queue.get()`, `put()` on a bounded queue,
  `Future.result()`) are `EFF111` only. Both are certified exact, as is `DATA101`
  (`docs/precision.md` has their census and holdout counts), and run by default.
- Only exact reports are shown by default. A report is *exact* when every premise of its claim was
  established and its rule was certified (a false exact report is a bug), and *heuristic* when a
  premise can be open, so that it can be wrong about the code. A heuristic rule runs when `select`
  or `extend-select` names its code or name, and with the new `[tool.heki.lint] heuristic = true`
  (`--heuristic`) also by default and for every prefix and `ALL`, as Ruff's `preview` does. An exact
  rule's heuristic reports (an `OPT` report whose rewrite is not proven) are shown under the same
  conditions. A contract rule runs when its contract is declared, whatever its certainty. The proven
  tier of the eight `OPT` rewrite rules (`OPT101`-`OPT103`, `OPT110`, `OPT111`, `OPT120`, `OPT121`,
  `OPT130`) is exact: a census of every one of its 234 reports on the Stage 0-A corpus, and a check
  of the 235 of one run on a frozen holdout corpus of 100 packages, found none wrong. `OPT113`'s
  exact tier is certified too (8 reports in the census and 6 in the holdout run, none wrong) and
  runs by default. `EFF101`, `EFF103`, and `DATA101` are exact too (`docs/precision.md` has their
  census and holdout counts). `heki check` with no configuration runs the exact rules (`EFF101`,
  `EFF103`, `DATA101`, and the proven tier of the `OPT` rules above) and the contract rules and
  shows the reports whose rewrite is proven safe, and `heki check --fix` applies the proven fixes of
  `OPT101` and `OPT102`. `MUT202`, which also reports `@lru_cache` / `@cache` results without a
  contract, is selected like the other heuristic rules, so a `readonly-returns` contract needs it
  named or `lint.heuristic = true`. `lint.heuristic = true` restores the rules of 0.1; baselines
  keep their keys.
- Every output labels heuristic reports: `[error, heuristic]` in `full`,
  `EFF102 [heuristic] ...` in `concise`, `title=heki EFF102 (heuristic)` in `github`, and
  ` [heuristic]` after the language server's message. `heki rule --all` lists each rule's
  certainty and when it runs (`on`, `off`, `opt-in`, or `contract`) instead of
  `default=true` / `false`.
- A rule's default severity follows what happens in the code it reports, not its family: a bug
  (a wrong result, an exception, a lost update, a deadlock, or a broken contract) is an
  `error`, slower code a `warning`. `EFF101`, `EFF102`, `EFF103`, `EFF105`, `DATA101`, and the
  `OPT` rules default to `warning` instead of `error`; the other rules stay `error`. This
  changes the output label, the SARIF level, the language server's severity, and the exit code
  (see Breaking changes: a warning no longer fails the run). `[tool.heki.lint.severity]` still
  overrides it. Each rule page shows its default severity, and `heki rule --output-format json`
  gives each rule's `consequence` (`bug` or `slowdown`).
- `OPT` messages state their claim and nothing else: what the statement does each time it runs in
  the loop (`` `rows = rows + [...]` copies the whole list each time it runs in the loop ``),
  without "on every iteration", which heki does not establish, and without cost estimates
  ("(quadratic)", "(O(n) per iteration)", "(O(n²))", "(O(n·m))"). `OPT110` names the container it
  scans (a list or a tuple), and `OPT130` says the constructor builds a new container for the
  membership test, not "the same container", which only a proven rewrite establishes. The rule pages
  state the claim the same way: its premises are the container's builtin type, the loop, and, for a
  rule's exact tier, the proven rewrite; no size is claimed. A proven report of a size-dependent
  `OPT` rule in a loop whose trip count the code fixes or caps (`range(50)`, `rows[:100]`, a display
  of 20 items, a length test such as `if len(rows) > 100: raise ...`) is heuristic: true, but no
  evidence either way.
- `OPT113` reports carry a tier. A report is in the exact tier when each key the inner loop loads is
  a plain attribute of an item whose class heki knows (a dataclass, `attrs`, or `NamedTuple` field,
  or what the class stores, with no property or other descriptor of that name in the project) or an
  item of a builtin container, both loops step through the function's inputs (parameters, or
  attributes or items of them), an inner item whose key differs runs nothing but the comparison (no
  statement before the test or after the `if`, no earlier `and` operand, and an inner iterable that
  is a builtin container by its type or construction, not a value of unknown type, a generator,
  `map`, `filter`, or another call), and the inner loop steps through the same collection, with the
  same keys, on every outer iteration; a loop whose trip count the code fixes or caps has no exact
  report. Its message states that claim ("for each item of `orders`, the loop over `customers`
  compares `order.customer_id == customer.id` and does nothing else for an item that does not
  match"); the other reports are heuristic. The help names a `set` or `dict` only for keys known to
  be hashable, and an index built before the outer loop only for an inner collection that stays the
  same; otherwise it states the condition ("if the keys are hashable, ..."). For a mapping the inner
  loop steps through by its own keys, it says to look each outer key up in the mapping.
- `OPT103`'s help for a `tuple` accumulator says to extend a list and convert it once with
  `tuple(...)`, not to join the pieces.
- `EFF102` and `EFF103` also report the function a builtin calls once per item: the function
  handed to `map`, `filter`, `itertools.starmap`, `itertools.filterfalse`, and
  `functools.reduce`, and the `key` of `sorted`, `list.sort`, `min`, `max`, and
  `itertools.groupby` (`sorted(users, key=lambda u: score(conn, u))`), when the items are not a
  fixed number. Its body runs at the call for every rule, so `EFF101` sees its blocking calls
  too. A `functools.partial` handed to such a builtin is followed to the function it calls.

### Fixed

- An f-string's replacement fields are reads of their values: each is converted and formatted
  where it stands, as CPython does (which may run the value's `__str__`, `__repr__`, or
  `__format__`), also in a format specification and in a template string's specifications,
  and the f-string is computed from them. The Soundness lane no longer proves a
  `collections.deque` rewrite safe for a list an f-string shows (`OPT120` / `OPT121`: the
  text of a `deque` differs), and `OPT113` no longer reports a traversal whose inner collection
  is computed from the outer element through an f-string (`index[f"{name}"]`).
- A project module that cannot be read or does not parse still owns its module name: the names
  under it resolve to nothing, never to a module of the same name the catalog lists. A broken
  `requests.py` in the project no longer makes `requests.get` a blocking call. The language
  server and `heki check --watch` keep such a file as the same unknown module while it does not
  parse.
- `heki check --fix` no longer rewrites `s = s | t` to `s |= t` (`OPT102`) where the set's
  iteration order can be seen afterwards. Growing a set in place can order the same elements
  differently from copying it, so a function that iterated, converted, formatted, or returned the
  set could return another result after the fix. The set form is fixed only when the set is just
  tested afterwards (`in`, `len`, truthiness, `==`); its other reports say
  `[not auto-fixable: SetOrderUnobserved(...)]`. The dict forms keep their insertion order and
  are fixed as before.
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
- `EFF111` no longer reports `put()` on a `queue.Queue`, `LifoQueue`, or `PriorityQueue` that is not
  known to be bounded: built without a positive `maxsize`, or one heki cannot follow to where it is
  built. A `put` waits only while its queue is full. heki follows the queue into local variables,
  the attributes its class family stores, module globals, and parameters whose callers bind known
  queues; an `effect-boundaries` contract forbidding `blocking-sync` counts a `put` the same way.
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
- `MUT107` (`hash-attr-mutation`) no longer reports a write that cannot change a hash a `set` or
  `dict` has filed: an in-place change (an item store, `append`) of an attribute `__hash__`
  hands whole to `hash()` (`hash(self.items)`: a list or `bytearray` there cannot be hashed, and
  a project value answers for its own hash), a writer whose class defines its own `__hash__`
  (or `__eq__` alone) below the hash's class, and a private helper that only `__init__` and
  other such helpers call (`__init__` -> `self._load()` -> `self._decode()`). A writer in a
  subclass with its own `__hash__` is now reported against that hash.
- `MUT202` (`readonly-return-mutation`) no longer reports a shared object filling its own memo
  (a cache-named attribute changed in place, a store under `if k not in ...`, `setdefault`, a
  lazy initialization), also in a method or helper it is handed to, nor a change of a copy a
  project class's own `copy()` makes (it copies as deeply as its body does).
- `MUT202` reports a change below a shallow copy made with `dict(...)`, `list(...)`,
  `tuple(...)`, `set(...)`, `frozenset(...)`, or a display that only unpacks the shared value
  (`{**shared}`, `[*rows]`), as for `.copy()`; they shared the values below and were taken as new
  objects. A helper changing an attribute's object below its own shallow copy of a parameter
  (`draft = copy.copy(post); draft.tags.append(t)`) now changes that parameter (`MUT202`,
  `MUT108`, `EFF201` `mutates-params`).
- `DATA101` no longer reports a value rebuilt from arithmetic on the previous one, such as a
  recursion that computes `x = np.concatenate((x - np.outer(d, a), [a]))` or a fixed-size
  quaternion product: every entry of the new array changed, so there are no unchanged parts to
  collect and combine once. A list that holds the previous value (`pd.concat([df] + parts)`)
  is still reported.
- A helper taking a path or a buffer is no longer taken to open the file (or fetch the URL) when
  its caller hands it a buffer or `None` (`EFF101`, `EFF102`, `EFF103`, `EFF105`, `EFF106`,
  `DATA101`): `hasattr(buf, "write")` and `isinstance` with the `io` classes are guards too, an
  `io.StringIO` / `io.BytesIO` the caller constructs, a display, or a comprehension decides
  them, and a test of a parameter the helper rebinds on some paths (`if buf is None: buf =
  StringIO()`) is decided per path. `EFF201` still reads no guards.
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
- A file that is one definition, class, lambda, or generator expression with nothing after it
  (no final newline) is checked: its function was taken for the module body, which has the same
  span, so it was never analyzed, and `heki check` panicked on such a generator expression
  (`(x async for x in xs)`).
- `heki check` no longer overflows its stack on a loop no path enters (after a `return`,
  `raise`, or `break`) that computes a value from itself (`d = d.x`, `d = d[mask]`): the value
  was defined by itself, and the walks along definitions went around it forever.
- A module-level name bound once to another object by name is that object, in its module and
  imported from it, unless another module rebinds it: `_set = object.__setattr__` makes
  `_set(node, "kind", v)` a `MUT201` write, and `sleep = time.sleep` makes `sleep(1)` in an
  `async def` an `EFF101` report. Any other global a module binds is never the builtin of the same
  name: after `open = functools.partial(...)`, `open(path)` is no blocking `builtins.open`
  (`EFF105`), and a call of it is unresolved unless a `[[tool.heki.ecosystem]]` entry declares it. A
  name heki does not see the module bind (a compiled submodule, a star import of one) keeps what the
  catalog says of it (`mypkg._native.read`).
- A method that no project class of the receiver's MRO defines is no longer taken from a base
  outside the project that is not known to define it (`abc.ABC._copy_model_type`,
  `typing.Generic.result_type`): the call is unresolved, as a call heki cannot follow. Special
  methods, public methods of builtin types, and the members the catalog lists still come from the
  base, when no base heki knows nothing of comes before it.
- `getattr(obj, "attr"[, default])` with a constant name is a load of the attribute for the write
  contracts, also behind a walrus
  (`if (extras := getattr(self, "extras", None)) is not None: extras["k"] = 2` is a `MUT201` write
  of `Node.extras["k"]`).
- A method, property, or function that returns an attribute of an argument on every `return`
  (`return self.items`) hands out that object: `node.members().append(1)` and `node.view.append(1)`
  are `MUT201` writes of `Node.items`. It does so on a class whose reads run it: not through a
  `cached_property` (an assignment can replace what the instance keeps), not on a member of a union,
  or a class, whose own attribute of the name comes first or is stored on the class outside its
  body, and not for a function that stores the attribute itself
  (`items = self.items; self.items = []; return items`) or a keyword-only parameter.
- `object.__new__(Node)`, also through an alias, and `Node.__new__(Node)` where no class of the MRO
  defines `__new__` construct a `Node`: the new object has its class, and with
  `fresh-objects = "allow"` a helper filling it is no `MUT201` report.
- A glob in the last segment of a `call-owners` target or an effect boundary's `call-targets`
  matches within the function, method, or class name, as the attribute of a `Class.attr` target
  does: `pkg.*._*` no longer names every function of a private module (`pkg.parser._concern.typed`),
  nor `pkg.api_*` the functions of `pkg.api_helpers`, so such functions are no targets that may call
  the others, and their calls are reported.
- A report of a glob target names the attribute, function, or class it matched
  (`reads `pkg.types.DataType._flag``, was `reads `pkg.*._*``), and one report standing for several
  members lists the others; the diagnostic target, and so the baseline key, stays the configured
  text.
- `call-owners` contracts (`MUT204`) and the `calls` of effect boundaries look a call's targets up
  from the callee's names instead of trying every target, and `MUT204` indexes the targets of all
  its contracts together: a contract of 1,300 targets on a 200,000-line project took about 30 s and
  now takes about 1 s, with the same reports.
- A read or a call through a value typed with a project `Protocol` that declares the member is
  judged by the protocol, the type the code uses it through: `def f(h: Host): h._m` is no `MUT203`
  read of `_m` of the classes implementing `Host`, `h._helper()` no `MUT204` call of theirs, and
  what an implementation's private method reads or calls inside stays with it instead of being
  handed to the caller (also when other callers call it through the class and take it over). A
  member the protocol does not declare is still one of the implementations', and a write through the
  protocol still writes their attribute.

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
