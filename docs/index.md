# heki

heki (壁, "wall") is an effect and alias checker for Python, written in Rust. It is the third
layer of static analysis next to a linter such as [Ruff](https://docs.astral.sh/ruff/) and a
type checker such as [ty](https://github.com/astral-sh/ty): instead of style or types, it
follows **what code does** across functions and modules — which objects it mutates, which
references it shares, and which blocking, network, or database work it reaches.

```console
$ heki check
app/api.py:5:12: EFF101 (async-blocking-call) [error] `async def handler` calls `app.users.load_user`, which reaches a blocking call `requests.get` (blocking_net) on the event loop; offload it (`asyncio.to_thread`) or use an async API
  |
5 |     user = load_user(user_id)
  |            ^
  = related: app/api.py:5:12: `handler` calls `app.users.load_user`
  = related: app/users.py:5:12: `load_user` calls `requests.get`

app/report.py:4:9: OPT101 (list-quadratic-concat) [error] `rows = rows + [...]` copies the whole list on every iteration (quadratic); append in place with `rows.append(...)` / `rows.extend(...)` [safe fix: `heki check --fix`]
  |
4 |         rows = rows + [item]
  |         ^

Found 2 diagnostic(s) in 4 file(s).
```

## What it finds

- **Shared state** (`MUT`): a `@cached_property` left stale after the attribute it reads
  changes, an attribute `__hash__` reads changing after the object was hashed, a single-pass
  iterator consumed twice, writes and reads of state another component owns (declared once in
  configuration), mutation of objects returned by `@lru_cache`, and side effects a public
  function gained since a saved snapshot.
- **Execution effects** (`EFF`, `DATA`): blocking calls reachable from `async def` through any
  number of helpers, HTTP / database clients created inside loops, N+1 queries, `await` or
  network I/O while a `threading` lock or a database transaction is held, tasks that only a
  weak reference keeps alive, effects a declared boundary forbids, and quadratic DataFrame /
  array accumulation.
- **Complexity rewrites** (`OPT`): quadratic list, dict, set, bytes, and queue patterns. heki
  rewrites them only when its alias analysis **proves** the rewrite preserves behavior; when it
  cannot, the diagnostic names the proof obligation that failed. Linear `list` lookups and
  nested-loop joins inside loops are reported as suggestions.

See the [rules reference](rules/index.md) for all 25 rules.

## Why another tool

These bugs cross function and module boundaries: the blocking call is three helpers away from
the `async def`, the cache is invalidated in a different file than the one that writes the
attribute. heki builds an SSA intermediate representation for every function, summarizes each
function's effects, and propagates the summaries over the project's call graph until they reach
a fixpoint. The summaries are small, so the whole of a 200,000-line project is checked in
about a second, and the editor integration re-checks only the file you edit.

The rewriting rules use a second, independent analysis that never trusts type annotations and
treats everything it cannot see as shared: a rewrite is offered only with a proof, so
`heki check --fix` does not change what your program does.

## Next steps

- [Install heki](installation.md) and [run it on a project](getting-started.md).
- [Configure](configuration.md) rule selection, ownership contracts, and the Python version.
- Read how [fixes are proven safe](fixes.md).
- [Adopt it gradually](adoption.md) with a Ratchet baseline in CI.
- Use it from [your editor](lsp.md) or [a coding agent](coding-agents.md).
