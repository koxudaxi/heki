# heki

heki (壁, "wall") is an effect and alias checker for Python, written in Rust. It is the third
layer of static analysis next to a linter such as Ruff and a type checker such as ty: it follows
**what code does** across functions and modules — which objects it mutates, which references it
shares, and which blocking, network, or database work it reaches.

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

- **Shared state** (`MUT`): stale `@cached_property` values, writes to state another
  component owns (declared once in `pyproject.toml`), mutation of `@lru_cache` results, and
  side effects a public function gained since a saved snapshot.
- **Execution effects** (`EFF`, `DATA`): blocking calls reachable from `async def` through any
  number of helpers, clients created inside loops, N+1 queries, quadratic DataFrame growth.
- **Proven rewrites** (`OPT`): quadratic list / dict / set / bytes / queue patterns, rewritten
  by `heki check --fix` only when an alias analysis that never trusts annotations proves the
  rewrite keeps behavior.
- **Fast and incremental**: about a second for a 200,000-line project; the LSP server
  re-analyzes only the edited file and stops propagating when a function's effects did not
  change.
- **Familiar interface**: Ruff-style `[tool.heki]` configuration, `select` / `ignore`,
  `# heki: ignore[CODE]` and `# noqa`, `full` / `concise` / `json` / `github` / `sarif` output,
  exit codes `0` / `1` / `2`, and line-number-independent Ratchet baselines.

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
- [Configuration](docs/configuration.md)
- [Rules](docs/rules/index.md) (17 rules)
- [Fixes and safety](docs/fixes.md)
- [Adopting heki in a code base](docs/adoption.md)
- [Editors (LSP)](docs/lsp.md) and [coding agents](docs/coding-agents.md)
- [CLI reference](docs/cli-reference.md)
- [Development](docs/development.md)

## License

MIT
