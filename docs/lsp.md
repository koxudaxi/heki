# Editors (LSP)

`heki server` (alias `heki lsp`) is a Language Server Protocol server on stdin / stdout: it shows
heki's diagnostics and fixes while you edit, with the results `heki check` reports.

## Set up your editor

=== "VS Code"

    heki does not ship its own extension yet. Any generic LSP client extension that starts a
    command for a language works; configure it to run `heki server` for `python` documents, for
    example:

    ```json
    {
      "command": "heki",
      "args": ["server"],
      "languages": ["python"]
    }
    ```

=== "Neovim"

    ```lua
    vim.lsp.config("heki", {
      cmd = { "heki", "server" },
      filetypes = { "python" },
      root_markers = { "pyproject.toml", "heki.toml", ".git" },
    })
    vim.lsp.enable("heki")
    ```

=== "Helix"

    ```toml
    # languages.toml
    [language-server.heki]
    command = "heki"
    args = ["server"]

    [[language]]
    name = "python"
    language-servers = ["pyright", "heki"]
    ```

=== "Zed"

    ```json
    {
      "lsp": {
        "heki": {
          "binary": { "path": "heki", "arguments": ["server"] }
        }
      },
      "languages": {
        "Python": { "language_servers": ["...", "heki"] }
      }
    }
    ```

## What the server provides

- Diagnostics for open files, with every hop of a call chain (for example the path from an
  `async def` to `time.sleep`) as related information you can jump to. The message is the
  report's one sentence; its help, what to do about it, is the last related-information entry
  (`help: ...`) at the diagnostic's own range, the way rust-analyzer shows a `help:` note.
- The message of a [heuristic](precision.md) report ends in `[heuristic]`, and every diagnostic's
  `data.certainty` is `"exact"` or `"heuristic"`. The rule code links to the rule's page
  (`codeDescription`).
- `heki server --heuristic` (or `--no-heuristic`) overrides `lint.heuristic` of the
  configuration, as on the command line.
- Quick fixes for the safe `OPT101` / `OPT102` rewrites.
- `refactor.rewrite.heki` code actions for the proven-safe size-dependent rewrites. A fix whose
  result would not parse is never offered, as `heki check --fix` refuses it.
- A quick fix that appends `# heki: ignore[CODE]` to the line of any diagnostic, unless a comment
  cannot go there (the line ends inside a string or with a `\` continuation).

Code actions edit the version of the document their diagnostics were computed on:

- while an edit waits for its analysis, none are offered;
- a client that applies versioned workspace edits (`workspaceEdit.documentChanges`) gets them
  with that version, so it rejects them if the document changed meanwhile.

## The same results as `heki check`

- The server reads the same [configuration](configuration.md), including the Ratchet baseline:
  with `baseline` configured, a file shows what `heki check` reports against it (every finding of
  an entry that grew).
- A file belongs to the project when `heki check` would find it. A file that an ignore file, a
  hidden directory, or `exclude` leaves out is not analyzed, also when it is created or opened.
- A file that does not parse is a module heki knows nothing about until it parses again, as in
  `heki check`: the names under it resolve to nothing in the other files.
- A configuration `heki check` rejects (an unknown rule, a contract target that is not
  `Class.attr`, an invalid pattern) is logged as a warning, and the server analyzes nothing until
  it is fixed.

One difference is deliberate. When more than ten `async def` boundaries reach the same blocking
call, `heki check` reports them once, with a count; the server groups them per file, so the file
you edit always shows its own boundary.

## When the server re-analyzes

The server indexes the workspace in the background, then keeps only the per-function effect
summaries in memory. It runs the analyses one at a time, in the order of the editor's messages
(an edit's analysis takes its place when its debounce ends), so the diagnostics do not depend on
which analysis happens to start first: of documents opened together, each is analyzed with the
ones opened before it already indexed.

| Event | Re-analyzed |
| ----- | ----------- |
| An edit, 250 ms after the last keystroke | the edited file alone |
| A save | the saved file, and the files whose results depend on the edit (below) |
| A file created, changed, or deleted outside the editor (`git checkout`, code generators) | as for a save; needs a client that supports watching files |
| A change to the configuration or to an ignore file | the whole workspace, indexed again |
| A change to the baseline file | every file |
| Closing a document with unsaved changes | the file as it is on the disk (or it leaves the project when it was never saved), and everything that affects |

On a save, the edit reaches other files only through what it changes:

| The edit changes | Also re-analyzed |
| ---------------- | ---------------- |
| a function's effects, the locks it takes (`EFF109`), the objects it uses one operation at a time (`EFF110`), what it returns in a set's order or writes of its parameters (`EFF112`), whether it reaches a call cycle (`EFF114`), or what the object it returns may be (a new one, its argument, or state it keeps) | its callers, and the functions of a call cycle through it (their effects change with it) |
| a declaration that names resolve through: a re-exported class, a new method or base class, a property getter, a subclass, an offloading decorator | every file that resolved names through it |
| a method of a class family | the classes of that family; with `MUT112` selected, every changed method counts: a value stored in `self.<attr>` is reported when a method of the family changes that attribute in place |
| what a function changes of its own containers, or what it does with the generators it calls (`MUT113`, when selected) | the classes of its family with generators suspending inside an iteration of their containers, and those of the generators it calls |
| the writes that leave a `@cached_property` stale (`MUT111`, when selected) | the files reading such a property: whether a read is reported depends on every writer of the class family, wherever it is |
| a function generators are handed to | those generators: whether it iterates them decides where their writes and reads are reported |
| a function's own in-place changes or its calls, while its effects stay the same | the files with `MUT108` / `MUT110` reports above it: a report's call chain ends at the first function whose body makes the change |
| the position of a call site, also when its bytes stay where they were | the files whose call chains show that call site |
| the position of a method's own change of a container | the files whose `MUT113` reports show it |

Contracts reach further when a function changes its calls:

| Configured | Also re-analyzed | Because |
| ---------- | ---------------- | ------- |
| an ownership or `invalidation` contract | every function below it | whether a function only runs inside allowed callers, or whether its callers announce its writes, depends on all callers above it |
| a contract on calls (`call-owners`, an effect boundary forbidding `calls`) | every function above it | a call of a target is reported at the callers of the private helper making it, and at the scope functions reaching it |
| an effect boundary, when the function changes its calls of library APIs, or the calls, module attribute reads, and import probes outside the project that a boundary forbids | every function above it | a boundary may allow one API and forbid another of the same effect, counts every call it reaches, and no summary records an access outside the project |

Edits that change none of these, such as most comments, stop at the edited file.

### Files outside the editor

When the client supports watching files, the server registers for
`workspace/didChangeWatchedFiles` on:

- Python files (`**/*.{py,pyi}`);
- the configuration: `pyproject.toml`, `heki.toml`, `.heki.toml`, and the summary snapshot of
  `MUT103`;
- the ignore files: `.gitignore`, `.ignore`, `.hekiignore`;
- TOML files, for the Ratchet baseline.

An open document stays the file's text until it is closed:

- indexing the workspace again keeps documents that were never saved;
- a file deleted on the disk while its document is open stays in the project until the document
  closes.

A deletion the editor reports holds until the file is reported again (created or changed) or
saved, even while the disk still lists the file: on Windows a deleted file stays listed until the
last program holding it open closes it.

## Without an editor: `--watch`

```console
$ heki check --watch
```

- It re-checks the changed files and the files affected by them, with the same incremental
  engine, and prints the full list of diagnostics after every change, compared with the Ratchet
  baseline as `heki check` compares them.
- It only reports: `--fix`, `--diff`, and `--update-baseline` are ignored, with a warning.
- It reloads everything when the configuration (`pyproject.toml`, `heki.toml`, `.heki.toml`, or
  the `--config` file), a `hatch.toml` (its workspace members), the summary snapshot, or the
  baseline changes. So does a change of the detected source roots (`src/`, `python/`, or the
  directory named after the project appearing, or becoming a package with an `__init__.py`; a
  workspace member's `pyproject.toml`, source roots, or packages appearing or disappearing) or a
  root `__init__.py` appearing or disappearing: module names below a source root, or relative imports that climb to the root,
  change with the module layout. The server does the same.
- What fails once the watch runs (a configuration that no longer parses, a watched path that
  disappears) is reported, and the watch goes on until it is fixed.
- A file counts as changed when its size, its modification time, or (on Unix) its inode or
  status-change time changes, so a replacement that keeps the modification time is seen too. A
  file stamped within the last two seconds is also compared by content, for file systems whose
  timestamps are coarse.
