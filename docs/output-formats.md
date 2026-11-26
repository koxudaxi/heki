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
