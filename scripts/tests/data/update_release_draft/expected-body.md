Before publishing this draft, bump the version to 0.2.0 in a pull request and merge it: `pyproject.toml`, `Cargo.toml` (`workspace.package.version` and the internal crates under `workspace.dependencies`), and `Cargo.lock`.

A note the owner wrote by hand.

## Breaking Changes

### Default Rule Changes
* `MUT101` reports setup methods - Classes whose `setup_*` methods write cached attributes get new reports (#31)

### Configuration and CLI Changes
* `--watch` is renamed - The option `--watch` is now `--follow` (#30)

### Hand-written Heading
* A change the owner described - Kept as written (#28)

## Changes

* Rename --watch (#30)
* MUT101: report setup\_\* methods - and \<b\>more\</b\> \@someone (#31)

## Thanks

Everyone who reported a false positive.
