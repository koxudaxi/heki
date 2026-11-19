# Configuration

heki reads `[tool.heki]` from `pyproject.toml`, or a `heki.toml` / `.heki.toml` file (same keys
without the `tool.heki` prefix). The nearest file at or above the checked path wins; `--config
<path>` points at one explicitly. Keys are kebab-case, as in Ruff.

```toml
[tool.heki]
target-version = "py312"            # default: inferred from project.requires-python
extend-exclude = ["generated"]      # added to the default excludes
baseline = "heki-baseline.toml"     # Ratchet baseline used by `heki check`

[tool.heki.lint]
select = ["MUT", "EFF", "DATA", "OPT"]
extend-select = ["EFF105"]
ignore = ["OPT110"]
per-file-ignores = { "tests/**" = ["EFF"], "scripts/*.py" = ["OPT"] }
relax-async-in-tests = true

[tool.heki.lint.severity]
OPT103 = "warning"
```

## Top-level settings

| Key | Default | Meaning |
| --- | ------- | ------- |
| `target-version` | the lower bound of `project.requires-python`, else `py312` | Python version of the code: `py311` … `py315` (or `3.11` … `3.15`). Without it, heki reads `requires-python` from the `pyproject.toml` that holds the configuration, or the one next to `heki.toml` / the `--config` file, and takes its oldest admitted version, clamped to 3.11–3.15 (`>=3.9` → `py311`). `--target-version` overrides both. |
| `exclude` | the Ruff defaults (`.git`, `.venv`, `build`, `dist`, `node_modules`, …) | Replaces the excluded paths (globs, matched against paths and path components). |
| `extend-exclude` | `[]` | Adds excluded paths. |
| `baseline` | none | Ratchet baseline file used when `--baseline` is not given. |

`.gitignore` and `.hekiignore` files are honored during discovery. Files named explicitly on the
command line are always checked.

## `[tool.heki.lint]`

| Key | Meaning |
| --- | ------- |
| `select` | Rules to enable instead of the defaults (codes, prefixes such as `EFF`, rule names, or `ALL`). |
| `extend-select` | Rules to add. Opt-in rules (`EFF105`, `MUT103`) need their exact code or name. |
| `ignore` | Rules to disable (prefixes match every rule they cover). |
| `per-file-ignores` | Glob → rules disabled in matching files (project-relative POSIX paths). |
| `severity` | Rule → `"error"` or `"warning"` (affects output and SARIF level; both make `heki check` fail). |
| `relax-async-in-tests` | `true` (default) skips `EFF101` / `EFF105` in `test_*` functions and files under `tests/`. |

The command line wins: `--select` replaces `select`, `--extend-select` and `--ignore` add to the
configured lists.

## Ownership contracts: `[[tool.heki.write-owners]]` (`MUT201`)

```toml
[[tool.heki.write-owners]]
name = "generation-store"
targets = ["pkg.model.DataModel.fields", "DataType.data_types"]
allowed-writers = ["pkg.parser.generation.*", "Store.*"]
forbid-cached-property-on = ["DataModel"]
```

- `targets`: `Class.attr` names. A target matches the fully qualified name or any dotted suffix
  (`DataModel.fields` matches `pkg.model.DataModel.fields`), and globs are allowed. Subclasses of
  a target class are covered too.
- `allowed-writers`: functions allowed to write (fully qualified or qualname, globs allowed).
- `forbid-cached-property-on`: classes on which defining a `@cached_property` is itself reported.

## Shared return values: `[[tool.heki.readonly-returns]]` (`MUT202`)

```toml
[[tool.heki.readonly-returns]]
name = "schema-graph"
producers = ["pkg.parser.SchemaGraphBuilder.schema"]
deep = true
```

Functions decorated with `@lru_cache` / `@cache` are protected without configuration.

## Ecosystem knowledge: `[[tool.heki.ecosystem]]`

heki ships a catalog of blocking, async-safe, and offloading APIs of the standard library and
common packages (`requests`, `httpx`, `aiohttp`, `sqlalchemy`, `django`, `redis`, `pandas`, …).
Add your own or override an entry:

```toml
[[tool.heki.ecosystem]]
symbol = "mypkg.db.run_query"
effects = ["blocking_db", "db_query"]

[[tool.heki.ecosystem]]
symbol = "mypkg.workers.in_thread"
offloads-blocking-arg = 0     # calls its first argument on a thread
async-safe = true

[[tool.heki.ecosystem]]
symbol = "mypkg.logging.emit"
excluded = true               # never reported
```

| Key | Meaning |
| --- | ------- |
| `symbol` | Fully qualified function or method name (`package.module.Class.method`). |
| `effects` | Any of `blocking_net`, `blocking_db`, `blocking_subprocess`, `blocking_sync`, `sleep`, `blocking_fs`, `unpooled_client_alloc`, `db_query`, `quadratic_realloc`. |
| `async-safe` | Safe to call from `async def`. |
| `offloads-blocking-arg` | Index of the callable argument this function runs on another thread. |
| `excluded` | Never reported (logging and similar). |
| `copy-sanitizer` | Returns a copy (ends `MUT202` protection). |

An override replaces the built-in entry for the same symbol as a whole.

## Inline suppressions

```python
x = load()            # heki: ignore[EFF101]
y = load()            # heki: ignore[EFF101, async-blocking-call]
z = load()            # heki: ignore
w = load()            # noqa: EFF101
```

A suppression applies to the line of the diagnostic. Codes and rule names are accepted; a
comment without codes silences every rule on that line.
