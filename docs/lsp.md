# Editors (LSP)

`heki server` (alias `heki lsp`) is a Language Server Protocol server on stdin / stdout. It
indexes the workspace in the background, then keeps only the per-function effect summaries in
memory. Editing a file re-analyzes that file alone (250 ms after the last keystroke). On save, the
files whose results depend on the edit are re-analyzed too: the files of the functions whose
effects changed (a call cycle through the edited file spans other files) and of their callers,
every file that resolved names through a changed declaration (a re-exported
class, a new method or base class, a property getter, a subclass, an offloading decorator), the
classes of a family a changed method belongs to, the generators handed to a changed function
(whether it iterates them decides where their writes and reads are reported), the files with
`MUT108` / `MUT110` reports above a function that changed its own in-place changes or its calls
while its effects stayed the same (a report's call chain ends at the first function whose body
makes the change), and the files whose call chains show a call site the edit moved. With
ownership or `invalidation` contracts configured, a function that changes its calls also
re-analyzes every function below it (whether a function only runs inside allowed callers, or
whether its callers announce its writes, depends on all callers above it); with a contract on
calls (`call-owners`, an effect boundary forbidding `calls`), also every function above it (a
call of a target is reported at the callers of the private helper making it, and at the scope
functions reaching it). Edits that change none of these, such as most comments, stop at the
edited file. The diagnostics are the ones `heki check` reports for the saved files.

Files created, changed, or deleted outside the editor (`git checkout`, code generators) are
picked up when the client supports watching files: the server registers for
`workspace/didChangeWatchedFiles` on Python files (`**/*.{py,pyi}`) and on the configuration
(`pyproject.toml`, `heki.toml`, `.heki.toml`, and the summary snapshot of `MUT103`). A change to
the configuration indexes the workspace again, like `heki check --watch` does. A file that does
not parse is left out of the project until it parses again, like `heki check` does.

The server provides:

- diagnostics for open files, with every hop of a call chain (for example the path from an
  `async def` to `time.sleep`) as related information you can jump to;
- quick fixes for the safe `OPT101` / `OPT102` rewrites;
- `refactor.rewrite.heki` code actions for proven-safe size-dependent rewrites;
- a quick fix that appends `# heki: ignore[CODE]` to the line of any diagnostic.

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
