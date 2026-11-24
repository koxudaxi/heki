# Output fields

## `heki check --output-format json`

A list of diagnostics. Each one has:

| Field | Content |
| ----- | ------- |
| `path`, `line`, `column`, `span` | location (byte span in the file) |
| `rule_code`, `rule_slug`, `severity`, `message` | the finding: `message` is one sentence saying what happens |
| `certainty` | `exact`, or `heuristic` when a premise of the report's claim can be open, so that it can be wrong about the code ([Rule Precision](https://heki.koxudaxi.dev/precision/)) |
| `help` | what to do about it (`null` when the rule has no advice for the report) |
| `qualname`, `target` | the function and the object involved |
| `related` | every hop of the call chain behind the finding |
| `evidence` | what was established (for `OPT` rules: the proof obligations) |
| `blocker` | the obligation that blocks an automatic fix, if any |
| `fix` | byte-span edits for proven-safe rewrites |
| `sites` | the violation sites a contract report stands for, when more than one |
| `allowed_examples` | for a contract report, up to three public functions (`fqn`, `path`) that do the same from a place the contract allows, which `help` names |

`severity` is `error` for a bug (a wrong result, an exception, a lost update, a deadlock, a
broken contract) and `warning` for slower code or a bug that is rarely harmful.

## `heki inspect --output-format json` (experimental)

One versioned object about one function (with `-` and the names on standard input, one per
line, an array of them from one analysis, in the order of the names):

| Field | Content |
| ----- | ------- |
| `effects` | the network, database, file, subprocess, sleep, and lock waits it reaches, and the clients and queries it creates, each with the shortest call chain |
| `changes` | the parameters, `self` attributes, other objects' attributes, module globals, and closure cells it changes, directly or through calls, with the call chain to the change |
| `retains` | the parameters it stores where they outlive the call |
| `returns` | whether its result is a new object, a parameter, or state that outlives the call |
| `purity` | `pure`, `impure` with the reasons, or `unknown` with the reasons; whatever the verdict, the functions below it whose calls heki cannot resolve, nearest first, each with the call chain to it; and what the verdict assumes |
| `callers` | who calls it, whether in a loop; with `--callers`, transitively |
| `reports` | what `heki check` reports in it; with `--callers`, also above it |
| `contracts` | the contracts that name it |

- `pure` means heki found no effect and no change of an input or of shared state, and resolved
  every call below. Libraries heki's catalog does not list count as having no effects.
- Constructing an object changes no existing one: `changes.class_attrs` marks the writes of an
  object under construction `fresh` (one the function allocates and fills, or the `self` of a
  class's own initialization, written by the initializer or by a helper it passes `self` to).
- `purity.unknown_reach.entries` lists the functions heki cannot follow among the function and
  its callees (a call it cannot resolve, or a body only a stub declares), nearest first, at most
  100; `truncated` says there are more. `purity.unknown_at` is the first of them. Both are given
  for an `impure` function too: what it changes may then be incomplete.
- A field the configuration does not compute is `null`, with the setting that computes it.
- Fact keys carry no line numbers, so a diff of the objects before and after an edit shows only
  what changed: a new entry in `effects`, `changes`, `retains`, or `returns` is new behavior.
  Tell the user about it unless they asked for it.

## `heki check --generate-prompt --output-format json` (experimental)

One versioned object: `question`, `configuration` (root, configuration file, target version,
source roots, rule selection, baseline, contracts), `rules` (code, name, certainty, activation,
and what a report states), `heuristic_rules_off`, `diagnostics` (the concise fields of the
diagnostics that fit, with `omitted` counting the rest), and `guidance`.
`heki check --output-format-json-schema generate-prompt` prints its JSON Schema. A word after
`--generate-prompt` is the question, so paths go before it.
