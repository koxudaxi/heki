# Coding Agents

heki gives coding agents (Claude Code, Codex, Cursor, and similar) the checks it gives people:
every diagnostic says what was proven, where the evidence is, and what blocks a fix.

## Use the CLI in the agent's loop

A typical instruction for the agent:

> After changing Python files, run `heki check --output-format json`. For each diagnostic,
> read `heki rule <rule_code>`, follow `related` to the root cause, and fix it there. Apply
> `heki check --fix` for the safe rewrites. Never silence a diagnostic with `# heki: ignore`
> unless the rule's documentation describes the case as intended.

```console
$ heki check --output-format json
```

Each diagnostic has:

| Field | Content |
| ----- | ------- |
| `path`, `line`, `column`, `span` | location (byte span in the file) |
| `rule_code`, `rule_slug`, `severity`, `message` | the finding |
| `qualname`, `target` | the function and the object involved |
| `related` | every hop of the call chain behind the finding |
| `evidence` | what was established (for `OPT` rules: the proof obligations) |
| `blocker` | the obligation that blocks an automatic fix, if any |
| `fix` | byte-span edits for proven-safe rewrites |
| `sites` | the violation sites a contract report stands for, when more than one |

`heki rule <CODE>` prints a rule's documentation: what it does, why it matters, and what to do
instead, which makes a good prompt for the fix. `heki rule --all --output-format json` gives
every rule's metadata and documentation.

## Use the LSP server

Agents that talk LSP (for example through an editor or an MCP bridge) can start `heki server`
and use its diagnostics, related information, and code actions directly, as editors do. See
[Editors (LSP)](lsp.md).

## Documentation for models

| File | Content |
| ---- | ------- |
| [llms.txt](llms.txt) | an index of every page, with a one-line description |
| [llms-full.txt](llms-full.txt) | the full text of every page, including all rules |
