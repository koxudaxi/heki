# heki

heki (Japanese for "wall") is an experimental effect and alias checker for Python, written in
Rust. It is the third layer of static analysis next to a linter such as Ruff and a type checker
such as ty: it follows **what code does** across functions and modules — which objects it
mutates, which references it shares, and which blocking, network, or database work it reaches.

> **Experimental.** heki is under active development: rules, configuration keys, diagnostics,
> and output formats can change between releases without a deprecation period. Try it on your
> code and report what it gets wrong, but do not depend on its output staying the same yet.

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

## Highlights

- **Shared state** (`MUT`): stale `@cached_property` values, attributes `__hash__` reads
  changing after hashing, iterators consumed twice, writes and reads of state another component
  owns (declared once in `pyproject.toml`), mutation of `@lru_cache` results, and side effects a
  public function gained since a saved snapshot.
- **Execution effects** (`EFF`, `DATA`): blocking calls reachable from `async def` through any
  number of helpers, clients created inside loops, N+1 queries, I/O while a lock or transaction
  is held, tasks left to the garbage collector, declared effect boundaries, quadratic DataFrame
  growth.
- **Proven rewrites** (`OPT`): quadratic list / dict / set / bytes / queue patterns, rewritten
  by `heki check --fix` only when an alias analysis that never trusts annotations proves the
  rewrite keeps behavior, plus linear list lookups and nested-loop joins inside loops.
- **Fast and incremental**: about a second for a 200,000-line project; the LSP server
  re-analyzes only the edited file and stops propagating when a function's effects did not
  change.
- **Familiar interface**: Ruff-style `[tool.heki]` configuration, `select` / `ignore`,
  `# heki: ignore[CODE]` and `# noqa`, `full` / `concise` / `json` / `github` / `sarif` output,
  exit codes `0` / `1` / `2`, and line-number-independent Ratchet baselines.

## Scope

- heki follows effects and aliases through calls. It is neither a linter nor a type checker
  and leaves their checks to them.
- Its knowledge comes from its own analysis, a catalog of library behavior, contracts you
  declare in `pyproject.toml`, and type annotations, which only help it resolve calls: rewrites
  never trust them.
- When it cannot tell, it stays quiet: a rewrite needs a proof and a report needs evidence. The
  price is missed findings, which annotations, catalog entries, and contracts close.
- It does not model `__getattr__` proxies, metaclasses, or framework machinery, and it never
  runs your code.

See [Scope and limitations](docs/scope.md) for the details.

## Installation

```console
$ uv tool install heki      # or: pipx install heki / pip install heki
$ uvx heki check            # run without installing
```

## Usage

```console
$ heki check                                    # check the project
$ heki check --diff                             # preview proven-safe fixes
$ heki check --fix                              # apply them
$ heki check --baseline heki-baseline.toml      # fail only on new findings
$ heki rule EFF101                              # explain a rule
$ heki server                                   # LSP server for editors
```

## Documentation

- [Getting started](docs/getting-started.md)
- [Scope and limitations](docs/scope.md)
- [Configuration](docs/configuration.md)
- [Rules](docs/rules/index.md) (25 rules)
- [Fixes and safety](docs/fixes.md)
- [Adopting heki in a code base](docs/adoption.md)
- [Editors (LSP)](docs/lsp.md) and [coding agents](docs/coding-agents.md)
- [CLI reference](docs/cli-reference.md)
- [Development](docs/development.md)

## License

MIT
