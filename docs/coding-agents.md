# Coding Agents

heki gives coding agents (Claude Code, Codex, Cursor, and similar) the checks it gives people:
every diagnostic says what was proven, where the evidence is, and what blocks a fix. `heki
inspect` says what one function does, so an agent does not read its callees to find out. The
[Agent Skill](#agent-skill-experimental) teaches an agent the workflow of this page.

## Analyze the project, not one file

heki follows calls across files. `heki check app/api.py` analyzes that file alone and misses what
its callees do: run heki on the project (the default `.`) and filter the JSON by path.

## Before an edit: `heki inspect` (experimental)

```console
$ heki inspect app.users.load_user --output-format json
$ heki inspect app/users.py:42
$ heki inspect app/users.py::load_user --callers --depth 3
```

| Field | Content |
| ----- | ------- |
| `effects` | the network, database, file, subprocess, sleep, and lock waits it reaches, and the clients and queries it creates, each with the shortest call chain |
| `changes` | the parameters, `self` attributes, other objects' attributes, module globals, and closure cells it changes, directly or through calls, with the call chain to the change |
| `retains` | the parameters it stores where they outlive the call |
| `returns` | whether its result is a new object, a parameter, or state that outlives the call |
| `purity` | `pure`, `impure` with the reasons, or `unknown` with the first function whose calls heki cannot resolve, and what the verdict assumes |
| `callers` | who calls it, whether in a loop; with `--callers`, transitively |
| `reports` | what `heki check` reports in it; with `--callers`, also above it |
| `contracts` | the contracts that name it |

- A name is a dotted name (`pkg.mod.Class.method`, or a suffix such as `Class.method`, or a
  glob), `path.py:line`, or `path.py::name`. An ambiguous or unknown name exits with 2 and lists
  the candidates (in JSON, under `error.candidates`).
- `pure` means heki found no effect and no change of an input or of shared state, and resolved
  every call below. Libraries the [catalog](configuration.md#ecosystem-knowledge-toolhekiecosystem)
  does not list count as having no effects.
- A field the configuration does not compute is `null`, with the setting that computes it.
- Exit code 0 when the function was inspected, 2 otherwise or when a file could not be analyzed
  (the report is still printed, with `complete: false`).

Inspect the edited function again after the edit and compare `effects`, `changes`, `retains`, and
`returns` with the run before it: a new entry is new behavior. Fact keys carry no line numbers, so
a diff of the two JSON objects shows only what changed.

## After an edit: `heki check`

A typical instruction for the agent:

> After changing Python files, run `heki check --output-format json` on the project. For each
> diagnostic, read its `help` and `heki rule <rule_code>`, follow `related` to the root cause,
> and fix it there. A diagnostic whose `certainty` is `heuristic` can be wrong about the code:
> confirm what its message says before changing anything. Apply `heki check --fix` for the safe
> rewrites. Never silence a diagnostic with `# heki: ignore` unless the rule's documentation
> describes the case as intended. When unsure, run `heki check --generate-prompt "<question>"`.

```console
$ heki check --output-format json
```

The configuration decides which rules run; `--heuristic` also runs the heuristic rules that are
off by default, whose reports can be wrong about the code.

Each diagnostic has:

| Field | Content |
| ----- | ------- |
| `path`, `line`, `column`, `span` | location (byte span in the file) |
| `rule_code`, `rule_slug`, `severity`, `message` | the finding: `message` is one sentence saying what happens |
| `certainty` | `exact`, or `heuristic` when a premise of the report's claim can be open, so that it can be wrong about the code ([Rule Precision](precision.md)) |
| `help` | what to do about it (`null` when the rule has no advice for the report) |
| `qualname`, `target` | the function and the object involved |
| `related` | every hop of the call chain behind the finding |
| `evidence` | what was established (for `OPT` rules: the proof obligations) |
| `blocker` | the obligation that blocks an automatic fix, if any |
| `fix` | byte-span edits for proven-safe rewrites |
| `sites` | the violation sites a contract report stands for, when more than one |

`heki rule <CODE>` prints a rule's documentation: what it does, why it matters, and what to do
instead, which makes a good prompt for the fix. `heki rule --all --output-format json` gives
every rule's metadata and documentation.

## Ask a model: `heki check --generate-prompt` (experimental)

When a diagnostic is unclear, or the fix or the contract to write is, print a prompt for a model
and give it to one:

```console
$ heki check --generate-prompt
$ heki check app/ --generate-prompt "Why is load_user reported, and where does the fix go?"
$ heki check --generate-prompt "Write a write-owners contract for Store.items" | claude -p
$ heki check --generate-prompt --output-format json
$ heki check --output-format-json-schema generate-prompt
```

| Section | Content |
| ------- | ------- |
| Question | the question, when one is given |
| Configuration | the project root, the configuration file, the target version, the source roots, the rule selection in effect (`select`, `extend-select`, `ignore`, `per-file-ignores`, `lint.heuristic`, `fail-on`), the baseline, and the declared contracts as TOML |
| Rules that apply | each rule the run applies: its code, name, certainty, activation, and the first paragraph of its "What a report states" |
| Diagnostics | the diagnostics of the run in the `concise` form (with a baseline, the reports it does not allow), up to 16 KiB of lines, with a note counting the rest, and the files heki could not analyze |
| How to answer | fix at the root cause, do not suppress, check `heuristic` reports, rely on proven fixes only, how a contract is written, and what the answer holds |

- A word after the option is the question: put the paths before it.
- The prompt comes from the analysis `heki check` runs with the same options and configuration.
  The command writes no file and exits with 0 once the prompt is printed, also when a file could
  not be analyzed; a configuration error exits with 2, as for `heki check`.
- It cannot be combined with `--fix`, `--diff`, `--watch`, `--update-baseline`, `--profile`, or
  `--include-size-dependent`.
- `--output-format json` prints the same content as one object, and
  `--output-format-json-schema generate-prompt` prints its JSON Schema. Its `version` changes
  when a field is removed or changes meaning. Every other output format prints the Markdown
  prompt.
- Pipe the prompt to a model's command line (`| claude -p`, `| codex exec`) or copy it
  (`| pbcopy`, `| xclip -selection clipboard`, `| clip.exe`).

## Agent Skill (experimental)

The repository publishes an [Agent Skill](https://agentskills.io/specification) at
[`skills/heki/`](https://github.com/koxudaxi/heki/tree/main/skills/heki) for skills-compatible
coding agents. It teaches the workflow of this page: `heki inspect` before editing a function,
`heki check --output-format json` after the edit, `related` and `heki rule <CODE>` to find the
root cause, `heuristic` reports confirmed before acting on them, only the fixes heki proves
safe, and `heki check --generate-prompt` when unsure. Its `SKILL.md` links this page, and its
`references/` hold the output fields and what to do when a command does not behave as expected.

The skill is experimental: its wording, when agents load it, and the install commands can change
as the agents evolve.

### Install the skill

| Agent | Command |
| ----- | ------- |
| Claude Code | `claude plugin marketplace add koxudaxi/heki`, then `claude plugin install heki@heki` |
| Codex | `gh skill install koxudaxi/heki heki --agent codex` |
| Other agents, with the GitHub CLI | `gh skill install koxudaxi/heki heki --agent <agent>` (`gh skill install --help` lists the agents) |
| Other agents, with the Agent Skills CLI | `npx skills add koxudaxi/heki` |

- The Claude Code marketplace in `.claude-plugin/marketplace.json` points at the same
  `skills/heki/` directory: the repository holds one copy of the skill.
- `gh skill install` installs the skill of the latest release, at project scope; add
  `--scope user` to install it for every project.
- An agent runs the skill when its description matches the task. Claude Code also runs it as a
  slash command, and Codex through `/skills` or a `$heki` mention.

Without these tools, copy the directory where the agent reads skills:

| Agent | Project | Every project |
| ----- | ------- | ------------- |
| Claude Code | `.claude/skills/heki/` | `~/.claude/skills/heki/` |
| Codex | `.agents/skills/heki/` | `~/.agents/skills/heki/` |

```console
$ mkdir -p .agents/skills
$ curl -sL https://github.com/koxudaxi/heki/archive/refs/heads/main.tar.gz \
    | tar -xz -C .agents/skills --strip-components=2 heki-main/skills/heki
```

Other skills-compatible agents read the same directory format: see the agent's documentation for
where it reads skills.

## Use the LSP server

Agents that talk LSP (for example through an editor or an MCP bridge) can start `heki server`
and use its diagnostics, related information, and code actions directly, as editors do. See
[Editors (LSP)](lsp.md).

## Documentation for models

| File | Content |
| ---- | ------- |
| [llms.txt](llms.txt) | an index of every page, with a one-line description |
| [llms-full.txt](llms-full.txt) | the full text of every page, including all rules |
