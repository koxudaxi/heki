# Editors (LSP)

`heki server` (alias `heki lsp`) is a Language Server Protocol server on stdin / stdout. It
indexes the workspace in the background, then keeps only the per-function effect summaries in
memory. Editing a file re-analyzes that file alone (250 ms after the last keystroke). On save, the
files whose results depend on the edit are re-analyzed too: the callers of a function whose
effects changed and the functions of a call cycle through it (their effects change with it), every file that resolved names through a changed declaration (a re-exported
class, a new method or base class, a property getter, a subclass, an offloading decorator), the
classes of a family a changed method belongs to, the generators handed to a changed function
(whether it iterates them decides where their writes and reads are reported), the files with
`MUT108` / `MUT110` reports above a function that changed its own in-place changes or its calls
while its effects stayed the same (a report's call chain ends at the first function whose body
makes the change), and the files whose call chains show a call site the edit moved (to another line or column,
also when its bytes stay where they were). With
ownership or `invalidation` contracts configured, a function that changes its calls also
re-analyzes every function below it (whether a function only runs inside allowed callers, or
whether its callers announce its writes, depends on all callers above it); with a contract on
calls (`call-owners`, an effect boundary forbidding `calls`), also every function above it (a
call of a target is reported at the callers of the private helper making it, and at the scope
functions reaching it); with an effect boundary, a function that changes its calls of library APIs
re-analyzes every function above it too (a boundary may allow one API and forbid another of the
same effect, and counts every call it reaches). Edits that change none of these, such as most comments, stop at the
edited file. The diagnostics are the ones `heki check` reports for the saved files. Closing a
document with unsaved changes discards them for the analysis too: the file is analyzed from the
disk again (or leaves the project when it was never saved), and so is everything that affects.

Files created, changed, or deleted outside the editor (`git checkout`, code generators) are
picked up when the client supports watching files: the server registers for
`workspace/didChangeWatchedFiles` on Python files (`**/*.{py,pyi}`), on the configuration
(`pyproject.toml`, `heki.toml`, `.heki.toml`, and the summary snapshot of `MUT103`), and on the
ignore files (`.gitignore`, `.ignore`, `.hekiignore`). A change to the configuration or to an
ignore file indexes the workspace again, like `heki check --watch` does. A file belongs to the
project when `heki check` would find it: a file an ignore file, a hidden directory, or `exclude`
leaves out is not analyzed, also when it is created or opened. A file that does
not parse is left out of the project until it parses again, like `heki check` does.

The server provides:

- diagnostics for open files, with every hop of a call chain (for example the path from an
  `async def` to `time.sleep`) as related information you can jump to;
- quick fixes for the safe `OPT101` / `OPT102` rewrites;
- `refactor.rewrite.heki` code actions for proven-safe size-dependent rewrites;
- a quick fix that appends `# heki: ignore[CODE]` to the line of any diagnostic, unless a
  comment cannot go there (the line ends inside a string or with a `\` continuation).

Code actions edit the version of the document their diagnostics were computed on: while an edit
waits for its analysis, none are offered, and a client that applies versioned workspace edits
(`workspaceEdit.documentChanges`) gets them with that version, so it rejects them if the
document changed meanwhile.

It reads the same configuration as `heki check`. One difference is deliberate: `heki check`
reports ten or more `async def` boundaries that reach the same blocking call once, with a count,
while the server groups them per file, so the file you edit always shows its own boundary.

## VS Code

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

## Neovim

```lua
vim.lsp.config("heki", {
  cmd = { "heki", "server" },
  filetypes = { "python" },
  root_markers = { "pyproject.toml", "heki.toml", ".git" },
})
vim.lsp.enable("heki")
```

## Helix

```toml
# languages.toml
[language-server.heki]
command = "heki"
args = ["server"]

[[language]]
name = "python"
language-servers = ["pyright", "heki"]
```

## Zed

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

## Without an editor: `--watch`

```console
$ heki check --watch
```

re-checks changed files and the files affected by them, using the same incremental engine, and
prints the full list of diagnostics after every change. A change to `pyproject.toml`,
`heki.toml`, or the summary snapshot reloads everything.
