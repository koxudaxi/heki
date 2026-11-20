Before publishing this draft, bump the version to 0.1.1 in a pull request and merge it: `pyproject.toml`, `Cargo.toml` (`workspace.package.version` and the internal crates under `workspace.dependencies`), and `Cargo.lock`.

A note the owner wrote by hand.

## Breaking Changes

### Configuration and CLI Changes
* `--watch` is renamed - The option `--watch` is now `--follow` (#30)

### Hand-written Heading
* A change the owner described - Kept as written (#28)

## Changes

* Rename --watch (#30)
* Retry the first analysis (#31)

## Thanks

Everyone who reported a false positive.
