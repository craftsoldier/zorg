# Zorg
```text
 ______   ______     ______     ______    
/\___  \ /\  __ \   /\  == \   /\  ___\   
\/_/  /__\ \ \/\ \  \ \  __<   \ \ \__ \  
  /\_____\\ \_____\  \ \_\ \_\  \ \_____\ 
  \/_____/ \/_____/   \/_/ /_/   \/_____/ 
```

Zorg is a fast, lightweight, and CLI-only Zcash wallet written in Rust. It provides a simple and secure interface for managing accounts, syncing with the Zcash network, and sending shielded transactions natively from your terminal.

## Features
- **CLI Native:** Clean and simple commands to manage your Zcash wallet.
- **Secure by Default:** Automatically uses the macOS Keychain to securely store your mnemonic phrases.
- **Shielded Transactions:** Fully supports Sapling and Orchard shielded pools.
- **Multi-Account:** Easily manage multiple accounts and track your balance securely.

## Accounts and storage

Run `zorg` to list accounts across all registered databases. Select an account by
its case-sensitive name, for example `zorg balance --account Savings`. Names are
unique across registered databases; they are independent of derivation indices.
With exactly one account you can omit `--account`, except when deleting it.

Each database holds one seed and can contain several derived accounts. Recovery
phrases are stored once per database in the protected macOS Keychain. Account
names, viewing keys, and transaction history live in ordinary, unencrypted SQLite.

On macOS, zorg stores its index and newly created databases in
`~/Library/Application Support/zorg/`; other platforms use `~/.zorg/`.
`index.toml` contains only absolute database paths:

```toml
[[databases]]
path = "/Users/jules/Library/Application Support/zorg/example.db"

[[databases]]
path = "/Volumes/Private/savings.db"
```

`zorg register <path>` remembers an existing database at any location. Its parent
directory must be private (0700). To move a registered database, stop commands
using it, move it, and update its path in `index.toml`. A missing database stays
registered and is reported as unavailable; zorg does not create a replacement.
Reconnect unavailable databases before creating, importing, registering, or
renaming accounts so zorg can check names across the full index.

This layout replaces database-path selection and numbered accounts. There is no
legacy database or Keychain migration, and `--db` / `ZORG_WALLET_DB` are removed.
Register accepts databases using the new seed metadata layout. An absent index
starts with no accounts; it does not automatically adopt an old `wallet.db`.

## Commands

| Command | Description |
|---|---|
| `zorg` / `zorg accounts` | List named accounts across registered databases |
| `zorg create [name]` | Create a new phrase and its first account in a new database |
| `zorg import [name] [--birthday <height>]` | Import a phrase's account at index 0 into a new database |
| `zorg register <path>` | Register an existing database location |
| `zorg rename <new-name> --account <name>` | Rename an account without changing its keys |
| `zorg balance [--account <name>]` | Show spendable, pending, and locked balances |
| `zorg address [--account <name>]` | Get a unified receiving address |
| `zorg sync [--account <name>]` | Sync all databases, or the chosen account's database |
| `zorg status [--account <name>]` | Check sync status for all databases, or the chosen account's database |
| `zorg send <to> <amount> [--memo <text>] [--account <name>]` | Send Zcash (TAZ on testnet) |
| `zorg history [--account <name>] [--limit <n>]` | View transaction history |
| `zorg recovery [--account <name>]` | Show the account's shared recovery phrase |
| `zorg validate <address>` | Validate a Zcash address |
| `zorg delete --account <name> [--yes]` | Delete an account after confirmation |

Create and import ask for a name when it is omitted. Import rejects a phrase
already registered on the same network. Import discovers index 0 only; adding
higher indices from the CLI and automatic account discovery are deferred.

### Global flags

| Flag | Meaning |
|---|---|
| `--network <main\|test\|regtest>` | Filter accounts by network; defaults to main for create/import |
| `--lwd <url>` | lightwalletd endpoint override |
| `--help`, `-V` | Show usage / version |

Environment equivalents are `ZORG_NETWORK` and `ZORG_LIGHTWALLETD_URL`.

## Requirements
- Rust & Cargo
- macOS (Required for Keychain integration)

## Building from Source
```bash
git clone https://github.com/craftsoldier/zorg.git
cd zorg
cargo build --release
```
The compiled binary will be available at `target/release/zorg`.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Zorg is dual-licensed under the [MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE) licenses; you may choose either.
