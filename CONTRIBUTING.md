# Contributing to Zorg

Thanks for your interest in improving Zorg. This document covers what you
need to know to get a patch merged.

## Development environment

- **Rust** (stable toolchain) — `rustup` is the easiest way to get it.
- **macOS 14+ with Secure Enclave** is required to run the wallet end-to-end.
  Install `age` and `age-plugin-se` with Homebrew for the default standalone build.
  The packaged build uses the existing protected Keychain.
- A lightwalletd endpoint is only needed for commands that touch the network
  (`create`, `sync`, `send`). Sensible defaults are built in; override with
  `--lwd`.

## Building and testing

```bash
cargo build --locked --release # the binary lands in target/release/zorg
cargo test --locked            # unit tests
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --all -- --check
```

CI runs these checks for both storage modes (plus a CLI help smoke test and dependency advisory audit) and they must all pass. To check the packaged mode, add `--no-default-features --features packaged` to the build, test, and Clippy commands. Running them locally
before pushing saves everyone a round trip.

## Ground rules

1. **Amounts are integers, always.** Zatoshis are `u64`. Floating point must
   never touch a value that represents money — `f64` rounding is a real bug
   class here, not a theoretical one (see issue #1).
2. **Keep PRs one-purpose.** One behavior change or one doc change per PR,
   with a description that says what changed and why.
3. **Commit style:** short imperative subject, conventional prefix when it
   fits (`fix:`, `feat:`, `docs:`). The body, if any, explains *why*.
4. **Test what you touch.** Parsing, amounts, and anything with a branch in
   it should have a unit test. Network-dependent behavior is exercised on
   testnet (`--network test`); never mainnet.
5. **Never commit secrets.** No mnemonics, no wallet databases, no
   keychain dumps. Wallet state lives outside the repo
   (`~/.zorg/` for standalone; `~/Library/Application Support/zorg/` for packaged).

## Reporting bugs

Open a GitHub issue with the command you ran, the output, and the network
you were on. Redact addresses only if you need to — testnet addresses are
public data anyway.

## Security issues

Please do not report wallet-fund vulnerabilities through public issues.
Use GitHub's private vulnerability reporting (Security tab) so a fix can
land before the bug is public.

## Licensing

By contributing, you agree that your contributions are dual-licensed under
MIT and Apache-2.0 (`MIT OR Apache-2.0`), the same as the rest of the
project.
