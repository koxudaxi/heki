# Coding Agents

heki gives coding agents (Claude Code, Codex, Cursor, and similar) the checks it gives people:
every diagnostic says what was proven, where the evidence is, and what blocks a fix. `heki
inspect` says what one function does, so an agent does not read its callees to find out. The
[Agent Skill](#agent-skill-experimental) teaches an agent the routine of this page after an edit.

## Analyze the project, not one file

heki follows calls across files, so it analyzes a project, never one file alone: `heki check
app/api.py` analyzes the project the file is in and shows the diagnostics that touch the file.
A directory below the project root is analyzed as given and misses what its callees elsewhere do
(heki notes the project modules its files import that it left out): run heki from the project
root.

A diagnostic sits where the problem shows, which is often not the file that was edited. In the
[first example](index.md#a-blocking-call-two-helpers-below-an-async-def) of the overview, adding
`requests.get` to `app/users.py` makes `EFF101` report at `app/api.py`, in the `async def` that
reaches it through two helpers. To see the diagnostics an edit can have caused, pass the edited
files to `--touched`, or name them as paths: heki still analyzes the project, and shows only the
diagnostics whose `path` or any `related[].path` is one of them, so the report in `app/api.py`
stays while `app/users.py` is the only edited file. The summary, the exit code, and every output
format follow the diagnostics shown. A change can also add a report in a file it left alone
without a call chain through it (a re-export, an annotation another module relied on): such a run
ends with a note on stderr counting the reports that would fail it and are not shown, also with
`--output-format json`, and with a baseline it shows every regression. Run the whole project
before finishing.

```console
$ heki check --touched app/users.py --output-format json
```

Or compare with the run before the edit: record the findings once in a baseline outside the
project, and each check with it reports only the new ones.

```console
$ heki check --baseline /tmp/heki-before.toml --update-baseline   # before the first edit
$ heki check --baseline /tmp/heki-before.toml --output-format json   # after each edit
```

## After an edit: `heki check`

A typical instruction for the agent:

> After changing Python files, run `heki check --touched <the files you changed> --output-format
> json` in the project: it shows the diagnostics whose `path` or a `related[].path` is a file you
> changed. For each, read its `help` and `heki rule <rule_code>`, follow `related` to the root
> cause, and fix it there. A diagnostic whose `certainty` is `heuristic` can be wrong about the
> code: confirm what its message says before changing anything. Apply `heki check --fix` for the
> safe rewrites. Never silence a diagnostic with `# heki: ignore` unless the rule's documentation
> describes the case as intended. When unsure, run `heki check --generate-prompt "<question>"`.
> Before you finish, run `heki check` on the whole project (with `--baseline` when it has one):
> `--touched` is for the loop while editing, and a change can cause a report in a file you did
> not touch.

```console
$ heki check --touched app/users.py app/api.py --output-format json
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
| `allowed_examples` | for a contract report, up to three public functions (`fqn`, `path`) that do the same from a place the contract allows, which `help` names |

A text run ends with one line per reported rule: `heki rule <CODE>` and the rule's page.

The run exits with 1 when it shows an exact `error`, a bug that can be critical, or with
`fail-on = "warning"` any report: a heuristic report counts as a warning, whatever its
`severity`.

When the run cannot check (exit code 2: a configuration, path, or file error), the error says
where the cause is, what to write instead, and which section of the documentation explains it.
With `--output-format json`, an error that stops the run (the configuration or a path) is an
object on stdout; a file that cannot be read or parsed is named on stderr, and stdout keeps the
diagnostics of the other files:

```json
{
  "error": {
    "message": "unknown key `select` in [tool.heki]",
    "path": "pyproject.toml",
    "line": 5,
    "column": 1,
    "help": [
      "`select` is a key of [tool.heki.lint]: move it there",
      "for example: `select = [\"EFF\", \"MUT101\"]`"
    ],
    "docs": "https://heki.koxudaxi.dev/configuration/#rule-selection-toolhekilint",
    "candidates": []
  }
}
```

Apply what `help` says; `candidates` holds the names a misspelled one may have meant, the closest
first.

`heki rule <CODE>` prints a rule's documentation: what it does, why it matters, and what to do
instead, which makes a good prompt for the fix. `heki rule --all --output-format json` gives
every rule's metadata and documentation, the premises of its exact claim, and its published
precision: its certification and its latest measurement ([Rule Precision](precision.md)).

## Architecture rules with agents

Do not describe the architecture in the project's `AGENTS.md` or `CLAUDE.md`. Declare it as
[contracts](configuration.md#contracts) and let `heki check` enforce it. Three lines are enough:

> After editing Python, run `heki check`.
> A report that names a contract means the code crossed a wall the maintainers declared: change the
> code as the report's help says.
> Do not edit the contracts or the baseline, and do not add a suppression, unless the maintainer
> asks.

The `message` of a contract is what the agent reads as the fix, so write it as an instruction.
Contract reports are heuristic, and a heuristic report fails a run only with
`fail-on = "warning"`: set it with the contracts, so that `heki check` exits with 1 on a crossed
wall.

```toml
# heki.toml
[lint]
fail-on = "warning"

[[write-owners]]
name = "generation-store"
targets = ["pkg.model.DataModel.fields"]
allowed-writers = ["Store.*"]
message = "use GenerationStore.set_fields() instead"
```

Put the contracts, `fail-on`, and the baseline path in `heki.toml`, and give that file an owner
(`CODEOWNERS`, or the host's rule for protected files). A change to a wall is then a reviewed
change, and an agent cannot move a wall to make a report go away.

```text
# .github/CODEOWNERS
/heki.toml           @your-org/maintainers
/heki-baseline.toml  @your-org/maintainers
```

`fail-on = "warning"` makes every warning and heuristic report fail the run, not only the
contracts' reports: record today's findings in the baseline first, so that only new ones fail.
The [starter recipe](adoption.md#walls-for-a-team-or-a-coding-agent) has the whole file.

### A Stop hook for Claude Code

A [Stop hook](https://code.claude.com/docs/en/hooks) runs when Claude Code is about to end its turn.
The script below runs `heki check` and, when it exits with 1 (findings that fail the run), sends
the text to the agent and keeps it working. Exit code 2 of the hook means "block, and read
stderr". heki's own failure (exit code 2, or no `heki` on the path) lets the turn end, so a
configuration error does not loop. The hook input has `stop_hook_active`, which is `true` while
Claude Code is already continuing because of a stop hook; the script stops blocking then.

```sh
#!/bin/sh
input=$(cat)
case $input in
  *'"stop_hook_active":true'* | *'"stop_hook_active": true'*) exit 0 ;;
esac
report=$(heki check 2>&1)
status=$?
[ "$status" -eq 1 ] || exit 0
printf '%s\n' "$report" >&2
exit 2
```

Register it in `.claude/settings.json`:

```json
{
  "hooks": {
    "Stop": [{ "hooks": [{ "type": "command", "command": ".claude/hooks/heki-stop.sh" }] }]
  }
}
```

Run on a project with findings that are already known, it would block every turn: use
`heki check --baseline heki-baseline.toml` in the script so that only new findings count.

## What a function does: `heki inspect` (experimental)

When a diagnostic or a question is about one function, `heki inspect` says what it reaches,
changes, keeps, and returns, with the call chains, without reading its callees. Run it when a
function's effects are in question, not before every edit.

```console
$ heki inspect app.users.load_user --output-format json
$ heki inspect app/users.py:42
$ heki inspect app/users.py::load_user --callers --depth 3
$ printf '%s\n' app.users.load_user 'app.orders.*' | heki inspect - --output-format json
```

| Field | Content |
| ----- | ------- |
| `effects` | the network, database, file, subprocess, sleep, and lock waits it reaches, and the clients and queries it creates, each with the shortest call chain |
| `changes` | the parameters, `self` attributes, other objects' attributes, module globals, and closure cells it changes, directly or through calls, with the call chain to the change |
| `retains` | the parameters it stores where they outlive the call |
| `only_reads` | its parameters annotated with a mutable container (`list`, `dict`, `set`), those it and every function it hands them to only read (no change, no store, every call resolved), those whose uses the read-only type (`Sequence`, `Mapping`, `AbstractSet`) allows, those whose name it binds once, and, by parameter it only reads, the kinds of read that reach the object here and in the functions it is handed to (`reads`). These are the function's facts: `heki recommend` also checks that no project subclass of the argument's class changes it in one of those reads, so a parameter listed here is no proof that every override only reads |
| `returns` | whether its result is a new object, a parameter, or state that outlives the call |
| `registered` | each registration that may reach it through a table of callbacks and a lookup by name (`getattr(self, <name>)`), with the parameter bound to the registering object and its class, the condition (`selector` equals the function's name; `others_open` when the name is not a constant), the sites on the way, and what it changes of that parameter under the context. Advisory: no other fact or report assumes it |
| `purity` | `pure`, `impure` with the reasons, or `unknown` with the reasons; whatever the verdict, the functions below it whose calls heki cannot resolve, nearest first, each with the call chain to it; and what the verdict assumes |
| `callers` | who calls it, whether in a loop; with `--callers`, transitively |
| `reports` | what `heki check` reports in it; with `--callers`, also above it |
| `contracts` | the contracts that name it, with its roles in them, and the contracts the reports above are of (role `reported`) |

- A name is a dotted name (`pkg.mod.Class.method`, or a suffix such as `Class.method`, or a
  glob), `path.py:line`, or `path.py::name`. An ambiguous or unknown name exits with 2: the error
  says what to do (in JSON, `error.help`; a misspelled name gets the functions whose names look
  like it) and lists the functions to choose from (`error.candidates`, `error.suggestions`,
  `error.methods`, or `error.functions`).
- With `-` for the name, the names are read from standard input, one per line, and heki analyzes
  the project once for all of them: the JSON output is an array of the objects, in the order of
  the names, and the text output one section per function. Here a glob names each function it
  matches, in the order of their full names (on the command line, a glob matching several is
  ambiguous). A name that names no single function takes its place in the array as the error
  object it prints alone, the others are still reported, and the exit code is 2. No names give
  an empty array.
- `pure` means heki found no effect and no change of an input or of shared state, and resolved
  every call below. Libraries the [catalog](configuration.md#ecosystem-knowledge-toolhekiecosystem)
  does not list count as having no effects.
- Constructing an object changes no existing one: `changes.class_attrs` marks the writes of an
  object under construction `fresh`, and the text form leaves them out. Such an object is one the
  function allocates and fills, or the `self` of a class's own initialization, written by the
  initializer or by a helper it passes `self` to (`Node(...)` running `_link(self)`).
- `purity.unknown_reach.entries` lists the functions heki cannot follow among the function and
  its callees (a call it cannot resolve, or a body only a stub declares), nearest first, at most
  100; `truncated` says there are more. `purity.unknown_at` is the first of them. Both are given
  for an `impure` function too: what it changes may then be incomplete.
- A field the configuration does not compute is `null`, with the setting that computes it.
- Exit code 0 when every function was inspected, 2 otherwise or when a file could not be
  analyzed (the reports are still printed, with `complete: false`).

To see what an edit changed in a function, inspect it before and after the edit and compare
`effects`, `changes`, `retains`, `only_reads`, and `returns`: a new entry is new behavior, and so
is a parameter leaving `only_reads.params`. Fact keys carry no line numbers, so a diff of the two
JSON objects shows only what changed.

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
| Configuration | the project root, the configuration file, the target version, the source roots (with the packages of flat workspace members), the rule selection in effect (`select`, `extend-select`, `ignore`, `per-file-ignores`, `lint.heuristic`, `fail-on`), the baseline, and the declared contracts as TOML |
| Rules that apply | each rule the run applies: its code, name, certainty, activation, and the first paragraph of its "What a report states" |
| Diagnostics | the diagnostics of the run in the `concise` form (with a baseline, the reports it does not allow), up to 16 KiB of lines, with a note counting the rest, and the files heki could not analyze |
| How to answer | fix at the root cause, do not suppress, check `heuristic` reports, rely on proven fixes only, how a contract is written, and what the answer holds |

- A word after the option is the question: put the paths before it.
- The prompt comes from the analysis `heki check` runs with the same options and configuration.
  The command writes no file and exits with 0 once the prompt is printed, also when a file could
  not be analyzed; a configuration error exits with 2, as for `heki check`.
- It cannot be combined with `--fix`, `--diff`, `--watch`, `--update-baseline`, or
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
coding agents. It is the routine of this page after an edit, short enough to read each time it
runs: `heki check --output-format json` on the project, the diagnostics the edit caused,
`related` and `heki rule <CODE>` to find the root cause, `heuristic` reports confirmed before
acting on them, and only the fixes heki proves safe. `heki inspect` and
`heki check --generate-prompt` are for when a function or a diagnostic is in question. Its
`SKILL.md` links this page, and its `references/` hold the output fields, what to do when a
command does not behave as expected, and a playbook for adopting heki in a repository.

The skill is experimental: its wording, when agents load it, and the install commands can change
as the agents evolve.

### Install the skill

heki installs the skill it was built with, which describes its own commands and output:

```console
$ heki skill install --agent claude-code
$ heki skill install --agent codex --scope user
```

- `--agent claude-code` writes `.claude/skills/heki/`, `--agent codex` writes
  `.agents/skills/heki/`.
- `--scope project` (the default) writes under the working directory, `--scope user` under the
  home directory, for every project.
- A skill already there is replaced only with `--overwrite`. The command is experimental.

Without heki installed, or to follow the repository instead:

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
