A note the owner wrote by hand.

## Breaking Changes

### Default Rule Changes
* `MUT101` reports setup methods - Classes whose `setup_*` methods write cached attributes get new reports (#31)

### Configuration and CLI Changes
* `--watch` is renamed - The option `--watch` is now `--follow` (#30)

### Hand-written Heading
* A change the owner described - Kept as written (#28)

## Experimental

* `EFF101` is new - An experimental rule for blocking calls in coroutines (#29)
* `EFF101` reports `time.sleep` - The experimental rule also reports `time.sleep` in coroutines (#31)

## Changes

* Rename --watch (#30)
* MUT101: report setup\_\* methods - and \<b\>more\</b\> \@someone (#31)

## Thanks

Everyone who reported a false positive.
