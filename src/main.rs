use zorg::keys;
use zorg::network::WalletNetwork;
use zorg::sync;

const USAGE: &str = "\
zorg — a modern Zcash CLI wallet

USAGE:
    zorg <command> [options]

COMMANDS:
    create [--name <name>] [--birthday <height>]       Create a new wallet
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
    --network <net>      main | test | regtest (default: main)
    --lwd <url>          Lightwalletd endpoint (default: mainnet.lightwalletd.com)
    --help, -h           Show this help

ENV:
    ZORG_WALLET_DB       Wallet database path
    ZORG_NETWORK         Network
    ZORG_LIGHTWALLETD_URL Lightwalletd endpoint
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
    let mut network: String = std::env::var("ZORG_NETWORK").unwrap_or_else(|_| "main".into());
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
                network = args.get(i).cloned().ok_or("--network requires a value")?;
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

    let net = keys::parse_network(&network)?;
    if let Some(parent) = std::path::Path::new(&db).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let lwd_url = lwd.unwrap_or_else(|| default_lwd_url(&net).to_string());

    let cmd = command_args[0].as_str();
    let opts = &command_args[1..];

    match cmd {
        "create" => cmd_create(&db, net, opts),
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
            sync::run_full_sync_blocking(&db, &lwd_url, &network)?;
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

fn cmd_create(db: &str, net: WalletNetwork, opts: &[String]) -> Result<(), String> {
    let name = flag_str(opts, "--name").unwrap_or("Account 1".into());
    let birthday = flag_u64(opts, "--birthday");
    let result = keys::create_wallet(net.as_str(), db, birthday, Some(&name))?;
    println!("Mnemonic (save this!): {}", result.mnemonic);
    println!("Account UUID: {}", result.account_uuid);
    println!("Address: {}", result.unified_address);
    Ok(())
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
    let amount_zat =
        (amount_str.parse::<f64>().map_err(|_| "Invalid amount")? * 100_000_000.0) as u64;
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
        if let Ok(seed) = zorg::secret_store::seed_from_macos_stored_mnemonic(net, account_uuid) {
            return Ok(seed);
        }
    }
    eprint!("Enter mnemonic: ");
    let mut mnemonic = String::new();
    std::io::stdin()
        .read_line(&mut mnemonic)
        .map_err(|e| format!("Failed to read mnemonic: {e}"))?;
    keys::mnemonic_to_seed(mnemonic.trim())
}
