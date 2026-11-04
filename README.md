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

On first run, zorg helps you create an account or import a recovery phrase. With an existing account, it requests protected Keychain access before opening the account home screen and syncing in the background. macOS handles authentication with Touch ID or your login password.

| Screen | What you can do |
|---|---|
| **Send** | Enter a recipient, amount, and optional memo; review the fee before confirming. |
| **Receive** | Show or copy your receiving address, generate a new shielded address, or show your transparent address. |
| **Transactions** | Browse up to 10 entries per page, move between Older and Newer pages, and open transaction details. |
| **Settings** | Switch accounts, choose a server, export a viewing key, or show and copy your recovery phrase. |

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
| `zorg accounts` | List named accounts. |
| `zorg create [name] --new [--network <net>]` | Create an account with a new recovery phrase. |
| `zorg create [name] --import [--birthday <height>] [--network <net>]` | Import an existing recovery phrase. |
| `zorg balance [--account <name>]` | Show spendable, pending, locked, and total balances. |
| `zorg address [--new \| --transparent] [--account <name>]` | Show a receiving address, generate a new shielded address, or show the transparent address. |
| `zorg sync [--account <name>]` | Sync all accounts, or one named account. |
| `zorg status [--account <name>]` | Show account sync status. |
| `zorg send <to> <amount> [--memo <text>] [--account <name>]` | Send a payment. |
| `zorg send --shield [--account <name>]` | Move transparent funds into the shielded balance. |
| `zorg history [--limit <n>] [--account <name>]` | Print transaction history. |
| `zorg tx <txid> [--account <name>]` | Show a transaction in full. |
| `zorg recovery [--account <name>]` | Show the numbered recovery phrase and wallet birthday. |
| `zorg validate <address>` | Check an address. |
| `zorg delete --account <name>` | Delete an account. |

Account names are case-sensitive. Use `--account <name>` to select one; each account has its own database.

`--network main`, `--network test`, or `--network regtest` chooses the network when creating or importing an account. The default is mainnet. Other commands use the account’s saved network. Amounts are in ZEC on mainnet and TAZ on testnet.

Without flags, `zorg create` guides you through choosing a name and creating or importing an account. Without a terminal, provide an account name and specify `--new` or `--import`. Explicit commands skip terminal menus and confirmation cards when run without a terminal; operations that read recovery phrases still require protected Keychain access.

| Global option | Purpose |
|---|---|
| `--lwd <url>` | Use a specific lightwalletd endpoint. |
| `--help`, `-h` | Show command help. |
| `--version`, `-V` | Show the version. |

## Recovery and storage

Keep an offline copy of your recovery words and wallet birthday. Import accepts a 12- or 24-word phrase without numbering. Supplying its birthday avoids scanning from the beginning.

The account index and individual account databases live in `~/Library/Application Support/zorg/`. Recovery phrases are stored separately in this Mac’s protected Keychain. Those Keychain entries do not move to another Mac; keep your own recovery copy.

## Build from source

Requires **macOS** and the **stable Rust toolchain**.

```sh
git clone https://github.com/craftsoldier/zorg.git
cd zorg
cargo build --locked --release
./target/release/zorg --help
```

The executable is `target/release/zorg`. Accessing recovery phrases requires the signed `zorg.app` bundle with its provisioning profile and Keychain entitlements; a Cargo build alone does not assemble or sign that bundle.

## Contribute

See [CONTRIBUTING.md](CONTRIBUTING.md) for development and testing instructions.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option.
