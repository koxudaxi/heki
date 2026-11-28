# Output Formats

Every JSON format heki writes has a JSON Schema in the repository, so that CI, governance tools,
and coding agents can check what they read. The schemas are in
[`docs/schemas/`](https://github.com/koxudaxi/heki/tree/main/docs/schemas) and published at
`https://heki.koxudaxi.dev/schemas/<name>.json`, the `$id` of each. They follow JSON Schema draft
2020-12 and are derived from the types that write the output, so they cannot drift from it.
`heki check --output-format-json-schema <name>` prints the same schema.

| Format | Written by | Schema |
| ------ | ---------- | ------ |
| Diagnostics, an array | `heki check --output-format json` | [check-1](schemas/check-1.json) |
| Suppressed reports | `heki check --report-suppressed PATH` | [report-suppressed-1](schemas/report-suppressed-1.json) |
| Effect summary snapshot | `heki snapshot --write`, or `heki snapshot` on stdout | [summary-snapshot-1](schemas/summary-snapshot-1.json) |
| Ratchet baseline, version 1 | files written before baselines counted sites | [baseline-1](schemas/baseline-1.json) |
| Ratchet baseline, version 2 | files that count sites | [baseline-2](schemas/baseline-2.json) |
| Ratchet baseline, version 3 | `heki check --baseline FILE --update-baseline` | [baseline-3](schemas/baseline-3.json) |
| Function facts | `heki inspect SYMBOL --output-format json` | [inspect-1](schemas/inspect-1.json) |
| Import facts | `heki inspect --imports MODULE --output-format json` | [inspect-imports-1](schemas/inspect-imports-1.json) |
| Rule metadata | `heki rule [CODE] --output-format json` | [rule-1](schemas/rule-1.json) |
| Assumption A1 hazards | `heki assumptions --json` | [assumptions-1](schemas/assumptions-1.json) |
| Failure of a command | any command, with `--output-format json` | [error-1](schemas/error-1.json) |
| Configuration advice | `heki recommend --output-format json` | [recommend-3](schemas/recommend-3.json), and [version 1](schemas/recommend-1.json) and [2](schemas/recommend-2.json) |
| Prompt for a language model | `heki check --generate-prompt --output-format json` | `heki check --output-format-json-schema generate-prompt` |

The baseline and the snapshot are files you commit. A baseline is TOML: its schema describes its
values as JSON, which is what a TOML parser gives. Validate a baseline by parsing it first.
SARIF output (`--output-format sarif`) follows the public
[SARIF 2.1.0](https://docs.oasis-open.org/sarif/sarif/v2.1.0/sarif-v2.1.0.html) schema, which
heki does not copy.

## Where a contract report points

A report of a contract rule gives, in `contract_locations` of the JSON diagnostics, where the
configuration defines each contract it is of, then the entry of the contract the report is
about, when there is one. For `effect-boundaries` (`EFF201`), `invalidation` (`MUT208`), and
`import-boundaries` (`IMP201`), that is the entry the access matched, glob or module part
included: the `call-targets`, `read-targets`, `write-targets`, or `write-universe` entry, or the
`forbid` token, of a boundary; the `targets` entry of an invalidation contract; the
`forbidden-imports` pattern of a forbidden import, or the `relative-imports` one of a module
imported by its full name. A report of an access several entries match points at the first,
the one its `target` names. For the other contracts it is the entry whose text is the report's `target` (a
target of `write-owners`, `read-owners`, or `call-owners`, a producer of `readonly-returns`
written without a glob). Each has a `path`, a 1-based `line` and byte `column`, and a
`message` (``write-owners contract `fields` ``, ``` `targets` entry `pkg.m.Model.fields` ```,
with ``from group `name` `` for an entry a group holds). The places are where the file writes the table and the entry: in the main configuration
file, an included one, a defaults table, or a group. A table's
[`source` and `entry-sources`](configuration.md#where-a-contract-is-defined) name other places,
which are given instead. SARIF output adds the same places to `relatedLocations`, after the
steps of the report's chain, and the [language server](lsp.md) to its related information. The
places are no part of a baseline key.

## What an effect boundary cannot judge

An effect boundary with `unresolved = "report"` reports, under `EFF201`, the calls and writes it
cannot judge in a scope function's reach, one report per kind
(`<boundary>:unresolved-calls`, `<boundary>:unresolved-writes:<attribute>`) at the first one. In
the JSON diagnostics, `unresolved_sites` of such a report lists every site it stands for, ordered by
`path`, `line`, and `column`, with `function`, the function holding the site by its fully
qualified name. `sites` is the number of listed sites and is written only above one: a report
without `sites` stands for one site. A call has:

- `callee`: the method or value called (`get` of `table.get(...)`), `null` for a value computed in
  place (`make()(...)`), or for a body heki does not follow, that function's full name.
- `reason`: why heki does not follow it. For a call it resolves to no function, the reasons
  `heki recommend` gives: `unannotated-parameter`, `unresolved-annotation`, `any-value`,
  `callable-value`, `unbound-name`, `unlisted-protocol`, `call-result`, `attribute`,
  `computed-name`, `too-many-implementations`, `unknown-member`, or `untyped-value`. For a call
  of a function whose body heki does not model (`eval`, a body too large), `unmodeled-body`; for
  one only a stub declares, `stub-body`. A scope function whose own body is one of these is
  listed at its definition, as its own `function` and `callee`.
- `parameter`: the parameter a reason is about (`unannotated-parameter`, `unresolved-annotation`),
  else `null`.

A write has `attribute` (`null` for a name computed at run time, `setattr(obj, name, v)`) and
`kind`, as `heki inspect` names write kinds (`direct_attr`, `set_attr`, `container_mut`, ...).

```json
"unresolved_sites": [
  {"path": "pkg/m.py", "line": 7, "column": 9, "function": "pkg.m.run",
   "callee": "strip", "reason": "unannotated-parameter", "parameter": "text"},
  {"path": "pkg/m.py", "line": 9, "column": 5, "function": "pkg.m.run",
   "callee": "fn", "reason": "unannotated-parameter", "parameter": "fn"}
]
```

The list holds the sites the walk from the scope function records: the code behind a call heki
does not follow is not in it. A site several scope functions reach is listed in each of their
reports. Other diagnostics have no `unresolved_sites`, and the text output, SARIF, and the
language server do not show the list. A baseline entry keys such a report by its target, which
names no site, and counts its sites.

## Versions

The version of a format is part of its schema name. A format with a `version` or
`schema_version` field (the snapshot, the suppressed report, the baselines, `heki inspect`,
`heki recommend`) writes the number its schema is named by; the others (the diagnostics, the rule
metadata, the assumptions, the failure object) have no field and are known by the schema name.

- A change that breaks a reader, such as a field that is removed, renamed, or changes meaning,
  or a value a reader would not expect, makes the next version: a new schema file with the next
  number, and a new `version` where the format has the field. It is listed under Breaking
  changes in the release notes. The files of the earlier versions stay published.
- A field that is added is not breaking. The schema of the version gains the property, and the
  release notes list it. The schemas forbid properties they do not list, because they describe
  what this release writes; a reader should ignore fields it does not know.
- A baseline is read in every version. `heki check --update-baseline` writes version 3, and a
  file of an earlier version keeps its meaning until then.
