Fixes #

## What changes for users
Default output, options, and docs. Write "nothing" if nothing does.

## Checks run
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `python3.15 scripts/build_docs.py --heki target/release/heki --check` and `python3.15 scripts/build_llms_txt.py --check` when docs changed

## Exact reports
Does this add or change exact reports? The maintainer runs the corpus for those.

- [ ] No
- [ ] Yes: which rules
