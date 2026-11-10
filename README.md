# zorg

```text
 ______   ______     ______      _____
/\___  \ /\  __ \   /\  == \    / ____|
\/_/  /__\ \ \/\ \  \ \  __<    | |  __
  /\_____\\ \_____\  \ \_\ \_\  | | |_ |
  \/_____/ \/_____/   \/_/ /_/   \_____|
```

**Zcash, in your terminal.**

A macOS wallet for shielded payments, named accounts, and transaction history. Open the interactive wallet with `zorg`, or use explicit commands from your terminal and scripts.

## Open your wallet

```sh
zorg
```

On first run, zorg helps you create an account or import a recovery phrase. With an existing account, it requests access to the protected recovery phrase before opening the account home screen and syncing in the background. macOS handles authentication with Touch ID or your login password.

| Screen | What you can do |
|---|---|
| **Send** | Enter a recipient, amount, and optional memo; review the fee before confirming. Press Tab at the amount to fill in the most you can send. |
| **Receive** | Show or copy your receiving address, generate a new shielded address, or show your transparent address. |
| **Transactions** | Browse up to 10 entries per page, move between pages with Left/Right or h/l, and open transaction details, where you can copy the txid. |
| **Settings** | Switch accounts (press `r` to rename or `d` to delete the highlighted one), choose a server, export a viewing key, or show and copy your recovery phrase. |

Press `b` on the home screen to hide or show balances, `s` for Settings, and `q` to quit. **Shield** appears when the account has enough transparent funds to offer shielding.

Recovery words are displayed with numbers for readability. **Copy phrase** copies the plain words so they can be imported.

## Use commands

For example, create a testnet account named Savings:

```sh
zorg create Savings --new --network test
zorg sync --account Savings
zorg balance --account Savings
```

| Command | Purpose |
|---|---|
| `zorg accounts` | List named accounts; `*` marks the one the app opens. |
| `zorg accounts --rename <name> --account <name>` | Rename an account. |
| `zorg accounts --default --account <name>` | Make an account the one the app opens. |
| `zorg accounts --delete --account <name>` | Delete an account. |
| `zorg create [name] --new [--network <net>]` | Create an account with a new recovery phrase. |
| `zorg create [name] --import [--birthday <height>] [--network <net>]` | Import an existing recovery phrase. |
| `zorg balance [--account <name>]` | Show spendable, pending, locked, and total balances. |
| `zorg address [--new \| --transparent] [--account <name>]` | Show a receiving address, generate a new shielded address, or show the transparent address. |
| `zorg address --parse <address>` | Show the receivers of an address. |
| `zorg sync [--account <name>]` | Sync all accounts, or one named account. |
| `zorg status [--account <name>]` | Show account sync status. |
| `zorg send <to> <amount> [--memo <text>] [--account <name>]` | Send a payment. |
| `zorg send <to> --max-amount [--memo <text>] [--account <name>]` | Send all spendable funds, minus the fee. |
| `zorg send --shield [--account <name>]` | Move transparent funds into the shielded balance. |
| `zorg history [--limit <n>] [--account <name>]` | Print transaction history. |
| `zorg tx <txid> [--account <name>]` | Show a transaction in full. |
| `zorg recovery [--account <name>]` | Show the numbered recovery phrase and wallet birthday. |

Account names are case-sensitive. Use `--account <name>` to select one; each account has its own database.

`--network main`, `--network test`, or `--network regtest` chooses the network when creating or importing an account. The default is mainnet. Other commands use the account’s saved network. Amounts are in ZEC on mainnet and TAZ on testnet.

Without flags, `zorg create` offers creation or import; press `n` there to change the network. Guided import then asks for a hidden recovery phrase, read from stdin when it is piped. After validating the phrase, zorg asks for the birthday and account name, then confirms the import details and estimated sync time before creating the account. Supplied name, network, and birthday arguments fill in those values.

The estimated sync range uses 200 blocks per second up to twice that duration, rounded up to minutes. It is a rough planning estimate based on the birthday and current chain tip, not a measured rate for your Mac. The confirmation needs a reachable lightwalletd endpoint; an omitted birthday estimates scanning from Sapling activation. Birthday `0` is accepted in both the prompt and flag.

Without a terminal, provide an account name and specify `--new` or `--import`. Explicit commands skip terminal menus and confirmation cards when run without a terminal; operations that read recovery phrases still require macOS authentication.

| Global option | Purpose |
|---|---|
| `--lwd <url>` | Use a specific lightwalletd endpoint. |
| `--help`, `-h` | Show command help. |
| `--version`, `-V` | Show the version. |

## Recovery and storage

Keep an offline copy of your recovery words and wallet birthday. Import accepts a 12- or 24-word phrase without numbering. Supplying its birthday avoids scanning from the beginning.

The two builds keep separate wallets:

| Build | Wallet directory | Recovery phrase storage |
|---|---|---|
| `standalone` (default) | `~/.zorg/` | age-encrypted files with a Secure Enclave identity per account. |
| `packaged` | `~/Library/Application Support/zorg/` | This Mac’s protected Keychain, as before. |

Standalone secrets are under the wallet directory’s `secrets/` folder. The identity files are required to decrypt the phrases, and work only on the original Mac. Neither copying these files nor copying Keychain-backed databases to another Mac replaces a recovery backup. Keep your written recovery words and birthday.

The builds do not discover or migrate each other’s accounts. To use an account in the other build, import its recovery words and birthday there. Both builds authorize phrase access with Touch ID or the Mac password; macOS controls prompts and any authorization reuse.

## Build from source

Requires **macOS 14 or later**, a Mac with a **Secure Enclave**, the **stable Rust toolchain**, and Homebrew’s `age` and `age-plugin-se` tools.

```sh
brew install age age-plugin-se
git clone https://github.com/craftsoldier/zorg.git
cd zorg
cargo build --locked --release
./target/release/zorg --help
```

The executable is `target/release/zorg`. This default `standalone` build uses age-plugin-se without a provisioning profile or developer certificate. Identity files are created exclusively with private permissions, and a phrase is decrypted and checked before account creation succeeds.

The `packaged` build retains the existing Keychain storage:

```sh
cargo build --locked --release --no-default-features --features packaged
```

Running phrase operations in that build requires the signed `zorg.app` bundle with its provisioning profile and Keychain entitlements. The feature selects storage behavior; it does not assemble or sign a bundle. Enable exactly one of `standalone` and `packaged`.

## Contribute

See [CONTRIBUTING.md](CONTRIBUTING.md) for development and testing instructions.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option.
