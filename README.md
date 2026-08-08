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
| `zorg create [name]` | Create a new Zcash wallet |
| `zorg import "<mnemonic>" "<passphrase>" [name]` | Import an existing wallet |
| `zorg accounts` | List all accounts in the wallet |
| `zorg balance` | Show your wallet balance |
| `zorg address [--account <uuid>]` | Get a unified receiving address |
| `zorg sync` | Sync your wallet with the blockchain |
| `zorg status` | Check current sync status |
| `zorg send <address> <amount> [memo]` | Send Zcash to an address |
| `zorg history` | View transaction history |
| `zorg validate <address>` | Validate a Zcash address |
| `zorg delete <uuid>` | Delete an account from the wallet |

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
