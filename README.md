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

## Commands

Run `zorg` to list accounts. Select an account by name with `--account Savings`.

| Command | Description |
|---|---|
| `zorg` / `zorg accounts` | List named accounts across known databases |
| `zorg create [name] --new [--network <net>]` | Create an account with a new recovery phrase |
| `zorg create [name] --import [--birthday <height>] [--network <net>]` | Create an account from an existing recovery phrase (index 0) |
| `zorg balance [--account <name>]` | Show spendable, pending, and locked balances |
| `zorg address [--new] [--account <name>]` | Get a unified receiving address, or make a new one |
| `zorg sync [--account <name>]` | Sync all databases, or the chosen account's database |
| `zorg status [--account <name>]` | Check sync status for all databases, or the chosen account's database |
| `zorg send <to> <amount> [--memo <text>] [--account <name>]` | Send Zcash (TAZ on testnet) |
| `zorg history [--account <name>] [--limit <n>]` | View transaction history |
| `zorg tx <txid> [--account <name>]` | Show one transaction in full |
| `zorg recovery [--account <name>]` | Show the recovery phrase |
| `zorg validate <address>` | Validate a Zcash address |
| `zorg delete --account <name>` | Delete an account after confirmation |

Create asks for a name if omitted, and in a terminal asks whether to import or start a new account when neither `--new` nor `--import` is given; without a terminal one of them is required. `--network <main|test|regtest>` picks the new account's network (default main); other commands use the network the account was created on.

### Global flags

| Flag | Meaning |
|---|---|
| `--lwd <url>` | lightwalletd endpoint override |
| `--help`, `-h` | Show usage |
| `--version`, `-V` | Show version |

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
