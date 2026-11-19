# Editors (LSP)

`heki server` (alias `heki lsp`) is a Language Server Protocol server on stdin / stdout. It
indexes the workspace in the background, then keeps only the per-function effect summaries in
memory. Editing a file re-analyzes that file alone (250 ms after the last keystroke); when the
edit changes what a function does, the files calling it are re-analyzed on save. Edits that do not
change any summary, such as comments, stop at the edited file.

The server provides:

- diagnostics for open files, with every hop of a call chain (for example the path from an
  `async def` to `time.sleep`) as related information you can jump to;
- quick fixes for the safe `OPT101` / `OPT102` rewrites;
- `refactor.rewrite.heki` code actions for proven-safe size-dependent rewrites;
- a quick fix that appends `# heki: ignore[CODE]` to the line of any diagnostic.

It reads the same configuration as `heki check`.

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
