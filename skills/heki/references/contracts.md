# Drafting and changing contracts

Read this before you add or change a contract (`write-owners`, `read-owners`, `call-owners`,
`effect-boundaries`, `import-boundaries`, and the others in
[Contracts](https://heki.koxudaxi.dev/configuration/#contracts)). A contract is a wall the
maintainers want kept. It is not a list of what the code does today.

## 1. Keep contracts thin

Three layers carry the checks. Use the lower ones first:

1. The default rules report the unconditional bugs. They need no configuration.
2. `MUT103` snapshots report a change of a public function's effects.
3. A dedicated `heki.toml` holds the few core boundaries a team wants enforced, and nothing
   else.

A long list of contracts that nobody can review is worse than a short one.

## 2. Start from evidence

Run `heki recommend`, `heki inspect <function>`, and `heki inspect --imports <module>`. The
contract candidates are invariants the code keeps today, not architectural intent. Propose only
the walls the maintainer wants enforced, and say why each one matters.

## 3. Declare the ideal wall, and put the old violations in a baseline

Write the wall as the team wants it to hold: `fail = true` on the table (contract reports are
heuristic, so without it `heki check` does not fail on them) and a `message` that tells the
reader what to do instead. `fail-on = "warning"` would also make every other warning fail the
run; use it only when you want that.

```toml
# heki.toml
[[write-owners]]
name = "generation-store"
targets = ["pkg.model.DataModel.fields"]
allowed-writers = ["Store.*"]
message = "use GenerationStore.set_fields() instead"
fail = true
```

When `heki check` then reports code that already crosses the wall, record those reports once:

```bash
heki check --baseline heki-baseline.toml --update-baseline
```

Set `baseline = "heki-baseline.toml"` in `heki.toml`. The old violations can then only shrink, and
a new one fails at once.

Never add an old or undesired writer, reader, caller, or import to `allowed-writers`,
`allowed-readers`, `allowed-callers`, `allowed-imports`, or another `allowed-*` list to turn
`heki check` green: that writes today's debt into the architecture, and nothing reports it again.

## 4. Never weaken a wall silently

Do not widen an `allowed-*` list, loosen `forbid`, grow a baseline, or add `# heki: ignore[...]`,
`# noqa`, or `ignore` to make a check pass, unless the maintainer has approved that exact change.

Present a change to `heki.toml` as a small diff the maintainer can review. Say what each wall
protects, why it is there, and how many old violations go into the baseline. Ask before you
write it when the maintainer has not seen the wall. The files `heki.toml`, the baseline, and the
CI configuration belong to the maintainers (`CODEOWNERS`).
