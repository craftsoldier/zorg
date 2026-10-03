use rusqlite::OptionalExtension;
use zorg::keys;
use zorg::network::WalletNetwork;
use zorg::sync;

const USAGE: &str = "\
zorg — a modern Zcash CLI wallet

USAGE:
    zorg <command> [options]

COMMANDS:
    create [--name <name>] [--birthday <height>]       Create a wallet (default birthday: chain tip − 100)
    import <mnemonic> [--passphrase <p>] [--name <n>]   Import from mnemonic
    accounts                                           List accounts
    balance [--account <uuid>]                          Show balance
    address [--account <uuid>]                          Show receive address
    sync                                                Sync with the chain
    status                                              Show sync progress
    send <to> <zec> [--memo <text>] [--account <uuid>] Send ZEC
    history [--limit <n>] [--account <uuid>]            Show transaction history
    validate <address>                                  Validate a Zcash address
    delete <uuid>                                       Delete an account

OPTIONS:
    --db <path>          Wallet database path (default: ~/.zorg/wallet.db)
    --network <net>      main | test | regtest (default: wallet DB network, else main)
    --lwd <url>          Lightwalletd endpoint (default: mainnet.lightwalletd.com)
    --help, -h           Show this help

ENV:
    ZORG_WALLET_DB       Wallet database path
    ZORG_NETWORK         Network
    ZORG_LIGHTWALLETD_URL Lightwalletd endpoint
    RUST_LOG             Log level: error | warn | info | debug | trace (default: info)
";

fn main() {
    zorg::init();
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprint!("{USAGE}");
        std::process::exit(0);
    }

    let result = run(&args[1..]);
    if let Err(e) = result {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

fn run(args: &[String]) -> Result<(), String> {
    // Extract global flags, leaving command + command args
    let mut db: String = std::env::var("ZORG_WALLET_DB").unwrap_or_else(|_| default_db_path());
    let mut network = std::env::var("ZORG_NETWORK").ok();
    let mut lwd: Option<String> = std::env::var("ZORG_LIGHTWALLETD_URL").ok();
    let mut command_args: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                eprint!("{USAGE}");
                return Ok(());
            }
            "--db" => {
                i += 1;
                db = args.get(i).cloned().ok_or("--db requires a path")?;
            }
            "--network" => {
                i += 1;
                network = Some(args.get(i).cloned().ok_or("--network requires a value")?);
            }
            "--lwd" => {
                i += 1;
                lwd = Some(args.get(i).cloned().ok_or("--lwd requires a URL")?);
            }
            _ => command_args.push(args[i].clone()),
        }
        i += 1;
    }

    if command_args.is_empty() {
        eprint!("{USAGE}");
        return Ok(());
    }

    let net = resolve_network(&db, network.as_deref())?;
    if let Some(parent) = std::path::Path::new(&db).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let lwd_url = lwd.unwrap_or_else(|| default_lwd_url(&net).to_string());

    let cmd = command_args[0].as_str();
    let opts = &command_args[1..];

    match cmd {
        "create" => cmd_create(&db, net, &lwd_url, opts),
        "import" => cmd_import(&db, net, opts),
        "accounts" => {
            for a in zorg::account::list_accounts(&db, net)? {
                println!("  {}  {:<20}  {}", a.uuid, a.name, a.unified_address);
            }
            Ok(())
        }
        "balance" => cmd_balance(&db, net, opts),
        "address" => {
            let account = flag_str(opts, "--account");
            let addr = zorg::account::get_address_from_db(&db, net, account.as_deref())?;
            println!("{addr}");
            Ok(())
        }
        "sync" => {
            println!("Syncing {db} via {lwd_url}…");
            sync::run_full_sync_blocking(&db, &lwd_url, net.as_str())?;
            println!("Sync complete.");
            Ok(())
        }
        "status" => {
            let p = sync::get_sync_progress(&db, net)?;
            if p.chain_tip_height == 0 {
                println!("Wallet not synced. Run `zorg sync`.");
            } else {
                let pct = p.scanned_height as f64 / p.chain_tip_height as f64 * 100.0;
                println!(
                    "Scanned: {} / {} ({:.1}%)",
                    p.scanned_height, p.chain_tip_height, pct
                );
                println!("Syncing: {} | Complete: {}", p.is_syncing, p.is_complete);
            }
            Ok(())
        }
        "send" => cmd_send(&db, net, &lwd_url, opts),
        "history" => cmd_history(&db, net, opts),
        "validate" => {
            let addr = opts.first().ok_or("Usage: zorg validate <address>")?;
            let t = sync::validate_address(addr)?;
            println!("Valid: {t}");
            Ok(())
        }
        "delete" => {
            let uuid = opts.first().ok_or("Usage: zorg delete <uuid>")?;
            zorg::account::delete_account(&db, net, uuid)?;
            println!("Account deleted.");
            Ok(())
        }
        _ => {
            eprint!("{USAGE}");
            Err(format!("Unknown command: {cmd}"))
        }
    }
}

fn cmd_create(db: &str, net: WalletNetwork, lwd_url: &str, opts: &[String]) -> Result<(), String> {
    let name = flag_str(opts, "--name").unwrap_or("Account 1".into());
    let birthday_arg = opts
        .iter()
        .position(|arg| arg == "--birthday")
        .map(|index| {
            opts.get(index + 1)
                .cloned()
                .ok_or("--birthday requires a block height")
        })
        .transpose()?;
    let birthday = resolve_create_birthday(birthday_arg.as_deref(), || {
        zorg::sync_engine::get_latest_block_height(lwd_url)
    })?;
    let result = keys::create_wallet(net.as_str(), db, Some(birthday), Some(&name))?;
    println!("Mnemonic (save this!): {}", result.mnemonic);
    println!("Account UUID: {}", result.account_uuid);
    println!("Address: {}", result.unified_address);
    Ok(())
}

fn resolve_create_birthday(
    birthday_arg: Option<&str>,
    fetch_tip: impl FnOnce() -> Result<u64, String>,
) -> Result<u64, String> {
    match birthday_arg {
        Some(height) => {
            let height = height
                .parse::<u64>()
                .map_err(|_| "Invalid --birthday height; expected a positive block height")?;
            if height == 0 {
                return Err("Invalid --birthday height; block heights start at 1".into());
            }
            Ok(height)
        }
        None => {
            let tip = fetch_tip()
                .map_err(|e| format!("Could not determine wallet birthday from chain tip: {e}"))?;
            birthday_from_chain_tip(tip)
        }
    }
}

fn birthday_from_chain_tip(tip: u64) -> Result<u64, String> {
    if tip == 0 {
        return Err(
            "Could not determine wallet birthday: lightwalletd returned an empty chain".into(),
        );
    }
    Ok(tip.saturating_sub(100).max(1))
}

fn cmd_import(db: &str, net: WalletNetwork, opts: &[String]) -> Result<(), String> {
    let positional: Vec<&String> = opts.iter().filter(|a| !a.starts_with("--")).collect();
    let mnemonic = positional
        .first()
        .ok_or("Usage: zorg import <mnemonic> [--passphrase <p>] [--name <n>]")?;
    let passphrase = flag_str(opts, "--passphrase").unwrap_or_default();
    let name = flag_str(opts, "--name").unwrap_or("Account 1".into());
    let birthday = flag_u64(opts, "--birthday");
    let result = keys::import_wallet(
        mnemonic,
        &passphrase,
        birthday,
        net.as_str(),
        db,
        Some(&name),
    )?;
    println!("Account UUID: {}", result.account_uuid);
    println!("Address: {}", result.unified_address);
    Ok(())
}

fn cmd_balance(db: &str, net: WalletNetwork, opts: &[String]) -> Result<(), String> {
    let uuid = match flag_str(opts, "--account") {
        Some(u) => u,
        None => first_account_uuid(db, net)?,
    };
    let bal = sync::get_wallet_balance(db, net, &uuid)?;
    println!("Spendable:  {}", fmt_zec(bal.spendable));
    println!("Pending:    {}", fmt_zec(bal.value_pending_spendability));
    println!("Locked:     {}", fmt_zec(bal.locked));
    println!("Total:      {}", fmt_zec(bal.total));
    if bal.transparent > 0 {
        println!("  Transparent: {}", fmt_zec(bal.transparent));
    }
    if bal.sapling > 0 {
        println!("  Sapling:      {}", fmt_zec(bal.sapling));
    }
    if bal.orchard > 0 {
        println!("  Orchard:      {}", fmt_zec(bal.orchard));
    }
    Ok(())
}

fn cmd_send(db: &str, net: WalletNetwork, lwd_url: &str, opts: &[String]) -> Result<(), String> {
    let positional: Vec<&String> = opts.iter().filter(|a| !a.starts_with("--")).collect();
    let to = positional
        .first()
        .ok_or("Usage: zorg send <to> <zec> [--memo <text>]")?;
    let amount_str = positional
        .get(1)
        .ok_or("Usage: zorg send <to> <zec> [--memo <text>]")?;
    let amount_zat = parse_zatoshi_amount(amount_str)?;
    let memo = flag_str(opts, "--memo");
    let uuid = match flag_str(opts, "--account") {
        Some(u) => u,
        None => first_account_uuid(db, net)?,
    };
    let send_flow_id = uuid::Uuid::new_v4().to_string();

    let proposal = sync::propose_send(
        db,
        net,
        &uuid,
        &send_flow_id,
        to,
        amount_zat,
        memo.as_deref(),
    )?;
    println!("Fee: {}", fmt_zec(proposal.fee_zatoshi));

    let seed = load_seed(db, net, &uuid)?;
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(sync::execute_proposal(
        db,
        lwd_url,
        proposal.proposal_id,
        &send_flow_id,
        seed,
        None,
        None,
    ))?;
    for txid in &result.txids {
        println!("Broadcasted: {txid}");
    }
    Ok(())
}

fn cmd_history(db: &str, net: WalletNetwork, opts: &[String]) -> Result<(), String> {
    let uuid = match flag_str(opts, "--account") {
        Some(u) => u,
        None => first_account_uuid(db, net)?,
    };
    let limit = flag_u64(opts, "--limit").map(|n| n as u32);
    let txs = sync::get_transaction_history(db, net, limit, &uuid)?;
    if txs.is_empty() {
        println!("No transactions yet.");
    }
    for tx in txs {
        let height = if tx.mined_height > 0 {
            tx.mined_height.to_string()
        } else {
            "pending".into()
        };
        println!(
            "  {:<10} {:<10} {:>16}  {:<8}  {}",
            tx.tx_kind,
            tx.display_pool,
            fmt_zec(tx.display_amount),
            height,
            &tx.txid_hex[..16.min(tx.txid_hex.len())]
        );
    }
    Ok(())
}

// ── helpers ──

fn flag_str(opts: &[String], flag: &str) -> Option<String> {
    let mut iter = opts.iter();
    while let Some(a) = iter.next() {
        if a == flag {
            return iter.next().cloned();
        }
    }
    None
}

fn flag_u64(opts: &[String], flag: &str) -> Option<u64> {
    flag_str(opts, flag)?.parse().ok()
}

fn first_account_uuid(db: &str, net: WalletNetwork) -> Result<String, String> {
    zorg::account::list_accounts(db, net)?
        .first()
        .map(|a| a.uuid.clone())
        .ok_or_else(|| "No accounts found. Run `zorg create`.".into())
}

fn fmt_zec(zatoshis: u64) -> String {
    format!("{:.8} ZEC", zatoshis as f64 / 100_000_000.0)
}

fn parse_zatoshi_amount(amount: &str) -> Result<u64, String> {
    const ZATOSHIS_PER_ZEC: u64 = 100_000_000;
    const MAX_ZATOSHIS: u64 = 21_000_000 * ZATOSHIS_PER_ZEC;
    let amount = amount.trim();
    if amount.starts_with('-') {
        return Err("Amount must be positive".into());
    }
    let (whole, fraction) = amount.split_once('.').unwrap_or((amount, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || amount.matches('.').count() > 1
    {
        return Err("Invalid amount; use a positive decimal ZEC amount".into());
    }
    if fraction.len() > 8 {
        return Err("Amount cannot have more than 8 decimal places".into());
    }
    let whole = whole
        .parse::<u64>()
        .map_err(|_| "Amount exceeds the maximum supply")?;
    let fractional = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u64>().map_err(|_| "Invalid amount")?
            * 10_u64.pow((8 - fraction.len()) as u32)
    };
    let zatoshis = whole
        .checked_mul(ZATOSHIS_PER_ZEC)
        .and_then(|value| value.checked_add(fractional))
        .ok_or("Amount exceeds the maximum supply")?;
    if zatoshis == 0 {
        return Err("Amount must be greater than zero".into());
    }
    if zatoshis > MAX_ZATOSHIS {
        return Err("Amount exceeds the maximum supply of 21,000,000 ZEC".into());
    }
    Ok(zatoshis)
}

fn resolve_network(db_path: &str, network: Option<&str>) -> Result<WalletNetwork, String> {
    let stored = network_in_wallet_db(db_path)?;
    if let Some(network) = network {
        let requested = keys::parse_network(network)?;
        if let Some(stored) = stored {
            if requested != stored {
                return Err(format!(
                    "Wallet database is for the {} network, but {} was explicitly selected",
                    stored.as_str(),
                    requested.as_str()
                ));
            }
        }
        return Ok(requested);
    }
    Ok(stored.unwrap_or(WalletNetwork::Main))
}

fn network_in_wallet_db(db_path: &str) -> Result<Option<WalletNetwork>, String> {
    if !std::path::Path::new(db_path).exists() {
        return Ok(None);
    }
    let conn = rusqlite::Connection::open(db_path)
        .map_err(|e| format!("Could not inspect wallet database network: {e}"))?;
    let has_accounts: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='accounts')",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|exists| exists != 0)
        .map_err(|e| format!("Could not inspect wallet database schema: {e}"))?;
    if !has_accounts {
        return Ok(None);
    }
    let encoded_key = conn
        .query_row(
            "SELECT uivk FROM accounts WHERE uivk IS NOT NULL AND uivk != '' LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|e| format!("Could not read wallet database network: {e}"))?;
    let Some(encoded_key) = encoded_key else {
        return Ok(None);
    };
    use zcash_address::unified::Encoding as _;
    let (network, _) = zcash_address::unified::Uivk::decode(&encoded_key).map_err(|e| {
        format!("Could not determine wallet database network from account key: {e}")
    })?;
    use zcash_protocol::consensus::NetworkType;
    Ok(Some(match network {
        NetworkType::Main => WalletNetwork::Main,
        NetworkType::Test => WalletNetwork::Test,
        NetworkType::Regtest => WalletNetwork::Regtest,
    }))
}

fn default_db_path() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    if cfg!(target_os = "macos") {
        format!("{home}/Library/Application Support/zorg/wallet.db")
    } else {
        format!("{home}/.zorg/wallet.db")
    }
}

fn default_lwd_url(net: &WalletNetwork) -> &'static str {
    match net {
        WalletNetwork::Main => "https://mainnet.lightwalletd.com:443",
        WalletNetwork::Test => "https://testnet.lightwalletd.com:443",
        WalletNetwork::Regtest => "http://localhost:9067",
    }
}

fn load_seed(
    _db: &str,
    net: WalletNetwork,
    account_uuid: &str,
) -> Result<secrecy::SecretVec<u8>, String> {
    #[cfg(target_os = "macos")]
    {
        match zorg::secret_store::seed_from_macos_stored_mnemonic(net, account_uuid) {
            Ok(seed) => return Ok(seed),
            Err(error) => {
                eprintln!("Keychain unavailable ({error}); falling back to manual mnemonic entry.")
            }
        }
    }
    eprint!("Enter mnemonic: ");
    let mut mnemonic = String::new();
    std::io::stdin()
        .read_line(&mut mnemonic)
        .map_err(|e| format!("Failed to read mnemonic: {e}"))?;
    keys::mnemonic_to_seed(mnemonic.trim())
}

#[cfg(test)]
mod tests {
    use super::{parse_zatoshi_amount, resolve_create_birthday, resolve_network};
    use zorg::network::WalletNetwork;

    #[test]
    fn send_amounts_are_exact_and_invalid_values_are_rejected() {
        for (input, expected) in [
            ("0.29", 29_000_000),
            ("0.1", 10_000_000),
            ("0.3", 30_000_000),
            ("1.00000001", 100_000_001),
            ("21000000", 2_100_000_000_000_000),
        ] {
            assert_eq!(parse_zatoshi_amount(input).unwrap(), expected, "{input}");
        }
        for input in [
            "0",
            "0.00000000",
            "-0.1",
            "21000000.00000001",
            "0.000000001",
            "NaN",
            "1e-8",
            "",
        ] {
            assert!(parse_zatoshi_amount(input).is_err(), "{input} should fail");
        }
    }

    #[test]
    fn network_selection_defaults_infers_and_rejects_conflicts() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_str().unwrap();
        assert_eq!(resolve_network(path, None).unwrap(), WalletNetwork::Main);
        let conn = rusqlite::Connection::open(file.path()).unwrap();
        conn.execute("CREATE TABLE accounts (uivk TEXT)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO accounts (uivk) VALUES (?1)",
            ["uivk1djetqg3fws7y7qu5tekynvcdhz69gsyq07ewvppmzxdqhpfzdgmx8urnkqzv7ylz78ez43ux266pqjhecd59fzhn7wpe6zarnzh804hjtkyad25ryqla5pnc8p5wdl3phj9fczhz64zprun3ux7y9jc08567xryumuz59rjmg4uuflpjqwnq0j0tzce0x74t4tv3gfjq7nczkawxy6y7hse733ae3vw7qfjd0ss0pytvezxp42p6rrpzeh6t2zrz7zpjk0xhngcm6gwdppxs58jkx56gsfflugehf5vjlmu7vj3393gj6u37wenavtqyhdvcdeaj86s6jczl4zq"],
        ).unwrap();
        drop(conn);

        assert_eq!(resolve_network(path, None).unwrap(), WalletNetwork::Main);
        assert_eq!(
            resolve_network(path, Some("main")).unwrap(),
            WalletNetwork::Main
        );
        assert!(resolve_network(path, Some("test"))
            .unwrap_err()
            .contains("explicitly selected"));
    }

    #[test]
    fn wallet_birthday_override_is_offline_and_default_uses_chain_tip() {
        let mut fetched_tip = false;
        assert_eq!(
            resolve_create_birthday(Some("123"), || {
                fetched_tip = true;
                Ok(1_000)
            })
            .unwrap(),
            123
        );
        assert!(
            !fetched_tip,
            "explicit birthday must not fetch the chain tip"
        );
        assert_eq!(resolve_create_birthday(None, || Ok(1_000)).unwrap(), 900);
        assert_eq!(resolve_create_birthday(None, || Ok(100)).unwrap(), 1);
        assert!(resolve_create_birthday(None, || Ok(0)).is_err());
        assert!(resolve_create_birthday(None, || Err("offline".into()))
            .unwrap_err()
            .contains("Could not determine wallet birthday"));
    }
}
