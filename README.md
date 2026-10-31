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
| `zorg create [name]` | Create a new account and recovery phrase |
| `zorg import [name] [--birthday <height>]` | Restore an account from a recovery phrase (index 0) |
| `zorg balance [--account <name>]` | Show spendable, pending, and locked balances |
| `zorg address [--account <name>]` | Get a unified receiving address |
| `zorg sync [--account <name>]` | Sync all databases, or the chosen account's database |
| `zorg status [--account <name>]` | Check sync status for all databases, or the chosen account's database |
| `zorg send <to> <amount> [--memo <text>] [--account <name>]` | Send Zcash (TAZ on testnet) |
| `zorg history [--account <name>] [--limit <n>]` | View transaction history |
| `zorg recovery [--account <name>]` | Show the recovery phrase |
| `zorg validate <address>` | Validate a Zcash address |
| `zorg delete --account <name>` | Delete an account after confirmation |

Create and import ask for a name if omitted.

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
