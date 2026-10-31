# AGENTS.md

Guidance for coding agents (and humans) working on zorg.

## Run CI before every commit — no exceptions

`.github/workflows/ci.yml` is the source of truth. Replicate it locally before
every commit and push. CI is green-before-merge; a red remote run wastes a
round trip.

```bash
cargo fmt --all -- --check                            # formatting
cargo clippy --locked --all-targets -- -D warnings    # warnings are errors
cargo test --locked                                   # full suite
cargo build --locked --release                        # release build must succeed
./target/release/zorg --help                          # CLI smoke test
cargo audit                                           # dependency advisories
```

- Use `--locked` on every command: it fails if your change silently edits
  `Cargo.lock`.
- `-D warnings` matters: warnings block CI, so block them locally too.
- `cargo check --locked --all-targets` on Linux runs remotely only — do not
  assume platform-specific code compiles there.
- Behavioral changes to commands need end-to-end runs against a throwaway
  regtest wallet (`--network regtest`, an index and databases in a private temp dir), not just
  unit tests.
- A known non-blocking `cargo audit` warning set exists (yanked transitive
  crates, e.g. `spin`); "allowed warnings" is a pass. New advisories are not.

## Conventions

- Single-sentence commit messages.
- Minimal diffs. Never reformat, rename, or "improve" adjacent code.
- Doc comments state what the thing is and its contract. Design rationale
  belongs in the PR description. Doc comment wording changes are review
  surface — propose, don't slip in.
- Implement strictly what was asked. No unrequested fallbacks, options, flags,
  or forward-compat extras.
- One task per PR, one branch off the latest `main`. Don't stack PRs.
- The CLI selects accounts by name, never by database path or internal UUID.
  One seed belongs to one database; it can derive multiple named accounts.
