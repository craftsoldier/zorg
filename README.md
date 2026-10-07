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

| Command | Description |
|---|---|
| `zorg create [--name <n>] [--birthday <height>]` | Create a new Zcash wallet (name defaults to "Account 1"; birthday defaults to the current chain tip) |
| `zorg import <mnemonic> [--passphrase <p>] [--name <n>]` | Import an existing wallet from its mnemonic |
| `zorg accounts` | List all accounts in the wallet (uuid, name, address) |
| `zorg balance [--account <uuid>]` | Show spendable, pending, and locked balances |
| `zorg address [--account <uuid>]` | Get a unified receiving address |
| `zorg sync` | Sync your wallet with the blockchain |
| `zorg status` | Check current sync status (scan height vs chain tip) |
| `zorg send <to> <amount> [--memo <text>] [--account <uuid>]` | Send Zcash to an address (TAZ on testnet) |
| `zorg history [--account <uuid>] [--limit <n>]` | View transaction history |
| `zorg validate <address>` | Validate a Zcash address |
| `zorg delete <uuid>` | Delete an account from the wallet |

### Global flags

Every flag works on any command:

| Flag | Meaning |
|---|---|
| `--network <main\|test>` | Network (defaults to mainnet) |
| `--db <path>` | Wallet database path |
| `--lwd <url>` | lightwalletd endpoint override |
| `--help`, `-V` | Show usage / version |

The same values can be set through the environment: `ZORG_NETWORK`, `ZORG_WALLET_DB`, `ZORG_LIGHTWALLETD_URL`.

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
